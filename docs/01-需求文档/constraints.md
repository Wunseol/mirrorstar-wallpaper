# MirrorStar Wallpaper（镜星壁纸）需求文档 — 约束与术语

[← 返回文档索引](../README.md) > [需求文档](./overview.md) > 约束与术语

> 相关文档：[项目概述](./overview.md) | [功能性需求](./functional-requirements.md) | [非功能性需求](./non-functional-requirements.md) | [用例](./use-cases.md)

## 9. 约束与假设

### 9.1 约束

| 编号 | 约束 | 说明 |
|------|------|------|
| CON-001 | 操作系统限制 | 仅支持 Windows 10 (1809+) 和 Windows 11 |
| CON-002 | 架构限制 | 仅支持 x86_64 架构 |
| CON-003 | WebView2 依赖 | 网页壁纸功能依赖 Microsoft Edge WebView2 Runtime，Windows 11 已内置，Windows 10 (2004+) 已预装；Windows 10 (1809~2003) 需用户安装或程序引导安装 WebView2 Bootstrapper |
| CON-004 | mpv 依赖 | 视频壁纸功能依赖外部 mpv.exe 可执行文件，需在 PATH 中或捆绑在应用目录的 mpv/ 子目录下。find_mpv() 支持捆绑路径优先查找 + PATH 回退 |
| CON-005 | 桌面窗口管理器 | 依赖 Windows Desktop Window Manager (DWM)，不支持禁用 DWM 的环境 |
| CON-006 | Rust 工具链 | 开发需要 Rust stable 工具链（1.80+） |
| CON-007 | 无跨平台计划 | v1 版本仅面向 Windows，不考虑 macOS/Linux 移植 |

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
| WorkerW | WorkerW | Windows 桌面窗口层次结构中的一个层，位于桌面图标（SHELLDLL_DefView）下方，动态壁纸通过将窗口嵌入此层实现在图标下方显示。注意：静态图片（JPG/PNG/BMP）不再使用 WorkerW 嵌入，改用 Native 壁纸 API |
| Progman | Program Manager | Windows 桌面窗口层次结构的顶层窗口，是桌面背景的容器 |
| DWM | Desktop Window Manager | Windows 桌面窗口管理器，负责窗口合成和视觉效果 |
| mpv | mpv | 开源的跨平台媒体播放器，MirrorStar 通过启动外部 mpv.exe 子进程作为视频壁纸的后端 |
| WebView2 | Microsoft Edge WebView2 | Microsoft 提供的 WebView 控件，基于 Chromium 内核，用于在应用中嵌入网页内容 |
| SHELLDLL_DefView | Shell Default View | Windows 桌面窗口层次结构中管理桌面图标显示的窗口 |
| 硬件加速解码 | Hardware-accelerated decoding | 利用 GPU 进行视频解码，降低 CPU 占用 |
| 工作集内存 | Working Set Memory | 进程当前驻留在物理内存中的页面集合，反映实际物理内存占用 |
| 壁纸库 | Wallpaper Library | 用户添加的壁纸集合，包含壁纸的元信息和缩略图 |
| 托盘图标 | System Tray Icon | Windows 任务栏通知区域（系统托盘）中的图标 |
| 全屏检测 | Fullscreen Detection | 检测系统中是否有应用程序以全屏模式运行的技术 |
| 冷启动 | Cold Start | 程序从完全关闭状态启动的过程 |
| 热启动 | Warm Start | 程序从最小化/后台状态恢复到前台的过程，窗口从零创建（非从隐藏状态恢复） |
| 鼠标穿透 | Click-through | 壁纸窗口不拦截鼠标事件，使鼠标操作直接传递到下层窗口 |
| 缩略图 | Thumbnail | 壁纸的缩小预览图，用于壁纸库展示 |
| GDI 双缓冲 | GDI Double Buffering | Windows 图形设备接口双缓冲技术，通过 CreateCompatibleDC 创建内存 DC，StretchDIBits 绘制到内存 DC，BitBlt 一次性拷贝到屏幕 DC，消除闪烁。ImageRenderer 和 GifRenderer 使用此技术渲染壁纸 |
| MpvIpcClient | Mpv IPC Client | mpv 原生 IPC 客户端（ipc/protocol.rs），通过命名管道（mirrorstar-mpv-{uuid}）与 mpv.exe 子进程通信，使用 mpv 原生 JSON 协议（JSON 行 + request_id 匹配） |
| WpProcIpcClient | WpProc IPC Client | Web 壁纸子进程 IPC 客户端（ipc/wp_proc.rs），通过命名管道与 mirrorstar-wp-proc.exe 通信，使用自定义 WpProcCommand JSON 协议（Play/Terminate/SetPosition/Navigate/Pause/Resume） |
| HALFTONE | HALFTONE | GDI 缩放模式常量（SetStretchBltMode），提供高质量图像缩放，ImageRenderer 和 GifRenderer 使用此模式渲染缩放后的壁纸帧 |
| TOML | Tom's Obvious Minimal Language | 一种最小化的配置文件格式，语法简洁，人类可读 |
| 子进程隔离 | Subprocess Isolation | 将壁纸渲染引擎作为独立子进程运行，崩溃不影响主进程。仅网页壁纸（wp-proc）和视频壁纸（mpv.exe）以子进程方式运行；GIF 与静态图片壁纸在主进程内渲染 |
| 事件驱动 | Event-driven | 程序架构模式，通过事件触发操作而非轮询，实现低 CPU 占用 |
| Tauri | Tauri | 基于 Rust 的桌面应用框架，使用系统 WebView 渲染前端 UI，Rust 处理后端逻辑 |
| Named Pipe | Named Pipe | Windows 命名管道，一种进程间通信机制，支持双向、可靠、有序的数据传输 |
| IPC | Inter-Process Communication | 进程间通信，不同进程之间交换数据和信号的机制 |
| SetWinEventHook | SetWinEventHook | Windows API，用于注册事件钩子回调，监听系统事件（如窗口切换、焦点变化） |
| UUID | Universally Unique Identifier | 通用唯一标识符，用于命名管道命名，避免 PID 在进程启动前未知的问题 |
| COM | Component Object Model | 微软组件对象模型，Windows 系统接口的基础，Core Audio API 基于 COM |
| bitflags | bitflags | Rust crate，用于定义位掩码类型。注：声明为 workspace 依赖但源码中未实际使用（暂停原因跟踪使用 AtomicBool 而非 bitflags） |
| Per-Monitor DPI | Per-Monitor DPI | Windows 8.1+ 特性，每个显示器可以有独立的 DPI 缩放比例 |
| 命名互斥体 | Named Mutex | Windows 内核对象，用于确保同一时间只有一个程序实例运行，通过 CreateMutexW API 创建 |
| WallpaperMode | Wallpaper Mode | 壁纸模式枚举，区分 Native（Windows 原生 API）和 WorkerW（窗口嵌入）两种壁纸设置路径 |
| PauseSender | Pause Sender | 暂停/恢复快速通道，绕过引擎 Mutex 直接向渲染线程发送命令的 mpsc 通道 |
| SystemParametersInfoW | System Parameters Info W | Windows 系统参数设置 API，用于原生壁纸设置 |
| WallPaperStyle | Wallpaper Style | 注册表键值，控制壁纸缩放模式（0=居中, 2=拉伸, 6=适应, 10=填充, 22=跨区） |
| Native Wallpaper | Native Wallpaper | 使用 Windows 原生 API 设置的静态壁纸，零资源占用 |
