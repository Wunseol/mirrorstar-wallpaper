[← 返回文档索引](../index.md) > 实施规划 > 项目目录结构

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 项目目录结构

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 仓库总览

以下结构基于对当前仓库（2026-08-29）的实测，不虚构。

```text
mirrorstar-wallpaper/
├── .cargo/config.toml              # Cargo 配置文件
├── .github/
│   └── workflows/
│       ├── checks.yml              # 可复用检查工作流（仅 rust-checks）
│       ├── ci.yml                  # 推送/PR 触发，复用 checks
│       └── release.yml             # v* tag 触发，含签名密钥前置门，构建并发布
├── crates/
│   ├── mirrorstar-core/            # 核心库（配置/桌面/壁纸/Audio/IPC/进程/性能/UI 协议）
│   ├── mirrorstar-ui/              # 原生主窗口 UI 子进程（Slint，独立二进制）
│   └── mirrorstar-wp-proc/         # Web 壁纸逻辑库（壳 --wp-proc 模式复用，WebView2 隔离）
├── docs/                           # 项目文档（01 需求 / 02 架构 / 03 技术栈 / 04 实施规划 / 测试报告 / 优化文档）
├── mpv/                            # 捆绑的 mpv 播放器（mpv.exe + license.txt）
├── src-tauri/                      # Tauri 壳（Rust + 配置 + 图标 + 测试）
├── wallpaper/                      # 本地示例壁纸（mp4 / png）
├── Cargo.toml                      # Rust workspace 清单
├── Cargo.lock                      # 依赖锁定
├── LICENSE                         # 开源许可证
├── README.md                       # 仓库说明
├── rust-toolchain.toml             # 固定 Rust 工具链（stable / MSRV 1.80）
└── rustfmt.toml                    # rustfmt 配置
```

> 原前端目录/文件（`src/`、`index.html`、`package.json`、`vite.config.ts`、`tsconfig.json`、`eslint.config.js`、`.prettierrc`、`test-resources/`、`coverage/`）已随主窗口原生 UI 重构删除。主窗口 UI 现由 `crates/mirrorstar-ui/` 的 Slint 原生实现提供。

## 2. Rust workspace

`Cargo.toml`（workspace 根）声明四个成员，不含 watchdog crate：

```text
[workspace]
members：
  - crates/mirrorstar-core      # 共享核心库
  - crates/mirrorstar-ui        # 原生主窗口 UI 子进程二进制（Slint）
  - crates/mirrorstar-wp-proc   # Web 壁纸逻辑库（由壳 --wp-proc 模式复用）
  - src-tauri                   # Tauri 应用壳
resolver = "2"
rust-version = 1.80              # MSRV
```

### 2.1 crates/mirrorstar-core

核心库，按业务子系统组织子模块（`crates/mirrorstar-core/src/`）：

| 目录 | 主要文件 | 说明 |
|------|----------|------|
| `lib.rs` (207 行) | — | crate 根，导出各模块与公共类型 |
| `perf.rs` (223 行) | — | 性能计数器 / 指标采样 |
| `config/` | `manager.rs` (2838 行) | 配置加载 / 保存 / 变更分发 |
| | `validation.rs` (36 行) | 配置项校验辅助 |
| | `settings.rs` (801 行) | 持久化设置结构体 |
| | `detect.rs` (819 行) | 显示器检测与映射 |
| | `hot_reload.rs` (600 行) | 配置文件热重载 |
| | `thumbnail.rs` (1303 行) | 缩略图生成 |
| `wallpaper/` | `manager.rs` (2591 行) | 壁纸生命周期管理（增删/应用/状态） |
| | `video.rs` (1259 行) | 视频壁纸（mpv）渲染协调 |
| | `web.rs` (652 行) | Web 壁纸渲染协调（走子进程） |
| | `image.rs` (1104 行) | 静态图片壁纸 |
| | `gif.rs` (2172 行) / `gif_decode.rs` (2689 行) / `gif_memory.rs` (808 行) | GIF 解码 / 播放 / 内存优化 |
| | `fast_path.rs` (1316 行) | 高性能快速路径 |
| | `gdi_base.rs` (860 行) / `gdi_cache.rs` (192 行) | GDI 基础与缓存 |
| | `mode_dispatch.rs` (514 行) | 渲染模式分发 |
| | `subprocess_base.rs` (494 行) | 子进程基类 |
| `audio/` | `volume.rs` (1119 行) | 音量控制 |
| `desktop/` | `window.rs` (272 行) | 桌面窗口 |
| | `native_wallpaper.rs` (599 行) | 原生壁纸后端 |
| | `worker_w.rs` (987 行) | WorkerW 窗口定位 |
| `ipc/` | `client.rs` (1021 行) | IPC 客户端 | 
| | `mpv_protocol.rs` (355 行) | mpv JSON IPC 协议 |
| | `wp_proc.rs` (699 行) | Web 子进程协议封装 |
| `process/` | `manager.rs` (1204 行) | 子进程启动 / 监控 / 停止 |

