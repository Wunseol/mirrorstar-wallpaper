//! 集成测试通用辅助模块
//!
//! 提供 MockRenderer（绕过 WorkerW 嵌入的测试双倍）和测试引擎/配置管理器构造辅助。
//! Tauri 命令层（`commands/wallpaper.rs`）是 `WallpaperEngine` 和 `ConfigManager` 的薄封装，
//! 因此集成测试直接测试这些核心类型即可覆盖命令背后的逻辑。
//!
//! 注：`#[allow(dead_code)]` 用于抑制跨测试文件的 dead_code 警告——每个集成测试文件
//! 作为独立 crate 编译，只会使用 common 模块的部分函数。

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use mirrorstar_core::wallpaper::{
    create_pause_channel, PauseCommand, PauseSender, ScalingMode, WallpaperRenderer, WallpaperState,
};
use mirrorstar_core::{DesktopIntegrator, VolumeControl, WallpaperEngine};
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

/// 共享的位置快照句柄：用于 update_positions 测试跨线程读取 set_position 调用参数 (x, y, w, h)
type SharedPosition = Arc<Mutex<Option<(i32, i32, i32, i32)>>>;

/// Mock 渲染器，用于测试 WallpaperEngine 的状态管理
///
/// `hwnd()` 返回 `None` 以跳过 WorkerW 嵌入逻辑，使 `embed_and_register_renderer`
/// 可以在无 Win32 桌面环境的情况下完成状态注册。
///
/// 重要：`_rx` 字段保持 pause channel 的接收端存活，否则 `sender.send()` 会因
/// 接收端已丢弃而返回错误。接收端在 MockRenderer 被 drop 时自动清理。
pub struct MockRenderer {
    state: WallpaperState,
    pause_sender: Option<PauseSender>,
    /// 保持 pause channel 接收端存活，防止 sender.send() 失败
    _rx: Option<tokio::sync::mpsc::UnboundedReceiver<PauseCommand>>,
    /// 最近一次 set_position 调用的参数快照（用于 update_positions 测试）
    /// 通过 Arc<Mutex> 共享给测试，使渲染器嵌入 engine 后仍可读取
    last_position: SharedPosition,
    /// 最近一次 set_interaction_mode 调用的参数快照（用于 set_interaction_mode 命令测试）
    last_interaction_mode: Arc<Mutex<Option<bool>>>,
    /// 最近一次 set_mouse_passthrough 调用的参数快照（用于 set_interaction_mode 命令测试）
    last_mouse_passthrough: Arc<Mutex<Option<bool>>>,
}

impl MockRenderer {
    pub fn new() -> Self {
        let (sender, rx, _shared) = create_pause_channel();
        Self {
            state: WallpaperState::Initializing,
            pause_sender: Some(sender),
            _rx: Some(rx),
            last_position: Arc::new(Mutex::new(None)),
            last_interaction_mode: Arc::new(Mutex::new(None)),
            last_mouse_passthrough: Arc::new(Mutex::new(None)),
        }
    }

    /// 返回最近一次 set_position 调用参数的共享句柄
    ///
    /// 测试在嵌入 engine 前调用以获取 Arc 克隆，渲染器被 engine 持有后仍可读取
    /// 该句柄验证 update_positions 是否真正调用了 set_position。
    pub fn last_position_handle(&self) -> SharedPosition {
        Arc::clone(&self.last_position)
    }

    /// 返回最近一次 set_interaction_mode 调用参数的共享句柄
    ///
    /// 测试在嵌入 engine 前调用以获取 Arc 克隆，渲染器被 engine 持有后仍可读取，
    /// 用于验证 set_interaction_mode 命令是否真正将模式传播到渲染器。
    pub fn last_interaction_mode_handle(&self) -> Arc<Mutex<Option<bool>>> {
        Arc::clone(&self.last_interaction_mode)
    }

    /// 返回最近一次 set_mouse_passthrough 调用参数的共享句柄
    pub fn last_mouse_passthrough_handle(&self) -> Arc<Mutex<Option<bool>>> {
        Arc::clone(&self.last_mouse_passthrough)
    }
}

impl Default for MockRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl WallpaperRenderer for MockRenderer {
    fn play(&mut self) -> Result<(), mirrorstar_core::MirrorStarError> {
        self.state = WallpaperState::Playing;
        Ok(())
    }
    fn pause(&mut self) -> Result<(), mirrorstar_core::MirrorStarError> {
        self.state = WallpaperState::Paused;
        Ok(())
    }
    fn resume(&mut self) -> Result<(), mirrorstar_core::MirrorStarError> {
        self.state = WallpaperState::Playing;
        Ok(())
    }
    fn set_position(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), mirrorstar_core::MirrorStarError> {
        *self.last_position.lock().unwrap() = Some((x, y, w, h));
        Ok(())
    }
    fn terminate(&mut self) -> Result<(), mirrorstar_core::MirrorStarError> {
        self.state = WallpaperState::Terminated;
        Ok(())
    }
    fn hwnd(&self) -> Option<HWND> {
        None // 返回 None 跳过 WorkerW 嵌入
    }
    fn state(&self) -> WallpaperState {
        self.state
    }
    fn set_speed(&mut self, _speed: f32) {}
    fn set_scaling_mode(&mut self, _mode: ScalingMode) {}
    fn set_mouse_passthrough(&mut self, enabled: bool) {
        *self.last_mouse_passthrough.lock().unwrap() = Some(enabled);
    }
    fn set_interaction_mode(&mut self, enabled: bool) {
        *self.last_interaction_mode.lock().unwrap() = Some(enabled);
    }
    fn create_pause_sender(&mut self, _display_id: &str) -> Option<PauseSender> {
        self.pause_sender.take()
    }
}

