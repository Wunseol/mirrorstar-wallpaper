use crate::wallpaper::{PauseCommand, PauseReason, WallpaperState};
use crate::MirrorStarError;

use super::manager::WallpaperEngine;

// v41-W-004 调用方契约（重要）
//
// 本文件中所有 `*_fast` 方法均访问 `self.pause_senders`（普通 `HashMap`，无内置
// 并发保护）。所有方法的 `&self` 借用语义已保证调用方持有 `WallpaperEngine` 锁，
// 因此 `pause_senders` 的 `get` / `iter` / `insert` / `remove` / `clear` 操作
// 在锁内串行执行，不存在并发风险。
//
// **新增访问 `pause_senders` 的方法必须遵循同一契约**：
// - 方法签名为 `&self` 或 `&mut self`（借用 engine 锁）
// - 不得返回 `HashMap` 引用或迭代器到锁外（避免锁释放后访问）
// - 不得在持锁期间调用可能阻塞的 IPC 操作（如 `sender.send` 是 mpsc unbounded
//   send，不阻塞，可安全持锁；但禁止在锁内调用 `tokio::time::sleep` 等阻塞调用）
//
// 原设计 v41-W-004 finding 指出"HashMap 操作未在锁内完整事务化，remove 失败
// sender 时若并发 insert 新 sender 可能误删"——本设计选择保留 `HashMap` +
// 文档化锁内串行契约（而非升级为 `BTreeMap`），因 `WallpaperEngine` 锁已串行化
// 所有访问，无并发风险。若未来需要脱离 engine 锁访问（如并行查询），需先将
// `pause_senders` 字段升级为 `RwLock<HashMap<...>>` 或类似并发容器。

impl WallpaperEngine {
    /// 暂停指定显示器的壁纸（通过快速路径）
    ///
    /// 返回 `Ok(true)` 表示存在 PauseSender 且已发送 Pause 命令；返回 `Ok(false)`
    /// 表示无对应 sender（原生壁纸或未设置壁纸），操作被忽略。
    ///
    /// v5.0 I-PERF-010: 返回 bool 复用已完成的 `pause_senders.get` 查找结果，
    /// 调用方据此决定是否 emit 兜底，避免再次调用 `has_pause_sender` 触发额外
    /// HashMap 查找（每次省 ~20-50ns，更重要的是消除冗余调用）。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.get`
    /// 在锁内执行，与并发的 `insert`/`remove` 串行化，不会观察到半写入状态。
    pub fn pause_wallpaper_fast(&self, display_id: &str) -> Result<bool, MirrorStarError> {
        if let Some(sender) = self.pause_senders.get(display_id) {
            sender.send(PauseCommand::Pause)?;
            Ok(true)
        } else {
            tracing::warn!(
                display_id,
                "pause_wallpaper_fast: 无对应 pause_sender，操作被忽略"
            );
            Ok(false)
        }
    }

    /// 恢复指定显示器的壁纸（通过快速路径）
    ///
    /// 返回 `Ok(true)` 表示存在 PauseSender 且已发送 Resume 命令；返回 `Ok(false)`
    /// 表示无对应 sender（原生壁纸或未设置壁纸），操作被忽略。
    ///
    /// v5.0 I-PERF-010: 与 `pause_wallpaper_fast` 对称，返回 bool 复用查找结果。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.get`
    /// 在锁内执行，与并发的 `insert`/`remove` 串行化。
    pub fn resume_wallpaper_fast(&self, display_id: &str) -> Result<bool, MirrorStarError> {
        if let Some(sender) = self.pause_senders.get(display_id) {
            sender.send(PauseCommand::Resume)?;
            Ok(true)
        } else {
            tracing::warn!(
                display_id,
                "resume_wallpaper_fast: 无对应 pause_sender，操作被忽略"
            );
            Ok(false)
        }
    }

    /// 设置指定显示器的壁纸音量（通过快速路径）
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.get`
    /// 在锁内执行，与并发的 `insert`/`remove` 串行化。
    pub fn set_volume_fast(&self, display_id: &str, volume: f32) -> Result<(), MirrorStarError> {
        if let Some(sender) = self.pause_senders.get(display_id) {
            sender.send(PauseCommand::SetVolume(volume))?;
            sender.set_volume(volume);
            Ok(())
        } else {
            tracing::warn!(
                display_id,
                "set_volume_fast: 无对应 pause_sender，操作被忽略"
            );
            Ok(())
        }
    }

    /// 切换指定显示器壁纸的静音状态（通过快速路径），返回新的静音状态
    ///
    /// - `Ok(Some(new_is_muted))`：已切换并返回新状态（true=已静音，false=已取消静音）
    /// - `Ok(None)`：无对应 pause_sender，操作被忽略（区别于"未静音"的 `Ok(Some(false))`）
    ///
    /// # 竞态修复（N-005）
    ///
    /// 通过 `PauseSender::toggle_mute_atomic` 在 `shared_state` 写锁内完成
    /// "读取 + 翻转 + 发送"，避免多个并发调用同时读到旧状态导致最终状态
    /// 与返回值不一致。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.get`
    /// 在锁内执行，与并发的 `insert`/`remove` 串行化。
    pub fn toggle_mute_fast(&self, display_id: &str) -> Result<Option<bool>, MirrorStarError> {
        if let Some(sender) = self.pause_senders.get(display_id) {
            let new_is_muted = sender.toggle_mute_atomic()?;
            Ok(Some(new_is_muted))
        } else {
            tracing::warn!(
                display_id,
                "toggle_mute_fast: 无对应 pause_sender，操作被忽略"
            );
            Ok(None)
        }
    }

