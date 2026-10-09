//! 视频壁纸卡帧常驻看护任务（src-tauri 侧，实现 spec D4/D6）。
//!
//! # 模块职责
//!
//! 以**低开销周期任务**看护"应为播放"的视频壁纸：每 [`LIVENESS_WATCH_INTERVAL`] 采样一次
//! `time-pos`，若连续两轮采样时钟未前进且未暂停，则判定渲染器输出停滞并复用 core 既有
//! 原语 [`WallpaperEngine::heal_stalled_renderer`] 分级自愈（`terminate()` →
//! `revive_terminated()` 重建渲染器）。
//!
//! # 为什么需要"常驻看护"（而不是只在退出纯净壁纸模式时校验）
//!
//! 实测根因表明 mpv 视频输出可能在**任意时刻**停滞（本次是 `--loop-file` 循环点的
//! "最后一帧 drain"未完成，约 10s 才自行恢复，且无任何保证）。退出编排内的即时校验
//! （另一处改动）只覆盖"退出纯净壁纸模式"这一个时间点；真正要兜底的是"任意时刻的
//! 壁纸冻结"。两者互补：
//!
//! - 退出编排校验：模式退出瞬间的强一致兜底（用户此刻正在看着壁纸）；
//! - 常驻看护：覆盖模式外的长时运行（用户没盯着看时冻结，也会被静默自愈）。
//!
//! # 判据与"确认采样"（消除周期混叠误报）
//!
//! 单周期的停滞判据是"`time-pos` 未前进且未暂停"（[`video_playback_stalled`]）。它存在
//! **周期混叠**风险：当视频循环时长**整除**采样周期（5s / 2.5s / 1.25s …）且两次采样抖动
//! < 1ms 时，`|Δtime_pos| < 1ms` 会偶然成立 → 误判停滞并自愈（用户看到壁纸莫名重启）。
//!
//! 故在"首段疑似停滞"后追加一次 [`CONFIRM_INTERVAL`]（1.3s）采样：1.3s 与 5s **互不整除**
//! （5 / 1.3 ≈ 3.85），对任意整除 5s 的循环时长，该区间上必然出现相位差（时钟恢复前进），
//! 从而证伪混叠。仅当**两段区间都"钉住"**（[`confirmed_stalls`]）才允许自愈；确认区间时钟
//! 前进则记 debug 并放弃自愈。
//!
//! # 为什么有这些跳过条件（顺序即判定顺序，任一命中即本轮不动作且**不更新采样基线**）
//!
//! 0. `crate::state::SHUTDOWN_DONE`：应用退出清理已开始 → 停止一切采样与自愈，避免与
//!    `engine.shutdown()` 竞争、避免 spawn 孤儿 mpv（F2）。
//! 1. `mirrorstar_core::pure_mode::is_active()`：模式内画面由"提升到最前"的窗口呈现，
//!    且退出编排会即时校验；模式内采样/自愈会与置顶编排竞争，故跳过。
//! 2. [`crate::state::atomic_swap_in_progress`]：轮换原子交换中渲染器正在换槽，探针会读到
//!    过渡态（旧渲染器 Terminated / 新渲染器未就绪），据此判定会误自愈。
//! 3. `pause_reasons_snapshot()` 非空：暂停态（用户/全屏/电池）下时钟本就不前进，属预期，
//!    绝不能判为停滞（spec D6「用户主动暂停 ⇒ 永不判定」）。
//! 4. `try_lock` 失败：engine 锁忙（如正在 `set_wallpaper`），下一轮再试，**不阻塞**。
//!
//! 条件 1/2 同时在**锁外前置判定**与**锁内复核**两处执行：锁外前置判定让常见跳过路径
//! 不产生一次 `spawn_blocking`；锁内复核用于收口 TOCTOU——锁外读到"非模式、非换槽"后，
//! 取锁前模式可能刚进入（窗口已摘出置顶）、轮换可能刚进入原子交换（渲染器正在换槽），
//! 此时自愈会重建已置顶窗口 / `terminate` 换槽中的渲染器。故取得引擎锁后必须再判一次，
//! 命中即 `return None`（不判定、不自愈、不更新基线）。
//!
//! **不更新基线**是正确性关键：跳过轮次若把"当前（可能已冻结的）快照"写入基线，则
//! "暂停 5s 后恢复"会在恢复后的首轮因基线过期而误判（把暂停期间的静止当成停滞）。
//! 保持基线不变 ⇒ 判定始终比较"两个都真实在播放的时刻"，暂停/锁忙/模式切换都不会污染判定。
//!
//! # 锁与阻塞
//!
//! `probe_playing_videos` 会做 mpv IPC 查询（探针已瘦身为 4 属性 × 500ms 超时，最坏约 2s）、
//! `heal_stalled_renderer` 会重建渲染器（含进程启停），均为阻塞调用。故"采样 → 判定 → 自愈"
//! 整体放在 `tokio::task::spawn_blocking` 内，且用**非阻塞的 `try_lock`** 获取 engine 锁：
//! **采样与自愈在锁内，但确认采样的 1.3s `sleep` 在锁外**（先释放锁再等待，避免长时间占锁
//! 阻塞 UI 命令与退出清理），绝不阻塞 tokio 工作线程与主线程。
//!
//! # 退出处理（spec D6）
//!
//! 应用退出清理开始（`SHUTDOWN_DONE` 置位）后，本任务在下一轮采样时命中跳过条件 0，
//! 停止一切采样与自愈，不再 spawn/重建任何 mpv。任务本身仍**随 runtime 结束**、不做显式
//! 停止/join：它是纯只读看护，不持有 desktop 锁等 shutdown 关键资源，且 engine 锁用
//! `try_lock` 获取（锁忙即跳过），不会像 `workerw_check` 那样持锁阻塞 `engine.shutdown()`。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use mirrorstar_core::{video_playback_stalled, LivenessProbe, WallpaperEngine};

