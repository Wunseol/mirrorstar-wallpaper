//! 主窗口的显示 / 还原 / 置前聚焦策略。
//!
//! ## 为什么用户主动唤起时要置前聚焦
//!
//! Windows 任务栏按钮的语义是「窗口在前台 → 一次点击最小化；窗口在后台 → 第一次点击只
//! 激活，第二次才最小化」。若主窗口显示时不抢焦点，窗口会停留在后台，任务栏按钮就需要
//! 两次点击才能收起。主流实现（Tauri 官方托盘示例的 `show + set_focus`、Lively Wallpaper
//! 的 `WM SHOW`）都会在用户主动唤起时把主窗口置为前台。
//!
//! 因此本模块的策略是：**用户主动唤起（托盘 / 菜单 / 二次启动）⇒ 置前并聚焦**。
//! 纯净壁纸模式等非用户主动场景仍禁止任何抢前台 API（见 `mirrorstar-core::pure_mode`）。
//!
//! ## 跨进程注意点
//!
//! 壳进程持有托盘图标，主窗口属于 `mirrorstar-ui.exe` 子进程。壳侧在 UI 进程已运行时
//! 按 pid 精确调用 `AllowSetForegroundWindow(ui_pid)`（冷启动时 pid 未知，不授权），
//! 本进程随后调用 `SetForegroundWindow`；若仍被前台锁拒绝，则用 `AttachThreadInput`
//! 临时把本线程输入队列附加到当前前台线程后再置前（附加成功即解除），这是跨进程置前的
//! 标准兜底。二次启动路径由新实例以 `AllowSetForegroundWindow(ASFW_ANY)` 授权（启动者
//! 持有用户输入）。
//!
//! 全屏检测按窗口标题精确排除「镜星壁纸」（`src-tauri/src/platform/fullscreen.rs`），
//! 因此聚焦主窗口不会被误判为全屏应用而暂停壁纸。

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, SetForegroundWindow,
    SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE,
};

// ── 按真实窗口状态分派（托盘左键 Toggle） ───────────────────────────────────

/// 一次窗口动作：由下面的纯决策函数给出，再由 Win32 辅助执行。
///
/// `Show` / `Restore` 需要显示路径（居中 / 图标 / 内容重建）并置前聚焦，`Raise` 置前聚焦、
/// `Hide` 走既有隐藏路径。`Hide` 不改变前台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// 已隐藏 → 显示（居中到鼠标所在显示器）并置前聚焦。
    Show,
    /// 已最小化 → 还原到最小化前的尺寸/位置并置前聚焦。
    Restore,
    /// 已可见 → 置前聚焦，不改几何。
    Raise,
    /// 已可见且未最小化 → 隐藏。
    Hide,
}

/// Show 语义：确保窗口可见（最小化优先还原，已可见仅置前聚焦）。
pub fn decide_show_action(visible: bool, minimized: bool) -> WindowAction {
    if minimized {
        WindowAction::Restore
    } else if !visible {
        WindowAction::Show
    } else {
        WindowAction::Raise
    }
}

/// 托盘切换语义：只按**进程内窗口状态**切换，不查询系统前台。
///
/// 判定有意不使用 `GetForegroundWindow()`：本项目的显示路径由用户主动触发，点击托盘
/// 本身会改变系统前台，等 `Toggle` 经 IPC 抵达 UI 时再查前台必然读到 `false`，
/// 导致可见窗口永远落到 `Raise`、一次点击收不回窗口（Win10/Win11 均复现）。
pub fn decide_toggle_action(visible: bool, minimized: bool) -> WindowAction {
    if minimized {
        WindowAction::Restore
    } else if !visible {
        WindowAction::Show
    } else {
        WindowAction::Hide
    }
}

/// 窗口是否处于最小化状态（只读查询）。
pub fn is_iconic(hwnd: HWND) -> bool {
    // SAFETY: 只读查询，不改变任何窗口状态；`IsIconic` 仅读取 `WS_MINIMIZE` 状态。
    unsafe { IsIconic(hwnd).as_bool() }
}

/// 还原最小化窗口到最小化前的尺寸/位置并激活（`SW_RESTORE`）。
///
/// 仅由用户主动唤起路径调用；纯净模式等禁止抢前台的路径不使用。
pub fn restore_window(hwnd: HWND) {
    // SAFETY: `SW_RESTORE` 还原窗口并按用户主动唤起的预期激活它。
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    crate::protocol::log(format!(
        "还原窗口：HWND=0x{:x} 已用 SW_RESTORE 还原并激活",
        hwnd.0 as usize
    ));
}

