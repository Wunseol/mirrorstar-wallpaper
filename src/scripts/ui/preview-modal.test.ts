import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import 被测模块之前声明（vitest 会自动提升到文件顶部）
vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: vi.fn((path: string, protocol: string = "asset") => `${protocol}://${path}`),
}));

vi.mock("../ipc", () => ({
  setWallpaper: vi.fn(),
}));

vi.mock("../state", () => ({
  appState: {
    _selectedDisplayId: "",
    allWallpapers: [] as unknown[],
    currentPreviewId: null as string | null,
    get selectedDisplayId() { return this._selectedDisplayId; },
    set selectedDisplayId(v: string) { this._selectedDisplayId = v; },
  },
}));

vi.mock("../utils/listeners", () => ({
  addEventListenerWithCleanup: vi.fn(
    (target: EventTarget, type: string, listener: EventListenerOrEventListenerObject) => {
      target.addEventListener(type, listener);
    },
  ),
  registerCleanup: vi.fn(),
}));

vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

vi.mock("./utils", () => ({
  extractFileName: vi.fn((path: string) => path.split(/[/\\]/).pop() || path),
  showStatus: vi.fn(),
  // F-006: 转发到真实 DOM 方法，保留 video 资源释放语义
  releaseVideoElement: vi.fn((video: HTMLVideoElement) => {
    video.pause();
    video.removeAttribute("src");
    video.load();
  }),
}));

import { setWallpaper } from "../ipc";
import { appState } from "../state";
import { addEventListenerWithCleanup } from "../utils/listeners";
import { showStatus } from "./utils";
import { openPreview, closePreview, setupPreviewModal } from "./preview-modal";
import type { WallpaperEntry } from "../types";

const sampleImageWp: WallpaperEntry = {
  id: "w-1",
  file_path: "C:/wallpapers/sample.png",
  wallpaper_type: "Image",
  display_id: null,
  added_at: "2026-01-01T00:00:00Z",
  thumbnail: "",
  file_size: 1024,
  metadata: null,
};

const sampleVideoWp: WallpaperEntry = {
  id: "w-2",
  file_path: "C:/wallpapers/sample.mp4",
  wallpaper_type: "Video",
  display_id: null,
  added_at: "2026-01-01T00:00:00Z",
  thumbnail: "",
  file_size: 2048,
  metadata: null,
};

function setupModalDOM() {
  document.body.innerHTML = `
    <div id="preview-modal">
      <div class="preview-overlay"></div>
      <button id="preview-close">×</button>
      <img id="preview-image" style="display:none" />
      <video id="preview-video" style="display:none"></video>
      <iframe id="preview-web" style="display:none"></iframe>
      <div id="preview-name"></div>
      <div id="preview-type"></div>
      <button id="preview-set-btn">设为壁纸</button>
    </div>
    <div id="wallpaper-grid"></div>
  `;
}

