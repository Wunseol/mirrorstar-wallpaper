//! UI 侧协议客户端（NDJSON over stdio）。
//!
//! ## 线程模型
//!
//! - **UI 线程**：运行 Slint 事件循环；所有状态仓写回与视图刷新都在此线程完成。
//! - **读取线程**（[`spawn_reader`]）：独占 stdin，用 [`mirrorstar_core::ui_proto::framing`]
//!   解析 NDJSON，再把消息经 [`Dispatcher`]**投递回 UI 线程**执行。读取线程本身不做任何
//!   状态修改，因此 UI 线程**绝不**被 IO 阻塞。
//! - **写**：任意线程都可调用 [`UiClient::request`]；写端用 `Mutex` 串行化，
//!   `request_id` 单调自增，响应按 id 回填到注册的回调。
//!
//! ## 为什么把"投递"抽象成 [`Dispatcher`]
//!
//! 生产环境注入 [`slint_dispatch`]（内部 `slint::invoke_from_event_loop`）；单测注入同步
//! 实现即可在无 GUI 的情况下验证"请求/响应匹配"与"事件分发"，无需启动事件循环。

use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Write};
use std::sync::{Arc, Mutex};

use mirrorstar_core::ui_proto::framing::{read_message, write_message};
use mirrorstar_core::ui_proto::{
    capability, ControlAction, Hello, UiControl, UiError, UiEvent, UiMessage, UiRequest,
    PROTOCOL_VERSION,
};
use mirrorstar_core::MirrorStarError;
use serde_json::Value;

/// 一次请求的结果：成功为 `data`（无数据时为 `null`），失败为结构化错误。
pub type RequestResult = Result<Value, UiError>;

/// 响应回调；经 [`Dispatcher`] 在 UI 线程执行，故必须是 `Send`。
type ResponseHandler = Box<dyn FnOnce(RequestResult) + Send + 'static>;

/// 事件回调（在 UI 线程执行）。
pub type EventHandler = Arc<dyn Fn(UiEvent) + Send + Sync + 'static>;
/// 控制指令回调（在 UI 线程执行）。
pub type ControlHandler = Arc<dyn Fn(ControlAction) + Send + Sync + 'static>;
/// 连接断开回调（在 UI 线程执行），入参为断开原因。
pub type DisconnectHandler = Arc<dyn Fn(String) + Send + Sync + 'static>;

/// 把闭包投递到 UI 线程执行；返回 `false` 表示投递失败（事件循环已不可用）。
pub type Dispatcher = Arc<dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync + 'static>;

/// 生产用投递器：经 Slint 事件循环执行闭包。
///
/// 只要 `MainWindow` 已创建，事件循环 provider 即存在；即使在 `run_event_loop_until_quit()` 之前调用，
/// 闭包也会被排队，并在事件循环启动后执行——这正是启动期快照响应能正确落地的前提。
pub fn slint_dispatch() -> Dispatcher {
    Arc::new(|task| slint::invoke_from_event_loop(task).is_ok())
}

/// 打印诊断日志到 **stderr**：stdout 是协议通道，绝不能混入日志。
pub fn log(line: impl AsRef<str>) {
    eprintln!("[mirrorstar-ui] {}", line.as_ref());
}

/// 壳侧必须具备的能力位（缺一即无法正常协作）。
pub const REQUIRED_SERVER_CAPABILITIES: u32 = capability::EVENT_PUSH | capability::REQUEST_RESPONSE;

/// 校验壳侧 Hello：协议版本一致 + 必需能力位齐备。
///
/// 纯函数，便于单测；失败返回 [`MirrorStarError::IpcError`]，调用方据此记录日志并退出。
pub fn validate_hello(hello: &Hello) -> Result<(), MirrorStarError> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(MirrorStarError::IpcError(format!(
            "协议版本不匹配（UI={} 壳={}），拒绝连接",
            PROTOCOL_VERSION, hello.protocol_version
        )));
    }
    if !hello.has(REQUIRED_SERVER_CAPABILITIES) {
        return Err(MirrorStarError::IpcError(format!(
            "壳侧能力位不足（需要 EVENT_PUSH|REQUEST_RESPONSE = {:#06x}，实际 {:#06x}）",
            REQUIRED_SERVER_CAPABILITIES, hello.capabilities
        )));
    }
    Ok(())
}

