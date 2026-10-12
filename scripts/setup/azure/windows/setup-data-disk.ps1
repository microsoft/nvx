# Idempotent setup of the single Azure managed data disk at LUN 0.
# Existing filesystems are never formatted. If the Azure temporary disk owns
# the requested letter and hosts the pagefile, setup continues through a scheduled startup task
# after one controlled reboot.
param(
    [int] $DataDiskLun = 0,
    [string] $DataDriveLetter = 'F',
    [string] $TempDiskFallbackLetter = 'T'
)

$ErrorActionPreference = 'Stop'
$DataDriveLetter = $DataDriveLetter.TrimEnd(':').ToUpperInvariant()
$TempDiskFallbackLetter = $TempDiskFallbackLetter.TrimEnd(':').ToUpperInvariant()
if ($DataDriveLetter.Length -ne 1 -or $TempDiskFallbackLetter.Length -ne 1) {
    throw 'Drive-letter parameters must each contain one letter.'
}

$logDir = 'C:\WindowsAzure\Logs'
$stateDir = Join-Path $env:ProgramFiles 'AzTemplates'
$continuationScript = Join-Path $stateDir 'setup-datadisk.ps1'
$continuationTask = 'AzTemplates-SetupDataDisk'
$pendingPath = 'HKLM:\SOFTWARE\AzTemplates\DataDiskSetup'
$initMarkerType = '{8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91}'
$basicDataType = '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'

New-Item -ItemType Directory -Path $logDir -Force | Out-Null
New-Item -ItemType Directory -Path $stateDir -Force | Out-Null
$logFile = Join-Path $logDir 'setup-datadisk.log'
$mutex = [System.Threading.Mutex]::new($false, 'Global\AzTemplates-SetupDataDisk')
$mutexHeld = $false
try {
    $mutexHeld = $mutex.WaitOne((New-TimeSpan -Minutes 15))
}
catch [System.Threading.AbandonedMutexException] {
    $mutexHeld = $true
}
if (-not $mutexHeld) {
    Write-Output 'ERROR: timed out waiting for another data-disk setup instance.'
    exit 1
}

function Write-DiskLog {
    param([string] $Message)
    $line = "[{0}] {1}" -f (Get-Date -Format 'o'), $Message
    Add-Content -Path $logFile -Value $line
    Write-Output $line
}

trap {
    Write-DiskLog "ERROR: $($_.Exception.Message)"
    if ($mutexHeld) {
        $mutex.ReleaseMutex()
        $script:mutexHeld = $false
    }
    exit 1
}

function Ensure-RegistryKey {
    param([string] $Path)
    if (-not (Test-Path $Path)) {
        New-Item -Path $Path -Force | Out-Null
    }
}

function Get-RegistryValue {
    param([string] $Path, [string] $Name)
    return (Get-ItemProperty -Path $Path -Name $Name -ErrorAction SilentlyContinue).$Name
}

function Get-DiskAddress {
    param([Parameter(Mandatory = $true)] $Disk)
    $adapter = $null
    $target = $null
    $lun = $null
    if ($Disk.Location -match 'Adapter\s*(\d+).*Target\s*(\d+)\s*:\s*LUN\s*(\d+)') {
        $adapter = [int]$Matches[1]
        $target = [int]$Matches[2]
        $lun = [int]$Matches[3]
    }
    [PSCustomObject]@{ Adapter = $adapter; Target = $target; Lun = $lun }
}

function Test-LocalNvmeDisk {
    param([Parameter(Mandatory = $true)] $Disk)
    "$($Disk.FriendlyName) $($Disk.Model)" -match '(?i)Microsoft NVMe Direct Disk'
}

function Test-ManagedNvmeDisk {
    param([Parameter(Mandatory = $true)] $Disk)
    "$($Disk.FriendlyName) $($Disk.Model)" -match '(?i)MSFT NVMe Accelerator'
}

function Get-FreeDriveLetter {
    param([string] $Preferred)
    $used = @(Get-Volume -ErrorAction SilentlyContinue |
        Where-Object DriveLetter |
        ForEach-Object { $_.DriveLetter.ToString().ToUpperInvariant() })
    foreach ($letter in @($Preferred, 'T', 'U', 'V', 'W', 'X', 'Y', 'Z')) {
        if ($letter -ne $DataDriveLetter -and $used -notcontains $letter) {
            return $letter
        }
    }
    throw 'No free fallback drive letter is available for the Azure temporary disk.'
}

