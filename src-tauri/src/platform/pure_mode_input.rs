//! 纯净壁纸模式输入捕获线程（src-tauri 侧，实现契约 D6）。
//!
//! # 模块职责
//!
//! 在纯净壁纸模式运行期间，捕获"任意键 / 鼠标点击"作为退出信号，并额外负责：
//!
//! 1. **吞掉触发输入**：触发事件本身及其后的短暂输入不传递给其他应用（见"吞掉策略"）。
//! 2. **宽限防误触**：进入模式后 [`GRACE_MS`] 内的输入不触发退出（避免托盘点击当场进出）。
//! 3. **光标空闲隐藏**：鼠标静止 [`IDLE_HIDE_MS`] 后隐藏光标，任何移动即恢复，退出时保证恢复。
//! 4. **系统广播监听**：`WM_DISPLAYCHANGE` → 请求退出；`TaskbarCreated` → 重断言置顶窗口 Z 序。
//!
//! 退出编排（回嵌 WorkerW、销毁临时渲染器、取消托盘勾选等）**不在本模块**，
//! 由上层（Task 5 的 `platform/pure_mode.rs`）在收到 [`ExitReason`] 后异步执行。
//!
//! # 线程模型
//!
//! 单个专用 `std::thread`（线程名 [`THREAD_NAME`]，512KB 栈，与 `fullscreen.rs` /
//! `explorer.rs` 的监控线程同构）：
//!
//! 1. 注册自有窗口类 [`CLASS_NAME`]（`lpfnWndProc` 指向本模块的 `extern "system"` 过程）；
//! 2. 创建**隐藏的顶层窗口**（`WS_EX_TOOLWINDOW | WS_POPUP` 且**不** `ShowWindow`）。
//!    **刻意不用 `HWND_MESSAGE`**：消息专用窗口按文档不接收 `HWND_BROADCAST` 广播，
//!    而本模块必须收到 `WM_DISPLAYCHANGE` / `TaskbarCreated`；`WS_EX_TOOLWINDOW` 保证
//!    它不进 Alt+Tab，`WS_POPUP` + 不显示保证不可见；
//! 3. 安装 `WH_KEYBOARD_LL` / `WH_MOUSE_LL` 低级钩子；
//! 4. `GetMessageW` 消息循环（同时驱动低级钩子回调与 `WM_TIMER`）。
//!
//! 低级钩子的回调运行在**安装钩子的线程**上下文，因此回调与该线程的消息循环同线程；
//! 回调内**禁止**阻塞 / 加锁 / 日志风暴（超时会被系统静默摘除钩子），只使用进程级原子量，
//! 并通过 `PostMessageW(hwnd, WM_APP_TRIGGER)` 把"重活"（通道通知 + 启动吞掉定时器）
//! 交给同一线程的消息循环处理。
//!
//! # 触发条件
//!
//! **只按 `wParam` 判断消息类型**，不读取 `KBDLLHOOKSTRUCT` / `MSLLHOOKSTRUCT`
//! （避免为项目新增 `Win32_UI_Input_KeyboardAndMouse` feature）：
//!
//! - 键盘：`WM_KEYDOWN` / `WM_SYSKEYDOWN`（首个按下即触发）
//! - 鼠标：`WM_LBUTTONDOWN` / `WM_RBUTTONDOWN` / `WM_MBUTTONDOWN`
//!
//! # 吞掉策略（spec D6）
//!
//! 触发瞬间 `return LRESULT(1)` 吞掉该事件，随后进入约 [`SWALLOW_MS`] 的"吞掉窗口"
//! （键盘与鼠标一同吞掉），定时器到期后才卸载钩子。设计意图：
//!
//! 1. 吞掉触发键的 **keyup**，避免 Win 键在恢复后的桌面弹出开始菜单
//!    （开始菜单在 Win 键**抬起**时打开）；
//! 2. 吞掉退出瞬间的点击，避免误点到恢复后的桌面内容。
//!
//! # 失败降级策略
//!
//! - **键盘钩子安装失败 → 拒绝进入模式**：`install` 返回 `Err`，线程在回传错误前
//!   已清理（卸载已装钩子 + 销毁窗口 + 注销窗口类），避免"进得去出不来"。
//! - **鼠标钩子安装失败 → 仅 warn 降级**：仍视为安装成功（键盘仍可退出）。
//!
//! # 进程级状态
//!
//! 本模块的原子量是**进程级**的，一次只能有一个活跃的捕获线程；调用方必须先
//! [`InputHook::shutdown`] 再 `install`（`install` 首部会复位全部状态）。
//!
//! # 显式卸载与异常退出的区分（A3）
//!
//! 线程收尾（`cleanup_after_loop`）只在"**非显式卸载** + **非用户触发** + **模式仍活跃**"
//! 三者同时成立时才投递 [`ExitReason::HookFailed`]（由纯函数 `should_notify_hook_failed` 判定）：
//! 显式卸载（[`InputHook::shutdown`]，退出编排与应用退出 `perform_shutdown_blocking` 都走它）
//! 属受控路径，即使模式标志仍活跃也不得投递，否则会在应用退出路径触发并发的退出编排。
//! 同时线程收尾统一释放本代退出通道（`release_exit_tx`，经 `same_channel` 防误清下一代），
//! 使"非输入路径退出"的退出监听任务 `recv()` 立即返回 `None` 并结束，不再悬挂到下次安装。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{FALSE, HINSTANCE, HWND, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, RegisterWindowMessageW,
    SetTimer, SetWindowsHookExW, ShowCursor, TranslateMessage, UnhookWindowsHookEx,
    UnregisterClassW, HHOOK, MSG, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_QUIT,
    WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// 捕获线程名（与仓库其它监控线程命名约定一致）
const THREAD_NAME: &str = "mirrorstar-pure-mode-input";

/// 隐藏顶层窗口类名（注册与创建处共用）
const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("MirrorStarPureModeInput");

/// 进入模式后的宽限期（毫秒）：期内输入**不触发退出、也不被吞掉**，直接放行
/// （避免"从托盘点击进入模式"的同一次点击当场把模式关掉）
const GRACE_MS: i64 = 800;

/// 触发后的"吞掉窗口"时长（毫秒）：期内键盘与鼠标事件一律吞掉，到期后卸载钩子
const SWALLOW_MS: u32 = 300;

/// 光标静止多久后隐藏（毫秒）
const IDLE_HIDE_MS: i64 = 3000;

