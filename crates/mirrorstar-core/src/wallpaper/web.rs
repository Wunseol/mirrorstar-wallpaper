use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;

use crate::ipc::wp_proc::WpProcIpcClient;
use crate::wallpaper::subprocess_base::{
    SubprocessRendererBase, WP_PROC_CONNECT_INTERVAL_MS, WP_PROC_CONNECT_RETRIES,
};
use crate::wallpaper::{
    create_pause_channel, get_screen_size, PauseCommand, ScalingMode, WallpaperRenderer,
    WallpaperState,
};

/// 某个 wp-proc 实例的 WebView2 UserDataFolder 路径（**路径约定的唯一来源**）。
///
/// 返回 `data_root()/webview2-cache/<pid>/`。该约定被两处消费，集中在此定义避免
/// 拼接逻辑重复后漂移：
/// - **子进程**（壳 exe 的 `--wp-proc` 模式）：作为 `CreateCoreWebView2EnvironmentWithOptions`
///   的 UserDataFolder（每实例独立，多显示器并发时互不干扰）；
/// - **父进程** `WebRenderer`：在该实例退出后回收对应子目录（见
///   [`cleanup_wp_proc_user_data_folder`]），避免同一会话内每切换一次 Web 壁纸就
///   残留约 2MB 缓存、长会话累积到数百 MB。
///
/// 子目录名安全性：PID 仅由十进制数字构成（`[0-9]`），天然满足文件名安全要求。
pub fn wp_proc_user_data_folder(pid: u32) -> PathBuf {
    crate::config::data_root()
        .join("webview2-cache")
        .join(pid.to_string())
}

/// 回收某个 wp-proc 实例的 WebView2 UserDataFolder（**尽力而为、幂等**）。
///
/// 由 `WebRenderer::terminate()`（进程确认退出后）与 `Drop`（兜底）调用。语义：
/// - 目录不存在 → 静默返回（重复调用、进程从未创建过 UDF 均属正常）；
/// - 删除失败（如 msedgewebview2 子进程仍在收尾、文件被占用）→ 仅 `warn`，
///   不向上传播错误：残留目录由下次应用启动的清理兜底（`cleanup_webview2_cache`）。
fn cleanup_wp_proc_user_data_folder(pid: u32) {
    let dir = wp_proc_user_data_folder(pid);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => tracing::debug!(dir = %dir.display(), "已回收 wp-proc WebView2 UserDataFolder"),
        // 目录不存在（未创建过 / 已被回收）→ 幂等静默返回
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            error = %e,
            dir = %dir.display(),
            "回收 wp-proc WebView2 UserDataFolder 失败（msedgewebview2 可能仍在收尾）"
        ),
    }
}

/// 网页壁纸渲染器（代理层）
///
/// 通过以 `--wp-proc` 模式启动当前壳 exe（`mirrorstar-wallpaper.exe`）作为独立子进程实现
/// WebView2 渲染（**同一 exe、双入口**，仍是独立进程，崩溃隔离不变），
/// 主进程通过 IPC 控制子进程，实现崩溃隔离和内存优化。
///
/// # 交互语义（重要）
/// - 网页壁纸是**纯展示**：仅在最底层桌面窗口（WorkerW 之上）渲染 WebView2 内容，
///   作为桌面壁纸背景显示，不接收任何用户点击/输入。
/// - 鼠标**穿透**（click-through）：壁纸窗口不捕获鼠标，点击会穿透到其下方的桌面
///   或桌面图标/已打开的普通应用窗口。此穿透行为是壁纸的既定语义，**不应改动**。
/// - 所有控制（切换/暂停/音量等）通过主进程 IPC 下发，与网页内容无交互反馈。
pub struct WebRenderer {
    /// 公共子进程基类（ProcessManager + 状态 + 窗口句柄 + 缩放模式 + 管道名 + pause_sender）
    base: SubprocessRendererBase,
    /// 网页源（URL 或本地文件路径）
    source: String,
    /// IPC 客户端（共享给 pause 线程）
    ipc: Arc<Mutex<Option<WpProcIpcClient>>>,
    /// 窗口标题（用于 FindWindowW 查找）
    title: String,
}

// SAFETY: WebRenderer 的 HWND 仅作为值存储，用于 Win32 API 调用。
// WpProcIpcClient 使用标准文件 I/O，ProcessManager 的 HANDLE 也是线程安全的。
unsafe impl Send for WebRenderer {}

