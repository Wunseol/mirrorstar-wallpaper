//! 文件类型检测
//!
//! 本模块提供壁纸类型检测能力：`detect_wallpaper_type` 基于扩展名检测。

use std::path::Path;

use crate::wallpaper::WallpaperType;

/// 视频扩展名
const VIDEO_EXTS: &[&str] = &[
    "mp4", "avi", "mkv", "mov", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "ts",
];

/// 图片扩展名
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tiff", "tif", "ico"];

/// 网页扩展名
const WEB_EXTS: &[&str] = &["html", "htm"];

/// 根据文件扩展名检测壁纸类型
///
/// 扩展名匹配不区分大小写。无扩展名或不支持的扩展名返回 `None`。
///
/// # 示例
///
/// ```
/// use mirrorstar_core::config::detect_wallpaper_type;
/// use mirrorstar_core::WallpaperType;
///
/// assert_eq!(detect_wallpaper_type("/path/to/video.mp4"), Some(WallpaperType::Video));
/// assert_eq!(detect_wallpaper_type("/path/to/anim.gif"), Some(WallpaperType::Gif));
/// assert_eq!(detect_wallpaper_type("/path/to/image.jpg"), Some(WallpaperType::Image));
/// assert_eq!(detect_wallpaper_type("/path/to/page.html"), Some(WallpaperType::Web));
/// assert_eq!(detect_wallpaper_type("/path/to/doc.pdf"), None);
/// ```
pub fn detect_wallpaper_type(file_path: &str) -> Option<WallpaperType> {
    let ext = Path::new(file_path).extension()?.to_str()?.to_lowercase();

    if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(WallpaperType::Video)
    } else if ext == "gif" {
        Some(WallpaperType::Gif)
    } else if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(WallpaperType::Image)
    } else if WEB_EXTS.contains(&ext.as_str()) {
        Some(WallpaperType::Web)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_wallpaper_type：扩展名匹配 ────────────────────────────────────

    #[test]
    fn detect_video_extensions() {
        assert_eq!(
            detect_wallpaper_type("/test/video.mp4"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/test/video.avi"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/test/video.mkv"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/test/video.mov"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/test/video.webm"),
            Some(WallpaperType::Video)
        );
    }

    #[test]
    fn detect_gif_extension() {
        assert_eq!(
            detect_wallpaper_type("/test/anim.gif"),
            Some(WallpaperType::Gif)
        );
    }

    #[test]
    fn detect_image_extensions() {
        assert_eq!(
            detect_wallpaper_type("/test/image.jpg"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type("/test/image.jpeg"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type("/test/image.png"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type("/test/image.bmp"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type("/test/image.webp"),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn detect_web_extensions() {
        assert_eq!(
            detect_wallpaper_type("/test/page.html"),
            Some(WallpaperType::Web)
        );
        assert_eq!(
            detect_wallpaper_type("/test/page.htm"),
            Some(WallpaperType::Web)
        );
    }

    // ── detect_wallpaper_type：不支持/无扩展名 ───────────────────────────────

    #[test]
    fn detect_unsupported_extensions_return_none() {
        assert_eq!(detect_wallpaper_type("/test/doc.pdf"), None);
        assert_eq!(detect_wallpaper_type("/test/music.mp3"), None);
        assert_eq!(detect_wallpaper_type("/test/archive.zip"), None);
        assert_eq!(detect_wallpaper_type("/test/text.txt"), None);
    }

    #[test]
    fn detect_no_extension_returns_none() {
        assert_eq!(detect_wallpaper_type("/test/noext"), None);
        assert_eq!(detect_wallpaper_type("noext"), None);
        assert_eq!(detect_wallpaper_type("/test/"), None);
    }

    // ── detect_wallpaper_type：大小写不敏感 ──────────────────────────────────

    #[test]
    fn detect_extension_case_insensitive() {
        assert_eq!(
            detect_wallpaper_type("/test/VIDEO.MP4"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/test/ANIM.GIF"),
            Some(WallpaperType::Gif)
        );
        assert_eq!(
            detect_wallpaper_type("/test/IMAGE.JPG"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type("/test/PAGE.HTML"),
            Some(WallpaperType::Web)
        );
        assert_eq!(
            detect_wallpaper_type("/test/MixedCase.Mp4"),
            Some(WallpaperType::Video)
        );
    }

    // ── detect_wallpaper_type：路径处理 ──────────────────────────────────────

    #[test]
    fn detect_windows_path_with_backslashes() {
        assert_eq!(
            detect_wallpaper_type(r"C:\wallpapers\video.mp4"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type(r"C:\wallpapers\anim.gif"),
            Some(WallpaperType::Gif)
        );
    }

    #[test]
    fn detect_path_with_dots_in_directory() {
        assert_eq!(
            detect_wallpaper_type("/path/with.dots/video.mp4"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type("/path/v1.2/file.gif"),
            Some(WallpaperType::Gif)
        );
    }
}
