use mirrorstar_core::{ConfigManager, FullscreenAction, PauseReason, WallpaperEngine};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::state::{
    pause_all_fast_blocking, resume_all_fast_blocking, terminate_all_fast_blocking,
    SendWinEventHook, FULLSCREEN_MONITOR_RUNNING, FULLSCREEN_MONITOR_THREAD,
    FULLSCREEN_MONITOR_THREAD_ID, FULLSCREEN_WAS, SHARED_CONFIG, SHARED_ENGINE, WIN_EVENT_HOOKS,
};

// 全屏检测算法与误判场景文档化
//
// # 全屏检测算法（事件驱动 + 即时查询）
//
// ## 检测入口（事件驱动状态调和）
//
// 通过两个 `SetWinEventHook`（`WINEVENT_OUTOFCONTEXT`）注册事件：
//
// 1. **系统事件**（`EVENT_SYSTEM_FOREGROUND .. EVENT_SYSTEM_MINIMIZEEND`）：前台切换、
//    最小化/还原；
// 2. **对象事件**（`EVENT_OBJECT_DESTROY .. EVENT_OBJECT_UNCLOAKED`，
//    `WINEVENT_SKIPOWNPROCESS`）：窗口销毁/隐藏/Cloak/位置与状态变化。
//
// 所有事件统一经 `win_event_callback` 过滤后调用 `reconcile_fullscreen(reason)`。
// 关键点：**独占全屏（D3D `ChangeDisplaySettings` / borderless→exclusive）在同一个
// 前台窗口内部完成，不会产生 `EVENT_SYSTEM_FOREGROUND`**；必须依赖对象事件中的
// `LOCATIONCHANGE`/`STATECHANGE` 才能发现"进入全屏"，否则会漏检到退出时才处置。
//
// ## 检测流程（`observe_fullscreen` + `resolve_fullscreen`）
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
// 4. **获取可见窗口矩形**：`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`（不含
//    Win10/11 不可见 resize 边框），失败回退 `GetWindowRect`
// 5. **获取显示器信息**：
//    - `MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)` 获取窗口所在显示器
//    - `GetMonitorInfoW` 读取显示器矩形；`MONITORINFOEXW.szDevice` 作为会话 `display_id`
// 6. **会话优先分级**（`resolve_fullscreen`）：
//    - 活跃会话（存活/未 Cloak/仍覆盖或 QUNS 仍全屏）→ 沿用会话级别；
//      覆盖层（任务管理器等不覆盖前台）不得改写会话身份
//    - 前台可信且 100% 覆盖 → `TrueFullscreen`（新会话）
//    - 前台可信且 `IsZoomed` 或 ≥95% → `Maximized`
//    - 其余 → `None`
// 7. **QUNS 辅助**：`SHQueryUserNotificationState` 只用于触发探针、维持会话、
//    确认"系统仍全屏"；不单独创建全屏会话（避免覆盖层成为身份）
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
// - **事件驱动**：仅在系统/对象事件或 2s 兜底调和时运行；普通窗口拖动产生的
//   `LOCATIONCHANGE` 若不属于前台根/会话窗口会被直接忽略，前台根事件做 ≥100ms 节流；
// - **单次调和开销**：~1-5ms（5 个 Win32 API 调用 + 字符串比较）；
// - **无缓存**：每次调和即时查询，不缓存结果（事件驱动模式下无需缓存）。
//
// # 纯净壁纸模式与全屏处置
//
// 纯净壁纸模式运行期间，`state.rs` 的共享 helper（`pause_all_fast_blocking` /
// `try_resume_all_fast` / `terminate_all_fast_blocking` / `resume_all_fast_blocking`）
// 首部守卫会跳过全部处置动作（返回 None），`reconcile_fullscreen` 也直接返回，
// `FULLSCREEN_WAS` / `FULLSCREEN_LEVEL` / `FULLSCREEN_SESSION` 状态机因此保持冻结，
// 避免打断沉浸画面。模式退出后由 `reevaluate_now()` 补跑一次调和，纠正状态漂移。
//
// # 全屏会话状态机（事件驱动调和）
//
// 状态机以**全屏会话**（`FULLSCREEN_SESSION`: 会话根 HWND + 级别）为核心，而不是定时器：
//
// 1. 任意事件/兜底轮询/启动初始化都调用 `reconcile_fullscreen(reason)`，由它计算当前
//    前台级别并驱动迁移；同一会话同级别为 NoOp（幂等，不重复处置）。
// 2. 新会话登记后等待 `CONFIRM_DELAY`（400ms）复核；复核通过才执行暂停/终止。这个
//    "确认窗口"只用于过滤"进入全屏后立刻被销毁"的瞬态窗口。
// 3. 会话窗口的 `DESTROY`/`HIDE`/`CLOAKED`/最小化，或级别变化、策略切 none，都会**立即**
//    取消确认意图并结束会话——不需要猜"过了多久"。
// 4. 权威退出信号（会话销毁/隐藏/Cloak/最小化、级别回落）直接触发
//    `resume_from_fullscreen_exit()`；不再有 500ms 恢复冷却或 2s 等待。
// 5. 2s 兜底线程每次都执行完整调和，覆盖漏检的进入与退出；监控启动时先调和一次，
//    识别"应用启动时已处于全屏"的前台窗口。
//
// 根因：旧实现只监听 `EVENT_SYSTEM_FOREGROUND`，而独占全屏切换不产生前台事件；日志
// （2026-11-04）显示 mpv 在全屏期间持续运行、直到退出瞬间才首次"检测到真全屏"并终止，
// 再叠加 500ms 恢复冷却 + 2s 复查，造成约 0.9~1.0s 的静态壁纸闪现。

/// HWND 的 Send/Sync 包装器（HWND 原始类型包含裸指针，未实现 Send/Sync）
///
/// 与 state.rs 中 `SendWinEventHook` 的 soundness 论证一致：HWND 是窗口句柄，
/// 本质是一个指针大小的数值令牌，不持有任何 Rust 可变状态，仅在线程间共享
/// 该令牌本身（窗口销毁回收由 Win32 管理，非 Rust 所有权），跨线程共享安全。
/// 存储于 `Mutex<Option<SendHwnd>>`，所有读写均在 Mutex 守卫下进行，无别名问题。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// 记录最近一个全屏窗口的句柄及其触发处置时的级别
///
/// 用于区分"临时覆盖层"与"真正退出全屏"。注意这里额外保存处置时的级别：
/// 最大化（Maximized）级别的暂停在用户离开窗口后应允许恢复（IsZoomed 对仍在后台
/// 保持最大化的窗口恒为 true，若不区分级别会误判"仍覆盖桌面"导致壁纸卡死暂停），
/// 仅 TrueFullscreen 级别才保留"后台窗口仍覆盖/仍最大化即拦截恢复"的覆盖层语义。
#[derive(Clone, Copy)]
struct LastFullscreen {
    hwnd: Option<SendHwnd>,
    level: FullscreenLevel,
}

static LAST_FULLSCREEN: std::sync::Mutex<LastFullscreen> = std::sync::Mutex::new(LastFullscreen {
    hwnd: None,
    level: FullscreenLevel::None,
});
/// 当前检测到的全屏级别（分级状态机的状态，供事件回调/周期复查/后台恢复共享）
static FULLSCREEN_LEVEL: std::sync::Mutex<FullscreenLevel> =
    std::sync::Mutex::new(FullscreenLevel::None);

/// 全屏会话：会话根 HWND + 级别 + 显示器 ID
///
/// 生命周期事件（destroy/hide/cloak/minimize）与对象事件过滤都以会话根 HWND 为准；
/// 同一会话同级别重复调和为 NoOp，只有"可信且 100% 覆盖"的新前台窗口才重新登记处置意图。
/// `display_id` 仅用于诊断与后续按显示器能力预留，当前处置仍为全局。
#[derive(Clone, Debug)]
struct FullscreenSession {
    hwnd: SendHwnd,
    level: FullscreenLevel,
    display_id: Option<String>,
}

static FULLSCREEN_SESSION: std::sync::Mutex<Option<FullscreenSession>> =
    std::sync::Mutex::new(None);

/// 调和串行锁：前台事件、生命周期事件、兜底轮询可能并发进入 `reconcile_fullscreen`
/// 使用 `try_lock`（拿不到说明已有调和在进行，本次直接跳过，后续事件/轮询会补上）。
static RECONCILE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 对象事件（LOCATIONCHANGE/STATECHANGE）节流时间戳：避免窗口拖动时高频进入状态机
static LAST_WINDOW_STATE_RECONCILE_MS: AtomicU64 = AtomicU64::new(0);

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

/// 终止防抖冷却时长：Terminate 后 1s 内抑制重复终止（防止高频重入）
const TERMINATE_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(1000);
/// 最近一次全屏终止的时间戳（进程级，供终止防抖判定）
static LAST_TERMINATE_TS: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// 终止代数（进程级单调递增计数器，供异步恢复线程比对"是否过期"）
///
/// 竞态根因：Terminate T1 后恢复线程慢跑（2-6s 冷启动 mpv），期间用户进入新真全屏
/// T2，T2 再次 Terminate（置 FULLSCREEN_WAS=true），但 T1 的恢复
/// 线程结束时仍无条件置 FULLSCREEN_WAS=false 并 restore 主窗口，把壁纸/主窗口恢复到
/// T2 的全屏游戏之上。`RESUME_IN_PROGRESS` 只防并发恢复，不防"恢复被更新的终止推翻"。
///
/// 修复：每次**成功终止**（置 FULLSCREEN_WAS=true）时递增此代数。恢复线程在 spawn 时
/// 捕获当前代数，结束后若发现代数已变化（期间又有新终止发生），则放弃本次恢复（保持
/// FULLSCREEN_WAS=true、不 restore 主窗口），从而防止过期恢复覆盖后续新终止。
static TERMINATION_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 待执行处置（暂停/终止）确认意图代数（进程级单调递增）
///
/// 新全屏会话登记后不立即处置，而是等待 [`CONFIRM_DELAY`] 复核。登记与取消
///（会话销毁/隐藏/Cloak/最小化、级别变化、策略切 none）都递增本代数，确认线程醒来后
/// 若发现代数已变化，说明本次意图已被取消，直接放弃处置。
///
/// 根因：独占全屏切换不产生前台事件，旧实现直到退出瞬间才首次识别全屏并同步终止，
/// 再叠加恢复冷却，导致约 0.9~1.0s 的静态壁纸闪现（真机日志 2026-11-04）。
static PENDING_ACTION_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 会话确认窗口：新会话登记后等待该时长再执行暂停/终止
///
/// 只用于过滤"刚进入全屏就被销毁/隐藏"的瞬态窗口；真正的取消由窗口生命周期事件完成，
/// 不再依赖"过了多久"的猜测。400ms 足以覆盖常见的打开即关闭/闪屏场景。
const CONFIRM_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// 确认意图的处置动作
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    Pause,
    Terminate,
}

