//! UI 进程 ↔ Tauri 壳的进程间协议（NDJSON over stdio）。
//!
//! ## 目的与边界
//!
//! 主窗口 UI 迁移到独立原生子进程（`mirrorstar-ui`）后，UI 进程与 Tauri 壳之间需要
//! 一条基于标准输入输出的行分帧 JSON 通道。本模块是该协议的**唯一定义源**：两端
//! 直接复用同一组类型，禁止任何一侧手写重复结构（避免协议漂移）。
//!
//! ## 分层
//!
//! - [`message`]：握手 / 请求 / 响应 / 推送事件 / 错误，以及统一信封 [`message::UiMessage`]。
//! - [`command_data`]：业务命令的返回数据负载结构（如 `get_unit_states` 的 [`command_data::UnitStateDto`]），
//!   两端共用、无手写重复定义。
//! - [`error_code`]：把 [`crate::MirrorStarError`] 的分类映射为对端可分支处理的稳定错误码。
//! - [`framing`]：把信封写入 `impl Write`、从 `impl BufRead` 读出单条消息的分帧读写器，
//!   以及带超时的 [`framing::MessageReceiver`]。
//!
//! ## 线格式
//!
//! 每行一条 UTF-8 JSON（NDJSON），行尾 `\n`，空行跳过。示例：
//!
//! ```text
//! {"type":"request","request_id":1,"command":"get_wallpapers","params":null}
//! {"type":"response","request_id":1,"status":"ok","data":[]}
//! {"type":"event","event":"wallpaper-added","data":{"id":"a"}}
//! ```

pub mod command_data;
pub mod error_code;
pub mod framing;
pub mod message;

pub use command_data::UnitStateDto;
pub use error_code::code_of;
pub use message::{
    capability, ControlAction, Hello, ResponseStatus, UiControl, UiError, UiEvent, UiMessage,
    UiRequest, UiResponse, PROTOCOL_VERSION,
};
