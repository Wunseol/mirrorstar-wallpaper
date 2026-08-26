//! # Blocking API 标注
//!
//! **ALL METHODS ON NamedPipeClient AND FREE FUNCTIONS IN THIS MODULE ARE BLOCKING.**
//!
//! 本模块所有公共方法均使用同步阻塞 I/O（`std::thread::sleep`、`WaitForSingleObject`、
//! `BufReader::read`），不得在 tokio async 上下文中直接调用。调用方必须通过
//! `tokio::task::spawn_blocking` 包裹，否则会阻塞 tokio worker 线程导致 runtime 卡顿。
//!
//! 超时对照表见 `ipc/mod.rs`。

//! 命名管道 IPC 客户端通用基础设施
//!
//! 提供 `NamedPipeClient<T>` 泛型基类，封装 Windows 命名管道客户端的
//! 共享状态（pipe_path/writer/reader/request_id）与通用行为（连接/断开/读写）。
//! 具体的协议客户端（如 `MpvIpcClient`、`WpProcIpcClient`）以薄封装形式基于此基类
//! 实现协议特定的命令构造与响应解析逻辑，从而消除 DRY 违反。

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::PeekNamedPipe;

use crate::MirrorStarError;

/// 命名管道客户端泛型基类
///
/// 封装 mpv / wp-proc 等命名管道 IPC 客户端共享的状态与行为：
/// - `pipe_path`：管道完整路径（`\\.\pipe\<name>`）
/// - `writer` / `reader`：连接后的带缓冲读写端
/// - `request_id`：自增请求 ID 计数器
///
/// 类型参数 `T` 为各协议的命令类型标记，用于在类型层面区分不同协议的客户端，
/// 避免误用。具体客户端通过薄封装（newtype）持有 `NamedPipeClient<T>` 并委托调用。
pub struct NamedPipeClient<T> {
    /// 命名管道路径
    pipe_path: String,
    /// 写入端（带缓冲）
    writer: Option<BufWriter<std::fs::File>>,
    /// 读取端（带缓冲）
    reader: Option<BufReader<std::fs::File>>,
    /// 请求 ID 计数器
    request_id: AtomicU64,
    _phantom: PhantomData<T>,
}

impl<T> NamedPipeClient<T> {
    /// 创建新的 IPC 客户端
    pub fn new(pipe_name: &str) -> Self {
        Self {
            pipe_path: format!(r"\\.\pipe\{}", pipe_name),
            writer: None,
            reader: None,
            request_id: AtomicU64::new(1),
            _phantom: PhantomData,
        }
    }

    /// 连接到命名管道
    /// 重试最多 retry_count 次，每次间隔 retry_interval_ms 毫秒
    pub fn connect(
        &mut self,
        retry_count: u32,
        retry_interval_ms: u64,
    ) -> Result<(), MirrorStarError> {
        let (writer, reader) = connect_named_pipe(&self.pipe_path, retry_count, retry_interval_ms)?;
        self.writer = Some(writer);
        self.reader = Some(reader);
        Ok(())
    }

    /// 断开连接（关闭读写端）
    ///
    /// 仅做资源清理，不记录日志；具体客户端的封装负责在断开后记录协议特定的日志。
    /// 该方法幂等，重复调用安全（内部使用 `take`）。
    pub fn disconnect(&mut self) {
        // 关闭写入端
        if let Some(mut writer) = self.writer.take() {
            if let Err(e) = writer.flush() {
                tracing::warn!(error = %e, "断开连接时 flush 写入端失败");
            }
        }
        // 关闭读取端
        drop(self.reader.take());
    }

    /// 获取管道路径
    pub fn pipe_path(&self) -> &str {
        &self.pipe_path
    }

    /// 分配下一个请求 ID（自增）
    pub fn next_request_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// 写入一行 JSON 命令（自动追加换行符并 flush）
    ///
    /// 分两次 write_all（json + `\n`）避免 format! 分配。
    /// BufWriter 会缓冲两次写入，最终一次 flush，无额外堆分配。
    pub fn send_line(&mut self, json: &str) -> Result<(), MirrorStarError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| MirrorStarError::IpcNotConnected("IPC 未连接".to_string()))?;
        writer.write_all(json.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// 读取一行响应（自定义超时，跳过空行），返回去除首尾空白后的内容
    ///
    /// 用于 get_property 等需要同步等待响应的场景下缩短超时（如 2s），
    /// 避免 mpv 端处理延迟时长时间阻塞 UI 线程。
    ///
    /// 管道已关闭或读取超时返回 `Err`。非匹配/不可解析的行由调用方在循环中处理。
    ///
    /// I01 总体超时：`timeout` 是整个调用（含跳过空行循环）的总预算，
    /// 而非每行单次读取的超时。循环外记录 deadline，每轮检查是否已超时，
    /// 防止远端周期性发送空行（虽每行读取在预算内，但累计可无限阻塞）。
    ///
    /// # Blocking
    ///
    /// 本方法内部调用 `read_line_with_timeout`，阻塞当前线程直到收到响应或超时。
    /// 在 async 上下文中调用时必须通过 `tokio::task::spawn_blocking` 包裹。
    pub fn read_response_line_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<String, MirrorStarError> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| MirrorStarError::IpcNotConnected("IPC 未连接".to_string()))?;
        // I01 总体超时：deadline 为整个调用（含跳过空行循环）的硬性截止时刻，
        // 防止远端周期性发送空行导致单次 read_line_with_timeout 不超时但累计无限阻塞。
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // 每轮检查是否已超出总体 deadline，超出即返回 IpcTimeout
            if std::time::Instant::now() >= deadline {
                return Err(MirrorStarError::IpcTimeout("IPC 读取超时".to_string()));
            }
            // 剩余预算传给单行读取：min(timeout, 剩余) 防止单行读取超出 deadline
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let line = read_line_with_timeout(reader, remaining)?;
            if line.is_empty() {
                return Err(MirrorStarError::IpcDisconnected("管道已关闭".to_string()));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            return Ok(trimmed.to_string());
        }
    }
}

