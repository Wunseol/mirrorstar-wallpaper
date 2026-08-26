//! 壁纸模式分发：根据壁纸来源与类型决定使用 Native API 还是 WorkerW 嵌入。
//!
//! # 模式切换状态机（v41-W-015）
//!
//! `WallpaperMode` 有两个状态，由 `determine_wallpaper_mode` 根据壁纸类型与来源决定：
//!
//! - `Native`：静态图片 + 原生 API 支持的格式（.jpg/.png 等）。零资源占用，
//!   不创建渲染器，不嵌入 WorkerW，直接调用 `native_wallpaper::set_wallpaper`。
//! - `WorkerW`：GIF / Video / Web，或原生 API 不支持的图片格式。创建对应渲染器，
//!   通过 `DesktopIntegrator::embed_wallpaper` 嵌入 WorkerW 层。
//!
//! ## 切换条件
//!
//! | 类型 | 来源 | 格式原生支持 | 模式 |
//! |------|------|--------------|------|
//! | Image | File | 是 | Native |
//! | Image | File | 否 | WorkerW |
//! | Image | Url | — | WorkerW（仅 File 支持 Native） |
//! | Gif/Video/Web | * | — | WorkerW |
//!
//! ## 切换副作用
//!
//! - **进入 Native**：`set_native_wallpaper_internal` 设置原生壁纸，`pause_senders`
//!   不注册（Native 模式无快速控制路径），`wallpaper_mode` 记录 Native。
//! - **进入 WorkerW**：`construct_renderer` 创建渲染器并 `play()`（含 IPC 连接），
//!   `embed_and_register_renderer` 嵌入 WorkerW + 注册 `pause_senders` +
//!   `wallpaper_sources`，`wallpaper_mode` 记录 WorkerW。
//! - **模式切换**（同显示器重新设置壁纸）：`prepare_set_wallpaper` 先关闭现有壁纸
//!   （Native 调用 `clear_native_wallpaper`，WorkerW 调用 `close_wallpaper` 清理渲染器），
//!   再按新类型决定模式。Native → WorkerW 或 WorkerW → Native 均通过此路径完成。
//!
//! ## 分发风格
//!
//! 模式分发统一在 `WallpaperEngine::set_wallpaper` 中通过 `pending.mode` 判断：
//! Native 分支调用 `set_native_wallpaper_internal`（返回 `Result`），
//! WorkerW 分支调用 `construct_renderer` + `complete_set_wallpaper`（返回 `Result`）。
//! 所有分支均返回 `Result<(), MirrorStarError>`，错误用 `MirrorStarError` 包装。

use crate::desktop::native_wallpaper;
use crate::wallpaper::{ScalingMode, WallpaperSource, WallpaperType};
use crate::MirrorStarError;

use super::manager::{construct_renderer, WallpaperEngine};

/// 壁纸模式：跟踪当前壁纸使用原生 API 还是 WorkerW 嵌入
#[derive(Debug, Clone, PartialEq)]
pub enum WallpaperMode {
    /// 静态图片使用 Windows 原生 API（零资源占用）
    Native,
    /// 动态壁纸使用 WorkerW 嵌入
    WorkerW,
}

/// 根据壁纸来源和类型决定使用 Native 还是 WorkerW 模式
///
/// - Image + 支持原生 API 的文件格式 → Native
/// - Image + 不支持原生 API 的格式 → WorkerW
/// - Gif/Video/Web → WorkerW
pub fn determine_wallpaper_mode(
    source: &WallpaperSource,
    wallpaper_type: WallpaperType,
) -> WallpaperMode {
    match wallpaper_type {
        WallpaperType::Image => {
            if let WallpaperSource::File(path) = source {
                if native_wallpaper::is_native_supported(path) {
                    return WallpaperMode::Native;
                }
            }
            WallpaperMode::WorkerW
        }
        WallpaperType::Gif | WallpaperType::Video | WallpaperType::Web => WallpaperMode::WorkerW,
    }
}

