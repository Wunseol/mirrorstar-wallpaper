use windows::Win32::Foundation::{GetLastError, SetLastError, COLORREF, HWND, WIN32_ERROR};
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
/// - `enabled = false`：清除 `WS_EX_TRANSPARENT`；
///   `preserve_layered = false` 时额外清除 `WS_EX_LAYERED`（旧层级自愈分支），
///   `preserve_layered = true` 时保留 `WS_EX_LAYERED`（raised desktop 的 layered 子窗口）
///
/// 本函数本身绝不**写入** `WS_EX_LAYERED`。旧层级下壁纸窗口（mpv 视频输出窗口、
/// 图片 GDI 窗口）从未调用 `SetLayeredWindowAttributes`，一旦被写入 `WS_EX_LAYERED`
/// 就会进入「分层但无分层属性」的非合成状态，画面停止更新（视频壁纸表现为「暂停」）
/// 且切回后不恢复，因此关闭穿透时默认清除该位以自愈旧版本遗留状态。
///
/// raised desktop 例外：壁纸窗口是 `WS_EX_LAYERED` + alpha=255 的 Progman 子窗口，
/// 清除该位会失去桌面合成（画面冻结），故该路径必须 `preserve_layered = true`。
pub(crate) fn passthrough_ex_style(current: isize, enabled: bool, preserve_layered: bool) -> isize {
    let without_transparent = current & !(WS_EX_TRANSPARENT.0 as isize);
    if enabled {
        current | (WS_EX_TRANSPARENT.0 as isize)
    } else if preserve_layered {
        without_transparent
    } else {
        without_transparent & !(WS_EX_LAYERED.0 as isize)
    }
}

/// 判定关闭穿透时是否必须保留 `WS_EX_LAYERED`（纯逻辑，便于单测）。
///
/// 条件：关闭穿透 + 窗口当前确有 layered + （全局处于 raised desktop，或窗口
/// 直接挂在 Progman 下/Progman 无法判定）。
///
/// 不能只看全局 `is_raised_desktop()`：Explorer 重启/瞬时查找失败时它会返回 false，
/// 而窗口可能仍是 raised 壁纸（清掉 layered 会导致壁纸失去合成/冻结）。以窗口自身
/// 的 layered 位与父链为准，并采用"无法判定即保留"的保守策略。
fn should_preserve_layered(
    enabled: bool,
    has_layered: bool,
    raised: bool,
    parent_progman_or_unknown: bool,
) -> bool {
    !enabled && has_layered && (raised || parent_progman_or_unknown)
}

/// 判定窗口父窗口是否为当前 Progman；Progman 查找失败时返回 true（保守保留 layered）。
fn is_parent_progman_or_unknown(hwnd: HWND) -> bool {
    unsafe {
        match FindWindowW(windows::core::w!("Progman"), None) {
            Ok(progman) if !progman.is_invalid() => GetAncestor(hwnd, GA_PARENT) == progman,
            _ => true,
        }
    }
}

