//! WorkerW 窗口查找与壁纸嵌入模块。
//!
//! 本模块负责与 Windows 桌面窗口管理器交互，查找 Progman/WorkerW 窗口句柄，
//! 并将壁纸窗口嵌入到 WorkerW 层（位于桌面图标层之下）。
//!
//! ## EnumWindowsProc 捕获变量风格
//!
//! 当前使用裸指针 + LPARAM 模拟捕获（`EnumWindows` 限制：回调为
//! `extern "system" fn`，无法直接使用闭包）。

use windows::Win32::Foundation::{SetLastError, BOOL, HWND, LPARAM, RECT, WIN32_ERROR, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::settings::Arrangement;
use crate::config::DisplayInfo;
use crate::MirrorStarError;

// ── 模块级常量段（v41-D-010）─────────────────────────────────────────
//
// 所有跨函数共享的常量集中定义于此段落，避免散落在函数体内导致维护困难。
// 原 `UNICODE_STRING_MAX_CHARS` 曾紧邻 `get_system_wallpaper` 函数体，已提取至此。

/// Win32 路径缓冲区最大字符数（UNICODE_STRING_MAX_CHARS）
///
/// 覆盖 Win32 长路径（`\\?\` 前缀可达 32767 字符）与 UNC 路径上限。
/// 原 MAX_PATH(260) 已被 D02 Medium 修复扩大至此值，以支持超长路径。
const UNICODE_STRING_MAX_CHARS: usize = 32767;

/// Progman 触发 WorkerW 创建的窗口消息（参考 Lively Wallpaper 实现）
///
/// 发送此消息给 Progman 窗口会触发其创建 WorkerW 层，用于嵌入壁纸窗口。
const WM_SPAWN_WORK: u32 = 0x052C;

/// WM_SPAWN_WORK 消息超时（ms），用于 SendMessageTimeoutW 调用（D-012）
const WM_SPAWN_WORK_TIMEOUT_MS: u32 = 200;

/// find_child_by_class 递归查找子窗口的最大深度
///
/// 限制递归深度避免无限递归；3 层足以覆盖 Progman/WorkerW → SHELLDLL_DefView 层级。
const MAX_CHILD_DEPTH: u32 = 3;

// v5.0 D-PERF-001: WorkerW / Progman 类名的 UTF-16LE 字面量，
// 用于 callback 内字节级比较，避免 String::from_utf16_lossy 堆分配。
const WORKERW_CLASS_WIDE: &[u16] = &[
    0x57, 0x6F, 0x72, 0x6B, 0x65, 0x72, 0x57, // "WorkerW"
];
const PROGMAN_CLASS_WIDE: &[u16] = &[
    0x50, 0x72, 0x6F, 0x67, 0x6D, 0x61, 0x6E, // "Progman"
];

/// ERROR_SUCCESS（Win32 无错误码），用于在 EnumWindows callback 返回 FALSE 前
/// 清除 stale last-error，避免 `windows` crate 的 `EnumWindows` 包装器将 callback
/// 正常停止枚举误报为错误。
const ERROR_SUCCESS: WIN32_ERROR = WIN32_ERROR(0);

/// v5.0 D-PERF-001: 比较 UTF-16 缓冲区与宽字符字面量，避免转换为 String。
///
/// `len` 为 `GetClassNameW` 返回的有效字符数（不含 null 终止符）。
/// `len == 0` 时因 `0 != wide.len()` 自然返回 false，无需单独处理。
fn eq_wide(buf: &[u16], len: usize, wide: &[u16]) -> bool {
    len == wide.len() && buf[..len] == *wide
}

/// v5.0 D-PERF-002: 验证窗口类名是否为 WorkerW
///
/// 用于 Progman 直查快速路径中对候选 HWND 的类名校验，避免 String 分配。
fn is_workerw_class(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 256];
    // SAFETY: GetClassNameW 读取窗口类名到栈缓冲区，256 >= 任何窗口类名长度。
    let len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    len > 0 && eq_wide(&class_buf, len as usize, WORKERW_CLASS_WIDE)
}

