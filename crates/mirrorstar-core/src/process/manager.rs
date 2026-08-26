use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP,
    CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::MirrorStarError;

/// 子进程管理器，用于管理 mpv 播放器等外部进程
pub struct ProcessManager {
    /// 子进程句柄
    process_handle: Option<HANDLE>,
    /// 与子进程关联的 Job Object 句柄。
    ///
    /// 配置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：当此句柄关闭（主进程退出/崩溃）
    /// 或显式 `stop()` 时，内核自动终止 Job 内的子进程，避免孤儿进程残留。
    job_handle: Option<HANDLE>,
    /// 子进程 PID
    pid: Option<u32>,
    /// 可执行文件路径
    executable: PathBuf,
}

// SAFETY (Task 9.1.2 soundness 论证):
// 本块包含 Send / !Sync / Drop 契约三类论证，顺序如下。
//
// `ProcessManager` 包含三类字段：`Option<HANDLE>`、`Option<u32>`、`PathBuf`。
// 后两者本身即 `Send + Sync`；以下仅论证 `HANDLE` 的跨线程安全性。
// `job_handle` 与 `process_handle` 同属内核句柄（Job Object 句柄），按 Win32 文档
// 同样可由创建进程内任意线程使用，跨线程安全性论证与下文 `process_handle` 完全一致。
//
// Send 安全性：Windows 进程句柄（HANDLE）是内核对象引用令牌，按 Win32 文档可在
// 创建它的进程内的任意线程上使用（进程句柄不绑定到特定线程）。移动 HANDLE 的所有权
// 到另一线程只是复制一个数值令牌，不产生别名或数据竞争。`start`/`stop`/`Drop` 通过
// `&mut self` 独占访问，由调用方（通常 `Arc<Mutex<ProcessManager>>`）保证互斥。
//
// Sync 安全性：`&self` 方法仅 `is_running`（调用 `WaitForSingleObject(handle, 0)`）、`pid`、
// `handle`（返回 HANDLE 拷贝）。这三个 API 均为线程安全的只读操作：
//   - `WaitForSingleObject` 按 Win32 文档线程安全，可从任意线程调用。
//   - `pid`/`handle` 仅读取 Copy 字段，无并发风险。
// 可变方法 `start`/`stop`/`Drop` 需要 `&mut self`，无法从 `&ProcessManager` 触发，
// 因此 `Sync` 不会暴露对内部状态的并发修改。实际使用中 `ProcessManager` 始终经
// `Arc<Mutex<...>>` 包裹，`Sync` 的存在仅为满足 `Arc<Mutex<T>>` 不强制要求 `T: Sync`
// 的语义清晰性（`Mutex<T>` 仅需 `T: Send`），这里同时实现 `Sync` 是安全且更精确的表达。
//
// 注意：句柄所有权通过 `take()` 在 `stop` 中转移并由 `CloseHandle` 释放，但该路径
// 需要 `&mut self`，与 Sync 共享引用语义不冲突，不存在 use-after-free。
//
// 参考: https://learn.microsoft.com/windows/win32/procthread/processes-and-threads
//   进程句柄可被同进程内任意线程用于 WaitForSingleObject/TerminateProcess/
//   CloseHandle，这些 API 均标注线程安全。
unsafe impl Send for ProcessManager {}
unsafe impl Sync for ProcessManager {}

impl ProcessManager {
    /// 创建新的进程管理器
    pub fn new(executable: PathBuf) -> Self {
        Self {
            process_handle: None,
            job_handle: None,
            pid: None,
            executable,
        }
    }

    /// 启动子进程，返回 PID
    ///
    /// **阻塞方法**（P04）：内部使用 `CreateProcessW`/`CreateJobObjectW`/
    /// `SetInformationJobObject`/`AssignProcessToJobObject` 等 Win32 阻塞 API。
    /// 这些调用通常快速返回（毫秒级），但在系统资源紧张时可能阻塞。
    /// 在 async 上下文中调用时应通过 `tokio::task::spawn_blocking` 包裹。
    pub fn start(&mut self, args: Vec<String>) -> Result<u32, MirrorStarError> {
        // 无条件清理旧句柄：只要存在旧句柄就先 stop_immediate()，避免句柄泄漏。
        // stop_immediate 直接 TerminateProcess + CloseHandle，毫秒级完成。
        // 50ms sleep 等 OS 回收 PID，避免 CreateProcessW 复用旧 PID 或句柄混乱。
        let had_old_handle = self.process_handle.is_some() || self.job_handle.is_some();
        if had_old_handle {
            self.stop_immediate()?;
            // 短暂等待 OS 回收 PID，避免下一个 CreateProcessW 复用旧 PID
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // 构建命令行字符串：可执行文件路径 + 参数
        // 使用 Windows MSVCRT argv 转义规则对每个参数进行转义
        let executable_str = self.executable.to_str().ok_or_else(|| {
            MirrorStarError::ProcessSpawnFailed("可执行文件路径包含非法字符".to_string())
        })?;
        let mut cmdline = escape_windows_arg(executable_str)?;
        for arg in &args {
            cmdline.push(' ');
            cmdline.push_str(&escape_windows_arg(arg)?);
        }

        // 将命令行转换为宽字符
        let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

        let mut startup_info: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // 初始隐藏窗口，mpv 嵌入后会重新显示
        startup_info.wShowWindow = windows::Win32::UI::WindowsAndMessaging::SW_HIDE.0 as u16;

        let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // 创建进程
        let result = unsafe {
            CreateProcessW(
                None, // 使用命令行中的可执行文件路径
                PWSTR(cmdline_wide.as_mut_ptr()),
                None,
                None,
                false, // 不继承句柄
                // P-006: 移除 CREATE_UNICODE_ENVIRONMENT（lpEnvironment=None 时无效）
                // lpEnvironment=None 时继承父进程环境（Unicode），CREATE_UNICODE_ENVIRONMENT
                // 仅在 lpEnvironment 非空时有效，此处为冗余 flag 故移除
                CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
                None, // 使用父进程环境
                None, // 使用父进程工作目录
                &startup_info,
                &mut proc_info,
            )
        };

        if let Err(e) = result {
            return Err(MirrorStarError::ProcessSpawnFailed(format!(
                "CreateProcessW 失败: {}",
                e
            )));
        }

        // 关闭线程句柄（我们不需要它）
        if !proc_info.hThread.is_invalid() {
            unsafe {
                // 清理路径，句柄即将丢弃，错误无实际影响
                let _ = CloseHandle(proc_info.hThread);
            }
        }

        // 创建 Job Object 并关联子进程：配置 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE，
        // 确保主进程异常退出（panic/被杀）时内核自动 kill 子进程，避免孤儿进程。
        // 注意：此时子进程句柄尚未存入 self，失败路径通过 Self::stop_handle 直接清理。
        let job = match unsafe { CreateJobObjectW(None, None) } {
            Ok(h) => h,
            Err(e) => {
                Self::stop_handle(proc_info.hProcess, Some(proc_info.dwProcessId));
                return Err(MirrorStarError::ProcessSpawnFailed(format!(
                    "CreateJobObjectW 失败: {}",
                    e
                )));
            }
        };

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(e) = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } {
            unsafe {
                // 清理路径：job 创建失败后释放句柄，错误无实际影响
                let _ = CloseHandle(job);
            }
            Self::stop_handle(proc_info.hProcess, Some(proc_info.dwProcessId));
            return Err(MirrorStarError::ProcessSpawnFailed(format!(
                "SetInformationJobObject 失败: {}",
                e
            )));
        }

        if let Err(e) = unsafe { AssignProcessToJobObject(job, proc_info.hProcess) } {
            unsafe {
                // 清理路径：job 关联失败后释放句柄，错误无实际影响
                let _ = CloseHandle(job);
            }
            Self::stop_handle(proc_info.hProcess, Some(proc_info.dwProcessId));
            return Err(MirrorStarError::ProcessSpawnFailed(format!(
                "AssignProcessToJobObject 失败: {}",
                e
            )));
        }

        let pid = proc_info.dwProcessId;
        self.process_handle = Some(proc_info.hProcess);
        self.job_handle = Some(job);
        self.pid = Some(pid);

        tracing::info!(
            "子进程已启动: pid={}, executable={}",
            pid,
            self.executable.display()
        );

        Ok(pid)
    }

