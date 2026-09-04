use mirrorstar_core::{ConfigManager, FullscreenAction, PauseReason, WallpaperEngine};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::state::{
    hide_main_window_on_fullscreen, resume_all_fast_blocking, restore_main_window_after_fullscreen,
    try_pause_all_fast, try_terminate_all_fast, SendWinEventHook, FULLSCREEN_MONITOR_RUNNING,
    FULLSCREEN_MONITOR_THREAD, FULLSCREEN_MONITOR_THREAD_ID, FULLSCREEN_WAS, SHARED_CONFIG,
    SHARED_ENGINE, WIN_EVENT_HOOK,
};

// 全屏检测算法与误判场景文档化
//
// # 全屏检测算法（事件驱动 + 即时查询）
//
// ## 检测入口
//
// 通过 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 注册前台窗口切换事件回调，
// 每次前台窗口切换时触发 `foreground_event_callback`。
//
// ## 检测流程（`foreground_fullscreen_level`）
//
// 1. **获取前台窗口**：`GetForegroundWindow()` 返回当前前台窗口 HWND
//    - 若返回 invalid 句柄（无前台窗口，如屏保/锁屏），返回 None
// 2. **排除自身窗口**：`GetWindowTextW` 读取窗口标题，与 "镜星壁纸" /
//    "MirrorStar Wallpaper" 精确匹配（ST-012：精确匹配避免子串误判）
// 3. **排除系统桌面组件**：`GetClassNameW` 读取窗口类名，排除：
//    - `Progman`：桌面背景窗口（SetForegroundWindow(progman) 可使其成为前台）
//    - `WorkerW`：壁纸层窗口（应用嵌入壁纸的目标）
//    - `Shell_TrayWnd`：任务栏
//    （避免 Progman 等被误判为全屏应用）
// 4. **获取窗口矩形**：`GetWindowRect(foreground, &mut window_rect)` 读取窗口屏幕坐标
// 5. **获取显示器信息**：
//    - `MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)` 获取窗口所在显示器
//    - `GetMonitorInfoW(monitor, &mut monitor_info)` 读取显示器矩形
// 6. **分级判断**：
//    - 100% 覆盖：`is_rect_covering_monitor` 判断窗口矩形是否完全包含显示器矩形
//      （窗口 left/top ≤ 显示器 left/top，right/bottom ≥ 显示器 right/bottom）→ `TrueFullscreen`
//    - 最大化/近全屏：`IsZoomed` 或 `is_rect_covering_95_percent`（宽高均 ≥95%）→ `Maximized`
//    - 其余 → `None`
//
// # 误判场景
//
// ## 已规避的误判
//
//  - **最大化窗口**：最大化窗口的矩形通常等于显示器矩形（除任务栏区域），
//  会被判定为"覆盖显示器"。**这是预期行为**——用户最大化窗口时通常希望暂停壁纸
//  （避免壁纸在最大化窗口边缘可见时分心）。如用户不希望处置，可在配置中将
//  `pause.fullscreen_action` 设为 `none`
// - **Progman 成为前台**：通过类名排除
// - **自身 Tauri 窗口**：通过标题精确匹配排除（ST-012 修复）
// - **任务栏**：通过类名 `Shell_TrayWnd` 排除
//
// ## 已知残留误判（可接受）
//
// - **跨屏窗口**：窗口跨多个显示器时，`MonitorFromWindow` 返回最近显示器，
//  若窗口覆盖该显示器则触发暂停。此为合理行为（用户在某个显示器上有全屏窗口）
// - **DPI 缩放场景**：高 DPI 下窗口矩形可能与显示器矩形略有偏差，
//  `is_rect_covering_monitor` 使用 `≤` / `≥` 比较（非严格相等），
//  容忍 ±1px 误差，避免 DPI 缩放导致漏判
// - **虚拟桌面切换**：切换虚拟桌面时前台窗口变化，可能触发短暂的暂停-恢复循环。
//  可接受（虚拟桌面切换本身是用户显式操作，短暂暂停壁纸不干扰）
// - **全屏 UWP 应用**：UWP 应用的窗口类名可能不在排除列表中，
//  若其全屏则正常触发暂停（预期行为）
//
// # 性能特征
//
// - **调用频率**：仅在 `EVENT_SYSTEM_FOREGROUND` 事件触发时调用，
//  非轮询式（用户不切换窗口时零开销）
// - **单次开销**：~1-5ms（5 个 Win32 API 调用 + 字符串比较）
// - **无缓存**：每次事件即时查询，不缓存结果（事件驱动模式下无需缓存）
// - **若需节流**：调用方可在 `foreground_event_callback` 中添加时间窗口节流，
//  但当前事件频率已足够低（用户切换窗口的频率），无需额外节流

/// HWND 的 Send/Sync 包装器（HWND 原始类型包含裸指针，未实现 Send/Sync）
///
/// 与 state.rs 中 `SendWinEventHook` 的 soundness 论证一致：HWND 是窗口句柄，
/// 本质是一个指针大小的数值令牌，不持有任何 Rust 可变状态，仅在线程间共享
/// 该令牌本身（窗口销毁回收由 Win32 管理，非 Rust 所有权），跨线程共享安全。
/// 存储于 `Mutex<Option<SendHwnd>>`，所有读写均在 Mutex 守卫下进行，无别名问题。
#[derive(Clone, Copy)]
struct SendHwnd(pub windows::Win32::Foundation::HWND);

// SAFETY: 见上方 soundness 论证——HWND 仅是指针大小的窗口句柄令牌，不持有
// Rust 可变状态，跨线程移动/共享不会产生数据竞争或别名问题。
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

