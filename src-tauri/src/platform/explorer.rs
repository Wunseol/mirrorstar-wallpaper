use std::sync::atomic::Ordering;

use crate::platform::power::handle_power_status_change;
use crate::state::{
    heal_debounce_ok, DESKTOP_LAYER_HEAL_EPOCH, DESKTOP_LAYER_HEAL_LAST_MS, EXPLORER_MONITOR_HWND,
    EXPLORER_MONITOR_RUNNING, EXPLORER_MONITOR_THREAD, EXPLORER_MONITOR_THREAD_ID,
    TASKBAR_CREATED_MSG,
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

/// 桌面层变化消息（自定义窗口消息）：WinEvent 销毁回调经 `PostMessageW` 转发到监控窗口
///
/// 取值 `WM_APP + 0x4D1`（`WM_APP = 0x8000`）：`WM_APP` 起的区间供应用私有消息使用，
/// 偏移 0x4D1 避免与 `TaskbarCreated`（`RegisterWindowMessageW` 动态分配）及
/// `src-tauri/src` 内其他私有消息冲突（全仓 Grep 确认 0x8000 段未被占用）。
const WM_APP_DESKTOP_LAYER_CHANGED: u32 = 0x8000 + 0x4D1;

/// `WTS_SESSION_UNLOCK`（会话解锁）
///
/// windows crate 未导出该常量（`Win32_System_RemoteDesktop` 仅提供注册 API 与
/// `NOTIFY_FOR_THIS_SESSION`），按 `wtsapi32.h` 定义本地常量：`WTS_SESSION_UNLOCK = 0x8`。
const WTS_SESSION_UNLOCK_VALUE: usize = 0x8;

/// 比较 `GetClassNameW` 结果与 ASCII 期望类名（零分配）
///
/// WinEvent 销毁回调是系统级高频回调，逐次 `String::from_utf16_lossy` 会带来不必要的
/// 堆分配；本类名只含 ASCII，按字节逐位比较即可（与 `desktop/worker_w.rs` 的
/// `eq_wide` 思路一致）。
fn class_name_equals_ascii(buf: &[u16], len: usize, expected: &str) -> bool {
    len == expected.len()
        && buf[..len]
            .iter()
            .zip(expected.bytes())
            .all(|(actual, want)| *actual == u16::from(want))
}

/// WorkerW / Progman 销毁事件的 WinEvent 回调
///
/// 只做"类名过滤 + `PostMessageW`"两件事：本回调经 `WINEVENT_OUTOFCONTEXT` 投递，
/// 运行在监控线程的消息循环上下文，若在回调内加锁或执行 `SetParent` / 嵌入等重操作，
/// 会阻塞该线程后续的消息分发（包括 `TaskbarCreated` / 电源 / 会话事件），
/// 因此重活统一转交窗口过程处理。
unsafe extern "system" fn desktop_layer_destroy_callback(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _ms: u32,
) {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, PostMessageW};

    if hwnd.is_invalid() {
        return;
    }

    // 类名过滤：只关心桌面层（WorkerW 承载壁纸窗口，Progman 为桌面根窗口）
    let mut class_buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    if len <= 0 {
        return;
    }
    let len = len as usize;
    if !(class_name_equals_ascii(&class_buf, len, "WorkerW")
        || class_name_equals_ascii(&class_buf, len, "Progman"))
    {
        return;
    }

    // 监控窗口句柄以数值形式存放（HWND 为裸指针类型，不能直接放进 static OnceLock）
    let Some(raw_hwnd) = EXPLORER_MONITOR_HWND.get() else {
        return;
    };
    let monitor = windows::Win32::Foundation::HWND(*raw_hwnd as *mut core::ffi::c_void);
    // PostMessageW 失败忽略：下一次事件或 300s 兜底轮询会兜底
    let _ = unsafe {
        PostMessageW(
            monitor,
            WM_APP_DESKTOP_LAYER_CHANGED,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
    };
}

/// Explorer 重启监控窗口过程
///
/// 用 `catch_unwind` 包裹消息处理体，避免窗口过程内部 panic 在 unwind 构建中
/// 向外展开并越过 `extern "system"` 边界（Win32 回调）。注意：release 构建启用
/// `panic = "abort"`，panic 会直接终止进程，本保护仅在 dev/test 构建生效。
unsafe extern "system" fn explorer_monitor_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_explorer_message(hwnd, msg, wparam, lparam)
    }));

    match result {
        Ok(lresult) => lresult,
        Err(_) => {
            // panic 已被捕获，仅记录，防止穿越 FFI 硬崩溃
            tracing::error!("Explorer 监控窗口过程 panic 已被捕获，防止穿越 FFI 硬崩溃");
            windows::Win32::Foundation::LRESULT(0)
        }
    }
}

