// ── Utility ──────────────────────────────────────────────────────────────────

// FE-002: statusTimer 移入 utils.ts 模块内部（仅 showStatus 使用，无需跨模块暴露）
let statusTimer: ReturnType<typeof setTimeout> | null = null;

// v5.0 F-PERF-015: 状态提示元素引用缓存。
// 原实现每次 showStatus 都执行 document.querySelector(".status-message")。
// showStatus 在多处调用（错误/成功/信息提示），缓存后消除重复查询。
// 特殊处理：元素可能被外部移除（如测试 beforeEach 清空 DOM），使用 isConnected
// 检测缓存元素是否仍在 DOM 中，失效时回退到查询+创建路径。
let cachedStatusEl: HTMLDivElement | null = null;

/**
 * 防抖函数：延迟执行，避免连续触发。
 *
 * F10: 类型签名使用 `unknown[]` 替代 `any[]` 以避免弱化类型安全。
 * - `T extends (...args: unknown[]) => void` 允许任意参数类型的函数（unknown 为顶类型），
 *   同时保留调用处对参数的静态检查
 * - 返回类型使用 `Parameters<T>` 保留原函数的参数元组类型，调用方无需手动断言
 * - 注：`() => Promise<void>` 仍可赋值给 `() => void`（TS 对 void 返回类型的特殊宽松规则），
 *   因此 async 函数同样可作为 fn 传入
 */
export function debounce<T extends (...args: unknown[]) => void>(fn: T, delay: number): (...args: Parameters<T>) => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return (...args: Parameters<T>) => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      fn(...args);
    }, delay);
  };
}

// ── File helpers ─────────────────────────────────────────────────────────────

export const IMAGE_EXTENSIONS = new Set(["jpg", "jpeg", "png", "bmp", "webp"]);
// FE-012: VIDEO/GIF/HTML_EXTENSIONS 仅在 isSupportedFile 内部使用，无外部消费者，改为模块私有。
const VIDEO_EXTENSIONS = new Set(["mp4", "avi", "mkv", "webm", "mov"]);
const GIF_EXTENSIONS = new Set(["gif"]);
const HTML_EXTENSIONS = new Set(["html", "htm"]);

export function getFileExtension(filePath: string): string {
  const parts = filePath.split(/[/\\]/);
  const fileName = parts[parts.length - 1] ?? "";
  const dotIndex = fileName.lastIndexOf(".");
  return dotIndex >= 0 ? fileName.substring(dotIndex + 1).toLowerCase() : "";
}

export function isSupportedFile(filePath: string): boolean {
  const ext = getFileExtension(filePath);
  return (
    IMAGE_EXTENSIONS.has(ext) ||
    VIDEO_EXTENSIONS.has(ext) ||
    GIF_EXTENSIONS.has(ext) ||
    HTML_EXTENSIONS.has(ext)
  );
}

export function extractFileName(filePath: string): string {
  const parts = filePath.split(/[/\\]/);
  return parts[parts.length - 1] || filePath;
}

/**
 * 释放 <video> 元素的解码资源：暂停 + 移除 src + 触发 load。
 *
 * F-006: 在 innerHTML 清空或卡片移除前必须显式调用，否则浏览器仍持有视频解码器
 * 导致内存泄漏（即使 DOM 节点被 GC）。三步组合是规范的视频释放序列：
 * 1. pause() — 停止播放与解码循环
 * 2. removeAttribute("src") — 解除媒体资源引用
 * 3. load() — 触发空 src 的加载周期，强制释放解码器
 *
 * 调用方需在调用前自行清理 onerror 等事件处理器（避免 load() 触发错误回调）。
 */
export function releaseVideoElement(video: HTMLVideoElement): void {
  video.pause();
  video.removeAttribute("src");
  video.load();
}

export function typeIcon(type: string): string {
  switch (type) {
    case "Image": return "🖼";
    case "Video": return "🎬";
    case "Gif":   return "🎞";
    case "Web":   return "🌐";
    default:      return "📄";
  }
}

export function showStatus(message: string, type: "error" | "success" | "info" | "warning") {
  // v5.0 F-PERF-015: 优先使用缓存引用（若仍在 DOM 中），避免重复 querySelector
  let el: HTMLDivElement | null = cachedStatusEl?.isConnected ? cachedStatusEl : null;
  if (!el) {
    el = document.querySelector(".status-message") as HTMLDivElement | null;
  }
  if (!el) {
    el = document.createElement("div");
    el.className = "status-message";
    el.setAttribute("role", "status");
    el.setAttribute("aria-live", "polite");
    el.setAttribute("aria-atomic", "true");
    document.body.appendChild(el);
  }
  cachedStatusEl = el;
  el.textContent = message;
  el.className = `status-message ${type}`;
  el.style.opacity = "1";
  // FE-002: statusTimer 已从 appState 移至模块内部
  if (statusTimer) clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    el!.style.opacity = "0";
  }, 3000);
}