impl WallpaperEngine {
    /// 设置壁纸到指定显示器（同步路径：在 engine 锁内完成全流程）
    ///
    /// # 职责边界（N-004）
    ///
    /// 本方法是**同步路径**入口，负责模式分发（Native vs WorkerW）并执行具体设置：
    /// - Native 模式：委托给 `set_native_wallpaper_internal`（消除重复调用）
    /// - WorkerW 模式：调用 `ensure_desktop_ready_with_retry` → 创建渲染器 →
    ///   `embed_and_register_renderer`
    ///
    /// 与之并行的还有 `src-tauri/src/commands/wallpaper.rs::set_wallpaper` 的
    /// **3 阶段异步路径**：`prepare_for_wallpaper` → `create_and_play_renderer`
    /// （锁外）→ `embed_and_register_renderer`。两条路径独立维护，互不调用。
    ///
    /// 调用者：`WallpaperEngine::set_scaling_mode` 在视频壁纸切换缩放模式时
    /// 调用本方法重启渲染器。
    ///
    /// # W04 锁范围说明
    ///
    /// 本方法内部使用 `prepare_set_wallpaper` / `complete_set_wallpaper` 分阶段
    /// 执行，但作为 `&mut self` 便利方法，整个调用仍持有 engine 锁（包括
    /// `construct_renderer` 内的 `play()`，最长达 8s 的 IPC 连接）。
    ///
    /// 需要将 `play()` 移出 engine 锁范围的调用方应直接使用三阶段 API：
    /// `prepare_set_wallpaper`（持锁）→ `construct_renderer`（锁外）→
    /// `complete_set_wallpaper`（持锁）。
    ///
    /// # 已知限制 (v41-W-006)
    ///
    /// 持有 `wallpaper_engine.lock().await` 期间调用 `construct_renderer`，
    /// IPC 等待首帧（Web 类型 wp-proc 连接最长 8s）会阻塞其他命令
    /// （pause/resume/set_volume）。
    ///
    /// **降级修复方案**（避免大重构引入新风险）：
    /// 1. 文档化权衡：本方法保留为同步便利路径，调用方若对锁竞争敏感应改用
    ///    三阶段 API（`prepare_set_wallpaper` / 锁外 `construct_renderer` /
    ///    `complete_set_wallpaper`）。
    /// 2. 缩短 IPC 超时：`web.rs::WP_PROC_CONNECT_RETRIES` 从 100 次缩减到 40 次
    ///    （40 * 200ms = 8s 兜底），将持锁最坏时长从 20s 缩减到 8s。8s 在多数
    ///    环境下足够 WebView2 冷启动（典型 5-15s 的下限），慢速环境下返回错误
    ///    让用户重试，比 20s 阻塞所有命令更可接受。
    pub fn set_wallpaper(
        &mut self,
        display_id: &str,
        source: &WallpaperSource,
        wallpaper_type: WallpaperType,
        scaling_mode: ScalingMode,
    ) -> Result<(), MirrorStarError> {
        tracing::info!(display_id, ?wallpaper_type, "设置壁纸");

        // W04: 阶段 1 — 准备（关闭现有壁纸 + 获取配置快照 + 决定模式）
        let pending = self.prepare_set_wallpaper(display_id, source, wallpaper_type)?;

        // Native 模式：直接设置（无需 play()），在锁内完成
        if pending.mode == WallpaperMode::Native {
            if let WallpaperSource::File(path) = source {
                tracing::info!(path = %path, "使用 Windows 原生 API 设置静态壁纸");
                return self.set_native_wallpaper_internal(
                    display_id,
                    path,
                    scaling_mode,
                    source,
                    wallpaper_type,
                );
            } else {
                return Err(MirrorStarError::DesktopIntegration(
                    "静态图片壁纸仅支持本地文件".to_string(),
                ));
            }
        }

        // WorkerW 模式：校验文件路径（Web 类型支持 URL）
        match wallpaper_type {
            WallpaperType::Image => {
                if !matches!(source, WallpaperSource::File(_)) {
                    return Err(MirrorStarError::DesktopIntegration(
                        "静态图片壁纸仅支持本地文件".to_string(),
                    ));
                }
            }
            WallpaperType::Video => {
                if !matches!(source, WallpaperSource::File(_)) {
                    return Err(MirrorStarError::DesktopIntegration(
                        "视频壁纸仅支持本地文件".to_string(),
                    ));
                }
            }
            WallpaperType::Gif => {
                if !matches!(source, WallpaperSource::File(_)) {
                    return Err(MirrorStarError::DesktopIntegration(
                        "GIF 壁纸仅支持本地文件".to_string(),
                    ));
                }
            }
            WallpaperType::Web => {
                // Web 类型支持 File 和 Url，无需校验
            }
        }

        if let WallpaperSource::File(path) = source {
            tracing::info!(path = %path, ?wallpaper_type, "准备加载 WorkerW 壁纸");
        } else if let WallpaperSource::Url(url) = source {
            tracing::info!(source = %url, "准备加载网页壁纸");
        }

        // W04: 阶段 2 — 创建并播放渲染器（仍持锁，便利方法）
        // 注意：完整 W04 修复需要调用方在此处释放 engine 锁，锁外执行 play()。
        // 当前保留便利方法以维持 API 兼容性，调用方可改用三阶段 API 实现锁外 play()。
        let renderer = construct_renderer(
            source,
            wallpaper_type,
            scaling_mode,
            &pending.config,
            pending.clear_native,
        )?;

        // W04: 阶段 3 — 嵌入并注册（在锁内）
        self.complete_set_wallpaper(renderer, display_id, source, wallpaper_type)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::volume::VolumeControl;
    use crate::desktop::DesktopIntegrator;
    use crate::wallpaper::{create_pause_channel, PauseSender, WallpaperRenderer, WallpaperState};
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    #[test]
    fn determine_mode_image_native_supported() {
        // .jpg files are supported by native API
        let source = WallpaperSource::File("test.jpg".to_string());
        let mode = determine_wallpaper_mode(&source, WallpaperType::Image);
        assert_eq!(mode, WallpaperMode::Native);
    }

    #[test]
    fn determine_mode_image_native_not_supported() {
        // .webp files are NOT supported by native API
        let source = WallpaperSource::File("test.webp".to_string());
        let mode = determine_wallpaper_mode(&source, WallpaperType::Image);
        assert_eq!(mode, WallpaperMode::WorkerW);
    }

    #[test]
    fn determine_mode_gif_always_workerw() {
        let source = WallpaperSource::File("test.gif".to_string());
        let mode = determine_wallpaper_mode(&source, WallpaperType::Gif);
        assert_eq!(mode, WallpaperMode::WorkerW);
    }

    #[test]
    fn determine_mode_video_always_workerw() {
        let source = WallpaperSource::File("test.mp4".to_string());
        let mode = determine_wallpaper_mode(&source, WallpaperType::Video);
        assert_eq!(mode, WallpaperMode::WorkerW);
    }

    #[test]
    fn determine_mode_web_always_workerw() {
        let source = WallpaperSource::Url("https://example.com".to_string());
        let mode = determine_wallpaper_mode(&source, WallpaperType::Web);
        assert_eq!(mode, WallpaperMode::WorkerW);
    }

    // ========== Mock Renderer & 集成测试 ==========

    /// Mock 渲染器，用于测试 WallpaperEngine 的状态管理
    ///
    /// `hwnd()` 返回 `None` 以跳过 WorkerW 嵌入逻辑，使 `embed_and_register_renderer`
    /// 可以在无 Win32 桌面环境的情况下完成状态注册。
    struct MockRenderer {
        state: WallpaperState,
        played: bool,
        terminated: bool,
        pause_sender: Option<PauseSender>,
    }

    impl MockRenderer {
        fn new() -> Self {
            let (sender, _rx, _shared) = create_pause_channel();
            Self {
                state: WallpaperState::Initializing,
                played: false,
                terminated: false,
                pause_sender: Some(sender),
            }
        }
    }

    impl WallpaperRenderer for MockRenderer {
        fn play(&mut self) -> Result<(), crate::MirrorStarError> {
            self.played = true;
            self.state = WallpaperState::Playing;
            Ok(())
        }
        fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Paused;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
            self.state = WallpaperState::Playing;
            Ok(())
        }
        fn set_position(
            &mut self,
            _x: i32,
            _y: i32,
            _w: i32,
            _h: i32,
        ) -> Result<(), crate::MirrorStarError> {
            Ok(())
        }
        fn terminate(&mut self) -> Result<(), crate::MirrorStarError> {
            self.terminated = true;
            self.state = WallpaperState::Terminated;
            Ok(())
        }
        fn hwnd(&self) -> Option<HWND> {
            None // 返回 None 跳过 WorkerW 嵌入
        }
        fn state(&self) -> WallpaperState {
            self.state
        }
        fn create_pause_sender(&mut self, _display_id: &str) -> Option<PauseSender> {
            self.pause_sender.take()
        }
    }

