#requires -Version 5.1

[CmdletBinding()]
param(
    [string]$Microvm,
    [string]$Kernel,
    [string]$Initrd,
    [ValidateRange(64, 65536)]
    [int]$MemoryMiB = 128,
    [ValidateRange(1, 300)]
    [int]$TimeoutSeconds = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($env:OS -ne 'Windows_NT') {
    throw 'WHP exec smoke testing requires Windows'
}

if (-not $Microvm) { $Microvm = Join-Path $PSScriptRoot '..\target\release\microvm.exe' }
if (-not $Kernel) { $Kernel = Join-Path $PSScriptRoot '..\build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $PSScriptRoot '..\build\initramfs.cpio.gz' }

$Microvm = (Resolve-Path $Microvm).Path
$Kernel = (Resolve-Path $Kernel).Path
$Initrd = (Resolve-Path $Initrd).Path
$HostErrorPrefix = 'NVX-HOST-ERROR:'

function ConvertTo-NativeArgument {
    param([AllowEmptyString()][string]$Value)

    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Invoke-Microvm {
    param(
        [string[]]$Arguments,
        [int]$ExpectedExitCode,
        [string]$RequiredOutput,
        [string]$RequiredError
    )

    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Microvm
    $startInfo.Arguments = (($Arguments | ForEach-Object {
                ConvertTo-NativeArgument ([string]$_)
            }) -join ' ')
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw 'failed to start microvm.exe'
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "microvm.exe exceeded ${TimeoutSeconds}s"
        }
        $process.WaitForExit()
        $stdout = $stdoutTask.Result
        $stderr = $stderrTask.Result
        if ($process.ExitCode -ne $ExpectedExitCode) {
            throw "microvm.exe exited $($process.ExitCode), expected $ExpectedExitCode`nstdout:`n$stdout`nstderr:`n$stderr"
        }
        if ($RequiredOutput -and $stdout -notmatch [regex]::Escape($RequiredOutput)) {
            throw "microvm.exe stdout did not contain '$RequiredOutput'`nstdout:`n$stdout`nstderr:`n$stderr"
        }
        if ($RequiredError -and $stderr -notmatch [regex]::Escape($RequiredError)) {
            throw "microvm.exe stderr did not contain '$RequiredError'`nstdout:`n$stdout`nstderr:`n$stderr"
        }
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout
            Stderr = $stderr
        }
    }
    finally {
        $process.Dispose()
    }
}

$workRoot = Join-Path ([IO.Path]::GetTempPath()) ('nvx-whp-exec-' + [Guid]::NewGuid().ToString('N'))
$scriptPath = Join-Path $workRoot 'workload.sh'
$snapshotPath = Join-Path $workRoot 'snapshot'
$utf8 = New-Object Text.UTF8Encoding($false)

try {
    New-Item -ItemType Directory -Path $workRoot | Out-Null

    foreach ($exitCode in @(0, 37)) {
        $marker = "NVX-EXEC-SMOKE-$exitCode"
        [IO.File]::WriteAllText($scriptPath, "echo $marker`nexit $exitCode`n", $utf8)
        $arguments = @(
            '--kernel', $Kernel,
            '--initrd', $Initrd,
            '--mem', [string]$MemoryMiB,
            '--mount', $workRoot,
            '--exec', '/mnt/host/workload.sh',
            '--log-level', 'off'
        )
        $result = Invoke-Microvm $arguments $exitCode $marker ''
        Write-Host "guest exec status $($result.ExitCode) propagated"
    }

    $legacyMarker = 'NVX-WHP-LEGACY-SHUTDOWN'
    [IO.File]::WriteAllText($scriptPath, "echo $legacyMarker`nexit 37`n", $utf8)
    $legacyArguments = @(
        '--kernel', $Kernel,
        '--initrd', $Initrd,
        '--mem', [string]$MemoryMiB,
        '--mount', $workRoot,
        '--cmdline', 'earlycon=xe9 console=hvc0 reboot=t panic=-1 nvx_exec=/mnt/host/workload.sh',
        '--log-level', 'off'
    )
    $result = Invoke-Microvm $legacyArguments 0 $legacyMarker ''
    Write-Host 'legacy WHP shutdown payload ignored'

    $missingArguments = @(
        '--kernel', $Kernel,
        '--initrd', $Initrd,
        '--mem', [string]$MemoryMiB,
        '--mount', $workRoot,
        '--exec', '/mnt/host/missing.sh',
        '--log-level', 'off'
    )
    $result = Invoke-Microvm $missingArguments 127 'executable script not found' ''
    Write-Host "missing workload status $($result.ExitCode) propagated"

    $conflictArguments = @(
        '--mount', $workRoot,
        '--exec', '/mnt/host/workload.sh',
        '--snapshot', $snapshotPath
    )
    $result = Invoke-Microvm $conflictArguments 2 '' $HostErrorPrefix
    Write-Host 'parser failure used the host-error prefix'

    $semanticArguments = @('--mount', $workRoot, '--exec', 'relative.sh')
    $result = Invoke-Microvm $semanticArguments 1 '' $HostErrorPrefix
    Write-Host 'semantic validation used the host-error prefix'

    $missingKernel = Join-Path $workRoot 'missing-vmlinux'
    $runtimeArguments = @('--kernel', $missingKernel, '--initrd', $Initrd)
    $result = Invoke-Microvm $runtimeArguments 1 '' $HostErrorPrefix
    Write-Host 'runtime failure used the host-error prefix'
}
finally {
    Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host 'WHP exec smoke test passed'