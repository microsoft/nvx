[CmdletBinding()]
param(
    [Parameter()]
    [string]$Workspace,

    [Parameter()]
    [string]$GuestArtifactsDirectory,

    [Parameter()]
    [switch]$CheckOnly,

    [Parameter()]
    [switch]$SkipBuild,

    [Parameter()]
    [switch]$RunnerOnly,

    [Parameter()]
    [string]$RunnerName,

    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$RepositoryUrl = "https://github.com/microsoft/nvx",

    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$RunnerDirectory = "$env:SystemDrive\actions-runner",

    [Parameter()]
    [string]$BenchmarkScratchDirectory,

    [Parameter()]
    [switch]$RunnerTokenStdin
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

if (-not $RunnerOnly) {
    if ([string]::IsNullOrWhiteSpace($Workspace)) {
        $Workspace = Join-Path $PSScriptRoot "..\.."
    }
    $Workspace = (Resolve-Path -LiteralPath $Workspace).Path
}

$RustToolchain = "stable"
$MinimumRustVersion = [version]"1.95.0"
$RustupVersion = "1.29.1"
$RustupSha256 = "6f4bef66261261fcb43131be8720bab817d403a09edec7455c371974b90bdb7e"
$CargoNextestVersion = "0.9.133"
$SccacheVersion = "0.18.0"
$SccacheSha256 = "1a63c1be2beab3f04d27e4cc145443e092e02d3dd83a51030989829d7023091b"
$RunnerVersion = "2.337.0"
$RunnerSha256 = "1150692afa94e71f872017e254ea55b6eece1eece3fe7e3a6d4c93d0a1b85cfc"
$ToolRoot = Join-Path $env:ProgramData "nvx"
$TrustedCargoHome = Join-Path $ToolRoot "cargo"
$CargoHome = Join-Path $RunnerDirectory "_work\_temp\cargo-home"
$SccacheDirectory = Join-Path $RunnerDirectory "_work\_sccache"
$BenchmarkScratchVariable = "NVX_BENCHMARK_SCRATCH"
$BenchmarkScratchName = "nvx-benchmark-scratch"
$RustupHome = Join-Path $ToolRoot "rustup"
$RequiredGuestArtifacts = @(
    "vmlinux",
    "vmlinux.config",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.packages.json",
    "initramfs-ubuntu.cpio.gz",
    "initramfs-ubuntu.cpio.gz.packages.json",
    "ubuntu-distro.erofs",
    "ubuntu-distro.erofs.manifest.json"
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
    $env:Path = "$TrustedCargoHome\bin;$machinePath"
}

function Add-MachinePathEntry {
    param([Parameter(Mandatory = $true)][string]$Path)
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $entries = @($machinePath -split ";" |
        Where-Object { $_ -and $_ -ne $Path })
    [Environment]::SetEnvironmentVariable(
        "Path",
        ((@($Path) + $entries) -join ";"),
        "Machine"
    )
}

function Set-ServiceDirectoryAcl {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ServiceRights,
        [Parameter()][string[]]$ExcludeChildren = @(),
        [Parameter()][switch]$AllowInternalLinks,
        [Parameter()][switch]$SkipChildren
    )
    $rootItem = Get-Item -LiteralPath $Path -Force
    if ($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "directory tree must not contain a reparse point: $($rootItem.FullName)"
    }
    $entries = @()
    if (-not $SkipChildren) {
        $entries = @(Get-ChildItem -LiteralPath $Path -Force)
        foreach ($entry in $entries) {
            if ($entry.Name -notin $ExcludeChildren) {
                if ($AllowInternalLinks) {
                    Assert-TreeLinksAreInternal `
                        -Root $entry.FullName `
                        -Boundary $Path
                }
                else {
                    Assert-NoTreeLinks -Root $entry.FullName
                }
            }
        }
    }
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $inheritance = [Security.AccessControl.InheritanceFlags]::ObjectInherit -bor
    [Security.AccessControl.InheritanceFlags]::ContainerInherit
    $propagation = [Security.AccessControl.PropagationFlags]::None
    $allow = [Security.AccessControl.AccessControlType]::Allow
    foreach ($entry in @(
            @("S-1-5-18", "FullControl"),
            @("S-1-5-32-544", "FullControl"),
            @("S-1-5-20", $ServiceRights)
        )) {
        $identity = [Security.Principal.SecurityIdentifier]::new($entry[0])
        $rights = [Security.AccessControl.FileSystemRights]$entry[1]
        $rule = [Security.AccessControl.FileSystemAccessRule]::new(
            $identity,
            $rights,
            $inheritance,
            $propagation,
            $allow
        )
        [void]$acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
    foreach ($entry in $entries) {
        if ($entry.Name -in $ExcludeChildren) {
            continue
        }
        $arguments = @($entry.FullName, "/reset", "/L", "/Q")
        if ($entry.PSIsContainer) {
            $arguments += "/T"
        }
        Invoke-Native "icacls.exe" $arguments
    }
}

function Assert-ServiceDirectoryAcl {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter()][switch]$Writable
    )
    $acl = Get-Acl -LiteralPath $Path
    if (-not $acl.AreAccessRulesProtected) {
        throw "service directory ACL inherits permissions: $Path"
    }
    $networkServiceSid = "S-1-5-20"
    $allRules = @($acl.GetAccessRules(
            $true,
            $true,
            [Security.Principal.SecurityIdentifier]
        ))
    $rules = @($allRules | Where-Object {
            $_.IdentityReference.Value -eq $networkServiceSid -and
            $_.AccessControlType -eq "Allow"
        })
    $required = if ($Writable) {
        [Security.AccessControl.FileSystemRights]::Modify
    }
    else {
        [Security.AccessControl.FileSystemRights]::ReadAndExecute
    }
    if (@($rules | Where-Object {
                ($_.FileSystemRights -band $required) -eq $required
            }).Count -eq 0) {
        throw "Network Service lacks $required on $Path"
    }
    $writeMask = [Security.AccessControl.FileSystemRights]::Write -bor
    [Security.AccessControl.FileSystemRights]::Delete -bor
    [Security.AccessControl.FileSystemRights]::ChangePermissions -bor
    [Security.AccessControl.FileSystemRights]::TakeOwnership
    $allowedWriters = @("S-1-5-18", "S-1-5-32-544")
    if ($Writable) {
        $allowedWriters += $networkServiceSid
    }
    if (@($allRules | Where-Object {
                $_.AccessControlType -eq "Allow" -and
                ($_.FileSystemRights -band $writeMask) -ne 0 -and
                $_.IdentityReference.Value -notin $allowedWriters
            }).Count -ne 0) {
        throw "untrusted identity can modify service directory: $Path"
    }
}

