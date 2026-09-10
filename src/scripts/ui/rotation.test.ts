import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import ./rotation 之前声明（vitest 会自动提升到文件顶部）
// 屏蔽 ipc，使轮换模块内各 IPC 调用完全可控
vi.mock("../ipc", () => ({
  createPool: vi.fn(),
  deletePool: vi.fn(),
  getDisplays: vi.fn(),
  getRotationConfig: vi.fn(),
  getUnitStates: vi.fn(),
  listPools: vi.fn(),
  nextWallpaper: vi.fn(),
  setActivePool: vi.fn(),
  setRotationEnabled: vi.fn(),
  updatePool: vi.fn(),
  updateRotationConfig: vi.fn(),
}));

// 屏蔽 state，避免 getter 告警并可控地同步 pools / allWallpapers
vi.mock("../state", () => ({
  appState: {
    _selectedDisplayId: "",
    allWallpapers: [] as unknown[],
    currentPreviewId: null as string | null,
    pools: [] as unknown[],
    get selectedDisplayId() { return this._selectedDisplayId; },
    set selectedDisplayId(v: string) { this._selectedDisplayId = v; },
  },
}));

// 屏蔽 logger，保持测试输出整洁
vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

// showStatus 用 mock，便于断言错误/成功提示
vi.mock("./utils", () => ({
  showStatus: vi.fn(),
}));

import { log } from "../utils/logger";
import { showStatus } from "./utils";
import { appState } from "../state";
import {
  createPool,
  deletePool,
  getDisplays,
  getRotationConfig,
  getUnitStates,
  listPools,
  nextWallpaper,
  setActivePool,
  setRotationEnabled,
  updatePool,
  updateRotationConfig,
} from "../ipc";
import type { DisplayInfo, Pool, RotationConfig, WallpaperEntry } from "../types";
import {
  createPoolFromInput,
  dedupeMembers,
  getPoolList,
  isOrder,
  loadPools,
  orderLabel,
  patchRotation,
  poolNamesForWallpaper,
  reorderMembers,
  renderUnitConfig,
  setupNextWallpaperButton,
  setupPoolCreate,
  unitKeysForArrangement,
} from "./rotation";

// ── 测试数据 ────────────────────────────────────────────────────────────────────

const sampleWallpapers: WallpaperEntry[] = [
  {
    id: "w1",
    file_path: "C:/wallpapers/山.png",
    wallpaper_type: "Image",
    display_id: null,
    added_at: "2026-01-01T00:00:00Z",
    thumbnail: "",
    file_size: 1024,
    metadata: null,
    groups: [],
  },
  {
    id: "w2",
    file_path: "C:/wallpapers/海.png",
    wallpaper_type: "Image",
    display_id: null,
    added_at: "2026-01-01T00:00:00Z",
    thumbnail: "",
    file_size: 2048,
    metadata: null,
    groups: [],
  },
];

function samplePool(overrides: Partial<Pool> = {}): Pool {
  return { id: "p1", name: "风景", member_ids: ["w1"], ...overrides };
}

function samplePools(): Pool[] {
  return [samplePool()];
}

function sampleDisplays(): DisplayInfo[] {
  return [
    { id: "d1", name: "显示器 1", width: 1920, height: 1080, x: 0, y: 0, is_primary: true, dpi: 1, current_wallpaper: null },
    { id: "d2", name: "显示器 2", width: 1920, height: 1080, x: 0, y: 0, is_primary: false, dpi: 1, current_wallpaper: null },
  ];
}

function baseRotation(): RotationConfig {
  return {
    enabled: false,
    on_boot: false,
    interval_minutes: 30,
    order: "sequential",
    arrangement: "per_monitor",
  };
}

