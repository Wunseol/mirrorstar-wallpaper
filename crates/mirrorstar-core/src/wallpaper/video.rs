use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;

use crate::audio::volume::VolumeControl;
use crate::ipc::mpv_protocol::MpvIpcClient;
use crate::wallpaper::subprocess_base::{
    SubprocessRendererBase, MPV_CONNECT_INTERVAL_MS, MPV_CONNECT_RETRIES,
};
use crate::wallpaper::{
    create_pause_channel, spawn_proc_exit_monitor, validate_renderer_speed, OwnedProcHandle,
    PauseCommand, ScalingMode, WallpaperRenderer, WallpaperState,
};

/// 判断是否应调用 WASAPI 音量操作（W11 COM 降级纯辅助函数）
///
/// 将 pause 线程中"COM 未初始化时跳过 WASAPI 调用"的判断逻辑从线程闭包中抽离，
/// 便于单元测试覆盖。WASAPI 调用需同时满足三个条件：
/// 1. `com_initialized`：pause 线程已成功初始化 COM（CoInitializeEx 返回 S_OK/S_FALSE）
/// 2. `pid` 为 `Some`：视频进程已启动且获取到 PID
/// 3. `has_volume_control`：渲染器持有 VolumeControl（非降级实例）
///
/// 任一条件不满足时跳过 WASAPI 调用，避免无效调用与误导性错误日志。
fn should_invoke_wasapi(com_initialized: bool, pid: Option<u32>, has_volume_control: bool) -> bool {
    com_initialized && pid.is_some() && has_volume_control
}

/// 视频音频状态，合并为单一 Mutex 以避免嵌套锁
struct VideoAudioState {
    /// 静音前的音量（用于恢复）；Some 表示当前已静音
    pre_mute_volume: Option<f32>,
    /// 当前音量
    current_volume: f32,
    /// 视频进程 PID
    pid: Option<u32>,
}

/// 视频壁纸渲染器，使用 mpv 播放器作为子进程
pub struct VideoRenderer {
    /// 公共子进程基类（ProcessManager + 状态 + 窗口句柄 + 缩放模式 + 管道名 + pause_sender）
    base: SubprocessRendererBase,
    /// 视频文件路径
    file_path: String,
    /// mpv IPC 客户端（共享给 pause 线程）
    ipc: Arc<Mutex<Option<MpvIpcClient>>>,
    /// 当前音量 (0.0~1.0)
    volume: f32,
    /// 是否静音
    muted: bool,
    /// 音量控制器（缓存 COM 接口），测试中可为 None
    volume_control: Option<Arc<Mutex<VolumeControl>>>,
}

// SAFETY: VideoRenderer 的 HWND 仅作为值存储，用于 Win32 API 调用。
// HWND 本身是线程安全的句柄值，Win32 窗口操作函数是线程安全的。
// MpvIpcClient 使用标准文件 I/O，ProcessManager 的 HANDLE 也是线程安全的。
// VolumeControl 内部的 COM 接口在 MTA 中可跨线程访问。
unsafe impl Send for VideoRenderer {}

impl VideoRenderer {
    pub fn new(
        file_path: String,
        scaling_mode: ScalingMode,
        volume_control: Option<Arc<Mutex<VolumeControl>>>,
    ) -> Self {
        let pipe_name = format!("mirrorstar-mpv-{}", uuid::Uuid::new_v4());
        let mpv_path = Self::find_mpv();

        Self {
            base: SubprocessRendererBase::new(mpv_path, pipe_name, scaling_mode),
            file_path,
            ipc: Arc::new(Mutex::new(None)),
            volume: 1.0,
            muted: false,
            volume_control,
        }
    }

    /// 查找 mpv 可执行文件路径
    fn find_mpv() -> std::path::PathBuf {
        SubprocessRendererBase::find_bundled_executable(Some("mpv"), "mpv.exe", "mpv")
    }

    /// 构建 mpv 日志文件路径（v5.2 诊断）
    ///
    /// 将 mpv 日志写入与应用日志相同的目录（数据根/logs/），
    /// 文件名格式 `mpv-<pipe_suffix>.log`，其中 pipe_suffix 从管道名提取
    ///（如 `mirrorstar-mpv-<uuid>` → `<uuid>`），确保多次启动的日志不互相覆盖。
    ///
    /// 数据根解析失败时回退为当前目录，mpv 跳过日志文件创建，不影响正常启动。
    fn build_mpv_log_path(pipe_name: &str) -> Option<std::path::PathBuf> {
        // 便携化：mpv 日志与应用日志同目录（数据根/logs）
        let log_dir = crate::config::data_root().join("logs");
        // 确保日志目录存在（忽略创建失败，mpv 会跳过日志写入）
        let _ = std::fs::create_dir_all(&log_dir);
        // 从 "mirrorstar-mpv-<uuid>" 提取 "<uuid>" 作为文件名后缀
        let suffix = pipe_name.rsplit('-').next().unwrap_or("unknown");
        Some(log_dir.join(format!("mpv-{}.log", suffix)))
    }

