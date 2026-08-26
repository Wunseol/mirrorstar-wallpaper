import { getVersion } from "@tauri-apps/api/app";
import { appState } from "./state";
import type {
  Arrangement,
  FullscreenAction,
  RegenerateProgressPayload,
  ScalingMode,
  WallpaperEntry,
} from "./types";
import {
  checkDesktopStatus,
  getAutoStartStatus,
  getWallpaperState,
  pauseWallpaper,
  regenerateThumbnails,
  resumeWallpaper,
  setInteractionMode,
  setScalingMode,
  setSpeed,
  setVolume,
  toggleAutoStart,
  toggleMute,
} from "./ipc";
import { cleanupAllListeners, listenWithCleanup, registerCleanup } from "./utils/listeners";
import { log } from "./utils/logger";
import { runAsync } from "./utils/async-helpers";
import {
  appendWallpaperCard,
  debounce,
  extractFileName,
  getLowercasedFileName,
  loadConfig,
  markSourceMissingCard,
  patchConfig,
  populateDisplaySelect,
  refreshWallpaperList,
  removeWallpaperCard,
  renderWallpaperList,
  setupAddButton,
  setupDragAndDrop,
  setupPreviewModal,
  showStatus,
  updateWallpaperCard,
} from "./ui/mod";

/**
 * 应用级监听器清理约定（F-010）
 *
 * 本文件（main.ts）作为应用入口，是单例页面——页面卸载即回收所有 DOM 监听器，
 * 实际无泄漏。因此本文件内的 `addEventListener` 调用无需改造为 `addEventListenerWithCleanup`。
 *
 * 但模块级代码（如 ui/preview-modal.ts、ui/drag-drop.ts）的监听器必须使用
 * `addEventListenerWithCleanup`（见 ui/utils.ts），因为模块可能被多次加载/卸载。
 */

/**
 * 事件绑定集中约定（v41-F-016 文档化）
 *
 * 1. 当前架构：
 *    - 所有 UI 事件绑定集中在 `main.ts` 的 `init()` 函数中（含 display-select、
 *      arrangement-select、scaling-mode-select、volume-slider、speed-slider、
 *      mute-btn、auto-start、pause/resume-btn、interaction-mode、search-input、
 *      settings-toggle、regenerate-btn 等）
 *    - UI 模块（如 wallpaper-list.ts、preview-modal.ts）仅导出渲染 / 业务函数，
 *      不在自身模块内绑定事件
 *
 * 2. 设计理由：
 *    - 统一入口便于事件生命周期管理（初始化顺序、清理时机一目了然）
 *    - 避免事件绑定散落在各 UI 模块导致的查找成本（修复一个事件需翻多个文件）
 *    - 与 F-010 清理约定配合：main.ts 内的事件随页面卸载自动回收，无需手动 cleanup
 *
 * 3. 新增 UI 模块事件约定：
 *    - 新增 UI 控件的事件绑定应在 `init()` 中追加，对应位置就近放置
 *    - 不要在 UI 模块内自行 `addEventListener`（除非是模块内动态创建元素的临时监听器）
 */

// ── 运行时类型窄化辅助 ────────────────────────────────────────────────────────
// select.value 始终是 string，需在运行时校验是否为合法枚举值，避免 as 断言绕过类型系统。
// 采用 Array.includes 模式，对常量数组做安全放宽转换后包含判断。

/// 合法的排列模式集合（与 Arrangement 类型保持同步）
const ARRANGEMENTS: readonly Arrangement[] = ["per_monitor", "span"];
function isArrangement(v: string): v is Arrangement {
  return (ARRANGEMENTS as readonly string[]).includes(v);
}

/// 合法的缩放模式集合（与 ScalingMode 类型保持同步）
const SCALING_MODES: readonly ScalingMode[] = ["fill", "fit", "stretch", "center", "original"];
function isScalingMode(v: string): v is ScalingMode {
  return (SCALING_MODES as readonly string[]).includes(v);
}

// ── Playback Button State ───────────────────────────────────────────────────

// F-001: 模块级请求序号——丢弃过期响应，避免快速切换显示器或连续 wallpaper-state-changed 事件时旧响应覆盖新响应。
let playbackSeq = 0;