/** 构造一个可控的 deferred，用于精细控制 mock 的 resolve 时机 */
function createDeferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** 等待一个 macrotask，确保此前排队的所有 microtask 都已执行完毕 */
function flushMicrotasks(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

beforeEach(() => {
  vi.clearAllMocks();
  document.body.innerHTML = "";
  appState.allWallpapers = sampleWallpapers;
  appState.pools = [];
  // IPC 默认成功返回
  vi.mocked(listPools).mockResolvedValue(samplePools());
  vi.mocked(getRotationConfig).mockResolvedValue(baseRotation());
  vi.mocked(getDisplays).mockResolvedValue(sampleDisplays());
  vi.mocked(updatePool).mockResolvedValue(undefined);
  vi.mocked(deletePool).mockResolvedValue(undefined);
  vi.mocked(createPool).mockResolvedValue(samplePool());
  vi.mocked(updateRotationConfig).mockResolvedValue(undefined);
  vi.mocked(setActivePool).mockResolvedValue(undefined);
  vi.mocked(setRotationEnabled).mockResolvedValue(undefined);
  vi.mocked(nextWallpaper).mockResolvedValue(undefined);
  // 默认无后端状态（空数组）→ 不回填，避免覆盖既有测试的默认渲染断言
  vi.mocked(getUnitStates).mockResolvedValue([]);
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

// ── 纯函数（既有断言风格）──────────────────────────────────────────────────────

describe("isOrder", () => {
  it("接受三个合法 Order 值", () => {
    expect(isOrder("sequential")).toBe(true);
    expect(isOrder("shuffle_bag")).toBe(true);
    expect(isOrder("pseudo_random")).toBe(true);
  });

  it("拒绝未知值与非字符串", () => {
    expect(isOrder("random")).toBe(false);
    expect(isOrder("")).toBe(false);
    expect(isOrder("Sequential")).toBe(false);
  });
});

describe("orderLabel", () => {
  it("映射顺序循环 / 洗牌袋 / 纯随机", () => {
    expect(orderLabel("sequential")).toBe("顺序循环");
    expect(orderLabel("shuffle_bag")).toBe("洗牌袋");
    expect(orderLabel("pseudo_random")).toBe("纯随机");
  });

  it("未知值原样返回（防御性）", () => {
    expect(orderLabel("unknown" as never)).toBe("unknown");
  });
});

describe("poolNamesForWallpaper", () => {
  const pools: readonly Pool[] = [
    { id: "p1", name: "风景", member_ids: [] },
    { id: "p2", name: "动漫", member_ids: [] },
    { id: "p3", name: "已删除", member_ids: [] },
  ];

  it("按 groups 中 id 映射为池名，顺序保持一致", () => {
    expect(poolNamesForWallpaper(["p1", "p2"], pools)).toEqual(["风景", "动漫"]);
  });

  it("跳过已不存在的池 id", () => {
    expect(poolNamesForWallpaper(["p1", "p9", "p2"], pools)).toEqual(["风景", "动漫"]);
  });

  it("重复 id 保留映射（与 groups 一一对应）", () => {
    expect(poolNamesForWallpaper(["p1", "p1"], pools)).toEqual(["风景", "风景"]);
  });

  it("groups 为空 / 未定义 / null 时返回空数组", () => {
    expect(poolNamesForWallpaper([], pools)).toEqual([]);
    expect(poolNamesForWallpaper(undefined, pools)).toEqual([]);
    expect(poolNamesForWallpaper(null, pools)).toEqual([]);
  });

  it("全部 id 无法解析时返回空数组", () => {
    expect(poolNamesForWallpaper(["p9"], pools)).toEqual([]);
  });
});

describe("dedupeMembers", () => {
  it("去重并保留首次出现顺序", () => {
    expect(dedupeMembers(["a", "b", "a", "c", "b"])).toEqual(["a", "b", "c"]);
  });

  it("无重复时返回原顺序", () => {
    expect(dedupeMembers(["x", "y", "z"])).toEqual(["x", "y", "z"]);
  });

  it("空数组返回空数组", () => {
    expect(dedupeMembers([])).toEqual([]);
  });
});

describe("reorderMembers", () => {
  it("把元素从 from 移到 to", () => {
    expect(reorderMembers(["a", "b", "c", "d"], 0, 2)).toEqual(["b", "c", "a", "d"]);
    expect(reorderMembers(["a", "b", "c", "d"], 2, 0)).toEqual(["c", "a", "b", "d"]);
  });

  it("from === to 返回原数组副本且不改变内容", () => {
    const ids = ["a", "b", "c"];
    const out = reorderMembers(ids, 1, 1);
    expect(out).toEqual(["a", "b", "c"]);
    expect(out).not.toBe(ids);
  });

  it("索引越界返回原数组副本", () => {
    const ids = ["a", "b", "c"];
    expect(reorderMembers(ids, -1, 0)).toEqual(ids);
    expect(reorderMembers(ids, 3, 0)).toEqual(ids);
    expect(reorderMembers(ids, 0, 3)).toEqual(ids);
    expect(reorderMembers(ids, 1, 99)).toEqual(ids);
  });

  it("不修改原数组", () => {
    const ids = ["a", "b", "c"];
    reorderMembers(ids, 0, 2);
    expect(ids).toEqual(["a", "b", "c"]);
  });
});

describe("unitKeysForArrangement", () => {
  const displays: DisplayInfo[] = sampleDisplays();

  it("PerMonitor 每个显示器一个单元（主屏加标注）", () => {
    expect(unitKeysForArrangement("per_monitor", displays)).toEqual([
      { key: "d1", label: "[主] 显示器 1" },
      { key: "d2", label: "显示器 2" },
    ]);
  });

  it("PerMonitor 无显示器时返回空数组", () => {
    expect(unitKeysForArrangement("per_monitor", [])).toEqual([]);
  });

  it("AllSame 返回唯一 all 单元", () => {
    expect(unitKeysForArrangement("all_same", displays)).toEqual([{ key: "all", label: "全部屏幕" }]);
  });

  it("Span 返回唯一 all 单元", () => {
    expect(unitKeysForArrangement("span", displays)).toEqual([{ key: "all", label: "全部屏幕" }]);
  });
});

// ── 10.1 patchRotation 串行化 ──────────────────────────────────────────────────

describe("patchRotation 串行化", () => {
  it("浅合并：get_rotation_config + update_rotation_config 传合并后的全量配置", async () => {
    vi.mocked(getRotationConfig).mockResolvedValue(baseRotation());
    vi.mocked(updateRotationConfig).mockResolvedValue(undefined);

    await patchRotation({ enabled: true });

    expect(getRotationConfig).toHaveBeenCalledTimes(1);
    expect(updateRotationConfig).toHaveBeenCalledWith({ ...baseRotation(), enabled: true });
  });

  it("两次快速调用时第二次 getRotationConfig 在第一次 updateRotationConfig 之后才执行", async () => {
    const sequence: string[] = [];
    let getCalls = 0;
    let updCalls = 0;
    const firstUpdDeferred = createDeferred<void>();

    vi.mocked(getRotationConfig).mockImplementation(() => {
      getCalls++;
      sequence.push(`get-${getCalls}`);
      return Promise.resolve(baseRotation());
    });
    vi.mocked(updateRotationConfig).mockImplementation(() => {
      updCalls++;
      sequence.push(`upd-${updCalls}`);
      if (updCalls === 1) {
        return firstUpdDeferred.promise;
      }
      return Promise.resolve();
    });

    const p1 = patchRotation({ enabled: true });
    const p2 = patchRotation({ interval_minutes: 15 });

    await flushMicrotasks();

    // 串行化关键断言：第一次 update 未 resolve 时，第二次 get 尚未执行
    expect(getRotationConfig).toHaveBeenCalledTimes(1);
    expect(updateRotationConfig).toHaveBeenCalledTimes(1);
    expect(sequence).toEqual(["get-1", "upd-1"]);

    firstUpdDeferred.resolve();
    await Promise.all([p1, p2]);

    expect(sequence).toEqual(["get-1", "upd-1", "get-2", "upd-2"]);
  });

  it("第一次 update 失败不阻塞第二次调用，且第一次调用者收到 reject", async () => {
    const sequence: string[] = [];
    let getCalls = 0;
    let updCalls = 0;

    vi.mocked(getRotationConfig).mockImplementation(() => {
      getCalls++;
      sequence.push(`get-${getCalls}`);
      return Promise.resolve(baseRotation());
    });
    vi.mocked(updateRotationConfig).mockImplementation(() => {
      updCalls++;
      sequence.push(`upd-${updCalls}`);
      if (updCalls === 1) {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve();
    });

    const p1 = patchRotation({ enabled: true });
    await expect(p1).rejects.toThrow("ipc failure");

    await flushMicrotasks();
    const p2 = patchRotation({ on_boot: true });
    await expect(p2).resolves.toBeUndefined();

    expect(sequence).toEqual(["get-1", "upd-1", "get-2", "upd-2"]);
  });
});

// ── 10.3 renderUnitConfig：按编排渲染单元配置 ───────────────────────────────────

describe("renderUnitConfig", () => {
  let container: HTMLDivElement;
  let poolContainer: HTMLDivElement;

  beforeEach(() => {
    container = document.createElement("div");
    container.id = "unit-config";
    document.body.appendChild(container);
    // loadPools 需要 #pool-list 容器才能同步 poolList / appState.pools
    poolContainer = document.createElement("div");
    poolContainer.id = "pool-list";
    document.body.appendChild(poolContainer);
    appState.pools = samplePools();
  });

  it("容器不存在时安全返回且不调用 IPC", async () => {
    container.remove();
    await renderUnitConfig();
    expect(getRotationConfig).not.toHaveBeenCalled();
    expect(getDisplays).not.toHaveBeenCalled();
  });

  it("per_monitor 编排：每个显示器渲染一个单元，含激活池下拉与轮换开关", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools(); // 同步 poolList 与 appState.pools
    await renderUnitConfig();

    const items = container.querySelectorAll(".unit-config-item");
    expect(items).toHaveLength(2);
    expect(items[0]?.getAttribute("data-unit-key")).toBe("d1");
    expect(items[1]?.getAttribute("data-unit-key")).toBe("d2");
    expect(items[0]!.querySelector(".unit-config-title")?.textContent).toBe("[主] 显示器 1");
    expect(items[1]!.querySelector(".unit-config-title")?.textContent).toBe("显示器 2");

    // 激活池下拉包含“全部壁纸池”+ 池选项
    const select = items[0]!.querySelector("select.unit-active-pool") as HTMLSelectElement;
    const options = Array.from(select.options).map((o) => o.value);
    expect(options).toEqual(["", "p1"]);
    // 每个单元含轮换开关 checkbox
    expect(items[0]!.querySelector("input.unit-rotation-enabled")).not.toBeNull();
    expect(items[1]!.querySelector("input.unit-rotation-enabled")).not.toBeNull();
  });

  it("all_same / span 编排：渲染唯一 all 单元", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    vi.mocked(getRotationConfig).mockResolvedValue({ ...baseRotation(), arrangement: "all_same" });
    await loadPools();
    await renderUnitConfig();

    const items = container.querySelectorAll(".unit-config-item");
    expect(items).toHaveLength(1);
    expect(items[0]!.getAttribute("data-unit-key")).toBe("all");
    expect(items[0]!.querySelector(".unit-config-title")?.textContent).toBe("全部屏幕");
  });

  it("无池可绑定时仍显示单元开关，激活池下拉仅“全部壁纸池”", async () => {
    vi.mocked(listPools).mockResolvedValue([]);
    await loadPools(); // poolList = []
    await renderUnitConfig();

    const items = container.querySelectorAll(".unit-config-item");
    expect(items).toHaveLength(2);
    for (const item of items) {
      const select = item.querySelector("select.unit-active-pool") as HTMLSelectElement;
      expect(Array.from(select.options).map((o) => o.value)).toEqual([""]);
      expect(item.querySelector("input.unit-rotation-enabled")).not.toBeNull();
    }
  });

  it("选择激活池：非空值调用 set_active_pool(key, poolId)", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools();
    await renderUnitConfig();

    const select = container.querySelector("select.unit-active-pool") as HTMLSelectElement;
    select.value = "p1";
    select.dispatchEvent(new Event("change"));
    await flushMicrotasks();

    expect(setActivePool).toHaveBeenCalledWith("d1", "p1");
  });

  it("选择激活池：空串映射为 null（全部壁纸池 DR-35）", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools();
    await renderUnitConfig();

    const select = container.querySelector("select.unit-active-pool") as HTMLSelectElement;
    expect(select.value).toBe("");
    select.dispatchEvent(new Event("change"));
    await flushMicrotasks();

    expect(setActivePool).toHaveBeenCalledWith("d1", null);
  });

  it("切换单元轮换开关调用 set_rotation_enabled(key, checked)", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools();
    await renderUnitConfig();

    const checkbox = container.querySelector("input.unit-rotation-enabled") as HTMLInputElement;
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event("change"));
    await flushMicrotasks();

    expect(setRotationEnabled).toHaveBeenCalledWith("d1", true);
  });

  it("set_active_pool 失败时展示错误提示", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    vi.mocked(setActivePool).mockRejectedValue(new Error("ipc failure"));
    await loadPools();
    await renderUnitConfig();

    const select = container.querySelector("select.unit-active-pool") as HTMLSelectElement;
    select.value = "p1";
    select.dispatchEvent(new Event("change"));
    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith(expect.stringContaining("设置单元"), "error");
    });
    expect(log.error).toHaveBeenCalled();
  });

  it("读取轮换配置失败时回退 per_monitor 并记录告警", async () => {
    vi.mocked(getRotationConfig).mockRejectedValue(new Error("config fail"));
    await renderUnitConfig();

    expect(log.warn).toHaveBeenCalledWith(expect.stringContaining("读取轮换配置失败"), expect.any(Error));
    // 回退 per_monitor + 默认 displays，渲染 2 个单元
    expect(container.querySelectorAll(".unit-config-item")).toHaveLength(2);
  });

  it("获取显示器列表失败时回退为全局单元（empty displays）", async () => {
    vi.mocked(getDisplays).mockRejectedValue(new Error("displays fail"));
    await renderUnitConfig();

    expect(log.warn).toHaveBeenCalledWith(expect.stringContaining("获取显示器列表失败"), expect.any(Error));
    // currentDisplays=[] 且 per_monitor → 无单元
    expect(container.querySelectorAll(".unit-config-item")).toHaveLength(0);
  });

  it("回读后端状态成功：回填激活池 select 与轮换开关 checkbox（Task 6.2）", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools();
    // 后端真实状态：d1 绑定 p1 池且开启, d2 全部池且关闭
    vi.mocked(getUnitStates).mockResolvedValue([
      { key: "d1", active_pool: "p1", enabled: true },
      { key: "d2", active_pool: null, enabled: false },
    ]);
    await renderUnitConfig();

    const items = container.querySelectorAll(".unit-config-item");
    expect(items).toHaveLength(2);
    const selects = Array.from(
      container.querySelectorAll("select.unit-active-pool"),
    ) as HTMLSelectElement[];
    expect(selects[0]!.value).toBe("p1");
    expect(selects[1]!.value).toBe("");

    const cbs = Array.from(
      container.querySelectorAll("input.unit-rotation-enabled"),
    ) as HTMLInputElement[];
    expect(cbs[0]!.checked).toBe(true);
    expect(cbs[1]!.checked).toBe(false);
  });

  it("回读后端状态失败：提示且不回填（Task 6.3，不静默覆盖）", async () => {
    vi.mocked(listPools).mockResolvedValue(samplePools());
    await loadPools();
    vi.mocked(getUnitStates).mockRejectedValue(new Error("get states fail"));
    await renderUnitConfig();

    expect(getUnitStates).toHaveBeenCalled();
    expect(log.warn).toHaveBeenCalledWith(expect.stringContaining("回读单元配置状态失败"), expect.any(Error));
    // 失败不回填：激活池回退空串、开关回退关，且对用户在指定池场景不静默覆盖。
    const select = container.querySelector("select.unit-active-pool") as HTMLSelectElement;
    expect(select.value).toBe("");
    const checkbox = container.querySelector("input.unit-rotation-enabled") as HTMLInputElement;
    expect(checkbox.checked).toBe(false);
  });
});