/// 看护采样间隔（spec D4）。
const LIVENESS_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// 疑似停滞时的**确认采样**等待（spec：消除周期混叠误报）。
///
/// 必须与 [`LIVENESS_WATCH_INTERVAL`] 构成**非整除偏移**：5s 周期的停滞判据会被"循环时长
/// 整除 5s（5s / 2.5s / 1.25s …）+ 采样抖动 < 1ms"的周期混叠欺骗（`|Δtime_pos| < 1ms`
/// 偶然成立）。1.3s 与 5s 互不整除，对任意整除 5s 的循环时长该区间上必然出现相位差，可证伪混叠。
const CONFIRM_INTERVAL: Duration = Duration::from_millis(1300);

/// 单显示器自愈冷却时间：冷却期内再次判定停滞只记日志、不重复自愈，避免重启风暴。
const HEAL_COOLDOWN: Duration = Duration::from_secs(60);

/// 各显示器上次自愈时间戳（display_id → Instant）。
///
/// 用 `LazyLock` 包裹（`HashMap::new()` 非 const fn，无法直接初始化 static）；
/// 访问仅发生在 `spawn_blocking` 闭包内、持锁极短，无跨 `.await` 持锁问题。
static LAST_HEAL_TS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 启动视频壁纸卡帧常驻看护任务。
///
/// 返回 `Option<JoinHandle<()>>`：正常启动返回 `Some(handle)`（便于测试/未来按需管理），
/// spawn 不可用时为 `None`。调用方**丢弃**该句柄即"后台运行"（drop `JoinHandle` 会 detach
/// 任务而非取消），符合"随 runtime 结束"的退出策略（见模块文档）。
///
/// 扫描间隔 [`LIVENESS_WATCH_INTERVAL`]，采样/判定/自愈在 `spawn_blocking` 内完成；
/// 跳过条件命中时本轮不动作且不更新基线（见模块文档）。
pub(crate) fn start_liveness_watchdog(
    engine: Arc<tokio::sync::Mutex<WallpaperEngine>>,
) -> Option<tauri::async_runtime::JoinHandle<()>> {
    let handle = tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(LIVENESS_WATCH_INTERVAL);
        // 跳过累积的错过 tick（任务被长时间挂起后不补跑多次采样）
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 采样基线（上一轮"完整完成采样"的快照）；跳过轮次保持不变。
        let mut prev: HashMap<String, LivenessProbe> = HashMap::new();

        loop {
            interval.tick().await;

            // 跳过条件 0：应用退出清理已开始（不再采样/自愈，避免与 engine.shutdown() 竞争
            // 与 spawn 孤儿 mpv）；命中即跳过本轮，**不更新基线**。
            if crate::state::SHUTDOWN_DONE.load(Ordering::Acquire) {
                tracing::debug!("存活看护：应用退出清理已开始，跳过本轮");
                continue;
            }
            // 跳过条件 1：纯净壁纸模式运行中（画面由提升窗口呈现，退出编排会即时校验）
            if mirrorstar_core::pure_mode::is_active() {
                tracing::debug!("存活看护：纯净壁纸模式运行中，跳过本轮（保持基线不变）");
                continue;
            }
            // 跳过条件 2：轮换原子交换进行中（渲染器正在换槽，探针会读到过渡态）
            if crate::state::atomic_swap_in_progress() {
                tracing::debug!("存活看护：轮换原子交换进行中，跳过本轮（保持基线不变）");
                continue;
            }

            // 采样基线随本轮一起移入阻塞闭包；闭包返回 None 表示本轮跳过（基线保持不变）。
            let baseline = prev.clone();
            let engine_for_round = engine.clone();
            let outcome =
                tokio::task::spawn_blocking(move || watch_once(&engine_for_round, &baseline)).await;

            match outcome {
                // 完成采样：用本轮快照整体替换基线
                Ok(Some(snapshot)) => prev = snapshot,
                // 跳过轮次（锁忙 / 暂停位图非空）：基线保持不变
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "存活看护：采样任务 join 失败，跳过本轮"),
            }
        }
    });
    Some(handle)
}

