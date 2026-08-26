/**
 * 应用全局状态管理模块（v41-F-015 文档化）。
 *
 * 状态管理约定：
 *
 * 1. 集中管理：
 *    - 全局状态集中在 `appState` 对象中（FE-002 改进后统一封装）
 *    - 不再使用散落的模块级 `let` 变量，避免新增状态时遗漏初始化或散落多处难以追踪
 *
 * 2. 新增状态初始化：
 *    - 新增状态需在 `appState` 对象字面量中显式声明并初始化（提供默认值）
 *    - 默认值应与后端首次返回值语义一致（如 `allWallpapers` 默认 `[]` 而非 `null`）
 *    - 避免在模块加载时执行副作用初始化（如 DOM 查询、IPC 调用），应在 `main.ts` 的 `init()` 中完成
 *
 * 3. 访问约定：
 *    - 读取：通过 `appState.xxx` 直接读取
 *    - 写入：普通字段直接赋值；需要校验的字段（如 `selectedDisplayId`）通过 setter 写入
 *    - 跨模块共享状态必须通过 `appState`，避免在各模块私有 `let` 变量中复制
 *
 * 4. 未来改进方向：
 *    - 用 `class AppState` + getter/setter 封装，提供更严格的访问控制与变更通知
 *    - 或用 `Proxy` 拦截读写，实现自动日志 / 校验 / 响应式更新
 *    - 当前 `appState` 为简单对象字面量，足够覆盖现有需求；封装收益与复杂度需权衡
 */
import type { WallpaperEntry } from "./types";

// ── Helpers ──────────────────────────────────────────────────────────────────

/**
 * 应用全局状态对象（FE-002 改进）
 *
 * 设计说明：
 * - statusTimer 已移至 utils.ts 模块内部（仅 showStatus 使用，无需跨模块暴露）
 * - selectedDisplayId 通过 setter 校验，空串被视为无效并记录告警
 *   （不强制拒绝，避免破坏现有 fallback 流程；FE-001 已在前端/后端统一将空串转 null）
 * - allWallpapers / currentPreviewId 仍为公开字段（多模块读写，封装收益低）
 */
export const appState = {
  _selectedDisplayId: "",
  allWallpapers: [] as WallpaperEntry[],
  currentPreviewId: null as string | null,

  /** 当前选中的显示器 ID（空串表示未选择，由下游 ipc.ts 统一转 null） */
  get selectedDisplayId(): string {
    return this._selectedDisplayId;
  },

  /** 设置当前选中的显示器 ID；空串会被接受但记录告警，便于追踪 FE-001 类问题 */
  set selectedDisplayId(value: string) {
    if (value === "") {
      // 不阻断赋值（保持向后兼容），但记录告警便于排查
      // 使用 console.warn 而非 logger 以避免循环依赖
      console.warn("[appState] selectedDisplayId 被设置为空串");
    }
    this._selectedDisplayId = value;
  },
};
