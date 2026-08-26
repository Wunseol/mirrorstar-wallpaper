/**
 * 前端共享类型定义模块（v41-F-016 文档化）。
 *
 * 1. 类型对应关系：
 *    - 本文件中的 interface / type 对应后端 Rust 结构体与枚举
 *    - 示例：`WallpaperEntry` ↔ Rust `WallpaperEntry`，`WallpaperState` ↔ Rust `WallpaperState` 枚举
 *    - 枚举的 serde 序列化格式（PascalCase / lowercase / snake_case）已在各类型注释中标注
 *
 * 2. 字段命名规则：
 *    - 响应类型字段（如 `WallpaperEntry.file_path`、`DisplayInfo.is_primary`）：
 *      使用 snake_case，与 Rust 结构体字段名直接对应（后端 serde 默认输出 snake_case，无需 rename）
 *    - IPC 请求参数（如 ipc.ts 中 `addWallpaper(filePath, displayId)` 的 args 对象）：
 *      使用 camelCase（TypeScript 惯例），后端 Tauri handler 通过 serde `rename_all = "camelCase"`
 *      或手动映射接受 camelCase 键
 *    - 历史不一致：部分 Finding（如 v41-F-016）提及 `DisplayId` vs `displayId` 差异，
 *      实际代码中响应字段统一为 snake_case（`display_id`），请求参数为 camelCase（`displayId`）；
 *      新增字段时需注意区分响应类型与请求参数的命名约定
 *
 * 3. 新增字段同步约定：
 *    - 新增 Rust 结构体字段时，必须同步在对应 TS interface 中添加字段
 *    - 字段类型需对应：`Option<T>` → `T | null`，`Vec<T>` → `T[]`，`String` → `string`，`bool` → `boolean`
 *    - 枚举新增变体时，TS 联合类型需同步追加（如 `WallpaperState` 新增状态）
 *    - 建议在 PR review 中检查 Rust 与 TS 的字段一致性
 *
 * 4. 未来改进方向：
 *    - 引入 `ts-rs` crate 从 Rust 结构体自动生成 TS 类型定义，消除手动同步成本
 *    - 或用 `specta` + `tauri-specta` 在 Tauri 命令层面自动生成 TS 类型与 IPC 绑定
 *    - 当前手动同步在字段数量适中时可控，自动化生成待后端重构时评估
 */

// Shared types

/// 壁纸状态联合类型，与后端 Rust WallpaperState 枚举的 serde 序列化格式一致（PascalCase）
export type WallpaperState = "Initializing" | "Playing" | "Paused" | "Terminated";

/// 缩放模式，与后端 Rust ScalingMode 枚举的 serde 序列化格式一致（lowercase）
export type ScalingMode = "fill" | "fit" | "stretch" | "center" | "original";

/// 显示器排列模式，与后端 Rust Arrangement 枚举的 serde 序列化格式一致（snake_case）
export type Arrangement = "per_monitor" | "span";

/// GIF 内存管理策略，与后端 Rust GifMemoryStrategy 枚举的 serde 序列化格式一致（PascalCase）
export type GifMemoryStrategy = "Aggressive" | "Balanced" | "Performance" | "Adaptive";

/// 壁纸类型，与后端 Rust WallpaperType 枚举的 serde 序列化格式一致（PascalCase）
export type WallpaperType = "Video" | "Gif" | "Web" | "Image";

export interface WallpaperEntry {
  id: string;
  file_path: string;
  wallpaper_type: WallpaperType;
  display_id: string | null;
  added_at: string;
  thumbnail: string;
  file_size: number;
  metadata: WallpaperMetadata | null;
}

export interface WallpaperMetadata {
  width: number | null;
  height: number | null;
  duration: number | null;
  frame_count: number | null;
}

export interface DisplayInfo {
  id: string;
  name: string;
  width: number;
  height: number;
  x: number;
  y: number;
  is_primary: boolean;
  dpi: number;
  current_wallpaper: string | null;
}

export interface AppConfig {
  general: GeneralConfig;
  audio: AudioConfig;
  pause: PauseConfig;
  display: DisplayConfig;
  video: VideoConfig;
  gif: GifConfig;
}

export interface GeneralConfig {
  auto_start: boolean;
  minimize_to_tray: boolean;
}

export interface AudioConfig {
  volume: number;
  muted: boolean;
}

/** 全屏时壁纸处置策略 */
export type FullscreenAction = "none" | "pause" | "terminate";

export interface PauseConfig {
  fullscreen_action: FullscreenAction;
  pause_on_battery: boolean;
}

export interface DisplayConfig {
  arrangement: Arrangement;
}

export interface VideoConfig {
  hwdec: boolean;
  speed: number;
}

export interface GifConfig {
  memory_strategy: GifMemoryStrategy;
  balanced_keep_frames: number;
  /** v41-W-012: GIF 帧像素内存预算上限（MB），默认 40，范围 [10, 500] */
  max_memory_mb: number;
}

/**
 * v16-A-011: 批量缩略图重生成进度事件 payload。
 *
 * 后端 `regenerate_thumbnails` command 在生成循环中节流 emit
 * `wallpaper-regenerate-progress`（每 5 项或 200ms 一次，循环结束再 emit 一次确保
 * 100% 送达）。字段全部为非负整数（后端 usize 序列化为 JSON number）。
 */
export interface RegenerateProgressPayload {
  /** 已处理项数（success + failed） */
  processed: number;
  /** 成功生成缩略图的项数 */
  success: number;
  /** 生成失败的项数 */
  failed: number;
  /** 总项数（wallpapers.len()） */
  total: number;
}

