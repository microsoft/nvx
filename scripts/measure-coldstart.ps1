<#
.SYNOPSIS
    Cold-start benchmark for the micro-VM on Windows (WHP backend). Mirrors
    scripts/measure-coldstart.sh.

.DESCRIPTION
    Measures cold-start time (first guest instruction -> boot marker) for several console
    configurations and prints the median of N runs each. Requires a built VMM (release),
    kernel and initramfs (see scripts\build-linux-artifacts.ps1).

.EXAMPLE
    scripts\measure-coldstart.ps1
    scripts\measure-coldstart.ps1 -Mem 256 -N 10
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [int]$Mem = 512,
    [int]$N = 6
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs.cpio.gz' }

if (-not (Test-Path $bin))    { throw "build the VMM first: cargo build --release" }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing initrd: $Initrd" }

# Runs the VMM once with the given argument list, waits up to $TimeoutSec, and returns the
# "cold-start:"/"restore:" milliseconds the VMM prints (or $null if the marker was not seen).
# Both pipes are drained concurrently via ReadToEndAsync so a chatty ("loud") guest cannot fill
# the ~64 KB pipe buffer and block before the marker -- PowerShell -Action event handlers would
# not run here because the main thread is parked in WaitForExit, but the async stream reads run
# on threadpool threads and keep draining regardless.
function Invoke-VmMs {
    param([string[]]$VmArgs, [int]$TimeoutSec = 30)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($VmArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
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
    '{0,8:N1} ms  (min {1:N0}, max {2:N0}, n={3})' -f $md, $s[0], $s[-1], $s.Count
}

# Runs N measurements to $Marker with the given extra args and returns a formatted median.
function Measure-Cold {
    param([string]$Marker, [string[]]$Extra, [int]$Ram = $Mem)
    $vals = @()
    for ($i = 0; $i -lt $N; $i++) {
        $a = @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Ram", '--exit-on-boot', '--boot-marker', $Marker) + $Extra
        $ms = Invoke-VmMs -VmArgs $a
        if ($ms -ne $null) { $vals += $ms }
    }
    Format-Median -Vals $vals
}

$runinit = 'Run /init as init process'
$banner  = 'ALPINE-MICROVM-BOOT-OK'
$base    = 'earlycon=xe9 console=hvc0 reboot=t panic=-1'
$silent  = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'
# WHP has no kvm-clock; the guest uses the TSC clocksource, so the fast line drops that token.
$fast    = 'tsc=reliable no_timer_check random.trust_cpu=on rcupdate.rcu_expedited=1 nokaslr mitigations=off cryptomgr.notests earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'

Write-Host "cold-start (guest start -> marker), median of $N runs, ${Mem} MiB, 1 vCPU"
Write-Host ''
Write-Host 'portb console (0xE9 -> hvc0), to kernel->userspace handoff, full logs:'
Write-Host ("  loud (rendered)    : {0}" -f (Measure-Cold -Marker $runinit -Extra @('--cmdline', $base)))
Write-Host ("  quiet (discarded)  : {0}" -f (Measure-Cold -Marker $runinit -Extra @('--quiet', '--cmdline', $base)))
Write-Host ''
Write-Host 'end-to-end (to interactive shell):'
Write-Host ("  loud full logs     : {0}" -f (Measure-Cold -Marker $banner -Extra @('--cmdline', $base)))
Write-Host ("  silent (quiet klog): {0}" -f (Measure-Cold -Marker $banner -Extra @('--quiet', '--cmdline', $silent)))
Write-Host ''
Write-Host 'fastest (silent, 128 MiB, tuned cmdline):'
Write-Host ("  fast               : {0}" -f (Measure-Cold -Marker $banner -Extra @('--quiet', '--cmdline', $fast) -Ram 128))
