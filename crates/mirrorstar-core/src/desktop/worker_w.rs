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

/// Progman 触发 WorkerW 创建的窗口消息
///
/// 发送此消息给 Progman 窗口会触发其创建 WorkerW 层，用于嵌入壁纸窗口。
const WM_SPAWN_WORK: u32 = 0x052C;

/// WM_SPAWN_WORK 的 WPARAM 参数
///
/// 对齐 Lively Wallpaper / electron-as-wallpaper 的实现：使用 0xD 而非 0，
/// 以在 Windows 11 24H2/25H2 的 raised desktop 下正确触发 WorkerW 创建。
const WM_SPAWN_WORK_WPARAM: usize = 0xD;

/// WM_SPAWN_WORK 的 LPARAM 参数
///
/// 对齐 Lively Wallpaper / electron-as-wallpaper 的实现：使用 0x1 而非 0。
const WM_SPAWN_WORK_LPARAM: isize = 0x1;

/// WM_SPAWN_WORK 消息超时（ms），用于 SendMessageTimeoutW 调用（D-012）
///
/// 1000ms：给 Progman 足够时间创建子 WorkerW（raised desktop 下创建较慢）。
const WM_SPAWN_WORK_TIMEOUT_MS: u32 = 1000;

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

/// 判断扩展窗口样式中是否含 `WS_EX_NOREDIRECTIONBITMAP`。
///
/// Windows 11 raised desktop 下 Progman 带此扩展样式（无重定向位图），
/// 可据此区分新旧桌面层级结构。
fn has_noredirection_bitmap(ex_style: u32) -> bool {
    ex_style & WS_EX_NOREDIRECTIONBITMAP.0 != 0
}

/// 判断指定 Progman 窗口是否处于 Windows 11 raised desktop 模式。
///
/// raised desktop 的标志：Progman 带 `WS_EX_NOREDIRECTIONBITMAP` 扩展样式，
/// 此时 `SHELLDLL_DefView` 是其 `WS_EX_LAYERED` 子窗口，承载壁纸的 WorkerW
/// 变成 Progman 的直接子窗口（不再有顶层兄弟 WorkerW）。
pub fn progman_is_raised_desktop(progman: HWND) -> bool {
    // SAFETY: GetWindowLongPtrW 仅读取指定窗口的扩展样式，不修改窗口状态。
    unsafe { has_noredirection_bitmap(GetWindowLongPtrW(progman, GWL_EXSTYLE) as u32) }
}

/// 判断当前桌面是否处于 Windows 11 raised desktop 模式。
///
/// 内部查找 Progman 窗口；找不到或句柄无效时返回 `false`（不 panic、不记 error）。
pub fn is_raised_desktop() -> bool {
    // SAFETY: FindWindowW 仅查找窗口，不修改任何窗口状态。
    match unsafe { FindWindowW(windows::core::w!("Progman"), None) } {
        Ok(progman) if !progman.is_invalid() => progman_is_raised_desktop(progman),
        _ => false,
    }
}

/// 查找 Progman 的直接子 WorkerW（raised desktop 下的壁纸承载层）。
///
/// 旧层级中 WorkerW 是 Progman 的顶层兄弟窗口；raised desktop 下 WorkerW
/// 改为 Progman 的直接子窗口，需用 `FindWindowExW(progman, ...)` 查找。
/// 句柄有效且类名确为 WorkerW 时返回 `Some`，否则返回 `None`。
fn find_progman_child_workerw(progman: HWND) -> Option<HWND> {
    // SAFETY: FindWindowExW 仅查找子窗口，不修改窗口状态。
    let Ok(workerw) =
        (unsafe { FindWindowExW(progman, HWND::default(), windows::core::w!("WorkerW"), None) })
    else {
        return None;
    };
    if !workerw.is_invalid() && is_workerw_class(workerw) {
        Some(workerw)
    } else {
        None
    }
}

/// 查找 Progman 的直接子窗口 `SHELLDLL_DefView`（桌面图标层）。
///
/// raised desktop 下壁纸窗口必须插入到 DefView 的正下方（z-order 锚点），
/// 使桌面图标与文字保持可见、可点击。句柄无效时返回 `None`。
fn find_defview(progman: HWND) -> Option<HWND> {
    // SAFETY: FindWindowExW 仅查找子窗口，不修改窗口状态。
    let Ok(defview) = (unsafe {
        FindWindowExW(
            progman,
            HWND::default(),
            windows::core::w!("SHELLDLL_DefView"),
            None,
        )
    }) else {
        return None;
    };
    (!defview.is_invalid()).then_some(defview)
}

/// 深度查找 `SHELLDLL_DefView`（桌面图标层）。
///
/// 先按历史行为查 Progman 的直接子窗口；未命中时（DefView 可能挂在 Progman
/// 直接子 WorkerW 的子树里，H2 场景）再对 Progman 的每个直接子窗口递归查找
///（深度 ≤ [`MAX_CHILD_DEPTH`]）。句柄无效时返回 `None`。
fn find_defview_deep(progman: HWND) -> Option<HWND> {
    if let Some(defview) = find_defview(progman) {
        return Some(defview);
    }

    // SAFETY: FindWindowExW/GetWindow 仅查找、遍历窗口，不修改窗口状态。
    unsafe {
        let mut child = HWND::default();
        loop {
            let Ok(next_child) = FindWindowExW(progman, child, None, None) else {
                break;
            };
            if next_child.is_invalid() {
                break;
            }
            if let Some(found) = find_child_by_class(
                next_child,
                windows::core::w!("SHELLDLL_DefView"),
                MAX_CHILD_DEPTH,
            ) {
                return Some(found);
            }
            child = next_child;
        }
    }
    None
}

