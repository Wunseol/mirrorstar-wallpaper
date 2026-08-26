import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import 被测模块之前声明（vitest 会自动提升到文件顶部）
vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: vi.fn((path: string, protocol: string = "asset") => `${protocol}://${path}`),
}));

vi.mock("../ipc", () => ({
  getWallpapers: vi.fn(),
  removeWallpaper: vi.fn(),
}));

vi.mock("../state", () => ({
  appState: {
    _selectedDisplayId: "",
    allWallpapers: [] as unknown[],
    currentPreviewId: null,
    get selectedDisplayId() { return this._selectedDisplayId; },
    set selectedDisplayId(v: string) { this._selectedDisplayId = v; },
  },
}));

vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

// extractFileName / typeIcon 用真实实现，便于断言 aria-label 与图标渲染
vi.mock("./utils", () => ({
  extractFileName: vi.fn((path: string) => path.split(/[/\\]/).pop() || path),
  showStatus: vi.fn(),
  typeIcon: vi.fn((type: string) => {
    switch (type) {
      case "Image": return "🖼";
      case "Video": return "🎬";
      case "Gif":   return "🎞";
      case "Web":   return "🌐";
      default:      return "📄";
    }
  }),
  // F-006: 转发到真实 DOM 方法，spy 仍可断言 pause/removeAttribute/load 调用
  releaseVideoElement: vi.fn((video: HTMLVideoElement) => {
    video.pause();
    video.removeAttribute("src");
    video.load();
  }),
}));

vi.mock("./preview-modal", () => ({
  openPreview: vi.fn(),
}));

import type { WallpaperEntry } from "../types";
// v41-F-008: removeWallpaper 用于断言删除回调
import { getWallpapers, removeWallpaper } from "../ipc";
import { appState } from "../state";
import { typeIcon, showStatus } from "./utils";
import { openPreview } from "./preview-modal";
import {
  appendWallpaperCard,
  refreshWallpaperList,
  renderWallpaperList,
  removeWallpaperCard,
  updateWallpaperCard,
  getLowercasedFileName,
  __getCacheSizeForTests,
} from "./wallpaper-list";

// 测试用壁纸：Image 类型且缩略图为空，触发源图直载分支
const sampleWp: WallpaperEntry = {
  id: "w-1",
  file_path: "C:/wallpapers/sample.png",
  wallpaper_type: "Image",
  display_id: null,
  added_at: "2026-01-01T00:00:00Z",
  thumbnail: "",
  file_size: 1024,
  metadata: null,
};

// ── F05: IntersectionObserver mock ───────────────────────────────────────────
// jsdom 不提供原生 IntersectionObserver，测试中需手动安装并触发回调。
// 默认不自动触发——测试需显式调用 trigger() 模拟进入视口，以验证懒渲染行为。

class MockIntersectionObserver {
  static instances: MockIntersectionObserver[] = [];
  static last: MockIntersectionObserver | null = null;

  readonly callback: IntersectionObserverCallback;
  readonly options: IntersectionObserverInit | undefined;
  observed: Element[] = [];
  unobserved: Element[] = [];
  disconnectCount = 0;

  constructor(cb: IntersectionObserverCallback, options?: IntersectionObserverInit) {
    this.callback = cb;
    this.options = options;
    MockIntersectionObserver.instances.push(this);
    MockIntersectionObserver.last = this;
  }
  observe(target: Element): void { this.observed.push(target); }
  unobserve(target: Element): void { this.unobserved.push(target); }
  disconnect(): void { this.disconnectCount++; this.observed = []; }
  takeRecords(): IntersectionObserverEntry[] { return []; }
  readonly root: Element | Document | null = null;
  readonly rootMargin: string = "";
  readonly thresholds: ReadonlyArray<number> = [];

  /** 测试辅助：模拟元素进入/离开视口，触发 callback */
  trigger(isIntersecting: boolean, targets?: Element[]): void {
    const els = targets ?? this.observed;
    const entries = els.map((target) => ({
      target,
      isIntersecting,
      intersectionRatio: isIntersecting ? 1 : 0,
      boundingClientRect: target.getBoundingClientRect(),
      intersectionRect: target.getBoundingClientRect(),
      rootBounds: null,
      time: 0,
    })) as unknown as IntersectionObserverEntry[];
    this.callback(entries, this);
  }
}

