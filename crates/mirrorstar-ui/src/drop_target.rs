//! 原生文件拖入（SubTask 5.5 / 9.7.4）。
//!
//! ## 为什么必须自实现
//!
//! Slint 的 winit 后端只实现了**窗口内**拖拽（`DragArea`/`DropArea`），不把操作系统的文件
//! 拖放事件转成任何可观测回调：winit 0.30 会把窗口注册成 OLE 放置目标（`RegisterDragDrop`
//! 及自带的 `IDropTarget`），但 `DroppedFile` 事件在 Slint 后端里被忽略。因此"从资源管理器
//! 拖入文件"必须由本模块自己接。
//!
//! ## 方案：自实现 OLE `IDropTarget`（Task 9.7.4 起为主通道）
//!
//! 旧式 `WM_DROPFILES`（`DragAcceptFiles`）只在**松手投放**时收到一条消息，**没有悬停**，
//! 无法实现"拖入态高亮"。因此主通道改为自实现 COM `IDropTarget`：`DragEnter`/`DragOver`/
//! `DragLeave` 提供悬停进出，`Drop` 提供最终投放。三者都在 UI 线程的消息泵中被调用。
//!
//! 有心跳的是：winit **默认已经**注册了 OLE 目标，它会抢占拖放。所以这里先 `RevokeDragDrop`
//! 撤掉它，再 `RegisterDragDrop` 挂上自己的目标（winit 在窗口销毁时还会自己 `RevokeDragDrop`
//! 一次，返回值被忽略，重复撤销无害）。
//!
//! 若 `RegisterDragDrop` 失败（例如 OLE 未初始化），回退到旧式 `WM_DROPFILES` + 子类化方案：
//! 功能仍可用，但**没有悬停高亮**。子类化用 `SetWindowLongPtrW(GWLP_WNDPROC)` 装一层壳，
//! 截获 `WM_DROPFILES` 自行消费，其余消息原样 `CallWindowProcW` 转交原窗口过程，不影响 winit。

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::{implement, Result as WinResult};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINTL, TRUE, WPARAM};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GetWindowLongPtrW, SetWindowLongPtrW, GWLP_WNDPROC,
    WM_DROPFILES, WNDPROC,
};

use crate::protocol::log;

/// 一次文件拖入的回调：入参为被拖入的本地路径（可能多个）。
pub type DropHandler = Arc<dyn Fn(Vec<String>) + Send + Sync + 'static>;

/// 悬停态回调：入参为"当前是否有可接受的本地文件悬停在窗口上"。
pub type HoverHandler = Arc<dyn Fn(bool) + Send + Sync + 'static>;

/// 安装期间可替换的两个回调（`install` 幂等重入时只更新这里的内容）。
struct Handlers {
    drop: DropHandler,
    hover: HoverHandler,
}

type SharedHandlers = Arc<Mutex<Handlers>>;

