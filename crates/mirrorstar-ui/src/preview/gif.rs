//! GIF 逐帧预览（Task 8.4）。
//!
//! ## 自研逐帧 + 惰性解码
//!
//! Slint 的软件渲染不支持动画 GIF（`Image` 只显示静态帧），故本模块自行逐帧驱动：
//! 用 `image::codecs::gif::GifDecoder` + `into_frames()` 得到**惰性帧迭代器**
//! （与 core `wallpaper::gif_decode` 用的是同一解码器与模式），每次只解出一帧、转成
//! Slint [`Image`] 覆盖前一帧。因此常驻帧缓冲恒为「当前帧 + 下一帧解码峰值」，与 GIF
//! 总帧数无关；关闭时迭代器与 Slint 侧 `Image` 一并释放。
//!
//! ## 帧延时
//!
//! 真实 GIF 各帧延时可能不同，而 Slint `Timer` 的 `Repeated` 周期固定。这里由本模块在
//! 每个 tick 用**单调时钟**判断是否到达下一帧的显示时刻（`next_due`），未到则跳过，
//! 从而在固定 tick（30ms）下近似还原各帧自身延时；到末尾自动从头循环（重新打开解码器）。

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use image::AnimationDecoder;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use super::photo::{PREVIEW_MAX_H, PREVIEW_MAX_W};

/// 帧延时兜底值（GIF 未声明延时时使用）。
const DEFAULT_DELAY_MS: u32 = 100;
/// 帧延时下限 / 上限（避免 0 或异常大的延时把播放卡死）。
const MIN_DELAY_MS: u32 = 20;
const MAX_DELAY_MS: u32 = 4_000;

/// 一个正在播放的 GIF 预览。
pub struct GifPreview {
    path: PathBuf,
    frames: image::Frames<'static>,
    /// 下一帧应显示的时刻（单调时钟）。
    next_due: Instant,
    /// 到末尾时重新打开解码器需要路径的可解析形式。
    path_str: String,
}

impl GifPreview {
    /// 打开 GIF 并返回（播放器, 首帧图像, 首帧延时）。
    pub fn open(path: &str) -> Result<(Self, Image, u32), String> {
        let frames = open_frames(path)?;
        let mut preview = Self {
            path: PathBuf::from(path),
            frames,
            next_due: Instant::now(),
            path_str: path.to_string(),
        };
        let (image, delay) = preview.pull_next()?;
        preview.next_due = Instant::now() + Duration::from_millis(delay as u64);
        Ok((preview, image, delay))
    }

    /// 若已到下一帧时刻则推进一帧；否则返回 `None`（保持当前帧）。
    pub fn tick(&mut self, now: Instant) -> Result<Option<Image>, String> {
        if now < self.next_due {
            return Ok(None);
        }
        let (image, delay) = self.pull_next()?;
        self.next_due = now + Duration::from_millis(delay as u64);
        Ok(Some(image))
    }

    /// 取出下一帧（到末尾则重新打开解码器从头循环）。
    fn pull_next(&mut self) -> Result<(Image, u32), String> {
        let frame = match self.frames.next() {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => return Err(format!("帧解码失败: {e}")),
            None => {
                self.frames = open_frames(&self.path_str)?;
                match self.frames.next() {
                    Some(Ok(frame)) => frame,
                    Some(Err(e)) => return Err(format!("循环帧解码失败: {e}")),
                    None => return Err("GIF 不含任何帧".to_string()),
                }
            }
        };
        let delay = delay_to_ms(frame.delay());
        let image = frame_to_image(frame.into_buffer());
        Ok((image, delay))
    }

    /// 源文件路径（诊断用）。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// 打开惰性帧迭代器（与 core `gif_decode::open_gif_frames` 同一模式）。
fn open_frames(path: &str) -> Result<image::Frames<'static>, String> {
    let file = File::open(path).map_err(|e| format!("打开失败: {e}"))?;
    let reader = BufReader::new(file);
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| format!("初始化解码器失败: {e}"))?;
    Ok(decoder.into_frames())
}

/// GIF 帧延时（毫秒）→ 归一化到 [`MIN_DELAY_MS`, `MAX_DELAY_MS`]。
fn delay_to_ms(delay: image::Delay) -> u32 {
    let (numer, denom) = delay.numer_denom_ms();
    let raw = numer.checked_div(denom).unwrap_or(DEFAULT_DELAY_MS);
    let raw = if raw == 0 { DEFAULT_DELAY_MS } else { raw };
    raw.clamp(MIN_DELAY_MS, MAX_DELAY_MS)
}

/// RGBA 帧 → Slint [`Image`]，超出预览上限时等比降采样以限制常驻内存。
fn frame_to_image(rgba: image::RgbaImage) -> Image {
    let (w, h) = rgba.dimensions();
    let (tw, th) = crate::thumbs::fit_within(w, h, PREVIEW_MAX_W, PREVIEW_MAX_H);
    let (tw, th) = (tw.max(1), th.max(1));
    let buffer = if (tw, th) == (w, h) {
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h)
    } else {
        let resized = image::imageops::resize(&rgba, tw, th, image::imageops::FilterType::Triangle);
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(resized.as_raw(), tw, th)
    };
    Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_normalizes_zero_and_clamps() {
        assert_eq!(
            delay_to_ms(image::Delay::from_numer_denom_ms(0, 1)),
            DEFAULT_DELAY_MS
        );
        assert_eq!(
            delay_to_ms(image::Delay::from_numer_denom_ms(5, 1)),
            MIN_DELAY_MS
        );
        assert_eq!(delay_to_ms(image::Delay::from_numer_denom_ms(60, 1)), 60);
        assert_eq!(
            delay_to_ms(image::Delay::from_numer_denom_ms(60_000, 1)),
            MAX_DELAY_MS
        );
    }
}