`crates/mirrorstar-core/tests/`（集成测试）：

| 文件 | 行数 |
|------|------|
| `audio_integration.rs` | 61 |
| `ipc_timeout.rs` | 239 |
| `video_wallpaper_fullpath.rs` | 124 |

### 2.2 crates/mirrorstar-wp-proc

Web 壁纸渲染逻辑库（壳 `--wp-proc` 模式复用，独立窗口渲染 Web 壁纸）：

| 文件 | 行数 | 说明 |
|------|------|------|
| `main.rs` | 422 | 子进程入口，组件注册与消息循环 |
| `ipc_server.rs` | 1466 | IPC 命令服务器 |
| `webview.rs` | 1452 | WebView 封装与渲染 |
| `command.rs` | 1145 | 命令路由与实现 |
| `com.rs` | 103 | COM 初始化 |

### 2.3 crates/mirrorstar-ui

原生主窗口 UI 子进程（独立二进制 `mirrorstar-ui.exe`，Slint 1.18，绑定软件渲染）：

| 内容 | 说明 |
|------|------|
| `main.rs` | 子进程入口：`Hello` 握手、stdio NDJSON 收发、窗口生命周期 |
| `*.slint` | Slint 声明式 UI（虚拟化壁纸网格、设置面板、预览等） |
| 预览实现 | 图片自绘 / 视频（进程内 spawn mpv 子窗口）/ GIF 逐帧 / 网页按需临时 WebView2 子窗口 |
| 驱动脚本 | `measure_grid.ps1`、`measure_scroll_reverify.ps1`、`measure_preview.ps1`、`keyboard_smoke.ps1`（以及可能位于此处的 `shell_measure.ps1`、`single_instance_check.ps1`） |

- 依赖 Slint：`default-features = false` + `features = ["renderer-software"]`（不编译 femtovg/skia）
- 与壳通过 `mirrorstar-core::ui_proto` 通信；不复制业务逻辑、不直接读写配置文件

### 2.4 src-tauri

Tauri 应用壳（Rust）：托盘、壁纸引擎、轮换调度、命令桥、UI 子进程管理、事件转发；**不创建任何 WebView2 主窗口**（`app.windows` 为 `[]`）。

> 注：下表列出的文件行数为历史审计值，主窗口重构后 `src-tauri/` 内部结构已调整（移除主窗口相关模块：前端加载、CSP、WebView2 裁剪/挂起等），行数不再逐一核对。

