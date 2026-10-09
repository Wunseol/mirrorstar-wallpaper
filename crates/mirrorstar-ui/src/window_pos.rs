//! 主窗口居中显示。
//!
//! ## 背景
//!
//! `MainWindow` 只声明了 `preferred-width/height` 与 `min-width/height`，**从未有任何定位代码**，
//! 窗口因此落在 Slint/winit 的默认位置（屏幕左上角）。本模块在每次显示前，把窗口居中到
//! **当前鼠标所在显示器的工作区**（物理像素）。
//!
//! ## 分离设计：纯函数 + Win32 入口
//!
//! - [`center_in`] 是**纯函数**：只看工作区矩形与窗口尺寸，算出左上角坐标。它不依赖任何
//!   Win32 类型，因此可在**任意平台**单测（覆盖居中/超大贴左上/相等/负坐标副屏等分支）。
//! - [`center_on_cursor_monitor`] 是 `#[cfg(windows)]` 的入口：取 HWND、查鼠标所在显示器、
//!   读工作区与外框尺寸、调用 [`center_in`]，再用 `SetWindowPos` 移动窗口。
//!
//! ## 不变量
//!
//! - **不抢前台**：`SetWindowPos` 带 `SWP_NOACTIVATE`（且 `SWP_NOZORDER`），全程不调用
//!   `SetForegroundWindow` / `SetActiveWindow`。
//! - **只移动不改尺寸**：`SetWindowPos` 带 `SWP_NOSIZE`，不影响 `min-*` 约束与网格列数联动。
//! - **best-effort**：任一步失败只记日志并跳过，绝不 panic、不阻断窗口显示。

#[cfg(windows)]
use crate::protocol::log;

/// 计算窗口在指定工作区内的左上角坐标（纯函数，跨平台可测）。
///
/// `work` = `(left, top, width, height)` 物理像素工作区；`size` = `(窗口宽, 窗口高)`。
/// 窗口小于工作区时居中；窗口大于工作区时贴工作区左上（差值夹取到非负，避免负偏移越界）。
/// 结果基于 `work` 原点，副屏原点为负时结果同样可为负。
pub fn center_in(work: (i32, i32, i32, i32), size: (i32, i32)) -> (i32, i32) {
    let (left, top, work_w, work_h) = work;
    let (win_w, win_h) = size;
    (
        left + (work_w - win_w).max(0) / 2,
        top + (work_h - win_h).max(0) / 2,
    )
}

/// 将窗口居中到当前鼠标所在显示器的工作区（仅 Windows；best-effort，不阻断显示）。
///
/// 步骤：取 HWND → `GetCursorPos` → `MonitorFromPoint(MONITOR_DEFAULTTONEAREST)` →
/// `GetMonitorInfoW` 取 `rcWork` → `GetWindowRect` 取外框尺寸 → [`center_in`] →
/// `SetWindowPos(SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)`。
#[cfg(windows)]
pub fn center_on_cursor_monitor(window: &slint::Window) {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    // 复用拖入路径的 HWND 提取（raw-window-handle 0.6）；句柄可能延迟，故显式判空。
    let Some(hwnd) = crate::drop_target::hwnd_of(window) else {
        log("居中：未获取到主窗口 HWND，跳过");
        return;
    };

    // SAFETY: 句柄来自 Slint 窗口且本函数在 UI 线程调用；各 Win32 调用均为只读查询或
    // 带 `SWP_NOACTIVATE | SWP_NOSIZE` 的位置移动，不改变可见性/焦点/尺寸。
    unsafe {
        let mut pt = POINT::default();
        if let Err(error) = GetCursorPos(&mut pt) {
            log(format!("居中：GetCursorPos 失败，跳过：{error}"));
            return;
        }

        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            log("居中：MonitorFromPoint 未返回有效显示器，跳过");
            return;
        }

        // `MONITORINFO` 必须先填 `cbSize`，否则 `GetMonitorInfoW` 会因结构体大小不匹配而失败。
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            log("居中：GetMonitorInfoW 失败，跳过");
            return;
        }

        let mut wr = RECT::default();
        if let Err(error) = GetWindowRect(hwnd, &mut wr) {
            log(format!("居中：GetWindowRect 失败，跳过：{error}"));
            return;
        }

        let work = (
            mi.rcWork.left,
            mi.rcWork.top,
            mi.rcWork.right - mi.rcWork.left,
            mi.rcWork.bottom - mi.rcWork.top,
        );
        let size = (wr.right - wr.left, wr.bottom - wr.top);
        let (x, y) = center_in(work, size);

        // 只移动：`SWP_NOSIZE` 保尺寸，`SWP_NOZORDER | SWP_NOACTIVATE` 不动 Z 序、不抢前台。
        if let Err(error) = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        ) {
            log(format!("居中：SetWindowPos 失败，跳过：{error}"));
            return;
        }

        log(format!(
            "居中：HWND=0x{:x} 工作区=({},{},{}x{})，窗口={}x{} → 落点=({x},{y})",
            hwnd.0 as usize, work.0, work.1, work.2, work.3, size.0, size.1
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::center_in;

    #[test]
    fn center_in_centers_when_window_smaller_than_work() {
        // 1000x800 工作区、400x300 窗口 → 各留一半边距。
        assert_eq!(center_in((0, 0, 1000, 800), (400, 300)), (300, 250));
    }

    #[test]
    fn center_in_pins_to_work_origin_when_window_larger_than_work() {
        // 窗口超过工作区：差值夹取到 0，贴工作区左上（非负偏移，不越界）。
        assert_eq!(center_in((0, 0, 800, 600), (1000, 900)), (0, 0));
    }

    #[test]
    fn center_in_pins_to_work_origin_when_size_equal_to_work() {
        // 恰好相等：偏移为 0，落在工作区左上。
        assert_eq!(center_in((0, 0, 800, 600), (800, 600)), (0, 0));
    }

    #[test]
    fn center_in_rounds_odd_difference_down() {
        // 奇数差整除取整（向零取余数）：601/2=300、501/2=250。
        assert_eq!(center_in((0, 0, 1001, 801), (400, 300)), (300, 250));
    }

    #[test]
    fn center_in_is_relative_to_work_origin_which_may_be_negative() {
        // 副屏（主屏左侧，left<0）：结果基于工作区原点，坐标同样为负。
        assert_eq!(center_in((-1920, 0, 1920, 1080), (960, 640)), (-1440, 220));
    }
}
