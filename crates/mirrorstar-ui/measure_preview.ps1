# Measure mirrorstar-ui Task 8 previews: per-type memory + child-process counts
# (video -> mpv.exe ; web -> msedgewebview2.exe), before / open / after-close.
#
# ASCII-only on purpose: PowerShell 5.1 decodes BOM-less UTF-8 as GBK and breaks on CJK.
#
# Plays the role of the Tauri shell: launches the built target\release\mirrorstar-ui.exe,
# performs the NDJSON Hello handshake, answers snapshot requests with 4 synthetic wallpapers
# (Image / Video / Gif / Web), sends Control::Show, then clicks the "preview" button of each
# card and samples the UI process private/WS memory plus mpv.exe / msedgewebview2.exe counts.
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File measure_preview.ps1

param(
    [int]$OpenWaitSeconds = 5,
    [int]$WebOpenWaitSeconds = 12,
    [int]$CloseWaitSeconds = 4
)

$ErrorActionPreference = "Stop"

# .NET Framework's Process.StandardInput emits the console encoding's BOM at stream start;
# force BOM-less UTF-8 so the NDJSON wire stays clean.
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent (Split-Path -Parent $here)
$exe = Join-Path $root "target\release\mirrorstar-ui.exe"
if (-not (Test-Path $exe)) { throw "missing release binary: $exe (run: cargo build --release -p mirrorstar-ui)" }

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class UIPrev {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
}
"@

# ---- synthetic assets -------------------------------------------------------
$assetDir = Join-Path $env:TEMP "mirrorstar-ui-preview"
New-Item -ItemType Directory -Force -Path $assetDir | Out-Null

$imgSrc = Join-Path $assetDir "preview_image.png"
if (-not (Test-Path $imgSrc)) {
    $bmp = New-Object System.Drawing.Bitmap 2560, 1440
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 40, 90, 160))
    $g.Dispose()
    $bmp.Save($imgSrc, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

$videoSrc = Get-ChildItem -Path (Join-Path $root "wallpaper") -Filter *.mp4 -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $videoSrc) { throw "no mp4 found under wallpaper/" }

$gifSrc = Join-Path $assetDir "preview_anim.gif"
if (-not (Test-Path $gifSrc)) {
    $py = @"
from PIL import Image
frames = []
for i in range(8):
    im = Image.new('P', (320, 180), i * 30 % 256)
    im.putpalette([(i*8) % 256 for i in range(768)])
    frames.append(im)
frames[0].save(r'$gifSrc', save_all=True, append_images=frames[1:], duration=120, loop=0)
"@
    $py | python -
}

$webSrc = Join-Path $assetDir "preview_page.html"
if (-not (Test-Path $webSrc)) {
    Set-Content -Path $webSrc -Encoding ASCII -Value "<!doctype html><meta charset='utf-8'><body style='background:#203a43;color:#eaf6ff'><h1>web preview</h1></body>"
}

# card order must be Image, Video, Gif, Web so cols 0..3 hit the four types.
function New-Wallpapers() {
    $list = New-Object System.Collections.ArrayList
    [void]$list.Add([pscustomobject][ordered]@{ id = "p-img"; file_path = $imgSrc;   wallpaper_type = "Image"; added_at = "1700000000"; thumbnail = $imgSrc })
    [void]$list.Add([pscustomobject][ordered]@{ id = "p-vid"; file_path = $videoSrc; wallpaper_type = "Video"; added_at = "1700000001"; thumbnail = "" })
    [void]$list.Add([pscustomobject][ordered]@{ id = "p-gif"; file_path = $gifSrc;   wallpaper_type = "Gif";   added_at = "1700000002"; thumbnail = "" })
    [void]$list.Add([pscustomobject][ordered]@{ id = "p-web"; file_path = $webSrc;   wallpaper_type = "Web";   added_at = "1700000003"; thumbnail = "" })
    return , $list.ToArray()
}

function Find-UiWindow([int]$procId) {
    $script:bestH = [IntPtr]::Zero
    $script:bestArea = 0
    $target = [uint32]$procId
    $cb = [UIPrev+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][UIPrev]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [UIPrev]::IsWindowVisible($h)) {
            $r = New-Object UIPrev+RECT
            [void][UIPrev]::GetWindowRect($h, [ref]$r)
            $area = ($r.R - $r.L) * ($r.B - $r.T)
            if ($area -gt $script:bestArea) { $script:bestArea = $area; $script:bestH = $h }
        }
        return $true
    }
    [void][UIPrev]::EnumWindows($cb, [IntPtr]::Zero)
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

