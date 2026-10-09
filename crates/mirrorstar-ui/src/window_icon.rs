//! 显式设置主窗口应用图标（best-effort 加固）。
//!
//! ## 为什么 EXE 已嵌入资源还要再设一次
//!
//! `mirrorstar-ui.exe` 已通过 `app.rc` 把 `icon.ico` 嵌入为最低 id 的 `RT_GROUP_ICON`，
//! Windows 在窗口与窗口类都未显式持有图标时会**回退取用**它，任务栏 / 标题栏 / Alt-Tab /
//! 资源管理器基本已能显示图标。此处显式 `WM_SETICON` 的加固动机是：`ICON_SMALL`（标题栏）
//! 与 `ICON_BIG`（任务栏 / Alt-Tab）各自需要 `.ico` 中**对应尺寸**的帧——按系统尺寸分别
//! `LoadImageW` 命中正确的帧，避免小尺寸被缩放导致发糊。
//!
//! ## best-effort
//!
//! 图标纯属装饰：任何一步失败都只记日志、**不 panic、不抢前台**；取不到 HWND（或未成功
//! 加载到任何一档）时安静跳过。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HINSTANCE, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, SendMessageW, SetClassLongPtrW, GCLP_HICON, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, LR_SHARED, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
};

use crate::protocol::log;

/// 图标资源 id，必须与 `app.rc` 里的 `1 ICON` 保持一致。
const ICON_RESOURCE_ID: u16 = 1;

/// 按系统尺寸加载一份图标；失败返回 `None` 并记日志。
///
/// `cx`/`cy` 决定挑 `.ico` 中哪一帧：任务栏走大图标尺寸、标题栏走小图标尺寸。
fn load_icon(hinst: HINSTANCE, cx: i32, cy: i32) -> Option<HANDLE> {
    // 名称是整数资源 id 的 PCWSTR：windows-rs 0.58 没有 `MAKEINTRESOURCEW`，
    // 直接把 id 当指针构造即可（低位整数即 user32 约定的「整数原子」形式）。
    let name = PCWSTR(ICON_RESOURCE_ID as usize as *const u16);
    // SAFETY: hinst 来自本进程 `GetModuleHandleW`；name 是整数原子而非解引用指针；
    // `LR_SHARED` 让系统共享缓存图标（勿自行 DestroyIcon），返回句柄在进程存活期内有效。
    match unsafe { LoadImageW(hinst, name, IMAGE_ICON, cx, cy, LR_SHARED) } {
        Ok(handle) => Some(handle),
        Err(e) => {
            log(format!("设置窗口图标：加载图标失败：{e}"));
            None
        }
    }
}

/// 给窗口显式设置应用图标（`WM_SETICON`）。幂等、best-effort，不 panic。
pub fn apply(window: &slint::Window) {
    let hwnd = match crate::drop_target::hwnd_of(window) {
        Some(hwnd) => hwnd,
        None => {
            log("设置窗口图标：未能从 Slint 窗口获取 HWND，跳过");
            return;
        }
    };

    // 当前 EXE 实例句柄：图标资源就嵌在本模块内。
    let hinst: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module.into(),
        Err(e) => {
            log(format!("设置窗口图标：GetModuleHandleW 失败：{e}"));
            return;
        }
    };

    // 大 / 小两档按系统尺寸分别加载，让各显示位取到 .ico 中对应尺寸的帧。
    let big = load_icon(hinst, unsafe { GetSystemMetrics(SM_CXICON) }, unsafe {
        GetSystemMetrics(SM_CYICON)
    });
    let small = load_icon(hinst, unsafe { GetSystemMetrics(SM_CXSMICON) }, unsafe {
        GetSystemMetrics(SM_CYSMICON)
    });

    let has_any = big.is_some() || small.is_some();

    if let Some(big) = big {
        // SAFETY: hwnd 来自本进程 Slint 窗口；图标句柄由系统以 `LR_SHARED` 管理，不悬垂。
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(big.0 as isize),
            );
            // 同步窗口类图标，使后续新建的同类窗口也继承该图标。
            SetClassLongPtrW(hwnd, GCLP_HICON, big.0 as isize);
        }
    }
    if let Some(small) = small {
        // SAFETY: 同上。
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small.0 as isize),
            );
        }
    }

    if has_any {
        log("已显式设置窗口图标（WM_SETICON）");
    } else {
        log("设置窗口图标：两档尺寸均加载失败，已跳过");
    }
}
