//! WebView2 初始化、窗口管理、URL 构建

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{
    GetLastError, BOOL, ERROR_CLASS_ALREADY_EXISTS, E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT,
    RECT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::System::WinRT::EventRegistrationToken;
use windows::Win32::UI::WindowsAndMessaging::*;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2Environment, CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2,
    ICoreWebView2Controller, ICoreWebView2Controller2, ICoreWebView2EnvironmentOptions,
    ICoreWebView2NewWindowRequestedEventArgs, ICoreWebView2ProcessFailedEventArgs,
    ICoreWebView2Settings3, COREWEBVIEW2_COLOR, COREWEBVIEW2_PROCESS_FAILED_KIND,
    COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
};
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NewWindowRequestedEventHandler, ProcessFailedEventHandler,
};

use mirrorstar_core::MirrorStarError;

// WP-010: 多显示器场景的回退尺寸。仅在未传 `--rect`（`None`）时使用；
// 若 `--rect` 已提供但解析失败，`parse_rect` 返回 Err 而不再回退到本尺寸。
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

/// 远程域名白名单（SEC-005，默认空 = 拒绝所有远程 http/https）。
///
/// 说明：Web 壁纸的源由用户显式添加，`build_url` 已通过协议白名单（仅允许
/// http/https/本地路径）限定加载范围。此处提供**可配置的**远程域名白名单作为纵深防御：
/// - 空列表（默认）→ **拒绝所有远程 http/https**（默认收紧）：仅放行
///   `localhost` / `127.0.0.1`（本机回环调试，无 SSRF 风险）。远程壁纸源需在
///   `ALLOWED_REMOTE_DOMAINS` 中显式加入其域名，避免壁纸源意外指向任意远程内容。
/// - 非空时 → 仅允许列表内域名（及始终允许 localhost/127.0.0.1 本地调试）。
///
/// 命中白名单之外的 http/https 宿主时，`build_url` 返回 `InvalidUrl`
/// （scheme="remote-domain-not-allowed:<host>"），阻止 WebView2 加载任意远程内容
/// （防 SSRF / 防诱导加载）。空表收紧是默认安全策略（fail-closed），
/// 而非"空表不限制"（fail-open）——后者已被 SEC-005 第二轮正确性优化移除。
const ALLOWED_REMOTE_DOMAINS: &[&str] = &[];

/// 注入 CSP 用的 JS 脚本（插入 <meta http-equiv="Content-Security-Policy">）。
///
/// 时序约束（实测）：`AddScriptToExecuteOnDocumentCreated` 在**文档创建阶段**执行，
/// 此时 `document.head` 与 `document.documentElement` **均为 null**（两者都由 HTML
/// 解析器在解析过程中创建）。因此：
/// - 原实现 `if (document.head) { ... }` 的守卫恒为假 → CSP 从未插入
///   （实测 `document.querySelector('meta[http-equiv="Content-Security-Policy"]')` 为 null）。
/// - 直接在此刻 `document.write('<meta ...>')` 虽可让解析器插入 meta，但实测会导致
///   Play 命令等待的 `NavigationCompleted` 永不触发（Web 壁纸无法加载），故不可用。
///
/// 采用方案：文档创建时先尝试一次（head 已存在的少数场景），否则挂 `DOMContentLoaded`
/// 监听，待解析出 `<head>` 后再插入 meta。已实测本 WebView2 会强制**脚本插入**的
/// CSP meta（插入 `script-src 'none'` 的 meta 后，随后插入的内联 `<script>` 被拦截）。
///
/// **作用域限定（R2）**：`AddScriptToExecuteOnDocumentCreated` 对**每个 document**
/// （含 iframe 子 frame）都执行本脚本。若不加限定，嵌入壁纸的第三方内容也会被插入
/// `object-src/base-uri/form-action 'none'`，破坏其 `<base href>` 相对 URL 解析、
/// 表单提交与 `<object>`/`<embed>` 加载——而加固的本意只是加固**壁纸顶层文档本身**。
/// 故脚本首行以 `window.top !== window.self` 提前返回，仅顶层文档注入。
///
/// **幂等守卫（R3）**：守卫必须基于**自有标记** `meta[data-mirrorstar-csp]`，而非
/// `meta[http-equiv="Content-Security-Policy"]`。后者判断的是**页面可控的 DOM**：
/// 若壁纸 HTML 自带一条精确大小写的同名 meta，原守卫会直接 `return`，导致应用侧
/// CSP 从未插入（多条 CSP 取交集 → 页面自己的策略生效），本轮加固被页面绕过。
///
/// 局限（如实说明）：CSP 在 `DOMContentLoaded` 时才生效，晚于页面自身的解析期脚本，
/// 因此它**不能**约束页面首屏的解析期资源；它约束的是此后发生的 `base-uri` 变更、
/// `form-action` 提交与 `object-src` 加载。若需在解析前即强制 CSP，须改用
/// `WebResourceRequested` 注入响应头方案（本轮未实施）。
const INJECT_CSP_SCRIPT: &str = r#"
(function () {
  // R2: 仅顶层文档注入——本脚本对每个 document（含 iframe 子 frame）都会执行，
  // 若向嵌入内容也注入 base-uri/form-action/object-src 'none'，会破坏其相对 URL
  // 解析（<base href>）、表单提交与 <object>/<embed> 加载。
  if (window.top !== window.self) return;
  var csp = "object-src 'none'; base-uri 'none'; form-action 'none'";
  function inject() {
    // R3: 幂等守卫用自有标记，而非页面可控的 http-equiv 查询——否则页面自带的同名
    // meta 会让应用侧 CSP 被跳过（加固被绕过）。每个文档最多插入一次。
    if (document.querySelector('meta[data-mirrorstar-csp]')) return;
    var root = document.head || document.documentElement;
    if (!root) return;
    var m = document.createElement('meta');
    m.setAttribute('http-equiv', 'Content-Security-Policy');
    m.setAttribute('data-mirrorstar-csp', '1');
    m.setAttribute('content', csp);
    root.appendChild(m);
  }
  inject();
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', inject, { once: true });
  } else {
    inject();
  }
})();
"#;

