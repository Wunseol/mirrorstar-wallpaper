//! 设置面板 / 轮换池管理的**纯逻辑**（无 Slint、无 IO，可无 GUI 单测）。
//!
//! Task 6 / Task 7 的界面只是一层渲染；真正容易出错的是"枚举 ↔ 索引 ↔ 文案"映射、
//! 播放按钮状态机、read-modify-write 的串行化与池成员的去重/重排。把这些抽成纯函数后，
//! 都能在无事件循环的环境下直接断言（见文件末尾单测），与本 crate 既有的 `grid` / `dnd`
//! 保持同一风格。
//!
//! ## 为什么把"写回串行化"也放这里
//!
//! 旧前端用模块级 promise 链（`patchConfig` / `patchRotation`）串行化
//! `get → merge → update`，避免并发 read-modify-write 互相覆盖（`alignment.md` 高风险 #10）。
//! 原生 UI 没有 promise 链，但同一条不变式依然成立。这里用 [`PatchQueue`] 表达"一次只允许
//! 一条在途、其余排队"的语义，队列本身与协议解耦，便于单测覆盖。

use std::collections::VecDeque;

use mirrorstar_core::config::settings::{Arrangement, FullscreenAction, Order};
use mirrorstar_core::config::DisplayInfo;
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperState};
use serde_json::Value;

// ── 枚举选项表（索引 ↔ 值 ↔ 中文标签）────────────────────────────────────────

/// 缩放模式（`ScalingMode` 线格式；设置面板下拉选项顺序与此一致）。
pub const SCALING_MODES: [&str; 5] = ["fill", "fit", "stretch", "tile", "center"];
pub const SCALING_LABELS: [&str; 5] = ["填充", "适应", "拉伸", "平铺", "居中"];

/// 多屏排列（顶层布局策略）。
pub const ARRANGEMENTS: [&str; 3] = ["per_monitor", "all_same", "span"];
pub const ARRANGEMENT_LABELS: [&str; 3] = ["每屏独立", "每屏同图", "跨屏合并"];

/// 全屏处置策略（顺序：terminate/pause/none）。
pub const FULLSCREEN_ACTIONS: [&str; 3] = ["terminate", "pause", "none"];
pub const FULLSCREEN_LABELS: [&str; 3] =
    ["释放（终止进程）", "暂停（进程驻留）", "持续（保持播放）"];

/// 切换顺序（线格式值不变，仅中文标签去术语化）。
pub const ORDERS: [&str; 3] = ["sequential", "shuffle_bag", "pseudo_random"];
pub const ORDER_LABELS: [&str; 3] = ["顺序循环", "随机（不重复）", "随机（可重复）"];

/// 「切换顺序」各选项的说明文案（设置面板下拉下方展示，索引与 [`ORDERS`] 一致）。
pub const ORDER_HINTS: [&str; 3] = [
    "按主窗口卡片顺序依次切换，一轮播完从头开始。",
    "打乱顺序播放，同一轮内不重复；一轮播完重新打乱。",
    "每次完全随机，可能连续出现同一张。",
];

/// `Order` → 说明文案。
pub fn order_hint(order: Order) -> &'static str {
    let index = index_of(&ORDERS, order_value(order));
    ORDER_HINTS
        .get(index as usize)
        .copied()
        .unwrap_or(ORDER_HINTS[0])
}

/// 在选项表中查值对应的索引；未命中返回 -1（Slint 侧据此显示占位）。
pub fn index_of(table: &[&str], value: &str) -> i32 {
    table
        .iter()
        .position(|v| *v == value)
        .map(|i| i as i32)
        .unwrap_or(-1)
}

