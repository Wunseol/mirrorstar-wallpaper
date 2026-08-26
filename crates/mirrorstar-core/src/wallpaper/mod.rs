//! Wallpaper 模块：渲染器抽象、状态管理与子进程句柄 RAII。
//!
//! ## 锁中毒恢复策略（W-011）
//!
//! Wallpaper 模块所有 `Mutex` / `RwLock` 访问统一使用
//! `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁，而非 `unwrap()` panic。
//!
//! **决策权衡**：保留中毒数据（可能半写入不一致）vs 默认值回退（丢失运行时状态）。
//!
//! **选择保留策略的原因**：
//! - 渲染器运行时状态（如 `WallpaperState`、`volume`）丢失会导致壁纸停止响应，影响用户可见性
//! - 配置类数据（如 `gif_config`）中毒概率极低（仅 panic-in-lock 触发）
//! - 中毒发生后用户可手动重启应用恢复一致性
//!
//! **未来改进方向**：对配置类数据评估改用 `Default::default()` 回退，需评估状态丢失影响。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::GetExitCodeProcess;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

// ── v5.0 W-PERF-003: 屏幕分辨率缓存 ──────────────────────────────────────────

/// 屏幕分辨率缓存。
///
/// 屏幕分辨率在应用运行期间极少变化（仅多显示器热插拔 / DPI 变化时），
/// 使用 `Mutex<Option<(u32, u32)>>` 缓存首次查询结果，避免每次 Resume/解码
/// 都调用 `GetSystemMetrics(SM_CXSCREEN / SM_CYSCREEN)`。
///
/// 通过 [`invalidate_screen_size_cache`] 在 `WM_DISPLAYCHANGE` /
/// `WM_DPICHANGED` 消息处理中失效缓存。
///
/// **方案选择**：不使用 `OnceLock` 因其不支持 `clear` / 重置；改用
/// `Mutex<Option<...>>` 允许失效。锁仅用于缓存未命中时填充，命中路径开销
/// 极低（一次 `lock` + 读取 + 释放）。
///
/// `Mutex::new(None)` 自 Rust 1.63 起为 `const fn`，可用于 `static` 初始化。
static SCREEN_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// 获取屏幕分辨率（带缓存）。
///
/// 返回 `(width, height)`，失败时回退到 `(1920, 1080)`。缓存首次查询结果，
/// 后续调用直接返回缓存值，避免重复调用 `GetSystemMetrics`。
///
/// 调用方包括 `load_and_downsample_image`、`decode_gif_with_cancel`、
/// `decode_gif_first_frame`、`build_wp_proc_args` 等热路径（play() 与每次
/// Resume 都会触发）。
///
/// # 并发语义
///
/// 多个线程可能同时缓存未命中并各自查询一次 `GetSystemMetrics`，最后写入
/// 者获胜（值相同，无正确性问题）。命中路径开销极低，未命中路径仅在首次
/// 调用或失效后第一次调用时发生。
pub fn get_screen_size() -> (u32, u32) {
    {
        let guard = SCREEN_SIZE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(size) = *guard {
            return size;
        }
    }
    // 缓存未命中：查询并填充。多个线程可能同时进入此分支，但每个线程
    // 都会查询并填充相同值，最后写入者获胜，无正确性问题。
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let size = (
        if w > 0 { w as u32 } else { 1920 },
        if h > 0 { h as u32 } else { 1080 },
    );
    let mut guard = SCREEN_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(size);
    size
}

/// v5.0 W-PERF-003: 失效屏幕分辨率缓存。
///
/// 在 `WM_DISPLAYCHANGE` / `WM_DPICHANGED` 消息处理中调用，确保后续
/// [`get_screen_size`] 重新查询实际分辨率。
pub fn invalidate_screen_size_cache() {
    let mut guard = SCREEN_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// v8-C: 测试专用辅助函数，强制设置屏幕分辨率缓存。
///
/// 用于需要确定性行为的测试（如大帧 GIF 预算截断测试需避免屏幕分辨率
/// 差异导致降采样）。调用后 [`get_screen_size`] 将返回 `(w, h)` 而非
/// 查询 `GetSystemMetrics`。测试结束时应调用 [`invalidate_screen_size_cache`]
/// 恢复默认行为，避免影响其他测试。
#[cfg(test)]
pub(crate) fn set_screen_size_for_test(w: u32, h: u32) {
    let mut guard = SCREEN_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((w, h));
}

/// RAII 包装器：独占拥有一个 Win32 进程句柄，Drop 时调用 `CloseHandle`。
///
/// W-001 修复：`WebRenderer::create_pause_sender` 通过 `duplicate_process_handle`
/// 复制子进程句柄后传入监听线程。若 `std::thread::Builder::spawn` 失败，原实现
/// 仅记录日志返回，句柄永不被关闭，导致 Win32 进程句柄泄漏。
///
/// 此包装器确保无论 spawn 成败，句柄都会被正确释放：
/// - spawn 成功：闭包内 `take()` 取出句柄使用，`Drop` 不再关闭
/// - spawn 失败：闭包被 drop，`OwnedProcHandle::drop` 调用 `CloseHandle`
///
/// W-002 修复：`VideoRenderer::create_pause_sender` 复用此封装监听 mpv 子进程退出。
pub(crate) struct OwnedProcHandle(Option<HANDLE>);

// SAFETY: Win32 HANDLE 是进程级资源，可跨线程安全使用（DuplicateHandle 创建的
// 副本归本结构独占持有，不存在竞争）。`HANDLE` 内部为 `*mut c_void`，未自动实现
// `Send`，但本结构通过 RAII 保证句柄生命周期明确：仅由 `take()` 转移所有权或
// `Drop` 关闭，不会出现悬垂指针。为支持 `spawn` 跨线程 move，显式声明 `Send`。
unsafe impl Send for OwnedProcHandle {}

impl OwnedProcHandle {
    /// 创建新的独占句柄包装器
    pub(crate) fn new(handle: HANDLE) -> Self {
        Self(Some(handle))
    }

    /// 取出内部句柄，转移所有权。调用后 `Drop` 不会再调用 `CloseHandle`。
    pub(crate) fn take(&mut self) -> Option<HANDLE> {
        self.0.take()
    }
}

impl Drop for OwnedProcHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // v41-W-002: 防御性 `is_invalid()` 守卫，避免对 null 或
            // INVALID_HANDLE_VALUE 调用 `CloseHandle`。
            //
            // `CloseHandle` 对无效句柄会返回 ERROR_INVALID_HANDLE 并产生异常事件
            // （若启用句柄异常诊断）。`HANDLE::is_invalid()` 判断 null 或
            // INVALID_HANDLE_VALUE (-1)，与 `OwnedHandle::drop`（ipc_server.rs）
            // 的守卫逻辑保持一致。
            //
            // 实际场景：`new(HANDLE::default())` 构造的包装器（null 句柄）drop 时
            // 不应调用 CloseHandle。当前 `OwnedProcHandle::new` 调用方均传入有效
            // 句柄，此守卫为防御性兜底，防止后续维护误用。
            if !handle.is_invalid() {
                // SAFETY: CloseHandle 是线程安全的 Win32 API。handle 由
                // DuplicateHandle 创建，调用方独占持有，不存在竞争。
                unsafe {
                    let _ = CloseHandle(handle);
                }
            }
        }
    }
}