// 顶层 beforeEach：每个测试前重置 mock 实例并安装 IntersectionObserver
beforeEach(() => {
  MockIntersectionObserver.instances = [];
  MockIntersectionObserver.last = null;
  globalThis.IntersectionObserver = MockIntersectionObserver as unknown as typeof IntersectionObserver;
});

afterEach(() => {
  // 恢复：删除 mock，避免影响其他测试文件（vitest 文件级隔离，但保持整洁）
  delete (globalThis as { IntersectionObserver?: typeof IntersectionObserver }).IntersectionObserver;
});

// ── F-004 壁纸卡片 a11y ───────────────────────────────────────────────────────

describe("F-004 壁纸卡片 a11y", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("渲染卡片时设置 tabindex=0、role=button、aria-label（含文件名）", () => {
    renderWallpaperList([sampleWp]);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    expect(card.tabIndex).toBe(0);
    expect(card.getAttribute("tabindex")).toBe("0");
    expect(card.getAttribute("role")).toBe("button");
    // aria-label 包含文件名与中文前缀
    expect(card.getAttribute("aria-label")).toBe("预览壁纸 sample.png");
  });

  it("Enter 键触发 openPreview 并阻止默认行为", () => {
    renderWallpaperList([sampleWp]);
    // F05: 触发懒渲染，绑定 keydown 监听器
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    const event = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    card.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(sampleWp);
  });

  it("Space 键触发 openPreview 并阻止默认滚动", () => {
    renderWallpaperList([sampleWp]);
    // F05: 触发懒渲染，绑定 keydown 监听器
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    const event = new KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true });
    card.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(sampleWp);
  });

  it("鼠标点击仍触发 openPreview（保留原有行为）", () => {
    renderWallpaperList([sampleWp]);
    // F05: 触发懒渲染，绑定 click 监听器
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    card.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));

    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(sampleWp);
  });

  it("非 Enter/Space 的按键不触发 openPreview", () => {
    renderWallpaperList([sampleWp]);
    // F05: 触发懒渲染，绑定 keydown 监听器
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    card.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }),
    );

    expect(openPreview).not.toHaveBeenCalled();
  });

  it("v5.0 F-PERF-001: 事件委托——进入视口前点击/Enter 即触发预览（监听器在 grid 上）", () => {
    // v5.0 F-PERF-001: 事件委托后，监听器注册在 grid 容器上（renderWallpaperList 时即注册），
    // 卡片无需 hydrate 即可响应 click/keydown。懒渲染仅延迟缩略图（img/video.src）填充。
    renderWallpaperList([sampleWp]);
    // 不触发 IntersectionObserver trigger——卡片尚未 hydrate（无缩略图），但事件委托已生效

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    card.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(sampleWp);

    card.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
    );
    expect(openPreview).toHaveBeenCalledTimes(2);
  });
});

// ── F-005 移除主动缩略图生成 ─────────────────────────────────────────────────

describe("F-005 移除主动缩略图生成", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("refreshWallpaperList 渲染单张壁纸卡片并走源图直载分支（thumbnail 为空）", async () => {
    vi.mocked(getWallpapers).mockResolvedValue([sampleWp]);

    await refreshWallpaperList();

    // 列表正常渲染：grid 内有一张壁纸卡片
    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    // F05: 触发懒渲染，填充缩略图内容
    MockIntersectionObserver.last!.trigger(true);
    // thumbnail 为空且 Image 类型：走源图直载分支（创建 <img> 加载源文件），不调用 typeIcon
    expect(typeIcon).not.toHaveBeenCalled();
    const thumb = card.querySelector(".wallpaper-card-thumb");
    expect(thumb).not.toBeNull();
    const img = thumb?.querySelector("img.wallpaper-thumb");
    expect(img).not.toBeNull();
    // appState.allWallpapers 已写入
    expect(appState.allWallpapers).toHaveLength(1);
    expect(appState.allWallpapers[0]).toBe(sampleWp);
    // v12.0: refreshWallpaperList 应预填充缓存（clear 后 prefetch 1 条）
    expect(__getCacheSizeForTests()).toBe(1);
  });
});

