use crate::state::{
    SHARED_APP_HANDLE, WORKERW_CHECK_NOTIFY, WORKERW_CHECK_RUNNING, WORKERW_CHECK_TASK,
};
use mirrorstar_core::DesktopIntegrator;
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

/// 启动 WorkerW 有效性兜底检查任务（5 分钟间隔）
///
/// 主要监控由 TaskbarCreated 事件驱动，此检查仅作为事件遗漏的最终兜底。
///
/// ST-003：保存 `JoinHandle` 到 `WORKERW_CHECK_TASK`，并在 `tokio::select!` 中
/// 同时等待 interval tick 与 `WORKERW_CHECK_NOTIFY`。`perform_shutdown_blocking`
/// 设置 `WORKERW_CHECK_RUNNING=false` + `notify_one()` 让任务立即跳出 300s tick
/// 阻塞、检查标志并退出，再 `abort()` 兜底取消。原实现 fire-and-forget，shutdown
/// 后任务最多 300s 才退出，期间可能持 desktop 锁阻塞 `engine.shutdown()`。
///
/// T07：检测到 WorkerW 失效并重新初始化后，通过 `SHARED_APP_HANDLE` emit
/// `desktop-status-changed` 事件通知前端刷新桌面状态（壁纸可能已重新嵌入）。
///
/// v16-C-007：成功与失败均 emit，payload `DesktopStatusPayload { ok, error }` 区分：
/// - 成功：`{ ok: true }`，前端可刷新显示器/壁纸状态
/// - 失败：`{ ok: false, error: Some(...) }`，前端启动 check_desktop_status 轮询
///   并在 30s 超时后提示用户"桌面状态异常，壁纸可能无法显示，请重启应用"
///
/// async 锁使用场景文档化
///
/// # 锁类型混用场景说明
///
/// 本模块涉及两类 Mutex，调用方需小心区分：
///
/// ## 1. `tokio::sync::Mutex`（async 锁）
///
/// - **使用位置**：本任务外部的 `state.wallpaper_engine`（`Arc<tokio::sync::Mutex<WallpaperEngine>>`）
/// - **使用场景**：需在持有锁期间执行 `.await`（如 `set_wallpaper` 三阶段流程的
///   `engine.lock().await`），或需跨 `.await` 点持有锁的命令
/// - **本任务不使用**：`start_workerw_check` 内部不获取 engine 锁，
///   仅通过 `desktop` 锁（std::sync::Mutex）操作 WorkerW 状态
///
/// ## 2. `std::sync::Mutex`（同步锁）
///
/// - **使用位置**：`desktop: Arc<std::sync::Mutex<DesktopIntegrator>>`（本任务参数）
/// - **使用场景**：
///   - `desktop.lock()` 获取守卫后调用同步方法 `is_workerw_valid()` /
///     `check_and_reinitialize()`（内部含 `EnumWindows` / `FindWindowW` 等 Win32 同步调用）
///   - 这些 Win32 调用本身阻塞，无法改为异步
///   - 同步锁的获取/释放开销低于 tokio Mutex（无需 await 点状态机）
/// - **阻塞窗口**：见函数体内"阻塞窗口评估"注释，单次检查通常 <50ms，
///   5 分钟间隔下可接受
///
/// ## 为什么不统一为 tokio::sync::Mutex？
///
/// - `DesktopIntegrator` 的所有方法都是同步的（Win32 API 调用），
///   改用 async 锁无实际收益
/// - 同步锁在 `spawn_blocking` 线程池中性能更优（无 future state machine 开销）
/// - `workerw_check` 任务虽运行于 tokio runtime，但 `desktop.lock()` 持有期间
///   无 `.await` 点（见函数体内"阻塞窗口评估"注释），不会阻塞 tokio 调度公平性
///
/// ## 为什么不统一为 std::sync::Mutex + spawn_blocking？
///
/// - `wallpaper_engine` 需在锁内执行 `.await`（如 `set_wallpaper` 三阶段），
///   `std::sync::Mutex` 跨 `.await` 持有会触发 clippy::await_holding_lock 警告
///   且可能导致死锁（如锁被持有期间 future 被取消，锁守卫泄漏）
/// - 因此 `wallpaper_engine` 必须使用 `tokio::sync::Mutex`
///
/// ## 调用方注意事项
///
/// - **不可在持有 `desktop` 锁期间 `.await`**：会阻塞 tokio worker 线程
///   （本任务内 `desktop.lock()` 后立即调用同步方法并释放，无 `.await`）
/// - **不可在持有 `wallpaper_engine` 锁期间获取 `desktop` 锁**：
///   会违反 state.rs 文档化的锁顺序（engine → desktop），可能形成锁环
///   （`set_wallpaper` 通过 `engine.lock().await` 后调用 engine 内部方法，
///   engine 内部方法才获取 desktop 锁，符合 engine → desktop 顺序）
pub(crate) fn start_workerw_check(desktop: Arc<std::sync::Mutex<DesktopIntegrator>>) {
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
            // ST-003: 此处使用 `std::sync::Mutex::lock()` 阻塞 tokio worker 线程。
            //
            // 阻塞窗口评估：
            // - 检查间隔 5 分钟（300s），频率极低
            // - `check_and_reinitialize` 内含 `EnumWindows` 调用，通常 <50ms
            //   （极端情况下可能阻塞数百毫秒）
            //
            // 可接受性理由：低频 + 短阻塞，不影响 tokio 调度公平性。
            // 整个 lock 持有期间无 await 点，不会跨 await 持有 std 锁。
            //
            // 未来优化方向：使用 `tokio::task::spawn_blocking` 包装锁持有段，
            // 彻底避免阻塞 tokio worker 线程。
            let mut desktop = match desktop.lock() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, "获取桌面锁失败");
                    continue;
                }
            };
            if !desktop.is_workerw_valid() {
                tracing::warn!("检测到 WorkerW 失效，尝试重新初始化...");
                // T14：check_and_reinitialize 返回 `Result<bool, MirrorStarError>`，
                // `Ok(true)` 表示实际执行了重新初始化，`Ok(false)` 表示 WorkerW 已有效无需重初始化
                match desktop.check_and_reinitialize() {
                    Ok(true) => {
                        tracing::info!("WorkerW 重新初始化成功");
                        // T07 / v16-C-007：通知前端桌面状态已变化（壁纸可能已重新嵌入）
                        emit_desktop_status(true, None);
                    }
                    Ok(false) => {
                        tracing::debug!("WorkerW 已恢复有效，无需重新初始化");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "WorkerW 重新初始化失败");
                        // v16-C-007：失败也 emit，前端据此启动轮询补救 + 超时提示
                        emit_desktop_status(false, Some(e.to_string()));
                    }
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
