//! 壁纸轮换调度器运行时（src-tauri 侧，设计 §5 / §6 / §13 / §16）。
//!
//! 职责边界（DR-1）：调度器是"换壁纸动作"的唯一决策者（WHEN / WHICH / HOW），
//! 渲染执行完全下放给 `WallpaperEngine`（原子交换或回退）。纯决策逻辑（采样 /
//! boot 解析 / playback 持久化）在 `mirrorstar_core::scheduler` 与
//! `mirrorstar_core::config::playback`，本模块只负责把决策串联成单个后台任务。
//!
//! ## 结构
//!
//! - [`SchedulerHandle`]：进程级共享句柄，暴露给命令层 / 托盘 / setup。持有
//!   - `playback`（权威单元播放状态，命令层与调度器共享）；
//!   - `manual_tx`（手动"下一张"请求通道）；
//!   - `wake`（通知调度器重算 deadline / 处理 config / 暂停 / 编排变化）。
//!   - `config_manager` / `engine` / `desktop` 引用。
//! - `run_loop`：单个 tokio 任务，负责启动对账 → 开机解析一次 → 主循环。
//!
//! ## 主循环
//!
//! 每次醒来后重新读取 `AppConfig` 并刷新镜像、对齐单元（编排 / 显示器变化）、
//! 计算下一次定时 deadline，然后 `select!` 等待：
//! - 定时到点 → 对每个参与单元执行轮换（已因非暂停进入此分支）；
//! - 通知（config / 暂停 / 池成员 / 编排变化）→ 失效重建采样器（DR-23）+ 重算 deadline；
//! - 手动下一张 → 对目标单元执行一次换图（不受暂停限制，DR-9 / DR-20）。
//!
//! ## 暂停抑制（DR-9 / 设计 §10 Layer 2）
//!
//! `pause_reasons` 非空时抑制**定时**轮换触发（换图动作暂停）；手动下一张不受限。
//! 位图清空后重算 deadline（不补时，DR-38）——本实现通过每次醒来重读暂停位图与
//! 重算 deadline 达成"无漂移、不补时"；托盘暂停切换经 `wake` 立即唤醒，全屏 /
//! 电池暂停期间定时到点自动抑制、下一次到点再判断（幂等，避免过度唤醒）。
//!
//! ## 应用路径（DR-33 原子交换）
//!
//! `apply_to_display` 走三段式：锁内 `prepare_atomic_swap`（旧壁纸保持占位）→ 锁内
//! `embed_atomic_into`（阶段 A：嵌入 + after_embed/loadfile）→ 锁外 `wait_new_ready`
//! （阶段 B：首帧就绪）→ 锁内 `commit_atomic_swap`（阶段 C：一次换槽，DR-40 双窗安全）。
//! 入口为 `display_id` acquire per-display guard（B2）贯穿全程，防轮换与手动/另一轮换
//! 同屏并发。Native 图片 / 无现有壁纸回退现有 `set_wallpaper` 同步便利路径（先关后设，
//! DR-33 注）。失败保持旧壁纸、游标已推进跳过（DR-19）。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::commands::wallpaper::DisplaySettingGuard;
use mirrorstar_core::config::{PlaybackState, PlaybackStore, Unit};
use mirrorstar_core::scheduler::{
    filter_candidates, invalidate_sampler, resolve_boot, sample_next, BootDecision, PoolEntry,
    SamplerState,
};
use mirrorstar_core::{
    build_new_renderer, wait_new_ready, AppConfig, Arrangement, AtomicSwapPrepare, BuildOutcome,
    ConfigManager, DesktopIntegrator, MirrorStarError, ScalingMode, SwapOutcome, WallpaperEngine,
    WallpaperRenderer, WallpaperSource, WallpaperType, ATOMIC_SWAP_READY_TIMEOUT,
};
use tauri::Emitter;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

/// 全局 / 全体调度单元 key（`AllSame` / `Span` 编排下的唯一单元；亦作为"全局开关"
/// 的 key，设计 §14）。
pub const ALL_UNIT_KEY: &str = "all";

/// `wallpaper-rotated` 事件负载（DR-30）。
#[derive(serde::Serialize, Clone)]
pub struct RotatedPayload {
    pub key: String,
    pub wallpaper_id: String,
}

/// 调度器共享句柄（命令层 / 托盘 / setup 与调度器任务之间通信的唯一入口）。
pub struct SchedulerHandle {
    /// 权威单元播放状态（`Unit` 即采样器状态；命令层与调度器经 `Mutex` 共享）。
    pub playback: Arc<Mutex<PlaybackState>>,
    /// 播放状态持久化 store（`playback.toml`，独立于 config 热重载，DR-15）。
    pub store: PlaybackStore,
    /// 手动"下一张"请求通道（`Option<String>` = 目标单元 key，`None` = 主/唯一单元）。
    pub manual_tx: UnboundedSender<Option<String>>,
    /// 唤醒信号：config / 暂停 / 池成员 / 编排变化 → `notify_waiters()`。
    pub wake: Arc<Notify>,

    config_manager: Arc<ConfigManager>,
    engine: Arc<tokio::sync::Mutex<WallpaperEngine>>,
    desktop: Arc<Mutex<DesktopIntegrator>>,
    /// 手动请求接收端，由 `start()` 取出交给主循环任务。
    manual_rx: Mutex<Option<UnboundedReceiver<Option<String>>>>,
}

