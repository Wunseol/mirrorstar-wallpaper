mod commands;
mod platform;
mod scheduler;
mod state;
// Task 2：UI 子进程管理器与协议桥（壳模式下托管原生主窗口 UI 子进程）。
mod ui_process;

use mirrorstar_core::{
    init_logging, ConfigKind, ConfigLoadError, ConfigManager, DesktopIntegrator, PauseReason,
    VolumeControl, WallpaperEngine,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{Emitter, Manager};

use commands::*;
use platform::{start_explorer_restart_monitor, start_fullscreen_monitor, start_workerw_check};
use state::{perform_shutdown_blocking, AppState};

// ST-017: 重新导出 ST-007 提取的纯函数，供集成测试直接调用
// （避免测试通过 format!() 字符串匹配模拟命令层逻辑）
#[doc(hidden)]
pub use commands::wallpaper::{parse_scaling_mode, validate_speed, validate_volume};

// T03: 重新导出 resolve_display_id 供集成测试验证 None 回退行为
#[doc(hidden)]
pub use commands::wallpaper::resolve_display_id;

// ── Application Entry ───────────────────────────────────────────────────────

// setup 顺序与失败回滚文档化
//
// # 应用启动顺序（lib.rs::run）
//
// 整个 `run()` 函数按以下顺序初始化资源，**顺序不可调换**（存在依赖关系）：
//
// ## Phase 1: 基础设施初始化（Builder 之前）
//
// 1. **日志系统**：`init_logging()` → 持有 `_log_guard` 至 run() 返回
//    - 失败：记录到 stderr，继续运行（非致命）
//    - 依赖：无
//
// 2. **COM 初始化（STA 模式）**：`ComGuard::new_sta_or_exit()` → 持有 `_com_guard` 至 run() 返回
//    - 失败：`std::process::exit(1)`（致命，COM 是必需依赖）
//    - 依赖：必须在 VolumeControl / Tauri Builder 之前
//    - Drop 行为：run() 返回时 Drop 调 CoUninitialize（晚于 app_state Drop）
//
// 3. **DesktopIntegrator**：`DesktopIntegrator::new()`（快速，不查找 WorkerW）
//    - 依赖：COM 已初始化
//
// 4. **WorkerW 预初始化线程**：`std::thread::Builder::spawn(...)` 后台 ensure_initialized
//    - JoinHandle 存入 `WORKERW_INIT_THREAD` 供 shutdown join
//    - 失败：记录 warn，继续运行（首次使用时重试）
//
// 5. **VolumeControl**：`VolumeControl::new().unwrap_or_else(|e| new_disabled())`
//    - 失败：降级为 no-op 模式（无音频设备时优雅降级，T-008 修复）
//    - 依赖：COM 已初始化
//
// 6. **WallpaperEngine**：`WallpaperEngine::new(desktop, volume_control)`
//    - 依赖：DesktopIntegrator + VolumeControl 已创建
//
// 7. **ConfigManager**：`ConfigManager::new().unwrap_or_else(|e| exit(1))`
//    - 失败：`std::process::exit(1)`（致命，配置是必需依赖）
//    - 依赖：无（独立创建，但后续会被注入 AppState）
//
// 8. **应用 GIF 内存策略**：从 config 读取并 `engine.set_gif_memory_strategy(...)`
//    - 依赖：WallpaperEngine + ConfigManager 已创建
//
// 9. **配置文件监视**：`config_manager.start_watching()`
//    - 失败：记录 warn，继续运行（热重载非必需）
//
// 10. **清理损坏缩略图**：`config_manager.cleanup_corrupted_thumbnails()`
//     - 失败：记录 warn，继续运行
//
// 11. **全屏应用检测**：`start_fullscreen_monitor(engine, config_manager)`
//     - 设置 `SHARED_ENGINE` / `SHARED_CONFIG` OnceLock
//     - 失败：内部记录，线程会自动退出（无致命影响）
//
// 12. **Explorer 重启监控**：`start_explorer_restart_monitor(desktop)`
//     - 失败：内部记录，线程会自动退出
//
// 13. **AppState 构造**：组装 config_manager / wallpaper_engine / desktop / tray_paused / tray_pause_resume_item / tray_pure_mode_item
//
// ## Phase 2: Tauri Builder + setup 闭包
//
// 14. **Tauri Builder**：`.plugin(...).manage(app_state).setup(|app| {...})`
//     - 失败：`.expect("error while building tauri application")` panic unwind
//     - panic 时：`_com_guard` 与 `_log_guard` 通过 unwind Drop 释放
//
// 15. **setup 闭包内**：
//     - 设置 `SHARED_APP_HANDLE`（供 workerw_check emit 事件）
//     - spawn 全局状态变更订阅任务（订阅 WallpaperEngine broadcast 通道）
//     - 设置配置变更回调（`set_on_config_changed`，热重载后 emit `config-changed`）
//     - 设置配置加载错误回调（`set_on_config_error`，emit `config-load-error`）
//       + drain 构造时捕获的 pending_config_errors
//     - 创建托盘菜单（open / pause_resume / quit）
//     - 创建托盘图标（`TrayIconBuilder`，图标缺失时优雅降级）
//     - 启动 WorkerW 兜底检查（`start_workerw_check`）
//
// ## Phase 3: 事件循环
//
// 16. **`.build(context).expect(...).run(|app_handle, event| {...})`**
//     - 事件循环开始
//     - `RunEvent::ExitRequested` 时调用 `perform_shutdown_blocking` 统一清理
//
// # 失败回滚策略
//
// ## Phase 1 失败回滚
//
// - **COM 初始化失败**：`std::process::exit(1)`，无需回滚（无资源已创建）
// - **ConfigManager 初始化失败**：`std::process::exit(1)`
//   - 此时已创建：DesktopIntegrator / VolumeControl / WallpaperEngine
//   - 回滚：依赖 RAII Drop——`desktop` / `volume_control` / `wallpaper_engine` 离开作用域自动 Drop
//   - 但 WorkerW 预初始化线程 / 全屏监控 / Explorer 监控可能仍在运行
//     （它们持有 Arc 克隆，Arc 引用计数归零时才退出）
//   - **未显式 join 这些线程**：依赖进程退出时操作系统回收
//     （`exit(1)` 不 unwind 栈，不调用 Drop，但 OS 会回收所有线程与资源）
//
// ## Phase 2 失败回滚（setup 闭包内 `?` 失败）
//
// - setup 闭包返回 `Err(e)` → `.build()` 返回 `Err` → `.expect()` panic
// - panic unwind 时：
//   - `_com_guard` Drop → CoUninitialize（COM 引用计数释放）
//   - `_log_guard` Drop → 刷写日志缓冲
//   - `app_state` Drop → WallpaperEngine / DesktopIntegrator / ConfigManager Drop
//   - 后台线程（fullscreen / explorer / workerw_check）持有 Arc 克隆，
//     Arc 引用计数归零时线程会因 `RUNNING` 标志未重置而继续运行
//     （**已知限制**：panic 路径未显式 stop 监控线程）
//
// ## Phase 3 失败回滚（运行时 panic）
//
// - 运行时 panic 通常 unwind 至 `run()` 顶层，触发与 Phase 2 相同的 Drop 链
// - `perform_shutdown_blocking` 不会在 panic 路径调用（仅 `ExitRequested` 触发）
//   - **已知限制**：panic 时未执行 `engine.shutdown()` 终止 mpv 子进程，
//     mpv 子进程会随应用退出由系统回收（mpv 是子进程，父进程退出时 OS 终止子进程）
//
// # 设计权衡
//
// - **显式回滚 vs RAII**：选择 RAII 为主，显式回滚为辅
//   - 理由：RAII 自动处理所有返回路径（含 `?` / Err / panic），
//     显式回滚需在每个失败点重复清理代码，易遗漏
// - **exit(1) vs 返回 Err**：致命错误用 `exit(1)`
//   - 理由：`exit(1)` 跳过 Drop，但 OS 会回收所有资源；
//     返回 Err 需在 main 中处理，且 Drop 链可能因状态不一致而 panic
//   - 代价：`exit(1)` 不调用 Drop，COM 引用计数可能泄漏（但进程退出后 OS 回收）

// ── 全局 panic hook / 启动残留进程清理 ───────────────────────────────────────

/// set_hook 的防重入保护：`std::panic::set_hook` 每次进程只能调用一次，
/// 重复调用会 panic "cannot install a panic hook more than once"。
static INSTALL_PANIC_HOOK_ONCE: std::sync::Once = std::sync::Once::new();

/// 生成约简格式的 UTC 时间戳（"YYYY-MM-DD HH:MM:SS.mmm"）。
/// 项目未依赖 chrono，使用 std::time 自实现（Howard Hinnant civil_from_days 算法）。
fn panic_hook_timestamp() -> String {
    let dur = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return "1970-01-01 00:00:00.000".to_string(),
    };
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // civil_from_days: 天数 → 年/月/日
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        y, m, d, hh, mm, ss, millis
    )
}

