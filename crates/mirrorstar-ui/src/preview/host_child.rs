//! 预览用原生宿主子窗口（Task 8.3 / 8.5 共用基础设施）。
//!
//! ## 为什么用宿主子窗口
//!
//! mpv（`--wid`）与 WebView2（`CreateCoreWebView2Controller`）都要求一个**已存在的
//! HWND** 作为渲染承载。这里统一用系统内置 `STATIC` 类创建一个 `WS_CHILD` 子窗口，
//! 免去注册自定义窗口类与维护消息循环。子窗口是 Slint 顶层窗口的**子窗口**，因此：
//!
//! - 天然叠加在父窗口（Slint 软件渲染）内容**之上**，无需 Z 序管理；
//! - 随父窗口最小化/移动/层级变化自动跟随，不产生独立任务栏项；
//! - 坐标是父窗口**客户区**物理像素，与 Slint 的逻辑坐标只需一次 `× scale_factor`。
//!
//! ## 为什么必须给父窗口补 `WS_CLIPCHILDREN`
//!
//! winit 创建的顶层窗口默认不带该样式；缺省时父窗口重绘会覆盖子窗口区域，导致
//! 预览画面被 Slint 的软件渲染擦除（间歇性黑块）。这里在首次挂载时补齐一次。

use slint::ComponentHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_HIDE, WINDOW_EX_STYLE, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

use crate::protocol::log;

/// 预览区域在父窗口客户区内的物理矩形 `(x, y, w, h)`。
pub type PhysRect = (i32, i32, i32, i32);

/// 给父窗口补齐 `WS_CLIPCHILDREN`（幂等）。
pub fn ensure_clip_children(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = style | (WS_CLIPCHILDREN.0 as isize);
        if new_style != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
            // 直接改 GWL_STYLE 不会触发框架重算非客户区；通知一次使新样式生效。
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }
}

/// 创建承载子窗口（`STATIC` 类，`WS_CHILD`）。
///
/// **隐藏创建**（不带 `WS_VISIBLE`）：预览宿主在首帧/定位就绪前不应暴露，否则打开视频/
/// 网页预览时会闪过一瞬间桌面壁纸左上角的残影。就绪后由 [`show_host`] 显式显示。
pub fn create_host_child(parent: HWND, rect: PhysRect) -> Result<HWND, String> {
    let (x, y, w, h) = rect;
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW 失败: {e}"))?;
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR::null(),
            WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            w,
            h,
            parent,
            None,
            hinstance,
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW 失败: {e}"))?;
    if hwnd.is_invalid() || hwnd == HWND::default() {
        return Err("CreateWindowExW 返回无效句柄".to_string());
    }
    Ok(hwnd)
}

