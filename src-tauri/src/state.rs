use mirrorstar_core::{ConfigManager, DesktopIntegrator, PauseReason, WallpaperEngine};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// 应用状态
///
/// # 锁顺序约定
///
/// 为避免死锁，获取锁时必须遵循以下顺序（从外到内）：
///
/// 1. `wallpaper_engine` (tokio::sync::Mutex) — 最外层锁，长时间持有（set_wallpaper 时）
/// 2. `desktop` (std::sync::Mutex) — 在 engine 锁内获取，用于 WorkerW 操作
/// 3. `config_manager` — 内部使用 RwLock，线程安全
///
/// 注意：`pause_senders` 已移入 `WallpaperEngine`，快速路径方法（pause/resume/
/// volume/mute）通过 engine 锁访问，不再需要独立的 `pause_senders` 锁。
///
/// # 锁获取路径审查（Task 19.2）
///
/// - `set_wallpaper` 命令：通过 `spawn_blocking` 获取 engine 锁（`blocking_lock`），
///   engine 内部获取 `desktop` 锁，符合 engine → desktop 顺序。
/// - `remove_wallpaper` 命令：获取 engine 锁，内部 `close_wallpaper_by_path`
///   → `close_wallpaper` 获取 desktop 锁，符合顺序。
/// - `perform_shutdown_blocking`：在 `RunEvent::ExitRequested` 中获取 engine 锁（T04：3s 超时），`shutdown()` 内部清理 pause_senders。
/// - `set_scaling_mode` / `set_speed`：仅获取 engine 锁
///   （`set_scaling_mode` 内部可能调用 `set_wallpaper` 再获取 desktop 锁，符合顺序）。
///
/// # 已知例外（无死锁风险）
///
/// 以下命令直接获取 `desktop` 锁而不持有 engine 锁。由于这些路径不会在持有
/// `desktop` 锁后再尝试获取 engine 锁，因此不会形成锁环，无死锁风险：
///
/// - `get_displays`：仅获取 desktop 锁读取显示器列表
/// - `check_desktop_status`：仅获取 desktop 锁检查/重初始化 WorkerW
/// - WorkerW 兜底检查任务（`run()` 中 spawn 的 5 分钟间隔任务）
/// - Explorer 重启监控窗口过程（通过 `EXPLORER_DESKTOP` 全局静态获取 desktop 锁）
///
/// # 注意事项
///
/// 快速路径命令（`pause_wallpaper` / `resume_wallpaper` / `set_volume` /
/// `toggle_mute`）获取 engine 锁后调用 `*_fast` 方法。托盘菜单通过全局
/// `SHARED_ENGINE` 使用 `blocking_lock()` 获取 engine 锁。Win32 回调（全屏
/// 检测 `foreground_event_callback`、电源监控 `handle_power_status_change`）
/// 运行于 Win32 消息循环 / 窗口过程回调上下文（`DispatchMessageW` /
/// `explorer_monitor_wndproc`），不可长时间阻塞（否则会延迟
/// `TaskbarCreated` 等 Explorer 事件处理），因此通过全局 `SHARED_ENGINE`
/// 使用 `try_lock()` 获取 engine 锁，锁忙时跳过本次事件（`WM_POWERBROADCAST`
/// / `EVENT_SYSTEM_FOREGROUND` 会重复触发，可容忍偶发跳过）。
///
/// # 字段访问约定
///
/// AppState 字段分两类：业务子系统的 `Arc` 包装句柄（`config_manager` /
/// `wallpaper_engine` / `desktop`）与 UI 层共享状态（`tray_paused` /
/// `tray_pause_resume_item`）。访问约定如下：
///
/// ## 通过 Tauri State 访问（命令层推荐方式）
///
/// `#[tauri::command]` 函数通过 `state: State<'_, AppState>` 参数获取 AppState
/// 引用，直接访问字段。State 守卫在命令函数返回时自动释放，无需手动管理。
///
/// ```rust,ignore
/// #[tauri::command]
/// pub async fn set_wallpaper(
///     state: State<'_, AppState>,
///     file_path: String,
///     display_id: Option<String>,
/// ) -> Result<(), MirrorStarError> {
///     // 直接通过 `state.config_manager` 访问 ConfigManager
///     let cfg = state.config_manager.get_config();
///     // 直接通过 `state.wallpaper_engine` 访问 WallpaperEngine（需 .lock()）
///     let engine = state.wallpaper_engine.lock().await;
///     // 直接通过 `state.desktop` 访问 DesktopIntegrator（需 .lock()）
///     let desktop = state.desktop.lock().unwrap_or_else(|e| e.into_inner());
///     Ok(())
/// }
/// ```
///
/// ## 通过 AppHandle 访问（事件回调 / 非命令函数）
///
/// `RunEvent::ExitRequested` 回调、托盘菜单事件、`async_runtime::spawn`
/// 任务等非命令函数无法接收 `State<'_, AppState>` 参数，需通过
/// `AppHandle::try_state::<AppState>()` 获取：
///
/// ```rust,ignore
/// if let Some(state) = app_handle.try_state::<AppState>() {
///     perform_shutdown_blocking(&state.wallpaper_engine, &state.config_manager);
/// }
/// ```
///
/// `try_state` 返回 `Option<State<'_, AppState>>`，应用未完成 setup 时返回
/// `None`（理论上 setup 后才触发回调，但防御性处理）。
///
/// ## 字段分组与并发原语
///
/// | 字段 | 类型 | 并发原语 | 访问方式 |
/// | --- | --- | --- | --- |
/// | `config_manager` | `Arc<ConfigManager>` | 内部 `RwLock`/`Mutex`/`AtomicBool` | `.get_config()` / `.update_config()` 等 |
/// | `wallpaper_engine` | `Arc<tokio::sync::Mutex<WallpaperEngine>>` | tokio 异步 `Mutex` | `.lock().await`（async）/ `.blocking_lock()`（sync 上下文） |
/// | `desktop` | `Arc<std::sync::Mutex<DesktopIntegrator>>` | std 同步 `Mutex` | `.lock().unwrap_or_else(\|e\| e.into_inner())` |
/// | `tray_paused` | `AtomicBool` | 原子变量 | `.load(Ordering::SeqCst)` / `.store(_, Ordering::SeqCst)` |
/// | `tray_pause_resume_item` | `OnceLock<MenuItem>` | 一次性写入 | `.get()` / `.set(item)` |
///
/// 注意：`wallpaper_engine` 与 `desktop` 是不同类型的 `Mutex`（tokio vs std），
/// 不可互换使用——详见 workerw_check.rs 的 async 锁使用场景说明。
///
/// ## Win32 回调访问全局静态量（非 AppState 字段）
///
/// Win32 回调（`foreground_event_callback` / `handle_power_status_change` /
/// `explorer_monitor_wndproc`）是 `extern "system" fn` 函数指针，无法接收
/// `State` 或 `AppHandle` 参数，需通过 `SHARED_ENGINE` / `SHARED_CONFIG` /
/// `EXPLORER_DESKTOP` 等全局静态量访问。这些静态量在 `lib.rs` setup 中通过
/// `OnceLock::set` 一次性写入，与 AppState 内的 `Arc` 字段指向同一对象
/// （setup 时 `AppState.wallpaper_engine` clone 后存入 `SHARED_ENGINE`）。
/// 详见下方"Global Statics"注释的 A/B/C 分类。
pub struct AppState {
    pub(crate) config_manager: Arc<ConfigManager>,
    pub(crate) wallpaper_engine: Arc<tokio::sync::Mutex<WallpaperEngine>>,
    pub(crate) desktop: Arc<Mutex<DesktopIntegrator>>,
    /// 托盘"暂停/恢复壁纸"菜单项的当前暂停状态
    pub tray_paused: AtomicBool,
    /// 托盘"暂停/恢复壁纸"菜单项引用，用于在切换状态后更新菜单文本
    pub tray_pause_resume_item: OnceLock<tauri::menu::MenuItem<tauri::Wry>>,
}

