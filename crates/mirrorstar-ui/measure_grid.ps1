# Measure mirrorstar-ui virtualized grid (Task 4): memory (200/1000 items) + scroll CPU.
#
# ASCII-only on purpose: PowerShell 5.1 decodes BOM-less UTF-8 as GBK and breaks on CJK.
#
# This script plays the role of the Tauri shell: it launches the already-built
# target\release\mirrorstar-ui.exe, performs the NDJSON Hello handshake, answers the
# 9 snapshot requests with N synthetic wallpapers, sends Control::Show, then measures
# the UI process private/WS memory and per-one-core CPU (idle + wheel-scroll).
#
# The 9 requests the UI proactively sends are: get_config, get_wallpapers, get_displays,
# get_arrangement, get_rotation_config, list_pools, get_unit_states, get_auto_start_status,
# get_wallpaper_state. Missing any of them leaves a request dangling and the UI never
# settles into its steady state (this was a measurement-accuracy bug: the harness used to
# stop after 8, silently dropping get_wallpaper_state).
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File measure_grid.ps1
#       -Counts 200,1000 -IdleSeconds 6 -CpuSampleSeconds 8 -ScrollMs 6000

param(
    [int[]]$Counts = @(200, 1000),
    [int]$IdleSeconds = 6,
    [int]$CpuSampleSeconds = 8,
    [int]$ScrollMs = 6000,
    [switch]$SkipShots
)

$ErrorActionPreference = "Stop"

# .NET Framework's Process.StandardInput derives its writer encoding from
# [Console]::InputEncoding and emits that encoding's BOM at stream start. On a
# UTF-8 console this injects EF BB BF into the child's stdin and breaks the
# NDJSON parser. Force a BOM-less UTF-8 so the wire stays clean.
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent (Split-Path -Parent $here)
$exe = Join-Path $root "target\release\mirrorstar-ui.exe"
if (-not (Test-Path $exe)) { throw "missing release binary: $exe (run: cargo build --release -p mirrorstar-ui)" }

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class UIGrid {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
}
"@

# ---- synthetic image assets (so real decode work happens) -------------------
$imgDir = Join-Path $env:TEMP "mirrorstar-ui-measure"
New-Item -ItemType Directory -Force -Path $imgDir | Out-Null

$thumbs = @()
for ($i = 0; $i -lt 8; $i++) {
    $p = Join-Path $imgDir ("thumb_{0}.jpg" -f $i)
    if (-not (Test-Path $p)) {
        $bmp = New-Object System.Drawing.Bitmap 320, 180
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.Clear([System.Drawing.Color]::FromArgb(255, (40 + $i * 25) % 256, (90 + $i * 17) % 256, (160 + $i * 11) % 256))
        $g.Dispose()
        $bmp.Save($p, [System.Drawing.Imaging.ImageFormat]::Jpeg)
        $bmp.Dispose()
    }
    $thumbs += $p
}
# one large source (1920x1080) to also exercise the "empty thumbnail -> capped source decode" path
$bigSrc = Join-Path $imgDir "big_src.png"
if (-not (Test-Path $bigSrc)) {
    $bmp = New-Object System.Drawing.Bitmap 1920, 1080
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 30, 30, 60))
    $g.Dispose()
    $bmp.Save($bigSrc, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

function New-Wallpapers([int]$count) {
    $list = New-Object System.Collections.ArrayList
    for ($i = 0; $i -lt $count; $i++) {
        $thumb = $thumbs[$i % $thumbs.Count]
        $entry = [ordered]@{
            id            = "w$i"
            file_path     = $bigSrc
            wallpaper_type = "Image"
            added_at      = "1700000000"
            thumbnail     = $thumb
        }
        if ($i % 17 -eq 0) { $entry.wallpaper_type = "Video"; $entry.thumbnail = "" }
        elseif ($i % 23 -eq 0) { $entry.wallpaper_type = "Web"; $entry.thumbnail = ""; $entry.file_path = "https://example.com/$i" }
        elseif ($i % 29 -eq 0) { $entry.wallpaper_type = "Gif"; $entry.thumbnail = "" }
        [void]$list.Add([pscustomobject]$entry)
    }
    return , $list.ToArray()
}

function Find-UiWindow([int]$procId) {
    $script:bestH = [IntPtr]::Zero
    $script:bestArea = 0
    $target = [uint32]$procId
    $cb = [UIGrid+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][UIGrid]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [UIGrid]::IsWindowVisible($h)) {
            $r = New-Object UIGrid+RECT
            [void][UIGrid]::GetWindowRect($h, [ref]$r)
            $area = ($r.R - $r.L) * ($r.B - $r.T)
            if ($area -gt $script:bestArea) { $script:bestArea = $area; $script:bestH = $h }
        }
        return $true
    }
    [void][UIGrid]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:bestH
}

