//! 配置字段值范围校验共享函数。
//!
//! 提取 `volume` / `speed` 的范围校验逻辑，供 `src-tauri` 的配置写入校验
//!（`validate_config_fields`）与命令参数校验（`validate_volume` / `validate_speed`）
//! 共享，避免范围定义重复导致的不一致风险。
//!
//! 校验失败返回 [`MirrorStarError::InvalidArgument`]；配置层调用方可按需
//! 映射为 `InvalidConfig`。

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

/// 校验播放速度范围：`(0.0, 10.0]` 有限值。
///
/// 配置文件中 `video.speed` 与 `set_speed` 命令参数均使用此范围。
/// 越界或 NaN/Inf 返回 `InvalidArgument`。
pub fn validate_speed(value: f32) -> Result<(), MirrorStarError> {
    if !value.is_finite() || value <= 0.0 || value > 10.0 {
        return Err(MirrorStarError::InvalidArgument {
            reason: format!("播放速度必须在 0.0-10.0 之间且大于 0，实际: {}", value),
        });
    }
    Ok(())
}
