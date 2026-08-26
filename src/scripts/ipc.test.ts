import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  getErrorMessage,
  getErrorCode,
  getWallpapers,
  addWallpaper,
  removeWallpaper,
  setWallpaper,
  pauseWallpaper,
  resumeWallpaper,
  getConfig,
  updateConfig,
  setVolume,
  toggleMute,
  setSpeed,
  getWallpaperState,
  setInteractionMode,
  getDisplays,
  setScalingMode,
  toggleAutoStart,
  openFileDialog,
  getAutoStartStatus,
  invokeWithTimeout,
} from "./ipc";

// mock @tauri-apps/api/core 的 invoke，避免真实 IPC 调用
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

describe("getErrorMessage", () => {
  it("字符串输入原样返回", () => {
    expect(getErrorMessage("网络连接失败")).toBe("网络连接失败");
  });

  it("普通英文字符串错误返回自身", () => {
    expect(getErrorMessage("timeout")).toBe("timeout");
  });

  it("含 message 字段的对象返回其 message", () => {
    expect(getErrorMessage({ message: "自定义错误" })).toBe("自定义错误");
  });

  it("Error 对象返回其 message", () => {
    expect(getErrorMessage(new Error("boom"))).toBe("boom");
  });

  it("null 输入返回 'null'", () => {
    expect(getErrorMessage(null)).toBe("null");
  });

  it("undefined 输入返回 'undefined'", () => {
    expect(getErrorMessage(undefined)).toBe("undefined");
  });

  it("数字输入返回字符串化结果", () => {
    expect(getErrorMessage(42)).toBe("42");
  });

  it("不含 message 的对象返回 [object Object]", () => {
    expect(getErrorMessage({ code: 500 })).toBe("[object Object]");
  });

  it("message 为非字符串时调用 String 转换", () => {
    expect(getErrorMessage({ message: 123 })).toBe("123");
  });
});

// ── v16-A-013: getErrorCode 提取 MirrorStarError 的 code 字段 ──────────────────

describe("v16-A-013: getErrorCode", () => {
  it("MirrorStarError 序列化对象返回 code 字符串", () => {
    expect(getErrorCode({ code: "InvalidConfig", message: "音频音量越界" })).toBe("InvalidConfig");
  });

  it("code 为 InvalidPath 时正确返回", () => {
    expect(getErrorCode({ code: "InvalidPath", message: "路径不存在" })).toBe("InvalidPath");
  });

  it("Error 对象（无 code 字段）返回 null", () => {
    expect(getErrorCode(new Error("命令超时"))).toBeNull();
  });

  it("字符串错误返回 null", () => {
    expect(getErrorCode("网络错误")).toBeNull();
  });

  it("null/undefined 返回 null", () => {
    expect(getErrorCode(null)).toBeNull();
    expect(getErrorCode(undefined)).toBeNull();
  });

  it("code 为非字符串时返回 null", () => {
    expect(getErrorCode({ code: 500, message: "server error" })).toBeNull();
  });

  it("对象无 code 字段时返回 null", () => {
    expect(getErrorCode({ message: "无 code" })).toBeNull();
  });
});

// ── v16-C-005: sanitizeErrorMessage 脱敏改进（basename 保留 + OS 错误码中文映射） ──

