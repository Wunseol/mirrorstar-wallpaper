use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::audio::volume::VolumeControl;
use crate::config::settings::Arrangement;
use crate::desktop::native_wallpaper;
use crate::desktop::DesktopIntegrator;
use crate::wallpaper::gif::GifRenderer;
use crate::wallpaper::image::ImageRenderer;
use crate::wallpaper::video::VideoRenderer;
use crate::wallpaper::web::WebRenderer;
use crate::wallpaper::{
    GifMemoryStrategy, PauseReason, PauseSender, ScalingMode, WallpaperRenderer, WallpaperSource,
    WallpaperType, DEFAULT_BALANCED_KEEP_FRAMES,
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
}

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
    /// 测试钩子：注入 `set_wallpaper` 返回值以测试 W-008 回退逻辑
    ///
    /// 仅 `#[cfg(test)]` 存在。测试预填结果队列，`call_set_wallpaper_for_scaling`
    /// 弹出队首代替真实调用；队列为空时回退到真实 `set_wallpaper`。
    #[cfg(test)]
    pub(crate) set_wallpaper_results:
        std::sync::Mutex<std::collections::VecDeque<Result<(), MirrorStarError>>>,
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

    /// 设置壁纸排列方式
    pub fn set_arrangement(&mut self, arrangement: Arrangement) {
        self.arrangement = arrangement;
        tracing::info!(?arrangement, "壁纸排列方式已切换");
    }

    /// 设置指定显示器的缩放模式
    pub fn set_scaling_mode(
        &mut self,
        display_id: &str,
        mode: ScalingMode,
    ) -> Result<(), MirrorStarError> {
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
                // W-008: 关闭原壁纸前记录原始缩放模式与壁纸源，用于失败回退
                let original_mode = self
                    .wallpaper_scaling_modes
                    .get(display_id)
                    .copied()
                    .unwrap_or_default();
                let original_source = source_clone.clone();
                // 先关闭当前壁纸
                self.close_wallpaper(display_id)?;
                // 尝试以新缩放模式重启（call_set_wallpaper_for_scaling 支持测试注入）
                let new_result = self.call_set_wallpaper_for_scaling(
                    display_id,
                    &source_clone,
                    type_clone,
                    mode,
                );
                match new_result {
                    Ok(()) => {
                        // 成功：更新缩放模式记录，供下次切换时作为回退目标
                        self.wallpaper_scaling_modes
                            .insert(display_id.to_string(), mode);
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
                                // 回退成功：返回包含原错误信息的 Err
                                // （engine 状态已恢复到 original_mode，但本次调用视为失败）
                                return Err(MirrorStarError::DesktopIntegration(format!(
                                    "切换缩放模式失败（已回退到原模式 {:?}）：{}",
                                    original_mode, new_err
                                )));
                            }
                            Err(rollback_err) => {
                                // 回退也失败：返回双重错误，明确提示壁纸已停止需手动恢复
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
        // 职责 1：关闭现有壁纸（含原生壁纸清除）
        if self.has_wallpaper(display_id) {
            self.close_wallpaper(display_id)?;
        }

        // 职责 2：决定壁纸模式
        let mode = determine_wallpaper_mode(source, wallpaper_type);

        // 职责 3：获取配置快照（Arc 克隆，不持有 engine 借用）
        let config = self.renderer_config();

        // 职责 4：计算 clear_native 标志
        // 注意：close_wallpaper 已清除原有壁纸模式，此处 is_native_mode 返回 false。
        // 但 construct_renderer 内部对 Video/Gif/Web 类型在 clear_native=true 时会
        // 调用 clear_native_wallpaper。为保持与原 set_wallpaper 一致的行为，
        // 在 close_wallpaper 之前记录原始模式。
        // 由于 close_wallpaper 已执行，此处 clear_native 始终为 false（与原逻辑一致：
        // 原 set_wallpaper 在 close_wallpaper 之后才调用 is_native_mode）。
        // W-012：此路径下 clear_native 恒为 false（因 close_wallpaper 已执行，
        // is_native_mode 返回 false）。该行为与原 set_wallpaper 在 close_wallpaper
        // 之后调用 is_native_mode 一致。仅 create_and_play_renderer（异步路径，
        // construct_renderer 入口）使用 clear_native = true，该路径不经过
        // close_wallpaper，可保留原始 is_native_mode 判断。
        let clear_native = self.is_native_mode(display_id);

        Ok(SetWallpaperPending {
            mode,
            config,
            clear_native,
        })
    }

    /// 阶段 3：完成设置壁纸（W04 修复：在 engine 锁内执行嵌入与注册）
    ///
    /// 将已 `play()` 的渲染器嵌入 WorkerW 并注册到 engine。
    /// 仅适用于 WorkerW 模式（Native 模式由 `set_native_wallpaper_internal` 处理）。
    pub fn complete_set_wallpaper(
        &mut self,
        renderer: Box<dyn WallpaperRenderer>,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
    ) -> Result<(), MirrorStarError> {
        self.embed_and_register_renderer(renderer, display_id, source, wallpaper_type)
    }

    /// 检查指定显示器是否已有壁纸
    pub fn has_wallpaper(&self, display_id: &str) -> bool {
        self.wallpapers.contains_key(display_id) || self.wallpaper_mode.contains_key(display_id)
    }

    /// 返回第一个活跃壁纸的 display_id（FE-001 修复）
    ///
    /// 用于命令层在 `display_id` 为 `None` 时回退查找当前活跃壁纸所在的显示器。
    /// HashMap 迭代顺序不保证，但单显示器场景（最常见）下唯一键即为所需值；
    /// 多显示器场景下前端应显式传入 `display_id`，避免依赖此回退。
    pub fn first_active_display_id(&self) -> Option<&str> {
        self.wallpapers.keys().next().map(|s| s.as_str())
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

    /// 查询指定显示器是否有 PauseSender
    ///
    /// 用于 Tauri 命令层（`pause_wallpaper` / `resume_wallpaper`）判断是否
    /// 需要 emit 兜底：
    /// - `true`：有 PauseSender（Video/Gif/Web/WorkerW Image 壁纸），pause 线程
    ///   会在状态变更后 emit，命令层**不应** emit（避免重复）。
    /// - `false`：无 PauseSender（原生壁纸或未设置壁纸），pause/resume 命令
    ///   对其无实际效果，命令层应 emit 兜底以通知前端刷新（虽然状态未变，
    ///   但保持前端与命令调用的一致性）。
    pub fn has_pause_sender(&self, display_id: &str) -> bool {
        self.pause_senders.contains_key(display_id)
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

        self.wallpaper_mode
            .insert(display_id.to_string(), WallpaperMode::WorkerW);
        self.wallpaper_sources
            .insert(display_id.to_string(), (source.clone(), wallpaper_type));
        let sender = renderer.create_pause_sender(display_id);
        if let Some(s) = sender {
            // 为新 PauseSender spawn 转发任务，将其 state_changed
            // broadcast 通道转发到 WallpaperEngine 的全局通道。Tauri 层订阅全局
            // 通道一次即可接收所有渲染器的状态变更通知。
            //
            // 使用 `Handle::try_current()` 而非直接 `tokio::spawn`：
            // `embed_and_register_renderer` 可能从同步测试（无 tokio runtime）调用，
            // 此时跳过 spawn（测试不依赖全局通道转发）；生产环境（Tauri async 命令）
            // 必有 runtime，spawn 正常执行。
            //
            // 转发任务生命周期：PauseSender 在 `close_wallpaper` 中从 pause_senders
            // 移除并被 drop，其内部 broadcast 通道关闭，转发任务的 `rx.recv().await`
            // 返回 `Err(Closed)`，任务自动退出，无泄漏。
            let rx = s.subscribe_state_changes();
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
            self.pause_senders.insert(display_id.to_string(), s);
        }
        self.wallpapers.insert(display_id.to_string(), renderer);
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
        if let Some(s) = renderer.create_pause_sender(display_id) {
            // 为新 PauseSender spawn 转发任务，将其 state_changed broadcast 通道
            // 转发到全局通道（与 embed_and_register_renderer 一致）。
            // `Handle::try_current()`：同步测试（无 tokio runtime）跳过 spawn。
            let rx = s.subscribe_state_changes();
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
            self.pause_senders.insert(display_id.to_string(), s);
        }

        self.wallpapers.insert(display_id.to_string(), renderer);
        tracing::info!(display_id, "全屏恢复：渲染器已重新嵌入并注册");
        Ok(())
    }

    /// 关闭指定显示器的壁纸
    ///
    /// 同步清理 5 个 per-display_id HashMap：pause_senders / wallpaper_mode /
    /// wallpaper_sources / wallpapers / wallpaper_scaling_modes，确保关闭后无残留状态。
    pub fn close_wallpaper(&mut self, display_id: &str) -> Result<(), MirrorStarError> {
        // 移除快速控制发送端
        self.pause_senders.remove(display_id);
        // 移除壁纸模式记录
        let mode = self.wallpaper_mode.remove(display_id);
        self.wallpaper_sources.remove(display_id);
        // v13.0: 同步清理 wallpaper_scaling_modes，与上述 4 个 HashMap 保持一致
        //（避免 W-008 set_scaling_mode 失败回退时使用已关闭壁纸的残留 scaling mode）
        self.wallpaper_scaling_modes.remove(display_id);

        if let Some(WallpaperMode::Native) = mode {
            // 原生壁纸：清除系统壁纸设置
            native_wallpaper::clear_native_wallpaper()?;
            tracing::info!(display_id, "关闭原生壁纸");
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

    /// 更新壁纸窗口位置（显示器配置变化时）
    pub fn update_positions(&mut self) -> Result<(), MirrorStarError> {
        let displays = crate::desktop::enumerate_displays();

        // W-005 修复：Span 模式下 GetSystemMetrics 调用结果与渲染器数量无关，
        // 在 for 循环前缓存为局部变量，循环内复用，避免 N 个渲染器产生 4N 次系统调用。
        // 各渲染器使用同一组虚拟屏幕坐标，计算结果与修复前一致。
        // 仅当存在待更新的渲染器时才调用 GetSystemMetrics（与原实现的惰性调用行为一致）。
        let span_metrics: Option<(i32, i32, i32, i32)> =
            if self.arrangement == Arrangement::Span && !self.wallpapers.is_empty() {
                use windows::Win32::UI::WindowsAndMessaging::{
                    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
                    SM_YVIRTUALSCREEN,
                };
                let virtual_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
                let virtual_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
                let virtual_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
                let virtual_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
                Some((virtual_x, virtual_y, virtual_w, virtual_h))
            } else {
                None
            };

        // 收集位置更新失败的显示器，循环结束后统一告警（不 fail-fast）
        let mut failures: Vec<(String, String)> = Vec::new();

        for (display_id, renderer) in &mut self.wallpapers {
            if let Some((virtual_x, virtual_y, virtual_w, virtual_h)) = span_metrics {
                // W01 修复：不 clamp SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN 的负值。
                // 多显示器场景下副显示器位于主显示器左侧/上方时，虚拟屏幕原点为负值，
                // clamp 到 0 会导致 Span 模式壁纸窗口位置错误（偏移到主显示器内）。
                // 仅保留宽高的 > 0 守卫（宽高不能为负，GetSystemMetrics 失败时返回 0）。
                if let Err(e) = renderer.set_position(
                    virtual_x,
                    virtual_y,
                    if virtual_w > 0 { virtual_w } else { 1920 },
                    if virtual_h > 0 { virtual_h } else { 1080 },
                ) {
                    failures.push((display_id.clone(), e.to_string()));
                }
            } else {
                if let Some(display) = displays.iter().find(|d| d.id == *display_id) {
                    if let Err(e) = renderer.set_position(
                        display.x,
                        display.y,
                        display.width as i32,
                        display.height as i32,
                    ) {
                        failures.push((display_id.clone(), e.to_string()));
                    }
                }
            }
        }

        if !failures.is_empty() {
            tracing::warn!(failed = ?failures, "部分显示器位置更新失败");
        }

        Ok(())
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
    construct_renderer(
        source,
        wallpaper_type,
        scaling_mode,
        config,
        true,
    )
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

    // ========== update_positions 测试 ==========

    #[test]
    fn update_positions_per_monitor_no_panic() {
        // PerMonitor 模式：使用 enumerate_displays 查找位置
        // 即使显示器 ID 不匹配真实显示器，函数也应返回 Ok（跳过该渲染器）
        let mut engine = create_test_engine();
        let (renderer, _shared) = MockRenderer::new();
        let source = WallpaperSource::File("/test.mp4".to_string());
        engine
            .embed_and_register_renderer(
                Box::new(renderer),
                "nonexistent_display",
                &source,
                WallpaperType::Video,
            )
            .unwrap();

        let result = engine.update_positions();
        assert!(result.is_ok());
    }

    #[test]
    fn update_positions_span_arrangement_no_panic() {
        // Span 模式：使用 GetSystemMetrics 获取虚拟屏幕尺寸
        let mut engine = create_test_engine();
        engine.set_arrangement(Arrangement::Span);
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

        let result = engine.update_positions();
        assert!(result.is_ok());

        // Span 模式下应调用 set_position（GetSystemMetrics 返回值）
        let state = shared.lock().unwrap();
        assert_eq!(state.position_calls.len(), 1);
    }

    #[test]
    fn update_positions_empty_wallpapers_returns_ok() {
        // 无壁纸时应返回 Ok（no-op）
        let mut engine = create_test_engine();
        let result = engine.update_positions();
        assert!(result.is_ok());
    }

    // ========== W01 修复测试：Span 模式负坐标不 clamp ==========

    #[test]
    fn update_positions_span_passes_raw_virtual_screen_coords_without_clamping() {
        // W01 修复验证：Span 模式下 SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN 可能为负值
        // （副显示器位于主显示器左侧/上方时，虚拟屏幕原点为负）。
        // 原实现 `if virtual_x > 0 { virtual_x } else { 0 }` 将负值 clamp 到 0，
        // 导致 Span 模式壁纸窗口位置错误（偏移到主显示器内）。
        // 修复后应将原始值直接传递给 set_position，仅保留宽高的 > 0 守卫。
        //
        // 本测试通过对比 GetSystemMetrics 原始值与 set_position 收到的值，
        // 验证无 clamp 转换。在副显示器位于左侧/上方的多显示器环境会捕获回归
        // （负值被错误 clamp 为 0）；单显示器环境下值通常为 0，测试仍通过且
        // 文档化预期行为。
        let mut engine = create_test_engine();
        engine.set_arrangement(Arrangement::Span);
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

        // 读取原始 GetSystemMetrics 值用于比较
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        let raw_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let raw_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let raw_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let raw_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

        let result = engine.update_positions();
        assert!(result.is_ok());

        let state = shared.lock().unwrap();
        assert_eq!(state.position_calls.len(), 1);
        let (x, y, w, h) = state.position_calls[0];
        // W01: x/y 应等于原始 GetSystemMetrics 值，不做 clamp 到 0
        assert_eq!(
            x, raw_x,
            "SM_XVIRTUALSCREEN 应直接传递给 set_position，不做 clamp"
        );
        assert_eq!(
            y, raw_y,
            "SM_YVIRTUALSCREEN 应直接传递给 set_position，不做 clamp"
        );
        // 宽高保留 > 0 守卫（GetSystemMetrics 失败时返回 0，回退到默认值）
        assert_eq!(w, if raw_w > 0 { raw_w } else { 1920 });
        assert_eq!(h, if raw_h > 0 { raw_h } else { 1080 });
    }

    // ========== W-005 修复测试：Span 模式 GetSystemMetrics 调用缓存 ==========

    #[test]
    fn w005_update_positions_span_caches_system_metrics() {
        // W-005 修复验证：Span 模式下 GetSystemMetrics(SM_XVIRTUALSCREEN 等) 调用次数
        // 应为常数次（4 次），与渲染器数量 N 无关。
        //
        // 原实现在 for 循环内对每个渲染器调用 4 次 GetSystemMetrics（共 4N 次）。
        // 修复后改为在循环前缓存为局部变量，循环内复用（共 4 次）。
        //
        // 由于 GetSystemMetrics 是 Win32 FFI，无法直接 mock 计数，本测试通过行为验证：
        // 注册 N=3 个渲染器，断言每个渲染器收到的位置值都相同，且与循环前
        // GetSystemMetrics 快照一致 —— 这正是"循环内复用缓存值"的预期行为。
        // 同时验证"多渲染器行为一致性"这一修复的关键不变量。
        let n = 3;
        let mut engine = create_test_engine();
        engine.set_arrangement(Arrangement::Span);

        let mut shared_states = Vec::new();
        for i in 0..n {
            let (renderer, shared) = MockRenderer::new();
            let source = WallpaperSource::File(format!("/test_{}.mp4", i));
            engine
                .embed_and_register_renderer(
                    Box::new(renderer),
                    &format!("monitor_{}", i),
                    &source,
                    WallpaperType::Video,
                )
                .unwrap();
            shared_states.push(shared);
        }
        assert_eq!(engine.wallpapers.len(), n);

        // 读取循环前 GetSystemMetrics 快照（与 update_positions 内的缓存时机一致）
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        let raw_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let raw_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let raw_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let raw_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        let expected = (
            raw_x,
            raw_y,
            if raw_w > 0 { raw_w } else { 1920 },
            if raw_h > 0 { raw_h } else { 1080 },
        );

        let result = engine.update_positions();
        assert!(result.is_ok());

        // 所有 N 个渲染器应各收到恰好 1 次 set_position 调用，
        // 且值与循环前 GetSystemMetrics 缓存快照一致（证明值被复用而非逐次重新获取）
        for (i, shared) in shared_states.iter().enumerate() {
            let state = shared.lock().unwrap();
            assert_eq!(
                state.position_calls.len(),
                1,
                "渲染器 {} 应收到恰好 1 次 set_position 调用",
                i
            );
            assert_eq!(
                state.position_calls[0], expected,
                "渲染器 {} 应收到缓存的虚拟屏幕坐标（与循环前快照一致，证明值被复用）",
                i
            );
        }

        // 关键不变量：N 个渲染器收到的位置值完全相同（同一组缓存坐标）。
        // 若实现回退为循环内逐次调用 GetSystemMetrics，在多显示器配置快速变化时
        // 可能产生不一致；缓存则保证一致性。
        let first = shared_states[0].lock().unwrap().position_calls[0];
        for (i, shared) in shared_states.iter().enumerate() {
            let state = shared.lock().unwrap();
            assert_eq!(
                state.position_calls[0], first,
                "所有渲染器应收到相同的缓存坐标（渲染器 {} 与渲染器 0 不一致）",
                i
            );
        }
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
}
