//! 卡片键盘可达性的**纯逻辑**（SubTask 9.7.2）。
//!
//! 键盘焦点在**过滤后的可见序列**上推进（索引 ∈ `[0, filtered_count)`），与虚拟化网格
//! 共用同一套几何。本模块只做两件事且**不依赖 Slint、不做 IO**：
//!
//! 1. [`move_focus`]：按方向键 / Tab 计算新的焦点索引；
//! 2. [`scroll_to_show`]：把焦点卡滚动进可见区所需的目标滚动偏移。
//!
//! 二者都能在无 GUI 环境下直接单测（空库、越界、首末边界、行列折返）。

use crate::grid::GridGeometry;

/// 一次焦点移动的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    /// 在过滤序列上前移一格（序列首夹住；可能跨行）。
    Left,
    /// 在过滤序列上后移一格（序列末夹住；可能跨行）。
    Right,
    /// 上移一行。
    Up,
    /// 下移一行。
    Down,
    /// 下一项（Tab；循环）。
    Next,
    /// 上一项（Shift+Tab；循环）。
    Prev,
}

/// 计算新的焦点索引。
///
/// - `current`：当前焦点索引（`None`，或越界时视为"无焦点"）。无焦点时正向动作落到首项、
///   反向动作落到末项，保证第一次按键即可进入网格。
/// - `count`：过滤后条目数；为 0 时返回 `None`。
/// - `columns`：列数（≥1）。
///
/// 方向键**不循环**（夹在首/末），Tab / Shift+Tab **循环**：与"方向键在网格内移动、
/// Tab 依次遍历"的直觉一致。
pub fn move_focus(
    current: Option<usize>,
    action: FocusMove,
    count: usize,
    columns: usize,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let columns = columns.max(1);
    let Some(cur) = current.filter(|c| *c < count) else {
        return Some(match action {
            FocusMove::Left | FocusMove::Up | FocusMove::Prev => count - 1,
            FocusMove::Right | FocusMove::Down | FocusMove::Next => 0,
        });
    };
    let next = match action {
        FocusMove::Left => cur.saturating_sub(1),
        FocusMove::Right => (cur + 1).min(count - 1),
        FocusMove::Up => cur.saturating_sub(columns),
        FocusMove::Down => (cur + columns).min(count - 1),
        FocusMove::Next => (cur + 1) % count,
        FocusMove::Prev => (cur + count - 1) % count,
    };
    Some(next)
}

