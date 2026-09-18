import { getArrangement } from "../ipc";
import type { Arrangement, DisplayInfo } from "../types";
import { log } from "../utils/logger";

// ═══════════════════════════════════════════════════════════════════════════════
// 多屏排列（顶层布局策略，布局域）
//
// 本模块承接"多屏排列"的顶层逻辑：合法的排列值校验（isArrangement）、
// 依据排列解析调度单元 key 列表（unitKeysForArrangement）、当前排列读取
// （getCurrentArrangement）与选择器同步（syncArrangementSelect）。
//
// 依赖方向：本模块不依赖轮换模块；轮换 UI（rotation.ts）与设置面板
// （config-panel.ts）反向消费本模块 —— 体现"多屏排列是布局策略层、轮换是消费方"
// 的模块定位（DR-2 布局域）。
//
// 选择器 change 事件仍按"事件绑定集中"约定在 main.ts init() 中绑定（调
// updateArrangement + 重建单元配置）；本模块只提供纯逻辑与控件同步。
// ═══════════════════════════════════════════════════════════════════════════════

/// 合法的多屏排列集合（与 Arrangement 类型保持同步；仅本模块内部使用）
const ARRANGEMENTS: readonly Arrangement[] = ["per_monitor", "all_same", "span"];

/** 判断字符串是否为合法多屏排列；用于 select.value（string）到 Arrangement 的安全窄化 */
export function isArrangement(v: string): v is Arrangement {
  return (ARRANGEMENTS as readonly string[]).includes(v);
}

/**
 * 依据多屏排列解析调度单元 key ± 标签（对齐 backend validate_unit_key DR-40）：
 * - PerMonitor：每个显示器一个单元（key=显示器 id）
 * - AllSame / Span：唯全局单元 "all"
 */
export function unitKeysForArrangement(
  arrangement: Arrangement,
  displays: readonly DisplayInfo[],
): Array<{ key: string; label: string }> {
  if (arrangement === "per_monitor") {
    if (displays.length === 0) return [];
    return displays.map((d) => ({
      key: d.id,
      label: d.is_primary ? `[主] ${d.name}` : d.name,
    }));
  }
  return [{ key: "all", label: "全部屏幕" }];
}

/** 读取当前多屏排列；失败时回退默认 per_monitor（对齐旧 resolveArrangement 语义）。 */
export async function getCurrentArrangement(): Promise<Arrangement> {
  try {
    return await getArrangement();
  } catch (e) {
    log.warn("读取多屏排列失败，使用默认 per_monitor", e);
    return "per_monitor";
  }
}

/**
 * 同步多屏排列选择器显示值（初始加载与 config-changed 事件复用）。
 * 控件不存在时静默跳过（页面结构未挂载 / 精简版页面）。
 */
export function syncArrangementSelect(arrangement: Arrangement): void {
  const select = document.getElementById("arrangement-select") as HTMLSelectElement | null;
  if (select) select.value = arrangement;
}
