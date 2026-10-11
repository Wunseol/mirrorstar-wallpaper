//! 纯净壁纸模式底层原语。
//!
//! ## 模块职责
//!
//! 提供"纯净壁纸模式"所需的进程级原语，供 core（引擎编排）与 src-tauri
//! （调度器 / 状态守卫 / 输入钩子线程）共同调用：
//!
//! 1. 进程级单源状态标志：[`is_active`] / [`set_active`]
//! 2. 壁纸窗口摘出 + 置顶：[`lift_wallpaper_window`] / [`lift_window_to_rect`]
//! 3. Z 序重断言：[`assert_topmost`]（单个）/ [`assert_topmost_all`]（注册表批量）
//! 4. 置顶窗口注册表：[`register_lifted`] / [`clear_lifted`]
//! 5. 虚拟桌面矩形查询：[`virtual_screen_rect`]
//!
//! 本模块只负责"窗口层级 / 几何"层面的原语，不含引擎编排
//! （`enter_pure_mode` / `exit_pure_mode`）、输入捕获与托盘逻辑——那些由上层模块负责。
//!
//! ## 设计约束（实现契约 D1 / D7；D2 与 D7 的编排/幂等由 desktop / wallpaper 上层实现）
//!
//! - **保持矩形不变（D1）**：把壁纸窗口从 WorkerW 层摘出并置顶时，**必须**先
//!   读取当前屏幕矩形，并原样复用该矩形，**绝不 resize**。mpv 的 D3D11 输出在
//!   窗口重父化 + resize 时会重配置（项目已用 `--d3d11-flip=no` 做双保险），
//!   零尺寸变化是规避该路径的核心降险手段。
//! - **不抢前台（D1）**：置顶过程只做"重定父级 + 设置 `HWND_TOPMOST`"，**禁止**调用
//!   任何抢前台的 API。窗口已带 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`（嵌入时由
//!   `remove_from_taskbar` 设置），置顶后不会成为前台窗口，从根上避免触发全屏检测。
//! - **标志时序（D7）**：`PURE_MODE_ACTIVE` 是进程级单源标志，供 core 与 src-tauri
//!   （调度器 / 状态守卫）读取；必须在**置顶前**置位、在**回嵌完成后**清除。
//! - **无锁重断言**：Win32 窗口过程回调（如收到 `TaskbarCreated` 广播）处于
//!   "不能加锁做重活"的上下文，通过 [`assert_topmost_all`] 重断言 Z 序；注册表内部
//!   只存 `HWND` 的 `isize` 值（`HWND` 是裸指针类型，不是 `Send`/`Sync`，存数值可
//!   避免 `unsafe impl`）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, SetParent, SetWindowPos, HWND_TOPMOST, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
};

use crate::MirrorStarError;

/// 进程级纯净壁纸模式标志（单源）。
///
/// 供 core 与 src-tauri（轮换调度器 / 全屏与电源状态守卫）读取，用于在模式运行期间
/// 抑制定时轮换与自动暂停/终止动作。**不使用 `PauseReason` 位图**：该位图对非空原因
/// 发原因无关的 `PauseCommand::Pause`，把"抑制轮换但不停播"塞入会污染语义。
///
/// 时序契约（D7）：必须在**置顶前**置位、在**回嵌完成后**清除，使任一读方观察到的
/// "活跃"状态都对应已完成置顶的一致窗口层级。
static PURE_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 读取纯净壁纸模式是否处于活跃状态。
pub fn is_active() -> bool {
    PURE_MODE_ACTIVE.load(Ordering::SeqCst)
}

/// 设置纯净壁纸模式活跃标志。
///
/// 调用方契约见 [`PURE_MODE_ACTIVE`]：置位须早于置顶，清除须晚于回嵌完成。
pub fn set_active(active: bool) {
    PURE_MODE_ACTIVE.store(active, Ordering::SeqCst);
}