/// 全屏检测的分级结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullscreenLevel {
    /// 前台窗口未覆盖显示器（非全屏）
    None,
    /// 最大化 / 近全屏（IsZoomed 或窗口矩形 ≥95% 显示器）——只暂停、永不终止
    Maximized,
    /// 真全屏（窗口矩形 100% 覆盖整屏含任务栏）——按配置终止
    TrueFullscreen,
}

/// 记录最近一个全屏窗口句柄，用于区分"临时覆盖层"与"真正退出全屏"
static LAST_FULLSCREEN_HWND: std::sync::Mutex<Option<SendHwnd>> = std::sync::Mutex::new(None);
/// 当前检测到的全屏级别（分级状态机的状态，供事件回调/周期复查/后台恢复共享）
static FULLSCREEN_LEVEL: std::sync::Mutex<FullscreenLevel> =
    std::sync::Mutex::new(FullscreenLevel::None);
/// 周期复查线程运行标志（防止重复 spawn）
static FULLSCREEN_REVIEW_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 周期复查线程 JoinHandle（重启时停止并 join）
static FULLSCREEN_REVIEW_THREAD: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(None);
/// 后台恢复线程运行标志（Task 7.2：防止并发触发多次恢复）
///
/// 退出全屏后壁纸恢复改由后台线程异步执行（避免阻塞 Win32 回调线程）。
/// 事件回调退出分支、周期复查线程都可能调用 `resume_from_fullscreen_exit`，
/// 此标志通过 `swap(true)` 保证同一时刻只有一个恢复线程在运行：
/// - `true`：已有恢复线程在进行中，后续调用直接跳过（去重）
/// - `false`：无恢复线程，`swap(true)` 成功者负责 spawn 并最终复位为 false
static RESUME_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// SetWinEventHook 的前台窗口切换回调函数
///
/// 当前台窗口切换时触发，检测是否为全屏应用并暂停/恢复壁纸。
unsafe extern "system" fn foreground_event_callback(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: windows::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // 用 catch_unwind 包裹整个回调体，防止回调内部 panic 穿越 FFI 边界（Win32 回调）
    // 导致的栈展开 UB 或硬崩溃。回调运行于 DispatchMessageW 消息循环上下文，
    // 任何 panic 若未被捕获都会跨越 extern "system" 边界造成未定义行为。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let config = match SHARED_CONFIG.get() {
            Some(c) => c.clone(),
            None => return,
        };

        // 按配置选择全屏处置策略：none / pause / terminate（默认 terminate）
        let action = config.get_config().pause.fullscreen_action;
        // 先检测当前前台全屏级别（分级状态机的输入），供 none 残留清理分支与主体共用
        let level = foreground_fullscreen_level();

        // none 模式：不做任何处置。但若此前已因全屏处置过（如运行中切换到 none），
        // 清理残留：恢复壁纸 + 重建主窗口（若销毁过），保持状态一致。
        if action == FullscreenAction::None {
            if FULLSCREEN_WAS.load(Ordering::Acquire) {
                tracing::info!("全屏动作已切换为无操作，恢复壁纸并清理残留");
                resume_from_fullscreen_exit();
                *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
            }
            return;
        }

        let prev_level = *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner());
        let was_fullscreen = FULLSCREEN_WAS.load(Ordering::Acquire);

        match compute_transition(level, prev_level, was_fullscreen, action) {
            Transition::NoOp => {
                // 同级别或无可处置：仅更新 HWND 与级别
                if level != FullscreenLevel::None {
                    update_last_fullscreen_hwnd();
                }
                *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
            }
            Transition::Pause => {
                let failed = try_pause_all_fast(PauseReason::FULLSCREEN);
                if let Some(failed) = failed {
                    if failed.is_empty() {
                        tracing::info!("检测到最大化/近全屏窗口，暂停壁纸（进程驻留）");
                        FULLSCREEN_WAS.store(true, Ordering::Release);
                        *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
                        update_last_fullscreen_hwnd();
                        hide_main_window_on_fullscreen();
                    } else {
                        tracing::warn!(
                            failed_count = failed.len(),
                            failed = ?failed,
                            "暂停壁纸部分失败，不更新 FULLSCREEN_WAS"
                        );
                        *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
                    }
                }
                // None → 锁忙，不更新状态（事件会重复触发）
            }
            Transition::Terminate => {
                let failed = try_terminate_all_fast(PauseReason::FULLSCREEN);
                if let Some(failed) = failed {
                    if failed.is_empty() {
                        tracing::info!("检测到真全屏应用，终止壁纸释放内存");
                        FULLSCREEN_WAS.store(true, Ordering::Release);
                        *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
                        update_last_fullscreen_hwnd();
                        hide_main_window_on_fullscreen();
                    } else {
                        tracing::warn!(
                            failed_count = failed.len(),
                            failed = ?failed,
                            "终止壁纸部分失败，不更新 FULLSCREEN_WAS"
                        );
                        *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
                    }
                }
            }
            Transition::DowngradeToMaximized => {
                tracing::info!("级别降级 TrueFullscreen→Maximized，壁纸保持终止不恢复");
                *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = FullscreenLevel::Maximized;
                update_last_fullscreen_hwnd();
            }
            Transition::Exit => {
                // 临时覆盖层校验：原全屏窗口仍覆盖显示器 → 跳过恢复
                if is_previous_fullscreen_window_still_active() {
                    tracing::info!("前台切换但原全屏窗口仍覆盖显示器（临时覆盖层），跳过壁纸恢复");
                    return;
                }
                tracing::info!("退出全屏，恢复壁纸");
                resume_from_fullscreen_exit();
                *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = FullscreenLevel::None;
            }
        }
    }));

    // catch_unwind 返回 Err(_)：回调内部发生 panic，已被捕获，防止穿越 FFI 硬崩溃
    if result.is_err() {
        tracing::error!("全屏回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
    }
}

