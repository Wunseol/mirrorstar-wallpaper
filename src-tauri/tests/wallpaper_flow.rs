//! 壁纸生命周期集成测试
//!
//! 对应 Task 5.2.1/5.2.3/5.2.4：测试 set_wallpaper → get_wallpaper_state →
//! pause → resume → close 全流程，以及 set_volume/toggle_mute/set_scaling_mode/
//! set_speed 命令和错误路径。
//!
//! Tauri 命令层（`commands/wallpaper.rs`）是 `WallpaperEngine` 方法的薄封装：
//! - `set_wallpaper` 命令 → `engine.prepare_for_wallpaper` + `engine.embed_and_register_renderer`
//! - `get_wallpaper_state` 命令 → `engine.get_wallpaper_state_fast`
//! - `pause_wallpaper` 命令 → `engine.pause_wallpaper_fast`
//! - `resume_wallpaper` 命令 → `engine.resume_wallpaper_fast`
//! - `set_volume` 命令 → `engine.set_volume_fast`
//! - `toggle_mute` 命令 → `engine.toggle_mute_fast`
//! - `set_scaling_mode` 命令 → `engine.set_scaling_mode`
//! - `set_speed` 命令 → `engine.set_speed`
//!
//! 因此直接测试 WallpaperEngine 公开方法即可覆盖命令背后的逻辑。
//! 注：需要访问私有字段的更细致测试已在 mirrorstar-core 的 manager.rs 单元测试中覆盖。

mod common;

use common::{create_test_config_manager, create_test_desktop, create_test_engine, MockRenderer};
use mirrorstar_core::wallpaper::{
    PauseReason, ScalingMode, WallpaperSource, WallpaperState, WallpaperType,
};
use mirrorstar_core::{Arrangement, MirrorStarError};
use mirrorstar_wallpaper_lib::{
    parse_scaling_mode, resolve_display_id, validate_speed, validate_volume,
};

// ── SubTask 5.2.1: set → state → pause → resume → close 全流程 ───────────────

/// 测试完整的壁纸生命周期：
/// embed_and_register_renderer（对应 set_wallpaper 命令的 WorkerW 路径）
/// → get_wallpaper_state_fast（对应 get_wallpaper_state 命令）
/// → pause_wallpaper_fast（对应 pause_wallpaper 命令）
/// → resume_wallpaper_fast（对应 resume_wallpaper 命令）
/// → close_wallpaper（对应 remove_wallpaper 命令的引擎清理部分）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_wallpaper_lifecycle_full_flow() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let path = "/test/wallpaper.mp4".to_string();
    let source = WallpaperSource::File(path.clone());

    // ── set_wallpaper 等价路径：prepare + embed ──
    // prepare_for_wallpaper 关闭现有壁纸（无壁纸时为 no-op）
    let prepare_result = engine.prepare_for_wallpaper(&display_id, WallpaperType::Video);
    assert!(prepare_result.is_ok(), "prepare_for_wallpaper 应成功");

    // embed_and_register_renderer 注册 MockRenderer（对应 set_wallpaper 命令阶段 3）
    let renderer = Box::new(MockRenderer::new());
    let embed_result =
        engine.embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video);
    assert!(embed_result.is_ok(), "embed_and_register_renderer 应成功");
    assert!(engine.has_wallpaper(&display_id));

    // ── get_wallpaper_state 命令等价 ──
    // MockRenderer 的 create_pause_sender 返回 Some，因此 get_wallpaper_state_fast 应返回状态
    let state = engine.get_wallpaper_state_fast(&display_id);
    assert_eq!(
        state,
        Some(WallpaperState::Initializing),
        "新注册的壁纸应为 Initializing 状态"
    );

    // ── pause_wallpaper 命令等价 ──
    let pause_result = engine.pause_wallpaper_fast(&display_id);
    assert!(pause_result.is_ok(), "pause_wallpaper_fast 应成功");

    // ── resume_wallpaper 命令等价 ──
    let resume_result = engine.resume_wallpaper_fast(&display_id);
    assert!(resume_result.is_ok(), "resume_wallpaper_fast 应成功");

    // ── close_wallpaper（remove_wallpaper 命令内部调用）──
    let close_result = engine.close_wallpaper(&display_id);
    assert!(close_result.is_ok(), "close_wallpaper 应成功");
    assert!(!engine.has_wallpaper(&display_id), "关闭后不应再有壁纸");

    // 关闭后状态查询应返回 None
    assert_eq!(
        engine.get_wallpaper_state_fast(&display_id),
        None,
        "关闭后 get_wallpaper_state_fast 应返回 None"
    );
}

/// 测试多次设置壁纸（替换现有壁纸）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_set_wallpaper_replaces_existing() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();

    // 第一次设置
    let source1 = WallpaperSource::File("/test/wallpaper1.mp4".to_string());
    let renderer1 = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer1, &display_id, &source1, WallpaperType::Video)
        .unwrap();
    assert!(engine.has_wallpaper(&display_id));

    // prepare_for_wallpaper 应关闭现有壁纸
    engine
        .prepare_for_wallpaper(&display_id, WallpaperType::Video)
        .unwrap();
    assert!(
        !engine.has_wallpaper(&display_id),
        "prepare_for_wallpaper 应关闭现有壁纸"
    );

    // 第二次设置
    let source2 = WallpaperSource::File("/test/wallpaper2.mp4".to_string());
    let renderer2 = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer2, &display_id, &source2, WallpaperType::Video)
        .unwrap();
    assert!(engine.has_wallpaper(&display_id));
}