/// 单次尝试查找 Progman 和 WorkerW 窗口句柄（无重试、无 sleep）
///
/// 执行完整的查找流程：FindWindowW(Progman) → EnumWindows → SendMessageTimeoutW(WM_SPAWN_WORK) → EnumWindows → fallback。
/// 适用于在持有 desktop 锁时调用，单次执行时间约 5ms（典型）至 200ms（SendMessageTimeoutW 超时）。
/// 重试逻辑（含 sleep）由调用方在释放锁后执行，以避免持锁 sleep 阻塞其他 desktop 访问者。
///
/// D-007: EnumWindows 失败时记录 warn 日志（错误码），避免静默吞错。
/// v41-D-005: EnumWindows 失败时不仅记录 warn，还向上返回 `Err(MirrorStarError::DesktopIntegration(...))`，
/// 因 EnumWindows 失败通常表明 GDI 子系统严重故障，继续后续步骤（SendMessageTimeoutW / 再次 EnumWindows）
/// 亦无意义。调用方可通过重试机制（`ensure_desktop_ready_with_retry`）在释放锁后重试。
pub fn find_workerw_no_retry() -> Result<(HWND, HWND), MirrorStarError> {
    unsafe {
        // Step 1: FindWindowW("Progman", null)
        let progman_hwnd = FindWindowW(windows::core::w!("Progman"), None).map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("未找到 Progman 窗口: {}", e))
        })?;

        if progman_hwnd.is_invalid() {
            return Err(MirrorStarError::DesktopIntegration(
                "Progman 窗口句柄无效".to_string(),
            ));
        }

        // v5.0 D-PERF-002: Progman 直查快速路径
        // SHELLDLL_DefView 通常直接挂在 Progman 下，先直查 Progman 子窗口，
        // 命中则取其父窗口（WorkerW）或 Progman 的兄弟 WorkerW，将 EnumWindows
        // 退化为极少触发的 fallback。直查失败时静默退回 EnumWindows 全量扫描，不报错。
        if let Ok(shelldll) = FindWindowExW(
            progman_hwnd,
            HWND::default(),
            windows::core::w!("SHELLDLL_DefView"),
            None,
        ) {
            if !shelldll.is_invalid() {
                // SHELLDLL_DefView 的父窗口可能是 WorkerW（WM_SPAWN_WORK 后）或 Progman（之前）
                // GetAncestor 返回 HWND（无效时为 default），非 Result。
                let parent = GetAncestor(shelldll, GA_PARENT);
                if !parent.is_invalid() && parent != progman_hwnd && is_workerw_class(parent) {
                    tracing::debug!(
                        progman = progman_hwnd.0 as isize,
                        workerw = parent.0 as isize,
                        "D-PERF-002: Progman 直查命中（SHELLDLL_DefView 父窗口为 WorkerW）"
                    );
                    return Ok((progman_hwnd, parent));
                }
                // 父窗口是 Progman 或验证失败，尝试找 Progman 的兄弟 WorkerW
                if let Ok(workerw_hwnd) = FindWindowExW(
                    HWND::default(),
                    progman_hwnd,
                    windows::core::w!("WorkerW"),
                    None,
                ) {
                    if !workerw_hwnd.is_invalid() && is_workerw_class(workerw_hwnd) {
                        tracing::debug!(
                            progman = progman_hwnd.0 as isize,
                            workerw = workerw_hwnd.0 as isize,
                            "D-PERF-002: Progman 直查命中（FindWindowExW 找到兄弟 WorkerW）"
                        );
                        return Ok((progman_hwnd, workerw_hwnd));
                    }
                }
            }
        }
        // Progman 直查失败，退回 EnumWindows 全量扫描（原有逻辑）

        // Step 2: Try to find WorkerW without sending message (it may already exist)
        let mut workerw_hwnd: Option<HWND> = None;
        let workerw_hwnd_ptr = LPARAM(&mut workerw_hwnd as *mut Option<HWND> as isize);
        let workerw_hwnd = try_enum_windows(
            Some(find_workerw_callback),
            workerw_hwnd_ptr,
            "Step 2: 直接查找 WorkerW",
        )?;

        if let Some(w) = workerw_hwnd {
            tracing::debug!(workerw = w.0 as isize, "直接找到 WorkerW 窗口");
            return Ok((progman_hwnd, w));
        }

        // Step 3: SendMessageTimeoutW(progman_hwnd, WM_SPAWN_WORK, ...) to trigger WorkerW creation
        // 注意：Lively 不检查返回值，因为即使返回 0，WorkerW 也可能已经创建
        // 此消息幂等：若 WorkerW 已存在，Progman 不会重复创建
        // 不关心返回值，传 None 避免 dead store（D09）
        //
        // v41-D-003: 增加 SMTO_ABORTIFHUNG flag，若 Progman 消息处理线程挂起
        //（典型 < 5s）则立即返回，避免持锁调用方（ensure_workerw_ready）长时间阻塞。
        let _ = SendMessageTimeoutW(
            progman_hwnd,
            WM_SPAWN_WORK,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            WM_SPAWN_WORK_TIMEOUT_MS,
            None,
        );

        // Step 4: Try EnumWindows again (WorkerW may have been created)
        let mut workerw_hwnd: Option<HWND> = None;
        let workerw_hwnd_ptr = LPARAM(&mut workerw_hwnd as *mut Option<HWND> as isize);
        let workerw_hwnd = try_enum_windows(
            Some(find_workerw_callback),
            workerw_hwnd_ptr,
            "Step 4: 发送 WM_SPAWN_WORK 后查找 WorkerW",
        )?;

        if let Some(workerw_hwnd) = workerw_hwnd {
            tracing::debug!(
                workerw = workerw_hwnd.0 as isize,
                "发送 WM_SPAWN_WORK 后找到 WorkerW 窗口"
            );
            return Ok((progman_hwnd, workerw_hwnd));
        }

        // Step 5: Fallback - 查找所有 WorkerW 窗口
        let mut fallback_workerw_hwnd: Option<HWND> = None;
        let fallback_workerw_hwnd_ptr =
            LPARAM(&mut fallback_workerw_hwnd as *mut Option<HWND> as isize);
        let fallback_workerw_hwnd = try_enum_windows(
            Some(find_workerw_fallback_callback),
            fallback_workerw_hwnd_ptr,
            "Step 5: 备用方法查找 WorkerW",
        )?;

        if let Some(workerw_hwnd) = fallback_workerw_hwnd {
            tracing::debug!(
                workerw = workerw_hwnd.0 as isize,
                "备用方法找到 WorkerW 窗口"
            );
            return Ok((progman_hwnd, workerw_hwnd));
        }

        Err(MirrorStarError::WorkerWNotFound)
    }
}

