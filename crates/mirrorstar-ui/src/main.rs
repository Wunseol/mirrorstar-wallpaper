//! MirrorStar 主窗口原生 UI（Slint 1.18，软件渲染）。
//!
//! ## 进程职责
//!
//! 本进程负责 UI：不注册托盘、不直接读写用户配置。所有业务数据都经 stdio NDJSON 协议
//! （[`mirrorstar_core::ui_proto`]）向 Tauri 壳请求或接收推送，是壳侧的只读消费者，托盘由
//! Tauri 进程持有。唯一的本地副作用是：为可见的视频卡片按需经 `mirrorstar_core` 视频后端
//! 生成首帧缩略图并写入缩略图缓存目录（见 `dispatch_video_thumbs`）。
//!
//! ## 虚拟化壁纸网格（Task 4）
//!
//! 库内条目数可以很大（验证过 1,000 项），但 Slint 没有内置窗口化。本实现的做法是：
//!
//! 1. **Rust 侧算窗口**（[`grid`]）：由 `Flickable.viewport-y` + 视口高度 + 条目总数算出
//!    行对齐的可见区间（上下各扩 `buffer_rows` 行），**只把这几十条**切片投影成 Slint 模型；
//! 2. **Slint 侧只实例化模型内条目**：`for card[i] in cards` 只为窗口内条目建实例，卡片用
//!    Rust 预算好的 `(row, col)` 绝对定位（`grid-pad + col*(w+gap)`），因此**不为全量条目建
//!    常驻实例**，`content-height` 由总行数决定，滚动条范围完整；
//! 3. **滚动驱动重建**：`viewport-y` 变化经回调上报，Rust 在"窗口区间或库版本变化"时才重建
//!    模型（小幅滚动不重建），旧窗口的图像随模型替换被丢弃，内存有界。
//!
//! ## 启动顺序（为什么是这个顺序）
//!
//! 1. 建 [`MainWindow`]：Slint 后端与事件循环 provider 在此就绪。
//! 2. 注册事件 / 控制 / 断开 / 滚动回调：必须在拉起读取线程**之前**完成。
//! 3. 同步握手：主线程独占 stdin 读一条 Hello（此刻读取线程尚未接管 stdin）。
//! 4. 拉起读取线程：**把同一个 `BufReader` 交出去**——握手用的 `BufReader` 可能已预读了
//!    后续字节，另建 reader 会丢缓冲。
//! 5. 拉取全量快照：响应经事件循环投递，在 `run_event_loop()` 启动后逐个落地。
//! 6. 进入事件循环。

// release 使用 GUI 子系统，避免壳（GUI 进程）拉起本进程时弹出多余的控制台窗口。
// 调试构建保留控制台，便于直接观察 stderr 日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::BufReader;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::{ComponentHandle, DataTransfer, Image, ModelRc, SharedString, VecModel};

use mirrorstar_core::config::settings::{AppConfig, Arrangement, RotationConfig};
use mirrorstar_core::config::{DisplayInfo, Pool, WallpaperEntry, WallpaperType};
use mirrorstar_core::ui_proto::{ControlAction, UiEvent};
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperState};
use serde_json::{json, Value};

mod desktop;
mod dnd;
#[cfg(windows)]
mod drop_target;
#[cfg(windows)]
mod focus;
mod grid;
mod keynav;
#[cfg(windows)]
mod preview;
mod protocol;
mod settings;
mod store;
mod thumbs;
mod videothumb;
#[cfg(windows)]
mod window_icon;
mod window_pos;

use grid::{GridGeometry, GridWindow};
use protocol::{log, read_hello, slint_dispatch, spawn_reader, validate_hello, UiClient};
use settings::PatchQueue;
use store::{AppStore, AppView, UnitStateView};
use thumbs::ThumbPlan;

// 卡片状态（与 `ui/main.slint` 的 `CardData.state` 一一对应）。
const STATE_OK: i32 = 0;
const STATE_WEB: i32 = 1;
const STATE_MISSING: i32 = 2;
const STATE_TYPE_ICON: i32 = 3;

/// 拖影圆角半径（与 `Theme.radius-lg` 一致）。
const DRAG_GHOST_RADIUS: u32 = 8;
/// 拖影整体不透明度。
const DRAG_GHOST_OPACITY: f32 = 0.85;

/// 项目仓库地址：关于面板「项目仓库」链接的唯一来源。
const REPO_URL: &str = "https://github.com/mirrorstar-wallpaper/mirrorstar-wallpaper";

/// 在系统默认浏览器中打开 `url`（best-effort：失败仅记日志，不 panic）。
#[cfg(windows)]
fn open_in_browser(url: &str) {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let url_w = HSTRING::from(url);
    let result = unsafe { ShellExecuteW(None, w!("open"), &url_w, None, None, SW_SHOWNORMAL) };
    // `ShellExecuteW` 约定：返回值 ≤ 32 表示失败，且不写 last-error，仅能提示。
    if result.0 as isize <= 32 {
        log(format!(
            "打开链接失败（ShellExecuteW 返回 {}）：{url}",
            result.0 as isize
        ));
    }
}

thread_local! {
    /// 桌面（WorkerW）状态轮询会话（仅在 UI 线程读写；`None` 表示无轮询进行中）。
    static DESKTOP_POLL: RefCell<Option<desktop::PollSession>> = const { RefCell::new(None) };
}

/// 虚拟化网格与视口的共享运行态（均只在 UI 线程读写，用 `Mutex` 仅为跨闭包共享）。
struct GridUiState {
    geometry: GridGeometry,
    /// 最近一次上报的视口纵偏移（内容坐标）。
    scroll_y: f64,
    /// 最近一次上报的视口高度；窗口隐藏时为 0，仅保留缓冲行，避免无谓解码。
    viewport_h: f64,
    /// 上次已实例化的窗口（用于跳过无变化的重建）。
    last_window: Option<GridWindow>,
    /// 上次重建时的库版本号。
    last_revision: u64,
    // ── 设置面板（Task 6 / Task 7）的 UI 运行态 ──
    /// 上次已写回的设置版本号（跳过未变化时的设置模型重建）。
    settings_revision: u64,
    /// 上次已写回时的壁纸库版本号（`available_rows` 依赖库内容）。
    settings_book_revision: u64,
    /// 上次已写回的编辑池 id（仅在其变化时重置名称输入框，避免打断输入）。
    settings_editing_pool: Option<String>,
    /// 写回串行队列（config / rotation / 池 等；避免并发 read-modify-write 互相覆盖）。
    mutations: PatchQueue<Mutation>,
    // ── Task 9.7.6 视频首帧缩略图 ──
    /// 跨线程共享的按需生成状态（去重 + 限并发 + 结果缓存）。
    thumbs: Arc<videothumb::ThumbStore>,
    /// 首帧生成完成计数：推进后强制重建窗口以载入新缩略图。
    thumb_revision: u64,
    /// 上次重建时已应用的首帧生成计数。
    last_thumb_revision: u64,
    /// 拖拽进行中：为真时冻结由网格宽度变化触发的列数重算，避免打断拖拽。
    dragging: bool,
    /// 最近一次上报的网格宽度；拖拽结束时据此补算列数。
    last_grid_width: f64,
}

impl GridUiState {
    fn new(geometry: GridGeometry) -> Self {
        Self {
            geometry,
            scroll_y: 0.0,
            viewport_h: 0.0,
            last_window: None,
            last_revision: u64::MAX,
            settings_revision: u64::MAX,
            settings_book_revision: u64::MAX,
            settings_editing_pool: None,
            mutations: PatchQueue::default(),
            thumbs: Arc::new(videothumb::ThumbStore::new()),
            thumb_revision: 0,
            last_thumb_revision: u64::MAX,
            dragging: false,
            last_grid_width: 0.0,
        }
    }
}

/// 一条待串行提交的写回意图。
struct Mutation {
    /// 协议命令名（静态字面量）。
    command: &'static str,
    /// 完整参数对象。
    args: Value,
    /// 成功后的补拉动作。
    after: AfterMutation,
    /// 失败时的本地回滚值（运行时命令无配置来源可重拉）。
    revert: Revert,
    /// 失败提示前缀。
    error_label: &'static str,
}

/// 写回失败时的本地回滚值。
#[derive(Clone)]
enum Revert {
    /// 无需回滚（配置类命令失败后靠全量重拉恢复真相）。
    None,
    Volume(f32),
    Mute(bool),
    Speed(f32),
    AutoStart(bool),
    InteractionMode(bool),
    /// 缩放模式回滚：记录目标 key（显示器 id / `"all"`）与其旧值（按显示器回滚）。
    Scaling {
        key: String,
        mode: ScalingMode,
    },
}

/// 写回成功后的补拉策略。
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterMutation {
    /// 无需补拉。
    None,
    /// 配置/轮换类：全量回填设置快照。
    Settings,
    /// 池/单元类：回填池列表 + 单元状态。
    PoolsAndUnits,
}

