# MirrorStar Wallpaper（镜星壁纸）技术栈 — 总览

[← 返回文档索引](../README.md) > [技术栈](./overview.md)

> 版本：1.0
> 最后更新：2026-06-10
> 状态：已实现

---

## 1. 技术栈总览

| 类别 | 技术 | 版本 | 用途 | 许可证 |
|------|------|------|------|--------|
| 核心语言 | Rust | 1.80+ | 应用主体开发 | MIT / Apache-2.0 |
| UI 框架 | Tauri | v2 | 桌面应用框架（Rust 后端 + WebView2 前端） | MIT / Apache-2.0 |
| 前端 | HTML + CSS + TypeScript | - | 用户界面渲染 | - |
| Windows API | windows-rs | 0.58+ | Windows 系统 API 绑定 | MIT / Apache-2.0 |
| 视频播放 | mpv.exe（外部进程） | - | 通过外部 mpv.exe 子进程播放视频（无 Rust crate 依赖，mpv.exe 须在 PATH 中或捆绑于 `<exe_dir>/mpv/`） | GPL-2.0 (mpv) |
| GIF 播放 | image crate (GifDecoder) + GDI 双缓冲 | latest | GIF 解码与 GDI 双缓冲渲染 | MIT |
| 网页壁纸 | WebView2 | - | 网页内容渲染（由子进程 mirrorstar-wp-proc.exe 创建） | BSD (Chromium) |
| 配置管理 | serde + toml | latest | TOML 配置读写 | MIT / MIT |
| 热重载 | notify | latest | 文件系统监控 | CC0-1.0 / MIT |
| 日志系统 | tracing + tracing-subscriber + tracing-appender | latest | 结构化日志 | MIT |
| 系统托盘 | tray-icon | latest | 系统托盘图标与菜单（通过 Tauri 内置 API 使用，不作为独立 crate 依赖） | MIT |
| 进程间通信 | Windows 命名管道（CreateNamedPipeW/ConnectNamedPipe） | - | 主进程与子进程双向 IPC（同步 Win32 API，两套独立协议：mpv 原生 IPC 与 wp-proc 自定义 IPC） | - |
| 异步运行时 | tokio | latest | 多线程异步运行时 | MIT |
| 序列化 | serde + serde_json + toml | latest | 数据序列化/反序列化 | MIT |
| 错误处理 | thiserror + anyhow | latest | 库级与应用级错误处理 | MIT / MIT |
| 图片解码 | image | latest | 静态图片与 GIF 壁纸解码（JPG/PNG/BMP/WebP/GIF） | MIT |
| WebView2 绑定 | webview2-com | 0.34 | WebView2 SDK Rust 绑定（子进程网页壁纸） | MIT |
| 文件锁 | fs2 | latest | 配置文件原子写入保护 | MIT / Apache-2.0 |
| 位掩码 | bitflags | 2 | 位标志类型定义（声明但未使用） | MIT / Apache-2.0 |
| 命令行解析 | clap | 4 | 子进程命令行参数解析 | MIT / Apache-2.0 |
| 系统目录 | dirs | 6 | 获取 %APPDATA% 等系统目录路径 | MIT / Apache-2.0 |
| 注册表操作 | winreg | 0.52+ | 开机自启注册表读写 | MIT |
| 窗口句柄 | raw-window-handle | 0.6+ | 跨 crate 窗口句柄传递（声明但未使用） | MIT / Apache-2.0 |
| 软件渲染 | softbuffer | 0.4+ | CPU 软件渲染（声明但未使用，实际使用 GDI 双缓冲） | MIT / Apache-2.0 |
| 文件对话框 | tauri-plugin-dialog | 2.0+ | Tauri 文件选择对话框插件 | MIT / Apache-2.0 |
| 开机自启 | tauri-plugin-autostart | 2.0+ | Tauri 开机自启插件（备选方案，架构设计采用 winreg 直接操作注册表） | MIT / Apache-2.0 |

---

## 2. 核心语言与运行时

### 选择：Rust 1.80+

**选择理由：**

- **零成本抽象**：Rust 的泛型、trait、迭代器等高级特性在编译时完全内联，运行时无额外开销，适合对性能敏感的桌面壁纸应用
- **内存安全**：所有权系统在编译期保证内存安全，杜绝空指针、悬垂指针、数据竞争等常见 Bug，无需运行时垃圾回收
- **无 GC 暂停**：垃圾回收（GC）会导致不可预测的停顿，在壁纸应用中表现为画面卡顿。Rust 无 GC，帧率稳定
- **小二进制体积**：静态链接后单文件分发，无需运行时依赖，典型 Release 构建体积 < 10MB
- **无运行时依赖**：用户无需安装 .NET、Java、Python 等运行时，开箱即用
- **现代工具链**：Cargo 包管理器、rustfmt 格式化、clippy 静态分析、完善的文档生态

### 与 C# (.NET Framework 4.7.2) 对比

Lively Wallpaper 使用 C# + .NET Framework 4.7.2，以下是关键差异：

| 维度 | C# (.NET Framework 4.7.2) | Rust (stable) |
|------|--------------------------|---------------|
| 运行时依赖 | 需要 .NET Framework 4.7.2（~60MB） | 无（静态链接） |
| 二进制体积 | ~50MB+（含依赖） | <10MB |
| 内存占用 | ~80-150MB（含 CLR 开销） | <20MB（暂停时） |
| GC 暂停 | 有，可能导致画面卡顿 | 无 |
| 启动速度 | 需 CLR 初始化，较慢 | 原生启动，极快 |
| 分发方式 | 需安装 .NET 或捆绑运行时 | 单文件分发 |

### 编译目标

```
x86_64-pc-windows-msvc
```

使用 MSVC 工具链，确保与 Windows SDK 和 WebView2 的最佳兼容性。不使用 GNU 工具链以避免 MinGW 依赖。

---

**相关章节：** [UI 框架](./ui-framework.md) | [Windows API 绑定](./windows-api.md) | [壁纸渲染](./wallpaper-rendering.md) | [基础设施](./infrastructure.md) | [风险评估](./risk-assessment.md)
