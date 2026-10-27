//! 纯净壁纸模式编排（src-tauri 侧，实现契约 D7 与 ADDED Requirement「托盘入口」）。
//!
//! # 模块职责
//!
//! 把「托盘勾选项 / 输入捕获线程」与「core 引擎的纯壁纸模式原语」串成两条编排链路：
//!
//! - **进入链路**（[`toggle`] → 安装输入钩子 → **关闭托盘溢出浮出窗** →
//!   `WallpaperEngine::enter_pure_mode` → 置勾选）：任一步失败都回滚并保持未勾选，
//!   **拒绝进入**（避免"进得去出不来"）；关闭浮出窗一步为 best-effort，失败不阻断。
//! - **退出链路**（[`exit_orchestration`]：`WallpaperEngine::exit_pure_mode` 回嵌桌面 →
//!   卸载输入钩子线程 → 取消勾选 → 补跑全屏/电源判定 → 存活校验 + 自愈 → 唤醒轮换
//!   调度器）：由输入捕获线程的退出消息与托盘二次点击共用，内部幂等。
//!
//! # 非阻塞保障
//!
//! 托盘菜单事件在**主线程**派发，任何阻塞调用（`install` / `InputHook::shutdown` /
//! `CheckMenuItem::set_checked` / `reevaluate_now`）都不得在主线程执行：
//!
//! - [`toggle`] 立即 `spawn` 到异步 runtime，主线程只做一次派发后返回；
//! - `install` / `InputHook::shutdown` / `set_checked` / `reevaluate_now` 全部经
//!   `tokio::task::spawn_blocking` 执行——`set_checked` 内部 `run_item_main_thread!`
//!   会等待主线程完成，在主线程直接调用即死锁。
//!
//! # 顺序约束
//!
//! 退出时必须**先恢复桌面（`exit_pure_mode` 回嵌 WorkerW）再卸载输入钩子**：
//! 否则钩子卸载后到回嵌完成之间，用户输入会落到仍处于置顶全屏状态的壁纸窗口上。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use mirrorstar_core::LivenessProbe;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetWindowThreadProcessId, IsHungAppWindow, LoadCursorW, SetCursor,
    WindowFromPoint, CURSORINFO, IDC_ARROW, IDC_WAIT,
};

use crate::platform::liveness_watchdog::{confirmed_stalls, stalled_display_ids};
use crate::platform::pure_mode_input::{self, ExitReason, InputHook};
use crate::state::AppState;

/// 当前安装的输入捕获句柄（进程内一次只允许一个捕获线程）。
///
/// `Mutex` 中毒（线程 panic）时用 `unwrap_or_else(|e| e.into_inner())` 继续取用，
/// 避免"中毒即彻底无法卸载钩子"导致残留低级钩子与隐藏窗口。
static INPUT_HOOK: Mutex<Option<InputHook>> = Mutex::new(None);

/// 退出编排进行中标志（幂等守卫）：并发退出请求（钩子线程消息 + 托盘二次点击）
/// 只有一个真正执行回嵌与卸载。
static EXIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 进入编排进行中标志（F2 幂等守卫）：快速双击托盘等并发进入请求只有一个真正执行安装与置顶。
static ENTER_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 编排互斥守卫的 RAII 复位器（进入 / 退出编排共用）。
///
/// 进入与退出的守卫完全同构：无论编排正常结束、提前返回还是 panic，都在 Drop 时复位
/// 携带的目标原子标志（`Release`），避免标志残留导致后续「点了没反应」或无法再次退出。
/// 两处编排分别以 `&EXIT_IN_PROGRESS` / `&ENTER_IN_PROGRESS` 构造。
struct OrchestrationGuard(&'static AtomicBool);

impl Drop for OrchestrationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 检测系统等待态（`IDC_WAIT`）的轮询间隔（毫秒）。
///
/// 覆盖窗 `SetWindowPos(SWP_SHOWWINDOW)` 后 Windows 才把光标切到 `IDC_WAIT`；
/// 固定延迟会留下可感知的等待指针窗口。改为短间隔轮询，检测到等待态立即复位，
/// 把可见等待压缩到 1 帧以内。
const CURSOR_WAIT_POLL_INTERVAL_MS: u64 = 2;

/// 等待系统进入等待态的最长轮询时间（毫秒）。
///
/// 正常显示窗口后系统在一个输入周期内设置等待态；250ms 足够覆盖慢机/低帧率。
/// 超时说明鼠标不在覆盖窗上（或系统未设置等待态），无需继续挂接。
const CURSOR_WAIT_POLL_MAX_MS: u64 = 250;

/// 是否应把当前线程挂接到目标线程以复位光标（纯函数，便于单测）。
///
/// 目标线程为 0（取不到线程）、与当前线程相同（当前线程已拥有该输入队列，直接
/// `SetCursor` 即可、无需挂接）或目标窗口无响应（挂接可能一起挂死）时返回 false。
fn should_attach_cursor_thread(target_tid: u32, current_tid: u32, target_hung: bool) -> bool {
    target_tid != 0 && target_tid != current_tid && !target_hung
}

/// 复位纯净模式覆盖窗的光标；返回是否真正执行了挂接 + `SetCursor`。
///
/// # 根因
///
/// 纯净模式的视频覆盖窗由后台线程创建、带 `WS_EX_NOACTIVATE` 并以
/// `SWP_NOACTIVATE` 置顶（D1 契约：不抢前台）。Windows 会在这类"后台出现的全屏
/// 顶层窗口"上显示系统 `IDC_WAIT`（Win11 的纯蓝圈；系统侧表现为启动中/忙碌状态）；
/// 覆盖窗线程不属于前台输入队列，`WM_SETCURSOR` / `SetCursor` 无法改写光标形状，
/// 蓝圈会一直持续到覆盖窗隐藏。
///
/// # 修复
///
/// 把当前线程临时 `AttachThreadInput` 到覆盖窗所属线程（即鼠标下窗口的输入队列），
/// `SetCursor(IDC_ARROW)` 后立即解除挂接。实测（真机）可在**不抢前台**的前提下把
/// `IDC_WAIT` 复位为箭头，解除挂接后保持。
///
/// 只处理"鼠标下窗口本身是本次纯净模式置顶窗口"的情形；桌面 / 任务栏 / 用户前台
/// 窗口 / mpv 子窗口一律不碰。失败或目标线程无响应只记录日志，不阻断进入。
fn reset_pure_mode_cursor() -> bool {
    // 并发退出时置顶注册表可能已清空，直接跳过。
    if !mirrorstar_core::pure_mode::is_active() {
        return false;
    }
    let lifted = mirrorstar_core::pure_mode::lifted_hwnds();
    if lifted.is_empty() {
        return false;
    }

    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return false;
    }
    let hover = unsafe { WindowFromPoint(point) };
    if hover.is_invalid() {
        return false;
    }
    // 只碰本次纯净模式注册过的顶层窗口（视频覆盖宿主 / 图片 / GIF / 网页宿主）。
    if !lifted.iter().any(|hwnd| hwnd.0 == hover.0) {
        tracing::debug!(
            hover = ?hover,
            "纯净模式光标复位：鼠标不在覆盖窗上，跳过"
        );
        return false;
    }

    let target_tid = unsafe { GetWindowThreadProcessId(hover, None) };
    let current_tid = unsafe { GetCurrentThreadId() };
    if !should_attach_cursor_thread(target_tid, current_tid, unsafe {
        IsHungAppWindow(hover).as_bool()
    }) {
        tracing::debug!(
            target_tid,
            current_tid,
            "纯净模式光标复位：目标线程不可挂接，跳过"
        );
        return false;
    }

    let arrow = match unsafe { LoadCursorW(None, IDC_ARROW) } {
        Ok(cursor) => cursor,
        Err(e) => {
            tracing::warn!(error = %e, "纯净模式光标复位：加载箭头光标失败");
            return false;
        }
    };

    // 挂接失败不重试：光标会停留在系统等待态，但不影响模式本身。
    if !unsafe { AttachThreadInput(current_tid, target_tid, true) }.as_bool() {
        tracing::warn!(target_tid, "纯净模式光标复位：AttachThreadInput 挂接失败");
        return false;
    }
    let previous = unsafe { SetCursor(arrow) };
    // 必须解除挂接，避免长期共享输入队列。
    if !unsafe { AttachThreadInput(current_tid, target_tid, false) }.as_bool() {
        tracing::warn!(target_tid, "纯净模式光标复位：AttachThreadInput 解除失败");
    }
    tracing::info!(
        target_tid,
        previous_cursor = previous.0 as isize,
        "纯净模式光标复位：覆盖窗光标已从系统等待态复位为箭头"
    );
    true
}