/// 索引 → 值；越界返回 `None`。
pub fn value_at(table: &[&'static str], index: i32) -> Option<&'static str> {
    if index < 0 {
        return None;
    }
    table.get(index as usize).copied()
}

/// `ScalingMode` → 线格式值。
pub fn scaling_value(mode: ScalingMode) -> &'static str {
    match mode {
        ScalingMode::Fill => "fill",
        ScalingMode::Fit => "fit",
        ScalingMode::Stretch => "stretch",
        ScalingMode::Tile => "tile",
        ScalingMode::Center => "center",
    }
}

/// 线格式值 → `ScalingMode`（供下拉回调窄化）。
pub fn scaling_from_value(value: &str) -> Option<ScalingMode> {
    match value {
        "fill" => Some(ScalingMode::Fill),
        "fit" => Some(ScalingMode::Fit),
        "stretch" => Some(ScalingMode::Stretch),
        "tile" => Some(ScalingMode::Tile),
        "center" => Some(ScalingMode::Center),
        _ => None,
    }
}

/// `ScalingMode` → 选项表索引。
pub fn scaling_index(mode: ScalingMode) -> i32 {
    index_of(&SCALING_MODES, scaling_value(mode))
}

/// `Arrangement` → 选项表索引。
pub fn arrangement_index(arrangement: Arrangement) -> i32 {
    index_of(&ARRANGEMENTS, arrangement_value(arrangement))
}

/// `Arrangement` → 线格式值。
pub fn arrangement_value(arrangement: Arrangement) -> &'static str {
    match arrangement {
        Arrangement::PerMonitor => "per_monitor",
        Arrangement::AllSame => "all_same",
        Arrangement::Span => "span",
    }
}

/// `Arrangement` → 设置面板中文标签（与 [`ARRANGEMENT_LABELS`] 同源）。
pub fn arrangement_label(arrangement: Arrangement) -> &'static str {
    let index = index_of(&ARRANGEMENTS, arrangement_value(arrangement));
    ARRANGEMENT_LABELS
        .get(index as usize)
        .copied()
        .unwrap_or(ARRANGEMENT_LABELS[0])
}

/// 线格式值 → `Arrangement`（供下拉回调窄化）。
pub fn arrangement_from_value(value: &str) -> Option<Arrangement> {
    match value {
        "per_monitor" => Some(Arrangement::PerMonitor),
        "all_same" => Some(Arrangement::AllSame),
        "span" => Some(Arrangement::Span),
        _ => None,
    }
}

/// `FullscreenAction` → 选项表索引。
pub fn fullscreen_index(action: FullscreenAction) -> i32 {
    index_of(&FULLSCREEN_ACTIONS, fullscreen_value(action))
}

/// `FullscreenAction` → 线格式值。
pub fn fullscreen_value(action: FullscreenAction) -> &'static str {
    match action {
        FullscreenAction::Terminate => "terminate",
        FullscreenAction::Pause => "pause",
        FullscreenAction::None => "none",
    }
}

/// 线格式值 → `FullscreenAction`。
pub fn fullscreen_from_value(value: &str) -> Option<FullscreenAction> {
    match value {
        "terminate" => Some(FullscreenAction::Terminate),
        "pause" => Some(FullscreenAction::Pause),
        "none" => Some(FullscreenAction::None),
        _ => None,
    }
}

/// `Order` → 选项表索引。
pub fn order_index(order: Order) -> i32 {
    index_of(&ORDERS, order_value(order))
}

/// `Order` → 线格式值。
pub fn order_value(order: Order) -> &'static str {
    match order {
        Order::Sequential => "sequential",
        Order::ShuffleBag => "shuffle_bag",
        Order::PseudoRandom => "pseudo_random",
    }
}

/// `Order` → 设置面板中文标签（与 [`ORDER_LABELS`] 同源）。
pub fn order_label(order: Order) -> &'static str {
    let index = index_of(&ORDERS, order_value(order));
    ORDER_LABELS
        .get(index as usize)
        .copied()
        .unwrap_or(ORDER_LABELS[0])
}

/// 线格式值 → `Order`。
pub fn order_from_value(value: &str) -> Option<Order> {
    match value {
        "sequential" => Some(Order::Sequential),
        "shuffle_bag" => Some(Order::ShuffleBag),
        "pseudo_random" => Some(Order::PseudoRandom),
        _ => None,
    }
}

// ── 播放按钮状态机（照搬 main.ts::updatePlaybackButtons）──────────────────────

/// 播放控制三键的呈现与分流状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackButtons {
    /// 暂停/恢复键是否可点。
    pub pause_enabled: bool,
    /// 暂停/恢复键文案。
    pub pause_label: &'static str,
    /// 点击语义：`"pause"` 或 `"resume"`。
    pub pause_action: &'static str,
    /// 上一张是否可点。
    pub prev_enabled: bool,
    /// 下一张是否可点。
    pub next_enabled: bool,
}