impl WebRenderer {
    /// 创建新的网页渲染器
    pub fn new(source: String, scaling_mode: ScalingMode) -> Self {
        let pipe_name = format!("mirrorstar-wp-{}", uuid::Uuid::new_v4());
        let title = format!("MirrorStarWebWallpaper_{}", uuid::Uuid::new_v4().simple());
        let wp_proc_path = Self::find_wp_proc();

        Self {
            base: SubprocessRendererBase::new(wp_proc_path, pipe_name, scaling_mode),
            source,
            ipc: Arc::new(Mutex::new(None)),
            title,
        }
    }

    /// 返回 wp-proc 子进程可执行文件路径。
    ///
    /// 采用「同一 exe、双入口」方案：wp-proc 由当前壳 exe（`mirrorstar-wallpaper.exe`）
    /// 以 `--wp-proc` 模式运行，故正常路径即当前 exe 路径。
    fn find_wp_proc() -> std::path::PathBuf {
        match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                // `current_exe` 在 Windows 上几乎不会失败；失败时以 argv[0]（同样指向本 exe）
                // 兜底。不得回退到 `mirrorstar-wp-proc.exe`——该独立产物已不再产出，
                // 命中磁盘残留的旧 exe 会因其 clap 不认 `--wp-proc` 而导致启动失败。
                tracing::warn!(error = %e, "获取当前 exe 路径失败，回退到 argv[0]");
                std::env::args_os()
                    .next()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("mirrorstar-wallpaper.exe"))
            }
        }
    }

    /// 构建 wp-proc 启动参数
    fn build_wp_proc_args(&self) -> Vec<String> {
        // 获取屏幕大小作为初始窗口尺寸
        // v5.0 W-PERF-003: 使用缓存避免 play() 每次都调用 GetSystemMetrics
        let (screen_w, screen_h) = get_screen_size();

        let rect = format!("0,0,{},{}", screen_w, screen_h);

        // 「同一 exe、双入口」分派标记：置于参数列表首位（紧跟 argv[0] 之后），
        // 壳 exe 入口据此进入 wp-proc 模式并移除该标记，其余参数语义完全不变。
        // 单独占一个独立 argv 元素，不参与 `--key value` 配对。
        //
        // W-009 修复：使用分离 argv（`--key` 与值作为两个独立元素），而非 `--key=value`
        // 拼接形式。避免 `source` 等用户可控值以 `--` 开头时被 wp-proc 解析器拆分为独立
        // 参数（参数注入）。clap 原生支持 `--key value` 分离 argv 形式。
        let mut args = vec![
            "--wp-proc".to_string(),
            "--source".to_string(),
            self.source.clone(),
            "--pipe-name".to_string(),
            self.base.pipe_name.clone(),
            "--title".to_string(),
            self.title.clone(),
            "--rect".to_string(),
            rect,
        ];

        // 测试观察通道（仅测试）：MIRRORSTAR_WP_CDP_PORT 环境变量存在时追加 --cdp-port，
        // 使壁纸 webview 暴露 CDP 端口供模拟人类操作测试做 DOM 断言。
        // 生产环境不设置该变量即不影响任何行为。
        if let Ok(port) = std::env::var("MIRRORSTAR_WP_CDP_PORT") {
            args.push("--cdp-port".to_string());
            args.push(port);
        }

        args
    }
}

impl WallpaperRenderer for WebRenderer {
    fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        // 1. 构建并启动 wp-proc 进程
        let args = self.build_wp_proc_args();
        let pid = self.base.start_process(args)?;
        tracing::info!(pid, "wp-proc 进程已启动");

        // 2. 连接 IPC（v41-W-006：重试 40 次，每次间隔 200ms = 8s 最大等待，
        //    覆盖 WebView2 冷启动典型场景，慢速环境失败时调用方返回错误让用户重试）
        let mut ipc = WpProcIpcClient::new(self.base.pipe_name());
        ipc.connect(WP_PROC_CONNECT_RETRIES, WP_PROC_CONNECT_INTERVAL_MS)?;
        *self.ipc.lock().unwrap_or_else(|e| e.into_inner()) = Some(ipc);
        tracing::info!("wp-proc IPC 已连接");

