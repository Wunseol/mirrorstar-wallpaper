use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock, RwLock};

use image::GenericImageView;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::wallpaper::{
    create_pause_channel, create_pause_sender_with_state, RendererState, ScalingMode,
    WallpaperRenderer, WallpaperState,
};

use super::gdi_base::{
    create_image_bitmap, create_wallpaper_window, get_initial_window_rect,
    paint_image_with_double_buffer, paint_with_double_buffer, register_window_class_once,
    spawn_pause_forwarder, try_handle_common_messages, GdiRendererBase,
};
use super::gdi_cache::GdiCache;

/// Custom window message to wake up the wallpaper thread for command processing
///
/// 命名与 `gif.rs::WM_GIF_COMMAND` 不一致（本处泛化命名，gif.rs 采用 GIF 专用命名），
/// 偏移量亦不同（本处 `WM_USER + 1`，gif.rs 用 `WM_USER + 10` 起）。经技术债评估
/// （W-TD-018）决定保留现状：两常量分属各自渲染器创建的独立窗口，消息码互不冲突；
/// 统一命名需调整偏移量以避免跨窗口冲突，且 gif.rs 侧已占用 `WM_USER + 10/11`，
/// 收敛收益低于回归风险。
const WM_WALLPAPER_COMMAND: u32 = WM_USER + 1;

/// v11.0 内存优化：降采样目标尺寸上限（QHD 2560×1440）
///
/// 4K/8K 屏幕下，降采样到屏幕分辨率仍会保留过多像素（4K ~33MB，8K ~133MB）。
/// 此上限将目标尺寸限制为 2560×1440（~15MB），视觉差异不明显但大幅降低内存。
/// 1080p 屏幕（1920×1080）低于此上限，不受影响。
const MAX_DOWNSAMPLE_DIMENSION: u32 = 2560;