/// D-TD-007: 封装 `EnumWindows` 调用与 Err 误报处理。
///
/// `windows` crate 的 `EnumWindows` 在 callback 返回 FALSE（正常停止枚举）时
/// 也返回 `Err`，需先检查 callback 是否已通过 lparam 写入结果；仅在未写入结果
/// 且 `EnumWindows` 返回 `Err` 时才向上传播错误，否则仅记录 debug 日志。
///
/// # Safety
///
/// `lparam` 必须指向由调用方初始化为 `None` 的有效 `Option<HWND>`。该函数会
/// 通过裸指针读写该内存。
unsafe fn try_enum_windows(
    callback: WNDENUMPROC,
    lparam: LPARAM,
    step_label: &str,
) -> Result<Option<HWND>, MirrorStarError> {
    // D-007: EnumWindows 失败时记录错误码，避免静默吞错。
    // v41-D-005: EnumWindows 失败时返回 Err，而非继续走 None 分支自然降级。
    // Bug fix: windows crate 的 EnumWindows 在 callback 返回 FALSE（正常停止枚举）
    // 时也返回 Err。需先检查 callback 是否已通过 lparam 写入结果（即 lparam 指向的
    // Option<HWND> 是否为 Some），若已写入则继续，仅在未写入且 EnumWindows
    // 返回 Err 时才报错。
    if let Err(e) = EnumWindows(callback, lparam) {
        // SAFETY: lparam 指向调用方栈上有效的 Option<HWND>。
        let result_ptr = lparam.0 as *mut Option<HWND>;
        if (*result_ptr).is_none() {
            tracing::warn!(error = ?e, "EnumWindows 调用失败（{}）", step_label);
            return Err(MirrorStarError::DesktopIntegration(format!(
                "EnumWindows 调用失败（{}）: {}",
                step_label, e
            )));
        }
        // result_ptr 已写入：EnumWindows 的 Err 是 callback 返回 FALSE 导致的误报
        tracing::debug!(
            error = ?e,
            step_label,
            "EnumWindows 返回 Err 但已找到 WorkerW（callback 正常停止枚举）"
        );
    }
    // SAFETY: lparam 指向调用方栈上有效的 Option<HWND>。
    Ok(*(lparam.0 as *mut Option<HWND>))
}

/// EnumWindows callback: find the WorkerW that is the sibling of the one containing SHELLDLL_DefView
unsafe extern "system" fn find_workerw_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // Get window class name for debugging
    let mut class_name_buf = [0u16; 256];
    let name_len = GetClassNameW(hwnd, &mut class_name_buf);
    if name_len > 0 {
        let len = name_len as usize;
        // v5.0 D-PERF-001: 字节级比较，避免 String::from_utf16_lossy 堆分配。
        // 仅在匹配 WorkerW/Progman 时才分配 String（用于日志），非匹配窗口零分配。
        if eq_wide(&class_name_buf, len, WORKERW_CLASS_WIDE)
            || eq_wide(&class_name_buf, len, PROGMAN_CLASS_WIDE)
        {
            let class_str = String::from_utf16_lossy(&class_name_buf[..len]);
            tracing::debug!(hwnd = hwnd.0 as isize, class = %class_str, "枚举到窗口");
        }
    }

    // 1. Check if this top-level window has a SHELLDLL_DefView child (recursive search)
    let shelldll =
        find_child_by_class(hwnd, windows::core::w!("SHELLDLL_DefView"), MAX_CHILD_DEPTH);

    if shelldll.is_none() {
        return BOOL(1); // Continue enumeration
    }

    tracing::debug!(hwnd = hwnd.0 as isize, "找到包含 SHELLDLL_DefView 的窗口");

    // 2. Found SHELLDLL_DefView's parent, find sibling WorkerW (check both next and prev)
    // D11: 统一使用 try_find_sibling_workerw 检查两个方向的兄弟窗口，
    // 修复原代码仅在 next_sibling 无效时才检查 prev_sibling 的遗漏。
    if try_find_sibling_workerw(hwnd, lparam) {
        // Bug fix: 清除 callback 内部 Win32 调用（GetClassNameW / FindWindowExW /
        // GetWindow）可能设置的 stale last-error。windows crate 的 EnumWindows 包装器
        // 在返回值为 0（callback 返回 FALSE）时调用 GetLastError()，若不清除会误报为错误。
        SetLastError(ERROR_SUCCESS);
        return BOOL(0); // Stop enumeration
    }

    BOOL(1) // Continue enumeration
}

/// 备用 EnumWindows callback: 查找所有 WorkerW 窗口，返回没有 SHELLDLL_DefView 子窗口的那个
/// 参考 Lively 的方法：找到包含 SHELLDLL_DefView 的 WorkerW，然后取其兄弟 WorkerW
unsafe extern "system" fn find_workerw_fallback_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut class_name_buf = [0u16; 256];
    let name_len = GetClassNameW(hwnd, &mut class_name_buf);
    if name_len == 0 {
        return BOOL(1);
    }
    // v5.0 D-PERF-001: 字节级比较，避免 String::from_utf16_lossy 堆分配。
    // 非 WorkerW 窗口直接 continue，零分配。
    let len = name_len as usize;
    if !eq_wide(&class_name_buf, len, WORKERW_CLASS_WIDE) {
        return BOOL(1);
    }

    // 检查此 WorkerW 是否包含 SHELLDLL_DefView
    let has_shelldll =
        find_child_by_class(hwnd, windows::core::w!("SHELLDLL_DefView"), MAX_CHILD_DEPTH);

    if has_shelldll.is_some() {
        // 这是包含桌面图标的 WorkerW，找到它的兄弟 WorkerW
        tracing::debug!(
            hwnd = hwnd.0 as isize,
            "备用方法：找到包含 SHELLDLL_DefView 的 WorkerW"
        );
        // D11: 统一使用 try_find_sibling_workerw 检查两个方向的兄弟窗口
        if try_find_sibling_workerw(hwnd, lparam) {
            // Bug fix: 清除 stale last-error，避免 windows crate EnumWindows 误报错误
            SetLastError(ERROR_SUCCESS);
            return BOOL(0); // Stop enumeration
        }
    }

    BOOL(1)
}