/// 将 `controller.CoreWebView2()` 失败转换为 `MirrorStarError::DesktopIntegration`。
///
/// 统一 3 处"获取 WebView 失败"错误转换模式（navigate_to_url /
/// execute_script_and_report / create_webview），避免前缀字符串修改需同步多处。
pub(crate) fn corewebview2_error(e: impl std::fmt::Display) -> MirrorStarError {
    MirrorStarError::DesktopIntegration(format!("获取 WebView 失败: {}", e))
}

/// 将 WebView2 环境创建失败转换为 `MirrorStarError::DesktopIntegration`。
///
/// 统一 `create_webview` 中 3 处"创建 WebView2 环境失败"错误转换模式
/// （UserDataFolder + options / UserDataFolder + 默认 options / 完全回退默认环境），
/// 避免前缀字符串修改需同步多处。
fn env_create_error(e: webview2_com::Error) -> MirrorStarError {
    MirrorStarError::DesktopIntegration(format!("创建 WebView2 环境失败: {}", e))
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
            // 透明壁纸（防白闪）：宿主窗口启用 WS_EX_LAYERED，配合下方
            // SetLayeredWindowAttributes + 透明的 WebView2 DefaultBackgroundColor，
            // 使未渲染区域以透明（而非白底）呈现，避免导航完成前的白闪。
            WINDOW_EX_STYLE(WS_EX_LAYERED.0),
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

    let hwnd = match hwnd {
        Ok(h) if !h.is_invalid() && h != HWND::default() => h,
        Ok(_) => {
            return Err(MirrorStarError::DesktopIntegration(
                "创建窗口失败: 无效句柄".to_string(),
            ))
        }
        Err(e) => {
            return Err(MirrorStarError::DesktopIntegration(format!(
                "创建窗口失败: {}",
                e
            )))
        }
    };

    // 透明壁纸：将分层窗口配置为按像素 alpha 合成（全局不透明度 100%），
    // 透明区域由 WebView2 的 per-pixel alpha（DefaultBackgroundColor alpha=0）提供。
    // 刻意不使用 LWA_COLORKEY 透明色键——色键会把 WebView 内容中同色像素也变透明，
    // 导致壁纸画面缺失。LWA_ALPHA(255) 仅指定合成规则，不遮挡内容。
    unsafe {
        if SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            255,
            LWA_ALPHA,
        )
        .is_err()
        {
            tracing::warn!("SetLayeredWindowAttributes 失败，将退回不透明窗口");
        }
    }

    Ok(hwnd)
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
/// 指向数据根（安装目录）下的 `webview2-cache/<pid>/`，便于便携化：删除安装目录即全部清除。
///
/// 多显示器修复：每个 wp-proc 实例使用**独立的**子目录（以本进程 PID 命名）。
/// WebView2 不允许两个进程同时使用同一 user data folder 创建环境（第二个会失败），
/// 若所有实例共用 `webview2-cache/`，per_monitor 模式下同时开启两个 Web 壁纸时，
/// 第二个实例的环境创建会失败，该显示器 Web 壁纸不可用。以 PID 作为子目录名即可
/// 让每个实例互不干扰。
///
/// 子目录名安全性：PID 仅由十进制数字构成（`[0-9]`），天然满足文件名安全要求，
/// 无需再做字符替换（管道名等含反斜杠/冒号的标识才需要转义，此处未采用）。
///
/// 若目录不存在则创建（`create_dir_all` 忽略失败，WebView2 会按需创建）。
///
/// 路径由 `mirrorstar_core::wallpaper::web::wp_proc_user_data_folder` 单源提供——父进程
/// 退出后回收该目录时用的是同一个函数，避免两处拼接漂移（见该函数文档）。
///
/// 返回 `Option<HSTRING>`：`Some` 时由调用方传给 `CreateCoreWebView2EnvironmentWithOptions`；
/// `None`（仅在 `HSTRING::from_wide` 失败时，极端罕见）时调用方回退到默认环境创建。
/// 路径计算与目录创建通过 `OnceLock` 缓存仅执行一次。`HSTRING` 实现 `Send + Sync`，
/// 可安全存入 `static`。
///
/// # Task 5.2 评估结论：**放弃**「主窗口与网页壁纸共享 WebView2 环境/浏览器进程」
///
/// 目标：让主窗口（Tauri/wry 托管的 WebView2）与 wp-proc 共用同一 UDF + 环境选项，
/// 以省下一份 msedgewebview2 进程基线（约 5-6 进程 / 150-300MB）。
///
/// **核实（WebView2 官方文档 Microsoft Learn）**：跨进程共享在模型上**是被支持的**——
/// 「A WebView2 control shares its WebView2 session with any other WebView2 control
/// that uses the same UDF. This is true whether the WebView2 controls are in the same
/// host app or different host apps.」（Manage user data folders），且默认
/// `ExclusiveUserDataFolderAccess=false` 不禁止其他进程加入同一 UDF
/// （ICoreWebView2EnvironmentOptions2）。因此本目录采用 per-PID UDF **并非**因为
/// 「WebView2 不允许共享」，而是**主动选择隔离**。故放弃共享是架构取舍，不是技术不可行。
///
/// **放弃理由（均为架构级回退，收益不抵代价）**：
/// 1. **崩溃隔离回退（决定性）**：wp-proc 存在的首要理由就是崩溃隔离（见
///    `mirrorstar-core/src/wallpaper/web.rs` 中 `WebRenderer` 的文档"实现崩溃隔离和内存优化"）。
///    壁纸页面是不受信任的第三方网页（fail-closed 沙箱面），是崩溃概率最高的一方；
///    共享 browser 进程后，其 browser/GPU 进程崩溃会一并拖垮主窗口设置 UI。
/// 2. **多显示器能力回退（决定性）**：per-PID UDF 让每个 wp-proc 成为独立进程集合，
///    多显示器可同时开多个 Web 壁纸、可各自独立 terminate/重启；共享单一 UDF 后所有
///    实例落进同一 browser 进程集合，且 `cleanup_wp_proc_user_data_folder` 会在实例退出
///    时 `remove_dir_all` 整个 UDF——若该 UDF 正被主窗口/其他实例使用，会破坏仍在运行的
///    环境（文档明确 UDF 只能在无活动 session 时删除/移动）。
/// 3. **环境选项无法对齐（历史理由，现已不适用）**：共享要求"同一 UDF + 等价环境配置"，
///    该约束此前指向与主窗口 WebView2 环境的对齐；但主窗口现已迁移为原生 Slint 子进程
///    （`crates/mirrorstar-ui`，进程内无 WebView2），不再存在可比对的主窗口 WebView2 环境，
///    此关切随之失效。历史上主窗口由 Tauri/wry 托管（`src-tauri/src/state.rs` 的
///    `WebviewWindowBuilder`，未指定 UDF 与 AdditionalBrowserArguments），wp-proc 无法控制
///    其 UDF 与参数；此外壁纸侧仍需 `--autoplay-policy=no-user-gesture-required`、进程级
///    `WEBVIEW2_DEFAULT_BACKGROUND_COLOR` 透明注入、CDP 注入等配置，不应与主窗口混用。
/// 4. **净收益不确定**：即便省下一份基线，也需把壁纸移入主进程或引入集中式 UDF 生命周期
///    管理 + 全参数对齐，复杂度与风险远超预期收益。
///
/// **若未来仍要共享，前置条件**：
/// - 把网页壁纸移入主进程，在同一 `ICoreWebView2Environment` 下为每个显示器创建独立
///   `CoreWebView2Controller`（挂到各显示器 WorkerW 子窗口）；
/// - UDF 生命周期集中管理，禁止单个 renderer 退出即删除 UDF；
/// - 接受崩溃隔离降级，并依赖 `ProcessFailed` 事件重建挂掉的壁纸 controller；
/// - 用 Task 1 的 MEM-ATTRIB 埋点对比总 `msedgewebview2.exe` 进程数与工作集，确认收益后再决策。
fn build_webview2_user_data_folder() -> Option<windows::core::HSTRING> {
    use std::os::windows::ffi::OsStrExt;
    static FOLDER: OnceLock<Option<windows::core::HSTRING>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            // 每个 wp-proc 实例独立的子目录（PID 唯一且为纯数字，文件名安全）。
            let path = mirrorstar_core::wallpaper::web::wp_proc_user_data_folder(std::process::id());
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
/// Task 5.1 内存裁剪（与主窗口裁剪同方向，保守低风险）：
/// - --renderer-process-limit=1：wp-proc 只渲染**一个**页面，限制渲染进程数为 1，
///   省去多余 renderer 进程的内存基线。风险低：跨域 iframe 会与主 frame 共用同一
///   renderer（牺牲站点隔离、单个 renderer 内存略升），但本进程是 fail-closed 沙箱面
///   （无导航/无用户输入/远程域名白名单收紧），该隔离损失可接受。
/// - --js-flags=--max-old-space-size=512：把 V8 老生代堆上限压到 512MB（64 位默认约
///   2-4GB）。**风险**：过小会让复杂网页壁纸（重 WebGL/长动画）OOM 或白屏，故取
///   保守的 512MB（普通网页堆通常 <100MB，重负载壁纸也极少超此值），仍能约束失控增长。
///   WebGL 纹理等不占 V8 堆，512MB 对图形型壁纸足够安全。
/// - 未追加其他 `--disable-features` 项：现有列表已覆盖可确认有效的缓存类特性，
///   其余特性 flag 未经实测，可能破坏渲染且收益不可测，故不引入。
///
/// E2E-02 音频自动播放修复：追加 `--autoplay-policy=no-user-gesture-required`。
///
/// 根因（模拟人类操作测试实证）：wp-proc 的 WebView2 实际生效的是严格的
/// `user-gesture-required` 自动播放策略——即使元素 `muted`，页面加载期 play()
/// 也返回 `NotAllowedError`（"play() failed because the user didn't interact with
/// the document first."）；宿主 ExecuteScript 注入 play() 同样被拒；唯一解锁途径是
/// 真实用户手势（CDP 合成点击验证 `navigator.userActivation.hasBeenActive` 由 false
/// 变 true 后 play() 成功）。而壁纸窗口是点击穿透（WorkerW 之上、不接收任何输入），
/// 永远不会获得用户激活 → 带背景音频的 Web 壁纸音频永远无法开始，音量/静音控制
/// 形同虚设。原"默认策略允许 muted 自动播放"的假设经实测为误。
///
/// 方案权衡：微软官方不建议在生产应用使用浏览器 flag（可能随时变更），但本场景
/// 壁纸 WebView2 是 fail-closed 沙箱面（无导航/无用户输入/远程域名白名单收紧），
/// `no-user-gesture-required` 是该受控面下恢复"壁纸环境音频自动播放"语义的必要手段，
/// 也是实证后唯一可靠的方案（宿主注入 play 被同策略拦截，无法绕过）。
/// 其余自动播放策略值（user-gesture-required / document-user-activation-required）
/// 均无法满足壁纸免手势播放需求。
///
/// 返回 `None` 时调用方应回退到 `CreateCoreWebView2Environment`（默认行为），
/// 保证环境选项创建失败不影响壁纸进程正常启动（降级路径）。
/// wp-proc WebView2 的 AdditionalBrowserArguments 参数串（**纯函数，便于单测**）。
///
/// 从 `build_webview2_environment_options` 中拆分出来：本函数不触碰 COM 对象，
/// 因此可在单测里直接断言"必须保留 / 必须存在 / 不得引入"的参数契约，防止后续
/// 维护误删 `--autoplay-policy=no-user-gesture-required`（E2E 实测必需）或改动
/// 内存裁剪参数。参数用途与风险见 `build_webview2_environment_options` 的文档注释。
fn build_additional_browser_args() -> &'static str {
    "--autoplay-policy=no-user-gesture-required \
     --disk-cache-size=10485760 \
     --disable-gpu-program-cache \
     --disable-gpu-shader-disk-cache \
     --renderer-process-limit=1 \
     --js-flags=--max-old-space-size=512 \
     --disable-features=MediaCache,FontCache,GpuMemDiscardable,BackForwardCache"
}

