//! 单实例二次启动：唤起已运行实例的主窗口。
//!
//! 主流桌面应用（Electron `second-instance`、Tauri single-instance 插件）在用户再次启动
//! 程序时不会只弹「已在运行」对话框，而是还原并显示已有主窗口。本项目用命名互斥体保证
//! 单实例，再用一个**命名事件**把「请显示主窗口」的信号投递给已运行的主实例：
//!
//! - 主实例在 Tauri setup 中创建自动重置事件并启动等待线程；
//! - 第二实例在 `main()` 检测到互斥体已存在时 `OpenEventW` + `SetEvent`，然后退出；
//! - 主实例被唤醒后走既有 [`crate::ui_process::open_main_window`]（`Show` 语义：
//!   隐藏→显示、最小化→还原、已可见→置前），并置前聚焦。
//!
//! 事件与互斥体同为会话级命名对象，同一用户会话内可见，无需额外权限。
//! 仅在事件尚不可用（主实例仍在早期启动、setup 未完成）的短暂窗口内重试；持续失败时
//! 调用方回退到原有的提示对话框，保证单实例语义不丢失。

use std::time::{Duration, Instant};

use core::ffi::c_void;

use tauri::AppHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// 二次启动显示信号的事件名（主实例与第二实例共享）。
fn show_event_name() -> windows::core::PCWSTR {
    windows::core::w!("MirrorStarWallpaper_ShowMainWindow")
}

/// 第二实例等待主实例创建事件的总时长与退避间隔。
///
/// 主实例 setup 在启动后 ~30ms 内完成；1s 上限足以覆盖冷启动抖动，
/// 又不会让重复启动的用户长时间等待。
const SIGNAL_RETRY_TOTAL: Duration = Duration::from_millis(1000);
const SIGNAL_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// 通知已运行的主实例显示主窗口；成功送达返回 `true`。
///
/// 仅打开事件并置位（`EVENT_MODIFY_STATE` 已足够），句柄随即关闭。
pub fn signal_primary_instance() -> bool {
    // 第二实例由用户从 Explorer / 开始菜单启动，通常具备前台切换权限；先临时授权
    // 所有进程置前，主实例的 UI 才能在被唤醒后成功 `SetForegroundWindow`（主流
    // “再次启动即聚焦已有窗口”的跨进程做法）。授权失败不影响事件通知。
    // SAFETY: 仅授予置前权限，不修改任何窗口状态。
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };

    let deadline = Instant::now() + SIGNAL_RETRY_TOTAL;
    loop {
        // SAFETY: 只打开并置位会话内已知名称的自动重置事件，不修改其它对象。
        unsafe {
            match OpenEventW(EVENT_MODIFY_STATE, false, show_event_name()) {
                Ok(event) => {
                    let signaled = SetEvent(event).is_ok();
                    let _ = CloseHandle(event);
                    if !signaled {
                        eprintln!("通知已运行实例显示主窗口失败：SetEvent 调用失败");
                    }
                    return signaled;
                }
                Err(error) => {
                    if Instant::now() >= deadline {
                        eprintln!("通知已运行实例显示主窗口失败：{error}");
                        return false;
                    }
                }
            }
        }
        std::thread::sleep(SIGNAL_RETRY_INTERVAL);
    }
}

/// 主实例侧：创建显示事件并启动等待线程（Tauri setup 中调用一次）。
///
/// 等待线程独占事件句柄直至进程退出；创建/启动失败仅告警降级——第二实例会回退到
/// 提示对话框，不影响主实例运行。
pub fn install_listener(app: AppHandle) {
    // SAFETY: 创建自动重置、初始未置位的会话内命名事件；句柄交等待线程独占持有。
    let event = match unsafe { CreateEventW(None, false, false, show_event_name()) } {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "创建单实例显示事件失败（二次启动将回退为提示对话框）"
            );
            return;
        }
    };

    // `HANDLE` 内部是裸指针（`!Send`），而等待线程需跨线程持有，故以 `isize` 传递，
    // 在闭包内还原（与 `ui_process::UiJobGuard` 的跨线程句柄保存方式一致）。
    let event_handle = event.0 as isize;
    let spawn = std::thread::Builder::new()
        .name("mirrorstar-second-instance".to_string())
        .spawn(move || loop {
            // SAFETY: 句柄由本线程独占持有；自动重置事件在每次等待返回后自动复位。
            let wait = unsafe {
                WaitForSingleObject(HANDLE(event_handle as *mut c_void), INFINITE)
            };
            if wait != WAIT_OBJECT_0 {
                tracing::warn!("单实例显示事件等待失败，停止监听");
                break;
            }
            tracing::info!("收到二次启动通知，显示主窗口");
            crate::ui_process::open_main_window(&app);
        });
    if let Err(error) = spawn {
        tracing::warn!(
            error = %error,
            "启动单实例监听线程失败（二次启动将回退为提示对话框）"
        );
    }
}
