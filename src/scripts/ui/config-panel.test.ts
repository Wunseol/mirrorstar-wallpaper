import { describe, it, expect, vi, beforeEach } from "vitest";

// mock 须在 import ./config-panel 之前声明（vitest 会自动提升到文件顶部）
// 屏蔽 ipc，使 patchConfig 内的 getConfig / updateConfig 完全可控
vi.mock("../ipc", () => ({
  getConfig: vi.fn(),
  updateConfig: vi.fn(),
}));

// 屏蔽 logger，保持测试输出整洁
vi.mock("../utils/logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

import { getConfig, updateConfig } from "../ipc";
import { patchConfig } from "./config-panel";
import type { AppConfig } from "../types";

/** 构造一份合法的默认 AppConfig，作为 getConfig 的返回值 */
function baseConfig(): AppConfig {
  return {
    general: { auto_start: false, minimize_to_tray: false },
    audio: { volume: 0.5, muted: false },
    pause: { fullscreen_action: "none", pause_on_battery: false },
    display: { arrangement: "per_monitor" },
    video: { hwdec: false, speed: 1.0 },
    gif: { memory_strategy: "Balanced", balanced_keep_frames: 20, max_memory_mb: 40 },
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
});

// ── F-002: patchConfig 串行化 ─────────────────────────────────────────────────

describe("patchConfig 串行化 (F-002)", () => {
  it("两次快速调用时第二次 getConfig 在第一次 updateConfig 之后才执行", async () => {
    const sequence: string[] = [];
    let getConfigCallCount = 0;
    let updateConfigCallCount = 0;
    // 第一次 updateConfig 返回 deferred，使其保持挂起态，便于观察调度顺序
    const firstUpdateDeferred = createDeferred<void>();

    vi.mocked(getConfig).mockImplementation(() => {
      getConfigCallCount++;
      sequence.push(`getConfig-${getConfigCallCount}`);
      return Promise.resolve(baseConfig());
    });
    vi.mocked(updateConfig).mockImplementation(() => {
      updateConfigCallCount++;
      sequence.push(`updateConfig-${updateConfigCallCount}`);
      if (updateConfigCallCount === 1) {
        return firstUpdateDeferred.promise;
      }
      return Promise.resolve();
    });

    // 快速连续发起两次调用（模拟用户快速勾选两个复选框）
    const p1 = patchConfig({ pause: { fullscreen_action: "terminate" } });
    const p2 = patchConfig({ pause: { pause_on_battery: true } });

    // 让 microtask 充分推进，使第一次 doPatchConfig 执行到 updateConfig（挂起）
    await flushMicrotasks();

    // 串行化关键断言：第一次 updateConfig 未 resolve 时，第二次 getConfig 尚未执行
    expect(getConfig).toHaveBeenCalledTimes(1);
    expect(updateConfig).toHaveBeenCalledTimes(1);
    expect(sequence).toEqual(["getConfig-1", "updateConfig-1"]);

    // resolve 第一次 updateConfig，触发第二次 getConfig-2 / updateConfig-2
    firstUpdateDeferred.resolve();
    await Promise.all([p1, p2]);

    expect(getConfig).toHaveBeenCalledTimes(2);
    expect(updateConfig).toHaveBeenCalledTimes(2);
    expect(sequence).toEqual([
      "getConfig-1",
      "updateConfig-1",
      "getConfig-2",
      "updateConfig-2",
    ]);
  });

  it("第一次 updateConfig 失败不阻塞第二次调用，且第一次调用者收到 reject", async () => {
    const sequence: string[] = [];
    let getConfigCallCount = 0;
    let updateConfigCallCount = 0;

    vi.mocked(getConfig).mockImplementation(() => {
      getConfigCallCount++;
      sequence.push(`getConfig-${getConfigCallCount}`);
      return Promise.resolve(baseConfig());
    });
    vi.mocked(updateConfig).mockImplementation(() => {
      updateConfigCallCount++;
      sequence.push(`updateConfig-${updateConfigCallCount}`);
      if (updateConfigCallCount === 1) {
        return Promise.reject(new Error("ipc failure"));
      }
      return Promise.resolve();
    });

    // 第一次调用：updateConfig reject，调用者应收到 reject
    const p1 = patchConfig({ pause: { fullscreen_action: "terminate" } });
    await expect(p1).rejects.toThrow("ipc failure");

    // 确保链已恢复为 resolved（catch 微任务已执行）
    await flushMicrotasks();

    // 第二次调用：链已恢复，应正常 resolve，且其 getConfig 被调用
    const p2 = patchConfig({ pause: { pause_on_battery: true } });
    await expect(p2).resolves.toBeUndefined();

    // 两次的 getConfig 都被调用，证明失败未阻塞后续
    expect(getConfig).toHaveBeenCalledTimes(2);
    expect(updateConfig).toHaveBeenCalledTimes(2);
    expect(sequence).toEqual([
      "getConfig-1",
      "updateConfig-1",
      "getConfig-2",
      "updateConfig-2",
    ]);
  });
});