/// 桌面层变化事件的统一触发入口（可选去抖 → [`crate::state::try_heal_desktop_layer`]）
///
/// 事件源：WorkerW/Progman 销毁（WinEvent）、`TaskbarCreated`（Explorer 重启）、
/// 会话解锁（`WM_WTSSESSION_CHANGE`）、睡眠恢复（`WM_POWERBROADCAST`）。
///
/// # `debounce` 只给销毁事件用（重要）
///
/// Explorer 重启会连续销毁多个 `WorkerW`/`Progman`，同一批事件只需自愈一次，
/// 故销毁事件走 1s 去抖。但 `TaskbarCreated` 是"新 shell 已就绪"的权威信号，
/// 且常常在第一波销毁事件后不到 1s 就到达（此时首次自愈多半因 Progman 尚未
/// 重建而失败）——若把它一起去抖，就会把这唯一一次可成功的重试吃掉，壁纸只能
/// 等 300s 兜底才恢复。会话解锁 / 睡眠恢复同属低频且语义独立，同样不去抖。
fn trigger_heal_desktop_layer(reason: &str, debounce: bool) {
    let now_ms = DESKTOP_LAYER_HEAL_EPOCH.elapsed().as_millis() as u64;
    let last_ms = DESKTOP_LAYER_HEAL_LAST_MS.load(Ordering::SeqCst);
    if debounce && !heal_debounce_ok(last_ms, now_ms) {
        tracing::debug!(
            reason,
            last_ms,
            now_ms,
            "桌面层自愈去抖窗口内（<1000ms），跳过本次触发"
        );
        return;
    }
    // 时间戳只在"真正执行过自愈"后写入：若本次因 engine 锁忙 / 守卫命中而跳过，
    // 同一批销毁事件中的后续事件仍能立即重试，避免等 300s 兜底。
    let Some(did_reinit) = crate::state::try_heal_desktop_layer() else {
        tracing::debug!(
            reason,
            "本次桌面层自愈未执行（锁忙/守卫/失败），后续事件可立即重试"
        );
        return;
    };
    DESKTOP_LAYER_HEAL_LAST_MS.store(now_ms, Ordering::SeqCst);
    tracing::info!(reason, did_reinit, "桌面层自愈完成");
}

