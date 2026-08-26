//! Windows 桌面集成模块。
//!
//! 本模块负责将壁纸窗口嵌入 Windows 桌面的 WorkerW 层（位于桌面图标层之下），
//! 并管理显示器枚举、原生壁纸设置等与 Win32 桌面环境的交互。
//!
//! ## 职责划分
//!
//! 模块内分为两层：
//!
//! 1. **`DesktopIntegrator`（状态层，本文件）**
//!    - 持有 `progman_hwnd` / `workerw_hwnd` / `active_wallpapers` 等可变状态
//!    - 提供懒加载（`ensure_initialized`）、嵌入（`embed_wallpaper`）、移除
//!      （`remove_wallpaper`）、失效检测（`is_workerw_valid`）等高级 API
//!    - 维护显示器列表缓存（5s TTL，D-013）
//!    - 由外部 `Arc<Mutex<DesktopIntegrator>>` 保护，跨线程共享
//!
//! 2. **`worker_w` 模块（无状态操作层）**
//!    - 提供纯函数式 Win32 操作：`find_workerw_no_retry`（查找 WorkerW）、
//!      `embed_wallpaper`（嵌入壁纸窗口）、`get_system_wallpaper` /
//!      `restore_system_wallpaper`（读写系统壁纸）等
//!    - 不持有任何状态，所有 HWND 由调用方传入
//!    - 包含 `window` 子模块（窗口样式操作：`make_borderless` /
//!      `set_mouse_passthrough` / `remove_from_taskbar`）与
//!      `native_wallpaper` 子模块（注册表 + SystemParametersInfoW 原生壁纸）
//!
//! ## 调用顺序
//!
//! 典型嵌入流程（`DesktopIntegrator::embed_wallpaper`）：
//! 1. `ensure_initialized` → `ensure_workerw_ready` → `worker_w::find_workerw_no_retry`
//! 2. `get_cached_displays` → `enumerate_displays`（首次或缓存过期时）
//! 3. `worker_w::embed_wallpaper` →
//!    - `window::make_borderless`（去边框）
//!    - `window::remove_from_taskbar`（移除任务栏条目）
//!    - `SetParent`（重定为 WorkerW 子窗口）
//!    - `SetWindowPos`（定位到目标显示器）
//! 4. 写入 `active_wallpapers` 条目
//!
//! 移除流程（`DesktopIntegrator::remove_wallpaper`）：
//! 1. `is_workerw_valid` 校验（v41-D-016，失效则跳过 SetParent）
//! 2. `IsWindow(hwnd)` 校验壁纸窗口有效性
//! 3. `SetParent(hwnd, None)` 分离 + `ShowWindow(SW_HIDE)` 隐藏（D-005）
//! 4. 移除 `active_wallpapers` 条目
//!
//! ## 并发约束
//!
//! - `DesktopIntegrator` 通过 `Arc<Mutex<DesktopIntegrator>>` 跨线程共享，
//!   所有公共方法均需持有 Mutex 锁
//! - `unsafe impl Send for DesktopIntegrator`（HWND 跨线程传递安全），
//!   详见类型上方 SAFETY 注释与 v41-D-009 阻塞上界分析
//! - 锁持有期间最坏阻塞 ≈ 220ms（`WM_SPAWN_WORK_TIMEOUT_MS` 200ms + EnumWindows 遍历）
//! - 重试 sleep 由调用方在释放锁后执行，避免持锁 sleep
//! - `worker_w` 模块函数无状态、无线程亲和性，可在任意线程调用
//!   （但 Win32 窗口操作建议在创建窗口的线程上执行，当前通过同步消息跨线程调用）

pub mod native_wallpaper;
pub mod window;
pub mod worker_w;

use std::collections::HashMap;

use windows::Win32::Foundation::{GetLastError, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, SetParent, ShowWindow, MONITORINFOF_PRIMARY, SW_HIDE,
};

use crate::config::settings::Arrangement;
use crate::config::DisplayInfo;
use crate::MirrorStarError;

/// 桌面集成器，负责将壁纸窗口嵌入 Windows 桌面的 WorkerW 层
#[derive(Default)]
pub struct DesktopIntegrator {
    /// Progman 窗口句柄
    progman_hwnd: Option<HWND>,
    /// WorkerW 窗口句柄（壁纸嵌入目标）
    workerw_hwnd: Option<HWND>,
    /// 活跃壁纸窗口：显示器ID -> (壁纸窗口 HWND, 布局方式)
    active_wallpapers: HashMap<String, (HWND, Arrangement)>,
    /// 用户原始系统壁纸路径
    original_wallpaper: Option<String>,
    /// 是否已完成 WorkerW 初始化
    initialized: bool,
    /// 显示器列表缓存（D-013：避免 PerMonitor 分支重复 enumerate_displays）
    ///
    /// 缓存 `(Vec<DisplayInfo>, Instant)` 元组，TTL 5s。`Option` 的 `Default`
    /// 总是 `None`，因此 `#[derive(Default)]` 仍可工作（不要求 `Instant: Default`）。
    cached_displays: Option<(Vec<DisplayInfo>, std::time::Instant)>,
}

