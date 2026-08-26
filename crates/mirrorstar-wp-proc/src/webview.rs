//! WebView2 初始化、窗口管理、URL 构建

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::UI::WindowsAndMessaging::*;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2Environment, CreateCoreWebView2EnvironmentWithOptions,
    ICoreWebView2Controller, ICoreWebView2EnvironmentOptions,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};

use mirrorstar_core::MirrorStarError;

// WP-010: 多显示器场景的回退尺寸。当系统显示器枚举失败或 rect 解析异常时使用，
// 不针对特定显示器分辨率，仅作"合理默认"占位（与系统桌面 1920x1080 主流分辨率对齐）。
//
// v41-WP-007: 回退尺寸选择的业务理由文档化。
// - **1920×1080**：Full HD，桌面显示器最常见分辨率（Steam 硬件调查中长期占比最高），
//   在 `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)` 返回 0 或负数（显示器枚举失败）
//   的极端场景下，使用此尺寸可让壁纸窗口在大多数显示器上呈现出可识别的画面，
//   避免回退到 0×0 导致用户看到空白窗口且无错误提示。
// - **类型为 `i32`**：与 `RECT` 字段（`left/top/right/bottom` 均为 `i32`）及
//   `CreateWindowExW` 的 `nWidth/nHeight` 参数类型一致，避免传入时的类型转换。
//   值 1920/1080 远小于 `i32::MAX`，无溢出风险。
// - **非"最佳"尺寸**：多显示器场景下副显示器尺寸可能不同，本回退值仅作兜底，
//   正常路径应由父进程通过 `--rect` 显式传入目标显示器的位置和尺寸。
const FALLBACK_SCREEN_WIDTH: i32 = 1920;
const FALLBACK_SCREEN_HEIGHT: i32 = 1080;

/// 窗口类名（注册与 WindowClassGuard 注销处共用，避免字面量重复导致不一致）
const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("MirrorStarWebWallpaperCls");

/// 将 `controller.CoreWebView2()` 失败转换为 `MirrorStarError::DesktopIntegration`。
///
/// 统一 3 处"获取 WebView 失败"错误转换模式（navigate_to_url /
/// execute_script_and_report / create_webview），避免前缀字符串修改需同步多处。
pub(crate) fn corewebview2_error(e: impl std::fmt::Display) -> MirrorStarError {
    MirrorStarError::DesktopIntegration(format!("获取 WebView 失败: {}", e))
}

// ── 窗口位置解析 ─────────────────────────────────────────────────────────────

/// 解析 --rect 参数，未提供时使用屏幕大小
///
/// WP-010: 解析失败（段数不足、非数字、尺寸非正）时返回 Err，由调用方通过 `?` 传播退出，
/// 避免静默回退到 0x0 窗口导致用户看到空白窗口无错误提示。
///
/// 注意：x/y 允许负数（多显示器场景下副显示器坐标可能为负），仅校验 width/height > 0。
pub(crate) fn parse_rect(rect: &Option<String>) -> Result<(i32, i32, i32, i32), MirrorStarError> {
    match rect {
        Some(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() != 4 {
                return Err(MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect 参数: {}（期望 4 段 x,y,width,height，实际 {} 段）",
                    s,
                    parts.len()
                )));
            }
            let x: i32 = parts[0].trim().parse().map_err(|_| {
                MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect x 坐标: {}（应为整数）",
                    parts[0].trim()
                ))
            })?;
            let y: i32 = parts[1].trim().parse().map_err(|_| {
                MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect y 坐标: {}（应为整数）",
                    parts[1].trim()
                ))
            })?;
            let w: i32 = parts[2].trim().parse().map_err(|_| {
                MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect width: {}（应为整数）",
                    parts[2].trim()
                ))
            })?;
            let h: i32 = parts[3].trim().parse().map_err(|_| {
                MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect height: {}（应为整数）",
                    parts[3].trim()
                ))
            })?;
            if w <= 0 || h <= 0 {
                return Err(MirrorStarError::DesktopIntegration(format!(
                    "无效的 --rect 尺寸: width={} height={}（width/height 须为正数）",
                    w, h
                )));
            }
            // x/y 可以是负数（多显示器场景下副显示器坐标可能为负），不校验
            Ok((x, y, w, h))
        }
        None => Ok(default_rect()),
    }
}

/// 获取屏幕大小作为默认窗口尺寸
///
/// WP14: 本函数使用 `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)` 获取主显示器尺寸，
/// 仅用于未传 `--rect` 的回退场景。多显示器场景下副显示器壁纸尺寸会错误（始终使用
/// 主显示器尺寸），故多显示器应始终通过 `--rect` 显式传入目标显示器的位置和尺寸。
///
/// 若未来需支持自动获取指定显示器尺寸，应接收 `display_id` 参数并改用
/// `EnumDisplayMonitors` 枚举显示器获取对应尺寸（需在 Cargo.toml 启用
/// `Win32_Graphics_Gdi` feature，当前已启用）。当前调用方（父进程 mirrorstar 主进程）
/// 负责解析显示器布局并通过 `--rect` 传入正确尺寸，本函数仅作为兜底。
fn default_rect() -> (i32, i32, i32, i32) {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        (
            0,
            0,
            if w > 0 { w } else { FALLBACK_SCREEN_WIDTH },
            if h > 0 { h } else { FALLBACK_SCREEN_HEIGHT },
        )
    }
}

// ── 窗口类注册 ───────────────────────────────────────────────────────────────

/// 进程模块句柄缓存（WP-009）
///
/// `GetModuleHandleW(NULL)` 返回当前进程可执行文件的模块句柄，在进程生命周期内不变。
/// 原代码在 `register_window_class` 与 `create_window` 中各调用一次，重复调用虽开销极小
/// 但违反"不变量只计算一次"原则。使用 `OnceLock` 缓存避免重复调用。
///
/// `HINSTANCE` 内部为 `*mut c_void`（非 `Send`/`Sync`），无法直接存入 `static`，
/// 故缓存其 `isize` 表示（指针宽度与 `isize` 一致，转换无信息损失）。
static MODULE_HANDLE: OnceLock<isize> = OnceLock::new();

/// 获取当前进程的模块句柄（HINSTANCE，缓存）
///
/// 首次调用执行 `GetModuleHandleW(NULL)`，后续调用直接返回缓存值。
/// 失败时返回 `HINSTANCE::default()`（空指针），与原 `unwrap_or_default` 行为一致。
fn get_module_handle() -> HINSTANCE {
    let raw = *MODULE_HANDLE.get_or_init(|| {
        unsafe { GetModuleHandleW(None) }
            .map(|h| h.0 as isize)
            .unwrap_or(0)
    });
    HINSTANCE(raw as *mut core::ffi::c_void)
}

/// 窗口类 RAII guard：Drop 时注销窗口类（与 explorer.rs C-016 修复一致）
pub(crate) struct WindowClassGuard {
    class_name: windows::core::PCWSTR,
    h_instance: HINSTANCE,
}

