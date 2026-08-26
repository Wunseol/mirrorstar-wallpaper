//! 缩略图生成
//!
//! 使用 `image` crate 为图片/GIF 生成 320x180 缩略图（JPEG，质量 85）。
//! 缩略图保存到 `data_dir/thumbnails/` 目录，文件名基于文件路径的 hex 编码
//! （C-006 修复，跨 Rust 版本稳定），便于去重。
//!
//! - Image/Gif 类型：直接读取图像文件生成缩略图（`generate_thumbnail`）
//! - Video 类型：通过 ffmpeg CLI 抽取首帧并直接作为缩略图（v5.0 跳过 re-encode，
//!   `generate_video_thumbnail` 直接 rename 临时帧文件）
//! - Web 类型：无源文件，由命令层直接跳过缩略图生成

use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use image::{GenericImageView, ImageEncoder, ImageFormat};

use crate::MirrorStarError;

/// v41-C-007 修复：临时帧文件 RAII 守卫
///
/// 确保函数退出时清理临时帧文件，即使 ffmpeg 抽帧或图像解码失败（`?` 传播错误）。
/// drop 时调用 `std::fs::remove_file` 清理文件（best-effort：文件不存在或删除失败
/// 均忽略，不传播错误）。
///
/// 临时帧文件由 ffmpeg 创建（`generate_video_thumbnail` 调用 ffmpeg 抽帧生成），
/// 守卫仅负责清理，不负责创建。守卫的 `path` 字段借用调用方持有的路径引用，
/// 因此守卫的生命周期不能超出调用方作用域（`'a` 约束）。
struct TmpFrameGuard<'a> {
    path: &'a Path,
}

impl<'a> TmpFrameGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path }
    }
}

impl<'a> Drop for TmpFrameGuard<'a> {
    fn drop(&mut self) {
        // best-effort：文件不存在或删除失败均忽略
        let _ = std::fs::remove_file(self.path);
    }
}

/// 缩略图最大宽度
const THUMB_MAX_W: u32 = 320;
/// 缩略图最大高度
const THUMB_MAX_H: u32 = 180;
/// JPEG 编码质量（1-100）
const JPEG_QUALITY: u8 = 85;
/// 源图像文件大小上限（SEC-003：防止解压炸弹，100MB）
///
/// 适用于压缩格式（PNG/JPEG/WebP/GIF 等）。未压缩格式（BMP/TIFF）使用
/// 更严格的 [`MAX_UNCOMPRESSED_IMAGE_FILE_SIZE`]，因为其文件大小近似等于
/// 解码后像素缓冲区大小。
const MAX_THUMBNAIL_FILE_SIZE: u64 = 100 * 1024 * 1024;
/// C06 修复：未压缩格式（BMP/TIFF）文件大小上限（50MB）
///
/// BMP/TIFF 等未压缩格式的文件大小近似等于解码后像素缓冲区大小（每像素 1-4
/// 字节）。100MB 的 BMP 解码后可能占用数百 MB 内存（含 image crate 内部的
/// RGBA 转换缓冲区），存在 OOM 风险。收紧至 50MB 将解码后内存占用控制在
/// ~200MB 以内。
const MAX_UNCOMPRESSED_IMAGE_FILE_SIZE: u64 = 50 * 1024 * 1024;
/// 源图像单边最大像素数（SEC-003：防止解压炸弹，20000×20000）
const MAX_IMAGE_DIMENSION: u32 = 20000;
/// C06 修复：解码后像素缓冲区大小上限（200MB，按 RGBA 4 字节/像素计算）
///
/// `width * height * 4` 必须不超过此值。用于捕获压缩格式（PNG/TIFF with LZW）
/// 解压后缓冲区远大于文件本身的情况。200MB 上限兼顾安全性与常见高分辨率壁纸
/// （如 8K = 7680×4320 = 33M 像素 = 132MB RGBA，远低于上限）。
const MAX_DECODED_PIXEL_BUFFER_SIZE: u64 = 200 * 1024 * 1024;

/// 生成缩略图
///
/// 读取 `file_path` 指向的图片文件，按保持宽高比的方式缩放至 320x180 以内
/// （不放大原图），以 JPEG 质量 85 编码后保存到 `thumbnail_dir`。
///
/// # 参数
///
/// - `file_path`: 源图片文件路径
/// - `thumbnail_dir`: 缩略图输出目录（不存在时自动创建）
///
/// # 返回
///
/// 成功时返回缩略图文件名（不含目录，例如 `thumb_1a2b3c4d.jpg`），
/// 调用方负责拼接完整路径。
///
/// # 错误
///
/// - 源文件无法打开或解码（非图片/GIF 格式）→ `MirrorStarError::ImageDecode`
/// - 缩略图目录创建/文件写入失败 → `MirrorStarError::Io`
/// - SEC-003：源文件超过 100MB 上限（压缩格式）→ `MirrorStarError::ImageDecode`
/// - C06：BMP/TIFF 未压缩格式超过 50MB 上限 → `MirrorStarError::ImageDecode`
/// - SEC-003：图像尺寸超过 20000×20000 上限 → `MirrorStarError::ImageDecode`
/// - C06：解码后像素缓冲区超过 200MB 上限 → `MirrorStarError::ImageDecode`
pub fn generate_thumbnail(file_path: &str, thumbnail_dir: &Path) -> Result<String, MirrorStarError> {
    std::fs::create_dir_all(thumbnail_dir)?;
    generate_thumbnail_from_image_file(Path::new(file_path), file_path, thumbnail_dir)
}

/// C-006 修复：将路径字节编码为**固定长度**十六进制 hash（64-bit FNV-1a，16 位 hex）
///
/// 跨 Rust 版本稳定的路径编码方式，用于生成缩略图文件名（C-006）与视频缩略图
/// 临时帧文件名（C-007）。同一路径在**同一平台**上稳定生成相同 hash；跨平台可能
/// 不同（可接受，缩略图缓存不跨平台共享）。
///
/// # 为什么用 hash 而非逐字节 hex（os error 123 修复）
///
/// 若直接把 `as_encoded_bytes()` 全部字节转 hex，源路径较长（中文类名 + 空格 +
/// 深层目录，Windows 上每字符 2 字节）时，生成的缩略图文件名单组件会超过 Windows
/// 255 字符上限，`File::create` 报 os error 123 (InvalidFilename)，进而误报「缩略图
/// 生成失败」。64-bit FNV-1a 输出恒定 16 位 hex，长度与源路径无关，彻底规避该问题。
///
/// # 跨平台
///
/// 使用 `as_encoded_bytes()` 获取路径的字节表示：
/// - Unix：UTF-8 字节
/// - Windows：UTF-16 字节（每个字符 2 字节，包括 ASCII 字符）
fn path_to_hex(source_path: &Path) -> String {
    // FNV-1a 64-bit：算法固定、跨 Rust 版本稳定（满足 C-006"不依赖 DefaultHasher
    // 内部算法"的要求），输出固定 16 位 hex。
    //
    // 修复背景（os error 123）：原实现把源路径全部字节直接转 hex 作为缩略图文件名。
    // Windows 上 `as_encoded_bytes()` 为每个字符 2 字节（UTF-16），中文/含空格且路径
    // 较长的源文件其 hex 文件名会超过 255 字符的 Windows 单文件组件上限，导致
    // `File::create(thumb_path)` 报 os error 123 (InvalidFilename) → 误触发
    // 「缩略图生成失败」弹窗（即使源文件是有效可渲染的 PNG/JPEG）。
    // 改为 64-bit FNV-1a 稳定哈希 → 文件名恒为固定长度，与源路径长度无关。
    let bytes = source_path.as_os_str().as_encoded_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
    }
    format!("{:016x}", hash)
}

