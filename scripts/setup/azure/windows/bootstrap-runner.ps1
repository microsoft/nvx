[CmdletBinding()]
param(
    [string]$ConfigPath = 'C:\ProgramData\NvxBootstrap\config.json'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$stateRoot = 'C:\ProgramData\NvxBootstrap'
$setupScript = Join-Path $stateRoot 'setup-windows-whp.ps1'
$diskScript = Join-Path $stateRoot 'setup-data-disk.ps1'
$continuationTask = 'Nvx-Setup-GitHub-Runner'
$featureBootFile = Join-Path $stateRoot 'feature-reboot-pending.txt'
$prerequisiteBootFile = Join-Path $stateRoot 'prerequisite-reboot-pending.txt'
$hostMetadataRoot = 'C:\ProgramData\NvxCiHostMetadata'

function Invoke-Setup {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$OutputPath,
        [string]$Token
    )
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = 'powershell.exe'
    $startInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$setupScript`" " + ($Arguments -join ' ')
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) { throw 'Failed to start runner setup.' }
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if ($Token) { $process.StandardInput.WriteLine($Token) }
        $process.StandardInput.Close()
        $process.WaitForExit()
        [IO.File]::WriteAllText($OutputPath, ($stdout.Result + "`n" + $stderr.Result))
        return $process.ExitCode
    }
    finally { $process.Dispose() }
}

function Request-Reboot {
    param([Parameter(Mandatory = $true)][string]$Reason)
    if (-not (Get-ScheduledTask -TaskName $continuationTask -ErrorAction SilentlyContinue)) {
        throw 'Runner continuation task is missing; refusing to reboot.'
    }
    & shutdown.exe /r /t 60 /c $Reason
    if ($LASTEXITCODE -ne 0) { throw "Failed to schedule reboot: $Reason" }
    Write-Output 'NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1'
}

function Get-RunnerName {
    param([Parameter(Mandatory = $true)]$Metadata)
    $prefix = $config.vmssName + '_'
    if ($Metadata.subscriptionId -ne $config.subscriptionId -or
        $Metadata.resourceGroupName -ne $config.resourceGroup -or
        $Metadata.vmScaleSetName -ne $config.vmssName -or
        -not ([string]$Metadata.name).StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Azure instance metadata does not identify the configured VMSS.'
    }
    $instance = ([string]$Metadata.name).Substring($prefix.Length)
    if ($instance -notmatch '^[0-9]+$') { throw 'VMSS instance ID is not numeric.' }
    return '{0}-{1}' -f $config.runnerNamePrefix, ([long]$instance + $config.runnerNumberOffset)
}

function Get-InstanceRunnerLabels {
    param(
        [Parameter(Mandatory = $true)]$Config,
        [Parameter(Mandatory = $true)][string]$RunnerName
    )
    $labels = @($Config.runnerLabels)
    if ($Config.PSObject.Properties['benchmarkRunnerName'] -and
        $Config.benchmarkRunnerName -ceq $RunnerName) {
        $labels = @($labels | Where-Object { $_ -ne 'general' }) + @('benchmark')
    }
    return $labels
}