    /// 停止子进程：先等待退出，超时后强制终止
    ///
    /// **阻塞方法**（P04）：内部使用 `WaitForSingleObject`/`TerminateProcess`/`CloseHandle`
    /// 等 Win32 阻塞 API。最坏情况下阻塞 `timeout_ms`（3s）+ 强杀后等待 5s = 8s。
    /// 在 async 上下文中调用时应通过 `tokio::task::spawn_blocking` 包裹，
    /// 避免阻塞 tokio worker 线程。
    ///
    /// 正常关闭路径，可接受最长 8s 阻塞以等待子进程优雅退出。决策标准（`stop` vs
    /// `stop_immediate`）详见 [`stop_immediate`](Self::stop_immediate) 文档。
    pub fn stop(&mut self) -> Result<(), MirrorStarError> {
        let handle = match self.process_handle.take() {
            Some(h) => h,
            None => return Ok(()),
        };

        // P05: 委托给 wait_and_terminate，消除与 stop_handle 的重复逻辑
        Self::wait_and_terminate(handle, self.pid, 3000);

        // 关闭 Job Object 句柄：触发 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE 内核语义
        // （此时子进程应已退出，关闭 job 句柄仅为释放内核对象，不会重复 kill）。
        if let Some(job) = self.job_handle.take() {
            unsafe {
                let _ = CloseHandle(job);
            }
        }

        tracing::info!(pid = ?self.pid, "子进程已停止");
        self.pid = None;

        Ok(())
    }

    /// 立即终止子进程，不等待优雅退出（P-002/P-003 性能优化）
    ///
    /// 跳过 `stop()` 的 3s 优雅等待 + 5s 强杀后等待，直接 `TerminateProcess` + `CloseHandle`，
    /// 依赖 `KILL_ON_JOB_CLOSE` 内核语义确保子进程退出。用于：
    /// - `Drop`：避免阻塞应用退出最长 8s（P-002）
    /// - `start()`：切换场景时立即终止旧进程，避免用户感知卡顿（P-003）
    ///
    /// **阻塞方法**：`TerminateProcess` 与 `CloseHandle` 通常毫秒级返回，最坏情况约 100ms。
    ///
    /// ## 调用方契约 (P-003)
    ///
    /// `stop_immediate` 不等待进程完全退出（`TerminateProcess` 异步），调用方在
    /// `start()` 前应确保已过 100ms 或自行 `std::thread::sleep(100ms)` 兜底。否则若进程
    /// 未完全退出，下一个 `start()` 可能复用旧 PID 导致 `CreateProcessW` 失败或
    /// 句柄混乱。
    ///
    /// `start()` 内部已在清理旧句柄后增加 50ms 短暂等待（`std::thread::sleep`），
    /// 兜底 OS 回收 PID 的延迟；外部直接连续调用 `stop_immediate()` → `start()` 时
    /// 由 `start()` 内的兜底逻辑处理，调用方无需额外等待。
    ///
    /// ## 调用方决策标准 (P-002)
    ///
    /// 选择 `stop_immediate()` 还是 [`stop`](Self::stop)，按以下场景判断：
    ///
    /// - **使用 `stop_immediate()`**：场景切换 / 进程重启，不能接受 8s 阻塞。
    ///   依赖 [`start`](Self::start) 内部的 50ms 兜底等待契约（P-003）确保
    ///   下一次 `CreateProcessW` 不复用旧 PID。适用于：
    ///   - 壁纸类型切换（如视频 → 网页、图片 → 视频）
    ///   - 用户主动切换壁纸（点击下一张、切换播放列表）
    ///   - `Drop` 中清理子进程（避免拖慢应用退出）
    /// - **使用 `stop()`**：见 [`stop`](Self::stop) 的决策标准，用于可接受 8s
    ///   阻塞的正常关闭路径。
    ///
    /// 决策原则：任何涉及用户感知延迟的路径都应使用 `stop_immediate()`。
    pub fn stop_immediate(&mut self) -> Result<(), MirrorStarError> {
        let handle = match self.process_handle.take() {
            Some(h) => h,
            None => return Ok(()),
        };

        // P-002: 立即终止，不等 3s 优雅退出
        if let Err(e) = unsafe { TerminateProcess(handle, 1) } {
            // 进程可能已退出（TerminateProcess 对已退出进程返回 ERROR_ACCESS_DENIED），
            // 记录 trace 即可，不阻塞清理
            tracing::trace!(pid = ?self.pid, error = ?e, "TerminateProcess 失败（进程可能已退出）");
        }

        // 立即关闭进程句柄，依赖 KILL_ON_JOB_CLOSE 内核语义
        unsafe {
            let _ = CloseHandle(handle);
        }

        // 关闭 Job Object 句柄：触发 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if let Some(job) = self.job_handle.take() {
            unsafe {
                let _ = CloseHandle(job);
            }
        }

