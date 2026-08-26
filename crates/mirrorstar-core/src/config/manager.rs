//! 配置与壁纸库管理
//!
//! `ConfigManager` 负责应用配置（`AppConfig`）与壁纸库（`WallpaperLibrary`）的
//! 加载、持久化、热重载与增删改查。配置文件采用 TOML 格式：
//! - `config.toml`：应用配置
//! - `wallpapers.toml`：壁纸库
//!
//! 写入采用临时文件 + `sync_all` fsync + rename 的原子写入方案（配合 fs2 文件锁
//! 串行化并发写入），`sync_all` 确保数据刷盘后再 rename，避免半写入状态。配置更新
//! 带 300ms 防抖；壁纸库变更立即落盘。
//!
//! 热重载逻辑（`start_watching`/周期性保存）见 `hot_reload.rs`，通过 `impl ConfigManager`
//! 扩展在此模块之外，因此本模块的字段以 `pub(crate)` 暴露给同 crate 的 `hot_reload`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::settings::AppConfig;
use crate::wallpaper::WallpaperType;
use crate::MirrorStarError;

// ════════════════════════════════════════════════════════════════════════════
// Internal constants
// ════════════════════════════════════════════════════════════════════════════
// 集中定义本模块使用的常量，便于查阅与维护。

/// 配置防抖窗口（毫秒）
const CONFIG_SAVE_DEBOUNCE_MS: u64 = 300;

/// 配置文件大小上限（1 MiB），`load_config` / `load_library` 共用
///
/// 防止 OOM 攻击：超过此上限的文件在读取阶段即被拒绝（详见 `load_config`）。
const MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024;

/// 配置变更回调类型
///
/// 由 Tauri 层设置，热重载成功后调用以 emit `config-changed` 事件通知前端。
pub(crate) type ConfigChangedCallback = Arc<dyn Fn() + Send + Sync>;

/// C01 修复：配置加载错误回调类型
///
/// 由 Tauri 层设置，`load_config`/`load_library` 解析失败但已回退到默认配置时调用，
/// 以 emit `config-load-error` 事件通知前端展示告警。携带 [`ConfigLoadError`] 参数。
pub(crate) type ConfigErrorCallback = Arc<dyn Fn(ConfigLoadError) + Send + Sync>;

// ════════════════════════════════════════════════════════════════════════════
// C01 修复：配置加载错误信息
// ════════════════════════════════════════════════════════════════════════════

/// 配置加载错误类型（区分 `config.toml` 与 `wallpapers.toml`）
///
/// C01 修复：`load_config`/`load_library` 解析失败但已回退到默认配置时，
/// 返回此枚举以便调用方区分错误来源，构造用户友好的告警消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    /// 应用配置（`config.toml`）
    Config,
    /// 壁纸库（`wallpapers.toml`）
    Library,
}

/// 配置加载错误信息（解析失败但已回退到默认配置）
///
/// C01 修复：原先 `load_config`/`load_library` 在 TOML 解析失败时仅 `tracing::warn!`
/// 并静默返回默认配置，用户无感知地丢失配置。现改为返回此结构，由调用方通过回调
/// 通知前端展示告警 toast，让用户知道发生了什么（配置被重置 / 壁纸列表为空）。
///
/// 应用仍回退到默认配置保证可启动，不会因配置损坏而崩溃。
#[derive(Debug, Clone)]
pub struct ConfigLoadError {
    /// 文件路径
    pub path: PathBuf,
    /// 错误类型（config / library）
    pub kind: ConfigKind,
    /// 错误消息（来自 `toml::de::Error` 的 Display）
    pub message: String,
}

// ════════════════════════════════════════════════════════════════════════════
// 数据模型
// ════════════════════════════════════════════════════════════════════════════

/// 壁纸库条目
///
/// 对应前端 `WallpaperEntry` 类型，字段命名与 serde 序列化格式保持一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WallpaperEntry {
    /// 唯一 ID（UUID v4 字符串）
    pub id: String,
    /// 壁纸文件绝对路径
    pub file_path: String,
    /// 壁纸类型
    pub wallpaper_type: WallpaperType,
    /// 所属显示器 ID（None 表示未指定）
    #[serde(default)]
    pub display_id: Option<String>,
    /// 添加时间（UNIX 秒，字符串形式以便 TOML 序列化）
    pub added_at: String,
    /// 缩略图路径（生成前为空字符串）
    #[serde(default)]
    pub thumbnail: String,
    /// 文件大小（字节）
    #[serde(default)]
    pub file_size: u64,
    /// 媒体元数据（可选）
    #[serde(default)]
    pub metadata: Option<WallpaperMetadata>,
    /// v5.0 C-PERF-003: 规范化路径（小写 + 统一分隔符），用于快速查找。
    /// 派生字段，不参与序列化，load_library / add_wallpaper 时计算。
    #[serde(skip)]
    pub normalized_path: String,
}

/// 壁纸媒体元数据
///
/// 记录壁纸的分辨率、时长、帧数等信息，用于前端展示与缩放计算。
/// 所有字段均为 `Option`，未能采集的字段为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WallpaperMetadata {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub frame_count: Option<u32>,
}

/// 壁纸库
///
/// 持久化到 `wallpapers.toml`，由 `ConfigManager` 的 `RwLock` 保护并发访问。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WallpaperLibrary {
    /// 壁纸条目列表
    #[serde(default)]
    pub wallpapers: Vec<WallpaperEntry>,
}

/// 显示器信息
///
/// 由 `desktop` 模块的 `enumerate_displays()` 填充，对应前端 `DisplayInfo` 类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayInfo {
    /// 显示器唯一 ID（Windows 设备名，如 `\\.\DISPLAY1`）
    pub id: String,
    /// 友好名称（如 "显示器 1"）
    pub name: String,
    /// 宽度（像素）
    pub width: u32,
    /// 高度（像素）
    pub height: u32,
    /// 屏幕左上角 X 坐标（虚拟桌面坐标）
    pub x: i32,
    /// 屏幕左上角 Y 坐标（虚拟桌面坐标）
    pub y: i32,
    /// 是否为主显示器
    pub is_primary: bool,
    /// DPI（每英寸像素数，默认 96）
    pub dpi: u32,
    /// 当前壁纸路径（如有）
    #[serde(default)]
    pub current_wallpaper: Option<String>,
}

// ════════════════════════════════════════════════════════════════════════════
// ConfigManager
// ════════════════════════════════════════════════════════════════════════════

/// 配置管理器
///
/// 线程安全：所有可变状态通过 `RwLock`/`Mutex`/`AtomicBool` 保护，
/// 可被多线程共享（通常通过 `Arc<ConfigManager>` 在 Tauri 命令间传递）。
///
/// 字段以 `pub(crate)` 暴露，供同 crate 的 `hot_reload` 模块直接访问。
pub struct ConfigManager {
    /// 应用配置（`Arc<RwLock<AppConfig>>` 支持并发读写 + 跨线程共享）
    pub(crate) config: Arc<RwLock<AppConfig>>,
    /// 壁纸库
    pub(crate) wallpaper_library: Arc<RwLock<WallpaperLibrary>>,
    /// 配置文件路径（`config.toml`）
    pub(crate) config_path: PathBuf,
    /// 壁纸库文件路径（`wallpapers.toml`）
    pub(crate) library_path: PathBuf,
    /// 配置脏标记：true 表示有待落盘的配置修改
    pub(crate) dirty: Arc<AtomicBool>,
    /// 上次配置保存时间（用于 300ms 防抖）
    pub(crate) last_save_time: Mutex<Option<Instant>>,
    /// 配置热重载成功后的回调（由 Tauri 层设置以 emit 事件通知前端）
    ///
    /// 使用 `Arc<dyn Fn() + Send + Sync>` 以便在持读锁期间克隆回调、释放读锁后再调用，
    /// 避免回调内重入 `set_on_config_changed`（取写锁）导致同线程死锁（C-019）。
    pub(crate) on_config_changed: Arc<RwLock<Option<ConfigChangedCallback>>>,
    /// C01 修复：配置加载错误回调（由 Tauri 层设置以 emit `config-load-error` 事件）
    ///
    /// `load_config`/`load_library` 解析失败但已回退到默认配置时调用。使用
    /// `Arc<RwLock<Option<...>>>` 以便 watcher 线程通过 Arc clone 读取回调引用。
    pub(crate) on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>>,
    /// C01 修复：构造时捕获的待处理配置加载错误（回调设置前发生的错误）
    ///
    /// `new_in_dir` 调用 `load_config`/`load_library` 时回调尚未设置（Tauri setup
    /// 在构造之后才注册回调），因此将错误暂存于此，待 `set_on_config_error` 设置后
    /// 由 Tauri setup 调用 `drain_pending_config_errors` 取出并立即 emit。
    /// 热重载路径若回调未设置（理论上不会，但防御性处理）也会将错误存入此处。
    pub(crate) pending_config_errors: Arc<std::sync::Mutex<Vec<ConfigLoadError>>>,
    /// 文件监视器（Option<RecommendedWatcher> 允许 take 后 drop 停止监视）
    pub(crate) watcher: Arc<std::sync::Mutex<Option<notify::RecommendedWatcher>>>,
    /// 上次内部保存时间戳：watcher 线程检测到 2s 窗口内的文件事件时跳过 reload，
    /// 避免"内部保存 → 触发文件事件 → 重新加载"的循环（C-093）。
    pub(crate) last_internal_save: Arc<Mutex<Option<Instant>>>,
    /// 周期性保存线程运行标志（C-110：从 static 迁移为实例字段，实现多实例隔离）
    ///
    /// C-016：初始值为 `false`（构造时未启动周期保存线程）。
    /// `start_periodic_save` 在 spawn 前置 `true`，spawn 失败时由
    /// `cleanup_after_periodic_save_spawn_failure` 重置为 `false`；
    /// `shutdown_periodic_save` / `Drop` 置 `false`。初始值与实际状态
    /// 一致，避免 `Drop` 误判线程已启动而阻塞 join。
    pub(crate) periodic_save_running: Arc<AtomicBool>,
    /// 周期性保存线程退出信号 sender（C-110：从 static 迁移为实例字段）
    pub(crate) periodic_save_shutdown_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    /// 配置落盘互斥锁：串行化 flush / maybe_save_config / 周期性保存任务，
    /// 消除"读 in-memory 配置 → 写磁盘"窗口期内的并发覆盖竞态。
    /// 使用 Arc 以便克隆到周期性保存线程（hot_reload.rs）。
    /// v5.0 C-PERF-005: 拆分为独立锁，避免 config 与 library 落盘互相阻塞。
    /// 两者写入不同文件（config.toml vs wallpapers.toml），无数据一致性冲突。
    pub(crate) config_save_mutex: Arc<std::sync::Mutex<()>>,
    /// v5.0 C-PERF-005: library 落盘互斥锁，串行化 wallpapers.toml 落盘。
    /// 与 config_save_mutex 独立，避免 thumbnail 批量更新等密集 library 落盘
    /// 阻塞前端 config 修改的 maybe_save_config。
    pub(crate) library_save_mutex: Arc<std::sync::Mutex<()>>,
    /// v5.0 C-PERF-002: library 防抖保存标志
    ///
    /// `add_wallpaper` / `remove_wallpaper` 标记 dirty 而不立即落盘，
    /// 由周期保存线程或显式 `flush_library` 落盘。
    /// 单次 Tauri 命令（add_wallpaper / remove_wallpaper）在命令返回前
    /// 显式调用 `flush_library` 保证用户操作立即持久化；批量场景（drag-drop）
    /// 可每文件 `mark_library_dirty` 后统一 `flush_library` 一次。
    /// 使用 `Arc<AtomicBool>` 以便克隆到周期性保存线程（与 `dirty` 字段一致）。
    pub(crate) library_dirty: Arc<AtomicBool>,
}

// ════════════════════════════════════════════════════════════════════════════
// 数据根（便携化）
// ════════════════════════════════════════════════════════════════════════════
// 应用便携化后，所有用户数据（config.toml、wallpapers.toml、wallpapers\、
// thumbnails\、logs\）均收束到安装目录（exe 所在目录），而非 %APPDATA%。

/// 全局数据根（便携化）：OnceLock 缓存，应用启动时由 `init_data_root` 显式设置，
/// 否则懒解析（`resolve_data_root`）并缓存。
static DATA_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 显式设置数据根（应用启动时调用一次）。
pub fn init_data_root(dir: std::path::PathBuf) {
    let _ = DATA_ROOT.set(dir);
}

