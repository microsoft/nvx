# Default Windows VMSS bootstrap ("cloud-init equivalent").
#
# Runs through the Custom Script Extension on every instance. It MUST stay
# idempotent: the extension can be re-run by the platform (extension
# reprovisioning, OS upgrades), and the script is re-run on every reimage.
#
# Note that the payload this script is configured from travels in
# osProfile.customData, which Azure only applies at provisioning time. Changes to
# the script, to publicKeys or to extraBootstrapScriptEncoded therefore reach
# existing instances only after `az vmss reimage` (new instances pick them up
# automatically).
#
# What it does:
#   1. Installs / enables the OpenSSH Server so `ssh` works out of the box.
#   2. Makes PowerShell the default SSH shell (so `ssh host` lands in a
#      PowerShell prompt, not cmd.exe).
#   3. Installs the provided SSH public keys for administrator login.
#   4. Opens TCP 22 in the Windows Firewall.
#   5. Runs an optional caller-supplied extension script, so deployments can
#      add their own setup without forking this file.
#
# Everything is logged to C:\WindowsAzure\Logs\vmss-bootstrap.log.

[CmdletBinding()]
param(
    # Path to the JSON configuration written by the deployment. When present its
    # values take precedence over the parameter defaults below. This is how the
    # Bicep templates pass configuration, because the payload (script + keys +
    # extension script) is far too large for a cmd.exe command line.
    [string] $ConfigPath = 'C:\AzureData\vmss-bootstrap-config.json',

    # Administrator account the SSH keys are installed for. Only used for
    # logging/diagnostics: OpenSSH on Windows authorizes all administrators
    # through administrators_authorized_keys.
    [string] $AdminUsername = 'ciadmin',

    # Base64 (UTF8) encoded newline-separated list of SSH public keys.
    [string] $PublicKeysBase64 = '',

    # Default shell for SSH sessions.
    [ValidateSet('powershell', 'pwsh', 'cmd')]
    [string] $DefaultShell = 'powershell',

    # Base64 (UTF8) encoded PowerShell script executed at the end of the
    # bootstrap. This is the supported extension point.
    [string] $ExtraScriptBase64 = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$logDir = 'C:\WindowsAzure\Logs'
$logFile = Join-Path $logDir 'vmss-bootstrap.log'
if (-not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Path $logDir -Force | Out-Null
}

function Write-Log {
    param([string] $Message, [string] $Level = 'INFO')
    $line = "[{0}] [{1}] {2}" -f (Get-Date -Format 'o'), $Level, $Message
    Add-Content -Path $logFile -Value $line
    Write-Output $line
}

function ConvertFrom-Base64Utf8 {
    param([string] $Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return '' }
    return [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($Value))
}

function Set-AuthorizedKeysAcl {
    <#
        sshd ignores administrators_authorized_keys unless it is writable only by
        Administrators and SYSTEM. The ACL is rebuilt from scratch rather than
        patched with icacls /grant, because /grant leaves unrelated explicit ACEs
        (and therefore possibly write access for other principals) in place.
    #>
    param([Parameter(Mandatory = $true)][string] $Path)

    # Well-known SIDs, so this does not depend on the system locale.
    $administrators = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-544')
    $system = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18')

    $acl = New-Object System.Security.AccessControl.FileSecurity
    # Protect from inheritance and drop any inherited rules.
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner($administrators)
    foreach ($sid in @($administrators, $system)) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $sid,
            [System.Security.AccessControl.FileSystemRights]::FullControl,
            [System.Security.AccessControl.AccessControlType]::Allow)))
    }
    Set-Acl -Path $Path -AclObject $acl

    # Verify, because a silently wrong ACL means sshd rejects every key.
    $applied = (Get-Acl -Path $Path).Access |
        Where-Object { $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]) -notin @($administrators, $system) }
    if ($applied) {
        throw "Unexpected ACEs remain on ${Path}: $(($applied | ForEach-Object { $_.IdentityReference }) -join ', ')"
    }
    Write-Log "ACL on $Path restricted to Administrators and SYSTEM"
}