fn build_webview2_environment_options() -> Option<ICoreWebView2EnvironmentOptions> {
    let options: ICoreWebView2EnvironmentOptions =
        webview2_com::CoreWebView2EnvironmentOptions::default().into();
    let additional_args = windows::core::HSTRING::from(build_additional_browser_args());
    unsafe {
        if let Err(e) = options.SetAdditionalBrowserArguments(&additional_args) {
            tracing::warn!(error = %e, "SetAdditionalBrowserArguments 失败, 回退到默认环境选项");
            return None;
        }
    }
    Some(options)
}

/// 将 WebView2 控制器设为透明背景（防白闪，H3 兜底）
///
/// 通过 `ICoreWebView2Controller2::SetDefaultBackgroundColor` 把默认背景色 alpha 置 0，
/// 使页面在导航/首帧渲染前以透明呈现（宿主窗口已启用 WS_EX_LAYERED，见 create_window），
/// 避免加载期间的白底闪烁。
///
/// H3 优化后：首帧透明已由环境变量 `WEBVIEW2_DEFAULT_BACKGROUND_COLOR=00ffffff`
/// （create_webview 中环境创建前设置）根治，本函数降级为**兜底**：覆盖环境变量
/// 被外部覆盖/未生效的场景，以及运行时动态调整默认背景色的需求。
///
/// WebView2 仅支持 alpha 为 0 或 255，透明即 alpha=0。
fn apply_transparent_controller(controller: &ICoreWebView2Controller) {
    let controller2 = match controller.cast::<ICoreWebView2Controller2>() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cast 到 ICoreWebView2Controller2 失败，跳过透明背景设置");
            return;
        }
    };
    let color = COREWEBVIEW2_COLOR {
        A: 0,
        R: 0,
        G: 0,
        B: 0,
    };
    unsafe {
        if let Err(e) = controller2.SetDefaultBackgroundColor(color) {
            tracing::warn!(error = %e, "SetDefaultBackgroundColor 失败，可能残留白底");
        }
    }
}

