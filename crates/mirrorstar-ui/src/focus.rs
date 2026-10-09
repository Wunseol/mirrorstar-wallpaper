//! Task 9.10.2：原生 UI 主窗口的**真正不激活显示**。
//!
//! ## 根因
//!
//! Slint 的 winit 后端在创建窗口时按 `WindowAttributes::default().active = true` 置位
//! winit 的 `MARKER_ACTIVATE`（winit 0.30 `platform_impl/windows/window.rs`：
//! `window_flags.set(WindowFlags::MARKER_ACTIVATE, attributes.active)`）。于是首次
//! `Window::show()`（即 `set_visible(true)`）在 `window_state.rs::apply_diff` 里走
//! `ShowWindow(SW_SHOW)` 而非 `SW_SHOWNOACTIVATE`，UI 窗口因此被激活、抢为前台窗口。
//!
//! 这违反规格 2.6（显示必须 `SWP_NOACTIVATE`），且会让 Tauri 壳的全屏检测把自身前台
//! 窗口当作"全屏应用"，存在暂停壁纸的风险（`src-tauri/src/platform/fullscreen.rs`）。
//!
//! ## 方案：`WS_EX_NOACTIVATE` 瞬时覆盖
//!
//! winit 未暴露关闭 `active` 的入口，故在应用层补救：显示前给窗口加上
//! `WS_EX_NOACTIVATE`（`ShowWindow(SW_SHOW)` 对带此位的窗口不会激活它），显示完成后
//! **立刻移除**该位。该位只覆盖"显示"这一瞬间，因此用户此后点击窗口仍能正常获得输入
//! 焦点，不会退化成"永远不能聚焦"。
//!
//! 全程**不调用任何抢前台 API**（无 `SetForegroundWindow` / `SetActiveWindow`）：
//! 窗口此前的显示路径之所以会抢占前台，仅因 winit 的 `SW_SHOW` 语义；屏蔽该语义后
//! 前台保持原样。若原窗口是别的应用，显示本窗口不会把前台夺走。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

/// `WS_EX_NOACTIVATE` 在 `isize` 扩展样式位中的掩码。
const NOACTIVATE_MASK: isize = WS_EX_NOACTIVATE.0 as isize;

/// 扩展样式是否已置 `WS_EX_NOACTIVATE`。
fn has_noactivate(ex_style: isize) -> bool {
    ex_style & NOACTIVATE_MASK != 0
}

/// 置位 `WS_EX_NOACTIVATE` 后的扩展样式。
fn with_noactivate(ex_style: isize) -> isize {
    ex_style | NOACTIVATE_MASK
}

/// 清除 `WS_EX_NOACTIVATE` 后的扩展样式。
fn without_noactivate(ex_style: isize) -> isize {
    ex_style & !NOACTIVATE_MASK
}

/// 显示期间的"禁止激活"RAII 守卫。
///
/// 构造时给窗口置 `WS_EX_NOACTIVATE`，`Drop` 时清除——从而把该位严格限制在
/// `ui.show()` 这一瞬间。若窗口原本就已置该位（例如外部环境、兼容模式），则不做改动
/// 也不在 `Drop` 中清除，避免破坏调用方的既有状态。
pub struct NoActivateGuard {
    hwnd: HWND,
    /// 是否由本守卫置位了 `WS_EX_NOACTIVATE`。
    added: bool,
}

impl NoActivateGuard {
    /// 对给定窗口句柄安装守卫；句柄无效时返回空守卫（不做任何事）。
    pub fn new(hwnd: Option<HWND>) -> Self {
        let Some(hwnd) = hwnd else {
            return Self {
                hwnd: HWND::default(),
                added: false,
            };
        };
        // SAFETY: 句柄来自 Slint 窗口的 raw-window-handle，且本函数在 UI 线程上调用。
        unsafe {
            let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if has_noactivate(ex_style) {
                return Self { hwnd, added: false };
            }
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, with_noactivate(ex_style));
        }
        Self { hwnd, added: true }
    }

    /// 从 Slint 窗口安装守卫（幂等：句柄缺失时退化为空守卫并记日志）。
    pub fn install(window: &slint::Window) -> Self {
        let Some(hwnd) = crate::drop_target::hwnd_of(window) else {
            // 关键前提缺失：无 HWND 就无法屏蔽 `SW_SHOW` 的激活语义，此处显式记录而非静默降级。
            crate::protocol::log(
                "显示前未获取到主窗口 HWND：无法置 WS_EX_NOACTIVATE，窗口可能抢占前台",
            );
            return Self {
                hwnd: HWND::default(),
                added: false,
            };
        };
        let guard = Self::new(Some(hwnd));
        if guard.added {
            crate::protocol::log(format!(
                "不激活显示：已置 WS_EX_NOACTIVATE（HWND=0x{:x}），显示后立即移除",
                hwnd.0 as usize
            ));
        }
        guard
    }
}

impl Drop for NoActivateGuard {
    fn drop(&mut self) {
        if !self.added {
            return;
        }
        // SAFETY: 同 `new`；`hwnd` 在守卫存活期间由 Slint 窗口持有，不至失效。
        unsafe {
            let ex_style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, without_noactivate(ex_style));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noactivate_bit_set_and_clear_roundtrip() {
        // 起始无该位 → 置位后命中 → 清除后复原。
        let start: isize = 0;
        assert!(!has_noactivate(start));
        let set = with_noactivate(start);
        assert!(has_noactivate(set));
        let cleared = without_noactivate(set);
        assert!(!has_noactivate(cleared));
        assert_eq!(cleared, start);
    }

    #[test]
    fn noactivate_bit_preserves_other_exstyle_bits() {
        // 其它扩展样式位（如 WS_EX_TOOLWINDOW=0x80、WS_EX_APPWINDOW=0x40000）不受影响。
        let other: isize = 0x0004_0080;
        let set = with_noactivate(other);
        assert_eq!(set & !NOACTIVATE_MASK, other);
        assert!(has_noactivate(set));
        assert_eq!(without_noactivate(set), other);
    }

    #[test]
    fn already_set_style_is_detected() {
        assert!(has_noactivate(with_noactivate(0)));
    }
}
