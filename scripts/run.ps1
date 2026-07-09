<#
.SYNOPSIS
    Convenience launcher for the micro-VM on Windows (WHP backend).

.DESCRIPTION
    Mirrors scripts/run.sh. Defaults the kernel and initramfs to the artifacts produced by
    scripts\build-linux-artifacts.ps1 (build\vmlinux, build\initramfs.cpio.gz).

.EXAMPLE
    scripts\run.ps1
    scripts\run.ps1 -Mem 256 -Quiet
    scripts\run.ps1 -ExitOnBoot          # boot, print cold-start time, and stop
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [int]$Mem = 512,
    [string]$Cmdline = "earlycon=xe9 console=hvc0 reboot=t panic=-1",
    [switch]$Quiet,
    [switch]$ExitOnBoot,
    # Use the debug build instead of release.
    [switch]$Debug
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

if (-not $Kernel) { $Kernel = Join-Path $repo "build\vmlinux" }
if (-not $Initrd) { $Initrd = Join-Path $repo "build\initramfs.cpio.gz" }

$profile = if ($Debug) { "debug" } else { "release" }
$bin = Join-Path $repo "target\$profile\microvm.exe"
if (-not (Test-Path $bin)) {
    throw "microvm.exe not found at $bin. Build it first: cargo build $(if (-not $Debug) {'--release'})"
}
if (-not (Test-Path $Kernel)) {
    throw "kernel image not found at $Kernel. Build it: scripts\build-linux-artifacts.ps1"
}

$args = @("--kernel", $Kernel, "--initrd", $Initrd, "--mem", "$Mem", "--cmdline", $Cmdline)
if ($Quiet) { $args += "--quiet" }
if ($ExitOnBoot) { $args += "--exit-on-boot" }

& $bin @args
exit $LASTEXITCODE
