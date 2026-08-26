use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock, RwLock};

use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::wallpaper::{PauseCommand, PauseSender, RendererState, ScalingMode, WallpaperState};

use super::gdi_cache::GdiCache;

/// WM_DPICHANGED 消息常量（windows crate 未默认导出，需手动定义）
const WM_DPICHANGED: u32 = 0x02E0;

/// WM_DISPLAYCHANGE 消息常量（windows crate 未默认导出，需手动定义）
///
/// 当显示分辨率发生变化（如显示器模式切换、热插拔）时发送给所有窗口。
/// v5.0 W-PERF-003：在此消息处理中失效屏幕分辨率缓存，使后续
/// `get_screen_size()` 重新查询实际分辨率。
const WM_DISPLAYCHANGE: u32 = 0x001E;

/// BI_BITFIELDS 32bpp 的 BITMAPINFO 等价结构：BITMAPINFOHEADER + 3 个颜色掩码 DWORD。
///
/// 用于让 GDI 直接解释 RGBA 字节序内存（[R,G,B,A]），省去像素处理路径
/// （`process_gif_frame` / `load_and_downsample_image`）的 RGBA→BGRA swap 循环。
///
/// 掩码对应 little-endian u32 = A<<24 | B<<16 | G<<8 | R 的各字节位置：
/// - R mask = 0x000000FF（低字节）
/// - G mask = 0x0000FF00（次低字节）
/// - B mask = 0x00FF0000（次高字节）
///
/// `repr(C)` 保证内存布局与 Win32 `BITMAPINFOHEADER` + `bmiColors[3]` 一致，
/// 可通过裸指针转换传给 `StretchDIBits` / `CreateDIBSection`（它们按
/// `biCompression=BI_BITFIELDS` 读取 header 后的 3 个掩码 DWORD）。
#[repr(C)]
struct BitmapInfoBitfields {
    header: BITMAPINFOHEADER,
    masks: [u32; 3],
}

impl BitmapInfoBitfields {
    /// 构造 top-down 32bpp RGBA 的 BITMAPINFO（BI_BITFIELDS + RGB 掩码）。
    ///
    /// `biHeight` 取负值表示 top-down DIB（与原 BI_RGB 路径一致）。
    fn new(img_w: i32, img_h: i32) -> Self {
        Self {
            header: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: img_w,
                biHeight: -img_h, // 负值表示从上到下的 DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_BITFIELDS.0,
                ..Default::default()
            },
            // 顺序为 R, G, B（Win32 BI_BITFIELDS 规范）
            masks: [0x000000FF, 0x0000FF00, 0x00FF0000],
        }
    }
}

/// GDI 渲染器公共基类，封装 `ImageRenderer` 和 `GifRenderer` 的共同模式：
/// - 公共状态字段（hwnd / thread_handle / scaling_mode / state / pause_sender）
/// - 通用命令发送（mpsc + PostMessageW 唤醒）
/// - 通用终止流程（发送终止命令、唤醒线程、join、清理状态）
///
/// 注意：具体命令枚举（`WallpaperCommand` / `GifCommand`）类型不同，
/// 因此 `send_command` / `terminate` 设计为泛型方法，由各子渲染器传入自身的
/// 命令通道与终止命令。Win32 窗口类注册、窗口创建、双缓冲绘制等逻辑提取为
/// 本模块的自由函数，供两个渲染器的专用线程与窗口过程复用。
pub struct GdiRendererBase {
    /// 壁纸窗口句柄（线程创建后设置）
    pub(crate) hwnd: Option<HWND>,
    /// 线程 JoinHandle
    pub(crate) thread_handle: Option<std::thread::JoinHandle<()>>,
    /// 缩放模式
    pub(crate) scaling_mode: ScalingMode,
    /// 当前状态
    pub(crate) state: WallpaperState,
    /// 快速控制发送端（play() 成功后由子渲染器 create_pause_sender 设置）
    pub(crate) pause_sender: Option<PauseSender>,
}

impl GdiRendererBase {
    /// 创建新的 GDI 渲染器基类
    pub fn new(scaling_mode: ScalingMode) -> Self {
        Self {
            hwnd: None,
            thread_handle: None,
            scaling_mode,
            state: WallpaperState::Initializing,
            pause_sender: None,
        }
    }

    /// 获取壁纸窗口句柄
    pub fn hwnd(&self) -> Option<HWND> {
        self.hwnd
    }

    /// 设置壁纸窗口句柄
    pub fn set_hwnd(&mut self, hwnd: Option<HWND>) {
        self.hwnd = hwnd;
    }

    /// 获取当前壁纸状态
    pub fn state(&self) -> WallpaperState {
        self.state
    }

