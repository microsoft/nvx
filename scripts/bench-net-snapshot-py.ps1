<#
.SYNOPSIS
    Snapshot/restore benchmark of a *networked* Python guest on Windows (WHP backend).
    Mirrors scripts/bench-net-snapshot-py.sh.

.DESCRIPTION
    Benchmarks snapshot/restore of a networked Python guest for two workloads:
      (a) net-hello.py  -- a bare CPython interpreter, and
      (b) net-pandas.py -- a warmed numpy/pandas interpreter,
    each of which proves the link works (a real HTTP round-trip to a host helper server) right
    before the timing marker, so a run that reaches the marker has resumed with a *live* NIC.

    For each app it times: cold boot to the marker (kernel boot + Python startup [+ pandas import] +
    NIC config + link check) versus restoring a snapshot captured at the warmed point.

    Unlike the KVM script no host TAP (and thus no admin rights or `sudo ip`) is needed: the WHP
    backend supplies networking through a built-in user-mode NAT. The guest apps GET the gateway
    (10.0.0.1:PORT); the NAT forwards gateway-destined flows to the host loopback, so the helper
    server is bound to 127.0.0.1:PORT.

    Requires the Python initramfs (build\initramfs-python.cpio.gz) and host Python (for the helper
    server). The initramfs is produced by scripts/build-python-initramfs.sh, which needs apk/pip
    (network) -- build it on a networked machine (or via the Docker toolchain) and copy it in.

.EXAMPLE
    scripts\bench-net-snapshot-py.ps1
    scripts\bench-net-snapshot-py.ps1 -Mem 256 -N 10
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [int]$Mem = 512,
    [int]$N = 8,
    [string]$Net = '10.0.0.2/24',
    [int]$Port = 8099
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs-python.cpio.gz' }

$cmdline = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'

if (-not (Test-Path $bin))    { throw "build the VMM first: cargo build --release" }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing python initramfs: $Initrd (scripts/build-python-initramfs.sh, needs network)" }

$py = Get-Command python -ErrorAction SilentlyContinue
if (-not $py) { $py = Get-Command python3 -ErrorAction SilentlyContinue }
if (-not $py) { throw "host Python not found (needed for the helper server); install Python or add it to PATH" }

# Runs the VMM once; returns the "cold-start:"/"restore:" ms the VMM prints (or $null). Both pipes
# are drained concurrently via ReadToEndAsync so a chatty guest can't fill the pipe buffer and block.
function Invoke-VmMs {
    param([string[]]$VmArgs, [int]$TimeoutSec = 60)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($VmArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardInput.Close()
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($TimeoutSec * 1000)) { try { $p.Kill() } catch {}; $p.WaitForExit(2000) | Out-Null }
    $text = $errTask.Result + "`n" + $outTask.Result
    if ($text -match '(?:cold-start|restore):\s*([0-9.]+)') { [pscustomobject]@{ Ms = [double]$Matches[1]; Text = $text } }
    else { [pscustomobject]@{ Ms = $null; Text = $text } }
}

function Format-Median {
    param([double[]]$Vals)
    $s = @($Vals | Where-Object { $_ -ne $null } | Sort-Object)
    if (-not $s.Count) { return 'NO DATA' }
    $md = if ($s.Count % 2) { $s[[int](($s.Count - 1) / 2)] } else { ($s[$s.Count/2 - 1] + $s[$s.Count/2]) / 2 }
    '{0,7:N1} ms  (min {1:N1}, max {2:N1}, n={3})' -f $md, $s[0], $s[-1], $s.Count
}