/// Explorer 重启监控窗口过程的消息处理体
///
/// 统一处理桌面层相关事件，均经 [`trigger_heal_desktop_layer`] 转 `state::try_heal_desktop_layer`：
/// - `TaskbarCreated`：Explorer 重启时系统向所有顶层窗口广播（**不去抖**，是新 shell 就绪的权威信号）
/// - `WM_APP_DESKTOP_LAYER_CHANGED`：WinEvent 回调转发 WorkerW/Progman 销毁（走 1s 去抖）
/// - `WM_WTSSESSION_CHANGE`（解锁）：锁屏解锁后桌面层可能被重建（不去抖）
/// - `WM_POWERBROADCAST`：电源状态变化暂停/恢复；睡眠恢复触发自愈（不去抖）
fn handle_explorer_message(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC,
        PBT_APMRESUMESUSPEND, WM_DESTROY, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE,
    };

    // 检查是否为 TaskbarCreated 消息
    if let Some(&taskbar_created) = TASKBAR_CREATED_MSG.get() {
        if msg == taskbar_created {
            tracing::info!("收到 TaskbarCreated 消息，Explorer 可能已重启，触发桌面层自愈");
            trigger_heal_desktop_layer("TaskbarCreated", false);
            return windows::Win32::Foundation::LRESULT(0);
        }
    }

    // WorkerW / Progman 被销毁（WinEvent 回调转发）
    if msg == WM_APP_DESKTOP_LAYER_CHANGED {
        trigger_heal_desktop_layer("desktop_layer_destroyed", true);
        return windows::Win32::Foundation::LRESULT(0);
    }

    // 会话解锁：锁屏期间桌面层可能被重建，解锁后立即自愈
    if msg == WM_WTSSESSION_CHANGE {
        if wparam.0 == WTS_SESSION_UNLOCK_VALUE {
            trigger_heal_desktop_layer("session_unlock", false);
        }
        return windows::Win32::Foundation::LRESULT(0);
    }

    if msg == WM_POWERBROADCAST {
        if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
            handle_power_status_change();
        } else if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize
            || wparam.0 == PBT_APMRESUMESUSPEND as usize
        {
            // 睡眠/休眠恢复：DWM 重置可能使桌面句柄与 D3D 呈现失效
            trigger_heal_desktop_layer("power_resume", false);
        }
        return windows::Win32::Foundation::LRESULT(0);
    }

    if msg == WM_DESTROY {
        // ST-018: 防御性处理——若外部代码（如系统或另一线程）通过 DestroyWindow 销毁本窗口，
        // WM_DESTROY 会在消息循环内被 dispatch。此处 PostQuitMessage 使下一次 GetMessageW
        // 返回 0 退出循环，确保监控线程能正常退出而不是无限等待已销毁窗口的消息。
        unsafe { PostQuitMessage(0) };
        return windows::Win32::Foundation::LRESULT(0);
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 启动 Explorer 重启监控线程（事件驱动，统一的桌面层变化事件源）
///
/// 创建一个不可见的消息窗口（HWND_MESSAGE 子窗口），在该线程的消息循环内监听：
/// - `TaskbarCreated`：Explorer 重启时系统广播
/// - WorkerW/Progman 销毁：`SetWinEventHook(EVENT_OBJECT_DESTROY)` + WinEvent 回调转发
/// - 会话解锁：`WTSRegisterSessionNotification` + `WM_WTSSESSION_CHANGE`
/// - 睡眠恢复：`WM_POWERBROADCAST`（`PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND`）
///
/// 所有事件统一经 `state::try_heal_desktop_layer` 自愈（销毁事件 1s 去抖，其余事件
/// 直接触发）；300s 轮询仅作兜底。不再需要 `desktop` Arc 入参——桌面句柄由引擎内部
/// 持有并按 engine → desktop 顺序取锁。
pub(crate) fn start_explorer_restart_monitor() {
    // C-014/C-015 修复：二次调用时先 stop 旧监控再 start
    // swap 返回旧值：若为 true 表示已有监控在运行，需先停止旧监控线程，
    // 避免旧线程消息循环泄漏。swap 同时将标志置为 true。
    if EXPLORER_MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        tracing::info!("Explorer 监控已在运行，先停止旧监控再重启");
        // 1. take 旧 thread_id 并 PostThreadMessage WM_QUIT 唤醒
        crate::state::signal_thread_exit(&EXPLORER_MONITOR_THREAD_ID);
        // 2. take 旧 JoinHandle 并 join 等待旧线程退出
        crate::state::join_monitor_thread(&EXPLORER_MONITOR_THREAD);
    }

    // 全局静态量说明（窗口过程 / WinEvent 回调是函数指针，无法捕获闭包）：
    // - TASKBAR_CREATED_MSG / EXPLORER_MONITOR_HWND 均为 OnceLock，首次 set 后保留。
    // - 二次调用时旧监控窗口已销毁，EXPLORER_MONITOR_HWND 保留旧句柄会使 WinEvent
    //   回调的 PostMessageW 失败（已忽略），但 TaskbarCreated / 电源 / 解锁事件源不受影响。
    //   实际调用路径仅 `lib.rs run()` 一次，该降级仅在未来的重启路径生效。
    // - SHARED_ENGINE / SHARED_CONFIG 已由 start_fullscreen_monitor 设置（OnceLock 只能 set 一次）。

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

            // 3. 创建隐藏的顶层窗口
            //
            // **刻意不用 `HWND_MESSAGE`**（与 `pure_mode_input.rs` 同一约定）：消息专用窗口
            // 按文档不接收 `HWND_BROADCAST` 广播，而 `TaskbarCreated` 与 `WM_POWERBROADCAST`
            // 都是广播消息——挂在消息窗口上会永远收不到，事件驱动自愈形同失效。
            // 这里用 `WS_EX_TOOLWINDOW | WS_POPUP` 且**从不 ShowWindow**：不可见、不进
            // Alt+Tab / 任务栏，但参与广播投递。
            let hwnd = match unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class_name,
                    class_name,
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    HWND::default(),
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

            // 3.1 保存监控窗口句柄，供 WinEvent 销毁回调 PostMessage 事件
            // （HWND 为裸指针类型 !Send/!Sync，全局静态量中存句柄数值）
            let _ = EXPLORER_MONITOR_HWND.set(hwnd.0 as usize);

            // 3.2 注册 WorkerW/Progman 销毁事件钩子
            // WINEVENT_OUTOFCONTEXT：回调经本线程消息队列投递；SKIPOWNPROCESS：忽略自身窗口。
            // 失败仅 warn，不中断线程——TaskbarCreated / 会话解锁 / 电源恢复事件源仍可用。
            let hook = unsafe {
                windows::Win32::UI::Accessibility::SetWinEventHook(
                    EVENT_OBJECT_DESTROY,
                    EVENT_OBJECT_DESTROY,
                    None,
                    Some(desktop_layer_destroy_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if hook.0.is_null() {
                tracing::warn!(
                    "SetWinEventHook(EVENT_OBJECT_DESTROY) 失败，WorkerW/Progman 销毁事件不可用\
                     （其余事件源与 300s 兜底轮询仍生效）"
                );
            }

            // 3.3 注册会话解锁通知（WM_WTSSESSION_CHANGE）
            // 失败仅 warn：其余事件源仍可用，不影响监控线程存活。
            if let Err(e) = unsafe {
                windows::Win32::System::RemoteDesktop::WTSRegisterSessionNotification(
                    hwnd,
                    windows::Win32::System::RemoteDesktop::NOTIFY_FOR_THIS_SESSION,
                )
            } {
                tracing::warn!(
                    error = %e,
                    "WTSRegisterSessionNotification 失败，会话解锁事件不可用（其余事件源仍生效）"
                );
            }

            // 4. 消息循环：分发消息，窗口过程处理 TaskbarCreated / 桌面层变化 / 电源 / 解锁
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

            // 5. 清理：卸载事件钩子 / 退出会话通知注册，再销毁窗口、注销窗口类
            // GetMessageW 退出循环的两种路径（WM_QUIT / 错误）均会到达此处
            // 线程退出清理路径：失败忽略（钩子与通知注册随进程退出由系统回收），无法传播错误
            if !hook.0.is_null() {
                let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(hook) };
            }
            let _ = unsafe {
                windows::Win32::System::RemoteDesktop::WTSUnRegisterSessionNotification(hwnd)
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 类名比较：命中 WorkerW / Progman，拒绝长度不符、内容不符与大小写不同的类名。
    #[test]
    fn class_name_equals_ascii_matrix() {
        let workerw: Vec<u16> = "WorkerW".encode_utf16().collect();
        assert!(class_name_equals_ascii(&workerw, workerw.len(), "WorkerW"));
        assert!(!class_name_equals_ascii(&workerw, workerw.len(), "Progman"));
        assert!(
            !class_name_equals_ascii(&workerw, workerw.len() - 1, "WorkerW"),
            "长度不符必须拒绝（避免前缀误判）"
        );
        assert!(
            !class_name_equals_ascii(&workerw, workerw.len(), "workerw"),
            "类名比较应区分大小写"
        );

        let progman: Vec<u16> = "Progman".encode_utf16().collect();
        assert!(class_name_equals_ascii(&progman, progman.len(), "Progman"));

        // 真实调用里缓冲区有尾部 0，只比较前 len 个字符
        let mut padded = vec![0u16; 64];
        padded[..workerw.len()].copy_from_slice(&workerw);
        assert!(class_name_equals_ascii(&padded, workerw.len(), "WorkerW"));
    }
}