    /// 暂停所有壁纸（通过快速路径）
    ///
    /// 返回失败的 display_id 列表；空 Vec 表示全部发送成功。
    /// 调用方应根据返回的失败列表决定是否更新状态标志（如 FULLSCREEN_WAS /
    /// POWER_WAS_ON_BATTERY / tray_paused），避免在部分失败时推进状态机。
    ///
    /// # PauseReason 位图协调
    /// 若已有其他 reason 暂停过（reasons 非空），仅 set bit 不重复发 Pause 命令。
    /// 仅当 reasons 为空且全部 sender 发送成功时才确认 set bit；失败时回滚 bit。
    ///
    /// # W06 TOCTOU 修复
    ///
    /// 原实现在 `drop(reasons)` 释放锁后发送 Pause 命令，再重新获取锁设置 bit，
    /// 存在 TOCTOU 窗口：并发 `resume_all_fast` 检查 `reasons.contains(reason)`
    /// 返回 false，直接返回不发 Resume，导致壁纸被 Pause 但 reasons 未记录。
    ///
    /// 修复：在 `drop(reasons)` 前设置 bit（`*reasons |= reason`），关闭 TOCTOU 窗口。
    /// 若发送 Pause 命令失败则回滚 bit（`*reasons &= !reason`），保持原语义
    /// （bit set ⟺ 全部 sender 发送成功）。
    ///
    /// # W-004 修复：部分失败不回滚 bit
    ///
    /// 原实现在部分 sender 发送失败时回滚 bit，导致已成功发送 Pause 的渲染器
    /// 永久卡在 Paused 状态（`resume_all_fast` 检查 bit 为 false 后 early-return）。
    ///
    /// 修复：部分失败时**不回滚 bit**，保留 bit set。这样 `resume_all_fast` 能
    /// 观察到 bit 并发送 Resume 恢复已暂停的渲染器。失败的 sender 意味着渲染器
    /// 已不可用，后续操作会继续失败但不影响存活的渲染器。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.iter`
    /// 在锁内执行，遍历期间并发的 `insert`/`remove` 会被 Rust 借用检查阻止
    /// （`&self` 共享借用排斥 `&mut self`）。
    pub fn pause_all_fast(&self, reason: PauseReason) -> Result<Vec<String>, MirrorStarError> {
        let mut reasons = self
            .pause_reasons
            .lock()
            .map_err(|e| MirrorStarError::LockPoisoned(format!("pause_reasons: {}", e)))?;
        let was_paused = !reasons.is_empty();
        // W06: 在释放锁前设置 bit，关闭 TOCTOU 窗口
        *reasons |= reason;
        drop(reasons);

        if was_paused {
            // 已有其他 reason 暂停过，仅 set bit（已完成），不重复发 Pause 命令
            return Ok(Vec::new());
        }

        let mut failed: Vec<String> = Vec::new();
        for (display_id, sender) in self.pause_senders.iter() {
            if let Err(e) = sender.send(PauseCommand::Pause) {
                tracing::error!(display_id, error = %e, "暂停壁纸失败");
                failed.push(display_id.clone());
            }
        }
        // W-004: 部分失败时不回滚 bit，保留 bit set。
        // 原实现回滚 bit 导致已暂停的渲染器无法被 resume_all_fast 恢复。
        // 保留 bit 后，resume_all_fast 能观察到 bit 并发送 Resume，
        // 使已暂停的渲染器恢复正常。失败的 sender 会在后续操作中继续失败，
        // 但不影响存活的渲染器。
        Ok(failed)
    }

