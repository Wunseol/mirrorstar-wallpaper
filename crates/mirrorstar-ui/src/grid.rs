//! 虚拟化网格的**纯几何计算**（SubTask 4.2 / 4.1）。
//!
//! ## 为什么单独成模块且不依赖 Slint
//!
//! "只实例化可见行 ± 缓冲"的核心是一段与渲染无关的区间算术：给定滚动偏移、视口高度与
//! 条目总数，算出应当放进 Slint 模型的那一段**行对齐连续区间**。把它抽成纯 Rust 后，
//! 边界（空库、不足一屏、滚到底、缓冲越界、行对齐）都能在无 GUI 下单测覆盖，
//! 不必起事件循环。
//!
//! ## 窗口为什么必须"行对齐"
//!
//! Slint 侧卡片按 `(row, col)` 绝对定位。若窗口起点不落在行首，第一行的卡片会缺少左侧
//! 若干列而出现空洞。因此 [`GridGeometry::window`] 的 `first_index` 恒为 `columns` 的整数倍。

/// 网格布局参数：固定卡片尺寸、间距、内边距与列数。
///
/// **不随条目数变化**，因此内存与滚动开销只与"可见行数 + 缓冲"相关，与库规模无关。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    /// 列数。**仅为初始提示值**：实际值由 [`columns_for_width`] 依网格可用宽度写入。
    pub columns: usize,
    /// 卡片宽度（逻辑像素）。
    pub card_w: f64,
    /// 卡片高度（逻辑像素）。
    pub card_h: f64,
    /// 卡片间距（横纵同值）。
    pub gap: f64,
    /// 网格内容四周内边距。
    pub pad: f64,
    /// 可见区间上下各多保留的行数（滚动时留出预取余量，避免边缘空白）。
    pub buffer_rows: usize,
}

impl Default for GridGeometry {
    fn default() -> Self {
        Self {
            columns: 4,
            card_w: 200.0,
            card_h: 152.0,
            gap: 12.0,
            pad: 8.0,
            buffer_rows: 2,
        }
    }
}

/// 行对齐的可见窗口：`[first_index, first_index + len)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridWindow {
    /// 窗口首个条目的全局索引（恒为 `columns` 的整数倍）。
    pub first_index: usize,
    /// 窗口内条目数。
    pub len: usize,
    /// 窗口首行行号。
    pub first_row: usize,
}

impl GridWindow {
    /// 空窗口。
    pub const EMPTY: GridWindow = GridWindow {
        first_index: 0,
        len: 0,
        first_row: 0,
    };

    /// 窗口右端（开区间）。
    pub fn end_index(&self) -> usize {
        self.first_index + self.len
    }
}

impl GridGeometry {
    /// 单行（含行间距）占用的高度。
    pub fn row_height(&self) -> f64 {
        self.card_h + self.gap
    }

    /// 总行数（`columns` 为 0 时视为 0 行，避免除零）。
    pub fn row_count(&self, item_count: usize) -> usize {
        if self.columns == 0 {
            return 0;
        }
        item_count.div_ceil(self.columns)
    }

    /// 内容总高度（末行不留尾部 gap，整体上下各留 `pad`）。
    pub fn content_height(&self, item_count: usize) -> f64 {
        let rows = self.row_count(item_count);
        if rows == 0 {
            return 0.0;
        }
        rows as f64 * self.row_height() - self.gap + 2.0 * self.pad
    }

    /// 计算应实例化的可见窗口。
    ///
    /// - `scroll_y`：Flickable 视口纵偏移（内容坐标，`0` 为顶部）。
    /// - `viewport_h`：视口高度。
    /// - `item_count`：库内条目总数。
    ///
    /// 返回的窗口恒为**行对齐**，上下各扩展 `buffer_rows` 行并夹紧到 `[0, row_count)`。
    pub fn window(&self, scroll_y: f64, viewport_h: f64, item_count: usize) -> GridWindow {
        let rows = self.row_count(item_count);
        if rows == 0 {
            return GridWindow::EMPTY;
        }
        let row_h = self.row_height();
        // 首个可见行：抵消内边距后按行高取整。负数（过度回弹）夹到 0。
        let raw_first_visible = ((scroll_y - self.pad) / row_h).floor().max(0.0) as usize;
        // "+1" 保证视口底部被部分裁切的那一行也在窗口内。
        let visible_rows = (viewport_h / row_h).ceil() as usize + 1;
        // 上限取 `rows - visible_rows`（饱和），而非 `rows - 1`：滚到底（含过冲回弹）时窗口
        // 顶端必须仍对齐视口顶端，否则会把视口上方若干可见行排除在窗口外而出现空洞。
        let first_visible = raw_first_visible.min(rows.saturating_sub(visible_rows));
        let start_row = first_visible.saturating_sub(self.buffer_rows);
        let end_row = (first_visible + visible_rows + self.buffer_rows).min(rows);
        let first_index = start_row * self.columns;
        let end_index = (end_row * self.columns).min(item_count);
        GridWindow {
            first_index,
            len: end_index - first_index,
            first_row: start_row,
        }
    }
}

