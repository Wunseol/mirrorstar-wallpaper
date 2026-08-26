import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock 须在 import ./main 之前声明（vitest 会自动提升到文件顶部）
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// 屏蔽 init 中的 getVersion 调用，避免加载真实 Tauri 应用 API
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn(),
}));

// 屏蔽 init 依赖的 UI 模块，避免加载真实 Tauri webview/convertFileSrc 等副作用
// FE-004: debounce mock 需透传函数，使音量/速度滑块的防抖回调在测试中可被立即触发
vi.mock("./ui/mod", () => ({
  // v5.0 A-PERF-001: 增量事件处理函数（mock 为空函数，事件监听器被 listenWithCleanup mock 拦截不会实际调用）
  appendWallpaperCard: vi.fn(),
  removeWallpaperCard: vi.fn(),
  updateWallpaperCard: vi.fn(),
  debounce: vi.fn((fn: (...args: unknown[]) => void) => fn),
  extractFileName: vi.fn(),
  loadConfig: vi.fn(),
  patchConfig: vi.fn(),
  populateDisplaySelect: vi.fn(),
  refreshWallpaperList: vi.fn(),
  renderWallpaperList: vi.fn(),
  setupAddButton: vi.fn(),
  setupDragAndDrop: vi.fn(),
  setupPreviewModal: vi.fn(),
  showStatus: vi.fn(),
}));

// 屏蔽 init 中的事件监听注册（避免调用真实 @tauri-apps/api/event）
vi.mock("./utils/listeners", () => ({
  listenWithCleanup: vi.fn(),
  cleanupAllListeners: vi.fn(),
}));

// 屏蔽 logger，保持测试输出整洁，并允许断言错误日志
vi.mock("./utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { log } from "./utils/logger";
import { showStatus, loadConfig, patchConfig } from "./ui/mod";
import { updatePlaybackButtons, init } from "./main";

describe("updatePlaybackButtons", () => {
  let pauseBtn: HTMLButtonElement;
  let resumeBtn: HTMLButtonElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    pauseBtn = document.createElement("button");
    pauseBtn.id = "pause-btn";
    resumeBtn = document.createElement("button");
    resumeBtn.id = "resume-btn";
    document.body.appendChild(pauseBtn);
    document.body.appendChild(resumeBtn);
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("Playing 状态：暂停按钮可点击，恢复按钮禁用", async () => {
    vi.mocked(invoke).mockResolvedValue("Playing");

    await updatePlaybackButtons("display-1");

    expect(pauseBtn.disabled).toBe(false);
    expect(resumeBtn.disabled).toBe(true);
  });

  it("Paused 状态：暂停按钮禁用，恢复按钮可点击", async () => {
    vi.mocked(invoke).mockResolvedValue("Paused");

    await updatePlaybackButtons("display-1");

    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(false);
  });

  it("null 状态：两个按钮均保持可点击", async () => {
    vi.mocked(invoke).mockResolvedValue(null);

    await updatePlaybackButtons("display-1");

    expect(pauseBtn.disabled).toBe(false);
    expect(resumeBtn.disabled).toBe(false);
  });

  it("Terminated 状态（全屏终止临时状态）：两个按钮禁用且展示 恢复中...", async () => {
    vi.mocked(invoke).mockResolvedValue("Terminated");

    await updatePlaybackButtons("display-1");

    // 后端退出全屏后会自动重启恢复为 Playing，期间禁止用户误点暂停/恢复
    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(true);
    expect(resumeBtn.textContent).toBe("恢复中...");
  });

  it("Terminated 状态结束后（恢复为 Playing）按钮恢复默认标签", async () => {
    vi.mocked(invoke).mockResolvedValue("Terminated");
    await updatePlaybackButtons("display-1");
    expect(resumeBtn.textContent).toBe("恢复中...");

    // 模拟后端自动重启完成，状态回到 Playing
    vi.mocked(invoke).mockResolvedValue("Playing");
    await updatePlaybackButtons("display-1");

    expect(pauseBtn.textContent).toBe("暂停");
    expect(resumeBtn.textContent).toBe("恢复");
    expect(pauseBtn.disabled).toBe(false);
    expect(resumeBtn.disabled).toBe(true);
  });

  it("invoke 抛错时按钮状态保持不变且记录告警日志", async () => {
    pauseBtn.disabled = false;
    resumeBtn.disabled = false;
    vi.mocked(invoke).mockRejectedValue(new Error("ipc failure"));

    await updatePlaybackButtons("display-1");

    expect(pauseBtn.disabled).toBe(false);
    expect(resumeBtn.disabled).toBe(false);
    expect(log.warn).toHaveBeenCalledWith("查询壁纸状态失败:", expect.any(Error));
  });

  it("缺少按钮元素时直接返回且不调用 invoke", async () => {
    document.body.innerHTML = "";
    vi.mocked(invoke).mockResolvedValue("Playing");

    await updatePlaybackButtons("display-1");

    expect(invoke).not.toHaveBeenCalled();
  });

  it("调用 invoke 时传入正确的命令名与 displayId", async () => {
    vi.mocked(invoke).mockResolvedValue("Playing");

    await updatePlaybackButtons("display-9");

    expect(invoke).toHaveBeenCalledWith("get_wallpaper_state", { displayId: "display-9" });
  });
});

// ── C-040 / C-041 错误捕获测试 ───────────────────────────────────────────────

describe("volume slider 错误处理 (C-040)", () => {
  let volumeSlider: HTMLInputElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    volumeSlider = document.createElement("input");
    volumeSlider.id = "volume-slider";
    volumeSlider.value = "50";
    document.body.appendChild(volumeSlider);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号默认 resolve（invoke 返回 undefined 即可）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("setVolume 抛错时 showStatus 被以 error 调用", async () => {
    // 仅 set_volume 命令失败，其余 IPC 命令正常 resolve
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "set_volume") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(undefined);
    });

    await init();

    // 触发 input 事件，进入异步回调
    volumeSlider.dispatchEvent(new Event("input"));

    // 等待异步 catch 完成
    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("音量设置失败，请重试", "error");
    });
    expect(log.error).toHaveBeenCalledWith("音量设置失败:", expect.any(Error));
  });
});