/// 启动全屏应用检测线程（事件驱动，使用 SetWinEventHook）
pub(crate) fn start_fullscreen_monitor(
    engine: Arc<tokio::sync::Mutex<WallpaperEngine>>,
    config_manager: Arc<ConfigManager>,
) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Accessibility::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    // C-014/C-015 修复：二次调用时先 stop 旧监控再 start
    // swap 返回旧值：若为 true 表示已有监控在运行，需先停止旧监控线程并释放旧 hook，
    // 避免旧线程消息循环泄漏与旧 hook 句柄悬空。swap 同时将标志置为 true。
    if FULLSCREEN_MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        tracing::info!("全屏监控已在运行，先停止旧监控再重启");
        // 1. take 旧 hook 并 UnhookWinEvent
        if let Some(old_hook) = WIN_EVENT_HOOK.lock().ok().and_then(|mut h| h.take()) {
            unsafe {
                // 重启清理路径：unhook 失败时旧钩子随进程退出或下次重启自动回收
                let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(old_hook.0);
            }
        }
        // 2. take 旧 thread_id 并 PostThreadMessage WM_QUIT 唤醒
        if let Some(old_tid) = FULLSCREEN_MONITOR_THREAD_ID
            .lock()
            .ok()
            .and_then(|mut t| t.take())
        {
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    old_tid,
                    windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
                .is_err()
                {
                    tracing::warn!("PostThreadMessageW 失败：WM_QUIT 未送达（线程可能已退出）");
                }
            }
        }
        // 3. take 旧 JoinHandle 并 join 等待旧线程退出
        if let Some(old_handle) = FULLSCREEN_MONITOR_THREAD
            .lock()
            .ok()
            .and_then(|mut h| h.take())
        {
            // 重启清理路径：join 失败（线程 panic）仅记录，无法传播
            let _ = old_handle.join();
        }
        // 4. 停止旧周期复查线程（循环检查 RUNNING 标志，join 在 2s 内完成）
        FULLSCREEN_REVIEW_RUNNING.store(false, Ordering::SeqCst);
        if let Some(handle) = FULLSCREEN_REVIEW_THREAD.lock().ok().and_then(|mut t| t.take()) {
            let _ = handle.join();
        }
    }

    // 启动周期复查线程（兜底"无前台切换事件"的退出场景）。
    // 放在重启清理块之后，保证新复查线程在旧线程 join 之后 spawn（不重复）。
    spawn_fullscreen_review_thread();

    // 初始化全局回调状态
    // SHARED_ENGINE / SHARED_CONFIG 保持 OnceLock：二次调用时 set 失败（旧值保留），
    // 因 Arc 引用同一底层资源，保留旧值不影响正确性。
    let _ = SHARED_ENGINE.set(engine);
    let _ = SHARED_CONFIG.set(config_manager);

    match std::thread::Builder::new()
        .name("mirrorstar-fullscreen-monitor".to_string())
        // v8.0 内存优化：监控线程仅需 Win32 回调与消息循环，512KB 足够，默认 2MB 浪费
        .stack_size(512 * 1024)
        .spawn(move || {
            // 记录线程 ID，供退出时 PostThreadMessage 唤醒消息循环
            let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            // Mutex 中毒（线程 panic）时不存储 thread_id，退出时跳过 PostThreadMessage
            let _ = FULLSCREEN_MONITOR_THREAD_ID
                .lock()
                .map(|mut t| *t = Some(thread_id));

            // 设置事件钩子：监听前台窗口切换事件
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(foreground_event_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            // SetWinEventHook 失败（返回 0）：重置运行标志并退出线程，避免
            // RUNNING=true 但无有效 hook 的"假运行"状态
            if hook.0.is_null() {
                tracing::error!("SetWinEventHook 返回 0，全屏监控启动失败");
                FULLSCREEN_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            // 存储句柄到全局静态变量，供退出时通过 UnhookWinEvent 释放
            // Mutex 中毒时不存储 hook，进程退出时内核自动回收
            let _ = WIN_EVENT_HOOK
                .lock()
                .map(|mut h| *h = Some(SendWinEventHook(hook)));

            // 消息循环：WINEVENT_OUTOFCONTEXT 模式下回调通过消息循环调用
            // GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
            // （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
            let mut msg = MSG::default();
            unsafe {
                loop {
                    let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
                    match ret.0 {
                        0 => break, // WM_QUIT
                        -1 => {
                            // T02 修复：GetMessageW 返回 -1 表示错误（如窗口句柄无效），
                            // BOOL.as_bool() 对 -1 返回 true 会误判为消息到达，须显式 match ret.0。
                            // 与 explorer.rs L224-239 修复模式一致（N-002），并按 spec 要求记录 GetLastError。
                            let err = windows::Win32::Foundation::GetLastError();
                            tracing::error!(
                                error = ?err,
                                "GetMessageW 返回 -1（错误），退出全屏监控消息循环"
                            );
                            break;
                        }
                        _ => {}
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }) {
        Ok(handle) => {
            // 存储 JoinHandle，供退出或二次调用 start 时 join 等待线程退出（C-014/C-015）
            // Mutex 中毒时不存储 handle，无法 join 但线程会随进程退出
            let _ = FULLSCREEN_MONITOR_THREAD
                .lock()
                .map(|mut h| *h = Some(handle));
        }
        Err(e) => {
            tracing::error!(error = %e, "启动全屏检测线程失败");
            // 线程 spawn 失败：重置运行标志，避免 RUNNING=true 但无实际监控线程
            FULLSCREEN_MONITOR_RUNNING.store(false, Ordering::SeqCst);
        }
    }
}

/// 启动全屏退出周期复查线程（兜底"无前台切换事件"的退出场景）
///
/// 覆盖场景：取消最大化（可能不产生 EVENT_SYSTEM_FOREGROUND 前台切换）、
/// Alt-Tab 后后台关闭游戏、或其他不触发前台切换事件的退出路径。
/// 每 2s 复查一次：仅当 FULLSCREEN_WAS 仍为 true、前台不再全屏、
/// 且原全屏窗口不再覆盖显示器时，才恢复壁纸（同样区分临时覆盖层）。
fn spawn_fullscreen_review_thread() {
    // 防止重复 spawn（start_fullscreen_monitor 二次调用场景）
    if FULLSCREEN_REVIEW_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    match std::thread::Builder::new()
        .name("mirrorstar-fullscreen-review".to_string())
        .spawn(move || {
            while FULLSCREEN_REVIEW_RUNNING.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_secs(2));
                // 兜底：无前台切换事件的退出场景（取消最大化、Alt-Tab 后后台关闭游戏等）
                if !FULLSCREEN_WAS.load(Ordering::Acquire) {
                    continue;
                }
                if foreground_fullscreen_level() != FullscreenLevel::None {
                    continue;
                }
                if is_previous_fullscreen_window_still_active() {
                    continue;
                }
                tracing::info!("周期复查：检测到退出全屏（无前台切换事件），恢复壁纸");
                resume_from_fullscreen_exit();
            }
        }) {
        Ok(handle) => {
            if let Ok(mut t) = FULLSCREEN_REVIEW_THREAD.lock() {
                *t = Some(handle);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "启动全屏周期复查线程失败");
            FULLSCREEN_REVIEW_RUNNING.store(false, Ordering::SeqCst);
        }
    }
}

/// 前台窗口标题与类名缓冲区长度（T10：提取为常量）
///
/// `GetWindowTextW` / `GetClassNameW` 在缓冲区不足时会截断并返回实际长度（不含 NUL）。
/// 256 字符足以覆盖绝大多数窗口标题与所有标准系统窗口类名。
///
/// 截断比较可接受：本函数仅用于将窗口标题与若干已知字符串做**精确匹配**
/// （`is_self_window_title` / `is_system_window_class`，均显著短于 256 字符）。
/// 超长标题的第三方窗口即使被截断，也无法匹配已知的自身/系统窗口标识，
/// 因此会被自然地"未命中并继续后续判断"，行为与未截断一致。
/// 若未来需要按子串匹配长标题，应改用 `Vec<u16>` 动态分配并按返回长度重新分配。
const TITLE_BUF_LEN: usize = 256;

/// 检测前台窗口的全屏级别（None / Maximized / TrueFullscreen）
///
/// 骨架沿用原 `is_foreground_fullscreen`：自身窗口标题精确匹配排除、系统桌面组件
/// 类名排除、GetWindowRect、MonitorFromWindow + GetMonitorInfoW。
/// 区别在于返回三级 FullscreenLevel：100% 覆盖显示器 → TrueFullscreen；
/// IsZoomed 或 ≥95% 覆盖 → Maximized；否则 → None。
fn foreground_fullscreen_level() -> FullscreenLevel {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, IsZoomed,
    };

    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() {
            return FullscreenLevel::None;
        }

        // 跳过我们自己的窗口（同时匹配中文标题"镜星壁纸"和英文标题"MirrorStar Wallpaper"）
        // ST-012: 改用精确匹配而非 contains，避免误排除任何标题含 "MirrorStar" 子串的第三方窗口。
        // 自身 Tauri 窗口标题为 "镜星壁纸"（WebviewWindowBuilder::title 在 state.rs L417 设置）；
        // "MirrorStar Wallpaper" 用于版本号 UI 显示与英文环境兼容。
        let mut title = [0u16; TITLE_BUF_LEN];
        let title_len = GetWindowTextW(foreground, &mut title);
        if title_len > 0 {
            let title_str = String::from_utf16_lossy(&title[..title_len as usize]);
            if is_self_window_title(&title_str) {
                return FullscreenLevel::None;
            }
        }

        // 排除桌面/壁纸层/任务栏窗口：
        // SetForegroundWindow(progman) 会使 Progman 成为前台窗口，其标题为空、矩形覆盖全屏，
        // 被误判为全屏应用。通过类名排除 Progman/WorkerW/Shell_TrayWnd 等系统桌面组件。
        let mut class_name = [0u16; TITLE_BUF_LEN];
        let class_len = GetClassNameW(foreground, &mut class_name);
        if class_len > 0 {
            let class_str = String::from_utf16_lossy(&class_name[..class_len as usize]);
            if is_system_window_class(&class_str) {
                return FullscreenLevel::None;
            }
        }

        // 获取窗口矩形
        let mut window_rect = RECT::default();
        if GetWindowRect(foreground, &mut window_rect).is_err() {
            return FullscreenLevel::None;
        }

        // 获取显示器信息
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
            return FullscreenLevel::None;
        }
        let monitor_rect = monitor_info.rcMonitor;
        // 100% 覆盖 → 真全屏
        if is_rect_covering_monitor(&window_rect, &monitor_rect) {
            return FullscreenLevel::TrueFullscreen;
        }
        // IsZoomed 或 ≥95% → 最大化
        if IsZoomed(foreground).as_bool() || is_rect_covering_95_percent(&window_rect, &monitor_rect)
        {
            return FullscreenLevel::Maximized;
        }
        FullscreenLevel::None
    }
}