describe("openPreview / closePreview", () => {
  beforeEach(() => {
    setupModalDOM();
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("openPreview 显示 modal 并设置 currentPreviewId", () => {
    openPreview(sampleImageWp);

    const modal = document.getElementById("preview-modal")!;
    expect(modal.classList.contains("active")).toBe(true);
    expect(appState.currentPreviewId).toBe("w-1");
  });

  it("openPreview Image 类型显示 img 元素并设置 alt", () => {
    openPreview(sampleImageWp);

    const img = document.getElementById("preview-image") as HTMLImageElement;
    expect(img.style.display).not.toBe("none");
    // 使用 getAttribute 避免 jsdom URL 规范化（wpfile://C:/x → wpfile://C/x）
    expect(img.getAttribute("src")).toContain("wpfile://");
  });

  it("openPreview Video 类型显示 video 元素", () => {
    openPreview(sampleVideoWp);

    const video = document.getElementById("preview-video") as HTMLVideoElement;
    expect(video.style.display).not.toBe("none");
  });

  it("closePreview 移除 active 类并清空 currentPreviewId", () => {
    openPreview(sampleImageWp);
    closePreview();

    const modal = document.getElementById("preview-modal")!;
    expect(modal.classList.contains("active")).toBe(false);
    expect(appState.currentPreviewId).toBeNull();
  });

  it("closePreview 暂停并清空 video src（resetMedia）", () => {
    const video = document.getElementById("preview-video") as HTMLVideoElement;
    video.pause = vi.fn();
    video.load = vi.fn();
    openPreview(sampleVideoWp);

    closePreview();

    expect(video.pause).toHaveBeenCalled();
    expect(video.getAttribute("src")).toBeNull();
  });

  it("openPreview 缺少必要 DOM 元素时安全返回（不抛错）", () => {
    document.body.innerHTML = "";
    expect(() => openPreview(sampleImageWp)).not.toThrow();
  });
});

describe("setupPreviewModal", () => {
  beforeEach(() => {
    setupModalDOM();
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("FE-003: closeBtn 使用 addEventListenerWithCleanup 注册", () => {
    setupPreviewModal();

    const closeBtn = document.getElementById("preview-close")!;
    expect(addEventListenerWithCleanup).toHaveBeenCalledWith(closeBtn, "click", expect.any(Function));
  });

  it("FE-003: overlay 使用 addEventListenerWithCleanup 注册", () => {
    setupPreviewModal();

    const overlay = document.querySelector(".preview-overlay")!;
    expect(addEventListenerWithCleanup).toHaveBeenCalledWith(overlay, "click", expect.any(Function));
  });

  it("FE-003: setBtn 使用 addEventListenerWithCleanup 注册", () => {
    setupPreviewModal();

    const setBtn = document.getElementById("preview-set-btn")!;
    expect(addEventListenerWithCleanup).toHaveBeenCalledWith(setBtn, "click", expect.any(Function));
  });

  it("点击 closeBtn 触发 closePreview", () => {
    setupPreviewModal();
    openPreview(sampleImageWp);

    const closeBtn = document.getElementById("preview-close")!;
    closeBtn.click();

    const modal = document.getElementById("preview-modal")!;
    expect(modal.classList.contains("active")).toBe(false);
  });

  it("modal 不存在时安全返回", () => {
    document.body.innerHTML = "";
    expect(() => setupPreviewModal()).not.toThrow();
  });
});

describe("FE-007: preview-set-btn click 使用安全的选择器匹配", () => {
  beforeEach(() => {
    setupModalDOM();
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("FE-007: 设为壁纸后通过 data-id 属性匹配激活卡片（不使用模板字符串选择器）", async () => {
    vi.mocked(setWallpaper).mockResolvedValue(undefined);
    setupPreviewModal();
    appState.currentPreviewId = "w-1";

    // 在 grid 中放一张卡片
    const grid = document.getElementById("wallpaper-grid")!;
    const card = document.createElement("div");
    card.className = "wallpaper-card";
    card.dataset.id = "w-1";
    grid.appendChild(card);

    const setBtn = document.getElementById("preview-set-btn")!;
    setBtn.click();

    await vi.waitFor(() => {
      expect(card.classList.contains("active")).toBe(true);
    });
    expect(showStatus).toHaveBeenCalledWith("壁纸已设置", "success");
  });

  it("setWallpaper 失败时显示错误状态", async () => {
    vi.mocked(setWallpaper).mockRejectedValue(new Error("ipc failure"));
    setupPreviewModal();
    appState.currentPreviewId = "w-1";

    const setBtn = document.getElementById("preview-set-btn")!;
    setBtn.click();

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("设置壁纸失败", "error");
    });
  });

  it("currentPreviewId 为 null 时不调用 setWallpaper", async () => {
    setupPreviewModal();
    appState.currentPreviewId = null;

    const setBtn = document.getElementById("preview-set-btn")!;
    setBtn.click();

    // 等待一个 microtask 确保异步回调已执行
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(setWallpaper).not.toHaveBeenCalled();
  });
});

// ── v41-F-003: openPreview iframe 安全属性加固 ───────────────────────────────

describe("v41-F-003 openPreview iframe 安全属性加固", () => {
  const sampleWebWp: WallpaperEntry = {
    id: "w-web",
    file_path: "C:/wallpapers/sample.html",
    wallpaper_type: "Web",
    display_id: null,
    added_at: "2026-01-01T00:00:00Z",
    thumbnail: "",
    file_size: 512,
    metadata: null,
  };

  beforeEach(() => {
    setupModalDOM();
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("v41_f003_iframe_has_security_attributes", () => {
    openPreview(sampleWebWp);

    const iframe = document.getElementById("preview-web") as HTMLIFrameElement;
    expect(iframe).not.toBeNull();
    // v41-F-003: 断言 iframe 设置了 referrer-policy / sandbox / allow 安全属性
    expect(iframe.getAttribute("referrer-policy")).toBe("no-referrer");
    expect(iframe.getAttribute("sandbox")).toBe("allow-scripts allow-same-origin");
    expect(iframe.getAttribute("allow")).toBe("geolocation=(), microphone=(), camera=()");
  });
});
