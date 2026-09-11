//! 开机 / 唤醒解析决策树（设计 §9，DR-12 / DR-18 / DR-21）。
//!
//! 开机只做一次解析、一次 apply（DR-12），本模块输出"本次开机该做什么"的
//! 纯决策，不含任何渲染 / 调度副作用。

/// 开机解析的决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// 恢复上次壁纸（携带其 id；含 Web 豁免的恢复，DR-18）。
    Restore(String),
    /// 推进到下一张（on_boot / 或填充避免空屏 DR-21）。
    Advance,
    /// 无事可做（清理留空，等手动或下次定时）。
    None,
}

/// 开机 / 唤醒解析决策树（严格对照设计 §9）。
///
/// # 参数
/// - `current_exists`：`current_wallpaper_id` 是否仍有效（未删未失效）。
/// - `current_id`：当前壁纸 id（`current_exists == true` 时传入）。
/// - `current_is_web`：当前壁纸是否为 Web（恢复 Web 豁免，DR-18）。
/// - `enabled`：`rotation.enabled`（全局主开关）。
/// - `on_boot`：`rotation.on_boot`（开机 / 唤醒是否"推进下一张"而非仅恢复）。
/// - `unit_enabled`：该调度单元是否参与轮换。
/// - `pool_count`：池过滤后有效张数（用 ≥1 / ≥2 判断）。
///
/// # 决策逻辑
/// - 若 `current_exists`：
///   - 当前是 Web → `Restore(current)`（DR-18 豁免，恒可恢复，不参与推进采样）。
///   - 否则若 `enabled && on_boot && unit_enabled && pool_count >= 2` → `Advance`。
///   - 否则 → `Restore(current)`（无条件基础行为，DR-12，不受 enabled 门控）。
/// - 否则（current 已删 / 无上次壁纸）：
///   - 若 `enabled && unit_enabled && pool_count >= 1` → `Advance`（填充避免空屏，
///     DR-21，不受 on_boot 门控）。
///   - 否则 → `None`（清理留空）。
pub fn resolve_boot(
    current_exists: bool,
    current_id: Option<&str>,
    current_is_web: bool,
    enabled: bool,
    on_boot: bool,
    unit_enabled: bool,
    pool_count: usize,
) -> BootDecision {
    if current_exists {
        if current_is_web {
            // 注意：此处"推进门控全开"判定优先于 Web 分支（P1-3 / DR-18 推进语义，
            // 见 §9「on_boot 推进时跳过 Web 取池内下一张」）。current 为 Web 时同样
            // Advance：`sample_next`（经 filter_candidates）天然跳过 Web，改取池内下一张
            // 非 Web 壁纸，避免开机被手动设的 Web 永久占住。
            if enabled && on_boot && unit_enabled && pool_count >= 2 {
                return BootDecision::Advance;
            }
            // DR-18 Web 豁免仅作用于非推进 Restore 路径：手动设 Web 的延续，恒允许恢复。
            return BootDecision::Restore(current_id.unwrap_or_default().to_string());
        }
        if enabled && on_boot && unit_enabled && pool_count >= 2 {
            // on_boot 门控：推进下一张
            return BootDecision::Advance;
        }
        // DR-12：恢复当前为无条件基础行为
        BootDecision::Restore(current_id.unwrap_or_default().to_string())
    } else if enabled && unit_enabled && pool_count >= 1 {
        // DR-21：current 已删 / 无上次壁纸 → 填充池内下一张，避免开机空屏
        // （不受 on_boot 门控）
        BootDecision::Advance
    } else {
        // 清理留空，等手动或下次定时
        BootDecision::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_current_exists_advances_when_all_gates_open() {
        match resolve_boot(true, Some("cur"), false, true, true, true, 2) {
            BootDecision::Advance => {}
            other => panic!("on_boot 全门控开放应 Advance，实际 {other:?}"),
        }
    }

    #[test]
    fn boot_current_exists_restore_is_unconditional() {
        // DR-12：恢复为无条件基础行为，任一门控关闭都退化为恢复
        for (enabled, on_boot, unit_enabled, pool) in [
            (false, false, false, 0),
            (false, true, true, 5),
            (true, false, true, 5), // on_boot=false
            (true, true, false, 5), // unit_enabled=false
            (true, true, true, 1),  // 池 < 2（DR-14 不轮换）
        ] {
            match resolve_boot(true, Some("cur"), false, enabled, on_boot, unit_enabled, pool) {
                BootDecision::Restore(id) => assert_eq!(id, "cur"),
                other => panic!("应 Restore(cur)，实际 {other:?}（enabled={enabled} on_boot={on_boot} unit={unit_enabled} pool={pool}）"),
            }
        }
    }

    #[test]
    fn boot_web_current_advances_when_gate_open_else_restore() {
        // P1-3 / DR-18：current=Web + 推进门控全开 → Advance（采样跳 Web 取池内下一张）。
        for (enabled, on_boot, unit_enabled, pool) in [(true, true, true, 5)] {
            match resolve_boot(
                true,
                Some("web"),
                true,
                enabled,
                on_boot,
                unit_enabled,
                pool,
            ) {
                BootDecision::Advance => {}
                other => panic!("Web+门控全开应 Advance，实际 {other:?}"),
            }
        }
        // 任一门控关闭 → Restore（DR-18 Web 豁免仅作用于非推进路径）。
        for (enabled, on_boot, unit_enabled, pool) in [
            (false, false, false, 0),
            (false, true, true, 5),
            (true, true, true, 1),
            (true, false, true, 5),
        ] {
            match resolve_boot(
                true,
                Some("web"),
                true,
                enabled,
                on_boot,
                unit_enabled,
                pool,
            ) {
                BootDecision::Restore(id) => assert_eq!(id, "web"),
                other => panic!("门控未全开应 Restore(web)，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn boot_current_deleted_advance_to_fill_without_on_boot_gate() {
        // DR-21：current 已删 + 轮换开 + 单元开 + 池 ≥1 → Advance，不受 on_boot 门控
        for on_boot in [false, true] {
            match resolve_boot(false, None, false, true, on_boot, true, 1) {
                BootDecision::Advance => {}
                other => panic!("应 Advance 填充避免空屏，实际 {other:?}"),
            }
        }
        // 池 = 2 张及以上同样推进
        assert_eq!(
            resolve_boot(false, None, false, true, false, true, 3),
            BootDecision::Advance
        );
    }

    #[test]
    fn boot_current_deleted_none_when_no_fill() {
        // 轮换关 / 单元关 / 池空 → None（清理留空）
        for (enabled, unit_enabled, pool) in [(false, true, 3), (true, false, 3), (true, true, 0)] {
            match resolve_boot(false, None, false, enabled, true, unit_enabled, pool) {
                BootDecision::None => {}
                other => panic!(
                    "应 None，实际 {other:?}（enabled={enabled} unit={unit_enabled} pool={pool}）"
                ),
            }
        }
    }
}
