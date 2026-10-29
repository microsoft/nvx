# Entry point of the Windows scale-set bootstrap, which the Custom Script
# Extension runs as SYSTEM on every provisioning and reimage.
#
# Python is not part of Windows images, and the runner toolchain installs its
# own Python only later. This shim therefore establishes the trust that the
# Python stager needs and an SSH recovery path that does not depend on it:
#   1. C:\ProgramData\NvxBootstrap is a regular directory that only SYSTEM and
#      Administrators can change and that this shim created protected, so that
#      no other account controls an entry in it. The directory of an earlier
#      installer is moved aside, and one that another account can change is
#      refused.
#   2. OpenSSH receives the administrator keys and inbound rule before the
#      interpreter download, so a failed download does not remove SSH recovery.
#   3. python.org's embeddable Python, pinned by version, URL, and SHA-256 in
#      the payload, is installed in its python directory; it is reused only
#      when its recorded archive digest and reported version still match.
#   4. The bootstrap package from the payload matches its SHA-256 before any
#      of its code runs.
# It then unpacks the package with Python's own tar extractor and runs its
# windows-stage command with that interpreter in isolated mode. The stager
# reapplies the authoritative OpenSSH configuration, installs the package,
# and hands the runner bootstrap to a SYSTEM task.
[CmdletBinding()]
param(
    # The decoded custom data that the extension command wrote.
    [string]$ConfigPath = 'C:\AzureData\vmss-bootstrap-config.json'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = 'C:\ProgramData\NvxBootstrap'
$logPath = 'C:\WindowsAzure\Logs\vmss-bootstrap.log'

function Write-Log {
    param([string]$Message)
    $line = '[{0}] {1}' -f [DateTime]::UtcNow.ToString('o'), $Message
    New-Item -ItemType Directory -Path (Split-Path -Parent $logPath) -Force | Out-Null
    Add-Content -LiteralPath $logPath -Value $line
    # The host, not the pipeline, so that functions that log return only their result.
    Write-Host $line
}

function Assert-NotReparsePoint {
    param([string]$Path, [switch]$Directory)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -ne [bool]$Directory -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Bootstrap path is not a regular $(if ($Directory) { 'directory' } else { 'file' }): $Path"
    }
}

function New-RecoveryKeySecurity {
    $security = [Security.AccessControl.FileSecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $security.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach ($sid in 'S-1-5-18', 'S-1-5-32-544') {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'Allow'))
    }
    return $security
}