// ── FE-008: refreshWallpaperList 失败时清空骨架屏 ──────────────────────────────

describe("FE-008 refreshWallpaperList 失败时清空骨架屏", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("getWallpapers 失败时清空骨架屏并显示错误提示", async () => {
    vi.mocked(getWallpapers).mockRejectedValue(new Error("ipc failure"));

    await refreshWallpaperList();

    // 骨架屏应已被清空（不再有 skeleton-card）
    const skeletons = grid.querySelectorAll(".skeleton-card");
    expect(skeletons).toHaveLength(0);
    // 应显示错误提示
    const hint = grid.querySelector(".empty-hint");
    expect(hint).not.toBeNull();
    expect(hint?.textContent).toContain("加载壁纸列表失败");
  });
});

// ── F05: IntersectionObserver 懒渲染 ─────────────────────────────────────────

describe("F05 IntersectionObserver 懒渲染", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("renderWallpaperList 创建 IntersectionObserver 并观察每张卡片", () => {
    const wp1 = { ...sampleWp, id: "w-1" };
    const wp2 = { ...sampleWp, id: "w-2" };
    renderWallpaperList([wp1, wp2]);

    // 创建了一个 observer
    expect(MockIntersectionObserver.instances).toHaveLength(1);
    const observer = MockIntersectionObserver.last!;
    expect(observer).toBeDefined();
    // 观察了两张卡片
    expect(observer.observed).toHaveLength(2);
    // observer options 中 rootMargin 为 200px（提前触发）
    expect(observer.options?.rootMargin).toBe("200px");
  });

  it("进入视口前卡片仅渲染外壳，不设置 img.src（懒渲染）", () => {
    const wp = { ...sampleWp, thumbnail: "C:/thumb.png" };
    renderWallpaperList([wp]);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    // 外壳属性已设置
    expect(card.dataset.id).toBe("w-1");
    expect(card.getAttribute("role")).toBe("button");
    // 缩略图容器存在但为空（未填充 img）
    const thumb = card.querySelector(".wallpaper-card-thumb");
    expect(thumb?.children).toHaveLength(0);
  });

  it("进入视口后卡片填充缩略图（设置 img.src）", () => {
    const wp = { ...sampleWp, id: "w-thumb", thumbnail: "C:/thumb.png" };
    renderWallpaperList([wp]);

    const observer = MockIntersectionObserver.last!;
    observer.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    const img = card.querySelector("img.wallpaper-thumb") as HTMLImageElement;
    expect(img).not.toBeNull();
    // src 经 convertFileSrc 转换（用 getAttribute 避免 URL 规范化）
    // 前端使用 wpfile:// 协议，mock 按 protocol 参数生成 URL
    expect(img.getAttribute("src")).toBe("wpfile://C:/thumb.png");
    // alt 取自 file_path 的文件名（sampleWp.file_path = C:/wallpapers/sample.png）
    expect(img.alt).toBe("sample.png");
    // hydrated 标记已设置
    expect(card.dataset.hydrated).toBe("1");
  });

  it("进入视口后卡片绑定 click 事件", () => {
    renderWallpaperList([sampleWp]);
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    card.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(sampleWp);
  });

  it("进入视口后 observer unobserve 该卡片（不重复触发）", () => {
    renderWallpaperList([sampleWp]);

    const observer = MockIntersectionObserver.last!;
    observer.trigger(true);

    // unobserve 被调用一次（卡片进入视口后停止观察）
    expect(observer.unobserved).toHaveLength(1);
    expect(observer.unobserved[0]).toBe(grid.querySelector(".wallpaper-card"));
  });

  it("重复触发 intersect 不重复 hydrate（dataset.hydrated 标记）", () => {
    renderWallpaperList([sampleWp]);

    const observer = MockIntersectionObserver.last!;
    // 模拟 observer 保留引用并再次触发（即使已 unobserve，防御性测试）
    const card = grid.querySelector(".wallpaper-card") as HTMLElement;
    observer.trigger(true, [card]);
    observer.trigger(true, [card]);

    // openPreview 仅在 click 时触发，此处验证 hydrate 不重复：
    // 缩略图容器内 img 子元素仅一个（重复 hydrate 会追加多个）
    const thumb = card.querySelector(".wallpaper-card-thumb");
    expect(thumb?.querySelectorAll("img.wallpaper-thumb")).toHaveLength(1);
  });

  it("重新渲染时调用旧 observer.disconnect() 防内存泄漏", () => {
    renderWallpaperList([sampleWp]);
    const firstObserver = MockIntersectionObserver.last!;
    expect(firstObserver.disconnectCount).toBe(0);

    // 再次渲染——应 disconnect 旧 observer
    renderWallpaperList([sampleWp]);
    expect(firstObserver.disconnectCount).toBe(1);
    // 新 observer 已创建
    expect(MockIntersectionObserver.instances).toHaveLength(2);
    expect(MockIntersectionObserver.last).toBe(MockIntersectionObserver.instances[1]);
  });

  it("空列表渲染时不创建 observer（无需懒渲染）", () => {
    appState.allWallpapers = [];
    renderWallpaperList([]);
    // 没有 observer 被创建
    expect(MockIntersectionObserver.instances).toHaveLength(0);
  });
});

