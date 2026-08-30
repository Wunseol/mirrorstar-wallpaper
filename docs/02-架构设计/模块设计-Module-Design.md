[← 返回文档索引](../../README.md) > [架构设计](./架构概述-Architecture-Overview.md) > 模块设计

# 模块设计

> 基于真实代码审计 + Lively 架构对比

| 项目   | 内容                        |
| ---- | ------------------------- |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

本文档描述 MirrorStar Wallpaper（镜星壁纸）的模块划分、各模块详细状态，以及与参考项目 Lively Wallpaper 的核心模块对比。所有数据均基于真实代码审计，反映当前实现状态。

---

## 1. 模块总览

| # | 模块 | 路径 | 状态 | 完成度 | 复杂度 |
|---|------|------|------|--------|--------|
| 1 | config (配置管理) | `crates/mirrorstar-core/src/config/` | ✅ 已实现 | 100% | 低 |
| 2 | desktop (桌面集成) | `crates/mirrorstar-core/src/desktop/` | ✅ 已实现 | 100% | 高 |
| 3 | wallpaper (壁纸引擎) | `crates/mirrorstar-core/src/wallpaper/` | ✅ 已实现 | 100% | 高 |
| 4 | input (输入控制) | `crates/mirrorstar-core/src/input/` | ❌ 已删除 | - | - |
| 5 | audio (音频控制) | `crates/mirrorstar-core/src/audio/` | ✅ 已实现 | 95% | 中 |
| 6 | ipc (进程间通信) | `crates/mirrorstar-core/src/ipc/` | ✅ 已实现 | 100% | 中 |
| 7 | process (进程管理) | `crates/mirrorstar-core/src/process/` | ✅ 已实现 | 95% | 中 |
| 8 | Tauri 应用层 | `src-tauri/src/` | ✅ 已实现 | 95% | 中 |
| 9 | 前端 UI | `src/scripts/` + `index.html` | ✅ 已实现 | 95% | 中 |
| 10 | mirrorstar-wp-proc | `crates/mirrorstar-wp-proc/` | ✅ 已实现 | 100% | 高 |

> **架构决策**：采用混合进程架构——仅 Web 壁纸运行在独立子进程（mirrorstar-wp-proc），按需创建；Image/Gif/Video 渲染器保留在主进程内。mirrorstar-watchdog 独立进程已在阶段2移除（空壳 crate 已删除）。
>
> **注意**：原文档中提到的 "tray (系统托盘)" core 模块不存在，已移除。托盘功能在 Tauri 应用层内联实现（使用 Tauri 的 `TrayIconBuilder`），并非独立模块。

### 混合进程架构示意

```
┌──────────────────────────────────────────────────────────┐
│              mirrorstar-wallpaper.exe（主进程）             │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
│  │ Tauri    │ │ 图片线程  │ │ GIF 线程  │ │ 视频线程   │  │
│  │ 主线程   │ │ (GetMsg) │ │ (Timer)  │ │ (mpv IPC) │  │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────────────┐  │
│  │ 全屏检测 │ │ 配置监视 │ │ WebRenderer (代理层)     │  │
│  │ (Hook)   │ │ (notify) │ │  ├ ProcessManager       │  │
│  └──────────┘ └──────────┘ │  └ WpProcIpcClient       │  │
│                              └──────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
       │                              │
       │ CreateProcessW               │ CreateProcessW + 命名管道 IPC
       ▼                              ▼
┌─────────────────┐         ┌──────────────────────────┐
│   mpv.exe       │         │ mirrorstar-wp-proc.exe    │
│   (视频壁纸)    │         │ (Web 壁纸子进程)          │
│   按需启动      │         │  ┌────────────────────┐  │
└─────────────────┘         │  │ WebView2 环境      │  │
                            │  │  └ 窗口 + 消息循环  │  │
                            │  └────────────────────┘  │
                            └──────────────────────────┘
                               ↑ 仅 Web 壁纸时启动
                               ↑ 关闭 Web 壁纸时终止
```

**设计原则：**
- 主进程（`mirrorstar-wallpaper.exe`）内通过 WallpaperEngine 管理所有壁纸渲染
- Image/Gif 渲染器在主进程专用线程中运行（GDI + 双缓冲）
- Video 渲染器通过 ProcessManager 直接 spawn `mpv.exe` 子进程（按需），通过 mpv 原生命名管道 IPC（`MpvIpcClient`，定义于 `ipc/mpv_protocol.rs`）控制，**不使用 libmpv2 FFI 库嵌入**
- **Web 渲染器已重构为代理层**：通过 ProcessManager spawn `mirrorstar-wp-proc.exe` 子进程（按需），通过 `WpProcIpcClient`（定义于 `ipc/wp_proc.rs`）命名管道通信
- HWND 通过 FindWindowW 获取后，主进程执行 WorkerW 嵌入（与 mpv 窗口嵌入逻辑一致）

**内存优化：**
- 无 Web 壁纸时：零子进程开销（仅主进程 + 可选的 mpv）
- WebView2 运行时仅加载在子进程中，主进程不加载 webview2-com
- 子进程在 Web 壁纸关闭时立即终止，释放全部内存
- 无独立看门狗进程（mirrorstar-watchdog 已在阶段2移除）

---

## 2. 各模块详细状态

### 2.1 config (配置管理) — ✅ 已实现 100%

**代码规模：** 5801 行代码，7 个文件（detect.rs 738 / hot_reload.rs 549 / manager.rs 2533 / mod.rs 45 / settings.rs 735 / thumbnail.rs 1168 / validation.rs 33）

**已实现：**
- AppConfig 完整结构（6 个子配置：General/Audio/Pause/Display/Video/Web）
- ConfigManager 全功能 CRUD：加载、保存、更新配置
- 原子文件写入 + fs2 文件锁
- 热重载文件监视（notify crate，500ms 防抖）— 已启用 `start_watching()`
- WallpaperLibrary 壁纸库管理（添加/删除/列表/更新缩略图）
- 文件类型自动检测（Video/Gif/Web/Image）
- 缩略图生成（image crate，320x180，JPEG 质量 85）
- 配置防抖保存（300ms）
- 19 个单元测试