/// 单轮"分段采样 + 确认判定 + 自愈"（在 `spawn_blocking` 线程内执行，engine 锁仅在采样/自愈
/// 的两小段内持有，**确认采样的 1.3s 等待在锁外**）。
///
/// # 返回值
///
/// - `Some(snapshot)`：**完整完成采样**——调用方据此用 `snapshot` 替换采样基线（若进入确认
///   步骤则用确认快照，否则用首段快照）；
/// - `None`：本轮命中跳过条件（退出中 / 模式 / 换槽 / 锁忙 / 暂停位图非空），调用方**保持
///   基线不变**。
///
/// 跳过条件 3/4（暂停位图 / 锁忙）在此处判定：`pause_reasons_snapshot()` 需要 engine 锁，
/// 故先 `try_lock`（失败即"锁忙"跳过），成功后再读暂停位图。两者行为一致——都不动作、
/// 都不更新基线，区别仅在日志原因。
fn watch_once(
    engine: &Arc<tokio::sync::Mutex<WallpaperEngine>>,
    prev: &HashMap<String, LivenessProbe>,
) -> Option<HashMap<String, LivenessProbe>> {
    // ── 第一段：取锁采样（不判定、不自愈） ────────────────────────────────────
    // 用作用域限定让 `guard` 在采样完成后立即释放：随后的疑似判定与确认等待都在**不持锁**
    // 状态下进行，锁不会跨越 1.3s 的等待（否则会长时间阻塞 UI 命令与退出清理）。
    let probes = {
        // 跳过条件 4：engine 锁忙（try_lock 非阻塞，下一轮再试）
        let mut guard = match engine.try_lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::debug!("存活看护：engine 锁忙，跳过本轮（保持基线不变）");
                return None;
            }
        };
        // 锁内复核（TOCTOU 收口）：锁外判定的跳过条件在"取锁前"可能已被改变——
        // 纯净壁纸模式刚进入（窗口已摘出置顶，此时自愈会重建置顶窗并让模式退出回嵌悬空）、
        // 轮换刚进入原子交换（此时自愈会 terminate 换槽中的渲染器）。
        if mirrorstar_core::pure_mode::is_active() {
            tracing::debug!("存活看护：取锁时纯净壁纸模式已激活，跳过本轮（保持基线不变）");
            return None;
        }
        if crate::state::atomic_swap_in_progress() {
            tracing::debug!("存活看护：取锁时轮换原子交换已开始，跳过本轮（保持基线不变）");
            return None;
        }
        // 跳过条件 3：暂停位图非空（用户/全屏/电池暂停）时时钟本就不前进，属预期
        if !guard.pause_reasons_snapshot().is_empty() {
            tracing::debug!("存活看护：存在暂停原因，跳过本轮（保持基线不变）");
            return None;
        }
        // 采样：仅 `state() == Playing` 且探针可用的视频渲染器（display_id 升序）
        guard.probe_playing_videos()
    };

    // 首段判定：找出疑似停滞的显示器（相对基线）。无疑似即本轮完成，直接返回首段快照。
    let suspects = stalled_display_ids(prev, &probes);
    if suspects.is_empty() {
        return Some(probes.into_iter().collect());
    }

    // ── 锁外确认等待 ─────────────────────────────────────────────────────────
    // 疑似停滞时等待非整除偏移的 CONFIRM_INTERVAL 再采一次确认快照；此 `sleep` **不持
    // engine 锁**（上一段的 guard 已释放），避免长时间占锁。
    tracing::debug!(
        suspect_count = suspects.len(),
        "存活看护：首段疑似停滞，锁外等待确认采样（不持引擎锁）"
    );
    std::thread::sleep(CONFIRM_INTERVAL);

    // ── 第二段：取锁复核 + 确认采样 + 自愈 ───────────────────────────────────
    let mut guard = match engine.try_lock() {
        Ok(g) => g,
        Err(_) => {
            tracing::debug!("存活看护：确认采样时 engine 锁忙，跳过本轮（保持基线不变）");
            return None;
        }
    };
    // 复核：等待期间状态可能已变，任一命中即放弃本轮（不判定、不自愈、不更新基线）。
    if crate::state::SHUTDOWN_DONE.load(Ordering::Acquire) {
        tracing::debug!("存活看护：应用退出清理已开始，跳过本轮（保持基线不变）");
        return None;
    }
    if mirrorstar_core::pure_mode::is_active() {
        tracing::debug!("存活看护：确认采样时纯净壁纸模式已激活，跳过本轮（保持基线不变）");
        return None;
    }
    if crate::state::atomic_swap_in_progress() {
        tracing::debug!("存活看护：确认采样时轮换原子交换已开始，跳过本轮（保持基线不变）");
        return None;
    }
    if !guard.pause_reasons_snapshot().is_empty() {
        tracing::debug!("存活看护：确认采样时存在暂停原因，跳过本轮（保持基线不变）");
        return None;
    }
    let confirm = guard.probe_playing_videos();

    // 确认判定（纯函数）：仅"首段疑似 + 确认区间同样钉住"才保留，证伪周期混叠。
    let confirmed = confirmed_stalls(suspects, &confirm);

    // 逐屏独立自愈（单屏失败不影响其它屏）
    let now = Instant::now();
    for (display_id, prev_time_pos, curr_time_pos) in confirmed {
        // 冷却期内命中停滞：只记 warn，不重复自愈
        if !heal_allowed(last_heal(&display_id), now, HEAL_COOLDOWN) {
            tracing::warn!(
                display_id,
                prev_time_pos,
                curr_time_pos,
                "存活看护：检测到视频壁纸停滞，但处于自愈冷却期内，跳过自愈"
            );
            continue;
        }
        // 自愈打点由 heal_stalled_renderer 内部负责，此处不重复
        match guard.heal_stalled_renderer(&display_id, prev_time_pos, curr_time_pos, "watchdog") {
            Ok(()) => record_heal(&display_id, now),
            Err(e) => {
                tracing::warn!(display_id, error = %e, "存活看护：自愈失败（仅记录，不阻断）")
            }
        }
    }

    // 完成采样：返回确认快照供调用方替换基线
    Some(confirm.into_iter().collect())
}