/// 进入纯净模式后轮询系统光标：一旦出现 `IDC_WAIT` 且鼠标位于覆盖窗上，立即复位。
///
/// 只在进入时运行一次；模式退出（`is_active=false`）立即返回。未观察等待态
/// （鼠标不在覆盖窗 / 系统未设置等待态）则超时退出，不反复挂接。
fn reset_pure_mode_cursor_when_waiting() {
    let wait_cursor = match unsafe { LoadCursorW(None, IDC_WAIT) } {
        Ok(cursor) => cursor,
        Err(e) => {
            tracing::warn!(error = %e, "纯净模式光标轮询：加载等待光标失败");
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_millis(CURSOR_WAIT_POLL_MAX_MS);
    while Instant::now() < deadline {
        if !mirrorstar_core::pure_mode::is_active() {
            return;
        }
        let mut info = CURSORINFO::default();
        info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if unsafe { GetCursorInfo(&mut info) }.is_ok() && info.hCursor == wait_cursor {
            // 等待态已出现：复位一次；若鼠标不在覆盖窗上则立即结束，不做无意义挂接。
            let _ = reset_pure_mode_cursor();
            return;
        }
        std::thread::sleep(Duration::from_millis(CURSOR_WAIT_POLL_INTERVAL_MS));
    }
    tracing::debug!("纯净模式光标轮询：未观察到系统等待态，跳过");
}

/// 托盘点击入口：按**权威状态**决定进入或退出。
///
/// 刻意不依赖菜单勾选态——原生菜单点击会自动翻转勾选（`CheckMenuItem` 的默认行为），
/// 该值与真实运行态可能不一致，只有引擎 `pure_mode.is_some()` / 进程级标志才是权威。
///
/// 托盘事件在主线程派发，本函数立即 `spawn` 到异步 runtime，**不阻塞主线程**。
pub(crate) fn toggle(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if is_pure_mode_active(&app).await {
            tracing::info!("托盘点击『纯净壁纸』：当前已在模式中，执行退出编排");
            exit_orchestration(app).await;
        } else {
            tracing::info!("托盘点击『纯净壁纸』：执行进入编排");
            enter_orchestration(app).await;
        }
    });
}

/// 查询是否处于纯净壁纸模式（引擎权威状态优先，退化到进程级标志）。
///
/// `WallpaperEngine::pure_mode.is_some()` 是权威运行态；`mirrorstar_core::pure_mode::is_active()`
/// 是进程级标志（置位早于置顶、回嵌完成后清除），两者在编排期间可能出现短暂不一致，
/// 故优先读引擎状态，仅在 `AppState` 不可用（理论上 setup 前）时退化为读标志。
async fn is_pure_mode_active(app: &AppHandle) -> bool {
    let engine = app
        .try_state::<AppState>()
        .map(|state| state.wallpaper_engine.clone());
    match engine {
        Some(engine) => engine.lock().await.is_pure_mode_active(),
        None => mirrorstar_core::pure_mode::is_active(),
    }
}

/// 托盘溢出浮出窗（任务栏"隐藏栏"）的窗口类名：Win11 由 XAML island 承载，
/// Win10 及部分旧版为经典窗口。两者都按**类名精确匹配**（实机取证见
/// `docs/04-优化文档/03-desktop模块.md`）。
const TRAY_OVERFLOW_CLASS_WIN11: &str = "TopLevelWindowForOverflowXamlIsland";
const TRAY_OVERFLOW_CLASS_LEGACY: &str = "NotifyIconOverflowWindow";

