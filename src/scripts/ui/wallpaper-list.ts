import { convertFileSrc } from "@tauri-apps/api/core";
import { getWallpapers, removeWallpaper } from "../ipc";
import { appState } from "../state";
import type { WallpaperEntry } from "../types";
import { log } from "../utils/logger";
import { extractFileName, releaseVideoElement, showStatus, typeIcon } from "./utils";
import { openPreview } from "./preview-modal";

// ── Wallpaper List Rendering ─────────────────────────────────────────────────

// F12: 模块级请求序号——丢弃过期响应，避免竞态（如快速连续刷新时旧响应覆盖新响应）。
// 与 main.ts 中的 150ms 防抖配合：防抖减少请求频率，序号取消丢弃过期响应。
let refreshSeq = 0;

/**
 * F-007: 骨架屏数量，与 CSS 网格列数对齐，填充首屏可见区域。
 *
 * 数值选择理由（v41-F-007 文档化）：
 * - 取 8 是基于常见显示器分辨率（1920×1080 / 2560×1440）下首屏可见卡片数的近似上限估算
 *   （按当前 CSS 网格每行 4 列 × 2 行可见区域估算，预留滚动缓冲）
 * - 太少会暴露空白闪烁，太多会延长首次 paint 时间（DOM 节点数线性增加）
 * - 若 CSS 网格列数变更（如响应式断点调整），需同步评估此值
 *
 * 注意：本常量为 `const`（不可变），如需运行时动态调整（如多分辨率自适应），
 * 应改为根据 `window.innerWidth` 计算的函数。
 */
const SKELETON_COUNT = 8;

// F05: 模块级懒渲染 observer——重新渲染前 disconnect 旧的，避免持有已移除 DOM 引用导致内存泄漏。
let lazyObserver: IntersectionObserver | null = null;

// F05: 卡片元素 → 壁纸数据的映射，供 observer 回调查询。WeakMap 在卡片被 GC 时自动释放条目。
const cardData = new WeakMap<HTMLElement, WallpaperEntry>();

// v5.0 F-PERF-004: file_path → 小写文件名 的缓存，避免搜索过滤时对同一 file_path
// 重复执行 extractFileName（含正则 split）+ toLowerCase（含字符串分配）。
// 在 refreshWallpaperList 全量赋值时预填充，appendWallpaperCard 追加时增量填充，
// 搜索过滤时直接查 Map，消除 O(N) 重复字符串操作。
const lowercasedFileNameCache = new Map<string, string>();

/**
 * 获取 file_path 对应的小写文件名（带缓存）。
 * 缓存未命中时执行 extractFileName + toLowerCase 并写入缓存。
 *
 * v5.0 F-PERF-004: 导出供 main.ts 的 debouncedSearch 复用缓存，
 * 避免搜索框每次键入都重新计算所有壁纸的文件名小写。
 */
export function getLowercasedFileName(filePath: string): string {
  let cached = lowercasedFileNameCache.get(filePath);
  if (cached === undefined) {
    cached = extractFileName(filePath).toLowerCase();
    lowercasedFileNameCache.set(filePath, cached);
  }
  return cached;
}

/**
 * v12.0: 仅供测试使用的缓存大小查询。生产代码不应调用。
 * 返回 lowercasedFileNameCache 当前条目数，用于断言缓存清理行为
 * （removeWallpaperCard 的 delete 与 refreshWallpaperList 的 clear）。
 */
export function __getCacheSizeForTests(): number {
  return lowercasedFileNameCache.size;
}

/**
 * 批量预填充 file_path → 小写文件名 缓存。供 refreshWallpaperList 全量赋值时调用。
 * 相比逐个 getLowercasedFileName 命中检查，直接遍历预填充更高效（无 Map 查找开销）。
 */
function prefetchFileNameCache(wallpapers: WallpaperEntry[]): void {
  for (const wp of wallpapers) {
    if (!lowercasedFileNameCache.has(wp.file_path)) {
      lowercasedFileNameCache.set(wp.file_path, extractFileName(wp.file_path).toLowerCase());
    }
  }
}

