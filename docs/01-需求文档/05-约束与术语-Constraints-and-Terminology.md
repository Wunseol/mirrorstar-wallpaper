# MirrorStar Wallpaper（镜星壁纸）需求文档 — 约束与术语 / Constraints & Terminology

[← 返回文档索引](../index.md) > [需求文档](./01-项目概述-Overview.md) > 约束与术语

> 相关文档：[项目概述](./01-项目概述-Overview.md) | [功能性需求](./02-功能性需求-Functional-Requirements.md) | [非功能性需求](./03-非功能性需求-Non-Functional-Requirements.md) | [用例](./04-用例-Use-Cases.md)

## 9. 约束与假设

### 9.1 约束

| 编号 | 约束 | 说明 |
|------|------|------|
| CON-001 | 操作系统限制 | 仅支持 Windows 10 (1809+) 和 Windows 11 |
| CON-002 | 架构限制 | 仅支持 x86_64 架构 |
| CON-003 | WebView2 依赖 | 网页壁纸功能依赖 Microsoft Edge WebView2 Runtime，Windows 11 已内置，Windows 10 (2004+) 已预装；Windows 10 (1809~2003) 需用户安装或程序引导安装 WebView2 Bootstrapper |
| CON-004 | mpv 依赖 | 视频壁纸功能依赖外部 mpv.exe 可执行文件，需在 PATH 中或捆绑在应用目录的 mpv/ 子目录下。find_mpv() 支持捆绑路径优先查找 + PATH 回退 |
| CON-005 | 桌面窗口管理器 | 依赖 Windows Desktop Window Manager (DWM)，不支持禁用 DWM 的环境 |
| CON-006 | Rust 工具链 | 开发需要 Rust stable 工具链（1.80+，edition 2021）；rust-toolchain.toml 锁定版本 |
| CON-007 | 无跨平台计划 | 面向 Windows，不考虑 macOS/Linux 移植 |
| CON-008 | 资源加载协议 | 壁纸图片/GIF/视频/网页资源经自定义 `wpfile://` 协议（http://wpfile.localhost）加载到 WebView，handler 内实现路径 scope 校验（allow：`$APPDATA/mirrorstar/**`、`$LOCALAPPDATA/mirrorstar/**`；deny 优先：`$HOME` 下 7 个敏感目录），越权返回 403。CSP 的 img-src/media-src/frame-src 仅允许 `wpfile:` 与 `https://wpfile.localhost` |

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
| 原生壁纸模式 | Native Mode | 通过 Windows 原生 API（`SystemParametersInfoW` + 注册表）设置静态壁纸，零资源占用、无渲染器、不嵌入 WorkerW。仅原生 API 支持的格式（jpg/jpeg/png/bmp/tif/tiff/dib）使用 |
| WorkerW | WorkerW | Windows 桌面窗口层次结构中的一个层，位于桌面图标（SHELLDLL_DefView）下方，动态壁纸通过将窗口嵌入此层实现在图标下方显示 |
| Progman | Program Manager | Windows 桌面窗口层次结构的顶层窗口，是桌面背景的容器 |
| DWM | Desktop Window Manager | Windows 桌面窗口管理器，负责窗口合成和视觉效果 |
| mpv | mpv | 开源的跨平台媒体播放器，MirrorStar 通过启动外部 mpv.exe 子进程作为视频壁纸的后端，通过命名管道 `\\.\pipe\mirrorstar-mpv-*` 接收 mpv 原生 JSON IPC 命令 |
| mirrorstar-wp-proc | mirrorstar-wp-proc | 网页壁纸子进程，承载 WebView2 渲染网页壁纸；主进程通过命名管道 `\\.\pipe\mirrorstar-wp-*`（高熵 UUID，难以猜中）与 wp-proc 通信，实现崩溃隔离和内存优化 |
| WebView2 | Microsoft Edge WebView2 | Microsoft 提供的 WebView 控件，基于 Chromium 内核，用于在应用中嵌入网页内容 |
| SHELLDLL_DefView | Shell Default View | Windows 桌面窗口层次结构中管理桌面图标显示的窗口 |
| 硬件加速解码 | Hardware-accelerated decoding | 利用 GPU 进行视频解码（mpv `--hwdec=auto`），降低 CPU 占用 |
| 事件驱动 | Event-driven | 壁纸状态变更（全屏、电源、Explorer 重启）通过 Windows 消息 / WinEvent 回调触发，而非轮询；实现暂停态零 CPU 占用 |
| PauseSender | PauseSender | 暂停/恢复快速通道：绕过引擎 Mutex，直接向渲染线程发送暂停命令，实现 < 10ms 的快速响应 |
| 工作集内存 | Working Set Memory | 进程当前驻留在物理内存中的页面集合，反映实际物理内存占用 |
| 壁纸库 | Wallpaper Library | 用户添加的壁纸集合，包含壁纸的元信息和缩略图 |
| 托盘图标 | System Tray Icon | Windows 任务栏通知区域（系统托盘）中的图标 |
| 全屏检测 | Fullscreen Detection | 检测系统中是否有应用程序以全屏模式运行的技术（SetWinEventHook / EVENT_SYSTEM_FOREGROUND） |
| 冷启动 | Cold Start | 程序从完全关闭状态启动的过程 |
| 热启动 | Warm Start | 程序从最小化/后台状态恢复到前台的过程，窗口从零创建（非从隐藏状态恢复） |
| 鼠标穿透 | Click-through | 壁纸窗口不拦截鼠标事件，使鼠标操作直接传递到下层窗口 |
| wpfile 协议 | wpfile protocol | 自定义 URI scheme 协议（http://wpfile.localhost），用于在 WebView 中加载壁纸本地资源，并做路径 scope 校验 |