describe("auto-start checkbox 错误处理 (C-040)", () => {
  let autoStartCheckbox: HTMLInputElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    autoStartCheckbox = document.createElement("input");
    autoStartCheckbox.type = "checkbox";
    autoStartCheckbox.id = "auto-start";
    autoStartCheckbox.checked = false;
    document.body.appendChild(autoStartCheckbox);
    vi.clearAllMocks();
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("toggleAutoStart 抛错时 checkbox 回滚到旧值", async () => {
    // get_auto_start_status 返回 false，toggle_auto_start 失败
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "toggle_auto_start") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(false);
    });

    await init();

    // 模拟用户勾选：checked 已变为 true，触发 change
    autoStartCheckbox.checked = true;
    autoStartCheckbox.dispatchEvent(new Event("change"));

    await vi.waitFor(() => {
      // 回滚到旧值 false
      expect(autoStartCheckbox.checked).toBe(false);
    });
    expect(showStatus).toHaveBeenCalledWith("开机自启设置失败，请重试", "error");
    expect(log.error).toHaveBeenCalledWith("开机自启设置失败:", expect.any(Error));
  });
});

describe("init 顶层错误处理 (C-041)", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    vi.clearAllMocks();
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("init() 抛错时顶层 catch 触发 showStatus", async () => {
    // 让 loadConfig 抛错，init 在第一个 await 处 reject
    vi.mocked(loadConfig).mockRejectedValue(new Error("init failure"));

    // 触发 DOMContentLoaded，进入 init().catch(...) 链
    document.dispatchEvent(new Event("DOMContentLoaded"));

    await vi.waitFor(() => {
      expect(showStatus).toHaveBeenCalledWith("初始化失败，请重启应用", "error");
    });
    expect(log.error).toHaveBeenCalledWith("应用初始化失败:", expect.any(Error));
  });
});

// ── F-003 IPC 失败 UI 回滚测试 ───────────────────────────────────────────────

