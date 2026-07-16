<#
.SYNOPSIS
    Direct-boot an interactive Python interpreter straight from a snapshot on Windows (WHP backend).
    Mirrors scripts/snapshot-boot.sh.

.DESCRIPTION
    Restore the VM captured at the point a warmed CPython interpreter (alpine/repl.py) requested a
    snapshot and resume it into a live ">>>" prompt on this console, skipping the kernel boot and the
    entire Python startup. Unlike snapshot-demo.ps1 (a benchmark that always recaptures), this reuses
    the snapshot: it is captured once on first use via a one-off cold boot, so every later run drops
    you at the interpreter prompt in milliseconds.

    Requires the Python initramfs (build\initramfs-python.cpio.gz) for the one-off capture. That
    artifact is produced by scripts/build-python-initramfs.sh, which needs apk/pip (network) -- build
    it on a networked machine (or via the Docker toolchain) and copy it in. Once a snapshot exists in
    -Snap, later runs need only the VMM.

.EXAMPLE
    scripts\snapshot-boot.ps1
    scripts\snapshot-boot.ps1 -Mem 512
    scripts\snapshot-boot.ps1 -SmokeTest
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [string]$Snap,
    [int]$Mem = 512,
    # Resume noninteractively, verify the Python banner, and exit. Intended for CI.
    [switch]$SmokeTest
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs-python.cpio.gz' }
if (-not $Snap) { $Snap = Join-Path $repo 'build\pyrepl' }

# `pyapp=repl.py` tells the initramfs init to run the interactive REPL app rather than the
# hello-world demo. The kernel stays quiet; capture output is redirected so failures are reported.
$cmdline = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 pyapp=repl.py'

# Windows PowerShell 5.1 lacks ProcessStartInfo.StandardInputEncoding. Process.Start copies the
# console input encoding into redirected stdin, so scope it to no-BOM UTF-8 while opening the pipe.
function Start-ProcessWithoutStdinBom([System.Diagnostics.ProcessStartInfo]$StartInfo) {
    $originalEncoding = [Console]::InputEncoding
    try {
        [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
        return [System.Diagnostics.Process]::Start($StartInfo)
    }
    finally {
        [Console]::InputEncoding = $originalEncoding
    }
}

if (-not (Test-Path $bin)) { throw "build the VMM first: cargo build --release" }

# Capture the snapshot on first use: cold-boot the interpreter once and let repl.py request a
# snapshot (control port 0x605), which the VMM captures into $Snap before exiting. Reused as long
# as $Snap\{state,mem}.bin exist -- remove the directory to force a fresh capture.
if (-not (Test-Path (Join-Path $Snap 'state.bin')) -or -not (Test-Path (Join-Path $Snap 'mem.bin'))) {
    if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
    if (-not (Test-Path $Initrd)) { throw "missing python initramfs: $Initrd (scripts/build-python-initramfs.sh, needs network)" }
    Write-Host ">> no snapshot at $Snap yet; capturing a warmed interpreter (one-off cold boot)"
    if (Test-Path $Snap) { Remove-Item -Recurse -Force $Snap }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $capArgs = @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--snapshot', $Snap, '--log-level', 'warn', '--cmdline', $cmdline)
    $psi.Arguments = ($capArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = Start-ProcessWithoutStdinBom $psi
    $p.StandardInput.BaseStream.Close()
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $timedOut = -not $p.WaitForExit(40000)
    if ($timedOut) { try { $p.Kill() } catch {}; $p.WaitForExit(2000) | Out-Null }
    $text = $errTask.Result + "`n" + $outTask.Result
    $diagnostic = (($text -split '\r?\n' | Where-Object { $_ } | Select-Object -Last 20) -join "`n")
    if ($timedOut) { throw "snapshot capture timed out`n$diagnostic" }
    if ($p.ExitCode -ne 0) { throw "snapshot capture exited $($p.ExitCode)`n$diagnostic" }
    if (-not (Test-Path (Join-Path $Snap 'state.bin'))) { throw "snapshot capture failed`n$diagnostic" }
}

Write-Host ">> resuming interactive Python interpreter from snapshot $Snap (Ctrl-D or exit() to quit)"
if ($SmokeTest) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $restoreArgs = @('--restore', $Snap, '--mem', "$Mem")
    $psi.Arguments = ($restoreArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false

    $p = Start-ProcessWithoutStdinBom $psi
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $inputBytes = [System.Text.UTF8Encoding]::new($false).GetBytes("exit()`n")
    $p.StandardInput.BaseStream.Write($inputBytes, 0, $inputBytes.Length)
    $p.StandardInput.BaseStream.Close()

    $timedOut = -not $p.WaitForExit(30000)
    if ($timedOut) { try { $p.Kill() } catch {}; $p.WaitForExit(2000) | Out-Null }
    $text = $errTask.Result + "`n" + $outTask.Result
    Write-Host $text
    if ($timedOut) { throw 'snapshot restore smoke test timed out' }
    if ($p.ExitCode -ne 0) { throw "snapshot restore smoke test exited $($p.ExitCode)" }
    if ($text -notmatch 'resumed from snapshot') { throw 'restored Python banner was not observed' }
    exit 0
}

& $bin --restore $Snap --mem "$Mem"
exit $LASTEXITCODE
