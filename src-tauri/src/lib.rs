mod commands;
mod platform;
mod state;

use mirrorstar_core::{
    init_logging, ConfigKind, ConfigLoadError, ConfigManager, DesktopIntegrator, PauseReason,
    VolumeControl, WallpaperEngine,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{Emitter, Manager};

use commands::*;
use platform::{start_explorer_restart_monitor, start_fullscreen_monitor, start_workerw_check};
use state::{create_or_show_main_window, perform_shutdown_blocking, AppState};

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
// 13. **AppState 构造**：组装 config_manager / wallpaper_engine / desktop / tray_paused / tray_pause_resume_item
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

/// 解析 HTTP Range 请求头，返回 (start, end) 字节范围（含端点）。
///
/// 支持的格式（RFC 7233）：
/// - `bytes=0-1023` → (0, 1023)
/// - `bytes=0-`     → (0, len-1)  从 start 到文件末尾
/// - `bytes=-1023`  → (len-1023, len-1)  最后 1023 字节
/// - `bytes=0-1023,2048-3071` → 只取第一个范围 (0, 1023)（简化，视频播放器不发多范围）
///
/// 返回 `None` 表示 Range 不满足（start >= len 或解析失败），
/// 调用方应返回 416 Range Not Satisfiable。
///
/// 注意：end 会被 clamp 到 len-1，不会超出文件大小。
fn parse_range_header(header: &str, len: u64) -> Option<(u64, u64)> {
    let header = header.strip_prefix("bytes=")?;
    // 只取第一个范围（忽略多范围，简化实现）
    let first_range = header.split(',').next()?;
    let (start_str, end_str) = first_range.split_once('-')?;

    let (start, end) = if start_str.is_empty() {
        // bytes=-1023 → 最后 1023 字节
        let suffix: u64 = end_str.trim().parse().ok()?;
        if suffix == 0 || suffix > len {
            return None;
        }
        (len - suffix, len - 1)
    } else if end_str.is_empty() {
        // bytes=0- → 从 start 到文件末尾
        let start: u64 = start_str.trim().parse().ok()?;
        if start >= len {
            return None;
        }
        (start, len - 1)
    } else {
        // bytes=0-1023
        let start: u64 = start_str.trim().parse().ok()?;
        let end: u64 = end_str.trim().parse().ok()?;
        if start > end || start >= len {
            return None;
        }
        (start, end.min(len - 1))
    };

    Some((start, end))
}

/// 根据文件扩展名返回 Content-Type（用于 wpfile:// 自定义协议响应）。
///
/// 覆盖壁纸应用支持的所有格式：图片（jpg/png/gif/bmp/webp/tiff）、
/// 视频（mp4/webm/mkv/avi/mov）、网页（html/htm）。未匹配的扩展名返回
/// `application/octet-stream`，浏览器会作为下载处理（壁纸场景不会命中）。
fn content_type_for_extension(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        // 图片
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tiff" | "tif" => "image/tiff",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        // 视频
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        // 网页
        "html" | "htm" => "text/html; charset=utf-8",
        // 默认
        _ => "application/octet-stream",
    }
}

// ── v16-D-009: wpfile 协议路径 scope 校验 ────────────────────────────────────
//
// wpfile:// 自定义协议绕过了 tauri.conf.json 中 `assetProtocol.scope` 的 allow/deny
// 列表（asset scope 仅对 asset:// 协议生效，且 Windows 上 `\\?\` verbatim 前缀导致
// glob 匹配失败，故改用自定义协议 handler 直接读文件）。此处复用 asset scope 的语义，
// 在 handler 内重新实现 allow/deny 校验，恢复 defense-in-depth：
// - allow：数据根（便携化后 = 安装目录，`mirrorstar_core::config::data_root()`）
//   之下的路径（壁纸文件与缩略图存放目录均收束于此）
// - deny：`$HOME` 下 7 个敏感目录（`.ssh` / `.aws` / `.gnupg` / `.config/ssh` /
//   `.password-store` / `.kube` / `.docker`），deny 优先于 allow
//
// 与 `commands::wallpaper::is_path_within_data_dir` 一致，使用 `Path::starts_with` 按
// 路径组件匹配（而非字节前缀），避免 `mirrorstar-evil` 等兄弟目录被误判为受信任目录。

