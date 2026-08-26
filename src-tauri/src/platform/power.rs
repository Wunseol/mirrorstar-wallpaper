use crate::state::{try_pause_all_fast, try_resume_all_fast, POWER_WAS_ON_BATTERY, SHARED_CONFIG};
use mirrorstar_core::PauseReason;
use std::sync::atomic::Ordering;

// 电源事件处理与恢复策略文档化
//
// # 电源事件处理总览
//
// 电源事件通过 `WM_POWERBROADCAST` 窗口消息分发，由 `explorer.rs` 的
// `explorer_monitor_wndproc` 接收并转发到本模块的 `handle_power_status_change`。
//
// ## 已处理事件
//
// ### PBT_APMPOWERSTATUSCHANGE（电源类型变化）
//
// - **触发场景**：交流供电 ↔ 电池供电切换（拔插电源适配器）
// - **处理逻辑**（`handle_power_status_change`）：
//   - 从 AC 切换到电池 → 暂停所有壁纸（`pause_all_fast(BATTERY)`）
//   - 从电池切换到 AC → 恢复所有壁纸（`resume_all_fast(BATTERY)`）
//   - 通过 `POWER_WAS_ON_BATTERY` AtomicBool 跟踪上一状态，避免重复暂停/恢复
// - **配置开关**：`pause.pause_on_battery`（默认启用）
//   - 禁用时若此前因电池暂停过，恢复壁纸并重置状态
// - **未知状态处理**（T-015）：`ACLineStatus=255`（未知）显式跳过，
//   不触发暂停/恢复，避免在电源状态不确定时误切换壁纸状态
//
// ## 未处理事件（已知限制）
//
// ### PBT_APMRESUMESUSPEND / PBT_APMRESUMEAUTOMATIC（从睡眠/休眠恢复）
//
// - **当前状态**：未处理。系统从睡眠恢复时不会触发 `PBT_APMPOWERSTATUSCHANGE`
//   （电源类型可能未变），仅触发 `PBT_APMRESUMESUSPEND` / `PBT_APMRESUMEAUTOMATIC`
// - **潜在影响**：睡眠期间 mpv 子进程可能被系统挂起（SuspendThread），
//   恢复后音频/视频可能不同步或卡顿；WorkerW 窗口可能因 DWM 重置失效
// - **当前缓解**：
//   - `workerw_check.rs` 5 分钟间隔兜底检查会检测到 WorkerW 失效并重新初始化
//   - `TaskbarCreated` 事件（Explorer 重启时触发）也会触发 WorkerW 重初始化
//   - 用户可通过 `check_desktop_status` 命令手动触发重初始化
// - **未来改进方向**：在 `explorer_monitor_wndproc` 中追加
//   `PBT_APMRESUMESUSPEND` / `PBT_APMRESUMEAUTOMATIC` 分支，
//   调用 `engine.restart_all()` 或 emit `desktop-status-changed` 事件
//   触发壁纸重新加载（需评估 mpv 子进程在睡眠后的状态）
//
// ### PBT_APMSUSPEND（系统即将进入睡眠/休眠）
//
// - **当前状态**：未处理。系统进入睡眠前不会主动暂停壁纸
// - **潜在影响**：睡眠期间壁纸渲染器持续运行（短暂，系统很快进入睡眠），
//   唤醒后可能因 mpv 子进程状态异常导致播放卡顿
// - **当前缓解**：系统进入睡眠时所有进程被挂起，壁纸暂停与否无实际差异
//   （用户看不到壁纸）。唤醒后的恢复才是关键（见上）
// - **未来改进方向**：可追加 `PBT_APMSUSPEND` 分支调用 `pause_all_fast(SUSPEND)`，
//   提前暂停壁纸以减少睡眠期间的资源占用（收益有限，优先级低）
//
// # 恢复策略
//
// ## 电池 → AC 恢复
//
// - **触发**：`PBT_APMPOWERSTATUSCHANGE` + `ACLineStatus=1`（AC 供电）
// - **动作**：`try_resume_all_fast(PauseReason::BATTERY)`
// - **失败处理**（C-008）：仅当 `resume_all_fast` 全部成功时才更新
//   `POWER_WAS_ON_BATTERY=false`，部分失败时保持原状态以让后续事件重试恢复
// - **锁忙处理**（T-001）：`try_lock` 失败时跳过本次事件，不更新状态
//   （`WM_POWERBROADCAST` 会重复触发，可容忍偶发跳过）
//
// ## 配置禁用时的恢复
//
// - **触发**：用户在配置中关闭 `pause.pause_on_battery` 时，
//   下次 `PBT_APMPOWERSTATUSCHANGE` 事件检测到配置已禁用
// - **动作**：若 `POWER_WAS_ON_BATTERY=true`（此前因电池暂停过），
//   调用 `try_resume_all_fast(BATTERY)` 恢复壁纸并重置状态
// - **设计意图**：用户禁用配置后立即恢复此前因电池暂停的壁纸，
//   而非等待下次 AC 切换事件