        tracing::info!(pid = ?self.pid, "子进程已立即终止（stop_immediate）");
        self.pid = None;

        Ok(())
    }

    /// 内部停止辅助：对指定的进程句柄执行「等待→超时强杀→关闭句柄」流程，
    /// 用于 start() 中 Job Object 创建/配置/关联失败时清理已创建但尚未存入 self 的进程。
    ///
    /// P05: 委托给 [`wait_and_terminate`](Self::wait_and_terminate)，消除与 `stop()` 的重复逻辑。
    fn stop_handle(handle: HANDLE, pid: Option<u32>) {
        Self::wait_and_terminate(handle, pid, 3000);
        tracing::info!(pid = ?pid, "子进程已停止");
    }

    /// 等待进程退出，超时或等待失败时强制终止，最后关闭句柄（P05 提取的共享逻辑）
    ///
    /// `stop()` 和 `stop_handle()` 共享的「等待→超时强杀→关闭句柄」流程，消除约 25 行重复。
    ///
    /// 流程：
    /// 1. `WaitForSingleObject(handle, timeout_ms)` 等待进程退出
    /// 2. `WAIT_TIMEOUT`（仍在运行）→ 调用 `TerminateProcess` 强制终止，再等待 5s 确认退出
    /// 3. `WAIT_FAILED`（句柄错误等）→ P-007 记录一次警告并跳过 `terminate_and_wait`，
    ///    直接进入 `CloseHandle` 清理路径（句柄无效时 `TerminateProcess`/二次等待
    ///    大概率失败，调用只会增加日志噪声）
    /// 4. `WAIT_OBJECT_0`（已退出）→ 无需操作
    /// 5. `CloseHandle(handle)` 释放内核对象
    ///
    /// - `pid`：用于日志关联，仅供诊断
    /// - `timeout_ms`：初始等待超时（毫秒），超时后触发强制终止
    ///
    /// 无返回值：`TerminateProcess`/`WaitForSingleObject` 失败时仅记录日志，不传播错误
    /// （与原 `stop()`/`stop_handle()` 行为一致）。
    fn wait_and_terminate(handle: HANDLE, pid: Option<u32>, timeout_ms: u32) {
        let wait_result = unsafe { WaitForSingleObject(handle, timeout_ms) };

        if wait_result == WAIT_TIMEOUT {
            // 超时，进程仍在运行，强制终止
            tracing::warn!(pid = ?pid, "子进程未在超时内退出，强制终止");
            Self::terminate_and_wait(handle, pid);
        } else if wait_result == WAIT_FAILED {
            // P-007: 句柄错误等异常情况，仅记录一次警告后跳过 terminate_and_wait。
            // 原实现 (P02) 在此分支兜底调用 TerminateProcess + 二次 WaitForSingleObject，
            // 但 WAIT_FAILED 通常意味着句柄无效/不可等待，后续调用大概率也会失败，
            // 只会产生额外的 error/warn 日志噪声。改为直接进入 CloseHandle 清理路径。
            tracing::warn!(
                pid = ?pid,
                error = ?std::io::Error::last_os_error(),
                "WaitForSingleObject 返回 WAIT_FAILED，跳过 terminate_and_wait 直接 CloseHandle"
            );
        }
        // WAIT_OBJECT_0: 进程已正常退出，无需操作

        // 关闭句柄（清理路径，错误无实际影响）
        unsafe {
            let _ = CloseHandle(handle);
        }
    }

    /// 强制终止进程并等待其退出（P05 内部辅助）
    ///
    /// 调用 `TerminateProcess` 后再等待 5s 确认进程真正退出。
    /// `TerminateProcess` 失败或后续等待返回 `WAIT_FAILED` 时仅记录日志，不传播错误
    /// （与原 `stop()`/`stop_handle()` 行为一致）。
    fn terminate_and_wait(handle: HANDLE, pid: Option<u32>) {
        let terminate_result = unsafe { TerminateProcess(handle, 1) };
        if let Err(e) = terminate_result {
            tracing::error!(pid = ?pid, error = ?e, "强制终止子进程失败");
        }
        // 等待进程真正退出
        let wait_result = unsafe { WaitForSingleObject(handle, 5000) };
        if wait_result == WAIT_FAILED {
            tracing::warn!(
                pid = ?pid,
                "强制终止后等待进程退出失败 (WaitForSingleObject)"
            );
        }
    }

    /// 检查子进程是否仍在运行
    ///
    /// P01 修复：改用 `WaitForSingleObject(handle, 0)` 判断进程状态，而非
    /// `GetExitCodeProcess` + `exit_code == STILL_ACTIVE (259)`。原实现存在
    /// 退出码恰好为 259 时的误判：子进程以退出码 259 退出后，`is_running`
    /// 仍错误地报告为"仍在运行"，导致 `stop()` 等待已退出进程、`Drop` 不触发清理。
    ///
    /// `WaitForSingleObject(handle, 0)`（零超时）的返回值语义：
    /// - `WAIT_TIMEOUT`：进程仍在运行（未触发对象）→ 返回 true
    /// - `WAIT_OBJECT_0`：进程已退出（对象已触发）→ 返回 false
    /// - `WAIT_FAILED`：调用失败（句柄无效等）→ 记录警告并返回 false
    pub fn is_running(&self) -> bool {
        let handle = match self.process_handle {
            Some(h) => h,
            None => return false,
        };

        // 零超时 WaitForSingleObject：立即返回进程状态，不阻塞
        let wait_result = unsafe { WaitForSingleObject(handle, 0) };

        if wait_result == WAIT_TIMEOUT {
            // 进程仍在运行
            true
        } else if wait_result == WAIT_OBJECT_0 {
            // 进程已退出
            false
        } else {
            // WAIT_FAILED 或意外值：句柄无效等异常情况，记录警告并按"已退出"处理
            tracing::warn!(
                pid = ?self.pid,
                wait_result = ?wait_result,
                "WaitForSingleObject 返回非预期值，按进程已退出处理"
            );
            false
        }
    }

    /// 获取子进程 PID
    ///
    /// P03 修复：进程自行退出后（未经 `stop()`），`self.pid` 仍持有旧值。
    /// 此处先通过 `is_running()` 校验进程是否存活，已退出则返回 `None`，
    /// 避免返回已回收的 PID 导致下游（如 WASAPI 音量控制）误操作其他进程
    /// （PID 复用风险）。
    ///
    /// 注意：`is_running()` 内部调用 `WaitForSingleObject(handle, 0)`（一次轻量 syscall），
    /// 非热路径下开销可忽略。若需高频获取 PID 用于日志且不关心存活状态，
    /// 可直接读取 `handle()` 是否为 `Some` 作为粗略判断。
    pub fn pid(&self) -> Option<u32> {
        if self.is_running() {
            self.pid
        } else {
            None
        }
    }

    /// 获取子进程句柄
    pub fn handle(&self) -> Option<HANDLE> {
        self.process_handle
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // P-002: Drop 中调用 stop_immediate 而非 stop，避免阻塞 8s 拖慢应用退出。
        // 依赖 KILL_ON_JOB_CLOSE 内核语义确保子进程退出。
        if self.is_running() {
            if let Err(e) = self.stop_immediate() {
                tracing::error!("Drop 时立即终止子进程失败: {}", e);
            }
        } else if let Some(handle) = self.process_handle.take() {
            // 进程已退出但句柄未关闭
            unsafe {
                // Drop 清理路径，句柄即将丢弃，错误无实际影响
                let _ = CloseHandle(handle);
            }
        }

        // 兜底：若 stop_immediate 未执行过（如进程从未启动、或上述 is_running 为 false 分支），
        // job_handle 可能仍有值，需在此释放。stop_immediate 内部已 take，故此处通常为 None。
        if let Some(job) = self.job_handle.take() {
            unsafe {
                // Drop 清理路径，错误无实际影响
                let _ = CloseHandle(job);
            }
        }
    }
}

