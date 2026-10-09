//! 分帧行读写器：统一信封 ↔ NDJSON 行（`\n` 分帧）。
//!
//! ## 复用而非重造
//!
//! 分帧的底层能力复用 [`crate::ipc::client`]：
//! - 写入：复用 [`crate::ipc::client::write_json_line`]（一行 JSON + 换行 + flush）；
//! - 读取：复用 [`crate::ipc::client::read_line_with_limit`]（`fill_buf` 增量读取 +
//!   单行 1MB 硬上限的 OOM 防护）。
//!
//! ## 关于超时
//!
//! `ipc::client` 中已有的 `read_line_with_timeout` 依赖 Windows `PeekNamedPipe`，
//! 且签名限定为 `BufReader<std::fs::File>`，无法用于泛型 `impl BufRead`（stdio 句柄、
//! 测试用 `Cursor` 等）。因此本模块对任意 `BufRead` 不提供"阻塞读 + 超时"的单函数，
//! 而是提供 [`MessageReceiver`]：把读取放进独立线程、用带超时的通道接收。这是不依赖
//! 平台特定 API 的通用做法，也正好对应用户态架构（UI 侧独立读取线程 → 事件循环）。

use std::io::{BufRead, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ipc::client::{read_line_with_limit, write_json_line};
use crate::MirrorStarError;

use super::message::UiMessage;

/// 单条协议消息的最大字节数（1MB），与 `ipc::client` 的 OOM 防护上限一致。
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// 把一条统一信封写入 `impl Write`（序列化为一行 JSON 并追加换行符）。
pub fn write_message<W: Write>(writer: &mut W, message: &UiMessage) -> Result<(), MirrorStarError> {
    let json = serde_json::to_string(message)
        .map_err(|e| MirrorStarError::IpcError(format!("UI 协议消息序列化失败: {}", e)))?;
    write_json_line(writer, &json)
}

/// 从 `impl BufRead` 读出单条消息。
///
/// - 返回 `Ok(Some(msg))`：解析到一条消息；
/// - 返回 `Ok(None)`：流已到 EOF；
/// - 返回 `Err(...)`：IO / UTF-8 / JSON 解析失败。
///
/// **空行跳过**：空白行（含历史遗留的"发送空行预热"、以及 JSON 之间多余的换行）
/// 会被忽略而非当作解析失败。
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<UiMessage>, MirrorStarError> {
    loop {
        // total_avail 传 usize::MAX：交由底层阻塞读取直到遇到 \n 或 EOF；
        // 单行长度仍受 MAX_MESSAGE_BYTES 硬上限保护。
        let line = read_line_with_limit(reader, MAX_MESSAGE_BYTES, usize::MAX)?;
        if line.is_empty() {
            // EOF：无更多数据。
            return Ok(None);
        }
        let text = std::str::from_utf8(&line)
            .map_err(|e| MirrorStarError::IpcError(format!("UI 协议消息非有效 UTF-8: {}", e)))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            // 空行：跳过，继续读下一行。
            continue;
        }
        let message = serde_json::from_str(trimmed)
            .map_err(|e| MirrorStarError::IpcError(format!("UI 协议消息解析失败: {}", e)))?;
        return Ok(Some(message));
    }
}

/// 后台读取线程 + 超时接收：从 `BufRead` 端持续拉取消息，供调用方按超时消费。
///
/// 详见模块级文档「关于超时」：泛型 `impl BufRead` 的底层 `read` 是阻塞的，
/// Rust 标准库无法对任意 `BufRead` 施加可中断的超时，故用独立线程 + 带超时通道实现。
/// UI 进程与 Tauri 壳各自用它承载自己的协议读取端。
pub struct MessageReceiver {
    /// 读取线程投递的消息 / 错误。
    rx: mpsc::Receiver<Result<UiMessage, MirrorStarError>>,
    /// 读取线程句柄；持有以保持其生命周期（drop 时分离，不阻塞等待）。
    _reader_thread: JoinHandle<()>,
}

