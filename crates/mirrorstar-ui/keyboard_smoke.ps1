# Task 9.7 GUI smoke: keyboard reachability of the native UI grid.
#
# ASCII-only on purpose: PowerShell 5.1 decodes BOM-less UTF-8 as GBK and breaks on CJK.
#
# What it automates (real window + real injected input):
#   * boots target\release\mirrorstar-ui.exe, answers the 9 snapshot requests with synthetic
#     wallpapers whose FIRST card is a Gif backed by a real .gif file,
#   * sends Control::Show, foregrounds the window, sets the cursor over it,
#   * injects Tab / arrows / Enter / Esc / Delete with keybd_event(),
#   * captures a screenshot after each key and captures the child stderr (its log stream),
#   * ASSERTS: the process stays alive, the window stays visible, and ENTER (after Tab) made
#     the focused card open a preview (a "[mirrorstar-ui] GIF" log line appears).
#
# What it CANNOT automate (see the MANUAL CHECKLIST printed at the end):
#   * the focus-ring / drag-hover / drag-ghost VISUALS need a human eye,
#   * a real OLE file drag (hover highlight + ghost + neighbour let-way) needs an actual
#     drag source; keybd_event/mouse_event cannot synthesise IDataObject drag-and-drop.
#     Those rows are reported as MANUAL, never as PASS.
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File keyboard_smoke.ps1 `
#       -Items 60 -IdleSeconds 3

param(
    [int]$Items = 60,
    [int]$IdleSeconds = 3
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
using System.Text;
using System.Runtime.InteropServices;
public static class SMOKE {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint uCode, uint uMapType);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
}
"@

# ---- synthetic assets -------------------------------------------------------
$imgDir = Join-Path $env:TEMP "mirrorstar-ui-smoke"
New-Item -ItemType Directory -Force -Path $imgDir | Out-Null
$shotDir = Join-Path $imgDir "shots"
New-Item -ItemType Directory -Force -Path $shotDir | Out-Null
$errLog = Join-Path $imgDir "ui_stderr.log"
if (Test-Path $errLog) { Remove-Item -LiteralPath $errLog -Force }

# A real single-frame GIF so card 0 (Gif type) can actually be previewed: a successful or
# failed GIF preview both reach the same log site, so either outcome proves Enter wired up.
$gif0 = Join-Path $imgDir "card0.gif"
if (-not (Test-Path $gif0)) {
    $bmp = New-Object System.Drawing.Bitmap 320, 180
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 200, 60, 60))
    $g.Dispose()
    $bmp.Save($gif0, [System.Drawing.Imaging.ImageFormat]::Gif)
    $bmp.Dispose()
}
$thumbJpg = Join-Path $imgDir "thumb.jpg"
if (-not (Test-Path $thumbJpg)) {
    $bmp = New-Object System.Drawing.Bitmap 320, 180
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 40, 120, 200))
    $g.Dispose()
    $bmp.Save($thumbJpg, [System.Drawing.Imaging.ImageFormat]::Jpeg)
    $bmp.Dispose()
}

function New-Wallpapers([int]$count) {
    $list = New-Object System.Collections.ArrayList
    for ($i = 0; $i -lt $count; $i++) {
        $entry = [ordered]@{
            id             = "w$i"
            file_path      = $thumbJpg
            wallpaper_type = "Image"
            added_at       = "1700000000"
            thumbnail      = $thumbJpg
        }
        if ($i -eq 0) {
            # Focused first card is a Gif backed by a real file -> Enter must open a preview.
            $entry.wallpaper_type = "Gif"
            $entry.thumbnail = ""
            $entry.file_path = $gif0
        }
        [void]$list.Add([pscustomobject]$entry)
    }
    return , $list.ToArray()
}

function Find-UiWindow([int]$procId) {
    $script:bestH = [IntPtr]::Zero
    $script:bestArea = 0
    $target = [uint32]$procId
    $cb = [SMOKE+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][SMOKE]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [SMOKE]::IsWindowVisible($h)) {
            $r = New-Object SMOKE+RECT
            [void][SMOKE]::GetWindowRect($h, [ref]$r)
            $area = ($r.R - $r.L) * ($r.B - $r.T)
            if ($area -gt $script:bestArea) { $script:bestArea = $area; $script:bestH = $h }
        }
        return $true
    }
    [void][SMOKE]::EnumWindows($cb, [IntPtr]::Zero)
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

# Virtual-key codes (ASCII comment, no CJK literals).
$VK_TAB = 0x09; $VK_RETURN = 0x0D; $VK_ESC = 0x1B; $VK_DELETE = 0x2E
$VK_LEFT = 0x25; $VK_UP = 0x26; $VK_RIGHT = 0x27; $VK_DOWN = 0x28
$KEYUP = 2
$EXTENDED = 1

# Inject a key tap. Faithful hardware-ish input: derive the real scan code via
# MapVirtualKey and set KEYEVENTF_EXTENDEDKEY for the extended keys (Delete and the
# arrow cluster). Without the extended flag a synthetic VK_DELETE is delivered as a
# key that winit cannot map to NamedKey::Delete, so it is dropped and never reaches
# the app (this was observed: Delete did nothing while Tab/arrows/Enter worked).
function Tap-Key([byte]$vk, [bool]$extended = $false) {
    $scan = [byte]([SMOKE]::MapVirtualKey([uint32]$vk, 0) -band 0xFF)
    $down = if ($extended) { $EXTENDED } else { 0 }
    [SMOKE]::keybd_event($vk, $scan, [uint32]$down, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 40
    [SMOKE]::keybd_event($vk, $scan, [uint32]($down -bor $KEYUP), [IntPtr]::Zero)
    Start-Sleep -Milliseconds 450
}

function Save-Shot([IntPtr]$hwnd, [string]$name) {
    if ($hwnd -eq [IntPtr]::Zero) { return }
    $r = New-Object SMOKE+RECT
    [void][SMOKE]::GetWindowRect($hwnd, [ref]$r)
    if (($r.R - $r.L) -le 0) { return }
    $path = Join-Path $shotDir ("{0}.png" -f $name)
    # Capture the window's OWN content via PrintWindow instead of CopyFromScreen. Screen grabs
    # read the composited desktop, which on a busy host can lag or miss the app's repaint and
    # make a correct state change look like an unchanged frame. PrintWindow renders the window
    # directly, so it reflects the app's current frame regardless of compositor timing.
    # PW_RENDERFULLCONTENT (0x2) is required for non-GDI (e.g. software-rendered) surfaces.
    $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][SMOKE]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

# MD5 of a saved screenshot, or $null when it is missing. Used to assert that a key
# actually changed the rendered frame (overlay appeared / focus moved / overlay closed)
# without needing pixel baselines.
function Shot-Hash([string]$name) {
    $path = Join-Path $shotDir ("{0}.png" -f $name)
    if (-not (Test-Path $path)) { return $null }
    return (Get-FileHash -LiteralPath $path -Algorithm MD5).Hash
}

# Force a window to the foreground. Plain SetForegroundWindow is refused while our own
# process is not the foreground process, so attach our input queue to the current
# foreground thread first (the classic AttachThreadInput workaround), with an ALT-tap
# fallback. Returns $true once GetForegroundWindow() reports the target.
function Force-Foreground([IntPtr]$hwnd) {
    [void][SMOKE]::ShowWindow($hwnd, 9)  # SW_RESTORE
    [void][SMOKE]::BringWindowToTop($hwnd)
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $fg = [SMOKE]::GetForegroundWindow()
        if ($fg -eq $hwnd) { return $true }
        $fgPid = [uint32]0
        $fgThread = [SMOKE]::GetWindowThreadProcessId($fg, [ref]$fgPid)
        $tPid = [uint32]0
        $targetThread = [SMOKE]::GetWindowThreadProcessId($hwnd, [ref]$tPid)
        if ($fgThread -ne 0 -and $fgThread -ne $targetThread) {
            [void][SMOKE]::AttachThreadInput($fgThread, $targetThread, $true)
            [void][SMOKE]::SetForegroundWindow($hwnd)
            [void][SMOKE]::AttachThreadInput($fgThread, $targetThread, $false)
        } else {
            # ALT tap unlocks SetForegroundWindow for the current thread.
            [SMOKE]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
            [SMOKE]::keybd_event(0x12, 0, $KEYUP, [IntPtr]::Zero)
            [void][SMOKE]::SetForegroundWindow($hwnd)
        }
        Start-Sleep -Milliseconds 250
        if ([SMOKE]::GetForegroundWindow() -eq $hwnd) { return $true }
    }
    return ($hwnd -ne [IntPtr]::Zero) -and ([SMOKE]::GetForegroundWindow() -eq $hwnd)
}

$checks = New-Object System.Collections.ArrayList
function Record([string]$name, $ok, [string]$detail) {
    $pass = [bool]$ok
    [void]$checks.Add([pscustomobject]@{ Check = $name; Result = $(if ($pass) { "PASS" } else { "FAIL" }); Detail = $detail })
}

# ---- launch + handshake -----------------------------------------------------
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true
$p = [System.Diagnostics.Process]::Start($psi)

# Capture the child's stderr (its [mirrorstar-ui] log stream) into a file. The event action
# only appends text, so the .NET-Framework scope caveat does not apply here.
$null = Register-ObjectEvent -InputObject $p -EventName ErrorDataReceived -MessageData $errLog -Action {
    if ($EventArgs.Data) { Add-Content -LiteralPath $event.MessageData -Value $EventArgs.Data }
}
$p.BeginErrorReadLine()

$hwnd = [IntPtr]::Zero
$aliveAtEnd = $false
try {
    Send-Json @{ type = 'hello'; protocol_version = 1; capabilities = 15; client = 'keyboard-smoke' }
    $uiHello = Read-Line 10000
    if (-not $uiHello) { throw "no UI hello" }
    Write-Host ("UI hello: {0}" -f $uiHello)

    $wallpapers = New-Wallpapers $Items
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
        elseif ($cmd -eq 'get_wallpaper_state') { $data = $null }
        else { $data = $null }
        Send-Json @{ type = 'response'; request_id = $msg.request_id; status = 'ok'; data = $data }
        if (-not $answered.ContainsKey($cmd)) { $answered[$cmd] = $true }
    }
    Write-Host ("answered: {0}" -f (($answered.Keys | Sort-Object) -join ','))

    Send-Json @{ type = 'control'; action = 'show' }
    Start-Sleep -Seconds 2
    $hwnd = Find-UiWindow $p.Id
    Write-Host ("window hwnd: {0}" -f $hwnd)
    if ($hwnd -eq [IntPtr]::Zero) { throw "UI window not found for pid $($p.Id)" }

    # Foreground + hover the cursor over the grid so injected keys reach the window.
    $fgOk = Force-Foreground $hwnd
    Write-Host ("foreground == ui window: {0}" -f $fgOk)
    $r = New-Object SMOKE+RECT
    [void][SMOKE]::GetWindowRect($hwnd, [ref]$r)
    [void][SMOKE]::SetCursorPos(($r.L + [int](($r.R - $r.L) * 0.35)), ($r.T + [int](($r.B - $r.T) * 0.55)))
    Start-Sleep -Milliseconds 300
    $script:fgForKeys = $fgOk

    Start-Sleep -Seconds $IdleSeconds
    Save-Shot $hwnd "00_initial"

    # Tab -> focus first card.
    Tap-Key $VK_TAB
    Save-Shot $hwnd "01_after_tab"
    # Enter -> open preview of the focused (Gif) card.
    Tap-Key $VK_RETURN
    Save-Shot $hwnd "02_after_enter_preview"
    # Esc -> dismiss preview.
    Tap-Key $VK_ESC
    Save-Shot $hwnd "03_after_esc"
    # Arrow navigation across the grid (arrows are extended keys).
    Tap-Key $VK_RIGHT $true; Tap-Key $VK_RIGHT $true; Tap-Key $VK_DOWN $true; Tap-Key $VK_LEFT $true; Tap-Key $VK_UP $true
    Save-Shot $hwnd "04_after_arrows"
    # Delete -> confirmation overlay (extended key).
    Tap-Key $VK_DELETE $true
    Save-Shot $hwnd "05_after_delete"
    # Esc -> cancel confirmation.
    Tap-Key $VK_ESC
    Save-Shot $hwnd "06_after_esc_cancel"

    # Capture end-state BEFORE any kill.
    $aliveAtEnd = -not $p.HasExited
    $visibleAtEnd = ($hwnd -ne [IntPtr]::Zero) -and [SMOKE]::IsWindowVisible($hwnd)
}
catch {
    Write-Host ("ERROR: " + $_)
    Write-Host ($_.ScriptStackTrace)
}
finally {
    # Give the async stderr handler a moment to flush, then stop it.
    Start-Sleep -Milliseconds 600
    Get-EventSubscriber -ErrorAction SilentlyContinue | Where-Object { $_.SourceObject -eq $p } | Unregister-Event -ErrorAction SilentlyContinue
}

# ---- assertions -------------------------------------------------------------
Record "window focused for key injection" $script:fgForKeys "GetForegroundWindow()==ui window"
Record "process alive through key sequence" $aliveAtEnd "alive before teardown"
Record "window still exists and visible" $visibleAtEnd "hwnd=$hwnd"

$logText = ""
if (Test-Path $errLog) { $logText = Get-Content -LiteralPath $errLog -Raw -ErrorAction SilentlyContinue }
if ($null -eq $logText) { $logText = "" }
# After Tab+Enter the focused Gif card must reach preview::open, which logs
# "[mirrorstar-ui] GIF ..." on either success or failure.
$previewFired = $logText -match '\[mirrorstar-ui\] GIF'
Record "ENTER on focused card opened a preview" $previewFired "stderr contains '[mirrorstar-ui] GIF'"

# Frame-diff assertions: each key must visibly change (or restore) the render. The
# screenshots are the same window rect, so equal MD5 == identical pixels.
$h00 = Shot-Hash "00_initial"
$h01 = Shot-Hash "01_after_tab"
$h02 = Shot-Hash "02_after_enter_preview"
$h03 = Shot-Hash "03_after_esc"
$h04 = Shot-Hash "04_after_arrows"
$h05 = Shot-Hash "05_after_delete"
$h06 = Shot-Hash "06_after_esc_cancel"
$shotsOk = @($h00, $h01, $h02, $h03, $h04, $h05, $h06) -notcontains $null
Record "all 7 screenshots captured" $shotsOk "shots dir: $shotDir"
if ($shotsOk) {
    Record "TAB moved focus (frame changed)" ($h01 -ne $h00) "00 != 01"
    Record "ENTER opened preview (frame changed)" ($h02 -ne $h01) "01 != 02"
    Record "ESC closed preview (frame restored)" ($h03 -eq $h01) "01 == 03"
    Record "ARROWS moved focus (frame changed)" ($h04 -ne $h03) "03 != 04"
    Record "DELETE opened confirm overlay (frame changed)" ($h05 -ne $h04) "04 != 05"
    Record "ESC cancelled confirm (frame restored)" ($h06 -eq $h04) "04 == 06"
}

# Teardown now that every observation has been captured.
try { $p.Kill() } catch {}

Write-Host ""
Write-Host "===== CHILD LOG TAIL ====="
if ($logText.Length -gt 1800) { Write-Host ($logText.Substring($logText.Length - 1800)) } else { Write-Host $logText }

Write-Host ""
Write-Host "===== AUTOMATED CHECKS ====="
$checks | Format-Table -AutoSize | Out-String | Write-Host

Write-Host "===== MANUAL CHECKLIST (not automatable here) ====="
Write-Host ("Screenshots (open each and eyeball): {0}" -f $shotDir)
Write-Host "  01_after_tab.png           : focused card shows a yellow (#ffd54f) 2px focus ring"
Write-Host "  02_after_enter_preview.png : preview overlay is on screen"
Write-Host "  03_after_esc.png           : preview closed; focus ring still on the same card"
Write-Host "  04_after_arrows.png        : focus ring moved right/right/down/left/up (and scrolled into view if needed)"
Write-Host "  05_after_delete.png        : delete confirmation overlay is on screen"
Write-Host "  06_after_esc_cancel.png    : confirmation dismissed, nothing deleted"
Write-Host "Drag-in hover + drag ghost + neighbour let-way CANNOT be automated here:"
Write-Host "  a real OLE drag needs an IDataObject drag source; keybd_event/mouse_event cannot"
Write-Host "  synthesise it. Manual steps:"
Write-Host "    1) drag a local image/video file from Explorer over the grid -> overlay"
Write-Host "       'drop to add wallpaper' highlight (#4fc3f7 border) must appear; it must clear"
Write-Host "       when the cursor leaves the window without dropping."
Write-Host "    2) drag a card within the grid -> a drag ghost follows the cursor, the source card"
Write-Host "       fades (opacity 0.35), and neighbours slide aside to open the drop slot."
Write-Host "    3) release -> the order updates; release outside -> cards snap back."

$failed = @($checks | Where-Object { $_.Result -eq "FAIL" })
if ($failed.Count -gt 0) { Write-Host ""; Write-Host ("RESULT: {0} automated check(s) FAILED" -f $failed.Count); exit 1 }
Write-Host ""
Write-Host "RESULT: all automated checks PASSED (visual + drag items are MANUAL)"
exit 0