function Get-TreeItemsWithoutFollowingLinks {
    param([Parameter(Mandatory = $true)][string]$Root)
    $pending = [Collections.Generic.Stack[IO.FileSystemInfo]]::new()
    foreach ($entry in Get-ChildItem -LiteralPath $Root -Force) {
        $pending.Push($entry)
    }
    while ($pending.Count -ne 0) {
        $entry = $pending.Pop()
        $entry
        if ($entry.PSIsContainer -and
            -not ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            foreach ($child in Get-ChildItem -LiteralPath $entry.FullName -Force) {
                $pending.Push($child)
            }
        }
    }
}

function Assert-NoTreeLinks {
    param([Parameter(Mandatory = $true)][string]$Root)
    $rootItem = Get-Item -LiteralPath $Root -Force
    if ($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "directory tree must not contain a reparse point: $($rootItem.FullName)"
    }
    $linkedItems = @(Get-TreeItemsWithoutFollowingLinks -Root $Root |
        Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint -or
            $_.LinkType -eq "HardLink"
        })
    if ($linkedItems.Count -ne 0) {
        throw "directory tree must not contain a link: $($linkedItems[0].FullName)"
    }
}

function Assert-TreeLinksAreInternal {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Boundary
    )
    $rootItem = Get-Item -LiteralPath $Root -Force
    $items = @($rootItem)
    if (-not ($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -and
        $rootItem.PSIsContainer) {
        $items += @(Get-TreeItemsWithoutFollowingLinks -Root $Root)
    }
    $hardLinks = @($items | Where-Object { $_.LinkType -eq "HardLink" })
    if ($hardLinks.Count -ne 0) {
        throw "trusted tree must not contain a hard link: $($hardLinks[0].FullName)"
    }
    $boundaryPath = [IO.Path]::GetFullPath($Boundary).TrimEnd("\")
    $boundaryPrefix = "$boundaryPath\"
    foreach ($item in $items | Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint
        }) {
        $target = @($item.Target)[0]
        if ([string]::IsNullOrWhiteSpace($target)) {
            throw "reparse point has no target: $($item.FullName)"
        }
        if (-not [IO.Path]::IsPathRooted($target)) {
            $target = Join-Path `
                ([IO.Path]::GetDirectoryName($item.FullName)) `
                $target
        }
        $targetPath = [IO.Path]::GetFullPath($target)
        $insideBoundary = [StringComparer]::OrdinalIgnoreCase.Equals(
            $targetPath,
            $boundaryPath
        ) -or $targetPath.StartsWith(
            $boundaryPrefix,
            [StringComparison]::OrdinalIgnoreCase
        )
        $targetItem = Get-Item `
            -LiteralPath $targetPath `
            -Force `
            -ErrorAction SilentlyContinue
        if (-not $insideBoundary -or $null -eq $targetItem -or
            $targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "trusted tree link has unsafe target: $($item.FullName)"
        }
    }
}

function Assert-ActionsRunnerWritablePaths {
    $workDirectory = Join-Path $RunnerDirectory "_work"
    foreach ($path in @(
            $RunnerDirectory,
            $workDirectory,
            (Join-Path $RunnerDirectory "_work\_temp"),
            (Join-Path $RunnerDirectory "_work\_diag"),
            $CargoHome,
            $SccacheDirectory
        )) {
        $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
        if ($null -eq $item) {
            continue
        }
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "runner writable path must not be a reparse point: $path"
        }
        if (-not $item.PSIsContainer) {
            throw "runner writable path is not a directory: $path"
        }
    }
}

function Assert-ActionsRunnerStatePaths {
    Assert-ActionsRunnerWritablePaths
    foreach ($name in @(
            ".credentials",
            ".credentials_rsaparams",
            ".nvx-labels",
            ".runner",
            ".service"
        )) {
        $path = Join-Path $RunnerDirectory $name
        $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
        if ($null -eq $item) {
            continue
        }
        if ($item.PSIsContainer -or
            $item.Attributes -band [IO.FileAttributes]::ReparsePoint -or
            $item.LinkType -eq "HardLink") {
            throw "runner state path is not a regular file: $path"
        }
    }
}

function Get-ProtectedActionsRunnerItems {
    Get-Item -LiteralPath $RunnerDirectory -Force
    foreach ($entry in Get-ChildItem -LiteralPath $RunnerDirectory -Force) {
        if ($entry.Name -in @("_work", "_diag")) {
            continue
        }
        $entry
        if ($entry.PSIsContainer -and
            -not ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            Get-TreeItemsWithoutFollowingLinks -Root $entry.FullName
        }
    }
}

function Assert-NoProtectedActionsRunnerLinks {
    $linkedItems = @(Get-ProtectedActionsRunnerItems | Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint -or
            $_.LinkType -eq "HardLink"
        })
    if ($linkedItems.Count -ne 0) {
        throw "protected runner path must not be a link: $($linkedItems[0].FullName)"
    }
}

function Set-ActionsRunnerOwners {
    Assert-NoProtectedActionsRunnerLinks
    Invoke-Native "icacls.exe" @(
        $RunnerDirectory, "/setowner", "*S-1-5-18", "/L", "/Q"
    )
    foreach ($entry in Get-ChildItem -LiteralPath $RunnerDirectory -Force) {
        if ($entry.Name -in @("_work", "_diag")) {
            continue
        }
        Invoke-Native "icacls.exe" @(
            $entry.FullName, "/setowner", "*S-1-5-18", "/T", "/L", "/Q"
        )
    }
}

function Assert-ActionsRunnerOwners {
    Assert-NoProtectedActionsRunnerLinks
    foreach ($item in Get-ProtectedActionsRunnerItems) {
        $owner = (Get-Acl -LiteralPath $item.FullName).GetOwner(
            [Security.Principal.SecurityIdentifier]
        ).Value
        if ($owner -ne "S-1-5-18") {
            throw "protected runner path is owned by ${owner}: $($item.FullName)"
        }
    }
}

function Assert-TrustedToolchainAcl {
    Assert-TreeLinksAreInternal -Root $ToolRoot -Boundary $ToolRoot
    Assert-ServiceDirectoryAcl -Path $ToolRoot
}

function Set-ActionsRunnerDisableUpdate {
    Assert-ActionsRunnerStatePaths
    $runnerConfiguration = Join-Path $RunnerDirectory ".runner"
    $configuration = Get-Content -LiteralPath $runnerConfiguration -Raw |
    ConvertFrom-Json
    $configuration | Add-Member `
        -NotePropertyName disableUpdate `
        -NotePropertyValue $true `
        -Force
    $encoding = [Text.UTF8Encoding]::new($true)
    $json = $configuration | ConvertTo-Json -Depth 16
    $bytes = $encoding.GetBytes($json)
    $stream = [IO.File]::Open(
        $runnerConfiguration,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Write,
        [IO.FileShare]::Read
    )
    try {
        $stream.SetLength(0)
        $stream.Write($bytes, 0, $bytes.Length)
    }
    finally {
        $stream.Dispose()
    }
}

function Test-ActionsRunnerDiagnosticsLink {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedTarget
    )
    $item = Get-Item `
        -LiteralPath $Path `
        -Force `
        -ErrorAction SilentlyContinue
    if ($null -eq $item) {
        return $false
    }
    if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        return $false
    }
    $target = @($item.Target)[0]
    if ([string]::IsNullOrWhiteSpace($target)) {
        return $false
    }
    if (-not [IO.Path]::IsPathRooted($target)) {
        $target = Join-Path $item.Parent.FullName $target
    }
    $actual = [IO.Path]::GetFullPath($target).TrimEnd("\")
    $expected = [IO.Path]::GetFullPath($ExpectedTarget).TrimEnd("\")
    return [StringComparer]::OrdinalIgnoreCase.Equals($actual, $expected)
}

function Protect-ActionsRunner {
    $workDirectory = Join-Path $RunnerDirectory "_work"
    $temporaryDirectory = Join-Path $workDirectory "_temp"
    $diagnosticsDirectory = Join-Path $RunnerDirectory "_diag"
    $diagnosticsTarget = Join-Path $workDirectory "_diag"
    Assert-ActionsRunnerWritablePaths
    New-Item `
        -ItemType Directory `
        -Path `
        $workDirectory, `
        $temporaryDirectory, `
        $diagnosticsTarget, `
        $CargoHome, `
        $SccacheDirectory `
        -Force |
    Out-Null

    $diagnostics = Get-Item `
        -LiteralPath $diagnosticsDirectory `
        -Force `
        -ErrorAction SilentlyContinue
    if ($null -ne $diagnostics -and
        -not (Test-ActionsRunnerDiagnosticsLink `
            -Path $diagnosticsDirectory `
            -ExpectedTarget $diagnosticsTarget)) {
        if ($diagnostics.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            Remove-Item -LiteralPath $diagnosticsDirectory -Force
        }
        else {
            Get-ChildItem -LiteralPath $diagnosticsDirectory -Force |
            Move-Item -Destination $diagnosticsTarget -Force
            Remove-Item -LiteralPath $diagnosticsDirectory -Recurse -Force
        }
    }
    if (-not (Test-Path -LiteralPath $diagnosticsDirectory)) {
        New-Item `
            -ItemType Junction `
            -Path $diagnosticsDirectory `
            -Target $diagnosticsTarget |
        Out-Null
    }

    Set-ActionsRunnerOwners
    Set-ServiceDirectoryAcl `
        -Path $RunnerDirectory `
        -ServiceRights "ReadAndExecute" `
        -ExcludeChildren @("_work", "_diag")
    foreach ($path in @(
            $workDirectory,
            $temporaryDirectory,
            $diagnosticsTarget,
            $CargoHome,
            $SccacheDirectory
        )) {
        Set-ServiceDirectoryAcl `
            -Path $path `
            -ServiceRights "Modify" `
            -SkipChildren
    }
    Invoke-Native "icacls.exe" @(
        $diagnosticsDirectory, "/reset", "/L", "/Q"
    )
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

function Get-WinGetCommand {
    $command = Get-Command winget.exe -ErrorAction SilentlyContinue |
    Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }

    $packages = @(Get-AppxPackage `
            -AllUsers `
            -Name Microsoft.DesktopAppInstaller `
            -ErrorAction SilentlyContinue |
        Sort-Object Version -Descending)
    foreach ($package in $packages) {
        $path = Join-Path $package.InstallLocation "winget.exe"
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            return $path
        }
    }
    return $null
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
    param([Parameter()][switch]$SkipWorkspace)
    if ($env:OS -ne "Windows_NT") {
        throw "this script requires Windows"
    }
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw "this script requires 64-bit Windows"
    }
    if ($SkipWorkspace) {
        return
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
    $winget = Get-WinGetCommand
    if ($null -ne $winget) {
        return $winget
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
    $winget = Get-WinGetCommand
    if ($null -eq $winget) {
        throw "required command not found: winget.exe"
    }
    Update-ProcessPath
    return $winget
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
    Assert-ActionsRunnerWritablePaths
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

    New-Item -ItemType Directory `
        -Path $TrustedCargoHome, $RustupHome `
        -Force |
    Out-Null
    Set-ServiceDirectoryAcl `
        -Path $ToolRoot `
        -ServiceRights "ReadAndExecute" `
        -AllowInternalLinks
    [Environment]::SetEnvironmentVariable("CARGO_HOME", $CargoHome, "Machine")
    [Environment]::SetEnvironmentVariable("RUSTUP_HOME", $RustupHome, "Machine")
    Add-MachinePathEntry "$TrustedCargoHome\bin"
    $env:CARGO_HOME = $TrustedCargoHome
    $env:RUSTUP_HOME = $RustupHome
    Update-ProcessPath

    $rustupPath = Join-Path $TrustedCargoHome "bin\rustup.exe"
    if (-not (Test-Path -LiteralPath $rustupPath -PathType Leaf)) {
        $rustupInstaller = Join-Path $ToolRoot `
            "rustup-init-$RustupVersion-$([guid]::NewGuid().ToString('N')).exe"
        try {
            Invoke-WebRequest `
                -UseBasicParsing `
                -Uri "https://static.rust-lang.org/rustup/archive/$RustupVersion/x86_64-pc-windows-msvc/rustup-init.exe" `
                -OutFile $rustupInstaller
            $actualHash = (Get-FileHash `
                    -LiteralPath $rustupInstaller `
                    -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actualHash -ne $RustupSha256) {
                throw "Rustup installer checksum mismatch: $actualHash"
            }
            Invoke-Native $rustupInstaller @(
                "-y",
                "--profile", "minimal",
                "--default-toolchain", $RustToolchain
            )
        }
        finally {
            Remove-Item `
                -LiteralPath $rustupInstaller `
                -Force `
                -ErrorAction SilentlyContinue
        }
    }
    Update-ProcessPath
    Invoke-Native $rustupPath @(
        "toolchain", "install", $RustToolchain, "--profile", "minimal"
    )
    Invoke-Native $rustupPath @(
        "target", "add",
        "x86_64-unknown-none",
        "x86_64-unknown-uefi",
        "--toolchain", $RustToolchain
    )

    $cargo = Join-Path $TrustedCargoHome "bin\cargo.exe"
    $nextest = Join-Path $TrustedCargoHome "bin\cargo-nextest.exe"
    $installNextest = -not (Test-Path -LiteralPath $nextest -PathType Leaf)
    if (-not $installNextest) {
        $nextestVersion = & $nextest --version
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

    $sccache = Join-Path $TrustedCargoHome "bin\sccache.exe"
    $installSccache = -not (Test-Path -LiteralPath $sccache -PathType Leaf)
    if (-not $installSccache) {
        $installedSccacheVersion = & $sccache --version
        Assert-LastExitCode "sccache --version"
        $installSccache = ($installedSccacheVersion -join "`n") -notmatch `
            "sccache $([regex]::Escape($SccacheVersion))"
    }
    if ($installSccache) {
        $archive = Join-Path $ToolRoot `
            "sccache-v$SccacheVersion-x86_64-pc-windows-msvc-$([guid]::NewGuid().ToString('N')).tar.gz"
        $extractDirectory = Join-Path $ToolRoot `
            "sccache-v$SccacheVersion-$([guid]::NewGuid().ToString('N'))"
        try {
            Invoke-WebRequest `
                -UseBasicParsing `
                -Uri "https://github.com/mozilla/sccache/releases/download/v$SccacheVersion/sccache-v$SccacheVersion-x86_64-pc-windows-msvc.tar.gz" `
                -OutFile $archive
            $actualHash = (Get-FileHash `
                    -LiteralPath $archive `
                    -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actualHash -ne $SccacheSha256) {
                throw "sccache archive checksum mismatch: $actualHash"
            }
            New-Item `
                -ItemType Directory `
                -Path $extractDirectory `
                -Force |
            Out-Null
            Invoke-Native (Get-RequiredCommand "tar.exe") @(
                "-xzf", $archive, "-C", $extractDirectory
            )
            $extracted = Join-Path `
                $extractDirectory `
                "sccache-v$SccacheVersion-x86_64-pc-windows-msvc\sccache.exe"
            if (-not (Test-Path -LiteralPath $extracted -PathType Leaf)) {
                throw "sccache executable was not found after extraction"
            }
            Copy-Item -LiteralPath $extracted -Destination $sccache -Force
        }
        finally {
            Remove-Item `
                -LiteralPath $archive, $extractDirectory `
                -Recurse `
                -Force `
                -ErrorAction SilentlyContinue
        }
    }

    Set-ServiceDirectoryAcl `
        -Path $ToolRoot `
        -ServiceRights "ReadAndExecute" `
        -AllowInternalLinks
    $env:CARGO_HOME = $CargoHome
}

function Configure-SccacheEnvironment {
    New-Item -ItemType Directory -Path $SccacheDirectory -Force |
    Out-Null
    Set-ServiceDirectoryAcl `
        -Path $SccacheDirectory `
        -ServiceRights "Modify" `
        -SkipChildren
    foreach ($entry in @{
            CARGO_INCREMENTAL   = "0"
            RUSTC_WRAPPER       = "sccache"
            SCCACHE_CACHE_SIZE  = "10G"
            SCCACHE_DIR         = $SccacheDirectory
        }.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable(
            $entry.Key,
            $entry.Value,
            "Machine"
        )
        [Environment]::SetEnvironmentVariable(
            $entry.Key,
            $entry.Value,
            "Process"
        )
    }
}

function Test-SystemVolumePath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $root = [IO.Path]::GetPathRoot([IO.Path]::GetFullPath($Path)).TrimEnd("\")
    return [StringComparer]::OrdinalIgnoreCase.Equals(
        $root,
        $env:SystemDrive.TrimEnd("\")
    )
}

function Get-BenchmarkScratchDirectory {
    if (-not [string]::IsNullOrWhiteSpace($BenchmarkScratchDirectory)) {
        return [IO.Path]::GetFullPath($BenchmarkScratchDirectory)
    }
    $configured = [Environment]::GetEnvironmentVariable(
        $BenchmarkScratchVariable,
        "Machine"
    )
    if (-not [string]::IsNullOrWhiteSpace($configured)) {
        return $configured
    }
    # Snapshot capture flushes guest RAM through benchmark scratch. Prefer the
    # largest data volume so that I/O avoids the system disk's build traffic
    # and burst throttling.
    $volume = @(Get-Volume | Where-Object {
            $_.DriveType -eq "Fixed" -and
            $_.FileSystem -eq "NTFS" -and
            "$($_.DriveLetter)" -match "^[A-Za-z]$" -and
            -not (Test-SystemVolumePath "$($_.DriveLetter):\")
        } | Sort-Object `
            -Property @{ Expression = "Size"; Descending = $true }, DriveLetter) |
    Select-Object -First 1
    if ($null -eq $volume) {
        return $null
    }
    return "$($volume.DriveLetter):\$BenchmarkScratchName"
}

function Configure-BenchmarkScratch {
    $directory = Get-BenchmarkScratchDirectory
    if ($null -eq $directory) {
        Write-Output "No data volume found; benchmarks use the system temporary directory."
        return
    }
    if (Test-SystemVolumePath $directory) {
        throw "benchmark scratch must not use the system volume: $directory"
    }
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    Set-ServiceDirectoryAcl `
        -Path $directory `
        -ServiceRights "Modify" `
        -SkipChildren
    foreach ($target in @("Machine", "Process")) {
        [Environment]::SetEnvironmentVariable(
            $BenchmarkScratchVariable,
            $directory,
            $target
        )
    }
}

function Assert-BenchmarkScratch {
    $configured = [Environment]::GetEnvironmentVariable(
        $BenchmarkScratchVariable,
        "Machine"
    )
    if ([string]::IsNullOrWhiteSpace($configured)) {
        if ($null -ne (Get-BenchmarkScratchDirectory)) {
            throw "machine $BenchmarkScratchVariable is not configured"
        }
        return
    }
    if (-not [string]::IsNullOrWhiteSpace($BenchmarkScratchDirectory) -and
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            $configured,
            [IO.Path]::GetFullPath($BenchmarkScratchDirectory)
        )) {
        throw "machine $BenchmarkScratchVariable is $configured, expected $BenchmarkScratchDirectory"
    }
    if (Test-SystemVolumePath $configured) {
        throw "benchmark scratch must not use the system volume: $configured"
    }
    $item = Get-Item -LiteralPath $configured -Force -ErrorAction SilentlyContinue
    if ($null -eq $item -or -not $item.PSIsContainer) {
        throw "benchmark scratch directory was not found: $configured"
    }
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "benchmark scratch directory must not be a reparse point: $configured"
    }
    Assert-ServiceDirectoryAcl -Path $configured -Writable
}

function Get-RelativePackageFiles {
    param([Parameter(Mandatory = $true)][string]$Root)
    $prefixLength = $Root.TrimEnd("\").Length + 1
    return @(Get-TreeItemsWithoutFollowingLinks -Root $Root |
        Where-Object { -not $_.PSIsContainer } | ForEach-Object {
            $_.FullName.Substring($prefixLength)
        })
}

function Assert-RunnerPackage {
    param(
        [Parameter(Mandatory = $true)][string]$ExpectedRoot,
        [Parameter(Mandatory = $true)][string]$ActualRoot
    )
    $expectedEntries = @(Get-ChildItem -LiteralPath $ExpectedRoot -Force |
        Select-Object -ExpandProperty Name)
    $allowedState = @(
        ".credentials",
        ".credentials_rsaparams",
        ".nvx-labels",
        ".runner",
        ".service",
        "_diag",
        "_work"
    )
    $unexpectedEntries = @(Get-ChildItem -LiteralPath $ActualRoot -Force |
        Where-Object {
            $_.Name -notin $expectedEntries -and $_.Name -notin $allowedState
        })
    if ($unexpectedEntries.Count -ne 0) {
        throw "installed runner package has unexpected entries: $($unexpectedEntries.Name -join ', ')"
    }
    foreach ($entry in Get-ChildItem -LiteralPath $ExpectedRoot -Force) {
        $actualEntry = Join-Path $ActualRoot $entry.Name
        $actualItem = Get-Item `
            -LiteralPath $actualEntry `
            -Force `
            -ErrorAction SilentlyContinue
        if ($null -eq $actualItem) {
            throw "installed runner package is missing entry: $($entry.Name)"
        }
        $linkedItems = @($actualItem | Where-Object {
                $_.Attributes -band [IO.FileAttributes]::ReparsePoint -or
                $_.LinkType -eq "HardLink"
            })
        if ($actualItem.PSIsContainer -and $linkedItems.Count -eq 0) {
            $linkedItems += @(Get-TreeItemsWithoutFollowingLinks `
                    -Root $actualEntry | Where-Object {
                        $_.Attributes -band [IO.FileAttributes]::ReparsePoint -or
                        $_.LinkType -eq "HardLink"
                    })
        }
        if ($linkedItems.Count -ne 0) {
            throw "installed runner package contains a link: $($linkedItems[0].FullName)"
        }
        if ($entry.PSIsContainer) {
            if (-not (Test-Path -LiteralPath $actualEntry -PathType Container)) {
                throw "installed runner package is missing directory: $($entry.Name)"
            }
            $expectedFiles = @(Get-RelativePackageFiles $entry.FullName | Sort-Object)
            $actualFiles = @(Get-RelativePackageFiles $actualEntry | Sort-Object)
            if (@(Compare-Object $expectedFiles $actualFiles).Count -ne 0) {
                throw "installed runner package has unexpected files: $($entry.Name)"
            }
        }
    }
    foreach ($expected in Get-ChildItem -LiteralPath $ExpectedRoot -Recurse -File) {
        $relative = $expected.FullName.Substring($ExpectedRoot.TrimEnd("\").Length + 1)
        $actual = Join-Path $ActualRoot $relative
        if (-not (Test-Path -LiteralPath $actual -PathType Leaf)) {
            throw "installed runner package is missing file: $relative"
        }
        if ($expected.Length -ne (Get-Item -LiteralPath $actual).Length -or
            (Get-FileHash -LiteralPath $expected.FullName -Algorithm SHA256).Hash -ne
            (Get-FileHash -LiteralPath $actual -Algorithm SHA256).Hash) {
            throw "installed runner package does not match ${RunnerVersion}: $relative"
        }
    }
}

function Install-ActionsRunner {
    param([Parameter()][string]$Token)
    $archiveName = "actions-runner-win-x64-$RunnerVersion.zip"
    $downloadUrl = "https://github.com/actions/runner/releases/download/v$RunnerVersion/$archiveName"

    New-Item -ItemType Directory -Path $RunnerDirectory -Force | Out-Null
    $packageDirectory = Join-Path $ToolRoot `
        "runner-package-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $packageDirectory | Out-Null
    try {
        $archivePath = Join-Path $packageDirectory $archiveName
        $packageRoot = Join-Path $packageDirectory "root"
        Invoke-WebRequest -UseBasicParsing -Uri $downloadUrl -OutFile $archivePath
        $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
        if ($actualHash -ne $RunnerSha256) {
            throw "Actions runner checksum mismatch: $actualHash"
        }
        Expand-Archive -LiteralPath $archivePath -DestinationPath $packageRoot

        $listener = Join-Path $RunnerDirectory "bin\Runner.Listener.exe"
        if (-not (Test-Path -LiteralPath $listener -PathType Leaf)) {
            if (@(Get-ChildItem -LiteralPath $RunnerDirectory -Force).Count -ne 0) {
                throw "refusing to install into partial runner directory: $RunnerDirectory"
            }
            Copy-Item `
                -Path (Join-Path $packageRoot "*") `
                -Destination $RunnerDirectory `
                -Recurse `
                -Force
        }
        Assert-RunnerPackage `
            -ExpectedRoot $packageRoot `
            -ActualRoot $RunnerDirectory
        Assert-ActionsRunnerStatePaths
    }
    finally {
        Remove-Item `
            -LiteralPath $packageDirectory `
            -Recurse `
            -Force `
            -ErrorAction SilentlyContinue
    }
    $listener = Join-Path $RunnerDirectory "bin\Runner.Listener.exe"

    $runnerConfiguration = Join-Path $RunnerDirectory ".runner"
    $serviceFile = Join-Path $RunnerDirectory ".service"
    $labelsFile = Join-Path $RunnerDirectory ".nvx-labels"
    $labels = "windows,whp,virtual-machine"
    $runnerNameValidated = $false
    if (Test-Path -LiteralPath $runnerConfiguration -PathType Leaf) {
        $configuration = Get-Content -LiteralPath $runnerConfiguration -Raw |
        ConvertFrom-Json
        $runnerNameValidated = $configuration.agentName -eq $RunnerName
    }
    $serviceInstalled = $false
    if (Test-Path -LiteralPath $serviceFile -PathType Leaf) {
        $serviceName = (Get-Content -LiteralPath $serviceFile -Raw).Trim()
        $serviceInstalled = -not [string]::IsNullOrWhiteSpace($serviceName) -and
        $null -ne (Get-Service -Name $serviceName -ErrorAction SilentlyContinue)
        if ($serviceInstalled) {
            Assert-ActionsRunnerServicePath -ServiceName $serviceName
        }
    }
    $labelsValidated = (Test-Path -LiteralPath $labelsFile -PathType Leaf) -and
    (Get-Content -LiteralPath $labelsFile -Raw).Trim() -eq $labels
    $registrationRequired = `
        -not (Test-Path -LiteralPath $runnerConfiguration -PathType Leaf) -or
    -not $runnerNameValidated -or
    -not $serviceInstalled -or
    -not $labelsValidated
    if ($registrationRequired) {
        if ([string]::IsNullOrWhiteSpace($Token)) {
            throw "runner registration token is required to configure service and labels"
        }
        if ($serviceInstalled) {
            Remove-ActionsRunnerService -ServiceName $serviceName
        }
        foreach ($name in @(
                ".runner",
                ".credentials",
                ".credentials_rsaparams",
                ".service",
                ".nvx-labels"
            )) {
            Remove-Item `
                -LiteralPath (Join-Path $RunnerDirectory $name) `
                -Force `
                -ErrorAction SilentlyContinue
        }
    }
    if (-not (Test-Path -LiteralPath $runnerConfiguration -PathType Leaf)) {
        Push-Location $RunnerDirectory
        try {
            Invoke-Native ".\config.cmd" @(
                "--unattended", "--replace",
                "--url", $RepositoryUrl,
                "--token", $Token,
                "--name", $RunnerName,
                "--labels", $labels,
                "--work", "_work",
                "--disableupdate",
                "--runasservice",
                "--windowslogonaccount", "NT AUTHORITY\NETWORK SERVICE"
            )
        }
        finally {
            Pop-Location
        }
        [IO.File]::WriteAllText(
            $labelsFile,
            $labels,
            [Text.UTF8Encoding]::new($false)
        )
    }

    $service = Get-ActionsRunnerService
    if ($service.Status -ne "Stopped") {
        Stop-Service -Name $service.Name -Force
    }
    Set-ActionsRunnerServiceAccount -ServiceName $service.Name
    Assert-ActionsRunnerServiceAccount -ServiceName $service.Name
    Protect-ActionsRunner
    Set-ActionsRunnerDisableUpdate
    Start-Service -Name $service.Name
}

function Get-ActionsRunnerService {
    $serviceFile = Join-Path $RunnerDirectory ".service"
    if (-not (Test-Path -LiteralPath $serviceFile -PathType Leaf)) {
        throw "Actions runner service file was not found: $serviceFile"
    }
    $serviceName = (Get-Content -LiteralPath $serviceFile -Raw).Trim()
    if ([string]::IsNullOrWhiteSpace($serviceName)) {
        throw "Actions runner service file is empty: $serviceFile"
    }
    Assert-ActionsRunnerServicePath -ServiceName $serviceName
    return Get-Service -Name $serviceName -ErrorAction Stop
}

function Assert-ActionsRunnerServicePath {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    if ($ServiceName -notmatch '^actions\.runner\.[A-Za-z0-9_.-]+$') {
        throw "invalid Actions runner service name: $ServiceName"
    }
    $service = Get-CimInstance `
        -ClassName Win32_Service `
        -Filter "Name='$ServiceName'"
    if ($null -eq $service) {
        throw "Actions runner service was not found: $ServiceName"
    }
    $expectedPath = Join-Path $RunnerDirectory "bin\RunnerService.exe"
    $actualPath = $service.PathName.Trim().Trim('"')
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals(
            $actualPath,
            $expectedPath
        )) {
        throw "Actions runner service does not belong to $RunnerDirectory"
    }
}