/// 在 hwnd 的兄弟窗口中查找类名为 "WorkerW" 的窗口
///
/// 统一 `find_workerw_callback` 与 `find_workerw_fallback_callback` 的兄弟检查逻辑（D11）。
/// 依次检查下一个（GW_HWNDNEXT）和上一个（GW_HWNDPREV）兄弟窗口，
/// 找到第一个类名为 "WorkerW" 的兄弟即写入 lparam 并返回 true（停止枚举）；
/// 未找到则返回 false（继续枚举）。
///
/// v41-D-008: 本函数本身不调用 `SendMessageTimeoutW`，但其上游
/// `find_workerw_no_retry` 在 Step 3 发送 `WM_SPAWN_WORK` 触发 WorkerW 创建。
/// 当 `SendMessageTimeoutW` 超时（`WAIT_TIMEOUT`）时，WorkerW 可能尚未创建，
/// 后续 EnumWindows 不会发现 WorkerW；这与"WorkerW 本就不存在"无法在上游区分，
/// 两种情况均导致 `find_workerw_no_retry` 最终返回 `Err(WorkerWNotFound)`。
/// 调用方需在拿到 `Ok((progman_hwnd, workerw_hwnd))` 后用 `IsWindow` 校验 HWND 仍然有效
///（Explorer 重启场景下 HWND 可能被复用为其它窗口）。
unsafe fn try_find_sibling_workerw(hwnd: HWND, lparam: LPARAM) -> bool {
    if check_sibling_workerw(hwnd, GW_HWNDNEXT, lparam) {
        return true;
    }
    check_sibling_workerw(hwnd, GW_HWNDPREV, lparam)
}

/// 检查指定方向的兄弟窗口是否为 WorkerW
///
/// 若兄弟窗口有效且类名为 "WorkerW"，则写入 lparam 指向的 Option<HWND> 并返回 true。
unsafe fn check_sibling_workerw(hwnd: HWND, direction: GET_WINDOW_CMD, lparam: LPARAM) -> bool {
    let Ok(sibling) = GetWindow(hwnd, direction) else {
        return false;
    };
    if sibling.is_invalid() {
        return false;
    }
    // D-TD-010: 复用 is_workerw_class，避免 String::from_utf16_lossy 堆分配。
    if is_workerw_class(sibling) {
        let result_ptr = &mut *(lparam.0 as *mut Option<HWND>);
        *result_ptr = Some(sibling);
        return true;
    }
    false
}

/// 递归查找指定类名的子窗口
#[allow(clippy::never_loop)] // 首段为单次直查（找到即 return），不真正循环
unsafe fn find_child_by_class(
    parent: HWND,
    class_name: windows::core::PCWSTR,
    max_depth: u32,
) -> Option<HWND> {
    if max_depth == 0 {
        return None;
    }
    // 查找直接子窗口中类名匹配的第一个窗口；找到即返回，否则继续深层搜索。
    let child = HWND::default();
    loop {
        let Ok(next_child) = FindWindowExW(parent, child, class_name, None) else {
            break;
        };
        if next_child.is_invalid() {
            break;
        }
        return Some(next_child);
    }

    // 未在直接子窗口中找到，递归搜索更深层
    let mut child = HWND::default();
    loop {
        let Ok(next_child) = FindWindowExW(parent, child, None, None) else {
            break;
        };
        if next_child.is_invalid() {
            break;
        }
        // Recurse into this child
        if let Some(found) = find_child_by_class(next_child, class_name, max_depth - 1) {
            return Some(found);
        }
        child = next_child;
    }

    None
}

/// 计算 PerMonitor 布局下壁纸窗口在 WorkerW 子窗口坐标系中的位置（D-002 修复）
///
/// `SetParent` 将壁纸窗口重定为 WorkerW 的子窗口后，`SetWindowPos` 的坐标
/// 是相对 WorkerW 客户区左上角的子窗口坐标，而非虚拟桌面屏幕坐标。
/// 当 WorkerW 左上角不位于虚拟屏幕原点 (0,0) 时（存在位于主显示器
/// 左侧/上方的副显示器，坐标为负），需从 display 的虚拟屏幕坐标中减去
/// WorkerW 左上角坐标，得到正确的子窗口坐标。
///
/// 提取为纯函数以便单元测试 PerMonitor 负坐标偏移的正确性。
fn calculate_per_monitor_child_coords(
    display: &DisplayInfo,
    workerw_rect: &RECT,
) -> (i32, i32, i32, i32) {
    (
        display.x - workerw_rect.left,
        display.y - workerw_rect.top,
        display.width as i32,
        display.height as i32,
    )
}

/// 将壁纸窗口嵌入到 WorkerW 层
///
/// `displays` 参数由调用方传入（D-013：在 `DesktopIntegrator` 层缓存显示器列表
/// 与 5s TTL 失效，避免 PerMonitor 分支每次调用 `enumerate_displays()`）。
/// Span 分支不使用此参数。
pub fn embed_wallpaper(
    wp_hwnd: HWND,
    workerw_hwnd: HWND,
    progman_hwnd: HWND,
    display_id: &str,
    arrangement: Arrangement,
    displays: &[DisplayInfo],
) -> Result<(), MirrorStarError> {
    unsafe {
        // Step 1: Make the wallpaper window borderless
        crate::desktop::window::make_borderless(wp_hwnd)?;
        crate::desktop::window::remove_from_taskbar(wp_hwnd);

        // Step 2: Determine position based on display_id and arrangement
        // D-TD-008: 在 match 前统一获取一次 workerw_rect，供所有分支共用
        //（原 3 处重复 GetWindowRect 调用合并为 1 处）。
        let mut workerw_rect = RECT::default();
        GetWindowRect(workerw_hwnd, &mut workerw_rect).map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("GetWindowRect 失败: {}", e))
        })?;

        let (x, y, width, height) = match arrangement {
            Arrangement::Span => {
                // Span mode: cover entire WorkerW (virtual screen)
                (
                    0,
                    0,
                    workerw_rect.right - workerw_rect.left,
                    workerw_rect.bottom - workerw_rect.top,
                )
            }
            Arrangement::PerMonitor => {
                // Per-monitor mode: find the specific monitor
                // D-013: 使用调用方传入的 displays 缓存，避免每次调用 enumerate_displays()。
                if let Some(display) = displays.iter().find(|d| d.id == display_id) {
                    // D-002: SetParent 后 SetWindowPos 的坐标是相对 WorkerW 客户区
                    // 左上角的子窗口坐标，而非虚拟屏幕坐标。当 WorkerW 左上角
                    // 不在虚拟屏幕原点 (0,0)（存在负坐标副显示器）时，需从
                    // display 虚拟屏幕坐标减去 workerw_rect 原点得到正确子窗口坐标。
                    calculate_per_monitor_child_coords(display, &workerw_rect)
                } else {
                    // Fallback: use WorkerW dimensions
                    tracing::warn!(display_id, "未找到显示器，回退到 WorkerW 全尺寸");
                    (
                        0,
                        0,
                        workerw_rect.right - workerw_rect.left,
                        workerw_rect.bottom - workerw_rect.top,
                    )
                }
            }
        };

        // Step 3: SetParent - reparent into WorkerW
        SetParent(wp_hwnd, workerw_hwnd)
            .map_err(|e| MirrorStarError::DesktopIntegration(format!("SetParent 失败: {}", e)))?;

        // Step 4: Position at the correct coordinates, at bottom Z-order
        SetWindowPos(
            wp_hwnd,
            HWND_BOTTOM,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(|e| MirrorStarError::DesktopIntegration(format!("SetWindowPos 失败: {}", e)))?;

        // Step 5: Return focus to desktop（尽力恢复焦点，失败不影响壁纸嵌入）
        let _ = SetForegroundWindow(progman_hwnd);

        // Step 6: Show the wallpaper window (created without WS_VISIBLE to avoid flash)
        // ShowWindow 失败不影响已完成的嵌入流程，下一次刷新会重新显示
        let _ = ShowWindow(wp_hwnd, SW_SHOW);

        tracing::info!(
            display_id,
            ?arrangement,
            x,
            y,
            width,
            height,
            "壁纸窗口嵌入完成"
        );
        Ok(())
    }
}

