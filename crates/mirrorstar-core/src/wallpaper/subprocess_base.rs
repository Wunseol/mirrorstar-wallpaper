use std::path::PathBuf;

use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, HWND};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetWindowThreadProcessId,
};

use crate::process::manager::{JobObjectGuard, ProcessManager};
use crate::wallpaper::{PauseSender, ScalingMode, WallpaperState};

// ── Connection constants（v41-W-010：集中子进程渲染器连接/重试参数）─────────
//
// 本段落集中子进程渲染器（mpv / wp-proc）的 IPC 连接重试参数与窗口查找重试参数，
// 统一 `{PROC}_CONNECT_{RETRIES|INTERVAL_MS}` 与 `WINDOW_FIND_{RETRIES|INTERVAL_MS}`
// 命名风格。原分散于 video.rs（MPV_*）与 web.rs（WP_PROC_*），集中后便于调整 IPC
// 超时与排查连接问题。常量由对应渲染器通过 `use super::subprocess_base::{...}` 引用。

/// 窗口查找重试次数（W-013）
const WINDOW_FIND_RETRIES: u32 = 20;
/// 窗口查找重试间隔（ms，W-013）
const WINDOW_FIND_INTERVAL_MS: u64 = 100;
/// mpv IPC 连接重试次数
///
/// v5.2 修复：从 15 次（3s）增加到 30 次（6s）。
/// Wave v9-C：重试间隔从 200ms 降至 50ms，重试次数相应调整为 120 次
/// （120 次 × 50ms = 6s 兜底），保持总超时不变，降低管道就绪检测延迟
/// （原 30 次 × 200ms = 6s）。
///
/// 冷启动优化（Task 7.4）：从 120 次（6s）缩减到 40 次（2s）。
///
/// 缩减理由：
/// - **主场景已无冷启动**：全屏分级后，最大化窗口（IDE/浏览器）只暂停不终止
///   （进程驻留），恢复零延迟；仅真全屏（游戏）才终止 mpv。真全屏退出恢复走
///   后台线程异步执行，`resume_all_fast` 的 `play()` 不再阻塞 Win32 回调线程，
///   因此 2s 的 IPC 连接窗口不再导致 UI 卡顿。
/// - **极端冷启动兜底**：mpv 冷启动 3-5s（GPU 驱动初始化 + DLL 加载 + 杀毒扫描）
///   的极端场景下，2s 内未连接成功会返回 Err 并触发 `play()` 失败清理
///   （terminate 孤儿进程），由后台恢复线程失败保留 FULLSCREEN_WAS 标志、
///   周期复查线程（2s 间隔）自动重试。重试通常命中 warm cache（~600ms）即成功，
///   比一次性 6s 阻塞等待更快恢复壁纸。
/// - **恢复延迟优先**：真全屏场景用户刚从游戏退出，期望壁纸尽快回来；
///   2s 失败 + 复查重试的最坏恢复时间仍显著低于旧实现的 6s 同步阻塞。
pub(crate) const MPV_CONNECT_RETRIES: u32 = 40;
/// mpv IPC 连接重试间隔（毫秒）
///
/// Wave v9-C：从 200ms 降至 50ms，配合 `MPV_CONNECT_RETRIES = 40` 保持
/// 2s 总超时不变（40 × 50ms = 2s），降低管道就绪后到下次检测的等待延迟。
pub(crate) const MPV_CONNECT_INTERVAL_MS: u64 = 50;
/// wp-proc IPC 连接重试次数
///
/// v41-W-006：从 100 次缩减到 40 次（40 * 200ms = 8s 兜底）。
/// Wave v9-C：重试间隔从 200ms 降至 50ms，重试次数相应调整为 160 次
/// （160 次 × 50ms = 8s 兜底），保持总超时不变，降低管道就绪检测延迟
/// （原 40 次 × 200ms = 8s）。
///
/// 原值 100 次 * 200ms = 20s 兜底，覆盖 WebView2 冷启动 5-15s。但 `set_wallpaper`
/// 在 engine 锁内调用 `construct_renderer`，持锁期间阻塞其他命令（pause/resume/
/// set_volume），20s 等待不可接受。8s 在多数环境下足够 WebView2 冷启动
/// （典型 5-15s 的下限），仅在慢速环境（运行时首次下载）下失败，调用方会返回
/// 错误让用户重试，比 20s 阻塞所有命令更可接受。
///
/// 详见 `set_wallpaper` 文档注释 "已知限制 (v41-W-006)" 段落。
pub(crate) const WP_PROC_CONNECT_RETRIES: u32 = 160;
/// wp-proc IPC 连接重试间隔（毫秒）
///
/// Wave v9-C：从 200ms 降至 50ms，配合 `WP_PROC_CONNECT_RETRIES = 160` 保持
/// 8s 总超时不变（160 × 50ms = 8s），降低管道就绪后到下次检测的等待延迟。
pub(crate) const WP_PROC_CONNECT_INTERVAL_MS: u64 = 50;

