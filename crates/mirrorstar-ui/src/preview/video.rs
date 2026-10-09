//! 视频预览（Task 8.3 / fix-manual-test-preview-drag-thumbnail-ui Task 2）。
//!
//! ## 独立 mpv 实例 + `--wid` 宿主子窗口
//!
//! 预览的 mpv 由 **UI 进程自行启动**，与壁纸侧（wp-proc 管理的）mpv **完全独立**：
//! 壁纸侧 mpv 经过一套离屏 geometry / `SetParent(WorkerW)` 的嵌入流程，二者共用会互相
//! 干扰。预览侧只需要把画面渲染进我们创建的 `WS_CHILD` 宿主子窗口，因此用最简单的
//! `--wid=<宿主句柄>` 方案，不需要 `SetWindowPos` 搬运窗口。
//!
//! 参数集来自 Task 8.2 spike 的实测结论：
//! 关键项为 `--auto-window-resize=no`（禁止 mpv 自改窗口尺寸导致越界）、`--focus-on=never`
//! 与 `--input-vo-keyboard=no`（不抢焦点 / 不吞键盘）、`--loop-file=yes`（循环播放）。
//! `--force-window` 在 `--wid` 下非必需，故不追加。
//!
//! ## 播放控制（Task 2）
//!
//! mpv 以 `--no-input-default-bindings` 启动（封死键盘/鼠标默认绑定，避免预览期间窃取主窗口交互），
//! 因此播放控制**只能经 IPC**：追加 `--input-ipc-server=\\.\pipe\<name>` 开一条命名管道，
//! 由一条**后台工作线程**独占 [`MpvIpcClient`] 收发命令。UI 线程只通过 `mpsc` 发消息、
//! 通过 `slint::invoke_from_event_loop` 收状态回投，**绝不阻塞**（IPC 全为阻塞式调用）。
//!
//! ## 生命周期
//!
//! 关闭预览时 **先停控制线程**（置停止位 + 丢弃发送端，`recv_timeout` 至多 150ms 返回），
//! 再 `stop_immediate()`（`TerminateProcess`，不再等 3s 优雅退出——循环播放的 mpv 不会自行
//! 退出），最后销毁宿主子窗口。顺序不能颠倒，否则线程可能访问已销毁资源。子进程同时被放入
//! 启用了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object，UI 进程异常退出时内核兜底回收，
//! 杜绝孤儿 mpv。上述能力全部复用 core 的 [`ProcessManager`]，不再自行拼装 Win32 调用。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use mirrorstar_core::ipc::mpv_protocol::MpvIpcClient;
use mirrorstar_core::process::manager::ProcessManager;
use mirrorstar_core::wallpaper::subprocess_base::SubprocessRendererBase;

use super::host_child::{self, PhysRect};
use crate::protocol::log;

/// UI 线程 → 后台控制线程的播放控制消息。
pub enum ControlMsg {
    /// 切换播放/暂停。
    TogglePlay,
    /// 跳转到指定秒数。
    Seek(f64),
    /// 设置播放倍速（1.0 为原速）。
    SetSpeed(f64),
    /// 设置音量（0..100，mpv 标度）。
    SetVolume(f32),
    /// 设置静音。
    SetMute(bool),
}

/// 预览 mpv 命名管道序号：管道名必须唯一（同一 UI 进程可能反复开关预览），
/// 又不宜为此新增 uuid 依赖，故用 `进程 id + 进程内自增序号` 组合。
static PREVIEW_PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一个正在播放的视频预览。
pub struct MpvPreview {
    manager: ProcessManager,
    host: HWND,
    pid: u32,
    /// 宿主子窗口是否已显式显示（隐藏创建 → 就绪后显示，避免打开时残影）。
    shown: bool,
    /// 首帧就绪态（控制线程置位，UI 线程读取）：`vo-configured` 或 `duration/time-pos>0`。
    ready: Arc<AtomicBool>,
    /// 打开时刻，用于首帧就绪超时兜底（避免 mpv 异常时宿主永不显示）。
    opened_at: Instant,
    /// 控制线程消息发送端。`Drop` 时先置 `None` 丢弃，使工作线程收到 `Disconnected` 而尽快退出。
    control_tx: Option<Sender<ControlMsg>>,
    /// 停止位；`Drop` 置位后工作线程在下一轮循环顶部退出。
    stop: Arc<AtomicBool>,
    /// 后台控制线程句柄。
    worker: Option<JoinHandle<()>>,
}

