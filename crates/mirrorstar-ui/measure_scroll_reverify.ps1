# Re-verify Task 9.2/9.3 "no unbounded growth after scrolling" with a forced-foreground
# wheel burst repeated over several rounds, sampling the UI process private memory each round.
#
# ASCII-only on purpose: PowerShell 5.1 decodes BOM-less UTF-8 as GBK and breaks on CJK.
#
# Why not measure_grid.ps1: that harness calls plain SetForegroundWindow(), which Windows can
# refuse when the harness is not the current foreground process; the wheel then goes to the
# wrong window and scroll CPU / after-scroll memory come out as 0. This harness attaches its
# input queue to the foreground thread first (AttachThreadInput) so the injection lands.
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File measure_scroll_reverify.ps1
#       -Items 200 -Rounds 3 -ScrollMs 8000

param(
    [int]$Items = 200,
    [int]$Rounds = 3,
    [int]$ScrollMs = 8000
)

$ErrorActionPreference = "Continue"
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent (Split-Path -Parent $here)
$exe = Join-Path $root "target\release\mirrorstar-ui.exe"
if (-not (Test-Path $exe)) { throw "missing release binary: $exe (run: cargo build --release -p mirrorstar-ui)" }

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class RSS {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
}
"@

# ---- synthetic assets (reuse measure_grid's, regenerate if missing) ---------
$imgDir = Join-Path $env:TEMP "mirrorstar-ui-measure"
New-Item -ItemType Directory -Force -Path $imgDir | Out-Null
$thumbs = @()
for ($i = 0; $i -lt 8; $i++) {
    $p = Join-Path $imgDir ("thumb_{0}.jpg" -f $i)
    if (-not (Test-Path $p)) {
        $bmp = New-Object System.Drawing.Bitmap 320, 180
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.Clear([System.Drawing.Color]::FromArgb(255, (40 + $i * 25) % 256, (90 + $i * 17) % 256, (160 + $i * 11) % 256))
        $g.Dispose(); $bmp.Save($p, [System.Drawing.Imaging.ImageFormat]::Jpeg); $bmp.Dispose()
    }
    $thumbs += $p
}
$bigSrc = Join-Path $imgDir "big_src.png"
if (-not (Test-Path $bigSrc)) {
    $bmp = New-Object System.Drawing.Bitmap 1920, 1080
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 30, 30, 60)); $g.Dispose()
    $bmp.Save($bigSrc, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
}

function New-Wallpapers([int]$count) {
    $list = New-Object System.Collections.ArrayList
    for ($i = 0; $i -lt $count; $i++) {
        [void]$list.Add([pscustomobject][ordered]@{
            id = "w$i"; file_path = $bigSrc; wallpaper_type = "Image"
            added_at = "1700000000"; thumbnail = $thumbs[$i % $thumbs.Count]
        })
    }
    return , $list.ToArray()
}

function Find-UiWindow([int]$procId) {
    $script:bestH = [IntPtr]::Zero; $script:bestArea = 0; $target = [uint32]$procId
    $cb = [RSS+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][RSS]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [RSS]::IsWindowVisible($h)) {
            $r = New-Object RSS+RECT
            [void][RSS]::GetWindowRect($h, [ref]$r)
            $area = ($r.R - $r.L) * ($r.B - $r.T)
            if ($area -gt $script:bestArea) { $script:bestArea = $area; $script:bestH = $h }
        }
        return $true
    }
    [void][RSS]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:bestH
}

function Read-Line([int]$timeoutMs) {
    $t = $p.StandardOutput.ReadLineAsync()
    if ($t.Wait($timeoutMs)) { return $t.Result } else { return $null }
}

function Send-Json($obj) {
    $line = $obj | ConvertTo-Json -Compress -Depth 6
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($line + "`n")
    $p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $p.StandardInput.BaseStream.Flush()
}

function Force-Foreground([IntPtr]$hwnd) {
    [void][RSS]::ShowWindow($hwnd, 9)
    [void][RSS]::BringWindowToTop($hwnd)
    for ($a = 0; $a -lt 4; $a++) {
        if ([RSS]::GetForegroundWindow() -eq $hwnd) { return $true }
        $fg = [RSS]::GetForegroundWindow(); $fgPid = [uint32]0
        $fgThread = [RSS]::GetWindowThreadProcessId($fg, [ref]$fgPid)
        $tPid = [uint32]0
        $tThread = [RSS]::GetWindowThreadProcessId($hwnd, [ref]$tPid)
        if ($fgThread -ne 0 -and $fgThread -ne $tThread) {
            [void][RSS]::AttachThreadInput($fgThread, $tThread, $true)
            [void][RSS]::SetForegroundWindow($hwnd)
            [void][RSS]::AttachThreadInput($fgThread, $tThread, $false)
        } else {
            [RSS]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
            [RSS]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
            [void][RSS]::SetForegroundWindow($hwnd)
        }
        Start-Sleep -Milliseconds 250
    }
    return ([RSS]::GetForegroundWindow() -eq $hwnd)
}