/// 启动期同步握手：读取一条消息并断言其为 `Hello`。
///
/// 仅用于事件循环启动前（此时读取线程尚未接管 stdin）；之后一律走 [`spawn_reader`]。
pub fn read_hello<R: BufRead>(reader: &mut R) -> Result<Hello, MirrorStarError> {
    match read_message(reader)? {
        Some(UiMessage::Hello(hello)) => Ok(hello),
        Some(other) => Err(MirrorStarError::IpcError(format!(
            "握手期间收到非 Hello 消息: {other:?}"
        ))),
        None => Err(MirrorStarError::IpcDisconnected(
            "握手期间 stdin 已关闭".to_string(),
        )),
    }
}

/// 请求 id 分配与响应配对表。
///
/// 与 IO、Slint 完全解耦（只有纯数据结构），因此可直接单测 id 匹配语义。
#[derive(Default)]
pub struct RequestTracker {
    next_id: u64,
    pending: HashMap<u64, ResponseHandler>,
}

impl RequestTracker {
    /// 分配下一个请求 id（从 1 开始单调递增，绕过 0）。
    pub fn alloc(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.next_id
    }

    /// 注册某 id 的响应回调。
    pub fn register(&mut self, id: u64, handler: ResponseHandler) {
        self.pending.insert(id, handler);
    }

    /// 取走（并移除）某 id 的回调；不存在返回 `None`。
    pub fn take(&mut self, id: u64) -> Option<ResponseHandler> {
        self.pending.remove(&id)
    }

    /// 尚未收到响应的请求数（诊断用）。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// UI 侧协议客户端。
///
/// 连接状态不在此处维护：权威连接态由状态仓（`AppStore::connected`）持有并投影到界面，
/// 避免"两处各存一份、谁先更新"的不一致。
pub struct UiClient {
    writer: Mutex<BufWriter<Box<dyn Write + Send>>>,
    tracker: Mutex<RequestTracker>,
    dispatcher: Dispatcher,
    event_handler: Mutex<Option<EventHandler>>,
    control_handler: Mutex<Option<ControlHandler>>,
    disconnect_handler: Mutex<Option<DisconnectHandler>>,
}

impl UiClient {
    /// 以 stdout 为写端构造客户端。
    pub fn new(dispatcher: Dispatcher) -> Arc<Self> {
        Self::with_writer(Box::new(std::io::stdout()), dispatcher)
    }

    /// 以自定义写端构造客户端（单测注入内存 sink 用）。
    pub fn with_writer(writer: Box<dyn Write + Send>, dispatcher: Dispatcher) -> Arc<Self> {
        Arc::new(Self {
            writer: Mutex::new(BufWriter::new(writer)),
            tracker: Mutex::new(RequestTracker::default()),
            dispatcher,
            event_handler: Mutex::new(None),
            control_handler: Mutex::new(None),
            disconnect_handler: Mutex::new(None),
        })
    }

    /// 注册事件回调。
    pub fn set_event_handler(&self, handler: EventHandler) {
        *self.lock_event() = Some(handler);
    }

    /// 注册控制指令回调。
    pub fn set_control_handler(&self, handler: ControlHandler) {
        *self.lock_control() = Some(handler);
    }

    /// 注册断开回调。
    pub fn set_disconnect_handler(&self, handler: DisconnectHandler) {
        *self.lock_disconnect() = Some(handler);
    }

