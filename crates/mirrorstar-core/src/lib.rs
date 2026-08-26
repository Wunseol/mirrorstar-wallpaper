pub mod audio;
pub mod config;
pub mod desktop;
pub mod ipc;
pub mod perf;
pub mod process;
pub mod wallpaper;

pub use audio::volume::VolumeControl;
pub use config::settings::AppConfig;
pub use config::settings::Arrangement;
pub use config::settings::FullscreenAction;
pub use config::ConfigKind;
pub use config::ConfigLoadError;
pub use config::ConfigManager;
pub use desktop::DesktopIntegrator;
pub use wallpaper::manager::WallpaperEngine;
pub use wallpaper::manager::WallpaperMode;
pub use wallpaper::{
    GifMemoryStrategy, PauseCommand, PauseReason, PauseSender, ScalingMode, WallpaperRenderer,
    WallpaperSource, WallpaperState, WallpaperType,
};

/// MirrorStar 全局错误类型
#[derive(Debug, thiserror::Error)]
pub enum MirrorStarError {
    /// 桌面集成错误
    #[error("桌面集成失败: {0}")]
    DesktopIntegration(String),

    /// 未找到 WorkerW 窗口
    #[error("未找到 WorkerW 窗口")]
    WorkerWNotFound,

    /// 子进程启动失败
    #[error("子进程启动失败: {0}")]
    ProcessSpawnFailed(String),

    /// IPC 通信失败
    #[error("IPC 通信失败: {0}")]
    IpcError(String),

    /// 音频控制错误
    #[error("音频控制失败: {0}")]
    AudioControl(String),

    /// 配置文件解析错误
    #[error("配置文件解析失败: {0}")]
    ConfigParse(String),

    /// 配置文件写入错误
    #[error("配置文件写入失败: {0}")]
    ConfigWrite(String),

    /// 图片解码失败
    #[error("图片解码失败: {0}")]
    ImageDecode(String),

    /// 文件监视器失败
    #[error("文件监视器失败: {0}")]
    FileWatcher(String),

    /// 锁中毒
    #[error("锁中毒: {0}")]
    LockPoisoned(String),

    /// 任务 join 失败
    #[error("任务 join 失败: {0}")]
    TaskJoin(String),

    /// IPC 超时
    #[error("IPC 超时: {0}")]
    IpcTimeout(String),

    /// IPC 连接断开
    #[error("IPC 连接断开: {0}")]
    IpcDisconnected(String),

    /// IPC 未连接
    #[error("IPC 未连接: {0}")]
    IpcNotConnected(String),

    /// Windows API 错误
    #[error("Win32 错误: {0}")]
    Win32(#[from] windows::core::Error),

    /// IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    // SEC-001: 路径校验失败（含路径遍历拒绝）
    #[error("无效的壁纸文件路径: {reason}")]
    InvalidPath { reason: String },

    // SEC-002: 配置字段范围校验失败
    #[error("无效的配置字段: {reason}")]
    InvalidConfig { reason: String },

    // SEC-004: URL 协议不在白名单
    #[error("无效的 URL 协议: {scheme}")]
    InvalidUrl { scheme: String },

    /// 无效参数（N-009: 命令行参数含非法字符如换行符）
    #[error("无效参数: {reason}")]
    InvalidArgument { reason: String },

    /// WebView2 异步操作超时（W-006: 替代无限阻塞的 wait_for_async_operation）
    #[error("WebView2 操作超时: {0}")]
    WebView2Timeout(String),
}

impl serde::Serialize for MirrorStarError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let code = match self {
            MirrorStarError::DesktopIntegration(_) => "DesktopIntegration",
            MirrorStarError::WorkerWNotFound => "WorkerWNotFound",
            MirrorStarError::ProcessSpawnFailed(_) => "ProcessSpawnFailed",
            MirrorStarError::IpcError(_) => "IpcError",
            MirrorStarError::AudioControl(_) => "AudioControl",
            MirrorStarError::ConfigParse(_) => "ConfigParse",
            MirrorStarError::ConfigWrite(_) => "ConfigWrite",
            MirrorStarError::ImageDecode(_) => "ImageDecode",
            MirrorStarError::FileWatcher(_) => "FileWatcher",
            MirrorStarError::LockPoisoned(_) => "LockPoisoned",
            MirrorStarError::TaskJoin(_) => "TaskJoin",
            MirrorStarError::IpcTimeout(_) => "IpcTimeout",
            MirrorStarError::IpcDisconnected(_) => "IpcDisconnected",
            MirrorStarError::IpcNotConnected(_) => "IpcNotConnected",
            MirrorStarError::Win32(_) => "Win32",
            MirrorStarError::Io(_) => "Io",
            MirrorStarError::InvalidPath { .. } => "InvalidPath",
            MirrorStarError::InvalidConfig { .. } => "InvalidConfig",
            MirrorStarError::InvalidUrl { .. } => "InvalidUrl",
            MirrorStarError::InvalidArgument { .. } => "InvalidArgument",
            MirrorStarError::WebView2Timeout(_) => "WebView2Timeout",
        };
        let mut state = serializer.serialize_struct("MirrorStarError", 2)?;
        state.serialize_field("code", code)?;
        state.serialize_field("message", &self.to_string())?;
        state.end()
    }
}

impl From<serde_json::Error> for MirrorStarError {
    fn from(e: serde_json::Error) -> Self {
        MirrorStarError::ConfigParse(e.to_string())
    }
}

impl From<toml::de::Error> for MirrorStarError {
    fn from(e: toml::de::Error) -> Self {
        MirrorStarError::ConfigParse(e.to_string())
    }
}

impl From<image::ImageError> for MirrorStarError {
    fn from(e: image::ImageError) -> Self {
        MirrorStarError::ImageDecode(e.to_string())
    }
}

impl From<notify::Error> for MirrorStarError {
    fn from(e: notify::Error) -> Self {
        MirrorStarError::FileWatcher(e.to_string())
    }
}

impl From<toml::ser::Error> for MirrorStarError {
    fn from(e: toml::ser::Error) -> Self {
        MirrorStarError::ConfigWrite(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MirrorStarError>;

/// 初始化日志系统
///
/// 返回 `WorkerGuard`，调用方必须将其持有至程序退出。
/// guard 被 drop 后非阻塞写入器的后台线程会退出并刷写缓冲，
/// 因此提前 drop 会导致后续日志丢失。原先通过 `std::mem::forget` 泄漏 guard
/// 来规避此问题，现改为显式返回由调用方持有，避免资源泄漏。
pub fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    use tracing_subscriber::EnvFilter;

    let log_dir = crate::config::data_root().join("logs");

    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(log_dir, "mirrorstar.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stdout.and(non_blocking))
        .with_ansi(true)
        .init();

    tracing::info!("日志系统初始化完成");
    Ok(guard)
}
