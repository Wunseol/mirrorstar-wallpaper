# Task 9.9.4 single-instance check (ASCII-only).
# Starts the Tauri shell (native UI architecture), then starts a SECOND copy and verifies the
# second copy is refused by the named-mutex guard (it shows a "提示" message box and does
# not proceed). The message box is modal, so the harness detects it and then kills it.
#
# Usage:
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File single_instance_check.ps1

$ErrorActionPreference = "Continue"
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent (Split-Path -Parent $here)
$appExe = Join-Path $root "target\release\mirrorstar-wallpaper.exe"
if (-not (Test-Path $appExe)) { throw "missing $appExe" }

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class SI {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
}
"@

function Start-App([hashtable]$env) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $appExe; $psi.UseShellExecute = $false; $psi.WorkingDirectory = $root
    foreach ($k in $env.Keys) { $psi.EnvironmentVariables[$k] = $env[$k] }
    return [System.Diagnostics.Process]::Start($psi)
}

# Visible top-level window titles owned by a pid.
function Get-Titles([int]$procId) {
    $titles = New-Object System.Collections.ArrayList
    $target = [uint32]$procId
    $cb = [SI+EnumProc] {
        param($h, $l)
        $wpid = [uint32]0
        [void][SI]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $target -and [SI]::IsWindowVisible($h)) {
            $sb = New-Object System.Text.StringBuilder 256
            [void][SI]::GetWindowText($h, $sb, 256)
            $t = $sb.ToString()
            if ($t.Length -gt 0) { [void]$titles.Add($t) }
        }
        return $true
    }
    [void][SI]::EnumWindows($cb, [IntPtr]::Zero)
    return , $titles.ToArray()
}

Get-Process mirrorstar-wallpaper -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

Write-Host "starting instance A ..."
$a = Start-App @{ RUST_LOG = "error" }
Start-Sleep -Seconds 6
Write-Host ("A alive: {0}  A window titles: [{1}]" -f (-not $a.HasExited), ((Get-Titles $a.Id) -join ' | '))

Write-Host "starting instance B (must be refused) ..."
$b = Start-App @{ RUST_LOG = "error" }
Start-Sleep -Seconds 5
$bAlive = -not $b.HasExited
$bTitles = Get-Titles $b.Id
Write-Host ("B alive: {0}  B window titles: [{1}]" -f $bAlive, ($bTitles -join ' | '))

# Expected message-box title is U+63D0 U+793A ("ti shi"). Built from code points so this
# source file stays pure ASCII (PS 5.1 would misdecode a raw CJK literal as GBK).
$expectedTitle = ([string][char]0x63D0) + ([string][char]0x793A)
$guardFired = ($bTitles -contains $expectedTitle)
Write-Host ("SINGLE_INSTANCE_GUARD_FIRED: {0}" -f $guardFired)
Write-Host ("instance A still alive: {0}" -f (-not $a.HasExited))

# cleanup
foreach ($p in @($a, $b)) { if ($p -and -not $p.HasExited) { try { $p.Kill() } catch {} } }
Get-Process mirrorstar-wallpaper -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Write-Host ("leftover mirrorstar-wallpaper: {0}" -f @(Get-Process mirrorstar-wallpaper -ErrorAction SilentlyContinue).Count)
Write-Host "=== done ==="