impl<T> Drop for NamedPipeClient<T> {
    fn drop(&mut self) {
        self.disconnect();
    }
}

// ── 命名管道连接与读取辅助函数 ─────────────────────────────────────────────

/// 连接到命名管道（客户端）
///
/// 重试最多 `retry_count` 次，每次间隔 `retry_interval_ms` 毫秒。
/// 返回带缓冲的读写端（写入端基于原始句柄，读取端基于克隆句柄）。
///
/// I05 阻塞提示：本方法使用 `std::thread::sleep` 进行重试等待，会阻塞当前线程
/// 最长 `retry_count * retry_interval_ms` 毫秒。在 async 上下文（如 tokio runtime）
/// 中调用时，必须通过 `tokio::task::spawn_blocking` 包裹，避免阻塞 tokio worker
/// 线程导致整个 runtime 卡顿。当前调用方（`NamedPipeClient::connect`）已遵循此约定，
/// 顶层均在 `spawn_blocking` 闭包中调用。
///
/// # Blocking
///
/// 本方法使用 `std::thread::sleep` 阻塞当前线程，最长阻塞 `retry_count * retry_interval_ms` 毫秒。
/// 在 async 上下文中调用时必须通过 `tokio::task::spawn_blocking` 包裹。
pub(crate) fn connect_named_pipe(
    pipe_path: &str,
    retry_count: u32,
    retry_interval_ms: u64,
) -> Result<(BufWriter<std::fs::File>, BufReader<std::fs::File>), MirrorStarError> {
    let mut attempts = 0u32;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_path)
        {
            Ok(file) => {
                let reader_file = file.try_clone().map_err(|e| {
                    MirrorStarError::IpcError(format!("克隆管道文件句柄失败: {}", e))
                })?;
                return Ok((BufWriter::new(file), BufReader::new(reader_file)));
            }
            Err(e) => {
                attempts += 1;
                if attempts > retry_count {
                    return Err(MirrorStarError::IpcError(format!(
                        "连接命名管道失败 (已重试 {} 次): {} (path={})",
                        retry_count, e, pipe_path
                    )));
                }
                tracing::warn!(attempts, error = %e, "命名管道未就绪，等待重试");
                std::thread::sleep(Duration::from_millis(retry_interval_ms));
            }
        }
    }
}

/// 单行最大字节数（SEC-003：防止 OOM 攻击）
const MAX_LINE_BYTES: usize = 1024 * 1024; // 1MB

/// 轮询初始间隔（C-113：指数退避起点）
const POLL_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
/// 轮询最大间隔（C-113：指数退避上限，避免响应延迟过大）
const POLL_MAX_BACKOFF: Duration = Duration::from_millis(100);

/// 指数退避辅助结构（C-113）
///
/// 在轮询循环中逐步增加休眠间隔，避免长时间无数据时持续高频轮询消耗 CPU。
/// 每次调用 [`next_delay`](Self::next_delay) 返回当前退避值并将下一次的值翻倍（上限为 `max`）。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// 最大退避值（含）
    max: Duration,
    /// 当前退避值
    current: Duration,
}

impl Backoff {
    /// 创建新的指数退避器
    ///
    /// - `initial`：首次休眠时长（必须 > 0）
    /// - `max`：休眠时长上限（必须 >= `initial`）
    ///
    /// # Panics
    /// 若 `initial` 为零，或 `max < initial`，则 panic（编程错误）。
    pub fn new(initial: Duration, max: Duration) -> Self {
        assert!(!initial.is_zero(), "Backoff::initial 必须大于零");
        assert!(max >= initial, "Backoff::max 不能小于 initial");
        Self {
            max,
            current: initial,
        }
    }

    /// 返回当前退避值，并将下一次的退避值翻倍（受 `max` 限制）
    pub fn next_delay(&mut self) -> Duration {
        let cur = self.current;
        // 翻倍，并以 max 为上限；用 saturating_mul 防止溢出
        let doubled = cur.saturating_mul(2);
        self.current = doubled.min(self.max);
        cur
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(POLL_INITIAL_BACKOFF, POLL_MAX_BACKOFF)
    }
}

