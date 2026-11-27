//! 壁纸切换的共享编排（手动「设为壁纸」与轮播共用）。
//!
//! ## 职责
//!
//! 把"换一张壁纸"这一动作的完整编排从 `scheduler` / `commands` 中抽出：
//! 路由决策（Native / 原子交换 / 三阶段回退）→ 构建渲染器 → 嵌入 → 首帧就绪 →
//! 换槽。调用方只需提供目标（display / source / type / scaling）与策略
//! （[`SwitchPolicy`]），拿到 [`SwitchOutcome`]。
//!
//! ## 路由（[`decide_route`]）
//!
//! - 目标为 Native 模式（静态图片走系统 API）→ 锁内 [`WallpaperEngine::set_wallpaper`]
//!   （快，不构建 WorkerW 渲染器）。
//! - WorkerW 目标且类型为 Web 或纯净壁纸模式活跃 → 旧"先关后设"三阶段回退
//!   （Web 无内容首帧信号；纯净模式避免原子换槽在 commit 时终止已被摘出/置顶的旧窗）。
//! - 其余 WorkerW 动画目标（Video / Gif / WorkerW Image）→ 原子交换（旧壁纸保持
//!   占位到新壁纸首帧就绪），无旧窗 / 旧 Native 时自动回退三阶段。
//!
//! ## 原子交换三段式（DR-33 / DR-40）
//!
//! 锁内 `prepare_atomic_swap` → 锁外 `build_new_renderer` → 锁内
//! `embed_atomic_into`（阶段 A）→ 锁外 `wait_new_ready`（阶段 B）→ 锁内
//! `commit_atomic_swap`（阶段 C）。任何失败都保持旧壁纸原封不动。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::wallpaper::{ComGuard, DisplaySettingGuard};
use mirrorstar_core::config::ConfigManager;
use mirrorstar_core::wallpaper::manager::{
    construct_renderer, determine_wallpaper_mode, WallpaperMode,
};
use mirrorstar_core::{
    build_new_renderer, wait_new_ready, AtomicSwapPrepare, BuildOutcome, MirrorStarError,
    ScalingMode, WallpaperEngine, WallpaperRenderer, WallpaperSource, WallpaperType,
    ATOMIC_SWAP_IN_PROGRESS, ATOMIC_SWAP_READY_TIMEOUT,
};
use tauri::Emitter;

/// 原子交换进行中守卫（RAII，计数型）。
///
/// 构造时 `ATOMIC_SWAP_IN_PROGRESS` 计数 +1，Drop 时 -1；计数 `> 0` 即存在进行中的
/// 交换。用计数而非布尔值，是因为手动设壁纸与轮播可能在不同显示器并发交换——
/// 先结束的那个不应把仍在进行的交换误标为"已结束"。在 [`switch_wallpaper`] 的
/// 原子交换路径（阶段 A 嵌入 → B 首帧就绪 → C commit）创建，确保：
/// - 全屏终止/恢复检测到计数 `> 0` 时跳过本次（state.rs 见 terminate_all_fast_blocking /
///   resume_all_fast_blocking），避免交换期间重建 WorkerW / 冷启动 mpv
/// - 任何错误路径（wait_new_ready 失败 / commit 校验失败）退出作用域时自动 -1，
///   不会残留计数卡死后续全屏处置
pub(crate) struct AtomicSwapInProgressGuard;

impl AtomicSwapInProgressGuard {
    pub(crate) fn new() -> Self {
        ATOMIC_SWAP_IN_PROGRESS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for AtomicSwapInProgressGuard {
    fn drop(&mut self) {
        // 守卫成对创建/销毁，正常不会下溢；debug 断言兜底捕获误用。
        let previous = ATOMIC_SWAP_IN_PROGRESS.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "ATOMIC_SWAP_IN_PROGRESS 计数下溢");
    }
}

/// 切换路由（纯函数 [`decide_route`] 的输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchRoute {
    /// 目标为 Native 模式：锁内 `set_wallpaper`（系统 API，零渲染器）
    Native,
    /// 原子交换：旧壁纸占位到新壁纸首帧就绪
    Atomic,
    /// 旧"先关后设"三阶段回退
    Legacy,
}

/// 路由决策（纯函数，便于单测）。
///
/// 规则：
/// - Native 模式 → [`SwitchRoute::Native`]；
/// - WorkerW 目标且（类型为 Web 或纯净模式活跃）→ [`SwitchRoute::Legacy`]；
/// - 其余 → [`SwitchRoute::Atomic`]。
pub(crate) fn decide_route(
    mode: WallpaperMode,
    wallpaper_type: WallpaperType,
    pure_mode_active: bool,
) -> SwitchRoute {
    if mode == WallpaperMode::Native {
        return SwitchRoute::Native;
    }
    if wallpaper_type == WallpaperType::Web || pure_mode_active {
        return SwitchRoute::Legacy;
    }
    SwitchRoute::Atomic
}