/// 找出"本轮相对上一轮停滞"的显示器（纯函数，不含冷却判定，便于单测）。
///
/// 仅对**两轮都有快照**的显示器判定（首轮无基线 ⇒ 不判定，避免启动即误愈）；
/// 返回 `(display_id, prev_time_pos, curr_time_pos)` 便于日志与自愈。
///
/// `pub(crate)`：与兄弟模块 [`crate::platform::pure_mode`] 的退出存活校验共用同一判定，
/// 避免两处维护同构逻辑。
pub(crate) fn stalled_display_ids(
    prev: &HashMap<String, LivenessProbe>,
    curr: &[(String, LivenessProbe)],
) -> Vec<(String, f64, f64)> {
    curr.iter()
        .filter_map(|(display_id, curr_probe)| {
            let prev_probe = prev.get(display_id)?;
            video_playback_stalled(prev_probe, curr_probe)
                .then(|| (display_id.clone(), prev_probe.time_pos, curr_probe.time_pos))
        })
        .collect()
}

/// 确认采样后的最终停滞集合（纯函数，便于单测）。
///
/// `suspects` 为第一段区间（`prev → mid`）命中的 `(display_id, prev_time_pos, mid_time_pos)`；
/// `confirmed` 为第二段区间（`mid → last`）的确认探针快照。仅当第二段**同样钉住**
/// （`video_playback_stalled(mid, last)` 成立）才保留——否则说明是周期混叠
/// （循环时长整除采样周期时首段会出现 `|Δ| ≈ 0` 的假阳性），应放弃自愈。
///
/// 返回项的 `curr` 用确认快照的 `time_pos`，便于日志体现"确认后仍钉住"。确认快照缺失该
/// 显示器的项直接丢弃（换槽/被移除等，不判定）。
///
/// 说明：首段命中已保证 `mid` 处于"播放中且非 idle"（否则 `stalled_display_ids` 不会返回），
/// 故用 `mid_time_pos` 重建 `mid` 探针即足以做第二段判定（`video_playback_stalled` 对首参
/// 仅读取 `time_pos`）。
///
/// `pub(crate)`：与兄弟模块 [`crate::platform::pure_mode`] 的退出存活校验共用同一确认判定。
pub(crate) fn confirmed_stalls(
    suspects: Vec<(String, f64, f64)>,
    confirmed: &[(String, LivenessProbe)],
) -> Vec<(String, f64, f64)> {
    suspects
        .into_iter()
        .filter_map(|(display_id, prev_time_pos, mid_time_pos)| {
            let last = confirmed
                .iter()
                .find(|(id, _)| id == &display_id)
                .map(|(_, p)| p)?;
            let mid = LivenessProbe {
                time_pos: mid_time_pos,
                paused: false,
                idle_active: false,
                eof_reached: false,
            };
            if video_playback_stalled(&mid, last) {
                Some((display_id, prev_time_pos, last.time_pos))
            } else {
                None
            }
        })
        .collect()
}