---

### 2.2 desktop (桌面集成) — ✅ 已实现 100%

**代码规模：** 2662 行代码，4 个文件（mod.rs 948 / native_wallpaper.rs 547 / window.rs 257 / worker_w.rs 910）

**已实现：**
- WorkerW 查找三重策略：直接 EnumWindows 查找 → 发送 0x052C 消息触发创建 → 5 次重试（50ms 间隔）→ 备用回调方法
- 壁纸窗口嵌入（SetParent + SetWindowPos HWND_BOTTOM + ShowWindow）
- Explorer 重启检测与恢复（TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底，调用 `check_and_reinitialize` 重建）
- 原始系统壁纸保存与恢复（SystemParametersInfoW）
- 窗口操作（无边框 `make_borderless`、移除任务栏 `remove_from_taskbar`、鼠标穿透 `set_mouse_passthrough`）
- 显示器枚举 `enumerate_displays()`（EnumDisplayMonitors + GetMonitorInfoW，返回分辨率、位置、DPI、主显示器标志）
- **原生壁纸 API**（`native_wallpaper.rs`）：JPG/PNG 等静态图片通过 SystemParametersInfoW + 注册表设置，零资源占用
- WallpaperMode 双路径：Native（原生 API）和 WorkerW（窗口嵌入）

---

### 2.3 wallpaper (壁纸引擎) — ✅ 已实现 100%

**代码规模：** 16162 行代码，13 个文件（fast_path.rs 1316 / gdi_base.rs 860 / gdi_cache.rs 192 / gif.rs 2172 / gif_decode.rs 2689 / gif_memory.rs 808 / image.rs 1104 / manager.rs 2591 / mod.rs 1511 / mode_dispatch.rs 514 / subprocess_base.rs 494 / video.rs 1259 / web.rs 652）

**已实现：**

**核心抽象：**
- WallpaperRenderer trait 完整生命周期定义（play/pause/resume/set_volume/set_position/terminate/hwnd/state/set_speed/navigate/set_scaling_mode/set_mouse_passthrough/set_interaction_mode/create_pause_sender）
- WallpaperEngine 管理器（2591 行，manager.rs）：set/close/pause/resume/shutdown + 快速路径控制
- **PauseSender 快速通道**：绕过引擎互斥锁，直接发送暂停/恢复/音量命令
- 5 种缩放模式（Fill/Fit/Stretch/Center/Original）
- WallpaperMode 双路径（Native/WorkerW）
- `update_positions` 多显示器位置更新（span 和 per_monitor）
- **子进程退出监听**：仅存在 `spawn_proc_exit_monitor`（`wallpaper/mod.rs` L186）退出监听，监听子进程状态，异常退出时更新为 `Terminated` 状态并 `notify_state_changed` 通知引擎；**无自动 respawn（无自动重启逻辑），异常后的恢复依赖用户或后续交互触发**
- 0 个单元测试（无单元测试）

**四种渲染器：**
- **ImageRenderer**（1104 行）：GDI + 双缓冲 + HALFTONE + 专用线程消息循环 + 超屏幕分辨率降采样 + 暂停时释放像素数据
- **GifRenderer**（2172 行）：image crate GIF 解码 + GDI 双缓冲 + WM_TIMER 帧驱动 + 40MB 内存预算限制 + 暂停时释放帧数据 + 速度控制
- **VideoRenderer**（1259 行）：mpv 子进程 + 命名管道 IPC + WASAPI 音量控制 + 7 个单元测试
- **WebRenderer**（652 行，✅ 代理层）：通过 ProcessManager 启动 `mirrorstar-wp-proc.exe` 子进程 + WpProcIpcClient 命名管道 IPC 通信 + FindWindowW + PID 验证获取 HWND，实现崩溃隔离和内存优化

---

### 2.4 input (输入控制) — ❌ 已删除（C-027 死代码清理）

> 该模块已于代码评审 cycle4a 中作为死代码删除。原 `input/mouse.rs`（MouseHandler 结构 + 8 个测试）在生产代码无任何调用方，实际交互模式切换由 `wallpaper/manager.rs` 直接调用 renderer 的 `set_interaction_mode` 方法处理，完全绕过 MouseHandler。原 `input/` 目录及其 `mod.rs` 一并移除。

---

### 2.5 audio (音频控制) — ✅ 已实现 95%

**代码规模：** 1042 行代码，2 个文件（mod.rs 1 / volume.rs 1041）

**已实现：**
- VolumeControl WASAPI 进程级音量控制
- COM 接口缓存（IMMDeviceEnumerator + IAudioSessionManager2）
- `set_process_volume` / `set_process_mute` / `get_process_volume` / `get_process_mute`
- `refresh_session_manager` 音频设备变更处理 — ⚠️ **已实现但未接入调用链（死代码）**：函数在 `crates/mirrorstar-core/src/audio/volume.rs:92` 已定义并实现（重新创建 session manager + 清空 session_cache），但通过全项目搜索确认生产代码中从未被调用，仅在 `volume.rs` 自身的单元测试中被引用。当前未在 `src-tauri/src/platform/` 中接入音频设备变更监听，待未来启用。

**缺失：**
- 无单元测试（COM 环境限制）
- `refresh_session_manager` 调用链接入（音频设备变更监听未实现）

---

### 2.6 ipc (进程间通信) — ✅ 已实现 100%

**代码规模：** 2103 行代码，4 个文件（client.rs 1021 / mod.rs 28 / mpv_protocol.rs 355 / wp_proc.rs 699）

