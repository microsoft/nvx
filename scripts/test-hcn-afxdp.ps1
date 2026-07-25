#requires -Version 5.1

[CmdletBinding()]
param(
    [string]$Microvm,
    [string]$Kernel,
    [string]$Initrd,
    [Parameter(Mandatory)]
    [string]$EndpointConfig,
    [string]$GuestAddress = $(
        if ($env:NVX_HCN_AFXDP_GUEST_ADDRESS) {
            $env:NVX_HCN_AFXDP_GUEST_ADDRESS
        } else {
            '192.168.240.2'
        }
    ),
    [ValidateRange(1, 30)]
    [int]$PrefixLength = 24,
    [string]$Gateway = $(
        if ($env:NVX_HCN_AFXDP_GATEWAY) {
            $env:NVX_HCN_AFXDP_GATEWAY
        } else {
            '192.168.240.1'
        }
    ),
    [ValidateRange(576, 4082)]
    [int]$Mtu = 1500,
    [ValidateRange(1, 65535)]
    [int]$WebPort = 8099,
    [ValidateRange(30, 900)]
    [int]$TimeoutSeconds = 300,
    [string]$LogPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Microvm) { $Microvm = Join-Path $PSScriptRoot '..\target\release\microvm.exe' }
if (-not $Kernel) { $Kernel = Join-Path $PSScriptRoot '..\build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $PSScriptRoot '..\build\initramfs.cpio.gz' }
if (-not $LogPath) {
    $LogPath = Join-Path $PSScriptRoot '..\build\performance\hcn-afxdp-smoke.log'
}
$LogPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogPath)