function Select-SafeAzureComputeDisk {
    param([Parameter(Mandatory = $true)]$Disk, [switch]$DataDisk)
    $size = [string]$Disk.diskSizeGB
    $accountType = [string]$Disk.managedDisk.storageAccountType
    $caching = [string]$Disk.caching
    $writeAccelerator = [string]$Disk.writeAcceleratorEnabled
    if ($size -notmatch '^[0-9]{1,10}$' -or
        $accountType -notmatch '^[A-Za-z0-9_]{1,64}$' -or
        $caching -notin @('None', 'ReadOnly', 'ReadWrite') -or
        $writeAccelerator -notmatch '^(?i:true|false)$') {
        throw 'Azure disk metadata contains an invalid performance setting.'
    }
    $safe = [ordered]@{
        diskSizeGB = $size
        managedDisk = [ordered]@{ storageAccountType = $accountType }
        caching = $caching
        writeAcceleratorEnabled = $writeAccelerator
    }
    if ($DataDisk) {
        $lun = [string]$Disk.lun
        if ($lun -notmatch '^[0-9]{1,2}$' -or [int]$lun -gt 63) {
            throw 'Azure data disk metadata contains an invalid LUN.'
        }
        $safe.lun = $lun
        foreach ($field in @('bytesPerSecondThrottle', 'opsPerSecondThrottle')) {
            $property = $Disk.PSObject.Properties[$field]
            $value = if ($property) { [string]$property.Value } else { '' }
            if ($value -notmatch '^[0-9]{0,20}$') {
                throw 'Azure disk metadata contains an invalid performance limit.'
            }
            $safe[$field] = $value
        }
    }
    else {
        $property = $Disk.PSObject.Properties['diffDiskSettings']
        $option = if ($property -and $property.Value) {
            [string]$property.Value.option
        } else { '' }
        if ($option -notmatch '^[A-Za-z]{0,32}$') {
            throw 'Azure OS disk metadata contains an invalid ephemeral option.'
        }
        $safe.diffDiskSettings = [ordered]@{ option = $option }
    }
    return $safe
}

function ConvertTo-SafeAzureComputeMetadata {
    param([Parameter(Mandatory = $true)]$Metadata)
    $size = [string]$Metadata.vmSize
    if ($size -notmatch '^Standard_[A-Za-z0-9_]{1,80}$') {
        throw 'Azure compute metadata contains an invalid VM size.'
    }
    $disks = @(
        foreach ($disk in $Metadata.storageProfile.dataDisks) {
            Select-SafeAzureComputeDisk -Disk $disk -DataDisk
        }
    )
    if ($disks.Count -eq 0) { throw 'Azure compute metadata has no managed data disks.' }
    $resourceDisk = $Metadata.storageProfile.PSObject.Properties['resourceDisk']
    $resourceSize = if ($resourceDisk -and $resourceDisk.Value) {
        [string]$resourceDisk.Value.size
    } else { '' }
    if ($resourceSize -notmatch '^[0-9]{0,16}$') {
        throw 'Azure resource disk metadata contains an invalid size.'
    }
    return [ordered]@{
        vmSize = $size
        storageProfile = [ordered]@{
            osDisk = Select-SafeAzureComputeDisk -Disk $Metadata.storageProfile.osDisk
            dataDisks = $disks
            resourceDisk = [ordered]@{ size = $resourceSize }
        }
    }
}

function Write-AzureComputeSnapshot {
    param([Parameter(Mandatory = $true)]$Metadata)
    $safe = ConvertTo-SafeAzureComputeMetadata -Metadata $Metadata
    if (Test-Path -LiteralPath $hostMetadataRoot) {
        $item = Get-Item -LiteralPath $hostMetadataRoot -Force
        if (-not $item.PSIsContainer -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Host metadata directory is not a regular directory.'
        }
    }
    else { New-Item -ItemType Directory -Path $hostMetadataRoot | Out-Null }
    $administrators = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    $system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
    $runner = [Security.Principal.SecurityIdentifier]::new('S-1-5-20')
    $directoryAcl = [Security.AccessControl.DirectorySecurity]::new()
    $directoryAcl.SetAccessRuleProtection($true, $false)
    $directoryAcl.SetOwner($administrators)
    foreach ($principal in @($system, $administrators)) {
        $directoryAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $principal, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    $directoryAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $runner, 'ReadAndExecute', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    Set-Acl -LiteralPath $hostMetadataRoot -AclObject $directoryAcl

    $path = Join-Path $hostMetadataRoot 'compute.json'
    if (Test-Path -LiteralPath $path) {
        $item = Get-Item -LiteralPath $path -Force
        if ($item.PSIsContainer -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Host metadata snapshot is not a regular file.'
        }
    }
    $fileAcl = [Security.AccessControl.FileSecurity]::new()
    $fileAcl.SetAccessRuleProtection($true, $false)
    $fileAcl.SetOwner($administrators)
    foreach ($principal in @($system, $administrators)) {
        $fileAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $principal, 'FullControl', 'None', 'None', 'Allow'))
    }
    $fileAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $runner, 'Read', 'None', 'None', 'Allow'))
    if (Test-Path -LiteralPath $path) {
        Set-Acl -LiteralPath $path -AclObject $fileAcl
    }
    $temporary = Join-Path $hostMetadataRoot ([guid]::NewGuid().ToString('N') + '.tmp')
    $backup = Join-Path $hostMetadataRoot ([guid]::NewGuid().ToString('N') + '.bak')
    try {
        [IO.File]::WriteAllText($temporary, ($safe | ConvertTo-Json -Depth 8 -Compress),
            [Text.UTF8Encoding]::new($false))
        Set-Acl -LiteralPath $temporary -AclObject $fileAcl
        if (Test-Path -LiteralPath $path) {
            # Windows PowerShell requires a backup path for File.Replace.
            [IO.File]::Replace($temporary, $path, $backup)
        }
        else { [IO.File]::Move($temporary, $path) }
        Set-Acl -LiteralPath $path -AclObject $fileAcl
    }
    finally {
        if (Test-Path -LiteralPath $backup) {
            Remove-Item -LiteralPath $backup -Force
        }
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force
        }
    }
}