function Remove-ActionsRunnerService {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    Assert-ActionsRunnerServicePath -ServiceName $ServiceName
    $service = Get-Service -Name $ServiceName -ErrorAction Stop
    if ($service.Status -ne "Stopped") {
        Stop-Service -Name $ServiceName -Force
    }
    $service = Get-CimInstance `
        -ClassName Win32_Service `
        -Filter "Name='$ServiceName'"
    $result = Invoke-CimMethod -InputObject $service -MethodName Delete
    if ($result.ReturnValue -ne 0) {
        throw "could not remove runner service: Win32 error $($result.ReturnValue)"
    }
}

function Set-ActionsRunnerServiceAccount {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    $service = Get-CimInstance `
        -ClassName Win32_Service `
        -Filter "Name='$ServiceName'"
    if ($null -eq $service) {
        throw "Actions runner service was not found: $ServiceName"
    }
    $account = [Security.Principal.NTAccount]::new($service.StartName)
    $sid = $account.Translate([Security.Principal.SecurityIdentifier]).Value
    if ($sid -eq "S-1-5-20") {
        return
    }
    $result = Invoke-CimMethod `
        -InputObject $service `
        -MethodName Change `
        -Arguments @{
            StartName     = "NT AUTHORITY\NetworkService"
            StartPassword = $null
        }
    if ($result.ReturnValue -ne 0) {
        throw "could not set runner service account: Win32 error $($result.ReturnValue)"
    }
}

function Assert-ActionsRunnerServiceAccount {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    $service = Get-CimInstance `
        -ClassName Win32_Service `
        -Filter "Name='$ServiceName'"
    if ($null -eq $service) {
        throw "Actions runner service was not found: $ServiceName"
    }
    $account = [Security.Principal.NTAccount]::new($service.StartName)
    $sid = $account.Translate([Security.Principal.SecurityIdentifier]).Value
    if ($sid -ne "S-1-5-20") {
        throw "Actions runner service uses $($service.StartName), expected Network Service"
    }
}

function Assert-ActionsRunner {
    Assert-ActionsRunnerStatePaths
    $runnerConfiguration = Join-Path $RunnerDirectory ".runner"
    if (-not (Test-Path -LiteralPath $runnerConfiguration -PathType Leaf)) {
        throw "GitHub Actions runner is not configured"
    }
    $configuration = Get-Content -LiteralPath $runnerConfiguration -Raw |
    ConvertFrom-Json
    if ($configuration.agentName -ne $RunnerName) {
        throw "configured runner is $($configuration.agentName), expected $RunnerName"
    }
    if ($configuration.disableUpdate -ne $true) {
        throw "Actions runner automatic updates are not disabled"
    }
    if (-not [string]::IsNullOrWhiteSpace($RunnerName)) {
        $labelsFile = Join-Path $RunnerDirectory ".nvx-labels"
        $expectedLabels = "windows,whp,virtual-machine"
        if (-not (Test-Path -LiteralPath $labelsFile -PathType Leaf) -or
            (Get-Content -LiteralPath $labelsFile -Raw).Trim() -ne
            $expectedLabels) {
            throw "Actions runner labels are not validated"
        }
    }
    Assert-ServiceDirectoryAcl -Path $RunnerDirectory
    Assert-ActionsRunnerOwners
    Assert-ServiceDirectoryAcl `
        -Path (Join-Path $RunnerDirectory "_work") `
        -Writable
    if (-not (Test-ActionsRunnerDiagnosticsLink `
            -Path (Join-Path $RunnerDirectory "_diag") `
            -ExpectedTarget (Join-Path $RunnerDirectory "_work\_diag"))) {
        throw "Actions runner diagnostics are not stored under _work"
    }
    $listener = Join-Path $RunnerDirectory "bin\Runner.Listener.exe"
    Invoke-Native $listener @("--version")

    $service = Get-ActionsRunnerService
    Assert-ActionsRunnerServiceAccount -ServiceName $service.Name
    if ($service.Status -ne "Running") {
        throw "Actions runner service is not running"
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
    $features = @("HypervisorPlatform")
    if ($RunnerOnly) {
        $features += "Microsoft-Hyper-V"
    }
    $restartNeeded = $false
    foreach ($featureName in $features) {
        $feature = Get-WindowsOptionalFeature `
            -Online `
            -FeatureName $featureName
        if ($feature.State -eq "Enabled") {
            continue
        }
        $result = Enable-WindowsOptionalFeature `
            -Online `
            -FeatureName $featureName `
            -All `
            -NoRestart
        $restartNeeded = [bool]$result.RestartNeeded -or $restartNeeded
    }
    return $restartNeeded
}

function Assert-PcatFirmware {
    $system32 = Join-Path $env:SystemRoot "System32"
    $pcatFirmware = @(
        (Join-Path $system32 "vmfirmwarepcat.dll"),
        (Join-Path $system32 "vmfirmware.dll")
    )
    if (@($pcatFirmware | Where-Object {
                Test-Path -LiteralPath $_ -PathType Leaf
            }).Count -eq 0) {
        throw "Hyper-V PCAT firmware was not found under $system32"
    }
    $svgaFirmware = Join-Path $system32 "VmEmulatedDevices.dll"
    if (-not (Test-Path -LiteralPath $svgaFirmware -PathType Leaf)) {
        throw "Hyper-V SVGA firmware was not found: $svgaFirmware"
    }
}

function Assert-Environment {
    param(
        [Parameter()][switch]$RequireBuild,
        [Parameter()][switch]$SkipWorkspace
    )
    Update-ProcessPath
    $env:CARGO_HOME = $CargoHome
    $env:RUSTUP_HOME = $RustupHome
    if ([Environment]::GetEnvironmentVariable("CARGO_HOME", "Machine") -ne
        $CargoHome) {
        throw "machine CARGO_HOME does not use the per-job Cargo cache"
    }
    if ([Environment]::GetEnvironmentVariable("RUSTUP_HOME", "Machine") -ne
        $RustupHome) {
        throw "machine RUSTUP_HOME does not use the trusted Rust toolchain"
    }
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    if ($TrustedCargoHome + "\bin" -notin @($machinePath -split ";")) {
        throw "trusted Cargo bin directory is missing from the machine PATH"
    }
    Assert-TrustedToolchainAcl
    $python = Get-PythonCommand
    foreach ($command in @(
            "git.exe",
            "rustup.exe",
            "cargo.exe",
            "cargo-nextest.exe",
            "sccache.exe"
        )) {
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
    $installedSccacheVersion = & (Get-RequiredCommand "sccache.exe") --version
    Assert-LastExitCode "sccache --version"
    if (($installedSccacheVersion -join "`n") -notmatch `
            "sccache $([regex]::Escape($SccacheVersion))") {
        throw "sccache $SccacheVersion is not installed"
    }
    if ($RunnerOnly) {
        $expectedEnvironment = @{
            CARGO_INCREMENTAL  = "0"
            RUSTC_WRAPPER      = "sccache"
            SCCACHE_CACHE_SIZE = "10G"
            SCCACHE_DIR        = $SccacheDirectory
        }
        foreach ($entry in $expectedEnvironment.GetEnumerator()) {
            if ([Environment]::GetEnvironmentVariable(
                    $entry.Key,
                    "Machine"
                ) -ne $entry.Value) {
                throw "machine $($entry.Key) is not configured"
            }
        }
        Assert-ServiceDirectoryAcl -Path $SccacheDirectory -Writable
        Assert-BenchmarkScratch
    }
    $installedTargets = & (Get-RequiredCommand "rustup.exe") `
        target list --installed --toolchain $RustToolchain
    Assert-LastExitCode "rustup target list"
    foreach ($target in @("x86_64-unknown-none", "x86_64-unknown-uefi")) {
        if ($target -notin @($installedTargets)) {
            throw "Rust target $target is not installed"
        }
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
    if ($RunnerOnly) {
        $feature = Get-WindowsOptionalFeature `
            -Online `
            -FeatureName Microsoft-Hyper-V
        if ($feature.State -ne "Enabled") {
            throw "Hyper-V is not enabled"
        }
        Assert-PcatFirmware
    }

    if ($SkipWorkspace) {
        return
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

if (-not $RunnerOnly -and -not [string]::IsNullOrWhiteSpace($RunnerName)) {
    throw "-RunnerName requires -RunnerOnly"
}
if ($RunnerTokenStdin -and
    (-not $RunnerOnly -or [string]::IsNullOrWhiteSpace($RunnerName))) {
    throw "-RunnerTokenStdin requires -RunnerOnly and -RunnerName"
}

Assert-SupportedHost -SkipWorkspace:$RunnerOnly
Assert-ActionsRunnerWritablePaths
if ($CheckOnly) {
    if (-not $RunnerOnly -and
        -not [string]::IsNullOrWhiteSpace($GuestArtifactsDirectory)) {
        Assert-GuestArtifactSet $GuestArtifactsDirectory (Get-CurrentRevision)
    }
    Assert-Environment `
        -RequireBuild:(-not $SkipBuild -and -not $RunnerOnly) `
        -SkipWorkspace:$RunnerOnly
    if ($RunnerOnly -and -not [string]::IsNullOrWhiteSpace($RunnerName)) {
        Assert-ActionsRunner
    }
    Write-Output "NVX_SETUP_CHECK=ok"
    exit 0
}

Assert-Administrator
Install-Toolchain
if ($RunnerOnly) {
    Configure-SccacheEnvironment
    Configure-BenchmarkScratch
}
$restartNeeded = Enable-Whp

if ($restartNeeded) {
    Write-Output "NVX_SETUP_REBOOT_REQUIRED=1"
    Write-Output "Restart Windows, then rerun this script with -CheckOnly."
    exit 3010
}

if ($RunnerOnly) {
    if (-not [string]::IsNullOrWhiteSpace($RunnerName)) {
        $runnerToken = $null
        if ($RunnerTokenStdin) {
            $runnerToken = [Console]::In.ReadLine().Trim()
        }
        Install-ActionsRunner -Token $runnerToken
        $runnerToken = $null
    }
    Assert-Environment -SkipWorkspace
    if (-not [string]::IsNullOrWhiteSpace($RunnerName)) {
        Assert-ActionsRunner
    }
    Write-Output "NVX_RUNNER_SETUP_COMPLETE=1"
    exit 0
}

Copy-GuestArtifacts
if (-not $SkipBuild) {
    Build-Nvx
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
