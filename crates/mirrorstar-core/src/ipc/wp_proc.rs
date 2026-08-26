use std::time::Duration;

use crate::ipc::client::NamedPipeClient;

/// I04: play 命令的超时时间（15s）。
///
/// `play` 路径需等待 WebView2 初始化完成（可能涉及进程启动、运行时下载等），
/// 默认 5s 超时不足。此处改用 15s 超时，与 connect 的 20s 兜底匹配，
/// 避免慢速环境下误超时。提取为常量便于 [`WpProcIpcClient::send_command`] 引用，
/// 以及在测试中通过 [`WpProcIpcClient::send_command_with_timeout`] 传入较短超时
/// 验证超时机制（无需等待真实 15s）。
const PLAY_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Web 壁纸子进程 IPC 命令
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum WpProcCommand {
    /// 播放指定源
    Play { request_id: u64, source: String },
    /// 终止子进程
    Terminate { request_id: u64 },
    /// 设置窗口位置和大小
    SetPosition {
        request_id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// 导航到指定 URL
    Navigate { request_id: u64, url: String },
    /// 暂停播放
    Pause { request_id: u64 },
    /// 恢复播放
    Resume { request_id: u64 },
}

impl WpProcCommand {
    /// 获取命令的 request_id
    pub fn request_id(&self) -> u64 {
        match self {
            WpProcCommand::Play { request_id, .. }
            | WpProcCommand::Terminate { request_id }
            | WpProcCommand::SetPosition { request_id, .. }
            | WpProcCommand::Navigate { request_id, .. }
            | WpProcCommand::Pause { request_id }
            | WpProcCommand::Resume { request_id } => *request_id,
        }
    }
}

/// IPC 响应状态
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    Ok,
    Error,
}

/// Web 壁纸子进程 IPC 响应
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WpProcResponse {
    pub request_id: u64,
    pub status: ResponseStatus,
    pub error: Option<String>,
}

/// Web 壁纸子进程 IPC 客户端，通过命名管道控制 mirrorstar-wp-proc
///
/// 基于 `NamedPipeClient<T>` 泛型基类的薄封装，仅实现 wp-proc 协议特定的
/// 命令构造与响应解析逻辑；连接/断开/读写等通用行为委托给基类。
/// 类型参数 `T = WpProcCommand` 即本协议的命令枚举。
pub struct WpProcIpcClient {
    inner: NamedPipeClient<WpProcCommand>,
}

impl WpProcIpcClient {
    /// 创建新的 IPC 客户端
    pub fn new(pipe_name: &str) -> Self {
        Self {
            inner: NamedPipeClient::new(pipe_name),
        }
    }

    /// 连接到 Web 壁纸子进程命名管道
    /// 重试最多 retry_count 次，每次间隔 retry_interval_ms 毫秒
    pub fn connect(
        &mut self,
        retry_count: u32,
        retry_interval_ms: u64,
    ) -> Result<(), crate::MirrorStarError> {
        self.inner.connect(retry_count, retry_interval_ms)
    }

    /// 发送命令到 Web 壁纸子进程并等待响应
    ///
    /// 保留用于 `play` 等需要确认子进程初始化是否成功的命令；
    /// pause / resume / terminate / set_position / navigate 等控制类命令
    /// 应改用 [`send_command_no_wait`](Self::send_command_no_wait) 避免 ack 等待（Bug #7）。
    ///
    /// I04 超时对齐：`play` 路径需等待 WebView2 初始化完成（可能涉及进程启动、
    /// 运行时下载等），默认 5s 超时不足。此处改用 15s 超时，与 connect 的 20s
    /// 兜底匹配，避免 play 命令在慢速环境下误超时。
    pub fn send_command(
        &mut self,
        command: WpProcCommand,
    ) -> Result<WpProcResponse, crate::MirrorStarError> {
        self.send_command_with_timeout(command, PLAY_COMMAND_TIMEOUT)
    }