impl MpvPreview {
    /// PID（诊断 / 测试用）。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 宿主子窗口句柄（供上层"先隐藏后销毁"）。
    pub fn host(&self) -> HWND {
        self.host
    }

    /// 显式显示宿主子窗口（幂等）。
    pub fn show(&mut self) {
        if self.shown {
            return;
        }
        host_child::show_host(self.host);
        self.shown = true;
    }

    /// 首帧就绪门控显示：仅当 mpv 已输出首帧（`ready`）或超过兜底上限时调用 [`Self::show`]。
    ///
    /// 隐藏创建后必须在"首帧就绪"时再显示，否则被显示的宿主子窗口是空白块（黑块/桌面残影），
    /// 并盖住 Slint 占位文案。`show()` 本身幂等（内部 `shown` 标记），可被 250ms 几何兜底
    /// Timer 高频调用。超出兜底上限仍未就绪时记录一条诊断日志并强制显示，避免极端情况下永不显示。
    pub fn maybe_show(&mut self) {
        /// 首帧就绪兜底上限：超过则强制显示（mpv 异常时不让宿主一直空白）。
        const FALLBACK: Duration = Duration::from_millis(1200);
        let ready = self.ready.load(Ordering::Relaxed);
        let elapsed = self.opened_at.elapsed();
        if should_show(self.shown, ready, elapsed, FALLBACK) {
            if !ready {
                log(format!(
                    "视频预览首帧未在 {}ms 内就绪，按兜底上限强制显示（pid={}）",
                    FALLBACK.as_millis(),
                    self.pid
                ));
            }
            self.show();
        }
    }

    /// 预览区尺寸变化时重排宿主子窗口（mpv 随宿主 `--wid` 自动铺满）。
    pub fn resize(&self, rect: PhysRect) {
        host_child::move_host(self.host, rect);
    }

    /// 切换播放/暂停（发送失败忽略：关闭竞态下线程可能已退出）。
    pub fn toggle_play(&self) {
        if let Some(tx) = self.control_tx.as_ref() {
            let _ = tx.send(ControlMsg::TogglePlay);
        }
    }

    /// 跳转到 `seconds` 秒（发送失败忽略）。
    pub fn seek(&self, seconds: f64) {
        if let Some(tx) = self.control_tx.as_ref() {
            let _ = tx.send(ControlMsg::Seek(seconds));
        }
    }

    /// 设置播放倍速（发送失败忽略：关闭竞态下线程可能已退出）。
    pub fn set_speed(&self, speed: f64) {
        if let Some(tx) = self.control_tx.as_ref() {
            let _ = tx.send(ControlMsg::SetSpeed(speed));
        }
    }

    /// 设置音量（0..100，mpv 标度；发送失败忽略）。
    pub fn set_volume(&self, volume: f32) {
        if let Some(tx) = self.control_tx.as_ref() {
            let _ = tx.send(ControlMsg::SetVolume(volume.clamp(0.0, 100.0)));
        }
    }

    /// 设置静音（发送失败忽略）。
    pub fn set_mute(&self, muted: bool) {
        if let Some(tx) = self.control_tx.as_ref() {
            let _ = tx.send(ControlMsg::SetMute(muted));
        }
    }
}

