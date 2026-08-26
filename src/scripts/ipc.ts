import { invoke } from "@tauri-apps/api/core";
import type { AppConfig, DisplayInfo, ScalingMode, WallpaperEntry, WallpaperState } from "./types";

/**
 * FE-001: 统一将 displayId 空串/undefined 转为 null，与后端 Option<String> 对齐。
 *
 * 后端 Tauri handler 期望 `display_id: Option<String>`：
 * - 有效显示器 ID（非空字符串）原样透传
 * - 空串（未选择显示器）/ undefined（参数缺省）/ null（显式空值）统一转为 null
 *
 * 抽取此 helper 替代各 wrapper 中重复的 `displayId || null` 表达式，
 * 避免每个调用点重复书写且方便后续统一调整空值语义。
 */
function toDisplayIdArg(displayId: string | null | undefined): string | null {
  return displayId || null;
}

// IPC command wrappers
export async function getWallpapers(): Promise<WallpaperEntry[]> {
  return invoke<WallpaperEntry[]>("get_wallpapers");
}

export async function addWallpaper(filePath: string, displayId?: string): Promise<string> {
  // F09: add_wallpaper 涉及文件复制 + 缩略图生成，可能耗时，使用 30s 超时
  return invokeWithTimeout<string>(
    "add_wallpaper",
    { filePath, displayId: toDisplayIdArg(displayId) },
    30000,
  );
}

export async function removeWallpaper(wallpaperId: string, deleteFile: boolean): Promise<void> {
  // F09: remove_wallpaper 涉及文件删除，大视频文件可能慢，使用 15s 超时
  // v16-B-005: 后端形参重命名为 `wallpaper_id`，Tauri v2 自动将前端 `wallpaperId`
  // 反序列化为 `wallpaper_id`（参数名 camelCase → snake_case）。
  await invokeWithTimeout<void>(
    "remove_wallpaper",
    { wallpaperId, deleteFile },
    15000,
  );
}

/**
 * v16-A-013：从 invoke reject 的错误对象中提取 `code` 字段。
 *
 * 后端 `MirrorStarError` 序列化为 `{ code: string, message: string }`，
 * 前端可通过此函数取出 `code` 做错误类型分支（如 `InvalidConfig` 提示用户检查配置、
 * `InvalidPath` 提示文件路径问题）。
 *
 * 非 MirrorStarError 错误（如 invoke 超时抛出的 `Error("命令 xxx 超时")`、
 * 网络层错误）返回 `null`，调用方应优先使用 `getErrorMessage` 展示脱敏消息。
 *
 * @param e invoke reject 的错误对象
 * @returns 错误 code 字符串（如 "InvalidConfig" / "InvalidPath"），非结构化错误返回 null
 */
export function getErrorCode(e: unknown): string | null {
  if (typeof e === "object" && e !== null && "code" in e) {
    const code = (e as { code: unknown }).code;
    return typeof code === "string" ? code : null;
  }
  return null;
}