/// 依据播放状态与"是否有可用显示器"推导三键状态。
///
/// 状态规则（与 `src/scripts/main.ts:111-168` 逐条一致）：
/// - 无显示器：三键全部禁用（对齐 `display-list.ts::disablePlaybackControls`）。
/// - `None`（无活跃壁纸）：三键均可用，暂停键语义为 pause。
/// - `Playing`：暂停可用、文案"暂停"，prev/next 可用。
/// - `Paused`：暂停可用、文案"恢复"（单键切换），prev/next 可用。
/// - `Terminated`：暂停禁用、文案"恢复中..."，prev/next 仍可用。
/// - 其他（`Initializing`）：暂停禁用、文案"暂停"，prev/next 可用。
pub fn playback_buttons(state: Option<WallpaperState>, has_display: bool) -> PlaybackButtons {
    if !has_display {
        return PlaybackButtons {
            pause_enabled: false,
            pause_label: PAUSE_LABEL,
            pause_action: "pause",
            prev_enabled: false,
            next_enabled: false,
        };
    }
    let (pause_enabled, pause_label, pause_action) = match state {
        None => (true, PAUSE_LABEL, "pause"),
        Some(WallpaperState::Playing) => (true, PAUSE_LABEL, "pause"),
        Some(WallpaperState::Paused) => (true, RESUME_LABEL, "resume"),
        Some(WallpaperState::Terminated) => (false, RESUMING_LABEL, "pause"),
        Some(WallpaperState::Initializing) => (false, PAUSE_LABEL, "pause"),
    };
    PlaybackButtons {
        pause_enabled,
        pause_label,
        pause_action,
        prev_enabled: true,
        next_enabled: true,
    }
}

const PAUSE_LABEL: &str = "暂停";
const RESUME_LABEL: &str = "恢复";
const RESUMING_LABEL: &str = "恢复中...";

// ── 深合并（复刻 config-panel.ts 的 patch 语义）───────────────────────────────

/// 把 `patch` 递归合并进 `base`：对象逐键递归，标量/数组则整段替换（后者胜）。
///
/// 用于 `patchConfig` 的 `get_config → merge → update_config`：
/// 每个 section 只覆盖显式给出的字段，未给出的保持不变。
pub fn merge_json(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, patch_value) in patch_map {
                match base_map.get_mut(key) {
                    Some(slot) => merge_json(slot, patch_value),
                    None => {
                        base_map.insert(key.clone(), patch_value.clone());
                    }
                }
            }
        }
        (base_slot, patch_value) => *base_slot = patch_value.clone(),
    }
}

// ── 串行化写回队列 ────────────────────────────────────────────────────────────

/// "一次只允许一条在途、其余 FIFO 排队"的写回队列。
///
/// 语义对应旧前端的 `patchChain` / `rotationChain`：并发提交会因
/// `get → merge → update` 交错而互相覆盖，故必须串行。
#[derive(Debug)]
pub struct PatchQueue<T> {
    /// 是否有请求在途（已提交、等待响应）。
    running: bool,
    /// 待提交项（FIFO）。
    pending: VecDeque<T>,
}

impl<T> Default for PatchQueue<T> {
    fn default() -> Self {
        Self {
            running: false,
            pending: VecDeque::new(),
        }
    }
}

impl<T> PatchQueue<T> {
    /// 入队一条写回意图。
    ///
    /// 返回 `true` 表示此前队列空闲、调用方应立即 [`PatchQueue::begin_next`] 启动泵；
    /// 返回 `false` 表示已有在途请求，本次仅排队（由在途请求的完成回调续投）。
    pub fn enqueue(&mut self, item: T) -> bool {
        self.pending.push_back(item);
        !self.running
    }

    /// 若当前空闲且有待办，取出一条并标记为在途；否则返回 `None`。
    pub fn begin_next(&mut self) -> Option<T> {
        if self.running {
            return None;
        }
        let item = self.pending.pop_front()?;
        self.running = true;
        Some(item)
    }

    /// 标记在途项完成；返回是否仍有待办（调用方据此决定是否继续泵）。
    pub fn finish(&mut self) -> bool {
        self.running = false;
        !self.pending.is_empty()
    }
}

// ── 池成员去重 / 重排（对齐 rotation.ts 与后端 Pool::validate DR-24）──────────

/// 成员 id 去重，保留首次出现顺序。
pub fn dedupe_members(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if seen.insert(id.clone()) {
            out.push(id.clone());
        }
    }
    out
}