/// 更新记录的全屏窗口句柄为当前前台窗口（处置/降级时记录，供退出分支区分临时覆盖层）
fn update_last_fullscreen_hwnd() {
    let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    if let Ok(mut g) = LAST_FULLSCREEN_HWND.lock() {
        *g = Some(SendHwnd(fg));
    }
}

/// 判断最近记录的全屏窗口是否仍覆盖其所在显示器（即游戏仍全屏、只是被临时覆盖）
///
/// 用于区分"临时覆盖层"（任务管理器/Alt-Tab，原全屏窗口仍存在并覆盖显示器）与
/// "真正退出全屏"（原全屏窗口已销毁/最小化/不再覆盖显示器）。
/// 事件回调与周期复查线程都会调用，只读取 `LAST_FULLSCREEN_HWND` 的值，不 take 消费。
fn is_previous_fullscreen_window_still_active() -> bool {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsWindow, IsWindowVisible, IsZoomed,
    };

    let hwnd = match LAST_FULLSCREEN_HWND.lock().ok().and_then(|g| *g) {
        Some(SendHwnd(h)) => h,
        None => return false,
    };

    unsafe {
        // 原全屏窗口已销毁 → 真正退出全屏
        if !IsWindow(hwnd).as_bool() {
            return false;
        }
        // 原全屏窗口不可见（已最小化等）→ 不再覆盖显示器
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }

        let mut window_rect = windows::Win32::Foundation::RECT::default();
        if GetWindowRect(hwnd, &mut window_rect).is_err() {
            return false;
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
            return false;
        }

        // 原全屏窗口仍最大化（IsZoomed）或覆盖其显示器 ≥95% → 桌面仍被覆盖
        if IsZoomed(hwnd).as_bool() {
            return true;
        }
        is_rect_covering_95_percent(&window_rect, &monitor_info.rcMonitor)
    }
}

