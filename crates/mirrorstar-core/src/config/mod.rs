//! 配置与壁纸库管理模块。
//!
//! 本模块聚合应用配置（`AppConfig`）、壁纸库（`WallpaperLibrary`）、配置管理器
//!（[`ConfigManager`]）、文件类型检测、热重载与缩略图生成等子模块。
//!
//! ## 模块导出约定
//!
//! 外部调用方**推荐使用 `config::` 顶层的重导出路径**（`pub use` 引入的公开 API），
//! 而非直接引用 `config::<子模块>::<类型>`。这样可保持子模块内部重组时调用方不需
//! 修改 import，降低耦合。具体推荐路径（按现有 `pub use` 重导出）：
//!
//! - 配置管理器：`mirrorstar_core::config::ConfigManager`
//! - 壁纸库条目：`mirrorstar_core::config::{WallpaperEntry, WallpaperLibrary, WallpaperMetadata}`
//! - 配置加载错误：`mirrorstar_core::config::{ConfigKind, ConfigLoadError}`
//! - 显示器信息：`mirrorstar_core::config::DisplayInfo`
//! - 文件类型检测：`mirrorstar_core::config::detect_wallpaper_type`
//! - 缩略图生成：`mirrorstar_core::config::{generate_thumbnail, generate_video_thumbnail, is_ffmpeg_available}`
//! - 壁纸类型枚举：`mirrorstar_core::config::WallpaperType`（重导出自 [`crate::wallpaper`]）
//!
//! 注意：`AppConfig` 及其子配置（`AudioConfig` / `VideoConfig` / `GifConfig` 等）通过
//! `pub mod settings` 暴露，外部调用方使用 crate 根重导出 `mirrorstar_core::AppConfig`
//!（见 `lib.rs`）访问；模块内部协作（`manager` / `hot_reload`）则使用完整路径
//! `crate::config::settings::AppConfig`。本模块不再额外 `pub use settings::AppConfig;`，
//! 以避免与 crate 根重导出产生冗余访问路径。
//!
//! `pub mod` 暴露的子模块（[`detect`] / [`hot_reload`] / [`manager`] / [`settings`] /
//! [`thumbnail`]）用于跨模块内部协作（如 `hot_reload` 为 `ConfigManager` 的 `impl`
//! 扩展），外部调用方应优先使用顶层重导出；如确需访问子模块内部类型（如
//! `ConfigChangedCallback` 类型别名），可通过 `config::manager::ConfigChangedCallback`
//! 显式引用，但这属于内部 API，未来可能调整。

pub mod detect;
pub mod hot_reload;
pub mod manager;
pub mod settings;
pub mod thumbnail;
pub mod validation;

// Re-export WallpaperType from wallpaper module to avoid duplication
pub use crate::wallpaper::WallpaperType;

// Re-export public API to preserve backward compatibility
pub use detect::detect_wallpaper_type;
pub use manager::{
    ConfigKind, ConfigLoadError, ConfigManager, DisplayInfo, WallpaperEntry, WallpaperLibrary,
    WallpaperMetadata, data_root, init_data_root, resolve_data_root,
};
pub use thumbnail::{generate_thumbnail, generate_video_thumbnail, is_ffmpeg_available};