    /// 构建 mpv 启动参数
    pub(crate) fn build_mpv_args(&self) -> Vec<String> {
        let ipc_path = format!(r"\\.\pipe\{}", self.base.pipe_name);

        // 根因 E：mpv 以 --idle=yes 启动（不加载任何文件），窗口嵌入 WorkerW
        // 壁纸层后再通过 IPC `loadfile` 加载视频文件。
        //
        // 原实现把视频文件作为命令行参数传给 mpv，mpv 启动即开始解码并创建 4K
        // 视频纹理（约 0.28s）；与此同时主程序调用 SetParent + SetWindowPos 将
        // mpv 窗口嵌入 WorkerW 壁纸层。窗口在重父化过程中被缩放触发 mpv D3D11
        // 视频输出重配置，此时创建纹理返回 E_OUTOFMEMORY(0x8007000e)（并非真实
        // 内存不足，而是 swapchain/窗口重父化竞争导致的设备状态异常）→ 桌面黑屏。
        // 该缺陷与 flip/bitblt 呈现模式无关（已实测两者均复现），根因是纹理创建
        // 与嵌入窗口 resize 的时序竞争。
        //
        // 修复方案（mpv-idle-load-test.ps1 已验证）：先 `--idle=yes` 启动空窗口
        // （无视频、无纹理创建），嵌入完成后通过 IPC loadfile 加载视频。此时纹理
        // 在窗口已稳定嵌入后才创建，不再有重父化竞争 → 正常渲染、无黑屏。
        //
        // v10-A 内存优化：禁用 mpv 缓存并限制解码队列上限。
        //
        // mpv 默认启用 demuxer 缓存（约 75MB），用于支持 seek 操作时的回放缓冲。
        // 但壁纸循环播放（--loop-file）场景下该缓存纯属浪费：
        // - 无需 seek：视频从头到尾循环，用户不会主动跳转
        // - 无需网络缓存：壁纸均为本地文件，不存在网络抖动
        // - 解码队列默认无上限：高码率/长视频场景下可能积累大量解码帧占用内存
        //
        // 通过以下 5 个参数限制内存占用（节省约 75MB demuxer 缓存 + 解码队列上限）：
        // - --cache=no：禁用 demuxer 缓存（壁纸循环播放，无需 seek 缓存，节省 ~75MB）
        // - --vd-queue-max-bytes=16777216：视频解码队列上限 16MB（默认无限制）
        // - --ad-queue-max-bytes=4194304：音频解码队列上限 4MB（默认无限制）
        // - --demuxer-max-back-bytes=0：禁用后退缓冲（默认 30MB）
        // - --cache-secs=0：禁用时间维度缓存（默认 1s）
        //
        // 注意：勿添加 --demuxer-max-bytes=0！该参数将解复用器缓冲上限清零，
        // 导致 demuxer 无法缓冲任何数据包（日志："Too many packets in the demuxer
        // packet queues: 0 packets"），mpv 立即 video EOF + "No media data to loop"
        // 退出（0.2s），已嵌入壁纸层的空窗口被销毁 → 桌面黑屏 + 设置失败。
        let mut args = vec![
            "--idle=yes".to_string(),
            "--no-input-default-bindings".to_string(),
            "--loop-file".to_string(),
            "--hwdec=auto".to_string(),
            "--vo=gpu".to_string(),
            "--input-vo-keyboard=no".to_string(),
            "--no-osc".to_string(),
            "--no-osd-bar".to_string(),
            "--title=MirrorStarVideo".to_string(),
            "--input-media-keys=no".to_string(),
            "--no-input-terminal".to_string(),
            "--no-terminal".to_string(),
            "--keep-open=no".to_string(),
            "--force-window=yes".to_string(),
            format!("--input-ipc-server={}", ipc_path),
            // v10-A：缓存与解码队列限制（详见函数开头注释）
            "--cache=no".to_string(),
            "--vd-queue-max-bytes=16777216".to_string(),
            "--ad-queue-max-bytes=4194304".to_string(),
            // v11.0：demuxer 后退缓冲清零 + 禁用时间维度缓存（见注释，勿加 --demuxer-max-bytes=0）
            "--demuxer-max-back-bytes=0".to_string(),
            "--cache-secs=0".to_string(),
            // 根因 D：--d3d11-flip=no 强制 bitblt-model presentation。
            // mpv 默认使用 flip-model presentation，窗口被 SetParent 嵌入 WorkerW 壁纸层后，
            // flip-model swapchain 处于非法状态，嵌入后 SetWindowPos 触发重配置时 D3D11
            // 创建 4K 视频纹理失败（E_OUTOFMEMORY 0x8007000e + shaderc internal error），
            // 视频无法渲染 → 桌面黑屏（mpv 进程仍存活，UI 显示"已设置"但无动态壁纸）。
            // bitblt 呈现模式与 SetParent 重新父化兼容，嵌入后仍能正常渲染。勿删此参数！
            // （注意：根因 E 的 idle-load 修复后此参数非必需，但保留可进一步降低
            //   嵌入后 swapchain 重配置的失败概率，作为双保险。）
            "--d3d11-flip=no".to_string(),
        ];

        // v5.2 诊断：添加 mpv 日志文件，用于诊断"mpv 嵌入后崩溃"问题。
        // 日志写入数据根/logs/mpv-<pipe_suffix>.log
        // 包含 mpv 内部初始化、解码、渲染、错误等信息，崩溃时可用于定位根因。
        if let Some(log_path) = Self::build_mpv_log_path(&self.base.pipe_name) {
            tracing::info!(mpv_log = %log_path.display(), "mpv 日志文件路径");
            args.push(format!("--log-file={}", log_path.display()));
        }

        // 根据缩放模式设置视频缩放参数
        match self.base.scaling_mode {
            ScalingMode::Fill => {
                args.push("--video-unscaled=no".to_string());
            }
            ScalingMode::Fit => {
                args.push("--video-unscaled=no".to_string());
                args.push("--panscan=0.0".to_string());
            }
            ScalingMode::Stretch => {
                args.push("--video-unscaled=no".to_string());
                args.push("--keepaspect=no".to_string());
            }
            ScalingMode::Center | ScalingMode::Original => {
                args.push("--video-unscaled=yes".to_string());
            }
        }

        // 根因 E：不再把视频文件作为命令行参数传给 mpv（mpv 以 --idle=yes 启动）。
        // 视频文件改为在窗口嵌入 WorkerW 后通过 IPC `loadfile` 加载（见 after_embed）。
        // 原 v41-W-014 的 `--` 分隔符参数注入防护随之下移到 IPC 层：loadfile 的 path
        // 经 serde_json 序列化转义，任意路径（含 `--` 开头/中文/空格）均安全。

        args
    }
    /// 诊断：查询 mpv 播放状态（供全链路运行时诊断测试使用）
    ///
    /// 通过 IPC 查询多个属性（`idle-active` / `time-pos` / `eof-reached` /
    /// `pause` / `width` / `height` / `video-format`），以 JSON 对象返回。
    /// 任一属性查询失败时在该字段记录错误信息而不中断整体查询。
    ///
    /// 诊断判定：
    /// - `idle-active = "no"` 且 `time-pos` 持续增长 → 视频已 loadfile 且正在播放
    /// - `width`/`height` > 0 → 视频纹理已成功创建（无黑屏）
    pub fn diagnostic_playback_status(
        &mut self,
    ) -> Result<serde_json::Value, crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ref mut ipc) = *ipc else {
            return Err(crate::MirrorStarError::IpcError(
                "mpv IPC 未连接，无法查询播放状态".to_string(),
            ));
        };
        let mut map = serde_json::Map::new();
        for prop in [
            "idle-active",
            "time-pos",
            "eof-reached",
            "pause",
            "width",
            "height",
            "video-format",
        ] {
            match ipc.get_property(prop) {
                Ok(v) => {
                    map.insert(prop.to_string(), v);
                }
                Err(e) => {
                    map.insert(
                        prop.to_string(),
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        }
        Ok(serde_json::Value::Object(map))
    }
}

impl WallpaperRenderer for VideoRenderer {
    fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        // 1. 构建并启动 mpv 进程
        let args = self.build_mpv_args();
        tracing::info!(args = ?args, "mpv 启动参数");
        let pid = self.base.start_process(args)?;

        tracing::info!(pid, "mpv 进程已启动");

        // 2. 连接 IPC（重试 40 次，每次间隔 50ms = 2s 最大等待，Task 7.4 冷启动优化）
        // Task 7.4：从 120 次（6s）缩减到 40 次（2s）。真全屏退出恢复走后台线程
        // 异步执行，2s 连接窗口不再阻塞 Win32 回调线程；极端冷启动（GPU 驱动初始化
        // + DLL 加载 + 杀毒扫描，3-5s）下 2s 内未连接成功即失败，由周期复查线程重试
        // （重试通常命中 warm cache ~600ms 即成功）。
        let mut ipc = MpvIpcClient::new(self.base.pipe_name());
        if let Err(e) = ipc.connect(MPV_CONNECT_RETRIES, MPV_CONNECT_INTERVAL_MS) {
            // 诊断：IPC 连接失败时，检查 mpv 进程是否仍在运行 + 管道是否存在
            let is_running = self.base.process.is_running();
            let pipe_path = format!(r"\\.\pipe\{}", self.base.pipe_name());
            let pipe_exists = std::fs::metadata(&pipe_path).is_ok();
            tracing::error!(
                error = %e,
                pid,
                pipe_path = %pipe_path,
                file_path = %self.file_path,
                mpv_running = is_running,
                pipe_exists = pipe_exists,
                "mpv IPC 连接失败诊断：mpv 进程状态 + 管道存在性 + 文件路径"
            );
            // Task 7.3：连接失败时清理已 spawn 的 mpv 进程，避免孤儿进程残留。
            // terminate() 内部 ipc.quit → stop_process（等待退出，超时强杀）→
            // disconnect → state=Terminated。清理后渲染器保持 Terminated 状态，
            // 调用方（resume_all_fast）计入 failed，由周期复查线程后续重试 play()。
            if let Err(cleanup_err) = self.terminate() {
                tracing::warn!(error = %cleanup_err, "IPC 连接失败后清理 mpv 进程也失败");
            }
            return Err(e);
        }
        *self.ipc.lock().unwrap_or_else(|e| e.into_inner()) = Some(ipc);

        tracing::info!("mpv IPC 已连接");

        // 3. 查找 mpv 窗口（最大 2s，从 5s 缩减）
        // Task 8.4：改用窗口类名 "mpv" 查找（mpv 编译时固定 MPV_WINDOW_CLASS_NAME = L"mpv"），
        // 比基于标题 "MirrorStarVideo" 的查找更稳定（标题可被外部进程修改或冲突）。
        let hwnd = match SubprocessRendererBase::find_window_by_class(pid, "mpv") {
            Some(hwnd) => hwnd,
            None => {
                tracing::error!(pid, "未能在超时内找到 mpv 窗口");
                // Task 7.3：窗口查找失败时清理已 spawn 的 mpv 进程，避免孤儿进程残留
                // （mpv 窗口未创建可能意味着渲染初始化失败，进程无意义驻留）。
                if let Err(cleanup_err) = self.terminate() {
                    tracing::warn!(error = %cleanup_err, "窗口查找失败后清理 mpv 进程也失败");
                }
                return Err(crate::MirrorStarError::DesktopIntegration(format!(
                    "未能在超时内找到 mpv 窗口 (pid={})",
                    pid
                )));
            }
        };
        self.base.set_hwnd(Some(hwnd));

        tracing::info!(hwnd = ?hwnd, "已找到 mpv 窗口");

        // 4. 通过 WASAPI 应用当前音量和静音设置（异步执行，不阻塞嵌入流程）
        //
        // v5.1 修复：原实现同步调用 set_process_volume，当 mpv 进程刚启动尚无音频会话时，
        // WASAPI 会重试枚举会话（典型 5+ 秒），期间 mpv 窗口（--force-window=yes 创建）
        // 作为独立窗口显示，用户看到"直接播放视频"而非壁纸。
        //
        // 改为后台线程异步执行：play() 立即返回，embed_wallpaper 立即嵌入窗口，
        // WASAPI 在后台线程初始化 COM（MTA）后执行音量设置。失败仅 warn 不影响主流程。
        if let (Some(pid), Some(vc)) = (self.base.process_pid(), self.volume_control.as_ref()) {
            let vc = vc.clone();
            let volume = self.volume;
            let muted = self.muted;
            let pid_for_audio = pid;
            if let Err(e) = std::thread::Builder::new()
                .name("mirrorstar-video-audio".to_string())
                .spawn(move || {
                    // 初始化 COM（MTA 模式），WASAPI 调用需要 COM 环境
                    let co_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                    let need_couninit = co_result.is_ok();
                    if !need_couninit {
                        tracing::warn!(
                            error = ?co_result,
                            "audio 后台线程 COM 初始化失败，跳过 WASAPI 调用"
                        );
                        return;
                    }

                    let vc = vc.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = vc.set_process_volume(pid_for_audio, volume) {
                        tracing::warn!(error = ?e, "set_process_volume 失败（后台线程）");
                    }
                    if muted {
                        if let Err(e) = vc.set_process_mute(pid_for_audio, true) {
                            tracing::warn!(error = ?e, "set_process_mute 失败（后台线程）");
                        }
                    }

                    unsafe { CoUninitialize() };
                })
            {
                tracing::warn!(error = %e, "启动 audio 后台线程失败，跳过 WASAPI 音量设置");
            }
        }

        self.base.set_state(WallpaperState::Playing);
        tracing::info!("视频壁纸开始播放");
        Ok(())
    }

    fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            ipc.pause()?;
        } else {
            tracing::warn!("IPC 未连接，暂停命令未实际下发");
        }
        self.base.set_state(WallpaperState::Paused);
        tracing::info!("视频壁纸已暂停");
        Ok(())
    }

    fn pause_for_fullscreen(&mut self) -> Result<(), crate::MirrorStarError> {
        // 设计简化：认清全屏恢复路径一律"从头播放"（见 after_embed），不保存/不续播
        // 播放进度。此处直接终止 mpv（terminate 内部：ipc.quit → stop_process →
        // disconnect → state=Terminated），省去 time-pos 查询与 last_position 记录，
        // 从根上消除续播相关（含 loadfile start 参数解析）的潜在故障面。
        self.terminate()
    }

    fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            ipc.resume()?;
        } else {
            tracing::warn!("IPC 未连接，恢复命令未实际下发");
        }
        self.base.set_state(WallpaperState::Playing);
        tracing::info!("视频壁纸已恢复");
        Ok(())
    }

    fn set_position(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), crate::MirrorStarError> {
        if let Some(hwnd) = self.base.hwnd() {
            unsafe {
                SetWindowPos(hwnd, None, x, y, w, h, Default::default())?;
            }
        } else {
            tracing::warn!("窗口句柄未就绪，设置位置命令未实际下发");
        }
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        // 1. 通过 IPC 请求 mpv 退出
        {
            let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut ipc) = *ipc {
                if let Err(e) = ipc.quit() {
                    tracing::warn!(error = %e, "IPC quit 失败，将等待进程超时强杀");
                }
            }
        }

        // 2. 停止进程（会等待退出，超时后强制终止）
        self.base.stop_process()?;

        // 3. 断开 IPC
        {
            let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut ipc) = *ipc {
                ipc.disconnect();
            }
            *ipc = None;
        }
        self.base.set_hwnd(None);
        self.base.set_state(WallpaperState::Terminated);

        tracing::info!("视频壁纸已终止");
        Ok(())
    }

    fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    fn state(&self) -> WallpaperState {
        self.base.state()
    }

    fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.base.set_scaling_mode(mode);
        // mpv 不支持运行时动态更改视频缩放模式
        // 如需更改，需要重启 mpv 进程
        tracing::warn!("视频壁纸缩放模式已记录，需重启 mpv 生效");
    }

    fn set_speed(&mut self, speed: f32) {
        // W-007: 校验 speed 必须为正有限数，与 GifRenderer::set_speed（W10）保持一致。
        // speed <= 0 或 NaN/Infinity 会导致 mpv 播放异常（如定时器 0/inf/NaN）。
        // 校验失败时跳过 IPC 发送，与 GIF 行为一致（不破坏调用方契约，仅 warn 不传播错误）。
        if !validate_renderer_speed(speed) {
            tracing::warn!(speed, "无效的视频播放速度，已忽略（speed 必须 > 0 且有限）");
            return;
        }
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            if let Err(e) = ipc.set_speed(speed) {
                tracing::error!(error = %e, "设置播放速度失败");
            }
        }
    }

    fn set_mouse_passthrough(&mut self, enabled: bool) {
        if let Some(hwnd) = self.hwnd() {
            crate::desktop::window::set_mouse_passthrough(hwnd, enabled);
        }
    }

    fn set_interaction_mode(&mut self, enabled: bool) {
        self.set_mouse_passthrough(!enabled);
    }

    /// 嵌入 WorkerW 壁纸层后通过 IPC 加载视频文件（根因 E 修复）
    ///
    /// mpv 以 `--idle=yes` 启动（空窗口、不加载文件），本方法在窗口被
    /// `SetParent` 嵌入 WorkerW 壁纸层后才发送 `loadfile` 命令。这样视频纹理
    /// 在窗口已稳定嵌入后才创建，避免嵌入前创建纹理时窗口重父化 + 缩放导致
    /// D3D11 纹理创建失败（`E_OUTOFMEMORY 0x8007000e`）→ 桌面黑屏。
    ///
    /// 设计简化：全屏终止后的恢复路径不论是否保存过播放进度，
    /// 一律从视频开头播放（不做"从保存位置续播"）。这样：
    /// - 恢复逻辑只剩一条固定路径 `loadfile(path)`，无分支、更简单
    /// - 删除 loadfile_from/start 参数，从根上消除其"start 必须整数"等解析故障面
    /// - 循环壁纸从头播放对观感几乎无影响
    ///
    /// 实现细节：
    /// - 使用 IPC `loadfile <path> replace`（fire-and-forget）
    /// - `--loop-file` 为全局选项，对 loadfile 加载的文件同样生效
    /// - path 经 serde_json 序列化转义，含中文/空格/反斜杠路径安全
    fn after_embed(&mut self) -> Result<(), crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        match ipc.as_mut() {
            Some(ipc_client) => {
                ipc_client.loadfile(&self.file_path)?;
                tracing::info!(
                    file_path = %self.file_path,
                    "窗口已嵌入 WorkerW，通过 IPC loadfile 从头加载视频"
                );
                Ok(())
            }
            None => {
                tracing::error!("IPC 未连接，无法通过 loadfile 加载视频");
                Err(crate::MirrorStarError::IpcError(
                    "mpv IPC 未连接，loadfile 失败".to_string(),
                ))
            }
        }
    }

    fn create_pause_sender(&mut self, display_id: &str) -> Option<crate::wallpaper::PauseSender> {
        let (sender, mut rx, shared_state) = create_pause_channel();

        // 初始化共享状态
        {
            let mut s = shared_state.write().unwrap_or_else(|e| e.into_inner());
            s.state = self.base.state();
            s.volume = self.volume;
        }

        let ipc = self.ipc.clone();
        let volume_control = self.volume_control.clone();
        let audio_state = Arc::new(Mutex::new(VideoAudioState {
            pre_mute_volume: None,
            current_volume: self.volume,
            pid: self.base.process_pid(),
        }));
        // clone sender 与 display_id move 进 pause 线程闭包，
        // 状态变更后通过 notify_state_changed 通知 Tauri 层 emit 事件
        let state_sender = sender.clone();
        let display_id = display_id.to_string();
        // clone 用于 mpv 进程退出监听线程（原值已被 pause 线程闭包 move）
        let monitor_display_id = display_id.clone();
        let monitor_shared = shared_state.clone();
        let monitor_sender = sender.clone();

        if let Err(e) = std::thread::Builder::new()
            .name("mirrorstar-video-pause".to_string())
            .spawn(move || {
                // 初始化 COM（MTA 模式），确保 pause 线程能正确访问 VolumeControl 的 WASAPI 接口。
                // VolumeControl 在主线程（STA）创建并经 Arc 共享到本线程，其 COM 接口需在
                // 已初始化 COM 的线程上调用。CoInitializeEx 返回 S_OK/S_FALSE 时需配对
                // CoUninitialize；返回 RPC_E_CHANGED_MODE（线程已以其他模式初始化）时不应调用。
                let co_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let need_couninit = co_result.is_ok();
                // W11: 记录 COM 初始化状态，音量操作前检查该标志。
                // COM 初始化失败（如 RPC_E_CHANGED_MODE）时 WASAPI 调用会失败或行为异常，
                // 跳过可避免无效调用与误导性错误日志。
                let com_initialized = need_couninit;
                if !com_initialized {
                    tracing::warn!(error = ?co_result, "pause 线程 COM 初始化失败，音量控制将跳过 WASAPI 调用");
                }

                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                        PauseCommand::Pause => {
                            let mut ipc_guard = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc_client) = *ipc_guard {
                                if let Err(e) = ipc_client.pause() {
                                    tracing::error!(error = %e, "视频暂停失败");
                                }
                            }
                            drop(ipc_guard);
                            shared_state.write().unwrap_or_else(|e| e.into_inner()).state = WallpaperState::Paused;
                            // 通知 Tauri 层 emit wallpaper-state-changed 事件
                            state_sender.notify_state_changed(&display_id);
                        }
                        PauseCommand::Resume => {
                            let mut ipc_guard = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc_client) = *ipc_guard {
                                if let Err(e) = ipc_client.resume() {
                                    tracing::error!(error = %e, "视频恢复失败");
                                }
                            }
                            drop(ipc_guard);
                            shared_state.write().unwrap_or_else(|e| e.into_inner()).state = WallpaperState::Playing;
                            // 通知 Tauri 层 emit wallpaper-state-changed 事件
                            state_sender.notify_state_changed(&display_id);
                        }
                        PauseCommand::SetVolume(volume) => {
                            // 在锁内更新状态，锁外调用 VolumeControl 以避免持锁过久
                            let pid = {
                                let mut state = audio_state.lock().unwrap_or_else(|e| e.into_inner());
                                state.current_volume = volume;
                                if volume > 0.0 {
                                    state.pre_mute_volume = None;
                                }
                                state.pid
                            };
                            shared_state.write().unwrap_or_else(|e| e.into_inner()).volume = volume;
                            // W11: COM 未初始化时跳过 WASAPI 调用，避免无效调用
                            if should_invoke_wasapi(com_initialized, pid, volume_control.is_some()) {
                                if let (Some(p), Some(vc)) = (pid, volume_control.as_ref()) {
                                    let vc = vc.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Err(e) = vc.set_process_volume(p, volume) {
                                        tracing::warn!(error = ?e, "SetVolume set_process_volume 失败");
                                    }
                                }
                            } else if !com_initialized {
                                tracing::debug!("COM 未初始化，跳过 SetVolume WASAPI 调用");
                            }
                        }
                        PauseCommand::ToggleMute => {
                            // 在锁内计算目标音量与 PID，锁外调用 VolumeControl
                            let (pid, target_volume, maybe_shared_volume) = {
                                let mut state = audio_state.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(prev) = state.pre_mute_volume.take() {
                                    // 当前已静音，恢复音量
                                    state.current_volume = prev;
                                    (state.pid, prev, Some(prev))
                                } else {
                                    // 当前未静音，静音
                                    let vol = state.current_volume;
                                    state.pre_mute_volume = Some(vol);
                                    // current_volume 保持不变，保留意图音量以便恢复
                                    (state.pid, 0.0, None)
                                }
                            };
                            // 仅恢复时更新共享状态音量（与原逻辑一致）
                            if let Some(v) = maybe_shared_volume {
                                shared_state.write().unwrap_or_else(|e| e.into_inner()).volume = v;
                            }
                            // W11: COM 未初始化时跳过 WASAPI 调用，避免无效调用
                            if should_invoke_wasapi(com_initialized, pid, volume_control.is_some()) {
                                if let (Some(p), Some(vc)) = (pid, volume_control.as_ref()) {
                                    let vc = vc.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Err(e) = vc.set_process_volume(p, target_volume) {
                                        tracing::warn!(error = ?e, "ToggleMute set_process_volume 失败");
                                    }
                                }
                            } else if !com_initialized {
                                tracing::debug!("COM 未初始化，跳过 ToggleMute WASAPI 调用");
                            }
                        }
                    }
                }
                if need_couninit {
                    unsafe { CoUninitialize() };
                }
                tracing::debug!("VideoRenderer pause 线程退出");
            })
        {
            tracing::error!(error = %e, "创建 VideoRenderer pause 线程失败");
            return None;
        }

        // 修复：监听 mpv 子进程退出事件
        //
        // 原实现启动 mpv 子进程后不监听其退出，子进程崩溃/异常退出时
        // engine 状态仍为 Playing，前端 UI 不刷新，用户感知不到壁纸已停止。
        //
        // 修复：spawn 监听线程等待 mpv 进程退出（WaitForSingleObject(INFINITE)），
        // 异常退出时（state != Terminated）通过 PauseSender::notify_state_changed
        // 通知 engine 更新状态，与 web.rs 的 wp-proc 退出监听机制统一。
        //
        // 使用 DuplicateHandle 复制句柄，避免监听线程的 wait 与 terminate() 中的
        // CloseHandle 产生竞争（Win32 文档明确指出在 wait 期间关闭同一句柄会导致
        // 未定义行为）。监听线程拥有独立句柄，退出时自行 CloseHandle。
        //
        // 正常退出路径：terminate() 先通过 IPC 请求 mpv 退出，再调用 stop_process()
        // 等待退出。此时监听线程的 wait 也会返回，但 state 已被 terminate() 流程
        // 标记为 Terminated（或即将标记），监听线程检查到 state == Terminated 后
        // 不发通知，避免对正常退出产生冗余通知。
        //
        // [Consistency]-12.2 修复：调用共享 `spawn_proc_exit_monitor` 收敛 video.rs
        // 与 web.rs 的实现差异。`OwnedProcHandle` RAII 管理句柄生命周期，
        // spawn 失败时由共享函数统一 `warn` 并通过 `OwnedProcHandle::drop` 关闭句柄。
        if let Some(proc_handle) = self.base.duplicate_process_handle() {
            // 使用 OwnedProcHandle RAII 包装器：spawn 失败时闭包被 drop，包装器自动
            // 调用 CloseHandle，不泄漏句柄；pause 通道已建立成功，仅 warn 不返回 None。
            let owned = OwnedProcHandle::new(proc_handle);
            spawn_proc_exit_monitor(owned, move || {
                // 检查是否为异常退出：state != Terminated 表示非 terminate() 触发
                let state = monitor_shared
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .state;
                if state != WallpaperState::Terminated {
                    tracing::warn!(
                        display_id = %monitor_display_id,
                        state = ?state,
                        "mpv 子进程异常退出，通知 engine 更新状态"
                    );
                    // 更新共享状态为 Terminated，使 any_playing 等查询返回正确结果
                    monitor_shared
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .state = WallpaperState::Terminated;
                    // 通知 Tauri 层 emit wallpaper-state-changed 事件刷新 UI
                    monitor_sender.notify_state_changed(&monitor_display_id);
                }
                tracing::debug!("VideoRenderer 进程监听线程退出");
            });
        }

        self.base.set_pause_sender(Some(sender.clone()));
        Some(sender)
    }
}