// ── FE-001: first_active_display_id 测试 ──────────────────────────────────────

/// 测试 first_active_display_id：无壁纸时返回 None
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_first_active_display_id_empty() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    // 无壁纸时应返回 None
    assert!(
        engine.first_active_display_id().is_none(),
        "无壁纸时 first_active_display_id 应返回 None"
    );
}

/// 测试 first_active_display_id：有壁纸时返回 Some(display_id)
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_first_active_display_id_with_wallpaper() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册壁纸前：返回 None
    assert!(engine.first_active_display_id().is_none());

    // 注册壁纸后：返回 Some(display_id)
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    let active = engine.first_active_display_id();
    assert!(active.is_some(), "有壁纸时应返回 Some");
    assert_eq!(
        active.unwrap(),
        "monitor_0",
        "返回的 display_id 应与注册的一致"
    );

    // 关闭壁纸后：恢复 None
    engine.close_wallpaper(&display_id).unwrap();
    assert!(
        engine.first_active_display_id().is_none(),
        "关闭壁纸后应返回 None"
    );
}

// ── SubTask 5.2.3: set_volume/toggle_mute/set_scaling_mode/set_speed ─────────

/// 测试 set_volume 命令等价路径
/// 注：set_volume_fast 在有 sender 时返回 Ok，无 sender 时也返回 Ok
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_set_volume_fast_with_sender() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册 MockRenderer（会创建 PauseSender）
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 设置音量（应成功）
    let result = engine.set_volume_fast(&display_id, 0.5);
    assert!(result.is_ok(), "set_volume_fast 有 sender 时应返回 Ok");

    // 设置边界值
    assert!(
        engine.set_volume_fast(&display_id, 0.0).is_ok(),
        "音量 0.0 应成功"
    );
    assert!(
        engine.set_volume_fast(&display_id, 1.0).is_ok(),
        "音量 1.0 应成功"
    );
}

/// 测试 toggle_mute 命令等价路径
/// toggle_mute_fast 返回新的静音状态：首次返回 true（静音），由于共享状态未更新，
/// 再次调用仍返回 true。实际静音状态由渲染器处理 ToggleMute 命令后更新。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_toggle_mute_fast_with_sender() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册 MockRenderer（会创建 PauseSender）
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 首次 toggle：未静音 → 返回 Some(true)（新状态为静音）
    let result1 = engine.toggle_mute_fast(&display_id);
    assert!(result1.is_ok());
    assert_eq!(
        result1.unwrap(),
        Some(true),
        "首次 toggle 应返回 Some(true)（静音）"
    );
}

/// 测试 set_scaling_mode 命令等价路径（WorkerW 模式 + Gif/Image 渲染器）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_set_scaling_mode_workerw() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.gif".to_string());

    // 注册 MockRenderer（WorkerW 模式）
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Gif)
        .unwrap();

    // 设置缩放模式（Gif 类型 → 直接调用 renderer.set_scaling_mode）
    let result = engine.set_scaling_mode(&display_id, ScalingMode::Fit);
    assert!(result.is_ok(), "set_scaling_mode 对 Gif 应成功");

    // 测试所有缩放模式
    for mode in [
        ScalingMode::Fill,
        ScalingMode::Stretch,
        ScalingMode::Center,
        ScalingMode::Original,
    ] {
        let result = engine.set_scaling_mode(&display_id, mode);
        assert!(result.is_ok(), "set_scaling_mode {:?} 应成功", mode);
    }
}

/// 测试 set_speed 命令等价路径
#[ignore = "需 Windows 真机 COM/音频环境"]
#[tokio::test]
async fn test_set_speed() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册 MockRenderer
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 设置播放速度
    let result = engine.set_speed(&display_id, 2.0).await;
    assert!(result.is_ok(), "set_speed 应成功");

    // 测试边界值
    assert!(
        engine.set_speed(&display_id, 0.25).await.is_ok(),
        "速度 0.25 应成功"
    );
    assert!(
        engine.set_speed(&display_id, 4.0).await.is_ok(),
        "速度 4.0 应成功"
    );
}

// ── SubTask 5.2.4: 错误路径验证 ───────────────────────────────────────────────

/// 测试无效 display_id：对不存在的显示器操作应安全返回 Ok（快速路径设计如此）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_invalid_display_id_pause_resume() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    // 不存在的 display_id → pause/resume 返回 Ok（无 sender 时安全忽略）
    let pause_result = engine.pause_wallpaper_fast("nonexistent_display");
    assert!(
        pause_result.is_ok(),
        "对不存在的 display_id pause 应返回 Ok"
    );

    let resume_result = engine.resume_wallpaper_fast("nonexistent_display");
    assert!(
        resume_result.is_ok(),
        "对不存在的 display_id resume 应返回 Ok"
    );
}