    /// 设置壁纸状态
    pub fn set_state(&mut self, state: WallpaperState) {
        self.state = state;
    }

    /// 获取缩放模式
    pub fn scaling_mode(&self) -> ScalingMode {
        self.scaling_mode
    }

    /// 设置缩放模式
    pub fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.scaling_mode = mode;
    }

    /// 设置线程 JoinHandle
    pub fn set_thread_handle(&mut self, handle: Option<std::thread::JoinHandle<()>>) {
        self.thread_handle = handle;
    }

    /// 设置 pause sender
    pub fn set_pause_sender(&mut self, sender: Option<PauseSender>) {
        self.pause_sender = sender;
    }

    /// 发送命令到壁纸线程，并通过 PostMessageW 唤醒消息循环。
    ///
    /// 泛型参数 `C` 为具体命令枚举类型，由子渲染器指定。
    pub fn send_command<C>(
        &self,
        cmd_tx: &Sender<C>,
        cmd: C,
        wake_msg: u32,
    ) -> Result<(), crate::MirrorStarError> {
        cmd_tx.send(cmd).map_err(|e| {
            crate::MirrorStarError::DesktopIntegration(format!("发送命令失败: {}", e))
        })?;
        if let Some(hwnd) = self.hwnd {
            // SAFETY: PostMessageW 是线程安全的，hwnd 仅作为值传递。
            // 此调用迁移自 ImageRenderer::send_command / GifRenderer::send_command，行为不变。
            unsafe {
                if PostMessageW(hwnd, wake_msg, WPARAM(0), LPARAM(0)).is_err() {
                    tracing::warn!("PostMessageW 失败：wake_msg 未送达（窗口可能已销毁）");
                }
            }
        }
        Ok(())
    }

    /// 通用终止流程：发送终止命令、唤醒消息循环、join 线程、清理状态。
    ///
    /// 泛型参数 `C` 为具体命令枚举类型，`terminate_cmd` 为对应的终止命令变体。
    pub fn terminate<C: Send + 'static>(
        &mut self,
        cmd_tx: &Sender<C>,
        terminate_cmd: C,
        wake_msg: u32,
    ) -> Result<(), crate::MirrorStarError> {
        if let Some(handle) = self.thread_handle.take() {
            // send_command 失败（通道断开）时线程仍可能在运行，继续 join 等待退出
            if let Err(e) = self.send_command(cmd_tx, terminate_cmd, wake_msg) {
                tracing::warn!(error = %e, "terminate 时发送终止命令失败");
            }
            // join 失败表示线程 panic，仅记录日志（无法传播）
            if let Err(e) = handle.join() {
                tracing::warn!(error = ?e, "terminate 时 join 线程失败（线程可能已 panic）");
            }
        }
        self.hwnd = None;
        self.state = WallpaperState::Terminated;
        Ok(())
    }
}

// ── Win32 辅助函数 ───────────────────────────────────────────────────────────

/// 获取屏幕分辨率作为初始窗口尺寸，失败时回退到 1920×1080。
///
/// 两个渲染器的 `play()` 都以此作为窗口初始矩形。
pub fn get_initial_window_rect() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics 仅读取系统指标，无副作用，迁移自原实现。
    let (screen_w, screen_h) = unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        (if w > 0 { w } else { 1920 }, if h > 0 { h } else { 1080 })
    };
    (0, 0, screen_w, screen_h)
}