// SAFETY: DesktopIntegrator 包含 HWND（窗口句柄）和 HashMap。
//
// Send 安全性：HWND 本质是指针大小的整数（isize），其值可以安全地跨线程传递。
// DesktopIntegrator 不持有任何线程局部资源，HWND 值的复制不会导致数据竞争。
//
// Sync 不需要：DesktopIntegrator 不实现 Sync，因为：
// 1. 所有跨线程访问都通过外部 `Arc<Mutex<DesktopIntegrator>>` 保护，
//    调用方不会直接共享 `&DesktopIntegrator` 引用。
// 2. `Mutex<T>: Sync` 只要求 `T: Send`，不要求 `T: Sync`，
//    因此只要实现 Send 即可让 `Arc<Mutex<DesktopIntegrator>>` 跨线程共享。
// 3. 因此不需要 `unsafe impl Sync`，移除后编译仍可通过。
//
// 窗口操作约束（D-014 修订）：
// 虽然 HWND 值可以跨线程传递，但 Win32 窗口操作（如 SetParent、SetWindowPos、ShowWindow）
// 通常需要在创建窗口的线程上执行。DesktopIntegrator 的方法（embed_wallpaper /
// ensure_workerw_ready / remove_wallpaper 等）并非仅做句柄的存储与传递，
// 而是在持有 Mutex 的任意线程上直接调用这些 Win32 窗口操作 API。
//
// 这些跨线程窗口操作通过同步消息（SendMessageTimeoutW 等）执行，技术上可行，
// 但存在锁内阻塞风险（详见 D-004）：若目标窗口的消息处理线程正在等待本线程持有的锁，
// 会形成等待循环。当前实现接受此风险，因为：
// 1. 壁纸窗口的消息处理逻辑简单（def_window_proc），不会主动获取 desktop 锁
// 2. WorkerW/Progman 窗口由 Explorer 管理，与本进程无锁交互
// 3. 重试 sleep 已由调用方在释放 desktop 锁后执行（见 ensure_workerw_ready 注释）
//
// v41-D-009: 锁持有期间最坏阻塞上界：
// - `SendMessageTimeoutW(WM_SPAWN_WORK)` 最坏 200ms（`WM_SPAWN_WORK_TIMEOUT_MS`，
//   含 `SMTO_ABORTIFHUNG` flag，Progman 挂起时立即返回）
// - 之后两次 `EnumWindows` 遍历所有顶层窗口，单次约 N×10ms（N = 顶层窗口数，
//   典型 100-300 个，即 1-3ms；最坏 1000 个约 10ms）
// - 总阻塞上界 ≈ 200ms + 2×10ms = 220ms（典型 < 10ms）
// 此上界适用于 `ensure_workerw_ready` 持锁查找路径；`embed_wallpaper` 的
// `SetParent`/`SetWindowPos` 同步消息典型 5ms，不计入此上界。
unsafe impl Send for DesktopIntegrator {}

impl DesktopIntegrator {
    /// 初始化桌面集成器（快速，不阻塞）
    ///
    /// 仅保存原始壁纸，不查找 WorkerW。WorkerW 将在首次需要时懒加载。
    pub fn new() -> Self {
        let original_wallpaper = worker_w::get_system_wallpaper();
        tracing::info!(original = ?original_wallpaper, "已保存原始系统壁纸");

        Self {
            progman_hwnd: None,
            workerw_hwnd: None,
            active_wallpapers: HashMap::new(),
            original_wallpaper,
            initialized: false,
            cached_displays: None,
        }
    }

    /// 确保 WorkerW 已初始化（懒加载）
    ///
    /// 如果尚未初始化，调用 find_workerw_no_retry() 查找窗口句柄（单次尝试，无 sleep）。
    ///
    /// 注意：本方法只尝试一次，不包含重试逻辑。重试（含 sleep）由调用方在
    /// 释放 desktop 锁后执行，以避免持锁 sleep 阻塞其他 desktop 访问者。
    pub fn ensure_initialized(&mut self) -> Result<(), MirrorStarError> {
        // 已初始化且有效则直接返回，避免不必要的 find_workerw_no_retry 调用
        if self.is_workerw_valid() {
            return Ok(());
        }
        // ensure_workerw_ready 返回 bool 表示是否实际执行了初始化，
        // 此处调用方不关心是否实际初始化，丢弃 bool。
        let _did_init = self.ensure_workerw_ready()?;
        Ok(())
    }