    /// 发送本端 `Hello`（能力位广播自身支持的全集）。
    pub fn send_hello(&self) -> Result<(), MirrorStarError> {
        let hello = UiMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities: capability::ALL,
            client: "mirrorstar-ui".to_string(),
        });
        self.send(&hello)
    }

    /// 上报窗口可见性（供壳侧空闲退出计时）。
    pub fn report_visibility(&self, shown: bool) {
        let action = if shown {
            ControlAction::WindowShown
        } else {
            ControlAction::WindowHidden
        };
        let message = UiMessage::Control(UiControl { action });
        if let Err(e) = self.send(&message) {
            log(format!("上报窗口可见性失败: {e}"));
        }
    }

    /// 发起一次请求；`handler` 将在 UI 线程收到结果。
    ///
    /// **先注册、后写**：写失败时回滚注册，杜绝"响应已到但回调未挂上"与回调泄漏两个方向的问题。
    pub fn request(
        &self,
        command: &str,
        params: Value,
        handler: impl FnOnce(RequestResult) + Send + 'static,
    ) -> Result<u64, MirrorStarError> {
        let id = {
            let mut tracker = self.lock_tracker();
            let id = tracker.alloc();
            tracker.register(id, Box::new(handler));
            id
        };
        let message = UiMessage::Request(UiRequest {
            request_id: id,
            command: command.to_string(),
            params,
        });
        if let Err(e) = self.send(&message) {
            let _ = self.lock_tracker().take(id);
            return Err(e);
        }
        Ok(id)
    }

    /// 写入一条消息（内部串行化）。
    fn send(&self, message: &UiMessage) -> Result<(), MirrorStarError> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        write_message(&mut *writer, message)
    }

    /// 把闭包投递到 UI 线程。
    fn post(&self, task: Box<dyn FnOnce() + Send>) {
        if !(self.dispatcher)(task) {
            log("投递到 UI 线程失败（事件循环已不可用）");
        }
    }

    /// 处理一条响应：按 id 取出回调并投递执行。
    fn on_response(&self, response: mirrorstar_core::ui_proto::UiResponse) {
        let handler = self.lock_tracker().take(response.request_id);
        let Some(handler) = handler else {
            log(format!(
                "收到未知 request_id={} 的响应，忽略",
                response.request_id
            ));
            return;
        };
        let result = if response.is_ok() {
            Ok(response.data.unwrap_or(Value::Null))
        } else {
            Err(response.error.unwrap_or(UiError {
                code: "unknown".to_string(),
                message: "响应缺少错误信息".to_string(),
            }))
        };
        self.post(Box::new(move || handler(result)));
    }

    /// 处理一条推送事件。
    fn on_event(&self, event: UiEvent) {
        let handler = self.lock_event().clone();
        if let Some(handler) = handler {
            self.post(Box::new(move || handler(event)));
        }
    }

    /// 处理一条控制指令。
    fn on_control(&self, control: UiControl) {
        let handler = self.lock_control().clone();
        if let Some(handler) = handler {
            self.post(Box::new(move || handler(control.action)));
        }
    }

    /// 处理连接断开：记录在途请求数（诊断价值：可判断是"握手前断开"还是"业务中失联"），
    /// 并投递断开回调。
    fn on_disconnect(&self, reason: &str) {
        let pending = self.lock_tracker().pending_len();
        log(format!("协议连接断开：{reason}（在途请求 {pending} 条）"));
        let handler = self.lock_disconnect().clone();
        if let Some(handler) = handler {
            let reason = reason.to_string();
            self.post(Box::new(move || handler(reason)));
        }
    }

    fn lock_tracker(&self) -> std::sync::MutexGuard<'_, RequestTracker> {
        self.tracker.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_event(&self) -> std::sync::MutexGuard<'_, Option<EventHandler>> {
        self.event_handler.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_control(&self) -> std::sync::MutexGuard<'_, Option<ControlHandler>> {
        self.control_handler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn lock_disconnect(&self) -> std::sync::MutexGuard<'_, Option<DisconnectHandler>> {
        self.disconnect_handler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// 启动读取线程：独占 `reader`，把每条消息投递回 UI 线程。