/// 注册窗口类（仅执行一次）。
///
/// `once_lock` 为各渲染器自有的静态 `OnceLock<()>`，确保窗口类仅注册一次。
/// `class_name` 为编译期常量类名，`wnd_proc` 为对应窗口过程函数指针。
/// 返回传入的 `class_name` 以便调用方链式使用。
///
/// W08：区分 `ERROR_CLASS_ALREADY_EXISTS`（窗口类已注册，幂等正常）与真实失败
/// （其他错误码，记录 `tracing::error!` 以便诊断根因）。原实现 `let _ = RegisterClassW(&wc)`
/// 丢弃所有返回值，掩盖了真实的注册失败（如 `wnd_proc` 无效、模块句柄错误等）。
///
/// 区分逻辑由纯辅助函数 [`classify_register_class_result`] 承载，便于单元测试
/// 覆盖三种结果（成功 / 已注册 / 真实失败）；本函数仅负责调用 Win32 API 与日志输出。
///
/// SAFETY: 内部使用 GetModuleHandleW / RegisterClassW，迁移自原
/// `register_window_class` / `register_gif_window_class`，行为不变。
pub fn register_window_class_once(
    once_lock: &'static OnceLock<()>,
    class_name: windows::core::PCWSTR,
    wnd_proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> windows::core::PCWSTR {
    once_lock.get_or_init(|| unsafe {
        let h_instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            hInstance: HINSTANCE(h_instance.0),
            lpszClassName: class_name,
            lpfnWndProc: Some(wnd_proc),
            ..Default::default()
        };
        // W08: 检查 RegisterClassW 返回值，通过纯辅助函数区分"已注册"与真实失败
        let atom = RegisterClassW(&wc);
        let err = GetLastError();
        // GetLastError 返回 WIN32_ERROR（tuple struct），classify 接收 u32，取 .0 解包
        match classify_register_class_result(atom, err.0) {
            RegisterClassOutcome::Success => {
                // 注册成功，无需额外日志（OnceLock 保证仅执行一次）
            }
            RegisterClassOutcome::AlreadyExists => {
                // 窗口类已注册（例如前次调用已成功），幂等正常，不记录错误
                tracing::debug!("窗口类已注册，跳过重复注册");
            }
            RegisterClassOutcome::Failed(code) => {
                // 真实失败：记录错误码以便诊断根因（wnd_proc 无效、模块句柄错误等）
                tracing::error!(
                    error_code = code,
                    class_name = ?class_name,
                    "RegisterClassW 失败（非 ERROR_CLASS_ALREADY_EXISTS）"
                );
            }
        }
    });
    class_name
}

/// `RegisterClassW` 调用结果的分类（W08）
///
/// 将 `RegisterClassW` 的返回值（atom）与 `GetLastError` 的错误码映射为三种互斥结果，
/// 供 `register_window_class_once` 决定日志级别。此纯函数从 Win32 调用现场抽离，
/// 便于单元测试覆盖全部分支——`RegisterClassW` 本身需真实窗口环境，无法直接单测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterClassOutcome {
    /// `RegisterClassW` 返回非零 atom，注册成功
    Success,
    /// atom == 0 且 `GetLastError` == `ERROR_CLASS_ALREADY_EXISTS`
    /// （窗口类已注册，幂等正常，调用方可继续使用该类名）
    AlreadyExists,
    /// atom == 0 且其他错误码（真实失败：`wnd_proc` 无效、模块句柄错误等）
    Failed(u32),
}

/// 将 `RegisterClassW(atom)` + `GetLastError(err)` 的结果分类（W08 纯辅助函数）
///
/// `atom` 为 `RegisterClassW` 的返回值（`ATOM` = `u16`）：非零表示成功，零表示失败。
/// `err` 为 `GetLastError()` 的返回值（`WIN32_ERROR.0` = `u32`）：仅在 `atom == 0` 时有意义。
fn classify_register_class_result(atom: u16, err: u32) -> RegisterClassOutcome {
    if atom != 0 {
        RegisterClassOutcome::Success
    } else if err == ERROR_CLASS_ALREADY_EXISTS.0 {
        RegisterClassOutcome::AlreadyExists
    } else {
        RegisterClassOutcome::Failed(err)
    }
}

/// 创建壁纸窗口（WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS）。
///
/// 创建失败返回 `MirrorStarError::DesktopIntegration`，与原实现一致。
pub fn create_wallpaper_window(
    class_name: windows::core::PCWSTR,
    title: windows::core::PCWSTR,
    initial_rect: (i32, i32, i32, i32),
) -> Result<HWND, crate::MirrorStarError> {
    // SAFETY: CreateWindowExW 参数均为值或 PCWSTR 常量，迁移自原 wallpaper_thread /
    // gif_wallpaper_thread 中的窗口创建逻辑，行为不变。
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            title,
            WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            initial_rect.0,
            initial_rect.1,
            initial_rect.2,
            initial_rect.3,
            HWND::default(),
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        )
    };
    match hwnd {
        Ok(h) if !h.is_invalid() && h != HWND::default() => Ok(h),
        Ok(_) => Err(crate::MirrorStarError::DesktopIntegration(
            "创建窗口失败: 无效句柄".to_string(),
        )),
        Err(e) => Err(crate::MirrorStarError::DesktopIntegration(format!(
            "创建窗口失败: {}",
            e
        ))),
    }
}