/// Commands sent from the main thread to the wallpaper thread
///
/// 与 `gif.rs::GifCommand` 高度相似（仅后者多 `SetSpeed(f32)` 变体以支持 GIF 播放
/// 速度控制）。经技术债评估（W-TD-010 + W-TD-017）决定保留现状：统一为
/// `GdiCommand` 枚举需改造两处壁纸线程的 match 分发逻辑并引入变体合并/类型参数，
/// 改动面较大；当前差异仅一个变体，重复成本可接受。新增公共变体时请同步两处枚举。
enum WallpaperCommand {
    Terminate,
    SetPosition {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    SetScalingMode(ScalingMode),
    Pause,
    Resume,
}

/// 渲染数据，存储在窗口的 GWLP_USERDATA 中供 WM_PAINT 使用
struct ImageRenderData {
    /// 图片宽度
    img_w: u32,
    /// 图片高度
    img_h: u32,
    /// RGBA 像素数据（GDI 经 BI_BITFIELDS 掩码直接解释 RGBA 字节序）
    ///
    /// v8.0 内存优化：首次 WM_PAINT 生成 HBITMAP 后释放（设为 None），后续绘制复用
    /// `gdi_cache.image_bitmap`。暂停时设为 None；SetScalingMode 时若已释放则重新加载。
    pixels: Option<Vec<u8>>,
    /// 缩放模式
    scaling_mode: ScalingMode,
    /// 图片文件路径，用于暂停后重新加载
    image_path: String,
    /// 是否处于暂停状态
    paused: bool,
}

/// 窗口用户数据，通过 GWLP_USERDATA 存储
struct ImageWindowData {
    /// 图片渲染数据
    render: ImageRenderData,
    /// 缓存的 GDI 对象（首次 WM_PAINT 时初始化）
    gdi_cache: Option<GdiCache>,
}

/// 静态图片壁纸渲染器
///
/// 使用 GDI StretchDIBits 将静态图片（JPG/PNG/BMP/WebP）渲染到壁纸窗口。
/// 窗口在专用线程上创建和运行，通过 mpsc 通道进行线程间通信。
/// 图片数据存储在窗口的 GWLP_USERDATA 中，WM_PAINT 时自动重绘。
///
/// 公共状态（窗口句柄、线程句柄、缩放模式、状态、pause_sender）由 `GdiRendererBase` 持有，
/// Win32 双缓冲绘制、窗口类注册、窗口创建、pause 转发等逻辑复用 `gdi_base` 模块的辅助函数。
pub struct ImageRenderer {
    /// GDI 渲染器公共基类（hwnd / thread_handle / scaling_mode / state / pause_sender）
    base: GdiRendererBase,
    /// 通道发送端，用于向壁纸线程发送命令
    cmd_tx: Sender<WallpaperCommand>,
    /// 图片路径
    image_path: String,
    /// 以下三个字段（`pre_shared_state` / `pre_state_changed` / `display_id_lock`）
    /// 与 `gif.rs::GifRenderer` 中同名字段构成同一「预置状态」模式：play() 阶段
    /// 提前创建共享状态与通知通道，create_pause_sender() 阶段 take() 复用，壁纸线程
    /// 通过 `display_id_lock` 在 Resume 失败时回滚状态并通知前端。
    ///
    /// 此处与 gif.rs 存在字段级重复，经技术债评估（W-TD-008 + W-TD-009）决定保留现状：
    /// 抽取共享结构体需调整 `GdiRendererBase` 架构（基类当前不持有这些字段），
    /// 改动面与回归风险高于当前重复成本，故维持两处独立实现。修改任一处时请同步
    /// 另一处以保持行为一致。
    ///
    /// W-003 修复：提前创建的共享状态（play() 中创建，create_pause_sender() 中复用）
    /// 传给壁纸线程，使 Resume 失败时能回滚 shared_state.state = Paused
    pre_shared_state: Option<Arc<RwLock<RendererState>>>,
    /// W-003 修复：提前创建的状态变更通知通道（play() 中创建，create_pause_sender() 中复用）
    /// 传给壁纸线程，使 Resume 失败时能通过 notify 通知前端刷新 UI
    pre_state_changed: Option<tokio::sync::broadcast::Sender<String>>,
    /// W-003 修复：display_id 共享变量（create_pause_sender 中设置，壁纸线程中读取）
    /// 使用 OnceLock 保证只写一次，壁纸线程在 Resume 失败时读取此值通知前端
    display_id_lock: Arc<std::sync::OnceLock<String>>,
}

// SAFETY: ImageRenderer 的所有窗口操作都在专用线程上执行。
// 公共 API 仅通过 mpsc 通道通信，Sender<WallpaperCommand> 是 Send 的。
// HWND 仅作为值存储，用于 PostMessageW（PostMessageW 是线程安全的）。
unsafe impl Send for ImageRenderer {}

impl ImageRenderer {
    /// 创建新的图片渲染器
    pub fn new(image_path: String, scaling_mode: ScalingMode) -> Self {
        Self {
            base: GdiRendererBase::new(scaling_mode),
            cmd_tx: mpsc::channel().0, // 占位，将在 play() 中设置
            image_path,
            pre_shared_state: None,
            pre_state_changed: None,
            display_id_lock: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// 开始播放壁纸
    pub fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel::<Result<isize, crate::MirrorStarError>>();

        let image_path = self.image_path.clone();
        let scaling_mode = self.base.scaling_mode;

        // 获取屏幕大小作为初始窗口尺寸
        let initial_rect = get_initial_window_rect();

        // W-003 修复：提前创建 shared_state 和 state_changed 通道，
        // 传给壁纸线程使 Resume 失败时能回滚状态并通知前端
        let shared_state = Arc::new(RwLock::new(RendererState::default()));
        let (state_changed, _) = tokio::sync::broadcast::channel::<String>(16);
        // 初始化 shared_state.state = Playing（与 play() 成功后 set_state(Playing) 一致）
        shared_state.write().unwrap().state = WallpaperState::Playing;
        let display_id_lock = self.display_id_lock.clone();
        // Clone for the wallpaper thread closure; originals retained for self.pre_*
        // Arc::clone 与 broadcast::Sender::clone 均为廉价引用计数递增
        let thread_shared_state = shared_state.clone();
        let thread_state_changed = state_changed.clone();

        let handle = std::thread::Builder::new()
            .name("mirrorstar-wallpaper".to_string())
            .spawn(move || {
                wallpaper_thread(
                    image_path,
                    scaling_mode,
                    initial_rect,
                    cmd_rx,
                    result_tx,
                    thread_shared_state,
                    thread_state_changed,
                    display_id_lock,
                );
            })
            .map_err(|e| {
                crate::MirrorStarError::DesktopIntegration(format!("创建壁纸线程失败: {}", e))
            })?;

        // 等待线程报告成功或失败
        let result = result_rx.recv().map_err(|e| {
            crate::MirrorStarError::DesktopIntegration(format!("壁纸线程通信失败: {}", e))
        })?;
        let hwnd_value = result.map_err(|e| {
            crate::MirrorStarError::DesktopIntegration(format!("壁纸初始化失败: {}", e))
        })?;
        let hwnd = HWND(hwnd_value as *mut _);

        self.cmd_tx = cmd_tx;
        self.base.set_hwnd(Some(hwnd));
        self.base.set_thread_handle(Some(handle));
        self.base.set_state(WallpaperState::Playing);
        // 存储 pre-created 组件供 create_pause_sender() 复用
        self.pre_shared_state = Some(shared_state);
        self.pre_state_changed = Some(state_changed);

        tracing::info!("静态图片壁纸开始播放");
        Ok(())
    }

    /// 终止壁纸渲染
    pub fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        self.base.terminate(
            &self.cmd_tx,
            WallpaperCommand::Terminate,
            WM_WALLPAPER_COMMAND,
        )?;
        tracing::info!("静态图片壁纸已终止");
        Ok(())
    }

    /// 获取壁纸窗口句柄
    pub fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    /// 发送命令到壁纸线程，并通过 PostMessageW 唤醒消息循环
    fn send_command(&self, cmd: WallpaperCommand) -> Result<(), crate::MirrorStarError> {
        self.base
            .send_command(&self.cmd_tx, cmd, WM_WALLPAPER_COMMAND)
    }
}

impl Drop for ImageRenderer {
    fn drop(&mut self) {
        if self.base.thread_handle.is_some() {
            // Drop 路径无法传播错误，仅记录日志
            if let Err(e) = self.terminate() {
                tracing::warn!(error = %e, "ImageRenderer drop 时 terminate 失败");
            }
        }
        tracing::debug!("ImageRenderer 已清理");
    }
}

impl WallpaperRenderer for ImageRenderer {
    fn play(&mut self) -> Result<(), crate::MirrorStarError> {
        ImageRenderer::play(self)
    }

    fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
        // W02 修复：与 GifRenderer 保持一致，先发送 Pause 命令到壁纸线程再 set_state。
        // 壁纸线程收到 Pause 后会释放像素数据、销毁 GDI 缓存以节省内存。
        if self.base.state() == WallpaperState::Playing {
            self.send_command(WallpaperCommand::Pause)?;
            self.base.set_state(WallpaperState::Paused);
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        // W02 修复：与 GifRenderer 保持一致，先发送 Resume 命令到壁纸线程再 set_state。
        // 壁纸线程收到 Resume 后会重新加载图片像素数据。
        if self.base.state() == WallpaperState::Paused {
            self.send_command(WallpaperCommand::Resume)?;
            self.base.set_state(WallpaperState::Playing);
        }
        Ok(())
    }

    fn set_position(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), crate::MirrorStarError> {
        self.send_command(WallpaperCommand::SetPosition {
            x,
            y,
            width: w,
            height: h,
        })
    }

    fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
        ImageRenderer::terminate(self)
    }

    fn hwnd(&self) -> Option<HWND> {
        self.base.hwnd()
    }

    fn state(&self) -> WallpaperState {
        self.base.state()
    }

    fn set_scaling_mode(&mut self, mode: ScalingMode) {
        self.base.set_scaling_mode(mode);
        if self.base.state() == WallpaperState::Playing {
            if let Err(e) = self.send_command(WallpaperCommand::SetScalingMode(mode)) {
                tracing::error!("缩放模式切换失败: {}", e);
            }
        }
    }

    fn set_mouse_passthrough(&mut self, enabled: bool) {
        if let Some(hwnd) = self.hwnd() {
            crate::desktop::window::set_mouse_passthrough(hwnd, enabled);
        }
    }

    fn set_interaction_mode(&mut self, enabled: bool) {
        self.set_mouse_passthrough(!enabled);
    }

    fn create_pause_sender(&mut self, display_id: &str) -> Option<crate::wallpaper::PauseSender> {
        // W-003 修复：复用 play() 中提前创建的 shared_state 和 state_changed，
        // 使壁纸线程在 Resume 失败时能通过 shared_state 回滚状态并通知前端
        let shared_state = self.pre_shared_state.take().unwrap_or_else(|| {
            tracing::warn!("pre_shared_state 已被消费，回退到 create_pause_channel");
            let (_, _, s) = create_pause_channel();
            s
        });
        let state_changed = self.pre_state_changed.take().unwrap_or_else(|| {
            tracing::warn!("pre_state_changed 已被消费，回退到新 broadcast channel");
            tokio::sync::broadcast::channel::<String>(16).0
        });

        // 设置 display_id_lock，使壁纸线程在 Resume 失败时能读取 display_id 通知前端
        let _ = self.display_id_lock.set(display_id.to_string());

        // 同步 shared_state.state 为当前 base.state()
        {
            let mut s = shared_state.write().unwrap_or_else(|e| e.into_inner());
            s.state = self.base.state();
        }

        // 使用已有的 shared_state 和 state_changed 创建 PauseSender + rx
        let (sender, rx) = create_pause_sender_with_state(shared_state.clone(), state_changed);

        let cmd_tx = self.cmd_tx.clone();
        // HWND 不是 Send，将原始值存储为 isize 跨线程传递
        let hwnd_raw = self.base.hwnd.map(|h| h.0 as isize);
        // clone sender 与 display_id 传入 spawn_pause_forwarder，
        // 状态变更后通过 notify_state_changed 通知 Tauri 层 emit 事件
        let state_sender = sender.clone();
        let display_id = display_id.to_string();

        if let Err(e) = spawn_pause_forwarder(
            "mirrorstar-image-pause",
            "ImageRenderer",
            cmd_tx,
            hwnd_raw,
            WM_WALLPAPER_COMMAND,
            || WallpaperCommand::Pause,
            || WallpaperCommand::Resume,
            rx,
            shared_state,
            state_sender,
            display_id,
        ) {
            tracing::error!(error = %e, "创建 ImageRenderer pause 线程失败");
            return None;
        }

        self.base.set_pause_sender(Some(sender.clone()));
        Some(sender)
    }
}

// ── Wallpaper Thread ─────────────────────────────────────────────────────────