/// 把 `from` 位置的元素移动到 `to` 位置；越界或原位返回原序列副本。
pub fn reorder_members(ids: &[String], from: usize, to: usize) -> Vec<String> {
    if from >= ids.len() || to >= ids.len() || from == to {
        return ids.to_vec();
    }
    let mut next = ids.to_vec();
    let moved = next.remove(from);
    next.insert(to, moved);
    next
}

// ── 调度单元 key（对齐 mirrorstar_core::layout 单元规则 / backend DR-40）──

/// 调度单元（key + 展示标签）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitKey {
    pub key: String,
    pub label: String,
}

/// 依据多屏排列列出调度单元：PerMonitor 每屏一个，AllSame/Span 唯全局单元 `"all"`。
///
/// 单元划分规则（key 列表）由 `mirrorstar_core::layout` 的 [`plan`] / [`unit_keys`]
/// 唯一提供，本函数只负责拼装展示标签（`[主] ` 前缀 / 显示器名称 / 全局单元文案）。
///
/// [`plan`]: mirrorstar_core::layout::plan
/// [`unit_keys`]: mirrorstar_core::layout::unit_keys
pub fn unit_keys_for_arrangement(
    arrangement: Arrangement,
    displays: &[DisplayInfo],
) -> Vec<UnitKey> {
    let monitors: Vec<mirrorstar_core::layout::LayoutMonitor> =
        displays.iter().map(Into::into).collect();
    let plane = mirrorstar_core::layout::plan(arrangement, &monitors);
    let keys = mirrorstar_core::layout::unit_keys(&plane);
    // 展示层兜底：全局编排下 core 在无显示器时返回空列表，但设置面板仍需展示唯一全局单元
    // （否则单元列表变空、开关无处可操作）；PerMonitor 无显示器则仍返回空（显示空态）。
    let keys = if keys.is_empty() && arrangement != Arrangement::PerMonitor {
        vec![mirrorstar_core::layout::ALL_UNIT_KEY.to_string()]
    } else {
        keys
    };
    keys.into_iter()
        .map(|key| {
            let label = match arrangement {
                // PerMonitor：展示层只补 `[主] ` 前缀与显示器名称。
                Arrangement::PerMonitor => match displays.iter().find(|d| d.id == key) {
                    Some(d) if d.is_primary => format!("[主] {}", d.name),
                    Some(d) => d.name.clone(),
                    // 理论上不会命中（key 来自同一显示器拓扑）；退化时直接显示 key。
                    None => key.clone(),
                },
                Arrangement::AllSame | Arrangement::Span => "全部屏幕".to_string(),
            };
            UnitKey { key, label }
        })
        .collect()
}

// ── 数值 sanitize ─────────────────────────────────────────────────────────────

/// `[0,1]` 浮点音量 → 整数百分比（四舍五入）。
pub fn volume_percent(volume: f32) -> i32 {
    (volume.clamp(0.0, 1.0) * 100.0).round() as i32
}

/// 播放速度滑块量程下限（单一来源见 `mirrorstar_core::ipc::mpv_protocol`）。
pub const SPEED_MIN: f32 = mirrorstar_core::ipc::mpv_protocol::SPEED_MIN;
/// 播放速度滑块量程上限。
pub const SPEED_MAX: f32 = mirrorstar_core::ipc::mpv_protocol::SPEED_MAX;

/// 播放速度 sanitize：NaN → 1.0，越界 clamp 到 `[SPEED_MIN, SPEED_MAX]`（滑块量程）。
pub fn sanitize_speed(speed: f32) -> f32 {
    if speed.is_nan() {
        return 1.0;
    }
    speed.clamp(SPEED_MIN, SPEED_MAX)
}

/// 速度 → 滑块归一化位置 `[0,1]`（把配置速度投影到滑块）。
pub fn speed_to_slider(speed: f32) -> f32 {
    (sanitize_speed(speed) - SPEED_MIN) / (SPEED_MAX - SPEED_MIN)
}

/// 滑块归一化位置 `[0,1]` → 速度（把滑块值提交为配置速度）。
pub fn slider_to_speed(slider: f32) -> f32 {
    sanitize_speed(SPEED_MIN + slider.clamp(0.0, 1.0) * (SPEED_MAX - SPEED_MIN))
}

/// 轮换间隔 sanitize：`< 1` 一律修正为 1（DR-16 下限）。
pub fn sanitize_interval(minutes: i32) -> i32 {
    minutes.max(1)
}

