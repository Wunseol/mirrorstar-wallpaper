use crate::wallpaper::{GifMemoryStrategy, ScalingMode, DEFAULT_BALANCED_KEEP_FRAMES};
use crate::MirrorStarError;

use super::gif_decode::{
    decode_gif_streaming, decode_single_frame_with_cursor, GifFrame, STREAMING_WINDOW_HALF,
};

/// GIF 渲染数据
pub(crate) struct GifRenderData {
    /// 所有帧
    pub(crate) frames: Vec<GifFrame>,
    /// 当前帧索引
    pub(crate) current_frame: usize,
    /// 缩放模式
    pub(crate) scaling_mode: ScalingMode,
    /// 播放速度倍率
    pub(crate) speed: f32,
    /// 是否暂停
    pub(crate) paused: bool,
    /// GIF 文件路径（用于暂停后恢复时重新解码）
    pub(crate) image_path: String,
    /// 帧数据是否已加载（暂停时释放帧数据后为 false）
    pub(crate) frames_loaded: bool,
    /// 暂停时保存的帧索引（用于恢复时定位）
    pub(crate) saved_frame_index: Option<usize>,
    /// 内存管理策略
    pub(crate) memory_strategy: GifMemoryStrategy,
    /// 平衡模式下保留的帧数
    pub(crate) balanced_keep_frames: usize,
    /// GIF 帧像素内存预算上限（MB）（v41-W-012: 从配置传入）
    pub(crate) max_memory_mb: usize,
    /// v5.0 W-PERF-001: 上次 SetTimer 设置的延迟（ms），用于跳过未变化的 SetTimer 调用。
    /// 0 表示尚未设置过 timer（首帧初始化前的状态）。
    pub(crate) last_timer_delay: u32,
    /// v8-C: GIF 原始总帧数。
    ///
    /// 由 `handle_frames_loaded` 在后台全量解码完成后设置，用于诊断与
    /// WM_TIMER 循环判断。注意：Balanced/Aggressive 策略 `handle_pause`
    /// 可能 drain/清空 `frames` 使其长度小于 `total_frames`，因此
    /// WM_TIMER 的循环回绕仍以 `frames.len()` 为准（保持现有行为），
    /// `total_frames` 仅作元数据记录。
    pub(crate) total_frames: usize,
    /// #2: sync 兜底解码的持久化帧迭代器（主线程局部，非 Send）。
    ///
    /// `reload_current_frame_pixels` 复用此迭代器，将 O(target) 降为 O(1)
    /// （前向）/ O(target)（回绕 / 首次）。`None` 表示尚未打开或已被重置
    /// （回绕 / resume 后）。迭代器持有独立的 `BufReader<File>` 句柄，
    /// 与 #1 预取 worker 的迭代器互不干扰。
    pub(crate) sync_frames_iter: Option<image::Frames<'static>>,
    /// #2: sync 兜底游标当前位置（`sync_frames_iter` 下一帧索引）。
    pub(crate) sync_cursor: usize,
}

impl GifRenderData {
    /// 估算帧数据总内存占用（字节）
    pub(crate) fn estimate_memory_bytes(&self) -> usize {
        self.frames.iter().map(|f| f.pixels.len()).sum()
    }

    /// 估算单帧平均内存占用（字节）
    fn estimate_avg_frame_bytes(&self) -> usize {
        if self.frames.is_empty() {
            return 0;
        }
        self.estimate_memory_bytes() / self.frames.len()
    }

    /// 获取系统可用物理内存（字节）
    fn get_available_memory_bytes() -> u64 {
        // 使用 Windows API 获取准确的可用内存
        use windows::Win32::System::SystemInformation::GlobalMemoryStatusEx;
        use windows::Win32::System::SystemInformation::MEMORYSTATUSEX;

        let mut mem_info = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        unsafe {
            if let Err(e) = GlobalMemoryStatusEx(&mut mem_info) {
                tracing::warn!(error = %e, "GlobalMemoryStatusEx 失败：将以 0 可用内存降级为保守策略");
            }
        }
        mem_info.ullAvailPhys
    }

