use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::audio::volume::VolumeControl;
use crate::config::settings::Arrangement;
use crate::desktop::native_wallpaper;
use crate::desktop::DesktopIntegrator;
use crate::wallpaper::gif::GifRenderer;
use crate::wallpaper::image::ImageRenderer;
use crate::wallpaper::video::VideoRenderer;
use crate::wallpaper::web::WebRenderer;
use crate::wallpaper::{
    GifMemoryStrategy, LivenessProbe, PauseCommand, PauseReason, PauseSender, ScalingMode,
    WallpaperRenderer, WallpaperSource, WallpaperState, WallpaperType,
    DEFAULT_BALANCED_KEEP_FRAMES,
};
use crate::MirrorStarError;

pub use super::mode_dispatch::{determine_wallpaper_mode, WallpaperMode};

/// 创建渲染器所需的配置快照（用于在 engine 锁外创建渲染器）
///
/// 由 `WallpaperEngine::renderer_config()` 产生，传给
/// `create_and_play_renderer()` 在 engine 锁外执行耗时操作。
pub struct RendererConfig {
    pub volume_control: Arc<Mutex<VolumeControl>>,
    pub desktop: Arc<Mutex<DesktopIntegrator>>,
    pub arrangement: Arrangement,
    pub gif_memory_strategy: GifMemoryStrategy,
    pub gif_balanced_keep_frames: usize,
    /// v41-W-012: GIF 帧像素内存预算上限（MB），由配置传入
    pub gif_max_memory_mb: usize,
}

/// GIF 内存管理配置（内部加锁，允许 `&self` 更新）
///
/// 此前 `gif_memory_strategy` 与 `gif_balanced_keep_frames` 为引擎直接字段，
/// `set_gif_memory_strategy` 需 `&mut self`，调用方（`update_config`）必须持有
/// engine 的 tokio Mutex 写锁，阻塞其他引擎命令（如 `set_wallpaper`）。
///
/// 现将两个字段合并为 `GifMemoryConfig` 并用 `std::sync::Mutex` 包裹，
/// `set_gif_memory_strategy` 改为 `&self`（仅锁内部 Mutex），调用方可用
/// `try_lock()` 获取 engine 引用而不阻塞其他命令。
#[derive(Debug, Clone)]
pub(crate) struct GifMemoryConfig {
    pub strategy: GifMemoryStrategy,
    pub keep_frames: usize,
    /// v41-W-012: GIF 帧像素内存预算上限（MB）
    pub max_memory_mb: usize,
}

/// `set_wallpaper` 的预备数据（W04 修复：用于将 `play()` 移出 engine 锁范围）
///
/// 由 `WallpaperEngine::prepare_set_wallpaper` 在 engine 锁内产生，
/// 传给 `construct_renderer` 在 engine 锁外执行耗时操作（含 `play()`，
/// 最长达 8s 的 IPC 连接（wp-proc WebView2 冷启动）），最后由 `WallpaperEngine::complete_set_wallpaper`
/// 在 engine 锁内完成嵌入与注册。
///
/// # 三阶段流程（W04）
///
/// ```ignore
/// // 阶段 1（持 engine 锁）：关闭现有壁纸 + 获取配置快照
/// let pending = engine.prepare_set_wallpaper(display_id, source, wp_type)?;
/// drop(engine);  // 释放 engine 锁
///
/// // 阶段 2（锁外执行 play()，最长达 8s 的 IPC 连接（wp-proc WebView2 冷启动））
/// let renderer = construct_renderer(
///     source, wp_type, scaling_mode, &pending.config, pending.clear_native,
/// )?;
///
/// // 阶段 3（重新获取 engine 锁）：嵌入 WorkerW + 注册状态
/// let mut engine = state.wallpaper_engine.lock().await;
/// engine.complete_set_wallpaper(renderer, display_id, source, wp_type)?;
/// ```
pub struct SetWallpaperPending {
    /// 壁纸模式（Native 或 WorkerW）
    pub mode: WallpaperMode,
    /// 渲染器配置快照
    pub config: RendererConfig,
    /// 是否需要在创建渲染器前清除原生壁纸
    pub clear_native: bool,
    /// 是否延迟清除旧的系统壁纸（旧 Native → 新 WorkerW 时置 true）。
    ///
    /// 消除"先关后设"间隙帧：`prepare_set_wallpaper` 关闭旧壁纸时**不立即清除**
    /// 系统壁纸（旧画面保持显示），待新 WorkerW 窗嵌入后再由
    /// `complete_set_wallpaper` 补清除（见 DR-33 原子交换 / 轮换丝滑度审计）。
    /// 仅当 `clear_native` 为 true（新 WorkerW 需清理旧 Native 壁纸）且旧壁纸
    /// 为 Native 模式时启用；旧 WorkerW / 无旧壁纸时恒为 false。
    pub defer_clear_native: bool,
}

// ── 原子交换（DR-33 方案 C / DR-40 双窗安全）───────────────────────────────
//
// 供"壁纸轮换调度器"的 apply 使用，消除换壁纸时的无壁纸窗口。三段式：
//   A `WallpaperEngine::embed_atomic_into`（锁内嵌入 + after_embed/loadfile）
//   B `wait_new_ready`（锁外首帧就绪）
//   C `WallpaperEngine::commit_atomic_swap`（锁内换槽 + terminate 旧）
// 详见方法与 `build_new_renderer` / `wait_new_ready`。

/// 原子交换的最大首帧就绪等待超时（视频 mpv 冷启动 + loadfile 渲染，锁外等待）
pub const ATOMIC_SWAP_READY_TIMEOUT: Duration = Duration::from_secs(8);

/// 原子交换进行中标志（全局静态）
///
/// 原子交换阶段 A→C（`embed_atomic_into` 嵌入新窗 → `commit_atomic_swap` 换槽）
/// 期间置位，src-tauri 全屏终止/恢复检测到该标志时跳过本次操作（事件会重复触发，
/// 可容忍偶发跳过）。防止全屏处置在原子交换期间重建 WorkerW / 冷启动 mpv，导致
/// commit 前校验失败（"新窗父窗口已非当前 WorkerW"）或新窗首帧等待超时。
/// 置位/复位由 src-tauri 调度器（`src-tauri/src/scheduler.rs`）的 RAII 守卫负责。
pub static ATOMIC_SWAP_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 原子交换准备结果（阶段 1 `prepare_atomic_swap` 的返回值）
pub enum AtomicSwapPrepare {
    /// 可进行原子交换：新渲染器就绪后可一键换槽，旧壁纸全程占位
    Swap(AtomicSwapPending),
    /// 无法原子交换（Native 模式无 hwnd 可压底 / 该 display 暂无壁纸），
    /// 调用方应回退到现有 `set_wallpaper` 三阶段流程（先关后设）。
    /// `old_renderer_id` 仍可由调用方用于"同目标短路"判断。
    Fallback { old_renderer_id: Option<String> },
}

/// 阶段 1 产出的原子交换待办数据
pub struct AtomicSwapPending {
    /// 渲染器配置快照（供锁外构建新渲染器）
    pub config: RendererConfig,
    /// 是否需在创建新渲染器前清除原生壁纸（原子交换路径恒为 `false`）
    pub clear_native: bool,
    /// 当前旧渲染器 id（由 `wallpaper_sources` 派生），供 `build_new_renderer`
    /// 短路"新图 == 旧图"以及 `commit_atomic_swap` 换槽。
    pub old_renderer_id: Option<String>,
}

/// `build_new_renderer` 的返回值
pub enum BuildOutcome {
    /// 构建成功，新渲染器已 `play()`，待调用方锁外 `wait_new_ready`
    Ready(Box<dyn WallpaperRenderer>),
    /// 新图与旧图相同（id 一致），无需更换——短路跳过整个 swap
    Skip,
}

/// `commit_atomic_swap` 的返回值
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOutcome {
    /// 换槽完成（old→new）
    Committed,
}

/// 纯净壁纸模式的运行态快照（D7）。
///
/// `WallpaperEngine::pure_mode` 为 `Some` 即"当前处于纯净壁纸模式"，是该模式是否
/// 活跃的**权威状态**（进程级 `pure_mode::is_active()` 标志只用于给调度器 / 状态守卫
/// 做无锁短路判断）。
///
/// # 字段职责
///
/// - `lifted`：本次从 WorkerW 摘出并置顶的（显示器ID, 窗口句柄）列表。退出时逐个
///   交给 `DesktopIntegrator::reattach_after_pure_mode` 按 `arrangement` 嵌回。
/// - `arrangement`：进入时的排列方式快照。退出回嵌必须沿用进入时的排列（模式内
///   用户不可改配置），避免用退出时刻的排列错误地重新布局窗口。
///
/// Native 静态图片的临时呈现窗口不在此结构中，单独存于
/// [`WallpaperEngine::pure_mode_temp`]（自带渲染器句柄，无需回嵌，直接终止即可）。
pub(crate) struct PureModeState {
    /// 已摘出并置顶的壁纸窗口：显示器ID -> 句柄
    pub(crate) lifted: Vec<(String, HWND)>,
    /// 进入时的壁纸排列方式快照（退出回嵌沿用）
    pub(crate) arrangement: Arrangement,
}

// SAFETY: PureModeState 含 HWND（窗口句柄），windows-rs 将其建模为裸指针
// （`*mut c_void`），故默认不满足 `Send`。
//
// Send 安全性：`HWND` 本质是指针大小的**句柄令牌**（不透明整数），其值可安全地跨线程
// 传递——它不指向 Rust 拥有的内存，不携带 Rust 可变状态，复制/移动该值不会产生数据
// 竞争。`String` / `Arrangement` 本身即 `Send`。
//
// 访问约束：本结构只由 `WallpaperEngine::pure_mode` 持有，而 `WallpaperEngine` 的
// 所有访问都经 `Arc<tokio::sync::Mutex<WallpaperEngine>>` 串行化（`&mut self` 借用），
// 因此 `lifted` 的写入（`enter_pure_mode`）与读取（`exit_pure_mode` /
// `abort_pure_mode`）在同一锁内串行执行，不存在并发读写。真正的 Win32 窗口操作
// （`detach_for_pure_mode` / `reattach_after_pure_mode`）统一委托给
// `DesktopIntegrator`，其跨线程安全性与锁内阻塞风险已在 `desktop/mod.rs` 的
// `unsafe impl Send for DesktopIntegrator` 处完整论证（D-004 / v41-D-009）。
//
// Sync 不需要：`Mutex<T>: Sync` 只要求 `T: Send`；调用方不会直接共享
// `&PureModeState` 引用（本结构也不实现 `Sync`）。
unsafe impl Send for PureModeState {}

/// 壁纸引擎，管理所有壁纸实例的生命周期
///
/// # 状态字段职责划分（v41-W-009）
///
/// `WallpaperEngine` 持有两类状态，新开发者需明确区分应操作哪一类：
///
/// - **engine 级状态**（直接字段）：描述壁纸引擎整体运行时配置与活跃壁纸注册表，
///   由 `wallpaper_engine` tokio Mutex 锁保护，所有读写必须在 engine 锁内串行执行。
///   包括：
///   - `wallpapers` / `wallpaper_mode` / `wallpaper_sources` / `wallpaper_scaling_modes`：
///     活跃壁纸实例与模式/来源/缩放注册表
///   - `pause_senders`：per-renderer 快速控制发送端映射（同时是渲染器共享状态的入口）
///   - `pause_reasons` / `gif_config` / `arrangement` / `interaction_mode`：引擎级配置
///   - `global_state_changed`：全局状态变更 broadcast 通道
///
/// - **渲染器共享状态**（间接持有，通过 `pause_senders` 中的
///   `PauseSender.shared_state`）：类型为 `Arc<RwLock<RendererState>>`，存储单个
///   渲染器的实时运行时状态（`state` / `volume` / `pre_mute_volume`），供快速路径
///   （pause/resume/volume/mute）在不获取 engine 锁的情况下读取与更新。`PauseSender`
///   通过 `&self`（engine 锁的共享借用）访问，pause 线程持有 clone 的 sender 直接
///   读写共享状态。
///
/// **判断准则**：修改引擎整体配置或壁纸注册表 → 操作 engine 级直接字段（持 engine 锁）；
/// 仅读写单个渲染器的实时状态/音量 → 通过 `PauseSender` 操作渲染器共享状态（无需
/// engine 锁，但需通过 `&self` 借用保证 `pause_senders` HashMap 访问串行化）。
///
/// # 锁顺序约定
///
/// 为避免死锁，获取锁时必须遵循以下顺序（从外到内）：
///
/// 1. `AppState.wallpaper_engine` (tokio::sync::Mutex) — 最外层锁
/// 2. `WallpaperEngine` 内部锁（如有）
/// 3. `AppState.desktop` (std::sync::Mutex) — 在 engine 锁内获取
///
/// 快速路径方法（pause/resume/volume/mute）通过 `self.pause_senders` 操作，
/// 需要获取 engine 锁（通过 `&self` 访问）。
///
/// # 锁获取路径审查（Task 19.2）
///
/// - `set_wallpaper` / `close_wallpaper`：在 engine 锁内获取 `desktop` 锁
///   （`check_and_reinitialize` / `embed_wallpaper` / `remove_wallpaper`），
///   遵循 engine → desktop 顺序，符合约定。
/// - `*_fast` 方法（`pause_wallpaper_fast` 等）：作为 `&self` 方法，
///   通过 engine 锁访问 `pause_senders`，不获取 desktop 锁。
/// - `shutdown` / `Drop`：在 engine 锁内获取 `desktop` 锁，符合顺序。
///
/// # 已知例外（无死锁风险）
///
/// `get_displays`/`check_desktop_status` 命令、WorkerW 兜底检查任务、Explorer 重启监控
/// 直接获取 `desktop` 锁而不持有 engine 锁，不会形成锁环，无死锁风险。
///
/// # 注意事项
///
/// Tauri 命令层的快速路径方法（`pause_wallpaper` / `resume_wallpaper` /
/// `set_volume` / `toggle_mute`）获取 engine 锁后调用 `*_fast` 方法。
/// Win32 回调（全屏检测、电源监控）和托盘菜单通过全局 `SHARED_ENGINE`
/// 使用 `blocking_lock()` 获取 engine 锁后调用 `pause_all_fast` / `resume_all_fast`。
///
/// # 锁中毒策略（v41-W-011）
///
/// `WallpaperEngine` 内部所有 `Mutex` / `RwLock` 访问统一使用
/// `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁，而非 `unwrap()` panic。
/// 这与 `wallpaper/mod.rs` 顶部"锁中毒恢复策略（W-011）"段落一致，确保锁中毒
/// （仅由 panic-in-lock 触发，概率极低）不会沿调用链传播为进程 panic。
///
/// **权衡**：保留中毒数据（可能半写入不一致）vs 默认值回退（丢失运行时状态）。
/// 选择保留策略的原因是渲染器运行时状态（`WallpaperState` / `volume`）丢失会
/// 导致壁纸停止响应，影响用户可见性；中毒发生后用户可手动重启应用恢复一致性。
/// 详见 `mod.rs` 顶部段落对未来改进方向（配置类数据改用 `Default::default()` 回退）的说明。
pub struct WallpaperEngine {
    /// 活跃壁纸实例：显示器ID -> 渲染器
    pub(crate) wallpapers: HashMap<String, Box<dyn WallpaperRenderer>>,
    /// 桌面集成器引用
    pub(crate) desktop: Arc<Mutex<DesktopIntegrator>>,
    /// 音量控制器（缓存 COM 接口）
    pub(crate) volume_control: Arc<Mutex<VolumeControl>>,
    /// 壁纸排列方式
    pub(crate) arrangement: Arrangement,
    /// 是否处于交互模式（默认 false = 穿透模式）
    pub(crate) interaction_mode: bool,
    /// 每个显示器的壁纸模式跟踪
    pub(crate) wallpaper_mode: HashMap<String, WallpaperMode>,
    /// 活跃壁纸的来源和类型：显示器ID -> (WallpaperSource, WallpaperType)
    pub(crate) wallpaper_sources: HashMap<String, (WallpaperSource, WallpaperType)>,
    /// GIF 内存管理配置（内部 Mutex 加锁，`set_gif_memory_strategy` 改为 `&self`）
    pub(crate) gif_config: std::sync::Mutex<GifMemoryConfig>,
    /// 快速控制发送端映射：显示器ID -> PauseSender
    ///
    /// # v41-W-004 调用方契约（重要）
    ///
    /// `pause_senders` 是普通 `HashMap`，自身不带并发保护。所有读写操作
    /// **必须** 在 `wallpaper_engine` 锁（`&self` 或 `&mut self` 借用）持有期间
    /// 串行执行，否则可能产生数据竞争：
    ///
    /// - **insert**（`embed_and_register_renderer`）：注册新渲染器时调用，必须持锁
    /// - **remove**（`close_wallpaper`）：移除渲染器时调用，必须持锁
    /// - **get / iter**（`fast_path.rs` 中的所有 `pause_*_fast` / `resume_*_fast` /
    ///   `set_volume_fast` / `toggle_mute_fast` / `get_wallpaper_state_fast` /
    ///   `any_playing` / `paused_displays`）：读取/遍历 sender，必须持锁
    /// - **clear**（`shutdown`）：清空所有 sender，必须持锁
    ///
    /// 原设计文档化"HashMap 操作未在锁内完整事务化，remove 失败 sender 时若
    /// 并发 insert 新 sender 可能误删"——本设计选择保留 `HashMap` 而非升级为
    /// `BTreeMap`，原因是引擎锁已串行化所有访问，无并发风险。新增任何访问
    /// `pause_senders` 的方法 **必须** 通过 `&self`（engine 锁的共享借用）访问，
    /// 不得脱离锁单独持有引用。
    ///
    /// 若未来需要脱离 engine 锁访问（如并行查询），需先将此字段升级为
    /// `RwLock<HashMap<String, PauseSender>>` 或类似并发容器。
    pub(crate) pause_senders: HashMap<String, PauseSender>,
    /// 暂停原因位图（协调 fullscreen / power / tray 多状态机）
    pub(crate) pause_reasons: std::sync::Mutex<PauseReason>,
    /// 全局状态变更通道
    ///
    /// 所有 PauseSender 的 `notify_state_changed` 通过 per-renderer 转发任务
    /// 汇聚到此全局通道。Tauri 层在应用启动时调用 `subscribe_state_changes()`
    /// 获取一个 receiver，spawn 一个 tokio task 持续 recv()，收到 display_id
    /// 后 emit `wallpaper-state-changed` 事件刷新前端 UI。
    ///
    /// 设计要点：
    /// - 容量 64：broadcast 通道满时新消息替换最旧未读消息（lag by），
    ///   不阻塞 sender，适合短时间内的多次状态变更通知。
    /// - 全局单通道：避免为每个新壁纸 spawn 新订阅任务（易泄漏），Tauri 层
    ///   只订阅一次即可接收所有渲染器的状态变更。
    /// - 转发任务在 `embed_and_register_renderer` 中 spawn，订阅该 PauseSender
    ///   的 `subscribe_state_changes()` 后转发到本通道；PauseSender drop 时
    ///   转发任务的 receiver 收到 Closed 自动退出。
    global_state_changed: tokio::sync::broadcast::Sender<String>,
    /// 每个显示器当前的缩放模式（W-008 修复：用于 set_scaling_mode 失败回退）
    ///
    /// 记录最近一次通过 `set_scaling_mode` 成功设置的缩放模式。首次调用前
    /// 使用 `ScalingMode::default()`（Fill）作为回退目标。
    ///
    /// 注意：此字段仅在 `set_scaling_mode` 路径内维护，不跟踪 `set_wallpaper`
    /// 初始设置的缩放模式（那需要修改 mode_dispatch.rs，超出 W-008 修复范围）。
    /// 对 W-008 的回退场景足够：用户连续切换缩放模式时，前一次成功的模式
    /// 会被记录，可作为下一次切换失败时的回退目标。
    pub(crate) wallpaper_scaling_modes: HashMap<String, ScalingMode>,
    /// 纯净壁纸模式运行态：`Some` 即"处于模式"（权威状态，见 [`PureModeState`]）。
    ///
    /// 由 [`WallpaperEngine::enter_pure_mode`] 写入、[`WallpaperEngine::exit_pure_mode`] /
    /// [`WallpaperEngine::abort_pure_mode`] 取出，天然提供进入/退出幂等语义。
    pub(crate) pure_mode: Option<PureModeState>,
    /// 纯净壁纸模式的临时呈现渲染器（D4）：显示器ID -> 渲染器。
    ///
    /// 用于给"使用原生 API 静态图片（`WallpaperMode::Native`、没有窗口）"的显示器
    /// 补一个可置顶的 `ImageRenderer` 窗口。**绝不**注册进 `wallpapers` /
    /// `pause_senders` / `wallpaper_sources` / `wallpaper_mode` 等权威状态映射——
    /// 它们不参与壁纸状态机、轮换、暂停位图，只在本模式生命周期内存在，退出/中止
    /// 时逐个 `terminate()` 并清空。
    pub(crate) pure_mode_temp: Vec<(String, Box<dyn WallpaperRenderer>)>,
    /// 测试钩子：注入 `set_wallpaper` 返回值以测试 W-008 回退逻辑
    ///
    /// 仅 `#[cfg(test)]` 存在。测试预填结果队列，`call_set_wallpaper_for_scaling`
    /// 弹出队首代替真实调用；队列为空时回退到真实 `set_wallpaper`。
    #[cfg(test)]
    pub(crate) set_wallpaper_results:
        std::sync::Mutex<std::collections::VecDeque<Result<(), MirrorStarError>>>,
}

