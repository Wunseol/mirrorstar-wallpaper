//! 网格交互的**纯逻辑**（Task 5.2 / 5.3 / 5.5 / 5.7）。
//!
//! 拖拽插入位反算、重排、搜索过滤、拖入文件合并全部落在本模块，且都**不依赖 Slint、
//! 不做 IO**：于是这些边界条件（空库、越界指针、大小写、重复文件、不支持的扩展名）
//! 都能在无 GUI 环境下直接单测（SubTask 5.7），Slint 侧只负责把坐标与负载送进来。

use std::collections::HashSet;

use mirrorstar_core::config::WallpaperEntry;

use crate::grid::GridGeometry;

/// 旧前端 `utils.ts` 的图片扩展名集合（小写，不含点）。
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp"];
/// 视频扩展名集合。
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "avi", "mkv", "webm", "mov"];
/// GIF 扩展名集合。
const GIF_EXTENSIONS: &[&str] = &["gif"];
/// 网页壁纸扩展名集合。
const HTML_EXTENSIONS: &[&str] = &["html", "htm"];

/// 取路径末段作为显示名（兼容 `/` 与 `\`）。
///
/// 与 `settings::file_name` 逐字符相同，故直接复用共享实现，避免多处副本漂移。
pub(crate) use crate::settings::file_name;

/// 小写扩展名（不含点）；无扩展名返回空串。
pub fn file_extension(path: &str) -> String {
    match file_name(path).rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 是否为受支持的壁纸文件（与旧前端 `isSupportedFile` 语义一致）。
pub fn is_supported_file(path: &str) -> bool {
    let ext = file_extension(path);
    IMAGE_EXTENSIONS.contains(&ext.as_str())
        || VIDEO_EXTENSIONS.contains(&ext.as_str())
        || GIF_EXTENSIONS.contains(&ext.as_str())
        || HTML_EXTENSIONS.contains(&ext.as_str())
}

/// 由指针位置反算插入位（**间隙语义**：返回值 ∈ `[0, item_count]`，`k` 表示插到原第 `k`
/// 项之前，`item_count` 表示追加到末尾）。
///
/// 输入坐标是**网格内容坐标系**（原点为内容左上角，含 `pad`；`y` 已折算滚动偏移）。
/// 做法与旧前端一致：定位指针所在单元格，再按指针落在单元格左半还是右半决定插到该格前/后。
/// 指针在末列右侧时自然落到下一行行首；超出末行则被夹到 `item_count`（追加）。
pub fn insert_index(x: f64, y: f64, geometry: &GridGeometry, item_count: usize) -> usize {
    if item_count == 0 || geometry.columns == 0 {
        return 0;
    }
    let cell_w = geometry.card_w + geometry.gap;
    let cell_h = geometry.card_h + geometry.gap;
    let local_x = x - geometry.pad;
    let local_y = y - geometry.pad;

    let columns = geometry.columns as f64;
    let col = (local_x / cell_w).floor().clamp(0.0, columns - 1.0) as usize;
    let row = (local_y / cell_h).floor().max(0.0) as usize;

    // 落在单元格右半 → 插到该格之后。
    let in_cell_x = local_x - col as f64 * cell_w;
    let after = in_cell_x > geometry.card_w / 2.0;

    let candidate = row * geometry.columns + col + usize::from(after);
    candidate.min(item_count)
}

/// 把 `ids[from]` 移动到插入位 `to`（间隙语义，见 [`insert_index`]），返回新顺序。
///
/// `from` 越界、或 `to` 落在"原位两侧"（`to == from || to == from + 1`）时判定为无变化，
/// 原样返回（调用方据此跳过落盘请求）。
pub fn reorder_ids(ids: &[String], from: usize, to: usize) -> Vec<String> {
    let len = ids.len();
    if len == 0 || from >= len {
        return ids.to_vec();
    }
    let to = to.min(len);
    if to == from || to == from + 1 {
        return ids.to_vec();
    }
    let mut next = ids.to_vec();
    let moved = next.remove(from);
    let insert_at = if to > from { to - 1 } else { to };
    next.insert(insert_at, moved);
    next
}

/// 条目是否命中关键字（关键字须已小写；按文件名子串匹配，与旧前端 `matchesSearch` 一致）。
pub fn matches_keyword(entry: &WallpaperEntry, keyword_lower: &str) -> bool {
    if keyword_lower.is_empty() {
        return true;
    }
    file_name(&entry.file_path)
        .to_lowercase()
        .contains(keyword_lower)
}

/// 过滤后的原始索引（保持库内顺序）。空关键字返回全部索引。
pub fn filter_indices(wallpapers: &[WallpaperEntry], keyword: &str) -> Vec<usize> {
    if keyword.is_empty() {
        return (0..wallpapers.len()).collect();
    }
    let keyword = keyword.to_lowercase();
    wallpapers
        .iter()
        .enumerate()
        .filter(|(_, wp)| matches_keyword(wp, &keyword))
        .map(|(index, _)| index)
        .collect()
}

/// 命中关键字的条目数（**不分配索引数组**；拖拽悬停时按帧调用，须廉价）。
pub fn count_matches(wallpapers: &[WallpaperEntry], keyword: &str) -> usize {
    if keyword.is_empty() {
        return wallpapers.len();
    }
    let keyword = keyword.to_lowercase();
    wallpapers
        .iter()
        .filter(|wp| matches_keyword(wp, &keyword))
        .count()
}

/// 拖入文件的分类结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DropMerge {
    /// 可添加的路径（已去重，保持拖入顺序）。
    pub accepted: Vec<String>,
    /// 扩展名不受支持、被拒绝的路径。
    pub unsupported: Vec<String>,
    /// 已在库中（或批内重复）而被跳过的路径。
    pub duplicate: Vec<String>,
}