/// 计算把第 `index` 项滚动进可见区所需的目标滚动偏移（内容坐标）。
///
/// 卡片已完全可见时返回原 `scroll_y`（不产生无谓滚动）；否则把卡片顶/底对齐到视口并
/// 留出 `pad` 余量。结果夹在 `[0, 内容高度 - 视口高度]`，避免过冲回弹。
pub fn scroll_to_show(
    index: usize,
    geometry: &GridGeometry,
    scroll_y: f64,
    viewport_h: f64,
    count: usize,
) -> f64 {
    if count == 0 || viewport_h <= 0.0 {
        return scroll_y;
    }
    let columns = geometry.columns.max(1);
    let row = index / columns;
    let top = geometry.pad + row as f64 * geometry.row_height();
    let bottom = top + geometry.card_h;

    let mut target = scroll_y;
    if top < scroll_y {
        target = top - geometry.pad;
    } else if bottom > scroll_y + viewport_h {
        target = bottom + geometry.pad - viewport_h;
    }
    let max = (geometry.content_height(count) - viewport_h).max(0.0);
    target.clamp(0.0, max)
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
    fn move_focus_enters_grid_on_first_key() {
        let g = geom();
        assert_eq!(move_focus(None, FocusMove::Right, 10, g.columns), Some(0));
        assert_eq!(move_focus(None, FocusMove::Down, 10, g.columns), Some(0));
        assert_eq!(move_focus(None, FocusMove::Next, 10, g.columns), Some(0));
        assert_eq!(move_focus(None, FocusMove::Left, 10, g.columns), Some(9));
        assert_eq!(move_focus(None, FocusMove::Prev, 10, g.columns), Some(9));
        // 空库：无焦点。
        assert_eq!(move_focus(None, FocusMove::Next, 0, g.columns), None);
        assert_eq!(move_focus(Some(3), FocusMove::Next, 0, g.columns), None);
    }

    #[test]
    fn move_focus_directions_clamp_at_edges() {
        let g = geom();
        // 首项左/上夹住。
        assert_eq!(move_focus(Some(0), FocusMove::Left, 10, g.columns), Some(0));
        assert_eq!(move_focus(Some(0), FocusMove::Up, 10, g.columns), Some(0));
        // 末项右/下夹住。
        assert_eq!(
            move_focus(Some(9), FocusMove::Right, 10, g.columns),
            Some(9)
        );
        assert_eq!(move_focus(Some(9), FocusMove::Down, 10, g.columns), Some(9));
    }

    #[test]
    fn move_focus_up_down_uses_row_stride() {
        let g = geom();
        // 索引 7（第 2 行第 3 列）下移一行 → 12；上移一行 → 2。
        assert_eq!(
            move_focus(Some(7), FocusMove::Down, 20, g.columns),
            Some(12)
        );
        assert_eq!(move_focus(Some(7), FocusMove::Up, 20, g.columns), Some(2));
        // 上移越界夹到同行首行。
        assert_eq!(move_focus(Some(2), FocusMove::Up, 20, g.columns), Some(0));
    }

    #[test]
    fn move_focus_tab_wraps_around() {
        let g = geom();
        assert_eq!(move_focus(Some(9), FocusMove::Next, 10, g.columns), Some(0));
        assert_eq!(move_focus(Some(0), FocusMove::Prev, 10, g.columns), Some(9));
    }

    #[test]
    fn move_focus_treats_out_of_range_current_as_no_focus() {
        let g = geom();
        assert_eq!(
            move_focus(Some(99), FocusMove::Next, 10, g.columns),
            Some(0)
        );
    }

    #[test]
    fn scroll_to_show_leaves_visible_card_untouched() {
        let g = geom();
        // 视口 0..640，卡片 0 行顶部 8、底部 142：完全可见 → 不变。
        let y = scroll_to_show(0, &g, 0.0, 640.0, 100);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn scroll_to_show_moves_card_below_viewport_into_view() {
        let g = geom();
        // 第 10 行：top = 8 + 10*146 = 1468，bottom = 1602；视口 0..640。
        let y = scroll_to_show(50, &g, 0.0, 640.0, 100);
        // target = bottom + pad - viewport = 1602 + 8 - 640 = 970
        assert_eq!(y, 970.0);
        // 校验目标位置下卡片可见：scroll<=top 且 bottom<=scroll+viewport
        let top = 8.0 + 10.0 * g.row_height();
        assert!(y <= top && top + g.card_h <= y + 640.0);
    }

    #[test]
    fn scroll_to_show_moves_card_above_viewport_into_view() {
        let g = geom();
        // 已滚到很低，焦点跳回首项：target = top - pad = 0。
        let y = scroll_to_show(0, &g, 3000.0, 640.0, 100);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn scroll_to_show_clamps_at_bottom() {
        let g = geom();
        let count = 20; // 4 行
        let max = g.content_height(count) - 640.0;
        let y = scroll_to_show(count - 1, &g, 0.0, 640.0, count);
        assert!(y <= max.max(0.0) + f64::EPSILON);
        assert!(y >= 0.0);
    }

    #[test]
    fn scroll_to_show_is_noop_when_empty_or_no_viewport() {
        let g = geom();
        assert_eq!(scroll_to_show(0, &g, 123.0, 640.0, 0), 123.0);
        assert_eq!(scroll_to_show(0, &g, 123.0, 0.0, 100), 123.0);
    }

    #[test]
    fn focus_and_scroll_stay_consistent_on_filtered_subset() {
        // 4.4：过滤子集（非整行）下的方向键步进与"滚动入可见区"。
        let g = geom();
        // 子集 3 项、列 5：下移步长超过子集 → 夹到最后一项；上/下越界夹住
        assert_eq!(move_focus(Some(0), FocusMove::Down, 3, g.columns), Some(2));
        assert_eq!(move_focus(Some(0), FocusMove::Up, 3, g.columns), Some(0));
        assert_eq!(move_focus(Some(2), FocusMove::Down, 3, g.columns), Some(2));
        // 内容不足一屏：无需滚动
        assert_eq!(scroll_to_show(2, &g, 0.0, 640.0, 3), 0.0);

        // 子集 13 项（3 行）、视口 200：末项需滚动入可见区且不超过最大偏移
        let y = scroll_to_show(12, &g, 0.0, 200.0, 13);
        let top = 8.0 + 2.0 * g.row_height();
        assert!(
            y <= top && top + g.card_h <= y + 200.0,
            "末项应完整可见: y={y}"
        );
        let max = (g.content_height(13) - 200.0).max(0.0);
        assert!(y <= max + f64::EPSILON && y >= 0.0);

        // 焦点越界（卡片被删除 / 数据刷新后失效）→ 视为无焦点，可重新进入网格
        assert_eq!(
            move_focus(Some(99), FocusMove::Next, 13, g.columns),
            Some(0)
        );
    }
}