/// display-id 解析纯逻辑（单一真相源，抽出便于单测各分支）
///
/// 回退链：显式非空 → 当前活跃壁纸 display_id → 主显示器真实 id → 兜底空串。
/// 空串视为「未指定」，任何一环为空串都继续向下回退；三者皆无时返回空串，
/// 保证无显示器环境下行为与改动前一致且不 panic。
pub(crate) fn resolve_display_id_from(
    explicit: Option<String>,
    first_active: Option<&str>,
    primary: Option<&str>,
) -> String {
    if let Some(id) = explicit {
        if !id.is_empty() {
            return id;
        }
    }
    if let Some(id) = first_active {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(id) = primary {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    String::new()
}

impl WallpaperEngine {
    /// 创建新的壁纸引擎
    pub fn new(
        desktop: Arc<Mutex<DesktopIntegrator>>,
        volume_control: Arc<Mutex<VolumeControl>>,
    ) -> Self {
        // 全局状态变更通道：容量 64，所有渲染器的 notify_state_changed
        // 通过转发任务汇聚到此通道，Tauri 层只订阅一次。
        let (global_state_changed, _) = tokio::sync::broadcast::channel::<String>(64);
        Self {
            wallpapers: HashMap::new(),
            desktop,
            volume_control,
            arrangement: Arrangement::PerMonitor,
            interaction_mode: false,
            wallpaper_mode: HashMap::new(),
            wallpaper_sources: HashMap::new(),
            gif_config: std::sync::Mutex::new(GifMemoryConfig {
                strategy: GifMemoryStrategy::default(),
                keep_frames: DEFAULT_BALANCED_KEEP_FRAMES,
                max_memory_mb: crate::wallpaper::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB,
            }),
            pause_senders: HashMap::new(),
            pause_reasons: std::sync::Mutex::new(PauseReason(0)),
            global_state_changed,
            wallpaper_scaling_modes: HashMap::new(),
            pure_mode: None,
            pure_mode_temp: Vec::new(),
            #[cfg(test)]
            set_wallpaper_results: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// 设置 GIF 内存管理策略（改为 `&self` + 内部 Mutex 加锁）
    ///
    /// 此前 `set_gif_memory_strategy` 需 `&mut self`，调用方必须持有 engine
    /// 的 tokio Mutex 写锁（`lock().await`），阻塞其他引擎命令。改为 `&self` 后，
    /// 调用方可用 `try_lock()` 获取 engine 引用，锁忙时跳过实时更新（非关键：
    /// 下次创建壁纸时从 ConfigManager 读取正确值）。
    ///
    /// 内部使用 `std::sync::Mutex` 保护 `GifMemoryConfig`，锁持有时间极短
    /// （仅字段赋值），不会阻塞其他线程。
    pub fn set_gif_memory_strategy(
        &self,
        strategy: GifMemoryStrategy,
        keep_frames: usize,
        max_memory_mb: usize,
    ) {
        let mut config = self.gif_config.lock().unwrap_or_else(|e| e.into_inner());
        config.strategy = strategy;
        config.keep_frames = keep_frames;
        config.max_memory_mb = max_memory_mb;
        tracing::info!(
            ?strategy,
            keep_frames,
            max_memory_mb,
            "已更新 GIF 内存管理策略"
        );
    }

    /// 获取 GIF 内存管理策略（读取内部 Mutex 保护的字段）
    #[cfg(test)]
    pub(crate) fn gif_memory_strategy(&self) -> GifMemoryStrategy {
        self.gif_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .strategy
    }

    /// 获取 GIF 平衡模式保留帧数（读取内部 Mutex 保护的字段）
    #[cfg(test)]
    pub(crate) fn gif_balanced_keep_frames(&self) -> usize {
        self.gif_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keep_frames
    }

    /// 设置壁纸排列方式。
    ///
    /// 由 src-tauri 通用配置路径同步调用（多屏排列布局域，引擎同步不再挂在轮换
    /// reconcile 上）：`update_config` / `update_arrangement` / 热更新回调，使渲染
    /// 嵌入层遵循顶层 `AppConfig.arrangement`。
    pub fn set_arrangement(&mut self, arrangement: Arrangement) {
        self.arrangement = arrangement;
        tracing::info!(?arrangement, "壁纸排列方式已切换");
    }

    /// 读取当前壁纸排列方式（由调度器编排同步驱动）。
    pub fn arrangement(&self) -> Arrangement {
        self.arrangement
    }

    /// 设置指定显示器的缩放模式
    pub fn set_scaling_mode(
        &mut self,
        display_id: &str,
        mode: ScalingMode,
    ) -> Result<(), MirrorStarError> {
        // 先取旧记忆（视频切换失败时作为回滚目标），再立即记录用户本次意图。
        // 必须在写入新值之前取，保证回退语义与实际生效值一致（不重复查找）。
        let original_mode = self.scaling_mode_for(display_id);
        // 记录用户选择的缩放模式：即使当前**无活跃壁纸**也写入，使后续
        // set_wallpaper（轮换 / 网格「设为壁纸」/ 启动恢复）的缺省回退能取到
        // 用户最新设置，而不是停留在旧值。
        self.wallpaper_scaling_modes
            .insert(display_id.to_string(), mode);

        // 检查是否为 Native 模式
        if self.is_native_mode(display_id) {
            // Native 模式重新调用 set_native_wallpaper 应用新缩放模式
            if let Some((WallpaperSource::File(path), _)) = self.wallpaper_sources.get(display_id) {
                tracing::info!(display_id, ?mode, "Native 模式重新设置壁纸以应用新缩放模式");
                // TODO: 原生壁纸模式暂不支持指定显示器，降级为主屏
                native_wallpaper::set_native_wallpaper(path, mode, None)?;
            }
            return Ok(());
        }

        // WorkerW 模式：检查是否为视频壁纸
        if let Some((source, wp_type)) = self.wallpaper_sources.get(display_id) {
            if *wp_type == WallpaperType::Video {
                // 视频壁纸需要重启 mpv 进程以应用新缩放模式
                // W-008 修复：增加失败回退逻辑，避免 set_wallpaper 失败后壁纸丢失
                // 且 engine 状态不可逆。回退流程：记录 original_mode → 关闭原壁纸 →
                // 尝试 set_wallpaper(new_mode) → 失败时尝试 set_wallpaper(original_mode) →
                // 回退失败时返回双重错误，明确提示"壁纸已停止，需手动恢复"。
                tracing::info!(display_id, ?mode, "视频壁纸切换缩放模式，重启渲染器");
                let source_clone = source.clone();
                let type_clone = *wp_type;
                // W-008: original_mode 已在入口处取（写入新值之前），此处直接复用
                let original_source = source_clone.clone();
                // 先关闭当前壁纸。close_wallpaper 会同步清理该显示器的
                // wallpaper_scaling_modes 条目，故关闭后立即重新写入用户本次
                // 选择的新值，保证视频重启期间记忆与用户意图一致（否则成功路径
                // 结束时记忆会因 close 清理而丢失）。
                self.close_wallpaper(display_id)?;
                self.wallpaper_scaling_modes
                    .insert(display_id.to_string(), mode);
                // 尝试以新缩放模式重启（call_set_wallpaper_for_scaling 支持测试注入）
                let new_result = self.call_set_wallpaper_for_scaling(
                    display_id,
                    &source_clone,
                    type_clone,
                    mode,
                );
                match new_result {
                    Ok(()) => {
                        // 成功：记忆已在入口处写入新值，无需重复
                        return Ok(());
                    }
                    Err(new_err) => {
                        tracing::warn!(
                            error = %new_err,
                            new_mode = ?mode,
                            original_mode = ?original_mode,
                            "新缩放模式重启失败，尝试回退到原缩放模式"
                        );
                        // W-008: 尝试回退到原缩放模式
                        match self.call_set_wallpaper_for_scaling(
                            display_id,
                            &original_source,
                            type_clone,
                            original_mode,
                        ) {
                            Ok(()) => {
                                // 回退成功：把记忆恢复为 original_mode（入口处已写入
                                // 未生效的新值），再返回包含原错误信息的 Err。
                                self.wallpaper_scaling_modes
                                    .insert(display_id.to_string(), original_mode);
                                return Err(MirrorStarError::DesktopIntegration(format!(
                                    "切换缩放模式失败（已回退到原模式 {:?}）：{}",
                                    original_mode, new_err
                                )));
                            }
                            Err(rollback_err) => {
                                // 回退也失败：壁纸已停止，需手动恢复。记忆同样恢复为
                                // original_mode（新值未生效），与实际呈现保持一致。
                                self.wallpaper_scaling_modes
                                    .insert(display_id.to_string(), original_mode);
                                return Err(MirrorStarError::DesktopIntegration(format!(
                                    "切换缩放模式失败且回退也失败，壁纸已停止，需手动恢复。原错误: {}; 回退错误: {}",
                                    new_err, rollback_err
                                )));
                            }
                        }
                    }
                }
            }
        }

        // 其他 WorkerW 模式（Gif/Image）：直接调用渲染器
        if let Some(renderer) = self.wallpapers.get_mut(display_id) {
            renderer.set_scaling_mode(mode);
        }
        Ok(())
    }

    /// 对全部活跃显示器逐个应用缩放模式（`display_id = "all"` 广播语义）。
    ///
    /// 单显示器失败不中断其余显示器（例如某屏走 Native 模式路径失败）：记录
    /// warn 并继续，函数最终返回首个错误；全部成功时返回 `Ok`。选择「记首错但
    /// 继续」的语义——广播的意图是尽力让每个显示器达成目标，个别失败不应
    /// 回滚已成功的显示器。无活跃显示器时为 no-op 返回 `Ok`。
    pub fn set_scaling_mode_for_all(&mut self, mode: ScalingMode) -> Result<(), MirrorStarError> {
        // 先写 "all" 兜底键（无精确项 / 无任何活跃显示器时生效），再广播到各
        // 活跃显示器（广播内部对具体 id 写入各自条目）。
        self.wallpaper_scaling_modes.insert("all".to_string(), mode);
        let ids = self.active_display_ids();
        let mut first_err: Option<MirrorStarError> = None;
        for id in ids {
            if let Err(e) = self.set_scaling_mode(&id, mode) {
                tracing::warn!(
                    display_id = %id,
                    error = %e,
                    "广播缩放模式对单个显示器失败，继续处理其余显示器"
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 调用 set_wallpaper 进行缩放模式切换（W-008：支持测试注入）
    ///
    /// 生产环境直接调用 `self.set_wallpaper`。测试环境（`#[cfg(test)]`）下，
    /// 若 `set_wallpaper_results` 队列非空，则弹出队首结果代替真实调用，
    /// 用于模拟 `set_wallpaper` 的成功/失败以测试 W-008 回退逻辑。
    /// 队列为空时回退到真实 `set_wallpaper` 调用，保持现有测试行为不变。
    ///
    /// 这样设计的原因：`set_wallpaper`（定义在 mode_dispatch.rs）内部调用
    /// `construct_renderer` 创建真实渲染器（VideoRenderer 需启动 mpv 进程），
    /// 在单元测试环境无法可靠触发受控失败。通过测试钩子注入结果，可精确
    /// 验证回退成功 / 双重失败两种场景的错误信息与状态流转。
    fn call_set_wallpaper_for_scaling(
        &mut self,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
        scaling_mode: ScalingMode,
    ) -> Result<(), MirrorStarError> {
        #[cfg(test)]
        {
            let mut queue = self
                .set_wallpaper_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(result) = queue.pop_front() {
                tracing::debug!(
                    ?scaling_mode,
                    injected_ok = result.is_ok(),
                    "测试钩子注入 set_wallpaper 结果"
                );
                return result;
            }
        }
        self.set_wallpaper(display_id, source, wallpaper_type, scaling_mode)
    }

    /// 设置指定显示器的播放速度
    pub async fn set_speed(&mut self, display_id: &str, speed: f32) -> Result<(), MirrorStarError> {
        if let Some(renderer) = self.wallpapers.get_mut(display_id) {
            renderer.set_speed(speed);
        }
        Ok(())
    }

    /// 设置交互模式
    pub fn set_interaction_mode(&mut self, enabled: bool) -> Result<(), MirrorStarError> {
        self.interaction_mode = enabled;
        // 纯净模式契约（D2）：模式内强制关闭鼠标穿透。此时已置顶的窗口不得再被打开
        // `WS_EX_TRANSPARENT`（否则会破坏"任意点击退出"的拦截）。故仅更新字段（供
        // `exit_pure_mode` 按 `!self.interaction_mode` 正确还原穿透），跳过对已置顶窗口的
        // 交互/穿透切换。
        if self.pure_mode.is_some() {
            tracing::info!(
                enabled,
                "纯净壁纸模式进行中：仅记录交互模式，跳过对已置顶窗口的穿透/交互切换"
            );
            return Ok(());
        }
        for renderer in self.wallpapers.values_mut() {
            renderer.set_interaction_mode(enabled);
            renderer.set_mouse_passthrough(!enabled);
        }
        tracing::info!(enabled, "切换交互模式");
        Ok(())
    }

    /// 切换交互模式
    pub fn toggle_interaction(&mut self) -> Result<bool, MirrorStarError> {
        let new_mode = !self.interaction_mode;
        self.set_interaction_mode(new_mode)?;
        Ok(new_mode)
    }

    /// 检查指定显示器是否使用原生壁纸模式
    pub fn is_native_mode(&self, display_id: &str) -> bool {
        matches!(
            self.wallpaper_mode.get(display_id),
            Some(WallpaperMode::Native)
        )
    }

    /// 读取当前全局暂停原因位图快照（调度器轮换抑制用，DR-9 / 设计 §10 Layer 2）。
    ///
    /// 位图仅由 `pause_all_fast` / `resume_all_fast` 在 engine 锁内读写，
    /// 此处短暂加锁读取；锁中毒时中性回退为"无暂停"（`PauseReason(0)`），
    /// 避免调度器因读取暂停状态而 panic（DR-32 健壮性）。
    ///
    /// 返回空位图表示轮换不被抑制；非空（FULLSCREEN / BATTERY / TRAY 任一）表示
    /// 调度器应抑制定时间隔触发的轮换（手动下一张不受限，DR-9）。
    pub fn pause_reasons_snapshot(&self) -> PauseReason {
        self.pause_reasons
            .lock()
            .map(|g| *g)
            .unwrap_or(PauseReason(0))
    }

    /// 将配置中持久化的「按显示器缩放模式」种入运行时记忆。
    ///
    /// 配置（`AppConfig.scaling.modes`）是持久化真相源：对已存在的同 key
    /// **以配置值覆盖**，避免运行时残留值遮挡配置（例如本次启动前临时的
    /// `set_scaling_mode` 调用）。启动时在配置加载后调用一次。
    ///
    /// 特殊键 `"all"`（全部显示器 / 新显示器兜底）原样种入，由
    /// [`Self::scaling_mode_for`] 的查找链在无精确项时兜底。
    pub fn seed_scaling_modes(&mut self, modes: &BTreeMap<String, ScalingMode>) {
        for (display_id, mode) in modes {
            self.wallpaper_scaling_modes
                .insert(display_id.clone(), *mode);
        }
        tracing::info!(count = modes.len(), "已种入按显示器缩放模式配置");
    }

    /// 读取指定显示器当前生效的缩放模式（DR-25，调度器轮换 apply 继承用）。
    ///
    /// 查找链：`精确 display_id` → 特殊键 `"all"`（全部显示器 / 新显示器兜底）
    /// → `ScalingMode::default()`。未记录时回退默认（Fill），与手动
    /// `set_wallpaper` 的默认路径一致。
    pub fn scaling_mode_for(&self, display_id: &str) -> ScalingMode {
        self.wallpaper_scaling_modes
            .get(display_id)
            .or_else(|| self.wallpaper_scaling_modes.get("all"))
            .copied()
            .unwrap_or_default()
    }

    /// 获取创建渲染器所需的配置快照（用于在 engine 锁外创建渲染器）
    pub fn renderer_config(&self) -> RendererConfig {
        // gif_config 使用内部 Mutex 保护，锁持有时间极短（仅读取字段）
        let gif = self.gif_config.lock().unwrap_or_else(|e| e.into_inner());
        RendererConfig {
            volume_control: self.volume_control.clone(),
            desktop: self.desktop.clone(),
            arrangement: self.arrangement,
            gif_memory_strategy: gif.strategy,
            gif_balanced_keep_frames: gif.keep_frames,
            gif_max_memory_mb: gif.max_memory_mb,
        }
    }

    /// 阶段 1：准备设置壁纸（W04 修复：在 engine 锁内执行，为锁外 `play()` 做准备）
    ///
    /// 完成以下操作：
    /// 1. 关闭该显示器上已有的壁纸（含原生壁纸清除）
    /// 2. 决定壁纸模式（Native 或 WorkerW）
    /// 3. 获取渲染器配置快照（`RendererConfig`，含 `Arc` 引用克隆，不持有 engine 借用）
    /// 4. 计算 `clear_native` 标志
    ///
    /// 调用方在获取返回值后应释放 engine 锁，在锁外调用 `construct_renderer`
    /// （含 `play()`），再重新获取锁调用 `complete_set_wallpaper`。
    ///
    /// 对于 Native 模式，`pending.mode == WallpaperMode::Native`，调用方应直接
    /// 调用 `set_native_wallpaper_internal`（在锁内，无需 `play()`），不需要
    /// 阶段 2/3。
    pub fn prepare_set_wallpaper(
        &mut self,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) -> Result<SetWallpaperPending, MirrorStarError> {
        // 职责 0：记录旧壁纸模式（必须在 close_wallpaper 移除 wallpaper_mode 之前读取）
        //
        // 用于"延迟清除"判定（旧 Native → 新 WorkerW）：关闭旧 Native 壁纸时不立即
        // 清除系统壁纸，待新 WorkerW 窗嵌入后由 complete_set_wallpaper 补清除，
        // 消除"先关后设"期间桌面露默认背景的间隙帧（轮换丝滑度审计 Task 3 结论）。
        let old_mode_was_native = self.has_wallpaper(display_id)
            && self.wallpaper_mode.get(display_id) == Some(&WallpaperMode::Native);

        // 新壁纸模式（纯函数，仅计算一次供职责 1/2/4 复用）
        let mode = determine_wallpaper_mode(source, wallpaper_type);

        // 职责 1：关闭现有壁纸
        // 旧 Native 壁纸的清除延后到新窗嵌入后（defer_clear_native=true 分支在
        // complete_set_wallpaper 中补调 clear_native_wallpaper），此处不立即清除，
        // 旧画面保持显示直至新窗覆盖。
        if self.has_wallpaper(display_id) {
            // 延迟清除仅适用于"旧 Native → 新 WorkerW"；其余场景 close 时立即清除
            let defer = old_mode_was_native && mode == WallpaperMode::WorkerW;
            self.close_wallpaper_with_clear(display_id, !defer)?;
        }

        // 职责 3：获取配置快照（Arc 克隆，不持有 engine 借用）
        let config = self.renderer_config();

        // 职责 4：计算 clear_native 标志
        // close_wallpaper 已移除 wallpaper_mode，此处 is_native_mode 恒为 false（W-012）：
        // 与原 set_wallpaper 在 close_wallpaper 之后才调用 is_native_mode 的行为一致。
        // 仅 create_and_play_renderer（异步路径，construct_renderer 入口）使用
        // clear_native = true，该路径不经过 close_wallpaper。
        let clear_native = self.is_native_mode(display_id);
        // 延迟清除判定：仅"旧 Native → 新 WorkerW"需要。新 Native 模式由
        // set_native_wallpaper_internal 直接覆盖旧壁纸，无需清除。
        let defer_clear_native = old_mode_was_native && mode == WallpaperMode::WorkerW;

        Ok(SetWallpaperPending {
            mode,
            config,
            clear_native,
            defer_clear_native,
        })
    }

    /// 阶段 3：完成设置壁纸（W04 修复：在 engine 锁内执行嵌入与注册）
    ///
    /// 将已 `play()` 的渲染器嵌入 WorkerW 并注册到 engine。
    /// 仅适用于 WorkerW 模式（Native 模式由 `set_native_wallpaper_internal` 处理）。
    ///
    /// `defer_clear_native` 由调用方从 `prepare_set_wallpaper` 返回的
    /// `SetWallpaperPending::defer_clear_native` 传入：为 true 表示旧壁纸是 Native
    /// 且切换前未立即清除（`prepare_set_wallpaper` 关闭旧壁纸时跳过清除），须在新窗
    /// 嵌入成功后补调 `clear_native_wallpaper`，消除"先关后设"间隙帧。
    pub fn complete_set_wallpaper(
        &mut self,
        renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
        defer_clear_native: bool,
    ) -> Result<(), MirrorStarError> {
        self.embed_and_register_renderer(renderer, display_id, source, wallpaper_type)?;

        // 补清除：新 WorkerW 窗已嵌入（覆盖全屏），此刻清除旧系统壁纸不会产生
        // 可见间隙。清除失败仅记录日志（系统壁纸残留被新窗完全覆盖，用户不可见）。
        if defer_clear_native {
            if let Err(e) = native_wallpaper::clear_native_wallpaper() {
                tracing::warn!(
                    error = %e,
                    display_id,
                    "延迟清除旧原生壁纸失败（残留壁纸已被新 WorkerW 窗覆盖）"
                );
            }
        }

        Ok(())
    }

    /// 检查指定显示器是否已有壁纸
    pub fn has_wallpaper(&self, display_id: &str) -> bool {
        self.wallpapers.contains_key(display_id) || self.wallpaper_mode.contains_key(display_id)
    }

    /// 返回当前存在活跃壁纸（WorkerW 或原生）的全部显示器 id。
    ///
    /// 供调度器在多屏排列变更时按新布局关闭"不再属于新布局"的渲染器
    /// （多屏排列即时生效，Task 5）。以 `wallpaper_mode` 为权威集合——两种模式
    /// （WorkerW 嵌入 / 原生设置）均写入该映射，与 `cleanup_internal` 一致。
    pub fn active_display_ids(&self) -> Vec<String> {
        self.wallpaper_mode.keys().cloned().collect()
    }

    /// 返回第一个活跃壁纸的 display_id（FE-001 修复）
    ///
    /// 用于命令层在 `display_id` 为 `None` 时回退查找当前活跃壁纸所在的显示器。
    /// HashMap 迭代顺序不保证，但单显示器场景（最常见）下唯一键即为所需值；
    /// 多显示器场景下前端应显式传入 `display_id`，避免依赖此回退。
    pub fn first_active_display_id(&self) -> Option<&str> {
        self.wallpapers.keys().next().map(|s| s.as_str())
    }

    /// 统一 display-id 解析器（单一真相源）
    ///
    /// 将「未指定 / 空串」的 display_id 规范化为真实显示器 id，回退链：
    /// 显式非空 → 当前活跃壁纸 display_id（`first_active_display_id`）
    /// → 主显示器真实 id（`primary_display_id`）→ 兜底空串（无显示器时）。
    /// 手动 / 轮换 / 启动恢复三条设置路径统一复用本方法，保证状态 HashMap
    /// 键一致，`""` 永不作为合法键。
    pub fn resolve_display_id(&self, display_id: Option<String>) -> String {
        resolve_display_id_from(
            display_id,
            self.first_active_display_id(),
            self.primary_display_id().as_deref(),
        )
    }

    /// 返回主显示器的真实 id（`is_primary == true` 项）
    ///
    /// 取自自由函数 `crate::desktop::enumerate_displays()`（不持锁，安全），
    /// 对齐 `layout::plan` / `layout::validate_unit_key` 的「缺省回退主显示器」约定。
    /// 无显示器（枚举为空 / 无主屏标记）时返回 `None`。
    fn primary_display_id(&self) -> Option<String> {
        crate::desktop::enumerate_displays()
            .into_iter()
            .find(|d| d.is_primary)
            .map(|d| d.id)
    }

    /// 订阅全局状态变更通知
    ///
    /// 返回全局 `broadcast::Receiver<String>`，payload 为 `display_id`。
    /// 所有渲染器的 `PauseSender::notify_state_changed` 通过 per-renderer
    /// 转发任务（在 `embed_and_register_renderer` 中 spawn）汇聚到此全局通道。
    ///
    /// # 调用时机
    ///
    /// Tauri 层在应用启动时（`setup` hook）调用**一次**，spawn 一个 tokio
    /// task 持续 `recv()`，收到消息后 emit `wallpaper-state-changed` 事件
    /// 以刷新前端 UI。新设置的壁纸会自动通过转发任务接入本通道，无需重新订阅。
    ///
    /// 注意：broadcast 通道的 `subscribe()` 每次返回新的 receiver，
    /// 仅接收订阅之后发送的消息（不接收历史消息）。
    pub fn subscribe_state_changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.global_state_changed.subscribe()
    }

    /// 准备设置新壁纸：关闭现有壁纸 + 桌面环境就绪预检
    ///
    /// 在获取渲染器配置后、创建渲染器前调用。完成以下职责：
    ///
    /// 1. 关闭该显示器上已有的壁纸（含原生壁纸清除）
    /// 2. 对 WorkerW 类型（Video/Gif/Web/WorkerW Image）执行 WorkerW 就绪预检
    ///    （调用 `ensure_desktop_ready_with_retry`），确保桌面环境在创建渲染器前已就绪。
    ///    这样可在早期失败，避免渲染器创建后才发现 WorkerW 不可用。
    ///
    /// 注意：`create_and_play_renderer` 内部仍会调用 `ensure_desktop_ready`，
    /// 二次检查开销极小（`is_workerw_valid` 仅调用 `IsWindow`），但可保证
    /// 即使外部未调用 `prepare_for_wallpaper` 也能正常工作。
    pub fn prepare_for_wallpaper(
        &mut self,
        display_id: &str,
        wallpaper_type: WallpaperType,
    ) -> Result<(), MirrorStarError> {
        // 职责 1：关闭现有壁纸（含原生壁纸清除）
        if self.has_wallpaper(display_id) {
            self.close_wallpaper(display_id)?;
        }

        // 职责 2：WorkerW 类型预检桌面环境就绪
        // Native 类型（部分静态图片）由 SystemParametersInfo 直接设置，不需要 WorkerW
        let needs_workerw = match wallpaper_type {
            WallpaperType::Image => {
                // Image 既可能走 Native 也可能走 WorkerW（取决于文件格式）
                // 此处保守地不预检，留给 set_wallpaper / create_and_play_renderer
                // 内部根据 determine_wallpaper_mode 决定
                false
            }
            WallpaperType::Video | WallpaperType::Gif | WallpaperType::Web => true,
        };
        if needs_workerw {
            self.ensure_desktop_ready_with_retry()?;
        }

        Ok(())
    }

    /// 设置原生壁纸（快速路径，不需要移出锁外）
    ///
    /// # 职责边界（N-004）
    ///
    /// 此方法是 Native 模式的具体执行入口，仅负责：
    /// 1. 调用 `native_wallpaper::set_native_wallpaper` 写入系统注册表
    /// 2. 在 `wallpaper_mode` / `wallpaper_sources` 中记录状态
    ///
    /// 不负责模式分发（Native vs WorkerW）——模式分发由
    /// `mode_dispatch.rs::set_wallpaper` 或外部调用方（如
    /// `src-tauri/src/commands/wallpaper.rs` 的 3 阶段流程）通过
    /// `determine_wallpaper_mode` 完成，再调用本方法执行 Native 路径。
    ///
    /// 调用者：
    /// - `src-tauri/src/commands/wallpaper.rs::set_wallpaper` 的 3 阶段
    ///   Native 路径（stage 1 `prepare_for_wallpaper` → stage 2 此方法）
    /// - `mode_dispatch.rs::set_wallpaper` 的 Native 分支（消除重复，详见该函数）
    pub fn set_native_wallpaper_internal(
        &mut self,
        display_id: &str,
        path: &str,
        scaling_mode: ScalingMode,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) -> Result<(), MirrorStarError> {
        tracing::info!(display_id, path, "设置原生壁纸");
        // TODO: 原生壁纸模式暂不支持指定显示器，降级为主屏
        native_wallpaper::set_native_wallpaper(path, scaling_mode, None)?;
        self.wallpaper_mode
            .insert(display_id.to_string(), WallpaperMode::Native);
        self.wallpaper_sources
            .insert(display_id.to_string(), (source.clone(), wallpaper_type));
        Ok(())
    }

    /// 确保 desktop 已就绪（WorkerW 已初始化且有效），失败时释放锁后 sleep 重试一次
    ///
    /// 适用场景：持有 `&self`（engine 锁内或 `prepare_for_wallpaper` 等 `&self`
    /// 方法）时调用，直接访问 `self.desktop`。锁外创建渲染器
    /// （`construct_renderer`）无法访问 `&self`，改用自由函数 `ensure_desktop_ready`。
    ///
    /// 将重试 sleep 移出 desktop 锁，避免持锁 sleep 阻塞其他 desktop 访问者
    /// （Task 33：原 `ensure_initialized` 内部持锁 sleep 已移除）。
    ///
    /// 委托给 `ensure_desktop_ready_impl`，与 `ensure_desktop_ready` 共享同一实现。
    pub(crate) fn ensure_desktop_ready_with_retry(&self) -> Result<(), MirrorStarError> {
        ensure_desktop_ready_impl(&self.desktop)
    }

    /// 嵌入 WorkerW 渲染器并记录状态
    ///
    /// 封装 WorkerW 嵌入的公共逻辑：
    /// 1. 获取 desktop 锁嵌入壁纸窗口
    /// 2. 嵌入失败时回滚（terminate 渲染器）
    /// 3. 记录 wallpaper_mode、wallpaper_sources、wallpapers
    /// 4. 将 PauseSender 注册到 self.pause_senders（原生壁纸返回 None 时不注册）
    pub fn embed_and_register_renderer(
        &mut self,
        mut renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) -> Result<(), MirrorStarError> {
        if let Some(hwnd) = renderer.hwnd() {
            let embed_result = {
                let mut desktop = self.desktop.lock().map_err(|e| {
                    MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
                })?;
                desktop.embed_wallpaper(hwnd, display_id, self.arrangement)
            };
            if let Err(e) = embed_result {
                if let Err(terminate_err) = renderer.terminate() {
                    tracing::warn!(
                        error = %terminate_err,
                        "嵌入失败后回滚终止渲染器也失败（错误已被原始嵌入错误覆盖）"
                    );
                }
                return Err(e);
            }
        }

        // 根因 E：嵌入 WorkerW 壁纸层完成后调用 after_embed 钩子。
        //
        // 视频壁纸的 mpv 现以 `--idle=yes` 启动（空窗口、不加载文件），本钩子在此
        // 通过 IPC `loadfile` 加载视频文件，确保视频纹理在窗口已稳定嵌入后才创建。
        // 若在嵌入前加载文件，mpv 会在窗口 `SetParent` 重父化 + `SetWindowPos` 缩放
        // 时创建 4K 视频纹理，触发 D3D11 纹理创建失败（E_OUTOFMEMORY 0x8007000e）
        // → 桌面黑屏。其他渲染器的 after_embed 为 no-op。
        //
        // 钩子失败（如 IPC 未连接）时回滚：终止渲染器并返回错误，与嵌入失败回滚一致。
        if let Err(e) = renderer.after_embed() {
            tracing::error!(
                error = %e,
                display_id,
                "after_embed 钩子执行失败（嵌入后加载视频失败），回滚终止渲染器"
            );
            if let Err(terminate_err) = renderer.terminate() {
                tracing::warn!(error = %terminate_err, "after_embed 失败后回滚终止渲染器也失败");
            }
            return Err(e);
        }

        self.register_renderer_maps(renderer, display_id, source, wallpaper_type);
        Ok(())
    }

    /// 全屏终止恢复路径：重新嵌入 WorkerW 并注册渲染器
    ///
    /// 全屏终止（`terminate_all_fast`）时渲染器被终止（state → Terminated），
    /// 退出全屏后 `resume_all_fast` 对 Terminated 渲染器先 `play()` 完整重启，
    /// 再调用本方法补齐 `embed_and_register_renderer` 的剩余步骤：
    ///
    /// 1. 重新嵌入 WorkerW 壁纸层（`embed_wallpaper`；原生图片 hwnd 为 None 时跳过）
    /// 2. 调用 `after_embed` 钩子（视频渲染器通过 IPC `loadfile` 重新加载视频）
    /// 3. 重建 PauseSender 并注册到 `pause_senders`（旧 sender drop → 旧 mpsc
    ///    channel 关闭 → 旧 pause 线程的 `blocking_recv()` 返回 None 自动退出，
    ///    无泄漏）
    ///
    /// 任一返回 `Err` 的步骤失败时回滚：终止渲染器并将渲染器放回 `wallpapers`，
    /// 保持 engine 状态一致。
    ///
    /// 注意：不修改 `wallpaper_mode` / `wallpaper_sources` / `wallpaper_scaling_modes`
    ///（全屏终止前已存在）。
    pub(crate) fn reembed_and_register_renderer(
        &mut self,
        display_id: &str,
    ) -> Result<(), MirrorStarError> {
        let mut renderer = self.wallpapers.remove(display_id).ok_or_else(|| {
            MirrorStarError::DesktopIntegration(format!(
                "重新嵌入失败：渲染器不存在 ({display_id})"
            ))
        })?;

        // 1. 重新嵌入 WorkerW 壁纸层（与 embed_and_register_renderer 一致；
        //    原生图片 hwnd=None 时跳过，不视为失败）
        if let Some(hwnd) = renderer.hwnd() {
            let embed_result = {
                let mut desktop = self.desktop.lock().map_err(|e| {
                    MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
                })?;
                desktop.embed_wallpaper(hwnd, display_id, self.arrangement)
            };
            if let Err(e) = embed_result {
                if let Err(terminate_err) = renderer.terminate() {
                    tracing::warn!(
                        error = %terminate_err,
                        "reembed 失败后回滚终止渲染器也失败（错误已被原始嵌入错误覆盖）"
                    );
                }
                self.wallpapers.insert(display_id.to_string(), renderer);
                return Err(e);
            }
        }

        // 2. after_embed 钩子：视频渲染器从头 loadfile 加载视频（不再续播，见 video.rs after_embed 注释）；
        //    网页渲染器为 no-op；其他渲染器使用 trait 默认实现（Ok）。
        if let Err(e) = renderer.after_embed() {
            tracing::error!(
                error = %e,
                display_id,
                "reembed after_embed 钩子执行失败（嵌入后加载视频失败），回滚终止渲染器"
            );
            if let Err(terminate_err) = renderer.terminate() {
                tracing::warn!(
                    error = %terminate_err,
                    "reembed after_embed 失败后回滚终止渲染器也失败"
                );
            }
            self.wallpapers.insert(display_id.to_string(), renderer);
            return Err(e);
        }

        // 3. 重建 PauseSender（替换旧 sender；返回 None 时不插入、不视为失败）
        if let Some(sender) = renderer.create_pause_sender(display_id) {
            self.register_pause_sender(sender, display_id);
        }

        self.wallpapers.insert(display_id.to_string(), renderer);
        tracing::info!(display_id, "全屏恢复：渲染器已重新嵌入并注册");
        Ok(())
    }

    /// 复活一个已被终止（`WallpaperState::Terminated`，`hwnd()` 为空）的渲染器。
    ///
    /// 由 `fast_path.rs::resume_all_fast` 的 Terminated 分支抽取而来（纯净壁纸模式
    /// `enter_pure_mode` 对"被全屏应用终止 / 子进程崩溃"的显示器同样需要先重建再置顶，
    /// 见 D3），行为与原内联实现**逐行一致**：
    ///
    /// 1. `play()` 完整重启渲染器（视频渲染器在 `after_embed` 时从保存进度续播）
    /// 2. `reembed_and_register_renderer` 重新嵌入 WorkerW、执行 `after_embed`
    ///    （IPC `loadfile` 重新加载视频）并重建 PauseSender
    /// 3. 将共享状态同步为 `Playing` 并 `notify_state_changed` 通知前端 UI
    ///
    /// # 暂停位图
    ///
    /// **不触碰** `pause_reasons`：本方法只负责"重建渲染器窗口"，暂停/恢复位图由
    /// 调用方（`resume_all_fast` / `enter_pure_mode`）自行协调，避免把无关语义
    /// 塞入位图。
    ///
    /// # 错误处理
    ///
    /// 任一失败分支均记录与原实现相同的 `error` 日志并返回 `Err`（调用方负责聚合
    /// 为 `failed` 列表或向上传播）。
    pub(crate) fn revive_terminated(&mut self, display_id: &str) -> Result<(), MirrorStarError> {
        // 先通过 `get_mut().map(|r| r.play())` 得到 `Option<Result<..>>`：
        // 闭包返回 play() 的 Result，借用随 map 调用结束而释放，之后才能
        // 以 `&mut self` 调用 reembed_and_register_renderer（避免借用冲突）。
        match self.wallpapers.get_mut(display_id).map(|r| r.play()) {
            Some(Ok(())) => {
                match self.reembed_and_register_renderer(display_id) {
                    Ok(()) => {
                        // 此处 get 返回 reembed 替换后的新 sender；将共享状态
                        // 同步为 Playing 并通知前端 UI。
                        if let Some(sender) = self.pause_senders.get(display_id) {
                            sender.set_state(WallpaperState::Playing);
                            sender.notify_state_changed(display_id);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!(
                            display_id,
                            error = %e,
                            "全屏终止后重启壁纸失败：重新嵌入失败"
                        );
                        Err(e)
                    }
                }
            }
            Some(Err(e)) => {
                tracing::error!(display_id, error = %e, "全屏终止后重启壁纸失败");
                Err(e)
            }
            None => {
                tracing::error!(display_id, "全屏终止后重启壁纸失败：渲染器不存在");
                Err(MirrorStarError::DesktopIntegration(format!(
                    "全屏终止后重启壁纸失败：渲染器不存在 ({display_id})"
                )))
            }
        }
    }

    /// 采集"应为播放"的视频渲染器探针快照（display_id 升序，便于测试确定性）。
    ///
    /// 只收集 `state() == Playing` 且 `liveness_probe()` 返回 `Some` 的显示器：
    /// - Terminated（已被全屏处置终止）不参与——由既有恢复路径负责；
    /// - Paused 不参与——暂停时时钟本就不前进，属预期；
    /// - 图片 / GIF / 网页渲染器探针为 `None`，自然不入集。
    ///
    /// 之所以把"逐屏可能阻塞的 IPC 查询"收敛到本方法并只在调用方显式采样时执行：
    /// 看护/退出编排持有上一轮快照做比较，判定策略留在调用方（引擎只提供原语），
    /// 避免引擎自行起线程或缓存采样引入额外状态。排序保证多屏结果稳定，便于断言。
    pub fn probe_playing_videos(&mut self) -> Vec<(String, LivenessProbe)> {
        let mut probes: Vec<(String, LivenessProbe)> = self
            .wallpapers
            .iter_mut()
            .filter(|(_, renderer)| renderer.state() == WallpaperState::Playing)
            .filter_map(|(display_id, renderer)| {
                renderer
                    .liveness_probe()
                    .map(|probe| (display_id.clone(), probe))
            })
            .collect();
        probes.sort_by(|a, b| a.0.cmp(&b.0));
        probes
    }

    /// 自愈停滞的视频渲染器（重建渲染器，复用既有"全屏终止后恢复"链路）。
    ///
    /// 顺序：`terminate()`（失败仅 warn 并继续）→ `revive_terminated(display_id)`
    /// （`play()` + `reembed_and_register_renderer()` + PauseSender 状态同步）。
    ///
    /// **禁止**在渲染器仍存活时直接调用 `play()`：`VideoRenderer::play()` 无条件
    /// spawn 新 mpv 进程/窗口，会产生第二个 mpv。故必须先 `terminate()`。
    /// **不触碰** `pause_reasons` 位图：自愈只重建渲染器，暂停语义由调用方保证。
    pub fn heal_stalled_renderer(
        &mut self,
        display_id: &str,
        prev_time_pos: f64,
        curr_time_pos: f64,
        source: &str,
    ) -> Result<(), MirrorStarError> {
        tracing::warn!(
            display_id,
            prev_time_pos,
            curr_time_pos,
            source,
            "检测到视频壁纸输出停滞，执行自愈（重建渲染器）"
        );
        // 先终止：terminate 会置 Terminated 状态 + `ipc.quit` + `stop_process`（超时强杀）。
        // 失败不阻断——即便进程未能优雅退出，随后 revive_terminated 的 play() 仍会尝试
        // 重建；强杀兜底在 stop_process 内已尽力。终止失败仅 warn 保留证据。
        if let Some(renderer) = self.wallpapers.get_mut(display_id) {
            if let Err(e) = renderer.terminate() {
                tracing::warn!(
                    display_id,
                    error = %e,
                    "停滞自愈：终止渲染器失败，继续尝试重建"
                );
            }
        }
        self.revive_terminated(display_id)
    }

    /// 为 PauseSender spawn 状态变更转发任务并注册到 `pause_senders`。
    ///
    /// 抽象自 `embed_and_register_renderer` / `reembed_and_register_renderer` 的
    /// 公共逻辑（DR-33 原子交换 `commit_atomic_swap` 亦复用之）：将 PauseSender 的
    /// `state_changed` broadcast 通道转发到全局通道。`Handle::try_current()` 使
    /// 同步测试（无 tokio runtime）环境跳过 spawn。转发任务在 sender 被
    /// `close_wallpaper` drop 时因 `recv()` 返回 `Closed` 自动退出，无泄漏。
    fn register_pause_sender(&mut self, sender: PauseSender, display_id: &str) {
        let rx = sender.subscribe_state_changes();
        let global_tx = self.global_state_changed.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut rx = rx;
                while let Ok(display_id) = rx.recv().await {
                    // send 失败仅因无订阅者（Tauri 层未启动或已退出），静默忽略
                    let _ = global_tx.send(display_id);
                }
            });
        }
        self.pause_senders.insert(display_id.to_string(), sender);
    }

    /// 将已完成嵌入的渲染器一次性注册进 engine 各 map（WorkerW 模式）。
    ///
    /// 抽象自 `embed_and_register_renderer` 的注册逻辑（DR-33 原子交换
    /// `commit_atomic_swap` 亦复用之）。写入 `wallpaper_mode` / `wallpaper_sources` /
    /// `pause_senders` / `wallpapers`；不负责嵌入（embed）与 `after_embed` 钩子，
    /// 由调用方在达到会失败前完成。
    fn register_renderer_maps(
        &mut self,
        mut renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) {
        self.wallpaper_mode
            .insert(display_id.to_string(), WallpaperMode::WorkerW);
        self.wallpaper_sources
            .insert(display_id.to_string(), (source.clone(), wallpaper_type));
        if let Some(sender) = renderer.create_pause_sender(display_id) {
            self.register_pause_sender(sender, display_id);
        }
        self.wallpapers.insert(display_id.to_string(), renderer);
    }

    /// 阶段 1（锁内，廉价）：准备原子交换的配置快照，旧壁纸保持不动。
    ///
    /// 原子交换（方案 C，DR-33 §8.4）用于"轮换 apply"时消除无壁纸窗口：旧壁纸
    /// 全程不 close/不 hide/不 pause、不离开槽位，继续盖住屏幕作为占位；返回的
    /// `config` 快照供调用方在引擎锁外构建新渲染器。
    ///
    /// - **绝不关闭旧壁纸**（与 `prepare_set_wallpaper` 不同，那是阶段 1 先关旧）。
    /// - 记录当前旧渲染器 `id`（由 `wallpaper_sources` 派生），供 `build_new_renderer`
    ///   短路新图 == 旧图，以及 `commit_atomic_swap` 换槽。
    ///
    /// # 回退
    ///
    /// 仅当该 display 当前存在 WorkerW（含 hwnd 可压底）渲染器时才返回 `Swap`；
    /// Native 模式（无 hwnd 可压底）或无现有壁纸时返回 `Fallback`，调用方应回退
    /// 到现有 `set_wallpaper` 三阶段流程（先关后设）。
    pub fn prepare_atomic_swap(
        &mut self,
        display_id: &str,
        _source: &WallpaperSource,
        _wallpaper_type: WallpaperType,
    ) -> AtomicSwapPrepare {
        let old_renderer_id = self
            .wallpaper_sources
            .get(display_id)
            .map(|(s, t)| renderer_id(s, *t));

        // 需要"旧 WorkerW 保持压底占位"才可原子交换；Native 无 hwnd 或暂无壁纸 → 回退
        if !self.wallpapers.contains_key(display_id) || self.is_native_mode(display_id) {
            return AtomicSwapPrepare::Fallback { old_renderer_id };
        }

        let config = self.renderer_config();
        AtomicSwapPrepare::Swap(AtomicSwapPending {
            config,
            clear_native: false,
            old_renderer_id,
        })
    }

    /// 阶段 A（锁内）：嵌入新窗并点火（after_embed / loadfile），旧壁纸保持占位。
    ///
    /// 原子交换三段式的首段：在引擎锁内将新窗以 `HWND_BOTTOM` 藏入旧壁纸之下，
    /// 然后触发 `after_embed`（视频：IPC `loadfile`）。返回后调用方可在引擎锁外
    /// `wait_new_ready` 等待首帧就绪。
    ///
    /// 本方法**不登记新窗**、**不从 `self.wallpapers` 移除旧窗**、**不 terminate 旧窗**
    /// ——这些留给 `commit_atomic_swap`（阶段 C）。任何失效步骤返回 `Err` 并
    /// terminate 新窗，旧壁纸仍在 map、原封不动（DR-40：A/B 阶段旧窗始终占位）。
    ///
    /// 所有权：调用方传入 `new_renderer`，本方法借用执行嵌入后原样返回（仍由调用方持有），
    /// 复用同一对象做 `wait_new_ready` 与 `commit_atomic_swap`。
    pub fn embed_atomic_into(
        &mut self,
        mut new_renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
    ) -> Result<Box<dyn WallpaperRenderer>, MirrorStarError> {
        // 1. 新窗藏入旧壁纸之下（HWND_BOTTOM）。嵌入失败 → 终止新窗并返回 Err，旧壁纸未动
        if let Some(hwnd) = new_renderer.hwnd() {
            let embed_result = {
                let mut desktop = self.desktop.lock().map_err(|e| {
                    MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
                })?;
                desktop.embed_wallpaper(hwnd, display_id, self.arrangement)
            };
            if let Err(e) = embed_result {
                tracing::warn!(error = %e, display_id, "原子交换：阶段 A 新窗嵌入失败，终止新窗、保持旧壁纸");
                let _ = new_renderer.terminate();
                return Err(e);
            }
        }

        // 2. after_embed（视频 loadfile；失败 terminate 新窗、旧壁纸未动）
        if let Err(e) = new_renderer.after_embed() {
            tracing::error!(error = %e, display_id, "原子交换：阶段 A after_embed 失败，终止新窗、保持旧壁纸");
            let _ = new_renderer.terminate();
            return Err(e);
        }

        tracing::info!(display_id, "原子交换：阶段 A 完成（新窗已嵌入并 loadfile）");
        Ok(new_renderer)
    }

    /// 阶段 C（锁内）：一次性换槽 old→new（DR-33 步骤 C，DR-40 双窗安全）。
    ///
    /// 前置条件：新渲染器已由调用方完成阶段 A（`embed_atomic_into`：嵌入 + loadfile）
    /// 与阶段 B（引擎锁外 `wait_new_ready`：首帧就绪）。
    ///
    /// 执行步骤（换槽临界区）：
    /// 1. DR-40 commit 前校验：新窗父窗口仍为当前 WorkerW，否则放弃 commit（terminate 新窗）。
    ///    校验先于换槽，校验失败时旧壁纸仍在 map 未动，无需回滚，也不会因 Drop 误杀旧壁纸
    /// 2. 校验通过后才 `self.wallpapers.remove(display_id)` 取旧（P1-5：A/B 阶段旧窗始终在 map）
    /// 3. 一次换槽（复用 `register_renderer_maps`，不先关旧）
    /// 4. 按当前全局 `PauseReason` 位图对新渲染器补发暂停（DR-20）
    /// 5. terminate 旧渲染器（失败则强毁旧 hwnd 兜底）
    ///
    /// 任何失效步骤在换槽前返回 `Err` 并 terminate 新渲染器；旧壁纸全程在 map、未动，
    /// 无需重嵌回滚。换槽后无 `Err` 路径，返回 `SwapOutcome::Committed`。
    pub fn commit_atomic_swap(
        &mut self,
        mut new_renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) -> Result<SwapOutcome, MirrorStarError> {
        // DR-40 commit 前校验：新窗父窗口仍为当前 WorkerW，否则放弃 commit（terminate 新窗）。
        // 校验必须先于换槽：若先 remove 旧窗，校验失败路径上 old_renderer 随函数返回被 Drop，
        // Drop 会终止旧壁纸进程（VideoRenderer::drop），导致屏幕空白且轮换永不提交。
        if let Some(hwnd) = new_renderer.hwnd() {
            let parent_ok = {
                let desktop = self.desktop.lock().map_err(|e| {
                    MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
                })?;
                desktop.is_child_of_workerw(hwnd)
            };
            if !parent_ok {
                tracing::warn!(
                    display_id,
                    "原子交换：commit 前校验失败（新窗父窗口已非当前 WorkerW），弃置新窗"
                );
                let _ = new_renderer.terminate();
                return Err(MirrorStarError::DesktopIntegration(
                    "原子交换 commit 前校验失败：新窗父窗口已非当前 WorkerW".to_string(),
                ));
            }
        }

        // 换槽临界区：校验通过后才从 map 取出旧窗（P1-5）。A/B 阶段旧窗始终在 map 占位。
        let old_renderer = self.wallpapers.remove(display_id);
        let old_hwnd = old_renderer.as_ref().and_then(|r| r.hwnd());

        // 一次换槽（复用注册逻辑，不先关旧）
        self.register_renderer_maps(new_renderer, display_id, source, wallpaper_type);

        // 按当前全局 PauseReason 位图对新渲染器补发暂停（DR-20）
        self.apply_pause_reasons_to(display_id);

        // terminate 旧渲染器（失败强毁旧 hwnd 兜底）
        if let Some(mut old) = old_renderer {
            if let Err(e) = old.terminate() {
                tracing::warn!(error = %e, display_id, "原子交换：终止旧渲染器失败，强毁旧窗口兜底");
                if let Some(old_hwnd) = old_hwnd {
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(old_hwnd);
                    }
                }
            }
        }

        tracing::info!(display_id, "原子交换：换槽完成（old→new）");
        Ok(SwapOutcome::Committed)
    }

    /// 按当前全局暂停原因位图，对指定显示器的渲染器补发暂停命令（DR-20）。
    ///
    /// 原子交换提交新渲染器时，若全局处于暂停态（位图非空），新壁纸须跟随暂停。
    fn apply_pause_reasons_to(&self, display_id: &str) {
        let reasons = self.pause_reasons.lock().unwrap_or_else(|e| e.into_inner());
        if reasons.is_empty() {
            return;
        }
        if let Some(sender) = self.pause_senders.get(display_id) {
            if let Err(e) = sender.send(PauseCommand::Pause) {
                tracing::warn!(error = %e, display_id, "原子交换：对新渲染器补发暂停失败");
            }
        }
    }

    /// 关闭指定显示器上的壁纸（立即清除系统壁纸）。
    ///
    /// 同步清理 5 个 per-display_id HashMap：pause_senders / wallpaper_mode /
    /// wallpaper_sources / wallpapers / wallpaper_scaling_modes，确保关闭后无残留状态。
    ///
    /// 等价于 `close_wallpaper_with_clear(display_id, true)`——Native 壁纸关闭时
    /// 同步清除系统壁纸设置。需要"关闭但不立即清除"（延迟清除，用于轮换丝滑度
    /// 修复）时请使用私有实现 `close_wallpaper_with_clear(display_id, false)`。
    pub fn close_wallpaper(&mut self, display_id: &str) -> Result<(), MirrorStarError> {
        self.close_wallpaper_with_clear(display_id, true)
    }

    /// 关闭指定显示器上的壁纸，可控制是否清除系统壁纸。
    ///
    /// `clear_native=false` 用于"先关后设"延迟清除场景：关闭旧 Native 壁纸时保留
    /// 系统壁纸（旧画面保持显示），待新 WorkerW 窗嵌入后再由调用方补清除。
    /// 所有其他调用点传入 `true` 保持原语义。
    fn close_wallpaper_with_clear(
        &mut self,
        display_id: &str,
        clear_native: bool,
    ) -> Result<(), MirrorStarError> {
        // 移除快速控制发送端
        self.pause_senders.remove(display_id);
        // 移除壁纸模式记录
        let mode = self.wallpaper_mode.remove(display_id);
        self.wallpaper_sources.remove(display_id);
        // v13.0: 同步清理 wallpaper_scaling_modes，与上述 4 个 HashMap 保持一致
        //（避免 W-008 set_scaling_mode 失败回退时使用已关闭壁纸的残留 scaling mode）
        self.wallpaper_scaling_modes.remove(display_id);

        if let Some(WallpaperMode::Native) = mode {
            if clear_native {
                // 原生壁纸：清除系统壁纸设置
                native_wallpaper::clear_native_wallpaper()?;
                tracing::info!(display_id, "关闭原生壁纸");
            } else {
                // 延迟清除：保留系统壁纸，待新窗嵌入后补清除
                tracing::debug!(display_id, "关闭原生壁纸（延迟清除，待新窗嵌入后补清）");
            }
        } else if let Some(mut renderer) = self.wallpapers.remove(display_id) {
            // WorkerW 壁纸：尝试全部清理步骤，收集失败信息（不 fail-fast）
            let mut errors: Vec<String> = Vec::new();
            if let Err(e) = renderer.terminate() {
                errors.push(format!("terminate: {}", e));
            }
            match self.desktop.lock() {
                Ok(mut desktop) => {
                    if let Err(e) = desktop.remove_wallpaper(display_id) {
                        errors.push(format!("remove_wallpaper: {}", e));
                    }
                }
                Err(e) => errors.push(format!("desktop_lock: {}", e)),
            }
            if errors.is_empty() {
                tracing::info!(display_id, "关闭 WorkerW 壁纸");
            } else {
                tracing::warn!(display_id, errors = ?errors, "关闭 WorkerW 壁纸部分失败");
                return Err(MirrorStarError::DesktopIntegration(format!(
                    "close_wallpaper 部分失败: {}",
                    errors.join("; ")
                )));
            }
        }
        Ok(())
    }

    /// 按文件路径关闭正在运行的壁纸
    /// 用于删除壁纸时关闭其运行实例
    ///
    /// W13 修复：路径比较前进行规范化（lowercase + 统一分隔符为 `\`），
    /// 避免 `C:/a/b.mp4` 与 `c:\a\b.mp4` 因大小写/分隔符差异无法匹配。
    /// 不使用 `std::fs::canonicalize` 因其要求文件存在（删除场景文件可能已不存在）。
    pub fn close_wallpaper_by_path(&mut self, path: &str) -> Result<(), MirrorStarError> {
        let normalized_target = normalize_path_for_compare(path);
        // 查找使用该文件路径的显示器
        let display_id_to_close: Option<String> =
            self.wallpaper_sources
                .iter()
                .find_map(|(display_id, (source, _))| {
                    if let WallpaperSource::File(p) = source {
                        if normalize_path_for_compare(p) == normalized_target {
                            return Some(display_id.clone());
                        }
                    }
                    None
                });

        if let Some(display_id) = display_id_to_close {
            tracing::info!(path = %path, display_id = %display_id, "关闭正在运行的壁纸");
            self.close_wallpaper(&display_id)?;
        }
        Ok(())
    }

    // ── 纯净壁纸模式编排（D3 / D4 / D7 / D8）───────────────────────────────
    //
    // 设计要点：
    // - **权威状态**：`self.pure_mode` 为 `Some` 即"处于模式"，进入/退出据此幂等。
    // - **标志时序（D7）**：`pure_mode::set_active(true)` 必须在任何置顶动作**之前**
    //   置位（使并发的调度器/状态守卫立即开始抑制），`set_active(false)` 必须在
    //   全部回嵌**完成之后**清除（使任何读方观察到的"活跃"都对应一致窗口层级）。
    // - **失败回滚**：逐屏置顶，任一屏失败即把已摘出的窗口全部嵌回、清标志并返回
    //   `Err`，不留"半进入"状态。
    // - **零 resize（D1）**：摘出/置顶复用 `pure_mode` 原语，矩形保持进入前不变。

    /// 进入纯净壁纸模式：把正在播放的壁纸窗口摘出 WorkerW 并置顶全屏。
    ///
    /// 幂等：已处于模式（`self.pure_mode.is_some()`）时直接返回 `Ok(())`。
    ///
    /// # 前置校验（D8）
    ///
    /// `wallpaper_mode` 为空（当前没有任何显示器设置壁纸）→ 返回
    /// [`MirrorStarError::InvalidArgument`]，记录 warn，不进入模式。
    ///
    /// # 执行顺序（见 D3 / D4 / D7）
    ///
    /// 1. `pure_mode::set_active(true)`（**早于任何置顶**）
    /// 2. 遍历 `wallpapers`：`hwnd()` 为空的渲染器先 [`Self::revive_terminated`] 重建，
    ///    再 `DesktopIntegrator::detach_for_pure_mode` 摘出 + 置顶；**视频显示器例外**
    ///    （架构 D1）：不摘出，原窗口全程留在 WorkerW 继续播放，改由临时顶层渲染器覆盖
    /// 3. Native 静态图片（无窗口）→ 为"有 Native 记录且没有 WorkerW 壁纸"的显示器
    ///    创建临时 `ImageRenderer` 窗口并置顶（见 [`Self::enter_pure_mode_temp_windows`]）
    /// 4. 全部成功后：关闭鼠标穿透（D2）、注册置顶窗口表（供 TaskbarCreated 重断言）、
    ///    写入 `self.pure_mode`
    ///
    /// 任一步失败 → 回滚（嵌回已摘出窗口 + 终止已建临时渲染器 + 清标志）并返回原错误。
    pub fn enter_pure_mode(&mut self) -> Result<(), MirrorStarError> {
        // 幂等：已处于模式，无操作
        if self.pure_mode.is_some() {
            return Ok(());
        }

        // 前置校验：没有任何壁纸时拒绝进入（D8）
        if self.wallpaper_mode.is_empty() {
            tracing::warn!("纯净壁纸模式：当前没有任何壁纸，拒绝进入");
            return Err(MirrorStarError::InvalidArgument {
                reason: "当前没有任何壁纸，无法进入纯净壁纸模式".into(),
            });
        }

        // 标志必须早于置顶（D7）：置位后调度器/状态守卫立即开始抑制轮换与自动暂停
        crate::pure_mode::set_active(true);

        // 进入时快照排列方式：退出回嵌沿用（模式内用户不可改配置）
        let arrangement = self.arrangement();
        // 先收集 display_id，避免遍历 self.wallpapers 与后续 &mut self 调用借用冲突
        let display_ids: Vec<String> = self.wallpapers.keys().cloned().collect();

        let mut lifted: Vec<(String, HWND)> = Vec::new();
        for display_id in &display_ids {
            // 无窗口（被全屏终止 / 子进程崩溃）→ 先重建渲染器再摘出（D3）
            let no_hwnd = self
                .wallpapers
                .get(display_id)
                .map(|r| r.hwnd().is_none())
                .unwrap_or(false);
            // 仅在需要时复活；失败即整体失败（回滚已摘出的窗口）
            let revive_result = if no_hwnd {
                self.revive_terminated(display_id)
            } else {
                Ok(())
            };
            if let Err(e) = revive_result {
                tracing::error!(display_id, error = %e, "纯净壁纸模式：重建渲染器失败，回滚");
                self.rollback_pure_mode_enter(lifted, arrangement);
                return Err(e);
            }

            // 架构 D1：视频显示器**原窗口全程不搬动**（不摘出、不 SetParent /
            // SetWindowPos / ShowWindow）。实测"把视频窗口摘出 WorkerW 并置顶"会毒化
            // 本会话的 WorkerW 子层（之后该层不再产出可呈现帧，且退出侧的重建 / 回嵌 /
            // 窗口级唤醒手段均被证伪），而顶层窗口呈现正常（置顶期间画面在动）。故纯净
            // 模式的视频呈现改用**临时顶层渲染器覆盖**：原窗口留在 WorkerW 继续播放
            //（`wallpapers` / `wallpaper_sources` / `wallpaper_mode` / `active_wallpapers`
            // / `pause_senders` 条目全程不变），退出时销毁覆盖窗口即原样显露。
            if matches!(
                self.wallpaper_sources.get(display_id),
                Some((_, WallpaperType::Video))
            ) {
                // 目标矩形（复用 `pure_mode_target_rect`：Span 用虚拟屏整屏，否则取该
                // 显示器矩形）。临时窗口初始在 +32000 离屏，不能用 GetWindowRect 取目标矩形。
                let Some((x, y, w, h)) = self.pure_mode_target_rect(display_id, arrangement) else {
                    tracing::error!(
                        display_id,
                        "纯净壁纸模式：未找到显示器矩形，放弃临时视频覆盖（原窗口不受影响）"
                    );
                    continue;
                };
                // 配置快照复用既有无副作用的 `renderer_config()`（`prepare_set_wallpaper`
                // 亦用之），避免调用 prepare 本体引入关闭旧壁纸等副作用。
                let config = self.renderer_config();
                let scaling = self.scaling_mode_for(display_id);
                let Some(source) = self
                    .wallpaper_sources
                    .get(display_id)
                    .map(|(s, _)| s.clone())
                else {
                    continue;
                };
                // 顺序 play → lift_window_to_rect → after_embed：与 boot"窗口稳定嵌入后
                // loadfile"语义一致，避免离屏先加载的呈现竞态。全程 best-effort：失败即
                // 终止该临时渲染器并放弃本显示器覆盖（error!），不回滚整个进入流程。
                let mut renderer = match construct_renderer(
                    &source,
                    WallpaperType::Video,
                    scaling,
                    &config,
                    false,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(
                            display_id,
                            error = %e,
                            "纯净壁纸模式：创建临时视频覆盖渲染器失败（放弃本显示器覆盖，原窗口不受影响）"
                        );
                        continue;
                    }
                };
                let Some(hwnd) = renderer.hwnd() else {
                    tracing::error!(
                        display_id,
                        "纯净壁纸模式：临时视频覆盖渲染器无句柄（放弃本显示器覆盖）"
                    );
                    let _ = renderer.terminate();
                    continue;
                };
                // 与 embed_wallpaper Step 1 同源：去边框 + 去任务栏（NOACTIVATE/TOOLWINDOW）。
                // 覆盖窗是新 mpv 顶层窗口，默认带标题栏；不做这两步会在纯净模式里露出窗口边框。
                if let Err(e) = crate::desktop::window::make_borderless(hwnd) {
                    tracing::warn!(
                        display_id,
                        error = %e,
                        "纯净壁纸模式：覆盖窗去边框失败（仅美观受影响，继续置顶）"
                    );
                }
                crate::desktop::window::remove_from_taskbar(hwnd);
                // 根因（SubTask 2.12）：mpv 的 `auto-window-resize`（默认 yes）会在载入
                // 视频时按工作区（任务栏占 40px）与视频 16:9 自适配窗口，覆写应用设定的
                // 几何（实测 1920x1080 被改为 1848x1040@(36,20)）。实测在 loadfile **之前**
                // 设 `auto-window-resize=no` 可稳定保持 1920x1080（14/14 采样，画面非黑且在动、
                // 无黑边）；loadfile 之后再设无效。故必须在置顶后、`after_embed()`（loadfile）
                // 之前关闭该属性（best-effort，失败仅 warn 继续）。
                if let Err(e) = renderer.disable_auto_window_resize() {
                    tracing::warn!(
                        display_id,
                        error = %e,
                        "纯净壁纸模式：禁用 mpv 自动窗口适配失败（覆盖窗可能不严格全屏，继续）"
                    );
                }
                if let Err(e) = crate::pure_mode::lift_window_to_rect(hwnd, x, y, w, h) {
                    tracing::error!(
                        display_id,
                        error = %e,
                        "纯净壁纸模式：临时视频覆盖窗口置顶失败（放弃本显示器覆盖，原窗口不受影响）"
                    );
                    let _ = renderer.terminate();
                    continue;
                }
                if let Err(e) = renderer.after_embed() {
                    tracing::error!(
                        display_id,
                        error = %e,
                        "纯净壁纸模式：临时视频覆盖 loadfile 失败（放弃本显示器覆盖，原窗口不受影响）"
                    );
                    let _ = renderer.terminate();
                    continue;
                }
                tracing::info!(
                    display_id,
                    "进入纯净壁纸模式：视频改用临时渲染器覆盖（原窗口不搬动）"
                );
                self.pure_mode_temp.push((display_id.clone(), renderer));
                continue;
            }

            // 摘出 + 置顶（保持矩形不变，不隐藏）
            let detach_result = {
                let mut desktop = self.desktop.lock().unwrap_or_else(|e| e.into_inner());
                desktop.detach_for_pure_mode(display_id)
            };
            match detach_result {
                Ok(hwnd) => lifted.push((display_id.clone(), hwnd)),
                Err(e) => {
                    tracing::error!(display_id, error = %e, "纯净壁纸模式：摘出置顶窗口失败，回滚");
                    self.rollback_pure_mode_enter(lifted, arrangement);
                    return Err(e);
                }
            }
        }

        // Native 静态图片的临时呈现窗口（D4 / D8）
        if let Err(e) = self.enter_pure_mode_temp_windows(arrangement, &lifted) {
            self.rollback_pure_mode_enter(lifted, arrangement);
            return Err(e);
        }

        // F5：进入后必须至少覆盖一个显示器（摘出窗口或临时呈现窗口），否则为"空模式"——
        // 会置位标志、安装输入钩子并抑制轮换，却没有任何窗口被覆盖，必须回滚拒绝。
        // 注意：`rollback_pure_mode_enter` 以值接收 `lifted`（此分支 `return`，
        // 故后续成功路径仍可继续使用 `lifted`）。
        if lifted.is_empty() && self.pure_mode_temp.is_empty() {
            tracing::warn!(
                "纯净壁纸模式：没有可覆盖的壁纸窗口（无活跃窗口且无临时呈现窗口），拒绝进入并回滚"
            );
            self.rollback_pure_mode_enter(lifted, arrangement);
            return Err(MirrorStarError::InvalidArgument {
                reason: "没有可覆盖的壁纸窗口，拒绝进入纯净壁纸模式".into(),
            });
        }

        // 关闭鼠标穿透（D2）：置顶窗若保持 WS_EX_TRANSPARENT，点击会落到下层桌面
        for (display_id, _) in &lifted {
            if let Some(renderer) = self.wallpapers.get_mut(display_id) {
                renderer.set_mouse_passthrough(false);
            }
        }

        // 注册置顶窗口（覆盖式写入），供 TaskbarCreated 广播后重断言 Z 序
        let all_hwnds = self.collect_pure_mode_hwnds(&lifted);
        crate::pure_mode::register_lifted(&all_hwnds);

        let temp_count = self.pure_mode_temp.len();
        tracing::info!(
            displays = lifted.len(),
            temp_windows = temp_count,
            "已进入纯净壁纸模式（壁纸窗口已摘出并置顶全屏）"
        );
        self.pure_mode = Some(PureModeState {
            lifted,
            arrangement,
        });
        Ok(())
    }

    /// 为 Native 静态图片创建临时呈现窗口并置顶（`enter_pure_mode` 的第 3 步，D4 / D8）。
    ///
    /// 仅当 `wallpaper_mode` 中存在 `WallpaperMode::Native`（原生 API 静态图片，没有
    /// 窗口）时才创建工作；创建方式为 `ImageRenderer::new` + `play()`
    /// （**不**走 `construct_renderer`，避免其 `ensure_desktop_ready` 对 WorkerW 的
    /// 重试依赖与副作用）。
    ///
    /// # 覆盖范围（D4 / D8）
    ///
    /// - `arrangement == Span`：创建 **1 个**跨虚拟屏窗口（矩形取
    ///   [`crate::pure_mode::virtual_screen_rect`]）
    /// - 否则：逐屏（`desktop::enumerate_displays`）为"**有 Native 记录**且
    ///   **没有 WorkerW 壁纸窗口**"的显示器各创建一个窗口；既无 WorkerW 壁纸也无
    ///   Native 记录的显示器保持桌面原样（不覆盖）
    ///
    /// # 失败
    ///
    /// 任一临时窗口创建失败 → 终止本次失败的渲染器并把错误上抛给 `enter_pure_mode`
    /// （由其统一回滚，此时已创建成功的临时渲染器已在 `pure_mode_temp` 中，会被一并
    /// 终止）。已创建的临时渲染器存入 `self.pure_mode_temp`，**绝不**注册进权威状态
    /// 映射，也不写入 `lifted`（它们不需要回嵌，只需终止）。
    fn enter_pure_mode_temp_windows(
        &mut self,
        arrangement: Arrangement,
        lifted: &[(String, HWND)],
    ) -> Result<(), MirrorStarError> {
        /// 单个临时呈现窗口的创建目标（显示器ID + 图片路径 + 目标屏幕矩形）。
        struct TempWindowTarget {
            display_id: String,
            path: String,
            rect: (i32, i32, i32, i32),
        }

        // 收集 Native 静态图片记录：display_id -> 文件路径
        let native_records: Vec<(String, String)> = self
            .wallpaper_mode
            .iter()
            .filter(|(_, mode)| **mode == WallpaperMode::Native)
            .filter_map(|(display_id, _)| {
                match self.wallpaper_sources.get(display_id) {
                    Some((WallpaperSource::File(path), _)) => {
                        Some((display_id.clone(), path.clone()))
                    }
                    // URL 来源的 Native 记录不适用（原生壁纸只支持本地文件）
                    _ => None,
                }
            })
            .collect();
        if native_records.is_empty() {
            return Ok(());
        }
        // lifted 只作只读参考（判断"该显示器是否已有 WorkerW 壁纸窗口"）
        let lifted_ids: Vec<&str> = lifted.iter().map(|(id, _)| id.as_str()).collect();

        // 计算临时窗口目标（见局部结构 `TempWindowTarget`）。统一经
        // [`Self::pure_mode_target_rect`] 取矩形（与视频覆盖分支同一实现）。
        let mut targets: Vec<TempWindowTarget> = Vec::new();
        if arrangement == Arrangement::Span {
            // Span：一个跨虚拟屏的窗口即可铺满全部显示器
            let (native_display_id, path) = &native_records[0];
            let rect = self
                .pure_mode_target_rect(native_display_id, arrangement)
                .expect("Span 模式下 virtual_screen_rect 恒返回有效矩形");
            targets.push(TempWindowTarget {
                display_id: native_display_id.clone(),
                path: path.clone(),
                rect,
            });
        } else {
            for display in crate::desktop::enumerate_displays() {
                // 已有 WorkerW 壁纸窗口的显示器：其窗口已置顶，不再叠加
                if lifted_ids.contains(&display.id.as_str()) {
                    continue;
                }
                // 仅"有 Native 记录"的显示器补窗口（D8：两者皆无的显示器不覆盖）
                let Some((_, path)) = native_records
                    .iter()
                    .find(|(display_id, _)| *display_id == display.id)
                else {
                    continue;
                };
                let Some(rect) = self.pure_mode_target_rect(&display.id, arrangement) else {
                    continue;
                };
                targets.push(TempWindowTarget {
                    display_id: display.id.clone(),
                    path: path.clone(),
                    rect,
                });
            }
        }

        for target in targets {
            let TempWindowTarget {
                display_id,
                path,
                rect: (x, y, w, h),
            } = target;
            let mut renderer = ImageRenderer::new(path, self.scaling_mode_for(&display_id));
            if let Err(e) = renderer.play() {
                tracing::error!(display_id, error = %e, "纯净壁纸模式：临时呈现窗口播放失败");
                if let Err(te) = renderer.terminate() {
                    tracing::warn!(error = %te, "临时呈现窗口创建失败后终止渲染器也失败");
                }
                return Err(e);
            }
            let Some(hwnd) = renderer.hwnd() else {
                let _ = renderer.terminate();
                return Err(MirrorStarError::DesktopIntegration(format!(
                    "临时呈现窗口无句柄: {display_id}"
                )));
            };
            // 与 embed_wallpaper Step 1 同源：去边框 + 去任务栏（NOACTIVATE/TOOLWINDOW）。
            // 临时呈现窗是新顶层窗口，默认带标题栏；不做这两步会在纯净模式里露出窗口边框。
            if let Err(e) = crate::desktop::window::make_borderless(hwnd) {
                tracing::warn!(
                    display_id,
                    error = %e,
                    "纯净壁纸模式：临时呈现窗去边框失败（仅美观受影响，继续置顶）"
                );
            }
            crate::desktop::window::remove_from_taskbar(hwnd);
            if let Err(e) = crate::pure_mode::lift_window_to_rect(hwnd, x, y, w, h) {
                tracing::error!(display_id, error = %e, "纯净壁纸模式：临时呈现窗口置顶失败");
                if let Err(te) = renderer.terminate() {
                    tracing::warn!(error = %te, "临时呈现窗口置顶失败后终止渲染器也失败");
                }
                return Err(e);
            }
            self.pure_mode_temp.push((display_id, Box::new(renderer)));
        }
        Ok(())
    }

    /// 进入失败的统一回滚：嵌回已摘出窗口、终止已建临时渲染器、清标志。
    ///
    /// 逐个 best-effort（失败仅 warn，不阻断回滚），最后清除进程级标志，使失败的
    /// 进入操作不遗留"半进入"状态。**调用后必定丢弃 `lifted`**（以值接收），
    /// 保证不会被误用于后续成功路径。
    fn rollback_pure_mode_enter(&mut self, lifted: Vec<(String, HWND)>, arrangement: Arrangement) {
        for (display_id, hwnd) in lifted {
            let reattach_result = {
                let mut desktop = self.desktop.lock().unwrap_or_else(|e| e.into_inner());
                desktop.reattach_after_pure_mode(&display_id, hwnd, arrangement)
            };
            if let Err(e) = reattach_result {
                // F1b：回滚路径同样兜底——进入失败不得留下仍置顶遮挡桌面的窗口。
                // 复用共享 helper（仅对有效句柄隐藏，隐藏失败补 warn，随后记 error）。
                crate::desktop::hide_window_on_reattach_failure(
                    &display_id,
                    hwnd,
                    &e,
                    "纯净壁纸模式进入失败回滚：嵌回壁纸窗口失败",
                );
            }
        }
        for (display_id, mut renderer) in self.pure_mode_temp.drain(..) {
            if let Err(e) = renderer.terminate() {
                tracing::warn!(
                    display_id,
                    error = %e,
                    "纯净壁纸模式进入失败回滚：终止临时渲染器失败"
                );
            }
        }
        crate::pure_mode::set_active(false);
    }

    /// 汇总当前置顶窗口句柄（`lifted` + 临时呈现窗口），供
    /// [`crate::pure_mode::register_lifted`] 注册。
    fn collect_pure_mode_hwnds(&self, lifted: &[(String, HWND)]) -> Vec<HWND> {
        let mut hwnds: Vec<HWND> = lifted.iter().map(|(_, hwnd)| *hwnd).collect();
        for (_, renderer) in &self.pure_mode_temp {
            if let Some(hwnd) = renderer.hwnd() {
                hwnds.push(hwnd);
            }
        }
        hwnds
    }

    /// 计算纯净壁纸模式覆盖窗口的目标屏幕矩形（单一实现，供视频覆盖分支与临时呈现窗口复用）。
    ///
    /// - `arrangement == Span`：整个虚拟屏矩形（[`crate::pure_mode::virtual_screen_rect`]），
    ///   恒为 `Some`；
    /// - 否则：按 `display_id` 在 [`crate::desktop::enumerate_displays`] 中查该显示器矩形，
    ///   查不到返回 `None`（调用方据此放弃该显示器的覆盖）。
    fn pure_mode_target_rect(
        &self,
        display_id: &str,
        arrangement: Arrangement,
    ) -> Option<(i32, i32, i32, i32)> {
        if arrangement == Arrangement::Span {
            Some(crate::pure_mode::virtual_screen_rect())
        } else {
            crate::desktop::enumerate_displays()
                .into_iter()
                .find(|d| d.id == display_id)
                .map(|d| (d.x, d.y, d.width as i32, d.height as i32))
        }
    }

    /// 退出纯净壁纸模式：按进入时记录的排列把窗口嵌回 WorkerW 并恢复穿透设置。
    ///
    /// 幂等：`self.pure_mode` 为 `None` 时直接返回 `Ok(())`。
    ///
    /// # 执行顺序（D7）
    ///
    /// 1. 逐个把 `lifted` 的窗口交给 `DesktopIntegrator::reattach_after_pure_mode`
    ///    嵌回（best-effort，失败仅 warn 并继续，含 F1 隐藏兜底）
    /// 2. `terminate()` 并清空全部临时呈现渲染器（含架构 D1 的视频临时覆盖窗口——
    ///    销毁覆盖窗口后，WorkerW 中全程未被搬动的原视频窗口即原样显露）
    /// 3. 按 `interaction_mode` 还原鼠标穿透（`set_mouse_passthrough(!interaction_mode)`）
    /// 4. `clear_lifted()` → `set_active(false)`（**标志清除晚于回嵌完成**）
    pub fn exit_pure_mode(&mut self) -> Result<(), MirrorStarError> {
        let Some(state) = self.pure_mode.take() else {
            return Ok(());
        };
        let PureModeState {
            lifted,
            arrangement,
        } = state;

        // 1. 嵌回 WorkerW（沿用进入时的排列方式，D1 的几何由 embed_wallpaper 重新计算）
        for (display_id, hwnd) in &lifted {
            let reattach_result = {
                let mut desktop = self.desktop.lock().unwrap_or_else(|e| e.into_inner());
                desktop.reattach_after_pure_mode(display_id, *hwnd, arrangement)
            };
            if let Err(e) = reattach_result {
                // F1：嵌回失败即降级兜底。该窗口仍带 HWND_TOPMOST，若继续置顶会遮挡桌面，
                // 而此时模式标志与输入钩子均已撤除，用户没有任何输入退出路径——故必须
                // best-effort 隐藏它（复用共享 helper：仅对有效句柄操作，隐藏失败补 warn）。
                crate::desktop::hide_window_on_reattach_failure(
                    display_id,
                    *hwnd,
                    &e,
                    "退出纯净壁纸模式：嵌回 WorkerW 失败（下一次设置壁纸或重启应用后恢复）",
                );
            }
        }

        // 2. 销毁临时呈现渲染器（不注册进权威状态，直接终止）。
        // 架构 D1：视频显示器的原窗口全程留在 WorkerW、未被搬动（不进入 `lifted`），其
        // 纯净模式呈现由这里 drain 的"临时顶层覆盖渲染器"承担；销毁覆盖窗口后原视频窗口
        // 即原样显露并继续播放，无需回嵌/重建。
        for (display_id, mut renderer) in self.pure_mode_temp.drain(..) {
            if let Err(e) = renderer.terminate() {
                tracing::warn!(display_id, error = %e, "退出纯净壁纸模式：终止临时渲染器失败");
            }
        }

        // 3. 还原穿透：配置为穿透模式时恢复穿透，交互模式下保持非穿透
        let restore_passthrough = !self.interaction_mode;
        for (display_id, _) in &lifted {
            if let Some(renderer) = self.wallpapers.get_mut(display_id) {
                renderer.set_mouse_passthrough(restore_passthrough);
            }
        }

        // 4. 清标志（晚于回嵌完成，D7）
        crate::pure_mode::clear_lifted();
        crate::pure_mode::set_active(false);

        tracing::info!(displays = lifted.len(), "已退出纯净壁纸模式");
        Ok(())
    }

    /// 中止纯净壁纸模式（应用退出路径专用）。
    ///
    /// 与 [`Self::exit_pure_mode`] 的区别：**不做回嵌**。应用退出时已摘出的窗口随后
    /// 由 `cleanup_internal` 统一终止（对已摘出窗口的 `remove_wallpaper` 的
    /// `SetParent(None) + SW_HIDE` 幂等安全），此时再嵌回桌面纯属浪费且可能触发
    /// 一次多余的 Explorer 交互。
    ///
    /// 只做三件事：终止全部临时呈现渲染器、清空置顶窗口注册表、清除进程级标志；
    /// 每个失败仅 warn（退出路径不阻断）。
    pub fn abort_pure_mode(&mut self) {
        let had_state = self.pure_mode.take().is_some();
        let temp_count = self.pure_mode_temp.len();
        for (display_id, mut renderer) in self.pure_mode_temp.drain(..) {
            if let Err(e) = renderer.terminate() {
                tracing::warn!(display_id, error = %e, "中止纯净壁纸模式：终止临时渲染器失败");
            }
        }
        crate::pure_mode::clear_lifted();
        crate::pure_mode::set_active(false);
        if had_state {
            tracing::info!(
                temp_windows = temp_count,
                "已中止纯净壁纸模式（不做回嵌，窗口随后由退出清理统一终止）"
            );
        }
    }

    /// 查询当前是否处于纯净壁纸模式（供 src-tauri 编排层读取）。
    pub fn is_pure_mode_active(&self) -> bool {
        self.pure_mode.is_some()
    }

    /// 查询当前是否至少有一个显示器已设置壁纸（供编排层 / 测试钩子判断"是否可进入纯净壁纸模式"）。
    ///
    /// 语义：`wallpaper_mode` 非空即表示至少有一个显示器已应用壁纸——WorkerW 类型在窗口
    /// 嵌入时写入该映射（因此非空同时意味着窗口已就位），Native 类型在设置系统壁纸时写入
    /// （无需窗口）。纯净壁纸模式的前置校验与"开机应用完成后才进入"的探针都用本方法。
    pub fn has_any_wallpaper(&self) -> bool {
        !self.wallpaper_mode.is_empty()
    }

    /// 内部清理：关闭所有壁纸、恢复桌面、清除快速路径发送端
    ///
    /// `shutdown()` 和 `Drop` 都调用此方法，确保一致的清理行为。
    /// 使用 `close_wallpaper` 逐个关闭（包含 terminate + remove_wallpaper + clear_native），
    /// 比直接 drain 更完整（会调用 desktop.remove_wallpaper）。
    ///
    /// v13.0：在 `pause_senders.clear()` 后追加 `wallpaper_scaling_modes.clear()`，
    /// 两个 clear() 对称调用作为 shutdown 路径的 safety net（即使 close_wallpaper
    /// 逐个清理时遗漏，此处兜底确保无残留）。
    fn cleanup_internal(&mut self) {
        // 收集所有活跃壁纸的显示器ID（包括原生模式和 WorkerW 模式）
        let display_ids: Vec<String> = self.wallpaper_mode.keys().cloned().collect();
        // 统计失败的 display_id（best-effort 清理，不阻断退出）
        let mut failed_ids: Vec<&str> = Vec::new();
        for display_id in &display_ids {
            if let Err(e) = self.close_wallpaper(display_id) {
                tracing::error!(display_id, error = %e, "关闭壁纸失败");
                failed_ids.push(display_id);
            }
        }
        if !failed_ids.is_empty() {
            tracing::warn!(failed = ?failed_ids, count = failed_ids.len(), "cleanup 阶段部分壁纸关闭失败");
        }
        // 清除快速路径发送端
        self.pause_senders.clear();
        // v13.0: 与 pause_senders.clear() 对称，清空 scaling mode 记录
        self.wallpaper_scaling_modes.clear();
        // 恢复桌面：lock 成功时由 restore_original_wallpaper 处理（无原始壁纸时已自行 refresh），
        // lock 中毒时降级为 refresh_desktop 兜底。
        // D12: restore_original_wallpaper 返回 Result<(), MirrorStarError>，
        // 恢复失败时记录 warn 但允许继续退出（shutdown 路径不阻断）。
        match self.desktop.lock() {
            Ok(d) => {
                if let Err(e) = d.restore_original_wallpaper() {
                    tracing::warn!(error = %e, "恢复原始壁纸失败，继续退出");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "desktop lock 中毒，尝试 refresh_desktop 兜底");
                crate::desktop::worker_w::refresh_desktop();
            }
        }
    }

    /// 关闭所有壁纸并清理（用于应用退出）
    pub fn shutdown(&mut self) {
        // 退出路径先中止纯净壁纸模式：**不回嵌**（窗口随后由 cleanup_internal 统一
        // 终止，对已摘出窗口的 remove_wallpaper 的 SetParent(None)+SW_HIDE 幂等安全），
        // 只终止临时呈现渲染器并清除进程级标志 / 置顶窗口注册表。
        self.abort_pure_mode();
        self.cleanup_internal();
        tracing::info!("WallpaperEngine 已关闭所有壁纸");
    }
}

/// 构造并播放渲染器（公共逻辑，不含状态注册）
///
/// 封装四种壁纸类型的渲染器构造 + `play()` 调用，由以下两条路径复用以消除
/// 重复逻辑（原 `create_and_play_renderer` 与 `mode_dispatch.rs::set_wallpaper`
/// 各自维护一份高度相似的 match 分发）：
///
/// - `create_and_play_renderer`：异步路径（锁外执行），`clear_native = true`
/// - `mode_dispatch.rs::set_wallpaper`：同步路径（锁内执行），
///   `clear_native = self.is_native_mode(display_id)`
///
/// # 参数
///
/// - `source`：壁纸来源（文件路径或 URL）
/// - `wallpaper_type`：壁纸类型（穷尽 match，新增类型时编译器强制更新）
/// - `scaling_mode`：缩放模式
/// - `config`：渲染器配置快照（由 `WallpaperEngine::renderer_config()` 产生）
/// - `clear_native`：是否在创建 Video/Gif/Web 渲染器前清除原生壁纸。
///   - `true`：调用方确认需要清除（如异步路径无法判断历史模式，保守清除）
///   - `false`：调用方确认不需要清除（如同步路径已确认 `is_native_mode` 为 false）
///   - Image 类型不受此参数影响（WorkerW Image 不涉及原生壁纸清除）
///
/// # 返回
///
/// 返回已调用 `play()` 的渲染器（`Box<dyn WallpaperRenderer>`），调用方
/// 负责 subsequent 的 `embed_and_register_renderer` 状态注册。
pub fn construct_renderer(
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
    config: &RendererConfig,
    clear_native: bool,
) -> Result<Box<dyn WallpaperRenderer>, MirrorStarError> {
    match wallpaper_type {
        WallpaperType::Image => {
            if let WallpaperSource::File(path) = source {
                // Image WorkerW 分支不涉及原生壁纸清除（Native 分支由调用方处理）
                ensure_desktop_ready(&config.desktop)?;
                let mut renderer = ImageRenderer::new(path.clone(), scaling_mode);
                renderer.play()?;
                Ok(Box::new(renderer))
            } else {
                Err(MirrorStarError::DesktopIntegration(
                    "静态图片壁纸仅支持本地文件".to_string(),
                ))
            }
        }
        WallpaperType::Video => {
            if let WallpaperSource::File(path) = source {
                if clear_native {
                    if let Err(e) = native_wallpaper::clear_native_wallpaper() {
                        tracing::warn!(error = %e, "创建视频壁纸前清除原生壁纸失败");
                    }
                }
                ensure_desktop_ready(&config.desktop)?;
                let mut renderer = VideoRenderer::new(
                    path.clone(),
                    scaling_mode,
                    Some(config.volume_control.clone()),
                );
                renderer.play()?;
                Ok(Box::new(renderer))
            } else {
                Err(MirrorStarError::DesktopIntegration(
                    "视频壁纸仅支持本地文件".to_string(),
                ))
            }
        }
        WallpaperType::Gif => {
            if let WallpaperSource::File(path) = source {
                if clear_native {
                    if let Err(e) = native_wallpaper::clear_native_wallpaper() {
                        tracing::warn!(error = %e, "创建 GIF 壁纸前清除原生壁纸失败");
                    }
                }
                ensure_desktop_ready(&config.desktop)?;
                let mut renderer = GifRenderer::with_strategy(
                    path.clone(),
                    scaling_mode,
                    config.gif_memory_strategy,
                    config.gif_balanced_keep_frames,
                    config.gif_max_memory_mb,
                );
                renderer.play()?;
                Ok(Box::new(renderer))
            } else {
                Err(MirrorStarError::DesktopIntegration(
                    "GIF 壁纸仅支持本地文件".to_string(),
                ))
            }
        }
        WallpaperType::Web => {
            let source_str = match source {
                WallpaperSource::File(path) => path.clone(),
                WallpaperSource::Url(url) => url.clone(),
            };
            if clear_native {
                if let Err(e) = native_wallpaper::clear_native_wallpaper() {
                    tracing::warn!(error = %e, "创建网页壁纸前清除原生壁纸失败");
                }
            }
            ensure_desktop_ready(&config.desktop)?;

            // 冷启动路径：启动 wp-proc 子进程 + WebView2 初始化（典型 5-15s）
            let mut renderer = WebRenderer::new(source_str, scaling_mode);
            renderer.play()?;
            Ok(Box::new(renderer))
        }
    }
}

/// 在 engine 锁外创建并播放渲染器（Task 32: 减少锁持有时间）
///
/// 此函数执行耗时的操作：进程启动、IPC 连接、窗口创建等。
/// 调用方应在释放 engine 锁后调用此函数。
///
/// # 调用者
///
/// - `src-tauri/src/commands/wallpaper.rs::set_wallpaper`：WorkerW 模式下的
///   3 阶段流程（stage 1 `prepare_for_wallpaper` → stage 2 此函数 →
///   stage 3 `embed_and_register_renderer`）使用此函数在 `spawn_blocking`
///   线程上创建渲染器，避免阻塞 tokio runtime 与持有 engine 锁过久。
/// - `WallpaperEngine::set_wallpaper`（mode_dispatch.rs）：作为同步路径的
///   对照实现，通过 `construct_renderer` 复用渲染器构造逻辑。
///
/// 该函数不是 dead code，是与 `WallpaperEngine::set_wallpaper` 并行的
/// "锁外创建渲染器" 入口，供 Tauri 命令层的低锁竞争路径使用。
///
/// # 实现说明
///
/// 等价于 `construct_renderer(source, wallpaper_type, scaling_mode, config, true)`：
/// 异步路径在锁外执行，调用方无法可靠判断目标显示器的历史模式，保守清除原生壁纸
/// 以避免残留。同步路径（`mode_dispatch.rs::set_wallpaper`）通过
/// `is_native_mode` 精确判断后传 `clear_native` 参数。
pub fn create_and_play_renderer(
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
    config: &RendererConfig,
) -> Result<Box<dyn WallpaperRenderer>, MirrorStarError> {
    construct_renderer(source, wallpaper_type, scaling_mode, config, true)
}

/// 依据壁纸来源与类型派生渲染器唯一 id（DR-33 同目标短路用，纯函数）
///
/// 由 `AtomicSwapPending::old_renderer_id`（`prepare_atomic_swap` 在锁内从
/// `wallpaper_sources` 派生）与 `build_new_renderer` 各自计算，二者相等表示
/// "新图与旧图相同"，可短路跳过整个 swap。
pub fn renderer_id(source: &WallpaperSource, wallpaper_type: WallpaperType) -> String {
    let src = match source {
        WallpaperSource::File(p) => format!("file:{}", p),
        WallpaperSource::Url(u) => format!("url:{}", u),
    };
    format!("{:?}:{}", wallpaper_type, src)
}

/// 阶段 2（锁外）：构建新渲染器并 `play()`（DR-33 步骤 build_new）。
///
/// 复用 `construct_renderer`。失败返回 `Err`（旧壁纸原封不动，零回滚）。
/// 若新渲染器 id 与 `pending.old_renderer_id` 一致，返回 `BuildOutcome::Skip`
/// （短路径跳过整个 swap），不创建渲染器。
pub fn build_new_renderer(
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
    pending: &AtomicSwapPending,
) -> Result<BuildOutcome, MirrorStarError> {
    if pending.old_renderer_id.as_deref() == Some(renderer_id(source, wallpaper_type).as_str()) {
        tracing::info!("原子交换：新图与旧图相同，短路跳过 swap");
        return Ok(BuildOutcome::Skip);
    }
    let renderer = construct_renderer(
        source,
        wallpaper_type,
        scaling_mode,
        &pending.config,
        pending.clear_native,
    )?;
    Ok(BuildOutcome::Ready(renderer))
}

/// 阶段 3（锁外）：等待新渲染器首帧就绪（DR-33 步骤 wait_new_ready）。
///
/// - 视频：复用 [`VideoRenderer::poll_first_frame_ready`]（trait 覆写，内部轮询
///   mpv 至 `width>0 且 idle-active=false`）；每 `poll_interval` 查询一次。
/// - 图片 / GIF：覆写为轮询首次 WM_PAINT 绘制完成标志（`first_frame_ready`），
///   确保换槽前新窗首帧已绘制并可见。
/// - 网页：trait 默认实现立即返回就绪（wp-proc WebView2 渲染异步，无内容渲染
///   信号上报，见轮换丝滑度审计结论，已知限制）。
///
/// 失败或超时返回 `Err`（旧壁纸照常显示，零回滚），由调用方 `terminate` 新渲染器。
pub fn wait_new_ready(
    renderer: &mut Box<dyn WallpaperRenderer>,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), MirrorStarError> {
    if renderer.poll_first_frame_ready()? {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    loop {
        std::thread::sleep(poll_interval);
        if renderer.poll_first_frame_ready()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(MirrorStarError::DesktopIntegration(
                "原子交换：新渲染器首帧就绪等待超时".to_string(),
            ));
        }
    }
}

/// 规范化路径用于比较（W13 修复）
///
/// 将路径转换为小写并将 `/` 替换为 `\\`，使 `C:/a/b.mp4` 与 `c:\a\b.mp4` 视为相等。
/// 不使用 `std::fs::canonicalize` 因其要求文件存在（删除场景文件可能已不存在）。
fn normalize_path_for_compare(p: &str) -> String {
    p.to_lowercase().replace('/', "\\")
}

/// 确保 WorkerW 就绪（释放 desktop 锁后 sleep 重试）
///
/// 适用场景：锁外创建渲染器（`construct_renderer`）时调用，无法访问 `&self`。
/// 持有 engine 引用时改用 `WallpaperEngine::ensure_desktop_ready_with_retry`。
///
/// 委托给 `ensure_desktop_ready_impl`，与方法版本共享同一实现。
fn ensure_desktop_ready(desktop: &Arc<Mutex<DesktopIntegrator>>) -> Result<(), MirrorStarError> {
    ensure_desktop_ready_impl(desktop)
}

/// 确保 WorkerW 就绪的公共实现（W05：消除重复逻辑）
///
/// 首次尝试（持锁但 `check_and_reinitialize` 内部不 sleep），失败后释放锁并
/// 按递增间隔重试 6 次（100ms 起步，每次递增 50ms，总等待 1350ms），避免持锁 sleep
/// 阻塞其他 desktop 访问者。
///
/// v5.0 D-PERF-007: 重试次数从 10 减至 6，首次等待从 200ms 减至 100ms，
/// 总等待从 4250ms 降至 1350ms。D-PERF-002（Progman 直查）使首次成功率极高，
/// 重试仅在罕见场景触发，6 次足够等待 WorkerW 创建。
///
/// 由以下两处调用：
/// - `WallpaperEngine::ensure_desktop_ready_with_retry`（方法版本，访问 `self.desktop`）
/// - `ensure_desktop_ready`（自由函数版本，由 `construct_renderer` 在锁外调用）
fn ensure_desktop_ready_impl(
    desktop: &Arc<Mutex<DesktopIntegrator>>,
) -> Result<(), MirrorStarError> {
    // 首次尝试（持有锁，但 check_and_reinitialize 内部已不 sleep，仅单次查找）
    let first_result = {
        let mut desktop = desktop.lock().map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
        })?;
        // check_and_reinitialize 返回 bool 表示是否实际重初始化，此处不关心
        desktop.check_and_reinitialize()
    };
    if first_result.is_ok() {
        return Ok(());
    }
    let first_err = first_result.unwrap_err();
    tracing::warn!(error = %first_err, "首次查找 WorkerW 失败，开始重试（释放 desktop 锁）...");

    // v5.0 D-PERF-007: 重试 6 次（原 10 次），总等待 1350ms（原 4250ms）
    use crate::desktop::worker_w::compute_retry_wait_ms;
    for i in 0..6u32 {
        let wait_ms = compute_retry_wait_ms(i);
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        let retry_result = {
            let mut desktop = desktop.lock().map_err(|e| {
                MirrorStarError::DesktopIntegration(format!("获取桌面集成器锁失败: {}", e))
            })?;
            // check_and_reinitialize 返回 bool 表示是否实际重初始化，此处不关心
            desktop.check_and_reinitialize()
        };
        match retry_result {
            Ok(_did_init) => {
                tracing::info!(attempt = i + 1, "重试成功，WorkerW 已找到");
                return Ok(());
            }
            Err(_) => {
                tracing::debug!(attempt = i + 1, max = 6, "等待 WorkerW 创建...");
            }
        }
    }
    tracing::error!(error = %first_err, "重试 6 次后仍未找到 WorkerW");
    Err(first_err)
}

impl Drop for WallpaperEngine {
    fn drop(&mut self) {
        self.cleanup_internal();
        tracing::info!("WallpaperEngine 已清理");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::volume::VolumeControl;
    use crate::config::settings::Arrangement;
    use crate::desktop::DesktopIntegrator;
    use crate::wallpaper::{create_pause_channel, PauseSender, WallpaperState};
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::HWND;

    // ── 原子交换同目标短路：renderer_id（DR-33 纯函数） ────────────────────

    #[test]
    fn renderer_id_deterministic_and_same_source_same_id() {
        let vid_a = WallpaperType::Video;
        let src_a = WallpaperSource::File("C:/w/cat.mp4".to_string());
        // 相同来源+类型 → 相同 id（可从 wallpaper_sources 派生并稳定比较）
        assert_eq!(renderer_id(&src_a, vid_a), renderer_id(&src_a, vid_a));
        // 不同类型 → 不同 id
        let gif_same_src = renderer_id(&src_a, WallpaperType::Gif);
        assert_ne!(renderer_id(&src_a, vid_a), gif_same_src);
        // 不同来源 → 不同 id
        let src_b = WallpaperSource::File("C:/w/dog.mp4".to_string());
        assert_ne!(renderer_id(&src_a, vid_a), renderer_id(&src_b, vid_a));
        // Url 来源也稳定
        let url = WallpaperSource::Url("https://x/x.mp4".to_string());
        assert_eq!(
            renderer_id(&url, WallpaperType::Web),
            renderer_id(&url, WallpaperType::Web)
        );
    }

    // ── 统一 display-id 解析（单一真相源，纯函数分支） ─────────────────────

    #[test]
    fn resolve_display_id_from_explicit_takes_priority() {
        // ① 显式非空优先返回，忽略活跃/主屏
        assert_eq!(
            resolve_display_id_from(
                Some(r"\\.\DISPLAY2".to_string()),
                Some(r"\\.\DISPLAY1"),
                Some(r"\\.\DISPLAY1"),
            ),
            r"\\.\DISPLAY2"
        );
    }

    #[test]
    fn resolve_display_id_from_falls_back_to_first_active() {
        // ② 显式空/None + 有活跃壁纸 → 返回活跃键
        assert_eq!(
            resolve_display_id_from(None, Some(r"\\.\DISPLAY1"), Some(r"\\.\DISPLAY9")),
            r"\\.\DISPLAY1"
        );
        assert_eq!(
            resolve_display_id_from(Some(String::new()), Some(r"\\.\DISPLAY1"), None),
            r"\\.\DISPLAY1"
        );
    }

    #[test]
    fn resolve_display_id_from_falls_back_to_primary() {
        // ③ 显式空/None + 无活跃壁纸但有主屏 → 返回主屏 id
        assert_eq!(
            resolve_display_id_from(None, None, Some(r"\\.\DISPLAY1")),
            r"\\.\DISPLAY1"
        );
        assert_eq!(
            resolve_display_id_from(Some(String::new()), Some(""), Some(r"\\.\DISPLAY1")),
            r"\\.\DISPLAY1"
        );
    }

    #[test]
    fn resolve_display_id_from_empty_when_all_missing() {
        // ④ 三者皆无 → 返回空串且不 panic（真实环境无显示器时的确定性行为）
        assert_eq!(resolve_display_id_from(None, None, None), String::new());
        assert_eq!(
            resolve_display_id_from(Some(String::new()), Some(""), Some("")),
            String::new()
        );
    }

    // ── 测试辅助 ──────────────────────────────────────────────────────────

    /// Mock 渲染器共享状态：记录渲染器接收到的所有调用
    struct MockRendererState {
        scaling_mode_calls: Vec<ScalingMode>,
        speed_calls: Vec<f32>,
        interaction_mode_calls: Vec<bool>,
        passthrough_calls: Vec<bool>,
        position_calls: Vec<(i32, i32, i32, i32)>,
        terminated: bool,
        played: bool,
        paused: bool,
        resumed: bool,
        state: WallpaperState,
        /// 存活探针结果队列：每次 `liveness_probe()` 弹出队首（空则返回 `None`）。
        ///
        /// 用预填队列而非固定返回值，便于测试按调用顺序区分多次采样（如看护/自愈场景
        /// 需要"第一次 Some、第二次 None"这类序列）。
        probe_results: std::collections::VecDeque<Option<LivenessProbe>>,
        /// 存活探针被调用的次数（用于断言采样确实发生）。
        probe_calls: usize,
    }

    impl MockRendererState {
        fn new() -> Self {
            Self {
                scaling_mode_calls: Vec::new(),
                speed_calls: Vec::new(),
                interaction_mode_calls: Vec::new(),
                passthrough_calls: Vec::new(),
                position_calls: Vec::new(),
                terminated: false,
                played: false,
                paused: false,
                resumed: false,
                state: WallpaperState::Initializing,
                probe_results: std::collections::VecDeque::new(),
                probe_calls: 0,
            }
        }
    }

    /// Mock 渲染器，用于测试 WallpaperEngine 的状态管理与渲染器委托逻辑
    ///
    /// 与 `mode_dispatch::tests::MockRenderer` 类似，`hwnd()` 返回 `None` 以跳过
    /// WorkerW 嵌入逻辑，使 `embed_and_register_renderer` 可在无 Win32 桌面环境的
    /// 情况下完成状态注册。额外通过 `Arc<Mutex<MockRendererState>>` 记录调用，
    /// 供测试断言渲染器是否收到正确的命令。
    struct MockRenderer {
        shared: Arc<Mutex<MockRendererState>>,
        pause_sender: Option<PauseSender>,
    }

    impl MockRenderer {
        /// 创建 MockRenderer，返回渲染器与共享状态句柄
        fn new() -> (Self, Arc<Mutex<MockRendererState>>) {
            let (sender, _rx, _shared) = create_pause_channel();
            let shared = Arc::new(Mutex::new(MockRendererState::new()));
            let renderer = Self {
                shared: shared.clone(),
                pause_sender: Some(sender),
            };
            (renderer, shared)
        }
    }

    impl WallpaperRenderer for MockRenderer {
        fn play(&mut self) -> Result<(), MirrorStarError> {
            let mut s = self.shared.lock().unwrap();
            s.played = true;
            s.state = WallpaperState::Playing;
            Ok(())
        }
        fn pause(&mut self) -> Result<(), MirrorStarError> {
            let mut s = self.shared.lock().unwrap();
            s.paused = true;
            s.state = WallpaperState::Paused;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), MirrorStarError> {
            let mut s = self.shared.lock().unwrap();
            s.resumed = true;
            s.state = WallpaperState::Playing;
            Ok(())
        }
        fn set_position(&mut self, x: i32, y: i32, w: i32, h: i32) -> Result<(), MirrorStarError> {
            self.shared
                .lock()
                .unwrap()
                .position_calls
                .push((x, y, w, h));
            Ok(())
        }
        fn terminate(&mut self) -> Result<(), MirrorStarError> {
            let mut s = self.shared.lock().unwrap();
            s.terminated = true;
            s.state = WallpaperState::Terminated;
            Ok(())
        }
        fn hwnd(&self) -> Option<HWND> {
            None // 返回 None 跳过 WorkerW 嵌入
        }
        fn state(&self) -> WallpaperState {
            self.shared.lock().unwrap().state
        }
        fn set_speed(&mut self, speed: f32) {
            self.shared.lock().unwrap().speed_calls.push(speed);
        }
        fn set_scaling_mode(&mut self, mode: ScalingMode) {
            self.shared.lock().unwrap().scaling_mode_calls.push(mode);
        }
        fn set_mouse_passthrough(&mut self, enabled: bool) {
            self.shared.lock().unwrap().passthrough_calls.push(enabled);
        }
        fn set_interaction_mode(&mut self, enabled: bool) {
            self.shared
                .lock()
                .unwrap()
                .interaction_mode_calls
                .push(enabled);
        }
        fn create_pause_sender(&mut self, _display_id: &str) -> Option<PauseSender> {
            self.pause_sender.take()
        }
        fn liveness_probe(&mut self) -> Option<LivenessProbe> {
            let mut s = self.shared.lock().unwrap();
            s.probe_calls += 1;
            // 弹出队首；队列空即返回 None（模拟"该渲染器不支持/无探针"）。
            s.probe_results.pop_front().unwrap_or(None)
        }
    }

    /// 创建测试用 WallpaperEngine（不依赖 COM/音频设备）
    ///
    /// 使用 `VolumeControl::new_disabled()` 避免 COM/WASAPI 依赖，使测试可在
    /// 任意 Windows 环境（含无音频 CI）运行。`DesktopIntegrator::new()` 仅读取
    /// 系统壁纸路径（只读 Win32），与 `desktop/mod.rs` 的测试一致，安全。
    fn create_test_engine() -> WallpaperEngine {
        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume_control = Arc::new(Mutex::new(VolumeControl::new_disabled()));
        WallpaperEngine::new(desktop, volume_control)
    }

    // ========== WallpaperEngine::new 默认状态测试 ==========

    #[test]
    fn engine_new_default_state() {
        // 验证 new() 设置的默认值
        let engine = create_test_engine();
        assert!(engine.wallpapers.is_empty(), "wallpapers 应为空");
        assert!(engine.wallpaper_mode.is_empty(), "wallpaper_mode 应为空");
        assert!(
            engine.wallpaper_sources.is_empty(),
            "wallpaper_sources 应为空"
        );
        assert!(engine.pause_senders.is_empty(), "pause_senders 应为空");
        assert_eq!(
            engine.arrangement,
            Arrangement::PerMonitor,
            "默认排列方式应为 PerMonitor"
        );
        assert!(
            !engine.interaction_mode,
            "默认 interaction_mode 应为 false（穿透模式）"
        );
        assert_eq!(
            engine.gif_memory_strategy(),
            GifMemoryStrategy::default(),
            "GIF 策略应为默认值"
        );
        assert_eq!(
            engine.gif_balanced_keep_frames(),
            DEFAULT_BALANCED_KEEP_FRAMES,
            "GIF 保留帧数应为默认值"
        );
    }

    // ========== set_gif_memory_strategy 测试 ==========

    #[test]
    fn set_gif_memory_strategy_updates_fields() {
        // 验证策略与保留帧数同步更新
        // set_gif_memory_strategy 改为 &self，不再需要 mut
        let engine = create_test_engine();
        engine.set_gif_memory_strategy(GifMemoryStrategy::Aggressive, 5, 60);
        assert_eq!(engine.gif_memory_strategy(), GifMemoryStrategy::Aggressive);
        assert_eq!(engine.gif_balanced_keep_frames(), 5);
    }

    #[test]
    fn set_gif_memory_strategy_zero_keep_frames() {
        // 边界值：0 帧保留（Aggressive 模式典型配置）
        // set_gif_memory_strategy 改为 &self，不再需要 mut
        let engine = create_test_engine();
        engine.set_gif_memory_strategy(GifMemoryStrategy::Aggressive, 0, 40);
        assert_eq!(engine.gif_balanced_keep_frames(), 0);
    }

    // ========== set_arrangement 测试 ==========

    #[test]
    fn set_arrangement_updates_field() {
        let mut engine = create_test_engine();
        assert_eq!(engine.arrangement, Arrangement::PerMonitor);
        engine.set_arrangement(Arrangement::Span);
        assert_eq!(engine.arrangement, Arrangement::Span);
    }

    // ========== is_native_mode 测试 ==========

    #[test]
    fn is_native_mode_no_entry_returns_false() {
        // 未设置任何模式的显示器应返回 false
        let engine = create_test_engine();
        assert!(!engine.is_native_mode("monitor_0"));
        assert!(!engine.is_native_mode(""));
    }

    #[test]
    fn is_native_mode_native_entry_returns_true() {
        // Native 模式记录应返回 true，其他显示器仍返回 false
        let mut engine = create_test_engine();
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::Native);
        assert!(engine.is_native_mode("monitor_0"));
        assert!(!engine.is_native_mode("monitor_1"));
        // 清理：避免 Drop 调用 clear_native_wallpaper（Win32 状态变更）
        engine.wallpaper_mode.clear();
    }

    #[test]
    fn is_native_mode_workerw_entry_returns_false() {
        // WorkerW 模式不应被识别为 Native
        let mut engine = create_test_engine();
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::WorkerW);
        assert!(!engine.is_native_mode("monitor_0"));
    }

    // ========== has_wallpaper 测试 ==========

    #[test]
    fn has_wallpaper_empty_returns_false() {
        let engine = create_test_engine();
        assert!(!engine.has_wallpaper("monitor_0"));
    }

    #[test]
    fn has_wallpaper_with_mode_only_returns_true() {
        // Native 模式下 wallpapers 为空但 wallpaper_mode 有记录
        let mut engine = create_test_engine();
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::Native);
        assert!(engine.has_wallpaper("monitor_0"));
        // 清理：避免 Drop 调用 clear_native_wallpaper
        engine.wallpaper_mode.clear();
    }

    #[test]
    fn has_wallpaper_with_renderer_returns_true() {
        // WorkerW 模式注册渲染器后应返回 true
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();
        assert!(engine.has_wallpaper("monitor_0"));
    }

    // ========== renderer_config 测试 ==========

    #[test]
    fn renderer_config_returns_snapshot() {
        // 验证配置快照与引擎当前状态一致
        let mut engine = create_test_engine();
        engine.set_gif_memory_strategy(GifMemoryStrategy::Performance, 20, 80);
        engine.set_arrangement(Arrangement::Span);
        let config = engine.renderer_config();
        assert_eq!(config.arrangement, Arrangement::Span);
        assert_eq!(config.gif_memory_strategy, GifMemoryStrategy::Performance);
        assert_eq!(config.gif_balanced_keep_frames, 20);
        assert_eq!(config.gif_max_memory_mb, 80);
        // 验证 Arc 引用同一对象（快照应共享底层数据）
        assert!(Arc::ptr_eq(&config.desktop, &engine.desktop));
        assert!(Arc::ptr_eq(&config.volume_control, &engine.volume_control));
    }

    #[test]
    fn renderer_config_default_values() {
        // 未修改任何配置时，快照应反映默认值
        let engine = create_test_engine();
        let config = engine.renderer_config();
        assert_eq!(config.arrangement, Arrangement::PerMonitor);
        assert_eq!(config.gif_memory_strategy, GifMemoryStrategy::default());
        assert_eq!(
            config.gif_balanced_keep_frames,
            DEFAULT_BALANCED_KEEP_FRAMES
        );
        assert_eq!(
            config.gif_max_memory_mb,
            crate::wallpaper::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB
        );
    }

    // ========== set_scaling_mode 测试 ==========

    #[test]
    fn set_scaling_mode_no_entry_returns_ok() {
        // 未注册任何壁纸时调用应返回 Ok（no-op）
        let mut engine = create_test_engine();
        let result = engine.set_scaling_mode("nonexistent", ScalingMode::Fit);
        assert!(result.is_ok());
    }

    #[test]
    fn set_scaling_mode_native_no_source_returns_ok() {
        // Native 模式但 wallpaper_sources 无记录：跳过 set_native_wallpaper
        // 这测试了 Native 模式重设壁纸路径的边界情况（mode 有记录但 source 丢失）
        let mut engine = create_test_engine();
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::Native);
        // 不插入 wallpaper_sources，使 if let 不匹配
        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_ok());
        // 清理：避免 Drop 调用 clear_native_wallpaper
        engine.wallpaper_mode.clear();
    }

    #[test]
    fn set_scaling_mode_workerw_gif_calls_renderer() {
        // WorkerW + Gif 类型：应直接调用 renderer.set_scaling_mode
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.gif".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Gif,
            )
            .unwrap();

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_ok());

        // 验证渲染器收到了 set_scaling_mode 调用
        let state = shared.lock().unwrap();
        assert_eq!(state.scaling_mode_calls, vec![ScalingMode::Fit]);
    }

    #[test]
    fn set_scaling_mode_workerw_image_calls_renderer() {
        // WorkerW + Image 类型（如 WebP 格式）：同样直接调用 renderer
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.webp".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Image,
            )
            .unwrap();

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Stretch);
        assert!(result.is_ok());

        let state = shared.lock().unwrap();
        assert_eq!(state.scaling_mode_calls, vec![ScalingMode::Stretch]);
    }

    #[test]
    fn set_scaling_mode_workerw_no_renderer_returns_ok() {
        // WorkerW 模式有 source 记录（非 Video）但 wallpapers 无渲染器
        // 这是一致性边界场景：source 存在但渲染器已丢失
        let mut engine = create_test_engine();
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::WorkerW);
        engine.wallpaper_sources.insert(
            "monitor_0".to_string(),
            (
                WallpaperSource::File("/test.gif".to_string()),
                WallpaperType::Gif,
            ),
        );
        // 不插入 wallpapers 渲染器

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_ok());
    }

    // ========== seed_scaling_modes / scaling_mode_for 兜底 测试 ==========

    #[test]
    fn seed_scaling_modes_scaling_mode_for_returns_config_value() {
        // 种子化后 scaling_mode_for 返回配置值（配置为持久化真相源）
        let mut engine = create_test_engine();
        let mut modes = BTreeMap::new();
        modes.insert(r"\\.\DISPLAY1".to_string(), ScalingMode::Tile);
        modes.insert(r"\\.\DISPLAY2".to_string(), ScalingMode::Fit);
        engine.seed_scaling_modes(&modes);

        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY1"), ScalingMode::Tile);
        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY2"), ScalingMode::Fit);
        // 无精确项且无 "all" 兜底 → 默认 Fill
        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY9"), ScalingMode::Fill);
    }

    #[test]
    fn seed_scaling_modes_overwrites_existing_runtime_value() {
        // 同 key 时以配置值覆盖运行时残留值
        let mut engine = create_test_engine();
        engine
            .wallpaper_scaling_modes
            .insert(r"\\.\DISPLAY1".to_string(), ScalingMode::Center);
        let mut modes = BTreeMap::new();
        modes.insert(r"\\.\DISPLAY1".to_string(), ScalingMode::Stretch);
        engine.seed_scaling_modes(&modes);

        assert_eq!(
            engine.scaling_mode_for(r"\\.\DISPLAY1"),
            ScalingMode::Stretch
        );
    }

    #[test]
    fn scaling_mode_for_all_fallback_used_when_no_exact_match() {
        // "all" 兜底：无精确项时对所有显示器生效；精确项优先
        let mut engine = create_test_engine();
        let mut modes = BTreeMap::new();
        modes.insert("all".to_string(), ScalingMode::Tile);
        modes.insert(r"\\.\DISPLAY1".to_string(), ScalingMode::Fit);
        engine.seed_scaling_modes(&modes);

        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY1"), ScalingMode::Fit);
        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY2"), ScalingMode::Tile);
        assert_eq!(engine.scaling_mode_for("unknown"), ScalingMode::Tile);
    }

    #[test]
    fn scaling_mode_for_defaults_to_fill_when_empty() {
        // 既无精确项也无 "all" → 默认 Fill（与既有默认行为一致）
        let engine = create_test_engine();
        assert_eq!(engine.scaling_mode_for(r"\\.\DISPLAY1"), ScalingMode::Fill);
    }

    // ========== set_scaling_mode_for_all（"all" 广播）测试 ==========

    #[test]
    fn set_scaling_mode_for_all_applies_to_every_active_display() {
        // "all" 广播应作用于全部活跃显示器（此处用 Gif 走 renderer 直调路径）
        let mut engine = create_test_engine();
        let (r1, s1) = MockRenderer::new();
        let (r2, s2) = MockRenderer::new();
        let src1 = WallpaperSource::File("/a.gif".to_string());
        let src2 = WallpaperSource::File("/b.gif".to_string());
        engine
            .embed_and_register_renderer(Box::new(r1), r"\\.\DISPLAY1", &src1, WallpaperType::Gif)
            .unwrap();
        engine
            .embed_and_register_renderer(Box::new(r2), r"\\.\DISPLAY2", &src2, WallpaperType::Gif)
            .unwrap();

        engine.set_scaling_mode_for_all(ScalingMode::Tile).unwrap();

        assert_eq!(
            s1.lock().unwrap().scaling_mode_calls,
            vec![ScalingMode::Tile]
        );
        assert_eq!(
            s2.lock().unwrap().scaling_mode_calls,
            vec![ScalingMode::Tile]
        );
    }

    #[test]
    fn set_scaling_mode_for_all_no_active_displays_returns_ok() {
        // 无活跃显示器时为 no-op 返回 Ok（不 panic）
        let mut engine = create_test_engine();
        assert!(engine.set_scaling_mode_for_all(ScalingMode::Fit).is_ok());
    }

    // ========== set_speed 测试 ==========

    #[tokio::test]
    async fn set_speed_with_renderer_calls_renderer() {
        // 有渲染器时应调用 renderer.set_speed
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.set_speed("monitor_0", 1.5).await;
        assert!(result.is_ok());

        let state = shared.lock().unwrap();
        assert_eq!(state.speed_calls, vec![1.5]);
    }

    #[tokio::test]
    async fn set_speed_no_renderer_returns_ok() {
        // 无渲染器时应返回 Ok（no-op）
        let mut engine = create_test_engine();
        let result = engine.set_speed("nonexistent", 2.0).await;
        assert!(result.is_ok());
    }

    // ========== set_interaction_mode 测试 ==========

    #[test]
    fn set_interaction_mode_true_updates_engine_and_renderer() {
        // 启用交互模式：engine.interaction_mode = true，渲染器收到 enabled=true + passthrough=false
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.gif".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Gif,
            )
            .unwrap();

        let result = engine.set_interaction_mode(true);
        assert!(result.is_ok());
        assert!(engine.interaction_mode);

        let state = shared.lock().unwrap();
        assert_eq!(state.interaction_mode_calls, vec![true]);
        assert_eq!(state.passthrough_calls, vec![false]); // !true = false
    }

    #[test]
    fn set_interaction_mode_false_updates_engine_and_renderer() {
        // 禁用交互模式：engine.interaction_mode = false，渲染器收到 enabled=false + passthrough=true
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.gif".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Gif,
            )
            .unwrap();

        let result = engine.set_interaction_mode(false);
        assert!(result.is_ok());
        assert!(!engine.interaction_mode);

        let state = shared.lock().unwrap();
        assert_eq!(state.interaction_mode_calls, vec![false]);
        assert_eq!(state.passthrough_calls, vec![true]); // !false = true
    }

    #[test]
    fn set_interaction_mode_no_renderer_returns_ok() {
        // 无渲染器时应返回 Ok，仅更新 engine 状态
        let mut engine = create_test_engine();
        let result = engine.set_interaction_mode(true);
        assert!(result.is_ok());
        assert!(engine.interaction_mode);
    }

    #[test]
    fn set_interaction_mode_skipped_while_pure_mode_active() {
        // B2：纯净模式活跃时，set_interaction_mode 仅更新字段（供退出时还原），
        // 跳过对已置顶窗口的交互/穿透切换（D2：模式内强制关闭穿透，不得重开）。
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.gif".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Gif,
            )
            .unwrap();
        // 模拟"已进入纯净模式"的权威状态（此处仅需 Some，不涉及真实窗口）。
        engine.interaction_mode = true;
        engine.pure_mode = Some(PureModeState {
            lifted: Vec::new(),
            arrangement: engine.arrangement(),
        });

        let result = engine.set_interaction_mode(false);
        assert!(result.is_ok());
        assert!(!engine.interaction_mode, "模式内仍应更新字段以供退出时还原");

        let state = shared.lock().unwrap();
        assert!(
            state.interaction_mode_calls.is_empty(),
            "纯净模式内不应切换渲染器交互模式"
        );
        assert!(
            state.passthrough_calls.is_empty(),
            "纯净模式内不应重开鼠标穿透（D2）"
        );
    }

    // ========== toggle_interaction 测试 ==========

    #[test]
    fn toggle_interaction_false_to_true() {
        // 默认 false → 切换为 true，返回 true
        let mut engine = create_test_engine();
        assert!(!engine.interaction_mode);
        let result = engine.toggle_interaction();
        assert!(result.is_ok());
        assert!(result.unwrap());
        assert!(engine.interaction_mode);
    }

    #[test]
    fn toggle_interaction_true_to_false() {
        // 先启用再切换 → 返回 false
        let mut engine = create_test_engine();
        engine.set_interaction_mode(true).unwrap();
        let result = engine.toggle_interaction();
        assert!(result.is_ok());
        assert!(!result.unwrap());
        assert!(!engine.interaction_mode);
    }

    #[test]
    fn toggle_interaction_twice_returns_to_original() {
        // 两次切换应回到原始状态
        let mut engine = create_test_engine();
        let first = engine.toggle_interaction().unwrap();
        let second = engine.toggle_interaction().unwrap();
        assert!(first);
        assert!(!second);
        assert!(!engine.interaction_mode);
    }

    // ========== prepare_for_wallpaper 测试 ==========

    #[test]
    fn prepare_for_wallpaper_image_no_existing_returns_ok() {
        // Image 类型不需要 WorkerW 预检，无现有壁纸时直接返回 Ok
        let mut engine = create_test_engine();
        let result = engine.prepare_for_wallpaper("monitor_0", WallpaperType::Image);
        assert!(result.is_ok());
    }

    #[test]
    fn prepare_for_wallpaper_image_closes_existing_workerw() {
        // Image 类型 + 已有 WorkerW 壁纸：应先关闭现有壁纸
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/old.gif".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Gif,
            )
            .unwrap();
        assert!(engine.has_wallpaper("monitor_0"));

        // 调用 prepare_for_wallpaper with Image type
        let result = engine.prepare_for_wallpaper("monitor_0", WallpaperType::Image);
        assert!(result.is_ok());

        // 验证现有壁纸已关闭
        assert!(!engine.has_wallpaper("monitor_0"));
        assert!(!engine.wallpapers.contains_key("monitor_0"));
        assert!(!engine.wallpaper_mode.contains_key("monitor_0"));
        assert!(!engine.wallpaper_sources.contains_key("monitor_0"));
        assert!(!engine.pause_senders.contains_key("monitor_0"));

        // 验证渲染器被 terminate
        let state = shared.lock().unwrap();
        assert!(state.terminated);
    }

    // ========== embed_and_register_renderer 测试 ==========

    #[test]
    fn embed_and_register_renderer_mock_registers_all_state() {
        // MockRenderer (hwnd=None) 跳过 WorkerW 嵌入，注册全部状态
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());

        let result = engine.embed_and_register_renderer(
            Box::new(renderer),
            "monitor_0",
            &source,
            WallpaperType::Video,
        );
        assert!(result.is_ok());

        // 验证所有状态已注册
        assert!(engine.wallpapers.contains_key("monitor_0"));
        assert_eq!(
            engine.wallpaper_mode.get("monitor_0"),
            Some(&WallpaperMode::WorkerW)
        );
        assert!(engine.wallpaper_sources.contains_key("monitor_0"));
        assert!(engine.pause_senders.contains_key("monitor_0"));
    }

    #[test]
    fn embed_and_register_renderer_multiple_displays() {
        // 多显示器独立注册：互不影响
        let mut engine = create_test_engine();
        let (r1, _s1) = MockRenderer::new();
        let (r2, _s2) = MockRenderer::new();
        let source1 = WallpaperSource::File("/test1.mp4".to_string());
        let source2 = WallpaperSource::File("/test2.gif".to_string());

        engine
            .embed_and_register_renderer(Box::new(r1), "monitor_0", &source1, WallpaperType::Video)
            .unwrap();
        engine
            .embed_and_register_renderer(Box::new(r2), "monitor_1", &source2, WallpaperType::Gif)
            .unwrap();

        assert_eq!(engine.wallpapers.len(), 2);
        assert_eq!(engine.wallpaper_mode.len(), 2);
        assert_eq!(engine.wallpaper_sources.len(), 2);
        assert_eq!(engine.pause_senders.len(), 2);
    }

    // ========== close_wallpaper 测试 ==========

    #[test]
    fn close_wallpaper_no_entry_returns_ok() {
        // 未注册的显示器应返回 Ok（no-op）
        let mut engine = create_test_engine();
        let result = engine.close_wallpaper("nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn close_wallpaper_workerw_mock_clears_all_state() {
        // 关闭 WorkerW 壁纸（MockRenderer）：清除全部状态，调用 renderer.terminate
        let mut engine = create_test_engine();
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.close_wallpaper("monitor_0");
        assert!(result.is_ok());

        // 验证所有状态已清除
        assert!(!engine.wallpapers.contains_key("monitor_0"));
        assert!(!engine.wallpaper_mode.contains_key("monitor_0"));
        assert!(!engine.wallpaper_sources.contains_key("monitor_0"));
        assert!(!engine.pause_senders.contains_key("monitor_0"));

        // 验证渲染器被 terminate
        assert!(shared.lock().unwrap().terminated);
    }

    // ========== close_wallpaper_by_path 测试 ==========

    #[test]
    fn close_wallpaper_by_path_match_closes_wallpaper() {
        // 路径匹配时应关闭对应壁纸
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let path = "/test/wallpaper.mp4".to_string();
        let source = WallpaperSource::File(path.clone());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.close_wallpaper_by_path(&path);
        assert!(result.is_ok());

        // 验证状态已清除
        assert!(!engine.has_wallpaper("monitor_0"));
    }

    #[test]
    fn close_wallpaper_by_path_no_match_returns_ok() {
        // 路径不匹配时应返回 Ok 且不修改状态
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let path = "/test/wallpaper.mp4".to_string();
        let source = WallpaperSource::File(path.clone());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.close_wallpaper_by_path("/different/path.mp4");
        assert!(result.is_ok());

        // 验证状态未被修改
        assert!(engine.has_wallpaper("monitor_0"));
    }

    #[test]
    fn close_wallpaper_by_path_url_source_no_match() {
        // URL 来源不应匹配文件路径
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::Url("https://example.com".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Web,
            )
            .unwrap();

        let result = engine.close_wallpaper_by_path("https://example.com");
        assert!(result.is_ok());
        // URL 不匹配 File 路径，状态应保持
        assert!(engine.has_wallpaper("monitor_0"));
    }

    // ========== shutdown 测试 ==========

    #[test]
    fn shutdown_clears_all_wallpapers() {
        // shutdown 应关闭所有壁纸并清空状态
        let mut engine = create_test_engine();
        let (r1, _s1) = MockRenderer::new();
        let (r2, _s2) = MockRenderer::new();
        let source1 = WallpaperSource::File("/test1.mp4".to_string());
        let source2 = WallpaperSource::File("/test2.gif".to_string());

        engine
            .embed_and_register_renderer(Box::new(r1), "monitor_0", &source1, WallpaperType::Video)
            .unwrap();
        engine
            .embed_and_register_renderer(Box::new(r2), "monitor_1", &source2, WallpaperType::Gif)
            .unwrap();
        assert_eq!(engine.wallpapers.len(), 2);

        engine.shutdown();

        // 验证所有壁纸已关闭
        assert!(engine.wallpapers.is_empty());
        assert!(engine.wallpaper_mode.is_empty());
        assert!(engine.wallpaper_sources.is_empty());
        assert!(engine.pause_senders.is_empty());
    }

    #[test]
    fn v13_cleanup_internal_clears_scaling_modes() {
        // v13.0: cleanup_internal（shutdown 路径）应清空 wallpaper_scaling_modes，
        // 与 pause_senders.clear() 对称。
        // 场景：设置 2 个壁纸 + 各自 set_scaling_mode 成功 → shutdown →
        // wallpaper_scaling_modes 为空
        let mut engine = create_test_engine();

        // 设置 2 个壁纸到不同 display_id
        for i in 0..2 {
            let (renderer, _shared) = MockRenderer::new();
            let source = WallpaperSource::File(format!("/test_{}.mp4", i));
            engine
                .embed_and_register_renderer(
                    Box::new(renderer),
                    &format!("monitor_{}", i),
                    &source,
                    WallpaperType::Video,
                )
                .unwrap();

            // set_scaling_mode 成功
            engine
                .set_wallpaper_results
                .lock()
                .unwrap()
                .push_back(Ok(()));
            let result = engine.set_scaling_mode(&format!("monitor_{}", i), ScalingMode::Fit);
            assert!(result.is_ok(), "set_scaling_mode for monitor_{} 应成功", i);
        }

        // 验证 wallpaper_scaling_modes 含 2 个条目
        assert_eq!(
            engine.wallpaper_scaling_modes.len(),
            2,
            "shutdown 前应有 2 个 scaling mode 条目"
        );

        // shutdown（内部调用 cleanup_internal）
        engine.shutdown();

        // 验证 wallpaper_scaling_modes 已清空
        assert!(
            engine.wallpaper_scaling_modes.is_empty(),
            "v13.0: cleanup_internal 应清空 wallpaper_scaling_modes"
        );
        // 同时验证 pause_senders 也已清空（回归防护）
        assert!(engine.pause_senders.is_empty());
    }

    #[test]
    fn shutdown_with_no_wallpapers_is_noop() {
        // 无壁纸时 shutdown 应不 panic
        let mut engine = create_test_engine();
        engine.shutdown();
        assert!(engine.wallpapers.is_empty());
    }

    // ========== W-008 修复测试：set_scaling_mode 失败回退 ==========

    #[test]
    fn w008_set_scaling_mode_video_success_updates_mode_tracking() {
        // W-008 成功路径验证：视频壁纸切换缩放模式成功时，应记录新缩放模式到
        // wallpaper_scaling_modes，供下次切换失败时作为回退目标。
        // 使用测试钩子注入 Ok 结果，避免真实 mpv 进程启动。
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // 预填测试钩子：set_wallpaper(new_mode) 成功
        engine
            .set_wallpaper_results
            .lock()
            .unwrap()
            .push_back(Ok(()));

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_ok(), "成功路径应返回 Ok");

        // 验证：wallpaper_scaling_modes 已记录新缩放模式
        assert_eq!(
            engine.wallpaper_scaling_modes.get("monitor_0"),
            Some(&ScalingMode::Fit),
            "成功切换后应记录新缩放模式，供下次回退使用"
        );

        // 验证：测试钩子队列已清空（一次调用消费一个结果）
        assert!(
            engine.set_wallpaper_results.lock().unwrap().is_empty(),
            "测试钩子队列应已清空"
        );
    }

    #[test]
    fn v13_close_wallpaper_clears_scaling_modes() {
        // v13.0: close_wallpaper 应同步清理 wallpaper_scaling_modes，
        // 与 pause_senders / wallpaper_mode / wallpaper_sources / wallpapers 保持一致。
        // 场景：设置壁纸 + set_scaling_mode 成功 → close_wallpaper →
        // wallpaper_scaling_modes 不含该 display_id（W-008 回退将使用默认值 Fill）
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test_video.mp4".to_string());

        // 注入 set_wallpaper 成功结果（避免真实 mpv 进程启动）
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // set_scaling_mode 成功后 wallpaper_scaling_modes 应含 monitor_0
        engine
            .set_wallpaper_results
            .lock()
            .unwrap()
            .push_back(Ok(()));
        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_ok(), "set_scaling_mode 应成功");
        assert_eq!(
            engine.wallpaper_scaling_modes.get("monitor_0"),
            Some(&ScalingMode::Fit),
            "set_scaling_mode 成功后应记录新 scaling mode"
        );

        // close_wallpaper 应清理 wallpaper_scaling_modes
        engine.close_wallpaper("monitor_0").unwrap();
        assert!(
            !engine.wallpaper_scaling_modes.contains_key("monitor_0"),
            "v13.0: close_wallpaper 应同步清理 wallpaper_scaling_modes"
        );
        // 同时验证其他 4 个 HashMap 也已清理（回归防护）
        assert!(!engine.wallpaper_mode.contains_key("monitor_0"));
        assert!(!engine.wallpaper_sources.contains_key("monitor_0"));
        assert!(!engine.pause_senders.contains_key("monitor_0"));
        assert!(!engine.wallpapers.contains_key("monitor_0"));
    }

    #[test]
    fn w008_set_scaling_mode_failure_rolls_back() {
        // W-008 回退成功场景：set_wallpaper(new_mode) 失败 → 回退到 original_mode 成功
        // → 返回包含原错误信息的 Err（提示已回退到原模式）。
        //
        // 通过测试钩子注入受控结果：第一次调用（new_mode）失败，第二次（rollback）成功。
        // 这样可精确验证回退逻辑的错误信息与流程，无需依赖真实 mpv 进程的失败行为。
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // 预填测试钩子结果：第一次（new_mode）失败，第二次（rollback）成功
        {
            let mut queue = engine.set_wallpaper_results.lock().unwrap();
            queue.push_back(Err(MirrorStarError::DesktopIntegration(
                "模拟新缩放模式重启失败".to_string(),
            )));
            queue.push_back(Ok(()));
        }

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);

        // 验证：返回 Err（回退成功也返回 Err，包含原错误信息）
        assert!(
            result.is_err(),
            "回退成功应返回包含原错误信息的 Err，而非 Ok"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("已回退到原模式"),
            "错误信息应提示已回退到原模式，实际: {}",
            err_msg
        );
        assert!(
            err_msg.contains("模拟新缩放模式重启失败"),
            "错误信息应包含原错误信息，实际: {}",
            err_msg
        );
        // 不应包含"壁纸已停止"（那是双重失败场景的提示）
        assert!(
            !err_msg.contains("壁纸已停止"),
            "回退成功场景不应包含'壁纸已停止'提示，实际: {}",
            err_msg
        );

        // 验证：测试钩子队列已清空（两次调用都消费了结果）
        assert!(
            engine.set_wallpaper_results.lock().unwrap().is_empty(),
            "测试钩子队列应已清空（new_mode + rollback 两次调用）"
        );
    }

    #[test]
    fn w008_set_scaling_mode_rollback_also_fails_reports_both_errors() {
        // W-008 双重失败场景：set_wallpaper(new_mode) 失败 → 回退也失败
        // → 返回包含双重错误信息的 Err，明确提示"壁纸已停止，需手动恢复"。
        //
        // 通过测试钩子注入两次失败结果，验证错误信息包含两个错误并提示需手动恢复。
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // 预填测试钩子结果：两次都失败
        {
            let mut queue = engine.set_wallpaper_results.lock().unwrap();
            queue.push_back(Err(MirrorStarError::DesktopIntegration(
                "新模式重启失败".to_string(),
            )));
            queue.push_back(Err(MirrorStarError::DesktopIntegration(
                "回退重启也失败".to_string(),
            )));
        }

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Stretch);

        // 验证：返回 Err，包含双重错误信息
        assert!(result.is_err(), "双重失败应返回 Err");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("壁纸已停止，需手动恢复"),
            "错误信息应明确提示壁纸已停止需手动恢复，实际: {}",
            err_msg
        );
        assert!(
            err_msg.contains("新模式重启失败"),
            "错误信息应包含原错误信息，实际: {}",
            err_msg
        );
        assert!(
            err_msg.contains("回退重启也失败"),
            "错误信息应包含回退错误信息，实际: {}",
            err_msg
        );

        // 验证：测试钩子队列已清空
        assert!(
            engine.set_wallpaper_results.lock().unwrap().is_empty(),
            "测试钩子队列应已清空（new_mode + rollback 两次调用）"
        );
    }

    // ========== set_scaling_mode 记忆写入（无活跃壁纸 / "all" 兜底 / 回退恢复） ==========

    #[test]
    fn set_scaling_mode_records_mode_without_active_wallpaper() {
        // 无活跃壁纸时调用 set_scaling_mode，也应记录用户选择的缩放模式。
        // 此前仅在视频重启成功分支写入，导致"无活跃壁纸改缩放"后记忆停留在旧值，
        // 后续 set_wallpaper 缺省回退取到错误值。
        let mut engine = create_test_engine();
        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Tile);
        assert!(result.is_ok(), "无活跃壁纸时应为 no-op 返回 Ok");
        assert_eq!(
            engine.scaling_mode_for("monitor_0"),
            ScalingMode::Tile,
            "无活跃壁纸时也应记录用户选择的缩放模式"
        );
    }

    #[test]
    fn set_scaling_mode_for_all_records_all_fallback_key() {
        // "all" 广播应写入 "all" 兜底键，使无精确项的显示器（含无任何活跃显示器
        // 的场景）也能取到用户选择。
        let mut engine = create_test_engine();
        engine
            .set_scaling_mode_for_all(ScalingMode::Center)
            .unwrap();
        assert_eq!(engine.scaling_mode_for("all"), ScalingMode::Center);
        assert_eq!(
            engine.scaling_mode_for(r"\\.\DISPLAY9"),
            ScalingMode::Center,
            "无精确项显示器应经 'all' 兜底取到该模式"
        );
    }

    #[test]
    fn w008_rollback_success_restores_original_mode_in_memory() {
        // 视频壁纸切换失败并回退成功时，记忆应恢复为 original_mode（而非未生效的新值）。
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();
        // 预置旧记忆为 Center，作为本次切换的 original_mode
        engine
            .wallpaper_scaling_modes
            .insert("monitor_0".to_string(), ScalingMode::Center);

        // 注入：new_mode 失败 → rollback 成功
        {
            let mut queue = engine.set_wallpaper_results.lock().unwrap();
            queue.push_back(Err(MirrorStarError::DesktopIntegration(
                "模拟新缩放模式重启失败".to_string(),
            )));
            queue.push_back(Ok(()));
        }

        let result = engine.set_scaling_mode("monitor_0", ScalingMode::Fit);
        assert!(result.is_err(), "回退成功仍应返回 Err");
        assert_eq!(
            engine.scaling_mode_for("monitor_0"),
            ScalingMode::Center,
            "回退成功后记忆应恢复为 original_mode"
        );
    }

    // ========== W13 修复测试：路径规范化匹配 ==========

    #[test]
    fn close_wallpaper_by_path_case_insensitive_match() {
        // W13: 路径比较应大小写不敏感（to_lowercase 规范化）
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("C:\\Users\\Test\\Wallpaper.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // 用全小写路径关闭，应匹配
        let result = engine.close_wallpaper_by_path("c:\\users\\test\\wallpaper.mp4");
        assert!(result.is_ok());
        assert!(
            !engine.has_wallpaper("monitor_0"),
            "大小写不敏感的路径应匹配并关闭壁纸"
        );
    }

    #[test]
    fn close_wallpaper_by_path_forward_slash_match() {
        // W13: 正斜杠路径应匹配反斜杠路径（/ → \\ 规范化）
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("C:\\Users\\Test\\Wallpaper.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        // 用正斜杠路径关闭，应匹配
        let result = engine.close_wallpaper_by_path("C:/Users/Test/Wallpaper.mp4");
        assert!(result.is_ok());
        assert!(
            !engine.has_wallpaper("monitor_0"),
            "正斜杠路径应匹配反斜杠路径并关闭壁纸"
        );
    }

    #[test]
    fn close_wallpaper_by_path_combined_case_and_slash_normalization() {
        // W13: 同时存在大小写差异 + 正斜杠差异应匹配
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("C:\\Users\\Test\\Wallpaper.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "monitor_0",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.close_wallpaper_by_path("c:/users/test/wallpaper.mp4");
        assert!(result.is_ok());
        assert!(
            !engine.has_wallpaper("monitor_0"),
            "大小写+分隔符混合差异应通过规范化匹配"
        );
    }

    #[test]
    fn normalize_path_for_compare_lowercases_and_unifies_separators() {
        // W13: 直接测试 normalize_path_for_compare 辅助函数
        // 大写 → 小写
        assert_eq!(
            normalize_path_for_compare("C:\\Users\\Test\\File.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        // 正斜杠 → 反斜杠
        assert_eq!(
            normalize_path_for_compare("C:/Users/Test/File.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        // 混合分隔符
        assert_eq!(
            normalize_path_for_compare("C:/Users\\Test/File.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        // 已规范化的路径保持不变
        assert_eq!(
            normalize_path_for_compare("c:\\users\\test\\file.mp4"),
            "c:\\users\\test\\file.mp4"
        );
        // 空路径
        assert_eq!(normalize_path_for_compare(""), "");
    }

    // ========== 原子交换三段式（B1 / DR-33 / P1-5）==========

    #[test]
    fn atomic_swap_three_phase_keeps_old_until_commit_and_terminates_it() {
        // B1 三段式 + P1-5：旧壁纸在 A 阶段全程留在 map，仅到 C 换槽临界区才移除并 terminate。
        let mut eng = create_test_engine();
        let src = WallpaperSource::File("C:/w/scene.mp4".to_string());

        // 阶段 1：先有旧 WorkerW 壁纸 → prepare 应返回 Swap（可原子交换）。
        let (old, old_shared) = MockRenderer::new();
        eng.register_renderer_maps(Box::new(old), "d1", &src, WallpaperType::Video);
        assert!(eng.wallpapers.contains_key("d1"), "旧壁纸应在 map");

        // 阶段 1（锁内快照）
        match eng.prepare_atomic_swap("d1", &src, WallpaperType::Video) {
            AtomicSwapPrepare::Swap(_) => {}
            _ => panic!("WorkerW 旧壁纸 + 非 Native 应返回 Swap"),
        }

        // 阶段 A（锁内 embed + after_embed）：新窗未登记、旧窗仍在 map。
        let (new, new_shared) = MockRenderer::new();
        let new = eng.embed_atomic_into(Box::new(new), "d1").unwrap();
        assert!(
            eng.wallpapers.contains_key("d1"),
            "A 阶段后旧壁纸仍应在 map 占位（P1-5）"
        );
        assert!(
            !old_shared.lock().unwrap().terminated,
            "A 阶段不应 terminate 旧窗"
        );

        // 阶段 C（锁内换槽）：一次换槽 → 新登记、旧 terminate。
        assert_eq!(
            eng.commit_atomic_swap(new, "d1", &src, WallpaperType::Video)
                .unwrap(),
            SwapOutcome::Committed
        );
        assert!(eng.wallpapers.contains_key("d1"), "换槽后新窗应在 map");
        assert!(
            old_shared.lock().unwrap().terminated,
            "C 阶段应 terminate 旧窗"
        );
        assert!(
            !new_shared.lock().unwrap().terminated,
            "新窗不应被 terminate"
        );
        // 换槽后 wallpaper_sources 已更新为新来源/类型。
        assert_eq!(
            eng.wallpaper_sources.get("d1").map(|(_, t)| *t),
            Some(WallpaperType::Video)
        );
    }

    #[test]
    fn atomic_swap_prepare_fallback_when_no_existing_wallpaper() {
        // 无现有壁纸 → prepare 返回 Fallback，调用方回退现有 set_wallpaper（先关后设）。
        let mut eng = create_test_engine();
        let src = WallpaperSource::File("C:/w/a.mp4".to_string());
        match eng.prepare_atomic_swap("d0", &src, WallpaperType::Video) {
            AtomicSwapPrepare::Fallback { old_renderer_id } => {
                assert_eq!(old_renderer_id, None, "无旧壁纸无 old_renderer_id");
            }
            _ => panic!("无现有壁纸应返回 Fallback"),
        }
    }

    #[test]
    fn atomic_swap_commit_succeeds_under_global_pause() {
        // B1 × B3 兼容：全局暂停位图非空时 commit 换槽仍须成功（补发暂停不破坏换槽），
        // 旧窗照常 terminate、新窗入槽。暂停补发经 `pause_senders` 通道下发。
        let mut eng = create_test_engine();
        let src = WallpaperSource::File("C:/w/b.mp4".to_string());

        let (old, old_shared) = MockRenderer::new();
        eng.register_renderer_maps(Box::new(old), "d1", &src, WallpaperType::Gif);
        *eng.pause_reasons.lock().unwrap() |= PauseReason::FULLSCREEN; // PauseReason 支持位掩码（BitOrAssign）

        let (new, new_shared) = MockRenderer::new();
        let new = eng.embed_atomic_into(Box::new(new), "d1").unwrap();
        assert_eq!(
            eng.commit_atomic_swap(new, "d1", &src, WallpaperType::Gif)
                .unwrap(),
            SwapOutcome::Committed
        );
        assert!(eng.wallpapers.contains_key("d1"));
        assert!(old_shared.lock().unwrap().terminated, "旧窗应 terminate");
        assert!(
            !new_shared.lock().unwrap().terminated,
            "新窗不应被 terminate"
        );
    }

    // ========== 纯净壁纸模式：进入/退出/中止/查询（D7 / D8）==========

    // `crate::pure_mode::is_active()` 是进程级全局静态量，而 libtest 默认并行运行测试：
    // 任一"置位该标志"的用例（enter 路径 / abort 路径）都会让并行运行的"断言标志为假"
    // 的用例偶发失败。所有读写该标志的用例共用同一把互斥锁即可彻底消除该竞态
    // （与 `wallpaper::SCREEN_SIZE_TEST_MUTEX` 的既有稳定性修复同构）。
    static PURE_MODE_FLAG_TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// 获取纯净壁纸模式标志测试的串行锁（容忍中毒，避免一个用例 panic 后连锁失败）。
    fn lock_pure_mode_flag_tests() -> std::sync::MutexGuard<'static, ()> {
        PURE_MODE_FLAG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// D8：`wallpaper_mode` 为空（无任何壁纸）时 `enter_pure_mode` 返回
    /// `InvalidArgument`，且**不置位**进程级标志（不进入"半进入"状态）。
    #[test]
    fn pure_mode_enter_rejects_when_no_wallpaper_and_leaves_flag_unset() {
        let _guard = lock_pure_mode_flag_tests();
        // 进程级标志是单源静态量，先归零，避免其它测试的遗留值干扰断言
        crate::pure_mode::set_active(false);
        let mut engine = create_test_engine();
        assert!(
            engine.wallpaper_mode.is_empty(),
            "新构造的测试引擎应无任何壁纸记录"
        );

        let err = engine
            .enter_pure_mode()
            .expect_err("无任何壁纸时 enter_pure_mode 应拒绝进入");
        let is_invalid_arg = matches!(err, MirrorStarError::InvalidArgument { .. });
        assert!(
            is_invalid_arg,
            "无任何壁纸时应返回 InvalidArgument，实际: {err:?}"
        );
        assert!(
            !crate::pure_mode::is_active(),
            "拒绝进入时不得置位进程级标志 pure_mode::is_active()"
        );
        assert!(
            !engine.is_pure_mode_active(),
            "拒绝进入时引擎权威状态应保持未进入"
        );
    }

    /// D7：未进入模式时 `exit_pure_mode` 是 no-op（`Ok(())`，无任何副作用）。
    #[test]
    fn pure_mode_exit_when_not_active_is_noop() {
        let _guard = lock_pure_mode_flag_tests();
        crate::pure_mode::set_active(false);
        let mut engine = create_test_engine();

        let result = engine.exit_pure_mode();
        assert!(
            result.is_ok(),
            "未进入模式时 exit_pure_mode 应为 no-op 返回 Ok，实际: {:?}",
            result.err()
        );
        assert!(engine.pure_mode.is_none(), "no-op 不应产生引擎权威状态");
        assert!(
            engine.pure_mode_temp.is_empty(),
            "no-op 不应产生临时呈现渲染器"
        );
        assert!(!crate::pure_mode::is_active(), "no-op 不应置位进程级标志");
        assert!(!engine.is_pure_mode_active(), "查询应保持未进入");
    }

    /// D7：未进入模式时 `abort_pure_mode` 安全（无 panic、清空临时渲染器），
    /// 且在"已置位标志"的构造下能复位进程级标志。
    #[test]
    fn pure_mode_abort_when_not_active_is_safe_and_resets_flag() {
        let _guard = lock_pure_mode_flag_tests();
        let mut engine = create_test_engine();
        // 构造"标志已置位"场景（模拟运行中/上一轮异常遗留）
        crate::pure_mode::set_active(true);

        engine.abort_pure_mode();
        assert!(
            !crate::pure_mode::is_active(),
            "abort_pure_mode 应复位进程级标志"
        );
        assert!(engine.pure_mode.is_none(), "中止后不应残留引擎权威状态");
        assert!(
            engine.pure_mode_temp.is_empty(),
            "中止后不应残留临时呈现渲染器"
        );

        // 幂等：再次调用不 panic，且标志仍为 false（测试结束不留污染）
        engine.abort_pure_mode();
        assert!(
            !crate::pure_mode::is_active(),
            "重复中止后进程级标志应仍为 false"
        );
    }

    /// D7：`is_pure_mode_active` 初值为 false（新引擎未进入模式）。
    #[test]
    fn pure_mode_is_pure_mode_active_initially_false() {
        let engine = create_test_engine();
        assert!(
            !engine.is_pure_mode_active(),
            "新引擎 is_pure_mode_active 初值应为 false"
        );
        assert!(engine.pure_mode.is_none(), "新引擎 pure_mode 应为 None");
    }

    /// F5：引擎状态不一致（`wallpaper_mode` 有 Native 记录但无可显示资源）时，
    /// `enter_pure_mode` 应拒绝进入"空模式"：返回 `Err`、回滚复位进程级标志、
    /// 不残留临时呈现渲染器、不写入引擎权威状态。
    #[test]
    fn pure_mode_enter_rejects_empty_mode_when_no_window_covered() {
        let _guard = lock_pure_mode_flag_tests();
        // 进程级标志是单源静态量，先归零，避免其它测试的遗留值干扰断言
        crate::pure_mode::set_active(false);
        let mut engine = create_test_engine();
        // 模拟"有记录但无可显示资源"的不一致状态：Native 记录存在，但
        // `wallpaper_sources` 为空 → 既无活跃窗口、也无临时呈现窗口。
        engine
            .wallpaper_mode
            .insert("d1".to_string(), WallpaperMode::Native);
        assert!(engine.wallpapers.is_empty(), "构造场景下应无活跃壁纸窗口");

        let err = engine
            .enter_pure_mode()
            .expect_err("无可覆盖窗口时 enter_pure_mode 应拒绝进入（F5 空模式）");
        assert!(
            matches!(err, MirrorStarError::InvalidArgument { .. }),
            "空模式应返回 InvalidArgument，实际: {err:?}"
        );
        assert!(
            !crate::pure_mode::is_active(),
            "拒绝进入时应回滚并复位进程级标志 pure_mode::is_active()"
        );
        assert!(
            engine.pure_mode_temp.is_empty(),
            "拒绝进入后不应残留临时呈现渲染器"
        );
        assert!(
            engine.pure_mode.is_none(),
            "拒绝进入后引擎权威状态应保持未进入"
        );

        // 复位进程级标志（幂等，避免污染其它测试）
        crate::pure_mode::set_active(false);
    }

    /// F1：`exit_pure_mode` 在嵌回失败时进入降级兜底（best-effort 隐藏 + error 日志），
    /// 以无效句柄构造"嵌回必失败"场景：断言不 panic、状态被清理、进程级标志复位。
    #[test]
    fn pure_mode_exit_hides_window_on_reattach_failure_without_panic() {
        let _guard = lock_pure_mode_flag_tests();
        crate::pure_mode::set_active(false);
        let mut engine = create_test_engine();
        // 直接写入"已进入模式"的权威状态，携带无效句柄（HWND(1)）使嵌回必然失败；
        // 此时 IsWindow 为假 → 隐藏兜底分支可能被跳过（允许），重点是不 panic 且状态被清理。
        engine.pure_mode = Some(PureModeState {
            lifted: vec![("D1".to_string(), HWND(1 as *mut _))],
            arrangement: engine.arrangement(),
        });

        let result = engine.exit_pure_mode();
        assert!(
            result.is_ok(),
            "嵌回失败应走降级兜底（不向上抛错），实际: {:?}",
            result.err()
        );
        assert!(engine.pure_mode.is_none(), "退出后引擎权威状态应被清空");
        assert!(!crate::pure_mode::is_active(), "退出后进程级标志应复位");

        // 复位进程级标志（幂等，避免污染其它测试）
        crate::pure_mode::set_active(false);
    }

    // ========== 纯净壁纸模式：文档标记测试（沿用仓库 include_str! 惯例）==========

    /// 提取源码中 `start..end` 之间的片段。
    ///
    /// `start` / `end` 取首次出现处：目标函数的定义位于测试模块之前，`find` 返回的
    /// 首个匹配即真实定义（与 `pure_mode/mod.rs::get_window_rect_precedes_set_parent`
    /// 的既有做法一致）。
    fn source_segment<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let s = source.find(start).expect("源码应包含起始标记");
        let e = s + source[s..].find(end).expect("源码应包含结束标记");
        &source[s..e]
    }

    /// 关键契约存在性：标志时序（置位早于置顶 / 清除晚于回嵌）、shutdown 先中止模式、
    /// 模式内关闭鼠标穿透、置顶窗口注册、`PureModeState` 的 `Send` 说明。
    #[test]
    fn pure_mode_source_contract_markers() {
        let source = include_str!("manager.rs");

        // D7：enter_pure_mode 段落必须先置位标志（早于任何置顶动作）
        let enter = source_segment(
            source,
            "pub fn enter_pure_mode(&mut self)",
            "fn enter_pure_mode_temp_windows(",
        );
        let set_true = ["set_active(", "true)"].join("");
        assert!(
            enter.contains(&set_true),
            "enter_pure_mode 段落应包含 {set_true}（标志置位早于置顶，D7）"
        );

        // D7：exit_pure_mode 段落应清除标志并清空置顶窗口注册表
        let exit = source_segment(
            source,
            "pub fn exit_pure_mode(&mut self)",
            "pub fn abort_pure_mode(&mut self)",
        );
        let set_false = ["set_active(", "false)"].join("");
        let clear = ["clear_", "lifted()"].join("");
        assert!(
            exit.contains(&set_false),
            "exit_pure_mode 段落应包含 {set_false}（标志清除晚于回嵌，D7）"
        );
        assert!(
            exit.contains(&clear),
            "exit_pure_mode 段落应包含 {clear}（清空置顶窗口注册表）"
        );

        // F1：exit_pure_mode 函数段（切至 abort_pure_mode 文档注释前）必须在嵌回失败时执行
        // 隐藏兜底。R3 后该兜底已抽为共享 helper `desktop::hide_window_on_reattach_failure`
        // （实现 `IsWindow` + `ShowWindow(SW_HIDE)`，位于 desktop 模块），故此处断言
        // exit_pure_mode 调用该 helper 并传入 `*hwnd`；helper 的隐藏实现断言见
        // `desktop::tests::pure_mode_half_detach_recovery_markers`（标记随实现一同移动）。
        let exit_body = {
            let start = source
                .find("pub fn exit_pure_mode(&mut self)")
                .expect("源码应包含 exit_pure_mode 定义");
            let end = start
                + source[start..]
                    .find("/// 中止纯净壁纸模式")
                    .expect("源码应包含 abort_pure_mode 文档注释");
            &source[start..end]
        };
        let hide_helper = ["hide_window_on_reattach_", "failure("].join("");
        let helper_at = exit_body
            .find(&hide_helper)
            .expect("exit_pure_mode 段落应调用共享隐藏兜底 helper（F1：嵌回失败隐藏兜底）");
        assert!(
            exit_body[helper_at..].contains(&["*hwnd", ","].join("")),
            "exit_pure_mode 的隐藏兜底调用应传入 *hwnd（F1：对嵌回失败窗口执行隐藏兜底）"
        );

        // F5：enter_pure_mode 段落必须包含"空模式拒绝"文案
        let reject = ["拒绝进入", "纯净壁纸模式"].join("");
        assert!(
            enter.contains(&reject),
            "enter_pure_mode 段落应包含 {reject} 文案（F5：空模式拒绝）"
        );

        // shutdown 首部应先中止模式（abort_pure_mode）再走既有清理（cleanup_internal）
        let shutdown = source_segment(
            source,
            "pub fn shutdown(&mut self)",
            "pub fn construct_renderer",
        );
        let abort = ["self.", "abort_pure_mode();"].join("");
        let cleanup = ["self.", "cleanup_internal();"].join("");
        let abort_at = shutdown
            .find(&abort)
            .expect("shutdown 应调用 abort_pure_mode()");
        let cleanup_at = shutdown
            .find(&cleanup)
            .expect("shutdown 应调用 cleanup_internal()");
        assert!(
            abort_at < cleanup_at,
            "shutdown 首部必须先 abort_pure_mode() 再 cleanup_internal()"
        );

        // D2：进入时对每个置顶窗关闭鼠标穿透
        let passthrough = ["set_mouse_passthrough(", "false)"].join("");
        assert!(
            source.contains(&passthrough),
            "源码应包含 {passthrough}（D2：模式内关闭鼠标穿透）"
        );

        // B2：set_interaction_mode 在纯净模式活跃时须提前返回（仅记录字段，跳过穿透切换），
        // 否则外部 enabled=false 会重开已置顶窗口的穿透，破坏 D2 契约。
        let sim = source_segment(
            source,
            "pub fn set_interaction_mode(&mut self,",
            "pub fn toggle_interaction",
        );
        assert!(
            sim.contains(&["self.pure_mode", ".is_some()"].join("")),
            "set_interaction_mode 应含纯净模式守卫（self.pure_mode.is_some()）"
        );
        assert!(
            sim.contains(&["return ", "Ok(())"].join("")),
            "set_interaction_mode 纯净模式守卫应提前 return Ok(())"
        );

        // B1：静态图片/GIF 的临时呈现窗（enter_pure_mode_temp_windows）同样须在置顶前
        // 去边框 + 去任务栏，与 embed_wallpaper Step 1 同源。
        let temp_fn = source_segment(
            source,
            "fn enter_pure_mode_temp_windows(",
            "/// 进入失败的统一回滚",
        );
        let temp_lift = ["lift_window_to_rect(", "hwnd, x, y, w, h)"].join("");
        let temp_lift_at = temp_fn
            .find(&temp_lift)
            .expect("临时呈现窗应调用 lift_window_to_rect(hwnd, x, y, w, h)");
        let temp_borderless = ["make_borderless(", "hwnd)"].join("");
        let temp_taskbar = ["remove_from_taskbar(", "hwnd)"].join("");
        let temp_borderless_at = temp_fn
            .find(&temp_borderless)
            .expect("临时呈现窗应在置顶前调用 make_borderless(hwnd)");
        let temp_taskbar_at = temp_fn
            .find(&temp_taskbar)
            .expect("临时呈现窗应在置顶前调用 remove_from_taskbar(hwnd)");
        assert!(
            temp_borderless_at < temp_lift_at,
            "临时呈现窗 {temp_borderless} 须位于 {temp_lift} 之前（B1：与 embed_wallpaper Step 1 同源）"
        );
        assert!(
            temp_taskbar_at < temp_lift_at,
            "临时呈现窗 {temp_taskbar} 须位于 {temp_lift} 之前（B1：与 embed_wallpaper Step 1 同源）"
        );

        // 置顶窗口注册（供 TaskbarCreated 广播后重断言 Z 序）
        let register = ["register_", "lifted"].join("");
        assert!(
            source.contains(&register),
            "源码应包含 {register}（登记置顶窗口以便重断言）"
        );

        // PureModeState 含 HWND（非 Send），须有 unsafe impl Send + SAFETY 论证
        let send_impl = ["unsafe impl Send for ", "PureModeState"].join("");
        assert!(
            source.contains(&send_impl),
            "源码应包含 {send_impl}（HWND 破坏 Send，需显式论证）"
        );
    }

    /// 架构 D1 契约：进入侧视频显示器**不摘出**，改用临时顶层渲染器覆盖；退出侧
    /// "退出重建 / 视频跳过回嵌"死代码已清除。
    ///
    /// 实验结论：进入侧"把视频窗口摘出 WorkerW 并置顶"会毒化本会话 WorkerW 子层
    /// （之后该层不再产出可呈现帧），而退出侧的窗口级唤醒/重建/回嵌手段全部被证伪。
    /// D1 让原视频窗口全程留在 WorkerW，纯净模式呈现改由临时顶层覆盖渲染器承担。
    /// 本测试以 `include_str!` + 运行期拼接搜索串（避免自匹配），断言：
    /// 1) `enter_pure_mode` 含"视频改用临时渲染器覆盖"标记与 `WallpaperType::Video` 判定；
    /// 2) 视频分支不调用 `detach_for_pure_mode`（原窗口不搬动）；
    /// 3) `exit_pure_mode` 不再包含"已重建视频渲染器"与"视频跳过回嵌"（死代码防回归）。
    #[test]
    fn pure_mode_enter_video_overlay_contract() {
        let source = include_str!("manager.rs");

        // enter 段：从函数定义到下一个 helper 定义前，仅取 enter_pure_mode 函数体
        // （视频分支已内联其中）。
        let enter_body = source_segment(
            source,
            "pub fn enter_pure_mode(&mut self)",
            "fn enter_pure_mode_temp_windows(",
        );

        // 运行期拼接，避免与本测试内的字面量自匹配。
        let overlay_marker = ["视频改用临时", "渲染器覆盖"].join("");
        let video_guard = ["Some((_, WallpaperType::", "Video))"].join("");
        let detach = ["detach_for_", "pure_mode(display_id)"].join("");

        assert!(
            enter_body.contains(&overlay_marker),
            "enter_pure_mode 应包含 {overlay_marker} 标记（D1：视频改用临时渲染器覆盖）"
        );
        assert!(
            enter_body.contains(&video_guard),
            "enter_pure_mode 应包含 {video_guard} 判定（识别视频显示器）"
        );

        // 视频分支区域：从视频类型判定 `if matches!(...)` 到其后非视频的"摘出 + 置顶"
        // 分支之前（该区域内含视频分支的全部内层 `continue;`），不得出现 detach 调用。
        let guard_at = enter_body
            .find(&video_guard)
            .expect("enter_pure_mode 应包含视频类型判定");
        let branch_rel_end = enter_body[guard_at..]
            .find("// 摘出 + 置顶")
            .expect("视频分支之后应有非视频摘出分支");
        let video_branch = &enter_body[guard_at..guard_at + branch_rel_end];
        assert!(
            video_branch.contains(&overlay_marker),
            "视频分支应包含 {overlay_marker}（使用临时渲染器覆盖）"
        );
        assert!(
            !video_branch.contains(&detach),
            "视频分支不得调用 {detach}（D1：原视频窗口全程留在 WorkerW，不摘出）"
        );

        // SubTask 2.10：覆盖窗须先无边框化（去边框 + 去任务栏）再置顶，与
        // embed_wallpaper Step 1 同源。运行期拼接，避免匹配文档注释中的函数名提及。
        let borderless = ["make_borderless(", "hwnd)"].join("");
        let taskbar = ["remove_from_taskbar(", "hwnd)"].join("");
        let lift = ["lift_window_to_rect(", "hwnd, x, y, w, h)"].join("");
        let borderless_at = video_branch
            .find(&borderless)
            .expect("视频覆盖分支应在置顶前调用 make_borderless(hwnd)");
        let taskbar_at = video_branch
            .find(&taskbar)
            .expect("视频覆盖分支应在置顶前调用 remove_from_taskbar(hwnd)");
        let lift_at = video_branch
            .find(&lift)
            .expect("视频覆盖分支应调用 lift_window_to_rect(hwnd, x, y, w, h)");
        assert!(
            borderless_at < lift_at,
            "{borderless} 须位于 {lift} 之前（与 embed_wallpaper Step 1 同源，先无边框再置顶）"
        );
        assert!(
            taskbar_at < lift_at,
            "{taskbar} 须位于 {lift} 之前（与 embed_wallpaper Step 1 同源，先前去边框再去任务栏再置顶）"
        );

        // SubTask 2.12：置顶前须先禁用 mpv 自动窗口适配（auto-window-resize），否则
        // mpv 会在 loadfile 时覆写应用设定的几何。须位于 lift_window_to_rect 之前。
        let disable_resize = ["disable_auto_window_", "resize()"].join("");
        let disable_at = video_branch
            .find(&disable_resize)
            .expect("视频覆盖分支应在置顶前调用 disable_auto_window_resize()");
        assert!(
            disable_at < lift_at,
            "{disable_resize} 须位于 {lift} 之前（载入前关闭 auto-window-resize，严格保持显示器矩形）"
        );

        // 死代码防回归：exit_pure_mode 不得再含"已重建视频渲染器"与"视频跳过回嵌"。
        let exit_body = source_segment(
            source,
            "pub fn exit_pure_mode(&mut self)",
            "/// 中止纯净壁纸模式",
        );
        let dead_rebuild = ["已重建视频", "渲染器"].join("");
        let dead_skip = ["视频跳过回嵌", "，直接重建"].join("");
        assert!(
            !exit_body.contains(&dead_rebuild),
            "exit_pure_mode 不应再包含 {dead_rebuild}（退出重建死代码应已删除）"
        );
        assert!(
            !exit_body.contains(&dead_skip),
            "exit_pure_mode 不应再包含 {dead_skip}（视频跳过回嵌死代码应已删除）"
        );
    }

    // ========== 渲染器存活探针 / 停滞自愈测试 ==========

    /// 构造一个 Playing 状态的 MockRenderer 并注册到 engine，返回共享状态句柄。
    fn register_playing_mock(
        engine: &mut WallpaperEngine,
        display_id: &str,
        probe: Option<LivenessProbe>,
    ) -> Arc<Mutex<MockRendererState>> {
        let (renderer, shared) = MockRenderer::new();
        let source = WallpaperSource::File(format!("/{display_id}.mp4"));
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                display_id,
                &source,
                WallpaperType::Video,
            )
            .unwrap();
        {
            let mut st = shared.lock().unwrap();
            st.state = WallpaperState::Playing;
            if let Some(probe) = probe {
                st.probe_results.push_back(Some(probe));
            }
        }
        shared
    }

    fn test_probe(time_pos: f64) -> LivenessProbe {
        LivenessProbe {
            time_pos,
            paused: false,
            idle_active: false,
            eof_reached: false,
        }
    }

    /// 测试 A：`probe_playing_videos` 只返回 `Playing` 且探针为 `Some` 的显示器。
    #[test]
    fn probe_playing_videos_filters_non_playing_and_none_probe() {
        let mut engine = create_test_engine();

        // Playing + 探针 Some → 应入集
        let _s0 = register_playing_mock(&mut engine, "monitor_0", Some(test_probe(3.0)));

        // Playing + 探针 None（该类型不支持探针）→ 不入集
        let s1 = {
            let (renderer, shared) = MockRenderer::new();
            let source = WallpaperSource::File("/m1.mp4".to_string());
            engine
                .embed_and_register_renderer(
                    Box::new(renderer),
                    "monitor_1",
                    &source,
                    WallpaperType::Video,
                )
                .unwrap();
            // 空队列 → liveness_probe() 返回 None
            shared.lock().unwrap().state = WallpaperState::Playing;
            shared
        };

        // Paused + 探针 Some → 不入集（且不应被查询）
        let s2 = {
            let (renderer, shared) = MockRenderer::new();
            let source = WallpaperSource::File("/m2.mp4".to_string());
            engine
                .embed_and_register_renderer(
                    Box::new(renderer),
                    "monitor_2",
                    &source,
                    WallpaperType::Video,
                )
                .unwrap();
            let mut st = shared.lock().unwrap();
            st.state = WallpaperState::Paused;
            st.probe_results.push_back(Some(test_probe(9.0)));
            drop(st);
            shared
        };

        let probes = engine.probe_playing_videos();
        assert_eq!(
            probes,
            vec![("monitor_0".to_string(), test_probe(3.0))],
            "仅 Playing 且探针为 Some 的显示器应入集"
        );
        // Playing+None 的显示器被查询了一次（返回 None 故不入集）
        assert_eq!(s1.lock().unwrap().probe_calls, 1);
        // Paused 的显示器因不满足 state 过滤，根本不应被查询
        assert_eq!(s2.lock().unwrap().probe_calls, 0);
    }

    /// 测试 B：`heal_stalled_renderer` 先 terminate 再 play（复用 revive 链路），终态 Playing。
    #[test]
    fn heal_stalled_renderer_terminates_then_plays() {
        let mut engine = create_test_engine();
        let shared = register_playing_mock(&mut engine, "monitor_0", None);

        engine
            .heal_stalled_renderer("monitor_0", 1.0, 1.0, "pure-mode-exit")
            .unwrap();

        let st = shared.lock().unwrap();
        assert!(st.terminated, "自愈应先 terminate 渲染器（避免残留旧 mpv）");
        assert!(st.played, "自愈应再 play() 重建渲染器");
        assert_eq!(st.state, WallpaperState::Playing, "自愈后终态应为 Playing");
    }

    /// 测试 C：`heal_stalled_renderer` 不触碰 `pause_reasons` 位图。
    ///
    /// 直接手动设置非空位图（而非 `pause_all_fast`，后者会把 state 置为 Paused 干扰断言）。
    #[test]
    fn heal_stalled_renderer_preserves_pause_reasons() {
        let mut engine = create_test_engine();
        let _shared = register_playing_mock(&mut engine, "monitor_0", None);

        // 手动设置非空位图：全屏 + 电池
        *engine.pause_reasons.lock().unwrap() = PauseReason::FULLSCREEN | PauseReason::BATTERY;
        let before = engine.pause_reasons_snapshot();

        engine
            .heal_stalled_renderer("monitor_0", 2.0, 2.0, "watchdog")
            .unwrap();

        let after = engine.pause_reasons_snapshot();
        assert_eq!(before, after, "自愈不应改动 pause_reasons 位图");
        assert!(!after.is_empty(), "位图应保持非空（暂停语义由调用方保证）");
    }
}
