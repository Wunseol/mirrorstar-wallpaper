//! 多屏排列布局域（DR-2 / 多屏排列顶层逻辑）。
//!
//! 本模块是"多屏排列"的**唯一逻辑归属**：将（排列方式 × 显示器拓扑）以纯函数计算为
//! 布局平面 [`LayoutPlane`]——单元划分、主显示器。调度器 / 命令层均只是该域的消费方，
//! 不再各自实现"编排 → 单元"映射（此前该规则在 `scheduler::reconcile_and_align` 与
//! `commands::rotation::validate_unit_key` 两处重复、零单测，已收敛到本模块）；前端
//! TS 侧 `unitKeysForArrangement` 为独立实现——TS 无法消费 Rust 函数，仅按同一规则
//! 保持语义对齐。
//!
//! ## 纯函数约束
//!
//! 本模块**不依赖 Win32**：[`LayoutMonitor`] 是显示器拓扑的纯数据抽象，由
//! [`crate::config::DisplayInfo`]（`enumerate_displays` 的产物）转换而来。因此
//! `plan` / `unit_keys` / `validate_unit_key` 均可脱离桌面环境单元测试。

use std::collections::HashMap;

use crate::config::settings::Arrangement;
use crate::config::DisplayInfo;

/// 全局 / 全体调度单元 key（`AllSame` / `Span` 编排下的唯一单元；亦作为"全局开关"
/// 的 key，设计 §14）。
pub const ALL_UNIT_KEY: &str = "all";

/// 布局域显示器抽象（与 Win32 解耦，由 `DisplayInfo` 转换而来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutMonitor {
    pub id: String,
    pub is_primary: bool,
}

impl From<&DisplayInfo> for LayoutMonitor {
    fn from(d: &DisplayInfo) -> Self {
        Self {
            id: d.id.clone(),
            is_primary: d.is_primary,
        }
    }
}

/// 布局平面：`arrangement` × 显示器拓扑 → 单元划分 + 主显示器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPlane {
    pub arrangement: Arrangement,
    /// 主显示器 id。
    pub primary: Option<String>,
    /// 显示器拓扑枚举顺序（`PerMonitor` 下即单元顺序，保证确定性迭代）。
    pub display_order: Vec<String>,
    /// unit key → 该单元覆盖的显示器 id 列表。
    pub unit_displays: HashMap<String, Vec<String>>,
}

impl Default for LayoutPlane {
    fn default() -> Self {
        Self {
            arrangement: Arrangement::PerMonitor,
            primary: None,
            display_order: Vec::new(),
            unit_displays: HashMap::new(),
        }
    }
}

/// 布局规则错误（供命令层映射为 `MirrorStarError::InvalidArgument`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// 无显示器可用（`PerMonitor` 缺省 key 时无处回退）。
    NoDisplays,
    /// key 命中不存在的显示器（孤儿单元）。
    OrphanUnit(String),
    /// 全局编排（`AllSame`/`Span`）下显式给出非 `all` key。
    InvalidUnitForArrangement(String),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::NoDisplays => write!(f, "无可用的显示器"),
            LayoutError::OrphanUnit(k) => write!(f, "孤儿单元（显示器不存在）: {k}"),
            LayoutError::InvalidUnitForArrangement(k) => {
                write!(f, "此编排下不允许设置单元: {k}（仅支持全局单元 {ALL_UNIT_KEY}）")
            }
        }
    }
}

/// 计算布局平面：`arrangement` × 显示器拓扑 → 单元划分 + 主显示器。
///
/// - `PerMonitor`：每显示器一个单元（key = 显示器 id）。
/// - `AllSame`：唯一全局单元 `all`，覆盖全部显示器。
/// - `Span`：唯一全局单元 `all`，覆盖全部显示器（虚拟桌面联合矩形由 worker_w.rs 另行计算）。
pub fn plan(arrangement: Arrangement, displays: &[LayoutMonitor]) -> LayoutPlane {
    let primary = displays.iter().find(|d| d.is_primary).map(|d| d.id.clone());
    let display_order: Vec<String> = displays.iter().map(|d| d.id.clone()).collect();
    let mut unit_displays = HashMap::new();

    match arrangement {
        Arrangement::PerMonitor => {
            for d in displays {
                unit_displays.insert(d.id.clone(), vec![d.id.clone()]);
            }
        }
        Arrangement::AllSame | Arrangement::Span => {
            if !displays.is_empty() {
                let ids: Vec<String> = displays.iter().map(|d| d.id.clone()).collect();
                unit_displays.insert(ALL_UNIT_KEY.to_string(), ids);
            }
        }
    }

    LayoutPlane {
        arrangement,
        primary,
        display_order,
        unit_displays,
    }
}

/// 布局平面的单元 key 列表。
///
/// `PerMonitor` 按显示器枚举顺序返回；`AllSame` / `Span` 返回唯一全局单元 `all`
///（无显示器时为 `Vec::new()`）。
pub fn unit_keys(plane: &LayoutPlane) -> Vec<String> {
    match plane.arrangement {
        Arrangement::PerMonitor => plane.display_order.clone(),
        Arrangement::AllSame | Arrangement::Span => {
            if plane.unit_displays.contains_key(ALL_UNIT_KEY) {
                vec![ALL_UNIT_KEY.to_string()]
            } else {
                Vec::new()
            }
        }
    }
}