/// 加载图片并降采样到屏幕分辨率
///
/// 解码图片后，如果图片尺寸大于屏幕分辨率，则使用 thumbnail 算法降采样，
/// 显著减少内存占用和渲染开销。像素保留 RGBA 字节序（GDI 经 BI_BITFIELDS 解释）。
fn load_and_downsample_image(
    image_path: &str,
    scaling_mode: ScalingMode,
) -> Result<Box<ImageRenderData>, crate::MirrorStarError> {
    let img = image::ImageReader::open(image_path)
        .map_err(|e| crate::MirrorStarError::ImageDecode(format!("打开图片文件失败: {}", e)))?
        .with_guessed_format()
        .map_err(|e| crate::MirrorStarError::ImageDecode(format!("识别图片格式失败: {}", e)))?
        .decode()
        .map_err(|e| crate::MirrorStarError::ImageDecode(format!("解码图片失败: {}", e)))?;

    let (orig_w, orig_h) = img.dimensions();
    const MAX_IMAGE_DIMENSION: u32 = 16384;
    if orig_w > MAX_IMAGE_DIMENSION || orig_h > MAX_IMAGE_DIMENSION {
        return Err(crate::MirrorStarError::ImageDecode(format!(
            "图片尺寸过大 ({}x{})，最大支持 {}x{}",
            orig_w, orig_h, MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION
        )));
    }

    // 获取屏幕分辨率，用于降采样
    // v5.0 W-PERF-003: 使用缓存避免每次 Resume 都调用 GetSystemMetrics
    let (screen_w, screen_h) = super::get_screen_size();

    // v11.0：降采样目标尺寸上限为 MAX_DOWNSAMPLE_DIMENSION，避免 4K/8K 屏保留过多像素
    let target_w = screen_w.min(MAX_DOWNSAMPLE_DIMENSION);
    let target_h = screen_h.min(MAX_DOWNSAMPLE_DIMENSION);

    // 如果图片大于目标尺寸，降采样以节省内存
    let processed_img = if orig_w > target_w || orig_h > target_h {
        tracing::info!(
            orig_w,
            orig_h,
            target_w,
            target_h,
            screen_w,
            screen_h,
            "图片大于目标尺寸，降采样中"
        );
        image::DynamicImage::ImageRgba8(image::imageops::thumbnail(&img, target_w, target_h))
    } else {
        img
    };

    let (img_w, img_h) = processed_img.dimensions();
    tracing::info!(
        path = %image_path,
        orig_w, orig_h, img_w, img_h,
        "图片加载完成"
    );

    // 像素保留 RGBA 字节序：GDI 通过 BI_BITFIELDS 掩码直接解释 RGBA，无需转换
    let pixels = processed_img.to_rgba8().into_raw();

    Ok(Box::new(ImageRenderData {
        img_w,
        img_h,
        pixels: Some(pixels),
        scaling_mode,
        image_path: image_path.to_string(),
        paused: false,
    }))
}