// v5.0 F-PERF-003: 卡片结构模板，cloneNode 比 23 次 DOM API 调用快 2-5x。
// 模块级单例，所有卡片共享同一模板，仅 cloneNode 生成新 DOM 子树。
const cardTemplate: HTMLTemplateElement = document.createElement("template");
cardTemplate.innerHTML = `
  <div class="wallpaper-card" tabindex="0" role="button">
    <div class="wallpaper-card-thumb"></div>
    <div class="wallpaper-card-info">
      <div class="wallpaper-card-name"></div>
      <div class="wallpaper-card-footer">
        <span class="type-badge"></span>
        <button class="wallpaper-card-delete" title="删除" aria-label="删除壁纸">✕</button>
      </div>
    </div>
  </div>
`;

export async function refreshWallpaperList() {
  // F12: 入口递增序号，捕获当前请求的序号用于响应比对
  const mySeq = ++refreshSeq;

  // 功能5: 加载开始时显示骨架屏占位
  // v5.0 F-PERF-012: 使用 replaceChildren 一次性替换（与 renderWallpaperList 一致），
  // 替代原 innerHTML="" + 逐个 appendChild（8 次重排）。骨架屏为简单 div，
  // 单次 replaceChildren 性能更优且语义统一。
  const grid = document.getElementById("wallpaper-grid");
  if (grid) {
    const skeletons = Array.from({ length: SKELETON_COUNT }, () => {
      const skeleton = document.createElement("div");
      skeleton.className = "skeleton-card";
      return skeleton;
    });
    grid.replaceChildren(...skeletons);
  }

  try {
    const wallpapers = await getWallpapers();
    // F12: 丢弃过期响应——若期间发起了新的 refresh，mySeq 已过期，直接返回不渲染
    if (mySeq !== refreshSeq) return;
    // F-005: 移除主动缩略图生成，改由后端在 add_wallpaper 内生成后
    // 通过 wallpaper-updated 事件触发 debouncedRefresh 刷新列表
    // 功能2: 缓存全部壁纸
    appState.allWallpapers = wallpapers;
    // v12.0: 全量刷新前先 clear()，确保缓存与 allWallpapers 严格同步
    // （覆盖 file_path 重命名场景：旧 file_path 不残留）
    lowercasedFileNameCache.clear();
    // v5.0 F-PERF-004: 全量赋值时预填充 file_path → 小写文件名 缓存，
    // 后续搜索过滤直接查 Map，消除重复 extractFileName + toLowerCase
    prefetchFileNameCache(wallpapers);
    // 功能2: 若搜索框有关键词，按关键词过滤后渲染
    const keyword = getCurrentSearchKeyword();
    if (keyword) {
      const filtered = wallpapers.filter(wp =>
        getLowercasedFileName(wp.file_path).includes(keyword)
      );
      renderWallpaperList(filtered);
    } else {
      renderWallpaperList(wallpapers);
    }
  } catch (e) {
    // F12: 错误响应也需检查序号，避免过期错误覆盖新响应的成功结果
    if (mySeq !== refreshSeq) return;
    log.error("加载壁纸列表失败:", e);
    // FE-008: 失败时清空骨架屏，避免永久残留加载动画
    if (grid) {
      grid.innerHTML = `<p class="empty-hint">加载壁纸列表失败，请检查后端服务</p>`;
    }
    showStatus("加载壁纸列表失败", "error");
  }
}

/**
 * 缩略图加载失败时的统一回退：清空容器并显示类型图标。
 * 用于 <img> onerror 与 <video> onerror，避免 DRY 违规。
 * 注意：textContent 赋值会自动移除所有子节点。
 */
function fallbackToEmoji(thumb: HTMLElement, wp: WallpaperEntry): void {
  thumb.textContent = typeIcon(wp.wallpaper_type);
  thumb.classList.add("wallpaper-thumb-error");
}

