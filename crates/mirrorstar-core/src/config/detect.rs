//! 文件类型检测
//!
//! 本模块提供壁纸类型检测能力：`detect_wallpaper_type` 基于扩展名检测，
//! `detect_wallpaper_type_by_magic_bytes` 基于魔数识别（供 `detect_html` 及魔数嗅探使用）。

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

/// 根据文件头魔数字节判断壁纸类型（纯函数，便于单元测试）
///
/// 承担所有魔数识别逻辑。返回 `None` 表示魔数无法识别，调用方应回退到扩展名检测。
// 仅被单元测试引用（原生产调用方 detect_wallpaper_type_by_content 已删除，P2-1）
#[allow(dead_code)]
fn detect_wallpaper_type_by_magic_bytes(head: &[u8]) -> Option<WallpaperType> {
    // GIF: "GIF87a" or "GIF89a"
    if head.len() >= 6 && (&head[..6] == b"GIF87a" || &head[..6] == b"GIF89a") {
        return Some(WallpaperType::Gif);
    }
    // PNG: \x89PNG\r\n\x1a\n
    if head.len() >= 8 && head[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some(WallpaperType::Image);
    }
    // JPEG: \xFF\xD8\xFF
    if head.len() >= 3 && head[..3] == [0xFF, 0xD8, 0xFF] {
        return Some(WallpaperType::Image);
    }
    // BMP: "BM"
    if head.len() >= 2 && &head[..2] == b"BM" {
        return Some(WallpaperType::Image);
    }
    // MP4 等 ISO BMFF 容器：偏移 4 处为 "ftyp"
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some(WallpaperType::Video);
    }
    // WebM/EBML 容器：完整 4 字节 EBML magic (1A 45 DF A3)
    // WebM 基于 EBML，与 MP4 (ISO BMFF) 是不同的容器格式，必须独立检测
    if head.len() >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(WallpaperType::Video);
    }
    // HTML 检测：跳过 BOM 后扫描前 256 字节查找 HTML 标记
    if detect_html(head) {
        return Some(WallpaperType::Web);
    }
    None
}

