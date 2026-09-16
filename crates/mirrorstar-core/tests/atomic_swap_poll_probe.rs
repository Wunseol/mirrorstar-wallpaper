//! 原子交换轮询探测测试
//!
//! 复现 DR-33 原子交换阶段 B（wait_new_ready）的轮询路径：
//! play() → after_embed()（loadfile）→ 反复 poll_first_frame_ready()，
//! 并详细打印每次 diagnostic_playback_status 的原始响应，定位
//! "视频已播放但 width>0 && idle-active=no 永不成立" 的根因。
//!
//! 运行方式：
//! ```bash
//! cargo test --test atomic_swap_poll_probe -- --ignored --nocapture
//! ```

#![cfg(windows)]

use mirrorstar_core::wallpaper::video::{video_first_frame_ready, VideoRenderer};
use mirrorstar_core::wallpaper::{ScalingMode, WallpaperRenderer};
use std::sync::{Arc, Mutex};

#[test]
#[ignore = "诊断测试：需要 mpv 与真实视频文件，手动运行"]
fn atomic_swap_poll_probe() {
    let _guard = mirrorstar_core::init_logging();

    let video_path =
        "C:\\Users\\w\\AppData\\Roaming\\mirrorstar\\wallpapers\\0449333d-81e5-4828-865e-11ef62f6bdba\\【哲风壁纸】光影-动漫美女-彩色.mp4";
    assert!(std::path::Path::new(video_path).exists(), "视频不存在: {video_path}");

    let volume_control = match mirrorstar_core::VolumeControl::new() {
        Ok(vc) => Some(Arc::new(Mutex::new(vc))),
        Err(e) => {
            println!("VolumeControl 初始化失败（降级）: {e}");
            None
        }
    };

    let mut renderer = VideoRenderer::new(video_path.to_string(), ScalingMode::Fit, volume_control);
    println!("=== 1. play() ===");
    renderer.play().expect("play 失败");
    println!("=== 2. after_embed() [loadfile] ===");
    renderer.after_embed().expect("after_embed 失败");

    println!("=== 3. 轮询 poll_first_frame_ready（最多 50 次 x 100ms）===");
    let mut ready = false;
    for i in 0..50u32 {
        let status = renderer.diagnostic_playback_status();
        match status {
            Ok(v) => {
                let ok = video_first_frame_ready(&v);
                println!(
                    "poll[{i}] ready={ok} status={}",
                    serde_json::to_string(&v).unwrap_or_default()
                );
                if ok {
                    ready = true;
                    break;
                }
            }
            Err(e) => {
                println!("poll[{i}] ERR: {e:?}");
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!("=== 4. 结果: ready={ready} ===");
    let _ = renderer.terminate();
    println!("=== 完成 ===");
}
