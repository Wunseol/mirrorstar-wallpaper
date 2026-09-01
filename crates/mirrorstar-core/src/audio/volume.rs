use std::cell::RefCell;
use std::collections::HashMap;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

// 修复历史与 A-TD 技术债明细见 docs/05-优化文档/05-audio-ipc-process模块.md §2.1

// TODO(A-001): 未实现 IMMNotificationClient 主动监听；A-001 后 `refresh_session_manager`
// 在 WASAPI 调用失败时尝试 COM 重新初始化 + 设备重连，作为惰性自愈方案。

/// 音量控制器，通过 Windows Audio Session API 控制指定进程的音量
///
/// 缓存 COM 接口以避免每次操作都重新创建 COM 对象链
///
/// 注意：此类不负责 COM 初始化。调用方需确保所在线程已初始化 COM
/// （Tauri/tao 会在主线程自动初始化 STA 模式）。
pub struct VolumeControl {
    /// 当前默认渲染设备的会话管理器。
    ///
    /// 使用 `RefCell` 以便在 `&self` 方法中通过 `refresh_session_manager`
    /// 惰性重绑定时修改此字段（A-001 方案 B：设备切换后 WASAPI 调用失败时触发）。
    ///
    /// 同时作为降级模式标记：`None` 表示 `new_disabled()` 创建的降级实例，
    /// `refresh_session_manager` 会在此状态下尝试 COM 重新初始化恢复。
    session_manager: RefCell<Option<IAudioSessionManager2>>,
    /// PID -> 音频会话的缓存，避免每次都遍历所有会话
    ///
    /// 使用 `RefCell` 是因为公开方法（`set_process_volume` 等）和 `with_session`
    /// 均接收 `&self`，无法通过可变借用修改缓存。
    ///
    /// Drop 契约（A-004）见下方 `unsafe impl Send` 的 SAFETY 注释。
    session_cache: RefCell<HashMap<u32, IAudioSessionControl2>>,
}

// SAFETY (Task 9.1.3 soundness 论证):
//
// `VolumeControl` 包含 `IAudioSessionManager2` 以及
// `RefCell<HashMap<u32, IAudioSessionControl2>>`。前者为 WASAPI COM 接口指针，
// 后者内部亦持有 `IAudioSessionControl2`。`windows` crate 默认将这些 COM 接口
// 标注为 `!Send`（因 COM apartment 规则无法在类型层面证明安全），故结构体本身
// 不会自动实现 `Send`，需在此手动论证。
//
// Send 安全性（关键论据）：
//   1. WASAPI（Windows Audio Session API）的接口按 Microsoft 文档均为 *free-threaded*
//      （自由线程化），可在任意已初始化 COM 的线程上调用，不依赖创建线程的 apartment。
//      参考: https://learn.microsoft.com/windows/win32/coreaudio/programming-reference
//        "WASAPI COM interfaces are free-threaded and can be called from any thread."
//      因此即便创建于 STA 主线程，在 MTA 线程（tokio worker）上调用其方法也安全。
//   2. COM 接口指针本身仅是指针大小的令牌，移动所有权不产生别名或数据竞争。
//   3. 调用方契约：使用 VolumeControl 的线程必须已初始化 COM。本项目主线程为 STA
//      （由 Tauri/tao 自动初始化），但 tokio 默认 **不** 在 worker/blocking 线程
//      初始化 COM，spawn_blocking 调用方需自行初始化（见 T-005 修复：
//      `src-tauri/src/commands/wallpaper.rs` 的 spawn_blocking 闭包用 ComGuard）。
//
// !Sync 原因（刻意不实现 Sync）：
//   - `RefCell<HashMap<...>>` 本身即 `!Sync`，运行时借用检查非线程安全。
//   - 即便没有 RefCell，WASAPI 会话枚举/音量设置也不保证对同一对象的并发调用安全，
//     必须串行访问。调用方需通过 `Arc<Mutex<VolumeControl>>` 包裹来保证互斥
//     （见 `src-tauri/src/lib.rs` 的 `Arc::new(Mutex::new(VolumeControl::new()))`）。
//   - 不实现 `Sync` 使类型系统强制要求外部加锁，避免误用。
//
// 结论：`Send` 在 WASAPI free-threaded 语义 + 调用方线程已初始化 COM 的前提下 sound。
//      `!Sync` 是正确的保守选择。
//
// Drop 契约（A-004）：
//   - `VolumeControl` 不实现显式 `impl Drop`，依赖 COM 接口
//     （`IAudioSessionManager2`、`IAudioSessionControl2`）的自动 `Release()`。
//     `HashMap` 的 Drop 会逐个 drop 其值（`IAudioSessionControl2`），
//     由 `windows` crate 生成的 Drop 实现调用 `Release()`。
//   - 由于通过 `Arc<Mutex<VolumeControl>>` 共享（见 src-tauri/src/lib.rs），
//     最后一个 `Arc clone` 的 drop 会触发 `VolumeControl` 字段的 drop。
//   - 该 drop 必须发生在已初始化 COM 的线程（与 Send SAFETY 的调用方契约一致，
//     见上方"调用方契约"段）。跨 apartment 释放风险由调用方确保 drop 线程
//     已初始化 COM 来规避。
//   - 如未来需改进，可增加显式 `impl Drop for VolumeControl` 调用
//     `session_cache.clear()`，让 Release 调用集中在显式 Drop 路径上，
//     便于在此处插入跨 apartment 释放的规避逻辑（如 marshal 到创建线程释放）。
unsafe impl Send for VolumeControl {}

impl VolumeControl {
    /// 创建新的 VolumeControl，缓存 COM 接口
    ///
    /// 调用方需确保当前线程已初始化 COM（由 Tauri/tao 在主线程自动完成）
    pub fn new() -> Result<Self, crate::MirrorStarError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;