function Get-ExtraRunnerLabels {
    param([string[]]$RunnerLabels)
    $requiredLabels = @('windows', 'whp', 'virtual-machine')
    if ($RunnerLabels.Count -lt $requiredLabels.Count) {
        throw 'Runner labels must preserve the platform and backend labels.'
    }
    for ($index = 0; $index -lt $requiredLabels.Count; $index++) {
        if ($RunnerLabels[$index] -ne $requiredLabels[$index]) {
            throw 'Runner labels must preserve the platform and backend labels.'
        }
    }
    [void](Test-BenchmarkRunner -RunnerLabels $RunnerLabels)
    return @(
        $RunnerLabels |
            Select-Object -Skip $requiredLabels.Count |
            Where-Object { $_ -notin @('general', 'benchmark') }
    ) -join ','
}

function Test-BenchmarkRunner {
    param([string[]]$RunnerLabels)
    $roleLabels = @(
        $RunnerLabels | Where-Object { $_ -in @('general', 'benchmark') }
    )
    if ($roleLabels -contains 'general' -and $roleLabels -contains 'benchmark') {
        throw 'Runner labels must not contain both general and benchmark.'
    }
    return $roleLabels -contains 'benchmark'
}

function Set-RunnerMetadataFirewall {
    $name = 'NvxRunnerDenyMetadata'
    $principal = 'D:(A;;CC;;;S-1-5-20)'
    $failure = 'The runner account is not blocked from Azure instance metadata.'
    foreach ($serviceName in @('BFE', 'MpsSvc')) {
        if ((Get-Service -Name $serviceName -ErrorAction Stop).Status -ne 'Running') {
            throw "$failure Windows Firewall service $serviceName is not running."
        }
    }
    $profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore -ErrorAction Stop)
    if ($profiles.Count -ne 3 -or
        @(Compare-Object @('Domain', 'Private', 'Public') @($profiles.Name)).Count -ne 0) {
        throw "$failure Effective firewall profiles are unavailable."
    }
    foreach ($profile in $profiles) {
        $disabledInterfaces = @($profile.DisabledInterfaceAliases | Where-Object {
            $_ -and $_ -ne 'NotConfigured'
        })
        if ($profile.Enabled -ne 'True' -or
            [string]$profile.AllowLocalFirewallRules -notin @('True', 'NotConfigured') -or
            $disabledInterfaces.Count -ne 0) {
            throw "$failure Profile $($profile.Name) does not enforce local rules on all interfaces."
        }
    }
    $rule = Get-NetFirewallRule -Name $name -PolicyStore PersistentStore `
        -ErrorAction SilentlyContinue
    if (-not $rule) {
        $rule = New-NetFirewallRule -Name $name -DisplayName $name `
            -Direction Outbound -Action Block -RemoteAddress '169.254.169.254' `
            -LocalUser $principal -Profile Any `
            -Authentication NotRequired -Encryption NotRequired
    }
    $effectiveRules = @(Get-NetFirewallRule -Name $name -PolicyStore ActiveStore `
        -TracePolicyStore -ErrorAction Stop)
    if ($effectiveRules.Count -ne 1 -or -not $effectiveRules[0]) {
        throw "$failure No unique effective metadata rule is available."
    }
    $rule = $effectiveRules[0]
    # Inactive profiles (5) are harmless when an active profile is enforced (1).
    # Use provider values because the display strings are localized.
    $enforcement = @($rule.CimInstanceProperties['EnforcementStatus'].Value)
    if ($rule.PrimaryStatus -ne 'OK' -or $enforcement -notcontains 1 -or
        @($enforcement | Where-Object { $_ -notin @(1, 5) }).Count -ne 0) {
        throw "$failure The effective metadata rule is not fully enforced."
    }
    $addressFilter = $rule | Get-NetFirewallAddressFilter
    $portFilter = $rule | Get-NetFirewallPortFilter
    $applicationFilter = $rule | Get-NetFirewallApplicationFilter
    $serviceFilter = $rule | Get-NetFirewallServiceFilter
    $interfaceFilter = $rule | Get-NetFirewallInterfaceFilter
    $interfaceTypeFilter = $rule | Get-NetFirewallInterfaceTypeFilter
    $securityFilter = $rule | Get-NetFirewallSecurityFilter
    $addresses = @($addressFilter.RemoteAddress)
    $unrestricted = @(
        ($addressFilter.LocalAddress -join ',')
        ($portFilter.LocalPort -join ',')
        ($portFilter.RemotePort -join ',')
        ($portFilter.IcmpType -join ',')
        ([string]$portFilter.DynamicTarget)
        ([string]$applicationFilter.Program)
        ([string]$serviceFilter.Service)
        ($interfaceFilter.InterfaceAlias -join ',')
        ([string]$interfaceTypeFilter.InterfaceType)
        ([string]$securityFilter.RemoteUser)
        ([string]$securityFilter.RemoteMachine)
    )
    $restrictedRule = $false
    foreach ($field in @('PolicyAppId', 'PackageFamilyName', 'RemoteDynamicKeywordAddresses')) {
        $property = $rule.PSObject.Properties[$field]
        if ($property -and -not [string]::IsNullOrEmpty($property.Value -join ',')) {
            $restrictedRule = $true
            break
        }
    }
    $canonicalUsers = [Security.AccessControl.RawSecurityDescriptor]::new($securityFilter.LocalUser).
        GetSddlForm([Security.AccessControl.AccessControlSections]::All)
    $canonicalPrincipal = [Security.AccessControl.RawSecurityDescriptor]::new($principal).
        GetSddlForm([Security.AccessControl.AccessControlSections]::All)
    if ($rule.Direction -ne 'Outbound' -or $rule.Action -ne 'Block' -or
        $rule.Enabled -ne 'True' -or $rule.Profile -ne 'Any' -or
        ($rule.Platform -join ',') -ne '' -or
        $restrictedRule -or [string]$applicationFilter.Package -notin @('', 'Any') -or
        [string]$portFilter.Protocol -notin @('Any', '256') -or
        $securityFilter.Authentication -ne 'NotRequired' -or
        $securityFilter.Encryption -ne 'NotRequired' -or
        @($unrestricted | Where-Object { $_ -ne 'Any' }).Count -ne 0 -or
        $addresses.Count -ne 1 -or $addresses[0] -ne '169.254.169.254' -or
        $canonicalUsers -ne $canonicalPrincipal) {
        throw $failure
    }
}

function Get-RegistrationToken {
    $vaultAccessToken = $null
    $githubToken = $null
    try {
        $identityUri = 'http://169.254.169.254/metadata/identity/oauth2/token' +
            '?api-version=2019-08-01&resource=https%3A%2F%2Fvault.azure.net'
        $identity = Invoke-RestMethod -Uri $identityUri -Headers @{ Metadata = 'true' } -TimeoutSec 30
        $vaultAccessToken = $identity.access_token
        if (-not $vaultAccessToken) { throw 'Managed identity returned no Key Vault access token.' }
        $secretUri = "https://$($config.keyVaultName).vault.azure.net/secrets/" +
            "$($config.bootstrapSecretName)?api-version=7.4"
        $secret = Invoke-RestMethod -Uri $secretUri -TimeoutSec 30 `
            -Headers @{ Authorization = "Bearer $vaultAccessToken" }
        $githubToken = $secret.value
        if (-not $githubToken) { throw 'Key Vault returned no runner bootstrap credential.' }
        $registration = Invoke-RestMethod -Method Post -TimeoutSec 30 `
            -Uri "https://api.github.com/repos/$($config.repository)/actions/runners/registration-token" `
            -Headers @{
                Authorization = "Bearer $githubToken"
                Accept = 'application/vnd.github+json'
                'X-GitHub-Api-Version' = '2022-11-28'
            }
        if (-not $registration.token) { throw 'GitHub returned no runner registration token.' }
        return $registration.token
    }
    finally {
        $vaultAccessToken = $null
        $githubToken = $null
    }
}

