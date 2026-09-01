[CmdletBinding()]
param(
    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$Workspace = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path,

    [Parameter()]
    [string]$GuestArtifactsDirectory,

    [Parameter()]
    [switch]$CheckOnly,

    [Parameter()]
    [switch]$SkipBuild
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$RustToolchain = "stable"
$MinimumRustVersion = [version]"1.95.0"
$CargoNextestVersion = "0.9.133"
$RequiredGuestArtifacts = @(
    "vmlinux",
    "vmlinux.config",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.packages.json"
)

function Assert-LastExitCode {
    param([Parameter(Mandatory = $true)][string]$Description)
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter()][string[]]$Arguments = @()
    )
    & $FilePath @Arguments
    Assert-LastExitCode $FilePath
}

function Update-ProcessPath {
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = "$machinePath;$userPath"
}

function Get-RequiredCommand {
    param([Parameter(Mandatory = $true)][string]$Name)
    $command = Get-Command $Name -ErrorAction SilentlyContinue |
    Select-Object -First 1
    if ($null -eq $command) {
        throw "required command not found: $Name"
    }
    return $command.Source
}

function Get-PythonCommand {
    param([Parameter()][switch]$AllowMissing)
    $commands = @(Get-Command python.exe -All -ErrorAction SilentlyContinue |
        Where-Object { $_.Source -notlike "*\WindowsApps\python.exe" } |
        Select-Object -ExpandProperty Source -Unique)
    foreach ($path in $commands) {
        $version = & $path -c `
            "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')" `
            2>$null
        if ($LASTEXITCODE -eq 0 -and [version]$version -ge [version]"3.10") {
            return $path
        }
    }
    if ($AllowMissing) {
        return $null
    }
    throw "Python 3.10 or newer was not found"
}

function Assert-SupportedHost {
    if ($env:OS -ne "Windows_NT") {
        throw "this script requires Windows"
    }
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw "this script requires 64-bit Windows"
    }
    if (-not (Test-Path -LiteralPath "$Workspace\scripts\nvx.py" -PathType Leaf)) {
        throw "NVX checkout not found at $Workspace"
    }
    if (-not (Test-Path -LiteralPath "$Workspace\openvmm\Cargo.toml" -PathType Leaf)) {
        throw "OpenVMM submodule is not initialized"
    }
    if ($null -ne (Get-Process openvmm -ErrorAction SilentlyContinue)) {
        throw "an OpenVMM process is already running"
    }
}

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "run this script from an elevated Windows PowerShell session"
    }
}

function Install-WinGet {
    $winget = Get-Command winget.exe -ErrorAction SilentlyContinue |
    Select-Object -First 1
    if ($null -ne $winget) {
        return $winget.Source
    }

    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Install-PackageProvider -Name NuGet -Force -Scope AllUsers -Confirm:$false |
    Out-Null
    Install-Module Microsoft.WinGet.Client `
        -Repository PSGallery `
        -Scope AllUsers `
        -Force `
        -AllowClobber `
        -Confirm:$false
    Import-Module Microsoft.WinGet.Client -Force
    Repair-WinGetPackageManager -AllUsers -Force -Latest
    Update-ProcessPath
    return (Get-RequiredCommand "winget.exe")
}

function Install-WinGetPackage {
    param(
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter()][string[]]$AdditionalArguments = @()
    )
    $arguments = @(
        "install",
        "--id", $Id,
        "--exact",
        "--silent",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity"
    ) + $AdditionalArguments
    Invoke-Native $script:WinGet $arguments
}

function Test-VisualStudioBuildTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} `
        "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        return $false
    }
    $instances = @(& $vswhere `
            -products * `
            -requires `
            Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            Microsoft.VisualStudio.Component.Windows11SDK.26100 `
            -property installationPath)
    return $LASTEXITCODE -eq 0 -and $instances.Count -gt 0
}