/// 全屏监控的 WinEvent 统一回调（系统事件 + 对象事件）
///
/// 只做"事件类型过滤 + 根祖先比较 + 节流判定 + 调和"，不在此加锁执行重活。
unsafe extern "system" fn win_event_callback(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // 用 catch_unwind 包裹整个回调体，避免回调内部 panic 在 unwind 构建中展开
    // 并越过 extern "system" 边界（Win32 回调）。注意 release 构建启用
    // panic = "abort"，panic 会直接终止进程，本保护仅在 dev/test 构建生效。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_win_event(event, hwnd)
    }));
    if result.is_err() {
        tracing::error!("全屏回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
    }
}

/// 事件过滤：把系统/对象事件映射为一次状态调和（其余事件直接忽略）
fn handle_win_event(event: u32, hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_STATECHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    };

    if event == EVENT_SYSTEM_FOREGROUND {
        reconcile_fullscreen("foreground");
        return;
    }
    if hwnd.is_invalid() {
        return;
    }

    // 销毁/隐藏/Cloak 事件对顶层窗口触发；直接用原始 HWND 比较（销毁后 GetAncestor 不可靠）。
    let raw_matches = session_hwnd_matches(SendHwnd(hwnd));
    let root = SendHwnd(root_hwnd(hwnd));
    let root_matches = session_hwnd_matches(root);

    match event {
        EVENT_SYSTEM_MINIMIZESTART => {
            if root_matches {
                end_session("minimize");
                reconcile_fullscreen("minimize");
            }
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            if root_matches {
                reconcile_fullscreen("restore");
            }
        }
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
            if raw_matches {
                let reason = match event {
                    EVENT_OBJECT_DESTROY => "session_destroyed",
                    EVENT_OBJECT_HIDE => "session_hidden",
                    _ => "session_cloaked",
                };
                end_session(reason);
                reconcile_fullscreen(reason);
            }
        }
        EVENT_OBJECT_UNCLOAKED => {
            if raw_matches || root_matches {
                reconcile_fullscreen("uncloak");
            }
        }
        EVENT_OBJECT_LOCATIONCHANGE | EVENT_OBJECT_STATECHANGE
            if (root_matches || root == foreground_root()) && window_state_reconcile_allowed() =>
        {
            reconcile_fullscreen("window_state");
        }
        _ => {}
    }
}

/// 全屏状态统一调和入口（唯一状态驱动入口）
///
/// 由前台事件、窗口生命周期事件、2s 兜底轮询、启动初始化、纯净模式退出补跑共同调用。
/// 幂等：会话与级别未变化时只更新诊断字段，不重复处置。
fn reconcile_fullscreen(reason: &'static str) {
    if mirrorstar_core::pure_mode::is_active() {
        tracing::debug!(
            reason,
            "纯净壁纸模式运行中，跳过全屏状态调和（退出后会补跑）"
        );
        return;
    }
    // 串行化：事件回调与兜底线程可能并发进入；拿不到锁说明已有调和在跑，本次跳过。
    let _guard = match RECONCILE_GUARD.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::debug!(reason, "已有全屏调和在进行，跳过本次触发");
            return;
        }
    };

    let Some(config) = SHARED_CONFIG.get() else {
        return;
    };
    let action = config.get_config().pause.fullscreen_action;

    // 单次观测：QUNS 只读一次，供级别解析与日志复用
    let observation = observe_fullscreen();
    let prev_level = *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner());
    let was_fullscreen = FULLSCREEN_WAS.load(Ordering::Acquire);
    let session = FULLSCREEN_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let session_probe = session.as_ref().map(|current| SessionProbe {
        hwnd: current.hwnd,
        level: current.level,
        alive: session_window_alive(current.hwnd),
        covers: is_window_covering_monitor(current.hwnd.0),
        cloaked: is_window_cloaked(current.hwnd.0),
    });
    let resolution = resolve_fullscreen(
        session_probe,
        observation.quns_fullscreen(),
        ObservedForeground {
            root: observation.fg_root,
            credible: observation.fg_credible,
            covers: observation.fg_covers,
            maximized: observation.fg_maximized,
        },
    );
    let level = resolution.level;
    let anchor = resolution.anchor.unwrap_or(observation.fg_root);

    tracing::debug!(
        reason,
        level = ?level,
        prev_level = ?prev_level,
        was_fullscreen,
        quns = ?observation.quns.map(|state| state.0),
        fg_root = ?observation.fg_root,
        fg_credible = observation.fg_credible,
        fg_covers = observation.fg_covers,
        fg_maximized = observation.fg_maximized,
        fg_iconic = observation.fg_iconic,
        fg_cloaked = observation.fg_cloaked,
        session = ?session,
        "全屏状态调和"
    );

    // 处置策略切换为 none：清理残留并恢复一次
    if action == FullscreenAction::None {
        if was_fullscreen || session.is_some() {
            tracing::info!(reason, "全屏动作已切换为无操作，恢复壁纸并清理残留");
            end_session("action_none");
            resume_from_fullscreen_exit();
            *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
        }
        return;
    }

    let transition = compute_transition(level, prev_level, was_fullscreen, action);
    match transition {
        Transition::NoOp => {
            if level != FullscreenLevel::None {
                // 覆盖层（不覆盖的前台）不得改写会话与 LAST：锚定窗口由解析结果给出
                update_last_fullscreen_at(anchor, level);
                // 全屏应用切换：只更新会话身份与 display_id，不重复暂停/终止
                if let Some(new_hwnd) = resolution.switch_to {
                    if !session.as_ref().is_some_and(|s| s.hwnd.0 == new_hwnd.0) {
                        begin_session(new_hwnd, level, observation.display_id.clone());
                    }
                } else if session.is_none()
                    && observation.fg_credible
                    && ((level == FullscreenLevel::TrueFullscreen && observation.fg_covers)
                        || (level == FullscreenLevel::Maximized && observation.fg_maximized))
                {
                    // 会话为空但级别已由上次处置残留：补登记身份，保证生命周期事件可结束会话
                    begin_session(observation.fg_root, level, observation.display_id.clone());
                }
                // 安全网：级别已是真全屏但壁纸仍为播放态（异常残留），重新登记确认
                if level == FullscreenLevel::TrueFullscreen
                    && !was_fullscreen
                    && observation.fg_credible
                    && observation.fg_covers
                {
                    let confirm_action = if action == FullscreenAction::Terminate {
                        ConfirmAction::Terminate
                    } else {
                        ConfirmAction::Pause
                    };
                    begin_session(observation.fg_root, level, observation.display_id.clone());
                    FULLSCREEN_WAS.store(true, Ordering::Release);
                    schedule_confirm_action(confirm_action);
                }
            }
            *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
        }
        Transition::Pause | Transition::Terminate => {
            let confirm_action = match transition {
                Transition::Pause => ConfirmAction::Pause,
                _ => ConfirmAction::Terminate,
            };
            // 终止防抖：刚终止过（1s 内）不重复终止，但仍登记会话，避免后续事件再次触发
            if confirm_action == ConfirmAction::Terminate && !should_allow_terminate() {
                tracing::warn!("检测到真全屏应用，但处于终止冷却期内，跳过本次终止（防抖）");
                cancel_pending_action();
                begin_session(observation.fg_root, level, observation.display_id.clone());
                *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
                update_last_fullscreen_at(observation.fg_root, level);
                return;
            }
            log_fullscreen_context(level);
            begin_session(observation.fg_root, level, observation.display_id.clone());
            FULLSCREEN_WAS.store(true, Ordering::Release);
            *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = level;
            update_last_fullscreen_at(observation.fg_root, level);
            schedule_confirm_action(confirm_action);
        }
        Transition::DowngradeToMaximized => {
            // 已离开真全屏但仍在最大化窗口下：取消待执行处置，会话级别降为 Maximized
            cancel_pending_action();
            if let Some(current) = session.as_ref() {
                if current.hwnd.0 == anchor.0 {
                    *FULLSCREEN_SESSION.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(FullscreenSession {
                            hwnd: current.hwnd,
                            level: FullscreenLevel::Maximized,
                            display_id: current.display_id.clone(),
                        });
                }
            }
            tracing::info!(reason, "级别降级 TrueFullscreen→Maximized，壁纸保持不恢复");
            *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) =
                FullscreenLevel::Maximized;
            update_last_fullscreen_at(anchor, FullscreenLevel::Maximized);
        }
        Transition::Exit => {
            // 临时覆盖层：原全屏窗口仍覆盖显示器 → 不恢复、不结束会话
            if is_previous_fullscreen_window_still_active() {
                tracing::info!(
                    reason,
                    "前台切换但原全屏窗口仍覆盖显示器（临时覆盖层），跳过壁纸恢复"
                );
                return;
            }
            end_session("exit");
            tracing::info!(reason, "退出全屏，恢复壁纸");
            resume_from_fullscreen_exit();
            *FULLSCREEN_LEVEL.lock().unwrap_or_else(|e| e.into_inner()) = FullscreenLevel::None;
        }
    }
}

/// 纯净壁纸模式退出后补跑一次全屏状态调和
pub(crate) fn reevaluate_now() {
    tracing::info!("纯净壁纸模式退出后补跑一次全屏状态调和");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reconcile_fullscreen("pure_mode_exit")
    }));
    if result.is_err() {
        tracing::error!("全屏补跑调和 panic 已被捕获，防止穿越 FFI 硬崩溃");
    }
}