impl Drop for VideoRenderer {
    fn drop(&mut self) {
        if self.base.state() != WallpaperState::Terminated {
            // v41-W-007 修复：显式断开 IPC socket 句柄，再调用 stop_immediate 终止进程。
            //
            // 原实现直接调用 terminate()，terminate 内部顺序为
            // `ipc.quit() → stop_process() → ipc.disconnect()`，存在两个问题：
            //
            // 1. 资源泄漏风险：若 ipc.quit() 或 stop_process() 失败/超时，
            //    disconnect() 可能不被执行，导致 mpv IPC 命名管道句柄泄漏。
            // 2. Drop 阻塞：stop_process() 等待进程退出（超时后强杀），
            //    Drop 期间最长阻塞数秒，拖慢应用退出。
            //
            // 修复：在 Drop 中绕过 terminate()，直接执行资源清理：
            // 1. 显式 disconnect IPC socket（关闭命名管道句柄，幂等安全）
            // 2. 调用 process_manager.stop_immediate()（TerminateProcess + CloseHandle，
            //    毫秒级完成，不等待进程退出）
            // 3. 清理 hwnd 与状态
            //
            // 不调用 ipc.quit()：Drop 路径优先保证资源不泄漏与快速退出，
            // 不依赖 IPC 优雅退出（mpv 会被 TerminateProcess 强杀）。
            // disconnect() 是幂等的，重复调用无副作用。
            {
                let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref mut ipc) = *ipc {
                    ipc.disconnect();
                }
                *ipc = None;
            }
            if let Err(e) = self.base.process.stop_immediate() {
                tracing::warn!(error = %e, "VideoRenderer drop 时 stop_immediate 失败");
            }
            self.base.set_hwnd(None);
            self.base.set_state(WallpaperState::Terminated);
        }
        tracing::debug!("VideoRenderer 已清理");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;

