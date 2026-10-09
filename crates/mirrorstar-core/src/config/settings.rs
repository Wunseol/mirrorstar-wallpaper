//! 应用配置数据模型与范围校验。
//!
//! 定义 [`AppConfig`] 及其子配置（音频 / 视频 / GIF / 显示 / 通用 / 暂停）的
//! 结构、序列化与反序列化、范围校验逻辑。配置文件以 TOML 格式持久化，由
//! [`crate::config::ConfigManager`] 负责加载与保存。
//!
//! ## validate 校验策略
//!
//! 各子 config 的 `validate(&mut self)` 方法**统一采用"内部修正 + warn 日志，调用方透明"
//! 策略**，而非返回 `Result` 描述校验失败原因：
//!
//! - **就地修正**：越界或非法值（如 `volume = 1.5`、`speed = -2.0`、
//!   `balanced_keep_frames = 0`、`NaN`）在 `validate()` 内部 clamp 到合法范围或回退到
//!   默认值，方法签名保持 `fn validate(&mut self)`（无返回值）。
//! - **warn 日志**：每次修正通过 `tracing::warn!` 记录原值与修正后值，便于排查
//!   配置文件被外部编辑导致的越界问题，但不向调用方传播错误。
//! - **调用方透明**：调用方（`ConfigManager::load_config` / `update_config`）无需
//!   处理 `Result`，修正后的配置始终处于可用状态。这与"配置损坏时仍要保证应用可启动"
//!   的整体设计目标一致（参见 [`crate::config::manager::ConfigManager`] 的回退语义）。
//!
//! 该策略统一适用于 [`AudioConfig::validate`] / [`VideoConfig::validate`] /
//! [`GifConfig::validate`]（[`AppConfig::validate`] 是聚合入口，依次调用各子 config 的
//! `validate`）。未来新增的子 config 应遵循同一策略；如需引入"硬校验失败"语义，
//! 应使用独立的 `Result` 返回类型方法（如 `validate_strict`），避免破坏既有契约。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wallpaper::{GifMemoryStrategy, ScalingMode};

/// 应用配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// 多屏排列（顶层布局策略，DR-2 / 多屏排列布局域）：per_monitor | all_same | span。
    /// 独立于轮换调度，属于"显示器布局策略"层；旧版 `[rotation] arrangement` 由
    /// [`AppConfig::migrate_legacy_arrangement`] 自动迁移。
    ///
    /// 采用宽松反序列化：未知取值回退默认（`per_monitor`）并 warn，不使整份配置失败。
    #[serde(default, deserialize_with = "de_arrangement_lenient")]
    pub arrangement: Arrangement,
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub pause: PauseConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub gif: GifConfig,
    #[serde(default)]
    pub rotation: RotationConfig,
    /// 缩放模式（按显示器记忆）
    #[serde(default)]
    pub scaling: ScalingConfig,
}

impl AppConfig {
    /// C02 修复：范围校验
    ///
    /// 对各子 config 的数值字段执行范围校验，越界值 clamp 到合法范围：
    /// - `AudioConfig.volume`：clamp 到 `[0.0, 1.0]`
    /// - `VideoConfig.speed`：clamp 到 `(0.0, MAX_VIDEO_SPEED]`（≤0 或 NaN 时回退到默认 1.0；C-017：超过上限 `MAX_VIDEO_SPEED`（10.0）回退到默认 1.0）
    /// - `GifConfig.balanced_keep_frames`：clamp 到 `[1, MAX_BALANCED_KEEP_FRAMES]`（越界回退到默认值）
    ///
    /// 该方法不返回错误，越界值就地修正后保证配置处于可用状态。供
    /// [`crate::config::ConfigManager::load_config`] 反序列化后调用，避免
    /// 用户手动编辑 `config.toml` 写入越界值被静默接受。
    pub fn validate(&mut self) {
        self.audio.validate();
        self.video.validate();
        self.gif.validate();
        self.rotation.validate();
    }

    /// 迁移旧版 `[rotation] arrangement`（v0.7.0 前）到顶层 `arrangement`。
    ///
    /// 旧配置中 `rotation.arrangement` 经 `RotationConfig::arrangement_legacy`
    /// 反序列化进入本结构；此处将其提升为顶层字段并清空 legacy（`skip_serializing`
    /// 保证保存时旧键不再写出）。
    ///
    /// `explicit` 表示顶层 `arrangement` 键是否**显式出现在配置文件文本中**：
    /// 显式出现时顶层值优先（**即使它恰好等于 [`Arrangement::default`]** 也不被
    /// legacy 覆盖）；未显式出现时才采用 legacy。无论是否采用，legacy 键都会被清除。
    /// 返回是否发生迁移流程（存在 legacy 键即为 `true`）。
    pub fn migrate_legacy_arrangement(&mut self, explicit: bool) -> bool {
        let Some(legacy) = self.rotation.arrangement_legacy else {
            return false;
        };
        if explicit {
            tracing::warn!(
                arrangement = ?self.arrangement,
                legacy = ?legacy,
                "顶层 arrangement 已显式配置，忽略旧版 [rotation] arrangement"
            );
        } else {
            self.arrangement = legacy;
            tracing::warn!(
                arrangement = ?self.arrangement,
                "检测到旧版 [rotation] arrangement 配置，已迁移到顶层 arrangement"
            );
        }
        self.rotation.arrangement_legacy = None;
        true
    }

    /// 一次性迁移：默认切换顺序由 `shuffle_bag` 改为 `sequential`。
    ///
    /// 旧配置在磁盘上持久化了 `order = "shuffle_bag"`，仅改 `Order` 的 `Default`
    /// 不会影响这些安装。迁移标记未置位时先置位；若取值仍为 `ShuffleBag`，改写为
    /// `Sequential` 并 warn。已置位时不做任何改写（尊重用户后续的手动选择）。
    ///
    /// 返回是否发生了取值改写。迁移是幂等的：若本次只在内存中生效而未落盘，
    /// 下次启动会以同样结果重新求值（无需调用方额外补偿）；应用自身写回配置时
    /// （`ConfigManager::update_config`）会置位标记并落盘，此后不再改写。
    pub fn migrate_order_default(&mut self) -> bool {
        if self.rotation.order_default_migrated {
            return false;
        }
        self.rotation.order_default_migrated = true;
        if self.rotation.order == Order::ShuffleBag {
            self.rotation.order = Order::Sequential;
            tracing::warn!("检测到旧版默认切换顺序 shuffle_bag，已迁移为 sequential（顺序循环）");
            true
        } else {
            false
        }
    }
}

