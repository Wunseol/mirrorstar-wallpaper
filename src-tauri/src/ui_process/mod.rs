//! UI 子进程管理器与协议桥（Task 2）。
//!
//! # 职责
//!
//! 主窗口 UI 迁到独立原生子进程（`mirrorstar-ui.exe`，由 Task 3 创建）后，本模块在 Tauri
//! 壳侧集中托管其生命周期与协议：
//!
//! - [`UiProcessManager`]：spawn / 握手 / stdio 接线 / 异常退出处理 / 空闲退出 / 单实例。
//! - [`bridge`]：命令桥（`ui_proto` 请求 → 现有 `commands::*`，业务逻辑零复制）与
//!   事件桥（壳侧 13 个事件 → 协议推送，惰性转发）。
//!
//! # 唯一主窗口（Task 10）
//!
//! 原生 UI 子进程即**唯一主窗口**：托盘"打开主窗口"始终拉起/唤显 `mirrorstar-ui.exe`，
//! **不再创建 WebView2 主窗口**（旧 WebView2 主窗口路径已随 Task 10 移除）。进程按需拉起、
//! 窗口隐藏超时后回收（见下"空闲退出"）。
//!
//! # 空闲退出（SubTask 2.5）
//!
//! 阈值由环境变量 `MIRRORSTAR_UI_IDLE_TIMEOUT_SECS` 配置，默认 300 秒。**选环境变量而非
//! 配置文件**：该项是运行期资源策略（非用户可见设置），不进入 `AppConfig`（避免为纯优化
//! 参数污染用户配置文件与前端设置模型）；环境变量便于测试与运维覆盖。
//!
//! 计时来源：UI 进程在窗口隐藏/显示时上报 [`ControlAction::WindowHidden`] /
//! [`ControlAction::WindowShown`]，壳侧据此启停计时；超时后向 UI 进程发 `Exit` 并回收进程，
//! 内存归零，下次打开可再次按需拉起。重复打开请求不会产生第二个实例（[`Self::ensure_running`]
//! 的单槽位判定）。
//!
//! # 窗口显隐与前台策略（SubTask 2.6）
//!
//! 壳侧**只发送控制消息**，不做任何抢前台调用（全程无 `SetForegroundWindow` /
//! `SetActiveWindow`）。UI 进程收到 `Show` 后必须以 `SWP_NOACTIVATE` 显示窗口。
//!
//! ## 纯净模式 / 全屏检测是否依赖主窗口（结论）
//!
//! **不依赖**。`platform/pure_mode.rs` 与 `platform/fullscreen.rs` 均以"引擎状态 +
//! `GetForegroundWindow()`"工作，不查询主窗口是否存在：
//! - 纯净模式进入/退出只操作引擎与 WorkerW，与主窗口无关；
//! - 全屏检测对前台窗口做类名/标题判定，主窗口仅作为"自身窗口"被**标题**匹配排除
//!   （`is_self_window_title` 匹配 "镜星壁纸" / "MirrorStar Wallpaper"）。
//! - 原"全屏时最小化主窗口 / 退出全屏恢复主窗口"逻辑已随 Task 10 移除——不再有 Tauri
//!   主窗口需要最小化或恢复。
//!
//! 因此不破坏现有前台策略。原生 UI 窗口标题保持"镜星壁纸"并遵守 `SWP_NOACTIVATE`，
//! 避免其成为前台窗口时被全屏检测误判为全屏应用。
//!
//! # 应用清单（SubTask 2.7）
//!
//! `mirrorstar-ui.exe` **必须自带应用清单**（`comctl32 v6` 的 side-by-side 依赖 +
//! `PerMonitorV2` DPI 感知），否则 release 导入 `comctl32!TaskDialogIndirect` 会在加载期
//! 失败（`STATUS_ENTRYPOINT_NOT_FOUND` `0xC0000139`）。
//!
//! - 实现：`crates/mirrorstar-ui/build.rs`（`/MANIFEST:EMBED` +
//!   `/MANIFESTINPUT:<app.manifest>`，清单见同目录 `app.manifest`）。
//! - Tauri 侧（本 crate）自身清单由 `tauri-build` 自动注入（`build.rs` 调用
//!   `tauri_build::build()`），已满足要求，无需手工处理。