/// 播放按钮默认标签（与 index.html 中 pause-btn / resume-btn 初始文案保持一致）。
const PAUSE_BTN_LABEL = "暂停";
const RESUME_BTN_LABEL = "恢复";
/// Terminated（全屏终止）临时状态下的展示文本：后端退出全屏后会自动重启子进程恢复为 Playing。
const RESUME_TERMINATED_LABEL = "恢复中...";

/**
 * 查询指定显示器的壁纸状态并更新暂停/恢复按钮的禁用状态与展示文本。
 * 统一封装按钮状态更新逻辑，供 init()、wallpaper-state-changed 监听器复用。
 *
 * 状态规则：
 * - Playing: 启用恢复按钮，禁用暂停按钮（其实是 pause-btn 可点击）
 * - Paused: 启用暂停按钮，禁用恢复按钮
 * - Terminated（全屏终止后的临时状态）: 两按钮均禁用，resume-btn 展示"恢复中..."，
 *   避免用户在全屏终止/自动恢复期间误点（此时 Resume 语义不适用）
 * - null 或其他状态：保持两按钮可点击（避免错误锁定 UI）
 */
export async function updatePlaybackButtons(displayId: string): Promise<void> {
  // F-001: 入口递增序号，捕获当前请求的序号用于响应比对
  const mySeq = ++playbackSeq;
  // F-002: 空 displayId 直接返回，保持 populateDisplaySelect 设置的禁用状态不被覆盖
  if (!displayId) return;
  const pauseBtn = document.getElementById("pause-btn") as HTMLButtonElement | null;
  const resumeBtn = document.getElementById("resume-btn") as HTMLButtonElement | null;
  if (!pauseBtn || !resumeBtn) return;
  try {
    const state = await getWallpaperState(displayId);
    // F-001: 丢弃过期响应——若期间发起了新的 updatePlaybackButtons，mySeq 已过期，直接返回不修改按钮
    if (mySeq !== playbackSeq) return;
    // 非 Terminated 状态下恢复按钮默认标签（防止此前展示的"恢复中..."残留）
    pauseBtn.textContent = PAUSE_BTN_LABEL;
    resumeBtn.textContent = RESUME_BTN_LABEL;
    if (!state) {
      // 无活跃壁纸：两按钮保持可点击（点击会触发命令并返回错误提示）
      pauseBtn.disabled = false;
      resumeBtn.disabled = false;
      return;
    }
    if (state === "Terminated") {
      // 全屏终止后的临时状态：后端退出全屏后会自动重启子进程恢复为 Playing。
      // 展示"恢复中..."并禁用播放按钮，避免用户在全屏终止/自动恢复期间误点。
      pauseBtn.disabled = true;
      resumeBtn.disabled = true;
      resumeBtn.textContent = RESUME_TERMINATED_LABEL;
      return;
    }
    const isPlaying = state === "Playing";
    pauseBtn.disabled = !isPlaying;
    resumeBtn.disabled = isPlaying;
  } catch (e) {
    log.warn("查询壁纸状态失败:", e);
  }
}

// ── App Initialization ───────────────────────────────────────────────────────

