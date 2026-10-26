//! 协议消息类型：UI 进程与 Tauri 壳共用的唯一协议定义源。
//!
//! 本文件中的全部类型均由 `serde` 派生，两端（`mirrorstar-ui` 与 `src-tauri`）
//! 直接复用同一份定义，禁止任何一侧手写重复结构，避免协议漂移。
//!
//! **兼容性约定**：所有结构体一律**不**使用 `#[serde(deny_unknown_fields)]`，
//! 因此反序列化会忽略未知字段——后续版本新增字段时不会破坏旧端。

use serde::{Deserialize, Serialize};

/// 协议版本号。
///
/// 握手阶段双方交换该值；不一致时由调用方（UI 进程管理器）以明确错误终止并记录日志。
/// 约定：**不兼容变更**（删除字段、改变字段语义、改动消息标签）必须递增该值；
/// 仅新增可选字段无需递增（反序列化容忍未知字段，见模块级说明）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 能力位（capabilities）定义。
///
/// 采用 `u32` 位掩码而非 `Vec<String>`：位运算判断廉价、无堆分配、天然可扩展，
/// 且在握手 JSON 中就是普通整数，两端解析结果确定。**新增能力仅可占用尚未使用的位**，
/// 一经分配不得改变其含义（该值与线上协议绑定）。
pub mod capability {
    /// 空能力集。
    pub const NONE: u32 = 0;
    /// 服务端（Tauri）支持主动推送事件（[`super::UiMessage::Event`]）。
    pub const EVENT_PUSH: u32 = 1 << 0;
    /// 支持同步请求-响应调用（[`super::UiMessage::Request`] / [`super::UiMessage::Response`]）。
    pub const REQUEST_RESPONSE: u32 = 1 << 1;
    /// 支持四类预览弹窗（图片 / 视频 / GIF / 网页）。
    pub const PREVIEW: u32 = 1 << 2;
    /// 支持从资源管理器拖入文件添加壁纸。
    pub const DRAG_DROP: u32 = 1 << 3;
    /// 当前协议定义的全部能力位之并集。
    pub const ALL: u32 = EVENT_PUSH | REQUEST_RESPONSE | PREVIEW | DRAG_DROP;
}

/// 握手消息（双向，进程启动时各发一次、各读一次）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// 发送方实现的协议版本，对应 [`PROTOCOL_VERSION`] 的取值。
    pub protocol_version: u32,
    /// 发送方能力位（[`capability`] 中常量的按位或）。
    pub capabilities: u32,
    /// 发送方角色 / 名称，仅用于日志诊断；缺省为空串。
    #[serde(default)]
    pub client: String,
}

impl Hello {
    /// 是否具备某项能力（`capability` 中的常量；多位置位时要求全部具备）。
    pub fn has(&self, capability: u32) -> bool {
        self.capabilities & capability == capability
    }
}

/// 请求（客户端 → 服务端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiRequest {
    /// 请求标识，由客户端单调自增分配；响应用同一值回填以完成配对。
    pub request_id: u64,
    /// 命令名，与现有 Tauri 命令名逐字一致（如 `get_wallpapers`）。
    pub command: String,
    /// 命令参数。用 [`serde_json::Value`] 承载而非为每个命令定义强类型入参结构：
    /// 命令数量多且各自演进，强类型会把协议文件高频耦合到业务改动。
    /// 字段缺省即 `null`（表示无参数）。
    #[serde(default)]
    pub params: serde_json::Value,
}

/// 响应状态标记。序列化为小写字符串，与 wp-proc 的 `status: "ok" | "error"` 风格一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    /// 成功：数据在 [`UiResponse::data`] 中。
    Ok,
    /// 失败：结构化错误在 [`UiResponse::error`] 中。
    Error,
}

/// 结构化错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiError {
    /// 稳定错误码（见 [`crate::ui_proto::error_code`]），对端据此分支处理。
    /// **一经发布视为协议的一部分，只增不改**。
    pub code: String,
    /// 人类可读信息，可能携带动态细节，仅供展示与日志，不用于分支判断。
    pub message: String,
}