fn main() {
    if let Err(error) = run() {
        log(format!("启动失败，退出：{error}"));
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let ui = MainWindow::new()?;
    let ui_weak = ui.as_weak();

    let store = Arc::new(Mutex::new(AppStore::default()));
    let gstate = Arc::new(Mutex::new(GridUiState::new(GridGeometry::default())));
    // 批量添加作业（原生拖入 / 文件对话框共用一条串行流水线）。
    let add_job = Arc::new(Mutex::new(AddJob::default()));
    let client = UiClient::new(slint_dispatch());

    // 网格几何由 Rust 单点写入 Slint，避免两处常量漂移。
    {
        let geometry = lock_state(&gstate).geometry;
        apply_geometry(&ui, &geometry);
    }
    // 枚举下拉的选项表是编译期常量，只需写一次。
    apply_static_options(&ui);

    install_handlers(&client, &store, &gstate, &add_job, &ui_weak);
    // 网格交互（Task 5）：搜索 / 选中 / 卡片操作 / 拖拽 / 添加按钮。
    install_interaction_handlers(&ui, &client, &store, &gstate, &add_job);
    // 桌面状态轮询补救（Task 9.7.1）：`desktop-status-changed (ok:false)` 后的 2s 轮询。
    install_desktop_handlers(&ui, &client, &store, &gstate);
    // 全局键盘可达性（Task 9.7.2 / 9.7.3）：Tab / 方向键 / Enter / Delete / Esc。
    install_keyboard_handlers(&ui, &store, &gstate);
    // 预览弹窗（Task 8）：关闭 / 网页轮询 / GIF 逐帧 / 尺寸重排回调。
    #[cfg(windows)]
    preview::install_handlers(&ui);
    // 预览音量/静音的实时下发与持久化（Task 3）。
    #[cfg(windows)]
    install_preview_audio_handlers(&ui, &client, &store, &gstate);
    // 设置面板（Task 6 / Task 7）：控件回写 / 池与单元管理 / 防抖 tick。
    install_settings_handlers(&ui, &client, &store, &gstate);
    // 滚动回调必须在事件循环启动前注册：否则首次布局上报的 viewport 高度会丢失，
    // 首屏窗口退化为"仅缓冲行"，导致可见卡片缺失。
    install_viewport_handler(&ui, &store, &gstate);

    // ── 握手 ────────────────────────────────────────────────────────────────
    // 先把整条 stdin 包进一个 BufReader，握手与后续读取线程共用它（见模块注释第 4 点）。
    let mut stdin = BufReader::new(std::io::stdin());
    client.send_hello()?;
    let shell_hello = read_hello(&mut stdin)?;
    validate_hello(&shell_hello)?;

    {
        let mut guard = lock(&store);
        guard.apply_hello(&shell_hello);
    }
    log(format!(
        "握手成功：壳={} 协议v{} 能力={:#06x}",
        shell_hello.client, shell_hello.protocol_version, shell_hello.capabilities
    ));
    after_store_change(&ui, &store, &gstate);

    // ── 读取线程 ────────────────────────────────────────────────────────────
    // 独占 stdin；解析后把消息投递回事件循环，UI 线程不被 IO 阻塞。
    spawn_reader(stdin, client.clone())?;

    // ── 全量快照 ────────────────────────────────────────────────────────────
    // 启动批次：均为一次性重试（偶发时序失败不致命）；关键数据最终失败会提示重开窗口。
    fetch_startup_snapshot(
        &client,
        "get_config",
        &store,
        &gstate,
        &ui_weak,
        apply_config_value,
        Some("配置"),
    );
    fetch_startup_snapshot(
        &client,
        "get_wallpapers",
        &store,
        &gstate,
        &ui_weak,
        apply_wallpapers_value,
        Some("壁纸库"),
    );
    fetch_startup_snapshot(
        &client,
        "get_displays",
        &store,
        &gstate,
        &ui_weak,
        apply_displays_value,
        None,
    );
    fetch_startup_snapshot(
        &client,
        "get_arrangement",
        &store,
        &gstate,
        &ui_weak,
        apply_arrangement_value,
        None,
    );
    fetch_startup_snapshot(
        &client,
        "get_rotation_config",
        &store,
        &gstate,
        &ui_weak,
        apply_rotation_value,
        None,
    );
    fetch_startup_snapshot(
        &client,
        "list_pools",
        &store,
        &gstate,
        &ui_weak,
        apply_pools_value,
        None,
    );
    fetch_startup_snapshot(
        &client,
        "get_unit_states",
        &store,
        &gstate,
        &ui_weak,
        apply_units_value,
        None,
    );
    fetch_startup_snapshot(
        &client,
        "get_auto_start_status",
        &store,
        &gstate,
        &ui_weak,
        apply_auto_start_value,
        None,
    );
    // 播放按钮初值：无活跃壁纸时壳侧返回 `null`。
    fetch_startup_snapshot(
        &client,
        "get_wallpaper_state",
        &store,
        &gstate,
        &ui_weak,
        apply_playback_value,
        None,
    );

    // 窗口初始保持隐藏：由壳侧 `Show` 控制指令驱动显示（窗口显隐权归壳，Task 2.6）。
    let result = slint::run_event_loop();
    log("事件循环已退出，UI 进程正常结束");
    result.map_err(|e| e.into())
}

/// 注册事件 / 控制 / 断开三类回调（滚动回调见 [`install_viewport_handler`]）。
fn install_handlers(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    add_job: &Arc<Mutex<AddJob>>,
    ui_weak: &slint::Weak<MainWindow>,
) {
    // 推送事件：增量更新状态仓后刷新视图与网格（Task 6.3 / 7.4 的事件回填在此接线）。
    //
    // - 提示类事件（loading / gif-oversized / config-load-error / audio-disabled…）直接弹 Toast；
    // - `wallpaper-state-changed` / `wallpaper-rotated` 的负载只有一个显示器 id，不足以推断
    //   播放状态，故递增防抖 token，由面板 Timer 合并成一次 `get_wallpaper_state` 查询；
    // - `config-changed` 负载为空，需全量重拉设置快照回填面板。
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui_weak.clone();
        let client_for_events = Arc::clone(client);
        client.set_event_handler(Arc::new(move |event: UiEvent| {
            let name = event.event.clone();
            {
                lock(&store).apply_event(&event);
            }
            let Some(ui) = ui_weak.upgrade() else { return };

            // 提示类事件 → Toast（rotated 走防抖合并，不在此列）。
            if let Some(toast) = settings::event_toast(&name, &event.data) {
                show_toast(&ui, toast.level, toast.text);
            }

            after_store_change(&ui, &store, &gstate);

            match name.as_str() {
                "wallpaper-state-changed" => {
                    bump_playback_refresh(&ui);
                }
                "wallpaper-rotated" => {
                    bump_playback_refresh(&ui);
                    let rotated = ui.get_rotated_toast_token();
                    ui.set_rotated_toast_token(rotated.saturating_add(1));
                }
                // 配置变更：壳侧发空负载，全量回填设置快照（含 arrangement / rotation / 池 / 单元）。
                "config-changed" | "config-load-error" => {
                    fetch_settings_snapshot(&client_for_events, &store, &gstate, &ui_weak);
                }
                // 桌面（WorkerW）状态（v16-C-007）：ok=true 恢复并刷新按钮；ok=false 启动轮询补救。
                "desktop-status-changed" => {
                    let ok = event
                        .data
                        .get("ok")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if ok {
                        stop_desktop_poll(&ui);
                        bump_playback_refresh(&ui);
                    } else {
                        start_desktop_poll(&ui);
                    }
                }
                _ => {}
            }
        }));
    }

    // 控制指令：壳侧驱动窗口显隐（Show/Hide 后回报可见性供空闲退出计时），Exit 请求退出。
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let add_job = Arc::clone(add_job);
        let ui_weak = ui_weak.clone();
        // 回调内需回报可见性，故单独克隆一份句柄，避免与被调用的接收者 `client` 争用。
        let client_for_control = Arc::clone(client);
        let client_for_drop = Arc::clone(client);
        client.set_control_handler(Arc::new(move |action: ControlAction| match action {
            ControlAction::Show => {
                if let Some(ui) = ui_weak.upgrade() {
                    // Task 9.10.2：真正不激活显示——仅在 `ui.show()` 这一瞬间给窗口置
                    // `WS_EX_NOACTIVATE`（`SW_SHOW` 对带此位的窗口不激活），显示后立刻移除，
                    // 因此用户此后点击窗口仍能正常输入焦点；全程不调用任何抢前台 API。
                    #[cfg(windows)]
                    {
                        let _no_activate = focus::NoActivateGuard::install(ui.window());
                        // 显示前把窗口居中到鼠标所在显示器的工作区（best-effort，不抢前台）。
                        #[cfg(windows)]
                        window_pos::center_on_cursor_monitor(ui.window());
                        if let Err(e) = ui.show() {
                            log(format!("显示窗口失败：{e}"));
                        }
                        // 窗口显示后才可能拿到惰性 HWND；显式设置应用图标（幂等，best-effort）。
                        window_icon::apply(ui.window());
                    }
                    #[cfg(not(windows))]
                    if let Err(e) = ui.show() {
                        log(format!("显示窗口失败：{e}"));
                    }
                    // 窗口显示后才可能拿到原生 HWND，此处挂载文件拖入接收器（幂等）。
                    install_native_drop_target(&ui, &client_for_drop, &store, &gstate, &add_job);
                    // 显示后布局才给出真实视口高度；此处主动重建一次，`viewport-changed`
                    // 回调会再以真实高度刷新，二者幂等（窗口无变化时会被跳过）。
                    after_store_change(&ui, &store, &gstate);
                }
                client_for_control.report_visibility(true);
            }
            ControlAction::Hide => {
                if let Some(ui) = ui_weak.upgrade() {
                    // 隐藏前先关闭预览：释放 mpv / WebView2 与宿主子窗口，杜绝后台残留。
                    #[cfg(windows)]
                    preview::close(&ui);
                    let _ = ui.hide();
                }
                client_for_control.report_visibility(false);
            }
            ControlAction::Exit => {
                log("收到壳侧退出指令");
                if let Some(ui) = ui_weak.upgrade() {
                    #[cfg(windows)]
                    preview::close(&ui);
                }
                let _ = slint::quit_event_loop();
            }
            // 这两个动作方向相反（UI→壳），不应由壳侧发来。
            ControlAction::WindowShown | ControlAction::WindowHidden => {}
        }));
    }

    // 连接断开：更新状态并优雅退出，避免 UI 进程成为孤儿挂死。
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui_weak.clone();
        client.set_disconnect_handler(Arc::new(move |reason: String| {
            {
                let mut guard = lock(&store);
                guard.set_connected(false);
                guard.record_error(reason);
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            let _ = slint::quit_event_loop();
        }));
    }
}

/// 按可用宽度重算列数并单点写回几何（幂等：列数未变时直接返回）。
///
/// 调用方**不得**持有 `gstate` 锁（内部会再次取锁，否则死锁）。
fn recompute_columns(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    width: f64,
) {
    let new_geometry = {
        let mut g = lock_state(gstate);
        let cols =
            grid::columns_for_width(width, g.geometry.card_w, g.geometry.gap, g.geometry.pad);
        if cols == g.geometry.columns {
            return;
        }
        g.geometry.columns = cols;
        // 列数变化会改变每张卡片的 (row, col) 投影，但行对齐窗口元组在小库下与列数无关
        // （`end_index` 会被条目数夹到同一值），会被 `refresh_grid` 的短路判定误判为"无变化"
        // 而跳过重建，导致卡片仍按旧列数落点渲染——例如 4 列布局放进 640px 网格时，
        // 末列（x≈644）被 `grid-area` 的 `clip` 裁掉，表现为"最右一列被设置面板遮挡"。
        // 故显式失效缓存窗口，强制按新列数重新投影。
        g.last_window = None;
        g.geometry
    };
    apply_geometry(ui, &new_geometry);
    refresh_grid(ui, store, gstate);
}

/// 注册网格滚动回调（`Flickable.viewport-y` / 高度变化时由 Slint 调用）。
fn install_viewport_handler(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    // 宽度处理器所需的克隆先于 viewport 闭包取好（后者会 move 走同名的局部绑定）。
    let width_store = Arc::clone(store);
    let width_gstate = Arc::clone(gstate);

    let store = Arc::clone(store);
    let gstate = Arc::clone(gstate);
    let ui_weak = ui.as_weak();
    ui.on_viewport_changed(move |y, h| {
        {
            let mut g = lock_state(&gstate);
            g.scroll_y = y as f64;
            // 真实布局完成前 Slint 可能上报高度 0；写入会把网格行窗口退化为"仅缓冲行"，
            // 故仅在 h > 0 时更新，否则保留上一次有效高度。
            if h > 0.0 {
                g.viewport_h = h as f64;
            }
        }
        if let Some(ui) = ui_weak.upgrade() {
            refresh_grid(&ui, &store, &gstate);
        }
    });

    // 网格宽度变化 → 重算列数并单点写回几何（Task 4.2）。
    {
        let store = width_store;
        let gstate = width_gstate;
        let ui_weak = ui.as_weak();
        ui.on_grid_width_changed(move |w: f32| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            // 先在一个块内记录最新宽度并读取拖拽态；锁不得跨 `recompute_columns` 借用。
            let dragging = {
                let mut g = lock_state(&gstate);
                g.last_grid_width = w as f64;
                g.dragging
            };
            // 拖拽中冻结列数重算（拖拽结束由 `on_drag_commit` / `on_drag_ended` 补算）。
            if dragging {
                return;
            }
            recompute_columns(&ui, &store, &gstate, w as f64);
        });
    }
}

/// 注册全局键盘动作回调（Task 9.7.2 / 9.7.3）。
///
/// Slint 根 `FocusScope` 的 `capture-key-pressed` 把按键映射为动作名后调到这里；
/// 返回 `true` 表示已消费（Slint 侧据此 accept）。焦点在**过滤序列**上推进，
/// 与虚拟化网格共用几何；`activate`/`delete` 依据焦点索引反查条目 id。
fn install_keyboard_handlers(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    let store = Arc::clone(store);
    let gstate = Arc::clone(gstate);
    let ui_weak = ui.as_weak();
    ui.on_key_command(move |action: SharedString| -> bool {
        let Some(ui) = ui_weak.upgrade() else {
            return false;
        };
        let action = action.as_str();

        // dismiss：预览优先（全屏时先退全屏，否则关闭预览），其次取消删除确认（Task 9.7.3）。
        if action == "dismiss" {
            if !ui.get_preview_id().is_empty() {
                if ui.get_preview_fullscreen() {
                    // 复用与全屏按钮相同的路径：set_fullscreen(false) + 复位属性与 chrome。
                    ui.invoke_preview_toggle_fullscreen();
                    return true;
                }
                #[cfg(windows)]
                preview::close(&ui);
                #[cfg(not(windows))]
                ui.set_preview_id(SharedString::default());
                return true;
            }
            if !ui.get_pending_delete_id().is_empty() {
                ui.invoke_cancel_delete();
                return true;
            }
            return false;
        }

        // 预览打开时的快捷键：空格=播放/暂停，←/→=快退/快进 ±5s，M=静音。
        if !ui.get_preview_id().is_empty() {
            match action {
                "activate" => {
                    ui.invoke_preview_toggle_play();
                    return true;
                }
                "mute" => {
                    ui.invoke_preview_toggle_mute();
                    return true;
                }
                "left" | "right" => {
                    #[cfg(windows)]
                    preview::seek_relative(&ui, if action == "left" { -5.0 } else { 5.0 });
                    return true;
                }
                _ => {}
            }
        }

        // activate / delete：以当前焦点卡为对象。
        if action == "activate" || action == "delete" {
            let index = ui.get_kb_focus_index();
            if index < 0 {
                return false;
            }
            let id = lock(&store).filtered_id_at(index as usize);
            let Some(id) = id else {
                return false;
            };
            if action == "activate" {
                ui.invoke_request_preview(SharedString::from(id));
            } else {
                ui.invoke_request_delete(SharedString::from(id));
            }
            return true;
        }

        let moves = match action {
            "left" => keynav::FocusMove::Left,
            "right" => keynav::FocusMove::Right,
            "up" => keynav::FocusMove::Up,
            "down" => keynav::FocusMove::Down,
            "next" => keynav::FocusMove::Next,
            "prev" => keynav::FocusMove::Prev,
            _ => return false,
        };

        let (count, geometry, scroll_y, viewport_h) = {
            let store = lock(&store);
            let g = lock_state(&gstate);
            (store.filtered_count(), g.geometry, g.scroll_y, g.viewport_h)
        };
        let current = {
            let index = ui.get_kb_focus_index();
            if index < 0 {
                None
            } else {
                Some(index as usize)
            }
        };
        let Some(next) = keynav::move_focus(current, moves, count, geometry.columns) else {
            return false;
        };
        ui.set_kb_focus_index(next as i32);
        let target = keynav::scroll_to_show(next, &geometry, scroll_y, viewport_h, count);
        if (target - scroll_y).abs() > 0.5 {
            ui.set_scroll_target(target as f32);
        }
        true
    });
}

/// 递增播放状态刷新防抖 token（由面板 Timer 合并为一次 `get_wallpaper_state`）。
fn bump_playback_refresh(ui: &MainWindow) {
    let token = ui.get_playback_refresh_token();
    ui.set_playback_refresh_token(token.saturating_add(1));
}

/// 启动桌面（WorkerW）状态轮询（幂等：已在轮询中则忽略）。
fn start_desktop_poll(ui: &MainWindow) {
    let in_flight = DESKTOP_POLL.with(|p| p.borrow().is_some());
    if !desktop::should_start_poll(false, in_flight) {
        return;
    }
    DESKTOP_POLL.with(|p| *p.borrow_mut() = Some(desktop::PollSession::start(Instant::now())));
    ui.set_desktop_polling(true);
    log("收到 desktop-status-changed (ok:false)，启动 WorkerW 状态轮询");
}

/// 停止桌面状态轮询并清除会话。
fn stop_desktop_poll(ui: &MainWindow) {
    DESKTOP_POLL.with(|p| *p.borrow_mut() = None);
    ui.set_desktop_polling(false);
}