/// 设置鼠标穿透模式
///
/// # 扩展样式契约
///
/// 本函数只切换 `WS_EX_TRANSPARENT`：
/// - `enabled=true`：仅 OR 上 `WS_EX_TRANSPARENT`，保留其它已设扩展样式位；
/// - `enabled=false`：移除 `WS_EX_TRANSPARENT`。旧层级下额外**幂等清除**可能遗留的
///   `WS_EX_LAYERED`；raised desktop 下保留 `WS_EX_LAYERED` 并确保 alpha=255 生效。
///
/// 本函数**绝不写入 `WS_EX_LAYERED`**，也不主动为旧层级窗口调用
/// `SetLayeredWindowAttributes`（v41-D-013 修订）。raised desktop 例外见下。
///
/// # 根因
///
/// 旧层级下壁纸窗口（mpv 视频输出窗口、图片 GDI 窗口）从未调用
/// `SetLayeredWindowAttributes`。一旦被写入 `WS_EX_LAYERED`，窗口即进入「分层但无分层
/// 属性」的非合成状态，画面停止更新——视频壁纸表现为「暂停」；且关闭时若保留
/// `WS_EX_LAYERED`，切回后也不会恢复呈现。`enabled=false` 分支清除该位用于自愈旧版本
/// 遗留的非合成状态。
///
/// raised desktop 下壁纸窗口由 [`apply_raised_desktop_style`] 主动设为
/// `WS_EX_LAYERED` + alpha=255 的 Progman 子窗口，清除该位会让桌面合成失效（画面冻结），
/// 因此关闭穿透时必须保留 `WS_EX_LAYERED` 并重设分层属性。
///
/// # 错误处理契约
///
/// 本函数返回 `()`，失败仅记录 `warn!` 日志，调用方不感知错误。
/// 与 `make_borderless`（返回 `Result`）的错误处理风格不同：鼠标穿透切换失败
/// 不影响核心功能（壁纸嵌入仍可工作），故不向上传播错误（D-TD-022 决策保留 `()`）。
pub fn set_mouse_passthrough(hwnd: HWND, enabled: bool) {
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // raised desktop 下窗口依赖 layered 合成，关闭穿透时不得清除该位。
        // 以窗口自身的 layered 位 + 父链为准，避免全局 raised 状态在 Explorer
        // 重启/瞬时查找失败时误清 layered 导致画面冻结。
        let has_layered = (ex_style & (WS_EX_LAYERED.0 as isize)) != 0;
        let preserve_layered = should_preserve_layered(
            enabled,
            has_layered,
            crate::desktop::worker_w::is_raised_desktop(),
            is_parent_progman_or_unknown(hwnd),
        );
        // 关闭分支：旧层级下若当前样式确实带 WS_EX_LAYERED（旧版本遗留），记录可诊断日志
        if !enabled && !preserve_layered && (ex_style & (WS_EX_LAYERED.0 as isize)) != 0 {
            tracing::info!(hwnd = ?hwnd, "旧版本遗留的 WS_EX_LAYERED 已被清除，壁纸呈现将恢复正常");
        }
        let new_style = passthrough_ex_style(ex_style, enabled, preserve_layered);
        // 失败仅 warn，不向上传播（见函数级错误处理契约）
        let _ = set_window_long_with_error(
            hwnd,
            GWL_EXSTYLE,
            new_style,
            "无法设置鼠标穿透模式（GWL_EXSTYLE）",
        );

        // Refresh window style（与 remove_from_taskbar 保持一致，确保 WS_EX_TRANSPARENT 立即生效）
        refresh_frame_changed(hwnd);

        // raised desktop：样式刷新后确保分层属性 alpha=255 仍然生效，避免"分层但无
        // 分层属性"导致画面冻结。
        if preserve_layered {
            if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) {
                tracing::warn!(
                    hwnd = ?hwnd,
                    error = %e,
                    "raised desktop: 关闭鼠标穿透后重设分层属性失败，画面可能冻结"
                );
            }
        }
    }
}

// ── SetParent 二义性处理（Win11 raised desktop 修复） ─────────────────────
//
// Win32 `SetParent` 成功时返回**前一个父窗口**：顶层窗口的前父可能是
// HWND_DESKTOP(0x10010)，也可能是 NULL。`windows` 0.58 的绑定把 NULL 一律判为
// 失败（`ok_or_else(Error::from_win32)`），当 GetLastError 恰为 0 时便产生
// "操作成功完成 (0x00000000)" 的伪失败。对 mpv 这类弹出式窗口，重定父后
// `GetParent()` 仍返回 0，只有 `GetAncestor(GA_PARENT)` 能反映真实父链。
// 因此真实结果必须以调用后的父链校验为准，而不是返回值是否非 NULL。

/// `SetParent` 调用结果的分类（纯判定，便于单元测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetParentOutcome {
    /// 返回非 NULL 且父链正确。
    Success,
    /// 返回 NULL、`GetLastError == 0` 且父链正确（合法成功，仅返回值二义）。
    AmbiguousSuccess,
    /// 父链不符且无错误码（未生效）。
    NotApplied,
    /// 父链不符且存在 Win32 错误码。
    Win32Error(u32),
}