// 网页音视频自动播放策略（E2E-02 修复后）
//
// 早期注释假设"WebView2 默认 autoplay 策略允许 muted 媒体免手势自动播放"，该假设
// 已被模拟人类操作测试**证伪**：wp-proc 的 WebView2 实际生效的是严格
// `user-gesture-required` 策略，muted 元素冷启动 play() 仍返回 NotAllowedError。
// 因此现在统一通过 `build_webview2_environment_options()` 注入
// `--autoplay-policy=no-user-gesture-required`（见其上方的 E2E-02 根因分析），
// 恢复"壁纸环境音频自动播放"语义。壁纸页面内自动播放/暂停仍由
// `command.rs` 的 Pause/Resume JS 注入控制。

/// 硬化 WebView2 安全配置（SEC-005）：防止用户手势从壁纸逃逸/加载任意内容。
///
/// 全部为**尽力而为**（每次调用均记录日志，单个失败不阻断壁纸创建），
/// 避免安全配置异常时导致壁纸无法加载。包含：
/// 1. 禁用右键上下文菜单（防止"在新标签页打开图像/查看源代码"等逃逸）
/// 2. 禁用状态栏（隐藏 URL，避免诱导点击）
/// 3. 禁用浏览器加速键（如 F3 查找、Ctrl+R 刷新等全局快捷键）
/// 4. 禁用内置错误页（隐藏敏感错误信息，避免误导）
/// 5. 阻止新窗口请求（`window.open` / `target=_blank` 被 SetHandled 拦截）
/// 6. 注入最小化 CSP（`object-src`/`base-uri`/`form-action` 收紧逃逸与 elevation 面）。
///    经 `AddScriptToExecuteOnDocumentCreated` 注入，实际在 `DOMContentLoaded` 时插入
///    meta（文档创建阶段 `head` 尚不存在，详见 `INJECT_CSP_SCRIPT` 的时序说明）。
///    **仅作用于壁纸顶层文档**：该注入 API 对每个 document（含 iframe 子 frame）都执行，
///    脚本内以 `window.top !== window.self` 提前返回，避免破坏嵌入内容的 `<base href>`
///    解析/表单提交/`<object>` 加载（R2）；幂等守卫用自有标记 `data-mirrorstar-csp`，
///    避免页面自带的同名 CSP meta 让应用侧注入被跳过（R3）。
fn harden_webview(webview: &ICoreWebView2) {
    // 1. ICoreWebView2Settings：禁用上下文菜单 / 状态栏 / 内置错误页。
    //    Settings() 失败时仅记录日志，不影响下方新窗口拦截与 CSP 注入。
    match unsafe { webview.Settings() } {
        Ok(settings) => {
            unsafe {
                if let Err(e) = settings.SetAreDefaultContextMenusEnabled(BOOL(0)) {
                    tracing::warn!(error = %e, "禁用右键菜单失败");
                }
            }
            unsafe {
                if let Err(e) = settings.SetIsStatusBarEnabled(BOOL(0)) {
                    tracing::warn!(error = %e, "禁用状态栏失败");
                }
            }
            unsafe {
                if let Err(e) = settings.SetIsBuiltInErrorPageEnabled(BOOL(0)) {
                    tracing::warn!(error = %e, "禁用内置错误页失败");
                }
            }

            // 2. 禁用浏览器加速键（ICoreWebView2Settings3，须 cast）。
            if let Ok(settings3) = settings.cast::<ICoreWebView2Settings3>() {
                unsafe {
                    if let Err(e) = settings3.SetAreBrowserAcceleratorKeysEnabled(BOOL(0)) {
                        tracing::warn!(error = %e, "禁用浏览器加速键失败");
                    }
                }
            } else {
                tracing::warn!("cast 到 ICoreWebView2Settings3 失败，跳过加速键禁用");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "获取 Settings 失败，跳过上下文菜单/状态栏/加速键禁用");
        }
    }

    // 3. 阻止新窗口逃逸：任何 NewWindowRequested 一律 SetHandled(true)。
    let handler = NewWindowRequestedEventHandler::create(Box::new(
        |_sender,
         args: Option<ICoreWebView2NewWindowRequestedEventArgs>|
         -> windows::core::Result<()> {
            if let Some(args) = args {
                // SAFETY: SetHandled(true) 告知 WebView2 该新窗口请求已被处理，
                // WebView2 不会创建新窗口，达到阻止从壁纸弹出独立窗口逃逸的目的。
                unsafe {
                    let _ = args.SetHandled(BOOL(1));
                }
            }
            Ok(())
        },
    ));
    let mut token = EventRegistrationToken::default();
    unsafe {
        if let Err(e) = webview.add_NewWindowRequested(&handler, &mut token) {
            tracing::warn!(error = %e, "注册 NewWindowRequested 拦截失败");
        }
    }

    // 4. 注入最小化 CSP。AddScriptToExecuteOnDocumentCreated 在每次文档创建时注入脚本；
    //    脚本在 head 就绪（DOMContentLoaded）后插入 CSP meta——文档创建阶段 head 尚不存在，
    //    且**仅顶层文档生效**（脚本内 `window.top !== window.self` 提前返回，R2），
    //    详见 INJECT_CSP_SCRIPT 的时序、作用域与局限说明（尽力而为，失败不阻断）。
    let csp_handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
        |_error_code: windows::core::Result<()>, _id: String| -> windows::core::Result<()> {
            Ok(())
        },
    ));
    unsafe {
        if let Err(e) = webview
            .AddScriptToExecuteOnDocumentCreated(&HSTRING::from(INJECT_CSP_SCRIPT), &csp_handler)
        {
            tracing::warn!(error = %e, "注入 CSP 脚本失败");
        }
    }
}