/// 切换结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// 已切换（Native 设置 / 原子交换换槽 / 三阶段完成）
    Switched,
    /// 同目标短路：目标与当前壁纸一致（含缩放模式），未重建
    Skipped,
    /// 该显示器正在切换（per-display guard 被占用），未做任何操作
    Busy,
}

/// 切换策略（调用方语义差异）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SwitchPolicy {
    /// 构建渲染器前是否从配置同步一次播放设置（音量 / 静音 / 速度）。
    ///
    /// 手动命令需要（配置是播放设置的真相源，消除与 UI 异步写入的竞态）；
    /// 轮播不需要（渲染器配置快照已由引擎维护）。
    pub sync_playback_settings: bool,
    /// 目标屏**无旧壁纸**时，是否在构建前清除系统壁纸。
    ///
    /// 必须按调用方保留改动前语义：
    /// - 手动「设为壁纸」：改动前 `create_and_play_renderer` 恒 `clear_native=true`，故为 `true`；
    /// - 调度器轮播：改动前回退路径 `set_wallpaper` 传入 `pending.clear_native`
    ///   （关闭后恒 `false`），故为 `false`。
    ///
    /// 注意 `clear_native_wallpaper` 是**整机** SPI：轮播若误清，会把其它显示器
    /// 正在使用的 Native 壁纸一并清空，且构建期露出黑屏。
    pub clear_native_when_no_old: bool,
}

impl SwitchPolicy {
    /// 手动「设为壁纸」：同步播放设置（Native 路由不构建渲染器，无需同步）；
    /// 无旧壁纸时按改动前语义先清系统壁纸。
    pub const MANUAL: Self = Self {
        sync_playback_settings: true,
        clear_native_when_no_old: true,
    };
    /// 调度器轮播：不同步播放设置；无旧壁纸时保持系统壁纸（不清）。
    pub const SCHEDULER: Self = Self {
        sync_playback_settings: false,
        clear_native_when_no_old: false,
    };
}

/// 共享编排的运行环境（手动 vs 轮播的差异项）。
///
/// 汇总调用方独有的可选依赖，避免编排入口参数过多：
/// - `app`：手动命令的 `AppHandle`（Web `wallpaper-loading` 提示），轮播传 `None`；
/// - `config_manager`：手动命令的配置管理器（播放设置真相源），轮播传 `None`
///   （轮播的播放设置已由 `apply_wallpaper_to_unit` 预先同步）。
pub(crate) struct SwitchEnv<'a> {
    pub app: Option<&'a tauri::AppHandle>,
    pub config_manager: Option<&'a Arc<ConfigManager>>,
}