/// 重新定位 / 缩放承载子窗口（不做 Z 序与激活变更，避免抢焦点）。
///
/// 不带 `SWP_SHOWWINDOW`：`SetWindowPos` 缺省不改变可见性，移动不会隐式显示隐藏中的宿主。
pub fn move_host(host: HWND, rect: PhysRect) {
    if host.is_invalid() {
        return;
    }
    let (x, y, w, h) = rect;
    unsafe {
        let _ = SetWindowPos(host, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// 显式显示承载子窗口（就绪后调用；不激活、不抢焦点）。
pub fn show_host(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// 显式隐藏承载子窗口（供"先隐藏后销毁"，避免关闭/切换预览时残留可见窗口）。
pub fn hide_host(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// 销毁承载子窗口。
pub fn destroy_host(host: HWND) {
    if !host.is_invalid() {
        unsafe {
            let _ = DestroyWindow(host);
        }
    }
}

/// 预览宿主子窗口：隐藏创建 → 首帧就绪后显示 → 先隐藏再销毁。
///
/// 收敛 mpv 预览、网页创建状态机与已就绪网页预览三处重复的
/// `shown` 标记 + 幂等显隐样板。句柄无效时全部操作 no-op。
pub struct PreviewHost {
    hwnd: HWND,
    /// 是否已显式显示（`show` 幂等；隐藏创建后首帧就绪前不得显示）。
    shown: bool,
}

impl PreviewHost {
    /// 创建隐藏的承载子窗口（就绪前不可见）。
    pub fn create(parent: HWND, rect: PhysRect) -> Result<Self, String> {
        Ok(Self {
            hwnd: create_host_child(parent, rect)?,
            shown: false,
        })
    }

    /// 宿主句柄。
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 是否已显式显示。
    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// 显式显示（幂等；不激活、不抢焦点）。
    pub fn show(&mut self) {
        if self.shown {
            return;
        }
        show_host(self.hwnd);
        self.shown = true;
    }

    /// 隐藏（幂等；供"先隐藏后销毁"）。
    pub fn hide(&mut self) {
        if !self.shown {
            return;
        }
        hide_host(self.hwnd);
        self.shown = false;
    }

    /// 重新定位 / 缩放（不改变可见性）。
    pub fn move_to(&self, rect: PhysRect) {
        move_host(self.hwnd, rect);
    }
}

impl Drop for PreviewHost {
    fn drop(&mut self) {
        // 先隐藏再销毁：关闭/切换预览时杜绝残留可见子窗口。
        self.hide();
        destroy_host(self.hwnd);
        self.hwnd = HWND::default();
    }
}

/// 逻辑像素矩形 × 缩放因子 → 物理像素矩形（纯函数，便于单测）。
///
/// 宽高设下限 64px：避免预览刚打开、布局尚未稳定时拿到退化尺寸导致子窗口不可见。
pub fn to_physical(x: f32, y: f32, w: f32, h: f32, scale: f32) -> PhysRect {
    (
        (x * scale).round() as i32,
        (y * scale).round() as i32,
        ((w * scale).round() as i32).max(64),
        ((h * scale).round() as i32).max(64),
    )
}

/// 读取 Slint 暴露的预览内容区几何并换算为物理矩形。
///
/// `preview-x/y/w/h` 是绑定了窗口尺寸的纯算术表达式，可在写入 `preview-id` 后立即读取，
/// 不依赖一次布局回填。
///
/// 几何守卫（SubTask 1.3）：控制条存在（`preview-controls-h > 0`）时，宿主子窗口物理矩形
/// 底边不得侵入控制条区域 —— 非 100% DPI 下取整可能让底边恰好越界 1px 级，从而盖住控制条。
/// 越界时夹回并记录一条诊断日志（不静默）；正常不越界不打日志（避免刷屏）。
pub fn physical_rect(ui: &crate::MainWindow) -> PhysRect {
    let scale = ui.window().scale_factor();
    let rect = to_physical(
        ui.get_preview_x(),
        ui.get_preview_y(),
        ui.get_preview_w(),
        ui.get_preview_h(),
        scale,
    );
    let controls_h = ui.get_preview_controls_h();
    if controls_h <= 0.0 {
        return rect;
    }
    let controls_top = ((ui.get_preview_y() + ui.get_preview_h()) * scale).round() as i32;
    let clamped = clamp_host_to_controls(rect, controls_top);
    if clamped != rect {
        log(format!(
            "预览宿主子窗口底边侵入控制条：rect={rect:?} controls_top={controls_top} 已夹回 {clamped:?}"
        ));
    }
    clamped
}

/// 把宿主子窗口物理矩形的高度夹回，使其底边不越过 `controls_top`（纯函数，便于单测）。
///
/// 未越界时原样返回；越界时高度取 `controls_top - y`（下限 1，避免零/负高度；
/// 退化尺寸的 64px 下限已由 [`to_physical`] 保障）。
fn clamp_host_to_controls(rect: PhysRect, controls_top: i32) -> PhysRect {
    let (x, y, w, h) = rect;
    if y + h <= controls_top {
        return rect;
    }
    (x, y, w, (controls_top - y).max(1))
}

#[cfg(test)]
mod tests {
    use super::{clamp_host_to_controls, to_physical};

    #[test]
    fn clamp_host_to_controls_trims_only_on_intrusion() {
        // 底边 100+200=300 未越界（controls_top=300）→ 原样返回。
        assert_eq!(
            clamp_host_to_controls((10, 100, 800, 200), 300),
            (10, 100, 800, 200)
        );
        // controls_top 更大（更靠下）→ 仍原样返回。
        assert_eq!(
            clamp_host_to_controls((10, 100, 800, 200), 400),
            (10, 100, 800, 200)
        );
        // 越界 1px（controls_top=299）→ 高度夹到 199，底边正好落在 299，不再侵入。
        assert_eq!(
            clamp_host_to_controls((10, 100, 800, 200), 299),
            (10, 100, 800, 199)
        );
        // 退化：可用高度 <= 0 → 下限 1。
        assert_eq!(
            clamp_host_to_controls((10, 100, 800, 200), 100),
            (10, 100, 800, 1)
        );
    }

    #[test]
    fn to_physical_scales_and_rounds() {
        let r = to_physical(12.0, 58.0, 880.0, 456.0, 1.5);
        assert_eq!(r, (18, 87, 1320, 684));
    }

    #[test]
    fn to_physical_at_100_percent_is_identity() {
        assert_eq!(to_physical(0.0, 0.0, 300.0, 200.0, 1.0), (0, 0, 300, 200));
    }

    #[test]
    fn to_physical_clamps_degenerate_size() {
        // 布局未稳定时的退化尺寸被抬到下限，保证子窗口可见。
        assert_eq!(to_physical(4.0, 4.0, 3.0, 2.0, 1.0), (4, 4, 64, 64));
    }

    #[test]
    fn to_physical_scales_position_and_size_across_dpi() {
        // 跨屏 DPI 变化时逻辑矩形不变、仅缩放因子变化：物理矩形（位置与尺寸）必须随之缩放，
        // 否则原生子窗口会与 Slint 布局错位。这正是 `preview-geometry-changed` 兜底重排要覆盖的场景。
        let (x, y, w, h) = (26.0, 84.0, 896.0, 536.0);
        assert_eq!(to_physical(x, y, w, h, 1.0), (26, 84, 896, 536));
        assert_eq!(to_physical(x, y, w, h, 1.5), (39, 126, 1344, 804));
        assert_eq!(to_physical(x, y, w, h, 2.0), (52, 168, 1792, 1072));
    }
}