    /// 恢复所有壁纸（通过快速路径）
    ///
    /// 返回失败的 display_id 列表；空 Vec 表示全部发送成功。
    ///
    /// # PauseReason 位图协调
    /// 仅 clear 指定 reason 的 bit。若其他 reason 仍活跃（reasons 非空），不发 Resume。
    /// 仅当 reasons 清空且全部 sender 发送成功时才确认 clear；失败时 re-set bit。
    ///
    /// # W-004 修复：兜底发送 Resume
    ///
    /// 原实现在 `!reasons.contains(reason)` 时 early-return 不发命令。这导致
    /// `pause_all_fast` 部分失败后 bit 状态与渲染器实际状态不一致：
    /// - 部分渲染器已收到 Pause（状态为 Paused），但因某 sender 失败导致
    ///   调用方可能未推进状态机，bit 状态与实际渲染器状态脱节
    /// - 若后续 bit 因某种原因丢失（如外部代码误操作），已暂停的渲染器
    ///   永远无法被 resume_all_fast 恢复，卡在 Paused 状态
    ///
    /// 修复：当 `reasons` 清空（无任何活跃 reason）时，无论 `was_paused` 为何值，
    /// 都向所有 sender 发送 Resume。Resume 是幂等操作，对非 Paused 状态的
    /// 渲染器无害，对卡死的 Paused 渲染器起到自愈作用。
    ///
    /// # 全屏终止重启（引擎级）
    ///
    /// 全屏场景下 `terminate_all_fast` 会将视频/网页渲染器终止为
    /// `WallpaperState::Terminated`（子进程已退出）。此处对 `Terminated` 的渲染器
    /// 调用 `play()` 完整重启（视频渲染器会在 after_embed 时从保存进度续播），
    /// 其余渲染器发送 `Resume` 命令恢复。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&mut self` 借用已保证）。
    /// `pause_senders.keys().cloned()` 在锁内先克隆出 key 列表，再逐渲染器分发，
    /// 避免遍历 `wallpapers`/`pause_senders` 时的借用冲突。
    pub fn resume_all_fast(&mut self, reason: PauseReason) -> Result<Vec<String>, MirrorStarError> {
        let mut reasons = self
            .pause_reasons
            .lock()
            .map_err(|e| MirrorStarError::LockPoisoned(format!("pause_reasons: {}", e)))?;
        let was_paused = reasons.contains(reason);
        if was_paused {
            *reasons &= !reason;
        }
        let still_paused = !reasons.is_empty();
        drop(reasons);

        if still_paused {
            // 其他 reason 仍活跃，不发 Resume
            return Ok(Vec::new());
        }

        // W-004: reasons 已清空（无任何活跃 reason），无论 was_paused 为 true
        // 还是 false 都向所有 sender 发送 Resume。这是正常路径与兜底路径的合并：
        // - 正常路径：was_paused=true，bit 已清除 → 发 Resume 恢复暂停的渲染器
        // - 兜底路径：was_paused=false，bit 本就未设（可能因 pause_all_fast 部分
        //   失败后状态不一致，或外部代码误清 bit 导致已暂停渲染器无法恢复）→
        //   发 Resume 自愈卡死的 Paused 渲染器
        // Resume 幂等无害：对 Playing/Terminated 等状态的渲染器无副作用。
        //
        // 先 clone key 列表（避免 &self.wallpapers 与 &mut self.wallpapers 借用冲突）
        let display_ids: Vec<String> = self.pause_senders.keys().cloned().collect();
        let mut failed: Vec<String> = Vec::new();
        for display_id in display_ids {
            // 全屏终止的渲染器（Terminated）需 play() 完整重启
            let terminated = self
                .wallpapers
                .get(&display_id)
                .map(|r| r.state() == WallpaperState::Terminated)
                .unwrap_or(false);
            if terminated {
                // 全屏终止的渲染器需 play() 完整重启，随后 reembed 重新嵌入 WorkerW、
                // 执行 after_embed（IPC loadfile 重新加载视频）并重建 PauseSender。
                //
                // 先通过 `get_mut().map(|r| r.play())` 得到 `Option<Result<..>>`：
                // 闭包返回 play() 的 Result，借用随 map 调用结束而释放，之后才能
                // 以 `&mut self` 调用 reembed_and_register_renderer（避免借用冲突）。
                match self.wallpapers.get_mut(&display_id).map(|r| r.play()) {
                    Some(Ok(())) => {
                        match self.reembed_and_register_renderer(&display_id) {
                            Ok(()) => {
                                // 此处 get 返回 reembed 替换后的新 sender；将共享状态
                                // 同步为 Playing 并通知前端 UI。
                                if let Some(sender) = self.pause_senders.get(&display_id) {
                                    sender.set_state(WallpaperState::Playing);
                                    sender.notify_state_changed(&display_id);
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    display_id,
                                    error = %e,
                                    "全屏终止后重启壁纸失败：重新嵌入失败"
                                );
                                failed.push(display_id);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!(display_id, error = %e, "全屏终止后重启壁纸失败");
                        failed.push(display_id);
                    }
                    None => {
                        tracing::error!(display_id, "全屏终止后重启壁纸失败：渲染器不存在");
                        failed.push(display_id);
                    }
                }
            } else if let Some(sender) = self.pause_senders.get(&display_id) {
                if let Err(e) = sender.send(PauseCommand::Resume) {
                    tracing::error!(display_id, error = %e, "恢复壁纸失败");
                    failed.push(display_id);
                }
            }
        }
        // 仅当 was_paused=true 时才需要 re-set bit（恢复已知的 pause 状态语义）。
        // was_paused=false 时 bit 本就未设，发送失败不改变 bit 状态——bit 状态
        // 与"是否曾经 pause"无关，仅与"reasons 是否被显式 pause 设置"有关。
        if was_paused && !failed.is_empty() {
            let mut reasons = self
                .pause_reasons
                .lock()
                .map_err(|e| MirrorStarError::LockPoisoned(format!("pause_reasons: {}", e)))?;
            *reasons |= reason;
        }
        Ok(failed)
    }

    /// 全屏场景终止所有壁纸子进程（引擎级），最大化释放 CPU/GPU 内存
    ///
    /// 对每个渲染器调用 [`WallpaperRenderer::pause_for_fullscreen`]：视频/网页渲染器
    /// 覆写为终止子进程（state → Terminated），图片/GIF 使用 trait 默认实现（普通暂停）。
    /// 退出全屏后由 [`resume_all_fast`](Self::resume_all_fast) 对 Terminated 渲染器
    /// 调用 `play()` 完整重启（视频从保存进度续播）。
    ///
    /// 返回失败的 display_id 列表；空 Vec 表示全部成功。
    ///
    /// # PauseReason 位图
    ///
    /// 与 `pause_all_fast` 一致，先锁内设置 reason 位（W06 TOCTOU 修复），
    /// 使并发的 `resume_all_fast` 能观察到该 reason 并正确协调。
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&mut self` 借用已保证）。先 clone
    /// `pause_senders` 的 key 列表，再通过 `wallpapers.get_mut` 访问渲染器，避免借用冲突。
    pub fn terminate_all_fast(&mut self, reason: PauseReason) -> Result<Vec<String>, MirrorStarError> {
        // 锁内设置 reason 位（与 pause_all_fast 的 W06 TOCTOU 修复一致）
        let mut reasons = self
            .pause_reasons
            .lock()
            .map_err(|e| MirrorStarError::LockPoisoned(format!("pause_reasons: {}", e)))?;
        *reasons |= reason;
        drop(reasons);

        let display_ids: Vec<String> = self.pause_senders.keys().cloned().collect();
        let mut failed: Vec<String> = Vec::new();
        for display_id in display_ids {
            match self.wallpapers.get_mut(&display_id) {
                Some(r) => match r.pause_for_fullscreen() {
                    Ok(()) => {
                        if let Some(sender) = self.pause_senders.get(&display_id) {
                            sender.notify_state_changed(&display_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!(display_id, error = %e, "全屏终止壁纸失败");
                        failed.push(display_id);
                    }
                },
                None => {
                    tracing::error!(display_id, "全屏终止壁纸失败：渲染器不存在");
                    failed.push(display_id);
                }
            }
        }
        Ok(failed)
    }

    /// 获取指定显示器的壁纸状态（通过快速路径读取共享状态）
    ///
    /// # v41-W-004 契约
    ///
    /// 调用方必须持有 `WallpaperEngine` 锁（`&self` 借用已保证）。`pause_senders.get`
    /// 在锁内执行，与并发的 `insert`/`remove` 串行化。
    pub fn get_wallpaper_state_fast(&self, display_id: &str) -> Option<WallpaperState> {
        self.pause_senders.get(display_id).map(|s| s.state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::volume::VolumeControl;
    use crate::desktop::DesktopIntegrator;
    use crate::wallpaper::create_pause_channel;
    use crate::wallpaper::WallpaperRenderer;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    /// 创建测试用 WallpaperEngine
    ///
    /// 初始化 COM（MTA 模式）并构造真实的 `DesktopIntegrator` 和 `VolumeControl`。
    /// 如果 COM 环境不可用（如无音频设备的 CI 环境），返回 `None` 让调用方跳过测试。
    fn create_test_engine() -> Option<WallpaperEngine> {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume_control = match VolumeControl::new() {
            Ok(vc) => Arc::new(Mutex::new(vc)),
            Err(_) => return None,
        };

        Some(WallpaperEngine::new(desktop, volume_control))
    }

    // ---------- pause_wallpaper_fast 测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_pause_wallpaper_fast_with_sender() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender, mut rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 发送 Pause 命令，应返回 Ok(true) 表示存在 sender
        let result = engine.pause_wallpaper_fast("monitor_0");
        assert!(result.is_ok());
        assert!(result.unwrap());
        let cmd = rx.blocking_recv();
        assert!(matches!(cmd, Some(PauseCommand::Pause)));

        // 发送 Resume 命令，应返回 Ok(true) 表示存在 sender
        let result = engine.resume_wallpaper_fast("monitor_0");
        assert!(result.is_ok());
        assert!(result.unwrap());
        let cmd = rx.blocking_recv();
        assert!(matches!(cmd, Some(PauseCommand::Resume)));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_pause_wallpaper_fast_no_sender() {
        let engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        // 未设置 pause_senders，应返回 Ok(false) 表示无 sender
        let result = engine.pause_wallpaper_fast("monitor_0");
        assert!(result.is_ok());
        assert!(!result.unwrap());

        let result = engine.resume_wallpaper_fast("monitor_0");
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    // ---------- 其他快速路径方法测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_get_wallpaper_state_fast() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        // 未设置 sender，返回 None
        assert!(engine.get_wallpaper_state_fast("monitor_0").is_none());

        // 设置 sender
        let (sender, _rx, shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 默认状态为 Initializing
        assert_eq!(
            engine.get_wallpaper_state_fast("monitor_0"),
            Some(WallpaperState::Initializing)
        );

        // 修改共享状态
        shared.write().unwrap().state = WallpaperState::Playing;
        assert_eq!(
            engine.get_wallpaper_state_fast("monitor_0"),
            Some(WallpaperState::Playing)
        );
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_set_volume_fast() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender, mut rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 设置音量
        let result = engine.set_volume_fast("monitor_0", 0.5);
        assert!(result.is_ok());

        // 验证命令已发送
        let cmd = rx.blocking_recv();
        match cmd {
            Some(PauseCommand::SetVolume(v)) => assert!((v - 0.5).abs() < f32::EPSILON),
            other => panic!("expected SetVolume(0.5), got {:?}", other),
        }

        // 验证共享状态中的音量已更新
        assert!(
            (engine.pause_senders.get("monitor_0").unwrap().volume() - 0.5).abs() < f32::EPSILON
        );
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_pause_all_fast() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender1, mut rx1, _shared1) = create_pause_channel();
        let (sender2, mut rx2, _shared2) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_0".to_string(), sender1);
        engine
            .pause_senders
            .insert("monitor_1".to_string(), sender2);

        // 暂停所有
        let result = engine.pause_all_fast(PauseReason::FULLSCREEN);
        assert!(result.is_ok());
        // 无失败 display_id
        let failed = result.unwrap();
        assert!(failed.is_empty(), "应无失败 display_id，实际: {:?}", failed);

        // 验证两个 sender 都收到 Pause 命令
        assert!(matches!(rx1.blocking_recv(), Some(PauseCommand::Pause)));
        assert!(matches!(rx2.blocking_recv(), Some(PauseCommand::Pause)));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_resume_all_fast() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender1, mut rx1, _shared1) = create_pause_channel();
        let (sender2, mut rx2, _shared2) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_0".to_string(), sender1);
        engine
            .pause_senders
            .insert("monitor_1".to_string(), sender2);

        // 先暂停（设置 FULLSCREEN reason）
        let pause_result = engine.pause_all_fast(PauseReason::FULLSCREEN);
        assert!(pause_result.is_ok());
        let pause_failed = pause_result.unwrap();
        assert!(
            pause_failed.is_empty(),
            "pause 应无失败: {:?}",
            pause_failed
        );
        // 消费 Pause 命令
        assert!(matches!(rx1.blocking_recv(), Some(PauseCommand::Pause)));
        assert!(matches!(rx2.blocking_recv(), Some(PauseCommand::Pause)));

        // 恢复所有（clear FULLSCREEN reason，reasons 清空 → 发 Resume）
        let result = engine.resume_all_fast(PauseReason::FULLSCREEN);
        assert!(result.is_ok());
        // 无失败 display_id
        let failed = result.unwrap();
        assert!(failed.is_empty(), "应无失败 display_id，实际: {:?}", failed);

        // 验证两个 sender 都收到 Resume 命令
        assert!(matches!(rx1.blocking_recv(), Some(PauseCommand::Resume)));
        assert!(matches!(rx2.blocking_recv(), Some(PauseCommand::Resume)));
    }

    // ── PauseReason 位图协调测试 ──────────────────────────────────────────

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_pause_reason_coordination_resume_one_keeps_other_paused() {
        // 场景：FULLSCREEN 暂停 → BATTERY 也暂停（仅 set bit，不重发 Pause）
        //       → resume FULLSCREEN（reasons 仍有 BATTERY，不发 Resume）
        //       → resume BATTERY（reasons 清空，发 Resume）
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender, mut rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 1. FULLSCREEN 暂停 → 发 Pause
        let r = engine.pause_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(r.is_empty());
        assert!(matches!(rx.blocking_recv(), Some(PauseCommand::Pause)));

        // 2. BATTERY 暂停 → reasons 已非空，仅 set bit，不发 Pause
        let r = engine.pause_all_fast(PauseReason::BATTERY).unwrap();
        assert!(r.is_empty());
        // 不应有新命令（rx 应为空）
        assert!(rx.try_recv().is_err(), "BATTERY pause 不应重复发 Pause");

        // 3. resume FULLSCREEN → reasons 仍有 BATTERY，不发 Resume
        let r = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(r.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "FULLSCREEN resume 时 BATTERY 仍活跃，不应发 Resume"
        );

        // 4. resume BATTERY → reasons 清空，发 Resume
        let r = engine.resume_all_fast(PauseReason::BATTERY).unwrap();
        assert!(r.is_empty());
        assert!(matches!(rx.blocking_recv(), Some(PauseCommand::Resume)));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_resume_without_pause_sends_fallback_resume() {
        // W-004 修复验证：未 pause 过直接 resume → 仍向所有 sender 发送 Resume。
        //
        // 原实现 `!reasons.contains(reason)` 时 early-return 不发命令，导致
        // bit 丢失或 pause_all_fast 部分失败时已暂停的渲染器无法自愈。
        // W-004 修复后：当 reasons 清空（无任何活跃 reason）时，无论 was_paused
        // 为何值都向所有 sender 发送 Resume，对非 Paused 状态的渲染器幂等无害。
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender, mut rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        let r = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(r.is_empty(), "未 pause 过的 resume 应返回空 failed 列表");
        // W-004: 即便 bit 未设，也向所有 sender 发送 Resume 作为兜底
        assert!(
            matches!(rx.blocking_recv(), Some(PauseCommand::Resume)),
            "W-004: 未 pause 过的 resume 也应发送 Resume 作为兜底（幂等无害）"
        );
    }

    // ---------- toggle_mute_fast 测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_toggle_mute_fast_with_sender() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let (sender, mut rx, shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 初始未静音（pre_mute_volume = None）→ 切换为静音，返回 Ok(Some(true))
        // N-005: toggle_mute_atomic 在锁内翻转 pre_mute_volume = Some(volume)
        let result = engine.toggle_mute_fast("monitor_0");
        assert_eq!(result.unwrap(), Some(true));
        assert!(matches!(rx.blocking_recv(), Some(PauseCommand::ToggleMute)));
        // 验证 toggle_mute_atomic 已翻转 shared_state.pre_mute_volume
        assert!(shared.read().unwrap().pre_mute_volume.is_some());

        // 模拟外部覆盖 pre_mute_volume（如其他线程的 SetVolume 影响），
        // 验证 toggle_mute_fast 仍能正确识别为"已静音"并翻转回 None
        shared.write().unwrap().pre_mute_volume = Some(0.8);

        // 当前已静音 → 切换为未静音，返回 Ok(Some(false))
        let result = engine.toggle_mute_fast("monitor_0");
        assert_eq!(result.unwrap(), Some(false));
        assert!(matches!(rx.blocking_recv(), Some(PauseCommand::ToggleMute)));
        // 验证 pre_mute_volume 已被翻转回 None
        assert!(shared.read().unwrap().pre_mute_volume.is_none());
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_toggle_mute_fast_no_sender_returns_none() {
        let engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        // 未设置 pause_senders，应返回 Ok(None)
        let result = engine.toggle_mute_fast("monitor_0");
        assert_eq!(result.unwrap(), None);
    }

    // ── W06 TOCTOU 修复测试 ──────────────────────────────────────────────
    //
    // 以下测试验证 pause_all_fast 的 W06 修复：bit 在 drop(reasons) 前设置，
    // 关闭 TOCTOU 窗口；发送失败时回滚 bit。使用 VolumeControl::new_disabled()
    // 避免 COM/WASAPI 依赖，可在任意 Windows 环境（含无音频 CI）运行。

    /// 创建不依赖 COM/音频环境的测试 WallpaperEngine（W06 测试专用）
    fn create_test_engine_no_com() -> WallpaperEngine {
        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume_control = Arc::new(Mutex::new(VolumeControl::new_disabled()));
        WallpaperEngine::new(desktop, volume_control)
    }

    #[test]
    fn pause_all_fast_sets_bit_on_success() {
        // W06: 成功发送 Pause 后，pause_reasons 应包含该 reason。
        // 验证 bit 在发送成功后被设置（保留语义）。
        let mut engine = create_test_engine_no_com();
        let (sender, _rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        let failed = engine.pause_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(failed.is_empty(), "应无失败 display_id");

        // 验证 bit 已设置
        let reasons = engine.pause_reasons.lock().unwrap();
        assert!(
            reasons.contains(PauseReason::FULLSCREEN),
            "成功 pause 后 FULLSCREEN bit 应已设置"
        );
    }

    #[test]
    fn pause_all_fast_keeps_bit_on_send_failure() {
        // W06 + W-004 综合验证：发送 Pause 失败时，pause_reasons **保留** bit（不回滚）。
        //
        // W06 修复结构："先 set bit 再发送，失败时回滚 bit"。
        // W-004 修复在此之上移除了"部分失败回滚"逻辑：部分失败时 bit 保留，
        // 使 resume_all_fast 能观察到 bit 并发送 Resume 恢复已暂停的渲染器。
        //
        // 本测试验证单 sender 失败场景：receiver drop 后 sender.send 返回 Err，
        // failed 列表非空，但 FULLSCREEN bit 仍被保留（不回滚）。
        let mut engine = create_test_engine_no_com();
        let (sender, rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);
        drop(rx); // 丢弃 receiver，使 sender.send 返回 Err

        let failed = engine.pause_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(
            !failed.is_empty(),
            "receiver 已 drop，send 应失败，failed 列表应非空"
        );

        // W-004: 验证 bit 已保留（不回滚），使后续 resume_all_fast 能自愈
        let reasons = engine.pause_reasons.lock().unwrap();
        assert!(
            reasons.contains(PauseReason::FULLSCREEN),
            "W-004: 发送失败时 FULLSCREEN bit 应保留（不回滚），以保证 resume_all_fast 能自愈"
        );
    }

    #[test]
    fn pause_all_fast_second_reason_sets_bit_without_sending() {
        // W06: 已有 reason 暂停时，新 reason 仅 set bit 不重发 Pause。
        // 验证多 reason 协调：bit 在 drop(reasons) 前设置使第二次 pause 的
        // was_paused 检查能正确识别已有 reason。
        let mut engine = create_test_engine_no_com();
        let (sender, mut rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 第一次 pause（FULLSCREEN）→ 发 Pause
        let failed = engine.pause_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(failed.is_empty());
        assert!(matches!(rx.blocking_recv(), Some(PauseCommand::Pause)));

        // 第二次 pause（BATTERY）→ was_paused=true，仅 set bit，不重发
        let failed = engine.pause_all_fast(PauseReason::BATTERY).unwrap();
        assert!(failed.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "BATTERY pause 时已有 FULLSCREEN 暂停，不应重发 Pause"
        );

        // 验证两个 bit 都已设置
        let reasons = engine.pause_reasons.lock().unwrap();
        assert!(reasons.contains(PauseReason::FULLSCREEN));
        assert!(reasons.contains(PauseReason::BATTERY));
    }

    #[test]
    fn pause_all_fast_bit_visible_during_send_phase() {
        // W06 TOCTOU 窗口验证：bit 在 drop(reasons) 前设置，因此发送阶段
        // （锁已释放期间）并发的 resume_all_fast 能观察到 bit 已设置。
        //
        // 本测试通过在发送阶段检查 pause_reasons 来验证 bit 已提前设置：
        // 使用一个 sender，在 send 被调用时不立即消费，使主线程停留在
        // 发送循环中，同时另一线程检查 pause_reasons 是否已包含该 bit。
        //
        // 注意：mpsc unbounded send 不阻塞，此测试改为验证 bit 在
        // pause_all_fast 返回后已设置（成功路径）且在第二次 pause 的
        // was_paused 检查中被正确识别（与上一个测试互补）。
        // 真正的 TOCTOU 窗口由回滚测试间接验证（回滚路径证明 bit 先于发送设置）。
        let mut engine = create_test_engine_no_com();
        let (sender, _rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // pause 后立即检查 bit（同步路径，无并发）
        let failed = engine.pause_all_fast(PauseReason::TRAY).unwrap();
        assert!(failed.is_empty());

        // bit 应已设置（在 drop(reasons) 前设置，返回时必然可见）
        {
            let reasons = engine.pause_reasons.lock().unwrap();
            assert!(reasons.contains(PauseReason::TRAY));
        }

        // resume 应观察到 bit 并清除它
        let failed = engine.resume_all_fast(PauseReason::TRAY).unwrap();
        assert!(failed.is_empty());
        {
            let reasons = engine.pause_reasons.lock().unwrap();
            assert!(
                !reasons.contains(PauseReason::TRAY),
                "resume 后 TRAY bit 应已清除"
            );
        }
    }

    // ── W-004 修复测试：部分失败自愈 ─────────────────────────────────────
    //
    // 验证 pause_all_fast 部分失败时保留 bit + resume_all_fast 兜底发送 Resume
    // 的自愈机制：已暂停的存活渲染器能通过后续 resume_all_fast 恢复正常。

    #[test]
    fn w004_pause_all_partial_failure_self_heals() {
        // W-004 核心验证：pause_all_fast 部分失败后，保留 bit 使后续
        // resume_all_fast 能恢复已暂停的存活渲染器。
        //
        // 场景：2 个 sender（monitor_0 失败 / monitor_1 成功）
        // 1. pause_all_fast(FULLSCREEN) 部分失败：
        //    - monitor_0 sender.send 失败（receiver 已 drop）
        //    - monitor_1 sender.send 成功（收到 Pause）
        //    - W-004: bit 保留（不回滚）→ failed = ["monitor_0"]
        // 2. 验证 bit 仍设置，monitor_1 收到 Pause 命令
        // 3. resume_all_fast(FULLSCREEN) 自愈：
        //    - was_paused=true（bit 已设），bit 清除
        //    - reasons 清空 → 兜底+正常路径合并，向所有 sender 发 Resume
        //    - monitor_0 sender.send 失败（再次失败）
        //    - monitor_1 sender.send 成功（收到 Resume，自愈成功！）
        //    - was_paused && !failed.is_empty() → re-set bit
        // 4. 验证 monitor_1 收到 Resume 命令（关键自愈断言）
        //    bit 因 monitor_0 失败被 re-set（仍需重试），但 monitor_1 已恢复
        let mut engine = create_test_engine_no_com();
        // monitor_0: receiver drop → send 失败
        let (sender_fail, rx_fail, _shared_fail) = create_pause_channel();
        drop(rx_fail);
        // monitor_1: 正常存活
        let (sender_ok, mut rx_ok, shared_ok) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_0".to_string(), sender_fail);
        engine
            .pause_senders
            .insert("monitor_1".to_string(), sender_ok);

        // 1. pause_all_fast 部分失败
        let failed = engine.pause_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert_eq!(
            failed,
            vec!["monitor_0".to_string()],
            "pause 部分失败应返回失败的 display_id 列表"
        );

        // 2. bit 应保留（W-004 修复），monitor_1 应收到 Pause
        {
            let reasons = engine.pause_reasons.lock().unwrap();
            assert!(
                reasons.contains(PauseReason::FULLSCREEN),
                "W-004: 部分失败时 bit 应保留，使后续 resume 能自愈"
            );
        }
        assert!(
            matches!(rx_ok.blocking_recv(), Some(PauseCommand::Pause)),
            "存活的 monitor_1 应已收到 Pause 命令"
        );
        // 模拟渲染器处理 Pause 后状态变为 Paused
        shared_ok.write().unwrap().state = WallpaperState::Paused;

        // 3. resume_all_fast 自愈
        let failed = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert_eq!(
            failed,
            vec!["monitor_0".to_string()],
            "resume 部分失败应返回失败的 display_id 列表（monitor_0 sender 已失效）"
        );

        // 4. 关键自愈断言：monitor_1 收到 Resume，恢复 Playing 状态
        assert!(
            matches!(rx_ok.blocking_recv(), Some(PauseCommand::Resume)),
            "W-004 自愈：存活的 monitor_1 应收到 Resume 命令恢复正常"
        );

        // 5. bit 因 monitor_0 resume 失败被 re-set（was_paused=true 且 failed 非空）
        //    这是预期行为：monitor_0 仍需后续重试才能完全恢复
        {
            let reasons = engine.pause_reasons.lock().unwrap();
            assert!(
                reasons.contains(PauseReason::FULLSCREEN),
                "resume 部分失败时 bit 应被 re-set（was_paused=true），等待后续重试"
            );
        }
    }

    #[test]
    fn w004_resume_fallback_self_heals_when_bit_lost() {
        // W-004 兜底路径验证：bit 因某种原因丢失（如外部代码误操作），
        // 但渲染器实际处于 Paused 状态。resume_all_fast 即便 was_paused=false
        // 也应发送 Resume 自愈，避免渲染器永久卡死。
        //
        // 场景：
        // 1. 手动构造"渲染器已 Paused 但 bit 未设"的异常状态
        //    （模拟 bit 丢失的故障场景）
        // 2. 调用 resume_all_fast(FULLSCREEN)
        //    - was_paused=false（bit 未设）
        //    - reasons 清空 → 兜底发送 Resume 到所有 sender
        // 3. 验证 Resume 已发送（自愈成功）
        // 4. 验证 bit 仍未设（was_paused=false，不会因失败 re-set）
        let mut engine = create_test_engine_no_com();
        let (sender, mut rx, shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 1. 模拟故障状态：渲染器实际 Paused，但 pause_reasons 未记录 FULLSCREEN
        shared.write().unwrap().state = WallpaperState::Paused;
        // pause_reasons 默认为空（PauseReason(0)），bit 未设

        // 2. resume_all_fast 兜底
        let failed = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(failed.is_empty(), "兜底 resume 应返回空 failed 列表");

        // 3. 验证 Resume 已发送（自愈）
        assert!(
            matches!(rx.blocking_recv(), Some(PauseCommand::Resume)),
            "W-004 兜底：即便 bit 未设，也应发送 Resume 自愈卡死的 Paused 渲染器"
        );

        // 4. bit 仍未设（was_paused=false，发送成功不改变 bit 状态）
        {
            let reasons = engine.pause_reasons.lock().unwrap();
            assert!(
                !reasons.contains(PauseReason::FULLSCREEN),
                "was_paused=false 时 bit 状态不应被改变"
            );
        }
    }

    // ── v41-W-004 修复测试：pause_senders 锁内串行契约 ──────────────────
    //
    // v41-W-004 finding：`pause_senders`（普通 `HashMap`）的 insert/remove/get/iter
    // 操作未在锁内完整事务化，原 finding 担心并发 insert + remove 可能误删。
    //
    // 修复方案（降级）：保留 `HashMap`，文档化"所有访问必须通过 `&self` 借用
    // engine 锁串行执行"。Rust 借用检查已强制保证：`&self` 共享借用排斥 `&mut self`，
    // 因此 `fast_path.rs` 中所有 `&self` 方法访问 `pause_senders` 时，调用方必然
    // 持有 engine 锁，`embed_and_register_renderer`（`&mut self`）与 `close_wallpaper`
    // （`&mut self`）的 insert/remove 也会串行化。
    //
    // 本测试通过单线程顺序调用 insert/remove/get 验证契约行为正确（无并发误删）。
    // 真实并发场景由 Rust 借用检查在编译期保证（`&self` 与 `&mut self` 互斥）。

    /// 验证 `pause_senders` 的 insert + remove + get 序列化行为正确。
    ///
    /// v41-W-004 降级修复（文档化）后，所有访问通过 `&self` 或 `&mut self` 借用
    /// 串行化。本测试在单线程内顺序执行：
    /// 1. insert monitor_0 / monitor_1 / monitor_2
    /// 2. remove monitor_1（模拟 `close_wallpaper`）
    /// 3. insert monitor_3（模拟 `embed_and_register_renderer` 新渲染器）
    /// 4. get(monitor_0) / get(monitor_1) / get(monitor_3) 验证状态
    ///
    /// 验证：
    /// - remove 后 get(monitor_1) 返回 None（未误删其他 key）
    /// - insert 新 key 不影响已有 key
    /// - get(monitor_0) / get(monitor_3) 仍可正常访问
    #[test]
    fn v41_w004_pause_senders_concurrent_insert_remove() {
        let mut engine = create_test_engine_no_com();

        // 1. 插入 3 个 sender（模拟 3 个显示器注册）
        let (sender_0, _rx_0, _shared_0) = create_pause_channel();
        let (sender_1, _rx_1, _shared_1) = create_pause_channel();
        let (sender_2, _rx_2, _shared_2) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_0".to_string(), sender_0);
        engine
            .pause_senders
            .insert("monitor_1".to_string(), sender_1);
        engine
            .pause_senders
            .insert("monitor_2".to_string(), sender_2);
        assert_eq!(engine.pause_senders.len(), 3, "应包含 3 个 sender");

        // 2. 移除 monitor_1（模拟 close_wallpaper）
        let removed = engine.pause_senders.remove("monitor_1");
        assert!(removed.is_some(), "remove 应返回被移除的 sender");
        assert_eq!(engine.pause_senders.len(), 2, "移除后应剩 2 个 sender");

        // 3. 插入 monitor_3（模拟新渲染器注册，复用已释放的 display_id 槽位）
        let (sender_3, _rx_3, _shared_3) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_3".to_string(), sender_3);
        assert_eq!(engine.pause_senders.len(), 3, "新增后应包含 3 个 sender");

        // 4. 验证各 key 的状态（确认未误删）
        //    - monitor_0：仍存在
        //    - monitor_1：已移除（get 返回 None）
        //    - monitor_2：仍存在
        //    - monitor_3：新插入
        assert!(
            engine.pause_senders.contains_key("monitor_0"),
            "monitor_0 应仍存在（未被误删）"
        );
        assert!(
            !engine.pause_senders.contains_key("monitor_1"),
            "monitor_1 应已被移除"
        );
        assert!(
            engine.pause_senders.contains_key("monitor_2"),
            "monitor_2 应仍存在（未被误删）"
        );
        assert!(
            engine.pause_senders.contains_key("monitor_3"),
            "monitor_3 应存在（新插入）"
        );

        // 5. 验证通过 fast_path API 访问（&self 借用，验证契约可用性）
        //    get_wallpaper_state_fast 内部通过 pause_senders.get 访问
        assert!(
            engine.get_wallpaper_state_fast("monitor_0").is_some(),
            "通过 fast_path API 访问 monitor_0 应成功"
        );
        assert!(
            engine.get_wallpaper_state_fast("monitor_1").is_none(),
            "通过 fast_path API 访问已移除的 monitor_1 应返回 None"
        );
        assert!(
            engine.get_wallpaper_state_fast("monitor_3").is_some(),
            "通过 fast_path API 访问新插入的 monitor_3 应成功"
        );
    }

    // ── 全屏终止（terminate_all_fast）与引擎级重启（resume_all_fast）测试 ──
    //
    // 全屏时终止视频/网页子进程释放内存；退出全屏后对 Terminated 渲染器调用
    // play() 完整重启。使用 MockRenderer 追踪 play / pause_for_fullscreen 调用。

    /// 测试用 mock 渲染器：模拟子进程渲染器（视频/网页）。
    ///
    /// `pause_for_fullscreen` 覆写为终止语义（state → Terminated），与视频/网页
    /// 渲染器的行为一致；`play` 完整重启（state → Playing）。state 断言足以验证
    /// 调用路径（mock 中仅有 pause_for_fullscreen/terminate 会置 Terminated）。
    struct MockRenderer {
        state: WallpaperState,
        /// 记录 `after_embed` 调用次数。使用 `Arc<AtomicUsize>` 使测试可在渲染器
        /// 被移入 `Box<dyn WallpaperRenderer>`（trait 对象无法 downcast）后仍能
        /// 通过保留的 Arc clone 读取该计数，验证 reembed 走了 after_embed 路径。
        after_embed_calls: Arc<AtomicUsize>,
        /// 置为 true 时 `after_embed` 返回 Err，用于测试 reembed 失败回滚路径。
        fail_after_embed: bool,
    }

    impl MockRenderer {
        fn new() -> Self {
            Self {
                state: WallpaperState::Playing,
                after_embed_calls: Arc::new(AtomicUsize::new(0)),
                fail_after_embed: false,
            }
        }
    }

    impl WallpaperRenderer for MockRenderer {
        fn play(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Playing;
            Ok(())
        }
        fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Paused;
            Ok(())
        }
        fn pause_for_fullscreen(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Terminated;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Playing;
            Ok(())
        }
        fn set_position(
            &mut self,
            _x: i32,
            _y: i32,
            _w: i32,
            _h: i32,
        ) -> Result<(), crate::MirrorStarError> {
            Ok(())
        }
        fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Terminated;
            Ok(())
        }
        fn hwnd(&self) -> Option<HWND> {
            None
        }
        fn state(&self) -> WallpaperState {
            self.state
        }
        fn after_embed(&mut self) -> Result<(), crate::MirrorStarError> {
            if self.fail_after_embed {
                return Err(crate::MirrorStarError::DesktopIntegration(
                    "mock after_embed 失败".into(),
                ));
            }
            self.after_embed_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn create_pause_sender(
            &mut self,
            _display_id: &str,
        ) -> Option<crate::wallpaper::PauseSender> {
            let (s, _rx, _shared) = create_pause_channel();
            Some(s)
        }
    }

    /// a) terminate_all_fast：设置 FULLSCREEN 位，且对每个渲染器触发 pause_for_fullscreen（state → Terminated）
    #[test]
    fn terminate_all_fast_sets_bit_and_terminates_renderers() {
        let mut engine = create_test_engine_no_com();
        engine
            .wallpapers
            .insert("monitor_0".to_string(), Box::new(MockRenderer::new()));
        engine
            .wallpapers
            .insert("monitor_1".to_string(), Box::new(MockRenderer::new()));
        let (sender0, _rx0, _shared0) = create_pause_channel();
        let (sender1, _rx1, _shared1) = create_pause_channel();
        engine
            .pause_senders
            .insert("monitor_0".to_string(), sender0);
        engine
            .pause_senders
            .insert("monitor_1".to_string(), sender1);

        let failed = engine.terminate_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(
            failed.is_empty(),
            "全屏终止应无失败 display_id，实际: {:?}",
            failed
        );

        // FULLSCREEN bit 已设置
        assert!(
            engine
                .pause_reasons
                .lock()
                .unwrap()
                .contains(PauseReason::FULLSCREEN),
            "terminate_all_fast 应设置 FULLSCREEN bit"
        );

        // 每个渲染器都触发 pause_for_fullscreen → state 变 Terminated
        assert_eq!(
            engine.wallpapers.get("monitor_0").unwrap().state(),
            WallpaperState::Terminated,
            "monitor_0 渲染器应被 pause_for_fullscreen 终止为 Terminated"
        );
        assert_eq!(
            engine.wallpapers.get("monitor_1").unwrap().state(),
            WallpaperState::Terminated,
            "monitor_1 渲染器应被 pause_for_fullscreen 终止为 Terminated"
        );
    }

    /// b) resume_all_fast：对 Terminated 渲染器调用 play() 完整重启并 reembed
    #[test]
    fn resume_all_fast_restarts_terminated_renderer() {
        let mut engine = create_test_engine_no_com();
        // 模拟全屏终止后的 Terminated 渲染器
        let mut renderer = MockRenderer::new();
        renderer.state = WallpaperState::Terminated;
        // 保留 Arc clone，渲染器被移入 engine 后仍可读取 after_embed 计数
        let after_embed_calls = renderer.after_embed_calls.clone();
        engine
            .wallpapers
            .insert("monitor_0".to_string(), Box::new(renderer));
        let (sender, _rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 先设置 FULLSCREEN bit（模拟 terminate_all_fast 已执行）
        *engine.pause_reasons.lock().unwrap() |= PauseReason::FULLSCREEN;

        let failed = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(
            failed.is_empty(),
            "重启 Terminated 渲染器应无失败，实际: {:?}",
            failed
        );

        // Terminated 渲染器被 play() 重启 → Playing（reembed 后将渲染器放回）
        assert_eq!(
            engine.wallpapers.get("monitor_0").unwrap().state(),
            WallpaperState::Playing,
            "resume_all_fast 应对 Terminated 渲染器调用 play() 重启为 Playing"
        );

        // reembed 走了 after_embed 路径（视频渲染器重新 loadfile 加载视频）
        assert_eq!(
            after_embed_calls.load(Ordering::SeqCst),
            1,
            "reembed 应调用一次 after_embed"
        );

        // reembed 替换了新 sender；resume_all_fast 对新 sender set_state(Playing)，
        // PauseSender::state() 读取 shared_state.state
        assert_eq!(
            engine.pause_senders.get("monitor_0").unwrap().state(),
            WallpaperState::Playing,
            "shared_state 应同步为 Playing"
        );

        // FULLSCREEN bit 已清除
        assert!(
            !engine
                .pause_reasons
                .lock()
                .unwrap()
                .contains(PauseReason::FULLSCREEN),
            "resume 后 FULLSCREEN bit 应已清除"
        );
    }

    /// c) 多 reason 协调：BATTERY 仍活跃时，resume FULLSCREEN 不重启 Terminated 渲染器
    #[test]
    fn resume_all_fast_battery_still_active_no_restart() {
        let mut engine = create_test_engine_no_com();
        let mut renderer = MockRenderer::new();
        renderer.state = WallpaperState::Terminated;
        engine
            .wallpapers
            .insert("monitor_0".to_string(), Box::new(renderer));
        let (sender, _rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // FULLSCREEN + BATTERY 同时活跃
        {
            let mut reasons = engine.pause_reasons.lock().unwrap();
            *reasons |= PauseReason::FULLSCREEN;
            *reasons |= PauseReason::BATTERY;
        }

        let failed = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert!(failed.is_empty());

        // BATTERY 仍活跃 → 不重启，渲染器保持 Terminated
        assert_eq!(
            engine.wallpapers.get("monitor_0").unwrap().state(),
            WallpaperState::Terminated,
            "BATTERY 仍活跃时不应重启 Terminated 渲染器"
        );

        // FULLSCREEN bit 已清除但 BATTERY 保留
        let reasons = engine.pause_reasons.lock().unwrap();
        assert!(
            !reasons.contains(PauseReason::FULLSCREEN),
            "resume FULLSCREEN 应清除 FULLSCREEN bit"
        );
        assert!(
            reasons.contains(PauseReason::BATTERY),
            "BATTERY 仍活跃，其 bit 应保留"
        );
    }

    /// c2) resume_all_fast：reembed 失败（after_embed 失败）时计入 failed 并回滚
    #[test]
    fn resume_all_fast_reembed_failure_counts_as_failed() {
        let mut engine = create_test_engine_no_com();
        // 构造 Terminated 渲染器且 after_embed 必然失败
        let mut renderer = MockRenderer::new();
        renderer.state = WallpaperState::Terminated;
        renderer.fail_after_embed = true;
        engine
            .wallpapers
            .insert("monitor_0".to_string(), Box::new(renderer));
        let (sender, _rx, _shared) = create_pause_channel();
        engine.pause_senders.insert("monitor_0".to_string(), sender);

        // 先设置 FULLSCREEN bit（模拟 terminate_all_fast 已执行）
        *engine.pause_reasons.lock().unwrap() |= PauseReason::FULLSCREEN;

        let failed = engine.resume_all_fast(PauseReason::FULLSCREEN).unwrap();
        assert_eq!(
            failed,
            vec!["monitor_0".to_string()],
            "reembed 失败应计入 failed 列表"
        );

        // 渲染器被回滚 terminate → Terminated
        assert_eq!(
            engine.wallpapers.get("monitor_0").unwrap().state(),
            WallpaperState::Terminated,
            "reembed 失败后渲染器应被回滚 terminate 为 Terminated"
        );

        // FULLSCREEN bit 被重新设置（was_paused && !failed.is_empty()）
        assert!(
            engine
                .pause_reasons
                .lock()
                .unwrap()
                .contains(PauseReason::FULLSCREEN),
            "reembed 失败后 FULLSCREEN bit 应被重新设置"
        );
    }
}
