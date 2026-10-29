# Metadata firewall operations of the Windows runner bootstrap.
#
# Inspect reports the firewall services, the effective profiles, and the
# effective metadata rule with every filter that could narrow it; the Python
# controller decides whether that rule blocks the runner account. Create adds
# the rule. The adapter reads a JSON request from standard input, or from
# -Request when another PowerShell script calls it, and writes a JSON result.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Operation,
    [string]$Request
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if (-not $PSBoundParameters.ContainsKey('Request')) {
    $reader = [IO.StreamReader]::new([Console]::OpenStandardInput(), [Text.UTF8Encoding]::new($false))
    $Request = $reader.ReadToEnd()
}
$arguments = if ([string]::IsNullOrWhiteSpace($Request)) { [pscustomobject]@{} } else { $Request | ConvertFrom-Json }

function Write-Result {
    param($Value)
    # Windows PowerShell converts $null to no text at all.
    $json = if ($null -eq $Value) { 'null' } else { ConvertTo-Json -InputObject $Value -Depth 6 -Compress }
    # The host also writes warnings and messages to standard output, so the
    # result has a marked line of its own, in ASCII for any console code page.
    Write-Output ('NVX-ADAPTER-RESULT ' +
        [regex]::Replace($json, '[^\x00-\x7F]', { '\u{0:x4}' -f [int][char]$args[0].Value }))
}

function Get-CanonicalSddl {
    param([string]$Sddl)
    return [Security.AccessControl.RawSecurityDescriptor]::new($Sddl).GetSddlForm(
        [Security.AccessControl.AccessControlSections]::All)
}

function Get-OptionalValue {
    param($Object, [string]$Name)
    $property = $Object.PSObject.Properties[$Name]
    if ($property) { return @($property.Value) -join ',' }
    return ''
}

# Filters come from the rule in ActiveStore: a stored rule alone does not
# show what the firewall enforces.
function ConvertTo-RuleFacts {
    param($Rule)
    $address = $Rule | Get-NetFirewallAddressFilter
    $port = $Rule | Get-NetFirewallPortFilter
    $application = $Rule | Get-NetFirewallApplicationFilter
    $service = $Rule | Get-NetFirewallServiceFilter
    $interface = $Rule | Get-NetFirewallInterfaceFilter
    $interfaceType = $Rule | Get-NetFirewallInterfaceTypeFilter
    $security = $Rule | Get-NetFirewallSecurityFilter
    return [ordered]@{
        direction = [string]$Rule.Direction
        action = [string]$Rule.Action
        enabled = [string]$Rule.Enabled
        profile = [string]$Rule.Profile
        platform = @($Rule.Platform) -join ','
        primaryStatus = [string]$Rule.PrimaryStatus
        # Provider value 1 means enforced; the display strings are localized.
        enforcement = @($Rule.CimInstanceProperties['EnforcementStatus'].Value | ForEach-Object { [int]$_ })
        restrictions = [ordered]@{
            policyAppId = Get-OptionalValue $Rule 'PolicyAppId'
            packageFamilyName = Get-OptionalValue $Rule 'PackageFamilyName'
            remoteDynamicKeywordAddresses = Get-OptionalValue $Rule 'RemoteDynamicKeywordAddresses'
        }
        package = [string]$application.Package
        protocol = [string]$port.Protocol
        authentication = [string]$security.Authentication
        encryption = [string]$security.Encryption
        remoteAddresses = @($address.RemoteAddress | ForEach-Object { [string]$_ })
        filters = [ordered]@{
            localAddress = @($address.LocalAddress) -join ','
            localPort = @($port.LocalPort) -join ','
            remotePort = @($port.RemotePort) -join ','
            icmpType = @($port.IcmpType) -join ','
            dynamicTarget = [string]$port.DynamicTarget
            program = [string]$application.Program
            service = [string]$service.Service
            interfaceAlias = @($interface.InterfaceAlias) -join ','
            interfaceType = [string]$interfaceType.InterfaceType
            remoteUser = [string]$security.RemoteUser
            remoteMachine = [string]$security.RemoteMachine
        }
        localUser = Get-CanonicalSddl $security.LocalUser
    }
}

switch ($Operation) {
    'Inspect' {
        $name = [string]$arguments.name
        $services = [ordered]@{}
        foreach ($serviceName in 'BFE', 'MpsSvc') {
            $services[$serviceName] = [string](Get-Service -Name $serviceName -ErrorAction Stop).Status
        }
        $profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore -ErrorAction Stop | ForEach-Object {
            [ordered]@{
                name = [string]$_.Name
                enabled = [string]$_.Enabled
                allowLocalFirewallRules = [string]$_.AllowLocalFirewallRules
                disabledInterfaceAliases = @($_.DisabledInterfaceAliases | ForEach-Object { [string]$_ })
            }
        })
        $stored = Get-NetFirewallRule -Name $name -PolicyStore PersistentStore -ErrorAction SilentlyContinue
        $effective = @(Get-NetFirewallRule -Name $name -PolicyStore ActiveStore -TracePolicyStore `
            -ErrorAction SilentlyContinue | Where-Object { $_ } | ForEach-Object { ConvertTo-RuleFacts $_ })
        Write-Result ([ordered]@{
            services = $services
            profiles = $profiles
            persistent = $null -ne $stored
            effective = $effective
            principal = Get-CanonicalSddl $arguments.principal
        })
    }
    'Create' {
        New-NetFirewallRule -Name $arguments.name -DisplayName $arguments.name `
            -Direction Outbound -Action Block -RemoteAddress $arguments.address `
            -LocalUser $arguments.principal -Profile Any `
            -Authentication NotRequired -Encryption NotRequired | Out-Null
        Write-Result $true
    }
    default { throw "Unsupported firewall operation: $Operation" }
}
