//! 壁纸轮换调度命令层（设计 §15 / Task 8）。
//!
//! 命令分四组：
//! - 轮换配置读写：`get_rotation_config` / `update_rotation_config`
//! - 池 CRUD：`list_pools` / `create_pool` / `update_pool` / `delete_pool`
//! - 单元配置：`get_unit_states` / `set_active_pool` / `set_rotation_enabled`
//! - 手动换图：`next_wallpaper` / `prev_wallpaper`
//!
//! 所有写操作（配置 / 池 / 单元状态）在落盘后通过 [`crate::scheduler::SchedulerHandle::wake`]
//! 的 `notify_waiters()` 唤醒调度器重算 deadline / 失效重建采样器（DR-23）。手动
//! `next_wallpaper` 走 `manual_tx` 通道，由主循环的 `select!` 即时消费（不受暂停限制，
//! DR-9 / DR-20）；`prev_wallpaper` 直接调调度器回退历史栈（同样不受暂停限制）。
//!
//! 单元 key 校验（DR-40）：`AllSame` / `Span` 仅允许全局单元 `"all"`；`PerMonitor`
//! 依显示器枚举校验 key 命中，缺省回退主显示器（DR-36）。孤儿单元一律拒绝。

use mirrorstar_core::config::settings::{RotationConfig, MIN_INTERVAL_MINUTES};
use mirrorstar_core::config::Pool;
use mirrorstar_core::layout::{self, LayoutMonitor};
use mirrorstar_core::ui_proto::UnitStateDto;
use mirrorstar_core::MirrorStarError;
use tauri::State;

use crate::state::AppState;

// ── 轮换配置 ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_rotation_config(state: State<'_, AppState>) -> Result<RotationConfig, MirrorStarError> {
    Ok(state.config_manager.get_config().rotation)
}

/// DR-16 硬校验：`interval_minutes` 下限为 core `MIN_INTERVAL_MINUTES`（1 分钟）。
///
/// 低于下限时 core `RotationConfig::validate` 会静默 clamp 到 1（前端收到 Ok 但实际
/// 值被改），故命令层以同一下限做硬校验，返回 `InvalidConfig` 让前端回滚乐观更新。
fn validate_rotation_config(config: &RotationConfig) -> Result<(), MirrorStarError> {
    if config.interval_minutes < MIN_INTERVAL_MINUTES {
        return Err(MirrorStarError::InvalidConfig {
            reason: format!(
                "轮换间隔过短（下限 {} 分钟）: {}",
                MIN_INTERVAL_MINUTES, config.interval_minutes
            ),
        });
    }
    Ok(())
}

#[tauri::command]
pub fn update_rotation_config(
    state: State<'_, AppState>,
    config: RotationConfig,
) -> Result<(), MirrorStarError> {
    // DR-16：core `RotationConfig::validate` 对 interval_minutes < 1 静默 clamp，
    // 命令层先做硬校验，非法输入返回 InvalidConfig（前端回滚乐观更新）。
    validate_rotation_config(&config)?;

    // 合并进完整 AppConfig（其余字段保持不变）后走既有 update_config 校验 + 落盘。
    let mut full = state.config_manager.get_config();
    full.rotation = config;
    state.config_manager.update_config(full)?;

    // 唤醒调度器重算 deadline（DR-23）。
    state.scheduler.wake.notify_waiters();
    Ok(())
}

// ── 池 CRUD ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_pools(state: State<'_, AppState>) -> Result<Vec<Pool>, MirrorStarError> {
    Ok(state.config_manager.list_pools())
}

#[tauri::command]
pub fn create_pool(
    state: State<'_, AppState>,
    name: Option<String>,
    member_ids: Vec<String>,
) -> Result<Pool, MirrorStarError> {
    let pool = state.config_manager.create_pool(name, member_ids)?;
    // 池成员变化 → 唤醒调度器失效重建采样器（DR-23）。
    state.scheduler.wake.notify_waiters();
    Ok(pool)
}

#[tauri::command]
pub fn update_pool(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    member_ids: Option<Vec<String>>,
) -> Result<(), MirrorStarError> {
    state
        .config_manager
        .update_pool(&id, name.as_deref(), member_ids)?;
    state.scheduler.wake.notify_waiters();
    Ok(())
}

#[tauri::command]
pub fn delete_pool(state: State<'_, AppState>, id: String) -> Result<(), MirrorStarError> {
    state.config_manager.delete_pool(&id)?;
    state.scheduler.wake.notify_waiters();
    Ok(())
}

/// 按新顺序重排池列表并落盘（`ids` 必须是当前池 id 集合的完全重排）。
///
/// 与 `update_pool` / `delete_pool` 一致：池列表变化后唤醒调度器重算（DR-23）。
#[tauri::command]
pub fn reorder_pools(state: State<'_, AppState>, ids: Vec<String>) -> Result<(), MirrorStarError> {
    state.config_manager.reorder_pools(ids)?;
    state.scheduler.wake.notify_waiters();
    Ok(())
}

// ── 单元配置 ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn set_active_pool(
    state: State<'_, AppState>,
    key: Option<String>,
    pool_id: Option<String>,
) -> Result<(), MirrorStarError> {
    let key = validate_unit_key(state.inner(), &key)?;
    // 校验 / 落盘 / 唤醒统一收敛到 SchedulerHandle。
    state.scheduler.set_unit_active_pool(&key, pool_id)
}

