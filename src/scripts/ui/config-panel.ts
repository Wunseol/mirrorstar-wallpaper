import { getConfig, updateConfig } from "../ipc";
import type { AppConfig } from "../types";
import { log } from "../utils/logger";
import { showStatus } from "./utils";

/** AppConfig 的局部更新类型：每个 section 可选，且 section 内字段也可选 */
type PartialAppConfig = {
  [K in keyof AppConfig]?: Partial<AppConfig[K]>;
};

/**
 * 模块级 promise 链：用于串行化所有 patchConfig 调用。
 *
 * 背景：patchConfig 内部为非原子的 read-modify-write（getConfig→深合并→updateConfig）。
 * 若两次调用并发执行，第二次的 getConfig 可能在第一次的 updateConfig 落库前返回，
 * 用陈旧 base 合并后覆盖写入，导致前一次变更丢失。通过该链强制两次调用串行执行。
 */
let patchChain: Promise<void> = Promise.resolve();

// v5.0 F-PERF-015: 配置面板元素引用缓存。
// 原实现每次 loadConfig 都执行 8 次 getElementById。loadConfig 在 init + 每次
// config-changed 事件时调用（main.ts:542-544），用户修改配置即触发。改为模块级
// 缓存，首次 loadConfig 时初始化一次，后续直接使用缓存引用（DOM 元素固定不变）。
// 元素可能为 null（如 DOM 未就绪或被移除），缓存后使用前仍需 null 检查。
interface ConfigElements {
  volumeSlider: HTMLInputElement | null;
  autoStartCheckbox: HTMLInputElement | null;
  fullscreenActionSelect: HTMLSelectElement | null;
  pauseOnBatteryCheckbox: HTMLInputElement | null;
  arrangementSelect: HTMLSelectElement | null;
  speedSlider: HTMLInputElement | null;
  speedValue: HTMLElement | null;
  muteBtn: HTMLElement | null;
  rotationEnabledCheckbox: HTMLInputElement | null;
  rotationOnBootCheckbox: HTMLInputElement | null;
  rotationIntervalInput: HTMLInputElement | null;
  rotationOrderSelect: HTMLSelectElement | null;
  rotationArrangementSelect: HTMLSelectElement | null;
}

let cachedConfigEls: ConfigElements | null = null;

function getConfigEls(): ConfigElements {
  if (cachedConfigEls) return cachedConfigEls;
  cachedConfigEls = {
    volumeSlider: document.getElementById("volume-slider") as HTMLInputElement | null,
    autoStartCheckbox: document.getElementById("auto-start") as HTMLInputElement | null,
    fullscreenActionSelect: document.getElementById(
      "fullscreen-action-select",
    ) as HTMLSelectElement | null,
    pauseOnBatteryCheckbox: document.getElementById("pause-on-battery") as HTMLInputElement | null,
    arrangementSelect: document.getElementById("arrangement-select") as HTMLSelectElement | null,
    speedSlider: document.getElementById("speed-slider") as HTMLInputElement | null,
    speedValue: document.getElementById("speed-value"),
    muteBtn: document.getElementById("mute-btn"),
    rotationEnabledCheckbox: document.getElementById("rotation-enabled") as HTMLInputElement | null,
    rotationOnBootCheckbox: document.getElementById(
      "rotation-on-boot",
    ) as HTMLInputElement | null,
    rotationIntervalInput: document.getElementById(
      "rotation-interval",
    ) as HTMLInputElement | null,
    rotationOrderSelect: document.getElementById("rotation-order") as HTMLSelectElement | null,
    rotationArrangementSelect: document.getElementById(
      "rotation-arrangement-select",
    ) as HTMLSelectElement | null,
  };
  return cachedConfigEls;
}

// ── App Initialization ───────────────────────────────────────────────────────