/// Task 5.1：记录前台窗口上下文，便于真机验证自家窗口识别/排除逻辑
///
/// 在全屏处置（Pause/Terminate）分支执行时记录前台窗口的 hwnd / 标题 / 类名 / 父窗口 / 进程 PID。
fn log_fullscreen_context(level: FullscreenLevel) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetParent, GetWindowTextW, GetWindowThreadProcessId,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return;
        }
        let mut title = [0u16; TITLE_BUF_LEN];
        let tlen = GetWindowTextW(fg, &mut title);
        let mut class = [0u16; TITLE_BUF_LEN];
        let clen = GetClassNameW(fg, &mut class);
        let mut pid: u32 = 0;
        let _ = GetWindowThreadProcessId(fg, Some(&mut pid));
        let parent = GetParent(fg);
        // 前台窗口切换频繁，完整上下文只在排查时用 DEBUG 观察，避免 INFO 日志刷屏。
        tracing::debug!(
            level = ?level,
            hwnd = ?fg,
            title = %String::from_utf16_lossy(&title[..tlen as usize]),
            class = %String::from_utf16_lossy(&class[..clen as usize]),
            parent = ?parent,
            pid,
            "全屏处置上下文：前台窗口信息"
        );
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
        // 1. 取出并注销全部旧 hook（系统 + 对象）
        let old_hooks = WIN_EVENT_HOOKS
            .lock()
            .map(|mut h| std::mem::take(&mut *h))
            .unwrap_or_default();
        for old_hook in old_hooks {
            unsafe {
                // 重启清理路径：unhook 失败时旧钩子随进程退出或下次重启自动回收
                let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(old_hook.0);
            }
        }
        // 2. take 旧 thread_id 并 PostThreadMessage WM_QUIT 唤醒
        crate::state::signal_thread_exit(&FULLSCREEN_MONITOR_THREAD_ID);
        // 3. take 旧 JoinHandle 并 join 等待旧线程退出
        crate::state::join_monitor_thread(&FULLSCREEN_MONITOR_THREAD);
        // 4. 停止旧周期复查线程（循环检查 RUNNING 标志，join 在 2s 内完成）
        FULLSCREEN_REVIEW_RUNNING.store(false, Ordering::SeqCst);
        if let Some(handle) = FULLSCREEN_REVIEW_THREAD
            .lock()
            .ok()
            .and_then(|mut t| t.take())
        {
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

            // 设置事件钩子：
            // 1) 系统事件：前台切换 + 最小化/还原；
            // 2) 对象事件：销毁/隐藏/Cloak/位置与状态变化（发现独占全屏在窗口内部切换）。
            let system_hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_MINIMIZEEND,
                    None,
                    Some(win_event_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            // 系统钩子失败（返回 0）：重置运行标志并退出线程，避免
            // RUNNING=true 但无有效 hook 的"假运行"状态
            if system_hook.0.is_null() {
                tracing::error!("SetWinEventHook(系统事件) 返回 0，全屏监控启动失败");
                FULLSCREEN_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            let mut registered_hooks = vec![SendWinEventHook(system_hook)];

            let object_hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_DESTROY,
                    EVENT_OBJECT_UNCLOAKED,
                    None,
                    Some(win_event_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if object_hook.0.is_null() {
                tracing::warn!(
                    "SetWinEventHook(对象事件) 返回 0：独占全屏进入/退出可能漏检，2s 兜底调和仍生效"
                );
            } else {
                registered_hooks.push(SendWinEventHook(object_hook));
            }

            // 存储句柄到全局静态变量，供退出/重启时通过 UnhookWinEvent 释放
            if let Ok(mut hooks) = WIN_EVENT_HOOKS.lock() {
                *hooks = registered_hooks;
            }

            // 启动初始化：识别"应用启动时已处于全屏"的前台窗口
            reconcile_fullscreen("startup");

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
                            // 与 explorer.rs 消息循环的同款 -1 判定（N-002）一致，并按 spec 要求记录 GetLastError。
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

            // 线程退出清理：注销本线程注册的全部钩子（shutdown 路径可能已提前 take，
            // 此时列表为空；两者谁先取到谁负责 Unhook，幂等安全）。
            let hooks = WIN_EVENT_HOOKS
                .lock()
                .map(|mut h| std::mem::take(&mut *h))
                .unwrap_or_default();
            for hook in hooks {
                unsafe {
                    let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(hook.0);
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

/// 全屏轻量探针快照（QUNS + 前台根 + 最小化/Cloak），用于 500ms 快速变化检测
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeSnapshot {
    /// `SHQueryUserNotificationState` 的原始值（调用失败为 None）
    quns: Option<i32>,
    /// 前台窗口根 HWND 数值（无前台窗口为 0）
    foreground_root: isize,
    /// 前台根窗口是否最小化
    iconic: bool,
    /// 前台根窗口是否被 DWM Cloak
    cloaked: bool,
}

/// 上次探针快照（监控线程私有节奏，不参与状态机）
static LAST_PROBE: std::sync::Mutex<Option<ProbeSnapshot>> = std::sync::Mutex::new(None);

/// 轻量探针周期：对齐主流实现（Lively 默认 500ms），保证独占全屏的发现时延 ≤1s
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 每 N 个探针 tick（≈2s）执行一次全量兜底调和
const FULL_RECONCILE_EVERY_TICKS: u32 = 4;

/// 采集当前轻量探针快照（只读 Win32 查询，不进入 engine 锁）
fn probe_snapshot() -> ProbeSnapshot {
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    let root = root_hwnd(fg);
    let valid = !root.is_invalid();
    ProbeSnapshot {
        quns: query_user_notification_state().map(|state| state.0),
        foreground_root: if valid { root.0 as isize } else { 0 },
        iconic: valid && unsafe { IsIconic(root) }.as_bool(),
        cloaked: valid && is_window_cloaked(root),
    }
}

/// 启动全屏状态探针线程（500ms 轻量探针 + 2s 全量兜底）
///
/// 覆盖场景：
/// - 独占全屏（D3D `ChangeDisplaySettings`）不产生前台/对象事件 → 由 QUNS 探针发现；
/// - 无前台切换事件的退出（取消最大化、后台关闭、最小化/Cloak）→ 由探针/兜底恢复；
/// - 事件源全部失效时仍有 2s 全量调和收敛。
fn spawn_fullscreen_review_thread() {
    // 防止重复 spawn（start_fullscreen_monitor 二次调用场景）
    if FULLSCREEN_REVIEW_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    match std::thread::Builder::new()
        .name("mirrorstar-fullscreen-probe".to_string())
        .spawn(move || {
            // 重启时清空快照，保证首个 tick 触发一次调和
            *LAST_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let mut tick: u32 = 0;
            while FULLSCREEN_REVIEW_RUNNING.load(Ordering::Acquire) {
                std::thread::sleep(PROBE_INTERVAL);
                if !FULLSCREEN_REVIEW_RUNNING.load(Ordering::Acquire) {
                    break;
                }
                tick = tick.wrapping_add(1);
                let current = probe_snapshot();
                let changed = {
                    let mut last = LAST_PROBE.lock().unwrap_or_else(|e| e.into_inner());
                    let changed = last.map(|prev| prev != current).unwrap_or(true);
                    *last = Some(current);
                    changed
                };
                if changed {
                    reconcile_fullscreen("probe");
                }
                if tick % FULL_RECONCILE_EVERY_TICKS == 0 {
                    // 2s 全量兜底：覆盖探针字段未变化但状态仍需修正的边角场景
                    reconcile_fullscreen("watchdog");
                }
            }
        }) {
        Ok(handle) => {
            if let Ok(mut t) = FULLSCREEN_REVIEW_THREAD.lock() {
                *t = Some(handle);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "启动全屏探针线程失败");
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

/// 自家壁纸渲染器窗口标识（mpv 经 `--title=MirrorStarVideo` 固定标题）
const OWN_WALLPAPER_TITLE: &str = "MirrorStarVideo";
/// 自家壁纸渲染器窗口类名（mpv 编译时固定 `MPV_WINDOW_CLASS_NAME = "mpv"`）
const OWN_WALLPAPER_CLASS: &str = "mpv";

/// 自家壁纸渲染器窗口的 SetPropW 窗口属性名（与 subprocess_base 打标记处一致）
///
/// `set_hwnd` 用 `SetPropW` 在本应用 mpv/wp-proc 窗口上打此标记，属性与窗口同生共死，
/// 无 PID 复用竞态；全屏检测用 `GetPropW` 查询命中即豁免。
const WALLPAPER_WINDOW_PROP: windows::core::PCWSTR = windows::core::w!("MirrorStarWallpaper");

/// QUNS 权威判定结果（纯数据，便于单测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QunsVerdict {
    /// 桌面被全屏/演示模式占用（QUNS 2/3/4）
    Fullscreen,
    /// 明确不在全屏（锁屏/屏保，QUNS 1）
    NotFullscreen,
    /// 无法由 QUNS 判定，回退窗口矩形分级（QUNS 5/6/7 或调用失败）
    Fallback,
}

/// 查询 Windows 用户通知状态（检测独占 D3D 全屏 / 演示模式的官方 API）
///
/// `QUNS_RUNNING_D3D_FULL_SCREEN` 是 Lively Wallpaper 等主流实现用来识别独占全屏的
/// 权威信号；查询失败时返回 None，调用方回退矩形判定。
fn query_user_notification_state(
) -> Option<windows::Win32::UI::Shell::QUERY_USER_NOTIFICATION_STATE> {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::debug!(error = %e, "SHQueryUserNotificationState 调用失败，回退矩形判定");
            None
        }
    }
}

/// QUNS → 全屏判定（纯函数，便于单测）
///
/// - 1（锁屏/屏保）→ 明确非全屏；
/// - 2（全屏应用/演示设置）、3（D3D 独占全屏）、4（演示模式）→ 全屏；
/// - 5/6/7 → 回退矩形判定。
fn quns_verdict(state: windows::Win32::UI::Shell::QUERY_USER_NOTIFICATION_STATE) -> QunsVerdict {
    use windows::Win32::UI::Shell::{
        QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    if state == QUNS_NOT_PRESENT {
        QunsVerdict::NotFullscreen
    } else if state == QUNS_RUNNING_D3D_FULL_SCREEN
        || state == QUNS_BUSY
        || state == QUNS_PRESENTATION_MODE
    {
        QunsVerdict::Fullscreen
    } else {
        QunsVerdict::Fallback
    }
}

/// DWM 可见窗口边界（不含 Win10/11 不可见 resize 边框）；失败回退 `GetWindowRect`
fn visible_window_rect(
    hwnd: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::RECT> {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    let mut rect = windows::Win32::Foundation::RECT::default();
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<windows::Win32::Foundation::RECT>() as u32,
        )
    };
    if result.is_ok() && rect.right > rect.left && rect.bottom > rect.top {
        return Some(rect);
    }
    // D3D 独占全屏可能绕过 DWM：回退 GetWindowRect（此时通常仍等于显示器矩形）
    let mut fallback = windows::Win32::Foundation::RECT::default();
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut fallback) }
        .is_ok()
    {
        Some(fallback)
    } else {
        None
    }
}

/// 窗口是否被 DWM Cloak（UWP 切后台/挂起时不可见）；API 失败按"未 Cloak"处理
fn is_window_cloaked(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    let mut cloaked: u32 = 0;
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    result.is_ok() && cloaked != 0
}

/// 前台窗口是否"可信"（排除自身窗口、系统桌面组件、自家壁纸渲染器、Cloak、最小化）
///
/// 这是"窗口身份"判据，与系统级 QUNS 无关：覆盖层（任务管理器、浮动窗口）虽然可能
/// 被 QUNS 报为全屏环境，但不能作为全屏会话身份。
fn foreground_window_is_credible(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetPropW, GetWindowTextW, IsIconic,
    };
    unsafe {
        if hwnd.is_invalid() {
            return false;
        }

        // 跳过我们自己的窗口（同时匹配中文标题"镜星壁纸"和英文标题"MirrorStar Wallpaper"）
        // ST-012：精确匹配而非 contains，避免误排除标题含 "MirrorStar" 子串的第三方窗口。
        let mut title = [0u16; TITLE_BUF_LEN];
        let title_len = GetWindowTextW(hwnd, &mut title);
        let title_str = if title_len > 0 {
            String::from_utf16_lossy(&title[..title_len as usize])
        } else {
            String::new()
        };
        if is_self_window_title(&title_str) {
            return false;
        }

        // 排除桌面/壁纸层/任务栏窗口（Progman/WorkerW/Shell_TrayWnd 等）
        let mut class_name = [0u16; TITLE_BUF_LEN];
        let class_len = GetClassNameW(hwnd, &mut class_name);
        let class_str = if class_len > 0 {
            String::from_utf16_lossy(&class_name[..class_len as usize])
        } else {
            String::new()
        };
        if is_system_window_class(&class_str) {
            return false;
        }

        // 排除自家壁纸渲染器窗口：优先窗口属性标记，标题/类名兜底
        let has_prop_mark = !GetPropW(hwnd, WALLPAPER_WINDOW_PROP).0.is_null();
        if has_prop_mark || is_own_wallpaper_window(&title_str, &class_str) {
            return false;
        }

        // UWP Cloak / 最小化窗口不是可见的全屏候选
        if is_window_cloaked(hwnd) || IsIconic(hwnd).as_bool() {
            return false;
        }
        true
    }
}

/// 窗口所在显示器的矩形；失败 None
fn monitor_rect_for_window(
    hwnd: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::RECT> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Some(info.rcMonitor)
    } else {
        None
    }
}

/// 窗口覆盖判定：返回 (是否 100% 覆盖, 是否 ≥95% 覆盖)
fn window_coverage(hwnd: windows::Win32::Foundation::HWND) -> (bool, bool) {
    let Some(rect) = visible_window_rect(hwnd) else {
        return (false, false);
    };
    let Some(monitor_rect) = monitor_rect_for_window(hwnd) else {
        return (false, false);
    };
    (
        is_rect_covering_monitor(&rect, &monitor_rect),
        is_rect_covering_95_percent(&rect, &monitor_rect),
    )
}

/// 窗口是否 100% 覆盖其所在显示器（DWM 可见边界）
fn is_window_covering_monitor(hwnd: windows::Win32::Foundation::HWND) -> bool {
    window_coverage(hwnd).0
}

/// 显示器设备名（如 `\\.\DISPLAY1`）；失败 None
fn display_id_for_window(hwnd: windows::Win32::Foundation::HWND) -> Option<String> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // MONITORINFOEXW 的 `monitorInfo` 为首字段（repr(C)），整体指针可安全转为 MONITORINFO*
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) }.as_bool();
    if !ok {
        return None;
    }
    let mut name = String::new();
    for &unit in info.szDevice.iter() {
        if unit == 0 {
            break;
        }
        name.push(char::from_u32(unit as u32).unwrap_or('?'));
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 单次全屏观测快照（一次采集，供调和 / 会话确认 / 日志复用，避免重复 QUNS 读取）
#[derive(Clone, Debug)]
struct FullscreenObservation {
    quns: Option<windows::Win32::UI::Shell::QUERY_USER_NOTIFICATION_STATE>,
    quns_verdict: QunsVerdict,
    fg_root: SendHwnd,
    fg_credible: bool,
    fg_covers: bool,
    fg_maximized: bool,
    fg_iconic: bool,
    fg_cloaked: bool,
    display_id: Option<String>,
}

impl FullscreenObservation {
    fn quns_fullscreen(&self) -> bool {
        self.quns_verdict == QunsVerdict::Fullscreen
    }
}

/// 采集一次全屏观测（QUNS 只读一次）
fn observe_fullscreen() -> FullscreenObservation {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic, IsZoomed};
    let foreground = unsafe { GetForegroundWindow() };
    let root = root_hwnd(foreground);

    let quns = query_user_notification_state();
    let quns_verdict = quns.map(quns_verdict).unwrap_or(QunsVerdict::Fallback);

    if root.is_invalid() {
        return FullscreenObservation {
            quns,
            quns_verdict,
            fg_root: SendHwnd(root),
            fg_credible: false,
            fg_covers: false,
            fg_maximized: false,
            fg_iconic: false,
            fg_cloaked: false,
            display_id: None,
        };
    }

    let fg_iconic = unsafe { IsIconic(root) }.as_bool();
    let fg_cloaked = is_window_cloaked(root);
    let fg_credible = foreground_window_is_credible(root);

    let (fg_covers, fg_maximized, display_id) = if fg_credible {
        let (covers, covers_95) = window_coverage(root);
        let maximized = covers || covers_95 || unsafe { IsZoomed(root) }.as_bool();
        (covers, maximized, display_id_for_window(root))
    } else {
        (false, false, None)
    };

    FullscreenObservation {
        quns,
        quns_verdict,
        fg_root: SendHwnd(root),
        fg_credible,
        fg_covers,
        fg_maximized,
        fg_iconic,
        fg_cloaked,
        display_id,
    }
}