// ── 10.2 loadPools 与池编辑器 ──────────────────────────────────────────────────

describe("loadPools", () => {
  let container: HTMLDivElement;

  beforeEach(() => {
    container = document.createElement("div");
    container.id = "pool-list";
    document.body.appendChild(container);
  });

  it("成功：渲染池卡片、同步 appState.pools 与模块缓存、派发 rotation-pools-changed", async () => {
    const pools = samplePools();
    vi.mocked(listPools).mockResolvedValue(pools);
    const eventSpy = vi.fn();
    window.addEventListener("rotation-pools-changed", eventSpy);

    await loadPools();

    // 池卡片渲染
    const cards = container.querySelectorAll(".rotation-pool");
    expect(cards).toHaveLength(1);
    expect(cards[0]!.getAttribute("data-pool-id")).toBe("p1");
    // 同步 state 与缓存
    expect(appState.pools).toBe(pools);
    expect(getPoolList()).toBe(pools);
    // 派发事件由 main.ts 监听 → 刷新壁纸列表
    expect(eventSpy).toHaveBeenCalledTimes(1);

    window.removeEventListener("rotation-pools-changed", eventSpy);
  });

  it("空池列表渲染空状态提示", async () => {
    vi.mocked(listPools).mockResolvedValue([]);
    await loadPools();
    const empty = container.querySelector(".rotation-pool-empty");
    expect(empty).not.toBeNull();
    expect(empty?.textContent).toContain("暂无轮换池");
  });

  it("IPC 失败时展示错误提示且不渲染", async () => {
    vi.mocked(listPools).mockRejectedValue(new Error("load failed"));
    await loadPools();
    expect(showStatus).toHaveBeenCalledWith("加载轮换池失败", "error");
    expect(log.error).toHaveBeenCalledWith(expect.stringContaining("加载轮换池失败"), expect.any(Error));
    expect(container.querySelector(".rotation-pool")).toBeNull();
  });

  it("容器不存在时安全返回且不调用 listPools", async () => {
    container.remove();
    await loadPools();
    expect(listPools).not.toHaveBeenCalled();
  });
});

