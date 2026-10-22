//! 网页预览（Task 8.5）。
//!
//! ## 按需创建、关闭彻底释放
//!
//! WebView2 由 UI 进程**按需**创建：一个 `WS_CHILD` 宿主子窗口 + 独立的
//! `UserDataFolder`（`data_root/webview2-cache/ui-<pid>`，与壁纸侧 wp-proc 的目录隔离）。
//! 关闭时 `controller.Close()` 并 drop 全部 COM 接口，浏览器进程（`msedgewebview2.exe`）
//! 随之退出。
//!
//! ## 为什么是异步状态机
//!
//! `CreateCoreWebView2EnvironmentWithOptions` / `CreateCoreWebView2Controller` 都是异步
//! 回调（WebView2 需要消息泵）。而 Slint 事件循环独占 UI 线程，**绝不能在回调里阻塞等待**。
//! 因此拆成 [`WebPending`] 状态机：环境与控制器就绪后由完成回调写入共享槽，
//! 由 Slint 侧定时器（`preview-pending` → `preview-poll-tick`）在 UI 线程轮询推进
//! （`poll()` 在环境就绪后于 UI 线程发起控制器创建请求，再由下一次轮询取回控制器并
//! `SetBounds` / `Navigate`）。完成回调只触碰线程本地的 `Rc<RefCell<..>>`，不直接触碰 Slint，
//! 从而不要求 `slint::Weak` 跨线程/`Send`。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
};
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use super::host_child::{PhysRect, PreviewHost};