/// 置顶窗口注册表：保存已置顶窗口的 `HWND` 数值（`isize`）。
///
/// 存 `isize` 而非 `HWND` 本体：`HWND` 是裸指针类型，不满足 `Send`/`Sync`，存数值
/// 可让本静态容器免于 `unsafe impl`。供 [`assert_topmost_all`] 在"不能加锁做重活"
/// 的窗口过程回调上下文（如 `TaskbarCreated`）中重断言 Z 序。
static LIFTED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 覆盖式写入置顶窗口注册表。
///
/// 每次进入纯净壁纸模式时用本次置顶的全部窗口句柄覆盖旧内容（不做追加），避免上次
/// 异常退出遗留的陈旧句柄影响本次重断言。返回后注册表内容即为传入切片的句柄数值。
pub fn register_lifted(hwnds: &[HWND]) {
    let mut guard = LIFTED.lock().unwrap_or_else(|e| e.into_inner());
    guard.clear();
    guard.extend(hwnds.iter().map(|h| h.0 as isize));
}

/// 清空置顶窗口注册表（退出纯净壁纸模式时调用）。
pub fn clear_lifted() {
    let mut guard = LIFTED.lock().unwrap_or_else(|e| e.into_inner());
    guard.clear();
}

/// 对注册表内所有置顶窗口重断言一次 `HWND_TOPMOST`（不动几何）。
///
/// 使用场景：收到 `TaskbarCreated` 广播（可能伴随任务栏重建并抢走 Z 序）时，由输入
/// 钩子线程的窗口过程回调调用。为满足"回调内禁止持锁做重活"的约束，本函数先克隆出
/// 句柄列表再释放锁，随后逐个 [`assert_topmost`]；单个失败仅记录 `warn` 并继续（不
/// panic，忽略已失效句柄）。
pub fn assert_topmost_all() {
    // 先取出句柄快照并释放锁，避免在回调上下文持锁调用 Win32 同步操作。
    let snapped: Vec<isize> = {
        let guard = LIFTED.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    for raw in snapped {
        let hwnd = HWND(raw as *mut _);
        if let Err(e) = assert_topmost(hwnd) {
            tracing::warn!(
                error = %e,
                hwnd = raw,
                "重断言置顶窗口 Z 序失败，忽略该句柄"
            );
        }
    }
}

/// 将 WorkerW 内的壁纸窗口摘出并置顶全屏（保持当前矩形，零 resize）。
///
/// 严格按以下顺序执行（D1）：
/// 1. `GetWindowRect` 读取当前屏幕矩形（**必须在摘出之前**，摘出后坐标含义变化）
/// 2. `SetParent(hwnd, None)` 从 WorkerW 层重定父级摘出
/// 3. `SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW |
///    SWP_FRAMECHANGED)` 置顶，矩形与第 1 步读到的完全一致
/// 4. `InvalidateRect(hwnd, None, false)` 触发 GDI 渲染器保险重绘
///
/// 调用方职责：调用前需已置位 [`set_active`]，并在失败时按需回滚（本函数不做回滚）；
/// 退出时须用现有 `DesktopIntegrator::embed_wallpaper` 嵌回。
///
/// 错误契约：任一步 Win32 调用失败即返回
/// [`MirrorStarError::DesktopIntegration`] 并记录日志；失败时窗口可能已处于"半摘出"
/// 状态（如已摘出但未置顶），调用方应终止/回滚该窗口。
pub fn lift_wallpaper_window(hwnd: HWND) -> Result<(), MirrorStarError> {
    // Step 1: 读取当前屏幕矩形（先取矩形再摘出——摘出后坐标含义会变化）
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).map_err(|e| {
            tracing::error!(
                error = %e,
                hwnd = hwnd.0 as isize,
                "GetWindowRect 失败，无法读取壁纸窗口当前矩形"
            );
            MirrorStarError::DesktopIntegration(format!("GetWindowRect 失败: {}", e))
        })?;
    }
    // 原样复用读到的矩形：宽度/高度不做任何调整（零 resize，见 D1）
    let (x, y, w, h) = (
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    );

    // Step 2: 从 WorkerW 层摘出（不隐藏，窗口必须保持可见以继续出画）
    unsafe {
        SetParent(hwnd, None).map_err(|e| {
            tracing::error!(
                error = %e,
                hwnd = hwnd.0 as isize,
                "SetParent 摘出壁纸窗口失败"
            );
            MirrorStarError::DesktopIntegration(format!("SetParent 摘出壁纸窗口失败: {}", e))
        })?;
    }

    // Step 3 + Step 4: 置顶全屏 + GDI 保险重绘（复用公共步骤；矩形与 Step 1 完全一致）
    set_topmost_to_rect(hwnd, x, y, w, h, "壁纸窗口")
}