/// H2: 渲染进程崩溃自愈——注册 `ICoreWebView2::ProcessFailed` 事件。
///
/// - `RENDER_PROCESS_EXITED`（渲染进程已退出，可自愈）：调用 `Reload()` 重新加载页面。
///   带防抖与预算限制，避免渲染进程反复崩溃时 Reload 死循环：
///   - 两次重载之间最小间隔 `RELOAD_MIN_INTERVAL`（5s），窗口内连续崩溃只重载一次
///   - 预算窗口 `RELOAD_BUDGET_WINDOW`（30s）内最多重载 `RELOAD_MAX_BUDGET`（3）次，
///     超出后放弃自愈并 `PostQuitMessage(0)` 退出子进程
/// - `BROWSER_PROCESS_EXITED`（浏览器进程已退出，不可自愈）：直接 `PostQuitMessage(0)`
///   退出子进程，由父进程既有进程退出监听（spawn_proc_exit_monitor，state != Terminated
///   即上报异常）兜底通知 UI，无需新增 IPC。
/// - 其他进程失败类型（GPU/utility/frame 等）：由 WebView2 自行处理，仅记录日志。
///
/// 防抖状态用 `Cell` 存储：COM 事件回调为 `FnMut`（无 Send bound），始终在
/// 拥有 WebView2 的线程上串行触发，无需跨线程同步。
fn install_crash_recovery(webview: &ICoreWebView2) {
    // 防抖常量：Reload 最小间隔与预算窗口内的最大重载次数。
    const RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(5);
    const RELOAD_MAX_BUDGET: u32 = 3;
    const RELOAD_BUDGET_WINDOW: Duration = Duration::from_secs(30);

    // 防抖状态：最近一次 Reload 时间、预算窗口内的重载次数、预算耗尽标记。
    let last_reload = Cell::new(None::<Instant>);
    let reload_count = Cell::new(0u32);
    let reload_disabled = Cell::new(false);
    // 闭包内需调用 Reload，捕获 webview 的独立引用计数（COM AddRef），
    // 事件处理器存活期间 webview 不会被释放。原参数引用仍用于下方注册事件。
    let webview_owned = webview.clone();

    let handler = ProcessFailedEventHandler::create(Box::new(
        move |_sender,
              args: Option<ICoreWebView2ProcessFailedEventArgs>|
              -> windows::core::Result<()> {
            let Some(args) = args else {
                tracing::warn!("ProcessFailed 事件参数为空，忽略");
                return Ok(());
            };
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            if let Err(e) = unsafe { args.ProcessFailedKind(&mut kind) } {
                tracing::warn!(error = %e, "获取 ProcessFailedKind 失败，忽略");
                return Ok(());
            }
            match kind {
                COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
                    // 渲染进程已退出：Reload 自愈（带防抖/预算）
                    let now = Instant::now();
                    if reload_disabled.get() {
                        tracing::error!("渲染进程频繁崩溃（预算已耗尽），退出子进程交由父进程兜底");
                        unsafe { PostQuitMessage(0) };
                        return Ok(());
                    }
                    // 距上次重载超过预算窗口 → 重置计数（重新开窗）
                    if last_reload
                        .get()
                        .is_some_and(|t| now.duration_since(t) >= RELOAD_BUDGET_WINDOW)
                    {
                        reload_count.set(0);
                    }
                    // 距上次重载不足最小间隔 → 崩溃过于频繁，跳过 Reload（防抖）
                    if last_reload
                        .get()
                        .is_some_and(|t| now.duration_since(t) < RELOAD_MIN_INTERVAL)
                    {
                        tracing::warn!("渲染进程短时间内再次崩溃，跳过 Reload（防抖）");
                        return Ok(());
                    }
                    if reload_count.get() >= RELOAD_MAX_BUDGET {
                        reload_disabled.set(true);
                        tracing::error!("渲染进程反复崩溃（预算耗尽），放弃 Reload，退出子进程");
                        unsafe { PostQuitMessage(0) };
                        return Ok(());
                    }
                    reload_count.set(reload_count.get() + 1);
                    last_reload.set(Some(now));
                    tracing::warn!(
                        count = reload_count.get(),
                        "渲染进程已退出，Reload 重新加载页面自愈"
                    );
                    // Reload 失败（如浏览器进程也已退出）→ 退出子进程让父进程兜底
                    // 闭包捕获的 webview_owned 为独立 COM 引用计数（'static），非参数借用
                    if let Err(e) = unsafe { webview_owned.Reload() } {
                        tracing::error!(error = %e, "Reload 失败，退出子进程交由父进程兜底");
                        unsafe { PostQuitMessage(0) };
                    }
                }
                COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
                    // 浏览器进程已退出：不可自愈，退出子进程，父进程退出监听会上报
                    tracing::error!("浏览器进程已退出，退出子进程交由父进程兜底");
                    unsafe { PostQuitMessage(0) };
                }
                _ => {
                    tracing::warn!(kind = ?kind.0, "其他进程失败类型，由 WebView2 自行处理");
                }
            }
            Ok(())
        },
    ));
    let mut token = EventRegistrationToken::default();
    unsafe {
        if let Err(e) = webview.add_ProcessFailed(&handler, &mut token) {
            tracing::warn!(error = %e, "注册 ProcessFailed 崩溃自愈事件失败");
        }
    }
}