/// 纯解析输入：前台窗口的几何/可信度
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObservedForeground {
    root: SendHwnd,
    credible: bool,
    covers: bool,
    maximized: bool,
}

/// 纯解析输入：当前会话的健康状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionProbe {
    hwnd: SendHwnd,
    level: FullscreenLevel,
    alive: bool,
    covers: bool,
    cloaked: bool,
}

/// 解析结果：有效级别、锚定窗口、是否切换会话身份
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resolution {
    level: FullscreenLevel,
    anchor: Option<SendHwnd>,
    switch_to: Option<SendHwnd>,
}

/// 会话优先的全屏级别解析（纯函数，便于单测）
///
/// 原则：**QUNS 负责“触发/维持”，矩形负责“身份”，会话负责“事实”**。
/// - 活跃会话（存活、未 Cloak、且窗口仍覆盖或 QUNS 仍全屏）优先沿用会话级别；
///   前台若只是不覆盖的覆盖层（任务管理器等），不得改写会话身份；
/// - 前台可信且 100% 覆盖的新窗口 → 新全屏会话（切换应用时仅更新身份）；
/// - 前台可信且最大化 → `Maximized`；
/// - 其余 → `None`。QUNS 单独存在不会创建新会话，避免把覆盖层当成全屏身份。
fn resolve_fullscreen(
    session: Option<SessionProbe>,
    quns_fullscreen: bool,
    fg: ObservedForeground,
) -> Resolution {
    if let Some(current) = session {
        let active = current.alive && !current.cloaked && (current.covers || quns_fullscreen);
        if active {
            if fg.credible && fg.covers && fg.root.0 != current.hwnd.0 {
                // 新的 100% 覆盖前台窗口：切换全屏应用，仅更新会话身份
                return Resolution {
                    level: FullscreenLevel::TrueFullscreen,
                    anchor: Some(fg.root),
                    switch_to: Some(fg.root),
                };
            }
            return Resolution {
                level: current.level,
                anchor: Some(current.hwnd),
                switch_to: None,
            };
        }
    }

    if fg.credible && fg.covers {
        Resolution {
            level: FullscreenLevel::TrueFullscreen,
            anchor: Some(fg.root),
            switch_to: None,
        }
    } else if fg.credible && fg.maximized {
        Resolution {
            level: FullscreenLevel::Maximized,
            anchor: Some(fg.root),
            switch_to: None,
        }
    } else {
        Resolution {
            level: FullscreenLevel::None,
            anchor: None,
            switch_to: None,
        }
    }
}

/// 记录最近一个全屏窗口的句柄及其处置级别（供退出分支区分临时覆盖层）
fn update_last_fullscreen_at(hwnd: SendHwnd, level: FullscreenLevel) {
    if let Ok(mut g) = LAST_FULLSCREEN.lock() {
        *g = LastFullscreen {
            hwnd: Some(hwnd),
            level,
        };
    }
}

/// 窗口根 HWND（`GetAncestor(GA_ROOT)`；无效/拿不到时退回原句柄）
fn root_hwnd(hwnd: windows::Win32::Foundation::HWND) -> windows::Win32::Foundation::HWND {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOT};
    if hwnd.is_invalid() {
        return hwnd;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() {
        hwnd
    } else {
        root
    }
}

/// 当前前台窗口的根 HWND
fn foreground_root() -> SendHwnd {
    let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    SendHwnd(root_hwnd(fg))
}

/// 会话 HWND 是否与给定窗口一致
fn session_hwnd_matches(hwnd: SendHwnd) -> bool {
    FULLSCREEN_SESSION
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.hwnd))
        .is_some_and(|session_hwnd| session_hwnd.0 == hwnd.0)
}

/// 登记全屏会话（覆盖旧会话；调用方负责递增确认意图代数）
fn begin_session(hwnd: SendHwnd, level: FullscreenLevel, display_id: Option<String>) {
    let next = FullscreenSession {
        hwnd,
        level,
        display_id,
    };
    tracing::debug!(
        session = ?next,
        "全屏会话登记/更新"
    );
    *FULLSCREEN_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(next);
}

/// 结束全屏会话：取消待执行处置并清空会话
fn end_session(reason: &'static str) {
    cancel_pending_action();
    let ended = FULLSCREEN_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(session) = ended {
        tracing::info!(
            reason,
            session = ?session,
            "全屏会话结束"
        );
    }
}