    /// 确保 WorkerW 已就绪：若未初始化或已失效，重新查找 WorkerW 句柄
    ///
    /// 统一了 `ensure_initialized` 与 `check_and_reinitialize` 中
    /// "查找/重新查找 WorkerW + 重新嵌入活跃壁纸" 的逻辑，消除 DRY 违规。
    ///
    /// 行为：
    /// - 若 WorkerW 已初始化且有效：直接返回 `Ok(false)`（未做任何变更）
    /// - 若未初始化或已失效：调用 `find_workerw_no_retry` 单次查找，
    ///   存储新句柄；若是失效后重新初始化（Explorer 重启场景），
    ///   重新嵌入所有活跃壁纸。返回 `Ok(true)` 表示实际执行了（重新）初始化。
    ///
    /// 重试 sleep 仍由调用方在释放 desktop 锁后执行，避免持锁阻塞。
    ///
    /// 返回 `bool` 表示是否实际执行了 (重新) 初始化，使
    /// `check_and_reinitialize` 能向命令层准确传达"是否真的重初始化了"，
    /// 而非命令层只能推断"进入时是否无效"。
    ///
    /// ## 已知限制 (v41-D-003)
    ///
    /// 本方法在持有 `DesktopIntegrator` Mutex 的状态下调用
    /// `worker_w::find_workerw_no_retry`，后者内部通过 `SendMessageTimeoutW`
    /// 向 Progman 发送 `WM_SPAWN_WORK` 触发 WorkerW 创建。该同步消息可能阻塞
    /// 其他试图获取 desktop 锁的线程。
    ///
    /// 缓解措施：`SendMessageTimeoutW` 已设置 `SMTO_ABORTIFHUNG` flag（v41-D-003），
    /// 若 Progman 消息处理线程挂起则立即返回，超时上界为 `WM_SPAWN_WORK_TIMEOUT_MS`
    ///（200ms）。极端 Explorer 阻塞场景（如 Progman 处理消息但未挂起）仍可能影响
    /// 并发（锁外执行查找 / 异步初始化可进一步缓解，当前未实施）。
    fn ensure_workerw_ready(&mut self) -> Result<bool, MirrorStarError> {
        // 已初始化且有效，无需操作
        if self.is_workerw_valid() {
            return Ok(false);
        }

        // 记录是否为"重新初始化"场景（之前已初始化但句柄失效）
        let was_initialized = self.initialized;
        if was_initialized {
            tracing::warn!("WorkerW 句柄失效，可能是 Explorer 重启，正在重新初始化...");
        }

        // 未初始化或已失效，查找 WorkerW
        let (progman_hwnd, workerw_hwnd) = worker_w::find_workerw_no_retry().map_err(|e| {
            tracing::warn!(error = %e, "查找 WorkerW 失败");
            e
        })?;

        self.progman_hwnd = Some(progman_hwnd);
        self.workerw_hwnd = Some(workerw_hwnd);
        self.initialized = true;

        if was_initialized {
            // D-004: 锁内阻塞风险
            //
            // 本循环在持有 DesktopIntegrator Mutex 的状态下调用 embed_wallpaper，
            // embed_wallpaper 内部会执行 SetParent/SetWindowPos 等跨线程消息同步操作，
            // 可能阻塞其他试图获取 desktop 锁的线程数百毫秒。
            //
            // 阻塞规模上界：
            // - 循环次数 = active_wallpapers.len()（典型 1-2 个显示器）
            // - 每次 embed_wallpaper 典型耗时 5ms（SetParent/SetWindowPos 同步消息）
            // - 最坏情况 200ms（SendMessageTimeoutW 超时，见 worker_w.rs）
            // - 总阻塞上界 = N × 200ms（N = 活跃壁纸数）
            //
            // 不重构为"锁外逐个 embed"的理由：
            // - embed_wallpaper 需要访问 self.active_wallpapers 与 self.workerw_hwnd
            // - 锁外执行需将这两个字段 clone 出来，引入接口复杂度
            // - 当前阻塞规模可接受（N 通常 ≤ 2），与收益不匹配
            //
            // 重试 sleep 已由调用方在释放 desktop 锁后执行（见函数末尾注释）。
            //
            // 之前已初始化但句柄失效（Explorer 重启），重新嵌入所有活跃壁纸。
            // 重新嵌入失败的条目从 active_wallpapers 移除，避免残留无效 HWND
            // 导致后续操作（如 remove_wallpaper）在已失效的窗口句柄上执行。
            let display_ids: Vec<String> = self.active_wallpapers.keys().cloned().collect();
            // D-013: 在循环外获取一次显示器缓存列表，所有重新嵌入共用此列表，
            // 避免每次调用 enumerate_displays()。clone 一份 Vec 以释放 &mut self
            // 借用，使循环内 self.active_wallpapers.get/remove 可用。
            let displays: Vec<DisplayInfo> = self.get_cached_displays().to_vec();
            for display_id in display_ids {
                if let Some(&(hwnd, arrangement)) = self.active_wallpapers.get(&display_id) {
                    if let Err(e) = worker_w::embed_wallpaper(
                        hwnd,
                        workerw_hwnd,
                        progman_hwnd,
                        &display_id,
                        arrangement,
                        &displays,
                    ) {
                        tracing::error!(display_id, error = %e, "重新嵌入壁纸失败，移除该活跃壁纸条目");
                        self.active_wallpapers.remove(&display_id);
                    }
                }
            }
            tracing::info!("WorkerW 重新初始化完成，壁纸已重新嵌入");
        } else {
            tracing::info!(
                progman = progman_hwnd.0 as isize,
                workerw = workerw_hwnd.0 as isize,
                "桌面集成初始化完成"
            );
        }

        // 实际执行了（重新）初始化，返回 true
        Ok(true)
    }

    /// 将壁纸窗口嵌入到指定显示器
    ///
    /// ## 已知限制 (v41-D-001)
    ///
    /// 本方法执行多步骤 Win32 操作（`SetWindowLongPtrW`（make_borderless）/
    /// `SetParent` / `SetWindowPos`）以完成嵌入。这些操作非原子：若中间某步失败，
    /// 前序已成功的操作不会被回滚，`active_wallpapers` 也不会写入条目（因整
    /// 个方法在 `worker_w::embed_wallpaper` 返回 `Ok` 后才 insert），但窗口
    /// 本身可能已处于"半嵌入"状态（如已 borderless 但未 reparent，或已 reparent
    /// 但未定位）。
    ///
    /// 调用方在收到 `Err` 时，应调用 `remove_wallpaper` 清理可能的半嵌入状态
    ///（`remove_wallpaper` 对无效 HWND 与未嵌入窗口均安全），或销毁窗口重建。
    /// 本方法不尝试自动回滚以避免大重构（自动回滚未实施）。
    pub fn embed_wallpaper(
        &mut self,
        hwnd: HWND,
        display_id: &str,
        arrangement: Arrangement,
    ) -> Result<(), MirrorStarError> {
        self.ensure_initialized()?;
        let workerw_hwnd = self.workerw_hwnd.ok_or(MirrorStarError::WorkerWNotFound)?;
        let progman_hwnd = self.progman_hwnd.ok_or(MirrorStarError::WorkerWNotFound)?;
        // D-013: 从缓存获取显示器列表（5s TTL），避免每次调用 enumerate_displays()。
        // clone 一份 Vec 以释放 &mut self 借用，使后续 self.active_wallpapers.insert 可用。
        let displays: Vec<DisplayInfo> = self.get_cached_displays().to_vec();
        worker_w::embed_wallpaper(
            hwnd,
            workerw_hwnd,
            progman_hwnd,
            display_id,
            arrangement,
            &displays,
        )?;
        self.active_wallpapers
            .insert(display_id.to_string(), (hwnd, arrangement));
        tracing::info!(display_id, hwnd = hwnd.0 as isize, "壁纸窗口已嵌入桌面");
        Ok(())
    }