/// 共享切换编排主入口。
///
/// 流程：
/// 1. 获取 per-display [`DisplaySettingGuard`]；被占用说明该屏正在切换 → 返回
///    [`SwitchOutcome::Busy`]（不返回 Err：轮播需要静默跳过）。
/// 2. 锁内同目标短路（[`WallpaperEngine::same_target`]）→ [`SwitchOutcome::Skipped`]。
/// 3. Web 目标在通过 guard + 短路后发送 `wallpaper-loading` 提示（仅手动传入 app）。
/// 4. 按 [`decide_route`] 路由到 Native / 原子交换 / 三阶段回退。
pub(crate) async fn switch_wallpaper(
    env: SwitchEnv<'_>,
    engine: &Arc<tokio::sync::Mutex<WallpaperEngine>>,
    display_id: &str,
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
    policy: SwitchPolicy,
) -> Result<SwitchOutcome, MirrorStarError> {
    // B2：per-display guard 贯穿全程，防手动 / 轮播同屏并发。
    let _guard = match DisplaySettingGuard::acquire(display_id.to_string()) {
        Ok(guard) => guard,
        Err(e) => {
            tracing::info!(display_id, error = %e, "切换跳过：显示器正在切换");
            return Ok(SwitchOutcome::Busy);
        }
    };

    // 同目标短路：来源/类型与缩放模式都一致 → 不重建、不闪烁。
    {
        let eng = engine.lock().await;
        if eng.same_target(display_id, source, wallpaper_type, scaling_mode) {
            return Ok(SwitchOutcome::Skipped);
        }
    }

    // Web 冷启动（WebView2 5-15s）提示：放在 guard + 同目标短路之后，避免
    // Busy / 重复点击当前壁纸时闪出无意义的加载提示。
    if wallpaper_type == WallpaperType::Web {
        if let Some(app) = env.app {
            #[derive(Clone, serde::Serialize)]
            struct WebLoadingPayload<'a> {
                display_id: &'a str,
                message: &'a str,
            }
            if let Err(e) = app.emit(
                "wallpaper-loading",
                WebLoadingPayload {
                    display_id,
                    message: "正在初始化网页壁纸引擎，预计 5-15 秒...",
                },
            ) {
                tracing::warn!(error = %e, "emit wallpaper-loading 失败");
            }
        }
    }

    let mode = determine_wallpaper_mode(source, wallpaper_type);
    let route = decide_route(
        mode,
        wallpaper_type,
        mirrorstar_core::pure_mode::is_active(),
    );
    tracing::info!(display_id, ?wallpaper_type, ?route, "切换壁纸（共享编排）");

    match route {
        SwitchRoute::Native => {
            // `set_wallpaper` 的 Native 分支会先关闭旧壁纸（视频 terminate 最长 8s）
            // 再调用 SystemParametersInfo，属阻塞操作，必须在 blocking 线程执行，
            // 不占用 tokio worker 线程。
            let engine = engine.clone();
            let display = display_id.to_string();
            let src = source.clone();
            tokio::task::spawn_blocking(move || {
                let mut eng = engine.blocking_lock();
                eng.set_wallpaper(&display, &src, wallpaper_type, scaling_mode)
            })
            .await
            .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))??;
            Ok(SwitchOutcome::Switched)
        }
        SwitchRoute::Legacy => {
            legacy_workerw_switch(
                engine,
                env.config_manager,
                display_id,
                source,
                wallpaper_type,
                scaling_mode,
                policy,
            )
            .await
        }
        SwitchRoute::Atomic => {
            // 阶段 1（锁内，廉价）：可选播放设置同步 + 准备快照；旧壁纸保持占位。
            let prepared = {
                let mut eng = engine.lock().await;
                if policy.sync_playback_settings {
                    if let Some(cm) = env.config_manager {
                        let cfg = cm.get_config();
                        eng.set_playback_settings(
                            cfg.audio.volume,
                            cfg.audio.muted,
                            cfg.video.speed,
                        );
                    }
                }
                eng.prepare_atomic_swap(display_id, source, wallpaper_type)
            };

            let pending = match prepared {
                AtomicSwapPrepare::Swap(pending) => pending,
                AtomicSwapPrepare::Fallback { .. } => {
                    // 无旧窗 / 旧 Native → 回退三阶段（旧系统壁纸延迟清除）。
                    return legacy_workerw_switch(
                        engine,
                        env.config_manager,
                        display_id,
                        source,
                        wallpaper_type,
                        scaling_mode,
                        policy,
                    )
                    .await;
                }
            };

            // 阶段 2（锁外）：构建新渲染器并 play（不持引擎锁）。
            let build_source = source.clone();
            let built =
                tokio::task::spawn_blocking(move || -> Result<BuildOutcome, MirrorStarError> {
                    let _com = ComGuard::new();
                    build_new_renderer(&build_source, wallpaper_type, scaling_mode, &pending)
                })
                .await
                .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))??;

            match built {
                BuildOutcome::Skip => Ok(SwitchOutcome::Skipped),
                BuildOutcome::Ready(renderer) => {
                    // 阶段 A→C 全程置"原子交换进行中"计数，全屏处置检测到 > 0 时跳过。
                    let _swap_guard = AtomicSwapInProgressGuard::new();
                    // 阶段 A（锁内）：嵌入新窗 + after_embed/loadfile，旧壁纸保持占位。
                    let mut renderer = {
                        let mut eng = engine.lock().await;
                        eng.embed_atomic_into(renderer, display_id)?
                    };
                    // 阶段 B（锁外）：等待首帧就绪。
                    let waited = tokio::task::spawn_blocking(
                        move || -> (Result<(), MirrorStarError>, Box<dyn WallpaperRenderer>) {
                            let result = wait_new_ready(
                                &mut renderer,
                                ATOMIC_SWAP_READY_TIMEOUT,
                                Duration::from_millis(100),
                            );
                            (result, renderer)
                        },
                    )
                    .await
                    .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))?;
                    let (ready, mut renderer) = waited;
                    if let Err(e) = ready {
                        // 首帧失败：由本层终止新窗，旧壁纸原封不动。
                        let _ = renderer.terminate();
                        return Err(e);
                    }
                    // 阶段 C（锁内）：一次换槽 old→new + 补发暂停 + terminate 旧。
                    // 失败时 commit 内部已处置新窗，不再重复 terminate。
                    let mut eng = engine.lock().await;
                    eng.commit_atomic_swap(
                        renderer,
                        display_id,
                        source,
                        wallpaper_type,
                        scaling_mode,
                    )?;
                    Ok(SwitchOutcome::Switched)
                }
            }
        }
    }
}

