#requires -Version 5.1

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $env:GITHUB_PATH) {
    throw 'GITHUB_PATH is unavailable; this script must run inside GitHub Actions'
}

$gitRoot = Split-Path (Split-Path (Get-Command git.exe).Source -Parent) -Parent
$gnuTar = Join-Path $gitRoot 'usr\bin\tar.exe'
if (-not (Test-Path -LiteralPath $gnuTar -PathType Leaf)) {
    throw "Git for Windows GNU tar is missing at $gnuTar"
}

$archive = Join-Path $env:RUNNER_TEMP 'zstd-v1.5.7-win64.zip'
Invoke-WebRequest `
    'https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-v1.5.7-win64.zip' `
    -UseBasicParsing `
    -OutFile $archive
$expectedHash = 'ACB4E8111511749DC7A3EBEDCA9B04190E37A17AFEB73F55D4425DBF0B90FAD9'
$observedHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
if ($observedHash -ne $expectedHash) {
    throw "zstd archive SHA-256 mismatch: $observedHash"
}

$destination = Join-Path $env:RUNNER_TEMP 'zstd-v1.5.7-win64'
Remove-Item -LiteralPath $destination -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -LiteralPath $archive -DestinationPath $destination
$zstd = Join-Path $destination 'zstd-v1.5.7-win64\zstd.exe'
if (-not (Test-Path -LiteralPath $zstd -PathType Leaf)) {
    throw "zstd.exe is missing from $destination"
}

$utf8 = New-Object Text.UTF8Encoding($false)
foreach ($directory in @((Split-Path $gnuTar -Parent), (Split-Path $zstd -Parent))) {
    [IO.File]::AppendAllText(
        $env:GITHUB_PATH,
        $directory + [Environment]::NewLine,
        $utf8
    )
}

& $gnuTar --version | Select-Object -First 1
& $zstd --version