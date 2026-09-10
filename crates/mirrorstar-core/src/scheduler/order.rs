//! 采样算法（设计 §7，DR-4 / DR-5 / DR-14 / DR-18 / DR-19 / DR-23）。
//!
//! 候选 = "池成员，且类型参与轮换（Web 剔除 DR-18），且文件有效"。Web 不参与
//! 轮换；Image / GIF / Video 参与。池过滤后 < 2 张 → 单元空闲，不轮换（DR-14）。
//!
//! 采样器不因单次采样改变全局；"当前状态" 由调用方显式传入（[`SamplerState`]），
//! 采样后返回抽中 id 并就地推进状态。
//!
//! 为避免公共 API 强依赖 `rand`，内部采用注入式 RNG：核心逻辑走
//! [`sample_next_inner`]（接受确定性闭包，供测试注入），公共包装
//! [`sample_next`] 用内置线性同余 / 混合伪随机数发生器提供真随机。
//!
//! # 前置契约
//! 调用方必须先用 [`filter_candidates`] 剔除 Web 与文件缺失者；本函数假定
//! `candidates` 已是"有效候选"（≥2 且非全 Web）。若误传未过滤候选导致剔除后
//! 袋为空，采样会返回 `None` 而非 panic。

use crate::config::settings::Order;
use crate::wallpaper::WallpaperType;
use std::cell::Cell;

/// 池成员（候选）。
#[derive(Debug, Clone)]
pub struct PoolEntry {
    /// 壁纸 id（池内唯一）
    pub id: String,
    /// 壁纸类型（决定是否参与轮换）
    pub ty: WallpaperType,
    /// 壁纸文件路径
    pub path: String,
}

/// 单元"内存状态"，供顺序 / 洗牌袋共用的游标 / 袋。
///
/// 可序列化式的纯内存状态：游标 id 锚定（DR-5），洗牌袋剩余（已剔除 Web，DR-18）。
#[derive(Debug, Clone, Default)]
pub struct SamplerState {
    /// 游标 id 锚定（顺序算法用，DR-5）
    pub order_cursor: Option<String>,
    /// 洗牌袋剩余（已剔除 Web）
    pub bag_remaining: Vec<String>,
}

/// 剔除 Web 类型（DR-18）与文件缺失者；返回按池顺序的有效候选（克隆）。
pub fn filter_candidates(entries: Vec<&PoolEntry>) -> Vec<PoolEntry> {
    entries
        .into_iter()
        .filter(|e| e.ty != WallpaperType::Web)
        .filter(|e| std::path::Path::new(&e.path).is_file())
        .cloned()
        .collect()
}

/// 公共采样入口（真随机）。
///
/// 语义：抽中 id 后推进 `state`（顺序更新 `order_cursor`；洗牌袋更新
/// `bag_remaining`），返回抽中 id。候选池 < 2 返回 `None`（DR-14）。
pub fn sample_next(
    order: Order,
    candidates: &[PoolEntry],
    state: &mut SamplerState,
) -> Option<String> {
    if matches!(order, Order::Sequential) {
        // 顺序算法不依赖随机数，传一个占位闭包即可（不会调用）。
        return sample_next_inner(order, candidates, state, &mut |_| 0);
    }
    // 洗牌袋 / 纯随机：用内置 PRNG。
    let seed = PRNG_SEED.with(|c| c.get());
    let mut rng = Lcg::new(seed);
    let result = sample_next_inner(order, candidates, state, &mut |bound| {
        (rng.next_u64() % bound as u64) as usize
    });
    PRNG_SEED.with(|c| c.set(rng.0));
    result
}

/// 内部采样入口（注入式 RNG，测试传确定性闭包）。
///
/// `rng(bound) -> usize`：返回 `[0, bound)` 内的索引。
fn sample_next_inner(
    order: Order,
    candidates: &[PoolEntry],
    state: &mut SamplerState,
    rng: &mut dyn FnMut(usize) -> usize,
) -> Option<String> {
    match order {
        Order::Sequential => sample_sequential(candidates, state),
        Order::ShuffleBag => sample_shuffle_bag(candidates, state, rng),
        Order::PseudoRandom => sample_pseudo_random(candidates, rng),
    }
}

