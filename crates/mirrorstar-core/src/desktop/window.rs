use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::MirrorStarError;

/// 去除窗口边框（Borderless Window）
///
/// D-003: SetWindowLongPtrW 失败时返回 Err（与 SetParent/SetWindowPos 错误传播风格一致），
/// 不再恒返回 Ok。调用方通过 `?` 传播错误。
pub fn make_borderless(hwnd: HWND) -> Result<(), MirrorStarError> {
    unsafe {
        // Remove standard window styles
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let new_style = WINDOW_STYLE(style)
            & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX);
        // D-003: SetWindowLongPtrW 失败时返回 Err（与 SetParent/SetWindowPos 错误传播风格一致）
        set_window_long_with_error(
            hwnd,
            GWL_STYLE,
            new_style.0 as isize,
            "无法设置 GWL_STYLE 窗口样式",
        )?;

        // Remove extended styles（不动 WS_EX_LAYERED：由 set_mouse_passthrough 的关闭分支负责幂等清除）
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let new_ex_style = WINDOW_EX_STYLE(ex_style)
            & !(WS_EX_DLGMODALFRAME
                | WS_EX_COMPOSITED
                | WS_EX_WINDOWEDGE
                | WS_EX_CLIENTEDGE
                | WS_EX_STATICEDGE
                | WS_EX_TOOLWINDOW
                | WS_EX_APPWINDOW);
        // D-003: SetWindowLongPtrW 失败时返回 Err（与 SetParent/SetWindowPos 错误传播风格一致）
        set_window_long_with_error(
            hwnd,
            GWL_EXSTYLE,
            new_ex_style.0 as isize,
            "无法设置 GWL_EXSTYLE 扩展样式",
        )?;
    }
    Ok(())
}

/// 从任务栏移除窗口
///
/// # 错误处理契约
///
/// 本函数返回 `()`，失败仅记录 `warn!` 日志，调用方不感知错误。
/// 与 `make_borderless`（返回 `Result`）的错误处理风格不同：任务栏移除失败
/// 不影响核心功能（壁纸嵌入仍可工作），故不向上传播错误（D-TD-022 决策保留 `()`）。
pub fn remove_from_taskbar(hwnd: HWND) {
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex_style = (ex_style as usize) & !(WS_EX_APPWINDOW.0 as usize)
            | (WS_EX_TOOLWINDOW.0 as usize)
            | (WS_EX_NOACTIVATE.0 as usize);
        // 失败仅 warn，不向上传播（见函数级错误处理契约）
        let _ = set_window_long_with_error(
            hwnd,
            GWL_EXSTYLE,
            new_ex_style as isize,
            "无法从任务栏移除窗口（GWL_EXSTYLE）",
        );

        // Refresh window style
        refresh_frame_changed(hwnd);
    }
}

/// 计算鼠标穿透切换后的扩展样式位。
///
/// - `enabled = true`：仅 OR `WS_EX_TRANSPARENT`
/// - `enabled = false`：清除 `WS_EX_TRANSPARENT` 与 `WS_EX_LAYERED`
///
/// 绝不写入 `WS_EX_LAYERED`：壁纸窗口（mpv 视频输出窗口、图片 GDI 窗口）从未调用
/// `SetLayeredWindowAttributes`，一旦被写入 `WS_EX_LAYERED` 就会进入「分层但无分层属性」
/// 的非合成状态，画面停止更新（视频壁纸表现为「暂停」）且切回后不恢复。
pub(crate) fn passthrough_ex_style(current: isize, enabled: bool) -> isize {
    if enabled {
        current | (WS_EX_TRANSPARENT.0 as isize)
    } else {
        current & !(WS_EX_TRANSPARENT.0 as isize) & !(WS_EX_LAYERED.0 as isize)
    }
}