/// 关闭托盘溢出浮出窗（任务栏"隐藏栏"，best-effort）。
///
/// # 为什么需要
///
/// 溢出浮出窗由 Explorer 拥有，正常情况下依靠"失去激活 / 用户点击别处"自动关闭。
/// 而纯净壁纸模式的进入编排**刻意不抢前台**（D1 契约：只做摘出 + `HWND_TOPMOST`，
/// 壁纸窗口带 `WS_EX_NOACTIVATE`），菜单点选后没有任何窗口接管激活 ⇒ 浮出窗收不到
/// 关闭信号，会残留在屏幕上叠在壁纸之上，破坏"桌面被壁纸完全覆盖"的沉浸目标。
///
/// # 手段
///
/// 1. `EnumWindows` 枚举顶层窗口，按类名精确匹配 [`TRAY_OVERFLOW_CLASS_WIN11`] /
///    [`TRAY_OVERFLOW_CLASS_LEGACY`]，只处理**当前可见**的命中窗口；
/// 2. 先 `PostMessageW(WM_CLOSE)`：走 Explorer 的正常关闭流程，不留"窗口仍在只是被
///    隐藏"的残留态；
/// 3. 对每个命中窗口约 120ms 后复核 `IsWindowVisible`，仍可见则 `ShowWindow(SW_HIDE)` 兜底。
///
/// 全程 best-effort：枚举不到已知类名、投递消息失败、关闭后仍可见，都只记日志、
/// **不阻断进入**；也不抢前台、不模拟键鼠输入、不触碰主托盘与任务栏。
///
/// 等待时长随命中窗口数量线性增长（逐个命中窗口各约 120ms），只可在阻塞线程池调用
/// （见 `enter_orchestration` 调用点）。
fn dismiss_tray_overflow_flyout() {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, IsWindowVisible, PostMessageW, ShowWindow, SW_HIDE, WM_CLOSE,
    };

    /// `EnumWindows` 回调：把类名命中且当前可见的窗口写入 `lparam` 指向的 `Vec`。
    ///
    /// 裸指针 + `LPARAM` 传参沿用仓库既有 `EnumWindows` 惯例（回调为 `extern "system"`，
    /// 无法捕获环境变量）；回调恒返回 `TRUE` 继续枚举，故不会触发 `EnumWindows` 的 Err 误报。
    unsafe extern "system" fn collect_visible_overflow(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        if len == 0 {
            return BOOL(1);
        }
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        if class != TRAY_OVERFLOW_CLASS_WIN11 && class != TRAY_OVERFLOW_CLASS_LEGACY {
            return BOOL(1);
        }
        if IsWindowVisible(hwnd).as_bool() {
            // SAFETY: lparam 指向调用方栈上存活的 hits，回调仅在本次 EnumWindows 内同步执行。
            (*(lparam.0 as *mut Vec<(HWND, String)>)).push((hwnd, class));
        }
        BOOL(1)
    }

    let mut hits: Vec<(HWND, String)> = Vec::new();
    // SAFETY: lparam 指向本函数栈上存活的 hits；EnumWindows 同步回调，返回前指针不会逃逸。
    if let Err(e) = unsafe {
        EnumWindows(
            Some(collect_visible_overflow),
            LPARAM(&mut hits as *mut Vec<(HWND, String)> as isize),
        )
    } {
        tracing::warn!(error = %e, "枚举顶层窗口失败，跳过关闭托盘溢出浮出窗");
        return;
    }
    if hits.is_empty() {
        tracing::debug!("未发现可见的托盘溢出浮出窗（隐藏栏未打开），无需关闭");
        return;
    }

    for (hwnd, class) in hits {
        // ① WM_CLOSE：让 Explorer 走正常关闭流程
        // SAFETY: PostMessageW 只投递消息，不阻塞调用线程、不改变调用线程状态。
        let posted = unsafe { PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok();
        if !posted {
            tracing::warn!(class = %class, "向托盘溢出浮出窗投递 WM_CLOSE 失败，改用 SW_HIDE 兜底");
        }
        // 等待 Explorer 处理关闭消息（浮出窗关闭动画通常 <120ms）
        std::thread::sleep(std::time::Duration::from_millis(120));

        // ② 复核：已不可见即视为关闭成功
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            tracing::info!(class = %class, "托盘溢出浮出窗已关闭（WM_CLOSE 生效）");
            continue;
        }
        // SAFETY: ShowWindow 只改变目标窗口的可见性，作用于其它进程窗口是 Win32 允许的。
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            tracing::warn!(class = %class, "托盘溢出浮出窗 WM_CLOSE 与 SW_HIDE 均未生效（窗口仍可见）");
        } else {
            tracing::info!(class = %class, "托盘溢出浮出窗已隐藏（WM_CLOSE 未生效，SW_HIDE 兜底）");
        }
    }
}