mod bridge;
mod paths;

use std::ffi::c_void;
use std::io::{BufReader, BufWriter};
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::AppHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use mirrorstar_core::ui_proto::framing::{write_message, MessageReceiver};
use mirrorstar_core::ui_proto::{
    capability, ControlAction, Hello, UiControl, UiMessage, PROTOCOL_VERSION,
};
use mirrorstar_core::MirrorStarError;

pub use bridge::install_event_forwarding;

pub use paths::resolve_ui_exe;

/// 空闲退出阈值的环境变量名（单位：秒）。
pub const IDLE_TIMEOUT_ENV: &str = "MIRRORSTAR_UI_IDLE_TIMEOUT_SECS";
/// 空闲退出默认阈值（秒）。
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;
/// 空闲检查周期（秒）。设为阈值的 1/10 量级，兼顾及时性与空转开销。
pub const IDLE_TICK_SECS: u64 = 30;

/// 握手超时：`mirrorstar-ui.exe` 冷启动（加载 Slint + 软件渲染）可能较慢，给足余量；
/// 超时即判定异常并终止，避免壳侧无限等待。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 协议泵轮询间隔：用于在无消息时周期性检查退出/停止条件，避免无限阻塞。
const PUMP_POLL: Duration = Duration::from_millis(1000);

