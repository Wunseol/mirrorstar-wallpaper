use std::sync::atomic::{AtomicBool, Ordering};

use crate::MirrorStarError;

/// 单帧数据
#[derive(Debug)]
pub(crate) struct GifFrame {
    /// RGBA 像素数据
    pub(crate) pixels: Vec<u8>,
    /// 帧宽度
    pub(crate) width: u32,
    /// 帧高度
    pub(crate) height: u32,
    /// 帧延迟（毫秒）
    pub(crate) delay_ms: u32,
}

/// GIF 帧像素内存预算默认上限（MB），解码后的帧总内存不超过此值
///
/// v41-W-012: 原为硬编码常量，现提取为配置项 `GifConfig.max_memory_mb`。
/// 此常量保留为默认值和测试用便捷参数。
///
/// v8-C: 从 40 降至 15。配合流式帧缓存（仅保留当前帧 + 前后各
/// [`STREAMING_WINDOW_HALF`] 帧像素），活跃播放内存从全量 ~40MB 降至
/// ~3 帧 × 降采样后尺寸。窗口外帧仅保留元数据（width/height/delay_ms），
/// WM_TIMER 推进到空像素帧时通过 [`decode_gif_frame_at`] 按需重新解码。
pub(crate) const DEFAULT_MAX_GIF_MEMORY_MB: usize = 15;

/// v8-C: 流式帧缓存窗口半幅。
///
/// 活跃播放时仅保留当前帧 + 前后各 N 帧的像素数据在内存中（N = 此常量）。
/// 例如 N=1 时窗口为 `[current-1, current+1]`，共 3 帧。
/// 窗口外帧的 `pixels` 字段被清空（`clear` + `shrink_to_fit`），仅保留
/// 元数据；推进到空像素帧时由 `decode_gif_frame_at` 从文件重新解码。
///
/// (d): 原值 2（5 帧窗口）。#1+#2 持久化解码游标落地后，前向 re-decode
/// 已降至 O(1) delta（见 `bench_cursor_o_1_forward` 实测：#1 prefetch per-call
/// ≈ #2 sync per-call，窗口收集开销可忽略），故将半幅降至 1（3 帧窗口），
/// 省内存 40% 而对 CPU 影响极小。窗口外帧由 #2 sync 兜底快速恢复。
pub(crate) const STREAMING_WINDOW_HALF: usize = 1;

/// v10-C: 单帧像素数据预取阈值（8MB）。
///
/// [`decode_gif_frame_range`] 解码的帧若超过此尺寸则跳过（不加入返回 Vec），
/// 避免 4K GIF（每帧 ~6MB）× (2*`STREAMING_WINDOW_HALF`+1) 帧造成瞬时内存尖峰。被跳过的帧
/// 仍由 [`decode_gif_frame_at`] 在 `WM_TIMER` 同步兜底解码（仅当前帧）。
pub(crate) const MAX_PREFETCH_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 使用 image crate 解码 GIF 文件的所有帧，并在帧尺寸超过屏幕分辨率时降采样。
/// 基于内存预算动态限制帧数：解码后帧总内存不超过 `max_memory_mb`。
///
/// W12：使用 `BufReader<File>` 流式读取，避免将整个 GIF 文件读入内存。
/// W03：`max_frames` 基于首帧实际尺寸计算（而非屏幕分辨率），避免小帧 GIF
/// 被过度截断（旧逻辑以屏幕分辨率估算每帧大小，1×1 帧 GIF 也会被当作
/// 1920×1080 计算预算，导致 max_frames ≈ 4，多帧小 GIF 被错误截断）。
///
/// v41-W-012: `max_memory_mb` 参数替代原硬编码常量，由调用方从配置传入。
/// v18 后：生产路径已改用 `decode_gif_streaming` / `decode_gif_with_cancel_streaming`，
/// 本函数仅测试模块使用（作为全量解码基准对照）。保留为 `pub(crate)` 供跨模块测试。
#[allow(dead_code)]
pub(crate) fn decode_gif(
    path: &str,
    max_memory_mb: usize,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    decode_gif_inner(path, None, max_memory_mb, None)
}

/// v18 后：生产路径已改用 `decode_gif_with_cancel_streaming`，
/// 本函数仅测试模块使用（取消机制基准对照）。保留为 `pub(crate)` 供跨模块测试。
#[allow(dead_code)]
pub(crate) fn decode_gif_with_cancel(
    path: &str,
    cancel: Option<&AtomicBool>,
    max_memory_mb: usize,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    decode_gif_inner(path, cancel, max_memory_mb, None)
}

/// v18: 流式窗口解码（可取消版）——解码过程中即时清空窗口外帧的像素。
///
/// 与 `decode_gif_with_cancel` 行为一致，但额外接受 `streaming_center`：解码
/// 每一帧后，若该帧索引落在 `[center-HALF, center+HALF]` 窗口外，立即
/// `clear + shrink_to_fit` 其 `pixels`（保留 width/height/delay_ms 元数据）。
///
/// 收益：将解码峰值像素内存从「预算上限（max_memory_mb）」降至「窗口大小+1
/// 帧」。对小帧多帧 GIF（如 200×200 × 96 帧，预算 15MB）峰值从 15MB 降至
/// ~640KB；帧数 ≤ 窗口大小时与无流式行为一致（不劣化）。最终窗口仍由
/// `apply_streaming_window` 以实际 `current_frame` 为中心重新校正。
pub(crate) fn decode_gif_with_cancel_streaming(
    path: &str,
    cancel: Option<&AtomicBool>,
    max_memory_mb: usize,
    streaming_center: usize,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    decode_gif_inner(path, cancel, max_memory_mb, Some(streaming_center))
}

/// v18: 流式窗口解码（不可取消版）——供 `handle_resume` 等主线程同步路径使用。
///
/// 等价于 `decode_gif_with_cancel_streaming(path, None, max_memory_mb, streaming_center)`。
pub(crate) fn decode_gif_streaming(
    path: &str,
    max_memory_mb: usize,
    streaming_center: usize,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    decode_gif_inner(path, None, max_memory_mb, Some(streaming_center))
}

/// v18: 判断帧索引是否在流式窗口外（应清空像素以限制峰值内存）。
fn is_outside_streaming_window(frame_idx: usize, center: usize) -> bool {
    let half = STREAMING_WINDOW_HALF;
    let win_start = center.saturating_sub(half);
    let win_end = center + half + 1; // exclusive
    frame_idx < win_start || frame_idx >= win_end
}

/// 内部解码实现：`decode_gif` / `decode_gif_with_cancel` /
/// `decode_gif_with_cancel_streaming` 的公共逻辑。
///
/// `streaming_center` 为 `Some(center)` 时启用 v18 流式窗口解码：每帧解码后
/// 若落在窗口外则立即清空像素，限制峰值内存。为 `None` 时保留全部像素
/// （与历史行为一致）。
fn decode_gif_inner(
    path: &str,
    cancel: Option<&AtomicBool>,
    max_memory_mb: usize,
    streaming_center: Option<usize>,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    // W12: 流式读取，BufReader 包装 File 避免一次性读入全部文件内容
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    // 获取屏幕分辨率用于降采样
    // v5.0 W-PERF-003: 使用缓存避免后台解码每次都调用 GetSystemMetrics
    let (screen_w, screen_h) = super::get_screen_size();

    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 解码初始化失败: {}", e)))?;
    let mut frames_iter = decoder.into_frames();

    // W09: 首帧前检查取消标志（窗口可能在解码启动前已销毁）
    if let Some(flag) = cancel {
        if flag.load(Ordering::Relaxed) {
            tracing::info!("GIF 解码在首帧前被取消");
            return Err(MirrorStarError::ImageDecode("GIF 解码已取消".to_string()));
        }
    }

    // W03: 先解码首帧，使用实际帧尺寸（降采样后）计算 max_frames
    let first_frame_result = frames_iter
        .next()
        .ok_or_else(|| MirrorStarError::ImageDecode("GIF 无有效帧".to_string()))?;
    let first_frame = first_frame_result
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 帧读取失败 (第 0 帧): {}", e)))?;
    let first_gif_frame = process_gif_frame(first_frame, screen_w, screen_h);

    // 根据首帧实际尺寸和内存预算计算最大帧数
    // 每帧像素内存 = width * height * 4 (RGBA)
    let frame_size_bytes = (first_gif_frame.width as usize) * (first_gif_frame.height as usize) * 4;
    let max_frames = if frame_size_bytes == 0 {
        // 理论上不会发生（process_gif_frame 保证尺寸 > 0），防御性回退
        500
    } else {
        (max_memory_mb * 1024 * 1024)
            .checked_div(frame_size_bytes)
            .unwrap_or(500)
    };
    // 保留一个合理的上限，避免极端小帧的 GIF 占用过多帧
    let max_frames = max_frames.min(1000);

    let mut frames = Vec::with_capacity(max_frames.min(64));
    frames.push(first_gif_frame);
    // v18: 流式窗口——首帧（索引 0）若在窗口外则立即清空像素
    if let Some(center) = streaming_center {
        if is_outside_streaming_window(0, center) {
            let f = frames.last_mut().expect("刚 push 首帧，必有末元素");
            f.pixels.clear();
            f.pixels.shrink_to_fit();
        }
    }

    for frame_result in frames_iter {
        // W09: 每帧前检查取消标志，窗口销毁时尽快退出解码循环
        if let Some(flag) = cancel {
            if flag.load(Ordering::Relaxed) {
                tracing::info!(
                    decoded_frames = frames.len(),
                    "GIF 解码被取消，返回已解码的帧"
                );
                break;
            }
        }

        if frames.len() >= max_frames {
            tracing::warn!(
                max_frames,
                memory_budget_mb = max_memory_mb,
                "GIF 帧数超过内存预算限制，已截断"
            );
            break;
        }

        let frame = frame_result.map_err(|e| {
            MirrorStarError::ImageDecode(format!("GIF 帧读取失败 (第 {} 帧): {}", frames.len(), e))
        })?;

        // #3: 流式窗口外帧跳过 process_gif_frame（降采样）——仅提取元数据（delay_ms/width/height），
        // 省 O(W*H) 像素遍历。这些帧的 pixels 本会被 v18 流式窗口逻辑立即清空，
        // 降采样是纯浪费。与 prefetch_with_cursor 的 "cursor < window_start：
        // 仅推进游标，丢弃 frame（省降采样）" 同理。
        //
        // 元数据说明：窗口外帧的 width/height 为降采样前尺寸（未调用 process_gif_frame），
        // 但这些值从不用于渲染——空像素帧被 WM_PAINT 守卫跳过，reload 时由
        // decode_single_frame_with_cursor / handle_frames_prefetched 覆盖。仅有
        // delay_ms 被消费（WM_TIMER 帧推进定时器），与 process_gif_frame 计算一致。
        // 首帧始终走 process_gif_frame（需准确尺寸算 max_frames），不进此快路径。
        let idx = frames.len();
        let outside = streaming_center
            .map(|c| is_outside_streaming_window(idx, c))
            .unwrap_or(false);
        let gif_frame = if outside {
            let delay_ms = frame_delay_ms(frame.delay());
            let (width, height) = frame.buffer().dimensions();
            GifFrame {
                pixels: Vec::new(),
                width,
                height,
                delay_ms,
            }
        } else {
            process_gif_frame(frame, screen_w, screen_h)
        };
        frames.push(gif_frame);
    }

    // 计算并记录实际内存使用量
    let total_memory_bytes: usize = frames.iter().map(|f| f.pixels.len()).sum();
    let total_memory_mb = total_memory_bytes as f64 / (1024.0 * 1024.0);
    if let Some(center) = streaming_center {
        let retained = frames
            .iter()
            .filter(|f| !f.pixels.is_empty())
            .count();
        tracing::info!(
            frame_count = frames.len(),
            retained_pixel_frames = retained,
            memory_mb = format!("{:.1}", total_memory_mb),
            budget_mb = max_memory_mb,
            streaming_center = center,
            "v18: 流式窗口解码完成，仅窗口内帧保留像素"
        );
    } else {
        tracing::info!(
            frame_count = frames.len(),
            memory_mb = format!("{:.1}", total_memory_mb),
            budget_mb = max_memory_mb,
            "GIF 解码完成，内存使用统计"
        );
    }

    Ok(frames)
}

