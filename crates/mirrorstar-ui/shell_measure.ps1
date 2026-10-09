# Task 9.2 / 9.3 acceptance harness: Tauri shell + native UI child.
#
# ASCII-only on purpose: PowerShell 5.1 decodes BOM-less UTF-8 as GBK and can break CJK.
#
# Launches the already-built target\release\mirrorstar-wallpaper.exe (no WebView2 main
# window is created under the native-UI architecture) and, for the "open" phase, sets
# MIRRORSTAR_TEST_AUTOSHOW_MAIN_WINDOW=1 so the shell spawns the native mirrorstar-ui.exe
# child and shows its window. Measures Private/WS for: shell-only, UI child, tree total,
# plus msedgewebview2 / mpv / wp-proc counts. Also samples idle + wheel-scroll CPU.
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File shell_measure.ps1 `
#       -IdleSeconds 20 -CpuSampleSeconds 8 -ScrollMs 8000

param(
    [int]$IdleSeconds = 20,
    [int]$CpuSampleSeconds = 8,
    [int]$ScrollMs = 8000,
    [string]$DataRoot = "",
    [int]$IdleTimeoutSecs = 15
)

$ErrorActionPreference = "Continue"
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent (Split-Path -Parent $here)
$binDir = Join-Path $root "target\release"
$appExe = Join-Path $binDir "mirrorstar-wallpaper.exe"
if (-not (Test-Path $appExe)) { throw "missing $appExe" }
if ([string]::IsNullOrEmpty($DataRoot)) { $DataRoot = Join-Path $binDir "baseline-data" }

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class MS {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
"@

$script:preexistingWv = @(Get-Process msedgewebview2 -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$script:preexistingMpv = @(Get-Process mpv -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$script:preexistingUi = @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })

function Get-TreeIds([int]$rootPid) {
    $all = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue
    $ids = New-Object System.Collections.Generic.List[int]
    $ids.Add($rootPid)
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($p in $all) {
            if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) {
                $ids.Add([int]$p.ProcessId); $changed = $true
            }
        }
    }
    return $ids
}

function Mb($bytes) { return [math]::Round($bytes / 1MB, 1) }

function Measure-Shell([int]$rootPid) {
    $ids = Get-TreeIds $rootPid
    $procs = @(Get-Process -Id $ids -ErrorAction SilentlyContinue)
    $shell = @($procs | Where-Object { $_.ProcessName -eq 'mirrorstar-wallpaper' })
    $ui = @($procs | Where-Object { $_.ProcessName -eq 'mirrorstar-ui' })
    $treePriv = ($procs | Measure-Object PrivateMemorySize64 -Sum).Sum
    $treeWs = ($procs | Measure-Object WorkingSet64 -Sum).Sum
    $shellPriv = ($shell | Measure-Object PrivateMemorySize64 -Sum).Sum
    $shellWs = ($shell | Measure-Object WorkingSet64 -Sum).Sum
    $uiPriv = ($ui | Measure-Object PrivateMemorySize64 -Sum).Sum
    $uiWs = ($ui | Measure-Object WorkingSet64 -Sum).Sum
    $names = ($procs | Group-Object ProcessName | ForEach-Object { "$($_.Name)x$($_.Count)" }) -join ' '
    $wv = @(Get-Process msedgewebview2 -ErrorAction SilentlyContinue | Where-Object { $ids.Contains([int]$_.Id) })
    $mpv = @(Get-Process mpv -ErrorAction SilentlyContinue | Where-Object { $ids.Contains([int]$_.Id) })
    $wpp = @(Get-Process mirrorstar-wp-proc -ErrorAction SilentlyContinue)
    return [pscustomobject]@{
        Procs       = $procs.Count
        ShellPriv   = (Mb $shellPriv)
        ShellWs     = (Mb $shellWs)
        UiPriv      = (Mb $uiPriv)
        UiWs        = (Mb $uiWs)
        UiCount     = $ui.Count
        TotalPriv   = (Mb $treePriv)
        TotalWs     = (Mb $treeWs)
        Wv2         = $wv.Count
        Mpv         = $mpv.Count
        WpProc      = $wpp.Count
        Members     = $names
    }
}

# Largest visible top-level window owned by the given pid (the Slint native main window).
function Get-UiWindow([int]$procId) {
    $script:bestH = [IntPtr]::Zero
    $script:bestArea = 0
    $target = [uint32]$procId
    $cb = [MS+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][MS]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [MS]::IsWindowVisible($h)) {
            $r = New-Object MS+RECT
            [void][MS]::GetWindowRect($h, [ref]$r)
            $area = ($r.R - $r.L) * ($r.B - $r.T)
            if ($area -gt $script:bestArea) { $script:bestArea = $area; $script:bestH = $h }
        }
        return $true
    }
    [void][MS]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:bestH
}

function Get-UiWindowTitle([IntPtr]$h) {
    if ($h -eq [IntPtr]::Zero) { return "" }
    $sb = New-Object System.Text.StringBuilder 256
    [void][MS]::GetWindowText($h, $sb, 256)
    return $sb.ToString()
}

function Invoke-Wheel([IntPtr]$hwnd, [int]$milliseconds) {
    $r = New-Object MS+RECT
    [void][MS]::GetWindowRect($hwnd, [ref]$r)
    $cx = $r.L + [int](($r.R - $r.L) * 0.35)
    $cy = $r.T + [int](($r.B - $r.T) * 0.55)
    [void][MS]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 300
    [void][MS]::SetCursorPos($cx, $cy)
    Start-Sleep -Milliseconds 200
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $dir = -1; $ticks = 0
    while ($sw.Elapsed.TotalMilliseconds -lt $milliseconds) {
        [MS]::mouse_event(0x0800, 0, 0, ($dir * 120), [IntPtr]::Zero)
        Start-Sleep -Milliseconds 30
        $ticks++
        if ($ticks % 25 -eq 0) { $dir = -$dir }
    }
    $sw.Stop()
}

function Get-CpuSnapshot([int[]]$ids) {
    $snap = @{}
    foreach ($p in (Get-Process -Id $ids -ErrorAction SilentlyContinue)) { $snap[[int]$p.Id] = $p.TotalProcessorTime.TotalSeconds }
    return $snap
}

function Cpu-Sum($a, $b) {
    $sum = 0.0
    foreach ($id in $a.Keys) { if ($b.ContainsKey($id)) { $sum += ($b[$id] - $a[$id]) } }
    return $sum
}

function Wait-NoUi {
    for ($i = 0; $i -lt 40; $i++) {
        if (@(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue).Count -eq 0) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

function Clear-Residual {
    Stop-Process -Name mirrorstar-wallpaper -Force -ErrorAction SilentlyContinue
    Stop-Process -Name mirrorstar-ui -Force -ErrorAction SilentlyContinue
    Stop-Process -Name mirrorstar-wp-proc -Force -ErrorAction SilentlyContinue
    foreach ($p in @(Get-Process msedgewebview2 -ErrorAction SilentlyContinue)) {
        if ($script:preexistingWv -notcontains $p.Id) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    }
    foreach ($p in @(Get-Process mpv -ErrorAction SilentlyContinue)) {
        if ($script:preexistingMpv -notcontains $p.Id) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    }
    Start-Sleep -Seconds 1
}

function Start-App([hashtable]$env) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $appExe
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = $root
    foreach ($k in $env.Keys) { $psi.EnvironmentVariables[$k] = $env[$k] }
    return [System.Diagnostics.Process]::Start($psi)
}

function Stop-Tree([System.Diagnostics.Process]$proc) {
    if ($null -eq $proc) { return }
    foreach ($id in (Get-TreeIds $proc.Id)) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
}

Write-Host "================================================================"
Write-Host "Task9 shell acceptance  IdleSeconds=$IdleSeconds CpuSample=$CpuSampleSeconds ScrollMs=$ScrollMs"
Write-Host "appExe=$appExe"
Write-Host "dataRoot=$DataRoot timeout=$IdleTimeoutSecs"
Write-Host "================================================================"

Clear-Residual

# ---- Phase 0: shell only, NO autoshow => baseline ("before open") --------
Write-Host ""
Write-Host "=== [BEFORE] shell only (no autoshow) ==="
$pB = Start-App @{ MIRRORSTAR_DATA_ROOT = $DataRoot; RUST_LOG = "error" }
Start-Sleep -Seconds $IdleSeconds
if ($pB.HasExited) { Write-Host "[BEFORE] FAILED exited code=$($pB.ExitCode)" }
else {
    $mB = Measure-Shell $pB.Id
    Write-Host ("[BEFORE] procs={0} shellPriv={1}MB shellWs={2}MB uiPriv={3}MB uiCount={4} totalPriv={5}MB totalWs={6}MB wv2={7} mpv={8} wp={9}" -f $mB.Procs,$mB.ShellPriv,$mB.ShellWs,$mB.UiPriv,$mB.UiCount,$mB.TotalPriv,$mB.TotalWs,$mB.Wv2,$mB.Mpv,$mB.WpProc)
    Write-Host ("[BEFORE] members: {0}" -f $mB.Members)
}
Stop-Tree $pB; Clear-Residual

# ---- Phase 1: shell + autoshow => open ("open, settled") + CPU ----------
Write-Host ""
Write-Host "=== [OPEN] shell + autoshow (native UI child) ==="
$pO = Start-App @{ MIRRORSTAR_TEST_AUTOSHOW_MAIN_WINDOW = "1"; MIRRORSTAR_DATA_ROOT = $DataRoot; MIRRORSTAR_UI_IDLE_TIMEOUT_SECS = "$IdleTimeoutSecs"; RUST_LOG = "error" }

# wait for the UI child process
$uiFound = $false
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 500
    if (@(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue).Count -gt 0) { $uiFound = $true; break }
}
Write-Host "[OPEN] ui child found: $uiFound"
Start-Sleep -Seconds $IdleSeconds

$mO = Measure-Shell $pO.Id
$uiProc = @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue | Sort-Object Id)[0]
Write-Host ("[OPEN] procs={0} shellPriv={1}MB shellWs={2}MB uiPriv={3}MB uiWs={4}MB uiCount={5} totalPriv={6}MB totalWs={7}MB wv2={8} mpv={9} wp={10}" -f $mO.Procs,$mO.ShellPriv,$mO.ShellWs,$mO.UiPriv,$mO.UiWs,$mO.UiCount,$mO.TotalPriv,$mO.TotalWs,$mO.Wv2,$mO.Mpv,$mO.WpProc)
Write-Host ("[OPEN] members: {0}" -f $mO.Members)

# window title / focus check (Task 9.4 fullscreen-detection exclusion)
$uiHwnd = [IntPtr]::Zero
if ($uiProc) { $uiHwnd = Get-UiWindow $uiProc.Id }
$uiTitle = Get-UiWindowTitle $uiHwnd
$fg = [MS]::GetForegroundWindow()
Write-Host ("[OPEN] uiHwnd={0} title='{1}' fgIsUiWindow={2}" -f $uiHwnd.ToInt64(), $uiTitle, ($fg -eq $uiHwnd))

# CPU: idle (whole tree) then scroll (tree)
if (-not $pO.HasExited) {
    $treeIds = Get-TreeIds $pO.Id
    $uiIds = @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })

    $a = Get-CpuSnapshot $treeIds
    $ua = Get-CpuSnapshot $uiIds
    Start-Sleep -Seconds $CpuSampleSeconds
    $b = Get-CpuSnapshot $treeIds
    $ub = Get-CpuSnapshot $uiIds
    $idleTree = [math]::Round(($(Cpu-Sum $a $b) / $CpuSampleSeconds) * 100, 1)
    $idleUi = [math]::Round(($(Cpu-Sum $ua $ub) / $CpuSampleSeconds) * 100, 1)
    Write-Host "[OPEN] CPU idle  tree=$idleTree %  ui=$idleUi %"

    if ($uiHwnd -ne [IntPtr]::Zero) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $s0 = Get-CpuSnapshot $treeIds
        $us0 = Get-CpuSnapshot $uiIds
        Invoke-Wheel $uiHwnd $ScrollMs
        $s1 = Get-CpuSnapshot $treeIds
        $us1 = Get-CpuSnapshot $uiIds
        $sw.Stop()
        $eff = [math]::Max(1.0, $sw.Elapsed.TotalMilliseconds - 500.0)
        $scrollTree = [math]::Round(($(Cpu-Sum $s0 $s1) * 100000.0 / $eff), 1)
        $scrollUi = [math]::Round(($(Cpu-Sum $us0 $us1) * 100000.0 / $eff), 1)
        Write-Host "[OPEN] CPU scroll tree=$scrollTree %  ui=$scrollUi %"
    } else {
        Write-Host "[OPEN] CPU scroll: UI window not found; skipped"
    }
    # memory after scroll
    Start-Sleep -Seconds 2
    $mO2 = Measure-Shell $pO.Id
    Write-Host ("[OPEN] after-scroll totalPriv={0}MB uiPriv={1}MB" -f $mO2.TotalPriv,$mO2.UiPriv)
}

# ---- Phase 2: close main window (WM_CLOSE on UI window) -> idle exit -----
Write-Host ""
Write-Host "=== [CLOSED] WM_CLOSE UI window, wait idle timeout (${IdleTimeoutSecs}s) + tick ==="
$uiProc2 = @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue | Sort-Object Id)[0]
if ($uiProc2) {
    $h2 = Get-UiWindow $uiProc2.Id
    Write-Host "[CLOSED] posting WM_CLOSE to $($h2.ToInt64())"
    if ($h2 -ne [IntPtr]::Zero) { [void][MS]::PostMessage($h2, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) }
    Start-Sleep -Seconds 3
    $stillUi = @(Get-Process mirrorstar-ui -ErrorAction SilentlyContinue).Count
    Write-Host "[CLOSED] ui alive 3s after WM_CLOSE: $stillUi"
    # idle tick is 30s; wait up to 45s for the shell to reclaim the UI process
    $reclaimed = Wait-NoUi
    Write-Host "[CLOSED] ui reclaimed by shell idle: $reclaimed"
}
Start-Sleep -Seconds 3
if ($pO.HasExited) { Write-Host "[CLOSED] shell exited (unexpected)" }
else {
    $mC = Measure-Shell $pO.Id
    Write-Host ("[CLOSED] procs={0} shellPriv={1}MB shellWs={2}MB uiPriv={3}MB uiCount={4} totalPriv={5}MB totalWs={6}MB wv2={7} mpv={8} wp={9}" -f $mC.Procs,$mC.ShellPriv,$mC.ShellWs,$mC.UiPriv,$mC.UiCount,$mC.TotalPriv,$mC.TotalWs,$mC.Wv2,$mC.Mpv,$mC.WpProc)
    Write-Host ("[CLOSED] members: {0}" -f $mC.Members)
}
Stop-Tree $pO; Clear-Residual

Write-Host ""
Write-Host "=== cleanup ==="
foreach ($n in @("mirrorstar-wallpaper","mirrorstar-ui","mirrorstar-wp-proc","msedgewebview2","mpv")) {
    Write-Host ("[cleanup] {0} remaining = {1}" -f $n, @(Get-Process -Name $n -ErrorAction SilentlyContinue).Count)
}
Write-Host "=== done ==="
