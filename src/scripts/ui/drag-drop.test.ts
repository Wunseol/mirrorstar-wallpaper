import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import 被测模块之前声明（vitest 会自动提升到文件顶部）
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: vi.fn(() => ({
    onDragDropEvent: vi.fn(() => Promise.resolve(() => {})),
  })),
}));

vi.mock("../ipc", () => ({
  addWallpaper: vi.fn(),
  setWallpaper: vi.fn(),
  openFileDialog: vi.fn(),
  getErrorMessage: vi.fn((e: unknown) => {
    if (typeof e === "object" && e !== null && "message" in e) {
      return String((e as { message: unknown }).message);
    }
    return String(e);
  }),
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
  getFileExtension: vi.fn((path: string) => {
    const parts = path.split(/[/\\]/);
    const fileName = parts[parts.length - 1] ?? "";
    const dotIndex = fileName.lastIndexOf(".");
    return dotIndex >= 0 ? fileName.substring(dotIndex + 1).toLowerCase() : "";
  }),
  IMAGE_EXTENSIONS: new Set(["jpg", "jpeg", "png", "bmp", "webp"]),
  // FE-012: VIDEO/GIF/HTML_EXTENSIONS 已改为 utils.ts 模块私有，mock 不再提供
  isSupportedFile: vi.fn((path: string) => {
    const parts = path.split(/[/\\]/);
    const fileName = parts[parts.length - 1] ?? "";
    const dotIndex = fileName.lastIndexOf(".");
    const ext = dotIndex >= 0 ? fileName.substring(dotIndex + 1).toLowerCase() : "";
    return ["jpg", "jpeg", "png", "bmp", "webp", "mp4", "avi", "mkv", "webm", "mov", "gif", "html", "htm"].includes(ext);
  }),
  showStatus: vi.fn(),
}));

import { addWallpaper, setWallpaper, openFileDialog } from "../ipc";
import { addEventListenerWithCleanup } from "../utils/listeners";
import { showStatus, isSupportedFile } from "./utils";
import { setupAddButton, setupDragAndDrop } from "./drag-drop";
import { getCurrentWebview } from "@tauri-apps/api/webview";

describe("setupAddButton", () => {
  beforeEach(() => {
    document.body.innerHTML = '<button id="add-wallpaper-btn">添加</button>';
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("FE-003: 使用 addEventListenerWithCleanup 注册 click 监听器", () => {
    setupAddButton();

    const btn = document.getElementById("add-wallpaper-btn")!;
    expect(addEventListenerWithCleanup).toHaveBeenCalledWith(btn, "click", expect.any(Function));
  });

  it("btn 不存在时安全返回", () => {
    document.body.innerHTML = "";
    expect(() => setupAddButton()).not.toThrow();
    expect(addEventListenerWithCleanup).not.toHaveBeenCalled();
  });

  it("点击按钮 → 选择文件 → addWallpaper 成功 → 显示添加成功（不自动设为壁纸）", async () => {
    vi.mocked(openFileDialog).mockResolvedValue("C:/pics/img.png");
    vi.mocked(addWallpaper).mockResolvedValue("w-1");
    vi.mocked(setWallpaper).mockResolvedValue(undefined);

    setupAddButton();
    const btn = document.getElementById("add-wallpaper-btn")!;
    btn.click();

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("壁纸添加成功", "success");
    });
    expect(addWallpaper).toHaveBeenCalledWith("C:/pics/img.png");
    expect(setWallpaper).not.toHaveBeenCalled();
  });

  it("用户取消对话框时不显示状态", async () => {
    vi.mocked(openFileDialog).mockResolvedValue(null);

    setupAddButton();
    const btn = document.getElementById("add-wallpaper-btn")!;
    btn.click();

    await new Promise(resolve => setTimeout(resolve, 0));
    expect(addWallpaper).not.toHaveBeenCalled();
    expect(showStatus).not.toHaveBeenCalled();
  });

  it("两阶段错误：addWallpaper 失败 → 显示添加失败", async () => {
    vi.mocked(openFileDialog).mockResolvedValue("C:/pics/img.png");
    vi.mocked(addWallpaper).mockRejectedValue(new Error("add failed"));

    setupAddButton();
    const btn = document.getElementById("add-wallpaper-btn")!;
    btn.click();

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("添加壁纸失败: add failed", "error");
    });
    expect(setWallpaper).not.toHaveBeenCalled();
  });

  it("添加成功不触发 setWallpaper（即使 setWallpaper mock 会失败）", async () => {
    vi.mocked(openFileDialog).mockResolvedValue("C:/pics/img.png");
    vi.mocked(addWallpaper).mockResolvedValue("w-1");
    vi.mocked(setWallpaper).mockRejectedValue(new Error("set failed"));

    setupAddButton();
    const btn = document.getElementById("add-wallpaper-btn")!;
    btn.click();

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("壁纸添加成功", "success");
    });
    expect(setWallpaper).not.toHaveBeenCalled();
  });

  it("非图片文件：addWallpaper 成功后不调用 setWallpaper，直接显示添加成功", async () => {
    vi.mocked(openFileDialog).mockResolvedValue("C:/videos/clip.mp4");
    vi.mocked(addWallpaper).mockResolvedValue("w-2");

    setupAddButton();
    const btn = document.getElementById("add-wallpaper-btn")!;
    btn.click();

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("壁纸添加成功", "success");
    });
    expect(setWallpaper).not.toHaveBeenCalled();
  });
});