function Invoke-Wheel([IntPtr]$hwnd, [int]$ms) {
    $r = New-Object RSS+RECT
    [void][RSS]::GetWindowRect($hwnd, [ref]$r)
    $cx = $r.L + [int](($r.R - $r.L) * 0.35)
    $cy = $r.T + [int](($r.B - $r.T) * 0.55)
    [void][RSS]::SetCursorPos($cx, $cy)
    Start-Sleep -Milliseconds 200
    $sw = [System.Diagnostics.Stopwatch]::StartNew(); $dir = -1; $t = 0
    while ($sw.Elapsed.TotalMilliseconds -lt $ms) {
        [RSS]::mouse_event(0x0800, 0, 0, ($dir * 120), [IntPtr]::Zero)
        Start-Sleep -Milliseconds 30
        $t++
        if ($t % 25 -eq 0) { $dir = -$dir }
    }
    $sw.Stop()
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe; $psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true; $psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $false; $psi.CreateNoWindow = $true
$p = [System.Diagnostics.Process]::Start($psi)

try {
    Send-Json @{ type = 'hello'; protocol_version = 1; capabilities = 15; client = 'scroll-reverify' }
    if (-not (Read-Line 10000)) { throw "no UI hello" }
    $wallpapers = New-Wallpapers $Items
    $answered = @{}; $guard = 0
    while ($answered.Count -lt 9 -and $guard -lt 200) {
        $guard++
        $line = Read-Line 15000
        if (-not $line) { break }
        $msg = $line | ConvertFrom-Json
        if ($msg.type -ne 'request') { continue }
        $cmd = $msg.command
        $data = $null
        if ($cmd -eq 'get_wallpapers') { $data = $wallpapers }
        elseif ($cmd -eq 'get_arrangement') { $data = 'span' }
        elseif ($cmd -eq 'get_auto_start_status') { $data = $false }
        elseif ($cmd -eq 'get_wallpaper_state') { $data = $null }
        elseif ($cmd -eq 'get_displays' -or $cmd -eq 'list_pools' -or $cmd -eq 'get_unit_states') { $data = @() }
        else { $data = @{} }
        Send-Json @{ type = 'response'; request_id = $msg.request_id; status = 'ok'; data = $data }
        if (-not $answered.ContainsKey($cmd)) { $answered[$cmd] = $true }
    }
    Write-Host ("answered: {0}" -f (($answered.Keys | Sort-Object) -join ','))

    Send-Json @{ type = 'control'; action = 'show' }
    Start-Sleep -Seconds 2
    $hwnd = Find-UiWindow $p.Id
    Write-Host ("window hwnd: {0}" -f $hwnd)
    if ($hwnd -eq [IntPtr]::Zero) { throw "UI window not found" }
    $fg = Force-Foreground $hwnd
    Write-Host ("foreground == ui window: {0}" -f $fg)

    Start-Sleep -Seconds 3
    $base = [math]::Round((Get-Process -Id $p.Id).PrivateMemorySize64 / 1MB, 1)
    Write-Host ("[BASE] uiPriv={0}MB" -f $base)

    for ($r = 1; $r -le $Rounds; $r++) {
        $t0 = (Get-Process -Id $p.Id).TotalProcessorTime
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        Invoke-Wheel $hwnd $ScrollMs
        $sw.Stop()
        $t1 = (Get-Process -Id $p.Id).TotalProcessorTime
        $cpu = [math]::Round((($t1 - $t0).TotalMilliseconds / $sw.Elapsed.TotalMilliseconds) * 100.0, 1)
        Start-Sleep -Seconds 2
        $priv = [math]::Round((Get-Process -Id $p.Id).PrivateMemorySize64 / 1MB, 1)
        $ws = [math]::Round((Get-Process -Id $p.Id).WorkingSet64 / 1MB, 1)
        Write-Host ("[ROUND {0}] scrollCpu={1}% uiPriv={2}MB uiWs={3}MB" -f $r, $cpu, $priv, $ws)
    }
}
catch {
    Write-Host ("ERROR: " + $_)
    Write-Host ($_.ScriptStackTrace)
}
finally {
    try { $p.Kill() } catch {}
    Start-Sleep -Milliseconds 500
    Write-Host ("leftover mirrorstar-ui: {0}" -f @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue).Count)
}
Write-Host "=== done ==="
