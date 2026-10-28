use crate::state::{
    SHARED_APP_HANDLE, WORKERW_CHECK_NOTIFY, WORKERW_CHECK_RUNNING, WORKERW_CHECK_TASK,
};
use mirrorstar_core::WallpaperEngine;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::Emitter;

/// v16-C-007：desktop-status-changed 事件 payload。
///
/// WorkerW 重新初始化结果（成功/失败）通过此 payload 传达给前端，
/// 前端据此决定是否启动 check_desktop_status 轮询补救 + 超时提示。
/// - `ok: true`：WorkerW 已恢复，前端可刷新显示器/壁纸状态
/// - `ok: false`：WorkerW 重新初始化失败，前端应启动轮询并在超时后提示用户
#[derive(Clone, serde::Serialize)]
struct DesktopStatusPayload {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 启动桌面层自愈兜底检查任务（300s / 5 分钟间隔）
///
/// 主要监控由 Explorer 监控窗口事件驱动（WorkerW/Progman 销毁、TaskbarCreated、
/// 会话解锁、睡眠恢复），此任务仅作为事件全部遗漏的最终兜底。
/// tick 内调用 `WallpaperEngine::heal_desktop_layer()`：重初始化桌面句柄 → 重嵌存活
/// 窗口 → 重建已销毁的渲染器（暂停中不重建），语义与事件路径完全一致。
///
/// ST-003：保存 `JoinHandle` 到 `WORKERW_CHECK_TASK`，并在 `tokio::select!` 中
/// 同时等待 interval tick 与 `WORKERW_CHECK_NOTIFY`。`perform_shutdown_blocking`
/// 设置 `WORKERW_CHECK_RUNNING=false` + `notify_one()` 让任务立即跳出 300s tick
/// 阻塞、检查标志并退出，再 `abort()` 兜底取消。原实现 fire-and-forget，shutdown
/// 后任务最多 300s 才退出，期间可能持 engine 锁阻塞 `engine.shutdown()`。
///
/// T07：桌面层自愈实际重初始化桌面句柄后，通过 `SHARED_APP_HANDLE` emit
/// `desktop-status-changed` 事件通知前端刷新桌面状态（壁纸可能已重新嵌入）。
///
/// v16-C-007：成功与失败均 emit，payload `DesktopStatusPayload { ok, error }` 区分：
/// - 成功：`{ ok: true }`，前端可刷新显示器/壁纸状态
/// - 失败：`{ ok: false, error: Some(...) }`，前端启动 check_desktop_status 轮询
///   并在 30s 超时后提示用户"桌面状态异常，壁纸可能无法显示，请重启应用"
///
/// async 锁使用场景文档化
///
/// # 锁使用说明
///
/// 本任务使用 engine 锁（`tokio::sync::Mutex<WallpaperEngine>`，即 `AppState.wallpaper_engine`
/// 的克隆）：
///
/// - `engine.lock().await` 获取守卫后调用同步方法 `heal_desktop_layer()`；
/// - 引擎内部按 state.rs 文档化的 **engine → desktop** 顺序访问
///   `desktop`（`std::sync::Mutex<DesktopIntegrator>`），desktop 锁仅在
///   重初始化 / 重嵌 / 重建期间短暂持有，符合锁顺序约定，不会形成锁环；
/// - 任务本身不再直接持有 `desktop` 锁，也不再使用 `std::sync::Mutex`。
///
/// ## 为什么用 tokio::sync::Mutex 而非 std::sync::Mutex
///
/// - engine 锁全局统一为 `tokio::sync::Mutex`（命令层 / 托盘 / 后台任务共用同一
///   `Arc`），本任务必须使用同类型锁才能与这些路径互斥；
/// - 本任务运行于 tokio runtime，`lock().await` 是标准用法，且引擎调用结束后立即
///   释放锁，不跨 `await` 持有。
///
/// ## 阻塞窗口评估
///
/// `heal_desktop_layer()` 为同步调用（内部含 `EnumWindows` / `SetParent` 等 Win32
/// 调用，必要时还会重建渲染器），持有 engine 锁期间会阻塞 tokio worker 线程。
/// 补偿措施：检查间隔 300s（低频）、`heal_desktop_layer` 仅在桌面层确实变化时
/// 才执行重活（渲染器存活时只做重嵌），且锁在 emit 前释放。
///
/// ## 调用方注意事项
///
/// - 不可在持有 engine 锁期间再获取 `desktop` 锁（违反 engine → desktop 顺序）；
///   引擎内部方法自行按序获取，任务侧无需（也不应）直接碰 desktop。
pub(crate) fn start_workerw_check(engine: Arc<tokio::sync::Mutex<WallpaperEngine>>) {
    // v8.0 内存优化说明：本任务为真正的 async 逻辑（tokio::select! + interval + Notify），
    // 运行于 tokio worker 线程池，无法通过 std::thread::Builder 设置独立栈大小。
    // 改用 spawn_blocking + 专用小栈线程需重写 ST-003 关闭协调（WORKERW_CHECK_TASK
    // JoinHandle、WORKERW_CHECK_NOTIFY），超出本次优化范围。任务体主要为 await
    // （5 分钟一次 tick），栈占用极小，暂不优化。
    let handle = tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        // ST-003：跳过累积的错过 tick（任务被长时间挂起后不会补跑多次检查）
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // 同时等待 interval tick 与 shutdown notify：
            // - tick 触发 → 执行常规检查
            // - notify 触发 → 立即检查 RUNNING 标志，shutdown 时无需等 300s
            tokio::select! {
                _ = interval.tick() => {}
                _ = WORKERW_CHECK_NOTIFY.notified() => {}
            }
            if !WORKERW_CHECK_RUNNING.load(Ordering::SeqCst) {
                break;
            }
            // 原子交换守卫（与事件路径的 `try_heal_desktop_layer` 对齐）：换槽 build 阶段
            // 新渲染器尚未注册，此刻重嵌旧窗口可能干扰 DefView 锚点/父链校验，跳过本 tick。
            if crate::state::atomic_swap_in_progress() {
                tracing::debug!("原子交换进行中，跳过本次桌面层兜底自愈");
                continue;
            }
            // 桌面层自愈：重初始化桌面句柄 + 修复已脱离窗口 + 重建已销毁渲染器
            // （暂停中不重建）。锁在 emit 前释放，避免 emit 期间持 engine 锁。
            //
            // 阻塞窗口评估：
            // - 检查间隔 300s，频率极低
            // - `heal_desktop_layer` 内含 `EnumWindows` / `SetParent` 等同步 Win32 调用，
            //   渲染器仍存活时通常 <100ms；仅在桌面层被销毁时才重建渲染器（耗时更长），
            //   属事件遗漏的兜底路径，可接受。
            let healed = {
                let mut engine = engine.lock().await;
                engine.heal_desktop_layer()
            };
            // T14 / v16-C-007：`Ok(true)` 表示桌面句柄被实际重初始化，emit 通知前端；
            // `Ok(false)` 表示无变化（不 emit），`Err` 表示自愈失败（emit 让前端补救）。
            match healed {
                Ok(true) => {
                    tracing::info!("桌面层自愈完成（桌面句柄已重初始化）");
                    // T07 / v16-C-007：通知前端桌面状态已变化（壁纸可能已重新嵌入）
                    emit_desktop_status(true, None);
                }
                Ok(false) => {
                    tracing::debug!("桌面层无需重初始化（无变化，不 emit）");
                }
                Err(e) => {
                    tracing::error!(error = %e, "桌面层自愈失败");
                    // v16-C-007：失败也 emit，前端据此启动轮询补救 + 超时提示
                    emit_desktop_status(false, Some(e.to_string()));
                }
            }
        }
    });

    // ST-003：保存 JoinHandle，供 perform_shutdown_blocking abort
    // Mutex 中毒时不存储 handle，任务会随进程退出
    if let Err(e) = WORKERW_CHECK_TASK.lock().map(|mut t| *t = Some(handle)) {
        tracing::warn!(error = ?e, "WORKERW_CHECK_TASK 锁中毒，无法存储 JoinHandle");
    }
}