function ConvertTo-NativeArgument {
    param([AllowEmptyString()][string]$Value)

    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Assert-IPv4Address {
    param([string]$Value, [string]$Name)

    $parsed = $null
    if (-not [Net.IPAddress]::TryParse($Value, [ref]$parsed) -or
        $parsed.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork) {
        throw "$Name must be an IPv4 address, got '$Value'"
    }
}

if ($env:OS -ne 'Windows_NT') {
    throw 'HCN AF_XDP smoke testing requires Windows'
}

$Microvm = (Resolve-Path $Microvm).Path
$Kernel = (Resolve-Path $Kernel).Path
$Initrd = (Resolve-Path $Initrd).Path
$EndpointConfig = (Resolve-Path $EndpointConfig).Path
Assert-IPv4Address $GuestAddress 'GuestAddress'
Assert-IPv4Address $Gateway 'Gateway'

$xdpApi = Join-Path $env:WINDIR 'System32\xdpapi.dll'
if (-not (Test-Path -LiteralPath $xdpApi -PathType Leaf)) {
    throw 'xdpapi.dll is missing; install signed XDP-for-Windows v1.3.0'
}
$xdpVersion = (Get-Item -LiteralPath $xdpApi).VersionInfo.ProductVersion
if ($xdpVersion -notlike '1.3.0*') {
    throw "XDP-for-Windows v1.3.0 is required, found '$xdpVersion'"
}
$hns = Get-Service -Name hns -ErrorAction Stop
if ($hns.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Running) {
    throw "Host Network Service is not running (status: $($hns.Status))"
}

& $Microvm --selftest --log-level warn
if ($LASTEXITCODE -ne 0) {
    throw "WHP self-test failed with exit code $LASTEXITCODE"
}

$endpoint = Get-Content -LiteralPath $EndpointConfig -Raw | ConvertFrom-Json
if (-not $endpoint.hostAttached -or
    [uint32]$endpoint.interfaceIndex -eq 0 -or
    [uint64]$endpoint.interfaceLuid -eq 0 -or
    -not $endpoint.gatewayMac) {
    throw "HCN endpoint descriptor does not contain a host-attached vNIC: $EndpointConfig"
}
if ($endpoint.guestAddress -ne $GuestAddress -or
    [int]$endpoint.prefixLength -ne $PrefixLength -or
    $endpoint.gateway -ne $Gateway) {
    throw "HCN endpoint descriptor addressing does not match the smoke-test parameters"
}

$python = (Get-Command python -ErrorAction Stop).Source
$webRoot = Join-Path ([IO.Path]::GetTempPath()) ('nvx-hcn-afxdp-www-' + [Guid]::NewGuid().ToString('N'))
$workRoot = Join-Path ([IO.Path]::GetTempPath()) ('nvx-hcn-afxdp-work-' + [Guid]::NewGuid().ToString('N'))

$pipeName = 'nvx-hcn-afxdp-' + [Guid]::NewGuid().ToString('N')
$pipePath = '\\.\pipe\' + $pipeName
$manifestPath = Join-Path ([IO.Path]::GetTempPath()) ("$pipeName.json")

$manifest = [ordered]@{
    version = 2
    attachment = [ordered]@{
        backend = 'hcn-afxdp-l2bridge'
        interfaceIndex = [uint32]$endpoint.interfaceIndex
        interfaceLuid = [uint64]$endpoint.interfaceLuid
        gatewayMac = [string]$endpoint.gatewayMac
        queueSelection = [ordered]@{
            mode = 'auto'
        }
    }
    device = [ordered]@{
        macAddress = [string]$endpoint.macAddress
        mtu = $Mtu
    }
    guestBootstrap = [ordered]@{
        ipv4 = [ordered]@{
            address = $GuestAddress
            prefixLength = $PrefixLength
            gateway = $Gateway
        }
        routes = @(
            [ordered]@{
                destination = '0.0.0.0/0'
                nextHop = $Gateway
            }
        )
        dns = [ordered]@{
            servers = @('1.1.1.1')
            search = @()
        }
    }
    runtime = [ordered]@{
        controlPipe = $pipePath
    }
}

$utf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText(
    $manifestPath,
    ($manifest | ConvertTo-Json -Depth 8 -Compress),
    $utf8
)

$pipe = $null
$reader = $null
$writer = $null
$process = $null
$stdoutTask = $null
$stderrTask = $null
$stdout = ''
$stderr = ''
$readyLine = ''
$exitCode = $null
$failure = $null
$started = $false
$serverProcess = $null
$serverStdoutTask = $null
$serverStderrTask = $null
$serverStdout = ''
$serverStderr = ''

try {
    New-Item -ItemType Directory -Path $workRoot | Out-Null
    $guestProbe = @'
marker=NVX-HCN-AFXDP
/bin/busybox timeout 20 /bin/busybox wget -qO /tmp/hcn-afxdp-http.log http://__GATEWAY__:__PORT__/
probe_status=$?
read guest_tx_packets < /sys/class/net/eth0/statistics/tx_packets
read guest_tx_errors < /sys/class/net/eth0/statistics/tx_errors
read guest_carrier < /sys/class/net/eth0/carrier
read guest_operstate < /sys/class/net/eth0/operstate
read host_response < /tmp/hcn-afxdp-http.log
echo "${marker}-GUEST-TX=${guest_tx_packets} ERRORS=${guest_tx_errors} CARRIER=${guest_carrier} OPERSTATE=${guest_operstate}"
echo "${marker}-HTTP=${host_response}"
if [ "${probe_status:-1}" -eq 0 ] && [ "${host_response}" = HELLO-HOST ]; then
    echo "${marker}-SMOKE-OK"
    exit 0
fi
echo "${marker}-SMOKE-FAIL"
exit 42
'@
    $guestProbe = $guestProbe.Replace('__GATEWAY__', $Gateway)
    $guestProbe = $guestProbe.Replace('__PORT__', [string]$WebPort)
    $guestProbe = $guestProbe.Replace("`r`n", "`n") + "`n"
    [IO.File]::WriteAllText((Join-Path $workRoot 'hcn-afxdp-smoke.sh'), $guestProbe, $utf8)

    $pipe = New-Object IO.Pipes.NamedPipeServerStream(
        $pipeName,
        [IO.Pipes.PipeDirection]::InOut,
        1,
        [IO.Pipes.PipeTransmissionMode]::Byte,
        [IO.Pipes.PipeOptions]::Asynchronous
    )
    $connectTask = $pipe.WaitForConnectionAsync()

    $arguments = @(
        '--kernel', $Kernel,
        '--initrd', $Initrd,
        '--mem', '512',
        '--cmdline', 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1',
        '--net-config', $manifestPath,
        '--mount', $workRoot,
        '--exec', '/mnt/host/hcn-afxdp-smoke.sh',
        '--log-level', 'info'
    )
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Microvm
    $startInfo.Arguments = (($arguments | ForEach-Object {
        ConvertTo-NativeArgument ([string]$_)
    }) -join ' ')
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw 'failed to start microvm.exe'
    }
    $started = $true
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)

    $connectRemaining = $deadline - [DateTime]::UtcNow
    if ($connectRemaining.TotalMilliseconds -le 0 -or -not $connectTask.Wait($connectRemaining)) {
        throw "timed out waiting for NVX to connect to control pipe $pipePath"
    }

    $reader = New-Object IO.StreamReader($pipe, $utf8, $false, 4096, $true)
    $writer = New-Object IO.StreamWriter($pipe, $utf8, 4096, $true)
    $writer.AutoFlush = $true

    $readyTask = $reader.ReadLineAsync()
    $readyRemaining = $deadline - [DateTime]::UtcNow
    if ($readyRemaining.TotalMilliseconds -le 0 -or -not $readyTask.Wait($readyRemaining)) {
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
    if ($ready.type -ne 'DataPlaneReady' -or [uint64]$ready.interfaceLuid -eq 0) {
        throw "invalid DataPlaneReady message: $readyLine"
    }
    $readyQueues = @($ready.queues)
    if ($readyQueues.Count -eq 0 -or -not ($readyQueues -contains 0)) {
        throw "AF_XDP automatic queue binding did not include queue 0: $readyLine"
    }

    New-Item -ItemType Directory -Path $webRoot | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $webRoot 'index.html'),
        'HELLO-HOST',
        (New-Object Text.ASCIIEncoding)
    )
    $serverInfo = New-Object Diagnostics.ProcessStartInfo
    $serverInfo.FileName = $python
    $serverInfo.Arguments = (@(
        '-m', 'http.server', [string]$WebPort,
        '--bind', '0.0.0.0',
        '--directory', $webRoot
    ) | ForEach-Object { ConvertTo-NativeArgument ([string]$_) }) -join ' '
    $serverInfo.UseShellExecute = $false
    $serverInfo.CreateNoWindow = $true
    $serverInfo.RedirectStandardOutput = $true
    $serverInfo.RedirectStandardError = $true
    $serverProcess = New-Object Diagnostics.Process
    $serverProcess.StartInfo = $serverInfo
    if (-not $serverProcess.Start()) {
        throw 'failed to start host HTTP helper'
    }
    $serverStdoutTask = $serverProcess.StandardOutput.ReadToEndAsync()
    $serverStderrTask = $serverProcess.StandardError.ReadToEndAsync()
    $serverReady = $false
    for ($attempt = 0; $attempt -lt 50 -and -not $serverReady; $attempt++) {
        $client = New-Object Net.Sockets.TcpClient
        try {
            $client.Connect('127.0.0.1', $WebPort)
            $serverReady = $true
        } catch [Net.Sockets.SocketException] {
            [Threading.Tasks.Task]::Delay(100).Wait()
        } finally {
            $client.Dispose()
        }
    }
    if (-not $serverReady) {
        throw "host HTTP helper did not listen on port $WebPort"
    }

    $writer.WriteLine('{"type":"StartVm"}')

    $processRemaining = $deadline - [DateTime]::UtcNow
    $waitMilliseconds = [int][Math]::Min(
        [int]::MaxValue,
        [Math]::Max(1, $processRemaining.TotalMilliseconds)
    )
    if ($processRemaining.TotalMilliseconds -le 0 -or -not $process.WaitForExit($waitMilliseconds)) {
        throw "HCN AF_XDP smoke test exceeded ${TimeoutSeconds}s"
    }
    $process.WaitForExit()
    $exitCode = $process.ExitCode
} catch {
    $failure = $_
} finally {
    if ($started) {
        try {
            if (-not $process.HasExited) {
                $process.Kill()
            }
            $process.WaitForExit()
        } catch [InvalidOperationException] {
            # The VM exited between HasExited and Kill/WaitForExit.
        }
    }
    if ($null -ne $stdoutTask) {
        try { $stdout = $stdoutTask.Result } catch { $stdout = '<stdout unavailable>' }
    } elseif ($null -ne $process) {
        try { $stdout = $process.StandardOutput.ReadToEnd() } catch { $stdout = '<stdout unavailable>' }
    }
    if ($null -ne $stderrTask) {
        try { $stderr = $stderrTask.Result } catch { $stderr = '<stderr unavailable>' }
    }
    if ($null -ne $serverProcess) {
        try {
            if (-not $serverProcess.HasExited) {
                $serverProcess.Kill()
            }
            $serverProcess.WaitForExit()
        } catch [InvalidOperationException] {
        }
        try { $serverStdout = $serverStdoutTask.Result } catch { $serverStdout = '<server stdout unavailable>' }
        try { $serverStderr = $serverStderrTask.Result } catch { $serverStderr = '<server stderr unavailable>' }
    }

    $logDirectory = Split-Path -Parent $LogPath
    if ($logDirectory) {
        New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
    }
    $log = @(
        "control: $readyLine",
        '--- stderr ---',
        $stderr,
        '--- stdout ---',
        $stdout,
        '--- HTTP helper stderr ---',
        $serverStderr,
        '--- HTTP helper stdout ---',
        $serverStdout
    ) -join [Environment]::NewLine
    [IO.File]::WriteAllText($LogPath, $log, $utf8)

    if ($null -ne $writer) { $writer.Dispose() }
    if ($null -ne $reader) { $reader.Dispose() }
    if ($null -ne $pipe) { $pipe.Dispose() }
    if ($null -ne $process) { $process.Dispose() }
    if ($null -ne $serverProcess) { $serverProcess.Dispose() }
    Remove-Item -LiteralPath $webRoot -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
}

Write-Host $stderr
Write-Host $stdout
if ($null -ne $failure) {
    throw $failure
}
if ($exitCode -ne 0) {
    throw "microvm.exe exited with code $exitCode"
}
if ($stdout -notmatch '(?m)^NVX-HCN-AFXDP-SMOKE-OK\r?$') {
    throw "guest-to-HCN gateway verification failed; see $LogPath"
}

Write-Host "PASS: externally managed HCN vNIC attached to AF_XDP (LUID $($ready.interfaceLuid), queues $($readyQueues -join ','))"