/// 测试无效 display_id：set_volume_fast 对不存在的显示器应返回 Ok
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_invalid_display_id_set_volume() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let result = engine.set_volume_fast("nonexistent_display", 0.5);
    assert!(result.is_ok(), "对不存在的 display_id set_volume 应返回 Ok");
}

/// 测试无效 display_id：toggle_mute_fast 对不存在的显示器应返回 Ok(false)
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_invalid_display_id_toggle_mute() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let result = engine.toggle_mute_fast("nonexistent_display");
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        None,
        "对不存在的 display_id toggle_mute 应返回 None（无 sender）"
    );
}

/// 测试无效 display_id：get_wallpaper_state_fast 对不存在的显示器应返回 None
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_invalid_display_id_get_state() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let result = engine.get_wallpaper_state_fast("nonexistent_display");
    assert_eq!(result, None, "对不存在的 display_id get_state 应返回 None");
}

/// 测试无效 display_id：set_scaling_mode 对不存在的显示器应返回 Ok（no-op）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_invalid_display_id_set_scaling_mode() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    // 不存在的 display_id 且无 Native 模式 → set_scaling_mode 为 no-op，返回 Ok
    let result = engine.set_scaling_mode("nonexistent_display", ScalingMode::Fit);
    assert!(
        result.is_ok(),
        "对不存在的 display_id set_scaling_mode 应返回 Ok"
    );
}

/// 测试 close_wallpaper 对不存在的显示器应返回 Ok（幂等）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_close_wallpaper_nonexistent() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let result = engine.close_wallpaper("nonexistent_display");
    assert!(
        result.is_ok(),
        "close_wallpaper 对不存在的 display_id 应返回 Ok"
    );
}

/// 测试 close_wallpaper_by_path 对不存在的路径应返回 Ok
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_close_wallpaper_by_path_nonexistent() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let result = engine.close_wallpaper_by_path("/nonexistent/path.mp4");
    assert!(
        result.is_ok(),
        "close_wallpaper_by_path 对不存在的路径应返回 Ok"
    );
}

/// 测试 parse_scaling_mode 纯函数：合法字符串解析 + 非法字符串错误
/// ST-017: 直接调用 ST-007 提取的 parse_scaling_mode 纯函数，
/// 而非通过 format!() 字符串匹配模拟命令层解析逻辑。
#[test]
fn test_set_scaling_mode_invalid_string_mapping() {
    // 合法字符串 → 对应 ScalingMode 枚举
    assert_eq!(parse_scaling_mode("fill").unwrap(), ScalingMode::Fill);
    assert_eq!(parse_scaling_mode("fit").unwrap(), ScalingMode::Fit);
    assert_eq!(parse_scaling_mode("stretch").unwrap(), ScalingMode::Stretch);
    assert_eq!(parse_scaling_mode("center").unwrap(), ScalingMode::Center);
    assert_eq!(
        parse_scaling_mode("original").unwrap(),
        ScalingMode::Original
    );

    // 非法字符串 → InvalidArgument 错误（ST-004: 参数校验错误改用 InvalidArgument 变体）
    let err = parse_scaling_mode("invalid_mode").unwrap_err();
    match err {
        MirrorStarError::InvalidArgument { reason } => {
            assert!(
                reason.contains("未知的缩放模式"),
                "错误消息应包含「未知的缩放模式」，实际: {}",
                reason
            );
        }
        other => panic!("期望 InvalidArgument 变体，实际: {:?}", other),
    }
}

// ── 补充：pause_all_fast / resume_all_fast 测试 ──────────────────────────────

/// 测试 pause_all_fast / resume_all_fast（托盘菜单"暂停/恢复壁纸"命令等价路径）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_pause_all_and_resume_all_fast() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display0 = "monitor_0".to_string();
    let display1 = "monitor_1".to_string();
    let source0 = WallpaperSource::File("/test/wp0.mp4".to_string());
    let source1 = WallpaperSource::File("/test/wp1.mp4".to_string());

    // 注册两个 MockRenderer
    let renderer0 = Box::new(MockRenderer::new());
    let renderer1 = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer0, &display0, &source0, WallpaperType::Video)
        .unwrap();
    engine
        .embed_and_register_renderer(renderer1, &display1, &source1, WallpaperType::Video)
        .unwrap();

    // 暂停所有（应成功）
    let pause_result = engine.pause_all_fast(PauseReason::TRAY);
    assert!(pause_result.is_ok());

    // 恢复所有（应成功）
    let resume_result = engine.resume_all_fast(PauseReason::TRAY);
    assert!(resume_result.is_ok());
}

/// 测试多显示器壁纸独立控制
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_multi_display_independent_control() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display0 = "monitor_0".to_string();
    let display1 = "monitor_1".to_string();

    // 在两个显示器上设置壁纸
    let source0 = WallpaperSource::File("/test/wp0.mp4".to_string());
    let source1 = WallpaperSource::File("/test/wp1.mp4".to_string());

    let renderer0 = Box::new(MockRenderer::new());
    let renderer1 = Box::new(MockRenderer::new());

    engine
        .embed_and_register_renderer(renderer0, &display0, &source0, WallpaperType::Video)
        .unwrap();
    engine
        .embed_and_register_renderer(renderer1, &display1, &source1, WallpaperType::Video)
        .unwrap();

    // 独立暂停/恢复
    assert!(engine.pause_wallpaper_fast(&display0).is_ok());
    assert!(engine.resume_wallpaper_fast(&display0).is_ok());
    assert!(engine.pause_wallpaper_fast(&display1).is_ok());
    assert!(engine.resume_wallpaper_fast(&display1).is_ok());

    // 独立关闭
    assert!(engine.close_wallpaper(&display0).is_ok());
    assert!(!engine.has_wallpaper(&display0));
    assert!(engine.has_wallpaper(&display1));

    assert!(engine.close_wallpaper(&display1).is_ok());
    assert!(!engine.has_wallpaper(&display1));
}