    /// 创建测试用 WallpaperEngine
    ///
    /// 初始化 COM（MTA 模式）并构造真实的 `DesktopIntegrator` 和 `VolumeControl`。
    /// 如果 COM 环境不可用（如无音频设备的 CI 环境），返回 `None` 让调用方跳过测试。
    ///
    /// 注意：不调用 `CoUninitialize`，由测试线程退出时自动清理 COM 引用计数。
    fn create_test_engine() -> Option<WallpaperEngine> {
        // 尝试初始化 COM（MTA 模式）。若线程已以其他模式初始化，
        // CoInitializeEx 返回 RPC_E_CHANGED_MODE，但 COM 仍可能可用。
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        let desktop = Arc::new(Mutex::new(DesktopIntegrator::new()));
        let volume_control = match VolumeControl::new() {
            Ok(vc) => Arc::new(Mutex::new(vc)),
            Err(_) => return None,
        };

        Some(WallpaperEngine::new(desktop, volume_control))
    }

    // ---------- is_native_mode 测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_is_native_mode() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        // 未设置任何模式
        assert!(!engine.is_native_mode("monitor_0"));

        // 设置 Native 模式
        engine
            .wallpaper_mode
            .insert("monitor_0".to_string(), WallpaperMode::Native);
        assert!(engine.is_native_mode("monitor_0"));
        assert!(!engine.is_native_mode("monitor_1")); // 其他显示器