/// 注册桌面状态轮询的 Slint 回调（`desktop-poll-tick`，由 2s 定时器驱动）。
fn install_desktop_handlers(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    let client = Arc::clone(client);
    let store = Arc::clone(store);
    let gstate = Arc::clone(gstate);
    let ui_weak = ui.as_weak();
    ui.on_desktop_poll_tick(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let now = Instant::now();
        // 会话缺失（已被停止）或已超时 → 结束轮询；超时则提示用户重启。
        let expired =
            DESKTOP_POLL.with(|p| p.borrow().as_ref().map(|s| s.expired(now)).unwrap_or(true));
        if expired {
            stop_desktop_poll(&ui);
            log("WorkerW 状态轮询 30s 超时仍未恢复");
            show_toast(
                &ui,
                settings::TOAST_ERROR,
                "桌面状态异常，壁纸可能无法显示，请重启应用",
            );
            return;
        }
        // 发一条补救查询：`true` = 已重初始化（恢复），`false` = 仍失效。
        let store_cb = Arc::clone(&store);
        let gstate_cb = Arc::clone(&gstate);
        let ui_weak_cb = ui_weak.clone();
        let outcome = client.request("check_desktop_status", Value::Null, move |result| {
            let Some(ui) = ui_weak_cb.upgrade() else {
                return;
            };
            match result {
                Ok(Value::Bool(true)) => {
                    stop_desktop_poll(&ui);
                    lock(&store_cb).set_desktop_ok(true);
                    after_store_change(&ui, &store_cb, &gstate_cb);
                    bump_playback_refresh(&ui);
                    show_toast(&ui, settings::TOAST_SUCCESS, "桌面状态已恢复");
                }
                // `false`：WorkerW 当前有效，无需重初始化，继续轮询直至恢复或超时。
                Ok(_) => {}
                Err(error) => {
                    lock(&store_cb)
                        .record_error(format!("check_desktop_status 失败：{}", error.message));
                }
            }
        });
        if let Err(error) = outcome {
            lock(&store).record_error(format!("check_desktop_status 请求发送失败：{error}"));
        }
    });
}

/// 状态仓变化后的统一刷新：写回标量视图 + 重建可见窗口 + 重投影设置模型。
fn after_store_change(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    let view = lock(store).view();
    apply_view(ui, &view);
    refresh_grid(ui, store, gstate);
    refresh_settings(ui, store, gstate);
}

/// 重试决策（纯函数）：还有剩余次数则返回下一次可用次数，否则 `None`。
///
/// 仅用于启动批次（`retries_left` 初值 1），便于单测覆盖边界。
fn next_retries(retries_left: u8) -> Option<u8> {
    retries_left.checked_sub(1)
}

/// 发起一条快照请求（薄封装，运行期调用点）：不重试、无失败 Toast。
fn fetch_snapshot(
    client: &Arc<UiClient>,
    command: &'static str,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
    apply: fn(&mut AppStore, Value) -> Result<(), String>,
) {
    fetch_snapshot_inner(
        client,
        command,
        Value::Null,
        store,
        gstate,
        ui_weak,
        apply,
        0,
        None,
    );
}

/// 发起一条启动快照请求（薄封装）：失败时重试一次，最终仍失败则按 `error_label` 提示。
fn fetch_startup_snapshot(
    client: &Arc<UiClient>,
    command: &'static str,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
    apply: fn(&mut AppStore, Value) -> Result<(), String>,
    error_label: Option<&'static str>,
) {
    fetch_snapshot_inner(
        client,
        command,
        Value::Null,
        store,
        gstate,
        ui_weak,
        apply,
        1,
        error_label,
    );
}

/// 查询播放状态（带 display_id，使状态与实际控制目标一致；不重试、失败不弹 Toast）。
fn fetch_playback_state(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
) {
    let display_id = {
        let guard = lock(store);
        playback_state_display_id(&guard)
    };
    fetch_snapshot_inner(
        client,
        "get_wallpaper_state",
        json!({ "display_id": display_id }),
        store,
        gstate,
        ui_weak,
        apply_playback_value,
        0,
        None,
    );
}

/// 发起一条快照请求：成功后解析并落到状态仓，失败记入错误；两者都刷新视图。
///
/// `retries_left > 0` 时（启动批次）在首次 `Err` 后重发一次，重试期间不弹 Toast；
/// 重试耗尽（`retries_left == 0`）后若给了 `error_label` 则提示关键数据加载失败。
/// `client`/`store`/`gstate`/`ui_weak` 均克隆进闭包，重试路径可重复使用。
#[allow(clippy::too_many_arguments)] // 快照请求的依赖项天然较多，拆结构体得不偿失。
fn fetch_snapshot_inner(
    client: &Arc<UiClient>,
    command: &'static str,
    args: Value,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
    apply: fn(&mut AppStore, Value) -> Result<(), String>,
    retries_left: u8,
    error_label: Option<&'static str>,
) {
    let client_for_cb = Arc::clone(client);
    let store_for_cb = Arc::clone(store);
    let gstate_for_cb = Arc::clone(gstate);
    let ui_weak_for_cb = ui_weak.clone();
    let outcome = client.request(command, args.clone(), move |result| {
        {
            let mut guard = lock(&store_for_cb);
            match result {
                Ok(value) => {
                    if let Err(message) = apply(&mut guard, value) {
                        guard.record_error(format!("{command} 结果解析失败：{message}"));
                    }
                }
                Err(error) => {
                    guard.record_error(format!("{command} 失败：{}", error.message));
                    // 先释放状态仓锁，重试/提示路径不再持锁（避免重试嵌套取锁）。
                    drop(guard);
                    if let Some(next) = next_retries(retries_left) {
                        log(format!("{command} 失败，重试一次：{}", error.message));
                        fetch_snapshot_inner(
                            &client_for_cb,
                            command,
                            args.clone(),
                            &store_for_cb,
                            &gstate_for_cb,
                            &ui_weak_for_cb,
                            apply,
                            next,
                            error_label,
                        );
                    } else if let Some(label) = error_label {
                        if let Some(ui) = ui_weak_for_cb.upgrade() {
                            show_toast(
                                &ui,
                                crate::settings::TOAST_WARNING,
                                format!("{label}加载失败，请重开窗口重试"),
                            );
                        }
                    }
                }
            }
        }
        if let Some(ui) = ui_weak_for_cb.upgrade() {
            after_store_change(&ui, &store_for_cb, &gstate_for_cb);
        }
    });

    // 写失败通常意味着对端已断开（断开回调另会处理），此处仅记录以便诊断。
    if let Err(error) = outcome {
        lock(store).record_error(format!("{command} 请求发送失败：{error}"));
    }
}

/// 把网格几何写入 Slint（单点写入：卡片尺寸/间距/内边距为常量，列数随网格宽度变化）。
fn apply_geometry(ui: &MainWindow, geometry: &GridGeometry) {
    ui.set_card_w(geometry.card_w as f32);
    ui.set_card_h(geometry.card_h as f32);
    ui.set_grid_gap(geometry.gap as f32);
    ui.set_grid_pad(geometry.pad as f32);
    // 列数只用于拖拽插入指示线的取模/除算，故与几何一并单点写入。
    ui.set_column_count(geometry.columns as i32);
}

/// 计算窗口 → 切片 → 解码 → 写回 Slint 模型。
///
/// **幂等且廉价**：窗口区间与库版本都未变化时直接返回，因此滚动时只有"跨行"才真正重建，
/// 也不会因无关事件反复解码图像。
///
/// 搜索过滤（SubTask 5.3）：窗口在**过滤后的序列**上计算，模型只投影过滤命中的条目，
/// 但仍以原始库索引定位 `(row, col)`，因此过滤与虚拟化可共存且不破坏虚拟化。
fn refresh_grid(ui: &MainWindow, store: &Arc<Mutex<AppStore>>, gstate: &Arc<Mutex<GridUiState>>) {
    // 1) 持锁取窗口 + 规划卡片（纯数据，不解码）。
    let (plans, content_h, visible, candidates, thumbs) = {
        let store_guard = lock(store);
        let mut g = lock_state(gstate);
        // 短路判断只需过滤后条目数，用 O(1) 计数的 `filtered_count` 而非全量索引数组，
        // 避免每次滚动都分配一次 O(n) 的 `filtered_indices`。
        let visible_count = store_guard.filtered_count();
        let window = g.geometry.window(g.scroll_y, g.viewport_h, visible_count);
        let revision = store_guard.revision();
        if g.last_window == Some(window)
            && g.last_revision == revision
            && g.last_thumb_revision == g.thumb_revision
        {
            return;
        }
        g.last_window = Some(window);
        g.last_revision = revision;
        g.last_thumb_revision = g.thumb_revision;
        let content_h = g.geometry.content_height(visible_count) as f32;
        let thumbs = Arc::clone(&g.thumbs);
        // 确认要重建后才取索引数组（供 `plan_cards` 映射回库内条目）。
        let indices = store_guard.filtered_indices();
        let plans = plan_cards(&store_guard, &g.geometry, window, &indices);
        // 可见视频缺缩略图（仍为类型图标）→ 作为按需生成候选（Task 9.7.6）。
        let candidates: Vec<String> = plans
            .iter()
            .filter(|p| {
                p.kind_index == kind_index(WallpaperType::Video) && p.state == STATE_TYPE_ICON
            })
            .map(|p| p.file_path.clone())
            .collect();
        (plans, content_h, visible_count as i32, candidates, thumbs)
    };

    // 2) 解码窗口内图像（UI 线程，锁外）。旧窗口的图像随模型替换被丢弃。
    // 同步保留每张卡片的源文件路径，供解码成功后清理陈旧的"缩略图失败"标记。
    let file_paths: Vec<String> = plans.iter().map(|p| p.file_path.clone()).collect();
    let cards = decode_cards(plans);

    // 2.1) 缩略图成功解码的卡片：幂等清理会话内陈旧失败标记（纵深防御）。
    // 不递增 revision：本轮已正确渲染，递增反而会触发多余的刷新（甚至刷新循环）。
    {
        let mut store_guard = lock(store);
        for (card, file_path) in cards.iter().zip(file_paths.iter()) {
            if card.state == STATE_OK {
                store_guard.clear_thumb_failed(file_path);
            }
        }
    }

    // 3) 写回 Slint。
    ui.set_content_height(content_h);
    ui.set_visible_count(visible);
    ui.set_cards(ModelRc::new(VecModel::from(cards)));

    // 4) 可见视频首帧按需异步生成（限并发、去重；完成后回到 UI 线程强刷）。
    if !candidates.is_empty() {
        dispatch_video_thumbs(ui, store, gstate, &thumbs, &candidates);
    }
}

/// 为可见视频卡片派发首帧生成任务（Task 9.7.6）。
fn dispatch_video_thumbs(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    thumbs: &Arc<videothumb::ThumbStore>,
    candidates: &[String],
) {
    for file_path in thumbs.dispatch_ready(candidates) {
        let ui_weak = ui.as_weak();
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let thumbs = Arc::clone(thumbs);
        // 生成在后台线程完成（mpv 为阻塞子进程），绝不占用 UI 线程。
        std::thread::spawn(move || {
            let result = mirrorstar_core::config::ConfigManager::data_dir().and_then(|dir| {
                mirrorstar_core::config::generate_video_thumbnail(
                    &file_path,
                    &dir.join("thumbnails"),
                )
            });
            if let Err(error) = result {
                log(format!("视频首帧生成失败：{file_path} — {error}"));
            }
            // 无论成功/失败都释放并发额度；成功由 card_state 的磁盘缓存探测体现。
            thumbs.finish();
            // 回到 UI 线程推进计数并强制重建（否则窗口/库版本未变会被跳过）。
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    {
                        let mut g = lock_state(&gstate);
                        g.thumb_revision = g.thumb_revision.wrapping_add(1);
                    }
                    refresh_grid(&ui, &store, &gstate);
                }
            });
        });
    }
}

/// 窗口内单张卡片的"待解码"规划。
struct CardPlan {
    id: String,
    name: String,
    kind_index: i32,
    state: i32,
    /// 源文件路径（视频首帧生成候选判定用）。
    file_path: String,
    /// 仅当 `state == STATE_OK` 时非空，按优先级排列的候选解码路径。
    ///
    /// 首个解码成功的候选即被采用；静态 Image/Gif 的候选为 `[缩略图, 源图]`，
    /// 允许缩略图不可读（0 字节/损坏）时回退源图直载。
    thumb_paths: Vec<String>,
    row: usize,
    col: usize,
    /// 过滤后序列中的绝对索引（键盘焦点 / 邻居让位用）。
    pos: usize,
}

/// 把状态仓的窗口切片投影为卡片规划（不触碰 Slint，也不做 IO）。
///
/// `indices` 为过滤后的原始库索引；`window` 是在该序列上算出的行对齐窗口，
/// 故投影时先按 `window` 切片拿到"第几个可见项"，再经 `indices` 映射回库内条目。
fn plan_cards(
    store: &AppStore,
    geometry: &GridGeometry,
    window: GridWindow,
    indices: &[usize],
) -> Vec<CardPlan> {
    let wallpapers = store.wallpapers();
    let end = window.end_index().min(indices.len());
    let start = window.first_index.min(end);
    let columns = geometry.columns.max(1);
    indices[start..end]
        .iter()
        .enumerate()
        .filter_map(|(offset, &global)| {
            let wp = wallpapers.get(global)?;
            let position = window.first_index + offset;
            let (state, thumb_paths) = card_state(store, wp);
            Some(CardPlan {
                id: wp.id.clone(),
                name: settings::file_name(&wp.file_path),
                kind_index: kind_index(wp.wallpaper_type),
                state,
                file_path: wp.file_path.clone(),
                thumb_paths,
                row: position / columns,
                col: position % columns,
                pos: position,
            })
        })
        .collect()
}