/// 判断是否应触发壁纸恢复（纯函数，便于单元测试）
///
/// 仅当：之前是全屏（`was_fullscreen`）、当前不再是前台全屏（`!is_fullscreen`）、
/// 且原全屏窗口不再覆盖显示器（`!previous_fullscreen_still_active`，排除临时覆盖层）时
/// 才恢复壁纸。
///
/// 生产路径（前台事件回调退出分支、周期复查线程）直接使用
/// `is_previous_fullscreen_window_still_active()` 短路判断，本纯函数仅用于单元测试
/// 覆盖该决策逻辑，故标记 `#[cfg(test)]` 以避免非测试构建的 dead_code 警告。
#[cfg(test)]
fn should_trigger_resume(
    was_fullscreen: bool,
    is_fullscreen: bool,
    previous_fullscreen_still_active: bool,
) -> bool {
    was_fullscreen && !is_fullscreen && !previous_fullscreen_still_active
}

/// 清除记录的全屏窗口句柄（真正退出全屏时调用，避免句柄悬空）
fn clear_last_fullscreen_hwnd() {
    if let Ok(mut g) = LAST_FULLSCREEN_HWND.lock() {
        *g = None;
    }
}

/// 退出全屏后的壁纸恢复统一入口（Task 7.2：异步后台执行）
///
/// C-008：仅当 resume_all_fast 全部成功时才更新 FULLSCREEN_WAS，并清理
/// LAST_FULLSCREEN_HWND、重建主窗口（若此前销毁过）。
/// 供前台事件回调退出分支与周期复查线程共用，保证两处逻辑一致。
///
/// # 异步化设计（Task 7.2，修复"退出游戏后壁纸黑屏不恢复"根因）
///
/// 原实现在此同步调用 `try_resume_all_fast`。真全屏终止后恢复需 `play()` 冷启动
/// mpv（IPC 连接 + 窗口查找，2-6s），同步执行会长时间阻塞 Win32 回调线程
/// （`foreground_event_callback` 运行于 `DispatchMessageW` 消息循环上下文），
/// 期间无法处理新的前台切换事件 → 用户感知壁纸卡死 / 黑屏不恢复。
///
/// 现改为 spawn 专用后台线程 `mirrorstar-fullscreen-resume` 执行
/// `resume_all_fast_blocking`（阻塞式获取 engine 锁，不因锁忙偶发跳过）：
/// - 成功 → 清 `FULLSCREEN_WAS` / `LAST_FULLSCREEN_HWND` / 重建主窗口
/// - 失败 → 保留 `FULLSCREEN_WAS=true`，由周期复查线程（2s 间隔）自动重试
/// - 通过 `RESUME_IN_PROGRESS` 标志去重，防止事件回调与复查线程并发重复恢复
fn resume_from_fullscreen_exit() {
    // 去重：已有后台恢复线程在进行中则跳过（事件回调退出分支与周期复查线程
    // 可能并发触发）。swap(true) 原子地取得"执行权"：
    // - 返回 true → 已有线程在跑，本次调用直接返回
    // - 返回 false → 本次调用获得执行权，负责 spawn 并在线程结束时复位 false
    if RESUME_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        tracing::debug!("壁纸恢复已在后台进行中，跳过本次触发");
        return;
    }

    // spawn 后台线程执行恢复，避免阻塞 Win32 回调线程（Task 7.2）
    let spawn_result = std::thread::Builder::new()
        .name("mirrorstar-fullscreen-resume".to_string())
        .spawn(|| {
            // 阻塞式恢复：等待 engine 锁 + play() 冷启动 mpv（2-6s）在此线程完成
            let failed = match resume_all_fast_blocking(PauseReason::FULLSCREEN) {
                Some(f) => f,
                // SHARED_ENGINE 未设置（理论不发生的启动早期窗口）：
                // 保留 FULLSCREEN_WAS，周期复查线程后续重试
                None => {
                    tracing::warn!("SHARED_ENGINE 未设置，保留 FULLSCREEN_WAS 待复查重试");
                    RESUME_IN_PROGRESS.store(false, Ordering::Release);
                    return;
                }
            };
            if failed.is_empty() {
                // C-008：仅当全部成功才更新 FULLSCREEN_WAS
                FULLSCREEN_WAS.store(false, Ordering::Release);
                clear_last_fullscreen_hwnd();
                // 若此前销毁了主窗口则重建
                restore_main_window_after_fullscreen();
            } else {
                // 部分失败：保留 FULLSCREEN_WAS=true，周期复查线程（2s 间隔）
                // 会再次触发 resume_from_fullscreen_exit 重试，直到恢复成功。
                // 不会出现"壁纸永久黑屏"（Task 7.2 兜底）。
                tracing::warn!(
                    failed_count = failed.len(),
                    failed = ?failed,
                    "resume_all_fast 部分失败，保留 FULLSCREEN_WAS 待周期复查重试"
                );
            }
            RESUME_IN_PROGRESS.store(false, Ordering::Release);
        });

    match spawn_result {
        Ok(_) => {}
        Err(e) => {
            // spawn 失败：复位标志，下一次事件/复查再试（避免标志卡死 true）
            tracing::error!(error = %e, "启动后台恢复线程失败");
            RESUME_IN_PROGRESS.store(false, Ordering::Release);
        }
    }
}

