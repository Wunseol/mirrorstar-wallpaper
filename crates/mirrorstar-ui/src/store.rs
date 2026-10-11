//! UI 侧状态仓：**唯一的内存状态源**。
//!
//! ## 为什么要有独立状态仓
//!
//! 协议推送事件（如 `wallpaper-added`）与命令响应（快照）是两条异构的入径。若各处置回调
//! 各自去改 Slint 属性，很快会出现"哪条路径改了哪些字段"无法追踪、刷新遗漏的问题。
//! 因此所有入径统一先落到本结构（[`AppStore`]），再由 [`AppStore::view`] 一次性投影为
//! 纯数据视图 [`AppView`]，由 `main` 写回 Slint 属性。**Slint 组件不持有任何业务状态**，
//! 只是一个渲染器。
//!
//! ## 为什么本模块不依赖 Slint
//!
//! 状态仓与刷新通路若耦合到 `MainWindow` 类型，单测就必须起事件循环。这里刻意让
//! [`AppStore`] / [`AppView`] 保持"纯 Rust + serde"，于是"事件到达如何增量更新"可以在
//! 无 GUI 环境下直接断言（见文件末尾单测）。

use std::collections::{BTreeMap, HashMap, HashSet};

use mirrorstar_core::config::settings::{
    AppConfig, Arrangement, FullscreenAction, Order, RotationConfig,
};
use mirrorstar_core::config::{DisplayInfo, Pool, WallpaperEntry, WallpaperType};
use mirrorstar_core::ui_proto::{Hello, UiEvent, UnitStateDto};
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperState};
use serde_json::Value;

use crate::dnd;
use crate::settings;

/// 下拉选项（值 + 展示标签）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionItemData {
    pub value: String,
    pub label: String,
}

/// 池列表行视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRowData {
    pub id: String,
    pub name: String,
    /// 成员数。
    pub members: i32,
    /// 是否可上移 / 下移（首/末行不可）。
    pub up: bool,
    pub down: bool,
}

/// 调度单元行视图（激活池用 `pool-index` 指向 [`OptionItemData`] 列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRowData {
    pub key: String,
    pub label: String,
    /// 在池选项列表中的索引；0 表示"全部壁纸池"（`active_pool` 为 `None`）。
    pub pool_index: i32,
    pub enabled: bool,
}

/// 池成员 / 可添加壁纸行视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRowData {
    pub id: String,
    pub name: String,
    pub up: bool,
    pub down: bool,
}

/// 轮换单元状态视图。
///
/// 直接复用 core 协议类型 [`UnitStateDto`]（`get_unit_states` 的返回元素），
/// 不再于本 crate 内自带镜像结构。
pub type UnitStateView = UnitStateDto;

/// UI 侧唯一内存状态源。
#[derive(Debug)]
pub struct AppStore {
    /// 壳侧 Hello 中的角色名（仅诊断展示）。
    server_client: String,
    /// 壳侧协议版本。
    protocol_version: u32,
    /// 壳侧能力位。
    server_capabilities: u32,
    /// 是否处于已连接状态。
    connected: bool,
    /// 全量配置快照。
    config: Option<AppConfig>,
    /// 多屏排列（顶层布局策略，`get_arrangement` 单独拉取）。
    arrangement: Option<Arrangement>,
    /// 壁纸库。
    wallpapers: Vec<WallpaperEntry>,
    /// 是否已收到 `get_wallpapers` 全量快照（用于骨架屏 / 空态区分）。
    wallpapers_loaded: bool,
    /// 壁纸库版本号：任何增删改 +1，供虚拟化网格判断"是否需重建可见窗口"。
    revision: u64,
    /// 源文件缺失的壁纸条目 id（来自 `wallpaper-source-missing`）。
    missing_ids: HashSet<String>,
    /// 缩略图生成失败的源文件路径（来自 `wallpaper-thumbnail-failed`）。
    thumb_failed_paths: HashSet<String>,
    /// 显示器列表。
    displays: Vec<DisplayInfo>,
    /// 轮换配置。
    rotation: Option<RotationConfig>,
    /// 池列表。
    pools: Vec<Pool>,
    /// 轮换单元状态。
    units: Vec<UnitStateView>,
    /// 开机自启状态。
    auto_start: bool,
    /// 最近一次已知的壁纸播放状态。
    playback: Option<WallpaperState>,
    /// 已接收事件计数（任何事件都累加，用于验证推送通路存活）。
    event_count: u32,
    /// 最近一条事件的简要描述。
    last_event: String,
    /// 最近一次错误（快照拉取失败 / 命令报错），供诊断展示。
    last_error: Option<String>,
    /// 搜索关键字（原样保存；匹配时统一转小写）。
    ///
    /// 变更时**推进版本号**：过滤结果变了，可见窗口与内容高度都要重算（SubTask 5.3）。
    search_keyword: String,
    /// 当前选中卡片 id（纯 UI 态，不推进版本号：只影响卡片描边，不重建模型）。
    selected_id: Option<String>,
    /// 最近一次"设为壁纸"成功的条目 id。
    ///
    /// 壳侧 `get_wallpaper_state` 只回播放状态、不含条目 id，故这里只能是**乐观标记**
    /// （会话内有效）；真正的"当前壁纸"归属需壳侧补一个带 id 的查询（见报告遗留项）。
    active_id: Option<String>,

    // ── Task 6 设置面板 ──────────────────────────────────────────────────────
    /// 目标显示器 id（默认主显示器；`None` 表示无可用显示器）。
    selected_display: Option<String>,
    /// 按显示器记忆的缩放模式（键为 `display_id`，特殊键 `"all"` 表示全部显示器 / 新显示器兜底）。
    ///
    /// 由配置快照 `config.scaling.modes` 回填；用户改下拉时乐观写入并经 `update_config` 落盘。
    scaling_modes: BTreeMap<String, ScalingMode>,
    /// 运行时音量 `[0,1]`。
    volume: f32,
    /// 运行时静音。
    muted: bool,
    /// 运行时播放速度。
    speed: f32,
    /// 是否已用配置种子化音量/静音/速度。
    ///
    /// 这三个是**运行时**属性：拖动 / 点击期间只改内存，提交时才经 `update_config` 落盘。
    /// 故只在首次 `apply_config` 种子化一次，避免拖动过程中到达的 `config-changed`
    /// （含自身落盘的配置回显）把用户改动临时覆盖回去。
    audio_initialized: bool,
    /// 壳侧报告音频设备不可用（禁用音量/静音控件）。
    audio_disabled: bool,
    /// 最近一次桌面（WorkerW）状态：`true` 正常、`false` 失效。
    ///
    /// 由 `desktop-status-changed` 事件更新（v16-C-007）；"ok=false 时启动 2s 轮询补救 +
    /// 30s 超时提示"的驱动逻辑在 `main` 侧（见 `desktop` 模块）。
    desktop_ok: bool,
    /// 鼠标交互模式（`true` = 允许交互；`false` = 穿透）。
    interaction_mode: bool,
    /// 设置面板是否展开。
    settings_open: bool,
    /// 当前正在编辑成员的池 id。
    editing_pool: Option<String>,
    /// 设置相关模型（显示器/池/单元）的版本号：仅在真正变化时推进，供 UI 侧跳过重建。
    settings_revision: u64,
}

impl Default for AppStore {
    fn default() -> Self {
        Self {
            server_client: String::new(),
            protocol_version: 0,
            server_capabilities: 0,
            connected: false,
            config: None,
            arrangement: None,
            wallpapers: Vec::new(),
            wallpapers_loaded: false,
            revision: 0,
            missing_ids: HashSet::new(),
            thumb_failed_paths: HashSet::new(),
            displays: Vec::new(),
            rotation: None,
            pools: Vec::new(),
            units: Vec::new(),
            auto_start: false,
            playback: None,
            event_count: 0,
            last_event: String::new(),
            last_error: None,
            search_keyword: String::new(),
            selected_id: None,
            active_id: None,
            selected_display: None,
            scaling_modes: BTreeMap::new(),
            volume: 0.8,
            muted: false,
            speed: 1.0,
            audio_initialized: false,
            audio_disabled: false,
            desktop_ok: true,
            interaction_mode: false,
            settings_open: true,
            editing_pool: None,
            settings_revision: 0,
        }
    }
}

impl AppStore {
    /// 记录握手结果。
    pub fn apply_hello(&mut self, hello: &Hello) {
        self.server_client = hello.client.clone();
        self.protocol_version = hello.protocol_version;
        self.server_capabilities = hello.capabilities;
        self.connected = true;
    }

    /// 更新连接状态（断开时由断开回调置 false）。
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub fn apply_config(&mut self, config: AppConfig) {
        // 音量/静音/速度是运行时属性（提交时才落盘）：仅在首次种子化，避免拖动期间的
        // `config-changed` 重拉覆盖用户改动。
        if !self.audio_initialized {
            self.volume = config.audio.volume;
            self.muted = config.audio.muted;
            self.speed = config.video.speed;
            self.audio_initialized = true;
        }
        self.arrangement = Some(config.arrangement);
        // 缩放映射以配置快照为权威来源：任何一次快照（首载 / config-changed / 写回后回填）
        // 都覆盖内存映射，保证重启后下拉显示与配置一致，并清除失效残留。
        self.scaling_modes = config.scaling.modes.clone();
        self.config = Some(config);
        // 配置内嵌排列变化后收敛目标显示器选择。
        self.reconcile_selected_display();
        self.bump_settings();
    }

    /// 应用 `get_arrangement` / `update_arrangement` 结果。
    ///
    /// **始终以本次结果为准**：排列是 `get_arrangement` 的唯一权威来源，且与 config 内嵌字段
    /// 可能短暂不一致（`update_arrangement` 后 config 尚未重拉），后者胜出可避免显示回退。
    pub fn apply_arrangement(&mut self, arrangement: Arrangement) {
        self.arrangement = Some(arrangement);
        if let Some(config) = &mut self.config {
            config.arrangement = arrangement;
        }
        // 排列变化会改变「目标显示器」下拉的语义与选择，收敛到新排列下的合法值。
        self.reconcile_selected_display();
        self.bump_settings();
    }

    /// 推进设置模型版本号。
    fn bump_settings(&mut self) {
        self.settings_revision = self.settings_revision.wrapping_add(1);
    }

    pub fn apply_wallpapers(&mut self, wallpapers: Vec<WallpaperEntry>) {
        self.wallpapers = wallpapers;
        self.wallpapers_loaded = true;
        // 全量快照是库的权威真相：收敛引用关系。事件路径已清理的悬挂 id 与标记，
        // 快照路径同样必须清理，否则旧快照会重新引入悬挂引用、并让标记集合只增不减。
        self.reconcile_references();
        self.revision = self.revision.wrapping_add(1);
    }