/// 解析空闲阈值；非法/缺失时回退默认值。
pub fn idle_timeout_from(value: Option<&str>, default: Duration) -> Duration {
    value
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// 空闲退出判定（纯函数，便于单测）。
///
/// 仅当**进程在运行**且**窗口已隐藏**且**隐藏时长 ≥ 阈值**时返回 true。
pub fn should_exit_idle(running: bool, hidden_for: Option<Duration>, timeout: Duration) -> bool {
    running && hidden_for.is_some_and(|d| d >= timeout)
}

/// 全局单例：在 `run()` 中初始化一次，之后由托盘/事件桥/空闲任务共享。
static MANAGER: OnceLock<Arc<UiProcessManager>> = OnceLock::new();

/// 初始化并注册全局 UI 进程管理器（幂等：重复调用返回既有实例）。
pub fn init() -> Arc<UiProcessManager> {
    if let Some(existing) = MANAGER.get() {
        return existing.clone();
    }
    let exe = resolve_ui_exe();
    let timeout = idle_timeout_from(
        std::env::var(IDLE_TIMEOUT_ENV).ok().as_deref(),
        Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
    );
    let manager = Arc::new(UiProcessManager::new(exe, timeout));
    let _ = MANAGER.set(manager.clone());
    manager
}

/// 取全局管理器（`init` 之前为 `None`）。
pub fn manager() -> Option<Arc<UiProcessManager>> {
    MANAGER.get().cloned()
}

/// 启动空闲退出检查后台线程（setup 中调用一次）。
///
/// 周期为 [`IDLE_TICK_SECS`]。线程持有管理器的强引用且 detached，随进程退出而结束。
pub fn spawn_idle_tick(manager: Arc<UiProcessManager>) {
    let interval = Duration::from_secs(IDLE_TICK_SECS);
    let spawn = std::thread::Builder::new()
        .name("mirrorstar-ui-idle".to_string())
        .spawn(move || loop {
            std::thread::sleep(interval);
            manager.tick_idle();
        });
    if let Err(e) = spawn {
        tracing::warn!(error = %e, "启动 UI 空闲检查线程失败（空闲退出不可用）");
    }
}

/// 托盘"打开主窗口"的统一入口：拉起/唤显原生 UI 子进程。
///
/// 集中在此处，调用方（托盘菜单 / 托盘左键 / 测试 autoshow）无需各自判断。
/// 管理器未初始化时仅记录警告（不影响壁纸与托盘）。
pub fn open_main_window(app: &AppHandle) {
    match manager() {
        Some(manager) => manager.request_show(app),
        None => tracing::warn!("请求打开主窗口，但 UI 进程管理器未初始化"),
    }
}

/// 壳侧 Job Object 守卫：为 UI 子进程建立 `KILL_ON_JOB_CLOSE` 作业。
///
/// 作用域是**壳进程生命周期**：壳退出/崩溃时内核关闭本句柄并终止作业内全部进程，从而回收
/// UI 子进程及其后代——UI 再 spawn 的预览 `mpv` 与 `msedgewebview2` 会因 Job 继承同属本
/// 作业（Windows 8+ 支持嵌套 Job，UI 侧为 mpv 自建的 Job 与之叠加不冲突）。UI 进程自身崩溃
/// 时的兜底回收仍由各自的 Job 负责，二者互补。
struct UiJobGuard {
    /// 原始句柄以 `isize` 保存：`HANDLE` 内部是裸指针（`!Send`/`!Sync`），而本管理器需经
    /// `Arc`/`Weak` 跨线程共享，故不直接持有 `HANDLE`。
    handle: isize,
}

impl UiJobGuard {
    /// 创建并配置作业；失败仅告警返回 `None`（降级为无内核兜底，不阻断 UI 拉起）。
    fn new() -> Option<Self> {
        let handle = match unsafe { CreateJobObjectW(None, None) } {
            Ok(h) => h.0 as isize,
            Err(e) => {
                tracing::warn!(error = %e, "创建 UI Job Object 失败（壳退出时无内核级兜底回收）");
                return None;
            }
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(e) = unsafe {
            SetInformationJobObject(
                HANDLE(handle as *mut c_void),
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } {
            tracing::warn!(error = %e, "配置 UI Job Object 失败（壳退出时无内核级兜底回收）");
            unsafe {
                let _ = CloseHandle(HANDLE(handle as *mut c_void));
            }
            return None;
        }
        Some(Self { handle })
    }

    /// 尽力把已 spawn 的子进程关联到作业。
    fn assign(&self, child: &Child) -> Result<(), MirrorStarError> {
        let process = HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(HANDLE(self.handle as *mut c_void), process) }.map_err(
            |e| {
                MirrorStarError::ProcessSpawnFailed(format!("AssignProcessToJobObject 失败: {}", e))
            },
        )
    }
}

impl Drop for UiJobGuard {
    fn drop(&mut self) {
        // 关闭作业句柄：因 KILL_ON_JOB_CLOSE，作业内残留进程被内核终止。
        unsafe {
            let _ = CloseHandle(HANDLE(self.handle as *mut c_void));
        }
    }
}

/// 正在运行的 UI 进程句柄（单槽位，保证同一时刻至多一个实例）。
struct RunningUi {
    pid: u32,
    child: Child,
    /// 协议写入端（stdin）；泵线程与事件转发共享。
    writer: Arc<Mutex<BufWriter<std::process::ChildStdin>>>,
    /// 壳侧 Job Object（`KILL_ON_JOB_CLOSE`）；创建/关联失败时为 `None`。
    _job: Option<UiJobGuard>,
}

/// UI 子进程管理器。
pub struct UiProcessManager {
    /// 至多一个运行实例；`None` = 未运行。
    inner: Mutex<Option<RunningUi>>,
    /// 已解析的可执行文件路径。
    exe: std::path::PathBuf,
    /// 空闲退出阈值。
    idle_timeout: Duration,
    /// 主窗口最近一次隐藏时刻；`None` = 窗口可见（或未运行），不计时。
    last_hidden_at: Mutex<Option<Instant>>,
}

impl UiProcessManager {
    fn new(exe: std::path::PathBuf, idle_timeout: Duration) -> Self {
        Self {
            inner: Mutex::new(None),
            exe,
            idle_timeout,
            last_hidden_at: Mutex::new(None),
        }
    }

    /// 是否有 UI 进程在运行。
    pub fn is_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 请求显示主窗口：确保进程已拉起，然后发送 `Show` 控制消息。
    ///
    /// **非阻塞**：拉进程 + 握手在独立线程执行，避免阻塞托盘事件所在的主线程。
    /// 重复请求不会产生第二个实例（[`Self::ensure_running`] 单槽位判定）。
    pub fn request_show(self: &Arc<Self>, app: &AppHandle) {
        self.mark_shown();
        let manager = Arc::clone(self);
        let app = app.clone();
        let spawn = std::thread::Builder::new()
            .name("mirrorstar-ui-spawn".to_string())
            .spawn(move || match manager.ensure_running(&app) {
                Ok(pid) => {
                    tracing::info!(pid, "UI 进程已就绪，发送显示指令");
                    let show = UiMessage::Control(UiControl {
                        action: ControlAction::Show,
                    });
                    if let Err(e) = manager.send(&show) {
                        tracing::warn!(error = %e, "发送显示指令失败");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, exe = %manager.exe.display(), "拉起 UI 进程失败（不影响壁纸与托盘）");
                }
            });
        if let Err(e) = spawn {
            tracing::warn!(error = %e, "创建 UI 进程拉起线程失败");
        }
    }

    /// 确保 UI 进程在运行：已在运行则直接返回其 pid；否则 spawn + 握手 + 启泵。
    fn ensure_running(self: &Arc<Self>, app: &AppHandle) -> Result<u32, MirrorStarError> {
        // 全程持锁：串行化并发拉起，天然保证"至多一个实例"。
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = guard.as_ref() {
            return Ok(running.pid);
        }

        let mut child = Command::new(&self.exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 继承：UI 进程日志与主进程共用同一控制台/日志重定向目标。
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                MirrorStarError::ProcessSpawnFailed(format!(
                    "启动 {} 失败: {}",
                    self.exe.display(),
                    e
                ))
            })?;
        let pid = child.id();

        // 壳侧 Job Object：把 UI 子进程纳入 `KILL_ON_JOB_CLOSE` 作业，壳进程退出/崩溃时
        // 由内核兜底回收 UI 及其全部后代（UI 再 spawn 的预览 mpv / msedgewebview2 因 Job
        // 继承同属本作业）。**尽力而为**：若壳自身已处于不允许嵌套的 Job 中，关联会失败，
        // 此时仅告警不阻断（退化为原有"下次启动清理"路径）。
        let job = UiJobGuard::new();
        if let Some(job) = job.as_ref() {
            if let Err(e) = job.assign(&child) {
                tracing::warn!(
                    error = %e,
                    "UI 子进程纳入 Job Object 失败（壳退出时无内核级兜底回收）"
                );
            }
        }

        // 取管道句柄（piped 时必存在）。
        let stdin = child.stdin.take().ok_or_else(|| {
            let _ = child.kill();
            MirrorStarError::ProcessSpawnFailed("UI 进程 stdin 管道缺失".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            let _ = child.kill();
            MirrorStarError::ProcessSpawnFailed("UI 进程 stdout 管道缺失".to_string())
        })?;
        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        // 读取端交给 Task 1 的带超时接收器（独立读取线程）。
        let receiver = MessageReceiver::spawn(BufReader::new(stdout));

        // 握手：双方各发一次、各读一次 Hello。
        let our_hello = UiMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities: capability::EVENT_PUSH | capability::REQUEST_RESPONSE,
            client: "mirrorstar-wallpaper".to_string(),
        });
        if let Err(e) = self.write_to(&writer, &our_hello) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(UiMessage::Hello(hello)) => {
                if hello.protocol_version != PROTOCOL_VERSION {
                    // 版本不匹配：双方以明确错误终止并记录日志。
                    tracing::error!(
                        their = hello.protocol_version,
                        ours = PROTOCOL_VERSION,
                        client = %hello.client,
                        "UI 协议版本不匹配，终止本次握手"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(MirrorStarError::IpcError(format!(
                        "UI 协议版本不匹配（UI={} 壳={}）",
                        hello.protocol_version, PROTOCOL_VERSION
                    )));
                }
                tracing::info!(
                    pid,
                    client = %hello.client,
                    capabilities = hello.capabilities,
                    "UI 进程握手成功"
                );
            }
            Ok(other) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MirrorStarError::IpcError(format!(
                    "握手期间收到非 Hello 消息: {:?}",
                    other
                )));
            }
            Err(e) => {
                tracing::error!(error = %e, "等待 UI 进程握手超时或失败");
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }

        *guard = Some(RunningUi {
            pid,
            child,
            writer: writer.clone(),
            _job: job,
        });

        // 协议泵线程：持续消费 UI → 壳 的请求并回响应；UI 侧上报的窗口可见性控制在此更新计时。
        // 用 `Weak` 回指管理器，避免"管理器 → 线程 → 管理器"的强引用环。
        let weak = Arc::downgrade(self);
        let app_for_pump = app.clone();
        let pump = std::thread::Builder::new()
            .name("mirrorstar-ui-pump".to_string())
            .spawn(move || pump_loop(receiver, writer, app_for_pump, weak, pid));
        if let Err(e) = pump {
            tracing::error!(error = %e, "启动 UI 协议泵线程失败，回收进程");
            if let Some(running) = guard.take() {
                let mut running = running;
                let _ = running.child.kill();
                let _ = running.child.wait();
            }
            return Err(MirrorStarError::ProcessSpawnFailed(format!(
                "启动协议泵线程失败: {}",
                e
            )));
        }

        Ok(pid)
    }

    /// 发送一条协议消息到 UI 进程（未运行时 `IpcNotConnected`）。
    pub fn send(&self, message: &UiMessage) -> Result<(), MirrorStarError> {
        let writer = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(running) => running.writer.clone(),
                None => {
                    return Err(MirrorStarError::IpcNotConnected(
                        "UI 进程未运行".to_string(),
                    ))
                }
            }
        };
        self.write_to(&writer, message)
    }

    fn write_to(
        &self,
        writer: &Arc<Mutex<BufWriter<std::process::ChildStdin>>>,
        message: &UiMessage,
    ) -> Result<(), MirrorStarError> {
        let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
        write_message(&mut *guard, message)
    }

    /// 标记窗口已隐藏（开始空闲计时）。
    fn mark_hidden(&self) {
        *self
            .last_hidden_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// 标记窗口已显示（清除空闲计时）。
    fn mark_shown(&self) {
        *self
            .last_hidden_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 空闲检查：隐藏超时则请求并回收 UI 进程（内存归零）。
    pub fn tick_idle(&self) {
        let running = self.is_running();
        let hidden_for = self
            .last_hidden_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed());
        if should_exit_idle(running, hidden_for, self.idle_timeout) {
            tracing::info!(
                timeout_secs = self.idle_timeout.as_secs(),
                "UI 进程空闲超时，退出以回收内存"
            );
            self.shutdown();
        }
    }

    /// 请求 UI 进程正常退出并回收（幂等）。应用退出与空闲退出共用。
    pub fn shutdown(&self) {
        // 先尽力发 Exit，让其走正常退出路径（若已断开则忽略）。
        let exit = UiMessage::Control(UiControl {
            action: ControlAction::Exit,
        });
        let _ = self.send(&exit);

        let running = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(mut running) = running {
            // 给正常退出留极短窗口，随后强制终止兜底（进程已死时 kill 返回 Err，忽略）。
            let _ = running.child.kill();
            let _ = running.child.wait();
            tracing::info!(pid = running.pid, "UI 进程已回收");
        }
        self.mark_shown();
    }

    /// 泵线程退出回调：若退出的是当前实例，则回收句柄并清除计时。
    fn on_pump_exit(&self, pid: u32) {
        let running = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(running) if running.pid == pid => guard.take(),
                _ => None,
            }
        };
        if let Some(mut running) = running {
            let status = running.child.try_wait().ok().flatten();
            let _ = running.child.kill();
            let _ = running.child.wait();
            self.mark_shown();
            tracing::warn!(
                pid = running.pid,
                status = ?status,
                "UI 进程已退出（连接断开），等待下次按需拉起"
            );
        }
    }
}