/// 把顶层窗口置顶到指定矩形并触发 GDI 保险重绘（[`lift_wallpaper_window`] /
/// [`lift_window_to_rect`] 的公共步骤，单一实现）。
///
/// 执行 `SetWindowPos(HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW |
/// SWP_FRAMECHANGED)`：失败即返回 [`MirrorStarError::DesktopIntegration`] 并记 `error!`；
/// 随后 `InvalidateRect(hwnd, None, false)` 保险重绘（失败非致命，仅 `warn`）。
///
/// `label` 区分调用方上下文（"壁纸窗口" / "临时呈现窗口"），用于拼接日志与错误文案。
fn set_topmost_to_rect(
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
) -> Result<(), MirrorStarError> {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .map_err(|e| {
            tracing::error!(
                error = %e,
                hwnd = hwnd.0 as isize,
                "SetWindowPos 置顶{}失败",
                label
            );
            MirrorStarError::DesktopIntegration(format!("SetWindowPos 置顶{}失败: {}", label, e))
        })?;

        // GDI 渲染器保险重绘（失败非致命，仅 warn）
        if !InvalidateRect(hwnd, None, false).as_bool() {
            tracing::warn!(
                hwnd = hwnd.0 as isize,
                "InvalidateRect 失败，{}保险重绘未触发",
                label
            );
        }
    }

    tracing::info!(hwnd = hwnd.0 as isize, x, y, w, h, "{}已置顶全屏", label);
    Ok(())
}

/// 将"本来就是顶层窗口"的临时呈现窗口置顶到指定矩形。
///
/// 与 [`lift_wallpaper_window`] 的区别：**不做重定父级**（该窗口本就不在 WorkerW 内），
/// 仅执行公共步骤 `set_topmost_to_rect`（`SetWindowPos(HWND_TOPMOST, …)` + `InvalidateRect`）。
/// 用于纯净壁纸模式的临时顶层呈现窗口（视频覆盖 / 静态图片）。
pub fn lift_window_to_rect(
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<(), MirrorStarError> {
    set_topmost_to_rect(hwnd, x, y, w, h, "临时呈现窗口")
}

/// 仅重断言窗口 Z 序为 `HWND_TOPMOST`，**不改变几何**。
///
/// 通过 `SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE` 保证不动位置/尺寸、不抢焦点，
/// 仅把窗口重新提到置顶层。用于 `TaskbarCreated` 广播后防止任务栏抢走 Z 序。
pub fn assert_topmost(hwnd: HWND) -> Result<(), MirrorStarError> {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .map_err(|e| {
            tracing::warn!(
                error = %e,
                hwnd = hwnd.0 as isize,
                "重断言窗口 HWND_TOPMOST 失败"
            );
            MirrorStarError::DesktopIntegration(format!("重断言窗口 HWND_TOPMOST 失败: {}", e))
        })?;
    }
    Ok(())
}