/// 创建 WebView2 控制器（不执行导航，导航由 Play 命令驱动）
///
/// E2E-01 竞态修复：create_webview 只负责环境/控制器/透明/安全加固等初始化，
/// 不再做预热导航——导航唯一入口是父进程的 Play 命令，从根源消除双重导航竞态
/// （预热导航 + Play 重复 Navigate → 第二次取消进行中导航 → NavigationCompleted
/// 误报 IsSuccess=false）。此处仍调用 build_url 做同步校验，无效源可快速失败。
///
/// v10.0 内存优化：通过 `CreateCoreWebView2EnvironmentWithOptions` 设置自定义
/// `UserDataFolder`（数据根下 `webview2-cache/<pid>/`，每实例独立子目录以支持多显示器）
/// 与磁盘缓存限制（10MB + 禁用 MediaCache），降低 WebView2 实际内存占用 20-50MB。
/// 若环境选项创建失败则回退到默认 `CreateCoreWebView2Environment`。
pub(crate) fn create_webview(
    hwnd: HWND,
    source: &str,
    cdp_port: Option<u16>,
) -> Result<ICoreWebView2Controller, MirrorStarError> {
    // H3: 首帧透明根治——在创建 WebView2 环境之前设置 WEBVIEW2_DEFAULT_BACKGROUND_COLOR
    // 环境变量（AARRGGBB 格式，前 2 位 alpha=00 = 全透明），使环境从创建起即透明，
    // 消除导航/首帧渲染前的白底闪烁（比 controller 创建后 SetDefaultBackgroundColor
    // 更早生效）。仅在子进程内设置（std::env 仅影响本进程，不影响父进程）。
    // 保留下方 apply_transparent_controller（SetDefaultBackgroundColor）作为兜底，
    // 覆盖环境变量被外部覆盖/未生效的场景。
    std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "00ffffff");

    // CDP 观察通道（仅测试）：传 --cdp-port 时在 WebView2 环境创建前注入
    // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS，使浏览器进程监听 --remote-debugging-port，
    // 供模拟人类操作测试做 DOM 断言。保留环境中已有参数（合并而非覆盖）。
    if let Some(port) = cdp_port {
        let existing = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
        let arg = format!("--remote-debugging-port={port}");
        let merged = if existing.is_empty() {
            arg
        } else {
            format!("{existing} {arg}")
        };
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", merged);
    }

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
                    .map_err(env_create_error)?;
                }
            }
            // v15-C-002: UserDataFolder 可用但 options 构建失败——保留 UserDataFolder
            // （卸载清理路径仍有效），用默认 options（无 AdditionalBrowserArguments）。
            (Some(user_data_folder), None) => {
                tracing::warn!("环境选项构建失败, 保留 UserDataFolder 使用默认 options");
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
                    .map_err(env_create_error)?;
                }
            }
            // v15-C-002: UserDataFolder 构建失败——完全回退到默认 CreateCoreWebView2Environment
            (None, _) => {
                tracing::warn!("UserDataFolder 构建失败, 回退到默认 CreateCoreWebView2Environment");
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

    // 透明背景（防白闪）：在首次 Navigate 前设置。仅尝试，失败不阻断壁纸加载。
    apply_transparent_controller(&controller);

    // 获取 WebView
    let webview = unsafe { controller.CoreWebView2() }.map_err(corewebview2_error)?;

    // 安全加固（SEC-005）：禁右键/状态栏/加速键/内置错误页/新窗口逃逸 + 注入最小化 CSP。
    // 全部尽力而为，失败仅 warn，不中断导航。
    harden_webview(&webview);

    // H2: 渲染进程崩溃自愈——注册 ProcessFailed 事件（尽力而为，失败仅 warn，不中断导航）。
    install_crash_recovery(&webview);

    // 构建导航 URL（SEC-004: build_url 会拒绝 javascript:/data:/vbscript:/about:/file: 等危险协议）。
    // E2E-01 竞态修复：此处仅做 URL 校验，不再执行预热导航。
    //
    // 根因：父进程 Play 命令到达前 create_webview 已 Navigate 一次（预热），Play 再 Navigate
    // 同一 URL 时第二次 Navigate 会取消进行中的首次导航，导致 NavigationCompleted 以
    // IsSuccess=false 回调，Play 命令误判加载失败，父进程随之终止子进程。
    //
    // 优雅方案（单一导航来源）：删除预热导航，让 Play 命令成为唯一导航入口——创建 WebView2
    // 控制器（含环境/控制器/透明/安全加固）与导航解耦，导航时机完全由父进程 Play 命令掌控，
    // 从根源消除双重导航竞态，同时保留 build_url 的同步校验（无效源仍可快速失败）。
    let _url = build_url(source)?;

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

/// 从 http/https URL 提取主机名（不含端口）。
///
/// 简单解析（不依赖外部 URL 库，避免引入新依赖）：
/// 1. 去 scheme（`"://"` 之前的部分）
/// 2. 截止到首个 `/` / `?` / `#`（路径、查询、片段分隔符）
/// 3. 处理主机部分：
///    - 以 `[` 开头 → IPv6 字面量，取方括号内内容（保留 scope id，如 `[fe80::1%3]`
///      → `fe80::1%3`；`[::1]` → `::1`），并忽略 `]` 之后可能的端口
///    - 否则 → 按首个 `:` 截止（剥离端口，兼容 IPv4 / 域名）
///
/// 仅用于远程域名白名单比对（SEC-005）。local 路径/非法输入返回 `None`。
fn extract_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // 截止到首个路径/查询/片段分隔符（主机部分之后不允许再出现这些字符）
    let host_part = match rest.find(['/', '?', '#']) {
        Some(idx) => &rest[..idx],
        None => rest,
    };
    // IPv6 字面量 `[addr]` / `[addr%scope]`：取方括号内内容（保留 scope id），
    // 并忽略 `]` 之后的 `:port`。非方括号形式按首个 `:` 截止剥离端口。
    let host = if let Some(inner) = host_part.strip_prefix('[') {
        match inner.split_once(']') {
            Some((addr, _)) => addr,
            None => inner, // 防御：未闭合的 `[`，按剩余内容处理
        }
    } else {
        host_part.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 判断远程域名是否在白名单内（或本身是本地调试域名），白名单由参数显式传入。
///
/// SEC-005：空白名单 → **拒绝所有远程域名**（默认收紧，fail-closed）；非空时仅放行
/// 白名单内域名，且始终放行 `localhost` / `127.0.0.1`（本机回环调试，无 SSRF 风险）。
/// 大小写不敏感（主机名不区分大小写）。解析不出主机名（无 scheme / 非法输入）时
/// **拒绝**（fail-closed），由 `build_url` 返回明确错误。
fn is_remote_domain_allowed_with(whitelist: &[&str], url: &str) -> bool {
    let host = match extract_host(url) {
        Some(h) => h.to_ascii_lowercase(),
        // 解析不出主机名时拒绝（fail-closed）：build_url 仅对 http/https 调用本函数，
        // 解析失败意味着输入异常，不应放行
        None => return false,
    };
    if host == "localhost" || host == "127.0.0.1" {
        return true;
    }
    whitelist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&host))
}