/**
 * 孤儿壁纸条目占位：在缩略图容器显示「源文件缺失」文本。
 * 用于 wallpaper-source-missing 事件（源文件被删/移动但库中仍保留），
 * 区分于 fallbackToEmoji（生成失败→类型图标）。字号下调避免 28px emoji 尺寸溢出。
 */
function fillSourceMissingPlaceholder(thumb: HTMLElement): void {
  thumb.replaceChildren();
  thumb.classList.remove("wallpaper-thumb-error");
  thumb.classList.add("wallpaper-source-missing");
  thumb.textContent = "源文件缺失";
}

/**
 * wallpaper-source-missing 事件处理：将对应卡片标记为「源文件缺失」孤儿条目。
 * 温和处理（非 error），不强制全量刷新，避免打断搜索/滚动（与 wallpaper-thumbnail-failed
 * 行为一致）。通过 dataset 标记持久占位，即使卡片尚未 hydrate 也能在 hydrate 时保留占位，
 * 不被 fallbackToEmoji 覆盖。
 */
export function markSourceMissingCard(id: string): void {
  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;
  const card = grid.querySelector<HTMLElement>(
    `.wallpaper-card[data-id="${CSS.escape(id)}"]`,
  );
  if (!card) return;
  card.dataset.sourceMissing = "1";
  const thumb = card.querySelector<HTMLElement>(".wallpaper-card-thumb");
  if (thumb) fillSourceMissingPlaceholder(thumb);
}

/**
 * F05: 创建壁纸卡片的轻量外壳。
 *
 * 仅渲染便宜的结构（容器、文字、badge、删除按钮壳），不设置 img/video.src、
 * 不绑定 click/keydown 事件。重活延迟到 hydrateCard 中由 IntersectionObserver 触发。
 *
 * v5.0 F-PERF-003: 改用 template.cloneNode 替代逐个 createElement，
 * 约 23 次 DOM API 调用降至 ~6 次（cloneNode + 3 次 querySelector + 4 次属性赋值）。
 */
function createCardShell(wp: WallpaperEntry): HTMLDivElement {
  // v5.0 F-PERF-003: 使用 template.cloneNode 替代逐个 createElement
  const card = cardTemplate.content.firstElementChild!.cloneNode(true) as HTMLDivElement;
  const name = card.querySelector<HTMLElement>(".wallpaper-card-name")!;
  const badge = card.querySelector<HTMLElement>(".type-badge")!;

  card.dataset.id = wp.id;
  // F-004: 保留原 aria-label 格式（a11y 测试依赖 "预览壁纸 <文件名>"）
  card.setAttribute("aria-label", `预览壁纸 ${extractFileName(wp.file_path)}`);
  name.textContent = extractFileName(wp.file_path);
  name.title = wp.file_path;
  badge.textContent = wp.wallpaper_type;
  badge.className = `type-badge ${wp.wallpaper_type.toLowerCase()}`;

  // thumb 内容由 fillThumbContent 在 hydrateCard 中填充
  return card;
}

/**
 * F05: 为缩略图容器填充图片/视频内容（设置 src、绑定 onload/onerror）。
 */
