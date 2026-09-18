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
/// 预取解码的帧若超过此尺寸则跳过（不加入返回 Vec），
/// 避免 4K GIF（每帧 ~6MB）× (2*`STREAMING_WINDOW_HALF`+1) 帧造成瞬时内存尖峰。被跳过的帧
/// 仍由 [`decode_gif_frame_at`] 在 `WM_TIMER` 同步兜底解码（仅当前帧）。
pub(crate) const MAX_PREFETCH_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// v18: 流式窗口解码（可取消版）——解码过程中即时清空窗口外帧的像素。
///
/// 与全量解码行为一致，但额外接受取消标志与 `streaming_center`：解码
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

/// 内部解码实现：`decode_gif_with_cancel_streaming` / `decode_gif_streaming` 的公共逻辑。
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
        let retained = frames.iter().filter(|f| !f.pixels.is_empty()).count();
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
/// 与全量解码的区别：仅解码第一帧，用于在创建窗口后立即显示首帧，
/// 剩余帧由后台线程解码（参见 `gif::gif_wallpaper_thread` 中的后台解码逻辑）。
/// 降采样逻辑与全量解码一致，确保首帧与全量解码结果一致。
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
/// 超 8MB 阈值意味着所有帧都会被 v10-C 跳过逻辑
/// 跳过预取，触发 v15-B-005 同步解码兜底，播放帧率下降。此时由调用方 emit
/// warning 提示用户"GIF 分辨率过高，播放可能不流畅"。
///
/// 与预取路径的跳过判断（`frame_size > MAX_PREFETCH_FRAME_BYTES`）
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
/// 从文件重新解码该帧的像素数据。降采样逻辑与全量解码一致，
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
/// 与范围预取实现不同，本函数复用调用方持有的 `frames_iter`+
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
    if delay_ms == 0 {
        100
    } else {
        delay_ms
    }
}