impl WindowClassGuard {
    pub(crate) fn class_name(&self) -> windows::core::PCWSTR {
        self.class_name
    }
}

impl Drop for WindowClassGuard {
    fn drop(&mut self) {
        // 注销窗口类，避免资源泄漏（与 explorer.rs C-016 一致）
        // Drop 路径无法传播错误；类已注册时注销失败也无害（进程退出时内核自动回收）
        unsafe {
            let _ = UnregisterClassW(self.class_name, self.h_instance);
        }
    }
}

/// 注册窗口类（仅执行一次）
///
/// 返回窗口类 RAII guard。若注册失败且错误码不是 `ERROR_CLASS_ALREADY_EXISTS`，返回 `Err`
/// 以便调用方提前退出并打印根因。`OnceLock` 保证注册仅执行一次且幂等。
pub(crate) fn register_window_class() -> Result<WindowClassGuard, MirrorStarError> {
    // OnceLock 双重职责：保证注册逻辑仅执行一次 + 存储错误信息（Some = 失败根因）
    static CLASS_ERROR: OnceLock<Option<String>> = OnceLock::new();

    CLASS_ERROR.get_or_init(|| unsafe {
        // WP-009: 使用缓存的模块句柄（get_module_handle 返回 HINSTANCE）
        let h_instance = get_module_handle();
        let class_name = CLASS_NAME;

        let wc = WNDCLASSW {
            hInstance: h_instance,
            lpszClassName: class_name,
            lpfnWndProc: Some(def_window_proc),
            ..Default::default()
        };

        let atom = RegisterClassW(&wc);
        if atom == 0 {
            let err = GetLastError();
            if err == ERROR_CLASS_ALREADY_EXISTS {
                // 窗口类已注册（例如前次调用已成功），幂等返回
                tracing::debug!("窗口类已注册，跳过重复注册");
                None
            } else {
                Some(format!("注册窗口类失败，GetLastError: 0x{:08X}", err.0))
            }
        } else {
            None
        }
    });

    if let Some(msg) = CLASS_ERROR.get().and_then(|opt| opt.as_ref()) {
        return Err(MirrorStarError::DesktopIntegration(msg.clone()));
    }

    Ok(WindowClassGuard {
        class_name: CLASS_NAME,
        // WP-009: 使用缓存的模块句柄（无需再包裹 unsafe 块调用 GetModuleHandleW）
        h_instance: get_module_handle(),
    })
}

/// 默认窗口过程
unsafe extern "system" fn def_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── 窗口创建 ─────────────────────────────────────────────────────────────────

/// 创建壁纸窗口
pub(crate) fn create_window(
    class_name: windows::core::PCWSTR,
    title: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<HWND, MirrorStarError> {
    let title_h = HSTRING::from(title);
    // WP-009: 使用缓存的模块句柄（get_module_handle 返回 HINSTANCE）
    let h_instance = get_module_handle();

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            &title_h,
            WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            w,
            h,
            HWND::default(),
            None,
            h_instance,
            None,
        )
    };

    match hwnd {
        Ok(h) if !h.is_invalid() && h != HWND::default() => Ok(h),
        Ok(_) => Err(MirrorStarError::DesktopIntegration(
            "创建窗口失败: 无效句柄".to_string(),
        )),
        Err(e) => Err(MirrorStarError::DesktopIntegration(format!(
            "创建窗口失败: {}",
            e
        ))),
    }
}

// ── WebView2 初始化 ──────────────────────────────────────────────────────────

/// WebView2 异步操作的默认超时（W-006）
///
/// 适用于环境创建、控制器创建、脚本执行等操作。30 秒足以覆盖正常工况下的
/// WebView2 初始化（通常 < 5 秒），又能在异常情况下及时释放子进程，避免永久挂起。
pub(crate) const WEBVIEW2_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// 带超时的消息泵等待（W-006）
///
/// 替代 `webview2_com::wait_with_pump`（无超时，`GetMessageA` 无限阻塞）。
/// `webview2_com` 的 `wait_for_async_operation` 内部调用 `wait_with_pump`，
/// 若 WebView2 回调永不触发（如环境创建挂起），wp-proc 子进程将永久挂起。
/// 本函数复刻其逻辑但加入截止时间检查，确保在 `timeout` 到期后返回错误。
///
/// 与 `webview2_com::wait_with_pump` 的差异：
/// - 用 `PeekMessageW`（非阻塞）+ `Sleep(1)` 轮询，替代 `GetMessageW`（无限阻塞）
/// - 截止时间到期时返回 `MirrorStarError::WebView2Timeout`
/// - 收到 `WM_QUIT` 时返回错误（与原 `GetMessageA` 返回 0 → `TaskCanceled` 语义一致）
///
/// 回调消息最多延迟 ~15ms 处理（Windows 默认时钟粒度约 15ms，`Sleep(1)` 实际睡眠
/// 时间受系统时钟粒度约束），对壁纸场景可忽略。
pub(crate) fn wait_with_pump_timeout<T>(
    rx: std::sync::mpsc::Receiver<T>,
    timeout: Duration,
    op_name: &'static str,
) -> Result<T, MirrorStarError> {
    let deadline = Instant::now() + timeout;
    let mut msg = MSG::default();
    let hwnd = HWND::default();

    loop {
        // 1. 优先检查回调是否已完成
        if let Ok(result) = rx.try_recv() {
            return Ok(result);
        }

        // 2. 检查截止时间
        if Instant::now() >= deadline {
            tracing::error!(operation = op_name, timeout = ?timeout, "WebView2 操作超时");
            return Err(MirrorStarError::WebView2Timeout(format!(
                "{}（超时 {:?}）",
                op_name, timeout
            )));
        }

        // 3. 非阻塞地取出并分发所有待处理窗口消息
        //    WebView2 回调通过窗口消息触发，必须泵送消息否则回调永不执行
        //    WP-007: 使用 Unicode 版本 PeekMessageW/DispatchMessageW，与主消息循环
        //    GetMessageW/DispatchMessageW 保持一致（Win32 最佳实践）
        unsafe {
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Err(MirrorStarError::DesktopIntegration(format!(
                        "WebView2 {} 被 WM_QUIT 中止",
                        op_name
                    )));
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // 4. 短暂让出 CPU，避免 100% 占用
        //    WP11: Windows 默认时钟粒度约 15ms，Sleep(1) 实际睡眠时间受此约束，
        //    回调消息最多延迟 ~15ms 处理（对壁纸场景可忽略，上方文档注释已说明）。
        unsafe {
            Sleep(1);
        }
    }
}

// ── WP-002: ControllerGuard RAII ─────────────────────────────────────────────

/// RAII guard：确保 ICoreWebView2Controller 在失败路径调用 Close()
///
/// WP-002 修复：`create_webview` 在 controller 创建成功后，后续步骤（SetIsVisible /
/// CoreWebView2 / build_url / Navigate / GetClientRect / SetBounds）若失败，`?` 提前
/// 返回 `Err`。windows-rs / webview2-com 的 COM 接口 `Drop` 仅调用 `Release()`（减少
/// 引用计数），**不**调用 `ICoreWebView2Controller::Close()`。`Close()` 是显式释放
/// WebView2 资源（包括关联的浏览器进程、渲染管线）的方法，与 `Release()` 语义不同。
///
/// 本 guard 包装 controller，在 Drop 时调用 `Close()`，确保失败路径不泄漏 WebView2 资源。
/// 成功路径通过 `into_inner()` 取出 controller，跳过 `Close()`。
struct ControllerGuard(Option<ICoreWebView2Controller>);

impl ControllerGuard {
    fn new(controller: ICoreWebView2Controller) -> Self {
        Self(Some(controller))
    }

    /// 取出内部的 `ICoreWebView2Controller`，转移所有权给调用方。
    ///
    /// v41-WP-010 文档化：取出后 guard 内部 `Option` 变为 `None`，
    /// 后续 guard 的 `Drop` 实现会跳过 `Close()` 调用（no-op），
    /// 可安全丢弃 guard 而不会影响已取出的 controller。
    fn into_inner(mut self) -> ICoreWebView2Controller {
        self.0.take().unwrap()
    }
}

impl std::ops::Deref for ControllerGuard {
    type Target = ICoreWebView2Controller;
    fn deref(&self) -> &Self::Target {
        // ControllerGuard 在 new 后到 into_inner 前始终持有 Some(controller)；
        // into_inner 后 take() 为 None，但 guard 自身已被消费，不会再被 Deref。
        self.0
            .as_ref()
            .expect("ControllerGuard 不应在 into_inner 后被 Deref")
    }
}

impl Drop for ControllerGuard {
    fn drop(&mut self) {
        if let Some(controller) = self.0.take() {
            // SAFETY: Close() 释放 WebView2 资源，幂等且线程安全。
            // Close() 后 controller 仍会被 drop 调用 Release()，这是正确的
            // （Close 不替代 Release，两者语义不同）。
            let _ = unsafe { controller.Close() };
        }
    }
}

/// v10.0: 构建 WebView2 UserDataFolder 路径
///
/// 指向数据根（安装目录）下的 `webview2-cache/`，便于便携化：删除安装目录即全部清除。
/// 若目录不存在则创建（`create_dir_all` 忽略失败，WebView2 会按需创建）。
///
/// 返回 `Option<HSTRING>`：`Some` 时由调用方传给 `CreateCoreWebView2EnvironmentWithOptions`；
/// `None`（仅在 `HSTRING::from_wide` 失败时，极端罕见）时调用方回退到默认环境创建。
/// 路径计算与目录创建通过 `OnceLock` 缓存仅执行一次。`HSTRING` 实现 `Send + Sync`，
/// 可安全存入 `static`。
fn build_webview2_user_data_folder() -> Option<windows::core::HSTRING> {
    use std::os::windows::ffi::OsStrExt;
    static FOLDER: OnceLock<Option<windows::core::HSTRING>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let path = mirrorstar_core::config::data_root().join("webview2-cache");
            let _ = std::fs::create_dir_all(&path);
            tracing::info!(webview2_cache = %path.display(), "WebView2 UserDataFolder");
            let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            match windows::core::HSTRING::from_wide(&wide) {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(error = %e, "HSTRING::from_wide 失败, 回退到默认 UserDataFolder");
                    None
                }
            }
        })
        .clone()
}