thread_local! {
    /// OLE 放置目标句柄：`RegisterDragDrop` 会 `AddRef` 保活，这里显式持有以便查询安装状态。
    static OLE_TARGET: RefCell<Option<IDropTarget>> = const { RefCell::new(None) };
    /// 回调表：OLE 目标与 WM_DROPFILES 回退共用同一份，便于幂等替换。
    static SHARED: RefCell<Option<SharedHandlers>> = const { RefCell::new(None) };
    /// WM_DROPFILES 回退路径：跨窗口过程调用存活的原窗口过程 + 回调表。
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

/// WM_DROPFILES 回退期间需要跨窗口过程调用存活的状态。
struct Hook {
    original: WNDPROC,
    handlers: SharedHandlers,
}

/// 从 Slint 窗口取原生 HWND（`raw-window-handle` 0.6 路径）。
///
/// 这是 Task 5.5 的最大不确定点：Slint 的 `Window::window_handle()` 需启用
/// `raw-window-handle-06` feature，返回的句柄是**惰性**的（可能要到窗口创建后才可用），
/// 因此调用点放在壳侧下发 `Show` 之后，并对失败做显式日志而非静默降级。
pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

/// 安装文件拖入接收器（**幂等**：重复调用只替换回调）。
///
/// 必须在 UI 线程调用（OLE 目标注册、子类化与 TLS 都绑定该线程）。
pub fn install(
    window: &slint::Window,
    handler: DropHandler,
    hover: HoverHandler,
) -> Result<(), String> {
    let hwnd = hwnd_of(window).ok_or_else(|| "未能从 Slint 窗口获取 HWND".to_string())?;

    // 幂等：已安装则仅替换回调（OLE 与回退两路都已装过就更新共享表即可）。
    if OLE_TARGET.with(|t| t.borrow().is_some()) || HOOK.with(|h| h.borrow().is_some()) {
        SHARED.with(|s| {
            if let Some(shared) = s.borrow().as_ref() {
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.drop = handler;
                guard.hover = hover;
            }
        });
        return Ok(());
    }

    let shared: SharedHandlers = Arc::new(Mutex::new(Handlers {
        drop: handler,
        hover,
    }));
    SHARED.with(|s| *s.borrow_mut() = Some(Arc::clone(&shared)));

    unsafe {
        match RevokeDragDrop(hwnd) {
            Ok(()) => log("已撤销 winit 默认 OLE 放置目标"),
            Err(error) => log(format!(
                "撤销 winit 默认 OLE 放置目标失败（仍尝试注册自实现目标）：{error}"
            )),
        }

        // 主通道：自实现 OLE IDropTarget（支持悬停高亮）。
        let target: IDropTarget = DropTarget.into();
        match RegisterDragDrop(hwnd, &target) {
            Ok(()) => {
                OLE_TARGET.with(|slot| *slot.borrow_mut() = Some(target));
                log(format!(
                    "原生文件拖入已就绪（OLE IDropTarget，支持悬停高亮；HWND=0x{:x}）",
                    hwnd.0 as usize
                ));
                return Ok(());
            }
            Err(error) => {
                log(format!(
                    "注册 OLE 放置目标失败，回退 WM_DROPFILES（无悬停高亮）：{error}"
                ));
            }
        }

        // 回退通道：旧式 WM_DROPFILES + 子类化（无悬停）。
        DragAcceptFiles(hwnd, TRUE);
        let previous = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as usize as isize);
        HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Hook {
                original: std::mem::transmute::<isize, WNDPROC>(previous),
                handlers: Arc::clone(&shared),
            });
        });
    }

    log(format!(
        "原生文件拖入已就绪（WM_DROPFILES 回退，无悬停高亮；HWND=0x{:x}）",
        hwnd.0 as usize
    ));
    Ok(())
}

/// 取出共享回调表并调用其中的投放回调（先克隆 `Arc`，避免持锁期间回调重入）。
///
/// 回调会穿越 OLE `IDropTarget::Drop` / `WM_DROPFILES` 窗口过程的 FFI 边界，
/// 因此用 `catch_unwind` 隔离 panic，避免其在 unwind 构建中展开越过 COM/Win32 边界。
/// 注意：release 构建启用 `panic = "abort"`，panic 会直接终止进程，无法被捕获。
fn dispatch_drop(shared: &SharedHandlers, paths: Vec<String>) {
    let callback = {
        let guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard.drop)
    };
    // 锁已释放，且 `paths` 在捕获 panic 后不再使用，可安全移入闭包。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(paths)));
    if result.is_err() {
        log("拖入投放回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
    }
}

/// 取出共享回调表并调用其中的悬停回调（同样隔离 panic，防止穿越 FFI 边界）。
fn dispatch_hover(shared: &SharedHandlers, over: bool) {
    let callback = {
        let guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard.hover)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(over)));
    if result.is_err() {
        log("拖入悬停回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
    }
}

// ── OLE IDropTarget 实现 ────────────────────────────────────────────────────

/// 自实现的文件放置目标。
///
/// 回调表通过 TLS（[`SHARED`]）读取：OLE 目标只在注册它的 UI 线程上收到回调，与
/// `WM_DROPFILES` 回退路径共用同一份回调表，且无需在结构体里另存一份。
#[implement(IDropTarget)]
struct DropTarget;

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let accepted = has_files(pdataobj);
        unsafe {
            *pdweffect = if accepted {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
        // 只有接受时才点亮高亮，避免把非文件负载误判为文件拖入。
        hover_from_tls(accepted);
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        // 能进入 DragOver 说明 DragEnter 已接受。
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> WinResult<()> {
        hover_from_tls(false);
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
        }
        hover_from_tls(false);
        if let Some(paths) = extract_paths(pdataobj) {
            drop_from_tls(paths);
        }
        Ok(())
    }
}