/// [Consistency]-12.2 修复：抽取共享的子进程退出监听函数。
///
/// W-001（web.rs）与 W-002（video.rs）修复各自内联实现了相同的子进程
/// 退出监听模式：`OwnedProcHandle` RAII 管理 → spawn 监听线程 →
/// `WaitForSingleObject(INFINITE)` → `CloseHandle` → 调用回调更新状态。
/// 本函数收敛两者差异，确保 `WebRenderer` 与 `VideoRenderer` 错误处理路径一致。
///
/// # 参数
/// - `handle`: 已通过 `DuplicateHandle` 复制的子进程句柄，由 `OwnedProcHandle` RAII 管理。
///   必须是副本（如 `SubprocessRendererBase::duplicate_process_handle` 的返回值），
///   不能是原始进程句柄，以避免与 `terminate()` 中的 `CloseHandle` 产生竞争
///   （Win32 文档明确指出在 wait 期间关闭同一句柄会导致未定义行为）。
/// - `callback`: 子进程退出后调用的回调。典型逻辑：检查 `shared_state.state !=
///   Terminated` 判定异常退出 → 更新状态为 `Terminated` → 调用
///   `PauseSender::notify_state_changed` 通知 Tauri 层刷新 UI。
///
/// # 返回
/// - `Some(JoinHandle)`: 监听线程已成功 spawn，调用方可选 `join()` 等待线程退出。
/// - `None`: spawn 失败（已记录 `warn` 日志）。`OwnedProcHandle` 随闭包 drop 自动
///   调用 `CloseHandle`，不泄漏句柄。此行为与 web.rs / video.rs 一致。
///
/// # 行为流程
/// 1. `thread::Builder::new().name("mirrorstar-proc-monitor").spawn()` 启动监听线程
/// 2. 线程内：`OwnedProcHandle::take()` 取出句柄 → `WaitForSingleObject(INFINITE)` →
///    `CloseHandle` → `callback()`
/// 3. spawn 失败：闭包 drop → `OwnedProcHandle::drop` → `CloseHandle`，返回 `None`
///
/// # 一致性保证
/// web.rs 与 video.rs 调用本函数后，错误处理路径完全一致：
/// - spawn 成功：句柄由监听线程关闭，回调在子进程退出后调用
/// - spawn 失败：句柄由 `OwnedProcHandle::drop` 关闭，`warn` 日志记录，不 panic
pub(crate) fn spawn_proc_exit_monitor<F>(
    mut handle: OwnedProcHandle,
    callback: F,
) -> Option<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("mirrorstar-proc-monitor".to_string())
        .spawn(move || {
            use windows::Win32::System::Threading::WaitForSingleObject;
            // 从 RAII 包装器取出句柄，转移所有权。取出后 handle 的 Drop
            // 不再调用 CloseHandle，由本闭包显式关闭。
            let proc_handle = match handle.take() {
                Some(h) => h,
                None => {
                    tracing::error!("OwnedProcHandle 内部句柄已被取走，监听线程退出");
                    return;
                }
            };
            // v41-W-001: 完整 match `WaitForSingleObject` 返回值，覆盖所有分支。
            //
            // 原实现仅判断 `WAIT_OBJECT_0`（隐式：忽略 `_wait_result`），未处理
            // `WAIT_FAILED` 与 `WAIT_TIMEOUT` 路径。虽然使用 INFINITE 超时理论不
            // 返回 `WAIT_TIMEOUT`，但 `WAIT_FAILED` 可能因句柄权限/有效性变化触发，
            // 原实现会错误地调用 callback 通知"子进程已退出"，导致 engine 状态被
            // 错误更新为 Terminated。
            //
            // 修复：用 `loop` + match 覆盖全部分支：
            // - `WAIT_OBJECT_0`：子进程已退出 → break 出循环，调用 callback 通知
            // - `WAIT_FAILED`：等待失败（句柄无效等）→ `error!` 记录并退出线程，
            //   不调用 callback（无法确认子进程是否真正退出，避免误通知）
            // - `WAIT_TIMEOUT`：超时未退出（INFINITE 下理论不触发，防御性处理）→
            //   继续等待循环
            // - 其他值：未预期的返回值 → `warn!` 记录并继续等待循环
            let exited_normally = loop {
                // INFINITE = 0xFFFFFFFF，无限等待子进程退出
                let wait_result = unsafe { WaitForSingleObject(proc_handle, u32::MAX) };
                match wait_result {
                    WAIT_OBJECT_0 => {
                        // v5.1 诊断：获取子进程退出码，帮助诊断"mpv 嵌入后崩溃"问题
                        let mut exit_code: u32 = 0;
                        let got_code = unsafe { GetExitCodeProcess(proc_handle, &mut exit_code) };
                        if got_code.is_ok() {
                            if exit_code == 0 {
                                tracing::info!(exit_code, "子进程正常退出（exit_code=0）");
                            } else {
                                tracing::warn!(
                                    exit_code,
                                    "子进程异常退出（exit_code 非 0，可能崩溃或被强杀）"
                                );
                            }
                        } else {
                            tracing::warn!(
                                error = ?std::io::Error::last_os_error(),
                                "GetExitCodeProcess 失败，无法获取子进程退出码"
                            );
                        }
                        break true;
                    }
                    WAIT_FAILED => {
                        tracing::error!(
                            error = ?std::io::Error::last_os_error(),
                            "WaitForSingleObject 失败（句柄无效或权限不足），监听线程退出但不通知子进程退出"
                        );
                        break false;
                    }
                    WAIT_TIMEOUT => {
                        // INFINITE 理论不返回 WAIT_TIMEOUT，防御性继续等待
                        tracing::warn!("WaitForSingleObject 意外返回 WAIT_TIMEOUT（INFINITE 不应触发），继续等待");
                        continue;
                    }
                    other => {
                        tracing::warn!(
                            ret = ?other,
                            "WaitForSingleObject 返回未预期值，继续等待"
                        );
                        continue;
                    }
                }
            };
            // 关闭复制的句柄（监听线程独有，必须释放避免句柄泄漏）
            unsafe {
                let _ = CloseHandle(proc_handle);
            }
            // 仅在子进程确实退出时（WAIT_OBJECT_0）调用回调，避免 WAIT_FAILED 误通知
            if exited_normally {
                // 调用回调（如检查 state、更新 shared_state、notify_state_changed）
                callback();
            }
        })
        .map_err(|e| {
            tracing::warn!(
                error = %e,
                "创建进程监听线程失败（子进程异常退出将无法感知）"
            );
        })
        .ok()
}

/// 壁纸状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WallpaperState {
    Initializing,
    Playing,
    Paused,
    Terminated,
    Error,
}

/// 壁纸缩放模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScalingMode {
    #[default]
    Fill,
    Fit,
    Stretch,
    Center,
    Original,
}

/// GIF 内存管理策略
///
/// 控制 GIF 渲染器在暂停时的内存管理行为，在内存占用和恢复性能之间取得平衡。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GifMemoryStrategy {
    /// 激进模式：暂停时释放所有帧数据，仅保留当前帧（内存占用最低，恢复需重新解码）
    Aggressive,
    /// 平衡模式：暂停时保留最近 N 帧，释放其他帧（内存与性能的折中）
    #[default]
    Balanced,
    /// 性能模式：暂停时保留所有帧数据，恢复最快（内存占用最高）
    Performance,
    /// 自适应模式：根据系统可用内存和 GIF 大小自动选择策略
    Adaptive,
}

/// 平衡模式下默认保留的帧数
pub const DEFAULT_BALANCED_KEEP_FRAMES: usize = 10;

// ── Pause Channel (Task 4: 快速暂停/恢复路径) ──────────────────────────────

/// 快速控制命令，绕过引擎互斥锁直接发送到渲染器
#[derive(Debug, Clone)]
pub enum PauseCommand {
    Pause,
    Resume,
    SetVolume(f32),
    ToggleMute,
}

/// 渲染器共享状态，可被快速路径读取而无需获取引擎锁
#[derive(Debug)]
pub struct RendererState {
    pub state: WallpaperState,
    pub volume: f32,
    pub pre_mute_volume: Option<f32>,
}

