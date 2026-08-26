//! Integration tests for I01/I03 总体超时 fixes.
//!
//! These tests verify that the overall timeout is enforced when the remote
//! sends periodic empty lines (I01) or non-matching responses/events (I03),
//! preventing infinite blocking that would occur if each individual line read
//! succeeds within the per-iteration timeout but the total elapsed time
//! exceeds the intended budget.

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

use mirrorstar_core::ipc::client::NamedPipeClient;
use mirrorstar_core::ipc::mpv_protocol::MpvIpcClient;
use mirrorstar_core::MirrorStarError;

/// 全局原子计数器，用于生成唯一的管道名称（避免并行测试冲突）
static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成唯一的命名管道名称
fn unique_pipe_name(suffix: &str) -> String {
    let n = PIPE_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("mirrorstar-test-{}-{}-{}", suffix, std::process::id(), n)
}

/// 创建命名管道服务端并周期性写入数据。
///
/// 在客户端连接后，每隔 `interval` 写入一次 `payload`，直到 `stop_flag` 被设置为 true。
///
/// 返回 `(pipe_name, stop_flag, join_handle)`：
/// - `pipe_name`：管道名称（不含 `\\.\pipe\` 前缀），传给 `NamedPipeClient::new`
/// - `stop_flag`：设置为 true 后服务端线程停止写入并退出
/// - `join_handle`：服务端线程的 JoinHandle
///
/// # 实现说明
/// `HANDLE`（windows 0.58）内部为 `*mut c_void`，未实现 `Send`，不能直接 move 到线程闭包。
/// 此处在主线程上将 `HANDLE` 转换为 `std::fs::File`（`File` 实现了 `Send`），
/// 再将 `File` move 到线程，避免在线程内构造 `HANDLE`。
///
/// 不调用 `ConnectNamedPipe`：Windows 在客户端 `CreateFileW` 连接管道时自动将管道
/// 从 "listening" 态转为 "connected" 态，服务端无需显式调用 `ConnectNamedPipe` 即可读写。
/// 客户端连接前的写入会失败（`ERROR_PIPE_NOT_CONNECTED`），通过 `let _ =` 忽略，
/// 线程持续重试直到客户端连接后写入成功。
fn spawn_periodic_pipe_server(
    suffix: &str,
    payload: Vec<u8>,
    interval: Duration,
) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let pipe_name = unique_pipe_name(suffix);
    let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
    let wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

    // 创建命名管道（服务端）
    // windows 0.58 中 CreateNamedPipeW 返回 HANDLE（非 Result），
    // 失败时返回无效句柄，通过 is_invalid() 判断
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

    // 在主线程上将 HANDLE 转为 std::fs::File（File 实现了 Send，可安全 move 到线程）
    // HANDLE 内部为 *mut c_void 未实现 Send，通过主线程完成 HANDLE → File 转换，
    // 避免在线程闭包内构造 HANDLE 导致 "*mut c_void cannot be sent between threads" 编译错误。
    // File 取得句柄所有权，drop 时关闭。
    let server_file = unsafe { std::fs::File::from_raw_handle(server_handle.0 as RawHandle) };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let join_handle = std::thread::spawn(move || {
        let mut file = server_file;
        // 周期性写入 payload，直到 stop_flag 被设置
        // 客户端连接前的写入会失败（ERROR_PIPE_NOT_CONNECTED），忽略错误继续重试
        while !stop_clone.load(Ordering::SeqCst) {
            let _ = file.write_all(&payload);
            let _ = file.flush();
            std::thread::sleep(interval);
        }
        // file 被 drop 时关闭服务端句柄
        drop(file);
    });

    (pipe_name, stop, join_handle)
}

/// I01: `read_response_line_with_timeout` 应在总体超时后返回 `IpcTimeout`，
/// 而非因远端周期性发送空行导致无限阻塞。
///
/// 场景：服务端每 20ms 发送一个空行（仅换行符）。
/// - 修复前：每次 `read_line_with_timeout(timeout)` 调用都在 ~20ms 内返回（读到空行），
///   外层循环跳过空行后继续，每次都用完整 timeout，导致无限阻塞。
/// - 修复后：deadline 在循环外固定，每轮检查是否超时，总体时间受 timeout 约束。
#[test]
fn read_response_line_with_timeout_overall_deadline_i01() {
    // 服务端每 20ms 发送一个空行（仅换行符）
    let (pipe_name, stop, server_thread) =
        spawn_periodic_pipe_server("i01", b"\n".to_vec(), Duration::from_millis(20));

    // 客户端连接（重试以等待服务端创建管道）
    let mut client: NamedPipeClient<()> = NamedPipeClient::new(&pipe_name);
    client.connect(10, 50).expect("连接命名管道失败");

    // 调用 read_response_line_with_timeout，使用短超时
    let timeout = Duration::from_millis(300);
    let start = Instant::now();
    let result = client.read_response_line_with_timeout(timeout);
    let elapsed = start.elapsed();

    // 停止服务端线程
    stop.store(true, Ordering::SeqCst);
    client.disconnect();
    let _ = server_thread.join();

    // 验证返回 IpcTimeout（而非无限阻塞或 IpcDisconnected）
    assert!(
        matches!(result, Err(MirrorStarError::IpcTimeout(_))),
        "I01: 应返回 IpcTimeout，实际: {:?}",
        result
    );

    // 验证总体时间不超过 timeout + 容差
    // 容差考虑线程调度延迟和 read_line_with_timeout 的退避轮询（最大 100ms）
    let tolerance = Duration::from_millis(500);
    assert!(
        elapsed <= timeout + tolerance,
        "I01: 总体超时应在 {:?} 内，实际 {:?}（远端周期性发送空行不应导致无限阻塞）",
        timeout + tolerance,
        elapsed
    );
}