// ── 事件 → Toast 文案（Task 6.3 / 7.4）────────────────────────────────────────

/// Toast 级别（与 `ui/main.slint` 的 `toast-level` 一一对应）。
pub const TOAST_INFO: i32 = 0;
pub const TOAST_SUCCESS: i32 = 1;
pub const TOAST_WARNING: i32 = 2;
pub const TOAST_ERROR: i32 = 3;

/// 结构化 Toast 内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub level: i32,
    pub text: String,
}

/// 提示类事件 → Toast；不产生 Toast 的事件返回 `None`。
///
/// 覆盖 `alignment.md` 表 2 中需要提示的事件（`wallpaper-rotated` 因需防抖合并在
/// `main` 侧单独处理，不在此列）。文案与旧前端 `main.ts` 保持一致。
pub fn event_toast(event: &str, data: &Value) -> Option<Toast> {
    match event {
        "wallpaper-loading" => data
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| Toast {
                level: TOAST_INFO,
                text: m.to_string(),
            }),
        "wallpaper-gif-oversized" => Some(Toast {
            level: TOAST_WARNING,
            text: "GIF 分辨率过高，播放可能不流畅".to_string(),
        }),
        "wallpaper-thumbnail-failed" => {
            data.get("file_path")
                .and_then(Value::as_str)
                .map(|path| Toast {
                    level: TOAST_ERROR,
                    text: format!("缩略图生成失败：{}", file_name(path)),
                })
        }
        "wallpaper-source-missing" => {
            data.get("file_path")
                .and_then(Value::as_str)
                .map(|path| Toast {
                    level: TOAST_INFO,
                    text: format!("源文件缺失：{}", file_name(path)),
                })
        }
        "config-load-error" => Some(Toast {
            level: TOAST_ERROR,
            text: data
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("配置加载失败，使用默认配置")
                .to_string(),
        }),
        "audio-disabled" => Some(Toast {
            level: TOAST_INFO,
            text: "未检测到音频设备，视频壁纸将无声播放".to_string(),
        }),
        _ => None,
    }
}