/// 进入编排（全异步），真实步骤顺序（编号与函数内注释一致）：
/// 第 0 步 进入互斥守卫（F2，`ENTER_IN_PROGRESS` + RAII `OrchestrationGuard`）→
/// 第 1 步 幂等检查（`pure_mode::is_active()`）→ 等待在途退出编排落位（A2）→
/// 安装输入钩子 → 关闭托盘溢出浮出窗（best-effort，必须在引擎置顶之前）→ 引擎进入 →
/// 存储句柄前复核模式活跃 → **同步置勾选 → 启动退出监听**
/// （置勾选必须早于启动退出监听，B5 不变量）。
///
/// 失败即回滚并保持未勾选（拒绝进入），每一步都有明确日志，便于排查"点了没反应"。
///
/// # 防跨代复位约束（A2）
///
/// - **等待在途退出编排落位**：退出编排卸载输入钩子时会先等待"吞掉窗口"到期（≤300ms）
///   再 join 旧捕获线程，期间进程级原子量仍由旧线程使用。必须先等 `EXIT_IN_PROGRESS`
///   释放（含 join 旧线程的卸载步骤已完成）再安装新钩子，满足 `pure_mode_input` 的
///   "先 shutdown 再 install"契约；否则新 `install` 跨代复位静态量，旧线程收尾可能误卸
///   新会话钩子或误投 `HookFailed`。
/// - **存储句柄前复核模式活跃**：引擎进入成功到存储句柄之间，在途退出编排可能已完成
///   （输入触发 / DisplayChange / HookFailed）并清除了进程级标志；此时若仍存句柄，退出
///   编排的槽位卸载（可能已执行）将无法覆盖它，留下"模式已退出但输入钩子仍在"的残留。
///   故存储前复核模式活跃，不活跃则卸载新钩子并直接返回（不置勾选）。
async fn enter_orchestration(app: AppHandle) {
    // 0. 进入互斥守卫（F2）：并发进入请求（快速双击托盘）只允许一个真正执行；
    //    无论后续哪条路径返回，RAII 守卫都会复位标志。
    if ENTER_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        tracing::debug!("纯净壁纸模式进入编排已在进行中，跳过重复进入");
        return;
    }
    let _guard = OrchestrationGuard(&ENTER_IN_PROGRESS);

    // 1. 幂等守卫：进程级标志已置位（模式运行中/正在进入）直接返回
    if mirrorstar_core::pure_mode::is_active() {
        tracing::info!("纯净壁纸模式已激活，跳过重复进入");
        return;
    }

    // 2. 串行化等待（A2）：退出编排卸载输入钩子时会先等待"吞掉窗口"到期（≤300ms）再
    //    join 旧捕获线程，期间进程级原子量仍由旧线程使用。必须先等退出编排完全落位
    //    （EXIT_IN_PROGRESS 释放 = 含 join 旧线程的卸载步骤已完成），再安装新钩子，
    //    满足 pure_mode_input 的"先 shutdown 再 install"契约。
    const EXIT_WAIT_TICK: std::time::Duration = std::time::Duration::from_millis(50);
    // 退出编排现含存活校验与可能的自愈（mpv 重建可达数秒），等待窗口需覆盖该时长，
    // 否则用户在退出尾段点击「进入」会被误拒。
    const EXIT_WAIT_TRIES: u32 = 200; // ≈10s 上界
    let mut waited_for_exit = false;
    for _ in 0..EXIT_WAIT_TRIES {
        if !EXIT_IN_PROGRESS.load(Ordering::Acquire) {
            break;
        }
        if !waited_for_exit {
            waited_for_exit = true;
            tracing::info!("检测到在途退出编排，先等待其完成（避免跨代复位输入钩子状态）");
        }
        tokio::time::sleep(EXIT_WAIT_TICK).await;
    }
    if EXIT_IN_PROGRESS.load(Ordering::Acquire) {
        tracing::warn!(
            "等待在途退出编排完成超时（≈10s，含存活校验/自愈），拒绝本次进入（保持未勾选）"
        );
        set_checked(&app, false);
        return;
    }

    let Some(state) = app.try_state::<AppState>() else {
        tracing::warn!("AppState 不可用，无法进入纯净壁纸模式（setup 未完成？）");
        return;
    };
    let engine = state.wallpaper_engine.clone();

    // 3. 建退出原因通道（输入钩子线程 → 退出监听任务）
    let (exit_tx, mut exit_rx) = tokio::sync::mpsc::unbounded_channel::<ExitReason>();

    // 4. 安装输入捕获钩子（阻塞最长 5s：等待钩子线程回传安装结果，必须在阻塞线程池执行）
    let hook = match tokio::task::spawn_blocking(move || pure_mode_input::install(exit_tx)).await {
        Ok(Ok(hook)) => hook,
        Ok(Err(e)) => {
            // 键盘钩子安装失败 → 拒绝进入（避免进得去出不来）
            tracing::warn!(error = %e, "输入捕获安装失败，拒绝进入纯净壁纸模式（保持未勾选）");
            set_checked(&app, false);
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "输入捕获安装任务 join 失败，拒绝进入纯净壁纸模式");
            set_checked(&app, false);
            return;
        }
    };

    // 4.5 关闭托盘溢出浮出窗（best-effort）
    //
    // 进入编排刻意不抢前台（D1 契约），浮出窗收不到"失去激活"关闭信号 ⇒ 会残留并叠在
    // 壁纸上；必须在壁纸置顶（第 5 步引擎进入）**之前**关闭。失败仅 warn、不阻断进入。
    // 等待时长随命中窗口数量线性增长（逐个命中窗口各约 120ms），经阻塞线程池执行，
    // 避免阻塞异步运行时线程。
    if let Err(e) = tokio::task::spawn_blocking(dismiss_tray_overflow_flyout).await {
        tracing::warn!(error = %e, "关闭托盘溢出浮出窗任务 join 失败（不阻断进入）");
    }

    // 5. 引擎进入（摘出 WorkerW + 置顶全屏；无壁纸时返回 InvalidArgument）
    let enter_result = engine.lock().await.enter_pure_mode();
    match enter_result {
        Err(e) => {
            // 回滚：卸载刚安装的钩子（engine 侧 enter_pure_mode 内部已自行回滚已置顶窗口）
            let rollback = tokio::task::spawn_blocking(move || {
                let mut hook = hook;
                hook.shutdown();
            })
            .await;
            if let Err(e) = rollback {
                tracing::warn!(error = %e, "回滚输入钩子任务 join 失败");
            }
            tracing::warn!(
                error = %e,
                "进入纯净壁纸模式失败（引擎拒绝，可能未设置任何壁纸），已卸载输入钩子并保持未勾选"
            );
            set_checked(&app, false);
        }
        Ok(()) => {
            // 5.5 复位覆盖窗光标（根因见 `reset_pure_mode_cursor` 文档）。
            //
            // 覆盖窗已置顶；Windows 随后把光标切到 IDC_WAIT（纯蓝圈），而覆盖窗线程
            // 不属于前台输入队列，无法自行改回箭头。立即以 2ms 间隔轮询，检测到等待态
            // 后挂接覆盖窗线程复位；任务失败/模式已退出都不影响进入主流程，故不 await。
            tauri::async_runtime::spawn(async {
                let _ = tokio::task::spawn_blocking(reset_pure_mode_cursor_when_waiting).await;
            });

            // 6. 存句柄（进程内唯一槽位，供退出编排与进程退出清理卸载）
            //    F2 防御性：先 take 出旧句柄并在**释放锁之后** shutdown（其内部 join 线程会阻塞，
            //    不得持锁调用），保证槽位唯一、旧捕获线程不泄漏；随后再存入新句柄。
            let stored = tokio::task::spawn_blocking(move || {
                let old = {
                    let mut slot = INPUT_HOOK.lock().unwrap_or_else(|e| e.into_inner());
                    slot.take()
                };
                if let Some(mut old_hook) = old {
                    tracing::warn!("检测到既有输入捕获句柄，先卸载旧句柄再存入新句柄（防止钩子线程泄漏）");
                    old_hook.shutdown();
                }
                // A2 存储校验：引擎进入成功到此处之间，在途退出编排可能已完成（输入触发 /
                // DisplayChange / HookFailed），此时进程级标志已被清除——若仍存句柄，
                // 退出编排的槽位卸载（可能已执行）将无法覆盖它，留下"模式已退出但输入钩子
                // 仍在"的残留。故不存句柄，直接卸载新钩子。
                if !mirrorstar_core::pure_mode::is_active() {
                    tracing::warn!("存储输入捕获句柄前检测到模式已不活跃（在途退出编排已完成），不存句柄并卸载新钩子");
                    let mut hook = hook;
                    hook.shutdown();
                    return false;
                }
                let mut slot = INPUT_HOOK.lock().unwrap_or_else(|e| e.into_inner());
                *slot = Some(hook);
                true
            })
            .await;
            match stored {
                Ok(true) => {}
                Ok(false) => {
                    // 模式已由在途退出编排恢复：不启动退出监听、不置勾选（勾选态已由退出编排取消）
                    return;
                }
                Err(e) => {
                    // 闭包 panic：句柄未入槽位（已随任务释放）。此时模式仍活跃而无输入钩子，
                    // 必须走完整退出编排恢复桌面，避免"进得去出不来"。
                    tracing::warn!(error = %e, "存储输入捕获句柄任务 join 失败，执行退出编排恢复桌面");
                    exit_orchestration(app).await;
                    return;
                }
            }
            tracing::info!("已进入纯净壁纸模式（按任意键 / 点击鼠标 / 托盘二次点击退出）");

            // 7. 先同步置勾选，再启动退出监听（B5）。
            //
            //    `set_checked` 内部是 fire-and-forget 的 `tokio::task::spawn_blocking`，不保证
            //    完成顺序：若先启动退出监听，退出事件可能立刻触发 `exit_orchestration` 的
            //    `set_checked(false)`，两条置位任务在阻塞池上竞争，最终勾选态可能错位停在
            //    true（托盘 `toggle` 刻意不读勾选态，错位不会被纠正）。故此处先**同步完成**
            //    置位再启动监听。
            //
            //    前提：`exit_rx` 为 `unbounded_channel`，监听启动前到达的退出事件已在通道中
            //    缓冲、不会丢失；且本次置位同步完成后，退出触发的 `set_checked(false)` 一定
            //    晚于本次置位。
            set_checked_and_wait(&app, true).await;

            // 8. 退出监听任务：一轮 install 只服务一次退出（退出后钩子已卸载，
            //    后续事件无意义；`exit_orchestration` 幂等，接收端关闭后 send 失败仅记录 debug）
            let app_for_task = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(reason) = exit_rx.recv().await {
                    tracing::info!(?reason, "纯净壁纸模式：收到退出请求");
                    exit_orchestration(app_for_task).await;
                }
            });
        }
    }
}

