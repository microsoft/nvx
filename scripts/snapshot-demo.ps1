<#
.SYNOPSIS
    Snapshot/restore benchmark with the Python pandas/numpy app on Windows (WHP backend).
    Mirrors scripts/snapshot-demo.sh.

.DESCRIPTION
    Measures snapshot/restore with the Python pandas/numpy benchmark app:
      1. cold-boot the VM and time reaching the DataFrame result (kernel boot + Python startup +
         the pandas/numpy import);
      2. take a VMM snapshot at the fully warmed point the app requests one (pandas/numpy imported
         and the DataFrame computation already exercised);
      3. restore from the snapshot N times and time reaching the DataFrame result.

    Requires the Python initramfs (build\initramfs-python.cpio.gz). That artifact bundles CPython +
    numpy/pandas and is produced by scripts/build-python-initramfs.sh, which needs apk/pip and thus a
    network connection -- build it on a networked machine (or via the Docker toolchain) and copy it in.

.EXAMPLE
    scripts\snapshot-demo.ps1
    scripts\snapshot-demo.ps1 -Mem 256 -N 10
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [string]$Snap,
    [int]$Mem = 512,
    [int]$N = 8
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs-python.cpio.gz' }
if (-not $Snap)   { $Snap   = Join-Path $repo 'build\pysnap' }

$marker  = "{'x': 10, 'y': 30}"
$cmdline = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'

if (-not (Test-Path $bin))    { throw "build the VMM first: cargo build --release" }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing python initramfs: $Initrd (scripts/build-python-initramfs.sh, needs network)" }

# Runs the VMM once with the given argument list, waits up to $TimeoutSec, and returns the
# "cold-start:"/"restore:" milliseconds the VMM prints (or $null if the marker was not seen).
# Both pipes are drained concurrently via ReadToEndAsync so a chatty guest can't fill the pipe
# buffer and block (PowerShell -Action handlers would not run while the main thread is parked in
# WaitForExit, but the async stream reads run on threadpool threads and keep draining).
function Invoke-VmMs {
    param([string[]]$VmArgs, [int]$TimeoutSec = 40)
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
    if ($text -match '(?:cold-start|restore):\s*([0-9.]+)') { [double]$Matches[1] } else { $null }
}

# Formats "median ms  (min, max, n)" like the shell script's median() helper.
function Format-Median {
    param([double[]]$Vals)
    $s = @($Vals | Where-Object { $_ -ne $null } | Sort-Object)
    if (-not $s.Count) { return 'NO DATA' }
    $md = if ($s.Count % 2) { $s[[int](($s.Count - 1) / 2)] } else { ($s[$s.Count/2 - 1] + $s[$s.Count/2]) / 2 }
    '{0,7:N1} ms  (min {1:N1}, max {2:N1}, n={3})' -f $md, $s[0], $s[-1], $s.Count
}

Write-Host "== cold boot -> pandas result (kernel boot + Python startup + pandas/numpy import) =="
$cold = @()
for ($i = 0; $i -lt $N; $i++) {
    $ms = Invoke-VmMs @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem",
        '--exit-on-boot', '--quiet', '--boot-marker', $marker, '--cmdline', $cmdline)
    if ($ms -ne $null) { $cold += $ms }
}
Write-Host ("  cold:    {0}" -f (Format-Median -Vals $cold))

Write-Host "== taking snapshot at the fully warmed point (pandas/numpy imported + computation warmed) =="
if (Test-Path $Snap) { Remove-Item -Recurse -Force $Snap }
Invoke-VmMs @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem",
    '--snapshot', $Snap, '--quiet', '--cmdline', $cmdline) | Out-Null
if (-not (Test-Path (Join-Path $Snap 'state.bin'))) { throw '  snapshot capture failed' }
$alloc = 0
& fsutil sparse queryrange (Join-Path $Snap 'mem.bin') 2>$null | ForEach-Object {
    if ($_ -match 'length:\s*(0x[0-9a-fA-F]+)') { $alloc += [Convert]::ToInt64($Matches[1], 16) }
}
$stateB = (Get-Item (Join-Path $Snap 'state.bin')).Length
Write-Host ("  snapshot: {0} (state.bin {1} B; mem.bin footprint ~{2:N0} MiB on disk)" -f $Snap, $stateB, ($alloc / 1MB))

Write-Host "== restore -> pandas result (resume the warmed interpreter) =="
$rest = @()
for ($i = 0; $i -lt $N; $i++) {
    $ms = Invoke-VmMs @('--restore', $Snap, '--mem', "$Mem", '--exit-on-boot', '--quiet', '--boot-marker', $marker) 30
    if ($ms -ne $null) { $rest += $ms }
}
Write-Host ("  restore: {0}" -f (Format-Median -Vals $rest))

Remove-Item -Recurse -Force $Snap -ErrorAction SilentlyContinue
