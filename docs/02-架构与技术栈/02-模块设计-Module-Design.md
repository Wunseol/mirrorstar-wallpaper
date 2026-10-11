[← 返回文档索引](../index.md) > 架构与技术栈 > 模块设计

# 模块设计

> 基于真实代码审计

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.0                      |
| 更新日期 | 2026-10-11                |
| 文档状态 | 已实现（已按真实代码合并重组，2026-10）        |

本文档描述 MirrorStar Wallpaper（镜星壁纸）的模块划分与各模块详细状态。所有数据均基于真实代码审计。

> **代码规模说明**：文中行数为审计记录值，随代码演进可能变化，**以实测为准**。
>
> **注（主窗口原生 UI 重构后，2026-10-02）**：原"前端 UI"模块（`src/scripts/` + `index.html`，Vite/TypeScript）已**整体删除**，其能力由新增的**原生 UI 子进程** `crates/mirrorstar-ui/`（Slint 1.18）承载；壳（`src-tauri/`）不再创建 WebView2 主窗口。本文中涉及前端/Tauri IPC（`emit`）与"主窗口懒创建/关闭即隐藏"的描述为历史记录，以本注为准。

---

## 1. 模块总览

MirrorStar 由 **11 个 `mirrorstar-core` 模块**（真实代码中的 `pub mod`：audio/config/desktop/ipc/layout/perf/process/pure_mode/scheduler/ui_proto/wallpaper）与 3 个应用/子进程 crate（`src-tauri`、`crates/mirrorstar-ui`、`crates/mirrorstar-wp-proc`）构成。

> **仓库目录与文件清单**：各 crate / 模块的目录布局与文件清单统一见《[项目目录结构](../03-实施与验证/03-项目目录结构-Project-Structure.md)》，本文只描述模块职责、依赖与进程架构，不再逐一罗列文件。

### 1.1 mirrorstar-core 模块（11 个 `pub mod`）

| # | 模块 | 路径 | 状态 |
|---|------|------|------|
| 1 | audio (音频控制) | `crates/mirrorstar-core/src/audio/` | ✅ 已实现 |
| 2 | config (配置管理) | `crates/mirrorstar-core/src/config/` | ✅ 已实现 |
| 3 | desktop (桌面集成) | `crates/mirrorstar-core/src/desktop/` | ✅ 已实现 |
| 4 | ipc (进程间通信) | `crates/mirrorstar-core/src/ipc/` | ✅ 已实现 |
| 5 | layout (布局策略) | `crates/mirrorstar-core/src/layout/` | ✅ 已实现 |
| 6 | perf (内存归因埋点) | `crates/mirrorstar-core/src/perf.rs` | ✅ 已实现 |
| 7 | process (进程管理) | `crates/mirrorstar-core/src/process/` | ✅ 已实现 |
| 8 | pure_mode (纯净壁纸模式) | `crates/mirrorstar-core/src/pure_mode/` | ✅ 已实现 |
| 9 | scheduler (轮换调度) | `crates/mirrorstar-core/src/scheduler/` | ✅ 已实现 |
| 10 | ui_proto (壳↔UI 协议) | `crates/mirrorstar-core/src/ui_proto/` | ✅ 已实现 |
| 11 | wallpaper (壁纸引擎) | `crates/mirrorstar-core/src/wallpaper/` | ✅ 已实现 |

### 1.2 应用 / 子进程 crate

| # | 名称 | 路径 | 类型 | 状态 |
|---|------|------|------|------|
| 12 | src-tauri（Tauri 应用壳） | `src-tauri/src/` | 应用主程序（壳 exe） | ✅ 已实现 |
| 13 | mirrorstar-ui（原生 UI 子进程） | `crates/mirrorstar-ui/` | 二进制 crate（`mirrorstar-ui.exe`） | ✅ 已实现 |
| 14 | mirrorstar-wp-proc（Web 壁纸子进程） | `crates/mirrorstar-wp-proc/` | **库 crate**（经壳 exe `--wp-proc` 模式运行，**非独立 exe**） | ✅ 已实现 |