describe("fullscreen-action-select 下拉框回滚 (F-003)", () => {
  let fullscreenActionSelect: HTMLSelectElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    fullscreenActionSelect = document.createElement("select");
    fullscreenActionSelect.id = "fullscreen-action-select";
    // 构造三个选项，初始选中 terminate（默认值）
    const optTerminate = document.createElement("option");
    optTerminate.value = "terminate";
    optTerminate.textContent = "terminate";
    const optPause = document.createElement("option");
    optPause.value = "pause";
    optPause.textContent = "pause";
    const optNone = document.createElement("option");
    optNone.value = "none";
    optNone.textContent = "none";
    fullscreenActionSelect.appendChild(optTerminate);
    fullscreenActionSelect.appendChild(optPause);
    fullscreenActionSelect.appendChild(optNone);
    fullscreenActionSelect.value = "terminate";
    document.body.appendChild(fullscreenActionSelect);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号/loadConfig 默认 resolve（clearAllMocks 不重置实现，需显式复位）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("patchConfig 抛错时 select 回滚到旧值并提示错误", async () => {
    // patchConfig 抛错，模拟 IPC 写配置失败
    vi.mocked(patchConfig).mockRejectedValue(new Error("ipc failure"));

    await init();

    // 模拟用户切换：value 变为 pause，触发 change
    fullscreenActionSelect.value = "pause";
    fullscreenActionSelect.dispatchEvent(new Event("change"));

    // 等待异步 catch 完成，断言回滚到旧值 terminate
    await vi.waitFor(() => {
      expect(fullscreenActionSelect.value).toBe("terminate");
    });
    expect(showStatus).toHaveBeenCalledWith("更新配置失败", "error");
    expect(log.error).toHaveBeenCalledWith("更新配置失败:", expect.any(Error));
  });
});

describe("pause-on-battery checkbox 回滚 (F-003)", () => {
  let pauseOnBatteryCheckbox: HTMLInputElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    pauseOnBatteryCheckbox = document.createElement("input");
    pauseOnBatteryCheckbox.type = "checkbox";
    pauseOnBatteryCheckbox.id = "pause-on-battery";
    pauseOnBatteryCheckbox.checked = false;
    document.body.appendChild(pauseOnBatteryCheckbox);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号/loadConfig 默认 resolve（clearAllMocks 不重置实现，需显式复位）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("patchConfig 抛错时 checkbox 回滚到旧值并提示错误", async () => {
    vi.mocked(patchConfig).mockRejectedValue(new Error("ipc failure"));

    await init();

    pauseOnBatteryCheckbox.checked = true;
    pauseOnBatteryCheckbox.dispatchEvent(new Event("change"));

    await vi.waitFor(() => {
      expect(pauseOnBatteryCheckbox.checked).toBe(false);
    });
    expect(showStatus).toHaveBeenCalledWith("更新配置失败", "error");
    expect(log.error).toHaveBeenCalledWith("更新配置失败:", expect.any(Error));
  });
});

describe("interaction-mode checkbox 回滚 (F-003)", () => {
  let interactionMode: HTMLInputElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    interactionMode = document.createElement("input");
    interactionMode.type = "checkbox";
    interactionMode.id = "interaction-mode";
    interactionMode.checked = false;
    document.body.appendChild(interactionMode);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号/loadConfig 默认 resolve（clearAllMocks 不重置实现，需显式复位）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("setInteractionMode 抛错时 checkbox 回滚到旧值", async () => {
    // set_interaction_mode 命令失败，其余 IPC 命令正常 resolve
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "set_interaction_mode") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(false);
    });

    await init();

    // 模拟用户勾选：checked 已变为 true，触发 change
    interactionMode.checked = true;
    interactionMode.dispatchEvent(new Event("change"));

    await vi.waitFor(() => {
      // 回滚到旧值 false
      expect(interactionMode.checked).toBe(false);
    });
    expect(showStatus).toHaveBeenCalledWith("切换模式失败，请重试", "error");
  });
});

