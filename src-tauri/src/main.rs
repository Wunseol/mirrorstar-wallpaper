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
                    // Already running, show message and exit
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
    if !ensure_single_instance() {
        return;
    }

    mirrorstar_wallpaper_lib::run()
}