    fn create_renderer(mode: ScalingMode) -> VideoRenderer {
        // 测试中不需要 VolumeControl（不会实际调用 COM）
        VideoRenderer::new("test_video.mp4".to_string(), mode, None)
    }

    // ========== Common args tests ==========

    #[test]
    fn build_mpv_args_contains_common_args() {
        let renderer = create_renderer(ScalingMode::Fill);
        let args = renderer.build_mpv_args();

        let common_args = [
            "--idle=yes",
            "--loop-file",
            "--hwdec=auto",
            "--vo=gpu",
            "--no-osc",
            "--no-osd-bar",
            "--title=MirrorStarVideo",
            "--force-window=yes",
        ];

        for expected in &common_args {
            assert!(
                args.iter().any(|a| a == *expected),
                "Common arg '{}' not found in mpv args",
                expected
            );
        }

        // v10-A：缓存与解码队列限制参数
        assert!(args.contains(&"--cache=no".to_string()));
        assert!(args.contains(&"--vd-queue-max-bytes=16777216".to_string()));
        assert!(args.contains(&"--ad-queue-max-bytes=4194304".to_string()));
        // v11.0：demuxer 后退缓冲清零 + 禁用时间维度缓存参数
        assert!(args.contains(&"--demuxer-max-back-bytes=0".to_string()));
        assert!(args.contains(&"--cache-secs=0".to_string()));
        // 根因 D：--d3d11-flip=no 强制 bitblt 呈现模式，避免嵌入 WorkerW 后 flip-model 纹理创建失败黑屏
        assert!(args.contains(&"--d3d11-flip=no".to_string()));
        // 根因 C：--demuxer-max-bytes=0 会把解复用器缓冲清零导致 mpv 无媒体数据可播（黑屏+设置失败），必须禁止出现
        assert!(
            !args.iter().any(|a| a.starts_with("--demuxer-max-bytes=")),
            "--demuxer-max-bytes 参数（尤其 =0）会阻止 demuxer 读取数据，禁止出现"
        );
    }