/// 创建测试用 WallpaperEngine
///
/// 初始化 COM（STA 模式，对齐生产环境 `src-tauri/src/lib.rs:44`）并构造真实的
/// `DesktopIntegrator` 和 `VolumeControl`。如果 COM 环境不可用（如无音频设备的 CI 环境），
/// 返回 `None` 让调用方跳过测试。
///
/// 注意：不调用 `CoUninitialize`，由测试线程退出时自动清理 COM 引用计数。
pub fn create_test_engine() -> Option<WallpaperEngine> {
    // T-010: 测试 COM 模式对齐生产环境（STA / COINIT_APARTMENTTHREADED）。
    // 若线程已初始化为 MTA，CoInitializeEx 返回 RPC_E_CHANGED_MODE，
    // 此时无法切换为 STA——记录 warn 日志说明测试结果可能不精确（COM 仍可能可用）。
    // CoInitializeEx 返回 HRESULT，调用 .ok() 转为 Result<(), Error> 后模式匹配，
    // 与生产环境 `src-tauri/src/lib.rs:44` 和 `commands/wallpaper.rs:26` 范式一致。
    match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() } {
        Ok(()) => {}
        Err(e) if e.code() == RPC_E_CHANGED_MODE => {
            tracing::warn!("测试线程已初始化为 MTA，STA 初始化失败（测试结果可能不精确）");
        }
        Err(e) => {
            tracing::warn!(error = ?e, "CoInitializeEx(STA) 失败");
        }
    }

    let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
    let volume_control = match VolumeControl::new() {
        Ok(vc) => Arc::new(Mutex::new(vc)),
        Err(_) => return None,
    };

    Some(WallpaperEngine::new(desktop, volume_control))
}

/// 创建测试用 DesktopIntegrator（用于 get_displays 命令等价测试）
///
/// 初始化 COM（STA 模式，对齐生产环境 `src-tauri/src/lib.rs:44`）并构造真实的
/// `DesktopIntegrator`。与 `create_test_engine` 不同，本函数不构造 VolumeControl，
/// 因此不依赖音频设备；仅在 COM 不可用时返回 None 让调用方跳过测试。
///
/// 注意：不调用 `CoUninitialize`，由测试线程退出时自动清理 COM 引用计数。
pub fn create_test_desktop() -> Option<Arc<Mutex<DesktopIntegrator>>> {
    // T-010: 测试 COM 模式对齐生产环境（STA / COINIT_APARTMENTTHREADED）。
    // 若线程已初始化为 MTA，CoInitializeEx 返回 RPC_E_CHANGED_MODE，
    // 此时无法切换为 STA——记录 warn 日志说明测试结果可能不精确（COM 仍可能可用）。
    // CoInitializeEx 返回 HRESULT，调用 .ok() 转为 Result<(), Error> 后模式匹配，
    // 与生产环境 `src-tauri/src/lib.rs:44` 和 `commands/wallpaper.rs:26` 范式一致。
    match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() } {
        Ok(()) => {}
        Err(e) if e.code() == RPC_E_CHANGED_MODE => {
            tracing::warn!("测试线程已初始化为 MTA，STA 初始化失败（测试结果可能不精确）");
        }
        Err(e) => {
            tracing::warn!(error = ?e, "CoInitializeEx(STA) 失败");
        }
    }
    Some(Arc::new(Mutex::new(DesktopIntegrator::new())))
}

/// 创建测试用 ConfigManager
///
/// 使用 `tempfile::TempDir` 创建独立临时目录并在其中初始化 `ConfigManager`，
/// 避免污染用户数据目录（`%APPDATA%/mirrorstar/`，C-020）。
///
/// T-013: 返回 `(ConfigManager, TempDir)` 元组，调用方必须持有 `TempDir` 直至测试结束，
/// 离开作用域时自动删除临时目录及配置文件。原实现使用 `std::env::temp_dir()` 按进程 ID
/// 拼接路径，存在跨测试残留与并发冲突风险；现改为每次调用创建独立 `TempDir`，
/// 既保证隔离又自动清理。
pub fn create_test_config_manager() -> (mirrorstar_core::ConfigManager, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().expect("创建临时目录失败");
    let cm = mirrorstar_core::ConfigManager::new_in_dir(temp_dir.path().to_path_buf())
        .expect("创建测试 ConfigManager 失败");
    (cm, temp_dir)
}

/// 构造测试用 WallpaperEntry
pub fn make_test_entry(
    id: &str,
    file_path: &str,
    wallpaper_type: mirrorstar_core::WallpaperType,
) -> mirrorstar_core::config::WallpaperEntry {
    mirrorstar_core::config::WallpaperEntry {
        id: id.to_string(),
        file_path: file_path.to_string(),
        wallpaper_type,
        display_id: None,
        added_at: "0".to_string(),
        thumbnail: String::new(),
        file_size: 0,
        metadata: None,
        normalized_path: String::new(),
    }
}
