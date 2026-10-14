//! 性能埋点工具（v17 性能优化专项）
//!
//! 轻量级、低开销的测量工具，通过 tracing 输出到日志文件
//! 数据根下 `logs/mirrorstar.log`。所有埋点使用 target
//! `mirrorstar::perf`，可用 `RUST_LOG=mirrorstar::perf=info` 单独过滤分析。
//!
//! 设计原则：测量驱动，避免盲目优化。埋点本身开销极小（一次 Instant 取值
//! + 少量整数累加），不影响被测路径性能；报告仅在达到阈值时输出一次日志。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

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

/// MB 换算常量：字节 → MB。
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// 归因最小采样间隔（30 秒）。调用方传入更小间隔时抬升到该值，避免高频系统调用。
pub const MIN_MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// 进程类别：按可执行文件名把应用进程树中的进程归类。
///
/// - `Main`：主进程自身（进程树根）
/// - `Mpv`：`mpv.exe`（视频解码）
/// - `WpProc`：与本进程**同 exe 名**的网页壁纸子进程（壳 exe 的 `--wp-proc` 模式；
///   进程树根除外，根固定归 `Main`）
/// - `WebView2`：`msedgewebview2.exe`（主窗口 / 子进程的 WebView2 多进程）
/// - `Other`：其余后代进程（含 conhost 等），便于发现遗漏
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessCategory {
    Main,
    Mpv,
    WpProc,
    WebView2,
    Other,
}

impl ProcessCategory {
    /// 按可执行文件名（大小写不敏感）归类；非目标名归入 `Other`。
    ///
    /// 「同一 exe、双入口」后 wp-proc 与本进程同 exe 名，无法再按固定字面量匹配，
    /// 故需传入本进程 exe 名 `self_exe_name`：exe 名与之相同者（且非进程树根，
    /// 根由调用方固定归 `Main`）归为 `WpProc`。`self_exe_name` 为空串时不匹配
    /// （退化为 `Other`，避免误归类）。
    fn from_exe_name(name: &str, self_exe_name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "mpv.exe" => ProcessCategory::Mpv,
            "msedgewebview2.exe" => ProcessCategory::WebView2,
            _ => {
                let self_lower = self_exe_name.to_ascii_lowercase();
                if !self_lower.is_empty() && lower == self_lower {
                    ProcessCategory::WpProc
                } else {
                    ProcessCategory::Other
                }
            }
        }
    }
}

/// 获取本进程可执行文件名（用于 `WpProc` 归因）。
///
/// 取不到时返回空串，此时同 exe 名的后代不被归为 `WpProc`（退化为 `Other`）。
fn current_exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// 单个进程类别内存归因统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CategoryUsage {
    /// 该类别的进程数
    pub process_count: u32,
    /// 工作集合计（WorkingSetSize），单位 MB
    pub working_set_mb: f64,
    /// 私有提交合计（PrivateUsage），单位 MB
    pub private_mb: f64,
}

/// 应用组逐子进程内存归因报告。
///
/// 各字段分别对应一类进程的合计；`total` 为所有相关进程之和
/// （即各类别之和），代表应用组的整体内存占用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct MemoryUsageReport {
    /// 主进程自身
    pub main_process: CategoryUsage,
    /// `mpv.exe`
    pub mpv: CategoryUsage,
    /// 与本进程同 exe 名的网页壁纸子进程（壳 exe 的 `--wp-proc` 模式）
    pub wp_proc: CategoryUsage,
    /// `msedgewebview2.exe`
    pub webview2: CategoryUsage,
    /// 其他后代进程
    pub other: CategoryUsage,
    /// 应用组总计（各类别之和）
    pub total: CategoryUsage,
}

/// 进程快照条目（纯数据结构，便于单测构造）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessEntry {
    /// 进程 ID
    pid: u32,
    /// 父进程 ID
    ppid: u32,
    /// 可执行文件名（如 `mpv.exe`）
    name: String,
}

/// 单进程内存采样（纯数据结构，便于单测构造）。
#[derive(Debug, Clone, Copy)]
struct ProcSample {
    category: ProcessCategory,
    working_set_mb: f64,
    private_mb: f64,
}

