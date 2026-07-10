<#
.SYNOPSIS
    virt-fs benchmark for the micro-VM on Windows (WHP backend). Mirrors scripts/bench-virtfs.sh.

.DESCRIPTION
    Benchmarks the virt-fs (see the VMM's --mount): guest I/O throughput to the mounted filesystem
    and the persistence round-trip of a read-write --mount-image. Two parts:
      1. Sequential write/read throughput as observed inside the guest (busybox dd), for a
         read-write mount that is ephemeral (--mount-rw, image held in guest memory) and one that
         is persistent (--mount-image, image mapped shared from a host file).
      2. Persistence round-trip on a persistent --mount-image: the wall-clock to create the image
         and write a payload the first time, versus reusing the image afterwards, verifying via a
         checksum that the guest's writes survived across VM runs (and in the host image file).

    Unlike the KVM script no host mke2fs (or any Linux tool) is needed: the WHP backend builds the
    image as a FAT filesystem in pure Rust, and the guest mounts it as vfat.

.EXAMPLE
    scripts\bench-virtfs.ps1
    scripts\bench-virtfs.ps1 -Mem 512 -N 5 -PayloadMB 64
#>
[CmdletBinding()]
param(
    [string]$Kernel,
    [string]$Initrd,
    [int]$Mem = 512,
    [int]$N = 5,
    [int]$PayloadMB = 64,
    # Give the FAT image comfortable headroom over the payload.
    [int]$ImgMB = 0
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\microvm.exe'
if (-not $Kernel) { $Kernel = Join-Path $repo 'build\vmlinux' }
if (-not $Initrd) { $Initrd = Join-Path $repo 'build\initramfs.cpio.gz' }
if ($ImgMB -le 0) { $ImgMB = $PayloadMB * 2 + 64 }
$cmdline = 'console=hvc0 quiet loglevel=0 reboot=t panic=-1'

if (-not (Test-Path $bin))    { throw "build the VMM first: cargo build --release" }
if (-not (Test-Path $Kernel)) { throw "missing kernel: $Kernel (scripts\build-linux-artifacts.ps1)" }
if (-not (Test-Path $Initrd)) { throw "missing initrd: $Initrd" }

# Boots the guest, feeds it $Script on stdin, and returns the combined console output. Both pipes
# are drained concurrently so a chatty guest can't fill the buffer and block.
function Invoke-Guest {
    param([string]$Script, [string[]]$MountArgs, [int]$TimeoutSec = 120)
    $a = @('--kernel', $Kernel, '--initrd', $Initrd, '--mem', "$Mem", '--log-level', 'off',
        '--cmdline', $cmdline) + $MountArgs
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($a | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    # The trailing newline matters: without it the guest shell never receives the final line.
    $p.StandardInput.Write($Script)
    $p.StandardInput.Close()
    if (-not $p.WaitForExit($TimeoutSec * 1000)) { try { $p.Kill() } catch {}; $p.WaitForExit(2000) | Out-Null }
    $err.Result + "`n" + $out.Result
}

# Extracts the dd throughput token (e.g. "355.6MB/s") from the Nth "copied" line of guest output.
function Get-DdRate {
    param([string]$Text, [int]$Occurrence)
    $lines = @($Text -split "`n" | Where-Object { $_ -match 'copied' })
    if ($lines.Count -lt $Occurrence) { return $null }
    if ($lines[$Occurrence - 1] -match '([0-9.]+)\s?([KMG]?)B/s') {
        $v = [double]$Matches[1]
        switch ($Matches[2]) { 'K' { $v * 1e-3 } 'M' { $v } 'G' { $v * 1e3 } default { $v * 1e-6 } }
    } else { $null }
}

# Reduces MB/s samples to a median line.
function Format-RateMedian {
    param([double[]]$Vals)
    $s = @($Vals | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object)
    if (-not $s.Count) { return '     n/a  (too fast for busybox dd to time)' }
    '{0,8:N1} MB/s  (min {1:N0}, max {2:N0}, n={3})' -f $s[[int]($s.Count/2)], $s[0], $s[-1], $s.Count
}

# Reduces millisecond samples to a median line.
function Format-MsMedian {
    param([double[]]$Vals)
    $s = @($Vals | Where-Object { $_ -ne $null } | Sort-Object)
    if (-not $s.Count) { return 'NO DATA' }
    '{0,7:N0} ms  (min {1:N0}, max {2:N0}, n={3})' -f $s[[int]($s.Count/2)], $s[0], $s[-1], $s.Count
}

# Extracts the cksum checksum for /mnt/host/data.bin from guest output.
function Get-DataCksum {
    param([string]$Text)
    $l = @($Text -split "`n" | Where-Object { $_ -match 'data\.bin' -and $_ -match '^\s*[0-9]+' })
    if ($l.Count -and $l[0] -match '([0-9]+)') { $Matches[1] } else { $null }
}

$seed = Join-Path ([System.IO.Path]::GetTempPath()) ("nvx-vfs-seed-" + [System.Guid]::NewGuid().ToString('N'))
$persistImg = Join-Path ([System.IO.Path]::GetTempPath()) ("nvx-vfs-persist-" + [System.Guid]::NewGuid().ToString('N') + '.img')
$roundImg = Join-Path ([System.IO.Path]::GetTempPath()) ("nvx-vfs-round-" + [System.Guid]::NewGuid().ToString('N') + '.img')
New-Item -ItemType Directory -Path $seed | Out-Null
Set-Content -Path (Join-Path $seed 'README') -Value 'virt-fs benchmark seed' -NoNewline

try {
    Write-Host "virt-fs benchmark: $PayloadMB MiB payload, ${Mem} MiB guest, image $ImgMB MiB, median of $N runs"
    Write-Host ''

    # Guest workload: write then read PAYLOAD_MB through the mount (plain dd, no pipelines).
    $ioScript = "dd if=/dev/zero of=/mnt/host/bench.bin bs=1M count=$PayloadMB conv=fsync 2>&1`n" +
                "sync`necho 3 > /proc/sys/vm/drop_caches 2>/dev/null`n" +
                "dd if=/mnt/host/bench.bin of=/dev/null bs=1M 2>&1`nsync`nreboot -f`n"

    # ---- Part 1: guest-observed sequential throughput ----
    function Invoke-IoBench {
        param([string]$Label, [string[]]$MountArgs)
        $w = @(); $r = @()
        for ($i = 0; $i -lt $N; $i++) {
            $o = Invoke-Guest -Script $ioScript -MountArgs $MountArgs
            $w += (Get-DdRate -Text $o -Occurrence 1)
            $r += (Get-DdRate -Text $o -Occurrence 2)
        }
        Write-Host ("  {0,-27} write {1}" -f $Label, (Format-RateMedian -Vals $w))
        Write-Host ("  {0,-27} read  {1}" -f '', (Format-RateMedian -Vals $r))
    }

    Write-Host '== sequential throughput (guest dd, conv=fsync writes) =='
    Invoke-IoBench -Label 'rw ephemeral (in-memory)'  -MountArgs @('--mount', $seed, '--mount-rw', '--mount-size', "$ImgMB")
    Invoke-IoBench -Label 'rw persistent (file-backed)' -MountArgs @('--mount', $seed, '--mount-image', $persistImg, '--mount-size', "$ImgMB")
    Remove-Item -Force $persistImg -ErrorAction SilentlyContinue
    Write-Host ''

    # ---- Part 2: persistence round-trip on a --mount-image ----
    Write-Host '== persistence round-trip (rw --mount-image) =='
    Remove-Item -Force $roundImg -ErrorAction SilentlyContinue
    $createScript = "dd if=/dev/zero of=/mnt/host/data.bin bs=1M count=$PayloadMB 2>/dev/null`n" +
                    "cksum /mnt/host/data.bin`nsync`nreboot -f`n"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $createOut = Invoke-Guest -Script $createScript -MountArgs @('--mount', $seed, '--mount-image', $roundImg, '--mount-size', "$ImgMB")
    $sw.Stop()
    $createCk = Get-DataCksum -Text $createOut
    Write-Host ("  create image + write {0} MiB       : {1:N0} ms" -f $PayloadMB, $sw.Elapsed.TotalMilliseconds)
    Write-Host ("  host image on disk                : {0} ({1:N0} MiB)" -f (Split-Path $roundImg -Leaf), ((Get-Item $roundImg).Length / 1MB))

    $reuse = @(); $ok = 0
    $verifyScript = "cksum /mnt/host/data.bin 2>/dev/null`nreboot -f`n"
    for ($i = 0; $i -lt $N; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $verifyOut = Invoke-Guest -Script $verifyScript -MountArgs @('--mount', $seed, '--mount-image', $roundImg, '--mount-size', "$ImgMB")
        $sw.Stop()
        $reuse += $sw.Elapsed.TotalMilliseconds
        if ($createCk -and (Get-DataCksum -Text $verifyOut) -eq $createCk) { $ok++ }
    }
    Write-Host ("  reuse image + verify (cold each)  : {0}" -f (Format-MsMedian -Vals $reuse))
    Write-Host ("  payload survived across runs      : {0}/{1} runs (cksum {2})" -f $ok, $N, $(if ($createCk) { $createCk } else { '?' }))
}
finally {
    Remove-Item -Recurse -Force $seed -ErrorAction SilentlyContinue
    Remove-Item -Force $persistImg, $roundImg -ErrorAction SilentlyContinue
}