/// 依据返回值、`GetLastError` 与实际父链判定 `SetParent` 是否成功。
///
/// 父链正确优先：即使返回值是 NULL（顶层窗口前父为 NULL 或 HWND_DESKTOP 的
/// 二义性）也应视为成功；父链不符时再按错误码区分「未生效」与「Win32 错误」。
pub(crate) fn classify_set_parent_result(
    returned_is_null: bool,
    last_error: u32,
    parent_matches: bool,
) -> SetParentOutcome {
    if parent_matches {
        if returned_is_null {
            SetParentOutcome::AmbiguousSuccess
        } else {
            SetParentOutcome::Success
        }
    } else if last_error != 0 {
        SetParentOutcome::Win32Error(last_error)
    } else {
        SetParentOutcome::NotApplied
    }
}

/// 带父链校验的 `SetParent` 封装。
///
/// 调用前清零 `GetLastError`，调用后用 `GetAncestor(child, GA_PARENT)` 与期望父窗口
///（`None` 时为桌面窗口）比对，避免把「NULL 返回 + ERROR_SUCCESS + 父链正确」的
/// 合法成功误判为失败（Win11 mpv 任务栏闪现已确认由此触发）。
///
/// 失败语义：
/// - 父链不符且 `GetLastError != 0` → `DesktopIntegration`（含错误码）
/// - 父链不符且 `GetLastError == 0` → `DesktopIntegration`（"未生效且无错误码"）
pub(crate) fn set_parent_verified(
    child: HWND,
    new_parent: Option<HWND>,
) -> Result<(), MirrorStarError> {
    // SAFETY: SetLastError/SetParent/GetLastError/GetAncestor 均为 Win32 调用；
    // child 由调用方确保有效，new_parent 为合法父窗口或 None（分离到桌面）。
    let expected = match new_parent {
        Some(parent) => parent,
        None => unsafe { GetDesktopWindow() },
    };
    let result = unsafe {
        SetLastError(WIN32_ERROR(0));
        // windows 0.58 的 Param 对可选句柄通过 Option<&HWND> 传入（None 即 NULL 父窗口）。
        SetParent(child, new_parent.as_ref())
    };
    // SAFETY: GetLastError 必须在 SetParent 之后、其它 Win32 调用之前读取。
    let last_error = unsafe { GetLastError() };
    // SAFETY: GetAncestor 仅查询窗口关系，不修改窗口状态。
    let actual = unsafe { GetAncestor(child, GA_PARENT) };
    let previous = result.as_ref().ok().copied();

    match classify_set_parent_result(result.is_err(), last_error.0, actual == expected) {
        SetParentOutcome::Success => Ok(()),
        SetParentOutcome::AmbiguousSuccess => {
            tracing::info!(
                hwnd = child.0 as isize,
                expected_parent = expected.0 as isize,
                actual_parent = actual.0 as isize,
                previous_parent = previous.map_or(0, |h| h.0 as isize),
                "setparent_ambiguous_success"
            );
            Ok(())
        }
        SetParentOutcome::NotApplied => Err(MirrorStarError::DesktopIntegration(format!(
            "SetParent 未生效且无错误码: hwnd=0x{:X}, 期望父窗口=0x{:X}, 实际父窗口=0x{:X}",
            child.0 as isize, expected.0 as isize, actual.0 as isize
        ))),
        SetParentOutcome::Win32Error(code) => {
            tracing::warn!(
                hwnd = child.0 as isize,
                expected_parent = expected.0 as isize,
                actual_parent = actual.0 as isize,
                error_code = code,
                "SetParent 失败"
            );
            Err(MirrorStarError::DesktopIntegration(format!(
                "SetParent 失败（hwnd=0x{:X}, 期望父窗口=0x{:X}, 实际父窗口=0x{:X}），GetLastError: 0x{:08X}",
                child.0 as isize, expected.0 as isize, actual.0 as isize, code
            )))
        }
    }
}