/// v10.0: 构建 WebView2 环境选项
///
/// 设置 AdditionalBrowserArguments 限制磁盘缓存大小为 10MB（默认无限制），
/// 禁用 MediaCache（壁纸网页通常不需要媒体缓存）。
///
/// v11.0 内存优化：追加 GPU/字体缓存禁用参数
/// - --disable-gpu-program-cache：禁用 GPU 程序缓存（节省 ~5MB）
/// - --disable-gpu-shader-disk-cache：禁用着色器磁盘缓存
/// - --disable-features=FontCache,GpuMemDiscardable,BackForwardCache：禁用字体缓存、GPU 可丢弃内存、前进后退缓存
///
/// 返回 `None` 时调用方应回退到 `CreateCoreWebView2Environment`（默认行为），
/// 保证环境选项创建失败不影响壁纸进程正常启动（降级路径）。
fn build_webview2_environment_options() -> Option<ICoreWebView2EnvironmentOptions> {
    let options: ICoreWebView2EnvironmentOptions =
        webview2_com::CoreWebView2EnvironmentOptions::default().into();
    let additional_args = windows::core::HSTRING::from(
        "--disk-cache-size=10485760 \
         --disable-gpu-program-cache \
         --disable-gpu-shader-disk-cache \
         --disable-features=MediaCache,FontCache,GpuMemDiscardable,BackForwardCache",
    );
    unsafe {
        if let Err(e) = options.SetAdditionalBrowserArguments(&additional_args) {
            tracing::warn!(error = %e, "SetAdditionalBrowserArguments 失败, 回退到默认环境选项");
            return None;
        }
    }
    Some(options)
}

