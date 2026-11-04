# MirrorStar Wallpaper（镜星壁纸）需求文档 — 项目概述 / Project Overview

[← 返回文档索引](../index.md) > [需求文档](./01-项目概述-Overview.md)

> 相关文档：[功能性需求](./02-功能性需求-Functional-Requirements.md) | [非功能性需求](./03-非功能性需求-Non-Functional-Requirements.md) | [用例](./04-用例-Use-Cases.md) | [约束与术语](./05-约束与术语-Constraints-and-Terminology.md)

| 项目 | 内容 |
|------|------|
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.2 |
| 创建日期 | 2026-06-10 |
| 更新日期 | 2026-11-04 |
| 文档状态 | 已实现（基于最新代码审计，2026-11） |

---

## 1. 项目概述

### 1.1 项目背景

Windows 平台上的动态壁纸软件已有多种实现，但现有主流方案多基于 C# WPF 开发、依赖运行时，普遍存在以下问题：

- **内存占用高**：即使壁纸暂停，主进程仍占用 50MB+ 内存
- **运行时依赖**：需要安装 .NET Runtime，增加了用户部署成本
- **功能臃肿**：包含 Unity、Godot 等小众壁纸类型，以及 YouTube 流媒体、壁纸创建器等非核心功能，导致安装包体积大、维护成本高
- **CPU 开销**：暂停状态下仍有后台活动，无法做到真正的零 CPU 占用

### 1.2 项目定义

MirrorStar Wallpaper（镜星壁纸）是一款基于 Rust 开发的轻量级、高性能 Windows 动态壁纸应用程序。

- **应用壳**：基于 Tauri v2（`tauri = 2.11`，启用 `tray-icon` 特性），仅承担系统托盘图标、原生对话框（tauri-plugin-dialog）、开机自启（tauri-plugin-autostart）与后端命令等壳层职责。
- **主窗口 UI**：采用 **Slint 1.18 原生渲染**的独立子进程（`mirrorstar-ui`，软件渲染）承载，**不再使用系统 WebView 渲染主界面**；主窗口按需 spawn，窗口隐藏超时（默认 300s）后 UI 子进程退出回收内存。壳进程与 `mirrorstar-ui` 子进程之间使用 `mirrorstar-core::ui_proto` 的 **NDJSON 行协议 over stdio** 通信，**不是命名管道**。
- **核心壁纸引擎**：纯 Rust + Win32 API 实现（无 GUI 框架依赖）。
- **网页壁纸渲染**：使用 WebView2（webview2-com 0.34），由壳 exe 以 `--wp-proc` 模式启动独立子进程承载，**仅在渲染网页壁纸时存在**。
- **桌面嵌入（WorkerW 模式）**：Windows 11 raised desktop 下实际嵌入为 Progman 的 layered 子窗口（宿主取 `SHELLDLL_DefView` 的直接父窗口，以 `SHELLDLL_DefView` 为 z-order 锚点，确保壁纸位于桌面图标层之下）；旧层级，或 raised 但 `SHELLDLL_DefView` 缺失时，回退 `SetParent(WorkerW)`。

项目保留动态壁纸的核心功能，去除非必要特性，利用 Rust 语言的内存安全和零成本抽象特性，实现极致的轻量化与高性能。

### 1.3 目标用户

- Windows 桌面用户，希望使用动态壁纸但不希望牺牲系统性能
- 低配电脑用户，对软件资源占用敏感
- 开发者/技术用户，偏好原生应用而非依赖运行时的应用

---

## 2. 项目愿景

**成为 Windows 平台上最轻量、最高效的动态壁纸软件。**

具体而言：

- **极致轻量**：以最小的资源开销提供完整的动态壁纸体验
- **原生体验**：无运行时依赖，单文件分发，即开即用
- **稳定可靠**：利用 Rust 的类型系统和所有权模型，杜绝内存安全问题和数据竞争
- **简洁克制**：只做壁纸该做的事，不做功能堆砌

---

## 3. 核心目标