/// RAII 守卫：确保 Win32 句柄在 drop 时调用 `CloseHandle`，避免句柄泄漏。
///
/// 与 `audio::volume` / `wallpaper::mod` 中的同类守卫模式保持一致。
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: CloseHandle 对无效句柄返回错误但不 UB；句柄即将丢弃，错误无实际影响。
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 纯逻辑：以 `root_pid` 为根，从全量进程列表中提取后代进程集合（含根自身）并归类。
///
/// 通过父 pid 关系迭代扩张：任意排序的进程列表都能正确构建后代集合；
/// 使用 `included` 集合去重，天然处理环/重复父 pid，不会死循环。
/// 根进程固定归类为 `Main`；后代按可执行文件名归类（exe 名与本进程 exe 名
/// `self_exe_name` 相同的后代归 `WpProc`）。
/// 非本进程树后代的进程（如其他应用的 `msedgewebview2.exe`）不会被计入。
fn attribute_processes(
    processes: &[ProcessEntry],
    root_pid: u32,
    self_exe_name: &str,
) -> Vec<(u32, ProcessCategory)> {
    let mut result = vec![(root_pid, ProcessCategory::Main)];
    let mut included = std::collections::HashSet::new();
    included.insert(root_pid);

    // 反复扫描直到一轮内没有新增，兼容任意排序的进程列表
    loop {
        let mut changed = false;
        for p in processes {
            if included.contains(&p.pid) {
                continue;
            }
            if included.contains(&p.ppid) {
                included.insert(p.pid);
                result.push((
                    p.pid,
                    ProcessCategory::from_exe_name(&p.name, self_exe_name),
                ));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// 纯逻辑：把逐进程采样聚合为分类别报告。空输入返回全 0 报告。
/// `total` 累加全部样本，故恒等于各类别之和。
fn aggregate(samples: &[ProcSample]) -> MemoryUsageReport {
    let mut report = MemoryUsageReport::default();
    for s in samples {
        let slot = match s.category {
            ProcessCategory::Main => &mut report.main_process,
            ProcessCategory::Mpv => &mut report.mpv,
            ProcessCategory::WpProc => &mut report.wp_proc,
            ProcessCategory::WebView2 => &mut report.webview2,
            ProcessCategory::Other => &mut report.other,
        };
        slot.process_count += 1;
        slot.working_set_mb += s.working_set_mb;
        slot.private_mb += s.private_mb;

        report.total.process_count += 1;
        report.total.working_set_mb += s.working_set_mb;
        report.total.private_mb += s.private_mb;
    }
    report
}

/// 枚举系统全部进程（pid / 父 pid / 可执行文件名）。
///
/// 失败时返回空列表并记日志，不 panic（调用方按"无匹配进程"处理）。
fn enumerate_processes() -> Vec<ProcessEntry> {
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(target: "mirrorstar::perf", error = ?e, "内存归因：创建进程快照失败");
            return Vec::new();
        }
    };
    let _snapshot_guard = HandleGuard(snapshot);

    // dwSize 必须正确设置，枚举 API 依赖它返回实际写入长度
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        tracing::warn!(target: "mirrorstar::perf", "内存归因：Process32FirstW 失败");
        return Vec::new();
    }

    let mut processes = Vec::new();
    loop {
        // 宽字符进程名转 String（遇 NUL 截断；极限情况 MAX_PATH 宽字符全满）
        let name_len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        processes.push(ProcessEntry {
            pid: entry.th32ProcessID,
            ppid: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..name_len]),
        });
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            break;
        }
    }
    processes
}

/// 取指定进程的工作集与私有提交（MB）。`OpenProcess` 失败（进程已退出/无权限）
/// 时返回 `None`，由调用方跳过并记日志，不 panic。
fn process_memory_mb(pid: u32) -> Option<(f64, f64)> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let _guard = HandleGuard(handle);

    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb,
        ..Default::default()
    };
    let ok = unsafe { GetProcessMemoryInfo(handle, &mut counters as *mut _ as *mut _, cb) };
    if ok.is_ok() {
        Some((
            counters.WorkingSetSize as f64 / BYTES_PER_MB,
            counters.PrivateUsage as f64 / BYTES_PER_MB,
        ))
    } else {
        None
    }
}

