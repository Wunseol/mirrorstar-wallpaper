// UI 模块统一出口：re-export 所有 UI 渲染与交互函数
export {
  refreshWallpaperList,
  renderWallpaperList,
  // v5.0 A-PERF-001: 增量事件处理函数，供 main.ts 事件监听器调用
  appendWallpaperCard,
  removeWallpaperCard,
  updateWallpaperCard,
  // v5.0 F-PERF-004: 搜索过滤缓存，供 main.ts debouncedSearch 复用
  getLowercasedFileName,
  // 孤儿壁纸条目（源文件缺失）占位标记，供 main.ts wallpaper-source-missing 监听器调用
  markSourceMissingCard,
} from "./wallpaper-list";
export { setupPreviewModal } from "./preview-modal";
export { loadConfig, patchConfig } from "./config-panel";
export { populateDisplaySelect } from "./display-list";
export { setupDragAndDrop, setupAddButton } from "./drag-drop";
export {
  debounce,
  extractFileName,
  showStatus,
  isSupportedFile,
} from "./utils";
