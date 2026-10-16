//! 协议桥：把 `ui_proto` 的请求映射到**现有** `commands::*` 实现（业务逻辑零复制），
//! 并把壳侧 14 个既有事件转发为协议推送。
//!
//! # 命令桥
//!
//! - 命令名与 Tauri 命令名逐字一致（`snake_case`），见 `commands::*` 与 [`alignment.md`]。
//! - 参数键名沿用前端现有 **camelCase**（如 `filePath` / `displayId`，Tauri v2 会自动把
//!   前端 camelCase 反序列化为后端 snake_case 形参）。本模块用 [`Params`] 同时接受
//!   camelCase 与 snake_case 两种键，避免因书写风格差异导致解析失败。
//! - 在非命令上下文复用命令实现：`State<'_, AppState>` 由 `app.state::<AppState>()` 取到，
//!   `AppHandle` 直接 `app.clone()`；异步命令在本模块的异步分发函数内 `.await`。
//!   返回的 `Result<T, MirrorStarError>` 序列化为响应 `data`，错误经
//!   `error_code::code_of` 转成结构化 `UiError`。
//!
//! [`alignment.md`]: ../../../.trae/specs/rewrite-main-window-native-ui/alignment.md
//!
//! # 事件桥
//!
//! 壳侧在各处直接 `app.emit(...)`（共 14 个发射点）。本模块**不逐个改动发射点**，而是
//! 在 setup 中通过 [`install_event_forwarding`] 用 `app.listen(...)` 订阅这 14 个事件名，
//! 再转发为协议推送 [`UiMessage::Event`]。转发为**惰性**：UI 进程未运行时直接返回，
//! 不产生序列化与 IO 开销。

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use mirrorstar_core::ui_proto::{code_of, UiError, UiEvent, UiMessage, UiRequest, UiResponse};
use mirrorstar_core::MirrorStarError;

use super::UiProcessManager;

/// 需要转发到 UI 进程的壳侧事件全集（14 个）。
///
/// 该常量是事件桥的**唯一事实来源**：新增壳侧事件时在此追加一项即可被转发；
/// 测试 [`tests::forwarded_events_cover_alignment_table`] 断言集合完整且无重复。
pub const FORWARDED_EVENTS: &[&str] = &[
    "wallpaper-added",
    "wallpaper-removed",
    "wallpaper-updated",
    "wallpaper-thumbnail-failed",
    "wallpaper-source-missing",
    "wallpaper-state-changed",
    "wallpaper-rotated",
    "wallpaper-loading",
    "wallpaper-gif-oversized",
    "config-changed",
    "config-load-error",
    "desktop-status-changed",
    "audio-disabled",
    // 显示器热插拔（`scheduler.rs` 的 `Why::DisplayCheck` 分支每 3s 轮询，
    // 仅集合真变化时发射）；UI 侧 `store.rs::apply_event` 已消费该事件。
    "displays-changed",
];

/// 订阅全部 [`FORWARDED_EVENTS`] 并转发为协议推送（setup 中调用一次）。
pub fn install_event_forwarding(app: &AppHandle, manager: std::sync::Arc<UiProcessManager>) {
    use tauri::Listener;
    for &event_name in FORWARDED_EVENTS {
        let manager = manager.clone();
        app.listen(event_name, move |event| {
            // 惰性转发：无 UI 进程时零开销、零报错。
            if !manager.is_running() {
                return;
            }
            // 事件负载对齐：Tauri 事件 payload 为 JSON 字符串（`()` 事件为 "null"）；
            // 解析失败按 null 处理，保证转发不因单个事件格式异常而中断。
            let data = serde_json::from_str(event.payload()).unwrap_or(Value::Null);
            let message = UiMessage::Event(UiEvent {
                event: event_name.to_string(),
                data,
            });
            if let Err(e) = manager.send(&message) {
                tracing::warn!(event = event_name, error = %e, "转发事件到 UI 进程失败");
            }
        });
    }
}