function Read-Line([int]$timeoutMs) {
    $t = $p.StandardOutput.ReadLineAsync()
    if ($t.Wait($timeoutMs)) { return $t.Result } else { return $null }
}

function Mark([string]$m) {
    try { Add-Content -Path (Join-Path $here "_trace.log") -Value ((Get-Date -Format "HH:mm:ss.fff") + " " + $m) } catch { }
}

function Send-Json($obj) {
    $line = $obj | ConvertTo-Json -Compress -Depth 6
    # Raw UTF-8 bytes + LF: bypasses any writer preamble so the wire is pure NDJSON.
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($line + "`n")
    $p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $p.StandardInput.BaseStream.Flush()
}

function Invoke-Wheel([IntPtr]$hwnd, [int]$milliseconds) {
    $r = New-Object UIGrid+RECT
    [void][UIGrid]::GetWindowRect($hwnd, [ref]$r)
    $cx = $r.L + [int](($r.R - $r.L) * 0.35)
    $cy = $r.T + [int](($r.B - $r.T) * 0.55)
    [void][UIGrid]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 300
    [void][UIGrid]::SetCursorPos($cx, $cy)
    Start-Sleep -Milliseconds 200
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $dir = -1
    $ticks = 0
    while ($sw.Elapsed.TotalMilliseconds -lt $milliseconds) {
        [UIGrid]::mouse_event(0x0800, 0, 0, ($dir * 120), [IntPtr]::Zero)  # MOUSEEVENTF_WHEEL
        Start-Sleep -Milliseconds 30
        $ticks++
        if ($ticks % 25 -eq 0) { $dir = -$dir }
    }
    $sw.Stop()
}

function Get-CpuPercent([int]$procId, [scriptblock]$work, [double]$wallMs) {
    $t0 = (Get-Process -Id $procId).TotalProcessorTime
    & $work
    $t1 = (Get-Process -Id $procId).TotalProcessorTime
    return [math]::Round((($t1 - $t0).TotalMilliseconds / $wallMs) * 100.0, 1)
}

$results = @()