/// C-006 修复：基于源文件路径生成跨 Rust 版本稳定的缩略图文件名
///
/// 将路径字节的十六进制编码作为文件名后缀，替代 `DefaultHasher`（其内部算法
/// 跨 Rust 版本不稳定，升级 Rust 后哈希变化会导致旧缩略图成孤儿、同文件重复生成）。
///
/// # 算法
///
/// `thumb_{hex}.jpg`，其中 `hex` 是路径字节（`Path::as_os_str().as_encoded_bytes()`）
/// 的十六进制编码（每字节 2 个 hex 字符）。同一路径每次调用生成相同文件名，
/// 不同路径生成不同文件名（除非路径字节完全相同）。
fn thumbnail_name_from_path(source_path: &Path) -> String {
    format!("thumb_{}.jpg", path_to_hex(source_path))
}

/// C-007 修复：生成视频缩略图临时帧文件名
///
/// 文件名格式：`_tmp_frame_{path_hex}_{counter}_{nanos}.jpg`
/// - `path_hex`：源视频路径字节的 hex 编码（与 `thumbnail_name_from_path` 一致，
///   跨 Rust 版本稳定，便于排查临时文件来源）
/// - `counter`：进程内全局 `AtomicU64` 计数器，每次调用单调递增，保证进程内唯一
/// - `nanos`：`SystemTime::now()` 自 UNIX_EPOCH 起的纳秒数，跨进程区分
///
/// # 并发安全
///
/// 修复前（C-007 Finding）：临时帧文件名仅基于源路径哈希，同一视频并发调用
/// 会使用相同路径，导致 ffmpeg 抽帧竞争文件。C-007 修复追加纳秒时间戳。
///
/// v41-C-006 修复：`SystemTime::now().as_nanos()` 在某些 Windows 系统上分辨率
/// 仅 100ns，同线程快速连续调用可能产生相同时间戳，导致文件名冲突。现追加进程内
/// 全局 `AtomicU64` 计数器组合时间戳，保证进程内单调唯一（即使时间戳分辨率不足，
/// 计数器仍单调递增）。计数器与时间戳组合后，跨进程通过时间戳区分，进程内通过
/// 计数器区分，全场景下文件名唯一。
static TMP_FRAME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_frame_name_from_path(source_path: &Path) -> String {
    let counter = TMP_FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "_tmp_frame_{}_{}_{}.jpg",
        path_to_hex(source_path),
        counter,
        nanos
    )
}

/// 从已解码的图像文件生成缩略图（内部复用函数）
///
/// `source_file_path_for_hash`: 用于计算缩略图文件名 hex 编码的路径（视频缩略图用原始视频路径，
/// 普通图片缩略图用图片自身路径），保证同一源文件生成的缩略图文件名稳定（C-006：跨 Rust 版本稳定）。
///
/// 该函数被 `generate_thumbnail`（图片/Gif）与 `generate_video_thumbnail`（视频首帧）
/// 共用，避免 resize + JPEG 编码逻辑重复。
fn generate_thumbnail_from_image_file(
    image_file_path: &Path,
    source_file_path_for_hash: &str,
    thumbnail_dir: &Path,
) -> Result<String, MirrorStarError> {
    // SEC-003：防止解压炸弹，限制源图像文件大小
    // （对临时帧文件可放宽，但仍检查防异常大文件）
    let metadata = std::fs::metadata(image_file_path)
        .map_err(|e| MirrorStarError::ImageDecode(format!("读取图像文件元数据失败: {}", e)))?;
    let file_size = metadata.len();

    // C06 修复：对未压缩格式（BMP/TIFF）收紧文件大小上限至 50MB
    //
    // 未压缩格式的文件大小近似等于解码后像素缓冲区大小，100MB 的 BMP 解码后
    // 可能占用数百 MB 内存（含 image crate 的 RGBA 转换缓冲区），存在 OOM 风险。
    // 通过扩展名识别格式，对 BMP/TIFF 应用更严格的 50MB 上限。
    let format = guess_image_format(&image_file_path.to_string_lossy());
    let is_uncompressed = matches!(format, Some(ImageFormat::Bmp) | Some(ImageFormat::Tiff));
    let size_limit = if is_uncompressed {
        MAX_UNCOMPRESSED_IMAGE_FILE_SIZE
    } else {
        MAX_THUMBNAIL_FILE_SIZE
    };
    if file_size > size_limit {
        let limit_mb = size_limit / (1024 * 1024);
        let format_name = if is_uncompressed {
            "（未压缩格式 BMP/TIFF）"
        } else {
            ""
        };
        return Err(MirrorStarError::ImageDecode(format!(
            "图像文件超过 {}MB 上限{}（当前 {} 字节）",
            limit_mb, format_name, file_size
        )));
    }

    // C-002 修复：在解码前设置图像限制，防止解压炸弹。
    // image::open 内部调用 load → decode，会将完整像素数据读入 DynamicImage。
    // 对于高压缩比格式（如 deflate），100MB 压缩文件可解码为数 GB 像素缓冲区，
    // 在后续尺寸/缓冲区检查执行前内存已分配（OOM）。
    // 改用 ImageReader::open + limits()，在解码阶段即拒绝超限图像
    // （max_image_width/max_image_height 为严格限制，max_alloc 限制解码期总分配内存）。
    // 后续 dimensions 检查保留作为双重防护。
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_PIXEL_BUFFER_SIZE);
    let mut reader = image::ImageReader::open(image_file_path)?;
    reader.limits(limits);
    let img = reader.decode()?;
    let (orig_w, orig_h) = img.dimensions();
    // SEC-003：防止解压炸弹，限制图像尺寸
    if orig_w > MAX_IMAGE_DIMENSION || orig_h > MAX_IMAGE_DIMENSION {
        return Err(MirrorStarError::ImageDecode(format!(
            "图像尺寸超过 {}x{} 上限（当前 {}x{}）",
            MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION, orig_w, orig_h
        )));
    }

    // C06 修复：校验解码后像素缓冲区大小（width * height * 4 ≤ 200MB）
    //
    // 即使文件大小在限制内，压缩格式（如 PNG with LZW、TIFF with LZW/Deflate）
    // 解压后缓冲区可能远大于文件本身。通过校验解码后像素数防止 OOM。
    // 详见 `check_decoded_pixel_buffer_size` 文档。
    check_decoded_pixel_buffer_size(orig_w, orig_h)?;

    // 按宽高比缩放至目标尺寸内（不放大：仅当原图大于目标尺寸时才 resize）
    let thumb = if orig_w > THUMB_MAX_W || orig_h > THUMB_MAX_H {
        // v5.0 C-PERF-007: Triangle 比 Lanczos3 快 3-5 倍，320×180 缩略图视觉差异不可察觉
        img.resize(
            THUMB_MAX_W,
            THUMB_MAX_H,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let rgb = thumb.to_rgb8();
    let (new_w, new_h) = rgb.dimensions();

    // C-006 修复：文件名基于"源文件路径"的 hex 编码（视频用视频路径，图片用图片路径），
    // 同一源文件重复生成会覆盖而非堆积。使用确定性 hex 编码替代 `DefaultHasher`，
    // 确保跨 Rust 版本稳定（避免升级 Rust 后哈希变化导致旧缩略图成孤儿）。
    let thumb_name = thumbnail_name_from_path(Path::new(source_file_path_for_hash));
    let thumb_path = thumbnail_dir.join(&thumb_name);

    // 使用 JPEG 编码器显式控制质量（image crate 默认 save_with_quality 不可用）
    let file = std::fs::File::create(&thumb_path)?;
    let mut writer = BufWriter::new(file);
    let encoder = JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
    encoder.write_image(rgb.as_raw(), new_w, new_h, image::ExtendedColorType::Rgb8)?;

    tracing::debug!(
        source_path = source_file_path_for_hash,
        thumb_path = %thumb_path.display(),
        orig_w,
        orig_h,
        new_w,
        new_h,
        "缩略图已生成"
    );

    Ok(thumb_name)
}

// v5.0 C-PERF-001: 缓存 ffmpeg 可用性探测结果（进程生命周期内不变）
static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// 检测系统是否安装 ffmpeg 可执行文件（PATH 查找）
///
/// 通过执行 `ffmpeg -version` 探测：进程启动成功且退出码为 0 即视为可用。
/// 调用方据此决定 Video 缩略图生成路径（可用 → 抽帧生成；不可用 → 降级跳过）。
///
/// # 阻塞警告（C05 修复）
///
/// **本函数为同步阻塞调用**：通过 `std::process::Command::output()` 等待子进程
/// 退出，期间阻塞当前线程。子进程启动 + 输出采集通常耗时 50-200ms（含进程
/// 创建、PATH 查找、ffmpeg 自身初始化），但最坏情况下可能更长（如系统负载高、
/// 杀毒软件扫描、网络路径上的 ffmpeg）。
///
/// **禁止在 tokio async 上下文直接调用**，否则会阻塞 tokio worker 线程，
/// 拖慢整个异步运行时（包括其他无关的 future 调度）。在 async 上下文中必须
/// 通过 [`tokio::task::spawn_blocking`] 包裹调用：
///
/// ```ignore
/// let available = tokio::task::spawn_blocking(is_ffmpeg_available)
///     .await
///     .unwrap_or(false);
/// ```
///
/// 当前已知的调用方（`src-tauri/src/commands/wallpaper.rs` 的 `add_wallpaper`
/// 与 `regenerate_thumbnails`）均在 `tokio::task::spawn_blocking` 闭包内调用，
/// 符合本契约。
///
/// v5.0 C-PERF-001: 使用 OnceLock 缓存首次探测结果。ffmpeg 是否安装在整个进程
/// 生命周期内不变，原先 `regenerate_thumbnails` 循环对每个视频壁纸重复探测
///（50 个视频 ≈ 5 秒纯浪费），现仅首次调用启动 ffmpeg 子进程，后续直接返回缓存值。
pub fn is_ffmpeg_available() -> bool {
    *FFMPEG_AVAILABLE.get_or_init(|| {
        match std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
        {
            Ok(output) => output.status.success(),
            // C-014: NotFound 静默返回 false（PATH 无 ffmpeg 是常态）
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            // C-014: 非 NotFound（如 PermissionDenied / Other）记录 warn 日志，
            // 便于排查"ffmpeg 已安装但 is_ffmpeg_available 仍返回 false"的异常场景
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "ffmpeg 检测失败（非 NotFound，可能是权限问题）"
                );
                false
            }
        }
    })
}