    /// 库内容被整体替换后收敛引用关系。
    ///
    /// - 选中 / 激活 id 若已不在库中则清除（否则留下悬挂 id：UI 永远高亮不到任何卡片）；
    /// - `missing_ids` / `thumb_failed_paths` 只保留仍存在于库中的条目，使其随库规模有界，
    ///   避免长时间事件流下集合无界增长。
    fn reconcile_references(&mut self) {
        let ids: HashSet<&str> = self.wallpapers.iter().map(|w| w.id.as_str()).collect();
        let paths: HashSet<&str> = self
            .wallpapers
            .iter()
            .map(|w| w.file_path.as_str())
            .collect();
        if self
            .selected_id
            .as_deref()
            .is_some_and(|id| !ids.contains(id))
        {
            self.selected_id = None;
        }
        if self
            .active_id
            .as_deref()
            .is_some_and(|id| !ids.contains(id))
        {
            self.active_id = None;
        }
        self.missing_ids.retain(|id| ids.contains(id.as_str()));
        self.thumb_failed_paths
            .retain(|path| paths.contains(path.as_str()));
    }

    /// 壁纸库只读切片（虚拟化网格据此切片可见窗口）。
    pub fn wallpapers(&self) -> &[WallpaperEntry] {
        &self.wallpapers
    }

    /// 壁纸库版本号（增删改均递增）。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 直接设置桌面状态（轮询成功重初始化后由 `main` 侧调用）。
    pub fn set_desktop_ok(&mut self, ok: bool) {
        self.desktop_ok = ok;
    }

    /// 该条目是否被标记为"源文件缺失"。
    pub fn is_source_missing(&self, id: &str) -> bool {
        self.missing_ids.contains(id)
    }

    /// 该源文件路径是否发生过缩略图生成失败。
    pub fn is_thumb_failed(&self, file_path: &str) -> bool {
        self.thumb_failed_paths.contains(file_path)
    }

    /// 清除某源文件路径的"缩略图生成失败"标记，返回是否真的移除了条目。
    ///
    /// 用途：缩略图在本会话内成功解码恢复后清理陈旧的失败标记，避免后续 `is_thumb_failed`
    /// 短路把已可用的缩略图永久遮蔽为类型图标。幂等（重复调用返回 `false`），不递增 `revision`。
    pub fn clear_thumb_failed(&mut self, file_path: &str) -> bool {
        self.thumb_failed_paths.remove(file_path)
    }

    pub fn apply_displays(&mut self, displays: Vec<DisplayInfo>) {
        self.displays = displays;
        // 清理已消失显示器的缩放记忆残留（保留 "all" 广播键），避免映射随插拔只增不减。
        {
            let live: HashSet<&str> = self.displays.iter().map(|d| d.id.as_str()).collect();
            self.scaling_modes
                .retain(|k, _| k == "all" || live.contains(k.as_str()));
        }
        // 显示器集合变化后按当前排列收敛目标显示器选择（对齐 `display-list.ts`）。
        self.reconcile_selected_display();
        self.bump_settings();
    }

    pub fn apply_rotation(&mut self, rotation: RotationConfig) {
        self.rotation = Some(rotation);
        self.bump_settings();
    }

    pub fn apply_pools(&mut self, pools: Vec<Pool>) {
        self.pools = pools;
        // 编辑目标失效则清除，避免编辑一个已删除的池。
        if let Some(id) = self.editing_pool.as_deref() {
            if !self.pools.iter().any(|p| p.id == id) {
                self.editing_pool = None;
            }
        }
        self.bump_settings();
    }

    pub fn apply_unit_states(&mut self, units: Vec<UnitStateView>) {
        self.units = units;
        self.bump_settings();
    }

    pub fn apply_auto_start(&mut self, enabled: bool) {
        self.auto_start = enabled;
    }

    /// 记录播放状态（由 `wallpaper-state-changed` 事件或查询结果驱动）。
    pub fn apply_playback(&mut self, state: Option<WallpaperState>) {
        self.playback = state;
    }

    /// 记录一条错误信息。
    pub fn record_error(&mut self, message: impl Into<String>) {
        self.last_error = Some(message.into());
    }

    // ── 设置面板访问器（Task 6 / 7）──────────────────────────────────────────

    pub fn selected_display(&self) -> Option<&str> {
        self.selected_display.as_deref()
    }

    /// 切换目标显示器；未知 id 视为无选择，`"all"`（广播语义）单独放行。
    ///
    /// `"all"` 并非真实显示器 id，但在 `all_same` / `span` 排列下是唯一合法目标，
    /// 故不能按"必须是已知显示器"过滤；其余排列下下拉不会产生 `"all"`。
    pub fn select_display(&mut self, id: Option<String>) {
        let valid = match id.as_deref() {
            None | Some("") => None,
            Some("all") => Some("all".to_string()),
            Some(id) => self
                .displays
                .iter()
                .any(|d| d.id == id)
                .then(|| id.to_string()),
        };
        self.selected_display = valid;
    }

    /// 按当前排列收敛目标显示器选择（显示器载入后 / 排列变化后调用）。
    ///
    /// - `per_monitor`：选择必须指向仍存在的具体显示器，否则收敛到主显示器
    ///   （`is_primary` 为真者；无标记则取首个）；显示器集合为空时清空选择。
    /// - `all_same` / `span`：选择固定为 `"all"`（该排列下具体显示器选择无意义）。
    fn reconcile_selected_display(&mut self) {
        match self.arrangement() {
            Arrangement::PerMonitor => {
                let still_valid = self
                    .selected_display
                    .as_deref()
                    .filter(|id| *id != "all")
                    .is_some_and(|id| self.displays.iter().any(|d| d.id == id));
                if !still_valid {
                    self.selected_display = self
                        .displays
                        .iter()
                        .find(|d| d.is_primary)
                        .or_else(|| self.displays.first())
                        .map(|d| d.id.clone());
                }
            }
            Arrangement::AllSame | Arrangement::Span => {
                self.selected_display = Some("all".to_string());
            }
        }
    }

    /// 当前缩放的 key：非 `per_monitor` 恒为 `"all"`，`per_monitor` 用选中显示器
    /// （无选择时回退 `"all"`）。与壳侧 `scaling_mode_for` 的「精确 → `"all"` → 默认」
    /// 回退链保持同一语义。
    pub fn scaling_key(&self) -> String {
        match self.arrangement() {
            Arrangement::PerMonitor => self
                .selected_display
                .clone()
                .unwrap_or_else(|| "all".to_string()),
            Arrangement::AllSame | Arrangement::Span => "all".to_string(),
        }
    }

    /// 读取指定 key 的缩放模式：精确命中 → `"all"` 兜底 → `ScalingMode` 默认（`Fill`）。
    pub fn scaling_mode_for_key(&self, key: &str) -> ScalingMode {
        self.scaling_modes
            .get(key)
            .or_else(|| self.scaling_modes.get("all"))
            .copied()
            .unwrap_or_default()
    }

    /// 写入指定 key 的缩放模式（乐观本地更新 + 推进设置版本号以重投影下拉）。
    pub fn set_scaling_mode_for_key(&mut self, key: String, mode: ScalingMode) {
        self.scaling_modes.insert(key, mode);
        self.bump_settings();
    }

    /// 当前生效的缩放模式（按排列 + 选中显示器解析 key 后读取）。
    pub fn current_scaling_mode(&self) -> ScalingMode {
        self.scaling_mode_for_key(&self.scaling_key())
    }