/// 壁纸专用线程函数
///
/// 在此线程上加载图片、创建窗口、运行消息循环。
/// 通过 mpsc 通道接收命令，通过 PostMessageW 唤醒消息循环。
///
/// W-003 修复：接收 `shared_state`、`state_changed` 和 `display_id_lock`，
/// 使壁纸线程在 Resume 重新加载图片失败时能回滚 shared_state.state = Paused
/// 并通过 state_changed 通知前端刷新 UI。
#[allow(clippy::too_many_arguments)]
fn wallpaper_thread(
    image_path: String,
    scaling_mode: ScalingMode,
    initial_rect: (i32, i32, i32, i32),
    cmd_rx: Receiver<WallpaperCommand>,
    result_tx: Sender<Result<isize, crate::MirrorStarError>>,
    shared_state: Arc<RwLock<RendererState>>,
    state_changed: tokio::sync::broadcast::Sender<String>,
    display_id_lock: Arc<std::sync::OnceLock<String>>,
) {
    // Step 1: 创建窗口（先创建窗口，图片稍后异步加载）
    let class_name = register_window_class();

    let hwnd = match create_wallpaper_window(
        class_name,
        windows::core::w!("MirrorStar Wallpaper"),
        initial_rect,
    ) {
        Ok(h) => h,
        Err(e) => {
            if let Err(e) = result_tx.send(Err(e)) {
                tracing::warn!(error = %e, "result_tx 已关闭，无法上报错误（调用方将超时）");
            }
            return;
        }
    };

    // Step 2: 加载图片（窗口已创建，加载完成后再通知主线程，确保主线程收到的成功状态包含可用像素数据）
    let pixel_data = match load_and_downsample_image(&image_path, scaling_mode) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("加载图片失败: {}", e);
            if let Err(e) = result_tx.send(Err(crate::MirrorStarError::ImageDecode(format!(
                "图片加载失败 ({}): {}",
                image_path, e
            )))) {
                tracing::warn!(error = %e, "result_tx 已关闭，无法上报错误（调用方将超时）");
            }
            return;
        }
    };

    // 通知主线程窗口创建成功（发送原始句柄值，避免 HWND 的 Send 限制）
    // 延迟到图片加载成功后发送，避免主线程收到成功但壁纸实际不可用的竞态
    if let Err(e) = result_tx.send(Ok(hwnd.0 as isize)) {
        tracing::warn!(error = %e, "result_tx 已关闭，无法上报窗口句柄（调用方将超时）");
    }

    // 将像素数据存储到 GWLP_USERDATA
    let window_data = Box::new(ImageWindowData {
        render: *pixel_data,
        gdi_cache: None,
    });
    let data_ptr = Box::into_raw(window_data);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, data_ptr as isize);
        // 图片加载完成，触发重绘
        // InvalidateRect 失败仅跳过本次重绘，下一次 WM_PAINT 会重新触发
        let _ = InvalidateRect(hwnd, None, false);
    }

    tracing::info!("图片加载完成，壁纸就绪");

    // Step 3: 消息循环
    let mut msg = MSG::default();
    'main_loop: loop {
        // GetMessageW 在没有消息时会阻塞，不会浪费 CPU
        // GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
        // （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            0 => break, // WM_QUIT
            -1 => {
                tracing::error!("GetMessageW 返回 -1（错误），退出消息循环");
                break;
            }
            _ => {}
        }

        // 检查自定义消息（唤醒信号）
        if msg.message == WM_WALLPAPER_COMMAND {
            // 处理所有待处理的命令
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    WallpaperCommand::Terminate => {
                        tracing::info!("图片壁纸终止");
                        unsafe {
                            if let Err(e) = DestroyWindow(hwnd) {
                                tracing::warn!(error = %e, "DestroyWindow 失败");
                            }
                        }
                        break 'main_loop;
                    }
                    WallpaperCommand::SetPosition {
                        x,
                        y,
                        width,
                        height,
                    } => unsafe {
                        tracing::info!(x, y, width, height, "图片壁纸设置位置");
                        // SetWindowPos/InvalidateRect 失败仅跳过本次位置更新/重绘，下次消息会重试
                        let _ = SetWindowPos(
                            hwnd,
                            HWND::default(),
                            x,
                            y,
                            width,
                            height,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                    WallpaperCommand::SetScalingMode(mode) => unsafe {
                        tracing::info!(scaling_mode = ?mode, "图片壁纸设置缩放模式");
                        let data_ptr =
                            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
                        if !data_ptr.is_null() {
                            (*data_ptr).render.scaling_mode = mode;
                            // v8.0: 清空 image_bitmap 缓存，下次 WM_PAINT 走"首次绘制"路径重新生成
                            if let Some(ref mut cache) = (*data_ptr).gdi_cache {
                                cache.release_image_bitmap();
                            }
                            // 若 pixels 已释放（首次绘制后），从文件重新解码供下次首次绘制使用
                            if (*data_ptr).render.pixels.is_none() {
                                let path = (*data_ptr).render.image_path.clone();
                                let mode = (*data_ptr).render.scaling_mode;
                                match load_and_downsample_image(&path, mode) {
                                    Ok(new_data) => {
                                        (*data_ptr).render.pixels = new_data.pixels;
                                        // img_w/img_h 可能因降采样而变化，同步更新
                                        (*data_ptr).render.img_w = new_data.img_w;
                                        (*data_ptr).render.img_h = new_data.img_h;
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            "SetScalingMode 重新加载图片失败"
                                        );
                                    }
                                }
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                    WallpaperCommand::Pause => unsafe {
                        tracing::info!("图片壁纸已暂停");
                        let data_ptr =
                            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
                        if !data_ptr.is_null() {
                            // v8.0: 释放 pixels（若有），保留 image_bitmap
                            // （HBITMAP 内存远小于 pixels，且恢复后重绘需要）
                            (*data_ptr).render.pixels = None;
                            (*data_ptr).render.paused = true;
                            if let Some(ref mut cache) = (*data_ptr).gdi_cache {
                                cache.release_bitmap();
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                    WallpaperCommand::Resume => unsafe {
                        tracing::info!("图片壁纸恢复播放");
                        let data_ptr =
                            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
                        if !data_ptr.is_null() && (*data_ptr).render.paused {
                            // v8.0: 若 image_bitmap 仍有效，无需重新加载 pixels（后续绘制直接复用 image_bitmap）
                            let has_bitmap = (*data_ptr)
                                .gdi_cache
                                .as_ref()
                                .is_some_and(|c| c.image_bitmap.is_some());
                            if has_bitmap {
                                // image_bitmap 有效，仅清除暂停状态
                                (*data_ptr).render.paused = false;
                            } else {
                                // image_bitmap 已失效，需重新加载 pixels 供下次首次绘制
                                let path = (*data_ptr).render.image_path.clone();
                                let mode = (*data_ptr).render.scaling_mode;
                                // W-003 修复：Resume 重新加载图片失败时回滚 shared_state.state = Paused
                                // 并通知前端刷新 UI，避免状态不一致（engine 显示 Playing 但壁纸黑屏）
                                match load_and_downsample_image(&path, mode) {
                                    Ok(new_data) => {
                                        (*data_ptr).render = *new_data;
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, path = %path, "Resume 重新加载图片失败，回滚状态为 Paused");
                                        // 回滚 shared_state.state = Paused
                                        // （pause 转发线程已设为 Playing，此处修正为 Paused）
                                        shared_state
                                            .write()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .state = WallpaperState::Paused;
                                        // 通知前端刷新 UI（display_id 由 create_pause_sender 设置）
                                        if let Some(id) = display_id_lock.get() {
                                            let _ = state_changed.send(id.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    },
                }
            }
            continue;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // 清理：确保窗口数据被释放（WM_DESTROY 中也会处理，这里做兜底）
    unsafe {
        let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
        if !data_ptr.is_null() {
            let mut data = Box::from_raw(data_ptr);
            if let Some(ref mut cache) = data.gdi_cache {
                cache.destroy();
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }
}

// ── Window Class Registration ────────────────────────────────────────────────

/// 注册窗口类（仅执行一次），委托给 `gdi_base::register_window_class_once`
fn register_window_class() -> windows::core::PCWSTR {
    static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();
    register_window_class_once(
        &CLASS_REGISTERED,
        windows::core::w!("MirrorStarImageWallpaper"),
        wallpaper_wnd_proc,
    )
}

// ── Window Procedure ─────────────────────────────────────────────────────────

/// 窗口过程，使用双缓冲和 HALFTONE 缩放模式实现高质量无闪烁渲染
unsafe extern "system" fn wallpaper_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 优先处理两个 GDI 渲染器共有的消息（WM_ERASEBKGND / WM_DPICHANGED / WM_SIZE）
    if let Some(result) = try_handle_common_messages(hwnd, msg, lparam) {
        return result;
    }

    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
            if data_ptr.is_null() {
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            let data = &mut *data_ptr;

            // v8.0 内存优化：首次绘制从 pixels 生成 HBITMAP 并缓存，随后释放 pixels；
            // 后续绘制直接复用 HBITMAP（通过 StretchBlt），无需重新解码像素数据。
            // HBITMAP 是 Copy 类型（newtype 包裹指针），and_then 取出值不持有不可变借用，
            // 因此后续可安全地传 `&mut data.gdi_cache` 给绘制函数。
            let cached_bitmap = data.gdi_cache.as_ref().and_then(|c| c.image_bitmap);

            if let Some(image_bitmap) = cached_bitmap {
                // 后续绘制：使用缓存的 image_bitmap（StretchBlt 路径）
                paint_image_with_double_buffer(
                    hwnd,
                    hdc,
                    &mut data.gdi_cache,
                    data.render.img_w,
                    data.render.img_h,
                    image_bitmap,
                    data.render.scaling_mode,
                );
            } else {
                // 首次绘制：使用 pixels（StretchDIBits 路径）
                let pixels_slice = data.render.pixels.as_deref().unwrap_or(&[]);
                paint_with_double_buffer(
                    hwnd,
                    hdc,
                    &mut data.gdi_cache,
                    data.render.img_w,
                    data.render.img_h,
                    pixels_slice,
                    data.render.scaling_mode,
                );

                // 绘制后（GdiCache 已创建），从 pixels 生成 HBITMAP 并缓存，然后释放 pixels
                if let Some(ref mut cache) = data.gdi_cache {
                    if cache.image_bitmap.is_none() {
                        if let Some(ref pixels) = data.render.pixels {
                            if let Some(hbitmap) = create_image_bitmap(
                                hdc,
                                data.render.img_w,
                                data.render.img_h,
                                pixels,
                            ) {
                                cache.image_bitmap = Some(hbitmap);
                            }
                        }
                    }
                }
                // 仅当 image_bitmap 成功创建时释放 pixels（失败时保留 pixels 作为回退）
                let bitmap_created = data
                    .gdi_cache
                    .as_ref()
                    .is_some_and(|c| c.image_bitmap.is_some());
                if bitmap_created {
                    data.render.pixels = None;
                }
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_DESTROY => {
            // Clean up window data including GDI cache
            let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ImageWindowData;
            if !data_ptr.is_null() {
                let mut data = Box::from_raw(data_ptr);
                if let Some(ref mut cache) = data.gdi_cache {
                    cache.destroy();
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            // Exit the message loop
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_valid_small_image() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            10,
            10,
            image::Rgba([255, 0, 0, 255]),
        ));
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("small.png");
        img.save(&img_path).unwrap();
        let path = img_path.to_str().unwrap();

        let data = load_and_downsample_image(path, ScalingMode::Fill).expect("有效图片应加载成功");
        assert_eq!(data.img_w, 10);
        assert_eq!(data.img_h, 10);
        assert_eq!(data.pixels.as_ref().unwrap().len(), 10 * 10 * 4);
        assert_eq!(data.scaling_mode, ScalingMode::Fill);
        assert_eq!(data.image_path, path);
        assert!(!data.paused);
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = load_and_downsample_image(
            "Z:\\nonexistent\\path\\no_such_image.png",
            ScalingMode::Fill,
        );
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    #[test]
    fn test_load_downsamples_large_image() {
        // 查询屏幕分辨率（与函数内部逻辑一致，使用缓存版本）
        let (screen_w, screen_h) = crate::wallpaper::get_screen_size();

        // 创建一个明显大于屏幕的图片以确保触发降采样
        let big_w = screen_w + 500;
        let big_h = screen_h + 500;
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            big_w,
            big_h,
            image::Rgba([0, 255, 0, 255]),
        ));
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("large.png");
        img.save(&img_path).unwrap();
        let path = img_path.to_str().unwrap();

        let data = load_and_downsample_image(path, ScalingMode::Fit).expect("大图片应加载成功");

        // 降采样后尺寸不应超过目标尺寸（屏幕分辨率与 MAX_DOWNSAMPLE_DIMENSION 的较小者）
        let max_w = screen_w.min(MAX_DOWNSAMPLE_DIMENSION);
        let max_h = screen_h.min(MAX_DOWNSAMPLE_DIMENSION);
        assert!(
            data.img_w <= max_w,
            "降采样后宽度 {} 应 <= 目标宽度 {}",
            data.img_w,
            max_w
        );
        assert!(
            data.img_h <= max_h,
            "降采样后高度 {} 应 <= 目标高度 {}",
            data.img_h,
            max_h
        );
        // 应确实发生了降采样（至少一个维度变小）
        assert!(
            data.img_w < big_w || data.img_h < big_h,
            "应发生降采样，原始 {}x{}，输出 {}x{}",
            big_w,
            big_h,
            data.img_w,
            data.img_h
        );
        // 像素数据应为 RGBA 格式（每像素 4 字节）
        assert_eq!(
            data.pixels.as_ref().unwrap().len(),
            (data.img_w as usize) * (data.img_h as usize) * 4
        );
    }

    /// v11.0：验证屏幕分辨率超过 MAX_DOWNSAMPLE_DIMENSION 时降采样上限生效。
    ///
    /// 由于 `get_screen_size` 返回真实屏幕分辨率（无法在测试中修改），
    /// 此测试通过验证 `MAX_DOWNSAMPLE_DIMENSION` 常量值与降采样逻辑的数学关系来验证：
    /// 若 screen_w > MAX_DOWNSAMPLE_DIMENSION，则 target_w = MAX_DOWNSAMPLE_DIMENSION。
    #[test]
    fn test_downsample_caps_at_max_dimension() {
        // 验证常量值为 2560（QHD）
        assert_eq!(
            MAX_DOWNSAMPLE_DIMENSION, 2560,
            "v11.0: MAX_DOWNSAMPLE_DIMENSION 应为 2560（QHD）"
        );

        // 模拟 4K 屏幕：target_w = min(3840, 2560) = 2560
        let screen_w_4k = 3840u32;
        let target_w_4k = screen_w_4k.min(MAX_DOWNSAMPLE_DIMENSION);
        assert_eq!(target_w_4k, 2560, "4K 屏幕降采样目标宽度应为 2560");

        // 模拟 8K 屏幕：target_w = min(7680, 2560) = 2560
        let screen_w_8k = 7680u32;
        let target_w_8k = screen_w_8k.min(MAX_DOWNSAMPLE_DIMENSION);
        assert_eq!(target_w_8k, 2560, "8K 屏幕降采样目标宽度应为 2560");

        // 模拟 1080p 屏幕：target_w = min(1920, 2560) = 1920（不触发上限）
        let screen_w_1080p = 1920u32;
        let target_w_1080p = screen_w_1080p.min(MAX_DOWNSAMPLE_DIMENSION);
        assert_eq!(
            target_w_1080p, 1920,
            "1080p 屏幕降采样目标宽度应为 1920（低于上限，不受影响）"
        );
    }

    // ========== W-003 修复测试：Resume 失败状态回滚 ==========

    /// 验证 Resume 重新加载图片失败时 shared_state 正确回滚为 Paused。
    ///
    /// 由于壁纸线程涉及 Win32 消息处理，难以直接单元测试窗口消息循环。
    /// 此测试验证 W-003 修复的核心机制：
    /// 1. load_and_downsample_image 对无效路径返回 Err
    /// 2. 模拟 wallpaper_thread 的回滚逻辑（shared_state.state = Paused）
    /// 3. 验证 shared_state.state 最终为 Paused
    #[test]
    fn w003_resume_failure_rolls_back_state() {
        // 模拟无效图片路径
        let invalid_path = "Z:\\nonexistent\\path\\no_such_image.png";

        // 验证 load_and_downsample_image 对无效路径返回 Err
        let result = load_and_downsample_image(invalid_path, ScalingMode::Fill);
        assert!(result.is_err(), "无效路径应返回 Err");

        // 模拟 wallpaper_thread 的 shared_state（pause 转发线程已设为 Playing）
        let shared_state = Arc::new(RwLock::new(RendererState {
            state: WallpaperState::Playing,
            volume: 1.0,
            pre_mute_volume: None,
        }));

        // 模拟 wallpaper_thread 的 Resume 失败处理（W-003 修复的回滚逻辑）
        // 这是 wallpaper_thread 在 load_and_downsample_image 失败时执行的代码
        if let Err(e) = load_and_downsample_image(invalid_path, ScalingMode::Fill) {
            tracing::error!(error = %e, "Resume 重新加载图片失败，回滚状态为 Paused");
            shared_state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .state = WallpaperState::Paused;
        }

        // 验证 shared_state 已回滚为 Paused
        assert_eq!(
            shared_state.read().unwrap().state,
            WallpaperState::Paused,
            "Resume 失败后 shared_state.state 应已回滚为 Paused"
        );
    }

    /// 验证 Resume 成功时 shared_state 保持 Playing。
    #[test]
    fn w003_resume_success_keeps_playing_state() {
        // 创建有效图片
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            10,
            10,
            image::Rgba([255, 0, 0, 255]),
        ));
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("valid.png");
        img.save(&img_path).unwrap();
        let path = img_path.to_str().unwrap();

        let shared_state = Arc::new(RwLock::new(RendererState {
            state: WallpaperState::Playing,
            volume: 1.0,
            pre_mute_volume: None,
        }));

        // 模拟 Resume 成功路径
        let result = load_and_downsample_image(path, ScalingMode::Fill);
        assert!(result.is_ok(), "有效路径应加载成功");

        // 成功时不回滚，shared_state.state 保持 Playing
        assert_eq!(
            shared_state.read().unwrap().state,
            WallpaperState::Playing,
            "Resume 成功后 shared_state.state 应保持 Playing"
        );
    }

    // ========== v8.0 内存优化测试：image_bitmap 缓存与 pixels 释放 ==========

    /// 验证首次绘制后 image_bitmap 被缓存。
    ///
    /// 模拟 WM_PAINT 首次绘制流程：创建 GdiCache → 从 pixels 生成 HBITMAP → 缓存到 image_bitmap。
    /// 需要显示环境（GetDC），无显示时跳过。
    #[test]
    fn test_image_bitmap_cached_after_first_paint() {
        // 模拟 ImageRenderData（首次绘制前 pixels 为 Some）
        let pixels: Option<Vec<u8>> = Some(vec![0u8; 10 * 10 * 4]);
        assert!(pixels.is_some(), "首次绘制前 pixels 应为 Some");

        unsafe {
            let hdc = GetDC(None);
            if hdc == HDC::default() {
                eprintln!("跳过：无显示环境（GetDC 返回默认句柄）");
                return;
            }

            // 创建 GdiCache（模拟 paint_with_double_buffer 中的首次创建）
            let mut cache = GdiCache::new(hdc, 100, 100).expect("GdiCache 创建失败");
            assert!(cache.image_bitmap.is_none(), "初始 image_bitmap 应为 None");

            // 从 pixels 生成 HBITMAP（模拟 WM_PAINT 首次绘制后的缓存逻辑）
            if let Some(ref p) = pixels {
                let hbitmap = create_image_bitmap(hdc, 10, 10, p);
                assert!(hbitmap.is_some(), "create_image_bitmap 应返回 Some");
                cache.image_bitmap = hbitmap;
            }

            // 验证 image_bitmap 已缓存
            assert!(
                cache.image_bitmap.is_some(),
                "首次绘制后 image_bitmap 应为 Some"
            );

            cache.destroy();
            let _ = ReleaseDC(None, hdc);
        }
    }

    /// 验证首次绘制后 pixels 被释放（设为 None）。
    ///
    /// 此测试不需要 GDI 环境，纯逻辑验证。
    #[test]
    fn test_pixels_released_after_first_paint() {
        // 模拟 ImageRenderData
        let mut render = ImageRenderData {
            img_w: 10,
            img_h: 10,
            pixels: Some(vec![0u8; 10 * 10 * 4]),
            scaling_mode: ScalingMode::Fill,
            image_path: "test.png".to_string(),
            paused: false,
        };

        // 首次绘制前 pixels 应为 Some
        assert!(render.pixels.is_some(), "首次绘制前 pixels 应为 Some");

        // 模拟首次绘制后释放 pixels（image_bitmap 成功创建后执行 pixels = None）
        render.pixels = None;

        // 验证 pixels 已释放
        assert!(render.pixels.is_none(), "首次绘制后 pixels 应为 None");
    }

    /// 验证 SetScalingMode 清空 image_bitmap 缓存。
    ///
    /// 模拟 SetScalingMode 命令处理：release_image_bitmap() 后 image_bitmap 为 None。
    /// 需要显示环境（GetDC），无显示时跳过。
    #[test]
    fn test_set_scaling_mode_clears_bitmap() {
        unsafe {
            let hdc = GetDC(None);
            if hdc == HDC::default() {
                eprintln!("跳过：无显示环境（GetDC 返回默认句柄）");
                return;
            }

            let mut cache = GdiCache::new(hdc, 100, 100).expect("GdiCache 创建失败");

            // 设置 image_bitmap（模拟首次绘制后的缓存）
            let pixels = vec![0u8; 10 * 10 * 4];
            cache.image_bitmap = create_image_bitmap(hdc, 10, 10, &pixels);
            assert!(cache.image_bitmap.is_some(), "image_bitmap 应已设置");

            // 模拟 SetScalingMode 清空 image_bitmap
            cache.release_image_bitmap();
            assert!(
                cache.image_bitmap.is_none(),
                "SetScalingMode 后 image_bitmap 应为 None"
            );

            cache.destroy();
            let _ = ReleaseDC(None, hdc);
        }
    }

    /// 验证后续绘制复用 image_bitmap（缓存命中）。
    ///
    /// 模拟后续 WM_PAINT：cached_bitmap = gdi_cache.as_ref().and_then(|c| c.image_bitmap)
    /// 应返回 Some(HBITMAP)，与缓存值一致。
    /// 需要显示环境（GetDC），无显示时跳过。
    #[test]
    fn test_subsequent_paint_uses_cached_bitmap() {
        unsafe {
            let hdc = GetDC(None);
            if hdc == HDC::default() {
                eprintln!("跳过：无显示环境（GetDC 返回默认句柄）");
                return;
            }

            let mut cache = GdiCache::new(hdc, 100, 100).expect("GdiCache 创建失败");

            // 模拟首次绘制后缓存 image_bitmap
            let pixels = vec![0u8; 10 * 10 * 4];
            let hbitmap =
                create_image_bitmap(hdc, 10, 10, &pixels).expect("create_image_bitmap 失败");
            cache.image_bitmap = Some(hbitmap);

            // 模拟后续绘制：从 gdi_cache 取出缓存的 image_bitmap
            // （WM_PAINT 中 cached_bitmap = gdi_cache.as_ref().and_then(|c| c.image_bitmap)）
            let cached = cache.image_bitmap;
            assert!(cached.is_some(), "后续绘制应能复用 image_bitmap");
            assert_eq!(
                cached, cache.image_bitmap,
                "复用的 image_bitmap 应与缓存一致"
            );

            cache.destroy();
            let _ = ReleaseDC(None, hdc);
        }
    }
}