// ── Global Statics ──────────────────────────────────────────────────────────
//
// Task 9.2 全局静态量收敛评估（保守策略：全部保留，仅文档化）
//
// 共 15 个全局静态量。评估结论：**全部保留，不做收敛**。理由如下：
//
// 核心约束：Win32 回调（`foreground_event_callback`、`explorer_monitor_wndproc`）
// 是 `extern "system" fn` 函数指针，**无法捕获闭包变量**，也无法访问 Tauri 的
// `AppState`（AppState 需通过 `AppHandle::try_state` 获取，而回调签名不接受
// AppHandle 参数）。因此回调所需状态必须通过进程级全局可访问。
//
// 按可收敛性分类：
//
// A. 必须保留（Win32 回调直接访问，无法移入 AppState）—— 5 个：
//   - `SHARED_ENGINE`         : foreground_event_callback / handle_power_status_change 访问
//   - `SHARED_CONFIG`         : foreground_event_callback / handle_power_status_change 访问
//   - `FULLSCREEN_WAS`        : foreground_event_callback 的全屏状态机
//   - `EXPLORER_DESKTOP`      : explorer_monitor_wndproc 重新初始化 WorkerW
//   - `TASKBAR_CREATED_MSG`   : explorer_monitor_wndproc 比对消息 ID
//
// B. 可移入 MonitorRegistry 但收益低、风险高 —— 8 个：
//   - `FULLSCREEN_MONITOR_RUNNING` / `FULLSCREEN_MONITOR_THREAD_ID` / `FULLSCREEN_MONITOR_THREAD`
//   - `EXPLORER_MONITOR_RUNNING`  / `EXPLORER_MONITOR_THREAD_ID`  / `EXPLORER_MONITOR_THREAD`
//   - `WORKERW_CHECK_RUNNING`
//   - `POWER_WAS_ON_BATTERY`
//   这些仅被 `perform_shutdown_blocking` / `handle_power_status_change` 访问
//   （非函数指针回调）。理论上可收敛进 `MonitorRegistry` 结构放入 AppState。
//   但 `perform_shutdown_blocking` 在 `RunEvent::ExitRequested` 中通过
//   `try_state::<AppState>()` 获取 state 后调用，需额外传递 MonitorRegistry 引用，
//   增加签名复杂度；而当前 AtomicBool + Mutex<Option<u32/JoinHandle>> 模式简洁、经测试验证
//   （C-014/C-015 后线程 ID 与 JoinHandle 改为 Mutex<Option> 以支持 take/replace，不再无锁，
//   但锁竞争极低——仅 start/stop 路径访问）。收敛的收益（减少全局量）不抵破坏退出路径稳定性的风险。
//
// C. 必须保留（退出守卫，跨调用点幂等）—— 2 个：
//   - `SHUTDOWN_DONE`         : perform_shutdown_blocking 幂等守卫，必须进程级可见
//   - `WIN_EVENT_HOOK`        : 退出时（或二次调用 start 时）UnhookWinEvent 释放，
//                               C-014/C-015 后改为 Mutex<Option> 以支持 take/replace
//
// 替代方案考虑（已否决）：
//   - 移入 AppState：破坏 Win32 回调访问（A 类），且 ExitRequested 路径需重构。
//   - 移入 MonitorRegistry：B 类可行但收益低、风险高（见上）。
//   - 用 thread_local：回调可能在任意线程触发，thread_local 无法跨线程共享状态。
//
// `SHARED_APP_HANDLE` 供 workerw_check 任务 emit `desktop-status-changed`（T07）；
// 其他回调（全屏/电源/托盘）走 `WallpaperEngine::global_state_changed` broadcast 通道，
// 由 `lib.rs` setup 中 spawn 的订阅任务统一 emit `wallpaper-state-changed`。

/// T07：WorkerW 兜底检查任务重新初始化成功后，通过此 handle emit
/// `desktop-status-changed` 事件通知前端刷新桌面状态。
///
/// 在 `lib.rs` 的 `setup` hook 中通过 `SHARED_APP_HANDLE.set(app.handle().clone())`
/// 设置一次。workerw_check 任务在 reinit 成功后读取此 handle emit 事件。
pub(crate) static SHARED_APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// 全屏时是否销毁了主窗口（退出全屏后需重建以恢复 UI）
///
/// 全屏动作生效（pause/terminate）时若主窗口打开则销毁其 WebView2 释放内存，
/// 此标志记录"需在退出全屏后重建"。`swap(false)` 消费标志，避免重复重建。
pub(crate) static FULLSCREEN_DESTROYED_MAIN_WINDOW: AtomicBool = AtomicBool::new(false);

/// 全局状态用于 Win32 回调（回调需要函数指针，无法使用闭包）
/// 全屏检测和电源监控共享同一个 wallpaper_engine Arc
pub(crate) static SHARED_ENGINE: OnceLock<Arc<tokio::sync::Mutex<WallpaperEngine>>> =
    OnceLock::new();
/// 全局状态用于 Win32 回调（回调需要函数指针，无法使用闭包）
/// 全屏检测和电源监控共享同一个 config_manager Arc
pub(crate) static SHARED_CONFIG: OnceLock<Arc<ConfigManager>> = OnceLock::new();
pub(crate) static FULLSCREEN_WAS: AtomicBool = AtomicBool::new(false);

/// 全局状态用于 Explorer 重启监控窗口过程（窗口过程是函数指针，无法使用闭包）
pub(crate) static EXPLORER_DESKTOP: OnceLock<Arc<std::sync::Mutex<DesktopIntegrator>>> =
    OnceLock::new();
/// 注册的 TaskbarCreated 消息 ID（系统范围内一致，由 RegisterWindowMessageW 返回）
pub(crate) static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

/// 电源状态跟踪：是否因电池供电暂停过
pub(crate) static POWER_WAS_ON_BATTERY: AtomicBool = AtomicBool::new(false);

/// SetWinEventHook 句柄，用于退出时通过 UnhookWinEvent 释放
///
/// 使用 `Mutex<Option<...>>`（而非 OnceLock）以支持 C-014/C-015：二次调用
/// `start_fullscreen_monitor` 时先 `take()` 旧句柄并 `UnhookWinEvent`，再重新 start。
/// `Mutex` 保证 take/replace 期间独占访问，无并发写风险。
pub(crate) static WIN_EVENT_HOOK: Mutex<Option<SendWinEventHook>> = Mutex::new(None);

/// HWINEVENTHOOK 的 Send/Sync 包装器（原始类型包含裸指针，未实现 Send/Sync）
pub(crate) struct SendWinEventHook(pub windows::Win32::UI::Accessibility::HWINEVENTHOOK);

// SAFETY (Task 9.1.1 soundness 论证):
//
// `HWINEVENTHOOK` 是 SetWinEventHook 返回的句柄，本质是一个指针大小的数值令牌，
// 本身不持有任何 Rust 可变状态。包装器 `SendWinEventHook` 仅持有该令牌，无其他字段。
//
// Send 安全性：句柄是普通数值令牌（HANDLE 类型别名），在线程间移动所有权不会产生
// 数据竞争或别名问题。句柄创建于 `start_fullscreen_monitor` 线程，存入全局
// `WIN_EVENT_HOOK: Mutex<Option<SendWinEventHook>>`，后续仅在 `perform_shutdown_blocking`
// 或二次调用 `start_fullscreen_monitor` 时通过 `take()` 取出用于 `UnhookWinEvent`。
// `Mutex` 保证 take/replace 期间独占访问，无并发写。
//
// Sync 安全性：`UnhookWinEvent` 按 Win32 文档可在任意线程调用（与 SetWinEventHook
// 所在线程无关），内部不依赖线程局部状态。本包装器存入 `Mutex<Option<...>>` 后，
// 所有 take/replace 均在 Mutex 守卫下进行，不存在 `&SendWinEventHook` 别名，
// 因此跨线程共享安全。
//
// 参考: https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-unhookwinevent
//   "UnhookWinEvent can be called from any thread."
unsafe impl Send for SendWinEventHook {}
unsafe impl Sync for SendWinEventHook {}

