# Copyright(c) The microvm authors.
# Licensed under the MIT License.

Set-StrictMode -Version Latest

if (-not ('Nvx.Hcn.NativeMethods' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace Nvx.Hcn
{
    public static class NativeMethods
    {
        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnCreateNetwork(
            ref Guid id, string settings, out IntPtr network, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnCloseNetwork(IntPtr network);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnEnumerateNetworks(
            string query, out IntPtr networks, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnOpenNetwork(
            ref Guid id, out IntPtr network, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnQueryNetworkProperties(
            IntPtr network, string query, out IntPtr properties, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnDeleteNetwork(ref Guid id, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnCreateEndpoint(
            IntPtr network, ref Guid id, string settings, out IntPtr endpoint,
            out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnOpenEndpoint(
            ref Guid id, out IntPtr endpoint, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnQueryEndpointProperties(
            IntPtr endpoint, string query, out IntPtr properties, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnCloseEndpoint(IntPtr endpoint);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnDeleteEndpoint(ref Guid id, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnEnumerateNamespaces(
            string query, out IntPtr namespaces, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnOpenNamespace(
            ref Guid id, out IntPtr hcnNamespace, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnQueryNamespaceProperties(
            IntPtr hcnNamespace, string query, out IntPtr properties, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnModifyNamespace(
            IntPtr hcnNamespace, string settings, out IntPtr errorRecord);

        [DllImport("computenetwork.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern int HcnCloseNamespace(IntPtr hcnNamespace);

        [DllImport("iphlpapi.dll", ExactSpelling = true)]
        public static extern uint ConvertInterfaceIndexToLuid(
            uint interfaceIndex, out ulong interfaceLuid);
    }
}
'@
}

$script:HcnQuery = '{"SchemaVersion":{"Major":2,"Minor":0},"Flags":1}'
$script:HcnNetworkNotFound = [uint32]::Parse(
    '803B0001',
    [Globalization.NumberStyles]::HexNumber
)
$script:HcnEndpointNotFound = [uint32]::Parse(
    '803B0002',
    [Globalization.NumberStyles]::HexNumber
)
$script:HcnEndpointNotAttached = [uint32]::Parse(
    '803B0034',
    [Globalization.NumberStyles]::HexNumber
)
$script:Win32NotFound = [uint32]::Parse(
    '80070490',
    [Globalization.NumberStyles]::HexNumber
)

function ConvertTo-HcnStatusCode {
    param([int]$Status)

    return [BitConverter]::ToUInt32([BitConverter]::GetBytes($Status), 0)
}

function Get-HcnAllocatedString {
    param([IntPtr]$Pointer)

    if ($Pointer -eq [IntPtr]::Zero) {
        return ''
    }
    try {
        return [Runtime.InteropServices.Marshal]::PtrToStringUni($Pointer)
    } finally {
        [Runtime.InteropServices.Marshal]::FreeCoTaskMem($Pointer)
    }
}

function Complete-HcnCall {
    param(
        [int]$Status,
        [IntPtr]$ErrorRecord,
        [string]$Operation,
        [uint32[]]$IgnoredStatus = @()
    )

    $record = Get-HcnAllocatedString $ErrorRecord
    $code = ConvertTo-HcnStatusCode $Status
    if ($Status -lt 0 -and $IgnoredStatus -notcontains $code) {
        if ([string]::IsNullOrWhiteSpace($record)) { $record = '<none>' }
        throw ('{0} failed with HRESULT 0x{1:X8}; error record: {2}' -f $Operation, $code, $record)
    }
}

function Close-HcnHandle {
    param(
        [IntPtr]$Handle,
        [ValidateSet('Network', 'Endpoint', 'Namespace')]
        [string]$Kind
    )

    if ($Handle -eq [IntPtr]::Zero) { return }
    $status = switch ($Kind) {
        'Network' { [Nvx.Hcn.NativeMethods]::HcnCloseNetwork($Handle) }
        'Endpoint' { [Nvx.Hcn.NativeMethods]::HcnCloseEndpoint($Handle) }
        'Namespace' { [Nvx.Hcn.NativeMethods]::HcnCloseNamespace($Handle) }
    }
    Complete-HcnCall $status ([IntPtr]::Zero) "HcnClose$Kind"
}

function Get-HcnEndpointProperties {
    param([Guid]$EndpointId)

    $handle = [IntPtr]::Zero
    $errorRecord = [IntPtr]::Zero
    $status = [Nvx.Hcn.NativeMethods]::HcnOpenEndpoint(
        [ref]$EndpointId,
        [ref]$handle,
        [ref]$errorRecord
    )
    Complete-HcnCall $status $errorRecord "HcnOpenEndpoint($EndpointId)"
    try {
        $properties = [IntPtr]::Zero
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnQueryEndpointProperties(
            $handle,
            $script:HcnQuery,
            [ref]$properties,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnQueryEndpointProperties($EndpointId)"
        return (Get-HcnAllocatedString $properties) | ConvertFrom-Json
    } finally {
        Close-HcnHandle $handle Endpoint
    }
}

function Get-HcnNetworkProperties {
    param([Guid]$NetworkId)

    $handle = [IntPtr]::Zero
    $errorRecord = [IntPtr]::Zero
    $status = [Nvx.Hcn.NativeMethods]::HcnOpenNetwork(
        [ref]$NetworkId,
        [ref]$handle,
        [ref]$errorRecord
    )
    Complete-HcnCall $status $errorRecord "HcnOpenNetwork($NetworkId)"
    try {
        $properties = [IntPtr]::Zero
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnQueryNetworkProperties(
            $handle,
            $script:HcnQuery,
            [ref]$properties,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnQueryNetworkProperties($NetworkId)"
        return (Get-HcnAllocatedString $properties) | ConvertFrom-Json
    } finally {
        Close-HcnHandle $handle Network
    }
}

function Get-NvxIpv4Range {
    param(
        [string]$Address,
        [ValidateRange(0, 32)]
        [int]$PrefixLength
    )

    $parsed = $null
    if (-not [Net.IPAddress]::TryParse($Address, [ref]$parsed) -or
        $parsed.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork) {
        throw "invalid IPv4 address '$Address'"
    }
    $bytes = $parsed.GetAddressBytes()
    [uint64]$value = [uint64]$bytes[0] * 16777216 +
        [uint64]$bytes[1] * 65536 +
        [uint64]$bytes[2] * 256 +
        [uint64]$bytes[3]
    [uint64]$size = [Math]::Pow(2, 32 - $PrefixLength)
    [uint64]$first = [Math]::Floor($value / $size) * $size
    return [pscustomobject]@{
        First = $first
        Last = $first + $size - 1
    }
}

function Assert-HcnPrefixAvailable {
    param(
        [string]$NetworkAddress,
        [ValidateRange(1, 30)]
        [int]$PrefixLength
    )

    $requested = Get-NvxIpv4Range $NetworkAddress $PrefixLength
    $identifiers = [IntPtr]::Zero
    $errorRecord = [IntPtr]::Zero
    $status = [Nvx.Hcn.NativeMethods]::HcnEnumerateNetworks(
        $script:HcnQuery,
        [ref]$identifiers,
        [ref]$errorRecord
    )
    Complete-HcnCall $status $errorRecord 'HcnEnumerateNetworks'
    $networkIds = (Get-HcnAllocatedString $identifiers) | ConvertFrom-Json

    foreach ($value in @($networkIds)) {
        $networkId = [Guid]$value
        $properties = Get-HcnNetworkProperties $networkId
        $ipamsProperty = $properties.PSObject.Properties['Ipams']
        if ($null -eq $ipamsProperty) { continue }
        foreach ($ipam in @($ipamsProperty.Value)) {
            $subnetsProperty = $ipam.PSObject.Properties['Subnets']
            if ($null -eq $subnetsProperty) { continue }
            foreach ($subnet in @($subnetsProperty.Value)) {
                $prefixProperty = $subnet.PSObject.Properties['IpAddressPrefix']
                if ($null -eq $prefixProperty) { continue }
                $prefixText = [string]$prefixProperty.Value
                $separator = $prefixText.LastIndexOf('/')
                if ($separator -le 0) { continue }
                $address = $prefixText.Substring(0, $separator)
                $existingPrefix = 0
                if (-not [int]::TryParse(
                    $prefixText.Substring($separator + 1),
                    [ref]$existingPrefix
                ) -or $existingPrefix -lt 0 -or $existingPrefix -gt 32) {
                    continue
                }
                try {
                    $existing = Get-NvxIpv4Range $address $existingPrefix
                } catch {
                    continue
                }
                if ($requested.First -le $existing.Last -and
                    $existing.First -le $requested.Last) {
                    throw "requested HCN prefix $NetworkAddress/$PrefixLength overlaps network $networkId prefix $prefixText"
                }
            }
        }
    }
}

function Get-HcnHostDefaultNamespace {
    $identifiers = [IntPtr]::Zero
    $errorRecord = [IntPtr]::Zero
    $status = [Nvx.Hcn.NativeMethods]::HcnEnumerateNamespaces(
        $script:HcnQuery,
        [ref]$identifiers,
        [ref]$errorRecord
    )
    Complete-HcnCall $status $errorRecord 'HcnEnumerateNamespaces'
    $namespaceIds = (Get-HcnAllocatedString $identifiers) | ConvertFrom-Json

    foreach ($value in @($namespaceIds)) {
        $namespaceId = [Guid]$value
        $handle = [IntPtr]::Zero
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnOpenNamespace(
            [ref]$namespaceId,
            [ref]$handle,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnOpenNamespace($namespaceId)"
        try {
            $properties = [IntPtr]::Zero
            $errorRecord = [IntPtr]::Zero
            $status = [Nvx.Hcn.NativeMethods]::HcnQueryNamespaceProperties(
                $handle,
                $script:HcnQuery,
                [ref]$properties,
                [ref]$errorRecord
            )
            Complete-HcnCall $status $errorRecord "HcnQueryNamespaceProperties($namespaceId)"
            $document = (Get-HcnAllocatedString $properties) | ConvertFrom-Json
            if ($document.Type -eq 'HostDefault') {
                return $namespaceId
            }
        } finally {
            Close-HcnHandle $handle Namespace
        }
    }
    throw 'HCN did not report a HostDefault namespace'
}

function Set-HcnNamespaceEndpoint {
    param(
        [Guid]$NamespaceId,
        [Guid]$EndpointId,
        [ValidateSet('Add', 'Remove')]
        [string]$Request,
        [switch]$IgnoreMissing
    )

    $handle = [IntPtr]::Zero
    $errorRecord = [IntPtr]::Zero
    $status = [Nvx.Hcn.NativeMethods]::HcnOpenNamespace(
        [ref]$NamespaceId,
        [ref]$handle,
        [ref]$errorRecord
    )
    $code = ConvertTo-HcnStatusCode $status
    if ($IgnoreMissing -and $status -lt 0 -and $code -eq $script:Win32NotFound) {
        $null = Get-HcnAllocatedString $errorRecord
        return
    }
    Complete-HcnCall $status $errorRecord "HcnOpenNamespace($NamespaceId)"
    try {
        $settings = [ordered]@{
            ResourceType = 'Endpoint'
            RequestType = $Request
            Settings = [ordered]@{ EndpointId = $EndpointId.ToString() }
        } | ConvertTo-Json -Depth 4 -Compress
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnModifyNamespace(
            $handle,
            $settings,
            [ref]$errorRecord
        )
        $code = ConvertTo-HcnStatusCode $status
        if ($IgnoreMissing -and
            $Request -eq 'Remove' -and
            $status -lt 0 -and
            $code -in @($script:HcnEndpointNotFound, $script:HcnEndpointNotAttached)) {
            $null = Get-HcnAllocatedString $errorRecord
            return
        }
        Complete-HcnCall $status $errorRecord "HcnModifyNamespace($Request, $EndpointId)"
    } finally {
        Close-HcnHandle $handle Namespace
    }
}

function Wait-NvxHcnInterface {
    param(
        [string]$MacAddress,
        [string]$EndpointName,
        [string]$Gateway,
        [int]$TimeoutSeconds = 30
    )

    $normalizedMac = $MacAddress.Replace('-', '').Replace(':', '')
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $matches = @(Get-NetAdapter -IncludeHidden -ErrorAction SilentlyContinue | Where-Object {
            $_.ifIndex -ne 0 -and
            $_.MacAddress -and
            $_.MacAddress.Replace('-', '').Replace(':', '') -eq $normalizedMac
        })
        if ($matches.Count -eq 1) {
            $adapter = $matches[0]
            [uint64]$luid = 0
            $status = [Nvx.Hcn.NativeMethods]::ConvertInterfaceIndexToLuid(
                [uint32]$adapter.ifIndex,
                [ref]$luid
            )
            if ($status -ne 0) {
                throw "ConvertInterfaceIndexToLuid($($adapter.ifIndex)) failed with Win32 status $status"
            }

            $neighbor = Get-NetNeighbor `
                -InterfaceIndex $adapter.ifIndex `
                -IPAddress $Gateway `
                -ErrorAction SilentlyContinue |
                Where-Object { $_.LinkLayerAddress -and $_.LinkLayerAddress -ne '00-00-00-00-00-00' } |
                Select-Object -First 1
            if ($neighbor) {
                return [pscustomobject][ordered]@{
                    interfaceIndex = [uint32]$adapter.ifIndex
                    interfaceLuid = $luid
                    interfaceAlias = [string]$adapter.Name
                    gatewayMac = [string]$neighbor.LinkLayerAddress
                }
            }
        } elseif ($matches.Count -gt 1) {
            throw "multiple host interfaces expose HCN endpoint MAC $MacAddress"
        }
        [Threading.Tasks.Task]::Delay(50).Wait()
    } while ([DateTime]::UtcNow -lt $deadline)

    throw "timed out resolving host vNIC '$EndpointName' with MAC $MacAddress and gateway $Gateway"
}

function New-NvxHcnEndpoint {
    param(
        [string]$GuestAddress,
        [ValidateRange(1, 30)]
        [int]$PrefixLength,
        [string]$NetworkAddress,
        [string]$Gateway,
        [string[]]$DnsServers,
        [string]$MacAddress,
        [string]$Owner = 'nvx-external',
        [switch]$AttachToHost
    )

    $networkId = [Guid]::NewGuid()
    $endpointId = [Guid]::NewGuid()
    $adapterId = [Guid]::NewGuid()
    $networkName = "nvx-$networkId"
    $endpointName = "nvx-$endpointId-ep"
    $networkHandle = [IntPtr]::Zero
    $endpointHandle = [IntPtr]::Zero
    $networkCreated = $false
    $endpointCreated = $false
    $namespaceId = [Guid]::Empty
    $hostAttached = $false

    Assert-HcnPrefixAvailable $NetworkAddress $PrefixLength

    $networkDocument = [ordered]@{
        SchemaVersion = [ordered]@{ Major = 2; Minor = 0 }
        Owner = $Owner
        Name = $networkName
        Type = 'NAT'
        MacPool = [ordered]@{
            Ranges = @([ordered]@{
                StartMacAddress = '00-15-5D-52-C0-00'
                EndMacAddress = '00-15-5D-52-CF-FF'
            })
        }
        Ipams = @([ordered]@{
            Type = 'Static'
            Subnets = @([ordered]@{
                IpAddressPrefix = "$NetworkAddress/$PrefixLength"
                Routes = @([ordered]@{
                    DestinationPrefix = '0.0.0.0/0'
                    NextHop = $Gateway
                })
            })
        })
    } | ConvertTo-Json -Depth 8 -Compress
    $endpointDocument = [ordered]@{
        SchemaVersion = [ordered]@{ Major = 2; Minor = 0 }
        Owner = $Owner
        Name = $endpointName
        HostComputeNetwork = $networkId.ToString()
        IpConfigurations = @([ordered]@{
            IpAddress = $GuestAddress
            PrefixLength = $PrefixLength
        })
        Routes = @([ordered]@{
            DestinationPrefix = '0.0.0.0/0'
            NextHop = $Gateway
        })
        Dns = [ordered]@{
            ServerList = @($DnsServers)
            Search = @()
        }
        MacAddress = $MacAddress
    } | ConvertTo-Json -Depth 8 -Compress

    try {
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnCreateNetwork(
            [ref]$networkId,
            $networkDocument,
            [ref]$networkHandle,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnCreateNetwork($networkId)"
        $networkCreated = $true

        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnCreateEndpoint(
            $networkHandle,
            [ref]$endpointId,
            $endpointDocument,
            [ref]$endpointHandle,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnCreateEndpoint($endpointId)"
        $endpointCreated = $true

        $propertiesPointer = [IntPtr]::Zero
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnQueryEndpointProperties(
            $endpointHandle,
            $script:HcnQuery,
            [ref]$propertiesPointer,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnQueryEndpointProperties($endpointId)"
        $properties = (Get-HcnAllocatedString $propertiesPointer) | ConvertFrom-Json
        if (-not $properties.MacAddress) {
            throw "HCN endpoint $endpointId has no MAC address"
        }

        if ($AttachToHost) {
            $namespaceId = Get-HcnHostDefaultNamespace
            $hostAttached = $true
            Set-HcnNamespaceEndpoint $namespaceId $endpointId Add
            $interface = Wait-NvxHcnInterface `
                -MacAddress $properties.MacAddress `
                -EndpointName $endpointName `
                -Gateway $Gateway
        } else {
            $interface = [pscustomobject][ordered]@{
                interfaceIndex = [uint32]0
                interfaceLuid = [uint64]0
                interfaceAlias = ''
                gatewayMac = ''
            }
        }

        $descriptor = [pscustomobject][ordered]@{
            version = 1
            owner = $Owner
            networkId = $networkId.ToString()
            endpointId = $endpointId.ToString()
            adapterId = $adapterId.ToString()
            networkName = $networkName
            endpointName = $endpointName
            guestAddress = $GuestAddress
            prefixLength = $PrefixLength
            networkAddress = $NetworkAddress
            gateway = $Gateway
            dnsServers = @($DnsServers)
            macAddress = [string]$properties.MacAddress
            hostAttached = $hostAttached
            hostNamespaceId = if ($hostAttached) { $namespaceId.ToString() } else { '' }
            interfaceIndex = $interface.interfaceIndex
            interfaceLuid = $interface.interfaceLuid
            interfaceAlias = $interface.interfaceAlias
            gatewayMac = $interface.gatewayMac
        }
        Close-HcnHandle $endpointHandle Endpoint
        $endpointHandle = [IntPtr]::Zero
        Close-HcnHandle $networkHandle Network
        $networkHandle = [IntPtr]::Zero
        return $descriptor
    } catch {
        $primaryFailure = $_
        $rollbackFailures = New-Object Collections.Generic.List[string]
        if ($hostAttached) {
            try {
                Set-HcnNamespaceEndpoint $namespaceId $endpointId Remove -IgnoreMissing
            } catch {
                $rollbackFailures.Add("namespace detach: $($_.Exception.Message)")
            }
        }
        try {
            Close-HcnHandle $endpointHandle Endpoint
        } catch {
            $rollbackFailures.Add("endpoint handle close: $($_.Exception.Message)")
        } finally {
            $endpointHandle = [IntPtr]::Zero
        }
        try {
            Close-HcnHandle $networkHandle Network
        } catch {
            $rollbackFailures.Add("network handle close: $($_.Exception.Message)")
        } finally {
            $networkHandle = [IntPtr]::Zero
        }
        if ($endpointCreated) {
            try {
                $errorRecord = [IntPtr]::Zero
                $status = [Nvx.Hcn.NativeMethods]::HcnDeleteEndpoint(
                    [ref]$endpointId,
                    [ref]$errorRecord
                )
                Complete-HcnCall $status $errorRecord "HcnDeleteEndpoint($endpointId)" `
                    -IgnoredStatus @($script:HcnEndpointNotFound)
            } catch {
                $rollbackFailures.Add("endpoint delete: $($_.Exception.Message)")
            }
        }
        if ($networkCreated) {
            try {
                $errorRecord = [IntPtr]::Zero
                $status = [Nvx.Hcn.NativeMethods]::HcnDeleteNetwork(
                    [ref]$networkId,
                    [ref]$errorRecord
                )
                Complete-HcnCall $status $errorRecord "HcnDeleteNetwork($networkId)" `
                    -IgnoredStatus @($script:HcnNetworkNotFound)
            } catch {
                $rollbackFailures.Add("network delete: $($_.Exception.Message)")
            }
        }
        if ($rollbackFailures.Count -ne 0) {
            throw "$($primaryFailure.Exception.Message); rollback failures: $($rollbackFailures -join '; ')"
        }
        throw $primaryFailure
    } finally {
        Close-HcnHandle $endpointHandle Endpoint
        Close-HcnHandle $networkHandle Network
    }
}

function Remove-NvxHcnEndpoint {
    param([Parameter(Mandatory)]$Descriptor)

    $endpointId = [Guid]$Descriptor.endpointId
    $networkId = [Guid]$Descriptor.networkId
    $failures = New-Object Collections.Generic.List[string]
    if ($Descriptor.hostAttached -and $Descriptor.hostNamespaceId) {
        try {
            Set-HcnNamespaceEndpoint `
                ([Guid]$Descriptor.hostNamespaceId) `
                $endpointId `
                Remove `
                -IgnoreMissing
        } catch {
            $failures.Add("namespace detach: $($_.Exception.Message)")
        }
    }

    try {
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnDeleteEndpoint(
            [ref]$endpointId,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnDeleteEndpoint($endpointId)" `
            -IgnoredStatus @($script:HcnEndpointNotFound)
    } catch {
        $failures.Add("endpoint delete: $($_.Exception.Message)")
    }

    try {
        $errorRecord = [IntPtr]::Zero
        $status = [Nvx.Hcn.NativeMethods]::HcnDeleteNetwork(
            [ref]$networkId,
            [ref]$errorRecord
        )
        Complete-HcnCall $status $errorRecord "HcnDeleteNetwork($networkId)" `
            -IgnoredStatus @($script:HcnNetworkNotFound)
    } catch {
        $failures.Add("network delete: $($_.Exception.Message)")
    }
    if ($failures.Count -ne 0) {
        throw "external HCN cleanup failed: $($failures -join '; ')"
    }
}

Export-ModuleMember -Function New-NvxHcnEndpoint, Remove-NvxHcnEndpoint