/// 顺序循环（DR-5 / C2 / 顺序循环回绕）。
///
/// - 游标存在且其 id 在候选中：取其**下一个**候选（结束绕回第一个）。
/// - 游标指向的 id 已不在候选（被删 / 被过滤）：**前跳**到第一个有效候选（C2）。
/// - 无游标：取第一个候选。抽中 id 设为新游标。
fn sample_sequential(candidates: &[PoolEntry], state: &mut SamplerState) -> Option<String> {
    if candidates.len() < 2 {
        return None;
    }
    let drawn = match state.order_cursor.as_deref() {
        Some(cursor) => {
            if let Some(idx) = candidates.iter().position(|c| c.id == cursor) {
                // 取下一个，结束绕回第一个（顺序循环）
                let next_idx = (idx + 1) % candidates.len();
                candidates[next_idx].id.clone()
            } else {
                // C2：游标指向已删 / 缺频 → 前跳第一个有效候选
                candidates[0].id.clone()
            }
        }
        None => candidates[0].id.clone(),
    };
    state.order_cursor = Some(drawn.clone());
    Some(drawn)
}

/// 洗牌袋（DR-18 / C1 / C4）。
///
/// - 袋空基于候选重新打乱成袋（初始化即剔除 Web，DR-18）。
/// - 抽中一个移出袋；袋内不重复，袋空重洗（C4）。
/// - 袋内残留的已删 / 缺频件在抽取前剔除（C1，对应当前候选快照）。
fn sample_shuffle_bag(
    candidates: &[PoolEntry],
    state: &mut SamplerState,
    rng: &mut dyn FnMut(usize) -> usize,
) -> Option<String> {
    if candidates.len() < 2 {
        return None;
    }
    let valid_ids: std::collections::HashSet<&str> =
        candidates.iter().map(|c| c.id.as_str()).collect();
    // C1：剔除袋内已不在候选中的残留 id（已删 / 缺文件）。
    state.bag_remaining.retain(|id| valid_ids.contains(id.as_str()));
    if state.bag_remaining.is_empty() {
        // 重新成袋，剔除 Web（DR-18）。
        state.bag_remaining = candidates
            .iter()
            .filter(|e| e.ty != WallpaperType::Web)
            .map(|e| e.id.clone())
            .collect();
        shuffle_mut(&mut state.bag_remaining, rng);
    }
    // 防御：剔除 Web 后袋仍为空（如调用方误传未过滤候选）→ 返回 None，
    // 避免 `rng(0)` 除零 / `remove(0)` 越界 panic。合法路径袋 ≥2 不触发。
    if state.bag_remaining.is_empty() {
        return None;
    }
    let idx = rng(state.bag_remaining.len());
    Some(state.bag_remaining.remove(idx))
}

/// 纯随机（DR-4 / C3）：每次独立抽取，可能连续同张。
fn sample_pseudo_random(
    candidates: &[PoolEntry],
    rng: &mut dyn FnMut(usize) -> usize,
) -> Option<String> {
    if candidates.len() < 2 {
        return None;
    }
    let idx = rng(candidates.len());
    Some(candidates[idx].id.clone())
}

/// 池增删 / 重排 / 换 active_pool 时的整体失效重建（DR-23）。
///
/// 清空洗牌袋；游标保留 `current_id`（若给出）作为继续锚点，否则置 `None`。
/// 当前壁纸尽量保留。
pub fn invalidate_sampler(state: &mut SamplerState, current_id: Option<&str>) {
    state.bag_remaining.clear();
    state.order_cursor = current_id.map(str::to_owned);
}

/// Fisher–Yates 打乱（原地）。
fn shuffle_mut<T>(slice: &mut [T], rng: &mut dyn FnMut(usize) -> usize) {
    for i in (1..slice.len()).rev() {
        let j = rng(i + 1);
        slice.swap(i, j.min(i));
    }
}

/// 内置线性同余 / 混合 PRNG（splitmix64），用于公共包装的真随机。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
}

/// 每线程独立的 PRNG 种子（从系统时间推算，足够"真随机"）。
fn seed_from_system() -> u64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    elapsed ^ std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