    #[test]
    fn build_mpv_args_does_not_contain_file_path() {
        // 根因 E：mpv 以 --idle=yes 启动（不加载文件），视频文件改为嵌入 WorkerW
        // 后通过 IPC loadfile 加载。启动参数中不得出现文件路径，否则 mpv 会启动
        // 即解码并在窗口嵌入前创建 4K 纹理 → D3D11 纹理创建失败 → 桌面黑屏。
        let renderer = create_renderer(ScalingMode::Fill);
        let args = renderer.build_mpv_args();

        assert!(
            !args.iter().any(|a| a == "test_video.mp4"),
            "启动参数不得包含视频文件路径（根因 E：mpv 须以 --idle=yes 启动）"
        );
        // v41-W-014 的 `--` 分隔符不再需要（无命令行文件参数），参数注入防护随之下移到 IPC loadfile
        assert!(
            !args.contains(&"--".to_string()),
            "启动参数不再包含 `--` 分隔符（文件路径已不在命令行）"
        );
    }

    // ========== Scaling mode specific tests ==========

    #[test]
    fn build_mpv_args_fill_mode() {
        let renderer = create_renderer(ScalingMode::Fill);
        let args = renderer.build_mpv_args();

        assert!(
            args.iter().any(|a| a == "--video-unscaled=no"),
            "Fill mode should contain --video-unscaled=no"
        );
        assert!(
            !args.iter().any(|a| a == "--keepaspect=no"),
            "Fill mode should NOT contain --keepaspect=no"
        );
        assert!(
            !args.iter().any(|a| a == "--panscan=0.0"),
            "Fill mode should NOT contain --panscan=0.0"
        );
    }