impl Drop for MpvPreview {
    fn drop(&mut self) {
        // 顺序：先停控制线程，再杀进程，最后销毁窗口。
        // 置停止位并丢弃发送端：工作线程的 recv_timeout 会尽快返回（超时或 Disconnected），
        // 避免其在线程内继续访问即将销毁的宿主窗口/进程。
        self.stop.store(true, Ordering::Relaxed);
        self.control_tx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // 立即终止（TerminateProcess + 关闭 Job 句柄，触发 KILL_ON_JOB_CLOSE 兜底），
        // 随后销毁宿主子窗口，确保关闭预览后不留 mpv 进程与窗口。
        if let Err(e) = self.manager.stop_immediate() {
            log(format!("终止预览 mpv 失败（pid={}）：{e}", self.pid));
        }
        host_child::destroy_host(self.host);
    }
}

/// 启动 mpv 播放 `video`，画面嵌入新建的宿主子窗口，并开一条 IPC 通道用于播放控制。
///
/// `parent` 为 Slint 顶层窗口；`rect` 为预览区在父窗口客户区内的物理矩形；
/// `ui_weak` 用于把播放状态回投到 Slint（`Weak` 本身可跨线程 move）。
pub fn open(
    parent: HWND,
    rect: PhysRect,
    video: &str,
    ui_weak: slint::Weak<crate::MainWindow>,
    volume: f32,
    muted: bool,
) -> Result<MpvPreview, String> {
    let host = host_child::create_host_child(parent, rect)?;
    let exe = SubprocessRendererBase::find_bundled_executable(Some("mpv"), "mpv.exe", "mpv");
    // 进程 id + 自增序号保证唯一：管道名不含 `\\.\pipe\` 前缀（由 MpvIpcClient 内部补全）。
    let pipe_name = format!(
        "mirrorstar-preview-mpv-{}-{}",
        std::process::id(),
        PREVIEW_PIPE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let args = build_mpv_args(host, &pipe_name, video, volume, muted);
    let mut manager = ProcessManager::new(exe);
    match manager.start(args) {
        Ok(pid) => {
            let (tx, rx) = channel::<ControlMsg>();
            let stop = Arc::new(AtomicBool::new(false));
            let ready = Arc::new(AtomicBool::new(false));
            let opened_at = Instant::now();
            let worker = {
                let stop = Arc::clone(&stop);
                let ready = Arc::clone(&ready);
                let pipe_name = pipe_name.clone();
                std::thread::spawn(move || {
                    run_control_worker(pipe_name, rx, stop, ready, ui_weak);
                })
            };
            Ok(MpvPreview {
                manager,
                host,
                pid,
                shown: false,
                ready,
                opened_at,
                control_tx: Some(tx),
                stop,
                worker: Some(worker),
            })
        }
        Err(e) => {
            host_child::destroy_host(host);
            Err(format!("启动 mpv 失败: {e}"))
        }
    }
}

/// 后台控制线程主体：独占 [`MpvIpcClient`]，处理控制消息并周期轮询播放状态。
///
/// 所有 IPC 调用均为阻塞式，故必须在此独立线程执行，UI 线程不受影响。
///
/// ## 连接鲁棒化（fix-preview-flash-and-misnamed-decode Task 2）
///
/// mpv 冷启动较慢，管道可能晚于旧实现的 ~1s 总预算才就绪。这里改为"连接失败不退出、
/// 循环内退避重试"：每次尝试用较短的单次预算（`connect(10, 50)` ≈ 500ms，缩短关闭时
/// `Drop` join 的等待），两次尝试间至少间隔 [`CONNECT_RETRY_INTERVAL`]，总重试上限
/// [`CONNECT_DEADLINE`]。达到上限仍未连上则记录一条日志并放弃连接，但线程仍继续消费
/// `rx` 与响应停止位 / `Disconnected`（退出时限为一次 `recv_timeout` 或一次 connect 单次预算）。
/// 连接成功前收到的控制消息直接丢弃（不积压）。
fn run_control_worker(
    pipe_name: String,
    rx: Receiver<ControlMsg>,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    weak: slint::Weak<crate::MainWindow>,
) {
    /// 控制消息接收超时：未连接时也周期醒来以推进连接重试决策。
    const RECV_TIMEOUT: Duration = Duration::from_millis(150);
    /// 两次连接尝试之间的最小间隔。
    const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(400);
    /// 连接总重试上限（自此放弃控制连接，不再重试）。
    const CONNECT_DEADLINE: Duration = Duration::from_secs(6);

    let mut client = MpvIpcClient::new(&pipe_name);
    let mut connected = false;
    let mut gave_up = false;
    let connect_deadline = Instant::now() + CONNECT_DEADLINE;
    let mut last_attempt: Option<Instant> = None;

    let mut last_pause = false;
    let mut last_dur = 0.0f64;
    let mut last_pos = 0.0f64;
    let mut last_poll = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(msg) => {
                // 未连接时无法发送，直接丢弃以避免通道积压。
                if connected {
                    let (cmd, new_pause) = control_to_command(&msg, last_pause);
                    let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
                    // 空命令（如非有限音量的防护分支）不下发，避免向 mpv 发送空 argv。
                    if argv.is_empty() {
                        continue;
                    }
                    if let Err(e) = client.send_command_no_wait(&argv) {
                        log(format!("预览 mpv 控制命令失败：{e}"));
                    }
                    last_pause = new_pause;
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // 未连接：退避重试连接（放弃后仅继续消费消息 / 等待停止位）。
        if !connected {
            if gave_up {
                continue;
            }
            match next_connect_action(
                false,
                Instant::now(),
                connect_deadline,
                last_attempt,
                CONNECT_RETRY_INTERVAL,
            ) {
                ConnectAction::Attempt => {
                    last_attempt = Some(Instant::now());
                    // 单次预算 connect(10, 50) ≈ 500ms；成功前不轮询。
                    if let Err(e) = client.connect(10, 50) {
                        // 未达上限时保持静默（避免刷屏），仅由上限分支记录一条日志。
                        let _ = e;
                    } else {
                        connected = true;
                        log("预览 mpv IPC 已连接");
                    }
                }
                ConnectAction::Wait => {}
                ConnectAction::GiveUp => {
                    gave_up = true;
                    log(format!(
                        "预览 mpv IPC 连接失败：已达重试上限（{}s），放弃播放控制",
                        CONNECT_DEADLINE.as_secs()
                    ));
                }
            }
            continue;
        }

        // 轮询节流：至少间隔 250ms 才查询一次，避免高频 get_property 堵塞 IPC。
        if last_poll.elapsed() < Duration::from_millis(250) {
            continue;
        }
        last_poll = Instant::now();

        // 查询失败时保留上一次已知值（不清零），避免状态回投出现跳变。
        let pos = match client.get_property_with_timeout("time-pos", Duration::from_millis(400)) {
            Ok(v) => sanitize_media_time(v.as_f64().unwrap_or(0.0)),
            Err(_) => last_pos,
        };
        let dur = match client.get_property_with_timeout("duration", Duration::from_millis(400)) {
            Ok(v) => sanitize_media_time(v.as_f64().unwrap_or(0.0)),
            Err(_) => last_dur,
        };
        let pause = match client.get_property_with_timeout("pause", Duration::from_millis(400)) {
            Ok(v) => parse_pause(&v),
            Err(_) => last_pause,
        };
        last_pos = pos;
        last_dur = dur;
        last_pause = pause;

        // 首帧就绪判定：优先 vo-configured；兜底 duration/time-pos > 0。就绪后不再查询。
        if !ready.load(Ordering::Relaxed) {
            let vo_configured = client
                .get_property_with_timeout("vo-configured", Duration::from_millis(400))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if vo_configured || dur > 0.0 || pos > 0.0 {
                ready.store(true, Ordering::Relaxed);
            }
        }

        let progress = progress_fraction(pos, dur);
        let pos_text = format_clock(pos);
        let dur_text = format_clock(dur);
        let playing = !pause;
        // Weak 需按轮次克隆：闭包要求 'static，直接 move 会跨循环迭代失效。
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                // 仅当仍处于视频预览时才写回（预览可能已被关闭/切换）。
                if !ui.get_preview_id().is_empty() && ui.get_preview_kind() == 1 {
                    ui.set_preview_playing(playing);
                    ui.set_preview_dur(dur as f32);
                    ui.set_preview_progress(progress);
                    ui.set_preview_pos_text(slint::SharedString::from(pos_text));
                    ui.set_preview_dur_text(slint::SharedString::from(dur_text));
                }
            }
        });
    }
    // 循环退出后 client 被 Drop → 自动 disconnect。
}