/// 处理电源状态变化（由 WM_POWERBROADCAST / PBT_APMPOWERSTATUSCHANGE 触发）
///
/// 检测交流供电与电池供电之间的切换：
/// - 从交流切换到电池时暂停所有壁纸
/// - 从电池切换到交流时恢复所有壁纸
///
/// 仅在配置 `pause.pause_on_battery` 启用时生效。
pub(crate) fn handle_power_status_change() {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    // 检查配置是否启用电池供电暂停
    let config = match SHARED_CONFIG.get() {
        Some(c) => c,
        None => return,
    };
    let config = config.get_config();
    if !config.pause.pause_on_battery {
        // 已禁用：若此前因电池暂停过，恢复壁纸并重置状态
        if POWER_WAS_ON_BATTERY.load(Ordering::SeqCst) {
            tracing::info!("电池供电暂停已禁用，恢复壁纸");
            // C-008 修复：仅当 resume_all_fast 全部成功时才更新 POWER_WAS_ON_BATTERY
            // T-001 修复：engine 锁改用 try_lock，锁忙时跳过本次电源事件
            // （handle_power_status_change 运行于 explorer_monitor_wndproc 回调上下文，
            // blocking_lock 会阻塞 explorer 消息循环，延迟 TaskbarCreated 处理；
            // WM_POWERBROADCAST 会重复触发，可容忍偶发跳过）
            //
            // ST-001 + ST-004：通过 try_resume_all_fast 统一处理 SHARED_ENGINE 未设置
            // 与 try_lock 失败两种情况——均返回 None，调用方据此 return，不更新状态。
            let failed = match try_resume_all_fast(PauseReason::BATTERY) {
                Some(f) => f,
                None => return, // SHARED_ENGINE 未设置或锁忙，不更新状态
            };
            if failed.is_empty() {
                POWER_WAS_ON_BATTERY.store(false, Ordering::SeqCst);
            } else {
                tracing::warn!(
                    failed_count = failed.len(),
                    failed = ?failed,
                    "resume_all_fast 部分失败，不更新 POWER_WAS_ON_BATTERY"
                );
            }
        }
        return;
    }

    // 获取系统电源状态
    let mut status = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut status) };
    if let Err(e) = ok {
        // T13：原实现 `if ok.is_err() { return; }` 静默退出，无任何日志输出，
        // 导致 GetSystemPowerStatus 失败（罕见，通常仅出现在驱动异常或极端系统状态）
        // 时排查困难。WM_POWERBROADCAST 会重复触发，此处记 warn 后跳过本次处理。
        tracing::warn!(error = ?e, "GetSystemPowerStatus 失败，跳过本次电源事件");
        return;
    }

    // ACLineStatus: 0 = 电池供电, 1 = 交流供电, 255 = 未知
    // T-015: ACLineStatus=255（未知）显式处理，保持当前电源策略。
    // 未知状态既不触发电池暂停也不触发交流恢复，避免在电源状态不确定时
    // 误切换壁纸状态。WM_POWERBROADCAST 会重复触发，待状态明确后再处理。
    let on_battery = match interpret_ac_line_status(status.ACLineStatus) {
        Some(b) => b,
        None => {
            tracing::warn!(
                ac_line_status = status.ACLineStatus,
                "未知电源状态，保持当前电源策略"
            );
            return; // 跳过本次处理，不触发暂停/恢复
        }
    };
    let was_on_battery = POWER_WAS_ON_BATTERY.load(Ordering::SeqCst);

    if on_battery && !was_on_battery {
        // 从交流切换到电池 → 暂停
        tracing::info!("检测到电池供电，暂停壁纸");
        // C-008 修复：仅当 pause_all_fast 全部成功时才更新 POWER_WAS_ON_BATTERY
        // ST-001 + ST-004：try_pause_all_fast 返回 None 时 return，不更新状态
        let failed = match try_pause_all_fast(PauseReason::BATTERY) {
            Some(f) => f,
            None => return,
        };
        if failed.is_empty() {
            POWER_WAS_ON_BATTERY.store(true, Ordering::SeqCst);
        } else {
            tracing::warn!(
                failed_count = failed.len(),
                failed = ?failed,
                "pause_all_fast 部分失败，不更新 POWER_WAS_ON_BATTERY"
            );
        }
    } else if !on_battery && was_on_battery {
        // 从电池切换到交流 → 恢复
        tracing::info!("恢复交流供电，恢复壁纸");
        // C-008 修复：仅当 resume_all_fast 全部成功时才更新 POWER_WAS_ON_BATTERY
        // ST-001 + ST-004：try_resume_all_fast 返回 None 时 return，不更新状态
        let failed = match try_resume_all_fast(PauseReason::BATTERY) {
            Some(f) => f,
            None => return,
        };
        if failed.is_empty() {
            POWER_WAS_ON_BATTERY.store(false, Ordering::SeqCst);
        } else {
            tracing::warn!(
                failed_count = failed.len(),
                failed = ?failed,
                "resume_all_fast 部分失败，不更新 POWER_WAS_ON_BATTERY"
            );
        }
    }
}

