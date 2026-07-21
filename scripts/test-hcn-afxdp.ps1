#requires -Version 5.1

[CmdletBinding()]
param(
    [string]$Microvm,
    [string]$Kernel,
    [string]$Initrd,
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

& $Microvm --backend hcs --selftest --log-level warn
if ($LASTEXITCODE -ne 0) {
    throw "HCS preflight failed with exit code $LASTEXITCODE"
}

$python = (Get-Command python -ErrorAction Stop).Source
$webRoot = Join-Path ([IO.Path]::GetTempPath()) ('nvx-hcn-afxdp-www-' + [Guid]::NewGuid().ToString('N'))

$pipeName = 'nvx-hcn-afxdp-' + [Guid]::NewGuid().ToString('N')
$pipePath = '\\.\pipe\' + $pipeName
$guidBytes = [Guid]::NewGuid().ToByteArray()
$macAddress = '00-15-5D-52-{0:X2}-{1:X2}' -f (0xC0 + ($guidBytes[0] -band 0x0F)), $guidBytes[1]
$manifestPath = Join-Path ([IO.Path]::GetTempPath()) ("$pipeName.json")

$manifest = [ordered]@{
    version = 2
    attachment = [ordered]@{
        backend = 'hcn-afxdp-l2bridge'
        queueSelection = [ordered]@{
            mode = 'auto'
        }
    }
    device = [ordered]@{
        macAddress = $macAddress
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
$stdoutPrefix = New-Object Collections.Generic.List[string]
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
$hcnCleanupFailure = $null

try {
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
        '--exit-on-boot',
        '--boot-marker', 'NVX-HCN-AFXDP-STOP',
        '--log-level', 'info'
    )
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Microvm
    $startInfo.Arguments = (($arguments | ForEach-Object {
        ConvertTo-NativeArgument ([string]$_)
    }) -join ' ')
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw 'failed to start microvm.exe'
    }
    $started = $true
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

    $bootReady = $false
    while (-not $bootReady) {
        $stdoutLineTask = $process.StandardOutput.ReadLineAsync()
        $bootRemaining = $deadline - [DateTime]::UtcNow
        if ($bootRemaining.TotalMilliseconds -le 0 -or -not $stdoutLineTask.Wait($bootRemaining)) {
            throw 'timed out waiting for Alpine guest readiness'
        }
        $stdoutLine = $stdoutLineTask.Result
        if ($null -eq $stdoutLine) {
            throw 'microvm.exe exited before Alpine reported guest readiness'
        }
        $stdoutPrefix.Add($stdoutLine)
        $bootReady = $stdoutLine.Contains('ALPINE-MICROVM-BOOT-OK:')
    }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()

    $guestProbe = @'
marker=NVX-HCN-AFXDP
/bin/busybox timeout 20 /bin/busybox wget -qO /tmp/hcn-afxdp-http.log http://__GATEWAY__:__PORT__/; probe_status=$?
read guest_tx_packets < /sys/class/net/eth0/statistics/tx_packets; read guest_tx_errors < /sys/class/net/eth0/statistics/tx_errors; read guest_carrier < /sys/class/net/eth0/carrier; read guest_operstate < /sys/class/net/eth0/operstate; read host_response < /tmp/hcn-afxdp-http.log; echo "${marker}-GUEST-TX=${guest_tx_packets} ERRORS=${guest_tx_errors} CARRIER=${guest_carrier} OPERSTATE=${guest_operstate}"; echo "${marker}-HTTP=${host_response}"; if [ "${probe_status:-1}" -eq 0 ] && [ "${host_response}" = HELLO-HOST ]; then echo "${marker}-SMOKE-OK"; else echo "${marker}-SMOKE-FAIL"; fi; echo "${marker}-STOP"
'@
    $guestProbe = $guestProbe.Replace('__GATEWAY__', $Gateway).Replace('__PORT__', [string]$WebPort) + "`n"
    $inputBytes = $utf8.GetBytes($guestProbe)
    $process.StandardInput.BaseStream.Write($inputBytes, 0, $inputBytes.Length)
    $process.StandardInput.BaseStream.Flush()

    if (-not $process.WaitForExit(30000)) {
        $interruptBytes = $utf8.GetBytes([string][char]3)
        $process.StandardInput.BaseStream.Write($interruptBytes, 0, $interruptBytes.Length)
        $process.StandardInput.BaseStream.Flush()
        [Threading.Tasks.Task]::Delay(500).Wait()
        $guestFallback = "`necho `"`${marker}-SMOKE-FAIL`"; echo `"`${marker}-STOP`"`n"
        $inputBytes = $utf8.GetBytes($guestFallback)
        $process.StandardInput.BaseStream.Write($inputBytes, 0, $inputBytes.Length)
        $process.StandardInput.BaseStream.Flush()
    }
    $process.StandardInput.BaseStream.Close()

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
    try {
        $normalizedMac = $macAddress.Replace('-', '')
        $ownedEndpoint = Get-HnsEndpoint | Where-Object {
            $_.Owner -eq 'nvx' -and
            $_.MacAddress -and
            $_.MacAddress.Replace('-', '').Replace(':', '') -eq $normalizedMac
        } | Select-Object -First 1
        if ($ownedEndpoint) {
            $ownedNetworkId = [string]$ownedEndpoint.VirtualNetwork
            $ownedEndpoint | Remove-HnsEndpoint | Out-Null
            Get-HnsNetwork | Where-Object {
                $_.Owner -eq 'nvx' -and $_.ID -eq $ownedNetworkId
            } | Remove-HnsNetwork | Out-Null
            $hcnCleanupFailure = "microvm.exe left HCN endpoint $($ownedEndpoint.ID) after exit"
        }
    } catch {
        $hcnCleanupFailure = "last-resort HCN cleanup failed: $($_.Exception.Message)"
    }
    if ($null -ne $stdoutTask) {
        try {
            $stdout = ($stdoutPrefix -join [Environment]::NewLine)
            if ($stdout.Length -gt 0) { $stdout += [Environment]::NewLine }
            $stdout += $stdoutTask.Result
        } catch { $stdout = '<stdout unavailable>' }
    } elseif ($null -ne $process) {
        try {
            $stdout = ($stdoutPrefix -join [Environment]::NewLine)
            if ($stdout.Length -gt 0) { $stdout += [Environment]::NewLine }
            $stdout += $process.StandardOutput.ReadToEnd()
        } catch { $stdout = '<stdout unavailable>' }
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
    Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
}

Write-Host $stderr
Write-Host $stdout
if ($null -ne $failure) {
    throw $failure
}
if ($null -ne $hcnCleanupFailure) {
    throw $hcnCleanupFailure
}
if ($exitCode -ne 0) {
    throw "microvm.exe exited with code $exitCode"
}
if ($stdout -notmatch '(?m)^NVX-HCN-AFXDP-SMOKE-OK\r?$') {
    throw "guest-to-HCN gateway verification failed; see $LogPath"
}

Write-Host "PASS: HCN-created vNIC attached to AF_XDP (LUID $($ready.interfaceLuid), queues $($readyQueues -join ','))"
