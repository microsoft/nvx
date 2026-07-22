#requires -Version 5.1

[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Runs = 5,
    [string]$Microvm,
    [string]$Kernel,
    [string]$Initrd,
    [Parameter(Mandatory)]
    [string]$EndpointConfig,
    [string]$SnapshotPath,
    [string]$GuestAddress = $(
        if ($env:NVX_HCN_AFXDP_GUEST_ADDRESS) { $env:NVX_HCN_AFXDP_GUEST_ADDRESS } else { '192.168.240.2' }
    ),
    [ValidateRange(1, 30)]
    [int]$PrefixLength = 24,
    [string]$Gateway = $(
        if ($env:NVX_HCN_AFXDP_GATEWAY) { $env:NVX_HCN_AFXDP_GATEWAY } else { '192.168.240.1' }
    ),
    [ValidateRange(576, 4082)]
    [int]$Mtu = 1500,
    [ValidateRange(30, 900)]
    [int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Microvm) { $Microvm = Join-Path $PSScriptRoot '..\target\release\microvm.exe' }
if (-not $Kernel) { $Kernel = Join-Path $PSScriptRoot '..\build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $PSScriptRoot '..\build\initramfs.cpio.gz' }
if (-not $SnapshotPath) { $SnapshotPath = Join-Path $PSScriptRoot '..\build\hcn-afxdp-netsnap' }

function ConvertTo-NativeArgument {
    param([AllowEmptyString()][string]$Value)

    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') { return $Value }
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Format-Milliseconds {
    param([double]$Value)
    return $Value.ToString('0.0', [Globalization.CultureInfo]::InvariantCulture)
}

function Format-Median {
    param([double[]]$Values)

    [double[]]$ordered = $Values | Sort-Object
    $middle = [int][Math]::Floor($ordered.Count / 2)
    $median = if ($ordered.Count % 2 -eq 1) {
        $ordered[$middle]
    } else {
        ($ordered[$middle - 1] + $ordered[$middle]) / 2
    }
    return '{0} ms  (min {1}, max {2}, n={3})' -f `
        (Format-Milliseconds $median), `
        (Format-Milliseconds $ordered[0]), `
        (Format-Milliseconds $ordered[-1]), `
        $ordered.Count
}

function Invoke-MicrovmSelfTest {
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Microvm
    $startInfo.Arguments = '--selftest --log-level warn'
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) { throw 'failed to start WHP self-test' }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.Result
        $stderr = $stderrTask.Result
        if ($stdout) { Write-Output $stdout }
        if ($stderr) { Write-Output $stderr }
        if ($process.ExitCode -ne 0) {
            throw "WHP self-test failed with exit code $($process.ExitCode)"
        }
    } finally {
        $process.Dispose()
    }
}

function Invoke-AfxdpVm {
    param(
        [string[]]$Arguments,
        [string]$RequiredMarker,
        [switch]$RequireMetric
    )

    $pipeName = 'nvx-hcn-afxdp-' + [Guid]::NewGuid().ToString('N')
    $pipePath = '\\.\pipe\' + $pipeName
    $manifestPath = Join-Path ([IO.Path]::GetTempPath()) ("$pipeName.json")
    $manifest = [ordered]@{
        version = 2
        attachment = [ordered]@{
            backend = 'hcn-afxdp-l2bridge'
            interfaceIndex = [uint32]$script:endpoint.interfaceIndex
            interfaceLuid = [uint64]$script:endpoint.interfaceLuid
            gatewayMac = [string]$script:endpoint.gatewayMac
            queueSelection = [ordered]@{ mode = 'auto' }
        }
        device = [ordered]@{
            macAddress = [string]$script:endpoint.macAddress
            mtu = $Mtu
        }
        guestBootstrap = [ordered]@{
            ipv4 = [ordered]@{
                address = $GuestAddress
                prefixLength = $PrefixLength
                gateway = $Gateway
            }
            routes = @([ordered]@{ destination = '0.0.0.0/0'; nextHop = $Gateway })
            dns = [ordered]@{ servers = @('1.1.1.1'); search = @() }
        }
        runtime = [ordered]@{ controlPipe = $pipePath }
    }
    [IO.File]::WriteAllText(
        $manifestPath,
        ($manifest | ConvertTo-Json -Depth 8 -Compress),
        $script:utf8
    )

    $pipe = $null
    $reader = $null
    $writer = $null
    $process = $null
    $stdoutTask = $null
    $stderrTask = $null
    $started = $false
    $stopwatch = [Diagnostics.Stopwatch]::new()
    try {
        $pipe = New-Object IO.Pipes.NamedPipeServerStream(
            $pipeName,
            [IO.Pipes.PipeDirection]::InOut,
            1,
            [IO.Pipes.PipeTransmissionMode]::Byte,
            [IO.Pipes.PipeOptions]::Asynchronous
        )
        $connectTask = $pipe.WaitForConnectionAsync()
        $allArguments = @($Arguments) + @('--net-config', $manifestPath)
        $startInfo = New-Object Diagnostics.ProcessStartInfo
        $startInfo.FileName = $Microvm
        $startInfo.Arguments = (($allArguments | ForEach-Object {
            ConvertTo-NativeArgument ([string]$_)
        }) -join ' ')
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $process = New-Object Diagnostics.Process
        $process.StartInfo = $startInfo
        # Measure only the native VMM lifetime; manifest and PowerShell setup are outside the path.
        $stopwatch.Start()
        if (-not $process.Start()) { throw 'failed to start microvm.exe' }
        $started = $true
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)

        $remaining = $deadline - [DateTime]::UtcNow
        if ($remaining.TotalMilliseconds -le 0 -or -not $connectTask.Wait($remaining)) {
            throw "timed out waiting for AF_XDP control pipe $pipePath"
        }
        $reader = New-Object IO.StreamReader($pipe, $script:utf8, $false, 4096, $true)
        $writer = New-Object IO.StreamWriter($pipe, $script:utf8, 4096, $true)
        $writer.AutoFlush = $true
        $readyTask = $reader.ReadLineAsync()
        $remaining = $deadline - [DateTime]::UtcNow
        if ($remaining.TotalMilliseconds -le 0 -or -not $readyTask.Wait($remaining)) {
            throw 'timed out waiting for AF_XDP data-plane readiness'
        }
        $readyLine = $readyTask.Result
        if ([string]::IsNullOrWhiteSpace($readyLine)) {
            throw 'control pipe closed before reporting AF_XDP readiness'
        }
        $ready = $readyLine | ConvertFrom-Json
        if ($ready.type -eq 'DataPlaneError') {
            throw "NVX reported data-plane failure: $($ready.message)"
        }
        $queues = @($ready.queues)
        if ($ready.type -ne 'DataPlaneReady' -or $queues.Count -eq 0 -or -not ($queues -contains 0)) {
            throw "invalid DataPlaneReady message: $readyLine"
        }
        $writer.WriteLine('{"type":"StartVm"}')

        $remaining = $deadline - [DateTime]::UtcNow
        $waitMilliseconds = [int][Math]::Min(
            [int]::MaxValue,
            [Math]::Max(1, $remaining.TotalMilliseconds)
        )
        if ($remaining.TotalMilliseconds -le 0 -or -not $process.WaitForExit($waitMilliseconds)) {
            throw "AF_XDP VM run exceeded ${TimeoutSeconds}s"
        }
        $stopwatch.Stop()
        $process.WaitForExit()
        $stdout = $stdoutTask.Result
        $stderr = $stderrTask.Result
        $text = $stdout + [Environment]::NewLine + $stderr
        if ($process.ExitCode -ne 0) {
            throw "microvm.exe exited with code $($process.ExitCode)`n$text"
        }
        if ($RequiredMarker -and $text -notmatch [regex]::Escape($RequiredMarker)) {
            throw "AF_XDP VM run did not emit '$RequiredMarker'`n$text"
        }
        $metric = $null
        if ($RequireMetric) {
            $match = [regex]::Match($text, '(?:cold-start|restore):\s*([0-9.]+)')
            if (-not $match.Success) {
                throw "AF_XDP VM run did not report a timing marker`n$text"
            }
            $metric = [double]::Parse(
                $match.Groups[1].Value,
                [Globalization.CultureInfo]::InvariantCulture
            )
        }
        return [pscustomobject]@{
            Metric = $metric
            Wall = $stopwatch.Elapsed.TotalMilliseconds
            Text = $text
        }
    } finally {
        $stopwatch.Stop()
        if ($started -and -not $process.HasExited) {
            try { $process.Kill(); $process.WaitForExit() } catch [InvalidOperationException] {}
        }
        if ($null -ne $writer) { $writer.Dispose() }
        if ($null -ne $reader) { $reader.Dispose() }
        if ($null -ne $pipe) { $pipe.Dispose() }
        if ($null -ne $process) { $process.Dispose() }
        Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
    }
}