> **架构决策**：采用混合进程架构——仅 Web 壁纸运行在独立子进程（壳 exe 的 `--wp-proc` 模式），按需创建；Image/Gif/Video 渲染器保留在壳进程内。mirrorstar-watchdog 独立进程已在阶段2移除（空壳 crate 已删除）。
>
> **已删除/不存在的模块**：
> - **input（输入控制）**：已在代码评审 cycle4a 作为死代码删除（生产代码无调用方，交互模式切换由 `wallpaper/manager.rs` 直接调用 renderer 的 `set_interaction_mode`）。
> - **前端 UI**：已整体删除（由原生 UI 子进程取代）。
> - **tray（系统托盘 core 模块）**：不存在。托盘功能在 Tauri 应用层内联实现（Tauri `TrayIconBuilder`），并非独立 core 模块。
> - **watchdog（看门狗进程）**：已移除。

### 1.3 混合进程架构示意

```
┌──────────────────────────────────────────────────────────┐
│        mirrorstar-wallpaper.exe（壳进程，Tauri）           │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
│  │ Tauri    │ │ 图片线程  │ │ GIF 线程  │ │ 视频线程   │  │
│  │ 主线程   │ │ (GetMsg) │ │ (Timer)  │ │ (mpv IPC) │  │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────────────┐  │
│  │ 全屏检测 │ │ 配置监视 │ │ WebRenderer (代理层)     │  │
│  │ (Hook)   │ │ (notify) │ │  ├ ProcessManager       │  │
│  └──────────┘ └──────────┘ │  └ WpProcIpcClient       │  │
│  ┌────────────────────────────────────────────────────┐  │
│  │ UI 子进程管理（spawn / 握手 / 空闲退出 / 单实例）  │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
       │                              │                    │
       │ CreateProcessW               │ CreateProcessW     │ CreateProcessW
       ▼                              ▼   + 命名管道 IPC    ▼  + stdio(NDJSON)
┌─────────────────┐         ┌──────────────────────────┐  ┌──────────────────┐
│   mpv.exe       │         │mirrorstar-wallpaper.exe  │  │ mirrorstar-ui.exe│
│   (视频壁纸)    │         │ --wp-proc Web 壁纸子进程 │  │ (原生主窗口 UI)  │
│   按需启动      │         │  ┌────────────────────┐  │  │ Slint 软件渲染   │
└─────────────────┘         │  │ WebView2 环境      │  │  │ 按需启动/空闲退出│
                            │  │  └ 窗口 + 消息循环  │  │  └──────────────────┘
                            │  └────────────────────┘  │
                            └──────────────────────────┘
                               ↑ 仅 Web 壁纸时启动      ↑ 托盘"打开主窗口"时启动
                               ↑ 关闭 Web 壁纸时终止     ↑ 窗口隐藏超时后退出
```

**设计原则：**
- 壳进程（`mirrorstar-wallpaper.exe`）内通过 WallpaperEngine 管理所有壁纸渲染；不再创建任何 WebView2 主窗口
- Image/Gif 渲染器在壳进程专用线程中运行（GDI + 双缓冲）
- Video 渲染器通过 ProcessManager 直接 spawn `mpv.exe` 子进程（按需），通过 mpv 原生命名管道 IPC（`MpvIpcClient`，定义于 `ipc/mpv_protocol.rs`）控制，**不使用 libmpv2 FFI 库嵌入**
- **Web 渲染器已重构为代理层**：通过 ProcessManager 以 `--wp-proc` 模式 spawn 壳进程自身（`mirrorstar-wallpaper.exe --wp-proc`，按需），通过 `WpProcIpcClient`（定义于 `ipc/wp_proc.rs`）命名管道通信
- **UI 主窗口为独立进程** `mirrorstar-ui.exe`（crate `crates/mirrorstar-ui/`，Slint 1.18 软件渲染），经 `mirrorstar-core::ui_proto` NDJSON over stdio 通信
- HWND 通过 FindWindowW 获取后，壳进程执行 WorkerW 嵌入（与 mpv 窗口嵌入逻辑一致）