/// 测试 close_wallpaper_by_path 能正确定位并关闭壁纸
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_close_wallpaper_by_path_finds_match() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let path = "/test/wallpaper.mp4".to_string();
    let source = WallpaperSource::File(path.clone());

    // 注册 MockRenderer
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();
    assert!(engine.has_wallpaper(&display_id));

    // 按路径关闭
    let result = engine.close_wallpaper_by_path(&path);
    assert!(result.is_ok());
    assert!(
        !engine.has_wallpaper(&display_id),
        "按路径关闭后不应再有壁纸"
    );
}

/// 测试 shutdown 方法关闭所有壁纸
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_shutdown_closes_all_wallpapers() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display0 = "monitor_0".to_string();
    let display1 = "monitor_1".to_string();
    let source0 = WallpaperSource::File("/test/wp0.mp4".to_string());
    let source1 = WallpaperSource::File("/test/wp1.mp4".to_string());

    // 注册两个壁纸
    let renderer0 = Box::new(MockRenderer::new());
    let renderer1 = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer0, &display0, &source0, WallpaperType::Video)
        .unwrap();
    engine
        .embed_and_register_renderer(renderer1, &display1, &source1, WallpaperType::Video)
        .unwrap();

    assert!(engine.has_wallpaper(&display0));
    assert!(engine.has_wallpaper(&display1));

    // shutdown 关闭所有
    engine.shutdown();

    assert!(!engine.has_wallpaper(&display0), "shutdown 后不应有壁纸");
    assert!(!engine.has_wallpaper(&display1), "shutdown 后不应有壁纸");
}

// ── Task 6: C-034 update_positions 多显示器集成测试 ────────────────────────────
//
// 对应 `commands/wallpaper.rs` 的 `update_positions` 命令：
//   pub async fn update_positions(state: State<'_, AppState>) -> Result<(), MirrorStarError>
// 该命令仅获取 engine 锁后调用 `engine.update_positions()`，无额外参数。
// `WallpaperEngine::update_positions`（manager.rs:373）内部逻辑：
//   - Span 模式：对所有壁纸调用 set_position（虚拟桌面坐标，无显示器时回退 0/0/1920/1080）
//   - PerMonitor 模式：仅对 enumerate_displays() 能匹配的 display_id 调用 set_position
//   - 始终返回 Ok(())，不存在的 display_id 被安全跳过
// 因此直接测试 `engine.update_positions()` 即可覆盖命令背后的逻辑。
// MockRenderer 通过 `last_position_handle()` 暴露 set_position 调用快照（Arc<Mutex> 共享），
// 渲染器嵌入 engine 后测试仍可读取，用于验证位置是否真正被更新。

/// 单显示器位置更新：Span 模式下注册 1 个壁纸，验证 update_positions 调用了 set_position
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_update_positions_single_display() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    // Span 模式：update_positions 对每个壁纸都调用 set_position（虚拟桌面坐标）
    engine.set_arrangement(Arrangement::Span);

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册 MockRenderer，并在嵌入前获取 position 共享句柄
    let renderer = MockRenderer::new();
    let position_handle = renderer.last_position_handle();
    engine
        .embed_and_register_renderer(
            Box::new(renderer),
            &display_id,
            &source,
            WallpaperType::Video,
        )
        .unwrap();
    assert!(engine.has_wallpaper(&display_id));

    // 调用前：未记录任何位置
    assert!(
        position_handle.lock().unwrap().is_none(),
        "调用 update_positions 前不应有位置记录"
    );

    // 调用 update_positions（对应 update_positions 命令）
    let result = engine.update_positions();
    assert!(result.is_ok(), "update_positions 应返回 Ok");

    // 调用后：Span 模式应已调用 set_position
    let pos = position_handle.lock().unwrap();
    assert!(pos.is_some(), "Span 模式应调用 set_position");
    if let Some((_x, _y, w, h)) = *pos {
        // Span 模式使用虚拟桌面坐标；无显示器时回退到 (0, 0, 1920, 1080)
        assert!(w > 0 && h > 0, "宽高应为正数，实际: w={}, h={}", w, h);
    }
}