export async function init() {
  log.info("镜星壁纸 - 初始化中...");

  // F11: beforeunload 监听器提前注册到 init 最开头，在任何 listenWithCleanup 之前。
  // 这样即使后续初始化步骤（loadConfig / listenWithCleanup 等）抛错，窗口卸载时
  // cleanupAllListeners 仍被调用，避免已注册的 Tauri 事件监听器泄漏。
  window.addEventListener("beforeunload", () => {
    cleanupAllListeners();
  });

  // Display select
  const displaySelect = document.getElementById("display-select") as HTMLSelectElement;
  if (displaySelect) {
    displaySelect.addEventListener("change", () => {
      appState.selectedDisplayId = displaySelect.value;
      // 切换显示器时刷新按钮状态
      runAsync(() => updatePlaybackButtons(appState.selectedDisplayId), "updatePlaybackButtons 失败");
    });
  }

  // Arrangement select
  const arrangementSelect = document.getElementById("arrangement-select") as HTMLSelectElement;
  if (arrangementSelect) {
    // change 触发时 value 已是新值，无法像 checkbox 那样取反得到旧值，
    // 通过闭包追踪上次成功提交的值，便于失败时回滚控件
    let lastValue = arrangementSelect.value;
    arrangementSelect.addEventListener("change", async () => {
      const prev = lastValue;
      try {
        // 运行时窄化校验：select.value 为 string，需确认是合法 Arrangement 再提交
        const arrangementValue = arrangementSelect.value;
        if (!isArrangement(arrangementValue)) {
          showStatus("无效的排列模式", "error");
          arrangementSelect.value = prev;
          return;
        }
        await patchConfig({ display: { arrangement: arrangementValue } });
        showStatus("排列模式已更新", "success");
        lastValue = arrangementSelect.value;
      } catch (e) {
        log.error("更新排列模式失败:", e);
        showStatus("更新排列模式失败，请重试", "error");
        arrangementSelect.value = prev;
      }
    });
  }

  // Scaling mode select
  const scalingSelect = document.getElementById("scaling-mode-select") as HTMLSelectElement;
  if (scalingSelect) {
    // change 触发时 value 已是新值，无法像 checkbox 那样取反得到旧值，
    // 通过闭包追踪上次成功提交的值，便于失败时回滚控件
    let lastValue = scalingSelect.value;
    scalingSelect.addEventListener("change", async () => {
      const prev = lastValue;
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        const displayId = appState.selectedDisplayId;
        // 运行时窄化校验：select.value 为 string，需确认是合法 ScalingMode 再提交
        const scalingValue = scalingSelect.value;
        if (!isScalingMode(scalingValue)) {
          showStatus("无效的缩放模式", "error");
          scalingSelect.value = prev;
          return;
        }
        await setScalingMode(displayId, scalingValue);
        showStatus("缩放模式已更新", "success");
        lastValue = scalingSelect.value;
      } catch (e) {
        log.error("更新缩放模式失败:", e);
        showStatus("更新缩放模式失败，请重试", "error");
        scalingSelect.value = prev;
      }
    });
  }

  // Volume slider
  const volumeSlider = document.getElementById("volume-slider") as HTMLInputElement;
  if (volumeSlider) {
    // v41-F-001: 追踪上次成功提交的值，IPC 失败时回滚滑块到此值
    let lastSuccessfulVolume = volumeSlider.value;
    // FE-004: 添加 150ms 防抖，避免拖动滑块时每像素触发一次 IPC 调用
    const debouncedSetVolume = debounce(async () => {
      // v41-F-001: 捕获原值用于失败回滚
      const oldValue = lastSuccessfulVolume;
      const volume = parseInt(volumeSlider.value, 10) / 100;
      // F02: NaN 校验——value 为空或非数字时 clamp 到默认值 0.5，避免 NaN 传入后端
      const safeVolume = Number.isNaN(volume) ? 0.5 : volume;
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        await setVolume(appState.selectedDisplayId, safeVolume);
        // v41-F-001: 成功后更新 lastSuccessfulVolume 为当前值
        lastSuccessfulVolume = volumeSlider.value;
      } catch (e) {
        log.error("音量设置失败:", e);
        showStatus("音量设置失败，请重试", "error");
        // v41-F-001: IPC 失败回滚 UI 到原值
        volumeSlider.value = oldValue;
      }
    }, 150);
    volumeSlider.addEventListener("input", debouncedSetVolume);
  }

  // Speed slider
  const speedSlider = document.getElementById("speed-slider") as HTMLInputElement;
  const speedValue = document.getElementById("speed-value");
  if (speedSlider && speedValue) {
    // v41-F-002: 追踪上次成功提交的值，IPC 失败时回滚滑块到此值
    let lastSuccessfulSpeed = speedSlider.value;
    // FE-004: 添加 150ms 防抖，避免拖动滑块时每像素触发一次 IPC 调用
    const debouncedSetSpeed = debounce(async () => {
      // v41-F-002: 捕获原值用于失败回滚
      const oldValue = lastSuccessfulSpeed;
      const speed = parseFloat(speedSlider.value);
      // F02: NaN 校验——value 为空或非数字时 clamp 到默认值 1.0，避免 NaN 传入后端及 UI 显示 "NaNx"
      const safeSpeed = Number.isNaN(speed) ? 1.0 : speed;
      speedValue.textContent = `${safeSpeed.toFixed(2)}x`;
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        await setSpeed(appState.selectedDisplayId, safeSpeed);
        // v41-F-002: 成功后更新 lastSuccessfulSpeed 为当前值
        lastSuccessfulSpeed = speedSlider.value;
      } catch (e) {
        log.error("设置速度失败", e);
        showStatus("设置速度失败，请重试", "error");
        // v41-F-002: IPC 失败回滚 UI 到原值（滑块值与文本显示）
        speedSlider.value = oldValue;
        const oldSpeed = parseFloat(oldValue);
        if (!Number.isNaN(oldSpeed)) {
          speedValue.textContent = `${oldSpeed.toFixed(2)}x`;
        }
      }
    }, 150);
    speedSlider.addEventListener("input", debouncedSetSpeed);
  }

  // Mute button
  const muteBtn = document.getElementById("mute-btn");
  if (muteBtn) {
    muteBtn.addEventListener("click", async () => {
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        const muted = await toggleMute(appState.selectedDisplayId);
        muteBtn.textContent = muted ? "🔇" : "🔊";
      } catch (e) {
        log.error("切换静音失败:", e);
      }
    });
  }

  // Auto start checkbox
  const autoStartCheckbox = document.getElementById("auto-start") as HTMLInputElement;
  if (autoStartCheckbox) {
    autoStartCheckbox.addEventListener("change", async () => {
      // change 触发时 checked 已是新值，取反得到旧值，便于失败时回滚
      const prev = !autoStartCheckbox.checked;
      try {
        await toggleAutoStart(autoStartCheckbox.checked);
      } catch (e) {
        log.error("开机自启设置失败:", e);
        showStatus("开机自启设置失败，请重试", "error");
        autoStartCheckbox.checked = prev;
      }
    });
  }

  // 全屏处置策略下拉框（none/pause/terminate）
  const fullscreenActionSelect = document.getElementById(
    "fullscreen-action-select",
  ) as HTMLSelectElement;
  if (fullscreenActionSelect) {
    // change 触发时 value 已是新值，无法像 checkbox 那样取反得到旧值，
    // 通过闭包追踪上次成功提交的值，便于失败时回滚控件
    let lastValue = fullscreenActionSelect.value;
    fullscreenActionSelect.addEventListener("change", async () => {
      const prev = lastValue;
      try {
        await patchConfig({
          pause: { fullscreen_action: fullscreenActionSelect.value as FullscreenAction },
        });
        lastValue = fullscreenActionSelect.value;
      } catch (e) {
        log.error("更新配置失败:", e);
        showStatus("更新配置失败", "error");
        fullscreenActionSelect.value = prev;
      }
    });
  }

  // 功能6: 电池供电时暂停复选框
  const pauseOnBatteryCheckbox = document.getElementById("pause-on-battery") as HTMLInputElement;
  if (pauseOnBatteryCheckbox) {
    pauseOnBatteryCheckbox.addEventListener("change", async () => {
      // change 触发时 checked 已是新值，取反得到旧值，便于失败时回滚
      const prev = !pauseOnBatteryCheckbox.checked;
      try {
        await patchConfig({ pause: { pause_on_battery: pauseOnBatteryCheckbox.checked } });
      } catch (e) {
        log.error("更新配置失败:", e);
        showStatus("更新配置失败", "error");
        pauseOnBatteryCheckbox.checked = prev;
      }
    });
  }

  // Pause/Resume wallpaper buttons
  const pauseBtn = document.getElementById("pause-btn");
  const resumeBtn = document.getElementById("resume-btn");
  if (pauseBtn) {
    pauseBtn.addEventListener("click", async () => {
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        await pauseWallpaper(appState.selectedDisplayId);
        showStatus("壁纸已暂停", "success");
      } catch (e) {
        log.error("暂停失败", e);
        showStatus("暂停失败，请重试", "error");
      }
    });
  }
  if (resumeBtn) {
    resumeBtn.addEventListener("click", async () => {
      try {
        // FE-001: 直接透传 selectedDisplayId，ipc.ts 层统一将空串转为 null
        await resumeWallpaper(appState.selectedDisplayId);
        showStatus("壁纸已恢复", "success");
      } catch (e) {
        log.error("恢复失败", e);
        showStatus("恢复失败，请重试", "error");
      }
    });
  }

  // Interaction mode checkbox
  const interactionMode = document.getElementById("interaction-mode") as HTMLInputElement;
  if (interactionMode) {
    interactionMode.addEventListener("change", async () => {
      // change 触发时 checked 已是新值，取反得到旧值，便于失败时回滚
      const prev = !interactionMode.checked;
      try {
        await setInteractionMode(interactionMode.checked);
        showStatus(interactionMode.checked ? "已切换到交互模式" : "已切换到穿透模式", "success");
      } catch (e) {
        log.error("切换模式失败", e);
        showStatus("切换模式失败，请重试", "error");
        interactionMode.checked = prev;
      }
    });
  }

  // Load initial config
  await loadConfig();

  // 功能4: 动态获取应用版本号
  try {
    const version = await getVersion();
    const versionEl = document.querySelector(".app-version");
    if (versionEl) {
      versionEl.textContent = `MirrorStar Wallpaper v${version}`;
    }
  } catch (e) {
    log.warn("获取应用版本失败:", e);
  }

  // Get auto start status
  try {
    const autoStartEnabled = await getAutoStartStatus();
    const autoStartCheckbox = document.getElementById("auto-start") as HTMLInputElement;
    if (autoStartCheckbox) autoStartCheckbox.checked = autoStartEnabled;
  } catch (e) {
    log.warn("查询自启动状态失败:", e);
  }

  // Populate display dropdown
  if (displaySelect) {
    await populateDisplaySelect(displaySelect);
  }

  // 初始化时查询当前显示器的壁纸状态，设置按钮初始 disabled 状态
  // FE-001: 直接透传 selectedDisplayId，updatePlaybackButtons 内部经 ipc.ts 统一转 null
  await updatePlaybackButtons(appState.selectedDisplayId);

  // Setup interactive features
  setupDragAndDrop();
  setupAddButton();
  // 功能1: 初始化预览模态框
  setupPreviewModal();

  // 功能2: 壁纸搜索框
  const searchInput = document.getElementById("wallpaper-search") as HTMLInputElement;
  if (searchInput) {
    // 输入防抖（150ms）：连续键入时仅最后一次触发过滤，避免高频渲染列表
    const debouncedSearch = debounce(() => {
      const keyword = searchInput.value.trim().toLowerCase();
      if (!keyword) {
        renderWallpaperList(appState.allWallpapers);
        return;
      }
      // v5.0 F-PERF-004: 使用 getLowercasedFileName 缓存（由 refreshWallpaperList
      // 预填充），消除每次键入时对所有壁纸重复执行 extractFileName + toLowerCase
      const filtered = appState.allWallpapers.filter(wp =>
        getLowercasedFileName(wp.file_path).includes(keyword)
      );
      renderWallpaperList(filtered);
    }, 150);
    searchInput.addEventListener("input", debouncedSearch);
  }

  // 功能3: 设置面板折叠
  const settingsToggle = document.getElementById("settings-toggle");
  if (settingsToggle) {
    // 初始化 a11y 状态：按钮为图标（☰），面板默认展开，故 aria-expanded=true
    settingsToggle.setAttribute("aria-expanded", "true");
    settingsToggle.setAttribute("aria-label", "收起设置");
    settingsToggle.addEventListener("click", () => {
      const panel = document.querySelector(".settings-panel");
      if (panel) {
        panel.classList.toggle("collapsed");
        // 同步 aria-expanded 与 aria-label，保证屏幕阅读器状态与可视状态一致
        const collapsed = panel.classList.contains("collapsed");
        settingsToggle.setAttribute("aria-expanded", String(!collapsed));
        settingsToggle.setAttribute("aria-label", collapsed ? "展开设置" : "收起设置");
      }
    });
  }

  // 重新生成缩略图按钮：批量重生成所有壁纸的缩略图
  const regenerateBtn = document.getElementById("regenerate-thumbs-btn") as HTMLButtonElement | null;
  // v16-A-011: 进度条 DOM 引用（后端节流 emit wallpaper-regenerate-progress 更新）
  const progressWrap = document.getElementById("regenerate-progress");
  const progressBar = document.getElementById("regenerate-progress-bar");
  const progressText = document.getElementById("regenerate-progress-text");
  if (regenerateBtn) {
    regenerateBtn.addEventListener("click", async () => {
      // 防止重复点击：执行期间禁用按钮
      regenerateBtn.disabled = true;
      // 重置并显示进度条（total 未知时先置 0%，首帧进度事件到达后更新）
      if (progressBar) progressBar.style.width = "0%";
      if (progressText) progressText.textContent = "准备中…";
      if (progressWrap) progressWrap.hidden = false;
      try {
        showStatus("正在重新生成缩略图...", "info");
        const result = await regenerateThumbnails();
        showStatus(
          `已重新生成 ${result.success} 个缩略图${result.failed > 0 ? `，失败 ${result.failed} 个` : ""}`,
          result.failed > 0 ? "error" : "success"
        );
      } catch (e) {
        log.error("重新生成缩略图失败:", e);
        showStatus("重新生成缩略图失败，请重试", "error");
      } finally {
        regenerateBtn.disabled = false;
        // 延迟隐藏进度条，让用户看到 100% 终态（与最终 toast 呼应）
        setTimeout(() => {
          if (progressWrap) progressWrap.hidden = true;
        }, 800);
      }
    });
  }

  // v5.0 A-PERF-001: 增量事件处理（替代原防抖全量刷新）
  // 单张壁纸变更时仅更新对应 DOM 卡片，避免全量 IPC 重拉 + 全量 DOM 重建。
  // 保留 refreshWallpaperList 作为 fallback（payload 不完整等异常场景）。

  // wallpaper-added: 批量合并 50ms 后增量追加（避免 drag-drop 批量添加时多次 DOM 操作）
  // payload 为完整 WallpaperEntry；若 payload 不完整则 fallback 到全量刷新
  const pendingAdds: WallpaperEntry[] = [];
  let addFlushScheduled = false;
  await listenWithCleanup<WallpaperEntry>("wallpaper-added", (wallpaper) => {
    if (!wallpaper || !wallpaper.id) {
      // payload 不完整（旧版后端兼容或异常），fallback 到全量刷新
      void refreshWallpaperList();
      return;
    }
    pendingAdds.push(wallpaper);
    if (!addFlushScheduled) {
      addFlushScheduled = true;
      setTimeout(() => {
        addFlushScheduled = false;
        const batch = pendingAdds.splice(0);
        for (const wp of batch) {
          appendWallpaperCard(wp);
        }
      }, 50);
    }
  });

  // wallpaper-removed: 增量移除（payload 为壁纸 id 字符串）
  await listenWithCleanup<string>("wallpaper-removed", (id) => {
    if (!id) {
      void refreshWallpaperList();
      return;
    }
    removeWallpaperCard(id);
  });

  // wallpaper-updated: 增量更新（payload 为完整 WallpaperEntry，如缩略图生成完成）
  await listenWithCleanup<WallpaperEntry>("wallpaper-updated", (wallpaper) => {
    if (!wallpaper || !wallpaper.id) {
      // payload 不完整（旧版后端兼容或批量场景异常），fallback 到全量刷新
      void refreshWallpaperList();
      return;
    }
    updateWallpaperCard(wallpaper);
  });
  // 后端缩略图生成失败时提示用户（不强制刷新列表，避免打断搜索/滚动）
  await listenWithCleanup<{ file_path: string; error: string }>(
    "wallpaper-thumbnail-failed",
    (payload) => {
      const fileName = extractFileName(payload.file_path);
      showStatus(`缩略图生成失败：${fileName}`, "error");
    }
  );
  // 孤儿壁纸条目（源文件缺失）：温和处理，非 error。在卡片缩略图区标「源文件缺失」
  // 占位 + info 级状态提示，不强制全量刷新（避免打断搜索/滚动，与 thumbnail-failed 一致）。
  await listenWithCleanup<{ id: string; file_path: string }>(
    "wallpaper-source-missing",
    (payload) => {
      if (!payload || !payload.id) return;
      markSourceMissingCard(payload.id);
      const fileName = extractFileName(payload.file_path);
      showStatus(`源文件缺失：${fileName}`, "info");
    }
  );
  // v16-A-011: 批量重生成进度事件。后端 regenerate_thumbnails 节流 emit
  // wallpaper-regenerate-progress（每 5 项或 200ms 一次），此处更新进度条 UI。
  // 进度条的显示/隐藏由 regenerateBtn 点击处理逻辑控制，本监听器仅负责更新数值。
  await listenWithCleanup<RegenerateProgressPayload>(
    "wallpaper-regenerate-progress",
    (payload) => {
      if (!payload || typeof payload.processed !== "number") return;
      const total = payload.total > 0 ? payload.total : 0;
      const pct = total > 0 ? Math.min(100, (payload.processed / total) * 100) : 0;
      if (progressBar) progressBar.style.width = `${pct.toFixed(1)}%`;
      if (progressText) {
        progressText.textContent =
          total > 0
            ? `${payload.processed}/${total}（成功 ${payload.success}，失败 ${payload.failed}）`
            : `${payload.processed}（成功 ${payload.success}，失败 ${payload.failed}）`;
      }
    }
  );
  // 监听器忽略 event.payload（全局暂停/恢复时 payload 为空字符串），
  // 始终查询当前选中显示器的状态，确保按钮状态与用户当前视图一致
  //
  // v5.0 F-PERF-016: 添加 150ms 防抖合并连续事件。后端壁纸启动/切换时可能连续
  // emit Initializing → Playing 等多个事件，F-001 序号取消使仅最后一次生效，但
  // 前几次 IPC 仍发出且后端执行查询。150ms 防抖合并连续事件为单次 IPC（比
  // refresh 的 300ms 短，因按钮状态更新更时效）。
  const debouncedUpdatePlayback = debounce(() => {
    runAsync(() => updatePlaybackButtons(appState.selectedDisplayId), "updatePlaybackButtons 失败");
  }, 150);
  await listenWithCleanup<string>("wallpaper-state-changed", () => debouncedUpdatePlayback());

  // v16-C-009: Web 壁纸冷启动进度提示。
  // 后端 set_wallpaper 检测到 Web 类型冷启动时 emit
  // `wallpaper-loading`，payload { display_id, message }。此处展示 info toast
  // 告知用户 WebView2 引擎初始化预计 5-15s，避免误以为卡死。
  // state-changed 事件到达后按钮状态更新，toast 由 showStatus 自身定时清除。
  await listenWithCleanup<{ display_id: string; message: string }>(
    "wallpaper-loading",
    (payload) => {
      if (payload && typeof payload.message === "string") {
        showStatus(payload.message, "info");
      }
    }
  );

  // v16-C-010: 4K GIF 帧超 8MB 阈值降级警告。
  // 后端 set_wallpaper 对 GIF 类型检测首帧（降采样后）超 8MB 阈值时 emit
  // `wallpaper-gif-oversized`，payload { display_id }。此处展示 warning toast
  // 提示用户播放可能不流畅（v15-B-005 同步解码兜底导致帧率下降）。
  // 文案前端硬编码（场景固定），payload 仅用于将来扩展（如按显示器区分）。
  await listenWithCleanup<{ display_id: string }>(
    "wallpaper-gif-oversized",
    () => {
      showStatus("GIF 分辨率过高，播放可能不流畅", "warning");
    }
  );

  // 监听配置热重载事件，刷新配置 UI
  await listenWithCleanup("config-changed", () => {
    runAsync(() => loadConfig(), "loadConfig 失败");
  });

  // C01 修复：监听配置加载错误事件，展示 toast 告警
  // 后端 load_config/load_library 解析失败但已回退到默认配置时 emit 此事件，
  // payload 为用户友好的错误消息字符串，展示为 error toast 让用户知晓配置被重置。
  await listenWithCleanup<string>("config-load-error", (payload) => {
    showStatus(payload, "error");
  });

  // v16-C-007：监听 WorkerW 失效事件，启动轮询补救 + 超时提示
  //
  // 后端 workerw_check.rs 检测到 WorkerW 失效后 emit `desktop-status-changed`，
  // payload `{ ok: boolean, error?: string }`：
  // - ok:true  → WorkerW 已恢复，刷新显示器/壁纸状态
  // - ok:false → 重新初始化失败，启动 2s 间隔 check_desktop_status 轮询，
  //              30s 超时后提示用户重启应用（按 system.rs docstring 约定）
  //
  // 轮询防重入：用 pollInFlight 标志避免事件连续触发时启动多个轮询循环。
  let pollInFlight = false;
  await listenWithCleanup<{ ok: boolean; error?: string }>("desktop-status-changed", (payload) => {
    if (payload && payload.ok) {
      // WorkerW 已恢复：刷新壁纸状态（按钮 + 列表）
      runAsync(() => updatePlaybackButtons(appState.selectedDisplayId), "updatePlaybackButtons 失败");
      return;
    }
    if (pollInFlight) {
      // 已有轮询进行中，忽略重复触发
      return;
    }
    pollInFlight = true;
    log.warn("收到 desktop-status-changed (ok:false)，启动 WorkerW 状态轮询...");
    const pollInterval = 2000; // 2s
    const deadline = Date.now() + 30000; // 30s 超时
    const timer = setInterval(async () => {
      try {
        const recovered = await checkDesktopStatus();
        if (recovered) {
          // 已恢复：停止轮询并刷新状态
          clearInterval(timer);
          pollInFlight = false;
          showStatus("桌面状态已恢复", "success");
          runAsync(() => updatePlaybackButtons(appState.selectedDisplayId), "updatePlaybackButtons 失败");
          return;
        }
        if (Date.now() >= deadline) {
          // 超时：停止轮询并提示用户
          clearInterval(timer);
          pollInFlight = false;
          log.error("WorkerW 状态轮询 30s 超时仍未恢复");
          showStatus("桌面状态异常，壁纸可能无法显示，请重启应用", "error");
        }
      } catch (e) {
        log.error("check_desktop_status 轮询失败:", e);
        // 轮询失败不立即停止，继续重试直到 deadline
        if (Date.now() >= deadline) {
          clearInterval(timer);
          pollInFlight = false;
          showStatus("桌面状态异常，壁纸可能无法显示，请重启应用", "error");
        }
      }
    }, pollInterval);
    // 登记清理：窗口卸载时停止轮询，避免泄漏
    registerCleanup(() => clearInterval(timer));
  });

  // v16-C-008：监听音频降级事件，禁用音量控件 + toast 提示
  //
  // 后端 lib.rs 启动检测到 VolumeControl::new() 失败（无音频设备）降级为 no-op 时
  // emit `audio-disabled`，前端禁用音量/静音控件并提示用户视频壁纸将无声播放，
  // 避免"调音量看似成功实际无声"的困惑。
  await listenWithCleanup("audio-disabled", () => {
    log.warn("收到 audio-disabled 事件，音量控件将禁用");
    const volumeSlider = document.getElementById("volume-slider") as HTMLInputElement | null;
    const muteBtn = document.getElementById("mute-btn") as HTMLButtonElement | null;
    if (volumeSlider) {
      volumeSlider.disabled = true;
      volumeSlider.title = "音频设备不可用，视频壁纸将无声播放";
    }
    if (muteBtn) {
      muteBtn.setAttribute("disabled", "");
      muteBtn.title = "音频设备不可用，视频壁纸将无声播放";
    }
    showStatus("未检测到音频设备，视频壁纸将无声播放", "info");
  });

  // Load initial wallpaper list
  await refreshWallpaperList();
}

document.addEventListener("DOMContentLoaded", () => {
  init().catch((e) => {
    log.error("应用初始化失败:", e);
    showStatus("初始化失败，请重启应用", "error");
  });
});
