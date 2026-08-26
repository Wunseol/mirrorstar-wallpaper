use mirrorstar_core::{AppConfig, MirrorStarError};
use tauri::State;

use crate::state::AppState;

// update_config 防抖策略与并发策略文档化
//
// # 防抖策略（300ms 窗口）
//
// `update_config` 命令本身不实现防抖，依赖 `ConfigManager` 内部的防抖机制：
//
// - **防抖窗口**：`CONFIG_SAVE_DEBOUNCE_MS = 300`（见 `mirrorstar-core/src/config/manager.rs`）
// - **触发点**：`update_config` → `ConfigManager::update_config` → `maybe_save_config`
// - **防抖逻辑**（`maybe_save_config` 内部）：
//   1. 获取 `config_save_mutex`（串行化 config.toml 落盘操作）
//   2. 读取 `last_save_time`，若距上次保存 < 300ms，跳过本次落盘
//   3. 跳过时 `dirty` 标志保留原值（不清除），待防抖窗口结束后由后续调用落盘
//   4. 防抖窗口外：原子地 check-and-clear `dirty` 标志，落盘并更新 `last_save_time`
// - **兜底机制**：
//   - **周期性保存任务**：`start_periodic_save` 后台线程定期检查 `dirty` 并落盘
//     （即使前端无 `update_config` 调用，dirty 配置也会在周期性任务中被持久化）
//   - **退出时 flush**：`perform_shutdown_blocking` 中调用 `config_manager.flush()`
//     强制落盘所有 dirty 配置（不受防抖窗口限制）
//
// # 并发策略（config_save_mutex / library_save_mutex 串行化）
//
// - **`config_save_mutex: Arc<std::sync::Mutex<()>>`**：串行化 config.toml 落盘
//   （`maybe_save_config` / `flush` / 周期性 config 保存）
// - **`library_save_mutex: Arc<std::sync::Mutex<()>>`**：串行化 wallpapers.toml 落盘
//   （`save_library` / `batch_update_thumbnails` / 周期性 library 保存）
// - 两者写入不同文件，无数据一致性冲突，故拆分为独立锁，
//   避免 thumbnail 批量更新等密集 library 落盘阻塞前端 config 修改的 `maybe_save_config`
// - **锁中毒处理**：`unwrap_or_else(|e| e.into_inner())` 静默恢复（持有锁的线程
//   panic 时仍返回内部数据继续执行，避免配置丢失）
// - **不与 engine 锁交叉**：落盘锁仅保护磁盘 IO，与 `wallpaper_engine` 锁无依赖，
//   不会形成锁环
//
// # 前端调用模式建议
//
// - **批量更新**：前端连续修改多个配置项时，应在 300ms 内合并为单次 `update_config` 调用，
//   避免每次修改都触发一次 invoke（虽 `ConfigManager` 内部会防抖，但 invoke 开销仍累积）
// - **即时反馈**：前端可乐观更新 UI，不等 `update_config` 返回即展示新配置
//   （配置写入由防抖兜底，最终一致）
// - **错误处理**：`update_config` 返回 `Err` 仅在配置非法（`validate_config_fields` 失败）
//   或 `ConfigManager.update_config` 内部错误时，前端应回滚乐观更新并提示用户

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<AppConfig, MirrorStarError> {
    Ok(state.config_manager.get_config())
}

/// SEC-002: 校验 AppConfig 所有数值字段的范围与有限性
///
/// 在 `update_config` 写入之前调用，避免非法值（NaN/Inf/越界/负数）进入配置文件
/// 与运行时引擎。错误以 `MirrorStarError::InvalidConfig` 返回，附带字段级 reason。
fn validate_config_fields(config: &AppConfig) -> Result<(), MirrorStarError> {
    // audio.volume: f32，合法范围 [0.0, 1.0]，且必须为有限值
    // 委托 mirrorstar-core 共享校验函数，映射 InvalidArgument → InvalidConfig
    if let Err(e) = mirrorstar_core::config::validation::validate_volume(config.audio.volume) {
        match e {
            MirrorStarError::InvalidArgument { reason } => {
                return Err(MirrorStarError::InvalidConfig {
                    reason: format!("音频音量: {}", reason),
                });
            }
            _ => return Err(e),
        }
    }

    // video.speed: f32，合法范围 (0.0, 10.0]，且必须为有限值
    if let Err(e) = mirrorstar_core::config::validation::validate_speed(config.video.speed) {
        match e {
            MirrorStarError::InvalidArgument { reason } => {
                return Err(MirrorStarError::InvalidConfig {
                    reason: format!("视频播放速度: {}", reason),
                });
            }
            _ => return Err(e),
        }
    }

    // gif.balanced_keep_frames: usize，业务上限远低于 i32::MAX，此处为防御性边界校验
    if config.gif.balanced_keep_frames > i32::MAX as usize {
        return Err(MirrorStarError::InvalidConfig {
            reason: format!(
                "GIF 平衡模式保留帧数超出合理范围: {}",
                config.gif.balanced_keep_frames
            ),
        });
    }

    Ok(())
}

