# Storage operations of the Windows data-disk setup.
#
# The Python controller decides which disk, partition, and drive letter to use
# and whether anything may change; this adapter only runs the Storage and CIM
# cmdlets that it names. It reads a JSON request from standard input, or from
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

function Get-Letter {
    param($Value)
    if ($null -ne $Value -and [char]::IsLetter([char]$Value)) { return ([string]$Value).ToUpperInvariant() }
    return $null
}

function ConvertTo-DiskResult {
    param($Disk)
    return [ordered]@{
        number = [int]$Disk.Number
        isBoot = [bool]$Disk.IsBoot
        isSystem = [bool]$Disk.IsSystem
        isOffline = [bool]$Disk.IsOffline
        isReadOnly = [bool]$Disk.IsReadOnly
        partitionStyle = [string]$Disk.PartitionStyle
        busType = [string]$Disk.BusType
        friendlyName = [string]$Disk.FriendlyName
        model = [string]$Disk.Model
        location = [string]$Disk.Location
        size = $Disk.Size
    }
}

function ConvertTo-PartitionResult {
    param($Partition)
    return [ordered]@{
        diskNumber = [int]$Partition.DiskNumber
        partitionNumber = [int]$Partition.PartitionNumber
        type = [string]$Partition.Type
        gptType = [string]$Partition.GptType
        offset = $Partition.Offset
        driveLetter = Get-Letter $Partition.DriveLetter
    }
}

function ConvertTo-VolumeResult {
    param($Volume)
    if (-not $Volume) { return $null }
    return [ordered]@{
        fileSystem = [string]$Volume.FileSystem
        fileSystemType = [string]$Volume.FileSystemType
        fileSystemLabel = [string]$Volume.FileSystemLabel
    }
}

function Get-RequestedPartition {
    return Get-Partition -DiskNumber $arguments.diskNumber -PartitionNumber $arguments.partitionNumber
}

switch ($Operation) {
    'Disks' {
        Update-HostStorageCache
        Write-Result ([ordered]@{
            disks = @(Get-Disk | ForEach-Object { ConvertTo-DiskResult $_ })
            guestDisks = @(Get-CimInstance -ClassName Win32_DiskDrive | ForEach-Object {
                [ordered]@{
                    index = $_.Index
                    deviceId = [string]$_.DeviceID
                    scsiLogicalUnit = $_.SCSILogicalUnit
                }
            })
        })
    }
    'Disk' { Write-Result (ConvertTo-DiskResult (Get-Disk -Number $arguments.number)) }
    'SetDisk' {
        if ($arguments.online) { Set-Disk -Number $arguments.number -IsOffline $false }
        if ($arguments.writable) { Set-Disk -Number $arguments.number -IsReadOnly $false }
        Write-Result (ConvertTo-DiskResult (Get-Disk -Number $arguments.number))
    }
    'Partitions' {
        Write-Result @(Get-Partition -DiskNumber $arguments.diskNumber -ErrorAction SilentlyContinue |
            ForEach-Object { ConvertTo-PartitionResult $_ })
    }
    'PartitionAtLetter' {
        $partition = Get-Partition -DriveLetter $arguments.letter -ErrorAction SilentlyContinue
        Write-Result $(if ($partition) { ConvertTo-PartitionResult $partition } else { $null })
    }
    'Volume' {
        Write-Result (ConvertTo-VolumeResult (Get-Volume -Partition (Get-RequestedPartition) -ErrorAction SilentlyContinue))
    }
    'VolumeAtLetter' {
        Write-Result (ConvertTo-VolumeResult (Get-Volume -DriveLetter $arguments.letter -ErrorAction SilentlyContinue))
    }
    'UsedLetters' {
        Write-Result @(Get-Volume -ErrorAction SilentlyContinue | ForEach-Object { Get-Letter $_.DriveLetter } |
            Where-Object { $_ })
    }
    'ActivePagefiles' {
        Write-Result @(Get-CimInstance -ClassName Win32_PageFileUsage -ErrorAction SilentlyContinue |
            ForEach-Object { [string]$_.Name })
    }
    'SetPartitionLetter' {
        Set-Partition -DiskNumber $arguments.diskNumber -PartitionNumber $arguments.partitionNumber `
            -NewDriveLetter $arguments.letter
        Write-Result $true
    }
    'InitializeGpt' {
        Initialize-Disk -Number $arguments.number -PartitionStyle GPT
        Write-Result (ConvertTo-DiskResult (Get-Disk -Number $arguments.number))
    }
    'NewPartition' {
        $partition = if ($arguments.PSObject.Properties['size']) {
            New-Partition -DiskNumber $arguments.diskNumber -Size $arguments.size -GptType $arguments.gptType
        }
        else {
            New-Partition -DiskNumber $arguments.diskNumber -UseMaximumSize -GptType $arguments.gptType
        }
        Write-Result (ConvertTo-PartitionResult $partition)
    }
    'FormatNtfs' {
        Format-Volume -Partition (Get-RequestedPartition) -FileSystem NTFS -NewFileSystemLabel 'data' `
            -Force -Confirm:$false | Out-Null
        Write-Result (ConvertTo-VolumeResult (Get-Volume -Partition (Get-RequestedPartition)))
    }
    'RemovePartition' {
        Remove-Partition -DiskNumber $arguments.diskNumber -PartitionNumber $arguments.partitionNumber `
            -Confirm:$false
        Write-Result $true
    }
    default { throw "Unsupported storage operation: $Operation" }
}