/// 目标为空 / 无法解析时展示的内置提示页。
const FALLBACK_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><style>
body{margin:0;font-family:"Microsoft YaHei",system-ui,sans-serif;background:linear-gradient(135deg,#0f2027,#203a43,#2c5364);color:#eaf6ff;display:flex;align-items:center;justify-content:center;height:100vh}
.card{text-align:center}h1{font-size:26px;margin:0 0 12px}p{opacity:.85;margin:6px 0}
</style></head><body><div class="card">
<h1>镜星壁纸 · 网页预览</h1>
<p>该网页壁纸地址为空或无法解析。</p>
</div></body></html>"#;

/// 网页预览目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 导航到 URL（http/https/file）。
    Url(String),
    /// 直接渲染 HTML 字符串（本内置提示页 / 后续可扩展）。
    Html(String),
}

/// 由壁纸条目的 `file_path` 解析导航目标（纯函数，便于单测）。
///
/// - 显式 `http(s)://` / `file://` 直接导航；
/// - 已存在的本地文件（如本地 html）转 `file:///` URL；
/// - 其余（空串 / 不存在的路径）退回内置提示页。
pub fn resolve_target(file_path: &str) -> Target {
    let trimmed = file_path.trim();
    if trimmed.is_empty() {
        return Target::Html(FALLBACK_HTML.to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://")
    {
        return Target::Url(trimmed.to_string());
    }
    if std::path::Path::new(trimmed).exists() {
        return Target::Url(file_url(trimmed));
    }
    Target::Html(FALLBACK_HTML.to_string())
}

/// Windows 本地路径 → `file:///` URL。
fn file_url(path: &str) -> String {
    format!("file:///{}", path.replace('\\', "/"))
}

/// 完成回调写入的共享槽。
///
/// WebView2 在创建它的 UI 线程（经消息泵）回调，故用线程本地的 `Rc<RefCell<..>>` 即可，
/// 不需要 `Send`/`Sync`（COM 接口本身也不满足）。
#[derive(Default)]
struct Shared {
    env: Option<ICoreWebView2Environment>,
    controller: Option<ICoreWebView2Controller>,
    error: Option<String>,
    controller_requested: bool,
    /// 预览在创建完成前被关闭：完成回调据此关闭已建立的控制器，避免 WebView2 泄漏。
    cancelled: bool,
}

/// WebView2 创建的异步状态机（自持专用宿主子窗口，就绪后移交 [`WebPreview`]）。
pub struct WebPending {
    /// 专用宿主子窗口；就绪移交 [`WebPreview`] 后置为 `None`，避免被重复销毁。
    host: Option<PreviewHost>,
    target: Target,
    width: i32,
    height: i32,
    shared: Rc<RefCell<Shared>>,
}

/// [`WebPending::poll`] 的结果。
pub enum WebPoll {
    /// 仍在创建中。
    Pending,
    /// 创建完成。
    Ready(WebPreview),
    /// 创建失败（含错误信息）。
    Failed(String),
}

impl WebPending {
    /// 在 UI 线程创建宿主子窗口并发起环境创建。`parent` 为 Slint 顶层窗口，
    /// `rect` 为预览区在父窗口客户区内的物理矩形。
    pub fn new(parent: HWND, rect: PhysRect, target: Target) -> Result<Self, String> {
        // 与视频预览同构：WebView2 必须承载在**专用 `WS_CHILD` 子窗口**内，否则
        // `SetBounds` 会以顶层窗口客户区为基准、把画面铺到整个窗口左上角（遮住
        // 关闭遮罩），且 `move_host` 会误搬动主窗口本身。
        let host = PreviewHost::create(parent, rect)?;
        let (width, height) = (rect.2, rect.3);

        ensure_com();
        let shared = Rc::new(RefCell::new(Shared::default()));
        let udf = HSTRING::from(user_data_folder().to_string_lossy().to_string());

        let shared_cb = Rc::clone(&shared);
        let callback = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |error_code, environment| -> windows::core::Result<()> {
                let mut guard = shared_cb.borrow_mut();
                match error_code {
                    Ok(()) => match environment {
                        Some(env) => guard.env = Some(env),
                        None => guard.error = Some("环境回调返回 None".to_string()),
                    },
                    Err(e) => guard.error = Some(format!("环境创建失败: {e}")),
                }
                Ok(())
            },
        ));

        let options: ICoreWebView2EnvironmentOptions =
            CoreWebView2EnvironmentOptions::default().into();
        unsafe {
            let args = HSTRING::from("--disk-cache-size=10485760 --renderer-process-limit=1");
            let _ = options.SetAdditionalBrowserArguments(&args);
            if let Err(e) =
                CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &udf, &options, &callback)
            {
                // `host` 随作用域 Drop 先隐藏再销毁。
                return Err(format!(
                    "CreateCoreWebView2EnvironmentWithOptions 失败: {e}"
                ));
            }
        }

        Ok(Self {
            host: Some(host),
            target,
            width,
            height,
            shared,
        })
    }

    /// 显式显示宿主子窗口（幂等）。就绪后调用，隐藏创建 → 就绪显示，避免残影。
    pub fn show(&mut self) {
        if let Some(host) = self.host.as_mut() {
            host.show();
        }
    }

    /// 预览区尺寸变化时同步宿主子窗口位置，并刷新待用边界（供后续 `configure` 使用）。
    pub fn resize(&mut self, rect: PhysRect) {
        self.width = rect.2;
        self.height = rect.3;
        if let Some(host) = self.host.as_ref() {
            host.move_to(rect);
        }
    }

    /// 推进一步状态机（在 UI 线程调用，由 Slint 定时器驱动）。
    pub fn poll(&mut self) -> WebPoll {
        // 1) 完成回调可能已写入错误或控制器。
        let (error, controller, env) = {
            let mut guard = self.shared.borrow_mut();
            if let Some(err) = guard.error.take() {
                (Some(err), None, None)
            } else {
                (None, guard.controller.take(), guard.env.clone())
            }
        };
        if let Some(err) = error {
            return WebPoll::Failed(err);
        }
        if let Some(controller) = controller {
            let Some(env) = env else {
                unsafe {
                    let _ = controller.Close();
                }
                return WebPoll::Failed("缺少 WebView2 环境句柄".to_string());
            };
            return match configure(&controller, self.width, self.height, &self.target) {
                Ok(webview) => {
                    // 就绪即显示宿主子窗口（隐藏创建 → 几何/首帧就绪后显示，避免左上角残影）；
                    // PreviewHost 承接 shown=true，上层再调 show() 为 no-op。
                    self.show();
                    // 宿主子窗口移交 WebPreview 接管，置 None 避免本状态机 Drop 时误销毁。
                    let Some(host) = self.host.take() else {
                        unsafe {
                            let _ = controller.Close();
                        }
                        return WebPoll::Failed("宿主子窗口缺失".to_string());
                    };
                    WebPoll::Ready(WebPreview {
                        host,
                        controller,
                        _environment: env,
                        _webview: webview,
                    })
                }
                Err(e) => {
                    unsafe {
                        let _ = controller.Close();
                    }
                    WebPoll::Failed(e)
                }
            };
        }

        // 2) 环境就绪后（仅一次）发起控制器创建请求。
        let request = {
            let mut guard = self.shared.borrow_mut();
            if !guard.controller_requested && guard.env.is_some() {
                guard.controller_requested = true;
                guard.env.clone()
            } else {
                None
            }
        };
        if let Some(env) = request {
            let shared_cb = Rc::clone(&self.shared);
            let callback = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |error_code, controller| -> windows::core::Result<()> {
                    let mut guard = shared_cb.borrow_mut();
                    // 预览已关闭：丢弃并关闭可能已建立的控制器，避免 WebView2 资源泄漏。
                    if guard.cancelled {
                        if error_code.is_ok() {
                            if let Some(c) = controller {
                                unsafe {
                                    let _ = c.Close();
                                }
                            }
                        }
                        return Ok(());
                    }
                    match error_code {
                        Ok(()) => match controller {
                            Some(c) => guard.controller = Some(c),
                            None => guard.error = Some("控制器回调返回 None".to_string()),
                        },
                        Err(e) => guard.error = Some(format!("控制器创建失败: {e}")),
                    }
                    Ok(())
                },
            ));
            unsafe {
                let host = self.host.as_ref().map(PreviewHost::hwnd).unwrap_or_default();
                if let Err(e) = env.CreateCoreWebView2Controller(host, &callback) {
                    return WebPoll::Failed(format!("CreateCoreWebView2Controller 调用失败: {e}"));
                }
            }
        }
        WebPoll::Pending
    }
}