/// 解析 Win32 SYSTEM_POWER_STATUS.ACLineStatus 字段（T-015 修复）
///
/// 提取为独立纯函数以便单元测试覆盖状态映射逻辑（无需 Win32 环境）。
///
/// # 返回值
/// - `Some(true)`：电池供电（ACLineStatus == 0）
/// - `Some(false)`：交流供电（ACLineStatus == 1）
/// - `None`：未知电源状态（ACLineStatus == 255 或其他值），调用方应跳过本次处理，
///   不触发暂停/恢复，避免在电源状态不确定时误切换壁纸状态。
///
/// Win32 文档：ACLineStatus 为 1 字节字段，0=offline(battery), 1=online(AC),
/// 255=unknown。其他值未定义，按未知处理。
fn interpret_ac_line_status(ac_line_status: u8) -> Option<bool> {
    match ac_line_status {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── interpret_ac_line_status: ACLineStatus 映射（T-015） ─────────────────────

    #[test]
    fn test_interpret_ac_line_status_battery() {
        // ACLineStatus=0：电池供电
        assert_eq!(interpret_ac_line_status(0), Some(true));
    }

    #[test]
    fn test_interpret_ac_line_status_ac() {
        // ACLineStatus=1：交流供电
        assert_eq!(interpret_ac_line_status(1), Some(false));
    }

    #[test]
    fn test_interpret_ac_line_status_unknown_255() {
        // T-015: ACLineStatus=255（未知）应返回 None，不触发暂停/恢复
        assert_eq!(interpret_ac_line_status(255), None);
    }

    #[test]
    fn test_interpret_ac_line_status_other_undefined_values() {
        // 其他未定义值（2~254）按未知处理，返回 None
        assert_eq!(interpret_ac_line_status(2), None);
        assert_eq!(interpret_ac_line_status(128), None);
        assert_eq!(interpret_ac_line_status(254), None);
    }
}