    #[test]
    fn build_mpv_args_fit_mode() {
        let renderer = create_renderer(ScalingMode::Fit);
        let args = renderer.build_mpv_args();

        assert!(
            args.iter().any(|a| a == "--video-unscaled=no"),
            "Fit mode should contain --video-unscaled=no"
        );
        assert!(
            args.iter().any(|a| a == "--panscan=0.0"),
            "Fit mode should contain --panscan=0.0"
        );
    }

    #[test]
    fn build_mpv_args_stretch_mode() {
        let renderer = create_renderer(ScalingMode::Stretch);
        let args = renderer.build_mpv_args();

        assert!(
            args.iter().any(|a| a == "--video-unscaled=no"),
            "Stretch mode should contain --video-unscaled=no"
        );
        assert!(
            args.iter().any(|a| a == "--keepaspect=no"),
            "Stretch mode should contain --keepaspect=no"
        );
    }

    #[test]
    fn build_mpv_args_center_mode() {
        let renderer = create_renderer(ScalingMode::Center);
        let args = renderer.build_mpv_args();

        assert!(
            args.iter().any(|a| a == "--video-unscaled=yes"),
            "Center mode should contain --video-unscaled=yes"
        );
    }

    #[test]
    fn build_mpv_args_original_mode() {
        let renderer = create_renderer(ScalingMode::Original);
        let args = renderer.build_mpv_args();

        assert!(
            args.iter().any(|a| a == "--video-unscaled=yes"),
            "Original mode should contain --video-unscaled=yes"
        );
    }

    // ========== W11 修复测试：COM 降级时跳过 WASAPI 调用 ==========

    #[test]
    fn w11_skip_wasapi_when_com_not_initialized() {
        // W11: COM 未初始化（com_initialized=false）时，即便 pid 与 volume_control 均就绪，
        // 也应跳过 WASAPI 调用，避免在未初始化 COM 的线程上调用 WASAPI 导致失败/UB。
        assert!(
            !should_invoke_wasapi(false, Some(1234), true),
            "COM 未初始化时应跳过 WASAPI 调用"
        );
    }

    #[test]
    fn w11_skip_wasapi_when_pid_missing() {
        // W11: COM 已初始化但 pid 为 None（视频进程未启动或已退出），应跳过 WASAPI 调用。
        assert!(
            !should_invoke_wasapi(true, None, true),
            "pid 为 None 时应跳过 WASAPI 调用"
        );
    }

    #[test]
    fn w11_skip_wasapi_when_no_volume_control() {
        // W11: COM 已初始化、pid 就绪，但渲染器未持有 VolumeControl（降级实例），
        // 应跳过 WASAPI 调用。
        assert!(
            !should_invoke_wasapi(true, Some(1234), false),
            "无 VolumeControl 时应跳过 WASAPI 调用"
        );
    }

    #[test]
    fn w11_invoke_wasapi_when_all_conditions_met() {
        // W11: 三个条件全部满足（COM 已初始化 + pid 就绪 + 有 VolumeControl）时，
        // 应执行 WASAPI 调用。
        assert!(
            should_invoke_wasapi(true, Some(1234), true),
            "三条件全满足时应执行 WASAPI 调用"
        );
    }

    #[test]
    fn w11_skip_wasapi_when_com_and_pid_both_missing() {
        // W11: 多个条件同时不满足（COM 未初始化 + pid 缺失），仍应跳过。
        assert!(
            !should_invoke_wasapi(false, None, true),
            "COM 未初始化 + pid 缺失时应跳过 WASAPI 调用"
        );
    }