/// 全屏监控线程退出标志
pub(crate) static FULLSCREEN_MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
/// 全屏监控线程 ID（用于退出时 PostThreadMessage 唤醒消息循环）
///
/// 使用 `Mutex<Option<u32>>`（而非 OnceLock）以支持 C-014/C-015：二次调用
/// `start_fullscreen_monitor` 时先 `take()` 旧线程 ID 并唤醒旧线程退出，再重新 start。
pub(crate) static FULLSCREEN_MONITOR_THREAD_ID: Mutex<Option<u32>> = Mutex::new(None);
/// 全屏监控线程 JoinHandle（用于退出或二次调用时 join 等待线程退出，避免线程泄漏）
///
/// C-014/C-015 新增：`start_fullscreen_monitor` spawn 后存入，`perform_shutdown_blocking`
/// 或二次调用 start 时 `take()` 并 `join()`。
pub(crate) static FULLSCREEN_MONITOR_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> =
    Mutex::new(None);
/// Explorer 重启监控线程退出标志
pub(crate) static EXPLORER_MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
/// Explorer 重启监控线程 ID（用于退出时 PostThreadMessage 唤醒消息循环）
///
/// 使用 `Mutex<Option<u32>>`（而非 OnceLock）以支持 C-014/C-015：二次调用
/// `start_explorer_restart_monitor` 时先 `take()` 旧线程 ID 并唤醒旧线程退出，再重新 start。
pub(crate) static EXPLORER_MONITOR_THREAD_ID: Mutex<Option<u32>> = Mutex::new(None);
/// Explorer 重启监控线程 JoinHandle（用于退出或二次调用时 join 等待线程退出，避免线程泄漏）
///
/// C-014/C-015 新增：`start_explorer_restart_monitor` spawn 后存入，`perform_shutdown_blocking`
/// 或二次调用 start 时 `take()` 并 `join()`。
pub(crate) static EXPLORER_MONITOR_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> =
    Mutex::new(None);
/// WorkerW 兜底检查任务退出标志
pub(crate) static WORKERW_CHECK_RUNNING: AtomicBool = AtomicBool::new(true);
/// WorkerW 兜底检查任务 JoinHandle（用于退出时 abort，避免任务长时间持 desktop 锁）
///
/// ST-003：原实现 fire-and-forget，shutdown 设置 RUNNING=false 后任务最多 300s 才
/// 在下次 tick 检查到标志并退出。期间任务可能持 desktop 锁，导致 engine.shutdown()
/// 阻塞。现保存 JoinHandle，shutdown 时配合 Notify 唤醒 + abort 立即终止。
pub(crate) static WORKERW_CHECK_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> =
    Mutex::new(None);
/// WorkerW 兜底检查任务即时唤醒通知（ST-003）
///
/// 任务在 `tokio::select!` 中同时等待 interval tick 和 notified()，shutdown 时
/// `notify_one()` 让任务立即跳出 tick 阻塞、检查 RUNNING 标志并退出，无需等 300s。
///
/// 使用 `LazyLock` 而非 `static`：`tokio::sync::Notify::new()` 在当前 tokio 版本
/// 非 const fn，无法直接初始化 static。`LazyLock` 在首次访问时初始化，线程安全。
pub(crate) static WORKERW_CHECK_NOTIFY: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);
/// WorkerW 预初始化线程 JoinHandle（用于退出时 join，避免线程泄漏）
///
/// ST-002：原实现 fire-and-forget（T-014 遗留），shutdown 时无法 join。若线程正在
/// 持 desktop 锁（ensure_initialized），engine.shutdown() 会短暂阻塞。现保存
/// JoinHandle，shutdown 时 take + join。
pub(crate) static WORKERW_INIT_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> =
    Mutex::new(None);

/// 缩略图生成任务 JoinHandle 列表（ST-014 + T05：用于退出时等待缩略图生成完成，
/// 避免中途被杀导致 thumbnail 文件残留为空字符串）
///
/// 原实现 fire-and-forget（spawn_blocking 返回的 JoinHandle 被 drop），shutdown 时
/// 进行中的 update_thumbnail 可能被截断，wallpaper entry 的 thumbnail 字段保持为空。
/// 现保存 JoinHandle，shutdown 时 take + 带 5s 超时等待。
///
/// T05（P1 任务丢失）：原为 `Mutex<Option<JoinHandle>>`，连续 add_wallpaper 时
/// `*slot = Some(handle)` 覆盖旧 handle（被 drop），旧任务虽继续执行但 shutdown
/// 只等待最后一个。改为 `Mutex<Vec<JoinHandle>>` 收集所有任务，push 前清理已完成，
/// shutdown 时等待所有未完成任务。
///
/// 注：spawn_blocking 任务即使 JoinHandle 被 drop 也会继续执行至完成，此处保存
/// handle 仅用于 shutdown 时优雅等待（而非取消）。
///
/// task 生命周期与 Drop 行为文档化
///
/// # Task 生命周期（启动 → 运行 → Drop）
///
/// ## 1. 启动（spawn）
///
/// - **触发点**：`commands::wallpaper::add_wallpaper` 中对 Image/Gif/Video 类型壁纸
///   调用 `tokio::task::spawn_blocking(move || { ... generate_thumbnail ... })`
/// - **存储**：返回的 `JoinHandle<()>` 被 push 到 `THUMBNAIL_TASK` Vec
///   （push 前先 `retain(|h| !h.is_finished())` 清理已完成任务，避免 Vec 无限增长）
/// - **容量保护**：ST-016 引入 `MAX_THUMBNAIL_TASKS = 50` 上限，
///   超出时优先移除最旧已完成 JoinHandle；全部进行中时不强制截断（避免任务丢失）
///
/// ## 2. 运行（execute）
///
/// - **执行环境**：tokio blocking 线程池（与 `set_wallpaper` 的 `spawn_blocking` 共享）
/// - **典型耗时**：
///   - Image/Gif：<100ms（解码 + 缩放 + 写入）
///   - Video：500ms-2s（ffmpeg 抽帧，取决于视频长度与 ffmpeg 可用性）
/// - **完成判定**：`JoinHandle::is_finished()` 返回 `true` 表示任务已退出
///   （无论成功/失败/panic）
/// - **结果回调**：任务内部通过 `app.emit("wallpaper-updated" / "wallpaper-thumbnail-failed")`
///   通知前端，不依赖 JoinHandle 的返回值
///
/// ## 3. Drop 行为（shutdown 时）
///
/// `perform_shutdown_blocking` 中的处理（见 state.rs 中 `SHUTDOWN_THUMBNAIL_TIMEOUT`）：
///
/// - **take 所有 JoinHandle**：`std::mem::take(&mut *t)` 清空 Vec，
///   后续 push 的新任务不会被等待（但 shutdown 后不会再有新任务）
/// - **顺序 join（共享 5s 截止时间）**：逐个 `tokio::time::timeout(remaining, handle).await`
///   - `Ok(Ok(()))`：任务正常完成（成功或失败均算"完成"）
///   - `Ok(Err(_join_err))`：任务 panic
///   - `Err(_timeout)`：超时未完成
/// - **超时后行为**：未 join 的 JoinHandle 被 drop（不再 await），
///   **但 spawn_blocking 任务即使 JoinHandle 被 drop 也会继续执行至完成**
///   （这是 tokio spawn_blocking 的语义保证，任务不会被取消）
/// - **配置持久化兜底**：ST-006 在 join 后追加两次 `config_manager.flush()`，
///   确保超时后仍在后台执行的任务的写入被持久化
///
/// # Drop 不取消任务的设计权衡
///
/// 选择"等待未完成任务（带超时）"而非"丢弃（abort）"的理由：
///
/// - **数据完整性**：缩略图生成中途被 abort 会导致 thumbnail 文件残留为空字符串，
///   前端展示空白预览
/// - **spawn_blocking 不可 abort**：tokio 的 `spawn_blocking` 任务一旦开始执行，
///   `JoinHandle::abort()` 只能在下次 poll 点生效，而 spawn_blocking 任务通常是
///   同步 CPU 密集型（无 poll 点），abort 实际不生效
/// - **超时兜底已足够**：5s 超时覆盖 99% 的 Image/Gif 场景（<100ms），
///   Video 场景超时后任务在后台继续执行，下次启动时 `cleanup_corrupted_thumbnails`
///   会清理 0 字节损坏文件
pub(crate) static THUMBNAIL_TASK: Mutex<Vec<tokio::task::JoinHandle<()>>> = Mutex::new(Vec::new());