    /// 判断是否应该保留所有帧（自适应模式）
    fn should_keep_all_frames(&self) -> bool {
        let memory_bytes = self.estimate_memory_bytes();
        let available_bytes = Self::get_available_memory_bytes();

        // 如果帧数据占用小于系统可用内存的 5%，则保留所有帧
        (memory_bytes as u64) * 20 < available_bytes
    }

    /// 计算自适应模式下的最优保留帧数
    fn calculate_adaptive_keep_frames(&self) -> usize {
        let available_bytes = Self::get_available_memory_bytes();
        let frame_bytes = self.estimate_avg_frame_bytes().max(1);

        // 目标是使用不超过可用内存 2% 的空间存储帧数据
        let target_bytes = available_bytes / 50;
        let optimal = (target_bytes as usize / frame_bytes).max(5);

        optimal
            .min(self.frames.len())
            .max(DEFAULT_BALANCED_KEEP_FRAMES)
    }

    /// v8-C: 应用流式帧缓存窗口，清空当前帧 ± [`STREAMING_WINDOW_HALF`] 之外
    /// 所有帧的 `pixels` 字段（保留 width/height/delay_ms 元数据）。
    ///
    /// 调用时机：
    /// - `handle_frames_loaded`：后台全量解码完成后，立即清空窗口外帧像素
    /// - `handle_resume`（Aggressive/Adaptive）：重新全量解码后清空窗口外帧像素
    /// - `WM_TIMER`：推进到空像素帧重新解码后，清空远离当前帧的帧像素
    ///
    /// 内存效果：活跃播放时仅 2*STREAMING_WINDOW_HALF+1 = 3 帧保留像素数据，
    /// 其余帧 pixels 为空 Vec（仅占 24 字节元数据）。
    ///
    /// 注意：本方法仅清空像素，不修改 `frames` 长度与 `current_frame`，
    /// 因此不影响 Balanced/Aggressive `handle_pause` 的 drain 逻辑（drain
    /// 不依赖像素内容）。
    pub(crate) fn apply_streaming_window(&mut self) {
        let total = self.frames.len();
        if total == 0 {
            return;
        }
        let current = self.current_frame.min(total.saturating_sub(1));
        let half = STREAMING_WINDOW_HALF;
        let window_start = current.saturating_sub(half);
        let window_end = (current + half + 1).min(total);
        let mut cleared = 0usize;
        for (i, frame) in self.frames.iter_mut().enumerate() {
            if (i < window_start || i >= window_end) && !frame.pixels.is_empty() {
                frame.pixels.clear();
                frame.pixels.shrink_to_fit();
                cleared += 1;
            }
        }
        if cleared > 0 {
            let memory_mb = self.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
            tracing::debug!(
                total_frames = total,
                current,
                window_start,
                window_end,
                cleared_frames = cleared,
                memory_mb = format!("{:.2}", memory_mb),
                "v8-C: 已应用流式帧缓存窗口"
            );
        }
    }

    /// v8-C: 重新解码当前帧的像素数据（当 pixels 被流式窗口清空后按需恢复）。
    ///
    /// #2: 复用主线程持久化游标（`sync_frames_iter` + `sync_cursor`），将
    /// 旧 O(current)（每次从 0 解码）降为 O(1)（前向，cursor ≈ current）/
    /// O(current)（回绕 / 首次）。失败时记录警告并保留空像素（渲染时会跳过）。
    ///
    /// 返回 `true` 表示成功恢复像素，`false` 表示失败或无需恢复。
    pub(crate) fn reload_current_frame_pixels(&mut self) -> bool {
        let current = self.current_frame;
        if current >= self.frames.len() {
            return false;
        }
        if !self.frames[current].pixels.is_empty() {
            return true; // 已有像素，无需重新解码
        }
        let (screen_w, screen_h) = super::get_screen_size();
        // #4: 用 split borrow 直接传 &self.image_path，避免每次调用 clone()
        // 分配 String。`&self.image_path`（不可变借用 image_path 字段）与
        // `&mut self.sync_frames_iter` / `&mut self.sync_cursor`（可变借用其他
        // 字段）互不冲突，借用检查器允许。`&String` 经 deref coercion 转 `&str`。
        match decode_single_frame_with_cursor(
            &self.image_path,
            screen_w,
            screen_h,
            &mut self.sync_frames_iter,
            &mut self.sync_cursor,
            current,
        ) {
            Some(frame) => {
                let f = &mut self.frames[current];
                f.pixels = frame.pixels;
                f.width = frame.width;
                f.height = frame.height;
                f.delay_ms = frame.delay_ms;
                tracing::debug!(
                    frame = current,
                    cursor = self.sync_cursor,
                    "#2: 帧像素已按需恢复（cursor）"
                );
                true
            }
            None => {
                tracing::warn!(
                    frame = current,
                    "#2: 按需重新解码帧失败，保留空像素"
                );
                false
            }
        }
    }