**内存优化：**
- 无 Web 壁纸、无主窗口时：零子进程开销（仅壳进程 + 可选的 mpv）
- WebView2 运行时仅加载在子进程中，壳进程不加载 webview2-com
- 子进程在 Web 壁纸关闭时立即终止，释放全部内存
- UI 子进程在窗口隐藏超时（默认 300s）后退出回收内存
- 无独立看门狗进程（mirrorstar-watchdog 已在阶段2移除）

### 1.4 混合进程架构（早期版本示意，作历史参考）

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
│   mpv.exe       │         │mirrorstar-wallpaper.exe  │
│   (视频壁纸)    │         │ --wp-proc Web 壁纸子进程 │
│   按需启动      │         │  ┌────────────────────┐  │
└─────────────────┘         │  │ WebView2 环境      │  │
                            │  │  └ 窗口 + 消息循环  │  │
                            │  └────────────────────┘  │
                            └──────────────────────────┘
                               ↑ 仅 Web 壁纸时启动
                               ↑ 关闭 Web 壁纸时终止
```

---

## 2. 各模块详细状态

### 2.1 config (配置管理) — ✅ 已实现 100%

**代码规模（审计记录，以实测为准）：** 7 个文件

**已实现：**
- AppConfig 完整结构（General/Audio/Pause/Display/Video/Gif 等子配置）
- ConfigManager 全功能 CRUD：加载、保存、更新配置
- 原子文件写入 + fs2 文件锁
- 热重载文件监视（notify crate，500ms 防抖）— 已启用 `start_watching()`
- WallpaperLibrary 壁纸库管理（添加/删除/列表/更新缩略图）
- 文件类型自动检测（Video/Gif/Web/Image）
- 缩略图生成（image crate，320x180，JPEG 质量 85）
- 配置防抖保存（300ms）

> 配置结构、数据根目录、校验策略与热重载详见[依赖与数据流](./08-依赖与数据流-Dependency-and-Data-Flow.md)。

---

### 2.2 desktop (桌面集成) — ✅ 已实现 100%

**代码规模（审计记录，以实测为准）：** 4 个文件

**已实现：**
- WorkerW 查找三重策略：直接 EnumWindows 查找 → 发送 0x052C 消息触发创建 → 5 次重试（50ms 间隔）→ 备用回调方法
- 壁纸窗口嵌入（SetParent + SetWindowPos HWND_BOTTOM + ShowWindow）
- Explorer 重启检测与恢复（TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底，调用 `check_and_reinitialize` 重建）
- 原始系统壁纸保存与恢复（SystemParametersInfoW）
- 窗口操作（无边框 `make_borderless`、移除任务栏 `remove_from_taskbar`、鼠标穿透 `set_mouse_passthrough`）
- 显示器枚举 `enumerate_displays()`（EnumDisplayMonitors + GetMonitorInfoW）
- **原生壁纸 API**（`native_wallpaper.rs`）：JPG/JPEG/PNG/BMP/TIF/TIFF/DIB 静态图片通过 SystemParametersInfoW + 注册表设置，零资源占用
- WallpaperMode 双路径：Native（原生 API）和 WorkerW（窗口嵌入）

> 详细的 WorkerW 嵌入流程、窗口样式、多显示器、DPI、Explorer 重启与纯净模式见[桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md)。

---

### 2.3 wallpaper (壁纸引擎) — ✅ 已实现 100%

**代码规模（审计记录，以实测为准）：** 13 个文件

**已实现：**

**核心抽象：**
- WallpaperRenderer trait 完整生命周期定义（play/pause/resume/set_volume/set_position/terminate/hwnd/state/set_speed/navigate/set_scaling_mode/set_mouse_passthrough/set_interaction_mode/create_pause_sender）
- WallpaperEngine 管理器（manager.rs）：set/close/pause/resume/shutdown + 快速路径控制
- **PauseSender 快速通道**：绕过引擎互斥锁，直接发送暂停/恢复/音量命令
- 5 种缩放模式（Fill/Fit/Stretch/Center/Original）
- WallpaperMode 双路径（Native/WorkerW）
- `update_positions` 多显示器位置更新（span 和 per_monitor）
- **子进程退出监听**：仅存在 `spawn_proc_exit_monitor`（`wallpaper/mod.rs`）退出监听，监听子进程状态，异常退出时更新为 `Terminated` 状态并 `notify_state_changed` 通知引擎；**无自动 respawn（无自动重启逻辑）**

**四种渲染器：**
- **ImageRenderer**：GDI + 双缓冲 + HALFTONE + 专用线程消息循环 + 超屏幕分辨率降采样 + 暂停时释放像素数据
- **GifRenderer**：image crate GIF 解码 + GDI 双缓冲 + WM_TIMER 帧驱动 + 40MB 内存预算限制 + 暂停时释放帧数据 + 速度控制
- **VideoRenderer**：mpv 子进程 + 命名管道 IPC + WASAPI 音量控制
- **WebRenderer**（代理层）：通过 ProcessManager 以 `--wp-proc` 模式启动壳进程自身子进程 + WpProcIpcClient 命名管道 IPC 通信 + FindWindowW + PID 验证获取 HWND，实现崩溃隔离和内存优化

> 各渲染器实现细节见[壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)。

---

### 2.4 audio (音频控制) — ✅ 已实现 95%

**代码规模（审计记录，以实测为准）：** 2 个文件

**已实现：**
- VolumeControl WASAPI 进程级音量控制
- COM 接口缓存（IMMDeviceEnumerator + IAudioSessionManager2）
- `set_process_volume` / `set_process_mute` / `get_process_volume` / `get_process_mute`
- `refresh_session_manager` 音频设备变更处理 — ⚠️ **已实现但未接入调用链（死代码）**：函数已定义并实现（重新创建 session manager + 清空 session_cache），但生产代码中从未被调用。当前未在 `src-tauri/src/platform/` 中接入音频设备变更监听，待未来启用。

**缺失：**
- 无单元测试（COM 环境限制）
- `refresh_session_manager` 调用链接入（音频设备变更监听未实现）

---

### 2.5 ipc (进程间通信) — ✅ 已实现 100%

**代码规模（审计记录，以实测为准）：** 4 个文件

> **重要说明**：项目中存在**多套独立 IPC 通道**，并非统一协议：
> - **mpv 原生 IPC**（管道名 `\\.\pipe\mirrorstar-mpv-{uuid}`）：由 `MpvIpcClient` 实现（定义于 `ipc/mpv_protocol.rs`），用于壳进程与 mpv.exe 子进程通信
> - **wp-proc WpProcCommand 协议**（管道名 `\\.\pipe\mirrorstar-wp-{uuid}`）：由 `WpProcIpcClient` 实现（定义于 `ipc/wp_proc.rs`），用于壳进程与壳 `--wp-proc` 模式子进程通信
> - **壳↔UI 子进程**：`ui_proto` NDJSON over stdio（`crates/mirrorstar-core/src/ui_proto/`），**不是命名管道**
>
> mpv 与 wp-proc 两者均采用命名管道 + JSON + 换行分隔的传输格式，并复用连接重试 + request_id 响应匹配模式，但命令集与协议语义各自独立。**注意：不存在 `ipc/protocol.rs` 文件**，协议定义位于 `mpv_protocol.rs` 与 `wp_proc.rs` 中。

**已实现：**
- MpvIpcClient mpv 命名管道 IPC 客户端（mpv_protocol.rs）
- WpProcIpcClient Web 壁纸子进程 IPC 客户端（wp_proc.rs，复用 MpvIpcClient 的连接重试 + request_id 模式）
- 连接重试机制：mpv `MPV_CONNECT_RETRIES` = 40 × 50ms = 2s；wp-proc `WP_PROC_CONNECT_RETRIES` = 160 × 50ms = 8s；窗口查找重试 = 20 × 100ms = 2s（均位于 `wallpaper/subprocess_base.rs`）
- `send_command` 命令发送 + request_id 响应匹配
- `get_property` / `set_property` 属性读写
- `pause` / `resume` / `set_volume` / `set_speed` / `quit` 控制命令（wp-proc 另有 play/terminate/set_position/navigate）

> IPC 协议完整定义见[进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)。

---

### 2.6 process (进程管理) — ✅ 已实现 95%

**代码规模（审计记录，以实测为准）：** 2 个文件（阶段2清理后）

**已实现：**
- ProcessManager：CreateProcessW 启动子进程 + 3 秒等待 + TerminateProcess 强制终止
- `is_running()` / `pid()` / `handle()` 进程状态查询

**阶段2清理：**
- 移除 `watchdog.rs`（死代码，Watchdog 结构体从未被 src-tauri 引用）
- 移除 `monitor.rs`（死代码，ProcessMonitor 从未被使用）
- 移除 `mirrorstar-watchdog` 独立 crate（空壳，不再需要独立看门狗进程）

> **注意**：实际架构中不存在 `ProcessMonitor` 结构体。全屏暂停状态通过 `AtomicBool`（`FULLSCREEN_WAS`）记录上一次状态实现去抖，电池暂停状态由独立的 bool 跟踪，二者并非位掩码组合。详见[暂停恢复机制](./06-暂停恢复机制-Pause-Resume.md)。

---

### 2.7 layout (布局策略) — ✅ 已实现 100%

**代码规模（审计记录，以实测为准）：** 1 个文件

**定位：** 多屏排列（Arrangement）的**唯一逻辑归属**（DR-2 布局域）。将"多屏排列 × 显示器拓扑"以纯函数计算为布局平面 `LayoutPlane`（单元划分 / 窗口矩形 / 主显示器），**不依赖 Win32**（`LayoutMonitor` 为由 `DisplayInfo` 转换的纯数据抽象）。调度器 / 命令层 / 前端均只是该域的消费方，不再各自实现"编排 → 单元"映射。

**已实现：**
- 纯函数 API：`plan()`（`Arrangement` × `LayoutMonitor[]` → `LayoutPlane`）、`unit_keys()`、`validate_unit_key()`
- 类型：`LayoutPlane`（单元划分 / 窗口矩形 / 主显示器）、`LayoutMonitor`、`LayoutRect`（含 `union()` 联合矩形，Span 虚拟桌面）
- 三种编排语义：`PerMonitor` → 每屏一个单元（key = 显示器 id）；`AllSame` → 全局 `all` 单元（窗口矩形 = 主屏矩形）；`Span` → 全局 `all` 单元（窗口矩形 = 虚拟桌面联合矩形）
- `validate_unit_key()` 收敛 DR-40 规则：`PerMonitor` 校验 / 回退主显示器 key；`AllSame`/`Span` 仅允许全局 `all` key，拒绝孤儿单元

**消费方：** 轮换调度器 `reconcile_and_align`（`src-tauri/src/scheduler.rs`）、命令层（`commands::rotation` / `commands::config`）。

---

### 2.8 scheduler (轮换调度) — ✅ 已实现

**路径：** `crates/mirrorstar-core/src/scheduler/`

**定位：** 壁纸轮换功能的纯逻辑归属——采样算法（顺序循环 / 洗牌袋 / 纯随机，`order.rs`）与开机解析（`boot.rs`）。调度器主循环与事件编排位于 `src-tauri/src/scheduler.rs`。

> 完整设计（数据模型、触发系统、原子交换、决策记录等）见[壁纸轮换调度器](./10-壁纸轮换调度器-Rotation-Scheduler.md)。

---

### 2.9 pure_mode (纯净壁纸模式) — ✅ 已实现

**路径：** `crates/mirrorstar-core/src/pure_mode/`

**定位：** 纯净壁纸模式的核心逻辑——通过"临时把壁纸窗口提升为顶层置顶窗口"实现全屏纯壁纸沉浸态，退出后再用 `embed_wallpaper` 嵌回。核心函数 `lift_wallpaper_window`（图片/GIF/网页路径）与视频路径的临时顶层覆盖渲染器。

> 层级对比、双路径呈现、进入/退出时序与关键约束见[桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md) 的「纯净壁纸模式」章节。

---

### 2.10 perf (内存归因埋点) — ✅ 已实现

**路径：** `crates/mirrorstar-core/src/perf.rs`

**定位：** 内存归因埋点——`ProcessCategory` 进程分类、`attribute_processes` 进程树归因、`aggregate` → `MemoryUsageReport`、周期采样 `spawn_memory_attribution_sampler`，以及启动期时间线 `spawn_startup_memory_timeline` / `mark_lifecycle`。

> 详见[性能优化](./09-性能优化-Performance.md) 的内存归因章节。

---

### 2.11 ui_proto (壳↔UI 协议) — ✅ 已实现

**路径：** `crates/mirrorstar-core/src/ui_proto/`

**定位：** 壳进程与原生 UI 子进程共享的协议定义，采用 **NDJSON 行协议 over stdio**（**非**命名管道）。消息类型 `Hello` / `Request` / `Response` / `Event` / `Control`。

> 详见[进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md) 的 IPC 设计章节。

---

### 2.12 src-tauri（Tauri 应用层） — ✅ 已实现 95%

**代码规模（审计记录，以实测为准）：** 详见[项目目录结构](../03-实施与验证/03-项目目录结构-Project-Structure.md)

**已实现：**
- **24 个 Tauri 命令全部完整实现**（无空操作），均为 `#[tauri::command]` 注解命令；托盘菜单的"暂停/恢复壁纸"复用 `pause_wallpaper`/`resume_wallpaper` 命令：
  `get_wallpapers`, `add_wallpaper`, `generate_thumbnail`, `regenerate_thumbnails`, `remove_wallpaper`, `set_wallpaper`, `pause_wallpaper`, `resume_wallpaper`, `get_config`, `update_config`, `set_volume`, `toggle_mute`, `set_interaction_mode`, `toggle_interaction`, `get_displays`, `get_wallpaper_state`, `open_file_dialog`, `toggle_auto_start`, `get_auto_start_status`, `set_scaling_mode`, `set_speed`, `update_positions`, `check_desktop_status`, `quit_app`