describe("setupDragAndDrop", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="wallpaper-grid"></div>';
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("grid 不存在时安全返回", async () => {
    document.body.innerHTML = "";
    await expect(setupDragAndDrop()).resolves.toBeUndefined();
  });

  it("成功注册拖放事件并登记清理", async () => {
    await setupDragAndDrop();
    // registerCleanup 在 onDragDropEvent 成功后被调用
    const { registerCleanup } = await import("../utils/listeners");
    expect(registerCleanup).toHaveBeenCalled();
  });
});

describe("isSupportedFile 过滤逻辑（通过 mock 验证调用）", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("isSupportedFile 对图片文件返回 true", () => {
    expect(isSupportedFile("C:/pics/photo.jpg")).toBe(true);
  });

  it("isSupportedFile 对不支持的文件返回 false", () => {
    expect(isSupportedFile("C:/docs/readme.txt")).toBe(false);
  });
});

// ── v41-F-004: 多文件拖放进度显示 ─────────────────────────────────────────────

describe("v41-F-004 多文件拖放进度显示", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="wallpaper-grid"></div>';
    vi.clearAllMocks();
    // 默认 addWallpaper 与 setWallpaper 成功
    vi.mocked(addWallpaper).mockResolvedValue("w-1");
    vi.mocked(setWallpaper).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  /** 从 mock 中提取 onDragDropEvent 注册的回调 */
  function getDropCallback(): (event: { payload: { type: string; paths: string[] } }) => Promise<void> {
    const webviewMock = vi.mocked(getCurrentWebview);
    const onDragDropEventMock = webviewMock.mock.results[0]!.value.onDragDropEvent;
    return onDragDropEventMock.mock.calls[0]![0];
  }

  it("v41_f004_multi_file_drop_shows_total_progress", async () => {
    await setupDragAndDrop();
    const dropCallback = getDropCallback();

    // 触发 3 个文件的 drop 事件
    await dropCallback({
      payload: {
        type: "drop",
        paths: ["C:/file1.png", "C:/file2.png", "C:/file3.png"],
      },
    });

    // 验证进度消息按 0/3, 1/3, 2/3 顺序显示
    expect(showStatus).toHaveBeenCalledWith("添加进度: 0 / 3", "info");
    expect(showStatus).toHaveBeenCalledWith("添加进度: 1 / 3", "info");
    expect(showStatus).toHaveBeenCalledWith("添加进度: 2 / 3", "info");
  });
});

// ── v41-F-011: 多文件拖放串行化（无竞态） ─────────────────────────────────────

describe("v41-F-011 多文件拖放串行化", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="wallpaper-grid"></div>';
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  /** 从 mock 中提取 onDragDropEvent 注册的回调 */
  function getDropCallback(): (event: { payload: { type: string; paths: string[] } }) => Promise<void> {
    const webviewMock = vi.mocked(getCurrentWebview);
    const onDragDropEventMock = webviewMock.mock.results[0]!.value.onDragDropEvent;
    return onDragDropEventMock.mock.calls[0]![0];
  }

  it("v41_f011_multi_file_add_no_race_condition", async () => {
    // 记录 addWallpaper 的调用顺序
    const callOrder: string[] = [];
    vi.mocked(addWallpaper).mockImplementation((filePath: string) => {
      callOrder.push(filePath);
      return Promise.resolve(`id-${filePath}`);
    });
    vi.mocked(setWallpaper).mockResolvedValue(undefined);

    await setupDragAndDrop();
    const dropCallback = getDropCallback();

    const files = ["C:/file1.png", "C:/file2.png", "C:/file3.png"];
    await dropCallback({
      payload: {
        type: "drop",
        paths: files,
      },
    });

    // 验证 addWallpaper 按文件顺序串行调用，无竞态导致的顺序错乱
    expect(callOrder).toEqual(files);
  });
});