describe("池编辑器：成员增删 / 重命名 / 删除 / 拖拽", () => {
  let container: HTMLDivElement;

  beforeEach(() => {
    container = document.createElement("div");
    container.id = "pool-list";
    document.body.appendChild(container);
    vi.mocked(listPools).mockResolvedValue(samplePools());
  });

  async function renderPool(): Promise<HTMLElement> {
    await loadPools();
    return container.querySelector(".rotation-pool") as HTMLElement;
  }

  it("添加成员：选中下拉并提交 update_pool（有序去重）", async () => {
    await renderPool();
    const addSelect = container.querySelector("select.rotation-pool-member-add") as HTMLSelectElement;
    // w2 不在成员中；去掉占位符后选中 w2
    expect(Array.from(addSelect.options).map((o) => o.value)).toEqual(["", "w2"]);
    addSelect.value = "w2";
    addSelect.dispatchEvent(new Event("change", { bubbles: true }));

    await vi.waitFor(() => {
      expect(updatePool).toHaveBeenCalledWith("p1", null, ["w1", "w2"]);
    });
    expect(showStatus).toHaveBeenCalledWith("已添加成员", "success");
  });

  it("移除成员：点击移除按钮提交 update_pool 移除对应成员", async () => {
    await renderPool();
    const removeBtn = container.querySelector(".rotation-member-remove") as HTMLButtonElement;
    removeBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));

    await vi.waitFor(() => {
      expect(updatePool).toHaveBeenCalledWith("p1", null, []);
    });
    expect(showStatus).toHaveBeenCalledWith("已移出池", "success");
  });

  it("重命名：prompt 确认后提交 update_pool(id, newName, null)", async () => {
    vi.spyOn(window, "prompt").mockReturnValue("新名称");
    await renderPool();
    const renameBtn = container.querySelector(".rotation-pool-rename") as HTMLButtonElement;
    renameBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));

    await vi.waitFor(() => {
      expect(updatePool).toHaveBeenCalledWith("p1", "新名称", null);
    });
    expect(showStatus).toHaveBeenCalledWith("已重命名", "success");
  });

  it("重命名：prompt 取消（null）或空串时不提交", async () => {
    vi.spyOn(window, "prompt").mockReturnValue(null);
    await renderPool();
    const renameBtn = container.querySelector(".rotation-pool-rename") as HTMLButtonElement;
    renameBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    await flushMicrotasks();
    expect(updatePool).not.toHaveBeenCalled();

    vi.mocked(window.prompt).mockReturnValue("   ");
    renameBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    await flushMicrotasks();
    expect(updatePool).not.toHaveBeenCalled();
  });

  it("删除池：confirm 确认后调用 delete_pool", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    await renderPool();
    const deleteBtn = container.querySelector(".rotation-pool-delete") as HTMLButtonElement;
    deleteBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));

    await vi.waitFor(() => {
      expect(deletePool).toHaveBeenCalledWith("p1");
    });
    expect(showStatus).toHaveBeenCalledWith("轮换池已删除", "success");
  });

  it("删除池：confirm 取消时不调用 delete_pool", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(false);
    await renderPool();
    const deleteBtn = container.querySelector(".rotation-pool-delete") as HTMLButtonElement;
    deleteBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    await flushMicrotasks();
    expect(deletePool).not.toHaveBeenCalled();
  });

  it("成员拖拽：dragstart 记录索引、drop 提交重排", async () => {
    // 池含两个成员以观察重排
    vi.mocked(listPools).mockResolvedValue([{
      id: "p1", name: "风景", member_ids: ["w1", "w2"],
    }]);
    await renderPool();

    const members = container.querySelectorAll(".rotation-member");
    expect(members).toHaveLength(2);
    const first = members[0] as HTMLElement;
    const second = members[1] as HTMLElement;

    // dragstart：写入 data，标记 dragging
    const dataTransfer = {
      setData: vi.fn(),
      getData: vi.fn(() => "0"),
    } as unknown as DataTransfer;
    const dragStartEvent = new Event("dragstart", { bubbles: true, cancelable: true });
    Object.defineProperty(dragStartEvent, "dataTransfer", { value: dataTransfer });
    first.dispatchEvent(dragStartEvent);
    expect(first.classList.contains("dragging")).toBe(true);
    expect(dataTransfer.setData).toHaveBeenCalledWith("text/plain", "0");

    // drop 到第二位：from=0 → to=1 → 重排 ["w2","w1"]
    const dropEvent = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(dropEvent, "dataTransfer", { value: dataTransfer });
    second.dispatchEvent(dropEvent);

    await vi.waitFor(() => {
      expect(updatePool).toHaveBeenCalledWith("p1", null, ["w2", "w1"]);
    });
    expect(showStatus).toHaveBeenCalledWith("已更新播放顺序", "success");

    // dragend 清掉 dragging 标记
    const dragEndEvent = new Event("dragend", { bubbles: true, cancelable: true });
    first.dispatchEvent(dragEndEvent);
    expect(first.classList.contains("dragging")).toBe(false);
  });

  it("update_pool 失败时展示错误提示", async () => {
    vi.mocked(updatePool).mockRejectedValue(new Error("ipc failure"));
    await renderPool();
    const removeBtn = container.querySelector(".rotation-member-remove") as HTMLButtonElement;
    removeBtn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("从池移除成员失败", "error");
    });
    expect(log.error).toHaveBeenCalled();
  });
});