/// 光标空闲检查定时器周期（毫秒）
const IDLE_TICK_MS: u32 = 500;

/// `install` 同步等待线程初始化结果的超时（超时按失败处理）
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5);

/// 吞掉窗口定时器 ID（必须非 0）
const TIMER_SWALLOW: usize = 1;

/// 光标空闲检查定时器 ID（必须非 0）
const TIMER_IDLE: usize = 2;

/// 触发私有消息（钩子回调 → 隐藏窗口过程，在消息循环上下文执行"重活"）
const WM_APP_TRIGGER: u32 = WM_APP + 1;

// ── 进程级状态（回调内只允许访问这些原子量） ────────────────────────────────

/// 进程内单调时钟基准（`install` 时初始化一次，此后只读；回调内用 `get()` 避免加锁）
static MONO_BASE: OnceLock<Instant> = OnceLock::new();

/// 本次安装的宽限期基准（`elapsed_ms` 坐标系下的毫秒值）
static GRACE_BASE_MS: AtomicI64 = AtomicI64::new(0);

/// 最近一次鼠标移动的毫秒值（`elapsed_ms` 坐标系）
static LAST_MOVE_MS: AtomicI64 = AtomicI64::new(0);

/// 吞掉窗口截止的毫秒值（0 表示未激活）
static SWALLOW_UNTIL: AtomicI64 = AtomicI64::new(0);

/// 是否已请求退出（保证一次安装只触发一次退出通知）
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 是否由调用方"显式卸载"（`InputHook::shutdown`）引发的线程退出（A3）。
///
/// `cleanup_after_loop` 用它区分"受控卸载"与"消息循环被外部打断"：显式卸载属受控路径
/// （退出编排 / 应用退出），即使模式标志仍活跃也不得投递 `HookFailed`，否则会在应用
/// 退出路径触发与 `perform_shutdown_blocking` 并发的退出编排。`install` 首部复位。
static EXPLICIT_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// 光标是否处于"已隐藏"状态（保证 `ShowCursor` 成对调用，显示计数器不累积为负）
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// 隐藏窗口句柄（以 `isize` 存储规避 `HWND` 非 `Send`/`Sync`）
static HIDDEN_HWND: AtomicIsize = AtomicIsize::new(0);

/// 键盘低级钩子句柄（以 `isize` 存储，同上）
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

/// 鼠标低级钩子句柄（以 `isize` 存储，同上；0 表示未安装/已卸载）
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);

/// 捕获线程 ID（供超时清理路径投递 `WM_QUIT`）
static CURRENT_TID: AtomicU32 = AtomicU32::new(0);

/// `TaskbarCreated` 注册消息 ID（0 表示注册失败，与任何消息都不相等）
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

/// 退出原因通道（`install` 时写入，窗口过程在触发/显示器变化时投递）
///
/// 仅在窗口过程（消息循环上下文）与 `thread_main`（线程初始化）中访问，
/// 不在低级钩子回调中访问，因此这里的 `Mutex` 不会出现在"禁止加锁"的回调路径上。
static EXIT_TX: Mutex<Option<UnboundedSender<ExitReason>>> = Mutex::new(None);

/// 退出原因（由上层异步任务消费）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitReason {
    /// 用户按键或点击鼠标触发退出
    Input,
    /// 显示器配置变化（`WM_DISPLAYCHANGE`）触发退出
    DisplayChange,
    /// 钩子线程非用户触发而退出（消息循环被外部打断 / 错误），
    /// 用于让编排层退出模式，避免"钩子没了但壁纸仍置顶"
    HookFailed,
}

/// 输入捕获句柄：持有捕获线程 ID 与 `JoinHandle`，用于幂等卸载。
pub(crate) struct InputHook {
    /// 捕获线程 ID（`PostThreadMessageW(WM_QUIT)` 唤醒消息循环用）
    thread_id: u32,
    /// 捕获线程 `JoinHandle`（`take` 后即为"已回收"）
    handle: Option<std::thread::JoinHandle<()>>,
    /// 是否仍处于已安装状态（幂等标志）
    installed: bool,
}

impl InputHook {
    /// 卸载：等待尚未到期的吞掉窗口 → `PostThreadMessageW(WM_QUIT)` 唤醒消息循环 + join 线程（幂等）
    ///
    /// # 为何先等待吞掉窗口（spec D6）
    ///
    /// 触发瞬间由 `request_exit` 设置 `SWALLOW_UNTIL = 触发时刻 + SWALLOW_MS`，钩子需在该窗口内
    /// 继续吞掉键盘/鼠标消息——尤其要吞掉触发键的 **keyup**（Win 键在**抬起**时弹出开始菜单）
    /// 与退出瞬间的点击。上层退出编排（`platform/pure_mode.rs`）第 2 步即调用本函数，若此时
    /// 立刻卸载钩子，剩余吞掉窗口内的事件会泄漏给其它应用（竞态：回嵌耗时 <300ms 时必现）。
    /// 因此此处先 `sleep(remaining_swallow_ms())`（最长 [`SWALLOW_MS`]）再卸载。
    ///
    /// 调用方契约：本函数运行在上层退出编排的 `spawn_blocking` / 同步退出路径，允许短暂阻塞
    /// （≤300ms）；**不得**在低级钩子回调或消息循环内调用。
    ///
    /// 幂等：重复调用直接返回；线程若已自然退出（吞掉窗口到期后 `PostQuitMessage`），
    /// `PostThreadMessageW` 会失败（仅记录 debug）而 `join` 仍能正常返回。
    pub(crate) fn shutdown(&mut self) {
        if !self.installed {
            return;
        }
        self.installed = false;
        // A3：标记本次为"显式卸载"。线程收尾只对"非触发 + 模式仍活跃 + 非显式卸载"
        // 的情形投递 HookFailed（应用退出路径由 perform_shutdown_blocking 卸载时模式
        // 标志仍活跃，若误报会触发并发的退出编排）。
        EXPLICIT_SHUTDOWN.store(true, Ordering::Release);
        // 尊重尚未到期的吞掉窗口：等触发输入（尤其 Win 键 keyup）被钩子吞掉后再卸载
        let remain = remaining_swallow_ms();
        if remain > 0 {
            tracing::debug!(
                remain_ms = remain,
                "退出编排早于吞掉窗口到期，等待后再卸载输入钩子（保证触发输入被吞掉）"
            );
            std::thread::sleep(Duration::from_millis(remain));
        }
        // 线程若已自然退出，此处投递失败，仅记录（不视为错误）
        if let Err(e) = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            tracing::debug!(error = %e, "PostThreadMessageW(WM_QUIT) 未送达（线程可能已自然退出）");
        }
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::warn!("纯净壁纸模式输入捕获线程 join 失败（线程内部 panic）");
            }
        }
        tracing::info!("纯净壁纸模式输入捕获已卸载");
    }
}