/// 采集一次应用组逐子进程内存归因报告（同步 Win32 调用，开销很小）。
///
/// 归因限定在当前进程进程树内：以当前 pid 为根，按父 pid 关系取后代进程集合，
/// 再按可执行文件名归类并逐进程取内存计数器，因此不会误算系统上其他
/// 应用的 `msedgewebview2.exe`。单个进程打开失败时跳过并记日志。
pub fn sample_memory_report() -> MemoryUsageReport {
    let processes = enumerate_processes();
    let root_pid = unsafe { GetCurrentProcessId() };
    // 「同一 exe、双入口」后 wp-proc 与本进程同 exe 名，需获取一次本进程 exe 名
    // 并向下传递，供后代归类为 `WpProc`。
    let self_exe_name = current_exe_name();
    let attributed = attribute_processes(&processes, root_pid, &self_exe_name);

    let mut samples = Vec::with_capacity(attributed.len());
    for (pid, category) in attributed {
        match process_memory_mb(pid) {
            Some((working_set_mb, private_mb)) => samples.push(ProcSample {
                category,
                working_set_mb,
                private_mb,
            }),
            None => tracing::warn!(
                target: "mirrorstar::perf",
                pid,
                category = ?category,
                "内存归因：打开进程失败，跳过该进程"
            ),
        }
    }
    aggregate(&samples)
}

/// 输出一条归因日志（target `mirrorstar::perf`，每次一行，便于日志分析对比）。
fn log_memory_report(report: &MemoryUsageReport) {
    let c = |u: &CategoryUsage| {
        (
            u.process_count,
            format!("{:.1}", u.working_set_mb),
            format!("{:.1}", u.private_mb),
        )
    };
    let (main_n, main_ws, main_pv) = c(&report.main_process);
    let (mpv_n, mpv_ws, mpv_pv) = c(&report.mpv);
    let (wp_n, wp_ws, wp_pv) = c(&report.wp_proc);
    let (wv_n, wv_ws, wv_pv) = c(&report.webview2);
    let (other_n, other_ws, other_pv) = c(&report.other);
    let (total_n, total_ws, total_pv) = c(&report.total);

    tracing::info!(
        target: "mirrorstar::perf",
        main_procs = main_n,
        main_ws_mb = main_ws,
        main_private_mb = main_pv,
        mpv_procs = mpv_n,
        mpv_ws_mb = mpv_ws,
        mpv_private_mb = mpv_pv,
        wp_proc_procs = wp_n,
        wp_proc_ws_mb = wp_ws,
        wp_proc_private_mb = wp_pv,
        webview2_procs = wv_n,
        webview2_ws_mb = wv_ws,
        webview2_private_mb = wv_pv,
        other_procs = other_n,
        other_ws_mb = other_ws,
        other_private_mb = other_pv,
        total_procs = total_n,
        total_ws_mb = total_ws,
        total_private_mb = total_pv,
        "MEM-ATTRIB: 应用组逐子进程内存归因"
    );
}

/// 启动后台周期采样线程：每 `interval` 输出一条归因日志（target `mirrorstar::perf`）。
///
/// - `interval` 会被抬升到 [`MIN_MEMORY_SAMPLE_INTERVAL`]（30 秒）以上，保证低开销
/// - 返回 `JoinHandle` 供调用方按需持有；直接丢弃即变为 detached 线程，
///   进程退出时随之终止。线程创建失败时记录日志并返回 `None`，不 panic。
///
/// 该函数不依赖 Tauri，可在 `mirrorstar-core` 中被上层自由调用。
pub fn spawn_memory_attribution_sampler(interval: Duration) -> Option<std::thread::JoinHandle<()>> {
    let interval = interval.max(MIN_MEMORY_SAMPLE_INTERVAL);
    let builder = std::thread::Builder::new().name("mem-attrib-sampler".to_string());
    let handle = builder.spawn(move || loop {
        std::thread::sleep(interval);
        log_memory_report(&sample_memory_report());
    });
    match handle {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(target: "mirrorstar::perf", error = %e, "内存归因：采样线程启动失败");
            None
        }
    }
}