#[tauri::command]
pub async fn update_config(
    state: State<'_, AppState>,
    config: AppConfig,
) -> Result<(), MirrorStarError> {
    // SEC-002: 写入前校验字段范围，避免非法值进入配置文件与壁纸引擎
    validate_config_fields(&config)?;

    // T09：更新壁纸引擎的 GIF 内存管理策略
    // set_gif_memory_strategy 改为 &self + 内部 Mutex，使用 try_lock 避免阻塞其他
    // 引擎命令（如 set_wallpaper）。锁忙时跳过实时更新（非关键：配置已写入
    // ConfigManager，下次创建壁纸时从配置读取正确值）。
    //
    // ST-017: try_lock 路径设计权衡（已评估并接受，方案 ②）
    // - `set_gif_memory_strategy` 改为 `&self + 内部 Mutex` 后，理论上无需 engine 锁，
    //   可通过 `Arc<WallpaperEngine>` 直接调用（方案 ①）
    // - 保留 `try_lock` 路径避免重构 engine 公共 API（方案 ① 涉及 engine 重构，复杂度中）
    // - 锁忙时跳过实时更新可接受：配置已写入 ConfigManager，下次创建壁纸时从配置读取正确值
    // - 此为已评估并接受的设计权衡
    //
    // try_lock 失败时返回 Ok(()) 的行为与用户体验文档化
    //
    // # try_lock 失败时的返回值行为
    //
    // 本函数在 `state.wallpaper_engine.try_lock()` 失败（锁忙）时：
    // - **不返回 Err**：继续执行 `state.config_manager.update_config(config)` 并返回 `Ok(())`
    // - **仅记录 warn 日志**：`"引擎锁忙，跳过 GIF 内存策略实时更新（下次创建壁纸时从配置读取）"`
    // - **配置仍被持久化**：`ConfigManager.update_config` 会写入 TOML 文件，
    //   下次启动或创建新壁纸时会读取最新配置
    //
    // # 用户体验影响
    //
    // - **前端无感知**：`update_config` 命令返回 `Ok(())`，前端认为配置已更新成功
    //   （实际配置已写入文件，仅 engine 实时策略未更新）
    // - **GIF 内存策略延迟生效**：若用户修改 GIF 内存策略时正好有 `set_wallpaper`
    //   命令在执行（持 engine 锁），新策略不会立即应用到当前运行的 GIF 壁纸，
    //   需等待下次创建壁纸时从配置读取
    // - **无错误提示**：前端不会展示"操作进行中，请稍后"等提示，
    //   因为此场景对用户透明（配置已保存，仅运行时策略延迟生效）
    //
    // # 为何不返回 Err（如 "操作进行中，请稍后"）？
    //
    // - **配置已成功写入**：`ConfigManager.update_config` 不依赖 engine 锁，
    //   配置文件已被持久化，返回 Err 会让前端误以为配置未保存而重试
    // - **非关键路径**：GIF 内存策略实时更新是优化项（非功能正确性），
    //   延迟生效不影响功能正确性
    // - **避免误导用户**：返回 Err 后前端展示"操作进行中"会让用户困惑
    //   （用户修改的是配置，而非壁纸切换操作）
    //
    // # 调用方注意事项
    //
    // - 前端无需特殊处理 try_lock 失败场景（`Ok(())` 即表示配置已保存）
    // - 如需确保 engine 实时应用最新配置，可在 `update_config` 后调用 `set_wallpaper`
    //   重新设置壁纸（会获取 engine 锁并应用新配置）
    {
        if let Ok(engine) = state.wallpaper_engine.try_lock() {
            engine.set_gif_memory_strategy(
                config.gif.memory_strategy,
                config.gif.balanced_keep_frames,
                config.gif.max_memory_mb,
            );
        } else {
            tracing::warn!("引擎锁忙，跳过 GIF 内存策略实时更新（下次创建壁纸时从配置读取）");
        }
    }
    state.config_manager.update_config(config)
}
