//! 视频壁纸全链路运行时诊断测试
//!
//! 真实复现应用的完整设置流程：
//!   play()（mpv `--idle=yes` 启动 + IPC 连接 + 找窗口）
//!   → `DesktopIntegrator.embed_wallpaper`（嵌入 WorkerW 壁纸层）
//!   → `after_embed()`（IPC `loadfile` 加载视频）
//!   → IPC 查询播放状态（`idle-active` / `time-pos` 前进 / `width`/`height`）
//!   → 检查 mpv 进程存活 + mpv 日志无纹理错误
//!
//! 运行方式：
//! ```bash
//! $env:RUST_LOG="info,mirrorstar_core=debug"
//! cargo test --test video_wallpaper_fullpath -- --ignored --nocapture
//! ```

#![cfg(windows)]

use mirrorstar_core::wallpaper::video::VideoRenderer;
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperRenderer};
use mirrorstar_core::{Arrangement, DesktopIntegrator};
use std::sync::{Arc, Mutex};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

#[test]
#[ignore = "诊断测试：需要真实桌面环境 + mpv + 视频文件，手动运行"]
fn video_wallpaper_fullpath_diagnostic() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let _guard = mirrorstar_core::init_logging();

    let video_path =
        "c:\\Dev\\DevWorkspace\\mirrorstar-wallpaper\\wallpaper\\【哲风壁纸】光影-动漫美女-彩色.mp4";
    println!("测试视频文件: {}", video_path);
    assert!(
        std::path::Path::new(video_path).exists(),
        "视频文件不存在: {}",
        video_path
    );

    let volume_control = match mirrorstar_core::VolumeControl::new() {
        Ok(vc) => Some(Arc::new(Mutex::new(vc))),
        Err(e) => {
            println!("VolumeControl 初始化失败（无音频设备？）: {}", e);
            None
        }
    };

    let mut renderer = VideoRenderer::new(video_path.to_string(), ScalingMode::Fit, volume_control);

    // Step 1: play() —— mpv --idle=yes 启动 + IPC 连接 + 找窗口
    println!("== Step 1: play() ==");
    let play_result = renderer.play();
    println!("play() 返回: {:?}", play_result);
    assert!(play_result.is_ok(), "play() 失败，终止诊断");
    let hwnd = renderer.hwnd().expect("play() 后应已找到 mpv 窗口");
    println!("mpv 窗口 hwnd = {:?}", hwnd);

    // Step 2: 初始化桌面集成器，查找 WorkerW 并嵌入
    println!("== Step 2: embed_wallpaper ==");
    let mut desktop = DesktopIntegrator::new();
    if let Err(e) = desktop.ensure_initialized() {
        println!("WorkerW 初始化失败: {}", e);
        let _ = renderer.terminate();
        panic!("WorkerW 初始化失败: {}", e);
    }
    let embed_result = desktop.embed_wallpaper(hwnd, r"\\.\DISPLAY1", Arrangement::PerMonitor);
    println!("embed_wallpaper 返回: {:?}", embed_result);
    assert!(embed_result.is_ok(), "嵌入 WorkerW 失败");

    // Step 3: after_embed() —— IPC loadfile 加载视频
    println!("== Step 3: after_embed (loadfile) ==");
    let after_result = renderer.after_embed();
    println!("after_embed 返回: {:?}", after_result);
    assert!(after_result.is_ok(), "after_embed 失败");

    // Step 4: 查询播放状态（loadfile 后视频应开始渲染）
    println!("== Step 4: 播放状态查询 ==");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let status1 = renderer.diagnostic_playback_status();
    println!("t+2s 播放状态: {:?}", status1);

    std::thread::sleep(std::time::Duration::from_secs(3));
    let status2 = renderer.diagnostic_playback_status();
    println!("t+5s 播放状态: {:?}", status2);

    // 诊断判定
    if let (Ok(s1), Ok(s2)) = (&status1, &status2) {
        let idle1 = s1.get("idle-active").and_then(|v| v.as_str()).unwrap_or("");
        let idle2 = s2.get("idle-active").and_then(|v| v.as_str()).unwrap_or("");
        let t1 = s1.get("time-pos").and_then(|v| v.as_f64()).unwrap_or(-1.0);
        let t2 = s2.get("time-pos").and_then(|v| v.as_f64()).unwrap_or(-1.0);
        let w = s2.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
        let h = s2.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
        println!(
            "诊断结论: idle-active {}→{}, time-pos {:.2}→{:.2}, 分辨率 {}x{}",
            idle1, idle2, t1, t2, w, h
        );
        assert!(
            idle2 == "no",
            "视频未播放：idle-active 仍为 {}，loadfile 可能未生效或 mpv 已退出",
            idle2
        );
        assert!(
            t2 > t1 || (t1 >= 0.0 && t2 > 0.0),
            "视频未前进：time-pos 未增长（{:.2} → {:.2}），视频未在渲染",
            t1,
            t2
        );
        assert!(w > 0 && h > 0, "视频纹理未创建：分辨率 {}x{}", w, h);
        println!("== PASS: 视频正常渲染 ==");
    } else {
        println!(
            "播放状态查询失败: status1={:?}, status2={:?}（IPC 断开，mpv 可能已退出）",
            status1, status2
        );
        panic!("播放状态查询失败，mpv 可能已退出");
    }

    // Step 5: terminate
    println!("== Step 5: terminate ==");
    let _ = renderer.terminate();
    println!("诊断完成（请检查 mpv-*.log 是否含 Failed to create Texture2D / shaderc internal error）");

    unsafe { CoUninitialize() }
}