describe("v16-C-005: sanitizeErrorMessage 改进", () => {
  it("Windows 路径仅保留 basename，不泄露目录结构", () => {
    // C:\Users\test\file.mp4 → file.mp4
    expect(getErrorMessage("复制壁纸文件失败: C:\\Users\\test\\file.mp4")).toBe(
      "复制壁纸文件失败: file.mp4",
    );
  });

  it("Unix 绝对路径仅保留 basename", () => {
    // /home/user/vid.mp4 → vid.mp4
    expect(getErrorMessage("文件不存在: /home/user/vid.mp4")).toBe("文件不存在: vid.mp4");
  });

  it("正斜杠 Windows 路径仅保留 basename", () => {
    expect(getErrorMessage("路径错误: C:/Users/test/wallpaper.gif")).toBe(
      "路径错误: wallpaper.gif",
    );
  });

  it("消息含多个路径时各自保留 basename", () => {
    expect(
      getErrorMessage(
        "符号链接: C:\\a\\link.mp4 指向 C:\\b\\target.mp4",
      ),
    ).toBe("符号链接: link.mp4 指向 target.mp4");
  });

  it("os error 5（EACCES）映射为「权限不足」", () => {
    // Rust io::Error Display: "Access is denied. (os error 5)"
    expect(getErrorMessage("拒绝访问。 (os error 5)")).toBe("拒绝访问。 (权限不足)");
  });

  it("os error 28（ENOSPC）映射为「磁盘空间不足」", () => {
    expect(getErrorMessage("No space left (os error 28)")).toBe("No space left (磁盘空间不足)");
  });

  it("os error 32（ESHARING）映射为「文件被占用」", () => {
    expect(getErrorMessage("共享冲突 (os error 32)")).toBe("共享冲突 (文件被占用)");
  });

  it("未命中的 os error 码回退为 <error-code> 占位符", () => {
    // 9999 不在映射表中
    expect(getErrorMessage("未知错误 (os error 9999)")).toBe("未知错误 (<error-code>)");
  });

  it("WinError(N) 带括号形式映射为中文", () => {
    expect(getErrorMessage("WinError(5)")).toBe("权限不足");
  });

  it("OS error (N) 带括号形式映射为中文", () => {
    expect(getErrorMessage("OS error (28)")).toBe("磁盘空间不足");
  });

  it("Win32 HRESULT 错误码 0x80004005 替换为 <error-code>", () => {
    expect(getErrorMessage("HRESULT: 0x80004005")).toBe("HRESULT: <error-code>");
  });

  it("堆栈行被移除", () => {
    const msg = "Error: boom\n    at foo (bar.ts:10)\n    at baz (qux.ts:20)";
    expect(getErrorMessage(msg)).toBe("Error: boom");
  });

  it("简单错误消息不误伤（无路径/错误码/堆栈）", () => {
    expect(getErrorMessage("网络连接失败")).toBe("网络连接失败");
    expect(getErrorMessage("timeout")).toBe("timeout");
    expect(getErrorMessage("add failed")).toBe("add failed");
  });

  it("超长消息截断至 200 字符并附加省略号", () => {
    // "错误：" 每组 3 个字符（错/误/：），70 组 = 210 字符 > 200 上限
    const long = "错误：".repeat(70);
    const result = getErrorMessage(long);
    expect(result.length).toBe(203); // 200 + "..."
    expect(result.endsWith("...")).toBe(true);
  });

  it("路径与 os error 混合时同时应用脱敏规则", () => {
    // 路径保留 basename，os error 5 映射为中文
    expect(
      getErrorMessage("复制 C:\\Users\\test\\file.mp4 失败: (os error 5)"),
    ).toBe("复制 file.mp4 失败: (权限不足)");
  });
});