/// 子进程渲染器公共基类，封装 `VideoRenderer` 和 `WebRenderer` 的共同模式：
/// - `ProcessManager` 管理子进程生命周期
/// - Win32 窗口查找（按标题 + PID 校验）
/// - 捆绑可执行文件查找
/// - 公共状态字段（state / hwnd / scaling_mode / pipe_name / pause_sender）
/// - Job Object 内存限制（v8-B：限制 mpv/wp-proc 子进程内存为 256MB）
///
/// 注意：IPC 客户端因 mpv 与 wp-proc 协议差异较大（方法名、命令集不同），
/// 由具体渲染器自行持有，本基类不涉及 IPC 通信。
pub struct SubprocessRendererBase {
    /// 进程管理器
    pub(crate) process: ProcessManager,
    /// 当前状态
    pub(crate) state: WallpaperState,
    /// 子进程窗口句柄
    pub(crate) hwnd: Option<HWND>,
    /// 缩放模式
    pub(crate) scaling_mode: ScalingMode,
    /// IPC 管道名称
    pub(crate) pipe_name: String,
    /// 快速控制发送端（play() 成功后创建）
    pub(crate) pause_sender: Option<PauseSender>,
    /// v8-B：带内存限制的 Job Object 守卫。
    ///
    /// `ProcessManager` 内部已关联一个仅 `KILL_ON_JOB_CLOSE` 的 Job Object（兜底
    /// 孤儿进程清理），本守卫额外施加 `JOB_OBJECT_LIMIT_PROCESS_MEMORY`（256MB）+
    /// `KILL_ON_JOB_CLOSE`。Windows 8+ 嵌套 Job Object 支持两者叠加生效。
    ///
    /// Drop 顺序（结构体字段声明序）：`process` 先于 `job_guard` drop，
    /// `ProcessManager::drop` 内部 `stop_immediate` 已终止进程，`job_guard` drop
    /// 时 `KILL_ON_JOB_CLOSE` 对已退出进程为 no-op，仅释放内核对象。
    pub(crate) job_guard: Option<JobObjectGuard>,
}