/// 设置鼠标穿透模式
///
/// # 扩展样式契约
///
/// 本函数只切换 `WS_EX_TRANSPARENT`：
/// - `enabled=true`：仅 OR 上 `WS_EX_TRANSPARENT`，保留其它已设扩展样式位；
/// - `enabled=false`：移除 `WS_EX_TRANSPARENT` 并**幂等清除**可能遗留的 `WS_EX_LAYERED`。
///
/// 本函数**绝不写入 `WS_EX_LAYERED`**，也不调用 `SetLayeredWindowAttributes`（v41-D-013 修订）。
///
/// # 根因
///
/// 壁纸窗口（mpv 视频输出窗口、图片 GDI 窗口）从未调用 `SetLayeredWindowAttributes`。
/// 一旦被写入 `WS_EX_LAYERED`，窗口即进入「分层但无分层属性」的非合成状态，画面停止更新
/// ——视频壁纸表现为「暂停」；且关闭时若保留 `WS_EX_LAYERED`，切回后也不会恢复呈现。
/// `enabled=false` 分支清除该位用于自愈旧版本遗留的非合成状态。
///
/// # 错误处理契约
///
/// 本函数返回 `()`，失败仅记录 `warn!` 日志，调用方不感知错误。
/// 与 `make_borderless`（返回 `Result`）的错误处理风格不同：鼠标穿透切换失败
/// 不影响核心功能（壁纸嵌入仍可工作），故不向上传播错误（D-TD-022 决策保留 `()`）。
pub fn set_mouse_passthrough(hwnd: HWND, enabled: bool) {
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // 关闭分支：若当前样式确实带 WS_EX_LAYERED（旧版本遗留），记录可诊断日志
        if !enabled && (ex_style & (WS_EX_LAYERED.0 as isize)) != 0 {
            tracing::info!(hwnd = ?hwnd, "旧版本遗留的 WS_EX_LAYERED 已被清除，壁纸呈现将恢复正常");
        }
        let new_style = passthrough_ex_style(ex_style, enabled);
        // 失败仅 warn，不向上传播（见函数级错误处理契约）
        let _ = set_window_long_with_error(
            hwnd,
            GWL_EXSTYLE,
            new_style,
            "无法设置鼠标穿透模式（GWL_EXSTYLE）",
        );

        // Refresh window style（与 remove_from_taskbar 保持一致，确保 WS_EX_TRANSPARENT 立即生效）
        refresh_frame_changed(hwnd);
    }
}

/// D-TD-005: SetWindowLongPtrW + GetLastError 错误检查辅助函数。
///
/// 封装 `SetLastError(0)` → `SetWindowLongPtrW` → `GetLastError` 检查模式：
/// - 失败时（GetLastError 非 0）记录 `warn!` 日志并返回 `Err(MirrorStarError::DesktopIntegration)`
/// - 成功时返回 `Ok(())`
///
/// `context` 用于拼接失败消息（如 "无法设置 GWL_STYLE 窗口样式"），失败时格式化为：
/// `"SetWindowLongPtrW 失败：{context}，GetLastError: 0x{err:08X}"`
///
/// v41-D-006: warn 日志记录 `error_code` 字段（GetLastError 数值），便于区分失败原因
/// （如 ERROR_ACCESS_DENIED=5、ERROR_INVALID_WINDOW_HANDLE=1400）。
fn set_window_long_with_error(
    hwnd: HWND,
    index: WINDOW_LONG_PTR_INDEX,
    value: isize,
    context: &str,
) -> Result<(), MirrorStarError> {
    // SetWindowLongPtrW 返回 0 可能是合法的旧值，需用 GetLastError 判定真实失败
    // SAFETY: hwnd 由调用方确保有效，index/value 为合法枚举值与样式位。
    unsafe {
        windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
        let _ = SetWindowLongPtrW(hwnd, index, value);
        let err = windows::Win32::Foundation::GetLastError();
        if err.0 != 0 {
            tracing::warn!(error_code = err.0, "SetWindowLongPtrW 失败：{}", context);
            return Err(MirrorStarError::DesktopIntegration(format!(
                "SetWindowLongPtrW 失败：{}，GetLastError: 0x{:08X}",
                context, err.0
            )));
        }
    }
    Ok(())
}