function fillThumbContent(thumb: HTMLElement, wp: WallpaperEntry): void {
  if (wp.thumbnail) {
    const img = document.createElement("img");
    // 使用 wpfile:// 自定义协议绕过 asset protocol scope 限制
    img.src = convertFileSrc(wp.thumbnail, "wpfile");
    img.alt = extractFileName(wp.file_path);
    img.loading = "lazy";
    // 功能5: 缩略图渐显
    img.className = "wallpaper-thumb";
    img.onload = () => img.classList.add("loaded");
    // 加载失败时回退到类型图标，避免 opacity:0 永久空白
    img.onerror = () => fallbackToEmoji(thumb, wp);
    thumb.appendChild(img);
  } else if (wp.wallpaper_type === "Video") {
    // 视频类型无缩略图：直接用 <video> 抓首帧渲染，避免仅有 emoji 的"空白"观感
    const video = document.createElement("video");
    // 使用 wpfile:// 自定义协议绕过 asset protocol scope 限制
    video.src = convertFileSrc(wp.file_path, "wpfile");
    video.preload = "metadata";
    video.muted = true;
    video.playsInline = true;
    video.setAttribute("disablepictureinpicture", "");
    video.className = "wallpaper-thumb";
    video.onloadedmetadata = () => {
      // 跳到 0.1s 触发首帧解码（部分浏览器 0 帧为黑帧）
      video.currentTime = 0.1;
    };
    video.onloadeddata = () => {
      // 首帧就绪后暂停，避免持续播放消耗资源；触发渐显
      video.pause();
      video.classList.add("loaded");
    };
    video.onerror = () => fallbackToEmoji(thumb, wp);
    thumb.appendChild(video);
  } else if (wp.wallpaper_type === "Image" || wp.wallpaper_type === "Gif") {
    // 无缩略图：直接加载源图片文件，与 Video 的 <video> 首帧方案对称
    const img = document.createElement("img");
    // 使用 wpfile:// 自定义协议绕过 asset protocol scope 限制
    img.src = convertFileSrc(wp.file_path, "wpfile");
    img.alt = extractFileName(wp.file_path);
    img.loading = "lazy";
    img.className = "wallpaper-thumb";
    img.onload = () => img.classList.add("loaded");
    img.onerror = () => fallbackToEmoji(thumb, wp);
    thumb.appendChild(img);
  } else {
    thumb.textContent = typeIcon(wp.wallpaper_type);
  }
}

/**
 * F05: 卡片进入视口时填充完整内容——设置 img/video.src。
 *
 * 防重复 hydrate：通过 dataset.hydrated 标记，observer 可能多次回调时仅首次生效。
 *
 * v5.0 F-PERF-001: 移除 4 个 addEventListener + performDelete 闭包，
 * 事件委托由 grid 容器的 2 个固定监听器处理（attachDelegatedListeners）。
 * 100 张壁纸：400 监听器 + 100 闭包 → 2 固定监听器。
 */
function hydrateCard(card: HTMLElement, wp: WallpaperEntry): void {
  if (card.dataset.hydrated === "1") return;
  card.dataset.hydrated = "1";

  const thumb = card.querySelector<HTMLElement>(".wallpaper-card-thumb");
  if (thumb) {
    // 孤儿壁纸条目：保留「源文件缺失」占位，避免 hydrate 时被 fallbackToEmoji 覆盖
    if (card.dataset.sourceMissing === "1") {
      fillSourceMissingPlaceholder(thumb);
    } else {
      fillThumbContent(thumb, wp);
    }
  }
  // v5.0 F-PERF-001: 同步 cardData（防御性——renderWallpaperList 已设置，
  // 此处确保直接调用 hydrateCard 时 cardData 也最新）
  cardData.set(card, wp);
}

// ── v5.0 F-PERF-001: 事件委托 ─────────────────────────────────────────────────
// 从 hydrateCard 闭包提取为按 id 查找的独立函数，供 grid 容器的 2 个固定监听器调用。
// 4N 监听器 + N 闭包 → 2 固定监听器，DOM 节点数无关。

/**
 * v5.0 F-PERF-001: 按 id 执行删除（从原 hydrateCard 的 performDelete 闭包提取）。
 * 保留完整逻辑：确认对话框 + IPC 调用 + 状态提示 + 错误处理。
 */
async function performDeleteById(id: string): Promise<void> {
  const wallpaper = appState.allWallpapers.find(w => w.id === id);
  if (!wallpaper) return;
  // FE-006: 复用 utils.ts 的 extractFileName，消除 DRY 违规
  const fileName = extractFileName(wallpaper.file_path);
  if (!confirm(`确认删除壁纸 "${fileName}"？\n\n壁纸文件将一并删除。`)) {
    return;
  }
  try {
    await removeWallpaper(wallpaper.id, true);
    showStatus("壁纸已删除", "success");
  } catch (err) {
    log.error("删除壁纸失败:", err);
    showStatus("删除壁纸失败", "error");
  }
}