/// 取消待执行的处置确认意图（递增代数使确认线程判定过期）
fn cancel_pending_action() {
    PENDING_ACTION_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// 处置确认代数的最终校验（纯函数，便于单测）
///
/// 确认线程在取得 engine 锁之后、执行暂停/终止之前再校验一次：若等待锁期间
/// 会话被销毁/隐藏（代数递增），则本次处置已过期，必须放弃。
fn generation_still_valid(expected: u64, current: u64) -> bool {
    expected == current
}

/// 会话身份快照（确认复核只需要 HWND + 级别，避免整份会话（含 display_id）被搬移）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionIdentity {
    hwnd: SendHwnd,
    level: FullscreenLevel,
}

/// 会话确认快照（纯函数判定入参，避免过多位置参数）
struct ConfirmCheck {
    gen_at_schedule: u64,
    current_gen: u64,
    session_now: Option<SessionIdentity>,
    expected_hwnd: SendHwnd,
    expected_level: FullscreenLevel,
    window_alive: bool,
    level_now: FullscreenLevel,
    previous_fullscreen_still_active: bool,
    /// 会话窗口是否被 DWM Cloak（UWP 挂起时不可见）
    cloaked: bool,
    /// QUNS 当前是否明确为全屏（2/3/4）
    quns_fullscreen: bool,
}

/// 会话确认是否仍应执行（纯函数，便于单测）
///
/// 条件：意图代数未变 + 会话窗口仍健康且未 Cloak + 会话未被替换 + 级别未变
///（或原全屏窗口仍覆盖显示器 / QUNS 仍明确为全屏）。
fn confirm_action_still_valid(check: &ConfirmCheck) -> bool {
    check.gen_at_schedule == check.current_gen
        && check.window_alive
        && !check.cloaked
        && check
            .session_now
            .as_ref()
            .is_some_and(|s| s.hwnd.0 == check.expected_hwnd.0 && s.level == check.expected_level)
        && (check.level_now == check.expected_level
            || (check.expected_level == FullscreenLevel::TrueFullscreen
                && (check.previous_fullscreen_still_active || check.quns_fullscreen)))
}

/// 会话窗口是否仍健康（存在 / 可见 / 非最小化）
fn session_window_alive(hwnd: SendHwnd) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow, IsWindowVisible};
    unsafe {
        IsWindow(hwnd.0).as_bool()
            && IsWindowVisible(hwnd.0).as_bool()
            && !IsIconic(hwnd.0).as_bool()
    }
}

/// 登记处置确认意图：等待 `CONFIRM_DELAY` 后复核执行暂停/终止（独立线程）
///
/// 真正的取消由窗口生命周期事件完成（见 [`handle_win_event`]）；线程醒来后仍按
/// [`confirm_action_still_valid`] 复核，避免在"进入后立刻退出"的瞬态窗口上误处置。
fn schedule_confirm_action(action: ConfirmAction) {
    let Some(session) = FULLSCREEN_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        return;
    };
    let gen_at_schedule = PENDING_ACTION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let expected_hwnd = session.hwnd;
    let expected_level = session.level;

    let spawn_result = std::thread::Builder::new()
        .name("mirrorstar-fullscreen-confirm".to_string())
        .spawn(move || {
            let started = std::time::Instant::now();
            std::thread::sleep(CONFIRM_DELAY);

            let current_gen = PENDING_ACTION_GENERATION.load(Ordering::SeqCst);
            let session_now = FULLSCREEN_SESSION
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            // 与会话优先解析共用同一观测：覆盖层不会把级别拉回前台窗口
            let observation = observe_fullscreen();
            let session_probe = session_now.as_ref().map(|current| SessionProbe {
                hwnd: current.hwnd,
                level: current.level,
                alive: session_window_alive(current.hwnd),
                covers: is_window_covering_monitor(current.hwnd.0),
                cloaked: is_window_cloaked(current.hwnd.0),
            });
            let resolution = resolve_fullscreen(
                session_probe,
                observation.quns_fullscreen(),
                ObservedForeground {
                    root: observation.fg_root,
                    credible: observation.fg_credible,
                    covers: observation.fg_covers,
                    maximized: observation.fg_maximized,
                },
            );
            let level_now = resolution.level;
            let previous_still_active = is_previous_fullscreen_window_still_active();
            let window_alive = session_window_alive(expected_hwnd);
            let cloaked = is_window_cloaked(expected_hwnd.0);
            let quns_fullscreen = observation.quns_fullscreen();

            let check = ConfirmCheck {
                gen_at_schedule,
                current_gen,
                session_now: session_now.as_ref().map(|current| SessionIdentity {
                    hwnd: current.hwnd,
                    level: current.level,
                }),
                expected_hwnd,
                expected_level,
                window_alive,
                level_now,
                previous_fullscreen_still_active: previous_still_active,
                cloaked,
                quns_fullscreen,
            };
            if !confirm_action_still_valid(&check) {
                tracing::info!(
                    gen_at_schedule,
                    current_gen,
                    level = ?level_now,
                    window_alive,
                    previous_still_active,
                    cloaked,
                    quns_fullscreen,
                    "会话确认被取消（已退出全屏 / 会话结束 / 意图过期 / Cloak），壁纸保持播放"
                );
                return;
            }

            let confirmed_after_ms = started.elapsed().as_millis() as u64;
            let expected_generation = current_gen;
            match action {
                ConfirmAction::Pause => {
                    let Some(failed) = pause_all_fast_blocking(PauseReason::FULLSCREEN, || {
                        generation_still_valid(
                            expected_generation,
                            PENDING_ACTION_GENERATION.load(Ordering::SeqCst),
                        )
                    }) else {
                        tracing::warn!("会话确认暂停未能执行（SHARED_ENGINE 缺失/守卫命中/锁等待期间被取消）");
                        return;
                    };
                    if failed.is_empty() {
                        tracing::info!(confirmed_after_ms, "会话确认通过：暂停壁纸（进程驻留）");
                    } else {
                        tracing::warn!(
                            failed_count = failed.len(),
                            failed = ?failed,
                            "会话确认暂停壁纸部分失败"
                        );
                    }
                }
                ConfirmAction::Terminate => {
                    let Some(failed) = terminate_all_fast_blocking(PauseReason::FULLSCREEN, || {
                        generation_still_valid(
                            expected_generation,
                            PENDING_ACTION_GENERATION.load(Ordering::SeqCst),
                        )
                    }) else {
                        tracing::warn!(
                            "会话确认终止未能执行（SHARED_ENGINE 缺失/守卫命中/锁等待期间被取消），保留 FULLSCREEN_WAS 待复查"
                        );
                        return;
                    };
                    record_terminate_time();
                    if failed.is_empty() {
                        tracing::info!(confirmed_after_ms, "会话确认通过：终止壁纸释放内存");
                        increment_termination_generation();
                    } else {
                        tracing::warn!(
                            failed_count = failed.len(),
                            failed = ?failed,
                            "会话确认终止壁纸部分失败，保留 FULLSCREEN_WAS 待复查"
                        );
                    }
                }
            }
        });
    if let Err(e) = spawn_result {
        tracing::error!(error = %e, "启动全屏会话确认线程失败");
    }
}

/// 对象事件的节流间隔（窗口拖动/缩放会高频产生 LOCATIONCHANGE）
const WINDOW_STATE_THROTTLE_MS: u64 = 100;

/// 节流时钟基准（进程级单调）
static WINDOW_STATE_EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// 节流判定（纯函数，便于单测）：首次放行，之后距上次 ≥ 间隔才放行
fn window_state_throttle_ok(last_ms: u64, now_ms: u64, interval_ms: u64) -> bool {
    last_ms == 0 || now_ms.saturating_sub(last_ms) >= interval_ms
}

/// 生产包装：对象事件是否允许本次调和（允许时刷新时间戳）
fn window_state_reconcile_allowed() -> bool {
    let now_ms = WINDOW_STATE_EPOCH.elapsed().as_millis() as u64;
    let last_ms = LAST_WINDOW_STATE_RECONCILE_MS.load(Ordering::SeqCst);
    if window_state_throttle_ok(last_ms, now_ms, WINDOW_STATE_THROTTLE_MS) {
        LAST_WINDOW_STATE_RECONCILE_MS.store(now_ms, Ordering::SeqCst);
        true
    } else {
        false
    }
}

/// 级别感知的恢复拦截决策（纯函数，便于单元测试；Maximized/None→false，TrueFullscreen→true）
///
/// 最大化级别的暂停：`IsZoomed` 对仍在后台保持最大化的窗口恒为 true，若不区分级别，
/// 会在用户 Alt-Tab/点桌面离开该窗口时仍判定"覆盖桌面"，导致壁纸卡死暂停无法恢复。
/// 故 Maximized 级别不拦截恢复（前台级别转为 None 即恢复）；仅 TrueFullscreen 保留
/// "后台窗口仍覆盖/仍最大化即拦截恢复"的临时覆盖层语义（区分任务管理器/Alt-Tab 覆盖层）。
fn should_intercept_resume(recorded_level: FullscreenLevel) -> bool {
    match recorded_level {
        FullscreenLevel::Maximized | FullscreenLevel::None => false,
        FullscreenLevel::TrueFullscreen => true,
    }
}

/// 终止冷却决策（纯函数，Task 3.4）：距上次终止未达冷却时长 → 不允许再次终止
fn should_allow_terminate_after(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    cooldown: std::time::Duration,
) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= cooldown,
    }
}

/// 生产包装：是否允许本次终止
fn should_allow_terminate() -> bool {
    let now = std::time::Instant::now();
    let last = LAST_TERMINATE_TS.lock().ok().and_then(|g| *g);
    should_allow_terminate_after(last, now, TERMINATE_COOLDOWN)
}

/// 记录本次终止时间戳（终止处置执行时调用）
fn record_terminate_time() {
    if let Ok(mut g) = LAST_TERMINATE_TS.lock() {
        *g = Some(std::time::Instant::now());
    }
}