| 目标 | 指标 | 说明 | 实测 |
|------|------|------|------|
| 轻量 | 主进程暂停态 < 20MB 内存 | 远低于常见方案的 50MB+ 水平 | ✅ 历史实测约 17.14MB（2026-07-24，未覆盖大 GIF 暂停态），待最新复测 |
| 高性能 | 暂停态 CPU 占用 < 0.1% | 无后台轮询，事件驱动架构 | ✅ 历史实测 0%（事件驱动快速通道），待最新复测 |
| 高性能 | 视频壁纸播放 CPU < 5% | 主进程仅负责 IPC 与窗口管理，渲染下沉至 mpv 子进程 | 历史测量 mpv 子进程约 18~24% 单核，主进程 0%；待复测 |
| 原生安全 | Rust 编写，无 GC | 零成本抽象，无运行时开销 | ✅ 纯 Rust，无 GC |
| 小体积 | 二进制 < 10MB | 无 .NET 依赖，单文件部署 | 待复测（需 Release 编译体积） |
| 快启动 | 冷启动 < 2 秒 | 无运行时初始化延迟 | 历史 < 1 秒（Debug 弱环境约 2s），待复测 |
| 快速切换 | 壁纸切换时间 < 1 秒 | 平滑切换不闪烁 | ⚠️ 历史约 3s（子进程启动 + 窗口嵌入开销），待复测 |

**说明**：表中“实测”列除特别标注外均为**历史数据**（含 2026-07-24 运行时测试），尚未在当前最新代码上复测，实际表现以实测为准。壁纸切换历史值（~3s）超出 1s 目标，主要瓶颈在子进程启动与窗口嵌入开销，已在架构优化路线图中列入重点改进项。暂停态内存、暂停态 CPU、冷启动三项历史数据来源为《[架构与技术栈总览](../02-架构与技术栈/01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)》性能指标表与《[测试报告](../03-实施与验证/05-测试报告-Test-Reports.md)》。

---

## 4. 项目状态

- **当前版本**：0.1.4（开发中）
- **目标平台**：Windows 10 (1809+) / Windows 11，x86_64
- **项目状态**：已实现（基于最新代码审计，2026-11）；持续优化中，API 与功能可能调整
- **工程元数据**：edition 2021，rust-version 1.80，license GPL-2.0-or-later
- **Rust workspace**：四个成员
  - `src-tauri`：应用主进程/壳（托盘、对话框、开机自启、后端命令）
  - `crates/mirrorstar-core`：核心库（12 个 `pub mod`：audio / config / desktop / ipc / layout / log_retention / perf / process / pure_mode / scheduler / ui_proto / wallpaper）
  - `crates/mirrorstar-wp-proc`：网页壁纸逻辑的**库 crate**，经壳 exe 的 `--wp-proc` 模式作为子进程运行
  - `crates/mirrorstar-ui`：Slint 主窗口独立子进程（软件渲染）
- **数据文件**（数据根/安装目录下）：`config.toml`、`wallpapers.toml`、`playback.toml`、`wallpapers/{id}/`、`thumbnails/`、`logs/`、`webview2-cache/`（含 `<pid>` 与 `ui-<pid>`）
- **壁纸渲染双模式**：
  - **Native**：jpg/jpeg/png/bmp/tif/tiff/dib 静态图片，零资源占用、不嵌 WorkerW
  - **WorkerW**：其余类型（GIF/视频/网页/WebP/ICO），嵌入桌面 WorkerW（或 Win11 raised desktop 的 Progman layered 层）渲染
  - **格式事实**：添加入口识别格式为 `jpg/jpeg/png/bmp/webp/tif/tiff/ico`；原生 API 支持 `jpg/jpeg/png/bmp/tif/tiff/dib`；`.dib` 不在添加识别列表，当前无法通过添加入口加入；WebP/ICO 走 WorkerW 渲染
- **已实现功能**：纯净壁纸模式；多屏排列（PerMonitor / AllSame / Span）；拖拽排序；预览播放器化控制（播放/暂停/进度/速度/音量/静音/全屏）；定时轮换与开机推进；壁纸池 CRUD 与成员排序；单元级激活池/开关；上一张/下一张（主窗口与托盘）；鼠标交互模式；状态栏（桌面/配置/轮换/播放摘要）；日志留存