# MirrorStar Wallpaper（镜星壁纸）需求文档 — 功能性需求 / Functional Requirements

[← 返回文档索引](../index.md) > [需求文档](./01-项目概述-Overview.md) > 功能性需求

> 相关文档：[项目概述](./01-项目概述-Overview.md) | [非功能性需求](./03-非功能性需求-Non-Functional-Requirements.md) | [用例](./04-用例-Use-Cases.md) | [约束与术语](./05-约束与术语-Constraints-and-Terminology.md)

## 4. 功能需求分析

### 4.1 壁纸渲染路径（v3.0 修正）

壁纸渲染采用 **双模式分发**，由 `WallpaperEngine::set_wallpaper`（`crates/mirrorstar-core/src/wallpaper/mode_dispatch.rs::determine_wallpaper_mode`）按壁纸来源与类型决定使用 **Native** 还是 **WorkerW** 模式：

| 类型 | 来源 | 原生格式支持 | 模式 |
|------|------|--------------|------|
| Image | File | 是 | **Native**（零资源占用，零渲染器、不嵌入 WorkerW） |
| Image | File | 否 | **WorkerW**（GDI 渲染） |
| Image | Url | — | **WorkerW**（仅 File 支持 Native） |
| Gif / Video / Web | * | — | **WorkerW** |

其中 `crates/mirrorstar-core/src/desktop/native_wallpaper.rs::is_native_supported` 判定的原生支持格式为：**jpg / jpeg / png / bmp / tif / tiff / dib**。其余图片格式（如 **WebP**）回退到 WorkerW 渲染路径（GDI 绘制）。

**进程与前端架构（v3.0 修正）**：主窗口**不再是 Tauri WebView 前端**，而是独立的 Slint 原生 UI 子进程 `crates/mirrorstar-ui`（Slint 1.18，软件渲染）。壳进程 `src-tauri`（Tauri v2，仅启用 `tray-icon`）承担托盘图标、对话框（tauri-plugin-dialog）、开机自启（tauri-plugin-autostart）与后端命令；网页壁纸使用的 WebView2（webview2-com 0.34）由**壳 exe 以 `--wp-proc` 模式** spawn 的子进程承载（`crates/mirrorstar-wp-proc` 为库 crate，`src-tauri/src/main.rs` 通过 `--wp-proc` 分派到 `mirrorstar_wp_proc::run_with_argv`），仍是独立进程，崩溃隔离不变。

### 4.2 保留功能

| 功能 | 说明 | 保留理由 |
|------|------|----------|
| 视频壁纸 | 通过 mpv 播放视频文件作为壁纸 | 核心功能，用户最常用的壁纸类型 |
| GIF 壁纸 | 播放 GIF 动图作为壁纸 | 核心功能，轻量级动态壁纸 |
| 网页壁纸 | 通过 WebView2 渲染网页作为壁纸 | 核心功能，支持 Shadertoy、网页动画等丰富内容 |
| 静态图片壁纸 | 设置静态图片为壁纸 | 基础功能，用户期望的最低支持 |
| 桌面集成 | 通过 WorkerW 将壁纸嵌入桌面 | 技术核心，实现壁纸在桌面图标下方显示 |
| 多显示器支持 | 为不同显示器设置不同壁纸 | 实用功能，多显示器用户刚需 |
| 暂停/恢复 | 检测全屏应用自动暂停壁纸 | 性能关键，游戏/全屏应用时释放资源 |
| 纯净壁纸模式 | 隐藏桌面图标与任务栏，仅保留壁纸，按任意键退出 | 演示/沉浸场景刚需 |
| 系统托盘 | 最小化到系统托盘运行 | 壁纸软件必备，后台运行不占任务栏 |
| 配置管理 | 保存和加载用户设置 | 基础功能，持久化用户偏好 |
| 拖放设置壁纸 | 拖拽文件到窗口设置壁纸 | 易用性功能，降低操作门槛 |

### 4.3 去除功能