// ── F12: 请求序号取消机制 ─────────────────────────────────────────────────────

describe("F12 请求序号取消机制", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("连续两次刷新时，第一次（慢）的过期响应被丢弃，UI 显示第二次结果", async () => {
    const firstWallpapers: WallpaperEntry[] = [
      { ...sampleWp, id: "w-first", file_path: "C:/first.png" },
    ];
    const secondWallpapers: WallpaperEntry[] = [
      { ...sampleWp, id: "w-second", file_path: "C:/second.png" },
    ];

    let resolveFirst: (val: WallpaperEntry[]) => void = () => {};
    let resolveSecond: (val: WallpaperEntry[]) => void = () => {};

    vi.mocked(getWallpapers)
      .mockReturnValueOnce(new Promise((r) => { resolveFirst = r; }))
      .mockReturnValueOnce(new Promise((r) => { resolveSecond = r; }));

    // 发起两次刷新：第一次慢（未 resolve），第二次快
    const p1 = refreshWallpaperList();
    const p2 = refreshWallpaperList();

    // 第二次先 resolve
    resolveSecond(secondWallpapers);
    await p2;
    // 第一次后 resolve（此时已过期）
    resolveFirst(firstWallpapers);
    await p1;

    // UI 应显示第二次的结果（卡片 id 为 w-second）
    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    expect(card.dataset.id).toBe("w-second");
    // appState.allWallpapers 是第二次的结果（第一次的过期响应未覆盖）
    expect(appState.allWallpapers).toEqual(secondWallpapers);
    expect(appState.allWallpapers[0]?.id).toBe("w-second");
  });

  it("过期响应的 catch 分支也被丢弃，不覆盖新响应的成功结果", async () => {
    const successWallpapers: WallpaperEntry[] = [
      { ...sampleWp, id: "w-success", file_path: "C:/success.png" },
    ];

    let rejectFirst: (err: Error) => void = () => {};
    let resolveSecond: (val: WallpaperEntry[]) => void = () => {};

    vi.mocked(getWallpapers)
      .mockReturnValueOnce(new Promise((_, rej) => { rejectFirst = rej; }))
      .mockReturnValueOnce(new Promise((r) => { resolveSecond = r; }));

    const p1 = refreshWallpaperList();
    const p2 = refreshWallpaperList();

    // 第二次成功 resolve
    resolveSecond(successWallpapers);
    await p2;
    // 第一次后 reject（过期错误不应覆盖第二次的成功结果）
    rejectFirst(new Error("ipc failure"));
    await p1;

    // UI 仍显示第二次的成功结果
    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    expect(card.dataset.id).toBe("w-success");
    expect(appState.allWallpapers).toEqual(successWallpapers);
    // showStatus 不应被过期的错误调用
    expect(showStatus).not.toHaveBeenCalled();
  });

  it("单次刷新正常完成时序号匹配，渲染正常", async () => {
    vi.mocked(getWallpapers).mockResolvedValue([sampleWp]);

    await refreshWallpaperList();

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    expect(card).not.toBeNull();
    expect(card.dataset.id).toBe("w-1");
    expect(appState.allWallpapers).toEqual([sampleWp]);
  });
});