// ── 启动期细粒度内存时间线与生命周期事件打点 ──────────────────────────────────

/// 启动期埋点开关环境变量名：值为 `0` 或 `off`（大小写不敏感、忽略首尾空白）时关闭。
///
/// **默认关闭（opt-in）**：仅当显式设为非 `0` / `off` 值时才开启，用于启动期前 60 秒
/// 采集内存时间线；未设置时不做任何启动期内存采样。
const MEM_TRACE_ENV: &str = "MIRRORSTAR_MEM_TRACE";

/// 进程启动基准时刻，惰性初始化：首次调用任意埋点 API 时记录。
///
/// 之后所有"相对启动耗时"均以该时刻为原点。
static STARTUP_INSTANT: OnceLock<Instant> = OnceLock::new();

/// 返回惰性初始化的进程启动基准时刻（首次调用时创建）。
fn startup_instant() -> Instant {
    *STARTUP_INSTANT.get_or_init(Instant::now)
}

/// 相对启动基准的耗时（毫秒）。首次调用会初始化基准时刻，故首次返回值约为 0。
///
/// 非阻塞、无系统调用，可在任意线程安全调用。
pub fn startup_elapsed_ms() -> u128 {
    startup_instant().elapsed().as_millis()
}

// ── 启动关键路径阶段耗时埋点（与内存采样完全无关，始终启用） ────────────────────

/// 启动关键路径阶段打点记录：(阶段标签, 相对进程基线耗时 ms)，按记录顺序追加。
///
/// 供 [`emit_boot_summary`] 汇总时检索；锁中毒等异常场景静默跳过，不影响启动。
static BOOT_STAGES: Mutex<Vec<(String, u128)>> = Mutex::new(Vec::new());

/// 记录一个启动关键路径阶段并立即输出一条可检索日志（target `mirrorstar::perf`）。
///
/// 相对进程基线（由首次调用惰性建立，见 [`startup_instant`]）取耗时，因此进程内
/// 首个调用（约定为 `run()` 开头的 `process_start`）耗时约为 0。**与内存采样完全
/// 无关**：不做进程树枚举、不调用 [`sample_memory_report`] / [`log_mem_trace`]，仅一次
/// `Instant` 取值 + 一次短锁追加，开销极小、非阻塞。**始终启用**，不受
/// [`MEM_TRACE_ENV`] 控制；失败不 panic。
pub fn mark_boot_stage(label: &str) {
    let elapsed_ms = startup_elapsed_ms();
    if let Ok(mut stages) = BOOT_STAGES.lock() {
        stages.push((label.to_string(), elapsed_ms));
    }
    tracing::info!(
        target: "mirrorstar::perf",
        stage = label,
        stage_ms = elapsed_ms,
        "PERF-BOOT: {}",
        label
    );
}

/// 启动关键路径各阶段耗时汇总：输出**一条**汇总日志（target `mirrorstar::perf`）。
///
/// 字段名稳定、可检索，便于从日志提取各阶段耗时：`process_start_ms`、`log_init_ms`、
/// `phase1_ready_ms`、`setup_ready_ms`、`scheduler_started_ms`、`boot_wallpaper_applied_ms`。
/// 汇总时尚未到达的阶段记 `-1`。与 [`mark_boot_stage`] 一样**始终启用**、非阻塞、失败不 panic。
pub fn emit_boot_summary() {
    let stages = BOOT_STAGES.lock().map(|s| s.clone()).unwrap_or_default();
    let stage_ms = |label: &str| -> i64 {
        stages
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, ms)| *ms as i64)
            .unwrap_or(-1)
    };
    tracing::info!(
        target: "mirrorstar::perf",
        process_start_ms = stage_ms("process_start"),
        log_init_ms = stage_ms("log_init"),
        phase1_ready_ms = stage_ms("phase1_ready"),
        setup_ready_ms = stage_ms("setup_ready"),
        scheduler_started_ms = stage_ms("scheduler_started"),
        boot_wallpaper_applied_ms = stage_ms("boot_wallpaper_applied"),
        "PERF-BOOT: 启动阶段耗时汇总"
    );
}

