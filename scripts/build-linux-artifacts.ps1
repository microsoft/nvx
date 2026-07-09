<#
.SYNOPSIS
    Builds the microvm Linux artifacts (PVH vmlinux + Alpine initramfs) with Docker.

.DESCRIPTION
    The kernel needs a Linux/GCC toolchain to compile, which is not available natively on
    Windows. This wrapper drives docker/Dockerfile's `artifacts` stage and exports the two
    build outputs onto the host with `docker build --output`.

    Produces:
      <Dest>\vmlinux
      <Dest>\initramfs.cpio.gz

.EXAMPLE
    scripts\build-linux-artifacts.ps1
    scripts\build-linux-artifacts.ps1 -Dest build -Kver 6.18.38
#>
[CmdletBinding()]
param(
    # Host directory to write the artifacts into.
    [string]$Dest = "build",
    # Linux kernel version to build.
    [string]$Kver = "6.18.38",
    # Alpine version / branch for the initramfs.
    [string]$Aver = "3.24.1",
    [string]$Abranch = "v3.24"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "docker was not found on PATH. Install Docker Desktop (with the Linux engine) first."
}

Push-Location $repo
try {
    Write-Host ">> building Linux artifacts into '$Dest' (kernel $Kver, Alpine $Aver)"
    docker build `
        -f docker/Dockerfile `
        --target artifacts `
        --build-arg "KVER=$Kver" `
        --build-arg "AVER=$Aver" `
        --build-arg "ABRANCH=$Abranch" `
        --output "type=local,dest=$Dest" `
        .
    if ($LASTEXITCODE -ne 0) { throw "docker build failed (exit $LASTEXITCODE)" }

    $vmlinux = Join-Path $Dest "vmlinux"
    $initrd = Join-Path $Dest "initramfs.cpio.gz"
    Write-Host ">> done:"
    Get-Item $vmlinux, $initrd | Format-Table Name, Length, FullName -AutoSize
}
finally {
    Pop-Location
}