| 功能 | 去除理由 |
|------|----------|
| Unity 壁纸 | 用户群体极小，Unity 运行时体积庞大（>100MB），与轻量化目标严重冲突 |
| Godot 壁纸 | 用户群体极小，Godot 运行时体积较大，与轻量化目标冲突 |
| BizHawk 模拟器壁纸 | 极其小众的需求，模拟器依赖复杂，维护成本极高 |
| YouTube/流媒体壁纸 | 依赖 youtube-dl/yt-dlp 等外部工具，网络流媒体增加复杂度和资源消耗，用户可直接下载视频后使用视频壁纸 |
| 壁纸创建器 | 非核心功能，用户可使用外部工具（如 VS Code、Shadertoy）创建壁纸内容 |
| 自动更新 | 安全风险（供应链攻击），增加复杂度，用户可手动更新或通过包管理器更新 |
| 多语言国际化 (i18n) | UI 仅支持简体中文界面，Slint UI 未引入 i18n 框架/多语言资源（代码结构预留扩展点） |
| 壁纸库 GIF 动态预览 | GIF 动态预览需解码并缓存帧数据，内存占用高；壁纸库改用静态缩略图（无缩略图时回退直载源图，仍为静态展示） |
| 按进程应用程序规则 | 全屏检测暂停已覆盖主要场景；按进程规则可后续版本添加 |

---

## 5. 功能性需求

> 优先级符号：P0（核心）/ P1（重要）/ P2（增强）。🔧 表示当前未实现或部分实现，需未来改进。

### 5.1 壁纸管理

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-001 | 视频壁纸播放 | P0 | 支持通过外部 mpv.exe 子进程播放 MP4/AVI/MKV/WebM/MOV/FLV 等常见视频格式作为动态壁纸，支持硬件加速解码（mpv `--hwdec=auto`）与循环播放（`--loop-file`）。mpv.exe 通过命名管道（`\\.\pipe\mirrorstar-mpv-*`）接收 mpv 原生 JSON IPC 命令控制（含 `set_speed`） |
| FR-002 | GIF 壁纸播放 | P0 | 支持播放 GIF 动图作为动态壁纸，循环播放（GDI + WM_TIMER 驱动）。含 **4 种内存管理策略**：Aggressive（激进释放）/ Balanced（平衡，默认）/ Performance（性能优先）/ Adaptive（自适应）；并对空像素帧派生后台预取线程解码当前帧附近帧 |
| FR-003 | 网页壁纸渲染 | P0 | 支持通过 WebView2 渲染 HTML/CSS/JavaScript 网页作为壁纸。网页壁纸由壳进程 `--wp-proc` 模式子进程承载（`crates/mirrorstar-wp-proc` 为库 crate，同一 exe 双入口，仍是独立进程，崩溃隔离不变），主进程通过命名管道 `\\.\pipe\mirrorstar-wp-*` 与 wp-proc 通信 |
| FR-004 | 静态图片壁纸 | P0 | 支持设置 JPG/JPEG/PNG/BMP/TIF/TIFF/DIB/WebP 等静态图片为壁纸。其中 jpg/jpeg/png/bmp/tif/tiff/dib 使用 Windows 原生壁纸 API（`SystemParametersInfoW` + 注册表，含"先备份后修改、失败回滚"事务语义），零资源占用；WebP 等不被原生 API 支持的格式回退到 WorkerW 渲染路径（GDI 绘制） |
| FR-005 | 壁纸切换 | P0 | 支持在已添加的壁纸之间切换（含同显示器 Native ↔ WorkerW 模式切换），切换过程平滑无闪烁（原子交换 `prepare_atomic_swap` / `complete_atomic_swap`，旧壁纸保持显示直到新壁纸就绪） |
| FR-006 | 壁纸删除 | P1 | 支持从壁纸库中移除壁纸，可选是否删除源文件（`remove_wallpaper` 命令 `delete_file` 参数）。删除文件前做 data_dir 边界校验，防配置篡改导致越界删除 |
| FR-007 | 壁纸信息显示 | P2 | 显示壁纸的文件名、类型、分辨率、文件大小等基本信息（`get_wallpapers` 返回含 `file_size` 的条目，分辨率经 `validate_and_get_metadata` 派生） |
| FR-034 | 壁纸缩放模式 | P1 | 支持壁纸缩放/适配模式设置：填充（Fill）、适应（Fit）、拉伸（Stretch）、居中（Center）、平铺（Tile）。Image/GIF 通过 PostMessageW 触发重绘；Video 通过 mpv 启动参数设置（`--panscan` / `--video-unscaled` / `--keepaspect`，运行时仅记录，需重启 mpv 生效）；Web 仅记录模式（WebView2 自行处理渲染）。缩放模式按显示器记忆（`scaling.modes`，特殊键 `"all"` 为兜底） |