/// 退出编排（供输入钩子线程消息、托盘二次点击共用）；内部幂等。
///
/// 步骤（顺序不可颠倒）：
/// 1. **先恢复桌面**：`engine.exit_pure_mode()` 逐个嵌回 WorkerW（幂等）；
/// 2. 卸载输入钩子线程（`PostThreadMessageW(WM_QUIT)` + join）；
/// 3. **同步取消托盘勾选**（用户可见态尽早一致）；
/// 4. 补跑一次全屏 / 电源判定（模式期间处置动作被守卫跳过，状态可能漂移）；
/// 5. 视频渲染器存活校验 + 自愈（含暂停短路）；
/// 6. 唤醒轮换调度器重算 deadline。
pub(crate) async fn exit_orchestration(app: AppHandle) {
    // 幂等守卫：已在进行中则直接返回（第二个退出请求无需重复回嵌/卸载）
    if EXIT_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        tracing::debug!("纯净壁纸模式退出编排已在进行中，跳过重复退出");
        return;
    }
    // 无论正常结束、提前返回还是 panic，均在 Drop 时复位标志
    let _guard = OrchestrationGuard(&EXIT_IN_PROGRESS);
    // 临时诊断埋点：整个退出编排总耗时起点
    let trace_total = Instant::now();

    // 1. 先恢复桌面（回嵌 WorkerW）：必须在卸载钩子之前，否则会出现"无钩子但壁纸仍置顶"的窗口
    match app
        .try_state::<AppState>()
        .map(|state| state.wallpaper_engine.clone())
    {
        Some(engine) => {
            // 临时诊断埋点：取引擎锁等待耗时
            let trace_lock = Instant::now();
            let mut guard = engine.lock().await;
            tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=lock_wait ms={}", trace_lock.elapsed().as_millis());
            // 临时诊断埋点：exit_pure_mode（回嵌桌面 + 销毁临时覆盖窗）耗时
            let trace_step1 = Instant::now();
            if let Err(e) = guard.exit_pure_mode() {
                tracing::warn!(error = %e, "退出纯净壁纸模式时引擎返回错误（继续卸载钩子并补跑判定）");
            }
            tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step1_exit_pure_mode ms={}", trace_step1.elapsed().as_millis());
        }
        None => tracing::warn!("AppState 不可用，跳过引擎回嵌（无法执行 exit_pure_mode）"),
    }

    // 2. 卸载输入钩子线程（阻塞：PostThreadMessageW + join，必须在阻塞线程池执行）
    // 临时诊断埋点：卸载输入钩子耗时
    let trace_step2 = Instant::now();
    if let Err(e) = tokio::task::spawn_blocking(shutdown_input_hook).await {
        tracing::warn!(error = %e, "卸载输入钩子任务 join 失败");
    }
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step2_input_hook ms={}", trace_step2.elapsed().as_millis());

    // 3. 取消勾选（内部 spawn_blocking，非阻塞）
    //    勾选态代表"模式是否活跃"，回嵌 + 卸载钩子完成后模式已实际退出；提前同步可避免
    //    后续存活校验/自愈（可能持续数秒）期间勾选态滞后，且校验失败也不影响勾选态一致性。
    // 临时诊断埋点：取消勾选耗时
    let trace_step3 = Instant::now();
    set_checked(&app, false);
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step3_set_checked ms={}", trace_step3.elapsed().as_millis());

    // 4. 补跑全屏 / 电源判定，纠正模式期间冻结的状态漂移（两处均为阻塞 Win32 查询/处置）
    // 临时诊断埋点：补跑全屏/电源判定耗时
    let trace_step4 = Instant::now();
    let reevaluate = tokio::task::spawn_blocking(move || {
        crate::platform::fullscreen::reevaluate_now();
        crate::platform::power::reevaluate_now();
    })
    .await;
    if let Err(e) = reevaluate {
        tracing::warn!(error = %e, "补跑全屏/电源判定任务 join 失败");
    }
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step4_reevaluate ms={}", trace_step4.elapsed().as_millis());

    // 5. 视频渲染器存活校验 + 自愈（本次 bug 主修复）
    //
    // 放在补跑判定之后：补跑可能按用户配置（fullscreen_action）终止壁纸，先判定再校验，
    // 避免"刚自愈又被终止"；放在唤醒调度器之前：保证退出完成时画面是活的。
    // 失败/停滞自愈失败均不阻断后续步骤（内部仅记日志）。
    // 临时诊断埋点：存活校验 + 自愈耗时
    let trace_step5 = Instant::now();
    liveness_check_and_heal(&app).await;
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step5_liveness ms={}", trace_step5.elapsed().as_millis());

    // 6. 唤醒调度器重算轮换 deadline（模式期间排期被抑制，退出后需重新计算）
    // 临时诊断埋点：唤醒调度器耗时
    let trace_step6 = Instant::now();
    match app.try_state::<AppState>() {
        Some(state) => state.scheduler.wake.notify_waiters(),
        None => tracing::warn!("AppState 不可用，无法唤醒调度器重算轮换 deadline"),
    }
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=step6_notify ms={}", trace_step6.elapsed().as_millis());

    // 临时诊断埋点：退出编排总耗时
    tracing::info!(target: "mirrorstar::exit_trace", "EXIT-TRACE ph=total ms={}", trace_total.elapsed().as_millis());
    tracing::info!("纯净壁纸模式退出编排完成");
}

