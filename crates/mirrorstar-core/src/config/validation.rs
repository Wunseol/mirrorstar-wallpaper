//! 配置字段值范围校验共享函数。
//!
//! 提取 `volume` / `speed` 的范围校验逻辑，供 `src-tauri` 的配置写入校验
//!（`validate_config_fields`）与命令参数校验（`validate_volume` / `validate_speed`）
//! 共享，避免范围定义重复导致的不一致风险。
//!
//! 校验失败返回 [`MirrorStarError::InvalidArgument`]；配置层调用方可按需
//! 映射为 `InvalidConfig`。

use crate::ipc::mpv_protocol::{SPEED_MAX, SPEED_MIN};
use crate::MirrorStarError;

/// 校验音量值范围：`[0.0, 1.0]` 有限值。
///
/// 配置文件中 `audio.volume` 与 `set_volume` 命令参数均使用此范围。
/// 越界或 NaN/Inf 返回 `InvalidArgument`。
pub fn validate_volume(value: f32) -> Result<(), MirrorStarError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(MirrorStarError::InvalidArgument {
            reason: format!("音量必须在 0.0-1.0 之间，实际: {}", value),
        });
    }
    Ok(())
}

/// 校验播放速度范围：`[SPEED_MIN, SPEED_MAX]`（即 `[0.25, 4.0]`）有限值。
///
/// 配置文件中 `video.speed` 与 `set_speed` 命令参数均使用此范围，
/// 上下限直接复用 [`crate::ipc::mpv_protocol::SPEED_MIN`] /
/// [`crate::ipc::mpv_protocol::SPEED_MAX`]，避免多处定义漂移。
/// 越界或 NaN/Inf 返回 `InvalidArgument`。
pub fn validate_speed(value: f32) -> Result<(), MirrorStarError> {
    if !value.is_finite() || !(SPEED_MIN..=SPEED_MAX).contains(&value) {
        return Err(MirrorStarError::InvalidArgument {
            reason: format!(
                "播放速度必须在 {}-{} 之间，实际: {}",
                SPEED_MIN, SPEED_MAX, value
            ),
        });
    }
    Ok(())
}