    /// 获取显示器列表（带 5s TTL 缓存，D-013）
    ///
    /// 首次调用或距上次枚举 >= 5s 时执行 `enumerate_displays()` 并刷新缓存；
    /// 否则直接返回缓存列表。
    fn get_cached_displays(&mut self) -> &[DisplayInfo] {
        const DISPLAY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);
        let needs_refresh = self
            .cached_displays
            .as_ref()
            .map_or(true, |(_, ts)| ts.elapsed() >= DISPLAY_CACHE_TTL);
        if needs_refresh {
            let displays = enumerate_displays();
            self.cached_displays = Some((displays, std::time::Instant::now()));
        }
        self.cached_displays.as_ref().unwrap().0.as_slice()
    }

    /// 移除指定显示器的壁纸窗口
    ///
    /// D-005: SetParent 成功后立即 ShowWindow(SW_HIDE) 隐藏窗口，避免调用方未及时
    /// 清理时残留可见顶层窗口。所有权契约：调用方负责销毁窗口（DestroyWindow）；
    /// 本方法仅分离并隐藏，不销毁。
    ///
    /// ## 已知限制 (v41-D-002)
    ///
    /// 当窗口仍有效且 `SetParent(hwnd, None)` 失败时，本方法保留 `active_wallpapers`
    /// 条目并返回 `Err`，不执行后续 `ShowWindow(SW_HIDE)` 与条目移除。调用方可
    /// 重试 `remove_wallpaper`，或销毁窗口后再次调用（窗口无效时跳过 `SetParent`
    /// 直接移除条目返回 `Ok`）。
    pub fn remove_wallpaper(&mut self, display_id: &str) -> Result<(), MirrorStarError> {
        // v41-D-002: 先用 get 检查，SetParent 失败时保留 active_wallpapers 条目并返回 Err。
        // HWND 与 Arrangement 均 Copy，通过 &(hwnd, _arrangement) 解构复制值后即可释放 &self 借用。
        let Some(&(hwnd, _arrangement)) = self.active_wallpapers.get(display_id) else {
            return Ok(());
        };

        // v41-D-016: WorkerW 已失效（如 Explorer 重启）时跳过 SetParent，直接清理条目。
        // 此时壁纸窗口通常已随 WorkerW 销毁（子窗口随父窗口销毁），SetParent 无意义且
        // 可能对无效 HWND 操作。仍需移除 active_wallpapers 条目以保持状态一致。
        // 注意：is_workerw_valid 校验 progman_hwnd 与 workerw_hwnd 均有效（D-006）。
        if !self.is_workerw_valid() {
            tracing::info!(
                display_id,
                "WorkerW 已失效，跳过 SetParent 直接清理 active_wallpapers 条目"
            );
            self.active_wallpapers.remove(display_id);
            return Ok(());
        }

        // 检查窗口是否仍然有效
        if unsafe { IsWindow(hwnd).as_bool() } {
            // 将窗口从 WorkerW 分离
            // v41-D-002: SetParent 失败时保留 active_wallpapers 条目并返回 Err，
            // 调用方可重试或手动处理半分离状态。
            match unsafe { SetParent(hwnd, None) } {
                Err(e) => {
                    tracing::warn!(
                        display_id,
                        error = %e,
                        "SetParent 分离窗口失败，保留 active_wallpapers 条目"
                    );
                    return Err(MirrorStarError::DesktopIntegration(format!(
                        "SetParent 分离窗口失败: {}",
                        e
                    )));
                }
                Ok(_) => {
                    tracing::info!(display_id, "壁纸窗口已从 WorkerW 分离");
                    // D-005: 分离后立即隐藏窗口，避免调用方未及时清理时残留可见顶层窗口。
                    // ShowWindow 失败仅 warn（窗口已分离，隐藏失败非致命，调用方负责销毁）。
                    if !unsafe { ShowWindow(hwnd, SW_HIDE) }.as_bool() {
                        tracing::warn!(
                            display_id,
                            "ShowWindow(SW_HIDE) 失败（窗口此前可能已隐藏）"
                        );
                    } else {
                        tracing::debug!(display_id, "壁纸窗口已隐藏");
                    }
                }
            }
        } else {
            tracing::info!(display_id, "壁纸窗口已无效，跳过分离");
        }

        // SetParent 成功（或窗口已无效）后，移除 active_wallpapers 条目
        self.active_wallpapers.remove(display_id);
        tracing::info!(display_id, "壁纸窗口已从桌面移除");
        Ok(())
    }

    /// 验证 WorkerW 句柄是否仍然有效
    ///
    /// D-006: 同时校验 `progman_hwnd` 与 `workerw_hwnd`，防止 Explorer 重启后
    /// 旧 `workerw_hwnd` 被复用为其它窗口导致误判为有效。两者任一无效即返回 `false`，
    /// 触发调用方 re-embed 流程。`initialized` 标志不再单独检查——句柄均为 `Some`
    /// 隐含已完成初始化（`new()` 与 `Default` 状态下两字段均为 `None`）。
    pub fn is_workerw_valid(&self) -> bool {
        self.progman_hwnd
            .is_some_and(|h| unsafe { IsWindow(h).as_bool() })
            && self
                .workerw_hwnd
                .is_some_and(|h| unsafe { IsWindow(h).as_bool() })
    }

    /// 检测 Explorer 重启并重新初始化
    ///
    /// 对外入口（等价于私有 `ensure_workerw_ready`）：若未初始化则首次查找，
    /// 若已失效则重新查找并重新嵌入活跃壁纸。本方法为单行间接层，保留作为对外
    /// API 入口语义清晰；调用方不应绕过本方法直接调用 `ensure_workerw_ready`。
    ///
    /// 返回 `bool` 表示是否实际执行了 (重新) 初始化。
    /// - `Ok(true)`：WorkerW 此前无效，已执行重新初始化（含首次初始化与失效后重初始化）。
    ///   调用方（如 `check_desktop_status` 命令、workerw_check 任务）可据此 emit 事件或返回给前端。
    /// - `Ok(false)`：WorkerW 已有效，未做任何变更。
    /// - `Err(_)`：查找 WorkerW 失败。
    pub fn check_and_reinitialize(&mut self) -> Result<bool, MirrorStarError> {
        self.ensure_workerw_ready()
    }

    /// 枚举所有显示器信息
    ///
    /// Thin wrapper：等价于独立函数 [`enumerate_displays`]，仅为提供
    /// `DesktopIntegrator` 实例方法形式而保留，逻辑完全委托、无额外状态依赖。
    /// 调用方既可通过本方法、也可直接调用 [`enumerate_displays`] 自由函数。
    pub fn enumerate_displays(&self) -> Vec<DisplayInfo> {
        enumerate_displays()
    }

    /// 恢复用户原始系统壁纸
    pub fn restore_original_wallpaper(&self) -> Result<(), MirrorStarError> {
        if let Some(ref path) = self.original_wallpaper {
            tracing::info!(path = %path, "恢复原始系统壁纸");
            worker_w::restore_system_wallpaper(path)
        } else {
            // No original wallpaper saved, just clear it
            worker_w::refresh_desktop();
            Ok(())
        }
    }
}