/// 安装输入捕获：成功返回可 shutdown 的句柄；键盘钩子安装失败时返回 `Err`（拒绝进入模式）。
///
/// 同步等待线程初始化结果（最长 [`INSTALL_TIMEOUT`]）：
/// - `Ok(thread_id)` → 键盘钩子已装好（鼠标钩子可能降级缺失）
/// - `Err(msg)` → 键盘钩子安装失败或初始化失败（线程已自行清理）
/// - 等待超时 → 按失败处理（先关闭接收端，再投递 `WM_QUIT` 并 join 回收）
pub(crate) fn install(exit_tx: UnboundedSender<ExitReason>) -> Result<InputHook, String> {
    // 复位上一次运行遗留的进程级状态（本模块要求"先 shutdown 再 install"）
    EXIT_REQUESTED.store(false, Ordering::Release);
    EXPLICIT_SHUTDOWN.store(false, Ordering::Release);
    SWALLOW_UNTIL.store(0, Ordering::Release);
    // F4：恢复上一轮异常退出可能遗留的隐藏光标（显示计数器成对性）
    if CURSOR_HIDDEN.load(Ordering::Acquire) {
        unsafe { ShowCursor(TRUE) };
    }
    CURSOR_HIDDEN.store(false, Ordering::Release);
    HIDDEN_HWND.store(0, Ordering::Release);
    KEYBOARD_HOOK.store(0, Ordering::Release);
    MOUSE_HOOK.store(0, Ordering::Release);
    CURRENT_TID.store(0, Ordering::Release);
    TASKBAR_CREATED_MSG.store(0, Ordering::Release);

    // 单调时钟基准（进程内只初始化一次）与本次宽限期/空闲基准（每次安装刷新）
    let base = MONO_BASE.get_or_init(Instant::now);
    let base_ms = base.elapsed().as_millis() as i64;
    GRACE_BASE_MS.store(base_ms, Ordering::Release);
    LAST_MOVE_MS.store(base_ms, Ordering::Release);

    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();

    let handle = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        // 与其它监控线程一致：仅需 Win32 回调与消息循环，512KB 足够
        .stack_size(512 * 1024)
        .spawn(move || thread_main(exit_tx, ready_tx))
        .map_err(|e| format!("启动纯净壁纸模式输入捕获线程失败: {e}"))?;

    // 先取结果再 match：超时分支需在 `join` 前 `drop(ready_rx)` 关闭接收端（见下），
    // 故不能把 `recv_timeout` 直接作为 match 判别式（其借用会延续整个 match 作用域）。
    let recv_result = ready_rx.recv_timeout(INSTALL_TIMEOUT);
    match recv_result {
        Ok(Ok(thread_id)) => {
            tracing::info!(thread_id, "纯净壁纸模式输入捕获已安装");
            Ok(InputHook {
                thread_id,
                handle: Some(handle),
                installed: true,
            })
        }
        Ok(Err(message)) => {
            // 线程已自行清理并退出，join 用于回收资源
            let _ = handle.join();
            Err(message)
        }
        Err(RecvTimeoutError::Timeout) => {
            tracing::error!(
                timeout_ms = INSTALL_TIMEOUT.as_millis(),
                "等待输入捕获线程初始化超时，按失败处理"
            );
            let tid = CURRENT_TID.load(Ordering::Acquire);
            if tid != 0 {
                // 极端路径：线程若尚未创建消息队列，投递失败也仅忽略
                let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
            }
            // 关键（B3）：必须在 `join` 前关闭接收端，使线程随后的
            // `ready.send(Ok(thread_id))`（见 thread_main_inner 第 8 步）必定失败，从而走既有的
            // "无人接收 → cleanup_after_loop 后退出"路径主动结束线程。否则若线程消息队列尚未
            // 建立导致上面的 `PostThreadMessageW` 无效，线程会成功 send 并进入 `GetMessageW`
            // 循环，`join` 永不返回 ⇒ 本函数与其外层 `spawn_blocking` 永久阻塞（ENTER_IN_PROGRESS
            // 永不复位，功能永久锁死）。关闭接收端后线程必然自行清理退出，`join` 可正常回收。
            drop(ready_rx);
            let _ = handle.join();
            Err("等待输入捕获线程初始化超时".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Err("输入捕获线程未回传初始化结果即退出".to_string())
        }
    }
}

/// 捕获线程入口：主体执行完后统一释放本代退出通道（含安装失败早退路径，A3）。
fn thread_main(exit_tx: UnboundedSender<ExitReason>, ready: mpsc::Sender<Result<u32, String>>) {
    let tx_for_release = exit_tx.clone();
    thread_main_inner(exit_tx, ready);
    release_exit_tx(&tx_for_release);
}

/// 捕获线程主体：窗口类 → 隐藏顶层窗口 → 钩子 → 消息循环 → 收尾清理。
fn thread_main_inner(
    exit_tx: UnboundedSender<ExitReason>,
    ready: mpsc::Sender<Result<u32, String>>,
) {
    let thread_id = unsafe { GetCurrentThreadId() };
    CURRENT_TID.store(thread_id, Ordering::Release);

    // 把退出通道存入全局，供窗口过程在触发/显示器变化时通知上层异步任务
    {
        let mut slot = EXIT_TX.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(exit_tx);
    }

    // 1. 注册 TaskbarCreated 广播消息 ID（供窗口过程比对）
    let taskbar_created = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
    if taskbar_created == 0 {
        report_install_failure(&ready, "注册 TaskbarCreated 消息失败");
        return;
    }
    TASKBAR_CREATED_MSG.store(taskbar_created, Ordering::Release);

    // 2. 模块实例句柄（RegisterClassW / CreateWindowExW / SetWindowsHookExW 共用）
    let h_module = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h,
        Err(e) => {
            report_install_failure(&ready, format!("获取模块实例句柄失败: {e}"));
            return;
        }
    };
    let h_instance = HINSTANCE(h_module.0);

    // 3. 注册自有窗口类
    let wc = WNDCLASSW {
        lpfnWndProc: Some(pure_mode_input_wndproc),
        hInstance: h_instance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        report_install_failure(&ready, "注册纯净壁纸模式输入窗口类失败");
        return;
    }

    // 4. 创建隐藏的顶层窗口
    //    不用 HWND_MESSAGE（不接收广播）；不调用 ShowWindow（保持不可见）
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS_NAME,
            CLASS_NAME,
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            None,
            h_instance,
            None,
        )
    } {
        Ok(h) if !h.is_invalid() => h,
        Ok(_) => {
            let _ = unsafe { UnregisterClassW(CLASS_NAME, h_instance) };
            report_install_failure(&ready, "创建纯净壁纸模式输入窗口失败: 无效句柄");
            return;
        }
        Err(e) => {
            let _ = unsafe { UnregisterClassW(CLASS_NAME, h_instance) };
            report_install_failure(&ready, format!("创建纯净壁纸模式输入窗口失败: {e}"));
            return;
        }
    };
    HIDDEN_HWND.store(hwnd.0 as isize, Ordering::Release);

    // 5. 键盘低级钩子（必需）：安装失败 → 清理后回传错误，拒绝进入模式
    let keyboard_hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), h_instance, 0) } {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, "键盘低级钩子安装失败，拒绝进入纯净壁纸模式");
                // 清理：销毁窗口 + 注销窗口类（此时尚未安装任何钩子）
                let _ = unsafe { DestroyWindow(hwnd) };
                let _ = unsafe { UnregisterClassW(CLASS_NAME, h_instance) };
                let _ = ready.send(Err(format!("键盘低级钩子安装失败: {e}")));
                return;
            }
        };
    KEYBOARD_HOOK.store(keyboard_hook.0 as isize, Ordering::Release);

    // 6. 鼠标低级钩子（可选）：失败仅降级（键盘仍能退出）
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), h_instance, 0) } {
        Ok(h) => MOUSE_HOOK.store(h.0 as isize, Ordering::Release),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "鼠标低级钩子安装失败，降级为仅键盘退出（鼠标点击不再能退出纯净壁纸模式）"
            );
        }
    }

    // 7. 光标空闲检查定时器（周期探测；静止超时后在 WM_TIMER 中隐藏光标）
    //    B7：`SetTimer` 返回 0 表示创建失败。定时器同时承担"空闲隐藏光标"与
    //    "吞掉窗口到期后回收/兜底自愈丢失的退出请求"两项职责，创建失败时二者会同时失效；
    //    仍按既有语义继续（退出仍可由正常输入触发的钩子回调路径完成）。
    if unsafe { SetTimer(hwnd, TIMER_IDLE, IDLE_TICK_MS, None) } == 0 {
        tracing::warn!(
            "纯净壁纸模式空闲检查定时器创建失败（SetTimer 返回 0），空闲隐藏光标与计时兜底自愈将同时失效；退出仍可由正常输入触发"
        );
    }

    // 8. 回传安装成功（含线程 ID）。若 install 侧已放弃等待（超时），立即自行清理
    if ready.send(Ok(thread_id)).is_err() {
        tracing::warn!("安装结果无人接收（install 可能已超时），立即清理并退出捕获线程");
        cleanup_after_loop(hwnd, h_instance);
        return;
    }

    // 9. 消息循环（同时驱动低级钩子回调与 WM_TIMER）
    //    GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
    //    （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        match ret.0 {
            0 => break, // WM_QUIT
            -1 => {
                let err = unsafe { windows::Win32::Foundation::GetLastError() };
                tracing::error!(error = ?err, "GetMessageW 返回 -1（错误），退出输入捕获消息循环");
                break;
            }
            _ => {}
        }
        let _ = unsafe { TranslateMessage(&msg) };
        unsafe { DispatchMessageW(&msg) };
    }

    // 10. 收尾：钩子/光标在正常路径已处理，这里的清理是幂等兜底
    cleanup_after_loop(hwnd, h_instance);
    tracing::info!("纯净壁纸模式输入捕获线程退出");
}