/// 解码 GIF 文件的首帧（Task 8.1：首帧快速显示）。
///
/// 与 `decode_gif` 的区别：仅解码第一帧，用于在创建窗口后立即显示首帧，
/// 剩余帧由后台线程解码（参见 `gif::gif_wallpaper_thread` 中的后台解码逻辑）。
/// 降采样逻辑与 `decode_gif` 一致，确保首帧与全量解码结果一致。
pub(crate) fn decode_gif_first_frame(path: &str) -> Result<GifFrame, MirrorStarError> {
    // W12: 流式读取，BufReader 包装 File 避免一次性读入全部文件内容
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    // v5.0 W-PERF-003: 使用缓存避免首帧解码每次都调用 GetSystemMetrics
    let (screen_w, screen_h) = super::get_screen_size();

    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 解码初始化失败: {}", e)))?;
    let mut frames_iter = decoder.into_frames();

    let frame = frames_iter
        .next()
        .ok_or_else(|| MirrorStarError::ImageDecode("GIF 无有效帧".to_string()))?
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 帧读取失败 (首帧): {}", e)))?;

    let frame = process_gif_frame(frame, screen_w, screen_h);
    tracing::info!(path = %path, width = frame.width, height = frame.height, "GIF 首帧解码完成");
    Ok(frame)
}

/// v16-C-010: 检测 GIF 首帧（降采样后）像素数据是否超过预取阈值（8MB）。
///
/// 供 `set_wallpaper` 命令层在创建 `GifRenderer` 前提前检测 4K GIF 场景：首帧
/// 超 8MB 阈值意味着所有帧都会被 `decode_gif_frame_range` 的 v10-C 跳过逻辑
/// 跳过预取，触发 v15-B-005 同步解码兜底，播放帧率下降。此时由调用方 emit
/// warning 提示用户"GIF 分辨率过高，播放可能不流畅"。
///
/// 与 `decode_gif_frame_range` 的跳过判断（`frame_size > MAX_PREFETCH_FRAME_BYTES`）
/// 使用同一阈值与同一降采样逻辑（`decode_gif_first_frame` → `process_gif_frame`），
/// 确保预测结果与实际跳过行为一致。
///
/// # 返回值
///
/// - `true`：首帧像素数据 > 8MB 阈值，预测会触发 v10-C 跳过 + v15-B-005 兜底
/// - `false`：首帧未超阈值，或解码失败（不阻塞 `set_wallpaper`，错误仅 `tracing::warn`）
///
/// # 性能说明
///
/// 本函数会完整解码首帧（含降采样），与 `GifRenderer::play` 内的首帧解码重复。
/// 首帧解码典型耗时 <100ms（4K GIF ~200-500ms），`set_wallpaper` 为非高频操作，
/// 重复解码开销可接受。调用方应在 `spawn_blocking` 线程内调用以避免阻塞 runtime。
pub fn gif_first_frame_oversized(path: &str) -> bool {
    match decode_gif_first_frame(path) {
        Ok(frame) => {
            let size = frame.width as usize * frame.height as usize * 4;
            if size > MAX_PREFETCH_FRAME_BYTES {
                tracing::info!(
                    path = %path,
                    width = frame.width,
                    height = frame.height,
                    frame_size_mb = size / (1024 * 1024),
                    "v16-C-010: GIF 首帧超 8MB 阈值，预测播放将触发同步解码兜底"
                );
                true
            } else {
                false
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path,
                "v16-C-010: GIF 首帧解码失败，跳过超阈值检测"
            );
            false
        }
    }
}

/// v8-C: 按索引解码 GIF 文件的单个帧。
///
/// 用于流式帧缓存场景：当 `WM_TIMER` 推进到像素已被清空的帧时，调用本函数
/// 从文件重新解码该帧的像素数据。降采样逻辑与 `decode_gif` 一致，
/// 确保按需解码结果与全量解码结果可互换。
///
/// # 性能说明
///
/// GIF 帧使用增量编码（每帧基于前帧差异），image crate 的 `Frames` 迭代器
/// 无法跳过中间帧。因此本函数需从第 0 帧顺序解码到 `frame_index`，复杂度
/// O(frame_index)。典型 GIF 帧数 < 100、单帧解码 < 10ms，可接受。
///
/// #1 后：生产预取路径已改用 [`prefetch_with_cursor`]（持久化解码游标，
/// O(half) 而非 O(target+half)），消除了主动预取的重复解码。本函数因每次
/// 调用都重新打开文件并从第 0 帧解码，仍为 O(target)；保留为同步兜底
/// （`gif_memory::reload_current_frame_pixels`，#1 后罕见但触发即卡顿）
/// 与测试/benchmark 基准对照使用。消除此 O(target) 兜底开销见候选优化
/// `reload_current_frame_pixels` 改造。
///
/// # 参数
///
/// - `path`: GIF 文件路径
/// - `frame_index`: 目标帧索引（0-based）
/// - `max_memory_mb`: 内存预算（仅用于日志上下文，单帧解码不触发预算截断）
///
/// # 错误
///
/// - 文件不存在 / 无法打开：返回 `MirrorStarError::Io`
/// - GIF 格式错误：返回 `MirrorStarError::ImageDecode`
/// - `frame_index` 超出总帧数：返回 `MirrorStarError::ImageDecode`
#[allow(dead_code)]
pub(crate) fn decode_gif_frame_at(
    path: &str,
    frame_index: usize,
    max_memory_mb: usize,
) -> Result<GifFrame, MirrorStarError> {
    // W12: 流式读取，BufReader 包装 File 避免一次性读入全部文件内容
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    // v5.0 W-PERF-003: 使用缓存避免按需解码每次都调用 GetSystemMetrics
    let (screen_w, screen_h) = super::get_screen_size();

    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 解码初始化失败: {}", e)))?;
    let frames_iter = decoder.into_frames();

    for (current, frame_result) in frames_iter.enumerate() {
        let frame = frame_result.map_err(|e| {
            MirrorStarError::ImageDecode(format!("GIF 帧读取失败 (第 {} 帧): {}", current, e))
        })?;
        if current == frame_index {
            let result = process_gif_frame(frame, screen_w, screen_h);
            tracing::debug!(
                frame_index,
                width = result.width,
                height = result.height,
                max_memory_mb,
                "v8-C: 按需解码单帧完成"
            );
            return Ok(result);
        }
    }

    Err(MirrorStarError::ImageDecode(format!(
        "GIF 帧索引 {} 超出范围（文件总帧数 <= {}）",
        frame_index, frame_index
    )))
}

/// v9-A: 按索引范围一次性解码 GIF 文件的多个连续帧。
///
/// 用于后台预取：当 `WM_TIMER` 推进到空像素帧时，后台线程调用本函数
/// 一次性解码 `[start, end]` 范围内的所有帧（含两端），避免对每个帧
/// 分别调用 `decode_gif_frame_at` 导致的 (2*HALF+1)× 重复解码开销（每帧都从 0
/// 顺序解码到目标索引）。
///
/// # 性能说明
///
/// 与 `decode_gif_frame_at` 一样需从第 0 帧顺序解码到 `end`（GIF 增量
/// 编码限制），但中途收集 `[start, end]` 范围内的帧，复杂度 O(end)
/// 而非 O((2*HALF+1)×end)。
///
/// #1 后：生产路径已改用 [`prefetch_with_cursor`]（持久化解码游标，O(half) 而非
/// O(target+half)），本函数仅测试模块使用（作为范围解码基准对照）。保留为
/// `pub(crate)` 供跨模块测试与 benchmark 使用。
#[allow(dead_code)]
pub(crate) fn decode_gif_frame_range(
    path: &str,
    start: usize,
    end: usize,
    max_memory_mb: usize,
) -> Result<Vec<GifFrame>, MirrorStarError> {
    if start > end {
        return Err(MirrorStarError::ImageDecode(format!(
            "GIF 帧范围无效: start={} > end={}",
            start, end
        )));
    }

    // W12: 流式读取，BufReader 包装 File 避免一次性读入全部文件内容
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    // v5.0 W-PERF-003: 使用缓存避免按需解码每次都调用 GetSystemMetrics
    let (screen_w, screen_h) = super::get_screen_size();

    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 解码初始化失败: {}", e)))?;
    let frames_iter = decoder.into_frames();

    let mut result: Vec<GifFrame> = Vec::with_capacity(end.saturating_sub(start) + 1);
    for (current, frame_result) in frames_iter.enumerate() {
        if current > end {
            break;
        }
        // 始终消费 frame_result 以推进迭代器（GIF 增量编码需顺序解码）
        let frame = frame_result.map_err(|e| {
            MirrorStarError::ImageDecode(format!("GIF 帧读取失败 (第 {} 帧): {}", current, e))
        })?;
        if current >= start {
            let frame = process_gif_frame(frame, screen_w, screen_h);
            // v10-C: 单帧像素数据超过 8MB 则跳过预取（仅当前帧同步解码）
            // 避免 4K GIF（每帧 ~6MB）× (2*STREAMING_WINDOW_HALF+1) 帧造成瞬时内存尖峰
            let frame_size = frame.width as usize * frame.height as usize * 4;
            if frame_size > MAX_PREFETCH_FRAME_BYTES {
                tracing::debug!(
                    frame_index = current,
                    frame_size_mb = frame_size / (1024 * 1024),
                    "v10-C: 帧过大，跳过预取"
                );
                continue;
            }
            result.push(frame);
        }
    }

    if result.is_empty() {
        return Err(MirrorStarError::ImageDecode(format!(
            "GIF 帧范围 [{}, {}] 无有效帧（start 可能超出总帧数）",
            start, end
        )));
    }

    tracing::debug!(
        start,
        end,
        decoded_count = result.len(),
        max_memory_mb,
        "v9-A: 范围解码完成"
    );
    Ok(result)
}

/// #1: 预取请求（跨线程发送到 worker）。
///
/// `target` 为预取窗口中心；`half` 为窗口半幅（前后各 `half` 帧），
/// 通常等于 [`STREAMING_WINDOW_HALF`]。worker 线程收到请求后调用
/// [`prefetch_with_cursor`] 执行解码。
pub(crate) struct PrefetchRequest {
    pub target: usize,
    pub half: usize,
}

/// #1: 打开 GIF 文件并创建 `'static` 帧迭代器。
///
/// `File: 'static` + `BufReader<File>: 'static` + `GifDecoder<...>: 'static`
/// → `into_frames(): Frames<'static>`。worker 线程将其作为局部变量持有，
/// 局部变量无需 `Send`，因此 `Frames` 非 `Send` 不影响线程创建。
fn open_gif_frames(path: &str) -> Result<image::Frames<'static>, MirrorStarError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|e| MirrorStarError::ImageDecode(format!("GIF 解码初始化失败: {}", e)))?;
    Ok(decoder.into_frames())
}

