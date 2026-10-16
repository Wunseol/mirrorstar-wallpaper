//! 预览弹窗（Task 8）：四类壁纸的原生预览。
//!
//! 由 Slint 侧 `request-preview(id)` 触发，按壁纸类型分发：
//!
//! | 类型 | 载体 | 关键点 |
//! |------|------|--------|
//! | 图片 | Slint `Image` | 限幅解码 + `image-fit: contain`；关闭显式清空 |
//! | 视频 | 独立 mpv `--wid` 子窗口 | Job Object 兜底、禁自缩放、不抢焦点 |
//! | GIF  | 自研逐帧（Slint 定时器） | 惰性解码，常驻仅当前帧；关闭释放 |
//! | 网页 | 按需临时 WebView2 | 独立 UDF；关闭 `Close()` + drop 接口 |
//!
//! ## 线程与所有权
//!
//! 控制器（HWND / COM 接口 / Slint 定时器驱动状态）**只在 UI 线程**使用，因此放在
//! `thread_local!` 中，不引入 `Arc`/`Send` 约束。异步的网页创建由 Slint 侧
//! `preview-pending` 定时器驱动轮询（见 `web` 模块），避免在完成回调里触碰 Slint。
//!
//! ## 几何协同（Task 8.6）
//!
//! 预览内容区的逻辑矩形由 `main.slint` 的 `preview-x/y/w/h` 暴露，Rust 读取后
//! `× scale_factor` 得到物理矩形，用于把原生子窗口精确叠加。窗口尺寸变化（含跨屏 DPI
//! 变化引发的重排）经 `preview-geometry-changed` 回调重排子窗口；`Hide`/`Exit` 控制指令
//! 会先关闭预览，杜绝后台残留。

mod gif;
mod host_child;
mod photo;
mod video;
mod web;

use std::cell::RefCell;
use std::time::Instant;

use slint::{ComponentHandle, Image, SharedString};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, VK_LBUTTON,
};

use mirrorstar_core::config::WallpaperType;

use crate::protocol::log;
use crate::MainWindow;

/// 预览类型（与 `main.slint` 的 `preview-kind` 一一对应）。
const KIND_IMAGE: i32 = 0;
const KIND_VIDEO: i32 = 1;
const KIND_GIF: i32 = 2;
const KIND_WEB: i32 = 3;

fn kind_of(wallpaper_type: WallpaperType) -> i32 {
    match wallpaper_type {
        WallpaperType::Image => KIND_IMAGE,
        WallpaperType::Video => KIND_VIDEO,
        WallpaperType::Gif => KIND_GIF,
        WallpaperType::Web => KIND_WEB,
    }
}

/// 取路径末段作为预览标题（兼容 `/` 与 `\`）。
fn display_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// 预览期间的本地运行态（仅 UI 线程）。
#[derive(Default)]
struct PreviewController {
    /// 当前预览类型（关闭后为 -1）。
    kind: i32,
    mpv: Option<video::MpvPreview>,
    web: Option<web::WebPreview>,
    web_pending: Option<web::WebPending>,
    gif: Option<gif::GifPreview>,
    /// 最近一次已应用的物理矩形；用于让周期性兜底重排幂等（同矩形不重复 `SetWindowPos`）。
    last_rect: Option<host_child::PhysRect>,
}

thread_local! {
    /// UI 线程独占的预览控制器。
    static CONTROLLER: RefCell<PreviewController> = RefCell::new(PreviewController::default());
}

/// 全屏 chrome 自动隐藏的光标 / 点击轮询状态（仅 UI 线程）。
#[derive(Default)]
struct ChromeTracking {
    /// 上次轮询到的光标屏幕坐标。
    last_cursor: Option<(i32, i32)>,
    /// 最近一次「活动」（光标移动或画面点击）时刻，用于静止判定。
    last_move_at: Option<Instant>,
    /// 上一次单击时刻，用于双击判定（超过系统双击阈值即视为新的单击）。
    last_click_at: Option<Instant>,
}

