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
    scripts\snapshot-boot.ps1 -Mem 256
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [string]$Snap,
    [int]$Mem = 512
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs-python.cpio.gz' }
if (-not $Snap)   { $Snap   = Join-Path $repo 'build\pyrepl' }

# `pyapp=repl.py` tells the initramfs init to run the interactive REPL app rather than the
# hello-world demo. Capture the snapshot with the console quiet so nothing pollutes it.
$cmdline = 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 pyapp=repl.py'

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
    $capArgs = @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--snapshot', $Snap, '--quiet', '--cmdline', $cmdline)
    $psi.Arguments = ($capArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardInput.Close()   # the guest sees stdin EOF, like `< /dev/null`
    if (-not $p.WaitForExit(40000)) { try { $p.Kill() } catch {}; throw "snapshot capture timed out" }
    if (-not (Test-Path (Join-Path $Snap 'state.bin'))) { throw "snapshot capture failed" }
}

Write-Host ">> resuming interactive Python interpreter from snapshot $Snap (Ctrl-D or exit() to quit)"
& $bin --restore $Snap --mem "$Mem"
exit $LASTEXITCODE