    /// 发送命令并使用自定义超时等待响应（I04 测试入口）
    ///
    /// 与 [`send_command`](Self::send_command) 行为一致，仅超时可由调用方指定。
    /// 提取此方法是为单元测试覆盖 I04 超时机制——`send_command` 硬编码 15s 超时，
    /// 在测试中等待 15s 不可接受；本方法允许传入较短超时（如 200ms）验证超时行为。
    ///
    /// 生产代码应使用 [`send_command`](Self::send_command)（固定 15s）。
    ///
    /// 与 `MpvIpcClient::send_command_with_timeout` 结构对称，差异仅在命令/响应类型。
    pub fn send_command_with_timeout(
        &mut self,
        command: WpProcCommand,
        timeout: Duration,
    ) -> Result<WpProcResponse, crate::MirrorStarError> {
        let req_id = command.request_id();

        // 序列化命令为 JSON 并写入（send_line 自动追加换行符）
        let cmd_json = serde_json::to_string(&command)
            .map_err(|e| crate::MirrorStarError::IpcError(format!("命令序列化失败: {}", e)))?;
        self.inner.send_line(&cmd_json)?;

        // I04: 使用调用方指定的超时（生产路径为 15s，测试路径可缩短）。
        // 总体超时由 read_response_line_with_timeout 内部的 deadline 机制保证
        // （循环跳过空行/不匹配行时累计不超过 timeout）。
        loop {
            let line = self.inner.read_response_line_with_timeout(timeout)?;

            // 尝试解析为响应
            if let Ok(response) = serde_json::from_str::<WpProcResponse>(&line) {
                if response.request_id == req_id {
                    if response.status != ResponseStatus::Ok {
                        return Err(crate::MirrorStarError::IpcError(format!(
                            "wp-proc 命令失败: {}",
                            response
                                .error
                                .unwrap_or_else(|| format!("{:?}", response.status))
                        )));
                    }
                    return Ok(response);
                }
                // request_id 不匹配，继续读取（可能是之前命令的延迟响应）
            }
            // 无法解析为响应，跳过
        }
    }

    /// 发送命令到 Web 壁纸子进程，不等待响应（fire-and-forget）
    ///
    /// 仅序列化命令为 JSON 写入管道即返回，不读取子进程的 ack 响应。
    /// 适用于 pause / resume / terminate / set_position / navigate 等无需返回数据的命令，
    /// 避免子进程处理延迟时同步等待 5s 超时导致 UI 卡顿（Bug #7）。
    ///
    /// I-008：延迟响应副作用（与 mpv 侧 `MpvIpcClient::send_command_no_wait` 一致）。
    /// 注意：调用方无法感知子进程是否成功执行命令。若命令失败，子进程仍会异步
    /// 返回 error 响应，但本方法不读取该响应——后续的 `send_command_with_timeout`
    /// 调用可能会读到这条延迟 error 响应并因 `request_id` 不匹配而跳过。
    ///
    /// 对于 `play` 等需要确认 WebView2 初始化成功的命令，仍应使用同步路径
    /// [`send_command`](Self::send_command)。
    pub fn send_command_no_wait(
        &mut self,
        command: WpProcCommand,
    ) -> Result<(), crate::MirrorStarError> {
        let cmd_json = serde_json::to_string(&command)
            .map_err(|e| crate::MirrorStarError::IpcError(format!("命令序列化失败: {}", e)))?;
        self.inner.send_line(&cmd_json)?;
        Ok(())
    }