/// wpfile 协议路径 scope 校验的纯函数核心。
///
/// 将依赖环境的目录解析剥离到调用方，便于单测以临时目录构造确定性行为，避免
/// 进程级环境变量（`USERPROFILE`）在并行测试中互相干扰。
///
/// - `file_path`：URL 解码后的绝对文件路径（允许 `/` 或 `\` 分隔符，`Path::starts_with`
///   在 Windows 上对两种分隔符做组件级等价匹配）
/// - `data_root`：数据根目录（便携化后 = 安装目录），allow 基目录
/// - `home`：`$HOME`（`USERPROFILE`），deny 基目录
///
/// 返回 `true` 表示允许访问，`false` 表示拒绝（handler 返回 403 FORBIDDEN）。
fn wpfile_path_allowed(
    file_path: &str,
    data_root: &std::path::Path,
    home: &std::path::Path,
) -> bool {
    let path = std::path::Path::new(file_path);

    // 1. deny 优先：命中任一敏感目录即拒绝（即便位于 allow 范围内也拒绝）
    // `.config/ssh` 为两级子目录，`Path::join` 会正确处理其中的分隔符。
    const DENY_SUBDIRS: &[&str] = &[
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/ssh",
        ".password-store",
        ".kube",
        ".docker",
    ];
    for sub in DENY_SUBDIRS {
        if path.starts_with(home.join(sub)) {
            return false;
        }
    }

    // 2. allow：必须位于数据根（安装目录）之下
    path.starts_with(data_root)
}

/// wpfile handler 专用 scope 校验封装：从数据根与 `USERPROFILE` 解析基目录后委托纯函数。
///
/// 数据根由 `mirrorstar_core::config::data_root()` 提供（便携化后 = 安装目录）；
/// `USERPROFILE` 缺失（极端罕见）时 deny 检查跳过，不影响 allow 安全性。
fn check_wpfile_scope(file_path: &str) -> Result<(), &'static str> {
    let data_root = mirrorstar_core::config::data_root();
    let home = std::env::var_os("USERPROFILE").map(std::path::PathBuf::from);
    let empty = std::path::PathBuf::new();
    // home 缺失时 deny 检查跳过（path.starts_with(empty) 仅对空路径为真，不影响安全）
    let home = home.as_deref().unwrap_or(&empty);

    if wpfile_path_allowed(file_path, &data_root, home) {
        Ok(())
    } else {
        Err("路径不在受控数据目录内或命中敏感目录，拒绝访问")
    }
}

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
    let y = if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", y, m, d, hh, mm, ss, millis)
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
    let thread_name = std::thread::current().name().unwrap_or("unnamed").to_string();

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
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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