/// 通用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// 开机自启
    #[serde(default)]
    pub auto_start: bool,
    /// 关闭窗口时最小化到托盘
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            auto_start: false,
            minimize_to_tray: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 音频配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// 全局音量 (0.0 ~ 1.0)
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// 是否静音
    #[serde(default)]
    pub muted: bool,
    /// 预览音量 (0.0 ~ 1.0)，默认 0.5（中等音量）
    #[serde(default = "default_preview_volume")]
    pub preview_volume: f32,
    /// 预览静音
    #[serde(default)]
    pub preview_muted: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            volume: 0.8,
            muted: false,
            preview_volume: 0.5,
            preview_muted: false,
        }
    }
}

impl AudioConfig {
    /// C02 修复：volume 范围校验，clamp 到 `[0.0, 1.0]`
    ///
    /// NaN 视为无效，回退到默认 0.8。
    pub fn validate(&mut self) {
        if self.volume.is_nan() {
            tracing::warn!(
                volume = self.volume,
                "AudioConfig.volume 为 NaN，回退到默认 0.8"
            );
            self.volume = default_volume();
        } else if self.volume < 0.0 {
            tracing::warn!(volume = self.volume, "AudioConfig.volume < 0，clamp 到 0.0");
            self.volume = 0.0;
        } else if self.volume > 1.0 {
            tracing::warn!(volume = self.volume, "AudioConfig.volume > 1，clamp 到 1.0");
            self.volume = 1.0;
        }

        if self.preview_volume.is_nan() {
            tracing::warn!(
                preview_volume = self.preview_volume,
                "AudioConfig.preview_volume 为 NaN，回退到默认 0.5"
            );
            self.preview_volume = default_preview_volume();
        } else if self.preview_volume < 0.0 {
            tracing::warn!(
                preview_volume = self.preview_volume,
                "AudioConfig.preview_volume < 0，clamp 到 0.0"
            );
            self.preview_volume = 0.0;
        } else if self.preview_volume > 1.0 {
            tracing::warn!(
                preview_volume = self.preview_volume,
                "AudioConfig.preview_volume > 1，clamp 到 1.0"
            );
            self.preview_volume = 1.0;
        }
    }
}

fn default_volume() -> f32 {
    0.8
}

fn default_preview_volume() -> f32 {
    0.5
}

/// 全屏时壁纸处置策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FullscreenAction {
    /// 不做任何处置
    #[serde(rename = "none")]
    None,
    /// 暂停（进程驻留，退出全屏即时恢复）
    #[serde(rename = "pause")]
    Pause,
    /// 终止子进程（激进释放 CPU/GPU 内存，退出全屏冷启动恢复）
    #[default]
    #[serde(rename = "terminate")]
    Terminate,
}

/// 暂停配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseConfig {
    /// 全屏时处置策略（默认终止）
    ///
    /// 采用宽松反序列化：未知取值回退默认（`terminate`）并 warn，不使整份配置失败。
    #[serde(default, deserialize_with = "de_fullscreen_action_lenient")]
    pub fullscreen_action: FullscreenAction,
    /// 电池供电时暂停
    #[serde(default)]
    pub pause_on_battery: bool,
}

impl Default for PauseConfig {
    fn default() -> Self {
        Self {
            fullscreen_action: FullscreenAction::Terminate,
            pause_on_battery: false,
        }
    }
}

/// 壁纸排列方式（三值编排，DR-2 / 设计 §4.4）
///
/// - `PerMonitor`：每屏一个调度单元，1 屏 = 1 次 apply，N 渲染器。
/// - `AllSame`：将全部显示器视为整体（单一调度单元），1 张壁纸 → 每屏各 apply 一次
///   （顺序执行），N 渲染器；换图窗口期各屏瞬时新/旧混杂为已知接受项（DR-39）。
/// - `Span`：将全部显示器视为整体（单一调度单元），1 个跨屏渲染器贯通虚拟桌面，1 次 apply。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Arrangement {
    /// 每个显示器独立壁纸
    #[default]
    #[serde(rename = "per_monitor")]
    PerMonitor,
    /// 每屏同图（每个显示器各设一次）
    #[serde(rename = "all_same")]
    AllSame,
    /// 跨显示器单张壁纸
    #[serde(rename = "span")]
    Span,
}

/// 缩放模式配置（按显示器记忆）
///
/// 以「显示器 id → 缩放模式」映射持久化每屏缩放模式；特殊键 `"all"` 表示
/// 「全部显示器 / 新显示器兜底」。`BTreeMap` 保证序列化顺序稳定。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScalingConfig {
    /// display_id → 缩放模式；特殊键 "all" 表示「全部显示器 / 新显示器兜底」。
    ///
    /// 映射值采用宽松反序列化：未知缩放模式回退默认（`fill`）并 warn，不使整份配置失败。
    #[serde(default, deserialize_with = "de_scaling_modes_lenient")]
    pub modes: BTreeMap<String, ScalingMode>,
}

// ── UI 可编辑枚举的宽松反序列化 ────────────────────────────────────────────
//
// 配置文件可被用户手动编辑，单个未知枚举值不应导致整份 `AppConfig` 反序列化失败
// （见 requirements「配置加载容错」）。以下 `de_*_lenient` 在解析失败时回退该字段
// 默认值并记录 warn，不影响同文件其它字段的解析。
//
// 注意：不改动 `Arrangement` / `FullscreenAction` / `ScalingMode` 自身的 derive——
// 它们还被命令参数等其它路径使用，全局放宽会改变别处语义。