    /// 播放指定源（同步等待响应，确认 WebView2 初始化成功）
    ///
    /// I-009：调用方契约——URL/源路径校验由调用方负责（上层 `WallpaperEngine` /
    /// 命令处理层已做协议白名单校验）。本方法作为 IPC 层薄封装，不做防御性校验，
    /// 直接序列化 source 为 JSON 发送给子进程。若未来需要在 IPC 层做纵深防御，
    /// 可参考 `MirrorStarError::InvalidUrl { scheme }` 变体添加协议白名单。
    pub fn play(&mut self, source: &str) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command(WpProcCommand::Play {
            request_id,
            source: source.to_string(),
        })?;
        Ok(())
    }

    /// 终止子进程（fire-and-forget）
    ///
    /// 调用方（如 `WebRenderer::terminate`）会在发送后通过 `stop_process`
    /// 等待进程退出并兜底强杀，ack 等待反而可能在子进程已崩溃时拖慢清理流程。
    pub fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command_no_wait(WpProcCommand::Terminate { request_id })
    }

    /// 设置窗口位置和大小（fire-and-forget）
    pub fn set_position(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command_no_wait(WpProcCommand::SetPosition {
            request_id,
            x,
            y,
            width,
            height,
        })
    }

    /// 导航到指定 URL（fire-and-forget）
    ///
    /// I-009：调用方契约——URL/源路径校验由调用方负责（上层 `WallpaperEngine` /
    /// 命令处理层已做协议白名单校验）。本方法作为 IPC 层薄封装，不做防御性校验，
    /// 直接序列化 url 为 JSON 发送给子进程。若未来需要在 IPC 层做纵深防御，
    /// 可参考 `MirrorStarError::InvalidUrl { scheme }` 变体添加协议白名单。
    pub fn navigate(&mut self, url: &str) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command_no_wait(WpProcCommand::Navigate {
            request_id,
            url: url.to_string(),
        })
    }

    /// 暂停播放（fire-and-forget，Bug #7）
    pub fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command_no_wait(WpProcCommand::Pause { request_id })
    }

    /// 恢复播放（fire-and-forget，Bug #7）
    pub fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        let request_id = self.inner.next_request_id();
        self.send_command_no_wait(WpProcCommand::Resume { request_id })
    }

    /// 断开连接
    pub fn disconnect(&mut self) {
        self.inner.disconnect();
        tracing::info!("已断开 wp-proc IPC 连接: {}", self.inner.pipe_path());
    }

    /// 获取管道路径
    pub fn pipe_path(&self) -> &str {
        self.inner.pipe_path()
    }
}