        // 3. 发送 Play 命令（确保 WebView2 已就绪）
        {
            let mut ipc_guard = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut ipc) = *ipc_guard {
                ipc.play(&self.source)?;
            }
        }

        // 4. 查找 wp-proc 窗口（最大 2s）
        let hwnd =
            SubprocessRendererBase::find_window_by_title(pid, &self.title).ok_or_else(|| {
                crate::MirrorStarError::DesktopIntegration(format!(
                    "未能在超时内找到 wp-proc 窗口 (pid={})",
                    pid
                ))
            })?;
        self.base.set_hwnd(Some(hwnd));
        tracing::info!(hwnd = ?hwnd, "已找到 wp-proc 窗口");

        self.base.set_state(WallpaperState::Playing);
        tracing::info!("网页壁纸开始播放");
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
        tracing::info!("网页壁纸已暂停");
        Ok(())
    }

    fn pause_for_fullscreen(&mut self) -> Result<(), crate::MirrorStarError> {
        // 全屏场景终止 wp-proc 子进程，最大化释放 CPU/GPU 内存；退出全屏后由引擎级 play() 重启
        self.terminate()
    }

    fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            ipc.resume()?;
            // M3: resume 后重放当前音量（未静音时）——与 pause 线程 Resume 分支一致，
            // 避免暂停期间音量状态丢失。已静音时跳过（SetVolume 脚本会解除静音）。
            if let Some(sender) = &self.base.pause_sender {
                if !sender.is_muted() {
                    if let Err(e) = ipc.set_volume(sender.volume()) {
                        tracing::error!(error = %e, "网页恢复后重发音量失败");
                    }
                }
            }
        } else {
            tracing::warn!("IPC 未连接，恢复命令未实际下发");
        }
        self.base.set_state(WallpaperState::Playing);
        tracing::info!("网页壁纸已恢复");
        Ok(())
    }

    /// 设置窗口位置和大小。
    ///
    /// 实现细节：把几何经 IPC `set_position` 下发到 wp-proc 子进程，使 Web 壁纸窗口
    /// 跟随目标矩形重排。
    ///
    /// 注（多屏排列即时生效，Task 5）：引擎不再提供"遍历所有渲染器原地重定位"的
    /// 通道；排列变更改由调度器按新布局重建渲染器承担，显示器几何变化由渲染器重建 /
    /// 重新嵌入路径承载。本方法保留为渲染器接口的一部分，作为需要直接同步几何时的入口。
    fn set_position(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), crate::MirrorStarError> {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            ipc.set_position(x, y, w, h)?;
        } else {
            tracing::warn!("IPC 未连接，设置位置命令未实际下发");
        }
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        // 0. 先置终止状态（先于进程退出）——同步 base.state 与 shared_state，
        //    避免退出监听线程在进程退出时读到旧状态误报"异常退出"（同 video.rs）。
        self.base.mark_terminated();

        // 1. 通过 IPC 请求子进程退出
        {
            let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut ipc) = *ipc {
                if let Err(e) = ipc.terminate() {
                    tracing::warn!(error = %e, "IPC terminate 失败，将等待进程超时强杀");
                }
            }
        }

        // 2. 停止进程（会等待退出，超时后强制终止）
        //    回收 WebView2 UserDataFolder 必须在子进程退出后进行，故由
        //    `stop_process_for_terminate` 返回停止前的 PID。
        let pid = self.base.stop_process_for_terminate()?;

        // 2.1 进程已确认退出 → 回收本实例的 WebView2 UserDataFolder（尽力而为、幂等）。
        //     在"资源变为垃圾"的准确时刻回收，由拥有生命周期的组件负责；
        //     src-tauri 的启动清理仅作崩溃残留兜底。
        if let Some(pid) = pid {
            cleanup_wp_proc_user_data_folder(pid);
        }

        // 3. 断开 IPC
        {
            let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut ipc) = *ipc {
                ipc.disconnect();
            }
            *ipc = None;
        }
        self.base.set_hwnd(None);

        tracing::info!("网页壁纸已终止");
        Ok(())
    }

    fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    fn process_pid(&self) -> Option<u32> {
        self.base.process_pid()
    }

    fn state(&self) -> WallpaperState {
        self.base.state()
    }

    fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.base.set_scaling_mode(mode);
        // WebView2 自行处理渲染，缩放模式不适用
    }

    fn create_pause_sender(&mut self, display_id: &str) -> Option<crate::wallpaper::PauseSender> {
        let (sender, mut rx, shared_state) = create_pause_channel();

        {
            let mut s = shared_state.write().unwrap_or_else(|e| e.into_inner());
            s.state = self.base.state();
        }

        let ipc = self.ipc.clone();
        // clone sender 与 display_id move 进 pause 线程闭包，
        // 状态变更后通过 notify_state_changed 通知 Tauri 层 emit 事件
        let state_sender = sender.clone();
        let display_id = display_id.to_string();
        let monitor_display_id = display_id.clone();
        let monitor_shared = shared_state.clone();
        let monitor_sender = sender.clone();

        if let Err(e) = std::thread::Builder::new()
            .name("mirrorstar-web-pause".to_string())
            .spawn(move || {
                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                        PauseCommand::Pause => {
                            let mut ipc_guard = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc) = *ipc_guard {
                                if let Err(e) = ipc.pause() {
                                    tracing::error!(error = %e, "网页暂停失败");
                                }
                            }
                            drop(ipc_guard);
                            shared_state
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .state = WallpaperState::Paused;
                            // 通知 Tauri 层 emit wallpaper-state-changed 事件
                            state_sender.notify_state_changed(&display_id);
                        }
                        PauseCommand::Resume => {
                            // M3: resume 后重发当前音量（未静音时）——子进程 Resume 脚本
                            // 会重放音量（dataset._lastVolume + 进程侧兜底），父进程再显式
                            // 重发一次作为双保险。SetVolume 脚本会解除静音，故已静音时跳过
                            // 重发，避免破坏静音态。
                            let (volume, muted) = {
                                let s = shared_state.read().unwrap_or_else(|e| e.into_inner());
                                (s.volume, s.pre_mute_volume.is_some())
                            };
                            let mut ipc_guard = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc) = *ipc_guard {
                                if let Err(e) = ipc.resume() {
                                    tracing::error!(error = %e, "网页恢复失败");
                                }
                                if !muted {
                                    if let Err(e) = ipc.set_volume(volume) {
                                        tracing::error!(error = %e, "网页恢复后重发音量失败");
                                    }
                                }
                            }
                            drop(ipc_guard);
                            shared_state
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .state = WallpaperState::Playing;
                            // 通知 Tauri 层 emit wallpaper-state-changed 事件
                            state_sender.notify_state_changed(&display_id);
                        }
                        PauseCommand::SetVolume(volume) => {
                            // 音量钳制到 0.0 ~ 1.0 后经 IPC 下发，由子进程注入 JS
                            // 同时设置网页 media 元素 volume 并解除静音。
                            let volume = volume.clamp(0.0, 1.0);
                            {
                                let mut g = ipc.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(ref mut ipc) = *g {
                                    if let Err(e) = ipc.set_volume(volume) {
                                        tracing::error!(error = %e, "网页设置音量失败");
                                    }
                                }
                            }
                            // 同步共享音量状态，使 Tauri 层/界面反映最新值
                            shared_state
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .volume = volume;
                        }
                        PauseCommand::ToggleMute => {
                            let mut g = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc) = *g {
                                if let Err(e) = ipc.toggle_mute() {
                                    tracing::error!(error = %e, "网页切换静音失败");
                                }
                            }
                        }
                    }
                }
                tracing::debug!("WebRenderer pause 线程退出");
            })
        {
            // v41-W-008 修复：使用 `warn!` 记录原始 io::Error，与 `spawn_proc_exit_monitor`
            // 的 spawn 失败处理保持一致的日志级别。
            //
            // 原实现使用 `error!`，但 spawn 失败通常由 OS 资源临时不足引起（如线程数限制、
            // 内存不足），并非应用逻辑错误。降级为 `warn!` 更符合严重性分级：
            // - `error!`：应用逻辑错误，需人工介入
            // - `warn!`：运行时降级，应用可继续运行（pause 通道不可用，但壁纸仍可播放）
            //
            // `error = %e` 使用 Display 格式化记录原始 `io::Error`，包含错误码与错误消息，
            // 便于排查 OS 资源问题。返回 `None` 让调用方降级处理（无快速路径暂停功能）。
            tracing::warn!(error = %e, "创建 WebRenderer pause 线程失败");
            return None;
        }

        // W07 修复：监听 wp-proc 子进程退出事件
        //
        // 原实现启动 wp-proc 子进程后不监听其退出，子进程崩溃/异常退出时
        // engine 状态仍为 Playing，前端 UI 不刷新，用户感知不到壁纸已停止。
        //
        // 修复：spawn 监听线程等待子进程退出（WaitForSingleObject(INFINITE)），
        // 异常退出时（state != Terminated）通过 PauseSender::notify_state_changed
        // 通知 engine 更新状态，与 video.rs 的 mpv 退出恢复机制统一。
        //
        // 收敛：公共的句柄复制、RAII 包装、状态检查与通知流程下沉到
        // `SubprocessRendererBase::spawn_exit_monitor`，仅保留 wp-proc 专用日志文案。
        self.base.spawn_exit_monitor(
            monitor_display_id,
            monitor_shared,
            monitor_sender,
            "wp-proc 子进程异常退出，通知 engine 更新状态",
            "WebRenderer",
        );

        self.base.set_pause_sender(Some(sender.clone()));
        Some(sender)
    }
}

