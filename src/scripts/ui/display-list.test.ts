import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import 被测模块之前声明（vitest 会自动提升到文件顶部）
vi.mock("../ipc", () => ({
  getDisplays: vi.fn(),
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

vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

// F08: mock ./utils 以便断言 showStatus 调用
vi.mock("./utils", () => ({
  showStatus: vi.fn(),
}));

import { getDisplays } from "../ipc";
import { appState } from "../state";
import { showStatus } from "./utils";
import { populateDisplaySelect } from "./display-list";
import type { DisplayInfo } from "../types";

function makeDisplay(overrides: Partial<DisplayInfo> = {}): DisplayInfo {
  return {
    id: overrides.id ?? "DISPLAY1",
    name: overrides.name ?? "主显示器",
    width: overrides.width ?? 1920,
    height: overrides.height ?? 1080,
    x: 0,
    y: 0,
    is_primary: overrides.is_primary ?? false,
    dpi: 96,
    current_wallpaper: null,
    ...overrides,
  };
}

describe("populateDisplaySelect", () => {
  let select: HTMLSelectElement;

  beforeEach(() => {
    document.body.innerHTML = '<select id="display-select"></select>';
    select = document.getElementById("display-select") as HTMLSelectElement;
    // 重置 appState 状态
    appState.selectedDisplayId = "";
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("为每个显示器创建一个 option", async () => {
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({ id: "D1", name: "显示器1" }),
      makeDisplay({ id: "D2", name: "显示器2" }),
    ]);

    await populateDisplaySelect(select);

    const options = select.querySelectorAll("option");
    expect(options).toHaveLength(2);
    expect(options[0]?.value).toBe("D1");
    expect(options[1]?.value).toBe("D2");
  });

  it("option 文本格式为 [主] 前缀 + 名称 + 分辨率", async () => {
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({
        id: "D1",
        name: "主显示器",
        width: 2560,
        height: 1440,
        is_primary: true,
      }),
      makeDisplay({
        id: "D2",
        name: "副显示器",
        width: 1920,
        height: 1080,
        is_primary: false,
      }),
    ]);

    await populateDisplaySelect(select);

    const options = select.querySelectorAll("option");
    expect(options[0]?.textContent).toBe("[主] 主显示器 (2560x1440)");
    expect(options[1]?.textContent).toBe("副显示器 (1920x1080)");
  });

  it("主显示器时同步 selectedDisplayId 与 select.value", async () => {
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({ id: "PRIMARY", is_primary: true }),
      makeDisplay({ id: "SECONDARY", is_primary: false }),
    ]);

    await populateDisplaySelect(select);

    expect(appState.selectedDisplayId).toBe("PRIMARY");
    expect(select.value).toBe("PRIMARY");
  });

  it("无主显示器时 selectedDisplayId 保持默认空串", async () => {
    // 边缘情况：所有显示器 is_primary=false
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({ id: "D1", is_primary: false }),
      makeDisplay({ id: "D2", is_primary: false }),
    ]);

    await populateDisplaySelect(select);

    // FE-001 关联：无主显示器分支，selectedDisplayId 保持 ""
    expect(appState.selectedDisplayId).toBe("");
    // select.value 默认为第一个 option（浏览器行为），但 appState 不会因此被设置
    expect(select.value).toBe("D1");
  });

  it("getDisplays 失败时不抛错，记录错误日志", async () => {
    const error = new Error("ipc failure");
    vi.mocked(getDisplays).mockRejectedValue(error);

    // 不应抛错（内部 try/catch）
    await expect(populateDisplaySelect(select)).resolves.toBeUndefined();

    // 应记录错误日志
    const { log } = await import("../utils/logger");
    expect(log.error).toHaveBeenCalledWith("获取显示器列表失败:", error);

    // 不应添加任何 option
    expect(select.querySelectorAll("option")).toHaveLength(0);
  });

  it("空显示器列表时不添加 option，selectedDisplayId 保持空串", async () => {
    vi.mocked(getDisplays).mockResolvedValue([]);

    await populateDisplaySelect(select);

    expect(select.querySelectorAll("option")).toHaveLength(0);
    expect(appState.selectedDisplayId).toBe("");
  });

  it("多个主显示器时仅最后一个被同步到 selectedDisplayId", async () => {
    // 边缘情况：违反 Win32 文档（至多一个主显示器），但代码逻辑上多个 is_primary=true 时
    // 会依次覆盖 appState.selectedDisplayId。验证此行为以锁定当前实现语义。
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({ id: "P1", is_primary: true }),
      makeDisplay({ id: "P2", is_primary: true }),
    ]);

    await populateDisplaySelect(select);

    // 最后一个主显示器覆盖前一个
    expect(appState.selectedDisplayId).toBe("P2");
  });
});

// ── F08: 空显示器列表容错 ─────────────────────────────────────────────────────

describe("F08: 空显示器列表容错", () => {
  let select: HTMLSelectElement;

  beforeEach(() => {
    document.body.innerHTML = `
      <select id="display-select"></select>
      <button id="pause-btn">暂停</button>
      <button id="resume-btn">恢复</button>
    `;
    select = document.getElementById("display-select") as HTMLSelectElement;
    appState.selectedDisplayId = "";
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("空列表时调用 showStatus('未检测到显示器', 'error')", async () => {
    vi.mocked(getDisplays).mockResolvedValue([]);

    await populateDisplaySelect(select);

    expect(showStatus).toHaveBeenCalledWith("未检测到显示器", "error");
  });

  it("空列表时禁用 display-select 与 pause/resume 按钮", async () => {
    vi.mocked(getDisplays).mockResolvedValue([]);

    await populateDisplaySelect(select);

    expect(select.disabled).toBe(true);
    const pauseBtn = document.getElementById("pause-btn") as HTMLButtonElement;
    const resumeBtn = document.getElementById("resume-btn") as HTMLButtonElement;
    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(true);
  });

  it("空列表时记录 warn 日志便于排查", async () => {
    vi.mocked(getDisplays).mockResolvedValue([]);

    await populateDisplaySelect(select);

    const { log } = await import("../utils/logger");
    expect(log.warn).toHaveBeenCalledWith("未检测到显示器（getDisplays 返回空数组）");
  });

  it("空列表时仅调用一次 showStatus（错误提示）", async () => {
    vi.mocked(getDisplays).mockResolvedValue([]);

    await populateDisplaySelect(select);

    expect(showStatus).toHaveBeenCalledTimes(1);
  });

  it("getDisplays 抛错时同样禁用 display-select 与 pause/resume 按钮", async () => {
    vi.mocked(getDisplays).mockRejectedValue(new Error("ipc failure"));

    await populateDisplaySelect(select);

    expect(select.disabled).toBe(true);
    const pauseBtn = document.getElementById("pause-btn") as HTMLButtonElement;
    const resumeBtn = document.getElementById("resume-btn") as HTMLButtonElement;
    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(true);
  });

  it("正常返回显示器时不调用 showStatus，控件保持可用", async () => {
    vi.mocked(getDisplays).mockResolvedValue([
      makeDisplay({ id: "D1", is_primary: true }),
    ]);

    await populateDisplaySelect(select);

    expect(showStatus).not.toHaveBeenCalled();
    expect(select.disabled).toBe(false);
    const pauseBtn = document.getElementById("pause-btn") as HTMLButtonElement;
    const resumeBtn = document.getElementById("resume-btn") as HTMLButtonElement;
    expect(pauseBtn.disabled).toBe(false);
    expect(resumeBtn.disabled).toBe(false);
  });
});