/// 依网格可用宽度计算能容纳的列数（至少 1）。
///
/// 约束：内容宽度 `2*pad + columns*card_w + (columns-1)*gap` 不超过 `avail_w`。
/// 解方程得 `columns = floor((avail_w - 2*pad + gap) / (card_w + gap))`。
/// 非有限值 / 单元宽度非正 / 结果小于 1 时回退为 1。
pub fn columns_for_width(avail_w: f64, card_w: f64, gap: f64, pad: f64) -> usize {
    let unit = card_w + gap;
    if !unit.is_finite() || unit <= 0.0 {
        return 1;
    }
    let usable = avail_w - 2.0 * pad + gap;
    if !usable.is_finite() {
        return 1;
    }
    let cols = (usable / unit).floor();
    if cols < 1.0 || !cols.is_finite() {
        1
    } else {
        cols as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> GridGeometry {
        GridGeometry {
            columns: 5,
            card_w: 160.0,
            card_h: 134.0,
            gap: 12.0,
            pad: 8.0,
            buffer_rows: 2,
        }
    }

    #[test]
    fn empty_library_yields_empty_window() {
        assert_eq!(geom().window(0.0, 640.0, 0), GridWindow::EMPTY);
        assert_eq!(geom().row_count(0), 0);
        assert_eq!(geom().content_height(0), 0.0);
    }

    #[test]
    fn row_count_rounds_up_and_content_height_has_no_trailing_gap() {
        let g = geom();
        assert_eq!(g.row_count(1), 1);
        assert_eq!(g.row_count(5), 1);
        assert_eq!(g.row_count(6), 2);
        assert_eq!(g.row_count(200), 40);
        // 1 行：card_h + 2*pad（无尾部 gap）
        assert_eq!(g.content_height(5), 134.0 + 16.0);
        // 2 行：2*card_h + gap + 2*pad
        assert_eq!(g.content_height(10), 2.0 * 134.0 + 12.0 + 16.0);
    }

    #[test]
    fn window_is_row_aligned_and_starts_at_top() {
        let g = geom();
        // 顶部：first_visible=0，start_row 被 sature 到 0
        let w = g.window(0.0, 640.0, 200);
        assert_eq!(w.first_index, 0);
        assert_eq!(w.first_row, 0);
        // viewport 640 / row_h 146 = 4.38 → ceil 5 + 1 = 6 可见行 + 2 缓冲 = 8 行
        assert_eq!(w.len, 8 * 5);
    }

    #[test]
    fn window_first_index_always_multiple_of_columns() {
        let g = geom();
        for scroll in [0.0_f64, 37.0, 146.0, 500.0, 4000.0, 100000.0] {
            let w = g.window(scroll, 640.0, 1000);
            assert_eq!(w.first_index % g.columns, 0, "scroll={scroll}");
        }
    }

    #[test]
    fn window_expands_by_buffer_rows_in_the_middle() {
        let g = geom();
        // 滚到第 10 行（content y = pad + 10*row_h）
        let scroll = 8.0 + 10.0 * 146.0;
        let w = g.window(scroll, 640.0, 1000);
        assert_eq!(w.first_row, 8); // 10 - 2
                                    // 10 + 6 + 2 = 18 行
        assert_eq!(w.first_index, 8 * 5);
        assert_eq!(w.len, 10 * 5);
    }

    #[test]
    fn window_clamps_at_bottom_of_library() {
        let g = geom();
        // 100 项 = 20 行；滚到最底（过冲）
        let w = g.window(1_000_000.0, 640.0, 100);
        let rows = g.row_count(100);
        assert_eq!(
            w.first_row + (w.len / g.columns),
            rows,
            "窗口右端应贴近库尾"
        );
        assert_eq!(w.end_index(), 100);
        // 最后一行的条目数应被 item_count 截断
        assert_eq!(w.first_index, (rows - 8) * 5);
        assert_eq!(w.len, 8 * 5);
    }

    #[test]
    fn window_at_bottom_never_leaves_a_hole_above_the_viewport() {
        let g = geom();
        let rows = g.row_count(1000);
        // 视口实际能显示的行数（含底部被部分裁切的一行）
        let visible = (640.0 / g.row_height()).ceil() as usize + 1;
        let w = g.window(1e9, 640.0, 1000);
        // 回归：曾用 `rows - 1` 夹紧起点，会在滚到底时把视口上方若干可见行排除在窗口外。
        assert!(
            w.first_row <= rows - visible,
            "底部窗口起点不应晚于视口顶端所在行: {w:?}"
        );
        assert_eq!(w.end_index(), 1000, "底部窗口应覆盖到库尾");
    }

    #[test]
    fn window_for_small_library_covers_everything() {
        let g = geom();
        let w = g.window(0.0, 640.0, 3);
        assert_eq!(w.first_index, 0);
        assert_eq!(w.len, 3);
        assert_eq!(w.first_row, 0);
    }

    #[test]
    fn negative_scroll_is_clamped() {
        let g = geom();
        // 过冲回弹（负偏移）不应产生下溢 panic
        let w = g.window(-500.0, 640.0, 200);
        assert_eq!(w.first_index, 0);
        assert_eq!(w.first_row, 0);
    }

    #[test]
    fn zero_viewport_still_keeps_buffer_rows() {
        let g = geom();
        let w = g.window(0.0, 0.0, 200);
        // ceil(0)=0 + 1 = 1 可见行 + 2 缓冲 = 3 行
        assert_eq!(w.len, 3 * 5);
    }

    #[test]
    fn window_scrolls_monotonically_with_offset() {
        let g = geom();
        let mut prev = GridWindow::EMPTY;
        for step in 0..40 {
            let scroll = step as f64 * 146.0;
            let w = g.window(scroll, 640.0, 1000);
            assert!(
                w.first_index >= prev.first_index,
                "窗口应随滚动单调前移：step={step}"
            );
            prev = w;
        }
    }

    #[test]
    fn window_bounds_hold_across_columns_count_and_viewport() {
        // 4.1：跨列数 / 条目数 / 视口高度 / 滚动偏移的不变量扫描，专防 usize 下溢与越界。
        let base = geom();
        for columns in [1usize, 3, 5, 7] {
            let mut g = base;
            g.columns = columns;
            for count in [0usize, 1, 4, 5, 6, 37, 100] {
                for vh in [0.0_f64, 1.0, 200.0, 640.0, 4000.0] {
                    for sy in [-100.0_f64, 0.0, 50.0, 500.0, 1_000_000.0] {
                        let w = g.window(sy, vh, count);
                        let ctx = format!("columns={columns} count={count} vh={vh} sy={sy}: {w:?}");
                        assert!(w.first_index <= w.end_index(), "右端不应小于起点 {ctx}");
                        assert!(w.end_index() <= count, "窗口不应越界 {ctx}");
                        assert_eq!(w.first_index % columns, 0, "窗口起点应行对齐 {ctx}");
                        if count == 0 {
                            assert_eq!(w, GridWindow::EMPTY, "空库应为空窗口 {ctx}");
                        } else {
                            assert!(w.len > 0, "非空库应至少覆盖一行 {ctx}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn window_tuple_is_not_a_sufficient_cache_key_across_column_counts() {
        // 回归：小库（条目数不足一屏 + 缓冲）下，行对齐窗口元组会被条目数夹到同一值，
        // 因而与列数无关——此处 4 列与 3 列都得到同一窗口。故"仅列数变化"会被以窗口为键
        // 的短路判定误判为"无变化"，上层必须显式失效缓存（见 `recompute_columns`），
        // 否则卡片会按旧列数落点渲染（4 列布局放进 640px 网格时末列被 clip 裁掉）。
        let mut g = GridGeometry::default();
        let four = g.window(0.0, 500.0, 10);
        g.columns = 3;
        let three = g.window(0.0, 500.0, 10);
        assert_eq!(
            four, three,
            "小库下窗口元组应与列数无关（这正是窗口不能单独作为缓存键的原因）"
        );
        assert_eq!(
            four,
            GridWindow {
                first_index: 0,
                len: 10,
                first_row: 0
            }
        );
    }

    #[test]
    fn columns_for_width_matches_content_width_formula() {
        // card_w=200, gap=12, pad=8 → 单元宽 212。
        // 960: usable=960-16+12=956, 956/212=4.50 → 4
        // 720: usable=716, 716/212=3.37 → 3
        // 1280: usable=1276, 1276/212=6.01 → 6
        assert_eq!(columns_for_width(960.0, 200.0, 12.0, 8.0), 4);
        assert_eq!(columns_for_width(720.0, 200.0, 12.0, 8.0), 3);
        assert_eq!(columns_for_width(1280.0, 200.0, 12.0, 8.0), 6);
    }

    #[test]
    fn columns_for_width_has_lower_bound_one() {
        assert_eq!(columns_for_width(100.0, 200.0, 12.0, 8.0), 1);
        assert_eq!(columns_for_width(0.0, 200.0, 12.0, 8.0), 1);
        assert_eq!(columns_for_width(-50.0, 200.0, 12.0, 8.0), 1);
    }

    #[test]
    fn columns_for_width_handles_degenerate_inputs() {
        // 单元宽度非正 → 1
        assert_eq!(columns_for_width(960.0, 0.0, 0.0, 8.0), 1);
        // 非有限可用宽度 → 1（不 panic）
        assert_eq!(columns_for_width(f64::NAN, 200.0, 12.0, 8.0), 1);
        assert_eq!(columns_for_width(f64::INFINITY, 200.0, 12.0, 8.0), 1);
    }

    #[test]
    fn default_geometry_fits_default_window() {
        let g = GridGeometry::default();
        let content_w = 2.0 * g.pad + g.columns as f64 * g.card_w + (g.columns - 1) as f64 * g.gap;
        assert!(
            content_w <= 960.0,
            "默认几何在 960 宽窗口内不应横向溢出：content_w={content_w}"
        );
    }
}