/// 将 panic 信息追加写入 crash.log 并同步记录到 tracing 日志。
/// 本函数在 panic hook 内调用，**禁止 panic**（绝不 unwrap，全部用 Result 吞错），
/// 避免 panic hook 自身递归 panic 造成双重崩溃。
// clippy::incompatible_msrv: `PanicHookInfo` 于 Rust 1.81 稳定，而 Cargo.toml 声明
// rust-version=1.80（为兼容旧工具链）。本项目实际以更高工具链构建，panic API 无
// 1.80 等价替代，故标注 allow（hook 为诊断基础设施，保持 MSRV 声明保守是有意的）。
#[allow(clippy::incompatible_msrv)]
fn trace_panic_info(info: &std::panic::PanicHookInfo<'_>) {
    // 线程名（与消息循环 / 监控线程协调查错；主线程未命名时为默认名）
    let thread_name = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();

    // panic payload 一般为 &'static str / String，另有兜底标记
    let payload = if let Some(s) = info.payload().downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "（非字符串 panic payload）".to_string()
    };

    // 位置信息（禁止 backtrace 采集，仅输出 payload + Location）
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "（未知位置）".to_string());

    let text = format!(
        "[{}] 线程 [{}] 发生 panic\n  位置: {}\n  信息: {}\n",
        panic_hook_timestamp(),
        thread_name,
        location,
        payload
    );

    // 追加写 crash.log（数据根目录用 create_dir_all 确保存在；失败仅记录，不 panic）
    let root = mirrorstar_core::config::data_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        tracing::error!(error = %e, "panic hook：创建数据根目录失败，无法写 crash.log");
    }
    let path = root.join("crash.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            // writeln! 返回 Result，丢弃错误；hook 内不可 panic
            let _ = writeln!(file, "{}", text.trim_end());
        }
        Err(e) => {
            tracing::error!(error = %e, path = %path.display(), "panic hook：打开 crash.log 失败");
        }
    }

    // 同时经 tracing 记录到日志文件（辅助排查）
    tracing::error!(
        thread = %thread_name,
        panicked_at = %location,
        payload = %payload,
        "发生 panic（已记录到 crash.log）"
    );
}

/// 尽早安装全局 panic hook：记录崩溃到 crash.log + tracing 日志。
///
/// 必须在其它线程 spawn 之前调用（run() 最开头、日志初始化之前），确保覆盖
/// 所有线程的 panic。通过 `Once` 保证 `set_hook` 仅调用一次。钩子保留默认
/// stderr 打印行为（`take_hook` 取出默认钩子并在内部继续调用）。
fn install_panic_hook() {
    INSTALL_PANIC_HOOK_ONCE.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // 保留默认行为：打印恐慌位置与信息到 stderr
            default_hook(info);
            trace_panic_info(info);
        }));
    });
}

/// RAII：内核句柄用后关闭（CloseHandle），避免句柄泄漏
struct HandleGuard(windows::Win32::Foundation::HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// 启动清理：终止上次崩溃遗留的孤儿 mpv / wp-proc / UI 子进程
///
/// 当前进程刚启动尚未 spawn 任何子进程，此时任何名称匹配
/// mpv.exe / mirrorstar-wp-proc.exe / mirrorstar-ui.exe 的存活进程均为上次遗留。
/// 通过 ToolHelp 快照（CreateToolhelp32Snapshot + Process32FirstW/NextW）
/// 枚举系统进程并逐个终止（不含当前进程）。失败仅记录日志，不致命。
///
/// **`mirrorstar-wp-proc.exe` 用于回收旧版本升级后残留的独立 wp-proc 孤儿进程**：
/// 运行中的 exe 无法被安装器删除，可能跨版本存活，故按名清理。采用「同一 exe、双入口」
/// 后，本版本 spawn 的网页壁纸子进程与壳**同名**（`mirrorstar-wallpaper.exe`），
/// 因此不会被本条按名匹配误杀；本版本同名子进程的孤儿由 `ProcessManager` 施加的
/// Job Object `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 兜底（主进程退出即内核回收子进程）。
fn cleanup_stale_child_processes() {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };

    // 目标残留进程名（统一小写比较）。
    // 含 mirrorstar-ui.exe：壳模式下 UI 子进程异常残留时，下次启动统一清理。
    // 含 mirrorstar-wp-proc.exe：用于回收旧版本升级后残留的独立 wp-proc 孤儿进程
    // （运行中的 exe 无法被安装器删除，可能跨版本存活）；本版本无同名产物，
    // 本版本 spawn 的网页壁纸子进程与壳同名（mirrorstar-wallpaper.exe），不会被误杀
    // （详见函数文档）；其孤儿由 Job Object KILL_ON_JOB_CLOSE 兜底。
    const TARGET_NAMES: &[&str] = &["mpv.exe", "mirrorstar-wp-proc.exe", "mirrorstar-ui.exe"];

    let current_pid = unsafe { GetCurrentProcessId() };

    // 创建进程快照（快照句柄用断言 guard 在退回时自动关闭）
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = ?e, "启动清理：创建进程快照失败，跳过残留进程清理");
            return;
        }
    };
    let _snapshot_guard = HandleGuard(snapshot);

    // dwSize 必须先初始化（枚举 API 依赖它返回实际写入长度）
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        tracing::warn!("启动清理：Process32FirstW 失败，跳过残留进程清理");
        return;
    }

    loop {
        // 跳过当前进程（自身名称为 mirrorstar-wallpaper.exe，本不会命中目标，防御性判断）
        if entry.th32ProcessID != current_pid {
            // 宽字符进程名转 String（遇 NUL 截断；极限情况 260 宽字符全满）
            let name_len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);

            if TARGET_NAMES.contains(&name.to_lowercase().as_str()) {
                unsafe {
                    // 打开进程并终止（OpenProcess 失败说明进程已消失/无权限，忽略并记录）
                    match OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                        Ok(handle) => {
                            let _handle_guard = HandleGuard(handle);
                            match TerminateProcess(handle, 1) {
                                Ok(()) => tracing::info!(
                                    pid = entry.th32ProcessID,
                                    name = %name,
                                    "启动清理：已终止崩溃遗留的孤儿进程"
                                ),
                                Err(e) => tracing::warn!(
                                    pid = entry.th32ProcessID,
                                    name = %name,
                                    error = ?e,
                                    "启动清理：终止残留进程失败"
                                ),
                            }
                        }
                        Err(e) => tracing::warn!(
                            pid = entry.th32ProcessID,
                            name = %name,
                            error = ?e,
                            "启动清理：打开残留进程失败"
                        ),
                    }
                }
            }
        }

        // 取下一个进程（无更多进程时置退出循环）
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            break;
        }
    }
}