/// 协议泵：读 UI → 壳 的消息并处理，直到连接断开或停止。
fn pump_loop(
    receiver: MessageReceiver,
    writer: Arc<Mutex<BufWriter<std::process::ChildStdin>>>,
    app: AppHandle,
    weak: std::sync::Weak<UiProcessManager>,
    pid: u32,
) {
    loop {
        match receiver.recv_timeout(PUMP_POLL) {
            Ok(UiMessage::Request(request)) => {
                // 业务命令分发：同步等待（泵线程为独立 std 线程，不经异步运行时线程）。
                let response =
                    tauri::async_runtime::block_on(bridge::handle_request(&app, &request));
                let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = write_message(&mut *guard, &UiMessage::Response(response)) {
                    tracing::warn!(error = %e, "回写命令响应失败，结束协议泵");
                    break;
                }
            }
            Ok(UiMessage::Control(control)) => {
                // UI 侧上报窗口可见性，用于空闲退出计时。
                if let Some(manager) = weak.upgrade() {
                    match control.action {
                        ControlAction::WindowHidden => manager.mark_hidden(),
                        ControlAction::WindowShown => manager.mark_shown(),
                        // Show/Hide/Exit 由壳侧发出，不应从 UI 侧收到；忽略。
                        ControlAction::Show | ControlAction::Hide | ControlAction::Exit => {}
                    }
                }
            }
            // Hello（重复）/ Response / Event 不应由 UI 侧发出，忽略。
            Ok(_) => {}
            Err(MirrorStarError::IpcTimeout(_)) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "UI 协议泵读取结束");
                break;
            }
        }
    }
    if let Some(manager) = weak.upgrade() {
        manager.on_pump_exit(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_parsing_falls_back_on_invalid() {
        let default = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
        assert_eq!(
            idle_timeout_from(Some("60"), default),
            Duration::from_secs(60)
        );
        assert_eq!(
            idle_timeout_from(Some(" 120 "), default),
            Duration::from_secs(120)
        );
        // 0 / 负数 / 非数字 / 缺失 → 回退默认
        assert_eq!(idle_timeout_from(Some("0"), default), default);
        assert_eq!(idle_timeout_from(Some("abc"), default), default);
        assert_eq!(idle_timeout_from(None, default), default);
    }

    #[test]
    fn ui_job_guard_creation_succeeds_and_drops() {
        // Windows Job Object 可用性：创建 + 配置 KILL_ON_JOB_CLOSE 应成功，
        // Drop 关闭句柄不 panic（无成员时不误杀）。
        let guard = UiJobGuard::new();
        assert!(guard.is_some(), "Job Object（KILL_ON_JOB_CLOSE）应能创建");
        drop(guard);
    }

    #[test]
    fn should_exit_idle_requires_running_and_hidden_beyond_threshold() {
        let timeout = Duration::from_secs(300);
        assert!(!should_exit_idle(
            false,
            Some(Duration::from_secs(600)),
            timeout
        ));
        assert!(!should_exit_idle(true, None, timeout));
        assert!(!should_exit_idle(
            true,
            Some(Duration::from_secs(299)),
            timeout
        ));
        assert!(should_exit_idle(
            true,
            Some(Duration::from_secs(300)),
            timeout
        ));
        assert!(should_exit_idle(
            true,
            Some(Duration::from_secs(600)),
            timeout
        ));
    }
}