function Test-PagefileOnLetter {
    param([string] $Letter)
    $pagingFiles = @(Get-RegistryValue `
        -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management' `
        -Name PagingFiles)
    return @($pagingFiles | Where-Object { $_ -match "^${Letter}:\\" }).Count -gt 0
}

function Test-ActivePagefileOnLetter {
    param([string] $Letter)
    return @(Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "${Letter}:\*" }).Count -gt 0
}

function Register-Continuation {
    if ($PSCommandPath -ne $continuationScript) {
        Copy-Item -LiteralPath $PSCommandPath -Destination $continuationScript -Force
    }
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$continuationScript`" -DataDiskLun $DataDiskLun -DataDriveLetter $DataDriveLetter -TempDiskFallbackLetter $TempDiskFallbackLetter"
    $action = New-ScheduledTaskAction -Execute 'PowerShell.exe' -Argument $arguments
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -ExecutionTimeLimit (New-TimeSpan -Minutes 15) `
        -RestartCount 10 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName $continuationTask -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Force | Out-Null
}

function Test-DiskPrefixIsZero {
    param([Parameter(Mandatory = $true)] $Disk)
    $path = "\\.\PhysicalDrive$($Disk.Number)"
    $stream = $null
    try {
        $stream = [System.IO.File]::Open(
            $path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite
        )
        $buffer = New-Object byte[] (1MB)
        $totalRead = 0
        while ($totalRead -lt $buffer.Length) {
            $read = $stream.Read($buffer, $totalRead, $buffer.Length - $totalRead)
            if ($read -le 0) { return $false }
            $totalRead += $read
        }
        for ($index = 0; $index -lt $buffer.Length; $index++) {
            if ($buffer[$index] -ne 0) { return $false }
        }
        return $true
    }
    finally {
        if ($stream) { $stream.Dispose() }
    }
}

Write-DiskLog "=== setup-datadisk starting (lun=$DataDiskLun, letter=$DataDriveLetter) ==="

$dataDisk = $null
$allDisks = @()
for ($attempt = 1; $attempt -le 12; $attempt++) {
    Update-HostStorageCache
    $allDisks = @(Get-Disk)
    $nonBootDisks = @($allDisks | Where-Object { -not $_.IsBoot -and -not $_.IsSystem })
    $scsiCandidates = @($nonBootDisks | Where-Object {
        $address = Get-DiskAddress $_
        $null -ne $address.Adapter -and $address.Adapter -ge 1 -and
        $address.Target -eq 0 -and $address.Lun -eq $DataDiskLun
    })
    $nvmeCandidates = @($nonBootDisks | Where-Object { Test-ManagedNvmeDisk $_ })
    if ($scsiCandidates.Count -eq 1) {
        $dataDisk = $scsiCandidates[0]
        break
    }
    if ($scsiCandidates.Count -eq 0 -and $nvmeCandidates.Count -eq 1) {
        $dataDisk = $nvmeCandidates[0]
        break
    }
    Start-Sleep -Seconds 5
}

foreach ($disk in $allDisks) {
    $address = Get-DiskAddress $disk
    Write-DiskLog "Disk $($disk.Number): Boot=$($disk.IsBoot) System=$($disk.IsSystem) Style=$($disk.PartitionStyle) Bus=$($disk.BusType) FriendlyName='$($disk.FriendlyName)' Model='$($disk.Model)' Location='$($disk.Location)' Adapter=$($address.Adapter) Target=$($address.Target) Lun=$($address.Lun)"
}
if (-not $dataDisk) {
    Write-DiskLog "ERROR: could not uniquely identify managed data disk LUN $DataDiskLun."
    exit 1
}
Write-DiskLog "Selected managed data disk $($dataDisk.Number)."