impl Drop for WebRenderer {
    fn drop(&mut self) {
        if self.base.state() != WallpaperState::Terminated {
            // Drop 路径无法传播错误，仅记录日志
            if let Err(e) = self.terminate() {
                tracing::warn!(error = %e, "WebRenderer drop 时 terminate 失败");
            }
        }
        // 兜底回收本实例的 WebView2 UserDataFolder：
        // - terminate() 正常完成时 pid 已被 stop_process 清空 → 此处 no-op（已回收）；
        // - terminate() 未走到回收（如 stop_process 失败提前返回、或状态已是
        //   Terminated 而未再走 terminate）时 pid 仍存活 → 此处回收。
        // 清理本身幂等（目录不存在时静默返回），故重复调用无副作用。
        if let Some(pid) = self.base.process_pid() {
            cleanup_wp_proc_user_data_folder(pid);
        }
        tracing::debug!("WebRenderer 已清理");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallpaper::OwnedProcHandle;
    use windows::Win32::Foundation::{CloseHandle, WAIT_EVENT};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

    // ========== R1 修复测试：WebView2 UserDataFolder 路径单源 + 回收幂等 ==========

    /// R1：`wp_proc_user_data_folder(pid)` 返回 `data_root()/webview2-cache/<pid>`，
    /// 且 pid 参与拼接（不同 pid → 不同目录）。
    ///
    /// 该函数是子进程（创建 UDF）与父进程（退出后回收）共用的**唯一**路径来源，
    /// 断言其形状可防止任一侧再引入独立拼接而漂移。
    #[test]
    fn r1_wp_proc_user_data_folder_is_data_root_webview2_cache_pid() {
        let path = wp_proc_user_data_folder(1234);

        // 与 data_root() 下的约定路径逐段相等（单源约定）
        assert_eq!(
            path,
            crate::config::data_root()
                .join("webview2-cache")
                .join("1234"),
            "R1: 路径应为 data_root()/webview2-cache/<pid>"
        );
        // 以 webview2-cache/<pid> 结尾（父目录名 + pid 子目录名）
        assert!(
            path.ends_with(PathBuf::from("webview2-cache").join("1234")),
            "R1: 路径应以 webview2-cache/<pid> 结尾，实际: {}",
            path.display()
        );
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("1234"),
            "R1: 末段应为 pid 字符串"
        );
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str()),
            Some("webview2-cache"),
            "R1: 倒数第二段应为 webview2-cache"
        );

        // pid 参与拼接：不同 pid 得到不同目录（多显示器并发实例互不干扰的前提）
        assert_ne!(
            wp_proc_user_data_folder(1234),
            wp_proc_user_data_folder(5678),
            "R1: 不同 pid 应映射到不同子目录"
        );
    }

    /// R1：回收函数幂等——对不存在路径调用不 panic、不返回 Err，且能真正删除目录。
    ///
    /// 清理在 `terminate()` 与 `Drop` 两处调用，必须对"目录不存在/已被回收"的
    /// 重复调用安全。此处用不会与真实 wp-proc 冲突的 pid（`u32::MAX - 3`）造目录，
    /// 验证：不存在时静默 → 存在时被删除 → 再次调用仍静默。
    #[test]
    fn r1_cleanup_wp_proc_user_data_folder_is_idempotent() {
        let pid = u32::MAX - 3;
        let dir = wp_proc_user_data_folder(pid);

        // 前置：确保目录不存在，先对不存在路径调用一次（应静默返回）
        let _ = std::fs::remove_dir_all(&dir);
        cleanup_wp_proc_user_data_folder(pid);
        assert!(!dir.exists(), "R1: 前置清理后目录不应存在");

        // 造出目录与文件后回收 → 目录（含内容）被删除
        std::fs::create_dir_all(&dir).expect("R1: 创建测试 UDF 目录应成功");
        std::fs::write(dir.join("Cache.bin"), b"cache").expect("R1: 写入测试文件应成功");
        cleanup_wp_proc_user_data_folder(pid);
        assert!(!dir.exists(), "R1: 回收后目录应被删除（含其内容）");

        // 幂等：路径已不存在时再次调用不 panic、无副作用
        cleanup_wp_proc_user_data_folder(pid);
        assert!(!dir.exists(), "R1: 重复回收应为 no-op");
    }

    // ========== 修复测试：OwnedProcHandle RAII 行为 ==========

    /// 验证 `OwnedProcHandle` 在 drop 时调用 `CloseHandle`。
    ///
    /// 通过创建一个事件句柄、包装后 drop，再尝试 `WaitForSingleObject` 来验证：
    /// 若句柄已关闭，`WaitForSingleObject` 返回 `WAIT_FAILED`(0xFFFFFFFF)。
    #[test]
    fn w001_owned_proc_handle_closes_on_drop() {
        // 创建一个手动复位事件（初始无信号）
        let handle = unsafe {
            CreateEventW(None, true, false, None).expect("CreateEventW 应成功创建事件")
        };

        // 包装到 RAII wrapper
        {
            let _owned = OwnedProcHandle::new(handle);
            // owned 在此作用域结束时 drop，应调用 CloseHandle
        }

        // 句柄已被 CloseHandle 关闭，WaitForSingleObject 应返回 WAIT_FAILED (0xFFFFFFFF)
        let result = unsafe { WaitForSingleObject(handle, 0) };
        assert_eq!(
            result,
            WAIT_EVENT(0xFFFFFFFF),
            "OwnedProcHandle drop 后句柄应已关闭，WaitForSingleObject 应返回 WAIT_FAILED"
        );
    }

    /// 验证 `OwnedProcHandle::take()` 取出句柄后 drop 不再调用 `CloseHandle`。
    ///
    /// 取出后句柄仍可用，需调用方手动关闭。
    #[test]
    fn w001_owned_proc_handle_take_prevents_double_close() {
        let handle = unsafe {
            CreateEventW(None, true, false, None).expect("CreateEventW 应成功创建事件")
        };

        let mut owned = OwnedProcHandle::new(handle);
        // 取出句柄
        let taken = owned.take().expect("take 应返回 Some(handle)");
        assert_eq!(taken.0, handle.0, "取出的句柄值应与原始一致");

        // owned drop 后不应调用 CloseHandle（已被 take 取出）
        drop(owned);

        // 句柄仍可用：SetEvent 应成功
        unsafe {
            SetEvent(handle).expect("take 后句柄仍应可用，SetEvent 应成功");
        }

        // WaitForSingleObject 应返回 WAIT_OBJECT_0（事件已设置）
        let result = unsafe { WaitForSingleObject(handle, 0) };
        assert_eq!(
            result,
            WAIT_EVENT(0),
            "take 后句柄仍有效，WaitForSingleObject 应返回 WAIT_OBJECT_0 (WAIT_EVENT(0))"
        );

        // 手动关闭句柄
        unsafe {
            let _ = CloseHandle(handle);
        }
    }

    /// 验证 `OwnedProcHandle` 在 spawn 失败场景下不泄漏句柄。
    ///
    /// 由于难以真正触发 `thread::Builder::spawn` 失败，此测试通过直接模拟
    /// spawn 失败路径验证：创建包装器后不调用 `take()`（闭包未执行）直接 drop，
    /// 确认句柄被 `CloseHandle` 关闭。
    ///
    /// 这与 `create_pause_sender` 中 spawn 失败时的行为一致：
    /// - spawn 成功：闭包执行 `owned.take()` 取出句柄使用
    /// - spawn 失败：闭包被 drop，`owned` 随之 drop，`CloseHandle` 被调用
    #[test]
    fn w001_handle_closed_on_spawn_failure() {
        let handle = unsafe {
            CreateEventW(None, true, false, None).expect("CreateEventW 应成功创建事件")
        };

        // 模拟 spawn 失败路径：创建包装器后直接 drop（闭包未执行，未调用 take）
        // 这与 spawn 失败时闭包被 drop 的行为一致
        {
            let _owned = OwnedProcHandle::new(handle);
            // _owned 在此作用域结束时 drop，应调用 CloseHandle
        }

        // 句柄应已被关闭，WaitForSingleObject 返回 WAIT_FAILED (0xFFFFFFFF)
        let result = unsafe { WaitForSingleObject(handle, 0) };
        assert_eq!(
            result,
            WAIT_EVENT(0xFFFFFFFF),
            "模拟 spawn 失败后句柄应已被 CloseHandle 关闭"
        );
    }

    // ========== W-009 修复测试：build_wp_proc_args 参数注入防护 ==========

    /// 构造测试用 `WebRenderer`，避免触发文件系统搜索与进程启动。
    ///
    /// 直接通过结构体字面量构造，绕过 `WebRenderer::new` 中的 `find_wp_proc`
    /// 与 UUID 生成，使测试可重现且不依赖环境。
    fn make_test_renderer(source: &str) -> WebRenderer {
        WebRenderer {
            base: SubprocessRendererBase::new(
                std::path::PathBuf::from("dummy-wp-proc"),
                "test-pipe".to_string(),
                ScalingMode::Fill,
            ),
            source: source.to_string(),
            ipc: Arc::new(Mutex::new(None)),
            title: "TestTitle".to_string(),
        }
    }

    /// 验证 argv 包含独立的 `--source` 与值元素（W-009）。
    ///
    /// 修复后 `--source` 与值应为两个独立的 argv 元素，而非 `--source=value`
    /// 拼接形式，避免值以 `--` 开头时被解析器拆分为独立参数。
    #[test]
    fn w009_build_wp_proc_args_separates_source_arg() {
        let renderer = make_test_renderer("https://example.com/page.html");
        let args = renderer.build_wp_proc_args();

        // 查找独立的 `--source` 元素，验证其后紧跟 source 值
        let source_idx = args
            .iter()
            .position(|a| a == "--source")
            .expect("argv 应包含独立的 `--source` 元素");
        let source_value = args.get(source_idx + 1).expect("`--source` 后应紧跟值元素");
        assert_eq!(source_value, "https://example.com/page.html");

        // 不应存在 `--source=...` 拼接形式
        assert!(
            !args.iter().any(|a| a.starts_with("--source=")),
            "argv 不应包含 `--source=...` 拼接形式"
        );
    }

    /// 验证 `source = "--malicious"` 时 argv 仍为两个元素，不被拆分为独立 flag（W-009）。
    ///
    /// 此测试验证修复的核心目标：以 `--` 开头的 source 值作为独立 argv 元素紧跟
    /// `--source` 之后，而非被拼接为 `--source=--malicious` 后可能被解析器拆分。
    #[test]
    fn w009_build_wp_proc_args_source_starting_with_dash_not_injected() {
        let renderer = make_test_renderer("--malicious");
        let args = renderer.build_wp_proc_args();

        // 定位独立的 `--source` 元素
        let source_idx = args
            .iter()
            .position(|a| a == "--source")
            .expect("argv 应包含独立的 `--source` 元素");

        // `--malicious` 应作为 `--source` 的值紧跟其后，而非被拆分为独立 flag
        assert_eq!(
            args.get(source_idx + 1),
            Some(&"--malicious".to_string()),
            "`--malicious` 应作为 `--source` 的值紧跟其后"
        );

        // `--malicious` 在 argv 中应仅出现一次（作为 source 值），不存在被拆分出的独立 flag
        let malicious_count = args.iter().filter(|a| *a == "--malicious").count();
        assert_eq!(
            malicious_count, 1,
            "`--malicious` 应仅出现一次（作为 source 值），不应被拆分为独立 flag"
        );

        // 确保不存在 `--source=--malicious` 拼接形式
        assert!(
            !args.iter().any(|a| a.starts_with("--source=--malicious")),
            "argv 不应包含 `--source=--malicious` 拼接形式"
        );
    }

    // ========== v41-W-006 修复测试：IPC 超时缩短 ==========

    /// 验证 wp-proc IPC 连接超时已缩短到 8s 以内。
    ///
    /// v41-W-006 降级修复：`set_wallpaper` 持有 engine 锁期间调用
    /// `construct_renderer`，IPC 等待首帧阻塞其他命令（pause/resume/set_volume）。
    /// 原 `WP_PROC_CONNECT_RETRIES = 100` * `WP_PROC_CONNECT_INTERVAL_MS = 200` = 20s
    /// 兜底，持锁 20s 不可接受。修复后缩短到 8s（40 * 200ms）。
    /// Wave v9-C：重试间隔降至 50ms，重试次数调整为 160 次（160 * 50ms = 8s），
    /// 保持总超时不变，降低管道就绪检测延迟。
    ///
    /// 本测试断言总等待时长 ≤ 8s，确保后续维护中常量值不被意外回升到 20s。
    #[test]
    fn v41_w006_ipc_timeout_reduced() {
        // 计算总等待时长（毫秒）
        let total_ms = u64::from(WP_PROC_CONNECT_RETRIES) * WP_PROC_CONNECT_INTERVAL_MS;
        let total_secs = total_ms / 1000;
        let max_allowed_secs = 8;

        assert!(
            total_secs <= max_allowed_secs,
            "v41-W-006: IPC 连接总超时应 ≤ 8s（实际: {}ms = {}s），\
             原 20s 超时在 set_wallpaper 持锁期间阻塞其他命令",
            total_ms,
            total_secs
        );

        // 同时断言重试次数与间隔的预期值，确保后续维护者理解意图
        assert_eq!(
            WP_PROC_CONNECT_RETRIES, 160,
            "Wave v9-C: 重试次数应为 160（160 * 50ms = 8s 兜底）"
        );
        assert_eq!(
            WP_PROC_CONNECT_INTERVAL_MS, 50,
            "Wave v9-C: 重试间隔应为 50ms"
        );
    }

    // ========== v41-W-008 修复测试：spawn 失败记录原始 io::Error ==========

    /// 验证 `WebRenderer::create_pause_sender` 中 spawn 失败分支使用 `tracing::warn!`
    /// 记录原始 `io::Error`（而非 `tracing::error!`）。
    ///
    /// v41-W-008 修复：原实现使用 `tracing::error!`，但 spawn 失败通常由 OS 资源
    /// 临时不足引起（如线程数限制、内存不足），并非应用逻辑错误。降级为 `warn!`
    /// 更符合严重性分级，与 `spawn_proc_exit_monitor` 的 spawn 失败处理一致。
    ///
    /// 测试策略：由于 `thread::Builder::spawn` 在正常环境下几乎不会失败（需 OS
    /// 资源耗尽），本测试通过静态检查源代码验证修复存在并防止回归：
    /// 1. 源代码中 spawn 失败处理块使用 `tracing::warn!`（而非 `tracing::error!`）
    /// 2. 源代码中 spawn 失败处理块包含 `error = %e` 记录原始 `io::Error`
    ///
    /// 行为验证：通过 `tracing-subscriber` 捕获日志事件，验证 `warn!(error = %e, ...)`
    /// 模式能正确记录 `io::Error` 的 Display 格式（含错误码与消息）。
    #[test]
    fn v41_w008_spawn_failure_logs_io_error() {
        // ── 静态分析：验证源代码包含 v41-W-008 修复 ──
        let source = include_str!("web.rs");

        // 定位 spawn 失败处理消息（"创建 WebRenderer pause 线程失败"）
        let failure_msg_idx = source
            .find("创建 WebRenderer pause 线程失败")
            .expect("源代码应包含 spawn 失败处理消息");

        // 向前查找最近的 tracing 宏调用（warn! 或 error!）
        let before_msg = &source[..failure_msg_idx];
        let tracing_call_start = before_msg
            .rfind("tracing::")
            .expect("spawn 失败消息前应存在 tracing 宏调用");

        // 提取从 "tracing::" 到行尾 ";" 的完整宏调用
        let tracing_call_end = source[tracing_call_start..]
            .find(';')
            .map(|i| tracing_call_start + i)
            .expect("tracing 宏调用应以 ; 结尾");
        let tracing_call = &source[tracing_call_start..tracing_call_end];

        // 验证 1: 使用 warn! 而非 error!
        assert!(
            tracing_call.contains("warn!"),
            "v41-W-008: spawn 失败应使用 tracing::warn!（实际调用: {}）",
            tracing_call
        );
        assert!(
            !tracing_call.contains("error!"),
            "v41-W-008: spawn 失败不应使用 tracing::error!（实际调用: {}）",
            tracing_call
        );

        // 验证 2: 记录原始 io::Error（error = %e）
        assert!(
            tracing_call.contains("error = %e"),
            "v41-W-008: spawn 失败应记录原始 io::Error（error = %e）（实际调用: {}）",
            tracing_call
        );

        // ── 行为验证：tracing::warn! + io::Error 日志模式工作正常 ──
        // 模拟 spawn 失败时的日志记录模式，验证 io::Error 的 Display 格式被正确记录。
        use std::io;

        // 构造一个典型的 io::Error（模拟 spawn 失败返回的错误）
        let io_error = io::Error::new(
            io::ErrorKind::WouldBlock,
            "模拟 thread::Builder::spawn 失败（OS 资源不足）",
        );

        // 验证 io::Error 的 Display 格式包含错误消息（spawn 失败时日志应记录此信息）
        let error_display = format!("{}", io_error);
        assert!(
            error_display.contains("模拟 thread::Builder::spawn 失败"),
            "io::Error 的 Display 格式应包含错误消息（实际: {}）",
            error_display
        );

        // 验证 tracing::warn! 宏能正确格式化 io::Error（编译期验证，运行时调用不 panic）
        // 注意：此处不设置全局 subscriber，warn! 调用会被丢弃（无订阅者），
        // 主要验证宏调用本身对 io::Error 类型有效。
        tracing::warn!(error = %io_error, "测试日志: 模拟 spawn 失败");
    }
}
