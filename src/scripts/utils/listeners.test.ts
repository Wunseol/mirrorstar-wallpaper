import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// mock @tauri-apps/api/event 的 listen 函数，避免依赖 Tauri 运行时
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_event: string, handler: (e: { payload: unknown }) => void) => {
    // 返回一个 spy unlisten，并保留 handler 引用便于测试触发
    const unlisten = vi.fn();
    (unlisten as unknown as { _handler: typeof handler })._handler = handler;
    return unlisten;
  }),
}));

import {
  registerCleanup,
  listenWithCleanup,
  addEventListenerWithCleanup,
  cleanupAllListeners,
} from "./listeners";
import { listen } from "@tauri-apps/api/event";

describe("registerCleanup", () => {
  beforeEach(() => {
    // 每个测试前清空 cleanups 数组
    cleanupAllListeners();
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanupAllListeners();
  });

  it("登记的清理函数在 cleanupAllListeners 时被调用", () => {
    const fn = vi.fn();
    registerCleanup(fn);

    cleanupAllListeners();

    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("多个清理函数按登记顺序依次调用", () => {
    const calls: string[] = [];
    registerCleanup(() => calls.push("first"));
    registerCleanup(() => calls.push("second"));
    registerCleanup(() => calls.push("third"));

    cleanupAllListeners();

    expect(calls).toEqual(["first", "second", "third"]);
  });

  it("cleanupAllListeners 后再次调用清理函数不再被触发（数组已清空）", () => {
    const fn = vi.fn();
    registerCleanup(fn);

    cleanupAllListeners();
    cleanupAllListeners();

    expect(fn).toHaveBeenCalledTimes(1);
  });
});

describe("cleanupAllListeners", () => {
  beforeEach(() => {
    cleanupAllListeners();
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanupAllListeners();
  });

  it("单个清理函数抛错不会中断其他清理函数（try/catch 吞错）", () => {
    const ok1 = vi.fn();
    const throwing = vi.fn(() => {
      throw new Error("cleanup failure");
    });
    const ok2 = vi.fn();

    registerCleanup(ok1);
    registerCleanup(throwing);
    registerCleanup(ok2);

    // 不应抛错（吞错行为）
    expect(() => cleanupAllListeners()).not.toThrow();

    // 抛错的清理函数之后的清理函数仍被调用
    expect(ok1).toHaveBeenCalledTimes(1);
    expect(throwing).toHaveBeenCalledTimes(1);
    expect(ok2).toHaveBeenCalledTimes(1);
  });

  it("多个连续抛错的清理函数均被尝试调用", () => {
    const throw1 = vi.fn(() => {
      throw new Error("first");
    });
    const throw2 = vi.fn(() => {
      throw new Error("second");
    });

    registerCleanup(throw1);
    registerCleanup(throw2);

    expect(() => cleanupAllListeners()).not.toThrow();
    expect(throw1).toHaveBeenCalledTimes(1);
    expect(throw2).toHaveBeenCalledTimes(1);
  });

  it("无清理函数时安全返回（空数组）", () => {
    expect(() => cleanupAllListeners()).not.toThrow();
  });
});

describe("listenWithCleanup", () => {
  beforeEach(() => {
    cleanupAllListeners();
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanupAllListeners();
  });

  it("调用 listen 注册事件，并返回 unlisten 函数", async () => {
    const handler = vi.fn();
    const unlisten = await listenWithCleanup("test-event", handler);

    expect(listen).toHaveBeenCalledTimes(1);
    expect(typeof unlisten).toBe("function");
  });

  it("登记的 unlisten 在 cleanupAllListeners 时被调用", async () => {
    const handler = vi.fn();
    const unlisten = await listenWithCleanup("test-event", handler);

    cleanupAllListeners();

    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("handler 接收解包后的 payload（不是原始 event 对象）", async () => {
    const handler = vi.fn();
    const unlisten = await listenWithCleanup<string>("test-event", handler);
    // 通过 mock 暴露的 _handler 触发事件
    const storedHandler = (unlisten as unknown as { _handler: (e: { payload: string }) => void })._handler;
    storedHandler({ payload: "hello" });

    expect(handler).toHaveBeenCalledWith("hello");
  });
});

describe("addEventListenerWithCleanup", () => {
  beforeEach(() => {
    cleanupAllListeners();
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanupAllListeners();
  });

  it("添加 DOM 事件监听器，触发时调用 listener", () => {
    const target = document.createElement("button");
    const listener = vi.fn();

    addEventListenerWithCleanup(target, "click", listener);

    target.click();

    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("cleanupAllListeners 时调用 removeEventListener 移除监听", () => {
    const target = document.createElement("button");
    const listener = vi.fn();

    addEventListenerWithCleanup(target, "click", listener);
    cleanupAllListeners();

    // 清理后再次触发，listener 不应被调用
    target.click();

    expect(listener).not.toHaveBeenCalled();
  });

  it("options 参数透传给 addEventListener 与 removeEventListener", () => {
    const target = document.createElement("div");
    const listener = vi.fn();
    // 用 spy 替换原生方法以验证 options 透传
    const addSpy = vi.spyOn(target, "addEventListener");
    const removeSpy = vi.spyOn(target, "removeEventListener");

    const options: AddEventListenerOptions = { capture: true, passive: false };
    addEventListenerWithCleanup(target, "wheel", listener, options);

    expect(addSpy).toHaveBeenCalledWith("wheel", listener, options);

    cleanupAllListeners();

    // removeEventListener 应使用相同的 options（capture 必须匹配才能正确移除）
    expect(removeSpy).toHaveBeenCalledWith("wheel", listener, options);
  });

  it("支持 EventTarget 子类（如 Window）", () => {
    const listener = vi.fn();

    addEventListenerWithCleanup(window, "resize", listener);

    window.dispatchEvent(new Event("resize"));

    expect(listener).toHaveBeenCalledTimes(1);

    cleanupAllListeners();

    window.dispatchEvent(new Event("resize"));
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("支持 EventListenerObject 形式", () => {
    const target = document.createElement("button");
    const handleEvent = vi.fn();
    const listenerObj: EventListenerObject = { handleEvent };

    addEventListenerWithCleanup(target, "click", listenerObj);

    target.click();

    expect(handleEvent).toHaveBeenCalledTimes(1);
  });
});