/// 线程收尾清理：卸载钩子 → 恢复光标 → 清窗口句柄 → 销毁窗口 → 注销窗口类（幂等）
///
/// F16：若线程在**非用户触发**情形下退出（`EXIT_REQUESTED` 为假，例如消息循环被外部
/// `WM_QUIT`/错误打断），而引擎侧模式仍活跃，则投递 [`ExitReason::HookFailed`]，
/// 让编排层完整退出模式，避免"钩子已卸载但壁纸仍置顶、用户无输入退出路径"。
/// 该分支也覆盖"安装失败"路径（此时模式尚未激活 → 条件为假，无副作用）。
/// A3：**显式卸载**（[`InputHook::shutdown`]）不触发该通知——它属受控路径，应用退出时
/// 模式标志仍活跃，误报会触发与 `perform_shutdown_blocking` 并发的退出编排。
fn cleanup_after_loop(hwnd: HWND, h_instance: HINSTANCE) {
    unhook_both();
    restore_cursor();
    if should_notify_hook_failed(
        EXPLICIT_SHUTDOWN.load(Ordering::Acquire),
        EXIT_REQUESTED.load(Ordering::Acquire),
        mirrorstar_core::pure_mode::is_active(),
    ) {
        tracing::warn!("输入捕获线程在非触发情形下退出且模式仍活跃，通知上层退出纯净壁纸模式");
        notify_exit(ExitReason::HookFailed);
    }
    HIDDEN_HWND.store(0, Ordering::Release);
    let _ = unsafe { DestroyWindow(hwnd) };
    let _ = unsafe { UnregisterClassW(CLASS_NAME, h_instance) };
}

/// 是否应由线程收尾投递 [`ExitReason::HookFailed`]（纯函数，便于单测；A3）
///
/// 仅当**非显式卸载**、**非用户触发退出**、**模式仍活跃**三者同时成立时才投递：
/// 此时线程是被外部打断（消息循环异常）退出的，必须让编排层退出模式，否则
/// "钩子已卸载但壁纸仍置顶、用户无输入退出路径"。显式卸载属受控路径，不通知。
fn should_notify_hook_failed(
    explicit_shutdown: bool,
    exit_requested: bool,
    mode_active: bool,
) -> bool {
    !explicit_shutdown && !exit_requested && mode_active
}