/// 解析内存埋点开关：未设置时**默认关闭**；显式设为 `0` / `off`（大小写不敏感、允许
/// 首尾空白）同样关闭，显式设为其它任意值才开启。抽为纯函数便于单测判定逻辑。
fn parse_mem_trace_enabled(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off")
        }
        None => false,
    }
}

/// 读取 [`MEM_TRACE_ENV`] 环境变量判定内存埋点是否开启（未设置 / `0` / `off` 时关闭）。
fn mem_trace_enabled() -> bool {
    parse_mem_trace_enabled(std::env::var(MEM_TRACE_ENV).ok().as_deref())
}

/// 输出一条带前缀 / 事件标签 / 相对启动耗时的分类别内存日志（target `mirrorstar::perf`）。
///
/// 字段与既有 `MEM-ATTRIB` 保持一致（分类别工作集/私有提交与总计），额外携带
/// `label`（事件标签）与 `elapsed_ms`（相对启动耗时）。供启动期时间线与
/// [`mark_lifecycle`] 复用。
fn log_mem_trace(prefix: &str, label: &str, elapsed_ms: u128, report: &MemoryUsageReport) {
    let c = |u: &CategoryUsage| {
        (
            u.process_count,
            format!("{:.1}", u.working_set_mb),
            format!("{:.1}", u.private_mb),
        )
    };
    let (main_n, main_ws, main_pv) = c(&report.main_process);
    let (mpv_n, mpv_ws, mpv_pv) = c(&report.mpv);
    let (wp_n, wp_ws, wp_pv) = c(&report.wp_proc);
    let (wv_n, wv_ws, wv_pv) = c(&report.webview2);
    let (other_n, other_ws, other_pv) = c(&report.other);
    let (total_n, total_ws, total_pv) = c(&report.total);

    tracing::info!(
        target: "mirrorstar::perf",
        label = label,
        elapsed_ms = elapsed_ms,
        main_procs = main_n,
        main_ws_mb = main_ws,
        main_private_mb = main_pv,
        mpv_procs = mpv_n,
        mpv_ws_mb = mpv_ws,
        mpv_private_mb = mpv_pv,
        wp_proc_procs = wp_n,
        wp_proc_ws_mb = wp_ws,
        wp_proc_private_mb = wp_pv,
        webview2_procs = wv_n,
        webview2_ws_mb = wv_ws,
        webview2_private_mb = wv_pv,
        other_procs = other_n,
        other_ws_mb = other_ws,
        other_private_mb = other_pv,
        total_procs = total_n,
        total_ws_mb = total_ws,
        total_private_mb = total_pv,
        "{}: 启动期分类别内存归因",
        prefix
    );
}

/// 纯函数：启动期时间线在相对启动耗时 `elapsed` 时是否应继续采样（`elapsed < total`）。
///
/// 抽为纯函数便于单测"时间线在总时长后停止"，避免测试真的睡满时长。
fn timeline_should_continue(elapsed: Duration, total: Duration) -> bool {
    elapsed < total
}

/// 启动后 `total` 时长内按 `interval` 间隔输出分类别内存时间线（target
/// `mirrorstar::perf`，message 前缀 `MEM-TIMELINE`）。
///
/// - 复用既有归因逻辑（进程树枚举 + `GetProcessMemoryInfo`），开销极小；**不持有**
///   引擎 / 桌面锁，不阻塞主线程
/// - 单次采样 best-effort：内部失败跳过并记日志，绝不 panic
/// - 到达 `total` 后线程自行结束，不影响既有 60 秒周期采样（`spawn_memory_attribution_sampler`）
/// - 返回 `JoinHandle` 供调用方按需持有；直接丢弃即 detached 线程。线程创建失败返回
///   `None`，不 panic
/// - 受 [`MEM_TRACE_ENV`] 开关控制（**默认关闭**）：未设置或为 `0` / `off` 时不启动线程，
///   直接返回 `None`
pub fn spawn_startup_memory_timeline(
    total: Duration,
    interval: Duration,
) -> Option<std::thread::JoinHandle<()>> {
    spawn_startup_memory_timeline_when(mem_trace_enabled(), total, interval)
}