# Host helper server: the guest apps do an HTTP GET to the gateway (NAT -> 127.0.0.1) to prove the
# link. Serve a directory whose index.html is "HELLO-HOST" via `python -m http.server`.
$www = Join-Path ([System.IO.Path]::GetTempPath()) ("nvx-www-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $www | Out-Null
Set-Content -Path (Join-Path $www 'index.html') -Value 'HELLO-HOST' -NoNewline

$srv = $null
try {
    $srvOut = Join-Path $www '.httpd.out'
    $srv = Start-Process -FilePath $py.Source -ArgumentList @('-m', 'http.server', '--bind', '127.0.0.1', "$Port") `
        -WorkingDirectory $www -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $srvOut -RedirectStandardError (Join-Path $www '.httpd.err')

    $ready = $false
    for ($i = 0; $i -lt 25 -and -not $ready; $i++) {
        try {
            $r = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/" -TimeoutSec 1 -UseBasicParsing
            if ($r.Content -match 'HELLO-HOST') { $ready = $true; break }
        } catch {}
        Start-Sleep -Milliseconds 200
    }
    if (-not $ready) { throw "host helper server did not come up on port $Port (already in use?)" }

    # Cold N -> snapshot -> restore N -> one loud verify run, for a single guest app.
    function Invoke-BenchApp {
        param([string]$App, [string]$Marker, [string]$Label)
        $snap = Join-Path $repo ('build\nspy_' + ($App -replace '\.py$', ''))
        Write-Host "== $Label =="

        $cold = @()
        for ($i = 0; $i -lt $N; $i++) {
            $r = Invoke-VmMs @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--net', $Net,
                '--exit-on-boot', '--quiet', '--boot-marker', $Marker, '--cmdline', "$cmdline pyapp=$App")
            if ($r.Ms -ne $null) { $cold += $r.Ms }
        }
        Write-Host ("  cold    (guest start  -> marker): {0}" -f (Format-Median -Vals $cold))

        if (Test-Path $snap) { Remove-Item -Recurse -Force $snap }
        Invoke-VmMs @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--net', $Net,
            '--snapshot', $snap, '--quiet', '--cmdline', "$cmdline pyapp=$App") | Out-Null
        if (-not (Test-Path (Join-Path $snap 'state.bin'))) { Write-Host '  snapshot capture failed'; return }
        $alloc = 0
        & fsutil sparse queryrange (Join-Path $snap 'mem.bin') 2>$null | ForEach-Object {
            if ($_ -match 'length:\s*(0x[0-9a-fA-F]+)') { $alloc += [Convert]::ToInt64($Matches[1], 16) }
        }
        Write-Host ("  snapshot: mem.bin footprint ~{0:N0} MiB on disk" -f ($alloc / 1MB))

        $rest = @()
        for ($i = 0; $i -lt $N; $i++) {
            $r = Invoke-VmMs @('--restore', $snap, '--mem', "$Mem", '--exit-on-boot', '--quiet', '--boot-marker', $Marker) 40
            if ($r.Ms -ne $null) { $rest += $r.Ms }
        }
        Write-Host ("  restore (guest resume -> marker): {0}" -f (Format-Median -Vals $rest))

        # Confirm the marker really is the OK variant (network verified), one loud run.
        $v = Invoke-VmMs @('--restore', $snap, '--mem', "$Mem", '--exit-on-boot', '--boot-marker', $Marker, '--log-level', 'warn') 40
        $seen = if ($v.Text -match ([regex]::Escape($Marker) + "[A-Za-z' :{},0-9]*")) { $Matches[0] } else { '<marker not seen>' }
        Write-Host ("  verified: {0}" -f $seen)
        Remove-Item -Recurse -Force $snap -ErrorAction SilentlyContinue
    }

    Write-Host "networked Python snapshot benchmark, median of $N, ${Mem} MiB, 1 vCPU, --net $Net (user-mode NAT)"
    Write-Host ''
    Invoke-BenchApp -App 'net-hello.py'  -Marker 'HELLOPY-NET OK'  -Label '(a) hello-world Python (bare interpreter + live NIC)'
    Write-Host ''
    Invoke-BenchApp -App 'net-pandas.py' -Marker 'PANDASPY-NET OK' -Label '(b) numpy + pandas app (warmed interpreter + live NIC)'
}
finally {
    if ($srv -and -not $srv.HasExited) { try { Stop-Process -Id $srv.Id -Force } catch {} }
    Remove-Item -Recurse -Force $www -ErrorAction SilentlyContinue
}