/// 判定单张卡片的显示状态（SubTask 4.3 / 4.4；Task 9.7.6 补视频首帧；Task 7 增分级回退）。
///
/// 返回 `(state, 候选缩略图路径列表)`：`state == STATE_OK` 时调用方按顺序尝试解码候选，
/// 首个成功即用；全部失败再降级类型图标。静态 Image/Gif 的候选含源图兜底，
/// 因此**不再被 `is_thumb_failed` 短路成类型图标**——是否可显示由解码结果决定。
fn card_state(store: &AppStore, wp: &WallpaperEntry) -> (i32, Vec<String>) {
    if store.is_source_missing(&wp.id) {
        return (STATE_MISSING, Vec::new());
    }
    // 视频无配置缩略图时，按确定性命名探测磁盘缓存（跨会话复用，避免重复抽帧）。
    if matches!(wp.wallpaper_type, WallpaperType::Video) && wp.thumbnail.is_empty() {
        if let Some(cache_path) = mirrorstar_core::config::thumbnail_cache_path(&wp.file_path) {
            let cache_path = cache_path.to_string_lossy().into_owned();
            if thumbs::path_exists(&cache_path) {
                return (STATE_OK, vec![cache_path]);
            }
        }
    }
    // 视频的 `is_thumb_failed` 短路保留：视频无源图可回退，抽帧失败即显示类型图标
    // （仍作为按需生成候选，由 `refresh_grid` 派发首帧生成）。静态 Image/Gif 不走此短路。
    if matches!(wp.wallpaper_type, WallpaperType::Video) && store.is_thumb_failed(&wp.file_path) {
        return (STATE_TYPE_ICON, Vec::new());
    }
    match thumbs::plan(wp.wallpaper_type, &wp.thumbnail, &wp.file_path) {
        ThumbPlan::Web => (STATE_WEB, Vec::new()),
        ThumbPlan::TypeIcon => (STATE_TYPE_ICON, Vec::new()),
        ThumbPlan::Path(_) => {
            // 有序候选：优先缩略图；静态 Image/Gif 再追加源图，供缩略图不可读时回退直载。
            let mut candidates: Vec<String> = Vec::new();
            if !wp.thumbnail.is_empty() {
                candidates.push(wp.thumbnail.clone());
            }
            if matches!(wp.wallpaper_type, WallpaperType::Image | WallpaperType::Gif)
                && !wp.file_path.is_empty()
                && wp.file_path != wp.thumbnail
            {
                candidates.push(wp.file_path.clone());
            }
            if candidates.is_empty() {
                // 无任何可用候选（缩略图/源路径皆空）：类型图标占位。
                (STATE_TYPE_ICON, Vec::new())
            } else {
                (STATE_OK, candidates)
            }
        }
    }
}

/// 解码窗口内图像，产出 Slint 模型条目。
///
/// `state == STATE_OK` 时按 [`CardPlan::thumb_paths`] 顺序尝试解码，首个成功即用；
/// 全部失败降级为类型图标占位（不 panic、不中断网格）。
fn decode_cards(plans: Vec<CardPlan>) -> Vec<CardData> {
    plans
        .into_iter()
        .map(|plan| {
            let mut state = plan.state;
            let mut thumb = Image::default();
            if state == STATE_OK {
                let mut decoded = None;
                for path in &plan.thumb_paths {
                    match thumbs::decode_capped(path) {
                        Ok(image) => {
                            decoded = Some(image);
                            break;
                        }
                        Err(error) => {
                            log(format!(
                                "缩略图候选解码失败（尝试下一候选）：{path} — {error}"
                            ));
                        }
                    }
                }
                match decoded {
                    Some(image) => thumb = image,
                    None => {
                        log(format!(
                            "缩略图全部候选解码失败（降级占位）：{}",
                            plan.file_path
                        ));
                        state = STATE_TYPE_ICON;
                    }
                }
            }
            CardData {
                id: SharedString::from(plan.id),
                name: SharedString::from(plan.name),
                kind_index: plan.kind_index,
                state,
                thumb,
                row: plan.row as i32,
                col: plan.col as i32,
                pos: plan.pos as i32,
            }
        })
        .collect()
}

/// 类型 → 卡片标签索引（与 `main.slint` 配色/文案一一对应）。
fn kind_index(wallpaper_type: WallpaperType) -> i32 {
    match wallpaper_type {
        WallpaperType::Image => 0,
        WallpaperType::Video => 1,
        WallpaperType::Gif => 2,
        WallpaperType::Web => 3,
    }
}

/// 把状态仓投影写回 Slint 标量属性（网格模型由 [`refresh_grid`] 单独写回）。
fn apply_view(ui: &MainWindow, view: &AppView) {
    ui.set_connected(view.connected);
    ui.set_item_count(view.item_count);
    ui.set_display_count(view.display_count);
    ui.set_config_summary(SharedString::from(view.config_summary.as_str()));
    ui.set_rotation_summary(SharedString::from(view.rotation_summary.as_str()));
    ui.set_playback_summary(SharedString::from(view.playback_summary.as_str()));
    ui.set_last_event(SharedString::from(view.last_event.as_str()));
    ui.set_loading(view.loading);
    // ── Task 5 交互态 ──
    ui.set_search_active(view.search_active);
    ui.set_selected_id(SharedString::from(view.selected_id.as_str()));
    ui.set_active_id(SharedString::from(view.active_id.as_str()));
    // 搜索激活时禁用拖拽排序：过滤后的插入位无法稳定映射回库内顺序。
    ui.set_drag_enabled(!view.search_active);
    // ── Task 6 设置面板标量（滑块位置等需门控的标量在 `refresh_settings` 写回）──
    ui.set_settings_open(view.settings_open);
    ui.set_display_index(view.display_index);
    ui.set_display_dropdown_enabled(view.display_dropdown_enabled);
    ui.set_scaling_index(view.scaling_index);
    ui.set_arrangement_index(view.arrangement_index);
    ui.set_fullscreen_index(view.fullscreen_index);
    ui.set_pause_enabled(view.pause_enabled);
    ui.set_pause_label(SharedString::from(view.pause_label.as_str()));
    ui.set_prev_enabled(view.prev_enabled);
    ui.set_next_enabled(view.next_enabled);
    ui.set_volume_percent(view.volume_percent);
    ui.set_muted(view.muted);
    ui.set_audio_controls_enabled(view.audio_controls_enabled);
    ui.set_desktop_ok(view.desktop_ok);
    ui.set_auto_start(view.auto_start);
    ui.set_pause_on_battery(view.pause_on_battery);
    ui.set_interaction_mode(view.interaction_mode);
    // ── Task 7 轮换 ──
    ui.set_rotation_enabled(view.rotation_enabled);
    ui.set_rotation_on_boot(view.rotation_on_boot);
    ui.set_rotation_interval(view.rotation_interval);
    ui.set_rotation_order_index(view.rotation_order_index);
    ui.set_rotation_order_hint(SharedString::from(view.rotation_order_hint.as_str()));
    ui.set_about_version(SharedString::from(view.about_version.as_str()));
}

/// 写回不随状态变化、仅由常量表决定的枚举下拉选项（启动时写一次）。
fn apply_static_options(ui: &MainWindow) {
    ui.set_scaling_options(option_model(
        &settings::SCALING_MODES,
        &settings::SCALING_LABELS,
    ));
    ui.set_arrangement_options(option_model(
        &settings::ARRANGEMENTS,
        &settings::ARRANGEMENT_LABELS,
    ));
    ui.set_fullscreen_options(option_model(
        &settings::FULLSCREEN_ACTIONS,
        &settings::FULLSCREEN_LABELS,
    ));
    ui.set_order_options(option_model(&settings::ORDERS, &settings::ORDER_LABELS));
}

/// 把「值表 + 标签表」投影为 Slint 下拉模型。
fn option_model(values: &[&str], labels: &[&str]) -> ModelRc<OptionItem> {
    let items: Vec<OptionItem> = values
        .iter()
        .zip(labels.iter())
        .map(|(value, label)| OptionItem {
            value: SharedString::from(*value),
            label: SharedString::from(*label),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// 注册预览音量/静音回调：实时下发 mpv，并在拖动结束 / 静音切换时持久化。
#[cfg(windows)]
fn install_preview_audio_handlers(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    // 拖动中：仅实时下发 mpv，不落盘。
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_set_volume(move |v: f32| {
            if let Some(ui) = ui_weak.upgrade() {
                preview::set_volume(&ui, v);
            }
        });
    }
    // 释放：实时下发并持久化。
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_preview_commit_volume(move |v: f32| {
            if let Some(ui) = ui_weak.upgrade() {
                preview::set_volume(&ui, v);
            }
            persist_preview_audio(
                &ui_weak,
                &client,
                &store,
                &gstate,
                v,
                preview::session_audio().1,
            );
        });
    }
    // 静音切换：翻转后持久化。
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_preview_toggle_mute(move || {
            if let Some(ui) = ui_weak.upgrade() {
                preview::toggle_mute(&ui);
            }
            let (v, m) = preview::session_audio();
            persist_preview_audio(&ui_weak, &client, &store, &gstate, v, m);
        });
    }
}

/// 把预览音量/静音写回配置（`volume_0_100` 为 mpv 标度 0..100）。
#[cfg(windows)]
fn persist_preview_audio(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    volume_0_100: f32,
    muted: bool,
) {
    let patch = json!({ "audio": { "preview_volume": preview::mpv_volume_to_config(volume_0_100), "preview_muted": muted } });
    let Some(config) = lock(store).merge_config_patch(&patch) else {
        return;
    };
    enqueue_mutation(
        ui_weak,
        client,
        store,
        gstate,
        Mutation {
            command: "update_config",
            args: json!({ "config": config }),
            // 配置类写回用 Settings 补拉，失败时可经全量重拉恢复（与其它 update_config 调用点一致）。
            after: AfterMutation::Settings,
            revert: Revert::None,
            error_label: "设置预览音量",
        },
    );
}

/// 把状态仓设置相关字段投影写回 Slint；版本号未变化时直接返回（幂等且廉价）。
fn refresh_settings(
    ui: &MainWindow,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    // 1) 门控：设置版本号 / 库版本号 / 编辑池均无变化则跳过（避免大库下反复重建）。
    let (revision, book_revision, editing_pool) = {
        let guard = lock(store);
        (
            guard.settings_revision(),
            guard.revision(),
            guard.editing_pool().map(str::to_string),
        )
    };
    // 预览音量/静音：按配置幂等种子化会话态（仅首次生效，不覆盖会话内用户改动）。
    // 读取与使用整体置于 Windows 分支：非 Windows 目标无 preview 模块，避免未使用变量告警。
    // 首次写入（seed 返回 true）且已有预览时，把会话态补齐到 Slint 属性与 mpv（配置晚到补齐）。
    #[cfg(windows)]
    {
        // 先取值再释放 store 锁：避免持锁期间调用 preview（Slint 属性写入 + IPC 下发）。
        let preview_audio = lock(store).preview_audio();
        if let Some((cfg_volume, cfg_muted)) = preview_audio {
            let seeded =
                preview::seed_session_audio(preview::config_volume_to_mpv(cfg_volume), cfg_muted);
            if seeded && !ui.get_preview_id().is_empty() {
                preview::apply_session_audio(ui);
            }
        }
    }
    {
        let mut g = lock_state(gstate);
        if g.settings_revision == revision
            && g.settings_book_revision == book_revision
            && g.settings_editing_pool == editing_pool
        {
            return;
        }
        g.settings_revision = revision;
        g.settings_book_revision = book_revision;
        g.settings_editing_pool = editing_pool.clone();
    }

    // 2) 投影模型（持 store 锁，锁外不触碰 gstate，保持与 `refresh_grid` 一致的加锁顺序）。
    let (display_options, pool_options, pool_rows, member_rows, available_rows, unit_rows) = {
        let guard = lock(store);
        let editing = guard.editing_pool();
        (
            guard.display_options(),
            guard.pool_options(),
            guard.pool_rows(),
            editing.map(|id| guard.member_rows(id)).unwrap_or_default(),
            editing
                .map(|id| guard.available_rows(id))
                .unwrap_or_default(),
            guard.unit_rows(),
        )
    };

    // 3) 需要门控的标量（滑块位置 / 编辑名输入框文本）：仅在设置变化时写回，
    //    避免用户拖拽或输入期间被无关刷新打断。
    let (volume_pos, speed_pos, speed_text, rotation_interval_text, editing_pool_name) = {
        let guard = lock(store);
        (
            guard.volume(),
            settings::speed_to_slider(guard.speed()),
            format!("{:.2}x", guard.speed()),
            guard
                .rotation()
                .map(|r| r.interval_minutes.to_string())
                .unwrap_or_else(|| "30".to_string()),
            editing_pool
                .as_deref()
                .and_then(|id| guard.pools().iter().find(|p| p.id == id))
                .map(|p| p.name.clone())
                .unwrap_or_default(),
        )
    };

    ui.set_display_options(option_model_from(display_options));
    ui.set_pool_options(option_model_from(pool_options));
    ui.set_pool_rows(pool_model(pool_rows));
    ui.set_member_rows(member_model(member_rows));
    ui.set_available_rows(member_model(available_rows));
    ui.set_unit_rows(unit_model(unit_rows));
    ui.set_has_editing_pool(editing_pool.is_some());
    ui.set_editing_pool_id(SharedString::from(editing_pool.unwrap_or_default()));
    ui.set_editing_pool_name(SharedString::from(editing_pool_name));
    ui.set_volume_pos(volume_pos);
    ui.set_speed_pos(speed_pos);
    ui.set_speed_text(SharedString::from(speed_text));
    ui.set_rotation_interval_text(SharedString::from(rotation_interval_text));
}