    #[test]
    fn w11_skip_wasapi_when_all_conditions_missing() {
        // W11: 三个条件全不满足，应跳过。
        assert!(
            !should_invoke_wasapi(false, None, false),
            "三条件全不满足时应跳过 WASAPI 调用"
        );
    }

    // ========== 修复测试：mpv 子进程退出监听 ==========

    /// 验证 mpv 子进程异常退出后，监听线程更新 shared_state 为 Terminated。
    ///
    /// 由于 `VideoRenderer` 完整初始化需要真实 mpv 可执行文件，此测试使用
    /// `SubprocessRendererBase` + `cmd.exe` 启动短生命周期占位进程（ping 2 次 ~1s），
    /// 复用 `create_pause_sender` 中的 `OwnedProcHandle` + `WaitForSingleObject`
    /// 监听模式，验证子进程退出后 shared_state.state 正确更新为 Terminated，
    /// 且通过 notify_state_changed 发送了 display_id 通知。
    ///
    /// 完整端到端测试（真实 mpv 崩溃 → engine 状态刷新 → 前端 UI 更新）需集成测试覆盖。
    #[test]
    fn w002_video_process_exit_updates_state() {
        use std::path::PathBuf;
        use windows::Win32::System::Threading::WaitForSingleObject;

        // 定位 cmd.exe（优先使用 SystemRoot 环境变量，回退到 WINDIR）
        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("WINDIR"))
            .expect("SystemRoot/WINDIR 环境变量应存在");
        let cmd_path = PathBuf::from(system_root).join("System32").join("cmd.exe");
        assert!(cmd_path.exists(), "cmd.exe 应存在于 {}", cmd_path.display());

        // 构造 SubprocessRendererBase 并启动短生命周期进程
        // ping -n 2 127.0.0.1：发送 2 个 ICMP 包，间隔 1s，进程存活约 1s 后自行退出
        let mut base =
            SubprocessRendererBase::new(cmd_path, "test-w002-pipe".to_string(), ScalingMode::Fill);
        base.start_process(vec!["/c".to_string(), "ping -n 2 127.0.0.1".to_string()])
            .expect("启动测试占位进程应成功");

        // 复制进程句柄（与 create_pause_sender 中的模式一致）
        let proc_handle = base
            .duplicate_process_handle()
            .expect("duplicate_process_handle 应返回 Some");

        // 创建共享状态与通知通道（模拟 PauseSender 的 shared_state）
        let (sender, _rx, shared_state) = create_pause_channel();
        // 订阅状态变更通知，验证 notify_state_changed 被调用
        let mut state_rx = sender.subscribe_state_changes();
        let monitor_sender = sender.clone();
        let monitor_display_id = "test_display_w002".to_string();
        let monitor_shared = shared_state.clone();

        // 设置初始状态为 Playing（模拟 mpv 正在播放）
        shared_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .state = WallpaperState::Playing;

        // 使用 OwnedProcHandle 包装句柄，spawn 监听线程
        // （与 video.rs create_pause_sender 中监听线程模式一致）
        let mut owned = OwnedProcHandle::new(proc_handle);
        let monitor_thread = std::thread::Builder::new()
            .name("test-w002-video-proc-monitor".to_string())
            .spawn(move || {
                let proc_handle = match owned.take() {
                    Some(h) => h,
                    None => return,
                };
                // 无限等待子进程退出（占位进程 ~1s 后自行退出）
                let _ = unsafe { WaitForSingleObject(proc_handle, u32::MAX) };
                unsafe {
                    let _ = CloseHandle(proc_handle);
                }
                // 检查是否为异常退出：state != Terminated 表示非 terminate() 触发
                let state = monitor_shared
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .state;
                if state != WallpaperState::Terminated {
                    monitor_shared
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .state = WallpaperState::Terminated;
                    monitor_sender.notify_state_changed(&monitor_display_id);
                }
            })
            .expect("spawn 监听线程应成功");

        // 轮询等待进程退出与监听线程处理（最多 10s）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if shared_state.read().unwrap_or_else(|e| e.into_inner()).state
                == WallpaperState::Terminated
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "子进程退出后 shared_state.state 应更新为 Terminated，但等待 10s 后仍未更新"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // 验证状态已更新为 Terminated
        assert_eq!(
            shared_state.read().unwrap_or_else(|e| e.into_inner()).state,
            WallpaperState::Terminated,
            "子进程退出后 shared_state.state 应为 Terminated"
        );

        // 验证收到了状态变更通知（给监听线程发送通知的短暂时间）
        std::thread::sleep(std::time::Duration::from_millis(50));
        let notify_result = state_rx.try_recv();
        assert!(
            notify_result.is_ok(),
            "子进程退出后应通过 notify_state_changed 发送通知"
        );
        assert_eq!(
            notify_result.unwrap(),
            "test_display_w002",
            "通知 payload 应为 display_id"
        );

        // 等待监听线程退出
        monitor_thread.join().expect("监听线程应正常退出");