    /// 只读的缩放映射（供构造完整的 `scaling.modes` patch，避免浅合并冲掉其它显示器记忆）。
    pub fn scaling_modes_map(&self) -> &BTreeMap<String, ScalingMode> {
        &self.scaling_modes
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        // 滑块位置在 `refresh_settings` 中按设置版本号重投影（拖拽期间不写回，避免跳动）。
        self.bump_settings();
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// 预览音量/静音的持久化默认值（配置标度 0..1）；配置尚未加载时返回 `None`。
    pub fn preview_audio(&self) -> Option<(f32, bool)> {
        self.config
            .as_ref()
            .map(|c| (c.audio.preview_volume, c.audio.preview_muted))
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = settings::sanitize_speed(speed);
        self.bump_settings();
    }

    pub fn interaction_mode(&self) -> bool {
        self.interaction_mode
    }

    pub fn set_interaction_mode(&mut self, enabled: bool) {
        self.interaction_mode = enabled;
    }

    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn set_settings_open(&mut self, open: bool) {
        self.settings_open = open;
    }

    pub fn rotation(&self) -> Option<&RotationConfig> {
        self.rotation.as_ref()
    }

    pub fn playback(&self) -> Option<WallpaperState> {
        self.playback
    }

    pub fn pools(&self) -> &[Pool] {
        &self.pools
    }

    pub fn settings_revision(&self) -> u64 {
        self.settings_revision
    }

    pub fn editing_pool(&self) -> Option<&str> {
        self.editing_pool.as_deref()
    }

    pub fn set_editing_pool(&mut self, id: Option<String>) {
        // 仅接受真实存在的池 id，避免悬空编辑目标。
        let next = id.filter(|id| self.pools.iter().any(|p| &p.id == id));
        if self.editing_pool != next {
            self.editing_pool = next;
            self.bump_settings();
        }
    }

    // ── Task 7 轮换 / 池 的乐观本地变更（调用方随后经协议提交）────────────────

    /// 当前多屏排列（缺省 `per_monitor`）。
    pub fn arrangement(&self) -> Arrangement {
        self.arrangement.unwrap_or(Arrangement::PerMonitor)
    }

    /// 把 JSON patch 深合并进当前配置（乐观本地更新）。
    ///
    /// 返回合并后的**完整配置 JSON**（供 `update_config` 提交）；无配置或解析失败返回 `None`
    /// （调用方应放弃提交）。
    pub fn merge_config_patch(&mut self, patch: &Value) -> Option<Value> {
        let mut value = self.config_json()?;
        settings::merge_json(&mut value, patch);
        let parsed: AppConfig = serde_json::from_value(value.clone()).ok()?;
        self.apply_config(parsed);
        Some(value)
    }

    /// 当前配置的 JSON 形态（供 `update_config` 提交）。
    pub fn config_json(&self) -> Option<Value> {
        serde_json::to_value(self.config.as_ref()?).ok()
    }

    /// 覆盖单个调度单元的启用态；未知 key 补一条记录。
    pub fn set_unit_enabled(&mut self, key: &str, enabled: bool) {
        match self.units.iter_mut().find(|u| u.key == key) {
            Some(unit) => unit.enabled = enabled,
            None => self.units.push(UnitStateView {
                key: key.to_string(),
                active_pool: None,
                enabled,
            }),
        }
        self.bump_settings();
    }

    /// 覆盖单个调度单元的激活池；未知 key 补一条记录。`None` = 全部壁纸池。
    pub fn set_unit_active_pool(&mut self, key: &str, pool_id: Option<String>) {
        match self.units.iter_mut().find(|u| u.key == key) {
            Some(unit) => unit.active_pool = pool_id,
            None => self.units.push(UnitStateView {
                key: key.to_string(),
                active_pool: pool_id,
                enabled: false,
            }),
        }
        self.bump_settings();
    }

    /// 池上移/下移；返回新的完整 id 顺序（供 `reorder_pools` 提交）。
    pub fn move_pool(&mut self, from: usize, to: usize) -> Option<Vec<String>> {
        if from >= self.pools.len() || to >= self.pools.len() || from == to {
            return None;
        }
        let moved = self.pools.remove(from);
        self.pools.insert(to, moved);
        self.bump_settings();
        Some(self.pools.iter().map(|p| p.id.clone()).collect())
    }

    /// 指定池内成员上移/下移；返回新的完整成员顺序（供 `update_pool` 提交）。
    pub fn move_pool_member(
        &mut self,
        pool_id: &str,
        from: usize,
        to: usize,
    ) -> Option<Vec<String>> {
        let pool = self.pools.iter_mut().find(|p| p.id == pool_id)?;
        if from >= pool.member_ids.len() || to >= pool.member_ids.len() || from == to {
            return None;
        }
        let ids = settings::reorder_members(&pool.member_ids, from, to);
        pool.member_ids = ids.clone();
        self.bump_settings();
        Some(ids)
    }

    /// 从池中移除成员；返回新成员顺序。
    pub fn remove_pool_member(&mut self, pool_id: &str, member_id: &str) -> Option<Vec<String>> {
        let ids = {
            let pool = self.pools.iter_mut().find(|p| p.id == pool_id)?;
            if !pool.member_ids.iter().any(|m| m == member_id) {
                return None;
            }
            pool.member_ids.retain(|m| m != member_id);
            pool.member_ids.clone()
        };
        self.bump_settings();
        Some(ids)
    }

    /// 向池中追加成员（去重）；返回新成员顺序。
    pub fn add_pool_member(&mut self, pool_id: &str, member_id: &str) -> Option<Vec<String>> {
        let ids = {
            let pool = self.pools.iter_mut().find(|p| p.id == pool_id)?;
            if pool.member_ids.iter().any(|m| m == member_id) {
                return None;
            }
            pool.member_ids.push(member_id.to_string());
            pool.member_ids.clone()
        };
        self.bump_settings();
        Some(ids)
    }

    /// 重命名池（乐观本地更新）；未知 id 返回 `false`。
    pub fn rename_pool(&mut self, pool_id: &str, name: String) -> bool {
        let Some(pool) = self.pools.iter_mut().find(|p| p.id == pool_id) else {
            return false;
        };
        pool.name = name;
        self.bump_settings();
        true
    }

    /// 指定池的成员 id 顺序（提交 `update_pool` 用）。
    pub fn pool_member_ids(&self, pool_id: &str) -> Option<Vec<String>> {
        self.pools
            .iter()
            .find(|p| p.id == pool_id)
            .map(|p| p.member_ids.clone())
    }

    /// 目标显示器下拉选项（按当前排列生成）：
    /// - `per_monitor`：仅列具体显示器（主屏 label 前缀 `[主] `），不再出现「全部显示器」；
    /// - `all_same` / `span`：仅含 `{ value: "all", label: "全部显示器" }`，
    ///   与壳侧 `display_id = "all"` 广播语义对齐。
    ///
    /// 排列尚未加载（`None`）时按 `per_monitor` 处理。
    pub fn display_options(&self) -> Vec<OptionItemData> {
        match self.arrangement() {
            Arrangement::PerMonitor => self
                .displays
                .iter()
                .map(|d| OptionItemData {
                    value: d.id.clone(),
                    label: if d.is_primary {
                        format!("[主] {}", d.name)
                    } else {
                        d.name.clone()
                    },
                })
                .collect(),
            Arrangement::AllSame | Arrangement::Span => vec![OptionItemData {
                value: "all".to_string(),
                label: "全部显示器".to_string(),
            }],
        }
    }

    /// 目标显示器下拉当前索引（对 [`Self::display_options`] 的命中位置；未命中为 0）。
    pub fn display_index(&self) -> i32 {
        match self.arrangement() {
            Arrangement::PerMonitor => self
                .selected_display
                .as_deref()
                .and_then(|id| self.displays.iter().position(|d| d.id == id))
                .map(|i| i as i32)
                .unwrap_or(0),
            Arrangement::AllSame | Arrangement::Span => 0,
        }
    }

    /// 是否存在可用目标（播放控制三键启用条件，SubTask 6.5）：
    /// - `per_monitor`：存在指向真实显示器的具体选择；
    /// - `all_same` / `span`：存在至少一个显示器（目标固定为 `"all"`）。
    pub fn has_target(&self) -> bool {
        match self.arrangement() {
            Arrangement::PerMonitor => self
                .selected_display
                .as_deref()
                .is_some_and(|id| self.displays.iter().any(|d| d.id == id)),
            Arrangement::AllSame | Arrangement::Span => !self.displays.is_empty(),
        }
    }

    /// 「目标显示器」下拉是否可用：仅 `per_monitor` 排列下选择有意义。
    pub fn display_dropdown_enabled(&self) -> bool {
        self.arrangement() == Arrangement::PerMonitor
    }

    /// 当前目标调度单元 key（PerMonitor 用选中显示器，其余排列为全局 `all`）。
    pub fn current_unit_key(&self) -> String {
        match self.arrangement() {
            Arrangement::PerMonitor => self.selected_display().unwrap_or("all").to_string(),
            Arrangement::AllSame | Arrangement::Span => "all".to_string(),
        }
    }

    /// 池下拉选项（首项"全部壁纸池"，值 `""`）。
    pub fn pool_options(&self) -> Vec<OptionItemData> {
        let mut items = vec![OptionItemData {
            value: String::new(),
            label: "全部壁纸池".to_string(),
        }];
        items.extend(self.pools.iter().map(|p| OptionItemData {
            value: p.id.clone(),
            label: p.name.clone(),
        }));
        items
    }

    /// 池列表行（含上移/下移可用性）。
    pub fn pool_rows(&self) -> Vec<PoolRowData> {
        let last = self.pools.len().saturating_sub(1);
        self.pools
            .iter()
            .enumerate()
            .map(|(i, pool)| PoolRowData {
                id: pool.id.clone(),
                name: pool.name.clone(),
                members: pool.member_ids.len() as i32,
                up: i > 0,
                down: i < last,
            })
            .collect()
    }

    /// 调度单元行（按其排列推导 key，并与 `get_unit_states` 的真实值合并）。
    pub fn unit_rows(&self) -> Vec<UnitRowData> {
        let arrangement = self.arrangement.unwrap_or(Arrangement::PerMonitor);
        let options = self.pool_options();
        settings::unit_keys_for_arrangement(arrangement, &self.displays)
            .into_iter()
            .map(|settings::UnitKey { key, label }| {
                let state = self.units.iter().find(|u| u.key == key);
                // 池选项首项为"全部壁纸池"；未指定 / 已失效的池都落回该首项。
                let pool_index = state
                    .and_then(|s| s.active_pool.as_deref())
                    .and_then(|id| options.iter().position(|o| o.value == id))
                    .map(|i| i as i32)
                    .unwrap_or(0);
                UnitRowData {
                    key,
                    label,
                    pool_index,
                    enabled: state.map(|s| s.enabled).unwrap_or(false),
                }
            })
            .collect()
    }

    /// 指定池的成员行（顺序即 `member_ids` 顺序）。
    pub fn member_rows(&self, pool_id: &str) -> Vec<MemberRowData> {
        let Some(pool) = self.pools.iter().find(|p| p.id == pool_id) else {
            return Vec::new();
        };
        let last = pool.member_ids.len().saturating_sub(1);
        pool.member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| MemberRowData {
                id: id.clone(),
                name: self
                    .wallpapers
                    .iter()
                    .find(|w| &w.id == id)
                    .map(|w| settings::file_name(&w.file_path))
                    .unwrap_or_else(|| id.clone()),
                up: i > 0,
                down: i < last,
            })
            .collect()
    }

    /// 可加入指定池的壁纸行：跳过已在池内者、重复路径者与网页类型（DR-18）。
    pub fn available_rows(&self, pool_id: &str) -> Vec<MemberRowData> {
        let Some(pool) = self.pools.iter().find(|p| p.id == pool_id) else {
            return Vec::new();
        };
        let mut seen_paths: HashSet<String> = self
            .wallpapers
            .iter()
            .filter(|w| pool.member_ids.contains(&w.id))
            .map(|w| w.file_path.clone())
            .collect();
        self.wallpapers
            .iter()
            .filter(|w| w.wallpaper_type != WallpaperType::Web)
            .filter(|w| !pool.member_ids.contains(&w.id))
            .filter(|w| seen_paths.insert(w.file_path.clone()))
            .map(|w| MemberRowData {
                id: w.id.clone(),
                name: settings::file_name(&w.file_path),
                up: false,
                down: false,
            })
            .collect()
    }

    // ── 网格交互（Task 5）────────────────────────────────────────────────────