function Click-Client([IntPtr]$hwnd, [int]$lx, [int]$ly) {
    $scale = [UIPrev]::GetDpiForWindow($hwnd) / 96.0
    $pt = New-Object UIPrev+POINT
    $pt.X = [int]($lx * $scale)
    $pt.Y = [int]($ly * $scale)
    [void][UIPrev]::ClientToScreen($hwnd, [ref]$pt)
    [void][UIPrev]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 250
    [void][UIPrev]::SetCursorPos($pt.X, $pt.Y)
    Start-Sleep -Milliseconds 150
    [UIPrev]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
    Start-Sleep -Milliseconds 70
    [UIPrev]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
    Start-Sleep -Milliseconds 350
}

function Shot([IntPtr]$hwnd, [string]$name) {
    if ($hwnd -eq [IntPtr]::Zero) { return }
    $r = New-Object UIPrev+RECT
    [void][UIPrev]::GetWindowRect($hwnd, [ref]$r)
    $shot = Join-Path $assetDir ("shot_{0}.png" -f $name)
    $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
    $g.Dispose()
    $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("screenshot: {0}" -f $shot)
}

function Sample([int]$procId) {
    $pr = Get-Process -Id $procId -ErrorAction SilentlyContinue
    $priv = if ($pr) { [math]::Round($pr.PrivateMemorySize64 / 1MB, 1) } else { -1 }
    $ws = if ($pr) { [math]::Round($pr.WorkingSet64 / 1MB, 1) } else { -1 }
    $mpv = (Get-Process mpv -ErrorAction SilentlyContinue | Measure-Object).Count
    $wv2 = (Get-Process msedgewebview2 -ErrorAction SilentlyContinue | Measure-Object).Count
    return [pscustomobject]@{ PrivateMB = $priv; WSMB = $ws; Mpv = $mpv; WebView2 = $wv2 }
}

$results = @()
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $false
$psi.CreateNoWindow = $true

$p = [System.Diagnostics.Process]::Start($psi)
try {
    Send-Json @{ type = 'hello'; protocol_version = 1; capabilities = 15; client = 'preview-harness' }
    $uiHello = Read-Line 10000
    if (-not $uiHello) { throw "no UI hello" }
    Write-Host ("UI hello: {0}" -f $uiHello)

    $wallpapers = New-Wallpapers
    $answered = @{}
    $needed = 8
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
        else { $data = $null }
        Send-Json @{ type = 'response'; request_id = $msg.request_id; status = 'ok'; data = $data }
        if (-not $answered.ContainsKey($cmd)) { $answered[$cmd] = $true }
    }
    Write-Host ("answered: {0}" -f (($answered.Keys | Sort-Object) -join ','))

    Send-Json @{ type = 'control'; action = 'show' }
    Start-Sleep -Seconds 2
    $hwnd = Find-UiWindow $p.Id
    Write-Host ("window hwnd: {0}" -f $hwnd)
    if ($hwnd -eq [IntPtr]::Zero) { throw "UI window not found" }
    Shot $hwnd "00_idle"

    $kinds = @("Image", "Video", "Gif", "Web")
    for ($i = 0; $i -lt 4; $i++) {
        $kind = $kinds[$i]
        $before = Sample $p.Id
        # first-row card i: card-w 160, gap 12, pad 8, top bar 52, card-h 134;
        # preview button center inside card ~= (81, 123).
        $lx = 8 + $i * (160 + 12) + 81
        $ly = 52 + 8 + 123
        Click-Client $hwnd $lx $ly
        $wait = if ($kind -eq "Web") { $WebOpenWaitSeconds } else { $OpenWaitSeconds }
        Start-Sleep -Seconds $wait
        $open = Sample $p.Id
        Shot $hwnd ("{0}_open_{1}" -f ($i + 1), $kind)
        # close by clicking the backdrop near the top-left corner (outside the dialog).
        Click-Client $hwnd 12 12
        Start-Sleep -Seconds $CloseWaitSeconds
        $after = Sample $p.Id
        Shot $hwnd ("{0}_closed_{1}" -f ($i + 1), $kind)

        Write-Host ("=== {0} ===" -f $kind)
        $row = [pscustomobject]@{
            Kind        = $kind
            BeforePriv  = $before.PrivateMB
            BeforeWS    = $before.WSMB
            OpenPriv    = $open.PrivateMB
            OpenWS      = $open.WSMB
            ClosedPriv  = $after.PrivateMB
            ClosedWS    = $after.WSMB
            MpvBefore   = $before.Mpv
            MpvOpen     = $open.Mpv
            MpvClosed   = $after.Mpv
            Wv2Before   = $before.WebView2
            Wv2Open     = $open.WebView2
            Wv2Closed   = $after.WebView2
        }
        $results += $row
        Write-Host ($row | Format-List | Out-String)
    }
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
    Start-Sleep -Milliseconds 800
    $left = (Get-Process mpv, msedgewebview2 -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-Host ("leftover mpv+webview2 after kill: {0}" -f $left)
}

Write-Host "===== SUMMARY ====="
$results | Format-Table -AutoSize | Out-String | Write-Host
