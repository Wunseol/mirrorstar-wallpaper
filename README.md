<p align="center">
  <img src="src-tauri/icons/icon.png" width="120" height="120" alt="镜星壁纸 MirrorStar Wallpaper 图标" />
</p>

<h1 align="center">MirrorStar Wallpaper（镜星壁纸）</h1>

<p align="center"><em>基于 Rust + Tauri v2 的轻量高效 Windows 动态壁纸软件。</em></p>

基于 Rust + Tauri v2 打造，通过 WorkerW 原生嵌入将视频、GIF、网页与静态图片壁纸渲染到桌面图标层下方，在保持桌面正常交互的同时实现低资源占用与流畅的动态壁纸体验。

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-GPLv2%2B-orange.svg" alt="License: GPL-2.0-or-later" /></a>
  <img src="https://img.shields.io/badge/Platform-Windows-blue" alt="Platform: Windows" />
  <img src="https://img.shields.io/badge/Rust-1.80-blue" alt="Rust" />
  <img src="https://img.shields.io/badge/Tauri-v2-blue" alt="Tauri" />
</p>

<!-- CI status badge: replace URL once repo is public -->
<!-- Release badge: replace URL once repo is public -->

## 简介

镜星壁纸是一款专注于 **Windows 桌面** 的高性能动态壁纸方案。它尊重系统原生行为，通过事件驱动而非轮询的方式感知全屏与应用状态，配合子进程隔离架构，在沉浸感与稳定性之间取得平衡。

- **极低资源占用**：事件驱动架构零轮询，静态壁纸直接调用原生壁纸 API，非视频/网页类型几乎不占用额外资源。
- **事件驱动零轮询**：全屏切换、电池状态等通过系统事件感知，无后台定时扫描。
- **子进程隔离**：视频与网页壁纸运行在独立子进程，单个子进程崩溃不影响主程序与其他壁纸。
- **多显示器支持**：每屏独立设置壁纸，支持多屏排列（每屏独立 / 每屏同图 / 跨屏合并）。
- **四种壁纸类型**：视频 / GIF / 网页 / 静态图片，覆盖主流动态壁纸需求。

## 特性

- **四种壁纸类型**
  - 视频：经 `mpv.exe` 外部子进程渲染
  - GIF
  - 网页：基于 WebView2（WebView2 Runtime）
  - 静态图片：支持原生壁纸 API
- **WorkerW 原生嵌入**：壁纸渲染在桌面图标层下方，不影响桌面交互与右键菜单
- **多显示器支持**：每屏独立设置壁纸，支持多屏排列
  - `per_monitor`：每屏独立
  - `all_same`：每屏同图
  - `span`：跨屏合并
- **事件驱动自动暂停**：全屏应用时自动暂停（事件驱动，非轮询）；电池供电时可暂停
- **播放控制**：暂停 / 恢复 / 音量 / 静音 / 播放速度
- **子进程隔离**：视频与网页壁纸运行在独立子进程，单个壁纸崩溃不影响主程序与其他壁纸
- **壁纸轮换**：定时轮换 + 采样算法（顺序循环 / 洗牌袋 / 纯随机）+ 轮换池
- **纯净壁纸模式**：托盘一键进入，壁纸置顶全屏覆盖图标 / 任务栏，任意键/鼠标点击退出（Win+L 会退出模式而非锁屏；模式内暂停轮换与自动暂停）
- **其他**：开机自启、缩略图管理、鼠标交互模式、托盘驻留（主窗口由独立 UI 子进程按需启动，空闲超时自动退出，降低驻留资源占用）

## 技术栈

| 层 | 技术 |
|---|---|
| UI | Slint 1.18（原生窗口，绑定软件渲染，独立子进程 `mirrorstar-ui.exe`） |
| 应用层 | Tauri v2.11 + Rust 1.80（edition 2021，仅作壳：托盘 / 壁纸引擎 / 调度 / UI 子进程管理） |
| 核心 | `mirrorstar-core`（Rust workspace 成员） |
| 子进程 | `mirrorstar-ui`（主窗口 UI）、壳进程 `--wp-proc` 模式（网页壁纸隔离）、`mpv.exe`（视频播放） |
| 渲染 | windows-rs 0.58 + WebView2（仅网页壁纸/预览）+ mpv.exe |
| 配置 | TOML + serde |
| 日志 | tracing 系列 |
| IPC | 壳↔UI 子进程：`ui_proto` NDJSON over stdio；壳↔mpv/wp-proc：Windows 命名管道（自研两套协议） |

> 主窗口已由 Tauri 内嵌 WebView2 改为**独立原生 UI 子进程**（Slint）；壳不再创建任何 WebView2 主窗口，仅保留托盘、壁纸引擎、轮换调度、命令桥与 UI 子进程管理。

## 项目结构

Rust workspace 四个成员：`src-tauri` / `mirrorstar-core` / `mirrorstar-ui` / `mirrorstar-wp-proc`。

```
mirrorstar-wallpaper/
├── src-tauri/                # Tauri 壳（Rust）：托盘 / 壁纸引擎 / 调度 / 命令桥 / UI 子进程管理
├── crates/
│   ├── mirrorstar-core/      # 核心逻辑（workspace 成员）
│   ├── mirrorstar-ui/        # 原生 UI 子进程（Slint，独立二进制 mirrorstar-ui.exe）
│   └── mirrorstar-wp-proc/   # 网页壁纸逻辑库（WebView2 隔离，由壳 --wp-proc 模式复用）
├── docs/                     # 项目文档（入口：docs/index.md）
├── .github/                  # CI/CD 工作流与依赖升级配置
└── Cargo.lock                # Rust 依赖锁定（已提交入库）
```

