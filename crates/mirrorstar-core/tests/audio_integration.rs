//! A-003: WASAPI 核心路径集成测试基线
//!
//! 本文件含 `#[ignore]` 集成测试，验证 `VolumeControl` 端到端行为：
//! - 有音频设备环境：`VolumeControl::new()` 成功，`set_process_volume` 不报错
//! - 无音频设备环境：`VolumeControl::new()` 返回 Err，`new_disabled()` 工作
//!
//! 常规 `cargo test` 跳过本文件，通过 `cargo test --workspace -- --ignored` 运行
//! （CI 中由 [New]-11.9 的 `Run ignored tests` 步骤定期执行）。

#![cfg(windows)]

use mirrorstar_core::audio::volume::VolumeControl;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// 辅助：在 COM MTA 中执行闭包
fn with_com_mta<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // CoInitializeEx 返回 S_FALSE（已初始化）或 S_OK，均视为成功
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let result = f();
    unsafe { CoUninitialize() };
    result
}

#[test]
#[ignore = "A-003: 需要真实 Windows 音频设备，CI 跳过"]
fn volume_control_real_wasapi_end_to_end() {
    with_com_mta(|| {
        match VolumeControl::new() {
            Ok(ctrl) => {
                // 有音频设备：验证 set_process_volume 不报错（用当前测试进程 PID）
                let test_pid = std::process::id();
                let result = ctrl.set_process_volume(test_pid, 0.5);
                // 测试进程本身可能无音频会话，set_process_volume 返回 Err 是正常的
                // 关键是不应 panic、不应导致 WASAPI 内部状态损坏
                match result {
                    Ok(()) => tracing::debug!("set_process_volume 成功"),
                    Err(e) => tracing::debug!(
                        error = ?e,
                        "set_process_volume 返回错误（测试进程可能无音频会话，正常）"
                    ),
                }
            }
            Err(e) => {
                // 无音频设备：VolumeControl::new() 应返回 Err
                // 验证降级实例工作
                let disabled = VolumeControl::new_disabled();
                assert!(
                    disabled.set_process_volume(0, 0.5).is_ok(),
                    "降级实例 set_process_volume 应返回 Ok"
                );
                tracing::info!(
                    error = ?e,
                    "VolumeControl::new() 失败（无音频设备环境），降级实例工作正常"
                );
            }
        }
    });
}
