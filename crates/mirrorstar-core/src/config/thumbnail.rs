//! 缩略图生成
//!
//! 使用 `image` crate 为图片/GIF 生成 320x180 缩略图（JPEG，质量 85）。
//! 缩略图保存到 `data_dir/thumbnails/` 目录，文件名基于文件路径的 hex 编码
//! （C-006 修复，跨 Rust 版本稳定），便于去重。
//!
//! - Image/Gif 类型：直接读取图像文件生成缩略图（`generate_thumbnail`）
//! - Video 类型：使用随包分发的 mpv（`--vo=image` 抽帧）抽取首帧
//! - Web 类型：无源文件，由命令层直接跳过缩略图生成

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use image::{GenericImageView, ImageEncoder, ImageFormat};

use crate::MirrorStarError;

/// 视频缩略图临时输出目录 RAII 守卫（mpv 后端）
///
/// mpv 的 `--vo=image` 会把抽出的帧写入 `--vo-image-outdir` 指定的目录
/// （默认文件名 `00000001.jpg`）。该目录为本次调用专用的临时目录，需在函数
/// 退出时（成功 / 失败 / `?` 提前返回）清理，避免残留堆积。守卫持有目录
/// `PathBuf`（拥有所有权），drop 时 best-effort 递归删除。
struct TmpDirGuard {
    path: PathBuf,
}