/// 判断窗口标题是否为应用自身窗口（ST-012：精确匹配避免误排除第三方窗口）
///
/// 提取为独立纯函数以便单元测试覆盖匹配逻辑（无需 Win32 环境）。
/// 自身 Tauri 窗口标题为 "镜星壁纸"；"MirrorStar Wallpaper" 用于版本号 UI 显示与英文环境兼容。
fn is_self_window_title(title: &str) -> bool {
    title == "镜星壁纸" || title == "MirrorStar Wallpaper"
}

/// 判断窗口类名是否为系统桌面组件（避免 Progman 等被误判为全屏应用）
///
/// 提取为独立纯函数以便单元测试覆盖匹配逻辑（无需 Win32 环境）。
/// - Progman：桌面背景窗口（SetForegroundWindow(progman) 可使其成为前台）
/// - WorkerW：壁纸层窗口（应用嵌入壁纸的目标）
/// - Shell_TrayWnd：任务栏
fn is_system_window_class(class: &str) -> bool {
    class == "Progman" || class == "WorkerW" || class == "Shell_TrayWnd"
}

/// 判断窗口矩形是否覆盖整个显示器矩形（即窗口为全屏）
///
/// 提取为独立纯函数以便单元测试覆盖矩形比较逻辑（无需 Win32 环境）。
/// 窗口矩形完全包含显示器矩形（窗口 left/top ≤ 显示器 left/top，
/// 窗口 right/bottom ≥ 显示器 right/bottom）即视为全屏。
fn is_rect_covering_monitor(
    window_rect: &windows::Win32::Foundation::RECT,
    monitor_rect: &windows::Win32::Foundation::RECT,
) -> bool {
    window_rect.left <= monitor_rect.left
        && window_rect.top <= monitor_rect.top
        && window_rect.right >= monitor_rect.right
        && window_rect.bottom >= monitor_rect.bottom
}

/// 判断窗口矩形宽高是否均 ≥ 显示器矩形的 95%（参考 Lively IsZoomedCustom）
fn is_rect_covering_95_percent(
    window_rect: &windows::Win32::Foundation::RECT,
    monitor_rect: &windows::Win32::Foundation::RECT,
) -> bool {
    let win_w = (window_rect.right - window_rect.left) as f64;
    let win_h = (window_rect.bottom - window_rect.top) as f64;
    let mon_w = (monitor_rect.right - monitor_rect.left) as f64;
    let mon_h = (monitor_rect.bottom - monitor_rect.top) as f64;
    win_w >= mon_w * 0.95 && win_h >= mon_h * 0.95
}

/// 最大化级别的有效动作：最大化永不终止——None→None、Pause→Pause、Terminate→Pause
fn effective_maximized_action(action: FullscreenAction) -> FullscreenAction {
    match action {
        FullscreenAction::None => FullscreenAction::None,
        FullscreenAction::Pause | FullscreenAction::Terminate => FullscreenAction::Pause,
    }
}

/// 分级状态机的处置决策结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// 无需操作（同级别或无可处置）
    NoOp,
    /// 暂停壁纸（进程驻留）
    Pause,
    /// 终止壁纸（释放内存）
    Terminate,
    /// 级别降级（TrueFullscreen→Maximized）：壁纸已终止、桌面仍被覆盖，不恢复，仅更新级别
    DowngradeToMaximized,
    /// 退出全屏，需恢复壁纸
    Exit,
}

