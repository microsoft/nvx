#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$OutputPath,
    [string]$GuestAddress = '192.168.240.2',
    [ValidateRange(1, 30)]
    [int]$PrefixLength = 24,
    [string]$Gateway = '192.168.240.1',
    [string[]]$DnsServers = @('1.1.1.1'),
    [string]$MacAddress,
    [switch]$AttachToHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-IPv4Bytes {
    param([string]$Value, [string]$Name)

    $parsed = $null
    if (-not [Net.IPAddress]::TryParse($Value, [ref]$parsed) -or
        $parsed.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork) {
        throw "$Name must be an IPv4 address, got '$Value'"
    }
    return $parsed.GetAddressBytes()
}

function ConvertFrom-IPv4Bytes {
    param([byte[]]$Bytes)

    return ([Net.IPAddress]::new($Bytes)).ToString()
}

if ($env:OS -ne 'Windows_NT') {
    throw 'HCN endpoint setup requires Windows'
}
if (Test-Path -LiteralPath $OutputPath) {
    throw "HCN endpoint descriptor already exists; clean it up before reuse: $OutputPath"
}
if (-not $DnsServers -or $DnsServers.Count -eq 0) {
    throw 'DnsServers must contain at least one IPv4 address'
}

$guestBytes = ConvertTo-IPv4Bytes $GuestAddress GuestAddress
$gatewayBytes = ConvertTo-IPv4Bytes $Gateway Gateway
foreach ($server in $DnsServers) {
    $null = ConvertTo-IPv4Bytes $server DnsServers
}

$networkBytes = New-Object byte[] 4
$broadcastBytes = New-Object byte[] 4
$remaining = $PrefixLength
for ($index = 0; $index -lt 4; $index++) {
    $bits = [Math]::Min(8, $remaining)
    $mask = if ($bits -eq 0) { 0 } else { 256 - [Math]::Pow(2, 8 - $bits) }
    $networkBytes[$index] = $guestBytes[$index] -band [byte]$mask
    $broadcastBytes[$index] = $networkBytes[$index] -bor [byte](255 - $mask)
    $remaining -= $bits
}
$expectedGateway = [byte[]]$networkBytes.Clone()
$expectedGateway[3]++
$networkAddress = ConvertFrom-IPv4Bytes $networkBytes
$broadcastAddress = ConvertFrom-IPv4Bytes $broadcastBytes
$expectedGatewayAddress = ConvertFrom-IPv4Bytes $expectedGateway
if ($Gateway -ne $expectedGatewayAddress) {
    throw "Gateway must be the first usable address $expectedGatewayAddress in $networkAddress/$PrefixLength"
}
if ($GuestAddress -in @($networkAddress, $broadcastAddress, $Gateway)) {
    throw "GuestAddress $GuestAddress is not usable in $networkAddress/$PrefixLength"
}

if (-not $MacAddress) {
    $bytes = [Guid]::NewGuid().ToByteArray()
    $MacAddress = '00-15-5D-52-{0:X2}-{1:X2}' -f (0xC0 + ($bytes[0] -band 0x0F)), $bytes[1]
}
if ($MacAddress -notmatch '^(?i:[0-9a-f]{2}-){5}[0-9a-f]{2}$') {
    throw "MacAddress must contain six hexadecimal octets separated by '-', got '$MacAddress'"
}

$parent = Split-Path -Parent $OutputPath
if ($parent) {
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
}
Import-Module (Join-Path $PSScriptRoot 'hcn-tools.psm1') -Force
$descriptor = $null
try {
    $descriptor = New-NvxHcnEndpoint `
        -GuestAddress $GuestAddress `
        -PrefixLength $PrefixLength `
        -NetworkAddress $networkAddress `
        -Gateway $Gateway `
        -DnsServers $DnsServers `
        -MacAddress $MacAddress `
        -AttachToHost:$AttachToHost

    $utf8 = New-Object Text.UTF8Encoding($false)
    $bytes = $utf8.GetBytes(($descriptor | ConvertTo-Json -Depth 6))
    $stream = $null
    try {
        $stream = [IO.File]::Open(
            $OutputPath,
            [IO.FileMode]::CreateNew,
            [IO.FileAccess]::Write,
            [IO.FileShare]::None
        )
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
} catch {
    $primaryFailure = $_
    if ($null -ne $descriptor) {
        try {
            Remove-NvxHcnEndpoint $descriptor
        } catch {
            throw "$($primaryFailure.Exception.Message); rollback failed for endpoint $($descriptor.endpointId): $($_.Exception.Message)"
        }
    }
    throw $primaryFailure
}

Write-Host "Created externally managed HCN endpoint $($descriptor.endpointId)"
Write-Host "Descriptor: $OutputPath"
if ($descriptor.hostAttached) {
    Write-Host "Host vNIC: ifIndex $($descriptor.interfaceIndex), LUID $($descriptor.interfaceLuid)"
}