/// `OptionItemData` → Slint `OptionItem` 模型。
fn option_model_from(items: Vec<store::OptionItemData>) -> ModelRc<OptionItem> {
    let items: Vec<OptionItem> = items
        .into_iter()
        .map(|item| OptionItem {
            value: SharedString::from(item.value),
            label: SharedString::from(item.label),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// `PoolRowData` → Slint `PoolRow` 模型。
fn pool_model(rows: Vec<store::PoolRowData>) -> ModelRc<PoolRow> {
    let items: Vec<PoolRow> = rows
        .into_iter()
        .map(|r| PoolRow {
            id: SharedString::from(r.id),
            name: SharedString::from(r.name),
            members: r.members,
            up: r.up,
            down: r.down,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// `UnitRowData` → Slint `UnitRow` 模型。
fn unit_model(rows: Vec<store::UnitRowData>) -> ModelRc<UnitRow> {
    let items: Vec<UnitRow> = rows
        .into_iter()
        .map(|r| UnitRow {
            key: SharedString::from(r.key),
            label: SharedString::from(r.label),
            pool_index: r.pool_index,
            enabled: r.enabled,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// `MemberRowData` → Slint `MemberRow` 模型（成员行与可添加行共用）。
fn member_model(rows: Vec<store::MemberRowData>) -> ModelRc<MemberRow> {
    let items: Vec<MemberRow> = rows
        .into_iter()
        .map(|r| MemberRow {
            id: SharedString::from(r.id),
            name: SharedString::from(r.name),
            up: r.up,
            down: r.down,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// 取状态仓锁；锁中毒时取回内部数据（UI 崩溃降级优先于二次 panic）。
fn lock(store: &Arc<Mutex<AppStore>>) -> std::sync::MutexGuard<'_, AppStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_state(gstate: &Arc<Mutex<GridUiState>>) -> std::sync::MutexGuard<'_, GridUiState> {
    gstate.lock().unwrap_or_else(|e| e.into_inner())
}

// ── 网格交互（Task 5）────────────────────────────────────────────────────────

/// 批量添加作业：原生拖入与"添加按钮"共用一条**串行**流水线。
///
/// 串行（而非并发）是刻意的：壳侧 `add_wallpaper` 要复制文件、抽缩略图，并发提交会让进度
/// 与失败提示错乱；串行还能把"成功/失败/跳过"计数收敛成一条结束 Toast。
#[derive(Default)]
struct AddJob {
    /// 待添加的文件路径队列（FIFO）。
    queue: VecDeque<String>,
    /// 是否已有一条 `add_wallpaper` 在途（在途时新任务只入队，不并发提交）。
    running: bool,
    added: u32,
    failed: u32,
    rejected: u32,
}

/// 取添加作业锁；锁中毒时取回内部数据（与 [`lock`] 同策略，避免二次 panic）。
fn lock_job(job: &Arc<Mutex<AddJob>>) -> std::sync::MutexGuard<'_, AddJob> {
    job.lock().unwrap_or_else(|e| e.into_inner())
}

/// 注册全部网格交互回调（搜索 / 选中 / 卡片操作 / 拖拽 / 添加按钮 / 预览打开）。
///
/// 约定：这些闭包会被 Slint 以 `Fn` 多次调用，故捕获的共享态只能是 `Arc`/`Weak`，
/// 且**不能把捕获值 move 出去**——需要移动时先在闭包体内克隆一份。
fn install_interaction_handlers(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    add_job: &Arc<Mutex<AddJob>>,
) {
    // `data-transfer` 在 Slint 中不透明，只能由宿主构造；负载即条目 id。
    ui.on_to_transfer(|id: SharedString| DataTransfer::from(id));

    // ── 搜索过滤（SubTask 5.3）：关键字写回状态仓并重建可见窗口 ──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_search_changed(move |keyword: SharedString| {
            {
                let mut guard = lock(&store);
                guard.set_search_keyword(keyword.to_string());
            }
            if let Some(ui) = ui_weak.upgrade() {
                // 过滤序列变化：键盘焦点失效，重置（下次按键重新进入网格）。
                ui.set_kb_focus_index(-1);
                after_store_change(&ui, &store, &gstate);
            }
        });
    }

    // ── 选中 / 取消选中（SubTask 5.4）：纯 UI 态，不推进版本号，不重解码 ──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_select_card(move |id: SharedString| {
            let id = id.to_string();
            let focus = {
                let mut guard = lock(&store);
                let already = guard.selected_id() == Some(id.as_str());
                guard.select(if already { None } else { Some(id.clone()) });
                // 点击卡片同步键盘焦点（关闭预览后归还焦点也复用该索引）。
                guard
                    .filtered_position_of(&id)
                    .map(|p| p as i32)
                    .unwrap_or(-1)
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_kb_focus_index(focus);
                after_store_change(&ui, &store, &gstate);
            }
        });
    }

    // ── 设为壁纸（SubTask 5.4）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_set_as_wallpaper(move |id: SharedString| {
            let id = id.to_string();
            let store_cb = Arc::clone(&store);
            let gstate_cb = Arc::clone(&gstate);
            let ui_weak_cb = ui_weak.clone();
            let id_cb = id.clone();
            let display_id = {
                let guard = lock(&store);
                display_id_param(&guard)
            };
            let outcome = client.request(
                "set_wallpaper",
                serde_json::json!({ "wallpaper_id": id, "display_id": display_id }),
                move |result| match result {
                    Ok(_) => {
                        lock(&store_cb).set_active(id_cb);
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            after_store_change(&ui, &store_cb, &gstate_cb);
                        }
                    }
                    Err(error) => {
                        lock(&store_cb)
                            .record_error(format!("set_wallpaper 失败：{}", error.message));
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            show_toast(
                                &ui,
                                settings::TOAST_ERROR,
                                format!("设为壁纸失败：{}", error.message),
                            );
                            after_store_change(&ui, &store_cb, &gstate_cb);
                        }
                    }
                },
            );
            if let Err(error) = outcome {
                lock(&store).record_error(format!("set_wallpaper 请求发送失败：{error}"));
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, settings::TOAST_ERROR, "设为壁纸失败：无法与主程序通信");
                }
            }
        });
    }

    // ── 删除（SubTask 5.4）：先弹确认浮层，确认后才回落盘 ──
    {
        let ui_weak = ui.as_weak();
        ui.on_request_delete(move |id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_delete_id(id);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_cancel_delete(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_delete_id(SharedString::default());
            }
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let ui_weak = ui.as_weak();
        ui.on_confirm_delete(move |id: SharedString| {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_pending_delete_id(SharedString::default());
            let id = id.to_string();
            let store_cb = Arc::clone(&store);
            let ui_weak_cb = ui_weak.clone();
            let outcome = client.request(
                "remove_wallpaper",
                serde_json::json!({ "wallpaper_id": id, "delete_file": true }),
                move |result| {
                    if let Err(error) = result {
                        lock(&store_cb)
                            .record_error(format!("remove_wallpaper 失败：{}", error.message));
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            show_toast(
                                &ui,
                                settings::TOAST_ERROR,
                                format!("删除失败：{}", error.message),
                            );
                        }
                    }
                    // 成功时壳侧会推送 `wallpaper-removed`，由事件通路刷新，无需在此重复。
                },
            );
            if let Err(error) = outcome {
                lock(&store).record_error(format!("remove_wallpaper 请求发送失败：{error}"));
                show_toast(&ui, settings::TOAST_ERROR, "删除失败：无法与主程序通信");
            }
        });
    }

    // ── 预览（Task 8）：查库后交给 `preview` 模块按类型分发 ──
    {
        let store = Arc::clone(store);
        let ui_weak = ui.as_weak();
        ui.on_request_preview(move |id: SharedString| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let found = {
                let guard = lock(&store);
                guard
                    .index_of(id.as_str())
                    .and_then(|i| guard.wallpapers().get(i))
                    .map(|wp| (wp.file_path.clone(), wp.wallpaper_type))
            };
            let Some((file_path, wallpaper_type)) = found else {
                log(format!("预览请求：未找到壁纸 {id}"));
                show_toast(&ui, settings::TOAST_WARNING, "未找到该壁纸");
                return;
            };
            // 记录触发卡片：预览关闭后键盘焦点归还给它（Task 9.7.3）。
            ui.set_kb_focus_index(
                lock(&store)
                    .filtered_position_of(id.as_str())
                    .map(|p| p as i32)
                    .unwrap_or(-1),
            );
            #[cfg(windows)]
            preview::open(&ui, id.as_str(), &file_path, wallpaper_type);
            #[cfg(not(windows))]
            let _ = (id.as_str(), file_path, wallpaper_type);
        });
    }

    // ── 添加按钮（SubTask 5.6）：复用壳侧文件对话框，走与拖入同一条添加流水线 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let add_job = Arc::clone(add_job);
        let ui_weak = ui.as_weak();
        ui.on_add_requested(move || {
            let client_cb = Arc::clone(&client);
            let store_cb = Arc::clone(&store);
            let gstate_cb = Arc::clone(&gstate);
            let add_job_cb = Arc::clone(&add_job);
            let ui_weak_cb = ui_weak.clone();
            let outcome = client.request("open_file_dialog", Value::Null, move |result| {
                match result {
                    Ok(Value::String(path)) if !path.is_empty() => {
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            handle_dropped_paths(
                                &ui,
                                &client_cb,
                                &store_cb,
                                &gstate_cb,
                                &add_job_cb,
                                vec![path],
                            );
                        }
                    }
                    // 用户取消：壳侧返回 `null`，静默忽略。
                    Ok(_) => {}
                    Err(error) => {
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            show_toast(
                                &ui,
                                settings::TOAST_ERROR,
                                format!("打开文件对话框失败：{}", error.message),
                            );
                        }
                    }
                }
            });
            if let Err(error) = outcome {
                log(format!("open_file_dialog 请求发送失败：{error}"));
            }
        });
    }

    // ── 拖拽悬停：反算插入位并写回 `drop-index`（SubTask 5.2）──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_drag_insert_index(move |data: DataTransfer, x: f32, y: f32| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (geometry, scroll_y) = {
                let g = lock_state(&gstate);
                (g.geometry, g.scroll_y)
            };
            let count = lock(&store).filtered_count();
            // DropArea 的局部坐标即视口坐标；折算滚动偏移后进入内容坐标系。
            let index = dnd::insert_index(x as f64, y as f64 + scroll_y, &geometry, count);
            ui.set_drop_index(index as i32);
            // 记录拖拽源在过滤序列中的位置（邻居让位动画依据它计算位移）。
            let source = data
                .plain_text()
                .ok()
                .and_then(|text| lock(&store).filtered_position_of(text.as_str()))
                .map(|p| p as i32)
                .unwrap_or(-1);
            ui.set_drag_source_index(source);
        });
    }

    // ── 拖拽开始：按卡片缩略图区逻辑尺寸生成圆角半透明拖影（Task 3）──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_drag_started(move |id: SharedString, w: f32, h: f32| {
            let Some(ui) = ui_weak.upgrade() else { return };
            // 进入拖拽：冻结网格宽度变化引发的列数重算，保证拖拽期间几何稳定。
            lock_state(&gstate).dragging = true;
            // `length` 映射为 f32 逻辑像素；至少 1 避免零尺寸位图。
            let gw = w.round().max(1.0) as u32;
            let gh = h.round().max(1.0) as u32;
            // 候选缩略图路径与卡片显示一致（缩略图优先，静态图回退源图；Web/类型图标为空）。
            let candidates = {
                let guard = lock(&store);
                guard
                    .index_of(id.as_str())
                    .map(|i| card_state(&guard, &guard.wallpapers()[i]).1)
                    .unwrap_or_default()
            };
            let mut ghost = None;
            for path in &candidates {
                match thumbs::ghost_from_path(path, gw, gh, DRAG_GHOST_RADIUS, DRAG_GHOST_OPACITY) {
                    Ok(image) => {
                        ghost = Some(image);
                        break;
                    }
                    Err(error) => log(format!("拖影生成失败（{path}）：{error}")),
                }
            }
            match ghost {
                Some(image) => {
                    ui.set_drag_ghost(image);
                    // 热点居中：偏移取位图尺寸的一半（单位：图像像素）。
                    ui.set_drag_ghost_offset_x((gw / 2) as i32);
                    ui.set_drag_ghost_offset_y((gh / 2) as i32);
                }
                None => {
                    // 无可用图像：清空拖影，避免显示裸图/旧图（空 image 不绘制 overlay）。
                    ui.set_drag_ghost(Image::default());
                }
            }
        });
    }

    // ── 拖拽落下：本地乐观重排 + 经壳落盘（SubTask 5.1）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_drag_commit(move |data: DataTransfer, index: i32| {
            let Some(ui) = ui_weak.upgrade() else { return };
            // 先解冻并补算拖拽期间可能发生的宽度变化（早于负载解析，失败也已解冻）。
            let last_width = {
                let mut g = lock_state(&gstate);
                g.dragging = false;
                g.last_grid_width
            };
            if last_width > 0.0 {
                recompute_columns(&ui, &store, &gstate, last_width);
            }
            ui.set_drop_index(-1);
            ui.set_drag_source_index(-1);
            // 落下即释放拖影位图。
            ui.set_drag_ghost(Image::default());
            let Ok(payload) = data.plain_text() else {
                log("拖拽提交缺少负载文本，忽略");
                return;
            };
            let id = payload.to_string();
            let ids = {
                let guard = lock(&store);
                let Some(from) = guard.index_of(&id) else {
                    return;
                };
                let current = guard.all_ids();
                let next = dnd::reorder_ids(&current, from, index.max(0) as usize);
                if next == current {
                    return; // 原位落下：无变化，不落盘
                }
                next
            };
            if !lock(&store).reorder_local(&ids) {
                log("拖拽重排被本地拒绝（id 集合不一致），放弃本次");
                return;
            }
            after_store_change(&ui, &store, &gstate);

            let client_cb = Arc::clone(&client);
            let store_cb = Arc::clone(&store);
            let gstate_cb = Arc::clone(&gstate);
            let ui_weak_cb = ui_weak.clone();
            let outcome = client.request(
                "reorder_wallpapers",
                serde_json::json!({ "ids": ids }),
                move |result| {
                    if let Err(error) = result {
                        lock(&store_cb)
                            .record_error(format!("reorder_wallpapers 失败：{}", error.message));
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            show_toast(&ui, settings::TOAST_WARNING, "排序未保存，正在重新同步");
                        }
                        // 落盘失败：重新拉取全量库兜底，避免本地顺序与壳侧长期分叉。
                        fetch_snapshot(
                            &client_cb,
                            "get_wallpapers",
                            &store_cb,
                            &gstate_cb,
                            &ui_weak_cb,
                            apply_wallpapers_value,
                        );
                    }
                },
            );
            if let Err(error) = outcome {
                lock(&store).record_error(format!("reorder_wallpapers 请求发送失败：{error}"));
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, settings::TOAST_ERROR, "排序未保存：无法与主程序通信");
                }
            }
        });
    }

    // 拖拽结束（无论是否落下）都清除插入指示线。
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_drag_ended(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_drop_index(-1);
                ui.set_drag_source_index(-1);
                // 释放拖影位图。
                ui.set_drag_ghost(Image::default());
                // 解冻并补算拖拽期间可能发生的宽度变化。
                let last_width = {
                    let mut g = lock_state(&gstate);
                    g.dragging = false;
                    g.last_grid_width
                };
                if last_width > 0.0 {
                    recompute_columns(&ui, &store, &gstate, last_width);
                }
            }
        });
    }
}