// ── F-006: 重渲染前释放已加载 <video> 的解码资源 ──────────────────────────────

describe("F-006: renderWallpaperList releases video resources", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("should call pause(), removeAttribute('src'), load() on each video before clearing", () => {
    // 在 grid 内放入两个已加载的 video 元素，模拟重渲染前的旧状态
    grid.innerHTML = `
      <video class="wallpaper-thumb" src="file://video1.mp4"></video>
      <video class="wallpaper-thumb" src="file://video2.mp4"></video>
    `;

    const videos = grid.querySelectorAll<HTMLVideoElement>("video.wallpaper-thumb");
    expect(videos.length).toBe(2);

    // 为每个 video 的方法挂上 spy
    const spies = Array.from(videos).map((video) => ({
      pause: vi.spyOn(video, "pause"),
      removeAttribute: vi.spyOn(video, "removeAttribute"),
      load: vi.spyOn(video, "load"),
    }));

    // 触发重渲染：传入空数组命中 wallpapers.length === 0 早返回分支（在 video 清理之后）
    renderWallpaperList([]);

    // 验证每个 video 都被 pause / removeAttribute('src') / load 调用了一次
    for (const spy of spies) {
      expect(spy.pause).toHaveBeenCalledTimes(1);
      expect(spy.removeAttribute).toHaveBeenCalledWith("src");
      expect(spy.load).toHaveBeenCalledTimes(1);
    }

    // 验证 grid 已被清空（video 元素被移除）
    expect(grid.querySelectorAll("video.wallpaper-thumb").length).toBe(0);
  });
});

// ── v41-F-006: renderWallpaperList 使用 replaceChildren 替代 innerHTML ────────

describe("v41-F-006 renderWallpaperList 使用 replaceChildren", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("v41_f006_render_uses_replace_children", () => {
    // 在 grid 中放入一些旧子节点，模拟重渲染前的状态
    const oldChild1 = document.createElement("div");
    const oldChild2 = document.createElement("span");
    grid.appendChild(oldChild1);
    grid.appendChild(oldChild2);

    // spy grid.replaceChildren
    const replaceChildrenSpy = vi.spyOn(grid, "replaceChildren");

    // 渲染非空列表，触发清空操作
    renderWallpaperList([{ ...sampleWp, id: "w-replace" }]);

    // v41-F-006: 断言 replaceChildren 被调用以清空旧子节点
    expect(replaceChildrenSpy).toHaveBeenCalled();
    // 旧子节点已被移除
    expect(grid.contains(oldChild1)).toBe(false);
    expect(grid.contains(oldChild2)).toBe(false);
    // 新卡片已渲染
    expect(grid.querySelector(".wallpaper-card")).not.toBeNull();
  });
});

// ── v41-F-009: 空列表渲染无内联样式（CSP 合规） ──────────────────────────────

describe("v41-F-009 空列表渲染无内联样式", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("v41_f009_empty_state_no_inline_style", () => {
    // allWallpapers 为空，渲染"拖拽图片到此处添加壁纸"的空状态
    renderWallpaperList([]);

    const emptyContainer = grid.querySelector(".wallpaper-list-empty");
    expect(emptyContainer).not.toBeNull();

    // 收集所有子元素并断言无 style 属性
    const allElements = [emptyContainer!, ...Array.from(emptyContainer!.querySelectorAll("*"))];
    for (const el of allElements) {
      expect(el.getAttribute("style")).toBeNull();
    }

    // 验证 hint 文本使用 .wallpaper-list-empty-hint 类（已在 main.css 中定义）
    const hint = emptyContainer!.querySelector(".wallpaper-list-empty-hint");
    expect(hint).not.toBeNull();
    expect(hint?.textContent).toContain("JPG");
  });
});

// ── v41-F-008: 卡片 Delete 键重复触发删除修复 ─────────────────────────────────