    /// 设置搜索关键字；**仅在变化时**推进版本号（过滤结果改变 → 需重建可见窗口）。
    pub fn set_search_keyword(&mut self, keyword: String) {
        if self.search_keyword != keyword {
            self.search_keyword = keyword;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// 搜索是否处于激活态（用于禁用拖拽排序、切换空态文案）。
    pub fn search_active(&self) -> bool {
        !self.search_keyword.is_empty()
    }

    /// 过滤后保留的原始索引（虚拟化网格据此切片，见 `main::refresh_grid`）。
    pub fn filtered_indices(&self) -> Vec<usize> {
        dnd::filter_indices(&self.wallpapers, &self.search_keyword)
    }

    /// 过滤后条目数（不分配索引数组；拖拽悬停时按帧调用）。
    pub fn filtered_count(&self) -> usize {
        dnd::count_matches(&self.wallpapers, &self.search_keyword)
    }

    /// 过滤序列中第 `index` 项的条目 id（键盘激活 / 删除用）。
    pub fn filtered_id_at(&self, index: usize) -> Option<String> {
        let filtered = self.filtered_indices();
        filtered
            .get(index)
            .and_then(|&global| self.wallpapers.get(global))
            .map(|wp| wp.id.clone())
    }

    /// 条目 id 在**过滤序列**中的位置（键盘焦点随点击卡片同步用）。
    pub fn filtered_position_of(&self, id: &str) -> Option<usize> {
        self.filtered_indices()
            .into_iter()
            .position(|global| self.wallpapers.get(global).is_some_and(|wp| wp.id == id))
    }

    /// 当前选中卡片 id。
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// 设置选中卡片（`None` 表示取消选中）。
    pub fn select(&mut self, id: Option<String>) {
        self.selected_id = id;
    }

    /// 标记"当前壁纸"（乐观：见字段说明）。
    pub fn set_active(&mut self, id: String) {
        self.active_id = Some(id);
    }

    /// 条目在库中的当前索引。
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.wallpapers.iter().position(|wp| wp.id == id)
    }

    /// 全量 id 列表（拖拽排序提交给壳侧用）。
    pub fn all_ids(&self) -> Vec<String> {
        self.wallpapers.iter().map(|wp| wp.id.clone()).collect()
    }

    /// 按给定 id 顺序**本地重排**（乐观更新），成功返回 `true`。
    ///
    /// 仅在 id 集合与库完全一致时接受，避免把不一致的顺序写进本地状态；失败时调用方应
    /// 直接放弃本次拖拽（不回滚，因为本地未改）。
    pub fn reorder_local(&mut self, ids: &[String]) -> bool {
        if ids.len() != self.wallpapers.len() {
            return false;
        }
        let mut by_id: HashMap<String, WallpaperEntry> = self
            .wallpapers
            .iter()
            .map(|wp| (wp.id.clone(), wp.clone()))
            .collect();
        if by_id.len() != ids.len() {
            return false;
        }
        let mut reordered = Vec::with_capacity(ids.len());
        for id in ids {
            match by_id.remove(id) {
                Some(entry) => reordered.push(entry),
                None => return false,
            }
        }
        self.wallpapers = reordered;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 应用一条推送事件（增量更新）。
    ///
    /// 壁纸库事件（added/updated/removed/source-missing/thumbnail-failed）做增量更新；
    /// `desktop-status-changed` / `audio-disabled` / `wallpaper-rotated` / `displays-changed`
    /// 更新各自的状态字段；其余事件（含空负载的 `config-changed`）只更新计数与「最近事件」
    /// 摘要，其快照重拉由 `main` 侧的事件回调完成。
    pub fn apply_event(&mut self, event: &UiEvent) {
        self.event_count = self.event_count.saturating_add(1);

        match event.event.as_str() {
            // 新增 / 更新统一走 upsert：`wallpaper-updated` 的缩略图回填正是"按 id 覆盖"，
            // 与新增共用一条路径可避免"已在库中又 push 一份"的重复条目。
            "wallpaper-added" | "wallpaper-updated" => {
                if let Some(entry) = parse_wallpaper(&event.data) {
                    self.upsert_wallpaper(entry);
                    self.last_event = format!("{}（库 {} 项）", event.event, self.wallpapers.len());
                    return;
                }
                // data 为 null（缩略图失败回退、条目已被删除）时无法增量，留待全量刷新。
            }
            "wallpaper-removed" => {
                if let Some(id) = parse_removed_id(&event.data) {
                    // 先取路径：缩略图失败标记以路径为键，条目删除后须一并清理（防止只增不减）。
                    let removed_path = self
                        .wallpapers
                        .iter()
                        .find(|w| w.id == id)
                        .map(|w| w.file_path.clone());
                    self.wallpapers.retain(|w| w.id != id);
                    self.missing_ids.remove(&id);
                    if let Some(path) = removed_path {
                        self.thumb_failed_paths.remove(&path);
                    }
                    // 选中/激活标记随条目一并清理，避免残留悬空 id。
                    if self.selected_id.as_deref() == Some(id.as_str()) {
                        self.selected_id = None;
                    }
                    if self.active_id.as_deref() == Some(id.as_str()) {
                        self.active_id = None;
                    }
                    self.revision = self.revision.wrapping_add(1);
                    self.last_event =
                        format!("wallpaper-removed（库 {} 项）", self.wallpapers.len());
                    return;
                }
            }
            // 源文件缺失：卡片占位 + 提示（SubTask 4.4）。
            "wallpaper-source-missing" => {
                if let Some(id) = event.data.get("id").and_then(Value::as_str) {
                    self.missing_ids.insert(id.to_string());
                }
                self.revision = self.revision.wrapping_add(1);
            }
            // 缩略图生成失败：卡片类型图标占位（不刷新列表）。
            "wallpaper-thumbnail-failed" => {
                if let Some(path) = event.data.get("file_path").and_then(Value::as_str) {
                    self.thumb_failed_paths.insert(path.to_string());
                }
                self.revision = self.revision.wrapping_add(1);
            }
            // 音频设备不可用：禁用音量/静音控件（Task 6.3）。提示文案由 `main` 侧统一产出。
            "audio-disabled" => {
                self.audio_disabled = true;
            }
            // WorkerW 失效/恢复（v16-C-007）：记载桌面状态；轮询补救由 `main` 侧驱动。
            // 负载为 `{ ok: bool, error?: String }`，缺失 `ok` 视为失效（与旧前端 `payload.ok` 一致）。
            "desktop-status-changed" => {
                self.desktop_ok = event
                    .data
                    .get("ok")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            "config-changed" | "config-load-error" => {
                // 配置变更的具体内容不在事件负载中（壳侧发空 payload），增量无从下手；
                // 由 `main` 侧收到事件后重新拉取 `get_config` 等快照补全（Task 6.3）。
            }
            // 轮换 / 手动切换 / 开机恢复成功换图：负载 `{ key, wallpaper_id }`，命中当前目标
            // 单元时更新「当前」角标（真实播放归属）。非命中单元（多屏下其它显示器轮换）忽略，
            // 避免角标跳到用户未在操作的显示器；负载畸形（缺 wallpaper_id / 非字符串）静默忽略。
            "wallpaper-rotated" => {
                let key = event.data.get("key").and_then(Value::as_str);
                let wallpaper_id = event.data.get("wallpaper_id").and_then(Value::as_str);
                if let Some(id) = wallpaper_id {
                    let target = self.current_unit_key();
                    if key == Some(target.as_str()) {
                        self.set_active(id.to_string());
                    }
                }
            }
            // 显示器热插拔（Task 7）：壳侧推送新显示器列表，更新列表并收敛选择 /
            // 清理失效缩放记忆。解析失败静默忽略（与其余事件分支一致），不 panic。
            "displays-changed" => {
                if let Ok(displays) = serde_json::from_value::<Vec<DisplayInfo>>(event.data.clone())
                {
                    self.apply_displays(displays);
                }
            }
            _ => {}
        }

        self.last_event = event.event.clone();
    }

    /// 按 id 覆盖或追加。
    fn upsert_wallpaper(&mut self, entry: WallpaperEntry) {
        // 缩略图回填成功（非空 thumbnail）后同时清除该路径的失败标记与该 id 的缺失标记：
        // 二者同源收敛——`wallpaper-thumbnail-failed` 写 `thumb_failed_paths`、
        // `wallpaper-source-missing` 写 `missing_ids`，都会降级/占位卡片。源文件恢复并
        // 回填缩略图时若不清 `missing_ids`，`card_state` 会先命中 `is_source_missing`
        // 把已恢复的卡片永久显示为"缺失"。
        if !entry.thumbnail.is_empty() {
            self.thumb_failed_paths.remove(&entry.file_path);
            self.missing_ids.remove(&entry.id);
        }
        match self.wallpapers.iter_mut().find(|w| w.id == entry.id) {
            Some(slot) => *slot = entry,
            None => self.wallpapers.push(entry),
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// 投影为纯数据视图（UI 线程写回 Slint 用）。
    ///
    /// 注意：**不含壁纸条目列表**。网格数据由虚拟化窗口单独切片（见 `main::refresh_grid`），
    /// 若在此处全量投影会在库规模增大时把内存与绑定成本重新拉满，违背虚拟化目标。
    pub fn view(&self) -> AppView {
        let arrangement = self.arrangement.unwrap_or(Arrangement::PerMonitor);
        // 播放三键启用条件为"存在可用目标"（见 [`Self::has_target`]），而非"存在选中显示器"：
        // 后者会让 `all_same` / `span` 排列下（选择固定为 `"all"`）默认进入时整体禁用。
        let buttons = settings::playback_buttons(self.playback, self.has_target());
        let (fullscreen_action, pause_on_battery) = self
            .config
            .as_ref()
            .map(|c| (c.pause.fullscreen_action, c.pause.pause_on_battery))
            .unwrap_or((FullscreenAction::Terminate, false));
        let rotation = self.rotation.as_ref();
        // 目标显示器下拉索引：按 [`Self::display_index`] 对当前排列的选项取命中位置。
        let display_index = self.display_index();

        AppView {
            connected: self.connected,
            server_info: self.server_info(),
            item_count: self.wallpapers.len() as i32,
            display_count: self.displays.len() as i32,
            event_count: self.event_count as i32,
            config_summary: self.config_summary(),
            rotation_summary: self.rotation_summary(),
            playback_summary: self.playback_summary(),
            last_event: self.last_event.clone(),
            loading: !self.wallpapers_loaded,
            search_active: self.search_active(),
            selected_id: self.selected_id.clone().unwrap_or_default(),
            active_id: self.active_id.clone().unwrap_or_default(),
            // ── Task 6 设置面板 ──
            settings_open: self.settings_open,
            display_index,
            display_dropdown_enabled: self.display_dropdown_enabled(),
            scaling_index: settings::scaling_index(self.current_scaling_mode()),
            arrangement_index: settings::arrangement_index(arrangement),
            fullscreen_index: settings::fullscreen_index(fullscreen_action),
            pause_enabled: buttons.pause_enabled,
            pause_label: buttons.pause_label.to_string(),
            prev_enabled: buttons.prev_enabled,
            next_enabled: buttons.next_enabled,
            volume_percent: settings::volume_percent(self.volume),
            muted: self.muted,
            auto_start: self.auto_start,
            pause_on_battery,
            interaction_mode: self.interaction_mode,
            audio_controls_enabled: !self.audio_disabled,
            desktop_ok: self.desktop_ok,
            // ── Task 7 轮换 ──
            rotation_enabled: rotation.map(|r| r.enabled).unwrap_or(false),
            rotation_on_boot: rotation.map(|r| r.on_boot).unwrap_or(false),
            rotation_interval: rotation.map(|r| r.interval_minutes as i32).unwrap_or(30),
            // 配置尚未到达时回退到默认算法（顺序循环）的索引 / 说明，避免短暂显示
            // 与其默认值不符的选项（默认已由 shuffle_bag 改为 sequential）。
            rotation_order_index: rotation
                .map(|r| settings::order_index(r.order))
                .unwrap_or(0),
            rotation_order_hint: rotation
                .map(|r| settings::order_hint(r.order))
                .unwrap_or(settings::ORDER_HINTS[0])
                .to_string(),
            about_version: format!("MirrorStar Wallpaper v{}", env!("CARGO_PKG_VERSION")),
        }
    }

    fn server_info(&self) -> String {
        if self.protocol_version == 0 && self.server_client.is_empty() {
            return "（未握手）".to_string();
        }
        let client = if self.server_client.is_empty() {
            "壳"
        } else {
            self.server_client.as_str()
        };
        format!(
            "{} · 协议 v{} · 能力 {:#06x}",
            client, self.protocol_version, self.server_capabilities
        )
    }

    fn config_summary(&self) -> String {
        let Some(config) = &self.config else {
            return "（未加载）".to_string();
        };
        format!(
            "排列={} · 音量={:.0}%{} · 硬解={} · 速度={:.2}x · 自启={}",
            arrangement_label(config.arrangement),
            config.audio.volume * 100.0,
            if config.audio.muted {
                "（静音）"
            } else {
                ""
            },
            if config.video.hwdec { "开" } else { "关" },
            config.video.speed,
            if self.auto_start { "开" } else { "关" },
        )
    }

    fn rotation_summary(&self) -> String {
        let Some(rotation) = &self.rotation else {
            return "（未加载）".to_string();
        };
        format!(
            "{} · 间隔={} 分钟 · 顺序={} · 开机推进={} · 单元={}",
            if rotation.enabled {
                "已启用"
            } else {
                "已停用"
            },
            rotation.interval_minutes,
            order_label(rotation.order),
            if rotation.on_boot { "开" } else { "关" },
            self.units.len(),
        )
    }

    fn playback_summary(&self) -> String {
        match self.playback {
            None => "（无活跃壁纸）".to_string(),
            Some(state) => wallpaper_state_label(state).to_string(),
        }
    }
}

/// 状态仓投影出的纯数据视图；`main` 负责其中已绑定 Slint 属性的字段写回（`server_info` /
/// `event_count` 目前仅供测试与诊断，未绑定）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppView {
    pub connected: bool,
    pub server_info: String,
    pub item_count: i32,
    pub display_count: i32,
    pub event_count: i32,
    pub config_summary: String,
    pub rotation_summary: String,
    pub playback_summary: String,
    pub last_event: String,
    /// 是否仍在加载壁纸库（`false` 表示快照已到，可脱离骨架屏）。
    pub loading: bool,
    /// 搜索是否激活（用于禁用拖拽排序、切换空态文案）。
    pub search_active: bool,
    /// 选中卡片 id（空串表示无选中）。
    pub selected_id: String,
    /// "当前壁纸"卡片 id（空串表示无；乐观标记，见 [`AppStore`] 字段说明）。
    pub active_id: String,
    // ── Task 6 设置面板 ──
    /// 设置面板是否展开（☰ 切换）。
    pub settings_open: bool,
    /// 目标显示器下拉索引（0 = "全部显示器" 或首个具体显示器）。
    pub display_index: i32,
    /// 「目标显示器」下拉是否可用（非 `per_monitor` 排列下选择无意义）。
    pub display_dropdown_enabled: bool,
    /// 缩放模式下拉索引（未命中为 -1）。
    pub scaling_index: i32,
    /// 多屏排列下拉索引。
    pub arrangement_index: i32,
    /// 全屏处置下拉索引。
    pub fullscreen_index: i32,
    /// 播放三键状态（见 [`settings::playback_buttons`]）。
    pub pause_enabled: bool,
    pub pause_label: String,
    pub prev_enabled: bool,
    pub next_enabled: bool,
    /// 音量百分比（展示用）。
    pub volume_percent: i32,
    pub muted: bool,
    pub auto_start: bool,
    pub pause_on_battery: bool,
    pub interaction_mode: bool,
    /// 音量/静音控件是否可用（音频设备不可用时禁用）。
    pub audio_controls_enabled: bool,
    /// 桌面（WorkerW）状态是否正常（状态栏展示）。
    pub desktop_ok: bool,
    // ── Task 7 轮换 ──
    pub rotation_enabled: bool,
    pub rotation_on_boot: bool,
    pub rotation_interval: i32,
    pub rotation_order_index: i32,
    /// 「切换顺序」当前算法的中文说明（设置面板下拉下方展示）。
    pub rotation_order_hint: String,
    /// 关于信息版本号文案。
    pub about_version: String,
}

/// 从事件负载解析壁纸条目（失败即 `None`，不 panic、不中断事件流）。
fn parse_wallpaper(data: &Value) -> Option<WallpaperEntry> {
    serde_json::from_value::<WallpaperEntry>(data.clone()).ok()
}

/// 从 `wallpaper-removed` 负载取出被删 id。
///
/// 壳侧现有实现发的是**裸字符串**（`emit("wallpaper-removed", wallpaper_id)`）；这里同时
/// 兼容 `{ "id": ... }` 形态，避免将来负载改为对象时静默失效。
fn parse_removed_id(data: &Value) -> Option<String> {
    match data {
        Value::String(id) => Some(id.clone()),
        Value::Object(_) => data
            .get("id")
            .or_else(|| data.get("wallpaperId"))
            .or_else(|| data.get("wallpaper_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn arrangement_label(arrangement: Arrangement) -> &'static str {
    match arrangement {
        Arrangement::PerMonitor => "每屏独立",
        Arrangement::AllSame => "每屏同图",
        Arrangement::Span => "跨屏单图",
    }
}

fn order_label(order: Order) -> &'static str {
    match order {
        Order::Sequential => "顺序",
        Order::ShuffleBag => "不重复随机",
        Order::PseudoRandom => "完全随机",
    }
}

fn wallpaper_state_label(state: WallpaperState) -> &'static str {
    match state {
        WallpaperState::Initializing => "初始化中",
        WallpaperState::Playing => "播放中",
        WallpaperState::Paused => "已暂停",
        WallpaperState::Terminated => "已终止",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, data: Value) -> UiEvent {
        UiEvent {
            event: name.to_string(),
            data,
        }
    }

    fn sample_entry(id: &str, file_path: &str) -> Value {
        json!({
            "id": id,
            "file_path": file_path,
            "wallpaper_type": "Video",
            "added_at": "1700000000",
        })
    }

    #[test]
    fn hello_populates_server_info_and_connection() {
        let mut store = AppStore::default();
        assert_eq!(store.view().server_info, "（未握手）");

        store.apply_hello(&Hello {
            protocol_version: 1,
            capabilities: 0b0011,
            client: "mirrorstar-wallpaper".to_string(),
        });
        let view = store.view();
        assert!(view.connected);
        assert!(view.server_info.contains("mirrorstar-wallpaper"));
        assert!(view.server_info.contains("协议 v1"));
    }

    #[test]
    fn snapshot_applies_counts_and_summaries() {
        let mut store = AppStore::default();
        let config: AppConfig = serde_json::from_value(json!({
            "arrangement": "span",
            "audio": { "volume": 0.5, "muted": true },
            "video": { "hwdec": false, "speed": 1.5 },
        }))
        .expect("config deserializes");
        store.apply_config(config);
        store.apply_wallpapers(vec![serde_json::from_value(sample_entry(
            "a",
            "C:/x/one.mp4",
        ))
        .unwrap()]);
        store.apply_auto_start(true);

        let view = store.view();
        assert_eq!(view.item_count, 1);
        assert!(!view.loading, "已应用快照后不应仍处于 loading");
        assert_eq!(store.wallpapers()[0].file_path, "C:/x/one.mp4");
        assert!(view.config_summary.contains("跨屏单图"));
        assert!(view.config_summary.contains("静音"));
        assert!(view.config_summary.contains("自启=开"));
    }

    #[test]
    fn view_is_loading_until_wallpapers_snapshot_arrives() {
        let mut store = AppStore::default();
        assert!(
            store.view().loading,
            "未拉取壁纸库前应处于 loading（骨架屏）"
        );
        store.apply_wallpapers(Vec::new());
        assert!(!store.view().loading, "空库快照到达后仍是已加载");
    }

    #[test]
    fn wallpaper_added_upserts_then_updated_overwrites() {
        let mut store = AppStore::default();
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/one.mp4")));
        assert_eq!(store.view().item_count, 1);
        assert_eq!(store.view().event_count, 1);

        // 同 id 更新：覆盖而非追加
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/two.mp4")));
        assert_eq!(store.view().item_count, 1, "同 id 不应产生重复条目");
        assert_eq!(store.wallpapers()[0].file_path, "C:/x/two.mp4");

        // 新增不同 id：追加
        store.apply_event(&event(
            "wallpaper-added",
            sample_entry("b", "C:/x/three.mp4"),
        ));
        assert_eq!(store.view().item_count, 2);
        assert_eq!(store.view().event_count, 3);
    }

    #[test]
    fn wallpaper_updated_with_null_payload_keeps_library() {
        let mut store = AppStore::default();
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/one.mp4")));
        // 缩略图失败的 fallback：payload 为 null，不应清空或 panic
        store.apply_event(&event("wallpaper-updated", Value::Null));
        assert_eq!(store.view().item_count, 1);
        assert_eq!(store.view().last_event, "wallpaper-updated");
    }

    #[test]
    fn wallpaper_removed_accepts_string_and_object_payloads() {
        let mut store = AppStore::default();
        store.apply_event(&event(
            "wallpaper-added",
            json!([
                sample_entry("a", "C:/x/a.mp4"),
                sample_entry("b", "C:/x/b.mp4")
            ]),
        ));
        // 数组 payload 无法解析为单条，库保持空（不 panic）
        assert_eq!(store.view().item_count, 0);

        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/a.mp4")));
        store.apply_event(&event("wallpaper-added", sample_entry("b", "C:/x/b.mp4")));
        assert_eq!(store.view().item_count, 2);

        // 壳侧现状：裸字符串 id
        store.apply_event(&event("wallpaper-removed", json!("a")));
        assert_eq!(store.view().item_count, 1);
        assert_eq!(store.wallpapers()[0].file_path, "C:/x/b.mp4");

        // 兼容对象形态
        store.apply_event(&event("wallpaper-removed", json!({ "id": "b" })));
        assert_eq!(store.view().item_count, 0);
    }

    #[test]
    fn source_missing_and_thumbnail_failed_marks_are_tracked() {
        let mut store = AppStore::default();
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/a.png")));
        store.apply_event(&event("wallpaper-added", sample_entry("b", "C:/x/b.mp4")));

        assert!(!store.is_source_missing("a"));
        store.apply_event(&event(
            "wallpaper-source-missing",
            json!({ "id": "a", "file_path": "C:/x/a.png" }),
        ));
        assert!(store.is_source_missing("a"), "应标记源文件缺失");
        assert!(!store.is_source_missing("b"));

        assert!(!store.is_thumb_failed("C:/x/b.mp4"));
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/b.mp4", "error": "ffmpeg 抽帧失败" }),
        ));
        assert!(store.is_thumb_failed("C:/x/b.mp4"), "应标记缩略图失败");

        // 删除条目后缺失标记随之清理
        store.apply_event(&event("wallpaper-removed", json!("a")));
        assert!(!store.is_source_missing("a"), "条目删除后不应残留缺失标记");
    }

    #[test]
    fn clear_thumb_failed_removes_mark_idempotently() {
        let mut store = AppStore::default();
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/a.png" }),
        ));
        assert!(store.is_thumb_failed("C:/x/a.png"), "应已标记缩略图失败");

        // 首次清理：确实移除了标记。
        assert!(
            store.clear_thumb_failed("C:/x/a.png"),
            "存在标记时首次清理应返回 true"
        );
        assert!(
            !store.is_thumb_failed("C:/x/a.png"),
            "清理后不应再命中失败标记"
        );

        // 再次清理：标记已不存在，返回 false（幂等）。
        assert!(
            !store.clear_thumb_failed("C:/x/a.png"),
            "标记已清除时再次清理应返回 false"
        );
    }

    #[test]
    fn revision_increases_on_library_mutations() {
        let mut store = AppStore::default();
        let r0 = store.revision();
        store.apply_wallpapers(vec![]);
        let r1 = store.revision();
        assert!(r1 > r0, "全量快照应推进版本号");
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/a.png")));
        let r2 = store.revision();
        assert!(r2 > r1, "新增应推进版本号");
        store.apply_event(&event("wallpaper-removed", json!("a")));
        assert!(store.revision() > r2, "删除应推进版本号");
    }

    #[test]
    fn unknown_event_only_bumps_counter_and_summary() {
        let mut store = AppStore::default();
        store.apply_event(&event("desktop-status-changed", json!(true)));
        let view = store.view();
        assert_eq!(view.event_count, 1);
        assert_eq!(view.last_event, "desktop-status-changed");
        assert_eq!(view.item_count, 0);
    }

    #[test]
    fn desktop_status_event_updates_desktop_ok() {
        let mut store = AppStore::default();
        // 初始视为正常。
        assert!(store.view().desktop_ok);
        // ok=false → 记为失效。
        store.apply_event(&event(
            "desktop-status-changed",
            json!({ "ok": false, "error": "WorkerW 失效" }),
        ));
        assert!(!store.view().desktop_ok);
        // ok=true → 恢复。
        store.apply_event(&event("desktop-status-changed", json!({ "ok": true })));
        assert!(store.view().desktop_ok);
        // 负载缺 `ok` → 保守判定为失效（与旧前端 `payload.ok` 语义一致）。
        store.apply_event(&event("desktop-status-changed", json!({})));
        assert!(!store.view().desktop_ok);
    }

    #[test]
    fn record_error_is_exposed_no_ui_side_effect() {
        let mut store = AppStore::default();
        store.record_error("get_config: 超时");
        assert_eq!(store.last_error.as_deref(), Some("get_config: 超时"));
    }

    #[test]
    fn search_keyword_filters_and_advances_revision_once() {
        let mut store = AppStore::default();
        for i in 0..6 {
            let name = if i % 2 == 0 { "beach" } else { "city" };
            store.apply_event(&event(
                "wallpaper-added",
                sample_entry(&format!("id-{i}"), &format!("C:/w/{name}{i}.png")),
            ));
        }
        let r0 = store.revision();
        assert_eq!(store.filtered_count(), 6);
        assert!(!store.search_active());

        store.set_search_keyword("beach".to_string());
        let r1 = store.revision();
        assert!(r1 > r0, "关键字变化应推进版本号");
        assert!(store.search_active());
        assert_eq!(store.filtered_count(), 3);
        assert_eq!(store.filtered_indices(), vec![0, 2, 4]);
        assert!(store.view().search_active);

        // 相同关键字不重复推进版本号
        store.set_search_keyword("beach".to_string());
        assert_eq!(store.revision(), r1, "未变化不应推进版本号");

        store.set_search_keyword(String::new());
        assert_eq!(store.filtered_count(), 6);
    }

    #[test]
    fn selection_and_active_are_ui_state_without_revision_bump() {
        let mut store = AppStore::default();
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/a.png")));
        let r0 = store.revision();

        store.select(Some("a".to_string()));
        store.set_active("a".to_string());
        assert_eq!(store.revision(), r0, "选中/激活属 UI 态，不应触发网格重建");
        let view = store.view();
        assert_eq!(view.selected_id, "a");
        assert_eq!(view.active_id, "a");

        store.select(None);
        assert_eq!(store.view().selected_id, "");
    }

    #[test]
    fn removed_entry_clears_selection_and_active_marks() {
        let mut store = AppStore::default();
        store.apply_event(&event("wallpaper-added", sample_entry("a", "C:/x/a.png")));
        store.select(Some("a".to_string()));
        store.set_active("a".to_string());

        store.apply_event(&event("wallpaper-removed", json!("a")));
        assert_eq!(store.view().selected_id, "");
        assert_eq!(store.view().active_id, "");
    }

    #[test]
    fn reorder_local_accepts_permutation_and_rejects_mismatch() {
        let mut store = AppStore::default();
        for id in ["a", "b", "c"] {
            store.apply_event(&event(
                "wallpaper-added",
                sample_entry(id, &format!("C:/x/{id}.png")),
            ));
        }
        let r0 = store.revision();

        assert!(store.reorder_local(&["c".into(), "a".into(), "b".into()]));
        assert!(store.revision() > r0, "重排应推进版本号");
        assert_eq!(store.all_ids(), vec!["c", "a", "b"]);
        assert_eq!(store.index_of("a"), Some(1));

        // id 集合不一致 → 拒绝且不改动本地库
        assert!(!store.reorder_local(&["a".into(), "b".into()]));
        assert!(!store.reorder_local(&["c".into(), "a".into(), "z".into()]));
        assert_eq!(store.all_ids(), vec!["c", "a", "b"]);
    }

    #[test]
    fn store_keeps_full_library_for_windowing() {
        let mut store = AppStore::default();
        for i in 0..250 {
            store.apply_event(&event(
                "wallpaper-added",
                sample_entry(&format!("id-{i}"), &format!("C:/x/{i}.mp4")),
            ));
        }
        // 状态仓保留全量（虚拟化发生在投影/实例化层，不是在这里截断）。
        assert_eq!(store.wallpapers().len(), 250);
        assert_eq!(store.view().item_count, 250);
    }

    // ── Task 6 / 7：设置面板与轮换池 ─────────────────────────────────────────

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

    fn pool(id: &str, name: &str, members: &[&str]) -> Pool {
        Pool {
            id: id.to_string(),
            name: name.to_string(),
            member_ids: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn entry(id: &str, path: &str, kind: &str) -> WallpaperEntry {
        serde_json::from_value(json!({
            "id": id,
            "file_path": path,
            "wallpaper_type": kind,
            "added_at": "1700000000",
        }))
        .expect("entry deserializes")
    }

    fn config_with_audio(volume: f32, muted: bool, speed: f32) -> AppConfig {
        serde_json::from_value(json!({
            "audio": { "volume": volume, "muted": muted },
            "video": { "hwdec": true, "speed": speed },
        }))
        .expect("config deserializes")
    }

    #[test]
    fn displays_default_to_primary_and_selection_persists() {
        let mut store = AppStore::default();
        store.apply_displays(vec![
            display("d1", "显示器 1", false),
            display("d2", "显示器 2", true),
        ]);
        assert_eq!(store.selected_display(), Some("d2"), "默认选中主显示器");
        assert_eq!(
            store.view().display_index,
            1,
            "per_monitor 下列表仅含具体显示器，主显示器索引为 1"
        );

        store.select_display(Some("d1".to_string()));
        assert_eq!(store.selected_display(), Some("d1"));
        // 重新拉取显示器时保留仍存在的选择
        store.apply_displays(vec![
            display("d1", "显示器 1", false),
            display("d2", "显示器 2", true),
        ]);
        assert_eq!(store.selected_display(), Some("d1"));

        // 选择消失 → 回退主显示器
        store.apply_displays(vec![display("d2", "显示器 2", true)]);
        assert_eq!(store.selected_display(), Some("d2"));

        // 未知 id 视为无选择；per_monitor 下下拉索引回退首个位置（0）。
        store.select_display(Some("nope".to_string()));
        assert_eq!(store.selected_display(), None);
        assert_eq!(store.view().display_index, 0);
    }

    #[test]
    fn display_options_follow_arrangement() {
        let mut store = AppStore::default();
        // 排列尚未加载（None）→ 按 per_monitor 处理。
        store.apply_displays(vec![
            display("d1", "显示器 1", false),
            display("d2", "显示器 2", true),
        ]);
        let options = store.display_options();
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, vec!["d1", "d2"], "per_monitor 仅列具体显示器");
        assert_eq!(store.selected_display(), Some("d2"), "默认为主显示器");
        let view = store.view();
        assert!(view.display_dropdown_enabled, "per_monitor 下可选");

        // all_same → 仅「全部显示器」，选择固定为 "all"。
        store.apply_arrangement(Arrangement::AllSame);
        let options = store.display_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].value, "all");
        assert_eq!(options[0].label, "全部显示器");
        assert_eq!(store.selected_display(), Some("all"));
        let view = store.view();
        assert_eq!(view.display_index, 0);
        assert!(!view.display_dropdown_enabled, "非 per_monitor 下置灰");
        assert!(view.pause_enabled && view.prev_enabled && view.next_enabled);

        // span → 同上。
        store.apply_arrangement(Arrangement::Span);
        assert_eq!(store.display_options()[0].value, "all");
        assert_eq!(store.selected_display(), Some("all"));

        // 切回 per_monitor → 收敛到主显示器（选择 "all" 失效）。
        store.apply_arrangement(Arrangement::PerMonitor);
        assert_eq!(store.selected_display(), Some("d2"));
        assert!(store.view().display_dropdown_enabled);
    }