// I-007：Drop 双重 disconnect 幂等性说明
//
// 外层 Drop 调用 `disconnect()` 后，`self.inner`（`NamedPipeClient`）drop 时会
// 再次调用 `disconnect()`。因 `disconnect` 使用 `Option::take()` 实现幂等性，
// 第二次调用是安全 no-op。保留外层 Drop 是为记录 `tracing::info!` 断开日志
// （`NamedPipeClient::Drop` 不记录日志）。
impl Drop for WpProcIpcClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- WpProcCommand serialization tests ---

    #[test]
    fn wp_proc_command_play_serialization() {
        let cmd = WpProcCommand::Play {
            request_id: 1,
            source: "https://example.com".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "play");
        assert_eq!(v["request_id"], 1);
        assert_eq!(v["source"], "https://example.com");
    }

    #[test]
    fn wp_proc_command_terminate_serialization() {
        let cmd = WpProcCommand::Terminate { request_id: 2 };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "terminate");
        assert_eq!(v["request_id"], 2);
    }

    #[test]
    fn wp_proc_command_set_position_serialization() {
        let cmd = WpProcCommand::SetPosition {
            request_id: 3,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "set_position");
        assert_eq!(v["request_id"], 3);
        assert_eq!(v["x"], 0);
        assert_eq!(v["y"], 0);
        assert_eq!(v["width"], 1920);
        assert_eq!(v["height"], 1080);
    }

    #[test]
    fn wp_proc_command_navigate_serialization() {
        let cmd = WpProcCommand::Navigate {
            request_id: 4,
            url: "https://example.com".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "navigate");
        assert_eq!(v["request_id"], 4);
        assert_eq!(v["url"], "https://example.com");
    }

    #[test]
    fn wp_proc_command_pause_serialization() {
        let cmd = WpProcCommand::Pause { request_id: 5 };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "pause");
        assert_eq!(v["request_id"], 5);
    }

    #[test]
    fn wp_proc_command_resume_serialization() {
        let cmd = WpProcCommand::Resume { request_id: 6 };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "resume");
        assert_eq!(v["request_id"], 6);
    }

    // --- WpProcCommand::request_id tests ---

    #[test]
    fn wp_proc_command_request_id_play() {
        let cmd = WpProcCommand::Play {
            request_id: 10,
            source: "x".to_string(),
        };
        assert_eq!(cmd.request_id(), 10);
    }

    #[test]
    fn wp_proc_command_request_id_terminate() {
        let cmd = WpProcCommand::Terminate { request_id: 20 };
        assert_eq!(cmd.request_id(), 20);
    }

    #[test]
    fn wp_proc_command_request_id_set_position() {
        let cmd = WpProcCommand::SetPosition {
            request_id: 30,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(cmd.request_id(), 30);
    }

    #[test]
    fn wp_proc_command_request_id_navigate() {
        let cmd = WpProcCommand::Navigate {
            request_id: 40,
            url: "x".to_string(),
        };
        assert_eq!(cmd.request_id(), 40);
    }

    #[test]
    fn wp_proc_command_request_id_pause() {
        let cmd = WpProcCommand::Pause { request_id: 50 };
        assert_eq!(cmd.request_id(), 50);
    }

    #[test]
    fn wp_proc_command_request_id_resume() {
        let cmd = WpProcCommand::Resume { request_id: 60 };
        assert_eq!(cmd.request_id(), 60);
    }

    // --- WpProcResponse deserialization tests ---

    #[test]
    fn wp_proc_response_ok() {
        let json = r#"{"request_id":1,"status":"ok"}"#;
        let resp: WpProcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.request_id, 1);
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.error, None);
    }

    #[test]
    fn wp_proc_response_error() {
        let json = r#"{"request_id":1,"status":"error","error":"WebView2 初始化失败"}"#;
        let resp: WpProcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.request_id, 1);
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.error, Some("WebView2 初始化失败".to_string()));
    }

    #[test]
    fn wp_proc_response_error_missing_error_field() {
        let json = r#"{"request_id":2,"status":"error"}"#;
        let resp: WpProcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.request_id, 2);
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.error, None);
    }

    // --- WpProcIpcClient::new tests ---

    #[test]
    fn ipc_client_new_pipe_path() {
        let client = WpProcIpcClient::new("test-pipe");
        assert_eq!(client.pipe_path(), r"\\.\pipe\test-pipe");
    }

    #[test]
    fn ipc_client_pipe_path_accessor() {
        let client = WpProcIpcClient::new("my-wp-proc-socket");
        assert_eq!(client.pipe_path(), r"\\.\pipe\my-wp-proc-socket");
    }

    // ── I04: send_command_with_timeout 超时机制测试 ──────────────────────────
    //
    // I04 修复：play 命令的超时从 5s 提升到 15s（PLAY_COMMAND_TIMEOUT），
    // 并提取 send_command_with_timeout 允许测试传入较短超时验证机制。
    //
    // 以下测试通过真实命名管道服务端验证：
    // 1. 永不响应场景下，send_command_with_timeout 在 timeout 内返回 IpcTimeout
    // 2. 正常响应场景下，send_command_with_timeout 正确返回匹配的 WpProcResponse
    //
    // 测试基础设施与 tests/ipc_timeout.rs 一致：
    // - CreateNamedPipeW 创建服务端管道（windows 0.58 返回 HANDLE，失败时 is_invalid()）
    // - 主线程将 HANDLE 转为 std::fs::File（File: Send，可 move 到线程闭包）
    // - 不调用 ConnectNamedPipe：客户端 CreateFileW 连接时自动转为 connected 态

    use std::io::Write;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    // MirrorStarError 未在父模块 use 引入（父模块用 crate::MirrorStarError 全路径），
    // super::* 不会带入，需显式导入以在 matches! 中使用 IpcTimeout 变体。
    use crate::MirrorStarError;

    /// 全局原子计数器，用于生成唯一的管道名称（避免并行测试冲突）
    static I04_PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 生成唯一的命名管道名称
    fn i04_unique_pipe_name(suffix: &str) -> String {
        let n = I04_PIPE_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("mirrorstar-i04-{}-{}-{}", suffix, std::process::id(), n)
    }

    /// 创建命名管道服务端但永不写入数据（用于触发客户端读取超时）。
    ///
    /// HANDLE → File 转换以实现 `Send`，详见本函数内联注释。
    ///
    /// 返回 `(pipe_name, stop_flag, join_handle)`：
    /// - `pipe_name`：管道名称（不含 `\\.\pipe\` 前缀），传给 `WpProcIpcClient::new`
    /// - `stop_flag`：设置为 true 后服务端线程退出
    /// - `join_handle`：服务端线程的 JoinHandle
    ///
    /// # 实现说明
    /// `HANDLE`（windows 0.58）内部为 `*mut c_void`，未实现 `Send`，不能直接 move 到线程闭包。
    /// 此处在主线程上将 `HANDLE` 转换为 `std::fs::File`（`File` 实现了 `Send`），
    /// 再将 `File` move 到线程，避免在线程内构造 `HANDLE`。
    ///
    /// 不调用 `ConnectNamedPipe`：Windows 在客户端 `CreateFileW` 连接管道时自动将管道
    /// 从 "listening" 态转为 "connected" 态，服务端无需显式调用 `ConnectNamedPipe` 即可读写。
    fn spawn_silent_pipe_server(
        suffix: &str,
    ) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let pipe_name = i04_unique_pipe_name(suffix);
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        let wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

        // windows 0.58: CreateNamedPipeW 返回 HANDLE（非 Result），失败时返回无效句柄
        let server_handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,    // 最大实例数
                4096, // 输出缓冲区
                4096, // 输入缓冲区
                0,    // 默认超时
                None, // 默认安全属性
            )
        };
        assert!(
            !server_handle.is_invalid(),
            "CreateNamedPipeW 失败: {}",
            std::io::Error::last_os_error()
        );

        // 主线程完成 HANDLE → File 转换（File: Send，可安全 move 到线程）
        let server_file = unsafe { std::fs::File::from_raw_handle(server_handle.0 as RawHandle) };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let join_handle = std::thread::spawn(move || {
            // 持有服务端句柄但不写入任何数据，使客户端读取超时
            // 客户端连接后，管道自动从 listening 转为 connected
            while !stop_clone.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(server_file);
        });

        (pipe_name, stop, join_handle)
    }

    /// I04: send_command_with_timeout 应在服务端永不响应时于 timeout 内返回 IpcTimeout。
    ///
    /// 场景：服务端创建管道但不写入任何数据。
    /// - 修复前：send_command 硬编码 5s 超时，测试需等待 5s 才能验证超时行为。
    /// - 修复后：提取 send_command_with_timeout 允许传入较短超时（如 200ms），
    ///   在 200ms 内验证 IpcTimeout 返回，无需等待 15s 生产超时。
    #[test]
    fn i04_send_command_with_timeout_returns_ipc_timeout_when_server_silent() {
        let (pipe_name, stop, server_thread) = spawn_silent_pipe_server("timeout");

        let mut client = WpProcIpcClient::new(&pipe_name);
        client.connect(10, 50).expect("连接命名管道失败");

        // 使用短超时（200ms）验证超时机制，无需等待 15s
        let timeout = Duration::from_millis(200);
        let start = Instant::now();
        let result = client.send_command_with_timeout(
            WpProcCommand::Play {
                request_id: 1,
                source: "https://example.com".to_string(),
            },
            timeout,
        );
        let elapsed = start.elapsed();

        // 停止服务端线程
        stop.store(true, Ordering::SeqCst);
        client.disconnect();
        let _ = server_thread.join();

        // 验证返回 IpcTimeout（而非 IpcDisconnected 或其他错误）
        assert!(
            matches!(result, Err(MirrorStarError::IpcTimeout(_))),
            "I04: 应返回 IpcTimeout，实际: {:?}",
            result
        );

        // 验证总体时间不超过 timeout + 容差（轮询退避最大 100ms）
        let tolerance = Duration::from_millis(500);
        assert!(
            elapsed <= timeout + tolerance,
            "I04: 超时应在 {:?} 内，实际 {:?}（服务端静默不应导致长时间阻塞）",
            timeout + tolerance,
            elapsed
        );

        // 关键验证：不是等待了 15s（PLAY_COMMAND_TIMEOUT）
        // 若误用 send_command（硬编码 15s）而非 send_command_with_timeout，将等待 15s
        assert!(
            elapsed < Duration::from_secs(14),
            "I04: 超时应在 200ms 附近，实际 {:?}（疑似使用了 15s 默认超时）",
            elapsed
        );
    }

    /// I04 正常路径：服务端发送匹配 request_id 的响应时，send_command_with_timeout
    /// 应正常返回 Ok(WpProcResponse)，确保超时机制修复不影响正常路径。
    #[test]
    fn i04_send_command_with_timeout_returns_response_when_server_responds() {
        let pipe_name = i04_unique_pipe_name("normal");
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        let wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

        let server_handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        assert!(
            !server_handle.is_invalid(),
            "CreateNamedPipeW 失败: {}",
            std::io::Error::last_os_error()
        );

        let server_file = unsafe { std::fs::File::from_raw_handle(server_handle.0 as RawHandle) };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let server_thread = std::thread::spawn(move || {
            // 服务端在客户端连接后发送匹配 request_id=1 的 Ok 响应
            // 客户端连接前的写入会失败（ERROR_PIPE_NOT_CONNECTED），忽略错误继续重试
            // 注意：写入成功后不能立即退出——退出会 drop server_file 关闭管道，
            // 导致客户端后续 send_line 写入命令时收到 BrokenPipe（ERROR_PIPE_NOT_CONNECTED）。
            // 因此写入成功后保持线程存活，等待 stop_flag 被设置后再退出。
            let mut server_file = server_file;
            let response = r#"{"request_id":1,"status":"ok"}"#;
            let mut written = false;
            while !stop_clone.load(Ordering::SeqCst) {
                if !written
                    && server_file
                        .write_all(format!("{}\n", response).as_bytes())
                        .is_ok()
                {
                    let _ = server_file.flush();
                    written = true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(server_file);
        });

        let mut client = WpProcIpcClient::new(&pipe_name);
        client.connect(10, 50).expect("连接命名管道失败");

        // 使用较长超时（2s），正常路径应立即返回数据而非超时
        let timeout = Duration::from_secs(2);
        let start = Instant::now();
        let result = client.send_command_with_timeout(
            WpProcCommand::Play {
                request_id: 1,
                source: "https://example.com".to_string(),
            },
            timeout,
        );
        let elapsed = start.elapsed();

        stop.store(true, Ordering::SeqCst);
        client.disconnect();
        let _ = server_thread.join();

        // 验证正常返回 Ok(WpProcResponse)
        assert!(
            result.is_ok(),
            "I04 正常路径: 应返回 Ok，实际: {:?}",
            result
        );
        let resp = result.unwrap();
        assert_eq!(resp.request_id, 1);
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.error, None);

        // 验证响应迅速（远小于超时）
        assert!(
            elapsed < Duration::from_millis(1000),
            "I04 正常路径: 应在 1s 内返回，实际 {:?}",
            elapsed
        );
    }

    /// I04: send_command（无超时参数版本）应委托给 send_command_with_timeout
    /// 并使用 PLAY_COMMAND_TIMEOUT（15s）。验证常量值确保生产路径超时为 15s。
    ///
    /// 此测试不实际等待 15s，仅验证常量值（通过代码审查 + 常量断言确认委托关系）。
    #[test]
    fn i04_play_command_timeout_is_15_seconds() {
        // PLAY_COMMAND_TIMEOUT 为 15s，与 connect 的 20s 兜底匹配，
        // 覆盖 WebView2 初始化（进程启动 + 运行时下载等）的慢速场景。
        assert_eq!(PLAY_COMMAND_TIMEOUT, Duration::from_secs(15));
    }
}
