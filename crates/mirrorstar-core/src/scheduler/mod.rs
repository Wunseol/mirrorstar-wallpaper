//! 壁纸轮换调度器（纯逻辑层，设计 §7 / §9）。
//!
//! 本期落地两个纯 Rust 逻辑子模块，不依赖 Windows API / tokio：
//! - [`order`]：采样算法（顺序循环 / 洗牌袋 / 纯随机），候选过滤与状态推进。
//! - [`boot`]：开机 / 唤醒解析决策树（DR-12 / DR-18 / DR-21）。
//!
//! 渲染执行完全下放给现有 `set_wallpaper` 三阶段流程（设计 §2），本模块只做
//! "决策"（WHICH / HOW），不触碰任何渲染层。

pub mod boot;
pub mod order;

pub use boot::{resolve_boot, BootDecision};
pub use order::{filter_candidates, invalidate_sampler, sample_next, PoolEntry, SamplerState};