    #[test]
    fn playback_buttons_enabled_when_target_exists() {
        // 默认进入（有显示器、per_monitor、已选主屏）播放控制应可用。
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        let view = store.view();
        assert!(view.pause_enabled && view.prev_enabled && view.next_enabled);

        // all_same 下（选择固定为 "all"）同样可用。
        store.apply_arrangement(Arrangement::AllSame);
        let view = store.view();
        assert!(view.pause_enabled && view.prev_enabled && view.next_enabled);

        // 无任何显示器 → 无可用目标，三键禁用。
        let mut empty = AppStore::default();
        empty.apply_displays(vec![]);
        empty.apply_arrangement(Arrangement::AllSame);
        let view = empty.view();
        assert!(!view.pause_enabled && !view.prev_enabled && !view.next_enabled);
    }

    #[test]
    fn audio_runtime_values_seed_once_from_config() {
        let mut store = AppStore::default();
        assert_eq!(store.volume(), 0.8, "未加载配置前使用默认音量");
        assert_eq!(store.speed(), 1.0);

        store.apply_config(config_with_audio(0.3, true, 1.5));
        assert_eq!(store.volume(), 0.3);
        assert!(store.muted());
        assert_eq!(store.speed(), 1.5);

        // 用户运行时改动（不落配置）
        store.set_volume(0.9);
        store.set_muted(false);
        store.set_speed(2.0);

        // `config-changed` 重拉不应覆盖运行时值
        store.apply_config(config_with_audio(0.3, true, 1.5));
        assert_eq!(store.volume(), 0.9);
        assert!(!store.muted());
        assert_eq!(store.speed(), 2.0);
    }

