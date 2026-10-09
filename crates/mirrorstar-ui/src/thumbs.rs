//! 缩略图直读与限幅解码（SubTask 4.3 / 4.4）。
//!
//! ## 为什么按本地文件路径直读，而不用 `wpfile://`
//!
//! `WallpaperEntry::thumbnail` 是**磁盘上生成好的缩略图绝对路径**（由 core 的
//! `generate_thumbnail` / `generate_video_thumbnail` 产出，320×180 JPEG；生成失败时命令层
//! 会回退为**源文件路径**）。它是普通文件路径，不是 data URI。旧 WebView2 前端经
//! `convertFileSrc(path, "wpfile")` 走自定义协议读取——那是 WebView2 的能力，原生进程
//! 不具备。本进程直接用文件路径读取即可，**协议里不传缩略图字节**。
//!
//! ## 为什么必须限幅解码
//!
//! 缩略图本身仅 320×180（解码后约 172KB RGB）；但"缩略图为空时回退源图直载"会命中完整
//! 分辨率源图（4K 整图 ≈ 33MB RGBA）。若整图解码后常驻，可见窗口几十张即爆内存。因此
//! 统一经 [`decode_capped`] 解码并缩放到 [`MAX_DECODE_W`]×[`MAX_DECODE_H`] 以内，
//! 保证单张常驻成本有界、与源图分辨率无关。

use std::path::Path;

use image::{GenericImageView, ImageReader};
use mirrorstar_core::config::WallpaperType;
use slint::{Image, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};

/// 解码后的最大宽度（与 core 生成缩略图的 320×180 一致）。
pub const MAX_DECODE_W: u32 = 320;
/// 解码后的最大高度。
pub const MAX_DECODE_H: u32 = 180;

/// 单边最大像素数（防解压炸弹，与 core 的 `MAX_IMAGE_DIMENSION` 一致）。
const MAX_IMAGE_DIMENSION: u32 = 20_000;
/// 解码期最大分配内存（200MB，与 core 的 `MAX_DECODED_PIXEL_BUFFER_SIZE` 一致）。
const MAX_DECODE_ALLOC: u64 = 200 * 1024 * 1024;

/// 缩略图来源决策（在解码前确定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbPlan {
    /// 解码该本地文件路径。
    Path(String),
    /// Web 类型专属占位（无源文件、不参与轮换）。
    Web,
    /// 无可用图像（如视频无缩略图），改显示类型图标占位。
    TypeIcon,
}

/// 依据类型与 `thumbnail` / `file_path` 决策缩略图来源。
///
/// 语义与旧前端对齐（`src/scripts/ui/wallpaper-list.ts`）：
/// - Web：专属占位；
/// - 有缩略图：直接用缩略图路径；
/// - 无缩略图且 Image/Gif：回退源图直载；
/// - 无缩略图且 Video：类型图标占位作为兜底。调用方（`main.rs` 的 `card_state`）会先用
///   `mirrorstar_core::config::thumbnail_cache_path` 探测确定性磁盘缓存，命中则直接显示首帧，
///   仅未命中时才落到本函数返回的 [`ThumbPlan::TypeIcon`]。
pub fn plan(wallpaper_type: WallpaperType, thumbnail: &str, file_path: &str) -> ThumbPlan {
    if wallpaper_type == WallpaperType::Web {
        return ThumbPlan::Web;
    }
    if !thumbnail.is_empty() {
        return ThumbPlan::Path(thumbnail.to_string());
    }
    match wallpaper_type {
        WallpaperType::Image | WallpaperType::Gif if !file_path.is_empty() => {
            ThumbPlan::Path(file_path.to_string())
        }
        _ => ThumbPlan::TypeIcon,
    }
}

/// 等比缩放到不超出 `(max_w, max_h)`；小于目标时不放大。
///
/// 返回至少 1×1，避免零尺寸缓冲区。纯函数，便于单测覆盖边界。
pub fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let dw = ((w as f64 * scale).round() as u32).max(1);
    let dh = ((h as f64 * scale).round() as u32).max(1);
    (dw, dh)
}