/// 执行双缓冲绘制：填充黑色背景 → HALFTONE 缩放 → StretchDIBits → BitBlt 到屏幕 DC。
///
/// 调用者负责 `BeginPaint` / `EndPaint` 以及从 `GWLP_USERDATA` 取出窗口数据，
/// 并传入待绘制图像的宽度、高度、RGBA 像素数据与缩放模式（GDI 通过 BI_BITFIELDS
/// 掩码直接解释 RGBA，无需调用方做 BGRA 转换）。
/// `gdi_cache` 为窗口数据中缓存的 GDI 对象，首次绘制或尺寸变化时自动创建/重建。
///
/// # Safety
///
/// 调用者必须确保 `hwnd` 是有效窗口、`hdc` 是有效的 paint DC。
/// 此函数迁移自原 `wallpaper_wnd_proc` / `gif_wnd_proc` 的 WM_PAINT 分支，行为不变。
pub unsafe fn paint_with_double_buffer(
    hwnd: HWND,
    hdc: HDC,
    gdi_cache: &mut Option<GdiCache>,
    img_w: u32,
    img_h: u32,
    pixels: &[u8],
    scaling_mode: ScalingMode,
) {
    // 获取窗口客户区大小
    let mut client_rect = RECT::default();
    if let Err(e) = GetClientRect(hwnd, &mut client_rect) {
        tracing::warn!(error = %e, "GetClientRect 失败，客户区尺寸将为 0");
    }
    let client_w = client_rect.right - client_rect.left;
    let client_h = client_rect.bottom - client_rect.top;

    // 确保缓存的 GDI 对象可用且尺寸匹配
    let needs_create = gdi_cache.is_none();
    let needs_resize = gdi_cache.as_ref().is_some_and(|c| {
        let (w, h) = c.dimensions();
        w != client_w || h != client_h
    });

    if needs_create {
        // 首次创建内存 DC 和位图
        *gdi_cache = match GdiCache::new(hdc, client_w, client_h) {
            Ok(cache) => Some(cache),
            Err(e) => {
                tracing::error!(error = %e, "GdiCache 创建失败，跳过本次渲染");
                None
            }
        };
    } else if needs_resize {
        // 窗口尺寸变化，重建位图（mem_dc 保留复用）
        if let Some(ref mut cache) = gdi_cache {
            if let Err(e) = cache.resize(hdc, client_w, client_h) {
                tracing::error!(error = %e, "GdiCache resize 失败，丢弃缓存（下次 WM_PAINT 重建）");
                *gdi_cache = None;
            }
        }
    }

    // 使用缓存的 GDI 对象进行渲染
    if let Some(ref cache) = gdi_cache {
        // 计算缩放区域（先算绘制矩形，再决定是否需要黑底填充）
        let (draw_x, draw_y, draw_w, draw_h) =
            super::calculate_scaling(img_w, img_h, client_w as u32, client_h as u32, scaling_mode);

        // #6: 仅当绘制矩形未完全覆盖客户区（存在黑边）或无像素可绘时才填充黑色背景。
        // Fill/Stretch 等覆盖模式下绘制矩形完全覆盖客户区，StretchDIBits 会覆写全部像素，
        // 此前 FillRect 的全屏黑底会被完全覆盖 → 跳过以省一次全屏 GDI 填充。
        // Fit/Center/Original 等留黑边模式仍需 FillRect 填充 letterbox 区域。
        let will_draw = img_w > 0 && img_h > 0 && !pixels.is_empty();
        let covers_full = will_draw
            && super::draw_rect_covers_full(draw_x, draw_y, draw_w, draw_h, client_w, client_h);
        if !covers_full {
            let black_brush = GetStockObject(BLACK_BRUSH);
            if FillRect(cache.mem_dc(), &client_rect, HBRUSH(black_brush.0)) == 0 {
                tracing::warn!("FillRect 失败：无法填充黑色背景");
            }
        }

        // v5.0 W-PERF-002: SetStretchBltMode(HALFTONE) 与 SetBrushOrgEx(0,0) 已移至
        // GdiCache::new 中一次性设置——DC 状态在 mem_dc 生命周期内持续有效，
        // 不会被 BitBlt/StretchDIBits/SelectObject(bitmap) 重置。

        // 将图片绘制到内存 DC
        if img_w > 0 && img_h > 0 && !pixels.is_empty() {
            // BI_BITFIELDS + RGB 掩码：GDI 直接解释 RGBA 字节序，无需 BGRA swap
            let bmi = BitmapInfoBitfields::new(img_w as i32, img_h as i32);

            if StretchDIBits(
                cache.mem_dc(),
                draw_x,
                draw_y,
                draw_w,
                draw_h,
                0,
                0,
                img_w as i32,
                img_h as i32,
                Some(pixels.as_ptr() as *const _),
                &bmi as *const BitmapInfoBitfields as *const BITMAPINFO,
                DIB_RGB_COLORS,
                SRCCOPY,
            ) == 0
            {
                tracing::error!("StretchDIBits 失败：图片绘制到内存 DC 失败（可能黑屏）");
            }
        }

        // 一次性 BitBlt 到屏幕 DC，消除闪烁
        if BitBlt(hdc, 0, 0, client_w, client_h, cache.mem_dc(), 0, 0, SRCCOPY).is_err() {
            tracing::error!("BitBlt 失败：内存 DC 合成到屏幕失败（可能黑屏）");
        }
    }
}