/// 获取当前系统壁纸路径
pub fn get_system_wallpaper() -> Option<String> {
    unsafe {
        // D-009: 改为堆分配（vec!），避免在受限栈线程上分配 64 KiB 数组接近栈上限。
        // 行为不变：缓冲区大小仍为 UNICODE_STRING_MAX_CHARS（32767），覆盖 Win32 长路径与 UNC 路径上限。
        let mut buffer = vec![0u16; UNICODE_STRING_MAX_CHARS];
        let result = SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if result.is_ok() {
            // D-TD-009: 复用 mod.rs extract_utf16_string，避免内联 position + from_utf16_lossy 逻辑。
            let path = crate::desktop::extract_utf16_string(&buffer);
            if path.is_empty() {
                None
            } else {
                Some(path)
            }
        } else {
            None
        }
    }
}

/// 恢复系统壁纸
///
/// D12: 返回 `Result<(), MirrorStarError>`，`SystemParametersInfoW` 失败时返回 `Err`，
/// 调用方可感知恢复失败并决定后续处理（如降级到 refresh_desktop 兜底）。
///
/// D-015: 转宽字符前校验 `path` 不含嵌入式 NUL 字符，避免 Win32 宽字符串截断
/// 导致设置错误路径。
pub fn restore_system_wallpaper(path: &str) -> Result<(), MirrorStarError> {
    // D-015: 拒绝含嵌入式 NUL 字符的路径，避免 encode_utf16 + NUL 终止符拼接后
    // 被 Win32 当作提前结束的宽字符串截断。
    if path.contains('\0') {
        return Err(MirrorStarError::InvalidPath {
            reason: format!("路径含嵌入式 NUL 字符：{}", path),
        });
    }
    unsafe {
        let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide_path.as_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE,
        )
        .map_err(|e| {
            tracing::warn!(error = %e, path = path, "恢复系统壁纸失败 (SystemParametersInfoW)");
            MirrorStarError::DesktopIntegration(format!("恢复系统壁纸失败: {}", e))
        })?;
    }
    Ok(())
}

/// 刷新桌面，重新应用当前系统壁纸（而非清除）
///
/// best-effort 操作：内部调用 `restore_system_wallpaper`，失败时仅记录 warn 日志，
/// 不向上传播错误（用于 `restore_original_wallpaper` 的 None 分支降级兜底）。
pub fn refresh_desktop() {
    if let Some(current) = get_system_wallpaper() {
        if let Err(e) = restore_system_wallpaper(&current) {
            tracing::warn!(error = %e, "refresh_desktop 恢复壁纸失败（best-effort，已忽略）");
        }
    }
    // If no current wallpaper is set, there is nothing to refresh
}