/// 连接重试决策（纯函数，便于单测）。
///
/// - `connected == true` → [`ConnectAction::Wait`]（已连接无需再决策）。
/// - 已达 `deadline` → [`ConnectAction::GiveUp`]。
/// - 距上次尝试不足 `retry_interval`（或从未尝试过之外的情形）→ [`ConnectAction::Wait`]。
/// - 其余情况 → [`ConnectAction::Attempt`]。
#[derive(Debug, PartialEq, Eq)]
enum ConnectAction {
    Attempt,
    Wait,
    GiveUp,
}

fn next_connect_action(
    connected: bool,
    now: Instant,
    deadline: Instant,
    last_attempt: Option<Instant>,
    retry_interval: Duration,
) -> ConnectAction {
    if connected {
        return ConnectAction::Wait;
    }
    if now >= deadline {
        return ConnectAction::GiveUp;
    }
    match last_attempt {
        Some(t) if now.duration_since(t) < retry_interval => ConnectAction::Wait,
        _ => ConnectAction::Attempt,
    }
}

/// 首帧就绪门控判定（纯函数，便于单测）：未显示且（已就绪或已达兜底上限）才应显示。
fn should_show(shown: bool, ready: bool, elapsed: Duration, fallback: Duration) -> bool {
    !shown && (ready || elapsed >= fallback)
}