/// 壁纸窗口的嵌入路径选择结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedPath {
    /// raised desktop 且 DefView 可用：layered 子窗口 + 锚点插入
    RaisedLayered(HWND),
    /// 旧层级，或 raised 但 DefView 缺失时的降级
    LegacyWorkerW,
}

/// 纯函数：按桌面层级与 DefView 可用性选择嵌入路径。
///
/// - `raised == true` 且 DefView 句柄有效 → `RaisedLayered(defview)`；
/// - 其余情况（旧层级，或 raised 但 DefView 缺失的用户状态）→ `LegacyWorkerW`，
///   即落回旧层级嵌入（`SetParent(WorkerW)` + `HWND_BOTTOM`）。
///
/// `Option<HWND>` 中的无效句柄（如 `HWND::default()`）按缺失归一化处理；
/// 本函数不调用任何 Win32 API，便于单测四象限。
fn choose_embed_path(raised: bool, defview: Option<HWND>) -> EmbedPath {
    match defview.filter(|h| !h.is_invalid()) {
        Some(defview) if raised => EmbedPath::RaisedLayered(defview),
        _ => EmbedPath::LegacyWorkerW,
    }
}

/// 判断 `hwnd` 的子树（深度 ≤ [`MAX_CHILD_DEPTH`]）是否包含指定类名的窗口。
///
/// 薄封装 [`find_child_by_class`]（该函数不检查 `hwnd` 自身，仅查其子孙），
/// 用于 `SysListView32` / `SHELLDLL_DefView` 归属探测。
fn subtree_has_class(hwnd: HWND, class_name: windows::core::PCWSTR) -> bool {
    // SAFETY: find_child_by_class 仅查找子窗口，不修改窗口状态。
    unsafe { find_child_by_class(hwnd, class_name, MAX_CHILD_DEPTH).is_some() }
}

/// 读取窗口类名；句柄无效或读取失败时返回 `"?"`（仅用于诊断日志）。
fn class_name_of(hwnd: HWND) -> String {
    if hwnd.is_invalid() {
        return "?".to_string();
    }
    let mut buf = [0u16; 256];
    // SAFETY: GetClassNameW 读取窗口类名到栈缓冲区，256 >= 任何窗口类名长度。
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        "?".to_string()
    }
}

/// 纯判定：宿主是否属于 Progman 的窗口层级。
///
/// 成立条件：宿主有效，且宿主本身等于 Progman，或宿主的父窗口等于 Progman。
/// 输入均为调用方已探测好的 HWND 值，本函数不调用任何 Win32 API（便于单测）。
fn host_is_under_progman(host: HWND, progman: HWND, host_parent: HWND) -> bool {
    !host.is_invalid() && !progman.is_invalid() && (host == progman || host_parent == progman)
}

/// 纯函数：选择壁纸窗口的宿主（即 DefView 的直接父窗口）。
///
/// 仅当 [`host_is_under_progman`] 成立时返回 `Some(宿主)`，否则返回 `None`。
/// 不调用 Win32 API，便于单测判定矩阵。
fn choose_wallpaper_host(defview_parent: HWND, host_parent: HWND, progman: HWND) -> Option<HWND> {
    host_is_under_progman(defview_parent, progman, host_parent).then_some(defview_parent)
}

/// Win32 探针：解析 raised desktop 下壁纸窗口的宿主（DefView 的直接父窗口）。
///
/// 通过 `GetAncestor(GA_PARENT)` 取得 DefView 的父窗口及其自身的父窗口，
/// 交给纯逻辑 [`choose_wallpaper_host`] 判定；不满足时返回 `None`。
fn resolve_wallpaper_host(defview: HWND, progman: HWND) -> Option<HWND> {
    // SAFETY: GetAncestor 仅查询窗口父链，不修改窗口状态。
    unsafe {
        let host = GetAncestor(defview, GA_PARENT);
        if host.is_invalid() {
            return None;
        }
        let host_parent = GetAncestor(host, GA_PARENT);
        choose_wallpaper_host(host, host_parent, progman)
    }
}

/// 汇总 Progman 直接子窗口链（`GW_CHILD` → `GW_HWNDNEXT`），供诊断日志使用。
///
/// 每项格式为 `"类名#vis=0/1#0x句柄"`；最多遍历 64 个兄弟窗口以防异常 z 序导致
/// 死循环，任何 Win32 调用失败都静默结束（不 panic、不修改窗口状态）。
fn progman_child_chain_summary(progman: HWND) -> Vec<String> {
    /// 单层最多遍历的兄弟窗口数（防止死循环）。
    const MAX_SIBLINGS: usize = 64;

    let mut chain = Vec::new();
    // SAFETY: GetWindow/GetClassNameW/IsWindowVisible 仅查询窗口关系与状态。
    unsafe {
        let Ok(mut child) = GetWindow(progman, GW_CHILD) else {
            return chain;
        };
        while !child.is_invalid() && chain.len() < MAX_SIBLINGS {
            let class = class_name_of(child);
            let visible = u8::from(IsWindowVisible(child).as_bool());
            chain.push(format!(
                "{}#vis={}#0x{:X}",
                class, visible, child.0 as isize
            ));
            let Ok(next) = GetWindow(child, GW_HWNDNEXT) else {
                break;
            };
            if next.is_invalid() {
                break;
            }
            child = next;
        }
    }
    chain
}