/// 使用默认白名单 `ALLOWED_REMOTE_DOMAINS` 判断远程域名是否允许。
fn is_remote_domain_allowed(url: &str) -> bool {
    is_remote_domain_allowed_with(ALLOWED_REMOTE_DOMAINS, url)
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
        // SEC-005: 远程域名白名单（纵深防御）。默认空表 = 拒绝所有远程 http/https
        // （fail-closed，仅放行 localhost/127.0.0.1），与既有"空表不限制"语义相反：
        // 远程壁纸源必须在 ALLOWED_REMOTE_DOMAINS 中显式加入其域名。
        if !is_remote_domain_allowed(source) {
            let host = extract_host(source).unwrap_or("");
            return Err(MirrorStarError::InvalidUrl {
                scheme: format!("remote-domain-not-allowed:{}", host),
            });
        }
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

    // ── Task 5.1: AdditionalBrowserArguments 参数契约 ──────────────────────

    /// Task 5.1：参数串必须保留 E2E 实测必需项，并包含本任务新增的内存裁剪项。
    ///
    /// 该测试锁定"必须保留"与"必须存在"两集合，防止后续维护误删
    /// `--autoplay-policy=no-user-gesture-required`（壁纸点击穿透、永远拿不到用户手势，
    /// 去掉会导致带音频网页壁纸无法自动播放）或回退内存裁剪参数。
    #[test]
    fn task5_1_additional_browser_args_contract() {
        let args = build_additional_browser_args();

        // E2E-02 必需项（保留原样）
        assert!(
            args.contains("--autoplay-policy=no-user-gesture-required"),
            "必须保留 autoplay-policy=no-user-gesture-required，实际: {args}"
        );
        // 既有磁盘缓存限制与 GPU 缓存裁剪（不得回退）
        assert!(
            args.contains("--disk-cache-size=10485760"),
            "应保留磁盘缓存限制，实际: {args}"
        );
        assert!(args.contains("--disable-gpu-program-cache"));
        assert!(args.contains("--disable-gpu-shader-disk-cache"));
        assert!(args.contains(
            "--disable-features=MediaCache,FontCache,GpuMemDiscardable,BackForwardCache"
        ));
        // Task 5.1 新增项
        assert!(
            args.contains("--renderer-process-limit=1"),
            "应限制渲染进程数为 1，实际: {args}"
        );
        assert!(
            args.contains("--js-flags=--max-old-space-size=512"),
            "应限制 V8 老生代堆上限为保守值 512MB，实际: {args}"
        );
        // 负向约束：不得引入会破坏透明首帧/WebGL 壁纸的激进开关
        assert!(
            !args.contains("--disable-gpu "),
            "不得整块禁用 GPU（会破坏 WebGL 壁纸与透明首帧）"
        );
        assert!(
            !args.contains("--disable-javascript"),
            "不得禁用 JavaScript（网页壁纸依赖 JS 渲染）"
        );
    }

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
        // H1 收紧后：远程域名默认不在白名单，应返回 Err（scheme=remote-domain-not-allowed:<host>）
        let result = build_url("http://example.com");
        assert!(result.is_err(), "远程 http 默认应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "remote-domain-not-allowed:example.com");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_build_url_https() {
        // H1 收紧后：远程域名默认不在白名单，应返回 Err（scheme=remote-domain-not-allowed:<host>）
        let result = build_url("https://example.com/page");
        assert!(result.is_err(), "远程 https 默认应被拒绝");
        match result {
            Err(MirrorStarError::InvalidUrl { scheme }) => {
                assert_eq!(scheme, "remote-domain-not-allowed:example.com");
            }
            other => panic!("期望 InvalidUrl 错误，实际: {:?}", other),
        }
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
                    assert_eq!(
                        scheme, "about",
                        "错误中应返回规范 scheme（输入: {}）",
                        input
                    );
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
        // 注：`ABOUT:blank` 已由 test_build_url_rejects_about_blank_case_insensitive 验证拒绝，
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
        // WP-001: http/https 大小写变体也应被允许（RFC 3986 scheme 大小写不敏感）。
        // H1 收紧后：远程域名默认拒绝（大小写变体同样拒绝），仅 localhost 大小写变体放行。
        for input in ["HTTP://example.com", "Https://example.com/page"] {
            let result = build_url(input);
            assert!(
                result.is_err(),
                "远程 http/https 大小写变体默认应被拒绝（输入: {}）",
                input
            );
        }
        // localhost 大小写变体仍放行（始终允许本机回环）
        assert_eq!(build_url("HTTP://localhost").unwrap(), "HTTP://localhost");
        assert_eq!(
            build_url("HTTPS://LOCALHOST:8443/w").unwrap(),
            "HTTPS://LOCALHOST:8443/w"
        );
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

    // ── SEC-005: 远程域名白名单 helper 测试 ─────────────────────────────
    // 注：ALLOWED_REMOTE_DOMAINS 默认为空表 = 拒绝所有远程 http/https（fail-closed，
    // 仅放行 localhost/127.0.0.1）。白名单命中/不命中/大小写通过显式传参的
    // is_remote_domain_allowed_with 测试（不依赖全局常量）。

    #[test]
    fn sec005_extract_host_basic() {
        assert_eq!(
            extract_host("https://example.com/page"),
            Some("example.com")
        );
        assert_eq!(
            extract_host("http://example.com:8080/p"),
            Some("example.com")
        );
        assert_eq!(extract_host("https://localhost:8080"), Some("localhost"));
        assert_eq!(
            extract_host("https://sub.example.com/"),
            Some("sub.example.com")
        );
    }

    // ── M4: extract_host IPv6 / 端口 / 路径增强测试 ─────────────────────

    #[test]
    fn m4_extract_host_ipv6_literal() {
        // IPv6 字面量 `[::1]`：取方括号内内容，不含端口
        assert_eq!(extract_host("http://[::1]:8080/w"), Some("::1"));
        assert_eq!(extract_host("https://[::1]/"), Some("::1"));
    }

    #[test]
    fn m4_extract_host_ipv6_scope_id() {
        // IPv6 scope id（`%3` 等）是地址的一部分，应保留
        assert_eq!(extract_host("http://[fe80::1%3]/w"), Some("fe80::1%3"));
        assert_eq!(
            extract_host("https://[fe80::1%25eth0]:443/x"),
            Some("fe80::1%25eth0")
        );
    }

    #[test]
    fn m4_extract_host_port_path_query_cutoff() {
        // 端口剥离（IPv4 与域名）
        assert_eq!(extract_host("http://example.com:8080"), Some("example.com"));
        // 路径/查询/片段截止
        assert_eq!(
            extract_host("http://example.com/a/b?x=1#frag"),
            Some("example.com")
        );
        assert_eq!(extract_host("http://example.com?x=1"), Some("example.com"));
        assert_eq!(extract_host("http://example.com#frag"), Some("example.com"));
        // 无 scheme 的裸主机（local 解析场景）
        assert_eq!(extract_host("example.com/path"), Some("example.com"));
        // 非法/空输入返回 None
        assert_eq!(extract_host(""), None);
        assert_eq!(extract_host("://"), None);
    }

    #[test]
    fn sec005_is_remote_domain_allowed_local_host() {
        // 本地调试域名始终放行（与白名单内容无关）。
        assert!(is_remote_domain_allowed("http://localhost/w"));
        assert!(is_remote_domain_allowed("http://127.0.0.1:3000/w"));
    }

    #[test]
    fn sec005_is_remote_domain_allowed_default_empty_list() {
        // H1 收紧后：默认空白名单拒绝所有远程域名（fail-closed）。
        assert!(
            !is_remote_domain_allowed("https://example.com/page"),
            "空白名单应拒绝远程域名"
        );
        // build_url 对远程 http/https 也应返回 Err
        assert!(build_url("https://example.com/page").is_err());
    }

    #[test]
    fn sec005_is_remote_domain_allowed_with_empty_list() {
        // 空表语义（H1）：仅放行 localhost/127.0.0.1，其余远程一律拒绝
        assert!(is_remote_domain_allowed_with(
            &[],
            "http://localhost:8080/w"
        ));
        assert!(is_remote_domain_allowed_with(&[], "http://127.0.0.1/w"));
        assert!(!is_remote_domain_allowed_with(
            &[],
            "https://example.com/page"
        ));
        assert!(!is_remote_domain_allowed_with(&[], "http://192.168.1.10/w"));
    }

    #[test]
    fn sec005_is_remote_domain_allowed_with_whitelist_hit_miss() {
        // 白名单命中：匹配时放行
        assert!(is_remote_domain_allowed_with(
            &["example.com"],
            "https://example.com/page"
        ));
        // 白名单未命中：拒绝
        assert!(!is_remote_domain_allowed_with(
            &["example.com"],
            "https://other.com/page"
        ));
        // 端口/路径不影响命中
        assert!(is_remote_domain_allowed_with(
            &["example.com"],
            "https://example.com:8443/api?x=1#f"
        ));
        // 解析失败（无 scheme 的非法输入）→ 拒绝（fail-closed）
        assert!(!is_remote_domain_allowed_with(&["example.com"], ""));
    }

    #[test]
    fn sec005_is_remote_domain_allowed_with_case_insensitive() {
        // 白名单匹配大小写不敏感（主机名不区分大小写）
        assert!(is_remote_domain_allowed_with(
            &["Example.COM"],
            "https://example.com/page"
        ));
        assert!(is_remote_domain_allowed_with(
            &["example.com"],
            "HTTPS://EXAMPLE.COM/Page"
        ));
        assert!(is_remote_domain_allowed_with(
            &["EXAMPLE.COM"],
            "https://Example.com/"
        ));
    }

    #[test]
    fn sec005_host_extraction_case_insensitive() {
        // 主机名大小写不敏感（升/降序比对逻辑正确性）。
        let host = extract_host("HTTPS://EXAMPLE.COM/Path").expect("应解析出主机");
        assert_eq!(host.to_ascii_lowercase(), "example.com");
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
