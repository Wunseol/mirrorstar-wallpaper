use mirrorstar_core::DesktopIntegrator;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::platform::power::handle_power_status_change;
use crate::state::{
    EXPLORER_DESKTOP, EXPLORER_MONITOR_RUNNING, EXPLORER_MONITOR_THREAD,
    EXPLORER_MONITOR_THREAD_ID, TASKBAR_CREATED_MSG,
};

// # 文件编码策略（UTF-8 without BOM）
//
// `src-tauri/src/` 下 Rust 源文件采用 UTF-8 without BOM，`rustc` 自动跳过开头 BOM。
//
// 配置文件读取（`ConfigManager`）：`read_to_string` 不主动剥离 BOM；
// TOML 解析器通常容忍 BOM，JSON（`serde_json`）需手动剥离。
// HTML 检测（`detect_wallpaper_type`）先跳过 BOM 再扫描标记。
//
// 写入时不添加 BOM（`std::fs::write` / `toml` 序列化均不产生 BOM）。

/// Explorer 监控窗口类名（注册与注销处共用，避免字面量重复导致不一致）
const EXPLORER_CLASS_NAME: windows::core::PCWSTR = windows::core::w!("MirrorStarExplorerMonitor");

/// Explorer 重启监控窗口过程
///
/// 处理 TaskbarCreated 消息：当 Explorer 重启时系统会向所有顶层窗口广播该消息，
/// 收到后立即重新初始化 WorkerW 并重新嵌入壁纸，实现事件驱动的即时检测。
/// 同时处理 WM_POWERBROADCAST 消息，在电源状态变化时暂停/恢复壁纸。
unsafe extern "system" fn explorer_monitor_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, PBT_APMPOWERSTATUSCHANGE, WM_DESTROY, WM_POWERBROADCAST,
    };

    // 检查是否为 TaskbarCreated 消息
    if let Some(&taskbar_created) = TASKBAR_CREATED_MSG.get() {
        if msg == taskbar_created {
            tracing::info!("收到 TaskbarCreated 消息，Explorer 可能已重启，尝试重新初始化");
            if let Some(desktop_arc) = EXPLORER_DESKTOP.get() {
                match desktop_arc.try_lock() {
                    Ok(mut desktop) => {
                        // T14：check_and_reinitialize 返回 bool 表示是否实际重初始化，
                        // 此处不关心返回值（仅记日志），失败时记 error。
                        match desktop.check_and_reinitialize() {
                            Ok(_did_reinit) => {}
                            Err(e) => {
                                tracing::error!(error = %e, "Explorer 重启后重新初始化失败");
                            }
                        }
                    }
                    Err(_) => {
                        tracing::trace!(
                            "explorer_monitor_wndproc: desktop 锁忙，跳过 TaskbarCreated 处理"
                        );
                    }
                }
            }
            return windows::Win32::Foundation::LRESULT(0);
        }
    }

    if msg == WM_POWERBROADCAST {
        if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
            handle_power_status_change();
        }
        return windows::Win32::Foundation::LRESULT(0);
    }

    if msg == WM_DESTROY {
        // ST-018: 防御性处理——若外部代码（如系统或另一线程）通过 DestroyWindow 销毁本窗口，
        // WM_DESTROY 会在消息循环内被 dispatch。此处 PostQuitMessage 使下一次 GetMessageW
        // 返回 0 退出循环，确保监控线程能正常退出而不是无限等待已销毁窗口的消息。
        PostQuitMessage(0);
        return windows::Win32::Foundation::LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 启动 Explorer 重启监控线程（事件驱动，监听 TaskbarCreated 消息）
///
/// 创建一个不可见的消息窗口（HWND_MESSAGE 子窗口），监听系统在 Explorer
/// 重启时广播的 TaskbarCreated 消息，实现即时检测。作为 5 分钟轮询的补充机制。
/// 同时处理 WM_POWERBROADCAST 消息，实现电源状态变化的即时检测。
pub(crate) fn start_explorer_restart_monitor(desktop: Arc<std::sync::Mutex<DesktopIntegrator>>) {
    // C-014/C-015 修复：二次调用时先 stop 旧监控再 start
    // swap 返回旧值：若为 true 表示已有监控在运行，需先停止旧监控线程，
    // 避免旧线程消息循环泄漏。swap 同时将标志置为 true。
    if EXPLORER_MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        tracing::info!("Explorer 监控已在运行，先停止旧监控再重启");
        // 1. take 旧 thread_id 并 PostThreadMessage WM_QUIT 唤醒
        if let Some(old_tid) = EXPLORER_MONITOR_THREAD_ID
            .lock()
            .ok()
            .and_then(|mut t| t.take())
        {
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    old_tid,
                    windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
                .is_err()
                {
                    tracing::warn!("PostThreadMessageW 失败：WM_QUIT 未送达（线程可能已退出）");
                }
            }
        }
        // 2. take 旧 JoinHandle 并 join 等待旧线程退出
        if let Some(old_handle) = EXPLORER_MONITOR_THREAD
            .lock()
            .ok()
            .and_then(|mut h| h.take())
        {
            // 重启清理路径：join 失败（线程 panic）仅记录，无法传播
            let _ = old_handle.join();
        }
    }

    // 存储到全局静态变量，供窗口过程函数访问（窗口过程是函数指针，无法捕获闭包）
    // EXPLORER_DESKTOP / TASKBAR_CREATED_MSG 保持 OnceLock：二次调用时 set 失败（旧值保留），
    // 因 Arc 引用同一底层资源且 TaskbarCreated 消息 ID 系统范围内一致，保留旧值不影响正确性。
    //
    // T11：原实现 `let _ = EXPLORER_DESKTOP.set(desktop)` 丢弃了二次调用时的 Err，
    // 静默掩盖"已设置"事实。改为显式 match 记录 debug 日志，便于排查二次调用场景
    // （如 C-014/C-015 重启监控路径先 stop 再 start 时会再次 set）。
    match EXPLORER_DESKTOP.set(desktop) {
        Ok(()) => {}
        Err(_) => tracing::debug!("EXPLORER_DESKTOP 已设置，保留旧值（仅首次调用有效）"),
    }
    // SHARED_ENGINE 和 SHARED_CONFIG 已由 start_fullscreen_monitor 设置（OnceLock 只能 set 一次）

    match std::thread::Builder::new()
        .name("mirrorstar-explorer-monitor".to_string())
        // v8.0 内存优化：监控线程仅需 Win32 回调与消息循环，512KB 足够，默认 2MB 浪费
        .stack_size(512 * 1024)
        .spawn(move || {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::*;

            // 记录线程 ID，供退出时 PostThreadMessage 唤醒消息循环
            let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            // Mutex 中毒（线程 panic）时不存储 thread_id，退出时跳过 PostThreadMessage
            let _ = EXPLORER_MONITOR_THREAD_ID
                .lock()
                .map(|mut t| *t = Some(thread_id));

            // 1. 注册 TaskbarCreated 消息（系统在 Explorer 重启时广播此消息）
            let taskbar_created =
                unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
            if taskbar_created == 0 {
                tracing::error!("注册 TaskbarCreated 消息失败");
                EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            // OnceLock 首次 set 必成功；二次 set 失败保留旧值，消息 ID 系统范围内一致
            let _ = TASKBAR_CREATED_MSG.set(taskbar_created);
            tracing::info!(msg_id = taskbar_created, "已注册 TaskbarCreated 消息监听");

            // 2. 注册窗口类
            let class_name = EXPLORER_CLASS_NAME;
            // ST-013: GetModuleHandleW 失败时显式记录并提前返回，避免 unwrap_or_default()
            // 返回 null HMODULE 静默吞掉根本原因（后续 RegisterClassW/CreateWindowExW 会
            // 因 hInstance=null 失败，但错误信息不包含"模块句柄获取失败"这一根本原因）。
            // GetModuleHandleW(None) 获取当前进程可执行文件句柄，极少失败（仅在系统资源
            // 极度匮乏时），但显式处理有助于诊断极端场景下的故障。
            let h_instance = match unsafe { GetModuleHandleW(None) } {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(error = %e, "GetModuleHandleW 失败，无法注册 Explorer 监控窗口类");
                    EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let wc = WNDCLASSW {
                lpfnWndProc: Some(explorer_monitor_wndproc),
                hInstance: windows::Win32::Foundation::HINSTANCE(h_instance.0),
                lpszClassName: class_name,
                ..Default::default()
            };

            let atom = unsafe { RegisterClassW(&wc) };
            if atom == 0 {
                tracing::error!("注册 Explorer 监控窗口类失败");
                EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                return;
            }

            // 3. 创建消息窗口（HWND_MESSAGE 作为父窗口，窗口不可见）
            let hwnd = match unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class_name,
                    class_name,
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    None,
                    h_instance,
                    None,
                )
            } {
                Ok(h) if !h.is_invalid() => {
                    tracing::info!("Explorer 重启监控消息窗口已创建");
                    h
                }
                Ok(_) => {
                    // CreateWindowExW 失败：注销已注册的窗口类，避免 atom 泄漏（T-002，与正常退出路径一致）
                    // 错误清理路径：注销失败随进程退出自动回收
                    let _ = unsafe {
                        UnregisterClassW(
                            EXPLORER_CLASS_NAME,
                            windows::Win32::Foundation::HINSTANCE(h_instance.0),
                        )
                    };
                    tracing::error!("创建 Explorer 监控窗口失败: 无效句柄");
                    EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    // CreateWindowExW 失败：注销已注册的窗口类，避免 atom 泄漏（T-002，与正常退出路径一致）
                    // 错误清理路径：注销失败随进程退出自动回收
                    let _ = unsafe {
                        UnregisterClassW(
                            EXPLORER_CLASS_NAME,
                            windows::Win32::Foundation::HINSTANCE(h_instance.0),
                        )
                    };
                    tracing::error!(error = %e, "创建 Explorer 监控窗口失败");
                    EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
            };

            // 4. 消息循环：分发消息，窗口过程会处理 TaskbarCreated
            // GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
            // （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
            let mut msg = MSG::default();
            unsafe {
                loop {
                    let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
                    match ret.0 {
                        0 => break, // WM_QUIT
                        -1 => {
                            tracing::error!("GetMessageW 返回 -1（错误），退出消息循环");
                            break;
                        }
                        _ => {}
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // 5. 清理：销毁窗口并注销窗口类，避免资源泄漏
            // GetMessageW 退出循环的两种路径（WM_QUIT / 错误）均会到达此处
            // 线程退出清理路径：失败随进程退出自动回收，无法传播错误
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = unsafe {
                UnregisterClassW(
                    EXPLORER_CLASS_NAME,
                    windows::Win32::Foundation::HINSTANCE(h_instance.0),
                )
            };
            tracing::info!("Explorer 重启监控线程退出");
        }) {
        Ok(handle) => {
            // 存储 JoinHandle，供退出或二次调用 start 时 join 等待线程退出（C-014/C-015）
            // Mutex 中毒时不存储 handle，无法 join 但线程会随进程退出
            let _ = EXPLORER_MONITOR_THREAD
                .lock()
                .map(|mut h| *h = Some(handle));
        }
        Err(e) => {
            tracing::error!(error = %e, "启动 Explorer 监控线程失败");
            // 线程 spawn 失败：重置运行标志，避免 RUNNING=true 但无实际监控线程
            EXPLORER_MONITOR_RUNNING.store(false, Ordering::SeqCst);
        }
    }
}