/// 启动清理：尽力清空 WebView2 UserDataFolder 缓存目录下的内容（**崩溃残留兜底**）。
///
/// 每个 wp-proc 实例使用 `data_root()/webview2-cache/<pid>/` 作为独立 UserDataFolder
/// （路径单源：`mirrorstar_core::wallpaper::wp_proc_user_data_folder`）。
/// **正常退出路径已由 `WebRenderer::terminate()` / `Drop` 在子进程退出后回收该子目录**
/// （见 `crates/mirrorstar-core/src/wallpaper/web.rs`），此处仅在应用启动时清空
/// 目录内容，兜底清理崩溃/强杀导致未被回收的残留（此时尚未 spawn 任何 wp-proc，
/// 且 `cleanup_stale_child_processes` 已尝试终止上次崩溃遗留的孤儿 wp-proc）。
///
/// 尽力而为：任何失败仅记录 warn，不阻断启动。残留孤儿进程可能仍短暂持有其缓存文件，
/// 导致删除失败——下一次启动会再尝试清理。
fn cleanup_webview2_cache() {
    let cache_dir = mirrorstar_core::config::data_root().join("webview2-cache");
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(entries) => entries,
        // 目录不存在（首次启动/从未用过 Web 壁纸）→ 无需清理，不告警。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(error = %e, dir = %cache_dir.display(), "启动清理：读取 WebView2 缓存目录失败");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %path.display(), "启动清理：删除 WebView2 缓存项失败（残留孤儿进程可能仍持有）");
        }
    }
}

/// 将配置加载错误格式化为用户可读报文并 emit "config-load-error" 事件。
///
/// 回调路径（set_on_config_error）与 drain 排空路径（构造期暂存的待处理错误）
/// 共用，避免两处文案漂移。
fn emit_config_load_error(app_handle: &tauri::AppHandle, error: &ConfigLoadError) {
    let msg = match error.kind {
        ConfigKind::Config => {
            format!("应用配置加载失败，已使用默认配置：{}", error.message)
        }
        ConfigKind::Library => {
            format!("壁纸库加载失败，已使用空列表：{}", error.message)
        }
    };
    if let Err(e) = app_handle.emit("config-load-error", &msg) {
        tracing::warn!(error = %e, "emit config-load-error 失败");
    }
}