> **重要说明**：项目中存在**两套独立的 IPC 协议**，并非统一协议：
> - **mpv 原生 IPC**（管道名 `mirrorstar-mpv-{uuid}`）：由 `MpvIpcClient` 实现（定义于 `ipc/mpv_protocol.rs` L27），用于主进程与 mpv.exe 子进程通信
> - **wp-proc WpProcCommand 协议**：由 `WpProcIpcClient` 实现（定义于 `ipc/wp_proc.rs` L73），用于主进程与 mirrorstar-wp-proc.exe 子进程通信
>
> 两者均采用命名管道 + JSON + 换行分隔的传输格式，并复用连接重试 + request_id 响应匹配模式，但命令集与协议语义各自独立。**注意：不存在 `ipc/protocol.rs` 文件**，协议定义位于 `mpv_protocol.rs` 与 `wp_proc.rs` 中。

**已实现：**
- MpvIpcClient mpv 命名管道 IPC 客户端（mpv_protocol.rs）
- WpProcIpcClient Web 壁纸子进程 IPC 客户端（wp_proc.rs，复用 MpvIpcClient 的连接重试 + request_id 模式）
- 连接重试机制（详见下方重试参数）：mpv `MPV_CONNECT_RETRIES` = 40 × 50ms = 2s；wp-proc `WP_PROC_CONNECT_RETRIES` = 160 × 50ms = 8s；窗口查找重试 = 20 × 100ms = 2s（均位于 `wallpaper/subprocess_base.rs`）
- `send_command` 命令发送 + request_id 响应匹配
- `get_property` / `set_property` 属性读写
- `pause` / `resume` / `set_volume` / `set_loop_file` / `set_speed` / `quit` 控制命令（wp-proc 另有 play/terminate/set_position/navigate）
- 9 + 17 个单元测试

---

### 2.7 process (进程管理) — ✅ 已实现 95%

**代码规模：** 1205 行代码，2 个文件（manager.rs 1204 / mod.rs 1，阶段2清理后）

**已实现：**
- ProcessManager（1204 行）：CreateProcessW 启动子进程 + 3 秒等待 + TerminateProcess 强制终止
- `is_running()` / `pid()` / `handle()` 进程状态查询

**阶段2清理：**
- 移除 `watchdog.rs`（136 行死代码，Watchdog 结构体从未被 src-tauri 引用）
- 移除 `monitor.rs`（30 行死代码，ProcessMonitor 从未被使用）
- 移除 `mirrorstar-watchdog` 独立 crate（空壳，不再需要独立看门狗进程）