/**
 * v5.0 F-PERF-001: 卡片点击处理（从原 hydrateCard 的 card click 监听器提取）。
 * 通过 cardData WeakMap 查找壁纸数据（O(1)，由 renderWallpaperList/appendWallpaperCard 维护）。
 */
function handleCardClick(card: HTMLElement): void {
  const wallpaper = cardData.get(card);
  if (!wallpaper) return;
  // 功能1: 点击卡片打开预览模态框
  openPreview(wallpaper);
}

/**
 * v5.0 F-PERF-001: 卡片键盘处理（从原 hydrateCard 的 card keydown 监听器提取）。
 * 保留原逻辑：Enter/Space 触发预览（preventDefault 避免 Space 滚动），Delete 触发删除。
 */
function handleCardKeydown(event: KeyboardEvent, card: HTMLElement): void {
  const wallpaper = cardData.get(card);
  if (!wallpaper) return;
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    openPreview(wallpaper);
  } else if (event.key === "Delete") {
    event.preventDefault();
    void performDeleteById(card.dataset.id ?? "");
  }
}

/**
 * v5.0 F-PERF-001: 已注册委托监听器的 grid 元素。
 * 用元素引用（而非布尔标志）跟踪：测试中每个 beforeEach 创建新 grid，
 * 元素比对可正确对新 grid 重新注册；同一 grid 仅注册一次。
 */
let delegatedGrid: HTMLElement | null = null;

/**
 * v5.0 F-PERF-001: 在 grid 容器注册 click + keydown 委托监听器（仅一次/grid）。
 *
 * - click：deleteBtn 命中 → stopPropagation + 删除；否则卡片 → 预览。
 * - keydown：deleteBtn + Delete → stopPropagation + 删除（v41-F-008 防重复触发）；
 *   否则卡片 → Enter/Space 预览、Delete 删除。
 *
 * 保留原 stopPropagation 语义：deleteBtn 的 click/keydown 阻止冒泡到 card 逻辑。
 */
function attachDelegatedListeners(grid: HTMLElement): void {
  if (delegatedGrid === grid) return;
  delegatedGrid = grid;

  grid.addEventListener("click", (event: MouseEvent) => {
    const target = event.target as HTMLElement;
    const deleteBtn = target.closest<HTMLElement>(".wallpaper-card-delete");
    if (deleteBtn) {
      // 原 deleteBtn click：stopPropagation + 删除（不触发 card 的预览）
      event.stopPropagation();
      const card = deleteBtn.closest<HTMLElement>(".wallpaper-card");
      if (card?.dataset.id) {
        void performDeleteById(card.dataset.id);
      }
      return;
    }
    const card = target.closest<HTMLElement>(".wallpaper-card");
    if (card?.dataset.id) {
      handleCardClick(card);
    }
  });

  grid.addEventListener("keydown", (event: KeyboardEvent) => {
    const target = event.target as HTMLElement;
    const deleteBtn = target.closest<HTMLElement>(".wallpaper-card-delete");
    const card = target.closest<HTMLElement>(".wallpaper-card");

    // v41-F-008: deleteBtn 上的 Delete 键独立处理，原 stopPropagation 阻止冒泡到 card
    if (deleteBtn && event.key === "Delete") {
      event.preventDefault();
      event.stopPropagation();
      if (card?.dataset.id) {
        void performDeleteById(card.dataset.id);
      }
      return;
    }

    // 原 card keydown 逻辑（deleteBtn 非 Delete 键也冒泡到此，保留原行为）
    if (card?.dataset.id) {
      handleCardKeydown(event, card);
    }
  });
}

/**
 * F05: 创建懒渲染 IntersectionObserver。
 *
 * rootMargin 提前 200px 触发，避免滚动到卡片时才加载产生闪烁。
 *
 * v5.0 A-PERF-001: 从 renderWallpaperList 提取为独立函数，供 appendWallpaperCard 复用，
 * 确保增量追加的卡片与全量渲染的卡片使用相同的懒渲染行为。
 *
 * 每次渲染创建新 observer（测试 mock 重置需求，不复用单例）。
 */
