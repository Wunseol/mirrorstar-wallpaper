<p align="center">
  <img src="src-tauri/icons/icon.png" width="120" height="120" alt="MirrorStar Wallpaper 图标" />
</p>

<h1 align="center">MirrorStar Wallpaper（镜星壁纸）</h1>

<p align="center"><em>基于 Rust + Tauri v2 的轻量高效 Windows 动态壁纸软件。</em></p>

MirrorStar Wallpaper 基于 Rust + Tauri v2 打造，通过 WorkerW 原生嵌入把视频、GIF 与静态图片壁纸渲染到桌面图标层下方，在保持桌面正常交互的同时实现低资源占用与流畅的动态壁纸体验。

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-GPLv2%2B-orange.svg" alt="License: GPL-2.0-or-later" /></a>
  <img src="https://img.shields.io/badge/Platform-Windows-blue" alt="Platform: Windows" />
  <img src="https://img.shields.io/badge/Rust-1.80-blue" alt="Rust" />
  <img src="https://img.shields.io/badge/Tauri-v2-blue" alt="Tauri" />
</p>

<!-- CI status badge: 仓库公开后替换为真实 workflow URL -->
<!-- Release badge: 仓库公开后替换为真实 release URL -->

## 简介

镜星壁纸是一款专注于 **Windows 桌面** 的高性能动态壁纸方案。它尊重系统原生行为，通过事件驱动而非轮询的方式感知全屏与应用状态，配合子进程隔离架构，在沉浸感与稳定性之间取得平衡。

- **极低资源占用**：事件驱动架构零轮询，静态壁纸直接调用原生壁纸 API，非视频类型几乎不占用额外资源。
- **事件驱动零轮询**：全屏切换、电池状态等通过系统事件（`SetWinEventHook`）感知，无后台定时扫描。
- **子进程隔离**：视频壁纸、以及按需启动的主窗口 UI 均运行在独立子进程，单个子进程崩溃不影响壳与其它壁纸。
- **多显示器支持**：每屏独立设置壁纸，支持三种多屏排列（每屏独立 / 每屏同图 / 跨屏合并）。
- **三种壁纸类型**：视频 / GIF / 静态图片，覆盖主流动态壁纸需求。

## 特性

- **三种壁纸类型**
  - 视频：经 `mpv.exe` 外部子进程渲染
  - GIF：`image` crate 逐帧解码
  - 静态图片：支持原生壁纸 API（`SystemParametersInfoW`）
- **WorkerW 原生嵌入**：壁纸渲染在桌面图标层下方，不影响桌面交互与右键菜单
- **多显示器支持**：每屏独立设置壁纸，支持三种多屏排列
  - `per_monitor`：每屏独立
  - `all_same`：每屏同图
  - `span`：跨屏合并
- **事件驱动自动暂停**：全屏应用时自动暂停（事件驱动，非轮询）；电池供电时可暂停
- **播放控制**：暂停 / 恢复 / 音量 / 静音 / 播放速度
- **子进程隔离**：视频壁纸运行在独立子进程，单个壁纸崩溃不影响壳与其它壁纸
- **壁纸轮换**：定时轮换 + 可组合候选池 + 采样算法（顺序循环 / 洗牌袋（随机不重复）/ 纯随机（随机可重复））
- **纯净壁纸模式**：托盘一键进入，壁纸置顶全屏覆盖图标 / 任务栏，任意键 / 鼠标点击退出（模式内暂停轮换与自动暂停）
- **托盘驻留 + 主窗口按需启动**：应用默认驻留系统托盘；主窗口为独立原生 UI 子进程 `mirrorstar-ui.exe`，按需拉起、隐藏超时（默认 300s）后自动退出回收内存，降低驻留资源占用
- **其他**：开机自启、缩略图管理、鼠标交互模式

## 技术栈

下表版本号以根 `Cargo.toml` / `src-tauri/tauri.conf.json` 为准。

