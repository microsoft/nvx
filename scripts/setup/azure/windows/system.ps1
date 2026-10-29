# System operations of the Windows runner bootstrap.
#
# The Python controller decides what to change; this adapter only runs the
# ScheduledTasks, CIM, DISM, Appx, and ACL cmdlets that it names. It reads a
# JSON request from standard input, or from -Request when another PowerShell
# script calls it, and writes a JSON result.
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

# Owned by Administrators and writable only by SYSTEM and Administrators;
# -ReaderSid principals may read. Directory rules apply to their contents.
function New-ProtectedSecurity {
    param([switch]$Directory, [string[]]$ReaderSid = @(), [string]$ReaderRights = 'Read')
    $security = if ($Directory) { [Security.AccessControl.DirectorySecurity]::new() } else { [Security.AccessControl.FileSecurity]::new() }
    $inheritance = if ($Directory) { 'ContainerInherit,ObjectInherit' } else { 'None' }
    $security.SetAccessRuleProtection($true, $false)
    $security.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach ($sid in 'S-1-5-18', 'S-1-5-32-544') {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', $inheritance, 'None', 'Allow'))
    }
    foreach ($sid in $ReaderSid) {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), $ReaderRights, $inheritance, 'None', 'Allow'))
    }
    return $security
}

function Assert-RegularItem {
    param([string]$Path)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Refusing to protect a reparse point: $Path"
    }
    return $item
}

switch ($Operation) {
    'TaskExists' {
        Write-Result ($null -ne (Get-ScheduledTask -TaskName $arguments.name -ErrorAction SilentlyContinue))
    }
    'RegisterTask' {
        $action = New-ScheduledTaskAction -Execute $arguments.execute -Argument $arguments.arguments `
            -WorkingDirectory $arguments.workingDirectory
        $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew `
            -ExecutionTimeLimit (New-TimeSpan -Hours $arguments.executionHours) `
            -RestartCount $arguments.restartCount `
            -RestartInterval (New-TimeSpan -Minutes $arguments.restartMinutes)
        Register-ScheduledTask -TaskName $arguments.name -Action $action -Trigger (New-ScheduledTaskTrigger -AtStartup) `
            -Principal $principal -Settings $settings -Force | Out-Null
        Write-Result $true
    }
    'StartTask' {
        Start-ScheduledTask -TaskName $arguments.name
        Write-Result $true
    }
    'UnregisterTask' {
        Unregister-ScheduledTask -TaskName $arguments.name -Confirm:$false
        Write-Result $true
    }
    'BootTime' {
        Write-Result (Get-CimInstance -ClassName Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
    }
    'Features' {
        Write-Result @($arguments.names | ForEach-Object {
            [ordered]@{ name = [string]$_; state = [string](Get-WindowsOptionalFeature -Online -FeatureName $_).State }
        })
    }
    'EnableFeatures' {
        foreach ($name in @($arguments.names)) {
            Enable-WindowsOptionalFeature -Online -FeatureName $name -All -NoRestart | Out-Null
        }
        Write-Result $true
    }
    'AppxPaths' {
        # SYSTEM has no registration of these winget dependencies, so the
        # machine path must name their install locations.
        Write-Result @(Get-AppxPackage -AllUsers | Where-Object {
            $_.Name -like 'Microsoft.VCLibs*' -or $_.Name -like 'Microsoft.UI.Xaml*'
        } | Sort-Object Version -Descending | Select-Object -ExpandProperty InstallLocation -Unique)
    }
    'ProtectHostMetadata' {
        # Network Service, which runs jobs, may read but not modify the snapshot.
        foreach ($path in @($arguments.paths)) {
            $item = Assert-RegularItem $path
            $security = New-ProtectedSecurity -Directory:$item.PSIsContainer -ReaderSid 'S-1-5-20' `
                -ReaderRights $(if ($item.PSIsContainer) { 'ReadAndExecute' } else { 'Read' })
            Set-Acl -LiteralPath $path -AclObject $security
        }
        Write-Result $true
    }
    'ProtectAuthorizedKeys' {
        # sshd ignores administrators_authorized_keys unless only SYSTEM and
        # Administrators can write it. Rebuild the ACL rather than amend it, so
        # no other explicit rule survives, and verify the result.
        $path = $arguments.path
        $null = Assert-RegularItem $path
        Set-Acl -LiteralPath $path -AclObject (New-ProtectedSecurity)
        $trusted = @('S-1-5-18', 'S-1-5-32-544')
        $other = @((Get-Acl -LiteralPath $path).Access | Where-Object {
            $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -notin $trusted
        })
        if ($other.Count -ne 0) {
            throw "Unexpected ACEs remain on ${path}: $(($other | ForEach-Object { $_.IdentityReference }) -join ', ')"
        }
        Write-Result $true
    }
    default { throw "Unsupported system operation: $Operation" }
}