/// [`spawn_startup_memory_timeline`] 的可注入开关版本（`enabled = false` 时不启动），
/// 便于单测"关闭开关不采集"而无需修改进程级环境变量。
fn spawn_startup_memory_timeline_when(
    enabled: bool,
    total: Duration,
    interval: Duration,
) -> Option<std::thread::JoinHandle<()>> {
    if !enabled {
        return None;
    }
    let builder = std::thread::Builder::new().name("mem-timeline".to_string());
    let handle = builder.spawn(move || loop {
        std::thread::sleep(interval);
        let elapsed_ms = startup_elapsed_ms();
        if !timeline_should_continue(Duration::from_millis(elapsed_ms as u64), total) {
            break;
        }
        log_mem_trace(
            "MEM-TIMELINE",
            "timeline",
            elapsed_ms,
            &sample_memory_report(),
        );
    });
    match handle {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(target: "mirrorstar::perf", error = %e, "启动期内存时间线：采样线程启动失败");
            None
        }
    }
}

/// 立即采集一次并输出生命周期事件打点（target `mirrorstar::perf`，message 前缀 `MEM-MARK`）。
///
/// 字段：事件标签 `label` + 相对启动耗时 `elapsed_ms` + 分类别内存。非阻塞、best-effort：
/// 内部采样失败不 panic；受 [`MEM_TRACE_ENV`] 开关控制，**默认关闭**（未设置 / `0` / `off`
/// 时不输出任何日志）。
pub fn mark_lifecycle(label: &str) {
    if !mem_trace_enabled() {
        return;
    }
    log_mem_trace(
        "MEM-MARK",
        label,
        startup_elapsed_ms(),
        &sample_memory_report(),
    );
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

    // ── 内存归因单元测试 ──

    /// 构造进程快照条目（测试辅助）
    fn entry(pid: u32, ppid: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid,
            name: name.to_string(),
        }
    }

    /// 归因报告总计恒等于各类别之和（进程数、工作集、私有提交）
    #[test]
    fn memory_report_total_equals_sum_of_categories() {
        let samples = vec![
            ProcSample {
                category: ProcessCategory::Main,
                working_set_mb: 120.5,
                private_mb: 80.0,
            },
            ProcSample {
                category: ProcessCategory::Mpv,
                working_set_mb: 60.0,
                private_mb: 40.0,
            },
            ProcSample {
                category: ProcessCategory::WpProc,
                working_set_mb: 50.0,
                private_mb: 30.0,
            },
            ProcSample {
                category: ProcessCategory::WebView2,
                working_set_mb: 200.0,
                private_mb: 150.0,
            },
            ProcSample {
                category: ProcessCategory::WebView2,
                working_set_mb: 90.0,
                private_mb: 70.0,
            },
            ProcSample {
                category: ProcessCategory::Other,
                working_set_mb: 10.0,
                private_mb: 5.0,
            },
        ];
        let r = aggregate(&samples);

        let sum_count = r.main_process.process_count
            + r.mpv.process_count
            + r.wp_proc.process_count
            + r.webview2.process_count
            + r.other.process_count;
        let sum_ws = r.main_process.working_set_mb
            + r.mpv.working_set_mb
            + r.wp_proc.working_set_mb
            + r.webview2.working_set_mb
            + r.other.working_set_mb;
        let sum_pv = r.main_process.private_mb
            + r.mpv.private_mb
            + r.wp_proc.private_mb
            + r.webview2.private_mb
            + r.other.private_mb;

        assert_eq!(r.total.process_count, sum_count);
        assert!((r.total.working_set_mb - sum_ws).abs() < 1e-6);
        assert!((r.total.private_mb - sum_pv).abs() < 1e-6);
        // 同名类别应合并计数
        assert_eq!(r.webview2.process_count, 2);
        assert_eq!(r.total.process_count, 6);
    }

    /// 空采样集合：不 panic，各类别与总计均为 0
    #[test]
    fn aggregate_empty_returns_zero() {
        let r = aggregate(&[]);
        assert_eq!(r.total.process_count, 0);
        assert_eq!(r.total.working_set_mb, 0.0);
        assert_eq!(r.total.private_mb, 0.0);
        assert_eq!(r.main_process.process_count, 0);
        assert_eq!(r.webview2.process_count, 0);
    }

    /// 后代集合构建与归类：乱序输入也能正确建树，类别按文件名区分
    #[test]
    fn attribute_processes_builds_tree_and_categories() {
        // 「同一 exe、双入口」后 wp-proc（pid 102）与根同 exe 名，故据 self_exe_name 归为 WpProc。
        let processes = vec![
            // 故意乱序，验证迭代扩张不依赖输入顺序
            entry(104, 102, "msedgewebview2.exe"),
            entry(101, 100, "mpv.exe"),
            entry(100, 0, "mirrorstar-wallpaper.exe"),
            entry(105, 100, "conhost.exe"),
            entry(102, 100, "mirrorstar-wallpaper.exe"),
            entry(103, 100, "msedgewebview2.exe"),
        ];
        let mut result = attribute_processes(&processes, 100, "mirrorstar-wallpaper.exe");
        result.sort_by_key(|(pid, _)| *pid);

        let pids: Vec<u32> = result.iter().map(|(pid, _)| *pid).collect();
        assert_eq!(pids, vec![100, 101, 102, 103, 104, 105]);

        let cat = |pid: u32| result.iter().find(|(p, _)| *p == pid).unwrap().1;
        assert_eq!(cat(100), ProcessCategory::Main);
        assert_eq!(cat(101), ProcessCategory::Mpv);
        assert_eq!(cat(102), ProcessCategory::WpProc);
        assert_eq!(cat(103), ProcessCategory::WebView2);
        assert_eq!(cat(104), ProcessCategory::WebView2);
        assert_eq!(cat(105), ProcessCategory::Other);
    }

    /// 非本进程树的其他应用 msedgewebview2.exe 不得被计入
    #[test]
    fn attribute_processes_excludes_foreign_processes() {
        let processes = vec![
            entry(100, 0, "mirrorstar-wallpaper.exe"),
            entry(101, 100, "msedgewebview2.exe"),
            // 其他应用的 WebView2：父进程 999 不在本进程树内
            entry(500, 999, "msedgewebview2.exe"),
            entry(999, 0, "other-app.exe"),
        ];
        let result = attribute_processes(&processes, 100, "mirrorstar-wallpaper.exe");
        let pids: Vec<u32> = result.iter().map(|(pid, _)| *pid).collect();
        assert_eq!(pids, vec![100, 101]);
        assert!(
            !result.iter().any(|(pid, _)| *pid == 500),
            "外部应用的 msedgewebview2 不应被计入"
        );
    }

    /// 无匹配进程/空进程集合：不 panic，仅返回根进程（Main）
    #[test]
    fn attribute_processes_no_match_returns_root_only() {
        let result = attribute_processes(&[], 1234, "mirrorstar-wallpaper.exe");
        assert_eq!(result, vec![(1234, ProcessCategory::Main)]);
    }

    /// 类别判定大小写不敏感，未知名称归入 Other；
    /// 与本进程同 exe 名（大小写不敏感）归 WpProc；self_exe_name 为空串时不匹配。
    #[test]
    fn process_category_classifies_by_name_case_insensitive() {
        assert_eq!(
            ProcessCategory::from_exe_name("MPV.EXE", "mirrorstar-wallpaper.exe"),
            ProcessCategory::Mpv
        );
        assert_eq!(
            ProcessCategory::from_exe_name("MirrorStar-Wallpaper.exe", "mirrorstar-wallpaper.exe"),
            ProcessCategory::WpProc
        );
        assert_eq!(
            ProcessCategory::from_exe_name("msedgewebview2.exe", "mirrorstar-wallpaper.exe"),
            ProcessCategory::WebView2
        );
        assert_eq!(
            ProcessCategory::from_exe_name("unknown.exe", "mirrorstar-wallpaper.exe"),
            ProcessCategory::Other
        );
        // self_exe_name 为空串（current_exe 失败）时，同 exe 名不退化为 WpProc
        assert_eq!(
            ProcessCategory::from_exe_name("mirrorstar-wallpaper.exe", ""),
            ProcessCategory::Other
        );
    }

    /// 真实 Win32 采样冒烟测试：至少包含当前进程，且 total 恒等于各类别之和
    #[test]
    fn sample_memory_report_total_matches_categories() {
        let r = sample_memory_report();
        assert!(r.total.process_count >= 1, "至少应包含当前进程");
        let sum = r.main_process.working_set_mb
            + r.mpv.working_set_mb
            + r.wp_proc.working_set_mb
            + r.webview2.working_set_mb
            + r.other.working_set_mb;
        assert!((r.total.working_set_mb - sum).abs() < 1e-6);
    }

    // ── 启动期时间线 / 事件打点单元测试 ──

    /// 开关解析：未设置默认关闭（opt-in）；显式设为非 `0` / `off` 值才开启，`0` / `off`
    /// （含大小写与空白差异）关闭
    #[test]
    fn mem_trace_switch_defaults_off_and_explicit_values_enable() {
        assert!(!parse_mem_trace_enabled(None), "未设置时默认关闭");
        assert!(parse_mem_trace_enabled(Some("1")), "显式设为 1 应开启");
        assert!(parse_mem_trace_enabled(Some("on")), "显式设为 on 应开启");
        assert!(
            parse_mem_trace_enabled(Some("true")),
            "显式设为 true 应开启"
        );
        assert!(!parse_mem_trace_enabled(Some("0")), "值 0 应关闭");
        assert!(!parse_mem_trace_enabled(Some("off")), "值 off 应关闭");
        assert!(!parse_mem_trace_enabled(Some("OFF")), "大小写不敏感");
        assert!(!parse_mem_trace_enabled(Some(" 0 ")), "允许首尾空白");
    }

    /// 关闭开关时不启动时间线线程（不输出 MEM-TIMELINE）
    #[test]
    fn startup_timeline_disabled_spawns_nothing() {
        let handle = spawn_startup_memory_timeline_when(
            false,
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        assert!(handle.is_none(), "关闭开关时不应启动采样线程");
    }

    /// 时间线在总时长后停止（纯函数判定，无需真睡满时长）
    #[test]
    fn timeline_should_continue_stops_after_total() {
        let total = Duration::from_secs(60);
        assert!(timeline_should_continue(Duration::ZERO, total));
        assert!(timeline_should_continue(
            Duration::from_millis(59_999),
            total
        ));
        assert!(!timeline_should_continue(total, total));
        assert!(!timeline_should_continue(Duration::from_secs(61), total));
    }

    /// 事件打点对异常输入（空串 / 超长 / 含特殊字符）不 panic（best-effort）
    #[test]
    fn mark_lifecycle_handles_odd_labels_without_panic() {
        mark_lifecycle("");
        mark_lifecycle("boot_wallpaper_applied all:Video");
        mark_lifecycle(&"x".repeat(10_000));
    }

    /// 相对启动耗时不倒退（基准时刻惰性初始化后单调递增）
    #[test]
    fn startup_elapsed_ms_is_non_decreasing() {
        let a = startup_elapsed_ms();
        let b = startup_elapsed_ms();
        assert!(b >= a, "相对启动耗时不应倒退：{a} -> {b}");
    }

    /// 启动阶段打点对异常输入不 panic，汇总输出不 panic（始终启用，不受内存开关影响）
    #[test]
    fn mark_boot_stage_and_summary_do_not_panic() {
        mark_boot_stage("process_start");
        mark_boot_stage("");
        mark_boot_stage(&"y".repeat(10_000));
        emit_boot_summary();
    }
}
