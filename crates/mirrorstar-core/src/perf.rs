//! 性能埋点工具（v17 性能优化专项）
//!
//! 轻量级、低开销的测量工具，通过 tracing 输出到日志文件
//! 数据根下 `logs/mirrorstar.log`。所有埋点使用 target
//! `mirrorstar::perf`，可用 `RUST_LOG=mirrorstar::perf=info` 单独过滤分析。
//!
//! 设计原则：测量驱动，避免盲目优化。埋点本身开销极小（一次 Instant 取值
//! + 少量整数累加），不影响被测路径性能；报告仅在达到阈值时输出一次日志。

use std::time::{Duration, Instant};

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

/// 采样进程内存计数器，返回 (工作集 MB, 私有提交 MB)。失败时返回 (0.0, 0.0)。
///
/// 一次 `GetProcessMemoryInfo` 调用同时取回 WorkingSetSize 与 PrivateUsage，
/// 避免重复系统调用。`cb` 字段必须显式设置，否则 API 返回失败。
fn sample_memory() -> (f64, f64) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_ok() {
        let rss_mb = counters.WorkingSetSize as f64 / (1024.0 * 1024.0);
        let private_mb = counters.PrivateUsage as f64 / (1024.0 * 1024.0);
        (rss_mb, private_mb)
    } else {
        (0.0, 0.0)
    }
}

/// 当前进程工作集（RSS）内存，单位 MB。
///
/// `WorkingSetSize` 反映当前驻留物理内存的页（含与其他进程共享的 DLL），
/// 与任务管理器"工作集"列一致。失败时返回 0.0。
pub fn process_rss_mb() -> f64 {
    sample_memory().0
}

/// 当前进程私有提交内存，单位 MB。
///
/// `PrivateUsage` 反映应用独占的提交内存（不含共享 DLL），比工作集更能
/// 体现应用的内存压力（工作集可能被换出）。失败时返回 0.0。
pub fn process_private_mb() -> f64 {
    sample_memory().1
}

/// GIF 帧性能跟踪器：累计帧间隔与绘制耗时，每 N 帧输出一次统计。
///
/// - `record_frame()` 在 `WM_TIMER` 帧推进时调用，测量实际帧间隔（基于 `Instant`）
/// - `record_paint()` 在 `WM_PAINT` 绘制完成后调用，记录绘制耗时
/// - 每 `report_every` 帧 log 一次：实际 FPS、慢帧数、平均/最大绘制耗时、RSS
/// - 慢帧阈值 = max(expected_delay × 1.5 + 16ms, 33ms)，避免低帧率 GIF 误报
///
/// 仅供壁纸线程的窗口过程访问（与 `GifWindowData` 生命周期绑定），不跨线程共享。
pub struct FramePerfTracker {
    label: &'static str,
    frames_since_report: usize,
    last_frame: Option<Instant>,
    last_report: Instant,
    report_every: usize,
    slow_threshold_ms: u32,
    slow_frames_since_report: usize,
    /// 绘制耗时累计（微秒）
    paint_total_us: u128,
    paint_max_us: u128,
    paint_count: usize,
}

impl FramePerfTracker {
    /// 创建跟踪器。
    ///
    /// - `label`：日志标签（如 "gif"）
    /// - `report_every`：每 N 帧输出一次统计
    /// - `expected_delay_ms`：期望帧间隔（帧标称延迟），用于慢帧判定
    pub fn new(label: &'static str, report_every: usize, expected_delay_ms: u32) -> Self {
        Self {
            label,
            frames_since_report: 0,
            last_frame: None,
            last_report: Instant::now(),
            report_every,
            // 慢帧阈值：期望的 1.5 倍 + 16ms 容差，下限 33ms（~30fps）
            slow_threshold_ms: ((expected_delay_ms as f32 * 1.5) as u32 + 16).max(33),
            slow_frames_since_report: 0,
            paint_total_us: 0,
            paint_max_us: 0,
            paint_count: 0,
        }
    }