/** 加载配置并同步到 UI 控件（初始加载与 config-changed 事件复用） */
export async function loadConfig() {
  try {
    const config = await getConfig();
    // v5.0 F-PERF-015: 使用缓存引用替代 8 次 getElementById
    const {
      volumeSlider,
      autoStartCheckbox,
      fullscreenActionSelect,
      pauseOnBatteryCheckbox,
      arrangementSelect,
      speedSlider,
      speedValue,
      muteBtn,
      rotationEnabledCheckbox,
      rotationOnBootCheckbox,
      rotationIntervalInput,
      rotationOrderSelect,
      rotationArrangementSelect,
    } = getConfigEls();
    if (volumeSlider) volumeSlider.value = String(Math.round(config.audio.volume * 100));
    if (autoStartCheckbox) autoStartCheckbox.checked = config.general.auto_start;
    // 全屏处置策略三选一（none/pause/terminate）
    if (fullscreenActionSelect) fullscreenActionSelect.value = config.pause.fullscreen_action;
    // 功能6: 同步电池暂停复选框状态
    if (pauseOnBatteryCheckbox) pauseOnBatteryCheckbox.checked = config.pause.pause_on_battery;
    // FE-013: 移除 `|| "per_monitor"` dead code——arrangement 类型为 Arrangement 联合类型
    // （"per_monitor" | "all_same" | "span"），始终 truthy，`|| "per_monitor"` 永不触发。
    if (arrangementSelect) arrangementSelect.value = config.display.arrangement;
    if (speedSlider && speedValue) {
      speedSlider.value = String(config.video.speed || 1.0);
      speedValue.textContent = `${parseFloat(speedSlider.value).toFixed(2)}x`;
    }
    // FE-005: 同步 mute 按钮图标与配置中的静音状态
    if (muteBtn) muteBtn.textContent = config.audio.muted ? "🔇" : "🔊";
    // 壁纸轮换（Task 10.1）：从 config.rotation 同步轮换设置控件
    if (rotationEnabledCheckbox) rotationEnabledCheckbox.checked = config.rotation.enabled;
    if (rotationOnBootCheckbox) rotationOnBootCheckbox.checked = config.rotation.on_boot;
    if (rotationIntervalInput) {
      rotationIntervalInput.value = String(config.rotation.interval_minutes);
    }
    if (rotationOrderSelect) rotationOrderSelect.value = config.rotation.order;
    if (rotationArrangementSelect) {
      rotationArrangementSelect.value = config.rotation.arrangement;
    }
    log.info("配置加载完成");
  } catch (e) {
    log.error("加载配置失败:", e);
    // F03: 配置加载失败时不再静默吞错，向用户展示提示以便知晓当前使用默认配置
    showStatus("配置加载失败，使用默认配置", "error");
  }
}

/**
 * 局部更新配置的实际执行体：getConfig + 深合并 patch + updateConfig。
 * 注意：此函数本身不处理并发，需由外层 patchConfig 通过 patchChain 串行调度。
 */
async function doPatchConfig(patch: PartialAppConfig): Promise<void> {
  const config = await getConfig();
  const merged: AppConfig = {
    general: { ...config.general, ...(patch.general ?? {}) },
    audio: { ...config.audio, ...(patch.audio ?? {}) },
    pause: { ...config.pause, ...(patch.pause ?? {}) },
    display: { ...config.display, ...(patch.display ?? {}) },
    video: { ...config.video, ...(patch.video ?? {}) },
    gif: { ...config.gif, ...(patch.gif ?? {}) },
    rotation: { ...config.rotation, ...(patch.rotation ?? {}) },
  };
  await updateConfig(merged);
}

/**
 * 局部更新配置：内部 getConfig + 深合并 patch + updateConfig。
 * 消除 main.ts 中重复的 getConfig+merge+updateConfig 模式。
 *
 * 通过模块级 patchChain 串行化所有调用：每次调用都挂在前一次完成之后，
 * 避免并发 read-modify-write 导致的后写覆盖先写问题（F-002）。
 * 某次失败不会阻塞后续调用（链已通过 catch 恢复为 resolved），
 * 但失败仍会向该次调用者正确 reject，可被 catch 捕获。
 */
export function patchConfig(patch: PartialAppConfig): Promise<void> {
  const run = patchChain.then(() => doPatchConfig(patch));
  // 维持链不断：吞掉本次错误使链恢复为 resolved，后续 then 可继续；
  // 注意不能直接把 patchChain 赋为 run，否则 reject 会传染到后续所有调用。
  patchChain = run.catch(() => {});
  return run;
}
