//! 文件类型检测
//!
//! 本模块提供壁纸类型检测能力：`detect_wallpaper_type` 基于扩展名，
//! `detect_wallpaper_type_by_content` 基于魔数。后者当前无调用方，
//! 保留为未来扩展（如 add_wallpaper 命令的魔数纠错 fallback）。

use std::io::Read;
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

/// 根据文件内容（魔数）检测壁纸类型
///
/// 读取文件头部字节，通过魔数判断类型。无法识别时回退到扩展名检测。
/// 主要用于扩展名缺失或被篡改的场景，作为 `detect_wallpaper_type` 的补充。
///
/// # 检测的魔数
///
/// - GIF：完整 6 字节魔数 `GIF87a` / `GIF89a`
/// - PNG：`\x89PNG\r\n\x1a\n`
/// - JPEG：`\xFF\xD8\xFF`
/// - BMP：`BM`
/// - MP4 等 ISO BMFF 容器：偏移 4 处为 `ftyp`
/// - WebM/EBML 容器：完整 4 字节 EBML magic `\x1A\x45\xDF\xA3`
/// - HTML：扫描前 256 字节查找 `<!doctype html` 或 `<html`，跳过 UTF-8/UTF-16 BOM
pub fn detect_wallpaper_type_by_content(file_path: &str) -> Option<WallpaperType> {
    // 256 字节缓冲区，覆盖 HTML 检测所需的 BOM 与长注释场景
    let mut buf = [0u8; 256];
    let n = match std::fs::File::open(file_path) {
        Ok(mut f) => match f.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %file_path,
                    "读取文件头部失败，回退到扩展名检测"
                );
                return detect_wallpaper_type(file_path);
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %file_path,
                "打开文件失败，回退到扩展名检测"
            );
            return detect_wallpaper_type(file_path);
        }
    };
    let head = &buf[..n];

    // 优先尝试魔数识别
    if let Some(t) = detect_wallpaper_type_by_magic_bytes(head) {
        return Some(t);
    }

    // 魔数无法识别，回退到扩展名检测
    detect_wallpaper_type(file_path)
}