/// 取出当前 TLS 中的回调表（`Arc` 克隆，避免持锁/借用期间重入）。
fn shared_handlers() -> Option<SharedHandlers> {
    SHARED.with(|slot| slot.borrow().clone())
}

fn hover_from_tls(over: bool) {
    if let Some(shared) = shared_handlers() {
        dispatch_hover(&shared, over);
    }
}

fn drop_from_tls(paths: Vec<String>) {
    if let Some(shared) = shared_handlers() {
        dispatch_drop(&shared, paths);
    }
}

/// 判断数据对象是否携带本地文件（`CF_HDROP`）。
fn has_files(data: Option<&IDataObject>) -> bool {
    let Some(data) = data else {
        return false;
    };
    let format = file_formatetc();
    // `QueryGetData` 仅查询可用性，不分配 medium。
    unsafe { data.QueryGetData(&format).is_ok() }
}

/// `CF_HDROP` 的 `FORMATETC`（`lindex = -1` 表示"任意"）。
fn file_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// 从数据对象取出被拖入的全部本地路径（无 `CF_HDROP` 时返回 `None`）。
fn extract_paths(data: Option<&IDataObject>) -> Option<Vec<String>> {
    let data = data?;
    let format = file_formatetc();
    let mut medium = unsafe { data.GetData(&format) }.ok()?;
    let paths = unsafe {
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            // `HDROP` 就是 `CF_HDROP`/`STGMEDIUM` 中承载 `DROPFILES` 的全局内存句柄。
            let hdrop = HDROP(medium.u.hGlobal.0);
            Some(read_paths(hdrop))
        } else {
            None
        }
    };
    // 无论成功与否都必须释放 OLE 分配的 medium（`DROPFILES` 内存由 ReleaseStgMedium 释放）。
    unsafe { ReleaseStgMedium(&mut medium) };
    paths
}

// ── WM_DROPFILES 回退（无悬停） ─────────────────────────────────────────────