    /// 记录一帧推进（`WM_TIMER` 调用）。达到报告阈值时输出统计日志。
    pub fn record_frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_frame {
            let interval_ms = (now - last).as_millis() as u32;
            if interval_ms > self.slow_threshold_ms {
                self.slow_frames_since_report += 1;
            }
        }
        self.last_frame = Some(now);
        self.frames_since_report += 1;

        if self.frames_since_report >= self.report_every {
            self.report();
        }
    }

    /// 记录一次绘制耗时（`WM_PAINT` 调用）。
    pub fn record_paint(&mut self, duration: Duration) {
        let us = duration.as_micros();
        self.paint_total_us += us;
        if us > self.paint_max_us {
            self.paint_max_us = us;
        }
        self.paint_count += 1;
    }

    /// 输出当前累计统计并重置计数器。
    fn report(&mut self) {
        let elapsed = self.last_report.elapsed();
        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let fps = self.frames_since_report as f64 / elapsed_secs;
        let avg_paint_ms = if self.paint_count > 0 {
            self.paint_total_us as f64 / self.paint_count as f64 / 1000.0
        } else {
            0.0
        };
        let max_paint_ms = self.paint_max_us as f64 / 1000.0;
        let (rss_mb, private_mb) = sample_memory();

        tracing::info!(
            target: "mirrorstar::perf",
            label = self.label,
            fps = format!("{:.1}", fps),
            frames = self.frames_since_report,
            slow_frames = self.slow_frames_since_report,
            avg_paint_ms = format!("{:.2}", avg_paint_ms),
            max_paint_ms = format!("{:.2}", max_paint_ms),
            rss_mb = format!("{:.1}", rss_mb),
            private_mb = format!("{:.1}", private_mb),
            "PERF-FPS: 帧率与绘制统计"
        );

        self.frames_since_report = 0;
        self.slow_frames_since_report = 0;
        self.paint_total_us = 0;
        self.paint_max_us = 0;
        self.paint_count = 0;
        self.last_report = Instant::now();
    }

    /// 重置跟踪器（暂停/恢复后调用，避免暂停时长污染 FPS 统计）。
    pub fn reset(&mut self) {
        self.frames_since_report = 0;
        self.slow_frames_since_report = 0;
        self.last_frame = None;
        self.last_report = Instant::now();
        self.paint_total_us = 0;
        self.paint_max_us = 0;
        self.paint_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_memory_returns_nonzero() {
        // 进程内存应大于 0（测试进程本身有内存占用）
        let rss = process_rss_mb();
        let private = process_private_mb();
        assert!(rss > 0.0, "RSS 应大于 0，实际 {rss}");
        assert!(private > 0.0, "私有内存应大于 0，实际 {private}");
    }

    #[test]
    fn fps_tracker_counts_frames_and_reports() {
        let mut tracker = FramePerfTracker::new("test", 3, 100);
        tracker.record_frame();
        tracker.record_frame();
        // 第三帧触发报告（在 record_frame 内部 log，不 panic 即可）
        tracker.record_frame();
        assert_eq!(tracker.frames_since_report, 0, "报告后应重置计数");
    }

    #[test]
    fn fps_tracker_reset_clears_state() {
        let mut tracker = FramePerfTracker::new("test", 100, 100);
        tracker.record_frame();
        tracker.record_frame();
        assert_eq!(tracker.frames_since_report, 2);
        tracker.reset();
        assert_eq!(tracker.frames_since_report, 0);
        assert!(tracker.last_frame.is_none());
    }

    #[test]
    fn fps_tracker_records_paint_stats() {
        let mut tracker = FramePerfTracker::new("test", 100, 100);
        tracker.record_paint(Duration::from_micros(500));
        tracker.record_paint(Duration::from_micros(800));
        assert_eq!(tracker.paint_count, 2);
        assert_eq!(tracker.paint_total_us, 1300);
        assert_eq!(tracker.paint_max_us, 800);
    }

    #[test]
    fn fps_tracker_slow_threshold_floor() {
        // expected_delay_ms=0 时阈值仍应 >= 33ms（下限保护）
        let tracker = FramePerfTracker::new("test", 100, 0);
        assert!(tracker.slow_threshold_ms >= 33);
    }
}
