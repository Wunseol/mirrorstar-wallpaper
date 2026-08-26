//! 视频壁纸 mpv 启动诊断测试
//!
//! 直接调用 VideoRenderer::play() 复现 IPC 连接失败问题，收集诊断日志。
//! 运行方式：
//! ```bash
//! $env:RUST_LOG="info,mirrorstar_core=debug"
//! cargo test --test video_mpv_diagnostic -- --ignored --nocapture
//! ```

#![cfg(windows)]

use mirrorstar_core::wallpaper::video::VideoRenderer;
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperRenderer};
use std::sync::{Arc, Mutex};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

#[test]
#[ignore = "诊断测试：需要 mpv 与真实视频文件，手动运行"]
fn video_renderer_play_diagnostic() {
    // 初始化 COM（MTA 模式）
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    // 初始化日志（输出到 stdout）
    let _guard = mirrorstar_core::init_logging();

    // 使用 wallpaper 目录下的视频文件
    let video_path =
        "c:\\Dev\\DevWorkspace\\mirrorstar-wallpaper\\wallpaper\\【哲风壁纸】光影-动漫美女-彩色.mp4";
    println!("测试视频文件: {}", video_path);
    assert!(
        std::path::Path::new(video_path).exists(),
        "视频文件不存在: {}",
        video_path
    );

    // 创建 VolumeControl（可能失败，降级为 None）
    let volume_control = match mirrorstar_core::VolumeControl::new() {
        Ok(vc) => Some(Arc::new(Mutex::new(vc))),
        Err(e) => {
            println!("VolumeControl 初始化失败（无音频设备？）: {}", e);
            None
        }
    };

    // 创建 VideoRenderer
    let mut renderer = VideoRenderer::new(video_path.to_string(), ScalingMode::Fit, volume_control);

    // 调用 play()，复现 IPC 连接失败
    println!("调用 VideoRenderer::play()...");
    let result = renderer.play();
    println!("play() 返回: {:?}", result);

    // 等待几秒观察 mpv 是否持续运行
    if result.is_ok() {
        println!("play() 成功，等待 3 秒观察 mpv 是否持续运行...");
        std::thread::sleep(std::time::Duration::from_secs(3));

        // 检查渲染器状态
        let state = renderer.state();
        println!("3 秒后渲染器状态: {:?}", state);
    }

    // 终止渲染器
    println!("调用 terminate()...");
    let _ = renderer.terminate();
    println!("测试完成");

    unsafe { CoUninitialize() }
}
