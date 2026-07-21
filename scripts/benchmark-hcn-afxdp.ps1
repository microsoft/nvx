#requires -Version 5.1

[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Runs = 5,
    [string]$Microvm,
    [string]$Kernel,
    [string]$Initrd,
    [Parameter(Mandatory)]
    [string]$EndpointConfig,
    [string]$GuestAddress = $(
        if ($env:NVX_HCN_AFXDP_GUEST_ADDRESS) {
            $env:NVX_HCN_AFXDP_GUEST_ADDRESS
        } else {
            '192.168.240.2'
        }
    ),
    [ValidateRange(1, 30)]
    [int]$PrefixLength = 24,
    [string]$Gateway = $(
        if ($env:NVX_HCN_AFXDP_GATEWAY) {
            $env:NVX_HCN_AFXDP_GATEWAY
        } else {
            '192.168.240.1'
        }
    ),
    [ValidateRange(576, 4082)]
    [int]$Mtu = 1500,
    [ValidateRange(1, 65535)]
    [int]$WebPort = 8099,
    [ValidateRange(30, 900)]
    [int]$TimeoutSeconds = 300,
    [string]$LogDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Microvm) { $Microvm = Join-Path $PSScriptRoot '..\target\release\microvm.exe' }
if (-not $Kernel) { $Kernel = Join-Path $PSScriptRoot '..\build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $PSScriptRoot '..\build\initramfs.cpio.gz' }
if (-not $LogDirectory) {
    $LogDirectory = Join-Path $PSScriptRoot '..\build\performance\hcn-afxdp-runs'
}

function Get-Median {
    param([double[]]$Values)

    [double[]]$ordered = $Values | Sort-Object
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if ($ordered.Count % 2 -eq 1) {
        return $ordered[$middle]
    }
    return ($ordered[$middle - 1] + $ordered[$middle]) / 2
}

function Format-Milliseconds {
    param([double]$Value)

    return $Value.ToString('0.0', [Globalization.CultureInfo]::InvariantCulture)
}

$testScript = Join-Path $PSScriptRoot 'test-hcn-afxdp.ps1'
New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null
$samples = New-Object Collections.Generic.List[double]

Write-Output "HCN AF_XDP verified network benchmark, median of $Runs runs"
for ($run = 1; $run -le $Runs; $run++) {
    $runLog = Join-Path $LogDirectory ("run-{0}.log" -f $run)
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    try {
        & $testScript `
            -Microvm $Microvm `
            -Kernel $Kernel `
            -Initrd $Initrd `
            -EndpointConfig $EndpointConfig `
            -GuestAddress $GuestAddress `
            -PrefixLength $PrefixLength `
            -Gateway $Gateway `
            -Mtu $Mtu `
            -WebPort $WebPort `
            -TimeoutSeconds $TimeoutSeconds `
            -LogPath $runLog
    } catch {
        throw "HCN AF_XDP benchmark run $run/$Runs failed: $($_.Exception.Message)"
    } finally {
        $stopwatch.Stop()
    }
    $samples.Add($stopwatch.Elapsed.TotalMilliseconds)
    Write-Output (
        "  verified network: run {0}/{1} complete (wall {2} ms)" -f
        $run,
        $Runs,
        (Format-Milliseconds $stopwatch.Elapsed.TotalMilliseconds)
    )
}

$median = Get-Median $samples.ToArray()
$minimum = ($samples | Measure-Object -Minimum).Minimum
$maximum = ($samples | Measure-Object -Maximum).Maximum
Write-Output (
    "  verified network wall : {0} ms  (min {1}, max {2}, n={3})" -f
    (Format-Milliseconds $median),
    (Format-Milliseconds $minimum),
    (Format-Milliseconds $maximum),
    $samples.Count
)
Write-Output '  verified marker       : NVX-HCN-AFXDP-SMOKE-OK'