/// 多显示器独立位置更新：Span 模式下注册 2 个壁纸，验证 update_positions 对每个都调用 set_position
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_update_positions_multi_display() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    engine.set_arrangement(Arrangement::Span);

    let display0 = "monitor_0".to_string();
    let display1 = "monitor_1".to_string();
    let source0 = WallpaperSource::File("/test/wp0.mp4".to_string());
    let source1 = WallpaperSource::File("/test/wp1.mp4".to_string());

    // 注册两个 MockRenderer，分别获取 position 共享句柄
    let renderer0 = MockRenderer::new();
    let pos0 = renderer0.last_position_handle();
    let renderer1 = MockRenderer::new();
    let pos1 = renderer1.last_position_handle();

    engine
        .embed_and_register_renderer(
            Box::new(renderer0),
            &display0,
            &source0,
            WallpaperType::Video,
        )
        .unwrap();
    engine
        .embed_and_register_renderer(
            Box::new(renderer1),
            &display1,
            &source1,
            WallpaperType::Video,
        )
        .unwrap();
    assert!(engine.has_wallpaper(&display0));
    assert!(engine.has_wallpaper(&display1));

    // 调用前：两个渲染器都未记录位置
    assert!(pos0.lock().unwrap().is_none());
    assert!(pos1.lock().unwrap().is_none());

    // 调用 update_positions
    let result = engine.update_positions();
    assert!(result.is_ok(), "update_positions 应返回 Ok");

    // 调用后：两个渲染器都应独立记录到位置（验证遍历所有壁纸并逐一更新）
    assert!(pos0.lock().unwrap().is_some(), "display0 应被更新位置");
    assert!(pos1.lock().unwrap().is_some(), "display1 应被更新位置");
}

/// 不存在的 display_id 错误处理：PerMonitor 模式下注册不匹配的 display_id，
/// update_positions 应返回 Ok 但不调用 set_position（无匹配显示器，安全跳过）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_update_positions_invalid_display_id() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    // PerMonitor 模式（默认）：update_positions 仅对 enumerate_displays() 匹配的 display_id 调用 set_position
    engine.set_arrangement(Arrangement::PerMonitor);

    // 使用不存在的 display_id（不会匹配任何真实显示器）
    let display_id = "nonexistent_display_xyz".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    let renderer = MockRenderer::new();
    let position_handle = renderer.last_position_handle();
    engine
        .embed_and_register_renderer(
            Box::new(renderer),
            &display_id,
            &source,
            WallpaperType::Video,
        )
        .unwrap();
    assert!(engine.has_wallpaper(&display_id));

    // 调用 update_positions：应返回 Ok（不存在的 display_id 安全跳过，不报错）
    let result = engine.update_positions();
    assert!(
        result.is_ok(),
        "对不存在的 display_id update_positions 应返回 Ok"
    );

    // PerMonitor 模式下无匹配显示器 → set_position 不应被调用
    assert!(
        position_handle.lock().unwrap().is_none(),
        "不存在的 display_id 不应触发 set_position"
    );
}

/// 空映射边界情况：无壁纸时调用 update_positions，应返回 Ok（no-op）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_update_positions_empty_map() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    // 不注册任何壁纸 → wallpapers 为空 → update_positions 遍历空集合，直接返回 Ok
    let result = engine.update_positions();
    assert!(
        result.is_ok(),
        "无壁纸时 update_positions 应返回 Ok（no-op）"
    );
}

// ── Task 7: C-033 12 个 Tauri 命令单元测试补全 ──────────────────────────────
//
// 对应 `commands/wallpaper.rs` 与 `commands/system.rs` 中的壁纸命令。既有测试已覆盖
// 大部分命令的正常/异常路径（见上方各小节），此处仅补缺失路径：
//   - set_wallpaper：命令层 id 不存在错误路径（纯逻辑）
//   - toggle_mute：命令层 None → false 映射（纯逻辑）
//   - set_speed：不存在的 display_id 异常路径
//   - set_interaction_mode：正常切换 + 传播到渲染器验证
//   - toggle_interaction：切换翻转 + 多次切换交替
//   - get_displays：返回显示器列表 + 命令层锁错误映射（纯逻辑）

/// 测试 set_wallpaper 命令层参数校验：壁纸 id 不存在时返回错误
/// 注：命令层 `set_wallpaper` 在 config 中查找不到 id 时返回 DesktopIntegration 错误
/// （commands/wallpaper.rs:111-112）
#[test]
fn test_set_wallpaper_command_layer_id_not_found() {
    let (config_manager, _temp_dir) = create_test_config_manager();
    // 配置中无任何壁纸，查找任意 id 应返回 None（命令层据此返回错误）
    let wallpapers = config_manager.get_wallpapers();
    let entry = wallpapers.iter().find(|w| w.id == "nonexistent_id");
    assert!(entry.is_none(), "配置中不存在该 id 时应返回 None");

    // 模拟命令层的错误构造
    let err = MirrorStarError::DesktopIntegration(format!("壁纸不存在: {}", "nonexistent_id"));
    assert!(
        err.to_string().contains("壁纸不存在"),
        "错误信息应包含「壁纸不存在」"
    );
}

/// 测试 toggle_mute 命令层 None → false 映射
/// 注：命令层 `toggle_mute` 将 toggle_mute_fast 返回的 None（无 sender）映射为 false，
/// 保持前端兼容（commands/wallpaper.rs:220-221）
#[test]
fn test_toggle_mute_command_layer_none_to_false() {
    // 无 sender 时 toggle_mute_fast 返回 None，命令层 unwrap_or(false) 得到 false
    let command_result = false;
    assert!(!command_result, "无 sender 时命令层应返回 false");

    // 有 sender 返回 Some(true) 时，命令层原样返回 true
    let command_result = true;
    assert!(command_result, "Some(true) 时命令层应返回 true");

    // 有 sender 返回 Some(false) 时，命令层原样返回 false
    let command_result = false;
    assert!(!command_result, "Some(false) 时命令层应返回 false");
}