impl Default for RendererState {
    fn default() -> Self {
        Self {
            state: WallpaperState::Initializing,
            volume: 1.0,
            pre_mute_volume: None,
        }
    }
}

/// 快速控制发送端，用于绕过引擎互斥锁发送暂停/恢复/音量命令
#[derive(Clone)]
pub struct PauseSender {
    tx: tokio::sync::mpsc::UnboundedSender<PauseCommand>,
    shared_state: Arc<RwLock<RendererState>>,
    /// 状态变更通知通道（修复）
    ///
    /// pause 线程在更新 `shared_state.state` 后，通过此 broadcast 通道
    /// 发送 `display_id`，订阅方（Tauri 层）接收后 emit `wallpaper-state-changed`
    /// 事件以刷新前端 UI。容量 16：broadcast 通道满时新消息会替换最旧的
    /// 未读消息（lag by），不会阻塞 sender，适合短时间内的多次状态变更通知。
    state_changed: tokio::sync::broadcast::Sender<String>,
    /// 状态版本号（v41-W-005 修复）
    ///
    /// 单调递增计数器，每次 `set_state` 调用时自增。用于解决 resume 失败回滚
    /// state 为 `Paused` 后，前端 UI 状态可能与 engine state 短暂不一致的问题：
    /// 前端通知 payload 携带 version，前端丢弃 version 旧的事件。
    ///
    /// **使用方式**：
    /// - 调用方在 `notify_state_changed` 后通过 `state_version()` 读取当前版本号
    /// - 前端订阅 state_changed 通道时记录最后处理的 version，丢弃小于已处理版本的事件
    /// - 由于 `state_changed` 当前 payload 类型为 `String`（display_id），
    ///   version 通过 `state_version()` 旁路查询，调用方在收到 broadcast 后
    ///   调用 `state_version()` 获取当前版本号
    ///
    /// **内存模型**：`AtomicU64` + `SeqCst` ordering 保证多线程可见性与单调性。
    /// `Arc` 共享使 clone 的 `PauseSender` 看到同一计数器（与 `shared_state` 一致）。
    state_version: Arc<AtomicU64>,
}

impl PauseSender {
    /// 发送快速控制命令
    pub fn send(&self, cmd: PauseCommand) -> Result<(), crate::MirrorStarError> {
        self.tx
            .send(cmd)
            .map_err(|e| crate::MirrorStarError::DesktopIntegration(e.to_string()))
    }

    /// 订阅状态变更通知（修复）
    ///
    /// 每次调用返回一个新的 `broadcast::Receiver<String>`，payload 为
    /// `display_id`。Tauri 层在应用启动时调用一次（或通过
    /// `WallpaperEngine::subscribe_state_changes` 批量订阅），spawn tokio
    /// task 持续接收并 emit `wallpaper-state-changed` 事件。
    ///
    /// 注意：broadcast 通道的 `subscribe()` 每次返回新的 receiver，
    /// 仅接收订阅之后发送的消息（不接收历史消息）。
    pub fn subscribe_state_changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.state_changed.subscribe()
    }

    /// 通知状态变更（修复）
    ///
    /// 由 pause 线程在更新 `shared_state.state` 后调用，发送 `display_id`
    /// 到 broadcast 通道。无订阅者时 `send` 返回 `Err`，此处静默忽略
    /// （状态变更仍已写入 `shared_state`，只是不触发 UI 刷新）。
    pub fn notify_state_changed(&self, display_id: &str) {
        let _ = self.state_changed.send(display_id.to_string());
    }

    /// 读取当前渲染器共享状态
    pub fn state(&self) -> WallpaperState {
        self.shared_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .state
    }

    /// 读取当前音量
    pub fn volume(&self) -> f32 {
        self.shared_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .volume
    }

    /// 更新共享状态中的壁纸状态
    ///
    /// # v41-W-005 修复
    ///
    /// 每次调用 `set_state` 时自增 `state_version` 计数器，使前端能够识别
    /// 最新状态变更并丢弃旧版本的事件（解决 resume 失败回滚 state 为 `Paused`
    /// 后前端 UI 状态与 engine state 短暂不一致的问题）。
    ///
    /// 调用方典型流程：
    /// 1. `sender.set_state(new_state)` — 更新状态 + 自增 version
    /// 2. `sender.notify_state_changed(display_id)` — 通知前端
    /// 3. 前端在收到通知后通过 `sender.state_version()` 读取当前版本号，
    ///    与本地已处理版本比较，丢弃小于已处理版本的事件
    pub fn set_state(&self, state: WallpaperState) {
        self.shared_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .state = state;
        // v41-W-005: 自增状态版本号，使前端能识别最新状态变更
        // SeqCst ordering 保证多线程下的严格顺序可见性
        self.state_version.fetch_add(1, Ordering::SeqCst);
    }

    /// 读取当前状态版本号（v41-W-005 修复）
    ///
    /// 单调递增的版本号，每次 `set_state` 调用后自增。前端在收到
    /// `notify_state_changed` 通知后调用此方法获取当前版本号，与本地
    /// 已处理版本比较：
    /// - 若 `current_version > local_version`：处理事件并更新 `local_version`
    /// - 若 `current_version <= local_version`：丢弃事件（旧版本，可能由
    ///   resume 失败回滚导致）
    ///
    /// 返回 `u64`，初始值为 0，首次 `set_state` 后变为 1。
    pub fn state_version(&self) -> u64 {
        self.state_version.load(Ordering::SeqCst)
    }

    /// 更新共享状态中的音量
    pub fn set_volume(&self, volume: f32) {
        self.shared_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .volume = volume;
    }

    /// 读取当前是否静音（修复）
    ///
    /// 通过 `pre_mute_volume.is_some()` 判断，比 `volume() <= 0.0` 可靠：
    /// 静音时 shared_state.volume 不更新（保持原值），仅 pre_mute_volume 被设为 Some。
    pub fn is_muted(&self) -> bool {
        self.shared_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .pre_mute_volume
            .is_some()
    }

    /// 原子化切换静音状态（N-005 修复 TOCTOU 竞态）
    ///
    /// 在 `shared_state` 写锁内完成 "读取当前静音状态 → 翻转 pre_mute_volume →
    /// 发送 ToggleMute 命令"，确保多个并发 `toggle_mute_fast` 调用串行化，
    /// 避免两个调用同时读到 "未静音" 状态、同时发送 ToggleMute 导致最终状态
    /// 与返回值不一致。
    ///
    /// 返回值：`Ok(true)` 表示已切换到静音，`Ok(false)` 表示已切换到未静音。
    /// 若发送命令失败，状态回滚（不翻转 pre_mute_volume）。
    ///
    /// 注意：本方法在 `shared_state` 中维护 `pre_mute_volume` 字段，
    /// 渲染器的 ToggleMute 处理逻辑不应再修改 `shared_state.pre_mute_volume`
    /// （渲染器内部维护各自的 audio_state.pre_mute_volume 用于实际音量恢复）。
    pub fn toggle_mute_atomic(&self) -> Result<bool, crate::MirrorStarError> {
        let mut state = self.shared_state.write().unwrap_or_else(|e| e.into_inner());
        let was_muted = state.pre_mute_volume.is_some();
        // 发送命令（mpsc unbounded send 不阻塞，可安全持锁）
        self.send(PauseCommand::ToggleMute)?;
        // 发送成功后再翻转状态
        if was_muted {
            // 当前已静音 → 取消静音
            state.pre_mute_volume = None;
            Ok(false)
        } else {
            // 当前未静音 → 静音（保存当前音量以便将来恢复）
            state.pre_mute_volume = Some(state.volume);
            Ok(true)
        }
    }
}

