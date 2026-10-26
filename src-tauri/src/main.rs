#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

fn ensure_single_instance() -> bool {
    unsafe {
        match CreateMutexW(
            None,
            false,
            windows::core::w!("MirrorStarWallpaper_SingleInstance"),
        ) {
            Ok(_mutex) => {
                // CreateMutexW returns Ok even if mutex already exists,
                // need to check GetLastError
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    // 主流行为：再次启动时唤起已运行实例的主窗口（隐藏→显示、最小化→还原、
                    // 已可见→置前），不再只弹「已在运行」提示。
                    if mirrorstar_wallpaper_lib::single_instance::signal_primary_instance() {
                        return false;
                    }
                    // 信号无法送达（主实例尚未创建事件或异常）：回退提示对话框。
                    MessageBoxW(
                        None,
                        windows::core::w!("镜星壁纸已在运行中。"),
                        windows::core::w!("提示"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                    return false;
                }
                // ST-011: windows-rs 0.58 中 CreateMutexW 返回的 HANDLE 是普通结构体
                // （不实现 Drop，且为 Copy），drop 时不会自动调用 CloseHandle。
                // 因此无需 Box::leak 保活——句柄本身无 Drop 行为，仅作为数值令牌保留。
                // `let _ = _mutex;` 用于消除"未使用"警告（前缀 _ 已表示有意丢弃），
                // 不产生任何运行时效果。句柄实际生命周期延伸至进程退出（操作系统自动回收）。
                let _ = _mutex;
                true
            }
            Err(e) => {
                // ST-006：CreateMutexW 极少失败（仅在系统资源耗尽等极端情况）。
                // 此处 tracing 尚未初始化（init_logging 在 lib::run 中调用），
                // 因此使用 eprintln! 而非 tracing::error! 输出到 stderr。
                //
                // 设计权衡：失败时仍返回 true（继续启动）而非 false（阻止启动）：
                // - 阻止启动会让用户在极端系统条件下完全无法使用应用
                // - 继续启动最坏情况是多实例运行（mpv 子进程/WorkerW 嵌入独立），
                //   配置文件写入由 ConfigManager 内部锁保护，不会数据损坏
                // - 真机环境几乎不会触发此分支
                eprintln!(
                    "创建单实例互斥体失败（单实例检测被绕过，应用仍将启动）: {}",
                    e
                );
                true
            }
        }
    }
}

fn main() {
    // 同一 exe、双入口：命令行含 `--wp-proc` 时进入网页壁纸子进程模式。
    //
    // 该判断必须在 `ensure_single_instance()` 之前：wp-proc 是主壳 spawn 的独立子进程，
    // 不能参与主实例的单实例互斥体检测，也不得初始化 Tauri/托盘。
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(pos) = args.iter().position(|a| a.to_str() == Some("--wp-proc")) {
        // 仅移除第一个 `--wp-proc` 分派标记：argv[0]（exe 路径）及其余参数保持不变。
        args.remove(pos);
        if let Err(e) = mirrorstar_wp_proc::run_with_argv(args) {
            // windows_subsystem="windows" 下不分配控制台，故该输出仅在带控制台运行时可见；
            // 关键行为是退出码 1，父进程据此检测子进程死亡。
            eprintln!("mirrorstar-wp-proc 退出: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if !ensure_single_instance() {
        return;
    }

    mirrorstar_wallpaper_lib::run()
}