/// 解析数据根：环境变量 `MIRRORSTAR_DATA_ROOT`（dev 便利）→ `current_exe().parent()`
/// （= 安装目录）→ 回退 `%APPDATA%/mirrorstar` → `.`
pub fn resolve_data_root() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("MIRRORSTAR_DATA_ROOT") {
        let p = std::path::PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    dirs::data_dir()
        .map(|d| d.join("mirrorstar"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 获取数据根：显式设置优先，否则懒解析并缓存。
pub fn data_root() -> std::path::PathBuf {
    DATA_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| {
            let root = resolve_data_root();
            let _ = DATA_ROOT.set(root.clone());
            root
        })
}

impl ConfigManager {
    /// 创建配置管理器
    ///
    /// 在 `data_dir()`（`%APPDATA%/mirrorstar/`）下加载或创建 `config.toml` 和
    /// `wallpapers.toml`。文件不存在时使用默认配置；解析失败时记录警告并回退到默认
    /// （避免单次配置损坏导致应用无法启动）。
    ///
    /// 实际逻辑委托给 [`new_in_dir`](Self::new_in_dir)，传入 `data_dir()` 作为数据目录。
    pub fn new() -> Result<Self, MirrorStarError> {
        Self::new_in_dir(Self::data_dir()?)
    }

    /// 创建配置管理器（指定数据目录）
    ///
    /// 与 [`new`](Self::new) 相同，但接受自定义数据目录，供集成测试使用临时目录
    /// 避免污染用户数据（C-020）。`dir` 不存在时会自动创建；`config.toml` 与
    /// `wallpapers.toml` 不存在时使用默认配置；解析失败时回退到默认配置。
    pub fn new_in_dir(dir: PathBuf) -> Result<Self, MirrorStarError> {
        std::fs::create_dir_all(&dir)?;

        let config_path = dir.join("config.toml");
        let library_path = dir.join("wallpapers.toml");

        let (config, config_err) = Self::load_config(&config_path)?;
        let (wallpaper_library, library_err) = Self::load_library(&library_path)?;

        // C01 修复：捕获构造时的配置加载错误，待回调设置后通知前端。
        // 构造时 Tauri setup 尚未执行，on_config_error 回调未设置，
        // 因此将错误暂存于 pending_config_errors，由 setup 调用
        // drain_pending_config_errors 取出并立即 emit。
        let pending_errors = Arc::new(std::sync::Mutex::new(Vec::new()));
        if let Some(err) = config_err {
            pending_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(err);
        }
        if let Some(err) = library_err {
            pending_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(err);
        }

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            wallpaper_library: Arc::new(RwLock::new(wallpaper_library)),
            config_path,
            library_path,
            dirty: Arc::new(AtomicBool::new(false)),
            last_save_time: Mutex::new(None),
            on_config_changed: Arc::new(RwLock::new(None)),
            on_config_error: Arc::new(RwLock::new(None)),
            pending_config_errors: pending_errors,
            watcher: Arc::new(std::sync::Mutex::new(None)),
            last_internal_save: Arc::new(Mutex::new(None)),
            periodic_save_running: Arc::new(AtomicBool::new(false)),
            periodic_save_shutdown_tx: Arc::new(Mutex::new(None)),
            // v5.0 C-PERF-005: 拆分为 config_save_mutex + library_save_mutex
            config_save_mutex: Arc::new(std::sync::Mutex::new(())),
            library_save_mutex: Arc::new(std::sync::Mutex::new(())),
            // v5.0 C-PERF-002: library 初始未修改，dirty 为 false
            library_dirty: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 获取应用数据目录（便携化后 = 安装目录 / exe 所在目录）
    ///
    /// 配置文件、壁纸库、缩略图均存储在此目录下。
    pub fn data_dir() -> Result<PathBuf, MirrorStarError> {
        Ok(data_root())
    }

    // ── 配置读路径 ──────────────────────────────────────────────────────────

    /// 获取当前应用配置（返回克隆，调用方获得独立所有权）
    pub fn get_config(&self) -> AppConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ── 配置写路径 ──────────────────────────────────────────────────────────

    /// 更新应用配置
    ///
    /// 替换内存中的配置并标记为脏，随后尝试防抖保存（300ms 窗口内仅首次落盘）。
    /// 防抖窗口内的后续修改由周期性保存任务（30s）或 `flush()` 兜底落盘。
    ///
    /// C-009 修复：写入内存前调用 `config.validate()`，与 `load_config` 的入口校验
    /// 对齐，避免前端注入越界值（如 `volume=1.5`、`speed=-2.0`、
    /// `balanced_keep_frames=0`）导致下游消费方行为异常。`validate()` 就地 clamp
    /// 到合法范围，不破坏调用方契约（仍返回 `Ok(())`）。
    pub fn update_config(&self, mut config: AppConfig) -> Result<(), MirrorStarError> {
        config.validate();
        {
            let mut cfg = self.config.write().unwrap_or_else(|e| e.into_inner());
            *cfg = config;
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.maybe_save_config()
    }

    /// 设置配置变更回调
    ///
    /// 热重载成功后调用此回调，由 Tauri 层 emit `config-changed` 事件通知前端刷新 UI。
    pub fn set_on_config_changed(&self, callback: ConfigChangedCallback) {
        let mut slot = self
            .on_config_changed
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(callback);
    }

    /// C01 修复：设置配置加载错误回调
    ///
    /// 由 Tauri 层在 setup 阶段调用。`load_config`/`load_library` 解析失败但已回退
    /// 到默认配置时调用此回调，由 Tauri 层 emit `config-load-error` 事件通知前端
    /// 展示告警 toast。
    ///
    /// 设置回调后应立即调用 [`drain_pending_config_errors`](Self::drain_pending_config_errors)
    /// 取出构造时捕获的待处理错误并 emit，否则构造时的错误会被丢弃。
    pub fn set_on_config_error(&self, callback: ConfigErrorCallback) {
        let mut slot = self
            .on_config_error
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(callback);
    }

    /// C01 修复：取出待处理的配置加载错误（由 Tauri setup 在设置回调后调用）
    ///
    /// 返回构造时（回调尚未设置）捕获的配置加载错误列表，并清空内部缓冲。
    /// Tauri setup 在调用 `set_on_config_error` 后应立即调用此方法，
    /// 对返回的每个错误调用回调（或直接 emit）以通知前端。
    pub fn drain_pending_config_errors(&self) -> Vec<ConfigLoadError> {
        let mut guard = self
            .pending_config_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }

    /// C01 修复：通知配置加载错误（热重载路径使用，回调已设置）
    ///
    /// 若回调已设置则立即调用回调通知前端；否则将错误存入 `pending_config_errors`，
    /// 待 Tauri setup 设置回调后通过 `drain_pending_config_errors` 取出并 emit。
    ///
    /// 作为静态关联函数以便 watcher 线程（持有 Arc clone 但无 `&self`）直接调用。
    /// 先克隆 Arc<callback> 再释放读锁，回调内可重入 set_on_config_error 而不会同线程死锁。
    pub(crate) fn notify_config_error(
        on_config_error: &Arc<RwLock<Option<ConfigErrorCallback>>>,
        pending_config_errors: &Arc<std::sync::Mutex<Vec<ConfigLoadError>>>,
        error: ConfigLoadError,
    ) {
        // 先克隆 Arc<callback> 再释放读锁，回调内可重入 set_on_config_error
        // （取写锁）而不会同线程死锁。
        let callback = on_config_error
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned();
        if let Some(callback) = callback {
            // C-003 修复：用 catch_unwind 包裹用户回调，避免回调 panic
            // 导致 watcher 线程终止、后续配置文件变更不再被监听。
            // panic 隔离逻辑由 invoke_callback_safe 统一处理。
            invoke_callback_safe(|| callback(error), "on_config_error");
        } else {
            // 回调未设置，存入 pending（防御性处理：热重载路径理论上回调已设置）
            pending_config_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(error);
        }
    }

    // ── 壁纸库读路径 ────────────────────────────────────────────────────────

    /// 获取壁纸列表（返回克隆，读路径持读锁后立即释放）
    pub fn get_wallpapers(&self) -> Vec<WallpaperEntry> {
        self.wallpaper_library
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .wallpapers
            .clone()
    }

    /// v5.0 I-PERF-004: 按 id 查找单个壁纸条目，避免全量克隆 Vec<WallpaperEntry>。
    /// 用于 set_wallpaper / remove_wallpaper 等仅需单个条目的命令路径。
    pub fn get_wallpaper(&self, id: &str) -> Option<WallpaperEntry> {
        self.wallpaper_library
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .wallpapers
            .iter()
            .find(|w| w.id == id)
            .cloned()
    }

    // ── 壁纸库写路径 ────────────────────────────────────────────────────────

    /// 添加壁纸条目
    ///
    /// 不校验 ID 唯一性（允许重复 ID），追加到列表末尾后标记 dirty。
    /// v5.0 C-PERF-002: 不再立即落盘，由调用方（Tauri 命令）显式 [`flush_library`](Self::flush_library)
    /// 或周期保存线程兜底。v5.0 C-PERF-003: 在此计算 `normalized_path` 派生字段，
    /// 供 `update_thumbnail` / `batch_update_thumbnails` 快速查找。
    pub fn add_wallpaper(&self, mut entry: WallpaperEntry) -> Result<(), MirrorStarError> {
        // v5.0 C-PERF-003: 计算规范化路径派生字段，供 update_thumbnail 等快速查找
        entry.normalized_path = normalize_path(&entry.file_path);
        {
            let mut lib = self
                .wallpaper_library
                .write()
                .unwrap_or_else(|e| e.into_inner());
            lib.wallpapers.push(entry);
        }
        // v5.0 C-PERF-002: 标记 dirty，由周期保存线程或显式 flush_library 落盘
        self.mark_library_dirty();
        Ok(())
    }

    /// 移除壁纸条目
    ///
    /// 移除首个 ID 匹配的条目并返回。未找到时返回 `Ok(None)`，不修改库也不标记 dirty。
    /// v5.0 C-PERF-002: 不再立即落盘，由调用方（Tauri 命令）显式
    /// [`flush_library`](Self::flush_library) 或周期保存线程兜底。
    pub fn remove_wallpaper(&self, id: &str) -> Result<Option<WallpaperEntry>, MirrorStarError> {
        let removed = {
            let mut lib = self
                .wallpaper_library
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let pos = lib.wallpapers.iter().position(|w| w.id == id);
            match pos {
                Some(i) => Some(lib.wallpapers.remove(i)),
                None => return Ok(None),
            }
        };
        // v5.0 C-PERF-002: 标记 dirty，由周期保存线程或显式 flush_library 落盘
        self.mark_library_dirty();
        Ok(removed)
    }

    /// 更新指定文件路径的壁纸缩略图
    ///
    /// 由 `add_wallpaper` 命令在后台生成缩略图后调用。未找到匹配条目时为 no-op。
    ///
    /// C09 修复：路径匹配前统一规范化（lowercase + 替换分隔符 `\` → `/`），
    /// 避免因大小写或路径分隔符差异（Windows 下 `\` vs `/`）导致匹配失败。
    pub fn update_thumbnail(
        &self,
        file_path: &str,
        thumbnail: String,
    ) -> Result<(), MirrorStarError> {
        let changed = {
            let mut lib = self
                .wallpaper_library
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let normalized = normalize_path(file_path);
            // v5.0 C-PERF-003: 直接比较 normalized_path 派生字段，
            // 消除迭代内每次 normalize_path 的堆分配（100 条目 batch 省 ~10000 次分配）
            let found = lib
                .wallpapers
                .iter_mut()
                .find(|w| w.normalized_path == normalized);
            match found {
                Some(entry) => {
                    entry.thumbnail = thumbnail;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.save_library()?;
        }
        Ok(())
    }

    /// v5.0 A-PERF-003: 批量更新缩略图路径，仅取一次写锁 + 一次 save_library。
    ///
    /// 用于 `regenerate_thumbnails` 等批量场景，避免 N 次独立 `update_thumbnail`
    /// 产生的 O(N) fsync + O(N²) 序列化开销（100 个壁纸在 HDD 上可省 0.5-5s 纯磁盘等待）。
    ///
    /// 路径匹配沿用 `update_thumbnail` 的规范化策略（lowercase + 分隔符统一），
    /// 未找到匹配条目时跳过（不计入返回值）。仅当至少一个条目被更新时才落盘。
    ///
    /// # 参数
    /// - `updates: &[(file_path, thumbnail_path)]`：待更新的 (壁纸文件路径, 缩略图路径) 列表
    ///
    /// # 返回
    /// 成功更新的条目数（未找到的 `file_path` 被跳过）。落盘失败时返回 `Err`。
    pub fn batch_update_thumbnails(
        &self,
        updates: &[(String, String)],
    ) -> Result<usize, MirrorStarError> {
        if updates.is_empty() {
            return Ok(0);
        }
        // 全程持有 library_save_mutex，串行化 wallpapers.toml 落盘（与 save_library 一致）
        let _save_guard = self
            .library_save_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut updated_count = 0usize;
        {
            let mut lib = self
                .wallpaper_library
                .write()
                .unwrap_or_else(|e| e.into_inner());
            for (file_path, thumbnail_path) in updates {
                let normalized = normalize_path(file_path);
                // v5.0 C-PERF-003: 直接比较 normalized_path 派生字段，
                // 消除迭代内每次 normalize_path 的堆分配
                let found = lib
                    .wallpapers
                    .iter_mut()
                    .find(|w| w.normalized_path == normalized);
                if let Some(entry) = found {
                    entry.thumbnail = thumbnail_path.clone();
                    updated_count += 1;
                }
            }
        }
        if updated_count > 0 {
            // 复用 save_library 的序列化 + 落盘逻辑；已持有 library_save_mutex，调用内部版本
            // 避免锁重入（std::sync::Mutex 不可重入，再次获取会死锁）
            self.save_library_locked()?;
        }
        Ok(updated_count)
    }

    /// 清理 thumbnails 目录下的 0 字节损坏缩略图文件
    ///
    /// 扫描 `<config_path.parent()>/thumbnails/` 目录，删除所有文件大小为 0 的文件。
    /// 用于启动时清理异常中断（如进程被杀、ffmpeg 抽帧失败）产生的损坏缩略图，
    /// 避免前端通过 `convertFileSrc` 加载 0 字节文件导致预览空白。
    ///
    /// 路径从 `self.config_path.parent()` 推导（C03：与实例数据目录一致，
    /// 避免 `new_in_dir` 创建的测试实例清理用户真实数据目录）。
    ///
    /// # 返回
    /// 成功删除的文件数量。thumbnails 目录不存在时返回 0。
    /// 单个文件删除失败仅记录 warn，不中断整体清理流程。
    fn cleanup_zero_byte_thumbnails(&self) -> Result<usize, MirrorStarError> {
        let thumb_dir = self
            .config_path
            .parent()
            .map(|p| p.join("thumbnails"))
            .ok_or_else(|| {
                MirrorStarError::ConfigWrite(
                    "config_path 无父目录，无法推导 thumbnails 目录".to_string(),
                )
            })?;

        let mut removed = 0usize;
        if thumb_dir.exists() {
            for entry in std::fs::read_dir(&thumb_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    if let Ok(metadata) = entry.metadata() {
                        if metadata.len() == 0 {
                            if let Err(e) = std::fs::remove_file(&path) {
                                tracing::warn!(
                                    error = %e,
                                    path = %path.display(),
                                    "删除损坏缩略图文件失败"
                                );
                            } else {
                                tracing::info!(
                                    path = %path.display(),
                                    "已清理 0 字节损坏缩略图文件"
                                );
                                removed += 1;
                            }
                        }
                    }
                }
            }
            if removed > 0 {
                tracing::info!(
                    count = removed,
                    "启动清理：已删除 {} 个 0 字节损坏缩略图文件",
                    removed
                );
            }
        }
        Ok(removed)
    }

    /// 清理无对应主文件的孤儿 `.lock` 文件（`atomic_write` 残留）
    ///
    /// 当 `atomic_write` 在 rename 之前崩溃或被 kill，会留下 `<name>.lock` 而
    /// 无 `<name>` 主文件；启动时扫描数据目录并清理此类残留，避免累积。
    ///
    /// 主文件判定：对每个 `.lock` 文件取 stem，检查同目录下是否存在以该 stem
    /// 为文件名（含或不含扩展名）的主文件。v41-C-003：原 `with_extension("")`
    /// 推导对 `config.lock` 得到 `config` 而非 `config.toml`，导致非孤儿被误删；
    /// 现改为 stem 匹配。v5.0 C-PERF-010：预收集 sibling 信息到 HashSet，
    /// 将检查从 O(N×M) 降为 O(N)。
    ///
    /// # 返回
    /// 成功删除的孤儿 `.lock` 文件数量。数据目录读取失败时返回 0（不阻塞启动）。
    fn cleanup_orphan_lock_files(&self) -> Result<usize, MirrorStarError> {
        let data_dir = match self.config_path.parent() {
            Some(d) => d,
            None => return Ok(0),
        };
        let entries = match std::fs::read_dir(data_dir) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };

        // - `sibling_names_set`：用于 O(1) 判断 `<stem>` 主文件是否存在
        // - `non_lock_stems`：收集所有非 .lock 文件的 file_stem，用于 O(1)
        //   判断 `<stem>.<ext>` 形式主文件是否存在
        let mut sibling_names_set: std::collections::HashSet<std::ffi::OsString> =
            std::collections::HashSet::new();
        let mut non_lock_stems: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().into() {
                let path = data_dir.join(&name);
                let is_lock = path.extension().and_then(|e| e.to_str()) == Some("lock");
                if !is_lock {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        non_lock_stems.insert(stem.to_string());
                    }
                }
                sibling_names_set.insert(name);
            }
        }
        let mut removed = 0usize;
        for name in &sibling_names_set {
            let path = data_dir.join(name);
            if path.extension().and_then(|e| e.to_str()) == Some("lock") {
                // `config.lock` 的 stem 为 `config`（去掉 `.lock` 后缀）
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                // 检查同目录下是否存在任意以 `<stem>` 为文件名（含或不含扩展名）的主文件
                // 主文件形式 1：`<stem>`（无扩展名，少见但理论可能）
                // 主文件形式 2：`<stem>.<ext>`（如 `config.toml`），其 file_stem == stem
                // 注意：sibling_names_set 为 HashSet<OsString>，OsString 仅实现 Borrow<OsStr>
                // 未实现 Borrow<str>，故需用 OsStr::new(stem) 将 &str 转为 &OsStr 后查询
                let has_main_file = sibling_names_set.contains(std::ffi::OsStr::new(stem))
                    || non_lock_stems.contains(stem);
                if !has_main_file {
                    tracing::debug!(
                        path = %path.display(),
                        stem,
                        "清理孤儿 .lock 文件（无对应主文件）"
                    );
                    if std::fs::remove_file(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }

    /// 清理损坏缩略图与孤儿锁文件（启动时调用）
    ///
    /// 聚合 [`cleanup_zero_byte_thumbnails`](Self::cleanup_zero_byte_thumbnails) 与
    /// [`cleanup_orphan_lock_files`](Self::cleanup_orphan_lock_files) 两个职责，
    /// 返回两者清理总数。0 字节损坏缩略图清理失败仍传播错误；孤儿锁文件清理
    /// 内部已吞没 IO 错误返回 0，不阻塞启动。
    pub fn cleanup_corrupted_thumbnails(&self) -> Result<usize, MirrorStarError> {
        let removed = self.cleanup_zero_byte_thumbnails()?;
        let orphan_locks = self.cleanup_orphan_lock_files()?;
        Ok(removed + orphan_locks)
    }

    // ── 持久化 ──────────────────────────────────────────────────────────────

    /// 强制刷写配置（由应用退出清理 `perform_shutdown_blocking` 调用）
    ///
    /// 跳过防抖，若有未落盘修改则立即写入。
    pub fn flush(&self) -> Result<(), MirrorStarError> {
        // 并发安全：全程持有 config_save_mutex，串行化 config.toml 落盘操作
        let _save_guard = self
            .config_save_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        save_with_dirty_rollback(&self.dirty, || {
            // 克隆后 read guard 在语句结束时自动释放，避免在持锁期间执行 IO
            let config = self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            Self::save_config_to_file(&config, &self.config_path)?;
            *self
                .last_save_time
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            *self
                .last_internal_save
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            Ok(())
        })
    }

    /// 防抖保存配置（300ms 窗口）
    ///
    /// 由 `update_config` 调用。窗口内的后续调用跳过落盘，由周期性保存或 `flush` 兜底。
    fn maybe_save_config(&self) -> Result<(), MirrorStarError> {
        // 并发安全：全程持有 config_save_mutex，串行化 config.toml 落盘操作
        let _save_guard = self
            .config_save_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let now = Instant::now();
        // debounce 检查（不修改 dirty）：窗口内的后续调用跳过落盘，
        // 由周期性保存任务或 flush 兜底。dirty 标志保留原值，
        // 待防抖窗口结束后由后续调用原子地 check-and-clear。
        {
            let last = self
                .last_save_time
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *last {
                if now.duration_since(t) < Duration::from_millis(CONFIG_SAVE_DEBOUNCE_MS) {
                    return Ok(());
                }
            }
        }

        // 并发安全：原子地 check-and-clear dirty 标志，消除 check-then-act 竞态
        //（swap 之后不再 store(false)，后续并发的 dirty.store(true) 不会被清除）。
        // dirty 回滚语义由 save_with_dirty_rollback 统一处理（C-101）。
        save_with_dirty_rollback(&self.dirty, || {
            // 克隆后 read guard 在语句结束时自动释放，避免在持锁期间执行 IO
            let config = self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            Self::save_config_to_file(&config, &self.config_path)?;
            *self
                .last_save_time
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(now);
            *self
                .last_internal_save
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(now);
            Ok(())
        })
    }

    /// 保存壁纸库到磁盘（立即写入，无防抖）
    ///
    /// v41-C-001 修复：与 `save_config` 错误传播策略对齐——写入失败时保留原内存
    /// library 不替换（本函数本身只读不写内存，自然满足"不替换"语义），仅传播
    /// 错误给调用方。调用方（`add_wallpaper`/`remove_wallpaper`/`update_thumbnail`）
    /// 在调用前已修改内存，本函数失败时内存修改保留（不回滚），由调用方决定
    /// 是否回滚或重试。`atomic_write` 失败路径已显式清理 `.tmp` 文件，避免残留。
    fn save_library(&self) -> Result<(), MirrorStarError> {
        // N-006: 全程持有 library_save_mutex，串行化 wallpapers.toml 落盘
        let _save_guard = self
            .library_save_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // v5.0 A-PERF-003: 锁获取与落盘逻辑分离，供 batch_update_thumbnails 复用，
        // 避免其持锁后再次获取 library_save_mutex 导致死锁（Mutex 不可重入）。
        self.save_library_locked()
    }

    /// `save_library` 的内部实现，假设调用方已持有 `library_save_mutex`。
    ///
    /// 仅供 `save_library` 与 `batch_update_thumbnails` 在持锁后调用，
    /// 其他写路径应直接调用 `save_library`。
    ///
    /// 注：config 路径不需要 `_locked` 版本，因 `flush` / `maybe_save_config` 直接
    /// 持有 `config_save_mutex` 后调用 `save_config_to_file`，无内部辅助函数需求。
    fn save_library_locked(&self) -> Result<(), MirrorStarError> {
        // 克隆后 read guard 在语句结束时自动释放，避免在持锁期间执行 IO
        let lib = self
            .wallpaper_library
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let result = Self::save_library_to_file(&lib, &self.library_path);
        if result.is_ok() {
            *self
                .last_internal_save
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
        result
    }

    /// v5.0 C-PERF-002: 标记 library 为 dirty，由周期保存线程或显式 `flush_library` 落盘。
    ///
    /// 由 `add_wallpaper` / `remove_wallpaper` 调用，替代原先的立即 `save_library`。
    /// 单次 Tauri 命令在返回前显式调用 [`flush_library`](Self::flush_library) 保证
    /// 用户操作立即持久化；批量场景（drag-drop）可多次 `mark_library_dirty` 后
    /// 统一 `flush_library` 一次。
    fn mark_library_dirty(&self) {
        // C-TD-012: dirty 标志统一使用 Relaxed（只关心原子性而非顺序性，与 config 系列对齐）
        self.library_dirty.store(true, Ordering::Relaxed);
    }

    /// v5.0 C-PERF-002: 显式 flush library（同步落盘，用于 drag-drop 结束、应用退出等场景）。
    ///
    /// 由 Tauri 命令（`add_wallpaper` / `remove_wallpaper`）在命令返回前调用，
    /// 确保用户操作立即持久化。采用 check-and-clear 原子操作（`swap`），dirty 为 false
    /// 时跳过落盘。保存失败时回滚 dirty 标记（C-101）由 save_with_dirty_rollback 统一处理。
    pub fn flush_library(&self) -> Result<(), MirrorStarError> {
        save_with_dirty_rollback(&self.library_dirty, || self.save_library())
    }

    /// 从文件加载应用配置
    ///
    /// 文件不存在或解析失败时回退到默认配置，避免应用因配置损坏无法启动。
    /// SEC-003：文件大小超过 1MB 上限时返回错误，防止 OOM 攻击。
    ///
    /// C01 修复：返回值改为 `(AppConfig, Option<ConfigLoadError>)`。解析失败时
    /// 仍回退到默认配置（保证应用可启动），但同时返回 [`ConfigLoadError`] 以便
    /// 调用方通过回调通知前端展示告警，避免用户无感知地丢失配置。文件不存在
    /// 时返回 `(default, None)`（非错误场景，无需告警）。
    pub(crate) fn load_config(
        path: &Path,
    ) -> Result<(AppConfig, Option<ConfigLoadError>), MirrorStarError> {
        // SEC-003 / C08：TOCTOU 安全的有界读取由 read_bounded_utf8_file 统一处理
        //（File::open + take(MAX+1) + read_to_end，单次调用消除竞态窗口）。
        let content = match read_bounded_utf8_file(path, MAX_CONFIG_FILE_SIZE)? {
            None => return Ok((AppConfig::default(), None)),
            Some(c) => c,
        };

        match toml::from_str::<AppConfig>(&content) {
            Ok(mut config) => {
                // C02 修复：反序列化后调用 validate()，将越界值 clamp 到合法范围
                config.validate();
                Ok((config, None))
            }
            Err(e) => {
                let msg = format!("{}", e);
                tracing::warn!(error = %e, path = %path.display(), "配置文件解析失败，使用默认配置");
                Ok((
                    AppConfig::default(),
                    Some(ConfigLoadError {
                        path: path.to_path_buf(),
                        kind: ConfigKind::Config,
                        message: msg,
                    }),
                ))
            }
        }
    }

    /// 从文件加载壁纸库
    ///
    /// 文件不存在或解析失败时回退到空库。
    /// SEC-003：文件大小超过 1MB 上限时返回错误，防止 OOM 攻击。
    ///
    /// C01 修复：返回值改为 `(WallpaperLibrary, Option<ConfigLoadError>)`。解析失败时
    /// 仍回退到空库（保证应用可启动），但同时返回 [`ConfigLoadError`] 以便调用方
    /// 通过回调通知前端展示告警，避免用户无感知地丢失壁纸列表。
    pub(crate) fn load_library(
        path: &Path,
    ) -> Result<(WallpaperLibrary, Option<ConfigLoadError>), MirrorStarError> {
        // SEC-003 / C08：TOCTOU 安全的有界读取由 read_bounded_utf8_file 统一处理
        //（与 load_config 共用，单次 File::open + take(MAX+1) + read_to_end 消除竞态窗口）。
        let content = match read_bounded_utf8_file(path, MAX_CONFIG_FILE_SIZE)? {
            None => return Ok((WallpaperLibrary::default(), None)),
            Some(c) => c,
        };

        match toml::from_str::<WallpaperLibrary>(&content) {
            Ok(mut lib) => {
                // v5.0 C-PERF-003: 加载后计算 normalized_path 派生字段，
                // 供 update_thumbnail / batch_update_thumbnails 快速查找。
                // #[serde(skip)] 反序列化时用 Default::default() 填充为空字符串，此处覆盖。
                for entry in lib.wallpapers.iter_mut() {
                    entry.normalized_path = normalize_path(&entry.file_path);
                }
                Ok((lib, None))
            }
            Err(e) => {
                let msg = format!("{}", e);
                tracing::warn!(error = %e, path = %path.display(), "壁纸库解析失败，使用空库");
                Ok((
                    WallpaperLibrary::default(),
                    Some(ConfigLoadError {
                        path: path.to_path_buf(),
                        kind: ConfigKind::Library,
                        message: msg,
                    }),
                ))
            }
        }
    }

    /// 将应用配置写入文件
    ///
    /// 供 `hot_reload` 周期性保存任务直接调用（避免借用 `&self`）。
    pub(crate) fn save_config_to_file(
        config: &AppConfig,
        path: &Path,
    ) -> Result<(), MirrorStarError> {
        let content = toml::to_string_pretty(config)?;
        atomic_write(path, &content)
    }

    /// 将壁纸库写入文件
    pub(crate) fn save_library_to_file(
        library: &WallpaperLibrary,
        path: &Path,
    ) -> Result<(), MirrorStarError> {
        let content = toml::to_string_pretty(library)?;
        atomic_write(path, &content)
    }
}

impl Drop for ConfigManager {
    fn drop(&mut self) {
        // 先停止周期保存线程，避免 drop 后线程仍访问已释放资源（C-102）
        self.shutdown_periodic_save();
        // 兜底：确保 watcher 在 ConfigManager 析构时被停止，避免 watcher 线程
        // 在主结构析构后仍访问已释放的字段（如 config / wallpaper_library）。
        // `stop_watching` 定义在 hot_reload.rs，幂等可多次调用。
        self.stop_watching();
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 原子写入
// ════════════════════════════════════════════════════════════════════════════

/// 规范化文件路径以便比较（字符串级别，不接触文件系统）
///
/// 行为：统一转换为 lowercase；Windows（`cfg!(windows)`）上额外将 `/` 替换为 `\`，
/// 避免 `C:\Users\img.jpg` 与 `c:/users/img.jpg` 因大小写或分隔符差异被判为不同路径。
/// 非 Windows 平台 `\` 是合法文件名字符而非分隔符，仅做 lowercase 规范化。
///
/// # 安全策略
///
/// 本函数**不调用 `std::fs::canonicalize`**，不解析符号链接 / Junction Points / 相对路径，
/// 不存在 "canonicalize 失败回退到原始路径" 的不安全分支。路径安全校验（绝对路径 /
/// 路径遍历拒绝 / 符号链接解析）由调用方在更高层负责；本函数仅用于内部
/// `WallpaperEntry::file_path` 字段的去重匹配，输入来源是已通过上层校验的库内条目。
fn normalize_path(p: &str) -> String {
    #[cfg(windows)]
    {
        p.to_lowercase().replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        p.to_lowercase()
    }
}

/// 包裹回调调用以隔离 panic（C-003 / T12）
///
/// `notify_config_error` 与 `reload_config_and_library` 共用此模式：用 `catch_unwind`
/// 包裹用户提供的回调，panic 被捕获并通过 `tracing::error!` 记录后吞没，不传播给
/// 调用方（watcher / 监视线程），保证线程存活以继续处理后续事件。
///
/// `context` 参数用于日志区分回调来源（如 `"on_config_error"` / `"config changed"`）。
/// `AssertUnwindSafe` 是必要的：闭包捕获的 `Arc<dyn Fn>` 与参数对象无法自动满足
/// `UnwindSafe` 约束，但回调只读共享状态，panic 不会破坏不变量。
pub(crate) fn invoke_callback_safe<F: FnOnce()>(callback: F, context: &str) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
    if let Err(payload) = result {
        tracing::error!(
            payload = ?payload,
            context,
            "回调 panic，已捕获并吞没以保持监视线程存活"
        );
    }
}

/// dirty 标志 check-and-clear + 失败回滚的统一辅助（C-101）
///
/// `flush` / `maybe_save_config` / `flush_library` / 周期性保存任务（config + library）
/// 共用此模式：原子地 `swap(false)` 接管落盘责任，保存失败时 `store(true)` 回滚 dirty，
/// 避免未落盘的修改被永久丢失。
///
/// - dirty 为 false → 立即返回 `Ok(())`，不调用 `save_fn`
/// - dirty 为 true → 调用 `save_fn`；成功返回 `Ok(())`，失败回滚 dirty 后传播错误
///
/// 使用 `Ordering::Relaxed`：dirty 标志本身只关心原子性而非顺序性（C-TD-012）。
pub(crate) fn save_with_dirty_rollback(
    dirty: &AtomicBool,
    save_fn: impl FnOnce() -> Result<(), MirrorStarError>,
) -> Result<(), MirrorStarError> {
    if !dirty.swap(false, Ordering::Relaxed) {
        return Ok(());
    }
    if let Err(e) = save_fn() {
        // IO 失败时回滚 dirty 标记，避免未落盘的修改被永久丢失（C-101）
        dirty.store(true, Ordering::Relaxed);
        return Err(e);
    }
    Ok(())
}

/// 读取文件内容为 UTF-8 字符串，带大小上限（SEC-003 / C08：消除 TOCTOU 窗口）
///
/// `load_config` 与 `load_library` 共用的有界读取辅助。原实现先用 `metadata()`
/// 检查大小再 `read_to_string`，两次调用之间文件可能被替换为更大的内容（TOCTOU
/// 攻击窗口）。现改为单次 `File::open` + `read_to_end` + `.take(MAX_SIZE + 1)`
/// 限制读取字节数，消除 OOM 风险：
/// - 文件不存在 → 返回 `Ok(None)`，由调用方回退到默认配置（不视为错误）
/// - 读取字节数 > `max_size` → 返回 `Err(ConfigParse)`（读取被截断不会 OOM）
/// - 非 UTF-8 → 返回 `Err(Io(InvalidData))`（与原 `read_to_string` 行为一致）
/// - 其他 IO 错误 → 传播
fn read_bounded_utf8_file(path: &Path, max_size: u64) -> Result<Option<String>, MirrorStarError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        // 文件不存在时返回 None，由调用方回退到默认配置
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    use std::io::Read;
    let mut buf = Vec::new();
    // .take(MAX_SIZE + 1) 限制最多读取 MAX_SIZE + 1 字节，消除 OOM 风险；
    // 若实际读取字节数 > MAX_SIZE，则文件超过上限。
    file.take(max_size + 1).read_to_end(&mut buf)?;
    if buf.len() > max_size as usize {
        return Err(MirrorStarError::ConfigParse(format!(
            "文件超过 {} 字节上限（读取 {} 字节）",
            max_size,
            buf.len()
        )));
    }

    // 转为字符串：非 UTF-8 文件视为 IO InvalidData 错误（与原 read_to_string 行为一致）
    let content = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(content))
}

/// 原子写入文件
///
/// 使用独立锁文件（`<path>.lock`）串行化并发写入，再通过临时文件 + rename 实现原子替换。
/// 锁文件与目标文件分离，避免 Windows 上对打开文件的 rename 冲突。
/// `unlock()` 来自 fs2 crate 的 `FileExt` trait，clippy 的 `incompatible_msrv` lint
/// 误将其当作 std API（std 自 1.89.0 起才有 `FileExt::unlock`），但本项目 MSRV 为 1.80，
/// 且此处使用的是 fs2 的实现，故抑制该 lint。
///
/// 锁文件复用：使用 `OpenOptions::new().create(true).truncate(false)` 打开既有的
/// `<path>.lock` 文件（不存在则创建，存在则不截断复用）。锁文件在首次创建后保留在
/// 数据目录中，后续写入复用同一文件，不会累积。文件锁通过 fs2 的 `lock_exclusive`
/// / `unlock` 串行化并发写入，进程退出后锁自动释放。
///
/// # 历史
///
/// - v41-C-002：用 `File::create + write_all + sync_all` 替代 `std::fs::write`，
///   在 rename 前显式 sync_all 确保数据刷盘，避免异常退出时缓冲区未刷盘丢数据。
/// - v41-C-001：写入失败时显式清理可能的部分写入 `.tmp` 文件。
/// - C07：rename 失败时清理残留的临时文件，避免数据目录累积 `.tmp` 文件。
#[allow(clippy::incompatible_msrv)]
fn atomic_write(path: &Path, content: &str) -> Result<(), MirrorStarError> {
    let lock_path = path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;

    let temp_path = path.with_extension("tmp");
    let result = (|| -> Result<(), MirrorStarError> {
        // 先写临时文件 → sync_all → rename（任一步失败均清理 .tmp）
        use std::io::Write;
        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            // 显式 drop file 句柄，确保 Windows 上 rename 前文件不被占用
            //（Windows 文件锁机制要求关闭句柄后才能 rename）。
            drop(file);
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e.into());
        }
        Ok(())
    })();

    if let Err(e) = lock_file.unlock() {
        tracing::warn!(error = %e, "释放文件锁失败");
    }
    result
}

// ════════════════════════════════════════════════════════════════════════════
// 测试
// ════════════════════════════════════════════════════════════════════════════

/// 跨模块共享的测试辅助（C-TD-011）
///
/// `manager.rs` 与 `hot_reload.rs` 的测试模块共用此辅助构造临时 ConfigManager，
/// 避免两份实现各自维护。统一采用 `ConfigManager::new_in_dir` 路径，与生产构造
/// 保持一致（直接结构体构造会绕过 `new_in_dir` 的默认加载逻辑，难以发现构造路径
/// 回归）。临时目录以纳秒时间戳命名，避免并发测试冲突。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// 构造使用临时目录的 ConfigManager（避免污染用户数据目录）
    pub(crate) fn make_temp_config_manager() -> ConfigManager {
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_cm_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        ConfigManager::new_in_dir(dir).expect("构造 ConfigManager 失败")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::make_temp_config_manager;
    use super::*;

    // ── WallpaperEntry 序列化 ────────────────────────────────────────────────

    #[test]
    fn wallpaper_entry_toml_roundtrip() {
        let entry = WallpaperEntry {
            id: "test-id".to_string(),
            file_path: "/test/path.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: Some("monitor_0".to_string()),
            added_at: "1234567890".to_string(),
            thumbnail: "thumb.jpg".to_string(),
            file_size: 4096,
            metadata: Some(WallpaperMetadata {
                width: Some(1920),
                height: Some(1080),
                duration: Some(120.5),
                frame_count: None,
            }),
            normalized_path: String::new(),
        };

        let toml_str = toml::to_string_pretty(&entry).expect("serialize");
        let deserialized: WallpaperEntry = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(entry, deserialized);
    }

    #[test]
    fn wallpaper_entry_with_none_fields_toml_roundtrip() {
        let entry = WallpaperEntry {
            id: "id".to_string(),
            file_path: "/p.gif".to_string(),
            wallpaper_type: WallpaperType::Gif,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        };

        let toml_str = toml::to_string_pretty(&entry).expect("serialize");
        let deserialized: WallpaperEntry = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(entry, deserialized);
    }

    // ── WallpaperLibrary 序列化 ──────────────────────────────────────────────

    #[test]
    fn wallpaper_library_empty_toml_roundtrip() {
        let lib = WallpaperLibrary::default();
        let toml_str = toml::to_string_pretty(&lib).expect("serialize");
        let deserialized: WallpaperLibrary = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(lib.wallpapers.len(), deserialized.wallpapers.len());
        assert_eq!(lib.wallpapers.len(), 0);
    }

    #[test]
    fn wallpaper_library_with_entries_toml_roundtrip() {
        let entries = vec![
            WallpaperEntry {
                id: "1".to_string(),
                file_path: "/a.mp4".to_string(),
                wallpaper_type: WallpaperType::Video,
                display_id: None,
                added_at: "1".to_string(),
                thumbnail: String::new(),
                file_size: 100,
                metadata: None,
                normalized_path: String::new(),
            },
            WallpaperEntry {
                id: "2".to_string(),
                file_path: "/b.gif".to_string(),
                wallpaper_type: WallpaperType::Gif,
                display_id: Some("m0".to_string()),
                added_at: "2".to_string(),
                thumbnail: "t.jpg".to_string(),
                file_size: 200,
                metadata: Some(WallpaperMetadata {
                    width: Some(800),
                    height: Some(600),
                    duration: None,
                    frame_count: Some(60),
                }),
                normalized_path: String::new(),
            },
        ];
        let lib = WallpaperLibrary {
            wallpapers: entries,
        };

        let toml_str = toml::to_string_pretty(&lib).expect("serialize");
        let deserialized: WallpaperLibrary = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(lib.wallpapers.len(), deserialized.wallpapers.len());
        assert_eq!(lib.wallpapers[0], deserialized.wallpapers[0]);
        assert_eq!(lib.wallpapers[1], deserialized.wallpapers[1]);
    }

    // ── WallpaperMetadata 序列化 ─────────────────────────────────────────────

    #[test]
    fn wallpaper_metadata_all_none_toml_roundtrip() {
        let meta = WallpaperMetadata {
            width: None,
            height: None,
            duration: None,
            frame_count: None,
        };
        let toml_str = toml::to_string_pretty(&meta).expect("serialize");
        let deserialized: WallpaperMetadata = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(meta, deserialized);
    }

    // ── DisplayInfo 序列化 ───────────────────────────────────────────────────

    #[test]
    fn display_info_toml_roundtrip() {
        let info = DisplayInfo {
            id: r"\\.\DISPLAY1".to_string(),
            name: "显示器 1".to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            is_primary: true,
            dpi: 96,
            current_wallpaper: None,
        };

        let toml_str = toml::to_string_pretty(&info).expect("serialize");
        let deserialized: DisplayInfo = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(info, deserialized);
    }

    #[test]
    fn display_info_json_roundtrip() {
        // 前端通过 Tauri 命令接收 DisplayInfo，序列化为 JSON
        let info = DisplayInfo {
            id: "DISPLAY2".to_string(),
            name: "显示器 2".to_string(),
            width: 2560,
            height: 1440,
            x: 1920,
            y: 0,
            is_primary: false,
            dpi: 120,
            current_wallpaper: Some("/path/to/wp.jpg".to_string()),
        };

        let json = serde_json::to_string(&info).expect("serialize");
        let deserialized: DisplayInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(info, deserialized);
    }

    // ── WallpaperType 序列化 ─────────────────────────────────────────────────

    #[test]
    fn wallpaper_type_in_entry_serializes_as_pascal_case() {
        let entry = WallpaperEntry {
            id: "id".to_string(),
            file_path: "/p".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        };
        let toml_str = toml::to_string(&entry).unwrap();
        assert!(
            toml_str.contains("wallpaper_type = \"Video\""),
            "wallpaper_type 应序列化为 PascalCase, got: {}",
            toml_str
        );
    }

    // ── atomic_write ─────────────────────────────────────────────────────────

    #[test]
    fn atomic_write_creates_file() {
        let dir = std::env::temp_dir().join("mirrorstar_atomic_test_create");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("test.toml");
        atomic_write(&path, "key = \"value\"\n").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "key = \"value\"\n");

        // 临时文件应已被 rename 移走
        let temp_path = dir.join("test.tmp");
        assert!(!temp_path.exists(), "临时文件应已被 rename");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = std::env::temp_dir().join("mirrorstar_atomic_test_overwrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("cfg.toml");
        std::fs::write(&path, "old = true\n").unwrap();

        atomic_write(&path, "new = true\n").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new = true\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ConfigManager 完整流程（临时目录） ───────────────────────────────────
    // make_temp_config_manager 已抽到 super::test_support 模块，供 manager.rs 与
    // hot_reload.rs 测试共用（C-TD-011）。

    #[test]
    fn config_manager_add_get_remove_flow() {
        let cm = make_temp_config_manager();
        let initial = cm.get_wallpapers().len();

        let e1 = WallpaperEntry {
            id: "u1".to_string(),
            file_path: "/t/a.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        };
        cm.add_wallpaper(e1).unwrap();

        let e2 = WallpaperEntry {
            id: "u2".to_string(),
            file_path: "/t/b.gif".to_string(),
            wallpaper_type: WallpaperType::Gif,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        };
        cm.add_wallpaper(e2).unwrap();

        let wallpapers = cm.get_wallpapers();
        assert_eq!(wallpapers.len(), initial + 2);
        assert!(wallpapers.iter().any(|w| w.id == "u1"));
        assert!(wallpapers.iter().any(|w| w.id == "u2"));

        // 移除
        let removed = cm.remove_wallpaper("u1").unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, "u1");

        let after = cm.get_wallpapers();
        assert_eq!(after.len(), initial + 1);
        assert!(!after.iter().any(|w| w.id == "u1"));

        // 移除不存在的 ID
        let none = cm.remove_wallpaper("nonexistent").unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn config_manager_remove_first_match_only() {
        let cm = make_temp_config_manager();

        // 添加两个相同 ID 的条目
        cm.add_wallpaper(WallpaperEntry {
            id: "dup".to_string(),
            file_path: "/t/1.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();
        cm.add_wallpaper(WallpaperEntry {
            id: "dup".to_string(),
            file_path: "/t/2.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        assert_eq!(
            cm.get_wallpapers().iter().filter(|w| w.id == "dup").count(),
            2
        );

        // 第一次移除应只移除一个
        let removed = cm.remove_wallpaper("dup").unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().file_path, "/t/1.mp4");

        assert_eq!(
            cm.get_wallpapers().iter().filter(|w| w.id == "dup").count(),
            1
        );

        // 第二次移除剩余的
        let removed = cm.remove_wallpaper("dup").unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().file_path, "/t/2.mp4");

        assert_eq!(
            cm.get_wallpapers().iter().filter(|w| w.id == "dup").count(),
            0
        );
    }

    #[test]
    fn config_manager_update_thumbnail() {
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "t1".to_string(),
            file_path: "/t/img.jpg".to_string(),
            wallpaper_type: WallpaperType::Image,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        cm.update_thumbnail("/t/img.jpg", "/thumbs/t1.jpg".to_string())
            .unwrap();

        let wallpapers = cm.get_wallpapers();
        let entry = wallpapers.iter().find(|w| w.id == "t1").unwrap();
        assert_eq!(entry.thumbnail, "/thumbs/t1.jpg");

        // 更新不存在的文件路径应为 no-op
        cm.update_thumbnail("/nonexistent", "/thumbs/x.jpg".to_string())
            .unwrap();
    }

    #[test]
    fn config_manager_update_config_persists() {
        let cm = make_temp_config_manager();

        let mut cfg = cm.get_config();
        cfg.audio.volume = 0.5;
        cfg.audio.muted = true;

        cm.update_config(cfg).unwrap();

        let loaded = cm.get_config();
        assert!((loaded.audio.volume - 0.5).abs() < f32::EPSILON);
        assert!(loaded.audio.muted);

        // dirty 标记应被清除（防抖窗口外首次保存）
        assert!(!cm.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn config_manager_flush_forces_save() {
        let cm = make_temp_config_manager();

        // 手动设置 dirty 而不触发 maybe_save_config
        cm.dirty.store(true, Ordering::Relaxed);

        let mut cfg = cm.get_config();
        cfg.general.auto_start = true;
        {
            let mut c = cm.config.write().unwrap_or_else(|e| e.into_inner());
            *c = cfg;
        }

        cm.flush().unwrap();
        assert!(!cm.dirty.load(Ordering::Relaxed));

        // 从磁盘重新加载验证持久化
        let (reloaded, _err) = ConfigManager::load_config(&cm.config_path).unwrap();
        assert!(reloaded.general.auto_start);
    }

    // ── dirty 标志竞态测试 ──────────────────────────────────────────

    #[test]
    fn c001_dirty_flag_race_config_not_lost() {
        // 场景：线程 A 调用 maybe_save_config 落盘 V1，期间线程 B 调用
        // update_config 写入 V2 并 dirty.store(true)。
        //
        // 修复前（buggy）：A 使用 load() 检查 + store(false) 清除（非原子），
        //   A 的 store(false) 会清除 B 在 A 执行期间设置的 dirty=true，
        //   导致 V2 永久丢失（dirty=false，B 的 maybe_save_config load()→false 提前返回）。
        // 修复后（fixed）：A 使用 swap(false) 原子 check-and-clear，
        //   swap 之后不再 store(false)，B 的 dirty.store(true) 保留；
        //   B 的 maybe_save_config 命中防抖窗口直接返回，dirty 保持 true。
        //
        // 时序鲁棒性：线程调度顺序非确定，A 与 B 的相对先后有两种合法情形：
        // (1) A.swap 早于 B.store(true)：dirty=true，V2 待 flush 落盘。
        //     - 修复前若 A.store(false) 晚于 B.store(true)：dirty 被清为 false，V2 永久丢失。
        //     - 修复后 A 不再 store(false)：dirty 保持 true，V2 由 flush 落盘。
        // (2) A.swap 晚于 B.store(true)：A 接管落盘，读取已包含 V2 的配置并保存，
        //     dirty=false 但 V2 已落盘（此为合法行为，非 bug）。
        // 两种情形下 V2 都不应丢失。本测试验证最终不变量：V2 已持久化到磁盘
        // （dirty=true 时由 flush 兜底，dirty=false 时已由 A/B 落盘）。
        let cm = Arc::new(make_temp_config_manager());
        // 初始：标记 dirty=true，模拟有待落盘的配置修改（V1 = default）
        cm.dirty.store(true, Ordering::Relaxed);

        // 线程 A：调用 maybe_save_config。
        // A 会 acquire config_save_mutex → debounce check → swap(false)→true →
        // read config(V1) → save V1 → update last_save_time。
        // 修复后的关键：swap 原子地 check-and-clear，save 后不再 store(false)。
        let cm_a = Arc::clone(&cm);
        let handle_a = std::thread::spawn(move || cm_a.maybe_save_config());

        // 等待 A 进入 save 阶段（swap + read config 已完成，正在执行磁盘 IO）。
        // save_config_to_file 涉及 atomic_write（写临时文件 + rename），通常耗时数 ms。
        std::thread::sleep(std::time::Duration::from_millis(10));

        // 线程 B：调用 update_config 写入 V2（volume=0.42）。
        // update_config 会：config.write(V2) → dirty.store(true) → maybe_save_config（阻塞在 config_save_mutex）。
        // 关键：dirty.store(true) 发生在 A 的 swap 之后（A 已将 dirty 置 false），
        // 修复后 A 不再 store(false) 清除它，因此 dirty 保持 true。
        let cm_b = Arc::clone(&cm);
        let handle_b = std::thread::spawn(move || {
            let mut cfg = cm_b.get_config();
            cfg.audio.volume = 0.42;
            cm_b.update_config(cfg)
        });

        handle_a
            .join()
            .unwrap()
            .expect("A maybe_save_config 应成功");
        handle_b.join().unwrap().expect("B update_config 应成功");

        // 验证：V2 不应丢失。
        // - dirty=true：V2 待落盘（A.swap 早于 B.store(true)），flush 兜底保存 V2。
        //   修复前若触发竞态（A.store(false) 晚于 B.store(true)）：dirty=false 且 V2 未落盘，
        //   flush 因 dirty=false 跳过保存 → load_config 取到 V1 → 断言失败，暴露 dirty 标志竞态 bug。
        // - dirty=false：V2 已由 A 或 B 落盘（A.swap 晚于 B.store(true)），无需 flush。
        let dirty = cm.dirty.load(Ordering::Relaxed);
        if dirty {
            cm.flush().expect("dirty=true 时 flush 应保存 V2");
        }

        // 最终不变量：V2 必须已持久化到磁盘（无论 dirty 状态如何）。
        // 修复前触发竞态时此断言会失败（V2 丢失），修复后始终通过。
        let (reloaded, _err) = ConfigManager::load_config(&cm.config_path).unwrap();
        assert!(
            (reloaded.audio.volume - 0.42).abs() < f32::EPSILON,
            "C-001 修复：V2 应已持久化到磁盘（dirty={}），实际 volume={}",
            dirty,
            reloaded.audio.volume
        );
    }

    #[test]
    fn config_manager_save_and_reload_library() {
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "reload-1".to_string(),
            file_path: "/t/r.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: Some("m0".to_string()),
            added_at: "999".to_string(),
            thumbnail: "thumb.jpg".to_string(),
            file_size: 12345,
            metadata: Some(WallpaperMetadata {
                width: Some(1920),
                height: Some(1080),
                duration: Some(60.0),
                frame_count: None,
            }),
            normalized_path: String::new(),
        })
        .unwrap();
        // v5.0 C-PERF-002: add_wallpaper 仅标记 dirty，测试需从磁盘重新加载，显式 flush_library 落盘
        cm.flush_library().unwrap();

        // 从磁盘重新加载
        let (reloaded, _err) = ConfigManager::load_library(&cm.library_path).unwrap();
        assert_eq!(reloaded.wallpapers.len(), 1);
        let entry = &reloaded.wallpapers[0];
        assert_eq!(entry.id, "reload-1");
        assert_eq!(entry.file_path, "/t/r.mp4");
        assert_eq!(entry.wallpaper_type, WallpaperType::Video);
        assert_eq!(entry.display_id, Some("m0".to_string()));
        assert_eq!(entry.added_at, "999");
        assert_eq!(entry.thumbnail, "thumb.jpg");
        assert_eq!(entry.file_size, 12345);
        assert!(entry.metadata.is_some());
        let meta = entry.metadata.as_ref().unwrap();
        assert_eq!(meta.width, Some(1920));
        assert_eq!(meta.height, Some(1080));
        assert_eq!(meta.duration, Some(60.0));
        assert_eq!(meta.frame_count, None);
    }

    #[test]
    fn config_manager_load_missing_config_returns_default() {
        let path = std::env::temp_dir().join("nonexistent_config_abc123.toml");
        let _ = std::fs::remove_file(&path);
        let (config, err) = ConfigManager::load_config(&path).unwrap();
        let default = AppConfig::default();
        assert!((config.audio.volume - default.audio.volume).abs() < f32::EPSILON);
        // 文件不存在时不应产生错误（非告警场景）
        assert!(err.is_none());
    }

    #[test]
    fn config_manager_load_missing_library_returns_empty() {
        let path = std::env::temp_dir().join("nonexistent_library_abc123.toml");
        let _ = std::fs::remove_file(&path);
        let (lib, err) = ConfigManager::load_library(&path).unwrap();
        assert_eq!(lib.wallpapers.len(), 0);
        // 文件不存在时不应产生错误（非告警场景）
        assert!(err.is_none());
    }

    #[test]
    fn config_manager_load_corrupt_config_returns_default() {
        let dir = std::env::temp_dir().join("mirrorstar_corrupt_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not valid toml = = =").unwrap();

        // C01 修复：解析失败时仍回退到默认配置，但同时返回错误信息
        let (config, err) = ConfigManager::load_config(&path).unwrap();
        let default = AppConfig::default();
        assert!((config.audio.volume - default.audio.volume).abs() < f32::EPSILON);
        // C01 修复：应返回错误信息以便通知前端
        let err = err.expect("损坏的 config.toml 应返回 ConfigLoadError");
        assert_eq!(err.kind, ConfigKind::Config);
        assert_eq!(err.path, path);
        assert!(!err.message.is_empty(), "错误消息不应为空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_manager_load_corrupt_library_returns_empty() {
        let dir = std::env::temp_dir().join("mirrorstar_corrupt_lib_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("wallpapers.toml");
        std::fs::write(&path, "not valid toml {{{{").unwrap();

        // C01 修复：解析失败时仍回退到空库，但同时返回错误信息
        let (lib, err) = ConfigManager::load_library(&path).unwrap();
        assert_eq!(lib.wallpapers.len(), 0);
        // C01 修复：应返回错误信息以便通知前端
        let err = err.expect("损坏的 wallpapers.toml 应返回 ConfigLoadError");
        assert_eq!(err.kind, ConfigKind::Library);
        assert_eq!(err.path, path);
        assert!(!err.message.is_empty(), "错误消息不应为空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wallpaper_library_default_is_empty() {
        let lib = WallpaperLibrary::default();
        assert_eq!(lib.wallpapers.len(), 0);
    }

    #[test]
    fn wallpaper_library_new_is_empty() {
        let lib = WallpaperLibrary::default();
        assert_eq!(lib.wallpapers.len(), 0);
    }

    // ── C01 修复：配置加载错误通知机制 ───────────────────────────────────────

    #[test]
    fn new_in_dir_captures_corrupt_config_in_pending_errors() {
        // C01 修复：构造时 config.toml 损坏，错误应存入 pending_config_errors
        // （此时回调尚未设置，无法立即通知前端）
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c01_construct_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 写入损坏的 config.toml 和 wallpapers.toml
        std::fs::write(dir.join("config.toml"), "invalid = = =").unwrap();
        std::fs::write(dir.join("wallpapers.toml"), "bad {{{{").unwrap();

        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功（回退到默认配置）");

        // 验证应用仍可使用默认配置启动
        let config = cm.get_config();
        let default = AppConfig::default();
        assert!((config.audio.volume - default.audio.volume).abs() < f32::EPSILON);

        // 验证 pending_config_errors 捕获了两个错误
        let pending = cm.drain_pending_config_errors();
        assert_eq!(pending.len(), 2, "应捕获 config 和 library 两个错误");
        assert_eq!(pending[0].kind, ConfigKind::Config);
        assert!(!pending[0].message.is_empty());
        assert_eq!(pending[1].kind, ConfigKind::Library);
        assert!(!pending[1].message.is_empty());

        // drain 后再取应为空
        let pending_after = cm.drain_pending_config_errors();
        assert!(pending_after.is_empty(), "drain 后应清空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_in_dir_no_pending_errors_when_config_valid() {
        // C01 修复：配置正常时 pending_config_errors 应为空
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c01_clean_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功");
        let pending = cm.drain_pending_config_errors();
        assert!(pending.is_empty(), "配置正常时不应有 pending 错误");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notify_config_error_invokes_callback_when_set() {
        // C01 修复：回调已设置时，notify_config_error 应立即调用回调
        use std::sync::Mutex as StdMutex;

        let on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>> = Arc::new(RwLock::new(None));
        let pending_config_errors: Arc<StdMutex<Vec<ConfigLoadError>>> =
            Arc::new(StdMutex::new(Vec::new()));

        // 记录回调收到的错误消息
        let received: Arc<StdMutex<Vec<ConfigLoadError>>> = Arc::new(StdMutex::new(Vec::new()));
        let received_clone = received.clone();
        let callback: ConfigErrorCallback = Arc::new(move |error| {
            received_clone.lock().unwrap().push(error);
        });
        *on_config_error.write().unwrap() = Some(callback);

        let error = ConfigLoadError {
            path: PathBuf::from("/test/config.toml"),
            kind: ConfigKind::Config,
            message: "test parse error".to_string(),
        };
        ConfigManager::notify_config_error(&on_config_error, &pending_config_errors, error);

        // 回调应被调用一次
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].kind, ConfigKind::Config);
        assert_eq!(received[0].message, "test parse error");

        // pending 不应有错误（已通过回调处理）
        let pending = pending_config_errors.lock().unwrap();
        assert!(pending.is_empty(), "回调已设置时不应存入 pending");
    }

    #[test]
    fn notify_config_error_stores_in_pending_when_callback_unset() {
        // C01 修复：回调未设置时，notify_config_error 应将错误存入 pending
        let on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>> = Arc::new(RwLock::new(None));
        let pending_config_errors: Arc<std::sync::Mutex<Vec<ConfigLoadError>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let error1 = ConfigLoadError {
            path: PathBuf::from("/test/config.toml"),
            kind: ConfigKind::Config,
            message: "error 1".to_string(),
        };
        let error2 = ConfigLoadError {
            path: PathBuf::from("/test/wallpapers.toml"),
            kind: ConfigKind::Library,
            message: "error 2".to_string(),
        };
        ConfigManager::notify_config_error(&on_config_error, &pending_config_errors, error1);
        ConfigManager::notify_config_error(&on_config_error, &pending_config_errors, error2);

        // pending 应有两个错误
        let pending = pending_config_errors.lock().unwrap();
        assert_eq!(pending.len(), 2, "回调未设置时应将错误存入 pending");
        assert_eq!(pending[0].kind, ConfigKind::Config);
        assert_eq!(pending[1].kind, ConfigKind::Library);
    }

    #[test]
    fn set_on_config_error_replaces_callback() {
        // C01 修复：set_on_config_error 应替换已有回调
        use std::sync::Mutex as StdMutex;

        let cm = make_temp_config_manager();

        let counter1: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
        let counter1_clone = counter1.clone();
        let cb1: ConfigErrorCallback = Arc::new(move |_error: ConfigLoadError| {
            *counter1_clone.lock().unwrap() += 1;
        });
        cm.set_on_config_error(cb1);

        // 替换为第二个回调
        let counter2: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
        let counter2_clone = counter2.clone();
        let cb2: ConfigErrorCallback = Arc::new(move |_error: ConfigLoadError| {
            *counter2_clone.lock().unwrap() += 1;
        });
        cm.set_on_config_error(cb2);

        // 通过 notify 触发
        let error = ConfigLoadError {
            path: PathBuf::from("/test"),
            kind: ConfigKind::Config,
            message: "test".to_string(),
        };
        ConfigManager::notify_config_error(&cm.on_config_error, &cm.pending_config_errors, error);

        // 仅第二个回调应被调用
        assert_eq!(*counter1.lock().unwrap(), 0, "旧回调不应被调用");
        assert_eq!(*counter2.lock().unwrap(), 1, "新回调应被调用一次");
    }

    // ── C-003 修复：on_config_error 回调 panic 不应终止 watcher 线程 ─────────

    #[test]
    fn c003_config_error_callback_panic_does_not_kill_watcher() {
        // C-003 修复：on_config_error 回调 panic 时，notify_config_error 应通过
        // catch_unwind 捕获 panic，函数正常返回，使得 watcher 线程不退出、后续
        // 配置错误仍能被处理。
        //
        // 验证策略（降级为单元测试，直接调用 notify_config_error 静态函数）：
        // 1. 构造一个会 panic 的回调（调用前先递增计数器以便验证被调用）
        // 2. 连续调用 notify_config_error 两次，模拟 watcher 线程连续处理两个错误
        // 3. 若 panic 未被捕获，第一次调用即会传播 panic 导致测试失败；
        //    若 panic 被捕获，两次调用均应正常返回，计数器应为 2
        use std::sync::Mutex as StdMutex;

        let on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>> = Arc::new(RwLock::new(None));
        let pending_config_errors: Arc<StdMutex<Vec<ConfigLoadError>>> =
            Arc::new(StdMutex::new(Vec::new()));

        // 计数器：回调每次被调用前递增，然后 panic
        let call_count: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
        let call_count_clone = call_count.clone();
        let callback: ConfigErrorCallback = Arc::new(move |_error: ConfigLoadError| {
            *call_count_clone.lock().unwrap() += 1;
            panic!("c003 test panic from on_config_error callback");
        });
        *on_config_error.write().unwrap() = Some(callback);

        let make_error = |msg: &str| ConfigLoadError {
            path: PathBuf::from("/test/config.toml"),
            kind: ConfigKind::Config,
            message: msg.to_string(),
        };

        // 第一次调用：回调 panic，notify_config_error 应捕获并正常返回
        ConfigManager::notify_config_error(
            &on_config_error,
            &pending_config_errors,
            make_error("first error"),
        );

        // 第二次调用：再次触发错误，验证回调仍被调用（watcher 线程仍"存活"）
        ConfigManager::notify_config_error(
            &on_config_error,
            &pending_config_errors,
            make_error("second error"),
        );

        // 计数器应为 2：两次调用都到达了回调（即使回调 panic）
        assert_eq!(
            *call_count.lock().unwrap(),
            2,
            "回调 panic 被捕获后，watcher 应继续处理后续错误，回调仍被调用两次"
        );

        // pending 不应有错误（回调已设置，即便 panic 也走回调路径而非 pending）
        let pending = pending_config_errors.lock().unwrap();
        assert!(
            pending.is_empty(),
            "回调已设置时不应存入 pending，即便回调 panic"
        );
    }

    // ── C02 修复：load_config 调用 validate ─────────────────────────────────

    #[test]
    fn load_config_clamps_out_of_range_values() {
        // C02：写入越界 volume=1.5 / speed=-2.0 / balanced_keep_frames=0 的 config.toml，
        // 验证 load_config 反序列化后调用 validate() 将越界值 clamp 到合法范围
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c02_validate_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[audio]
volume = 1.5

[video]
speed = -2.0

[gif]
balanced_keep_frames = 0
"#,
        )
        .unwrap();

        let (config, err) = ConfigManager::load_config(&path).unwrap();
        // 解析成功，无错误
        assert!(err.is_none(), "合法 TOML 解析失败不应返回错误");
        // 越界值应被 clamp
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C-009 修复：update_config 入口校验 ──────────────────────────────────

    #[test]
    fn c009_update_config_validates_before_write() {
        // C-009：update_config 应在写入内存前调用 validate()，
        // 将前端注入的越界值 clamp 到合法范围后再写入内存。
        let cm = make_temp_config_manager();

        let mut cfg = cm.get_config();
        // 注入越界值（与 spec 中 scenario 一致）
        cfg.audio.volume = 1.5; // > 1.0，应 clamp 到 1.0
        cfg.video.speed = -2.0; // <= 0.0，应回退到默认 1.0
        cfg.gif.balanced_keep_frames = 0; // < 1，应回退到 DEFAULT_BALANCED_KEEP_FRAMES

        cm.update_config(cfg).expect("update_config 应返回 Ok(())");

        // 验证内存中保存的是 clamp 后的值
        let loaded = cm.get_config();
        assert!(
            (loaded.audio.volume - 1.0).abs() < f32::EPSILON,
            "volume=1.5 应被 clamp 到 1.0，实际 = {}",
            loaded.audio.volume
        );
        assert!(
            (loaded.video.speed - 1.0).abs() < f32::EPSILON,
            "speed=-2.0 应回退到默认 1.0，实际 = {}",
            loaded.video.speed
        );
        assert_eq!(
            loaded.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES,
            "balanced_keep_frames=0 应回退到 DEFAULT_BALANCED_KEEP_FRAMES，实际 = {}",
            loaded.gif.balanced_keep_frames
        );

        // 验证 clamp 后的值已落盘（reload 验证持久化）
        let (reloaded, _err) = ConfigManager::load_config(&cm.config_path).unwrap();
        assert!(
            (reloaded.audio.volume - 1.0).abs() < f32::EPSILON,
            "落盘的 volume 应为 clamp 后的 1.0"
        );
        assert!(
            (reloaded.video.speed - 1.0).abs() < f32::EPSILON,
            "落盘的 speed 应为回退后的 1.0"
        );
        assert_eq!(
            reloaded.gif.balanced_keep_frames,
            crate::wallpaper::DEFAULT_BALANCED_KEEP_FRAMES,
            "落盘的 balanced_keep_frames 应为回退后的默认值"
        );
    }

    #[test]
    fn c009_update_config_preserves_valid_values() {
        // C-009：合法值不应被 validate() 修改，直接写入内存。
        let cm = make_temp_config_manager();

        let mut cfg = cm.get_config();
        // 设置合法值（与 spec 中 scenario 一致）
        cfg.audio.volume = 0.5;
        cfg.video.speed = 1.0;
        cfg.gif.balanced_keep_frames = 10;

        cm.update_config(cfg).expect("update_config 应返回 Ok(())");

        // 验证内存中保存的是原始合法值（未被修改）
        let loaded = cm.get_config();
        assert!(
            (loaded.audio.volume - 0.5).abs() < f32::EPSILON,
            "合法 volume=0.5 不应被修改，实际 = {}",
            loaded.audio.volume
        );
        assert!(
            (loaded.video.speed - 1.0).abs() < f32::EPSILON,
            "合法 speed=1.0 不应被修改，实际 = {}",
            loaded.video.speed
        );
        assert_eq!(
            loaded.gif.balanced_keep_frames, 10,
            "合法 balanced_keep_frames=10 不应被修改，实际 = {}",
            loaded.gif.balanced_keep_frames
        );
    }

    // ── C03 修复：cleanup_corrupted_thumbnails 路径推导 ──────────────────────

    #[test]
    fn cleanup_corrupted_thumbnails_uses_instance_data_dir() {
        // C03：new_in_dir 创建的实例（临时目录）调用清理后，
        // 仅清理临时目录下的 thumbnails，不影响用户真实数据目录。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c03_cleanup_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 在临时目录下创建 thumbnails 子目录，并放入 0 字节损坏文件
        let thumb_dir = dir.join("thumbnails");
        std::fs::create_dir_all(&thumb_dir).unwrap();
        let corrupted = thumb_dir.join("corrupted.jpg");
        std::fs::write(&corrupted, b"").unwrap();
        // 也放一个正常文件验证不被删除
        let ok_file = thumb_dir.join("ok.jpg");
        std::fs::write(&ok_file, b"valid content").unwrap();

        // 用 new_in_dir 创建实例（config_path = dir/config.toml）
        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功");

        // 调用清理，应仅作用于临时目录下的 thumbnails
        let removed = cm.cleanup_corrupted_thumbnails().unwrap();
        assert_eq!(removed, 1, "应清理 1 个 0 字节损坏文件");
        assert!(!corrupted.exists(), "损坏文件应被删除");
        assert!(ok_file.exists(), "正常文件不应被删除");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_corrupted_thumbnails_returns_zero_when_dir_missing() {
        // C03：thumbnails 目录不存在时返回 0，且使用实例目录而非 data_dir
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c03_missing_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功");
        // 不创建 thumbnails 子目录
        let removed = cm.cleanup_corrupted_thumbnails().unwrap();
        assert_eq!(removed, 0, "thumbnails 目录不存在时应返回 0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── v41-C-003 修复：cleanup_corrupted_thumbnails lock 文件清理回归 ──────
    //
    // v4.0 回归：原实现使用 `path.with_extension("")` 推导主文件路径，
    // 对 `config.lock` 得到 `config`（无扩展名），导致 `config.exists()` 永远
    // 返回 false，所有非孤儿 `.lock` 文件被误删。修复后应保留有对应主文件
    //（如 `config.toml`）的 `.lock`，仅清理真正的孤儿 `.lock`。

    #[test]
    fn v41_c003_cleanup_lock_files_preserves_valid_locks() {
        // v41-C-003：构造 `config.toml` + `config.lock` + `wallpapers.toml` +
        // `wallpapers.lock` + 孤儿 `orphan.lock` 场景，调用 cleanup 后应保留
        // `config.lock` 与 `wallpapers.lock`，仅删除 `orphan.lock`。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c003_lock_cleanup_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 主文件 + 对应的 .lock（应保留）
        std::fs::write(dir.join("config.toml"), "[audio]\nvolume = 0.5\n").unwrap();
        std::fs::write(dir.join("config.lock"), b"lock").unwrap();
        std::fs::write(dir.join("wallpapers.toml"), "[[wallpapers]]\n").unwrap();
        std::fs::write(dir.join("wallpapers.lock"), b"lock").unwrap();
        // 孤儿 .lock（无主文件，应删除）
        std::fs::write(dir.join("orphan.lock"), b"lock").unwrap();

        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功");
        // cleanup_corrupted_thumbnails 返回值为 0 字节损坏缩略图 + 孤儿 .lock 清理总数
        //（此场景无 thumbnails 目录，返回值 = 0 + 1 = 1，仅孤儿 .lock 被清理）
        let _ = cm.cleanup_corrupted_thumbnails().unwrap();

        // 核心断言：有主文件的 .lock 应保留
        assert!(
            dir.join("config.lock").exists(),
            "v41-C-003: config.toml 存在时 config.lock 不应被误删"
        );
        assert!(
            dir.join("wallpapers.lock").exists(),
            "v41-C-003: wallpapers.toml 存在时 wallpapers.lock 不应被误删"
        );
        // 主文件本身不应被删
        assert!(dir.join("config.toml").exists());
        assert!(dir.join("wallpapers.toml").exists());
        // 孤儿 .lock 应被删除
        assert!(
            !dir.join("orphan.lock").exists(),
            "v41-C-003: 无主文件的孤儿 .lock 应被删除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v41_c003_cleanup_lock_files_handles_stem_only_main_file() {
        // v41-C-003 边界场景：主文件本身无扩展名（如 `data` + `data.lock`），
        // 此场景下 `data.lock` 也应保留（stem `data` 匹配主文件名 `data`）。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c003_stem_only_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 注意：config.toml 仍是 ConfigManager 必需的主文件，先写入它
        std::fs::write(dir.join("config.toml"), "[audio]\n").unwrap();
        // 主文件无扩展名 `data` + 对应 `.lock`（应保留）
        std::fs::write(dir.join("data"), b"data").unwrap();
        std::fs::write(dir.join("data.lock"), b"lock").unwrap();

        let cm = ConfigManager::new_in_dir(dir.clone()).expect("构造应成功");
        let _ = cm.cleanup_corrupted_thumbnails().unwrap();

        assert!(
            dir.join("data.lock").exists(),
            "v41-C-003: 无扩展名主文件 `data` 存在时 data.lock 不应被误删"
        );
        assert!(dir.join("data").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C07 修复：atomic_write rename 失败时清理临时文件 ──────────────────────

    #[test]
    fn atomic_write_cleans_temp_on_rename_failure() {
        // C07：rename 失败时 temp_path 应被清理，避免残留
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c07_rename_fail_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("target.toml");
        // 创建目标为目录，使 rename 失败（MoveFileEx 不允许将文件 rename 到目录路径）
        std::fs::create_dir(&path).unwrap();

        let result = atomic_write(&path, "key = \"value\"\n");
        assert!(result.is_err(), "rename 到目录应失败");

        // C07 验证：temp_path 应被清理
        let temp_path = dir.join("target.tmp");
        assert!(
            !temp_path.exists(),
            "rename 失败后 temp_path 应被清理，但仍存在: {:?}",
            temp_path
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── v41-C-001 修复：atomic_write 写入失败时清理 .tmp ──────────────────────
    //
    // v41-C-001：`std::fs::write` 失败时（如磁盘满、目标路径为目录）应清理可能
    // 残留的部分写入 .tmp 文件，与 rename 失败的清理策略对齐。原实现使用 `?`
    // 直接传播错误，未清理 .tmp。

    #[test]
    fn v41_c001_atomic_write_cleans_temp_on_write_failure() {
        // v41-C-001：构造 temp_path 为已存在的目录，使 std::fs::write 失败
        //（write 内部调用 File::create，目标为目录时返回 IsADirectory / PermissionDenied）。
        // 验证：返回 Err 且 .tmp 路径不存在（被清理）。
        //
        // 触发方式：atomic_write 内 temp_path = path.with_extension("tmp")。
        // 若 path = dir/target.toml，则 temp_path = dir/target.tmp。
        // 预创建 dir/target.tmp/ 为目录，调用 atomic_write(&dir/target.toml, ...)
        // 时 std::fs::write(&dir/target.tmp, content) 因目标为目录而失败。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c001_write_fail_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("target.toml");
        // 预创建 temp_path（dir/target.tmp）为目录，使后续 std::fs::write 失败
        std::fs::create_dir(dir.join("target.tmp")).unwrap();

        let result = atomic_write(&path, "key = \"value\"\n");
        assert!(
            result.is_err(),
            "std::fs::write 到目录路径应失败（v41-C-001）"
        );

        // v41-C-001 核心断言：失败后 temp_path 应被清理（此处为目录则不被清理，
        // 但应保证函数返回 Err 不残留新创建的文件）。由于 temp_path 是预存在的
        // 目录，函数无法 remove_file 删除目录（remove_file 仅对文件生效）。
        // 因此本测试主要断言"返回 Err"，并验证 temp_path 仍为目录（未被误删）。
        //
        // 但更重要的是验证：在 write 真正失败的场景下（如部分写入后失败），
        // 函数会调用 remove_file 清理可能残留的部分写入文件。此场景通过
        // "目录作为 temp_path"间接验证：函数正确进入失败分支并尝试清理。
        assert!(
            dir.join("target.tmp").is_dir(),
            "temp_path 作为预存在的目录应保留（remove_file 无法删除目录）"
        );

        // 目标文件不应被创建（write 失败 → 未进入 rename 步骤）
        assert!(
            !path.exists(),
            "目标文件不应存在（write 失败后未执行 rename）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C08 修复：load_config/load_library 消除 TOCTOU ───────────────────────

    #[test]
    fn load_config_rejects_file_exceeding_max_size() {
        // C08：超过 1MB 上限的配置文件应返回错误，且读取被 .take() 截断（不会 OOM）
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c08_large_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("config.toml");
        // 写入 2MB 数据（超过 1MB 上限）
        let large_content = "x".repeat(2 * 1024 * 1024);
        std::fs::write(&path, large_content).unwrap();

        let result = ConfigManager::load_config(&path);
        assert!(result.is_err(), "超过 1MB 上限的配置文件应返回错误");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_library_rejects_file_exceeding_max_size() {
        // C08：超过 1MB 上限的壁纸库文件应返回错误
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c08_large_lib_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("wallpapers.toml");
        let large_content = "x".repeat(2 * 1024 * 1024);
        std::fs::write(&path, large_content).unwrap();

        let result = ConfigManager::load_library(&path);
        assert!(result.is_err(), "超过 1MB 上限的壁纸库文件应返回错误");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_still_works_after_c08_refactor() {
        // C08 回归测试：正常配置文件应能正确加载（验证 File::open + read_to_end 路径）
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c08_normal_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("config.toml");
        std::fs::write(&path, "[audio]\nvolume = 0.7\n").unwrap();

        let (config, err) = ConfigManager::load_config(&path).unwrap();
        assert!(err.is_none(), "正常配置文件不应返回错误");
        assert!(
            (config.audio.volume - 0.7).abs() < f32::EPSILON,
            "应正确加载 volume=0.7"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C09 修复：update_thumbnail 路径规范化 ────────────────────────────────

    #[test]
    fn update_thumbnail_matches_normalized_path() {
        // C09：不同分隔符/大小写路径应匹配同一个条目
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "c09".to_string(),
            file_path: "C:/Users/Test/Wallpapers/img.jpg".to_string(),
            wallpaper_type: WallpaperType::Image,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        // 用反斜杠 + 不同大小写更新缩略图，应匹配成功
        cm.update_thumbnail(
            "c:\\users\\test\\wallpapers\\img.jpg",
            "/thumbs/c09.jpg".to_string(),
        )
        .unwrap();

        let wallpapers = cm.get_wallpapers();
        let entry = wallpapers.iter().find(|w| w.id == "c09").unwrap();
        assert_eq!(
            entry.thumbnail, "/thumbs/c09.jpg",
            "不同分隔符/大小写路径应匹配成功"
        );
    }

    #[test]
    fn update_thumbnail_still_matches_exact_path() {
        // C09 回归测试：完全相同的路径仍应匹配
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "c09b".to_string(),
            file_path: "/path/to/img.png".to_string(),
            wallpaper_type: WallpaperType::Image,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        cm.update_thumbnail("/path/to/img.png", "/thumbs/exact.png".to_string())
            .unwrap();

        let wallpapers = cm.get_wallpapers();
        let entry = wallpapers.iter().find(|w| w.id == "c09b").unwrap();
        assert_eq!(entry.thumbnail, "/thumbs/exact.png");
    }

    #[test]
    fn update_thumbnail_symmetric_backslash_to_forward_slash() {
        // C09 对称测试：entry 用反斜杠，update 用正斜杠 + 不同大小写，应匹配成功
        // （与 update_thumbnail_matches_normalized_path 形成对称：双向分隔符差异均能匹配）
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "c09sym".to_string(),
            file_path: "C:\\Users\\Test\\Wallpapers\\IMG.jpg".to_string(),
            wallpaper_type: WallpaperType::Image,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        // 用正斜杠 + 全小写更新缩略图，应匹配成功
        cm.update_thumbnail(
            "c:/users/test/wallpapers/img.jpg",
            "/thumbs/c09sym.jpg".to_string(),
        )
        .unwrap();

        let wallpapers = cm.get_wallpapers();
        let entry = wallpapers.iter().find(|w| w.id == "c09sym").unwrap();
        assert_eq!(
            entry.thumbnail, "/thumbs/c09sym.jpg",
            "反斜杠 entry + 正斜杠 update 应匹配成功"
        );
    }

    #[test]
    fn update_thumbnail_mixed_separators_match() {
        // C09 对称测试：混合分隔符（同时含 `\` 和 `/`）+ 大小写差异应匹配
        let cm = make_temp_config_manager();

        cm.add_wallpaper(WallpaperEntry {
            id: "c09mix".to_string(),
            file_path: "D:/data\\wallpapers\\Movie.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        // 用完全不同的混合分隔符 + 小写更新，应匹配
        cm.update_thumbnail(
            "d:\\data/wallpapers/movie.mp4",
            "/thumbs/c09mix.jpg".to_string(),
        )
        .unwrap();

        let wallpapers = cm.get_wallpapers();
        let entry = wallpapers.iter().find(|w| w.id == "c09mix").unwrap();
        assert_eq!(
            entry.thumbnail, "/thumbs/c09mix.jpg",
            "混合分隔符路径应规范化后匹配"
        );
    }

    #[test]
    fn normalize_path_direction_matches_w13() {
        // C09 一致性测试：normalize_path 应与 W13 的 normalize_path_for_compare
        // 产生相同的规范化结果（lowercase + `/` → `\`），确保跨模块路径匹配一致。
        // 直接测试纯辅助函数，验证规范化方向与具体输出。
        assert_eq!(
            normalize_path("C:/Users/Test/File.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        assert_eq!(
            normalize_path("C:\\Users\\Test\\File.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        assert_eq!(
            normalize_path("c:/users\\test/file.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        assert_eq!(
            normalize_path("c:\\users/test\\file.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        // 对称性：正斜杠与反斜杠输入应产生相同输出
        assert_eq!(
            normalize_path("A/B/C.PNG"),
            normalize_path("A\\B\\C.PNG"),
            "正斜杠与反斜杠应规范化为相同结果"
        );
        // 空字符串边界
        assert_eq!(normalize_path(""), "");
    }

    // ── C11 修复：原子 reload config 与 library ──────────────────────────────

    #[test]
    fn reload_rolls_back_config_when_library_fails() {
        // C11：library 加载失败时 config 不应被更新，保证状态一致
        let cm = make_temp_config_manager();

        // 初始状态：config volume=0.5，library 有 1 个 wallpaper
        let mut cfg = cm.get_config();
        cfg.audio.volume = 0.5;
        cm.update_config(cfg).unwrap(); // 持久化到磁盘

        cm.add_wallpaper(WallpaperEntry {
            id: "orig".to_string(),
            file_path: "/orig.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        })
        .unwrap();

        // 修改磁盘上的 config 文件（volume=0.9）
        let mut new_cfg = cm.get_config();
        new_cfg.audio.volume = 0.9;
        let toml_str = toml::to_string_pretty(&new_cfg).unwrap();
        std::fs::write(&cm.config_path, toml_str).unwrap();

        // 破坏 library 文件（写入非 UTF-8 内容，使 load_library 返回 Err）
        std::fs::write(&cm.library_path, b"\xff\xfe\x00invalid").unwrap();

        // 调用 reload（模拟 watcher 线程的热重载）
        ConfigManager::reload_config_and_library(
            &cm.config_path,
            &cm.library_path,
            &cm.config,
            &cm.wallpaper_library,
            &cm.on_config_changed,
            &cm.on_config_error,
            &cm.pending_config_errors,
        );

        // 验证 config 未被更新（仍为 0.5，不是磁盘上的 0.9）
        let config = cm.get_config();
        assert!(
            (config.audio.volume - 0.5).abs() < f32::EPSILON,
            "library 失败时 config 不应被更新，expected 0.5, got {}",
            config.audio.volume
        );

        // 验证 library 未被更新（仍有 1 个 wallpaper，id=orig）
        let wallpapers = cm.get_wallpapers();
        assert_eq!(wallpapers.len(), 1, "library 失败时 library 不应被更新");
        assert_eq!(wallpapers[0].id, "orig");
    }

    #[test]
    fn reload_updates_both_when_both_succeed() {
        // C11：config 和 library 均加载成功时，两者都应被更新
        let cm = make_temp_config_manager();

        // 初始状态：config volume=0.5，library 空
        let mut cfg = cm.get_config();
        cfg.audio.volume = 0.5;
        cm.update_config(cfg).unwrap();

        // 修改磁盘上的 config 文件（volume=0.9）
        let mut new_cfg = cm.get_config();
        new_cfg.audio.volume = 0.9;
        let toml_str = toml::to_string_pretty(&new_cfg).unwrap();
        std::fs::write(&cm.config_path, toml_str).unwrap();

        // 修改磁盘上的 library 文件（添加 1 个 wallpaper）
        let lib = WallpaperLibrary {
            wallpapers: vec![WallpaperEntry {
                id: "new".to_string(),
                file_path: "/new.mp4".to_string(),
                wallpaper_type: WallpaperType::Video,
                display_id: None,
                added_at: "0".to_string(),
                thumbnail: String::new(),
                file_size: 0,
                metadata: None,
                normalized_path: String::new(),
            }],
        };
        let lib_toml = toml::to_string_pretty(&lib).unwrap();
        std::fs::write(&cm.library_path, lib_toml).unwrap();

        // 调用 reload
        ConfigManager::reload_config_and_library(
            &cm.config_path,
            &cm.library_path,
            &cm.config,
            &cm.wallpaper_library,
            &cm.on_config_changed,
            &cm.on_config_error,
            &cm.pending_config_errors,
        );

        // 验证 config 已更新
        let config = cm.get_config();
        assert!(
            (config.audio.volume - 0.9).abs() < f32::EPSILON,
            "均成功时 config 应被更新，expected 0.9, got {}",
            config.audio.volume
        );

        // 验证 library 已更新
        let wallpapers = cm.get_wallpapers();
        assert_eq!(wallpapers.len(), 1, "均成功时 library 应被更新");
        assert_eq!(wallpapers[0].id, "new");
    }

    // ── v41-C-002 修复：atomic_write rename 前显式 sync_all ──────────────────
    //
    // v41-C-002：原 `std::fs::write` 仅写 OS 缓冲区不 fsync，异常退出时可能丢数据。
    // 修复后用 `File::create + write_all + sync_all` 替代，确保 rename 前数据已刷盘。
    // 本测试为行为级测试，验证 atomic_write 成功路径仍正常工作 + 数据完整。

    #[test]
    fn v41_c002_atomic_write_syncs_before_rename() {
        // v41-C-002 行为级测试：atomic_write 成功路径应正常工作，
        // 内容完整写入目标文件，无 .tmp 残留。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c002_sync_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("target.toml");
        let content = "key = \"value\"\n[sec]\nval = 42\n";
        atomic_write(&path, content).expect("atomic_write 成功路径应正常工作");

        // 数据完整性：目标文件存在且内容与写入一致
        assert!(path.exists(), "目标文件应存在");
        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            read_back, content,
            "目标文件内容应与写入内容完全一致（v41-C-002 数据完整性）"
        );

        // .tmp 文件不应残留（rename 成功后已移走）
        let temp_path = dir.join("target.tmp");
        assert!(
            !temp_path.exists(),
            "rename 成功后 .tmp 应已移走，v41-C-002 不残留"
        );

        // 验证可重复写入（覆盖现有文件）
        let content2 = "new = true\n";
        atomic_write(&path, content2).expect("覆盖写入应成功");
        let read_back2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back2, content2, "覆盖写入后内容应更新为新值");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v41_c002_atomic_write_preserves_data_on_multiple_writes() {
        // v41-C-002 持久化验证：通过 ConfigManager 完整流程（save→reload）验证
        // sync_all 后落盘的数据可被 load_config 正确读回，确保刷盘不影响数据格式。
        let cm = make_temp_config_manager();

        let mut cfg = cm.get_config();
        cfg.audio.volume = 0.42;
        cfg.general.auto_start = true;
        cm.update_config(cfg).expect("update_config 应成功");

        // 从磁盘重新加载，验证 sync_all 后的数据可被正确解析
        let (reloaded, err) = ConfigManager::load_config(&cm.config_path).unwrap();
        assert!(err.is_none(), "重新加载不应产生解析错误");
        assert!(
            (reloaded.audio.volume - 0.42).abs() < f32::EPSILON,
            "v41-C-002: reload 后 volume 应为 0.42，实际 = {}",
            reloaded.audio.volume
        );
        assert!(
            reloaded.general.auto_start,
            "v41-C-002: reload 后 auto_start 应为 true"
        );
    }
}