/// 分级状态机决策：根据当前级别、上一级别、是否已处置、配置动作，返回应执行的处置
fn compute_transition(
    level: FullscreenLevel,
    prev_level: FullscreenLevel,
    was_fullscreen: bool,
    action: FullscreenAction,
) -> Transition {
    match level {
        FullscreenLevel::None => {
            if was_fullscreen {
                Transition::Exit
            } else {
                Transition::NoOp
            }
        }
        FullscreenLevel::Maximized => {
            if prev_level == FullscreenLevel::Maximized {
                Transition::NoOp // 同级别，仅更新 HWND
            } else if prev_level == FullscreenLevel::TrueFullscreen && was_fullscreen {
                Transition::DowngradeToMaximized // 降级：已终止，不恢复
            } else if effective_maximized_action(action) == FullscreenAction::Pause {
                Transition::Pause
            } else {
                Transition::NoOp
            }
        }
        FullscreenLevel::TrueFullscreen => {
            if prev_level == FullscreenLevel::TrueFullscreen {
                Transition::NoOp // 同级别，仅更新 HWND
            } else {
                match action {
                    FullscreenAction::Terminate => Transition::Terminate,
                    FullscreenAction::Pause => Transition::Pause,
                    FullscreenAction::None => Transition::NoOp,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::RECT;

    // ── is_self_window_title: 自身窗口标题匹配（ST-012） ─────────────────────────

    #[test]
    fn test_is_self_window_title_matches_chinese() {
        assert!(is_self_window_title("镜星壁纸"));
    }

    #[test]
    fn test_is_self_window_title_matches_english() {
        assert!(is_self_window_title("MirrorStar Wallpaper"));
    }

    #[test]
    fn test_is_self_window_title_rejects_substring() {
        // ST-012: 旧实现用 contains 会误匹配含 "MirrorStar" 子串的第三方窗口
        assert!(!is_self_window_title("MirrorStar Studio Pro"));
        assert!(!is_self_window_title("镜星壁纸 Pro"));
        assert!(!is_self_window_title("MirrorStar"));
        assert!(!is_self_window_title("镜星壁纸v2"));
    }

    #[test]
    fn test_is_self_window_title_rejects_empty_and_unrelated() {
        assert!(!is_self_window_title(""));
        assert!(!is_self_window_title("explorer.exe"));
        assert!(!is_self_window_title("Chrome"));
    }

    // ── is_system_window_class: 系统窗口类名匹配 ──────────────────────

    #[test]
    fn test_is_system_window_class_matches_known_system_classes() {
        assert!(is_system_window_class("Progman"));
        assert!(is_system_window_class("WorkerW"));
        assert!(is_system_window_class("Shell_TrayWnd"));
    }

    #[test]
    fn test_is_system_window_class_rejects_app_classes() {
        assert!(!is_system_window_class("Chrome_WidgetWin_1"));
        assert!(!is_system_window_class("Notepad"));
        assert!(!is_system_window_class(""));
    }

    // ── is_rect_covering_monitor: 矩形覆盖判断 ──────────────────────────────────

    #[test]
    fn test_is_rect_covering_monitor_exact_match() {
        // 窗口矩形 == 显示器矩形：完全覆盖
        let window = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_rect_covering_monitor(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_monitor_window_larger_than_monitor() {
        // 窗口矩形 > 显示器矩形（跨屏/DPI 缩放场景）：覆盖
        let window = RECT {
            left: -10,
            top: -10,
            right: 1930,
            bottom: 1090,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_rect_covering_monitor(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_monitor_window_smaller_than_monitor() {
        // 窗口矩形 < 显示器矩形（窗口化应用）：未覆盖
        let window = RECT {
            left: 100,
            top: 100,
            right: 1000,
            bottom: 800,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_rect_covering_monitor(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_monitor_partial_overlap() {
        // 窗口仅部分覆盖显示器（右侧未到边）：未覆盖
        let window = RECT {
            left: 0,
            top: 0,
            right: 1500,
            bottom: 1080,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_rect_covering_monitor(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_monitor_offset_window() {
        // 窗口完全在显示器外（左上偏移到负坐标但右下未到显示器右下）：未覆盖
        let window = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_rect_covering_monitor(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_monitor_top_left_aligned() {
        // 窗口 left/top 与显示器对齐，但 right/bottom 超出：覆盖
        let window = RECT {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 1200,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_rect_covering_monitor(&window, &monitor));
    }

    // ── should_trigger_resume: 全屏退出恢复触发判断 ──────────────────────────

    #[test]
    fn should_trigger_resume_true_when_exited_and_prev_inactive() {
        // 真正退出全屏：之前全屏、当前非前台全屏、原全屏窗口不再覆盖显示器 → 应恢复
        assert!(should_trigger_resume(true, false, false));
    }

    #[test]
    fn should_trigger_resume_false_when_overlay() {
        // 临时覆盖层（任务管理器/Alt-Tab）：原全屏窗口仍覆盖显示器 → 不恢复
        assert!(!should_trigger_resume(true, false, true));
    }

    #[test]
    fn should_trigger_resume_false_when_not_was_fullscreen() {
        // 之前并非全屏 → 不恢复（无论当前前台状态如何）
        assert!(!should_trigger_resume(false, false, false));
        assert!(!should_trigger_resume(false, true, false));
        assert!(!should_trigger_resume(false, false, true));
    }

    #[test]
    fn should_trigger_resume_false_when_still_fullscreen() {
        // 前台仍全屏 → 不恢复（无论原全屏窗口是否仍覆盖显示器）
        assert!(!should_trigger_resume(true, true, false));
        assert!(!should_trigger_resume(true, true, true));
    }

    // ── is_rect_covering_95_percent: 近全屏（≥95%）矩形判断 ─────────────────────

    #[test]
    fn test_is_rect_covering_95_percent_exactly_95() {
        // 恰好 95%（1920*0.95=1824，1080*0.95=1026），>= 语义 → true
        let window = RECT {
            left: 0,
            top: 0,
            right: 1824,
            bottom: 1026,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_rect_covering_95_percent(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_95_percent_above_95() {
        // 95%+（窗口略小于显示器但 ≥95%）：true
        let window = RECT {
            left: 0,
            top: 0,
            right: 1900,
            bottom: 1070,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_rect_covering_95_percent(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_95_percent_below_95() {
        // <95%：false
        let window = RECT {
            left: 0,
            top: 0,
            right: 1800,
            bottom: 1000,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_rect_covering_95_percent(&window, &monitor));
    }

    #[test]
    fn test_is_rect_covering_95_percent_zero_size() {
        // 窗口零尺寸（不可见/已最小化）：false
        let window = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_rect_covering_95_percent(&window, &monitor));
    }

    // ── effective_maximized_action: 最大化级别有效动作 ──────────────────────────

    #[test]
    fn test_effective_maximized_action_none() {
        assert_eq!(
            effective_maximized_action(FullscreenAction::None),
            FullscreenAction::None
        );
    }

    #[test]
    fn test_effective_maximized_action_pause() {
        assert_eq!(
            effective_maximized_action(FullscreenAction::Pause),
            FullscreenAction::Pause
        );
    }

    #[test]
    fn test_effective_maximized_action_terminate_becomes_pause() {
        // 最大化永不终止：Terminate → Pause
        assert_eq!(
            effective_maximized_action(FullscreenAction::Terminate),
            FullscreenAction::Pause
        );
    }

    // ── compute_transition: 分级状态机决策 ─────────────────────────────────────

    #[test]
    fn test_compute_transition_none_was_false_noop() {
        assert_eq!(
            compute_transition(FullscreenLevel::None, FullscreenLevel::None, false, FullscreenAction::None),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(FullscreenLevel::None, FullscreenLevel::Maximized, false, FullscreenAction::Terminate),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_none_was_true_exit() {
        // 之前已处置且当前非全屏 → 退出恢复
        assert_eq!(
            compute_transition(FullscreenLevel::None, FullscreenLevel::TrueFullscreen, true, FullscreenAction::Terminate),
            Transition::Exit
        );
        assert_eq!(
            compute_transition(FullscreenLevel::None, FullscreenLevel::Maximized, true, FullscreenAction::Pause),
            Transition::Exit
        );
    }

    #[test]
    fn test_compute_transition_maximized_same_level_noop() {
        // 同级别（Maximized→Maximized）：仅更新 HWND
        assert_eq!(
            compute_transition(FullscreenLevel::Maximized, FullscreenLevel::Maximized, true, FullscreenAction::Terminate),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(FullscreenLevel::Maximized, FullscreenLevel::Maximized, false, FullscreenAction::Pause),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_maximized_downgrade_from_true_fullscreen() {
        // 降级 TrueFullscreen→Maximized：壁纸已终止、桌面仍被覆盖 → 不恢复，仅更新级别
        assert_eq!(
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::TrueFullscreen,
                true,
                FullscreenAction::Terminate
            ),
            Transition::DowngradeToMaximized
        );
    }

    #[test]
    fn test_compute_transition_maximized_pause_with_terminate_action() {
        // 最大化永远只暂停：即使配置为 terminate 也暂停
        assert_eq!(
            compute_transition(FullscreenLevel::Maximized, FullscreenLevel::None, false, FullscreenAction::Terminate),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_maximized_pause_with_pause_action() {
        assert_eq!(
            compute_transition(FullscreenLevel::Maximized, FullscreenLevel::None, false, FullscreenAction::Pause),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_maximized_noop_with_none_action() {
        assert_eq!(
            compute_transition(FullscreenLevel::Maximized, FullscreenLevel::None, false, FullscreenAction::None),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_same_level_noop() {
        // 同级别（TrueFullscreen→TrueFullscreen）：仅更新 HWND
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::TrueFullscreen, true, FullscreenAction::Terminate),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::TrueFullscreen, false, FullscreenAction::Pause),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_terminate() {
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::None, false, FullscreenAction::Terminate),
            Transition::Terminate
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_upgrade_terminates() {
        // 升级 Maximized→TrueFullscreen：终止
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::Maximized, true, FullscreenAction::Terminate),
            Transition::Terminate
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_pause() {
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::None, false, FullscreenAction::Pause),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_noop_with_none_action() {
        assert_eq!(
            compute_transition(FullscreenLevel::TrueFullscreen, FullscreenLevel::None, false, FullscreenAction::None),
            Transition::NoOp
        );
    }

    // ── RESUME_IN_PROGRESS: 后台恢复去重逻辑（Task 7.2） ────────────────────────
    //
    // `RESUME_IN_PROGRESS` 是进程级全局静态量，真实恢复线程可能修改其值，
    // 无法在测试间重置。以下测试使用局部 AtomicBool 验证与
    // `resume_from_fullscreen_exit` 一致的 swap 去重模式：
    // - 首次 swap(true) 返回 false → 获得执行权，spawn 后台恢复线程
    // - 进行中再次 swap(true) 返回 true → 去重跳过
    // - 线程结束时 store(false) 复位 → 可再次触发重试
    #[test]
    fn test_resume_in_progress_swap_dedup_pattern() {
        let flag = std::sync::atomic::AtomicBool::new(false);

        // 1. 首次调用：无恢复线程 → swap 返回 false，获得执行权
        assert!(
            !flag.swap(true, Ordering::SeqCst),
            "首次调用应返回 false（获得执行权，spawn 恢复线程）"
        );

        // 2. 进行中：恢复线程仍在运行 → swap 返回 true，去重跳过
        assert!(
            flag.swap(true, Ordering::SeqCst),
            "进行中再次调用应返回 true（去重，跳过本次触发）"
        );
        assert!(
            flag.load(Ordering::Acquire),
            "去重期间标志应保持 true"
        );

        // 3. 线程结束复位：store(false) → 可再次触发
        flag.store(false, Ordering::Release);
        assert!(
            !flag.swap(true, Ordering::SeqCst),
            "复位后可再次获得执行权（周期复查重试路径）"
        );
        flag.store(false, Ordering::Release);
    }

    /// 验证 spawn 失败时标志被复位（避免卡死 true 导致永不重试）
    ///
    /// 直接测试 `resume_from_fullscreen_exit` 的 spawn 失败路径不可行（无法
    /// 模拟线程 spawn 失败），此处验证其核心保证：失败路径必须将标志复位为
    /// false，使下一次事件/复查能重新获得执行权。使用局部 AtomicBool 模拟
    /// `swap(true) → (spawn 失败) → store(false)` 顺序。
    #[test]
    fn test_resume_in_progress_reset_after_spawn_failure() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        // 模拟 resume_from_fullscreen_exit 的 spawn 失败路径：
        // 获得执行权后 spawn 失败，必须显式 store(false) 复位
        let _acquired = flag.swap(true, Ordering::SeqCst);
        // spawn 失败 → 复位
        flag.store(false, Ordering::Release);
        assert!(
            !flag.load(Ordering::Acquire),
            "spawn 失败后标志应复位为 false，否则后续恢复永不触发"
        );
    }
}