function createLazyObserver(): IntersectionObserver {
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          const card = entry.target as HTMLElement;
          const wp = cardData.get(card);
          if (wp) hydrateCard(card, wp);
          observer.unobserve(card);
        }
      }
    },
    { rootMargin: "200px" },
  );
  return observer;
}

/**
 * 渲染空状态提示（无壁纸 / 无搜索结果）。
 * 消除 renderWallpaperList 与 updateEmptyState 中的空状态 HTML 重复。
 */
function renderEmptyState(grid: HTMLElement, mode: "no-wallpaper" | "no-match"): void {
  if (mode === "no-match") {
    grid.innerHTML = `<p class="empty-hint">未找到匹配的壁纸</p>`;
  } else {
    // v41-F-009: 移除内联 style，改用 main.css 的 .wallpaper-list-empty-hint 类，避免违反 CSP
    grid.innerHTML = `
        <div class="wallpaper-list-empty">
            <div class="empty-icon">🖼</div>
            <p>拖拽图片到此处添加壁纸</p>
            <p class="wallpaper-list-empty-hint">支持 JPG、PNG、BMP、WebP 格式</p>
        </div>
    `;
  }
}

export function renderWallpaperList(wallpapers: WallpaperEntry[]) {
  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;

  // v5.0 F-PERF-001: 注册委托监听器（仅一次/grid）。grid 是固定 DOM 元素，
  // 监听器持久有效，后续 appendWallpaperCard 追加的卡片也自动覆盖。
  attachDelegatedListeners(grid);

  // F05: 重新渲染前 disconnect 旧 observer 的观察列表，防止内存泄漏
  if (lazyObserver) {
    lazyObserver.disconnect();
  }
  // 置空引用：空库提前 return 分支不再残留已 disconnect 的 observer，
  // 后续 appendWallpaperCard 首次拖入时才能重新创建 observer（否则 observe no-op 导致缩略图空白）
  lazyObserver = null;

  // F-006: 重渲染前显式释放已加载 <video> 的解码资源，避免 innerHTML="" 后浏览器仍持有解码器
  for (const video of grid.querySelectorAll<HTMLVideoElement>("video.wallpaper-thumb")) {
    video.pause();
    video.removeAttribute("src");
    video.load();
  }

  // v41-F-006: 使用 replaceChildren() 替代 innerHTML="" 清空子节点，语义更明确且避免 HTML 解析开销
  grid.replaceChildren();

  if (wallpapers.length === 0) {
    // 功能2: 区分无壁纸和无搜索结果
    renderEmptyState(grid, appState.allWallpapers.length > 0 ? "no-match" : "no-wallpaper");
    return;
  }

  // F05: 每次渲染创建新 observer（测试 mock 重置需求）
  lazyObserver = createLazyObserver();
  const observer = lazyObserver;

  const fragment = document.createDocumentFragment();
  for (const wp of wallpapers) {
    const card = createCardShell(wp);
    cardData.set(card, wp);
    fragment.appendChild(card);
  }
  // F-008: 在 append 前捕获 children 引用，因 appendChild(fragment) 后 fragment.children 会被清空
  const cards = Array.from(fragment.children) as HTMLElement[];
  grid.appendChild(fragment);
  // F-008: 卡片插入 DOM 后再 observe，保持懒渲染语义
  for (const card of cards) {
    observer.observe(card);
  }
}

// ── v5.0 A-PERF-001: 增量事件处理 ─────────────────────────────────────────────
// 以下三个导出函数供 main.ts 事件监听器调用，替代原防抖全量刷新。
// 设计目标：单张壁纸变更时仅更新对应 DOM 卡片，避免全量 IPC 重拉 + 全量 DOM 重建。
// 保留 refreshWallpaperList 作为 fallback（搜索状态、payload 不完整等场景）。