/// 解析 mpv `pause` 属性值（兼容 JSON 布尔与 `yes`/`no` 字符串）。
fn parse_pause(value: &serde_json::Value) -> bool {
    match value.as_bool() {
        Some(b) => b,
        None => matches!(value.as_str(), Some("yes")),
    }
}

/// 把控制消息转换为 mpv IPC 命令参数与更新后的 `last_pause`（纯函数，便于单测）。
fn control_to_command(msg: &ControlMsg, last_pause: bool) -> (Vec<String>, bool) {
    match msg {
        ControlMsg::TogglePlay => {
            let new_pause = !last_pause;
            (
                vec![
                    "set_property".to_string(),
                    "pause".to_string(),
                    if new_pause { "yes" } else { "no" }.to_string(),
                ],
                new_pause,
            )
        }
        ControlMsg::Seek(secs) => (
            vec![
                "set_property".to_string(),
                "time-pos".to_string(),
                secs.to_string(),
            ],
            last_pause,
        ),
        ControlMsg::SetSpeed(speed) => (
            vec![
                "set_property".to_string(),
                "speed".to_string(),
                speed.to_string(),
            ],
            last_pause,
        ),
        ControlMsg::SetVolume(v) => {
            // 非有限值（NaN/∞）不产生命令：返回空 argv 交由工作线程跳过，避免污染 mpv 音量。
            if !v.is_finite() {
                return (Vec::new(), last_pause);
            }
            (
                vec![
                    "set_property".to_string(),
                    "volume".to_string(),
                    v.clamp(0.0, 100.0).to_string(),
                ],
                last_pause,
            )
        }
        ControlMsg::SetMute(m) => (
            vec![
                "set_property".to_string(),
                "mute".to_string(),
                if *m { "yes" } else { "no" }.to_string(),
            ],
            last_pause,
        ),
    }
}

/// 清洗媒体时间：非有限或负数一律归零（mpv 未就绪时可能返回 null/NaN）。
fn sanitize_media_time(v: f64) -> f64 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

/// 秒 → `m:ss`（≥1h 用 `h:mm:ss`）；负数/非有限按 0。
pub fn format_clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds.floor() as u64
    } else {
        0
    };
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 播放进度比例（0..1）：`dur<=0` 或非有限 → 0；结果夹到 `[0,1]`。
pub fn progress_fraction(pos: f64, dur: f64) -> f32 {
    if !dur.is_finite() || dur <= 0.0 {
        return 0.0;
    }
    let f = (pos / dur).clamp(0.0, 1.0);
    if f.is_finite() {
        f as f32
    } else {
        0.0
    }
}

