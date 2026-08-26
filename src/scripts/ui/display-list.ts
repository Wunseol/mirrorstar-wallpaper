import { getDisplays } from "../ipc";
import { appState } from "../state";
import { log } from "../utils/logger";
import { showStatus } from "./utils";

// ── Display List Rendering ───────────────────────────────────────────────────

/** 填充显示器下拉选择框，并选中主显示器 */
export async function populateDisplaySelect(displaySelect: HTMLSelectElement) {
  try {
    const displays = await getDisplays();
    // F08: getDisplays 返回空数组时提示用户并禁用依赖 displayId 的控件，
    // 避免用户在"无显示器"状态下提交无效 displayId 到后端
    if (displays.length === 0) {
      log.warn("未检测到显示器（getDisplays 返回空数组）");
      showStatus("未检测到显示器", "error");
      displaySelect.disabled = true;
      // 依赖 displayId 的播放控件也一并禁用，避免用户误操作
      disablePlaybackControls();
      return;
    }
    for (const display of displays) {
      const option = document.createElement("option");
      option.value = display.id;
      const prefix = display.is_primary ? "[主] " : "";
      option.textContent = `${prefix}${display.name} (${display.width}x${display.height})`;
      displaySelect.appendChild(option);
      if (display.is_primary) {
        appState.selectedDisplayId = display.id;
        displaySelect.value = display.id;
      }
    }
  } catch (e) {
    log.error("获取显示器列表失败:", e);
    // 异常路径同样禁用控件，保持与空列表一致的容错语义
    displaySelect.disabled = true;
    disablePlaybackControls();
  }
}

/**
 * F08: 禁用依赖 displayId 的播放控件。
 *
 * 当无可用显示器时，pause/resume 等操作无意义且会向后端发送无效 displayId，
 * 故统一禁用按钮避免误操作。控件在下次成功 populateDisplaySelect 时不会被自动恢复
 * （浏览器刷新或重新初始化才会重置 disabled 状态）。
 */
function disablePlaybackControls(): void {
  const ids = ["pause-btn", "resume-btn"];
  for (const id of ids) {
    const btn = document.getElementById(id) as HTMLButtonElement | null;
    if (btn) btn.disabled = true;
  }
}