/// 释放本代退出通道（线程收尾统一出口，A3）
///
/// 仅当全局 `EXIT_TX` 中的 sender 与本线程持有的 sender 属于同一通道时才清空
/// （`same_channel`），防止误清下一代的通道。释放后退出监听任务的 `recv()` 会返回
/// `None` 并正常结束——修复"非输入路径退出（如托盘二次点击）后监听任务悬挂到下次安装"。
fn release_exit_tx(thread_tx: &UnboundedSender<ExitReason>) {
    let mut slot = EXIT_TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(current) = slot.as_ref() {
        if current.same_channel(thread_tx) {
            *slot = None;
        }
    }
}

/// 安装失败时统一回传（日志 + 通过 ready 通道告知 `install`）
fn report_install_failure(ready: &mpsc::Sender<Result<u32, String>>, message: impl Into<String>) {
    let message = message.into();
    tracing::error!(error = %message, "纯净壁纸模式输入捕获安装失败");
    let _ = ready.send(Err(message));
}

// ── 窗口过程 ────────────────────────────────────────────────────────────

/// 隐藏窗口过程
///
/// 整个过程用 `catch_unwind` 包裹：Win32 回调不可让 panic 穿越 FFI 边界（仓库既有先例）。
unsafe extern "system" fn pure_mode_input_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_window_message(hwnd, msg, wparam, lparam)
    }));
    match outcome {
        Ok(result) => result,
        // panic 已被捕获，防穿越 FFI 硬崩溃；退回默认处理
        Err(_) => {
            tracing::error!("纯净壁纸模式输入窗口过程 panic 已被捕获，防止穿越 FFI 硬崩溃");
            def_window_proc(hwnd, msg, wparam, lparam)
        }
    }
}

/// 窗口过程处理体（消息分派）
fn handle_window_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 触发消息（由钩子回调投递）：在此执行"重活"——通知上层 + 启动吞掉窗口定时器
    if msg == WM_APP_TRIGGER {
        tracing::info!("检测到退出输入，通知上层执行退出编排并启动吞掉窗口");
        notify_exit(ExitReason::Input);
        unsafe { SetTimer(hwnd, TIMER_SWALLOW, SWALLOW_MS, None) };
        return LRESULT(0);
    }

    if msg == WM_TIMER {
        return match wparam.0 {
            TIMER_SWALLOW => {
                tracing::info!("吞掉窗口到期，卸载输入钩子并退出捕获线程");
                unhook_both();
                restore_cursor();
                // 结束消息循环：线程自然退出（InputHook::shutdown 的 join 也能正常返回）
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            TIMER_IDLE => {
                maybe_hide_cursor();
                // F3 兜底自愈：触发时的 WM_APP_TRIGGER 投递若失败，退出请求会永久丢失。
                // 周期空闲定时器在此补做收尾，保证模式一定能退出。
                if needs_self_heal() {
                    tracing::warn!("退出请求投递疑似丢失，由空闲定时器自愈：补投通知并收尾");
                    // 重复投递由上层编排幂等消化（EXIT_IN_PROGRESS 守卫）
                    notify_exit(ExitReason::Input);
                    unhook_both();
                    restore_cursor();
                    unsafe { PostQuitMessage(0) };
                }
                LRESULT(0)
            }
            _ => def_window_proc(hwnd, msg, wparam, lparam),
        };
    }

    // 显示器配置变化（分辨率变化 / 热插拔）→ 请求退出（不在此直接做引擎操作）
    if msg == WM_DISPLAYCHANGE {
        tracing::info!("收到 WM_DISPLAYCHANGE，显示器配置变化，请求退出纯净壁纸模式");
        notify_exit(ExitReason::DisplayChange);
        return LRESULT(0);
    }

    // Explorer 重启（任务栏重建）→ 重断言所有置顶窗口 Z 序，防任务栏抢 Z 序
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if taskbar_created != 0 && msg == taskbar_created {
        tracing::info!("收到 TaskbarCreated 广播，重断言置顶窗口 Z 序");
        // 内部先克隆再释放锁，可在"不能持锁做重活"的回调上下文调用
        mirrorstar_core::pure_mode::assert_topmost_all();
        return LRESULT(0);
    }

    if msg == WM_DESTROY {
        // 防御性处理：外部销毁窗口时确保消息循环退出（沿用 explorer.rs 的写法）
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }

    def_window_proc(hwnd, msg, wparam, lparam)
}

// ── 低级钩子回调 ────────────────────────────────────────────────────────

/// 键盘低级钩子过程（`WH_KEYBOARD_LL`）
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_keyboard_event(code, wparam, lparam)
    }));
    match outcome {
        Ok(result) => result,
        Err(_) => {
            tracing::error!("键盘低级钩子回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
            call_next_hook(code, wparam, lparam)
        }
    }
}

/// 鼠标低级钩子过程（`WH_MOUSE_LL`）
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_mouse_event(code, wparam, lparam)
    }));
    match outcome {
        Ok(result) => result,
        Err(_) => {
            tracing::error!("鼠标低级钩子回调 panic 已被捕获，防止穿越 FFI 硬崩溃");
            call_next_hook(code, wparam, lparam)
        }
    }
}

/// 键盘事件处理（回调体，只访问原子量；无阻塞、无加锁、无日志）
fn handle_keyboard_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return call_next_hook(code, wparam, lparam);
    }
    // 吞掉窗口内：键盘事件一律吞掉（含触发键的 keyup，避免 Win 键抬起弹出开始菜单）
    if should_swallow() {
        return LRESULT(1);
    }
    // 宽限期内放行且不触发（避免托盘点击造成的瞬时进出）
    if should_trigger_keyboard(wparam.0) && !in_grace_period() && request_exit() {
        return LRESULT(1);
    }
    call_next_hook(code, wparam, lparam)
}

/// 鼠标事件处理（回调体，只访问原子量；无阻塞、无加锁、无日志）
fn handle_mouse_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return call_next_hook(code, wparam, lparam);
    }
    if should_swallow() {
        return LRESULT(1);
    }
    if should_trigger_mouse_click(wparam.0) && !in_grace_period() && request_exit() {
        return LRESULT(1);
    }
    // 鼠标移动：刷新空闲计时并恢复光标（非触发事件，继续放行）
    if is_mouse_move(wparam.0) {
        on_mouse_move();
    }
    call_next_hook(code, wparam, lparam)
}