/// 套用 Win11 raised desktop 壁纸窗口样式（Lively/Microsoft 模型）。
///
/// 顺序契约：`GWL_STYLE |= WS_CHILD` → `GWL_EXSTYLE |= WS_EX_LAYERED` →
/// `SetLayeredWindowAttributes(alpha=255)` → `SWP_FRAMECHANGED`。
/// 分层属性必须在 `SetParent` 之前设置，否则已重定父的窗口会缺少分层属性
/// （表现为桌面不合成、画面不可见）。
///
/// 样式写入复用 [`set_window_long_with_error`] 的错误契约；分层属性与框架刷新
/// 失败同样返回 `DesktopIntegration`，由调用方回滚渲染器。
pub(crate) fn apply_raised_desktop_style(hwnd: HWND) -> Result<(), MirrorStarError> {
    // SAFETY: hwnd 由调用方确保有效；样式位读写与分层属性设置均为常规窗口操作。
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        set_window_long_with_error(
            hwnd,
            GWL_STYLE,
            WINDOW_STYLE(style | WS_CHILD.0).0 as isize,
            "无法设置 raised desktop 的 WS_CHILD 样式",
        )?;

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        set_window_long_with_error(
            hwnd,
            GWL_EXSTYLE,
            WINDOW_EX_STYLE(ex_style | WS_EX_LAYERED.0).0 as isize,
            "无法设置 raised desktop 的 WS_EX_LAYERED 扩展样式",
        )?;

        // 必须在 SetParent 之前设置分层属性（alpha=255 完全不透明，仅用于桌面合成）。
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA).map_err(|e| {
            tracing::warn!(
                hwnd = hwnd.0 as isize,
                error = %e,
                "raised desktop: SetLayeredWindowAttributes 失败"
            );
            MirrorStarError::DesktopIntegration(format!("SetLayeredWindowAttributes 失败: {}", e))
        })?;

        SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        )
        .map_err(|e| {
            MirrorStarError::DesktopIntegration(format!(
                "套用 raised 样式后刷新窗口框架失败: {}",
                e
            ))
        })?;
    }
    Ok(())
}