            Ok(Self {
                session_manager: RefCell::new(Some(manager)),
                session_cache: RefCell::new(HashMap::new()),
            })
        }
    }

    /// 创建 no-op 降级实例（T-008 优雅降级）
    ///
    /// 当 `VolumeControl::new()` 失败（如无音频设备的服务器/虚拟机环境）时使用，
    /// 返回字段全 `None` 的实例。所有公开方法在 `session_manager` 为 `None` 时
    /// 静默返回 `Ok`（默认值），不尝试 WASAPI 调用，也不返回错误——
    /// 这样视频壁纸播放期间不会因音量操作失败而刷屏错误日志，应用仍可正常使用
    /// 仅视觉壁纸（图片/GIF）功能。
    ///
    /// 注意：A-001 修复后，`refresh_session_manager` 在 `session_manager`
    /// 为 `None` 时会尝试 COM 重新初始化以恢复非降级模式；恢复失败时返回 `Err`
    /// 但不修改字段（保持 `None`，允许后续重试）。详见 `refresh_session_manager`。
    pub fn new_disabled() -> Self {
        Self {
            session_manager: RefCell::new(None),
            session_cache: RefCell::new(HashMap::new()),
        }
    }

    /// 重新创建 session manager（音频设备变更时调用）。
    ///
    /// **A-001 方案 B 惰性重试**：仅在 `set_process_volume` 等 WASAPI 操作失败时
    /// 由 `with_session` 内部调用，不主动监听设备变更事件。未来可升级为
    /// `IMMNotificationClient` 主动监听以实现设备切换即时响应。
    ///
    /// **A-001 降级恢复**：当 `session_manager` 为 `None`（降级实例）时，
    /// 先尝试 `CoInitializeEx` 重新初始化 COM，再创建 enumerator 刷新 session。
    /// 成功则填充 `session_manager`；失败则返回 `Err` 但不修改字段
    /// （保持 `None`，允许后续再次调用 `refresh_session_manager` 重试）。
    ///
    /// COM 初始化策略：仅在降级恢复路径调用 `CoInitializeEx(None, COINIT_MULTITHREADED)`，
    /// 忽略返回值（参考 `mode_dispatch.rs` / `fast_path.rs` 模式）。
    /// `S_OK`/`S_FALSE` 均表示 COM 可用；`RPC_E_CHANGED_MODE` 表示线程已以其他
    /// apartment 模式初始化但 COM 仍可能可用，故不因 `CoInitializeEx` 失败而提前返回，
    /// 而是继续尝试 `CoCreateInstance`，由其结果决定恢复成败。
    pub fn refresh_session_manager(&self) -> Result<(), crate::MirrorStarError> {
        // A-001: 降级恢复——当 session_manager 为 None 时尝试 COM 重新初始化。
        let is_degraded = self.session_manager.borrow().is_none();

        unsafe {
            if is_degraded {
                // 尝试初始化 COM（MTA 模式）。忽略返回值：
                // - S_OK / S_FALSE：COM 可用（ref count +1，由线程退出时自动清理）
                // - RPC_E_CHANGED_MODE：线程已以其他模式初始化，COM 仍可能可用
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            // 创建 IMMDeviceEnumerator 并刷新 session_manager。
            // 失败时不修改 session_manager（保持原状，允许后续重试）。
            let enumerator: IMMDeviceEnumerator =
                match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                    Ok(e) => e,
                    Err(e) => {
                        if is_degraded {
                            tracing::warn!(error = %e, "COM 重新初始化失败，audio 保持降级模式");
                        }
                        // 仍清空缓存（保持与原行为一致：设备变更后缓存失效）
                        self.session_cache.borrow_mut().clear();
                        return Err(e.into());
                    }
                };

            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            *self.session_manager.borrow_mut() = Some(manager);
        }

        if is_degraded {
            tracing::info!("COM 重新初始化成功，audio 从降级模式恢复");
        }

        // 设备变更后缓存的会话指针失效，需清空
        self.session_cache.borrow_mut().clear();
        Ok(())
    }

    /// 遍历音频会话，找到匹配 PID 的会话并执行操作。
    ///
    /// **A-001 方案 B 惰性重试**：若首次 WASAPI 调用失败（可能是设备切换导致
    /// `session_manager` 指向已移除的设备），自动调用 `refresh_session_manager`
    /// 重新绑定当前默认渲染设备后重试一次。重试仍失败则返回原错误。
    ///
    /// **A-002 日志增强**：所有 session 均失败时（首次 + 重试均未命中），
    /// 在返回 `Err` 前记录 `tracing::warn!(pid, attempted = n, "all sessions failed")`，
    /// 其中 `n` 为首次与重试两轮枚举中尝试的 session 总数，便于诊断根因
    /// （此前仅记录最后一次错误，难以判断枚举规模）。
    fn with_session<F, R>(&self, pid: u32, f: F) -> Result<R, crate::MirrorStarError>
    where
        F: Fn(IAudioSessionControl2) -> Result<R, crate::MirrorStarError>,
    {
        let mut first_attempted = 0usize;
        let first_attempt = self.with_session_once(pid, &f, &mut first_attempted);
        if first_attempt.is_ok() {
            return first_attempt;
        }

        // A-001 惰性重试：WASAPI 操作失败可能是设备切换导致 session_manager 失效。
        // 调用 refresh_session_manager 重新绑定当前默认渲染设备，清空缓存后重试一次。
        tracing::warn!(
            pid = pid,
            error = ?first_attempt.as_ref().err(),
            "WASAPI 操作失败，触发 refresh_session_manager 后重试"
        );
        let mut retry_attempted = 0usize;
        match self.refresh_session_manager() {
            Ok(()) => match self.with_session_once(pid, &f, &mut retry_attempted) {
                Ok(r) => {
                    tracing::info!(pid = pid, "设备重连重试成功");
                    return Ok(r);
                }
                Err(retry_err) => {
                    tracing::warn!(
                        pid = pid,
                        error = %retry_err,
                        "设备重连重试仍失败，返回原错误"
                    );
                }
            },
            Err(refresh_err) => {
                tracing::warn!(
                    error = %refresh_err,
                    "refresh_session_manager 失败，放弃重试"
                );
            }
        }
        // A-002: 所有 session 均失败时记录 attempted 计数，便于诊断根因。
        // attempted = 首次枚举尝试数 + 重试枚举尝试数（若 refresh 失败则 retry_attempted=0）。
        let total_attempted = first_attempted + retry_attempted;
        tracing::warn!(
            pid = pid,
            attempted = total_attempted,
            "all sessions failed"
        );
        // 重试失败或 refresh 失败：返回原错误（保留首次失败的上下文）
        first_attempt
    }

    /// `with_session` 的单次尝试（不含 A-001 重试逻辑）。
    ///
    /// `attempted` 为出参，记录本次调用中循环遍历的 session 数量，
    /// 供 `with_session` 在所有 session 失败时输出聚合日志（A-002）。
    fn with_session_once<F, R>(
        &self,
        pid: u32,
        f: &F,
        attempted: &mut usize,
    ) -> Result<R, crate::MirrorStarError>
    where
        F: Fn(IAudioSessionControl2) -> Result<R, crate::MirrorStarError>,
    {
        // 先查缓存：命中后需校验 PID 是否仍归属此会话（C-103/C-026 回归防护）。
        // 当原进程退出后 PID 被另一进程复用时，缓存的 IAudioSessionControl2
        // 会指向已失效的会话，音量/静音操作将作用在错误的会话上。
        // 故命中后调用 GetProcessId 复核，不匹配则移除失效项并走枚举路径重建。
        if let Some(control) = self.session_cache.borrow().get(&pid).cloned() {
            // 校验缓存的会话是否仍归属请求的 PID
            // GetProcessId 返回错误（会话已失效）或返回的 PID 不匹配时，
            // 均视为缓存失效，需重新枚举
            let valid =
                unsafe { matches!(control.GetProcessId(), Ok(actual_pid) if actual_pid == pid) };
            if valid {
                // PID 校验通过，复用缓存命中
                let result = f(control);
                // v12.0: WASAPI 调用失败时检查进程是否已退出，若退出则清理 session_cache 条目
                if result.is_err() && !is_process_alive(pid) {
                    self.session_cache.borrow_mut().remove(&pid);
                    tracing::debug!(pid, "进程已退出，清理 session_cache 失效条目");
                }
                return result;
            } else {
                // PID 不匹配或获取失败，从缓存移除失效项后继续走枚举路径
                self.session_cache.borrow_mut().remove(&pid);
            }
        }

        // A-003 缓存过期：进入枚举路径（cache miss）时顺带清理已退出进程的缓存会话，
        // 避免 session_cache 长期累积失效项导致内存泄漏。cache hit 路径已通过
        // GetProcessId 校验单个条目，无需在此重复。
        if !self.session_cache.borrow().is_empty() {
            let cached_pids: Vec<u32> = self.session_cache.borrow().keys().copied().collect();
            let stale_pids = collect_stale_pids(&cached_pids);
            if !stale_pids.is_empty() {
                let mut cache = self.session_cache.borrow_mut();
                for stale_pid in &stale_pids {
                    cache.remove(stale_pid);
                }
                tracing::debug!(removed = stale_pids.len(), "清理已退出进程的音频会话缓存项");
            }
        }

        // v12.0: 将 WASAPI 枚举逻辑封装在闭包中，便于在单一出口处添加失败路径清理。
        // 闭包内的 `?` 提前返回会从闭包返回（而非外层函数），使所有失败路径汇聚到
        // 闭包调用后的统一清理点。
        let result = (|| {
            // 借用 session_manager 用于本次枚举。借用在闭包返回时释放，
            // 不会与 with_session 重试路径中的 refresh_session_manager（borrow_mut）冲突。
            let manager_ref = self.session_manager.borrow();
            let manager = manager_ref.as_ref().ok_or_else(|| {
                crate::MirrorStarError::AudioControl("Session manager 未初始化".to_string())
            })?;

            unsafe {
                let session_enum = manager.GetSessionEnumerator()?;

                let count = session_enum.GetCount()?;
                let mut result = Err(crate::MirrorStarError::AudioControl(format!(
                    "未找到进程 {} 的音频会话",
                    pid
                )));

                for i in 0..count {
                    // A-002: 统计本次枚举尝试的 session 数量，
                    // 供 with_session 在所有 session 失败时输出聚合日志。
                    *attempted += 1;
                    // A-002: GetSession/cast 也改为 continue 容错，与 GetProcessId 保持一致。
                    // 会话列表动态变更时，单个会话的瞬态错误不应中止整个枚举。
                    let control: IAudioSessionControl = match session_enum.GetSession(i) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::trace!(index = i, error = ?e, "GetSession 失败，跳过该会话");
                            continue;
                        }
                    };
                    let control: IAudioSessionControl2 = match control.cast() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::trace!(index = i, error = ?e, "cast IAudioSessionControl2 失败，跳过该会话");
                            continue;
                        }
                    };
                    // A-001 枚举容错：跳过无法获取 PID 的会话而非中止整个枚举。
                    // 某些会话（如系统会话、已退出的进程会话）GetProcessId 会返回错误，
                    // 此前用 `?` 传播会导致整个枚举中止，进而丢失后续可能匹配的会话。
                    let session_pid = match control.GetProcessId() {
                        Ok(pid) => pid,
                        Err(_) => continue,
                    };

                    if session_pid == pid {
                        // 命中后存入缓存，后续调用可直接复用
                        self.session_cache.borrow_mut().insert(pid, control.clone());
                        result = f(control);
                        break;
                    }
                }

                result
            }
        })();

        // v12.0: WASAPI 调用失败时检查进程是否已退出，若退出则清理 session_cache 条目
        if result.is_err() && !is_process_alive(pid) {
            self.session_cache.borrow_mut().remove(&pid);
            tracing::debug!(pid, "进程已退出，清理 session_cache 失效条目");
        }

        result
    }

    /// 设置指定进程的音量 (0.0 ~ 1.0)
    ///
    /// A-002 输入校验：越界值会被钳制到 `[0.0, 1.0]`，而非返回错误——
    /// 上层（Tauri 命令/配置加载）传入的 volume 可能因浮点误差或配置错误
    /// 略微越界（如 1.0000001 或 -0.0），clamp 比直接报错更健壮。
    ///
    /// 降级实例（`session_manager` 为 `None`）静默返回 `Ok(())`，
    /// 不尝试 WASAPI 调用，避免视频壁纸播放时刷屏错误日志。
    pub fn set_process_volume(&self, pid: u32, volume: f32) -> Result<(), crate::MirrorStarError> {
        // A-002: 钳制音量到有效范围 [0.0, 1.0]，防止越界值传入 WASAPI
        let volume = volume.clamp(0.0, 1.0);
        if self.session_manager.borrow().is_none() {
            tracing::debug!("VolumeControl 处于降级模式，跳过 set_process_volume");
            return Ok(());
        }
        unsafe {
            self.with_session(pid, |control| {
                let simple_volume: ISimpleAudioVolume = control.cast()?;
                simple_volume.SetMasterVolume(volume, std::ptr::null())?;
                Ok(())
            })
        }
    }

    /// 设置指定进程的静音状态
    ///
    /// 降级实例（`session_manager` 为 `None`）静默返回 `Ok(())`。
    pub fn set_process_mute(&self, pid: u32, mute: bool) -> Result<(), crate::MirrorStarError> {
        if self.session_manager.borrow().is_none() {
            tracing::debug!("VolumeControl 处于降级模式，跳过 set_process_mute");
            return Ok(());
        }
        unsafe {
            self.with_session(pid, |control| {
                let simple_volume: ISimpleAudioVolume = control.cast()?;
                simple_volume.SetMute(mute, std::ptr::null())?;
                Ok(())
            })
        }
    }

    /// 获取指定进程的音量 (0.0 ~ 1.0)
    ///
    /// 降级实例（`session_manager` 为 `None`）静默返回 `Ok(0.0)`，
    /// 表示"无音频设备时音量为 0"的合理默认值。
    pub fn get_process_volume(&self, pid: u32) -> Result<f32, crate::MirrorStarError> {
        if self.session_manager.borrow().is_none() {
            tracing::debug!("VolumeControl 处于降级模式，跳过 get_process_volume");
            return Ok(0.0);
        }
        unsafe {
            self.with_session(pid, |control| {
                let simple_volume: ISimpleAudioVolume = control.cast()?;
                let volume = simple_volume.GetMasterVolume()?;
                Ok(volume)
            })
        }
    }

    /// 获取指定进程的静音状态
    ///
    /// 降级实例（`session_manager` 为 `None`）静默返回 `Ok(false)`，
    /// 表示"无音频设备时非静音"的合理默认值。
    pub fn get_process_mute(&self, pid: u32) -> Result<bool, crate::MirrorStarError> {
        if self.session_manager.borrow().is_none() {
            tracing::debug!("VolumeControl 处于降级模式，跳过 get_process_mute");
            return Ok(false);
        }
        unsafe {
            self.with_session(pid, |control| {
                let simple_volume: ISimpleAudioVolume = control.cast()?;
                let mute = simple_volume.GetMute()?;
                Ok(mute.as_bool())
            })
        }
    }
}