/// 触发退出请求（原子 CAS 保证一次安装只触发一次）
///
/// 返回 `true` 表示本次调用是"首个触发者"（调用方据此吞掉该事件）；
/// 返回 `false` 表示已请求过（调用方放行，避免钩子卸载异常时永久吞掉输入）。
fn request_exit() -> bool {
    if EXIT_REQUESTED.swap(true, Ordering::AcqRel) {
        return false;
    }
    // 设置吞掉截止时间（触发时刻 + SWALLOW_MS）
    let deadline = elapsed_ms().unwrap_or(0) + SWALLOW_MS as i64;
    SWALLOW_UNTIL.store(deadline, Ordering::Release);
    // 把"通道通知 + 启动吞掉定时器"投递到消息循环上下文执行（回调内保持轻量）
    let raw = HIDDEN_HWND.load(Ordering::Acquire);
    if raw != 0 {
        let hwnd = HWND(raw as *mut _);
        if let Err(e) = unsafe { PostMessageW(hwnd, WM_APP_TRIGGER, WPARAM(0), LPARAM(0)) } {
            tracing::warn!(error = %e, "PostMessageW(WM_APP_TRIGGER) 失败，退出编排通知可能丢失");
        }
    } else {
        tracing::warn!("隐藏窗口句柄缺失，无法投递触发消息");
    }
    true
}

/// 通过通道通知上层执行退出编排（`send` 失败仅忽略：接收端已关闭说明已在退出流程中）
fn notify_exit(reason: ExitReason) {
    let sender = match EXIT_TX.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => {
            tracing::warn!(error = ?e, "EXIT_TX 锁中毒，无法通知上层退出编排");
            None
        }
    };
    match sender {
        Some(tx) => {
            if tx.send(reason).is_err() {
                tracing::debug!(?reason, "退出通知发送失败（接收端已关闭），忽略");
            }
        }
        None => tracing::warn!("退出通道未设置，无法通知上层退出编排"),
    }
}

/// 光标空闲检查（`WM_TIMER(TIMER_IDLE)`）：静止超时且未隐藏时隐藏一次
fn maybe_hide_cursor() {
    let Some(now) = elapsed_ms() else {
        return;
    };
    if now - LAST_MOVE_MS.load(Ordering::Acquire) < IDLE_HIDE_MS {
        return;
    }
    // swap 保证只调一次 ShowCursor(FALSE)，避免显示计数器累积为负
    if CURSOR_HIDDEN.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe { ShowCursor(FALSE) };
    tracing::debug!(idle_ms = IDLE_HIDE_MS, "光标静止超时，已隐藏");
}

/// 鼠标移动：刷新空闲计时；若光标已隐藏则立即恢复（与隐藏处成对，保证计数器平衡）
fn on_mouse_move() {
    LAST_MOVE_MS.store(elapsed_ms().unwrap_or(0), Ordering::Release);
    if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
        unsafe { ShowCursor(TRUE) };
    }
}

/// 卸载两个低级钩子（幂等：句柄原子地清零，重复调用为空操作）
fn unhook_both() {
    let raw_keyboard = KEYBOARD_HOOK.swap(0, Ordering::AcqRel);
    if raw_keyboard != 0 {
        if let Err(e) = unsafe { UnhookWindowsHookEx(HHOOK(raw_keyboard as *mut _)) } {
            tracing::warn!(error = %e, "卸载键盘低级钩子失败");
        }
    }
    let raw_mouse = MOUSE_HOOK.swap(0, Ordering::AcqRel);
    if raw_mouse != 0 {
        if let Err(e) = unsafe { UnhookWindowsHookEx(HHOOK(raw_mouse as *mut _)) } {
            tracing::warn!(error = %e, "卸载鼠标低级钩子失败");
        }
    }
}

/// 恢复光标（仅当此前隐藏过，保证与 `ShowCursor(FALSE)` 成对）
fn restore_cursor() {
    if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
        unsafe { ShowCursor(TRUE) };
    }
}

/// 是否处于进入模式的宽限期（基准缺失时按"已过宽限期"处理，保证用户随时能退出）
fn in_grace_period() -> bool {
    match elapsed_ms() {
        Some(now) => now - GRACE_BASE_MS.load(Ordering::Acquire) < GRACE_MS,
        None => false,
    }
}

/// 是否处于触发后的吞掉窗口内
fn should_swallow() -> bool {
    let until = SWALLOW_UNTIL.load(Ordering::Acquire);
    if until <= 0 {
        return false;
    }
    match elapsed_ms() {
        Some(now) => now < until,
        None => false,
    }
}

/// 是否需要"自愈"：已请求退出、吞掉窗口已过期、且钩子仍在 → 说明 `WM_APP_TRIGGER`
/// 投递丢失，必须由周期定时器补做收尾（否则模式无法退出）。
fn needs_self_heal() -> bool {
    EXIT_REQUESTED.load(Ordering::Acquire)
        && !should_swallow()
        && (KEYBOARD_HOOK.load(Ordering::Acquire) != 0 || MOUSE_HOOK.load(Ordering::Acquire) != 0)
}

/// 距"吞掉窗口"到期的剩余毫秒数（0 表示无需吞掉 / 已到期）。
///
/// 基于既有 [`SWALLOW_UNTIL`]（触发时刻 + [`SWALLOW_MS`]）与 [`elapsed_ms`] 单调时钟：
/// `remaining = deadline - now`，≤0 时返回 0；`elapsed_ms()` 返回 `None`（时钟基准未初始化）
/// 时返回 0（保守策略：不额外等待）。
///
/// 用途：[`InputHook::shutdown`] 在卸载钩子前按此值等待，保证触发输入（尤其 Win 键的
/// **keyup**，其**抬起**时才弹出开始菜单）仍被钩子吞掉后再卸载。
fn remaining_swallow_ms() -> u64 {
    let until = SWALLOW_UNTIL.load(Ordering::Acquire);
    if until <= 0 {
        return 0;
    }
    match elapsed_ms() {
        Some(now) => {
            let remain = until - now;
            if remain > 0 {
                remain as u64
            } else {
                0
            }
        }
        None => 0,
    }
}

/// 自进程单调时钟基准起经过的毫秒数（基准未初始化时返回 `None`，调用方按最宽松策略处理）
fn elapsed_ms() -> Option<i64> {
    MONO_BASE
        .get()
        .map(|base| base.elapsed().as_millis() as i64)
}

/// 键盘消息是否应触发退出（纯函数，便于单测）：`WM_KEYDOWN` / `WM_SYSKEYDOWN`
fn should_trigger_keyboard(wparam: usize) -> bool {
    let msg = wparam as u32;
    msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN
}