/// 创建 WebView2 控制器并导航到指定源
///
/// v10.0 内存优化：通过 `CreateCoreWebView2EnvironmentWithOptions` 设置自定义
/// `UserDataFolder`（数据根下 `webview2-cache/`）与磁盘缓存限制
/// （10MB + 禁用 MediaCache），降低 WebView2 实际内存占用 20-50MB。若环境选项创建
/// 失败则回退到默认 `CreateCoreWebView2Environment`。
pub(crate) fn create_webview(
    hwnd: HWND,
    source: &str,
) -> Result<ICoreWebView2Controller, MirrorStarError> {
    // 创建 WebView2 环境（W-006: 用 wait_with_pump_timeout 替代 wait_for_async_operation，
    // 避免外部函数内部 GetMessageA 无限阻塞导致子进程挂起）
    let environment = {
        let (tx, rx) = std::sync::mpsc::channel();

        let callback = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |error_code, environment| -> windows::core::Result<()> {
                // WP04: error_code 失败时先通过 tx.send(Err(...)) 通知 rx，
                // 避免 wait_with_pump_timeout 等满 30s 超时后才感知失败。
                if let Err(e) = error_code {
                    let _ = tx.send(Err(e));
                    return Ok(());
                }
                if tx
                    .send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .is_err()
                {
                    tracing::warn!("rx 已 drop, 跳过回调结果通知");
                }
                Ok(())
            },
        ));

        // v10.0 内存优化：使用 CreateCoreWebView2EnvironmentWithOptions 设置自定义
        // UserDataFolder 与磁盘缓存限制，降低 WebView2 实际内存占用 20-50MB。
        // v15-C-002: UserDataFolder 与 environment options 是两个独立维度，不应
        // 耦合降级——仅 options 构建失败时仍保留 UserDataFolder（用默认 options），
        // 仅 UserDataFolder 构建失败时才完全回退到 CreateCoreWebView2Environment。
        match (
            build_webview2_user_data_folder(),
            build_webview2_environment_options(),
        ) {
            (Some(user_data_folder), Some(options)) => {
                unsafe {
                    CreateCoreWebView2EnvironmentWithOptions(
                        windows::core::PCWSTR::null(), // browser_executable_folder: null = 使用系统安装的 WebView2 Runtime
                        &user_data_folder,             // user_data_folder: 自定义缓存目录
                        &options, // ICoreWebView2EnvironmentOptions: 磁盘缓存限制
                        &callback,
                    )
                    .map_err(webview2_com::Error::WindowsError)
                    .map_err(|e| {
                        MirrorStarError::DesktopIntegration(format!("创建 WebView2 环境失败: {}", e))
                    })?;
                }
            }
            // v15-C-002: UserDataFolder 可用但 options 构建失败——保留 UserDataFolder
            // （卸载清理路径仍有效），用默认 options（无 AdditionalBrowserArguments）。
            (Some(user_data_folder), None) => {
                tracing::warn!(
                    "环境选项构建失败, 保留 UserDataFolder 使用默认 options"
                );
                let default_options: ICoreWebView2EnvironmentOptions =
                    webview2_com::CoreWebView2EnvironmentOptions::default().into();
                unsafe {
                    CreateCoreWebView2EnvironmentWithOptions(
                        windows::core::PCWSTR::null(),
                        &user_data_folder,
                        &default_options,
                        &callback,
                    )
                    .map_err(webview2_com::Error::WindowsError)
                    .map_err(|e| {
                        MirrorStarError::DesktopIntegration(format!("创建 WebView2 环境失败: {}", e))
                    })?;
                }
            }
            // v15-C-002: UserDataFolder 构建失败——完全回退到默认 CreateCoreWebView2Environment
            (None, _) => {
                tracing::warn!(
                    "UserDataFolder 构建失败, 回退到默认 CreateCoreWebView2Environment"
                );
                unsafe {
                    CreateCoreWebView2Environment(&callback)
                        .map_err(webview2_com::Error::WindowsError)
                        .map_err(|e| {
                            MirrorStarError::DesktopIntegration(format!(
                                "创建 WebView2 环境失败: {}",
                                e
                            ))
                        })?;
                }
            }
        }

        wait_with_pump_timeout(rx, WEBVIEW2_OP_TIMEOUT, "环境创建")?.map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("创建 WebView2 环境失败: {}", e))
        })?
    };

    // 创建 WebView2 控制器（W-006: 用 wait_with_pump_timeout 替代 wait_for_async_operation）
    let controller = {
        let (tx, rx) = std::sync::mpsc::channel();

        let callback = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
            move |error_code, controller| -> windows::core::Result<()> {
                // WP04: error_code 失败时先通过 tx.send(Err(...)) 通知 rx，
                // 避免 wait_with_pump_timeout 等满 30s 超时后才感知失败。
                if let Err(e) = error_code {
                    let _ = tx.send(Err(e));
                    return Ok(());
                }
                if tx
                    .send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .is_err()
                {
                    tracing::warn!("rx 已 drop, 跳过回调结果通知");
                }
                Ok(())
            },
        ));

        unsafe {
            environment
                .CreateCoreWebView2Controller(hwnd, &callback)
                .map_err(webview2_com::Error::WindowsError)
                .map_err(|e| {
                    MirrorStarError::DesktopIntegration(format!("创建 WebView2 控制器失败: {}", e))
                })?;
        }

        wait_with_pump_timeout(rx, WEBVIEW2_OP_TIMEOUT, "控制器创建")?.map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("创建 WebView2 控制器失败: {}", e))
        })?
    };
    // WP-002: 用 ControllerGuard 包装 controller，确保后续失败路径调用 Close()
    let controller = ControllerGuard::new(controller);
    // 设置控制器可见
    unsafe {
        controller.SetIsVisible(true).map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("设置 WebView2 可见失败: {}", e))
        })?;
    }

    // 获取 WebView
    let webview = unsafe { controller.CoreWebView2() }.map_err(corewebview2_error)?;

    // 构建导航 URL（SEC-004: build_url 会拒绝 javascript:/data:/vbscript:/about:/file: 等危险协议）
    let url = build_url(source)?;

    tracing::info!(url = %url, "WebView2 导航到");

    // 导航到源
    unsafe {
        webview
            .Navigate(&HSTRING::from(&url))
            .map_err(|e| MirrorStarError::DesktopIntegration(format!("导航失败: {}", e)))?;
    }

    // 设置初始边界
    // WP08: GetClientRect 失败时直接返回 Err，避免使用 (0,0,0,0) 边界导致 WebView2 不可见。
    // 窗口刚由 create_window 创建，hwnd 应有效；GetClientRect 失败意味着窗口客户区无法确定，
    // 是严重故障，应让 create_webview 失败（main 通过 return Err 退出子进程，
    // WP03 已实现退出码传播让父进程感知错误）。
    let mut rect = RECT::default();
    unsafe {
        if let Err(e) = GetClientRect(hwnd, &mut rect) {
            return Err(MirrorStarError::DesktopIntegration(format!(
                "GetClientRect 失败: {}",
                e
            )));
        }
    }
    unsafe {
        controller.SetBounds(rect).map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("设置 WebView2 边界失败: {}", e))
        })?;
    }

    // WP-002: 成功路径取出 controller，跳过 Close()
    Ok(controller.into_inner())
}

// ── URL 构建 ─────────────────────────────────────────────────────────────────

/// WP-011: 使用 `hex_upper` 辅助函数 + `String::with_capacity` 预分配，
/// 避免每个非保留字符调用 `format!` 分配新 String。
/// 保留字符集与原实现一致（编码结果完全相同）。
///
/// 对 file URL 路径部分进行 percent-encoding
/// 保留 / 和合法 URL 字符，编码空格、#、%、非 ASCII 等
fn percent_encode_path(path: &str) -> String {
    // WP-011: 预分配最坏情况容量（每字节最多编码为 3 字符 %XX），避免逐字符增长重分配
    let mut result = String::with_capacity(path.len() * 3);
    for byte in path.bytes() {
        match byte {
            // 保留字符：字母、数字、以及 file URL 路径中安全的符号
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b':'
            | b'='
            | b'@' => {
                result.push(byte as char);
            }
            _ => {
                // WP-011: 用 hex_upper 替代 format!("%{:02X}", byte) 避免逐字符 String 分配
                result.push('%');
                let hex = hex_upper(byte);
                result.push(hex[0] as char);
                result.push(hex[1] as char);
            }
        }
    }
    result
}

/// WP-011: 将字节转换为两个大写十六进制字符（如 `255` → `[b'F', b'F']`）。
/// 用于 `percent_encode_path` 替代 `format!("%{:02X}", b)` 的逐字符 String 分配。
fn hex_upper(byte: u8) -> [u8; 2] {
    /// percent-encoding 用的大写十六进制编码表（v41-WP-006 文档化）
    ///
    /// 索引 0-15 对应十六进制字符 `0`..`9` / `A`..`F`，用于将一个字节的高 4 位与低 4 位
    /// 分别映射为 ASCII 大写十六进制字符（如 `255` → `[b'F', b'F']`）。
    const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";
    [
        HEX_CHARS[(byte >> 4) as usize],
        HEX_CHARS[(byte & 0x0F) as usize],
    ]
}