impl SchedulerHandle {
    /// 构造调度器句柄；加载 `playback.toml`（损坏 / 旧版回退默认，DR-32）。
    pub fn new(
        config_manager: Arc<ConfigManager>,
        engine: Arc<tokio::sync::Mutex<WallpaperEngine>>,
        desktop: Arc<Mutex<DesktopIntegrator>>,
    ) -> Arc<Self> {
        let store = PlaybackStore::new();
        let playback = Arc::new(Mutex::new(store.load()));
        let (manual_tx, manual_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            playback,
            store,
            manual_tx,
            wake: Arc::new(Notify::new()),
            config_manager,
            engine,
            desktop,
            manual_rx: Mutex::new(Some(manual_rx)),
        })
    }

    /// 启动调度器主循环（单个后台 tokio 任务）。仅可调用一次。
    ///
    /// 启动顺序（设计 §16）：读 config / playback → 启动对账（DR-34）→ 对齐单元 →
    /// `resolve_boot()` 一次 apply → 进入主循环并订阅 `wake` / 手动通道。
    pub fn start(self: &Arc<Self>, app: tauri::AppHandle) {
        let handle = self.clone();
        tauri::async_runtime::spawn(async move {
            // 手动通道接收端（创建句柄时生成，取一次）。
            let manual_rx = match handle.manual_rx.lock().map(|mut r| r.take()) {
                Ok(Some(rx)) => rx,
                _ => {
                    tracing::error!("调度器：手动通道接收端已被取走或锁中毒，跳过主循环");
                    return;
                }
            };
            run_loop(handle, app, manual_rx).await;
        });
    }

    // ── 命令层触发的单元状态变更（set_active_pool / set_rotation_enabled） ─────

    /// 设置单元生效池（DR-35）。`pool_id = None` → 回退隐式"全部"池。
    /// 命令层已校验 key 合法（孤儿单元拒绝）；此处仅校验池存在。
    ///
    /// 写后立即落盘 + `wake` 唤醒调度器重算 deadline / 失效重建采样器（DR-23）。
    pub fn set_unit_active_pool(
        &self,
        key: &str,
        pool_id: Option<String>,
    ) -> Result<(), MirrorStarError> {
        if let Some(pid) = &pool_id {
            if self.config_manager.get_pool(pid).is_none() {
                return Err(MirrorStarError::InvalidArgument {
                    reason: format!("池不存在: {pid}"),
                });
            }
        }
        {
            let mut play = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            let unit = play.ensure_unit(key.to_string());
            unit.active_pool = pool_id;
        }
        self.flush_playback();
        self.wake.notify_waiters();
        Ok(())
    }

    /// 设置单元是否参与轮换。写后立即落盘 + `wake` 唤醒调度器重算 deadline。
    pub fn set_unit_enabled(&self, key: &str, enabled: bool) {
        {
            let mut play = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            let unit = play.ensure_unit(key.to_string());
            unit.enabled = enabled;
        }
        self.flush_playback();
        self.wake.notify_waiters();
    }

    /// 按当前编排批量启用布局单元（全局 `rotation.enabled` 由 false→true 时联动）。
    ///
    /// 复用 `ensure_unit`（单元不存在则新建后启用）的语义，但一次持锁遍历所有 key，
    /// 统一 `flush_playback()` 一次并 `wake.notify_waiters()` 一次，避免为每个单元
    /// 重复落盘 + 唤醒。此后用户仍可经 [`set_unit_enabled`](Self::set_unit_enabled)
    /// 单独关闭某单元，该关闭保持到下次全局再开启为止。
    pub fn enable_layout_units(&self, keys: &[String]) {
        {
            let mut play = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            set_layout_units_enabled(&mut play, keys);
        }
        self.flush_playback();
        self.wake.notify_waiters();
    }

    // ── playback 防抖落盘（DR-29 简化：轮换/开机低频，直接原子写，PS1） ────────

    pub(crate) fn flush_playback(&self) {
        let state = match self.playback.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if let Err(e) = self.store.save(&state) {
            tracing::warn!(error = %e, "playback.toml 保存失败，内存态继续运行（PS1）");
        }
    }
}

/// 无副作用地批量置位布局单元的 `enabled`（供 [`SchedulerHandle::enable_layout_units`]
/// 与测试复用）。单元不存在则经 `ensure_unit` 新建后启用；不落盘、不唤醒。
fn set_layout_units_enabled(play: &mut PlaybackState, keys: &[String]) {
    for key in keys {
        play.ensure_unit(key.clone()).enabled = true;
    }
}

// ── 布局（编排 + 显示器 → 单元 key → display 集合映射）──────────────────────

struct Layout {
    arrangement: Arrangement,
    /// unit_key → 该单元覆盖的 display id 列表。
    unit_displays: HashMap<String, Vec<String>>,
    /// 主显示器 id（PerMonitor 下用于默认手动目标，DR-36）。
    pub primary: Option<String>,
}

impl Layout {
    fn new() -> Self {
        Self {
            arrangement: Arrangement::PerMonitor,
            unit_displays: HashMap::new(),
            primary: None,
        }
    }