/// 合并一次拖入的路径：过滤不支持的类型、去重（与库内已有路径及批内之前项比较）。
pub fn merge_drop_paths(dropped: &[String], existing_paths: &[String]) -> DropMerge {
    let mut seen: HashSet<String> = existing_paths.iter().map(|p| normalize_path(p)).collect();
    let mut merged = DropMerge::default();
    for path in dropped {
        if !is_supported_file(path) {
            merged.unsupported.push(path.clone());
            continue;
        }
        if !seen.insert(normalize_path(path)) {
            merged.duplicate.push(path.clone());
            continue;
        }
        merged.accepted.push(path.clone());
    }
    merged
}

/// 路径归一化（Windows 不区分大小写与斜杠方向）。
fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn geom() -> GridGeometry {
        // 自带 fixture：位置断言（如"第二行首格 → 索引 5"）以 5 列为基准，不随默认几何漂移。
        GridGeometry {
            columns: 5,
            card_w: 160.0,
            card_h: 134.0,
            gap: 12.0,
            pad: 8.0,
            buffer_rows: 2,
        }
    }

    fn entry(id: &str, file_path: &str) -> WallpaperEntry {
        serde_json::from_value(json!({
            "id": id,
            "file_path": file_path,
            "wallpaper_type": "Image",
            "added_at": "1700000000",
        }))
        .expect("构造测试条目")
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // ── 5.7①：插入位反算 ───────────────────────────────────────────────────
    #[test]
    fn insert_index_before_or_after_cell_by_horizontal_half() {
        let g = geom(); // 列 5、卡 160x134、间距 12、内边距 8
                        // 第一格左半区 → 插到第 0 项之前
        assert_eq!(insert_index(8.0, 8.0, &g, 10), 0);
        // 越过卡片中线 → 插到第 0 项之后
        assert_eq!(insert_index(8.0 + 120.0, 8.0, &g, 10), 1);
    }

    #[test]
    fn insert_index_maps_rows_and_columns() {
        let g = geom();
        let row_h = g.row_height(); // 146
        let cell_w = g.card_w + g.gap; // 172
                                       // 第二行第一格左半 → 第 5 项之前
        assert_eq!(insert_index(8.0, 8.0 + row_h, &g, 10), 5);
        // 第一行第三格右半 → 第 3 项之前
        assert_eq!(insert_index(8.0 + 2.0 * cell_w + 120.0, 8.0, &g, 10), 3);
    }

    #[test]
    fn insert_index_clamps_at_both_ends() {
        let g = geom();
        // 负向过冲（回弹）夹到最前
        assert_eq!(insert_index(-500.0, -500.0, &g, 10), 0);
        // 远超库尾 → 追加到末尾
        assert_eq!(insert_index(10_000.0, 10_000.0, &g, 10), 10);
        // 空库恒为 0
        assert_eq!(insert_index(100.0, 100.0, &g, 0), 0);
    }

    // ── 5.7②：重排 + 区间与过滤组合 ─────────────────────────────────────────
    #[test]
    fn reorder_ids_moves_item_to_target_gap() {
        let list = ids(&["a", "b", "c", "d"]);
        assert_eq!(reorder_ids(&list, 0, 2), ids(&["b", "a", "c", "d"]));
        assert_eq!(reorder_ids(&list, 0, 4), ids(&["b", "c", "d", "a"]));
        assert_eq!(reorder_ids(&list, 3, 0), ids(&["d", "a", "b", "c"]));
    }

    #[test]
    fn reorder_ids_is_noop_in_place_or_out_of_range() {
        let list = ids(&["a", "b", "c"]);
        assert_eq!(reorder_ids(&list, 1, 1), list);
        assert_eq!(reorder_ids(&list, 1, 2), list);
        assert_eq!(reorder_ids(&list, 9, 0), list);
        assert_eq!(reorder_ids(&[], 0, 0), Vec::<String>::new());
    }

    #[test]
    fn filter_indices_combines_with_window_bounds() {
        let library: Vec<WallpaperEntry> = (0..25)
            .map(|i| {
                let name = if i % 2 == 0 { " beach " } else { "city" };
                entry(&format!("id-{i}"), &format!("C:/w/{}{i}.png", name.trim()))
            })
            .collect();

        // 无关键字 → 全部索引，过滤后总数与库一致
        let all = filter_indices(&library, "");
        assert_eq!(all.len(), 25);
        assert_eq!(count_matches(&library, ""), 25);

        // 过滤后只保留命中项，索引仍指向原始库位置（虚拟化映射的前提）
        let beach = filter_indices(&library, "beach");
        assert_eq!(beach.len(), 13);
        assert!(beach.iter().all(|&i| i % 2 == 0), "保留项必须是偶数索引");
        assert_eq!(count_matches(&library, "beach"), 13);

        // 大小写不敏感
        assert_eq!(filter_indices(&library, "BEACH").len(), 13);

        // 无命中 → 空；虚拟化窗口据此退化为空窗口
        let none = filter_indices(&library, "nomatch");
        assert!(none.is_empty());
        assert_eq!(count_matches(&library, "nomatch"), 0);

        // 过滤后的区间切片：用过滤结果的总数算窗口，不越界
        let g = geom();
        let window = g.window(0.0, 640.0, beach.len());
        assert!(window.end_index() <= beach.len());
    }

    // ── 5.7③：批量拖入合并 ─────────────────────────────────────────────────
    #[test]
    fn merge_drop_paths_filters_unsupported_and_dedupes() {
        let existing = ids(&["C:/lib/a.png"]);
        let dropped = ids(&[
            "C:/new/b.jpg",
            "C:/LIB/A.PNG", // 与库内重复（大小写/斜杠不敏感）
            "C:/new/c.txt", // 不支持
            "C:/new/b.jpg", // 批内重复
            "C:/new/d.mp4",
            "C:/new/e", // 无扩展名
        ]);

        let merged = merge_drop_paths(&dropped, &existing);
        assert_eq!(merged.accepted, ids(&["C:/new/b.jpg", "C:/new/d.mp4"]));
        assert_eq!(merged.unsupported, ids(&["C:/new/c.txt", "C:/new/e"]));
        assert_eq!(merged.duplicate, ids(&["C:/LIB/A.PNG", "C:/new/b.jpg"]));
    }

    #[test]
    fn merge_drop_paths_keeps_supported_types() {
        let merged = merge_drop_paths(
            &ids(&[
                "C:/a/1.png",
                "C:/a/2.JPEG",
                "C:/a/3.gif",
                "C:/a/4.webm",
                "C:/a/5.HTML",
            ]),
            &[],
        );
        assert_eq!(merged.accepted.len(), 5);
        assert!(merged.unsupported.is_empty());
        assert!(merged.duplicate.is_empty());
    }

    #[test]
    fn extension_and_file_name_helpers() {
        assert_eq!(file_extension("C:/a/B.PNG"), "png");
        assert_eq!(file_extension("C:/a/noext"), "");
        assert_eq!(file_extension("C:/a/."), "");
        assert_eq!(file_name("C:/a/b/c.mp4"), "c.mp4");
        assert_eq!(file_name("C:\\a\\b\\c.mp4"), "c.mp4");
        assert!(!is_supported_file("C:/a/x.psd"));
        assert!(!is_supported_file(""));
    }

    #[test]
    fn insert_index_and_window_share_filtered_count() {
        // 4.4：过滤叠加 —— 插入位与虚拟化窗口都以**过滤后条目数**为界。
        let g = geom();
        let w = g.window(0.0, 640.0, 7);
        assert_eq!(w.end_index(), 7, "窗口右端应贴近过滤子集大小");

        // 拖到过滤子集首位之前
        assert_eq!(insert_index(8.0, 8.0, &g, 7), 0);
        // 跨行：第二行首格左半 → 第 5 项之前（仍在子集内）
        assert_eq!(insert_index(8.0, 8.0 + g.row_height(), &g, 7), 5);
        // 远超子集末尾 → 夹到 7（追加）
        assert_eq!(insert_index(10_000.0, 10_000.0, &g, 7), 7);
        // 子集不足一行：插入位不得超过子集大小
        assert_eq!(insert_index(10_000.0, 8.0, &g, 3), 3);
        // 空子集恒为 0
        assert_eq!(insert_index(500.0, 500.0, &g, 0), 0);
    }

    #[test]
    fn reorder_ids_round_trips_against_insert_index_gaps() {
        // 4.2：`insert_index` 的间隙语义与 `reorder_ids` 的落位语义必须一致。
        let g = geom();
        let list = ids(&["a", "b", "c", "d", "e", "f"]);
        // 指针落在第 0 格右半 → 间隙 1 → 把 a 移到 b 之前 = 原位（无变化）
        let gap = insert_index(8.0 + 120.0, 8.0, &g, list.len());
        assert_eq!(gap, 1);
        assert_eq!(reorder_ids(&list, 0, gap), list, "原位落下应为原样");

        // 指针落在第 2 格右半 → 间隙 3 → a 移到 c 之后
        let gap = insert_index(8.0 + 2.0 * (g.card_w + g.gap) + 120.0, 8.0, &g, list.len());
        assert_eq!(gap, 3);
        assert_eq!(
            reorder_ids(&list, 0, gap),
            ids(&["b", "c", "a", "d", "e", "f"])
        );

        // 拖到末行右半 → 夹到末尾 → 追加
        let gap = insert_index(10_000.0, 10_000.0, &g, list.len());
        assert_eq!(gap, list.len());
        assert_eq!(
            reorder_ids(&list, 0, gap),
            ids(&["b", "c", "d", "e", "f", "a"])
        );
    }
}