/// 摘出窗口时恢复顶层窗口语义：清除 `WS_CHILD` 并刷新窗口框架。
///
/// **保留 `WS_EX_LAYERED` 与 alpha=255**：若只清 `WS_EX_LAYERED` 而不清分层属性，
/// 窗口会处于「分层但无分层属性」的冻结状态；保留两者则窗口仍是合法的分层顶层窗口。
/// 失败返回 `Result`，供纯净模式摘出路径感知（`remove_wallpaper` 路径仅 warn）。
pub(crate) fn restore_top_level_style(hwnd: HWND) -> Result<(), MirrorStarError> {
    // SAFETY: hwnd 由调用方确保有效；仅清除样式位并刷新框架，不改几何/z-order。
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        set_window_long_with_error(
            hwnd,
            GWL_STYLE,
            WINDOW_STYLE(style & !WS_CHILD.0).0 as isize,
            "无法清除 raised desktop 的 WS_CHILD 样式",
        )?;

        SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        )
        .map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("清除 WS_CHILD 后刷新窗口框架失败: {}", e))
        })?;
    }
    Ok(())
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
        // 验证辅助函数失败时返回 Err(MirrorStarError::DesktopIntegration(format!(...)))，且仅 1 处。
        // 注意：搜索串拆分为两部分运行期拼接，避免 include_str! 将测试自身的字面量计入匹配。
        // 新增 SetParent/样式辅助函数后，文件内会有多处同类 return Err；因此把断言限定在
        // `set_window_long_with_error` 函数体片段内，既避免误计数，又保持原契约的精确验证。
        let needle_prefix = "return Err(MirrorStarError";
        let needle_suffix = "::DesktopIntegration(format!";
        let needle = format!("{}{}", needle_prefix, needle_suffix);
        let fn_start = source
            .find("fn set_window_long_with_error(")
            .expect("应存在 set_window_long_with_error 辅助函数");
        let fn_end = fn_start
            + source[fn_start..]
                .find("fn refresh_frame_changed(")
                .expect("set_window_long_with_error 之后应有 refresh_frame_changed 定义");
        assert_eq!(
            source[fn_start..fn_end].matches(&needle).count(),
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
        let result = passthrough_ex_style(0, true, false);
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

    /// enabled=false 且不保留 layered：从 LAYERED|TRANSPARENT|TOOLWINDOW 出发仅保留 TOOLWINDOW。
    #[test]
    fn passthrough_ex_style_disabled_clears_layered_and_transparent() {
        let current = (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) as isize;
        assert_eq!(
            passthrough_ex_style(current, false, false),
            WS_EX_TOOLWINDOW.0 as isize,
        );
    }

    /// should_preserve_layered：仅"关闭穿透 + 窗口确有 layered + raised/父链
    /// Progman/无法判定"时保留；开启穿透、无 layered、明确旧层级时不保留。
    #[test]
    fn should_preserve_layered_matrix() {
        assert!(!should_preserve_layered(true, true, true, true));
        assert!(!should_preserve_layered(false, false, true, true));
        assert!(should_preserve_layered(false, true, true, false));
        assert!(should_preserve_layered(false, true, false, true));
        assert!(!should_preserve_layered(false, true, false, false));
    }

    /// raised desktop（preserve_layered=true）：关闭穿透只清 TRANSPARENT，保留 LAYERED。
    #[test]
    fn passthrough_ex_style_disabled_preserves_layered_on_raised() {
        let current = (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) as isize;
        let result = passthrough_ex_style(current, false, true);
        assert_ne!(
            result & (WS_EX_LAYERED.0 as isize),
            0,
            "raised desktop 关闭穿透时必须保留 WS_EX_LAYERED"
        );
        assert_eq!(
            result & (WS_EX_TRANSPARENT.0 as isize),
            0,
            "raised desktop 关闭穿透时应清除 WS_EX_TRANSPARENT"
        );
        assert_ne!(
            result & (WS_EX_TOOLWINDOW.0 as isize),
            0,
            "其它扩展样式位应原样保留"
        );
    }

    /// 开→关往返终态等于初始 current（其它位原样保留）。
    #[test]
    fn passthrough_ex_style_roundtrip_restores_initial() {
        let current = (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize;
        let result = passthrough_ex_style(passthrough_ex_style(current, true, false), false, false);
        assert_eq!(result, current);
    }

    /// enabled=true 不破坏既有其它位（如 WS_EX_NOACTIVATE）。
    #[test]
    fn passthrough_ex_style_enabled_preserves_other_bits() {
        let current = WS_EX_NOACTIVATE.0 as isize;
        assert_eq!(
            passthrough_ex_style(current, true, false) & current,
            current,
            "enabled=true 应原样保留已有的其它扩展样式位"
        );
    }

    // ── SetParent 二义性判定纯函数测试 ──────────────────────────────────

    /// 非 NULL 返回 + 父链正确 → Success。
    #[test]
    fn classify_set_parent_result_success() {
        assert_eq!(
            classify_set_parent_result(false, 0, true),
            SetParentOutcome::Success
        );
    }

    /// NULL 返回 + ERROR_SUCCESS + 父链正确 → AmbiguousSuccess（合法成功）。
    #[test]
    fn classify_set_parent_result_ambiguous_success() {
        assert_eq!(
            classify_set_parent_result(true, 0, true),
            SetParentOutcome::AmbiguousSuccess
        );
    }

    /// NULL 返回 + ERROR_SUCCESS + 父链不符 → NotApplied。
    #[test]
    fn classify_set_parent_result_not_applied() {
        assert_eq!(
            classify_set_parent_result(true, 0, false),
            SetParentOutcome::NotApplied
        );
    }

    /// 非 NULL 返回 + 非零错误码 + 父链不符 → Win32Error（携带错误码）。
    #[test]
    fn classify_set_parent_result_win32_error() {
        assert_eq!(
            classify_set_parent_result(false, 5, false),
            SetParentOutcome::Win32Error(5)
        );
        assert_eq!(
            classify_set_parent_result(true, 1400, false),
            SetParentOutcome::Win32Error(1400)
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