function Install-Toolchain {
    $script:WinGet = Install-WinGet
    Update-ProcessPath
    if ($null -eq (Get-Command git.exe -ErrorAction SilentlyContinue |
            Select-Object -First 1)) {
        Install-WinGetPackage "Git.Git" @("--scope", "machine")
        Update-ProcessPath
    }
    if ($null -eq (Get-PythonCommand -AllowMissing)) {
        Install-WinGetPackage "Python.Python.3.12" @("--scope", "machine")
        Update-ProcessPath
    }
    if (-not (Test-VisualStudioBuildTools)) {
        $vsArguments = @(
            "--override",
            "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.26100 --includeRecommended"
        )
        Install-WinGetPackage "Microsoft.VisualStudio.2022.BuildTools" $vsArguments
    }
    Update-ProcessPath

    $rustup = Get-Command rustup.exe -ErrorAction SilentlyContinue |
    Select-Object -First 1
    if ($null -eq $rustup) {
        $rustupInstaller = Join-Path $env:TEMP "rustup-init.exe"
        Invoke-WebRequest `
            -UseBasicParsing `
            -Uri "https://win.rustup.rs/x86_64" `
            -OutFile $rustupInstaller
        Invoke-Native $rustupInstaller @(
            "-y",
            "--profile", "minimal",
            "--default-toolchain", $RustToolchain
        )
    }
    Update-ProcessPath
    $rustupPath = Get-RequiredCommand "rustup.exe"
    Invoke-Native $rustupPath @(
        "toolchain", "install", $RustToolchain, "--profile", "minimal"
    )

    $cargo = Get-RequiredCommand "cargo.exe"
    $nextest = Get-Command cargo-nextest.exe -ErrorAction SilentlyContinue |
    Select-Object -First 1
    $installNextest = $null -eq $nextest
    if (-not $installNextest) {
        $nextestVersion = & $nextest.Source --version
        Assert-LastExitCode "cargo-nextest --version"
        $installNextest = ($nextestVersion -join "`n") -notmatch `
            "cargo-nextest $([regex]::Escape($CargoNextestVersion))"
    }
    if ($installNextest) {
        Invoke-Native $cargo @(
            "+$RustToolchain", "install", "--locked", "--force", "cargo-nextest",
            "--version", $CargoNextestVersion
        )
    }
}

function Get-CurrentRevision {
    $git = Get-RequiredCommand "git.exe"
    $revision = (& $git -C $Workspace rev-parse HEAD).Trim()
    Assert-LastExitCode "git rev-parse"
    return $revision
}

function Assert-GuestArtifactSet {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$ExpectedRevision
    )
    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
        throw "guest artifact directory not found: $Directory"
    }
    $checksumPath = Join-Path $Directory "SHA256SUMS"
    if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
        throw "guest artifact checksum file not found: $checksumPath"
    }

    $expectedHashes = @{}
    foreach ($line in Get-Content -LiteralPath $checksumPath) {
        if ($line -notmatch "^([0-9a-fA-F]{64})\s+\*?(.+)$") {
            throw "invalid checksum line: $line"
        }
        $expectedHashes[$Matches[2]] = $Matches[1].ToLowerInvariant()
    }

    foreach ($name in @($RequiredGuestArtifacts) + @("REVISION")) {
        if (-not $expectedHashes.ContainsKey($name)) {
            throw "missing checksum for guest artifact: $name"
        }
        $path = Join-Path $Directory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "missing guest artifact: $path"
        }
        $actualHash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $expectedHashes[$name]) {
            throw "checksum mismatch for guest artifact: $name"
        }
    }
    $bundleRevision = (Get-Content `
            -LiteralPath (Join-Path $Directory "REVISION") `
            -Raw).Trim()
    if ($bundleRevision -ne $ExpectedRevision) {
        throw "guest artifact revision is $bundleRevision, expected $ExpectedRevision"
    }
}

function Copy-GuestArtifacts {
    if ([string]::IsNullOrWhiteSpace($GuestArtifactsDirectory)) {
        return
    }
    $revision = Get-CurrentRevision
    Assert-GuestArtifactSet $GuestArtifactsDirectory $revision

    $buildDirectory = Join-Path $Workspace "build"
    New-Item -ItemType Directory -Path $buildDirectory -Force | Out-Null
    foreach ($name in @($RequiredGuestArtifacts) + @("REVISION", "SHA256SUMS")) {
        Copy-Item `
            -LiteralPath (Join-Path $GuestArtifactsDirectory $name) `
            -Destination $buildDirectory `
            -Force
    }
    Assert-GuestArtifactSet $buildDirectory $revision
}

function Get-VisualStudioEnvironment {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} `
        "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "Visual Studio Installer vswhere.exe was not found"
    }
    $installationOutput = @(& $vswhere `
            -latest `
            -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath)
    Assert-LastExitCode "Visual Studio discovery"
    $installationPath = ($installationOutput -join "").Trim()
    if ([string]::IsNullOrWhiteSpace($installationPath)) {
        throw "Visual Studio 2022 C++ build tools were not found"
    }
    $vsDevCmd = Join-Path $installationPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $vsDevCmd -PathType Leaf)) {
        throw "VsDevCmd.bat was not found at $vsDevCmd"
    }
    return $vsDevCmd
}