/// 校验 / 解析单元 key（收敛 DR-40 规则）。
///
/// - `PerMonitor`：`key` 必须命中任一显示器 id；`None` / 空串 → 主显示器（无主则
///   回退首个显示器）。
/// - `AllSame` / `Span`：仅允许全局单元 `"all"`；显式给出其它 key → 拒绝孤儿单元。
pub fn validate_unit_key(
    plane: &LayoutPlane,
    key: Option<&str>,
) -> Result<String, LayoutError> {
    match plane.arrangement {
        Arrangement::PerMonitor => {
            let key = match key {
                Some(k) if !k.is_empty() => k.to_string(),
                _ => plane
                    .primary
                    .clone()
                    .or_else(|| plane.display_order.first().cloned())
                    .ok_or(LayoutError::NoDisplays)?,
            };
            if !plane.unit_displays.contains_key(&key) {
                return Err(LayoutError::OrphanUnit(key));
            }
            Ok(key)
        }
        Arrangement::AllSame | Arrangement::Span => {
            if let Some(k) = key {
                if !k.is_empty() && k != ALL_UNIT_KEY {
                    return Err(LayoutError::InvalidUnitForArrangement(k.to_string()));
                }
            }
            Ok(ALL_UNIT_KEY.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, primary: bool) -> LayoutMonitor {
        LayoutMonitor {
            id: id.to_string(),
            is_primary: primary,
        }
    }

    #[test]
    fn plan_per_monitor_splits_each_display() {
        let displays = vec![monitor("d1", true), monitor("d2", false)];
        let plane = plan(Arrangement::PerMonitor, &displays);
        assert_eq!(plane.primary.as_deref(), Some("d1"));
        assert_eq!(unit_keys(&plane), vec!["d1".to_string(), "d2".to_string()]);
        assert_eq!(plane.unit_displays["d1"], vec!["d1".to_string()]);
        assert_eq!(plane.unit_displays["d2"], vec!["d2".to_string()]);
    }

    #[test]
    fn plan_per_monitor_empty_displays_yields_no_units() {
        let plane = plan(Arrangement::PerMonitor, &[]);
        assert_eq!(unit_keys(&plane), Vec::<String>::new());
        assert_eq!(plane.primary, None);
    }

    #[test]
    fn plan_all_same_yields_global_unit() {
        let displays = vec![monitor("d1", false), monitor("d2", true)];
        let plane = plan(Arrangement::AllSame, &displays);
        assert_eq!(unit_keys(&plane), vec![ALL_UNIT_KEY.to_string()]);
        assert_eq!(plane.unit_displays[ALL_UNIT_KEY], vec!["d1".to_string(), "d2".to_string()]);
    }

    #[test]
    fn plan_span_yields_global_unit() {
        let displays = vec![monitor("d1", true), monitor("d2", false)];
        let plane = plan(Arrangement::Span, &displays);
        assert_eq!(unit_keys(&plane), vec![ALL_UNIT_KEY.to_string()]);
        assert_eq!(plane.unit_displays[ALL_UNIT_KEY], vec!["d1".to_string(), "d2".to_string()]);
    }

    #[test]
    fn plan_span_empty_displays_yields_no_units() {
        let plane = plan(Arrangement::Span, &[]);
        assert_eq!(unit_keys(&plane), Vec::<String>::new());
    }

    #[test]
    fn validate_unit_key_per_monitor_resolves_primary_or_first() {
        let plane =
            plan(Arrangement::PerMonitor, &[monitor("d1", true), monitor("d2", false)]);
        assert_eq!(validate_unit_key(&plane, None).unwrap(), "d1");
        assert_eq!(validate_unit_key(&plane, Some("")).unwrap(), "d1");
        assert_eq!(validate_unit_key(&plane, Some("d2")).unwrap(), "d2");
    }

    #[test]
    fn validate_unit_key_per_monitor_rejects_orphan() {
        let plane = plan(Arrangement::PerMonitor, &[monitor("d1", true)]);
        assert_eq!(
            validate_unit_key(&plane, Some("ghost")),
            Err(LayoutError::OrphanUnit("ghost".to_string()))
        );
    }

    #[test]
    fn validate_unit_key_per_monitor_no_displays_errors() {
        let plane = plan(Arrangement::PerMonitor, &[]);
        assert_eq!(validate_unit_key(&plane, None), Err(LayoutError::NoDisplays));
    }

    #[test]
    fn validate_unit_key_global_only_allows_all() {
        let plane = plan(Arrangement::Span, &[monitor("d1", true)]);
        assert_eq!(validate_unit_key(&plane, None).unwrap(), ALL_UNIT_KEY);
        assert_eq!(validate_unit_key(&plane, Some("all")).unwrap(), ALL_UNIT_KEY);
        assert_eq!(
            validate_unit_key(&plane, Some("d1")),
            Err(LayoutError::InvalidUnitForArrangement("d1".to_string()))
        );
    }
}
