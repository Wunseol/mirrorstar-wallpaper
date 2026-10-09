//! 图片预览（Task 8.1）。
//!
//! 直接按本地路径限幅解码为 Slint [`Image`]，在弹窗内容区以 `image-fit: contain`
//! 等比完整显示。限幅上限取 1280×720（预览区典型尺寸量级）：既保证观感，
//! 又让单张常驻解码成本有界（≤ 1280×720×3 ≈ 2.6MB RGB8），与源图分辨率无关。
//!
//! **关闭时必须显式清空 `Image`**（置为 `Image::default()`）：软件渲染器对已上传的
//! 位图持有常驻引用，不显式替换则 4K 级解码图像会一直挂在 UI 进程上（实测约 +123MB）。

use slint::Image;

/// 预览解码最大宽度。
pub const PREVIEW_MAX_W: u32 = 1280;
/// 预览解码最大高度。
pub const PREVIEW_MAX_H: u32 = 720;

/// 解码图片为预览 [`Image`]。
pub fn decode(path: &str) -> Result<Image, String> {
    crate::thumbs::decode_capped_to(path, PREVIEW_MAX_W, PREVIEW_MAX_H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_downscales_large_source_within_preview_bounds() {
        let dir = std::env::temp_dir().join(format!("mirrorstar_ui_photo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("big.png");
        image::DynamicImage::new_rgb8(2560, 1440)
            .save_with_format(&src, image::ImageFormat::Png)
            .unwrap();

        let img = decode(src.to_str().unwrap()).expect("应可解码");
        assert_eq!(img.size().width, PREVIEW_MAX_W);
        assert_eq!(img.size().height, PREVIEW_MAX_H);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_reports_missing_file() {
        let err = decode("C:/definitely/not/here.png").unwrap_err();
        assert!(err.contains("打开失败"), "实际错误: {err}");
    }
}