impl TmpDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TmpDirGuard {
    fn drop(&mut self) {
        // best-effort：目录不存在或删除失败均忽略
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 缩略图原子落盘的临时文件 RAII 守卫
///
/// 持有同目录唯一临时文件路径，drop 时 best-effort 删除。用于保证
/// `generate_thumbnail_from_image_file` 中任何 `?` 提前返回都会清理临时文件；
/// rename 成功后临时文件已不存在，drop 时 `remove_file` 找不到文件也无害。
struct TmpFileGuard(PathBuf);

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        // best-effort：文件已被 rename 走或删除失败均忽略
        let _ = std::fs::remove_file(&self.0);
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
pub fn generate_thumbnail(
    file_path: &str,
    thumbnail_dir: &Path,
) -> Result<String, MirrorStarError> {
    std::fs::create_dir_all(thumbnail_dir)?;
    generate_thumbnail_from_image_file(Path::new(file_path), file_path, thumbnail_dir)
}

/// C-006 修复：将路径字节编码为**固定长度**十六进制 hash（64-bit FNV-1a，16 位 hex）
///
/// 跨 Rust 版本稳定的路径编码方式，用于生成缩略图文件名（C-006）与 mpv 抽帧
/// 临时目录命名（C-007）。同一路径在**同一平台**上稳定生成相同 hash；跨平台可能
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

/// 返回源文件对应的缓存缩略图**绝对路径**
///
/// 路径为 `data_root()/thumbnails/{thumbnail_name_from_path}`，即
/// `data_root()/thumbnails/thumb_{path_hex}.jpg`。该路径与
/// [`generate_thumbnail`] / [`generate_video_thumbnail`] 实际写入的路径完全一致
/// （同一源路径 → 同一缓存文件），因此调用方可用它探测缓存是否存在。
///
/// 由于路径基于 [`crate::config::manager::data_root`]（内部含平台默认值与 fallback），
/// 任意组件无需读取配置字段即可定位缓存文件。
///
/// # 参数
///
/// - `file_path`: 源文件路径
///
/// # 返回
///
/// - `file_path` 为空字符串时返回 `None`
/// - 否则返回缓存缩略图的绝对路径（不保证文件已存在）
pub fn thumbnail_cache_path(file_path: &str) -> Option<PathBuf> {
    if file_path.is_empty() {
        return None;
    }
    Some(
        crate::config::manager::data_root()
            .join("thumbnails")
            .join(thumbnail_name_from_path(Path::new(file_path))),
    )
}

/// C-007 修复：mpv 抽帧临时目录命名的并发唯一性计数器
///
/// mpv 后端每次调用会创建唯一临时输出目录
/// `_mpv_tmp_{path_hex}_{counter}_{nanos}`：
/// - `path_hex`：源视频路径字节的 hex 编码（见 [`path_to_hex`]，跨 Rust 版本稳定，
///   便于排查临时目录来源）
/// - `counter`：进程内全局 `AtomicU64` 计数器（本静态量），每次调用单调递增，
///   保证进程内唯一
/// - `nanos`：`SystemTime::now()` 自 UNIX_EPOCH 起的纳秒数，跨进程区分
///
/// # 并发安全
///
/// 修复前（C-007 Finding）：临时输出目录名仅基于源路径哈希，同一视频并发调用
/// 会使用相同目录，导致 mpv 抽帧互相覆盖。C-007 修复追加纳秒时间戳。
///
/// v41-C-006 修复：`SystemTime::now().as_nanos()` 在某些 Windows 系统上分辨率
/// 仅 100ns，同线程快速连续调用可能产生相同时间戳，导致目录名冲突。现追加进程内
/// 全局 `AtomicU64` 计数器组合时间戳，保证进程内单调唯一（即使时间戳分辨率不足，
/// 计数器仍单调递增）。计数器与时间戳组合后，跨进程通过时间戳区分，进程内通过
/// 计数器区分，全场景下目录名唯一。
static TMP_FRAME_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    // 内容嗅探优先、扩展名兜底：`ImageReader::open` 默认按扩展名推断格式，遇到
    // misnamed 文件（如 JPEG 内容却命名为 .png）会选错解码器而失败。调用
    // `with_guessed_format` 先按文件魔数嗅探内容格式，仅在无法识别时回退扩展名，
    // 与壁纸渲染路径（wallpaper/image.rs）保持一致。
    let mut reader = image::ImageReader::open(image_file_path)?.with_guessed_format()?;
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

    // 按宽高比缩放至目标尺寸内（不放大：仅当原图大于目标尺寸时才缩放）。
    // scale = min(THUMB_MAX_W/orig_w, THUMB_MAX_H/orig_h)，保持宽高比，
    // 避免非 16:9 图片缩略图被压扁。目标尺寸 round 取整且至少 1×1。
    let thumb = if orig_w > THUMB_MAX_W || orig_h > THUMB_MAX_H {
        let scale = (THUMB_MAX_W as f64 / orig_w as f64).min(THUMB_MAX_H as f64 / orig_h as f64);
        let dw = ((orig_w as f64 * scale).round() as u32).max(1);
        let dh = ((orig_h as f64 * scale).round() as u32).max(1);
        // v5.0 C-PERF-007: Triangle 比 Lanczos3 快 3-5 倍，320×180 缩略图视觉差异不可察觉
        img.resize(dw, dh, image::imageops::FilterType::Triangle)
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

    // 原子落盘：先写同目录唯一临时文件，再 rename 替换最终文件。
    //
    // 理由：壳进程（添加壁纸）与 UI 进程（可见卡片按需生成）可能并发为同一源文件
    // 生成同一张确定性缩略图，直接 `File::create` 会让读者读到撕裂的半成品。
    // 临时文件与目标放在同一目录（同卷），`rename` 才具原子性。
    // Windows 上 `rename` 到已存在目标会失败，故失败时先删除目标再 rename
    // （幂等替换；并发两个生产者内容相同，允许后者覆盖）。
    let tmp_path = thumbnail_dir.join(format!(
        ".{}.{}.{}.tmp",
        thumb_name,
        std::process::id(),
        TMP_FRAME_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // RAII 守卫：任何 `?` 提前返回都会清理临时文件。rename 成功后临时文件已被移走，
    // Drop 时 remove_file 找不到文件也无害。
    let _tmp_guard = TmpFileGuard(tmp_path.clone());

    // 使用 JPEG 编码器显式控制质量（image crate 默认 save_with_quality 不可用）
    let file = std::fs::File::create(&tmp_path)?;
    let mut writer = BufWriter::new(file);
    let encoder = JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
    encoder.write_image(rgb.as_raw(), new_w, new_h, image::ExtendedColorType::Rgb8)?;
    writer.flush()?;
    // flush 仅把缓冲刷入 OS，再 sync_all 确保数据落盘后才 rename（换出后读者立即读到完整内容）。
    let file = writer
        .into_inner()
        .map_err(|e| MirrorStarError::Io(e.into_error()))?;
    file.sync_all()?;

    // 原子替换：优先直接 rename；Windows 目标已存在时失败则先删后 rename。
    if std::fs::rename(&tmp_path, &thumb_path).is_err() {
        let _ = std::fs::remove_file(&thumb_path);
        std::fs::rename(&tmp_path, &thumb_path)?;
    }

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

/// C04 修复：转义 CLI 输入路径，防止参数注入
///
/// 若 `file_path` 以 `-` 开头（如 `-secret.mp4`），CLI 参数解析会将其误解析为
/// 命令行选项而非输入文件路径。通过在路径前加 `./` 前缀（仅当路径以 `-`
/// 开头时），使 CLI 将其识别为相对路径文件名而非选项。
///
/// 绝对路径（Windows `C:\...` / `\\server\...`，Unix `/...`）不会以 `-`
/// 开头，无需处理；包含子目录的相对路径（如 `data/-foo.mp4`）整体不以 `-`
/// 开头，也不会被误解析。仅需处理"裸文件名以 `-` 开头"的场景。
fn escape_cli_input(file_path: &str) -> String {
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

/// 解析随应用分发的 mpv 可执行文件路径
///
/// 复用子进程渲染器的路径解析逻辑（应用目录 → 仓库根回溯 → PATH）。
fn resolve_mpv_executable() -> PathBuf {
    crate::wallpaper::subprocess_base::SubprocessRendererBase::find_bundled_executable(
        Some("mpv"),
        "mpv.exe",
        "mpv",
    )
}

/// 在 mpv 输出目录中查找抽出的帧文件
///
/// 优先返回 mpv 默认命名的 `00000001.jpg`；若不存在则回退返回目录内任意 `*.jpg`
/// （按文件名排序取第一个），兼容不同 mpv 版本的命名差异。
fn find_mpv_frame(dir: &Path) -> Option<PathBuf> {
    let preferred = dir.join("00000001.jpg");
    if preferred.exists() {
        return Some(preferred);
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .map(|ext| ext.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// 使用随包 mpv 抽取视频首帧生成缩略图（唯一后端实现）
///
/// 作为视频缩略图的唯一后端，调用
/// `mpv --no-config --no-audio --frames=1 --vo=image --vo-image-format=jpg
/// --vo-image-outdir=<tmp_dir> --start=0.1 -- <input>`，由 mpv 的 `image` 视频输出
/// 将首帧写为 JPEG（默认 `00000001.jpg`），再把抽出的帧交给
/// [`generate_thumbnail_from_image_file`] 做解码/缩放/编码，产出 `thumb_{path_hex}.jpg`。
///
/// 每次调用使用唯一的临时输出目录（`_mpv_tmp_{path_hex}_{counter}_{nanos}`），
/// 避免壳后台与 UI 按需生成并发时互相覆盖；临时目录由 [`TmpDirGuard`] RAII 守卫在
/// 所有返回路径（成功 / 失败 / `?` 提前返回）自动清理。
///
/// # 参数
/// - `file_path`: 源视频文件路径
/// - `thumbnail_dir`: 缩略图输出目录
///
/// # 错误
/// - mpv 调用失败（非零退出码 / IO 错误）→ `MirrorStarError::ImageDecode`（含 stderr 摘要）
/// - mpv 未产出帧文件 → `MirrorStarError::ImageDecode`
/// - 帧文件解码/缩放/落盘失败 → 透传 `generate_thumbnail_from_image_file` 的错误
fn generate_video_thumbnail_via_mpv(
    file_path: &str,
    thumbnail_dir: &Path,
) -> Result<String, MirrorStarError> {
    // 显式创建缩略图目录（mpv 会自行创建 outdir，但此处更稳妥；同时保证被测函数
    // 独立调用时目录就绪）。
    std::fs::create_dir_all(thumbnail_dir)?;

    let mpv_exe = resolve_mpv_executable();

    // 本次调用唯一的临时输出目录，避免并发互相覆盖（counter + nanos，见
    // TMP_FRAME_COUNTER 的并发唯一性说明）。
    let counter = TMP_FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_dir = thumbnail_dir.join(format!(
        "_mpv_tmp_{}_{}_{}",
        path_to_hex(Path::new(file_path)),
        counter,
        nanos
    ));
    std::fs::create_dir_all(&tmp_dir)?;

    // RAII 守卫：无论成功/失败/`?` 提前返回，临时目录都会被 best-effort 清理。
    let _dir_guard = TmpDirGuard::new(tmp_dir.clone());

    // 路径转义：以 `-` 开头的源路径加 `./` 前缀，防止被误解析为 CLI 选项。
    let safe_input = escape_cli_input(file_path);

    let output = std::process::Command::new(&mpv_exe)
        .arg("--no-config")
        .arg("--no-audio")
        .arg("--frames=1")
        .arg("--vo=image")
        .arg("--vo-image-format=jpg")
        .arg(format!("--vo-image-outdir={}", tmp_dir.display()))
        .arg("--start=0.1")
        // `--` 隔离后续参数，防止源路径被误解析为 mpv 选项
        .arg("--")
        .arg(&safe_input)
        .output()
        .map_err(|e| {
            MirrorStarError::ImageDecode(format!(
                "调用 mpv 抽帧失败（{}）: {}",
                mpv_exe.display(),
                e
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MirrorStarError::ImageDecode(format!(
            "mpv 抽帧失败 (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.chars().take(500).collect::<String>()
        )));
    }

    let frame_path = find_mpv_frame(&tmp_dir).ok_or_else(|| {
        MirrorStarError::ImageDecode(format!(
            "mpv 未产出帧文件（输出目录 {} 下无 00000001.jpg / *.jpg）",
            tmp_dir.display()
        ))
    })?;

    // 复用统一解码/缩放/编码逻辑，产出 thumb_{path_hex}.jpg。
    generate_thumbnail_from_image_file(&frame_path, file_path, thumbnail_dir)
}

/// 为 Video 类型壁纸生成缩略图
///
/// 保留对外签名与返回语义不变。内部始终使用随包 mpv 后端
/// [`generate_video_thumbnail_via_mpv`] 抽取视频首帧。
///
/// # 错误
/// - C-013：`file_path` 含 `://`（非本地路径，可能触发协议注入）→ `MirrorStarError::InvalidPath`
/// - mpv 后端失败 → `MirrorStarError::ImageDecode`
///
/// # C-013 路径本地化校验
/// 拒绝含 `://` 的路径（如 `http://...` / `concat:...`）以防协议注入。
/// CLI 输入支持 `concat:` / `http://` / `pipe:` 等协议前缀，若 `file_path`
/// 来自不可信来源（如配置文件 / 前端输入），可能被构造为协议 URL 触发非预期
/// 行为（如远程拉流、拼接多个文件）。入口处直接拒绝含 `://` 的路径，确保
/// 仅本地文件路径进入后端命令行。
pub fn generate_video_thumbnail(
    file_path: &str,
    thumbnail_dir: &Path,
) -> Result<String, MirrorStarError> {
    // C-013: 拒绝含 '://' 的路径以防协议注入（concat:/http:// 等）
    if file_path.contains("://") {
        return Err(MirrorStarError::InvalidPath {
            reason: format!(
                "generate_video_thumbnail 拒绝非本地路径（含 '://'）：{}",
                file_path
            ),
        });
    }

    std::fs::create_dir_all(thumbnail_dir)?;

    generate_video_thumbnail_via_mpv(file_path, thumbnail_dir)
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
    fn thumbnail_preserves_aspect_ratio_for_square_image() {
        // 测试 A（1:1 方图）：800x800 → scale = min(320/800, 180/800) = 0.225
        // → round(800*0.225) = 180，缩略图应为 180x180，宽高比保持 1.0
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_square");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src_path = dir.join("square.png");
        let img = image::DynamicImage::new_rgb8(800, 800);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(result.is_ok(), "缩略图生成应成功: {:?}", result.err());
        let thumb_path = thumb_dir.join(result.unwrap());

        // 用 ImageReader 解码输出的 JPEG 缩略图
        let thumb_img = image::ImageReader::open(&thumb_path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        let (w, h) = thumb_img.dimensions();
        assert_eq!(
            (w, h),
            (180, 180),
            "800x800 方图应等比缩放为 180x180（宽高比 1.0），实际 {}x{}",
            w,
            h
        );
        assert_eq!(w as f64 / h as f64, 1.0, "方图宽高比应保持 1.0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_preserves_aspect_ratio_for_wide_image() {
        // 测试 B（超宽图）：640x180 → scale = min(320/640, 180/180) = 0.5
        // → (round(640*0.5), round(180*0.5)) = (320, 90)
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_wide");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src_path = dir.join("wide.png");
        let img = image::DynamicImage::new_rgb8(640, 180);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(result.is_ok(), "缩略图生成应成功: {:?}", result.err());
        let thumb_path = thumb_dir.join(result.unwrap());

        let thumb_img = image::ImageReader::open(&thumb_path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        let (w, h) = thumb_img.dimensions();
        assert_eq!(
            (w, h),
            (320, 90),
            "640x180 超宽图应等比缩放为 320x90，实际 {}x{}",
            w,
            h
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_does_not_upscale_small_image() {
        // 测试 C（小图不缩放）：100x50 未超过 320x180，缩略图应保持原尺寸
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_small_no_upscale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src_path = dir.join("small.png");
        let img = image::DynamicImage::new_rgb8(100, 50);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(result.is_ok(), "缩略图生成应成功: {:?}", result.err());
        let thumb_path = thumb_dir.join(result.unwrap());

        let thumb_img = image::ImageReader::open(&thumb_path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        let (w, h) = thumb_img.dimensions();
        assert_eq!(
            (w, h),
            (100, 50),
            "100x50 小图不应被放大，应保持原尺寸，实际 {}x{}",
            w,
            h
        );

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
        // 背景：拖动静态壁纸进主窗口时，部分 .png 源文件实际内容并非合法图像
        // （如损坏文件，或文件头既不匹配 PNG 魔数 0x89 50 4E 47，也不匹配任何其它
        // 已知图像格式）。解码路径现为「内容嗅探优先、扩展名兜底」：先按文件魔数
        // 嗅探格式，无法识别时回退按扩展名（.png）选择解码器。本例是纯垃圾字节
        // （非任何已知魔数），回退为 PNG 解码器后仍失败 → ImageDecode。
        // （真正的 misnamed 文件如 JPEG 内容却命名 .png 会被内容嗅探正确识别并成功，
        // 见 generate_thumbnail_succeeds_for_jpeg_content_named_png。）
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
    fn generate_thumbnail_succeeds_for_jpeg_content_named_png() {
        // 回归：misnamed 文件——内容为合法 JPEG（文件头 FF D8 FF ...），扩展名却是 .png。
        // 修复前 ImageReader::open 按扩展名选 PNG 解码器 → "Invalid PNG signature" 失败。
        // 修复后内容嗅探优先识别为 JPEG，正确解码并生成缩略图。
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_thumbnail_test_misnamed_jpeg_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 用 JPEG 编码内容，但文件名以 .png 结尾
        let src_path = dir.join("actually_jpeg.png");
        let img = image::DynamicImage::new_rgb8(200, 120);
        img.save_with_format(&src_path, ImageFormat::Jpeg).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let result = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir);
        assert!(
            result.is_ok(),
            "JPEG 内容名为 .png 的 misnamed 文件应成功生成缩略图: {:?}",
            result.err()
        );
        let thumb_name = result.unwrap();
        let thumb_path = thumb_dir.join(&thumb_name);
        assert!(thumb_path.exists(), "缩略图文件应存在");

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
        let file_name =
            "批注 2026-08-14 114650 测试用例文件名 for os error regression one two three.png";
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

    // ── C04 修复：CLI 参数注入防护 ──────────────────────────────────────────

    #[test]
    fn escape_cli_input_prepends_dot_slash_for_dash_prefix() {
        // 以 `-` 开头的路径应加 `./` 前缀
        assert_eq!(escape_cli_input("-secret.mp4"), "./-secret.mp4");
        assert_eq!(escape_cli_input("--weird.mp4"), "./--weird.mp4");
        assert_eq!(escape_cli_input("-x"), "./-x");
    }

    #[test]
    fn escape_cli_input_keeps_normal_paths_unchanged() {
        // 普通相对路径不应被修改
        assert_eq!(escape_cli_input("video.mp4"), "video.mp4");
        assert_eq!(escape_cli_input("data/video.mp4"), "data/video.mp4");
        assert_eq!(escape_cli_input("foo/bar/-dash.mp4"), "foo/bar/-dash.mp4");
    }

    #[test]
    fn escape_cli_input_keeps_absolute_paths_unchanged() {
        // 绝对路径不以 `-` 开头，不应被修改
        assert_eq!(
            escape_cli_input("C:\\Users\\test\\video.mp4"),
            "C:\\Users\\test\\video.mp4"
        );
        assert_eq!(
            escape_cli_input("/home/test/video.mp4"),
            "/home/test/video.mp4"
        );
        assert_eq!(
            escape_cli_input("\\\\server\\share\\video.mp4"),
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

    // ── v5.3 修复：mpv 后端 / 临时目录清理 ───────────────────────────────────

    /// mpv 后端 lavfi 测试输入（无需真实视频文件即可端到端验证抽帧）
    const MPV_LAVFI_INPUT: &str = "av://lavfi:testsrc=duration=2:size=640x360:rate=10";

    /// 检测 `dir` 下是否存在 `_mpv_tmp_*` 残留目录
    fn has_mpv_tmp_residue(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().starts_with("_mpv_tmp_"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn generate_video_thumbnail_rejects_protocol_path() {
        // C-013：含 '://' 的路径应被入口拒绝（InvalidPath）
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_protocol");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result = generate_video_thumbnail("http://x/a.mp4", &dir);
        assert!(
            matches!(result, Err(MirrorStarError::InvalidPath { .. })),
            "含 '://' 的路径应返回 InvalidPath，实际: {:?}",
            result
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mpv_backend_generates_and_decodes_thumbnail() {
        // 能力探测：未随包分发/未找到 mpv 时跳过（测试仍通过）
        if !resolve_mpv_executable().exists() {
            eprintln!("跳过：未找到随包 mpv");
            return;
        }

        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_mpv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let thumb_dir = dir.join("thumbnails");

        let result = generate_video_thumbnail_via_mpv(MPV_LAVFI_INPUT, &thumb_dir);
        let thumb_name = result.expect("mpv 后端应能为 lavfi 输入生成缩略图");
        assert!(
            thumb_name.starts_with("thumb_"),
            "文件名应以 thumb_ 开头: {}",
            thumb_name
        );
        assert!(
            thumb_name.ends_with(".jpg"),
            "文件名应以 .jpg 结尾: {}",
            thumb_name
        );

        let thumb_path = thumb_dir.join(&thumb_name);
        assert!(thumb_path.exists(), "缩略图文件应存在");

        let img = image::open(&thumb_path).expect("缩略图应可被 image::open 解码");
        let (w, h) = img.dimensions();
        assert!(w <= THUMB_MAX_W, "宽度 {} 应 <= {}", w, THUMB_MAX_W);
        assert!(h <= THUMB_MAX_H, "高度 {} 应 <= {}", h, THUMB_MAX_H);

        // 临时目录应已被清理（无 _mpv_tmp_* 残留）
        assert!(
            !has_mpv_tmp_residue(&thumb_dir),
            "mpv 临时目录应被清理，不应有 _mpv_tmp_* 残留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mpv_backend_cleans_temp_dir_on_failure() {
        // 失败路径（不存在的输入）也应清理临时目录
        if !resolve_mpv_executable().exists() {
            eprintln!("跳过：未找到随包 mpv");
            return;
        }

        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_mpv_fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let thumb_dir = dir.join("thumbnails");
        std::fs::create_dir_all(&thumb_dir).unwrap();

        let result = generate_video_thumbnail_via_mpv(
            &dir.join("definitely_absent.mp4").to_string_lossy(),
            &thumb_dir,
        );
        assert!(result.is_err(), "不存在的输入应返回错误");
        assert!(
            !has_mpv_tmp_residue(&thumb_dir),
            "失败路径下 mpv 临时目录也应被清理"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v53_tmp_dir_guard_removes_dir_on_drop() {
        // TmpDirGuard 在 drop 时应递归删除整个临时目录
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_v53_tmpdir_guard_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("_mpv_tmp_nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("00000001.jpg"), b"frame").unwrap();
        assert!(nested.exists(), "测试前置：临时目录应存在");

        {
            let _guard = TmpDirGuard::new(nested.clone());
        } // _guard 在此 drop

        assert!(
            !nested.exists(),
            "v5.3: TmpDirGuard drop 后临时目录应被删除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── thumbnail_cache_path ──────────────────────────────────────────────────

    #[test]
    fn thumbnail_cache_path_stable_for_same_path() {
        let p1 = thumbnail_cache_path("/wallpapers/foo.jpg");
        let p2 = thumbnail_cache_path("/wallpapers/foo.jpg");
        assert!(p1.is_some(), "非空路径应返回 Some");
        assert_eq!(p1, p2, "同一路径两次调用应返回相同缓存路径");
    }

    #[test]
    fn thumbnail_cache_path_differs_for_different_paths() {
        let p1 = thumbnail_cache_path("/wallpapers/foo.jpg").unwrap();
        let p2 = thumbnail_cache_path("/wallpapers/bar.jpg").unwrap();
        assert_ne!(p1, p2, "不同路径应返回不同缓存路径");
    }

    #[test]
    fn thumbnail_cache_path_none_for_empty() {
        assert_eq!(thumbnail_cache_path(""), None, "空字符串应返回 None");
    }

    #[test]
    fn thumbnail_cache_path_filename_format() {
        let p = thumbnail_cache_path("/wallpapers/foo.jpg").unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("thumb_"),
            "文件名应以 thumb_ 开头: {}",
            name
        );
        assert!(name.ends_with(".jpg"), "文件名应以 .jpg 结尾: {}", name);
        let hex = &name["thumb_".len()..name.len() - ".jpg".len()];
        assert_eq!(hex.len(), 16, "hex 部分应为 16 位，实际: {}", hex);
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex 部分应全部为合法 hex 字符，实际: {}",
            hex
        );
    }

    #[test]
    fn thumbnail_cache_path_parent_is_thumbnails() {
        let p = thumbnail_cache_path("/wallpapers/foo.jpg").unwrap();
        assert_eq!(
            p.parent().unwrap().file_name().unwrap().to_str().unwrap(),
            "thumbnails",
            "父目录应以 thumbnails 结尾，实际: {}",
            p.display()
        );
    }

    // ── 原子落盘 ──────────────────────────────────────────────────────────────

    #[test]
    fn generate_thumbnail_atomic_write_is_decodable_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join("mirrorstar_thumbnail_test_atomic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 生成一张真实小图（640x360，会被缩放）
        let src_path = dir.join("src.png");
        let img = image::DynamicImage::new_rgb8(640, 360);
        img.save_with_format(&src_path, ImageFormat::Png).unwrap();

        let thumb_dir = dir.join("thumbnails");
        let name1 = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir).unwrap();
        // 第二次调用触发「目标已存在」的覆盖分支（Windows 需先删后 rename）
        let name2 = generate_thumbnail(src_path.to_str().unwrap(), &thumb_dir).unwrap();
        assert_eq!(name1, name2, "同一源文件两次生成的文件名应一致");

        let thumb_path = thumb_dir.join(&name2);
        assert!(thumb_path.exists(), "最终缩略图文件应存在");
        let decoded = image::open(&thumb_path).expect("最终缩略图应可正常解码");
        let (w, h) = decoded.dimensions();
        assert!(
            w <= THUMB_MAX_W && h <= THUMB_MAX_H,
            "缩略图尺寸应在 320x180 内，实际 {}x{}",
            w,
            h
        );

        // 目录内不应有 .tmp 残留
        let has_tmp = std::fs::read_dir(&thumb_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!has_tmp, "缩略图目录内不应有 .tmp 残留文件");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