/**
 * F09: 带超时的 invoke 封装。
 *
 * 基于 Promise.race：原始 invoke 与一个 setTimeout 后 reject 的 Promise 竞速。
 * 超时后 reject 并附带清晰的命令名与超时时长，便于调用方通过 showStatus 提示用户。
 *
 * 注意：超时不会取消底层 invoke（Tauri IPC 不支持取消），后端命令可能继续执行完毕，
 * 但前端不再等待其结果。调用方应处理超时 reject 并提示用户。
 *
 * 超时命令选择标准：
 * - 必须包装：涉及文件 I/O、子进程创建、视频解码的命令（set_wallpaper、
 *   regenerate_thumbnails、add_wallpaper、remove_wallpaper）
 * - 无需包装：纯内存查询命令（get_wallpapers、get_config、get_wallpaper_state、get_displays、
 *   get_auto_start_status）；open_file_dialog 为用户交互阻塞命令，不应超时
 *
 * 超时时长分档（v41-F-014 文档化）：
 * - 短超时（默认 10000ms / 10s）：一般耗时命令（set_wallpaper、
 *   regenerate_thumbnails），覆盖大部分进程创建与单文件 I/O 场景
 * - 中超时（15000ms / 15s）：大文件删除命令（remove_wallpaper），预留磁盘 I/O 时间
 * - 长超时（30000ms / 30s）：资源加载命令（add_wallpaper，含文件复制 + 缩略图生成）
 *
 * 单位与常见错误（v41-F-014）：
 * - 超时参数单位为毫秒（ms），非秒；30s 必须写 30000，切勿写成 3000（3s）或 30
 * - 历史缺陷：曾有调用方将 30s 误写为 3000ms，导致大文件加载频繁超时；
 *   新增调用点时务必核对单位与量级
 *
 * 未来改进方向（v41-F-014）：
 * - 提供预设常量 `TIMEOUTS.SHORT / MEDIUM / LONG`（如 `{ SHORT: 10000, MEDIUM: 15000, LONG: 30000 }`）
 * - 调用方传 `invokeWithTimeout(cmd, args, TIMEOUTS.LONG)` 而非裸数字，避免单位混淆
 * - 可进一步结合命令类型自动选择默认超时（如基于 cmd 名查表）
 *
 * @param cmd 命令名
 * @param args 参数对象（可选）
 * @param timeout 超时时长（ms），默认 10000
 */