/// 单次尝试查找 Progman 和 WorkerW 窗口句柄（无重试、无 sleep）
///
/// 按桌面层级分流两条查找路径：
///
/// - 旧层级（`raised == false`）：FindWindowW(Progman) → Progman 直查（SHELLDLL_DefView 父窗口为
///   WorkerW / 顶层兄弟 WorkerW）→ 顶层 EnumWindows → SendMessageTimeoutW(WM_SPAWN_WORK) →
///   再次 EnumWindows → fallback。
/// - raised desktop（Progman 带 `WS_EX_NOREDIRECTIONBITMAP`）：只探测 Progman 的直接子 WorkerW
///   （发送 `0x052C` 前后各一次），跳过顶层兄弟 WorkerW 与全部顶层 `EnumWindows`，
///   避免误选第三方壁纸工具创建的顶层 WorkerW。
///
/// 适用于在持有 desktop 锁时调用，单次执行时间约 5ms（典型）至 1000ms（SendMessageTimeoutW 超时）。
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

        // Windows 11 24H2/25H2 raised desktop 下，WorkerW 是 Progman 的直接子窗口，
        // 旧层级的“顶层兄弟 WorkerW / 顶层 EnumWindows”路径不适用，且可能误命中
        // 第三方壁纸工具创建的顶层 WorkerW，因此 raised 下只走子窗口探测。
        let raised = progman_is_raised_desktop(progman_hwnd);

        // v5.0 D-PERF-002: Progman 直查快速路径
        // SHELLDLL_DefView 通常直接挂在 Progman 下，先直查 Progman 子窗口，
        // 命中则取其父窗口（WorkerW）或 Progman 的兄弟 WorkerW，将 EnumWindows
        // 退化为极少触发的 fallback。直查失败时静默退回 EnumWindows 全量扫描，不报错。
        if !raised {
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
        }
        // 非 raised desktop 下 Progman 直查失败时退回 EnumWindows 全量扫描（原有逻辑）；raised 下不执行顶层枚举

        // Progman 直查未命中时（与 SHELLDLL_DefView 是否存在无关），探测 Progman 的
        // 直接子 WorkerW：raised desktop 下这是唯一有效承载层；旧层级下该探测通常不命中，
        // 不影响后续 Step 2/4/5 的原有顺序。
        if let Some(w) = find_progman_child_workerw(progman_hwnd) {
            tracing::debug!(
                progman = progman_hwnd.0 as isize,
                workerw = w.0 as isize,
                "raised desktop: 找到 Progman 直接子 WorkerW"
            );
            return Ok((progman_hwnd, w));
        }

        // Step 2: Try to find WorkerW without sending message (it may already exist)
        if !raised {
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
        }

        // Step 3: SendMessageTimeoutW(progman_hwnd, WM_SPAWN_WORK, ...) to trigger WorkerW creation
        // 注意：不检查返回值，因为即使返回 0，WorkerW 也可能已经创建
        // 此消息幂等：若 WorkerW 已存在，Progman 不会重复创建
        // 不关心返回值，传 None 避免 dead store（D09）
        //
        // v41-D-003: 增加 SMTO_ABORTIFHUNG flag，若 Progman 消息处理线程挂起
        //（典型 < 5s）则立即返回，避免持锁调用方（ensure_workerw_ready）长时间阻塞。
        let _ = SendMessageTimeoutW(
            progman_hwnd,
            WM_SPAWN_WORK,
            WPARAM(WM_SPAWN_WORK_WPARAM),
            LPARAM(WM_SPAWN_WORK_LPARAM),
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            WM_SPAWN_WORK_TIMEOUT_MS,
            None,
        );

        // Step 4: 再次查找 WorkerW（raised desktop 下仅做 Progman 子窗口探测，非 raised 额外做 EnumWindows）

        // 先探测一次 Progman 的直接子 WorkerW：raised desktop 下 WM_SPAWN_WORK 创建的就是该子窗口（唯一候选），
        // 旧层级下通常不命中，不影响后续 EnumWindows。
        if let Some(w) = find_progman_child_workerw(progman_hwnd) {
            tracing::debug!(
                progman = progman_hwnd.0 as isize,
                workerw = w.0 as isize,
                "raised desktop: 发送 WM_SPAWN_WORK 后找到 Progman 直接子 WorkerW"
            );
            return Ok((progman_hwnd, w));
        }

        if !raised {
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
        }

        // Step 5: Fallback - 查找所有 WorkerW 窗口
        if !raised {
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
/// 找到包含 SHELLDLL_DefView 的 WorkerW，然后取其兄弟 WorkerW
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
unsafe fn find_child_by_class(
    parent: HWND,
    class_name: windows::core::PCWSTR,
    max_depth: u32,
) -> Option<HWND> {
    if max_depth == 0 {
        return None;
    }
    // 查找直接子窗口中类名匹配的第一个窗口；找到即返回，否则继续深层搜索。
    if let Ok(child) = FindWindowExW(parent, HWND::default(), class_name, None) {
        if !child.is_invalid() {
            return Some(child);
        }
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

/// 计算 PerMonitor 布局下壁纸窗口在宿主子窗口坐标系中的位置（D-002 修复）
///
/// `SetParent` 将壁纸窗口重定为宿主（WorkerW 或 raised desktop 的 Progman）的
/// 子窗口后，`SetWindowPos` 的坐标是相对宿主客户区左上角的子窗口坐标，而非
/// 虚拟桌面屏幕坐标。当宿主左上角不位于虚拟屏幕原点 (0,0) 时（存在位于主显示器
/// 左侧/上方的副显示器，坐标为负），需从 display 的虚拟屏幕坐标中减去
/// 宿主左上角坐标，得到正确的子窗口坐标。
///
/// 提取为纯函数以便单元测试 PerMonitor 负坐标偏移的正确性。
fn calculate_per_monitor_child_coords(
    display: &DisplayInfo,
    host_rect: &RECT,
) -> (i32, i32, i32, i32) {
    (
        display.x - host_rect.left,
        display.y - host_rect.top,
        display.width as i32,
        display.height as i32,
    )
}

/// 按布局方式计算壁纸窗口相对宿主窗口（WorkerW / Progman）的子窗口坐标。
///
/// - `Span`：铺满宿主窗口（虚拟桌面）全尺寸，原点 (0,0)
/// - `PerMonitor` / `AllSame`：目标显示器虚拟屏幕坐标减去宿主原点；未找到显示器时
///   回退到宿主全尺寸（与历史行为一致）
///
/// 宿主为 Progman（raised desktop）时与 WorkerW 的坐标语义相同：两者都覆盖虚拟桌面。
fn calculate_embed_coords(
    host_rect: &RECT,
    display_id: &str,
    arrangement: Arrangement,
    displays: &[DisplayInfo],
) -> (i32, i32, i32, i32) {
    match arrangement {
        Arrangement::Span => (
            0,
            0,
            host_rect.right - host_rect.left,
            host_rect.bottom - host_rect.top,
        ),
        Arrangement::PerMonitor | Arrangement::AllSame => {
            // D-013: 使用调用方传入的 displays 缓存，避免每次调用 enumerate_displays()。
            // AllSame 编排每个显示器各设一次独立渲染器，定位与 PerMonitor 一致。
            if let Some(display) = displays.iter().find(|d| d.id == display_id) {
                calculate_per_monitor_child_coords(display, host_rect)
            } else {
                // Fallback: use host dimensions
                tracing::warn!(display_id, "未找到显示器，回退到宿主窗口全尺寸");
                (
                    0,
                    0,
                    host_rect.right - host_rect.left,
                    host_rect.bottom - host_rect.top,
                )
            }
        }
    }
}

/// 保证 raised desktop 下 WorkerW 位于 Progman 子窗口 z-order 的最底层。
///
/// Windows 11 24H2/25H2 的 raised desktop 下，壁纸承载 WorkerW 是 Progman 的
/// 直接子窗口，而桌面图标层 `SHELLDLL_DefView` 也挂在 Progman 下。WorkerW 必须
/// 是 Progman 的最后一个（最底）子窗口，否则壁纸窗口会盖住桌面图标。
///
/// 仅对 raised desktop 生效：旧层级（WorkerW 为顶层兄弟窗口）直接返回，
/// 零行为变更。本函数为 best-effort：`SetWindowPos` 失败仅记录 warn，不返回 Err。
pub fn ensure_workerw_zorder(progman: HWND, workerw: HWND) {
    if !progman_is_raised_desktop(progman) {
        return;
    }

    // 宿主复核：raised desktop 下壁纸宿主是 DefView 的直接父窗口（Progman 或
    // Progman 直接子 WorkerW）。宿主恰为该 WorkerW 时，置底它只改变它与 Progman
    // 其它直接子窗口的先后，不影响其内部 DefView 与壁纸窗口的相对 z 序；
    // 宿主为 Progman 时，置底 WorkerW 也是把承载层移到 DefView 之下，同样满足
    // "壁纸位于图标层之下"。两种情况均无需跳过置底。
    //
    // EnumChildWindows 会递归枚举子孙窗口（壁纸宿主已挂到 WorkerW 时 last_child 永远不是 WorkerW），
    // 因此这里改用 GetWindow 只遍历 Progman 的直接子窗口：
    // GW_CHILD 取首个（最上层）直接子窗口，GW_HWNDLAST 在同一层级内取最底层兄弟。
    // SAFETY: GetWindow 仅查询窗口关系，不修改窗口状态。
    if let Ok(first_child) = unsafe { GetWindow(progman, GW_CHILD) } {
        if !first_child.is_invalid() {
            // SAFETY: first_child 是 Progman 的直接子窗口，GW_HWNDLAST 返回同层级最底层兄弟。
            if let Ok(last_child) = unsafe { GetWindow(first_child, GW_HWNDLAST) } {
                if last_child == workerw {
                    return;
                }
            }
        }
    }

    // SAFETY: SetWindowPos 仅调整 z-order，不移动、不改变大小、不激活窗口。
    match unsafe {
        SetWindowPos(
            workerw,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    } {
        Ok(()) => tracing::debug!(
            progman = progman.0 as isize,
            workerw = workerw.0 as isize,
            "raised desktop: 已将 WorkerW 置为 Progman 最底层子窗口"
        ),
        Err(e) => tracing::warn!(
            progman = progman.0 as isize,
            workerw = workerw.0 as isize,
            error = %e,
            "raised desktop: 调整 WorkerW z-order 失败（壁纸可能遮挡桌面图标）"
        ),
    }
}

/// 纯判定：窗口的上一个兄弟窗口（`GW_HWNDPREV`）是否正是 z 序锚点。
///
/// `prev_sibling == anchor` 表示窗口紧贴在锚点下方（z 序更低），即壁纸位于
/// 桌面图标层之下。不调用 Win32 API，便于单测。
fn is_below_anchor(prev_sibling: HWND, anchor: HWND) -> bool {
    !anchor.is_invalid() && prev_sibling == anchor
}

/// 校验 raised desktop 下壁纸窗口是否紧贴 z 序锚点（DefView）下方（best-effort）。
///
/// 流程：
/// 1. `GetWindow(hwnd, GW_HWNDPREV)` 与锚点比对，通过即返回 `true`；
/// 2. 不通过则用 `SetWindowPos(hwnd, anchor, ...)` 重试一次并再次校验；
/// 3. 仍不通过则回退到宿主内最底层（`HWND_BOTTOM`）并记录 `warn`，返回 `false`。
///
/// 不返回 `Err`：z 序校验失败不应让已完成的嵌入整体失败，兜底后由日志
/// `z_anchor_ok=false` 暴露问题。
fn ensure_below_anchor(hwnd: HWND, anchor: HWND, progman: HWND) -> bool {
    // SAFETY: GetWindow/SetWindowPos 仅查询或调整 z 序，不移动、不改变大小、不激活窗口。
    unsafe {
        if GetWindow(hwnd, GW_HWNDPREV).is_ok_and(|prev| is_below_anchor(prev, anchor)) {
            return true;
        }

        // 重试一次：锚点插入可能被后续同步消息打回兄弟 z 序顶部。
        if SetWindowPos(
            hwnd,
            anchor,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .is_ok()
            && GetWindow(hwnd, GW_HWNDPREV).is_ok_and(|prev| is_below_anchor(prev, anchor))
        {
            return true;
        }

        // 兜底：放到宿主内最底层，保证壁纸不会停留在兄弟 z 序顶部遮挡图标。
        let prev = GetWindow(hwnd, GW_HWNDPREV).unwrap_or_default();
        let fallback_ok = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .is_ok();
        tracing::warn!(
            hwnd = hwnd.0 as isize,
            anchor = anchor.0 as isize,
            progman = progman.0 as isize,
            prev = prev.0 as isize,
            fallback_ok,
            "raised desktop: z 序锚点校验失败，已回退到宿主最底层（壁纸可能覆盖桌面图标）"
        );
        false
    }
}

/// 将壁纸窗口嵌入桌面壁纸层。
///
/// 按桌面层级分两条路径（`progman_hwnd` 由 `DesktopIntegrator` 传入，避免重复
/// `FindWindowW`）：
///
/// - **raised desktop**（Progman 带 `WS_EX_NOREDIRECTIONBITMAP`）：按 Lively/Microsoft
///   模型把窗口设为 `WS_CHILD` + `WS_EX_LAYERED`（alpha=255）的子窗口，
///   宿主 = `SHELLDLL_DefView` 的直接父窗口（Progman 或 Progman 直接子 WorkerW），
///   z-order 插入 `SHELLDLL_DefView` 正下方，并在插入后校验/兜底。
/// - **raised 但找不到 `SHELLDLL_DefView`**（如用户关闭"显示桌面图标"）：记录 warn 后
///   降级为旧层级嵌入，壁纸仍挂在 WorkerW 上显示，不整体失败。
/// - **旧层级**：保持原行为——`SetParent(WorkerW)` + `HWND_BOTTOM`，不改子窗口/分层样式。
///
/// 路径选择由纯函数 [`choose_embed_path`] 决定。
///
/// `displays` 参数由调用方传入（D-013：在 `DesktopIntegrator` 层缓存显示器列表
/// 与 5s TTL 失效，避免 PerMonitor 分支每次调用 `enumerate_displays()`）。
/// Span 分支不使用此参数。
pub fn embed_wallpaper(
    wp_hwnd: HWND,
    progman_hwnd: HWND,
    workerw_hwnd: HWND,
    display_id: &str,
    arrangement: Arrangement,
    displays: &[DisplayInfo],
) -> Result<(), MirrorStarError> {
    unsafe {
        // Step 1: Make the wallpaper window borderless
        crate::desktop::window::make_borderless(wp_hwnd)?;
        crate::desktop::window::remove_from_taskbar(wp_hwnd);

        let raised = progman_is_raised_desktop(progman_hwnd);
        // 深度查找覆盖"Progman 直接子"与"Progman → WorkerW → DefView"两种层级；
        // 仅 raised 下需要 DefView 作为 z-order 锚点与宿主探测入口。
        let probed_defview = if raised {
            find_defview_deep(progman_hwnd)
        } else {
            None
        };

        // raised 且 DefView 可用 → layered 锚点插入；raised 但 DefView 缺失（或旧层级）
        // → 落入下方唯一的旧层级实现（降级路径不复制粘贴第二份）。
        if let EmbedPath::RaisedLayered(defview) = choose_embed_path(raised, probed_defview) {
            // 宿主 = DefView 的直接父窗口（Progman 或 Progman 直接子 WorkerW），
            // 要求父链上溯可到 Progman；否则不嵌入，交由既有重试/自愈处理。
            let Some(host) = resolve_wallpaper_host(defview, progman_hwnd) else {
                let host_probe = GetAncestor(defview, GA_PARENT);
                return Err(MirrorStarError::DesktopIntegration(format!(
                    "raised desktop 下壁纸宿主校验失败（progman=0x{:X}, workerw=0x{:X}, defview=0x{:X}, host=0x{:X}）",
                    progman_hwnd.0 as isize,
                    workerw_hwnd.0 as isize,
                    defview.0 as isize,
                    host_probe.0 as isize
                )));
            };

            // 坐标按宿主矩形计算：Progman 覆盖整个虚拟桌面；Progman 子 WorkerW
            // 同样覆盖其桌面区域，两者坐标语义一致。
            let mut host_rect = RECT::default();
            GetWindowRect(host, &mut host_rect).map_err(|e| {
                MirrorStarError::DesktopIntegration(format!("GetWindowRect(宿主) 失败: {}", e))
            })?;
            let (x, y, width, height) =
                calculate_embed_coords(&host_rect, display_id, arrangement, displays);

            // WS_CHILD + WS_EX_LAYERED + alpha=255 必须在 SetParent 之前套用。
            crate::desktop::window::apply_raised_desktop_style(wp_hwnd)?;
            crate::desktop::window::set_parent_verified(wp_hwnd, Some(host))?;

            // 插入到 DefView 正下方：壁纸位于桌面图标层之下。
            SetWindowPos(
                wp_hwnd,
                defview,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(|e| {
                MirrorStarError::DesktopIntegration(format!("SetWindowPos(raised) 失败: {}", e))
            })?;

            // SetParent 会把重定父窗口放到新兄弟 z 序顶部，必须校验并兜底，
            // 否则壁纸停留在 DefView 之上会盖住桌面图标。
            let z_anchor_ok = ensure_below_anchor(wp_hwnd, defview, progman_hwnd);

            // 嵌入时不主动抢占前台（不再为恢复焦点调用 SetForegroundWindow）：每次壁纸
            // 嵌入（轮换/手动切换）都会触发 EVENT_SYSTEM_FOREGROUND，扰乱全屏检测状态机，
            // 并在自动轮换时长按闪烁主控制窗口。需要恢复焦点时由调用方负责。
            // ShowWindow 失败不影响已完成的嵌入流程，下一次刷新会重新显示
            let _ = ShowWindow(wp_hwnd, SW_SHOW);

            log_embed_success(
                wp_hwnd,
                raised,
                progman_hwnd,
                workerw_hwnd,
                Some(defview),
                "defview",
                host,
                z_anchor_ok,
                display_id,
                arrangement,
                x,
                y,
                width,
                height,
            );
            return Ok(());
        }

        // ── 旧层级：行为与历史实现逐行一致（WorkerW 父窗口 + HWND_BOTTOM） ──
        // raised 但无 DefView 时也落到此处（降级）：只有 warn 提示，嵌入流程与旧层级完全相同，
        // 不写 WS_CHILD / WS_EX_LAYERED；日志中 defview=0 + z_order_anchor="bottom" + raised=true
        // 即代表走了降级路径。
        if raised {
            tracing::warn!(
                progman = progman_hwnd.0 as isize,
                workerw = workerw_hwnd.0 as isize,
                "raised desktop 下未找到 SHELLDLL_DefView，已降级为 WorkerW 嵌入；若桌面图标不可见，请确认：桌面右键 → 查看 → 显示桌面图标 已勾选"
            );
        }

        // Step 2: Determine position based on display_id and arrangement
        // D-TD-008: 在 match 前统一获取一次 workerw_rect，供所有分支共用
        //（原 3 处重复 GetWindowRect 调用合并为 1 处）。
        let mut workerw_rect = RECT::default();
        GetWindowRect(workerw_hwnd, &mut workerw_rect).map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("GetWindowRect 失败: {}", e))
        })?;

        let (x, y, width, height) =
            calculate_embed_coords(&workerw_rect, display_id, arrangement, displays);

        // Step 3: SetParent - reparent into WorkerW（带父链校验，兼容 NULL 返回二义性）
        crate::desktop::window::set_parent_verified(wp_hwnd, Some(workerw_hwnd))?;

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

        // Step 5: Show the wallpaper window (created without WS_VISIBLE to avoid flash)
        let _ = ShowWindow(wp_hwnd, SW_SHOW);

        log_embed_success(
            wp_hwnd,
            raised,
            progman_hwnd,
            workerw_hwnd,
            None,
            "bottom",
            workerw_hwnd,
            true,
            display_id,
            arrangement,
            x,
            y,
            width,
            height,
        );
        Ok(())
    }
}

/// 记录嵌入成功后的可诊断信息（raised/父链/宿主/样式/z-order 锚点/图标层归属/几何）。
///
/// 字段用于真机复测直接判定走了哪条路径：`raised`、`progman`、`workerw`、
/// `defview`（raised 且找到 DefView 时为句柄；降级/旧层级时为 0）、`parent_after`、
/// `host`、`host_class`、`style_has_child`、`exstyle_has_layered`、`z_order_anchor`、
/// `z_anchor_ok`、`defview_has_icon_list`、`workerw_has_defview`、
/// `workerw_has_icon_list`、`sibling_chain`、`display_id`、坐标尺寸。
///
/// 降级路径的判据：`raised=true` + `defview=0` + `z_order_anchor="bottom"` +
/// `style_has_child=false` + `exstyle_has_layered=false`。
#[allow(clippy::too_many_arguments)] // 诊断字段天然较多，拆结构体得不偿失。
fn log_embed_success(
    hwnd: HWND,
    raised: bool,
    progman: HWND,
    workerw: HWND,
    defview: Option<HWND>,
    z_order_anchor: &str,
    host: HWND,
    z_anchor_ok: bool,
    display_id: &str,
    arrangement: Arrangement,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    // SAFETY: 仅查询窗口样式与父链，不修改窗口状态。
    let (style_has_child, exstyle_has_layered, parent_after) = unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        (
            style & WS_CHILD.0 != 0,
            ex_style & WS_EX_LAYERED.0 != 0,
            GetAncestor(hwnd, GA_PARENT),
        )
    };
    let host_class = class_name_of(host);

    // 图标层归属探测与 Progman 子窗口链仅用于 raised 路径诊断（H1/H2 判定）；
    // 旧层级没有 layered 子窗口模型，跳过这些额外 Win32 探测。
    let (defview_has_icon_list, workerw_has_defview, workerw_has_icon_list, sibling_chain) =
        if raised {
            (
                defview.is_some_and(|d| subtree_has_class(d, windows::core::w!("SysListView32"))),
                subtree_has_class(workerw, windows::core::w!("SHELLDLL_DefView")),
                subtree_has_class(workerw, windows::core::w!("SysListView32")),
                progman_child_chain_summary(progman),
            )
        } else {
            (false, false, false, Vec::new())
        };

    tracing::info!(
        display_id,
        ?arrangement,
        raised,
        progman = progman.0 as isize,
        workerw = workerw.0 as isize,
        defview = defview.map_or(0, |h| h.0 as isize),
        parent_after = parent_after.0 as isize,
        host = host.0 as isize,
        host_class = %host_class,
        style_has_child,
        exstyle_has_layered,
        z_order_anchor,
        z_anchor_ok,
        defview_has_icon_list,
        workerw_has_defview,
        workerw_has_icon_list,
        ?sibling_chain,
        x,
        y,
        width,
        height,
        "壁纸窗口嵌入完成"
    );
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
///
/// raised desktop 下 `SPI_SETDESKWALLPAPER` 会销毁当前承载动态壁纸的 WorkerW
/// （参考 Lively Wallpaper 的 RefreshDesktop 早退实现），因此直接返回。
pub fn refresh_desktop() {
    if is_raised_desktop() {
        tracing::debug!(
            "raised desktop 下跳过 refresh_desktop：SPI_SETDESKWALLPAPER 会销毁当前 WorkerW"
        );
        return;
    }
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

    // ── has_noredirection_bitmap 纯函数测试 ─────────────────────────────
    //
    // raised desktop 判定依赖 WS_EX_NOREDIRECTIONBITMAP 扩展样式位，
    // 此处仅覆盖纯位运算逻辑，不依赖交互式桌面环境。

    #[test]
    fn has_noredirection_bitmap_zero_style_returns_false() {
        assert!(!has_noredirection_bitmap(0));
    }

    #[test]
    fn has_noredirection_bitmap_flag_returns_true() {
        assert!(has_noredirection_bitmap(WS_EX_NOREDIRECTIONBITMAP.0));
    }

    #[test]
    fn has_noredirection_bitmap_with_other_flags_returns_true() {
        let ex_style = WS_EX_NOREDIRECTIONBITMAP.0 | WS_EX_TRANSPARENT.0;
        assert!(has_noredirection_bitmap(ex_style));
    }

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

    // ── raised desktop：以 Progman 矩形为宿主的坐标测试 ───────────────────
    //
    // raised desktop 下壁纸窗口的父窗口是 Progman，`SetWindowPos` 坐标相对
    // Progman 客户区。Progman 通常覆盖整个虚拟桌面，故原点可能为 (0,0)，
    // 也可能为负值（副显示器位于主显示器左侧/上方）。

    #[test]
    fn raised_permonitor_progman_at_origin_unchanged() {
        // Progman 原点 (0,0)：子窗口坐标应等于 display 虚拟屏幕坐标。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY1".to_string(),
            name: "显示器 1".to_string(),
            width: 2560,
            height: 1440,
            x: 0,
            y: 0,
            is_primary: true,
            dpi: 96,
        };
        let progman_rect = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let (x, y, width, height) = calculate_per_monitor_child_coords(&display, &progman_rect);
        assert_eq!((x, y, width, height), (0, 0, 2560, 1440));
    }

    #[test]
    fn raised_permonitor_progman_negative_origin_offset() {
        // Progman 原点 (-1920,0)：副显示器虚拟屏幕坐标减去 Progman 原点应为 (0,0)。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY2".to_string(),
            name: "显示器 2".to_string(),
            width: 1920,
            height: 1080,
            x: -1920,
            y: 0,
            is_primary: false,
            dpi: 96,
        };
        let progman_rect = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let (x, y, width, height) = calculate_per_monitor_child_coords(&display, &progman_rect);
        assert_eq!(
            (x, y, width, height),
            (0, 0, 1920, 1080),
            "Progman 负原点时子窗口坐标应正确归零"
        );
    }

    #[test]
    fn raised_span_uses_progman_full_size() {
        // Span 以宿主矩形全尺寸铺满：Progman 原点为负也不影响 Span 的 (0,0) 原点。
        let progman_rect = RECT {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let (x, y, width, height) =
            calculate_embed_coords(&progman_rect, "\\\\.\\DISPLAY1", Arrangement::Span, &[]);
        assert_eq!((x, y, width, height), (0, 0, 3840, 1080));
    }

    // ── raised desktop 宿主选择 / z 序纯函数测试 ─────────────────────────
    //
    // choose_wallpaper_host / host_is_under_progman / is_below_anchor 均为纯逻辑，
    // 用伪造 HWND 值覆盖判定矩阵即可，不依赖真实桌面环境。

    /// 构造伪造 HWND：仅用于纯函数判定，不解引用、不传给 Win32 API。
    fn fake_hwnd(value: isize) -> HWND {
        HWND(value as *mut std::ffi::c_void)
    }

    #[test]
    fn host_is_under_progman_matrix() {
        let progman = fake_hwnd(0x100);
        let workerw = fake_hwnd(0x200);
        let other = fake_hwnd(0x300);

        // 宿主 == Progman：成立
        assert!(host_is_under_progman(progman, progman, other));
        // 宿主的父窗口 == Progman：成立
        assert!(host_is_under_progman(workerw, progman, progman));
        // 宿主的父窗口 != Progman：不成立
        assert!(!host_is_under_progman(workerw, progman, other));
        // 宿主无效：不成立
        assert!(!host_is_under_progman(HWND::default(), progman, progman));
        // Progman 无效（桌面重建/查找失败）：不成立
        assert!(!host_is_under_progman(
            workerw,
            HWND::default(),
            HWND::default()
        ));
    }

    #[test]
    fn choose_wallpaper_host_matrix() {
        let progman = fake_hwnd(0x100);
        let workerw = fake_hwnd(0x200);
        let other = fake_hwnd(0x300);

        // DefView 直接挂在 Progman 下：宿主 = Progman（宿主自身的父窗口不是 Progman）
        assert_eq!(
            choose_wallpaper_host(progman, other, progman),
            Some(progman)
        );
        // DefView 挂在 Progman 直接子 WorkerW 下：宿主 = 该 WorkerW
        assert_eq!(
            choose_wallpaper_host(workerw, progman, progman),
            Some(workerw)
        );
        // 宿主父窗口既不是 Progman、宿主也不是 Progman：拒绝
        assert_eq!(choose_wallpaper_host(workerw, other, progman), None);
        // DefView 父窗口无效：拒绝
        assert_eq!(
            choose_wallpaper_host(HWND::default(), progman, progman),
            None
        );
    }

    #[test]
    fn is_below_anchor_matrix() {
        let anchor = fake_hwnd(0x100);

        // 上一个兄弟正是锚点：位于锚点之下
        assert!(is_below_anchor(anchor, anchor));
        // 上一个兄弟是别的窗口：不在锚点之下
        assert!(!is_below_anchor(fake_hwnd(0x200), anchor));
        // 没有上一个兄弟（z 序顶部）：不在锚点之下
        assert!(!is_below_anchor(HWND::default(), anchor));
        // 锚点无效：不成立
        assert!(!is_below_anchor(anchor, HWND::default()));
    }

    #[test]
    fn choose_embed_path_four_quadrants() {
        let defview = fake_hwnd(0x400);

        // raised + DefView 可用：layered 锚点插入
        assert_eq!(
            choose_embed_path(true, Some(defview)),
            EmbedPath::RaisedLayered(defview)
        );
        // raised 但 DefView 缺失（用户关闭"显示桌面图标"）：降级到旧层级嵌入
        assert_eq!(choose_embed_path(true, None), EmbedPath::LegacyWorkerW);
        // 旧层级即使探测到 DefView 也不走 layered 模型
        assert_eq!(
            choose_embed_path(false, Some(defview)),
            EmbedPath::LegacyWorkerW
        );
        // 旧层级 + 无 DefView：旧层级嵌入
        assert_eq!(choose_embed_path(false, None), EmbedPath::LegacyWorkerW);
        // 无效 HWND 归一化为缺失，不得误判为 raised layered
        assert_eq!(
            choose_embed_path(true, Some(HWND::default())),
            EmbedPath::LegacyWorkerW
        );
    }

    #[test]
    fn raised_workerw_host_permonitor_negative_origin_offset() {
        // 宿主为 Progman 直接子 WorkerW，原点位于负坐标 (-1920,0)：
        // 子窗口坐标应为 display 虚拟屏幕坐标减去宿主原点。
        let display = DisplayInfo {
            id: "\\\\.\\DISPLAY2".to_string(),
            name: "显示器 2".to_string(),
            width: 1920,
            height: 1080,
            x: -1920,
            y: 0,
            is_primary: false,
            dpi: 96,
        };
        let workerw_rect = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let (x, y, width, height) = calculate_embed_coords(
            &workerw_rect,
            &display.id,
            Arrangement::PerMonitor,
            std::slice::from_ref(&display),
        );
        assert_eq!((x, y, width, height), (0, 0, 1920, 1080));
    }

    #[test]
    fn raised_workerw_host_span_uses_host_full_size() {
        // 宿主为 Progman 子 WorkerW 时，Span 仍以宿主全尺寸铺满（与 Progman 一致）。
        let workerw_rect = RECT {
            left: -1920,
            top: -200,
            right: 1920,
            bottom: 1080,
        };
        let (x, y, width, height) =
            calculate_embed_coords(&workerw_rect, "\\\\.\\DISPLAY1", Arrangement::Span, &[]);
        assert_eq!((x, y, width, height), (0, 0, 3840, 1280));
    }

    #[test]
    fn progman_child_chain_summary_no_panic() {
        // 真实桌面上遍历 Progman 直接子窗口链：不 panic，且长度受 64 上限约束。
        let progman = unsafe { FindWindowW(windows::core::w!("Progman"), None) };
        if let Ok(hwnd) = progman {
            let chain = progman_child_chain_summary(hwnd);
            assert!(chain.len() < 64, "子窗口链长度应受上限约束");
            for item in &chain {
                assert!(item.contains("#vis="), "每项应含可见性字段: {item}");
            }
        }
        // Progman 找不到时（极端情况）跳过，不 panic 即通过
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
    /// 环境下几乎不会失败（需损坏的 GDI 子系统），无法在单元测试中可靠触发失败分支，
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