describe("IPC wrappers", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("getWallpapers 调用 get_wallpapers 命令并透传返回值", async () => {
    const expected = [
      {
        id: "w1",
        file_path: "C:/a.mp4",
        wallpaper_type: "Video",
        display_id: null,
        added_at: "2026-01-01",
        thumbnail: "thumb",
        file_size: 100,
        metadata: null,
      },
    ];
    vi.mocked(invoke).mockResolvedValue(expected);
    expect(await getWallpapers()).toBe(expected);
    expect(invoke).toHaveBeenCalledWith("get_wallpapers");
  });

  it("addWallpaper 带 displayId 时原样传递", async () => {
    vi.mocked(invoke).mockResolvedValue("id-1");
    expect(await addWallpaper("C:/a.mp4", "disp1")).toBe("id-1");
    expect(invoke).toHaveBeenCalledWith("add_wallpaper", {
      filePath: "C:/a.mp4",
      displayId: "disp1",
    });
  });

  it("addWallpaper displayId 缺省时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue("id-2");
    await addWallpaper("C:/b.mp4");
    expect(invoke).toHaveBeenCalledWith("add_wallpaper", {
      filePath: "C:/b.mp4",
      displayId: null,
    });
  });

  it("addWallpaper displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue("id-3");
    await addWallpaper("C:/c.mp4", "");
    expect(invoke).toHaveBeenCalledWith("add_wallpaper", {
      filePath: "C:/c.mp4",
      displayId: null,
    });
  });

  it("removeWallpaper 调用 remove_wallpaper 命令并传递 wallpaperId 与 deleteFile", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await removeWallpaper("w1", true);
    // v16-B-005: 后端形参 wallpaper_id，Tauri v2 自动 camelCase → snake_case
    expect(invoke).toHaveBeenCalledWith("remove_wallpaper", {
      wallpaperId: "w1",
      deleteFile: true,
    });
  });

  it("setWallpaper 仅传 wallpaperId 时 displayId 为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setWallpaper("w1");
    expect(invoke).toHaveBeenCalledWith("set_wallpaper", {
      wallpaperId: "w1",
      displayId: null,
    });
  });

  it("setWallpaper 传 wallpaperId 与 displayId 时原样传递", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setWallpaper("w1", "disp1");
    expect(invoke).toHaveBeenCalledWith("set_wallpaper", {
      wallpaperId: "w1",
      displayId: "disp1",
    });
  });

  it("setWallpaper 传 scalingMode 时序列化到 args（v16-B-004）", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setWallpaper("w1", "disp1", "fill");
    expect(invoke).toHaveBeenCalledWith("set_wallpaper", {
      wallpaperId: "w1",
      displayId: "disp1",
      scalingMode: "fill",
    });
  });

  it("setWallpaper 不传 scalingMode 时不包含该字段（v16-B-004）", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setWallpaper("w1", "disp1");
    const callArgs = vi.mocked(invoke).mock.calls[0]![1] as Record<string, unknown>;
    expect(callArgs.scalingMode).toBeUndefined();
  });

  it("pauseWallpaper 带 displayId 时原样传递", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await pauseWallpaper("disp1");
    expect(invoke).toHaveBeenCalledWith("pause_wallpaper", {
      displayId: "disp1",
    });
  });

  it("pauseWallpaper displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await pauseWallpaper("");
    expect(invoke).toHaveBeenCalledWith("pause_wallpaper", {
      displayId: null,
    });
  });

  it("resumeWallpaper 带 displayId 时原样传递", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await resumeWallpaper("disp1");
    expect(invoke).toHaveBeenCalledWith("resume_wallpaper", {
      displayId: "disp1",
    });
  });

  it("resumeWallpaper displayId 缺省时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await resumeWallpaper();
    expect(invoke).toHaveBeenCalledWith("resume_wallpaper", {
      displayId: null,
    });
  });

  it("getConfig 调用 get_config 命令并透传返回值", async () => {
    const config = {
      general: { auto_start: false, minimize_to_tray: true },
      audio: { volume: 50, muted: false },
      pause: { fullscreen_action: "terminate" as const, pause_on_battery: false },
      display: { arrangement: "per_monitor" },
      video: { hwdec: true, speed: 1 },
      gif: { memory_strategy: "Balanced", balanced_keep_frames: 30, max_memory_mb: 40 },
    };
    vi.mocked(invoke).mockResolvedValue(config);
    expect(await getConfig()).toBe(config);
    expect(invoke).toHaveBeenCalledWith("get_config");
  });

  it("updateConfig 调用 update_config 命令并传递 config 对象", async () => {
    const config = {
      general: { auto_start: true, minimize_to_tray: false },
      audio: { volume: 80, muted: true },
      pause: { fullscreen_action: "pause" as const, pause_on_battery: true },
      display: { arrangement: "span" as const },
      video: { hwdec: false, speed: 2 },
      gif: { memory_strategy: "Performance" as const, balanced_keep_frames: 10, max_memory_mb: 40 },
    };
    vi.mocked(invoke).mockResolvedValue(undefined);
    await updateConfig(config);
    expect(invoke).toHaveBeenCalledWith("update_config", { config });
  });

  it("setVolume 调用 set_volume 命令并传递 displayId 与 volume", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setVolume("disp1", 75);
    expect(invoke).toHaveBeenCalledWith("set_volume", {
      displayId: "disp1",
      volume: 75,
    });
  });

  it("FE-001: setVolume displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setVolume("", 50);
    expect(invoke).toHaveBeenCalledWith("set_volume", {
      displayId: null,
      volume: 50,
    });
  });

  it("FE-001: setVolume displayId 缺省时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setVolume(undefined, 50);
    expect(invoke).toHaveBeenCalledWith("set_volume", {
      displayId: null,
      volume: 50,
    });
  });

  it("toggleMute 调用 toggle_mute 命令并透传 boolean 返回值", async () => {
    vi.mocked(invoke).mockResolvedValue(true);
    expect(await toggleMute("disp1")).toBe(true);
    expect(invoke).toHaveBeenCalledWith("toggle_mute", { displayId: "disp1" });
  });

  it("FE-001: toggleMute displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(false);
    await toggleMute("");
    expect(invoke).toHaveBeenCalledWith("toggle_mute", { displayId: null });
  });

  it("FE-001: toggleMute displayId 缺省时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(false);
    await toggleMute(undefined);
    expect(invoke).toHaveBeenCalledWith("toggle_mute", { displayId: null });
  });

  it("setSpeed 调用 set_speed 命令并传递 displayId 与 speed", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setSpeed("disp1", 1.5);
    expect(invoke).toHaveBeenCalledWith("set_speed", {
      displayId: "disp1",
      speed: 1.5,
    });
  });

  it("FE-001: setSpeed displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setSpeed("", 1.0);
    expect(invoke).toHaveBeenCalledWith("set_speed", {
      displayId: null,
      speed: 1.0,
    });
  });

  it("getWallpaperState 调用 get_wallpaper_state 命令并透传状态字符串", async () => {
    vi.mocked(invoke).mockResolvedValue("Playing");
    expect(await getWallpaperState("disp1")).toBe("Playing");
    expect(invoke).toHaveBeenCalledWith("get_wallpaper_state", {
      displayId: "disp1",
    });
  });

  it("getWallpaperState 后端返回 null 时透传 null", async () => {
    vi.mocked(invoke).mockResolvedValue(null);
    expect(await getWallpaperState("disp1")).toBeNull();
  });

  it("FE-001: getWallpaperState displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(null);
    await getWallpaperState("");
    expect(invoke).toHaveBeenCalledWith("get_wallpaper_state", {
      displayId: null,
    });
  });

  it("setInteractionMode 传 true 时透传 enabled", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setInteractionMode(true);
    expect(invoke).toHaveBeenCalledWith("set_interaction_mode", {
      enabled: true,
    });
  });

  it("setInteractionMode 传 false 时透传 enabled", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setInteractionMode(false);
    expect(invoke).toHaveBeenCalledWith("set_interaction_mode", {
      enabled: false,
    });
  });

  it("getDisplays 调用 get_displays 命令并透传返回值", async () => {
    const displays = [
      {
        id: "disp1",
        name: "Monitor 1",
        width: 1920,
        height: 1080,
        x: 0,
        y: 0,
        is_primary: true,
        dpi: 96,
        current_wallpaper: null,
      },
    ];
    vi.mocked(invoke).mockResolvedValue(displays);
    expect(await getDisplays()).toBe(displays);
    expect(invoke).toHaveBeenCalledWith("get_displays");
  });

  it("setScalingMode 调用 set_scaling_mode 命令并传递 displayId 与 mode", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setScalingMode("disp1", "stretch");
    expect(invoke).toHaveBeenCalledWith("set_scaling_mode", {
      displayId: "disp1",
      mode: "stretch",
    });
  });

  it("FE-001: setScalingMode displayId 为空字符串时序列化为 null", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setScalingMode("", "fit");
    expect(invoke).toHaveBeenCalledWith("set_scaling_mode", {
      displayId: null,
      mode: "fit",
    });
  });

  it("toggleAutoStart 传 true 时透传 enabled", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await toggleAutoStart(true);
    expect(invoke).toHaveBeenCalledWith("toggle_auto_start", {
      enabled: true,
    });
  });

  it("toggleAutoStart 传 false 时透传 enabled", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await toggleAutoStart(false);
    expect(invoke).toHaveBeenCalledWith("toggle_auto_start", {
      enabled: false,
    });
  });

  it("openFileDialog 返回字符串路径时透传", async () => {
    vi.mocked(invoke).mockResolvedValue("C:/selected.mp4");
    expect(await openFileDialog()).toBe("C:/selected.mp4");
    expect(invoke).toHaveBeenCalledWith("open_file_dialog");
  });

  it("openFileDialog 后端返回 null 时透传 null", async () => {
    vi.mocked(invoke).mockResolvedValue(null);
    expect(await openFileDialog()).toBeNull();
  });

  it("getAutoStartStatus 后端返回 true 时透传", async () => {
    vi.mocked(invoke).mockResolvedValue(true);
    expect(await getAutoStartStatus()).toBe(true);
    expect(invoke).toHaveBeenCalledWith("get_auto_start_status");
  });

  it("getAutoStartStatus 后端返回 false 时透传", async () => {
    vi.mocked(invoke).mockResolvedValue(false);
    expect(await getAutoStartStatus()).toBe(false);
  });
});