Write-Log "=== vmss bootstrap starting (admin=$AdminUsername, shell=$DefaultShell) ==="

# ---------------------------------------------------------------------------
# 0. Configuration file (written by the deployment bootstrapper)
# ---------------------------------------------------------------------------
if ($ConfigPath -and (Test-Path $ConfigPath)) {
    Write-Log "Reading configuration from $ConfigPath"
    $config = Get-Content -Path $ConfigPath -Raw | ConvertFrom-Json

    if ($config.adminUsername) { $AdminUsername = $config.adminUsername }
    if ($config.defaultShell) { $DefaultShell = $config.defaultShell }
    if ($config.extraScriptBase64) { $ExtraScriptBase64 = $config.extraScriptBase64 }
    if ($config.publicKeys) {
        $keyText = (@($config.publicKeys) -join "`n")
        $PublicKeysBase64 = [System.Convert]::ToBase64String(
            [System.Text.Encoding]::UTF8.GetBytes($keyText))
    }
    Write-Log "Configuration applied (admin=$AdminUsername, shell=$DefaultShell)"
}
else {
    Write-Log "No configuration file at $ConfigPath; using parameter values"
}

# ---------------------------------------------------------------------------
# 1. OpenSSH Server
# ---------------------------------------------------------------------------
try {
    $capability = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' |
        Select-Object -First 1
    if ($capability -and $capability.State -ne 'Installed') {
        Write-Log 'Installing OpenSSH.Server Windows capability'
        Add-WindowsCapability -Online -Name $capability.Name | Out-Null
    }
    else {
        Write-Log 'OpenSSH.Server capability already installed'
    }
}
catch {
    # Some images ship sshd without exposing the capability. Only fail later if
    # the service is genuinely missing.
    Write-Log "OpenSSH capability check failed: $($_.Exception.Message)" 'WARN'
}

$sshd = Get-Service -Name sshd -ErrorAction SilentlyContinue
if (-not $sshd) {
    Write-Log 'sshd service not found after capability install' 'ERROR'
    throw 'OpenSSH Server is not available on this image. Use an image that supports OpenSSH Server.'
}

foreach ($svc in @('sshd', 'ssh-agent')) {
    if (Get-Service -Name $svc -ErrorAction SilentlyContinue) {
        Set-Service -Name $svc -StartupType Automatic
        # Set-Service does not start a stopped service, and both are expected to
        # be running for the lifetime of the instance.
        Start-Service -Name $svc -ErrorAction SilentlyContinue
        Write-Log "Service $svc set to Automatic startup, status $((Get-Service -Name $svc).Status)"
    }
}