/// 根据文件头魔数字节判断壁纸类型（纯函数，便于单元测试）
///
/// 此函数被 `detect_wallpaper_type_by_content` 调用，承担所有魔数识别逻辑。
/// 返回 `None` 表示魔数无法识别，调用方应回退到扩展名检测。
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

    // ── detect_wallpaper_type_by_content：回退行为 ───────────────────────────
    //
    // 无法读取文件时回退到扩展名检测

    #[test]
    fn detect_by_content_falls_back_to_extension_when_file_missing() {
        assert_eq!(
            detect_wallpaper_type_by_content("/nonexistent/path/video.mp4"),
            Some(WallpaperType::Video)
        );
        assert_eq!(
            detect_wallpaper_type_by_content("/nonexistent/path/anim.gif"),
            Some(WallpaperType::Gif)
        );
        assert_eq!(
            detect_wallpaper_type_by_content("/nonexistent/path/image.jpg"),
            Some(WallpaperType::Image)
        );
        assert_eq!(
            detect_wallpaper_type_by_content("/nonexistent/path/page.html"),
            Some(WallpaperType::Web)
        );
        assert_eq!(
            detect_wallpaper_type_by_content("/nonexistent/path/doc.pdf"),
            None
        );
        assert_eq!(detect_wallpaper_type_by_content("/nonexistent/noext"), None);
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

    // ── C-008: read 错误日志与回退测试 ───────────────────────────────────────
    //
    // 验证当 File::open 成功但 read 失败时（如路径指向目录），
    // 应记录 tracing::warn! 日志并回退到扩展名检测。

    #[test]
    fn c008_detect_read_error_logs_warning_and_falls_back() {
        // C-008: 当 File::open 成功但 read 失败时，应记录 warn 日志并回退到扩展名检测。
        //
        // 触发方式：创建一个名为 video.mp4 的目录。
        // - 在 Unix-like 平台：File::open 目录成功，read 返回 EISDIR 错误，
        //   命中 C-008 修复路径（warn 日志 + 回退到扩展名检测）。
        // - 在 Windows 平台：File::open 目录可能成功或失败（取决于 CreateFileW 行为）；
        //   若 open 失败则命中既有 File::open 回退路径，若 read 失败则命中 C-008 路径，
        //   两条路径都回退到 detect_wallpaper_type，结果一致。
        //
        // 无论走哪条回退路径，结果都应基于扩展名 `.mp4` 返回 Video。
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let dir_path = tmp.path().join("video.mp4");
        std::fs::create_dir(&dir_path).expect("failed to create dir");

        let path_str = dir_path.to_str().expect("path is not valid utf-8");

        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Video),
            "read 失败或 open 失败时应回退到扩展名检测，.mp4 应识别为 Video"
        );
    }

    // ── v41-C-005 修复：File::open 失败时记录 warn 日志 ─────────────────────
    //
    // v4.0 回归：原实现 `Err(_) => return detect_wallpaper_type(file_path)` 静默
    // 吞掉 open 错误，根因被掩盖。修复后应通过 tracing::warn! 记录错误与路径，
    // 再回退到扩展名检测。本测试覆盖 open 失败的两条路径：
    // 1. 不存在的文件（NotFound）
    // 2. 路径指向目录（Windows 上 open 可能失败，也可能 read 失败，任一路径都
    //    应回退到扩展名检测且不 panic）

    #[test]
    fn v41_c005_open_failure_falls_back_with_warning_nonexistent() {
        // v41-C-005: File::open 对不存在的文件返回 NotFound 错误，
        // 修复后应记录 warn 日志并回退到扩展名检测（返回 Some(Video)）。
        // 测试不直接断言日志输出（依赖 tracing subscriber 状态），
        // 仅验证回退行为正确，且函数不 panic。
        let path = "/nonexistent/v41_c005/video.mp4";
        assert_eq!(
            detect_wallpaper_type_by_content(path),
            Some(WallpaperType::Video),
            "v41-C-005: open 失败时应回退到扩展名检测，.mp4 应识别为 Video"
        );

        // 无扩展名的不存在路径应返回 None（与原行为一致）
        let path_no_ext = "/nonexistent/v41_c005/noext";
        assert_eq!(
            detect_wallpaper_type_by_content(path_no_ext),
            None,
            "v41-C-005: open 失败时回退到扩展名检测，无扩展名应返回 None"
        );
    }

    #[test]
    fn v41_c005_open_failure_falls_back_with_warning_directory_path() {
        // v41-C-005: 路径指向目录时 File::open 行为平台相关：
        // - Unix: open 目录成功，read 返回 EISDIR（命中 C-008 read 失败路径）
        // - Windows: open 目录可能成功或失败
        // 无论走 open 失败还是 read 失败路径，都应回退到扩展名检测且不 panic。
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        // 创建名为 image.png 的目录（路径扩展名为 .png，应回退识别为 Image）
        let dir_path = tmp.path().join("image.png");
        std::fs::create_dir(&dir_path).expect("failed to create dir");

        let path_str = dir_path.to_str().expect("path is not valid utf-8");
        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Image),
            "v41-C-005: open/read 失败时应回退到扩展名检测，.png 应识别为 Image"
        );
    }

    // ── v41-C-015: BMP / JPEG 魔数检测测试 ──────────────────────────────────
    //
    // 验证 BMP ("BM" = 0x42 0x4D) 与 JPEG (0xFF 0xD8 0xFF) 魔数识别为 Image 类型，
    // 字节不足时不误判，且不干扰其他魔数检测（GIF / PNG / EBML / MP4 等）。
    // 通过纯函数 detect_wallpaper_type_by_magic_bytes 测试，无需文件系统；
    // 另通过 detect_wallpaper_type_by_content 做端到端验证（含扩展名篡改场景）。

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

    #[test]
    fn v41_c015_bmp_file_identified_by_content() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let file_path = tmp.path().join("test.bmp");
        let content: [u8; 14] = [
            b'B', b'M', 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00,
        ];
        std::fs::write(&file_path, content).expect("failed to write bmp file");
        let path_str = file_path.to_str().expect("path is not valid utf-8");
        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Image),
            "v41-C-015: BMP 文件应通过魔数识别为 Image"
        );
    }

    #[test]
    fn v41_c015_jpeg_file_identified_by_content() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let file_path = tmp.path().join("test.jpg");
        let content: [u8; 9] = [0xFF, 0xD8, 0xFF, 0xE0, 0x10, 0x4A, 0x46, 0x49, 0x46];
        std::fs::write(&file_path, content).expect("failed to write jpeg file");
        let path_str = file_path.to_str().expect("path is not valid utf-8");
        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Image),
            "v41-C-015: JPEG 文件应通过魔数识别为 Image"
        );
    }

    #[test]
    fn v41_c015_bmp_identified_even_with_wrong_extension() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let file_path = tmp.path().join("bmp_renamed.txt");
        let content: [u8; 14] = [
            b'B', b'M', 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00,
        ];
        std::fs::write(&file_path, content).expect("failed to write file");
        let path_str = file_path.to_str().expect("path is not valid utf-8");
        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Image),
            "v41-C-015: BMP 文件即使扩展名错误也应通过魔数识别为 Image"
        );
    }

    #[test]
    fn v41_c015_jpeg_identified_even_with_wrong_extension() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let file_path = tmp.path().join("jpeg_renamed.dat");
        let content: [u8; 9] = [0xFF, 0xD8, 0xFF, 0xE0, 0x10, 0x4A, 0x46, 0x49, 0x46];
        std::fs::write(&file_path, content).expect("failed to write file");
        let path_str = file_path.to_str().expect("path is not valid utf-8");
        assert_eq!(
            detect_wallpaper_type_by_content(path_str),
            Some(WallpaperType::Image),
            "v41-C-015: JPEG 文件即使扩展名错误也应通过魔数识别为 Image"
        );
    }
}