describe("v41-F-008 卡片 Delete 键重复触发删除", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    vi.clearAllMocks();
    // 默认 confirm 接受删除
    vi.spyOn(window, "confirm").mockReturnValue(true);
    // v5.0 F-PERF-001: performDeleteById 从 appState.allWallpapers 查找壁纸
    // （生产中 refreshWallpaperList 在 renderWallpaperList 前已设置 appState）
    appState.allWallpapers = [sampleWp];
  });

  afterEach(() => {
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  it("v41_f008_delete_key_on_deletebtn_does_not_double_trigger", async () => {
    // mock removeWallpaper 成功
    vi.mocked(removeWallpaper).mockResolvedValue(undefined);
    renderWallpaperList([sampleWp]);
    // 触发懒渲染，绑定 keydown 监听器
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    const deleteBtn = card.querySelector(".wallpaper-card-delete") as HTMLButtonElement;
    expect(deleteBtn).not.toBeNull();

    // 模拟焦点在 deleteBtn 时按 Delete 键
    deleteBtn.focus();
    const event = new KeyboardEvent("keydown", {
      key: "Delete",
      bubbles: true,
      cancelable: true,
    });
    Object.defineProperty(event, "target", { value: deleteBtn, configurable: true });
    deleteBtn.dispatchEvent(event);

    // 等待异步 performDelete 完成
    await vi.waitFor(() => {
      expect(removeWallpaper).toHaveBeenCalledTimes(1);
    });
    // v41-F-008: 断言删除回调只调用一次，未重复触发
    expect(removeWallpaper).toHaveBeenCalledTimes(1);
    expect(removeWallpaper).toHaveBeenCalledWith(sampleWp.id, true);
  });

  it("v41-F-008 卡片聚焦（非 deleteBtn）按 Delete 键触发一次删除", async () => {
    vi.mocked(removeWallpaper).mockResolvedValue(undefined);
    renderWallpaperList([sampleWp]);
    MockIntersectionObserver.last!.trigger(true);

    const card = grid.querySelector(".wallpaper-card") as HTMLDivElement;
    card.focus();
    const event = new KeyboardEvent("keydown", {
      key: "Delete",
      bubbles: true,
      cancelable: true,
    });
    Object.defineProperty(event, "target", { value: card, configurable: true });
    card.dispatchEvent(event);

    await vi.waitFor(() => {
      expect(removeWallpaper).toHaveBeenCalledTimes(1);
    });
  });
});

// ── v12.0: 缓存清理（Wave v12-A / v12-B） ─────────────────────────────────────

describe("v12.0 缓存清理", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
    // jsdom 不提供 CSS 全局对象，removeWallpaperCard 使用 CSS.escape 构造选择器
    if (typeof globalThis.CSS === "undefined") {
      (globalThis as { CSS: { escape: (s: string) => string } }).CSS = {
        escape: (s: string) => s.replace(/[^a-zA-Z0-9_-]/g, "\\$&"),
      };
    }
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("test_removeWallpaperCard_clears_cache_entry", async () => {
    // 用 refreshWallpaperList 设置已知缓存状态（v12-B 会先 clear 再 prefetch）
    const wp = { ...sampleWp, id: "w-remove", file_path: "C:/wallpapers/remove.png" };
    vi.mocked(getWallpapers).mockResolvedValue([wp]);
    await refreshWallpaperList();
    expect(__getCacheSizeForTests()).toBe(1);

    // 移除卡片（v12-A：应清理对应缓存条目）
    removeWallpaperCard(wp.id);

    // v12.0: removeWallpaperCard 应删除对应 file_path 的缓存条目
    expect(__getCacheSizeForTests()).toBe(0);
    // appState 同步移除
    expect(appState.allWallpapers).toHaveLength(0);
  });

  it("test_removeWallpaperCard_does_not_clear_other_entries", async () => {
    // 验证仅删除目标条目，不影响其他条目
    const wp1 = { ...sampleWp, id: "w-keep", file_path: "C:/wallpapers/keep.png" };
    const wp2 = { ...sampleWp, id: "w-remove", file_path: "C:/wallpapers/remove.png" };
    vi.mocked(getWallpapers).mockResolvedValue([wp1, wp2]);
    await refreshWallpaperList();
    expect(__getCacheSizeForTests()).toBe(2);

    removeWallpaperCard(wp2.id);

    // 仅删除 wp2 的条目，wp1 保留
    expect(__getCacheSizeForTests()).toBe(1);
  });

  it("test_refresh_clears_stale_cache_entries", async () => {
    // 场景：file_path 重命名后全量刷新，旧 file_path 不应残留
    // 第一次刷新：填充缓存 with old path
    const wpOld = { ...sampleWp, id: "w-rename", file_path: "C:/wallpapers/old.jpg" };
    vi.mocked(getWallpapers).mockResolvedValue([wpOld]);
    await refreshWallpaperList();
    expect(__getCacheSizeForTests()).toBe(1);

    // 第二次刷新：file_path 重命名为 new path（同 id 不同 file_path）
    const wpNew = { ...sampleWp, id: "w-rename", file_path: "C:/wallpapers/new.jpg" };
    vi.mocked(getWallpapers).mockResolvedValue([wpNew]);
    await refreshWallpaperList();

    // v12.0: clear() 后再 prefetch，旧 file_path 不残留
    // 若未 clear，缓存会有 2 个条目（old + new）；clear 后仅 1 个（new）
    expect(__getCacheSizeForTests()).toBe(1);

    // 进一步验证旧路径已不在缓存中：调用 getLowercasedFileName(old) 应触发缓存未命中
    // （缓存大小从 1 增至 2，证明 old 之前不在缓存中）
    getLowercasedFileName(wpOld.file_path);
    expect(__getCacheSizeForTests()).toBe(2);
  });

  it("test_refresh_clears_cache_on_empty_list", async () => {
    // 场景：从有壁纸变为无壁纸，缓存应被清空
    const wp = { ...sampleWp, id: "w-clear", file_path: "C:/wallpapers/clear.png" };
    vi.mocked(getWallpapers).mockResolvedValue([wp]);
    await refreshWallpaperList();
    expect(__getCacheSizeForTests()).toBe(1);

    // 刷新为空列表
    vi.mocked(getWallpapers).mockResolvedValue([]);
    await refreshWallpaperList();

    // v12.0: clear() 后 prefetch 空列表，缓存应为空
    expect(__getCacheSizeForTests()).toBe(0);
  });
});