thread_local! {
    static CHROME_TRACKING: RefCell<ChromeTracking> = RefCell::new(ChromeTracking::default());
}

thread_local! {
    /// 预览音量/静音会话语义：`None` 表示尚未从配置种子化，取默认 (50.0, 未静音)。
    static SESSION_AUDIO: std::cell::Cell<Option<(f32, bool)>> = const { std::cell::Cell::new(None) };
}

/// 光标静止多久后隐藏 chrome。
const CHROME_IDLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// 复位 chrome 显示状态与轮询计时（进入/退出全屏、打开/关闭预览时调用）。
pub fn reset_chrome_tracking(ui: &MainWindow) {
    ui.set_preview_chrome_visible(true);
    CHROME_TRACKING.with(|t| {
        let mut t = t.borrow_mut();
        t.last_cursor = None;
        t.last_move_at = Some(Instant::now());
        t.last_click_at = None;
    });
}

/// 读写 `preview-chrome-visible` 并在变化后**立即**重排原生子窗口。
///
/// chrome 显隐不会触发 `changed dlg-*`；若不在此处主动重排，就只能等 250ms 兜底 Timer，
/// 期间 mpv 原生子窗口仍按旧（铺满）几何绘制，会盖住刚显示出来的控制条。
fn set_chrome_visible(ui: &MainWindow, visible: bool) {
    if ui.get_preview_chrome_visible() == visible {
        return;
    }
    if !visible {
        // chrome 自动隐藏时同步收起根级下拉与底部带速度档位条：避免再次显示时残留展开态。
        // 值未变化时不产生副作用。
        ui.set_dd_open(false);
        ui.set_preview_speed_open(false);
    }
    ui.set_preview_chrome_visible(visible);
    CONTROLLER.with(|c| {
        let mut c = c.borrow_mut();
        c.last_rect = None;
        c.sync_geometry_inner(ui);
    });
}

/// 系统双击判定阈值（毫秒）。
fn double_click_ms() -> u128 {
    unsafe { GetDoubleClickTime() as u128 }
}

/// 判断屏幕坐标 `cursor` 是否落在「视频内容区」物理矩形内。
///
/// 内容区矩形由 `preview-x/y/w/h` 派生（唯一坐标来源），加上客户区原点的屏幕坐标即得屏幕矩形。
/// 句柄或换算不可用时返回 `true`（退化为仅按静止判定隐藏，不影响功能可用性）。
fn cursor_in_content(ui: &MainWindow, cursor: (i32, i32)) -> bool {
    let Some(hwnd) = crate::drop_target::hwnd_of(ui.window()) else {
        return true;
    };
    let mut origin = windows::Win32::Foundation::POINT::default();
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        return true;
    }
    // 只做坐标换算，**不**调用 `physical_rect`：后者在底边越界时会写诊断日志，而本函数每 100ms
    // 调用一次，越界（非 100% DPI 取整）会造成日志刷屏。坐标来源仍是 `preview-x/y/w/h`（唯一来源）。
    let (cx, cy, cw, ch) = host_child::to_physical(
        ui.get_preview_x(),
        ui.get_preview_y(),
        ui.get_preview_w(),
        ui.get_preview_h(),
        ui.window().scale_factor(),
    );
    let left = origin.x + cx;
    let top = origin.y + cy;
    cursor.0 >= left && cursor.0 < left + cw && cursor.1 >= top && cursor.1 < top + ch
}