/// 构建预览 mpv 参数（纯函数，便于单测）。最后一项是视频路径。
fn build_mpv_args(
    host: HWND,
    pipe_name: &str,
    video: &str,
    volume: f32,
    muted: bool,
) -> Vec<String> {
    let mut args = vec![
        "--no-config".to_string(),
        "--no-input-default-bindings".to_string(),
        "--input-vo-keyboard=no".to_string(),
        "--input-media-keys=no".to_string(),
        "--no-terminal".to_string(),
        "--no-input-terminal".to_string(),
        "--loop-file=yes".to_string(),
        "--no-osc".to_string(),
        "--no-osd-bar".to_string(),
        "--keep-open=yes".to_string(),
        // 禁止 mpv 自行改变窗口尺寸/位置（Task 8.2 竞态结论）
        "--auto-window-resize=no".to_string(),
        "--hwdec=auto".to_string(),
        "--vo=gpu".to_string(),
        format!("--volume={}", volume.clamp(0.0, 100.0)),
        // 不抢焦点、不处理键盘（避免预览期间窃取主窗口交互）；播放控制改走 IPC。
        "--focus-on=never".to_string(),
        // 离屏初始定位（对照壁纸侧根因 G）：mpv 顶层窗口先在屏幕外创建，避免嵌入/首帧
        // 就绪前在桌面左上角闪现。
        "--geometry=+32000+32000".to_string(),
        // 播放控制 IPC 通道：mpv 会创建同名命名管道供 MpvIpcClient 连接。
        format!(r"--input-ipc-server=\\.\pipe\{pipe_name}"),
    ];
    // 启动即静音（有声音默认由 `--volume` 决定）。置于视频路径之前，避免依赖
    // "选项可出现在位置参数之后" 的解析行为。
    if muted {
        args.push("--mute=yes".to_string());
    }
    args.push(format!("--wid={}", host.0 as usize));
    args.push(video.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HWND;

    #[test]
    fn mpv_args_pin_host_and_loop_without_force_window() {
        let host = HWND(0x1234 as *mut core::ffi::c_void);
        let args = build_mpv_args(
            host,
            "mirrorstar-preview-mpv-1-0",
            r"C:\videos\a.mp4",
            50.0,
            false,
        );
        assert!(
            args.iter().any(|a| a == "--wid=4660"),
            "应带十进制宿主句柄：{args:?}"
        );
        assert!(args.iter().any(|a| a == "--auto-window-resize=no"));
        assert!(args.iter().any(|a| a == "--focus-on=never"));
        assert!(
            args.iter().any(|a| a == "--geometry=+32000+32000"),
            "应带离屏初始定位，避免顶层窗口闪现：{args:?}"
        );
        assert!(args.iter().any(|a| a == "--loop-file=yes"));
        assert!(!args.iter().any(|a| a.starts_with("--force-window")));
        assert_eq!(args.last().map(String::as_str), Some(r"C:\videos\a.mp4"));
    }

    #[test]
    fn mpv_args_carry_volume_and_omit_mute_when_unmuted() {
        let host = HWND(0x1234 as *mut core::ffi::c_void);
        let args = build_mpv_args(
            host,
            "mirrorstar-preview-mpv-1-0",
            r"C:\videos\a.mp4",
            50.0,
            false,
        );
        assert!(
            args.iter().any(|a| a == "--volume=50"),
            "应带音量参数：{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--volume=0"),
            "不应再硬编码静音：{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--mute=yes"),
            "未静音时不应带 --mute=yes：{args:?}"
        );
    }

    #[test]
    fn mpv_args_add_mute_when_muted() {
        let host = HWND(0x1234 as *mut core::ffi::c_void);
        let args = build_mpv_args(
            host,
            "mirrorstar-preview-mpv-1-0",
            r"C:\videos\a.mp4",
            30.0,
            true,
        );
        assert!(
            args.iter().any(|a| a == "--mute=yes"),
            "静音时应带 --mute=yes：{args:?}"
        );
        assert!(args.iter().any(|a| a == "--volume=30"));
    }

    #[test]
    fn mpv_args_include_ipc_server_pipe() {
        let host = HWND(0x1234 as *mut core::ffi::c_void);
        let args = build_mpv_args(
            host,
            "mirrorstar-preview-mpv-7-3",
            r"C:\videos\a.mp4",
            50.0,
            false,
        );
        assert!(
            args.iter()
                .any(|a| a.starts_with(r"--input-ipc-server=\\.\pipe\")),
            "应带 IPC 命名管道参数（用于播放控制）：{args:?}"
        );
        assert!(args
            .iter()
            .any(|a| a == r"--input-ipc-server=\\.\pipe\mirrorstar-preview-mpv-7-3"));
    }

    #[test]
    fn control_to_command_toggle_from_playing_state() {
        // last_pause=false（正在播放）→ 切到暂停：命令为 "yes"，返回 true。
        let (cmd, new_pause) = control_to_command(&ControlMsg::TogglePlay, false);
        assert_eq!(cmd, vec!["set_property", "pause", "yes"]);
        assert!(new_pause);
    }

    #[test]
    fn control_to_command_toggle_from_paused_state() {
        // last_pause=true（已暂停）→ 切到播放：命令为 "no"，返回 false。
        let (cmd, new_pause) = control_to_command(&ControlMsg::TogglePlay, true);
        assert_eq!(cmd, vec!["set_property", "pause", "no"]);
        assert!(!new_pause);
    }

    #[test]
    fn control_to_command_seek_keeps_pause_state() {
        let (cmd, new_pause) = control_to_command(&ControlMsg::Seek(42.5), true);
        assert_eq!(cmd, vec!["set_property", "time-pos", "42.5"]);
        assert!(new_pause, "Seek 不应改变 last_pause");
    }

    #[test]
    fn control_to_command_set_speed_keeps_pause_state() {
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetSpeed(1.5), false);
        assert_eq!(cmd, vec!["set_property", "speed", "1.5"]);
        assert!(!new_pause, "SetSpeed 不应改变 last_pause");
        // 已暂停态下同样只改速度，不动暂停位。
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetSpeed(0.5), true);
        assert_eq!(cmd, vec!["set_property", "speed", "0.5"]);
        assert!(new_pause, "SetSpeed 不应改变 last_pause");
    }

    #[test]
    fn control_to_command_set_volume_keeps_pause_state() {
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetVolume(50.0), false);
        assert_eq!(cmd, vec!["set_property", "volume", "50"]);
        assert!(!new_pause, "SetVolume 不应改变 last_pause");
        // 已暂停态同样只改音量。
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetVolume(33.5), true);
        assert_eq!(cmd, vec!["set_property", "volume", "33.5"]);
        assert!(new_pause);
        // 越界值先被夹取。
        let (cmd, _) = control_to_command(&ControlMsg::SetVolume(150.0), false);
        assert_eq!(cmd, vec!["set_property", "volume", "100"]);
    }

    #[test]
    fn control_to_command_set_volume_rejects_non_finite() {
        // NaN / ∞ → 空命令且 last_pause 不变。
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetVolume(f32::NAN), false);
        assert!(cmd.is_empty());
        assert!(!new_pause);
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetVolume(f32::INFINITY), true);
        assert!(cmd.is_empty());
        assert!(new_pause);
    }

    #[test]
    fn control_to_command_set_mute_keeps_pause_state() {
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetMute(true), false);
        assert_eq!(cmd, vec!["set_property", "mute", "yes"]);
        assert!(!new_pause, "SetMute 不应改变 last_pause");
        let (cmd, new_pause) = control_to_command(&ControlMsg::SetMute(false), true);
        assert_eq!(cmd, vec!["set_property", "mute", "no"]);
        assert!(new_pause);
    }

    #[test]
    fn sanitize_media_time_rejects_non_finite_and_negative() {
        assert_eq!(sanitize_media_time(12.5), 12.5);
        assert_eq!(sanitize_media_time(0.0), 0.0);
        assert_eq!(sanitize_media_time(-3.0), 0.0);
        assert_eq!(sanitize_media_time(f64::NAN), 0.0);
        assert_eq!(sanitize_media_time(f64::INFINITY), 0.0);
    }

    #[test]
    fn format_clock_covers_minutes_hours_and_invalid() {
        assert_eq!(format_clock(0.0), "0:00");
        assert_eq!(format_clock(59.0), "0:59");
        assert_eq!(format_clock(61.0), "1:01");
        assert_eq!(format_clock(3661.0), "1:01:01");
        assert_eq!(format_clock(-5.0), "0:00");
        assert_eq!(format_clock(f64::NAN), "0:00");
    }

    #[test]
    fn progress_fraction_handles_zero_overrun_and_normal() {
        assert_eq!(progress_fraction(10.0, 0.0), 0.0);
        assert_eq!(progress_fraction(10.0, -1.0), 0.0);
        assert_eq!(progress_fraction(50.0, 25.0), 1.0, "超过总时长应夹到 1.0");
        assert_eq!(progress_fraction(25.0, 100.0), 0.25);
    }

    #[test]
    fn parse_pause_accepts_bool_and_string() {
        assert!(parse_pause(&serde_json::json!(true)));
        assert!(!parse_pause(&serde_json::json!(false)));
        assert!(parse_pause(&serde_json::json!("yes")));
        assert!(!parse_pause(&serde_json::json!("no")));
        assert!(!parse_pause(&serde_json::Value::Null));
    }

    #[test]
    fn next_connect_action_attempts_before_deadline_when_interval_elapsed() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(6);
        let last_attempt = Some(now - Duration::from_millis(500));
        assert_eq!(
            next_connect_action(
                false,
                now,
                deadline,
                last_attempt,
                Duration::from_millis(400)
            ),
            ConnectAction::Attempt,
            "未连接、未到 deadline 且距上次尝试已超过间隔 → Attempt"
        );
        // 从未尝试过也应立即 Attempt。
        assert_eq!(
            next_connect_action(false, now, deadline, None, Duration::from_millis(400)),
            ConnectAction::Attempt
        );
    }

    #[test]
    fn next_connect_action_waits_when_interval_not_elapsed() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(6);
        let last_attempt = Some(now - Duration::from_millis(100));
        assert_eq!(
            next_connect_action(
                false,
                now,
                deadline,
                last_attempt,
                Duration::from_millis(400)
            ),
            ConnectAction::Wait,
            "距上次尝试过近 → Wait"
        );
    }

    #[test]
    fn next_connect_action_gives_up_at_deadline() {
        let now = Instant::now();
        let deadline = now;
        assert_eq!(
            next_connect_action(
                false,
                now,
                deadline,
                Some(now - Duration::from_millis(500)),
                Duration::from_millis(400)
            ),
            ConnectAction::GiveUp,
            "已达 deadline → GiveUp"
        );
    }

    #[test]
    fn next_connect_action_waits_when_already_connected() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(6);
        assert_eq!(
            next_connect_action(true, now, deadline, None, Duration::from_millis(400)),
            ConnectAction::Wait,
            "已连接 → Wait"
        );
    }

    #[test]
    fn should_show_only_when_ready_or_fallback_elapsed() {
        let fallback = Duration::from_millis(1200);
        // 未就绪、未超时 → 不显示。
        assert!(!should_show(
            false,
            false,
            Duration::from_millis(300),
            fallback
        ));
        // 已就绪 → 显示。
        assert!(should_show(
            false,
            true,
            Duration::from_millis(300),
            fallback
        ));
        // 超时兜底 → 显示。
        assert!(should_show(
            false,
            false,
            Duration::from_millis(1500),
            fallback
        ));
        // 已显示 → 幂等不重复。
        assert!(!should_show(
            true,
            true,
            Duration::from_millis(1500),
            fallback
        ));
    }
}