/// v8-B：子进程内存上限（2048MB）。
///
/// 真实根因修复（2026-08-23）：原 256MB 上限会导致 mpv 解码 4K 视频壁纸时
/// D3D11 创建视频纹理失败。实测（独立运行 mpv 加载 4K H.264 视频）：
///   WorkingSet=182MB，Private/Paged=425.5MB
/// mpv 进程的 commit charge 在 4K 硬件解码 + D3D11 纹理分配时超过 256MB，
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` 拦截后续分配，D3D11 返回
/// `E_OUTOFMEMORY (0x8007000e)` → 纹理创建失败 → 桌面黑屏 / mpv 退出。
/// （这正是历次应用实测 mpv 日志反复出现 0x8007000e、而独立 PowerShell 测试
/// 无内存限制却成功的原因——根因 A-E 均为此表象下的次要问题。）
///
/// 提高至 2048MB：为 4K（实测 425MB）保留约 4.8 倍余量，兼容 8K/超高码率
/// 壁纸；同时保留内存泄漏兜底（严重泄漏最终仍会触发上限，KILL_ON_JOB_CLOSE
/// 清理进程）。wp-proc（WebView2）典型 100-200MB，2048MB 上限同样安全。
const SUBPROCESS_MEMORY_LIMIT_BYTES: usize = 2048 * 1024 * 1024;

impl SubprocessRendererBase {
    /// 创建新的子进程渲染器基类
    pub fn new(executable: PathBuf, pipe_name: String, scaling_mode: ScalingMode) -> Self {
        Self {
            process: ProcessManager::new(executable),
            state: WallpaperState::Initializing,
            hwnd: None,
            scaling_mode,
            pipe_name,
            pause_sender: None,
            job_guard: None,
        }
    }

    /// 启动子进程，返回 PID
    ///
    /// v8-B：进程启动后关联 `JobObjectGuard`（256MB 内存限制 + `KILL_ON_JOB_CLOSE`）。
    ///
    /// **关于 `CREATE_SUSPENDED`**：现有 `ProcessManager::start` 未使用 `CREATE_SUSPENDED`，
    /// 进程在 `CreateProcessW` 后立即运行，到 `JobObjectGuard::assign_process` 之间存在
    /// 极短竞争窗口（微秒级），理论上子进程可能在此窗口内 fork 孙进程不受内存限制。
    /// 评估结论：mpv/wp-proc 启动初期仅加载 DLL 与初始化，不会立即 fork 子进程，
    /// 竞争窗口极短可接受。添加 `CREATE_SUSPENDED` 需修改 `ProcessManager::start`，
    /// 增加复杂度且收益有限，故不采用。
    pub fn start_process(&mut self, args: Vec<String>) -> Result<u32, crate::MirrorStarError> {
        // v8-B：重启时先清理旧 job_guard（Drop 触发 KILL_ON_JOB_CLOSE 终止旧进程）
        // 与 ProcessManager::start 内部的 stop_immediate 清理保持同步
        self.job_guard = None;

        let pid = self.process.start(args)?;

        // v8-B：创建带内存限制的 Job Object 并关联子进程
        let job_guard = match JobObjectGuard::new(SUBPROCESS_MEMORY_LIMIT_BYTES) {
            Ok(g) => g,
            Err(e) => {
                // Job Object 创建失败，清理已启动的进程
                let _ = self.process.stop_immediate();
                return Err(e);
            }
        };

        if let Some(handle) = self.process.handle() {
            if let Err(e) = job_guard.assign_process(handle) {
                // 关联失败，清理已启动的进程
                let _ = self.process.stop_immediate();
                return Err(e);
            }
        }

        self.job_guard = Some(job_guard);
        Ok(pid)
    }

    /// 停止子进程（等待退出，超时后强制终止）
    ///
    /// v8-B：停止后清理 `job_guard`（Drop 关闭 Job Object 句柄）。此时进程已由
    /// `process.stop()` 终止，`KILL_ON_JOB_CLOSE` 为 no-op，仅释放内核对象。
    pub fn stop_process(&mut self) -> Result<(), crate::MirrorStarError> {
        let result = self.process.stop();
        // 无论 stop 结果如何，清理 Job Object 守卫
        self.job_guard = None;
        result
    }

    /// 获取子进程 PID
    pub fn process_pid(&self) -> Option<u32> {
        self.process.pid()
    }

    /// 获取壁纸窗口句柄
    pub fn hwnd(&self) -> Option<HWND> {
        self.hwnd
    }

    /// 获取当前壁纸状态
    pub fn state(&self) -> WallpaperState {
        self.state
    }

    /// 设置状态
    pub fn set_state(&mut self, state: WallpaperState) {
        self.state = state;
    }

    /// 设置窗口句柄
    pub fn set_hwnd(&mut self, hwnd: Option<HWND>) {
        self.hwnd = hwnd;
    }

    /// 获取管道名称
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// 复制子进程句柄用于子进程退出监听（W07 修复）
    ///
    /// 返回独立拥有的句柄副本，调用方负责 `CloseHandle`。
    /// 使用 `DuplicateHandle` 确保监听线程的 `WaitForSingleObject` 不会与
    /// `stop()` 中的 `CloseHandle` 产生竞争（Win32 文档明确指出在 wait 期间
    /// 关闭同一句柄会导致未定义行为）。
    ///
    /// 返回 `None` 表示子进程未启动或句柄已被 `stop()` 取走。
    pub fn duplicate_process_handle(&self) -> Option<HANDLE> {
        let src = self.process.handle()?;
        let mut dup = HANDLE::default();
        // SAFETY: DuplicateHandle 是线程安全的 Win32 API。src 句柄由 ProcessManager
        // 管理（pub(crate) 字段），在 stop() 调用前始终有效。GetCurrentProcess()
        // 返回伪句柄，无需关闭。DUPLICATE_SAME_ACCESS 复制相同访问权限。
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                src,
                GetCurrentProcess(),
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .ok()?;
        }
        Some(dup)
    }

    /// 获取缩放模式
    pub fn scaling_mode(&self) -> ScalingMode {
        self.scaling_mode
    }

    /// 设置缩放模式
    pub fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.scaling_mode = mode;
    }

    /// 存储 pause sender
    pub fn set_pause_sender(&mut self, sender: Option<PauseSender>) {
        self.pause_sender = sender;
    }

    /// 在应用目录下查找捆绑的可执行文件，回退到 PATH 查找
    ///
    /// - `subdir`：可执行文件所在的子目录（如 `"mpv"`），`None` 表示直接位于 exe 同目录
    /// - `filename`：可执行文件名（如 `"mpv.exe"` 或 `"mirrorstar-wp-proc.exe"`）
    /// - `log_name`：用于日志显示的名称
    pub fn find_bundled_executable(
        subdir: Option<&str>,
        filename: &str,
        log_name: &str,
    ) -> PathBuf {
        // 1. 检查捆绑的可执行文件（应用目录下）
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(dir) = exe_path.parent() {
                let bundled = match subdir {
                    Some(sd) => dir.join(sd).join(filename),
                    None => dir.join(filename),
                };
                if bundled.exists() {
                    tracing::info!(path = %bundled.display(), "找到捆绑的 {}", log_name);
                    return bundled;
                }
            }
        }

        // 2. 向上回溯仓库根查找（仅对带子目录的调用生效，如 mpv）
        //
        // 开发模式下应用 exe 位于 `target/debug/`，而捆绑的 mpv 位于仓库根 `mpv/`。
        // 从 exe 目录的祖先开始向上回溯（跳过 exe 目录本身，最多 8 层），
        // 检查 `<祖先>/<subdir>/<filename>` 是否存在。优先级：应用 exe 目录 > 仓库根回溯 > PATH。
        if let Some(sd) = subdir {
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(dir) = exe_path.parent() {
                    for ancestor in dir.ancestors().skip(1).take(8) {
                        let repo_bundled = ancestor.join(sd).join(filename);
                        if repo_bundled.exists() {
                            tracing::info!(path = %repo_bundled.display(), "找到仓库根的 {}", log_name);
                            return repo_bundled;
                        }
                    }
                }
            }
        }

        // 3. 让操作系统在 PATH 中查找
        tracing::info!("使用系统 PATH 中的 {}", log_name);
        PathBuf::from(filename)
    }

    /// 通过窗口标题查找子进程窗口，并验证 PID 匹配
    ///
    /// 轮询 `WINDOW_FIND_RETRIES` 次，每次间隔 `WINDOW_FIND_INTERVAL_MS`ms（最大 2 秒）。
    /// `title` 不含 NUL 终止符，本函数内部自动追加。
    pub fn find_window_by_title(pid: u32, title: &str) -> Option<HWND> {
        let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        for _ in 0..WINDOW_FIND_RETRIES {
            // 20 * 100ms = 2 秒
            // SAFETY: FindWindowW 仅按标题字符串查找顶层窗口，不操作调用方内存。
            // title_wide 以 NUL 结尾，PCWSTR 读取有效。此 unsafe 调用迁移自
            // VideoRenderer::find_mpv_window 与 WebRenderer::find_web_window，行为不变。
            let hwnd = unsafe { FindWindowW(None, windows::core::PCWSTR(title_wide.as_ptr())) };

            if let Ok(hwnd) = hwnd {
                if hwnd != HWND::default() && !hwnd.is_invalid() {
                    // 验证 PID 匹配，避免捕获同标题的其他进程窗口
                    let mut window_pid: u32 = 0;
                    // SAFETY: GetWindowThreadProcessId 仅写入 window_pid 局部变量，
                    // hwnd 由 FindWindowW 返回且已校验非空/非无效。迁移自原实现。
                    let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };
                    if window_pid == pid {
                        return Some(hwnd);
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(WINDOW_FIND_INTERVAL_MS));
        }

        None
    }

    /// 通过窗口类名查找子进程窗口，并验证 PID 匹配
    ///
    /// 轮询 `WINDOW_FIND_RETRIES` 次，每次间隔 `WINDOW_FIND_INTERVAL_MS`ms（最大 2 秒）。每次轮询内部枚举所有匹配类名的
    /// 顶层窗口（通过 `hwnd_child_after` 逐个推进），找到 PID 匹配的那个即返回。
    ///
    /// 相比 `find_window_by_title` 的优势：
    /// - 类名由 mpv 编译时固定（`MPV_WINDOW_CLASS_NAME = L"mpv"`），不会被运行时修改；
    /// - 标题可被动态修改或与外部进程冲突，类名查找更稳定；
    /// - 通过 `hwnd_child_after` 枚举可正确处理多个同类窗口，PID 校验精确匹配本进程。
    ///
    /// `class_name` 不含 NUL 终止符，本函数内部自动追加。
    pub fn find_window_by_class(pid: u32, class_name: &str) -> Option<HWND> {
        let class_wide: Vec<u16> = class_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        for _ in 0..WINDOW_FIND_RETRIES {
            // 20 * 100ms = 2 秒
            // 枚举所有匹配类名的顶层窗口，找到 PID 匹配的那个
            // SAFETY: FindWindowExW(parent=NULL, child_after=NULL, class, title=NULL)
            // 查找顶层窗口，仅读取 class_wide 字符串，不操作调用方内存。
            // class_wide 以 NUL 结尾，PCWSTR 读取有效。
            let mut hwnd_prev = HWND::default();
            loop {
                let hwnd = unsafe {
                    FindWindowExW(
                        HWND::default(),
                        hwnd_prev,
                        windows::core::PCWSTR(class_wide.as_ptr()),
                        None,
                    )
                };

                let Ok(hwnd) = hwnd else { break };

                if hwnd == HWND::default() || hwnd.is_invalid() {
                    // 枚举结束（无更多匹配窗口）
                    break;
                }

                // 验证 PID 匹配
                let mut window_pid: u32 = 0;
                // SAFETY: GetWindowThreadProcessId 仅写入 window_pid 局部变量，
                // hwnd 由 FindWindowExW 返回且已校验非空/非无效。
                let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };
                if window_pid == pid {
                    return Some(hwnd);
                }

                // 推进到下一个同类窗口继续枚举
                hwnd_prev = hwnd;
            }

            std::thread::sleep(std::time::Duration::from_millis(WINDOW_FIND_INTERVAL_MS));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_new_initializes_fields() {
        let base = SubprocessRendererBase::new(
            PathBuf::from("dummy.exe"),
            "mirrorstar-test-pipe".to_string(),
            ScalingMode::Fit,
        );

        assert_eq!(base.state(), WallpaperState::Initializing);
        assert!(base.hwnd().is_none());
        assert_eq!(base.scaling_mode(), ScalingMode::Fit);
        assert_eq!(base.pipe_name(), "mirrorstar-test-pipe");
        assert!(base.process_pid().is_none());
    }

    #[test]
    fn base_setters_update_fields() {
        let mut base = SubprocessRendererBase::new(
            PathBuf::from("dummy.exe"),
            "pipe".to_string(),
            ScalingMode::Fill,
        );

        base.set_state(WallpaperState::Playing);
        assert_eq!(base.state(), WallpaperState::Playing);

        base.set_state(WallpaperState::Terminated);
        assert_eq!(base.state(), WallpaperState::Terminated);

        base.set_scaling_mode(ScalingMode::Stretch);
        assert_eq!(base.scaling_mode(), ScalingMode::Stretch);
    }

    #[test]
    fn find_bundled_executable_falls_back_to_filename() {
        // 使用一个确定不存在的文件名，验证回退到 PATH 查找（返回文件名本身）
        let path = SubprocessRendererBase::find_bundled_executable(
            Some("nonexistent-subdir"),
            "definitely-not-here-12345.exe",
            "test-exe",
        );
        assert_eq!(path, PathBuf::from("definitely-not-here-12345.exe"));
    }

    #[test]
    fn find_bundled_executable_no_subdir_falls_back() {
        let path = SubprocessRendererBase::find_bundled_executable(
            None,
            "definitely-not-here-67890.exe",
            "test-exe",
        );
        assert_eq!(path, PathBuf::from("definitely-not-here-67890.exe"));
    }

    #[test]
    fn find_bundled_executable_dev_mode_finds_repo_root_mpv() {
        let path =
            SubprocessRendererBase::find_bundled_executable(Some("mpv"), "mpv.exe", "mpv");

        // 通过 current_exe 的祖先回溯判断仓库根是否存在 mpv/mpv.exe
        let repo_has_mpv = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .map(|dir| {
                dir.ancestors()
                    .skip(1)
                    .take(8)
                    .any(|ancestor| ancestor.join("mpv").join("mpv.exe").exists())
            })
            .unwrap_or(false);

        if repo_has_mpv {
            // 仓库根存在 mpv 时，应找到仓库根 mpv 而非回退到 PATH
            assert_ne!(path, PathBuf::from("mpv.exe"), "应找到仓库根的 mpv，而非回退到 PATH");
            assert_eq!(
                path.file_name().and_then(|s| s.to_str()),
                Some("mpv.exe"),
                "文件名应为 mpv.exe"
            );
            assert_eq!(
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str()),
                Some("mpv"),
                "父目录名应为 mpv"
            );
        }
        // 仓库根无 mpv 时跳过断言（温和，不失败）
    }
}