/**
 * 获取当前搜索关键词（小写、去空格）。无搜索框或空串时返回空串。
 * refreshWallpaperList 与增量更新函数共用此逻辑。
 */
function getCurrentSearchKeyword(): string {
  const searchInput = document.getElementById("wallpaper-search") as HTMLInputElement | null;
  return searchInput ? searchInput.value.trim().toLowerCase() : "";
}

/**
 * 判断壁纸是否匹配搜索关键词（按文件名子串匹配）。
 * 与 refreshWallpaperList / main.ts 中的过滤逻辑保持一致。
 *
 * v5.0 F-PERF-004: 使用 getLowercasedFileName 缓存，消除重复字符串操作。
 */
function matchesSearch(wallpaper: WallpaperEntry, keyword: string): boolean {
  return getLowercasedFileName(wallpaper.file_path).includes(keyword);
}

/**
 * 更新 grid 的空状态显示。
 * 当 grid 中无壁纸卡片时，根据 appState.allWallpapers 显示对应提示：
 * - allWallpapers 非空但无匹配：显示"未找到匹配的壁纸"
 * - allWallpapers 为空：显示"拖拽图片到此处添加壁纸"
 * 当 grid 中有卡片时，不做任何操作。
 */
function updateEmptyState(): void {
  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;
  if (grid.querySelector(".wallpaper-card")) return;
  grid.replaceChildren();
  renderEmptyState(grid, appState.allWallpapers.length > 0 ? "no-match" : "no-wallpaper");
}

/**
 * v5.0 A-PERF-001: 增量添加单张壁纸卡片。
 *
 * 更新 appState.allWallpapers + 在 grid 末尾追加单张卡片。
 * 若当前有搜索过滤且壁纸不匹配关键词，仅更新 appState 不追加 DOM。
 * 若 grid 当前显示空状态提示，先清空再追加。
 */
export function appendWallpaperCard(wallpaper: WallpaperEntry): void {
  // 1. 更新 appState。
  // 竞态修复：若 updateWallpaperCard 已抢先用带缩略图的数据 upsert（wallpaper-updated
  // 早于本函数到达、且此时已在 grid 渲染之前入库），则复用该最新条目（不 push，
  // 避免重复卡片，也避免用 wallpaper-added 的过期空缩略图覆盖）；否则按传入数据 push。
  let effective = wallpaper;
  const existingIdx = appState.allWallpapers.findIndex(w => w.id === wallpaper.id);
  if (existingIdx === -1) {
    appState.allWallpapers.push(wallpaper);
  } else {
    effective = appState.allWallpapers[existingIdx]!;
  }
  // v5.0 F-PERF-004: 增量填充 file_path → 小写文件名 缓存
  prefetchFileNameCache([effective]);

  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;

  // 2. 检查搜索过滤：不匹配时仅更新 appState，不追加 DOM
  const keyword = getCurrentSearchKeyword();
  if (keyword && !matchesSearch(effective, keyword)) {
    // 若 grid 当前显示"无壁纸"空状态但 appState 已有条目，更新为"未找到匹配的壁纸"
    updateEmptyState();
    return;
  }

  // 3. 若 grid 当前显示空状态（无卡片），清空空状态提示
  if (!grid.querySelector(".wallpaper-card")) {
    grid.replaceChildren();
  }

  // 4. 创建单张卡片并追加到 grid 末尾
  const card = createCardShell(effective);
  cardData.set(card, effective);
  grid.appendChild(card);

  // 5. IntersectionObserver 监听新卡片（复用 renderWallpaperList 创建的 observer）
  // 空库首次拖入时 renderWallpaperList 未创建 observer（lazyObserver 为 null），此处补建，保证卡片被 hydrate
  if (lazyObserver === null) {
    lazyObserver = createLazyObserver();
  }
  lazyObserver.observe(card);
}

