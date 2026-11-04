# MirrorStar Wallpaper（镜星壁纸）需求文档 — 约束与术语 / Constraints & Terminology

[← 返回文档索引](../index.md) > [需求文档](./01-项目概述-Overview.md) > 约束与术语

> 相关文档：[项目概述](./01-项目概述-Overview.md) | [功能性需求](./02-功能性需求-Functional-Requirements.md) | [非功能性需求](./03-非功能性需求-Non-Functional-Requirements.md) | [用例](./04-用例-Use-Cases.md)

## 9. 约束与假设

### 9.1 约束

| 编号 | 约束 | 说明 |
|------|------|------|
| CON-001 | 操作系统限制 | 仅支持 Windows 10 (1809+) 和 Windows 11 |
| CON-002 | 架构限制 | 仅支持 x86_64 架构 |
| CON-003 | WebView2 依赖 | 网页壁纸与网页预览的网页渲染依赖 Microsoft Edge WebView2 Runtime（Windows 11 通常内置；Windows 10 视版本/设备可能需用户自行安装）。**程序不提供内置安装引导 / Bootstrapper**；缺失时网页壁纸与网页预览不可用，其余壁纸类型不受影响 |
| CON-004 | mpv 依赖 | 视频壁纸功能依赖外部 `mpv.exe` 可执行文件。`VideoRenderer::find_mpv()` 复用 `SubprocessRendererBase::find_bundled_executable(Some("mpv"), "mpv.exe", "mpv")`，查找优先级为：① 应用 exe 同目录下的 `mpv\mpv.exe`（捆绑分发路径）；② 从 exe 目录起向上回溯最多 8 层祖先目录的 `<祖先>\mpv\mpv.exe`（覆盖开发模式 `target\debug\` → 仓库根 `mpv\`）；③ 以上均未命中则回退为 `mpv.exe`，交由操作系统在 `PATH` 中查找。即**应用目录 > 仓库根回溯 > PATH** |
| CON-005 | 桌面窗口管理器 | 依赖 Windows Desktop Window Manager (DWM)，不支持禁用 DWM 的环境 |
| CON-006 | Rust 工具链 | 开发使用 Rust stable 工具链；workspace `Cargo.toml` 声明 `edition = "2021"`、`rust-version = "1.80"`（MSRV）。`rust-toolchain.toml` 固定 `channel = "stable"` 并声明 `rustfmt`/`clippy` 组件（**不固定具体版本号**，低于 MSRV 时 cargo 会给出明确错误） |
| CON-007 | 无跨平台计划 | 面向 Windows，不考虑 macOS/Linux 移植 |
| CON-008 | 网页来源限制 | 添加入口仅接受本地 `.html` / `.htm` 文件；远程 http/https 加载由 `ALLOWED_REMOTE_DOMAINS` 白名单控制，**默认为空 = 全部拒绝**，仅 `localhost` / `127.0.0.1` 放行；源字符串以 `javascript:` / `data:` / `vbscript:` / `file:` 开头一律拒绝，本地文件路径由程序转换为 `file://` 后交给 WebView2 |
| CON-009 | 数据根与托管复制 | 数据根解析顺序为 `MIRRORSTAR_DATA_ROOT` → exe 所在目录 → `%APPDATA%\mirrorstar`；外部壁纸文件复制进 `wallpapers/{id}/{filename}`，壁纸库自包含；安装版删除安装目录即完成全量清理 |
| CON-010 | 轮换限制 | Web 类型壁纸不参与轮换；候选（池成员且文件存在）少于 2 个时不轮换；轮换间隔下限 1 分钟、UI 上限 1440 分钟；非 `PerMonitor` 排列下仅存在全局单元 `"all"` |

> 说明：历史约束「资源加载协议（wpfile）」已废弃并移除，不是现行实现。`wpfile://`（`http://wpfile.localhost`）自定义协议属于**已废弃的旧 WebView2 前端**，用于经 `convertFileSrc` 读取本地壁纸资源；该前端已整体删除，当前主窗口为 **Slint 原生 UI 子进程**（`mirrorstar-ui`），不使用该协议——缩略图/预览由原生进程**直接按本地文件路径解码**（见 `crates/mirrorstar-ui/src/thumbs.rs`，其中仅残留一条对已废弃协议的说明性注释，非实际实现）。网页壁纸的本地资源加载由沙箱化的临时 WebView2（`--wp-proc` 子进程）自行按文件路径/URL 处理。当前代码库中已无 wpfile 协议注册与 scope 校验实现。

### 9.2 假设