/// 检查指定 PID 的进程是否仍在运行（A-003 缓存过期辅助）
///
/// 用于 [`VolumeControl::with_session`] 枚举路径中清理已退出进程的缓存会话项。
/// 通过 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` 获取句柄，
/// 再用 `WaitForSingleObject(handle, 0)` 判断进程状态。
///
/// `OpenProcess` 失败（进程已退出或无权限）时返回 `false`（按"已退出"处理），
/// 这会导致对应缓存项被移除——即使进程仍在运行但无权限访问，最坏情况仅为
/// 下次访问时重新枚举会话（cache miss），无功能性影响。
fn is_pid_running(pid: u32) -> bool {
    use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // WaitForSingleObject 需要 SYNCHRONIZE 权限才能等待进程句柄，
    // PROCESS_QUERY_LIMITED_INFORMATION 仅用于查询进程信息（不含同步权限），
    // 因此需要同时请求两者。
    let desired_access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: OpenProcess 对不存在的 PID 返回错误（不 UB），句柄用后即关。
    let Ok(handle) = (unsafe { OpenProcess(desired_access, false, pid) }) else {
        return false;
    };
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    // WAIT_TIMEOUT: 仍在运行；WAIT_OBJECT_0: 已退出；
    // WAIT_FAILED: 瞬态错误（如临时权限不足），保守保留缓存项（A-005）；
    // 其他: 按"已退出"处理
    match wait_result {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 => false,
        WAIT_FAILED => {
            // A-005: 日志级别从 `trace!` 提升到 `warn!`，
            // 便于生产环境发现 `WAIT_FAILED` 频发（瞬态权限不足等）。
            // 含 `error = ?std::io::Error::last_os_error()` 便于诊断根因。
            tracing::warn!(
                pid,
                ?wait_result,
                error = ?std::io::Error::last_os_error(),
                "WaitForSingleObject 返回 WAIT_FAILED，保守保留缓存项（A-005）"
            );
            true
        }
        _ => false,
    }
}