/// 全屏 chrome 交互轮询（100ms）：光标移动 / 单击画面唤起控制条，双击画面切换播放/暂停，
/// 仅当「光标在内容区内 + 左键未按下 + 静止超阈值」时隐藏。
fn chrome_tick(ui: &MainWindow) {
    if ui.get_preview_id().is_empty() || !ui.get_preview_fullscreen() {
        return;
    }
    let mut point = windows::Win32::Foundation::POINT::default();
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) }.is_err() {
        return;
    }
    let pos = (point.x, point.y);
    let now = Instant::now();
    // 左键状态：低位 = 自上次轮询以来被按下（可捕获短促单击）；高位 = 当前按住。
    let keys = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16;
    let just_pressed = (keys & 0x0001) != 0;
    let held = (keys & 0x8000) != 0;
    let inside = cursor_in_content(ui, pos);

    let mut show = false;
    let mut hide = false;
    let mut double_click = false;
    CHROME_TRACKING.with(|t| {
        let mut t = t.borrow_mut();
        // 光标移动 → 立即显示并刷新活动时刻。
        if t.last_cursor != Some(pos) {
            t.last_cursor = Some(pos);
            t.last_move_at = Some(now);
            if !ui.get_preview_chrome_visible() {
                show = true;
            }
        }
        // 画面点击 → 绝不隐藏：单击显示并重置计时，双击切换播放/暂停。
        if just_pressed && inside {
            t.last_move_at = Some(now);
            if !ui.get_preview_chrome_visible() {
                show = true;
            }
            let is_double = t
                .last_click_at
                .map(|t0| now.duration_since(t0).as_millis() <= double_click_ms())
                .unwrap_or(false);
            if is_double {
                t.last_click_at = None;
                double_click = true;
            } else {
                t.last_click_at = Some(now);
            }
        }
        // 隐藏：仅在「本轮未显示 + 当前可见 + 光标在内容区内 + 左键未按下 + 静止超阈值」时。
        if !show && ui.get_preview_chrome_visible() {
            let idle = t
                .last_move_at
                .map(|t0| now.duration_since(t0) >= CHROME_IDLE)
                .unwrap_or(true);
            if idle && inside && !held {
                hide = true;
            }
        }
    });

    if double_click {
        toggle_play(ui);
    }
    if show {
        set_chrome_visible(ui, true);
    } else if hide {
        set_chrome_visible(ui, false);
    }
}