/// 处理一条来自 UI 进程的请求，产出响应信封。
pub async fn handle_request(app: &AppHandle, request: &UiRequest) -> UiResponse {
    match dispatch(app, &request.command, &request.params).await {
        Ok(data) => UiResponse::ok(request.request_id, data),
        Err(error) => {
            tracing::warn!(
                command = %request.command,
                code = code_of(&error),
                error = %error,
                "UI 协议命令执行失败"
            );
            // 错误经 `error_code::code_of` 映射为稳定错误码，避免手写漂移。
            UiResponse::error(request.request_id, UiError::from(&error))
        }
    }
}

/// 把命令返回值序列化为响应 `data`（`()` → `null`）。
fn to_json<T: Serialize>(value: T) -> Result<Value, MirrorStarError> {
    serde_json::to_value(value)
        .map_err(|e| MirrorStarError::IpcError(format!("命令响应序列化失败: {}", e)))
}

/// 命令分发：协议命令名 → 现有 `commands::*` 实现。
///
/// 命令名与 `lib.rs::generate_handler!` 中注册的名字逐字一致。
async fn dispatch(
    app: &AppHandle,
    command: &str,
    params: &Value,
) -> Result<Value, MirrorStarError> {
    let p = Params::new(params);
    // `State<'_, AppState>` 在非命令上下文的取法：`app.state::<AppState>()`。
    macro_rules! state {
        () => {
            app.state::<crate::state::AppState>()
        };
    }
    match command {
        // ── config ────────────────────────────────────────────────────────────
        "get_config" => crate::commands::get_config(state!()).and_then(to_json),
        "get_arrangement" => crate::commands::get_arrangement(state!()).and_then(to_json),
        "update_arrangement" => {
            crate::commands::update_arrangement(state!(), p.require("arrangement")?)
                .await
                .and_then(to_json)
        }
        "update_config" => crate::commands::update_config(state!(), p.require("config")?)
            .await
            .and_then(to_json),

        // ── system ────────────────────────────────────────────────────────────
        "get_displays" => crate::commands::get_displays(state!()).and_then(to_json),
        // 诊断命令：新 UI 可不消费，但保持可调用（与命令面 1:1，避免"漏一条"）。
        "get_memory_report" => crate::commands::get_memory_report().and_then(to_json),
        "report_lifecycle_mark" => {
            crate::commands::report_lifecycle_mark(p.require("label")?).and_then(to_json)
        }
        "check_desktop_status" => crate::commands::check_desktop_status(state!())
            .await
            .and_then(to_json),
        "set_interaction_mode" => {
            crate::commands::set_interaction_mode(state!(), p.require("enabled")?)
                .await
                .and_then(to_json)
        }
        // open_file_dialog 依赖 rfd 原生对话框：**不复制到 UI 进程**，经协议复用壳侧实现。
        "open_file_dialog" => crate::commands::open_file_dialog(app.clone())
            .await
            .and_then(to_json),
        "toggle_auto_start" => {
            crate::commands::toggle_auto_start(app.clone(), p.require("enabled")?)
                .await
                .and_then(to_json)
        }
        "get_auto_start_status" => crate::commands::get_auto_start_status(app.clone())
            .await
            .and_then(to_json),

        // ── wallpaper ─────────────────────────────────────────────────────────
        "get_wallpapers" => crate::commands::get_wallpapers(state!()).and_then(to_json),
        "reorder_wallpapers" => {
            crate::commands::reorder_wallpapers(state!(), p.require("ids")?).and_then(to_json)
        }
        "add_wallpaper" => crate::commands::add_wallpaper(
            app.clone(),
            state!(),
            p.require("file_path")?,
            p.get("display_id")?,
        )
        .await
        .and_then(to_json),
        "remove_wallpaper" => crate::commands::remove_wallpaper(
            app.clone(),
            state!(),
            p.require("wallpaper_id")?,
            p.require("delete_file")?,
        )
        .await
        .and_then(to_json),
        "set_wallpaper" => crate::commands::set_wallpaper(
            app.clone(),
            state!(),
            p.require("wallpaper_id")?,
            p.get("display_id")?,
            p.get("scaling_mode")?,
        )
        .await
        .and_then(to_json),
        "pause_wallpaper" => {
            crate::commands::pause_wallpaper(app.clone(), state!(), p.get("display_id")?)
                .await
                .and_then(to_json)
        }
        "resume_wallpaper" => {
            crate::commands::resume_wallpaper(app.clone(), state!(), p.get("display_id")?)
                .await
                .and_then(to_json)
        }
        "set_volume" => {
            crate::commands::set_volume(state!(), p.get("display_id")?, p.require("volume")?)
                .await
                .and_then(to_json)
        }
        "toggle_mute" => crate::commands::toggle_mute(state!(), p.get("display_id")?)
            .await
            .and_then(to_json),
        "get_wallpaper_state" => {
            crate::commands::get_wallpaper_state(state!(), p.get("display_id")?)
                .await
                .and_then(to_json)
        }
        "set_scaling_mode" => {
            crate::commands::set_scaling_mode(state!(), p.get("display_id")?, p.require("mode")?)
                .await
                .and_then(to_json)
        }
        "set_speed" => {
            crate::commands::set_speed(state!(), p.get("display_id")?, p.require("speed")?)
                .await
                .and_then(to_json)
        }

        // ── rotation ──────────────────────────────────────────────────────────
        "get_rotation_config" => crate::commands::get_rotation_config(state!()).and_then(to_json),
        "update_rotation_config" => {
            crate::commands::update_rotation_config(state!(), p.require("config")?)
                .and_then(to_json)
        }
        "list_pools" => crate::commands::list_pools(state!()).and_then(to_json),
        "create_pool" => crate::commands::create_pool(
            state!(),
            p.get("name")?,
            p.get("member_ids")?.unwrap_or_default(),
        )
        .and_then(to_json),
        "update_pool" => crate::commands::update_pool(
            state!(),
            p.require("id")?,
            p.get("name")?,
            p.get("member_ids")?,
        )
        .and_then(to_json),
        "delete_pool" => crate::commands::delete_pool(state!(), p.require("id")?).and_then(to_json),
        "reorder_pools" => {
            crate::commands::reorder_pools(state!(), p.require("ids")?).and_then(to_json)
        }
        "set_active_pool" => {
            crate::commands::set_active_pool(state!(), p.get("key")?, p.get("pool_id")?)
                .and_then(to_json)
        }
        "set_rotation_enabled" => {
            crate::commands::set_rotation_enabled(state!(), p.get("key")?, p.require("enabled")?)
                .and_then(to_json)
        }
        "get_unit_states" => crate::commands::get_unit_states(state!()).and_then(to_json),
        "next_wallpaper" => {
            crate::commands::next_wallpaper(state!(), p.get("key")?).and_then(to_json)
        }
        "prev_wallpaper" => crate::commands::prev_wallpaper(state!(), app.clone(), p.get("key")?)
            .await
            .and_then(to_json),

        other => Err(MirrorStarError::InvalidArgument {
            reason: format!("未知协议命令: {}", other),
        }),
    }
}