/// 处理单帧：解析延迟、按需降采样。像素保留 RGBA 字节序（GDI 经 BI_BITFIELDS
/// 掩码直接解释，无需 RGBA→BGRA 转换）。
fn process_gif_frame(frame: image::Frame, screen_w: u32, screen_h: u32) -> GifFrame {
    let delay_ms = frame_delay_ms(frame.delay());

    let img = frame.into_buffer();
    let (width, height) = img.dimensions();

    // 如果帧尺寸超过屏幕分辨率，等比降采样到屏幕范围内以减少内存占用。
    // scale = min(screen_w/width, screen_h/height)，保持帧宽高比不变，
    // 避免 thumbnail 非等比压扁导致动态 GIF 壁纸变形。目标尺寸 round 取整且至少 1×1。
    let (final_width, final_height, pixels) = if width > screen_w || height > screen_h {
        let scale = (screen_w as f64 / width as f64).min(screen_h as f64 / height as f64);
        let dw = ((width as f64 * scale).round() as u32).max(1);
        let dh = ((height as f64 * scale).round() as u32).max(1);
        let thumb =
            image::imageops::thumbnail(&image::DynamicImage::ImageRgba8(img), dw, dh);
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
    use super::super::SCREEN_SIZE_TEST_MUTEX;

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

    // ========== W12 修复测试：BufReader 流式读取大 GIF ==========

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
        assert!(
            !is_outside_streaming_window(0, 1),
            "center=1 时帧 0 在窗口内"
        );
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=3, half=2 → window=[1,5]，cursor: 0→6
        let r1 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            3,
            2,
        );
        assert_eq!(r1.len(), 5, "请求 1 应返回 5 帧 [1,5]");
        assert_eq!(cursor, 6);

        // 请求 2：target=4, half=2 → window=[2,6]。
        // cursor=6 >= window_start=2，故帧 6 被处理；帧 2-5 已由请求 1 填充（cursor 跳过）。
        // 期望仅返回帧 6（delta），而非全部 [2,6]。
        let r2 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            4,
            2,
        );
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(20);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=5, half=2 → window=[3,7]，cursor: 0→8
        let r1 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            5,
            2,
        );
        assert_eq!(r1.len(), 5);
        assert_eq!(cursor, 8);

        // 请求 2：target=7, half=2 → window=[5,9]。
        // cursor=8 在窗口 [5,9] 内，仅处理帧 8,9（帧 5-7 已由请求 1 填充）。
        let r2 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            7,
            2,
        );
        assert_eq!(r2.len(), 2, "请求 2 应仅返回 delta 帧 8,9");
        let indices: Vec<usize> = r2.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![8, 9]);
    }

    /// #1: 回绕——window_end < cursor 触发重新打开 GIF，cursor 重置为 0。
    #[test]
    fn test_prefetch_with_cursor_rewind_reopens_gif() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=8, half=2 → window=[6,10]（实际 [6,9]，GIF 只有 10 帧）。
        // cursor: 0→6（skip 0-5）→ 6,7,8,9 处理 → cursor=10，iter.next()=None，break。
        // 游标停在 10（break 前未 ++）。
        let r1 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            8,
            2,
        );
        assert_eq!(r1.len(), 4, "请求 1 应返回帧 6,7,8,9");
        assert!(cursor >= 10, "游标应到达或超过 10");

        // 请求 2：target=2, half=2 → window=[0,4]，window_end=4 < cursor=10 → 回绕。
        // 重新打开 GIF，cursor=0，解码 0..=4 = 5 帧。
        let r2 = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            2,
            2,
        );
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
        let result = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            1,
            2,
        );
        assert!(
            result.is_empty(),
            "v10-C: 全部帧超 8MB 阈值应被跳过，返回空"
        );

        super::super::invalidate_screen_size_cache();
    }

    /// #1: 错误路径——文件不存在时 open_gif_frames 返回 Err，预取返回空 Vec。
    #[test]
    fn test_prefetch_with_cursor_nonexistent_file_returns_empty() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let corrupted = &MINIMAL_GIF[..10];
        let dir = tempfile::tempdir().unwrap();
        let gif_path = dir.path().join("corrupted.gif");
        std::fs::write(&gif_path, corrupted).unwrap();
        let path = gif_path.to_str().unwrap();

        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result = prefetch_with_cursor(
            path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            0,
            2,
        );
        assert!(result.is_empty(), "损坏的 GIF 应返回空 Vec");
    }

    /// #1: target=0 + half=2 → window=[0,2]，无 skip，全收集。
    #[test]
    fn test_prefetch_with_cursor_window_at_start() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            0,
            2,
        );
        assert_eq!(result.len(), 3, "窗口 [0,2] 应返回 3 帧");
        let indices: Vec<usize> = result.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(cursor, 3);
    }

    /// #1: target+half 超出总帧数时优雅降级——返回实际存在的帧。
    #[test]
    fn test_prefetch_with_cursor_window_beyond_end_degrades_gracefully() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(5); // 仅 5 帧（索引 0-4）
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        // target=3, half=2 → window=[1,5]，但帧 5 不存在
        let result = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            3,
            2,
        );
        assert_eq!(result.len(), 4, "应返回帧 1,2,3,4（帧 5 不存在）");
        let indices: Vec<usize> = result.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![1, 2, 3, 4]);
    }

    /// #1: 像素值正确性——验证 RGBA 保持与帧索引匹配。
    #[test]
    fn test_prefetch_with_cursor_pixel_values_correct() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;
        let result = prefetch_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            5,
            2,
        );
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=3，cursor: 0→4
        let f1 = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            3,
        )
        .expect("请求 1 应成功");
        assert_eq!(f1.pixels[0], 3, "请求 1 应返回帧 3");
        assert_eq!(cursor, 4);

        // 请求 2：target=4，cursor=4，仅解码 1 帧（delta）
        let f2 = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            4,
        )
        .expect("请求 2 应成功");
        assert_eq!(f2.pixels[0], 4, "请求 2 应返回帧 4");
        assert_eq!(cursor, 5, "游标应推进到 5");
    }

    /// #2: 回绕——target < cursor 触发重新打开 GIF，cursor 重置为 0。
    #[test]
    fn test_decode_single_frame_with_cursor_rewind_reopens_gif() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 请求 1：target=8，cursor: 0→9
        let f1 = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            8,
        )
        .expect("请求 1 应成功");
        assert_eq!(f1.pixels[0], 8);
        assert_eq!(cursor, 9);

        // 请求 2：target=2 < cursor=9 → 回绕，重新打开，cursor=0，解码 0..=2
        let f2 = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            2,
        )
        .expect("请求 2 应成功");
        assert_eq!(f2.pixels[0], 2, "回绕后应返回帧 2");
        assert_eq!(cursor, 3, "回绕后游标应推进到 3");
    }

    /// #2: 连续前向解码多个帧——模拟 WM_TIMER 逐帧推进，每次 O(1)。
    #[test]
    fn test_decode_single_frame_with_cursor_sequential_forward() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(10);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // 首次：target=0，cursor: 0→1（O(1)）
        let f0 = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            0,
        )
        .expect("帧 0 应成功");
        assert_eq!(f0.pixels[0], 0);
        assert_eq!(cursor, 1);

        // 逐帧推进 1→2→3→4，每次 O(1)
        for target in 1..=4 {
            let f = decode_single_frame_with_cursor(
                &path,
                screen_w,
                screen_h,
                &mut frames_iter,
                &mut cursor,
                target,
            )
            .expect("前向解码应成功");
            assert_eq!(f.pixels[0], target as u8, "帧 {} R 分量", target);
            assert_eq!(cursor, target + 1, "游标应推进到 {}", target + 1);
        }
    }

    /// #2: target 超出总帧数返回 None（迭代器耗尽）。
    #[test]
    fn test_decode_single_frame_with_cursor_target_beyond_end() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
        let (_dir, path) = make_indexed_gif(5);
        let (screen_w, screen_h) = super::super::get_screen_size();
        let mut frames_iter: Option<image::Frames<'static>> = None;
        let mut cursor = 0usize;

        // target=10 超出 5 帧总数
        let result = decode_single_frame_with_cursor(
            &path,
            screen_w,
            screen_h,
            &mut frames_iter,
            &mut cursor,
            10,
        );
        assert!(result.is_none(), "超出范围的 target 应返回 None");
    }

    /// #2: 不存在的文件返回 None。
    #[test]
    fn test_decode_single_frame_with_cursor_nonexistent_file() {
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
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
        let _guard = SCREEN_SIZE_TEST_MUTEX.lock().unwrap();
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
                        path,
                        screen_w,
                        screen_h,
                        &mut frames_iter,
                        &mut cursor,
                        i,
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
    fn time_full_loop_prefetch(path: &str, screen_w: u32, screen_h: u32, n_frames: usize) -> f64 {
        let half = STREAMING_WINDOW_HALF;
        let mut s: Vec<u128> = (0..3)
            .map(|_| {
                let mut frames_iter: Option<image::Frames<'static>> = None;
                let mut cursor = 0usize;
                let t = std::time::Instant::now();
                for i in 0..n_frames {
                    let _ = prefetch_with_cursor(
                        path,
                        screen_w,
                        screen_h,
                        &mut frames_iter,
                        &mut cursor,
                        i,
                        half,
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

        let configs: &[(u32, u32, usize, &str)] =
            &[(320, 240, 100, "320×240"), (640, 480, 60, "640×480")];

        for &(w, h, n_frames, label) in configs {
            // 生成与 bench_decode_o_n_growth 相同的多帧 GIF（7 色循环纯色帧）
            let dir = tempfile::tempdir().unwrap();
            let gif_path = dir
                .path()
                .join(format!("bench_cursor_{w}x{h}_{n_frames}f.gif"));
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

    // ========== spec: fix-aspect-preserving-downsample（等比降采样测试） ==========
    //
    // process_gif_frame(frame, screen_w, screen_h) 的 screen_w/screen_h 为显式参数，
    // 测试完全确定，无需修改全局屏幕尺寸，故不使用 SCREEN_SIZE_TEST_MUTEX。

    #[test]
    fn test_process_gif_frame_downsample_wide_frame_keeps_aspect_ratio() {
        // A: 宽幅帧 2560×1080 + 屏幕 1920×1080。
        // scale = min(1920/2560, 1080/1080) = 0.75 → dw = 1920, dh = 810。
        let img = image::RgbaImage::from_pixel(2560, 1080, image::Rgba([0, 0, 0, 255]));
        let frame = image::Frame::new(img);
        let result = process_gif_frame(frame, 1920, 1080);

        // 期望值精确断言
        assert_eq!(result.width, 1920, "宽幅帧应等比降采样到宽 1920");
        assert_eq!(result.height, 810, "宽幅帧应等比降采样到高 810");
        // 不超屏幕
        assert!(result.width <= 1920, "宽度不应超过屏幕宽 1920");
        assert!(result.height <= 1080, "高度不应超过屏幕高 1080");
        // 宽高比保持（2560/1080 ≈ 2.3704，容差 0.02）
        let aspect = result.width as f64 / result.height as f64;
        let expected = 2560.0 / 1080.0;
        assert!(
            (aspect - expected).abs() <= 0.02,
            "宽高比应保持，实际 {aspect:.4}，期望 {expected:.4}"
        );
        // 像素长度 = 1920×810×4
        assert_eq!(result.pixels.len(), (1920 * 810 * 4) as usize);
    }

    #[test]
    fn test_process_gif_frame_downsample_portrait_frame_keeps_aspect_ratio() {
        // B: 竖幅帧 1080×1920 + 屏幕 1920×1080。
        // scale = min(1920/1080, 1080/1920) = 0.5625
        // → dw = round(1080*0.5625) = round(607.5) = 608, dh = round(1920*0.5625) = 1080。
        let img = image::RgbaImage::from_pixel(1080, 1920, image::Rgba([0, 0, 0, 255]));
        let frame = image::Frame::new(img);
        let result = process_gif_frame(frame, 1920, 1080);

        // 不超屏幕
        assert!(result.width <= 1920, "宽度不应超过屏幕宽 1920");
        assert!(result.height <= 1080, "高度不应超过屏幕高 1080");
        // 宽高比保持（1080/1920 = 0.5625，容差 0.02）
        let aspect = result.width as f64 / result.height as f64;
        let expected = 1080.0 / 1920.0;
        assert!(
            (aspect - expected).abs() <= 0.02,
            "宽高比应保持，实际 {aspect:.4}，期望 {expected:.4}"
        );
    }

    #[test]
    fn test_process_gif_frame_small_frame_not_downsampled() {
        // C: 小帧 100×50 + 屏幕 1920×1080，未超屏幕 → 原样保留，不降采样。
        let img = image::RgbaImage::from_pixel(100, 50, image::Rgba([0, 0, 0, 255]));
        let frame = image::Frame::new(img);
        let result = process_gif_frame(frame, 1920, 1080);

        assert_eq!(result.width, 100, "小帧宽度应原样保留");
        assert_eq!(result.height, 50, "小帧高度应原样保留");
        assert_eq!(
            result.pixels.len(),
            100 * 50 * 4,
            "小帧像素长度应为 100×50×4"
        );
    }
}