    fn unit_keys(&self) -> Vec<String> {
        match self.arrangement {
            Arrangement::PerMonitor => self.unit_displays.keys().cloned().collect(),
            Arrangement::AllSame | Arrangement::Span => {
                if self.unit_displays.contains_key(ALL_UNIT_KEY) {
                    vec![ALL_UNIT_KEY.to_string()]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

// ── 候选 / 采样辅助（纯决策，复用 mirrorstar_core::scheduler）────────────────

/// 由生效池解析候选 `PoolEntry`：显式池按 `member_ids` 顺序、隐式"全部"池按壁纸库顺序
///（顺序均用于 Sequential 采样，DR-5）。已删除 / 缺文件者交由 `filter_candidates` 剔除。
fn build_pool_entries(cm: &ConfigManager, active_pool: Option<&str>) -> Vec<PoolEntry> {
    match active_pool {
        Some(pid) => {
            let by_id: HashMap<String, mirrorstar_core::config::WallpaperEntry> = cm
                .get_wallpapers()
                .into_iter()
                .map(|e| (e.id.clone(), e))
                .collect();
            cm.get_pool(pid)
                .into_iter()
                .flat_map(|p| p.member_ids)
                .filter_map(|id| {
                    by_id.get(&id).map(|e| PoolEntry {
                        id: e.id.clone(),
                        ty: e.wallpaper_type,
                        path: e.file_path.clone(),
                    })
                })
                .collect()
        }
        None => cm
            .get_wallpapers()
            .into_iter()
            .map(|e| PoolEntry {
                id: e.id,
                ty: e.wallpaper_type,
                path: e.file_path,
            })
            .collect(),
    }
}

/// 过滤后候选数（Web 剔除 + 文件缺失剔除，DR-18）；用于 boot 决策的池 ≥1 / ≥2 判断。
fn pool_count(cm: &ConfigManager, active_pool: Option<&str>) -> usize {
    filter_candidates(build_pool_entries(cm, active_pool).iter().collect()).len()
}

// ── 主循环 ──────────────────────────────────────────────────────────────────

async fn run_loop(
    handle: Arc<SchedulerHandle>,
    app: tauri::AppHandle,
    mut manual_rx: UnboundedReceiver<Option<String>>,
) {
    // 启动阶段：读配置 + 对账 + 对齐单元 + 开机解析一次（设计 §16）。
    let cfg = handle.config_manager.get_config();
    let mut layout = Layout::new();
    reconcile_and_align(&handle, cfg.rotation.arrangement, &mut layout);
    invalidate_all_samplers(&handle, &layout);
    apply_boot(&handle, &layout, &app).await;
    handle.flush_playback();

    let mut next_rotation: Option<Instant> = None;

    loop {
        // 每次醒来重读配置（热重载 / config 命令 / watcher 均落在此处生效）。
        let cfg = handle.config_manager.get_config();
        if reconcile_and_align(&handle, cfg.rotation.arrangement, &mut layout) {
            handle.flush_playback();
        }

        // 暂停抑制（Layer 2，DR-9）：pause_reasons 非空 → 不安排定时轮换。
        let paused = {
            let engine = handle.engine.lock().await;
            !engine.pause_reasons_snapshot().is_empty()
        };

        let can_time = cfg.rotation.enabled && !paused && has_rotatable_unit(&handle, &layout);

        let desired: Option<Instant> = if can_time {
            match next_rotation {
                // 保留已定案且尚未到期的 timer（sleep_until 无漂移）。
                Some(t) if t > Instant::now() => Some(t),
                _ => {
                    let t = Instant::now() + rotation_interval(&cfg);
                    next_rotation = Some(t);
                    Some(t)
                }
            }
        } else {
            next_rotation = None;
            None
        };

        let wake = handle.wake.clone();
        let notified = wake.notified();
        tokio::pin!(notified);
        let manual_fut = manual_rx.recv();
        tokio::pin!(manual_fut);

        let mut sleep: Pin<Box<dyn Future<Output = ()> + Send>> = match desired {
            Some(t) => {
                let delay = t.saturating_duration_since(Instant::now());
                Box::pin(tokio::time::sleep(delay))
            }
            None => Box::pin(std::future::pending()),
        };

        enum Why {
            Timed,
            Wake,
            Manual(Option<Option<String>>),
        }

        let why = tokio::select! {
            _ = &mut sleep => Why::Timed,
            _ = &mut notified => Why::Wake,
            m = &mut manual_fut => Why::Manual(m),
        };

        match why {
            Why::Timed => {
                // B3 / DR-38（3.2）：到点执行前重读暂停位图。若在此期间被暂停
                // （全屏 / 电池 / 托盘），跳过本轮（幂等），回到循环顶部重算 deadline
                // 并等待下次唤醒，避免暂停期误换图。
                let paused_now = {
                    let engine = handle.engine.lock().await;
                    !engine.pause_reasons_snapshot().is_empty()
                };
                if paused_now {
                    next_rotation = None;
                } else {
                    let interval = rotation_interval(&handle.config_manager.get_config());
                    next_rotation = Some(Instant::now() + interval);
                    rotate_all(&handle, &layout, &app).await;
                    handle.flush_playback();
                }
            }
            Why::Wake => {
                // config / 暂停 / 池成员 / 编排变化：整体失效重建（DR-23）+ 重算 deadline。
                invalidate_all_samplers(&handle, &layout);
                next_rotation = None;
            }
            Why::Manual(m) => {
                match m {
                    None => {
                        // 通道关闭（handle 全部 drop = 应用退出中）。
                        tracing::info!("手动'下一张'通道关闭，调度器主循环退出");
                        break;
                    }
                    Some(key) => {
                        let target = resolve_target_key(&layout, key);
                        if let Some(k) = target {
                            sample_and_apply_unit(&handle, &layout, &k, &app).await;
                        }
                        handle.flush_playback();
                    }
                }
            }
        }
    }
}

fn rotation_interval(cfg: &AppConfig) -> Duration {
    Duration::from_secs(u64::from(cfg.rotation.interval_minutes.max(1)) * 60)
}

// ── 启动对账（DR-34）与单元对齐 ─────────────────────────────────────────────

/// DR-22：计算编排切换时"来源单元 → 目标单元"的迁移对（纯函数，供单测）。
///
/// 返回 `(src_key, dst_key)`；`None` = 无需迁移。仅当跨编排形态（AllSame/Span ↔
/// PerMonitor）或主屏 id 变化时才迁移，避免 current / active_pool / 游标丢失导致
/// 开机从池重选而非延续上次壁纸。
fn resolve_migration(
    prev_arrangement: Arrangement,
    arrangement: Arrangement,
    prev_primary: Option<&str>,
    new_primary: Option<&str>,
    had_all_unit: bool,
) -> Option<(String, String)> {
    match (prev_arrangement, arrangement) {
        // all → 主屏单元
        (Arrangement::AllSame | Arrangement::Span, Arrangement::PerMonitor) => {
            if had_all_unit {
                prev_primary.map(|pk| (ALL_UNIT_KEY.to_string(), pk.to_string()))
            } else {
                None
            }
        }
        // 主屏 → all
        (Arrangement::PerMonitor, Arrangement::AllSame | Arrangement::Span) => {
            prev_primary.map(|pk| (pk.to_string(), ALL_UNIT_KEY.to_string()))
        }
        // 主屏变化：old primary → new primary
        (Arrangement::PerMonitor, Arrangement::PerMonitor) => match (prev_primary, new_primary) {
            (Some(old), Some(new)) if old != new => Some((old.to_string(), new.to_string())),
            _ => None,
        },
        _ => None,
    }
}

/// 按当前编排枚举显示器建立 / 对齐单元映射；清理引用已删壁纸 / 已删池的条目并回退；
/// 编排切换时按 DR-22 迁移主屏 / `all` 单元状态到目标单元。
fn reconcile_and_align(
    handle: &SchedulerHandle,
    arrangement: Arrangement,
    layout: &mut Layout,
) -> bool {
    // DR-22 迁移需在上次映射被覆盖前快照"来源"侧状态。
    let mut changed = false;
    let prev_arrangement = layout.arrangement;
    let prev_primary = layout.primary.clone();
    let had_all_unit = layout.unit_displays.contains_key(ALL_UNIT_KEY);

    let displays = {
        let desktop = match handle.desktop.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        desktop.enumerate_displays()
    };

    layout.arrangement = arrangement;
    layout.unit_displays.clear();
    layout.primary = None;

    let display_ids: Vec<String> = displays.iter().map(|d| d.id.clone()).collect();
    for d in &displays {
        if d.is_primary {
            layout.primary = Some(d.id.clone());
        }
    }

    match arrangement {
        Arrangement::PerMonitor => {
            for d in &displays {
                layout
                    .unit_displays
                    .insert(d.id.clone(), vec![d.id.clone()]);
            }
        }
        Arrangement::AllSame | Arrangement::Span => {
            if !display_ids.is_empty() {
                layout
                    .unit_displays
                    .insert(ALL_UNIT_KEY.to_string(), display_ids);
            }
        }
    }

    // 对齐到已建 / 新建单元，并对孤儿单元归档；清理已删池 / 已删壁纸引用（DR-34）。
    let mut play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
    let cm = &handle.config_manager;
    let current_keys: HashSet<String> = layout.unit_displays.keys().cloned().collect();
    let live_keys: Vec<String> = current_keys.iter().cloned().collect();
    for k in live_keys {
        if !play.units.contains_key(&k) {
            changed = true;
        }
        play.ensure_unit(k);
    }

    // DR-22：编排切换时迁移主屏 / all 单元状态到目标单元，避免 current 丢失导致
    // 开机从池重选而非延续。source → dst 迁移 current / active_pool / order_cursor / enabled。
    let migration = resolve_migration(
        prev_arrangement,
        arrangement,
        prev_primary.as_deref(),
        layout.primary.as_deref(),
        had_all_unit,
    );
    if let Some((src_key, dst_key)) = migration {
        if src_key != dst_key {
            changed = true;
            if let Some(src) = play.units.get(&src_key).cloned() {
                if let Some(dst) = play.units.get_mut(&dst_key) {
                    dst.current_wallpaper_id = src.current_wallpaper_id.clone();
                    dst.active_pool = src.active_pool.clone();
                    dst.order_cursor = src.order_cursor.clone();
                    dst.bag_remaining = src.bag_remaining.clone();
                    dst.enabled = src.enabled;
                }
            }
        }
    }

    // 孤儿 key（显示器已拔下 / 编排切换不再覆盖的单元）降级为非启用、保留状态备恢复。
    let orphan: Vec<String> = play
        .units
        .keys()
        .filter(|k| !current_keys.contains(*k))
        .cloned()
        .collect();
    for k in orphan {
        if let Some(u) = play.units.get_mut(&k) {
            if u.enabled {
                u.enabled = false;
                changed = true;
            }
        }
    }
    // current / active_pool / order_cursor / bag_remaining 一致性：剔除引用已删
    // 壁纸 / 已删池的条目（DR-34 / DR-35）。
    for u in play.units.values_mut() {
        if scrub_unit_refs(
            u,
            |id| cm.get_wallpaper(id).is_some(),
            |pid| cm.get_pool(pid).is_some(),
        ) {
            changed = true;
        }
    }
    changed
}

/// 单元条目的引用失效清理（纯函数，DR-34 / DR-35）：剔除引用已删壁纸 / 已删池
/// 的游标与会话残留，避免轮换关闭时残留引用原样写回 `playback.toml`。
///
/// - `current_wallpaper_id` 引用已删壁纸 → `None`；
/// - `active_pool` 引用已删池 → `None`（回退"全部"池）；
/// - `order_cursor` 引用已删壁纸 → `None`；
/// - `bag_remaining` 过滤掉已删壁纸的 id（保留仍存在的）。
///
/// 存活判定由调用方以 `wallpaper_live` / `pool_live` 闭包注入，使本函数保持纯且可单测。
fn scrub_unit_refs(
    unit: &mut Unit,
    wallpaper_live: impl Fn(&str) -> bool,
    pool_live: impl Fn(&str) -> bool,
) -> bool {
    let mut changed = false;
    if let (Some(id), true) = (
        unit.current_wallpaper_id.as_deref(),
        unit.current_wallpaper_id.is_some(),
    ) {
        if !wallpaper_live(id) {
            unit.current_wallpaper_id = None;
            changed = true;
        }
    }
    if let Some(pid) = unit.active_pool.as_deref() {
        if !pid.is_empty() && !pool_live(pid) {
            // 指向已删池 → 回退"全部"池（None）
            unit.active_pool = None;
            changed = true;
        }
    }
    // DR-34 对账扩展：order_cursor / bag_remaining 中引用已删壁纸的残留一并清理。
    if let Some(id) = unit.order_cursor.as_deref() {
        if !wallpaper_live(id) {
            unit.order_cursor = None;
            changed = true;
        }
    }
    let old_len = unit.bag_remaining.len();
    unit.bag_remaining.retain(|id| wallpaper_live(id));
    changed || unit.bag_remaining.len() != old_len
}

/// 失效重建所有单元的采样器（DR-23）：清袋，游标锚定到当前壁纸。
fn invalidate_all_samplers(handle: &SchedulerHandle, layout: &Layout) {
    let mut play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
    for key in layout.unit_keys() {
        let unit = play.ensure_unit(key);
        let mut sampler = SamplerState {
            order_cursor: unit.order_cursor.clone(),
            bag_remaining: unit.bag_remaining.clone(),
        };
        invalidate_sampler(&mut sampler, unit.current_wallpaper_id.as_deref());
        unit.order_cursor = sampler.order_cursor;
        unit.bag_remaining = sampler.bag_remaining;
    }
}

// ── 开机 / 唤醒解析（一次 apply，DR-12 / DR-18 / DR-21）─────────────────────

async fn apply_boot(handle: &SchedulerHandle, layout: &Layout, app: &tauri::AppHandle) {
    let cfg = handle.config_manager.get_config();
    for key in layout.unit_keys() {
        let unit = handle
            .playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .units
            .get(&key)
            .cloned();
        let Some(unit) = unit else { continue };

        let count = pool_count(&handle.config_manager, unit.active_pool.as_deref());
        // 过滤后 ≥1 / ≥2 用 pool_count（已剔除 Web 与缺文件）。
        let pool_ge_two = count >= 2;
        let pool_ge_one = count >= 1;

        let current_exists = unit
            .current_wallpaper_id
            .as_ref()
            .is_some_and(|id| handle.config_manager.get_wallpaper(id).is_some());
        let current_is_web = unit
            .current_wallpaper_id
            .as_ref()
            .and_then(|id| handle.config_manager.get_wallpaper(id))
            .is_some_and(|e| e.wallpaper_type == WallpaperType::Web);

        let decision = resolve_boot(
            current_exists,
            unit.current_wallpaper_id.as_deref(),
            current_is_web,
            cfg.rotation.enabled,
            cfg.rotation.on_boot,
            unit.enabled,
            if pool_ge_two {
                2
            } else if pool_ge_one {
                1
            } else {
                0
            },
        );

        match decision {
            BootDecision::Restore(id) => {
                if apply_wallpaper_to_unit(handle, layout, &key, &id, app).await {
                    emit_rotated(app, &key, &id);
                }
            }
            BootDecision::Advance => {
                // sample_and_apply_unit 内部 emit rotated；此处无需利用返回值。
                sample_and_apply_unit(handle, layout, &key, app).await;
            }
            BootDecision::None => {}
        }
    }
}

// ── 轮换执行 ───────────────────────────────────────────────────────────────

async fn rotate_all(handle: &SchedulerHandle, layout: &Layout, app: &tauri::AppHandle) {
    for key in layout.unit_keys() {
        if unit_is_rotatable(handle, layout, &key) {
            sample_and_apply_unit(handle, layout, &key, app).await;
        }
    }
}

/// 单单元轮换：采样 → apply（原子交换 / 回退）→ 更新 current → emit。
/// 返回被换到的壁纸 id；返回 `None` 表示无可换（池 <2 / 采样返回 None / apply 失败）。
async fn sample_and_apply_unit(
    handle: &SchedulerHandle,
    layout: &Layout,
    key: &str,
    app: &tauri::AppHandle,
) -> Option<String> {
    let (order, active_pool, cursor, bag) = {
        let mut play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
        let unit = play.ensure_unit(key.to_string());
        (
            handle.config_manager.get_config().rotation.order,
            unit.active_pool.clone(),
            unit.order_cursor.clone(),
            unit.bag_remaining.clone(),
        )
    };

    let mut sampler = SamplerState {
        order_cursor: cursor,
        bag_remaining: bag,
    };
    let entries = build_pool_entries(&handle.config_manager, active_pool.as_deref());
    let filtered = filter_candidates(entries.iter().collect());
    let target = sample_next(order, &filtered, &mut sampler);

    // 回写采样状态（无论 target，游标 / 袋已推进——DR-19 失败时游标跳过防原地卡死）。
    {
        let mut play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
        let unit = play.ensure_unit(key.to_string());
        unit.order_cursor = sampler.order_cursor;
        unit.bag_remaining = sampler.bag_remaining;
    }

    let target = target?;
    if apply_wallpaper_to_unit(handle, layout, key, &target, app).await {
        emit_rotated(app, key, &target);
        Some(target)
    } else {
        None
    }
}

/// 轮换 apply 的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyOutcome {
    /// 已换槽成功（旋转生效，emit rotated）
    Swapped,
    /// 同目标短路（新图 == 旧图，未换）
    Skipped,
    /// 该显示器正在切换（手动 set_wallpaper / 另一轮换占用），静默跳过
    Busy,
}

/// 把指定壁纸应用到单元覆盖的所有显示器（AllSame / Span 顺序逐屏；PerMonitor 单屏）。
/// 任一屏成功即更新单元 current 并返回 `true`（DR-19：单屏失败记录，其余继续）。
async fn apply_wallpaper_to_unit(
    handle: &SchedulerHandle,
    layout: &Layout,
    key: &str,
    wallpaper_id: &str,
    app: &tauri::AppHandle,
) -> bool {
    let entry = match handle.config_manager.get_wallpaper(wallpaper_id) {
        Some(e) => e,
        None => {
            tracing::warn!(key, wallpaper_id, "轮换目标壁纸已不存在，跳过");
            return false;
        }
    };
    let source = WallpaperSource::File(entry.file_path.clone());
    let ty = entry.wallpaper_type;

    let displays = layout.unit_displays.get(key).cloned().unwrap_or_default();
    if displays.is_empty() {
        return false;
    }

    let mut any_ok = false;
    for disp in &displays {
        let scaling = {
            let engine = handle.engine.lock().await;
            engine.scaling_mode_for(disp) // DR-25：继承该屏当前缩放模式
        };
        match apply_to_display(&handle.engine, disp, source.clone(), ty, scaling).await {
            Ok(ApplyOutcome::Swapped) => {
                any_ok = true;
                let _ = app.emit("wallpaper-state-changed", disp.clone());
            }
            Ok(ApplyOutcome::Skipped) | Ok(ApplyOutcome::Busy) => {
                // 未换（同目标短路 / 显示器正在切换）：不 emit rotated、不算错误。
            }
            Err(e) => {
                // DR-19 skip-broken：保持旧壁纸，游标已推进。
                tracing::warn!(key, display = %disp, error = %e, "轮换 apply 失败（保持旧壁纸、游标推进跳过，DR-19）");
            }
        }
    }

    if any_ok {
        let mut play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(u) = play.units.get_mut(key) {
            u.current_wallpaper_id = Some(wallpaper_id.to_string());
        }
    }
    any_ok
}

/// 原子交换 apply（设计 §8.4，方案 C，DR-33 / DR-40）。
///
/// 三段式：A `embed_atomic_into`（锁内嵌入 + loadfile）→ B `wait_new_ready`
/// （锁外首帧就绪）→ C `commit_atomic_swap`（锁内换槽）。B2：入口即为 `display_id`
/// acquire per-display guard 并贯穿 A→B→C 全程持有，防轮换与手动/另一轮换同屏并发。
///
/// 返回 `ApplyOutcome`：`Swapped` 已换槽、`Skipped` 同目标短路（未换）、`Busy` 该屏
/// 正在切换（静默跳过）；`Err` 表示失败（旧壁纸原封不动，零回滚）。
async fn apply_to_display(
    engine: &Arc<tokio::sync::Mutex<WallpaperEngine>>,
    display_id: &str,
    source: WallpaperSource,
    wallpaper_type: WallpaperType,
    scaling_mode: ScalingMode,
) -> Result<ApplyOutcome, MirrorStarError> {
    // B2：贯穿 A→B→C 全程持有 per-display guard。acquire 失败说明该屏 busy
    //（手动 set_wallpaper / 另一轮换进行中）→ 静默跳过该显示器，不算错误。
    let _guard = match DisplaySettingGuard::acquire(display_id.to_string()) {
        Ok(guard) => guard,
        Err(_) => {
            tracing::info!(display_id, "轮换跳过：显示器正在切换");
            return Ok(ApplyOutcome::Busy);
        }
    };

    // 阶段 1（锁内，廉价）：快照配置，旧壁纸保持占位；Native / 无旧窗 → Fallback。
    let pending = {
        let mut eng = engine.lock().await;
        eng.prepare_atomic_swap(display_id, &source, wallpaper_type)
    };

    match pending {
        AtomicSwapPrepare::Fallback { .. } => {
            // 原生图片或暂无旧窗 → 回退现有同步 set_wallpaper（先关后设，DR-33 注）。
            let mut eng = engine.lock().await;
            eng.set_wallpaper(display_id, &source, wallpaper_type, scaling_mode)?;
            Ok(ApplyOutcome::Swapped)
        }
        AtomicSwapPrepare::Swap(pending) => {
            let commit_source = source.clone();
            // 阶段 2（锁外）：build_new_renderer（不持引擎锁）。
            let built =
                tokio::task::spawn_blocking(move || -> Result<BuildOutcome, MirrorStarError> {
                    build_new_renderer(&source, wallpaper_type, scaling_mode, &pending)
                })
                .await
                .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {e}")))?;

            match built {
                Ok(BuildOutcome::Skip) => {
                    // 新图与旧图相同 → 短路，无需换槽。
                    Ok(ApplyOutcome::Skipped)
                }
                Ok(BuildOutcome::Ready(renderer)) => {
                    // 阶段 A（锁内）：嵌入 + after_embed（loadfile）。旧壁纸保持占位。
                    let mut renderer = {
                        let mut eng = engine.lock().await;
                        eng.embed_atomic_into(renderer, display_id)?
                    };
                    // 阶段 B（锁外，不持引擎锁）：首帧就绪（视频等待加载）。
                    let ready = tokio::task::spawn_blocking(
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
                    .map_err(|e| MirrorStarError::TaskJoin(format!("任务 join 失败: {e}")))?;
                    let (ready_result, mut renderer) = ready;
                    if let Err(e) = ready_result {
                        let _ = renderer.terminate();
                        return Err(e);
                    }
                    // 阶段 C（锁内）：一次换槽 old→new + 补发暂停（DR-20）+ terminate 旧。
                    let mut eng = engine.lock().await;
                    match eng.commit_atomic_swap(
                        renderer,
                        display_id,
                        &commit_source,
                        wallpaper_type,
                    )? {
                        SwapOutcome::Committed => Ok(ApplyOutcome::Swapped),
                    }
                }
                Err(e) => Err(e),
            }
        }
    }
}

fn resolve_target_key(layout: &Layout, key: Option<String>) -> Option<String> {
    match layout.arrangement {
        Arrangement::AllSame | Arrangement::Span => {
            if layout.unit_displays.contains_key(ALL_UNIT_KEY) {
                Some(ALL_UNIT_KEY.to_string())
            } else {
                None
            }
        }
        Arrangement::PerMonitor => {
            let k = match key {
                Some(k) => k,
                None => layout.primary.clone()?,
            };
            if layout.unit_displays.contains_key(&k) {
                Some(k)
            } else {
                tracing::warn!(key = %k, "手动'下一张'目标单元不存在（显示器已拔下？），忽略");
                None
            }
        }
    }
}

fn unit_is_rotatable(handle: &SchedulerHandle, _layout: &Layout, key: &str) -> bool {
    let play = handle.playback.lock().unwrap_or_else(|e| e.into_inner());
    let Some(unit) = play.units.get(key) else {
        return false;
    };
    if !unit.enabled {
        return false;
    }
    // 池过滤后 ≥2 张才参与轮换（DR-14）。
    pool_count(&handle.config_manager, unit.active_pool.as_deref()) >= 2
}

fn has_rotatable_unit(handle: &SchedulerHandle, layout: &Layout) -> bool {
    layout
        .unit_keys()
        .into_iter()
        .any(|k| unit_is_rotatable(handle, layout, &k))
}

fn emit_rotated(app: &tauri::AppHandle, key: &str, wallpaper_id: &str) {
    if let Err(e) = app.emit(
        "wallpaper-rotated",
        RotatedPayload {
            key: key.to_string(),
            wallpaper_id: wallpaper_id.to_string(),
        },
    ) {
        tracing::warn!(error = %e, "emit wallpaper-rotated 失败（DR-30）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造布局：PerMonitor 时 keys 即各显示器单元（primary 指向其中一个）；
    /// AllSame / Span 时以 ALL_UNIT_KEY 承载全部，primary 仅作默认值。
    fn layout_with(arrangement: Arrangement, keys: &[&str], primary: Option<&str>) -> Layout {
        let mut l = Layout::new();
        l.arrangement = arrangement;
        for k in keys {
            l.unit_displays
                .insert((*k).to_string(), vec![(*k).to_string()]);
        }
        l.primary = primary.map(str::to_owned);
        l
    }

    // ── resolve_target_key（手动"下一张"目标解析）─────────────────────────

    #[test]
    fn target_all_same_always_returns_all_when_unit_exists() {
        let l = layout_with(Arrangement::AllSame, &[ALL_UNIT_KEY], None);
        // AllSame 下无论 key 为何均回退全局单元
        assert_eq!(
            resolve_target_key(&l, Some("any".to_string())).as_deref(),
            Some(ALL_UNIT_KEY)
        );
        assert_eq!(resolve_target_key(&l, None).as_deref(), Some(ALL_UNIT_KEY));
    }

    #[test]
    fn target_span_returns_none_when_all_unit_absent() {
        // Span 但无显示器（unit_displays 无 all）→ 无有效目标
        let l = layout_with(Arrangement::Span, &[], None);
        assert_eq!(resolve_target_key(&l, None), None);
    }

    #[test]
    fn target_per_monitor_defaults_to_primary() {
        let l = layout_with(Arrangement::PerMonitor, &["a", "b"], Some("a"));
        assert_eq!(resolve_target_key(&l, None).as_deref(), Some("a"));
    }

    #[test]
    fn target_per_monitor_uses_explicit_key() {
        let l = layout_with(Arrangement::PerMonitor, &["a", "b"], Some("a"));
        assert_eq!(
            resolve_target_key(&l, Some("b".to_string())).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn target_per_monitor_unknown_key_ignored() {
        let l = layout_with(Arrangement::PerMonitor, &["a", "b"], Some("a"));
        assert_eq!(resolve_target_key(&l, Some("ghost".to_string())), None);
    }

    // ── resolve_migration（DR-22 编排切换迁移）────────────────────────────

    #[test]
    fn migration_all_same_to_per_monitor_moves_to_primary() {
        // all → 主屏单元
        assert_eq!(
            resolve_migration(
                Arrangement::AllSame,
                Arrangement::PerMonitor,
                Some("m1"),
                Some("m1"),
                true
            ),
            Some((ALL_UNIT_KEY.to_string(), "m1".to_string()))
        );
    }

    #[test]
    fn migration_all_same_to_per_monitor_no_primary_no_move() {
        // had_all_unit 为 false → 无迁移
        assert_eq!(
            resolve_migration(
                Arrangement::AllSame,
                Arrangement::PerMonitor,
                Some("m1"),
                Some("m1"),
                false
            ),
            None
        );
    }

    #[test]
    fn migration_per_monitor_to_all_same_moves_to_all() {
        // 主屏 → all
        assert_eq!(
            resolve_migration(
                Arrangement::PerMonitor,
                Arrangement::Span,
                Some("m2"),
                None,
                false
            ),
            Some(("m2".to_string(), ALL_UNIT_KEY.to_string()))
        );
    }

    #[test]
    fn migration_primary_change_within_per_monitor() {
        // 主屏由 m1 变为 m2 → 迁移 m1→m2
        assert_eq!(
            resolve_migration(
                Arrangement::PerMonitor,
                Arrangement::PerMonitor,
                Some("m1"),
                Some("m2"),
                false
            ),
            Some(("m1".to_string(), "m2".to_string()))
        );
    }

    #[test]
    fn migration_same_primary_no_move() {
        assert_eq!(
            resolve_migration(
                Arrangement::PerMonitor,
                Arrangement::PerMonitor,
                Some("m1"),
                Some("m1"),
                false
            ),
            None
        );
        assert_eq!(
            resolve_migration(
                Arrangement::PerMonitor,
                Arrangement::PerMonitor,
                None,
                None,
                false
            ),
            None
        );
    }

    #[test]
    fn migration_span_to_span_never_moves() {
        // 同形态（Span → Span）无迁移
        assert_eq!(
            resolve_migration(
                Arrangement::Span,
                Arrangement::Span,
                Some("m1"),
                Some("m1"),
                true
            ),
            None
        );
    }

    // ── scrub_unit_refs（DR-34 引用失效清理）──────────────────────────────

    #[test]
    fn scrub_clears_dangling_current_and_cursor_and_filters_bag() {
        let mut unit = Unit {
            key: "all".to_string(),
            current_wallpaper_id: Some("gone".to_string()),
            active_pool: Some("pool-ok".to_string()),
            order_cursor: Some("gone".to_string()),
            bag_remaining: vec!["alive".to_string(), "gone".to_string(), "gone2".to_string()],
            enabled: true,
        };
        let live = |id: &str| id == "alive" || id == "pool-ok"; // 壁纸与池同集判断
        assert!(scrub_unit_refs(&mut unit, live, live));
        assert_eq!(unit.current_wallpaper_id, None);
        assert_eq!(unit.order_cursor, None);
        assert_eq!(unit.bag_remaining, vec!["alive".to_string()]);
        assert_eq!(unit.active_pool.as_deref(), Some("pool-ok"));
    }

    #[test]
    fn scrub_keeps_live_cursor_and_drops_deleted_pool() {
        let mut unit = Unit {
            key: "all".to_string(),
            current_wallpaper_id: Some("alive".to_string()),
            active_pool: Some("deleted-pool".to_string()),
            order_cursor: Some("alive".to_string()),
            bag_remaining: vec!["alive".to_string()],
            enabled: true,
        };
        let paper_live = |id: &str| id == "alive";
        let pool_live = |id: &str| id != "deleted-pool";
        assert!(scrub_unit_refs(&mut unit, paper_live, pool_live));
        assert_eq!(unit.current_wallpaper_id.as_deref(), Some("alive"));
        assert_eq!(unit.order_cursor.as_deref(), Some("alive"));
        assert_eq!(unit.bag_remaining, vec!["alive".to_string()]);
        assert_eq!(unit.active_pool, None);
    }

    #[test]
    fn scrub_noop_returns_false() {
        let mut unit = Unit {
            key: "all".to_string(),
            current_wallpaper_id: Some("alive".to_string()),
            active_pool: Some("pool".to_string()),
            order_cursor: Some("alive".to_string()),
            bag_remaining: vec!["alive".to_string()],
            enabled: true,
        };
        let live = |id: &str| id == "alive" || id == "pool";
        // 无任何残留需清理 → 返回 false
        assert!(!scrub_unit_refs(&mut unit, live, live));
        assert_eq!(unit.current_wallpaper_id.as_deref(), Some("alive"));
        assert_eq!(unit.active_pool.as_deref(), Some("pool"));
        assert_eq!(unit.order_cursor.as_deref(), Some("alive"));
        assert_eq!(unit.bag_remaining, vec!["alive".to_string()]);
    }

    // ── enable_layout_units（全局 rotation.enabled false→true 联动）────────────

    /// 构造含两张真实图片壁纸的调度句柄（供 has_rotatable_unit / unit_is_rotatable
    /// 集成断言）。返回的 TempDir 保持文件存活至测试结束。
    fn handle_with_two_wallpapers() -> (Arc<SchedulerHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir 创建失败");
        let cm = Arc::new(
            ConfigManager::new_in_dir(dir.path().to_path_buf()).expect("配置管理器创建失败"),
        );
        // 两张真实存在的图片文件（满足 filter_candidates 的"非 Web + 文件存在"条件）。
        for (id, name) in [("w1", "w1.jpg"), ("w2", "w2.jpg")] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"test").expect("写测试壁纸文件");
            cm.add_wallpaper(mirrorstar_core::config::WallpaperEntry {
                id: id.to_string(),
                file_path: path.to_string_lossy().into_owned(),
                wallpaper_type: WallpaperType::Image,
                display_id: None,
                added_at: "0".to_string(),
                thumbnail: String::new(),
                file_size: 4,
                metadata: None,
                groups: Vec::new(),
                normalized_path: String::new(),
            })
            .expect("添加测试壁纸");
        }
        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume = Arc::new(Mutex::new(mirrorstar_core::VolumeControl::new_disabled()));
        let engine = Arc::new(tokio::sync::Mutex::new(WallpaperEngine::new(
            desktop.clone(),
            volume,
        )));
        (SchedulerHandle::new(cm, engine, desktop), dir)
    }

    #[test]
    fn set_layout_units_enabled_enables_all_keys() {
        let mut state = PlaybackState::default();
        state.ensure_unit("b".to_string()); // 预置一个关闭单元
        assert!(!state.units["b"].enabled);
        // 布局启用：不存在的新建启用、已存在关闭的重新启用（无副作用：不落盘/不唤醒）。
        set_layout_units_enabled(&mut state, &["a".to_string(), "b".to_string()]);
        assert_eq!(state.units.len(), 2);
        assert!(state.units["a"].enabled);
        assert!(state.units["b"].enabled);
    }

    #[test]
    fn layout_units_enabled_yields_rotatable_and_disabled_unit_excluded() {
        let (handle, _dir) = handle_with_two_wallpapers();
        // 清空可能来自 data_root 的残留，隔离测试状态（只读，不落盘）。
        handle.playback.lock().unwrap().units.clear();

        let layout = layout_with(Arrangement::PerMonitor, &["a", "b"], Some("a"));
        // 单元默认全关 → 无可轮换单元。
        assert!(!has_rotatable_unit(&handle, &layout));

        // 联动启用两个布局单元 → 池 ≥2 张时判定可轮换。
        {
            let mut play = handle.playback.lock().unwrap();
            set_layout_units_enabled(&mut play, &["a".to_string(), "b".to_string()]);
        }
        assert!(has_rotatable_unit(&handle, &layout));
        assert!(unit_is_rotatable(&handle, &layout, "a"));
        assert!(unit_is_rotatable(&handle, &layout, "b"));

        // 单独关闭 b → 该单元不参与；其余开启单元（a）仍参与 → 全局仍可触发。
        {
            let mut play = handle.playback.lock().unwrap();
            play.units.get_mut("b").unwrap().enabled = false;
        }
        assert!(
            !unit_is_rotatable(&handle, &layout, "b"),
            "被关闭单元不应参与轮换"
        );
        assert!(
            unit_is_rotatable(&handle, &layout, "a"),
            "其它开启单元仍应参与"
        );
        assert!(has_rotatable_unit(&handle, &layout));

        // 全关 → 皆不参与，全局不再可触发。
        {
            let mut play = handle.playback.lock().unwrap();
            play.units.get_mut("a").unwrap().enabled = false;
        }
        assert!(!has_rotatable_unit(&handle, &layout));
        assert!(!unit_is_rotatable(&handle, &layout, "a"));
        assert!(!unit_is_rotatable(&handle, &layout, "b"));
    }
}