| 层 | 技术 | 说明 |
|---|---|---|
| 应用壳 | Tauri v2.11（启用 `tray-icon`） | 仅作壳：托盘 / 壁纸引擎 / 轮换调度 / 命令桥 / UI 子进程管理 |
| 主窗口 UI | Slint 1.18（软件渲染） | 原生声明式 UI，独立子进程 `mirrorstar-ui.exe`，无 Web 运行时 |
| Windows API | windows-rs 0.58 | Win32 绑定（桌面嵌入、事件钩子、进程 / 作业管理等） |
| 视频播放 | mpv.exe | 外部子进程，捆绑于 `<exe_dir>/mpv/` 或回退 PATH |
| 配置 | TOML + serde | 配置读写与热重载（`notify`） |
| 日志 | tracing 系列 | 结构化日志（`tracing` + subscriber + appender） |
| 异步运行时 | tokio 1.52 | `rt` / `rt-multi-thread` / `macros` / `time` / `fs` / `sync` / `io-util` |
| IPC | 双协议 | 壳↔UI：`mirrorstar-core::ui_proto` NDJSON over stdio；壳↔渲染子进程（mpv 等）：Windows 命名管道 |

> 主窗口已由 Tauri 内嵌 Web 视图改为**独立原生 UI 子进程**（Slint）；壳不再创建任何 WebView 主窗口（`app.windows = []`），仅保留托盘、壁纸引擎、轮换调度、命令桥与 UI 子进程管理。原前端构建产物及其工具链已随重构删除，仓库不含任何前端构建链。

## 项目结构

Rust workspace 四个成员：`src-tauri` / `crates/mirrorstar-core` / `crates/mirrorstar-wp-proc` / `crates/mirrorstar-ui`。

```
mirrorstar-wallpaper/
├── .github/                  # CI/CD 工作流与依赖升级配置
├── crates/
│   ├── mirrorstar-core/      # 共享核心库（配置 / 桌面 / 壁纸 / 音频 / IPC / 进程 / 调度 / 布局 / UI 协议）
│   ├── mirrorstar-ui/        # 原生主窗口 UI 子进程（Slint，独立二进制 mirrorstar-ui.exe）
│   └── mirrorstar-wp-proc/   # 隔离渲染辅助逻辑库（由壳 --wp-proc 模式复用，无独立 exe）
├── docs/                     # 项目文档（入口：docs/index.md）
├── mpv/                      # 运行时视频播放器（mpv.exe 被忽略、license 入库）
├── src-tauri/                # Tauri 壳（Rust）：托盘 / 壁纸引擎 / 调度 / 命令桥 / UI 子进程管理
├── wallpaper/                # 本地示例壁纸（mp4 / jpg / png）
├── Cargo.toml                # Rust workspace 清单
├── Cargo.lock                # Rust 依赖锁定（已提交入库）
├── LICENSE                   # 开源许可证（GPL-2.0-or-later）
├── README.md                 # 仓库说明
├── rust-toolchain.toml       # 固定 Rust 工具链（stable）
└── rustfmt.toml              # rustfmt 配置
```

> 渲染子进程 wp-proc 已改为「同一 exe 双入口」——即壳 `mirrorstar-wallpaper.exe` 的 `--wp-proc` 模式，**不再有独立的 wp-proc exe**。

## 快速开始

### 最终用户

从 **Release** 页面下载 NSIS 安装包（`productName` 为 `MirrorStar Wallpaper`，identifier `com.mirrorstar.wallpaper`），双击安装即可使用，无需自行构建。

### 开发者

#### 环境要求

- Windows 10 / 11
- Rust 1.80+（`rust-toolchain.toml` 锁定 stable 通道）
- MSVC Build Tools（Visual Studio 2022，"Desktop development with C++" 工作负载，含 Windows SDK）
- Tauri CLI 2.11（`cargo install tauri-cli --version "^2.11" --locked`）

> **不再需要 Node.js / npm**：项目已无前端构建产物，CI 亦不安装 Node；主窗口为原生 Slint UI 子进程。

#### 构建与运行

```bash
# 克隆仓库（将 <repo-url> 替换为实际仓库地址）
git clone <repo-url>
cd mirrorstar-wallpaper

# 开发模式：src-tauri/build.rs 会以隔离的 CARGO_TARGET_DIR
# 单独执行 `cargo build -p mirrorstar-ui --release`，并把产物复制到
# target/<PROFILE>/ 与 target/release/（wp-proc 已并入壳，无需单独构建）
cargo tauri dev
```

#### mpv 运行时获取

视频壁纸依赖 `mpv.exe`（体积大，约 100+ MB，已被 `.gitignore` 忽略、不入库）。请按官方发布获取 Windows x64 构建（项目 CI 固定 release `20260903` 并以 SHA256 校验下载），将解压得到的 `mpv.exe` 放入仓库根 `mpv/` 目录；`tauri_build` 在编译期会校验该文件存在。`mpv/license` 为随源码入库的许可文本，无需另行获取。

#### 常用命令