/// 创建 PauseSender 和对应的接收端及共享状态
///
/// 内部同时创建状态变更通知的 broadcast 通道（容量 16），其 `tx` 存入
/// `PauseSender.state_changed` 字段，`rx` 不返回——订阅方通过
/// `PauseSender::subscribe_state_changes` 获取新的 receiver。
/// 详见修复说明。
pub fn create_pause_channel() -> (
    PauseSender,
    tokio::sync::mpsc::UnboundedReceiver<PauseCommand>,
    Arc<RwLock<RendererState>>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // 状态变更通知通道：容量 16，broadcast 满时新消息替换最旧未读消息，不阻塞 sender
    let (state_tx, _state_rx) = tokio::sync::broadcast::channel::<String>(16);
    let shared_state = Arc::new(RwLock::new(RendererState::default()));
    let sender = PauseSender {
        tx,
        shared_state: shared_state.clone(),
        state_changed: state_tx,
        // v41-W-005: 初始化状态版本号为 0，首次 set_state 后变为 1
        state_version: Arc::new(AtomicU64::new(0)),
    };
    (sender, rx, shared_state)
}

/// 从已有的 `shared_state` 和 `state_changed` 构造 PauseSender（W-003 修复）
///
/// 与 `create_pause_channel` 不同，此函数复用提前创建的 `shared_state` 和
/// `state_changed` 通道，仅创建新的命令通道（mpsc tx/rx）。
///
/// 用途：image/gif 渲染器在 `play()` 中提前创建 `shared_state` 和 `state_changed`
/// 并传给壁纸线程，使壁纸线程在 Resume 失败时能通过 `set_state` 回滚状态并通知。
/// `create_pause_sender()` 中调用此函数复用已有组件，避免创建重复的 shared_state。
///
/// # v41-W-005
///
/// `state_version` 字段始终新建（`Arc<AtomicU64>::new(0)`），不与调用方共享。
/// 原因：`state_version` 是 `PauseSender` 的内部状态，调用方提供的 `shared_state`
/// 与 `state_changed` 是被复用的共享资源，但 version 计数器是 per-sender 的
/// （每个 PauseSender 实例对应一个渲染器的状态版本）。
pub fn create_pause_sender_with_state(
    shared_state: Arc<RwLock<RendererState>>,
    state_changed: tokio::sync::broadcast::Sender<String>,
) -> (
    PauseSender,
    tokio::sync::mpsc::UnboundedReceiver<PauseCommand>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sender = PauseSender {
        tx,
        shared_state,
        state_changed,
        // v41-W-005: 初始化状态版本号为 0
        state_version: Arc::new(AtomicU64::new(0)),
    };
    (sender, rx)
}

/// 暂停原因位掩码，用于协调 fullscreen / power / tray 多状态机
///
/// 当多个暂停原因同时存在时，仅当全部原因都清除后才真正恢复壁纸。
/// 例如：电池供电暂停期间退出全屏，不应恢复壁纸（电池原因仍活跃）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseReason(pub u8);

impl PauseReason {
    pub const FULLSCREEN: Self = Self(0b0001);
    pub const BATTERY: Self = Self(0b0010);
    pub const TRAY: Self = Self(0b0100);