describe("arrangement-select 回滚 (F-003)", () => {
  let arrangementSelect: HTMLSelectElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    arrangementSelect = document.createElement("select");
    arrangementSelect.id = "arrangement-select";
    // 构造两个选项，初始选中 per_monitor（合法 Arrangement 值）
    const optPerMonitor = document.createElement("option");
    optPerMonitor.value = "per_monitor";
    optPerMonitor.textContent = "per_monitor";
    const optSpan = document.createElement("option");
    optSpan.value = "span";
    optSpan.textContent = "span";
    arrangementSelect.appendChild(optPerMonitor);
    arrangementSelect.appendChild(optSpan);
    arrangementSelect.value = "per_monitor";
    document.body.appendChild(arrangementSelect);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号/loadConfig 默认 resolve（clearAllMocks 不重置实现，需显式复位）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("patchConfig 抛错时 select 回滚到旧值并提示错误", async () => {
    vi.mocked(patchConfig).mockRejectedValue(new Error("ipc failure"));

    await init();

    // 模拟用户切换：value 变为 span，触发 change
    arrangementSelect.value = "span";
    arrangementSelect.dispatchEvent(new Event("change"));

    // 等待异步 catch 完成，断言回滚到旧值 per_monitor
    await vi.waitFor(() => {
      expect(arrangementSelect.value).toBe("per_monitor");
    });
    expect(showStatus).toHaveBeenCalledWith("更新排列模式失败，请重试", "error");
    expect(log.error).toHaveBeenCalledWith("更新排列模式失败:", expect.any(Error));
  });
});

describe("scaling-mode-select 回滚 (F-003)", () => {
  let scalingSelect: HTMLSelectElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    scalingSelect = document.createElement("select");
    scalingSelect.id = "scaling-mode-select";
    const optFit = document.createElement("option");
    optFit.value = "fit";
    optFit.textContent = "fit";
    const optFill = document.createElement("option");
    optFill.value = "fill";
    optFill.textContent = "fill";
    scalingSelect.appendChild(optFit);
    scalingSelect.appendChild(optFill);
    scalingSelect.value = "fit";
    document.body.appendChild(scalingSelect);
    vi.clearAllMocks();
    // init 依赖的 IPC/版本号/loadConfig 默认 resolve（clearAllMocks 不重置实现，需显式复位）
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("setScalingMode 抛错时 select 回滚到旧值并提示错误", async () => {
    // set_scaling_mode 命令失败，其余 IPC 命令正常 resolve
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "set_scaling_mode") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(undefined);
    });

    await init();

    // 模拟用户切换：value 变为 fill，触发 change
    scalingSelect.value = "fill";
    scalingSelect.dispatchEvent(new Event("change"));

    await vi.waitFor(() => {
      // 回滚到旧值 fit
      expect(scalingSelect.value).toBe("fit");
    });
    expect(showStatus).toHaveBeenCalledWith("更新缩放模式失败，请重试", "error");
    expect(log.error).toHaveBeenCalledWith("更新缩放模式失败:", expect.any(Error));
  });
});

// ── F-001 / F-002 updatePlaybackButtons 序号取消与空 displayId 守卫 ──────────

describe("F-001: updatePlaybackButtons sequence cancellation", () => {
  let pauseBtn: HTMLButtonElement;
  let resumeBtn: HTMLButtonElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    pauseBtn = document.createElement("button");
    pauseBtn.id = "pause-btn";
    resumeBtn = document.createElement("button");
    resumeBtn.id = "resume-btn";
    document.body.appendChild(pauseBtn);
    document.body.appendChild(resumeBtn);
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("应丢弃过期响应——慢的第一次调用结果不应覆盖快的第二次调用结果", async () => {
    // 第一次调用：延迟 100ms 返回 "Playing"（会成为过期响应）
    // 第二次调用：立即返回 "Paused"（应胜出）
    let callCount = 0;
    vi.mocked(invoke).mockImplementation(() => {
      callCount++;
      if (callCount === 1) {
        return new Promise(resolve => setTimeout(() => resolve("Playing"), 100));
      }
      return Promise.resolve("Paused");
    });

    // 并发发起两次调用：p1 慢、p2 快
    const p1 = updatePlaybackButtons("A");
    const p2 = updatePlaybackButtons("B");
    await Promise.all([p1, p2]);

    // 最终状态应反映第二次调用（Paused），而非第一次（Playing）：
    // - pauseBtn.disabled = true（Paused 状态下不能暂停）
    // - resumeBtn.disabled = false（Paused 状态下可以恢复）
    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(false);
  });
});