- 系统托盘（**在 lib.rs setup 中构建（state.rs 管理状态），菜单仅 3 项：打开主窗口 / 暂停-恢复壁纸 / 退出；无音量子菜单、无显示器子菜单**）+ 点击图标显示窗口
- 单实例保护（Win32 CreateMutexW）
- Explorer 重启后台检测（TaskbarCreated 事件驱动 + WorkerW 5 分钟轮询兜底，**无 30 秒轮询**）
- 全屏应用检测（**纯 SetWinEventHook(EVENT_SYSTEM_FOREGROUND) 事件驱动** + 状态去抖 + 自身窗口排除；**无 2 秒轮询回退**）— **全局暂停/恢复**（`pause_all_fast`），**非按显示器粒度**
- 退出时壁纸恢复（`perform_shutdown`）
- 配置热重载已启用
- 主窗口按需启动原生 UI 子进程（`mirrorstar-ui.exe`），空闲超时退出
- PerMonitorV2 DPI 感知
- **COM STA 初始化**：主进程显式调用 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA），wp-proc 子进程同样使用 STA
- PauseSender 快速路径（绕过引擎互斥锁）
- 2 个插件（tauri-plugin-dialog, tauri-plugin-autostart）

---

### 2.13 mirrorstar-ui（原生 UI 子进程） — ✅ 已实现