/// 递增终止代数并返回新值（仅在**成功终止**且置 FULLSCREEN_WAS=true 时调用）
///
/// `fetch_add(1) + 1` 保证单调递增（即使 fetch_add 返回旧值溢出，新值仍大于旧值）。
/// 递增后，任何在递增之前 spawn 的恢复线程都会因代数变化而判定"过期"，放弃本次恢复，
/// 从而防止旧恢复覆盖新的终止状态。
fn increment_termination_generation() -> u64 {
    TERMINATION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// 读取当前终止代数（供恢复线程 spawn 前捕获，用于比对恢复是否过期）
fn current_termination_generation() -> u64 {
    TERMINATION_GENERATION.load(Ordering::SeqCst)
}

/// 判定一次"恢复"能否继续（纯函数，供恢复线程与测试复用）
///
/// 仅当恢复线程 spawn 时捕获的代数 `spawned_at_gen` 与当前代数 `current_gen` 相同，
/// 才允许本次恢复继续（写 FULLSCREEN_WAS=false / 重建主窗口）。一旦期间发生了新的
/// 成功终止（代数已递增），本次恢复已"过期"，应放弃，避免壁纸/主窗口覆盖到新全屏之上。
fn resume_generation_can_proceed(spawned_at_gen: u64, current_gen: u64) -> bool {
    spawned_at_gen == current_gen
}

/// 判断最近记录的全屏窗口是否仍覆盖其所在显示器（仅 TrueFullscreen 级别拦截）
///
/// 用于区分"临时覆盖层"（任务管理器/Alt-Tab，原全屏窗口仍存在并覆盖显示器）与
/// "真正退出全屏"（原全屏窗口已销毁/最小化/不再覆盖显示器）。
/// 事件回调与周期复查线程都会调用，只读取 `LAST_FULLSCREEN` 的值，不 take 消费。
fn is_previous_fullscreen_window_still_active() -> bool {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    };

    let last = match LAST_FULLSCREEN.lock().ok() {
        Some(g) => *g,
        None => return false,
    };
    let hwnd = match last.hwnd {
        Some(SendHwnd(h)) => h,
        None => return false,
    };

    // 级别感知：仅当处置级别为 TrueFullscreen 时才做覆盖层拦截（见 should_intercept_resume）
    if !should_intercept_resume(last.level) {
        return false;
    }

    unsafe {
        // 原全屏窗口已销毁 → 真正退出全屏
        if !IsWindow(hwnd).as_bool() {
            return false;
        }
        // 原全屏窗口不可见（已最小化等）→ 不再覆盖显示器
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
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

/// 清除记录的全屏窗口句柄与级别（真正退出全屏时调用，避免句柄悬空）
fn clear_last_fullscreen_hwnd() {
    if let Ok(mut g) = LAST_FULLSCREEN.lock() {
        g.hwnd = None;
        g.level = FullscreenLevel::None;
    }
}

/// 退出全屏后的壁纸恢复统一入口（Task 7.2：异步后台执行）
///
/// C-008：仅当 resume_all_fast 全部成功时才更新 FULLSCREEN_WAS 并清理
/// LAST_FULLSCREEN。原"重建主窗口"逻辑已随 Task 10 移除（不再有 Tauri 主窗口）。
/// 供前台事件回调退出分支与周期复查线程共用，保证两处逻辑一致。
///
/// # 异步化设计（Task 7.2，修复"退出游戏后壁纸黑屏不恢复"根因）
///
/// 原实现在此同步调用 `try_resume_all_fast`。真全屏终止后恢复需 `play()` 冷启动
/// mpv（IPC 连接 + 窗口查找，2-6s），同步执行会长时间阻塞 Win32 回调线程
/// （`win_event_callback` 运行于 `DispatchMessageW` 消息循环上下文），
/// 期间无法处理新的前台切换事件 → 用户感知壁纸卡死 / 黑屏不恢复。
///
/// 现改为 spawn 专用后台线程 `mirrorstar-fullscreen-resume` 执行
/// `resume_all_fast_blocking`（阻塞式获取 engine 锁，不因锁忙偶发跳过）：
/// - 成功 → 清 `FULLSCREEN_WAS` / `LAST_FULLSCREEN`
/// - 失败 → 保留 `FULLSCREEN_WAS=true`，由周期复查线程（2s 间隔）自动重试
/// - 通过 `RESUME_IN_PROGRESS` 标志去重，防止事件回调与复查线程并发重复恢复
///
/// # 终止代数比对（Task 2：防恢复覆盖更新终止）
///
/// Terminate T1 后恢复线程慢跑期间，用户可能进入新真全屏 T2（再次 Terminate）。
/// 恢复线程在 spawn **之前**捕获当时的终止代数 `gen_at_spawn`；线程内恢复成功
/// （failed 为空）后，**先比对代数**：若 `current_termination_generation() !=
/// gen_at_spawn`，说明期间又发生了新的成功终止，本次恢复已"过期"，应放弃——
/// 保持 `FULLSCREEN_WAS=true`、不恢复壁纸，仅打 warn 日志，防止过期恢复把壁纸
/// 恢复到 T2 全屏游戏之上。
fn resume_from_fullscreen_exit() {
    // 权威退出信号直接恢复，无冷却等待（冷却期已由会话生命周期取消语义取代）。
    // 去重：已有后台恢复线程在进行中则跳过（事件回调退出分支与周期复查线程
    // 可能并发触发）。swap(true) 原子地取得"执行权"：
    // - 返回 true → 已有线程在跑，本次调用直接返回
    // - 返回 false → 本次调用获得执行权，负责 spawn 并在线程结束时复位 false
    if RESUME_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        tracing::debug!("壁纸恢复已在后台进行中，跳过本次触发");
        return;
    }

    // spawn 后台恢复线程之前捕获当前终止代数，move 进闭包用于比对恢复是否过期
    let gen_at_spawn = current_termination_generation();

    // spawn 后台线程执行恢复，避免阻塞 Win32 回调线程（Task 7.2）
    let spawn_result = std::thread::Builder::new()
        .name("mirrorstar-fullscreen-resume".to_string())
        .spawn(move || {
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
                // 终止代数比对：若恢复线程慢跑期间又有新的成功终止发生，则本次
                // 恢复已"过期"，放弃——保持 FULLSCREEN_WAS=true、不 restore 主窗口，
                // 防止过期恢复覆盖后续新终止的全屏状态（Task 2）。
                if !resume_generation_can_proceed(gen_at_spawn, current_termination_generation()) {
                    tracing::warn!(
                        spawned_at_gen = gen_at_spawn,
                        current_gen = current_termination_generation(),
                        "恢复线程慢跑期间发生新的全屏终止，放弃本次过期恢复（保持 FULLSCREEN_WAS=true）"
                    );
                    RESUME_IN_PROGRESS.store(false, Ordering::Release);
                    return;
                }
                // C-008：仅当全部成功且代数未变化才更新 FULLSCREEN_WAS
                FULLSCREEN_WAS.store(false, Ordering::Release);
                clear_last_fullscreen_hwnd();
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
///
/// TODO(Task 9.4)：主窗口 UI 迁到原生子进程后，原生 UI 窗口标题必须同样保持
/// "镜星壁纸"（并遵守 `SWP_NOACTIVATE`），否则其成为前台窗口时会被当作"全屏应用"
/// 而暂停壁纸。该行为依赖 Task 3 产出的 `mirrorstar-ui.exe`，需回归验证。
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

/// 判断窗口标题/类名是否为自家壁纸渲染器窗口标识（纯函数，字符串兜底）
///
/// 标题 `MirrorStarVideo`（`OWN_WALLPAPER_TITLE`）**且**类名 `mpv`（`OWN_WALLPAPER_CLASS`）
/// **同时命中**才判为自家壁纸窗口。作为 `SetPropW` 窗口属性标记（主识别，与窗口同生共死）
/// 之外的纯字符串兜底：当标记因窗口在 `set_hwnd` 打标前即被前台检测命中时，仍能靠此识别。
///
/// 采用 AND 而非 OR：`mpv` 是通用播放器类名、`MirrorStarVideo` 是普通字符串，任一单一条件
/// 命中都可能与第三方窗口撞车（第三方 mpv 播放器全屏时类名同为 `mpv`）。双条件同时命中才
/// 豁免可避免误豁免第三方窗口（其全屏本应触发壁纸暂停）。自家 mpv 窗口标题与类名均由本应用
/// 启动参数固定（`--title=MirrorStarVideo`、类名 `mpv`），双条件恒满足。
/// 返回 true 时全屏检测应返回 `FullscreenLevel::None`（不做任何处置）。
fn is_own_wallpaper_window(title: &str, class: &str) -> bool {
    title == OWN_WALLPAPER_TITLE && class == OWN_WALLPAPER_CLASS
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

/// 判断窗口矩形宽高是否均 ≥ 显示器矩形的 95%（全屏判定）
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

    // ── should_intercept_resume: 级别感知的恢复拦截决策 ──────────────────────

    #[test]
    fn should_intercept_resume_true_fullscreen_intercepts() {
        // TrueFullscreen：保留"后台窗口仍覆盖/仍最大化即拦截恢复"的临时覆盖层语义
        assert!(should_intercept_resume(FullscreenLevel::TrueFullscreen));
    }

    #[test]
    fn should_intercept_resume_maximized_does_not_intercept() {
        // 修复"壁纸莫名卡在暂停"：Maximized 级别不拦截恢复（前台级别转 None 即恢复），
        // 避免 IsZoomed 对后台保持最大化的窗口恒为 true 导致恢复被永久跳过
        assert!(!should_intercept_resume(FullscreenLevel::Maximized));
    }

    #[test]
    fn should_intercept_resume_none_does_not_intercept() {
        // None / 无记录：不拦截恢复
        assert!(!should_intercept_resume(FullscreenLevel::None));
    }

    // ── LAST_FULLSCREEN: 记录句柄 + 级别的存储与读取一致性 ────────────────────

    #[test]
    fn test_last_fullscreen_records_hwnd_and_level() {
        // 验证 update_last_fullscreen_at 同时保存句柄与级别，读取一致
        let fake = SendHwnd(windows::Win32::Foundation::HWND(
            1 as *mut core::ffi::c_void,
        ));
        update_last_fullscreen_at(fake, FullscreenLevel::Maximized);
        {
            let g = LAST_FULLSCREEN.lock().unwrap();
            match g.hwnd {
                Some(SendHwnd(h)) => assert_eq!(h, fake.0),
                None => panic!("应记录全屏窗口句柄"),
            }
            assert_eq!(g.level, FullscreenLevel::Maximized);
        }
    }

    #[test]
    fn test_last_fullscreen_overwrites_level_on_reupdate() {
        // 再次记录（如降级 Maximized）会覆盖之前的级别，不残留旧值
        let fake = SendHwnd(windows::Win32::Foundation::HWND(
            1 as *mut core::ffi::c_void,
        ));
        update_last_fullscreen_at(fake, FullscreenLevel::TrueFullscreen);
        update_last_fullscreen_at(fake, FullscreenLevel::Maximized);
        {
            let g = LAST_FULLSCREEN.lock().unwrap();
            assert_eq!(g.level, FullscreenLevel::Maximized);
        }
    }

    #[test]
    fn test_last_fullscreen_clear_resets_hwnd_and_level() {
        // clear_last_fullscreen_hwnd 同时清理句柄与级别
        let fake = SendHwnd(windows::Win32::Foundation::HWND(
            1 as *mut core::ffi::c_void,
        ));
        update_last_fullscreen_at(fake, FullscreenLevel::TrueFullscreen);
        clear_last_fullscreen_hwnd();
        let g = LAST_FULLSCREEN.lock().unwrap();
        assert!(g.hwnd.is_none());
        assert_eq!(g.level, FullscreenLevel::None);
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
            compute_transition(
                FullscreenLevel::None,
                FullscreenLevel::None,
                false,
                FullscreenAction::None
            ),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(
                FullscreenLevel::None,
                FullscreenLevel::Maximized,
                false,
                FullscreenAction::Terminate
            ),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_none_was_true_exit() {
        // 之前已处置且当前非全屏 → 退出恢复
        assert_eq!(
            compute_transition(
                FullscreenLevel::None,
                FullscreenLevel::TrueFullscreen,
                true,
                FullscreenAction::Terminate
            ),
            Transition::Exit
        );
        assert_eq!(
            compute_transition(
                FullscreenLevel::None,
                FullscreenLevel::Maximized,
                true,
                FullscreenAction::Pause
            ),
            Transition::Exit
        );
    }

    #[test]
    fn test_compute_transition_maximized_same_level_noop() {
        // 同级别（Maximized→Maximized）：仅更新 HWND
        assert_eq!(
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::Maximized,
                true,
                FullscreenAction::Terminate
            ),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::Maximized,
                false,
                FullscreenAction::Pause
            ),
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
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::None,
                false,
                FullscreenAction::Terminate
            ),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_maximized_pause_with_pause_action() {
        assert_eq!(
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::None,
                false,
                FullscreenAction::Pause
            ),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_maximized_noop_with_none_action() {
        assert_eq!(
            compute_transition(
                FullscreenLevel::Maximized,
                FullscreenLevel::None,
                false,
                FullscreenAction::None
            ),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_same_level_noop() {
        // 同级别（TrueFullscreen→TrueFullscreen）：仅更新 HWND
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::TrueFullscreen,
                true,
                FullscreenAction::Terminate
            ),
            Transition::NoOp
        );
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::TrueFullscreen,
                false,
                FullscreenAction::Pause
            ),
            Transition::NoOp
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_terminate() {
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::None,
                false,
                FullscreenAction::Terminate
            ),
            Transition::Terminate
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_upgrade_terminates() {
        // 升级 Maximized→TrueFullscreen：终止
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::Maximized,
                true,
                FullscreenAction::Terminate
            ),
            Transition::Terminate
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_pause() {
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::None,
                false,
                FullscreenAction::Pause
            ),
            Transition::Pause
        );
    }

    #[test]
    fn test_compute_transition_true_fullscreen_noop_with_none_action() {
        assert_eq!(
            compute_transition(
                FullscreenLevel::TrueFullscreen,
                FullscreenLevel::None,
                false,
                FullscreenAction::None
            ),
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
        assert!(flag.load(Ordering::Acquire), "去重期间标志应保持 true");

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

    // ── is_own_wallpaper_window: 自家壁纸渲染器窗口识别（Task 1.2） ────────

    #[test]
    fn test_is_own_wallpaper_window_own_window() {
        // 自家 mpv 窗口：标题 MirrorStarVideo + 类名 mpv 同时命中 → true
        assert!(is_own_wallpaper_window("MirrorStarVideo", "mpv"));
    }

    #[test]
    fn test_is_own_wallpaper_window_own_partial_match_not_own() {
        // 仅标题命中（类名不同）→ 不豁免（AND 语义，避免误豁免第三方）
        assert!(!is_own_wallpaper_window("MirrorStarVideo", "SomeClass"));
        // 仅类名命中（标题不同/为空）→ 不豁免（第三方 mpv 播放器类名同为 mpv，不能豁免）
        assert!(!is_own_wallpaper_window("", "mpv"));
        assert!(!is_own_wallpaper_window("Third Party Title", "mpv"));
    }

    #[test]
    fn test_is_own_wallpaper_window_third_party() {
        // 第三方应用无关标题且无关类名 → 不豁免
        assert!(!is_own_wallpaper_window("Third Party App", "NotMpv"));
        assert!(!is_own_wallpaper_window("Some Other Window", "OtherClass"));
        assert!(!is_own_wallpaper_window("", ""));
        // 第三方窗口标题恰为 MirrorStarVideo 但非 mpv 类名 → 不豁免
        assert!(!is_own_wallpaper_window("MirrorStarVideo", "NotMpv"));
    }

    // ── 终止代数（Task 2）：increment / current 单调递增与读取 ────────────────────
    //
    // `TERMINATION_GENERATION` 是进程级全局静态量，测试通过"读到旧值→递增→断言新值
    // 更大"的相对方式验证单调递增，不依赖其具体数值（避免与其他测试的执行次序耦合）。
    // 递增操作只在本测试中发生，不影响其他测试（其余测试不读取代数）。

    #[test]
    fn test_termination_generation_increments_monotonically() {
        // 连续两次递增：每次返回的新值都应严格大于前一次
        let before = current_termination_generation();
        let g1 = increment_termination_generation();
        assert!(g1 > before, "第一次递增后新值应大于旧值");
        let g2 = increment_termination_generation();
        assert!(g2 > g1, "第二次递增后新值应大于第一次");
    }

    #[test]
    fn test_termination_generation_current_reads_latest() {
        // 递增后 current 应读取到返回的最新值
        let latest = increment_termination_generation();
        assert_eq!(
            current_termination_generation(),
            latest,
            "current_termination_generation 应返回最近一次递增后的值"
        );
    }

    #[test]
    fn test_resume_generation_can_proceed_same_gen_allows() {
        // 恢复线程 spawn 捕获的代数与当前相同 → 允许恢复继续
        assert!(resume_generation_can_proceed(5, 5));
        assert!(resume_generation_can_proceed(0, 0));
    }

    #[test]
    fn test_resume_generation_can_proceed_stale_gen_blocked() {
        // 期间发生了新的成功终止（当前代数已递增）→ 本次恢复过期，应放弃
        assert!(!resume_generation_can_proceed(3, 4));
        assert!(!resume_generation_can_proceed(0, 100));
        assert!(!resume_generation_can_proceed(u64::MAX - 1, u64::MAX));
    }

    // ── 终止防抖纯函数：should_allow_terminate_after ──────────────────────────

    #[test]
    fn test_should_allow_terminate_after_no_last_allows() {
        // 无上次终止记录 → 允许终止
        let now = std::time::Instant::now();
        assert!(should_allow_terminate_after(
            None,
            now,
            std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn test_should_allow_terminate_after_within_cooldown_denies() {
        // 距上次终止 500ms（<1s）→ 不允许
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_millis(500);
        assert!(!should_allow_terminate_after(
            Some(last),
            now,
            std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn test_should_allow_terminate_after_past_cooldown_allows() {
        // 距上次终止 1100ms（>1s）→ 允许
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_millis(1100);
        assert!(should_allow_terminate_after(
            Some(last),
            now,
            std::time::Duration::from_secs(1)
        ));
    }

    // ── 全屏会话确认 / 对象事件节流（optimize-fullscreen-detection）──────────

    fn fake_hwnd(raw: isize) -> SendHwnd {
        SendHwnd(windows::Win32::Foundation::HWND(
            raw as *mut core::ffi::c_void,
        ))
    }

    /// 测试用便捷包装：按位置参数构造 `ConfirmCheck` 后调用生产纯函数
    #[allow(clippy::too_many_arguments)]
    fn confirm_action_still_valid(
        gen_at_schedule: u64,
        current_gen: u64,
        session_now: Option<FullscreenSession>,
        expected_hwnd: SendHwnd,
        expected_level: FullscreenLevel,
        window_alive: bool,
        level_now: FullscreenLevel,
        previous_fullscreen_still_active: bool,
    ) -> bool {
        super::confirm_action_still_valid(&ConfirmCheck {
            gen_at_schedule,
            current_gen,
            session_now: session_now.as_ref().map(|current| SessionIdentity {
                hwnd: current.hwnd,
                level: current.level,
            }),
            expected_hwnd,
            expected_level,
            window_alive,
            level_now,
            previous_fullscreen_still_active,
            cloaked: false,
            quns_fullscreen: false,
        })
    }

    /// 会话确认有效性：代数 / 窗口健康 / 会话身份 / 级别（或覆盖层）共同决定。
    #[test]
    fn test_confirm_action_still_valid_matrix() {
        let a = fake_hwnd(1);
        let b = fake_hwnd(2);
        let session_a = Some(FullscreenSession {
            hwnd: a,
            level: FullscreenLevel::TrueFullscreen,
            display_id: None,
        });

        // 代数一致 + 会话未变 + 窗口健康 + 仍真全屏 → 执行
        assert!(confirm_action_still_valid(
            7,
            7,
            session_a.clone(),
            a,
            FullscreenLevel::TrueFullscreen,
            true,
            FullscreenLevel::TrueFullscreen,
            false
        ));
        // 前台变为任务管理器（None），但原全屏窗口仍覆盖 → 执行（覆盖层）
        assert!(confirm_action_still_valid(
            7,
            7,
            session_a.clone(),
            a,
            FullscreenLevel::TrueFullscreen,
            true,
            FullscreenLevel::None,
            true
        ));
        // 代数已变化（被取消）→ 不执行
        assert!(!confirm_action_still_valid(
            7,
            8,
            session_a.clone(),
            a,
            FullscreenLevel::TrueFullscreen,
            true,
            FullscreenLevel::TrueFullscreen,
            false
        ));
        // 会话窗口已销毁/隐藏/最小化 → 不执行
        assert!(!confirm_action_still_valid(
            7,
            7,
            session_a.clone(),
            a,
            FullscreenLevel::TrueFullscreen,
            false,
            FullscreenLevel::TrueFullscreen,
            false
        ));
        // 会话已被新窗口替换 → 不执行
        assert!(!confirm_action_still_valid(
            7,
            7,
            Some(FullscreenSession {
                hwnd: b,
                level: FullscreenLevel::TrueFullscreen,
                display_id: None,
            }),
            a,
            FullscreenLevel::TrueFullscreen,
            true,
            FullscreenLevel::TrueFullscreen,
            false
        ));
        // 会话已清空（end_session）→ 不执行
        assert!(!confirm_action_still_valid(
            7,
            7,
            None,
            a,
            FullscreenLevel::TrueFullscreen,
            true,
            FullscreenLevel::TrueFullscreen,
            false
        ));
        // Maximized 会话：级别一致才执行；仅覆盖层不放行
        assert!(confirm_action_still_valid(
            7,
            7,
            Some(FullscreenSession {
                hwnd: a,
                level: FullscreenLevel::Maximized,
                display_id: None,
            }),
            a,
            FullscreenLevel::Maximized,
            true,
            FullscreenLevel::Maximized,
            false
        ));
        assert!(!confirm_action_still_valid(
            7,
            7,
            Some(FullscreenSession {
                hwnd: a,
                level: FullscreenLevel::Maximized,
                display_id: None,
            }),
            a,
            FullscreenLevel::Maximized,
            true,
            FullscreenLevel::None,
            true
        ));
    }

    /// 对象事件节流：首次放行；之后距上次 ≥ 间隔才放行；时钟回绕按拒绝处理。
    #[test]
    fn test_window_state_throttle_ok_matrix() {
        let interval = 100u64;
        // 首次（无记录）放行
        assert!(window_state_throttle_ok(0, 12345, interval));
        // 间隔不足 → 拒绝
        assert!(!window_state_throttle_ok(50, 120, interval));
        // 正好达到间隔 → 放行
        assert!(window_state_throttle_ok(20, 120, interval));
        // 时钟回绕（now < last）→ 拒绝
        assert!(!window_state_throttle_ok(200, 50, interval));
    }

    /// 取消意图 = 递增处置确认代数；确认线程据此放弃处置。
    #[test]
    fn test_cancel_pending_action_invalidates_intent() {
        let a = fake_hwnd(1);
        let session = Some(FullscreenSession {
            hwnd: a,
            level: FullscreenLevel::TrueFullscreen,
            display_id: None,
        });
        let gen_at_schedule = PENDING_ACTION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        cancel_pending_action();
        let current_gen = PENDING_ACTION_GENERATION.load(Ordering::SeqCst);
        assert!(
            !confirm_action_still_valid(
                gen_at_schedule,
                current_gen,
                session,
                a,
                FullscreenLevel::TrueFullscreen,
                true,
                FullscreenLevel::TrueFullscreen,
                false
            ),
            "取消后本次意图必须判定为过期，不得执行暂停/终止"
        );
    }

    /// 最终并发校验：等待 engine 锁期间代数变化 → 处置必须放弃。
    #[test]
    fn test_generation_still_valid_matrix() {
        assert!(generation_still_valid(7, 7), "代数未变应允许执行");
        assert!(
            !generation_still_valid(7, 8),
            "代数已变（会话被销毁/隐藏）应放弃执行"
        );
    }

    // ── QUNS / 探针 / Cloak（align-fullscreen-detection-with-mainstream）──────

    /// QUNS 映射：1→明确非全屏；2/3/4→全屏；5/6/7→回退矩形。
    #[test]
    fn test_quns_verdict_matrix() {
        use windows::Win32::UI::Shell::{
            QUNS_ACCEPTS_NOTIFICATIONS, QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT,
            QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN,
        };
        assert_eq!(quns_verdict(QUNS_NOT_PRESENT), QunsVerdict::NotFullscreen);
        assert_eq!(
            quns_verdict(QUNS_RUNNING_D3D_FULL_SCREEN),
            QunsVerdict::Fullscreen
        );
        assert_eq!(quns_verdict(QUNS_BUSY), QunsVerdict::Fullscreen);
        assert_eq!(
            quns_verdict(QUNS_PRESENTATION_MODE),
            QunsVerdict::Fullscreen
        );
        assert_eq!(
            quns_verdict(QUNS_ACCEPTS_NOTIFICATIONS),
            QunsVerdict::Fallback
        );
        assert_eq!(quns_verdict(QUNS_QUIET_TIME), QunsVerdict::Fallback);
        assert_eq!(quns_verdict(QUNS_APP), QunsVerdict::Fallback);
    }

    /// 探针快照：任一字段变化都应触发调和；完全相同则不触发。
    #[test]
    fn test_probe_snapshot_change_detection() {
        let base = ProbeSnapshot {
            quns: Some(5),
            foreground_root: 0x100,
            iconic: false,
            cloaked: false,
        };
        let same = base;
        assert_eq!(base, same, "相同快照不应触发调和");
        assert_ne!(
            base,
            ProbeSnapshot {
                quns: Some(3),
                ..base
            },
            "QUNS 变化应触发调和"
        );
        assert_ne!(
            base,
            ProbeSnapshot {
                foreground_root: 0x200,
                ..base
            },
            "前台根变化应触发调和"
        );
        assert_ne!(
            base,
            ProbeSnapshot {
                iconic: true,
                ..base
            },
            "最小化状态变化应触发调和"
        );
        assert_ne!(
            base,
            ProbeSnapshot {
                cloaked: true,
                ..base
            },
            "Cloak 状态变化应触发调和"
        );
    }

    /// 确认复核：会话被 Cloak 或 QUNS 已回落且原窗口不再覆盖 → 取消处置。
    #[test]
    fn test_confirm_cancelled_on_cloak_and_quns_exit() {
        let a = fake_hwnd(1);
        let session_now = Some(SessionIdentity {
            hwnd: a,
            level: FullscreenLevel::TrueFullscreen,
        });
        let base = ConfirmCheck {
            gen_at_schedule: 7,
            current_gen: 7,
            session_now,
            expected_hwnd: a,
            expected_level: FullscreenLevel::TrueFullscreen,
            window_alive: true,
            level_now: FullscreenLevel::TrueFullscreen,
            previous_fullscreen_still_active: false,
            cloaked: false,
            quns_fullscreen: false,
        };
        assert!(
            super::confirm_action_still_valid(&base),
            "基准条件应允许执行"
        );

        let cloaked = ConfirmCheck {
            cloaked: true,
            ..base
        };
        assert!(
            !super::confirm_action_still_valid(&cloaked),
            "会话被 Cloak 时必须取消"
        );

        let quns_exit = ConfirmCheck {
            level_now: FullscreenLevel::None,
            previous_fullscreen_still_active: false,
            quns_fullscreen: false,
            ..base
        };
        assert!(
            !super::confirm_action_still_valid(&quns_exit),
            "QUNS 回落且原窗口不再覆盖时必须取消"
        );

        let overlay = ConfirmCheck {
            level_now: FullscreenLevel::None,
            previous_fullscreen_still_active: false,
            quns_fullscreen: true,
            ..base
        };
        assert!(
            super::confirm_action_still_valid(&overlay),
            "QUNS 仍为全屏（覆盖层场景）时应继续执行"
        );
    }

    /// 会话优先解析：覆盖层不改写身份、会话最小化+QUNS 滞留不新建会话、新全屏窗口切换身份。
    #[test]
    fn test_resolve_fullscreen_matrix() {
        let a = fake_hwnd(1);
        let b = fake_hwnd(2);

        let session = SessionProbe {
            hwnd: a,
            level: FullscreenLevel::TrueFullscreen,
            alive: true,
            covers: true,
            cloaked: false,
        };
        // 覆盖层前台（不覆盖、可信）：沿用会话级别与身份
        let overlay = ObservedForeground {
            root: b,
            credible: true,
            covers: false,
            maximized: false,
        };
        assert_eq!(
            resolve_fullscreen(Some(session), false, overlay),
            Resolution {
                level: FullscreenLevel::TrueFullscreen,
                anchor: Some(a),
                switch_to: None,
            }
        );
        // 覆盖层 + QUNS 仍全屏：即使会话矩形测量失败也维持会话
        let no_covers = SessionProbe {
            covers: false,
            ..session
        };
        assert_eq!(
            resolve_fullscreen(Some(no_covers), true, overlay),
            Resolution {
                level: FullscreenLevel::TrueFullscreen,
                anchor: Some(a),
                switch_to: None,
            }
        );
        // 新的 100% 覆盖前台窗口：切换会话身份，级别仍为真全屏
        let new_fullscreen = ObservedForeground {
            root: b,
            credible: true,
            covers: true,
            maximized: true,
        };
        assert_eq!(
            resolve_fullscreen(Some(session), false, new_fullscreen),
            Resolution {
                level: FullscreenLevel::TrueFullscreen,
                anchor: Some(b),
                switch_to: Some(b),
            }
        );
        // 会话最小化（alive=false）+ QUNS 滞留 + 非覆盖前台 → 不新建会话
        let minimized = SessionProbe {
            alive: false,
            ..session
        };
        assert_eq!(
            resolve_fullscreen(Some(minimized), true, overlay),
            Resolution {
                level: FullscreenLevel::None,
                anchor: None,
                switch_to: None,
            }
        );
        // 会话被 Cloak → 不视为活跃；QUNS 单独存在也不新建
        let cloaked = SessionProbe {
            cloaked: true,
            ..session
        };
        assert_eq!(
            resolve_fullscreen(Some(cloaked), true, overlay),
            Resolution {
                level: FullscreenLevel::None,
                anchor: None,
                switch_to: None,
            }
        );
        // 无会话 + 前台 100% 覆盖 → 新建真全屏会话
        assert_eq!(
            resolve_fullscreen(None, false, new_fullscreen),
            Resolution {
                level: FullscreenLevel::TrueFullscreen,
                anchor: Some(b),
                switch_to: None,
            }
        );
        // 无会话 + 前台最大化 → Maximized
        let maximized = ObservedForeground {
            root: b,
            credible: true,
            covers: false,
            maximized: true,
        };
        assert_eq!(
            resolve_fullscreen(None, false, maximized),
            Resolution {
                level: FullscreenLevel::Maximized,
                anchor: Some(b),
                switch_to: None,
            }
        );
        // 无会话 + QUNS 单独存在 + 非覆盖前台 → None（不把覆盖层当全屏身份）
        assert_eq!(
            resolve_fullscreen(None, true, overlay),
            Resolution {
                level: FullscreenLevel::None,
                anchor: None,
                switch_to: None,
            }
        );
    }
}