### 5.2 桌面集成

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-008 | WorkerW 桌面嵌入 | P0 | 将壁纸窗口嵌入 Windows 桌面的 WorkerW 层，使壁纸显示在桌面图标下方。仅动态壁纸（视频/GIF/网页）与不支持原生 API 的静态图片需要 WorkerW 嵌入；jpg/jpeg/png/bmp/tif/tiff/dib 静态壁纸使用 Windows 原生 API，无需 WorkerW |
| FR-009 | 壁纸窗口定位 | P0 | 壁纸窗口精确定位到桌面工作区，覆盖整个桌面背景区域。WorkerW 异步初始化（后台线程），不阻塞启动流程 |
| FR-010 | 桌面图标交互 | P0 | 壁纸运行时，桌面图标仍可正常点击、选择、拖动 |
| FR-035 | Explorer 重启检测 | P1 | 检测 Windows Explorer 重启事件，自动重新嵌入壁纸窗口到新的 WorkerW 层（`TaskbarCreated` 广播消息事件驱动即时恢复；`workerw_check.rs` 5 分钟轮询检测 WorkerW 句柄有效性作为兜底） |
| FR-039 | DPI 缩放适配 | P1 | 壁纸窗口在 Per-Monitor DPI V2 模式下正确显示（`manifest.xml` / `app.manifest` 声明 `PerMonitorV2`），支持不同 DPI 缩放比例的多显示器环境，DPI 变更时自动调整壁纸尺寸（GDI 渲染器处理 `WM_DPICHANGED`，`gdi_base.rs` 重新 `SetWindowPos` 并失效屏幕尺寸缓存） |

### 5.3 多显示器

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-011 | 多显示器识别 | P0 | 自动检测并列出系统中所有连接的显示器（`enumerate_displays`，带 5s TTL 缓存） |
| FR-012 | 独立壁纸设置 | P0 | 支持为每个显示器独立设置不同的壁纸 |
| FR-013 | 显示器热插拔 | P1 | **已实现**：调度器 `src-tauri/src/scheduler.rs` 每 3 秒轮询检测显示器集合变化（`display_set_changed`，顺序无关、同值短路）并发出 `displays-changed` 事件、重建布局；GDI 渲染器处理 `WM_DISPLAYCHANGE` 重新查询实际屏幕尺寸并调整壁纸窗口定位（`gdi_base.rs` / `invalidate_screen_size_cache`）。UI 侧同步：`src-tauri/src/ui_process/bridge.rs::FORWARDED_EVENTS`（14 个事件）**已含** `displays-changed`，UI 进程（`crates/mirrorstar-ui/src/store.rs::apply_event`）收到后刷新显示器选择卡列表、清理已消失显示器的缩放记忆（保留 `"all"` 键）并收敛失效的当前选择；转发断链已修复并经注入式端到端验证 |
| FR-014 | 跨显示器壁纸 | P2 | **已实现**：通过 `Arrangement::Span`（跨屏拉伸，单一跨屏渲染器贯通虚拟桌面）支持将单个壁纸跨所有显示器显示。相关排列见 FR-046 |

