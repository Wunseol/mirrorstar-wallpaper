//! IPC 模块：命名管道通信层，支持 mpv（视频壁纸）与 wp-proc（网页壁纸）两种子进程协议。
//!
//! ## 文件结构
//!
//! - `client.rs`：`NamedPipeClient<T>` 泛型基类 + `read_line_with_limit` OOM 防护 +
//!   `Backoff` 指数退避。提供 connect / send_line / read_response_line_with_timeout /
//!   disconnect 等通用同步 API（所有方法均为阻塞，调用方需通过 `spawn_blocking` 包裹）。
//! - `mpv_protocol.rs`：`MpvIpcClient` mpv JSON 协议薄封装 + `MpvResponse`。
//! - `wp_proc.rs`：`WpProcIpcClient` wp-proc JSON 协议薄封装 + `WpProcCommand` 枚举 /
//!   `WpProcResponse` / `ResponseStatus`。
//!
//! ## 超时默认值对照表（I-006）
//!
//! 各协议的超时默认值分散在不同文件中，下表汇总便于调用方选择正确入口：
//!
//! | 入口 | 默认值 | 用途 | 使用场景 | 位置 |
//! |------|--------|------|----------|------|
//! | `PLAY_COMMAND_TIMEOUT` | 15s | wp-proc `play` 命令（需等待 WebView2 初始化） | wp-proc `play` 命令（可能触发 WebView2 初始化/运行时下载等资源加载） | `wp_proc.rs:12` |
//! | `send_command` | 5s | mpv 同步命令（含 ack 等待） | mpv 一般命令（如需 ack 的 `set_property`、`apply-preset`）；无需 ack 的命令应改用 `send_command_no_wait` | `mpv_protocol.rs:56-58` |
//! | `get_property` | 2s | mpv 属性查询（调用方期望快速响应） | mpv 属性查询（如 `volume`、`speed`、`pause`），频繁调用需快速返回 | `mpv_protocol.rs:143-145` |
//!
//! **设计说明**：各超时值反映业务语义差异（如 `play` 需等待 WebView2 初始化可能涉及
//! 运行时下载，`get_property` 调用方通常期望快速响应），并非不一致。调整时需跨文件
//! 搜索 `Duration::from_secs` 避免遗漏。

pub mod client;
pub mod mpv_protocol;
pub mod wp_proc;
