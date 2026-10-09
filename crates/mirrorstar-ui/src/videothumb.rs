//! 视频首帧缩略图的**按需生成状态**（Task 9.7.6）。
//!
//! ## 背景
//!
//! `thumbs::plan` 对"有源文件但配置里没有 `thumbnail` 的视频"退化为类型图标占位。真实库里
//! 视频通常由壳侧后台生成缩略图，但生成未完成 / 失败时 UI 只能显示图标。本模块用
//! `mirrorstar-core` 的 `generate_video_thumbnail`（随包 mpv 抽帧）为**可见卡片**补齐首帧，
//! 结果落盘到 `data_dir/thumbnails/`（core 的确定性命名天然去重）。
//!
//! ## 状态真相
//!
//! - **成功**：由磁盘上的确定性缓存体现——`mirrorstar_core::config::thumbnail_cache_path`
//!   得到的绝对路径存在即为成功；跨会话自动复用，不再重复抽帧。
//! - **会话内失败**：由 `dispatched` 去重体现——本会话内该路径不再重试。
//!
//! ## 硬约束（Task 9.7.6）
//!
//! - 只为**可见卡片**触发（调用方只把可见窗口内的候选送进来）；
//! - 进程内去重：同一 `file_path` 每次会话只尝试一次；
//! - 并发受限：同一时刻至多 [`MAX_IN_FLIGHT`] 个 mpv 抽帧任务。
//!
//! 本模块只维护纯数据（不含 IO / 不含 Slint），去重与限流的边界可直接单测。

use std::collections::HashSet;
use std::sync::Mutex;

/// 同时在跑的抽帧任务上限（限制 CPU / 内存 / 子进程数；串行即足够）。
pub const MAX_IN_FLIGHT: usize = 1;

#[derive(Default)]
struct Inner {
    /// 已派发过的路径（进行中 / 已成功 / 会话内失败），用于去重。
    dispatched: HashSet<String>,
    /// 进行中的任务数。
    in_flight: usize,
}

/// 跨线程共享的视频首帧缩略图派发状态。
#[derive(Default)]
pub struct ThumbStore {
    inner: Mutex<Inner>,
}

impl ThumbStore {
    /// 新建空状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从候选路径中挑出应派发的任务（去重、限并发），并标记为已派发。
    ///
    /// 返回的路径数量 ≤ 剩余并发额度；调用方须为每条路径各调用一次 [`Self::finish`]。
    pub fn dispatch_ready(&self, candidates: &[String]) -> Vec<String> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut jobs = Vec::new();
        for path in candidates {
            if inner.in_flight >= MAX_IN_FLIGHT {
                break;
            }
            if inner.dispatched.contains(path) {
                continue;
            }
            inner.dispatched.insert(path.clone());
            inner.in_flight += 1;
            jobs.push(path.clone());
        }
        jobs
    }

    /// 记录一次任务结束（成功或失败），仅释放并发额度；成功由磁盘缓存探测体现。
    pub fn finish(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.in_flight = inner.in_flight.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_is_deduped_and_limited() {
        let store = ThumbStore::new();
        let candidates = vec![
            "a.mp4".to_string(),
            "b.mp4".to_string(),
            "a.mp4".to_string(),
        ];

        // MAX_IN_FLIGHT=1：首次只派发一个。
        let jobs = store.dispatch_ready(&candidates);
        assert_eq!(jobs, vec!["a.mp4".to_string()]);

        // 进行中未结束：不得再派发（含未派发过的 b）。
        assert!(store.dispatch_ready(&candidates).is_empty());

        // 结束 a 后可派发 b；a 因已派发不再重复。
        store.finish();
        let jobs = store.dispatch_ready(&candidates);
        assert_eq!(jobs, vec!["b.mp4".to_string()]);
    }

    #[test]
    fn finished_paths_stay_dispatched_but_new_paths_can_dispatch() {
        let store = ThumbStore::new();
        store.dispatch_ready(&["a.mp4".to_string()]);
        store.finish();
        // a 已派发过，即使额度已释放也不会再次派发；新的未派发路径可派发。
        let jobs = store.dispatch_ready(&["a.mp4".to_string(), "c.mp4".to_string()]);
        assert_eq!(jobs, vec!["c.mp4".to_string()]);
    }

    #[test]
    fn empty_candidates_yield_no_jobs() {
        let store = ThumbStore::new();
        assert!(store.dispatch_ready(&[]).is_empty());
    }
}
