# OpenSSH operations of the Windows scale-set bootstrap.
#
# The Python stager decodes the configuration, chooses the default shell,
# writes the authorized keys, and decides the order of these operations;
# this adapter only runs the capability, service, and firewall cmdlets that
# it names. It reads a JSON request from standard input, or from -Request when
# another PowerShell script calls it, and writes a JSON result.
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

function Write-Result {
    param($Value)
    # Windows PowerShell converts $null to no text at all.
    $json = if ($null -eq $Value) { 'null' } else { ConvertTo-Json -InputObject $Value -Depth 6 -Compress }
    # The host also writes warnings and messages to standard output, so the
    # result has a marked line of its own, in ASCII for any console code page.
    Write-Output ('NVX-ADAPTER-RESULT ' +
        [regex]::Replace($json, '[^\x00-\x7F]', { '\u{0:x4}' -f [int][char]$args[0].Value }))
}

switch ($Operation) {
    'Prepare' {
        $capability = 'present'
        $warning = $null
        try {
            $installed = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
            if ($installed -and $installed.State -ne 'Installed') {
                Add-WindowsCapability -Online -Name $installed.Name | Out-Null
                $capability = 'added'
            }
        }
        catch {
            # Some images ship sshd without exposing the capability; the
            # service check below decides.
            $capability = 'unknown'
            $warning = $_.Exception.Message
        }
        $services = @()
        $present = $null -ne (Get-Service -Name sshd -ErrorAction SilentlyContinue)
        if ($present) {
            foreach ($name in 'sshd', 'ssh-agent') {
                if (Get-Service -Name $name -ErrorAction SilentlyContinue) {
                    Set-Service -Name $name -StartupType Automatic
                    # Set-Service does not start a stopped service.
                    Start-Service -Name $name -ErrorAction SilentlyContinue
                    $services += [ordered]@{ name = $name; status = [string](Get-Service -Name $name).Status }
                }
            }
        }
        Write-Result ([ordered]@{
            capability = $capability
            warning = $warning
            sshd = $present
            services = $services
        })
    }
    'AllowInbound' {
        $name = 'OpenSSH-Server-In-TCP'
        $created = $null -eq (Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue)
        if ($created) {
            New-NetFirewallRule -Name $name -DisplayName 'OpenSSH Server (sshd)' -Enabled True `
                -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
        }
        else {
            Enable-NetFirewallRule -Name $name
        }
        Write-Result ([ordered]@{ created = $created })
    }
    'Restart' {
        Restart-Service -Name sshd -Force
        Write-Result ([ordered]@{ status = [string](Get-Service -Name sshd).Status })
    }
    default { throw "Unsupported OpenSSH operation: $Operation" }
}