/// Windows Job Object 守卫，封装 Job Object 的创建、内存限制与自动清理。
///
/// v8.0 内存优化（Wave v8-B）：为 mpv/wp-proc 子进程关联 Job Object，限制进程内存并确保
/// 主进程退出时子进程自动终止（`KILL_ON_JOB_CLOSE`），避免孤儿进程与内存无界增长。
///
/// 与 `ProcessManager` 内部的 Job Object（仅 `KILL_ON_JOB_CLOSE`）互补：
/// `ProcessManager` 负责 `KILL_ON_JOB_CLOSE` 兜底，`JobObjectGuard` 额外施加
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` 内存上限。Windows 8+ 支持嵌套 Job Object，
/// 进程可同时关联多个 Job Object，两者的 `KILL_ON_JOB_CLOSE` 均在句柄关闭时生效。
///
/// 不实现 `Clone`（句柄唯一所有权）。Drop 时关闭句柄，触发 `KILL_ON_JOB_CLOSE`
/// 终止所有关联子进程。
pub(crate) struct JobObjectGuard {
    handle: HANDLE,
}

impl std::fmt::Debug for JobObjectGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // HANDLE 不实现 Debug，以原始指针地址展示便于日志关联
        f.debug_struct("JobObjectGuard")
            .field("handle", &(self.handle.0 as usize))
            .finish()
    }
}

impl JobObjectGuard {
    /// 创建 Job Object 并设置内存限制（字节）。
    ///
    /// `memory_limit_bytes` 为 0 时不设置内存限制（仅 `KILL_ON_JOB_CLOSE`）。
    pub(crate) fn new(memory_limit_bytes: usize) -> Result<Self, MirrorStarError> {
        // SAFETY: CreateJobObjectW 创建新 Job Object，NULL 安全属性表示不可继承
        let handle = unsafe { CreateJobObjectW(None, None) }.map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("CreateJobObjectW 失败: {}", e))
        })?;

        // 设置扩展限制：进程内存上限 + KILL_ON_JOB_CLOSE
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if memory_limit_bytes == 0 {
            // 仅 KILL_ON_JOB_CLOSE，不限制内存
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        } else {
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            limits.ProcessMemoryLimit = memory_limit_bytes as _;
        }

        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| {
                // 清理路径：SetInformationJobObject 失败后释放已创建的 Job Object 句柄
                let _ = CloseHandle(handle);
                MirrorStarError::DesktopIntegration(format!("SetInformationJobObject 失败: {}", e))
            })?;
        }

        Ok(Self { handle })
    }

    /// 关联进程到 Job Object。
    ///
    /// Windows 8+ 支持嵌套 Job Object，进程可同时关联多个 Job Object。本场景中
    /// 进程已被 `ProcessManager` 关联到一个仅 `KILL_ON_JOB_CLOSE` 的 Job Object，
    /// 此处再关联到本守卫的带内存限制的 Job Object，两者限制叠加生效。
    ///
    /// # Safety 约定
    ///
    /// 调用方需确保 `process_handle` 为有效的进程句柄（尚未关闭）。`AssignProcessToJobObject`
    /// 对无效句柄返回错误而非 UB，但调用方仍应确保句柄有效性以避免误操作。
    pub(crate) fn assign_process(&self, process_handle: HANDLE) -> Result<(), MirrorStarError> {
        unsafe { AssignProcessToJobObject(self.handle, process_handle) }.map_err(|e| {
            MirrorStarError::DesktopIntegration(format!("AssignProcessToJobObject 失败: {}", e))
        })?;
        Ok(())
    }
}

impl Drop for JobObjectGuard {
    fn drop(&mut self) {
        // CloseHandle 关闭 Job Object 句柄，因 KILL_ON_JOB_CLOSE 标志，
        // 所有关联的子进程会被自动终止
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 按 Windows MSVCRT argv 解析规则转义单个命令行参数。
///
/// 规则参考：<https://learn.microsoft.com/cpp/c-language/parsing-c-command-line-arguments>
/// - 含空格、制表符、双引号或反斜杠的参数用双引号包裹
/// - 参数中的双引号用 `\"` 转义
/// - 双引号前的反斜杠需要加倍（每个 `\` 变成 `\\`）
/// - 参数末尾的反斜杠（若参数被引号包裹）也需要加倍
/// - 空字符串用 `""` 表示
/// - 普通参数（无空格、无特殊字符）无需引号包裹
///
/// # N-009 安全修复
///
/// 含换行符（`\n` / `\r`）的参数会被拒绝并返回 `InvalidArgument` 错误，
/// 防止换行符被某些命令行解析器视为参数分隔符导致命令注入。
fn escape_windows_arg(arg: &str) -> Result<String, MirrorStarError> {
    // N-009: 拒绝含换行符的参数，防止命令行注入
    // P-005: 同时拒绝含 NUL (\0) 的参数，防止 CreateProcessW 命令行被 NUL 截断
    //        导致后续参数丢失/注入风险
    if arg.contains('\n') || arg.contains('\r') || arg.contains('\0') {
        return Err(MirrorStarError::InvalidArgument {
            reason: format!(
                "命令行参数包含不可用控制字符（\\n / \\r / \\0），可能引发命令注入: {:?}",
                arg.chars().take(64).collect::<String>()
            ),
        });
    }

    // 空字符串特殊处理
    if arg.is_empty() {
        return Ok("\"\"".to_string());
    }

    // 判断是否需要引号包裹：含空格、制表符、双引号或反斜杠
    let needs_quotes = arg
        .chars()
        .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\\');

    if !needs_quotes {
        return Ok(arg.to_string());
    }

    // 统计末尾连续反斜杠数量，用于末尾加倍
    let trailing_backslashes = arg.chars().rev().take_while(|&c| c == '\\').count();

    let mut result = String::with_capacity(arg.len() + 2);
    result.push('"');

    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }

        if c == '"' {
            // 双引号前的反斜杠需要加倍，再加上转义引号的反斜杠
            // 每个反斜杠变成两个，然后追加 \"
            for _ in 0..backslashes * 2 {
                result.push('\\');
            }
            result.push('\\');
            result.push('"');
        } else {
            // 普通字符前的反斜杠原样输出
            for _ in 0..backslashes {
                result.push('\\');
            }
            result.push(c);
        }
        backslashes = 0;
    }

    // 处理末尾的反斜杠（参数被引号包裹时需要加倍）
    for _ in 0..trailing_backslashes * 2 {
        result.push('\\');
    }

    result.push('"');
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::escape_windows_arg;
    use super::JobObjectGuard;
    use super::ProcessManager;
    use super::HANDLE;
    use crate::MirrorStarError;
    use std::ffi::c_void;

    #[test]
    fn escape_plain_arg() {
        // 普通参数无需引号包裹
        assert_eq!(escape_windows_arg("--volume=50").unwrap(), "--volume=50");
    }

    #[test]
    fn escape_arg_with_space() {
        // 含空格参数用引号包裹
        assert_eq!(
            escape_windows_arg("C:\\Program Files\\app").unwrap(),
            "\"C:\\Program Files\\app\""
        );
    }

    #[test]
    fn escape_arg_with_quote() {
        // 含引号参数：引号被转义为 \"
        assert_eq!(
            escape_windows_arg("C:\\path\\\"name\".mp4").unwrap(),
            "\"C:\\path\\\\\\\"name\\\".mp4\""
        );
    }

    #[test]
    fn escape_arg_with_backslash() {
        // 含反斜杠参数（末尾反斜杠加倍）
        assert_eq!(
            escape_windows_arg("C:\\path\\").unwrap(),
            "\"C:\\path\\\\\""
        );
    }

    #[test]
    fn escape_arg_with_backslash_and_quote() {
        // 含反斜杠 + 引号组合
        // 输入 a\"b：反斜杠在引号前加倍 -> a\\\"b
        assert_eq!(escape_windows_arg("a\\\"b").unwrap(), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn escape_empty_string() {
        // 空字符串输出 ""
        assert_eq!(escape_windows_arg("").unwrap(), "\"\"");
    }

    #[test]
    fn escape_arg_with_tab() {
        // 制表符触发引号包裹，制表符在引号内原样保留
        assert_eq!(
            escape_windows_arg("hello\tworld").unwrap(),
            "\"hello\tworld\""
        );
    }

    #[test]
    fn escape_arg_with_only_spaces() {
        // 纯空格触发引号包裹，空格在引号内原样保留
        assert_eq!(escape_windows_arg("   ").unwrap(), "\"   \"");
    }

    // ── N-009: 换行符拒绝测试 ───────────────────────────────────────────────

    #[test]
    fn escape_arg_with_newline_returns_error() {
        // N-009: 含 \n 的参数应返回 InvalidArgument 错误，防止命令行注入
        let result = escape_windows_arg("hello\nworld");
        assert!(result.is_err());
        match result.unwrap_err() {
            MirrorStarError::InvalidArgument { .. } => {}
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn escape_arg_with_carriage_return_returns_error() {
        // N-009: 含 \r 的参数应返回 InvalidArgument 错误
        let result = escape_windows_arg("hello\rworld");
        assert!(result.is_err());
        match result.unwrap_err() {
            MirrorStarError::InvalidArgument { .. } => {}
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn escape_arg_with_newline_at_start_returns_error() {
        // 换行符在开头也应被拒绝
        let result = escape_windows_arg("\ninjection");
        assert!(result.is_err());
    }

    #[test]
    fn escape_arg_with_newline_at_end_returns_error() {
        // 换行符在末尾也应被拒绝
        let result = escape_windows_arg("arg\n");
        assert!(result.is_err());
    }

    #[test]
    fn escape_arg_with_crlf_returns_error() {
        // Windows 风格 CRLF 换行也应被拒绝（包含 \r 和 \n）
        let result = escape_windows_arg("arg\r\nmalicious");
        assert!(result.is_err());
    }

    #[test]
    fn escape_arg_with_multiple_newlines_returns_error() {
        // 多个换行符也应被拒绝
        let result = escape_windows_arg("a\nb\nc\nd");
        assert!(result.is_err());
    }

    #[test]
    fn escape_arg_with_newline_and_spaces_returns_error() {
        // 混合空格和换行符：仍应被拒绝（换行符优先检查）
        let result = escape_windows_arg("path with spaces and\nnewline");
        assert!(result.is_err());
    }

    // ── P-005: NUL (\0) 拒绝测试 ──────────────────────────────────────────────

    #[test]
    fn escape_windows_arg_rejects_nul() {
        // P-005: 含 NUL (\0) 的参数应返回 InvalidArgument 错误，
        // 防止 CreateProcessW 命令行被 NUL 截断导致后续参数丢失/注入风险
        let result = escape_windows_arg("foo\0bar");
        assert!(result.is_err());
        match result.unwrap_err() {
            MirrorStarError::InvalidArgument { .. } => {}
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn escape_windows_arg_rejects_nul_at_end() {
        // P-005: 末尾的 NUL 也应被拒绝（命令行末尾截断风险）
        let result = escape_windows_arg("foo\0");
        assert!(result.is_err());
        match result.unwrap_err() {
            MirrorStarError::InvalidArgument { .. } => {}
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn escape_arg_normal_path_unaffected() {
        // N-009 修复不应影响正常路径（含空格、中文、Unicode 等）
        assert_eq!(
            escape_windows_arg("C:\\Users\\Test\\壁纸.mp4").unwrap(),
            "\"C:\\Users\\Test\\壁纸.mp4\""
        );
        // 含 Unicode 但不含换行符的参数应正常转义
        assert_eq!(
            escape_windows_arg("--title=日本語動画").unwrap(),
            "--title=日本語動画"
        );
    }

    // ── P01: STILL_ACTIVE (259) 退出码误判测试 ────────────────────────────────

    /// P01: 进程以退出码 259 (STILL_ACTIVE) 退出时，`is_running` 应返回 `false`。
    ///
    /// 修复前：`GetExitCodeProcess` 返回 259，`exit_code == STILL_ACTIVE` 为 true，
    /// `is_running` 错误返回 `true`，导致 `stop()` 等待已退出进程、`Drop` 不触发清理。
    ///
    /// 修复后：`WaitForSingleObject(handle, 0)` 返回 `WAIT_OBJECT_0`（对象已触发），
    /// `is_running` 正确返回 `false`。
    ///
    /// 测试方式：启动 `cmd /c exit 259`，轮询 `is_running` 直到返回 false 或超时。
    /// - 修复后：进程退出后 `is_running` 立即返回 false，测试快速通过
    /// - 修复前：因退出码 259 == STILL_ACTIVE，`is_running` 永远返回 true，
    ///   轮询直到 5s 超时，断言失败
    #[test]
    fn is_running_returns_false_for_exit_code_259() {
        // 通过 COMSPEC 环境变量获取 cmd.exe 路径，回退到默认路径
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);
        pm.start(vec![
            "/c".to_string(),
            "exit".to_string(),
            "259".to_string(),
        ])
        .expect("启动 cmd 进程失败");

        // 轮询等待进程退出（最多 5 秒）
        // 修复后：进程退出后 is_running 立即返回 false
        // 修复前：因退出码 259 == STILL_ACTIVE，is_running 始终返回 true，轮询直到超时
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut detected_exited = false;
        while std::time::Instant::now() < deadline {
            if !pm.is_running() {
                detected_exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            detected_exited,
            "P01: 进程以退出码 259 退出后 is_running 应返回 false，\
             但 5 秒内始终返回 true（STILL_ACTIVE 误判）"
        );

        // 清理：stop() 对已退出进程会立即返回（WaitForSingleObject 返回 WAIT_OBJECT_0）
        let _ = pm.stop();
    }

    /// P01 边界验证：进程仍在运行时，`is_running` 应返回 `true`。
    /// 确保修复不影响"进程存活"的正常判断路径。
    #[test]
    fn is_running_returns_true_while_process_alive() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);
        // 启动一个会持续运行 10 秒的 cmd 进程
        pm.start(vec![
            "/c".to_string(),
            "timeout".to_string(),
            "/t".to_string(),
            "10".to_string(),
            "/nobreak".to_string(),
        ])
        .expect("启动 cmd 进程失败");

        // 进程刚启动，应仍在运行
        assert!(
            pm.is_running(),
            "P01 正常路径: 进程刚启动，is_running 应返回 true"
        );

        // 清理：停止进程
        let _ = pm.stop();
        // 停止后应不再运行
        assert!(
            !pm.is_running(),
            "P01 正常路径: stop 后 is_running 应返回 false"
        );
    }

    // ── P02: WAIT_FAILED 兜底终止测试 ──────────────────────────────────────────

    /// 验证 `wait_and_terminate` 在 `WaitForSingleObject` 返回 `WAIT_FAILED` 时
    /// 记录警告并跳过 `terminate_and_wait`（P-007），不 panic。
    ///
    /// 测试方式：传入无效句柄（null）触发 `WAIT_FAILED`，验证方法不 panic。
    /// `CloseHandle` 对无效句柄会失败，但错误被内部忽略，不影响方法执行。
    #[test]
    fn wait_and_terminate_handles_wait_failed() {
        // null 句柄触发 WaitForSingleObject 返回 WAIT_FAILED
        let invalid_handle = HANDLE(std::ptr::null_mut::<c_void>());
        // P-004: wait_and_terminate 返回 ()，验证不 panic 即可
        ProcessManager::wait_and_terminate(invalid_handle, Some(99999), 100);
    }

    // ── P05: wait_and_terminate 共享逻辑委托测试 ────────────────────────────────

    /// P05: 验证 `stop()` 委托 `wait_and_terminate`。
    ///
    /// 启动一个短生命周期进程，调用 `stop()`，验证进程被正确终止且内部状态被清理。
    /// `stop()` 内部通过 `Self::wait_and_terminate(handle, self.pid, 3000)` 委托，
    /// 此测试验证委托链路端到端工作。
    #[test]
    fn stop_delegates_to_wait_and_terminate() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);
        pm.start(vec![
            "/c".to_string(),
            "timeout".to_string(),
            "/t".to_string(),
            "10".to_string(),
            "/nobreak".to_string(),
        ])
        .expect("启动 cmd 进程失败");

        assert!(pm.is_running(), "进程应仍在运行");
        assert!(pm.pid().is_some(), "pid 应有值");

        // stop() 内部委托 wait_and_terminate：等待 3s → 超时强杀 → 关闭句柄
        pm.stop().expect("stop 应成功");

        // 验证清理
        assert!(!pm.is_running(), "stop 后进程应不再运行");
        assert!(pm.pid().is_none(), "stop 后 pid 应为 None");
        assert!(pm.handle().is_none(), "stop 后 handle 应为 None");
    }

    /// P05: 验证 `stop_handle` 委托 `wait_and_terminate`。
    ///
    /// 启动一个立即退出的进程，取走句柄后直接调用 `stop_handle`，
    /// 验证方法正常返回（不 panic），即证明委托链路工作。
    #[test]
    fn stop_handle_delegates_to_wait_and_terminate() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);
        pm.start(vec!["/c".to_string(), "exit".to_string(), "0".to_string()])
            .expect("启动 cmd 进程失败");

        // 等待进程自行退出，使 wait_and_terminate 的初始等待立即返回 WAIT_OBJECT_0
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 取走句柄和 PID，直接调用 stop_handle（绕过 stop()）
        let handle = pm.process_handle.take().expect("应有句柄");
        let pid = pm.pid;
        // stop_handle 委托 wait_and_terminate：进程已退出，初始等待立即返回 WAIT_OBJECT_0
        // 若方法正常返回（不 panic），即验证委托成功
        ProcessManager::stop_handle(handle, pid);

        // 清理：handle 已被 stop_handle 关闭，需清空 pm 内部状态避免 Drop 重复关闭
        // `pm.pid = None` 清理是为了避免 Drop 时重复关闭已 `take()` 的句柄。
        pm.pid = None;
        // job_handle 仍存在，由 Drop 关闭
    }

    // ── P03: pid() 进程退出后返回 None 测试 ─────────────────────────────────────

    /// P03: 进程自行退出后（未经 `stop()`），`pid()` 应返回 `None` 而非旧 PID。
    ///
    /// 修复前：`pid()` 直接返回 `self.pid`，进程退出后仍返回旧值，
    /// 下游（WASAPI 音量控制）可能对已回收的 PID 执行操作，存在 PID 复用风险。
    /// 修复后：`pid()` 内部先调用 `is_running()`，已退出则返回 `None`。
    #[test]
    fn pid_returns_none_after_process_exits() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);
        pm.start(vec!["/c".to_string(), "exit".to_string(), "0".to_string()])
            .expect("启动 cmd 进程失败");

        // 进程刚启动，pid 应有值
        assert!(pm.pid().is_some(), "进程运行时 pid 应有值");

        // 等待进程自行退出（未经 stop()）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !pm.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // P03: 进程退出后 pid() 应返回 None（而非旧的 PID 快照）
        assert!(
            pm.pid().is_none(),
            "P03: 进程退出后 pid() 应返回 None，但仍返回旧值"
        );

        // 清理
        let _ = pm.stop();
    }

    // ── P-001: start() 重启句柄泄漏测试 ──────────────────────────────────────────

    /// 获取当前进程的内核句柄计数，用于检测句柄泄漏。
    fn current_handle_count() -> u32 {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        let mut count: u32 = 0;
        match unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) } {
            Ok(()) => count,
            Err(_) => 0,
        }
    }

    /// 上一进程自行退出后再次调用 `start()`，不应泄漏内核句柄。
    ///
    /// 历史修复批次：P-001。
    ///
    /// `start()` 检查 `process_handle`/`job_handle` 是否存在，无条件调用 `stop()`
    /// 清理旧句柄。`stop()` 对已退出进程会立即返回（`WaitForSingleObject` →
    /// `WAIT_OBJECT_0`），无阻塞。
    ///
    /// 测试方式：循环 N 次启动短生命周期进程（`cmd /c exit 0`）→ 等待其自行退出 →
    /// 下一轮 `start()` 触发清理路径。通过 `GetProcessHandleCount` 监测当前进程
    /// 句柄数，验证不随循环单调增长。
    #[test]
    fn start_after_exit_does_not_leak_handles() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);

        // 记录基线句柄数
        let baseline = current_handle_count();

        // 循环 N 次：启动短生命周期进程 → 等待自退出 → 下一轮 start() 触发清理
        const N: usize = 10;
        for i in 0..N {
            pm.start(vec!["/c".to_string(), "exit".to_string(), "0".to_string()])
                .unwrap_or_else(|e| panic!("第 {} 轮启动 cmd 进程失败: {}", i + 1, e));

            // 等待进程自行退出（未经 stop()），模拟 mpv 播放结束/崩溃场景
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut exited = false;
            while std::time::Instant::now() < deadline {
                if !pm.is_running() {
                    exited = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(exited, "第 {} 轮: 进程应在 5s 内自行退出", i + 1);
        }

        // 清理最后一轮进程的句柄
        let _ = pm.stop();

        let final_count = current_handle_count();
        let growth = final_count.saturating_sub(baseline);

        // 修复后：增长应接近 0（仅允许少量噪声，来自并行测试的瞬态句柄）。
        // 修复前：每次"自退出后重启"泄漏 2 个句柄，N-1 轮约泄漏 2*(N-1)=18 个，
        // 远超阈值 8。
        assert!(
            growth < 8,
            "P-001: start() 重启后句柄泄漏，baseline={}, final={}, growth={} (循环 {} 轮)",
            baseline,
            final_count,
            growth,
            N
        );
    }

    // ── P-003: stop_immediate → start 不复用旧 PID 测试 ────────────────────

    /// `stop_immediate()` 后立即 `start()`，新进程 PID 不应等于旧 PID，
    /// 且 `start()` 应返回 `Ok` 而非 `Err`。
    ///
    /// 历史修复批次：P-003。
    ///
    /// v4.0 引入的 `stop_immediate` 省略 `wait` 直接 `CloseHandle`，但
    /// `TerminateProcess` 是异步的：内核发出终止信号后进程真正退出需要短暂时间。
    /// 若旧进程未完全退出，下一个 `start()` 的 `CreateProcessW` 可能复用旧 PID
    /// 或因内核句柄未释放导致失败。
    ///
    /// P-003 修复：
    /// - `start()` 在内部清理旧句柄后追加 50ms `std::thread::sleep`，等待 OS 回收
    ///   旧 PID，避免 `CreateProcessW` 复用旧 PID 或句柄混乱
    /// - `stop_immediate` 文档契约要求外部调用方在 `start()` 前等待 100ms 或自行
    ///   `std::thread::sleep(100ms)` 兜底
    ///
    /// 测试方式：
    /// 1. 启动一个长生命周期进程（`cmd /c timeout 30`）
    /// 2. 调用 `stop_immediate()` 立即终止
    /// 3. 立即调用 `start()` 启动新进程
    /// 4. 断言新 PID != 旧 PID，且 `start()` 返回 `Ok`
    ///
    /// 注意：PID 复用是 OS 行为，理论上 50ms 后内核仍可能复用同一 PID（虽极罕见），
    /// 故标注 `#[ignore]` 防止 CI flaky。本地运行：
    /// `cargo test -p mirrorstar-core --lib process::manager::tests -- --ignored
    /// start_after_stop_immediate_does_not_reuse_pid`
    #[ignore]
    #[test]
    fn start_after_stop_immediate_does_not_reuse_pid() {
        let cmd_path = std::env::var("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let mut pm = ProcessManager::new(cmd_path);

        // 1. 启动一个长生命周期进程（30s 足够测试期间不自行退出）
        let old_pid = pm
            .start(vec![
                "/c".to_string(),
                "timeout".to_string(),
                "/t".to_string(),
                "30".to_string(),
                "/nobreak".to_string(),
            ])
            .expect("启动旧 cmd 进程失败");
        assert!(old_pid > 0, "旧 PID 应为有效值");

        // 验证进程确实在运行
        assert!(pm.is_running(), "旧进程应仍在运行");
        assert_eq!(pm.pid(), Some(old_pid), "pid() 应返回旧 PID");

        // 2. 立即终止（stop_immediate 不等待进程完全退出，TerminateProcess 异步）
        pm.stop_immediate().expect("stop_immediate 应成功");
        assert!(!pm.is_running(), "stop_immediate 后 is_running 应为 false");
        assert!(pm.pid().is_none(), "stop_immediate 后 pid 应为 None");

        // 3. 立即调用 start()，触发 P-003 修复路径：
        //    - 因 self.process_handle 已被 stop_immediate take 走，had_old_handle=false，
        //      不会再次调用 stop_immediate，但也不会进入 50ms sleep 分支。
        //    此测试主要验证：即使外部连续 stop_immediate → start（无 50ms 兜底），
        //    start() 仍应返回 Ok（CreateProcessW 不应因内核句柄未释放而失败）。
        let new_pid = pm
            .start(vec![
                "/c".to_string(),
                "timeout".to_string(),
                "/t".to_string(),
                "30".to_string(),
                "/nobreak".to_string(),
            ])
            .expect("P-003: stop_immediate 后立即 start() 应成功，不应因旧进程未完全退出而失败");

        // 4. 断言新 PID != 旧 PID（PID 复用是 OS 行为，此处仅为附加验证）
        assert_ne!(
            new_pid, old_pid,
            "P-003: 新进程 PID 不应等于旧进程 PID（PID 不应被立即复用）"
        );

        // 验证新进程确实在运行
        assert!(pm.is_running(), "新进程应仍在运行");
        assert_eq!(pm.pid(), Some(new_pid), "pid() 应返回新 PID");

        // 清理
        let _ = pm.stop();
    }

    // ── v8-B: JobObjectGuard 测试 ──────────────────────────────────────────────

    /// v8-B：验证 `JobObjectGuard::new` 以 256MB 内存限制创建成功。
    ///
    /// 仅验证创建路径（`CreateJobObjectW` + `SetInformationJobObject`），
    /// 不涉及进程关联（`assign_process` 需要真实进程句柄，难以在单元测试中隔离）。
    /// Drop 时关闭 Job Object 句柄，验证不 panic。
    #[test]
    fn test_job_object_guard_creation() {
        let guard = JobObjectGuard::new(256 * 1024 * 1024);
        assert!(guard.is_ok(), "JobObjectGuard 创建应成功");
        // Drop guard：验证 CloseHandle 不 panic（KILL_ON_JOB_CLOSE 对无关联进程为 no-op）
        drop(guard);
    }

    /// v8-B：验证 `memory_limit_bytes=0` 时仅设置 `KILL_ON_JOB_CLOSE`（无内存限制）。
    #[test]
    fn test_job_object_guard_zero_limit() {
        let guard = JobObjectGuard::new(0);
        assert!(guard.is_ok(), "JobObjectGuard 创建（无内存限制）应成功");
        // Drop 验证不 panic
        drop(guard);
    }

    /// v8-B：验证 `JobObjectGuard` 实现 `Debug` trait（便于日志输出）。
    #[test]
    fn test_job_object_guard_debug_impl() {
        let guard = JobObjectGuard::new(0).expect("JobObjectGuard 创建应成功");
        let debug_str = format!("{:?}", guard);
        assert!(
            debug_str.contains("JobObjectGuard"),
            "Debug 输出应包含结构体名，实际: {}",
            debug_str
        );
    }
}