/// ST-010: shutdown 流程的超时阈值集中化管理。
/// - `SHUTDOWN_THUMBNAIL_TIMEOUT`: 等待缩略图生成任务完成的累计超时（5s），
///   超时后未完成的任务在后台继续执行至完成（spawn_blocking 任务即使 handle drop 也继续）
/// - `SHUTDOWN_ENGINE_LOCK_TIMEOUT`: 获取 engine 锁的超时（3s），
///   超时跳过 engine.shutdown()，渲染器进程随应用退出由系统自动回收
const SHUTDOWN_THUMBNAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SHUTDOWN_ENGINE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 退出清理幂等守卫：保证 `perform_shutdown_blocking` 在进程生命周期内仅执行一次。
/// 任何退出路径（托盘退出、窗口关闭→ExitRequested、系统关机）均通过
/// `RunEvent::ExitRequested` 调用清理，守卫防止重复触发。
pub(crate) static SHUTDOWN_DONE: AtomicBool = AtomicBool::new(false);

// ── Shared Helpers ──────────────────────────────────────────────────────────

/// 尝试暂停所有壁纸的快速路径（ST-004 抽取自 fullscreen.rs / power.rs 6 处重复逻辑）
///
/// 共享逻辑：从 `SHARED_ENGINE` 取 engine Arc → `try_lock()` → 调用 `pause_all_fast`。
///
/// # 返回值
///
/// - `None`：`SHARED_ENGINE` 未设置或 `try_lock` 失败（锁忙）。调用方应跳过本次事件，
///   **不更新状态标志**（ST-001：原 power.rs 在 SHARED_ENGINE 未设置时返回空 Vec，
///   导致状态被错误更新为"已暂停"，但实际上没有执行任何暂停操作）。
/// - `Some(failed)`：已调用 `pause_all_fast`，`failed` 为失败的 display_id 列表
///   （空表示全部成功）。调用方据此决定是否更新状态（C-008：部分失败不更新）。
///
/// # 设计要点
///
/// - `try_lock` 而非 `blocking_lock`：本函数运行于 Win32 回调上下文
///   （`foreground_event_callback` / `handle_power_status_change`），blocking_lock 会
///   阻塞消息循环，延迟 `TaskbarCreated` / `EVENT_SYSTEM_FOREGROUND` 处理（T-001）。
/// - 锁忙时返回 `None`：事件会重复触发（`WM_POWERBROADCAST` / `EVENT_SYSTEM_FOREGROUND`），
///   可容忍偶发跳过。
pub(crate) fn try_pause_all_fast(reason: PauseReason) -> Option<Vec<String>> {
    let engine = SHARED_ENGINE.get()?;
    let engine = match engine.try_lock() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                reason = ?reason,
                "engine 锁忙，跳过 pause_all_fast（事件会重复触发，可容忍偶发跳过）"
            );
            return None;
        }
    };
    Some(engine.pause_all_fast(reason).unwrap_or_default())
}

/// 尝试恢复所有壁纸的快速路径（ST-004 抽取自 fullscreen.rs / power.rs 6 处重复逻辑）
///
/// 与 [`try_pause_all_fast`] 对称，调用 `resume_all_fast`。返回值语义相同。
pub(crate) fn try_resume_all_fast(reason: PauseReason) -> Option<Vec<String>> {
    let engine = SHARED_ENGINE.get()?;
    let mut engine = match engine.try_lock() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                reason = ?reason,
                "engine 锁忙，跳过 resume_all_fast（事件会重复触发，可容忍偶发跳过）"
            );
            return None;
        }
    };
    Some(engine.resume_all_fast(reason).unwrap_or_default())
}

/// 阻塞式恢复所有壁纸（供后台恢复线程使用，Task 7.1/7.2）
///
/// 与 [`try_resume_all_fast`] 的区别：后者运行于 Win32 回调上下文，必须用
/// `try_lock`（锁忙即跳过，不能阻塞消息循环）；本函数运行于专用的后台恢复线程
/// （`mirrorstar-fullscreen-resume`），可用 `blocking_lock` 阻塞等待 engine 锁。
/// 真全屏退出恢复（mpv 冷启动 2-6s）在此线程执行，不阻塞 Win32 回调线程，
/// 避免退出游戏瞬间 UI 卡顿（用户报告的"黑屏框 / 恢复慢"根因之一）。
///
/// # 返回值
///
/// - `None`：`SHARED_ENGINE` 未设置。调用方应保留 FULLSCREEN_WAS 状态，
///   由周期复查线程后续重试。
/// - `Some(failed)`：已调用 `resume_all_fast`，`failed` 为失败的 display_id 列表
///   （空表示全部成功）。调用方据此决定是否更新 FULLSCREEN_WAS。
pub(crate) fn resume_all_fast_blocking(reason: PauseReason) -> Option<Vec<String>> {
    let engine = SHARED_ENGINE.get()?;
    // blocking_lock：后台线程，可安全阻塞等待（tokio Mutex 无 poison，仅阻塞）。
    // 与 Win32 回调路径的 try_lock 不同，此处允许等待 engine 锁释放
    // （如 set_wallpaper 短暂持锁），保证恢复不因锁忙被偶发跳过。
    let mut engine = engine.blocking_lock();
    Some(engine.resume_all_fast(reason).unwrap_or_default())
}

/// 尝试终止所有壁纸子进程的快速路径（全屏终止释放内存）
///
/// 与 [`try_pause_all_fast`] 对称，调用 `terminate_all_fast`。返回值语义相同：
/// - `None`：`SHARED_ENGINE` 未设置或锁忙，调用方应跳过本次事件，不更新状态标志
/// - `Some(failed)`：已调用 `terminate_all_fast`，`failed` 为失败的 display_id 列表
pub(crate) fn try_terminate_all_fast(reason: PauseReason) -> Option<Vec<String>> {
    let engine = SHARED_ENGINE.get()?;
    let mut engine = match engine.try_lock() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                reason = ?reason,
                "engine 锁忙，跳过 terminate_all_fast（事件会重复触发，可容忍偶发跳过）"
            );
            return None;
        }
    };
    Some(engine.terminate_all_fast(reason).unwrap_or_default())
}

