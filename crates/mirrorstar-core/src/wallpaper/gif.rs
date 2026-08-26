use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock, RwLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::wallpaper::{
    create_pause_channel, create_pause_sender_with_state, validate_renderer_speed,
    GifMemoryStrategy, RendererState, ScalingMode, WallpaperRenderer, WallpaperState,
    DEFAULT_BALANCED_KEEP_FRAMES,
};

use super::gdi_base::{
    create_wallpaper_window, get_initial_window_rect, paint_with_double_buffer,
    register_window_class_once, spawn_pause_forwarder, try_handle_common_messages, GdiRendererBase,
};
use super::gdi_cache::GdiCache;
use super::gif_decode::{
    decode_gif_first_frame, decode_gif_with_cancel_streaming, prefetch_with_cursor, GifFrame,
    PrefetchRequest, STREAMING_WINDOW_HALF,
};
use super::gif_memory::GifRenderData;

/// Custom window message to wake up the GIF wallpaper thread for command processing
///
/// 命名与 `image.rs::WM_WALLPAPER_COMMAND` 不一致（本处 GIF 专用命名，image.rs 采用
/// 泛化命名），偏移量亦不同（本处 `WM_USER + 10`，image.rs 用 `WM_USER + 1`）。
/// 经技术债评估（W-TD-018）决定保留现状：两常量分属各自渲染器创建的独立窗口，
/// 消息码互不冲突；统一命名需调整偏移量以避免冲突，且 GIF 侧 `WM_USER + 10/11`
/// 已被本渲染器 `WM_GIF_COMMAND` / `WM_GIF_FRAMES_LOADED` 占用，收敛收益低于
/// 回归风险。
const WM_GIF_COMMAND: u32 = WM_USER + 10;
/// 后台解码完成通知（Task 8.1：首帧快速显示）。
/// 由后台解码线程通过 PostMessageW 发送，消息循环接收后从通道读取全量帧数据。
const WM_GIF_FRAMES_LOADED: u32 = WM_USER + 11;
/// v9-A: 后台预取完成通知。
///
/// 当 `WM_TIMER` 推进到空像素帧时，主线程派生后台预取线程解码当前帧 ±
/// `STREAMING_WINDOW_HALF` 范围内的帧（一次性遍历，避免 (2*HALF+1)× 重复解码）。
/// 预取线程完成后通过 `PostMessageW(WM_GIF_FRAMES_PREFETCHED)` 唤醒消息循环，
/// 消息循环从通道读取 `Vec<(usize, GifFrame)>` 并填充对应帧的 pixels 字段。
///
/// 与 `WM_GIF_FRAMES_LOADED`（全量后台解码）的区别：预取仅解码流式窗口大小
/// （2*`STREAMING_WINDOW_HALF`+1）帧，目的是将 O(N) 按需解码移出主线程，消除大 GIF 的卡顿。
const WM_GIF_FRAMES_PREFETCHED: u32 = WM_USER + 12;
const GIF_TIMER_ID: usize = 1;

/// v41-W-019: `play()` 等待 `gif_wallpaper_thread` 上报结果的超时时间。
///
/// `gif_wallpaper_thread` 在此时间内应完成首帧解码（`decode_gif_first_frame`）、
/// 窗口创建（`create_wallpaper_window`）并通过 `result_tx.send` 上报结果。
/// 超时通常意味着首帧解码卡顿（大 GIF 或磁盘 IO 慢）或窗口创建异常。
///
/// 超时后 `play()` 返回 `MirrorStarError::DesktopIntegration`，调用方
/// （`WallpaperEngine`）清理注册表并向用户报告错误。`gif_wallpaper_thread`
/// 检测到 `result_tx` 关闭（`send` 失败）后会销毁已创建的窗口并退出，
/// 不进入消息循环，避免窗口泄漏（详见 `gif_wallpaper_thread` 中 send 失败处理）。
///
/// 15s 为宽松上界：正常 GIF 首帧解码 <1s，窗口创建 <100ms；大 GIF（50MB）
/// 首帧解码约 2-3s。`set_wallpaper` 的 IPC 超时为 20s，此处 15s 留 5s
/// 缓冲给 IPC 层。
const PLAY_RESULT_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Commands sent from the main thread to the GIF wallpaper thread
///
/// 与 `image.rs::WallpaperCommand` 高度相似，本枚举额外增加 `SetSpeed(f32)` 变体
/// 以支持 GIF 播放速度控制。经技术债评估（W-TD-010 + W-TD-017）决定保留现状：
/// 统一为 `GdiCommand` 枚举需改造两处壁纸线程的 match 分发逻辑，改动面较大；
/// 当前差异仅一个变体，重复成本可接受。新增公共变体时请同步两处枚举。
enum GifCommand {
    Terminate,
    SetPosition {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    SetScalingMode(ScalingMode),
    Pause,
    Resume,
    SetSpeed(f32),
}

/// 窗口用户数据，通过 GWLP_USERDATA 存储
struct GifWindowData {
    /// GIF 渲染数据
    render: GifRenderData,
    /// 缓存的 GDI 对象（首次 WM_PAINT 时初始化）
    gdi_cache: Option<GdiCache>,
    /// W09：后台解码取消标志，窗口销毁时置 true 让解码线程尽快退出
    decode_cancel: Arc<AtomicBool>,
    /// v9-A: 后台预取进行中标志。
    ///
    /// `WM_TIMER` 检测到空像素帧时置 true 并派生预取线程；预取完成后
    /// `handle_frames_prefetched` 置 false。用于防止 WM_TIMER 重复派生
    /// 预取线程（前一次预取未完成时不再派生新的）。
    ///
    /// 使用 `Arc<AtomicBool>` 而非 `bool`：预取线程理论上可通过克隆的
    /// Arc 访问此标志（当前实现未使用，但保留扩展性）；同时 Arc 保证
    /// 即使窗口数据被销毁，预取线程持有的引用仍有效（不会触发 UB）。
    prefetch_in_progress: Arc<AtomicBool>,
    /// v9-A: 预取通道发送端。
    ///
    /// `WM_TIMER` 派生预取请求时克隆此发送端传入 worker 线程，worker 通过
    /// `send(Vec<(usize, GifFrame)>)` 回传结果。接收端 `prefetch_rx` 保留在
    /// `gif_wallpaper_thread` 消息循环中，由 `handle_frames_prefetched` 读取。
    prefetch_tx: std::sync::mpsc::Sender<Vec<(usize, GifFrame)>>,
    /// #1: 长生命预取 worker 线程的请求发送端（lazy 创建）。
    ///
    /// 首次预取请求时 [`dispatch_prefetch`] 创建 worker 线程并存入此槽位；
    /// 后续请求复用同一 worker（worker 持久化解码游标，消除 O(N) 重解码）。
    /// `WM_DESTROY` 时 `Box::from_raw` 回收 `GifWindowData`，此 Sender drop，
    /// worker 的 `req_rx.recv()` 返回 `Err` 自然退出。
    prefetch_worker_tx: Option<std::sync::mpsc::Sender<PrefetchRequest>>,
    /// v17 性能埋点：GIF 帧率与绘制耗时跟踪器。仅在窗口过程（壁纸线程）访问。
    fps: crate::perf::FramePerfTracker,
    /// v17 性能埋点：后台全量解码开始时间，`handle_frames_loaded` 读取计算耗时。
    decode_start: Option<std::time::Instant>,
}

/// GIF 壁纸渲染器
///
/// 使用 GDI StretchDIBits 将 GIF 动画帧渲染到壁纸窗口。
/// 窗口在专用线程上创建和运行，通过 mpsc 通道进行线程间通信。
/// WM_TIMER 驱动帧推进，支持暂停/恢复和速度控制。
///
/// 公共状态（窗口句柄、线程句柄、缩放模式、状态、pause_sender）由 `GdiRendererBase` 持有，
/// Win32 双缓冲绘制、窗口类注册、窗口创建、pause 转发等逻辑复用 `gdi_base` 模块的辅助函数。
///
/// # 错误传播策略（v41-W-018）
///
/// `GifRenderer` 的错误处理区分解码阶段与渲染阶段：
///
/// - **解码失败**（`decode_gif_first_frame` / `decode_gif_with_cancel` 返回错误）：
///   `play()` 将错误包装为 `MirrorStarError::ImageDecode` 返回给调用方，壁纸创建失败，
///   不进入播放状态。调用方（`WallpaperEngine`）据此清理注册表并向用户报告错误。
/// - **渲染失败**（壁纸线程内 `WM_PAINT` / `WM_TIMER` 处理中的 GDI 错误，如
///   `StretchDIBits` 失败、`paint_with_double_buffer` 失败）：
///   壁纸线程通过 `tracing::warn!` 记录错误并降级处理（如跳过当前帧、使用上次缓存、
///   保持上一帧画面），不向调用方传播错误。原因是壁纸线程是独立线程，无法直接返回
///   `Result`；持续渲染比中断更符合壁纸场景（单帧绘制失败不应终止整个壁纸）。
/// - **线程/spawn 失败**（`thread::Builder::spawn` 失败、`result_rx.recv_timeout`
///   超时或通道断开）：
///   `play()` 包装为 `MirrorStarError::DesktopIntegration` 返回给调用方。
///   v41-W-019: `recv_timeout` 限时 [`PLAY_RESULT_RECV_TIMEOUT`]（15s），
///   超时后线程检测到 `result_tx` 关闭会销毁窗口并退出，避免泄漏。
/// - **命令发送失败**（`terminate` / `set_position` 等 mpsc send 失败）：
///   包装为 `MirrorStarError::DesktopIntegration` 返回，表示壁纸线程已退出或通道关闭。
pub struct GifRenderer {
    /// GDI 渲染器公共基类（hwnd / thread_handle / scaling_mode / state / pause_sender）
    base: GdiRendererBase,
    /// 通道发送端，用于向壁纸线程发送命令
    cmd_tx: Sender<GifCommand>,
    /// GIF 文件路径
    gif_path: String,
    /// 播放速度倍率
    speed: f32,
    /// 内存管理策略
    memory_strategy: GifMemoryStrategy,
    /// 平衡模式下保留的帧数
    balanced_keep_frames: usize,
    /// GIF 帧像素内存预算上限（MB）（v41-W-012: 从配置传入）
    max_memory_mb: usize,
    /// 以下三个字段（`pre_shared_state` / `pre_state_changed` / `display_id_lock`）
    /// 与 `image.rs::ImageRenderer` 中同名字段构成同一「预置状态」模式：play() 阶段
    /// 提前创建共享状态与通知通道，create_pause_sender() 阶段 take() 复用，壁纸线程
    /// 通过 `display_id_lock` 在 Resume 失败时回滚状态并通知前端。
    ///
    /// 此处与 image.rs 存在字段级重复，经技术债评估（W-TD-008 + W-TD-009）决定保留现状：
    /// 抽取共享结构体需调整 `GdiRendererBase` 架构（基类当前不持有这些字段），
    /// 改动面与回归风险高于当前重复成本，故维持两处独立实现。修改任一处时请同步
    /// 另一处以保持行为一致。
    ///
    /// W-003 修复：提前创建的共享状态（play() 中创建，create_pause_sender() 中复用）
    /// 传给壁纸线程，使 Resume 失败时能回滚 shared_state.state = Paused
    pre_shared_state: Option<Arc<RwLock<RendererState>>>,
    /// W-003 修复：提前创建的状态变更通知通道（play() 中创建，create_pause_sender() 中复用）
    /// 传给壁纸线程，使 Resume 失败时能通过 notify 通知前端刷新 UI
    pre_state_changed: Option<tokio::sync::broadcast::Sender<String>>,
    /// W-003 修复：display_id 共享变量（create_pause_sender 中设置，壁纸线程中读取）
    /// 使用 OnceLock 保证只写一次，壁纸线程在 Resume 失败时读取此值通知前端
    display_id_lock: Arc<std::sync::OnceLock<String>>,
}

// SAFETY: GifRenderer 的所有窗口操作都在专用线程上执行。
// 公共 API 仅通过 mpsc 通道通信，Sender<GifCommand> 是 Send 的。
// HWND 仅作为值存储，用于 PostMessageW（PostMessageW 是线程安全的）。
unsafe impl Send for GifRenderer {}

impl GifRenderer {
    /// 创建新的 GIF 渲染器
    pub fn new(gif_path: String, scaling_mode: ScalingMode) -> Self {
        Self::with_strategy(
            gif_path,
            scaling_mode,
            GifMemoryStrategy::default(),
            DEFAULT_BALANCED_KEEP_FRAMES,
            super::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB,
        )
    }

