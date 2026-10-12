[CmdletBinding()]
param(
    [string]$ConfigPath = 'C:\AzureData\vmss-bootstrap-config.json'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
$payload = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
$bytes = [Convert]::FromBase64String($payload.package.data)
$hash = [Security.Cryptography.SHA256]::Create()
try {
    $digest = ([BitConverter]::ToString($hash.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
}
finally { $hash.Dispose() }
if ($digest -ne $payload.package.sha256) { throw 'Runner package checksum mismatch.' }

$root = 'C:\ProgramData\NvxBootstrap'
if (Test-Path -LiteralPath $root) {
    $item = Get-Item -LiteralPath $root -Force
    if (-not $item.PSIsContainer -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Bootstrap directory is not a regular directory.'
    }
}
else { New-Item -ItemType Directory -Path $root | Out-Null }
$acl = [Security.AccessControl.DirectorySecurity]::new()
$acl.SetAccessRuleProtection($true, $false)
foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl',
        'ContainerInherit,ObjectInherit', 'None', 'Allow'))
}
Set-Acl -LiteralPath $root -AclObject $acl

$expected = @(
    'config.json', 'setup-windows-whp.ps1', 'tool-versions.conf',
    'setup-data-disk.ps1', 'bootstrap-runner.ps1'
)
$stream = [IO.MemoryStream]::new($bytes)
$archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read)
try {
    $names = @($archive.Entries | ForEach-Object FullName)
    if ($names.Count -ne $expected.Count -or
        @($names | Select-Object -Unique).Count -ne $expected.Count -or
        @(Compare-Object $expected $names).Count -ne 0) {
        throw 'Runner package contains unexpected or duplicate file names.'
    }
    foreach ($entry in $archive.Entries) {
        $path = Join-Path $root $entry.FullName
        foreach ($candidate in @($path)) {
            if (Test-Path -LiteralPath $candidate) {
                $item = Get-Item -LiteralPath $candidate -Force
                if ($item.PSIsContainer -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                    throw "Bootstrap target is not a regular file: $candidate"
                }
            }
        }
        $temporary = Join-Path $root ([guid]::NewGuid().ToString('N') + '.tmp')
        $entryStream = $entry.Open()
        try {
            $output = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
            try { $entryStream.CopyTo($output) }
            finally { $output.Dispose() }
            Move-Item -LiteralPath $temporary -Destination $path -Force
        }
        finally {
            $entryStream.Dispose()
            if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
        }
    }
}
finally { $archive.Dispose(); $stream.Dispose() }
& powershell.exe -NoProfile -ExecutionPolicy Bypass `
    -File (Join-Path $root 'bootstrap-runner.ps1') -ConfigPath (Join-Path $root 'config.json')
if ($LASTEXITCODE -ne 0) { throw "Runner bootstrap failed: $LASTEXITCODE" }
