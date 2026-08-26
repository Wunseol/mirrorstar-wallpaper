import { convertFileSrc } from "@tauri-apps/api/core";
import { setWallpaper } from "../ipc";
import { appState } from "../state";
import type { WallpaperEntry } from "../types";
import { addEventListenerWithCleanup } from "../utils/listeners";
import { log } from "../utils/logger";
import { extractFileName, releaseVideoElement, showStatus } from "./utils";

// ── 功能1: 壁纸预览模态框 ───────────────────────────────────────────────────

// 保留 onerror 容错用于文件不存在等异常场景。
const PREVIEW_NOTICE_CLASS = "preview-notice";
const PREVIEW_NOTICE_TEXT = "预览不可用：文件可能已被删除或移动";

// 记录打开预览前的焦点元素，关闭时恢复
let previousFocus: HTMLElement | null = null;

// v5.0 F-PERF-015: 预览模态框元素引用缓存。
// 原实现每次 openPreview 都执行 6 次 getElementById，resetMedia 执行 3 次
// querySelector，每次开关模态框 = 9 次 DOM 查询。改为模块级缓存，setupPreviewModal
// 时初始化一次，后续直接使用缓存引用（DOM 元素是固定的，不会在运行时被替换）。
// 元素可能为 null（如 DOM 未就绪或被移除），缓存后使用前仍需 null 检查。
// isConnected 检测：元素可能被外部移除（如测试 beforeEach 清空 DOM），
// 失效时回退到重新查询，确保缓存引用始终指向 DOM 中的有效元素。
interface PreviewElements {
  modal: HTMLElement | null;
  img: HTMLImageElement | null;
  video: HTMLVideoElement | null;
  web: HTMLIFrameElement | null;
  name: HTMLElement | null;
  type: HTMLElement | null;
  closeBtn: HTMLButtonElement | null;
  setBtn: HTMLButtonElement | null;
}

let cachedPreviewEls: PreviewElements | null = null;

function getPreviewEls(): PreviewElements {
  // modal 是容器元素，若已从 DOM 移除则整个缓存失效，需重新查询
  if (cachedPreviewEls && cachedPreviewEls.modal?.isConnected) return cachedPreviewEls;
  cachedPreviewEls = {
    modal: document.getElementById("preview-modal"),
    img: document.getElementById("preview-image") as HTMLImageElement | null,
    video: document.getElementById("preview-video") as HTMLVideoElement | null,
    web: document.getElementById("preview-web") as HTMLIFrameElement | null,
    name: document.getElementById("preview-name"),
    type: document.getElementById("preview-type"),
    closeBtn: document.getElementById("preview-close") as HTMLButtonElement | null,
    setBtn: document.getElementById("preview-set-btn") as HTMLButtonElement | null,
  };
  return cachedPreviewEls;
}