///
/// EOF / 解析错误 → 触发断开处理（UI 侧据此优雅退出，而非挂死）。
pub fn spawn_reader<R: BufRead + Send + 'static>(
    mut reader: R,
    client: Arc<UiClient>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("mirrorstar-ui-reader".to_string())
        .spawn(move || loop {
            match read_message(&mut reader) {
                Ok(Some(UiMessage::Response(response))) => client.on_response(response),
                Ok(Some(UiMessage::Event(event))) => client.on_event(event),
                Ok(Some(UiMessage::Control(control))) => client.on_control(control),
                // Hello（重复）/ Request 不应由壳侧发出，忽略以保持前向兼容。
                Ok(Some(_)) => {}
                Ok(None) => {
                    client.on_disconnect("stdin 已关闭（EOF）");
                    break;
                }
                Err(e) => {
                    client.on_disconnect(&format!("读取失败: {e}"));
                    break;
                }
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirrorstar_core::ui_proto::{UiControl, UiResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 内存写端，收集写入的字节。
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 同步投递器：立即在调用线程执行闭包（测试无需事件循环）。
    fn sync_dispatch() -> Dispatcher {
        Arc::new(|task| {
            task();
            true
        })
    }

    fn client_with_sink() -> (Arc<UiClient>, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let client = UiClient::with_writer(Box::new(SharedSink(bytes.clone())), sync_dispatch());
        (client, bytes)
    }

    fn written_lines(bytes: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn tracker_alloc_is_monotonic_and_skips_zero() {
        let mut tracker = RequestTracker::default();
        assert_eq!(tracker.alloc(), 1);
        assert_eq!(tracker.alloc(), 2);
        assert_eq!(tracker.alloc(), 3);
        // u64 回绕后不得产出 0
        tracker.next_id = u64::MAX;
        assert_eq!(tracker.alloc(), 1);
    }

    #[test]
    fn request_id_pairs_with_response() {
        let (client, bytes) = client_with_sink();
        let received = Arc::new(Mutex::new(None));

        let got = received.clone();
        let id = client
            .request("get_config", Value::Null, move |result| {
                *got.lock().unwrap() = Some(result);
            })
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(client.lock_tracker().pending_len(), 1);

        // 请求确实以 NDJSON 写了出去，且带同一 id
        let lines = written_lines(&bytes);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"type\":\"request\""));
        assert!(lines[0].contains("\"request_id\":1"));

        // 匹配 id 的响应回填
        client.on_response(UiResponse::ok(id, serde_json::json!({ "v": 1 })));
        assert_eq!(
            *received.lock().unwrap(),
            Some(Ok(serde_json::json!({ "v": 1 })))
        );
        assert_eq!(client.lock_tracker().pending_len(), 0, "回调取走后不应残留");
    }

    #[test]
    fn error_response_is_delivered_as_ui_error() {
        let (client, _bytes) = client_with_sink();
        let received = Arc::new(Mutex::new(None));
        let got = received.clone();
        let id = client
            .request("set_volume", Value::Null, move |result| {
                *got.lock().unwrap() = Some(result);
            })
            .unwrap();

        client.on_response(UiResponse::error(
            id,
            UiError {
                code: "invalid_argument".to_string(),
                message: "越界".to_string(),
            },
        ));
        let got = received.lock().unwrap().clone().unwrap().unwrap_err();
        assert_eq!(got.code, "invalid_argument");
        assert_eq!(got.message, "越界");
    }

    #[test]
    fn unknown_response_id_is_ignored_without_panic() {
        let (client, _bytes) = client_with_sink();
        client.on_response(UiResponse::ok(999, Value::Null));
        assert_eq!(client.lock_tracker().pending_len(), 0);
    }

    #[test]
    fn event_handler_receives_dispatched_event() {
        let (client, _bytes) = client_with_sink();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        client.set_event_handler(Arc::new(move |event| {
            sink.lock().unwrap().push(event.event);
        }));

        client.on_event(UiEvent {
            event: "wallpaper-added".to_string(),
            data: serde_json::json!({ "id": "a" }),
        });
        assert_eq!(seen.lock().unwrap().as_slice(), ["wallpaper-added"]);
    }

    #[test]
    fn control_handler_receives_action() {
        let (client, _bytes) = client_with_sink();
        let count = Arc::new(AtomicUsize::new(0));
        let sink = count.clone();
        client.set_control_handler(Arc::new(move |action| {
            assert_eq!(action, ControlAction::Exit);
            sink.fetch_add(1, Ordering::SeqCst);
        }));
        client.on_control(UiControl {
            action: ControlAction::Exit,
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disconnect_invokes_handler() {
        let (client, _bytes) = client_with_sink();
        let reasons = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = reasons.clone();
        client.set_disconnect_handler(Arc::new(move |reason| {
            sink.lock().unwrap().push(reason);
        }));

        client.on_disconnect("EOF");
        assert_eq!(reasons.lock().unwrap().len(), 1);
    }

    #[test]
    fn report_visibility_emits_control_message() {
        let (client, bytes) = client_with_sink();
        client.report_visibility(true);
        client.report_visibility(false);
        let lines = written_lines(&bytes);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"action\":\"window_shown\""));
        assert!(lines[1].contains("\"action\":\"window_hidden\""));
    }

    #[test]
    fn validate_hello_accepts_matching_version_and_capabilities() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities: REQUIRED_SERVER_CAPABILITIES,
            client: "mirrorstar-wallpaper".to_string(),
        };
        assert!(validate_hello(&hello).is_ok());
    }

    #[test]
    fn validate_hello_rejects_version_mismatch() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            capabilities: REQUIRED_SERVER_CAPABILITIES,
            client: String::new(),
        };
        let err = validate_hello(&hello).unwrap_err();
        assert!(matches!(err, MirrorStarError::IpcError(_)));
    }

    #[test]
    fn validate_hello_rejects_missing_capabilities() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            capabilities: capability::EVENT_PUSH,
            client: String::new(),
        };
        assert!(validate_hello(&hello).is_err());
    }

    #[test]
    fn read_hello_reads_first_message() {
        let mut input = Vec::new();
        write_message(
            &mut input,
            &UiMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                capabilities: capability::ALL,
                client: "shell".to_string(),
            }),
        )
        .unwrap();
        let mut reader = std::io::Cursor::new(input);
        let hello = read_hello(&mut reader).unwrap();
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn read_hello_rejects_non_hello() {
        let mut input = Vec::new();
        write_message(
            &mut input,
            &UiMessage::Event(UiEvent {
                event: "x".to_string(),
                data: Value::Null,
            }),
        )
        .unwrap();
        let mut reader = std::io::Cursor::new(input);
        assert!(read_hello(&mut reader).is_err());
    }
}
