import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import ./arrangement 之前声明（vitest 会自动提升到文件顶部）
// 屏蔽 ipc，使布局域模块内 IPC 调用完全可控
vi.mock("../ipc", () => ({
  getArrangement: vi.fn(),
  updateArrangement: vi.fn(),
}));

// 屏蔽 logger，保持测试输出整洁
vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

import { getArrangement } from "../ipc";
import type { DisplayInfo } from "../types";
import {
  getCurrentArrangement,
  isArrangement,
  syncArrangementSelect,
  unitKeysForArrangement,
} from "./arrangement";

function sampleDisplays(): DisplayInfo[] {
  return [
    { id: "d1", name: "显示器 1", width: 1920, height: 1080, x: 0, y: 0, is_primary: true, dpi: 1, current_wallpaper: null },
    { id: "d2", name: "显示器 2", width: 1920, height: 1080, x: 0, y: 0, is_primary: false, dpi: 1, current_wallpaper: null },
  ];
}

beforeEach(() => {
  vi.clearAllMocks();
  document.body.innerHTML = "";
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("isArrangement", () => {
  it("接受三个合法多屏排列值", () => {
    expect(isArrangement("per_monitor")).toBe(true);
    expect(isArrangement("all_same")).toBe(true);
    expect(isArrangement("span")).toBe(true);
  });

  it("拒绝未知值与非字符串", () => {
    expect(isArrangement("random")).toBe(false);
    expect(isArrangement("")).toBe(false);
    expect(isArrangement("PerMonitor")).toBe(false);
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

describe("getCurrentArrangement", () => {
  it("成功读取后端多屏排列", async () => {
    vi.mocked(getArrangement).mockResolvedValue("span");
    await expect(getCurrentArrangement()).resolves.toBe("span");
  });

  it("读取失败时回退默认 per_monitor", async () => {
    vi.mocked(getArrangement).mockRejectedValue(new Error("ipc failure"));
    await expect(getCurrentArrangement()).resolves.toBe("per_monitor");
  });
});

describe("syncArrangementSelect", () => {
  it("同步多屏排列选择器显示值", () => {
    document.body.innerHTML = `
      <select id="arrangement-select">
        <option value="per_monitor">每屏独立</option>
        <option value="all_same">每屏同图</option>
        <option value="span">跨屏合并</option>
      </select>`;
    syncArrangementSelect("all_same");
    const select = document.getElementById("arrangement-select") as HTMLSelectElement;
    expect(select.value).toBe("all_same");
  });

  it("控件不存在时静默跳过", () => {
    expect(() => syncArrangementSelect("per_monitor")).not.toThrow();
  });
});