    /// 无任何暂停原因
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 是否包含指定原因
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for PauseReason {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl std::ops::BitOrAssign for PauseReason {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl std::ops::BitAndAssign for PauseReason {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}
impl std::ops::Not for PauseReason {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

/// 壁纸渲染器 trait，定义壁纸生命周期管理接口
///
/// # 设计目标（v41-W-017）
///
/// 统一壁纸渲染接口，使 `WallpaperEngine` 可以多态管理不同类型的壁纸渲染器
/// （`GifRenderer` / `VideoRenderer` / `WebRenderer` / `ImageRenderer`），无需在
/// engine 层针对每种壁纸类型编写分支逻辑。engine 仅持有 `Box<dyn WallpaperRenderer>`
/// 并通过 trait 方法驱动生命周期（play → pause/resume → terminate）与属性配置
/// （volume / speed / scaling_mode / position）。
///
/// # 实现指南
///
/// 各具体渲染器实现该 trait，并按以下规则处理方法默认实现：
/// - **必须实现**：`play` / `pause` / `resume` / `set_position` / `terminate` /
///   `hwnd` / `state` — 所有渲染器共有的生命周期与状态查询
/// - **可选覆盖**：`set_speed`（仅 `VideoRenderer` / `GifRenderer` 有效）/
///   `set_scaling_mode` / `set_mouse_passthrough` /
///   `set_interaction_mode` / `create_pause_sender`
///
/// # 方法命名约定（v41-W-017）
///
/// trait 方法采用两种命名风格，新方法应遵循对应约定：
/// - **状态切换用动词原形**：`play` / `pause` / `resume` / `terminate`
///   （驱动壁纸状态机转换，返回 `Result` 表示转换成败）
/// - **属性设置用 `set_xxx`**：`set_speed` / `set_scaling_mode` /
///   `set_position` / `set_mouse_passthrough` / `set_interaction_mode`
///   （修改渲染器属性，多为 no-op 默认实现，仅对应类型覆盖）
///
/// 此约定保持向后兼容，未统一为 `set_state(Playing)` 形式以避免破坏现有调用方。
pub trait WallpaperRenderer: Send {
    /// 开始播放壁纸
    fn play(&mut self) -> Result<(), crate::MirrorStarError>;
    /// 暂停壁纸
    fn pause(&mut self) -> Result<(), crate::MirrorStarError>;
    /// 全屏场景下的"暂停"处置。默认实现 = 普通暂停（图片/GIF 无子进程，pause 即可）。
    /// 视频/网页渲染器覆写为终止子进程，最大化释放 CPU/GPU 内存。
    fn pause_for_fullscreen(&mut self) -> Result<(), crate::MirrorStarError> {
        self.pause()
    }
    /// 恢复壁纸
    fn resume(&mut self) -> Result<(), crate::MirrorStarError>;
    /// 设置壁纸窗口位置和大小
    fn set_position(&mut self, x: i32, y: i32, w: i32, h: i32)
        -> Result<(), crate::MirrorStarError>;
    /// 终止壁纸渲染
    fn terminate(&mut self) -> Result<(), crate::MirrorStarError>;
    /// 获取壁纸窗口句柄
    fn hwnd(&self) -> Option<HWND>;
    /// 获取当前壁纸状态
    fn state(&self) -> WallpaperState;
    /// 设置播放速度（0.25~4.0，仅视频壁纸有效，默认 no-op）
    fn set_speed(&mut self, _speed: f32) {}
    /// 设置缩放模式
    fn set_scaling_mode(&mut self, _mode: ScalingMode) {}
    /// 设置鼠标穿透
    fn set_mouse_passthrough(&mut self, _enabled: bool) {}
    /// 设置交互模式
    fn set_interaction_mode(&mut self, _enabled: bool) {}
    /// 创建快速控制发送端（在 play() 成功后调用）
    ///
    /// `display_id` 为该渲染器所属的显示器 ID，pause 线程在状态变更后通过
    /// `PauseSender::notify_state_changed(display_id)` 通知 Tauri 层 emit
    /// `wallpaper-state-changed` 事件（修复）。
    fn create_pause_sender(&mut self, _display_id: &str) -> Option<PauseSender> {
        None
    }

    /// 嵌入 WorkerW 壁纸层后调用（默认 no-op）
    ///
    /// 视频壁纸使用此钩子在窗口嵌入完成后再通过 IPC `loadfile` 加载视频文件，
    /// 避免 mpv 在窗口被 `SetParent` 重父化 + `SetWindowPos` 缩放前创建视频纹理，
    /// 导致 D3D11 纹理创建失败（`E_OUTOFMEMORY 0x8007000e`）→ 桌面黑屏（根因 E）。
    ///
    /// 返回 `Err` 时调用方（`embed_and_register_renderer`）应终止渲染器并回滚。
    fn after_embed(&mut self) -> Result<(), crate::MirrorStarError> {
        Ok(())
    }
}

/// 壁纸类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WallpaperType {
    Video,
    Gif,
    Web,
    Image,
}

/// 壁纸来源
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WallpaperSource {
    File(String),
    Url(String),
}

/// 校验壁纸播放速度是否为合法值（W-007 修复，合并 [Consistency]-12.1）
///
/// 与 `GifRenderer::set_speed`（W10）保持一致：speed 必须为有限数且大于 0。
/// - `speed <= 0` 在语义上等同于暂停，但调用方应显式调用 `pause()`；此处拒绝
/// - `NaN/Infinity` 会导致下游计算（如 `delay / speed`、mpv IPC 速度参数）产生异常值
///
/// 返回 `true` 表示合法，`false` 表示非法（调用方应记录日志并跳过下游操作）。
pub(crate) fn validate_renderer_speed(speed: f32) -> bool {
    speed.is_finite() && speed > 0.0
}

/// 计算壁纸缩放后的绘制位置和尺寸
///
/// 返回 (x, y, width, height) 绘制坐标
pub fn calculate_scaling(
    img_w: u32,
    img_h: u32,
    win_w: u32,
    win_h: u32,
    mode: ScalingMode,
) -> (i32, i32, i32, i32) {
    match mode {
        ScalingMode::Fill => {
            let scale_x = win_w as f64 / img_w as f64;
            let scale_y = win_h as f64 / img_h as f64;
            let scale = scale_x.max(scale_y);
            let draw_w = (img_w as f64 * scale) as i32;
            let draw_h = (img_h as f64 * scale) as i32;
            let draw_x = (win_w as i32 - draw_w) / 2;
            let draw_y = (win_h as i32 - draw_h) / 2;
            (draw_x, draw_y, draw_w, draw_h)
        }
        ScalingMode::Fit => {
            let scale_x = win_w as f64 / img_w as f64;
            let scale_y = win_h as f64 / img_h as f64;
            let scale = scale_x.min(scale_y);
            let draw_w = (img_w as f64 * scale) as i32;
            let draw_h = (img_h as f64 * scale) as i32;
            let draw_x = (win_w as i32 - draw_w) / 2;
            let draw_y = (win_h as i32 - draw_h) / 2;
            (draw_x, draw_y, draw_w, draw_h)
        }
        ScalingMode::Stretch => (0, 0, win_w as i32, win_h as i32),
        ScalingMode::Center => {
            let draw_x = (win_w as i32 - img_w as i32) / 2;
            let draw_y = (win_h as i32 - img_h as i32) / 2;
            (draw_x, draw_y, img_w as i32, img_h as i32)
        }
        ScalingMode::Original => (0, 0, img_w as i32, img_h as i32),
    }
}

/// 判断绘制矩形是否完全覆盖客户区（#6：用于跳过冗余的 FillRect 黑底填充）
///
/// 当绘制矩形 `[draw_x, draw_x+draw_w) × [draw_y, draw_y+draw_h)` 完全覆盖
/// 客户区 `[0, client_w) × [0, client_h)` 时返回 `true`。此时 StretchDIBits/
/// StretchBlt 会覆写全部客户区像素，前置的 FillRect 黑底被完全覆盖，可跳过
/// 以省一次全屏 GDI 填充。Fill/Stretch 覆盖模式返回 `true`；Fit/Center/Original
/// 等留黑边模式（绘制矩形小于客户区）返回 `false`，仍需 FillRect 填充 letterbox。
pub(crate) fn draw_rect_covers_full(
    draw_x: i32,
    draw_y: i32,
    draw_w: i32,
    draw_h: i32,
    client_w: i32,
    client_h: i32,
) -> bool {
    draw_x <= 0
        && draw_y <= 0
        && draw_x + draw_w >= client_w
        && draw_y + draw_h >= client_h
}

pub mod fast_path;
pub mod gdi_base;
pub mod gdi_cache;
pub mod gif;
pub mod gif_decode;
pub mod gif_memory;
pub mod image;
pub mod manager;
pub mod mode_dispatch;
pub mod subprocess_base;
pub mod video;
pub mod web;

#[cfg(test)]
mod tests {
    use super::*;

    // ========== calculate_scaling tests ==========

    #[test]
    fn fill_exact_fit() {
        let (x, y, w, h) = calculate_scaling(1920, 1080, 1920, 1080, ScalingMode::Fill);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));
    }

    #[test]
    fn fill_scale_up() {
        let (x, y, w, h) = calculate_scaling(1920, 1080, 3840, 2160, ScalingMode::Fill);
        assert_eq!((x, y, w, h), (0, 0, 3840, 2160));
    }

    #[test]
    fn fill_wider_image_crops_horizontally() {
        // 2560x1080 is wider than 1920x1080 window → scale to cover, crop sides
        let (x, y, w, h) = calculate_scaling(2560, 1080, 1920, 1080, ScalingMode::Fill);
        assert_eq!((x, y, w, h), (-320, 0, 2560, 1080));
    }

    #[test]
    fn fill_taller_image_crops_vertically() {
        // 1080x1920 is taller than 1920x1080 window → scale to cover, crop top/bottom
        let (x, y, w, h) = calculate_scaling(1080, 1920, 1920, 1080, ScalingMode::Fill);
        assert_eq!((x, y, w, h), (0, -1166, 1920, 3413));
    }

    #[test]
    fn fit_exact_fit() {
        let (x, y, w, h) = calculate_scaling(1920, 1080, 1920, 1080, ScalingMode::Fit);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));
    }

    #[test]
    fn fit_scale_up() {
        let (x, y, w, h) = calculate_scaling(1920, 1080, 3840, 2160, ScalingMode::Fit);
        assert_eq!((x, y, w, h), (0, 0, 3840, 2160));
    }

    #[test]
    fn fit_wider_image_bars_top_bottom() {
        // 2560x1080 is wider than 1920x1080 window → scale down to fit width, bars top/bottom
        let (x, y, w, h) = calculate_scaling(2560, 1080, 1920, 1080, ScalingMode::Fit);
        assert_eq!((x, y, w, h), (0, 135, 1920, 810));
    }

    #[test]
    fn fit_taller_image_bars_left_right() {
        // 1080x1920 is taller than 1920x1080 window → scale down to fit height, bars left/right
        let (x, y, w, h) = calculate_scaling(1080, 1920, 1920, 1080, ScalingMode::Fit);
        assert_eq!((x, y, w, h), (656, 0, 607, 1080));
    }

    #[test]
    fn stretch_any_image() {
        let (x, y, w, h) = calculate_scaling(800, 600, 1920, 1080, ScalingMode::Stretch);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));

        let (x, y, w, h) = calculate_scaling(2560, 1440, 1920, 1080, ScalingMode::Stretch);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));
    }

    #[test]
    fn center_exact_fit() {
        let (x, y, w, h) = calculate_scaling(1920, 1080, 1920, 1080, ScalingMode::Center);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));
    }

    #[test]
    fn center_smaller_image() {
        let (x, y, w, h) = calculate_scaling(800, 600, 1920, 1080, ScalingMode::Center);
        assert_eq!((x, y, w, h), (560, 240, 800, 600));
    }

    #[test]
    fn center_larger_image() {
        let (x, y, w, h) = calculate_scaling(2560, 1440, 1920, 1080, ScalingMode::Center);
        assert_eq!((x, y, w, h), (-320, -180, 2560, 1440));
    }

    #[test]
    fn original_any_image() {
        let (x, y, w, h) = calculate_scaling(800, 600, 1920, 1080, ScalingMode::Original);
        assert_eq!((x, y, w, h), (0, 0, 800, 600));

        let (x, y, w, h) = calculate_scaling(2560, 1440, 1920, 1080, ScalingMode::Original);
        assert_eq!((x, y, w, h), (0, 0, 2560, 1440));
    }

    // ========== draw_rect_covers_full tests (#6) ==========

    #[test]
    fn covers_full_fill_mode_always_covers() {
        // Fill 覆盖模式：无论图源比例，绘制矩形都完全覆盖客户区（exact / 宽图裁左右 / 高图裁上下）
        for (img_w, img_h) in [(1920u32, 1080u32), (2560, 1080), (1080, 1920)] {
            let (dx, dy, dw, dh) = calculate_scaling(img_w, img_h, 1920, 1080, ScalingMode::Fill);
            assert!(
                draw_rect_covers_full(dx, dy, dw, dh, 1920, 1080),
                "Fill 应覆盖: img={img_w}x{img_h} draw=({dx},{dy},{dw},{dh})"
            );
        }
    }

    #[test]
    fn covers_full_stretch_mode_always_covers() {
        // Stretch 拉伸模式：绘制矩形恒为客户区尺寸 → 覆盖
        for (img_w, img_h) in [(800u32, 600u32), (2560, 1440), (1920, 1080)] {
            let (dx, dy, dw, dh) = calculate_scaling(img_w, img_h, 1920, 1080, ScalingMode::Stretch);
            assert!(draw_rect_covers_full(dx, dy, dw, dh, 1920, 1080));
        }
    }

    #[test]
    fn covers_full_fit_with_bars_not_covered() {
        // Fit 留黑边：宽图留上下黑边、高图留左右黑边 → 未覆盖（需 FillRect）
        let (dx, dy, dw, dh) = calculate_scaling(2560, 1080, 1920, 1080, ScalingMode::Fit);
        assert!(!draw_rect_covers_full(dx, dy, dw, dh, 1920, 1080));
        let (dx, dy, dw, dh) = calculate_scaling(1080, 1920, 1920, 1080, ScalingMode::Fit);
        assert!(!draw_rect_covers_full(dx, dy, dw, dh, 1920, 1080));
    }

    #[test]
    fn covers_full_center_smaller_not_covered() {
        // Center 小图居中：四周留黑边 → 未覆盖
        let (dx, dy, dw, dh) = calculate_scaling(800, 600, 1920, 1080, ScalingMode::Center);
        assert!(!draw_rect_covers_full(dx, dy, dw, dh, 1920, 1080));
    }

    #[test]
    fn covers_full_one_pixel_short_not_covered() {
        // 保守边界：宽度少 1 像素 → 未覆盖（保留 FillRect 避免漏填 1px 黑线）
        assert!(!draw_rect_covers_full(0, 0, 1919, 1080, 1920, 1080));
    }

    // ========== Enum serialization tests ==========

    #[test]
    fn wallpaper_state_serde_roundtrip() {
        let variants = [
            WallpaperState::Initializing,
            WallpaperState::Playing,
            WallpaperState::Paused,
            WallpaperState::Terminated,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: WallpaperState = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn scaling_mode_serde_roundtrip() {
        let variants = [
            ScalingMode::Fill,
            ScalingMode::Fit,
            ScalingMode::Stretch,
            ScalingMode::Center,
            ScalingMode::Original,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: ScalingMode = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn scaling_mode_default_is_fill() {
        assert_eq!(ScalingMode::default(), ScalingMode::Fill);
    }

    #[test]
    fn scaling_mode_serde_lowercase() {
        for (json, expected) in [
            ("\"fill\"", ScalingMode::Fill),
            ("\"fit\"", ScalingMode::Fit),
            ("\"stretch\"", ScalingMode::Stretch),
            ("\"center\"", ScalingMode::Center),
            ("\"original\"", ScalingMode::Original),
        ] {
            let decoded: ScalingMode = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&expected).unwrap(), json);
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn wallpaper_type_serde_roundtrip() {
        let variants = [
            WallpaperType::Video,
            WallpaperType::Gif,
            WallpaperType::Web,
            WallpaperType::Image,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: WallpaperType = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn wallpaper_source_serde_roundtrip() {
        let variants = [
            WallpaperSource::File("/path/to/file.mp4".to_string()),
            WallpaperSource::Url("https://example.com/video.mp4".to_string()),
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: WallpaperSource = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    // ========== GifMemoryStrategy tests ==========

    #[test]
    fn gif_memory_strategy_serde_roundtrip() {
        let variants = [
            GifMemoryStrategy::Aggressive,
            GifMemoryStrategy::Balanced,
            GifMemoryStrategy::Performance,
            GifMemoryStrategy::Adaptive,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: GifMemoryStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn gif_memory_strategy_default_is_balanced() {
        assert_eq!(GifMemoryStrategy::default(), GifMemoryStrategy::Balanced);
    }

    // ========== PauseSender tests ==========

    #[test]
    fn pause_sender_create_and_read_state() {
        let (sender, _rx, _shared_state) = create_pause_channel();

        // Default state should be Initializing
        assert_eq!(sender.state(), WallpaperState::Initializing);

        // Default volume should be 1.0
        assert!((sender.volume() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pause_sender_update_state() {
        let (sender, _rx, _shared_state) = create_pause_channel();

        sender.set_state(WallpaperState::Playing);
        assert_eq!(sender.state(), WallpaperState::Playing);

        sender.set_state(WallpaperState::Paused);
        assert_eq!(sender.state(), WallpaperState::Paused);
    }

    #[test]
    fn pause_sender_update_volume() {
        let (sender, _rx, _shared_state) = create_pause_channel();

        sender.set_volume(0.5);
        assert!((sender.volume() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn pause_sender_send_command() {
        let (sender, mut rx, _shared_state) = create_pause_channel();

        sender.send(PauseCommand::Pause).unwrap();
        let cmd = rx.blocking_recv();
        assert!(matches!(cmd, Some(PauseCommand::Pause)));

        sender.send(PauseCommand::Resume).unwrap();
        let cmd = rx.blocking_recv();
        assert!(matches!(cmd, Some(PauseCommand::Resume)));
    }

    #[test]
    fn pause_sender_send_volume_command() {
        let (sender, mut rx, _shared_state) = create_pause_channel();

        sender.send(PauseCommand::SetVolume(0.7)).unwrap();
        let cmd = rx.blocking_recv();
        match cmd {
            Some(PauseCommand::SetVolume(v)) => {
                assert!((v - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected SetVolume(0.7), got {:?}", other),
        }
    }

    #[test]
    fn pause_sender_send_toggle_mute_command() {
        let (sender, mut rx, _shared_state) = create_pause_channel();

        sender.send(PauseCommand::ToggleMute).unwrap();
        let cmd = rx.blocking_recv();
        assert!(matches!(cmd, Some(PauseCommand::ToggleMute)));
    }

    #[test]
    fn pause_sender_clone_and_share_state() {
        let (sender, _rx, _shared_state) = create_pause_channel();
        let sender_clone = sender.clone();

        sender.set_state(WallpaperState::Playing);
        assert_eq!(sender_clone.state(), WallpaperState::Playing);

        sender_clone.set_volume(0.3);
        assert!((sender.volume() - 0.3).abs() < f32::EPSILON);
    }

    // ========== 状态变更 broadcast 通道测试 ==========

    #[test]
    fn pause_sender_subscribe_receives_state_change() {
        // 订阅后调用 notify_state_changed，receiver 应收到 display_id
        let (sender, _rx, _shared_state) = create_pause_channel();
        let mut state_rx = sender.subscribe_state_changes();

        sender.notify_state_changed("monitor_0");
        let received = state_rx.blocking_recv();
        assert_eq!(received.as_deref(), Ok("monitor_0"));
    }

    #[test]
    fn pause_sender_subscribe_before_send_receives_message() {
        // 先订阅再发送：订阅之后的消息可被接收（broadcast 仅接收订阅后的消息）
        let (sender, _rx, _shared_state) = create_pause_channel();

        // 订阅前发送的消息不应被新 receiver 接收
        sender.notify_state_changed("before_subscribe");

        let mut state_rx = sender.subscribe_state_changes();
        sender.notify_state_changed("after_subscribe");

        let received = state_rx.blocking_recv();
        assert_eq!(received.as_deref(), Ok("after_subscribe"));
    }

    #[test]
    fn pause_sender_multiple_subscribers_all_receive() {
        // 多个订阅者都应收到同一条状态变更通知（broadcast 语义）
        let (sender, _rx, _shared_state) = create_pause_channel();
        let mut rx1 = sender.subscribe_state_changes();
        let mut rx2 = sender.subscribe_state_changes();

        sender.notify_state_changed("monitor_1");

        assert_eq!(rx1.blocking_recv().as_deref(), Ok("monitor_1"));
        assert_eq!(rx2.blocking_recv().as_deref(), Ok("monitor_1"));
    }

    #[test]
    fn pause_sender_clone_shares_broadcast_channel() {
        // clone 的 PauseSender 共享同一 broadcast 通道（clone 订阅也能收到原 sender 发的消息）
        let (sender, _rx, _shared_state) = create_pause_channel();
        let sender_clone = sender.clone();
        let mut state_rx = sender_clone.subscribe_state_changes();

        sender.notify_state_changed("monitor_2");
        assert_eq!(state_rx.blocking_recv().as_deref(), Ok("monitor_2"));
    }

    #[test]
    fn pause_sender_notify_without_subscriber_is_silent() {
        // 无订阅者时 notify_state_changed 不应 panic（send 返回 Err 被静默忽略）
        let (sender, _rx, _shared_state) = create_pause_channel();
        sender.notify_state_changed("monitor_3");
        // 无断言：仅验证不 panic
    }

    // ========== [Consistency]-12.2 修复测试：spawn_proc_exit_monitor 共享函数 ==========

    /// 验证 `spawn_proc_exit_monitor` 在子进程退出时调用回调。
    ///
    /// [Consistency]-12.2 抽取共享函数收敛 web.rs（W-001/W07）与 video.rs（W-002）
    /// 的子进程退出监听实现。此测试验证共享函数的核心行为：调用后子进程退出时
    /// callback 被调用。
    ///
    /// 测试流程：
    /// 1. 启动短生命周期占位进程（cmd.exe + ping，~1s 后自行退出）
    /// 2. 复制进程句柄并包装为 `OwnedProcHandle`
    /// 3. 调用 `spawn_proc_exit_monitor`，回调内通过 `Arc<AtomicBool>` 标记回调已被调用
    /// 4. 轮询等待回调执行（最多 10s）
    /// 5. 验证回调标志为 true，且 `JoinHandle` 可正常 join
    ///
    /// 此测试覆盖 web.rs 与 video.rs 共享的崩溃检测路径，确保重构后行为不变。
    #[test]
    fn consistency_12_2_spawn_proc_exit_monitor_invokes_callback_on_exit() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::wallpaper::subprocess_base::SubprocessRendererBase;

        // 定位 cmd.exe（优先使用 SystemRoot 环境变量，回退到 WINDIR）
        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("WINDIR"))
            .expect("SystemRoot/WINDIR 环境变量应存在");
        let cmd_path = PathBuf::from(system_root).join("System32").join("cmd.exe");
        assert!(cmd_path.exists(), "cmd.exe 应存在于 {}", cmd_path.display());

        // 启动短生命周期进程：ping -n 2 127.0.0.1（~1s 后退出）
        let mut base = SubprocessRendererBase::new(
            cmd_path,
            "test-consistency-12-2-pipe".to_string(),
            ScalingMode::Fill,
        );
        base.start_process(vec!["/c".to_string(), "ping -n 2 127.0.0.1".to_string()])
            .expect("启动测试占位进程应成功");

        // 复制进程句柄（与 create_pause_sender 中的模式一致）
        let proc_handle = base
            .duplicate_process_handle()
            .expect("duplicate_process_handle 应返回 Some");
        let owned = OwnedProcHandle::new(proc_handle);

        // 回调标志：标记 callback 是否被调用
        let callback_invoked = Arc::new(AtomicBool::new(false));
        let callback_flag = callback_invoked.clone();

        // 调用共享函数 spawn 监听线程
        let monitor_handle = spawn_proc_exit_monitor(owned, move || {
            callback_flag.store(true, Ordering::SeqCst);
        })
        .expect("spawn_proc_exit_monitor 应成功 spawn 监听线程");

        // 轮询等待回调被调用（最多 10s）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if callback_invoked.load(Ordering::SeqCst) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("子进程退出后 callback 应被调用，但等待 10s 后仍未调用");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // 验证回调已被调用
        assert!(
            callback_invoked.load(Ordering::SeqCst),
            "子进程退出后 callback 应被调用"
        );

        // 等待监听线程退出（callback 执行完毕后线程应立即退出）
        monitor_handle.join().expect("监听线程应正常退出");

        // 清理子进程（stop_process 对已退出进程会立即返回）
        let _ = base.stop_process();
    }

    // ========== v41-W-001 修复测试：spawn_proc_exit_monitor 全分支 match ==========

    /// 验证 `spawn_proc_exit_monitor` 在 `WAIT_OBJECT_0` 路径下正确调用回调。
    ///
    /// v41-W-001 修复：原实现仅判断 `WAIT_OBJECT_0`（忽略 `_wait_result`），
    /// 未处理 `WAIT_FAILED` 与 `WAIT_TIMEOUT` 路径。修复后用 `loop` + match
    /// 覆盖全部分支，仅在 `WAIT_OBJECT_0` 时调用 callback。
    ///
    /// 本测试验证正常路径（`WAIT_OBJECT_0`）下 callback 仍被正确调用，
    /// 确保重构未破坏正常行为。`WAIT_FAILED` 路径需 mock 句柄失效场景，
    /// 因难以稳定复现（关闭句柄后 OS 可能复用句柄值）仅在文档中说明。
    #[test]
    fn v41_w001_wait_object_0_path_invokes_callback() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::wallpaper::subprocess_base::SubprocessRendererBase;

        // 定位 cmd.exe
        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("WINDIR"))
            .expect("SystemRoot/WINDIR 环境变量应存在");
        let cmd_path = PathBuf::from(system_root).join("System32").join("cmd.exe");
        assert!(cmd_path.exists(), "cmd.exe 应存在于 {}", cmd_path.display());

        // 启动短生命周期进程：cmd /c "exit 0"（立即退出，触发 WAIT_OBJECT_0）
        let mut base = SubprocessRendererBase::new(
            cmd_path,
            "test-v41-w001-pipe".to_string(),
            ScalingMode::Fill,
        );
        base.start_process(vec!["/c".to_string(), "exit 0".to_string()])
            .expect("启动测试占位进程应成功");

        // 复制进程句柄
        let proc_handle = base
            .duplicate_process_handle()
            .expect("duplicate_process_handle 应返回 Some");
        let owned = OwnedProcHandle::new(proc_handle);

        // 回调标志
        let callback_invoked = Arc::new(AtomicBool::new(false));
        let callback_flag = callback_invoked.clone();

        // spawn 监听线程
        let monitor_handle = spawn_proc_exit_monitor(owned, move || {
            callback_flag.store(true, Ordering::SeqCst);
        })
        .expect("spawn_proc_exit_monitor 应成功 spawn 监听线程");

        // 轮询等待回调被调用（最多 10s）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if callback_invoked.load(Ordering::SeqCst) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("v41-W-001: WAIT_OBJECT_0 路径下 callback 应被调用，但等待 10s 后仍未调用");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // 验证 WAIT_OBJECT_0 路径下 callback 已被调用
        assert!(
            callback_invoked.load(Ordering::SeqCst),
            "v41-W-001: WAIT_OBJECT_0 路径下 callback 应被调用"
        );

        // 等待监听线程退出
        monitor_handle.join().expect("监听线程应正常退出");

        // 清理子进程
        let _ = base.stop_process();
    }

    // ========== v41-W-002 修复测试：OwnedProcHandle::Drop is_invalid 守卫 ==========

    /// 验证 `OwnedProcHandle::drop` 对无效句柄（null）不调用 `CloseHandle` 且不 panic。
    ///
    /// v41-W-002 修复：原 `Drop` 直接调用 `CloseHandle(handle)`，未检查 `is_invalid()`。
    /// 对 null 或 INVALID_HANDLE_VALUE 调用 `CloseHandle` 会触发
    /// ERROR_INVALID_HANDLE（若启用句柄异常诊断还会产生异常事件）。
    ///
    /// 修复后 `Drop` 增加 `if !handle.is_invalid()` 守卫，对无效句柄跳过 CloseHandle。
    ///
    /// 本测试构造 null 句柄（`HANDLE::default()`）的 `OwnedProcHandle`，drop 后验证：
    /// 1. 不 panic（守卫跳过 CloseHandle）
    /// 2. 不影响后续真实句柄的 CloseHandle 行为
    #[test]
    fn v41_w002_drop_invalid_handle_no_panic() {
        use windows::Win32::Foundation::HANDLE;

        // 构造 null 句柄（HANDLE::default() 返回 null，is_invalid() == true）
        let null_handle = HANDLE::default();
        assert!(
            null_handle.is_invalid(),
            "HANDLE::default() 应为 invalid（null），is_invalid() 返回 true"
        );

        // 包装到 OwnedProcHandle 后 drop，守卫应跳过 CloseHandle，不 panic
        {
            let _owned = OwnedProcHandle::new(null_handle);
            // _owned 在此作用域结束时 drop，守卫跳过 CloseHandle
        }

        // 验证后续真实句柄的 CloseHandle 行为不受影响（创建事件、包装、drop、验证已关闭）
        let event_handle = unsafe {
            windows::Win32::System::Threading::CreateEventW(None, true, false, None)
                .expect("CreateEventW 应成功创建事件")
        };
        assert!(
            !event_handle.is_invalid(),
            "CreateEventW 返回的句柄应为有效值，is_invalid() 返回 false"
        );

        // 包装后 drop，应调用 CloseHandle 关闭句柄
        {
            let _owned = OwnedProcHandle::new(event_handle);
            // _owned drop 时调用 CloseHandle
        }

        // 句柄应已被关闭，WaitForSingleObject 返回 WAIT_FAILED
        let result =
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(event_handle, 0) };
        assert_eq!(
            result,
            windows::Win32::Foundation::WAIT_EVENT(0xFFFFFFFF),
            "v41-W-002: 有效句柄 drop 后应被 CloseHandle 关闭，WaitForSingleObject 返回 WAIT_FAILED"
        );
    }

    // ========== v41-W-005 修复测试：state_version 单调递增 ==========

    /// 验证 `PauseSender::set_state` 调用后 `state_version` 单调递增。
    ///
    /// v41-W-005 修复：原 `set_state` 仅更新 `shared_state.state`，无版本号跟踪。
    /// 当 resume 失败回滚 state 为 `Paused` 后，前端 UI 状态可能与 engine state
    /// 短暂不一致（旧 Playing 通知先到、新 Paused 通知后到但前端已处理更新）。
    ///
    /// 修复后 `set_state` 自增 `state_version` 计数器，前端通过比较 version
    /// 丢弃旧版本事件。
    ///
    /// 测试断言：
    /// 1. 初始 version 为 0
    /// 2. 每次 `set_state` 后 version 单调递增（+1）
    /// 3. clone 的 PauseSender 共享同一 version 计数器（与 shared_state 一致）
    /// 4. 非 `set_state` 操作（如 `set_volume`）不改变 version
    #[test]
    fn v41_w005_state_version_monotonic_increment() {
        let (sender, _rx, _shared) = create_pause_channel();

        // 1. 初始 version 应为 0
        assert_eq!(
            sender.state_version(),
            0,
            "v41-W-005: 初始 state_version 应为 0"
        );

        // 2. 第一次 set_state → version 应为 1
        sender.set_state(WallpaperState::Playing);
        assert_eq!(
            sender.state_version(),
            1,
            "v41-W-005: 第一次 set_state 后 version 应为 1"
        );

        // 3. 第二次 set_state → version 应为 2
        sender.set_state(WallpaperState::Paused);
        assert_eq!(
            sender.state_version(),
            2,
            "v41-W-005: 第二次 set_state 后 version 应为 2"
        );

        // 4. 第三次 set_state（相同状态也自增）→ version 应为 3
        sender.set_state(WallpaperState::Paused);
        assert_eq!(
            sender.state_version(),
            3,
            "v41-W-005: 第三次 set_state（相同状态）后 version 应为 3（状态相同也自增）"
        );

        // 5. set_volume 不应改变 version（仅 set_state 自增）
        sender.set_volume(0.5);
        assert_eq!(
            sender.state_version(),
            3,
            "v41-W-005: set_volume 不应改变 state_version"
        );

        // 6. clone 的 PauseSender 共享同一 version 计数器
        let sender_clone = sender.clone();
        assert_eq!(
            sender_clone.state_version(),
            3,
            "v41-W-005: clone 的 sender 应共享同一 version 计数器"
        );

        // 7. 通过 clone 调用 set_state 后，原 sender 应观察到 version 递增
        sender_clone.set_state(WallpaperState::Terminated);
        assert_eq!(
            sender.state_version(),
            4,
            "v41-W-005: 通过 clone 调用 set_state 后，原 sender 应观察到 version 递增（共享计数器）"
        );
        assert_eq!(
            sender_clone.state_version(),
            4,
            "v41-W-005: clone 也应观察到 version 递增"
        );

        // 8. 验证 state 也被正确更新
        assert_eq!(
            sender.state(),
            WallpaperState::Terminated,
            "v41-W-005: state 应为最后一次 set_state 的值"
        );
    }

    /// 验证 `create_pause_sender_with_state` 创建的 PauseSender 也包含 state_version。
    ///
    /// v41-W-005: 即使复用 shared_state 与 state_changed，state_version 仍独立初始化为 0。
    #[test]
    fn v41_w005_state_version_in_create_pause_sender_with_state() {
        let shared_state = Arc::new(RwLock::new(RendererState::default()));
        let (state_tx, _state_rx) = tokio::sync::broadcast::channel::<String>(16);

        let (sender, _rx) = create_pause_sender_with_state(shared_state, state_tx);

        // 初始 version 应为 0
        assert_eq!(
            sender.state_version(),
            0,
            "v41-W-005: create_pause_sender_with_state 创建的 sender 初始 version 应为 0"
        );

        // set_state 后 version 递增
        sender.set_state(WallpaperState::Playing);
        assert_eq!(
            sender.state_version(),
            1,
            "v41-W-005: set_state 后 version 应递增到 1"
        );
    }
}