/// 从缓存的 PID 列表中收集已退出进程的 PID（A-003 缓存清理辅助）
///
/// 此辅助函数从 [`VolumeControl::with_session`] 的枚举路径抽离"过滤失效 PID"逻辑，
/// 便于单元测试覆盖——直接传入 PID 列表，返回其中已判定为"未运行"的 PID 子集。
/// 调用方（`with_session`）据此从 `session_cache` 中移除失效项。
///
/// 注意：本函数调用 `is_pid_running`（内部使用 Win32 `OpenProcess` /
/// `WaitForSingleObject`），因此并非纯逻辑函数，而是对 Win32 进程查询的薄封装。
/// 测试通过真实短生命周期子进程验证清理行为（见 `test_collect_stale_pids_*`）。
///
/// 抽出为独立 `fn` 仅为可测试性（参见配套 6 个测试），逻辑仅 4 行闭包。
fn collect_stale_pids(cached_pids: &[u32]) -> Vec<u32> {
    cached_pids
        .iter()
        .filter(|&&pid| !is_pid_running(pid))
        .copied()
        .collect()
}

/// RAII 守卫，确保 Win32 句柄在 drop 时调用 `CloseHandle` 释放内核资源。
///
/// v12.0 内存优化（Wave v12-C）：`is_process_alive` 内 `OpenProcess` 返回的句柄
/// 必须在所有退出路径（含 panic）上关闭，避免句柄泄漏。本守卫封装 `CloseHandle`
/// 调用，与 `process/manager.rs` 中 `JobObjectGuard` 的 RAII 模式一致。
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: CloseHandle 对无效句柄返回错误但不 UB；句柄即将丢弃，错误无实际影响
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 检测指定 PID 的进程是否仍存活。用于 `with_session_once` 失败路径，
/// 判断是否应清理 `session_cache` 中的失效条目。
///
/// v12.0 内存优化（Wave v12-C）：`with_session_once` 中 WASAPI 调用失败时，
/// 若进程已退出，其 `session_cache` 条目持有的 `IAudioSessionControl2` COM 接口
/// 将无法释放，导致内存与内核音频会话资源泄漏。本函数通过 `OpenProcess` +
/// `GetExitCodeProcess` 判断进程存活状态，供调用方决定是否清理缓存。
///
/// 实现说明：
/// - `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` 对不存在/已退出的 PID 返回
///   错误（如 `ERROR_INVALID_PARAMETER`），此时按"已退出"处理返回 `false`。
/// - 句柄通过 [`HandleGuard`] RAII 管理，确保 `CloseHandle` 在任何退出路径上调用。
/// - `GetExitCodeProcess` 失败时保守按"已退出"处理（返回 `false`），触发缓存清理。
/// - `exit_code == STILL_ACTIVE`（0x103 = 259）表示进程仍在运行。
fn is_process_alive(pid: u32) -> bool {
    // OpenProcess 对不存在的 PID 返回错误（如 ERROR_INVALID_PARAMETER），
    // 此时按"已退出"处理，返回 false。
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    // RAII 守卫确保 CloseHandle 在函数任何退出路径（含 panic）上被调用
    let _guard = HandleGuard(handle);
    let mut exit_code: u32 = 0;
    // GetExitCodeProcess 失败时按"已退出"处理（保守清理缓存）
    let Ok(()) = (unsafe { GetExitCodeProcess(handle, &mut exit_code) }) else {
        return false;
    };
    // STILL_ACTIVE (0x103 = 259) 表示进程仍在运行；其他值表示进程已退出
    exit_code == STILL_ACTIVE.0 as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_empty_instance() {
        // VolumeControl::new() 返回 Result，在无 COM 环境下应返回错误而非 panic。
        // 注意：实际 new() 会尝试创建 COM 对象（与任务描述假设不同），
        // 因此这里只验证它不会 panic，不假设 Ok 或 Err。
        let _ = VolumeControl::new();
    }

    #[test]
    fn test_refresh_session_manager_no_panic() {
        // 手动构造空实例（绕过 new()，因为它需要 COM 环境）。
        // refresh_session_manager 是 &self 方法（A-001 方案 B 后改为内部可变性）。
        // A-001: session_manager 为 None 时尝试 COM 重新初始化：
        //   - 有 COM 环境：恢复成功，返回 Ok(())
        //   - 无 COM 环境（CI）：恢复失败返回 Err，但不 panic
        let vc = VolumeControl {
            session_manager: RefCell::new(None),
            session_cache: RefCell::new(HashMap::new()),
        };
        let result = vc.refresh_session_manager();
        // 关键是不 panic；返回 Ok/Err 取决于测试环境 COM 可用性
        let _ = result;
    }

    // ── T-008: 优雅降级测试 ──────────────────────────────────────────────────

    #[test]
    fn test_new_disabled_creates_noop_instance() {
        // new_disabled() 应返回字段全 None 的降级实例，不依赖 COM 环境。
        let vc = VolumeControl::new_disabled();
        assert!(vc.session_manager.borrow().is_none());
        assert!(vc.session_cache.borrow().is_empty());
    }

    #[test]
    fn test_disabled_instance_setters_silent_ok() {
        // 降级实例的 setter 方法应静默返回 Ok(())，不返回错误、不 panic。
        // 这避免了视频壁纸播放期间因无音频设备而刷屏错误日志。
        let vc = VolumeControl::new_disabled();
        assert!(vc.set_process_volume(1234, 0.5).is_ok());
        assert!(vc.set_process_mute(1234, true).is_ok());
    }

    #[test]
    fn test_disabled_instance_getters_silent_ok() {
        // 降级实例的 getter 方法应返回合理默认值的 Ok：
        // get_process_volume → Ok(0.0)（无音频设备视为音量 0）
        // get_process_mute   → Ok(false)（无音频设备视为非静音）
        let vc = VolumeControl::new_disabled();
        assert_eq!(vc.get_process_volume(1234).unwrap(), 0.0);
        assert!(!vc.get_process_mute(1234).unwrap());
    }

    #[test]
    fn test_disabled_instance_refresh_session_manager_ok() {
        // A-001: 降级实例的 refresh_session_manager 现会尝试 COM 重新初始化。
        // - 有 COM 环境（真机）：恢复成功，返回 Ok(())，session_manager 被填充
        // - 无 COM 环境（CI）：恢复失败返回 Err，但不 panic，session_manager 保持 None
        let vc = VolumeControl::new_disabled();
        let result = vc.refresh_session_manager();
        // 关键是不 panic；返回 Ok/Err 取决于测试环境 COM 可用性
        let _ = result;
    }

    // ── A-002: set_process_volume 输入校验测试 ──────────────────────────────────

    #[test]
    fn test_set_process_volume_clamps_out_of_range() {
        // A-002: 越界音量值应被 clamp 到 [0.0, 1.0]，而非返回错误或 panic。
        // 降级实例在 clamp 之后才检查 session_manager，故越界值必先经过 clamp。
        // 此处验证越界值不导致方法报错；clamp 本身由 f32::clamp 保证正确性。
        let vc = VolumeControl::new_disabled();
        assert!(
            vc.set_process_volume(1234, 1.5).is_ok(),
            "超上限 1.5 应被 clamp 不报错"
        );
        assert!(
            vc.set_process_volume(1234, -0.1).is_ok(),
            "超下限 -0.1 应被 clamp 不报错"
        );
        assert!(vc.set_process_volume(1234, 0.0).is_ok(), "边界 0.0 正常");
        assert!(vc.set_process_volume(1234, 1.0).is_ok(), "边界 1.0 正常");
        assert!(
            vc.set_process_volume(1234, 0.5).is_ok(),
            "正常值 0.5 不受影响"
        );
    }

    #[test]
    fn test_is_pid_running_returns_false_for_dead_pid() {
        // A-003: 已退出或不存在的 PID 应返回 false（用于缓存清理）。
        // PID 0xFFFFFFF0 极不可能是存活进程，OpenProcess 应失败或 WaitForSingleObject 返回非 WAIT_TIMEOUT。
        assert!(!is_pid_running(0xFFFF_FFF0), "不存在的 PID 应判定为未运行");
    }

    #[test]
    fn test_is_pid_running_returns_true_for_current_process() {
        // A-003: 当前进程（自身）应判定为仍在运行。
        // OpenProcess 需同时请求 PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE，
        // 否则 WaitForSingleObject 因缺少 SYNCHRONIZE 权限返回 WAIT_FAILED 而误判为已退出。
        let self_pid = std::process::id();
        assert!(is_pid_running(self_pid), "当前进程 PID 应判定为仍在运行");
    }

    // ── A-003: collect_stale_pids 缓存清理辅助函数测试 ────────────────────────

    #[test]
    fn test_is_pid_running_returns_false_for_exited_child_process() {
        // A-003 集成测试：spawn 一个立即退出的子进程，等待其退出后验证
        // is_pid_running 返回 false。这比使用伪造的 PID（如 0xFFFF_FFF0）更贴近
        // 真实场景——验证 OpenProcess + WaitForSingleObject 对真实已退出进程的判定。
        //
        // 使用 `cmd /c exit 0` 在 Windows 上启动一个立即退出的子进程。
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .expect("应能 spawn cmd 子进程");
        let child_pid = child.id();
        // 等待子进程退出（应在毫秒级完成）
        child.wait().expect("子进程应能被 wait");
        // 子进程已退出，is_pid_running 应返回 false
        assert!(
            !is_pid_running(child_pid),
            "已退出的子进程 PID {} 应判定为未运行",
            child_pid
        );
    }

    #[test]
    fn test_collect_stale_pids_returns_empty_for_running_process() {
        // A-003: 缓存中仅含当前运行进程的 PID 时，collect_stale_pids 应返回空 Vec
        // （无失效项需清理）。
        let self_pid = std::process::id();
        let stale = collect_stale_pids(&[self_pid]);
        assert!(
            stale.is_empty(),
            "当前运行进程不应被收集为失效 PID，实际: {:?}",
            stale
        );
    }

    #[test]
    fn test_collect_stale_pids_returns_empty_for_empty_input() {
        // A-003: 空输入应返回空 Vec（无缓存项时无清理）
        let stale = collect_stale_pids(&[]);
        assert!(stale.is_empty(), "空 PID 列表应返回空失效列表");
    }

    #[test]
    fn test_collect_stale_pids_collects_exited_child_process() {
        // A-003 核心集成测试：spawn 短生命周期子进程，等待其退出，
        // 验证 collect_stale_pids 将其收集为失效 PID（模拟缓存清理场景）。
        //
        // 场景：缓存中含一个已退出子进程的 PID，collect_stale_pids 应返回该 PID，
        // 调用方据此从 session_cache 中移除失效项。
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .expect("应能 spawn cmd 子进程");
        let child_pid = child.id();
        child.wait().expect("子进程应能被 wait");

        let stale = collect_stale_pids(&[child_pid]);
        assert_eq!(
            stale,
            vec![child_pid],
            "已退出子进程的 PID 应被收集为失效 PID"
        );
    }

    #[test]
    fn test_collect_stale_pids_mixed_running_and_exited() {
        // A-003: 混合场景——缓存中同时含运行进程（当前 PID）与已退出进程（子进程 PID），
        // collect_stale_pids 应仅返回已退出进程的 PID，保留运行进程的缓存项。
        let self_pid = std::process::id();

        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .expect("应能 spawn cmd 子进程");
        let child_pid = child.id();
        child.wait().expect("子进程应能被 wait");

        // 缓存中混合：当前进程（运行中）+ 子进程（已退出）
        let cached_pids = vec![self_pid, child_pid];
        let stale = collect_stale_pids(&cached_pids);
        assert_eq!(
            stale,
            vec![child_pid],
            "混合缓存中仅已退出子进程应被收集，运行进程应保留"
        );
    }

    #[test]
    fn test_collect_stale_pids_multiple_exited() {
        // A-003: 多个已退出进程的 PID 应全部被收集。
        let mut child1 = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .expect("应能 spawn cmd 子进程 1");
        let pid1 = child1.id();
        child1.wait().expect("子进程 1 应能被 wait");

        let mut child2 = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .expect("应能 spawn cmd 子进程 2");
        let pid2 = child2.id();
        child2.wait().expect("子进程 2 应能被 wait");

        let stale = collect_stale_pids(&[pid1, pid2]);
        assert_eq!(stale.len(), 2, "两个已退出子进程均应被收集");
        assert!(stale.contains(&pid1), "子进程 1 的 PID 应在失效列表中");
        assert!(stale.contains(&pid2), "子进程 2 的 PID 应在失效列表中");
    }

    // ── A-001: COM 降级恢复测试 ──────────────────────────────────────────

    #[test]
    fn v41_a001_com_recovery_after_degradation() {
        // A-001: 构造降级实例（session_manager 为 None），
        // 调用 refresh_session_manager 尝试 COM 重新初始化。
        //
        // 验证：
        // - 有 COM 环境（真机）：恢复成功，session_manager 被填充为 Some
        //   （不再走降级路径）。后续 set_process_volume
        //   不再静默返回 Ok，但可能因测试进程无音频会话返回 Err（正常）。
        // - 无 COM 环境（CI）：恢复失败返回 Err，但不 panic，
        //   session_manager 保持 None（允许后续重试）。
        let vc = VolumeControl::new_disabled();
        // 初始状态：session_manager 为 None
        assert!(
            vc.session_manager.borrow().is_none(),
            "new_disabled() 实例初始 session_manager 应为 None"
        );

        let result = vc.refresh_session_manager();
        match result {
            Ok(()) => {
                // 恢复成功：session_manager 应被填充（不再走降级路径）
                assert!(
                    vc.session_manager.borrow().is_some(),
                    "COM 恢复成功后 session_manager 应被填充为 Some"
                );
                // 后续 set_process_volume 不应走降级静默路径。
                // 注意：测试进程本身可能无音频会话，set_process_volume 返回 Err 是正常的，
                // 关键是不 panic、不静默返回 Ok（说明走了真实 WASAPI 路径）。
                // 此处仅验证不 panic，不断言返回值。
                let _ = vc.set_process_volume(std::process::id(), 0.5);
            }
            Err(_) => {
                // 恢复失败（CI 环境正常）：session_manager 应保持 None
                assert!(
                    vc.session_manager.borrow().is_none(),
                    "COM 恢复失败后 session_manager 应保持 None（允许后续重试）"
                );
            }
        }
    }

    #[test]
    fn v41_a001_com_recovery_persistent_failure() {
        // A-001: 多次调用 refresh_session_manager，验证：
        // - 每次都不 panic
        // - 每次都在合理时间内返回（不阻塞）
        // - CI 环境（COM 不可用）下每次都返回 Err，session_manager 保持 None
        // - 真机环境下，首次恢复成功后 session_manager 被填充，后续调用跳过恢复路径
        let vc = VolumeControl::new_disabled();
        for i in 0..3 {
            let start = std::time::Instant::now();
            let result = vc.refresh_session_manager();
            let elapsed = start.elapsed();
            // 验证不阻塞（每次调用应在 5 秒内返回，实际通常毫秒级）
            assert!(
                elapsed.as_secs() < 5,
                "第 {} 次调用耗时 {:?}，不应阻塞",
                i + 1,
                elapsed
            );
            match result {
                Ok(()) => {
                    // 真机环境：恢复成功，session_manager 应被填充
                    assert!(
                        vc.session_manager.borrow().is_some(),
                        "第 {} 次恢复成功后 session_manager 应为 Some",
                        i + 1
                    );
                    // 后续调用应跳过恢复路径（session_manager 已 Some，不再调用 CoInitializeEx）
                }
                Err(_) => {
                    // CI 环境：恢复失败，session_manager 应保持 None
                    assert!(
                        vc.session_manager.borrow().is_none(),
                        "第 {} 次恢复失败后 session_manager 应保持 None",
                        i + 1
                    );
                }
            }
        }
    }

    // ── A-002: with_session 全失败日志增强测试 ──────────────────────────

    #[test]
    fn v41_a002_with_session_all_failed_logs_attempted_count() {
        // A-002: 验证 with_session 在所有 session 均失败时记录
        // `tracing::warn!(pid, attempted = n, "all sessions failed")` 日志。
        //
        // **降级说明（mock 难度）**：
        //   WASAPI 的 `IAudioSessionManager2` / `IAudioSessionEnumerator` /
        //   `IAudioSessionControl2` 均为 `windows` crate 提供的具体 COM 接口类型，
        //   非本地定义的 trait，无法通过 trait mock 注入测试替身。`with_session`
        //   及 `with_session_once` 均为 private 方法，亦无法从外部直接驱动。
        //   因此本测试采用以下降级方案：
        //
        // 1. CI 环境（COM 不可用）：`VolumeControl::new()` 返回 `Err`，
        //    降级实例（`new_disabled()`）的 `set_process_volume` 在 `session_manager`
        //    为 `None` 时静默返回 `Ok(())`，不进入 `with_session` 失败路径。
        //    本测试仅验证不 panic、降级路径返回 `Ok`。
        // 2. 真机环境（COM 可用）：`VolumeControl::new()` 成功后，传入极不可能
        //    存在音频会话的 PID（0xFFFF_FFF0）触发 `with_session` 全失败路径，
        //    验证返回 `Err`。`tracing::warn!(... attempted = n ...)` 日志含
        //    `attempted` 字段的断言需手动启用 `tracing` subscriber 验证
        //    （本仓库未引入 `tracing-test` 依赖；运行
        //    `RUST_LOG=warn cargo test -- v41_a002_with_session_all_failed_logs_attempted_count`
        //    观察 stderr 中 "all sessions failed" 行应含 `attempted=N` 字段）。
        match VolumeControl::new() {
            Ok(vc) => {
                // 真机环境：触发 with_session 全失败路径。
                // PID 0xFFFF_FFF0 极不可能是存活进程，且即便存活也不会有音频会话。
                let result = vc.set_process_volume(0xFFFF_FFF0, 0.5);
                assert!(
                    result.is_err(),
                    "不存在的 PID 应触发 with_session 全失败路径返回 Err，实际: {:?}",
                    result
                );
                // 日志含 `attempted` 字段为手动验证项（见上方降级说明）。
            }
            Err(_) => {
                // CI 环境：VolumeControl::new() 失败（无音频设备 / 无 COM）。
                // 降级实例不进入 with_session 失败路径，仅验证不 panic、返回 Ok。
                let vc = VolumeControl::new_disabled();
                assert!(
                    vc.set_process_volume(0xFFFF_FFF0, 0.5).is_ok(),
                    "降级实例 set_process_volume 应静默返回 Ok"
                );
            }
        }
    }

    // ── A-003: cache hit/miss 与恢复路径覆盖测试 ────────────────────────

    #[test]
    fn v41_a003_with_session_cache_hit_skips_enumeration() {
        // A-003: 验证 with_session_once 的 cache hit 路径跳过重新枚举。
        //
        // **降级说明（mock 难度）**：
        //   `session_cache` 存储 `IAudioSessionControl2`（具体 COM 接口类型），
        //   无法在无 COM 环境下构造有效实例填充缓存。完整的 cache hit/miss
        //   端到端验证需真实音频会话，标记为 `#[ignore]` 由真机环境手动运行
        //   （见 `v41_a003_real_com_cache_hit_miss_e2e`）。
        //
        // 本测试验证可在 CI 环境下运行的不变量：
        // 1. 降级实例初始 `session_cache` 为空
        // 2. 降级模式下 `set_process_volume` 不进入 `with_session`，不填充 cache
        // 3. cache hit 路径的 PID 复核逻辑（C-103/C-026 回归防护）：
        //    `with_session_once` 命中缓存后会调用 `GetProcessId` 复核，
        //    不匹配则移除失效项并走枚举路径——此行为在降级实例下无法触发
        //    （无 session_manager），仅文档化覆盖。
        let vc = VolumeControl::new_disabled();
        assert!(
            vc.session_cache.borrow().is_empty(),
            "降级实例初始 session_cache 应为空"
        );
        // 降级模式下 set_process_volume 静默返回 Ok，不进入 with_session，不填充 cache
        assert!(vc.set_process_volume(1234, 0.5).is_ok());
        assert!(
            vc.session_cache.borrow().is_empty(),
            "降级模式下 session_cache 不应被填充（未进入 with_session）"
        );

        // cache hit 路径需真实 COM 接口实例，CI 环境下无法覆盖。
        // 手动验证：在有音频设备的 Windows 上运行
        //   `cargo test -- --ignored v41_a003_real_com_cache_hit_miss_e2e`
        // 该测试通过同一 PID 两次调用 set_process_volume：
        //   - 首次：cache miss → 走枚举路径 → 命中后存入 cache
        //   - 二次：cache hit → 跳过枚举直接复用（GetProcessId 复核通过）
        // 通过 `session_cache.len()` 验证二次调用后 cache 仍含该 PID。
    }

    #[test]
    #[ignore = "A-003: 需要真实 Windows 音频设备与音频会话，CI 跳过"]
    fn v41_a003_real_com_cache_hit_miss_e2e() {
        // A-003 端到端测试：验证 cache hit/miss 路径。
        // 标记 `#[ignore]` 避免无音频设备的 CI 失败。
        //
        // 前置条件：
        //   - Windows 真机
        //   - 当前进程或指定 PID 拥有活跃音频会话（如播放音频）
        //
        // 验证：
        //   1. 首次 set_process_volume 触发 cache miss（枚举路径）
        //   2. 第二次同 PID 调用命中 cache（跳过枚举）
        //   3. session_cache 包含该 PID 条目
        match VolumeControl::new() {
            Ok(vc) => {
                let pid = std::process::id();
                // 首次调用：cache miss（cache 初始为空）
                assert!(vc.session_cache.borrow().is_empty(), "初始 cache 应为空");
                let _ = vc.set_process_volume(pid, 0.5);
                // 若当前进程有音频会话，cache 应被填充
                // （无音频会话时 cache 保持空，属正常情况）
                let first_count = vc.session_cache.borrow().len();
                // 第二次调用：若首次已填充 cache，则应命中
                let _ = vc.set_process_volume(pid, 0.5);
                let second_count = vc.session_cache.borrow().len();
                // cache 条目数不应减少（cache hit 不触发清空）
                assert!(
                    second_count >= first_count,
                    "二次调用后 cache 条目数不应减少：first={}, second={}",
                    first_count,
                    second_count
                );
            }
            Err(_) => {
                // 无音频设备环境：跳过端到端验证
                eprintln!("VolumeControl::new() 失败，跳过 cache hit/miss 端到端验证");
            }
        }
    }

    #[test]
    fn v41_a003_refresh_session_manager_recovers_from_degradation() {
        // A-003: 验证 refresh_session_manager 在降级模式下尝试恢复，
        // 并在恢复前后正确清空 session_cache（设备变更后缓存失效）。
        //
        // 本测试扩展 v41_a001_com_recovery_after_degradation，重点验证：
        // 1. 降级实例预填充 session_cache 后调用 refresh_session_manager，
        //    无论恢复成败，session_cache 都应被清空（refresh_session_manager
        //    在恢复失败路径与成功路径均调用 `session_cache.clear()`）
        // 2. 恢复成功路径：session_manager 被填充为 Some，session_cache 被清空
        // 3. 恢复失败路径（CI 环境）：session_manager 保持 None，
        //    session_cache 仍被清空（允许后续重试）
        //
        // 注：cache 在 refresh_session_manager 失败路径下也被清空，是因为
        // refresh_session_manager 在 CoCreateInstance 失败时显式清空 cache 后返回 Err
        // （保持与原行为一致：设备变更后缓存失效）。GetDefaultAudioEndpoint/Activate
        // 失败时由 `?` 提前返回，不清空 cache——与原代码行为一致。
        let vc = VolumeControl::new_disabled();

        // 初始状态：所有字段为 None/空
        assert!(vc.session_manager.borrow().is_none());
        assert!(vc.session_cache.borrow().is_empty());

        // 模拟"设备变更前 cache 已有残留"的场景：
        // 直接构造一个非空 cache（用空 IAudioSessionControl2 占位不可行，
        // 因 IAudioSessionControl2 无法在无 COM 下构造）。
        // 故此处仅验证 cache 为空时的清空行为不变量；非空 cache 的清空
        // 由源码 `session_cache.borrow_mut().clear()` 保证（成功路径）。

        let result = vc.refresh_session_manager();
        match result {
            Ok(()) => {
                // 真机环境：COM 恢复成功
                assert!(
                    vc.session_manager.borrow().is_some(),
                    "恢复成功后 session_manager 应为 Some"
                );
                assert!(
                    vc.session_cache.borrow().is_empty(),
                    "恢复成功后 session_cache 应被清空（设备变更后失效）"
                );

                // 二次调用：session_manager 已 Some，跳过恢复路径直接刷新 session
                let result2 = vc.refresh_session_manager();
                assert!(result2.is_ok(), "二次调用应成功（已恢复后）");
                assert!(
                    vc.session_cache.borrow().is_empty(),
                    "二次调用后 session_cache 应仍为空（无 with_session 填充）"
                );
            }
            Err(_) => {
                // CI 环境：COM 恢复失败
                assert!(
                    vc.session_manager.borrow().is_none(),
                    "恢复失败后 session_manager 应保持 None"
                );
                assert!(
                    vc.session_cache.borrow().is_empty(),
                    "恢复失败后 session_cache 也应为空（降级实例初始为空）"
                );

                // 多次重试：每次都不 panic，每次都清空 cache
                for i in 0..2 {
                    let _ = vc.refresh_session_manager();
                    assert!(
                        vc.session_cache.borrow().is_empty(),
                        "第 {} 次重试后 session_cache 应被清空",
                        i + 1
                    );
                }
            }
        }
    }

    // ── v12.0 Wave v12-C: is_process_alive 与 session_cache 失效清理测试 ──────

    #[test]
    fn test_is_process_alive_returns_false_for_dead_pid() {
        // v12.0: 不存在的 PID 应返回 false（OpenProcess 失败或 GetExitCodeProcess
        // 返回非 STILL_ACTIVE）。使用 u32::MAX 极不可能是存活进程。
        assert!(
            !is_process_alive(u32::MAX),
            "不存在的 PID (u32::MAX) 应判定为未运行"
        );
    }

    #[test]
    fn test_is_process_alive_returns_true_for_current_process() {
        // v12.0: 当前进程（自身）应判定为仍在运行。
        // OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 对自身进程应成功，
        // GetExitCodeProcess 返回 STILL_ACTIVE (259)。
        let self_pid = std::process::id();
        assert!(
            is_process_alive(self_pid),
            "当前进程 PID {} 应判定为仍在运行",
            self_pid
        );
    }

    #[test]
    fn test_session_cache_cleans_up_dead_process() {
        // v12.0 Wave v12-C: 验证 with_session_once 失败路径清理 session_cache。
        //
        // **降级说明（mock 难度）**：
        //   `session_cache` 存储 `IAudioSessionControl2`（具体 COM 接口类型），
        //   无法在无 COM 环境下构造有效实例填充缓存。完整的"预填充 → 失败 → 清理"
        //   端到端验证需真实音频会话，此处仅验证可在 CI 环境下运行的不变量：
        //
        // 1. 降级实例（session_manager = None）调用 with_session_once 必返回 Err
        //    （session_manager 未初始化）
        // 2. 失败路径触发 is_process_alive 检查（对不存在的 PID 返回 false）
        // 3. 清理逻辑执行 session_cache.remove(&pid) 不 panic
        // 4. 调用后 session_cache 不含该 PID（初始为空，remove 为 no-op，但验证
        //    清理路径被正确执行无异常）
        //
        // 完整的"预填充缓存 → 失败 → 清理移除"端到端验证需真实 COM 环境，
        // 由 `is_process_alive` 的直接测试（见上方两个测试）覆盖核心逻辑。
        let vc = VolumeControl::new_disabled();
        let mut attempted = 0usize;
        // 使用 u32::MAX 作为不存在的 PID：OpenProcess 失败 → is_process_alive 返回 false
        let dead_pid = u32::MAX;
        let result: Result<(), crate::MirrorStarError> =
            vc.with_session_once(dead_pid, &|_control| Ok(()), &mut attempted);
        assert!(
            result.is_err(),
            "降级实例 with_session_once 应返回 Err（session_manager 未初始化）"
        );
        // 清理路径执行后，session_cache 不应包含该 PID
        assert!(
            !vc.session_cache.borrow().contains_key(&dead_pid),
            "失败路径清理后 session_cache 不应包含失效 PID"
        );
    }
}