impl Drop for WebPending {
    fn drop(&mut self) {
        // 未走到 Ready 就被丢弃（关闭/切换预览、创建失败）：先标记取消并关闭可能已建立的
        // 控制器（控制器回调可能晚于本次 drop），再销毁宿主子窗口，避免空白子窗口与
        // WebView2 资源残留。Ready 时 host 与 controller 均已被 take()，此处为 no-op。
        let controller = {
            let mut guard = self.shared.borrow_mut();
            guard.cancelled = true;
            guard.controller.take()
        };
        if let Some(controller) = controller {
            unsafe {
                let _ = controller.Close();
            }
        }
        // 宿主子窗口（若尚未移交）由 `host` 字段 Drop 先隐藏再销毁，避免空白子窗口残留。
    }
}

/// 已就绪的网页预览。
pub struct WebPreview {
    host: PreviewHost,
    controller: ICoreWebView2Controller,
    /// 保持环境与 WebView 接口存活，直到控制器关闭。
    _environment: ICoreWebView2Environment,
    _webview: ICoreWebView2,
}

impl WebPreview {
    /// 承载子窗口句柄。
    pub fn host(&self) -> HWND {
        self.host.hwnd()
    }

    /// 显式显示宿主子窗口（幂等）。
    pub fn show(&mut self) {
        self.host.show();
    }

    /// 预览区尺寸变化时同步 WebView 边界。
    pub fn resize(&self, width: i32, height: i32) {
        let rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe {
            let _ = self.controller.SetBounds(rect);
        }
    }
}

impl Drop for WebPreview {
    fn drop(&mut self) {
        // 显式 Close() 释放 WebView2 资源（浏览器进程、渲染管线），随后字段依次 Drop
        // 递减 COM 引用计数；宿主子窗口由 `host` 字段 Drop 先隐藏再销毁，避免残留可见窗口。
        unsafe {
            let _ = self.controller.Close();
        }
    }
}

/// 首次调用时把 UI 线程初始化为 STA（WebView2 要求）。
fn ensure_com() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        // 已初始化过时返回 S_FALSE / RPC_E_CHANGED_MODE，均可忽略。
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    });
}

/// `data_root/webview2-cache/ui-<pid>`（与壁纸侧目录隔离，便于清理与归因）。
fn user_data_folder() -> PathBuf {
    let dir = mirrorstar_core::config::data_root()
        .join("webview2-cache")
        .join(format!("ui-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 配置控制器：可见、设定边界、导航到目标。
fn configure(
    controller: &ICoreWebView2Controller,
    width: i32,
    height: i32,
    target: &Target,
) -> Result<ICoreWebView2, String> {
    unsafe {
        controller
            .SetIsVisible(true)
            .map_err(|e| format!("SetIsVisible 失败: {e}"))?;
        let rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        controller
            .SetBounds(rect)
            .map_err(|e| format!("SetBounds 失败: {e}"))?;
        let webview = controller
            .CoreWebView2()
            .map_err(|e| format!("CoreWebView2 失败: {e}"))?;
        match target {
            Target::Url(url) => {
                let hs = HSTRING::from(url.as_str());
                webview
                    .Navigate(&hs)
                    .map_err(|e| format!("Navigate 失败: {e}"))?;
            }
            Target::Html(html) => {
                let hs = HSTRING::from(html.as_str());
                webview
                    .NavigateToString(&hs)
                    .map_err(|e| format!("NavigateToString 失败: {e}"))?;
            }
        }
        Ok(webview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_schemes_pass_through() {
        assert_eq!(
            resolve_target("https://example.com/a?b=1"),
            Target::Url("https://example.com/a?b=1".to_string())
        );
        assert_eq!(
            resolve_target("  http://example.com  "),
            Target::Url("http://example.com".to_string())
        );
        assert_eq!(
            resolve_target("file:///C:/x/y.html"),
            Target::Url("file:///C:/x/y.html".to_string())
        );
    }

    #[test]
    fn empty_or_missing_falls_back_to_html() {
        assert!(matches!(resolve_target(""), Target::Html(_)));
        assert!(matches!(
            resolve_target("C:/definitely/not/here.html"),
            Target::Html(_)
        ));
    }

    #[test]
    fn local_file_becomes_file_url() {
        let dir = std::env::temp_dir().join(format!("mirrorstar_ui_web_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("index.html");
        std::fs::write(&page, "<html></html>").unwrap();

        let expected = format!("file:///{}", page.to_string_lossy().replace('\\', "/"));
        assert_eq!(
            resolve_target(page.to_str().unwrap()),
            Target::Url(expected)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