/// 子类化后的窗口过程：截获 `WM_DROPFILES`，其余消息转交 winit 的原窗口过程。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DROPFILES {
        let hdrop = HDROP(wparam.0 as *mut c_void);
        let paths = read_paths(hdrop);
        // 无论是否取到路径都必须释放：HDROP 指向 shell 分配的全局内存。
        DragFinish(hdrop);
        // 先克隆出 Arc 再调用，避免回调重入时再次借用 TLS 造成 panic。
        let shared = HOOK.with(|hook| hook.borrow().as_ref().map(|s| Arc::clone(&s.handlers)));
        if let Some(shared) = shared {
            dispatch_drop(&shared, paths);
        }
        // 已消费，不转发（winit 不认识该消息，转发只会落到 DefWindowProc）。
        return LRESULT(0);
    }

    // windows 0.58 的 `WNDPROC` 是 `Option<unsafe extern "system" fn…>`（空指针即 `None`）。
    let original = HOOK.with(|hook| hook.borrow().as_ref().map(|state| state.original));
    match original.flatten() {
        Some(previous) => CallWindowProcW(Some(previous), hwnd, msg, wparam, lparam),
        None => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 从 `HDROP` 取出全部路径（不负责释放句柄，由调用方 `DragFinish` / `ReleaseStgMedium`）。
unsafe fn read_paths(hdrop: HDROP) -> Vec<String> {
    // `ifile = 0xFFFF_FFFF` 是"查询文件数量"的约定。
    let count = DragQueryFileW(hdrop, u32::MAX, None);
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // 只传 hdrop/ifile 时返回所需缓冲区长度（不含结尾 NUL）。
        let len = DragQueryFileW(hdrop, index, None);
        if len == 0 {
            continue;
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let written = DragQueryFileW(hdrop, index, Some(buffer.as_mut_slice()));
        if written == 0 {
            continue;
        }
        let end = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(written as usize);
        paths.push(String::from_utf16_lossy(&buffer[..end]));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    /// 在**本进程**构造一个合法的 `DROPFILES` 全局内存块并调用 [`read_paths`]，
    /// 验证一次拖入的**每个**文件路径都能被逐条取出（Task 5.5 要求多文件拖入）。
    #[test]
    fn read_paths_returns_every_dropped_file() {
        let expected = [r"C:\tmp\alpha.png", r"C:\tmp\beta.jpg", r"C:\tmp\note.txt"];

        // DROPFILES 头：pFiles(DWORD=20) + pt(POINT=0,0) + fNC(BOOL=0) + fWide(BOOL=1)，随后是
        // 各路径的 UTF-16 串（单个 NUL 结尾），整体以双 NUL 收尾。
        let mut buf = Vec::new();
        buf.extend_from_slice(&20u32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&1i32.to_le_bytes());
        for path in expected {
            for unit in path.encode_utf16() {
                buf.extend_from_slice(&unit.to_le_bytes());
            }
            buf.extend_from_slice(&0u16.to_le_bytes());
        }
        buf.extend_from_slice(&0u16.to_le_bytes());

        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, buf.len()).expect("GlobalAlloc 失败");
            let ptr = GlobalLock(hglobal);
            assert!(!ptr.is_null(), "GlobalLock 失败");
            std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr.cast::<u8>(), buf.len());
            // `GlobalUnlock` 成功时返回 0，windows-rs 会把它映射成 `Err`，故忽略返回值。
            let _ = GlobalUnlock(hglobal);

            let hdrop = HDROP(hglobal.0);
            let got = read_paths(hdrop);
            // 与生产路径一致：读完后必须释放（`DragFinish` 负责释放该全局内存）。
            DragFinish(hdrop);

            let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, expected);
        }
    }

    /// 投放回调 panic 必须被 `dispatch_drop` 吞掉：它会穿越 OLE/`WM_DROPFILES` 的
    /// FFI 边界，unwind 越界会直接 abort 进程。
    #[test]
    fn dispatch_swallows_panicking_drop_callback() {
        let shared: SharedHandlers = Arc::new(Mutex::new(Handlers {
            drop: Arc::new(|_paths: Vec<String>| panic!("投放回调故意 panic")),
            hover: Arc::new(|_over: bool| {}),
        }));

        // 若 panic 未被捕获，测试线程会 unwind，测试失败。
        dispatch_drop(&shared, vec![r"C:\tmp\alpha.png".to_string()]);
    }

    /// 悬停回调 panic 同样必须被 `dispatch_hover` 吞掉。
    #[test]
    fn dispatch_swallows_panicking_hover_callback() {
        let shared: SharedHandlers = Arc::new(Mutex::new(Handlers {
            drop: Arc::new(|_paths: Vec<String>| {}),
            hover: Arc::new(|_over: bool| panic!("悬停回调故意 panic")),
        }));

        dispatch_hover(&shared, true);
        dispatch_hover(&shared, false);
    }

    /// panic 隔离后回调表仍可用：锁未被中毒、无状态残留，替换后的回调能正常收到路径。
    #[test]
    fn dispatch_recovers_after_panicking_drop_callback() {
        let shared: SharedHandlers = Arc::new(Mutex::new(Handlers {
            drop: Arc::new(|_paths: Vec<String>| panic!("投放回调故意 panic")),
            hover: Arc::new(|_over: bool| {}),
        }));

        dispatch_drop(&shared, vec![r"C:\tmp\alpha.png".to_string()]);

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_in_cb = Arc::clone(&calls);
        let seen_in_cb = Arc::clone(&seen);
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drop = Arc::new(move |paths: Vec<String>| {
                calls_in_cb.fetch_add(1, Ordering::SeqCst);
                *seen_in_cb
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = paths;
            });

        dispatch_drop(&shared, vec![r"C:\tmp\beta.jpg".to_string()]);

        assert_eq!(calls.load(Ordering::SeqCst), 1, "panic 后新回调应被调用一次");
        assert_eq!(
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![r"C:\tmp\beta.jpg".to_string()],
            "panic 后回调仍应收到正确路径"
        );
    }
}
