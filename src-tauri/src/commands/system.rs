use crate::state::AppState;
use std::time::Duration;
use tauri::State;

/// ST-002 / ST-015: 文件对话框超时上限（5 分钟）。
///
/// 使用回调式 `pick_file()` API 后不再占用 tokio 阻塞线程池，但 rfd 对话框内部仍有一个
/// 独立 std::thread 在 `block_on` 等待用户操作。超过此时长后超时返回 None，避免命令永久挂起；
/// 超时后该内部线程可能短暂残留直至用户关闭对话框（rfd 不支持程序化关闭对话框）。
const FILE_DIALOG_TIMEOUT: Duration = Duration::from_secs(300);

#[tauri::command]
pub fn get_displays(
    state: State<'_, AppState>,
) -> Result<Vec<mirrorstar_core::config::DisplayInfo>, mirrorstar_core::MirrorStarError> {
    let desktop = state
        .desktop
        .lock()
        .map_err(|e| mirrorstar_core::MirrorStarError::LockPoisoned(format!("锁中毒: {}", e)))?;
    Ok(desktop.enumerate_displays())
}

// ST-013: check_desktop_status 保留 async 标注的设计权衡
// - 当前函数体内无 `.await`（desktop 为 std::sync::Mutex，state.desktop.lock() 为同步阻塞调用），
//   async 标注带来轻微调度开销（约 1-2 个 future state machine 状态转换）
// - 保留 async 前瞻性：未来若 `desktop` 改用 tokio::sync::Mutex（如需在持有锁时 await 其他异步操作），
//   可平滑过渡无需修改命令签名（避免破坏前端 invoke 调用契约）
// - 不改为 sync 以避免未来重复修改签名
//
// 调用方使用约定文档化
//
// # 调用方使用约定（前端 invoke 指导）
//
// 此命令当前未被前端主动调用（WorkerW 失效检测由后台 `workerw_check.rs`
// 5 分钟间隔兜底 + Explorer 重启 `TaskbarCreated` 事件驱动覆盖），
// 作为前端在收到 `desktop-status-changed` 事件后的补救轮询入口。
//
// ## 建议轮询频率
//
// - **触发式轮询**：收到 `desktop-status-changed` 事件后启动轮询
// - **轮询间隔**：2s（2000ms），平衡"快速恢复壁纸"与"避免 Win32 调用开销"
//   （单次 `check_and_reinitialize` 含 `EnumWindows` + `FindWindowW`，通常 <50ms）
// - **总超时上限**：30s（最多 15 次轮询），超时后停止轮询并提示用户
//   "桌面状态异常，请重启应用"
//
// ## 返回值语义
//
// - `Ok(true)`：WorkerW 已失效并已成功重新初始化（壁纸需重新嵌入，前端应刷新状态）
// - `Ok(false)`：WorkerW 当前有效，无需重初始化
// - `Err(...)`：desktop 锁中毒或重初始化失败（前端应记录并提示用户）
//
// ## 并发安全
//
// 函数内部通过 `state.desktop.lock()` 获取 `std::sync::Mutex` 守卫，
// 与 `get_displays` / `workerw_check` 任务共享同一 desktop 锁。
// 锁持有期间无 `.await` 点，不会跨 await 持有 std 锁。
#[tauri::command]
pub async fn check_desktop_status(
    state: State<'_, AppState>,
) -> Result<bool, mirrorstar_core::MirrorStarError> {
    let mut desktop = state
        .desktop
        .lock()
        .map_err(|e| mirrorstar_core::MirrorStarError::LockPoisoned(format!("锁中毒: {}", e)))?;
    // T14：直接消费 check_and_reinitialize 返回的 bool（是否实际执行了重初始化），
    // 不再用 !is_workerw_valid() workaround 推断——后者在 WorkerW 进入时有效但内部
    // 因其他原因重初始化时会返回错误的 false。
    desktop.check_and_reinitialize()
}

#[tauri::command]
pub async fn set_interaction_mode(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    let mut engine = state.wallpaper_engine.lock().await;
    engine.set_interaction_mode(enabled)
}