/// 取路径末段作为显示名（兼容 `/` 与 `\`）。
pub(crate) fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn option_table_round_trips() {
        for (i, value) in SCALING_MODES.iter().enumerate() {
            assert_eq!(index_of(&SCALING_MODES, value), i as i32);
            assert_eq!(value_at(&SCALING_MODES, i as i32), Some(*value));
        }
        assert_eq!(index_of(&SCALING_MODES, "bogus"), -1);
        assert_eq!(value_at(&SCALING_MODES, -1), None);
        assert_eq!(value_at(&SCALING_MODES, 99), None);
        // 标签表长度与值表一致（防止两表漂移）
        assert_eq!(SCALING_MODES.len(), SCALING_LABELS.len());
        assert_eq!(ARRANGEMENTS.len(), ARRANGEMENT_LABELS.len());
        assert_eq!(FULLSCREEN_ACTIONS.len(), FULLSCREEN_LABELS.len());
        assert_eq!(ORDERS.len(), ORDER_LABELS.len());
        assert_eq!(ORDERS.len(), ORDER_HINTS.len());
    }

    #[test]
    fn order_hint_follows_variant() {
        assert_eq!(order_hint(Order::Sequential), ORDER_HINTS[0]);
        assert_eq!(order_hint(Order::ShuffleBag), ORDER_HINTS[1]);
        assert_eq!(order_hint(Order::PseudoRandom), ORDER_HINTS[2]);
    }

    #[test]
    fn arrangement_and_order_mapping() {
        assert_eq!(arrangement_index(Arrangement::PerMonitor), 0);
        assert_eq!(arrangement_index(Arrangement::AllSame), 1);
        assert_eq!(arrangement_index(Arrangement::Span), 2);
        assert_eq!(arrangement_from_value("span"), Some(Arrangement::Span));
        assert_eq!(arrangement_from_value("bogus"), None);

        assert_eq!(order_index(Order::Sequential), 0);
        assert_eq!(order_index(Order::ShuffleBag), 1);
        assert_eq!(order_index(Order::PseudoRandom), 2);
        assert_eq!(order_from_value("pseudo_random"), Some(Order::PseudoRandom));
        assert_eq!(order_from_value("bogus"), None);

        // fullscreen 顺序与设置面板选项顺序一致：terminate/pause/none
        assert_eq!(fullscreen_index(FullscreenAction::Terminate), 0);
        assert_eq!(fullscreen_index(FullscreenAction::Pause), 1);
        assert_eq!(fullscreen_index(FullscreenAction::None), 2);
        assert_eq!(
            fullscreen_from_value("pause"),
            Some(FullscreenAction::Pause)
        );
        assert_eq!(fullscreen_from_value("bogus"), None);
    }

    #[test]
    fn scaling_mode_mapping_round_trips() {
        // 表驱动：线格式值 → 枚举 → 值 + 索引 三方往返一致。
        for (i, value) in SCALING_MODES.iter().enumerate() {
            let mode = scaling_from_value(value).expect("线格式值应可解析为 ScalingMode");
            assert_eq!(scaling_value(mode), *value);
            assert_eq!(scaling_index(mode), i as i32);
        }
        // 未知线格式值 → None（下拉回调据此拒绝非法输入）。
        assert_eq!(scaling_from_value("bogus"), None);
    }

    #[test]
    fn playback_buttons_without_display_all_disabled() {
        let b = playback_buttons(Some(WallpaperState::Playing), false);
        assert!(!b.pause_enabled && !b.prev_enabled && !b.next_enabled);
    }

    #[test]
    fn playback_buttons_none_state_all_clickable() {
        let b = playback_buttons(None, true);
        assert!(b.pause_enabled && b.prev_enabled && b.next_enabled);
        assert_eq!(b.pause_label, "暂停");
        assert_eq!(b.pause_action, "pause");
    }

    #[test]
    fn playback_buttons_playing_paused_terminated_initializing() {
        let playing = playback_buttons(Some(WallpaperState::Playing), true);
        assert!(playing.pause_enabled);
        assert_eq!(playing.pause_label, "暂停");
        assert_eq!(playing.pause_action, "pause");
        assert!(playing.prev_enabled && playing.next_enabled);

        let paused = playback_buttons(Some(WallpaperState::Paused), true);
        assert!(paused.pause_enabled);
        assert_eq!(paused.pause_label, "恢复");
        assert_eq!(paused.pause_action, "resume");

        let terminated = playback_buttons(Some(WallpaperState::Terminated), true);
        assert!(!terminated.pause_enabled, "Terminated 时暂停键应禁用");
        assert_eq!(terminated.pause_label, "恢复中...");
        assert!(terminated.prev_enabled && terminated.next_enabled);

        let init = playback_buttons(Some(WallpaperState::Initializing), true);
        assert!(!init.pause_enabled, "Initializing 时暂停键应禁用");
        assert_eq!(init.pause_label, "暂停");
        assert!(init.prev_enabled && init.next_enabled);
    }

    #[test]
    fn merge_json_deep_merges_objects_and_replaces_scalars() {
        let mut base = json!({
            "audio": { "volume": 0.8, "muted": false },
            "pause": { "fullscreen_action": "terminate", "pause_on_battery": false },
            "arrangement": "per_monitor"
        });
        merge_json(&mut base, &json!({ "pause": { "pause_on_battery": true } }));
        assert_eq!(base["pause"]["pause_on_battery"], true);
        assert_eq!(base["pause"]["fullscreen_action"], "terminate");
        assert_eq!(base["audio"]["volume"], 0.8);

        // 标量整段替换
        merge_json(&mut base, &json!({ "arrangement": "span" }));
        assert_eq!(base["arrangement"], "span");

        // 新键插入
        merge_json(&mut base, &json!({ "video": { "speed": 1.5 } }));
        assert_eq!(base["video"]["speed"], 1.5);
    }

    #[test]
    fn patch_queue_serializes_one_at_a_time() {
        let mut q: PatchQueue<i32> = PatchQueue::default();
        assert!(q.enqueue(1), "空闲时入队应立即启动");
        assert_eq!(q.begin_next(), Some(1), "取走首条并置为在途");
        assert_eq!(q.begin_next(), None, "在途时不得再取");

        // 在途期间入队只排队（enqueue 返回 false 表示未启动新泵）
        assert!(!q.enqueue(2));
        assert!(!q.enqueue(3));

        // 完成当前 → 仍有待办 → 取下一条
        assert!(q.finish());
        assert_eq!(q.begin_next(), Some(2));
        assert!(q.finish());
        assert_eq!(q.begin_next(), Some(3));
        assert!(!q.finish(), "队列耗尽后 finish 返回 false");
        assert_eq!(q.begin_next(), None);
    }

    #[test]
    fn dedupe_and_reorder_members() {
        let ids = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ];
        assert_eq!(dedupe_members(&ids), vec!["a", "b", "c"]);

        let base = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(reorder_members(&base, 0, 2), vec!["b", "c", "a"]);
        assert_eq!(reorder_members(&base, 2, 0), vec!["c", "a", "b"]);
        // 原位 / 越界 → 原序列副本
        assert_eq!(reorder_members(&base, 1, 1), base);
        assert_eq!(reorder_members(&base, 5, 0), base);
    }

    fn display(id: &str, name: &str, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: id.to_string(),
            name: name.to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            is_primary: primary,
            dpi: 96,
        }
    }

    #[test]
    fn unit_keys_follow_arrangement() {
        let displays = vec![
            display("d1", "显示器 1", true),
            display("d2", "显示器 2", false),
        ];
        let per = unit_keys_for_arrangement(Arrangement::PerMonitor, &displays);
        assert_eq!(per.len(), 2);
        assert_eq!(
            per[0],
            UnitKey {
                key: "d1".into(),
                label: "[主] 显示器 1".into()
            }
        );
        assert_eq!(
            per[1],
            UnitKey {
                key: "d2".into(),
                label: "显示器 2".into()
            }
        );

        let all = unit_keys_for_arrangement(Arrangement::AllSame, &displays);
        assert_eq!(
            all,
            vec![UnitKey {
                key: "all".into(),
                label: "全部屏幕".into()
            }]
        );
        let span = unit_keys_for_arrangement(Arrangement::Span, &[]);
        assert_eq!(
            span,
            vec![UnitKey {
                key: "all".into(),
                label: "全部屏幕".into()
            }]
        );
        // 全局编排无显示器：core 返回空 → UI 展示层兜底为唯一全局单元（两种全局编排一致）。
        assert_eq!(
            unit_keys_for_arrangement(Arrangement::AllSame, &[]),
            vec![UnitKey {
                key: "all".into(),
                label: "全部屏幕".into()
            }]
        );

        // PerMonitor 但无显示器 → 空（单元面板据此显示空态，播放控件禁用）
        assert!(unit_keys_for_arrangement(Arrangement::PerMonitor, &[]).is_empty());
    }

    #[test]
    fn numeric_sanitizers() {
        assert_eq!(volume_percent(0.799), 80);
        assert_eq!(volume_percent(1.5), 100);
        assert_eq!(volume_percent(-0.1), 0);

        assert_eq!(sanitize_speed(f32::NAN), 1.0);
        assert_eq!(sanitize_speed(0.0), 0.25);
        assert_eq!(sanitize_speed(99.0), 4.0);
        assert_eq!(sanitize_speed(1.5), 1.5);

        assert_eq!(sanitize_interval(0), 1);
        assert_eq!(sanitize_interval(-3), 1);
        assert_eq!(sanitize_interval(30), 30);
    }

    #[test]
    fn event_toast_maps_known_events() {
        let t = event_toast("wallpaper-loading", &json!({ "message": "正在初始化" })).unwrap();
        assert_eq!(t.level, TOAST_INFO);
        assert_eq!(t.text, "正在初始化");

        let t = event_toast("wallpaper-gif-oversized", &json!({ "display_id": "d1" })).unwrap();
        assert_eq!(t.level, TOAST_WARNING);

        let t = event_toast(
            "wallpaper-thumbnail-failed",
            &json!({ "file_path": "C:/x/a.mp4" }),
        )
        .unwrap();
        assert_eq!(t.level, TOAST_ERROR);
        assert_eq!(t.text, "缩略图生成失败：a.mp4");

        let t = event_toast("config-load-error", &json!("配置文件损坏")).unwrap();
        assert_eq!(t.text, "配置文件损坏");

        let t = event_toast("audio-disabled", &Value::Null).unwrap();
        assert_eq!(t.level, TOAST_INFO);

        // 无 message / 无关事件 → None
        assert!(event_toast("wallpaper-loading", &json!({})).is_none());
        assert!(event_toast("wallpaper-rotated", &json!({ "key": "all" })).is_none());
    }
}