/// 挂载原生文件拖入接收器（SubTask 5.5；仅 Windows 有实现）。
#[cfg(windows)]
fn install_native_drop_target(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    add_job: &Arc<Mutex<AddJob>>,
) {
    let client = Arc::clone(client);
    let store = Arc::clone(store);
    let gstate = Arc::clone(gstate);
    let add_job = Arc::clone(add_job);
    let ui_weak = ui.as_weak();
    let handler: drop_target::DropHandler = Arc::new(move |paths: Vec<String>| {
        let client = Arc::clone(&client);
        let store = Arc::clone(&store);
        let gstate = Arc::clone(&gstate);
        let add_job = Arc::clone(&add_job);
        let ui_weak = ui_weak.clone();
        // 窗口过程已在 UI 线程调用，但仍投递回事件循环：与其余回合同序，且不在消息处理中期做重活。
        let dispatched = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                handle_dropped_paths(&ui, &client, &store, &gstate, &add_job, paths);
            }
        });
        if dispatched.is_err() {
            log("拖入文件投递到事件循环失败（事件循环已退出）");
        }
    });
    // 悬停态回调（Task 9.7.4）：原生侧 DragEnter/DragLeave 驱动网格高亮层。
    let hover_ui = ui.as_weak();
    let hover: drop_target::HoverHandler = Arc::new(move |over: bool| {
        let hover_ui = hover_ui.clone();
        let dispatched = slint::invoke_from_event_loop(move || {
            if let Some(ui) = hover_ui.upgrade() {
                ui.set_file_drag_over(over);
            }
        });
        if dispatched.is_err() {
            log("文件拖入悬停态投递到事件循环失败（事件循环已退出）");
        }
    });

    match drop_target::install(ui.window(), handler, hover) {
        Ok(()) => {}
        Err(error) => log(format!("原生文件拖入接收器安装失败：{error}")),
    }
}

#[cfg(not(windows))]
fn install_native_drop_target(
    _ui: &MainWindow,
    _client: &Arc<UiClient>,
    _store: &Arc<Mutex<AppStore>>,
    _gstate: &Arc<Mutex<GridUiState>>,
    _add_job: &Arc<Mutex<AddJob>>,
) {
    log("当前平台不支持原生文件拖入（仅 Windows 实现）");
}

/// 处理一批拖入/选中的文件路径（SubTask 5.5 / 5.6）：过滤不支持与重复，入队串行添加。
fn handle_dropped_paths(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    add_job: &Arc<Mutex<AddJob>>,
    paths: Vec<String>,
) {
    if paths.is_empty() {
        return;
    }
    let existing: Vec<String> = lock(store)
        .wallpapers()
        .iter()
        .map(|wp| wp.file_path.clone())
        .collect();
    let merged = dnd::merge_drop_paths(&paths, &existing);
    if !merged.unsupported.is_empty() || !merged.duplicate.is_empty() {
        log(format!(
            "拖入文件被跳过 {} 个（不支持 {}，重复 {}）",
            merged.unsupported.len() + merged.duplicate.len(),
            merged.unsupported.len(),
            merged.duplicate.len()
        ));
    }
    {
        let mut job = lock_job(add_job);
        job.rejected += (merged.unsupported.len() + merged.duplicate.len()) as u32;
        job.queue.extend(merged.accepted);
    }
    let ui_weak = ui.as_weak();
    pump_add_jobs(client, store, gstate, &ui_weak, add_job);
}

/// 串行推进批量添加：一次只提交一条 `add_wallpaper`，响应到达后续投下一条。
///
/// 循环用于消化"发送即失败"（无响应可等）的情形；队列耗尽则汇总计数并弹 Toast。
/// **不递归**：成功提交后直接 `return`，由响应回调重新进入本函数，栈深度恒定。
fn pump_add_jobs(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
    add_job: &Arc<Mutex<AddJob>>,
) {
    loop {
        let next = {
            let mut job = lock_job(add_job);
            if job.running {
                return; // 已有请求在途：由它的回调续投，避免并发
            }
            match job.queue.pop_front() {
                Some(path) => {
                    job.running = true;
                    Some(path)
                }
                None => None,
            }
        };

        let Some(path) = next else {
            let (added, failed, rejected) = {
                let mut job = lock_job(add_job);
                let counts = (job.added, job.failed, job.rejected);
                job.added = 0;
                job.failed = 0;
                job.rejected = 0;
                counts
            };
            finish_add_jobs(ui_weak, added, failed, rejected);
            return;
        };

        let client_cb = Arc::clone(client);
        let store_cb = Arc::clone(store);
        let gstate_cb = Arc::clone(gstate);
        let ui_weak_cb = ui_weak.clone();
        let add_job_cb = Arc::clone(add_job);
        let outcome = client.request(
            "add_wallpaper",
            serde_json::json!({ "file_path": path }),
            move |result| {
                {
                    let mut job = lock_job(&add_job_cb);
                    job.running = false;
                    match result {
                        Ok(_) => job.added += 1,
                        Err(error) => {
                            job.failed += 1;
                            log(format!("add_wallpaper 失败：{}", error.message));
                        }
                    }
                }
                if let Some(ui) = ui_weak_cb.upgrade() {
                    after_store_change(&ui, &store_cb, &gstate_cb);
                }
                pump_add_jobs(&client_cb, &store_cb, &gstate_cb, &ui_weak_cb, &add_job_cb);
            },
        );
        if let Err(error) = outcome {
            let mut job = lock_job(add_job);
            job.running = false;
            job.failed += 1;
            drop(job);
            log(format!("add_wallpaper 请求发送失败：{error}"));
            continue; // 取下一条，避免一条写失败就中断整批
        }
        // 请求已发出：等待响应回调续投。
        return;
    }
}

/// 批量添加收尾：汇总一条 Toast（成功/失败/跳过）并记录日志。
fn finish_add_jobs(ui_weak: &slint::Weak<MainWindow>, added: u32, failed: u32, rejected: u32) {
    if added == 0 && failed == 0 && rejected == 0 {
        return;
    }
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("已添加 {added} 个"));
    }
    if failed > 0 {
        parts.push(format!("{failed} 个失败"));
    }
    if rejected > 0 {
        parts.push(format!("{rejected} 个已跳过"));
    }
    let message = parts.join("，");
    log(format!("批量添加结束：{message}"));
    if let Some(ui) = ui_weak.upgrade() {
        let level = if failed > 0 || rejected > 0 {
            settings::TOAST_WARNING
        } else {
            settings::TOAST_INFO
        };
        show_toast(&ui, level, message);
    }
}

// ── 设置面板（Task 6 / Task 7）───────────────────────────────────────────────

/// 把回滚值写回状态仓。
fn apply_revert(store: &Arc<Mutex<AppStore>>, revert: Revert) {
    let mut guard = lock(store);
    match revert {
        Revert::None => {}
        Revert::Volume(v) => guard.set_volume(v),
        Revert::Mute(v) => guard.set_muted(v),
        Revert::Speed(v) => guard.set_speed(v),
        Revert::AutoStart(v) => guard.apply_auto_start(v),
        Revert::InteractionMode(v) => guard.set_interaction_mode(v),
        Revert::Scaling { key, mode } => guard.set_scaling_mode_for_key(key, mode),
    }
}

/// 目标显示器 id 参数；无选择时传 `null`（壳侧 `Params::get` 视为缺省）。
fn display_id_param(store: &AppStore) -> Value {
    match store.selected_display() {
        Some(id) => Value::String(id.to_string()),
        None => Value::Null,
    }
}

/// 缩放命令的 `display_id` 参数：`per_monitor` 传具体选中显示器（无选择传 `null`，保持既有
/// `display_id_param` 语义）；其余排列传 `"all"`（壳侧广播到全部活跃显示器）。
fn scaling_display_id_param(store: &AppStore) -> Value {
    match store.arrangement() {
        Arrangement::PerMonitor => display_id_param(store),
        Arrangement::AllSame | Arrangement::Span => Value::String("all".to_string()),
    }
}

/// 播放状态查询的 `display_id`：PerMonitor 传选中显示器（与实际控制目标一致），
/// 其余排列传 null——`"all"` 是广播键，没有单屏状态可言，交给壳侧回退到活跃显示器。
fn playback_state_display_id(store: &AppStore) -> Value {
    match store.arrangement() {
        Arrangement::PerMonitor => display_id_param(store),
        Arrangement::AllSame | Arrangement::Span => Value::Null,
    }
}

/// 设置 Toast：先写级别再写文本（文本非空即触发显示与自动消隐）。
fn show_toast(ui: &MainWindow, level: i32, text: impl Into<SharedString>) {
    ui.set_toast_level(level);
    ui.set_toast_text(text.into());
}

/// 把一条配置补丁经 `update_config` 落盘。
///
/// 音量 / 静音 / 速度的运行时命令（`set_volume` / `toggle_mute` / `set_speed`）只作用于
/// 正在运行的 mpv，**不回写 `config.toml`**；若不额外落盘，重开主窗口时会从配置重新
/// 种子化回默认值。补丁先经 `merge_config_patch` 合并为完整配置（乐观写内存），再提交。
fn persist_config_patch(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    patch: Value,
    error_label: &'static str,
) {
    let Some(config) = lock(store).merge_config_patch(&patch) else {
        return;
    };
    enqueue_mutation(
        ui_weak,
        client,
        store,
        gstate,
        Mutation {
            command: "update_config",
            args: json!({ "config": config }),
            after: AfterMutation::None,
            revert: Revert::None,
            error_label,
        },
    );
}

/// 提交播放速度：乐观写内存 → 落盘 `video.speed` → 下发引擎，三处一致。
///
/// `speed` 为速度本身（非滑块归一化值），调用方需已 sanitize。
fn apply_speed(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    speed: f32,
) {
    let previous = {
        let mut guard = lock(store);
        let previous = guard.speed();
        guard.set_speed(speed);
        previous
    };
    let display_id = {
        let guard = lock(store);
        display_id_param(&guard)
    };
    if let Some(ui) = ui_weak.upgrade() {
        after_store_change(&ui, store, gstate);
    }
    persist_config_patch(
        ui_weak,
        client,
        store,
        gstate,
        json!({ "video": { "speed": speed } }),
        "保存播放速度",
    );
    enqueue_mutation(
        ui_weak,
        client,
        store,
        gstate,
        Mutation {
            command: "set_speed",
            args: json!({ "display_id": display_id, "speed": speed }),
            // 内存态已乐观更新；−/+ 连点时不额外拉配置，避免每次点击都触发设置面板重投影。
            after: AfterMutation::None,
            revert: Revert::Speed(previous),
            error_label: "设置播放速度",
        },
    );
}

/// 入队一条写回意图；若泵空闲则立即启动。
fn enqueue_mutation(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    mutation: Mutation,
) {
    let start = {
        let mut g = lock_state(gstate);
        if g.mutations.enqueue(mutation) {
            g.mutations.begin_next()
        } else {
            None
        }
    };
    if let Some(mutation) = start {
        send_mutation(ui_weak, client, store, gstate, mutation);
    }
}

/// 取下一条待办并提交（由完成回调续投，栈深恒定）。
fn pump_mutations(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    let next = lock_state(gstate).mutations.begin_next();
    if let Some(mutation) = next {
        send_mutation(ui_weak, client, store, gstate, mutation);
    }
}