/**
 * v5.0 A-PERF-001: 增量移除单张壁纸卡片。
 *
 * 从 appState.allWallpapers 移除 + 按 data-id 移除 DOM 卡片。
 * 若卡片不在 DOM 中（如被搜索过滤），仅更新 appState（no-op for DOM）。
 * 释放 video 元素解码资源（pause + 清除 src + load），避免内存泄漏（参照 v4.0 F-006）。
 */
export function removeWallpaperCard(id: string): void {
  // 1. 从 appState 移除
  const idx = appState.allWallpapers.findIndex(w => w.id === id);
  if (idx === -1) return;
  // v12.0: 先捕获 wallpaper 引用，splice 后再清理缓存条目
  const wallpaper = appState.allWallpapers[idx]!;
  appState.allWallpapers.splice(idx, 1);
  // v12.0: 清理 lowercasedFileNameCache 中的过期条目，避免长期增删壁纸后缓存累积
  lowercasedFileNameCache.delete(wallpaper.file_path);

  // 2. 从 DOM 移除卡片
  const grid = document.getElementById("wallpaper-grid");
  if (grid) {
    const card = grid.querySelector<HTMLElement>(
      `.wallpaper-card[data-id="${CSS.escape(id)}"]`,
    );
    if (card) {
      lazyObserver?.unobserve(card);
      // F-006: 释放 video 元素解码资源，避免 innerHTML 清空后浏览器仍持有解码器
      const video = card.querySelector<HTMLVideoElement>("video.wallpaper-thumb");
      if (video) {
        releaseVideoElement(video);
      }
      card.remove();
    }
  }

  // 3. 更新空状态显示（若移除后 grid 无卡片）
  updateEmptyState();
}

/**
 * v5.0 A-PERF-001: 增量更新单张壁纸卡片（如缩略图生成完成）。
 *
 * 更新 appState + 定向更新卡片的缩略图区域。
 * 若卡片未 hydrate（未进入视口），仅更新 cardData（WeakMap），hydrate 时读取最新数据。
 * 若卡片已 hydrate，清空旧缩略图内容并重新填充。
 *
 * 搜索状态下 fallback 到全量刷新（文件名可能变化导致匹配状态改变）。
 */
export function updateWallpaperCard(wallpaper: WallpaperEntry): void {
  // 1. 更新 appState（upsert）。
  // 竞态修复：首次拖入时后端先 emit wallpaper-added（thumbnail 为空），后台缩略图
  // 生成完成后立即 emit wallpaper-updated（带 thumbnail）。若 wallpaper-updated 在
  // appendWallpaperCard 追加之前到达，此时条目尚未入库（findIndex == -1）。
  // 改为 upsert（已存在则覆盖、不存在则 push），避免带缩略图的更新数据被丢弃。
  const idx = appState.allWallpapers.findIndex(w => w.id === wallpaper.id);
  if (idx === -1) {
    appState.allWallpapers.push(wallpaper);
  } else {
    appState.allWallpapers[idx] = wallpaper;
  }

  // 2. 搜索状态下 fallback 到全量刷新（文件名可能变化导致匹配状态改变）
  if (getCurrentSearchKeyword()) {
    void refreshWallpaperList();
    return;
  }

  // 3. 定位 DOM 卡片
  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;
  const card = grid.querySelector<HTMLElement>(
    `.wallpaper-card[data-id="${CSS.escape(wallpaper.id)}"]`,
  );
  if (!card) return;

  // 4. 更新卡片数据（WeakMap）
  cardData.set(card, wallpaper);

  // 5. 若卡片已 hydrate，更新缩略图区域（如缩略图刚生成完成）
  if (card.dataset.hydrated === "1") {
    const thumb = card.querySelector<HTMLElement>(".wallpaper-card-thumb");
    if (thumb) {
      // 清空旧内容（img/video/emoji）并移除错误状态类
      thumb.replaceChildren();
      thumb.classList.remove("wallpaper-thumb-error");
      // 重新填充缩略图（复用 fillThumbContent 逻辑）
      fillThumbContent(thumb, wallpaper);
    }
  }
  // 若卡片未 hydrate，cardData 已更新，hydrate 时会读取最新数据
}
