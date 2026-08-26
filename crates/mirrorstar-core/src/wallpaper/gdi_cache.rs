use crate::MirrorStarError;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, SelectObject,
    SetBrushOrgEx, SetStretchBltMode, HALFTONE, HBITMAP, HDC, HGDIOBJ,
};

/// GDI 双缓冲缓存，封装内存 DC 和位图的创建、重建和销毁
///
/// 用于避免每次 WM_PAINT 时重建内存 DC 和位图，减少 GDI 对象创建开销。
/// 调用者负责管理生命周期：通过 `new()` 创建，`resize()` 调整尺寸，`destroy()` 销毁。
/// 实现了 Drop trait，drop 时自动调用 `destroy()`。由于 `destroy()` 是幂等的，
/// 即使调用者已在 WM_DESTROY 中手动调用 `destroy()`，Drop 也不会产生双重释放。
pub struct GdiCache {
    /// 内存 DC（兼容 DC，窗口生命周期内复用）
    mem_dc: HDC,
    /// 内存位图（窗口尺寸变化时重建）
    mem_bitmap: HBITMAP,
    /// SelectObject 前的原始位图，清理时需要恢复
    old_bitmap: HGDIOBJ,
    /// 当前缓存位图的宽度
    bitmap_width: i32,
    /// 当前缓存位图的高度
    bitmap_height: i32,
    /// v8.0 内存优化：Image 渲染器首次绘制生成的 HBITMAP 缓存。
    ///
    /// 首次 WM_PAINT 时从 pixels 生成 HBITMAP 存入此字段，随后释放 pixels。
    /// 后续 WM_PAINT 直接复用此 HBITMAP，无需重新解码。
    /// SetScalingMode 命令清空此缓存，触发下次 WM_PAINT 从文件重新解码。
    pub(crate) image_bitmap: Option<HBITMAP>,
}

impl GdiCache {
    /// 创建新的 GDI 缓存
    ///
    /// 创建与指定 DC 兼容的内存 DC 和位图，并将位图选入内存 DC。
    /// 任何 GDI 句柄创建失败均返回 `Err`，调用方据此跳过本次渲染。
    pub fn new(hdc: HDC, width: i32, height: i32) -> Result<Self, MirrorStarError> {
        // 源头拦截 0 尺寸（上游 GetClientRect 失败时 client_w/h 为 0）
        if width <= 0 || height <= 0 {
            return Err(MirrorStarError::DesktopIntegration(format!(
                "GdiCache 创建失败: 无效尺寸 {}x{}",
                width, height
            )));
        }
        unsafe {
            let mem_dc = CreateCompatibleDC(hdc);
            if mem_dc == HDC::default() {
                return Err(MirrorStarError::DesktopIntegration(
                    "创建兼容 DC 失败 (CreateCompatibleDC 返回默认句柄)".to_string(),
                ));
            }
            let mem_bitmap = CreateCompatibleBitmap(hdc, width, height);
            if mem_bitmap == HBITMAP::default() {
                // 清理已创建的 mem_dc 避免泄漏（清理路径，错误无实际影响）
                let _ = DeleteDC(mem_dc);
                return Err(MirrorStarError::DesktopIntegration(
                    "创建兼容位图失败 (CreateCompatibleBitmap 返回默认句柄)".to_string(),
                ));
            }
            let old_bitmap = SelectObject(mem_dc, mem_bitmap);
            // v5.0 W-PERF-002: DC 状态在 mem_dc 生命周期内持续有效，
            // 移至创建时一次性设置，避免每次 WM_PAINT 重复调用。
            // HALFTONE 模式与画刷原点不会因 BitBlt/StretchDIBits/SelectObject(bitmap) 而被重置。
            let _ = SetStretchBltMode(mem_dc, HALFTONE);
            let _ = SetBrushOrgEx(mem_dc, 0, 0, None);
            Ok(GdiCache {
                mem_dc,
                mem_bitmap,
                old_bitmap,
                bitmap_width: width,
                bitmap_height: height,
                image_bitmap: None,
            })
        }
    }