/// 提交一条写回：成功补拉、失败回滚并提示，随后续投队列（一次只有一条在途）。
fn send_mutation(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    mutation: Mutation,
) {
    let Mutation {
        command,
        args,
        after,
        revert,
        error_label,
    } = mutation;
    let client_cb = Arc::clone(client);
    let store_cb = Arc::clone(store);
    let gstate_cb = Arc::clone(gstate);
    let ui_weak_cb = ui_weak.clone();
    let revert_cb = revert.clone();
    let outcome = client.request(command, args, move |result| {
        match result {
            // `toggle_mute` 返回新的静音态，据实回填（其余命令无返回语义）。
            Ok(value) => {
                if command == "toggle_mute" {
                    if let Some(muted) = value.as_bool() {
                        lock(&store_cb).set_muted(muted);
                    }
                }
            }
            Err(error) => {
                apply_revert(&store_cb, revert_cb);
                lock(&store_cb).record_error(format!("{command} 失败：{}", error.message));
                if let Some(ui) = ui_weak_cb.upgrade() {
                    show_toast(
                        &ui,
                        settings::TOAST_ERROR,
                        format!("{error_label}失败：{}", error.message),
                    );
                    after_store_change(&ui, &store_cb, &gstate_cb);
                }
            }
        }
        if let Some(ui) = ui_weak_cb.upgrade() {
            run_after_mutation(after, &ui, &client_cb, &store_cb, &gstate_cb);
        }
        if lock_state(&gstate_cb).mutations.finish() {
            pump_mutations(&ui_weak_cb, &client_cb, &store_cb, &gstate_cb);
        }
    });
    // 发送即失败：无响应可等，就地回滚并续投。
    if let Err(error) = outcome {
        apply_revert(store, revert);
        lock(store).record_error(format!("{command} 请求发送失败：{error}"));
        if let Some(ui) = ui_weak.upgrade() {
            show_toast(
                &ui,
                settings::TOAST_ERROR,
                format!("{error_label}失败：无法与主程序通信"),
            );
            after_store_change(&ui, store, gstate);
        }
        if lock_state(gstate).mutations.finish() {
            pump_mutations(ui_weak, client, store, gstate);
        }
    }
}

/// 写回成功 / 失败后的补拉策略。
fn run_after_mutation(
    after: AfterMutation,
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    let ui_weak = ui.as_weak();
    match after {
        AfterMutation::None => {}
        AfterMutation::Settings => fetch_settings_snapshot(client, store, gstate, &ui_weak),
        AfterMutation::PoolsAndUnits => fetch_pools_units(client, store, gstate, &ui_weak),
    }
}

/// 全量回填设置快照（`config-changed` 与配置类写回成功后调用）。
fn fetch_settings_snapshot(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
) {
    fetch_snapshot(
        client,
        "get_config",
        store,
        gstate,
        ui_weak,
        apply_config_value,
    );
    fetch_snapshot(
        client,
        "get_arrangement",
        store,
        gstate,
        ui_weak,
        apply_arrangement_value,
    );
    fetch_snapshot(
        client,
        "get_rotation_config",
        store,
        gstate,
        ui_weak,
        apply_rotation_value,
    );
    fetch_snapshot(
        client,
        "get_auto_start_status",
        store,
        gstate,
        ui_weak,
        apply_auto_start_value,
    );
    fetch_pools_units(client, store, gstate, ui_weak);
}

/// 回填池列表与单元状态。
fn fetch_pools_units(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
) {
    fetch_snapshot(
        client,
        "list_pools",
        store,
        gstate,
        ui_weak,
        apply_pools_value,
    );
    fetch_snapshot(
        client,
        "get_unit_states",
        store,
        gstate,
        ui_weak,
        apply_units_value,
    );
}

/// 提交指定池的名称 + 成员（去重）到壳侧。
fn submit_pool_update(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    pool_id: &str,
    error_label: &'static str,
) {
    let payload = {
        let guard = lock(store);
        guard
            .pools()
            .iter()
            .find(|p| p.id == pool_id)
            .map(|p| (p.name.clone(), settings::dedupe_members(&p.member_ids)))
    };
    let Some((name, members)) = payload else {
        return;
    };
    enqueue_mutation(
        ui_weak,
        client,
        store,
        gstate,
        Mutation {
            command: "update_pool",
            args: json!({ "id": pool_id, "name": name, "member_ids": members }),
            after: AfterMutation::PoolsAndUnits,
            revert: Revert::None,
            error_label,
        },
    );
}

/// 提交轮换配置（乐观本地更新后经 `update_rotation_config` 落盘）。
fn submit_rotation(
    ui_weak: &slint::Weak<MainWindow>,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    next: RotationConfig,
    error_label: &'static str,
) {
    let Ok(config) = serde_json::to_value(&next) else {
        return;
    };
    enqueue_mutation(
        ui_weak,
        client,
        store,
        gstate,
        Mutation {
            command: "update_rotation_config",
            args: json!({ "config": config }),
            after: AfterMutation::Settings,
            revert: Revert::None,
            error_label,
        },
    );
}

/// 提交一次"上一张 / 下一张"并重查播放状态。
fn request_step(
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
    ui_weak: &slint::Weak<MainWindow>,
    command: &'static str,
) {
    let key = {
        let guard = lock(store);
        guard.current_unit_key()
    };
    let client_cb = Arc::clone(client);
    let store_cb = Arc::clone(store);
    let gstate_cb = Arc::clone(gstate);
    let ui_weak_cb = ui_weak.clone();
    let outcome = client.request(command, json!({ "key": key }), move |result| {
        if let Err(error) = result {
            lock(&store_cb).record_error(format!("{command} 失败：{}", error.message));
            if let Some(ui) = ui_weak_cb.upgrade() {
                show_toast(
                    &ui,
                    settings::TOAST_ERROR,
                    format!("切换壁纸失败：{}", error.message),
                );
            }
        }
        fetch_playback_state(&client_cb, &store_cb, &gstate_cb, &ui_weak_cb);
    });
    if let Err(error) = outcome {
        lock(store).record_error(format!("{command} 请求发送失败：{error}"));
    }
}