foreach ($count in $Counts) {
    Write-Host ("=== items={0} ===" -f $count)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    # stderr is left inherited (not redirected): .NET Framework's async ErrorDataReceived
    # handler runs in a scope where $script: is unreliable and can crash the process.
    # The child's own log lines simply surface on our stderr.
    $psi.RedirectStandardError = $false
    $psi.CreateNoWindow = $true

    $p = [System.Diagnostics.Process]::Start($psi)

    try {
        Send-Json @{ type = 'hello'; protocol_version = 1; capabilities = 15; client = 'measure-harness' }
        $uiHello = Read-Line 10000
        if (-not $uiHello) { throw "no UI hello" }
        Write-Host ("UI hello: {0}" -f $uiHello)

        $wallpapers = New-Wallpapers $count
        $answered = @{}
        $needed = 9
        $guard = 0
        while ($answered.Count -lt $needed -and $guard -lt 200) {
            $guard++
            $line = Read-Line 15000
            if (-not $line) { break }
            $msg = $line | ConvertFrom-Json
            if ($msg.type -ne 'request') { continue }
            $cmd = $msg.command
            $data = $null
            if ($cmd -eq 'get_config') { $data = @{} }
            elseif ($cmd -eq 'get_wallpapers') { $data = $wallpapers }
            elseif ($cmd -eq 'get_displays') { $data = @() }
            elseif ($cmd -eq 'get_arrangement') { $data = 'span' }
            elseif ($cmd -eq 'get_rotation_config') { $data = @{} }
            elseif ($cmd -eq 'list_pools') { $data = @() }
            elseif ($cmd -eq 'get_unit_states') { $data = @() }
            elseif ($cmd -eq 'get_auto_start_status') { $data = $false }
            elseif ($cmd -eq 'get_wallpaper_state') { $data = $null }  # -> Option<WallpaperState>::None
            else { $data = $null }
            Send-Json @{ type = 'response'; request_id = $msg.request_id; status = 'ok'; data = $data }
            if (-not $answered.ContainsKey($cmd)) { $answered[$cmd] = $true }
        }
        Write-Host ("answered: {0}" -f (($answered.Keys | Sort-Object) -join ','))

        # drive the window visible so layout reports a real viewport height
        Mark "before control show"
        Send-Json @{ type = 'control'; action = 'show' }
        Mark "after control show"
        Start-Sleep -Seconds 2
        Mark "after show sleep; childExited=$($p.HasExited)"
        $hwnd = Find-UiWindow $p.Id
        Mark "after Find-UiWindow hwnd=$hwnd"
        Write-Host ("window hwnd: {0}" -f $hwnd)
        if ($hwnd -eq [IntPtr]::Zero) {
            # Make the failure loud: previously a not-found window silently skipped the
            # wheel burst, so ScrollCpuPct/after-scroll memory looked "clean" but were
            # never actually measured.
            Write-Warning "UI window not found for pid $($p.Id): wheel-scroll CPU and after-scroll memory are NOT measured this tier"
        }
        if ($hwnd -ne [IntPtr]::Zero -and -not $SkipShots) {
            $r = New-Object UIGrid+RECT
            [void][UIGrid]::GetWindowRect($hwnd, [ref]$r)
            $shot = Join-Path $imgDir ("shot_{0}.png" -f $count)
            $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
            $g.Dispose()
            $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
            $bmp.Dispose()
            Write-Host ("screenshot: {0}" -f $shot)
        }

        # idle settle -> memory
        Start-Sleep -Seconds $IdleSeconds
        $pr = Get-Process -Id $p.Id
        $privMB = [math]::Round($pr.PrivateMemorySize64 / 1MB, 1)
        $wsMB = [math]::Round($pr.WorkingSet64 / 1MB, 1)
        $threads = $pr.Threads.Count

        # idle CPU over CpuSampleSeconds (no input)
        $idleWall = [System.Diagnostics.Stopwatch]::StartNew()
        $t0 = (Get-Process -Id $p.Id).TotalProcessorTime
        Start-Sleep -Seconds $CpuSampleSeconds
        $idleWall.Stop()
        $t1 = (Get-Process -Id $p.Id).TotalProcessorTime
        $idleCpu = [math]::Round((($t1 - $t0).TotalMilliseconds / $idleWall.Elapsed.TotalMilliseconds) * 100.0, 1)

        # scroll CPU
        $scrollCpu = $null
        if ($hwnd -ne [IntPtr]::Zero) {
            $sw = [System.Diagnostics.Stopwatch]::StartNew()
            $s0 = (Get-Process -Id $p.Id).TotalProcessorTime
            Invoke-Wheel $hwnd $ScrollMs
            $s1 = (Get-Process -Id $p.Id).TotalProcessorTime
            $sw.Stop()
            # wall excludes the 0.5s foreground/cursor warmup inside Invoke-Wheel
            $effective = [math]::Max(1.0, $sw.Elapsed.TotalMilliseconds - 500.0)
            $scrollCpu = [math]::Round((($s1 - $s0).TotalMilliseconds / $effective) * 100.0, 1)
        }

        # memory after scrolling (window churn should return to a bounded level)
        Start-Sleep -Seconds 2
        $pr2 = Get-Process -Id $p.Id
        $privAfterScroll = [math]::Round($pr2.PrivateMemorySize64 / 1MB, 1)
        $wsAfterScroll = [math]::Round($pr2.WorkingSet64 / 1MB, 1)

        $row = [pscustomobject]@{
            Items            = $count
            PrivateMB        = $privMB
            WSMB             = $wsMB
            Threads          = $threads
            IdleCpuPct       = $idleCpu
            ScrollCpuPct     = $scrollCpu
            PrivAfterScrollMB = $privAfterScroll
            WSAfterScrollMB  = $wsAfterScroll
        }
        $results += $row
        Write-Host ($row | Format-List | Out-String)
    }
    catch {
        Write-Host ("ERROR: " + $_)
        Write-Host ($_.ScriptStackTrace)
    }
    finally {
        $exited = $false
        try { $exited = $p.HasExited } catch {}
        if ($exited) { Write-Host ("child exited early, ExitCode=" + $p.ExitCode) }
        try { $p.Kill() } catch {}
        Start-Sleep -Milliseconds 500
    }
}

Write-Host "===== SUMMARY ====="
$results | Format-Table -AutoSize | Out-String | Write-Host