/// 响应（服务端 → 客户端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiResponse {
    /// 与请求相同的标识。
    pub request_id: u64,
    /// 成功 / 失败标记。
    pub status: ResponseStatus,
    /// 成功时的返回数据；失败时省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// 失败时的结构化错误；成功时省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<UiError>,
}

impl UiResponse {
    /// 构造成功响应。
    pub fn ok(request_id: u64, data: serde_json::Value) -> Self {
        Self {
            request_id,
            status: ResponseStatus::Ok,
            data: Some(data),
            error: None,
        }
    }

    /// 构造失败响应。
    pub fn error(request_id: u64, error: UiError) -> Self {
        Self {
            request_id,
            status: ResponseStatus::Error,
            data: None,
            error: Some(error),
        }
    }

    /// 是否为成功响应。
    pub fn is_ok(&self) -> bool {
        self.status == ResponseStatus::Ok
    }
}

/// 主动推送事件（服务端 → 客户端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiEvent {
    /// 事件名，与现有 Tauri 事件名逐字一致（如 `wallpaper-added`）。
    pub event: String,
    /// 事件负载，结构与既有事件 payload 对齐；缺省即 `null`。
    #[serde(default)]
    pub data: serde_json::Value,
}

/// 控制动作。本类型**双向**使用（Task 2.5 / 2.6 的窗口生命周期约定）：
///
/// - 服务端（Tauri 壳）→ 客户端：`Show` / `Hide` / `Toggle` / `Exit`，驱动 UI 进程显隐与退出；
/// - 客户端（UI 进程）→ 服务端：`WindowHidden` / `WindowShown`，告知壳侧主窗口当前可见性，
///   供壳侧空闲退出计时（据此判定"窗口隐藏后超过阈值"）。
///
/// 之所以把显隐做成独立控制消息而非普通 `Request`：`Request` 的语义是"UI 请求壳侧执行业务
/// 命令"（客户端 → 服务端单向），窗口显隐是壳侧对 UI 的驱动，方向相反；用独立控制信封
/// 可保持两个方向语义清晰、不与 36 个业务命令混在一起。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// 服务端 → 客户端：显示主窗口并置前聚焦（用户主动唤起路径；跨进程聚焦受 Windows
    /// 前台锁限制，失败仅记日志、不阻断显示）。
    Show,
    /// 服务端 → 客户端：隐藏主窗口（不改变前台）。
    Hide,
    /// 服务端 → 客户端：按主窗口**可见性**切换——可见且未最小化则隐藏；最小化则还原并
    /// 置前聚焦；已隐藏则显示并置前聚焦。判定不查询 `GetForegroundWindow`（点击托盘会改变
    /// 系统前台，跨进程前台判定必然失真）。
    Toggle,
    /// 服务端 → 客户端：请求 UI 进程正常退出（空闲退出 / 应用退出）。
    Exit,
    /// 客户端 → 服务端：主窗口已隐藏（开始空闲计时）。
    WindowHidden,
    /// 客户端 → 服务端：主窗口已显示（清除空闲计时）。
    WindowShown,
}

/// 控制指令信封负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiControl {
    /// 控制动作。
    pub action: ControlAction,
}

/// 统一消息信封：两端唯一的一读一写入口。
///
/// 采用**内部标签**（`#[serde(tag = "type")]`），理由：
/// - 外部标签（serde 默认）会生成 `{"Request":{...}}`，冗长且对非 Rust 端不友好；
/// - 邻接标签（`tag` + `content`）会生成 `{"type":"request","content":{...}}`，多一层嵌套；
/// - 内部标签生成扁平结构 `{"type":"request", ...}`，与现有 wp-proc 的
///   `{"command":"play", ...}` 家族风格一致，人工排查日志也更直观。
///
/// 代价：内部标签不支持 `deny_unknown_fields`（本项目本就要求容忍未知字段），
/// 且反序列化时需缓冲一次负载（消息体量小，可接受）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiMessage {
    /// 协议握手。
    Hello(Hello),
    /// 请求（客户端 → 服务端）。
    Request(UiRequest),
    /// 响应（服务端 → 客户端）。
    Response(UiResponse),
    /// 主动推送事件（服务端 → 客户端）。
    Event(UiEvent),
    /// 控制指令（双向：服务端驱动显隐/退出；客户端上报窗口可见性）。
    Control(UiControl),
}