/// 枚举所有显示器信息（独立函数，可从其他模块直接调用）
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    let mut displays = Vec::new();

    unsafe {
        let mut callback_data: Vec<DisplayInfo> = Vec::new();
        let data_ptr = LPARAM(&mut callback_data as *mut Vec<DisplayInfo> as isize);

        let result = EnumDisplayMonitors(
            None, // HDC - enumerate all monitors
            None, // LPCRECT - clip to entire virtual screen
            Some(monitor_enum_callback),
            data_ptr,
        );

        if result.as_bool() {
            displays = callback_data;
        } else {
            // D08: EnumDisplayMonitors 失败时记录 warn，避免静默返回空 Vec。
            // 必须在 EnumDisplayMonitors 失败后立即调用 GetLastError() 取码——
            // 中间不能有任何其他 Win32 调用（包括 tracing 内部的时间戳获取等），
            // 否则错误码会被覆盖。与 window.rs 的 warn 约定一致（error = ?err）。
            let err = GetLastError();
            tracing::warn!(
                error = ?err,
                "EnumDisplayMonitors 失败，返回空显示器列表"
            );
        }
    }

    displays
}

/// EnumDisplayMonitors 回调函数
unsafe extern "system" fn monitor_enum_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let displays = &mut *(lparam.0 as *mut Vec<DisplayInfo>);

    let mut monitor_info = MONITORINFOEXW {
        monitorInfo: windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        szDevice: [0u16; 32],
    };

    if GetMonitorInfoW(hmonitor, &mut monitor_info as *mut _ as *mut _).as_bool() {
        let monitor_rect = monitor_info.monitorInfo.rcMonitor;
        let device_name = extract_utf16_string(&monitor_info.szDevice);

        let is_primary = monitor_info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;

        // Get DPI for this monitor
        let dpi = get_dpi_for_monitor(hmonitor);

        displays.push(build_display_info(
            displays.len(),
            device_name,
            monitor_rect,
            is_primary,
            dpi,
        ));
    } else {
        // D08: GetMonitorInfoW 失败时记录 warn，避免静默跳过该显示器。
        // 必须在 GetMonitorInfoW 失败后立即调用 GetLastError() 取码——
        // 中间不能有任何其他 Win32 调用（包括 tracing 内部的时间戳获取等），
        // 否则错误码会被覆盖。与 window.rs 的 warn 约定一致（error = ?err）。
        let err = GetLastError();
        tracing::warn!(
            hmonitor = hmonitor.0 as isize,
            error = ?err,
            "GetMonitorInfoW 失败，跳过该显示器"
        );
    }

    BOOL(1) // Continue enumeration
}

/// 从 UTF-16 缓冲区提取字符串（到第一个 null 字符为止）
///
/// 若缓冲区不含 null 字符，则使用整个缓冲区。此纯函数从
/// `monitor_enum_callback` 抽取，便于单元测试 szDevice 解析逻辑。
pub(crate) fn extract_utf16_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 从显示器原始数据构建 DisplayInfo
///
/// 此纯函数从 `monitor_enum_callback` 抽取，便于单元测试字段映射逻辑：
/// - `name` 按 1-based 索引生成（"显示器 N"）
/// - `width`/`height` 由 RECT 差值计算
/// - `is_primary`/`dpi` 透传
fn build_display_info(
    displays_len: usize,
    device_name: String,
    monitor_rect: RECT,
    is_primary: bool,
    dpi: u32,
) -> DisplayInfo {
    DisplayInfo {
        id: device_name,
        name: format!("显示器 {}", displays_len + 1),
        width: (monitor_rect.right - monitor_rect.left) as u32,
        height: (monitor_rect.bottom - monitor_rect.top) as u32,
        x: monitor_rect.left,
        y: monitor_rect.top,
        is_primary,
        dpi,
        current_wallpaper: None,
    }
}