/// 从带缓冲的读取端读取一行（带字节上限，SEC-003 / I02 防止 OOM；I-001 防止 fill_buf 阻塞）
///
/// 设计理由（OOM 防护 / UTF-8 截断处理）见 v4.0 finding 文档与下方各 `I-001`/`I02`/`v41-I-001` 段落。
///
/// 使用 `BufRead::fill_buf` 增量填充内部缓冲区（默认 8KB），每次填充后检查累计长度：
/// - 在累计字节数达到 `max_bytes` 之前遇到 `\n` → 返回含换行符的完整行
/// - 累计长度超过 `max_bytes` 仍未遇到 `\n` → 立即返回 `IpcError`，避免内存耗尽
/// - EOF 时仍未遇到 `\n` → 返回已读取的内容（与 `BufReader::read_line` 行为一致）
/// - **I-001**：累计消费字节数达到 `total_avail` 仍未遇到 `\n` → 主动返回当前已读取的
///   部分（不含 `\n`），将控制流交还外层调用方重新 peek + 检查 deadline，避免在
///   `PIPE_WAIT` 阻塞模式下 `fill_buf` → `File::read` 无限阻塞
/// - **v41-I-001**：在 `total_avail` 边界处额外检查 UTF-8 完整性。若 `total_avail`
///   恰在多字节 UTF-8 字符中间截断（`Utf8Error::error_len() == None`），允许继续
///   `fill_buf` 读取后续字节（受 `max_bytes` 硬上限约束，多字节字符最多 4 字节，
///   截断时最多再读 3 字节），避免外层 `String::from_utf8` 失败误判为协议错误。
///   若为无效字节（`error_len() == Some(_)`），直接返回 `IpcError`。
///
/// 与 `read_line` / `read_until` 的关键区别：后者会在遇到 `\n` 前持续分配内存，
/// 恶意端发送超大无换行数据可导致 OOM；本函数通过 `fill_buf` + `consume` 精确控制
/// 每次读取量（受 BufReader 内部缓冲区容量限制），并在复制前检查累计长度，
/// 使 `buf` 内存占用有确定性硬上限（`max_bytes`）。
///
/// 返回原始字节 `Vec<u8>` 而非 `String`：I-001 修复后，`total_avail` 限制可能在
/// 多字节 UTF-8 字符边界处截断，此时 `String::from_utf8` 会失败。UTF-8 解码由
/// 外层调用方（[`read_line_with_timeout`]）在累积完整行后统一执行。
///
/// 返回的 `Vec<u8>` 保留尾部 `\n`（与 `read_line` 一致），由调用方按需处理。
///
/// 泛型 `R: BufRead` 使其可用 `std::io::Cursor` 进行单元测试，无需真实命名管道。
pub fn read_line_with_limit<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
    total_avail: usize,
) -> Result<Vec<u8>, MirrorStarError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut consumed: usize = 0;
    loop {
        // I-001：已消费量达到 peek 报告的可用量时，主动返回控制流给外层，
        // 避免在 PIPE_WAIT 模式下 fill_buf → File::read 无限阻塞。
        // 外层（read_line_with_timeout）检查返回内容是否含 \n：
        //   - 含 \n → 返回完整行
        //   - 不含 \n → 累积后重新 peek + 检查 deadline
        if consumed >= total_avail {
            // v41-I-001：在 total_avail 边界处检查 UTF-8 完整性，区分截断 vs 无效字节：
            //   - 截断（error_len() == None）：多字节字符在边界处被切断，继续 fill_buf
            //     读取后续字节（受 max_bytes 硬上限约束），避免外层 from_utf8 失败误判。
            //     多字节 UTF-8 字符最多 4 字节，截断时最多再读 3 字节即可构成完整字符。
            //   - 无效字节（error_len() == Some(_)）：输入流含非法 UTF-8 字节，直接返回
            //     IpcError，避免外层累积后仍失败。
            //   - Ok(_)：纯 ASCII 或完整 UTF-8，正常返回。
            match std::str::from_utf8(&buf) {
                Ok(_) => break,
                Err(e) => match e.error_len() {
                    None => {
                        // 截断：继续 fill_buf 读取后续字节（不 break，进入下方 fill_buf 分支）。
                        // 截断模式下 consumed >= total_avail 始终成立，下方 chunk_len 计算
                        // 会切换到"读取全部 available"模式（仍受 max_bytes 硬上限约束）。
                    }
                    Some(_) => {
                        return Err(MirrorStarError::IpcError(format!(
                            "IPC 响应非有效 UTF-8: {}",
                            e
                        )));
                    }
                },
            }
        }
        // fill_buf 返回内部缓冲区的不可变借用，而 consume 需要 &mut self。
        // 用块作用域在调用 consume 前结束 fill_buf 的借用，避免借用冲突。
        let (consume_len, found_newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // EOF：返回已读取的内容（可能为空 Vec）
                break;
            }
            let nl_pos = available.iter().position(|&b| b == b'\n');
            // I-001：未找到 \n 时，将单次消费量限制为剩余可用量（total_avail - consumed），
            // 确保 consumed 精确达到 total_avail 后即返回控制流，避免 fill_buf 返回的块
            // 大于 peek 报告量时过度消费（InfiniteByteStream / 大缓冲 BufReader 场景）。
            // 找到 \n 时不过度限制：行终止符意味着读取完成，无后续 fill_buf 阻塞风险。
            // v41-I-001：截断模式下（consumed >= total_avail），remaining_avail 为 0，
            // 此时不再限制 chunk_len（读取全部 available），以便构成完整 UTF-8 字符。
            let remaining_avail = total_avail.saturating_sub(consumed);
            let chunk_len = match nl_pos {
                Some(pos) => pos + 1, // 含 \n
                None => {
                    if consumed >= total_avail {
                        // v41-I-001 截断模式：读取全部可用数据以构成完整 UTF-8 字符
                        available.len()
                    } else {
                        available.len().min(remaining_avail)
                    }
                }
            };
            // I02 核心（check before copy）：在复制前检查累计长度，
            // 确保 buf 内存占用永不超过 max_bytes，即使 fill_buf 返回超大块。
            if buf.len() + chunk_len > max_bytes {
                return Err(MirrorStarError::IpcError(format!(
                    "IPC 单行超过 {} 字节上限（当前 {} 字节）",
                    max_bytes,
                    buf.len() + chunk_len
                )));
            }
            buf.extend_from_slice(&available[..chunk_len]);
            (chunk_len, nl_pos.is_some())
        };
        reader.consume(consume_len);
        consumed += consume_len;
        if found_newline {
            break;
        }
    }
    Ok(buf)
}