#[tauri::command]
pub async fn open_file_dialog(
    app: tauri::AppHandle,
) -> Result<Option<String>, mirrorstar_core::MirrorStarError> {
    use tauri_plugin_dialog::DialogExt;
    // ST-002 修复：使用 tauri-plugin-dialog v2 的非阻塞回调式 pick_file() API
    // + tokio::sync::oneshot 通道，替代原 spawn_blocking + blocking_pick_file 方案。
    //
    // 原实现问题（ST-002 [High] 资源管理）：spawn_blocking + blocking_pick_file 会占用
    // tokio 阻塞线程池的一个线程；超时后该线程无法回收（仍阻塞在 blocking_pick_file
    // 内部的 rx.recv()），多次超时会累积无法回收的线程，可能耗尽 tokio 阻塞线程池。
    //
    // 新实现优势：
    // 1. 不占用 tokio 阻塞线程池——pick_file() 回调内部使用 run_on_main_thread + 独立
    //    std::thread，与 tokio 运行时解耦
    // 2. 超时后 drop oneshot receiver，当用户最终关闭对话框时回调触发 send 返回 Err
    //    （receiver 已 drop），安全忽略，内部 std::thread 随即退出
    // 3. 不存在线程累积问题——每次调用至多遗留一个短暂存活的 std::thread
    //
    // 已知限制：rfd 文件对话框无法通过 API 程序化关闭。若用户长时间不关闭对话框，
    // 内部 std::thread 会持续存在直到对话框关闭。系统 UI 阻塞场景（如模态对话框卡死）
    // 需用户手动关闭对话框。超时上限 5 分钟（FILE_DIALOG_TIMEOUT）以限制单次最长占用。
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("所有文件", &["*"])
        .add_filter("图片文件", &["jpg", "jpeg", "png", "bmp", "webp"])
        .add_filter("GIF 动画", &["gif"])
        .add_filter("视频文件", &["mp4", "avi", "mkv", "mov", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "ts"])
        .pick_file(move |file_path| {
            // 对话框关闭后回调。若超时已触发，receiver 已 drop，send 返回 Err，安全忽略。
            let _ = tx.send(file_path);
        });
    match tokio::time::timeout(FILE_DIALOG_TIMEOUT, rx).await {
        Ok(Ok(file_path)) => Ok(file_path.map(|p| p.to_string())),
        Ok(Err(_)) => {
            // sender 被 drop 且未发送数据——仅在内部 std::thread 异常退出时发生
            // （如 block_on panic）。记录 warn 以便排查，返回 None 避免命令永久挂起。
            tracing::warn!("文件对话框通道异常关闭");
            Ok(None)
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = FILE_DIALOG_TIMEOUT.as_secs(),
                "文件对话框超时；后台线程可能持续到对话框手动关闭"
            );
            Ok(None)
        }
    }
}

#[tauri::command]
pub async fn toggle_auto_start(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| {
            mirrorstar_core::MirrorStarError::DesktopIntegration(format!("启用自启动失败: {}", e))
        })?;
    } else {
        manager.disable().map_err(|e| {
            mirrorstar_core::MirrorStarError::DesktopIntegration(format!("禁用自启动失败: {}", e))
        })?;
    }
    tracing::info!(enabled, "切换开机自启");
    Ok(())
}

#[tauri::command]
pub async fn get_auto_start_status(
    app: tauri::AppHandle,
) -> Result<bool, mirrorstar_core::MirrorStarError> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    manager.is_enabled().map_err(|e| {
        mirrorstar_core::MirrorStarError::DesktopIntegration(format!("查询自启动状态失败: {}", e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── ST-002 单元测试 ──
    //
    // 文件对话框本身难以自动化测试（需真实 OS 对话框交互），以下测试验证
    // open_file_dialog 所依赖的核心机制（超时常量、oneshot 通道、timeout 行为），
    // 确保超时后不会累积线程。端到端验证需手动运行：打开对话框 → 不关闭 →
    // 等待 5 分钟 → 确认命令返回 None 且应用仍可正常响应。

    /// ST-002：验证文件对话框超时常量为 5 分钟（300 秒）。
    ///
    /// 超时上限从原 10 分钟降至 5 分钟，以限制单次对话框最长占用时长，
    /// 减少超时后残留线程的存活窗口。
    #[test]
    fn st002_timeout_constant_is_5_minutes() {
        assert_eq!(
            FILE_DIALOG_TIMEOUT,
            Duration::from_secs(300),
            "FILE_DIALOG_TIMEOUT 应为 5 分钟（300 秒）"
        );
    }

    /// ST-002：验证超时后 oneshot receiver 返回 Elapsed（而非永久挂起）。
    ///
    /// 模拟文件对话框超时场景：sender 未发送任何数据但保持存活（模拟对话框未关闭、
    /// 回调线程仍在运行），timeout 在指定时长后触发返回 `Err(Elapsed)`。
    /// 这保证了 `open_file_dialog` 在超时后能正常返回 None，不会永久阻塞。
    #[tokio::test]
    async fn st002_oneshot_timeout_returns_elapsed() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<()>>();
        // 保持 sender 存活但不发送数据，模拟对话框未关闭、回调线程仍在运行的场景
        let _keep_alive = tx;
        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert!(
            result.is_err(),
            "超时后应返回 Err(Elapsed)，实际: {:?}",
            result
        );
    }

    /// ST-002：验证 sender 被 drop 后 receiver 返回 RecvError。
    ///
    /// 模拟回调线程异常退出（如 block_on panic）导致 sender 被 drop 的场景。
    /// 此时 receiver.await 返回 Err(RecvError)，`open_file_dialog` 会记录 warn
    /// 并返回 None。这保证了即使内部线程异常退出，命令也不会永久挂起。
    #[tokio::test]
    async fn st002_oneshot_dropped_sender_yields_recv_error() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<()>>();
        drop(tx); // 模拟 sender 被 drop（回调线程异常退出）
        let result = rx.await;
        assert!(
            result.is_err(),
            "sender drop 后 receiver 应返回 Err(RecvError)，实际: {:?}",
            result
        );
    }

    /// ST-002：验证正常路径——回调发送数据后 receiver 在超时前正确接收。
    ///
    /// 模拟用户正常选择文件：回调通过 oneshot 发送 Some(path)，receiver 在超时前收到。
    /// 确保新实现不破坏正常流程。
    #[tokio::test]
    async fn st002_oneshot_normal_delivery_succeeds() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(Some("test.mp4".to_string()));
        });
        let result = tokio::time::timeout(Duration::from_secs(1), rx).await;
        match result {
            Ok(Ok(Some(path))) => assert_eq!(path, "test.mp4"),
            other => panic!("期望 Ok(Ok(Some(\"test.mp4\")))，实际: {:?}", other),
        }
    }
}
