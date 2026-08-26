import { describe, it, expect, vi, beforeEach } from "vitest";

// mock logger 以便断言 log.error 调用
vi.mock("./logger", () => ({
  log: {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  },
}));

import { log } from "./logger";
import { runAsync } from "./async-helpers";

describe("runAsync", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("fn 成功 resolve 时不调用 log.error 也不调用 errorHandler", async () => {
    const fn = vi.fn().mockResolvedValue(undefined);
    const errorHandler = vi.fn();

    runAsync(fn, "应不报错", errorHandler);

    // 等待微任务完成
    await new Promise(resolve => setTimeout(resolve, 0));

    expect(fn).toHaveBeenCalledTimes(1);
    expect(log.error).not.toHaveBeenCalled();
    expect(errorHandler).not.toHaveBeenCalled();
  });

  it("fn 抛错且未提供 errorHandler 时调用 log.error", async () => {
    const err = new Error("boom");
    const fn = vi.fn().mockRejectedValue(err);

    runAsync(fn, "默认错误处理");

    await new Promise(resolve => setTimeout(resolve, 0));

    expect(fn).toHaveBeenCalledTimes(1);
    expect(log.error).toHaveBeenCalledWith("默认错误处理", err);
  });

  it("v41_f005_run_async_supports_custom_error_handler", async () => {
    const err = new Error("custom boom");
    const fn = vi.fn().mockRejectedValue(err);
    const errorHandler = vi.fn();

    runAsync(fn, "应使用自定义 handler", errorHandler);

    await new Promise(resolve => setTimeout(resolve, 0));

    expect(fn).toHaveBeenCalledTimes(1);
    // v41-F-005: 自定义 errorHandler 被调用，替代 log.error
    expect(errorHandler).toHaveBeenCalledTimes(1);
    expect(errorHandler).toHaveBeenCalledWith(err);
    // log.error 不应被调用（已被 errorHandler 替代）
    expect(log.error).not.toHaveBeenCalled();
  });
});