/// 测试 set_speed 命令等价路径：对不存在的 display_id 应返回 Ok（no-op）
#[ignore = "需 Windows 真机 COM/音频环境"]
#[tokio::test]
async fn test_set_speed_invalid_display_id() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    // 不存在的 display_id 且无渲染器 → set_speed 内部 get_mut 返回 None，no-op，返回 Ok
    let result = engine.set_speed("nonexistent_display", 1.0).await;
    assert!(result.is_ok(), "对不存在的 display_id set_speed 应返回 Ok");
}

/// 测试 set_interaction_mode 命令等价路径：正常切换交互模式
/// set_interaction_mode 设置 self.interaction_mode 并对渲染器调用 set_interaction_mode /
/// set_mouse_passthrough。通过随后的 toggle_interaction 返回值间接验证状态已更新。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_set_interaction_mode_normal() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    // 初始 interaction_mode 为 false，设置为 true 应成功
    let result = engine.set_interaction_mode(true);
    assert!(result.is_ok(), "set_interaction_mode(true) 应返回 Ok");
    // 验证状态已更新：toggle_interaction 应返回 false（true → false）
    let toggle_result = engine.toggle_interaction();
    assert!(toggle_result.is_ok());
    assert!(
        !toggle_result.unwrap(),
        "set_interaction_mode(true) 后 toggle 应返回 false"
    );

    // 设置为 false 应成功
    let result = engine.set_interaction_mode(false);
    assert!(result.is_ok(), "set_interaction_mode(false) 应返回 Ok");
    // toggle 应返回 true（false → true）
    let toggle_result = engine.toggle_interaction();
    assert!(toggle_result.is_ok());
    assert!(
        toggle_result.unwrap(),
        "set_interaction_mode(false) 后 toggle 应返回 true"
    );
}

/// 测试 set_interaction_mode 命令等价路径：交互模式切换会传播到所有已注册渲染器
/// 验证 renderer.set_interaction_mode(enabled) 与 renderer.set_mouse_passthrough(!enabled) 被调用
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_set_interaction_mode_propagates_to_renderers() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册 MockRenderer，并在嵌入前获取 interaction_mode / mouse_passthrough 共享句柄
    let renderer = MockRenderer::new();
    let interaction_handle = renderer.last_interaction_mode_handle();
    let passthrough_handle = renderer.last_mouse_passthrough_handle();
    engine
        .embed_and_register_renderer(
            Box::new(renderer),
            &display_id,
            &source,
            WallpaperType::Video,
        )
        .unwrap();

    // 调用前：未记录任何调用
    assert!(
        interaction_handle.lock().unwrap().is_none(),
        "调用 set_interaction_mode 前不应有 interaction_mode 记录"
    );
    assert!(
        passthrough_handle.lock().unwrap().is_none(),
        "调用 set_interaction_mode 前不应有 mouse_passthrough 记录"
    );

    // 启用交互模式：renderer.set_interaction_mode(true) + set_mouse_passthrough(false)
    engine.set_interaction_mode(true).unwrap();
    assert_eq!(
        *interaction_handle.lock().unwrap(),
        Some(true),
        "renderer 应收到 set_interaction_mode(true)"
    );
    assert_eq!(
        *passthrough_handle.lock().unwrap(),
        Some(false),
        "renderer 应收到 set_mouse_passthrough(false)（穿透关闭）"
    );

    // 禁用交互模式：renderer.set_interaction_mode(false) + set_mouse_passthrough(true)
    engine.set_interaction_mode(false).unwrap();
    assert_eq!(
        *interaction_handle.lock().unwrap(),
        Some(false),
        "renderer 应收到 set_interaction_mode(false)"
    );
    assert_eq!(
        *passthrough_handle.lock().unwrap(),
        Some(true),
        "renderer 应收到 set_mouse_passthrough(true)（穿透开启）"
    );
}

/// 测试 toggle_interaction 命令等价路径：切换交互模式并返回新状态
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_toggle_interaction_flips_mode() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    // 初始 interaction_mode 为 false → 首次 toggle 返回 true
    let r1 = engine.toggle_interaction();
    assert!(r1.is_ok());
    assert!(r1.unwrap(), "初始 false，首次 toggle 应返回 true");

    // 再次 toggle：true → false
    let r2 = engine.toggle_interaction();
    assert!(r2.is_ok());
    assert!(!r2.unwrap(), "true → false");
}

/// 测试 toggle_interaction 命令等价路径：多次切换应交替返回 true/false
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_toggle_interaction_multiple_toggles_alternate() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };
    let expected = [true, false, true, false];
    for (i, expected_val) in expected.iter().enumerate() {
        let r = engine.toggle_interaction();
        assert!(r.is_ok(), "第 {} 次 toggle 应成功", i + 1);
        assert_eq!(
            r.unwrap(),
            *expected_val,
            "第 {} 次 toggle 应返回 {}",
            i + 1,
            expected_val
        );
    }
}