// ── 竞态修复：wallpaper-updated 早于 append 到达时不丢弃缩略图 ──────────────
// 首次拖入壁纸时后端先后 emit：wallpaper-added（thumbnail 为空，进入 pendingAdds
// 延迟 50ms 后 append）→ wallpaper-updated（后台缩略图生成完成，带 thumbnail）。
// 竞态：若 wallpaper-updated 在 append 之前到达，此时条目尚未入库且无 DOM 卡片，
// updateWallpaperCard 原 findIndex==-1 直接 return 会丢弃缩略图；append 随后用
// 过期空缩略图渲染。修复后 updateWallpaperCard 改为 upsert、append 复用最新条目。

describe("竞态修复: updated 早于 append 时保留缩略图", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("wallpaper-updated(带缩略图) 早于 append 到达时，最终渲染使用带缩略图条目且不产生重复卡片", () => {
    // 先渲染一次空列表：注册 grid 事件委托监听器（appendWallpaperCard 本身不注册委托）
    renderWallpaperList([]);

    // wallpaper-updated 的 payload：缩略图已生成，同 id 为 w-1
    const updatedWithThumb: WallpaperEntry = {
      ...sampleWp,
      id: "w-1",
      thumbnail: "C:/thumb.png",
    };
    // wallpaper-added 的 payload：缩略图为空（同 id）
    const addedEmptyThumb: WallpaperEntry = { ...sampleWp, id: "w-1", thumbnail: "" };

    // 竞态时序：先收到 wallpaper-updated（此时条目尚未入库/未 append）
    updateWallpaperCard(updatedWithThumb);
    // 再收到 wallpaper-added 并（50ms 后）append
    appendWallpaperCard(addedEmptyThumb);

    // 1. appState 仅有该条目一份，且保留带缩略图的数据（而非被过期空缩略图覆盖）
    expect(appState.allWallpapers).toHaveLength(1);
    expect(appState.allWallpapers[0]).toBe(updatedWithThumb);

    // 2. DOM 仅一张卡片，无重复
    const cards = grid.querySelectorAll(".wallpaper-card");
    expect(cards).toHaveLength(1);
    const card = cards[0] as HTMLDivElement;
    expect(card.dataset.id).toBe("w-1");

    // 3. 渲染/点击使用带缩略图的最新条目（cardData 来自 append 复用的 effective）
    card.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    expect(openPreview).toHaveBeenCalledTimes(1);
    expect(openPreview).toHaveBeenCalledWith(updatedWithThumb);
    // 明确不使用 wallpaper-added 的空缩略图 payload
    expect(openPreview).not.toHaveBeenCalledWith(addedEmptyThumb);
  });

  it("batch 场景：多个 add + 一个 updated 组合不产生重复条目", () => {
    renderWallpaperList([]);

    const updated2: WallpaperEntry = { ...sampleWp, id: "w-2", thumbnail: "C:/t2.png" };
    const w3: WallpaperEntry = { ...sampleWp, id: "w-3", file_path: "C:/wallpapers/w3.png" };

    // 混合同一 id 的 updated 与 add，以及独立 add
    updateWallpaperCard(updated2);
    appendWallpaperCard({ ...sampleWp, id: "w-2", thumbnail: "" });
    appendWallpaperCard(w3);

    // 共 2 个不同 id，无重复
    expect(appState.allWallpapers).toHaveLength(2);
    expect(grid.querySelectorAll(".wallpaper-card")).toHaveLength(2);
    // w-2 保留带缩略图的数据
    const w2 = appState.allWallpapers.find(w => w.id === "w-2");
    expect(w2?.thumbnail).toBe("C:/t2.png");
  });
});