/// #1: 持久化解码游标的预取实现——消除 O(N) 重解码。
///
/// 与 [`decode_gif_frame_range`] 不同，本函数复用调用方持有的 `frames_iter`+
/// `cursor`：worker 线程局部变量，跨多次预取请求保留解码状态。
///
/// # 复杂度
///
/// - **前向前进**（`cursor <= window_end`）：从 `cursor` 解码到 `window_end`，
///   `O(window_end - cursor) ≤ O(half)`。
/// - **回绕**（`window_end < cursor`，整个窗口在游标前，常见于 GIF 循环重置）：
///   重新打开 GIF，`cursor` 重置为 0，解码 `0..=window_end`。典型 GIF 循环后
///   `target` 较小（接近 0），故 `O(target+half) ≈ O(half)`。
/// - 旧实现每次预取都从 0 解码到 `window_end = O(target+half)`，前向播放每循环
///   N-1 前向 + 1 回绕均为 O(N)，总复杂度 O(N²)；新实现总复杂度 O(N)。
///
/// # 窗口跳过逻辑
///
/// - `cursor >= window_start` 的帧：解码 + [`process_gif_frame`] + v10-C 跳过超大帧
///   （>[`MAX_PREFETCH_FRAME_BYTES`]）后收集到返回 Vec。
/// - `cursor < window_start` 的帧：仅 `iter.next()` 推进游标，不调用
///   [`process_gif_frame`]（省降采样成本；这些帧的像素已由前次预取
///   填充到主线程渲染器，本次仅需推进游标以到达窗口起点）。
///
/// # 参数
///
/// - `path`: GIF 文件路径（仅在 `need_open` 时使用）
/// - `screen_w`, `screen_h`: 调用方查询的屏幕分辨率（处理 DPI 变化）
/// - `frames_iter`: worker 持有的帧迭代器，`None` 表示尚未打开或已被重置
/// - `cursor`: worker 持有的游标，表示 `frames_iter` 当前指向的下一帧索引
/// - `target`: 预取目标帧索引（窗口中心）
/// - `half`: 窗口半幅
///
/// # 返回
///
/// 窗口 `[target-half, target+half]` 内有效帧的 `(帧索引, GifFrame)` 列表。
/// 空列表表示窗口内所有帧均超 8MB 阈值被跳过，或解码出错（错误已 `tracing` 记录）。
pub(crate) fn prefetch_with_cursor(
    path: &str,
    screen_w: u32,
    screen_h: u32,
    frames_iter: &mut Option<image::Frames<'static>>,
    cursor: &mut usize,
    target: usize,
    half: usize,
) -> Vec<(usize, GifFrame)> {
    let window_start = target.saturating_sub(half);
    let window_end = target + half; // inclusive

    // 判断是否需要重新打开 GIF：
    // - frames_iter 为 None（首次调用或上次出错已重置）
    // - window_end < *cursor（回绕：整个窗口在游标前，必须从 0 重新解码）
    let need_open = frames_iter.is_none() || window_end < *cursor;
    if need_open {
        match open_gif_frames(path) {
            Ok(iter) => {
                *frames_iter = Some(iter);
                *cursor = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "#1: 打开 GIF 失败，预取返回空");
                *frames_iter = None;
                *cursor = 0;
                return Vec::new();
            }
        }
    }

    let iter = match frames_iter.as_mut() {
        Some(it) => it,
        None => return Vec::new(), // 理论不可达（need_open 失败已 return）
    };

    let mut result: Vec<(usize, GifFrame)> = Vec::with_capacity(2 * half + 1);
    // 循环到 window_end：cursor >= window_start 的帧收集，cursor < window_start 的帧仅推进
    while *cursor <= window_end {
        let current = *cursor;
        match iter.next() {
            Some(Ok(frame)) => {
                if current >= window_start {
                    let f = process_gif_frame(frame, screen_w, screen_h);
                    let frame_size = f.width as usize * f.height as usize * 4;
                    if frame_size > MAX_PREFETCH_FRAME_BYTES {
                        tracing::debug!(
                            frame_index = current,
                            frame_size_mb = frame_size / (1024 * 1024),
                            "v10-C: 帧过大，跳过预取"
                        );
                    } else {
                        result.push((current, f));
                    }
                }
                // cursor < window_start：仅推进游标，丢弃 frame（省降采样）
            }
            Some(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    frame_index = current,
                    "#1: 帧读取失败，预取中止"
                );
                // 解码错误：重置迭代器，下次预取重新打开
                *frames_iter = None;
                *cursor = 0;
                break;
            }
            None => {
                // 迭代器耗尽（current 之后无更多帧）。不重置：下次回绕请求
                // （window_end < cursor）会自然触发 need_open 重新打开。
                tracing::debug!(
                    frame_index = current,
                    window_end,
                    "#1: GIF 帧迭代器耗尽，已到达末帧"
                );
                break;
            }
        }
        *cursor += 1;
    }

    tracing::debug!(
        target,
        half,
        window_start,
        window_end,
        decoded_count = result.len(),
        "#1: prefetch_with_cursor 完成"
    );
    result
}

/// #2: 持久化解码游标的单帧解码——消除 sync 兜底 O(target) 重解码。
///
/// 取代 [`decode_gif_frame_at`] 在 `reload_current_frame_pixels` 中的用途。
/// 与 [`decode_gif_frame_at`]（每次从 0 解码到 target，O(target)）不同，本函数
/// 复用调用方持有的 `frames_iter` + `cursor`：主线程 `GifRenderData` 局部变量，
/// 跨多次 `reload_current_frame_pixels` 调用保留解码状态。
///
/// 与 [`prefetch_with_cursor`] 的区别：
/// - 仅返回 target 帧（非窗口），用于同步兜底解码当前帧
/// - 不应用 v10-C 8MB 跳过（sync 兜底需处理 4K 帧，与 `decode_gif_frame_at` 一致）
///
/// # 复杂度
///
/// - **前向前进**（`cursor <= target`）：从 `cursor` 解码到 `target`，
///   `O(target - cursor)`。典型 cursor ≈ target（上次 sync 兜底位置 + 1），故 O(1)。
/// - **回绕**（`target < cursor`，GIF 循环重置）：重新打开 GIF，`cursor` 重置为 0，
///   解码 `0..=target`，O(target)。
/// - **首次调用**（`frames_iter` 为 None）：打开 GIF，O(target)。
///
/// 旧 `decode_gif_frame_at` 每次调用 O(target)；本函数首次 O(target)，后续 O(1)。
///
/// # 参数
///
/// - `path`: GIF 文件路径（仅在 `need_open` 时使用）
/// - `screen_w`, `screen_h`: 调用方查询的屏幕分辨率（处理 DPI 变化）
/// - `frames_iter`: 调用方持有的帧迭代器，`None` 表示尚未打开或已被重置
/// - `cursor`: 调用方持有的游标，表示 `frames_iter` 当前指向的下一帧索引
/// - `target`: 目标帧索引
///
/// # 返回
///
/// `Some(GifFrame)` 表示成功解码；`None` 表示失败（文件打开失败 / 帧读取错误 /
/// 索引超出范围，错误已 `tracing` 记录）。
pub(crate) fn decode_single_frame_with_cursor(
    path: &str,
    screen_w: u32,
    screen_h: u32,
    frames_iter: &mut Option<image::Frames<'static>>,
    cursor: &mut usize,
    target: usize,
) -> Option<GifFrame> {
    // 判断是否需要重新打开 GIF：
    // - frames_iter 为 None（首次调用或上次出错已重置）
    // - target < *cursor（回绕：目标帧在游标前，必须从 0 重新解码）
    let need_open = frames_iter.is_none() || target < *cursor;
    if need_open {
        match open_gif_frames(path) {
            Ok(iter) => {
                *frames_iter = Some(iter);
                *cursor = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "#2: 打开 GIF 失败，sync 兜底返回 None");
                *frames_iter = None;
                *cursor = 0;
                return None;
            }
        }
    }

    // 理论不可达（need_open 失败已 return）；保留 ? 防御 None 残留。
    let iter = frames_iter.as_mut()?;

    // 推进游标到 target：cursor < target 的帧仅 next() 推进（省降采样），
    // cursor == target 的帧解码 + process_gif_frame 后返回。
    while *cursor <= target {
        let current = *cursor;
        match iter.next() {
            Some(Ok(frame)) => {
                if current == target {
                    let result = process_gif_frame(frame, screen_w, screen_h);
                    tracing::debug!(frame_index = target, "#2: sync 兜底单帧解码完成");
                    *cursor += 1;
                    return Some(result);
                }
                // cursor < target：仅推进游标，丢弃 frame（省降采样）
            }
            Some(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    frame_index = current,
                    "#2: 帧读取失败，sync 兜底中止"
                );
                *frames_iter = None;
                *cursor = 0;
                return None;
            }
            None => {
                tracing::debug!(
                    frame_index = current,
                    target,
                    "#2: GIF 帧迭代器耗尽，target 超出总帧数"
                );
                return None;
            }
        }
        *cursor += 1;
    }
    None
}

/// 从 `image::Delay` 提取毫秒延迟，处理零延迟与无效分母。
///
/// #3: 抽取自 `process_gif_frame` 的延迟计算逻辑，供流式解码的元数据快速
/// 提取路径（窗口外帧跳过 process_gif_frame（降采样））复用，确保窗口内/外帧的 `delay_ms`
/// 计算完全一致。
fn frame_delay_ms(delay: image::Delay) -> u32 {
    let (numer, denom) = delay.numer_denom_ms();
    let delay_ms = if denom > 0 {
        (numer as f64 / denom as f64) as u32
    } else {
        100
    };
    if delay_ms == 0 { 100 } else { delay_ms }
}