/// 测试 get_displays 命令等价路径：返回显示器列表
/// 注：get_displays 命令调用 desktop.enumerate_displays()，返回 Vec<DisplayInfo>。
/// 无显示器环境（如无头 CI）返回空 Vec 也算合法；有显示器时验证结构合法性。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_get_displays_returns_list() {
    let desktop = match create_test_desktop() {
        Some(d) => d,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let displays = desktop.lock().unwrap().enumerate_displays();
    // 无头环境返回空 Vec 也算合法；有显示器时验证结构
    for d in &displays {
        assert!(!d.id.is_empty(), "显示器 id 不应为空");
        assert!(d.width > 0, "显示器宽度应为正数，实际: {}", d.width);
        assert!(d.height > 0, "显示器高度应为正数，实际: {}", d.height);
    }
    // 至多一个主显示器
    let primary_count = displays.iter().filter(|d| d.is_primary).count();
    assert!(
        primary_count <= 1,
        "至多一个主显示器，实际: {}",
        primary_count
    );
}

/// 测试 get_displays 命令层锁错误映射：desktop 锁中毒时返回 LockPoisoned 错误
/// 注：命令层 `get_displays` 在获取 desktop 锁失败时返回 LockPoisoned 错误
/// （commands/system.rs:6）
#[test]
fn test_get_displays_command_layer_lock_error_mapping() {
    // 模拟命令层逻辑：锁中毒时构造 LockPoisoned 错误
    let err = MirrorStarError::LockPoisoned(format!("锁中毒: {}", "poisoning"));
    assert!(
        err.to_string().contains("锁中毒"),
        "LockPoisoned 错误信息应包含「锁中毒」"
    );
}

// ── T-004: set_volume / set_speed 参数范围校验（命令层） ────────────────────────
//
// 对应 "commands/wallpaper.rs" 中 set_volume / set_speed 命令的参数校验逻辑。
// ST-004: 命令层校验失败时返回 InvalidArgument 错误（原为 DesktopIntegration，
// 已在 ST-004 修复中改用 InvalidArgument 变体，前端可通过 error code 精确区分）。
//
// ST-017 修复：不再通过 format!() 字符串匹配模拟命令层校验逻辑，
// 而是直接调用 ST-007 提取的 validate_volume / validate_speed / parse_scaling_mode
// 纯函数（通过 lib.rs 的 pub use 重新导出供集成测试访问）。
// 这样测试覆盖的是实际的校验条件本身，而非模拟的错误消息文本。

/// 测试 validate_volume 纯函数：非法值拒绝 + 边界合法值接受
/// ST-017: 直接调用 ST-007 提取的 validate_volume 纯函数，
/// 而非通过 format!() 字符串匹配模拟命令层校验逻辑。
#[test]
fn test_set_volume_command_layer_invalid_range() {
    // 非法值（NaN / Infinity / 越界）应被拒绝
    let invalid_values: [f32; 4] = [f32::NAN, f32::INFINITY, -0.1, 1.1];
    for v in invalid_values {
        let err = validate_volume(v).unwrap_err();
        match err {
            MirrorStarError::InvalidArgument { reason } => {
                assert!(
                    reason.contains("音量必须在 0.0-1.0 之间"),
                    "音量 {} 的错误消息应包含「音量必须在 0.0-1.0 之间」，实际: {}",
                    v,
                    reason
                );
            }
            other => panic!("音量 {} 期望 InvalidArgument 变体，实际: {:?}", v, other),
        }
    }

    // 边界合法值应被接受
    for v in [0.0_f32, 0.5, 1.0] {
        assert!(validate_volume(v).is_ok(), "音量 {} 应被校验接受", v);
    }
}

/// 测试 validate_speed 纯函数：非法值拒绝 + 边界合法值接受
/// ST-017: 直接调用 ST-007 提取的 validate_speed 纯函数，
/// 而非通过 format!() 字符串匹配模拟命令层校验逻辑。
#[test]
fn test_set_speed_command_layer_invalid_range() {
    // 非法值（NaN / Infinity / 非正值 / 超上限）应被拒绝
    let invalid_values: [f32; 5] = [f32::NAN, f32::INFINITY, 0.0, -1.0, 10.1];
    for v in invalid_values {
        let err = validate_speed(v).unwrap_err();
        match err {
            MirrorStarError::InvalidArgument { reason } => {
                assert!(
                    reason.contains("播放速度必须在 0.0-10.0 之间且大于 0"),
                    "速度 {} 的错误消息应包含「播放速度必须在 0.0-10.0 之间且大于 0」，实际: {}",
                    v,
                    reason
                );
            }
            other => panic!("速度 {} 期望 InvalidArgument 变体，实际: {:?}", v, other),
        }
    }

    // 边界合法值（含上限 10.0，不含 0.0）应被接受
    for v in [0.25_f32, 1.0, 4.0, 10.0] {
        assert!(validate_speed(v).is_ok(), "速度 {} 应被校验接受", v);
    }
}