    /// 根据策略处理暂停时的内存管理
    pub(crate) fn handle_pause(&mut self) {
        self.paused = true;

        match self.memory_strategy {
            GifMemoryStrategy::Aggressive => {
                // 激进模式：释放所有帧，仅保留当前帧
                if self.frames_loaded && self.frames.len() > 1 {
                    let saved_index = self.current_frame;
                    let current = self.frames.swap_remove(saved_index);
                    let current_memory_mb = current.pixels.len() as f64 / (1024.0 * 1024.0);
                    self.frames.clear();
                    self.frames.push(current);
                    self.current_frame = 0;
                    self.frames_loaded = false;
                    self.saved_frame_index = Some(saved_index);
                    tracing::info!(
                        saved_index,
                        current_frame_memory_mb = format!("{:.1}", current_memory_mb),
                        strategy = "aggressive",
                        "GIF 已暂停，已释放帧数据"
                    );
                }
            }
            GifMemoryStrategy::Balanced => {
                // 平衡模式：保留当前帧附近的 N 帧
                // C-109: 兜底 keep >= 1，防止 balanced_keep_frames=0 时 drain 出空帧
                // 向量导致 gif.rs 的 % frames.len() 除零 panic
                let keep = self.balanced_keep_frames.max(1);
                if self.frames.len() > keep {
                    let current = self.current_frame;
                    let half = keep / 2;
                    let start = current.saturating_sub(half);
                    let end = (start + keep).min(self.frames.len());
                    let start = end.saturating_sub(keep);

                    // N-010: 移除无效赋值（原 `let released_count = ...; let _ = released_count;`），
                    // 将释放帧数并入下方 tracing 日志，避免 dead code 同时保留诊断信息。
                    let kept: Vec<GifFrame> = self.frames.drain(start..end).collect();
                    let released_count = self.frames.len();
                    self.frames = kept;
                    self.current_frame = current.saturating_sub(start);
                    self.saved_frame_index = Some(self.current_frame);

                    let memory_mb = self.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
                    tracing::info!(
                        kept_frames = self.frames.len(),
                        released_frames = released_count,
                        current_frame = self.current_frame,
                        memory_mb = format!("{:.1}", memory_mb),
                        strategy = "balanced",
                        "GIF 已暂停，保留部分帧数据"
                    );
                } else {
                    self.saved_frame_index = Some(self.current_frame);
                }
            }
            GifMemoryStrategy::Performance => {
                // 性能模式：保留所有帧，不做任何释放
                self.saved_frame_index = Some(self.current_frame);
                tracing::info!(
                    frame_count = self.frames.len(),
                    strategy = "performance",
                    "GIF 已暂停，保留所有帧数据"
                );
            }
            GifMemoryStrategy::Adaptive => {
                // 自适应模式：根据系统内存和 GIF 大小决定
                if self.should_keep_all_frames() {
                    // 内存充足，保留所有帧
                    self.saved_frame_index = Some(self.current_frame);
                    tracing::info!(
                        frame_count = self.frames.len(),
                        strategy = "adaptive(performance)",
                        "GIF 已暂停，内存充足，保留所有帧数据"
                    );
                } else {
                    // 内存紧张，使用平衡策略
                    let keep = self.calculate_adaptive_keep_frames();
                    if self.frames.len() > keep {
                        let current = self.current_frame;
                        let half = keep / 2;
                        let start = current.saturating_sub(half);
                        let end = (start + keep).min(self.frames.len());
                        let start = end.saturating_sub(keep);

                        let kept: Vec<GifFrame> = self.frames.drain(start..end).collect();
                        self.frames = kept;
                        self.current_frame = current.saturating_sub(start);
                        self.saved_frame_index = Some(self.current_frame);

                        let memory_mb = self.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
                        tracing::info!(
                            kept_frames = self.frames.len(),
                            current_frame = self.current_frame,
                            memory_mb = format!("{:.1}", memory_mb),
                            strategy = "adaptive(balanced)",
                            "GIF 已暂停，内存紧张，保留部分帧数据"
                        );
                    } else {
                        self.saved_frame_index = Some(self.current_frame);
                    }
                }
            }
        }
    }