/// 处理单帧：解析延迟、按需降采样。像素保留 RGBA 字节序（GDI 经 BI_BITFIELDS
/// 掩码直接解释，无需 RGBA→BGRA 转换）。
fn process_gif_frame(frame: image::Frame, screen_w: u32, screen_h: u32) -> GifFrame {
    let delay_ms = frame_delay_ms(frame.delay());

    let img = frame.into_buffer();
    let (width, height) = img.dimensions();

    // 如果帧尺寸超过屏幕分辨率，降采样到屏幕尺寸以减少内存占用。
    // 像素保留 image crate 输出的 RGBA 字节序，GDI 端用 BI_BITFIELDS 解释。
    let (final_width, final_height, pixels) = if width > screen_w || height > screen_h {
        let thumb =
            image::imageops::thumbnail(&image::DynamicImage::ImageRgba8(img), screen_w, screen_h);
        let (tw, th) = thumb.dimensions();
        (tw, th, thumb.into_raw())
    } else {
        (width, height, img.into_raw())
    };

    GifFrame {
        pixels,
        width: final_width,
        height: final_height,
        delay_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// v10-C: 序列化使用 `set_screen_size_for_test` 的测试，避免并行运行时
    /// 全局屏幕尺寸缓存互相干扰（一个测试 invalidate 时另一个测试正在读取）。
    static SCREEN_SIZE_TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// 最小有效 GIF（1x1 像素，GIF89a，含 2 色全局颜色表）
    const MINIMAL_GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // GIF89a
        0x01, 0x00, 0x01, 0x00, // 宽度1, 高度1
        0x80, 0x00, 0x00, // packed: GCT flag=1, 2 色; 背景色=0; 像素比例=0
        0x00, 0x00, 0x00, // GCT 颜色 0: 黑色
        0xFF, 0xFF, 0xFF, // GCT 颜色 1: 白色
        0x2C, // Image Descriptor
        0x00, 0x00, 0x00, 0x00, // 左上角
        0x01, 0x00, 0x01, 0x00, // 宽高
        0x00, // 无 LCT
        0x02, 0x02, 0x4C, 0x01, 0x00, // 最小 LZW 数据
        0x3B, // Trailer
    ];

    #[test]
    fn test_decode_valid_minimal_gif() {
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("有效 GIF 应解码成功");
        assert_eq!(frames.len(), 1, "最小 GIF 应有 1 帧");
        let frame = &frames[0];
        assert_eq!(frame.width, 1);
        assert_eq!(frame.height, 1);
        // RGBA 像素：1x1 = 4 字节
        assert_eq!(frame.pixels.len(), 4);
    }

    #[test]
    fn test_decode_corrupted_gif() {
        // 截断的 GIF 数据（仅前 10 字节，缺少完整结构）
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "损坏的 GIF 应返回错误而非 panic");
    }

    #[test]
    fn test_decode_empty_data() {
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("empty.gif");
        std::fs::write(&gif_path, b"").unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "空数据应返回错误而非 panic");
    }

    #[test]
    fn test_decode_nonexistent_file() {
        let result = decode_gif(
            "Z:\\nonexistent\\path\\no_such_file.gif",
            DEFAULT_MAX_GIF_MEMORY_MB,
        );
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    // ========== decode_gif_first_frame tests (Task 8.1) ==========

    #[test]
    fn test_decode_first_frame_valid_minimal_gif() {
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let frame = decode_gif_first_frame(path).expect("有效 GIF 首帧应解码成功");
        assert_eq!(frame.width, 1);
        assert_eq!(frame.height, 1);
        assert_eq!(frame.pixels.len(), 4, "1x1 RGBA = 4 字节");
        assert!(frame.delay_ms > 0, "延迟应为正数（0 会被替换为 100）");
    }

    #[test]
    fn test_decode_first_frame_matches_decode_gif_first() {
        // 首帧解码结果应与全量解码的第一帧完全一致
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let all_frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        let first_frame = decode_gif_first_frame(path).expect("首帧解码应成功");

        assert_eq!(all_frames[0].width, first_frame.width);
        assert_eq!(all_frames[0].height, first_frame.height);
        assert_eq!(all_frames[0].delay_ms, first_frame.delay_ms);
        assert_eq!(
            all_frames[0].pixels, first_frame.pixels,
            "首帧像素数据应与全量解码一致"
        );
    }

    #[test]
    fn test_decode_first_frame_corrupted_gif() {
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_first_frame(path);
        assert!(result.is_err(), "损坏的 GIF 首帧解码应返回错误");
    }

    #[test]
    fn test_decode_first_frame_nonexistent_file() {
        let result = decode_gif_first_frame("Z:\\nonexistent\\path\\no_such_file.gif");
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    // ========== W03 修复测试：小帧 GIF max_frames 计算 ==========

    #[test]
    fn test_decode_small_frame_gif_not_over_truncated() {
        // W03: 小帧 GIF 不应基于屏幕分辨率过度截断 max_frames
        // 创建 5 帧 1×1 GIF，旧逻辑以屏幕分辨率（如 1920×1080）估算每帧大小，
        // max_frames ≈ 40MB / 8.3MB ≈ 4，会截断到 4 帧；新逻辑基于实际帧尺寸
        // （1×1×4 = 4 字节），max_frames = 1000（cap），5 帧全部保留。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("small_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder); // 确保写入 GIF trailer

        let path = gif_path.to_str().unwrap();
        let frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("解码应成功");
        assert_eq!(
            frames.len(),
            5,
            "5 帧 1×1 GIF 应全部解码，不应被屏幕分辨率截断"
        );
    }

    #[test]
    fn test_decode_large_frame_gif_truncated_by_budget() {
        // W03: 大帧 GIF 仍应受内存预算限制截断
        // 创建 5 帧 1920×1080 GIF，每帧 8.3MB，5 帧 = 41.5MB > 40MB 预算，
        // max_frames = 40MB / 8.3MB ≈ 4，应截断到 4 帧。
        //
        // v8-C: 此处使用显式 40MB 预算而非 DEFAULT_MAX_GIF_MEMORY_MB（已从 40 降至 15），
        // 保持本测试对预算截断行为的覆盖不变。15MB 默认值下大帧 GIF 仅解码 1 帧
        // （由 test_decode_large_frame_gif_truncated_by_default_budget 覆盖）。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("large_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        // 1920×1080 RGBA = 8294400 字节/帧
        let pixels = vec![0u8; 1920 * 1080 * 4];
        for _ in 0..5 {
            encoder
                .encode(&pixels, 1920, 1080, ExtendedColorType::Rgba8)
                .expect("编码大帧 GIF 应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        // 显式 40MB 预算：max_frames = floor(40MB / 8294400) = floor(4.82) = 4
        let explicit_budget_mb = 40;
        let frames = decode_gif(path, explicit_budget_mb).expect("解码应成功");
        assert!(frames.len() <= 5, "大帧 GIF 帧数不应超过 max_frames 上限");
        assert!(
            frames.len() >= 4,
            "大帧 GIF 至少应解码到预算允许的帧数（~4 帧）"
        );
    }

    #[test]
    fn test_decode_large_frame_gif_truncated_by_default_budget() {
        // v8-C: 验证降低后的 DEFAULT_MAX_GIF_MEMORY_MB=15 对大帧 GIF 的截断行为。
        // 5 帧 1920×1080 GIF，每帧 8.3MB，15MB 预算下 max_frames = floor(15MB/8.3MB) = 1，
        // 仅解码首帧（循环首帧检查 frames.len() >= max_frames 后即退出）。
        //
        // 注意：测试机屏幕分辨率可能 < 1920×1080 导致降采样，使 frame_size_bytes 变小、
        // max_frames 变大。因此本测试强制 SCREEN_SIZE 缓存为 3840×2160（大于帧尺寸），
        // 确保 process_gif_frame 不触发降采样，使预算计算基于原始 1920×1080 尺寸。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        // v10-C: 获取互斥锁，序列化使用 set_screen_size_for_test 的测试
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        // 强制屏幕分辨率为 3840×2160，避免 1920×1080 帧被降采样
        super::super::set_screen_size_for_test(3840, 2160);

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("large_5frames_default_budget.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        let pixels = vec![0u8; 1920 * 1080 * 4];
        for _ in 0..5 {
            encoder
                .encode(&pixels, 1920, 1080, ExtendedColorType::Rgba8)
                .expect("编码大帧 GIF 应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("解码应成功");
        // 15MB / 8.3MB ≈ 1.80 → max_frames = 1，首帧后即截断
        assert_eq!(
            frames.len(),
            1,
            "v8-C: 15MB 默认预算下大帧 GIF 应仅解码首帧，实际 {}",
            frames.len()
        );

        // 恢复缓存，避免影响后续测试
        super::super::invalidate_screen_size_cache();
    }

    // ========== W09 修复测试：decode_gif_with_cancel 取消机制 ==========

    #[test]
    fn test_decode_gif_with_cancel_before_first_frame_returns_error() {
        // W09: 取消标志在调用前已置 true，应在首帧解码前立即返回 Err，
        // 不读取任何帧（窗口可能在解码线程启动前已销毁）。
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let cancel = AtomicBool::new(true);
        let result = decode_gif_with_cancel(path, Some(&cancel), DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "首帧前取消应返回 Err 而非 Ok");
        match result {
            Err(MirrorStarError::ImageDecode(msg)) => {
                assert!(msg.contains("取消"), "错误信息应包含'取消'，实际: {}", msg);
            }
            other => panic!("期望 ImageDecode 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_decode_gif_with_cancel_none_matches_decode_gif() {
        // W09: cancel=None 时行为应与 decode_gif 完全一致（返回相同帧数）
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let plain = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("decode_gif 应成功");
        let with_none = decode_gif_with_cancel(path, None, DEFAULT_MAX_GIF_MEMORY_MB)
            .expect("cancel=None 应成功");
        assert_eq!(
            plain.len(),
            with_none.len(),
            "cancel=None 应返回与 decode_gif 相同的帧数"
        );
    }

    #[test]
    fn test_decode_gif_with_cancel_mid_decode_returns_partial_frames() {
        // W09: 多帧 GIF 解码过程中设置取消标志，应在中途退出并返回已解码的帧（含首帧）。
        // 创建 100 帧 1×1 GIF，主线程在解码启动后设置取消标志，
        // 验证返回帧数 < 100 且 >= 1（首帧已解码）。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;
        use std::sync::Arc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("cancel_mid_100frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..100u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap().to_string();
        let cancel = Arc::new(AtomicBool::new(false));

        // 在子线程中解码，主线程设置取消标志。
        // 使用 started 标志同步：主线程等待解码线程已启动后再 sleep，确保
        // decode_gif_with_cancel 已进入函数体并通过首帧前的 cancel 检查点
        // （原实现仅 sleep 5ms，若线程调度延迟 >5ms 会导致 cancel 在首帧前触发，返回 Err）。
        let started = Arc::new(AtomicBool::new(false));
        let started_for_thread = started.clone();
        let cancel_for_thread = cancel.clone();
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            started_for_thread.store(true, Ordering::SeqCst);
            decode_gif_with_cancel(
                &path_for_thread,
                Some(&cancel_for_thread),
                DEFAULT_MAX_GIF_MEMORY_MB,
            )
        });

        // 自旋等待解码线程启动（started=true 表示线程已开始执行）
        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }

        // 给解码线程足够时间通过首帧前的 cancel 检查点并解码首帧：
        // decode_gif_with_cancel 首帧检查在文件打开 + GifDecoder 创建之后，
        // 这些步骤耗时微秒级，20ms 远超其完成时间，确保 cancel 不会在首帧前触发。
        // 即便线程在标志设置前已完成所有 100 帧解码，测试仍验证"取消不导致 panic/Err"。
        std::thread::sleep(Duration::from_millis(20));
        cancel.store(true, Ordering::Relaxed);

        let result = handle.join().expect("解码线程不应 panic");
        let frames = result.expect("取消后应返回 Ok（含已解码的帧），实际");
        // 取消后应返回部分帧：至少 1 帧（首帧必解码），至多 100 帧（若线程跑得极快）
        assert!(!frames.is_empty(), "取消后应至少返回首帧，实际 0 帧");
        assert!(
            frames.len() <= 100,
            "帧数不应超过 GIF 总帧数 100，实际 {}",
            frames.len()
        );
        // 若取消生效（线程未在 20ms 内完成全部 100 帧解码），帧数应 < 100
        // 此处不强制断言 < 100，因机器速度快时可能在标志设置前已完成全部解码；
        // 关键验证点：取消机制不导致 panic / Err / 数据损坏。
    }

    #[test]
    fn test_decode_gif_with_cancel_false_completes_full_decode() {
        // W09: 取消标志全程为 false 时，应解码全部帧（与无取消标志一致）。
        // 验证取消机制在未触发时不影响正常解码。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("no_cancel_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let cancel = AtomicBool::new(false);
        let frames = decode_gif_with_cancel(path, Some(&cancel), DEFAULT_MAX_GIF_MEMORY_MB)
            .expect("应成功解码全部 5 帧");
        assert_eq!(
            frames.len(),
            5,
            "cancel=false 时应解码全部 5 帧，实际 {}",
            frames.len()
        );
    }

    // ========== W12 修复测试：BufReader 流式读取大 GIF ==========

    #[test]
    fn test_decode_gif_streaming_large_multi_frame_gif() {
        // W12: 验证 BufReader<File> 流式读取能正确解码多帧大 GIF。
        // 创建 20 帧 100×100 GIF（每帧 40000 字节 RGBA，总 ~800KB），
        // 验证所有帧解码成功且尺寸/像素正确，不 panic。
        // BufReader 包装使 GifDecoder 增量读取而非一次性读入全部文件内容。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("streaming_20frames_100x100.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        // 每帧使用不同的基色，便于验证解码结果正确性
        for i in 0..20u8 {
            let pixels: Vec<u8> = [i, 100, 200, 255].repeat(100 * 100);
            encoder
                .encode(&pixels, 100, 100, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames =
            decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("流式解码 20 帧大 GIF 应成功");
        assert_eq!(frames.len(), 20, "应解码全部 20 帧");
        for (idx, frame) in frames.iter().enumerate() {
            assert_eq!(frame.width, 100, "第 {} 帧宽度应为 100", idx);
            assert_eq!(frame.height, 100, "第 {} 帧高度应为 100", idx);
            // RGBA 像素：100×100×4 = 40000 字节
            assert_eq!(
                frame.pixels.len(),
                40000,
                "第 {} 帧像素数据长度应为 40000",
                idx
            );
            // 验证首像素 RGBA [i,100,200,255]（GDI 经 BI_BITFIELDS 解释，无需转换）
            let expected_r = idx as u8;
            let expected_g = 100u8;
            let expected_b = 200u8;
            assert_eq!(frame.pixels[0], expected_r, "第 {} 帧首像素 R 分量", idx);
            assert_eq!(frame.pixels[1], expected_g, "第 {} 帧首像素 G 分量", idx);
            assert_eq!(frame.pixels[2], expected_b, "第 {} 帧首像素 B 分量", idx);
            assert_eq!(frame.pixels[3], 255, "第 {} 帧首像素 A 分量", idx);
        }
    }

    #[test]
    fn test_decode_gif_first_frame_streaming_large_gif() {
        // W12: 验证 decode_gif_first_frame 也使用 BufReader 流式读取，
        // 仅解码首帧而不读入后续帧数据。创建多帧 GIF，验证首帧解码正确。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("first_frame_streaming_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(50 * 50);
            encoder
                .encode(&pixels, 50, 50, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frame = decode_gif_first_frame(path).expect("首帧流式解码应成功");
        assert_eq!(frame.width, 50);
        assert_eq!(frame.height, 50);
        assert_eq!(frame.pixels.len(), 50 * 50 * 4, "50×50×4 = 10000 字节");
        // 首帧 RGBA [0,50,150,255]（GDI 经 BI_BITFIELDS 解释，无需转换）
        assert_eq!(frame.pixels[0], 0, "首帧首像素 R 分量");
        assert_eq!(frame.pixels[1], 50, "首帧首像素 G 分量");
        assert_eq!(frame.pixels[2], 150, "首帧首像素 B 分量");
        assert_eq!(frame.pixels[3], 255, "首帧首像素 A 分量");
    }

    #[test]
    fn test_decode_gif_streaming_does_not_panic_on_large_budget_ok() {
        // W12: 验证流式读取在内存预算允许的范围内正确解码大帧 GIF。
        // 创建 3 帧 1280×720 GIF（每帧 ~3.7MB，3 帧 ~11MB < 40MB 预算），
        // 验证全部 3 帧解码成功（不被预算截断），证明 BufReader 流式读取
        // 配合内存预算机制工作正常。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("streaming_3frames_1280x720.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        let pixels = vec![0u8; 1280 * 720 * 4];
        for _ in 0..3 {
            encoder
                .encode(&pixels, 1280, 720, ExtendedColorType::Rgba8)
                .expect("编码 1280×720 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames =
            decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("流式解码 3 帧 1280×720 GIF 应成功");
        // v8-C: 3 帧 × 3.52MB = 10.55MB < 15MB 默认预算，不应被截断
        assert_eq!(
            frames.len(),
            3,
            "3 帧 1280×720 GIF 在预算内应全部解码，实际 {}",
            frames.len()
        );
        for frame in &frames {
            assert_eq!(frame.width, 1280);
            assert_eq!(frame.height, 720);
            assert_eq!(frame.pixels.len(), 1280 * 720 * 4);
        }
    }

    // ========== v8-C: decode_gif_frame_at 按需单帧解码测试 ==========

    #[test]
    fn test_decode_gif_frame_at_first_frame_minimal() {
        // v8-C: 按索引 0 解码最小 GIF 的首帧
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let frame =
            decode_gif_frame_at(path, 0, DEFAULT_MAX_GIF_MEMORY_MB).expect("应成功解码第 0 帧");
        assert_eq!(frame.width, 1);
        assert_eq!(frame.height, 1);
        assert_eq!(frame.pixels.len(), 4, "1x1 RGBA = 4 字节");
    }

    #[test]
    fn test_decode_gif_frame_at_matches_decode_gif() {
        // v8-C: decode_gif_frame_at(N) 的结果应与 decode_gif 的第 N 帧完全一致
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("match_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            // 每帧使用不同基色，便于区分
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let all_frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        assert_eq!(all_frames.len(), 5);

        // 逐帧验证按需解码与全量解码结果一致
        for (idx, full) in all_frames.iter().enumerate().take(5) {
            let on_demand =
                decode_gif_frame_at(path, idx, DEFAULT_MAX_GIF_MEMORY_MB).expect("按需解码应成功");
            assert_eq!(on_demand.width, full.width, "第 {} 帧宽度应一致", idx);
            assert_eq!(on_demand.height, full.height, "第 {} 帧高度应一致", idx);
            assert_eq!(on_demand.delay_ms, full.delay_ms, "第 {} 帧延迟应一致", idx);
            assert_eq!(
                on_demand.pixels, full.pixels,
                "第 {} 帧像素数据应与全量解码一致",
                idx
            );
        }
    }

    #[test]
    fn test_decode_gif_frame_at_specific_frame_pixel_value() {
        // v8-C: 验证按需解码的像素值正确（RGBA 保持，GDI 经 BI_BITFIELDS 解释）
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("pixel_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            let pixels = vec![i, 0, 0, 255]; // RGBA: R=i, G=0, B=0, A=255
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        // 解码第 3 帧：RGBA [3,0,0,255]
        let frame =
            decode_gif_frame_at(path, 3, DEFAULT_MAX_GIF_MEMORY_MB).expect("应成功解码第 3 帧");
        assert_eq!(frame.pixels.len(), 4);
        assert_eq!(frame.pixels[0], 3, "R 分量应为 3");
        assert_eq!(frame.pixels[1], 0, "G 分量应为 0");
        assert_eq!(frame.pixels[2], 0, "B 分量应为 0");
        assert_eq!(frame.pixels[3], 255, "A 分量应为 255");
    }

    #[test]
    fn test_decode_gif_frame_at_out_of_range() {
        // v8-C: 帧索引超出范围应返回错误而非 panic
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_frame_at(path, 10, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "超出范围的帧索引应返回错误");
        match result {
            Err(MirrorStarError::ImageDecode(msg)) => {
                assert!(
                    msg.contains("超出范围"),
                    "错误信息应包含'超出范围'，实际: {}",
                    msg
                );
            }
            other => panic!("期望 ImageDecode 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_decode_gif_frame_at_nonexistent_file() {
        // v8-C: 不存在的文件应返回错误
        let result = decode_gif_frame_at(
            "Z:\\nonexistent\\path\\no_such_file.gif",
            0,
            DEFAULT_MAX_GIF_MEMORY_MB,
        );
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    #[test]
    fn test_decode_gif_frame_at_corrupted_gif() {
        // v8-C: 损坏的 GIF 应返回错误而非 panic
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_frame_at(path, 0, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "损坏的 GIF 应返回错误");
    }

    // ========== v9-A: decode_gif_frame_range 范围解码测试 ==========

    #[test]
    fn test_decode_gif_frame_range_single_frame_minimal() {
        // v9-A: 范围 [0, 0] 解码最小 GIF 的首帧
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let frames =
            decode_gif_frame_range(path, 0, 0, DEFAULT_MAX_GIF_MEMORY_MB).expect("应成功解码 1 帧");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].width, 1);
        assert_eq!(frames[0].height, 1);
        assert_eq!(frames[0].pixels.len(), 4, "1x1 RGBA = 4 字节");
    }

    #[test]
    fn test_decode_gif_frame_range_matches_decode_gif() {
        // v9-A: decode_gif_frame_range(start, end) 的结果应与 decode_gif 的
        // 第 [start, end] 帧完全一致（像素、尺寸、延迟）
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("range_match_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let all_frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        assert_eq!(all_frames.len(), 5);

        // 解码范围 [1, 3]，应返回 3 帧（索引 1, 2, 3）
        let range_frames =
            decode_gif_frame_range(path, 1, 3, DEFAULT_MAX_GIF_MEMORY_MB).expect("范围解码应成功");
        assert_eq!(range_frames.len(), 3, "范围 [1,3] 应返回 3 帧");

        // 验证每帧与全量解码结果一致
        for (offset, range_frame) in range_frames.iter().enumerate() {
            let full_index = 1 + offset;
            assert_eq!(
                range_frame.width, all_frames[full_index].width,
                "第 {} 帧宽度",
                full_index
            );
            assert_eq!(
                range_frame.height, all_frames[full_index].height,
                "第 {} 帧高度",
                full_index
            );
            assert_eq!(
                range_frame.delay_ms, all_frames[full_index].delay_ms,
                "第 {} 帧延迟",
                full_index
            );
            assert_eq!(
                range_frame.pixels, all_frames[full_index].pixels,
                "第 {} 帧像素数据应与全量解码一致",
                full_index
            );
        }
    }

    #[test]
    fn test_decode_gif_frame_range_full_range() {
        // v9-A: 范围 [0, end] 应返回与全量解码相同的帧
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("range_full_5frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..5u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let all_frames = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        let range_frames =
            decode_gif_frame_range(path, 0, 4, DEFAULT_MAX_GIF_MEMORY_MB).expect("范围解码应成功");
        assert_eq!(range_frames.len(), all_frames.len());
        for (i, (rf, af)) in range_frames.iter().zip(all_frames.iter()).enumerate() {
            assert_eq!(rf.pixels, af.pixels, "第 {} 帧像素应一致", i);
        }
    }

    #[test]
    fn test_decode_gif_frame_range_end_beyond_total_degrades_gracefully() {
        // v9-A: end 超出总帧数时优雅降级，返回 [start, 实际末帧] 范围的帧
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("range_beyond_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..3u8 {
            let pixels = vec![i, 0, 0, 255];
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        // GIF 只有 3 帧（索引 0,1,2），请求范围 [0, 10] 应返回 3 帧
        let range_frames =
            decode_gif_frame_range(path, 0, 10, DEFAULT_MAX_GIF_MEMORY_MB).expect("应成功解码");
        assert_eq!(
            range_frames.len(),
            3,
            "end 超出总帧数时应返回实际存在的 3 帧"
        );
    }

    #[test]
    fn test_decode_gif_frame_range_start_beyond_total_returns_error() {
        // v9-A: start 超出总帧数时返回错误（范围内无有效帧）
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_frame_range(path, 10, 20, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "start 超出总帧数应返回错误");
    }

    #[test]
    fn test_decode_gif_frame_range_invalid_start_gt_end() {
        // v9-A: start > end 应返回错误
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("minimal.gif");
        std::fs::write(&gif_path, MINIMAL_GIF).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_frame_range(path, 3, 1, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "start > end 应返回错误");
        match result {
            Err(MirrorStarError::ImageDecode(msg)) => {
                assert!(msg.contains("无效"), "错误信息应包含'无效'，实际: {}", msg);
            }
            other => panic!("期望 ImageDecode 错误，实际: {:?}", other),
        }
    }

    #[test]
    fn test_decode_gif_frame_range_nonexistent_file() {
        // v9-A: 不存在的文件应返回错误
        let result = decode_gif_frame_range(
            "Z:\\nonexistent\\path\\no_such_file.gif",
            0,
            0,
            DEFAULT_MAX_GIF_MEMORY_MB,
        );
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    #[test]
    fn test_decode_gif_frame_range_corrupted_gif() {
        // v9-A: 损坏的 GIF 应返回错误而非 panic
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let result = decode_gif_frame_range(path, 0, 0, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(result.is_err(), "损坏的 GIF 应返回错误");
    }

    // ========== v10-C: 单帧像素数据预取阈值测试 ==========

    #[test]
    fn test_max_prefetch_frame_bytes_threshold_value() {
        // v10-C: 验证预取阈值常量为 8MB
        assert_eq!(
            MAX_PREFETCH_FRAME_BYTES,
            8 * 1024 * 1024,
            "v10-C: MAX_PREFETCH_FRAME_BYTES 应为 8MB"
        );
    }

    #[test]
    fn test_decode_gif_frame_range_skips_oversized_frames() {
        // v10-C: 验证单帧像素数据超过 8MB 时被跳过预取。
        // 创建 3 帧 1500×1500 GIF（每帧 9MB > 8MB 阈值），全部应被跳过，
        // 返回空 Vec，函数返回错误（范围内无有效帧）。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        // v10-C: 获取互斥锁，序列化使用 set_screen_size_for_test 的测试
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        // 强制屏幕分辨率为 3840×2160，避免 1500×1500 帧被降采样
        // （降采样后帧尺寸变小，无法触发 8MB 阈值）
        super::super::set_screen_size_for_test(3840, 2160);

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("oversized_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        // 1500×1500×4 = 9,000,000 字节 ≈ 8.58 MB > 8 MB 阈值
        let pixels = vec![0u8; 1500 * 1500 * 4];
        for _ in 0..3 {
            encoder
                .encode(&pixels, 1500, 1500, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let result = decode_gif_frame_range(path, 0, 2, DEFAULT_MAX_GIF_MEMORY_MB);
        assert!(
            result.is_err(),
            "v10-C: 全部帧超 8MB 阈值应被跳过，返回错误"
        );

        // 恢复缓存，避免影响后续测试
        super::super::invalidate_screen_size_cache();
    }

    #[test]
    fn test_decode_gif_frame_range_keeps_frames_under_threshold() {
        // v10-C: 验证单帧像素数据低于 8MB 阈值时不被跳过。
        // 1920×1080×4 = 8,294,400 字节 ≈ 7.91 MB < 8 MB 阈值，应全部保留。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        // v10-C: 获取互斥锁，序列化使用 set_screen_size_for_test 的测试
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        // 强制屏幕分辨率为 3840×2160，避免 1920×1080 帧被降采样
        super::super::set_screen_size_for_test(3840, 2160);

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("under_threshold_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        let pixels = vec![0u8; 1920 * 1080 * 4];
        for _ in 0..3 {
            encoder
                .encode(&pixels, 1920, 1080, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames = decode_gif_frame_range(path, 0, 2, DEFAULT_MAX_GIF_MEMORY_MB)
            .expect("v10-C: 低于阈值的帧应成功解码");
        assert_eq!(frames.len(), 3, "v10-C: 3 帧均低于 8MB 阈值，应全部保留");
        for (idx, frame) in frames.iter().enumerate() {
            assert_eq!(frame.width, 1920, "第 {} 帧宽度应为 1920", idx);
            assert_eq!(frame.height, 1080, "第 {} 帧高度应为 1080", idx);
        }

        // 恢复缓存，避免影响后续测试
        super::super::invalidate_screen_size_cache();
    }

    // ========== v18: 流式窗口解码测试 ==========

    #[test]
    fn test_is_outside_streaming_window() {
        // (d): STREAMING_WINDOW_HALF=1，center=5 → 窗口 [4, 7)
        assert!(!is_outside_streaming_window(4, 5), "帧 4 在窗口内");
        assert!(!is_outside_streaming_window(5, 5), "帧 5（中心）在窗口内");
        assert!(!is_outside_streaming_window(6, 5), "帧 6 在窗口内");
        assert!(is_outside_streaming_window(3, 5), "帧 3 在窗口外");
        assert!(is_outside_streaming_window(7, 5), "帧 7 在窗口外");
        // center=0 → 窗口 [0, 2)
        assert!(!is_outside_streaming_window(0, 0), "帧 0 在窗口内");
        assert!(!is_outside_streaming_window(1, 0), "帧 1 在窗口内");
        assert!(is_outside_streaming_window(2, 0), "帧 2 在窗口外");
        // center 靠近开头：saturating_sub 防止下溢
        assert!(!is_outside_streaming_window(0, 1), "center=1 时帧 0 在窗口内");
    }

    #[test]
    fn test_decode_gif_streaming_center_5_clears_outside_window() {
        // (d): 10 帧 GIF，streaming_center=5，窗口 [4,7)。
        // 帧 4-6 保留像素，帧 0-3 和 7-9 像素应被清空（仅保留元数据）。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_center5_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 5).expect("流式解码应成功");
        assert_eq!(frames.len(), 10, "应解码全部 10 帧（保留元数据）");
        for (i, frame) in frames.iter().enumerate() {
            if (4..7).contains(&i) {
                assert!(
                    !frame.pixels.is_empty(),
                    "v18: 窗口内帧 {} 应保留像素，实际 len={}",
                    i,
                    frame.pixels.len()
                );
            } else {
                assert!(
                    frame.pixels.is_empty(),
                    "v18: 窗口外帧 {} 像素应已清空，实际 len={}",
                    i,
                    frame.pixels.len()
                );
            }
        }
    }

    #[test]
    fn test_decode_gif_streaming_center_0_clears_outside_window() {
        // (d): 10 帧 GIF，streaming_center=0，窗口 [0,2)。
        // 帧 0-1 保留像素，帧 2-9 像素应被清空。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_center0_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 0).expect("流式解码应成功");
        assert_eq!(frames.len(), 10);
        for (i, frame) in frames.iter().enumerate() {
            if i < 2 {
                assert!(!frame.pixels.is_empty(), "v18: 帧 {} 应保留像素", i);
            } else {
                assert!(frame.pixels.is_empty(), "v18: 帧 {} 像素应已清空", i);
            }
        }
    }

    #[test]
    fn test_decode_gif_streaming_in_window_pixels_match_full_decode() {
        // v18: 流式解码窗口内帧的像素应与全量解码完全一致
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_match_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let full = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        let streamed =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 5).expect("流式解码应成功");

        assert_eq!(full.len(), streamed.len(), "帧数应一致");
        // (d): 窗口 [4,7) 内的帧像素应完全匹配
        for i in 4..7 {
            assert_eq!(
                streamed[i].pixels,
                full[i].pixels,
                "v18: 窗口内帧 {} 像素应与全量解码一致",
                i
            );
            assert_eq!(streamed[i].width, full[i].width, "帧 {} 宽度应一致", i);
            assert_eq!(streamed[i].height, full[i].height, "帧 {} 高度应一致", i);
        }
    }

    #[test]
    fn test_decode_gif_streaming_preserves_metadata_for_cleared_frames() {
        // v18: 被清空像素的帧仍应保留 width/height/delay_ms 元数据，
        // 供 WM_TIMER 帧推进定时器使用（delay_ms 驱动播放节奏）。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_metadata_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let full = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        let streamed =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 0).expect("流式解码应成功");

        // 帧 5（窗口外）像素被清空，但元数据应保留
        assert!(streamed[5].pixels.is_empty(), "帧 5 像素应已清空");
        assert_eq!(streamed[5].width, full[5].width, "帧 5 宽度元数据应保留");
        assert_eq!(streamed[5].height, full[5].height, "帧 5 高度元数据应保留");
        assert_eq!(streamed[5].delay_ms, full[5].delay_ms, "帧 5 延迟元数据应保留");
    }

    #[test]
    fn test_decode_gif_streaming_few_frames_no_clearing() {
        // (d): 帧数 ≤ 窗口大小时，所有帧都在窗口内，不应清空任何像素。
        // 创建 3 帧 GIF（窗口大小 = 2*HALF+1 = 3），center=1，窗口 [0,3) 覆盖全部。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..3u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 1).expect("流式解码应成功");
        assert_eq!(frames.len(), 3);
        for (i, frame) in frames.iter().enumerate() {
            assert!(
                !frame.pixels.is_empty(),
                "v18: 帧数 ≤ 窗口大小时帧 {} 应保留像素",
                i
            );
        }
    }

    #[test]
    fn test_decode_gif_streaming_peak_memory_lower_than_full() {
        // (d): 验证流式解码的稳态像素内存低于全量解码。
        // 创建 20 帧 50×50 GIF（每帧 10KB），全量解码 20 帧像素 = 200KB，
        // 流式解码 center=10 仅保留窗口 [9,12) = 3 帧像素 = 30KB。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_streaming_peak_20frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..20u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(50 * 50);
            encoder
                .encode(&pixels, 50, 50, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let full = decode_gif(path, DEFAULT_MAX_GIF_MEMORY_MB).expect("全量解码应成功");
        let streamed =
            decode_gif_streaming(path, DEFAULT_MAX_GIF_MEMORY_MB, 10).expect("流式解码应成功");

        let full_mem: usize = full.iter().map(|f| f.pixels.len()).sum();
        let streamed_mem: usize = streamed.iter().map(|f| f.pixels.len()).sum();
        assert_eq!(full.len(), 20, "全量解码应有 20 帧");
        assert_eq!(streamed.len(), 20, "流式解码应保留 20 帧元数据");
        assert!(
            streamed_mem < full_mem,
            "v18: 流式解码像素内存 {} 应小于全量 {} ",
            streamed_mem,
            full_mem
        );
        // (d): 窗口 [9,12) = 3 帧 × 10000 字节 = 30000 字节
        assert_eq!(
            streamed_mem, 30000,
            "v18: 流式解码应保留 3 帧像素，实际 {}",
            streamed_mem
        );
    }

    #[test]
    fn test_decode_gif_with_cancel_streaming_center_zero() {
        // (d): decode_gif_with_cancel_streaming 与 cancel=None 应等价于
        // decode_gif_streaming。验证 cancel=None + center=0 时帧 0-1 保留像素。
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("v18_cancel_streaming_10frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..10u8 {
            let pixels: Vec<u8> = [i, 50, 150, 255].repeat(10 * 10);
            encoder
                .encode(&pixels, 10, 10, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);

        let path = gif_path.to_str().unwrap();
        let frames = decode_gif_with_cancel_streaming(path, None, DEFAULT_MAX_GIF_MEMORY_MB, 0)
            .expect("应成功");
        assert_eq!(frames.len(), 10);
        for (i, frame) in frames.iter().enumerate() {
            if i < 2 {
                assert!(!frame.pixels.is_empty(), "帧 {} 应保留像素", i);
            } else {
                assert!(frame.pixels.is_empty(), "帧 {} 应清空像素", i);
            }
        }
    }

    // ========== #1: prefetch_with_cursor 持久化解码游标测试 ==========

    /// 辅助：编码 N 帧 1×1 GIF（每帧 R 通道 = 帧索引），返回临时目录与路径。
    /// 索引通过首像素 RGBA 的 R 分量（pixels[0]）读取，便于断言"哪些帧被解码"。
    fn make_indexed_gif(n_frames: u8) -> (tempfile::TempDir, String) {
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("indexed.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for i in 0..n_frames {
            let pixels = vec![i, 0, 0, 255]; // RGBA: R=i
            encoder
                .encode(&pixels, 1, 1, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);
        let path = gif_path.to_str().unwrap().to_string();
        (dir, path)
    }

    /// #1: 首次预取——frames_iter=None 触发 need_open，从 0 解码到 window_end。
    #[test]
    fn test_prefetch_with_cursor_first_call_opens_gif() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // target=5, half=2 → window=[3,7]，应解码帧 3,4,5,6,7
        let result = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            5,
            2,
        );
        assert_eq!(result.len(), 5, "窗口 [3,7] 应返回 5 帧");
        let indices: Vec<usize> = result.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![3, 4, 5, 6, 7], "应包含窗口内全部 5 帧");
        // 游标应推进到 window_end+1=8
        assert_eq!(cursor, 8, "游标应推进到 8");
        assert!(frames_iter.is_some(), "迭代器应保留以复用");
    }

    /// #1: 前向前进——游标已在窗口起点之后，仅解码 delta 帧（O(half) 而非 O(N)）。
    #[test]
    fn test_prefetch_with_cursor_forward_advance_only_decodes_delta() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=3, half=2 → window=[1,5]，cursor: 0→6
        let r1 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 3, 2);
        assert_eq!(r1.len(), 5, "请求 1 应返回 5 帧 [1,5]");
        assert_eq!(cursor, 6);

        // 请求 2：target=4, half=2 → window=[2,6]。
        // cursor=6 >= window_start=2，故帧 6 被处理；帧 2-5 已由请求 1 填充（cursor 跳过）。
        // 期望仅返回帧 6（delta），而非全部 [2,6]。
        let r2 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 4, 2);
        assert_eq!(
            r2.len(),
            1,
            "请求 2 应仅返回 delta 帧 6（帧 2-5 已由请求 1 填充）"
        );
        assert_eq!(r2[0].0, 6, "请求 2 应返回帧 6");
        assert_eq!(cursor, 7, "游标应推进到 7");
    }

    /// #1: 窗口重叠跳过——连续两次预取窗口部分重叠，第二次不重解码已覆盖帧。
    #[test]
    fn test_prefetch_with_cursor_overlapping_window_skips_already_decoded() {
        let (_dir, path) = make_indexed_gif(20);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=5, half=2 → window=[3,7]，cursor: 0→8
        let r1 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 5, 2);
        assert_eq!(r1.len(), 5);
        assert_eq!(cursor, 8);

        // 请求 2：target=7, half=2 → window=[5,9]。
        // cursor=8 在窗口 [5,9] 内，仅处理帧 8,9（帧 5-7 已由请求 1 填充）。
        let r2 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 7, 2);
        assert_eq!(r2.len(), 2, "请求 2 应仅返回 delta 帧 8,9");
        let indices: Vec<usize> = r2.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![8, 9]);
    }

    /// #1: 回绕——window_end < cursor 触发重新打开 GIF，cursor 重置为 0。
    #[test]
    fn test_prefetch_with_cursor_rewind_reopens_gif() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=8, half=2 → window=[6,10]（实际 [6,9]，GIF 只有 10 帧）。
        // cursor: 0→6（skip 0-5）→ 6,7,8,9 处理 → cursor=10，iter.next()=None，break。
        // 游标停在 10（break 前未 ++）。
        let r1 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 8, 2);
        assert_eq!(r1.len(), 4, "请求 1 应返回帧 6,7,8,9");
        assert!(cursor >= 10, "游标应到达或超过 10");

        // 请求 2：target=2, half=2 → window=[0,4]，window_end=4 < cursor=10 → 回绕。
        // 重新打开 GIF，cursor=0，解码 0..=4 = 5 帧。
        let r2 = prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 2, 2);
        assert_eq!(r2.len(), 5, "回绕后应返回帧 0,1,2,3,4");
        let indices: Vec<usize> = r2.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
        assert_eq!(cursor, 5, "回绕后游标应推进到 5");
    }

    /// #1: 超大帧跳过——帧像素 >8MB 时不收集（v10-C 行为保持）。
    #[test]
    fn test_prefetch_with_cursor_skips_oversized_frames() {
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        super::super::set_screen_size_for_test(3840, 2160);

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("oversized_3frames.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        // 1500×1500×4 = 9MB > 8MB 阈值
        let pixels = vec![0u8; 1500 * 1500 * 4];
        for _ in 0..3 {
            encoder
                .encode(&pixels, 1500, 1500, ExtendedColorType::Rgba8)
                .expect("编码 GIF 帧应成功");
        }
        drop(encoder);
        let path = gif_path.to_str().unwrap().to_string();

        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result =
            prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 1, 2);
        assert!(
            result.is_empty(),
            "v10-C: 全部帧超 8MB 阈值应被跳过，返回空"
        );

        super::super::invalidate_screen_size_cache();
    }

    /// #1: 错误路径——文件不存在时 open_gif_frames 返回 Err，预取返回空 Vec。
    #[test]
    fn test_prefetch_with_cursor_nonexistent_file_returns_empty() {
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result = prefetch_with_cursor(
            "Z:\\nonexistent\\path\\no_such_file.gif",
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            5,
            2,
        );
        assert!(result.is_empty(), "不存在的文件应返回空 Vec");
        assert!(frames_iter.is_none(), "失败后迭代器应保持 None");
        assert_eq!(cursor, 0, "失败后游标应保持 0");
    }

    /// #1: 损坏的 GIF——open_gif_frames 解码初始化失败，返回空 Vec。
    #[test]
    fn test_prefetch_with_cursor_corrupted_gif_returns_empty() {
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result =
            prefetch_with_cursor(path, screen_w, screen_h, &mut frames_iter, &mut cursor, 0, 2);
        assert!(result.is_empty(), "损坏的 GIF 应返回空 Vec");
    }

    /// #1: target=0 + half=2 → window=[0,2]，无 skip，全收集。
    #[test]
    fn test_prefetch_with_cursor_window_at_start() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result =
            prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 0, 2);
        assert_eq!(result.len(), 3, "窗口 [0,2] 应返回 3 帧");
        let indices: Vec<usize> = result.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(cursor, 3);
    }

    /// #1: target+half 超出总帧数时优雅降级——返回实际存在的帧。
    #[test]
    fn test_prefetch_with_cursor_window_beyond_end_degrades_gracefully() {
        let (_dir, path) = make_indexed_gif(5); // 仅 5 帧（索引 0-4）
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        // target=3, half=2 → window=[1,5]，但帧 5 不存在
        let result =
            prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 3, 2);
        assert_eq!(result.len(), 4, "应返回帧 1,2,3,4（帧 5 不存在）");
        let indices: Vec<usize> = result.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![1, 2, 3, 4]);
    }

    /// #1: 像素值正确性——验证 RGBA 保持与帧索引匹配。
    #[test]
    fn test_prefetch_with_cursor_pixel_values_correct() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result =
            prefetch_with_cursor(&path, screen_w, screen_h, &mut frames_iter, &mut cursor, 5, 2);
        // 帧 i 的 RGBA = [i,0,0,255]（GDI 经 BI_BITFIELDS 解释，无需转换），1×1 = 4 字节
        for (i, frame) in &result {
            assert_eq!(frame.pixels.len(), 4);
            assert_eq!(frame.pixels[0], *i as u8, "帧 {} R 分量应等于帧索引", i);
            assert_eq!(frame.pixels[1], 0, "帧 {} G 分量", i);
            assert_eq!(frame.pixels[2], 0, "帧 {} B 分量", i);
            assert_eq!(frame.pixels[3], 255, "帧 {} A 分量", i);
        }
    }

    // ========== #2: decode_single_frame_with_cursor 持久化游标单帧解码测试 ==========

    /// #2: 首次调用——frames_iter=None 触发 need_open，从 0 解码到 target。
    #[test]
    fn test_decode_single_frame_with_cursor_first_call_opens_gif() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        let frame = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            5,
        )
        .expect("应成功解码帧 5");

        // 帧 5 的 RGBA = [5,0,0,255]（GDI 经 BI_BITFIELDS 解释，无需转换）
        assert_eq!(frame.pixels.len(), 4);
        assert_eq!(frame.pixels[0], 5, "R 分量应等于帧索引 5");
        // 游标应推进到 target+1=6
        assert_eq!(cursor, 6, "游标应推进到 6");
        assert!(frames_iter.is_some(), "迭代器应保留以复用");
    }

    /// #2: 前向前进——cursor ≈ target，仅解码 1 帧 delta（O(1) 而非 O(target)）。
    #[test]
    fn test_decode_single_frame_with_cursor_forward_advance_only_decodes_delta() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=3，cursor: 0→4
        let f1 = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 3,
        )
        .expect("请求 1 应成功");
        assert_eq!(f1.pixels[0], 3, "请求 1 应返回帧 3");
        assert_eq!(cursor, 4);

        // 请求 2：target=4，cursor=4，仅解码 1 帧（delta）
        let f2 = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 4,
        )
        .expect("请求 2 应成功");
        assert_eq!(f2.pixels[0], 4, "请求 2 应返回帧 4");
        assert_eq!(cursor, 5, "游标应推进到 5");
    }

    /// #2: 回绕——target < cursor 触发重新打开 GIF，cursor 重置为 0。
    #[test]
    fn test_decode_single_frame_with_cursor_rewind_reopens_gif() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=8，cursor: 0→9
        let f1 = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 8,
        )
        .expect("请求 1 应成功");
        assert_eq!(f1.pixels[0], 8);
        assert_eq!(cursor, 9);

        // 请求 2：target=2 < cursor=9 → 回绕，重新打开，cursor=0，解码 0..=2
        let f2 = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 2,
        )
        .expect("请求 2 应成功");
        assert_eq!(f2.pixels[0], 2, "回绕后应返回帧 2");
        assert_eq!(cursor, 3, "回绕后游标应推进到 3");
    }

    /// #2: 连续前向解码多个帧——模拟 WM_TIMER 逐帧推进，每次 O(1)。
    #[test]
    fn test_decode_single_frame_with_cursor_sequential_forward() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 首次：target=0，cursor: 0→1（O(1)）
        let f0 = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 0,
        )
        .expect("帧 0 应成功");
        assert_eq!(f0.pixels[0], 0);
        assert_eq!(cursor, 1);

        // 逐帧推进 1→2→3→4，每次 O(1)
        for target in 1..=4 {
            let f = decode_single_frame_with_cursor(
                &path, screen_w, screen_h, &mut frames_iter, &mut cursor, target,
            )
            .expect("前向解码应成功");
            assert_eq!(f.pixels[0], target as u8, "帧 {} R 分量", target);
            assert_eq!(cursor, target + 1, "游标应推进到 {}", target + 1);
        }
    }

    /// #2: target 超出总帧数返回 None（迭代器耗尽）。
    #[test]
    fn test_decode_single_frame_with_cursor_target_beyond_end() {
        let (_dir, path) = make_indexed_gif(5);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // target=10 超出 5 帧总数
        let result = decode_single_frame_with_cursor(
            &path, screen_w, screen_h, &mut frames_iter, &mut cursor, 10,
        );
        assert!(result.is_none(), "超出范围的 target 应返回 None");
    }

    /// #2: 不存在的文件返回 None。
    #[test]
    fn test_decode_single_frame_with_cursor_nonexistent_file() {
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result = decode_single_frame_with_cursor(
            "nonexistent_file.gif",
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            0,
        );
        assert!(result.is_none(), "不存在的文件应返回 None");
        assert!(frames_iter.is_none(), "失败后迭代器应为 None");
        assert_eq!(cursor, 0, "失败后游标应重置为 0");
    }

    /// #2: 像素值正确性——RGBA 保持与帧索引匹配（与 prefetch 测试一致）。
    #[test]
    fn test_decode_single_frame_with_cursor_pixel_values_correct() {
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let frame = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            7,
        )
        .expect("应成功解码帧 7");
        // 帧 7 的 RGBA = [7,0,0,255]（GDI 经 BI_BITFIELDS 解释，无需转换）
        assert_eq!(frame.pixels.len(), 4);
        assert_eq!(frame.pixels[0], 7, "R 分量应等于帧索引 7");
        assert_eq!(frame.pixels[1], 0, "G 分量");
        assert_eq!(frame.pixels[2], 0, "B 分量");
        assert_eq!(frame.pixels[3], 255, "A 分量");
    }

    /// #2: 不跳过超大帧——与 prefetch_with_cursor 不同，sync 兜底需处理 4K 帧
    /// （与原 decode_gif_frame_at 行为一致，v15-B-005 4K GIF 兜底依赖此语义）。
    #[test]
    fn test_decode_single_frame_with_cursor_no_oversized_skip() {
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        super::super::set_screen_size_for_test(3840, 2160);

        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("oversized_single.gif");
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut encoder = GifEncoder::new(file);
        // 1500×1500×4 = 9MB > 8MB 阈值，prefetch_with_cursor 会跳过，但本函数不应跳过
        let pixels = vec![0u8; 1500 * 1500 * 4];
        encoder
            .encode(&pixels, 1500, 1500, ExtendedColorType::Rgba8)
            .expect("编码 GIF 帧应成功");
        drop(encoder);
        let path = gif_path.to_str().unwrap().to_string();

        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let frame = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            0,
        )
        .expect("超大帧不应被跳过（sync 兜底语义）");
        // 3840×2160 屏幕下降采样到 ≤3840×2160，但 1500×1500 < 屏幕尺寸，不降采样
        assert_eq!(frame.width, 1500);
        assert_eq!(frame.height, 1500);
        assert_eq!(
            frame.pixels.len(),
            1500 * 1500 * 4,
            "9MB 帧应完整返回（不跳过）"
        );
    }

    // ========== #1 基线：O(N) 重解码耗时测量 ==========
    //
    // 目的：采集 decode_gif_frame_at(N) / decode_gif_frame_range 随 N 的耗时增长
    // 曲线，数据驱动判断「持久化解码游标消除 O(N)」优化(#1)是否值得实施。
    //
    // 运行：cargo test -p mirrorstar-core bench_decode_o_n_growth --ignored --nocapture
    //
    // 标记 #[ignore] 因：生成多帧真实分辨率 GIF + 多次解码耗时较长，
    // 不纳入常规测试运行。输出为测量表格（非断言），供人工分析。

    /// 中位数计时（3 次），返回毫秒。嵌套 fn 避免闭包引用参数的生命周期推断问题。
    fn time_decode_at(path: &str, n: usize) -> f64 {
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let t = std::time::Instant::now();
                let _ = decode_gif_frame_at(path, n, DEFAULT_MAX_GIF_MEMORY_MB).unwrap();
                t.elapsed().as_micros()
            })
            .collect();
        s.sort_unstable();
        s[1] as f64 / 1000.0
    }

    fn time_decode_range(path: &str, start: usize, end: usize) -> f64 {
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let t = std::time::Instant::now();
                let _ = decode_gif_frame_range(path, start, end, DEFAULT_MAX_GIF_MEMORY_MB).unwrap();
                t.elapsed().as_micros()
            })
            .collect();
        s.sort_unstable();
        s[1] as f64 / 1000.0
    }

    #[test]
    #[ignore]
    fn bench_decode_o_n_growth() {
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        // 序列化屏幕尺寸缓存，避免与其他 set_screen_size_for_test 测试并行干扰
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        // 设为大屏，使测试帧不触发降采样（测量纯 LZW 解码 + 帧合成 + 降采样成本）
        super::super::set_screen_size_for_test(3840, 2160);

        println!();
        println!("=== #1 基线：O(N) 重解码耗时 (median of 3, 屏幕 3840×2160 不降采样) ===");
        println!("注：测试帧为大面积纯色（高压缩），LZW 解码部分为真实 GIF 的下界；");
        println!("    帧合成 + 降采样成本 ∝ 帧像素数，与真实 GIF 一致。");

        // (宽, 高, 帧数, 标签)
        let configs: &[(u32, u32, usize, &str)] = &[
            (320, 240, 100, "320×240"),
            (640, 480, 60, "640×480"),
        ];
        let mut per_frame: Vec<(f64, &str)> = Vec::new();

        for &(w, h, n_frames, label) in configs {
            // 生成多帧 GIF：每帧大面积纯色，R 通道随帧索引变化（7 色循环）
            let dir = tempfile::tempdir().unwrap();
            let gif_path = dir.path().join(format!("bench_{w}x{h}_{n_frames}f.gif"));
            let file = std::fs::File::create(&gif_path).unwrap();
            let mut encoder = GifEncoder::new(file);
            for i in 0..n_frames {
                let r = (i % 7) as u8;
                let px: Vec<u8> = [r, 100, 200, 255].repeat((w as usize) * (h as usize));
                encoder
                    .encode(&px, w, h, ExtendedColorType::Rgba8)
                    .expect("编码 GIF 帧应成功");
            }
            drop(encoder);
            let path = gif_path.to_str().unwrap().to_string();

            println!();
            println!("[{label}, {n_frames} 帧]  decode_gif_frame_at(N):");
            let mut max_ms = 0.0;
            let mut max_n = 0usize;
            for &n in &[
                0usize,
                n_frames / 4,
                n_frames / 2,
                (3 * n_frames) / 4,
                n_frames - 1,
            ] {
                let ms = time_decode_at(&path, n);
                let per = if n > 0 { ms / n as f64 } else { 0.0 };
                println!("  N={n:>3}  → {ms:>7.2} ms   (per-frame ≈ {per:.3} ms)");
                if n >= max_n {
                    max_ms = ms;
                    max_n = n;
                }
            }
            // 用最大 N 估算 per-frame 成本（固定开销已摊薄，最稳定）
            let c = if max_n > 0 { max_ms / max_n as f64 } else { 0.0 };
            per_frame.push((c, label));

            // decode_gif_frame_range 模拟预取窗口 [N-half, N+half]（half=STREAMING_WINDOW_HALF）
            // 仍从 0 解码到 end，故耗时 ∝ end（O(N)）。
            let half = STREAMING_WINDOW_HALF;
            println!("[{label}, {n_frames} 帧]  decode_gif_frame_range(N-half, N+half)  (预取窗口):");
            for &n in &[n_frames / 2, n_frames - 1] {
                let start = n.saturating_sub(half);
                let end = (n + half).min(n_frames - 1);
                let ms = time_decode_range(&path, start, end);
                println!("  N={n:>3}  range[{start},{end}]  → {ms:>7.2} ms  (解码 0..={end})");
            }
        }

        // 交叉点分析：同步兜底 decode_gif_frame_at(N) 阻塞主线程超过帧延迟 → 卡顿
        println!();
        println!("=== 交叉点分析 (主线程同步兜底阻塞 > 帧延迟 → 卡顿) ===");
        for &(c, label) in &per_frame {
            if c <= 0.0 {
                continue;
            }
            println!("{label}: per-frame ≈ {c:.3} ms");
            println!("  超过 33ms (30fps) 的 N ≈ {:.0} 帧", 33.0 / c);
            println!("  超过 100ms (10fps) 的 N ≈ {:.0} 帧", 100.0 / c);
        }
        println!();
        println!("说明：N < 交叉点 → 不卡顿；N > 交叉点 → 同步兜底阻塞 > 帧延迟。");
        println!("      前向播放每循环 N-1 前向 + 1 回绕(回绕仍 O(N) 从 0 解码)。");
        println!("      交叉点为下界估计（真实 GIF LZW 解码更慢，交叉点更小）。");

        super::super::invalidate_screen_size_cache();
    }

    // ========== #1 + #2: 持久化游标 O(1) 前向解码对比测量 ==========
    //
    // 目的：对比旧路径（每次从 0 解码，N 次前向调用 = O(N²) 总耗时）与
    // 新路径（#1 prefetch_with_cursor / #2 decode_single_frame_with_cursor
    // 持久化游标，前向仅解码 1 帧 delta，N 次前向调用 = O(N) 总耗时）在
    // 完整前向播放循环（target=0..N-1，模拟一次 loop 的前向段）下的耗时。
    //
    // 运行：cargo test -p mirrorstar-core bench_cursor_o_1_forward --ignored --nocapture
    //
    // 标记 #[ignore] 因：生成多帧真实分辨率 GIF + 多次解码耗时较长，
    // 不纳入常规测试运行。输出为测量表格（非断言），供人工分析 #1+#2 实际改善，
    // 并为后续 (d) STREAMING_WINDOW_HALF 2→1 调优提供数据支撑。

    /// 旧路径完整前向循环：N 次 `decode_gif_frame_at(i)`，每次从 0 解码到 i。总 O(N²)。
    fn time_full_loop_old(path: &str, n_frames: usize) -> f64 {
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let t = std::time::Instant::now();
                for i in 0..n_frames {
                    let _ = decode_gif_frame_at(path, i, DEFAULT_MAX_GIF_MEMORY_MB).unwrap();
                }
                t.elapsed().as_micros()
            })
            .collect();
        s.sort_unstable();
        s[1] as f64 / 1000.0
    }

    /// #2 sync 兜底完整前向循环：复用 `(frames_iter, cursor)`，每次前向 O(1) delta。总 O(N)。
    fn time_full_loop_sync(path: &str, screen_w: u32, screen_h: u32, n_frames: usize) -> f64 {
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let mut frames_iter: Option<image::Frames<'static>> = None;
                let mut cursor = 0usize;
                let t = std::time::Instant::now();
                for i in 0..n_frames {
                    let _ = decode_single_frame_with_cursor(
                        path, screen_w, screen_h, &mut frames_iter, &mut cursor, i,
                    )
                    .unwrap();
                }
                t.elapsed().as_micros()
            })
            .collect();
        s.sort_unstable();
        s[1] as f64 / 1000.0
    }

    /// #1 prefetch 完整前向循环：复用 `(frames_iter, cursor)`，每次前向 O(1) delta
    /// （窗口滑动仅解码新进入的 1 帧）。总 O(N)。
    fn time_full_loop_prefetch(
        path: &str,
        screen_w: u32,
        screen_h: u32,
        n_frames: usize,
    ) -> f64 {
        let half = STREAMING_WINDOW_HALF;
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let mut frames_iter: Option<image::Frames<'static>> = None;
                let mut cursor = 0usize;
                let t = std::time::Instant::now();
                for i in 0..n_frames {
                    let _ = prefetch_with_cursor(
                        path, screen_w, screen_h, &mut frames_iter, &mut cursor, i, half,
                    );
                }
                t.elapsed().as_micros()
            })
            .collect();
        s.sort_unstable();
        s[1] as f64 / 1000.0
    }

    #[test]
    #[ignore]
    fn bench_cursor_o_1_forward() {
        use image::codecs::gif::GifEncoder;
        use image::ExtendedColorType;

        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        // 与 bench_decode_o_n_growth 同条件：大屏使测试帧不触发降采样，
        // 测量纯 LZW 解码 + 帧合成 + 降采样成本。
        super::super::set_screen_size_for_test(3840, 2160);

        println!();
        println!("=== #1 + #2: 持久化游标 vs 旧 O(N) 路径（完整前向循环 target=0..N-1, median of 3, 屏幕 3840×2160）===");
        println!("注：与 bench_decode_o_n_growth 同条件（纯色高压缩帧，LZW 为真实 GIF 下界）。");
        println!("    旧路径每次调用从 0 解码到 target → N 次调用 = O(N²)；");
        println!("    新路径持久化游标，前向仅解码 1 帧 delta → N 次调用 = O(N)。");

        let configs: &[(u32, u32, usize, &str)] = &[
            (320, 240, 100, "320×240"),
            (640, 480, 60, "640×480"),
        ];

        for &(w, h, n_frames, label) in configs {
            // 生成与 bench_decode_o_n_growth 相同的多帧 GIF（7 色循环纯色帧）
            let dir = tempfile::tempdir().unwrap();
            let gif_path = dir.path().join(format!("bench_cursor_{w}x{h}_{n_frames}f.gif"));
            let file = std::fs::File::create(&gif_path).unwrap();
            let mut encoder = GifEncoder::new(file);
            for i in 0..n_frames {
                let r = (i % 7) as u8;
                let px: Vec<u8> = [r, 100, 200, 255].repeat((w as usize) * (h as usize));
                encoder
                    .encode(&px, w, h, ExtendedColorType::Rgba8)
                    .expect("编码 GIF 帧应成功");
            }
            drop(encoder);
            let path = gif_path.to_str().unwrap().to_string();
            let (screen_w, screen_h) = super::super::get_screen_size();

            println!();
            println!("[{label}, {n_frames} 帧] 完整前向循环 target=0..{n_frames}-1:");

            // 旧路径：N 次 decode_gif_frame_at(i)，每次 O(i) → 总 O(N²)
            let old_ms = time_full_loop_old(&path, n_frames);
            let old_per = old_ms / n_frames as f64;
            println!(
                "  旧 decode_gif_frame_at    总 {old_ms:>9.2} ms   (per-call ≈ {old_per:.3} ms)  O(N²)"
            );

            // #2 sync 兜底：N 次 decode_single_frame_with_cursor(i) 复用 cursor，前向 O(1)
            let sync_ms = time_full_loop_sync(&path, screen_w, screen_h, n_frames);
            let sync_per = sync_ms / n_frames as f64;
            println!(
                "  #2 decode_single_frame    总 {sync_ms:>9.2} ms   (per-call ≈ {sync_per:.3} ms)  O(N)"
            );

            // #1 prefetch：N 次 prefetch_with_cursor(i, half=STREAMING_WINDOW_HALF) 复用 cursor，前向 O(1)
            let prefetch_ms = time_full_loop_prefetch(&path, screen_w, screen_h, n_frames);
            let prefetch_per = prefetch_ms / n_frames as f64;
            println!(
                "  #1 prefetch_with_cursor   总 {prefetch_ms:>9.2} ms   (per-call ≈ {prefetch_per:.3} ms)  O(N)"
            );

            // 加速比
            if sync_ms > 0.0 {
                println!("  → #2 vs 旧 加速比 ≈ {:.2}×", old_ms / sync_ms);
            }
            if prefetch_ms > 0.0 {
                println!("  → #1 vs 旧 加速比 ≈ {:.2}×", old_ms / prefetch_ms);
            }
        }

        println!();
        println!("说明：N=帧数。前向段加速比 ≈ N/2（旧 O(N²) 总 / 新 O(N) 总的理论比）。");
        println!("      实测加速比低于 N/2 因首帧仍 O(N) 打开 + 帧合成/降采样固定开销。");
        println!("      回绕段未测（旧回绕 O(1)，新回绕 O(half)，差异小且仅 1 次/loop）。");
        println!("      此数据为 (d) STREAMING_WINDOW_HALF 2→1 调优提供基线：");
        println!("      若 #1 前向 per-call 已接近 #2，说明窗口解码成本可忽略，");
        println!("      (d) 降 half 主要省内存而对 CPU 影响极小。");

        super::super::invalidate_screen_size_cache();
    }
}