/// C04 修复：转义 ffmpeg 输入路径，防止参数注入
///
/// 若 `file_path` 以 `-` 开头（如 `-secret.mp4`），ffmpeg 会将其误解析为
/// 命令行选项而非输入文件路径。通过在路径前加 `./` 前缀（仅当路径以 `-`
/// 开头时），使 ffmpeg 将其识别为相对路径文件名而非选项。
///
/// 绝对路径（Windows `C:\...` / `\\server\...`，Unix `/...`）不会以 `-`
/// 开头，无需处理；包含子目录的相对路径（如 `data/-foo.mp4`）整体不以 `-`
/// 开头，ffmpeg 也不会误解析。仅需处理"裸文件名以 `-` 开头"的场景。
fn escape_ffmpeg_input(file_path: &str) -> String {
    if file_path.starts_with('-') {
        format!("./{}", file_path)
    } else {
        file_path.to_string()
    }
}

/// C06 修复：校验解码后像素缓冲区大小（width * height * 4 ≤ 200MB）
///
/// 即使源文件大小在限制内，压缩格式（如 PNG with LZW、TIFF with LZW/Deflate）
/// 解压后缓冲区可能远大于文件本身。通过校验解码后像素数防止 OOM：
/// 200MB / 4 bytes(RGBA) = 52,428,800 像素上限（约 7241×7241 正方形图像）。
///
/// 使用 u64 乘法避免 u32 溢出（`orig_w`/`orig_h` 为 u32，乘积可能溢出）。
/// 提取为独立函数以便单元测试覆盖边界条件，无需在测试中分配超大图像。
fn check_decoded_pixel_buffer_size(width: u32, height: u32) -> Result<(), MirrorStarError> {
    let pixel_buffer_size = (width as u64) * (height as u64) * 4;
    if pixel_buffer_size > MAX_DECODED_PIXEL_BUFFER_SIZE {
        let limit_mb = MAX_DECODED_PIXEL_BUFFER_SIZE / (1024 * 1024);
        return Err(MirrorStarError::ImageDecode(format!(
            "解码后像素缓冲区超过 {}MB 上限（当前 {}x{} = {} 字节 RGBA）",
            limit_mb, width, height, pixel_buffer_size
        )));
    }
    Ok(())
}

