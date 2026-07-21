#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$DescriptorPath,
    [switch]$KeepDescriptor
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $DescriptorPath -PathType Leaf)) {
    Write-Host "HCN descriptor is already absent: $DescriptorPath"
    return
}

Import-Module (Join-Path $PSScriptRoot 'hcn-tools.psm1') -Force
$descriptor = Get-Content -LiteralPath $DescriptorPath -Raw | ConvertFrom-Json
Remove-NvxHcnEndpoint $descriptor
if (-not $KeepDescriptor) {
    Remove-Item -LiteralPath $DescriptorPath -Force
}
Write-Host "Removed externally managed HCN endpoint $($descriptor.endpointId)"
