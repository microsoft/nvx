<#
.SYNOPSIS
    Networking + snapshot benchmark for the micro-VM on Windows (WHP backend). Mirrors
    scripts/bench-net-snapshot.sh.

.DESCRIPTION
    Measures how long to reach a working-network guest by
      1. cold boot with -Net (kernel boot + virtio-net probe + guest ifconfig), versus
      2. restore from a snapshot of a warmed, network-configured guest.
    Unlike the KVM script no host TAP (and thus no admin rights) is needed: the WHP backend
    supplies networking through a built-in user-mode NAT. The snapshot is captured once via the
    alpine/init `netsnap` token, which configures the link, checks it, and asks the VMM to
    snapshot; on restore the guest re-checks the link and prints NETSNAP-RESTORE-OK.

.EXAMPLE
    scripts\bench-net-snapshot.ps1
    scripts\bench-net-snapshot.ps1 -Mem 256 -N 10 -Net 10.0.0.2/24
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [string]$Snap,
    [int]$Mem = 256,
    [int]$N = 10,
    [string]$Net = '10.0.0.2/24'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs.cpio.gz' }
if (-not $Snap)   { $Snap   = Join-Path $repo 'build\netsnap' }

if (-not (Test-Path $bin))    { throw "build the VMM first: cargo build --release" }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing initrd: $Initrd" }

$banner  = 'ALPINE-MICROVM-BOOT-OK'
$rmark   = 'NETSNAP-RESTORE-OK'
$cmdline = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'

# Runs the VMM once; returns @{ Wall = <ms>; Ms = <internal ms or $null>; Exited = <bool> }.
# Both pipes are drained concurrently via ReadToEndAsync so a chatty guest can't fill the pipe
# buffer and block (PowerShell -Action handlers would not run while the main thread is parked in
# WaitForExit, but the async stream reads run on threadpool threads and keep draining).
function Invoke-Vm {
    param([string[]]$VmArgs, [int]$TimeoutSec = 40)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($VmArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardInput.Close()   # no interactive input for benchmarks; the guest sees stdin EOF
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $ok = $p.WaitForExit($TimeoutSec * 1000)
    if (-not $ok) { try { $p.Kill() } catch {}; $p.WaitForExit(2000) | Out-Null }
    $sw.Stop()
    $text = $errTask.Result + "`n" + $outTask.Result
    $ms = if ($text -match '(?:cold-start|restore):\s*([0-9.]+)') { [double]$Matches[1] } else { $null }
    [pscustomobject]@{ Wall = $sw.Elapsed.TotalMilliseconds; Ms = $ms; Exited = $ok }
}

function Format-Median {
    param([double[]]$Vals)
    $s = @($Vals | Where-Object { $_ -ne $null } | Sort-Object)
    if (-not $s.Count) { return 'NO DATA' }
    $md = if ($s.Count % 2) { $s[[int](($s.Count - 1) / 2)] } else { ($s[$s.Count/2 - 1] + $s[$s.Count/2]) / 2 }
    '{0,7:N1} ms  (min {1:N1}, max {2:N1}, n={3})' -f $md, $s[0], $s[-1], $s.Count
}

Write-Host "networking + snapshot benchmark, median of $N, ${Mem} MiB, 1 vCPU, -Net $Net (user-mode NAT)"
Write-Host ''

Write-Host '== cold boot -> working-network shell (kernel boot + virtio-net + ifconfig) =='
$cold = @()
for ($i = 0; $i -lt $N; $i++) {
    $r = Invoke-Vm @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--net', $Net,
        '--exit-on-boot', '--quiet', '--boot-marker', $banner, '--cmdline', $cmdline)
    if ($r.Ms -ne $null) { $cold += $r.Ms }
}
Write-Host ("  cold  (guest start -> marker): {0}" -f (Format-Median -Vals $cold))

Write-Host '== capture a warmed, network-configured snapshot (one-off) =='
if (Test-Path $Snap) { Remove-Item -Recurse -Force $Snap }
Invoke-Vm @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--net', $Net,
    '--snapshot', $Snap, '--quiet', '--cmdline', "$cmdline netsnap") | Out-Null
if (-not (Test-Path (Join-Path $Snap 'state.bin'))) { throw '  snapshot capture failed' }
$alloc = 0
& fsutil sparse queryrange (Join-Path $Snap 'mem.bin') 2>$null | ForEach-Object {
    if ($_ -match 'length:\s*(0x[0-9a-fA-F]+)') { $alloc += [Convert]::ToInt64($Matches[1], 16) }
}
$stateB = (Get-Item (Join-Path $Snap 'state.bin')).Length
Write-Host ("  snapshot: {0} (state.bin {1} B; mem.bin footprint ~{2:N0} MiB on disk)" -f $Snap, $stateB, ($alloc / 1MB))

Write-Host '== restore -> working-network shell (resume + rebuild NAT + verify link) =='
$restMs = @(); $restWall = @()
for ($i = 0; $i -lt $N; $i++) {
    $r = Invoke-Vm @('--restore', $Snap, '--mem', "$Mem", '--exit-on-boot', '--quiet', '--boot-marker', $rmark) 30
    if ($r.Ms -ne $null) { $restMs += $r.Ms; $restWall += $r.Wall }
}
Write-Host ("  restore (guest resume -> marker): {0}" -f (Format-Median -Vals $restMs))

Write-Host '== end-to-end wall-clock (process start -> exit) =='
Write-Host ("  restore wall-clock             : {0}" -f (Format-Median -Vals $restWall))

Remove-Item -Recurse -Force $Snap -ErrorAction SilentlyContinue