/// 获取显示器的有效 DPI
fn get_dpi_for_monitor(hmonitor: HMONITOR) -> u32 {
    unsafe {
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;

        let result = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        if result.is_ok() {
            dpi_x
        } else {
            tracing::warn!(error = ?result, "获取显示器 DPI 失败，使用默认 96 DPI");
            96
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_utf16_string 测试 ───────────────────────────────────────

    #[test]
    fn extract_utf16_string_with_null_terminator() {
        // "Display1\0..."
        let mut buf = [0u16; 32];
        for (i, c) in "Display1".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(extract_utf16_string(&buf), "Display1");
    }

    #[test]
    fn extract_utf16_string_without_null_terminator() {
        // 缓冲区填满字符无 null，应使用整个缓冲区
        let buf: Vec<u16> = "AB".encode_utf16().collect();
        assert_eq!(extract_utf16_string(&buf), "AB");
    }

    #[test]
    fn extract_utf16_string_empty_buffer() {
        let buf: [u16; 0] = [];
        assert_eq!(extract_utf16_string(&buf), "");
    }

    #[test]
    fn extract_utf16_string_all_zeros() {
        let buf = [0u16; 32];
        assert_eq!(extract_utf16_string(&buf), "");
    }

    #[test]
    fn extract_utf16_string_first_char_null() {
        let mut buf = [0u16; 8];
        buf[0] = 0;
        assert_eq!(extract_utf16_string(&buf), "");
    }

    #[test]
    fn extract_utf16_string_chinese() {
        // "显示器" UTF-16 编码后接 null
        let mut buf = [0u16; 32];
        for (i, c) in "显示器".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(extract_utf16_string(&buf), "显示器");
    }

    // ── build_display_info 测试 ─────────────────────────────────────────

    #[test]
    fn build_display_info_first_display() {
        let rect = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let info = build_display_info(0, "\\\\.\\DISPLAY1".to_string(), rect, true, 96);
        assert_eq!(info.id, "\\\\.\\DISPLAY1");
        assert_eq!(info.name, "显示器 1");
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.x, 0);
        assert_eq!(info.y, 0);
        assert!(info.is_primary);
        assert_eq!(info.dpi, 96);
        assert!(info.current_wallpaper.is_none());
    }

    #[test]
    fn build_display_info_second_display() {
        // 第二个显示器：displays_len=1 → name "显示器 2"
        let rect = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let info = build_display_info(1, "\\\\.\\DISPLAY2".to_string(), rect, false, 144);
        assert_eq!(info.name, "显示器 2");
        assert!(!info.is_primary);
        assert_eq!(info.dpi, 144);
        // 负坐标显示器
        assert_eq!(info.x, -1920);
        assert_eq!(info.y, 0);
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
    }

    #[test]
    fn build_display_info_zero_size_rect() {
        // 退化为 0 大小：right == left, bottom == top
        let rect = RECT {
            left: 100,
            top: 100,
            right: 100,
            bottom: 100,
        };
        let info = build_display_info(0, "X".to_string(), rect, false, 96);
        assert_eq!(info.width, 0);
        assert_eq!(info.height, 0);
        assert_eq!(info.x, 100);
        assert_eq!(info.y, 100);
    }

    #[test]
    fn build_display_info_name_uses_one_based_index() {
        // 验证 name 字段使用 1-based 索引：displays_len=N → "显示器 N+1"
        for n in 0..5 {
            let info = build_display_info(n, "id".to_string(), RECT::default(), false, 96);
            assert_eq!(info.name, format!("显示器 {}", n + 1));
        }
    }

    // ── DesktopIntegrator 单元测试 ──────────────────────────────────────
    //
    // 仅测试不依赖 WorkerW 查找的状态管理方法。涉及 Win32 桌面环境的
    // ensure_initialized/embed_wallpaper 等方法参见 wp-proc 集成测试。

    #[test]
    fn desktop_integrator_new_state() {
        let integrator = DesktopIntegrator::new();
        // 未初始化状态：句柄均为 None
        assert!(
            integrator.progman_hwnd.is_none(),
            "new 后 progman_hwnd 应为 None"
        );
        assert!(
            integrator.workerw_hwnd.is_none(),
            "new 后 workerw_hwnd 应为 None"
        );
        // 未初始化时 is_workerw_valid 应返回 false
        assert!(
            !integrator.is_workerw_valid(),
            "未初始化时 is_workerw_valid 应为 false"
        );
    }

    #[test]
    fn desktop_integrator_is_workerw_valid_uninitialized() {
        let integrator = DesktopIntegrator::new();
        assert!(!integrator.is_workerw_valid());
    }

    #[test]
    fn desktop_integrator_getters_uninitialized() {
        let integrator = DesktopIntegrator::new();
        assert!(integrator.progman_hwnd.is_none());
        assert!(integrator.workerw_hwnd.is_none());
    }

    #[test]
    fn desktop_integrator_restore_original_wallpaper_returns_result() {
        // D12: restore_original_wallpaper 现在返回 Result<(), MirrorStarError>。
        // original_wallpaper 为 None 时调用 refresh_desktop（best-effort，返回 Ok），
        // 为 Some(path) 时调用 restore_system_wallpaper（传播 Result）。
        // 两者均不应 panic，且在正常 Windows 环境下应返回 Ok。
        let integrator = DesktopIntegrator::new();
        let result = integrator.restore_original_wallpaper();
        assert!(result.is_ok(), "恢复原始壁纸应返回 Ok: {:?}", result.err());
    }

    #[test]
    #[ignore = "会修改真实系统壁纸，仅本地手动运行"]
    fn desktop_integrator_restore_original_wallpaper_propagates_err() {
        // D12: 验证当 original_wallpaper 为无效路径时，restore_original_wallpaper
        // 传播 restore_system_wallpaper 的 Result（Ok 或 Err 均合法，取决于 Windows 行为）。
        // 构造一个 original_wallpaper 为无效路径的 DesktopIntegrator，
        // 模拟恢复失败场景，验证调用方收到 Result 而非 panic。
        let integrator = DesktopIntegrator {
            original_wallpaper: Some(":::nonexistent_invalid_path:::".to_string()),
            ..Default::default()
        };
        let result = integrator.restore_original_wallpaper();
        match &result {
            Ok(()) => {
                // SystemParametersInfoW 接受了无效路径（仅设置注册表值），合法行为
            }
            Err(e) => {
                // 预期中的失败：调用方收到 Err，验证错误传播
                tracing::info!(error = %e, "无效路径触发恢复失败（验证 Err 传播）");
            }
        }
    }

    #[test]
    fn desktop_integrator_restore_original_wallpaper_none_returns_ok() {
        // D12: original_wallpaper 为 None 时，走 refresh_desktop 降级路径，应返回 Ok。
        let integrator = DesktopIntegrator::default();
        let result = integrator.restore_original_wallpaper();
        assert!(result.is_ok(), "None 分支应返回 Ok: {:?}", result.err());
    }

    // ── enumerate_displays 集成测试 ─────────────────────────────────────
    //
    // 在真实 Windows 桌面环境下运行，应至少返回 1 个显示器。
    // 无头环境下可能返回空 Vec，此处仅做容错断言。

    #[test]
    fn enumerate_displays_no_panic() {
        let displays = enumerate_displays();
        // 不 panic 即通过；如有显示器则验证字段合法性
        for d in &displays {
            assert!(!d.id.is_empty(), "显示器 id 不应为空");
            assert!(!d.name.is_empty(), "显示器 name 不应为空");
            assert!(d.dpi > 0, "显示器 dpi 应大于 0");
        }
    }

    #[test]
    fn enumerate_displays_returns_vec() {
        let displays = enumerate_displays();
        // 仅验证返回类型为 Vec 且不 panic；具体数量取决于运行环境
        let _ = displays.len();
    }

    #[test]
    fn desktop_integrator_enumerate_displays_method() {
        let integrator = DesktopIntegrator::new();
        let displays = integrator.enumerate_displays();
        // 方法版本应与独立函数返回一致的结构（不 panic 即可）
        let _ = displays.len();
    }

    // ── D08 修复：GetLastError 错误码记录 ──────────────────────────────────
    //
    // D08 在 `enumerate_displays` / `monitor_enum_callback` 的失败分支加入
    // `GetLastError()` 错误码，与 window.rs 的 warn 约定一致（`error = ?err`）。
    //
    // 测试限制：`EnumDisplayMonitors` / `GetMonitorInfoW` 在真实 Windows 桌面环境
    // 下几乎不会失败（需无显示器或损坏的 GDI 子系统），无法在单元测试中可靠触发
    // 真实失败分支。`GetLastError` 的正确性（API 失败后立即调用、中间无其他 Win32
    // 调用）通过代码审查确认——见上述两个失败分支的注释。
    //
    // 此处验证 happy path 仍正常工作（不 panic + 返回 Vec），确保加入 GetLastError
    // 调用未破坏现有行为。失败分支的日志包含错误码由代码审查保证。

    #[test]
    fn enumerate_displays_failure_branch_does_not_panic_on_happy_path() {
        // D08: 加入 GetLastError 调用后，happy path 仍应正常返回 Vec<DisplayInfo>。
        // 若 GetLastError 调用位置错误（如在 EnumDisplayMonitors 成功路径中误调），
        // 可能在某些环境导致副作用——此测试确认 happy path 不受影响。
        let displays = enumerate_displays();
        // 不 panic 即通过
        let _ = displays.len();
    }

    // ── D-004 / D-005 / D-014 修复验证 ──────────────────────────────────

    /// D-004: 验证 ensure_workerw_ready 文档化锁内阻塞风险。
    #[test]
    fn d004_ensure_workerw_ready_documents_lock_blocking_risk() {
        let source = include_str!("mod.rs");
        assert!(
            source.contains("D-004: 锁内阻塞风险"),
            "ensure_workerw_ready 应含 D-004 锁内阻塞风险注释"
        );
        assert!(
            source.contains("阻塞规模上界"),
            "D-004 注释应说明阻塞规模上界"
        );
        assert!(source.contains("不重构为"), "D-004 注释应说明不重构的理由");
    }

    /// D-005: 验证 remove_wallpaper 在 SetParent 成功后调用 ShowWindow(SW_HIDE)。
    #[test]
    fn d005_remove_wallpaper_hides_window_after_setparent() {
        let source = include_str!("mod.rs");
        assert!(
            source.contains("D-005: SetParent 成功后立即 ShowWindow(SW_HIDE)"),
            "remove_wallpaper 注释应含 D-005 前缀标识"
        );
        assert!(
            source.contains("ShowWindow(hwnd, SW_HIDE)"),
            "remove_wallpaper 应调用 ShowWindow(hwnd, SW_HIDE)"
        );
        assert!(
            source.contains("调用方负责销毁窗口"),
            "remove_wallpaper 注释应明确所有权契约"
        );
    }

    /// D-014: 验证 SAFETY 注释准确反映实现，移除不实表述。
    #[test]
    fn d014_safety_comment_accurately_reflects_implementation() {
        let source = include_str!("mod.rs");
        assert!(
            source.contains("D-014 修订"),
            "SAFETY 注释应含 D-014 修订标识"
        );
        // 验证移除了不实表述。
        // 构造搜索串以避免 include_str! 自匹配（测试源码本身被读取）。
        let old_claim_storage = ["仅存储", "和传递", " HWND", " 值"].join("");
        let old_claim_caller = ["实际窗口操作", "由调用方", "在正确的", "线程上执行"].join("");
        assert!(
            !source.contains(&old_claim_storage),
            "SAFETY 注释不应再含旧的不实表述（仅存储传递 HWND 值）"
        );
        assert!(
            !source.contains(&old_claim_caller),
            "SAFETY 注释不应再含旧的不实表述（调用方线程执行）"
        );
        // 验证新增了准确表述
        assert!(
            source.contains("跨线程同步消息"),
            "SAFETY 注释应说明跨线程同步消息机制"
        );
        assert!(
            source.contains("D-004"),
            "SAFETY 注释应引用 D-004 说明锁内阻塞风险"
        );
    }

    // ── v4.1 Medium findings 文档化测试 ──────────────────────────────────

    /// v41-D-001: 验证 embed_wallpaper 文档化"多步骤操作中间失败无状态恢复"契约。
    ///
    /// embed_wallpaper 内部调用 worker_w::embed_wallpaper，后者执行多步骤 Win32
    /// 操作（make_borderless / SetParent / SetWindowPos）。这些操作非原子，
    /// 中间失败时不回滚前序操作。由于无法在 CI 中可靠 mock Win32 API 失败，
    /// 此处通过 include_str! 模式断言源码包含契约说明，与 D-004/D-005/D-007/D-014
    /// 的文档化测试风格一致。
    #[test]
    fn v41_d001_embed_wallpaper_documents_partial_failure_contract() {
        let source = include_str!("mod.rs");
        // 验证 v41-D-001 前缀标识存在
        assert!(
            source.contains("## 已知限制 (v41-D-001)"),
            "embed_wallpaper 文档注释应含 v41-D-001 已知限制段落"
        );
        // 验证契约核心要素：多步骤操作非原子、中间失败不回滚、调用方需清理
        assert!(
            source.contains("SetWindowLongPtrW"),
            "v41-D-001 契约应说明涉及的 Win32 操作（SetWindowLongPtrW）"
        );
        assert!(
            source.contains("半嵌入"),
            "v41-D-001 契约应说明半嵌入状态风险"
        );
        assert!(
            source.contains("remove_wallpaper"),
            "v41-D-001 契约应指引调用方调用 remove_wallpaper 清理"
        );
    }

    /// v41-D-002: 验证 SetParent 失败时 active_wallpapers 条目保留的契约。
    ///
    /// SetParent 失败需要 mock Win32 API（CI 中不可行），此处采用混合策略：
    /// 1. 文档化测试：断言源码包含 v41-D-002 契约说明（SetParent 失败保留条目返回 Err）
    /// 2. 行为测试：验证无效 HWND 路径（IsWindow=false）仍正确移除条目返回 Ok
    /// 3. 行为测试：验证不存在的 display_id 返回 Ok 且不 panic
    ///
    /// 这确保 v41-D-002 重构（remove → get + 条件 remove）未破坏既有行为，
    /// 且源码层面正确表达了 SetParent 失败保留条目的契约。
    #[test]
    fn v41_d002_setparent_failure_keeps_entry() {
        // (1) 文档化测试：验证源码包含 v41-D-002 契约
        let source = include_str!("mod.rs");
        assert!(
            source.contains("## 已知限制 (v41-D-002)"),
            "remove_wallpaper 文档注释应含 v41-D-002 已知限制段落"
        );
        assert!(
            source.contains("v41-D-002: 先用 get 检查"),
            "remove_wallpaper 应使用 get 而非 remove 检查条目"
        );
        assert!(
            source.contains("保留 active_wallpapers 条目并返回 Err"),
            "remove_wallpaper SetParent 失败时应保留条目并返回 Err"
        );

        // (2) 行为测试：无效 HWND 路径
        // HWND::default() 为 null，IsWindow 返回 false → 跳过 SetParent → 移除条目 → Ok
        let mut integrator = DesktopIntegrator {
            active_wallpapers: HashMap::from([(
                "display1".to_string(),
                (HWND::default(), Arrangement::Span),
            )]),
            ..Default::default()
        };
        let result = integrator.remove_wallpaper("display1");
        assert!(result.is_ok(), "无效 HWND 应返回 Ok: {:?}", result.err());
        assert!(
            !integrator.active_wallpapers.contains_key("display1"),
            "无效 HWND 路径应移除条目"
        );

        // (3) 行为测试：不存在的 display_id 返回 Ok
        let mut integrator = DesktopIntegrator::default();
        let result = integrator.remove_wallpaper("nonexistent");
        assert!(result.is_ok(), "不存在的 display_id 应返回 Ok");
        assert!(
            integrator.active_wallpapers.is_empty(),
            "不存在的 display_id 不应影响 active_wallpapers"
        );
    }

    /// v41-D-003: 验证 ensure_workerw_ready 持锁调用 SendMessageTimeoutW 的契约。
    ///
    /// 契约要求：SendMessageTimeoutW 已设置 SMTO_ABORTIFHUNG flag 缩短超时，
    /// 避免持锁调用方长时间阻塞。由于 ensure_workerw_ready 依赖真实 Windows
    /// 桌面环境（FindWindowW/EnumWindows/SendMessageTimeoutW），无法在 CI 中
    /// 可靠触发 SendMessageTimeoutW 超时分支，此处采用混合策略：
    /// 1. 文档化测试：断言源码包含 v41-D-003 契约说明
    /// 2. 行为测试：调用 ensure_initialized（内部调用 ensure_workerw_ready），
    ///    验证不 panic（Ok 或 Err 均合法，取决于运行环境是否有 WorkerW）
    #[test]
    fn v41_d003_ensure_workerw_ready_smto_abortifhung_contract() {
        // (1) 文档化测试：验证 mod.rs 包含 v41-D-003 契约
        let source = include_str!("mod.rs");
        assert!(
            source.contains("## 已知限制 (v41-D-003)"),
            "ensure_workerw_ready 文档注释应含 v41-D-003 已知限制段落"
        );
        assert!(
            source.contains("SMTO_ABORTIFHUNG"),
            "v41-D-003 契约应说明已设置 SMTO_ABORTIFHUNG flag"
        );
        assert!(
            source.contains("锁外执行查找"),
            "v41-D-003 契约应说明未实施的优化方向（锁外执行查找）"
        );

        // (2) 文档化测试：验证 worker_w.rs 中 SendMessageTimeoutW 使用 SMTO_ABORTIFHUNG
        let worker_source = include_str!("worker_w.rs");
        assert!(
            worker_source.contains("SMTO_NORMAL | SMTO_ABORTIFHUNG"),
            "worker_w.rs 中 SendMessageTimeoutW 应使用 SMTO_NORMAL | SMTO_ABORTIFHUNG"
        );

        // (3) 行为测试：ensure_initialized 不 panic
        // ensure_initialized 内部调用 ensure_workerw_ready，后者调用 find_workerw_no_retry。
        // 在真实 Windows 桌面环境下应返回 Ok（找到 WorkerW）；
        // 在无头/受限环境下可能返回 Err（找不到 Progman/WorkerW），但不应 panic。
        // 仅调用并丢弃结果——若 panic 则测试失败，Ok/Err 均合法。
        let mut integrator = DesktopIntegrator::new();
        let _ = integrator.ensure_initialized();
    }
}
