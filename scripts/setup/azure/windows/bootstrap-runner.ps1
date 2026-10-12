[CmdletBinding()]
param(
    [string]$ConfigPath = 'C:\ProgramData\NvxBootstrap\config.json'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
if ($config.os -ne 'windows') { throw 'Expected a Windows runner configuration.' }
$stateRoot = 'C:\ProgramData\NvxBootstrap'
$setupScript = Join-Path $stateRoot 'setup-windows-whp.ps1'
$diskScript = Join-Path $stateRoot 'setup-data-disk.ps1'
$continuationTask = 'Nvx-Setup-GitHub-Runner'
$featureBootFile = Join-Path $stateRoot 'feature-reboot-pending.txt'
$prerequisiteBootFile = Join-Path $stateRoot 'prerequisite-reboot-pending.txt'

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
    $metadata = Invoke-RestMethod -Uri (
        'http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01'
    ) -Headers @{ Metadata = 'true' } -TimeoutSec 30
    $prefix = $config.vmssName + '_'
    if ($metadata.subscriptionId -ne $config.subscriptionId -or
        $metadata.resourceGroupName -ne $config.resourceGroup -or
        $metadata.vmScaleSetName -ne $config.vmssName -or
        -not ([string]$metadata.name).StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Azure instance metadata does not identify the configured VMSS.'
    }
    $instance = ([string]$metadata.name).Substring($prefix.Length)
    if ($instance -notmatch '^[0-9]+$') { throw 'VMSS instance ID is not numeric.' }
    return '{0}-{1}' -f $config.runnerNamePrefix, ([long]$instance + $config.runnerNumberOffset)
}

function Set-RunnerMetadataFirewall {
    $name = 'NvxRunnerDenyMetadata'
    $principal = 'D:(A;;CC;;;S-1-5-20)'
    $rule = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
    if (-not $rule) {
        $rule = New-NetFirewallRule -Name $name -DisplayName $name `
            -Direction Outbound -Action Block -RemoteAddress '169.254.169.254' `
            -LocalUser $principal -Profile Any
    }
    $addresses = @($rule | Get-NetFirewallAddressFilter | Select-Object -ExpandProperty RemoteAddress)
    $users = ($rule | Get-NetFirewallSecurityFilter).LocalUser
    $canonicalUsers = [Security.AccessControl.RawSecurityDescriptor]::new($users).
        GetSddlForm([Security.AccessControl.AccessControlSections]::All)
    $canonicalPrincipal = [Security.AccessControl.RawSecurityDescriptor]::new($principal).
        GetSddlForm([Security.AccessControl.AccessControlSections]::All)
    if ($rule.Direction -ne 'Outbound' -or $rule.Action -ne 'Block' -or
        $rule.Enabled -ne 'True' -or $addresses.Count -ne 1 -or
        $addresses[0] -ne '169.254.169.254' -or $canonicalUsers -ne $canonicalPrincipal) {
        throw 'The runner account is not blocked from Azure instance metadata.'
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

$runnerName = Get-RunnerName
New-Item -ItemType Directory -Path $stateRoot -Force | Out-Null
Start-Transcript -Path (Join-Path $stateRoot 'setup-runner.log') -Append | Out-Null
$registrationToken = $null
try {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument (
        "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -ConfigPath `"$ConfigPath`""
    )
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -ExecutionTimeLimit (New-TimeSpan -Hours 4) -RestartCount 60 -RestartInterval (New-TimeSpan -Minutes 2)
    Register-ScheduledTask -TaskName $continuationTask -Action $action `
        -Trigger (New-ScheduledTaskTrigger -AtStartup) -Principal $principal -Settings $settings -Force | Out-Null

    $drive = $config.dataDisk.driveLetter
    if ($drive -eq $env:SystemDrive.TrimEnd(':')) { throw 'Data disk must not use the system drive letter.' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $diskScript `
        -DataDiskLun $config.dataDisk.lun -DataDriveLetter $drive `
        -TempDiskFallbackLetter $config.dataDisk.temporaryDriveFallback
    if ($LASTEXITCODE -ne 0) { throw "Managed data disk setup failed: $LASTEXITCODE" }
    $diskReady = Get-ItemProperty -Path 'HKLM:\SOFTWARE\AzTemplates\DataDiskSetup' `
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

    $requiredLabels = @('windows', 'whp', 'virtual-machine')
    $configuredLabels = @($config.runnerLabels)
    if ($configuredLabels.Count -lt $requiredLabels.Count) {
        throw 'Runner labels must preserve the platform and backend labels.'
    }
    for ($index = 0; $index -lt $requiredLabels.Count; $index++) {
        if ($configuredLabels[$index] -cne $requiredLabels[$index]) {
            throw 'Runner labels must preserve the platform and backend labels.'
        }
    }
    $extraLabels = @($configuredLabels | Select-Object -Skip $requiredLabels.Count) -join ','
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
    $result = Invoke-Setup -Arguments ($arguments + @(
        '-RunnerName', $runnerName, '-RunnerTokenStdin'
    )) -OutputPath (Join-Path $stateRoot 'setup-registration.log') -Token $registrationToken
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
finally {
    $registrationToken = $null
    Stop-Transcript | Out-Null
}