/// 退出纯净壁纸模式后的视频渲染器存活校验 + 自愈。
///
/// 纯净壁纸模式的核心价值是"让用户看见壁纸"，因此退出时必须保证画面是活的：
/// mpv 视频输出偶发在循环点停滞（最后一帧 drain 未完成，画面钉在最后一帧），
/// 退出编排原先只回嵌窗口、从不校验渲染器是否真的在播放，导致冻结被"带"到退出之后。
///
/// 判定：对"应为 Playing"的视频渲染器采样两次（间隔 500ms），`time_pos` 未前进
/// 且未暂停即判定停滞；为避免极低帧率内容（帧间隔 > 采样窗口）造成误判，
/// 仅当首对采样命中时才追加第三次采样（再等 400ms）做确认，两段区间都"钉住"
/// 才执行自愈。无视频渲染器时不产生任何额外等待。
///
/// 全部引擎交互（含 MPV 属性查询与 `sleep`）都在 `spawn_blocking` 内执行：异步运行时
/// 线程不得阻塞（探针为 4 属性 × 500ms 短超时，最坏约 2s），与本文件既有编排写法一致。采样只在
/// **短暂持锁**中取快照后立即释放，仅最后一次持锁时逐个调用
/// `heal_stalled_renderer`（需要 `&mut`），避免长时间持锁导致其它模块因锁忙跳过。
async fn liveness_check_and_heal(app: &AppHandle) {
    const SOURCE: &str = "pure-mode-exit";

    let Some(state) = app.try_state::<AppState>() else {
        tracing::warn!("AppState 不可用，跳过退出时的视频渲染器存活校验");
        return;
    };
    let engine = state.wallpaper_engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        // 采样 #1：短暂持锁取快照并立即释放（不长时间持锁）
        let probes_first = {
            let mut guard = engine.blocking_lock();
            // 暂停短路（与看护任务一致，符合 D6 契约）：存在任何暂停原因
            // （全屏终止/暂停、电池、托盘暂停）时**不判定停滞、不自愈**——
            // 暂停/终止态下时钟本就不前进，属预期；此处若自愈会把"应暂停"的
            // 壁纸重建为 Playing，与用户/全屏意图相悖。
            if !guard.pause_reasons_snapshot().is_empty() {
                tracing::debug!("退出存活校验：存在暂停原因，跳过（不自愈）");
                return;
            }
            guard.probe_playing_videos()
        };
        // 无"应为 Playing"的视频渲染器：不 sleep、直接返回（保留"零额外等待"契约）
        if probes_first.is_empty() {
            tracing::debug!("退出存活校验：当前无 Playing 状态的视频渲染器，跳过（零额外等待）");
            return;
        }

        // 判定间隔 500ms
        std::thread::sleep(std::time::Duration::from_millis(500));

        // 采样 #2：同样短暂持锁取快照并立即释放。B4：500ms 等待期间状态可能已变，
        // 取锁后先复核暂停原因（符合 D6：用户/全屏暂停下时钟本就不前进，绝不能自愈）
        // 与轮换原子交换（换槽中探针会读到过渡态，据此自愈会打断换槽），命中即放弃本轮。
        let probes_second = {
            let mut guard = engine.blocking_lock();
            if !guard.pause_reasons_snapshot().is_empty() {
                tracing::debug!("退出存活校验：第二次采样前检测到暂停原因，放弃自愈（符合 D6）");
                return;
            }
            if crate::state::atomic_swap_in_progress() {
                tracing::debug!("退出存活校验：第二次采样前检测到轮换原子交换进行中，放弃自愈");
                return;
            }
            guard.probe_playing_videos()
        };

        // 首对采样判定：复用看护任务的共享纯函数 `stalled_display_ids`（仅对"两段都有
        // 快照"的显示器判定，首段无基线则不判定），仅收集命中项（时钟未前进且未暂停）
        // 作为待确认集合。第二段快照缺失该显示器的项会被其静默跳过——这里补一条聚合
        // debug 保留可观测性（不再逐屏记录）。
        let missing_second = probes_first
            .iter()
            .filter(|(id, _)| !probes_second.iter().any(|(second_id, _)| second_id == id))
            .count();
        if missing_second > 0 {
            tracing::debug!(
                missing = missing_second,
                "退出存活校验：第二次采样未返回部分显示器探针（渲染器状态已变化），跳过判定"
            );
        }
        let first_map: HashMap<String, LivenessProbe> = probes_first.into_iter().collect();
        let suspects = stalled_display_ids(&first_map, &probes_second);
        if suspects.is_empty() {
            tracing::debug!("退出存活校验：所有视频渲染器时钟均在前进，无需自愈");
            return;
        }

        // 确认采样：仅当首对命中时才追加（避免极低帧率内容——帧间隔 > 采样窗口——误判），
        // 再等 400ms 后第三次采样；两段区间都"钉住"才执行自愈。
        tracing::debug!(
            suspects = suspects.len(),
            "退出存活校验：首对采样命中疑似停滞，追加第三次采样确认"
        );
        std::thread::sleep(std::time::Duration::from_millis(400));

        // 采样 #3：B4 同样在持锁后先复核暂停原因与轮换原子交换，命中即放弃本轮。
        let probes_third = {
            let mut guard = engine.blocking_lock();
            if !guard.pause_reasons_snapshot().is_empty() {
                tracing::debug!("退出存活校验：第三次采样前检测到暂停原因，放弃自愈（符合 D6）");
                return;
            }
            if crate::state::atomic_swap_in_progress() {
                tracing::debug!("退出存活校验：第三次采样前检测到轮换原子交换进行中，放弃自愈");
                return;
            }
            guard.probe_playing_videos()
        };

        // 自愈：在最后一次持锁中逐个重建（heal 需要 &mut 引擎）；逐屏独立，单屏失败不影响其它屏。
        // B4：自愈前复核暂停原因与轮换原子交换（判定期间状态可能已变），命中即放弃本轮。
        let mut guard = engine.blocking_lock();
        if !guard.pause_reasons_snapshot().is_empty() {
            tracing::debug!("退出存活校验：自愈前检测到暂停原因，放弃自愈（符合 D6）");
            return;
        }
        if crate::state::atomic_swap_in_progress() {
            tracing::debug!("退出存活校验：自愈前检测到轮换原子交换进行中，放弃自愈");
            return;
        }
        // 确认判定（复用看护任务的共享纯函数 `confirmed_stalls`）：仅"首段疑似 + 确认段
        // 同样钉住"才自愈，证伪周期混叠 / 极低帧率误判；确认段快照缺失该显示器的项同样被
        // 静默丢弃（不判定）。返回项的 `curr` 取确认段（probes_third）的 `time_pos`，与看护
        // 任务一致；该字段仅用于 `heal_stalled_renderer` 内部日志，不影响自愈行为。
        for (display_id, prev_time_pos, curr_time_pos) in confirmed_stalls(suspects, &probes_third)
        {
            if let Err(e) =
                guard.heal_stalled_renderer(&display_id, prev_time_pos, curr_time_pos, SOURCE)
            {
                tracing::warn!(
                    display_id = %display_id,
                    error = %e,
                    "退出存活校验：自愈（重建渲染器）失败，已跳过（不阻断退出编排）"
                );
            }
        }
    })
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "退出存活校验任务 join 失败（不阻断退出编排）");
    }
}