## 快速开始

### 最终用户

从 **Release** 页面下载对应平台的安装包（当前为 NSIS 安装器），双击安装即可使用，无需自行构建。

### 开发者

#### 环境要求

- Windows 10 / 11
- Rust 1.80+
- WebView2 Runtime（Windows 11 已内置；仅网页壁纸与网页预览需要，主窗口不再依赖）

> 不再需要 Node.js / npm：项目已无前端构建产物，主窗口为原生 Slint UI。

#### 构建与运行

```bash
# 克隆仓库（将 <repo-url> 替换为实际仓库地址）
git clone <repo-url>
cd mirrorstar-wallpaper

# 开发模式（build.rs 会以隔离的 CARGO_TARGET_DIR 单独构建
# mirrorstar-ui 并复制到 target/release/；wp-proc 已作为库编入壳）
cargo tauri dev
```

#### 常用命令

| 用途 | 命令 |
|---|---|
| 应用开发模式 | `cargo tauri dev` |
| 完整应用构建 | `cargo tauri build` |
| Rust 测试 | `cargo test --workspace` |
| Rust Clippy 静态检查 | `cargo clippy --workspace` |
| 格式检查 | `cargo fmt --all --check` |
| 依赖安全审计 | `cargo audit` |

#### 构建产物维护

`target/`（cargo 构建产物）已在 `.gitignore` 中忽略，不纳入版本控制。频繁执行构建 / 测试会使 `target/` 持续累积，建议在 `target/` 超过 5 GB 时执行：

```bash
cargo clean   # 删除整个 target/，下次构建自动复现
```

> 仅 `target/` 可安全删除；`mpv/`（视频壁纸运行时播放器，在 `.gitignore` 中，按指引单独下载解压）需保留。

### 数据存放位置（便携版）

全部用户数据（配置、壁纸库、壁纸资源、缩略图、日志、WebView2 缓存）统一存储在数据根目录，由 `resolve_data_root()` 按优先级解析：环境变量 `MIRRORSTAR_DATA_ROOT`（dev 便利）→ exe 所在目录（始终采用）→ `%APPDATA%\mirrorstar`（仅作兜底）。安装版数据落在安装目录（即 exe 所在目录），删除整个安装目录即可一键全量清理；而开发时通过 `cargo run` 运行的程序（exe 位于 `target\<profile>` 下）数据随之落在 `target\<debug|release>\`，会随 `cargo clean` 一并清除——这一行为是**刻意设计**（dev 数据视为可再生的临时产物），并非便携性缺陷；详细优先级见 [`docs/05-优化文档/02-config模块.md`](./docs/05-优化文档/02-config模块.md) 的「1.4 数据根目录」。

## 使用说明

- **添加壁纸**：将视频 / GIF / 图片 / 网页资源加入轮换池，即可作为动态壁纸。
- **切换类型**：视频 / GIF / 网页 / 图片四种类型可按需自由切换。
- **全屏自动暂停**：进入全屏应用的窗口时，壁纸会依据事件自动暂停播放，退出全屏后自动恢复；电池供电时也可选择暂停以避免额外耗电。
- **播放控制**：支持暂停 / 恢复 / 音量 / 静音 / 播放速度等控制。
- **托盘驻留**：应用默认驻留系统托盘；主窗口由独立 UI 子进程按需启动，窗口隐藏超时（默认 300s）后自动退出回收内存，再次打开时可重新拉起，负载更轻。

## 文档入口

详细文档请参阅 [docs/index.md](./docs/index.md)。

`docs/` 目录按以下分类组织，覆盖从需求到合规的完整设计资料：

- **01-需求文档**：项目概述、功能性 / 非功能性需求、用例、约束
- **02-架构设计**：系统架构、模块设计、进程架构、桌面集成、暂停 / 恢复、错误处理、性能
- **03-技术栈**：UI 框架、Windows API、壁纸渲染、基础设施、风险评估
- **04-实施规划**：开发环境、阶段、项目结构、质量保障
- **05-优化文档**：模块优化、构建基础设施、实施路线图、附录
- **06-测试报告**：性能与资源占用、运行时测试报告

## 项目状态

- **当前版本**：0.1.0（开发中）
- **目标平台**：Windows 10 / 11
- **项目状态**：积极开发中，API 与功能可能调整

## 贡献指南

欢迎参与贡献。在提交变更前，请：

1. 阅读 [docs/index.md](./docs/index.md) 及相关目录文档，理解架构与规范。
2. 确保以下门槛全部通过，零警告：
   - `cargo test --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo fmt --all --check`

## 许可证

项目采用 GPL-2.0-or-later（GNU GPL v2 或其后版本，SPDX 标识符 `GPL-2.0-or-later`），完整协议文本见 [LICENSE](./LICENSE)。

- **mpv.exe（GPL-2.0-or-later）**：与项目采用相同许可证，捆绑分发无兼容性问题。
- **WebView2（微软专有）**：作为系统内置组件（Windows 11 已内置）通过 WebView2 Runtime 使用。