/// v8.0: 从像素数据创建 HBITMAP（DIB Section）
///
/// 使用 `CreateDIBSection` 创建 32bpp RGBA top-down DIB Section（BI_BITFIELDS +
/// RGB 掩码），并将像素数据复制进去。创建后调用者可释放原始 `pixels`，仅保留
/// HBITMAP 用于后续绘制（`StretchBlt`）。
///
/// 返回 `None` 表示创建失败（GDI 资源不足或参数无效），调用者应保留 pixels 作为回退。
pub fn create_image_bitmap(hdc: HDC, img_w: u32, img_h: u32, pixels: &[u8]) -> Option<HBITMAP> {
    if img_w == 0 || img_h == 0 || pixels.is_empty() {
        return None;
    }
    unsafe {
        // BI_BITFIELDS + RGB 掩码：DIB Section 内存布局为 RGBA（与 StretchDIBits 路径一致）
        let bmi = BitmapInfoBitfields::new(img_w as i32, img_h as i32);
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbitmap = CreateDIBSection(
            hdc,
            &bmi as *const BitmapInfoBitfields as *const BITMAPINFO,
            DIB_RGB_COLORS,
            &mut bits as *mut *mut _,
            None,
            0,
        )
        .ok()?;
        if bits.is_null() {
            let _ = DeleteObject(hbitmap);
            return None;
        }
        // 将 pixels 复制到 DIB Section 的位图数据区
        let bits_size = (img_w as usize) * (img_h as usize) * 4;
        let copy_size = bits_size.min(pixels.len());
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, copy_size);
        Some(hbitmap)
    }
}

/// v8.0: 使用缓存的 HBITMAP 执行双缓冲绘制（Image 渲染器专用）
///
/// 与 [`paint_with_double_buffer`] 的差异：使用 HBITMAP（通过 `SelectObject` + `StretchBlt`）
/// 替代 pixels（通过 `StretchDIBits`），避免每次 WM_PAINT 重新解码像素数据。
/// GdiCache 的创建/resize 逻辑与 `paint_with_double_buffer` 完全一致。
///
/// # Safety
///
/// 调用者必须确保 `hwnd` 是有效窗口、`hdc` 是有效的 paint DC、`image_bitmap` 是有效的 HBITMAP。
pub unsafe fn paint_image_with_double_buffer(
    hwnd: HWND,
    hdc: HDC,
    gdi_cache: &mut Option<GdiCache>,
    img_w: u32,
    img_h: u32,
    image_bitmap: HBITMAP,
    scaling_mode: ScalingMode,
) {
    // 获取窗口客户区大小
    let mut client_rect = RECT::default();
    if let Err(e) = GetClientRect(hwnd, &mut client_rect) {
        tracing::warn!(error = %e, "GetClientRect 失败，客户区尺寸将为 0");
    }
    let client_w = client_rect.right - client_rect.left;
    let client_h = client_rect.bottom - client_rect.top;

    // 确保缓存的 GDI 对象可用且尺寸匹配（与 paint_with_double_buffer 一致）
    let needs_create = gdi_cache.is_none();
    let needs_resize = gdi_cache.as_ref().is_some_and(|c| {
        let (w, h) = c.dimensions();
        w != client_w || h != client_h
    });

    if needs_create {
        *gdi_cache = match GdiCache::new(hdc, client_w, client_h) {
            Ok(cache) => Some(cache),
            Err(e) => {
                tracing::error!(error = %e, "GdiCache 创建失败，跳过本次渲染");
                None
            }
        };
    } else if needs_resize {
        if let Some(ref mut cache) = gdi_cache {
            if let Err(e) = cache.resize(hdc, client_w, client_h) {
                tracing::error!(error = %e, "GdiCache resize 失败，丢弃缓存（下次 WM_PAINT 重建）");
                *gdi_cache = None;
            }
        }
    }

    // 使用缓存的 GDI 对象进行渲染
    if let Some(ref cache) = gdi_cache {
        // 填充黑色背景
        let black_brush = GetStockObject(BLACK_BRUSH);
        if FillRect(cache.mem_dc(), &client_rect, HBRUSH(black_brush.0)) == 0 {
            tracing::warn!("FillRect 失败：无法填充黑色背景");
        }

        // 计算缩放区域
        let (draw_x, draw_y, draw_w, draw_h) =
            super::calculate_scaling(img_w, img_h, client_w as u32, client_h as u32, scaling_mode);

        // v8.0: 使用缓存的 image_bitmap 通过 StretchBlt 绘制到内存 DC
        // 创建临时源 DC，选入 image_bitmap，StretchBlt 到 mem_dc，最后清理
        if img_w > 0 && img_h > 0 {
            let src_dc = CreateCompatibleDC(hdc);
            if src_dc != HDC::default() {
                let old_obj = SelectObject(src_dc, image_bitmap);
                // HALFTONE 模式已由 mem_dc 的 SetStretchBltMode 设置（GdiCache::new 中），
                // StretchBlt 使用目标 DC 的拉伸模式
                if !StretchBlt(
                    cache.mem_dc(),
                    draw_x,
                    draw_y,
                    draw_w,
                    draw_h,
                    src_dc,
                    0,
                    0,
                    img_w as i32,
                    img_h as i32,
                    SRCCOPY,
                )
                .as_bool()
                {
                    tracing::error!("StretchBlt 失败：图片绘制到内存 DC 失败（可能黑屏）");
                }
                SelectObject(src_dc, old_obj);
                let _ = DeleteDC(src_dc);
            }
        }

        // 一次性 BitBlt 到屏幕 DC，消除闪烁
        if BitBlt(hdc, 0, 0, client_w, client_h, cache.mem_dc(), 0, 0, SRCCOPY).is_err() {
            tracing::error!("BitBlt 失败：内存 DC 合成到屏幕失败（可能黑屏）");
        }
    }
}