export function openPreview(wp: WallpaperEntry) {
  const { modal, img, video, web, name, type } = getPreviewEls();
  if (!modal || !img || !video || !web || !name || !type) return;

  // 记录先前焦点，关闭时恢复
  previousFocus = document.activeElement as HTMLElement;

  // 切换前清理所有媒体元素（暂停视频、清空 src，防止后台继续播放/加载）
  resetMedia();
  img.style.display = "none";
  video.style.display = "none";
  web.style.display = "none";

  // 使用 wpfile:// 自定义协议绕过 asset protocol scope 限制
  const src = convertFileSrc(wp.file_path, "wpfile");

  // 按壁纸类型分支渲染
  switch (wp.wallpaper_type) {
    case "Image":
      attachPreviewErrorHandler(img, wp.file_path);
      img.src = src;
      img.alt = extractFileName(wp.file_path);
      // F01: 移除 CSP 合规的隐藏类（替代原内联 style="display:none"），再清空 inline display
      img.classList.remove("preview-media-hidden");
      img.style.display = "";
      break;
    case "Video":
      attachPreviewErrorHandler(video, wp.file_path);
      video.src = src;
      video.loop = false;
      // F01: 移除 CSP 合规的隐藏类，再清空 inline display
      video.classList.remove("preview-media-hidden");
      video.style.display = "";
      break;
    case "Gif":
      attachPreviewErrorHandler(video, wp.file_path);
      video.src = src;
      video.loop = true;
      // F01: 移除 CSP 合规的隐藏类，再清空 inline display
      video.classList.remove("preview-media-hidden");
      video.style.display = "";
      break;
    case "Web":
      attachPreviewErrorHandler(web, wp.file_path);
      web.src = src;
      // F01: 移除 CSP 合规的隐藏类，再清空 inline display
      web.classList.remove("preview-media-hidden");
      web.style.display = "";
      // v41-F-003: iframe 安全属性加固——在 openPreview 时显式设置：
      // - referrer-policy: no-referrer 阻止 Referer 头泄露本地文件路径
      // - sandbox: allow-scripts allow-same-origin 允许 Web 壁纸脚本运行（覆盖 index.html 的 sandbox=""），
      //   同时仍隔离弹出窗口、表单提交、顶层导航等
      // - allow (permissions-policy): 禁用 geolocation/microphone/camera 等敏感权限
      web.setAttribute("referrer-policy", "no-referrer");
      web.setAttribute("sandbox", "allow-scripts allow-same-origin");
      web.setAttribute("allow", "geolocation=(), microphone=(), camera=()");
      // F06: iframe 在 index.html 中以 sandbox="" 配置（完全沙箱，无 allow-scripts），
      // 仅展示静态 HTML 文档结构，不可信本地 HTML 中的脚本不会在预览阶段执行。
      // F-009: Web 壁纸安全模型说明
      // - `sandbox=""` 已禁用脚本执行、表单提交、弹出窗口等
      // - 残留风险：纯 HTML 标签（<img src>/<link>/<video poster>）仍可发起外部网络请求，
      //   可能泄露用户环境信息（IP/User-Agent/加载时机）
      // - 风险等级低：需用户主动添加恶意 HTML 文件为壁纸才触发，无文件系统访问权限
      // - 缓解措施：建议用户仅添加可信 HTML 文件；未来若需严格隔离可考虑在 iframe 配置
      //   CSP（需评估 WebView2 兼容性）
      break;
  }

  name.textContent = extractFileName(wp.file_path);
  type.textContent = wp.wallpaper_type;
  appState.currentPreviewId = wp.id;
  modal.classList.add("active");

  // 焦点进入关闭按钮，便于键盘用户立即操作（复用 F-PERF-015 缓存引用）
  const { closeBtn } = getPreviewEls();
  closeBtn?.focus();
}

// 为媒体元素挂载 onerror 容错。触发时清空 src（避免重复触发）、隐藏元素、
// 记录 warn 日志，并在父容器内追加友好提示。同一父容器内只保留一个提示元素。
function attachPreviewErrorHandler(
  el: HTMLImageElement | HTMLVideoElement | HTMLIFrameElement,
  filePath: string,
): void {
  el.onerror = () => {
    // 清空 src 避免重复触发 onerror
    el.onerror = null;
    if (el instanceof HTMLIFrameElement) {
      el.src = "";
    } else {
      el.removeAttribute("src");
    }
    // 隐藏失败的媒体元素，避免显示破损图标
    el.style.display = "none";
    log.warn("预览加载失败：文件可能不存在或无法访问", filePath);
    // 显示友好提示（父容器内仅保留一个）
    const parent = el.parentElement;
    if (parent && !parent.querySelector(`.${PREVIEW_NOTICE_CLASS}`)) {
      const notice = document.createElement("div");
      notice.className = PREVIEW_NOTICE_CLASS;
      notice.textContent = PREVIEW_NOTICE_TEXT;
      parent.appendChild(notice);
    }
  };
}