impl MessageReceiver {
    /// 启动后台读取线程；`reader` 的所有权移交线程。
    ///
    /// 线程在 EOF、首个解析/IO 错误、或接收端已丢弃后退出。
    pub fn spawn<R: BufRead + Send + 'static>(mut reader: R) -> Self {
        let (tx, rx) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || loop {
            match read_message(&mut reader) {
                Ok(Some(message)) => {
                    if tx.send(Ok(message)).is_err() {
                        // 接收端已丢弃，停止读取，让线程自然结束。
                        break;
                    }
                }
                // EOF：流正常结束。
                Ok(None) => break,
                Err(error) => {
                    let _ = tx.send(Err(error));
                    break;
                }
            }
        });
        Self {
            rx,
            _reader_thread: reader_thread,
        }
    }

    /// 在 `timeout` 内接收一条消息。
    ///
    /// - 超时 → [`MirrorStarError::IpcTimeout`]；
    /// - 读取线程已结束（EOF）→ [`MirrorStarError::IpcDisconnected`]；
    /// - 读取线程报告解析/IO 错误 → 原样返回该错误。
    pub fn recv_timeout(&self, timeout: Duration) -> Result<UiMessage, MirrorStarError> {
        match self.rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(MirrorStarError::IpcTimeout(format!(
                "等待 UI 协议消息超时（{}ms）",
                timeout.as_millis()
            ))),
            Err(RecvTimeoutError::Disconnected) => Err(MirrorStarError::IpcDisconnected(
                "UI 协议读取线程已结束".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_proto::message::{
        Hello, UiError, UiEvent, UiRequest, UiResponse, PROTOCOL_VERSION,
    };

    use std::io::{Cursor, Read};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn roundtrip(message: &UiMessage) -> UiMessage {
        let mut buffer = Vec::new();
        write_message(&mut buffer, message).expect("写入失败");
        // 每行以 \n 分帧
        assert_eq!(buffer.last(), Some(&b'\n'), "消息应以换行符结尾");
        let mut reader = Cursor::new(buffer);
        read_message(&mut reader)
            .expect("读取失败")
            .expect("不应为 EOF")
    }

    fn sample_messages() -> Vec<UiMessage> {
        vec![
            UiMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                capabilities: super::super::message::capability::ALL,
                client: "mirrorstar-ui".to_string(),
            }),
            UiMessage::Request(UiRequest {
                request_id: 7,
                command: "get_wallpapers".to_string(),
                params: serde_json::json!({ "offset": 0, "limit": 50 }),
            }),
            UiMessage::Response(UiResponse::ok(7, serde_json::json!([{ "id": "a" }]))),
            UiMessage::Response(UiResponse::error(
                9,
                UiError {
                    code: crate::ui_proto::error_code::IPC_TIMEOUT.to_string(),
                    message: "读取超时".to_string(),
                },
            )),
            UiMessage::Event(UiEvent {
                event: "wallpaper-added".to_string(),
                data: serde_json::json!({ "id": "a", "type": "image" }),
            }),
            UiMessage::Control(super::super::message::UiControl {
                action: super::super::message::ControlAction::Show,
            }),
        ]
    }

    #[test]
    fn all_message_kinds_roundtrip() {
        for message in sample_messages() {
            let back = roundtrip(&message);
            assert_eq!(back, message, "往返后不一致: {:?}", message);
        }
    }

    #[test]
    fn envelope_uses_internal_type_tag() {
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &UiMessage::Request(UiRequest {
                request_id: 1,
                command: "get_wallpapers".to_string(),
                params: serde_json::Value::Null,
            }),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buffer).unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["command"], "get_wallpapers");
        // 内部标签：字段扁平，不存在 content/payload 嵌套层
        assert!(value.get("content").is_none());
        assert!(value.get("payload").is_none());
    }

    #[test]
    fn request_params_defaults_to_null_when_omitted() {
        let json = r#"{"type":"request","request_id":3,"command":"pause"}"#;
        let message: UiMessage = serde_json::from_str(json).unwrap();
        match message {
            UiMessage::Request(req) => {
                assert_eq!(req.request_id, 3);
                assert_eq!(req.command, "pause");
                assert_eq!(req.params, serde_json::Value::Null);
            }
            other => panic!("期望 Request，实际: {:?}", other),
        }
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // 各类型均塞入未来版本可能新增的字段，解析应成功且不影响已知字段。
        let request = r#"{"type":"request","request_id":1,"command":"get_wallpapers","params":{},"future":true}"#;
        match serde_json::from_str::<UiMessage>(request).unwrap() {
            UiMessage::Request(req) => assert_eq!(req.command, "get_wallpapers"),
            other => panic!("期望 Request，实际: {:?}", other),
        }

        let response =
            r#"{"type":"response","request_id":1,"status":"ok","data":null,"elapsed_ms":12}"#;
        match serde_json::from_str::<UiMessage>(response).unwrap() {
            UiMessage::Response(resp) => assert!(resp.is_ok()),
            other => panic!("期望 Response，实际: {:?}", other),
        }

        let event = r#"{"type":"event","event":"config-changed","data":{},"seq":99}"#;
        match serde_json::from_str::<UiMessage>(event).unwrap() {
            UiMessage::Event(ev) => assert_eq!(ev.event, "config-changed"),
            other => panic!("期望 Event，实际: {:?}", other),
        }

        let hello = r#"{"type":"hello","protocol_version":1,"capabilities":0,"client":"x","build":"debug"}"#;
        match serde_json::from_str::<UiMessage>(hello).unwrap() {
            UiMessage::Hello(h) => assert_eq!(h.protocol_version, PROTOCOL_VERSION),
            other => panic!("期望 Hello，实际: {:?}", other),
        }
    }

    #[test]
    fn error_response_roundtrips_without_data() {
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &UiMessage::Response(UiResponse::error(
                5,
                UiError {
                    code: "invalid_argument".to_string(),
                    message: "参数非法".to_string(),
                },
            )),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buffer).unwrap();
        // 失败响应不携带 data 字段
        assert!(value.get("data").is_none());
        assert_eq!(value["status"], "error");
        assert_eq!(value["error"]["code"], "invalid_argument");
    }

    #[test]
    fn empty_lines_are_skipped() {
        // 历史"发送空行预热"及多余换行应被跳过，不视为解析失败。
        let mut input = Vec::new();
        input.extend_from_slice(b"\n");
        input.extend_from_slice(b"   \n");
        write_message(
            &mut input,
            &UiMessage::Event(UiEvent {
                event: "wallpaper-removed".to_string(),
                data: serde_json::json!({ "id": "b" }),
            }),
        )
        .unwrap();
        let mut reader = Cursor::new(input);
        match read_message(&mut reader).unwrap() {
            Some(UiMessage::Event(ev)) => assert_eq!(ev.event, "wallpaper-removed"),
            other => panic!("期望 Event，实际: {:?}", other),
        }
        // 后续为 EOF
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn eof_returns_none() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn hello_capability_bits() {
        use super::super::message::capability;
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities: capability::EVENT_PUSH | capability::PREVIEW,
            client: String::new(),
        };
        assert!(hello.has(capability::EVENT_PUSH));
        assert!(hello.has(capability::PREVIEW));
        assert!(!hello.has(capability::DRAG_DROP));
        assert!(hello.has(capability::EVENT_PUSH | capability::PREVIEW));
        assert!(!hello.has(capability::ALL));
    }

    #[test]
    fn message_receiver_delivers_message() {
        let mut input = Vec::new();
        write_message(
            &mut input,
            &UiMessage::Request(UiRequest {
                request_id: 42,
                command: "get_config".to_string(),
                params: serde_json::Value::Null,
            }),
        )
        .unwrap();
        let receiver = MessageReceiver::spawn(Cursor::new(input));
        match receiver.recv_timeout(Duration::from_secs(2)).unwrap() {
            UiMessage::Request(req) => assert_eq!(req.request_id, 42),
            other => panic!("期望 Request，实际: {:?}", other),
        }
    }

    /// 永远阻塞（不产生数据、不 EOF）的读取端，用于验证超时行为。
    struct BlockingReader {
        released: Arc<AtomicBool>,
    }

    impl Read for BlockingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            while !self.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // 释放后返回 EOF，使读取线程自然退出。
            Ok(0)
        }
    }

    #[test]
    fn message_receiver_times_out_without_panicking() {
        let released = Arc::new(AtomicBool::new(false));
        let receiver = MessageReceiver::spawn(std::io::BufReader::new(BlockingReader {
            released: released.clone(),
        }));

        let timeout = Duration::from_millis(50);
        let start = Instant::now();
        let result = receiver.recv_timeout(timeout);
        let elapsed = start.elapsed();

        // 释放读取线程，避免测试结束后残留阻塞线程。
        released.store(true, Ordering::SeqCst);

        assert!(
            matches!(result, Err(MirrorStarError::IpcTimeout(_))),
            "读不到消息时应返回 IpcTimeout，实际: {:?}",
            result
        );
        // 不应无限阻塞：在超时 + 容差内返回。
        assert!(
            elapsed < Duration::from_secs(2),
            "超时应迅速返回，实际耗时 {:?}",
            elapsed
        );
    }

    #[test]
    fn message_receiver_disconnected_after_eof() {
        let receiver = MessageReceiver::spawn(Cursor::new(Vec::<u8>::new()));
        // 等待读取线程读到 EOF 并结束（Empty 流立即 EOF）。
        let result = receiver.recv_timeout(Duration::from_secs(1));
        assert!(
            matches!(result, Err(MirrorStarError::IpcDisconnected(_))),
            "EOF 后应返回 IpcDisconnected，实际: {:?}",
            result
        );
    }

    // ── 信封鲁棒性边界（未知 type / 缺字段 / 非法 UTF-8 / 超长 / 截断） ──────

    #[test]
    fn unknown_message_type_is_rejected() {
        // 未知 `type`：内部标签枚举无法命中任何变体 → 明确报错（而非静默丢弃）。
        let mut reader = Cursor::new(br#"{"type":"bogus","request_id":1}"#.to_vec());
        let err = read_message(&mut reader).expect_err("未知 type 应解析失败");
        assert!(matches!(err, MirrorStarError::IpcError(_)), "实际: {err:?}");
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        // Request 缺 `command`
        let mut reader = Cursor::new(br#"{"type":"request","request_id":1}"#.to_vec());
        assert!(
            read_message(&mut reader).is_err(),
            "Request 缺必需字段应失败"
        );
        // Response 缺 `status`
        let mut reader = Cursor::new(br#"{"type":"response","request_id":1}"#.to_vec());
        assert!(
            read_message(&mut reader).is_err(),
            "Response 缺必需字段应失败"
        );
        // UiError 缺 `message`
        let mut reader = Cursor::new(
            br#"{"type":"response","request_id":1,"status":"error","error":{"code":"x"}}"#.to_vec(),
        );
        assert!(
            read_message(&mut reader).is_err(),
            "UiError 缺必需字段应失败"
        );
    }

    #[test]
    fn invalid_utf8_line_is_rejected() {
        // 0xFF 为非法 UTF-8 起始字节；分帧层必须在解码处明确报错。
        let mut reader = Cursor::new(vec![0xFF, 0xFE, b'\n']);
        let err = read_message(&mut reader).expect_err("非法 UTF-8 应返回错误");
        assert!(
            matches!(&err, MirrorStarError::IpcError(msg) if msg.contains("UTF-8")),
            "实际: {err:?}"
        );
    }

    #[test]
    fn oversized_line_beyond_limit_is_rejected() {
        // 超过 MAX_MESSAGE_BYTES 的“超大 payload”：不得 OOM，必须明确报错。
        let mut data = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        data.push(b'\n');
        let mut reader = std::io::BufReader::new(Cursor::new(data));
        let err = read_message(&mut reader).expect_err("超长行应返回错误");
        assert!(matches!(err, MirrorStarError::IpcError(_)), "实际: {err:?}");
    }

    #[test]
    fn truncated_final_line_without_newline_is_error() {
        // 对端写到一半断开（无换行结尾）：行不完整 → JSON 解析失败 → 明确报错，
        // 而非误判为 EOF（EOF 仅由“流尾无数据”触发）。
        let mut reader = Cursor::new(br#"{"type":"event","event":"x""#.to_vec());
        let err = read_message(&mut reader).expect_err("截断的 JSON 应返回错误");
        assert!(matches!(err, MirrorStarError::IpcError(_)), "实际: {err:?}");
    }

    #[test]
    fn request_id_extremes_roundtrip() {
        // request_id 线格式为 u64：含 0 与 u64::MAX 的往返必须逐位保持。
        for id in [0u64, 1, u64::MAX] {
            let message = UiMessage::Request(UiRequest {
                request_id: id,
                command: "pause".to_string(),
                params: serde_json::Value::Null,
            });
            match roundtrip(&message) {
                UiMessage::Request(req) => assert_eq!(req.request_id, id),
                other => panic!("期望 Request，实际: {other:?}"),
            }
        }
    }
}