/// 从带缓冲的管道读取端读取一行（带超时）
///
/// 使用 `PeekNamedPipe` 轮询可用数据，避免无限阻塞：
/// - 有数据可读时委托 [`read_line_with_limit`] 读取最多 `total_avail` 字节
///   （I-001：限制 fill_buf 消费量，避免 PIPE_WAIT 阻塞）
/// - 返回的片段含 `\n` → 拼接已累积部分并返回完整行
/// - 返回的片段不含 `\n` → 累积到 `partial`，检查 deadline 后重新 peek
/// - 无数据时按指数退避轮询（10ms 起，每次翻倍，上限 100ms），超过 `timeout` 返回超时错误
/// - 管道已关闭（EOF / PeekNamedPipe 失败）时返回空字符串，由调用方判断
/// - 单行超过 [`MAX_LINE_BYTES`]（1MB）时返回 `IpcError`，防止恶意端写入超大行导致 OOM（I02）
///
/// **OOM 防护（I02）**：原实现使用 `read_line` 读取整行后才检查长度，恶意端可发送
/// 超长无换行数据导致内存先被耗尽再被拒绝。现通过 [`read_line_with_limit`] 使用
/// `fill_buf` + `consume` 增量读取，每次填充后检查累计长度，使内存占用有硬上限。
///
/// **I-001 超时盲区修复**：原实现将整行读取委托给 `read_line_with_limit`，后者在
/// `fill_buf` 缓冲区耗尽后会调用 `File::read`，对 `PIPE_WAIT` 管道会无限阻塞，
/// 导致外层 deadline 检查永不执行。现传入 `total_avail`（= BufReader 缓冲残留 +
/// `PeekNamedPipe` 报告的管道可用量），限制单次 `read_line_with_limit` 消费量，
/// 消费完毕即返回控制流，由本函数重新 peek + 检查 deadline，消除阻塞盲区。
///
/// # Blocking
///
/// 本方法使用 `PeekNamedPipe` 轮询 + `std::thread::sleep` 退避，阻塞当前线程直到读取完整行或超时。
/// 在 async 上下文中调用时必须通过 `tokio::task::spawn_blocking` 包裹。
pub fn read_line_with_timeout(
    reader: &mut BufReader<std::fs::File>,
    timeout: Duration,
) -> Result<String, MirrorStarError> {
    use std::os::windows::io::AsRawHandle;

    let handle = HANDLE(reader.get_ref().as_raw_handle());
    let deadline = std::time::Instant::now() + timeout;
    let mut backoff = Backoff::default();
    // I-001：累积部分行字节（跨多次 peek-read 循环拼接），延迟到完整行时再做 UTF-8
    // 解码，避免部分行在多字节字符边界处截断导致 String::from_utf8 失败。
    let mut partial: Vec<u8> = Vec::new();

    loop {
        // I-001：将 BufReader 内部缓冲区已有数据计入 total_avail，
        // 避免缓冲区有残留数据但 PeekNamedPipe 报告 0 时跳过读取。
        let buffered_len = reader.buffer().len();
        let mut peeked: u32 = 0;
        let peeked_result =
            unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut peeked), None) };
        match peeked_result {
            Ok(()) => {
                let total_avail = buffered_len + peeked as usize;
                if total_avail > 0 {
                    // I-001：传入 total_avail 限制 fill_buf 消费量，避免 PIPE_WAIT 阻塞
                    // I02：max_bytes 限制单行总长度（含已累积 partial），防止 OOM
                    let max_remaining = MAX_LINE_BYTES.saturating_sub(partial.len());
                    if max_remaining == 0 {
                        return Err(MirrorStarError::IpcError(format!(
                            "IPC 单行超过 {} 字节上限",
                            MAX_LINE_BYTES
                        )));
                    }
                    let chunk = read_line_with_limit(reader, max_remaining, total_avail)?;
                    if chunk.is_empty() {
                        // EOF：管道已关闭。若 partial 非空，返回已累积的部分（无 \n），
                        // 与原 read_line_with_limit 在 EOF 时返回已读取内容的行为一致。
                        if partial.is_empty() {
                            return Ok(String::new());
                        }
                        return String::from_utf8(partial).map_err(|e| {
                            MirrorStarError::IpcError(format!("IPC 响应非有效 UTF-8: {}", e))
                        });
                    }
                    // read_line_with_limit 遇到 \n 时返回的片段必以 \n 结尾（无后续字节）
                    if chunk.ends_with(b"\n") {
                        // 含 \n：拼接 partial + chunk 并返回完整行
                        partial.extend_from_slice(&chunk);
                        return String::from_utf8(partial).map_err(|e| {
                            MirrorStarError::IpcError(format!("IPC 响应非有效 UTF-8: {}", e))
                        });
                    } else {
                        // 不含 \n：累积部分，继续 peek + 检查 deadline
                        partial.extend_from_slice(&chunk);
                    }
                }
                // total_avail == 0：暂无数据，继续轮询
            }
            Err(e) => {
                // I-002: 区分 EOF（ERROR_BROKEN_PIPE/ERROR_NO_DATA）与真实错误。
                // EOF 语义：管道正常关闭，返回空串（与原行为一致）。
                // 其他错误：记录 Windows 错误码并返回 IpcError，避免误导调用方。
                use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA};
                let code = e.code();
                // HRESULT 中 FACILITY_WIN32 错误的 Win32 错误码位于低 16 位
                // （参考 wp-proc/ipc_server.rs 中 ConnectNamedPipe 的同类处理）
                let win32_code = (code.0 as u32) & 0xFFFF;
                let is_eof = win32_code == ERROR_BROKEN_PIPE.0 || win32_code == ERROR_NO_DATA.0;
                if is_eof {
                    // 管道正常关闭（EOF）
                    if partial.is_empty() {
                        return Ok(String::new());
                    }
                    return String::from_utf8(partial).map_err(|e| {
                        MirrorStarError::IpcError(format!("IPC 响应非有效 UTF-8: {}", e))
                    });
                }
                // 非 EOF 错误：记录 Windows 错误码并返回 IpcError
                tracing::debug!(
                    error = ?e,
                    error_code = code.0,
                    win32_code,
                    "PeekNamedPipe 失败（非 EOF）"
                );
                return Err(MirrorStarError::IpcError(format!(
                    "PeekNamedPipe 失败: {} (code={})",
                    e, code.0
                )));
            }
        }

        if std::time::Instant::now() >= deadline {
            return Err(MirrorStarError::IpcTimeout("IPC 读取超时".to_string()));
        }
        // C-113：使用指数退避替代固定 10ms 轮询
        std::thread::sleep(backoff.next_delay());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_pipe_client_new_pipe_path() {
        let client: NamedPipeClient<()> = NamedPipeClient::new("test-pipe");
        assert_eq!(client.pipe_path(), r"\\.\pipe\test-pipe");
    }

    #[test]
    fn named_pipe_client_pipe_path_accessor() {
        let client: NamedPipeClient<()> = NamedPipeClient::new("my-mpv-socket");
        assert_eq!(client.pipe_path(), r"\\.\pipe\my-mpv-socket");
    }

    #[test]
    fn named_pipe_client_next_request_id_increments() {
        let client: NamedPipeClient<()> = NamedPipeClient::new("test-pipe");
        assert_eq!(client.next_request_id(), 1);
        assert_eq!(client.next_request_id(), 2);
        assert_eq!(client.next_request_id(), 3);
    }

    #[test]
    fn named_pipe_client_disconnect_when_not_connected_is_safe() {
        let mut client: NamedPipeClient<()> = NamedPipeClient::new("test-pipe");
        // 未连接时 disconnect 应安全无 panic
        client.disconnect();
        client.disconnect();
    }

    // ── Backoff（C-113 指数退避）单元测试 ──────────────────────────────────

    #[test]
    fn backoff_next_delay_returns_current_and_advances() {
        let mut b = Backoff::default();
        // 序列：10 → 20 → 40 → 80 → 100（封顶） → 100 …
        assert_eq!(b.next_delay(), Duration::from_millis(10));
        assert_eq!(b.next_delay(), Duration::from_millis(20));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(80));
        assert_eq!(b.next_delay(), Duration::from_millis(100)); // 触顶
        assert_eq!(b.next_delay(), Duration::from_millis(100)); // 保持上限
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn backoff_never_exceeds_max() {
        let mut b = Backoff::new(Duration::from_millis(15), Duration::from_millis(50));
        for _ in 0..20 {
            let v = b.next_delay();
            assert!(
                v <= Duration::from_millis(50),
                "退避值 {:?} 超过上限 50ms",
                v
            );
        }
    }

    #[test]
    fn backoff_custom_initial_and_max() {
        let mut b = Backoff::new(Duration::from_millis(5), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(5));
        assert_eq!(b.next_delay(), Duration::from_millis(10));
        assert_eq!(b.next_delay(), Duration::from_millis(20));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(80));
        assert_eq!(b.next_delay(), Duration::from_millis(160));
        assert_eq!(b.next_delay(), Duration::from_millis(200)); // 触顶
        assert_eq!(b.next_delay(), Duration::from_millis(200));
    }

    #[test]
    fn backoff_equal_initial_and_max_stays_constant() {
        // initial == max 时退避值应恒等于该值
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(100));
        for _ in 0..5 {
            assert_eq!(b.next_delay(), Duration::from_millis(100));
        }
    }

    #[test]
    #[should_panic(expected = "initial 必须大于零")]
    fn backoff_new_panics_on_zero_initial() {
        let _ = Backoff::new(Duration::ZERO, Duration::from_millis(100));
    }

    #[test]
    #[should_panic(expected = "max 不能小于 initial")]
    fn backoff_new_panics_when_max_lt_initial() {
        let _ = Backoff::new(Duration::from_millis(100), Duration::from_millis(10));
    }

    // ── read_line_with_limit（I02 OOM 防护）单元测试 ───────────────────────
    //
    // 通过 `Cursor<Vec<u8>>` 模拟管道读取，验证 `fill_buf` + `consume` 增量读取
    // 在每次填充后检查累计长度，超过 `max_bytes` 即返回错误（OOM 防护核心）。
    //
    // 注：直接使用 `Cursor` 时 `fill_buf` 返回全部剩余数据（无 8KB 分块），
    //     对于 OOM 防护测试需用 `BufReader` 包装以模拟真实管道的分块读取行为。
    use std::io::Cursor;

    #[test]
    fn test_read_line_with_limit_normal_line() {
        // 正常行（含尾部换行符）应原样返回，与原 read_line 行为一致
        let mut reader = Cursor::new(b"hello\n".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, b"hello\n");
    }

    #[test]
    fn test_read_line_with_limit_json_command() {
        // 模拟 IPC JSON 命令（含尾部换行符）
        let mut reader = Cursor::new(b"{\"command\":\"play\"}\n".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, b"{\"command\":\"play\"}\n");
    }

    #[test]
    fn test_read_line_with_limit_empty_eof() {
        // EOF（无任何数据）应返回空 Vec（调用方据此判断管道关闭）
        let mut reader = Cursor::new(b"".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, b"");
    }

    #[test]
    fn test_read_line_with_limit_partial_line_no_newline_eof() {
        // 无换行符但到达 EOF：应返回已读取的内容（与 read_line 行为一致）
        let mut reader = Cursor::new(b"partial line without newline".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, b"partial line without newline");
    }

    #[test]
    fn test_read_line_with_limit_exceeds_max_no_newline() {
        // 超长行（MAX_LINE_BYTES + 1 字节，不含换行）应返回 Err
        // check before copy：buf.len() + chunk_len > MAX_LINE_BYTES → Err
        // 使用 BufReader 包装以模拟真实管道的 8KB 分块读取行为
        let data = vec![b'A'; MAX_LINE_BYTES + 1];
        let mut reader = BufReader::new(Cursor::new(data));
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX);
        let err = result.expect_err("超长行应返回错误");
        match err {
            MirrorStarError::IpcError(msg) => {
                assert!(
                    msg.contains(&format!("超过 {} 字节上限", MAX_LINE_BYTES)),
                    "错误信息应包含'超过 {} 字节上限'，实际: {}",
                    MAX_LINE_BYTES,
                    msg
                );
            }
            other => panic!("期望 IpcError，实际: {:?}", other),
        }
    }

    #[test]
    fn test_read_line_with_limit_oversized_2mb_no_oom() {
        // OOM 防护核心：输入 2MB 无换行数据应返回 Err，且 buf 不会增长到 2MB
        // 使用 BufReader 包装 Cursor，使 fill_buf 每次最多返回 8KB（模拟真实管道行为），
        // 增量长度检查在 buf 略超 1MB 时即触发，远小于 2MB 输入。
        let size = MAX_LINE_BYTES * 2;
        let data = vec![b'A'; size];
        let mut reader = BufReader::new(Cursor::new(data));
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX);
        assert!(result.is_err(), "2MB 无换行数据应返回错误");
        if let Err(MirrorStarError::IpcError(msg)) = &result {
            assert!(
                msg.contains(&format!("超过 {} 字节上限", MAX_LINE_BYTES)),
                "错误信息应包含'超过 {} 字节上限'，实际: {}",
                MAX_LINE_BYTES,
                msg
            );
        }
    }

    #[test]
    fn test_read_line_with_limit_oom_protection_infinite_stream() {
        // OOM 防护终极场景：模拟恶意端发送无限数据不含 \n（等价于 100MB+ 攻击）
        // 使用自定义 Read 产生无限字节流，无需实际分配 100MB 内存。
        // BufReader 的 8KB 内部缓冲使 fill_buf 每次最多返回 8KB，
        // 增量长度检查在 buf 略超 1MB 时即触发，不会无限增长。
        struct InfiniteByteStream(u8);
        impl std::io::Read for InfiniteByteStream {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                for b in buf.iter_mut() {
                    *b = self.0;
                }
                Ok(buf.len())
            }
        }
        // BufReader 包装使 InfiniteByteStream（仅实现 Read）满足 BufRead 约束
        let mut reader = BufReader::new(InfiniteByteStream(b'x'));
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX);
        assert!(result.is_err(), "无限数据流应因超过上限返回错误");
        if let Err(MirrorStarError::IpcError(msg)) = &result {
            assert!(
                msg.contains(&format!("超过 {} 字节上限", MAX_LINE_BYTES)),
                "错误信息: {}",
                msg
            );
        }
    }

    #[test]
    fn test_read_line_with_limit_exactly_max_no_newline_eof_ok() {
        // 边界（`>` 语义）：恰好 MAX_LINE_BYTES 字节（不含换行，EOF）应返回 Ok
        // 因为 buf.len() == MAX_LINE_BYTES，不满足 > MAX_LINE_BYTES
        let data = vec![b'A'; MAX_LINE_BYTES];
        let mut reader = Cursor::new(data);
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line.len(), MAX_LINE_BYTES);
    }

    #[test]
    fn test_read_line_with_limit_exactly_max_with_newline_ok() {
        // 边界（`>` 语义）：MAX_LINE_BYTES - 1 字节 + '\n' = MAX_LINE_BYTES 字节应返回 Ok
        let mut data = vec![b'A'; MAX_LINE_BYTES - 1];
        data.push(b'\n');
        let mut reader = Cursor::new(data);
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line.len(), MAX_LINE_BYTES);
        assert!(line.ends_with(b"\n"));
    }

    #[test]
    fn test_read_line_with_limit_max_plus_one_with_newline_err() {
        // 边界（`>` 语义）：MAX_LINE_BYTES 字节 + '\n' = MAX_LINE_BYTES + 1 字节应返回 Err
        let mut data = vec![b'A'; MAX_LINE_BYTES];
        data.push(b'\n');
        let mut reader = Cursor::new(data);
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX);
        assert!(
            result.is_err(),
            "MAX_LINE_BYTES + 1 字节（含换行）应返回错误"
        );
    }

    #[test]
    fn test_read_line_with_limit_utf8_multibyte() {
        // UTF-8 多字节字符应以原始字节返回（UTF-8 解码由外层 read_line_with_timeout 统一执行）
        // "héllo" = h(1) + é(2) + l(1) + l(1) + o(1) = 6 字节，+ '\n' = 7 字节
        let mut reader = Cursor::new("héllo\n".as_bytes().to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, "héllo\n".as_bytes());
    }

    #[test]
    fn test_read_line_with_limit_invalid_utf8_returns_raw_bytes() {
        // I-001 修复后 read_line_with_limit 返回原始字节 Vec<u8>，不再做 UTF-8 校验。
        // 非法 UTF-8 字节应原样返回（0xFF 是非法 UTF-8 起始字节），
        // UTF-8 校验由外层 read_line_with_timeout 在累积完整行后统一执行。
        let mut reader = Cursor::new(vec![0xFF, b'\n']);
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line, vec![0xFF, b'\n']);
    }

    #[test]
    fn test_read_line_with_limit_custom_max_bytes_exceeded() {
        // 自定义 max_bytes 上限应生效
        let mut reader = Cursor::new(b"abcdefgh\n".to_vec());
        // max_bytes = 5：行长度 9 > 5，应返回 Err
        let result = read_line_with_limit(&mut reader, 5, usize::MAX);
        assert!(result.is_err(), "超过自定义上限应返回错误");
    }

    #[test]
    fn test_read_line_with_limit_custom_max_bytes_boundary() {
        // 自定义上限边界：恰好等于 max_bytes 应返回 Ok（`>` 语义）
        let mut reader = Cursor::new(b"abc\n".to_vec());
        // 行长度 4（含 \n），max_bytes = 4：4 > 4 为 false → Ok
        let line = read_line_with_limit(&mut reader, 4, usize::MAX).unwrap();
        assert_eq!(line, b"abc\n");
    }

    #[test]
    fn test_read_line_with_limit_multiple_lines() {
        // 验证连续读取多行互不干扰（consume 位置正确）
        let data = b"line1\nline2\nline3\n".to_vec();
        let mut reader = Cursor::new(data);

        let l1 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(l1, b"line1\n");

        let l2 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(l2, b"line2\n");

        let l3 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(l3, b"line3\n");

        // EOF 后返回空 Vec
        let l4 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(l4, b"");
    }

    // ── I-001 超时盲区修复单元测试 ─────────────────────────────────────────
    //
    // 验证 `read_line_with_limit` 的 `total_avail` 参数限制消费量，避免在 PIPE_WAIT
    // 阻塞模式下 `fill_buf` → `File::read` 无限阻塞。这是 I-001 修复的核心机制：
    // 消费量达到 `total_avail` 后主动返回控制流，由外层 `read_line_with_timeout`
    // 重新 peek + 检查 deadline，使超时保证始终有效。
    //
    // 注：`read_line_with_timeout` 本身需要真实 `NamedPipeClient`（PeekNamedPipe），
    //     完整的端到端超时测试由集成测试 `tests/ipc_timeout.rs` 覆盖。
    //     此处仅单元测试 `read_line_with_limit` 的 total_avail 限制行为，
    //     并模拟外层循环的累积逻辑验证部分行拼接正确性。

    #[test]
    fn i001_total_avail_zero_returns_empty_without_reading() {
        // total_avail = 0 时应立即返回空 Vec，不调用 fill_buf（避免阻塞）
        // 模拟 PeekNamedPipe 报告 0 字节可用时 read_line_with_timeout 不应读取
        let mut reader = Cursor::new(b"hello world\n".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 0).unwrap();
        assert_eq!(line, b"");
        // 验证未消费任何数据：再次以 usize::MAX 读取应得到完整行
        let line2 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(line2, b"hello world\n");
    }

    #[test]
    fn i001_total_avail_limits_consumption_returns_partial() {
        // 模拟 I-001 场景：PeekNamedPipe 报告 N 字节可用（不含 \n），
        // read_line_with_limit 消费 N 字节后主动返回部分行（不含 \n），
        // 而非继续 fill_buf 阻塞等待 \n。
        let data = b"partial without newline".to_vec();
        let total_avail = data.len(); // peek 报告的全部可用量
        let mut reader = Cursor::new(data);
        let chunk = read_line_with_limit(&mut reader, MAX_LINE_BYTES, total_avail).unwrap();
        // 应返回全部已 peek 的数据（不含 \n），不阻塞
        assert_eq!(chunk, b"partial without newline");
        assert!(!chunk.ends_with(b"\n"), "部分行不应包含换行符");
    }

    #[test]
    fn i001_total_avail_less_than_data_returns_truncated_partial() {
        // total_avail 小于数据长度时，只消费 total_avail 字节即返回
        // 模拟 peek 只报告部分可用量（如管道分批送达）
        // 使用 BufReader::with_capacity(5, ...) 使 fill_buf 每次最多返回 5 字节，
        // 模拟真实管道分块读取：Cursor 的 fill_buf 会一次返回全部数据，无法测出
        // total_avail 的限制效果。
        let mut reader = BufReader::with_capacity(5, Cursor::new(b"hello world\n".to_vec()));
        // peek 只报告 5 字节可用
        let chunk = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 5).unwrap();
        assert_eq!(chunk, b"hello");
        assert!(!chunk.ends_with(b"\n"));
        // 再次读取剩余数据（模拟外层重新 peek 后继续读取）
        let chunk2 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(chunk2, b" world\n");
    }

    #[test]
    fn i001_partial_accumulation_simulates_outer_loop() {
        // 模拟 read_line_with_timeout 的外层累积逻辑：
        // 服务端分批发送 "hello world\n"，每次 peek 只看到部分数据。
        // 外层循环多次调用 read_line_with_limit 并累积，最终拼出完整行。
        let data = b"hello world\n".to_vec();
        // 使用 BufReader::with_capacity(5, ...) 使 fill_buf 每次最多返回 5 字节，
        // 模拟真实管道分块读取：BufReader::new 默认 8KB 缓冲会一次返回全部数据，
        // 无法测出 total_avail 限制下分批累积的正确性。
        let mut reader = BufReader::with_capacity(5, Cursor::new(data));
        let mut accumulated: Vec<u8> = Vec::new();

        // 第一次 peek 报告 5 字节（"hello"），不含 \n → 累积
        let chunk1 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 5).unwrap();
        assert!(!chunk1.ends_with(b"\n"));
        accumulated.extend_from_slice(&chunk1);

        // 第二次 peek 报告剩余全部字节（" world\n"），含 \n → 拼接并完成
        let chunk2 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert!(chunk2.ends_with(b"\n"));
        accumulated.extend_from_slice(&chunk2);

        // 验证累积结果为完整行
        let line = String::from_utf8(accumulated).unwrap();
        assert_eq!(line, "hello world\n");
    }

    #[test]
    fn i001_total_avail_prevents_infinite_stream_blocking() {
        // 验证 total_avail 防止无限数据流导致 fill_buf 无限阻塞（I-001 核心）
        // 使用 InfiniteByteStream 模拟管道持续有数据但不含 \n 的场景：
        //   - 无 total_avail 限制：fill_buf 持续读取，最终因 max_bytes 报错（I02）
        //   - 有 total_avail 限制：消费 total_avail 字节后立即返回，不阻塞
        struct InfiniteByteStream(u8);
        impl std::io::Read for InfiniteByteStream {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                for b in buf.iter_mut() {
                    *b = self.0;
                }
                Ok(buf.len())
            }
        }
        let mut reader = BufReader::new(InfiniteByteStream(b'x'));
        // total_avail = 10：只消费 10 字节即返回，不因无限数据流阻塞
        let chunk = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 10).unwrap();
        assert_eq!(chunk.len(), 10);
        assert_eq!(chunk, vec![b'x'; 10]);
        assert!(!chunk.ends_with(b"\n"));
    }

    #[test]
    fn i001_total_avail_with_newline_returns_complete_line() {
        // total_avail 足够大时，遇到 \n 即返回完整行（与无限制行为一致）
        // 确认 total_avail 限制不影响正常路径（含 \n 的完整行读取）
        let mut reader = Cursor::new(b"hello\nworld\n".to_vec());
        // total_avail = 6（恰好覆盖 "hello\n"）
        let chunk = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 6).unwrap();
        assert_eq!(chunk, b"hello\n");
        assert!(chunk.ends_with(b"\n"));
        // 剩余数据可继续读取
        let chunk2 = read_line_with_limit(&mut reader, MAX_LINE_BYTES, usize::MAX).unwrap();
        assert_eq!(chunk2, b"world\n");
    }

    // ── v41-I-001 UTF-8 截断处理单元测试 ───────────────────────────────────
    //
    // 验证 `read_line_with_limit` 在 `total_avail` 边界处遇到多字节 UTF-8 字符截断时，
    // 通过 `Utf8Error::error_len()` 区分"截断"（None）与"无效字节"（Some(_)）：
    //   - 截断：继续 fill_buf 读取后续字节，直到构成完整字符或遇到 \n / EOF / max_bytes
    //   - 无效字节：直接返回 IpcError，避免外层累积后仍解码失败

    #[test]
    fn v41_i001_utf8_truncation_continues_reading() {
        // 场景：peek 报告 total_avail = 7，恰好在多字节 UTF-8 字符 "中"（0xE4 0xB8 0xAD）
        // 的首字节后截断。v41-I-001 修复前：返回 "Hello \xE4"（截断字节），外层
        // String::from_utf8 失败误判为协议错误。修复后：检测到 error_len() == None
        // （截断），继续 fill_buf 读取后续 0xB8 0xAD + "\n"，返回完整 "Hello 中\n"。
        //
        // 使用 BufReader::with_capacity(7, Cursor) 使 fill_buf 第一次最多返回 7 字节
        // （模拟 peek 只报告 7 字节 + BufReader 内部缓冲分块），第二次返回剩余 3 字节。
        let data = "Hello 中\n".as_bytes().to_vec();
        let mut reader = BufReader::with_capacity(7, Cursor::new(data));
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 7).unwrap();
        assert_eq!(line, "Hello 中\n".as_bytes());
        assert!(line.ends_with(b"\n"));
        // 验证 UTF-8 解码成功（外层 read_line_with_timeout 会执行此步）
        let s = String::from_utf8(line).expect("应成功解码为 UTF-8");
        assert_eq!(s, "Hello 中\n");
    }

    #[test]
    fn v41_i001_utf8_truncation_4byte_continues_reading() {
        // 场景：4 字节 UTF-8 字符（如 U+1F600 = 0xF0 0x9F 0x98 0x80）在首字节后截断。
        // total_avail = 2（"A" + 0xF0），截断后需继续读取 3 字节构成完整字符。
        let data = "A😀\n".as_bytes().to_vec();
        let mut reader = BufReader::with_capacity(2, Cursor::new(data));
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 2).unwrap();
        assert_eq!(line, "A😀\n".as_bytes());
        let s = String::from_utf8(line).expect("应成功解码为 UTF-8");
        assert_eq!(s, "A😀\n");
    }

    #[test]
    fn v41_i001_utf8_invalid_byte_returns_error() {
        // 场景：输入流含无效 UTF-8 字节（0xFF 是非法起始字节，非 0xxxxxxx /
        // 110xxxxx / 1110xxxx / 11110xxx 中的任何一种）。error_len() == Some(1)
        // 表示无效字节而非截断，应直接返回 IpcError，而非继续读取。
        let mut reader = Cursor::new(vec![0xFF, 0xFE]);
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 2);
        let err = result.expect_err("无效 UTF-8 字节应返回错误");
        match err {
            MirrorStarError::IpcError(msg) => {
                assert!(
                    msg.contains("UTF-8"),
                    "错误信息应包含 'UTF-8'，实际: {}",
                    msg
                );
            }
            other => panic!("期望 IpcError，实际: {:?}", other),
        }
    }

    #[test]
    fn v41_i001_utf8_invalid_byte_after_valid_returns_error() {
        // 场景：有效 ASCII 后跟无效字节。"Hello" + 0xFF + "\n"
        // total_avail = 6（"Hello" + 0xFF），consumed 达到 total_avail 时检查 UTF-8，
        // 0xFF 是无效字节（error_len() == Some(1)），返回 IpcError。
        // 使用 BufReader::with_capacity(6, ...) 使 fill_buf 第一次最多返回 6 字节，
        // 确保 \n 不在第一次 fill_buf 范围内（否则会因找到 \n 一次读取全部，不触发 UTF-8 检查）。
        let mut reader = BufReader::with_capacity(6, Cursor::new(b"Hello\xFF\n".to_vec()));
        let result = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 6);
        let err = result.expect_err("有效 ASCII 后跟无效字节应返回错误");
        match err {
            MirrorStarError::IpcError(msg) => {
                assert!(
                    msg.contains("UTF-8"),
                    "错误信息应包含 'UTF-8'，实际: {}",
                    msg
                );
            }
            other => panic!("期望 IpcError，实际: {:?}", other),
        }
    }

    #[test]
    fn v41_i001_utf8_truncation_eof_returns_partial_bytes() {
        // 场景：截断后遇到 EOF（管道关闭），无法构成完整 UTF-8 字符。
        // 此时返回已读取的部分字节（让外层 read_line_with_timeout 在 EOF 时
        // 调用 from_utf8 失败并报告错误，与原行为一致）。
        // 输入："Hello \xE4"（截断，无后续字节，无 \n）
        let mut reader = Cursor::new(b"Hello \xE4".to_vec());
        let line = read_line_with_limit(&mut reader, MAX_LINE_BYTES, 7).unwrap();
        // 截断模式下继续 fill_buf，但 EOF 返回已读取内容
        assert_eq!(line, b"Hello \xE4");
        // 验证外层 from_utf8 会失败（确认这是真实的截断场景）
        assert!(
            String::from_utf8(line).is_err(),
            "截断字节应无法解码为 UTF-8"
        );
    }
}