/// 按路径解码图像并限幅到 [`MAX_DECODE_W`]×[`MAX_DECODE_H`]，产出可直接喂给 Slint 的
/// [`Image`]（RGB8）。
///
/// 失败（文件缺失 / 损坏 / 非图片 / 超限）返回可读错误字符串；调用方据此切到"异常态"占位，
/// **不 panic、不中断网格**。
pub fn decode_capped(path: &str) -> Result<Image, String> {
    decode_capped_to(path, MAX_DECODE_W, MAX_DECODE_H)
}

/// 同 [`decode_capped`]，但由调用方指定限幅尺寸（Task 8 预览需要比缩略图更高的上限）。
///
/// 限幅保证单张常驻解码成本有界、与源图分辨率无关；防解压炸弹的上限
/// （[`MAX_IMAGE_DIMENSION`] / [`MAX_DECODE_ALLOC`]）两条路径共用。
pub fn decode_capped_to(path: &str, max_w: u32, max_h: u32) -> Result<Image, String> {
    let img = decode_with_limits(path)?;

    let (w, h) = img.dimensions();
    let (tw, th) = fit_within(w, h, max_w, max_h);
    if tw == 0 || th == 0 {
        return Err(format!("图像尺寸非法: {w}x{h}"));
    }
    let rgb = if (tw, th) == (w, h) {
        img.to_rgb8()
    } else {
        img.resize(tw, th, image::imageops::FilterType::Triangle)
            .to_rgb8()
    };
    let (rw, rh) = rgb.dimensions();
    let buffer = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(rgb.as_raw(), rw, rh);
    Ok(Image::from_rgb8(buffer))
}

/// 以共用防解压炸弹上限打开并解码本地文件。
///
/// [`decode_capped_to`] 与 [`ghost_from_path`] 共用，两条路径命中相同的
/// [`MAX_IMAGE_DIMENSION`] / [`MAX_DECODE_ALLOC`] 上限。
fn decode_with_limits(path: &str) -> Result<image::DynamicImage, String> {
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    // 内容嗅探优先、扩展名兜底：`ImageReader::open` 默认按扩展名推断格式，misnamed
    // 文件（如 JPEG 内容却命名 .png）会选错解码器而失败；`with_guessed_format` 先按
    // 文件魔数嗅探，无法识别时才回退扩展名。此函数是缩略图/拖影/预览共用的唯一解码
    // 入口，改这里即一并修复。
    let mut reader = ImageReader::open(Path::new(path))
        .map_err(|e| format!("打开失败: {e}"))?
        .with_guessed_format()
        .map_err(|e| format!("打开失败: {e}"))?;
    reader.limits(limits);
    reader.decode().map_err(|e| format!("解码失败: {e}"))
}

/// 由已解码图像生成卡片式拖影：cover 裁切到 `(w, h)`，烘焙圆角 alpha 与半透明。
///
/// - `w` / `h` 至少为 1；`radius` 夹取到 `min(radius, min(w,h)/2)`；`opacity` 夹取到 0..=1。
/// - 圆角处按 `radius - dist + 0.5` 得到 0..=1 的覆盖率做 1px 抗锯齿。
/// - 所有像素 alpha 再乘以 `opacity`；RGB 保持不变。
///
/// 纯函数（返回 `image::RgbaImage`），便于单测直接检查像素。
pub(crate) fn compose_ghost(
    img: &image::DynamicImage,
    w: u32,
    h: u32,
    radius: u32,
    opacity: f32,
) -> image::RgbaImage {
    let w = w.max(1);
    let h = h.max(1);
    let radius = radius.min(w.min(h) / 2);
    let opacity = opacity.clamp(0.0, 1.0);

    // cover 裁切 + 缩放到精确的 w×h。
    let mut rgba = img
        .clone()
        .resize_to_fill(w, h, image::imageops::FilterType::Triangle)
        .to_rgba8();

    let r = radius as f32;
    for y in 0..h {
        for x in 0..w {
            let mut coverage = 1.0f32;
            if radius > 0 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let cx = if px < r {
                    Some(r)
                } else if px > w as f32 - r {
                    Some(w as f32 - r)
                } else {
                    None
                };
                let cy = if py < r {
                    Some(r)
                } else if py > h as f32 - r {
                    Some(h as f32 - r)
                } else {
                    None
                };
                if let (Some(cx), Some(cy)) = (cx, cy) {
                    let dx = px - cx;
                    let dy = py - cy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    coverage = (r - dist + 0.5).clamp(0.0, 1.0);
                }
            }
            let base_alpha = rgba.get_pixel(x, y)[3] as f32;
            let alpha = (base_alpha * coverage * opacity).round().clamp(0.0, 255.0) as u8;
            rgba.get_pixel_mut(x, y)[3] = alpha;
        }
    }
    rgba
}