/// 宽松反序列化 [`Arrangement`]：未知取值回退默认（`per_monitor`）。
fn de_arrangement_lenient<'de, D>(deserializer: D) -> Result<Arrangement, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Arrangement::deserialize(deserializer) {
        Ok(value) => Ok(value),
        Err(e) => {
            tracing::warn!(error = %e, "arrangement 取值未知，已回退默认 per_monitor");
            Ok(Arrangement::default())
        }
    }
}

/// 宽松反序列化 [`FullscreenAction`]：未知取值回退默认（`terminate`）。
fn de_fullscreen_action_lenient<'de, D>(deserializer: D) -> Result<FullscreenAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match FullscreenAction::deserialize(deserializer) {
        Ok(value) => Ok(value),
        Err(e) => {
            tracing::warn!(error = %e, "fullscreen_action 取值未知，已回退默认 terminate");
            Ok(FullscreenAction::default())
        }
    }
}

/// 宽松反序列化 [`ScalingConfig::modes`] 的映射值。
///
/// 逐项解析每个显示器的缩放模式，单项解析失败（未知取值或非字符串）时回退默认
/// （`fill`），不影响同一映射内其它显示器条目。
fn de_scaling_modes_lenient<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ScalingMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, LenientScalingMode>::deserialize(deserializer)?;
    Ok(raw.into_iter().map(|(k, v)| (k, v.0)).collect())
}

/// [`ScalingMode`] 的宽松反序列化包装，供 [`de_scaling_modes_lenient`] 逐项使用。
struct LenientScalingMode(ScalingMode);

impl<'de> Deserialize<'de> for LenientScalingMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match ScalingMode::deserialize(deserializer) {
            Ok(mode) => Ok(LenientScalingMode(mode)),
            Err(e) => {
                tracing::warn!(error = %e, "scaling.modes 存在未知缩放模式，已回退默认 fill");
                Ok(LenientScalingMode(ScalingMode::default()))
            }
        }
    }
}

/// 采样算法（DR-4，设计 §7）
///
/// - `Sequential`：顺序循环，游览标追随用户自定义池顺序，id 锚定（DR-5）。
/// - `ShuffleBag`：洗牌袋，袋内不重复，袋空重洗，初始化剔除 Web（DR-18）。
/// - `PseudoRandom`：纯随机，每次独立抽取，可能连续同张。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    /// 顺序循环
    #[default]
    Sequential,
    /// 洗牌袋
    ShuffleBag,
    /// 纯随机
    PseudoRandom,
}

/// 视频配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// 硬件解码
    #[serde(default = "default_true")]
    pub hwdec: bool,
    /// 播放速度
    #[serde(default = "default_speed")]
    pub speed: f32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            hwdec: true,
            speed: 1.0,
        }
    }
}

impl VideoConfig {
    /// C02 / C-017 修复：speed 范围校验
    ///
    /// speed 必须 `> 0.0`（≤0 或 NaN 视为无效），无效时回退到默认 1.0。
    /// C-017：同时校验上限 `MAX_VIDEO_SPEED`（默认 10.0，与 ST-007 `validate_speed`
    /// 对齐），超过时回退到默认值，避免用户写入过大值导致播放器异常（音视频不同步、
    /// 解码跟不上等）。
    pub fn validate(&mut self) {
        if self.speed.is_nan() || self.speed <= 0.0 {
            tracing::warn!(
                speed = self.speed,
                "VideoConfig.speed 非法（应 > 0），回退到默认 1.0"
            );
            self.speed = default_speed();
        } else if self.speed > MAX_VIDEO_SPEED {
            tracing::warn!(
                speed = self.speed,
                max = MAX_VIDEO_SPEED,
                "VideoConfig.speed 超过上限，回退到默认 1.0"
            );
            self.speed = default_speed();
        }
    }
}

/// C-017 修复：VideoConfig.speed 上限常量
///
/// 防止用户通过手动编辑 `config.toml` 设置过大的 `speed`（如 999999），
/// 过高的播放速度可能导致播放器异常（音视频不同步、解码跟不上）。
/// 超过该上限时在 `validate()` 中回退到默认值 `1.0`（与 ST-007 `validate_speed`
/// 风格对齐，同样使用 `10.0` 作为上限）。
const MAX_VIDEO_SPEED: f32 = 10.0;

fn default_speed() -> f32 {
    1.0
}

/// GIF 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GifConfig {
    /// GIF 内存管理策略
    #[serde(default)]
    pub memory_strategy: GifMemoryStrategy,
    /// 平衡模式下保留的帧数
    #[serde(default = "default_gif_keep_frames")]
    pub balanced_keep_frames: usize,
    /// GIF 帧像素内存预算上限（MB），解码后的帧总内存不超过此值
    #[serde(default = "default_gif_max_memory_mb")]
    pub max_memory_mb: usize,
}

impl Default for GifConfig {
    fn default() -> Self {
        Self {
            memory_strategy: GifMemoryStrategy::default(),
            balanced_keep_frames: default_gif_keep_frames(),
            max_memory_mb: default_gif_max_memory_mb(),
        }
    }
}

/// C-005 修复：balanced_keep_frames 上限常量
///
/// 防止用户通过手动编辑 `config.toml` 设置过大的 `balanced_keep_frames`
/// （如 999999999）导致 GIF 解码后内存占用过高（OOM 风险）。
/// 超过该上限时在 `validate()` 中回退到默认值。
/// `pub`：命令层（src-tauri，外部 crate）在写入前用同一上限做硬校验（SEC-002），
/// 避免 1001~2.1B 的值被 `GifConfig::validate` 静默回退默认（前端收到 Ok 但实际存了默认值）。
pub const MAX_BALANCED_KEEP_FRAMES: usize = 1000;