$dConflict = Get-Partition -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue
if ($dConflict -and $dConflict.DiskNumber -ne $dataDisk.Number) {
    $conflictDisk = Get-Disk -Number $dConflict.DiskNumber
    $address = Get-DiskAddress $conflictDisk
    $isTempDisk = (Test-LocalNvmeDisk $conflictDisk) -or
        ($address.Adapter -eq 0 -and $address.Target -eq 0 -and $address.Lun -eq 1) -or
        (@(Get-Volume -Partition $dConflict -ErrorAction SilentlyContinue |
            Where-Object FileSystemLabel -eq 'Temporary Storage').Count -gt 0)
    if (-not $isTempDisk) {
        Write-DiskLog "ERROR: ${DataDriveLetter}: belongs to disk $($conflictDisk.Number), which is not positively identified as Azure temporary storage."
        exit 1
    }

    $fallback = Get-RegistryValue -Path $pendingPath -Name TempDriveLetter
    if ([string]::IsNullOrWhiteSpace($fallback)) {
        $fallback = Get-FreeDriveLetter $TempDiskFallbackLetter
    }

    $activePagefile = Test-ActivePagefileOnLetter $DataDriveLetter
    $configuredPagefile = Test-PagefileOnLetter $DataDriveLetter
    if ($activePagefile -or $configuredPagefile) {
        Write-DiskLog "Azure temporary disk at ${DataDriveLetter}: hosts the active pagefile. Moving the pagefile to C: and scheduling post-reboot continuation."
        $memoryManagement = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management'
        Ensure-RegistryKey $pendingPath
        $currentBoot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
        $requestedBoot = Get-RegistryValue -Path $pendingPath -Name RebootRequestedFromBoot
        if ($requestedBoot -and $requestedBoot -ne $currentBoot) {
            Write-DiskLog 'ERROR: the D: pagefile conflict persists after the continuation reboot.'
            exit 1
        }
        Set-ItemProperty -Path $pendingPath -Name TempDiskNumber -Value $conflictDisk.Number
        Set-ItemProperty -Path $pendingPath -Name TempDriveLetter -Value $fallback
        Set-ItemProperty -Path $pendingPath -Name RebootRequestedFromBoot -Value $currentBoot
        Register-Continuation
        Set-ItemProperty -Path $memoryManagement -Name PagingFiles -Value @('C:\pagefile.sys 0 0')
        shutdown.exe /r /t 60 /c 'Completing persistent Azure data-disk setup after moving the pagefile.'
        if ($LASTEXITCODE -ne 0) {
            Write-DiskLog 'ERROR: failed to schedule the required reboot.'
            exit 1
        }
        Write-DiskLog 'Reboot scheduled; setup will continue automatically at startup.'
        exit 0
    }

    Write-DiskLog "Moving Azure temporary disk from ${DataDriveLetter}: to ${fallback}:."
    Set-Partition -DiskNumber $conflictDisk.Number -PartitionNumber $dConflict.PartitionNumber -NewDriveLetter $fallback
    for ($attempt = 1; $attempt -le 20; $attempt++) {
        if (-not (Get-Partition -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 500
    }
    if (Get-Partition -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue) {
        Write-DiskLog "ERROR: ${DataDriveLetter}: was not released by the temporary disk."
        exit 1
    }
    $movedTemp = Get-Partition -DriveLetter $fallback -ErrorAction SilentlyContinue
    if (-not $movedTemp -or $movedTemp.DiskNumber -ne $conflictDisk.Number) {
        Write-DiskLog "ERROR: temporary disk was not verified at ${fallback}:."
        exit 1
    }
}

if ($dataDisk.IsOffline) { Set-Disk -Number $dataDisk.Number -IsOffline $false }
if ($dataDisk.IsReadOnly) { Set-Disk -Number $dataDisk.Number -IsReadOnly $false }
$dataDisk = Get-Disk -Number $dataDisk.Number

$allPartitions = @(Get-Partition -DiskNumber $dataDisk.Number -ErrorAction SilentlyContinue)
$markerPartitions = @($allPartitions | Where-Object { $_.GptType -eq $initMarkerType })
$dataPartitions = @($allPartitions | Where-Object {
    $_.Type -ne 'Reserved' -and $_.GptType -ne $initMarkerType
})
if ($markerPartitions.Count -gt 1 -or $dataPartitions.Count -gt 1) {
    Write-DiskLog 'ERROR: unexpected partition layout on the managed data disk; refusing to choose or modify a partition.'
    exit 1
}

if ($dataDisk.PartitionStyle -eq 'RAW') {
    if (-not (Test-DiskPrefixIsZero $dataDisk)) {
        Write-DiskLog 'ERROR: RAW disk contains nonzero data in its first MiB; refusing to initialize it.'
        exit 1
    }
    Write-DiskLog 'Initializing truly RAW managed data disk as GPT.'
    Initialize-Disk -Number $dataDisk.Number -PartitionStyle GPT
    $dataDisk = Get-Disk -Number $dataDisk.Number
    $allPartitions = @()
    $markerPartitions = @()
    $dataPartitions = @()
}

if ($dataDisk.PartitionStyle -ne 'GPT') {
    Write-DiskLog "ERROR: existing disk uses partition style '$($dataDisk.PartitionStyle)', not GPT; preserving it."
    exit 1
}

if ($dataPartitions.Count -eq 0) {
    if ($markerPartitions.Count -eq 0) {
        Write-DiskLog 'Creating persistent on-disk initialization marker.'
        $markerPartitions = @(New-Partition -DiskNumber $dataDisk.Number -Size 16MB -GptType $initMarkerType)
    }
    Write-DiskLog 'Creating the data partition.'
    $dataPartitions = @(New-Partition -DiskNumber $dataDisk.Number -UseMaximumSize -GptType $basicDataType)
}

$partition = $dataPartitions[0]
$volume = Get-Volume -Partition $partition -ErrorAction SilentlyContinue
$isUnformatted = (-not $volume) -or
    ($volume.FileSystemType -eq 'Unknown') -or
    ([string]::IsNullOrWhiteSpace($volume.FileSystem)) -or
    ($volume.FileSystem -eq 'RAW')
if ($isUnformatted -or $markerPartitions.Count -eq 1) {
    if ($markerPartitions.Count -ne 1) {
        Write-DiskLog 'ERROR: filesystem-less partition lacks the on-disk initialization marker; refusing to format it.'
        exit 1
    }
    Write-DiskLog "Formatting script-created partition $($partition.PartitionNumber) as NTFS."
    Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel 'data' -Force -Confirm:$false | Out-Null
    $partition = Get-Partition -DiskNumber $dataDisk.Number -PartitionNumber $partition.PartitionNumber
    $volume = Get-Volume -Partition $partition
}
else {
    Write-DiskLog "Existing filesystem '$($volume.FileSystem)' found; preserving all data."
}

if ($volume.FileSystem -ne 'NTFS') {
    Write-DiskLog "ERROR: existing filesystem '$($volume.FileSystem)' is not NTFS; preserving it but refusing to use it as the Windows data volume."
    exit 1
}

if ($markerPartitions.Count -eq 1) {
    Write-DiskLog 'Initialization completed; removing the on-disk marker partition.'
    $dataOffset = $partition.Offset
    Remove-Partition -DiskNumber $dataDisk.Number -PartitionNumber $markerPartitions[0].PartitionNumber -Confirm:$false
    $partition = Get-Partition -DiskNumber $dataDisk.Number -ErrorAction SilentlyContinue |
        Where-Object { $_.Offset -eq $dataOffset } |
        Select-Object -First 1
    if (-not $partition) {
        Write-DiskLog 'ERROR: data partition could not be found after removing the initialization marker.'
        exit 1
    }
}

$letterConflict = Get-Volume -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue
if ($letterConflict) {
    $letterPartition = Get-Partition -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue
    if (-not $letterPartition -or $letterPartition.DiskNumber -ne $dataDisk.Number -or
        $letterPartition.PartitionNumber -ne $partition.PartitionNumber) {
        Write-DiskLog "ERROR: ${DataDriveLetter}: remains occupied by an unexpected volume."
        exit 1
    }
}
if ($partition.DriveLetter -ne $DataDriveLetter) {
    try {
        Set-Partition -DiskNumber $dataDisk.Number -PartitionNumber $partition.PartitionNumber -NewDriveLetter $DataDriveLetter
    }
    catch {
        Write-DiskLog "ERROR: failed to assign ${DataDriveLetter}: to the managed data disk: $($_.Exception.Message)"
        exit 1
    }
}

$verifiedPartition = Get-Partition -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue
$verifiedVolume = Get-Volume -DriveLetter $DataDriveLetter -ErrorAction SilentlyContinue
if (-not $verifiedPartition -or $verifiedPartition.DiskNumber -ne $dataDisk.Number -or
    -not $verifiedVolume -or $verifiedVolume.FileSystem -ne 'NTFS') {
    Write-DiskLog "ERROR: final verification failed; ${DataDriveLetter}: is not the selected managed NTFS data disk."
    exit 1
}

Ensure-RegistryKey $pendingPath
Set-ItemProperty -Path $pendingPath -Name Completed -Value 1
Remove-ItemProperty -Path $pendingPath -Name RebootRequestedFromBoot -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $continuationTask -Confirm:$false -ErrorAction SilentlyContinue
Write-DiskLog "=== setup-datadisk finished OK; managed disk is mounted at ${DataDriveLetter}: ==="
$mutex.ReleaseMutex()
$mutexHeld = $false