/// 从本地文件路径生成卡片拖影位图（失败返回可读错误）。
///
/// 以与 [`decode_capped_to`] 相同的防解压炸弹上限解码，再经 [`compose_ghost`] 烘焙圆角与半透明，
/// 产出可直接喂给 `DragArea.drag-image` 的 RGBA8 [`Image`]。
pub fn ghost_from_path(
    path: &str,
    w: u32,
    h: u32,
    radius: u32,
    opacity: f32,
) -> Result<Image, String> {
    let img = decode_with_limits(path)?;
    let rgba = compose_ghost(&img, w, h, radius, opacity);
    let (rw, rh) = rgba.dimensions();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rw, rh);
    Ok(Image::from_rgba8(buffer))
}

/// 判断本地文件是否存在（供"源文件缺失"判定）。
pub fn path_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_does_not_upscale() {
        assert_eq!(fit_within(100, 50, 320, 180), (100, 50));
        assert_eq!(fit_within(320, 180, 320, 180), (320, 180));
    }

    #[test]
    fn fit_within_preserves_aspect_ratio_16_9() {
        assert_eq!(fit_within(1920, 1080, 320, 180), (320, 180));
        assert_eq!(fit_within(3840, 2160, 320, 180), (320, 180));
    }

    #[test]
    fn fit_within_square_and_wide() {
        assert_eq!(fit_within(800, 800, 320, 180), (180, 180));
        assert_eq!(fit_within(640, 180, 320, 180), (320, 90));
    }

    #[test]
    fn fit_within_zero_dimensions_is_safe() {
        assert_eq!(fit_within(0, 100, 320, 180), (0, 0));
        assert_eq!(fit_within(100, 0, 320, 180), (0, 0));
    }

    #[test]
    fn plan_prefers_thumbnail_when_present() {
        let p = plan(WallpaperType::Image, "C:/t/thumb_1.jpg", "C:/src/a.png");
        assert_eq!(p, ThumbPlan::Path("C:/t/thumb_1.jpg".to_string()));
    }

    #[test]
    fn plan_falls_back_to_source_for_image_and_gif_without_thumbnail() {
        assert_eq!(
            plan(WallpaperType::Image, "", "C:/src/a.png"),
            ThumbPlan::Path("C:/src/a.png".to_string())
        );
        assert_eq!(
            plan(WallpaperType::Gif, "", "C:/src/a.gif"),
            ThumbPlan::Path("C:/src/a.gif".to_string())
        );
    }

    #[test]
    fn plan_uses_type_icon_for_video_without_thumbnail() {
        assert_eq!(
            plan(WallpaperType::Video, "", "C:/src/a.mp4"),
            ThumbPlan::TypeIcon
        );
    }

    #[test]
    fn plan_uses_web_placeholder_regardless_of_paths() {
        assert_eq!(
            plan(WallpaperType::Web, "", "https://example.com"),
            ThumbPlan::Web
        );
        assert_eq!(plan(WallpaperType::Web, "C:/t/x.jpg", ""), ThumbPlan::Web);
    }

    #[test]
    fn decode_capped_downscales_large_source() {
        let dir =
            std::env::temp_dir().join(format!("mirrorstar_ui_thumb_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("big.png");
        let img = image::DynamicImage::new_rgb8(1920, 1080);
        img.save_with_format(&src, image::ImageFormat::Png).unwrap();

        let decoded = decode_capped(src.to_str().unwrap()).expect("应可解码大图");
        assert_eq!(decoded.size().width, MAX_DECODE_W);
        assert_eq!(decoded.size().height, MAX_DECODE_H);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_capped_errors_on_missing_file() {
        let err = decode_capped("C:/definitely/not/here.png").unwrap_err();
        assert!(err.contains("打开失败"), "实际错误: {err}");
    }

    #[test]
    fn decode_capped_reads_ico() {
        use image::ImageEncoder;

        let dir =
            std::env::temp_dir().join(format!("mirrorstar_ui_ico_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.ico");

        // core 的 IMAGE_EXTS 含 "ico"，此处验证启用 ico 特性后确实可编码并解码。
        // image 的 ICO 解码器要求 PNG 压缩条目为 RGBA，故以 RGBA 编码。
        let img = image::DynamicImage::new_rgba8(16, 16);
        let file = std::fs::File::create(&path).unwrap();
        image::codecs::ico::IcoEncoder::new(file)
            .write_image(img.as_bytes(), 16, 16, image::ExtendedColorType::Rgba8)
            .expect("应可编码 ico");

        let decoded = decode_capped(path.to_str().unwrap()).expect("ico 应可解码");
        assert_eq!(decoded.size().width, 16);
        assert_eq!(decoded.size().height, 16);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_capped_reads_jpeg_content_named_png() {
        // 回归：misnamed 文件——内容为合法 JPEG，扩展名却是 .png。修复前按扩展名
        // 选 PNG 解码器会失败；修复后内容嗅探优先识别为 JPEG，正常解码并限幅。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_ui_misnamed_jpeg_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actually_jpeg.png");
        let img = image::DynamicImage::new_rgb8(640, 360);
        img.save_with_format(&path, image::ImageFormat::Jpeg)
            .unwrap();

        let decoded =
            decode_capped(path.to_str().unwrap()).expect("JPEG 内容名为 .png 的文件应可解码");
        assert!(
            decoded.size().width <= MAX_DECODE_W && decoded.size().height <= MAX_DECODE_H,
            "限幅后尺寸应在 {}x{} 内，实际 {}x{}",
            MAX_DECODE_W,
            MAX_DECODE_H,
            decoded.size().width,
            decoded.size().height
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_exists_false_for_empty_and_missing() {
        assert!(!path_exists(""));
        assert!(!path_exists("C:/definitely/not/here.png"));
    }

    /// 构造纯色 RGBA 源图（尺寸 ≥ 目标，保证 cover 有内容）。
    fn solid_source(w: u32, h: u32, rgba: [u8; 4]) -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(w, h, image::Rgba(rgba)))
    }

    #[test]
    fn compose_ghost_output_size_is_exact() {
        let src = solid_source(400, 400, [10, 20, 30, 255]);
        let ghost = compose_ghost(&src, 160, 94, 8, 0.85);
        assert_eq!(ghost.dimensions(), (160, 94));
    }

    #[test]
    fn compose_ghost_corners_are_transparent() {
        let src = solid_source(400, 400, [10, 20, 30, 255]);
        let ghost = compose_ghost(&src, 160, 94, 8, 0.85);
        for (x, y) in [(0u32, 0u32), (159, 0), (0, 93), (159, 93)] {
            assert_eq!(ghost.get_pixel(x, y)[3], 0, "角点 ({x},{y}) 应完全透明");
        }
    }

    #[test]
    fn compose_ghost_center_opacity_and_rgb() {
        let src = solid_source(400, 400, [10, 20, 30, 255]);
        let ghost = compose_ghost(&src, 160, 94, 8, 0.85);
        let center = ghost.get_pixel(80, 47);
        let expected = (0.85f32 * 255.0).round() as i32;
        assert!(
            (center[3] as i32 - expected).abs() <= 3,
            "中心 alpha 应≈{expected}，实际 {}",
            center[3]
        );
        assert_eq!(&center.0[0..3], &[10, 20, 30]);
    }

    #[test]
    fn compose_ghost_zero_opacity_is_fully_transparent() {
        let src = solid_source(200, 200, [10, 20, 30, 255]);
        let ghost = compose_ghost(&src, 160, 94, 8, 0.0);
        assert!(ghost.pixels().all(|p| p[3] == 0));
    }

    #[test]
    fn compose_ghost_zero_radius_keeps_corners_opaque() {
        let src = solid_source(200, 200, [10, 20, 30, 255]);
        let ghost = compose_ghost(&src, 160, 94, 0, 0.85);
        let expected = (0.85f32 * 255.0).round() as u8;
        assert_eq!(ghost.get_pixel(0, 0)[3], expected);
        assert_eq!(ghost.get_pixel(159, 93)[3], expected);
    }
}
