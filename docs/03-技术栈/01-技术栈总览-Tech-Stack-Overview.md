[← 返回文档索引](../index.md) > 技术栈 > 技术栈总览

# MirrorStar Wallpaper（镜星壁纸）技术栈 — 总览

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 技术栈总览

下表基于当前实际代码审计得出，版本号以 workspace `Cargo.toml` 与 `package.json` 为准。

| 类别 | 技术 | 版本 | 用途 | 许可证 |
|------|------|------|------|--------|
| 核心语言 | Rust | 1.80（`rust-version`） | 应用主体开发（workspace edition 2021） | MIT / Apache-2.0 |
| UI 框架 | Tauri | v2.11（启用 `tray-icon` feature） | 桌面应用框架（Rust 后端 + WebView2 前端） | MIT / Apache-2.0 |
| 前端 | 原生 TypeScript + HTML + CSS | TS 5.5 / Vite 5.4 | 用户界面渲染（未使用 React/Vue/Angular） | MIT |
| Windows API | windows-rs | 0.58 | Windows 系统 API 绑定 | MIT / Apache-2.0 |
| 视频播放 | mpv.exe（外部子进程） | 外部进程（无 Rust crate） | 通过 mpv.exe 播放视频，捆绑于 `<exe_dir>/mpv/` 或 PATH | GPL-2.0 (mpv) |
| 图片 / GIF | image crate | 0.25（`default-features=false`，启用 png/jpeg/gif/bmp/webp/tiff + rayon） | 静态图片与 GIF 解码（WebP 回退路径 + GIF 渲染） | MIT |
| 网页壁纸 | webview2-com | 0.34 | WebView2 SDK Rust 绑定（由子进程 mirrorstar-wp-proc.exe 使用） | MIT |
| 配置管理 | serde + toml | serde 1 / toml 0.8 | TOML 配置读写 | MIT |
| 热重载 | notify | 7 | 配置文件系统监控 | CC0-1.0 / MIT |
| 日志系统 | tracing + tracing-subscriber + tracing-appender | tracing 0.1 / subscriber 0.3 / appender 0.2 | 结构化日志 | MIT |
| 系统托盘 | Tauri 2 内置 `tray-icon` feature | v2.11 | 系统托盘图标与菜单（不作为独立 crate 依赖） | MIT / Apache-2.0 |
| 进程间通信 | Windows 命名管道（CreateNamedPipeW / ConnectNamedPipe） | - | 主进程与子进程双向 IPC（同步 Win32 API，两套独立协议） | - |
| 异步运行时 | tokio | 1.52（`rt` / `rt-multi-thread` / `macros` / `time` / `fs` / `sync` / `io-util`） | 多线程异步运行时 | MIT |
| 序列化 | serde + serde_json + toml | serde 1 / serde_json 1 / toml 0.8 | 数据序列化 / 反序列化 | MIT |
| 错误处理 | thiserror | 2 | 库级错误类型定义（无 anyhow） | MIT / Apache-2.0 |
| 文件锁 | fs2 | 0.4 | 配置文件写入原子保护 | MIT / Apache-2.0 |
| 命令行解析 | clap | 4（`derive`） | 子进程命令行参数解析 | MIT / Apache-2.0 |
| 系统目录 | dirs | 6 | 获取系统目录路径 | MIT / Apache-2.0 |
| 注册表操作 | winreg | 0.55 | 开机自启与壁纸缩放模式注册表读写 | MIT |
| UUID | uuid | 1（`v4`） | 命名管道、实例等唯一标识 | MIT / Apache-2.0 |
| URL 工具 | percent-encoding | 2 | `wpfile://` 自定义协议路径解码 | MIT / Apache-2.0 |
| 文件对话框 | tauri-plugin-dialog | 2.7 | Tauri 文件选择对话框插件 | MIT / Apache-2.0 |
| 开机自启 | tauri-plugin-autostart | 2.5 | Tauri 开机自启插件 | MIT / Apache-2.0 |
| 构建 / 测试工具 | Tauri CLI / Vite / Vitest / ESLint / Prettier / Terser | @tauri-apps/cli 2.11.2 / vite 5.4 / vitest 2.1.9 / eslint 9 / prettier 3.4 / terser 5.37 | 前端构建、测试、代码规范 | MIT |

> 说明：早期文档中列出的 `anyhow`、`bitflags`、`softbuffer`、`raw-window-handle` 均为虚构依赖，实际代码未引入，已从本表移除。

***

## 2. 核心语言与运行时

### 选择：Rust 1.80+

**选择理由：**

- **零成本抽象**：泛型、trait、迭代器等高级特性在编译时内联，适合对性能敏感的壁纸渲染
- **内存安全**：所有权系统在编译期保证内存安全，无需运行时垃圾回收
- **无 GC 暂停**：Rust 无 GC，帧率稳定，避免画面卡顿
- **无运行时依赖**：用户无需安装 .NET、Java、Python 等运行时，开箱即用
- **现代工具链**：Cargo 包管理器、rustfmt、clippy 静态分析

### 与 C# (.NET Framework 4.7.2) 对比

Lively Wallpaper 使用 C# + .NET Framework 4.7.2，关键差异：

| 维度 | C# (.NET Framework 4.7.2) | Rust (stable) |
|------|--------------------------|---------------|
| 运行时依赖 | 需要 .NET Framework（~60MB） | 无（静态链接） |
| 二进制体积 | ~50MB+（含依赖） | 相对更小 |
| 内存占用 | ~80-150MB（含 CLR 开销） | 显著更低 |
| GC 暂停 | 有，可能影响画面 | 无 |
| 启动速度 | 需 CLR 初始化，较慢 | 原生启动，极快 |

### 编译目标

```
x86_64-pc-windows-msvc
```

使用 MSVC 工具链，确保与 Windows SDK 和 WebView2 的最佳兼容性。Release 构建启用 `lto = true`、`codegen-units = 1`、`strip = "symbols"`、`panic = "abort"` 以减小体积。

***

**相关章节：** [UI 框架](./02-UI框架-UI-Framework.md) | [Windows 系统 API](./03-Windows系统API-Windows-System-API.md) | [壁纸渲染](./04-壁纸渲染-Wallpaper-Rendering.md) | [基础设施](./05-基础设施-Infrastructure.md) | [风险评估](./06-风险评估-Risk-Assessment.md)