if ($env:OS -ne 'Windows_NT') { throw 'HCN AF_XDP benchmarking requires Windows' }
$Microvm = (Resolve-Path $Microvm).Path
$Kernel = (Resolve-Path $Kernel).Path
$Initrd = (Resolve-Path $Initrd).Path
$EndpointConfig = (Resolve-Path $EndpointConfig).Path
$SnapshotPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SnapshotPath)
$script:utf8 = New-Object Text.UTF8Encoding($false)
$script:endpoint = Get-Content -LiteralPath $EndpointConfig -Raw | ConvertFrom-Json
if (-not $script:endpoint.hostAttached -or
    [uint32]$script:endpoint.interfaceIndex -eq 0 -or
    [uint64]$script:endpoint.interfaceLuid -eq 0 -or
    -not $script:endpoint.gatewayMac) {
    throw "HCN endpoint descriptor does not contain a host-attached vNIC: $EndpointConfig"
}

Invoke-MicrovmSelfTest

$cmdline = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 virtnet_probe=$Gateway"
$coldMarker = "VIRTNET-PROBE-OK: $Gateway"
$restoreMarker = "NETSNAP-RESTORE-PROBE-OK: $Gateway"
$cold = New-Object Collections.Generic.List[double]
$coldWall = New-Object Collections.Generic.List[double]
$restored = New-Object Collections.Generic.List[double]
$restoreWall = New-Object Collections.Generic.List[double]

