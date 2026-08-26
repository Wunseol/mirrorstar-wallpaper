import { log } from "./logger";

/**
 * F-011: 在同步事件回调中调用 async 函数并捕获错误。
 *
 * 封装 `void (async () => { await fn(); })().catch((e) => log.error(errorMsg, e))` 模式，
 * 避免重复 IIFE 与遗漏 catch（如 F-004 的 catch 丢失错误对象）。
 *
 * v41-F-005 文档化：runAsync 仅记录 console.error（或调用自定义 errorHandler），不调用 showStatus。
 * 调用方负责在需要用户感知时调用 showStatus 显示友好错误信息。
 * 如需自定义错误处理，传入 errorHandler 回调（v41-F-005 扩展）。
 *
 * @param fn 异步函数（无参数，无返回值）
 * @param errorMsg 错误日志消息（如 "updatePlaybackButtons 失败"）
 * @param errorHandler 可选的自定义错误处理回调；若提供则替代 log.error 被调用
 */
export function runAsync(
  fn: () => Promise<void>,
  errorMsg: string,
  errorHandler?: (e: unknown) => void,
): void {
  void (async () => {
    await fn();
  })().catch((e: unknown) => {
    if (errorHandler) {
      errorHandler(e);
    } else {
      log.error(errorMsg, e);
    }
  });
}
