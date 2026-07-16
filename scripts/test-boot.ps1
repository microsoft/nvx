<#
.SYNOPSIS
    End-to-end boot smoke test for the micro-VM on Windows (WHP backend).

.DESCRIPTION
    Mirrors scripts/test-boot.sh. Boots the guest, sends a small command sequence to its
    console, and succeeds only when the initramfs userspace marker appears in the output.

.EXAMPLE
    scripts\test-boot.ps1
    scripts\test-boot.ps1 -Kernel build\vmlinux -Initrd build\initramfs.cpio.gz
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [int]$Mem = 512,
    [int]$TimeoutSec = 90
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs.cpio.gz' }

$marker = 'ALPINE-MICROVM-BOOT-OK'
$cmdline = 'earlycon=xe9 console=hvc0 reboot=t panic=-1'

if (-not (Test-Path $bin)) { throw 'build the VMM first: cargo build --release' }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing initrd: $Initrd (scripts\build-linux-artifacts.ps1)" }

$vmArgs = @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--cmdline', $cmdline)
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $bin
$psi.Arguments = ($vmArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false

$process = [System.Diagnostics.Process]::Start($psi)
$stdoutTask = $process.StandardOutput.ReadToEndAsync()
$stderrTask = $process.StandardError.ReadToEndAsync()
$process.StandardInput.WriteLine('cat /etc/alpine-release')
$process.StandardInput.WriteLine('uname -a')
$process.StandardInput.WriteLine('reboot -f')
$process.StandardInput.Close()

if (-not $process.WaitForExit($TimeoutSec * 1000)) {
    try { $process.Kill() } catch {}
    $process.WaitForExit(2000) | Out-Null
}

$text = $stderrTask.Result + "`n" + $stdoutTask.Result
if ($text.Contains($marker)) {
    Write-Host "PASS: guest reached userspace ($marker)"
    $alpineRelease = [regex]::Match($text, '(?m)^3\.[0-9]+\.[0-9]+[^\r\n]*')
    if ($alpineRelease.Success) { Write-Host "  alpine-release: $($alpineRelease.Value.Trim())" }
    $uname = [regex]::Match($text, '(?m)^Linux .* x86_64[^\r\n]*')
    if ($uname.Success) { Write-Host "  uname: $($uname.Value.Trim())" }
    exit 0
}

Write-Host "FAIL: '$marker' not found in boot output"
Write-Host '--- last 25 lines ---'
($text -split '\r?\n' | Select-Object -Last 25) | Write-Host
exit 1