/// 为 Video 类型壁纸生成缩略图（通过 ffmpeg CLI 抽取首帧）
///
/// 调用 `ffmpeg -ss 0.1 -i <input> -frames:v 1 -vf scale=320:180:force_original_aspect_ratio=decrease -q:v 2 -y <tmp.jpg>`
/// 抽取首帧到临时文件。ffmpeg 已通过 `-vf scale=...` 缩放至目标尺寸并以 JPEG 质量 2
///（≈ 85）输出，因此直接将临时帧文件 rename 为最终缩略图，跳过 image crate 的
/// decode + re-encode 周期（v5.0 C-PERF-004，节省 5-15ms/视频）。
///
/// # 参数
/// - `file_path`: 源视频文件路径
/// - `thumbnail_dir`: 缩略图输出目录
///
/// # 返回
/// 成功时返回缩略图文件名（`thumb_{hex}.jpg`），文件名基于**原始视频路径**的 hex
/// 编码（C-006 修复，跨 Rust 版本稳定），因此同一视频重复生成会覆盖而非堆积。
///
/// # 错误
/// - ffmpeg 调用失败（非零退出码/IO 错误）→ `MirrorStarError::ImageDecode`
/// - 临时帧文件 rename/copy 到最终路径失败 → `MirrorStarError::ImageDecode`
/// - C-013：`file_path` 含 `://`（非本地路径，可能为 ffmpeg 协议注入）→ `MirrorStarError::InvalidPath`
///
/// # 临时文件清理
/// 抽帧产生的临时文件 `_tmp_frame_{path_hex}_{counter}_{nanos}.jpg`（基于源路径 hex
/// 编码 + 进程内全局计数器 + 纳秒时间戳，C-007 / v41-C-006 修复，确保并发唯一）
/// 会在函数返回前删除（无论成功或失败）。
///
/// v41-C-007 修复：临时帧文件的清理由 [`TmpFrameGuard`] RAII 守卫负责，
/// 在函数任何路径退出（包括 `?` 传播错误、ffmpeg 失败、rename/copy 失败）时自动
/// 触发 `remove_file`，避免手动清理遗漏导致临时文件残留。
///
/// # C-013 路径本地化校验
/// 拒绝含 `://` 的路径（如 `http://...` / `concat:...`）以防 ffmpeg 协议注入。
/// ffmpeg `-i` 参数支持 `concat:` / `http://` / `pipe:` 等协议前缀，若 `file_path`
/// 来自不可信来源（如配置文件 / 前端输入），可能被构造为协议 URL 触发非预期
/// 行为（如远程拉流、拼接多个文件）。入口处直接拒绝含 `://` 的路径，确保
/// 仅本地文件路径进入 ffmpeg 命令行。
pub fn generate_video_thumbnail(
    file_path: &str,
    thumbnail_dir: &Path,
) -> Result<String, MirrorStarError> {
    // C-013: 拒绝含 '://' 的路径以防 ffmpeg 协议注入（concat:/http:// 等）
    if file_path.contains("://") {
        return Err(MirrorStarError::InvalidPath {
            reason: format!(
                "generate_video_thumbnail 拒绝非本地路径（含 '://'）：{}",
                file_path
            ),
        });
    }

    std::fs::create_dir_all(thumbnail_dir)?;

    // C-007 修复：临时帧文件名基于源路径 hex + 纳秒时间戳，确保并发唯一。
    // 同一视频并发调用 generate_video_thumbnail 时，路径 hex 相同但时间戳不同，
    // 避免多个 ffmpeg 进程竞争同一临时文件。
    let tmp_frame = thumbnail_dir.join(tmp_frame_name_from_path(Path::new(file_path)));

    // v41-C-007 修复：创建 RAII 守卫，确保函数退出时（无论成功或失败）清理临时帧文件。
    // 守卫的 drop 会调用 `remove_file`（best-effort），替代原先分散在多处的手动清理。
    // 即使后续 ffmpeg 调用失败或 `?` 提前传播错误，守卫仍会自动触发清理。
    let _guard = TmpFrameGuard::new(&tmp_frame);

    // C04 修复：防止 ffmpeg 参数注入（详见 `escape_ffmpeg_input` 文档）
    let safe_input = escape_ffmpeg_input(file_path);

    // ffmpeg 抽帧：-ss 0.1 跳过可能黑屏的首帧，scale 保持宽高比缩放至 320x180 内
    let output = std::process::Command::new("ffmpeg")
        .arg("-ss")
        .arg("0.1")
        .arg("-i")
        .arg(&safe_input)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg("scale=320:180:force_original_aspect_ratio=decrease")
        .arg("-q:v")
        // v5.0 C-PERF-004: JPEG 质量 2 ≈ 质量 85，与原 JpegEncoder 一致；
        // ffmpeg 已输出缩放后的 JPEG，下游直接 rename 无需 re-encode
        .arg("2")
        .arg("-y") // 覆盖输出
        .arg(&tmp_frame)
        .output()
        .map_err(|e| MirrorStarError::ImageDecode(format!("调用 ffmpeg 失败: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // v41-C-007: 不再需要手动清理 tmp_frame，_guard 在函数返回时自动 drop 清理
        return Err(MirrorStarError::ImageDecode(format!(
            "ffmpeg 抽帧失败 (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.chars().take(500).collect::<String>()
        )));
    }

    // v5.0 C-PERF-004: ffmpeg 已通过 `-vf scale=...` 缩放至目标尺寸并以 JPEG 质量 2
    // 输出 tmp_frame，无需再经 image crate decode + re-encode。直接 rename 为最终
    // 缩略图路径，节省 5-15ms/视频。
    //
    // 文件名基于原始视频路径 hex 编码（与原 generate_thumbnail_from_image_file 内部
    // 调用 thumbnail_name_from_path 一致），保证同一视频重复生成覆盖而非堆积。
    let final_thumb_name = thumbnail_name_from_path(Path::new(file_path));
    let final_thumb_path = thumbnail_dir.join(&final_thumb_name);

    // tmp_frame 与 final_thumb_path 同在 thumbnail_dir 内（同盘符），rename 通常成功；
    // Windows 跨盘符场景下 rename 会失败，fallback 到 copy + remove。
    // 错误路径下 _guard 自动 drop 清理 tmp_frame；成功路径下 rename 消费原文件
    //（copy 成功后手动 remove），_guard 的 drop 为 best-effort no-op。
    if let Err(e) = std::fs::rename(&tmp_frame, &final_thumb_path) {
        // rename 失败（如跨盘符），fallback 到 copy + remove
        if let Err(copy_e) = std::fs::copy(&tmp_frame, &final_thumb_path) {
            // copy 也失败：返回错误，_guard 在函数退出时自动清理 tmp_frame
            return Err(MirrorStarError::ImageDecode(format!(
                "缩略图文件落盘失败: rename={}, copy={}",
                e, copy_e
            )));
        }
        // copy 成功：手动删除 tmp_frame（_guard 也会 best-effort 清理，此处显式删除）
        let _ = std::fs::remove_file(&tmp_frame);
    }

    tracing::debug!(
        source_path = file_path,
        thumb_path = %final_thumb_path.display(),
        "视频缩略图已生成（跳过 re-encode）"
    );

    Ok(final_thumb_name)
}

/// 探测图片格式并返回对应的 image crate `ImageFormat`
///
/// 暴露为 `pub(crate)` 以便测试覆盖格式映射逻辑。
pub(crate) fn guess_image_format(file_path: &str) -> Option<ImageFormat> {
    let ext = Path::new(file_path).extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "webp" => Some(ImageFormat::WebP),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        "ico" => Some(ImageFormat::Ico),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── guess_image_format ────────────────────────────────────────────────────

    #[test]
    fn guess_format_supported_extensions() {
        assert_eq!(guess_image_format("/test/img.jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(
            guess_image_format("/test/img.jpeg"),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(guess_image_format("/test/img.png"), Some(ImageFormat::Png));
        assert_eq!(guess_image_format("/test/anim.gif"), Some(ImageFormat::Gif));
        assert_eq!(guess_image_format("/test/img.bmp"), Some(ImageFormat::Bmp));
        assert_eq!(
            guess_image_format("/test/img.webp"),
            Some(ImageFormat::WebP)
        );
        assert_eq!(
            guess_image_format("/test/img.tiff"),
            Some(ImageFormat::Tiff)
        );
        assert_eq!(guess_image_format("/test/img.tif"), Some(ImageFormat::Tiff));
        assert_eq!(guess_image_format("/test/img.ico"), Some(ImageFormat::Ico));
    }

    #[test]
    fn guess_format_unsupported_returns_none() {
        assert_eq!(guess_image_format("/test/video.mp4"), None);
        assert_eq!(guess_image_format("/test/page.html"), None);
        assert_eq!(guess_image_format("/test/doc.pdf"), None);
        assert_eq!(guess_image_format("/test/noext"), None);
    }

    #[test]
    fn guess_format_case_insensitive() {
        assert_eq!(guess_image_format("/test/IMG.JPG"), Some(ImageFormat::Jpeg));
        assert_eq!(guess_image_format("/test/IMG.PNG"), Some(ImageFormat::Png));
        assert_eq!(guess_image_format("/test/ANIM.GIF"), Some(ImageFormat::Gif));
    }

    // ── generate_thumbnail：端到端 ────────────────────────────────────────────
    //
    // 使用 image crate 生成一个真实的小图片，再调用 generate_thumbnail 验证流程。

    #[test]
    fn generate_thumbnail_for_real_image() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 生成 100x100 的测试图片
        let src_path = dir.join("src.png");
        let img = image::DynamicImage::new_rgb8(100, 100);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(
            result.is_ok(),
            "generate_thumbnail 应成功: {:?}",
            result.err()
        );
        let thumb_name = result.unwrap();
        assert!(thumb_name.starts_with("thumb_"));
        assert!(thumb_name.ends_with(".jpg"));

        let thumb_path = thumb_dir.join(&thumb_name);
        assert!(thumb_path.exists(), "缩略图文件应存在");

        // 验证缩略图可被解码
        let thumb_img = image::open(&thumb_path);
        assert!(thumb_img.is_ok(), "缩略图应可被解码");
        let thumb_img = thumb_img.unwrap();
        // 100x100 < 320x180，resize 不会放大，应保持原尺寸
        assert_eq!(thumb_img.dimensions(), (100, 100));

        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_downscales_large_image() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_large");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 生成 1920x1080 的测试图片
        let src_path = dir.join("large.png");
        let img = image::DynamicImage::new_rgb8(1920, 1080);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(result.is_ok());
        let thumb_name = result.unwrap();
        let thumb_path = thumb_dir.join(&thumb_name);

        let thumb_img = image::open(&thumb_path).unwrap();
        let (w, h) = thumb_img.dimensions();
        // 缩放后应在 320x180 以内且保持 16:9 宽高比
        assert!(w <= THUMB_MAX_W, "宽度 {} 应 <= {}", w, THUMB_MAX_W);
        assert!(h <= THUMB_MAX_H, "高度 {} 应 <= {}", h, THUMB_MAX_H);
        // 1920x1080 按 16:9 缩放至 320x180
        assert_eq!((w, h), (320, 180));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_overwrites_same_file() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_overwrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src_path = dir.join("src.png");
        let img = image::DynamicImage::new_rgb8(50, 50);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let name1 = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir).unwrap();
        let name2 = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir).unwrap();

        // 同一文件路径生成的缩略图文件名应相同（基于路径哈希）
        assert_eq!(name1, name2, "同一文件应生成相同文件名的缩略图");

        // 目录中应只有一个缩略图文件
        let count = std::fs::read_dir(&thumb_dir).unwrap().count();
        assert_eq!(count, 1, "目录中应只有一个缩略图文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_fails_for_nonexistent_file() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result = generate_thumbnail("/nonexistent/file.png", &dir);
        assert!(result.is_err(), "对不存在的文件应返回错误");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_fails_for_non_image_file() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_notimg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 写入非图片内容
        let not_img = dir.join("not_an_image.jpg");
        std::fs::write(&not_img, b"this is not an image").unwrap();

        let result = generate_thumbnail(not_img.to_str().unwrap(), &dir);
        assert!(result.is_err(), "对非图片文件应返回错误");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_returns_image_decode_for_misnamed_png() {
        // 背景：拖动静态壁纸进主窗口时，部分 .png 源文件实际不是合法 PNG
        // （可能是 WebP/AVIF 改名或损坏）。image crate 的 ImageReader 按内容嗅探
        // 识别格式，非 PNG 魔数（0x89 50 4E 47）的内容解码失败 → ImageDecode。
        // 命令层据此错误码回退为直接用源文件路径作缩略图，因此这里必须断言返回
        // 的是 ImageDecode 而非 Io（文件打不开）等其它变体。
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_misnamed_png");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 非 PNG 魔数的任意字节内容（文件存在、可打开，但内容无法解码）
        let src_path = dir.join("test.png");
        std::fs::write(
            &src_path,
            [0x00u8, 0x00, 0x00, 0x0c, 0x4a, 0x00, 0x00, 0x00, 0x6d],
        )
        .unwrap();

        let result = generate_thumbnail(src_path.to_str().unwrap(), &dir);
        let err = result.expect_err("伪 PNG 文件应解码失败");
        assert!(
            matches!(&err, MirrorStarError::ImageDecode(_)),
            "伪 PNG 文件应返回 ImageDecode 错误，实际: {:?}",
            err
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_generates_for_long_cjk_space_filename() {
        // 回归：os error 123. 修复前 path_to_hex 把源路径全部字节转 hex 作缩略图文件名，
        // Windows 上每字符 2 字节（UTF-16），中文 + 空格的长路径会让文件名单组件超
        // 255 字符 → File::create 报 os error 123 (InvalidFilename)，即使源文件是有效
        // 可渲染 PNG/JPEG，也会被误判为「缩略图生成失败」并弹窗。修复后缩略图文件名
        // 为固定 16 位 hash，与源路径长度无关。本测试构造长中文含空格路径的合法 PNG，
        // 断言可正常生成缩略图（旧实现会 Err(os error 123)）。
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_os123_cjk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 长中文含空格文件名（模拟「批注 2026-08-14 114650.png」场景），路径足够长以复现旧缺陷
        let file_name = "批注 2026-08-14 114650 测试用例文件名 for os error regression one two three.png";
        let src_path = dir.join(file_name);
        let img = image::DynamicImage::new_rgb8(800, 600);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(
            result.is_ok(),
            "长中文含空格路径应成功生成缩略图（os error 123 回归），实际: {:?}",
            result
        );
        let thumb_name = result.unwrap();
        let thumb_path = thumb_dir.join(&thumb_name);
        assert!(thumb_path.exists(), "缩略图文件应存在");
        assert!(image::open(&thumb_path).is_ok(), "缩略图应可解码");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C04 修复：ffmpeg 参数注入防护 ────────────────────────────────────────

    #[test]
    fn escape_ffmpeg_input_prepends_dot_slash_for_dash_prefix() {
        // 以 `-` 开头的路径应加 `./` 前缀
        assert_eq!(escape_ffmpeg_input("-secret.mp4"), "./-secret.mp4");
        assert_eq!(escape_ffmpeg_input("--weird.mp4"), "./--weird.mp4");
        assert_eq!(escape_ffmpeg_input("-x"), "./-x");
    }

    #[test]
    fn escape_ffmpeg_input_keeps_normal_paths_unchanged() {
        // 普通相对路径不应被修改
        assert_eq!(escape_ffmpeg_input("video.mp4"), "video.mp4");
        assert_eq!(escape_ffmpeg_input("data/video.mp4"), "data/video.mp4");
        assert_eq!(
            escape_ffmpeg_input("foo/bar/-dash.mp4"),
            "foo/bar/-dash.mp4"
        );
    }

    #[test]
    fn escape_ffmpeg_input_keeps_absolute_paths_unchanged() {
        // 绝对路径不以 `-` 开头，不应被修改
        assert_eq!(
            escape_ffmpeg_input("C:\\Users\\test\\video.mp4"),
            "C:\\Users\\test\\video.mp4"
        );
        assert_eq!(
            escape_ffmpeg_input("/home/test/video.mp4"),
            "/home/test/video.mp4"
        );
        assert_eq!(
            escape_ffmpeg_input("\\\\server\\share\\video.mp4"),
            "\\\\server\\share\\video.mp4"
        );
    }

    // ── C06 修复：解压炸弹防护 ──────────────────────────────────────────────

    #[test]
    fn check_pixel_buffer_size_accepts_small_images() {
        // 小图像应在限制内
        assert!(check_decoded_pixel_buffer_size(100, 100).is_ok());
        assert!(check_decoded_pixel_buffer_size(1920, 1080).is_ok());
        assert!(check_decoded_pixel_buffer_size(3840, 2160).is_ok()); // 4K
        assert!(check_decoded_pixel_buffer_size(7680, 4320).is_ok()); // 8K
    }

    #[test]
    fn check_pixel_buffer_size_rejects_excessive_pixels() {
        // 超过 200MB RGBA 上限的图像应被拒绝
        // 200MB / 4 = 52,428,800 像素上限；7242x7242 = 52,446,564 像素 = ~200.07MB，刚好超限
        assert!(
            check_decoded_pixel_buffer_size(7242, 7242).is_err(),
            "7242x7242 应超过 200MB RGBA 上限"
        );
        // 10000x10000 = 100M 像素 = 400MB RGBA，远超上限
        assert!(
            check_decoded_pixel_buffer_size(10000, 10000).is_err(),
            "10000x10000 应超过 200MB RGBA 上限"
        );
        // 极端宽图像：1 x 60000000 = 60M 像素 = 240MB RGBA，超上限
        assert!(
            check_decoded_pixel_buffer_size(1, 60_000_000).is_err(),
            "1x60000000 应超过 200MB RGBA 上限"
        );
    }

    #[test]
    fn check_pixel_buffer_size_boundary_just_under_limit() {
        // 边界值：刚好在限制内
        // 200MB = 209,715,200 字节 / 4 = 52,428,800 像素
        // 7240x7240 = 52,417,600 像素 = 209,670,400 字节 < 209,715,200，在限制内
        assert!(
            check_decoded_pixel_buffer_size(7240, 7240).is_ok(),
            "7240x7240 应在 200MB RGBA 上限内"
        );
    }

    #[test]
    fn generate_thumbnail_rejects_oversized_bmp_file() {
        // C06：BMP 未压缩格式超过 50MB 上限应被拒绝
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_bmp_oversized");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 创建一个 51MB 的 .bmp 文件（使用 set_len 创建稀疏文件，无需实际写入数据）
        // 文件大小检查发生在 image::open 之前，因此文件内容无需为有效 BMP
        let bmp_path = dir.join("huge.bmp");
        let oversized = MAX_UNCOMPRESSED_IMAGE_FILE_SIZE + 1; // 50MB + 1 字节
        let file = std::fs::File::create(&bmp_path).unwrap();
        file.set_len(oversized).unwrap();
        drop(file);

        let result = generate_thumbnail(bmp_path.to_str().unwrap(), &dir);
        assert!(result.is_err(), "超过 50MB 的 BMP 文件应被拒绝");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("50MB"),
            "错误消息应提及 50MB 上限，实际: {}",
            err_msg
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_thumbnail_accepts_small_bmp_file() {
        // C06：小尺寸 BMP 文件应在 50MB 上限内，正常生成缩略图
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_bmp_small");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 生成 100x100 的 BMP 测试图片
        let src_path = dir.join("src.bmp");
        let img = image::DynamicImage::new_rgb8(100, 100);
        img.save_with_format(&src_path, ImageFormat::Bmp).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(
            result.is_ok(),
            "小尺寸 BMP 应正常生成缩略图: {:?}",
            result.err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c002_decompression_bomb_rejected_before_decode() {
        // C-002 修复：验证超大尺寸图像在解码阶段即被 limits 拒绝，
        // 而非解码完成后才被尺寸检查拒绝（此时内存已分配，可能 OOM）。
        //
        // 构造一个宽度超过 MAX_IMAGE_DIMENSION (20000) 的 PNG：20001x1。
        // 该图像文件本身很小（纯色 PNG 压缩后数 KB），通过文件大小检查，
        // 但在 decode 阶段会触发 ImageReader 的 max_image_width 限制。
        //
        // 修复前（image::open）：会先解码完整图像到 DynamicImage，再执行尺寸检查，
        // 对于真正的解压炸弹（高压缩比 + 超大尺寸）会在检查前 OOM。
        // 修复后（ImageReader + limits）：在 decode 阶段调用 set_limits →
        // check_dimensions，直接返回 LimitErrorKind::DimensionError，
        // 错误消息为 "Image size exceeds limit"（来自 image crate）。
        //
        // 通过断言错误消息包含 "exceeds limit"（image crate 的 limits 错误），
        // 而非 "图像尺寸超过"（我们的 post-decode 检查错误），证明 limits 在解码前生效。
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_c002_bomb");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 构造 20001x1 的 PNG（宽度超过 MAX_IMAGE_DIMENSION）
        let bomb_path = dir.join("bomb.png");
        let img = image::DynamicImage::new_rgb8(MAX_IMAGE_DIMENSION + 1, 1);
        img.save_with_format(&bomb_path, ImageFormat::Png).unwrap();

        // 验证文件大小在限制内（确保是尺寸限制而非文件大小限制触发拒绝）
        let file_size = std::fs::metadata(&bomb_path).unwrap().len();
        assert!(
            file_size < MAX_THUMBNAIL_FILE_SIZE,
            "测试 PNG 文件大小应在 {} 上限内，实际 {} 字节",
            MAX_THUMBNAIL_FILE_SIZE,
            file_size
        );

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(bomb_path.to_str().unwrap(), &thumb_dir);
        assert!(result.is_err(), "超过尺寸上限的图像应被拒绝");

        let err_msg = format!("{}", result.unwrap_err());
        // 修复后：错误来自 image crate 的 limits 检查（"Image size exceeds limit"）
        // 修复前（若 limits 未生效）：错误来自 post-decode 检查（"图像尺寸超过 20000x20000 上限"）
        assert!(
            err_msg.contains("exceeds limit"),
            "C-002 修复：错误应由 image crate 的 limits 检查产生（包含 'exceeds limit'），\
             实际: {}。若错误消息为 '图像尺寸超过'，说明 limits 未在解码前生效",
            err_msg
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C-006 修复：缩略图文件名跨版本稳定性 ──────────────────────────────────
    //
    // 验证 `thumbnail_name_from_path` 使用确定性 hex 编码（替代 `DefaultHasher`），
    // 确保跨 Rust 版本稳定：同一路径生成相同文件名，不同路径生成不同文件名。

    #[test]
    fn c006_thumbnail_name_stable_across_calls() {
        // C-006 Scenario: 同一路径生成相同文件名
        // 同一路径每次调用应生成相同文件名，不依赖 DefaultHasher 的内部算法
        let path = Path::new("/wallpapers/foo.jpg");
        let name1 = thumbnail_name_from_path(path);
        let name2 = thumbnail_name_from_path(path);
        let name3 = thumbnail_name_from_path(path);
        assert_eq!(name1, name2, "同一路径每次调用应生成相同文件名");
        assert_eq!(name1, name3, "同一路径多次调用应稳定生成相同文件名");
        // 文件名格式校验：thumb_{hex}.jpg
        assert!(
            name1.starts_with("thumb_") && name1.ends_with(".jpg"),
            "文件名应为 thumb_{{hex}}.jpg 格式，实际: {}",
            name1
        );
        // hex 部分应为固定长度 16 位（64-bit FNV-1a hash），与源路径长度无关
        // （os error 123 修复：全路径 hex 会超 Windows 255 字符文件名上限）
        let hex_part = &name1["thumb_".len()..name1.len() - ".jpg".len()];
        assert_eq!(
            hex_part.len(),
            16,
            "hex 部分应为固定 16 位长度，实际 hex 长度 {}",
            hex_part.len()
        );
        // hex 部分应全部是合法 hex 字符
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "hex 部分应全部是合法 hex 字符，实际: {}",
            hex_part
        );
    }

    #[test]
    fn c006_thumbnail_name_different_for_different_paths() {
        // C-006 Scenario: 不同路径生成不同文件名
        let path_a = Path::new("/wallpapers/foo.jpg");
        let path_b = Path::new("/wallpapers/bar.jpg");
        let name_a = thumbnail_name_from_path(path_a);
        let name_b = thumbnail_name_from_path(path_b);
        assert_ne!(
            name_a, name_b,
            "不同路径应生成不同文件名（{} vs {}）",
            name_a, name_b
        );
        // 不同路径的稳定 hash（path_to_hex 输出）应不同；文件名即由该 hash 生成
        assert_ne!(
            path_to_hex(path_a),
            path_to_hex(path_b),
            "不同路径的 path hash 应不同"
        );
    }

    // ── C-007 修复：视频缩略图临时文件名唯一性 ──────────────────────────────
    //
    // 验证 `tmp_frame_name_from_path` 在路径 hex 编码基础上追加纳秒时间戳，
    // 确保同一视频并发调用 `generate_video_thumbnail` 时使用不同的临时帧文件名，
    // 避免多个 ffmpeg 进程竞争同一临时文件。

    #[test]
    fn c007_video_thumbnail_temp_name_unique_per_call() {
        // C-007 Scenario: 同一视频并发调用应使用不同临时帧文件名
        //
        // v41-C-006 修复后文件名格式：`_tmp_frame_{path_hex}_{counter}_{nanos}.jpg`
        // - path_hex：路径 hex 编码（同一视频路径相同）
        // - counter：进程内全局 AtomicU64 计数器（每次调用单调递增）
        // - nanos：SystemTime::now() 纳秒时间戳（每次调用不同）
        //
        // 连续两次调用 tmp_frame_name_from_path，即使路径相同，counter 也不同，
        // 因此文件名应不同。这是 C-007 + v41-C-006 修复的核心保证：并发调用互不干扰。
        let path = Path::new("/wallpapers/foo.mp4");
        let name1 = tmp_frame_name_from_path(path);
        let name2 = tmp_frame_name_from_path(path);

        // 核心断言：两次调用生成不同文件名（counter 单调递增）
        assert_ne!(
            name1, name2,
            "C-007: 同一路径连续两次调用应生成不同临时文件名（实际 name1={}, name2={}）",
            name1, name2
        );

        // 文件名格式校验：_tmp_frame_{hex}_{counter}_{nanos}.jpg
        assert!(
            name1.starts_with("_tmp_frame_") && name1.ends_with(".jpg"),
            "C-007: 临时帧文件名格式应为 _tmp_frame_{{hex}}_{{counter}}_{{nanos}}.jpg，实际: {}",
            name1
        );
        assert!(
            name2.starts_with("_tmp_frame_") && name2.ends_with(".jpg"),
            "C-007: 临时帧文件名格式应为 _tmp_frame_{{hex}}_{{counter}}_{{nanos}}.jpg，实际: {}",
            name2
        );

        // 两次文件名的 path_hex 前缀部分应相同（同一视频路径）
        // v41-C-006 格式：`_tmp_frame_{path_hex}_{counter}_{nanos}.jpg`
        // 截取 `_tmp_frame_` 之后、`.jpg` 之前的部分，按 `_` 分割取首段作为 path_hex
        // （path_hex 是 hex 编码，不含下划线）
        let prefix = "_tmp_frame_";
        let suffix = ".jpg";
        let body1 = &name1[prefix.len()..name1.len() - suffix.len()];
        let body2 = &name2[prefix.len()..name2.len() - suffix.len()];
        // body 应至少包含两个下划线分隔符（path_hex_counter_nanos）
        assert!(
            body1.matches('_').count() >= 2 && body2.matches('_').count() >= 2,
            "C-007: 临时帧文件名应包含 path_hex/counter/nanos 之间的下划线分隔符，\
             实际 name1={}, name2={}",
            name1,
            name2
        );
        // 取第一个下划线之前的部分作为 path_hex 比较
        // （counter / nanos 均为十进制数字，不含下划线）
        let path_hex1 = &body1[..body1.find('_').unwrap()];
        let path_hex2 = &body2[..body2.find('_').unwrap()];
        assert_eq!(
            path_hex1, path_hex2,
            "C-007: 同一视频路径的 hex 编码应相同（实际 {} vs {}）",
            path_hex1, path_hex2
        );

        // path_hex 应与 path_to_hex 的输出一致（与 C-006 编码方式一致）
        let expected_hex = path_to_hex(path);
        assert_eq!(
            path_hex1, &expected_hex,
            "C-007: path_hex 应与 path_to_hex 输出一致（实际 {} vs 期望 {}）",
            path_hex1, expected_hex
        );
    }

    #[test]
    fn c007_video_thumbnail_temp_name_different_for_different_paths() {
        // C-007 补充：不同视频路径应生成不同 path_hex 部分（继承自 C-006）
        let path_a = Path::new("/wallpapers/foo.mp4");
        let path_b = Path::new("/wallpapers/bar.mp4");
        let name_a = tmp_frame_name_from_path(path_a);
        let name_b = tmp_frame_name_from_path(path_b);
        assert_ne!(
            name_a, name_b,
            "不同路径应生成不同临时文件名（{} vs {}）",
            name_a, name_b
        );
    }

    // ── v41-C-006 修复：tmp_frame_name_from_path 进程内全局唯一性 ────────────
    //
    // v4.0 回归：`SystemTime::now().as_nanos()` 在某些 Windows 系统上分辨率仅 100ns，
    // 同线程快速连续调用可能产生相同时间戳。修复后追加进程内全局 AtomicU64 计数器，
    // 保证进程内单调唯一。本测试用 8 线程各生成 100 个名字，断言全部唯一。

    #[test]
    fn v41_c006_tmp_frame_name_unique_across_threads() {
        // v41-C-006: 8 线程各生成 100 个名字，全部应唯一
        // 即使部分线程的 SystemTime::now() 返回相同纳秒（在某些 Windows 系统上
        // 分辨率仅 100ns），AtomicU64 计数器仍保证每次调用递增，因此文件名唯一。
        use std::sync::{Arc, Mutex};
        const THREAD_COUNT: usize = 8;
        const NAMES_PER_THREAD: usize = 100;

        let path = Path::new("/wallpapers/v41_c006_test.mp4");
        let all_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for _ in 0..THREAD_COUNT {
            let path = path.to_path_buf();
            let all_names = Arc::clone(&all_names);
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::with_capacity(NAMES_PER_THREAD);
                for _ in 0..NAMES_PER_THREAD {
                    local.push(tmp_frame_name_from_path(&path));
                }
                all_names.lock().unwrap().extend(local);
            }));
        }

        for h in handles {
            h.join().expect("worker thread panicked");
        }

        let all_names = all_names.lock().unwrap();
        let total = THREAD_COUNT * NAMES_PER_THREAD;
        assert_eq!(all_names.len(), total);

        // 用 HashSet 检测重复
        let unique: std::collections::HashSet<&String> = all_names.iter().collect();
        assert_eq!(
            unique.len(),
            total,
            "v41-C-006: 8 线程 x 100 名字应全部唯一（共 {}），但仅有 {} 个唯一值",
            total,
            unique.len()
        );

        // 文件名格式校验：所有名字应匹配 `_tmp_frame_{hex}_{counter}_{nanos}.jpg`
        for name in all_names.iter() {
            assert!(
                name.starts_with("_tmp_frame_") && name.ends_with(".jpg"),
                "v41-C-006: 文件名格式错误: {}",
                name
            );
        }
    }

    #[test]
    fn v41_c006_tmp_frame_name_counter_monotonic() {
        // v41-C-006: 同一线程连续调用，counter 部分应单调递增
        // 验证 AtomicU64 计数器的单调性（即使时间戳分辨率不足，counter 仍保证唯一）
        let path = Path::new("/wallpapers/v41_c006_monotonic.mp4");
        let names: Vec<String> = (0..10).map(|_| tmp_frame_name_from_path(path)).collect();

        // 解析每个文件名的 counter 部分（`_tmp_frame_{hex}_{counter}_{nanos}.jpg`）
        let prefix = "_tmp_frame_";
        let suffix = ".jpg";
        let counters: Vec<u64> = names
            .iter()
            .map(|name| {
                let body = &name[prefix.len()..name.len() - suffix.len()];
                // body = `{hex}_{counter}_{nanos}`，按 `_` 分割取第二段
                let parts: Vec<&str> = body.splitn(3, '_').collect();
                assert_eq!(parts.len(), 3, "v41-C-006: 文件名应包含 3 段: {}", name);
                parts[1].parse::<u64>().expect("counter 应为数字")
            })
            .collect();

        // counter 应严格单调递增
        for i in 1..counters.len() {
            assert!(
                counters[i] > counters[i - 1],
                "v41-C-006: counter[{}] ({}) 应大于 counter[{}] ({})",
                i,
                counters[i],
                i - 1,
                counters[i - 1]
            );
        }
    }

    // ── v41-C-007 修复：TmpFrameGuard RAII 守卫 ──────────────────────────────
    //
    // v41-C-007：临时帧文件的清理由 RAII 守卫负责，确保函数退出时（无论成功或失败）
    // 自动清理临时文件。本测试验证守卫的 Drop 行为：drop 时删除文件。

    #[test]
    fn v41_c007_tmp_frame_guard_drops_file() {
        // v41-C-007: 守卫 drop 时应删除文件
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c007_guard_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 创建一个临时文件作为守卫的目标
        let tmp_file = dir.join("tmp_frame.jpg");
        std::fs::write(&tmp_file, b"fake frame content").unwrap();
        assert!(tmp_file.exists(), "测试前置：临时文件应存在");

        // 创建守卫，超出作用域时 drop 应删除文件
        {
            let _guard = TmpFrameGuard::new(&tmp_file);
        } // _guard 在此 drop

        // 核心断言：守卫 drop 后文件应被删除
        assert!(
            !tmp_file.exists(),
            "v41-C-007: 守卫 drop 后临时文件应被删除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v41_c007_tmp_frame_guard_drop_silent_on_nonexistent_file() {
        // v41-C-007: 守卫 drop 时若文件不存在（如 ffmpeg 未创建），
        // remove_file 应静默失败（best-effort），不传播错误。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c007_guard_nonexistent_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let nonexistent = dir.join("does_not_exist.jpg");
        assert!(!nonexistent.exists(), "测试前置：文件不应存在");

        // 创建守卫，drop 时尝试删除不存在的文件，应不 panic
        {
            let _guard = TmpFrameGuard::new(&nonexistent);
        } // _guard drop，remove_file 失败但被 `let _ =` 静默

        // 验证：文件仍不存在（删除失败不影响状态）
        assert!(!nonexistent.exists(), "文件应仍不存在");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v41_c007_tmp_frame_cleaned_on_decode_failure() {
        // v41-C-007 行为级测试：构造解码失败场景（生成损坏的临时帧文件，
        // 使 generate_thumbnail_from_image_file 解码失败），
        // 验证守卫在错误传播时自动清理临时帧文件。
        //
        // 由于 generate_video_thumbnail 依赖 ffmpeg（测试环境可能无 ffmpeg），
        // 本测试直接构造一个损坏的"临时帧文件"并调用 generate_thumbnail_from_image_file
        // 模拟解码失败路径，验证守卫 Drop 后文件被清理。
        //
        // 等价验证：直接构造 TmpFrameGuard + 调用 generate_thumbnail_from_image_file
        //（解码失败 → 函数返回 Err → 守卫超出作用域 drop → 文件被清理）。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v41_c007_decode_fail_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 创建损坏的"临时帧文件"（非图片内容，使 image::decode 失败）
        let tmp_frame = dir.join("_tmp_frame_corrupted.jpg");
        std::fs::write(&tmp_frame, b"this is not a valid image").unwrap();
        assert!(tmp_frame.exists(), "测试前置：临时帧文件应存在");

        // 模拟 generate_video_thumbnail 中 generate_thumbnail_from_image_file 失败后的
        // 守卫清理行为：守卫在函数退出时（包括 `?` 传播错误）自动 drop 清理文件。
        let result: Result<String, MirrorStarError> = {
            let _guard = TmpFrameGuard::new(&tmp_frame);
            // 模拟解码失败：调用 generate_thumbnail_from_image_file 解码损坏文件
            let r = generate_thumbnail_from_image_file(&tmp_frame, "/fake/video.mp4", &dir);
            // 模拟 `?` 传播错误：r 是 Err，函数返回，_guard drop 清理文件
            r
        };

        // 解码应失败（损坏的图片文件）
        assert!(
            result.is_err(),
            "v41-C-007: 损坏的临时帧文件解码应失败，实际: {:?}",
            result
        );

        // 核心断言：守卫在错误传播时自动清理了临时帧文件
        assert!(
            !tmp_frame.exists(),
            "v41-C-007: 解码失败后临时帧文件应被守卫自动清理"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