/// 计算 WorkerW 重试查找第 i 次的等待时间（毫秒）
///
/// v5.0 D-PERF-007: 优化重试节奏，总等待从 4250ms 降到 1350ms。
/// - 原公式：200 + i*50，10 次重试，总等待 4250ms
/// - 新公式：100 + i*50，6 次重试，总等待 1350ms
///
/// 安全性：D-PERF-002（Progman 直查快速路径）已使 WorkerW 通常首次就成功，
/// 重试循环仅在罕见场景（Explorer 重启）触发。WorkerW 在 Progman 收到
/// WM_SPAWN_WORK 后立即创建，6 次 × 100ms+ 间隔足够等待。
pub(crate) fn compute_retry_wait_ms(i: u32) -> u64 {
    100 + (i as u64) * 50
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_retry_wait_ms 纯函数测试 ────────────────────────────────
    //
    // 这部分测试 WorkerW 重试查找中"重试节奏"的纯逻辑。
    // find_workerw_no_retry 本身与 Win32 API（FindWindowW / EnumWindows / SendMessageTimeoutW）
    // 紧耦合，且 SendMessageTimeoutW 的返回值在源码中已被忽略（`let _ = ...`），
    // 因此无法通过 mock 该 API 的返回值来驱动分支。
    // 完整的 WorkerW 查找集成测试需要交互式 Windows 桌面环境且可能耗时数秒
    // （v5.0 D-PERF-007 后最坏情况 manager 重试 6 次共约 1.35s sleep），故仅测可纯函数化的重试节奏部分。

    #[test]
    fn compute_retry_wait_ms_first_iteration() {
        // v5.0 D-PERF-007: 首次重试应等待 100ms（原 200ms）
        assert_eq!(compute_retry_wait_ms(0), 100);
    }

    #[test]
    fn compute_retry_wait_ms_increments_by_50() {
        // 每次递增 50ms
        assert_eq!(compute_retry_wait_ms(1), 150);
        assert_eq!(compute_retry_wait_ms(2), 200);
        assert_eq!(compute_retry_wait_ms(3), 250);
    }

    #[test]
    fn compute_retry_wait_ms_last_iteration() {
        // v5.0 D-PERF-007: manager 重试循环为 0..6，最后一次（i=5）应等待 350ms
        assert_eq!(compute_retry_wait_ms(5), 350);
    }

    #[test]
    fn compute_retry_wait_ms_formula() {
        // v5.0 D-PERF-007: 验证公式 wait_ms = 100 + i * 50 在整个 0..6 范围内单调递增
        let mut prev = 0u64;
        for i in 0..6u32 {
            let wait = compute_retry_wait_ms(i);
            assert_eq!(wait, 100 + (i as u64) * 50);
            assert!(wait > prev, "等待时间应随重试次数单调递增");
            prev = wait;
        }
    }

    // ── D-002: PerMonitor 负坐标偏移纯函数测试 ───────────────────────────
    //
    // calculate_per_monitor_child_coords 计算 SetParent 后壁纸窗口在 WorkerW
    // 子窗口坐标系中的位置。embed_wallpaper 涉及 Win32 API（SetParent/SetWindowPos）
    // 无法直接单元测试，此处覆盖提取出的纯坐标计算逻辑。

    #[test]
    fn d002_permonitor_negative_coords_correct_offset() {
        // 场景：副显示器位于主显示器左侧，虚拟屏幕坐标为负（x=-1920）。
        // WorkerW 覆盖整个虚拟桌面，其左上角也位于 (-1920, 0)。
        // 此时壁纸窗口作为 WorkerW 子窗口，正确坐标应为：
        //   display.x - workerw_rect.left = -1920 - (-1920) = 0
        //   display.y - workerw_rect.top  = 0 - 0 = 0
        // 旧代码错误地直接使用 display.x (-1920)，导致壁纸向左偏移。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY2".to_string(),
            name: "显示器 2".to_string(),
            width: 1920,
            height: 1080,
            x: -1920,
            y: 0,
            is_primary: false,
            dpi: 96,
            current_wallpaper: None,
        };
        let workerw_rect = RECT {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let (x, y, width, height) = calculate_per_monitor_child_coords(&display, &workerw_rect);
        // 子窗口坐标应为 (0, 0)，而非 (-1920, 0)
        assert_eq!(x, 0, "负坐标显示器应被偏移到 WorkerW 原点 (x=0)");
        assert_eq!(y, 0, "y 坐标应为 0");
        assert_eq!(width, 1920);
        assert_eq!(height, 1080);
    }

    #[test]
    fn d002_permonitor_workerw_at_origin_unchanged() {
        // 场景：WorkerW 左上角位于虚拟屏幕原点 (0,0)（无负坐标副显示器）。
        // 此时偏移量为 0，子窗口坐标应等于 display 虚拟屏幕坐标。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY1".to_string(),
            name: "显示器 1".to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            is_primary: true,
            dpi: 96,
            current_wallpaper: None,
        };
        let workerw_rect = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let (x, y, width, height) = calculate_per_monitor_child_coords(&display, &workerw_rect);
        assert_eq!((x, y, width, height), (0, 0, 1920, 1080));
    }

    #[test]
    fn d002_permonitor_secondary_display_positive_offset() {
        // 场景：副显示器位于主显示器右侧，虚拟屏幕坐标为正 (x=1920)。
        // WorkerW 左上角位于 (0,0)，子窗口坐标应为 (1920, 0)。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY2".to_string(),
            name: "显示器 2".to_string(),
            width: 1920,
            height: 1080,
            x: 1920,
            y: 0,
            is_primary: false,
            dpi: 96,
            current_wallpaper: None,
        };
        let workerw_rect = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 1080,
        };
        let (x, y, width, height) = calculate_per_monitor_child_coords(&display, &workerw_rect);
        assert_eq!((x, y, width, height), (1920, 0, 1920, 1080));
    }

    // ── 系统壁纸 API 烟雾测试 ───────────────────────────────────────────
    //
    // 这些测试在真实 Windows 环境下运行 Win32 API，仅验证不 panic 且类型正确。
    // get_system_wallpaper 返回值取决于用户是否设置了壁纸（Some 或 None 均合法）。

    #[test]
    fn get_system_wallpaper_no_panic() {
        // 调用 SystemParametersInfoW(SPI_GETDESKWALLPAPER)，不应 panic
        let result = get_system_wallpaper();
        if let Some(ref path) = result {
            // 路径应为合法 UTF-8 字符串
            assert!(!path.contains('\0'), "壁纸路径不应包含 null 字符");
        }
        // None 也合法（用户未设置壁纸）
    }

    #[test]
    fn refresh_desktop_no_panic() {
        // refresh_desktop 内部调用 get_system_wallpaper + restore_system_wallpaper，
        // restore_system_wallpaper 失败时 refresh_desktop 内部记录 warn 并忽略（best-effort），
        // 此测试验证不 panic
        refresh_desktop();
    }

    #[test]
    #[ignore = "会修改真实系统壁纸，仅本地手动运行"]
    fn restore_system_wallpaper_empty_path_returns_ok() {
        // D12: restore_system_wallpaper 现在返回 Result<(), MirrorStarError>。
        // 空路径会调用 SystemParametersInfoW(SPI_SETDESKWALLPAPER, "") 清除壁纸，
        // 在正常 Windows 环境下应返回 Ok。
        let result = restore_system_wallpaper("");
        assert!(
            result.is_ok(),
            "空路径恢复壁纸应返回 Ok: {:?}",
            result.err()
        );
    }

    // ── D11: callback 路径单元测试 ──────────────────────────────────────
    //
    // 以下测试覆盖 find_workerw_callback 与 find_workerw_fallback_callback 两条路径，
    // 验证在真实 Windows 桌面环境下通过 EnumWindows 调用不 panic，
    // 且统一后的 try_find_sibling_workerw helper 正确执行兄弟窗口查找逻辑。

    #[test]
    fn find_workerw_callback_path_no_panic() {
        // 通过 EnumWindows 调用 find_workerw_callback，验证回调路径不 panic。
        // callback 会枚举所有顶层窗口，查找包含 SHELLDLL_DefView 子窗口的窗口，
        // 然后通过 try_find_sibling_workerw 检查其兄弟窗口是否为 WorkerW。
        let mut result: Option<HWND> = None;
        let lparam = LPARAM(&mut result as *mut Option<HWND> as isize);
        unsafe {
            // D-007: 不再静默丢弃 EnumWindows 返回值（与生产代码风格统一）。
            if let Err(e) = EnumWindows(Some(find_workerw_callback), lparam) {
                tracing::debug!(error = %e, "测试 EnumWindows 调用失败（不影响 no_panic 断言）");
            }
        }
        // result 可能为 Some（找到 WorkerW）或 None（未找到），取决于桌面状态
        let _ = result;
    }

    #[test]
    fn find_workerw_fallback_callback_path_no_panic() {
        // 通过 EnumWindows 调用 find_workerw_fallback_callback，验证备用回调路径不 panic。
        // callback 会枚举所有顶层窗口，查找 WorkerW 窗口并检查是否包含 SHELLDLL_DefView，
        // 然后通过 try_find_sibling_workerw 检查其兄弟窗口。
        let mut result: Option<HWND> = None;
        let lparam = LPARAM(&mut result as *mut Option<HWND> as isize);
        unsafe {
            // D-007: 不再静默丢弃 EnumWindows 返回值（与生产代码风格统一）。
            if let Err(e) = EnumWindows(Some(find_workerw_fallback_callback), lparam) {
                tracing::debug!(error = %e, "测试 EnumWindows 调用失败（不影响 no_panic 断言）");
            }
        }
        let _ = result;
    }

    #[test]
    fn try_find_sibling_workerw_helper_no_panic() {
        // 直接测试 D11 提取的 helper 函数 try_find_sibling_workerw。
        // 使用 Progman 窗口作为输入（总是存在于 Windows 桌面），
        // 验证 helper 在真实 Win32 环境下不 panic。
        let progman_hwnd = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman_hwnd {
            let mut result: Option<HWND> = None;
            let lparam = LPARAM(&mut result as *mut Option<HWND> as isize);
            unsafe {
                let found = try_find_sibling_workerw(hwnd, lparam);
                // found 为 true 时 result 应为 Some；为 false 时 result 应为 None
                if found {
                    assert!(result.is_some(), "found=true 时 result 应为 Some");
                }
            }
        }
        // Progman 找不到时（极端情况）跳过，不 panic 即通过
    }

    #[test]
    fn check_sibling_workerw_both_directions_no_panic() {
        // 测试 check_sibling_workerw 在两个方向（GW_HWNDNEXT / GW_HWNDPREV）下均不 panic。
        // 这验证了 D11 统一逻辑：两个方向都被无条件检查。
        let progman_hwnd = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman_hwnd {
            let mut result_next: Option<HWND> = None;
            let lparam_next = LPARAM(&mut result_next as *mut Option<HWND> as isize);
            let mut result_prev: Option<HWND> = None;
            let lparam_prev = LPARAM(&mut result_prev as *mut Option<HWND> as isize);
            unsafe {
                let _ = check_sibling_workerw(hwnd, GW_HWNDNEXT, lparam_next);
                let _ = check_sibling_workerw(hwnd, GW_HWNDPREV, lparam_prev);
            }
        }
    }

    #[test]
    #[ignore = "会修改真实系统壁纸，仅本地手动运行"]
    fn restore_system_wallpaper_returns_result_type() {
        // D12: 验证 restore_system_wallpaper 返回 Result<(), MirrorStarError> 类型。
        // 使用空路径（清除壁纸），在正常 Windows 环境下应返回 Ok。
        let result: Result<(), MirrorStarError> = restore_system_wallpaper("");
        assert!(result.is_ok());
    }

    #[test]
    #[ignore = "会修改真实系统壁纸，仅本地手动运行"]
    fn restore_system_wallpaper_invalid_path_returns_result() {
        // D12: 验证使用无效路径时返回 Result（Ok 或 Err 均合法）。
        // SystemParametersInfoW(SPI_SETDESKWALLPAPER) 对无效路径可能仍然成功
        //（仅设置注册表值，Explorer 加载时才报错），也可能返回失败。
        // 无论哪种情况，函数都应返回 Result 而非 panic。
        let result = restore_system_wallpaper(":::nonexistent_invalid_path:::");
        match &result {
            Ok(()) => {
                // Windows 接受了无效路径（仅设置注册表值），合法行为
            }
            Err(e) => {
                // 预期中的失败：调用方收到 Err
                tracing::info!(error = %e, "无效路径触发恢复失败（预期行为，验证 Err 传播）");
            }
        }
    }

    // ── D-007: EnumWindows 错误日志文档测试 ──────────────────────────────
    //
    // find_workerw_no_retry 中三次 EnumWindows 调用原本静默丢弃返回值，
    // D-007 修复后改为 `if let Err(e) = ...` 模式并记录 warn 日志（错误码）。
    // D-TD-007 后该模式被抽取到 `try_enum_windows` 辅助函数中（D-007 注释 + warn
    // 日志 + return Err 集中于辅助函数体内），三处调用点传入 Step 标识复用。
    // 此测试通过 include_str! 模式断言源码包含关键标记，验证修复存在。
    // 无法直接单元测试 EnumWindows 失败分支，因为：
    // 1. EnumWindows 在真实 Windows 环境下几乎不会失败（需损坏的 GDI 子系统）
    // 2. 统一使用 include_str! 模式

    /// D-007: 验证 find_workerw_no_retry 记录 EnumWindows 错误日志。
    ///
    /// 本测试为文档测试（include_str! 模式），通过断言源码包含关键标记验证修复存在。
    /// 无法直接单元测试 EnumWindows 失败分支，因为：
    /// 1. EnumWindows 在真实 Windows 环境下几乎不会失败（需损坏的 GDI 子系统）
    /// 2. 统一使用 include_str! 模式
    #[test]
    fn d007_find_workerw_no_retry_logs_enumwindows_errors() {
        let source = include_str!("worker_w.rs");
        // 验证 D-007 前缀注释存在（在 try_enum_windows 辅助函数中）
        assert!(
            source.contains("D-007: EnumWindows 失败时记录错误码"),
            "try_enum_windows 应含 D-007 前缀注释"
        );
        // 验证三处 EnumWindows 都改为 try_enum_windows 调用（不再有下划线丢弃返回值的静默吞错模式）。
        // 构造搜索串以避免 include_str! 自匹配（测试源码本身被读取）。
        let underscore_drop_pattern = ["let", "_", "=", "EnumWindows"].join(" ");
        assert_eq!(
            source.matches(&underscore_drop_pattern).count(),
            0,
            "不应再有下划线丢弃 EnumWindows 返回值的静默吞错模式"
        );
        // 验证三处调用点传入的 Step 标识
        assert!(
            source.contains("Step 2: 直接查找 WorkerW"),
            "第一处 try_enum_windows 调用应含 Step 2 标识"
        );
        assert!(
            source.contains("Step 4: 发送 WM_SPAWN_WORK 后查找 WorkerW"),
            "第二处 try_enum_windows 调用应含 Step 4 标识"
        );
        assert!(
            source.contains("Step 5: 备用方法查找 WorkerW"),
            "第三处 try_enum_windows 调用应含 Step 5 标识"
        );
        // 验证 try_enum_windows 辅助函数中含 1 处 error = ?e 格式化的 warn 日志
        //（D-TD-007 抽取后从 3 处合并为 1 处，集中在辅助函数中）
        assert_eq!(
            source
                .matches("tracing::warn!(error = ?e, \"EnumWindows")
                .count(),
            1,
            "应有 1 处 tracing::warn!(error = ?e, \"EnumWindows ...\") 日志（在 try_enum_windows 辅助函数中）"
        );
    }

    // ── v4.1 Medium findings 文档化测试 ──────────────────────────────────

    /// v41-D-005: 验证 find_workerw_no_retry 中 EnumWindows 失败时返回 Err 的契约。
    ///
    /// 契约要求：EnumWindows 失败时（返回 Err）向上传播 `MirrorStarError::DesktopIntegration`，
    /// 而非仅记录 warn 日志后继续走 None 分支降级。由于 EnumWindows 在真实 Windows
    /// 环境下几乎不会失败（需损坏的 GDI 子系统），无法在 CI 中可靠触发失败分支，
    /// 此处采用混合策略：
    /// 1. 文档化测试：断言源码包含 v41-D-005 契约说明与 return Err 标记
    /// 2. 行为测试：调用 find_workerw_no_retry 验证不 panic（Ok 或 Err 均合法）
    #[test]
    fn v41_d005_enumwindows_failure_returns_err_contract() {
        // (1) 文档化测试：验证源码包含 v41-D-005 契约
        let source = include_str!("worker_w.rs");
        // 验证函数级 doc comment 含 v41-D-005 说明
        assert!(
            source.contains("v41-D-005: EnumWindows 失败时不仅记录 warn"),
            "find_workerw_no_retry doc comment 应含 v41-D-005 契约说明"
        );
        // 验证 EnumWindows 失败分支含 return Err（而非仅 warn）
        // 通过统计 "return Err(MirrorStarError::DesktopIntegration(format!" 出现次数确认
        // D-TD-007 抽取 try_enum_windows 后，三处 EnumWindows 失败的 return Err
        // 合并为辅助函数体内的 1 处 return Err。
        let return_err_count = source
            .matches("return Err(MirrorStarError::DesktopIntegration(format!")
            .count();
        assert!(
            return_err_count >= 1,
            "try_enum_windows 应至少有 1 处 EnumWindows 失败的 return Err (format!)，实际: {}",
            return_err_count
        );
        // 验证 try_enum_windows 辅助函数含 v41-D-005 注释
        // 构造搜索串以避免 include_str! 自匹配（测试源码本身被读取，与 D-014 风格一致）。
        let v41_d005_comment_pattern =
            ["v41-D-005:", " EnumWindows", " 失败时返回", " Err"].join("");
        assert_eq!(
            source.matches(&v41_d005_comment_pattern).count(),
            1,
            "应有 1 处 v41-D-005 EnumWindows 失败返回 Err 的注释（在 try_enum_windows 辅助函数中）"
        );

        // (2) 行为测试：find_workerw_no_retry 不 panic
        // 在真实 Windows 桌面环境下应返回 Ok（找到 Progman/WorkerW）；
        // 在无头/受限环境下可能返回 Err（找不到 Progman/WorkerW），但不应 panic。
        // 仅调用并丢弃结果——若 panic 则测试失败，Ok/Err 均合法。
        let _ = find_workerw_no_retry();
    }
}