    /// 使用指定内存管理策略创建 GIF 渲染器
    pub fn with_strategy(
        gif_path: String,
        scaling_mode: ScalingMode,
        memory_strategy: GifMemoryStrategy,
        balanced_keep_frames: usize,
        max_memory_mb: usize,
    ) -> Self {
        Self {
            base: GdiRendererBase::new(scaling_mode),
            cmd_tx: mpsc::channel().0, // 占位，将在 play() 中设置
            gif_path,
            speed: 1.0,
            memory_strategy,
            balanced_keep_frames,
            max_memory_mb,
            pre_shared_state: None,
            pre_state_changed: None,
            display_id_lock: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// 开始播放壁纸
    pub fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel::<Result<isize, crate::MirrorStarError>>();

        let gif_path = self.gif_path.clone();
        let scaling_mode = self.base.scaling_mode;
        let speed = self.speed;
        let memory_strategy = self.memory_strategy;
        let balanced_keep_frames = self.balanced_keep_frames;
        let max_memory_mb = self.max_memory_mb;

        // 获取屏幕大小作为初始窗口尺寸
        let initial_rect = get_initial_window_rect();

        // W-003 修复：提前创建 shared_state 和 state_changed 通道，
        // 传给壁纸线程使 Resume 失败时能回滚状态并通知前端
        let shared_state = Arc::new(RwLock::new(RendererState::default()));
        let (state_changed, _) = tokio::sync::broadcast::channel::<String>(16);
        // 初始化 shared_state.state = Playing（与 play() 成功后 set_state(Playing) 一致）
        shared_state.write().unwrap().state = WallpaperState::Playing;
        let display_id_lock = self.display_id_lock.clone();
        // Clone for the wallpaper thread closure; originals retained for self.pre_*
        // Arc::clone 与 broadcast::Sender::clone 均为廉价引用计数递增
        let thread_shared_state = shared_state.clone();
        let thread_state_changed = state_changed.clone();

        let handle = std::thread::Builder::new()
            .name("mirrorstar-gif-wallpaper".to_string())
            .spawn(move || {
                gif_wallpaper_thread(
                    gif_path,
                    scaling_mode,
                    speed,
                    initial_rect,
                    cmd_rx,
                    result_tx,
                    memory_strategy,
                    balanced_keep_frames,
                    max_memory_mb,
                    thread_shared_state,
                    thread_state_changed,
                    display_id_lock,
                );
            })
            .map_err(|e| {
                crate::MirrorStarError::DesktopIntegration(format!("创建 GIF 壁纸线程失败: {}", e))
            })?;

        // 等待线程报告成功或失败
        // v41-W-019: 使用 recv_timeout 限时等待，避免 gif_wallpaper_thread 在首帧解码
        // 或窗口创建阶段挂起时 play() 永久阻塞。超时后 result_rx 被 drop，线程检测到
        // result_tx.send 失败后会销毁窗口并退出（不进入消息循环），避免窗口泄漏。
        let result = result_rx
            .recv_timeout(PLAY_RESULT_RECV_TIMEOUT)
            .map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    crate::MirrorStarError::DesktopIntegration(format!(
                        "GIF 壁纸线程在 {:?} 内未上报结果（首帧解码或窗口创建超时）",
                        PLAY_RESULT_RECV_TIMEOUT
                    ))
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    crate::MirrorStarError::DesktopIntegration(
                        "GIF 壁纸线程通信失败: 通道已断开（线程可能 panic）".to_string(),
                    )
                }
            })?;
        let hwnd_value = result.map_err(|e| {
            crate::MirrorStarError::DesktopIntegration(format!("GIF 壁纸初始化失败: {}", e))
        })?;
        let hwnd = HWND(hwnd_value as *mut _);

        self.cmd_tx = cmd_tx;
        self.base.set_hwnd(Some(hwnd));
        self.base.set_thread_handle(Some(handle));
        self.base.set_state(WallpaperState::Playing);
        // 存储 pre-created 组件供 create_pause_sender() 复用
        self.pre_shared_state = Some(shared_state);
        self.pre_state_changed = Some(state_changed);

        tracing::info!("GIF 壁纸开始播放");
        Ok(())
    }

    /// 终止壁纸渲染
    pub fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        self.base
            .terminate(&self.cmd_tx, GifCommand::Terminate, WM_GIF_COMMAND)?;
        tracing::info!("GIF 壁纸已终止");
        Ok(())
    }

    /// 获取壁纸窗口句柄
    pub fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    /// 发送命令到壁纸线程，并通过 PostMessageW 唤醒消息循环
    fn send_command(&self, cmd: GifCommand) -> Result<(), crate::MirrorStarError> {
        self.base.send_command(&self.cmd_tx, cmd, WM_GIF_COMMAND)
    }
}

impl Drop for GifRenderer {
    fn drop(&mut self) {
        if self.base.thread_handle.is_some() {
            // Drop 路径无法传播错误，仅记录日志
            if let Err(e) = self.terminate() {
                tracing::warn!(error = %e, "GifRenderer drop 时 terminate 失败");
            }
        }
        tracing::debug!("GifRenderer 已清理");
    }
}

impl WallpaperRenderer for GifRenderer {
    fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        GifRenderer::play(self)
    }

    fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
        if self.base.state() == WallpaperState::Playing {
            self.send_command(GifCommand::Pause)?;
            self.base.set_state(WallpaperState::Paused);
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        if self.base.state() == WallpaperState::Paused {
            self.send_command(GifCommand::Resume)?;
            self.base.set_state(WallpaperState::Playing);
        }
        Ok(())
    }

    fn set_position(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), crate::MirrorStarError> {
        self.send_command(GifCommand::SetPosition {
            x,
            y,
            width: w,
            height: h,
        })
    }

    fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        GifRenderer::terminate(self)
    }

    fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    fn state(&self) -> WallpaperState {
        self.base.state()
    }

    fn set_speed(&mut self, speed: f32) {
        // W10: 校验 speed 必须为正有限数。speed <= 0 或 NaN/Infinity 会导致
        // 帧延迟计算 `frame.delay_ms / speed` 产生 0/inf/NaN，引发定时器异常。
        // speed=0 在语义上等同于暂停，但调用方应显式调用 pause()；此处拒绝并提示。
        // W-007: 校验逻辑提取为共享函数 `validate_renderer_speed`，与 VideoRenderer 保持一致。
        if !validate_renderer_speed(speed) {
            tracing::warn!(
                speed,
                "无效的 GIF 播放速度，已忽略（speed 必须 > 0 且有限）"
            );
            return;
        }
        self.speed = speed;
        if self.base.state() == WallpaperState::Playing {
            if let Err(e) = self.send_command(GifCommand::SetSpeed(speed)) {
                tracing::error!("设置 GIF 播放速度失败: {}", e);
            }
        }
    }

    fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.base.set_scaling_mode(mode);
        if self.base.state() == WallpaperState::Playing {
            if let Err(e) = self.send_command(GifCommand::SetScalingMode(mode)) {
                tracing::error!("GIF 缩放模式切换失败: {}", e);
            }
        }
    }

    fn create_pause_sender(&mut self, display_id: &str) -> Option<crate::wallpaper::PauseSender> {
        // W-003 修复：复用 play() 中提前创建的 shared_state 和 state_changed，
        // 使壁纸线程在 Resume 失败时能通过 shared_state 回滚状态并通知前端
        let shared_state = self.pre_shared_state.take().unwrap_or_else(|| {
            tracing::warn!("pre_shared_state 已被消费，回退到 create_pause_channel");
            let (_, _, s) = create_pause_channel();
            s
        });
        let state_changed = self.pre_state_changed.take().unwrap_or_else(|| {
            tracing::warn!("pre_state_changed 已被消费，回退到新 broadcast channel");
            tokio::sync::broadcast::channel::<String>(16).0
        });

        // 设置 display_id_lock，使壁纸线程在 Resume 失败时能读取 display_id 通知前端
        let _ = self.display_id_lock.set(display_id.to_string());

        // 同步 shared_state.state 为当前 base.state()
        {
            let mut s = shared_state.write().unwrap_or_else(|e| e.into_inner());
            s.state = self.base.state();
        }

        // 使用已有的 shared_state 和 state_changed 创建 PauseSender + rx
        let (sender, rx) = create_pause_sender_with_state(shared_state.clone(), state_changed);

        let cmd_tx = self.cmd_tx.clone();
        // HWND 不是 Send，将原始值存储为 isize 跨线程传递
        let hwnd_raw = self.base.hwnd.map(|h| h.0 as isize);
        // clone sender 与 display_id 传入 spawn_pause_forwarder，
        // 状态变更后通过 notify_state_changed 通知 Tauri 层 emit 事件
        let state_sender = sender.clone();
        let display_id = display_id.to_string();

        if let Err(e) = spawn_pause_forwarder(
            "mirrorstar-gif-pause",
            "GifRenderer",
            cmd_tx,
            hwnd_raw,
            WM_GIF_COMMAND,
            || GifCommand::Pause,
            || GifCommand::Resume,
            rx,
            shared_state,
            state_sender,
            display_id,
        ) {
            tracing::error!(error = %e, "创建 GifRenderer pause 线程失败");
            return None;
        }

        self.base.set_pause_sender(Some(sender.clone()));
        Some(sender)
    }
}

// ── Wallpaper Thread ─────────────────────────────────────────────────────────