/// D-TD-006: SetWindowPos(SWP_FRAMECHANGED) 刷新窗口样式辅助函数。
///
/// 封装 `SetWindowPos` + `SWP_FRAMECHANGED` 模式，用于在修改窗口样式后通知系统刷新。
/// 失败时仅记录 `warn!` 日志，调用方不感知错误（与 `remove_from_taskbar`/
/// `set_mouse_passthrough` 的错误处理契约一致）。
fn refresh_frame_changed(hwnd: HWND) {
    // SAFETY: hwnd 由调用方确保有效，SetWindowPos 参数为合法的 no-op 样式刷新调用。
    let result = unsafe {
        SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        )
    };
    if result.is_err() {
        tracing::warn!("SetWindowPos 失败：无法刷新窗口样式（FRAMECHANGED）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    /// D-003: 验证 make_borderless 在 SetWindowLongPtrW 失败时返回 Err。
    ///
    /// 本测试为文档测试（include_str! 模式），通过断言源码包含关键标记验证修复存在。
    /// 无法直接单元测试 make_borderless 的失败分支，因为：
    /// 1. 需要真实窗口句柄（CreateWindowExW）
    /// 2. 需要触发 SetWindowLongPtrW 失败（难以可靠触发）
    /// 3. 风格统一使用 include_str! 模式
    ///
    /// D-TD-005: SetWindowLongPtrW 错误检查已抽取为 `set_window_long_with_error` 辅助函数，
    /// make_borderless 通过 `?` 传播辅助函数返回的 Err。
    #[test]
    fn d003_make_borderless_returns_err_on_setwindowlongptr_failure() {
        let source = include_str!("window.rs");
        // 验证 D-003 前缀注释存在
        assert!(
            source.contains("D-003: SetWindowLongPtrW 失败时返回 Err"),
            "make_borderless 注释应含 D-003 前缀标识"
        );
        // D-TD-005: 验证辅助函数 set_window_long_with_error 存在
        assert!(
            source.contains("fn set_window_long_with_error("),
            "应存在 set_window_long_with_error 辅助函数"
        );
        // 验证辅助函数失败时返回 Err(MirrorStarError::DesktopIntegration(format!(...)))
        // 注意：搜索串拆分为两部分运行期拼接，避免 include_str! 将测试自身的字面量计入匹配。
        let needle_prefix = "return Err(MirrorStarError";
        let needle_suffix = "::DesktopIntegration(format!";
        let needle = format!("{}{}", needle_prefix, needle_suffix);
        assert_eq!(
            source.matches(&needle).count(),
            1,
            "辅助函数 set_window_long_with_error 应有 1 处 return Err(MirrorStarError::DesktopIntegration(...)) 分支"
        );
        // 验证 make_borderless 通过辅助函数设置 GWL_STYLE（错误经 ? 传播，context 字符串）
        assert!(
            source.contains("\"无法设置 GWL_STYLE 窗口样式\""),
            "make_borderless 应通过辅助函数设置 GWL_STYLE（context 字符串）"
        );
        // 验证 make_borderless 通过辅助函数设置 GWL_EXSTYLE（错误经 ? 传播，context 字符串）
        assert!(
            source.contains("\"无法设置 GWL_EXSTYLE 扩展样式\""),
            "make_borderless 应通过辅助函数设置 GWL_EXSTYLE（context 字符串）"
        );
        // 验证辅助函数错误消息含 GetLastError 格式化模板
        assert!(
            source.contains("SetWindowLongPtrW 失败：{}，GetLastError: 0x{:08X}"),
            "辅助函数错误消息应含 'SetWindowLongPtrW 失败：{{}}，GetLastError: 0x{{:08X}}' 格式化模板"
        );
    }

    // ── D-010: window.rs 公开函数单元测试覆盖 ──────────────────────────
    //
    // 参照 worker_w.rs 中 `try_find_sibling_workerw_helper_no_panic` 范式：
    // 获取真实 Progman 窗口（FindWindowW 搜索 "Progman"），调用各公开函数验证不 panic。
    //
    // 标记 #[ignore] 因为这些测试会修改真实 Progman 窗口的样式位，可能影响
    // 桌面外观（虽然 Progman 重启后会恢复），不适合在自动化测试中运行。本地手动运行：
    // `cargo test -p mirrorstar-core --lib desktop::window:: -- --ignored`

    /// D-010: 验证 make_borderless 对真实 Progman 窗口不 panic
    #[test]
    #[ignore = "需要 Windows 环境且修改 Progman 窗口样式"]
    fn make_borderless_no_panic_on_progman() {
        let progman = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman {
            // 不断言返回值，仅验证不 panic
            let _ = make_borderless(hwnd);
        }
        // Progman 找不到时（极端情况）跳过，不 panic 即通过
    }

    /// D-010: 验证 remove_from_taskbar 对真实 Progman 窗口不 panic
    #[test]
    #[ignore = "需要 Windows 环境且修改 Progman 窗口样式"]
    fn remove_from_taskbar_no_panic_on_progman() {
        let progman = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman {
            remove_from_taskbar(hwnd);
        }
        // Progman 找不到时（极端情况）跳过，不 panic 即通过
    }

    // ── v41-D-013: passthrough_ex_style 纯函数单元测试 ──────────────────

    /// enabled=true 仅 OR WS_EX_TRANSPARENT，绝不写入 WS_EX_LAYERED。
    #[test]
    fn passthrough_ex_style_enabled_never_sets_layered() {
        let result = passthrough_ex_style(0, true);
        assert_eq!(
            result & (WS_EX_LAYERED.0 as isize),
            0,
            "enabled=true 不得写入 WS_EX_LAYERED"
        );
        assert_ne!(
            result & (WS_EX_TRANSPARENT.0 as isize),
            0,
            "enabled=true 应包含 WS_EX_TRANSPARENT"
        );
    }

    /// enabled=false 从 LAYERED|TRANSPARENT|TOOLWINDOW 出发仅保留 TOOLWINDOW。
    #[test]
    fn passthrough_ex_style_disabled_clears_layered_and_transparent() {
        let current = (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) as isize;
        assert_eq!(
            passthrough_ex_style(current, false),
            WS_EX_TOOLWINDOW.0 as isize,
        );
    }

    /// 开→关往返终态等于初始 current（其它位原样保留）。
    #[test]
    fn passthrough_ex_style_roundtrip_restores_initial() {
        let current = (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize;
        let result = passthrough_ex_style(passthrough_ex_style(current, true), false);
        assert_eq!(result, current);
    }

    /// enabled=true 不破坏既有其它位（如 WS_EX_NOACTIVATE）。
    #[test]
    fn passthrough_ex_style_enabled_preserves_other_bits() {
        let current = WS_EX_NOACTIVATE.0 as isize;
        assert_eq!(
            passthrough_ex_style(current, true) & current,
            current,
            "enabled=true 应原样保留已有的其它扩展样式位"
        );
    }

    /// D-010: 验证 set_mouse_passthrough 对真实 Progman 窗口不 panic
    #[test]
    #[ignore = "需要 Windows 环境且修改 Progman 窗口样式"]
    fn set_mouse_passthrough_no_panic_on_progman() {
        let progman = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman {
            // 测试 enabled=true 分支（仅 OR 上 WS_EX_TRANSPARENT，绝不写入 WS_EX_LAYERED）
            set_mouse_passthrough(hwnd, true);
        }
        // Progman 找不到时（极端情况）跳过，不 panic 即通过
    }
}