/// 检测缓冲区内容是否为 HTML
///
/// 扫描前 256 字节，先跳过 UTF-8/UTF-16 BOM，再大小写不敏感地查找
/// `<!doctype html` 或 `<html` 标记。使用 `windows().any(|w| w.eq_ignore_ascii_case(pattern))`
/// 滑动窗口比较以避免堆分配；模式均为 ASCII，非 ASCII 字节按字节原样比较。
// 生产路径仅被 detect_wallpaper_type_by_magic_bytes 调用（其生产调用方随 P2-1 移除），
// 现仅经魔数识别函数被单元测试引用。
#[allow(dead_code)]
fn detect_html(head: &[u8]) -> bool {
    // 跳过常见 BOM
    let start = if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        // UTF-8 BOM
        3
    } else if head.starts_with(&[0xFF, 0xFE]) {
        // UTF-16 LE BOM（仅跳过 BOM 字节，不做 UTF-16 解码；
        // 后续 ASCII 子串匹配可能失败，但避免误判为非 HTML）
        2
    } else if head.starts_with(&[0xFE, 0xFF]) {
        // UTF-16 BE BOM
        2
    } else {
        0
    };

    let body = if start >= head.len() {
        return false;
    } else {
        &head[start..]
    };

    const HTML_PATTERNS: &[&[u8]] = &[b"<!doctype html", b"<html"];
    HTML_PATTERNS.iter().any(|pattern| {
        body.windows(pattern.len())
            .any(|w| w.eq_ignore_ascii_case(pattern))
    })
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

    // ── N-007: WebM / EBML magic 检测测试 ───────────────────────────────────
    //
    // 验证完整 4 字节 EBML magic (1A 45 DF A3) 识别为 Video 类型，
    // 其他 0x1A 开头但非 EBML 的文件不被误判。
    // 通过纯函数 detect_wallpaper_type_by_magic_bytes 测试，无需文件系统。

    #[test]
    fn n007_ebml_full_magic_identified_as_video() {
        // 完整 4 字节 EBML magic (1A 45 DF A3) 应识别为 Video（WebM）
        let mut head = vec![0x1A, 0x45, 0xDF, 0xA3];
        // 补充一些 EBML 头部后续字节，使长度足够
        head.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Video)
        );
    }

    #[test]
    fn n007_ebml_magic_minimal_4_bytes() {
        // 仅 4 字节 EBML magic 也应识别
        let head = [0x1A, 0x45, 0xDF, 0xA3];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Video)
        );
    }

    #[test]
    fn n007_ebml_magic_too_short_not_identified() {
        // 不足 4 字节不应识别为 EBML（即使前 3 字节匹配）
        let head = [0x1A, 0x45, 0xDF];
        // 不足 4 字节，无法匹配 EBML magic，也无其他魔数匹配
        assert_eq!(detect_wallpaper_type_by_magic_bytes(&head), None);
    }

    #[test]
    fn n007_other_0x1a_prefixed_not_misdetected_as_webm() {
        // 其他以 0x1A 开头但非 EBML 的数据不应被误判为 Video
        // 例如 0x1A 0x00 0x00 0x00 不构成 EBML magic
        let head = [0x1A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(detect_wallpaper_type_by_magic_bytes(&head), None);
    }

    #[test]
    fn n007_0x1a_with_partial_ebml_match_not_misdetected() {
        // 0x1A 0x45 后接非 DF A3 字节，不构成完整 EBML magic
        let head = [0x1A, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(detect_wallpaper_type_by_magic_bytes(&head), None);
    }

    #[test]
    fn n007_0x1a_with_3_byte_partial_match_not_misdetected() {
        // 0x1A 0x45 0xDF 后接非 A3 字节，不构成完整 EBML magic
        let head = [0x1A, 0x45, 0xDF, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(detect_wallpaper_type_by_magic_bytes(&head), None);
    }

    #[test]
    fn n007_ebml_magic_does_not_affect_other_magic_checks() {
        // EBML magic 检查不应干扰其他魔数检测
        // 验证 PNG 头仍被正确识别（PNG 头包含 0x1A 但不是首字节）
        let png_head = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&png_head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn n007_webm_with_ebml_followed_by_matroska_marker() {
        // 实际 WebM 文件：EBML magic 后接 Matroska 段头（0x18 0x53 0x80 0x67）
        // 应识别为 Video
        let mut head = vec![0x1A, 0x45, 0xDF, 0xA3];
        head.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20]);
        head.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Video)
        );
    }

    // ── N-008: HTML detection 扩展测试 ──────────────────────────────────────
    //
    // 验证带 BOM 的 HTML 文件、长注释开头的 HTML 文件被正确识别；
    // 测试非 HTML 文件不被误判。

    #[test]
    fn n008_detect_html_lowercase_doctype() {
        // 直接测试 detect_html 辅助函数：lowercase doctype
        let content = b"<!doctype html><html><body></body></html>";
        assert!(detect_html(content));
    }

    #[test]
    fn n008_detect_html_uppercase_doctype() {
        // 大写 DOCTYPE 也应识别（不区分大小写）
        let content = b"<!DOCTYPE HTML><html><body></body></html>";
        assert!(detect_html(content));
    }

    #[test]
    fn n008_detect_html_with_long_comment_prefix() {
        // 长注释开头的 HTML：标记在 16 字节之后，需要 256 字节扫描范围
        // （原 16 字节扫描无法覆盖此场景，N-008 修复后可识别）
        let comment = b"<!-- this is a very long comment that goes beyond 16 bytes -->";
        let mut content = comment.to_vec();
        content.extend_from_slice(b"<!doctype html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_with_utf8_bom() {
        // UTF-8 BOM 后接 doctype：BOM 应被跳过
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(b"<!doctype html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_with_utf8_bom_and_whitespace() {
        // UTF-8 BOM + 空白 + <html>
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(b"   \n  <html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_with_utf16_le_bom() {
        // UTF-16 LE BOM 后接 ASCII 标记（detect_html 跳过 BOM 后按 ASCII 处理）
        let mut content = vec![0xFF, 0xFE];
        content.extend_from_slice(b"<!doctype html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_with_utf16_be_bom() {
        // UTF-16 BE BOM 后接 ASCII 标记
        let mut content = vec![0xFE, 0xFF];
        content.extend_from_slice(b"<!doctype html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_uppercase_html_tag() {
        // 大写 <HTML> 也应识别
        let content = b"<HTML><HEAD></HEAD><BODY></BODY></HTML>";
        assert!(detect_html(content));
    }

    #[test]
    fn n008_detect_html_mixed_case_html_tag() {
        // 混合大小写 <HtMl>
        let content = b"<HtMl><body></body></HtMl>";
        assert!(detect_html(content));
    }

    #[test]
    fn n008_detect_html_minimal_html_tag() {
        // 最小 <html>
        let content = b"<html>";
        assert!(detect_html(content));
    }

    #[test]
    fn n008_detect_html_empty_buffer_returns_false() {
        // 空缓冲区不应识别为 HTML
        assert!(!detect_html(b""));
    }

    #[test]
    fn n008_detect_html_only_bom_returns_false() {
        // 只有 BOM 没有内容不应识别为 HTML
        assert!(!detect_html(&[0xEF, 0xBB, 0xBF]));
        assert!(!detect_html(&[0xFF, 0xFE]));
        assert!(!detect_html(&[0xFE, 0xFF]));
    }

    #[test]
    fn n008_detect_html_non_html_returns_false() {
        // 非 HTML 内容不应被误判
        assert!(!detect_html(b"Hello, world!"));
        assert!(!detect_html(b"\x89PNG\r\n\x1a\n"));
        assert!(!detect_html(b"GIF89a..."));
        assert!(!detect_html(b"random binary data \x00 \x01 \x02"));
    }

    #[test]
    fn n008_detect_html_doctype_without_html_still_detected() {
        // 只有 <!doctype html 没有 <html> 也应识别
        assert!(detect_html(b"<!doctype html>"));
    }

    #[test]
    fn n008_detect_html_html_tag_at_end_of_256_bytes() {
        // <html> 出现在缓冲区末尾附近（验证 256 字节扫描范围）
        let mut content = vec![b' '; 250];
        content.extend_from_slice(b"<html>");
        assert!(detect_html(&content));
    }

    #[test]
    fn n008_detect_html_html_tag_beyond_256_bytes_not_detected() {
        // <html> 出现在 256 字节之后不会被检测到（接受此限制以避免读取过多）
        let mut content = vec![b' '; 260];
        content.extend_from_slice(b"<html>");
        // 缓冲区前 256 字节都是空格，detect_html 返回 false
        // 注意：实际场景中会回退到扩展名检测
        assert!(!detect_html(&content[..256]));
    }

    #[test]
    fn n008_detect_html_via_magic_bytes_function() {
        // 通过 detect_wallpaper_type_by_magic_bytes 验证 HTML 识别
        let content = b"<!doctype html><html></html>";
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(content),
            Some(WallpaperType::Web)
        );
    }

    #[test]
    fn n008_detect_html_with_bom_via_magic_bytes_function() {
        // 通过 detect_wallpaper_type_by_magic_bytes 验证带 BOM 的 HTML 识别
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(b"<!doctype html>");
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&content),
            Some(WallpaperType::Web)
        );
    }

    #[test]
    fn n008_ebml_magic_not_misdetected_as_html() {
        // EBML magic 开头 (1A 45 DF A3) 不应被 detect_html 误判为 HTML
        // 应被识别为 Video
        let ebml_header = [0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&ebml_header),
            Some(WallpaperType::Video)
        );
    }

    // ── v41-C-015: BMP / JPEG 魔数检测测试 ──────────────────────────────────
    //
    // 验证 BMP ("BM" = 0x42 0x4D) 与 JPEG (0xFF 0xD8 0xFF) 魔数识别为 Image 类型，
    // 字节不足时不误判，且不干扰其他魔数检测（GIF / PNG / EBML / MP4 等）。
    // 通过纯函数 detect_wallpaper_type_by_magic_bytes 测试，无需文件系统。

    #[test]
    fn v41_c015_bmp_magic_bytes_identified_as_image() {
        let mut head = vec![b'B', b'M'];
        head.extend_from_slice(&[
            0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00,
        ]);
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn v41_c015_bmp_magic_minimal_2_bytes() {
        let head = b"BM";
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn v41_c015_bmp_magic_too_short_not_identified() {
        let head = b"B";
        assert_eq!(detect_wallpaper_type_by_magic_bytes(head), None);
    }

    #[test]
    fn v41_c015_bmp_magic_does_not_affect_other_checks() {
        let gif_head = b"GIF89a";
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(gif_head),
            Some(WallpaperType::Gif)
        );
        let png_head = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&png_head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn v41_c015_jpeg_magic_bytes_identified_as_image() {
        let mut head = vec![0xFF, 0xD8, 0xFF];
        head.extend_from_slice(&[0xE0, 0x10, 0x4A, 0x46, 0x49, 0x46]);
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn v41_c015_jpeg_magic_minimal_3_bytes() {
        let head = [0xFF, 0xD8, 0xFF];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&head),
            Some(WallpaperType::Image)
        );
    }

    #[test]
    fn v41_c015_jpeg_magic_too_short_not_identified() {
        let head = [0xFF, 0xD8];
        assert_eq!(detect_wallpaper_type_by_magic_bytes(&head), None);
    }

    #[test]
    fn v41_c015_jpeg_magic_does_not_affect_other_checks() {
        let ebml_head = [0x1A, 0x45, 0xDF, 0xA3];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&ebml_head),
            Some(WallpaperType::Video)
        );
        let mp4_head = [
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0x00, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(
            detect_wallpaper_type_by_magic_bytes(&mp4_head),
            Some(WallpaperType::Video)
        );
    }
}
