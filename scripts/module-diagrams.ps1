<#
.SYNOPSIS
Draws the module diagrams of the nvx crate with cargo-modules and Graphviz.

.DESCRIPTION
Analyzes nvx-rs with its cli, testing, and guest features and writes, for each target, to
build\modules unless -OutDir names another directory:

    <target>-tree.txt         the module tree (cargo modules structure)
    <target>-overview.svg     the top-level modules
    <target>-hierarchy.svg    every module, by containment
    <target>-uses.svg         containment and the internal use dependencies (dense)

The Graphviz source of each diagram (.dot) stays next to it. -Focus and -MaxDepth add suffixes
to the file names, so focused diagrams do not replace the full ones. -Atlas also writes
index.html, an interactive page with the diagrams of every target.

Needs cargo-modules and Graphviz, and Python for -Atlas:

    cargo +1.98 install cargo-modules --version 0.28.0 --locked
    winget install --id Graphviz.Graphviz --exact

.PARAMETER Target
The platforms to analyze: linux (x86_64-unknown-linux-musl, which includes the guest modules),
windows (x86_64-pc-windows-msvc), or both.

.PARAMETER View
The outputs to generate: tree, overview, hierarchy, and uses. Defaults to all of them.

.PARAMETER Focus
A module path to focus on, such as nvx::host. cargo-modules keeps related modules for context.

.PARAMETER MaxDepth
The maximum depth below the crate root, or below -Focus, of the tree, hierarchy, and uses views.

.PARAMETER Format
The format of the diagrams: svg (default), png, or pdf.

.PARAMETER OutDir
The output directory. Defaults to build\modules in the repository.

.PARAMETER Atlas
Also writes index.html: an interactive page with the hierarchy, each top-level module, and the
dependencies between top-level modules, which scripts\module_atlas.py builds. It covers both
targets unless -Target is given, adds the tree, hierarchy, and uses outputs that it reads to
-View, and needs the whole module graph, so it does not combine with -Focus or -MaxDepth.

.PARAMETER Open
Opens the generated files, or only the interactive page with -Atlas.

.EXAMPLE
.\scripts\module-diagrams.ps1

Writes the tree and the three diagrams of the Linux target to build\modules.

.EXAMPLE
.\scripts\module-diagrams.ps1 -Target linux, windows -View hierarchy -Open

.EXAMPLE
.\scripts\module-diagrams.ps1 -Focus nvx::host -View hierarchy, uses

.EXAMPLE
.\scripts\module-diagrams.ps1 -Atlas -Open

Writes the outputs of both targets and the interactive page, then opens the page.
#>

#Requires -Version 7.4

[CmdletBinding()]
param(
    [Parameter()]
    [ValidateSet("linux", "windows")]
    [string[]]$Target = "linux",

    [Parameter()]
    [ValidateSet("tree", "overview", "hierarchy", "uses")]
    [string[]]$View = @("tree", "overview", "hierarchy", "uses"),

    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$Focus,

    [Parameter()]
    [ValidateRange(1, 1000)]
    [int]$MaxDepth,

    [Parameter()]
    [ValidateSet("svg", "png", "pdf")]
    [string]$Format = "svg",

    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$OutDir,

    [Parameter()]
    [switch]$Atlas,

    [Parameter()]
    [switch]$Open
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repoRoot "nvx-rs\Cargo.toml"
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw "$manifest not found: keep this script in the scripts directory of the NVX repository"
}
$OutDir = if ($OutDir) {
    $PSCmdlet.GetUnresolvedProviderPathFromPSPath($OutDir)
} else {
    Join-Path $repoRoot "build\modules"
}
$atlasPage = Join-Path $OutDir "index.html"
if ($Atlas) {
    if ($Focus -or $MaxDepth) {
        throw "-Atlas needs the whole module graph; omit -Focus and -MaxDepth"
    }
    if (-not $PSBoundParameters.ContainsKey("Target")) {
        $Target = @("linux", "windows")
    }
    # The outputs that the page reads and links to.
    $View = @($View) + @("tree", "hierarchy", "uses") | Select-Object -Unique
}