/// 处理 GDI 渲染器共有的窗口消息（`WM_ERASEBKGND` / `WM_DPICHANGED` /
/// `WM_DISPLAYCHANGE` / `WM_SIZE`）。
///
/// 返回 `Some(LRESULT)` 表示已处理（调用方应直接返回该值），
/// 返回 `None` 表示未处理（调用方应继续匹配或调用 `DefWindowProcW`）。
///
/// # v5.0 W-PERF-003
///
/// 在 `WM_DPICHANGED` 与 `WM_DISPLAYCHANGE` 处理中调用
/// `invalidate_screen_size_cache()` 失效屏幕分辨率缓存，使后续
/// `get_screen_size()` 重新查询实际分辨率（覆盖多显示器热插拔 / DPI 变更场景）。
///
/// # Safety
///
/// 调用者必须确保 `hwnd` 是有效窗口。此函数迁移自原
/// `wallpaper_wnd_proc` / `gif_wnd_proc` 中三个完全相同的消息分支，行为不变。
pub unsafe fn try_handle_common_messages(hwnd: HWND, msg: u32, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_ERASEBKGND => {
            // 防止闪烁 - 背景在 WM_PAINT 中处理
            Some(LRESULT(1))
        }
        WM_DPICHANGED => {
            // lParam contains the suggested new window rect
            let rect = &*(lparam.0 as *const RECT);
            if SetWindowPos(
                hwnd,
                HWND::default(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .is_err()
            {
                tracing::warn!("SetWindowPos 失败：WM_DPICHANGED 时无法更新窗口位置/尺寸");
            }
            // v5.0 W-PERF-003: DPI 变化意味着屏幕物理分辨率可能改变，
            // 失效缓存使后续 get_screen_size() 重新查询实际分辨率。
            super::invalidate_screen_size_cache();
            // InvalidateRect 失败仅跳过本次重绘，下一次 WM_PAINT 会重新触发
            let _ = InvalidateRect(hwnd, None, false);
            Some(LRESULT(0))
        }
        WM_DISPLAYCHANGE => {
            // v5.0 W-PERF-003: 显示器配置变更（分辨率切换 / 热插拔），
            // 失效缓存使后续 get_screen_size() 重新查询实际分辨率。
            super::invalidate_screen_size_cache();
            // 触发重绘以反映可能的尺寸变化
            // InvalidateRect 失败仅跳过本次重绘，下一次 WM_PAINT 会重新触发
            let _ = InvalidateRect(hwnd, None, false);
            Some(LRESULT(0))
        }
        WM_SIZE => {
            // 窗口大小变化时触发重绘
            // InvalidateRect 失败仅跳过本次重绘，下一次 WM_PAINT 会重新触发
            let _ = InvalidateRect(hwnd, None, false);
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// 启动 pause 转发后台线程，将 `PauseCommand` 转换为渲染器专用命令并发送到壁纸线程。
///
/// 消除 `ImageRenderer::create_pause_sender` 与 `GifRenderer::create_pause_sender` 中
/// 近乎相同的线程启动逻辑。两个无音频渲染器对 `SetVolume` / `ToggleMute` 一律忽略。
///
/// - `thread_name`：线程名（用于诊断）
/// - `renderer_name`：渲染器名（用于退出日志）
/// - `cmd_tx`：壁纸线程命令通道发送端（移入线程）
/// - `hwnd_raw`：窗口句柄的原始 `isize` 值（HWND 非 Send，跨线程传递）
/// - `wake_msg`：唤醒消息常量（`WM_WALLPAPER_COMMAND` / `WM_GIF_COMMAND`）
/// - `make_pause` / `make_resume`：构造对应终止命令变体的闭包
/// - `rx` / `shared_state`：由 `create_pause_channel` 创建
/// - `state_changed`：`PauseSender` 的 clone，用于在状态变更后通知 Tauri 层
///   emit `wallpaper-state-changed` 事件
/// - `display_id`：该渲染器所属显示器 ID，作为 notify_state_changed 的 payload
///
/// 返回 `Err(io::Error)` 表示线程创建失败，调用方应返回 `None`。
#[allow(clippy::too_many_arguments)]
pub fn spawn_pause_forwarder<C: Send + 'static>(
    thread_name: &str,
    renderer_name: &'static str,
    cmd_tx: Sender<C>,
    hwnd_raw: Option<isize>,
    wake_msg: u32,
    make_pause: impl Fn() -> C + Send + 'static,
    make_resume: impl Fn() -> C + Send + 'static,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<PauseCommand>,
    shared_state: Arc<RwLock<RendererState>>,
    state_changed: PauseSender,
    display_id: String,
) -> Result<(), std::io::Error> {
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    PauseCommand::Pause => {
                        if let Err(e) = cmd_tx.send(make_pause()) {
                            tracing::error!(error = %e, "cmd_tx 已关闭：渲染线程已退出，暂停命令未送达");
                        }
                        if let Some(raw) = hwnd_raw {
                            // SAFETY: PostMessageW 线程安全，hwnd_raw 来自 play() 设置的有效句柄。
                            unsafe {
                                if PostMessageW(
                                    HWND(raw as *mut _),
                                    wake_msg,
                                    WPARAM(0),
                                    LPARAM(0),
                                )
                                .is_err() {
                                    tracing::warn!("PostMessageW 失败：wake_msg 未送达（窗口可能已销毁）");
                                }
                            }
                        }
                        shared_state
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .state = WallpaperState::Paused;
                        // 通知 Tauri 层 emit wallpaper-state-changed 事件
                        state_changed.notify_state_changed(&display_id);
                    }
                    PauseCommand::Resume => {
                        if let Err(e) = cmd_tx.send(make_resume()) {
                            tracing::error!(error = %e, "cmd_tx 已关闭：渲染线程已退出，恢复命令未送达");
                        }
                        if let Some(raw) = hwnd_raw {
                            // SAFETY: 同上。
                            unsafe {
                                if PostMessageW(
                                    HWND(raw as *mut _),
                                    wake_msg,
                                    WPARAM(0),
                                    LPARAM(0),
                                )
                                .is_err() {
                                    tracing::warn!("PostMessageW 失败：wake_msg 未送达（窗口可能已销毁）");
                                }
                            }
                        }
                        shared_state
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .state = WallpaperState::Playing;
                        // 通知 Tauri 层 emit wallpaper-state-changed 事件
                        state_changed.notify_state_changed(&display_id);
                    }
                    PauseCommand::SetVolume(_) | PauseCommand::ToggleMute => {
                        // 无音频渲染器忽略音量相关命令
                    }
                }
            }
            tracing::debug!("{} pause 线程退出", renderer_name);
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_new_initializes_fields() {
        let base = GdiRendererBase::new(ScalingMode::Fit);
        assert_eq!(base.state(), WallpaperState::Initializing);
        assert!(base.hwnd().is_none());
        assert!(base.thread_handle.is_none());
        assert_eq!(base.scaling_mode(), ScalingMode::Fit);
        assert!(base.pause_sender.is_none());
    }

    #[test]
    fn base_setters_update_fields() {
        let mut base = GdiRendererBase::new(ScalingMode::Fill);
        base.set_state(WallpaperState::Playing);
        assert_eq!(base.state(), WallpaperState::Playing);

        base.set_scaling_mode(ScalingMode::Stretch);
        assert_eq!(base.scaling_mode(), ScalingMode::Stretch);

        base.set_state(WallpaperState::Terminated);
        assert_eq!(base.state(), WallpaperState::Terminated);
    }

    #[test]
    fn base_send_command_without_hwnd_is_ok() {
        // 无 hwnd 时 send_command 仅发送到通道，不应调用 PostMessageW
        let base = GdiRendererBase::new(ScalingMode::Fill);
        let (tx, rx) = std::sync::mpsc::channel();
        let val: i32 = 42;
        base.send_command(&tx, val, WM_USER + 1).unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
    }

    #[test]
    fn base_send_command_disconnected_returns_err() {
        let base = GdiRendererBase::new(ScalingMode::Fill);
        let (tx, rx) = std::sync::mpsc::channel::<i32>();
        drop(rx);
        let result = base.send_command(&tx, 1, WM_USER + 1);
        assert!(result.is_err(), "通道断开应返回错误");
    }

    #[test]
    fn get_initial_window_rect_returns_positive_size() {
        let (x, y, w, h) = get_initial_window_rect();
        assert_eq!(x, 0);
        assert_eq!(y, 0);
        assert!(w > 0, "屏幕宽度应 > 0（或回退到 1920）");
        assert!(h > 0, "屏幕高度应 > 0（或回退到 1080）");
    }

    // ========== W08 修复测试：register_window_class_once 区分逻辑 ==========

    #[test]
    fn classify_register_class_result_success() {
        // W08: RegisterClassW 返回非零 atom 表示注册成功，错误码字段应被忽略
        // （成功时 GetLastError 返回 0 或残留值，不影响分类）
        assert_eq!(
            classify_register_class_result(1, 0),
            RegisterClassOutcome::Success,
            "非零 atom 应判定为 Success"
        );
        assert_eq!(
            classify_register_class_result(0xC000, 0),
            RegisterClassOutcome::Success,
            "任意非零 atom 均应判定为 Success"
        );
        // 即使 GetLastError 残留非零值，成功时也应判定为 Success
        assert_eq!(
            classify_register_class_result(1, 87),
            RegisterClassOutcome::Success,
            "非零 atom 时错误码不应影响 Success 判定"
        );
    }

    #[test]
    fn classify_register_class_result_already_exists() {
        // W08: atom == 0 且 GetLastError == ERROR_CLASS_ALREADY_EXISTS
        // 表示窗口类已注册（幂等正常），不应被误判为真实失败
        assert_eq!(
            classify_register_class_result(0, ERROR_CLASS_ALREADY_EXISTS.0),
            RegisterClassOutcome::AlreadyExists,
            "atom=0 + ERROR_CLASS_ALREADY_EXISTS 应判定为 AlreadyExists（幂等正常）"
        );
    }

    #[test]
    fn classify_register_class_result_real_failure() {
        // W08: atom == 0 且 GetLastError != ERROR_CLASS_ALREADY_EXISTS
        // 表示真实失败（如 wnd_proc 无效、模块句柄错误等），应携带错误码
        // ERROR_INVALID_PARAMETER = 87
        assert_eq!(
            classify_register_class_result(0, 87),
            RegisterClassOutcome::Failed(87),
            "atom=0 + ERROR_INVALID_PARAMETER 应判定为 Failed(87)"
        );
        // ERROR_PROC_NOT_FOUND = 127
        assert_eq!(
            classify_register_class_result(0, 127),
            RegisterClassOutcome::Failed(127),
            "atom=0 + ERROR_PROC_NOT_FOUND 应判定为 Failed(127)"
        );
        // 边界：错误码 0（理论上不应发生——atom==0 但 GetLastError==0），
        // 仍应判定为 Failed(0)，避免静默吞错
        assert_eq!(
            classify_register_class_result(0, 0),
            RegisterClassOutcome::Failed(0),
            "atom=0 + err=0 应判定为 Failed(0)（防御性，不静默吞错）"
        );
    }

    #[test]
    fn classify_register_class_result_already_exists_not_confused_with_failure() {
        // W08: 关键区分——ERROR_CLASS_ALREADY_EXISTS (1411) 不应与任意错误码 1411 混淆
        // 即便错误码数值相同，只有 atom==0 时才进入 AlreadyExists 分支
        assert_eq!(
            classify_register_class_result(1, ERROR_CLASS_ALREADY_EXISTS.0),
            RegisterClassOutcome::Success,
            "atom 非零时即便 err == ERROR_CLASS_ALREADY_EXISTS 也应判定为 Success"
        );
        // 反向：atom==0 + ERROR_CLASS_ALREADY_EXISTS 应判定为 AlreadyExists 而非 Failed
        assert_ne!(
            classify_register_class_result(0, ERROR_CLASS_ALREADY_EXISTS.0),
            RegisterClassOutcome::Failed(ERROR_CLASS_ALREADY_EXISTS.0),
            "ERROR_CLASS_ALREADY_EXISTS 不应被误判为 Failed"
        );
    }
}