// ── F09: invokeWithTimeout 超时封装 ───────────────────────────────────────────

describe("invokeWithTimeout (F09)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("命令在超时前 resolve 时透传结果", async () => {
    vi.mocked(invoke).mockResolvedValue("ok");
    const result = await invokeWithTimeout<string>("fast_cmd");
    expect(result).toBe("ok");
    expect(invoke).toHaveBeenCalledWith("fast_cmd", undefined);
  });

  it("命令超时后 reject 并附带命令名与超时时长", async () => {
    // invoke 永不 resolve，强制触发超时
    vi.mocked(invoke).mockImplementation(() => new Promise(() => {}));
    const promise = invokeWithTimeout<string>("slow_cmd", { foo: "bar" }, 1000);
    vi.advanceTimersByTime(1000);
    await expect(promise).rejects.toThrow("命令 slow_cmd 超时（1000ms）");
    expect(invoke).toHaveBeenCalledWith("slow_cmd", { foo: "bar" });
  });

  it("命令在超时前 reject 时透传原始错误", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("ipc failure"));
    await expect(invokeWithTimeout<string>("fail_cmd", undefined, 1000)).rejects.toThrow("ipc failure");
  });

  it("默认超时为 10000ms", async () => {
    vi.mocked(invoke).mockImplementation(() => new Promise(() => {}));
    const promise = invokeWithTimeout<string>("default_timeout_cmd");
    // 推进 9999ms，promise 仍应 pending（默认 10000ms 未到）
    await vi.advanceTimersByTimeAsync(9999);
    let settled = false;
    promise.then(
      () => { settled = true; },
      () => { settled = true; },
    );
    await Promise.resolve();
    expect(settled).toBe(false);
    // 再推进 1ms 达到 10000ms，触发超时 reject
    await vi.advanceTimersByTimeAsync(1);
    await expect(promise).rejects.toThrow("命令 default_timeout_cmd 超时（10000ms）");
  });

  it("setWallpaper 通过 invokeWithTimeout 调用 invoke（耗时命令）", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await setWallpaper("w1", "disp1");
    expect(invoke).toHaveBeenCalledWith("set_wallpaper", {
      wallpaperId: "w1",
      displayId: "disp1",
    });
  });
});