impl GifConfig {
    /// C02 / C-005 修复：balanced_keep_frames 范围校验
    ///
    /// 必须 `≥ 1`（0 通常由反序列化缺失值或用户手动编辑导致），< 1 时回退到默认值。
    /// C-005：同时校验上限 `MAX_BALANCED_KEEP_FRAMES`（默认 1000），超过时回退到
    /// 默认值，避免用户写入过大值导致 GIF 解码 OOM。
    pub fn validate(&mut self) {
        if self.balanced_keep_frames < 1 {
            tracing::warn!(
                balanced_keep_frames = self.balanced_keep_frames,
                "GifConfig.balanced_keep_frames < 1（应 ≥ 1），回退到默认值"
            );
            self.balanced_keep_frames = default_gif_keep_frames();
        } else if self.balanced_keep_frames > MAX_BALANCED_KEEP_FRAMES {
            tracing::warn!(
                value = self.balanced_keep_frames,
                max = MAX_BALANCED_KEEP_FRAMES,
                default = default_gif_keep_frames(),
                "balanced_keep_frames 超过上限，回退到默认值"
            );
            self.balanced_keep_frames = default_gif_keep_frames();
        }

        // v41-W-012: max_memory_mb 范围校验
        if self.max_memory_mb < MIN_GIF_MEMORY_MB {
            tracing::warn!(
                value = self.max_memory_mb,
                min = MIN_GIF_MEMORY_MB,
                "GifConfig.max_memory_mb < 下限，回退到默认值"
            );
            self.max_memory_mb = default_gif_max_memory_mb();
        } else if self.max_memory_mb > MAX_GIF_MEMORY_MB_LIMIT {
            tracing::warn!(
                value = self.max_memory_mb,
                max = MAX_GIF_MEMORY_MB_LIMIT,
                "GifConfig.max_memory_mb 超过上限，回退到默认值"
            );
            self.max_memory_mb = default_gif_max_memory_mb();
        }
    }
}

fn default_gif_keep_frames() -> usize {
    crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES
}

/// GIF 内存预算默认值（MB）
const DEFAULT_GIF_MEMORY_MB: usize = 40;
/// GIF 内存预算下限（MB），低于此值大 GIF 几乎无法解码
const MIN_GIF_MEMORY_MB: usize = 10;
/// GIF 内存预算上限（MB），防止用户设置过大导致 OOM
const MAX_GIF_MEMORY_MB_LIMIT: usize = 500;

fn default_gif_max_memory_mb() -> usize {
    DEFAULT_GIF_MEMORY_MB
}

/// 轮换配置（设计 §4.1 / DR-37）
///
/// 配置壁纸轮换调度器的全局开关、定时与采样算法。多屏排列（`arrangement`）已提升为
/// 顶层 [`AppConfig::arrangement`]（布局策略层），不再属于本配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationConfig {
    /// 全局主开关；false 时仍执行"开机恢复当前壁纸"（见设计 §9）
    #[serde(default)]
    pub enabled: bool,
    /// 开机/唤醒是否"推进下一张"（而非仅恢复当前）
    #[serde(default)]
    pub on_boot: bool,
    /// 定时间隔（分钟）；下限 clamp 1 分钟（60s，防短间隔反复 spawn/kill 视频进程，DR-16）
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u32,
    /// 采样算法：sequential | shuffle_bag | pseudo_random（DR-4）
    #[serde(default)]
    pub order: Order,
    /// 一次性迁移标记：默认切换顺序由 `shuffle_bag` 改为 `sequential` 时置位，
    /// 保证旧配置只被迁移一次——用户之后手动选择「随机（不重复）」不会被下次
    /// 启动再次覆盖（见 [`AppConfig::migrate_order_default`]）。
    #[serde(default)]
    pub order_default_migrated: bool,
    /// 兼容旧版配置（v0.7.0 前）：旧 `[rotation] arrangement` 键在反序列化时进入此
    /// 字段，由 [`AppConfig::migrate_legacy_arrangement`] 迁移到顶层后清空；
    /// `skip_serializing` 保证保存时旧键不再写出。
    #[serde(rename = "arrangement", default, skip_serializing)]
    pub arrangement_legacy: Option<Arrangement>,
}

/// 定时间隔默认值（分钟）
const DEFAULT_INTERVAL_MINUTES: u32 = 30;
/// 定时间隔下限（分钟），DR-16：clamp 到 1 分钟，防短间隔反复 spawn/kill 视频进程。
/// `pub`：命令层 `update_rotation_config`（src-tauri，外部 crate）用同一下限做硬校验（P2-9）。
pub const MIN_INTERVAL_MINUTES: u32 = 1;

fn default_interval_minutes() -> u32 {
    DEFAULT_INTERVAL_MINUTES
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_boot: false,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            order: Order::default(),
            order_default_migrated: false,
            arrangement_legacy: None,
        }
    }
}

