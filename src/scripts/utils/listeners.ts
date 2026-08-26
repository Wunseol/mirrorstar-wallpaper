import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// 事件监听清理机制：统一登记 unlisten 函数，窗口卸载时一次性清理
const cleanups: Array<() => void> = [];

/** 登记一个清理函数（用于非 Tauri event 的监听器，如 DOM addEventListener / webview.onDragDropEvent） */
export function registerCleanup(fn: () => void): void {
  cleanups.push(fn);
}

/** 监听 Tauri 事件并自动登记清理函数；handler 接收解包后的 payload */
export async function listenWithCleanup<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  const unlisten = await listen<T>(event, (e) => handler(e.payload));
  cleanups.push(unlisten);
  return unlisten;
}

/**
 * 添加 DOM 事件监听并自动登记清理函数（F-012 / v41-F-013）。
 *
 * 1. 清理闭包推入模块级 `cleanups` 数组，由 `cleanupAllListeners()` 在窗口卸载时批量清理；
 *    适用于页面级长生命周期 target（如 `document`、`window`、应用级 DOM 元素）。
 *
 * 2. 若 `target` 是动态创建的 DOM 元素（如模态框、临时卡片），必须在元素销毁时显式
 *    `target.removeEventListener(...)`，否则清理闭包对 target 的强引用会泄漏；
 *    推荐改用 `registerCleanup(() => target.removeEventListener(...))` 显式登记。
 *
 * @param target 事件目标（应为长生命周期元素）
 * @param type 事件类型
 * @param listener 监听器
 * @param options addEventListener 选项
 */
export function addEventListenerWithCleanup(
  target: EventTarget,
  type: string,
  listener: EventListenerOrEventListenerObject,
  options?: boolean | AddEventListenerOptions,
): void {
  target.addEventListener(type, listener, options);
  cleanups.push(() => target.removeEventListener(type, listener, options));
}

/** 清理所有已登记的监听器（窗口卸载时调用） */
export function cleanupAllListeners(): void {
  cleanups.forEach((fn) => {
    try {
      fn();
    } catch {
      /* ignore cleanup errors */
    }
  });
  cleanups.length = 0;
}