# ---------------------------------------------------------------------------
# 2. Default shell for SSH sessions
# ---------------------------------------------------------------------------
$shellPath = switch ($DefaultShell) {
    'pwsh' {
        $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
        if ($pwsh) { $pwsh.Source }
        else {
            Write-Log 'pwsh.exe not found, falling back to Windows PowerShell' 'WARN'
            Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        }
    }
    'cmd' { Join-Path $env:SystemRoot 'System32\cmd.exe' }
    default { Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe' }
}

$sshRegPath = 'HKLM:\SOFTWARE\OpenSSH'
if (-not (Test-Path $sshRegPath)) {
    New-Item -Path $sshRegPath -Force | Out-Null
}
$shellCommandOption = if ($DefaultShell -eq 'cmd') { '/c' } else { '-Command' }
New-ItemProperty -Path $sshRegPath -Name DefaultShell -Value $shellPath -PropertyType String -Force | Out-Null
New-ItemProperty -Path $sshRegPath -Name DefaultShellCommandOption -Value $shellCommandOption -PropertyType String -Force | Out-Null
Write-Log "Default SSH shell set to $shellPath (command option $shellCommandOption)"

# ---------------------------------------------------------------------------
# 3. Authorized keys
# ---------------------------------------------------------------------------
$sshDataDir = Join-Path $env:ProgramData 'ssh'
if (-not (Test-Path $sshDataDir)) {
    New-Item -ItemType Directory -Path $sshDataDir -Force | Out-Null
}

$authorizedKeysFile = Join-Path $sshDataDir 'administrators_authorized_keys'
$publicKeys = (ConvertFrom-Base64Utf8 $PublicKeysBase64) -split "`r?`n" |
    ForEach-Object { $_.Trim() } |
    Where-Object { $_ -and -not $_.StartsWith('#') }

if ($publicKeys.Count -gt 0) {
    # The deployed key set is AUTHORITATIVE: the file is replaced, not merged.
    # Merging would make key removal impossible -- dropping a key from
    # `publicKeys` and redeploying has to actually revoke that key's access.
    # Consequence: keys added out-of-band on the instance are removed here.
    $desired = @($publicKeys | Select-Object -Unique)

    # Write without a BOM (sshd rejects BOM-prefixed key files) and via a
    # temporary file, so an interrupted run cannot leave a truncated key file
    # that would lock everyone out.
    $tempKeysFile = "$authorizedKeysFile.new"
    [System.IO.File]::WriteAllText(
        $tempKeysFile,
        ($desired -join "`n") + "`n",
        (New-Object System.Text.UTF8Encoding($false)))

    Set-AuthorizedKeysAcl -Path $tempKeysFile
    Move-Item -Path $tempKeysFile -Destination $authorizedKeysFile -Force
    Set-AuthorizedKeysAcl -Path $authorizedKeysFile

    Write-Log "Authorized keys file set to the $($desired.Count) deployed key(s)"
}
else {
    Write-Log 'No public keys supplied; existing authorized keys left untouched and password authentication remains the only option' 'WARN'
}

# ---------------------------------------------------------------------------
# 4. Firewall
# ---------------------------------------------------------------------------
$firewallRuleName = 'OpenSSH-Server-In-TCP'
$rule = Get-NetFirewallRule -Name $firewallRuleName -ErrorAction SilentlyContinue
if (-not $rule) {
    New-NetFirewallRule -Name $firewallRuleName `
        -DisplayName 'OpenSSH Server (sshd)' `
        -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
    Write-Log 'Created inbound firewall rule for TCP 22'
}
else {
    Enable-NetFirewallRule -Name $firewallRuleName
    Write-Log 'Inbound firewall rule for TCP 22 already present and enabled'
}

# ---------------------------------------------------------------------------
# 5. Start / restart sshd so the configuration above takes effect
# ---------------------------------------------------------------------------
Restart-Service -Name sshd -Force
Write-Log "sshd status: $((Get-Service -Name sshd).Status)"

# ---------------------------------------------------------------------------
# 6. Caller-supplied extension script
# ---------------------------------------------------------------------------
$extraScript = ConvertFrom-Base64Utf8 $ExtraScriptBase64
if (-not [string]::IsNullOrWhiteSpace($extraScript)) {
    $extraScriptPath = Join-Path $logDir 'vmss-bootstrap-extra.ps1'
    Set-Content -Path $extraScriptPath -Value $extraScript -Encoding UTF8
    Write-Log "Running extra bootstrap script ($extraScriptPath)"
    try {
        & powershell.exe -ExecutionPolicy Bypass -NoProfile -File $extraScriptPath *>&1 |
            ForEach-Object { Write-Log "[extra] $_" }
        # $ErrorActionPreference does not turn a child process's nonzero exit
        # into an exception, so the exit code has to be checked explicitly.
        if ($LASTEXITCODE -ne 0) {
            throw "Extra bootstrap script exited with code $LASTEXITCODE"
        }
        Write-Log 'Extra bootstrap script completed'
    }
    catch {
        Write-Log "Extra bootstrap script failed: $($_.Exception.Message)" 'ERROR'
        throw
    }
}
else {
    Write-Log 'No extra bootstrap script supplied'
}

Write-Log '=== vmss bootstrap completed successfully ==='