export async function invokeWithTimeout<T>(
  cmd: string,
  args?: Record<string, unknown>,
  timeout = 10000,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeoutPromise = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      reject(new Error(`命令 ${cmd} 超时（${timeout}ms）`));
    }, timeout);
  });
  try {
    return await Promise.race([invoke<T>(cmd, args), timeoutPromise]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

export async function setWallpaper(
  wallpaperId: string,
  displayId?: string,
  scalingMode?: ScalingMode,
): Promise<void> {
  // F09: set_wallpaper 涉及进程创建与窗口挂载，可能耗时，使用带超时的 invoke。
  // v16-C-009: Web 壁纸冷启动需初始化 WebView2 引擎（5-15s），原默认 10s 超时
  // 会在冷启动场景误判为超时。统一上调到 20s 覆盖 Web 冷启动 + 后续状态注册全流程；
  // Image/Video/Gif 类型通常 <5s 完成，20s 上限不影响正常路径。
  // v16-B-004: 新增可选 `scalingMode` 参数，未传时后端使用默认值（Fill）。
  //   仅当非 undefined 时序列化到 args，避免传 undefined（被 JSON.stringify 丢弃
  //   也无副作用，但显式过滤更清晰且与后端 Option<ScalingMode> 对齐）。
  // v16-B-005: 后端形参重命名为 `wallpaper_id`，Tauri v2 自动 camelCase → snake_case。
  const args: Record<string, unknown> = {
    wallpaperId,
    displayId: toDisplayIdArg(displayId),
  };
  if (scalingMode !== undefined) {
    args.scalingMode = scalingMode;
  }
  await invokeWithTimeout<void>("set_wallpaper", args, 20000);
}

export async function pauseWallpaper(displayId?: string): Promise<void> {
  await invoke<void>("pause_wallpaper", { displayId: toDisplayIdArg(displayId) });
}

export async function resumeWallpaper(displayId?: string): Promise<void> {
  await invoke<void>("resume_wallpaper", { displayId: toDisplayIdArg(displayId) });
}

export async function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("get_config");
}

export async function updateConfig(config: AppConfig): Promise<void> {
  await invoke<void>("update_config", { config });
}

export async function setVolume(displayId: string | null | undefined, volume: number): Promise<void> {
  await invoke<void>("set_volume", { displayId: toDisplayIdArg(displayId), volume });
}

export async function toggleMute(displayId: string | null | undefined): Promise<boolean> {
  return invoke<boolean>("toggle_mute", { displayId: toDisplayIdArg(displayId) });
}

export async function setSpeed(displayId: string | null | undefined, speed: number): Promise<void> {
  await invoke<void>("set_speed", { displayId: toDisplayIdArg(displayId), speed });
}

export async function getWallpaperState(displayId: string | null | undefined): Promise<WallpaperState | null> {
  return invoke<WallpaperState | null>("get_wallpaper_state", { displayId: toDisplayIdArg(displayId) });
}

export async function setInteractionMode(enabled: boolean): Promise<void> {
  await invoke<void>("set_interaction_mode", { enabled });
}

export async function getDisplays(): Promise<DisplayInfo[]> {
  return invoke<DisplayInfo[]>("get_displays");
}

export async function setScalingMode(displayId: string | null | undefined, mode: ScalingMode): Promise<void> {
  await invoke<void>("set_scaling_mode", { displayId: toDisplayIdArg(displayId), mode });
}

export async function toggleAutoStart(enabled: boolean): Promise<void> {
  await invoke<void>("toggle_auto_start", { enabled });
}

export async function openFileDialog(): Promise<string | null> {
  const result = await invoke<string | null>("open_file_dialog");
  return result;
}

export async function getAutoStartStatus(): Promise<boolean> {
  return invoke<boolean>("get_auto_start_status");
}

/**
 * v16-C-007：查询桌面状态并在必要时重新初始化 WorkerW。
 *
 * 供前端在收到 `desktop-status-changed`（payload `ok: false`）事件后启动
 * 2s 间隔轮询使用，30s 超时后停止并提示用户重启应用（见 main.ts 监听器）。
 *
 * 返回值语义：
 * - true：WorkerW 已失效并已成功重新初始化（壁纸需重新嵌入）
 * - false：WorkerW 当前有效，无需重初始化
 */
export async function checkDesktopStatus(): Promise<boolean> {
  return invoke<boolean>("check_desktop_status");
}

export interface RegenerateResult {
  total: number;
  success: number;
  failed: number;
}

export async function regenerateThumbnails(): Promise<RegenerateResult> {
  // F09: 批量重新生成缩略图可能很慢（取决于壁纸数量），使用带超时的 invoke
  return invokeWithTimeout<RegenerateResult>("regenerate_thumbnails");
}

/**
 * F04: 后端错误敏感信息脱敏规则表。
 *
 * 仅匹配稳定的、可能泄露内部细节的模式（文件路径、Win32/HRESULT 错误码、堆栈行），
 * 不会误伤后端主动抛出的简单错误描述（如 "add failed"、"timeout"）。
 *
 * v16-C-005 改进：
 * - 路径类错误保留 basename（文件名）而非替换为 `<path>`，既避免泄露目录结构
 *   又让用户能定位是哪个文件（例：`C:\Users\test\file.mp4` → `file.mp4`）。
 * - `os error N` / `WinError(N)` / `errno(N)` 等系统错误码映射为中文描述
 *   （os error 5 → "权限不足"），未命中映射表的错误码回退为 `<error-code>` 占位符。
 *
 * 替换为占位符后仅展示概要信息，原始细节由调用方通过 log.error 记录。
 */
const SENSITIVE_PATTERNS: ReadonlyArray<{
  pattern: RegExp;
  replacement: (substring: string, ...args: string[]) => string;
}> = [
  // Windows 盘符路径（C:\Users\... 或 C:/Users/...）及 Unix 绝对路径（/home/...）
  // v16-C-005：保留 basename 而非替换为 <path>，让用户能定位文件
  {
    pattern: /(?:[A-Za-z]:[\\/]|[\\/])[^\s'"<>]*\.\w+/g,
    replacement: (match: string) => extractBasename(match),
  },
  // Win32/HRESULT 错误码：0x80004005、0x00000005 等
  { pattern: /0x[0-9A-Fa-f]{8}/g, replacement: () => "<error-code>" },
  // Rust io::Error Display 常见格式 "os error N"（无括号，如 "Access is denied. (os error 5)"）
  // v16-C-005：映射为中文描述，未命中错误码表则替换为 <error-code> 占位符
  {
    pattern: /os error (\d+)/gi,
    replacement: (_match: string, code: string) => mapOsErrorCode(code),
  },
  // WinError(N) / errno(N) / OS error (N) 等带括号形式
  {
    pattern: /(?:WinError|errno|OS error)\s*\(\s*(\d+)\s*\)/gi,
    replacement: (_match: string, code: string) => mapOsErrorCode(code),
  },
  // 堆栈行：以空白 + at + 函数/路径信息开头（常见于 Error.stack 输出）
  { pattern: /\s+at\s+[^\n]+/g, replacement: () => "" },
];

/** F04: 错误消息展示长度上限，避免长堆栈或大段上下文通过 UI 泄露 */
const MAX_ERROR_MESSAGE_LENGTH = 200;

/**
 * F04 v16-C-005：常见 OS 错误码 → 中文映射表。
 *
 * 用于将 Rust `io::Error` Display 输出中的 "os error N" / "WinError(N)" 等
 * 技术性错误码转换为用户可理解的中文描述，避免 `<error-code>` 占位符
 * 对用户无意义。未在表中的错误码回退为 `<error-code>` 占位符。
 *
 * 覆盖 Windows 常见错误码（5/32/122）与 POSIX 标准错误码（1/2/13/28）。
 */
const OS_ERROR_CODE_MESSAGES: Readonly<Record<number, string>> = {
  1: "权限不足", // EPERM：操作不允许
  2: "文件不存在", // ENOENT：文件或目录不存在
  5: "权限不足", // EACCES（Windows Access denied）
  13: "权限不足", // EACCES（POSIX）
  28: "磁盘空间不足", // ENOSPC：磁盘满
  32: "文件被占用", // ESHARING（Windows sharing violation）
  122: "磁盘空间不足", // EDQUOT：配额超限
};

/**
 * v16-C-005：将 OS 错误码映射为中文描述，未命中返回 `<error-code>` 占位符。
 */
function mapOsErrorCode(codeStr: string): string {
  const code = Number.parseInt(codeStr, 10);
  return OS_ERROR_CODE_MESSAGES[code] ?? "<error-code>";
}

/**
 * v16-C-005：从完整路径中提取 basename（文件名）。
 *
 * 保留文件名让用户能定位具体文件，同时避免泄露完整目录结构。
 * 同时处理 `\` 和 `/` 分隔符（Windows 路径可能混用）。
 *
 * 例：`C:\Users\test\file.mp4` → `file.mp4`
 *     `/home/user/vid.mp4` → `vid.mp4`
 *     `C:/Users/test/file.mp4` → `file.mp4`
 */
function extractBasename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? "<path>";
}

/**
 * F04: 对错误消息做脱敏处理。
 *
 * 移除文件路径、Win32/HRESULT 错误码、堆栈行等敏感细节，
 * 并截断过长消息，仅保留概要信息供用户查看。
 *
 * v16-C-005：路径保留 basename、OS 错误码映射为中文，提升可操作性。
 */
function sanitizeErrorMessage(message: string): string {
  let sanitized = message;
  for (const { pattern, replacement } of SENSITIVE_PATTERNS) {
    sanitized = sanitized.replace(pattern, replacement);
  }
  if (sanitized.length > MAX_ERROR_MESSAGE_LENGTH) {
    sanitized = sanitized.slice(0, MAX_ERROR_MESSAGE_LENGTH) + "...";
  }
  return sanitized;
}

/**
 * 从错误对象中提取可读消息，兼容 string 和 { code, message } 格式。
 *
 * F04: 对提取的消息做脱敏处理，避免向后端或 UI 泄露文件路径、错误码、堆栈等内部细节。
 * 原始错误对象由调用方通过 log.error 记录，仅展示脱敏后的概要信息给用户。
 */
export function getErrorMessage(e: unknown): string {
  let raw: string;
  if (typeof e === "object" && e !== null && "message" in e) {
    raw = String((e as { message: unknown }).message);
  } else {
    raw = String(e);
  }
  return sanitizeErrorMessage(raw);
}
