<#
.SYNOPSIS
    Builds the Python RAM initramfs (CPython + numpy/pandas + the snapshot apps) with Docker.
    Mirrors `make python-initramfs` for Windows.

.DESCRIPTION
    The Python initramfs bundles a full CPython interpreter and the numpy/pandas stack installed
    into an Alpine mini root filesystem with `apk`. That needs a Linux userland (musl apk) and
    network access, so this wrapper drives docker/Dockerfile's `python-artifacts` stage and exports
    the result onto the host with `docker build --output`.

    Produces:
      <Dest>\initramfs-python.cpio.gz

    Used by scripts\snapshot-demo.ps1, scripts\snapshot-boot.ps1 and scripts\bench-net-snapshot-py.ps1.

.EXAMPLE
    scripts\build-python-initramfs.ps1
    scripts\build-python-initramfs.ps1 -Dest build -Aver 3.24.1
#>
[CmdletBinding()]
param(
    # Host directory to write the artifact into.
    [string]$Dest = "build",
    # Alpine version / branch for the rootfs.
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
    Write-Host ">> building the Python initramfs into '$Dest' (Alpine $Aver; downloads CPython + numpy/pandas)"
    docker build `
        -f docker/Dockerfile `
        --target python-artifacts `
        --build-arg "AVER=$Aver" `
        --build-arg "ABRANCH=$Abranch" `
        --output "type=local,dest=$Dest" `
        .
    if ($LASTEXITCODE -ne 0) { throw "docker build failed (exit $LASTEXITCODE)" }

    $out = Join-Path $Dest "initramfs-python.cpio.gz"
    Write-Host ">> done:"
    Get-Item $out | Format-Table Name, Length, FullName -AutoSize
}
finally {
    Pop-Location
}