// ── F-003: invokeWithTimeout try/finally 清理 timer ────────────────────────────

describe("F-003: invokeWithTimeout clears timer on completion", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("命令快速 resolve 时调用 clearTimeout 清理 timer", async () => {
    const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
    vi.mocked(invoke).mockResolvedValue("result");

    const result = await invokeWithTimeout<string>("test_cmd");

    expect(result).toBe("result");
    expect(clearTimeoutSpy).toHaveBeenCalled();

    clearTimeoutSpy.mockRestore();
  });

  it("命令快速 reject 时也调用 clearTimeout 清理 timer", async () => {
    const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
    vi.mocked(invoke).mockRejectedValue(new Error("cmd failed"));

    await expect(invokeWithTimeout<string>("test_cmd")).rejects.toThrow("cmd failed");
    expect(clearTimeoutSpy).toHaveBeenCalled();

    clearTimeoutSpy.mockRestore();
  });

  it("命令超时后 finally 块仍调用 clearTimeout（timer 已触发）", async () => {
    const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
    // invoke 永不 resolve，强制触发超时
    vi.mocked(invoke).mockImplementation(() => new Promise(() => {}));

    const promise = invokeWithTimeout<string>("test_cmd", undefined, 10000);

    // 推进时间使 setTimeout 触发 reject
    vi.advanceTimersByTime(10000);

    await expect(promise).rejects.toThrow("命令 test_cmd 超时");
    expect(clearTimeoutSpy).toHaveBeenCalled();

    clearTimeoutSpy.mockRestore();
  });
});

