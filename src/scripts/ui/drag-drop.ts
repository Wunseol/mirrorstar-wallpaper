import { getCurrentWebview } from "@tauri-apps/api/webview";
import { addWallpaper, getErrorMessage, openFileDialog } from "../ipc";
import { addEventListenerWithCleanup, registerCleanup } from "../utils/listeners";
import { log } from "../utils/logger";
import { extractFileName, isSupportedFile, showStatus } from "./utils";

// ── Drag and Drop ────────────────────────────────────────────────────────────

export async function setupDragAndDrop() {
  const grid = document.getElementById("wallpaper-grid");
  if (!grid) return;

  // Tauri drag-drop event - provides actual file paths via webview API
  // 保存返回的 unlisten 函数，登记到全局清理机制
  try {
    const unlisten = await getCurrentWebview().onDragDropEvent(async (event) => {
      if (event.payload.type === "enter" || event.payload.type === "over") {
        grid.classList.add("drag-over");
      } else if (event.payload.type === "leave") {
        grid.classList.remove("drag-over");
      } else if (event.payload.type === "drop") {
        grid.classList.remove("drag-over");
        const paths = event.payload.paths;
        // v41-F-004: 多文件拖放维护 pendingCount 计数器，显示总进度
        let pendingCount = paths.length;
        if (paths.length > 1) {
          showStatus(`添加进度: ${paths.length - pendingCount} / ${paths.length}`, "info");
        }
        // v41-F-011: 串行化多文件添加，避免并发 addWallpaper 竞态导致顺序错乱或后写覆盖
        for (const filePath of paths) {
          await handleDroppedFile(filePath);
          pendingCount--;
          if (paths.length > 1 && pendingCount > 0) {
            showStatus(`添加进度: ${paths.length - pendingCount} / ${paths.length}`, "info");
          }
        }
      }
    });
    // onDragDropEvent 返回 Promise<UnlistenFn>，解析后登记清理
    registerCleanup(unlisten);
  } catch (e) {
    log.error("注册拖放事件监听失败:", e);
  }
}

/**
 * 添加壁纸入库，不自动设为桌面壁纸。
 *
 * 用户添加壁纸后仅入库，需在预览模态框中手动点击"设为壁纸"按钮才会应用。
 * 失败时提示"添加壁纸失败"。
 *
 * 返回值表示添加是否成功（true=成功，false=失败且已向用户提示）。
 * 调用方依据返回值决定是否显示"壁纸添加成功"，避免重复或矛盾的状态提示。
 */
async function addWallpaperToLibrary(filePath: string): Promise<boolean> {
  try {
    await addWallpaper(filePath);
  } catch (e) {
    log.error("添加壁纸失败", e);
    showStatus(`添加壁纸失败: ${getErrorMessage(e)}`, "error");
    return false;
  }
  return true;
}

async function handleDroppedFile(filePath: string) {
  if (!isSupportedFile(filePath)) {
    showStatus(`不支持的文件类型: ${extractFileName(filePath)}`, "error");
    return;
  }
  // addWallpaperToLibrary 内部已处理错误提示，仅当完全成功时显示"添加成功"
  const ok = await addWallpaperToLibrary(filePath);
  if (ok) {
    showStatus("壁纸添加成功", "success");
  }
}

// ── Add Wallpaper Button ─────────────────────────────────────────────────────

export function setupAddButton() {
  const btn = document.getElementById("add-wallpaper-btn");
  if (!btn) return;

  // FE-003: 使用 addEventListenerWithCleanup 登记清理，符合 F-010 约定
  addEventListenerWithCleanup(btn, "click", async () => {
    try {
      const filePath = await openFileDialog();
      if (!filePath) return; // User cancelled

      // addWallpaperToLibrary 内部已处理错误提示，仅当完全成功时显示"添加成功"
      const ok = await addWallpaperToLibrary(filePath);
      if (ok) {
        showStatus("壁纸添加成功", "success");
      }
    } catch (e) {
      log.error("添加壁纸失败:", e);
      showStatus(`添加壁纸失败: ${getErrorMessage(e)}`, "error");
    }
  });
}