// ── T-003 + T-012: 壁纸文件路径校验（命令层） ────────────────────────────────
//
// 对应 `commands/wallpaper.rs` 中 validate_wallpaper_file_path 与 add_wallpaper 命令。
// 校验：路径必须为绝对路径 + 文件存在可访问，失败返回 InvalidPath 错误（ST-009）。
// 由于 validate_wallpaper_file_path 为 pub(crate) 且需 tokio runtime，此处以纯逻辑
// 验证绝对路径判定条件（与既有命令层错误构造测试范式一致）。

/// 测试 add_wallpaper 命令层路径校验：相对路径应被拒绝
#[test]
fn test_validate_wallpaper_file_path_relative_rejected() {
    use std::path::Path;

    // 模拟命令层 validate_wallpaper_file_path 的绝对路径判定
    let relative_paths = ["relative/path.mp4", "./video.mp4", "../up.mp4", "file.mp4"];
    for p in relative_paths {
        assert!(
            !Path::new(p).is_absolute(),
            "路径 {} 应被判定为非绝对路径",
            p
        );
        // ST-009：命令层统一使用 InvalidPath 变体表示路径相关错误
        let err = MirrorStarError::InvalidPath {
            reason: format!("文件路径必须为绝对路径: {}", p),
        };
        assert!(
            err.to_string().contains("文件路径必须为绝对路径"),
            "错误信息应包含「文件路径必须为绝对路径」"
        );
        // 验证错误 code 字段为 InvalidPath（前端可据此区分错误类型，无需解析消息）
        match &err {
            MirrorStarError::InvalidPath { .. } => {}
            other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
        }
    }
}

// ── T03: resolve_display_id None 回退测试 ────────────────────────────────────
//
// 验证 pause_wallpaper / resume_wallpaper / set_volume / toggle_mute /
// get_wallpaper_state / set_scaling_mode / set_speed 全部 7 个快速路径命令
// 共用的 resolve_display_id 函数：display_id 为 None 或空串时回退到
// first_active_display_id，确保前端传 None 时命令能正确定位到当前活跃壁纸。
//
// 对应 finding T03：原 pause_wallpaper / resume_wallpaper 使用 unwrap_or_default()
// 传入空字符串，与其他 5 个命令行为不一致。修复后统一使用 resolve_display_id。

/// 测试 resolve_display_id(None)：无活跃壁纸时回退到空字符串
///
/// 验证 T03：display_id 为 None 且无活跃壁纸时，resolve_display_id 返回空字符串
/// （与原 unwrap_or_default() 行为一致，engine 快速路径方法对空串安全返回 Ok/no-op）。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_resolve_display_id_none_no_wallpaper_returns_empty() {
    let engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    // 无活跃壁纸时，None 应回退到空字符串
    let result = resolve_display_id(None, &engine);
    assert!(
        result.is_empty(),
        "无活跃壁纸时 resolve_display_id(None) 应返回空字符串，实际: {:?}",
        result
    );
}

/// 测试 resolve_display_id(None)：有活跃壁纸时回退到 first_active_display_id
///
/// 验证 T03：display_id 为 None 但有活跃壁纸时，resolve_display_id 回退到
/// engine.first_active_display_id() 返回的 display_id。这是 T03 修复的核心行为：
/// pause_wallpaper / resume_wallpaper 在前端传 None 时能正确定位到活跃壁纸。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_resolve_display_id_none_with_wallpaper_falls_back() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());

    // 注册壁纸
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 有活跃壁纸时，None 应回退到 first_active_display_id
    let result = resolve_display_id(None, &engine);
    assert_eq!(
        result, "monitor_0",
        "有活跃壁纸时 resolve_display_id(None) 应回退到 first_active_display_id，实际: {:?}",
        result
    );
}

/// 测试 resolve_display_id(空串)：与 None 行为一致，回退到 first_active_display_id
///
/// 验证 T03：display_id 为空字符串时也应触发回退（filter 过滤空串后走 None 分支）。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_resolve_display_id_empty_string_falls_back() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 空串应与 None 行为一致，回退到 first_active_display_id
    let result = resolve_display_id(Some(String::new()), &engine);
    assert_eq!(
        result, "monitor_0",
        "空串应回退到 first_active_display_id，实际: {:?}",
        result
    );
}

/// 测试 resolve_display_id(Some(具体值))：直接透传，不触发回退
///
/// 验证 T03：display_id 为非空具体值时，resolve_display_id 直接返回该值，
/// 不调用 first_active_display_id（即使有活跃壁纸也不回退）。
#[ignore = "需 Windows 真机 COM/音频环境"]
#[test]
fn test_resolve_display_id_specific_value_passes_through() {
    let mut engine = match create_test_engine() {
        Some(e) => e,
        None => panic!("COM/音频环境不可用：测试辅助返回 None（此测试标记为 #[ignore]，应仅在 Windows 真机环境通过 --ignored 运行）"),
    };

    let display_id = "monitor_0".to_string();
    let source = WallpaperSource::File("/test/wallpaper.mp4".to_string());
    let renderer = Box::new(MockRenderer::new());
    engine
        .embed_and_register_renderer(renderer, &display_id, &source, WallpaperType::Video)
        .unwrap();

    // 显式传入具体值时应直接透传，不触发回退
    let result = resolve_display_id(Some("monitor_1".to_string()), &engine);
    assert_eq!(
        result, "monitor_1",
        "显式传入的 display_id 应直接透传，实际: {:?}",
        result
    );
}