        // 清理子进程（stop_process 对已退出进程会立即返回）
        let _ = base.stop_process();
    }

    // ========== W-007 修复测试：set_speed 校验（合并 [Consistency]-12.1）==========

    /// 验证 `VideoRenderer::set_speed` 对非法速度值的校验。
    ///
    /// 覆盖三种非法值：`-1.0`（负数）、`NaN`（非数）、`0.0`（零）。
    /// 校验逻辑由共享函数 `validate_renderer_speed` 提供（与 `GifRenderer::set_speed` 一致）。
    ///
    /// 由于 `VideoRenderer` 不存储 speed 字段（与 GIF 不同），通过直接验证
    /// `validate_renderer_speed` 的返回值确认校验语义；同时调用 `set_speed`
    /// 确保非法值不引发 panic（早返回路径）。
    #[test]
    fn w007_video_set_speed_invalid_value_rejected() {
        // -1.0：负数应被拒绝
        assert!(!validate_renderer_speed(-1.0), "-1.0 应被拒绝（负数非法）");
        // NaN：非有限数应被拒绝
        assert!(
            !validate_renderer_speed(f32::NAN),
            "NaN 应被拒绝（非有限数非法）"
        );
        // 0.0：零应被拒绝（语义等同于暂停，应显式调用 pause()）
        assert!(
            !validate_renderer_speed(0.0),
            "0.0 应被拒绝（零非法，应调用 pause()）"
        );
        // 补充：Infinity 也应被拒绝（非有限数）
        assert!(
            !validate_renderer_speed(f32::INFINITY),
            "Infinity 应被拒绝（非有限数非法）"
        );
        assert!(
            !validate_renderer_speed(f32::NEG_INFINITY),
            "NegInfinity 应被拒绝（非有限数非法）"
        );

        // 验证 VideoRenderer::set_speed 对非法值不 panic（早返回路径）
        // 测试中 IPC 为 None，即便通过校验也不会实际发送 IPC；
        // 此处主要确保校验失败路径不引发异常。
        let mut renderer = create_renderer(ScalingMode::Fill);
        renderer.set_speed(-1.0);
        renderer.set_speed(f32::NAN);
        renderer.set_speed(0.0);
        renderer.set_speed(f32::INFINITY);
    }

    /// 验证 `VideoRenderer::set_speed` 对合法速度值的接受。
    ///
    /// 覆盖 `1.5`（加速）与 `0.5`（减速）两种合法值，另补充极小正数边界值。
    #[test]
    fn w007_video_set_speed_valid_value_accepted() {
        // 1.5：加速倍率应被接受
        assert!(validate_renderer_speed(1.5), "1.5 应被接受（合法正有限数）");
        // 0.5：减速倍率应被接受
        assert!(validate_renderer_speed(0.5), "0.5 应被接受（合法正有限数）");
        // 补充：极小正数应被接受（> 0 且有限）
        assert!(
            validate_renderer_speed(0.001),
            "0.001 应被接受（合法正有限数）"
        );
        // 补充：典型倍率 1.0 / 2.0 / 4.0 应被接受
        assert!(validate_renderer_speed(1.0), "1.0 应被接受");
        assert!(validate_renderer_speed(2.0), "2.0 应被接受");
        assert!(validate_renderer_speed(4.0), "4.0 应被接受");

        // 验证 VideoRenderer::set_speed 对合法值不 panic
        // 测试中 IPC 为 None，通过校验后进入 if let Some(ipc) 分支（None 不发送）
        let mut renderer = create_renderer(ScalingMode::Fill);
        renderer.set_speed(1.5);
        renderer.set_speed(0.5);
        renderer.set_speed(1.0);
    }

    // ========== v41-W-007 修复测试：Drop 显式断开 IPC socket 再停止进程 ==========

    /// 验证 `VideoRenderer::Drop` 显式断开 IPC socket 并立即终止进程。
    ///
    /// v41-W-007 修复：原 Drop 直接调用 `terminate()`，terminate 内部顺序为
    /// `ipc.quit() → stop_process() → ipc.disconnect()`。若 quit/stop_process 失败
    /// 或超时，disconnect 可能不被执行导致 IPC 管道句柄泄漏；且 stop_process 等待
    /// 进程退出会阻塞 Drop 数秒。
    ///
    /// 修复后 Drop 绕过 terminate()，直接执行：
    /// 1. 显式 disconnect IPC socket（关闭命名管道句柄，幂等）
    /// 2. 调用 process_manager.stop_immediate()（TerminateProcess + CloseHandle，毫秒级）
    /// 3. 清理 hwnd 与状态
    ///
    /// 测试策略（mock 场景）：构造一个 VideoRenderer，使用 cmd.exe 作为 "mpv 替身"
    /// 启动长生命周期子进程（ping -t 持续运行），手动设置 state 为 Playing 模拟
    /// "已启动未终止" 场景，然后 drop renderer。
    ///
    /// 验证：
    /// 1. Drop 后 ipc Arc 内部为 None（说明显式 disconnect 被调用并置 None）
    /// 2. Drop 后子进程已被终止（说明 stop_immediate 被调用）
    /// 3. Drop 完成时间 < 2s（说明使用 stop_immediate 毫秒级，而非 stop_process 超时等待）
    ///
    /// 关于顺序验证：disconnect 与 stop_immediate 的相对顺序由 Drop 实现的代码结构
    /// 保证（disconnect 在 stop_immediate 之前）。本测试通过验证两者均被执行来
    /// 间接验证修复的正确性。
    #[test]
    fn v41_w007_drop_closes_ipc_socket_before_process_stop() {
        use std::path::PathBuf;
        use std::time::Instant;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // 定位 cmd.exe（与 w002 测试一致的环境假设）
        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("WINDIR"))
            .expect("SystemRoot/WINDIR 环境变量应存在");
        let cmd_path = PathBuf::from(system_root).join("System32").join("cmd.exe");
        assert!(cmd_path.exists(), "cmd.exe 应存在于 {}", cmd_path.display());

        // 直接构造 VideoRenderer 结构体，绕过 new() 中的 find_mpv（避免依赖真实 mpv）
        let mut renderer = VideoRenderer {
            base: SubprocessRendererBase::new(
                cmd_path,
                "test-v41-w007-pipe".to_string(),
                ScalingMode::Fill,
            ),
            file_path: "test.mp4".to_string(),
            ipc: Arc::new(Mutex::new(None)),
            volume: 1.0,
            muted: false,
            volume_control: None,
        };

        // 启动长生命周期进程：ping -t 持续 ping，进程不会自行退出，必须由 stop_immediate 终止
        renderer
            .base
            .start_process(vec![
                "/c".to_string(),
                "ping -t 127.0.0.1 > nul".to_string(),
            ])
            .expect("启动测试占位进程应成功");

        let pid = renderer.base.process_pid().expect("PID 应已就绪");

        // 模拟 "已启动未终止" 状态（触发 Drop 清理路径）
        renderer.base.set_state(WallpaperState::Playing);

        // 克隆 ipc Arc 以便 drop 后验证内部状态
        let ipc_arc = renderer.ipc.clone();

        // 记录 drop 开始时间（用于验证 stop_immediate 的毫秒级行为）
        let drop_start = Instant::now();

        // Drop renderer，应触发 v41-W-007 修复路径：
        // 1. 显式 disconnect IPC（此处 ipc 为 None，disconnect 是 no-op，但仍置 None）
        // 2. stop_immediate 终止进程（TerminateProcess + CloseHandle，毫秒级）
        // 3. set_state(Terminated)
        drop(renderer);

        let drop_duration = drop_start.elapsed();

        // 验证 1: Drop 后 ipc 内部为 None（v41-W-007: 显式 disconnect 并置 None）
        assert!(
            ipc_arc.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "Drop 后 ipc 应为 None（v41-W-007: 显式 disconnect 并置 None）"
        );

        // 验证 2: Drop 完成时间应 < 2s
        // stop_immediate 直接 TerminateProcess + CloseHandle，毫秒级完成；
        // 若误用 stop_process（等待退出，超时后强杀），Drop 会长达数秒。
        assert!(
            drop_duration.as_secs() < 2,
            "Drop 应使用 stop_immediate（毫秒级），实际耗时 {:?}（若 >=2s 说明可能误用 stop_process）",
            drop_duration
        );

        // 验证 3: 子进程已被终止（exit_code != 259 STILL_ACTIVE）
        let process_handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        if let Ok(handle) = process_handle {
            let mut exit_code: u32 = 0;
            let _ = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
            unsafe {
                let _ = CloseHandle(handle);
            }
            assert_ne!(
                exit_code, 259,
                "Drop 后子进程应已被终止（exit_code != 259 STILL_ACTIVE）"
            );
        }
        // 若 OpenProcess 失败（进程已完全消失，OS 已回收），也视为通过
    }
}