| 文件 | 行数 | 说明 |
|------|------|------|
| `lib.rs` | 1314 | 应用构建 / 命令注册（20 个）/ 托盘 / 单实例 / 事件回调 |
| `main.rs` | 61 | 二进制入口，调用 `lib::run()` |
| `state.rs` | 1102 | `AppState`：config_manager / wallpaper_engine / desktop 共享状态 |
| `commands/wallpaper.rs` | 2449 | 壁纸生命周期命令（13 个） |
| `commands/system.rs` | 239 | 系统级命令（显示器 / 自动启动 / 对话框等） |
| `commands/config.rs` | 161 | 配置命令（get/update） |
| `platform/fullscreen.rs` | 1239 | 全屏检测 / 处置 |
| `platform/power.rs` | 235 | 电源状态（电池供电检测） |
| `platform/explorer.rs` | 289 | 资源管理器集成 |
| `platform/workerw_check.rs` | 245 | WorkerW 可用性检查 |
| `tests/config_flow.rs` | 231 | 配置流程集成测试 |
| `tests/csp_guard.rs` | 160 | CSP 策略守卫测试 |
| `tests/wallpaper_flow.rs` | 1231 | 壁纸生命周期集成测试 |
| `tests/common/mod.rs` | 222 | 测试公共工具 |

配置与其余文件：

- `tauri.conf.json` — 见「配置」一节。
- `capabilities/default.json` — 权限清单。
- `build.rs` / `manifest.xml` — 构建脚本与清单。
- `icons/` — 各尺寸应用图标。
- `gen/schemas/` — 自动生成的 schema。

## 3. 前端（src/，已删除）

> **历史记录**：原基于 Vite + TypeScript 的前端（`src/`、`index.html`、`src/scripts/main.ts`、`src/scripts/ipc.ts`、`src/scripts/ui/*`、`src/styles/main.css` 及 `*.test.ts`）与构建/测试配置（`package.json`、`vite.config.ts`、`tsconfig.json`、`eslint.config.js`）已随主窗口原生 UI 重构**整体删除**。主窗口 UI 现由 `crates/mirrorstar-ui/`（Slint）实现，见 2.3。前端相关 Vitest/ESLint/Prettier 与 CDP e2e（`test-resources/`）测试体系亦已移除。

## 4. CI/CD（.github/workflows/）

| 工作流 | 说明 |
|--------|------|
| `checks.yml` | 可复用检查：仅 `rust-checks`（fmt/clippy/test/audit），`frontend-checks` 已删除 |
| `ci.yml` | push / PR 触发，复用 `checks.yml` |
| `release.yml` | `v*` tag 触发，需签名密钥，前置运行 checks，然后 `cargo install tauri-cli` + `tauriScript: cargo tauri` 构建发布（draft release），不再使用 Node/npm |

## 5. 配置（tauri.conf.json）

- `productName`：镜星壁纸；`identifier`：`com.mirrorstar.wallpaper`。
- `app.windows`：`[]`（壳不创建任何主窗口；主窗口由 `mirrorstar-ui.exe` 子进程按需启动，壁纸窗口运行时创建）。
- `bundle.targets`：`["nsis"]`。
- `bundle.resources`：`mpv/mpv.exe`、`mpv/license`、内置 `target/release/mirrorstar-ui.exe`（与主 exe 同目录，`paths.rs` 依此解析）。
- `build`：为空（无 `beforeBuildCommand` / `frontendDist` / `devUrl`，不再依赖前端产物）。
- `src-tauri/build.rs`：在 `tauri_build::try_build()` 前用**隔离 CARGO_TARGET_DIR**单独构建 `mirrorstar-ui`（单独一次 cargo 调用，避免 slint 特性合并）并复制到 `target/release/`；`mirrorstar-wp-proc` 已并入壳，作为库编译，不再单独构建产物。

## 6. mpv 捆绑

仓库根 `mpv/` 包含 `mpv.exe`（视频播放器）与 `license.txt`（许可证），通过 `bundle.resources` 随安装包分发（见第 5 节）。运行时查找策略为：捆绑路径优先，找不到时回退到系统 `PATH`。

## 7. 相关章节

- [实施规划总览](01-实施规划总览-Implementation-Overview.md)
- [开发阶段划分](03-开发阶段划分-Development-Phases.md)
- [质量保障](06-质量保障-Quality-Assurance.md)
- [架构概述与系统架构（02）](../02-架构设计/01-架构概述与系统架构-Architecture-Overview.md)
- [模块设计（02）](../02-架构设计/03-模块设计-Module-Design.md)
- [进程架构（02）](../02-架构设计/04-进程架构-Process-Architecture.md)