/// 协议参数读取器：同时接受 camelCase（前端既有风格）与 snake_case（Rust 形参名）。
struct Params<'a> {
    value: &'a Value,
}

impl<'a> Params<'a> {
    fn new(value: &'a Value) -> Self {
        Self { value }
    }

    /// 按 snake_case 名取原始值：先查同名字段，再查其 camelCase 形式。
    fn raw(&self, name: &str) -> Option<&'a Value> {
        if !self.value.is_object() {
            return None;
        }
        self.value
            .get(name)
            .or_else(|| self.value.get(snake_to_camel(name)))
    }

    /// 取可选参数：字段缺失或为 `null` → `Ok(None)`。
    fn get<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, MirrorStarError> {
        match self.raw(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|e| MirrorStarError::InvalidArgument {
                    reason: format!("参数 `{}` 反序列化失败: {}", name, e),
                }),
        }
    }

    /// 取必需参数：缺失或为 `null` → `InvalidArgument`。
    fn require<T: DeserializeOwned>(&self, name: &str) -> Result<T, MirrorStarError> {
        self.get(name)?
            .ok_or_else(|| MirrorStarError::InvalidArgument {
                reason: format!("缺少必需参数 `{}`", name),
            })
    }
}

/// `snake_case` → `camelCase`（`member_ids` → `memberIds`）。
fn snake_to_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper_next = false;
    for c in name.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn forwarded_events_cover_alignment_table() {
        // 14 个事件：数量、无重复、与壳侧各发射点集合逐字一致。
        assert_eq!(FORWARDED_EVENTS.len(), 14, "事件桥应覆盖 14 个壳侧事件");
        let mut sorted = FORWARDED_EVENTS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 14, "事件名不得重复");

        for expected in [
            "wallpaper-added",
            "wallpaper-removed",
            "wallpaper-updated",
            "wallpaper-thumbnail-failed",
            "wallpaper-source-missing",
            "wallpaper-state-changed",
            "wallpaper-rotated",
            "wallpaper-loading",
            "wallpaper-gif-oversized",
            "config-changed",
            "config-load-error",
            "desktop-status-changed",
            "audio-disabled",
            "displays-changed",
        ] {
            assert!(
                FORWARDED_EVENTS.contains(&expected),
                "事件桥缺少 {}",
                expected
            );
        }
    }

    #[test]
    fn snake_to_camel_converts_multi_segment_names() {
        assert_eq!(snake_to_camel("file_path"), "filePath");
        assert_eq!(snake_to_camel("display_id"), "displayId");
        assert_eq!(snake_to_camel("member_ids"), "memberIds");
        assert_eq!(snake_to_camel("id"), "id");
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        value: u32,
    }

    #[test]
    fn params_accept_camel_case_keys() {
        // 前端既有风格：camelCase 键。
        let params = serde_json::json!({ "filePath": "a.mp4", "displayId": "d1" });
        let p = Params::new(&params);
        assert_eq!(p.require::<String>("file_path").unwrap(), "a.mp4");
        assert_eq!(
            p.get::<String>("display_id").unwrap().as_deref(),
            Some("d1")
        );
    }

    #[test]
    fn params_accept_snake_case_keys() {
        let params = serde_json::json!({ "file_path": "a.mp4" });
        let p = Params::new(&params);
        assert_eq!(p.require::<String>("file_path").unwrap(), "a.mp4");
    }

    #[test]
    fn params_treat_missing_and_null_as_none() {
        let params = serde_json::json!({ "displayId": null });
        let p = Params::new(&params);
        assert_eq!(p.get::<String>("display_id").unwrap(), None);
        assert_eq!(p.get::<String>("wallpaper_id").unwrap(), None);
    }

    #[test]
    fn params_require_missing_yields_invalid_argument() {
        let params = serde_json::json!({});
        let p = Params::new(&params);
        let err = p.require::<String>("file_path").unwrap_err();
        assert!(matches!(err, MirrorStarError::InvalidArgument { .. }));
    }

    #[test]
    fn params_reject_wrong_type() {
        let params = serde_json::json!({ "volume": "loud" });
        let p = Params::new(&params);
        let err = p.require::<f32>("volume").unwrap_err();
        assert!(matches!(err, MirrorStarError::InvalidArgument { .. }));
    }

    #[test]
    fn params_non_object_is_empty() {
        // params 为 null / 数组时不应 panic，读到 None。
        let p = Params::new(&Value::Null);
        assert_eq!(p.get::<String>("any").unwrap(), None);
    }

    #[test]
    fn to_json_serializes_unit_as_null() {
        assert_eq!(to_json(()).unwrap(), Value::Null);
    }

    #[test]
    fn to_json_serializes_struct() {
        let value = to_json(Sample { value: 7 }).unwrap();
        assert_eq!(value["value"], 7);
    }
}
