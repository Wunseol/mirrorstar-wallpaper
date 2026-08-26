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

use serde::{Deserialize, Serialize};

use crate::wallpaper::GifMemoryStrategy;

/// 应用配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub pause: PauseConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub gif: GifConfig,
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
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            volume: 0.8,
            muted: false,
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
    }
}

fn default_volume() -> f32 {
    0.8
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
    #[serde(default)]
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

/// 壁纸排列方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Arrangement {
    /// 每个显示器独立壁纸
    #[default]
    #[serde(rename = "per_monitor")]
    PerMonitor,
    /// 跨显示器单张壁纸
    #[serde(rename = "span")]
    Span,
}

/// 显示配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// 壁纸排列方式
    #[serde(default)]
    pub arrangement: Arrangement,
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
const MAX_BALANCED_KEEP_FRAMES: usize = 1000;

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

        // display
        assert_eq!(
            config.display.arrangement,
            Arrangement::PerMonitor,
            "arrangement should default to per_monitor"
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
            general: GeneralConfig {
                auto_start: true,
                minimize_to_tray: false,
            },
            audio: AudioConfig {
                volume: 0.5,
                muted: true,
            },
            pause: PauseConfig {
                fullscreen_action: FullscreenAction::None,
                pause_on_battery: true,
            },
            display: DisplayConfig {
                arrangement: Arrangement::Span,
            },
            video: VideoConfig {
                hwdec: false,
                speed: 1.5,
            },
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Performance,
                balanced_keep_frames: 10,
                ..GifConfig::default()
            },
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
        assert_eq!(deserialized.display.arrangement, config.display.arrangement);
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
        assert_eq!(
            config.pause.fullscreen_action,
            FullscreenAction::Terminate
        );
        assert!(!config.pause.pause_on_battery);
        assert_eq!(config.display.arrangement, Arrangement::PerMonitor);
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
        assert_eq!(config.display.arrangement, default.display.arrangement);
        assert_eq!(config.video.hwdec, default.video.hwdec);
        assert!((config.video.speed - default.video.speed).abs() < f32::EPSILON);
        assert_eq!(config.gif.memory_strategy, default.gif.memory_strategy);
        assert_eq!(
            config.gif.balanced_keep_frames,
            default.gif.balanced_keep_frames
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
}