/// 不激活地把窗口置到 Z 序顶部（`SWP_NOACTIVATE`，best-effort）。
///
/// 用于聚焦失败时的兜底，保证窗口至少不被其它窗口遮挡。
pub fn raise_no_activate(hwnd: HWND) {
    // SAFETY: 仅调整 Z 序；`SWP_NOMOVE | SWP_NOSIZE` 不改几何，`SWP_NOACTIVATE` 不夺前台。
    let result = unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    match result {
        Ok(()) => crate::protocol::log(format!(
            "置前窗口：HWND=0x{:x} 已用 SWP_NOACTIVATE 置顶（不激活）",
            hwnd.0 as usize
        )),
        Err(error) => crate::protocol::log(format!("置前窗口：SetWindowPos 失败，跳过：{error}")),
    }
}

/// 把窗口置为前台并聚焦；返回是否成功。
///
/// 先直接 `SetForegroundWindow`；被 Windows 前台锁拒绝（跨进程唤起时常见且间歇出现）时，
/// 用 [`attach_and_activate`] 兜底。仍失败时窗口保持可见/置前（由 Show / Restore / Raise
/// 保证），仅缺键盘焦点，不阻断功能。
pub fn activate(hwnd: HWND) -> bool {
    // SAFETY: 只读查询当前前台窗口；随后只改变前台窗口，不修改窗口样式/几何。
    let foreground = unsafe { GetForegroundWindow() };
    let direct = unsafe { SetForegroundWindow(hwnd).as_bool() };
    if direct {
        crate::protocol::log(format!(
            "置前聚焦：HWND=0x{:x} 已置为前台（直接）",
            hwnd.0 as usize
        ));
        return true;
    }

    // SAFETY: 兜底路径内部已处理前台句柄无效 / 前台线程 hung 的情况。
    let activated = unsafe { attach_and_activate(hwnd, foreground) };
    if activated {
        crate::protocol::log(format!(
            "置前聚焦：HWND=0x{:x} 直接调用被拒（前台 HWND=0x{:x}），AttachThreadInput 兜底成功",
            hwnd.0 as usize, foreground.0 as usize
        ));
    } else {
        crate::protocol::log(format!(
            "置前聚焦：HWND=0x{:x} 被前台锁拒绝（前台 HWND=0x{:x}），窗口仍已显示/置前",
            hwnd.0 as usize, foreground.0 as usize
        ));
    }
    activated
}

/// 前台锁兜底：临时把本线程输入队列附加到当前前台线程，再尝试置前。
///
/// 附加成功后 `SetForegroundWindow` 不再受前台锁限制；无论结果如何都立即解除附加，
/// 避免长期共享输入队列。前台线程挂起（hung）时直接放弃，防止 `AttachThreadInput` 阻塞。
///
/// # Safety
///
/// 调用方需保证 `hwnd` 来自本进程窗口；`foreground` 为 `GetForegroundWindow` 的返回值。
unsafe fn attach_and_activate(hwnd: HWND, foreground: HWND) -> bool {
    if foreground.0.is_null() || IsHungAppWindow(foreground).as_bool() {
        return false;
    }
    let foreground_thread = GetWindowThreadProcessId(foreground, None);
    let current_thread = GetCurrentThreadId();
    if foreground_thread == 0 || foreground_thread == current_thread {
        return false;
    }
    if !AttachThreadInput(current_thread, foreground_thread, true).as_bool() {
        return false;
    }
    let activated = SetForegroundWindow(hwnd).as_bool();
    let _ = AttachThreadInput(current_thread, foreground_thread, false);
    activated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_action_restores_minimized_window_first() {
        // 最小化优先：即便 Slint 侧认为可见，也要先还原。
        assert_eq!(decide_show_action(false, true), WindowAction::Restore);
        assert_eq!(decide_show_action(true, true), WindowAction::Restore);
    }

    #[test]
    fn show_action_shows_hidden_window() {
        assert_eq!(decide_show_action(false, false), WindowAction::Show);
    }

    #[test]
    fn show_action_raises_visible_window() {
        assert_eq!(decide_show_action(true, false), WindowAction::Raise);
    }

    #[test]
    fn toggle_action_restores_minimized_window_regardless_of_visibility() {
        // 最小化优先：即便 Slint 侧认为可见，也要先还原而不是隐藏。
        assert_eq!(decide_toggle_action(true, true), WindowAction::Restore);
        assert_eq!(decide_toggle_action(false, true), WindowAction::Restore);
    }

    #[test]
    fn toggle_action_shows_hidden_window() {
        assert_eq!(decide_toggle_action(false, false), WindowAction::Show);
    }

    #[test]
    fn toggle_action_hides_visible_window() {
        // 纯可见性切换：只要可见且未最小化就隐藏，不再区分前台/后台。
        assert_eq!(decide_toggle_action(true, false), WindowAction::Hide);
    }
}