# Establish SSH recovery before the interpreter download. The Python stager
# repeats this authoritatively after the verified package is available.
function Initialize-RecoverySsh {
    param([Parameter(Mandatory = $true)]$Payload)
    $keys = @($Payload.publicKeys | ForEach-Object { "$_" -split "`r?`n" } |
        ForEach-Object { $_.Trim() } | Where-Object { $_ -and -not $_.StartsWith('#') } |
        Select-Object -Unique)
    if ($keys.Count -eq 0) {
        Write-Log 'WARN: No public keys supplied; pre-Python SSH recovery is unavailable.'
        return
    }
    try {
        $capability = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
        if ($null -ne $capability -and $capability.State -ne 'Installed') {
            Add-WindowsCapability -Online -Name $capability.Name | Out-Null
        }
    }
    catch { Write-Log "WARN: OpenSSH capability check failed: $($_.Exception.Message)" }
    if (@(Get-Service -Name sshd -ErrorAction SilentlyContinue).Count -ne 1) {
        throw 'OpenSSH Server is unavailable; pre-Python SSH recovery cannot be configured.'
    }
    Set-Service -Name sshd -StartupType Automatic
    Start-Service -Name sshd
    $ssh = Join-Path $env:ProgramData 'ssh'
    if (-not (Test-Path -LiteralPath $ssh)) {
        throw 'OpenSSH Server did not create its data directory.'
    }
    Assert-NotReparsePoint $ssh -Directory
    $target = Join-Path $ssh 'administrators_authorized_keys'
    if (Test-Path -LiteralPath $target) { Assert-NotReparsePoint $target }
    $temporary = "$target.new"
    try {
        [IO.File]::WriteAllText($temporary, ($keys -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
        Set-Acl -LiteralPath $temporary -AclObject (New-RecoveryKeySecurity)
        Move-Item -LiteralPath $temporary -Destination $target -Force
    }
    finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force } }
    $rule = Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue
    if ($null -eq $rule) {
        New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
            -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
    }
    else { Enable-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' | Out-Null }
    Write-Log "Pre-Python SSH recovery configured with $($keys.Count) administrator key(s)."
}

# Owned by Administrators, with inherited full control for SYSTEM and
# Administrators only.
function New-TrustedSecurity {
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $security.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach ($sid in 'S-1-5-18', 'S-1-5-32-544') {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl',
            'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    return $security
}

function Test-TrustedSecurity {
    param([Parameter(Mandatory = $true)]$Security)
    $trusted = 'S-1-5-18', 'S-1-5-32-544'
    $owner = $Security.GetOwner([Security.Principal.SecurityIdentifier])
    if ($null -eq $owner -or $owner.Value -notin $trusted -or -not $Security.AreAccessRulesProtected) {
        return $false
    }
    $rules = @($Security.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    return @($rules | Where-Object {
        $_.AccessControlType -ne 'Allow' -or $_.IdentityReference.Value -notin $trusted
    }).Count -eq 0
}

# Creates the directory with its protection, so that no other account can add
# an entry to it at any moment.
function New-ProtectedDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)
    [void][IO.Directory]::CreateDirectory($Path, (New-TrustedSecurity))
}

function Test-TrustedMark {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf) -or
        (Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        return $false
    }
    $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier])
    return $null -ne $owner -and $owner.Value -in 'S-1-5-18', 'S-1-5-32-544'
}

# The stager runs code from this directory as SYSTEM, so every entry in it
# must come from SYSTEM or Administrators. This shim creates the directory
# protected from the start and then marks it, and uses an existing directory
# again only with that mark. Earlier installers protected the directory only
# after creating it, so another account may have added entries first: such a
# directory is moved aside, never searched or deleted. A directory that
# another account can change is refused.
function Set-TrustedDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)
    $mark = Join-Path $Path 'created-protected'
    if (Test-Path -LiteralPath $Path) {
        Assert-NotReparsePoint $Path -Directory
        if (-not (Test-TrustedSecurity (Get-Acl -LiteralPath $Path))) {
            throw "Bootstrap directory is not protected; refusing to use it: $Path"
        }
        if (Test-TrustedMark $mark) { return }
        $aside = '{0}.legacy-{1}' -f (Split-Path -Leaf $Path), [guid]::NewGuid().ToString('N')
        Rename-Item -LiteralPath $Path -NewName $aside
        Write-Log "Moved the bootstrap directory of an earlier installer aside to $aside."
    }
    New-ProtectedDirectory $Path
    Assert-NotReparsePoint $Path -Directory
    # Another account may have created the directory first.
    if (-not (Test-TrustedSecurity (Get-Acl -LiteralPath $Path))) {
        throw "Bootstrap directory is not protected; refusing to use it: $Path"
    }
    [IO.File]::WriteAllText($mark, [DateTime]::UtcNow.ToString('o'))
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try { return -join ($algorithm.ComputeHash($Bytes) | ForEach-Object { $_.ToString('x2') }) }
    finally { $algorithm.Dispose() }
}

function Test-Python {
    param([string]$Executable, [string]$Version)
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { return $false }
    try { $reported = & $Executable -I -V }
    catch { return $false }
    return $LASTEXITCODE -eq 0 -and [string]$reported -eq "Python $Version"
}