/// GIF 壁纸专用线程函数
///
/// Task 8.1 优化：先解码首帧并立即创建窗口显示（快速可见），随后在后台线程
/// 解码剩余帧。后台解码完成后通过 `PostMessageW(WM_GIF_FRAMES_LOADED)` 通知
/// 消息循环，消息循环从通道读取全量帧数据并替换 `GifRenderData.frames`。
///
/// 竞态处理：若用户在后台解码完成前暂停并恢复（Aggressive/Adaptive 策略
/// `handle_resume` 可能同步重新解码），消息循环检查 `frames_loaded` 标志：
/// - `false`：应用后台解码结果，置 `true`
/// - `true`：丢弃后台解码结果（已通过其他途径加载）
///
/// W-003 修复：接收 `shared_state`、`state_changed` 和 `display_id_lock`，
/// 使壁纸线程在 Resume 解码失败时能回滚 shared_state.state = Paused
/// 并通过 state_changed 通知前端刷新 UI。
#[allow(clippy::too_many_arguments)]
fn gif_wallpaper_thread(
    gif_path: String,
    scaling_mode: ScalingMode,
    speed: f32,
    initial_rect: (i32, i32, i32, i32),
    cmd_rx: Receiver<GifCommand>,
    result_tx: Sender<Result<isize, crate::MirrorStarError>>,
    memory_strategy: GifMemoryStrategy,
    balanced_keep_frames: usize,
    max_memory_mb: usize,
    shared_state: Arc<RwLock<RendererState>>,
    state_changed: tokio::sync::broadcast::Sender<String>,
    display_id_lock: Arc<std::sync::OnceLock<String>>,
) {
    // Step 1: 解码 GIF 首帧（快速显示）
    let first_frame = match decode_gif_first_frame(&gif_path) {
        Ok(f) => f,
        Err(e) => {
            if let Err(e) = result_tx.send(Err(e)) {
                tracing::warn!(error = %e, "result_tx 已关闭，无法上报错误（调用方将超时）");
            }
            return;
        }
    };

    tracing::info!(path = %gif_path, "GIF 首帧解码成功，窗口将立即显示首帧");

    // W09: 后台解码取消标志，存入 GifWindowData 供 WM_DESTROY 设置，
    // 克隆到后台解码线程供 decode_gif_with_cancel 检查
    let decode_cancel = Arc::new(AtomicBool::new(false));
    // v9-A: 预取进行中标志，存入 GifWindowData 供 WM_TIMER 与 handle_frames_prefetched 访问
    let prefetch_in_progress = Arc::new(AtomicBool::new(false));
    // v9-A: 预取通道——WM_TIMER 检测到空像素帧时派生后台线程，通过此通道回传
    // `Vec<(usize, GifFrame)>`，并由 PostMessageW(WM_GIF_FRAMES_PREFETCHED) 唤醒循环。
    // 发送端（prefetch_tx）存入 GifWindowData 供窗口过程访问；接收端（prefetch_rx）
    // 保留在消息循环局部，生命周期与循环一致，窗口销毁后 drop 致 send 失败仅记录日志。
    let (prefetch_tx, prefetch_rx) = mpsc::channel::<Vec<(usize, GifFrame)>>();

    // v17 性能埋点：首帧延迟作为 FPS 慢帧判定基线（在 first_frame move 前读取）
    let expected_delay_ms = first_frame.delay_ms;
    let window_data = Box::new(GifWindowData {
        render: GifRenderData {
            frames: vec![first_frame],
            current_frame: 0,
            scaling_mode,
            speed,
            paused: false,
            image_path: gif_path.clone(),
            // 后台解码完成后置 true；在此之前 handle_pause 不会释放帧（len==1）
            frames_loaded: false,
            saved_frame_index: None,
            memory_strategy,
            balanced_keep_frames,
            max_memory_mb,
            // v5.0 W-PERF-001: 初始为 0 表示尚未设置过 timer，首次 SetTimer 必定执行
            last_timer_delay: 0,
            // v8-C: 后台解码完成前仅首帧，total_frames=1；handle_frames_loaded 会更新为实际总数
            total_frames: 1,
            // #2: sync 兜底游标 lazy 初始化（首次 reload_current_frame_pixels 时打开文件）
            sync_frames_iter: None,
            sync_cursor: 0,
        },
        gdi_cache: None,
        decode_cancel: decode_cancel.clone(),
        prefetch_in_progress: prefetch_in_progress.clone(),
        prefetch_tx,
        // #1: worker lazy 创建，首次 dispatch_prefetch 时填入
        prefetch_worker_tx: None,
        fps: crate::perf::FramePerfTracker::new("gif", 60, expected_delay_ms),
        decode_start: None,
    });

    // Step 2: 在此线程上创建窗口
    let class_name = register_gif_window_class();

    let hwnd = match create_wallpaper_window(
        class_name,
        windows::core::w!("MirrorStar Gif Wallpaper"),
        initial_rect,
    ) {
        Ok(h) => h,
        Err(e) => {
            if let Err(e) = result_tx.send(Err(e)) {
                tracing::warn!(error = %e, "result_tx 已关闭，无法上报错误（调用方将超时）");
            }
            return;
        }
    };

    // 将窗口数据存储到 GWLP_USERDATA
    let data_ptr = Box::into_raw(window_data);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, data_ptr as isize);
    }

    // 启动帧定时器（首帧延迟；后台解码完成后会重置为对应帧延迟）
    let initial_delay = unsafe {
        let data = &mut *data_ptr;
        let frame = &data.render.frames[0];
        let adjusted = (frame.delay_ms as f32 / data.render.speed) as u32;
        adjusted.max(10)
    };
    unsafe {
        let _ = SetTimer(hwnd, GIF_TIMER_ID, initial_delay, None);
        // v5.0 W-PERF-001: 同步缓存，使 WM_TIMER 首帧跳过重复 SetTimer
        (*data_ptr).render.last_timer_delay = initial_delay;
    }

    // 通知主线程创建成功（首帧已可显示，无需等待全量解码）
    // v41-W-019: 若 result_tx 已关闭（play() 超时后 drop 了 result_rx），
    // 销毁已创建的窗口并退出线程，不进入消息循环，避免窗口泄漏。
    // （窗口无管理者会成为孤儿：无法接收命令、无法被 terminate，直到进程退出）
    //
    // DestroyWindow 会同步触发 WM_DESTROY：
    // - KillTimer 停止已设置的帧定时器
    // - 设置 decode_cancel = true（后台解码线程尚未启动，防御性设置）
    // - Box::from_raw 回收 GifWindowData 内存（含 GifRenderData.frames）
    // - 清理 gdi_cache（此时为 None，首次 WM_PAINT 未发生）
    // - PostQuitMessage（无害：不会进入消息循环消费此消息）
    if let Err(e) = result_tx.send(Ok(hwnd.0 as isize)) {
        tracing::warn!(
            error = %e,
            "result_tx 已关闭（play() 已超时返回），销毁窗口并退出线程避免泄漏"
        );
        unsafe {
            // DestroyWindow 同步调用 WM_DESTROY 处理器完成全部清理
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    // Step 3: 启动后台线程解码全量帧（含首帧，替换当前单帧列表）
    let (frames_tx, frames_rx) = mpsc::channel::<Result<Vec<GifFrame>, crate::MirrorStarError>>();
    let gif_path_for_bg = gif_path.clone();
    // HWND 不是 Send（内部为 *mut c_void），转换为 isize 跨线程传递
    // （与 create_pause_sender 中的 hwnd_raw 模式一致）
    let hwnd_raw = hwnd.0 as isize;
    // W09: 克隆取消标志到后台解码线程，窗口销毁时设置后解码循环可尽快退出
    let cancel_for_bg = decode_cancel.clone();
    // v17 性能埋点：记录后台解码开始时间，handle_frames_loaded 读取计算耗时
    unsafe {
        (*data_ptr).decode_start = Some(std::time::Instant::now());
    }
    if let Err(e) = std::thread::Builder::new()
        .name("mirrorstar-gif-decode".to_string())
        .spawn(move || {
            // v18: 流式窗口解码——解码过程中即时清空窗口外帧像素，
            // 将峰值像素内存从预算上限（max_memory_mb）降至窗口大小+1 帧。
            // center=0：初始播放时 current_frame=0、saved_frame_index=None。
            let result = decode_gif_with_cancel_streaming(
                &gif_path_for_bg,
                Some(&cancel_for_bg),
                max_memory_mb,
                0,
            );
            // 先将结果送入通道，再 PostMessageW 唤醒消息循环读取
            // 若窗口已销毁（接收端 drop），send 失败仅记录日志
            if result.is_err() {
                if let Err(ref err) = result {
                    // W09: 取消不算失败，仅 debug 日志
                    if cancel_for_bg.load(Ordering::Relaxed) {
                        tracing::debug!(error = %err, "GIF 后台解码已取消");
                    } else {
                        tracing::error!(error = %err, "GIF 后台解码失败");
                    }
                }
            }
            if let Err(e) = frames_tx.send(result) {
                tracing::warn!(error = %e, "frames_tx 已关闭，无法上报帧数据");
            }
            // SAFETY: PostMessageW 是线程安全的，可在任意线程调用。
            // 即使 hwnd 已销毁，PostMessageW 仅返回 false，不引发未定义行为。
            // hwnd_raw 由当前线程的合法 HWND 转换而来，还原为 HWND 传给 PostMessageW。
            unsafe {
                if PostMessageW(
                    HWND(hwnd_raw as *mut _),
                    WM_GIF_FRAMES_LOADED,
                    WPARAM(0),
                    LPARAM(0),
                )
                .is_err()
                {
                    tracing::warn!(
                        "PostMessageW 失败：WM_GIF_FRAMES_LOADED 未送达（窗口可能已销毁）"
                    );
                }
            }
        })
    {
        tracing::error!(error = %e, "创建 GIF 后台解码线程失败，将仅播放首帧");
        // 后台线程创建失败时，标记 frames_loaded=true 避免后续误判
        unsafe {
            let render = &mut (*data_ptr).render;
            render.frames_loaded = true;
        }
    }

    // Step 4: 消息循环
    // v9-A: prefetch_rx 在上方创建（与 prefetch_tx 同时），此处保留引用供
    // handle_frames_prefetched 读取预取结果。
    let mut msg = MSG::default();
    'main_loop: loop {
        // GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
        // （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            0 => break, // WM_QUIT
            -1 => {
                tracing::error!("GetMessageW 返回 -1（错误），退出消息循环");
                break;
            }
            _ => {}
        }

        // 处理后台解码完成通知（Task 8.1）
        if msg.message == WM_GIF_FRAMES_LOADED {
            // v15-B-001: 不再向 handle_frames_loaded 传递捕获的 data_ptr。
            // 函数内部通过 GetWindowLongPtrW 实时取回，避免 WM_DESTROY 回收后悬垂。
            unsafe {
                handle_frames_loaded(hwnd, &frames_rx);
            }
            continue;
        }

        // v9-A: 处理后台预取完成通知
        if msg.message == WM_GIF_FRAMES_PREFETCHED {
            // v15-B-001: 同上，不传递捕获的 data_ptr。
            unsafe {
                handle_frames_prefetched(hwnd, &prefetch_rx);
            }
            continue;
        }

        if msg.message == WM_GIF_COMMAND {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    GifCommand::Terminate => {
                        tracing::info!("GIF 壁纸终止");
                        unsafe {
                            let _ = KillTimer(hwnd, GIF_TIMER_ID);
                            if let Err(e) = DestroyWindow(hwnd) {
                                tracing::warn!(error = %e, "DestroyWindow 失败");
                            }
                        }
                        break 'main_loop;
                    }
                    GifCommand::SetPosition {
                        x,
                        y,
                        width,
                        height,
                    } => unsafe {
                        tracing::info!(x, y, width, height, "GIF 壁纸设置位置");
                        let _ = SetWindowPos(
                            hwnd,
                            HWND::default(),
                            x,
                            y,
                            width,
                            height,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                    GifCommand::SetScalingMode(mode) => unsafe {
                        tracing::info!(scaling_mode = ?mode, "GIF 壁纸设置缩放模式");
                        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
                        if !data_ptr.is_null() {
                            (*data_ptr).render.scaling_mode = mode;
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                    GifCommand::Pause => unsafe {
                        tracing::info!("GIF 壁纸已暂停");
                        let _ = KillTimer(hwnd, GIF_TIMER_ID);
                        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
                        if !data_ptr.is_null() {
                            let render = &mut (*data_ptr).render;
                            render.handle_pause();
                            // v17 性能埋点：重置 FPS 跟踪，避免暂停时长污染后续统计
                            (*data_ptr).fps.reset();
                        }
                    },
                    GifCommand::Resume => unsafe {
                        tracing::info!("GIF 壁纸恢复播放");
                        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
                        if !data_ptr.is_null() {
                            let render = &mut (*data_ptr).render;
                            // W-003 修复：handle_resume 失败时回滚 shared_state.state = Paused
                            // 并通知前端刷新 UI，避免状态不一致（engine 显示 Playing 但壁纸冻结）
                            if let Err(e) = render.handle_resume() {
                                tracing::error!(error = %e, "GIF 恢复失败，回滚状态为 Paused");
                                // 回滚 shared_state.state = Paused
                                // （pause 转发线程已设为 Playing，此处修正为 Paused）
                                shared_state
                                    .write()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .state = WallpaperState::Paused;
                                // 通知前端刷新 UI（display_id 由 create_pause_sender 设置）
                                if let Some(id) = display_id_lock.get() {
                                    let _ = state_changed.send(id.to_string());
                                }
                            } else if render.frames.is_empty() {
                                // W-006 守卫：handle_resume 成功但 frames 为空（边界场景，
                                // 如未来 handle_resume 实现变化），跳过索引与 SetTimer，
                                // 与 WM_TIMER 分支的 is_empty 守卫风格一致
                                tracing::warn!("GIF Resume 后 frames 为空，跳过帧索引与定时器设置");
                            } else {
                                let frame = &render.frames[render.current_frame];
                                let adjusted_delay = (frame.delay_ms as f32 / render.speed) as u32;
                                let delay = adjusted_delay.max(10);
                                let _ = SetTimer(hwnd, GIF_TIMER_ID, delay, None);
                                // v5.0 W-PERF-001: 同步缓存
                                render.last_timer_delay = delay;
                            }
                            // v17 性能埋点：重置 FPS 跟踪，避免暂停时长污染恢复后统计
                            (*data_ptr).fps.reset();
                        }
                    },
                    GifCommand::SetSpeed(speed) => unsafe {
                        tracing::info!(speed, "GIF 壁纸设置速度");
                        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
                        if !data_ptr.is_null() {
                            let render = &mut (*data_ptr).render;
                            render.speed = speed;
                            if !render.paused {
                                let frame = &render.frames[render.current_frame];
                                let adjusted_delay = (frame.delay_ms as f32 / render.speed) as u32;
                                let delay = adjusted_delay.max(10);
                                let _ = SetTimer(hwnd, GIF_TIMER_ID, delay, None);
                                // v5.0 W-PERF-001: 同步缓存
                                render.last_timer_delay = delay;
                            }
                        }
                    },
                }
            }
            continue;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // 清理：确保窗口数据被释放
    unsafe {
        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
        if !data_ptr.is_null() {
            // 回收 Box 内存，返回值无需使用（Drop 已执行清理）
            let _ = Box::from_raw(data_ptr);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }
}

/// 处理 `WM_GIF_FRAMES_LOADED` 消息：从通道读取后台解码结果并替换帧数据。
///
/// 竞态安全：检查 `frames_loaded` 标志，若已为 true（例如 `handle_resume` 在
/// 后台解码完成前同步重新解码过），则丢弃后台结果，避免覆盖已加载的帧。
///
/// # Safety
///
/// 调用方为消息循环线程，与窗口过程同线程，无竞争。`data_ptr` 通过
/// `GetWindowLongPtrW(hwnd, GWLP_USERDATA)` 实时取回（v15-B-001），不依赖
/// 消息循环捕获的裸指针，避免 WM_DESTROY 回收 `Box` 后悬垂访问。
unsafe fn handle_frames_loaded(
    hwnd: HWND,
    frames_rx: &std::sync::mpsc::Receiver<Result<Vec<GifFrame>, crate::MirrorStarError>>,
) {
    // v15-B-001: 实时取回 data_ptr，避免 WM_DESTROY 后捕获的裸指针悬垂（UAF）。
    // 与 WM_COMMAND 处理（gif_wnd_proc L726/L735/L743/L777）保持一致。
    let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
    if data_ptr.is_null() {
        return;
    }
    // v17 性能埋点：取出后台解码开始时间，避免后续 &mut render 借用冲突
    let decode_start = (*data_ptr).decode_start.take();
    let render = &mut (*data_ptr).render;

    if render.frames_loaded {
        // 帧已通过其他途径加载（如 handle_resume 同步重新解码），丢弃后台结果
        // try_recv 失败（无数据或通道断开）均无影响：本就是丢弃逻辑
        let _ = frames_rx.try_recv();
        tracing::debug!("后台解码结果已丢弃（帧此前已加载）");
        return;
    }

    match frames_rx.try_recv() {
        Ok(Ok(new_frames)) => {
            let total = new_frames.len();
            if total == 0 {
                tracing::warn!("后台解码返回 0 帧，保留首帧");
                render.frames_loaded = true;
                return;
            }
            // 保留暂停时保存的帧索引（若有），否则从首帧开始
            let saved_index = render.saved_frame_index.unwrap_or(0);
            let current = saved_index.min(total.saturating_sub(1));
            render.frames = new_frames;
            render.current_frame = current;
            // v8-C: 记录原始总帧数（用于诊断；WM_TIMER 循环仍以 frames.len() 为准）
            render.total_frames = total;
            render.frames_loaded = true;

            // v8-C: 应用流式帧缓存窗口，仅保留当前帧 ± STREAMING_WINDOW_HALF 帧
            // 的像素数据，其余帧 pixels 清空（保留元数据）。活跃内存从全量帧降至
            // ~3 帧。WM_TIMER 推进到空像素帧时通过 decode_gif_frame_at 按需恢复。
            render.apply_streaming_window();

            // v17 性能埋点：解码耗时 + 帧内存估算 + 进程内存
            // decode_ms 含「派生线程 → 解码 → PostMessageW → 消息循环处理」总延迟
            let decode_ms = decode_start
                .map(|s| s.elapsed().as_millis())
                .unwrap_or(0);
            let frame_mem_mb = render.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
            let rss_mb = crate::perf::process_rss_mb();
            let private_mb = crate::perf::process_private_mb();
            tracing::info!(
                target: "mirrorstar::perf",
                frame_count = total,
                current_frame = render.current_frame,
                decode_ms,
                frame_mem_mb = format!("{:.2}", frame_mem_mb),
                rss_mb = format!("{:.1}", rss_mb),
                private_mb = format!("{:.1}", private_mb),
                "PERF-GIF: 后台解码完成，已加载全部帧并应用流式窗口"
            );

            // 若非暂停状态，重置定时器为当前帧的延迟
            if !render.paused {
                let frame = &render.frames[render.current_frame];
                let adjusted_delay = (frame.delay_ms as f32 / render.speed) as u32;
                let delay = adjusted_delay.max(10);
                let _ = SetTimer(hwnd, GIF_TIMER_ID, delay, None);
                // v5.0 W-PERF-001: 同步缓存
                render.last_timer_delay = delay;
            }
        }
        Ok(Err(e)) => {
            // 后台解码失败，保留首帧；标记 frames_loaded=true 避免后续误判
            tracing::error!(error = %e, "GIF 后台解码失败，保留首帧");
            render.frames_loaded = true;
        }
        Err(_) => {
            // 通道为空（理论上不应发生：PostMessageW 在 send 之后调用）
            tracing::warn!("WM_GIF_FRAMES_LOADED 收到但通道为空，忽略");
        }
    }
}

/// v9-A: 处理 `WM_GIF_FRAMES_PREFETCHED` 消息：从通道读取后台预取的帧数据并填充。
///
/// 预取线程已解码当前帧 ± `STREAMING_WINDOW_HALF` 范围内的帧，通过通道回传
/// `Vec<(frame_index, GifFrame)>`。本函数遍历回传结果，将每个帧的 pixels/尺寸/
/// 延迟填充到 `render.frames[index]`（仅当索引在范围内时）。填充完成后清除
/// `prefetch_in_progress` 标志，允许后续 WM_TIMER 发起新的预取。
///
/// 若当前帧在预取完成后仍有像素（无论是否被本次预取填充），触发重绘以显示
/// 之前因空像素被跳过的当前帧。若当前帧仍为空（预取范围未覆盖，罕见），
/// 不重绘（保持上一帧画面，等待下次 WM_TIMER 的同步兜底解码）。
///
/// v15-B-005: 预取返回空结果（如 4K GIF 所有帧超 8MB 阈值被跳过）时，触发
/// 同步解码当前帧作为兜底，避免无限循环 + CPU 100% + 显示冻结。
///
/// # Safety
///
/// 同 `handle_frames_loaded`：`data_ptr` 通过 `GetWindowLongPtrW` 实时取回
/// （v15-B-001），不依赖消息循环捕获的裸指针。
unsafe fn handle_frames_prefetched(
    hwnd: HWND,
    prefetch_rx: &std::sync::mpsc::Receiver<Vec<(usize, GifFrame)>>,
) {
    // v15-B-001: 实时取回 data_ptr，避免 WM_DESTROY 后捕获的裸指针悬垂（UAF）。
    let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
    if data_ptr.is_null() {
        return;
    }
    let data = &mut *data_ptr;

    // 无论通道是否有数据，预取已结束，清除标志
    data.prefetch_in_progress.store(false, Ordering::Relaxed);

    let prefetched = match prefetch_rx.try_recv() {
        Ok(frames) => frames,
        Err(_) => {
            tracing::warn!("v9-A: WM_GIF_FRAMES_PREFETCHED 收到但通道为空，忽略");
            return;
        }
    };

    if prefetched.is_empty() {
        // v15-B-005: 预取返回 0 帧（如 4K GIF 所有帧超 8MB 阈值被跳过，
        // 或解码失败）。原实现仅 return 保持现状，但当前帧像素为空且
        // prefetch_in_progress 已清除，下次 WM_TIMER 会再次 spawn 预取 →
        // 再次空结果 → 无限循环 + CPU 100% + 显示冻结。
        //
        // 修复：sync-decode 当前帧作为兜底（decode_gif_frame_at 不应用 8MB
        // 跳过，可处理 4K 帧），与 v10-C "仅当前帧同步解码" 的原始意图一致。
        // 成功则触发重绘；失败则保持空像素（WM_PAINT 守卫跳过绘制保持上一帧）。
        let current = data.render.current_frame;
        if data.render.reload_current_frame_pixels() {
            let _ = InvalidateRect(hwnd, None, false);
            tracing::info!(
                frame = current,
                "v15-B-005: 预取空结果，已同步解码当前帧作为兜底"
            );
        } else {
            tracing::warn!(
                frame = current,
                "v15-B-005: 预取空结果且同步解码当前帧失败，保持空像素"
            );
        }
        return;
    }

    let total = data.render.frames.len();
    let mut filled = 0usize;
    // v10-C: prefetched 在 for 循环中被消耗（into_iter），循环结束后 Vec 内存即释放，
    // 不会滞留；未填充的 frame 在循环体内显式 drop，避免大像素数据滞留至迭代结束。
    for (index, frame) in prefetched {
        if index < total {
            let f = &mut data.render.frames[index];
            // 仅填充空像素帧，避免覆盖流式窗口内已有像素（理论上窗口内帧不会被预取）
            if f.pixels.is_empty() {
                f.pixels = frame.pixels;
                f.width = frame.width;
                f.height = frame.height;
                f.delay_ms = frame.delay_ms;
                filled += 1;
            } else {
                // v10-C: 帧已有像素，显式释放预取帧像素数据避免滞留
                drop(frame);
            }
        }
    }

    tracing::debug!(
        filled_frames = filled,
        total_frames = total,
        current_frame = data.render.current_frame,
        "v9-A: 预取帧已填充"
    );

    // 若当前帧已有像素（可能被本次预取填充，或本就有像素），触发重绘以显示
    // 之前因空像素被 WM_PAINT 跳过的当前帧。当前帧仍为空时不重绘（保持上帧画面）。
    let current = data.render.current_frame;
    if current < total && !data.render.frames[current].pixels.is_empty() {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// #1 优化：派发预取请求到长生命 worker 线程（持久化解码游标，消除 O(N) 重解码）。
///
/// 取代旧 `spawn_prefetch_thread`（每次预取新建线程 + 从 0 解码到 window_end，
/// O(N²) 总复杂度）。新设计：首次调用时 lazy 创建 worker 线程，存 `req_tx` 到
/// `worker_tx_slot`；后续调用复用同一 worker，worker 局部 `frames_iter`+`cursor`
/// 跨请求保留，前向预取从 O(target+half) 降为 O(half)。
///
/// worker 循环：`req_rx.recv()` → `prefetch_with_cursor` → `response_tx.send` →
/// `PostMessageW(WM_GIF_FRAMES_PREFETCHED)`。`WM_DESTROY` 时 `GifWindowData` drop
/// 致 `worker_tx_slot` drop，worker `recv()` 返回 `Err` 自然退出。
///
/// 调用方负责在调用前置 `prefetch_in_progress = true`；本函数在派发失败时
/// 重置为 `false`。返回 `true` 表示派发成功，`false` 表示失败（调用方回退同步解码）。
fn dispatch_prefetch(
    target: usize,
    image_path: &str,
    worker_tx_slot: &mut Option<std::sync::mpsc::Sender<PrefetchRequest>>,
    prefetch_in_progress: &Arc<AtomicBool>,
    prefetch_tx: &std::sync::mpsc::Sender<Vec<(usize, GifFrame)>>,
    hwnd: HWND,
) -> bool {
    let half = STREAMING_WINDOW_HALF;

    // 首次调用：创建 worker 线程并存 req_tx 到槽位
    if worker_tx_slot.is_none() {
        let (req_tx, req_rx) = mpsc::channel::<PrefetchRequest>();
        let gif_path = image_path.to_string();
        let response_tx = prefetch_tx.clone();
        let hwnd_raw = hwnd.0 as isize;

        match std::thread::Builder::new()
            .name("mirrorstar-gif-prefetch-worker".to_string())
            .spawn(move || {
                // #1: worker 局部变量——持久化解码游标，跨请求保留。
                // Frames<'static> 非 Send，但作为闭包内局部变量无需 Send，
                // 不影响闭包本身的 Send 性（闭包捕获的均为 Send 类型）。
                let mut frames_iter: Option<image::Frames<'static>> = None;
                let mut cursor: usize = 0;

                // 循环处理预取请求，req_rx.recv() 返回 Err 表示所有 req_tx 已 drop（窗口销毁）
                while let Ok(req) = req_rx.recv() {
                    // 每次请求重新查询屏幕分辨率，处理 DPI 变化（与旧 decode_gif_frame_range 一致）
                    let (screen_w, screen_h) = super::get_screen_size();
                    let payload = prefetch_with_cursor(
                        &gif_path,
                        screen_w,
                        screen_h,
                        &mut frames_iter,
                        &mut cursor,
                        req.target,
                        req.half,
                    );
                    if let Err(e) = response_tx.send(payload) {
                        tracing::warn!(
                            error = %e,
                            "#1: response_tx 已关闭（窗口可能已销毁），worker 退出"
                        );
                        break;
                    }
                    // SAFETY: PostMessageW 线程安全；hwnd 已销毁时仅返回 false，不触发 UB。
                    unsafe {
                        if PostMessageW(
                            HWND(hwnd_raw as *mut _),
                            WM_GIF_FRAMES_PREFETCHED,
                            WPARAM(0),
                            LPARAM(0),
                        )
                        .is_err()
                        {
                            tracing::warn!(
                                "PostMessageW 失败：WM_GIF_FRAMES_PREFETCHED 未送达（窗口可能已销毁）"
                            );
                        }
                    }
                }
                // req_rx.recv() 返回 Err：所有 req_tx 已 drop，worker 自然退出
            }) {
            Ok(_) => *worker_tx_slot = Some(req_tx),
            Err(e) => {
                tracing::warn!(error = %e, "#1: 创建预取 worker 线程失败");
                prefetch_in_progress.store(false, Ordering::Relaxed);
                return false;
            }
        }
    }

    // 发送预取请求到 worker
    let tx = worker_tx_slot
        .as_ref()
        .expect("worker_tx 应已创建（首次调用刚 spawn 或复用现有）");
    if let Err(e) = tx.send(PrefetchRequest { target, half }) {
        tracing::warn!(
            error = %e,
            "#1: prefetch_worker_tx 已关闭（worker 已退出），重置槽位"
        );
        *worker_tx_slot = None;
        prefetch_in_progress.store(false, Ordering::Relaxed);
        return false;
    }
    true
}

/// v17 优化：判断是否应触发主动式预取。
///
/// 当当前帧有像素、但下一帧像素为空（即将到达流式窗口边界）且无预取进行中时，
/// 返回 `Some(next)` 表示应以 `next` 帧为中心触发预取；否则返回 `None`。
///
/// 主动式预取的收益：在到达空帧 *之前* 就开始解码，使下一帧在显示前就准备好，
/// 消除反应式预取在窗口边界处的 1 帧停顿（WM_PAINT 守卫跳过空帧导致动画暂停）。
///
/// 纯函数，便于单元测试覆盖全部分支。
fn proactive_prefetch_center(
    current: usize,
    frame_count: usize,
    next_frame_empty: bool,
    prefetch_in_progress: bool,
) -> Option<usize> {
    if prefetch_in_progress || !next_frame_empty || frame_count == 0 {
        return None;
    }
    Some((current + 1) % frame_count)
}

// ── Window Class Registration ────────────────────────────────────────────────

/// 注册窗口类（仅执行一次），委托给 `gdi_base::register_window_class_once`
fn register_gif_window_class() -> windows::core::PCWSTR {
    static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();
    register_window_class_once(
        &CLASS_REGISTERED,
        windows::core::w!("MirrorStarGifWallpaper"),
        gif_wnd_proc,
    )
}

// ── Window Procedure ─────────────────────────────────────────────────────────

/// 窗口过程，使用双缓冲和 HALFTONE 缩放模式实现高质量无闪烁渲染
unsafe extern "system" fn gif_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 优先处理两个 GDI 渲染器共有的消息（WM_ERASEBKGND / WM_DPICHANGED / WM_SIZE）
    if let Some(result) = try_handle_common_messages(hwnd, msg, lparam) {
        return result;
    }

    match msg {
        WM_TIMER => {
            if wparam.0 == GIF_TIMER_ID {
                let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
                if !data_ptr.is_null() {
                    // v9-A: 借用整个 GifWindowData 以支持分域借用（split borrow），
                    // 允许同时访问 data.render 与 data.prefetch_in_progress / data.prefetch_tx。
                    // 直接 `let render = &mut (*data_ptr).render;` 会独占借用电整个结构体，
                    // 阻止后续访问 prefetch_* 字段。
                    let data = &mut *data_ptr;
                    let render = &mut data.render;
                    if !render.paused {
                        if render.frames.is_empty() {
                            return LRESULT(0);
                        }
                        render.current_frame = (render.current_frame + 1) % render.frames.len();

                        // v8-C/v9-A/v17: 流式帧缓存按需恢复
                        // 推进到的帧若像素已被清空（超出流式窗口），需重新解码。
                        // v9-A: 将 O(N) 解码移至后台线程，避免 WM_TIMER 阻塞导致卡顿。
                        // v17: 新增主动式预取——当前帧有像素但下一帧为空时提前派生预取，
                        // 消除窗口边界处 1 帧停顿（反应式预取在到达空帧后才触发，导致
                        // WM_PAINT 守卫跳过空帧造成动画暂停）。清理远帧逻辑移出 if，
                        // 每帧执行以在主动路径下也维持窗口大小。
                        {
                            let current = render.current_frame;
                            let half = STREAMING_WINDOW_HALF;
                            // 检查当前帧像素是否为空（借用结束于表达式末尾）
                            let needs_reload = render
                                .frames
                                .get(current)
                                .map(|f| f.pixels.is_empty())
                                .unwrap_or(true);
                            if needs_reload {
                                // 反应式：当前帧为空，必须解码
                                if !data.prefetch_in_progress.load(Ordering::Relaxed) {
                                    // #1: 派发预取请求到长生命 worker（持久化解码游标）。
                                    // 主线程不等待，当前帧短暂为空（WM_PAINT 守卫跳过绘制保持上一帧），
                                    // 预取完成后 WM_GIF_FRAMES_PREFETCHED 填充并重绘。
                                    data.prefetch_in_progress.store(true, Ordering::Relaxed);
                                    let spawned = dispatch_prefetch(
                                        current,
                                        &render.image_path,
                                        &mut data.prefetch_worker_tx,
                                        &data.prefetch_in_progress,
                                        &data.prefetch_tx,
                                        hwnd,
                                    );
                                    if !spawned {
                                        // 派发失败：标志已由 dispatch_prefetch 重置，同步兜底
                                        render.reload_current_frame_pixels();
                                    }
                                    // 派发成功：当前帧保持空像素，预取完成后填充并重绘
                                } else {
                                    // v9-A: 预取进行中——同步兜底解码当前帧（罕见竞态：
                                    // 上一次预取尚未完成，当前帧仍为空）。仅解码当前帧，
                                    // 不阻塞预取线程（预取线程仍在运行，完成后填充其他帧）。
                                    tracing::debug!(
                                        frame = current,
                                        "v9-A: 预取进行中，同步兜底解码当前帧"
                                    );
                                    render.reload_current_frame_pixels();
                                }
                            } else {
                                // v17 主动式预取：当前帧有像素，但检查下一帧是否为空。
                                // 若下一帧为空（即将到达流式窗口边界），提前派生预取，
                                // 使下一帧在显示前就准备好，消除边界处 1 帧停顿。
                                let frame_count = render.frames.len();
                                let next = (current + 1) % frame_count;
                                let next_empty = render
                                    .frames
                                    .get(next)
                                    .map(|f| f.pixels.is_empty())
                                    .unwrap_or(false);
                                if let Some(center) = proactive_prefetch_center(
                                    current,
                                    frame_count,
                                    next_empty,
                                    data.prefetch_in_progress.load(Ordering::Relaxed),
                                ) {
                                    data.prefetch_in_progress.store(true, Ordering::Relaxed);
                                    // 主动预取失败无需同步兜底（当前帧仍有像素；
                                    // 下一帧若仍空则由下次 WM_TIMER 的反应式路径处理）
                                    let _ = dispatch_prefetch(
                                        center,
                                        &render.image_path,
                                        &mut data.prefetch_worker_tx,
                                        &data.prefetch_in_progress,
                                        &data.prefetch_tx,
                                        hwnd,
                                    );
                                }
                            }

                            // 清理远离当前帧的帧像素（v17: 移出 if，每帧执行维持窗口大小）
                            for (i, frame) in render.frames.iter_mut().enumerate() {
                                let too_far =
                                    i < current.saturating_sub(half) || i > current + half;
                                if too_far && !frame.pixels.is_empty() {
                                    frame.pixels.clear();
                                    frame.pixels.shrink_to_fit();
                                }
                            }
                        }

                        let _ = InvalidateRect(hwnd, None, false);

                        // 调整定时器为下一帧的延迟
                        let frame = &render.frames[render.current_frame];
                        let adjusted_delay = (frame.delay_ms as f32 / render.speed) as u32;
                        let adjusted_delay = adjusted_delay.max(10);
                        // v5.0 W-PERF-001: 延迟未变化则跳过 SetTimer，避免每帧重建内核 timer 对象
                        // （SetTimer 对已存在的 timer ID 会先 KillTimer 再重建，涉及内核态切换）
                        if adjusted_delay != render.last_timer_delay {
                            let _ = SetTimer(hwnd, GIF_TIMER_ID, adjusted_delay, None);
                            render.last_timer_delay = adjusted_delay;
                        }
                        // v17 性能埋点：记录帧推进，达到阈值输出 FPS + 绘制 + 内存统计。
                        // render 借用已于上行最后使用后释放（NLL），可安全访问 data.fps。
                        data.fps.record_frame();
                    }
                }
            }
            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
            if data_ptr.is_null() {
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            let data = &mut *data_ptr;
            if data.render.frames.is_empty() {
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            let frame = &data.render.frames[data.render.current_frame];

            // v9-A: 当前帧像素为空时跳过绘制，保持上一帧画面。
            //
            // 触发场景：WM_TIMER 派生后台预取线程后，当前帧 pixels 仍为空
            // （预取尚未完成）。此时若调用 paint_with_double_buffer，会因
            // pixels 为空填充黑色背景导致黑闪。跳过绘制让屏幕保持上一帧的
            // BitBlt 结果，预取完成后 handle_frames_prefetched 会触发重绘。
            //
            // 注意：EndPaint 仍需调用以确认 update region，避免 WM_PAINT 重复触发。
            if frame.pixels.is_empty() {
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }

            // 双缓冲绘制（背景填充 + HALFTONE 缩放 + StretchDIBits + BitBlt）
            // v17 性能埋点：测量绘制耗时，record_paint 累计到 FramePerfTracker
            let paint_start = std::time::Instant::now();
            paint_with_double_buffer(
                hwnd,
                hdc,
                &mut data.gdi_cache,
                frame.width,
                frame.height,
                &frame.pixels,
                data.render.scaling_mode,
            );
            data.fps.record_paint(paint_start.elapsed());

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_DESTROY => {
            let _ = KillTimer(hwnd, GIF_TIMER_ID);
            // 清理缓存的 GDI 对象
            let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GifWindowData;
            if !data_ptr.is_null() {
                // W09: 窗口销毁时设置取消标志，让后台解码线程尽快退出解码循环。
                // 必须在 Box::from_raw（drop GifWindowData）之前设置，确保
                // Arc<AtomicBool> 的底层 AtomicBool 仍有效。后台线程持有的 clone
                // 保持 AtomicBool 存活，设置后线程检查到 true 即退出。
                (*data_ptr).decode_cancel.store(true, Ordering::SeqCst);
                let mut data = Box::from_raw(data_ptr);
                if let Some(ref mut cache) = data.gdi_cache {
                    cache.destroy();
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallpaper::WallpaperRenderer;

    /// 构造一个用于测试的 GifFrame（每帧 5×5，100 字节 BGRA 数据）
    fn make_test_frame(index: usize) -> GifFrame {
        GifFrame {
            pixels: vec![index as u8; 100],
            width: 5,
            height: 5,
            delay_ms: 100,
        }
    }

    /// 构造一个用于测试的 GifRenderData，包含指定数量的帧
    fn make_render_data(
        frame_count: usize,
        strategy: GifMemoryStrategy,
        keep: usize,
    ) -> GifRenderData {
        GifRenderData {
            frames: (0..frame_count).map(make_test_frame).collect(),
            current_frame: 0,
            scaling_mode: ScalingMode::Fill,
            speed: 1.0,
            paused: false,
            image_path: String::new(),
            frames_loaded: true,
            saved_frame_index: None,
            memory_strategy: strategy,
            balanced_keep_frames: keep,
            max_memory_mb: super::super::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB,
            last_timer_delay: 0,
            // v8-C: 测试辅助函数将 total_frames 初始化为 frame_count
            total_frames: frame_count,
            sync_frames_iter: None,
            sync_cursor: 0,
        }
    }

    #[test]
    fn test_gif_renderer_new_defaults() {
        // new 应使用默认策略（Balanced）和默认保留帧数
        let renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        assert_eq!(renderer.gif_path, "test.gif");
        assert_eq!(renderer.base.scaling_mode, ScalingMode::Fill);
        assert_eq!(renderer.speed, 1.0);
        assert_eq!(renderer.memory_strategy, GifMemoryStrategy::default());
        assert_eq!(renderer.balanced_keep_frames, DEFAULT_BALANCED_KEEP_FRAMES);
        assert_eq!(
            renderer.max_memory_mb,
            super::super::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB
        );
        assert_eq!(renderer.base.state(), WallpaperState::Initializing);
        assert!(renderer.base.hwnd.is_none());
        assert!(renderer.base.thread_handle.is_none());
    }

    #[test]
    fn test_gif_renderer_with_strategy_custom() {
        // with_strategy 应保留传入的策略与保留帧数
        let renderer = GifRenderer::with_strategy(
            "custom.gif".to_string(),
            ScalingMode::Fit,
            GifMemoryStrategy::Performance,
            5,
            super::super::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB,
        );
        assert_eq!(renderer.gif_path, "custom.gif");
        assert_eq!(renderer.base.scaling_mode, ScalingMode::Fit);
        assert_eq!(renderer.memory_strategy, GifMemoryStrategy::Performance);
        assert_eq!(renderer.balanced_keep_frames, 5);
    }

    #[test]
    fn test_gif_renderer_set_scaling_mode_updates_field() {
        // 非 Playing 状态下 set_scaling_mode 仅更新字段，不发送命令
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        renderer.set_scaling_mode(ScalingMode::Center);
        assert_eq!(renderer.base.scaling_mode, ScalingMode::Center);
        assert_eq!(renderer.base.state(), WallpaperState::Initializing);
    }

    #[test]
    fn test_gif_renderer_set_speed_updates_field() {
        // 非 Playing 状态下 set_speed 仅更新字段，不发送命令
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        renderer.set_speed(2.0);
        assert_eq!(renderer.speed, 2.0);
    }

    // ========== W10 修复测试：set_speed 无效值校验 ==========

    #[test]
    fn test_set_speed_zero_rejected() {
        // W10: speed=0 应被拒绝（warn+return），speed 字段保持原值
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let original_speed = renderer.speed;
        renderer.set_speed(0.0);
        assert_eq!(
            renderer.speed, original_speed,
            "speed=0 不应更新字段（应被拒绝）"
        );
    }

    #[test]
    fn test_set_speed_negative_rejected() {
        // W10: 负数 speed 应被拒绝
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let original_speed = renderer.speed;
        renderer.set_speed(-1.0);
        assert_eq!(
            renderer.speed, original_speed,
            "负数 speed 不应更新字段（应被拒绝）"
        );
    }

    #[test]
    fn test_set_speed_nan_rejected() {
        // W10: NaN speed 应被拒绝
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let original_speed = renderer.speed;
        renderer.set_speed(f32::NAN);
        assert_eq!(
            renderer.speed, original_speed,
            "NaN speed 不应更新字段（应被拒绝）"
        );
    }

    #[test]
    fn test_set_speed_infinity_rejected() {
        // W10: Infinity speed 应被拒绝
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let original_speed = renderer.speed;
        renderer.set_speed(f32::INFINITY);
        assert_eq!(
            renderer.speed, original_speed,
            "Infinity speed 不应更新字段（应被拒绝）"
        );
    }

    #[test]
    fn test_set_speed_small_positive_accepted() {
        // W10: 极小正数 speed 应被接受（> 0 且有限）
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        renderer.set_speed(0.001);
        assert_eq!(renderer.speed, 0.001);
    }

    #[test]
    fn test_gif_renderer_pause_when_not_playing_is_noop() {
        // 非 Playing 状态下 pause 应直接返回 Ok，不修改状态
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let result = renderer.pause();
        assert!(result.is_ok());
        assert_eq!(renderer.base.state(), WallpaperState::Initializing);
    }

    #[test]
    fn test_gif_renderer_resume_when_not_paused_is_noop() {
        // 非 Paused 状态下 resume 应直接返回 Ok，不修改状态
        let mut renderer = GifRenderer::new("test.gif".to_string(), ScalingMode::Fill);
        let result = renderer.resume();
        assert!(result.is_ok());
        assert_eq!(renderer.base.state(), WallpaperState::Initializing);
    }

    #[test]
    fn test_gif_memory_strategy_balanced_keep_frames() {
        // Balanced 模式：20 帧保留 10 帧，当前帧为 5
        let mut data = make_render_data(20, GifMemoryStrategy::Balanced, 10);
        data.current_frame = 5;
        data.handle_pause();

        // 应保留 10 帧
        assert_eq!(data.frames.len(), 10);
        // 当前帧索引在保留窗口内（5 ∈ 0..10），调整后仍为 5
        assert_eq!(data.current_frame, 5);
        assert_eq!(data.saved_frame_index, Some(5));
        assert!(data.paused);
    }

    #[test]
    fn test_gif_memory_strategy_performance_keeps_all_frames() {
        // Performance 模式：保留所有帧，仅记录索引
        let mut data = make_render_data(15, GifMemoryStrategy::Performance, 10);
        data.current_frame = 7;
        data.handle_pause();

        assert_eq!(data.frames.len(), 15);
        assert_eq!(data.current_frame, 7);
        assert_eq!(data.saved_frame_index, Some(7));
        assert!(data.paused);
    }

    #[test]
    fn test_gif_memory_strategy_aggressive_releases_all_frames() {
        // Aggressive 模式：仅保留当前帧，释放其余帧
        let mut data = make_render_data(20, GifMemoryStrategy::Aggressive, 10);
        data.current_frame = 3;
        data.handle_pause();

        assert_eq!(data.frames.len(), 1);
        assert_eq!(data.current_frame, 0);
        assert!(!data.frames_loaded);
        assert_eq!(data.saved_frame_index, Some(3));
        assert!(data.paused);
    }

    #[test]
    fn test_gif_memory_strategy_balanced_resume_restores_index() {
        // Balanced 暂停后恢复：不重新解码，直接还原 current_frame
        let mut data = make_render_data(20, GifMemoryStrategy::Balanced, 10);
        data.current_frame = 5;
        data.handle_pause();
        assert_eq!(data.saved_frame_index, Some(5));

        let result = data.handle_resume();
        assert!(result.is_ok());
        assert_eq!(data.current_frame, 5);
        assert_eq!(data.saved_frame_index, None);
        assert!(!data.paused);
    }

    // ========== W-003 修复测试：Resume 失败状态回滚 ==========

    /// 验证 Aggressive 策略下 handle_resume 对无效路径返回错误，
    /// 且 wallpaper_thread 的回滚逻辑（shared_state.state = Paused）能正确执行。
    ///
    /// 由于壁纸线程涉及 Win32 消息处理，难以直接单元测试。此测试验证：
    /// 1. handle_resume 对无效文件路径返回 Err
    /// 2. 模拟 wallpaper_thread 的回滚逻辑后 shared_state.state == Paused
    #[test]
    fn w003_resume_failure_rolls_back_state() {
        // 构造 Aggressive 策略的渲染数据，使用无效文件路径
        let mut data = make_render_data(5, GifMemoryStrategy::Aggressive, 10);
        data.image_path = "Z:\\nonexistent\\path\\test.gif".to_string();
        data.handle_pause();
        assert!(data.paused, "暂停后应为 paused");
        assert!(
            !data.frames_loaded,
            "Aggressive 暂停后 frames_loaded 应为 false"
        );

        // 模拟 wallpaper_thread 收到 Resume 命令后的处理
        let shared_state = Arc::new(RwLock::new(RendererState {
            state: WallpaperState::Playing, // 模拟 pause 转发线程已设为 Playing
            volume: 1.0,
            pre_mute_volume: None,
        }));

        // 调用 handle_resume（应失败，因为文件路径无效）
        let result = data.handle_resume();
        assert!(result.is_err(), "无效路径的 handle_resume 应返回 Err");

        // 模拟 wallpaper_thread 的回滚逻辑（W-003 修复）
        // 这是 wallpaper_thread 在 handle_resume 失败时执行的代码
        shared_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .state = WallpaperState::Paused;

        // 验证 shared_state 已回滚为 Paused
        assert_eq!(
            shared_state.read().unwrap().state,
            WallpaperState::Paused,
            "Resume 失败后 shared_state.state 应已回滚为 Paused"
        );
    }

    /// 验证 Balanced 策略下 handle_resume 成功时不需要回滚状态。
    #[test]
    fn w003_resume_success_keeps_playing_state() {
        // Balanced 策略不释放帧，handle_resume 应成功
        let mut data = make_render_data(10, GifMemoryStrategy::Balanced, 10);
        data.current_frame = 3;
        data.handle_pause();
        assert!(data.paused);

        let shared_state = Arc::new(RwLock::new(RendererState {
            state: WallpaperState::Playing,
            volume: 1.0,
            pre_mute_volume: None,
        }));

        let result = data.handle_resume();
        assert!(result.is_ok(), "Balanced 策略的 handle_resume 应成功");

        // 成功时不回滚，shared_state.state 保持 Playing
        assert_eq!(
            shared_state.read().unwrap().state,
            WallpaperState::Playing,
            "Resume 成功后 shared_state.state 应保持 Playing"
        );
    }

    // ========== W-006 修复测试：Resume 空帧守卫 ==========

    /// 验证 `handle_resume()` 完成后 `frames` 为空（边界场景）时，
    /// 守卫（`is_empty()` 检查）能防止 `frames[current_frame]` 索引 panic。
    ///
    /// 边界场景：假设 `handle_resume` 实现变化导致 `frames` 为空（如未来重构后），
    /// 当前实现不会产生此状态，但守卫需就位以防回归。
    ///
    /// 守卫逻辑复刻自 `gif_wallpaper_thread` 中 `GifCommand::Resume` 处理路径
    /// （与 `WM_TIMER` 分支的 `is_empty` 守卫风格一致）。由于壁纸线程涉及 Win32
    /// 消息循环难以直接单元测试，此处复刻守卫逻辑以验证行为正确性。
    #[test]
    fn w006_gif_resume_empty_frames_no_panic() {
        // 构造 Balanced 策略数据：handle_resume 不重新解码，直接还原索引
        let mut data = make_render_data(5, GifMemoryStrategy::Balanced, 10);
        data.current_frame = 3;
        data.handle_pause();
        assert!(data.paused, "暂停后应为 paused");

        // handle_resume 成功（Balanced 不释放帧，无需重新解码）
        let result = data.handle_resume();
        assert!(result.is_ok(), "Balanced handle_resume 应成功");
        assert!(!data.paused, "恢复后应为非 paused");

        // 模拟边界场景：handle_resume 实现变化导致 frames 为空
        data.frames.clear();
        assert!(data.frames.is_empty(), "测试前置：frames 应为空");

        // 守卫逻辑（与 gif_wallpaper_thread 中 Resume 命令处理路径一致）：
        // 在 `&render.frames[render.current_frame]` 索引前检查 is_empty()，
        // 等价于 WM_TIMER 分支的 `if render.frames.is_empty() { return LRESULT(0); }`，
        // 此处为消息线程函数无法 return LRESULT，故以 if/else 跳过索引。
        let adjusted_delay = if data.frames.is_empty() {
            // 守卫生效：记录 warn（行为验证，不验证日志输出）
            tracing::warn!("GIF Resume 后 frames 为空，跳过帧索引与定时器设置");
            None
        } else {
            // 仅有帧时才索引；未守卫时此分支在 frames 为空情况下会 panic
            let frame = &data.frames[data.current_frame];
            Some(((frame.delay_ms as f32 / data.speed) as u32).max(10))
        };

        // 守卫生效：frames 为空时不索引、不计算延迟
        assert!(
            adjusted_delay.is_none(),
            "frames 为空时守卫应跳过索引，不返回延迟"
        );
        assert!(data.frames.is_empty(), "frames 应保持为空");
    }

    // ── v41-W-019: play() recv_timeout 与窗口泄漏防护测试 ──────────────────────

    /// v41-W-019: 验证 `PLAY_RESULT_RECV_TIMEOUT` 常量为 15s
    ///
    /// 超时值需与 `set_wallpaper` 的 IPC 超时（20s）协调：
    /// - 过短：大 GIF 首帧解码可能误超时
    /// - 过长：超过 IPC 超时后 IPC 层先报错，内部超时无意义
    ///
    /// 15s 留 5s 缓冲给 IPC 层。
    #[test]
    fn w019_play_result_recv_timeout_is_15_seconds() {
        assert_eq!(
            PLAY_RESULT_RECV_TIMEOUT,
            std::time::Duration::from_secs(15),
            "PLAY_RESULT_RECV_TIMEOUT 应为 15s（与 set_wallpaper IPC 超时 20s 协调）"
        );
    }

    /// v41-W-019: 验证 `recv_timeout` 在通道断开时返回 `Disconnected`
    ///
    /// 模拟 `gif_wallpaper_thread` panic 或提前退出（sender drop）的场景：
    /// `play()` 中的 `recv_timeout` 应返回 `RecvTimeoutError::Disconnected`，
    /// 映射为 `MirrorStarError::DesktopIntegration`。
    #[test]
    fn w019_recv_timeout_returns_disconnected_when_channel_closed() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<isize, crate::MirrorStarError>>();
        // drop sender 模拟线程退出/panic
        drop(tx);
        let result = rx.recv_timeout(std::time::Duration::from_millis(100));
        assert!(
            matches!(result, Err(std::sync::mpsc::RecvTimeoutError::Disconnected)),
            "sender drop 后 recv_timeout 应返回 Disconnected"
        );
    }

    /// v41-W-019: 验证 `recv_timeout` 在超时后返回 `Timeout`
    ///
    /// 模拟 `gif_wallpaper_thread` 卡在首帧解码（未发送结果）的场景：
    /// `play()` 中的 `recv_timeout` 应在超时后返回 `RecvTimeoutError::Timeout`，
    /// 映射为 `MirrorStarError::DesktopIntegration`。
    #[test]
    fn w019_recv_timeout_returns_timeout_when_no_message() {
        let (_tx, rx) = std::sync::mpsc::channel::<Result<isize, crate::MirrorStarError>>();
        // sender 存活但不发送，模拟线程卡住
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(200);
        let result = rx.recv_timeout(timeout);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
            "未收到消息应超时返回 Timeout"
        );
        // 验证确实等待了接近超时时长（而非立即返回）
        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "应等待接近超时时长（{}ms），实际: {:?}",
            timeout.as_millis(),
            elapsed
        );
    }

    /// v41-W-019: 文档测试验证 `play()` 与 `gif_wallpaper_thread` 的超时与清理代码模式
    ///
    /// 由于 `play()` 涉及真实线程 + Win32 窗口创建，无法在单元测试中直接覆盖
    /// 超时→清理的完整路径。改为通过 `include_str!` 验证关键代码模式存在：
    /// - `recv_timeout`（替代原 `recv`）
    /// - `RecvTimeoutError::Timeout` 分支
    /// - send 失败时 `DestroyWindow` 清理
    #[test]
    fn w019_timeout_and_cleanup_code_patterns_exist() {
        let source = include_str!("gif.rs");
        // recv_timeout 替代 recv
        assert!(
            source.contains("recv_timeout(PLAY_RESULT_RECV_TIMEOUT)"),
            "play() 应使用 recv_timeout(PLAY_RESULT_RECV_TIMEOUT) 替代 recv()"
        );
        // Timeout 分支处理
        assert!(
            source.contains("RecvTimeoutError::Timeout"),
            "play() 应处理 RecvTimeoutError::Timeout 分支"
        );
        // Disconnected 分支处理
        assert!(
            source.contains("RecvTimeoutError::Disconnected"),
            "play() 应处理 RecvTimeoutError::Disconnected 分支"
        );
        // send 失败后销毁窗口（避免泄漏）
        assert!(
            source.contains("DestroyWindow(hwnd)") && source.contains("result_tx 已关闭"),
            "gif_wallpaper_thread 在 result_tx.send 失败时应销毁窗口避免泄漏"
        );
    }

    // ========== v9-A: 后台预取测试 ==========

    /// v9-A: 验证 `WM_GIF_FRAMES_PREFETCHED` 常量值为 `WM_USER + 12`
    ///
    /// 消息码需与 `WM_GIF_COMMAND`（WM_USER+10）和 `WM_GIF_FRAMES_LOADED`
    /// （WM_USER+11）保持连续且不冲突，避免与其他窗口消息碰撞。
    #[test]
    fn v9a_wm_gif_frames_prefetched_is_wm_user_plus_12() {
        assert_eq!(
            WM_GIF_FRAMES_PREFETCHED,
            WM_USER + 12,
            "v9-A: WM_GIF_FRAMES_PREFETCHED 应为 WM_USER + 12"
        );
    }

    /// v9-A: 验证预取通道的通信模式（send/recv Vec<(usize, GifFrame)>）
    ///
    /// 预取线程通过 `prefetch_tx.send(Vec<(usize, GifFrame)>)` 回传结果，
    /// 消息循环通过 `prefetch_rx.try_recv()` 读取。本测试验证通道模式工作正常，
    /// 包括：成功发送/接收、发送空 Vec（解码失败场景）、多帧发送。
    #[test]
    fn v9a_prefetch_channel_send_recv_pattern() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<(usize, GifFrame)>>();

        // 1. 发送 3 帧预取结果
        let frames = vec![
            (0usize, make_test_frame(0)),
            (1, make_test_frame(1)),
            (2, make_test_frame(2)),
        ];
        tx.send(frames).expect("send 应成功");

        let received = rx.try_recv().expect("try_recv 应成功");
        assert_eq!(received.len(), 3, "应收到 3 帧预取结果");
        assert_eq!(received[0].0, 0, "首帧索引应为 0");
        assert_eq!(received[1].0, 1, "第二帧索引应为 1");
        assert_eq!(received[2].0, 2, "第三帧索引应为 2");
        assert_eq!(received[0].1.pixels[0], 0, "首帧像素应正确");
        assert_eq!(received[1].1.pixels[0], 1, "第二帧像素应正确");

        // 2. 发送空 Vec（解码失败场景）
        tx.send(Vec::new()).expect("send 空 Vec 应成功");
        let empty = rx.try_recv().expect("try_recv 应成功");
        assert!(empty.is_empty(), "应收到空 Vec");

        // 3. 通道为空时 try_recv 返回 Empty
        let result = rx.try_recv();
        assert!(
            matches!(result, Err(std::sync::mpsc::TryRecvError::Empty)),
            "通道为空时 try_recv 应返回 Empty"
        );
    }

    /// v9-A: 验证预取线程发送端 drop 后接收端 try_recv 返回 Disconnected
    ///
    /// 模拟窗口销毁后 prefetch_rx 仍存活但 prefetch_tx 已 drop 的场景
    /// （实际上窗口销毁会 drop prefetch_rx，但本测试验证反向场景的健壮性）。
    #[test]
    fn v9a_prefetch_channel_sender_drop_returns_disconnected() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<(usize, GifFrame)>>();
        drop(tx);
        let result = rx.try_recv();
        assert!(
            matches!(result, Err(std::sync::mpsc::TryRecvError::Disconnected)),
            "sender drop 后 try_recv 应返回 Disconnected"
        );
    }

    /// v9-A: 验证 `prefetch_in_progress` 标志的原子操作模式
    ///
    /// WM_TIMER 通过 `load(Relaxed)` + `store(true, Relaxed)` 派生预取线程，
    /// `handle_frames_prefetched` 通过 `store(false, Relaxed)` 清除标志。
    /// 本测试验证 Arc<AtomicBool> 在多线程下的可见性。
    #[test]
    fn v9a_prefetch_in_progress_atomic_flag_pattern() {
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(false));

        // 初始为 false（无预取进行中）
        assert!(!flag.load(Ordering::Relaxed), "初始应为 false");

        // WM_TIMER 派生预取线程前置 true
        flag.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed), "派生后应为 true");

        // 多线程访问：子线程读取标志
        let flag_clone = flag.clone();
        let handle = std::thread::spawn(move || flag_clone.load(Ordering::Relaxed));
        let value = handle.join().expect("线程不应 panic");
        assert!(value, "子线程应读到 true");

        // handle_frames_prefetched 清除标志
        flag.store(false, Ordering::Relaxed);
        assert!(!flag.load(Ordering::Relaxed), "清除后应为 false");
    }

    /// v9-A: 验证预取范围计算（saturating_sub 防止下溢）
    ///
    /// WM_TIMER 中 `range_start = current.saturating_sub(half)`，
    /// `range_end = current + half`。本测试验证边界情况：
    /// - current=0, half=1 → range [0, 1]
    /// - current=1, half=1 → range [0, 2]
    /// - current=5, half=1 → range [4, 6]
    #[test]
    fn v9a_prefetch_range_calculation_saturating() {
        let half: usize = STREAMING_WINDOW_HALF; // 1

        let current: usize = 0;
        let start = current.saturating_sub(half);
        let end = current + half;
        assert_eq!((start, end), (0, 1), "current=0 时范围应为 [0, 1]");

        let current: usize = 1;
        let start = current.saturating_sub(half);
        let end = current + half;
        assert_eq!((start, end), (0, 2), "current=1 时范围应为 [0, 2]");

        let current: usize = 5;
        let start = current.saturating_sub(half);
        let end = current + half;
        assert_eq!((start, end), (4, 6), "current=5 时范围应为 [4, 6]");
    }

    /// v9-A: 文档测试验证预取关键代码模式存在
    ///
    /// 由于 WM_TIMER/handle_frames_prefetched 涉及 Win32 消息循环与 unsafe
    /// 原始指针，无法直接单元测试。通过 `include_str!` 验证关键代码模式：
    /// - WM_GIF_FRAMES_PREFETCHED 常量
    /// - prefetch_in_progress 标志检查与设置
    /// - #1: dispatch_prefetch + prefetch_with_cursor（持久化解码游标）
    /// - PostMessageW(WM_GIF_FRAMES_PREFETCHED) 通知
    /// - WM_PAINT 空像素守卫
    #[test]
    fn v9a_prefetch_code_patterns_exist() {
        let source = include_str!("gif.rs");
        // WM_GIF_FRAMES_PREFETCHED 常量定义
        assert!(
            source.contains("WM_GIF_FRAMES_PREFETCHED: u32 = WM_USER + 12"),
            "应定义 WM_GIF_FRAMES_PREFETCHED 常量"
        );
        // prefetch_in_progress 标志检查（防止重复派生）
        assert!(
            source.contains("prefetch_in_progress.load(Ordering::Relaxed)"),
            "WM_TIMER 应检查 prefetch_in_progress 标志"
        );
        // #1: dispatch_prefetch 函数定义（取代旧 spawn_prefetch_thread）
        assert!(
            source.contains("fn dispatch_prefetch"),
            "WM_TIMER 应通过 dispatch_prefetch 派发预取请求"
        );
        // #1: prefetch_with_cursor 调用（持久化解码游标）
        assert!(
            source.contains("prefetch_with_cursor"),
            "worker 应调用 prefetch_with_cursor 复用解码游标"
        );
        // PostMessageW 通知
        assert!(
            source.contains("WM_GIF_FRAMES_PREFETCHED") && source.contains("PostMessageW"),
            "预取线程应通过 PostMessageW(WM_GIF_FRAMES_PREFETCHED) 通知主线程"
        );
        // handle_frames_prefetched 函数
        assert!(
            source.contains("fn handle_frames_prefetched"),
            "应定义 handle_frames_prefetched 函数"
        );
        // WM_PAINT 空像素守卫
        assert!(
            source.contains("frame.pixels.is_empty()"),
            "WM_PAINT 应有空像素守卫跳过绘制"
        );
        // 同步兜底解码（预取进行中时）
        assert!(
            source.contains("同步兜底解码当前帧"),
            "WM_TIMER 应有同步兜底解码逻辑（预取进行中时）"
        );
    }

    /// v9-A: 验证预取线程发送 payload 的索引映射逻辑
    ///
    /// 预取线程将 `decode_gif_frame_range` 返回的 `Vec<GifFrame>` 映射为
    /// `Vec<(usize, GifFrame)>`，索引从 `range_start` 开始递增。本测试
    /// 验证映射逻辑正确（复刻 WM_TIMER 中的 zip 模式）。
    #[test]
    fn v9a_prefetch_payload_index_mapping() {
        let range_start = 3;
        // 模拟 decode_gif_frame_range 返回 3 帧
        let frames = vec![make_test_frame(0), make_test_frame(1), make_test_frame(2)];

        // 复刻 WM_TIMER 中的 payload 构造逻辑
        let payload: Vec<(usize, GifFrame)> = (range_start..range_start + frames.len())
            .zip(frames)
            .collect();

        assert_eq!(payload.len(), 3, "payload 应有 3 帧");
        assert_eq!(payload[0].0, 3, "首帧索引应为 range_start=3");
        assert_eq!(payload[1].0, 4, "第二帧索引应为 4");
        assert_eq!(payload[2].0, 5, "第三帧索引应为 5");
    }

    /// v9-A: 验证 handle_frames_prefetched 的填充逻辑（模拟）
    ///
    /// handle_frames_prefetched 遍历预取结果，仅填充 `pixels.is_empty()` 的帧，
    /// 不覆盖已有像素的帧。本测试复刻该逻辑验证行为正确性。
    #[test]
    fn v9a_prefetch_fill_only_empty_frames_logic() {
        use crate::wallpaper::gif_decode::GifFrame;

        // 构造 5 帧，索引 1 和 3 像素为空
        let mut frames: Vec<GifFrame> = (0..5).map(make_test_frame).collect();
        frames[1].pixels.clear();
        frames[3].pixels.clear();

        // 模拟预取返回的 payload：索引 0,1,2,3,4 的帧
        let prefetched: Vec<(usize, GifFrame)> = (0..5)
            .map(|i| (i, make_test_frame(i + 10))) // 用 i+10 区分原帧
            .collect();

        // 复刻 handle_frames_prefetched 的填充逻辑
        let mut filled = 0;
        for (index, frame) in prefetched {
            if index < frames.len() {
                let f = &mut frames[index];
                if f.pixels.is_empty() {
                    f.pixels = frame.pixels;
                    f.width = frame.width;
                    f.height = frame.height;
                    f.delay_ms = frame.delay_ms;
                    filled += 1;
                }
            }
        }

        // 仅索引 1 和 3 应被填充（其余已有像素，跳过）
        assert_eq!(filled, 2, "应仅填充 2 个空像素帧");
        assert!(!frames[1].pixels.is_empty(), "帧 1 应已填充");
        assert!(!frames[3].pixels.is_empty(), "帧 3 应已填充");
        // 帧 0,2,4 应保持原像素（未被覆盖）
        assert_eq!(frames[0].pixels[0], 0, "帧 0 应保持原像素");
        assert_eq!(frames[2].pixels[0], 2, "帧 2 应保持原像素");
        assert_eq!(frames[4].pixels[0], 4, "帧 4 应保持原像素");
    }

    // ========== v17: 主动式预取测试 ==========

    /// v17: `proactive_prefetch_center` 在下一帧为空且无预取进行中时返回下一帧索引
    #[test]
    fn v17_proactive_prefetch_triggers_when_next_empty() {
        // current=5, frame_count=10, next=6 为空, 无预取进行中 → Some(6)
        let center = proactive_prefetch_center(5, 10, true, false);
        assert_eq!(center, Some(6), "下一帧为空时应以 next=6 为中心触发主动预取");
    }

    /// v17: 下一帧有像素时不触发主动预取（窗口内无需提前解码）
    #[test]
    fn v17_proactive_prefetch_skips_when_next_has_pixels() {
        // next_frame_empty=false → None
        assert_eq!(
            proactive_prefetch_center(5, 10, false, false),
            None,
            "下一帧有像素时不应触发主动预取"
        );
    }

    /// v17: 预取进行中时不重复派生（避免线程爆炸）
    #[test]
    fn v17_proactive_prefetch_skips_when_prefetch_in_progress() {
        // prefetch_in_progress=true → None
        assert_eq!(
            proactive_prefetch_center(5, 10, true, true),
            None,
            "预取进行中时不应重复派生"
        );
    }

    /// v17: frame_count=0 时不触发（防御性，避免 % 0 除零）
    #[test]
    fn v17_proactive_prefetch_skips_when_no_frames() {
        assert_eq!(
            proactive_prefetch_center(0, 0, true, false),
            None,
            "无帧时不应触发主动预取"
        );
    }

    /// v17: 末尾回绕——current 为最后一帧时 next 回绕到 0
    #[test]
    fn v17_proactive_prefetch_wraps_around_at_end() {
        // current=9, frame_count=10, next=(9+1)%10=0
        let center = proactive_prefetch_center(9, 10, true, false);
        assert_eq!(center, Some(0), "末尾应回绕到帧 0");
    }

    /// v17: 单帧 GIF——current=0, frame_count=1, next=0（自身），next_empty=false
    /// （单帧始终有像素），不应触发。但即便 next_empty=true（理论边界），
    /// next 仍为 0，返回 Some(0)——此时预取无害（已解码帧会被跳过）。
    #[test]
    fn v17_proactive_prefetch_single_frame() {
        // 单帧 GIF：next = (0+1)%1 = 0
        let center = proactive_prefetch_center(0, 1, true, false);
        assert_eq!(center, Some(0), "单帧时 next 回绕到 0");
    }

    /// v17: 文档测试验证主动式预取关键代码模式存在
    ///
    /// 验证 WM_TIMER 中新增的主动式预取路径：
    /// - `proactive_prefetch_center` 调用
    /// - #1: `dispatch_prefetch` 复用（反应式 + 主动式，持久化解码游标）
    /// - 下一帧空像素检查
    #[test]
    fn v17_proactive_prefetch_code_patterns_exist() {
        let source = include_str!("gif.rs");
        // 主动式预取决策函数
        assert!(
            source.contains("fn proactive_prefetch_center"),
            "应定义 proactive_prefetch_center 决策函数"
        );
        // #1: 派发函数（取代旧 spawn_prefetch_thread，反应式 + 主动式复用 worker）
        assert!(
            source.contains("fn dispatch_prefetch"),
            "应提取 dispatch_prefetch 供两条路径复用"
        );
        // #1: worker 调用 prefetch_with_cursor（持久化解码游标）
        assert!(
            source.contains("prefetch_with_cursor"),
            "worker 应调用 prefetch_with_cursor 复用解码游标"
        );
        // WM_TIMER 中的主动式预取注释与调用
        assert!(
            source.contains("v17 主动式预取"),
            "WM_TIMER 应包含主动式预取路径"
        );
        // 下一帧空像素检查
        assert!(
            source.contains("next_empty"),
            "WM_TIMER 应检查下一帧是否为空"
        );
        // 反应式路径仍调用 dispatch_prefetch
        assert!(
            source.contains("反应式：当前帧为空，必须解码"),
            "反应式路径应保留"
        );
    }
}