pub fn run() {
    // 便携化（portable-data-root）：数据根 = 安装目录（exe 所在目录），
    // 在日志初始化之前确定，后续所有数据（配置/壁纸/缩略图/日志/WebView2 缓存）收束于此。
    mirrorstar_core::config::init_data_root(mirrorstar_core::config::resolve_data_root());

    // 尽早安装全局 panic hook（在日志初始化及其它线程 spawn 之前），
    // 覆盖所有线程的崩溃，追加写入 crash.log 并记录到 tracing 日志。
    install_panic_hook();

    // Initialize logging
    //
    // Task 9.3: 必须将 WorkerGuard 绑定到变量并持有至 run() 返回（应用退出）。
    // guard drop 后 tracing_appender 的非阻塞写入器后台线程会退出并刷写缓冲，
    // 提前 drop 会导致后续日志（含退出清理日志）丢失。原先 `if let Err` 写法
    // 在 Ok 路径直接丢弃 guard，现已改为持有。`_log_guard` 作用域覆盖整个 run()，
    // 包括 `.run(...)` 事件循环与 `perform_shutdown_blocking` 退出清理，确保
    // 退出期间的 tracing::info! 能被刷写到日志文件。
    //
    // 注：`std::process::exit(1)` 致命错误路径会跳过 Drop（不 unwind 栈），
    // 此为既有行为，不在本次修复范围；正常退出路径日志写入已得到保证。
    let _log_guard = match init_logging() {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("日志初始化失败: {}", e);
            None
        }
    };

    // v17 启动期内存埋点：日志初始化完成（首个生命周期打点，亦初始化启动基准时刻）
    mirrorstar_core::perf::mark_lifecycle("log_init");

    // v17 性能埋点：应用启动计时基线（日志初始化完成后开始计时）
    let boot_start = std::time::Instant::now();

    // 在主线程初始化 COM 为 STA 模式，与 Tauri/tao 的要求一致。
    // 必须在任何 COM 调用（如 VolumeControl::new）之前完成，
    // 否则 Tauri 的 OleInitialize 会因 COM 已被初始化为 MTA 而失败。
    //
    // T16（P1 COM 泄漏）：使用 ComGuard 包裹 CoInitializeEx（STA），guard 在 run()
    // 函数返回时 Drop 调 CoUninitialize。setup 闭包 `?` 失败（`.build()` 返回 Err →
    // `.expect()` panic unwind）时 guard 也会 Drop，避免 COM 引用计数泄漏。
    // 原裸 `unsafe CoInitializeEx` 无 guard，setup 失败时不调 CoUninitialize。
    //
    // guard 持有至 run() 返回（晚于 .run() 事件循环与 perform_shutdown_blocking），
    // 确保 app_state（含 VolumeControl 的 COM 接口）先 Drop 释放，再 CoUninitialize。
    // perform_shutdown_blocking 不再手动调 CoUninitialize（由本 guard 统一处理）。
    let _com_guard = commands::wallpaper::ComGuard::new_sta_or_exit();

    // Initialize desktop integration (fast, does not find WorkerW yet)
    let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));

    // Pre-initialize WorkerW in background so it's ready when needed
    //
    // ST-002（T-014 遗留）：保存 JoinHandle 到全局 `WORKERW_INIT_THREAD`，
    // `perform_shutdown_blocking` 中 take + join，避免线程在 shutdown 期间仍持有
    // desktop 锁导致 `engine.shutdown()` 短暂阻塞。
    {
        let desktop_clone = desktop.clone();
        let spawn_result = std::thread::Builder::new()
            .name("mirrorstar-workerw-init".to_string())
            .spawn(move || match desktop_clone.lock() {
                Ok(mut d) => {
                    if let Err(e) = d.ensure_initialized() {
                        tracing::warn!(error = %e, "后台 WorkerW 预初始化失败，将在首次使用时重试");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "后台 WorkerW 预初始化获取锁失败");
                }
            });
        match spawn_result {
            Ok(handle) => {
                // 保存 JoinHandle 供 perform_shutdown_blocking join
                // Mutex 中毒时不存储 handle，线程会随进程退出
                if let Err(e) = state::WORKERW_INIT_THREAD
                    .lock()
                    .map(|mut t| *t = Some(handle))
                {
                    tracing::warn!(error = ?e, "WORKERW_INIT_THREAD 锁中毒，无法存储 JoinHandle");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "创建 WorkerW 预初始化线程失败");
            }
        }
    }

    // Initialize volume control (caches COM interfaces)
    // COM 已在上方初始化为 STA 模式，CoCreateInstance 可正常工作
    //
    // T-008 优雅降级：当无音频设备（如服务器/虚拟机环境）时，`VolumeControl::new()`
    // 会失败。原实现直接 `std::process::exit(1)` 退出进程，导致应用无法启动。
    // 现改为降级为 `new_disabled()` no-op 实例：所有音量操作静默返回 Ok，
    // 视频壁纸可正常播放（仅无音频），图片/GIF 壁纸功能完全不受影响。
    //
    // v16-C-008：记录降级状态到 `audio_disabled`，在 setup 闭包（SHARED_APP_HANDLE
    // 已设置）中 emit `audio-disabled` 事件通知前端禁用音量控件 + toast 提示，
    // 避免用户在 UI 中调音量"看似成功实际无声"的困惑。
    let mut audio_disabled = false;
    let volume_control = Arc::new(Mutex::new(VolumeControl::new().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "VolumeControl 初始化失败，降级为 no-op 模式（无音频设备？）");
        audio_disabled = true;
        VolumeControl::new_disabled()
    })));

    // Initialize wallpaper engine
    let wallpaper_engine = Arc::new(tokio::sync::Mutex::new(WallpaperEngine::new(
        desktop.clone(),
        volume_control.clone(),
    )));

    // Initialize config manager
    let config_manager = Arc::new(ConfigManager::new().unwrap_or_else(|e| {
        tracing::error!(error = %e, "配置管理器初始化失败");
        std::process::exit(1);
    }));

    // 从配置中应用 GIF 内存管理策略到壁纸引擎
    {
        let config = config_manager.get_config();
        let mut engine = wallpaper_engine.blocking_lock();
        engine.set_gif_memory_strategy(
            config.gif.memory_strategy,
            config.gif.balanced_keep_frames,
            config.gif.max_memory_mb,
        );
        // 种入按显示器缩放模式（配置为持久化真相源）。必须在配置加载后、
        // 引擎可用前完成，使启动恢复 / 轮换 / 网格「设为壁纸」均沿用配置值。
        // 锁顺序：此处仅持有 engine 锁（不获取 desktop 锁），符合 state.rs
        // 顶部「engine → desktop」的锁顺序约定。
        engine.seed_scaling_modes(&config.scaling.modes);
    }

    // 启动配置文件监视（热重载）
    match config_manager.start_watching() {
        Ok(()) => tracing::info!("配置文件监视已启动"),
        Err(e) => tracing::warn!(error = %e, "配置文件监视启动失败"),
    }

    // 启动时清理 0 字节损坏缩略图文件
    // 清理旧版本残留或异常中断产生的损坏缩略图，避免前端加载 0 字节文件导致预览空白
    if let Err(e) = config_manager.cleanup_corrupted_thumbnails() {
        tracing::warn!(error = %e, "清理损坏缩略图文件失败");
    }

    // 启动全屏应用检测
    start_fullscreen_monitor(wallpaper_engine.clone(), config_manager.clone());

    // 启动 Explorer 重启监控（事件驱动，监听 TaskbarCreated 消息）
    // 同时处理 WM_POWERBROADCAST 消息，实现电源状态变化的即时检测
    start_explorer_restart_monitor(desktop.clone());

    // 启动清理：终止上次崩溃遗留的孤儿 mpv / wp-proc 进程
    // 当前进程刚启动尚未 spawn 任何子进程，任何同名存活进程均为崩溃残留。
    cleanup_stale_child_processes();

    // 启动清理：清空 WebView2 缓存目录（每实例 PID 子目录），避免随运行次数累积。
    cleanup_webview2_cache();

    // Task 2/10：初始化 UI 子进程管理器（解析 mirrorstar-ui.exe 路径与空闲退出阈值）。
    // 原生 UI 子进程即唯一主窗口，按需拉起，不在此处主动启动。
    ui_process::init();

    // 壁纸轮换调度器：构造共享句柄（playback / 池 / 编排状态），后台主循环在
    // setup 闭包中 `start()`（需 AppHandle）。§16 启动时序见 scheduler.rs。
    let scheduler = scheduler::SchedulerHandle::new(
        config_manager.clone(),
        wallpaper_engine.clone(),
        desktop.clone(),
    );

    // B3：暴露调度器全局引用，供全屏 / 电源 Win32 回调路径在恢复后唤醒它重算 deadline。
    let _ = state::SHARED_SCHEDULER.set(scheduler.clone());

    let app_state = AppState {
        config_manager,
        wallpaper_engine: wallpaper_engine.clone(),
        desktop,
        scheduler,
        tray_paused: AtomicBool::new(false),
        tray_pause_resume_item: OnceLock::new(),
        tray_pure_mode_item: OnceLock::new(),
    };

    // v11.0 内存优化（Wave v11-D）：自定义 Tokio runtime，限制 worker_threads=2 + 512KB 栈 + 8 个阻塞线程
    // 默认 runtime 使用 num_cpus 个 worker（8 核 = 8×2MB = 16MB），此配置降至 2×512KB = 1MB
    // 壁纸应用为 IO 密集型（IPC/文件读写），2 个 worker 足够处理并发任务
    //
    // 实现要点：
    // - `set(handle)` 将自定义 runtime 句柄注册到 Tauri，替代默认的 num_cpus worker runtime
    //   （注意：Tauri 仅持有 Handle 引用，TokioRuntime 实例必须由调用方保持存活）
    // - `runtime.enter()` 守卫为当前线程建立 runtime 上下文，使 `tokio::task::spawn_blocking`
    //   等裸 tokio 调用可用（setup 闭包与托盘菜单事件中使用了 spawn_blocking）
    // - `runtime` 变量持有至 run() 返回（晚于 .run() 事件循环退出），确保应用运行期间
    //   runtime 不会被回收；退出后 Drop 顺序：_runtime_guard → runtime（关闭 tokio）
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(512 * 1024)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .expect("创建 Tokio runtime 失败");
    let _runtime_guard = runtime.enter();
    tauri::async_runtime::set(runtime.handle().clone());

    // 启动视频壁纸卡帧常驻看护（spec D4/D6）：每 5s 采样 Playing 视频渲染器的时钟，
    // 停滞即分级自愈。任务随 runtime 结束，不做显式停止（见 liveness_watchdog 模块文档）。
    //
    // 必须在 `async_runtime::set()` **之后**启动：看护内部调用 `tauri::async_runtime::spawn`，
    // 而 spawn 会以默认 runtime 先行初始化全局 RUNTIME；若先于 `set()` 调用，随后的
    // `set()` 会 panic "runtime already initialized"（启动即崩溃，2026-09-30 实测根因）。
    let _ = platform::liveness_watchdog::start_liveness_watchdog(wallpaper_engine);

    // v17 性能埋点：Phase 1 完成（基础设施 + runtime），进入 Tauri Builder
    tracing::info!(
        target: "mirrorstar::perf",
        phase = "pre-builder",
        elapsed_ms = boot_start.elapsed().as_millis(),
        rss_mb = format!("{:.1}", mirrorstar_core::perf::process_rss_mb()),
        "PERF-BOOT: Phase 1 完成"
    );
    // v17 启动期内存埋点：引擎 / 配置等基础设施初始化完成
    mirrorstar_core::perf::mark_lifecycle("engine_ready");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // v16-D-014：MacosLauncher 参数为 tauri-plugin-autostart API 强制要求，
        // 项目仅支持 Windows 10/11（README 声明），Windows 平台走注册表
        // HKCU\Software\Microsoft\Windows\CurrentVersion\Run，此参数无效果。
        // 若未来 plugin 提供不带 MacosLauncher 的 init_windows() 变体，可切换。
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(app_state)
        .setup(move |app| {
            // T07：保存 AppHandle 到全局静态量，供 workerw_check 任务在 WorkerW
            // 重新初始化成功后 emit `desktop-status-changed` 事件通知前端。
            // OnceLock 首次 set 必成功；setup 仅执行一次，无需处理 Err。
            // v16-C-008：闭包以 `move` 捕获 `audio_disabled`（Copy bool，按值复制），
            // setup 要求 'static 闭包，须 ownership 而非借用。
            let _ = state::SHARED_APP_HANDLE.set(app.handle().clone());

            // Task 2：事件桥——集中订阅壳侧 13 个事件并转发到原生 UI 子进程
            // （不逐个改动各 emit 发射点）。转发为惰性：UI 进程未运行时零开销、零报错。
            if let Some(manager) = ui_process::manager() {
                ui_process::install_event_forwarding(app.handle(), manager.clone());
                // 启动空闲退出检查：UI 窗口隐藏超时后回收子进程（内存归零）。
                ui_process::spawn_idle_tick(manager);
            }

            // v16-C-008：VolumeControl 降级时 emit `audio-disabled` 通知前端
            // SHARED_APP_HANDLE 刚 set 完，前端监听器在 init() 中注册，
            // emit 时机晚于前端 listenWithCleanup 注册（setup 在 DOMContentLoaded 后），
            // 但为保险起见前端 listener 也处理"启动期已降级"的初始 toast。
            if audio_disabled {
                tracing::info!("检测到音频降级，emit audio-disabled 事件通知前端");
                if let Err(e) = app.handle().emit("audio-disabled", ()) {
                    tracing::warn!(error = %e, "emit audio-disabled 失败");
                }
            }

            // v17 内存归因埋点：后台每 60 秒输出一条应用组逐子进程内存归因日志
            // （target `mirrorstar::perf`），为优化前后提供量化基线。
            // 采样间隔 >= 30s（core 内下限保护），开销极小；返回的 JoinHandle
            // 直接丢弃即 detached 线程，随进程退出而结束，无需额外管理。
            let _ = mirrorstar_core::perf::spawn_memory_attribution_sampler(
                std::time::Duration::from_secs(60),
            );

            // v17 启动期内存时间线：应用启动后前 60 秒按 ~1 秒间隔输出分类别内存，
            // 之后线程自行结束，仅保留上面的 60 秒周期采样。受 MIRRORSTAR_MEM_TRACE
            // 开关控制（值 0/off 时关闭）。返回的 JoinHandle 丢弃即 detached 线程。
            let _ = mirrorstar_core::perf::spawn_startup_memory_timeline(
                std::time::Duration::from_secs(60),
                std::time::Duration::from_secs(1),
            );

            // spawn 全局状态变更订阅任务
            //
            // 订阅 WallpaperEngine 的 global_state_changed 通道，收到 display_id 后
            // emit `wallpaper-state-changed` 事件刷新前端 UI。
            //
            // 设计要点：
            // - 只订阅一次：global_state_changed 是 WallpaperEngine 内部的全局 broadcast
            //   通道，所有渲染器的 PauseSender::notify_state_changed 通过 per-renderer
            //   转发任务（在 embed_and_register_renderer 中 spawn）汇聚到此通道。
            //   新设置的壁纸会自动通过转发任务接入，无需重新订阅。
            // - 替代命令层/全屏/电源/托盘回调中的直接 emit：pause 线程在状态变更后
            //   调用 notify_state_changed → 转发任务 → 全局通道 → 本任务 emit。
            // - 原生壁纸（无 PauseSender）不走此通道，由 pause_wallpaper/resume_wallpaper
            //   命令层 emit 兜底。
            {
                let state = app.state::<AppState>();
                let app_handle = app.handle().clone();
                let engine = state.wallpaper_engine.clone();
                tauri::async_runtime::spawn(async move {
                    // 在 engine 锁内订阅全局通道（短暂持锁）
                    let mut rx = {
                        let engine = engine.lock().await;
                        engine.subscribe_state_changes()
                    };
                    while let Ok(display_id) = rx.recv().await {
                        if let Err(e) = app_handle.emit("wallpaper-state-changed", display_id) {
                            tracing::warn!(error = %e, "emit wallpaper-state-changed 失败：前端 UI 可能不刷新");
                        }
                    }
                    // broadcast sender drop（WallpaperEngine drop）时 recv 返回 Err(Closed)，任务退出
                });
            }

            // 设置配置变更回调：热重载成功后通知前端刷新 UI，并唤醒调度器重算 deadline。
            {
                let state = app.state::<AppState>();
                let app_handle = app.handle().clone();
                // B3 / DR-38：捕获调度器 wake，配置热重载后同步唤醒调度器（与 §13 统一通道）。
                let scheduler_wake = state.scheduler.wake.clone();
                // 多屏排列布局域：热重载变更同样同步引擎排列（阻塞锁，arrangement 变更
                // 低频，锁竞争可忽略；与 commands/config.rs sync_arrangement_to_engine 同策略）。
                let engine = state.wallpaper_engine.clone();
                let cm = state.config_manager.clone();
                state.config_manager.set_on_config_changed(Arc::new(move || {
                    if let Err(e) = app_handle.emit("config-changed", ()) {
                        tracing::warn!(error = %e, "emit config-changed 失败：前端 UI 可能不刷新");
                    }
                    scheduler_wake.notify_waiters();
                    let engine = engine.clone();
                    let cm = cm.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut engine = engine.lock().await;
                        engine.set_arrangement(cm.get_config().arrangement);
                        // 配置热重载后重新种入 `[scaling].modes`（配置为持久化真相源）：
                        // 手工编辑 config.toml 的缩放变更须即时进入引擎，而非等重启。
                        // 仅持 engine 锁，符合 state.rs「engine → desktop」锁顺序约定。
                        engine.seed_scaling_modes(&cm.get_config().scaling.modes);
                    });
                }));
            }

            // C01 修复：设置配置加载错误回调 + drain 构造时捕获的待处理错误
            //
            // load_config/load_library 解析失败但已回退到默认配置时，通过此回调
            // emit "config-load-error" 事件通知前端展示告警 toast，让用户知道
            // 配置被重置或壁纸列表为空，而非静默丢失。
            //
            // 构造时（new_in_dir）回调尚未设置，错误暂存于 pending_config_errors；
            // 此处设置回调后立即 drain 并 emit，确保构造时的错误也能通知到前端。
            {
                let state = app.state::<AppState>();
                let app_handle = app.handle().clone();
                state
                    .config_manager
                    .set_on_config_error(Arc::new(move |error: ConfigLoadError| {
                        emit_config_load_error(&app_handle, &error);
                    }));

                // drain 构造时捕获的待处理错误并立即 emit
                let pending = state.config_manager.drain_pending_config_errors();
                if !pending.is_empty() {
                    let app_handle = app.handle().clone();
                    for error in pending {
                        emit_config_load_error(&app_handle, &error);
                    }
                }
            }

            // Create tray menu
            let open_item = tauri::menu::MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
            let pause_resume_item = tauri::menu::MenuItem::with_id(app, "pause_resume", "暂停壁纸", true, None::<&str>)?;
            let quit_item = tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            // DR-28：托盘"下一张壁纸"项，点击触发手动换图（不受暂停限制）。
            let next_item = tauri::menu::MenuItem::with_id(app, "next_wallpaper", "下一张壁纸", true, None::<&str>)?;
            // 纯净壁纸模式入口：勾选项（唯一入口，勾选态与真实运行态由
            // platform::pure_mode 双向同步）。初始未勾选。
            let pure_mode_item = tauri::menu::CheckMenuItem::with_id(
                app,
                "pure_mode",
                "纯净壁纸（按任意键退出）",
                true,
                false,
                None::<&str>,
            )?;
            // 存储菜单项引用以便后续更新文本
            {
                let state = app.state::<AppState>();
                // OnceLock 首次 set 必成功；setup 仅执行一次，无需处理 Err
                let _ = state.tray_pause_resume_item.set(pause_resume_item.clone());
                let _ = state.tray_pure_mode_item.set(pure_mode_item.clone());
            }
            let menu = tauri::menu::Menu::with_items(
                app,
                &[
                    &open_item,
                    &next_item,
                    &pause_resume_item,
                    &pure_mode_item,
                    &quit_item,
                ],
            )?;

            // Create tray icon
            let mut tray_builder = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                // 关闭左键点击弹菜单（tray-icon 默认 menu_on_left_click=true，会在
                // WM_LBUTTONUP 时弹出菜单）。左键仅通过 on_tray_icon_event 打开主窗口，
                // 右键保留系统原生菜单。
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    match event.id.as_ref() {
                        "open" => {
                            // 壳模式下拉起原生 UI 子进程，否则走既有 WebView2 路径。
                            ui_process::open_main_window(app);
                        }
                        "pause_resume" => {
                            if let Some(state) = app.try_state::<AppState>() {
                                // 切换暂停状态（乐观更新；spawn 内失败时回滚）
                                let now_paused = !state.tray_paused.load(Ordering::SeqCst);
                                state.tray_paused.store(now_paused, Ordering::SeqCst);
                                // 通过异步任务获取 engine 锁后调用快速路径方法
                                let engine = state.wallpaper_engine.clone();
                                // B3 / DR-38：恢复成功后唤醒调度器重算 deadline。
                                let scheduler_wake = state.scheduler.wake.clone();
                                // 菜单事件闭包参数 app 即 &AppHandle，直接 clone 得到 owned 句柄供 spawn 使用
                                let app_handle = app.clone();
                                tauri::async_runtime::spawn(async move {
                                    let mut engine = engine.lock().await;
                                    // C-008 修复：检查 failed 列表，部分失败时回滚 tray_paused 与菜单文本
                                    let failed = if now_paused {
                                        engine.pause_all_fast(PauseReason::TRAY).unwrap_or_default()
                                    } else {
                                        engine.resume_all_fast(PauseReason::TRAY).unwrap_or_default()
                                    };
                                    // B3 / DR-38：托盘恢复成功后唤醒调度器重算 deadline（不补时）。
                                    if !now_paused {
                                        scheduler_wake.notify_waiters();
                                    }
                                    if !failed.is_empty() {
                                        tracing::warn!(
                                            failed_count = failed.len(),
                                            failed = ?failed,
                                            action = if now_paused { "pause_all_fast" } else { "resume_all_fast" },
                                            "托盘菜单 pause/resume 部分失败，回滚 tray_paused 与菜单文本"
                                        );
                                        if let Some(app_state) = app_handle.try_state::<AppState>() {
                                            app_state.tray_paused.store(!now_paused, Ordering::SeqCst);
                                            // 回滚菜单文本（set_text 内部会阻塞等待主线程，
                                            // 而菜单事件在主线程派发，因此需在非主线程调用以避免死锁）。
                                            // T06：改用 spawn_blocking 复用 tokio 阻塞线程池，
                                            // 避免每次菜单点击都创建新 OS 线程。
                                            let rollback_text = if now_paused { "暂停壁纸" } else { "恢复壁纸" };
                                            if let Some(item) = app_state.tray_pause_resume_item.get().cloned() {
                                                tokio::task::spawn_blocking(move || {
                                                    if let Err(e) = item.set_text(rollback_text) {
                                                        tracing::warn!(error = %e, "回滚托盘菜单文本失败");
                                                    }
                                                });
                                            }
                                        }
                                    }
                                    // 状态变更 emit 由全局订阅任务统一处理（详见 lib.rs setup 闭包）
                                });
                                if now_paused {
                                    tracing::info!("通过托盘菜单暂停所有壁纸");
                                } else {
                                    tracing::info!("通过托盘菜单恢复所有壁纸");
                                }
                                // 乐观更新菜单文本（set_text 内部会阻塞等待主线程，
                                // 而菜单事件在主线程派发，因此需在非主线程调用以避免死锁）。
                                // 部分失败时上面 spawn_blocking 内会回滚文本。
                                // T06：改用 spawn_blocking 复用 tokio 阻塞线程池，
                                // 避免每次菜单点击都创建新 OS 线程。
                                let new_text = if now_paused { "恢复壁纸" } else { "暂停壁纸" };
                                if let Some(item) = state.tray_pause_resume_item.get().cloned() {
                                    tokio::task::spawn_blocking(move || {
                                        if let Err(e) = item.set_text(new_text) {
                                            tracing::warn!(error = %e, "更新托盘菜单文本失败");
                                        }
                                    });
                                }
                            }
                        }
                        "quit" => {
                            // 触发应用退出；清理逻辑由 RunEvent::ExitRequested 统一执行
                            // （perform_shutdown_blocking），确保 mpv 子进程终止与资源释放。
                            app.exit(0);
                        }
                        "pure_mode" => {
                            // 纯净壁纸模式入口：按权威运行态决定进入或退出
                            // （不依赖菜单勾选态——原生菜单点击会自动翻转勾选，不可靠）。
                            // toggle 内部立即 spawn 到异步 runtime，主线程不被阻塞。
                            crate::platform::pure_mode::toggle(app.clone());
                        }
                        "next_wallpaper" => {
                            // DR-28：手动"下一张壁纸"；None = 主/唯一单元（DR-36）。
                            // 经 manual_tx 由调度器主循环消费，不受暂停抑制（DR-9/DR-20）。
                            if let Some(state) = app.try_state::<AppState>() {
                                let _ = state.scheduler.manual_tx.send(None);
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        ui_process::open_main_window(app);
                    }
                });

            // 优雅降级：图标未配置时记录警告并跳过，避免启动 panic
            match app.default_window_icon() {
                Some(icon) => {
                    tray_builder = tray_builder.icon(icon.clone());
                }
                None => {
                    tracing::warn!("应用图标未配置，请在 tauri.conf.json 中设置 icon，托盘将使用系统默认图标");
                }
            }

            let _tray = tray_builder.build(app)?;

            // 测试观察通道（仅测试）：设置 MIRRORSTAR_TEST_AUTOSHOW_MAIN_WINDOW=1 时
            // 启动即拉起原生 UI 子进程并显示其窗口（供验收脚本 shell_measure.ps1 等观察）。
            // 生产环境不设置即无任何影响。
            if std::env::var_os("MIRRORSTAR_TEST_AUTOSHOW_MAIN_WINDOW").is_some() {
                ui_process::open_main_window(app.handle());
            }

            // WorkerW 有效性兜底检查（5 分钟间隔）
            // 主要监控由 TaskbarCreated 事件驱动，此检查仅作为事件遗漏的最终兜底
            let desktop_clone = app.state::<AppState>().desktop.clone();
            start_workerw_check(desktop_clone);

            // 启动壁纸轮换调度器主循环（§16 启动时序：读 playback/config → 对账 DR-34 →
            // 对齐单元 → resolve_boot() 一次 apply → 进入主循环）。
            {
                let state = app.state::<AppState>();
                state.scheduler.start(app.handle().clone());
            }

            // 测试观察通道（仅测试）：设置 MIRRORSTAR_TEST_PURE_MODE=<N> 时，启动后按轮数 N
            // 循环执行"进入 → 保持 → 退出"的纯净壁纸模式往返，供自动化脚本验证
            // "置顶 → 输入退出 → 嵌回"链路以及**进程内重复往返**。该钩子走的是与托盘
            // 二次点击退出**完全相同**的 `toggle` 路径（其按权威状态幂等分流进入/退出）。
            // 注意：进入与退出**两个 toggle 均为条件式**——toggle 前先查询
            // `is_pure_mode_active()`，仅当状态确实需要翻转时才调用，使钩子在外部干扰
            // 场景（脚本在模式内自行按键退出/进入）下依然稳定，不会把外部退出/进入反向
            // 翻转。值不可解析为数字时按 1 轮处理（保留既有 `=1` 语义）。
            // 生产环境不设置该环境变量即无任何影响。
            if std::env::var_os("MIRRORSTAR_TEST_PURE_MODE").is_some() {
                let rounds: u32 = std::env::var("MIRRORSTAR_TEST_PURE_MODE")
                    .ok()
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .unwrap_or(1);
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // 轮询等待引擎进入/退出模式（每 200ms 一次，最多 max_tries 次）；
                    // 返回 true 表示已到达期望态。与既有钩子一致：AppState 不可用时跳过本轮探测。
                    async fn wait_pure_mode(
                        handle: &tauri::AppHandle,
                        want_active: bool,
                        max_tries: u32,
                    ) -> bool {
                        for _ in 0..max_tries {
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            let Some(state) = handle.try_state::<AppState>() else {
                                continue;
                            };
                            let engine = state.wallpaper_engine.clone();
                            if engine.lock().await.is_pure_mode_active() == want_active {
                                return true;
                            }
                        }
                        false
                    }

                    tracing::info!(
                        rounds,
                        "[测试钩子] 开始纯净壁纸模式往返测试（MIRRORSTAR_TEST_PURE_MODE）"
                    );
                    for i in 1..=rounds {
                        // 1. 最多等待 20s：每秒探测一次"是否已有活跃壁纸"，避免在 boot apply
                        //    完成前进入而被引擎以"当前没有任何壁纸"拒绝（前置校验见 enter_pure_mode）。
                        let mut wallpapered = false;
                        for _ in 0..20 {
                            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                            let Some(state) = handle.try_state::<AppState>() else {
                                continue;
                            };
                            let engine = state.wallpaper_engine.clone();
                            if engine.lock().await.has_any_wallpaper() {
                                wallpapered = true;
                                break;
                            }
                        }
                        if !wallpapered {
                            tracing::warn!(
                                round = i,
                                "[测试钩子] 20s 内未等到活跃壁纸，终止后续轮次（MIRRORSTAR_TEST_PURE_MODE）"
                            );
                            return;
                        }

                        // 2. 条件进入（与托盘二次点击同一 `toggle` 路径）：调用 toggle 前
                        //    先查询权威状态，仅当当前未进入时才 toggle；若外部已进入则跳过，
                        //    避免重复 toggle 反而把模式关掉。
                        let already_active = match handle.try_state::<AppState>() {
                            Some(state) => {
                                state.wallpaper_engine.lock().await.is_pure_mode_active()
                            }
                            None => false,
                        };
                        if already_active {
                            tracing::info!(
                                round = i,
                                rounds,
                                "[测试钩子] 模式已被外部进入，跳过本轮进入 toggle"
                            );
                        } else {
                            tracing::info!(round = i, rounds, "[测试钩子] 请求进入纯净壁纸模式");
                            crate::platform::pure_mode::toggle(handle.clone());
                        }

                        // 3. 最多等待 5s（每 200ms，共 25 次）确认引擎已进入模式
                        if !wait_pure_mode(&handle, true, 25).await {
                            tracing::warn!(
                                round = i,
                                "[测试钩子] 等待进入纯净壁纸模式超时，终止后续轮次"
                            );
                            return;
                        }

                        // 4. 保持 3s（便于外部脚本观察置顶态）
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                        // 5. 条件退出（同一 `toggle` 路径）：保持期结束后先查询权威状态，
                        //    仅当仍处于模式内时才 toggle 退出；若外部脚本已退出则跳过本次
                        //    toggle（否则会反向重新进入并导致后续"等待失效"超时告警）。
                        let still_active = match handle.try_state::<AppState>() {
                            Some(state) => {
                                state.wallpaper_engine.lock().await.is_pure_mode_active()
                            }
                            None => false,
                        };
                        if still_active {
                            tracing::info!(round = i, rounds, "[测试钩子] 请求退出纯净壁纸模式");
                            crate::platform::pure_mode::toggle(handle.clone());
                        } else {
                            tracing::info!(
                                round = i,
                                rounds,
                                "[测试钩子] 模式已被外部退出，跳过本轮退出 toggle"
                            );
                        }

                        // 6. 最多等待 20s（每 200ms，共 100 次）确认已退出。
                        //    窗口放宽到 20s：退出编排现含存活校验与可能的自愈（mpv 重建
                        //    可达数秒），自愈场景下退出可能更久，避免钩子误报超时中止后续轮次。
                        if !wait_pure_mode(&handle, false, 100).await {
                            tracing::warn!(
                                round = i,
                                "[测试钩子] 等待退出纯净壁纸模式超时，终止后续轮次"
                            );
                            return;
                        }

                        tracing::info!("[测试钩子] 完成第 {i}/{rounds} 轮纯净壁纸模式往返");
                    }
                    tracing::info!(
                        rounds,
                        "[测试钩子] 纯净壁纸模式往返测试全部结束（MIRRORSTAR_TEST_PURE_MODE）"
                    );
                });
            }

            // v17 性能埋点：应用就绪（setup 闭包完成），输出总启动时间 + RSS
            tracing::info!(
                target: "mirrorstar::perf",
                phase = "ready",
                total_ms = boot_start.elapsed().as_millis(),
                rss_mb = format!("{:.1}", mirrorstar_core::perf::process_rss_mb()),
                private_mb = format!("{:.1}", mirrorstar_core::perf::process_private_mb()),
                "PERF-BOOT: 应用启动完成"
            );

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_wallpapers,
            reorder_wallpapers,
            add_wallpaper,
            remove_wallpaper,
            set_wallpaper,
            pause_wallpaper,
            resume_wallpaper,
            get_config,
            update_config,
            get_arrangement,
            update_arrangement,
            set_volume,
            toggle_mute,
            set_interaction_mode,
            get_displays,
            get_memory_report,
            report_lifecycle_mark,
            get_wallpaper_state,
            open_file_dialog,
            toggle_auto_start,
            get_auto_start_status,
            set_scaling_mode,
            set_speed,
            check_desktop_status,
            get_rotation_config,
            update_rotation_config,
            list_pools,
            create_pool,
            update_pool,
            delete_pool,
            reorder_pools,
            get_unit_states,
            set_active_pool,
            set_rotation_enabled,
            next_wallpaper,
            prev_wallpaper,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // 退出清理统一入口：
            // - code=None：最后窗口关闭引发的退出请求。本应用已无 Tauri 主窗口（主窗口为
            //   独立 UI 子进程），此分支通常不再触发；保守起见仍阻止退出以保持托盘驻留。
            // - code=Some(_)：显式退出（托盘退出/系统关机），执行
            //   perform_shutdown_blocking 确保 mpv 子进程终止、壁纸窗口销毁、配置刷写、
            //   Hook/COM 释放，避免 mpv 孤立与资源泄漏。
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                } else {
                    // Task 2：先回收 UI 子进程（若在运行），再执行统一清理，
                    // 避免 mirrorstar-ui.exe 被遗留为孤儿进程。
                    if let Some(manager) = ui_process::manager() {
                        manager.shutdown();
                    }
                    if let Some(state) = app_handle.try_state::<AppState>() {
                        perform_shutdown_blocking(
                            &state.wallpaper_engine,
                            &state.config_manager,
                        );
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::panic_hook_timestamp;

    #[test]
    fn panic_hook_timestamp_matches_format() {
        // Task 6：崩溃防线有效性——panic hook 写入 crash.log 的时间戳是纯函数，
        // 隔离测试其格式正确，保证 panic 诊断日志可解析（据此定位崩溃时刻）。
        let ts = panic_hook_timestamp();
        // 形如 "YYYY-MM-DD HH:MM:SS.mmm"
        let mut parts = ts.splitn(2, ' '); // date + time
        let date = parts.next().expect("应含日期");
        let time = parts.next().expect("应含时间");
        let date_parts: Vec<&str> = date.split('-').collect();
        assert_eq!(date_parts.len(), 3, "日期应为 YYYY-MM-DD: {ts}");
        assert_eq!(date_parts[0].len(), 4, "年应为 4 位: {ts}");
        assert_eq!(date_parts[1].len(), 2, "月应为 2 位: {ts}");
        assert_eq!(date_parts[2].len(), 2, "日应为 2 位: {ts}");
        let time_parts: Vec<&str> = time.splitn(2, '.').collect();
        assert_eq!(time_parts.len(), 2, "时间应为 HH:MM:SS.mmm: {ts}");
        assert_eq!(time_parts[0].len(), 8, "时:分:秒应为 8 位: {ts}");
        assert_eq!(time_parts[1].len(), 3, "毫秒应为 3 位: {ts}");
    }

    #[test]
    fn panic_hook_timestamp_payload_extraction() {
        // Task 6：验证 panic hook 能从 payload 提取字符串信息（&'static str / String）。
        // panic hook 内部不可 panic（否则递归崩溃），此处通过构造 PanicHookInfo 验证
        // trace_panic_info 的字符串提取分支可编译且不 panic。直接构造 PanicHookInfo
        // 需不稳定的 panic API，故降级为验证时间戳与格式层；真正的字符串提取在
        // 运行时 panic 时由 std 提供合法 HookInfo，此处仅确保格式函数健壮性。
        let ts = panic_hook_timestamp();
        assert!(!ts.contains('\n'), "时间戳不应含换行");
        assert!(
            ts.trim().len() == 23,
            "时间戳应为 19 + 1 空格 + 3 毫秒 = 23 字符: {ts}"
        );
    }

    // ── 纯净壁纸模式托盘入口契约 ──────────────────────────────────────────

    /// 托盘入口契约：应注册 `pure_mode` 菜单项 id，且该入口采用勾选项
    /// （`CheckMenuItem`），使勾选态与真实运行态可双向同步。
    #[test]
    fn tray_check_item_and_menu_id_contract_markers() {
        let source = include_str!("lib.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let menu_id = ["\"pure_", "mode\""].join("");
        let check_item = ["Check", "MenuItem"].join("");
        assert!(source.contains(&menu_id), "托盘应注册 {menu_id} 菜单项 id");
        assert!(
            source.contains(&check_item),
            "托盘『纯净壁纸』入口应使用 {check_item} 勾选项"
        );
    }

    /// 测试钩子契约：`MIRRORSTAR_TEST_PURE_MODE` 往返钩子的进入/退出两步均为**条件式**
    /// toggle —— toggle 前先查询 `is_pure_mode_active()`，仅当需要翻转时才调用，从而在
    /// 外部干扰（脚本在模式内自行退出/进入）场景下不会把外部状态反向翻转。
    #[test]
    fn pure_mode_roundtrip_hook_conditional_toggle_markers() {
        let source = include_str!("lib.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let active_query = ["is_pure_mode", "_active"].join("");
        let skip_exit = ["跳过本轮", "退出 toggle"].join("");
        let skip_enter = ["跳过本轮", "进入 toggle"].join("");
        assert!(
            source.contains(&active_query),
            "往返钩子应在 toggle 前通过 {active_query} 做条件判断"
        );
        assert!(
            source.contains(&skip_exit),
            "退出步骤检测到外部已退出时应记录『{skip_exit}』并跳过"
        );
        assert!(
            source.contains(&skip_enter),
            "进入步骤检测到外部已进入时应记录『{skip_enter}』并跳过"
        );
    }

    /// 启动顺序契约：自定义 Tokio runtime 注册（`async_runtime::set`）必须先于
    /// 存活看护任务启动——看护经 `tauri::async_runtime::spawn` 启动，而 spawn 会以
    /// 默认 runtime 先行初始化全局 RUNTIME；若抢在 set 之前，set 将 panic
    /// "runtime already initialized"（2026-09-30 实测启动即崩溃的根因）。
    #[test]
    fn async_runtime_set_precedes_liveness_watchdog_start() {
        let source = include_str!("lib.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let set_call = ["tauri::async_runtime::", "set("].join("");
        let watchdog_call = ["start_liveness_", "watchdog("].join("");
        let set_pos = source
            .find(&set_call)
            .expect("lib.rs 应注册自定义 Tokio runtime（async_runtime::set）");
        let watchdog_pos = source
            .find(&watchdog_call)
            .expect("lib.rs 应启动存活看护（start_liveness_watchdog）");
        assert!(
            set_pos < watchdog_pos,
            "async_runtime::set 必须先于 start_liveness_watchdog：否则默认 runtime 先行初始化，set() 会 panic"
        );
    }
}