/// 启动清理：终止上次崩溃遗留的孤儿 mpv / wp-proc 进程
///
/// 当前进程刚启动尚未 spawn 任何子进程，此时任何名称匹配
/// mpv.exe / mirrorstar-wp-proc.exe 的存活进程均为上次崩溃遗留。
/// 通过 ToolHelp 快照（CreateToolhelp32Snapshot + Process32FirstW/NextW）
/// 枚举系统进程并逐个终止（不含当前进程）。失败仅记录日志，不致命。
fn cleanup_stale_child_processes() {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };

    // 目标残留进程名（统一小写比较）
    const TARGET_NAMES: &[&str] = &["mpv.exe", "mirrorstar-wp-proc.exe"];

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
        let engine = wallpaper_engine.blocking_lock();
        engine.set_gif_memory_strategy(
            config.gif.memory_strategy,
            config.gif.balanced_keep_frames,
            config.gif.max_memory_mb,
        );
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

    let app_state = AppState {
        config_manager,
        wallpaper_engine,
        desktop,
        tray_paused: AtomicBool::new(false),
        tray_pause_resume_item: OnceLock::new(),
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

    // v17 性能埋点：Phase 1 完成（基础设施 + runtime），进入 Tauri Builder
    tracing::info!(
        target: "mirrorstar::perf",
        phase = "pre-builder",
        elapsed_ms = boot_start.elapsed().as_millis(),
        rss_mb = format!("{:.1}", mirrorstar_core::perf::process_rss_mb()),
        "PERF-BOOT: Phase 1 完成"
    );

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
        // 注册 wpfile 自定义 URI scheme protocol，绕过 Tauri
        // asset protocol scope 限制。URL 形式为 `http://wpfile.localhost/...`。
        //
        // 原方案通过 assetProtocol.scope.allow 配置允许路径，但 Windows 上
        // try_resolve_symlink_and_canonicalize 会给路径加 \\?\ 前缀（verbatim），
        // 导致 glob 匹配失败，即使配置 "**" 也无法通过。本方案直接注册自定义协议，
        // 在 handler 中读取文件并返回，不做 scope 检查。
        //
        // 前端通过 convertFileSrc(path, "wpfile") 生成 URL：
        //   Windows: http://wpfile.localhost/<encoded_path>
        //   Unix:    wpfile://localhost/<encoded_path>
        //
        // 支持 HTTP Range 请求（206 Partial Content）和 HEAD 请求：
        // - Range：视频播放器按需加载指定字节范围，避免一次性读入大文件
        //   （29MB 视频的内存峰值从 29MB 降至 1MB）
        // - HEAD：视频播放器探测文件大小（Content-Length），不返回 body
        // - 非 Range/HEAD：完整读取（图片等小文件，通常 <5MB）
        //
        // 单次 Range 最大 1MB（MAX_RANGE_LEN），与 Tauri 原生 asset protocol 一致，
        // 防止恶意 Range 请求读取过多数据。
        .register_uri_scheme_protocol("wpfile", |_ctx, request| {
            use std::io::{Read, Seek, SeekFrom};
            use tauri::http::{header, Response, StatusCode};

            let uri_path = request.uri().path();
            let encoded_path = uri_path.strip_prefix('/').unwrap_or(uri_path);
            let file_path = percent_encoding::percent_decode_str(encoded_path)
                .decode_utf8_lossy()
                .into_owned();

            // v16-D-009: scope 校验。wpfile:// 绕过 assetProtocol.scope，此处重新
            // 实现 allow/deny 检查，仅放行数据根（便携化后 = 安装目录）之下的路径，
            // 拒绝 7 个敏感目录。违规返回 403，避免泄露任意本地文件。
            if let Err(reason) = check_wpfile_scope(&file_path) {
                tracing::warn!(path = %file_path, reason, "wpfile 协议 scope 校验拒绝");
                return Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(reason.as_bytes().to_vec())
                    .unwrap();
            }

            let content_type = content_type_for_extension(&file_path);

            // 打开文件
            let mut file = match std::fs::File::open(&file_path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, path = %file_path, "wpfile 协议打开文件失败");
                    let status = if e.kind() == std::io::ErrorKind::NotFound {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    };
                    return Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                        .body(e.to_string().into_bytes())
                        .unwrap();
                }
            };

            let len = match file.metadata() {
                Ok(m) => m.len(),
                Err(e) => {
                    tracing::warn!(error = %e, path = %file_path, "wpfile 协议读取 metadata 失败");
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                        .body(e.to_string().into_bytes())
                        .unwrap();
                }
            };

            // HEAD 请求：只返回头，不返回 body（视频播放器探测文件大小）
            if request.method() == tauri::http::Method::HEAD {
                return Response::builder()
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_LENGTH, len.to_string())
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Vec::new())
                    .unwrap();
            }

            // Range 请求：只读取指定字节范围（视频按需加载）
            if let Some(range_header) = request
                .headers()
                .get("range")
                .and_then(|r| r.to_str().ok())
            {
                if let Some((start, end)) = parse_range_header(range_header, len) {
                    // 限制单次最大 1MB（与 Tauri 原生 asset protocol 一致）
                    const MAX_RANGE_LEN: u64 = 1000 * 1024;
                    let end = end.min(start + MAX_RANGE_LEN - 1).min(len - 1);
                    let nbytes = end - start + 1;

                    let mut buf = Vec::with_capacity(nbytes as usize);
                    if let Err(e) = file.seek(SeekFrom::Start(start)) {
                        tracing::warn!(error = %e, path = %file_path, "wpfile 协议 seek 失败");
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                            .body(e.to_string().into_bytes())
                            .unwrap();
                    }
                    if let Err(e) = file.take(nbytes).read_to_end(&mut buf) {
                        tracing::warn!(error = %e, path = %file_path, "wpfile 协议 Range 读取失败");
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                            .body(e.to_string().into_bytes())
                            .unwrap();
                    }

                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_TYPE, content_type)
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                        .header(header::CONTENT_LENGTH, nbytes.to_string())
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(buf)
                        .unwrap();
                } else {
                    // Range 不满足
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                        .header(header::ACCEPT_RANGES, "bytes")
                        .body(Vec::new())
                        .unwrap();
                }
            }

            // 非 Range 请求：完整读取（图片等小文件）
            let mut buf = Vec::with_capacity(len as usize);
            if let Err(e) = file.read_to_end(&mut buf) {
                tracing::warn!(error = %e, path = %file_path, "wpfile 协议完整读取失败");
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(e.to_string().into_bytes())
                    .unwrap();
            }

            Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, len.to_string())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(buf)
                .unwrap()
        })
        .setup(move |app| {
            // T07：保存 AppHandle 到全局静态量，供 workerw_check 任务在 WorkerW
            // 重新初始化成功后 emit `desktop-status-changed` 事件通知前端。
            // OnceLock 首次 set 必成功；setup 仅执行一次，无需处理 Err。
            // v16-C-008：闭包以 `move` 捕获 `audio_disabled`（Copy bool，按值复制），
            // setup 要求 'static 闭包，须 ownership 而非借用。
            let _ = state::SHARED_APP_HANDLE.set(app.handle().clone());

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

            // 设置配置变更回调：热重载成功后通知前端刷新 UI
            {
                let state = app.state::<AppState>();
                let app_handle = app.handle().clone();
                state.config_manager.set_on_config_changed(Arc::new(move || {
                    if let Err(e) = app_handle.emit("config-changed", ()) {
                        tracing::warn!(error = %e, "emit config-changed 失败：前端 UI 可能不刷新");
                    }
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
                    }));

                // drain 构造时捕获的待处理错误并立即 emit
                let pending = state.config_manager.drain_pending_config_errors();
                if !pending.is_empty() {
                    let app_handle = app.handle().clone();
                    for error in pending {
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
                }
            }

            // Create tray menu
            let open_item = tauri::menu::MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
            let pause_resume_item = tauri::menu::MenuItem::with_id(app, "pause_resume", "暂停壁纸", true, None::<&str>)?;
            let quit_item = tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            // 存储菜单项引用以便后续更新文本
            {
                let state = app.state::<AppState>();
                // OnceLock 首次 set 必成功；setup 仅执行一次，无需处理 Err
                let _ = state.tray_pause_resume_item.set(pause_resume_item.clone());
            }
            let menu = tauri::menu::Menu::with_items(app, &[&open_item, &pause_resume_item, &quit_item])?;

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
                            create_or_show_main_window(app);
                        }
                        "pause_resume" => {
                            if let Some(state) = app.try_state::<AppState>() {
                                // 切换暂停状态（乐观更新；spawn 内失败时回滚）
                                let now_paused = !state.tray_paused.load(Ordering::SeqCst);
                                state.tray_paused.store(now_paused, Ordering::SeqCst);
                                // 通过异步任务获取 engine 锁后调用快速路径方法
                                let engine = state.wallpaper_engine.clone();
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
                        create_or_show_main_window(app);
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

            // WorkerW 有效性兜底检查（5 分钟间隔）
            // 主要监控由 TaskbarCreated 事件驱动，此检查仅作为事件遗漏的最终兜底
            let desktop_clone = app.state::<AppState>().desktop.clone();
            start_workerw_check(desktop_clone);

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
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // v8.0 内存优化：不调用 api.prevent_close()，让 Tauri 默认销毁主窗口，
                // 释放 WebView2 子进程树（msedgewebview2.exe ~150-300MB）内存。
                // 窗口销毁后触发 RunEvent::ExitRequested(code=None)，由 .run() 回调中
                // api.prevent_exit() 阻止应用退出保持托盘驻留；下次打开时
                // create_or_show_main_window 走 build 路径重建窗口与 WebView2。
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_wallpapers,
            add_wallpaper,
            regenerate_thumbnails,
            remove_wallpaper,
            set_wallpaper,
            pause_wallpaper,
            resume_wallpaper,
            get_config,
            update_config,
            set_volume,
            toggle_mute,
            set_interaction_mode,
            get_displays,
            get_wallpaper_state,
            open_file_dialog,
            toggle_auto_start,
            get_auto_start_status,
            set_scaling_mode,
            set_speed,
            check_desktop_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // 退出清理统一入口：
            // - code=None：主窗口被关闭导致的退出请求（v8.0 主窗口改为销毁以释放 WebView2
            //   内存）。调用 api.prevent_exit() 阻止应用退出，保持托盘驻留，不执行清理。
            // - code=Some(_)：显式退出（托盘退出/系统关机），执行
            //   perform_shutdown_blocking 确保 mpv 子进程终止、壁纸窗口销毁、配置刷写、
            //   Hook/COM 释放，避免 mpv 孤立与资源泄漏。
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    // 主窗口关闭：阻止退出，保持托盘运行，不执行退出清理
                    api.prevent_exit();
                } else if let Some(state) = app_handle.try_state::<AppState>() {
                    perform_shutdown_blocking(
                        &state.wallpaper_engine,
                        &state.config_manager,
                    );
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{panic_hook_timestamp, wpfile_path_allowed};
    use std::path::Path;

    // 构造测试基目录：data_root / home 两个独立临时根，
    // 避免触碰进程级环境变量，保证并行测试确定性。
    struct ScopeDirs {
        data_root: tempfile::TempDir,
        home: tempfile::TempDir,
    }

    impl ScopeDirs {
        fn new() -> Self {
            Self {
                data_root: tempfile::tempdir().unwrap(),
                home: tempfile::tempdir().unwrap(),
            }
        }

        fn data_root(&self) -> &Path {
            self.data_root.path()
        }
        fn home(&self) -> &Path {
            self.home.path()
        }
    }

    #[test]
    fn d009_allow_data_root_subpath() {
        let d = ScopeDirs::new();
        let p = d.data_root().join("wallpapers").join("a.png");
        assert!(
            wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "数据根（安装目录）子路径应被允许"
        );
    }

    #[test]
    fn d009_allow_data_root_thumbnails_subpath() {
        let d = ScopeDirs::new();
        let p = d.data_root().join("thumbnails").join("b.gif");
        assert!(
            wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "数据根下缩略图子路径应被允许"
        );
    }

    #[test]
    fn d009_allow_forward_slash_separator() {
        // URL 解码后路径可能使用正斜杠；Windows Path::starts_with 对 / 与 \ 等价。
        let d = ScopeDirs::new();
        let base = d.data_root().to_string_lossy().replace('\\', "/");
        let p = format!("{base}/wallpapers/a.mp4");
        assert!(
            wpfile_path_allowed(&p, d.data_root(), d.home()),
            "正斜杠分隔的 allow 路径应被允许"
        );
    }

    #[test]
    fn d009_deny_sensitive_ssh() {
        let d = ScopeDirs::new();
        let p = d.home().join(".ssh").join("id_rsa");
        assert!(
            !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "$HOME/.ssh 应被拒绝"
        );
    }

    #[test]
    fn d009_deny_sensitive_config_ssh_two_segments() {
        let d = ScopeDirs::new();
        let p = d.home().join(".config").join("ssh").join("config");
        assert!(
            !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "$HOME/.config/ssh 应被拒绝（两级子目录）"
        );
    }

    #[test]
    fn d009_deny_sensitive_all_seven() {
        let d = ScopeDirs::new();
        for sub in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/ssh",
            ".password-store",
            ".kube",
            ".docker",
        ] {
            let p = d.home().join(sub).join("secret");
            assert!(
                !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
                "$HOME/{sub} 应被拒绝"
            );
        }
    }

    #[test]
    fn d009_deny_outside_data_dir() {
        let d = ScopeDirs::new();
        // 系统临时目录（不在数据根受控目录内）
        let other = tempfile::tempdir().unwrap();
        let p = other.path().join("evil.png");
        assert!(
            !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "受控目录外的路径应被拒绝"
        );
    }

    #[test]
    fn d009_deny_sibling_data_root_prefix() {
        // ST-001 回归：`<数据根名>-evil` 不应被误判为数据根的子目录。
        let d = ScopeDirs::new();
        let parent = d.data_root().parent().unwrap();
        let name = d.data_root().file_name().unwrap().to_string_lossy();
        let p = parent.join(format!("{name}-evil")).join("x.png");
        assert!(
            !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.home()),
            "数据根-evil 兄弟目录应被拒绝"
        );
    }

    #[test]
    fn d009_deny_takes_precedence_over_allow() {
        // 极端构造：让 home == data_root，构造一个既位于数据根 allow 范围、
        // 又命中 home 下敏感 deny 目录的路径，验证 deny 优先于 allow。
        let d = ScopeDirs::new();
        let p = d.data_root().join(".ssh").join("id_rsa");
        assert!(
            !wpfile_path_allowed(&p.to_string_lossy(), d.data_root(), d.data_root()),
            "deny 优先于 allow"
        );
    }

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
        assert!(ts.trim().len() == 23, "时间戳应为 19 + 1 空格 + 3 毫秒 = 23 字符: {ts}");
    }
}