    /// 根据策略处理恢复播放
    pub(crate) fn handle_resume(&mut self) -> Result<(), MirrorStarError> {
        self.paused = false;

        match self.memory_strategy {
            GifMemoryStrategy::Aggressive => {
                // 激进模式：需要重新解码
                if !self.frames_loaded {
                    // v18: 流式窗口解码——以 saved_index 为中心，解码中即时清空
                    // 窗口外帧像素，将峰值内存从预算上限降至窗口大小+1 帧。
                    let saved_index = self.saved_frame_index.unwrap_or(0);
                    let new_frames =
                        decode_gif_streaming(&self.image_path, self.max_memory_mb, saved_index)?;
                    let total = new_frames.len();
                    self.frames = new_frames;
                    self.current_frame = saved_index.min(total.saturating_sub(1));
                    self.total_frames = total;
                    self.frames_loaded = true;
                    self.saved_frame_index = None;
                    // #2: 重新全量解码后旧 sync 游标失效（指向旧迭代器），重置以备下次 sync 兜底重新打开
                    self.sync_frames_iter = None;
                    self.sync_cursor = 0;

                    // v8-C: 重新全量解码后立即应用流式窗口，仅保留当前帧 ± 1 帧像素
                    self.apply_streaming_window();

                    let memory_mb = self.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
                    tracing::info!(
                        frame_count = total,
                        current_frame = self.current_frame,
                        memory_mb = format!("{:.1}", memory_mb),
                        strategy = "aggressive",
                        "GIF 已恢复，重新解码帧数据并应用流式窗口"
                    );
                }
            }
            GifMemoryStrategy::Balanced | GifMemoryStrategy::Performance => {
                // 平衡模式和性能模式：帧数据已保留，直接恢复
                if let Some(saved_index) = self.saved_frame_index {
                    self.current_frame = saved_index.min(self.frames.len().saturating_sub(1));
                    self.saved_frame_index = None;
                }
                tracing::info!(
                    frame_count = self.frames.len(),
                    current_frame = self.current_frame,
                    strategy = ?self.memory_strategy,
                    "GIF 已恢复，帧数据已保留"
                );
            }
            GifMemoryStrategy::Adaptive => {
                // 自适应模式：根据是否有帧数据决定是否需要重新解码
                if !self.frames_loaded || self.frames.is_empty() {
                    // v18: 流式窗口解码——以 saved_index 为中心，解码中即时清空
                    // 窗口外帧像素，将峰值内存从预算上限降至窗口大小+1 帧。
                    let saved_index = self.saved_frame_index.unwrap_or(0);
                    let new_frames =
                        decode_gif_streaming(&self.image_path, self.max_memory_mb, saved_index)?;
                    let total = new_frames.len();
                    self.frames = new_frames;
                    self.current_frame = saved_index.min(total.saturating_sub(1));
                    self.total_frames = total;
                    self.frames_loaded = true;
                    self.saved_frame_index = None;
                    // #2: 重新全量解码后旧 sync 游标失效（指向旧迭代器），重置以备下次 sync 兜底重新打开
                    self.sync_frames_iter = None;
                    self.sync_cursor = 0;

                    // v8-C: 重新全量解码后立即应用流式窗口
                    self.apply_streaming_window();

                    let memory_mb = self.estimate_memory_bytes() as f64 / (1024.0 * 1024.0);
                    tracing::info!(
                        frame_count = total,
                        current_frame = self.current_frame,
                        memory_mb = format!("{:.1}", memory_mb),
                        strategy = "adaptive",
                        "GIF 已恢复，重新解码帧数据并应用流式窗口"
                    );
                } else if let Some(saved_index) = self.saved_frame_index {
                    self.current_frame = saved_index.min(self.frames.len().saturating_sub(1));
                    self.saved_frame_index = None;
                    tracing::info!(
                        frame_count = self.frames.len(),
                        current_frame = self.current_frame,
                        strategy = "adaptive",
                        "GIF 已恢复，帧数据已保留"
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试用单帧：像素首字节填入帧索引，便于在测试中识别帧身份
    fn make_frame(index: u8, width: u32, height: u32) -> GifFrame {
        GifFrame {
            pixels: vec![index; (width * height * 4) as usize],
            width,
            height,
            delay_ms: 100,
        }
    }

    /// 构造测试用 GifRenderData
    fn make_render_data(
        frames: Vec<GifFrame>,
        strategy: GifMemoryStrategy,
        keep: usize,
        current: usize,
    ) -> GifRenderData {
        let total = frames.len();
        GifRenderData {
            frames,
            current_frame: current,
            scaling_mode: ScalingMode::Fill,
            speed: 1.0,
            paused: false,
            image_path: String::new(),
            frames_loaded: true,
            saved_frame_index: None,
            memory_strategy: strategy,
            balanced_keep_frames: keep,
            max_memory_mb: crate::wallpaper::gif_decode::DEFAULT_MAX_GIF_MEMORY_MB,
            last_timer_delay: 0,
            total_frames: total,
            sync_frames_iter: None,
            sync_cursor: 0,
        }
    }

    // ── 内存预算计算 ──────────────────────────────────────────────────────────

    #[test]
    fn test_memory_budget_calculation() {
        // 3 帧：10x10 (400B), 20x20 (1600B), 5x5 (100B)
        let frames = vec![
            make_frame(0, 10, 10),
            make_frame(1, 20, 20),
            make_frame(2, 5, 5),
        ];
        let data = make_render_data(frames, GifMemoryStrategy::Balanced, 10, 0);

        // 总内存 = 400 + 1600 + 100 = 2100 字节
        assert_eq!(data.estimate_memory_bytes(), 2100);
        // 平均 = 2100 / 3 = 700 字节
        assert_eq!(data.estimate_avg_frame_bytes(), 700);
    }

    #[test]
    fn test_memory_budget_empty_frames() {
        // 空帧场景：内存为 0，平均帧字节为 0（不应 panic / 除零）
        let data = make_render_data(vec![], GifMemoryStrategy::Balanced, 10, 0);
        assert_eq!(data.estimate_memory_bytes(), 0);
        assert_eq!(data.estimate_avg_frame_bytes(), 0);
    }

    // ── Balanced 模式保留帧数逻辑 ─────────────────────────────────────────────

    #[test]
    fn test_balanced_keep_frames() {
        // 10 帧，current=5，keep=4
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Balanced, 4, 5);

        data.handle_pause();

        // half = 4/2 = 2, start = 5-2 = 3, end = min(3+4,10) = 7, start = 7-4 = 3
        // 保留区间 frames[3..7] -> 帧 3,4,5,6
        assert_eq!(data.frames.len(), 4, "balanced 模式应保留 keep 帧");
        let ids: Vec<u8> = data.frames.iter().map(|f| f.pixels[0]).collect();
        assert_eq!(ids, vec![3, 4, 5, 6]);
        // current_frame 应映射到保留区间内的相对位置：5 - 3 = 2
        assert_eq!(data.current_frame, 2);
        assert_eq!(data.saved_frame_index, Some(2));
        assert!(data.paused);
        // balanced 模式不会标记 frames_loaded=false（帧数据仍在内存中）
        assert!(data.frames_loaded);
    }

    #[test]
    fn test_balanced_keep_frames_at_end() {
        // current 位于末尾：10 帧，current=9，keep=4
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Balanced, 4, 9);

        data.handle_pause();

        // half=2, start=9-2=7, end=min(7+4,10)=10, start=10-4=6 -> frames[6..10]
        assert_eq!(data.frames.len(), 4);
        let ids: Vec<u8> = data.frames.iter().map(|f| f.pixels[0]).collect();
        assert_eq!(ids, vec![6, 7, 8, 9]);
        // current_frame = 9 - 6 = 3
        assert_eq!(data.current_frame, 3);
    }

    #[test]
    fn test_balanced_keep_frames_fewer_than_keep() {
        // 帧数 <= keep：不应丢弃任何帧
        let frames: Vec<GifFrame> = (0..3).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Balanced, 10, 1);

        data.handle_pause();

        assert_eq!(data.frames.len(), 3, "帧数 <= keep 时不应丢弃帧");
        assert_eq!(data.current_frame, 1);
        assert_eq!(data.saved_frame_index, Some(1));
    }

    // ── 帧丢弃 / 加载决策 ─────────────────────────────────────────────────────

    #[test]
    fn test_frame_drop_decision_aggressive() {
        // 5 帧，current=2，Aggressive 模式：仅保留当前帧，其余丢弃
        let frames: Vec<GifFrame> = (0..5).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Aggressive, 10, 2);

        data.handle_pause();

        // 仅保留 1 帧（当前帧）
        assert_eq!(data.frames.len(), 1, "aggressive 模式应仅保留当前帧");
        // 保留的应是原 current_frame=2 的那一帧
        assert_eq!(data.frames[0].pixels[0], 2);
        assert_eq!(data.current_frame, 0);
        // aggressive 模式释放帧数据后标记为未加载，恢复时需重新解码
        assert!(!data.frames_loaded);
        assert_eq!(data.saved_frame_index, Some(2));
        assert!(data.paused);
    }

    #[test]
    fn test_performance_keeps_all_frames() {
        // 8 帧，Performance 模式：保留所有帧，不释放
        let frames: Vec<GifFrame> = (0..8).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 3);

        data.handle_pause();

        assert_eq!(data.frames.len(), 8, "performance 模式应保留所有帧");
        assert_eq!(data.current_frame, 3, "current_frame 不变");
        assert_eq!(data.saved_frame_index, Some(3));
        assert!(data.frames_loaded, "performance 模式不释放帧数据");
        assert!(data.paused);
    }