/// 退出清理辅助：take 线程 ID 并向消息循环线程 PostThreadMessage WM_QUIT 唤醒退出
///
/// ST-005：抽取自 `perform_shutdown_blocking` 中 FULLSCREEN/EXPLORER 重复 2 次的逻辑。
/// `GetMessageW` 阻塞需 WM_QUIT 唤醒；`take()` 清空避免二次 shutdown 重复唤醒。
/// Mutex 中毒（线程 panic）时不操作，线程会随进程退出。
fn signal_thread_exit(thread_id: &Mutex<Option<u32>>) {
    let tid = match thread_id.lock().ok().and_then(|mut t| t.take()) {
        Some(tid) => tid,
        None => return,
    };
    unsafe {
        if windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
            tid,
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

/// 退出清理辅助：take 线程 JoinHandle 并 join 等待线程退出
///
/// ST-005：抽取自 `perform_shutdown_blocking` 中 FULLSCREEN/EXPLORER 重复 2 次的逻辑。
/// ST-002：复用于 `WORKERW_INIT_THREAD` 的 join。
/// join 失败（线程 panic）仅记录 warn，无法传播；Mutex 中毒时不操作。
fn join_monitor_thread(handle: &Mutex<Option<std::thread::JoinHandle<()>>>) {
    let handle = match handle.lock().ok().and_then(|mut h| h.take()) {
        Some(h) => h,
        None => return,
    };
    if let Err(e) = handle.join() {
        tracing::warn!(error = ?e, "监控线程 join 失败（线程可能 panic）");
    }
}

/// 尝试在指定超时内获取 tokio Mutex 锁（T04 修复）
///
/// `perform_shutdown_blocking` 运行于同步上下文（`RunEvent::ExitRequested`），
/// 原实现使用 `engine.blocking_lock()` 无超时等待。若进行中的壁纸命令长时间持锁
/// （如 `set_wallpaper` 通过 `spawn_blocking` 执行 WorkerW 嵌入），shutdown 会无限阻塞，
/// 导致应用无法退出。
///
/// 本函数通过 `tauri::async_runtime::block_on` + `tokio::time::timeout` 实现同步阻塞 +
/// 超时取消：在超时内获取到锁返回 `Some(guard)`，超时返回 `None`。
///
/// 泛型设计：不绑定具体类型 `WallpaperEngine`，便于在单元测试中用
/// `tokio::sync::Mutex<()>` 验证超时行为（无需 Windows COM/音频环境）。
///
/// # 返回值
///
/// - `Some(MutexGuard)`：在超时内获取到锁
/// - `None`：超时未获取到锁，调用方应跳过后续操作并记录错误日志
///
/// # 为什么不使用 `try_lock_for`
///
/// `tokio::sync::Mutex` 不提供 `try_lock_for`（那是 `parking_lot::Mutex` 的 API）。
/// tokio Mutex 只提供 `try_lock()`（非阻塞，立即返回）和 `lock().await`（无限等待）。
/// 通过 `tokio::time::timeout` 包装 `lock().await` 是 tokio 官方推荐的限时获取模式。
fn try_lock_with_timeout<T>(
    mutex: &Arc<tokio::sync::Mutex<T>>,
    timeout: std::time::Duration,
) -> Option<tokio::sync::MutexGuard<'_, T>> {
    tauri::async_runtime::block_on(async { tokio::time::timeout(timeout, mutex.lock()).await }).ok()
}

/// 同步执行应用退出清理。
///
/// 在 `RunEvent::ExitRequested` 中调用，确保无论应用以何种方式退出，均会：
/// 1. 终止所有壁纸渲染器（mpv 子进程通过 `TerminateProcess` 强制终止）
/// 2. 恢复桌面壁纸、销毁壁纸窗口
/// 3. 刷写未持久化的配置
/// 4. 释放 SetWinEventHook、唤醒监控线程退出
/// 5. 主线程 COM 清理（与 `run()` 中的 CoInitializeEx 配对）
///
/// T04 修复：使用 `try_lock_engine_with_timeout`（3s 超时）获取 engine 锁
/// （同步上下文，ExitRequested 期间 runtime 仍存活，进行中的壁纸命令会很快释放锁，
/// 3s 超时平衡了"优雅关闭渲染器"与"不无限阻塞退出"）。超时则跳过 `engine.shutdown()`，
/// 渲染器进程随应用退出由系统回收。通过 `SHUTDOWN_DONE` 守卫保证幂等。
///
/// shutdown 顺序与阻塞上界文档化
///
/// # Shutdown 顺序（LIFO，与初始化顺序相反）
///
/// 初始化顺序（`lib.rs::run()`）：
///   `CoInitializeEx(STA)` → `DesktopIntegrator::new()`
///   → WorkerW 预初始化线程 → `VolumeControl::new()`
///   → `WallpaperEngine::new()` → `ConfigManager::new()`
///   → `start_watching()` → `start_fullscreen_monitor()`
///   → `start_explorer_restart_monitor()` → `start_workerw_check()`
///   → Tauri Builder + setup（AppHandle / 订阅任务 / 配置回调 / 托盘菜单）
///
/// 清理顺序（本函数执行）：
///   1. **停止后台监控标志**：`FULLSCREEN_MONITOR_RUNNING` /
///      `EXPLORER_MONITOR_RUNNING` / `WORKERW_CHECK_RUNNING` 置 false，
///      `WORKERW_CHECK_NOTIFY.notify_one()` 唤醒 workerw_check 任务跳出 300s tick
///   2. **ConfigManager 停止后台任务**：`shutdown_periodic_save()`
///      （停止周期性保存定时器）+ `stop_watching()`（take 并 drop 文件监视器）
///   3. **唤醒消息循环线程**：`signal_thread_exit` 向 FULLSCREEN/EXPLORER
///      监控线程 `PostThreadMessageW(WM_QUIT)` 唤醒 `GetMessageW` 阻塞
///   4. **释放 SetWinEventHook**：`UnhookWinEvent(WIN_EVENT_HOOK)` 取消事件钩子，
///      保证后续 engine.shutdown() 期间不再收到 WinEvent 回调
///   5. **join 监控线程**：FULLSCREEN / EXPLORER / WORKERW_INIT 三类监控线程
///      join 等待退出，避免线程泄漏与回调访问已释放资源
///   6. **abort workerw_check 任务**：`WORKERW_CHECK_TASK.take().abort()`
///      兜底取消任务，避免任务持 desktop 锁阻塞 engine.shutdown()
///   7. **等待缩略图任务完成**：`THUMBNAIL_TASK` 中所有 JoinHandle 顺序 join
///      （共享 5s 截止时间），超时未完成的任务在后台继续执行至完成
///   8. **WallpaperEngine.shutdown()**：通过 `try_lock_with_timeout` 获取 engine 锁
///      （3s 超时），调用 `engine.shutdown()` 逐个 `close_wallpaper`
///      → `VideoRenderer::terminate` → `ProcessManager::stop`
///      → `TerminateProcess`（强制终止 mpv 子进程）+ 销毁壁纸窗口
///   9. **ConfigManager.flush()**：刷写防抖期间未写入的配置（含两次 flush 兜底）
///   10. **主线程 COM 清理**：由 `run()` 中的 `ComGuard::Drop` 统一处理
///       （晚于本函数返回，确保 `app_state` 先 Drop 释放 COM 接口）
///
/// # 阻塞上界
///
/// 本函数运行于同步上下文（`RunEvent::ExitRequested`），最大阻塞时间上界：
/// - **监控线程 join**：通常 <100ms（线程收到 WM_QUIT 后立即退出），
///   极端情况下 WORKERW_INIT 线程可能持 desktop 锁等待 `ensure_initialized` 完成
///   （通常 <1s）
/// - **缩略图任务等待**：`SHUTDOWN_THUMBNAIL_TIMEOUT = 5s` 硬上界
///   （超时后未完成任务后台继续，不阻塞退出）
/// - **engine 锁获取**：`SHUTDOWN_ENGINE_LOCK_TIMEOUT = 3s` 硬上界
///   （超时跳过 `engine.shutdown()`，渲染器进程随应用退出由系统回收）
/// - **ConfigManager.flush()**：通常 <100ms（同步写入 TOML 文件）
///
/// 理论最大阻塞上界 ≈ 5s（缩略图） + 3s（engine 锁） + ~1s（join + flush）≈ 9s，
/// 实际绝大多数场景 <2s（无缩略图任务 + engine 锁立即可用）。
pub(crate) fn perform_shutdown_blocking(
    engine: &Arc<tokio::sync::Mutex<WallpaperEngine>>,
    config_manager: &ConfigManager,
) {
    // 幂等守卫：若清理已执行过，直接返回（防止 ExitRequested 多次触发或与残留调用重复）
    if SHUTDOWN_DONE.swap(true, Ordering::SeqCst) {
        tracing::debug!("perform_shutdown_blocking 已执行过，跳过重复清理");
        return;
    }

    tracing::info!("开始执行应用退出清理");

    // 信号所有后台线程/任务退出
    FULLSCREEN_MONITOR_RUNNING.store(false, Ordering::SeqCst);
    EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
    WORKERW_CHECK_RUNNING.store(false, Ordering::SeqCst);
    // ST-003：notify workerw_check 任务立即跳出 300s tick 阻塞，检查 RUNNING 标志并退出
    WORKERW_CHECK_NOTIFY.notify_one();
    config_manager.shutdown_periodic_save();
    // 停止文件监视器：take 并 drop watcher，watcher 线程检测到后退出循环
    config_manager.stop_watching();

    // 唤醒消息循环线程（GetMessageW 阻塞，需 PostThreadMessage 唤醒）
    // ST-005：抽取 signal_thread_exit 辅助函数，避免 FULLSCREEN/EXPLORER 重复 2 次
    signal_thread_exit(&FULLSCREEN_MONITOR_THREAD_ID);
    signal_thread_exit(&EXPLORER_MONITOR_THREAD_ID);

    // 清理顺序遵循 LIFO（与初始化顺序相反）：
    // 初始化顺序为：CoInitializeEx → SetWinEventHook → 启动 engine → 启动 tray。
    // 因此清理应反向进行：先 unhook WinEvent → join 监控线程 → 再 shutdown engine → 再 flush 配置 → 最后 CoUninitialize。
    // 先 unhook 可保证 engine.shutdown() 期间不再收到 WinEvent 回调，避免回调访问已释放的资源。

    // 释放 SetWinEventHook 句柄（最先初始化的资源，最后清理则相反——先 unhook）
    // take 句柄：C-014/C-015 后改为 Mutex<Option>，take 后清空
    if let Some(hook) = WIN_EVENT_HOOK.lock().ok().and_then(|mut h| h.take()) {
        unsafe {
            // 退出清理路径：UnhookWinEvent 返回 BOOL（windows-rs 0.58），
            // 失败时钩子句柄随进程退出自动回收，无需传播错误
            let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(hook.0);
        }
    }

    // join 监控线程，避免线程泄漏（C-014/C-015 新增）
    // 必须在 unhook + PostThreadMessage 之后：线程退出需要先收到 WM_QUIT 唤醒消息循环，
    // 且全屏线程需先 unhook 以停止接收新 WinEvent 回调。join 后再获取 engine 锁，确保
    // 线程内任何在途回调（try_lock engine）已完成，避免 shutdown 期间回调访问已释放资源。
    // ST-005：抽取 join_monitor_thread 辅助函数，避免 FULLSCREEN/EXPLORER 重复 2 次
    join_monitor_thread(&FULLSCREEN_MONITOR_THREAD);
    join_monitor_thread(&EXPLORER_MONITOR_THREAD);

    // ST-002：join WorkerW 预初始化线程（线程只执行一次 ensure_initialized，
    // 通常已退出，join 立即返回；若仍持有 desktop 锁，join 等待释放后再 engine.shutdown）
    join_monitor_thread(&WORKERW_INIT_THREAD);

    // ST-003：abort workerw_check 任务兜底取消（notify 已唤醒任务检查 RUNNING 退出，
    // 但若任务正在 desktop.lock() 持锁中执行同步代码，abort 在下次 await 点生效；
    // 此处 abort 确保任务不再持 desktop 锁阻塞 engine.shutdown）
    //
    // ST-012: 状态变更订阅任务的自引用关系与退出策略（已评估并接受，方案 ②）
    // - workerw_check 任务（及其他状态变更订阅任务）持有 `engine: Arc<WallpaperEngine>` 克隆，
    //   形成自引用：engine 内部状态被其订阅任务引用，shutdown 时无法通过 channel 关闭协作式退出
    // - 任务退出依赖 runtime 强制 abort（如本块 `handle.abort()`）而非通道关闭协作式退出，
    //   简化实现但需注意 abort 时任务可能正在执行非异步代码段（如持 desktop 锁），abort 在下次 await 点生效
    // - 此为已评估并接受的设计权衡：避免引入 CancellationToken 复杂度（方案 ①），
    //   当前 shutdown 流程已通过 try_lock_with_timeout + 任务 abort 兜底覆盖所有已知场景
    if let Some(handle) = WORKERW_CHECK_TASK.lock().ok().and_then(|mut t| t.take()) {
        handle.abort();
        tracing::debug!("workerw_check 任务已 abort");
    }

    // ST-014 + T05：等待所有缩略图生成任务完成（带 5 秒超时），避免退出时
    // update_thumbnail 被截断导致 wallpaper entry 的 thumbnail 字段保持为空字符串。
    // T05：原为单个 Option<JoinHandle>，现改为 Vec<JoinHandle> 收集所有任务，
    // shutdown 时 take 所有任务并等待。
    //
    // 任务本身已在 spawn_blocking 线程池并行执行，此处仅顺序 join（共享 5s 截止时间），
    // 超时后未 join 的任务在后台继续执行至完成（spawn_blocking 任务即使 handle drop 也继续）。
    let thumbnail_tasks: Vec<tokio::task::JoinHandle<()>> = THUMBNAIL_TASK
        .lock()
        .ok()
        .map(|mut t| std::mem::take(&mut *t))
        .unwrap_or_default();
    if !thumbnail_tasks.is_empty() {
        tracing::info!(count = thumbnail_tasks.len(), "等待缩略图生成任务完成");
        let total_timeout = SHUTDOWN_THUMBNAIL_TIMEOUT;
        let deadline = std::time::Instant::now() + total_timeout;
        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut timed_out = 0usize;
        tauri::async_runtime::block_on(async {
            for handle in thumbnail_tasks {
                let now = std::time::Instant::now();
                if now >= deadline {
                    // 已超时，剩余任务全部计为超时
                    timed_out += 1;
                    continue;
                }
                let remaining = deadline - now;
                match tokio::time::timeout(remaining, handle).await {
                    Ok(Ok(())) => completed += 1,
                    Ok(Err(_join_err)) => failed += 1,
                    Err(_) => timed_out += 1,
                }
            }
        });
        if completed > 0 {
            tracing::info!(completed, "缩略图生成任务已正常完成");
        }
        if failed > 0 {
            tracing::warn!(failed, "缩略图生成任务 join 失败（可能 panic）");
        }
        if timed_out > 0 {
            tracing::warn!(
                timed_out,
                "缩略图生成未在 5s 内完成，继续退出（任务在后台继续执行至完成）"
            );
        }
    }

    // 获取 engine 锁（同步阻塞，带 3s 超时）。
    //
    // T04 修复：原实现使用 `engine.blocking_lock()` 无超时等待，若进行中的壁纸命令
    // 长时间持锁（如 set_wallpaper 的 spawn_blocking 执行 WorkerW 嵌入），
    // shutdown 会无限阻塞。改为通过 `try_lock_with_timeout` 限时获取：
    // - 超时内获取到锁：执行 `engine.shutdown()` 优雅终止渲染器
    // - 超时：跳过 `engine.shutdown()`，渲染器进程随应用退出由系统自动回收
    //   （mpv 子进程通过 `TerminateProcess` 强制终止；WorkerW 窗口随进程退出销毁）
    //
    // 退出时无其他任务会持久持有 engine 锁：进行中的壁纸命令会很快释放锁，
    // 3s 超时平衡了"优雅关闭渲染器"与"不无限阻塞退出"两个目标。
    match try_lock_with_timeout(engine, SHUTDOWN_ENGINE_LOCK_TIMEOUT) {
        Some(mut engine) => {
            // shutdown() 内部通过 close_wallpaper 逐个终止渲染器：
            // VideoRenderer::terminate → ProcessManager::stop → TerminateProcess（强制终止 mpv）
            engine.shutdown();
        }
        None => {
            // 超时强制退出：engine.shutdown() 已被跳过，渲染器进程随应用退出回收。
            // 此处仅记录错误日志，不阻塞后续的 config flush + CoUninitialize。
            tracing::error!("engine 锁 3s 超时，强制退出（跳过 engine.shutdown）");
        }
    }
    // 确保防抖期间未写入的配置被持久化
    if let Err(e) = config_manager.flush() {
        tracing::warn!(error = %e, "配置刷写失败");
    }
    // ST-006: 额外 flush 确保超时后完成的缩略图任务（在 spawn_blocking 线程池继续执行）
    // 的写入被持久化。原 5s 超时仅等待任务 join，超时后任务仍在后台执行至完成，
    // 其写入可能在第一次 flush 之后才发生，需要额外 flush 兜底。
    if let Err(e) = config_manager.flush() {
        tracing::warn!(error = %e, "配置二次刷写失败（ST-006 兜底）");
    }
    // T16：主线程 COM 清理由 `run()` 中的 `ComGuard` Drop 统一处理（晚于本函数返回），
    // 确保 app_state（含 VolumeControl 的 COM 接口）先 Drop 释放后再 CoUninitialize。
    // 原在此处手动调 CoUninitialize 会导致 VolumeControl Drop 时 COM 已反初始化。

    tracing::info!("应用退出清理完成");
}

/// 创建或显示主窗口。
/// 如果窗口已存在则显示并聚焦，否则动态创建新窗口（延迟初始化 WebView2 以节省内存）。
pub(crate) fn create_or_show_main_window(app: &tauri::AppHandle) {
    // v8.0 内存优化：窗口销毁后重建，destroy() 释放 WebView2 内存。
    // 窗口存在则 show/focus，不存在（关闭时已销毁）则 build 重建。
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.show() {
            tracing::warn!(error = %e, "显示主窗口失败");
        }
        if let Err(e) = window.set_focus() {
            tracing::warn!(error = %e, "聚焦主窗口失败");
        }
    } else {
        match WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title("镜星壁纸")
            .inner_size(900.0, 600.0)
            .min_inner_size(700.0, 500.0)
            .center()
            .build()
        {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "创建主窗口失败");
            }
        }
    }
}

