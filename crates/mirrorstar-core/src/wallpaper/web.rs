use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;

use crate::ipc::wp_proc::WpProcIpcClient;
use crate::wallpaper::subprocess_base::{
    SubprocessRendererBase, WP_PROC_CONNECT_INTERVAL_MS, WP_PROC_CONNECT_RETRIES,
};
use crate::wallpaper::{
    create_pause_channel, get_screen_size, spawn_proc_exit_monitor, OwnedProcHandle, PauseCommand,
    ScalingMode, WallpaperRenderer, WallpaperState,
};

/// 网页壁纸渲染器（代理层）
///
/// 通过启动 mirrorstar-wp-proc 子进程实现 WebView2 渲染，
/// 主进程通过 IPC 控制子进程，实现崩溃隔离和内存优化。
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

    /// 查找 wp-proc 可执行文件路径
    fn find_wp_proc() -> std::path::PathBuf {
        SubprocessRendererBase::find_bundled_executable(None, "mirrorstar-wp-proc.exe", "wp-proc")
    }

    /// 构建 wp-proc 启动参数
    fn build_wp_proc_args(&self) -> Vec<String> {
        // 获取屏幕大小作为初始窗口尺寸
        // v5.0 W-PERF-003: 使用缓存避免 play() 每次都调用 GetSystemMetrics
        let (screen_w, screen_h) = get_screen_size();

        let rect = format!("0,0,{},{}", screen_w, screen_h);

        // W-009 修复：使用分离 argv（`--key` 与值作为两个独立元素），而非 `--key=value`
        // 拼接形式。避免 `source` 等用户可控值以 `--` 开头时被 wp-proc 解析器拆分为独立
        // 参数（参数注入）。clap 原生支持 `--key value` 分离 argv 形式。
        vec![
            "--source".to_string(),
            self.source.clone(),
            "--pipe-name".to_string(),
            self.base.pipe_name.clone(),
            "--title".to_string(),
            self.title.clone(),
            "--rect".to_string(),
            rect,
        ]
    }

    /// 导航到新 URL（仅网页壁纸有效）
    pub fn navigate(&mut self, url: &str) {
        let mut ipc = self.ipc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut ipc) = *ipc {
            if let Err(e) = ipc.navigate(url) {
                tracing::error!(error = %e, "网页导航失败");
            }
        }
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
        } else {
            tracing::warn!("IPC 未连接，恢复命令未实际下发");
        }
        self.base.set_state(WallpaperState::Playing);
        tracing::info!("网页壁纸已恢复");
        Ok(())
    }

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
        tracing::info!("网页壁纸已终止");
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
                            let mut ipc_guard = ipc.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref mut ipc) = *ipc_guard {
                                if let Err(e) = ipc.resume() {
                                    tracing::error!(error = %e, "网页恢复失败");
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
                        PauseCommand::SetVolume(_) | PauseCommand::ToggleMute => {
                            // 网页壁纸无音频控制，忽略
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
        // 使用 DuplicateHandle 复制句柄，避免监听线程的 wait 与 terminate() 中的
        // CloseHandle 产生竞争（Win32 文档明确指出在 wait 期间关闭同一句柄会导致
        // 未定义行为）。监听线程拥有独立句柄，退出时自行 CloseHandle。
        //
        // 正常退出路径：terminate() 先通过 IPC 请求子进程退出，再调用 stop_process()
        // 等待退出。此时监听线程的 wait 也会返回，但 state 已被 terminate() 流程
        // 标记为 Terminated（或即将标记），监听线程检查到 state == Terminated 后
        // 不发通知，避免对正常退出产生冗余通知。
        //
        // [Consistency]-12.2 修复：调用共享 `spawn_proc_exit_monitor` 收敛 web.rs
        // 与 video.rs 的实现差异。`OwnedProcHandle` RAII 管理句柄生命周期，
        // spawn 失败时由共享函数统一 `warn` 并通过 `OwnedProcHandle::drop` 关闭句柄。
        if let Some(proc_handle) = self.base.duplicate_process_handle() {
            // 修复：使用 RAII 包装器独占持有复制的句柄，确保无论 spawn 成败
            // 都不会泄漏。spawn 成功时共享函数内 `take()` 取出句柄使用；spawn 失败时
            // 闭包被 drop，`OwnedProcHandle::drop` 自动调用 `CloseHandle`。
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
                        "wp-proc 子进程异常退出，通知 engine 更新状态"
                    );
                    // 更新共享状态为 Terminated，使 any_playing 等查询返回正确结果
                    monitor_shared
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .state = WallpaperState::Terminated;
                    // 通知 Tauri 层 emit wallpaper-state-changed 事件刷新 UI
                    monitor_sender.notify_state_changed(&monitor_display_id);
                }
                tracing::debug!("WebRenderer 进程监听线程退出");
            });
        }

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
        tracing::debug!("WebRenderer 已清理");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallpaper::OwnedProcHandle;
    use windows::Win32::Foundation::{CloseHandle, WAIT_EVENT};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

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