/// 旧"先关后设"三阶段路径（Web 目标 / 纯净模式 / 无旧窗回退）。
///
/// 锁内 `prepare_set_wallpaper`（关旧 + 配置快照）→ 锁外 `construct_renderer`
/// （带 COM 初始化）→ 锁内 `complete_set_wallpaper`（嵌入注册 + 延迟清除旧系统壁纸）。
async fn legacy_workerw_switch(
    engine: &Arc<tokio::sync::Mutex<WallpaperEngine>>,
    config_manager: Option<&Arc<ConfigManager>>,
    display_id: &str,
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
    policy: SwitchPolicy,
) -> Result<SwitchOutcome, MirrorStarError> {
    // 必须在 prepare 之前取"是否有旧壁纸"：无旧壁纸时保留旧的"先清系统壁纸"语义。
    let (had_old, pending) = {
        let mut eng = engine.lock().await;
        let had_old = eng.has_wallpaper(display_id);
        if policy.sync_playback_settings {
            if let Some(cm) = config_manager {
                let cfg = cm.get_config();
                eng.set_playback_settings(cfg.audio.volume, cfg.audio.muted, cfg.video.speed);
            }
        }
        let pending = eng.prepare_set_wallpaper(display_id, source, wallpaper_type)?;
        (had_old, pending)
    };

    // 无旧壁纸时的清除语义必须按调用方保留（见 [`SwitchPolicy::clear_native_when_no_old`]）：
    // - 手动：构建前清（改动前 `create_and_play_renderer` 恒 clear）；
    // - 轮播：不清（改动前回退 `set_wallpaper` 传 `pending.clear_native=false`）。
    // 旧 Native → 新动态仍走 `defer_clear_native`（新窗嵌入后由
    // `complete_set_wallpaper` 补清除），构建期间保持系统壁纸可见。
    let build_clear_native = if had_old {
        pending.clear_native
    } else {
        policy.clear_native_when_no_old
    };
    let defer_clear_native = pending.defer_clear_native;
    let config = pending.config;

    let build_source = source.clone();
    let renderer = tokio::task::spawn_blocking(move || {
        let _com = ComGuard::new();
        construct_renderer(
            &build_source,
            wallpaper_type,
            scaling_mode,
            &config,
            build_clear_native,
        )
    })
    .await
    .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))??;

    {
        let mut eng = engine.lock().await;
        eng.complete_set_wallpaper(
            renderer,
            display_id,
            source,
            wallpaper_type,
            defer_clear_native,
        )?;
    }
    Ok(SwitchOutcome::Switched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_route_native_target_is_native() {
        assert_eq!(
            decide_route(WallpaperMode::Native, WallpaperType::Image, false),
            SwitchRoute::Native
        );
        // 纯净模式不影响 Native 目标路由。
        assert_eq!(
            decide_route(WallpaperMode::Native, WallpaperType::Image, true),
            SwitchRoute::Native
        );
    }

    #[test]
    fn decide_route_workerw_animation_targets_are_atomic() {
        for ty in [
            WallpaperType::Video,
            WallpaperType::Gif,
            WallpaperType::Image,
        ] {
            assert_eq!(
                decide_route(WallpaperMode::WorkerW, ty, false),
                SwitchRoute::Atomic,
                "{ty:?} WorkerW 目标应走原子交换"
            );
        }
    }

    #[test]
    fn decide_route_web_is_legacy() {
        assert_eq!(
            decide_route(WallpaperMode::WorkerW, WallpaperType::Web, false),
            SwitchRoute::Legacy
        );
    }

    #[test]
    fn decide_route_pure_mode_falls_back_to_legacy() {
        for ty in [
            WallpaperType::Video,
            WallpaperType::Gif,
            WallpaperType::Image,
            WallpaperType::Web,
        ] {
            assert_eq!(
                decide_route(WallpaperMode::WorkerW, ty, true),
                SwitchRoute::Legacy,
                "{ty:?} 在纯净模式活跃时应回退旧路径"
            );
        }
    }

    #[test]
    fn switch_policy_preserves_native_clear_semantics() {
        // 手动路径改动前恒先清系统壁纸；轮播改动前不清（整机 SPI，误清会波及其它显示器）。
        assert!(SwitchPolicy::MANUAL.clear_native_when_no_old);
        assert!(!SwitchPolicy::SCHEDULER.clear_native_when_no_old);
        assert!(SwitchPolicy::MANUAL.sync_playback_settings);
        assert!(!SwitchPolicy::SCHEDULER.sync_playback_settings);
    }
}