#[tauri::command]
pub fn set_rotation_enabled(
    state: State<'_, AppState>,
    key: Option<String>,
    enabled: bool,
) -> Result<(), MirrorStarError> {
    let key = validate_unit_key(state.inner(), &key)?;
    state.scheduler.set_unit_enabled(&key, enabled);
    Ok(())
}

/// 单元配置回读结构（前端渲染单元面板初始态用，见 fix-rotation-scheduler Task 6）。
///
/// 定义已下沉到 [`mirrorstar_core::ui_proto::UnitStateDto`]，本命令直接复用该核心类型
/// （与 `mirrorstar-ui` 进程共用同一份定义，禁止任一侧手写重复结构）。
///
/// 回读调度器 `playback` 中所有单元的激活池与轮换开关。
///
/// 供前端在渲染单元配置面板前回填真实状态，避免 UI 默认态（全部 / 关）覆盖
/// 后端已绑定/开启的配置（fix-rotation-scheduler Task 6 / P2）。纯内存读，无需
/// 超时包装；锁内浅拷贝后返回。
#[tauri::command]
pub fn get_unit_states(state: State<'_, AppState>) -> Result<Vec<UnitStateDto>, MirrorStarError> {
    let play = state
        .scheduler
        .playback
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let states = play
        .units
        .values()
        .map(|u| UnitStateDto {
            key: u.key.clone(),
            active_pool: u.active_pool.clone(),
            enabled: u.enabled,
        })
        .collect();
    Ok(states)
}

// ── 手动换图 ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn next_wallpaper(
    state: State<'_, AppState>,
    key: Option<String>,
) -> Result<(), MirrorStarError> {
    let key = validate_unit_key(state.inner(), &key)?;
    // 手动通道即时唤醒主循环执行（不受暂停限制，DR-9 / DR-20）。
    let _ = state.scheduler.manual_tx.send(Some(key));
    Ok(())
}

/// 回退上一张（对称 `next_wallpaper`；Task 1 边界 1/3/4/5/6/7/8/9/10/11/12）。
///
/// 返回 `None` = 无上一张（历史栈空 / 可用历史已消费尽），前端提示"没有上一张"并
/// 保持按钮可用；`Some(id)` = 已回退到的壁纸 id。不受暂停限制（与 next 一致）。
#[tauri::command]
pub async fn prev_wallpaper(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    key: Option<String>,
) -> Result<Option<String>, MirrorStarError> {
    let key = validate_unit_key(state.inner(), &key)?;
    Ok(state.scheduler.prev_wallpaper(&key, &app).await)
}

// ── 单元 key 校验辅助 ───────────────────────────────────────────────────────

/// 按当前编排解析并校验调度单元 key（DR-40）。
///
/// - `PerMonitor`：枚举显示器，`key` 必须命中任一显示器 id；`None`/空串 → 回退主显示器
///   （无主则回退首个显示器，DR-36）。
/// - `AllSame` / `Span`：仅允许全局单元 `"all"`；显式给出其它 key → 拒绝孤儿单元。
///
/// 规则收敛到多屏排列布局域（`mirrorstar_core::layout::validate_unit_key`），命令层
/// 只负责提供真实显示器拓扑并映射错误。
fn validate_unit_key(state: &AppState, key: &Option<String>) -> Result<String, MirrorStarError> {
    let arrangement = state.config_manager.get_config().arrangement;
    let displays = {
        let desktop = state.desktop.lock().unwrap_or_else(|e| e.into_inner());
        desktop.enumerate_displays()
    };
    let monitors: Vec<LayoutMonitor> = displays.iter().map(LayoutMonitor::from).collect();
    let plane = layout::plan(arrangement, &monitors);
    layout::validate_unit_key(&plane, key.as_deref()).map_err(|e| {
        MirrorStarError::InvalidArgument {
            reason: e.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_rotation_config 单元测试（DR-16 校验边界） ──────────────────

    #[test]
    fn validate_rotation_config_rejects_interval_below_minimum() {
        // P2-9: interval_minutes 低于 core 下限 MIN_INTERVAL_MINUTES（1）应返回
        // InvalidConfig。此前命令层绕过校验，0 会被 RotationConfig::validate 静默
        // clamp 到 1（前端收到 Ok 但实际值被改）。
        let config = RotationConfig {
            interval_minutes: 0,
            ..Default::default()
        };
        let err = validate_rotation_config(&config).unwrap_err();
        match err {
            MirrorStarError::InvalidConfig { reason } => {
                assert!(
                    reason.contains("轮换间隔过短"),
                    "reason 应包含「轮换间隔过短」，实际: {}",
                    reason
                );
            }
            other => panic!("期望 InvalidConfig 变体，实际: {:?}", other),
        }
    }

    #[test]
    fn validate_rotation_config_accepts_interval_at_minimum() {
        // 下限处（1 分钟）的值应通过校验
        let config = RotationConfig {
            interval_minutes: MIN_INTERVAL_MINUTES,
            ..Default::default()
        };
        assert!(validate_rotation_config(&config).is_ok());
    }
}