/// I03: `send_command_with_timeout` 应在总体超时后返回 `IpcTimeout`，
/// 而非因 mpv 持续发送非匹配响应（事件）导致无限阻塞。
///
/// 场景：服务端每 20ms 发送一个 mpv 事件（property-change），不包含匹配的 request_id。
/// - 修复前：每次 `read_response_line_with_timeout(timeout)` 调用都在 ~20ms 内返回（读到事件），
///   外层循环因 request_id 不匹配而继续，每次都用完整 timeout，导致无限阻塞。
/// - 修复后：deadline 在循环外固定，每轮将剩余时间传给 `read_response_line_with_timeout`，
///   剩余为 0 时返回 `IpcTimeout`。
#[test]
fn send_command_with_timeout_overall_deadline_i03() {
    // 服务端每 20ms 发送一个 mpv 事件（非匹配 request_id 的响应）
    let event = b"{\"event\":\"property-change\",\"name\":\"volume\",\"data\":50}\n".to_vec();
    let (pipe_name, stop, server_thread) =
        spawn_periodic_pipe_server("i03", event, Duration::from_millis(20));

    // 客户端连接
    let mut client = MpvIpcClient::new(&pipe_name);
    client.connect(10, 50).expect("连接命名管道失败");

    // 调用 send_command_with_timeout，使用短超时
    let timeout = Duration::from_millis(300);
    let start = Instant::now();
    let result = client.send_command_with_timeout(&["get_property", "volume"], timeout);
    let elapsed = start.elapsed();

    // 停止服务端线程
    stop.store(true, Ordering::SeqCst);
    client.disconnect();
    let _ = server_thread.join();

    // 验证返回 IpcTimeout
    assert!(
        matches!(result, Err(MirrorStarError::IpcTimeout(_))),
        "I03: 应返回 IpcTimeout，实际: {:?}",
        result
    );

    // 验证总体时间不超过 timeout + 容差
    let tolerance = Duration::from_millis(500);
    assert!(
        elapsed <= timeout + tolerance,
        "I03: 总体超时应在 {:?} 内，实际 {:?}（mpv 持续发送事件不应导致无限阻塞）",
        timeout + tolerance,
        elapsed
    );
}

/// I01 边界验证：当远端发送非空行时，`read_response_line_with_timeout` 应正常返回内容，
/// 而非因 deadline 检查过早超时。确保总体超时修复不影响正常路径。
#[test]
fn read_response_line_with_timeout_returns_data_when_available() {
    // 服务端立即发送一行有效数据（非空行）
    let (pipe_name, stop, server_thread) = spawn_periodic_pipe_server(
        "i01-normal",
        b"{\"error\":\"success\",\"data\":42,\"request_id\":1}\n".to_vec(),
        Duration::from_millis(10),
    );

    let mut client: NamedPipeClient<()> = NamedPipeClient::new(&pipe_name);
    client.connect(10, 50).expect("连接命名管道失败");

    // 使用较长超时（1s），应立即返回数据而非超时
    let timeout = Duration::from_secs(1);
    let start = Instant::now();
    let result = client.read_response_line_with_timeout(timeout);
    let elapsed = start.elapsed();

    stop.store(true, Ordering::SeqCst);
    client.disconnect();
    let _ = server_thread.join();

    // 验证正常返回数据
    assert!(
        result.is_ok(),
        "I01 正常路径: 应返回数据，实际: {:?}",
        result
    );
    let line = result.unwrap();
    assert!(line.contains("success"), "返回内容应包含 success");

    // 验证响应迅速（远小于超时）
    assert!(
        elapsed < Duration::from_millis(500),
        "I01 正常路径: 应在 500ms 内返回，实际 {:?}",
        elapsed
    );
}