/// 鼠标消息是否应触发退出（纯函数，便于单测）：左/右/中键按下
fn should_trigger_mouse_click(wparam: usize) -> bool {
    let msg = wparam as u32;
    msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN
}

/// 是否为鼠标移动消息（用于空闲计时与光标恢复）
fn is_mouse_move(wparam: usize) -> bool {
    wparam as u32 == WM_MOUSEMOVE
}

/// `CallNextHookEx` 的安全包装（钩子回调统一在此放行事件）
fn call_next_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// `DefWindowProcW` 的安全包装（窗口过程统一在此退回默认处理）
fn def_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_KEYUP, WM_LBUTTONUP, WM_MBUTTONUP, WM_RBUTTONUP,
    };

    // ── 触发判定纯函数 ──────────────────────────────────────────────────

    /// 键盘按下（含系统键）应触发退出
    #[test]
    fn keyboard_down_messages_trigger() {
        assert!(should_trigger_keyboard(WM_KEYDOWN as usize));
        assert!(should_trigger_keyboard(WM_SYSKEYDOWN as usize));
    }

    /// 键盘抬起与其它消息不应触发退出
    #[test]
    fn keyboard_up_and_other_messages_do_not_trigger() {
        assert!(!should_trigger_keyboard(WM_KEYUP as usize));
        assert!(!should_trigger_keyboard(WM_MOUSEMOVE as usize));
        assert!(!should_trigger_keyboard(0));
    }

    /// 鼠标左/右/中键按下应触发退出
    #[test]
    fn mouse_click_messages_trigger() {
        assert!(should_trigger_mouse_click(WM_LBUTTONDOWN as usize));
        assert!(should_trigger_mouse_click(WM_RBUTTONDOWN as usize));
        assert!(should_trigger_mouse_click(WM_MBUTTONDOWN as usize));
    }

    /// 鼠标移动/抬起不应被判定为点击
    #[test]
    fn mouse_move_and_up_are_not_clicks() {
        assert!(!should_trigger_mouse_click(WM_MOUSEMOVE as usize));
        assert!(!should_trigger_mouse_click(WM_LBUTTONUP as usize));
        assert!(is_mouse_move(WM_MOUSEMOVE as usize));
        assert!(!is_mouse_move(WM_LBUTTONDOWN as usize));
    }

    /// 边界：三个判定纯函数只按 `wParam` 的消息类型比较，不做范围校验
    /// （`code < 0`、进入宽限期、吞掉窗口等前置条件均由调用点在纯函数之外处理）。
    /// 因此任意取值都必须安全返回且不 panic。
    #[test]
    fn trigger_predicates_handle_boundary_values_safely() {
        // 极端值不得 panic，也不得被误判为触发（均不等于目标消息常量）
        assert!(!should_trigger_keyboard(usize::MAX));
        assert!(!should_trigger_mouse_click(usize::MAX));
        assert!(!is_mouse_move(usize::MAX));
        assert!(!should_trigger_keyboard(0));
        assert!(!should_trigger_mouse_click(0));
        assert!(!is_mouse_move(0));
        assert!(!is_mouse_move(WM_KEYDOWN as usize));

        // 其余鼠标抬起消息同样不得被判为点击
        assert!(!should_trigger_mouse_click(WM_RBUTTONUP as usize));
        assert!(!should_trigger_mouse_click(WM_MBUTTONUP as usize));

        // 系统键按下（Alt 组合）与普通键按下一样必须触发
        assert!(should_trigger_keyboard(WM_SYSKEYDOWN as usize));
    }

    // ── 吞掉窗口剩余时长（竞态修复：shutdown 需等待它到期）──────────────────

    /// 未设置吞掉截止（`SWALLOW_UNTIL = 0`）时剩余时长为 0（不额外等待）
    #[test]
    fn remaining_swallow_ms_zero_when_not_armed() {
        // 先清掉其它测试可能遗留的截止时间，避免相互污染（本文件静态量跨测试共享）
        SWALLOW_UNTIL.store(0, Ordering::Release);
        assert_eq!(remaining_swallow_ms(), 0, "未触发时不应需要等待");

        // 复位，避免污染其它测试
        SWALLOW_UNTIL.store(0, Ordering::Release);
    }

    /// 截止时间设为 `now + 200ms` 时，剩余时长 > 0 且 ≤ 200；到期后回到 0
    #[test]
    fn remaining_swallow_ms_positive_within_deadline() {
        // 单调时钟基准：OnceLock 幂等（已初始化则复用），保证 elapsed_ms() 返回 Some
        let base = MONO_BASE.get_or_init(Instant::now);
        let now = base.elapsed().as_millis() as i64;

        SWALLOW_UNTIL.store(now + 200, Ordering::Release);
        let remain = remaining_swallow_ms();
        assert!(remain > 0, "截止时间未到，剩余时长应 > 0，实际 {remain}");
        assert!(remain <= 200, "剩余时长不应超过 200ms，实际 {remain}");

        // 复位，避免污染其它测试
        SWALLOW_UNTIL.store(0, Ordering::Release);
        assert_eq!(remaining_swallow_ms(), 0, "复位后应无需等待");
    }

    // ── 退出请求投递失败的兜底自愈（F3）─────────────────────────────────

    /// `needs_self_heal` 三态：
    /// ① 未请求退出 → false；
    /// ② 已请求退出且仍在吞掉窗口内 → false（等待吞掉窗口正常收尾）；
    /// ③ 已请求退出 + 吞掉窗口已过期 + 钩子句柄非 0 → true。
    ///
    /// 本测试操作进程级静态量（`EXIT_REQUESTED` / `SWALLOW_UNTIL` / `KEYBOARD_HOOK` /
    /// `MOUSE_HOOK`），设置前保存、断言后务必复位，避免污染其它测试。
    #[test]
    fn needs_self_heal_three_states() {
        // 保证 elapsed_ms() 返回 Some，使"吞掉中"状态可判定（OnceLock 幂等）
        let base = MONO_BASE.get_or_init(Instant::now);
        let now = base.elapsed().as_millis() as i64;

        // 保存原始值，断言后复位
        let saved_exit = EXIT_REQUESTED.load(Ordering::Acquire);
        let saved_swallow = SWALLOW_UNTIL.load(Ordering::Acquire);
        let saved_keyboard = KEYBOARD_HOOK.load(Ordering::Acquire);
        let saved_mouse = MOUSE_HOOK.load(Ordering::Acquire);

        // 状态①：未请求退出 → 即使钩子仍在也不需自愈
        EXIT_REQUESTED.store(false, Ordering::Release);
        SWALLOW_UNTIL.store(0, Ordering::Release);
        KEYBOARD_HOOK.store(1, Ordering::Release);
        assert!(!needs_self_heal(), "未请求退出时不应触发自愈");

        // 状态②：已请求退出且仍在吞掉窗口内 → 无需自愈
        EXIT_REQUESTED.store(true, Ordering::Release);
        SWALLOW_UNTIL.store(now + 10_000, Ordering::Release);
        assert!(!needs_self_heal(), "吞掉窗口未过期时不应触发自愈");

        // 状态③：已请求退出 + 吞掉窗口已过期 + 钩子句柄仍在 → 需要自愈
        SWALLOW_UNTIL.store(0, Ordering::Release);
        assert!(needs_self_heal(), "吞掉窗口过期且钩子仍在时应触发自愈");

        // 复位全部进程级静态量
        EXIT_REQUESTED.store(saved_exit, Ordering::Release);
        SWALLOW_UNTIL.store(saved_swallow, Ordering::Release);
        KEYBOARD_HOOK.store(saved_keyboard, Ordering::Release);
        MOUSE_HOOK.store(saved_mouse, Ordering::Release);
    }

    // ── 文档标记测试（沿用仓库 include_str! 断言惯例）─────────────────────

    /// 源码应包含输入捕获线程的关键契约标记：双低级钩子、吞掉窗口时长、
    /// 显示器变化广播、刻意不用 `HWND_MESSAGE`（其收不到广播）、`SetTimer` 计时。
    #[test]
    fn source_contains_hook_contract_markers() {
        let source = include_str!("pure_mode_input.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let kb_hook = ["WH_KEYBOARD", "_LL"].join("");
        let ms_hook = ["WH_MOUSE", "_LL"].join("");
        let swallow = ["SWALLOW", "_MS"].join("");
        let display_change = ["WM_DISPLAY", "CHANGE"].join("");
        let msg_only = ["HWND", "_MESSAGE"].join("");
        let set_timer = ["Set", "Timer"].join("");
        assert!(source.contains(&kb_hook), "应使用低级键盘钩子 {kb_hook}");
        assert!(source.contains(&ms_hook), "应使用低级鼠标钩子 {ms_hook}");
        assert!(
            source.contains(&swallow),
            "应定义吞掉触发输入的窗口时长 {swallow}"
        );
        assert!(
            source.contains(&display_change),
            "应处理 {display_change} 广播并请求退出"
        );
        assert!(
            source.contains(&msg_only),
            "注释应说明刻意不用 {msg_only}（消息专用窗口收不到 HWND_BROADCAST 广播）"
        );
        assert!(
            source.contains(&set_timer),
            "应使用 {set_timer} 实现吞掉窗口与空闲计时"
        );
    }

    /// 三处兜底契约（F3 自愈 / F4 光标复位 / F16 异常退出通知）的文档标记断言。
    #[test]
    fn source_contains_fallback_contract_markers() {
        let source = include_str!("pure_mode_input.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let self_heal_call = ["if needs_self", "_heal()"].join("");
        let show_true = ["ShowCursor", "(TRUE)"].join("");
        let cursor_store = ["CURSOR_HIDDEN.", "store(false"].join("");
        let hook_failed = ["ExitReason::", "HookFailed"].join("");
        let install_marker = ["fn ", "install("].join("");

        // F3：TIMER_IDLE 分支应调用自愈检查
        assert!(
            source.contains(&self_heal_call),
            "TIMER_IDLE 分支应调用 {self_heal_call} 兜底自愈丢失的退出请求"
        );

        // F4：install 段落中先 ShowCursor(TRUE) 恢复遗留隐藏光标，再复位 CURSOR_HIDDEN
        let install_at = source
            .find(&install_marker)
            .expect("源码应含 install 函数定义");
        let install_seg = &source[install_at..];
        let show_at = install_seg
            .find(&show_true)
            .expect("install 应调用 ShowCursor(TRUE) 恢复上一轮遗留的隐藏光标");
        let reset_at = install_seg
            .find(&cursor_store)
            .expect("install 应复位 CURSOR_HIDDEN 标志");
        assert!(
            show_at < reset_at,
            "install 中必须先 ShowCursor(TRUE) 再复位 CURSOR_HIDDEN（保证显示计数器平衡）"
        );

        // F16：线程收尾应向编排层投递 HookFailed 通知
        assert!(
            source.contains(&hook_failed),
            "cleanup_after_loop 应投递 {hook_failed} 通知编排层退出模式"
        );
    }

    // ── 显式卸载与异常退出的区分（A3）───────────────────────────────────

    /// `should_notify_hook_failed` 四态：异常退出+模式活跃 → 通知；用户已触发 / 模式不活跃 /
    /// 显式卸载 → 均不通知。
    #[test]
    fn should_notify_hook_failed_four_states() {
        assert!(
            should_notify_hook_failed(false, false, true),
            "异常退出且模式活跃：应通知"
        );
        assert!(
            !should_notify_hook_failed(false, true, true),
            "用户已触发退出：不通知"
        );
        assert!(
            !should_notify_hook_failed(false, false, false),
            "模式不活跃：不通知"
        );
        assert!(
            !should_notify_hook_failed(true, false, true),
            "显式卸载：不通知（应用退出路径）"
        );
    }

    /// A3 契约的源码标记：显式卸载标志的复位/置位、线程收尾的退出通道释放、
    /// `cleanup_after_loop` 改用纯函数判定。运行期拼接搜索串避免自匹配。
    #[test]
    fn source_contains_explicit_shutdown_and_release_markers() {
        let source = include_str!("pure_mode_input.rs");
        let reset = ["EXPLICIT_", "SHUTDOWN.store(false"].join("");
        let set = ["EXPLICIT_", "SHUTDOWN.store(true"].join("");
        let release = ["fn release_", "exit_tx"].join("");
        let notify_use = ["should_notify_", "hook_failed(\n"].join(""); // 调用点（换行后的参数列表）
        assert!(
            source.contains(&reset),
            "install 首部应复位 {reset}（跨轮次一致性）"
        );
        assert!(source.contains(&set), "InputHook::shutdown 应置位 {set}");
        assert!(
            source.contains(&release),
            "线程收尾应释放退出通道（{release}）"
        );
        assert!(
            source.contains(&notify_use),
            "cleanup_after_loop 应改用纯函数判定 {notify_use}"
        );
    }
}