        // 设置 WorkerW 模式
        engine
            .wallpaper_mode
            .insert("monitor_1".to_string(), WallpaperMode::WorkerW);
        assert!(!engine.is_native_mode("monitor_1"));

        // 清除 Native 模式记录，避免 Drop 时调用 clear_native_wallpaper
        engine.wallpaper_mode.clear();
    }

    // ---------- close_wallpaper_by_path 测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_close_wallpaper_by_path_finds_match() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let display_id = "monitor_0".to_string();
        let path = "/test/wallpaper.mp4".to_string();

        // 模拟已设置的壁纸状态（WorkerW 模式，无渲染器）
        // 不添加渲染器到 wallpapers，使 close_wallpaper 跳过 terminate 和 desktop.remove_wallpaper
        engine
            .wallpaper_mode
            .insert(display_id.clone(), WallpaperMode::WorkerW);
        engine.wallpaper_sources.insert(
            display_id.clone(),
            (WallpaperSource::File(path.clone()), WallpaperType::Video),
        );

        // close_wallpaper_by_path 应找到并关闭
        let result = engine.close_wallpaper_by_path(&path);
        assert!(result.is_ok());

        // 验证状态已清除
        assert!(!engine.wallpaper_sources.contains_key(&display_id));
        assert!(!engine.wallpaper_mode.contains_key(&display_id));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_close_wallpaper_by_path_no_match() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let display_id = "monitor_0".to_string();
        let path = "/test/wallpaper.mp4".to_string();

        // 设置壁纸状态
        engine
            .wallpaper_mode
            .insert(display_id.clone(), WallpaperMode::WorkerW);
        engine.wallpaper_sources.insert(
            display_id.clone(),
            (WallpaperSource::File(path.clone()), WallpaperType::Video),
        );

        // 用不匹配的路径调用，应返回 Ok(()) 且不修改状态
        let result = engine.close_wallpaper_by_path("/different/path.mp4");
        assert!(result.is_ok());

        // 验证状态未被修改
        assert!(engine.wallpaper_sources.contains_key(&display_id));
        assert!(engine.wallpaper_mode.contains_key(&display_id));
    }

    // ---------- 使用 MockRenderer 的集成测试 ----------

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_embed_and_register_renderer_with_mock() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let display_id = "monitor_0".to_string();
        let path = "/test/wallpaper.mp4".to_string();
        let source = WallpaperSource::File(path.clone());

        // 使用 MockRenderer（hwnd 返回 None，跳过 WorkerW 嵌入）
        let renderer = Box::new(MockRenderer::new());
        let result = engine.embed_and_register_renderer(
            renderer,
            &display_id,
            &source,
            WallpaperType::Video,
        );
        assert!(result.is_ok());

        // 验证所有状态已注册
        assert!(engine.wallpapers.contains_key(&display_id));
        assert_eq!(
            engine.wallpaper_mode.get(&display_id),
            Some(&WallpaperMode::WorkerW)
        );
        assert!(engine.wallpaper_sources.contains_key(&display_id));
        assert!(engine.pause_senders.contains_key(&display_id));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_close_wallpaper_with_mock_renderer() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let display_id = "monitor_0".to_string();
        let path = "/test/wallpaper.mp4".to_string();
        let source = WallpaperSource::File(path.clone());

        // 先注册 MockRenderer
        let renderer = Box::new(MockRenderer::new());
        engine
            .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
            .unwrap();
        assert!(engine.wallpapers.contains_key(&display_id));

        // 关闭壁纸（会调用 renderer.terminate 和 desktop.remove_wallpaper）
        // desktop.remove_wallpaper 在 active_wallpapers 为空时直接返回 Ok
        let result = engine.close_wallpaper(&display_id);
        assert!(result.is_ok());

        // 验证所有状态已清除
        assert!(!engine.wallpapers.contains_key(&display_id));
        assert!(!engine.wallpaper_mode.contains_key(&display_id));
        assert!(!engine.wallpaper_sources.contains_key(&display_id));
        assert!(!engine.pause_senders.contains_key(&display_id));
    }

    #[ignore = "需 Windows 真机 COM/音频环境"]
    #[test]
    fn test_close_wallpaper_by_path_with_mock_renderer() {
        let mut engine = match create_test_engine() {
            Some(e) => e,
            None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
        };

        let display_id = "monitor_0".to_string();
        let path = "/test/wallpaper.mp4".to_string();
        let source = WallpaperSource::File(path.clone());

        // 先注册 MockRenderer
        let renderer = Box::new(MockRenderer::new());
        engine
            .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
            .unwrap();

        // 按路径关闭
        let result = engine.close_wallpaper_by_path(&path);
        assert!(result.is_ok());

        // 验证所有状态已清除
        assert!(!engine.wallpapers.contains_key(&display_id));
        assert!(!engine.wallpaper_mode.contains_key(&display_id));
        assert!(!engine.wallpaper_sources.contains_key(&display_id));
        assert!(!engine.pause_senders.contains_key(&display_id));
    }
}