/// 查询虚拟桌面（所有显示器并集）的屏幕矩形。
///
/// 返回 `(x, y, w, h)`，其中 `(x, y)` 为虚拟屏幕左上角坐标（存在左侧/上方副显示器
/// 时可能为负），`(w, h)` 为宽高。宽高任一非正（无头/异常环境）时回退
/// `(0, 0, 1920, 1080)`。
pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics 为无副作用的只读查询。
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w > 0 && h > 0 {
            (x, y, w, h)
        } else {
            tracing::warn!(w, h, "虚拟屏幕尺寸非正，回退到 (0, 0, 1920, 1080)");
            (0, 0, 1920, 1080)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 状态标志测试 ────────────────────────────────────────────────────

    /// 标志 set / clear / is_active 行为：置位后读到 true，清除后读到 false。
    #[test]
    fn set_active_and_is_active_roundtrip() {
        // 初始应为 false（进程级静态初值）
        assert!(!is_active(), "初始 PURE_MODE_ACTIVE 应为 false");

        set_active(true);
        assert!(is_active(), "set_active(true) 后 is_active 应为 true");

        set_active(false);
        assert!(!is_active(), "set_active(false) 后 is_active 应为 false");
    }

    // ── 虚拟屏幕矩形测试 ────────────────────────────────────────────────

    /// virtual_screen_rect 宽高为正且不 panic（真实桌面环境应返回实际值）。
    #[test]
    fn virtual_screen_rect_positive_and_no_panic() {
        let (_x, _y, w, h) = virtual_screen_rect();
        assert!(w > 0, "虚拟屏幕宽度应为正，实际 {w}");
        assert!(h > 0, "虚拟屏幕高度应为正，实际 {h}");
    }

    // ── 置顶窗口注册表测试 ──────────────────────────────────────────────

    /// register_lifted + clear_lifted + assert_topmost_all 对无效句柄不 panic，
    /// 且 clear 后注册表为空。
    #[test]
    fn register_clear_and_assert_all_with_invalid_handles_no_panic() {
        use windows::Win32::Foundation::HWND;

        // 故意传入无效句柄（非空伪句柄 + 空句柄）
        let invalid = [HWND(1 as *mut _), HWND::default()];
        register_lifted(&invalid);

        // 断言注册表已覆盖式写入为传入的 2 个句柄
        {
            let guard = LIFTED.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.len(), 2, "register_lifted 应写入 2 个句柄");
            assert_eq!(guard[0], 1, "register_lifted 应存 HWND 的 isize 值");
            assert_eq!(guard[1], 0, "HWND::default() 的 isize 值为 0");
        }

        // 对无效句柄重断言：SetWindowPos 会失败，但函数应只 warn、不 panic
        assert_topmost_all();

        // 清空后注册表应为空
        clear_lifted();
        {
            let guard = LIFTED.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.is_empty(), "clear_lifted 后注册表应为空");
        }
    }

    // ── 文档标记测试（仓库惯例：include_str! 断言关键契约） ──────────────

    /// 源码应包含置顶所需的关键 Win32 契约标记。
    #[test]
    fn source_contains_topmost_contract_markers() {
        let source = include_str!("mod.rs");
        assert!(
            source.contains("SWP_NOACTIVATE"),
            "源码应包含 SWP_NOACTIVATE（置顶时不抢焦点）"
        );
        assert!(
            source.contains("HWND_TOPMOST"),
            "源码应包含 HWND_TOPMOST（置顶全屏契约）"
        );
    }

    /// lift_wallpaper_window 中 GetWindowRect 必须出现在 SetParent 之前
    /// （保证"先取矩形再摘出"的 D1 契约）。
    #[test]
    fn get_window_rect_precedes_set_parent() {
        let source = include_str!("mod.rs");
        // 用带 `.map_err` 的实际调用形式作为搜索串，避免匹配到文档注释中的提及。
        // 函数定义位于测试模块之前，find 返回首次（最早）出现位置即真实调用点。
        let get_rect_at = source
            .find("GetWindowRect(hwnd, &mut rect).map_err")
            .expect("源码应包含 GetWindowRect(hwnd, &mut rect) 调用");
        let set_parent_at = source
            .find("SetParent(hwnd, None).map_err")
            .expect("源码应包含 SetParent(hwnd, None) 调用");
        assert!(
            get_rect_at < set_parent_at,
            "GetWindowRect 必须先于 SetParent 出现（先取矩形再摘出）"
        );
    }

    /// 源码不得调用任何抢前台 API（以前台窗口设置 API 为代表）。
    #[test]
    fn source_does_not_set_foreground_window() {
        let source = include_str!("mod.rs");
        // 运行期拼接搜索串，避免 include_str! 将测试自身的字面量计入匹配。
        let needle = ["SetFore", "groundWindow"].join("");
        assert!(
            !source.contains(&needle),
            "源码不得调用抢前台 API（{needle}）"
        );
    }

    // ── 真机手动测试（需真实桌面会话，CI 中 #[ignore]） ─────────────────

    /// 真机验证：对一个自有窗口执行"摘出 → 置顶"，断言窗口带 `WS_EX_TOPMOST`
    /// 且无父窗口。
    ///
    /// 说明：需真实桌面会话，标记 `#[ignore]`，本地手动运行
    /// `cargo test -p mirrorstar-core --lib pure_mode:: -- --ignored`。
    ///
    /// # 应用级真机验证步骤
    ///
    /// 1. 运行应用（`cargo run -p mirrorstar-wallpaper` 或发布版），确保至少一个显示器已设置
    ///    视频 / 网页 / GIF 壁纸并正在播放。
    /// 2. 右键托盘图标 → 勾选「纯净壁纸（按任意键退出）」。
    /// 3. 观察壁纸置顶后是否**继续出画**（视频持续播放、网页动画持续运行），
    ///    无黑屏 / 无卡死 / 无白屏；桌面图标、任务栏与其他窗口均被覆盖。
    /// 4. 按任意键或点击鼠标退出，确认壁纸回到图标层之下且恢复播放。
    #[test]
    #[ignore = "需真机桌面会话，仅本地手动运行"]
    fn lift_wallpaper_window_manual_real_session() {
        use std::sync::OnceLock;

        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, DestroyWindow, GetParent, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST,
        };

        // 自有窗口过程：仅需能创建窗口，交给默认处理即可。
        unsafe extern "system" fn test_wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        // 1. 注册自有窗口类（复用 gdi_base 的幂等注册辅助函数）
        static TEST_CLASS_ONCE: OnceLock<()> = OnceLock::new();
        let class_name = crate::wallpaper::gdi_base::register_window_class_once(
            &TEST_CLASS_ONCE,
            w!("MirrorStarPureModeManualTest"),
            test_wnd_proc,
        );

        // 2. 创建一个小尺寸自有窗口
        let hwnd = crate::wallpaper::gdi_base::create_wallpaper_window(
            class_name,
            w!("PureModeTest"),
            (0, 0, 320, 240),
        )
        .expect("创建测试窗口应成功");

        // 3. 摘出 + 置顶
        let result = lift_wallpaper_window(hwnd);
        assert!(
            result.is_ok(),
            "lift_wallpaper_window 应成功: {:?}",
            result.err()
        );

        // 4. 断言已带 WS_EX_TOPMOST
        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        assert_ne!(
            ex_style & (WS_EX_TOPMOST.0 as isize),
            0,
            "窗口应带有 WS_EX_TOPMOST 扩展样式"
        );

        // 5. 断言窗口为顶层（无父窗口）。
        // windows-rs 将 GetParent 返回的 null 句柄包装为 Err，故"无父窗口"表现为 is_err()。
        assert!(
            unsafe { GetParent(hwnd) }.is_err(),
            "置顶后窗口应无父窗口（顶层）"
        );

        // 6. 清理
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
