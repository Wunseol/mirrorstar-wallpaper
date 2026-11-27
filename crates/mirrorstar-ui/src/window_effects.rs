//! 主窗口显示前的 Windows 合成效果收口。
//!
//! Slint/winit 在 Windows 上以 `transparent = true` 创建窗口（DWM blur-behind 空区域），
//! 且首次映射时预绘制内容会被系统丢弃，导致首帧短暂透出桌面。主窗口本身是全不透明
//! 的深色面板，不需要透明/模糊；在 `show()` 前关闭 DWM 过渡与 blur-behind，避免首帧
//! 以半透明过渡呈现。

#[cfg(windows)]
pub fn prepare_show(window: &slint::Window) {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::Graphics::Dwm::{
        DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED,
        DWM_BB_ENABLE, DWM_BLURBEHIND,
    };

    let Some(hwnd) = crate::drop_target::hwnd_of(window) else {
        crate::protocol::log("显示前未获取到主窗口 HWND，跳过 DWM 过渡收口");
        return;
    };

    // SAFETY: HWND 来自本进程 Slint 窗口；两个调用都只修改本窗口的 DWM 合成属性，
    // 不改变窗口可见性、尺寸、输入或内容。失败仅忽略（best-effort）。
    unsafe {
        let disable_transition = BOOL::from(true);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disable_transition as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );

        let blur_off = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE,
            fEnable: false.into(),
            ..Default::default()
        };
        let _ = DwmEnableBlurBehindWindow(hwnd, &blur_off);
    }
}