Write-Output "HCN AF_XDP networking + snapshot benchmark, median of $Runs, 256 MiB, 1 vCPU"
try {
    for ($run = 1; $run -le $Runs; $run++) {
        $result = Invoke-AfxdpVm -RequireMetric -RequiredMarker $coldMarker -Arguments @(
            '--kernel', $Kernel,
            '--initrd', $Initrd,
            '--mem', '256',
            '--cmdline', $cmdline,
            '--exit-on-boot',
            '--boot-marker', $coldMarker
        )
        $cold.Add($result.Metric)
        $coldWall.Add($result.Wall)
        Write-Output "  AF_XDP cold boot: run $run/$Runs complete (guest $(Format-Milliseconds $result.Metric) ms, wall $(Format-Milliseconds $result.Wall) ms)"
    }

    Remove-Item -LiteralPath $SnapshotPath -Recurse -Force -ErrorAction SilentlyContinue
    $capture = Invoke-AfxdpVm -RequiredMarker 'netsnap: pre-snapshot link OK' -Arguments @(
        '--kernel', $Kernel,
        '--initrd', $Initrd,
        '--mem', '256',
        '--cmdline', "$cmdline netsnap",
        '--snapshot', $SnapshotPath
    )
    if (-not (Test-Path -LiteralPath (Join-Path $SnapshotPath 'state.bin') -PathType Leaf) -or
        -not (Test-Path -LiteralPath (Join-Path $SnapshotPath 'mem.bin') -PathType Leaf)) {
        throw "AF_XDP snapshot capture is incomplete: $SnapshotPath"
    }
    Write-Output "  AF_XDP snapshot capture complete ($(Format-Milliseconds $capture.Wall) ms)"

    for ($run = 1; $run -le $Runs; $run++) {
        $result = Invoke-AfxdpVm -RequireMetric -RequiredMarker $restoreMarker -Arguments @(
            '--restore', $SnapshotPath,
            '--mem', '256',
            '--exit-on-boot',
            '--boot-marker', $restoreMarker
        )
        $restored.Add($result.Metric)
        $restoreWall.Add($result.Wall)
        Write-Output "  AF_XDP restore: run $run/$Runs complete (guest $(Format-Milliseconds $result.Metric) ms, wall $(Format-Milliseconds $result.Wall) ms)"
    }

    Write-Output "  cold  (guest start -> marker): $(Format-Median $cold.ToArray())"
    Write-Output "  cold wall-clock               : $(Format-Median $coldWall.ToArray())"
    Write-Output "  restore (guest resume -> marker): $(Format-Median $restored.ToArray())"
    Write-Output "  restore wall-clock             : $(Format-Median $restoreWall.ToArray())"
    Write-Output '  verified marker                : NETSNAP-RESTORE-OK'
} finally {
    Remove-Item -LiteralPath $SnapshotPath -Recurse -Force -ErrorAction SilentlyContinue
}