/// 注册设置面板（Task 6 / Task 7）全部回调。
fn install_settings_handlers(
    ui: &MainWindow,
    client: &Arc<UiClient>,
    store: &Arc<Mutex<AppStore>>,
    gstate: &Arc<Mutex<GridUiState>>,
) {
    // ── 面板开合 ──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_settings_toggled(move || {
            {
                let mut guard = lock(&store);
                let open = guard.settings_open();
                guard.set_settings_open(!open);
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
        });
    }

    // ── 目标显示器（下拉首项为"全部显示器"，值空串）──
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_display_selected(move |index: i32| {
            {
                let mut guard = lock(&store);
                let value = guard
                    .display_options()
                    .get(index.max(0) as usize)
                    .map(|o| o.value.clone())
                    .unwrap_or_default();
                guard.select_display(if value.is_empty() { None } else { Some(value) });
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
        });
    }

    // ── 缩放模式（按显示器持久化 + 运行时应用）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_scaling_selected(move |index: i32| {
            let Some(value) = settings::value_at(&settings::SCALING_MODES, index) else {
                return;
            };
            // 窄化为合法枚举，非法值直接拒绝（选项表与解析表保持一致）。
            let Some(mode) = settings::scaling_from_value(value) else {
                return;
            };
            // 1) 解析目标 key 与该 key 的旧值；2) 乐观更新内存映射；3) 取出更新后的完整映射。
            let (key, previous, modes) = {
                let mut guard = lock(&store);
                let key = guard.scaling_key();
                let previous = guard.scaling_mode_for_key(&key);
                guard.set_scaling_mode_for_key(key.clone(), mode);
                (
                    key,
                    previous,
                    serde_json::to_value(guard.scaling_modes_map()).ok(),
                )
            };
            let Some(modes) = modes else {
                return;
            };
            // 落盘：发送**完整** modes 映射（而非单条），避免浅合并把其它显示器的记忆项冲掉。
            let patch = json!({ "scaling": { "modes": modes } });
            let Some(config) = lock(&store).merge_config_patch(&patch) else {
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, settings::TOAST_ERROR, "设置失败：配置尚未加载");
                }
                return;
            };
            let display_id = {
                let guard = lock(&store);
                scaling_display_id_param(&guard)
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            // 先经 `update_config` 持久化 `scaling.modes`（壳侧不落该运行时命令），
            // 再经 `set_scaling_mode` 应用到引擎；失败时按 key 回滚内存值。
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "update_config",
                    args: json!({ "config": config }),
                    after: AfterMutation::None,
                    revert: Revert::None,
                    error_label: "设置缩放模式",
                },
            );
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "set_scaling_mode",
                    args: json!({ "display_id": display_id, "mode": mode }),
                    after: AfterMutation::Settings,
                    revert: Revert::Scaling {
                        key,
                        mode: previous,
                    },
                    error_label: "设置缩放模式",
                },
            );
        });
    }

    // ── 多屏排列 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_arrangement_selected(move |index: i32| {
            let Some(value) = settings::value_at(&settings::ARRANGEMENTS, index) else {
                return;
            };
            let Some(arrangement) = settings::arrangement_from_value(value) else {
                return;
            };
            lock(&store).apply_arrangement(arrangement);
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "update_arrangement",
                    args: json!({ "arrangement": value }),
                    after: AfterMutation::Settings,
                    revert: Revert::None,
                    error_label: "设置多屏排列",
                },
            );
        });
    }

    // ── 全屏处置（配置字段，走 update_config 深合并）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_fullscreen_selected(move |index: i32| {
            let Some(value) = settings::value_at(&settings::FULLSCREEN_ACTIONS, index) else {
                return;
            };
            // 窄化为合法枚举，非法值直接拒绝（选项表与解析表保持一致）。
            if settings::fullscreen_from_value(value).is_none() {
                return;
            }
            let patch = json!({ "pause": { "fullscreen_action": value } });
            let payload = lock(&store).merge_config_patch(&patch);
            let Some(config) = payload else {
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, settings::TOAST_ERROR, "设置失败：配置尚未加载");
                }
                return;
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "update_config",
                    args: json!({ "config": config }),
                    after: AfterMutation::Settings,
                    revert: Revert::None,
                    error_label: "设置全屏处置",
                },
            );
        });
    }

    // ── 播放：暂停/恢复（照搬 updatePlaybackButtons 的 pause_action 分流）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pause_toggled(move || {
            let (command, display_id) = {
                let guard = lock(&store);
                let buttons = settings::playback_buttons(guard.playback(), guard.has_target());
                let command = if buttons.pause_action == "resume" {
                    "resume_wallpaper"
                } else {
                    "pause_wallpaper"
                };
                (command, display_id_param(&guard))
            };
            let client_cb = Arc::clone(&client);
            let store_cb = Arc::clone(&store);
            let gstate_cb = Arc::clone(&gstate);
            let ui_weak_cb = ui_weak.clone();
            let outcome = client.request(
                command,
                json!({ "display_id": display_id }),
                move |result| {
                    if let Err(error) = result {
                        lock(&store_cb).record_error(format!("{command} 失败：{}", error.message));
                        if let Some(ui) = ui_weak_cb.upgrade() {
                            show_toast(
                                &ui,
                                settings::TOAST_ERROR,
                                format!("播放控制失败：{}", error.message),
                            );
                        }
                    }
                    fetch_playback_state(&client_cb, &store_cb, &gstate_cb, &ui_weak_cb);
                },
            );
            if let Err(error) = outcome {
                lock(&store).record_error(format!("{command} 请求发送失败：{error}"));
            }
        });
    }

    // ── 播放：上一张 / 下一张 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_prev_requested(move || {
            request_step(&client, &store, &gstate, &ui_weak, "prev_wallpaper");
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_next_requested(move || {
            request_step(&client, &store, &gstate, &ui_weak, "next_wallpaper");
        });
    }

    // ── 播放：音量 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_volume_committed(move |value: f32| {
            let previous = {
                let mut guard = lock(&store);
                let previous = guard.volume();
                guard.set_volume(value);
                previous
            };
            let display_id = {
                let guard = lock(&store);
                display_id_param(&guard)
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            // 落盘 `audio.volume`：`set_volume` 只作用于运行中的 mpv，不回写配置。
            persist_config_patch(
                &ui_weak,
                &client,
                &store,
                &gstate,
                json!({ "audio": { "volume": value } }),
                "保存音量",
            );
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "set_volume",
                    args: json!({ "display_id": display_id, "volume": value }),
                    // 内存态已乐观更新，无需再拉配置；`update_config` 的 `config-changed`
                    // 回显也不会覆盖运行时值（`audio_initialized` 门控）。
                    after: AfterMutation::None,
                    revert: Revert::Volume(previous),
                    error_label: "设置音量",
                },
            );
        });
    }

    // ── 播放：静音（`toggle_mute` 返回新态，据实回填）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_mute_toggled(move |value: bool| {
            let previous = {
                let mut guard = lock(&store);
                let previous = guard.muted();
                guard.set_muted(value);
                previous
            };
            let display_id = {
                let guard = lock(&store);
                display_id_param(&guard)
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            // 落盘 `audio.muted`：`toggle_mute` 只作用于运行中的 mpv，不回写配置。
            persist_config_patch(
                &ui_weak,
                &client,
                &store,
                &gstate,
                json!({ "audio": { "muted": value } }),
                "保存静音",
            );
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "toggle_mute",
                    args: json!({ "display_id": display_id }),
                    // 新静音态由命令返回值据实回填，无需再拉配置。
                    after: AfterMutation::None,
                    revert: Revert::Mute(previous),
                    error_label: "切换静音",
                },
            );
        });
    }

    // ── 播放：速度（滑块归一化值 → [0.25,4.0]）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_speed_committed(move |value: f32| {
            let speed = settings::slider_to_speed(value);
            apply_speed(&ui_weak, &client, &store, &gstate, speed);
        });
    }

    // ── 播放：速度微调（−/+ 增量，单位即速度本身）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_speed_stepped(move |delta: f32| {
            // 先量化到 0.01 再 clamp，避免连续累加产生浮点漂移（显示保留两位小数）。
            let speed = {
                let guard = lock(&store);
                settings::sanitize_speed(((guard.speed() + delta) * 100.0).round() / 100.0)
            };
            apply_speed(&ui_weak, &client, &store, &gstate, speed);
        });
    }

    // ── 系统：开机自启 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_auto_start_toggled(move |value: bool| {
            {
                let mut guard = lock(&store);
                guard.apply_auto_start(value);
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "toggle_auto_start",
                    args: json!({ "enabled": value }),
                    after: AfterMutation::Settings,
                    revert: Revert::AutoStart(!value),
                    error_label: "设置开机自启",
                },
            );
        });
    }

    // ── 系统：电池供电时暂停（配置字段）──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pause_on_battery_toggled(move |value: bool| {
            let patch = json!({ "pause": { "pause_on_battery": value } });
            let payload = lock(&store).merge_config_patch(&patch);
            let Some(config) = payload else {
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, settings::TOAST_ERROR, "设置失败：配置尚未加载");
                }
                return;
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "update_config",
                    args: json!({ "config": config }),
                    after: AfterMutation::Settings,
                    revert: Revert::None,
                    error_label: "设置电池暂停",
                },
            );
        });
    }

    // ── 系统：鼠标交互模式 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_interaction_mode_toggled(move |value: bool| {
            let previous = {
                let mut guard = lock(&store);
                let previous = guard.interaction_mode();
                guard.set_interaction_mode(value);
                previous
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "set_interaction_mode",
                    args: json!({ "enabled": value }),
                    after: AfterMutation::None,
                    revert: Revert::InteractionMode(previous),
                    error_label: "设置鼠标交互",
                },
            );
        });
    }

    // ── 轮换：启用 / 开机推进 / 间隔 / 采样算法 ──
    for field in [RotationField::Enabled, RotationField::OnBoot] {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        let handler = move |value: bool| {
            let next = {
                let mut guard = lock(&store);
                let Some(mut rotation) = guard.rotation().cloned() else {
                    return;
                };
                match field {
                    RotationField::Enabled => rotation.enabled = value,
                    RotationField::OnBoot => rotation.on_boot = value,
                }
                guard.apply_rotation(rotation.clone());
                rotation
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_rotation(&ui_weak, &client, &store, &gstate, next, "设置轮换开关");
        };
        match field {
            RotationField::Enabled => ui.on_rotation_enabled_toggled(handler),
            RotationField::OnBoot => ui.on_rotation_on_boot_toggled(handler),
        }
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let store_for_step = Arc::clone(&store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        // ±1 步进与回车提交共用一条通路（文本非法时 sanitize 兜底）。
        let apply_interval = move |value: i32| {
            let next = {
                let mut guard = lock(&store);
                let Some(mut rotation) = guard.rotation().cloned() else {
                    return;
                };
                rotation.interval_minutes = settings::sanitize_interval(value) as u32;
                guard.apply_rotation(rotation.clone());
                rotation
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_rotation(&ui_weak, &client, &store, &gstate, next, "设置轮换间隔");
        };
        {
            let apply_interval = apply_interval.clone();
            ui.on_rotation_interval_step(move |delta: i32| {
                let current = {
                    lock(&store_for_step)
                        .rotation()
                        .map(|r| r.interval_minutes as i32)
                        .unwrap_or(30)
                };
                apply_interval(current + delta);
            });
        }
        ui.on_rotation_interval_submitted(move |text: SharedString| {
            let parsed = text.trim().parse::<i32>().unwrap_or(30);
            apply_interval(parsed);
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_order_selected(move |index: i32| {
            let Some(value) = settings::value_at(&settings::ORDERS, index) else {
                return;
            };
            let Some(order) = settings::order_from_value(value) else {
                return;
            };
            let next = {
                let mut guard = lock(&store);
                let Some(mut rotation) = guard.rotation().cloned() else {
                    return;
                };
                rotation.order = order;
                guard.apply_rotation(rotation.clone());
                rotation
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_rotation(&ui_weak, &client, &store, &gstate, next, "设置采样算法");
        });
    }

    // ── 壁纸池：新建 / 选择 / 排序 / 删除 / 重命名 / 成员编辑 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_create_requested(move || {
            let name = {
                let guard = lock(&store);
                format!("壁纸池 {}", guard.pools().len() + 1)
            };
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "create_pool",
                    args: json!({ "name": name, "member_ids": [] }),
                    after: AfterMutation::PoolsAndUnits,
                    revert: Revert::None,
                    error_label: "新建壁纸池",
                },
            );
        });
    }
    {
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_select(move |id: SharedString| {
            lock(&store).set_editing_pool(Some(id.to_string()));
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_move(move |id: SharedString, delta: i32| {
            let ids = {
                let mut guard = lock(&store);
                let Some(from) = guard.pools().iter().position(|p| p.id == id.as_str()) else {
                    return;
                };
                let to = from as i32 + delta;
                if to < 0 {
                    return;
                }
                guard.move_pool(from, to as usize)
            };
            let Some(ids) = ids else {
                return;
            };
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "reorder_pools",
                    args: json!({ "ids": ids }),
                    after: AfterMutation::PoolsAndUnits,
                    revert: Revert::None,
                    error_label: "调整池顺序",
                },
            );
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_pool_delete_requested(move |id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_delete_pool_id(id);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_pool_delete_cancelled(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_delete_pool_id(SharedString::default());
            }
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_delete_confirmed(move |id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_delete_pool_id(SharedString::default());
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "delete_pool",
                    args: json!({ "id": id.as_str() }),
                    after: AfterMutation::PoolsAndUnits,
                    revert: Revert::None,
                    error_label: "删除壁纸池",
                },
            );
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_rename(move |name: SharedString| {
            let name = name.trim().to_string();
            if name.is_empty() {
                return;
            }
            let Some(pool_id) = ({ lock(&store).editing_pool().map(str::to_string) }) else {
                return;
            };
            if !lock(&store).rename_pool(&pool_id, name) {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_pool_update(&ui_weak, &client, &store, &gstate, &pool_id, "重命名壁纸池");
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_member_move(move |id: SharedString, delta: i32| {
            let Some(pool_id) = ({ lock(&store).editing_pool().map(str::to_string) }) else {
                return;
            };
            {
                let mut guard = lock(&store);
                let Some(from) = guard
                    .pool_member_ids(&pool_id)
                    .and_then(|ids| ids.iter().position(|m| m == id.as_str()))
                else {
                    return;
                };
                let to = from as i32 + delta;
                if to < 0 {
                    return;
                }
                if guard
                    .move_pool_member(&pool_id, from, to as usize)
                    .is_none()
                {
                    return;
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_pool_update(&ui_weak, &client, &store, &gstate, &pool_id, "调整成员顺序");
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_member_remove(move |id: SharedString| {
            let Some(pool_id) = ({ lock(&store).editing_pool().map(str::to_string) }) else {
                return;
            };
            if lock(&store)
                .remove_pool_member(&pool_id, id.as_str())
                .is_none()
            {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_pool_update(&ui_weak, &client, &store, &gstate, &pool_id, "移除池成员");
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_pool_member_add(move |id: SharedString| {
            let Some(pool_id) = ({ lock(&store).editing_pool().map(str::to_string) }) else {
                return;
            };
            if lock(&store)
                .add_pool_member(&pool_id, id.as_str())
                .is_none()
            {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            submit_pool_update(&ui_weak, &client, &store, &gstate, &pool_id, "添加池成员");
        });
    }

    // ── 单元配置：激活池 / 启用态 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_unit_pool_selected(move |key: SharedString, index: i32| {
            let key = key.to_string();
            let value = {
                let guard = lock(&store);
                guard
                    .pool_options()
                    .get(index.max(0) as usize)
                    .map(|o| o.value.clone())
                    .unwrap_or_default()
            };
            let pool_id = if value.is_empty() {
                None
            } else {
                Some(value.clone())
            };
            lock(&store).set_unit_active_pool(&key, pool_id);
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            let pool_param = if value.is_empty() {
                Value::Null
            } else {
                Value::String(value)
            };
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "set_active_pool",
                    args: json!({ "key": key, "pool_id": pool_param }),
                    after: AfterMutation::PoolsAndUnits,
                    revert: Revert::None,
                    error_label: "设置激活池",
                },
            );
        });
    }
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_unit_enabled_toggled(move |key: SharedString, value: bool| {
            let key = key.to_string();
            lock(&store).set_unit_enabled(&key, value);
            if let Some(ui) = ui_weak.upgrade() {
                after_store_change(&ui, &store, &gstate);
            }
            enqueue_mutation(
                &ui_weak,
                &client,
                &store,
                &gstate,
                Mutation {
                    command: "set_rotation_enabled",
                    args: json!({ "key": key, "enabled": value }),
                    after: AfterMutation::PoolsAndUnits,
                    revert: Revert::None,
                    error_label: "设置单元轮换",
                },
            );
        });
    }

    // ── 防抖到期：合并窗口结束时才真正查询 / 弹窗 ──
    {
        let client = Arc::clone(client);
        let store = Arc::clone(store);
        let gstate = Arc::clone(gstate);
        let ui_weak = ui.as_weak();
        ui.on_playback_refresh_tick(move || {
            fetch_playback_state(&client, &store, &gstate, &ui_weak);
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_rotated_toast_tick(move || {
            if let Some(ui) = ui_weak.upgrade() {
                show_toast(&ui, settings::TOAST_SUCCESS, "已自动切换壁纸");
            }
        });
    }

    // ── 关于面板：点击「项目仓库」在默认浏览器打开仓库 ──
    ui.on_open_repo(move || {
        #[cfg(windows)]
        open_in_browser(REPO_URL);
    });
}

/// 轮换配置中由开关切换的字段。
#[derive(Clone, Copy)]
enum RotationField {
    Enabled,
    OnBoot,
}

// ── 快照解析函数（fn 指针，便于作为回调捕获）──────────────────────────────────

fn apply_config_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let config: AppConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_config(config);
    Ok(())
}

fn apply_wallpapers_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let wallpapers: Vec<WallpaperEntry> =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_wallpapers(wallpapers);
    Ok(())
}

fn apply_displays_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let displays: Vec<DisplayInfo> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_displays(displays);
    Ok(())
}

fn apply_arrangement_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let arrangement: Arrangement = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_arrangement(arrangement);
    Ok(())
}

fn apply_rotation_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let rotation: RotationConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_rotation(rotation);
    Ok(())
}

fn apply_pools_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let pools: Vec<Pool> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_pools(pools);
    Ok(())
}

fn apply_units_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let units: Vec<UnitStateView> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_unit_states(units);
    Ok(())
}

fn apply_auto_start_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_auto_start(enabled);
    Ok(())
}

fn apply_playback_value(store: &mut AppStore, value: Value) -> Result<(), String> {
    let state: Option<WallpaperState> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    store.apply_playback(state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程隔离的临时目录（复用统一命名，测试结束清理）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mirrorstar_ui_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 启动快照仅重试一次：有剩余次数时递减，耗尽后不再重试。
    #[test]
    fn next_retries_allows_single_startup_retry() {
        assert_eq!(next_retries(1), Some(0));
        assert_eq!(next_retries(0), None);
    }

    #[test]
    fn decode_cards_falls_back_to_source_when_thumbnail_broken() {
        let dir = temp_dir("decode_fallback");
        let good = dir.join("good.png");
        image::DynamicImage::new_rgb8(40, 30)
            .save_with_format(&good, image::ImageFormat::Png)
            .unwrap();
        // 缩略图候选指向不存在（等效损坏/0 字节）路径，源图为有效 png。
        let broken = dir.join("broken_thumb.jpg");

        let plan = CardPlan {
            id: "a".to_string(),
            name: "good.png".to_string(),
            kind_index: 0,
            state: STATE_OK,
            file_path: good.to_string_lossy().into_owned(),
            thumb_paths: vec![
                broken.to_string_lossy().into_owned(),
                good.to_string_lossy().into_owned(),
            ],
            row: 0,
            col: 0,
            pos: 0,
        };

        let cards = decode_cards(vec![plan]);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].state, STATE_OK, "坏候选应回退到有效源图");
        assert!(cards[0].thumb.size().width > 0, "回退后应得到非空缩略图");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_cards_degrades_when_all_candidates_fail() {
        let plan = CardPlan {
            id: "b".to_string(),
            name: "b.png".to_string(),
            kind_index: 0,
            state: STATE_OK,
            file_path: "C:/definitely/not/here.png".to_string(),
            thumb_paths: vec![
                "C:/definitely/not/here-thumb.png".to_string(),
                "C:/definitely/not/here-src.png".to_string(),
            ],
            row: 0,
            col: 0,
            pos: 0,
        };

        let cards = decode_cards(vec![plan]);
        assert_eq!(
            cards[0].state, STATE_TYPE_ICON,
            "全部候选失败应降级类型图标"
        );
        assert_eq!(cards[0].thumb.size().width, 0, "降级时缩略图应为默认空图");
    }
}
