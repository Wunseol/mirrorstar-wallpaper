//! 桌面（WorkerW）状态轮询策略（SubTask 9.7.1，v16-C-007）。
//!
//! 壳侧 `workerw_check` 检测到 WorkerW 失效时推送 `desktop-status-changed`（`ok:false`），
//! 旧前端据此进入"2s 间隔 `check_desktop_status` 轮询、30s 超时提示重启"的补救路径。
//! 本模块只承载该路径的**纯策略**（间隔、超时、是否应启动轮询），便于无 GUI 单测；
//! 定时器与协议请求由 `main` 侧驱动。

use std::time::{Duration, Instant};

/// 轮询总超时（与旧前端 `deadline = 30_000` 一致）。
///
/// 注：2s 的轮询间隔由 `ui/main.slint` 的 `Timer` 承载（`desktop-poll-tick`），
/// 故此处只保留超时这一条由 Rust 判定的策略。
pub const POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// 何时应（重新）启动一次轮询：仅当本次事件报"失效"且当前无轮询进行中。
///
/// `ok == true`（已恢复）不启动轮询；已在轮询中则忽略重复的失效事件（防重入，
/// 与旧前端 `pollInFlight` 标志一致）。
pub fn should_start_poll(status_ok: bool, polling: bool) -> bool {
    !status_ok && !polling
}

/// 一次轮询会话的截止时刻。
#[derive(Debug, Clone, Copy)]
pub struct PollSession {
    deadline: Instant,
}

impl PollSession {
    /// 以 `now` 为起点开启会话（30s 后到期）。
    pub fn start(now: Instant) -> Self {
        Self {
            deadline: now + POLL_TIMEOUT,
        }
    }

    /// 会话是否已超时（`now >= deadline`）。
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_only_on_failure_and_when_idle() {
        assert!(should_start_poll(false, false));
        assert!(!should_start_poll(false, true), "轮询中不重复启动");
        assert!(!should_start_poll(true, false), "已恢复不启动");
        assert!(!should_start_poll(true, true));
    }

    #[test]
    fn session_expires_after_timeout() {
        let start = Instant::now();
        let session = PollSession::start(start);
        assert!(!session.expired(start));
        assert!(!session.expired(start + POLL_TIMEOUT - Duration::from_millis(1)));
        assert!(session.expired(start + POLL_TIMEOUT));
        assert!(session.expired(start + POLL_TIMEOUT + Duration::from_secs(1)));
    }
}
