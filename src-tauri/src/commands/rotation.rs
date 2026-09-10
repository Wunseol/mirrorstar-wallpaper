//! 壁纸轮换调度命令层（设计 §15 / Task 8）。
//!
//! 命令分四组：
//! - 轮换配置读写：`get_rotation_config` / `update_rotation_config`
//! - 池 CRUD：`list_pools` / `create_pool` / `update_pool` / `delete_pool`
//! - 单元配置：`get_unit_states` / `set_active_pool` / `set_rotation_enabled`
//! - 手动换图：`next_wallpaper`
//!
//! 所有写操作（配置 / 池 / 单元状态）在落盘后通过 [`crate::scheduler::SchedulerHandle::wake`]
//! 的 `notify_waiters()` 唤醒调度器重算 deadline / 失效重建采样器（DR-23）。手动
//! `next_wallpaper` 走 `manual_tx` 通道，由主循环的 `select!` 即时消费（不受暂停限制，
//! DR-9 / DR-20）。
//!
//! 单元 key 校验（DR-40）：`AllSame` / `Span` 仅允许全局单元 `"all"`；`PerMonitor`
//! 依显示器枚举校验 key 命中，缺省回退主显示器（DR-36）。孤儿单元一律拒绝。

use mirrorstar_core::config::settings::RotationConfig;
use mirrorstar_core::config::Pool;
use mirrorstar_core::{Arrangement, MirrorStarError};
use tauri::State;

use crate::scheduler::ALL_UNIT_KEY;
use crate::state::AppState;

// ── 轮换配置 ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_rotation_config(
    state: State<'_, AppState>,
) -> Result<RotationConfig, MirrorStarError> {
    Ok(state.config_manager.get_config().rotation)
}

#[tauri::command]
pub fn update_rotation_config(
    state: State<'_, AppState>,
    config: RotationConfig,
) -> Result<(), MirrorStarError> {
    // 合并进完整 AppConfig（其余字段保持不变）后走既有 update_config 校验 + 落盘。
    let mut full = state.config_manager.get_config();
    let old_enabled = full.rotation.enabled;
    let new_enabled = config.enabled;
    full.rotation = config;
    state.config_manager.update_config(full)?;

    // 全局 `rotation.enabled` 由 false→true：按当前编排联动启用对应布局单元，
    // 否则仅开全局开关不会轮换（调度器判定定时可触发取决于单元的 enabled，见
    // scheduler 的 unit_is_rotatable）。此后用户仍可单独关闭某单元，保持到下次再开。
    if !old_enabled && new_enabled {
        let arrangement = state.config_manager.get_config().rotation.arrangement;
        let keys: Vec<String> = match arrangement {
            Arrangement::PerMonitor => {
                let displays = {
                    let desktop = state.desktop.lock().unwrap_or_else(|e| e.into_inner());
                    desktop.enumerate_displays()
                };
                displays.into_iter().map(|d| d.id).collect()
            }
            Arrangement::AllSame | Arrangement::Span => vec![ALL_UNIT_KEY.to_string()],
        };
        state.scheduler.enable_layout_units(&keys);
    }

    // 编排 / 间隔 / 开关等变化 → 唤醒调度器重算 deadline（DR-23）。
    // enable_layout_units 内部已唤醒一次；此处再唤醒一次幂等无害，保持既有行为。
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
    state.config_manager.update_pool(&id, name.as_deref(), member_ids)?;
    state.scheduler.wake.notify_waiters();
    Ok(())
}

#[tauri::command]
pub fn delete_pool(state: State<'_, AppState>, id: String) -> Result<(), MirrorStarError> {
    state.config_manager.delete_pool(&id)?;
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
#[derive(serde::Serialize, Clone)]
pub struct UnitStateDto {
    pub key: String,
    /// 生效池 id；None = "全部"池（DR-35）
    pub active_pool: Option<String>,
    /// 单元轮换开关
    pub enabled: bool,
}

/// 回读调度器 `playback` 中所有单元的激活池与轮换开关。
///
/// 供前端在渲染单元配置面板前回填真实状态，避免 UI 默认态（全部 / 关）覆盖
/// 后端已绑定/开启的配置（fix-rotation-scheduler Task 6 / P2）。纯内存读，无需
/// 超时包装；锁内浅拷贝后返回。
#[tauri::command]
pub fn get_unit_states(
    state: State<'_, AppState>,
) -> Result<Vec<UnitStateDto>, MirrorStarError> {
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

// ── 单元 key 校验辅助 ───────────────────────────────────────────────────────

/// 按当前编排解析并校验调度单元 key（DR-40）。
///
/// - `PerMonitor`：枚举显示器，`key` 必须命中任一显示器 id；`None`/空串 → 回退主显示器
///   （无主则回退首个显示器，DR-36）。
/// - `AllSame` / `Span`：仅允许全局单元 `"all"`；显式给出其它 key → 拒绝孤儿单元。
fn validate_unit_key(state: &AppState, key: &Option<String>) -> Result<String, MirrorStarError> {
    let arrangement = state.config_manager.get_config().rotation.arrangement;
    match arrangement {
        Arrangement::PerMonitor => {
            let displays = {
                let desktop = state.desktop.lock().unwrap_or_else(|e| e.into_inner());
                desktop.enumerate_displays()
            };
            let key = match key.as_deref() {
                Some(k) if !k.is_empty() => k.to_string(),
                _ => {
                    // 缺省 → 主显示器（无主则首个）
                    displays
                        .iter()
                        .find(|d| d.is_primary)
                        .or_else(|| displays.first())
                        .map(|d| d.id.clone())
                        .ok_or_else(|| MirrorStarError::InvalidArgument {
                            reason: "无可用的显示器".to_string(),
                        })?
                }
            };
            if !displays.iter().any(|d| d.id == key) {
                return Err(MirrorStarError::InvalidArgument {
                    reason: format!("孤儿单元（显示器不存在）: {key}"),
                });
            }
            Ok(key)
        }
        Arrangement::AllSame | Arrangement::Span => {
            if let Some(k) = key.as_deref() {
                if !k.is_empty() && k != ALL_UNIT_KEY {
                    return Err(MirrorStarError::InvalidArgument {
                        reason: format!("此编排下不允许设置单元: {k}（仅支持全局单元 {ALL_UNIT_KEY}）"),
                    });
                }
            }
            Ok(ALL_UNIT_KEY.to_string())
        }
    }
}