function Install-Python {
    param([Parameter(Mandatory = $true)]$Pin)
    if ($Pin.version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+$' -or $Pin.sha256 -cnotmatch '^[0-9a-f]{64}$' -or
        $Pin.url -cnotmatch '^https://www\.python\.org/ftp/python/') {
        throw 'The payload has no valid Python pin.'
    }
    $directory = Join-Path $root 'python'
    $executable = Join-Path $directory 'python.exe'
    $recorded = Join-Path $directory 'nvx-archive.sha256'
    if (Test-Path -LiteralPath $directory) { Assert-NotReparsePoint $directory -Directory }
    if ((Test-Path -LiteralPath $recorded -PathType Leaf) -and
        ([IO.File]::ReadAllText($recorded).Trim() -ceq $Pin.sha256) -and
        (Test-Python $executable $Pin.version)) {
        Write-Log "Using the installed Python $($Pin.version)."
        return $executable
    }
    $archive = Join-Path $root ([guid]::NewGuid().ToString('N') + '.zip')
    $staging = Join-Path $root ([guid]::NewGuid().ToString('N'))
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor
            [Net.SecurityProtocolType]::Tls12
        for ($attempt = 1; ; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $Pin.url -OutFile $archive
                break
            }
            catch {
                if ($attempt -ge 5) { throw }
                Write-Log "Python download attempt $attempt failed: $($_.Exception.Message)"
                Start-Sleep -Seconds (15 * $attempt)
            }
        }
        $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($digest -cne $Pin.sha256) { throw "Python archive checksum mismatch: $digest" }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::ExtractToDirectory($archive, $staging)
        if (-not (Test-Python (Join-Path $staging 'python.exe') $Pin.version)) {
            throw "The Python archive does not provide Python $($Pin.version)."
        }
        [IO.File]::WriteAllText((Join-Path $staging 'nvx-archive.sha256'), $Pin.sha256)
        if (Test-Path -LiteralPath $directory) {
            Assert-NotReparsePoint $directory -Directory
            Remove-Item -LiteralPath $directory -Recurse -Force
        }
        Move-Item -LiteralPath $staging -Destination $directory
    }
    finally {
        foreach ($path in $archive, $staging) {
            if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
        }
    }
    Write-Log "Installed Python $($Pin.version) from $($Pin.url)."
    return $executable
}

function Save-Package {
    param([Parameter(Mandatory = $true)]$Package)
    $bytes = [Convert]::FromBase64String($Package.data)
    if ((Get-Sha256 $bytes) -cne $Package.sha256) { throw 'Runner package checksum mismatch.' }
    $path = Join-Path $root 'package.tar.xz'
    if (Test-Path -LiteralPath $path) { Assert-NotReparsePoint $path }
    $temporary = Join-Path $root ([guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::WriteAllBytes($temporary, $bytes)
        Move-Item -LiteralPath $temporary -Destination $path -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
    return $path
}

try {
    Write-Log '=== vmss bootstrap starting ==='
    $payload = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
    Initialize-RecoverySsh $payload
    Set-TrustedDirectory $root
    $python = Install-Python $payload.python
    $package = Save-Package $payload.package
    $staging = Join-Path $root ([guid]::NewGuid().ToString('N'))
    try {
        # Python's own extractor, with its safe data filter, unpacks the
        # verified package so that the stager inside it can run.
        & $python -I -B -m tarfile --filter data -e $package $staging
        if ($LASTEXITCODE -ne 0) { throw "Could not unpack the runner package: $LASTEXITCODE." }
        & $python -I -B $staging windows-stage --payload $ConfigPath --package $package
        if ($LASTEXITCODE -ne 0) { throw "Windows staging failed with exit code $LASTEXITCODE." }
    }
    finally {
        if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    }
    Write-Log '=== vmss bootstrap completed successfully ==='
}
catch {
    Write-Log "ERROR: $($_.Exception.Message)"
    throw
}