describe("F-002: updatePlaybackButtons empty displayId guard", () => {
  let pauseBtn: HTMLButtonElement;
  let resumeBtn: HTMLButtonElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    pauseBtn = document.createElement("button");
    pauseBtn.id = "pause-btn";
    // 模拟 populateDisplaySelect 的 disablePlaybackControls 初始禁用状态
    pauseBtn.disabled = true;
    resumeBtn = document.createElement("button");
    resumeBtn.id = "resume-btn";
    resumeBtn.disabled = true;
    document.body.appendChild(pauseBtn);
    document.body.appendChild(resumeBtn);
    vi.clearAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("空 displayId 时不应调用 invoke 也不应修改按钮状态", async () => {
    // 即便 invoke 配置为返回 "Playing"，F-002 守卫应在调用前提前返回
    vi.mocked(invoke).mockResolvedValue("Playing");

    await updatePlaybackButtons("");

    // invoke 不应被调用（getWallpaperState 未触发）
    expect(invoke).not.toHaveBeenCalled();
    // 按钮应保持初始禁用状态（未被覆盖）
    expect(pauseBtn.disabled).toBe(true);
    expect(resumeBtn.disabled).toBe(true);
  });
});

// ── v41-F-001 / v41-F-002 滑块 IPC 失败 UI 回滚测试 ──────────────────────────

describe("v41-F-001 volume slider IPC 失败 UI 回滚", () => {
  let volumeSlider: HTMLInputElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    volumeSlider = document.createElement("input");
    volumeSlider.id = "volume-slider";
    volumeSlider.type = "range";
    volumeSlider.min = "0";
    volumeSlider.max = "100";
    volumeSlider.value = "50";
    document.body.appendChild(volumeSlider);
    vi.clearAllMocks();
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("v41_f001_volume_slider_rolled_back_on_failure", async () => {
    // set_volume 失败，其余 IPC 命令正常 resolve
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "set_volume") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(undefined);
    });

    await init();

    // 模拟用户拖动滑块：值从 50 变为 80
    volumeSlider.value = "80";
    volumeSlider.dispatchEvent(new Event("input"));

    // 等待异步 catch 完成，断言滑块回滚到原值 50
    await vi.waitFor(() => {
      expect(volumeSlider.value).toBe("50");
    });
    expect(showStatus).toHaveBeenCalledWith("音量设置失败，请重试", "error");
    expect(log.error).toHaveBeenCalledWith("音量设置失败:", expect.any(Error));
  });
});

describe("v41-F-002 speed slider IPC 失败 UI 回滚", () => {
  let speedSlider: HTMLInputElement;
  let speedValue: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    speedSlider = document.createElement("input");
    speedSlider.id = "speed-slider";
    speedSlider.type = "range";
    speedSlider.min = "0.25";
    speedSlider.max = "4.0";
    speedSlider.step = "0.25";
    speedSlider.value = "1.0";
    speedValue = document.createElement("span");
    speedValue.id = "speed-value";
    speedValue.textContent = "1.00x";
    document.body.appendChild(speedSlider);
    document.body.appendChild(speedValue);
    vi.clearAllMocks();
    vi.mocked(invoke).mockResolvedValue(undefined);
    vi.mocked(getVersion).mockResolvedValue("1.0.0");
    vi.mocked(loadConfig).mockResolvedValue(undefined);
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("v41_f002_speed_slider_rolled_back_on_failure", async () => {
    // set_speed 失败，其余 IPC 命令正常 resolve
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "set_speed") {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve(undefined);
    });

    await init();

    // 模拟用户拖动滑块：值从 1.0 变为 2.5
    speedSlider.value = "2.5";
    speedSlider.dispatchEvent(new Event("input"));

    // 等待异步 catch 完成，断言滑块回滚到原值 1.0
    await vi.waitFor(() => {
      expect(speedSlider.value).toBe("1.0");
    });
    expect(showStatus).toHaveBeenCalledWith("设置速度失败，请重试", "error");
    expect(log.error).toHaveBeenCalledWith("设置速度失败", expect.any(Error));
  });
});