impl RotationConfig {
    /// DR-16：`interval_minutes` 范围校验，下限 clamp 到 1 分钟（60s）。
    ///
    /// 遵循现有 `validate()` 策略：就地修正 + `tracing::warn!`，调用方透明。
    pub fn validate(&mut self) {
        if self.interval_minutes < MIN_INTERVAL_MINUTES {
            tracing::warn!(
                value = self.interval_minutes,
                min = MIN_INTERVAL_MINUTES,
                default = DEFAULT_INTERVAL_MINUTES,
                "RotationConfig.interval_minutes < 下限（1 分钟，DR-16），回退到默认值"
            );
            self.interval_minutes = MIN_INTERVAL_MINUTES;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AppConfig default values ───────────────────────────────────────

    #[test]
    fn app_config_default_values() {
        let config = AppConfig::default();

        // general
        assert!(
            !config.general.auto_start,
            "auto_start should default to false"
        );
        assert!(
            config.general.minimize_to_tray,
            "minimize_to_tray should default to true"
        );

        // audio
        assert!(
            (config.audio.volume - 0.8).abs() < f32::EPSILON,
            "volume should default to 0.8"
        );
        assert!(!config.audio.muted, "muted should default to false");

        // pause
        assert_eq!(
            config.pause.fullscreen_action,
            FullscreenAction::Terminate,
            "fullscreen_action should default to Terminate"
        );
        assert!(
            !config.pause.pause_on_battery,
            "pause_on_battery should default to false"
        );

        // video
        assert!(config.video.hwdec, "hwdec should default to true");
        assert!(
            (config.video.speed - 1.0).abs() < f32::EPSILON,
            "speed should default to 1.0"
        );

        // gif
        assert_eq!(
            config.gif.memory_strategy,
            GifMemoryStrategy::Balanced,
            "memory_strategy should default to Balanced"
        );
        assert_eq!(
            config.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES,
            "balanced_keep_frames should default to DEFAULT_BALANCED_KEEP_FRAMES"
        );
    }

    // ── TOML round-trip ───────────────────────────────────────────────

    #[test]
    fn app_config_toml_roundtrip() {
        let config = AppConfig {
            arrangement: Arrangement::PerMonitor,
            general: GeneralConfig {
                auto_start: true,
                minimize_to_tray: false,
            },
            audio: AudioConfig {
                volume: 0.5,
                muted: true,
                ..AudioConfig::default()
            },
            pause: PauseConfig {
                fullscreen_action: FullscreenAction::None,
                pause_on_battery: true,
            },
            rotation: RotationConfig::default(),
            video: VideoConfig {
                hwdec: false,
                speed: 1.5,
            },
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Performance,
                balanced_keep_frames: 10,
                ..GifConfig::default()
            },
            scaling: ScalingConfig::default(),
        };

        let toml_str = toml::to_string_pretty(&config).expect("serialize config");
        let deserialized: AppConfig = toml::from_str(&toml_str).expect("deserialize config");

        assert_eq!(deserialized.general.auto_start, config.general.auto_start);
        assert_eq!(
            deserialized.general.minimize_to_tray,
            config.general.minimize_to_tray
        );
        assert!((deserialized.audio.volume - config.audio.volume).abs() < f32::EPSILON);
        assert_eq!(deserialized.audio.muted, config.audio.muted);
        assert_eq!(
            deserialized.pause.fullscreen_action,
            config.pause.fullscreen_action
        );
        assert_eq!(
            deserialized.pause.pause_on_battery,
            config.pause.pause_on_battery
        );
        assert_eq!(deserialized.video.hwdec, config.video.hwdec);
        assert!((deserialized.video.speed - config.video.speed).abs() < f32::EPSILON);
        assert_eq!(deserialized.gif.memory_strategy, config.gif.memory_strategy);
        assert_eq!(
            deserialized.gif.balanced_keep_frames,
            config.gif.balanced_keep_frames
        );
    }

    // ── Missing field fallback ─────────────────────────────────────────

    #[test]
    fn empty_tables_use_defaults() {
        let toml_str = r#"
[general]
[audio]
[pause]
[display]
[video]
[gif]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("deserialize empty tables");

        assert!(!config.general.auto_start);
        assert!(config.general.minimize_to_tray);
        assert!((config.audio.volume - 0.8).abs() < f32::EPSILON);
        assert!(!config.audio.muted);
        assert_eq!(config.pause.fullscreen_action, FullscreenAction::Terminate);
        assert!(!config.pause.pause_on_battery);
        assert!(config.video.hwdec);
        assert!((config.video.speed - 1.0).abs() < f32::EPSILON);
        assert_eq!(config.gif.memory_strategy, GifMemoryStrategy::Balanced);
        assert_eq!(
            config.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES
        );
    }

    #[test]
    fn empty_toml_produces_default_config() {
        let config: AppConfig = toml::from_str("").expect("deserialize empty string");
        let default = AppConfig::default();

        assert_eq!(config.general.auto_start, default.general.auto_start);
        assert_eq!(
            config.general.minimize_to_tray,
            default.general.minimize_to_tray
        );
        assert!((config.audio.volume - default.audio.volume).abs() < f32::EPSILON);
        assert_eq!(config.audio.muted, default.audio.muted);
        assert_eq!(
            config.pause.fullscreen_action,
            default.pause.fullscreen_action
        );
        assert_eq!(
            config.pause.pause_on_battery,
            default.pause.pause_on_battery
        );
        assert_eq!(config.video.hwdec, default.video.hwdec);
        assert!((config.video.speed - default.video.speed).abs() < f32::EPSILON);
        assert_eq!(config.gif.memory_strategy, default.gif.memory_strategy);
        assert_eq!(
            config.gif.balanced_keep_frames,
            default.gif.balanced_keep_frames
        );
    }

    // ── 旧配置向前兼容（unify-arrangement-single-source）──────────────────

    #[test]
    fn legacy_display_table_is_ignored_and_top_arrangement_uses_default() {
        // 旧版 config.toml 残留 `[display]` 表：AppConfig 未设置 deny_unknown_fields，
        // serde 应忽略未知表不报错；顶层 `arrangement` 仍取默认 PerMonitor。
        let toml_str = r#"
[display]
arrangement = "span"

[rotation]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("legacy [display] table ignored");
        assert_eq!(
            config.arrangement,
            Arrangement::PerMonitor,
            "top-level arrangement should stay default per_monitor despite legacy [display]"
        );
    }

    // ── C02 修复：范围校验 ──────────────────────────────────────────────────

    #[test]
    fn validate_clamps_audio_volume_above_1() {
        // 越界值 1.5 应被 clamp 到 1.0
        let mut config = AppConfig {
            audio: AudioConfig {
                volume: 1.5,
                muted: false,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!(
            (config.audio.volume - 1.0).abs() < f32::EPSILON,
            "volume 1.5 应被 clamp 到 1.0，实际 {}",
            config.audio.volume
        );
    }

    #[test]
    fn validate_clamps_audio_volume_below_0() {
        let mut config = AppConfig {
            audio: AudioConfig {
                volume: -0.5,
                muted: false,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!(
            (config.audio.volume - 0.0).abs() < f32::EPSILON,
            "volume -0.5 应被 clamp 到 0.0，实际 {}",
            config.audio.volume
        );
    }

    #[test]
    fn validate_clamps_audio_volume_nan() {
        let mut config = AppConfig {
            audio: AudioConfig {
                volume: f32::NAN,
                muted: false,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!(
            (config.audio.volume - default_volume()).abs() < f32::EPSILON,
            "NaN volume 应回退到默认 0.8，实际 {}",
            config.audio.volume
        );
    }

    #[test]
    fn validate_keeps_audio_volume_in_range() {
        // 合法范围内的值不应被修改
        for &v in &[0.0_f32, 0.5, 1.0] {
            let mut config = AppConfig {
                audio: AudioConfig {
                    volume: v,
                    muted: false,
                    ..AudioConfig::default()
                },
                ..AppConfig::default()
            };
            config.validate();
            assert!(
                (config.audio.volume - v).abs() < f32::EPSILON,
                "合法 volume {} 不应被修改，实际 {}",
                v,
                config.audio.volume
            );
        }
    }

    #[test]
    fn audio_preview_defaults_to_half_volume_unmuted() {
        let audio = AudioConfig::default();
        assert!(
            (audio.preview_volume - 0.5).abs() < f32::EPSILON,
            "preview_volume 默认 0.5，实际 {}",
            audio.preview_volume
        );
        assert!(!audio.preview_muted, "preview_muted 默认 false");
    }

    #[test]
    fn validate_clamps_audio_preview_volume_out_of_range_and_nan() {
        // < 0 → 0.0
        let mut config = AppConfig {
            audio: AudioConfig {
                preview_volume: -0.5,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!((config.audio.preview_volume - 0.0).abs() < f32::EPSILON);
        // > 1 → 1.0
        let mut config = AppConfig {
            audio: AudioConfig {
                preview_volume: 1.5,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!((config.audio.preview_volume - 1.0).abs() < f32::EPSILON);
        // NaN → 默认 0.5
        let mut config = AppConfig {
            audio: AudioConfig {
                preview_volume: f32::NAN,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!(
            (config.audio.preview_volume - default_preview_volume()).abs() < f32::EPSILON,
            "NaN preview_volume 应回退到默认 0.5，实际 {}",
            config.audio.preview_volume
        );
        // 合法值不变
        let mut config = AppConfig {
            audio: AudioConfig {
                preview_volume: 0.3,
                ..AudioConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!((config.audio.preview_volume - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn audio_preview_fields_fallback_when_absent_in_json() {
        // 旧 JSON 缺 preview_* 字段时应取默认（0.5 / false）。
        let config: AppConfig = serde_json::from_str("{}").expect("deserialize legacy json");
        assert!(
            (config.audio.preview_volume - 0.5).abs() < f32::EPSILON,
            "缺字段时 preview_volume 应取默认 0.5，实际 {}",
            config.audio.preview_volume
        );
        assert!(!config.audio.preview_muted);
    }

    #[test]
    fn validate_resets_video_speed_zero_or_negative() {
        // 0 / 负数 / NaN 都应回退到默认 1.0
        for &s in &[0.0_f32, -1.0, f32::NAN] {
            let mut config = AppConfig {
                video: VideoConfig {
                    hwdec: true,
                    speed: s,
                },
                ..AppConfig::default()
            };
            config.validate();
            assert!(
                (config.video.speed - default_speed()).abs() < f32::EPSILON,
                "speed {} 应回退到默认 1.0，实际 {}",
                s,
                config.video.speed
            );
        }
    }

    #[test]
    fn validate_keeps_video_speed_positive() {
        let mut config = AppConfig {
            video: VideoConfig {
                hwdec: true,
                speed: 1.5,
            },
            ..AppConfig::default()
        };
        config.validate();
        assert!(
            (config.video.speed - 1.5).abs() < f32::EPSILON,
            "合法 speed 1.5 不应被修改"
        );
    }

    #[test]
    fn validate_resets_gif_balanced_keep_frames_zero() {
        // 0 应回退到默认值
        let mut config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 0,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES,
            "balanced_keep_frames 0 应回退到默认值"
        );
    }

    #[test]
    fn validate_keeps_gif_balanced_keep_frames_positive() {
        // ≥ 1 的合法值不应被修改
        let mut config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 5,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.balanced_keep_frames, 5,
            "合法 balanced_keep_frames 5 不应被修改"
        );
    }

    #[test]
    fn validate_clamps_out_of_range_toml_end_to_end() {
        // C02 端到端测试：模拟用户手动编辑 config.toml 写入越界值，
        // 反序列化后调用 validate()，验证被 clamp
        let toml_str = r#"
[audio]
volume = 1.5

[video]
speed = -2.0

[gif]
balanced_keep_frames = 0
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("deserialize out-of-range");
        config.validate();
        assert!(
            (config.audio.volume - 1.0).abs() < f32::EPSILON,
            "volume 1.5 应被 clamp 到 1.0"
        );
        assert!(
            (config.video.speed - 1.0).abs() < f32::EPSILON,
            "speed -2.0 应回退到 1.0"
        );
        assert_eq!(
            config.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES,
            "balanced_keep_frames 0 应回退到默认值"
        );
    }

    // ── C-005 修复：balanced_keep_frames 上限校验 ───────────────────────────

    #[test]
    fn c005_gif_config_validate_rejects_oversized_keep_frames() {
        // C-005：balanced_keep_frames = 999999999 超过 MAX_BALANCED_KEEP_FRAMES (1000)，
        // 应回退到 default_gif_keep_frames()
        let mut config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 999_999_999,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.balanced_keep_frames,
            default_gif_keep_frames(),
            "balanced_keep_frames 999999999 超过上限 {}，应回退到默认值 {}",
            MAX_BALANCED_KEEP_FRAMES,
            default_gif_keep_frames()
        );
    }

    #[test]
    fn c005_gif_config_validate_accepts_normal_keep_frames() {
        // C-005：balanced_keep_frames = 10 在合法范围 [1, 1000] 内，值应保持不变
        let mut config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 10,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.balanced_keep_frames, 10,
            "合法 balanced_keep_frames 10 不应被修改"
        );
    }

    // ── max_memory_mb 范围校验 ─────────────────────────────────────────────

    #[test]
    fn w012_gif_config_validate_rejects_below_min_memory_mb() {
        // max_memory_mb = 5 < MIN_GIF_MEMORY_MB (10)，应回退到默认值
        let mut config = AppConfig {
            gif: GifConfig {
                max_memory_mb: 5,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.max_memory_mb,
            default_gif_max_memory_mb(),
            "max_memory_mb 5 < 下限 {}，应回退到默认值 {}",
            MIN_GIF_MEMORY_MB,
            default_gif_max_memory_mb()
        );
    }

    #[test]
    fn w012_gif_config_validate_rejects_above_max_memory_mb() {
        // max_memory_mb = 99999 > MAX_GIF_MEMORY_MB_LIMIT (500)，应回退到默认值
        let mut config = AppConfig {
            gif: GifConfig {
                max_memory_mb: 99_999,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.max_memory_mb,
            default_gif_max_memory_mb(),
            "max_memory_mb 99999 > 上限 {}，应回退到默认值 {}",
            MAX_GIF_MEMORY_MB_LIMIT,
            default_gif_max_memory_mb()
        );
    }

    #[test]
    fn w012_gif_config_validate_accepts_normal_memory_mb() {
        // max_memory_mb = 100 在合法范围 [10, 500] 内，值应保持不变
        let mut config = AppConfig {
            gif: GifConfig {
                max_memory_mb: 100,
                ..GifConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate();
        assert_eq!(
            config.gif.max_memory_mb, 100,
            "合法 max_memory_mb 100 不应被修改"
        );
    }

    // ── RotationConfig 轮换配置（DR-37/DR-16）───────────────────────────────

    #[test]
    fn rotation_config_default_values() {
        let config = RotationConfig::default();
        assert!(!config.enabled, "enabled 默认 false（DR-37，避免开箱即动）");
        assert!(!config.on_boot, "on_boot 默认 false");
        assert_eq!(config.interval_minutes, 30, "interval 默认 30 分钟");
        assert_eq!(
            config.order,
            Order::Sequential,
            "order 默认 sequential（顺序循环）"
        );
        assert_eq!(
            Order::default(),
            Order::Sequential,
            "Order::default 应为 sequential"
        );
        assert_eq!(
            config.arrangement_legacy, None,
            "arrangement 已提升为顶层，RotationConfig 不再持有（legacy 仅旧配置迁移用）"
        );
    }

    #[test]
    fn rotation_config_validate_clamps_zero_interval() {
        // DR-16：interval_minutes = 0 应 clamp 到 1 分钟
        let mut config = RotationConfig {
            interval_minutes: 0,
            ..RotationConfig::default()
        };
        config.validate();
        assert_eq!(
            config.interval_minutes, 1,
            "interval_minutes 0 应 clamp 到 1 分钟"
        );
    }

    #[test]
    fn rotation_config_validate_keeps_normal_interval() {
        let mut config = RotationConfig {
            interval_minutes: 45,
            ..RotationConfig::default()
        };
        config.validate();
        assert_eq!(config.interval_minutes, 45, "合法 interval 45 不应被修改");
    }

    #[test]
    fn app_config_toml_roundtrip_rotation() {
        let config = AppConfig {
            arrangement: Arrangement::AllSame,
            rotation: RotationConfig {
                enabled: true,
                on_boot: true,
                interval_minutes: 60,
                order: Order::Sequential,
                order_default_migrated: true,
                arrangement_legacy: None,
            },
            ..AppConfig::default()
        };
        let toml_str = toml::to_string_pretty(&config).expect("serialize config");
        let deserialized: AppConfig = toml::from_str(&toml_str).expect("deserialize config");
        assert_eq!(
            deserialized.rotation.enabled, config.rotation.enabled,
            "rotation.enabled round-trip"
        );
        assert_eq!(
            deserialized.rotation.interval_minutes, config.rotation.interval_minutes,
            "rotation.interval_minutes round-trip"
        );
        assert_eq!(
            deserialized.rotation.order, config.rotation.order,
            "rotation.order round-trip"
        );
        assert_eq!(
            deserialized.arrangement, config.arrangement,
            "top-level arrangement round-trip"
        );
        assert_eq!(
            deserialized.rotation.arrangement_legacy, None,
            "旧键 [rotation] arrangement 不应被序列化（legacy skip_serializing）"
        );
    }

    #[test]
    fn missing_rotation_table_uses_defaults() {
        let config: AppConfig = toml::from_str("[display]\n[video]\n").expect("no rotation table");
        let default = RotationConfig::default();
        assert_eq!(config.rotation.enabled, default.enabled);
        assert_eq!(config.rotation.interval_minutes, default.interval_minutes);
        assert_eq!(config.rotation.order, default.order);
        assert_eq!(
            config.arrangement,
            Arrangement::default(),
            "顶层 arrangement 缺省回退 per_monitor"
        );
        assert_eq!(config.rotation.arrangement_legacy, None);
    }

    // ── 旧版 [rotation] arrangement 迁移（BREAKING 兼容）────────────────────

    #[test]
    fn legacy_rotation_arrangement_migrates_to_top_level() {
        // 旧版配置：arrangement 位于 [rotation] 表内 → 反序列化进入 arrangement_legacy，
        // 迁移后提升为顶层并清空 legacy。
        let toml_str = r#"
[rotation]
enabled = true
arrangement = "span"
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("legacy config parses");
        assert_eq!(
            config.rotation.arrangement_legacy,
            Some(Arrangement::Span),
            "旧键应进入 legacy 兼容字段"
        );
        // 配置文本中顶层无 arrangement 键 → explicit = false，采用 legacy
        assert!(config.migrate_legacy_arrangement(false), "应发生迁移");
        assert_eq!(config.arrangement, Arrangement::Span, "迁移后顶层为 span");
        assert_eq!(config.rotation.arrangement_legacy, None, "legacy 已清空");
        // 序列化后旧键消失（skip_serializing + 迁移清空）：重新解析确认无 legacy 残留
        let out = toml::to_string_pretty(&config).expect("serialize");
        let reparsed: AppConfig = toml::from_str(&out).expect("reparse");
        assert_eq!(
            reparsed.arrangement,
            Arrangement::Span,
            "顶层 arrangement 保留"
        );
        assert_eq!(
            reparsed.rotation.arrangement_legacy, None,
            "保存后旧键 [rotation] arrangement 应被清理"
        );
    }

    #[test]
    fn top_level_arrangement_wins_over_legacy() {
        // 新旧键并存（手动编辑）：顶层显式值优先，legacy 被清空。
        let toml_str = r#"
arrangement = "all_same"

[rotation]
arrangement = "span"
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("mixed config parses");
        // 顶层 arrangement 键显式出现 → explicit = true
        assert!(
            config.migrate_legacy_arrangement(true),
            "legacy 存在即触发迁移流程"
        );
        assert_eq!(config.arrangement, Arrangement::AllSame, "顶层显式值优先");
        assert_eq!(config.rotation.arrangement_legacy, None, "legacy 已清空");
    }

    #[test]
    fn explicit_default_arrangement_not_overridden_by_legacy() {
        // 顶层显式写入默认值 per_monitor + 旧键 span：显式值优先（即使等于默认值），
        // 不被 legacy 覆盖，且旧键被清除。
        let toml_str = r#"
arrangement = "per_monitor"

[rotation]
arrangement = "span"
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("mixed config parses");
        assert_eq!(
            config.arrangement,
            Arrangement::PerMonitor,
            "顶层显式值解析"
        );
        assert!(
            config.migrate_legacy_arrangement(true),
            "legacy 存在即触发迁移流程"
        );
        assert_eq!(
            config.arrangement,
            Arrangement::PerMonitor,
            "顶层显式 per_monitor（= 默认值）不应被 legacy span 覆盖"
        );
        assert_eq!(config.rotation.arrangement_legacy, None, "legacy 已清空");
    }

    // ── 默认切换顺序一次性迁移（shuffle_bag → sequential）────────────────────

    #[test]
    fn migrate_order_default_rewrites_legacy_shuffle_bag() {
        // 旧配置：order = shuffle_bag 且未迁移 → 改写为 sequential 并置位标记
        let mut config = AppConfig {
            rotation: RotationConfig {
                order: Order::ShuffleBag,
                order_default_migrated: false,
                ..RotationConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(config.migrate_order_default(), "旧 shuffle_bag 应发生改写");
        assert_eq!(
            config.rotation.order,
            Order::Sequential,
            "已迁移为 sequential"
        );
        assert!(config.rotation.order_default_migrated, "迁移后标记应置位");
    }

    #[test]
    fn migrate_order_default_skips_when_already_migrated() {
        // 已置位：尊重用户后续手动选择，不再改写
        let mut config = AppConfig {
            rotation: RotationConfig {
                order: Order::ShuffleBag,
                order_default_migrated: true,
                ..RotationConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(!config.migrate_order_default(), "已迁移不应再次改写");
        assert_eq!(
            config.rotation.order,
            Order::ShuffleBag,
            "已置位时保留用户手动选择"
        );
        assert!(config.rotation.order_default_migrated, "标记保持置位");
    }

    #[test]
    fn migrate_order_default_keeps_sequential_and_marks() {
        // 非 shuffle_bag 取值：不改写，但标记仍置位（后续不再处理）
        let mut config = AppConfig {
            rotation: RotationConfig {
                order: Order::Sequential,
                order_default_migrated: false,
                ..RotationConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(!config.migrate_order_default(), "sequential 无需改写");
        assert_eq!(config.rotation.order, Order::Sequential, "取值保持不变");
        assert!(config.rotation.order_default_migrated, "标记应置位");
    }

    #[test]
    fn rotation_config_missing_order_default_migrated_key_defaults_false() {
        // 旧配置缺 order_default_migrated 键 → 反序列化得到 false
        let toml_str = r#"
[rotation]
enabled = true
order = "shuffle_bag"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("legacy config parses");
        assert!(
            !config.rotation.order_default_migrated,
            "缺省键应反序列化为 false"
        );
        assert_eq!(
            config.rotation.order,
            Order::ShuffleBag,
            "order 保留磁盘取值"
        );
    }

    // ── [scaling] 缩放模式配置段（Task 1）────────────────────────────────────

    #[test]
    fn scaling_config_toml_roundtrip() {
        let mut modes = BTreeMap::new();
        modes.insert("all".to_string(), ScalingMode::Fill);
        modes.insert("DISPLAY1".to_string(), ScalingMode::Tile);
        modes.insert("DISPLAY2".to_string(), ScalingMode::Fit);
        let config = AppConfig {
            scaling: ScalingConfig {
                modes: modes.clone(),
            },
            ..AppConfig::default()
        };
        let toml_str = toml::to_string_pretty(&config).expect("serialize config");
        let deserialized: AppConfig = toml::from_str(&toml_str).expect("deserialize config");
        assert_eq!(
            deserialized.scaling.modes, modes,
            "[scaling] modes 应 TOML round-trip"
        );
    }

    #[test]
    fn scaling_config_absent_yields_empty_modes() {
        // 旧配置无 [scaling] 段：加载后 modes 为空映射
        let config: AppConfig = toml::from_str("[general]\n").expect("legacy config parses");
        assert!(
            config.scaling.modes.is_empty(),
            "无 [scaling] 段时 modes 应为空映射"
        );
    }

    #[test]
    fn unknown_enum_values_fall_back_keep_other_fields() {
        // 未知枚举值应容错回退该字段默认，不影响同文件其它字段
        let toml_str = r#"
arrangement = "diagonal"

[pause]
fullscreen_action = "explode"

[audio]
volume = 0.42

[rotation]
interval_minutes = 45

[scaling]
modes = { DISPLAY1 = "tile", DISPLAY2 = "spiral" }
"#;
        let config: AppConfig =
            toml::from_str(toml_str).expect("未知枚举值不得导致整份配置反序列化失败");
        assert_eq!(
            config.arrangement,
            Arrangement::default(),
            "未知 arrangement 回退默认 per_monitor"
        );
        assert_eq!(
            config.pause.fullscreen_action,
            FullscreenAction::default(),
            "未知 fullscreen_action 回退默认 terminate"
        );
        assert!(
            (config.audio.volume - 0.42).abs() < f32::EPSILON,
            "audio.volume 应保留配置值"
        );
        assert_eq!(
            config.rotation.interval_minutes, 45,
            "rotation.interval_minutes 应保留配置值"
        );
        assert_eq!(
            config.scaling.modes.get("DISPLAY1"),
            Some(&ScalingMode::Tile),
            "合法缩放模式应保留"
        );
        assert_eq!(
            config.scaling.modes.get("DISPLAY2"),
            Some(&ScalingMode::Fill),
            "未知缩放模式回退默认 fill"
        );
    }
}