// ── 空库首拖修复：renderWallpaperList([]) 后首次拖入卡片仍被 observe 并 hydrate ──
// 根因：renderWallpaperList 在 wallpapers.length===0 时提前 return，不创建模块级
// lazyObserver；首次拖入（空库）时 appendWallpaperCard 调用 lazyObserver?.observe(card)
// 因 lazyObserver 为 null 而 no-op，卡片永不 hydrate（缩略图空白）。
// 修复：renderWallpaperList 提前 return 前置 lazyObserver=null，appendWallpaperCard
// 在 observer 不存在时补建，保证首次拖入的卡片也能被懒渲染填充缩略图。

describe("空库首拖: 首次拖入卡片被 observe 且 hydrate 渲染缩略图", () => {
  let grid: HTMLDivElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    grid = document.createElement("div");
    grid.id = "wallpaper-grid";
    document.body.appendChild(grid);
    appState.allWallpapers = [];
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("空库首拖：renderWallpaperList([]) 后 appendWallpaperCard 的卡片被 observe 且 hydrate 渲染缩略图", () => {
    // 1. 模拟空库启动：renderWallpaperList([]) 提前 return，并（因修复）置 lazyObserver=null
    renderWallpaperList([]);

    // 2. 构造带缩略图的 Image 壁纸并调用 appendWallpaperCard（模拟首次拖入）
    const wpWithThumb: WallpaperEntry = {
      ...sampleWp,
      id: "w-first-drag",
      file_path: "C:/wallpapers/first-drag.png",
      thumbnail: "C:/thumb.png",
    };
    appendWallpaperCard(wpWithThumb);

    // 3. append 补建了 observer 并 observe 该卡片（lazyObserver 非 null）
    expect(MockIntersectionObserver.instances.length).toBeGreaterThanOrEqual(1);
    const observer = MockIntersectionObserver.last!;
    const card = grid.querySelector(".wallpaper-card") as HTMLElement;
    expect(card).not.toBeNull();
    expect(observer.observed).toContain(card);

    // 4. 触发 hydrate 后，卡片缩略图容器内渲染 <img class="wallpaper-thumb"> 且 src 非空
    observer.trigger(true);
    const thumb = card.querySelector(".wallpaper-card-thumb");
    expect(thumb).not.toBeNull();
    const img = thumb?.querySelector("img.wallpaper-thumb") as HTMLImageElement;
    expect(img).not.toBeNull();
    // src 经 convertFileSrc 转换（wpfile:// 协议），断言包含 thumb 即可
    expect(img.getAttribute("src")).toContain("thumb");

    // 5. appState 仅含 1 条且 thumbnail 为 "C:/thumb.png"
    expect(appState.allWallpapers).toHaveLength(1);
    const stored = appState.allWallpapers[0] as WallpaperEntry;
    expect(stored.thumbnail).toBe("C:/thumb.png");
  });
});