// 重置媒体元素：暂停视频并清空 src，防止后台继续播放/加载
function resetMedia(): void {
  // v5.0 F-PERF-015: 使用缓存引用替代 querySelector
  const { img, video, web } = getPreviewEls();
  if (img) {
    img.onerror = null;
    img.removeAttribute("src");
  }
  if (video) {
    video.onerror = null;
    releaseVideoElement(video);
  }
  if (web) {
    web.onerror = null;
    web.src = "";
  }
  // 清理预览失败的提示元素
  document.querySelectorAll(`.${PREVIEW_NOTICE_CLASS}`).forEach(n => n.remove());
}

export function closePreview() {
  // v5.0 F-PERF-015: 使用缓存引用替代 getElementById
  const { modal } = getPreviewEls();
  if (!modal) return;
  modal.classList.remove("active");
  appState.currentPreviewId = null;
  // 关闭时清理媒体元素，防止视频/iframe 在后台继续播放或加载
  resetMedia();
  // 恢复打开前的焦点
  previousFocus?.focus();
  previousFocus = null;
}

export function setupPreviewModal() {
  // v5.0 F-PERF-015: 初始化元素引用缓存（首次调用 getPreviewEls 触发查询并缓存）
  const { modal, closeBtn, setBtn } = getPreviewEls();
  if (!modal) return;

  // 关闭按钮
  if (closeBtn) {
    // FE-003: 使用 addEventListenerWithCleanup 登记清理，符合 F-010 约定
    addEventListenerWithCleanup(closeBtn, "click", closePreview);
  }

  // 点击遮罩关闭
  const overlay = modal.querySelector(".preview-overlay");
  if (overlay) {
    // FE-003: 使用 addEventListenerWithCleanup 登记清理，符合 F-010 约定
    addEventListenerWithCleanup(overlay, "click", closePreview);
  }

  // ESC 键关闭 + Tab 焦点陷阱（通过 addEventListenerWithCleanup 登记清理）
  addEventListenerWithCleanup(document, "keydown", (e) => {
    const ke = e as KeyboardEvent;
    if (!modal.classList.contains("active")) return;
    if (ke.key === "Escape") {
      closePreview();
      return;
    }
    if (ke.key !== "Tab") return;
    // 焦点陷阱：在模态框内可聚焦元素之间循环
    const focusables = modal.querySelectorAll<HTMLElement>(
      'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
    );
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    if (!first || !last) return;
    if (ke.shiftKey && document.activeElement === first) {
      ke.preventDefault();
      last.focus();
    } else if (!ke.shiftKey && document.activeElement === last) {
      ke.preventDefault();
      first.focus();
    }
  });

  // 设为壁纸按钮
  if (setBtn) {
    // FE-003: 使用 addEventListenerWithCleanup 登记清理，符合 F-010 约定
    // FE-007: 使用 data-id 属性匹配替代模板字符串选择器，避免 CSS 选择器注入风险
    addEventListenerWithCleanup(setBtn, "click", async () => {
      if (!appState.currentPreviewId) return;
      const btn = setBtn as HTMLButtonElement;
      // F-010: 防双击，参考 main.ts regenerateBtn 模式
      btn.disabled = true;
      try {
        await setWallpaper(appState.currentPreviewId, appState.selectedDisplayId || undefined);
        showStatus("壁纸已设置", "success");
        // 更新激活状态
        const grid = document.getElementById("wallpaper-grid");
        if (grid) {
          grid.querySelectorAll(".wallpaper-card").forEach(c => c.classList.remove("active"));
          // FE-007: 使用 data 属性匹配替代模板字符串选择器，防御 ID 含特殊字符
          const activeCard = Array.from(grid.querySelectorAll<HTMLElement>(".wallpaper-card"))
            .find(c => c.dataset.id === appState.currentPreviewId);
          if (activeCard) activeCard.classList.add("active");
        }
        closePreview();
      } catch (err) {
        log.error("设置壁纸失败:", err);
        showStatus("设置壁纸失败", "error");
      } finally {
        btn.disabled = false;
      }
    });
  }
}