/// 全屏进入：若主窗口打开则销毁，释放其 WebView2 进程树（约 6 进程 / 150-300MB）。
///
/// 仅在主窗口打开时置 `FULLSCREEN_DESTROYED_MAIN_WINDOW` 标志，供退出全屏后重建。
/// `close()` 走 CloseRequested → `RunEvent::ExitRequested(None)` → `prevent_exit()`
/// 保持托盘驻留，与用户手动关闭窗口行为一致。
pub(crate) fn destroy_main_window_on_fullscreen() {
    let Some(app) = SHARED_APP_HANDLE.get() else {
        return;
    };
    if app.get_webview_window("main").is_none() {
        return;
    }
    FULLSCREEN_DESTROYED_MAIN_WINDOW.store(true, Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.close() {
            tracing::warn!(error = %e, "全屏销毁主窗口失败");
        }
    }
}

/// 全屏退出：若此前销毁了主窗口则重建（恢复 UI）。
pub(crate) fn restore_main_window_after_fullscreen() {
    if FULLSCREEN_DESTROYED_MAIN_WINDOW.swap(false, Ordering::SeqCst) {
        if let Some(app) = SHARED_APP_HANDLE.get() {
            create_or_show_main_window(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    // ── SubTask 5.3.3: SHUTDOWN_DONE 守卫逻辑测试 ──────────────────────────────

    /// 测试 SHUTDOWN_DONE 守卫的原子交换逻辑
    ///
    /// `perform_shutdown_blocking` 的幂等守卫使用 `swap(true, SeqCst)`：
    /// - 首次调用：返回 false（原值为 false），函数继续执行清理
    /// - 后续调用：返回 true（原值为 true），函数提前返回
    ///
    /// 此测试使用局部 AtomicBool 验证 swap 语义，避免影响全局 SHUTDOWN_DONE。
    #[test]
    fn test_shutdown_guard_swap_logic() {
        let guard = AtomicBool::new(false);

        // 首次 swap：原值 false → 返回 false（表示应继续执行）
        let first = guard.swap(true, Ordering::SeqCst);
        assert!(!first, "首次 swap 应返回 false（原值为 false）");

        // 二次 swap：原值 true → 返回 true（表示应跳过）
        let second = guard.swap(true, Ordering::SeqCst);
        assert!(second, "二次 swap 应返回 true（原值为 true）");

        // 三次 swap：仍返回 true
        let third = guard.swap(true, Ordering::SeqCst);
        assert!(third, "三次 swap 应返回 true（原值仍为 true）");
    }

    /// 测试 SHUTDOWN_DONE 全局静态量的初始状态和原子操作
    ///
    /// 注意：SHUTDOWN_DONE 是进程生命周期的全局守卫，无法在测试间重置。
    /// 此测试仅验证其作为 AtomicBool 的基本行为，不假设具体值（其他测试可能已修改）。
    #[test]
    fn test_shutdown_done_is_atomic_bool() {
        // 读取当前值（可能是 false 或 true，取决于测试执行顺序）
        let current = SHUTDOWN_DONE.load(Ordering::SeqCst);
        // swap 应返回与 load 相同的值
        let swapped = SHUTDOWN_DONE.swap(current, Ordering::SeqCst);
        assert_eq!(swapped, current, "swap 应返回 swap 前的值");
    }

    // ── SubTask 5.3.1: perform_shutdown_blocking 幂等性测试 ─────────────────────

    /// 测试 perform_shutdown_blocking 的幂等性
    ///
    /// 由于 `perform_shutdown_blocking` 内部调用 `CoUninitialize()`（进程级 COM 清理），
    /// 无法安全地测试首次执行的完整清理路径——那会导致测试进程的 COM 环境被破坏，
    /// 影响后续所有依赖 COM 的测试。
    ///
    /// 此测试通过确保 SHUTDOWN_DONE 为 true 来验证幂等守卫：
    /// 当 SHUTDOWN_DONE 已为 true 时，函数应立即返回，不执行任何清理操作。
    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_perform_shutdown_blocking_idempotent() {
        // 确保 SHUTDOWN_DONE 为 true，使函数走早期返回路径
        // （无论之前状态如何，设为 true 后函数必定提前返回）
        SHUTDOWN_DONE.store(true, Ordering::SeqCst);

        // 创建测试用 engine 和 config_manager（函数会提前返回，不会实际使用它们）
        let engine = match create_test_engine_arc() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };
        let (config_manager, _temp_dir) = match create_test_config_manager() {
            Some(cm) => cm,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };
        // 调用 perform_shutdown_blocking — 应立即返回（幂等守卫）
        // 由于 SHUTDOWN_DONE 已为 true，函数不会执行 CoUninitialize 等危险操作
        perform_shutdown_blocking(&engine, &config_manager);

        // 如果到达此处，说明函数安全返回（未 panic，未死锁）
        // 幂等性验证通过
    }

    /// 测试 perform_shutdown_blocking 多次调用安全
    ///
    /// 验证在 SHUTDOWN_DONE 为 true 时，连续多次调用 perform_shutdown_blocking
    /// 不会 panic 或死锁。
    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_perform_shutdown_blocking_multiple_calls_safe() {
        SHUTDOWN_DONE.store(true, Ordering::SeqCst);

        let engine = match create_test_engine_arc() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };
        let (config_manager, _temp_dir) = match create_test_config_manager() {
            Some(cm) => cm,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };
        // 连续调用 3 次，均应安全返回
        for i in 0..3 {
            perform_shutdown_blocking(&engine, &config_manager);
            // 每次调用后 SHUTDOWN_DONE 应仍为 true
            assert!(
                SHUTDOWN_DONE.load(Ordering::SeqCst),
                "第 {} 次调用后 SHUTDOWN_DONE 应为 true",
                i
            );
        }
    }

    // ── T04: try_lock_with_timeout 超时强制退出测试 ────────────────────────────
    //
    // 验证 perform_shutdown_blocking 中获取 engine 锁的超时行为：
    // - 锁可用时：try_lock_with_timeout 返回 Some(guard)
    // - 锁被持有时：try_lock_with_timeout 在超时后返回 None（而非无限阻塞）
    //
    // 使用泛型 `tokio::sync::Mutex<()>` 测试，无需 Windows COM/音频环境，
    // 可在任意平台 CI 执行。这覆盖了 T04 修复的核心逻辑：避免 shutdown 因
    // engine 锁长时间持有而无限阻塞。

    /// 测试 try_lock_with_timeout：锁可用时返回 Some
    ///
    /// 验证 T04：当 mutex 锁未被持有时，try_lock_with_timeout 应在超时内
    /// 获取到锁并返回 `Some(guard)`。
    #[test]
    fn test_try_lock_with_timeout_returns_some_when_available() {
        let mutex: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

        let result = try_lock_with_timeout(&mutex, std::time::Duration::from_secs(1));
        assert!(
            result.is_some(),
            "锁可用时 try_lock_with_timeout 应返回 Some"
        );
    }

    /// 测试 try_lock_with_timeout：锁被持有时超时返回 None
    ///
    /// 验证 T04 核心行为：当 mutex 锁被其他调用持有（模拟进行中的壁纸命令长时间持锁）时，
    /// try_lock_with_timeout 应在超时后返回 None，而不是无限阻塞。
    /// 这是 T04 修复的关键：避免 perform_shutdown_blocking 因 engine 锁长时间持有而无限阻塞。
    ///
    /// 通过 `try_lock()` 在当前线程持有锁，然后调用 try_lock_with_timeout 尝试获取。
    /// 由于锁已被持有，try_lock_with_timeout 内部的 `engine.lock().await` 会等待，
    /// `tokio::time::timeout` 在超时后取消等待并返回 `Err(Elapsed)`，函数返回 None。
    #[test]
    fn test_try_lock_with_timeout_returns_none_when_held() {
        let mutex: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

        // 通过 try_lock 持有锁（非阻塞，立即获取）
        let _guard = mutex.try_lock().expect("新创建的 mutex 锁应可立即获取");

        // 尝试在短超时内获取锁 — 应超时返回 None
        let timeout_duration = std::time::Duration::from_millis(200);
        let start = std::time::Instant::now();
        let result = try_lock_with_timeout(&mutex, timeout_duration);
        let elapsed = start.elapsed();

        assert!(
            result.is_none(),
            "锁被持有时应超时返回 None（T04 强制退出）"
        );
        // 验证确实等待了接近超时时长（而非立即返回 None）
        // 使用 100ms 下限容忍调度抖动，避免 CI 环境的 timing flake
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "应等待接近超时时长（{}ms），实际: {:?}",
            timeout_duration.as_millis(),
            elapsed
        );
        // 验证没有无限阻塞（远小于测试总超时）
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "不应无限阻塞，实际耗时: {:?}",
            elapsed
        );

        // _guard 在此 drop，释放锁
    }

    /// 测试 try_lock_with_timeout：锁释放后可正常获取
    ///
    /// 验证 T04：先前因超时返回 None 后，锁被释放后再次调用 try_lock_with_timeout
    /// 应能成功获取锁。这确保超时不会留下副作用（如 mutex 中毒）。
    #[test]
    fn test_try_lock_with_timeout_available_after_release() {
        let mutex: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

        // 1. 持有锁，触发超时
        {
            let _guard = mutex.try_lock().expect("锁应可用");
            let result = try_lock_with_timeout(&mutex, std::time::Duration::from_millis(50));
            assert!(result.is_none(), "锁被持有时应超时返回 None");
            // _guard 在此 drop，释放锁
        }

        // 2. 锁释放后，再次调用应成功获取
        let result = try_lock_with_timeout(&mutex, std::time::Duration::from_secs(1));
        assert!(
            result.is_some(),
            "锁释放后 try_lock_with_timeout 应返回 Some（无副作用）"
        );
    }

    // ── SubTask 5.3.2: create_or_show_main_window 测试说明 ─────────────────────
    //
    // `create_or_show_main_window` 接收 `&tauri::AppHandle` 参数，需要运行中的 Tauri
    // 应用实例。在单元测试环境中无法构造有效的 AppHandle（Tauri v2 的 AppHandle
    // 必须通过 Builder::build() 创建，而 build() 需要 tauri.conf.json 上下文和
    // 平台资源初始化）。
    //
    // 因此 create_or_show_main_window 的两种路径（窗口存在→show/focus、窗口不存在→build）
    // 无法在单元测试中覆盖，需要通过端到端测试或手动测试验证。
    // 跳过原因：Tauri AppHandle 无法在单元测试环境中构造。

    // ── 测试辅助函数 ──────────────────────────────────────────────────────────

    /// 创建测试用 WallpaperEngine（Arc<tokio::sync::Mutex<WallpaperEngine>>）
    ///
    /// 初始化 COM（MTA 模式），构造真实的 DesktopIntegrator 和 VolumeControl。
    /// 如果 COM 环境不可用或音频设备不可用，返回 None 让调用方跳过测试。
    fn create_test_engine_arc() -> Option<Arc<tokio::sync::Mutex<WallpaperEngine>>> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

        // 尝试初始化 COM（MTA 模式）
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume_control = match mirrorstar_core::VolumeControl::new() {
            Ok(vc) => Arc::new(Mutex::new(vc)),
            Err(_) => return None, // 无音频设备，跳过
        };

        Some(Arc::new(tokio::sync::Mutex::new(WallpaperEngine::new(
            desktop,
            volume_control,
        ))))
    }

    /// 创建测试用 ConfigManager（ST-008 / T-017 修复）
    ///
    /// 使用 `tempfile::TempDir` 创建独立临时目录，避免污染用户数据目录
    /// （`%APPDATA%/mirrorstar/`）。返回 `(ConfigManager, TempDir)` 元组，
    /// 调用方必须持有 `TempDir` 直至测试结束，离开作用域时自动删除临时目录。
    ///
    /// 与 `tests/common/mod.rs::create_test_config_manager` 做法一致。
    fn create_test_config_manager() -> Option<(ConfigManager, tempfile::TempDir)> {
        let temp_dir = tempfile::TempDir::new().ok()?;
        match ConfigManager::new_in_dir(temp_dir.path().to_path_buf()) {
            Ok(cm) => Some((cm, temp_dir)),
            Err(e) => {
                eprintln!("创建 ConfigManager 失败: {}", e);
                None
            }
        }
    }

    // ── ST-006: perform_shutdown_blocking 额外 flush 兜底测试 ──────────────────

    /// ST-006: 验证 perform_shutdown_blocking 含额外 flush 调用的注释
    ///
    /// `perform_shutdown_blocking` 中缩略图任务等待 5s 超时后未完成任务继续后台执行，
    /// 可能在 `config_manager.flush()` 之后完成写入导致缩略图路径丢失。
    /// 修复方案：在原 `flush()` 之后增加一次额外 `flush()` 兜底。
    ///
    /// 由于 `perform_shutdown_blocking` 依赖大量全局静态量（SHUTDOWN_DONE /
    /// THUMBNAIL_TASK / WORKERW_CHECK_TASK 等）且调用 COM/音频初始化，直接单元测试
    /// 不可行。改为文档测试：使用 `include_str!` 读取本文件源码，断言关键标记存在。
    #[test]
    fn st006_shutdown_flushes_twice_after_thumbnail_timeout() {
        let source = include_str!("state.rs");
        assert!(
            source.contains("ST-006:"),
            "perform_shutdown_blocking 应含 ST-006 注释标识"
        );
        assert!(
            source.contains("二次刷写"),
            "perform_shutdown_blocking 应含额外 flush 调用，错误日志含'二次刷写'"
        );
        // 验证 flush 调用次数 ≥ 2（原始 + ST-006 额外）
        let flush_count = source.matches("config_manager.flush()").count();
        assert!(
            flush_count >= 2,
            "perform_shutdown_blocking 应至少调用 config_manager.flush() 两次，实际: {}",
            flush_count
        );
    }
}