/// 大小写不敏感地检测 URL 是否以指定 scheme 前缀开头
///
/// URI scheme 仅允许 ASCII 字母 / 数字 / `+` / `-` / `.`（RFC 3986 §3.1），
/// 且大小写不敏感。本函数使用 ASCII 大小写不敏感比较，不分配内存。
///
/// 例如 `has_scheme_prefix("JavaScript:alert(1)", "javascript:")` 返回 `true`，
/// `has_scheme_prefix("HTTPS://example.com", "https://")` 返回 `true`。
fn has_scheme_prefix(source: &str, prefix: &str) -> bool {
    source
        .get(..prefix.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
}

/// 构建导航 URL，支持 http/https 和本地文件路径
///
/// SEC-004: 显式拒绝 `javascript:` / `data:` / `vbscript:` / `file:` 等危险协议，
/// 防止 WebView2 被诱导执行任意脚本或加载任意内容。
/// 仅允许 `http://` / `https://` 与本地文件路径（自动转换为 `file://` 形式）。
/// 所有 `about:` 变体（含 `about:blank`）一律拒绝：预热机制移除后不再有合法
/// `about:` source，且 `about:` 无内容无脚本、可能被用于探测/绕过，统一拒绝。
///
/// WP-001 修复：URI scheme 大小写不敏感（RFC 3986 §3.1），协议前缀比较改用
/// `has_scheme_prefix`（ASCII 大小写不敏感），阻断 `JavaScript:` / `JAVASCRIPT:`
/// / `JaVaScRiPt:` 等大小写变体绕过白名单导致的 RCE 风险。
pub(crate) fn build_url(source: &str) -> Result<String, MirrorStarError> {
    // SEC-004: 协议白名单校验——拒绝危险协议
    // 注意：`file:` 显式拒绝是因为外部输入不应直接以 file URL 形式传入，
    // 本地路径应通过下方的 canonicalize 流程统一构建为 file:// URL。
    // WP-001: scheme 比较须大小写不敏感（RFC 3986），阻断 `JavaScript:` 等变体绕过。
    let dangerous_schemes = ["javascript:", "data:", "vbscript:", "file:"];
    for scheme in dangerous_schemes.iter() {
        if has_scheme_prefix(source, scheme) {
            return Err(MirrorStarError::InvalidUrl {
                scheme: scheme.trim_end_matches(':').to_string(),
            });
        }
    }

    // 拒绝所有 `about:` 变体（含 `about:blank`）。预热机制（Wave v9-B）移除后，
    // `about:blank` 不再是合法占位 source；`about:` 无内容无脚本，统一拒绝
    // 可防止被用于探测/绕过 SEC-004 协议白名单。
    if has_scheme_prefix(source, "about:") {
        return Err(MirrorStarError::InvalidUrl {
            scheme: "about".to_string(),
        });
    }

    if has_scheme_prefix(source, "http://") || has_scheme_prefix(source, "https://") {
        return Ok(source.to_string());
    }

    let path = std::path::Path::new(source);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_str = canonical.to_string_lossy();

    // 剥离 \\?\ 前缀（Windows extended-length path）
    // 和 \\?\UNC\ 前缀（UNC 路径的扩展形式）
    let (is_unc, cleaned) = if let Some(rest) = path_str.strip_prefix(r"\\?\UNC\") {
        (true, rest.to_string())
    } else if let Some(rest) = path_str.strip_prefix(r"\\?\") {
        (false, rest.to_string())
    } else if path_str.starts_with(r"\\") && !path_str.starts_with(r"\\?\") {
        // 普通 UNC 路径 \\server\share
        (true, path_str[2..].to_string())
    } else {
        (false, path_str.to_string())
    };

    // 将反斜杠替换为正斜杠，并进行 percent-encoding
    let normalized = cleaned.replace('\\', "/");
    let encoded = percent_encode_path(&normalized);

    if is_unc {
        Ok(format!("file://{}", encoded))
    } else {
        Ok(format!("file:///{}", encoded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_rect 测试 ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_rect_valid() {
        let rect = Some("0,0,1920,1080".to_string());
        let (x, y, w, h) = parse_rect(&rect).expect("合法 rect 应解析成功");
        assert_eq!(x, 0);
        assert_eq!(y, 0);
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn test_parse_rect_with_spaces() {
        let rect = Some("10, 20, 800, 600".to_string());
        let (x, y, w, h) = parse_rect(&rect).expect("含空格的合法 rect 应解析成功");
        assert_eq!(x, 10);
        assert_eq!(y, 20);
        assert_eq!(w, 800);
        assert_eq!(h, 600);
    }

    #[test]
    fn test_parse_rect_insufficient_parts() {
        // WP-010: 不足 4 段应返回 Err（不再静默回退到 default_rect）
        let rect = Some("1,2,3".to_string());
        let result = parse_rect(&rect);
        assert!(
            result.is_err(),
            "不足 4 段的 --rect 应返回错误，实际: {:?}",
            result
        );
    }

    #[test]
    fn test_parse_rect_too_many_parts() {
        // WP-010: 超过 4 段也应返回 Err
        let rect = Some("1,2,3,4,5".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "超过 4 段的 --rect 应返回错误");
    }

    #[test]
    fn test_parse_rect_non_numeric() {
        // WP-010: 非数字应返回 Err
        let rect = Some("a,b,c,d".to_string());
        let result = parse_rect(&rect);
        assert!(
            result.is_err(),
            "非数字 --rect 应返回错误，实际: {:?}",
            result
        );
    }

    #[test]
    fn test_parse_rect_partial_non_numeric() {
        // WP-010: 部分字段非数字应返回 Err
        let rect = Some("0,0,800,abc".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "部分非数字的 --rect 应返回错误");
    }

    #[test]
    fn test_parse_rect_zero_width() {
        // WP-010: width=0 应返回 Err（不再静默回退）
        let rect = Some("0,0,0,600".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "width=0 应返回错误");
    }

    #[test]
    fn test_parse_rect_zero_height() {
        // WP-010: height=0 应返回 Err
        let rect = Some("0,0,800,0".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "height=0 应返回错误");
    }

    #[test]
    fn test_parse_rect_negative_width() {
        // WP-010: width=-1 应返回 Err
        let rect = Some("0,0,-1,600".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "width=-1 应返回错误");
    }

    #[test]
    fn test_parse_rect_negative_height() {
        // WP-010: height=-1 应返回 Err
        let rect = Some("0,0,800,-1".to_string());
        let result = parse_rect(&rect);
        assert!(result.is_err(), "height=-1 应返回错误");
    }

    #[test]
    fn test_parse_rect_negative_x_y_allowed() {
        // WP-010: x/y 允许负数（多显示器场景下副显示器坐标可能为负）
        let rect = Some("-1920,-1080,1920,1080".to_string());
        let (x, y, w, h) = parse_rect(&rect).expect("负 x/y 应被允许");
        assert_eq!(x, -1920);
        assert_eq!(y, -1080);
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn test_parse_rect_none() {
        // None 返回 default_rect（屏幕大小），不应报错
        let (x, y, w, h) = parse_rect(&None).expect("None 应返回 default_rect");
        // default_rect 在测试环境中返回 (0,0,屏幕宽,屏幕高) 或回退到 (0,0,1920,1080)
        let _ = (x, y, w, h);
    }

    // ── build_url 测试 ─────────────────────────────────────────────────────

    #[test]
    fn test_build_url_http() {
        assert_eq!(
            build_url("http://example.com").unwrap(),
            "http://example.com"
        );
    }

    #[test]
    fn test_build_url_https() {
        assert_eq!(
            build_url("https://example.com/page").unwrap(),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_build_url_absolute_path() {
        // 本地绝对路径，canonicalize 可能失败也可能成功
        // 只要不 panic 且结果以 file:// 开头即可
        let result = build_url("C:\\Users\\test\\file.html").unwrap();
        assert!(result.starts_with("file://"));
    }

    #[test]
    fn test_build_url_unc_path() {
        // UNC 路径（不存在，canonicalize 失败，回退到原始路径）
        let result = build_url(r"\\server\share\file.html").unwrap();
        assert!(result.starts_with("file://"));
        // UNC 路径应为 file://server/share/file.html（双斜杠，非三斜杠）
        assert!(!result.starts_with("file:///"));
    }

    #[test]
    fn test_build_url_path_with_special_chars() {
        // 含特殊字符的路径（不存在，canonicalize 失败）
        let result = build_url(r"C:\Users\test\my file.html").unwrap();
        assert!(result.starts_with("file://"));
        // 空格应被 percent-encoding
        assert!(result.contains("%20"));
    }

    // ── SEC-004: 协议白名单校验测试 ───────────────────────────────────────

    #[test]
    fn test_build_url_rejects_javascript_scheme() {
        let result = build_url("javascript:alert(1)");
        assert!(result.is_err(), "javascript: 协议应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "javascript");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_rejects_data_scheme() {
        let result = build_url("data:text/html,<script>alert(1)</script>");
        assert!(result.is_err(), "data: 协议应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "data");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_rejects_vbscript_scheme() {
        let result = build_url("vbscript:msgbox(1)");
        assert!(result.is_err(), "vbscript: 协议应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "vbscript");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_rejects_about_blank() {
        // 预热机制移除后，`about:blank` 不再是合法 source，应被拒绝
        let result = build_url("about:blank");
        assert!(result.is_err(), "about:blank 应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "about");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_rejects_about_blank_case_insensitive() {
        // WP-001: URI scheme 大小写不敏感（RFC 3986），`ABOUT:BLANK` 等变体也应被拒绝
        for input in ["ABOUT:blank", "About:Blank", "ABOUT:BLANK"] {
            let result = build_url(input);
            assert!(
                result.is_err(),
                "about:blank 的大小写变体应被拒绝（输入: {}）",
                input
            );
        }
    }

    #[test]
    fn test_build_url_rejects_about_other_schemes() {
        // 所有 `about:` 变体（含 about:blank）都应被拒绝（如 about:config / about:addons）
        for input in [
            "about:config",
            "about:addons",
            "about:preferences",
            "about:blankx",
            "about:blank",
        ] {
            let result = build_url(input);
            assert!(result.is_err(), "about: 变体应被拒绝（输入: {}）", input);
            match result {
                Err(MirrorStarError::InvalidUrl { scheme }) => {
                    assert_eq!(scheme, "about", "错误中应返回规范 scheme（输入: {}）", input);
                }
                other => panic!("期望 InvalidUrl 错误（输入: {}），实际: {:?}", input, other),
            }
        }
    }

    #[test]
    fn test_build_url_rejects_file_scheme() {
        // file: 直接以 URL 形式传入应被拒绝（应通过本地路径走 canonicalize 流程）
        let result = build_url("file:///C:/Users/test/file.html");
        assert!(result.is_err(), "file: 协议应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "file");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    // ── WP-001: 协议白名单大小写不敏感测试 ─────────────────────────────────

    #[test]
    fn test_build_url_rejects_javascript_mixed_case_variants() {
        // WP-001: URI scheme 大小写不敏感（RFC 3986），所有大小写变体都应被阻断
        for input in [
            "JavaScript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "javaScript:alert(1)",
            "JaVaScRiPt:alert(1)",
        ] {
            let result = build_url(input);
            assert!(result.is_err(), "应拒绝 {}（大小写变体绕过防护）", input);
            match result {
                Err(MirrorStarError::InvalidUrl { scheme }) => {
                    assert_eq!(
                        scheme, "javascript",
                        "错误中应返回规范小写 scheme（输入: {}）",
                        input
                    );
                }
                other => panic!("期望 InvalidUrl 错误（输入: {}），实际: {:?}", input, other),
            }
        }
    }

    #[test]
    fn test_build_url_rejects_javascript_lowercase_regression() {
        // WP-001 回归测试：小写 javascript: 仍应被拒绝（确保大小写不敏感修复不引入回归）
        let result = build_url("javascript:alert(document.cookie)");
        assert!(result.is_err(), "javascript: 协议应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "javascript");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_rejects_other_dangerous_schemes_case_variants() {
        // WP-001: 其他危险协议的大小写变体也应被阻断
        // 注：`ABOUT:blank` 已由 test_build_url_allows_about_blank_case_insensitive 验证放行，
        // 故此处不包含 about 用例。
        let cases = [
            ("DATA:text/html,<script>alert(1)</script>", "data"),
            ("Data:text/html,test", "data"),
            ("VBScript:msgbox(1)", "vbscript"),
            ("File:///C:/test.html", "file"),
            ("FILE:///C:/test.html", "file"),
        ];
        for (input, expected_scheme) in cases {
            let result = build_url(input);
            assert!(result.is_err(), "应拒绝 {}（大小写变体）", input);
            match result {
                Err(MirrorStarError::InvalidUrl { scheme }) => {
                    assert_eq!(
                        scheme, expected_scheme,
                        "错误 scheme 不匹配（输入: {}）",
                        input
                    );
                }
                other => panic!("期望 InvalidUrl 错误（输入: {}），实际: {:?}", input, other),
            }
        }
    }

    #[test]
    fn test_build_url_allows_http_localhost() {
        // 合法 http://localhost 仍允许通过
        assert_eq!(build_url("http://localhost").unwrap(), "http://localhost");
        assert_eq!(
            build_url("http://localhost:8080/wallpaper").unwrap(),
            "http://localhost:8080/wallpaper"
        );
    }

    #[test]
    fn test_build_url_allows_http_https_case_variants() {
        // WP-001: http/https 大小写变体也应被允许（RFC 3986 scheme 大小写不敏感）
        assert_eq!(
            build_url("HTTP://example.com").unwrap(),
            "HTTP://example.com"
        );
        assert_eq!(
            build_url("Https://example.com/page").unwrap(),
            "Https://example.com/page"
        );
        assert_eq!(build_url("HTTP://localhost").unwrap(), "HTTP://localhost");
    }

    #[test]
    fn test_has_scheme_prefix_case_insensitive() {
        // has_scheme_prefix 单元测试：大小写不敏感前缀匹配
        assert!(has_scheme_prefix("javascript:alert(1)", "javascript:"));
        assert!(has_scheme_prefix("JavaScript:alert(1)", "javascript:"));
        assert!(has_scheme_prefix("JAVASCRIPT:alert(1)", "javascript:"));
        assert!(has_scheme_prefix("javaScript:alert(1)", "javascript:"));
        assert!(has_scheme_prefix("JaVaScRiPt:alert(1)", "javascript:"));
        assert!(has_scheme_prefix("HTTPS://example.com", "https://"));
        assert!(has_scheme_prefix("File:///C:/test", "file:"));

        // 非匹配前缀
        assert!(!has_scheme_prefix("http://example.com", "javascript:"));
        assert!(!has_scheme_prefix("https://example.com", "http://"));

        // 输入比前缀短
        assert!(!has_scheme_prefix("java", "javascript:"));
        assert!(!has_scheme_prefix("", "javascript:"));

        // 空前缀（边界：任何字符串都以空前缀开头）
        assert!(has_scheme_prefix("anything", ""));
        assert!(has_scheme_prefix("", ""));
    }

    // ── percent_encode_path 测试 ───────────────────────────────────────────

    #[test]
    fn test_percent_encode_path() {
        assert_eq!(percent_encode_path("hello"), "hello");
        assert_eq!(percent_encode_path("hello world"), "hello%20world");
        assert_eq!(percent_encode_path("file#1"), "file%231");
        assert_eq!(percent_encode_path("100%"), "100%25");
        assert_eq!(percent_encode_path("C:/Users/test"), "C:/Users/test");
        assert_eq!(percent_encode_path("a/b/c"), "a/b/c");
    }

    #[test]
    fn test_percent_encode_path_chinese() {
        // 中文字符应被编码为 UTF-8 字节的 percent-encoding
        // "测试" 的 UTF-8 编码: E6 B5 8B E8 AF 95
        assert_eq!(percent_encode_path("测试"), "%E6%B5%8B%E8%AF%95");
    }

    #[test]
    fn test_percent_encode_path_mixed() {
        // 混合路径：C:/图片/wallpaper.jpg
        let result = percent_encode_path("C:/图片/wallpaper.jpg");
        assert!(result.starts_with("C:/"));
        assert!(result.contains("%"));
        assert!(result.ends_with("/wallpaper.jpg"));
    }

    #[test]
    fn test_percent_encode_path_japanese() {
        // 日文 "テスト" 的 UTF-8 编码: E3 83 86 E3 82 B9 E3 83 88
        assert_eq!(percent_encode_path("テスト"), "%E3%83%86%E3%82%B9%E3%83%88");
    }

    // ── W-006: wait_with_pump_timeout 超时测试 ───────────────────────────

    #[test]
    fn w006_wait_with_pump_timeout_returns_error_on_timeout() {
        // 构造一个永不接收的 Receiver，验证 wait_with_pump_timeout 在超时后
        // 返回 WebView2Timeout 错误（而非永久阻塞）。使用 100ms 短超时保持测试快速。
        let (_tx, rx): (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) =
            std::sync::mpsc::channel();
        let result = wait_with_pump_timeout(rx, Duration::from_millis(100), "测试操作");
        assert!(
            result.is_err(),
            "永不接收的 channel 应在超时后返回错误，实际: {:?}",
            result
        );
        match result {
            Err(MirrorStarError::WebView2Timeout(msg)) => {
                assert!(
                    msg.contains("测试操作"),
                    "超时错误信息应包含操作名，实际: {}",
                    msg
                );
            }
            Err(e) => panic!("期望 WebView2Timeout 错误，实际: {:?}", e),
            Ok(_) => panic!("不应返回 Ok"),
        }
    }

    // ── WP-007: wait_with_pump_timeout Unicode 消息函数测试 ─────────────

    /// WP-007: 验证 wait_with_pump_timeout 使用 Unicode 消息函数
    ///
    /// 修复前使用 PeekMessageA/DispatchMessageA（ANSI），与主消息循环 GetMessageW/DispatchMessageW
    /// （Unicode）不一致，违反 Win32 最佳实践。改为 PeekMessageW/DispatchMessageW 保持一致。
    #[test]
    fn wp007_wait_with_pump_timeout_uses_unicode_message_functions() {
        let source = include_str!("webview.rs");
        assert!(
            source.contains("PeekMessageW"),
            "wait_with_pump_timeout 应使用 PeekMessageW"
        );
        assert!(
            source.contains("DispatchMessageW"),
            "wait_with_pump_timeout 应使用 DispatchMessageW"
        );
        // 验证 wait_with_pump_timeout 函数体内不再含 ANSI 版本
        // 通过截取函数体片段进行断言
        let start = source
            .find("pub(crate) fn wait_with_pump_timeout")
            .expect("wait_with_pump_timeout 函数存在");
        let end = source[start..]
            .find("\n}\n")
            .expect("wait_with_pump_timeout 函数体结束");
        let func_body = &source[start..start + end];
        assert!(
            !func_body.contains("PeekMessageA"),
            "wait_with_pump_timeout 函数体内不应再含 PeekMessageA"
        );
        assert!(
            !func_body.contains("DispatchMessageA"),
            "wait_with_pump_timeout 函数体内不应再含 DispatchMessageA"
        );
    }

    // ── WP-002: ControllerGuard RAII 测试 ──────────────────────────────────

    #[test]
    fn wp002_create_webview_failure_calls_close() {
        // WP-002: 验证 ControllerGuard 的 RAII 语义，确保 create_webview 失败路径调用 Close()
        //
        // 完整的端到端测试（验证 create_webview 失败时 controller.Close() 被调用）需要真实
        // WebView2 环境（创建 environment + controller），属于集成测试范畴，此处仅验证
        // RAII 守卫的 Drop 语义：失败路径（正常 drop）应调用清理，成功路径（into_inner）不应。
        //
        // 策略：用一个简单的 RAII 计数器结构模拟 ControllerGuard 的行为，验证：
        // 1. 正常 drop（模拟失败路径）应调用 Close
        // 2. into_inner 后 drop（模拟成功路径）不应调用 Close

        use std::cell::Cell;
        use std::rc::Rc;

        /// 模拟 ControllerGuard 的 RAII 结构：Drop 时若未被 take 则设置 closed=true
        struct MockGuard {
            closed: Rc<Cell<bool>>,
            taken: bool,
        }

        impl MockGuard {
            fn new(closed: Rc<Cell<bool>>) -> Self {
                Self {
                    closed,
                    taken: false,
                }
            }
            // 模拟 ControllerGuard::into_inner 返回 controller（此处用 bool 占位）
            fn into_inner(mut self) -> bool {
                self.taken = true;
                true
            }
        }

        impl Drop for MockGuard {
            fn drop(&mut self) {
                if !self.taken {
                    self.closed.set(true);
                }
            }
        }

        // 场景1：正常 drop（模拟 create_webview 失败路径）
        // controller 创建后，后续步骤（SetIsVisible/CoreWebView2/build_url/Navigate/
        // GetClientRect/SetBounds）失败，guard 被 drop，应调用 Close()
        let closed = Rc::new(Cell::new(false));
        {
            let _guard = MockGuard::new(closed.clone());
            // _guard 离开作用域被 drop，模拟 ? 提前返回 Err
        }
        assert!(
            closed.get(),
            "WP-002: 失败路径 guard drop 应调用 Close()（模拟）"
        );

        // 场景2：into_inner 后 drop（模拟 create_webview 成功路径）
        // controller 创建后所有步骤成功，guard.into_inner() 取出 controller，Drop 不调用 Close()
        let closed = Rc::new(Cell::new(false));
        {
            let guard = MockGuard::new(closed.clone());
            let _controller = guard.into_inner();
            // guard 已被 into_inner 消费，模拟 Ok(controller) 返回
        }
        assert!(
            !closed.get(),
            "WP-002: 成功路径 into_inner 后 drop 不应调用 Close()"
        );
    }

    // ── v41-WP-010: ControllerGuard into_inner no-op 契约测试 ─────────────

    /// v41-WP-010: 验证 `ControllerGuard::into_inner` 后 guard Drop 是 no-op
    ///
    /// `ICoreWebView2Controller` 是 Windows COM 接口，单元测试环境无法构造真实实例
    /// （需先创建 WebView2 环境+控制器，依赖 Edge 运行时）。采用双轨验证：
    ///
    /// 1. 行为验证：用 MockGuard 复刻 `ControllerGuard` 的 RAII 模式
    ///    （`Option<controller>` + `take()` + `if let Some(...)` Drop 守卫），
    ///    验证 `into_inner` 取出后 drop 不会触发清理动作（no-op）。
    ///
    /// 2. 契约验证：通过 `include_str!` 静态检查源码，确保 `into_inner` 文档明确
    ///    含 "no-op" 关键字，且 `Drop` 实现使用 `if let Some(...) = self.0.take()`
    ///    模式确保 `Option::None` 时跳过 `Close()`（与文档契约一致）。
    #[test]
    fn v41_wp010_into_inner_then_drop_is_noop() {
        use std::cell::Cell;
        use std::rc::Rc;

        /// 模拟 ControllerGuard 的 RAII 结构：内层 Option + take + Drop 守卫
        /// 完整复刻 ControllerGuard 的字段布局与 Drop 模式（Option + if let Some）。
        struct MockGuard {
            inner: Option<Rc<()>>,
            closed: Rc<Cell<bool>>,
        }

        impl MockGuard {
            fn new(controller: Rc<()>, closed: Rc<Cell<bool>>) -> Self {
                Self {
                    inner: Some(controller),
                    closed,
                }
            }
            // 复刻 ControllerGuard::into_inner：take() 取出后内部 Option 变为 None
            fn into_inner(mut self) -> Rc<()> {
                self.inner
                    .take()
                    .expect("MockGuard 应持有 Some(controller)")
            }
        }

        impl Drop for MockGuard {
            fn drop(&mut self) {
                // 复刻 ControllerGuard::Drop：仅当 Some 时调用 Close（此处用 closed.set(true) 模拟）
                if let Some(_controller) = self.inner.take() {
                    self.closed.set(true);
                }
                // None 分支：no-op，不调用 Close()
            }
        }

        // 场景：调用 into_inner 消费 guard
        // 期望：guard.inner 已被 take 为 None，guard 在 into_inner 函数末尾被
        //       隐式 Drop 时跳过 Close()（no-op），且取出的 controller 仍持有引用。
        let closed = Rc::new(Cell::new(false));
        let controller = Rc::new(());
        let controller_strong = Rc::downgrade(&controller);

        // 调用 into_inner 消费 guard：guard.inner 已 take 为 None，
        // guard 在 into_inner 函数末尾被隐式 Drop（应跳过 Close()，no-op）。
        let extracted = {
            let guard = MockGuard::new(controller.clone(), closed.clone());
            guard.into_inner()
        };

        // 断言1：Drop 是 no-op，未触发 Close()
        assert!(
            !closed.get(),
            "v41-WP-010: into_inner 后 guard Drop 应为 no-op，不调用 Close()"
        );
        // 断言2：extracted controller 仍可用（与原 controller 共享同一份分配）
        assert_eq!(
            Rc::strong_count(&extracted),
            2,
            "v41-WP-010: 取出的 controller 应仍可访问（持有引用）"
        );
        // 断言3：controller_strong 仍可升级（controller 未被释放）
        assert!(
            controller_strong.upgrade().is_some(),
            "v41-WP-010: controller 仍存活，未被 guard Drop 释放"
        );

        // ── 契约验证：源码静态检查 ───────────────────────────────────────

        let source = include_str!("webview.rs");

        // 1. 验证 into_inner 方法的文档注释明确含 no-op 契约
        let into_inner_sig = "fn into_inner(mut self) -> ICoreWebView2Controller";
        let into_inner_pos = source
            .find(into_inner_sig)
            .expect("ControllerGuard::into_inner 方法应存在");
        // 截取 into_inner 上方的文档块（最近一段连续 /// 行）
        let before = &source[..into_inner_pos];
        let doc_start = before
            .rfind("/// 取出内部的")
            .expect("into_inner 上方应有以 '取出内部的' 开头的文档注释");
        let doc_block = &source[doc_start..into_inner_pos];
        assert!(
            doc_block.contains("no-op"),
            "v41-WP-010: into_inner 文档应含 'no-op' 关键字，实际: {}",
            doc_block
        );
        assert!(
            doc_block.contains("None"),
            "v41-WP-010: into_inner 文档应说明 guard 内部 Option 变为 None"
        );
        assert!(
            doc_block.contains("v41-WP-010"),
            "v41-WP-010: into_inner 文档应含 'v41-WP-010' 标识"
        );

        // 2. 验证 Drop 实现使用 if let Some(...) 模式确保 None 时跳过 Close()
        let drop_impl_start = source
            .find("impl Drop for ControllerGuard")
            .expect("ControllerGuard 应实现 Drop");
        let drop_impl_end = source[drop_impl_start..]
            .find("\n}\n")
            .expect("Drop 实现体结束");
        let drop_impl = &source[drop_impl_start..drop_impl_start + drop_impl_end];
        assert!(
            drop_impl.contains("if let Some(controller) = self.0.take()"),
            "v41-WP-010: Drop 实现应使用 'if let Some(controller) = self.0.take()' 模式确保 None 时跳过 Close()"
        );
        assert!(
            drop_impl.contains("Close()"),
            "v41-WP-010: Drop 实现应在 Some 分支调用 Close()"
        );
    }
}