if (-not (Get-Command cargo-modules -CommandType Application -ErrorAction SilentlyContinue)) {
    throw "cargo-modules not found; install it with: cargo +1.98 install cargo-modules --version 0.28.0 --locked"
}
$dot = $null
if ($View | Where-Object { $_ -ne "tree" }) {
    $dot = Get-Command dot -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty Source
    if (-not $dot) {
        # The Graphviz installer does not always add itself to PATH.
        $dot = Join-Path $env:ProgramFiles "Graphviz\bin\dot.exe"
        if (-not (Test-Path -LiteralPath $dot -PathType Leaf)) {
            throw "Graphviz not found; install it with: winget install --id Graphviz.Graphviz --exact"
        }
    }
}
$python = $null
if ($Atlas) {
    $python = Get-Command python -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty Source
    if (-not $python) {
        throw "Python not found; -Atlas runs scripts\module_atlas.py with it"
    }
}

$triples = @{
    linux = "x86_64-unknown-linux-musl"
    windows = "x86_64-pc-windows-msvc"
}
$focusSuffix = if ($Focus) { "-" + ($Focus -replace "\W+", "-").Trim([char]"-") } else { "" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outputs = [System.Collections.Generic.List[string]]::new()
$noColor = $env:NO_COLOR
# Runs cargo where rust-toolchain.toml selects the repository's toolchain.
Push-Location -LiteralPath $repoRoot
try {
    # Keeps terminal color codes out of the files.
    $env:NO_COLOR = "1"
    foreach ($platform in $Target | Select-Object -Unique) {
        $scope = @(
            "--lib", "--manifest-path", $manifest, "--features", "cli,testing,guest",
            "--target", $triples[$platform], "--no-fns", "--no-traits", "--no-types"
        )
        if ($Focus) {
            $scope += "--focus-on", $Focus
        }
        foreach ($name in $View | Select-Object -Unique) {
            $stem = "$platform-$name$focusSuffix"
            $depth = $MaxDepth
            if ($name -eq "overview") {
                $depth = 1
            } elseif ($MaxDepth) {
                $stem += "-depth$MaxDepth"
            }
            $arguments = @(if ($name -eq "tree") { "structure" } else { "dependencies" }) + $scope
            if ($depth) {
                $arguments += "--max-depth", $depth
            }
            if ($name -eq "uses") {
                # Graphviz's dot layout loses some of these edges, so this view keeps
                # cargo-modules' default layout.
                $arguments += "--no-externs", "--no-sysroot"
            } elseif ($name -ne "tree") {
                $arguments += "--no-uses", "--layout", "dot"
            }
            $base = Join-Path $OutDir $stem
            $source = if ($name -eq "tree") { "$base.txt" } else { "$base.dot" }
            Write-Host "[$platform] $name"
            # PowerShell 7.4 and later keep the bytes of native output redirected to a file.
            cargo modules @arguments > $source
            if ($LASTEXITCODE -ne 0) {
                Remove-Item -LiteralPath $source -ErrorAction SilentlyContinue
                throw "cargo modules $($arguments[0]) failed with exit code $LASTEXITCODE"
            }
            if ($name -eq "tree") {
                $outputs.Add($source)
                continue
            }
            $image = "$base.$Format"
            & $dot "-T$Format" $source -o $image
            if ($LASTEXITCODE -ne 0) {
                throw "Graphviz failed with exit code $LASTEXITCODE on $source"
            }
            $outputs.Add($image)
        }
    }
    if ($Atlas) {
        Write-Host "[atlas] index.html"
        $atlasArguments = @(
            (Join-Path $PSScriptRoot "module_atlas.py"), "--input", $OutDir, "--dot", $dot
        )
        foreach ($platform in $Target | Select-Object -Unique) {
            $atlasArguments += "--platform", $platform
        }
        & $python @atlasArguments
        if ($LASTEXITCODE -ne 0) {
            throw "module_atlas.py failed with exit code $LASTEXITCODE"
        }
        $outputs.Add($atlasPage)
    }
} finally {
    $env:NO_COLOR = $noColor
    Pop-Location
}

Get-Item -LiteralPath $outputs
if ($Open) {
    $opened = if ($Atlas) { @($atlasPage) } else { $outputs }
    foreach ($output in $opened) {
        Invoke-Item -LiteralPath $output
    }
}