function Build-Nvx {
    $python = Get-PythonCommand
    $vsDevCmd = Get-VisualStudioEnvironment
    $previousToolchain = $env:RUSTUP_TOOLCHAIN
    $env:RUSTUP_TOOLCHAIN = $RustToolchain
    Push-Location $Workspace
    try {
        Invoke-Native $python @("scripts\nvx.py", "verify")
        $command = 'call "{0}" -no_logo -arch=amd64 -host_arch=amd64 && "{1}" scripts\nvx.py build-openvmm' -f $vsDevCmd, $python
        Invoke-Native $env:ComSpec @("/d", "/s", "/c", $command)
    }
    finally {
        Pop-Location
        $env:RUSTUP_TOOLCHAIN = $previousToolchain
    }
}

function Enable-Whp {
    $feature = Get-WindowsOptionalFeature `
        -Online `
        -FeatureName HypervisorPlatform
    if ($feature.State -eq "Enabled") {
        return $false
    }
    $result = Enable-WindowsOptionalFeature `
        -Online `
        -FeatureName HypervisorPlatform `
        -All `
        -NoRestart
    return [bool]$result.RestartNeeded
}

function Assert-Environment {
    param([Parameter()][switch]$RequireBuild)
    Update-ProcessPath
    $python = Get-PythonCommand
    foreach ($command in @("git.exe", "rustup.exe", "cargo.exe", "cargo-nextest.exe")) {
        [void](Get-RequiredCommand $command)
    }

    $rustVersion = & (Get-RequiredCommand "rustup.exe") `
        run $RustToolchain rustc --version
    Assert-LastExitCode "stable Rust"
    $rustVersionText = $rustVersion -join "`n"
    if ($rustVersionText -notmatch "rustc ([0-9]+\.[0-9]+\.[0-9]+)" -or
        [version]$Matches[1] -lt $MinimumRustVersion) {
        throw "Rust $MinimumRustVersion or newer is required"
    }
    $nextestVersion = & (Get-RequiredCommand "cargo-nextest.exe") --version
    Assert-LastExitCode "cargo-nextest --version"
    if (($nextestVersion -join "`n") -notmatch `
            "cargo-nextest $([regex]::Escape($CargoNextestVersion))") {
        throw "cargo-nextest $CargoNextestVersion is not installed"
    }
    if (-not (Test-VisualStudioBuildTools)) {
        throw "Visual Studio 2022 C++ tools and Windows SDK 26100 were not found"
    }

    $feature = Get-WindowsOptionalFeature `
        -Online `
        -FeatureName HypervisorPlatform
    if ($feature.State -ne "Enabled") {
        throw "Windows Hypervisor Platform is not enabled"
    }

    Push-Location $Workspace
    try {
        Invoke-Native $python @("scripts\nvx.py", "verify")
        if ($RequireBuild) {
            Assert-GuestArtifactSet "$Workspace\build" (Get-CurrentRevision)
            if (-not (Test-Path `
                        -LiteralPath "$Workspace\openvmm\target\release\openvmm.exe" `
                        -PathType Leaf)) {
                throw "missing OpenVMM release binary"
            }
            Invoke-Native $python @(
                "scripts\nvx.py", "run",
                "--hypervisor", "whp",
                "--dry-run"
            )
        }
    }
    finally {
        Pop-Location
    }
}

Assert-SupportedHost
if ($CheckOnly) {
    if (-not [string]::IsNullOrWhiteSpace($GuestArtifactsDirectory)) {
        Assert-GuestArtifactSet $GuestArtifactsDirectory (Get-CurrentRevision)
    }
    Assert-Environment -RequireBuild:(-not $SkipBuild)
    Write-Output "NVX_SETUP_CHECK=ok"
    exit 0
}

Assert-Administrator
Install-Toolchain
Copy-GuestArtifacts
if (-not $SkipBuild) {
    Build-Nvx
}
$restartNeeded = Enable-Whp

if ($restartNeeded) {
    Write-Output "NVX_SETUP_REBOOT_REQUIRED=1"
    Write-Output "Restart Windows, then rerun this script with -CheckOnly."
    exit 3010
}

if (-not $SkipBuild -and
    [string]::IsNullOrWhiteSpace($GuestArtifactsDirectory) -and
    -not (Test-Path -LiteralPath "$Workspace\build\vmlinux" -PathType Leaf)) {
    Write-Output "NVX_GUEST_ARTIFACTS_REQUIRED=1"
    Write-Output "Stage a same-revision guest bundle and rerun with -GuestArtifactsDirectory."
    exit 21
}

Assert-Environment -RequireBuild:(-not $SkipBuild)
Write-Output "NVX_SETUP_COMPLETE=1"