    /// 重建位图（窗口尺寸变化时调用，保留 mem_dc）
    ///
    /// 恢复原始位图，删除当前位图，创建新尺寸的位图并选入内存 DC。
    /// 与 `new()` 一致：校验尺寸 > 0 与 `CreateCompatibleBitmap` 返回非默认句柄；
    /// 任一校验失败时将 cache 字段重置为默认（确保 Drop/destroy 幂等清理）并返回 `Err`，
    /// 调用方应丢弃 cache（`*gdi_cache = None`），下次 WM_PAINT 重新 `new()`。
    pub fn resize(&mut self, hdc: HDC, width: i32, height: i32) -> Result<(), MirrorStarError> {
        // 源头拦截 0 尺寸（与 new() 一致，上游 GetClientRect 失败时 client_w/h 为 0）
        if width <= 0 || height <= 0 {
            return Err(MirrorStarError::DesktopIntegration(format!(
                "GdiCache resize 失败: 无效尺寸 {}x{}",
                width, height
            )));
        }
        unsafe {
            SelectObject(self.mem_dc, self.old_bitmap);
            // 删除旧位图（清理路径，错误无实际影响，后续会创建新位图）
            let _ = DeleteObject(self.mem_bitmap);
            let mem_bitmap = CreateCompatibleBitmap(hdc, width, height);
            if mem_bitmap == HBITMAP::default() {
                // 新 bitmap 创建失败：将 cache 字段重置为默认，使 Drop/destroy 幂等清理
                // （mem_dc 保留，destroy 时会 DeleteDC；mem_bitmap=default 时 DeleteObject 无操作）
                self.mem_bitmap = HBITMAP::default();
                self.old_bitmap = HGDIOBJ::default();
                self.bitmap_width = 0;
                self.bitmap_height = 0;
                return Err(MirrorStarError::DesktopIntegration(
                    "GdiCache resize 失败: CreateCompatibleBitmap 返回默认句柄".to_string(),
                ));
            }
            let old_bitmap = SelectObject(self.mem_dc, mem_bitmap);
            self.mem_bitmap = mem_bitmap;
            self.old_bitmap = old_bitmap;
            self.bitmap_width = width;
            self.bitmap_height = height;
            Ok(())
        }
    }

    /// 销毁 GDI 缓存，释放所有资源
    ///
    /// 恢复原始位图，删除当前位图，删除内存 DC。
    /// 此方法是幂等的：多次调用不会产生双重释放。
    pub fn destroy(&mut self) {
        unsafe {
            if self.mem_dc == HDC::default() {
                return;
            }
            SelectObject(self.mem_dc, self.old_bitmap);
            // 清理路径：GDI 对象删除失败通常意味着句柄无效，无法恢复，仅继续清理
            let _ = DeleteObject(self.mem_bitmap);
            // v8.0: 清理 image_bitmap 缓存（若存在）
            if let Some(image_bitmap) = self.image_bitmap.take() {
                let _ = DeleteObject(image_bitmap);
            }
            let _ = DeleteDC(self.mem_dc);
            self.mem_dc = HDC::default();
            self.mem_bitmap = HBITMAP::default();
            self.old_bitmap = HGDIOBJ::default();
            self.bitmap_width = 0;
            self.bitmap_height = 0;
        }
    }

    /// 释放位图但保留内存 DC（暂停时调用以减少内存占用）
    ///
    /// 恢复原始位图并删除当前位图，重置尺寸为 0。
    /// 下次 WM_PAINT 时会通过 `resize()` 重建位图。
    /// 此方法是幂等的：多次调用不会产生双重释放。
    ///
    /// v8.0: 注意此方法仅释放窗口尺寸的 mem_bitmap，**不释放 image_bitmap**。
    /// 暂停时保留 image_bitmap（HBITMAP 内存远小于 pixels，且恢复后重绘需要）。
    pub fn release_bitmap(&mut self) {
        unsafe {
            if self.mem_bitmap == HBITMAP::default() {
                return;
            }
            SelectObject(self.mem_dc, self.old_bitmap);
            // 释放位图（清理路径，错误无实际影响）
            let _ = DeleteObject(self.mem_bitmap);
            self.mem_bitmap = HBITMAP::default();
            self.old_bitmap = HGDIOBJ::default();
            self.bitmap_width = 0;
            self.bitmap_height = 0;
        }
    }

    /// v8.0: 释放 image_bitmap 缓存（SetScalingMode 时调用）
    ///
    /// DeleteObject 当前 image_bitmap 并设为 None。
    /// 下次 WM_PAINT 会走"首次绘制"路径重新生成 HBITMAP。
    /// 此方法是幂等的：多次调用不会产生双重释放。
    pub fn release_image_bitmap(&mut self) {
        if let Some(image_bitmap) = self.image_bitmap.take() {
            unsafe {
                let _ = DeleteObject(image_bitmap);
            }
        }
    }

    /// 获取内存 DC（渲染时使用）
    pub fn mem_dc(&self) -> HDC {
        self.mem_dc
    }

    /// 获取当前位图尺寸
    pub fn dimensions(&self) -> (i32, i32) {
        (self.bitmap_width, self.bitmap_height)
    }
}

impl Drop for GdiCache {
    fn drop(&mut self) {
        self.destroy();
    }
}