> **注意**：实际架构中不存在 `ProcessMonitor` 结构体，也不存在 `PauseReason` bitflags 组合状态机。全屏暂停状态通过 `AtomicBool`（`FULLSCREEN_WAS`）记录上一次状态实现去抖，电池暂停状态由独立的 bool 跟踪，二者并非位掩码组合。详见 [2.8 Tauri 应用层](#28-tauri-应用层--已实现-95) 与 [暂停恢复机制](./暂停恢复机制-Pause-Resume.md)。

---

### 2.8 Tauri 应用层 — ✅ 已实现 95%

**代码规模：** 12 个文件——`lib.rs`（1220 行）+ `main.rs`（57 行）+ `state.rs`（1031 行）+ `commands/`（4 个文件：config.rs 152/system.rs 226/wallpaper.rs 2285/mod.rs 100）+ `platform/`（5 个文件：explorer.rs 271/fullscreen.rs 1145/power.rs 222/workerw_check.rs 236/mod.rs 7），共 6952 行

**已实现：**
- **24 个 Tauri 命令全部完整实现**（无空操作），均为 `#[tauri::command]` 注解命令；托盘菜单的"暂停/恢复壁纸"复用 `pause_wallpaper`/`resume_wallpaper` 命令，并非独立命令：
  `get_wallpapers`, `add_wallpaper`, `generate_thumbnail`, `regenerate_thumbnails`, `remove_wallpaper`, `set_wallpaper`, `pause_wallpaper`, `resume_wallpaper`, `get_config`, `update_config`, `set_volume`, `toggle_mute`, `set_interaction_mode`, `toggle_interaction`, `get_displays`, `get_wallpaper_state`, `open_file_dialog`, `toggle_auto_start`, `get_auto_start_status`, `set_scaling_mode`, `set_speed`, `update_positions`, `check_desktop_status`, `quit_app`
- 系统托盘（**在 lib.rs setup 中构建（state.rs 管理状态），菜单仅 3 项：打开主窗口 / 暂停-恢复壁纸 / 退出；无音量子菜单、无显示器子菜单**，托盘菜单项定义于 lib.rs L967-976）+ 点击图标显示窗口
- 单实例保护（Win32 CreateMutexW）
- Explorer 重启后台检测（TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底；`start_workerw_check` 使用 tokio interval 300 秒 + Notify 唤醒，失效时调用 `check_and_reinitialize` 重建，**无 30 秒轮询**）
- 全屏应用检测（**纯 SetWinEventHook(EVENT_SYSTEM_FOREGROUND) 事件驱动** + 状态去抖 + 自身窗口排除；**无 2 秒轮询回退**，Hook 失败仅记录并退出监控线程）— **全局暂停/恢复**（`pause_all_fast`），**非按显示器粒度**
- 退出时壁纸恢复（`perform_shutdown`）
- 配置热重载已启用
- 主窗口懒创建 + 关闭即隐藏（hide）释放 WebView2 内存
- PerMonitorV2 DPI 感知
- **COM STA 初始化**：主进程显式调用 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA），wp-proc 子进程同样使用 STA
- 4 个前端事件 emit（wallpaper-added/updated/removed/state-changed）
- PauseSender 快速路径（绕过引擎互斥锁）
- 2 个插件（tauri-plugin-dialog, tauri-plugin-autostart）

**轻微问题（已修复）：**
- main.rs 中 tracing 在 logging 初始化前调用 — **已修复**（main.rs 已无 tracing 调用，仅 `ensure_single_instance` + `run()`）
- 2 个未使用的 OnceLock（FULLSCREEN_ENGINE, FULLSCREEN_RT） — **已修复**（OnceLock 已移除，全局静态量经 Task 9.2 评估后收敛为 `SHARED_ENGINE` / `SHARED_CONFIG` 等，详见 state.rs）

---

### 2.9 前端 UI — ✅ 已实现 95%

**代码规模：** 13 个 TS 文件（共 1523 行）+ 546 行 main.css + 114 行 index.html

**模块化结构（`src/scripts/` 下）：**

| 层级 | 文件 | 行数 | 职责 |
|------|------|------|------|
| 根目录 | `main.ts` | 425 | 入口与启动 |
| 根目录 | `ipc.ts` | 160 | 20 个 Tauri invoke 命令封装 |
| 根目录 | `state.ts` | 29 | 全局状态 |
| 根目录 | `types.ts` | 85 | TypeScript 类型定义 |
| `ui/` | `wallpaper-list.ts` | 249 | 壁纸库网格 + 卡片 + 搜索 + 加载骨架屏 |
| `ui/` | `preview-modal.ts` | 201 | 壁纸预览模态框（点击放大 + 设为壁纸） |
| `ui/` | `drag-drop.ts` | 96 | 拖拽添加壁纸（含视觉反馈） |
| `ui/` | `config-panel.ts` | 83 | 设置面板（音量/自启动/全屏暂停/电池暂停） |
| `ui/` | `utils.ts` | 77 | UI 工具函数 |
| `ui/` | `display-list.ts` | 51 | 显示器选择下拉框 |
| `ui/` | `mod.ts` | 18 | UI 模块导出 |
| `utils/` | `listeners.ts` | 34 | 事件监听（含 wallpaper-updated/state-changed） |
| `utils/` | `logger.ts` | 15 | 前端日志 |

**已实现：**
- 壁纸库网格 + 卡片布局
- 拖拽添加壁纸（Tauri 原生 onDragDropEvent）+ 拖拽视觉反馈
- 文件选择添加壁纸
- 设置面板（音量滑块/自启动/全屏暂停/电池供电暂停）
- 显示器选择下拉框（主显示器标记 [主]）
- 排列模式选择器（per_monitor/span）
- 缩放模式选择器（fill/fit/stretch/center/original）
- Toast 消息提示
- 壁纸缩略图显示（convertFileSrc + 懒加载 + 渐显）
- 类型徽章（Image/Video/Gif/Web 不同颜色）
- 自动为 Image 类型生成缩略图
- 20 个 invoke 命令封装
- 完整的 TypeScript 类型定义
- 深色主题设计
- ✅ 壁纸预览（点击放大模态框，含"设为壁纸"按钮）
- ✅ 壁纸搜索（按文件名实时过滤）
- ✅ 响应式设计（媒体查询适配窄屏 < 768px）
- ✅ 加载动画（骨架屏 + 缩略图渐显）
- ✅ 关于页面（动态版本号 + 技术栈）
- ✅ 电池供电暂停 UI（后端 GetSystemPowerStatus + 前端开关）
- ✅ 事件监听（wallpaper-updated / wallpaper-state-changed）
- ✅ 删除壁纸确认对话框

**缺失：**
- 手动暂停/恢复 UI 按钮（后端有命令，前端无 UI）
- 播放速度控制 UI（后端 mpv 支持，前端无 UI）
- 鼠标穿透/交互模式切换 UI（后端有命令，前端无 UI）
- 删除源文件选项（deleteFile 硬编码 false）
- 静音按钮 UI

---

### 2.10 mirrorstar-wp-proc — ✅ 已实现 100%（Web 壁纸子进程）

**状态：** 完整实现（5 个文件，4588 行：com.rs 103 / command.rs 1145 / ipc_server.rs 1466 / main.rs 422 / webview.rs 1452），CLI 参数 + 命名管道服务端 + IPC 线程 + WebView2 渲染 + 消息循环 + 命令处理

**实现要点：**
- clap CLI 参数：`--source`（初始网页源）、`--pipe-name`（管道名称）、`--title`（窗口标题）、`--rect`（初始位置）
- 命名管道服务端：`CreateNamedPipeW` + `ConnectNamedPipe` + `BufReader`/`BufWriter`
- IPC 线程：读取 JSON 命令行 → 反序列化 `WpProcCommand` → mpsc 发送到主线程 → `PostMessageW` 唤醒 → 等待响应 → 序列化写回管道
- WebView2 环境：`CreateCoreWebView2Environment` + `CreateCoreWebView2Controller` + URL 导航（支持 http/https 和 file://）
- 窗口类注册：固定类名 `MirrorStarWebWallpaperCls`，窗口标题使用 `--title` 参数（用于 `FindWindowW` 查找）
- 主消息循环：`GetMessageW` + `WM_WEB_COMMAND`（`WM_USER + 20`）处理
- 命令处理：Play（导航到源）、Terminate（销毁窗口+退出）、SetPosition（`SetWindowPos` + `SetBounds`）、Navigate（`webview.Navigate`）、Pause/Resume（状态标记）
- **COM 初始化**：`CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA，与主进程一致）
- **崩溃隔离**：子进程异常退出时，主进程 `wallpaper/mod.rs` 的 `spawn_proc_exit_monitor` 会检测到退出并把状态更新为 `Terminated` 并 `notify_state_changed` 通知引擎，**无自动重启逻辑**

**IPC 协议（JSON + 换行分隔，与 mpv IPC 传输格式一致）：**

命令（主进程 → 子进程）：
```json
{"command":"play","request_id":1,"source":"https://..."}
{"command":"terminate","request_id":2}
{"command":"set_position","request_id":3,"x":0,"y":0,"width":1920,"height":1080}
{"command":"navigate","request_id":4,"url":"https://..."}
{"command":"pause","request_id":5}
{"command":"resume","request_id":6}
```

响应（子进程 → 主进程）：
```json
{"request_id":1,"status":"ok"}
{"request_id":1,"status":"error","error":"WebView2 未初始化"}
```

---

## 3. 核心模块与 Lively 对比

以下对比基于真实代码审计，系统化呈现 MirrorStar 与参考项目 Lively Wallpaper 在各核心模块上的设计差异。

### 3.1 桌面集成（WorkerW 嵌入）对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `desktop/` 目录（4 个文件） | `wp_lib/SetupDesktop.cs` |
| **代码行数** | 2662 行（mod.rs 948、window.rs 257、worker_w.rs 910、native_wallpaper.rs 547） | 2788 行（1 文件） |
| **WorkerW 查找** | 三重策略：直接 EnumWindows → 发送 0x052C 消息 → 5 次重试 → 备用回调 | 发消息触发 → EnumWindows 查找 |
| **超时策略** | SendMessageTimeoutW 200ms + 5次重试(50ms间隔) | SendMessageTimeout 2s + 固定等待 |
| **原生壁纸 API** | SystemParametersInfoW + 注册表设置（零资源占用） | 无 |
| **WallpaperMode 双路径** | Native（原生 API，静态图片）+ WorkerW（窗口嵌入，动态内容） | 仅 WorkerW 嵌入 |
| **窗口嵌入** | SetParent → SetWindowPos(HWND_BOTTOM) | SetParent → SetWindowPos |
| **坐标转换** | 直接使用屏幕坐标 | 通过 Win32 坐标转换 API 转换到 WorkerW 客户区坐标 |
| **Win7 兼容** | 未特殊处理 | 区分 Win7（Progman 嵌入）和 Win10+（WorkerW 嵌入） |
| **高对比度模式** | 未处理 | 检测并使用 bottom-most 渲染模式 |
| **Explorer 重启** | TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底（check_and_reinitialize） | 无特殊处理 |
| **显示器枚举** | EnumDisplayMonitors + GetMonitorInfoW（完整实现） | System.Windows.Forms.Screen.AllScreens |
| **窗口操作** | make_borderless、remove_from_taskbar、set_mouse_passthrough | SetParent + SetWindowPos |

**关键差异：**

1. **原生壁纸 API 创新**：MirrorStar 实现了 `native_wallpaper.rs`（547 行），对 JPG/PNG 等静态图片通过 `SystemParametersInfoW` + 注册表设置直接使用系统壁纸 API，零资源占用。这是 MirrorStar 的独创点，Lively 无此优化。

2. **WallpaperMode 双路径**：MirrorStar 区分 Native（原生 API，适用于静态图片）和 WorkerW（窗口嵌入，适用于动态内容）两种路径，根据壁纸类型自动选择最优方案。Lively 仅使用 WorkerW 嵌入方式。

3. **坐标系统**：Lively 使用 Win32 坐标转换 API 将屏幕坐标转换为 WorkerW 客户区坐标后定位壁纸窗口，这是更准确的做法。MirrorStar 直接使用屏幕坐标 + `SetWindowPos`，在大多数情况下有效，但在某些 DPI/多显示器配置下可能存在偏差。

4. **Win7 兼容**：Lively 区分 Win7 和 Win10+ 的嵌入策略——Win7 直接嵌入 Progman，Win10+ 嵌入 WorkerW。MirrorStar 仅支持 WorkerW 方式，不支持 Win7。

5. **高对比度模式**：Lively 检测系统高对比度模式并切换到 bottom-most 渲染（不嵌入 WorkerW，而是将壁纸窗口置于最底层），这是一个重要的无障碍功能。MirrorStar 未处理此场景。

6. **代码复杂度**：Lively 的 SetupDesktop.cs 有 2788 行，承担了过多职责（壁纸创建、定位、关闭、进程监控、CefSharp 消息处理等）。MirrorStar 将这些职责拆分到 4 个文件中（mod.rs、window.rs、worker_w.rs、native_wallpaper.rs），更符合单一职责原则。

### 3.2 壁纸渲染对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `wallpaper/` 目录（13 个文件） | 多个 WPBaseClass 子类 |
| **代码行数** | 16162 行 | 未统计（分散在多个类中） |
| **架构模式** | 策略模式（WallpaperRenderer trait），4 种渲染器统一接口 | 类型分支（if/switch）+ 类继承体系（WPBaseClass） |
| **图片渲染** | ImageRenderer（1104 行）：GDI 双缓冲 + HALFTONE + 专用线程 + 超屏幕降采样 + 暂停释放像素 | WPF Image 控件 |
| **GIF 渲染** | GifRenderer（2172 行）：image crate 解码 + GDI 双缓冲 + WM_TIMER + 40MB 内存预算 + 暂停释放帧 + 速度控制 | XamlAnimatedGIF 库 |
| **视频渲染** | VideoRenderer（1259 行）：mpv 子进程 + 命名管道 IPC + WASAPI 音量 + find_mpv 捆绑/PATH 回退 | WPF MediaElement / WPFMediaKit (DirectShow) |
| **Web 渲染** | WebRenderer（652 行，✅ 代理层）：通过 ProcessManager 启动 mirrorstar-wp-proc.exe 子进程 + WpProcIpcClient 命名管道 IPC 通信 + FindWindowW + PID 验证获取 HWND | CefSharp（独立子进程） |
| **引擎层** | WallpaperEngine（2591 行，manager.rs）：set/close/pause/resume/shutdown + 快速路径控制 | WPBaseClass 基类 |
| **缩放模式** | 5种（Fill/Fit/Stretch/Center/Original） | WPF Stretch 枚举 |
| **降采样** | 超屏幕分辨率自动降采样 | 无 |
| **帧数限制** | GIF 40MB 内存预算 | 无 |
| **快速通道** | PauseSender 快速通道绕过引擎互斥锁 | 无 |
| **多显示器** | update_positions() 重新计算位置 | SystemEvents.DisplaySettingsChanged 事件 |

**关键差异：**

1. **渲染架构**：MirrorStar 使用策略模式（`WallpaperRenderer` trait），4 种渲染器（ImageRenderer 990行、GifRenderer 2003行、VideoRenderer 1152行、WebRenderer 580行）实现统一接口，引擎层（WallpaperEngine 2387行，manager.rs）通过 `Box<dyn WallpaperRenderer>` 多态调度。Lively 使用 if/switch 分支 + 类继承体系（WPBaseClass），每种类型有不同的处理逻辑，代码重复较多。

2. **PauseSender 快速通道创新**：MirrorStar 实现了 PauseSender 快速通道，绕过引擎互斥锁，直接发送暂停/恢复/音量命令。这避免了高优先级操作（如全屏暂停）被引擎锁阻塞，显著降低响应延迟。Lively 无此机制。

3. **GIF 内存预算**：MirrorStar 对 GIF 实现了 40MB 内存预算控制 + 暂停时释放帧 + 速度控制，Lively 无此优化。对于高分辨率长 GIF，MirrorStar 的内存占用更可控。

4. **图片降采样**：MirrorStar 对超屏幕分辨率的图片自动降采样，并在暂停时释放像素数据。Lively 无此优化。

5. **视频播放方案**：这是最大的架构差异。Lively 使用 WPF 内置的 MediaElement（基于 Windows Media Foundation）或 WPFMediaKit（基于 DirectShow），播放器窗口直接是 WPF Window，嵌入简单。MirrorStar 使用 mpv 子进程，通过命名管道 IPC 控制，需要额外的进程管理和窗口查找逻辑，但获得了更好的编解码器支持和硬件解码能力。find_mpv 支持捆绑路径优先 + PATH 回退。

6. **Web 壁纸隔离**：两者均将 Web 渲染放在独立子进程中（MirrorStar 的 mirrorstar-wp-proc / Lively 的 CefSharp），崩溃不影响主进程。MirrorStar 的子进程按需启动（仅 Web 壁纸时），关闭 Web 壁纸即终止子进程，内存占用更低；Lively 的 CefSharp 子进程启动即加载。

### 3.3 全屏检测对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `src-tauri/src/platform/fullscreen.rs` + `lib.rs` | `wp_lib/Pause.cs` + `SetupDesktop.cs` |
| **代码行数** | platform/fullscreen.rs（1145 行）+ lib.rs（1220 行） | Pause.cs 551 行 + SetupDesktop.cs |
| **检测方式** | SetWinEventHook(EVENT_SYSTEM_FOREGROUND) 事件驱动 | System.Threading.Timer 轮询（500ms 间隔） |
| **检测算法** | GetForegroundWindow + GetWindowRect + MonitorFromWindow + GetMonitorInfoW，比较窗口矩形与显示器矩形 | IsZoomedCustom() 检查窗口面积是否超过屏幕 95% |
| **暂停方式** | 逻辑暂停（PauseSender 快速通道，全局 `pause_all_fast`） | SuspendThread/ResumeThread 挂起所有线程 + 静音 |
| **自身窗口排除** | 通过 GetWindowTextW 匹配标题 | 无特殊处理 |
| **状态去抖** | AtomicBool（FULLSCREEN_WAS）记录上一次状态 | 无 |
| **配置开关** | pause_on_fullscreen | 支持 |
| **应用规则** | 无 | 支持 pause/ignore/kill 规则 |
| **电池检测** | 独立 bool 跟踪（非位掩码组合） | 支持（拔电源时暂停） |
| **多显示器感知** | 无（全局暂停/恢复所有壁纸） | 有（仅暂停全屏显示器上的壁纸） |
| **双算法** | 无 | foreground（轻量）和 all（全面但开销大） |

**关键差异：**

1. **事件驱动 vs 轮询**：MirrorStar 使用 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 事件驱动，仅在前台窗口切换时触发检测，CPU 开销接近零。Lively 使用定时器轮询（500ms 间隔），持续消耗 CPU。

2. **状态去抖**：MirrorStar 使用 AtomicBool（`FULLSCREEN_WAS`）记录上一次状态，避免重复触发暂停/恢复。Lively 无此优化。

3. **自身窗口排除**：MirrorStar 通过 GetWindowTextW 匹配标题排除自身窗口，避免应用自身全屏时误暂停壁纸。Lively 无此处理。

4. **全屏判定算法**：Lively 的 `IsZoomedCustom()` 检查窗口面积是否超过屏幕 95%，这能覆盖非标准最大化的全屏窗口（如游戏）。MirrorStar 仅比较窗口矩形与显示器矩形是否相等，可能遗漏某些全屏模式。

5. **多显示器感知**：Lively 可以仅暂停全屏显示器上的壁纸，其他显示器继续播放。MirrorStar 是全局暂停/恢复（`pause_all_fast`），不够精细。

6. **应用规则**：Lively 支持用户自定义规则（指定进程名 → pause/ignore/kill），这是一个实用的功能。MirrorStar 目前仅跳过自身窗口。

7. **暂停方式**：Lively 对外部进程使用 `SuspendThread/ResumeThread` 挂起所有线程，这是一种不安全的暂停方式（可能导致死锁），但简单有效。MirrorStar 使用 PauseSender 快速通道实现逻辑暂停，更安全但需要每种渲染器单独实现。

### 3.4 音量控制对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `audio/volume.rs` | `wp_lib/VolumeMixer.cs` |
| **代码行数** | 1042 行（volume.rs 1041 + mod.rs 1） | 230 行 |
| **API** | WASAPI (windows crate COM) | Core Audio API (P/Invoke COM) |
| **接口链** | IMMDeviceEnumerator → IMMDevice → IAudioSessionManager2 → IAudioSessionEnumerator → ISimpleAudioVolume | 相同 |
| **控制粒度** | 进程级音量 + 静音 | 进程级音量 + 静音 |
| **COM 接口缓存** | 有（优化性能） | 无 |
| **双通道控制** | WASAPI（进程级）+ mpv IPC（播放器级） | 仅 WASAPI 单通道 |

**关键差异：** 两者使用完全相同的 Windows Core Audio API 接口链，实现原理一致。MirrorStar 额外通过 mpv IPC 设置音量，形成双通道控制（WASAPI 进程级 + mpv IPC 播放器级），确保视频壁纸音量控制更可靠。此外，MirrorStar 缓存 COM 接口优化性能。

### 3.5 配置管理对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `config/` 目录（7 个文件） | `save/SaveData.cs` |
| **代码行数** | 5801 行 | 1322 行 |
| **配置格式** | TOML | JSON |
| **持久化文件** | `config.toml` + `wallpapers.toml` | `lively_config.json` + `lively_config_b.json` + 多个独立文件 |
| **写入安全** | 文件锁 + 临时文件 + rename（原子写入） | 双写备份（主文件 + _b 备份文件） |
| **热重载** | notify crate 文件监视 + 500ms 防抖（已启用） | 无 |
| **配置防抖保存** | 300ms | 无 |
| **向前兼容** | serde #[serde(default)] | JSON 默认值处理 |
| **缩略图生成** | image crate，320x180，JPEG 质量 85 | 无独立缩略图生成 |
| **壁纸元数据** | 内嵌在 wallpapers.toml | 独立 LivelyInfo.json 文件 |
| **崩溃恢复** | 无 | 配置文件损坏时自动从备份恢复 |

**关键差异：**

1. **写入安全策略**：MirrorStar 使用文件锁 + 临时文件 + rename 的原子写入方案，确保配置文件不会因写入中断而损坏。Lively 使用双写备份（同时写入主文件和 _b 备份文件），加载时若主文件损坏则尝试备份文件。两种方案各有优劣：原子写入保证单文件一致性，双写备份提供灾难恢复。

2. **热重载**：MirrorStar 支持配置文件热重载（通过 notify crate 监视文件变更 + 500ms 防抖），修改配置文件后应用自动更新。Lively 无此功能。

3. **缩略图生成**：MirrorStar 使用 image crate 生成 320x180、JPEG 质量 85 的缩略图。Lively 无独立缩略图生成机制。

4. **配置防抖保存**：MirrorStar 实现 300ms 配置防抖保存，避免频繁写入。Lively 无此优化。

5. **配置分离**：Lively 将配置分为多个文件（主配置、布局、规则、运行进程、壁纸元数据），更细粒度。MirrorStar 仅用两个文件，更简洁但扩展性稍差。

### 3.6 多显示器支持对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **显示器枚举** | EnumDisplayMonitors + GetMonitorInfoW（已实现） | System.Windows.Forms.Screen.AllScreens |
| **排列模式** | per_monitor / span（已实现） | per / span / duplicate |
| **显示器标识** | 设备名 | 设备名 |
| **DPI 感知** | GetDpiForMonitor + PerMonitorV2 DPI 感知 | DpiHelper.cs |
| **动态调整** | update_positions() 重新计算（已实现） | SystemEvents.DisplaySettingsChanged 事件 |

**关键差异：**

1. **duplicate 模式**：Lively 支持 duplicate（同壁纸在多显示器上重复显示），MirrorStar 目前不支持。

2. **DPI 处理**：MirrorStar 实现 PerMonitorV2 DPI 感知 + GetDpiForMonitor，是较新的 DPI 感知方案。Lively 有专门的 DpiHelper 类处理 DPI 缩放。

3. **显示器变更响应**：Lively 监听 `SystemEvents.DisplaySettingsChanged` 事件，显示器配置变化时自动重新布局。MirrorStar 依赖 update_positions() 重新计算（已实现），但需手动触发或由其他事件联动。

### 3.7 鼠标交互对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `desktop/window.rs`（`set_mouse_passthrough`，原 `input/mouse.rs` 已删除） | `wp_lib/RawInputDX.xaml.cs` |
| **代码行数** | 集成在 desktop/window.rs 中（原 input/mouse.rs 119 行已随 C-027 删除） | 252 行 |
| **穿透模式** | WS_EX_TRANSPARENT 标志 | 无（始终可交互） |
| **交互模式** | 切换 WS_EX_TRANSPARENT | RawInput 全局鼠标捕获 + PostMessage 转发 |
| **应用方式** | apply_to_hwnd 应用到窗口 | WM_INPUT → PostMessageW 转发到壁纸窗口 |
| **桌面判断** | 无 | 检查前台窗口是否为 WorkerW/Progman |
| **输入转发** | 无 | WM_INPUT → PostMessageW 到壁纸窗口 |

**关键差异：**

1. **交互方式**：MirrorStar 使用简单的 `WS_EX_TRANSPARENT` 标志切换穿透/交互模式——穿透模式下鼠标点击穿过壁纸到达桌面图标，交互模式下壁纸窗口接收鼠标事件。Lively 使用 RawInput API 全局捕获鼠标输入，然后 `PostMessageW` 转发到壁纸窗口，实现更精细的鼠标控制。

2. **桌面判断**：Lively 的 RawInputDX 会判断当前是否在桌面（通过检查前台窗口是否为 WorkerW/Progman），仅在桌面时转发鼠标事件。MirrorStar 无此逻辑。

3. **功能完整度**：Lively 的鼠标交互方案更完整，支持 Web 壁纸中的鼠标点击、悬停等交互。MirrorStar 的 WS_EX_TRANSPARENT 方案简单但粗糙——交互模式下壁纸窗口会拦截所有鼠标事件，桌面图标无法点击。

### 3.8 进程管理对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `process/` 目录（2 个文件，阶段2清理后） | livelySubProcess |
| **代码行数** | 1205 行 | 未统计 |
| **ProcessManager** | 1204 行：CreateProcessW + 3 秒等待 + TerminateProcess | WaitForExit |
| **子进程管理对象** | mpv.exe（视频壁纸）+ mirrorstar-wp-proc.exe（Web 壁纸，已实现） | CefSharp + 外部程序 |
| **独立看门狗进程** | 无（watchdog crate 已在阶段2移除） | 有（livelySubProcess） |

**关键差异：**

1. **实现完整度**：MirrorStar 的 ProcessManager（1204 行）已完整实现，用于管理 mpv 子进程和 wp-proc 子进程。阶段2已清理 watchdog/monitor 死代码，移除 mirrorstar-watchdog 独立 crate。Lively 的 livelySubProcess 完整实现并实际运行。

2. **架构决策**：MirrorStar 采用混合架构——仅 Web 壁纸在独立子进程（mirrorstar-wp-proc）中运行，按需启动；Image/Gif/Video 在主进程内。独立看门狗进程不再需要（watchdog crate 已在阶段2移除），因为主进程崩溃时操作系统会自动回收子进程。Lively 的多进程架构（CefSharp + 看门狗 + 外部程序）已完整实现。

### 3.9 IPC 通信对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `ipc/` 目录（4 个文件：client.rs/mpv_protocol.rs/wp_proc.rs/mod.rs） | CefSharp stdin/stdout |
| **代码行数** | 2103 行 | 未统计 |
| **通信对象** | mpv（已实现）+ wp-proc（已实现） | CefSharp + 外部程序 |
| **通信方式** | 命名管道（JSON + 换行分隔） | stdin/stdout 管道 |
| **功能** | MpvIpcClient（mpv_protocol.rs）：pause/resume/set_volume/set_speed/quit 等命令；WpProcIpcClient（wp_proc.rs）：play/terminate/set_position/navigate/pause/resume | 消息传递 |
| **连接重试** | 有 + request_id 响应匹配（mpv 40×50ms / wp-proc 160×50ms / 窗口查找 20×100ms） | 无 |
| **事件通知** | 无（wp-proc 不发送异步事件，HWND 通过 FindWindowW 获取） | 无 |

**关键差异：** MirrorStar 的 IPC 用于两类子进程通信——mpv（已实现）和 wp-proc（已实现），均采用命名管道 + JSON + 换行分隔协议，实现连接重试 + request_id 响应匹配机制（重试参数见 `wallpaper/subprocess_base.rs`：mpv `MPV_CONNECT_RETRIES` 40×50ms=2s / wp-proc `WP_PROC_CONNECT_RETRIES` 160×50ms=8s / 窗口查找 20×100ms=2s）。wp-proc 的 HWND 通过 FindWindowW + PID 验证获取，不通过 IPC 回传。Lively 使用 stdin/stdout 管道与 CefSharp 和外部程序通信。

### 3.10 Tauri 应用层对比

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| **实现文件** | `src-tauri/src/` 目录（12 个文件：lib.rs + main.rs + state.rs + commands/ + platform/） | `App.xaml.cs` |
| **代码行数** | 12 个文件共 6952 行（lib.rs 1220 + main.rs 57 + state.rs 1031 + commands/ 4 文件 + platform/ 5 文件） | 未统计 |
| **Tauri 命令** | 24 个全部完整实现（均为 `#[tauri::command]` 注解命令，托盘暂停/恢复复用 `pause_wallpaper`/`resume_wallpaper`） | - |
| **系统托盘** | 在 lib.rs setup 中构建（state.rs 管理状态），托盘菜单项定义于 lib.rs L967-976（TrayIconBuilder，3 项菜单：打开/暂停-恢复/退出，无音量子菜单） | 完整 |
| **单实例保护** | Win32 CreateMutexW | Mutex |
| **Explorer 重启检测** | TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底（start_workerw_check：interval 300s + Notify，check_and_reinitialize） | 无 |
| **全屏检测** | SetWinEventHook 事件驱动（Hook 失败仅记录并退出监控线程，无轮询回退） | Timer 轮询 |
| **主窗口** | 懒创建 + 关闭即隐藏（hide） | 常驻 |
| **DPI 感知** | PerMonitorV2 | DpiHelper |
| **COM 初始化** | 主进程显式 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA），wp-proc 同样 STA | - |
| **前端事件** | 4 个 emit | - |

**关键差异：** MirrorStar 的 Tauri 应用层实现了 24 个命令（均为 `#[tauri::command]` 注解命令，托盘"暂停/恢复壁纸"复用 `pause_wallpaper`/`resume_wallpaper` 命令，全部完整实现），系统托盘在 lib.rs setup 中构建（state.rs 管理状态），托盘菜单仅 3 项（打开/暂停-恢复/退出，无音量子菜单、无显示器子菜单，定义于 lib.rs L967-976），单实例保护（Win32 CreateMutexW），Explorer 重启检测（TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底，`start_workerw_check` 使用 interval 300 秒 + Notify，失效时 `check_and_reinitialize`，无 30 秒轮询），全屏检测（纯 SetWinEventHook 事件驱动，全局 `pause_all_fast` 暂停所有壁纸，Hook 失败仅记录并退出监控线程，无轮询回退），主窗口懒创建 + 关闭即隐藏（hide），PerMonitorV2 DPI 感知，主进程显式 COM STA 初始化（`CoInitializeEx(None, COINIT_APARTMENTTHREADED)`），4 个前端事件 emit。

---

**相关文档：**

- [架构概述](./架构概述-Architecture-Overview.md)
- [系统架构](./系统架构-System-Architecture.md)
- [进程架构](./进程架构-Process-Architecture.md)
- [依赖与数据流](./依赖与数据流-Dependency-and-Data-Flow.md)
- [桌面集成](./桌面集成-Desktop-Integration.md)
- [暂停恢复机制](./暂停恢复机制-Pause-Resume.md)
- [错误处理](./错误处理-Error-Handling.md)
- [性能优化](./性能优化-Performance.md)