| 用途 | 命令 |
|---|---|
| 编译全部成员 | `cargo build --workspace` |
| 应用开发模式 | `cargo tauri dev` |
| 打包（NSIS 安装包） | `cargo tauri build` |
| Rust 测试 | `cargo test --workspace` |
| Lint（零警告） | `cargo clippy --workspace --all-targets -- -D warnings` |
| 格式检查 | `cargo fmt --all --check` |
| 依赖安全审计 | `cargo audit` |

#### 构建产物维护

`target/`（Cargo 构建产物）已在 `.gitignore` 中忽略，不纳入版本控制。频繁构建 / 测试会使 `target/` 持续累积，建议在超过 5 GB 时执行：

```bash
cargo clean   # 删除整个 target/，下次构建自动复现
```

> 仅 `target/` 可安全删除；`mpv/mpv.exe`（视频壁纸运行时播放器，被忽略、需按上节单独获取）需保留，`mpv/license` 已入库。

### 数据存放位置（便携版）

全部用户数据（配置、壁纸库、壁纸资源、缩略图、日志）统一存储在数据根目录，由 `resolve_data_root()` 按三级优先级解析：环境变量 `MIRRORSTAR_DATA_ROOT` → exe 所在目录（始终采用）→ `%APPDATA%\mirrorstar`（仅作兜底）。安装版数据落在安装目录（即 exe 所在目录），删除整个安装目录即可一键全量清理；开发时经 `cargo run` / `cargo tauri dev` 运行的程序（exe 位于 `target\<profile>` 下）数据随之落在 `target\<debug|release>\`，会随 `cargo clean` 一并清除——这一行为是**刻意设计**（dev 数据视为可再生的临时产物），并非便携性缺陷；详细优先级见 [docs/04-优化文档/02-config模块.md](./docs/04-优化文档/02-config模块.md) 的「1.4 数据根目录」。

## 使用说明

- **添加壁纸**：将视频 / GIF / 图片资源加入轮换池，即可作为动态壁纸。
- **切换类型**：视频 / GIF / 图片三种类型可按需自由切换。
- **全屏自动暂停**：进入全屏应用的窗口时，壁纸会依据事件自动暂停播放，退出全屏后自动恢复；电池供电时也可选择暂停以避免额外耗电。
- **播放控制**：支持暂停 / 恢复 / 音量 / 静音 / 播放速度等控制。
- **壁纸轮换**：可配置多个候选池与采样算法（顺序循环 / 洗牌袋 / 纯随机），并设定轮换间隔。
- **托盘驻留**：应用默认驻留系统托盘；主窗口由独立 UI 子进程按需启动，窗口隐藏超时（默认 300s）后自动退出回收内存，再次打开时可重新拉起，负载更轻。

## 文档入口

详细文档请参阅 [docs/index.md](./docs/index.md)。

`docs/` 目录按以下分类组织，覆盖从需求到合规的完整设计资料：

- **01-需求文档**：项目概述、功能性 / 非功能性需求、用例、约束与术语
- **02-架构与技术栈**：架构与技术栈总览、模块设计、进程架构与 IPC、桌面集成与 Windows API、壁纸渲染、暂停恢复、错误处理、依赖与数据流、性能优化、壁纸轮换调度器
- **03-实施与验证**：实施与验证总览、开发环境与构建、项目目录结构、质量保障、测试报告（历史）
- **04-优化文档**：工程审计与优化演进记录（归档性，供维护者追溯）

## 项目状态

- **当前版本**：0.1.0（开发中）
- **目标平台**：Windows 10 / 11
- **项目状态**：积极开发中，API 与功能可能调整

## 贡献指南

欢迎参与贡献。在提交变更前，请：

1. 阅读 [docs/index.md](./docs/index.md) 及相关目录文档，理解架构与规范。
2. 确保以下门槛全部通过（与 CI 质量门一致），零警告：
   - `cargo test --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo fmt --all --check`
   - `cargo audit`

## 许可证

项目采用 **GPL-2.0-or-later**（GNU GPL v2 或其后版本，SPDX 标识符 `GPL-2.0-or-later`），完整协议文本见 [LICENSE](./LICENSE)。

第三方关键组件：

- **Slint 1.18（GPL-3.0 或商业双许可）**：主窗口 UI 框架。项目自身为 GPLv2+（可选择升级到 GPLv3），与 Slint 的 GPLv3 分支兼容。
- **mpv.exe（GPL-2.0-or-later）**：视频播放运行时，与项目采用相同许可证，捆绑分发无兼容性问题。