// ── 10.2 createPoolFromInput / setupPoolCreate ────────────────────────────────

describe("createPoolFromInput", () => {
  beforeEach(() => {
    // loadPools 内部依赖 #pool-list 容器，需预先挂载
    const poolContainer = document.createElement("div");
    poolContainer.id = "pool-list";
    document.body.appendChild(poolContainer);
    vi.mocked(listPools).mockResolvedValue(samplePools());
  });

  it("非空名称：createPool(name, []) 并清空输入、提示、重载池", async () => {
    const input = document.createElement("input");
    input.value = "动漫";

    await createPoolFromInput(input);

    expect(createPool).toHaveBeenCalledWith("动漫", []);
    expect(input.value).toBe("");
    expect(showStatus).toHaveBeenCalledWith('已新建池"动漫"', "success");
    expect(listPools).toHaveBeenCalled();
  });

  it("空名称：createPool(null, []) 并提示默认命名", async () => {
    const input = document.createElement("input");
    input.value = "   ";

    await createPoolFromInput(input);

    expect(createPool).toHaveBeenCalledWith(null, []);
    expect(showStatus).toHaveBeenCalledWith("已新建轮换池", "success");
  });

  it("创建失败时展示错误提示", async () => {
    vi.mocked(createPool).mockRejectedValue(new Error("create failed"));
    const input = document.createElement("input");
    input.value = "动漫";

    await createPoolFromInput(input);

    expect(showStatus).toHaveBeenCalledWith("创建轮换池失败", "error");
    expect(log.error).toHaveBeenCalled();
  });
});