/// 卸载输入钩子线程（供本模块退出编排与 `perform_shutdown_blocking` 调用）；幂等。
///
/// 阻塞调用（内部 `InputHook::shutdown` 会 `join` 线程），禁止在主线程执行。
pub(crate) fn shutdown_input_hook() {
    let hook = {
        let mut slot = INPUT_HOOK.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    match hook {
        Some(mut hook) => hook.shutdown(),
        None => tracing::debug!("输入捕获钩子未安装或已卸载，跳过卸载"),
    }
}

/// 置勾选态的实际执行体（阻塞调用，禁止在主线程执行；失败仅记录 warn，不影响模式本身）。
///
/// `CheckMenuItem::set_checked` 内部 `run_item_main_thread!` 会等待主线程完成，
/// 在主线程（托盘事件派发线程）直接调用会死锁，故一律在阻塞线程池中执行。
fn apply_pure_mode_checked(app: &AppHandle, checked: bool) {
    let Some(state) = app.try_state::<AppState>() else {
        tracing::warn!(checked, "AppState 不可用，无法同步托盘『纯净壁纸』勾选态");
        return;
    };
    let Some(item) = state.tray_pure_mode_item.get().cloned() else {
        tracing::warn!(checked, "托盘『纯净壁纸』菜单项引用未设置，无法同步勾选态");
        return;
    };
    if let Err(e) = item.set_checked(checked) {
        tracing::warn!(error = %e, checked, "更新托盘『纯净壁纸』勾选态失败");
    }
}

/// 同步托盘『纯净壁纸』勾选态（fire-and-forget；失败仅记录 warn，不影响模式本身）。
///
/// 内部经 `tokio::task::spawn_blocking` 执行：`CheckMenuItem::set_checked` 内部
/// `run_item_main_thread!` 会等待主线程完成，而在主线程（托盘事件派发线程）直接调用会死锁。
/// 不保证完成顺序，仅供不要求顺序的调用点（如退出编排）使用。
fn set_checked(app: &AppHandle, checked: bool) {
    let app = app.clone();
    tokio::task::spawn_blocking(move || apply_pure_mode_checked(&app, checked));
}

/// 同步置勾选态并等待其真正落位（供进入编排第 7 步使用）。
///
/// 与 fire-and-forget 的 [`set_checked`] 不同，此处 `await` 阻塞任务的 join 句柄，
/// 保证置勾选**执行完成**后才返回——进入编排据此保证"置位"一定先于退出监听触发的
/// `set_checked(false)`（否则勾选态可能被并发退出错位停在 true）。join 失败仅记录 warn。
async fn set_checked_and_wait(app: &AppHandle, checked: bool) {
    let app = app.clone();
    if let Err(e) =
        tokio::task::spawn_blocking(move || apply_pure_mode_checked(&app, checked)).await
    {
        tracing::warn!(error = %e, checked, "同步置勾选任务 join 失败");
    }
}

#[cfg(test)]
mod tests {
    use super::should_attach_cursor_thread;

    /// 编排关键契约存在性（沿用仓库 `include_str!` 文档测试惯例）：
    /// 退出链路必须**先调用引擎 `exit_pure_mode()` 恢复桌面、再卸载输入钩子**
    /// （否则钩子卸载后到回嵌完成之间，输入会落到仍置顶的壁纸窗口上），
    /// 随后唤醒轮换调度器并同步托盘勾选态。
    #[test]
    fn source_contains_orchestration_contract_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let exit_engine = ["exit_", "pure_mode()"].join("");
        let unhook = ["shutdown_", "input_hook"].join("");
        let notify = ["notify_", "waiters"].join("");
        let checked = ["set_", "checked"].join("");

        // 全文件范围会被"函数定义"与别处的调用命中（shutdown_input_hook 的定义出现在
        // exit_orchestration 之后），据此比较位置无意义；故先截取 exit_orchestration
        // 函数体（其签名到 liveness_check_and_heal 文档注释之间）再断言。
        let exit_fn = ["pub(crate) async fn ", "exit_orchestration"].join("");
        let liveness_doc = ["退出纯净壁纸模式后的", "视频渲染器存活校验"].join("");
        let start = source
            .find(&exit_fn)
            .expect("源码应含 exit_orchestration 定义");
        let end = source
            .find(&liveness_doc)
            .expect("源码应含 liveness_check_and_heal 文档注释");
        let exit_body = &source[start..end];

        assert!(
            exit_body.contains(&exit_engine),
            "退出编排函数体内应调用引擎 {exit_engine} 恢复桌面"
        );
        assert!(
            exit_body.contains(&unhook),
            "退出编排函数体内应调用 {unhook} 卸载输入钩子线程"
        );
        assert!(
            exit_body.contains(&checked),
            "退出编排函数体内应 {checked} 同步托盘勾选态"
        );
        assert!(
            exit_body.contains(&notify),
            "退出编排函数体内应 {notify} 唤醒轮换调度器重算 deadline"
        );

        // 退出编排函数体内顺序契约：
        // exit_pure_mode() < shutdown_input_hook < set_checked < notify_waiters
        let exit_at = exit_body
            .find(&exit_engine)
            .expect("退出编排应含 exit_pure_mode() 调用");
        let unhook_at = exit_body
            .find(&unhook)
            .expect("退出编排应含 shutdown_input_hook 调用");
        let checked_at = exit_body
            .find(&checked)
            .expect("退出编排应含 set_checked 调用");
        let notify_at = exit_body
            .find(&notify)
            .expect("退出编排应含 notify_waiters 调用");
        assert!(
            exit_at < unhook_at && unhook_at < checked_at && checked_at < notify_at,
            "退出编排函数体内顺序契约：exit_pure_mode() < shutdown_input_hook < set_checked < notify_waiters"
        );
    }

    /// 进入互斥与句柄唯一性契约（F2，沿用仓库 `include_str!` 文档测试惯例）：
    /// 进入编排必须有 `ENTER_IN_PROGRESS` 互斥守卫；存储新句柄前必须先 take 出旧句柄并 `shutdown()`。
    #[test]
    fn source_contains_enter_mutex_and_slot_uniqueness_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let enter_mutex = ["ENTER_IN", "_PROGRESS"].join("");
        let take_old = ["slot.", "take()"].join("");
        let shutdown_old = ["old_hook.", "shutdown()"].join("");
        assert!(
            source.contains(&enter_mutex),
            "进入编排应使用 {enter_mutex} 互斥守卫拒绝重复进入"
        );
        assert!(
            source.contains(&take_old),
            "存储新句柄前应先从槽位 {take_old} 取出旧句柄"
        );
        assert!(
            source.contains(&shutdown_old),
            "取出旧句柄后应调用 {shutdown_old} 卸载，避免钩子线程泄漏"
        );

        // 顺序契约：先 take 出旧句柄（释放锁）再 shutdown，最后才存入新句柄
        let take_at = source.find(&take_old).expect("源码应含 take 旧句柄");
        let shutdown_at = source
            .find(&shutdown_old)
            .expect("源码应含 shutdown 旧句柄");
        assert!(
            take_at < shutdown_at,
            "必须先 take 出旧句柄（释放锁）再 shutdown（避免持锁阻塞）"
        );
    }

    /// A2 契约：进入编排必须先等待在途退出编排落位（`EXIT_IN_PROGRESS.load`，保证旧捕获
    /// 线程已 join），并在存储钩子前复核模式活跃（不活跃则不存句柄、直接卸载新钩子）。
    #[test]
    fn source_contains_exit_wait_and_store_guard_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let wait_load = ["EXIT_IN", "_PROGRESS.load"].join("");
        let store_guard = ["不存句柄", "并卸载新钩子"].join("");
        let store_new = ["*slot = Some(", "hook)"].join("");
        assert!(
            source.contains(&wait_load),
            "进入编排应轮询 {wait_load} 等待在途退出编排落位"
        );
        assert!(
            source.contains(&store_guard),
            "存储句柄前应复核模式活跃（{store_guard} 文案唯一标识该分支）"
        );
        assert!(
            source.contains(&store_new),
            "存储新句柄应通过 {store_new} 写入槽位"
        );
        // 顺序契约：等待（EXIT_IN_PROGRESS.load）必须先于存入新句柄出现
        let wait_at = source.find(&wait_load).expect("源码应含等待标记");
        let store_new_at = source.find(&store_new).expect("源码应含存入新句柄");
        assert!(
            wait_at < store_new_at,
            "等待在途退出编排落位必须先于存入新句柄（否则旧线程未 join 就覆盖槽位）"
        );
    }

    /// 退出编排存活校验契约（D3，沿用仓库 `include_str!` 文档测试惯例）：
    /// 退出编排必须调用 `liveness_check_and_heal`（校验视频渲染器时钟是否在前进并自愈），
    /// 且调用位置**晚于** `exit_pure_mode()`（先回嵌桌面）、**早于** `notify_waiters`
    /// （校验画面是活的之后再唤醒轮换调度器）。
    #[test]
    fn source_contains_liveness_check_orchestration_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let liveness = ["liveness_", "check_and_heal"].join("");
        let exit_engine = ["exit_", "pure_mode()"].join("");
        let notify = ["notify_", "waiters"].join("");
        assert!(
            source.contains(&liveness),
            "退出编排应调用 {liveness} 校验视频渲染器存活并自愈"
        );

        // 顺序契约：回嵌桌面（exit_pure_mode）< 存活校验（liveness_check_and_heal）< 唤醒调度器（notify_waiters）
        let exit_at = source
            .find(&exit_engine)
            .expect("源码应含 exit_pure_mode() 调用");
        let liveness_at = source
            .find(&liveness)
            .expect("源码应含 liveness_check_and_heal 调用");
        let notify_at = source.find(&notify).expect("源码应含 notify_waiters");
        assert!(
            exit_at < liveness_at,
            "存活校验必须晚于 exit_pure_mode()（先回嵌桌面，再校验画面是否在播）"
        );
        assert!(
            liveness_at < notify_at,
            "存活校验必须早于 notify_waiters（校验完成后才唤醒轮换调度器）"
        );

        // 用户可见态优先：set_checked(false) 必须早于存活校验（校验/自愈可能持续数秒）。
        // 注意：enter_orchestration 中也有 set_checked 调用、且早于本文件里的
        // liveness_check_and_heal 定义，若在全文件范围定位会恒真；故先截取
        // exit_orchestration 函数体（其签名到 liveness_check_and_heal 文档注释之间）再定位。
        let exit_fn = ["pub(crate) async fn ", "exit_orchestration"].join("");
        let liveness_doc = ["退出纯净壁纸模式后的", "视频渲染器存活校验"].join("");
        let exit_body = {
            let start = source
                .find(&exit_fn)
                .expect("源码应含 exit_orchestration 定义");
            let end = source
                .find(&liveness_doc)
                .expect("源码应含 liveness_check_and_heal 文档注释");
            &source[start..end]
        };
        let checked = ["set_", "checked"].join("");
        let checked_at = exit_body
            .find(&checked)
            .expect("退出编排应含 set_checked 调用");
        let liveness_call_at = exit_body
            .find(&liveness)
            .expect("退出编排应含 liveness_check_and_heal 调用");
        assert!(
            checked_at < liveness_call_at,
            "取消勾选必须早于存活校验（避免用户可见态滞后）"
        );
    }

    /// 托盘溢出浮出窗关闭契约（沿用仓库 `include_str!` 文档测试惯例）：
    /// 进入编排必须调用 `dismiss_tray_overflow_flyout()` 关闭"隐藏栏"浮出窗，
    /// 且调用位置**早于**引擎进入（`enter_pure_mode()`）——浮出窗必须在壁纸置顶之前
    /// 关闭，否则会叠在壁纸上；同时必须覆盖 Win11 / Win10- 两类类名常量。
    #[test]
    fn source_contains_tray_overflow_dismiss_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let class_win11 = ["TopLevelWindowFor", "OverflowXamlIsland"].join("");
        let class_legacy = ["NotifyIcon", "OverflowWindow"].join("");
        let dismiss = ["dismiss_tray_overflow", "_flyout"].join("");
        let enter_engine = ["enter_", "pure_mode()"].join("");
        assert!(
            source.contains(&class_win11),
            "应覆盖 Win11 溢出浮出窗类名常量 {class_win11}"
        );
        assert!(
            source.contains(&class_legacy),
            "应覆盖 Win10- 溢出浮出窗类名常量 {class_legacy}"
        );

        // 只在 enter_orchestration 函数体内定位调用点与引擎进入，避免匹配到 helper 定义处
        let enter_fn = ["async fn ", "enter_orchestration"].join("");
        let exit_fn_doc = ["/// 退出编排（供输入钩子线程消息", "、托盘二次点击共用）"].join("");
        let enter_body = {
            let start = source
                .find(&enter_fn)
                .expect("源码应含 enter_orchestration 定义");
            let end = source
                .find(&exit_fn_doc)
                .expect("源码应含 exit_orchestration 文档注释");
            &source[start..end]
        };
        let dismiss_at = enter_body
            .find(&dismiss)
            .expect("进入编排应调用 dismiss_tray_overflow_flyout 关闭托盘溢出浮出窗");
        let enter_at = enter_body
            .find(&enter_engine)
            .expect("进入编排应含 enter_pure_mode() 调用");
        assert!(
            dismiss_at < enter_at,
            "关闭托盘溢出浮出窗必须早于引擎进入（壁纸置顶后浮出窗会叠在壁纸上）"
        );
    }

    /// 光标挂接判定：目标线程有效、非当前线程、且窗口未无响应时才挂接。
    #[test]
    fn should_attach_cursor_thread_rules() {
        assert!(
            should_attach_cursor_thread(123, 456, false),
            "有效且不同的线程应挂接"
        );
        assert!(
            !should_attach_cursor_thread(0, 456, false),
            "取不到目标线程时不挂接"
        );
        assert!(
            !should_attach_cursor_thread(456, 456, false),
            "目标线程等于当前线程时不挂接"
        );
        assert!(
            !should_attach_cursor_thread(123, 456, true),
            "目标窗口无响应时不挂接（避免一起挂死）"
        );
    }

    /// 光标复位契约（根因：后台 NOACTIVATE 覆盖窗被 Windows 显示为 IDC_WAIT）：
    /// 进入编排必须在引擎置顶**之后**、同步置勾选**之前**调用 `reset_pure_mode_cursor`，
    /// 且实现使用 `AttachThreadInput` + `SetCursor`，不得改成抢前台。
    #[test]
    fn source_contains_cursor_reset_markers() {
        let source = include_str!("pure_mode.rs");
        // 运行期拼接搜索串，避免 include_str! 把测试自身的字面量计入匹配
        let attach = ["Attach", "ThreadInput"].join("");
        let set_cursor = ["Set", "Cursor"].join("");
        let reset_fn = ["fn reset_pure_mode_", "cursor()"].join("");
        let reset_call = ["reset_pure_mode_", "cursor_when_waiting"].join("");
        let poll = ["GetCursor", "Info"].join("");
        let wait_marker = ["IDC_", "WAIT"].join("");
        let foreground = ["SetFore", "groundWindow"].join("");
        let enter_engine = ["enter_", "pure_mode()"].join("");
        let checked_wait = ["set_checked_", "and_wait"].join("");

        assert!(source.contains(&attach), "光标复位应使用 {attach}");
        assert!(source.contains(&set_cursor), "光标复位应使用 {set_cursor}");
        assert!(source.contains(&reset_fn), "应定义 {reset_fn}");
        assert!(source.contains(&poll), "光标轮询应使用 {poll}");
        assert!(
            source.contains(&wait_marker),
            "光标轮询应识别系统等待态 {wait_marker}"
        );
        assert!(
            !source.contains(&foreground),
            "光标复位不得改为抢前台（{foreground}）"
        );

        // 只在 enter_orchestration 函数体内定位顺序
        let enter_fn = ["async fn ", "enter_orchestration"].join("");
        let exit_fn_doc = ["/// 退出编排（供输入钩子线程消息", "、托盘二次点击共用）"].join("");
        let enter_body = {
            let start = source
                .find(&enter_fn)
                .expect("源码应含 enter_orchestration 定义");
            let end = source
                .find(&exit_fn_doc)
                .expect("源码应含 exit_orchestration 文档注释");
            &source[start..end]
        };
        let enter_at = enter_body
            .find(&enter_engine)
            .expect("进入编排应含 enter_pure_mode() 调用");
        let reset_at = enter_body
            .find(&reset_call)
            .expect("进入编排应调用 reset_pure_mode_cursor");
        let checked_at = enter_body
            .find(&checked_wait)
            .expect("进入编排应含 set_checked_and_wait 调用");
        assert!(
            enter_at < reset_at,
            "光标复位必须晚于引擎置顶（覆盖窗与置顶注册表此时才存在）"
        );
        assert!(
            reset_at < checked_at,
            "光标复位应早于同步置勾选（尽快恢复用户可见光标）"
        );
    }
}
