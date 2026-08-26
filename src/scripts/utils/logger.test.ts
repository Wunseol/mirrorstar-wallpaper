import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { log } from "./logger";

describe("logger", () => {
  let logSpy: ReturnType<typeof vi.spyOn>;
  let warnSpy: ReturnType<typeof vi.spyOn>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    logSpy.mockRestore();
    warnSpy.mockRestore();
    errorSpy.mockRestore();
  });

  it("log.info 调用 console.log（dev 模式）", () => {
    log.info("info message", 123);
    expect(logSpy).toHaveBeenCalledWith("info message", 123);
  });

  it("log.warn 调用 console.warn", () => {
    log.warn("warn message");
    expect(warnSpy).toHaveBeenCalledWith("warn message");
  });

  it("log.error 调用 console.error", () => {
    log.error("error message", { code: 500 });
    expect(errorSpy).toHaveBeenCalledWith("error message", { code: 500 });
  });

  it("log.info 无参数时不抛错", () => {
    expect(() => log.info()).not.toThrow();
  });
});