/// 自愈是否被允许（纯函数，便于单测）。
///
/// - 无历史（`last == None`）⇒ 允许；
/// - 距离上次自愈已满 `cooldown` ⇒ 允许；
/// - 仍在冷却期内 ⇒ 拒绝（只记日志，避免重启风暴）。
///
/// `Instant` 为进程内单调时钟，`now` 通常不早于 `last`；若出现异常（`checked_duration_since`
/// 返回 `None`）按"允许"处理，避免因时钟异常永久禁用自愈。
fn heal_allowed(last: Option<Instant>, now: Instant, cooldown: Duration) -> bool {
    match last.and_then(|t| now.checked_duration_since(t)) {
        Some(elapsed) => elapsed >= cooldown,
        None => true,
    }
}

/// 读取指定显示器的上次自愈时间戳（无记录返回 `None`）。
///
/// 锁中毒时沿用仓库既有约定 `unwrap_or_else(|e| e.into_inner())` 取回内部数据：
/// 若降级为"无历史"，`heal_allowed` 会永远视为允许，导致冷却失效、可能自愈风暴。
fn last_heal(display_id: &str) -> Option<Instant> {
    LAST_HEAL_TS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(display_id)
        .copied()
}

/// 记录指定显示器本次自愈时间戳（供冷却判定）。
///
/// 与 [`last_heal`] 一致：锁中毒时仍写入真实时间戳，避免冷却判定因丢失历史而失效。
fn record_heal(display_id: &str, at: Instant) {
    LAST_HEAL_TS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(display_id.to_string(), at);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个"正在播放"的探针快照（未暂停、非 idle）。
    fn probe(time_pos: f64) -> LivenessProbe {
        LivenessProbe {
            time_pos,
            paused: false,
            idle_active: false,
            eof_reached: false,
        }
    }

    fn snapshot(entries: &[(&str, LivenessProbe)]) -> HashMap<String, LivenessProbe> {
        entries
            .iter()
            .map(|(id, p)| ((*id).to_string(), *p))
            .collect()
    }

    // ── 冷却纯函数 ────────────────────────────────────────────────────────────

    #[test]
    fn heal_allowed_respects_history_and_cooldown() {
        let now = Instant::now();
        // 无历史 → 允许
        assert!(heal_allowed(None, now, HEAL_COOLDOWN));
        // 冷却期内（30s < 60s）→ 拒绝
        assert!(!heal_allowed(
            Some(now),
            now + Duration::from_secs(30),
            HEAL_COOLDOWN
        ));
        // 冷却结束（恰好 60s）→ 允许
        assert!(heal_allowed(
            Some(now),
            now + Duration::from_secs(60),
            HEAL_COOLDOWN
        ));
        // 远超冷却（120s）→ 允许
        assert!(heal_allowed(
            Some(now),
            now + Duration::from_secs(120),
            HEAL_COOLDOWN
        ));
    }

    // ── 停滞判定组合（经由本模块的 stalled_display_ids） ───────────────────────

    #[test]
    fn stalled_when_clock_not_advancing_across_two_rounds() {
        // 上一轮 3.0，本轮仍 3.0（未暂停、非 idle）→ 命中停滞
        let prev = snapshot(&[("monitor_0", probe(3.0))]);
        let curr = vec![("monitor_0".to_string(), probe(3.0))];
        let stalled = stalled_display_ids(&prev, &curr);
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].0, "monitor_0");

        // 无基线（首轮）→ 不判定
        let empty: HashMap<String, LivenessProbe> = HashMap::new();
        assert!(stalled_display_ids(&empty, &curr).is_empty());
    }

    #[test]
    fn not_stalled_when_clock_advances() {
        // 时钟前进（3.0 → 3.9）→ 不命中
        let prev = snapshot(&[("monitor_0", probe(3.0))]);
        let curr = vec![("monitor_0".to_string(), probe(3.9))];
        assert!(stalled_display_ids(&prev, &curr).is_empty());
    }

    #[test]
    fn not_stalled_when_paused_or_idle() {
        let prev = snapshot(&[("monitor_0", probe(3.0))]);

        // 暂停（paused=true）：即便时钟不动也不判定
        let paused = LivenessProbe {
            paused: true,
            ..probe(3.0)
        };
        assert!(stalled_display_ids(&prev, &[("monitor_0".to_string(), paused)]).is_empty());

        // idle（无文件）：同样不判定
        let idle = LivenessProbe {
            idle_active: true,
            ..probe(3.0)
        };
        assert!(stalled_display_ids(&prev, &[("monitor_0".to_string(), idle)]).is_empty());
    }

    // ── 确认采样（经由本模块的 confirmed_stalls） ──────────────────────────────

    #[test]
    fn confirm_rejects_cycle_aliasing_when_clock_re_advances() {
        // 首段钉住（prev=3.0 → mid=3.0）但确认区间时钟恢复前进（mid=3.0 → last=3.4）
        // ⇒ 判定为周期混叠，证伪，不自愈
        let suspects = vec![("monitor_0".to_string(), 3.0, 3.0)];
        let confirmed = vec![("monitor_0".to_string(), probe(3.4))];
        assert!(confirmed_stalls(suspects, &confirmed).is_empty());
    }

    #[test]
    fn confirm_keeps_display_when_clock_stays_pinned() {
        // 确认区间同样钉住（mid=3.0 → last=3.0）⇒ 保留该项，curr 用确认快照的 time_pos
        let suspects = vec![("monitor_0".to_string(), 3.0, 3.0)];
        let confirmed = vec![("monitor_0".to_string(), probe(3.0))];
        assert_eq!(
            confirmed_stalls(suspects, &confirmed),
            vec![("monitor_0".to_string(), 3.0, 3.0)]
        );
    }

    #[test]
    fn confirm_drops_display_missing_from_confirmation() {
        // 确认快照缺失该显示器（如换槽中/被移除）⇒ 不保留（不判定）
        let suspects = vec![("monitor_0".to_string(), 3.0, 3.0)];
        let confirmed: Vec<(String, LivenessProbe)> = Vec::new();
        assert!(confirmed_stalls(suspects, &confirmed).is_empty());
    }
}