**路径：** `crates/mirrorstar-ui/`

**定位：** 主窗口 UI（Slint 1.18，**绑定软件渲染**），经 `ui_proto` NDJSON over stdio 与壳通信。按需启动、握手（`Hello`）、窗口隐藏超时（默认 300s）退出、单实例；不复制业务逻辑、不直接读写配置；实现原生虚拟化壁纸网格、四类预览（图片自绘 / 视频 UI 进程内 mpv 子窗口 / GIF 逐帧 / 网页临时 WebView2 子窗口）、拖拽、搜索、键盘导航。

---

### 2.14 mirrorstar-wp-proc（Web 壁纸子进程） — ✅ 已实现 100%

**状态：** 完整实现（`crates/mirrorstar-wp-proc/`，5 个文件），CLI 参数 + 命名管道服务端 + IPC 线程 + WebView2 渲染 + 消息循环 + 命令处理。

> **crate 形态**：`mirrorstar-wp-proc` 为**库 crate**（无独立 exe 产物），经**壳 exe `--wp-proc` 模式**运行，随壳分发。

**实现要点：**
- clap CLI 参数：`--source`（初始网页源）、`--pipe-name`（管道名称）、`--title`（窗口标题）、`--rect`（初始位置）
- 命名管道服务端：`CreateNamedPipeW` + `ConnectNamedPipe` + `BufReader`/`BufWriter`
- IPC 线程：读取 JSON 命令行 → 反序列化 `WpProcCommand` → mpsc 发送到主线程 → `PostMessageW` 唤醒 → 等待响应 → 序列化写回管道
- WebView2 环境：`CreateCoreWebView2Environment` + `CreateCoreWebView2Controller` + URL 导航（支持 http/https 和 file://）
- 窗口类注册：固定类名 `MirrorStarWebWallpaperCls`，窗口标题使用 `--title` 参数（用于 `FindWindowW` 查找）
- 主消息循环：`GetMessageW` + `WM_WEB_COMMAND`（`WM_USER + 20`）处理
- 命令处理：Play（导航到源）、Terminate（销毁窗口+退出）、SetPosition（`SetWindowPos` + `SetBounds`）、Navigate（`webview.Navigate`）、Pause/Resume（状态标记）
- **COM 初始化**：`CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA，与主进程一致）
- **崩溃隔离**：子进程异常退出时，壳进程 `wallpaper/mod.rs` 的 `spawn_proc_exit_monitor` 会检测到退出并把状态更新为 `Terminated` 并 `notify_state_changed` 通知引擎，**无自动重启逻辑**

**IPC 协议（JSON + 换行分隔，与 mpv IPC 传输格式一致）：**

命令（壳进程 → 子进程）：
```json
{"command":"play","request_id":1,"source":"https://..."}
{"command":"terminate","request_id":2}
{"command":"set_position","request_id":3,"x":0,"y":0,"width":1920,"height":1080}
{"command":"navigate","request_id":4,"url":"https://..."}
{"command":"pause","request_id":5}
{"command":"resume","request_id":6}
```

响应（子进程 → 壳进程）：
```json
{"request_id":1,"status":"ok"}
{"request_id":1,"status":"error","error":"WebView2 未初始化"}
```

---

## 3. 已删除模块（历史记录）

| 模块 | 原路径 | 状态 | 说明 |
|------|--------|------|------|
| input（输入控制） | `crates/mirrorstar-core/src/input/` | ❌ 已删除 | 代码评审 cycle4a 作为死代码删除；交互模式切换由 `wallpaper/manager.rs` 直接调用 renderer 的 `set_interaction_mode` 处理 |
| 前端 UI | `src/scripts/` + `index.html` | ❌ 已删除 | Vite/TypeScript 前端整体删除，由原生 UI 子进程取代；`wpfile://` 自定义协议、`percent-encoding` 依赖一并移除 |
| watchdog（看门狗） | `crates/mirrorstar-watchdog/` | ❌ 已删除 | 阶段2移除（空壳 crate），依赖 OS 自动回收子进程 |

---

**相关文档：**

- [架构与技术栈总览](./01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)
- [桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md)
- [壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)
- [依赖与数据流](./08-依赖与数据流-Dependency-and-Data-Flow.md)
- [壁纸轮换调度器](./10-壁纸轮换调度器-Rotation-Scheduler.md)