    #[test]
    fn unit_rows_merge_arrangement_displays_and_state() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.apply_pools(vec![pool("p1", "风景", &[])]);
        store.apply_unit_states(vec![UnitStateView {
            key: "d1".into(),
            active_pool: Some("p1".into()),
            enabled: true,
        }]);

        let rows = store.unit_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "d1");
        assert_eq!(rows[0].label, "[主] 显示器 1");
        assert_eq!(rows[0].pool_index, 1, "池选项首项为'全部壁纸池'");
        assert!(rows[0].enabled);

        // 切到 AllSame → 单元变为全局 "all"；无对应状态时回填为未启用/全部池
        store.apply_arrangement(Arrangement::AllSame);
        let rows = store.unit_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "all");
        assert_eq!(rows[0].pool_index, 0, "无状态时回填'全部壁纸池'");
        assert!(!rows[0].enabled);
    }

    #[test]
    fn pool_rows_and_member_rows_reflect_order_and_membership() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![
            entry("w1", "C:/w/a.png", "Image"),
            entry("w2", "C:/w/b.png", "Video"),
        ]);
        store.apply_pools(vec![
            pool("p1", "池一", &["w2", "w1"]),
            pool("p2", "池二", &[]),
        ]);

        let rows = store.pool_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].members, 2);
        assert!(!rows[0].up && rows[0].down);
        assert!(rows[1].up && !rows[1].down);

        let members = store.member_rows("p1");
        assert_eq!(
            members.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["w2", "w1"]
        );
        assert_eq!(members[0].name, "b.png");
        assert!(!members[0].up && members[0].down);

        // 编辑目标随池删除而失效
        store.set_editing_pool(Some("p1".to_string()));
        assert_eq!(store.editing_pool(), Some("p1"));
        store.apply_pools(vec![pool("p2", "池二", &[])]);
        assert_eq!(store.editing_pool(), None);
    }

    #[test]
    fn available_rows_skip_members_web_and_duplicates() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![
            entry("w1", "C:/w/a.png", "Image"),
            entry("w2", "C:/w/a.png", "Image"), // 与 w1 路径重复
            entry("w3", "C:/w/c.web", "Web"),   // 网页类型
            entry("w4", "C:/w/d.mp4", "Video"),
        ]);
        store.apply_pools(vec![pool("p1", "池一", &["w1"])]);
        let rows = store.available_rows("p1");
        // w1 已在池内、w2 路径重复、w3 为 Web → 仅剩 w4
        assert_eq!(
            rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["w4"]
        );
    }

    #[test]
    fn view_projects_playback_buttons_and_settings_indices() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_playback(Some(WallpaperState::Paused));
        let view = store.view();
        assert!(view.pause_enabled);
        assert_eq!(view.pause_label, "恢复");
        assert!(view.prev_enabled && view.next_enabled);
        assert_eq!(view.display_index, 0);
        assert!(
            view.display_dropdown_enabled,
            "per_monitor 下目标显示器可选"
        );
        assert_eq!(view.scaling_index, 0, "默认缩放模式 fill");
        assert_eq!(view.arrangement_index, 0);
        assert_eq!(view.fullscreen_index, 0, "默认全屏处置 terminate");
        assert_eq!(view.volume_percent, 80);
        assert!(view.audio_controls_enabled);
        assert_eq!(view.rotation_interval, 30);
        assert!(view.about_version.contains("MirrorStar Wallpaper v"));
    }

    #[test]
    fn scaling_key_resolution_follows_arrangement() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        // 默认 per_monitor + 已选中主显示器 → 用具体显示器 id。
        assert_eq!(store.scaling_key(), "d1");
        store.apply_arrangement(Arrangement::AllSame);
        assert_eq!(store.scaling_key(), "all", "all_same 恒用全局 key");
        store.apply_arrangement(Arrangement::Span);
        assert_eq!(store.scaling_key(), "all", "span 恒用全局 key");
        // per_monitor 下无选择 → 回退 "all"。
        store.apply_arrangement(Arrangement::PerMonitor);
        store.select_display(None);
        assert_eq!(store.scaling_key(), "all", "无选择时回退全局 key");
    }

    #[test]
    fn scaling_index_follows_target_display() {
        let mut store = AppStore::default();
        store.apply_displays(vec![
            display("d1", "显示器 1", true),
            display("d2", "显示器 2", false),
        ]);
        store.apply_arrangement(Arrangement::PerMonitor);
        // d1 记忆「适应」(index 1)、d2 记忆「填充」(index 0)。
        store.set_scaling_mode_for_key("d1".to_string(), ScalingMode::Fit);
        store.set_scaling_mode_for_key("d2".to_string(), ScalingMode::Fill);

        store.select_display(Some("d1".to_string()));
        assert_eq!(
            store.view().scaling_index,
            1,
            "选中 d1 显示其记忆的「适应」"
        );
        store.select_display(Some("d2".to_string()));
        assert_eq!(store.view().scaling_index, 0, "切到 d2 应刷新为「填充」");
    }

    #[test]
    fn scaling_falls_back_to_all_then_default() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_arrangement(Arrangement::PerMonitor);
        // 未命中任何项 → 默认 Fill（index 0）。
        assert_eq!(store.scaling_mode_for_key("d1"), ScalingMode::Fill);
        // 仅配置了 "all" 兜底 → 精确未命中时取其值。
        store.set_scaling_mode_for_key("all".to_string(), ScalingMode::Tile);
        assert_eq!(store.scaling_mode_for_key("d1"), ScalingMode::Tile);
        assert_eq!(store.view().scaling_index, 3, "兜底后下拉显示「平铺」");
        // 精确命中优先于 "all"。
        store.set_scaling_mode_for_key("d1".to_string(), ScalingMode::Center);
        assert_eq!(store.scaling_mode_for_key("d1"), ScalingMode::Center);
        assert_eq!(store.view().scaling_index, 4);
    }

    #[test]
    fn scaling_mapping_backfilled_from_config_snapshot() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        // 配置快照携带 per-display 记忆：d1 → tile、all → fit。
        let config: AppConfig = serde_json::from_value(json!({
            "arrangement": "per_monitor",
            "scaling": { "modes": { "d1": "tile", "all": "fit" } },
        }))
        .expect("config deserializes");
        store.apply_config(config);
        assert_eq!(store.scaling_mode_for_key("d1"), ScalingMode::Tile);
        // 回填后下拉索引与配置一致（tile → 3），未被"全部"兜底覆盖。
        assert_eq!(store.view().scaling_index, 3, "回填后下拉应显示「平铺」");
    }

    #[test]
    fn audio_disabled_event_disables_audio_controls() {
        let mut store = AppStore::default();
        store.apply_event(&event("audio-disabled", Value::Null));
        assert!(!store.view().audio_controls_enabled);
    }

    // ── Task 7：显示器热插拔后的缩放残留清理与选择收敛 ─────────────────────

    #[test]
    fn apply_displays_prunes_stale_scaling_modes_and_reconciles_selection() {
        let mut store = AppStore::default();
        store.apply_displays(vec![
            display("d1", "显示器 1", true),
            display("d2", "显示器 2", false),
        ]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.set_scaling_mode_for_key("d1".to_string(), ScalingMode::Fit);
        store.set_scaling_mode_for_key("d2".to_string(), ScalingMode::Tile);
        store.set_scaling_mode_for_key("all".to_string(), ScalingMode::Center);
        store.select_display(Some("d2".to_string()));
        assert_eq!(store.scaling_key(), "d2");

        // 拔出 d2（仍在的 d1 为主显示器）。
        store.apply_displays(vec![display("d1", "显示器 1", true)]);

        // 已消失显示器的缩放记忆被清除，"all" 广播键保留，仍在的 d1 记忆保留。
        let modes = store.scaling_modes_map();
        assert!(!modes.contains_key("d2"), "已拔出 d2 的缩放记忆应被清除");
        assert!(modes.contains_key("d1"), "仍在的 d1 记忆应保留");
        assert_eq!(
            modes.get("all"),
            Some(&ScalingMode::Center),
            "\"all\" 广播键必须保留"
        );
        // 选择收敛到仍存在的主显示器，且 scaling_key 不再指向已拔出的 id。
        assert_eq!(store.selected_display(), Some("d1"));
        assert_eq!(store.scaling_key(), "d1");
    }

    #[test]
    fn apply_displays_converges_selection_to_first_when_primary_removed() {
        let mut store = AppStore::default();
        store.apply_displays(vec![
            display("d1", "显示器 1", false),
            display("d2", "显示器 2", true),
        ]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.select_display(Some("d2".to_string()));
        assert_eq!(store.scaling_key(), "d2");

        // 拔出主显示器 d2 → 无主显示器标记时收敛到首个仍存在的显示器 d1。
        store.apply_displays(vec![display("d1", "显示器 1", false)]);
        assert_eq!(store.selected_display(), Some("d1"));
        assert_eq!(store.scaling_key(), "d1", "不再指向已拔出的 d2");
    }

    #[test]
    fn apply_event_displays_changed_updates_list_prunes_scaling_and_converges() {
        let mut store = AppStore::default();
        store.apply_displays(vec![
            display("d1", "显示器 1", true),
            display("d2", "显示器 2", false),
        ]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.set_scaling_mode_for_key("d2".to_string(), ScalingMode::Tile);
        store.select_display(Some("d2".to_string()));
        assert_eq!(store.scaling_key(), "d2");
        assert_eq!(store.display_options().len(), 2);
        let revision_before = store.settings_revision();

        // 拔出 d2（d1 仍在且为主显示器）——经通用 `apply_event` 单次投影更新状态仓。
        let payload = serde_json::to_value(vec![display("d1", "显示器 1", true)])
            .expect("显示器列表应可序列化");
        store.apply_event(&event("displays-changed", payload));

        assert_eq!(store.display_options().len(), 1, "显示器列表应随事件更新");
        assert!(
            !store.scaling_modes_map().contains_key("d2"),
            "已拔出 d2 的缩放记忆应被清理"
        );
        assert_eq!(
            store.selected_display(),
            Some("d1"),
            "选择应收敛到仍在的主显示器"
        );
        assert_eq!(store.scaling_key(), "d1");
        assert!(
            store.settings_revision() > revision_before,
            "显示器变化应推进设置版本号（单次投影）"
        );
    }

    #[test]
    fn apply_event_displays_changed_ignores_malformed_payload() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        // 负载非显示器数组（解析失败）→ 静默忽略，既有状态不变、不 panic。
        store.apply_event(&event("displays-changed", json!({ "unexpected": true })));
        assert_eq!(store.display_options().len(), 1);
        assert_eq!(store.selected_display(), Some("d1"));
    }

    #[test]
    fn apply_event_rotated_updates_active_when_key_matches() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_arrangement(Arrangement::PerMonitor);
        assert_eq!(store.selected_display(), Some("d1"));

        store.apply_event(&event(
            "wallpaper-rotated",
            json!({ "key": "d1", "wallpaper_id": "w1" }),
        ));
        assert_eq!(
            store.view().active_id,
            "w1",
            "命中目标单元应更新「当前」角标"
        );
    }

    #[test]
    fn apply_event_rotated_ignores_other_unit_key() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.set_active("old".to_string());

        // 非选中/不存在的单元 key：角标不得跳到用户未在操作的显示器。
        store.apply_event(&event(
            "wallpaper-rotated",
            json!({ "key": "nope", "wallpaper_id": "w1" }),
        ));
        assert_eq!(store.view().active_id, "old");
    }

    #[test]
    fn apply_event_rotated_ignores_malformed_payload() {
        let mut store = AppStore::default();
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        store.apply_arrangement(Arrangement::PerMonitor);
        store.set_active("old".to_string());

        // 缺 wallpaper_id / 非字符串：静默忽略、不 panic、角标保持原值。
        store.apply_event(&event("wallpaper-rotated", json!({ "key": "d1" })));
        assert_eq!(store.view().active_id, "old");
        store.apply_event(&event(
            "wallpaper-rotated",
            json!({ "key": "d1", "wallpaper_id": 42 }),
        ));
        assert_eq!(store.view().active_id, "old");
    }

    #[test]
    fn current_unit_key_matches_arrangement() {
        // per_monitor 且无选择 → 回退 "all"。
        let mut store = AppStore::default();
        store.apply_arrangement(Arrangement::PerMonitor);
        assert_eq!(store.current_unit_key(), "all");

        // per_monitor 有选择 → 等于选中显示器。
        store.apply_displays(vec![
            display("d1", "显示器 1", false),
            display("d2", "显示器 2", true),
        ]);
        store.select_display(Some("d1".to_string()));
        assert_eq!(store.current_unit_key(), "d1");

        // all_same / span → 恒为 "all"。
        store.apply_arrangement(Arrangement::AllSame);
        assert_eq!(store.current_unit_key(), "all");
        store.apply_arrangement(Arrangement::Span);
        assert_eq!(store.current_unit_key(), "all");
    }

    #[test]
    fn settings_revision_bumps_on_settings_mutations() {
        let mut store = AppStore::default();
        let r0 = store.settings_revision();
        store.set_volume(0.5);
        let r1 = store.settings_revision();
        assert!(r1 > r0, "音量改动需重投影滑块位置");
        store.apply_displays(vec![display("d1", "显示器 1", true)]);
        let r2 = store.settings_revision();
        assert!(r2 > r1, "显示器变化应推进设置版本号");
        store.apply_pools(vec![pool("p1", "池一", &[])]);
        assert!(store.settings_revision() > r2, "池变化应推进设置版本号");
    }

    // ── 维度③：全量快照 / 删除后的引用收敛与内存有界 ──────────────────────────

    #[test]
    fn full_snapshot_prunes_dangling_ui_state_and_marks() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![entry("a", "C:/x/a.mp4", "Video")]);
        store.select(Some("a".to_string()));
        store.set_active("a".to_string());
        store.apply_event(&event(
            "wallpaper-source-missing",
            json!({ "id": "a", "file_path": "C:/x/a.mp4" }),
        ));
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/a.mp4" }),
        ));
        assert!(store.is_source_missing("a"));
        assert!(store.is_thumb_failed("C:/x/a.mp4"));

        // 快照中已不含 a（例如壳侧在无事件的情况下刷新了库）
        store.apply_wallpapers(vec![entry("b", "C:/x/b.mp4", "Video")]);
        let view = store.view();
        assert_eq!(view.selected_id, "", "快照后悬挂选中 id 应被清除");
        assert_eq!(view.active_id, "", "快照后悬挂激活 id 应被清除");
        assert!(!store.is_source_missing("a"), "已删除条目的缺失标记应清除");
        assert!(
            !store.is_thumb_failed("C:/x/a.mp4"),
            "已删除条目的缩略图失败标记应清除"
        );
    }

    #[test]
    fn full_snapshot_keeps_marks_for_remaining_entries() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![
            entry("a", "C:/x/a.mp4", "Video"),
            entry("b", "C:/x/b.mp4", "Video"),
        ]);
        store.select(Some("a".to_string()));
        store.apply_event(&event("wallpaper-source-missing", json!({ "id": "a" })));
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/b.mp4" }),
        ));

        // 快照仍含 a、b：选中与标记应保留
        store.apply_wallpapers(vec![
            entry("a", "C:/x/a.mp4", "Video"),
            entry("b", "C:/x/b.mp4", "Video"),
        ]);
        assert_eq!(store.view().selected_id, "a");
        assert!(store.is_source_missing("a"));
        assert!(store.is_thumb_failed("C:/x/b.mp4"));
    }

    #[test]
    fn wallpaper_removed_clears_thumb_failed_path_mark() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![entry("a", "C:/x/a.mp4", "Video")]);
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/a.mp4" }),
        ));
        assert!(store.is_thumb_failed("C:/x/a.mp4"));
        store.apply_event(&event("wallpaper-removed", json!("a")));
        assert!(
            !store.is_thumb_failed("C:/x/a.mp4"),
            "条目删除后其缩略图失败标记不应残留"
        );
    }

    #[test]
    fn wallpaper_updated_with_thumbnail_clears_thumb_failed_mark() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![entry("a", "C:/x/a.mp4", "Video")]);
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/a.mp4" }),
        ));
        assert!(store.is_thumb_failed("C:/x/a.mp4"));

        // 缩略图回填成功（wallpaper-updated 携带非空 thumbnail）→ 应清除失败标记。
        // 否则 card_state 会在 thumbs::plan 之前命中 is_thumb_failed，把新缩略图遮蔽回类型图标。
        store.apply_event(&event(
            "wallpaper-updated",
            json!({
                "id": "a",
                "file_path": "C:/x/a.mp4",
                "wallpaper_type": "Video",
                "added_at": "1700000000",
                "thumbnail": "C:/t/thumb_a.jpg",
            }),
        ));
        assert_eq!(store.wallpapers()[0].thumbnail, "C:/t/thumb_a.jpg");
        assert!(
            !store.is_thumb_failed("C:/x/a.mp4"),
            "缩略图回填成功后应清除失败标记，避免遮蔽新缩略图"
        );

        // 空 thumbnail 的更新（如 fallback null/空回填）不应误清标记。
        store.apply_event(&event(
            "wallpaper-thumbnail-failed",
            json!({ "file_path": "C:/x/a.mp4" }),
        ));
        store.apply_event(&event(
            "wallpaper-updated",
            json!({
                "id": "a",
                "file_path": "C:/x/a.mp4",
                "wallpaper_type": "Video",
                "added_at": "1700000000",
            }),
        ));
        assert!(
            store.is_thumb_failed("C:/x/a.mp4"),
            "空 thumbnail 的更新不应清除失败标记"
        );
    }

    #[test]
    fn wallpaper_updated_with_thumbnail_clears_source_missing_mark() {
        let mut store = AppStore::default();
        store.apply_wallpapers(vec![entry("a", "C:/x/a.mp4", "Video")]);
        store.apply_event(&event("wallpaper-source-missing", json!({ "id": "a" })));
        assert!(store.is_source_missing("a"), "应标记源文件缺失");

        // 源文件恢复并回填缩略图成功 → 缺失标记应清除。否则 `card_state` 会先命中
        // `is_source_missing`，把已恢复的卡片永久显示为"缺失"。
        store.apply_event(&event(
            "wallpaper-updated",
            json!({
                "id": "a",
                "file_path": "C:/x/a.mp4",
                "wallpaper_type": "Video",
                "added_at": "1700000000",
                "thumbnail": "C:/t/thumb_a.jpg",
            }),
        ));
        assert_eq!(store.wallpapers()[0].thumbnail, "C:/t/thumb_a.jpg");
        assert!(
            !store.is_source_missing("a"),
            "缩略图回填成功后应清除源文件缺失标记"
        );
    }

    // ── 维度④ 4.4：过滤 + 删除 + 重排 + 键盘焦点的交叉一致性 ─────────────────

    #[test]
    fn filter_remove_and_reorder_keep_filtered_sequence_consistent() {
        let mut store = AppStore::default();
        for (i, name) in ["beach", "city", "beach", "city", "beach"]
            .iter()
            .enumerate()
        {
            store.apply_event(&event(
                "wallpaper-added",
                sample_entry(&format!("id-{i}"), &format!("C:/w/{name}{i}.png")),
            ));
        }
        store.set_search_keyword("beach".to_string());
        assert_eq!(store.filtered_count(), 3);
        assert_eq!(store.filtered_indices(), vec![0, 2, 4]);

        // 删除一个命中项后，过滤序列随之收敛（不残留悬挂引用）
        store.apply_event(&event("wallpaper-removed", json!("id-2")));
        assert_eq!(store.filtered_count(), 2);
        assert_eq!(store.filtered_indices(), vec![0, 3]);
        assert_eq!(store.filtered_id_at(1).as_deref(), Some("id-4"));
        assert!(store.filtered_id_at(2).is_none(), "越界应返回 None");

        // 清除过滤后按全量重排，过滤序列仍与库一致
        store.set_search_keyword(String::new());
        assert!(store.reorder_local(&[
            "id-4".to_string(),
            "id-0".to_string(),
            "id-3".to_string(),
            "id-1".to_string(),
        ]));
        assert_eq!(store.all_ids(), vec!["id-4", "id-0", "id-3", "id-1"]);
        store.set_search_keyword("beach".to_string());
        assert_eq!(store.filtered_indices(), vec![0, 1]);

        // 键盘焦点索引在删除后失效：越界应被当作"无焦点"并可重新进入
        assert_eq!(
            keynav_move(Some(9), store.filtered_count()),
            Some(0),
            "越界焦点应回落到首项"
        );
    }

    /// 测试用薄封装：镜像 `keynav::move_focus` 的"越界即无焦点、正向落首项"语义。
    fn keynav_move(current: Option<usize>, count: usize) -> Option<usize> {
        crate::keynav::move_focus(current, crate::keynav::FocusMove::Next, count, 5)
    }
}