    #[test]
    fn test_aggressive_single_frame_noop() {
        // 单帧 Aggressive：frames.len() <= 1，不进入释放分支
        let frames = vec![make_frame(0, 1, 1)];
        let mut data = make_render_data(frames, GifMemoryStrategy::Aggressive, 10, 0);

        data.handle_pause();

        assert_eq!(data.frames.len(), 1);
        assert!(data.frames_loaded, "单帧时不应标记为未加载");
        // 当前实现下，单帧 Aggressive 不进入释放分支，不会设置 saved_frame_index
        assert_eq!(data.saved_frame_index, None);
        assert!(data.paused);
    }

    // ── v8-C: 流式帧缓存窗口测试 ─────────────────────────────────────────────

    #[test]
    fn test_apply_streaming_window_clears_outside_frames() {
        // (d): apply_streaming_window 应清空 current ± 1 之外帧的 pixels
        // 10 帧，current=5，窗口半幅=1 → 保留 [4,5,6]，清空 [0,1,2,3,7,8,9]
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 5);

        data.apply_streaming_window();

        assert_eq!(data.frames.len(), 10, "frames 长度不变（仅清空像素）");
        for (i, frame) in data.frames.iter().enumerate() {
            if (4..=6).contains(&i) {
                assert!(!frame.pixels.is_empty(), "窗口内帧 {} 应保留像素", i);
            } else {
                assert!(
                    frame.pixels.is_empty(),
                    "窗口外帧 {} 应清空像素，实际长度 {}",
                    i,
                    frame.pixels.len()
                );
            }
        }
    }

    #[test]
    fn test_apply_streaming_window_near_start() {
        // (d): current 靠近开头时窗口左边界应 saturating_sub 到 0
        // 10 帧，current=0，窗口 [0,1]，清空 [2..9]
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 0);

        data.apply_streaming_window();

        for (i, frame) in data.frames.iter().enumerate() {
            if i <= 1 {
                assert!(!frame.pixels.is_empty(), "帧 {} 应保留像素", i);
            } else {
                assert!(frame.pixels.is_empty(), "帧 {} 应清空像素", i);
            }
        }
    }

    #[test]
    fn test_apply_streaming_window_near_end() {
        // (d): current 靠近末尾时窗口右边界应 min 到 total
        // 10 帧，current=9，窗口 [8,9]，清空 [0..7]
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 9);

        data.apply_streaming_window();

        for (i, frame) in data.frames.iter().enumerate() {
            if i >= 8 {
                assert!(!frame.pixels.is_empty(), "帧 {} 应保留像素", i);
            } else {
                assert!(frame.pixels.is_empty(), "帧 {} 应清空像素", i);
            }
        }
    }

    #[test]
    fn test_apply_streaming_window_empty_frames() {
        // v8-C: frames 为空时 apply_streaming_window 应安全返回（无 panic）
        let mut data = make_render_data(vec![], GifMemoryStrategy::Performance, 10, 0);
        data.apply_streaming_window();
        assert!(data.frames.is_empty());
    }

    #[test]
    fn test_apply_streaming_window_idempotent() {
        // (d): 多次调用 apply_streaming_window 应幂等（不会重复清空已空帧）
        let frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 5);

        data.apply_streaming_window();
        data.apply_streaming_window();

        // 第二次调用不应改变状态
        for (i, frame) in data.frames.iter().enumerate() {
            if (4..=6).contains(&i) {
                assert!(!frame.pixels.is_empty(), "帧 {} 应仍保留像素", i);
            } else {
                assert!(frame.pixels.is_empty(), "帧 {} 应仍为空", i);
            }
        }
    }

    #[test]
    fn test_handle_pause_with_empty_pixels_aggressive() {
        // (d): 暂停时部分帧像素已空（播放中被流式窗口清空），
        // Aggressive handle_pause 应仍正确保留当前帧（有像素）。
        // 模拟：10 帧，current=5，窗口 [4,5,6] 有像素，其余空
        let mut frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        for (i, frame) in frames.iter_mut().enumerate() {
            if !(4..=6).contains(&i) {
                frame.pixels.clear();
                frame.pixels.shrink_to_fit();
            }
        }
        let mut data = make_render_data(frames, GifMemoryStrategy::Aggressive, 10, 5);
        data.handle_pause();

        // Aggressive 保留当前帧（frame 5，有像素）
        assert_eq!(data.frames.len(), 1, "aggressive 模式应仅保留当前帧");
        assert_eq!(data.frames[0].pixels[0], 5, "保留的应是当前帧");
        assert_eq!(data.current_frame, 0);
        assert!(!data.frames_loaded);
        assert_eq!(data.saved_frame_index, Some(5));
        assert!(data.paused);
    }

    #[test]
    fn test_handle_pause_with_empty_pixels_balanced() {
        // (d): Balanced handle_pause 在部分帧像素为空时应正常 drain。
        // 模拟：10 帧，current=5，keep=2，窗口 [4,5,6] 有像素。
        // Balanced drain 区间：half=2/2=1, start=5-1=4, end=min(4+2,10)=6, start=6-2=4 → drain [4..6)
        // 保留 frames[4,5]，其中 4,5 都在窗口 [4,5,6] 内，有像素。
        let mut frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        for (i, frame) in frames.iter_mut().enumerate() {
            if !(4..=6).contains(&i) {
                frame.pixels.clear();
                frame.pixels.shrink_to_fit();
            }
        }
        let mut data = make_render_data(frames, GifMemoryStrategy::Balanced, 2, 5);
        data.handle_pause();

        // 应保留 2 帧（4,5），current_frame = 5-4 = 1
        assert_eq!(data.frames.len(), 2, "balanced 模式应保留 keep 帧");
        let ids: Vec<u8> = data.frames.iter().map(|f| f.pixels[0]).collect();
        assert_eq!(ids, vec![4, 5]);
        assert_eq!(data.current_frame, 1);
    }

    #[test]
    fn test_handle_pause_balanced_drain_may_include_empty_pixels() {
        // (d): 当 keep > 流式窗口时，Balanced drain 可能保留含空像素的帧。
        // 模拟：10 帧，current=5，流式窗口 [4,5,6]，keep=8。
        // drain 区间：half=4, start=5-4=1, end=min(1+8,10)=9, start=9-8=1 → drain [1..9)
        // 保留 frames[1..8]，其中 1,2,3,7 像素为空（在窗口外），4,5,6 有像素，8 为空。
        // drain 后 frames=[1,2,3,4,5,6,7,8]，其中 1,2,3,7,8 像素为空。
        // 验证：handle_pause 不 panic，frames 长度正确。
        let mut frames: Vec<GifFrame> = (0..10).map(|i| make_frame(i, 1, 1)).collect();
        for (i, frame) in frames.iter_mut().enumerate() {
            if !(4..=6).contains(&i) {
                frame.pixels.clear();
                frame.pixels.shrink_to_fit();
            }
        }
        let mut data = make_render_data(frames, GifMemoryStrategy::Balanced, 8, 5);
        data.handle_pause();

        assert_eq!(data.frames.len(), 8, "应保留 8 帧（含空像素帧）");
        // current_frame = 5 - 1 = 4
        assert_eq!(data.current_frame, 4);
        // 当前帧（原索引 5，现索引 4）应有像素
        assert!(!data.frames[4].pixels.is_empty(), "当前帧应有像素");
    }

    #[test]
    fn test_reload_current_frame_pixels_noop_when_already_loaded() {
        // v8-C: 当前帧已有像素时 reload_current_frame_pixels 应返回 true 且不重新解码
        let frames: Vec<GifFrame> = (0..5).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 2);
        // image_path 为空，若尝试重新解码会失败；但当前帧有像素，应直接返回 true
        let result = data.reload_current_frame_pixels();
        assert!(result, "已有像素时应返回 true（无需重新解码）");
        assert!(!data.frames[2].pixels.is_empty());
    }

    #[test]
    fn test_reload_current_frame_pixels_empty_path_fails() {
        // v8-C: 当前帧像素为空且 image_path 无效时，reload 应返回 false
        let frames: Vec<GifFrame> = (0..5).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 2);
        // 清空当前帧像素
        data.frames[2].pixels.clear();
        data.frames[2].pixels.shrink_to_fit();
        // image_path 为空 → 解码失败
        let result = data.reload_current_frame_pixels();
        assert!(!result, "空路径解码应失败，返回 false");
        assert!(data.frames[2].pixels.is_empty(), "失败后像素应仍为空");
    }

    #[test]
    fn test_reload_current_frame_pixels_from_file() {
        // v8-C: 从真实 GIF 文件按需解码当前帧像素
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("reload_test_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..3u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap().to_string();
        let frames: Vec<GifFrame> = (0..3).map(|i| make_frame(i, 1, 1)).collect();
        let mut data = make_render_data(frames, GifMemoryStrategy::Performance, 10, 1);
        data.image_path = path;
        // 清空当前帧（索引 1）像素
        data.frames[1].pixels.clear();
        data.frames[1].pixels.shrink_to_fit();
        assert!(data.frames[1].pixels.is_empty());

        // 重新解码
        let result = data.reload_current_frame_pixels();
        assert!(result, "从文件重新解码应成功");
        assert_eq!(data.frames[1].pixels.len(), 4, "1x1 RGBA = 4 字节");
        // RGBA [1,0,0,255]（GDI 经 BI_BITFIELDS 解释，无需转换）
        assert_eq!(data.frames[1].pixels[0], 1, "R 分量");
        assert_eq!(data.frames[1].pixels[1], 0, "G 分量");
        assert_eq!(data.frames[1].pixels[2], 0, "B 分量");
        assert_eq!(data.frames[1].pixels[3], 255, "A 分量");
    }

    #[test]
    fn test_total_frames_field_initialized() {
        // v8-C: make_render_data 应将 total_frames 初始化为 frames.len()
        let frames: Vec<GifFrame> = (0..7).map(|i| make_frame(i, 1, 1)).collect();
        let data = make_render_data(frames, GifMemoryStrategy::Balanced, 5, 3);
        assert_eq!(data.total_frames, 7);
    }
}