// ── v41-F-017: invokeWithTimeout 边界场景扩展 ─────────────────────────────────
// 覆盖超时触发 / 错误传播 / 0ms 边界等场景，补充 F09 与 F-003 基本路径之外的边界用例

describe("v41-F-017: invokeWithTimeout boundary scenarios", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("0ms 超时边界：立即触发超时 reject（不阻塞调用方）", async () => {
    // invoke 永不 resolve，强制由 timeout 触发 reject
    vi.mocked(invoke).mockImplementation(() => new Promise(() => {}));
    const promise = invokeWithTimeout<string>("zero_timeout_cmd", undefined, 0);
    // 使用同步版本推进时间触发 0ms timer；若用 async 版本会先刷新微任务，
    // 导致 rejection 在 handler 附加前被处理，引发 unhandled rejection
    vi.advanceTimersByTime(1);
    await expect(promise).rejects.toThrow("命令 zero_timeout_cmd 超时（0ms）");
  });

  it("超时触发后 clearTimeout 被调用且仅调用一次（验证无 timer 泄漏）", async () => {
    const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
    // invoke 永不 resolve，强制触发超时
    vi.mocked(invoke).mockImplementation(() => new Promise(() => {}));

    const promise = invokeWithTimeout<string>("leak_check_cmd", undefined, 1000);
    vi.advanceTimersByTime(1000);

    await expect(promise).rejects.toThrow("命令 leak_check_cmd 超时");
    // finally 块应调用一次 clearTimeout 清理 timer，避免 timer 泄漏
    expect(clearTimeoutSpy).toHaveBeenCalledTimes(1);

    clearTimeoutSpy.mockRestore();
  });

  it("IPC 抛出字符串错误时原样传播给调用方", async () => {
    // 模拟后端抛出非 Error 对象（如字符串）
    vi.mocked(invoke).mockRejectedValue("backend string error");
    await expect(
      invokeWithTimeout<string>("string_err_cmd", undefined, 1000),
    ).rejects.toBe("backend string error");
  });

  it("IPC 抛出含 code 字段的结构化错误对象时原样传播", async () => {
    // 模拟后端抛出结构化错误对象（{ code, message } 风格）
    const structuredError = { code: "IpcError", message: "command failed" };
    vi.mocked(invoke).mockRejectedValue(structuredError);
    await expect(
      invokeWithTimeout<string>("structured_err_cmd", undefined, 1000),
    ).rejects.toBe(structuredError);
  });

  it("IPC 抛出 Error 实例时透传引用与原型链", async () => {
    const error = new Error("detailed failure");
    vi.mocked(invoke).mockRejectedValue(error);
    // 验证调用方收到的是同一个 Error 引用（而非包装后的新对象）
    await expect(
      invokeWithTimeout<string>("error_instance_cmd", undefined, 1000),
    ).rejects.toBe(error);
  });

  it("命令在超时前 resolve 时 clearTimeout 仅调用一次（无重复清理）", async () => {
    const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
    vi.mocked(invoke).mockResolvedValue("fast-result");

    const result = await invokeWithTimeout<string>("fast_resolve_cmd", undefined, 5000);

    expect(result).toBe("fast-result");
    // resolve 路径下 finally 仅调用一次 clearTimeout
    expect(clearTimeoutSpy).toHaveBeenCalledTimes(1);

    clearTimeoutSpy.mockRestore();
  });
});