/// v16-C-007：emit `desktop-status-changed` 事件。
///
/// 成功 (`ok: true`) 与失败 (`ok: false`) 均会 emit，让前端能感知两种状态。
/// SHARED_APP_HANDLE 未设置时静默跳过（启动早期，前端尚未就绪）。
fn emit_desktop_status(ok: bool, error: Option<String>) {
    let Some(app_handle) = SHARED_APP_HANDLE.get() else {
        tracing::debug!(
            "SHARED_APP_HANDLE 未设置，跳过 desktop-status-changed emit（ok={})",
            ok
        );
        return;
    };
    let payload = DesktopStatusPayload { ok, error };
    if let Err(e) = app_handle.emit("desktop-status-changed", payload) {
        tracing::warn!(
            error = %e,
            "emit desktop-status-changed 失败：前端 UI 可能不刷新"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── v16-C-007: DesktopStatusPayload 序列化与 emit_desktop_status 语义测试 ──

    /// 测试成功 payload 序列化：`{ ok: true }`，error 字段因 skip_serializing_if 省略。
    ///
    /// 前端 listener 类型 `{ ok: boolean; error?: string }` 据此解包。
    #[test]
    fn test_desktop_status_payload_success_serializes_ok_only() {
        let payload = DesktopStatusPayload {
            ok: true,
            error: None,
        };
        let json = serde_json::to_string(&payload).expect("序列化成功 payload 失败");
        assert!(
            json.contains("\"ok\":true"),
            "成功 payload 应包含 ok:true，实际: {}",
            json
        );
        assert!(
            !json.contains("error"),
            "error 为 None 时应被 skip_serializing_if 省略，实际: {}",
            json
        );
    }

    /// 测试失败 payload 序列化：`{ ok: false, error: "..." }`，
    /// 前端据此进入 check_desktop_status 轮询补救路径。
    #[test]
    fn test_desktop_status_payload_failure_serializes_ok_and_error() {
        let payload = DesktopStatusPayload {
            ok: false,
            error: Some("WorkerW init failed".to_string()),
        };
        let json = serde_json::to_string(&payload).expect("序列化失败 payload 失败");
        assert!(
            json.contains("\"ok\":false"),
            "失败 payload 应包含 ok:false，实际: {}",
            json
        );
        assert!(
            json.contains("\"error\":\"WorkerW init failed\""),
            "失败 payload 应包含 error 字段，实际: {}",
            json
        );
    }

    /// 测试 emit_desktop_status 在 SHARED_APP_HANDLE 未设置时为安全 no-op。
    ///
    /// 单元测试环境下 SHARED_APP_HANDLE 未被 set（setup hook 未执行），
    /// emit_desktop_status 应静默跳过不 panic——这是启动早期 WorkerW 预初始化
    /// 线程先于 setup 完成时的安全降级行为。
    ///
    /// 注：无法在单元测试中设置真实的 AppHandle（需 Tauri runtime），
    /// 此测试验证 "handle 未设置 → no-op" 路径；"handle 已设置 → emit" 路径
    /// 由集成测试 / 手动测试覆盖。
    #[test]
    fn test_emit_desktop_status_noop_when_handle_unset() {
        // SHARED_APP_HANDLE 在测试进程中未 set，emit_desktop_status 应直接返回
        // 不应 panic，不应尝试 emit
        emit_desktop_status(true, None);
        emit_desktop_status(false, Some("test error".to_string()));
        // 若执行到此行未 panic，测试通过
    }
}