thread_local! {
    static PRNG_SEED: Cell<u64> = Cell::new(seed_from_system());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, ty: WallpaperType) -> PoolEntry {
        PoolEntry {
            id: id.to_string(),
            ty,
            // 采样测试直接传入"已过滤"候选，path 未真实存在不影响 sampling。
            path: format!("C:/fake/{id}"),
        }
    }

    fn seq_candidates() -> Vec<PoolEntry> {
        ["a", "b", "c", "d"]
            .iter()
            .map(|s| entry(s, WallpaperType::Image))
            .collect()
    }

    // ── filter_candidates：Web 剔除（DR-18）+ 文件缺失剔除 ──────────────

    #[test]
    fn filter_candidates_drops_web_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.png");
        std::fs::write(&img, b"x").unwrap();
        let gif = dir.path().join("b.gif");
        std::fs::write(&gif, b"x").unwrap();

        let e1 = PoolEntry {
            id: "a".into(),
            ty: WallpaperType::Image,
            path: img.to_string_lossy().into_owned(),
        };
        let e2 = PoolEntry {
            id: "b".into(),
            ty: WallpaperType::Gif,
            path: gif.to_string_lossy().into_owned(),
        };
        let e3 = PoolEntry {
            id: "web".into(),
            ty: WallpaperType::Web,
            path: "C:/fake/web".into(),
        };
        let e4 = PoolEntry {
            id: "missing".into(),
            ty: WallpaperType::Image,
            path: "C:/fake/not_exists.png".into(),
        };

        let filtered = filter_candidates(vec![&e1, &e2, &e3, &e4]);
        assert_eq!(filtered.len(), 2, "应剔除 Web 与文件缺失者");
        assert_eq!(filtered[0].id, "a");
        assert_eq!(filtered[1].id, "b");
    }

    // ── Sequential（DR-5 / C2 / 顺序循环）────────────────────────────────

    #[test]
    fn sequential_advances_from_cursor() {
        let candidates = seq_candidates();
        let mut state = SamplerState {
            order_cursor: Some("a".into()),
            ..SamplerState::default()
        };
        let next = sample_next_inner(Order::Sequential, &candidates, &mut state, &mut |_| 0);
        assert_eq!(next.as_deref(), Some("b"), "a 之后应取 b");
        assert_eq!(state.order_cursor.as_deref(), Some("b"), "抽中 id 应设为新游标");
    }

    #[test]
    fn sequential_cursor_not_in_candidates_jumps_to_first() {
        // C2：游标指向已删 / 缺频 id → 前跳第一个有效候选
        let candidates = seq_candidates();
        let mut state = SamplerState {
            order_cursor: Some("z".into()),
            ..SamplerState::default()
        };
        let next = sample_next_inner(Order::Sequential, &candidates, &mut state, &mut |_| 0);
        assert_eq!(next.as_deref(), Some("a"));
    }

    #[test]
    fn sequential_wraps_around_after_last() {
        // 顺序循环：末尾绕回到第一个
        let candidates = seq_candidates();
        let mut state = SamplerState {
            order_cursor: Some("d".into()),
            ..SamplerState::default()
        };
        let next = sample_next_inner(Order::Sequential, &candidates, &mut state, &mut |_| 0);
        assert_eq!(next.as_deref(), Some("a"));
    }

    #[test]
    fn sequential_no_cursor_starts_at_first() {
        let candidates = seq_candidates();
        let mut state = SamplerState::default();
        let next = sample_next_inner(Order::Sequential, &candidates, &mut state, &mut |_| 0);
        assert_eq!(next.as_deref(), Some("a"));
    }

    // ── ShuffleBag（C4 袋内不重复 / 袋空重洗，DR-18 剔除 Web，C1 剔残）────

    #[test]
    fn shuffle_bag_no_repeat_until_bag_empty() {
        let candidates = vec![
            entry("a", WallpaperType::Image),
            entry("b", WallpaperType::Image),
            entry("c", WallpaperType::Image),
        ];
        let mut state = SamplerState::default();
        // 确定性 rng：恒取最小索引（测试打乱 + 抽取的确定性）
        let mut rng = |_n: usize| 0usize;

        let mut drawn = Vec::new();
        for _ in 0..candidates.len() {
            let d = sample_next_inner(Order::ShuffleBag, &candidates, &mut state, &mut rng)
                .expect("袋内应有候选");
            assert!(!drawn.contains(&d), "袋内不应重复，重复={d}");
            drawn.push(d);
        }
        // 袋空 → 第四次调用应重洗（C4），仍返回合法候选
        let d4 = sample_next_inner(Order::ShuffleBag, &candidates, &mut state, &mut rng)
            .expect("袋空应重洗");
        assert!(["a", "b", "c"].contains(&d4.as_str()));
    }

    #[test]
    fn shuffle_bag_excludes_web_on_init() {
        // DR-18：初始化即剔除 Web，不靠运行时撞上再跳
        let candidates = vec![
            entry("a", WallpaperType::Image),
            entry("web", WallpaperType::Web),
            entry("b", WallpaperType::Image),
        ];
        let mut state = SamplerState::default();
        let mut rng = |_n: usize| 0usize;
        for _ in 0..8 {
            let d = sample_next_inner(Order::ShuffleBag, &candidates, &mut state, &mut rng)
                .expect("至少 2 个有效候选");
            assert_ne!(d, "web", "洗牌袋不应抽出 Web");
        }
    }

    #[test]
    fn shuffle_bag_unfiltered_all_web_returns_none_not_panic() {
        // 前置契约防御：误传未过滤候选（全 Web、len≥2）→ 重建袋剔除 Web 后为空，
        // 应返回 None，而非 `rng(0)` 除零 / `remove(0)` 越界 panic。
        let candidates = vec![
            entry("web1", WallpaperType::Web),
            entry("web2", WallpaperType::Web),
        ];
        let mut state = SamplerState::default();
        // 注入确定性 rng：若防御缺失，`rng(0)` 会除零 panic（此处因返回 None 不会被调用）。
        let mut rng = |_n: usize| 0usize;
        assert_eq!(
            sample_next_inner(Order::ShuffleBag, &candidates, &mut state, &mut rng),
            None,
            "全 Web 未过滤候选应返回 None 而非 panic"
        );
    }

    #[test]
    fn shuffle_bag_skips_deleted_entry() {
        // C1：抽到已删 / 缺文件 → 袋内移除重抽
        let candidates = vec![
            entry("a", WallpaperType::Image),
            entry("b", WallpaperType::Image),
            entry("c", WallpaperType::Image),
        ];
        let mut state = SamplerState::default();
        let mut rng = |_n: usize| 0usize;
        // 首抽填充袋子
        let _ = sample_next_inner(Order::ShuffleBag, &candidates, &mut state, &mut rng);

        // 模拟 "a" 被删除：候选快照移除 a（剩 b,c，仍 ≥2 可采样）
        let shrunk: Vec<PoolEntry> = candidates.into_iter().filter(|c| c.id != "a").collect();
        assert_eq!(shrunk.len(), 2);
        for _ in 0..6 {
            let d = sample_next_inner(Order::ShuffleBag, &shrunk, &mut state, &mut rng)
                .expect("剩 2 个候选应能采样");
            assert_ne!(d, "a", "已删条目不应再被抽出");
        }
    }

    // ── PseudoRandom（C3）──────────────────────────────────────────────

    #[test]
    fn pseudo_random_can_repeat_consecutively() {
        // rng 恒取 0 → 恒抽 candidates[0]，证明"可连续同张"是被接受特性
        let candidates = vec![
            entry("a", WallpaperType::Image),
            entry("b", WallpaperType::Image),
        ];
        let mut state = SamplerState::default();
        let mut rng = |_n: usize| 0usize;
        let d1 = sample_next_inner(Order::PseudoRandom, &candidates, &mut state, &mut rng);
        let d2 = sample_next_inner(Order::PseudoRandom, &candidates, &mut state, &mut rng);
        assert_eq!(d1, Some("a".to_string()));
        assert_eq!(d2, Some("a".to_string()));
        // 纯随机不推进任何游标 / 袋
        assert!(state.order_cursor.is_none());
        assert!(state.bag_remaining.is_empty());
    }

    // ── DR-14：候选 < 2 → None ─────────────────────────────────────────

    #[test]
    fn insufficient_candidates_returns_none() {
        let one = vec![entry("a", WallpaperType::Image)];
        let mut state = SamplerState::default();
        let mut rng = |_n: usize| 0usize;
        for order in [Order::Sequential, Order::ShuffleBag, Order::PseudoRandom] {
            assert_eq!(
                sample_next_inner(order, &one, &mut state, &mut rng),
                None,
                "{order:?} 候选 < 2 应返回 None（DR-14）"
            );
        }
        let empty: Vec<PoolEntry> = vec![];
        assert_eq!(sample_next_inner(Order::ShuffleBag, &empty, &mut state, &mut rng), None);
    }

    // ── invalidate_sampler（DR-23）─────────────────────────────────────

    #[test]
    fn invalidate_sampler_clears_bag_keeps_current_anchor() {
        let mut state = SamplerState {
            order_cursor: Some("x".into()),
            bag_remaining: vec!["a".into(), "b".into(), "x".into()],
        };
        // 给出 current_id：清袋、保留游标锚点
        invalidate_sampler(&mut state, Some("x"));
        assert!(state.bag_remaining.is_empty(), "袋应清空重建");
        assert_eq!(state.order_cursor.as_deref(), Some("x"), "游标应保留 current_id");
        // 无 current_id：游标置 None
        invalidate_sampler(&mut state, None);
        assert!(state.order_cursor.is_none());
    }
}