### 5.4 暂停与恢复

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-015 | 全屏应用检测 | P0 | 检测到全屏应用（如游戏、全屏视频）运行时自动处置所有壁纸，退出全屏时恢复。采用**事件驱动**机制（`src-tauri/src/platform/fullscreen.rs` 的 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`，非轮询）+ 周期复查线程。处置策略由 `pause.fullscreen_action` 决定：`none`（不处置）/ `pause`（暂停，退出全屏即时恢复）/ `terminate`（终止子进程，退出全屏冷启动恢复，默认）。注：当前为全局处置，按显示器独立处置为未来改进项 |
| FR-016 | 手动暂停/恢复 | P0 | 支持用户手动暂停和恢复壁纸播放（托盘菜单 + 快速通道 `pause_wallpaper_fast` / `resume_wallpaper_fast`） |
| FR-017 | 暂停时静态帧 | P1 | 壁纸暂停时显示当前帧的静态画面，而非黑屏或隐藏（GIF 保留当前帧像素；Video 暂停 mpv 停留当前帧） |
| FR-018 | 音频暂停 | P1 | 壁纸暂停时同步静音，恢复时恢复音量（Video 渲染器维护 `pre_mute_volume`，暂停降为静音、恢复还原） |
| FR-032 | 电池供电暂停 | P2 | **已实现**：检测到系统切换到电池供电时自动暂停壁纸播放，接通电源后自动恢复。基于 `WM_POWERBROADCAST` / `PBT_APMPOWERSTATUSCHANGE` 事件实现（`src-tauri/src/platform/power.rs`，`POWER_WAS_ON_BATTERY` 跟踪上一状态防重复切换，`ACLineStatus=255` 未知态显式跳过）。开关为 `pause.pause_on_battery`（配置默认 `false`，需用户在设置中启用） |

### 5.5 系统托盘

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-019 | 托盘图标 | P0 | 程序启动时仅创建系统托盘图标，不创建主窗口。托盘菜单文本切换暂停/恢复状态（图标本身不随状态变化，仅菜单项文本反映当前状态）。主窗口按需启动：用户点击托盘"打开主窗口"时，由壳侧按需 spawn 原生 UI 子进程 `mirrorstar-ui`（Slint），窗口隐藏超时后 UI 子进程退出回收内存 |
| FR-020 | 托盘菜单 | P0 | 右键托盘图标弹出菜单，包含：打开（按需启动原生 UI 子进程）、暂停壁纸/恢复壁纸、纯净壁纸模式（勾选切换，按任意键退出）、退出。注：更丰富的托盘菜单（音量控制子菜单、显示器壁纸信息子菜单）为未来改进项 |
| FR-021 | 关闭/隐藏窗口 | P0 | 主窗口隐藏（hide）后，UI 子进程在空闲超时（默认 300s，`MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`，tick 30s）后退出回收内存；再次打开时按需重新拉起 UI 子进程。同一时刻至多一个 UI 实例 |
| FR-022 | 开机自启 | P1 | 支持设置开机自动启动（tauri-plugin-autostart，写入注册表启动项，无需管理员权限） |
| FR-040 | 单实例运行 | P0 | 程序同时只能运行一个实例，重复启动时提示"镜星壁纸已在运行中"并退出新实例（通过 Windows 命名互斥体 `MirrorStarWallpaper_SingleInstance` 实现）。`--wp-proc` 子进程模式在单实例检测之前分派，不参与互斥体检测 |

### 5.6 配置管理

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-023 | 配置持久化 | P0 | 用户设置（当前壁纸、音量、暂停策略等）持久化保存，重启后恢复（`config.toml` + 壁纸库 `wallpapers.toml`，TOML 格式） |
| FR-024 | 壁纸库管理 | P0 | 维护用户添加的壁纸列表，包含壁纸路径、类型、缩略图、所属池等信息 |
| FR-025 | 音量控制 | P1 | 支持调节视频/网页壁纸的音频音量，包括全局静音（通过 Core Audio API `IAudioSessionManager2` 按子进程 pid 控制音频会话音量，`crates/mirrorstar-core/src/audio/volume.rs`） |
| FR-026 | 播放速度 | P2 | 支持调节视频和 GIF 壁纸的播放速度。视频壁纸通过 mpv IPC `set_speed` 命令实现；GIF 壁纸通过 `GifCommand::SetSpeed` 调整定时器间隔实现。UI 设置滑块量程 **0.25×~4.0×**（`mirrorstar-ui` `SPEED_MIN`/`SPEED_MAX`）；core 配置层 `VideoConfig.speed` 校验上限 `MAX_VIDEO_SPEED=10.0`（越界回退默认 1.0） |
| FR-027 | 配置文件路径 | P2 | **已修正为便携化**：用户数据（`config.toml`、`wallpapers.toml`、`wallpapers\`、`thumbnails\`、`logs\`）统一收束到**程序安装目录（exe 所在目录）**，而非 `%APPDATA%`；解析顺序为 `MIRRORSTAR_DATA_ROOT` 环境变量 → `current_exe().parent()` → 回退 `%APPDATA%/mirrorstar` |
| FR-036 | 壁纸搜索 | P2 | 支持按文件名在壁纸库中实时搜索过滤壁纸，不区分大小写（`mirrorstar-ui` `dnd.rs::matches_keyword` / `filter_indices`） |
| FR-037 | 配置热重载 | P2 | 支持配置文件的热重载（`crates/mirrorstar-core/src/config/hot_reload.rs`，基于 `notify`，监视 `config.toml` 与 `wallpapers.toml`，500ms 防抖）。用户通过外部编辑器修改配置文件后，程序自动检测变更并应用新配置（部分配置项立即生效，部分需重启） |
| FR-038 | WebView2 数据目录管理 | P2 | **部分实现**：每个 wp-proc 实例使用独立用户数据目录 `data_root()/webview2-cache/<pid>/`（`wp_proc_user_data_folder` 为路径单源，支持多显示器并发互不干扰）；wp-proc 退出时回收本实例目录（`terminate()` / `Drop`，幂等），应用启动时 `cleanup_webview2_cache` 兜底清空崩溃残留。🔧 **未实现**："定期清理超过 30 天未使用的数据目录"的 TTL 策略（当前改为启动兜底清空） |

### 5.7 用户交互

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-028 | 拖放添加壁纸 | P0 | 支持将文件从资源管理器拖放到主窗口（Slint UI 原生窗口 `DragAcceptFiles` / `WM_DROPFILES`）来添加壁纸，去重并过滤不支持类型（`mirrorstar-ui` `dnd.rs`） |
| FR-029 | 文件选择添加 | P0 | 支持通过系统文件选择对话框（tauri-plugin-dialog）添加壁纸文件 |
| FR-030 | 壁纸缩略图 | P1 | 壁纸库中显示壁纸的静态缩略图（核心生成 320×180 JPEG：图片直出、视频取首帧；无缩略图时 Image/GIF 回退直载源图，网页/视频无缩略图时显示类型占位图标） |
| FR-031 | 鼠标穿透 | P2 | 可开启壁纸的鼠标穿透模式（`desktop/window.rs::set_mouse_passthrough`，`WS_EX_TRANSPARENT`），使壁纸不接收鼠标事件。WorkerW 壁纸默认即为点击穿透 |
| FR-033 | 鼠标交互模式 | P2 | 🔧 **未实现**：网页壁纸当前为点击穿透（WorkerW 之上、不接收任何输入），暂无"可接收鼠标点击事件"的交互模式开关 |

### 5.8 前端用户体验（UI）

原生 UI 为独立的 Slint 子进程 `crates/mirrorstar-ui`（Slint 1.18，软件渲染，事件循环在主线程）。UI 与壳进程通过 JSON 协议通信（`crates/mirrorstar-core/src/ui_proto/` + `crates/mirrorstar-ui/src/protocol.rs`），壳侧命令经 `src-tauri/src/ui_process/bridge.rs` 分派。

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-041 | 壁纸预览 | P2 | 支持点击壁纸卡片弹出居中覆盖层预览（`mirrorstar-ui/src/preview/`：photo/gif/video/web 四种宿主），图片/GIF/视频内嵌预览、网页按需创建 WebView2 子窗口；支持关闭（遮罩/ESC）。详见 FR-048 播放器控制 |
| FR-042 | 壁纸搜索（同 FR-036） | P2 | 在壁纸库中实时搜索过滤壁纸，不区分大小写，清空搜索框恢复显示全部 |
| FR-043 | 加载动画 | P2 | 壁纸列表加载时显示骨架屏占位（`main.slint` 骨架屏 + `store.rs` loading 态）；缩略图加载失败显示类型图标/占位并渐显 |
| FR-044 | 响应式设计 | P2 | 🔧 **待复测/待验证**：原"窗口宽度 < 768px 时设置面板改为可折叠抽屉式、网格列数自适应"在 Slint UI 中未见对应实现（`mirrorstar-ui` 无 768px 断点/抽屉折叠逻辑），需在实机确认当前窗口自适应行为 |

**原生 UI 模块**（`crates/mirrorstar-ui/src/`，Rust + Slint 软件渲染）：

| 模块 | 能力 |
|------|------|
| `main.rs` | 窗口/事件循环装配、虚拟化壁纸网格模型投影（`buffer_rows` 行缓冲窗口化，支持 1,000+ 项）、回调接线（拖拽、预览、设置、播放控制） |
| `store.rs` | 状态仓：壁纸库/池与单元/设置快照、增量事件应用（`apply_event`，含 `displays-changed` / `audio-disabled` / `wallpaper-rotated` 处理）、设置写回与合并（`merge_config_patch`）、速度/音量 sanitize |
| `grid.rs` | 网格几何与命中（列数/行高、可见窗口切片、`insert_index` 拖拽插入位计算） |
| `dnd.rs` | 文件拖入合并（`merge_drop_paths`：去重 + 过滤不支持类型）、拖拽重排纯函数（`reorder_ids`）、搜索匹配（`matches_keyword` / `filter_indices`） |
| `drop_target.rs` | 自实现 OLE `IDropTarget`（获得 DragEnter/DragLeave 悬停高亮） |
| `keynav.rs` | 键盘导航（方向键移动网格焦点、删除等动作白名单；预览态动作转发/陷阱） |
| `thumbs.rs` / `videothumb.rs` | 缩略图来源决策（磁盘缩略图 → 源图直载 → 类型占位）与视频缩略图加载 |
| `preview/`（`mod.rs`/`photo.rs`/`gif.rs`/`video.rs`/`web.rs`/`host_child.rs`） | 预览弹窗：图片/GIF/视频/网页四种宿主，视频用宿主原生子窗口承载 |
| `settings.rs` | 设置面板模型：标签/选项（多屏排列、全屏处置、采样算法）、速度滑块与 sanitize、事件 Toast 映射 |
| `protocol.rs` | UI ↔ 壳 JSON 协议客户端（请求/响应匹配、事件分发、`slint_dispatch` 投递到 UI 线程） |
| `window_pos.rs` / `window_icon.rs` / `focus.rs` / `desktop.rs` | 窗口定位、图标、焦点与桌面交互辅助 |

### 5.9 新增功能需求（v3.0 补充）

| 需求 ID | 需求名称 | 优先级 | 描述 |
|---------|----------|--------|------|
| FR-045 | 纯净壁纸模式（Pure Mode） | P2 | 隐藏桌面图标与任务栏，仅呈现壁纸，营造沉浸式桌面；进入后按任意键（或再次触发）退出。底层由 `crates/mirrorstar-core/src/pure_mode/`（进程级活跃标志、置顶窗口注册/断言、临时顶层呈现窗口 `lift_window_to_rect`）与 `src-tauri/src/platform/{pure_mode.rs, pure_mode_input.rs, liveness_watchdog.rs}`（输入监听、存活看护）实现；入口为托盘菜单"纯净壁纸（按任意键退出）"勾选项 |
| FR-046 | 多屏排列 | P1 | 支持三种多屏排列方式（`Arrangement`，`config/settings.rs`，`src-tauri/src/scheduler.rs` 编排）：**每屏独立（PerMonitor）** / **全部相同（AllSame）** / **跨屏拉伸（Span）**。UI 设置面板提供"多屏排列"下拉（`mirrorstar-ui` `settings.rs`），非 PerMonitor 排列下"目标显示器"选择置灰 |
| FR-047 | 壁纸拖拽排序 | P2 | 支持在壁纸库网格中拖拽卡片调整顺序（`mirrorstar-ui`：`on_drag_started` / `on_drag_insert_index` / `on_drag_commit`，`dnd::reorder_ids` 计算新顺序，本地先行 `reorder_local` 后下发 `reorder_wallpapers` 命令持久化）。拖拽中冻结列数重算，含拖拽幽灵图（drag ghost） |
| FR-048 | 预览播放器控制 | P2 | 预览弹窗内提供**两行式播放器带**（高 **76px**，上下内边距**对称各 6px**：6 + 进度行 24 + 间隔 6 + 控制行 34 + 6）：**上层进度行**紧贴内容区下缘独占一行，含**当前时间 / 进度条可拖动 seek / 总时长**，进度条为 **4px 细轨道（圆角 2px）、悬停或拖动时增高至 6px**，拖柄为 12px 圆点、**默认隐藏、悬停或拖动时才出现**（命中区仍覆盖整行高度，拖动/点击 seek 逻辑与比例换算不变）；**下层控制行**为居中传输簇 `−5s / 播放暂停 / +5s`，右侧 `播放速度 / 静音 / 音量滑块`，**控制带下方不再有常驻说明文字**（原小字说明已移除，释放的高度归还视频内容区）。其中 **−5s / +5s** 为相对跳转按钮（复用 `preview::seek_relative`，时长元数据未就绪时为 no-op），**播放/暂停**、**音量滑块**与**静音**（实时下发，释放后持久化）、**全屏/退出全屏**、**ESC 关闭**（预览态 ESC 语义关闭弹窗，Tab/Backtab 在弹窗内循环）能力均保留（播放速度见 FR-049）。GIF 仅提供单行播放/暂停；图片与网页无控制带 |
| FR-049 | 播放速度精细化 | P2 | 设置面板"播放速度"行提供内联 **−/+ 微调按钮**（步长 **0.01×**，先量化到 0.01 再 clamp，避免浮点漂移），滑块量程 0.25×~4.0×；按钮在滑块达到边界时**置灰**（`minus-enabled` / `plus-enabled`）。预览底部带另提供**内联横向档位条**，覆盖 **0.25×–4.0× 共 16 档、相邻档位间隔恒为 0.25×**（0.25× / 0.5× / 0.75× / 1× / 1.25× / 1.5× / 1.75× / 2× / 2.25× / 2.5× / 2.75× / 3× / 3.25× / 3.5× / 3.75× / 4×），展开时**原位替换进度行** |