describe("setupPoolCreate", () => {
  it("绑定按钮 click 与输入框 Enter，创建成功", async () => {
    document.body.innerHTML = `
      <input id="pool-name-input" value="动漫" />
      <button id="pool-create-btn">+</button>
    `;

    setupPoolCreate();
    const btn = document.getElementById("pool-create-btn")!;
    btn.click();
    await vi.waitFor(() => {
      expect(createPool).toHaveBeenCalledWith("动漫", []);
    });

    const input = document.getElementById("pool-name-input") as HTMLInputElement;
    input.value = "风景";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await vi.waitFor(() => {
      expect(createPool).toHaveBeenCalledWith("风景", []);
    });
  });

  it("元素缺失时安全返回", () => {
    document.body.innerHTML = "";
    expect(() => setupPoolCreate()).not.toThrow();
  });
});

// ── 10.5 setupNextWallpaperButton：手动“下一张” ───────────────────────────────

describe("setupNextWallpaperButton", () => {
  it("点击按钮调用 next_wallpaper（缺省主屏单元）", async () => {
    document.body.innerHTML = '<button id="next-wallpaper-btn">下一张</button>';
    const btn = document.getElementById("next-wallpaper-btn") as HTMLButtonElement;

    setupNextWallpaperButton();
    btn.click();
    await vi.waitFor(() => {
      expect(nextWallpaper).toHaveBeenCalledTimes(1);
    });
    expect(nextWallpaper).toHaveBeenCalledWith();
  });

  it("按钮缺失时安全返回", () => {
    document.body.innerHTML = "";
    expect(() => setupNextWallpaperButton()).not.toThrow();
  });

  it("next_wallpaper 失败时展示错误提示", async () => {
    vi.mocked(nextWallpaper).mockRejectedValue(new Error("ipc failure"));
    document.body.innerHTML = '<button id="next-wallpaper-btn">下一张</button>';
    const btn = document.getElementById("next-wallpaper-btn") as HTMLButtonElement;

    setupNextWallpaperButton();
    btn.click();
    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("手动切换下一张失败", "error");
    });
  });
});