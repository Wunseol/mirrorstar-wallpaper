//! 协议命令的返回数据负载类型。
//!
//! 与 [`super::message`] 的信封类型分开：本模块承载"业务命令的返回数据"结构。这类
//! 类型同样是**协议线格式的一部分**，由 Tauri 壳与 `mirrorstar-ui` 进程**共用同一份
//! 定义**，禁止任一侧手写重复结构（避免两端字段漂移）。

use serde::{Deserialize, Serialize};

/// 轮换单元状态（`get_unit_states` 命令的返回元素）。
///
/// 字段名即线上 JSON 键（无 `serde` 重命名），两端序列化 / 反序列化结果一致：
///
/// - `key`：调度单元 key（全局单元 `"all"` 或显示器 id）；
/// - `active_pool`：生效池 id，`None` = "全部"池（DR-35）；
/// - `enabled`：单元轮换开关。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitStateDto {
    pub key: String,
    /// 生效池 id；`None` = "全部"池（DR-35）。
    pub active_pool: Option<String>,
    /// 单元轮换开关。
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线上字段名与结构必须逐字保持（`key` / `active_pool` / `enabled`），
    /// 因该结构与 Tauri 壳 `get_unit_states` 的返回 JSON 绑定。
    #[test]
    fn unit_state_dto_wire_format_is_stable() {
        let dto = UnitStateDto {
            key: "all".to_string(),
            active_pool: Some("p1".to_string()),
            enabled: true,
        };
        let json = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({ "key": "all", "active_pool": "p1", "enabled": true })
        );

        // `active_pool` 为 None 时仍以 `null` 出现（而非省略字段）。
        let dto = UnitStateDto {
            key: "d1".to_string(),
            active_pool: None,
            enabled: false,
        };
        let json = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({ "key": "d1", "active_pool": null, "enabled": false })
        );

        let back: UnitStateDto = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.key, "d1");
        assert_eq!(back.active_pool, None);
        assert!(!back.enabled);
    }
}