/// 注册预览自身的 Slint 回调（`request-preview` 由 `main.rs` 注册，携带壁纸库查询）。
pub fn install_handlers(ui: &MainWindow) {
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_closed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                close(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_poll_tick(move || {
            if let Some(ui) = ui_weak.upgrade() {
                poll_tick(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_gif_tick(move || {
            if let Some(ui) = ui_weak.upgrade() {
                gif_tick(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_geometry_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                sync_geometry(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_toggle_play(move || {
            if let Some(ui) = ui_weak.upgrade() {
                toggle_play(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_seek(move |fraction| {
            if let Some(ui) = ui_weak.upgrade() {
                seek(&ui, fraction);
            }
        });
    }
    // ±5s 相对跳转（Task 1/2）：参数为秒增量，复用 `seek_relative`（时长元数据未就绪时内部 no-op）。
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_seek_relative(move |delta| {
            if let Some(ui) = ui_weak.upgrade() {
                seek_relative(&ui, delta as f64);
            }
        });
    }
    // 播放倍速（Task 2）：仅视频生效，经 IPC 工作线程写 mpv `speed`。
    ui.on_preview_set_speed(move |speed| {
        CONTROLLER.with(|c| {
            let c = c.borrow();
            if c.kind != KIND_VIDEO {
                return;
            }
            if let Some(mpv) = c.mpv.as_ref() {
                mpv.set_speed(speed as f64);
            }
        });
    });
    // 全屏 / 退出全屏（Task 3/8）：进入真·系统全屏，复位 chrome 后强制清缓存重排，
    // 让原生子窗口随新几何同步。
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_toggle_fullscreen(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let fullscreen = !ui.get_preview_fullscreen();
                ui.set_preview_fullscreen(fullscreen);
                // 系统级全屏（覆盖整屏含任务栏）。进入/退出全屏一律复位 chrome 显示并重置光标计时。
                ui.window().set_fullscreen(fullscreen);
                reset_chrome_tracking(&ui);
                CONTROLLER.with(|c| {
                    let mut c = c.borrow_mut();
                    // 清 last_rect 以绕过 sync_geometry_inner 的幂等短路：
                    // 否则"同矩形"会被拦截，原生子窗口不会随全屏重排。
                    c.last_rect = None;
                    c.sync_geometry_inner(&ui);
                });
            }
        });
    }
    // 全屏 chrome 自动隐藏轮询（Task 9）：由 Slint 定时器驱动。
    {
        let ui_weak = ui.as_weak();
        ui.on_preview_chrome_tick(move || {
            if let Some(ui) = ui_weak.upgrade() {
                chrome_tick(&ui);
            }
        });
    }
}

/// 打开指定壁纸的预览（先关闭既有预览）。
pub fn open(ui: &MainWindow, id: &str, file_path: &str, wallpaper_type: WallpaperType) {
    // 仅视频 / 网页需要父窗口 HWND（嵌入原生子窗口）；图片 / GIF 完全由 Slint 侧渲染，
    // 句柄惰性不可用时它们仍应正常预览，故此处不再提前拒绝。
    let parent = crate::drop_target::hwnd_of(ui.window());
    if let Some(parent_hwnd) = parent {
        // 原生子窗口要求父窗口带 WS_CLIPCHILDREN，否则会被 Slint 软件渲染覆盖。
        host_child::ensure_clip_children(parent_hwnd);
    }
    CONTROLLER.with(|c| {
        c.borrow_mut()
            .open_inner(ui, id, file_path, wallpaper_type, parent)
    });
}

/// 关闭预览并释放全部原生资源。
pub fn close(ui: &MainWindow) {
    CONTROLLER.with(|c| c.borrow_mut().close_inner(ui));
}

/// 网页创建轮询（由 Slint `preview-pending` 定时器驱动）。
pub fn poll_tick(ui: &MainWindow) {
    CONTROLLER.with(|c| c.borrow_mut().poll_tick_inner(ui));
}

/// GIF 逐帧推进（由 Slint `preview-gif-playing` 定时器驱动）。
pub fn gif_tick(ui: &MainWindow) {
    CONTROLLER.with(|c| c.borrow_mut().gif_tick_inner(ui));
}

/// 预览区尺寸变化时重排原生子窗口。
pub fn sync_geometry(ui: &MainWindow) {
    CONTROLLER.with(|c| c.borrow_mut().sync_geometry_inner(ui));
}

/// 切换预览播放/暂停：视频经 IPC 通知 mpv；GIF 直接翻转定时器开关。
pub fn toggle_play(ui: &MainWindow) {
    CONTROLLER.with(|c| {
        let c = c.borrow();
        match c.kind {
            KIND_VIDEO => {
                if let Some(mpv) = c.mpv.as_ref() {
                    mpv.toggle_play();
                }
            }
            KIND_GIF => {
                // 翻转后由 GIF 定时器继续推进；若 gif_tick 解码出错会自行置回 false。
                ui.set_preview_gif_playing(!ui.get_preview_gif_playing());
            }
            _ => {}
        }
    });
}

/// 预览进度跳转：`fraction` 为 0..1 比例，仅视频生效（经 IPC 通知 mpv）。
pub fn seek(ui: &MainWindow, fraction: f32) {
    CONTROLLER.with(|c| {
        let c = c.borrow();
        if c.kind != KIND_VIDEO {
            return;
        }
        let dur = ui.get_preview_dur() as f64;
        let secs = (fraction as f64).clamp(0.0, 1.0) * dur;
        if let Some(mpv) = c.mpv.as_ref() {
            mpv.seek(secs);
        }
    });
}

/// 从配置种子化预览音量/静音会话态（`volume_0_100` 为 mpv 标度）。
///
/// **幂等**：仅当会话态尚未初始化（`None`）时写入，绝不覆盖会话内用户改动。
/// 返回 `true` 表示本次真正写入；`false` 表示已有会话态、未写入。
pub fn seed_session_audio(volume_0_100: f32, muted: bool) -> bool {
    SESSION_AUDIO.with(|s| {
        if s.get().is_none() {
            s.set(Some((volume_0_100.clamp(0.0, 100.0), muted)));
            true
        } else {
            false
        }
    })
}

/// 把会话音量/静音同步到 Slint 属性与 mpv（供「配置晚到时补齐」使用）。
///
/// 音量用 mpv 标度（0..100）；仅当当前为视频预览且 mpv 存在时才下发 mpv。
pub fn apply_session_audio(ui: &MainWindow) {
    let (v, m) = session_audio();
    ui.set_preview_volume(v);
    ui.set_preview_muted(m);
    CONTROLLER.with(|c| {
        let c = c.borrow();
        if c.kind == KIND_VIDEO {
            if let Some(mpv) = c.mpv.as_ref() {
                mpv.set_volume(v);
                mpv.set_mute(m);
            }
        }
    });
}

/// 配置标度（0.0..1.0）→ mpv 标度（0..100）。
pub fn config_volume_to_mpv(v01: f32) -> f32 {
    v01.clamp(0.0, 1.0) * 100.0
}

/// mpv 标度（0..100）→ 配置标度（0.0..1.0）。
pub fn mpv_volume_to_config(v100: f32) -> f32 {
    v100.clamp(0.0, 100.0) / 100.0
}

/// 读取预览音量/静音会话态（单位 0..100）；未种子化时回退默认 `(50.0, false)`。
pub fn session_audio() -> (f32, bool) {
    SESSION_AUDIO.with(|s| s.get().unwrap_or((50.0, false)))
}

/// 设置预览音量（`volume_0_100` 为 mpv 标度，仅视频生效）。
pub fn set_volume(ui: &MainWindow, volume_0_100: f32) {
    // 非有限值直接忽略：不入会话态、不下发 mpv（避免 NaN 污染状态与 IPC）。
    if !volume_0_100.is_finite() {
        return;
    }
    if CONTROLLER.with(|c| c.borrow().kind != KIND_VIDEO) {
        return;
    }
    let v = volume_0_100.clamp(0.0, 100.0);
    let (_, muted) = session_audio();
    SESSION_AUDIO.with(|s| s.set(Some((v, muted))));
    CONTROLLER.with(|c| {
        if let Some(mpv) = c.borrow().mpv.as_ref() {
            mpv.set_volume(v);
        }
    });
    ui.set_preview_volume(v);
}

/// 切换预览静音（仅视频生效）。
pub fn toggle_mute(ui: &MainWindow) {
    if CONTROLLER.with(|c| c.borrow().kind != KIND_VIDEO) {
        return;
    }
    let (v, muted) = session_audio();
    let new_muted = !muted;
    SESSION_AUDIO.with(|s| s.set(Some((v, new_muted))));
    CONTROLLER.with(|c| {
        if let Some(mpv) = c.borrow().mpv.as_ref() {
            mpv.set_mute(new_muted);
        }
    });
    ui.set_preview_muted(new_muted);
}

/// 相对当前进度快进/快退 `delta_secs` 秒（仅视频生效）。
pub fn seek_relative(ui: &MainWindow, delta_secs: f64) {
    if CONTROLLER.with(|c| c.borrow().kind != KIND_VIDEO) {
        return;
    }
    let Some(target) = seek_target(ui.get_preview_progress(), ui.get_preview_dur(), delta_secs)
    else {
        return;
    };
    CONTROLLER.with(|c| {
        if let Some(mpv) = c.borrow().mpv.as_ref() {
            mpv.seek(target);
        }
    });
}

/// 计算相对跳转目标秒数；时长元数据未就绪（`dur <= 0` 或非有限）时返回 `None`（no-op）。
fn seek_target(progress: f32, dur: f32, delta_secs: f64) -> Option<f64> {
    if !dur.is_finite() || dur <= 0.0 {
        return None;
    }
    let target = (progress.clamp(0.0, 1.0) as f64 * dur as f64 + delta_secs).clamp(0.0, dur as f64);
    Some(target)
}

impl PreviewController {
    fn open_inner(
        &mut self,
        ui: &MainWindow,
        id: &str,
        file_path: &str,
        wallpaper_type: WallpaperType,
        parent: Option<HWND>,
    ) {
        self.teardown(ui);
        let kind = kind_of(wallpaper_type);
        self.kind = kind;

        ui.set_preview_kind(kind);
        ui.set_preview_title(SharedString::from(display_name(file_path)));
        ui.set_preview_image(Image::default());
        ui.set_preview_frame(Image::default());
        ui.set_preview_pending(false);
        ui.set_preview_gif_playing(false);
        // 播放控制状态复位，避免上一次预览的进度/时长残留到新预览。
        ui.set_preview_playing(false);
        ui.set_preview_dur(0.0);
        ui.set_preview_progress(0.0);
        ui.set_preview_pos_text(SharedString::from("0:00"));
        ui.set_preview_dur_text(SharedString::from("0:00"));
        // 播放倍速 / 全屏复位：打开新预览一律回到 1× 与默认尺寸（不继承上一次的全屏）。
        // 默认档位下标由 Slint 的 `preview-speed-default-index` 提供（唯一来源），避免 Rust 硬编码档位下标。
        ui.window().set_fullscreen(false);
        ui.set_preview_fullscreen(false);
        ui.set_preview_speed_index(ui.get_preview_speed_default_index());
        ui.set_preview_speed_open(false);
        // 预览音量/静音：无条件按会话态种子化（会话内用户改动优先，seed 为幂等）。
        let (session_v, session_m) = session_audio();
        ui.set_preview_volume(session_v);
        ui.set_preview_muted(session_m);
        // 最后写 id 以显示覆盖层；几何为纯绑定，可立即读取。
        ui.set_preview_id(SharedString::from(id));
        reset_chrome_tracking(ui);
        let rect = host_child::physical_rect(ui);
        // 取证日志（SubTask 1.1）：记录内容区 / 宿主子窗口物理矩形 / 控制条矩形的真实几何，
        // 用于判断控制条是否被原生子窗口覆盖或不可辨。
        log(format!(
            "预览几何：kind={kind} controls_h={} preview_w={} preview_h={} host_rect={:?} controls_rect=({},{},{},{})",
            ui.get_preview_controls_h(),
            ui.get_preview_w(),
            ui.get_preview_h(),
            rect,
            ui.get_preview_x(),
            ui.get_preview_y() + ui.get_preview_h(),
            ui.get_preview_w(),
            ui.get_preview_controls_h(),
        ));

        match kind {
            KIND_IMAGE => match photo::decode(file_path) {
                Ok(image) => ui.set_preview_image(image),
                Err(e) => {
                    log(format!("图片预览失败：{e}"));
                    toast(ui, "图片预览失败");
                }
            },
            KIND_VIDEO => {
                let Some(parent) = parent else {
                    log("预览：未获取到主窗口 HWND，无法嵌入视频子窗口");
                    toast(ui, "无法打开预览：未获取到窗口句柄");
                    return;
                };
                match video::open(parent, rect, file_path, ui.as_weak(), session_v, session_m) {
                    Ok(preview) => {
                        // 宿主仍为隐藏创建：不在此立即显示（此时 mpv 尚未输出首帧，显示的是空白块，
                        // 会盖住 Slint 占位文案）。改由 250ms 几何兜底 Timer / 几何变化的
                        // `sync_geometry` 经 `maybe_show` 在首帧就绪（或超时兜底）后再显示。
                        log(format!("视频预览已启动 pid={}", preview.pid()));
                        self.mpv = Some(preview);
                        self.last_rect = Some(rect);
                    }
                    Err(e) => {
                        log(format!("视频预览启动失败：{e}"));
                        toast(ui, "视频预览启动失败");
                    }
                }
            }
            KIND_GIF => match gif::GifPreview::open(file_path) {
                Ok((preview, image, delay)) => {
                    log(format!(
                        "GIF 预览已启动：{}（首帧延时 {delay}ms）",
                        preview.path().display()
                    ));
                    ui.set_preview_frame(image);
                    ui.set_preview_gif_playing(true);
                    self.gif = Some(preview);
                }
                Err(e) => {
                    log(format!("GIF 预览失败：{e}"));
                    toast(ui, "GIF 预览失败");
                }
            },
            KIND_WEB => {
                let Some(parent) = parent else {
                    log("预览：未获取到主窗口 HWND，无法嵌入网页子窗口");
                    toast(ui, "无法打开预览：未获取到窗口句柄");
                    return;
                };
                let target = web::resolve_target(file_path);
                match web::WebPending::new(parent, rect, target) {
                    Ok(pending) => {
                        ui.set_preview_pending(true);
                        self.web_pending = Some(pending);
                        self.last_rect = Some(rect);
                    }
                    Err(e) => {
                        log(format!("网页预览启动失败：{e}"));
                        toast(ui, "网页预览启动失败");
                    }
                }
            }
            _ => {}
        }
    }

    fn close_inner(&mut self, ui: &MainWindow) {
        let visible = !ui.get_preview_id().is_empty();
        // 关闭时也复位 chrome，保证下次打开为显示态。
        reset_chrome_tracking(ui);
        self.teardown(ui);
        // 全屏状态清理：先退系统全屏再复位属性，下次打开回到默认尺寸。
        ui.window().set_fullscreen(false);
        ui.set_preview_fullscreen(false);
        if visible {
            ui.set_preview_id(SharedString::default());
        }
        // 程序化关闭预览（热插拔 / 在别处应用壁纸 / 切换预览等）后强制复位根级下拉状态，杜绝孤儿下拉。
        ui.set_dd_open(false);
    }

    fn sync_geometry_inner(&mut self, ui: &MainWindow) {
        if ui.get_preview_id().is_empty() {
            return;
        }
        let rect = host_child::physical_rect(ui);
        // 首帧就绪门控：必须在同矩形幂等短路之前推进，否则几何不变时（250ms 几何兜底 Timer
        // 触发）会被短路拦截，导致 `maybe_show` 永不执行、宿主子窗口永不显示。
        if let Some(mpv) = self.mpv.as_mut() {
            mpv.maybe_show();
        }
        // 幂等：同一物理矩形不重复 `SetWindowPos`（周期性兜底重排会高频调用本函数，
        // 重复应用会让 mpv 反复重配窗口/画面）。
        if self.last_rect == Some(rect) {
            return;
        }
        self.last_rect = Some(rect);
        if let Some(mpv) = self.mpv.as_mut() {
            mpv.resize(rect);
        }
        if let Some(web) = self.web.as_mut() {
            web.resize(rect.2, rect.3);
            host_child::move_host(web.host(), rect);
            web.show();
        }
        // 网页仍在创建中时，宿主子窗口已存在，需同步移动并刷新待用边界，
        // 否则创建期间发生的重排会以旧几何落位。
        if let Some(pending) = self.web_pending.as_mut() {
            pending.resize(rect);
        }
    }

    fn poll_tick_inner(&mut self, ui: &MainWindow) {
        let result = match self.web_pending.as_mut() {
            Some(pending) => pending.poll(),
            None => {
                ui.set_preview_pending(false);
                return;
            }
        };
        match result {
            web::WebPoll::Pending => {}
            web::WebPoll::Ready(mut preview) => {
                log("网页预览已就绪");
                // 就绪后显式显示宿主（隐藏创建 → 就绪显示）；WebPending 已置 shown=true，
                // 此处为 no-op 兜底。
                preview.show();
                self.web = Some(preview);
                self.web_pending = None;
                ui.set_preview_pending(false);
                // 刚移交的 WebPreview 需强制落一次几何（其边界由 configure 以旧宽高设定），
                // 清缓存以绕过幂等短路。
                self.last_rect = None;
                self.sync_geometry_inner(ui);
            }
            web::WebPoll::Failed(e) => {
                log(format!("网页预览创建失败：{e}"));
                self.web_pending = None;
                ui.set_preview_pending(false);
                toast(ui, "网页预览创建失败");
            }
        }
    }

    fn gif_tick_inner(&mut self, ui: &MainWindow) {
        let Some(preview) = self.gif.as_mut() else {
            ui.set_preview_gif_playing(false);
            return;
        };
        let result = preview.tick(Instant::now());
        match result {
            Ok(Some(image)) => ui.set_preview_frame(image),
            Ok(None) => {}
            Err(e) => {
                log(format!("GIF 预览播放中止：{e}"));
                // 解码器已损坏：丢弃槽位并停止定时器，避免继续驱动坏掉的解码器。
                self.gif = None;
                ui.set_preview_gif_playing(false);
            }
        }
    }

    /// 释放全部原生资源并清空 UI 侧图像 / 定时器开关。
    fn teardown(&mut self, ui: &MainWindow) {
        // 先隐藏再销毁（Drop 内部 destroy_host）：关闭/切换预览时杜绝残留可见子窗口。
        if let Some(mpv) = self.mpv.as_ref() {
            host_child::hide_host(mpv.host());
        }
        if let Some(web) = self.web.as_ref() {
            host_child::hide_host(web.host());
        }
        // 视频预览的就绪态（`ready`/`opened_at`/`shown`）随 `self.mpv = None` 一并 Drop，
        // 无需额外重置；下次打开会创建全新的 `MpvPreview`（就绪态从头开始）。
        self.mpv = None;
        self.web_pending = None;
        self.web = None;
        self.gif = None;
        self.kind = -1;
        self.last_rect = None;
        ui.set_preview_pending(false);
        ui.set_preview_gif_playing(false);
        // 关闭预览时清空播放控制状态（覆盖层隐藏后不再显示残留进度）。
        ui.set_preview_playing(false);
        ui.set_preview_dur(0.0);
        ui.set_preview_progress(0.0);
        ui.set_preview_pos_text(SharedString::from("0:00"));
        ui.set_preview_dur_text(SharedString::from("0:00"));
        ui.set_preview_image(Image::default());
        ui.set_preview_frame(Image::default());
    }
}

impl Drop for PreviewController {
    fn drop(&mut self) {
        // 线程/进程退出兜底：显式释放原生资源，避免 mpv 与 WebView2 残留。
        self.mpv = None;
        self.web_pending = None;
        self.web = None;
        self.gif = None;
    }
}

/// 预览错误提示（Toast，error 级）。
fn toast(ui: &MainWindow, text: &str) {
    ui.set_toast_level(3);
    ui.set_toast_text(SharedString::from(text));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_target_none_when_duration_not_ready() {
        assert_eq!(seek_target(0.5, 0.0, 5.0), None);
        assert_eq!(seek_target(0.5, -1.0, 5.0), None);
        assert_eq!(seek_target(0.5, f32::NAN, 5.0), None);
        assert_eq!(seek_target(0.5, f32::INFINITY, 5.0), None);
    }

    #[test]
    fn seek_target_clamps_into_range() {
        // 前进超出末尾 → 夹到总时长。
        assert_eq!(seek_target(0.5, 100.0, 999.0), Some(100.0));
        // 后退到负 → 夹到 0。
        assert_eq!(seek_target(0.5, 100.0, -999.0), Some(0.0));
        // 正常前进：50 + 10 = 60。
        assert_eq!(seek_target(0.5, 100.0, 10.0), Some(60.0));
        // progress 越界先按 [0,1] 夹取。
        assert_eq!(seek_target(2.0, 100.0, 0.0), Some(100.0));
        assert_eq!(seek_target(-1.0, 100.0, 0.0), Some(0.0));
    }
}