| 编号 | 假设 | 说明 |
|------|------|------|
| ASM-001 | 用户系统已安装显卡驱动 | 硬件加速视频解码依赖正常工作的显卡驱动 |
| ASM-002 | 用户系统内存 ≥ 4GB | 低内存设备可能无法流畅运行动态壁纸 |
| ASM-003 | WebView2 Runtime 可用 | 假设 Windows 11 用户已有 WebView2，Windows 10 用户可安装 |
| ASM-004 | 用户有基本的文件管理能力 | 用户能够通过资源管理器找到和管理壁纸文件 |
| ASM-005 | 不与同类软件共存 | 假设用户不会同时运行多个动态壁纸软件 |

---

## 10. 术语表

| 术语 | 英文 | 定义 |
|------|------|------|
| 原生壁纸模式 | Native Mode | 通过 Windows 原生 API（`SystemParametersInfoW` + 注册表）设置静态壁纸，零渲染器、不嵌入 WorkerW。原生 API 支持的格式为 jpg/jpeg/png/bmp/tif/tiff/dib；添加识别列表另含 `ico` 与 `webp`，但这两类不走原生 API，改走 WorkerW 模式 |
| WorkerW 模式 | WorkerW Mode | 动态壁纸（GIF/视频/网页）及不被原生 API 支持的静态图片（如 WebP/ICO）所走的渲染路径，含两条嵌入路径：`RaisedLayered`（Win11 raised desktop，即 Progman 的 layered 子窗口，以 DefView 为锚点插入）与 `LegacyWorkerW`（旧层 `SetParent(WorkerW)` + `HWND_BOTTOM`）；`WallpaperMode::WorkerW` 为二者统称 |
| WorkerW | WorkerW | Windows 桌面窗口层次结构中的壁纸承载层。旧层级中位于桌面图标（`SHELLDLL_DefView`）下方；Win11 raised desktop 下壁纸实际作为 Progman 的 layered 子窗口位于 DefView 下方，WorkerW 变为 Progman 的直接子窗口 |
| Progman | Program Manager | Windows 桌面窗口层次结构的顶层窗口，是桌面背景的容器 |
| DWM | Desktop Window Manager | Windows 桌面窗口管理器，负责窗口合成和视觉效果 |
| mpv | mpv | 开源的跨平台媒体播放器，MirrorStar 通过启动外部 `mpv.exe` 子进程作为视频壁纸的后端，通过命名管道 `\\.\pipe\mirrorstar-mpv-*` 接收 mpv 原生 JSON IPC 命令 |
| mirrorstar-wp-proc | mirrorstar-wp-proc | 承载网页壁纸的 **库 crate**（非独立 exe）。逻辑由壳 exe（`mirrorstar-wallpaper.exe`）在 `--wp-proc` 模式下经其 `run` / `run_with_argv` 入口调用，作为独立子进程承载 WebView2 渲染网页壁纸（崩溃隔离不变）；主进程通过命名管道 `\\.\pipe\mirrorstar-wp-*`（高熵 UUID，难以猜中）与 wp-proc 通信 |
| WebView2 | Microsoft Edge WebView2 | Microsoft 提供的 WebView 控件，基于 Chromium 内核。用于网页壁纸（`--wp-proc` 子进程）与预览弹窗的按需临时渲染 |
| SHELLDLL_DefView | Shell Default View | Windows 桌面窗口层次结构中管理桌面图标显示的窗口 |
| 硬件加速解码 | Hardware-accelerated decoding | 利用 GPU 进行视频解码（mpv `--hwdec=auto`），降低 CPU 占用 |
| 事件驱动 | Event-driven | 壁纸状态变更（全屏、电源、Explorer 重启）通过 Windows 消息 / WinEvent 回调触发，而非轮询；实现暂停态近似零 CPU 占用 |
| PauseSender | PauseSender | 暂停/恢复快速通道：绕过引擎 Mutex，直接向渲染线程发送暂停命令，实现 < 10ms 的快速响应 |
| 工作集内存 | Working Set Memory | 进程当前驻留在物理内存中的页面集合，反映实际物理内存占用 |
| 壁纸库 | Wallpaper Library | 用户添加的壁纸集合，包含壁纸的元信息和缩略图；外部文件复制进托管目录 `data_root/wallpapers/{id}/`；库内顺序可拖拽调整，影响隐式「全部壁纸池」的 Sequential 采样顺序 |
| playback.toml | playback.toml | `data_root()` 下的播放状态持久化文件，记录单元激活池、顺序游标、历史栈、洗牌袋等状态；文件损坏或版本不符时回退为默认空状态 |
| 壁纸池 | Pool | `Pool { id, name, member_ids }`，持久化于 `wallpapers.toml` 的 `pools` 段；成员顺序即播放顺序 |
| 隐式全部池 | Implicit All Pool | `active_pool = None` 时使用壁纸库中的全部有效壁纸；对应 UI 下拉首项「全部壁纸池」 |
| 采样算法 | Order | 轮换候选的取样策略：`sequential` / `shuffle_bag` / `pseudo_random`；Web 类型与失效文件被剔除，候选少于 2 个时不轮换 |
| 轮换 | Rotation | 按 `rotation.enabled`、`interval_minutes`、`on_boot`、`order` 配置的定时切换；暂停期间抑制定时轮换 |
| 托管壁纸库 | Managed Library | 外部壁纸文件复制到 `data_root/wallpapers/{id}/`，壁纸库自包含，不依赖源文件路径 |
| 日志留存 | Log Retention | 应用日志保留 7 个按日轮转文件、mpv 日志剪枝、`crash.log` 超过 1MB 时轮转 |
| 内存归因 | Memory Report | `get_memory_report` 按进程树归类内存占用：主进程 / mpv / wp-proc / msedgewebview2 / other |
| 托盘图标 | System Tray Icon | Windows 任务栏通知区域（系统托盘）中的图标 |
| 全屏检测 | Fullscreen Detection | 检测系统中是否有应用程序以全屏模式运行的技术（`SetWinEventHook` / `EVENT_SYSTEM_FOREGROUND`） |
| 冷启动 | Cold Start | 程序从完全关闭状态启动的过程 |
| 热启动 | Warm Start | 程序从最小化/后台状态恢复到前台的过程，窗口从零创建（非从隐藏状态恢复） |
| 鼠标穿透 | Click-through | 壁纸窗口不拦截鼠标事件，使鼠标操作直接传递到下层窗口 |
| 纯净壁纸模式 | Pure Mode | 由托盘"纯净壁纸"勾选项进入的沉浸模式（core `pure_mode` + `src-tauri/src/platform/pure_mode*.rs`）：将壁纸窗口从桌面层（WorkerW / raised desktop 的 Progman 层）摘出并以 `HWND_TOPMOST` 置顶全屏、安装输入捕获钩子且**刻意不抢前台**（`WS_EX_NOACTIVATE`），按任意键/点击鼠标/再次点击托盘即退出。退出编排保证"先恢复桌面再卸载钩子"，并做视频渲染器存活校验与自愈 |
| 多屏排列 | Arrangement | 多显示器布局策略枚举（core `config::settings::Arrangement`，由 `mirrorstar-core::layout` 作为唯一权威逻辑）。三态：`PerMonitor`（每屏独立，每显示器一个调度单元，key = 显示器 id）、`AllSame`（每屏同图，唯一全局单元 `all`，一张壁纸各屏顺序各 apply 一次，N 渲染器）、`Span`（跨屏合并，唯一全局单元 `all`，1 个跨屏渲染器贯通虚拟桌面、1 次 apply） |
| 调度单元 | Unit | 多屏排列下壁纸应用的粒度：`PerMonitor` 下每显示器一个单元；`AllSame`/`Span` 下唯一全局单元 `all`（常量 `ALL_UNIT_KEY`） |
| Slint | Slint | 用于构建 MirrorStar **主窗口**的原生 GUI 工具包。主窗口是独立的 Slint 原生 UI 子进程（`mirrorstar-ui.exe`），**非 Tauri WebView 前端**；Tauri v2 仅承担托盘/对话框/自启/后端命令等壳职责 |
| mirrorstar-ui | mirrorstar-ui | 主窗口 UI 的独立子进程 crate（Slint 原生渲染）。由壳侧按需 spawn，窗口隐藏超时（默认 300s）后退出回收内存；同一时刻至多一个实例 |
| ui_proto | UI protocol | `mirrorstar-core` 中的公开模块，定义壳（`src-tauri`）与 UI 子进程（`mirrorstar-ui`）之间的 **NDJSON over stdio** 协议（命令与事件结构）。UI 侧经事件桥（`src-tauri/src/ui_process/bridge.rs` 的 `FORWARDED_EVENTS`）接收壳侧事件 |
| 预览播放器化 | Player-ized Preview | 预览弹窗底部控制条（chrome）具备播放器级控制：播放/暂停、进度拖动、倍速、音量/静音、全屏（四角箭头）；`Esc` 在全屏时先退全屏再关闭弹窗。实现在 `crates/mirrorstar-ui/src/preview/`（视频经 mpv IPC，GIF 经 Slint 定时器；网页预览使用临时 WebView2，无控制带） |
| 托盘溢出浮出窗 | Tray Overflow Flyout | 任务栏"隐藏栏"浮出窗（Win11 类名 `TopLevelWindowForOverflowXamlIsland`，旧版 `NotifyIconOverflowWindow`）。进入纯净壁纸模式时因不抢前台需主动关闭（`WM_CLOSE`，失败回退 `SW_HIDE`），否则会叠在置顶壁纸之上 |