New-Item -ItemType Directory -Path $stateRoot -Force | Out-Null
Start-Transcript -Path (Join-Path $stateRoot 'setup-runner.log') -Append | Out-Null
$registrationToken = $null
try {
    $config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
    if ($config.os -ne 'windows') { throw 'Expected a Windows runner configuration.' }
    if (-not (Get-ScheduledTask -TaskName $continuationTask -ErrorAction SilentlyContinue)) {
        throw 'Runner continuation task is missing; stage the package before starting bootstrap.'
    }
    Set-RunnerMetadataFirewall
    $metadata = Invoke-RestMethod -Uri (
        'http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01'
    ) -Headers @{ Metadata = 'true' } -TimeoutSec 30
    $runnerName = Get-RunnerName -Metadata $metadata
    Write-AzureComputeSnapshot -Metadata $metadata
    $drive = $config.dataDisk.driveLetter
    if ($drive -eq $env:SystemDrive.TrimEnd(':')) { throw 'Data disk must not use the system drive letter.' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $diskScript `
        -DataDiskLun $config.dataDisk.lun -DataDriveLetter $drive `
        -TempDiskFallbackLetter $config.dataDisk.temporaryDriveFallback
    if ($LASTEXITCODE -ne 0) { throw "Managed data disk setup failed: $LASTEXITCODE" }
    $diskReady = Get-ItemProperty -Path 'HKLM:\SOFTWARE\NvxRunner\DataDiskSetup' `
        -Name Completed -ErrorAction SilentlyContinue
    if (-not $diskReady -or $diskReady.Completed -ne 1) {
        Write-Output 'Data disk setup scheduled a reboot; runner setup continues at startup.'
        return
    }
    $volume = Get-Volume -DriveLetter $drive -ErrorAction SilentlyContinue
    if (-not $volume -or $volume.FileSystem -ne 'NTFS') { throw 'Managed NTFS data volume is unavailable.' }

    $bootTime = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
    $pending = @('HypervisorPlatform', 'Microsoft-Hyper-V' | ForEach-Object {
        $feature = Get-WindowsOptionalFeature -Online -FeatureName $_
        if ($feature.State -ne 'Enabled') { $feature }
    })
    if ($pending.Count -gt 0) {
        if ((Test-Path -LiteralPath $featureBootFile) -and
            (Get-Content -LiteralPath $featureBootFile -Raw).Trim() -ne $bootTime) {
            throw 'WHP/Hyper-V features are still disabled after their reboot.'
        }
        [IO.File]::WriteAllText($featureBootFile, $bootTime)
        foreach ($feature in $pending) {
            if ($feature.State -ne 'EnablePending') {
                Enable-WindowsOptionalFeature -Online -FeatureName $feature.FeatureName -All -NoRestart | Out-Null
            }
        }
        Request-Reboot -Reason 'Enabling WHP and Hyper-V for the CI runner.'
        return
    }
    if (Test-Path -LiteralPath $featureBootFile) {
        if ((Get-Content -LiteralPath $featureBootFile -Raw).Trim() -eq $bootTime) {
            Request-Reboot -Reason 'Completing Windows virtualization feature enablement.'
            return
        }
        Remove-Item -LiteralPath $featureBootFile -Force
    }
    Set-RunnerMetadataFirewall
    $appxPaths = @(Get-AppxPackage -AllUsers | Where-Object {
        $_.Name -like 'Microsoft.VCLibs*' -or $_.Name -like 'Microsoft.UI.Xaml*'
    } | Sort-Object Version -Descending | Select-Object -ExpandProperty InstallLocation -Unique)
    $remaining = @([Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';' | Where-Object {
        $_ -and $_ -notmatch '\\WindowsApps\\Microsoft\.VCLibs\.' -and
        $_ -notmatch '\\WindowsApps\\Microsoft\.UI\.Xaml\.'
    })
    $machinePath = (@($appxPaths) + $remaining) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $machinePath, 'Machine')
    $env:Path = $machinePath

    $runnerLabels = @(Get-InstanceRunnerLabels -Config $config -RunnerName $runnerName)
    $extraLabels = Get-ExtraRunnerLabels -RunnerLabels $runnerLabels
    $benchmarkRunner = Test-BenchmarkRunner -RunnerLabels $runnerLabels
    $arguments = @(
        '-RunnerOnly', '-RepositoryUrl', "https://github.com/$($config.repository)",
        '-RunnerDirectory', $config.runnerDirectory
    )
    if ($extraLabels) {
        $arguments += @('-ExtraRunnerLabels', $extraLabels)
    }
    if ($config.benchmarkScratchDirectory) {
        $arguments += @('-BenchmarkScratchDirectory', $config.benchmarkScratchDirectory)
    }
    $result = Invoke-Setup -Arguments $arguments -OutputPath (Join-Path $stateRoot 'setup-prewarm.log')
    if ($result -eq 3010) {
        if ((Test-Path -LiteralPath $prerequisiteBootFile) -and
            (Get-Content -LiteralPath $prerequisiteBootFile -Raw).Trim() -ne $bootTime) {
            throw 'Runner prerequisites still require a reboot after their previous reboot.'
        }
        [IO.File]::WriteAllText($prerequisiteBootFile, $bootTime)
        Request-Reboot -Reason 'Completing CI runner prerequisites.'
        return
    }
    if ($result -ne 0) { throw "Runner prerequisites failed: $result; see setup-prewarm.log." }
    if (Test-Path -LiteralPath $prerequisiteBootFile) { Remove-Item -LiteralPath $prerequisiteBootFile -Force }
    $registrationToken = Get-RegistrationToken
    $registrationArguments = $arguments + @(
        '-RunnerName', $runnerName, '-RunnerTokenStdin'
    )
    if ($benchmarkRunner) {
        $registrationArguments += '-BenchmarkRunner'
    }
    $result = Invoke-Setup -Arguments $registrationArguments `
        -OutputPath (Join-Path $stateRoot 'setup-registration.log') -Token $registrationToken
    $registrationToken = $null
    if ($result -eq 3010) {
        [IO.File]::WriteAllText($prerequisiteBootFile, $bootTime)
        Request-Reboot -Reason 'Completing CI runner registration prerequisites.'
        return
    }
    if ($result -ne 0) { throw "Runner registration failed: $result; see setup-registration.log." }
    Unregister-ScheduledTask -TaskName $continuationTask -Confirm:$false -ErrorAction Stop
    Write-Output 'NVX_CLOUD_RUNNER_SETUP_COMPLETE=1'
}
catch {
    Write-Output 'NVX_CLOUD_RUNNER_SETUP_FAILED=1'
    Write-Error -ErrorRecord $_ -ErrorAction Continue
    throw
}
finally {
    $registrationToken = $null
    Stop-Transcript | Out-Null
}
