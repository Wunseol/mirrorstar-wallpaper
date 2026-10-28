[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 项目目录结构

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-10-12                |
| 文档状态 | 已实现（基于真实代码审计，2026-10）        |

> 本文以**仓库/目录布局**为主，基于当前仓库实测，不虚构。模块职责仅作概述，详见 [模块设计](../02-架构与技术栈/02-模块设计-Module-Design.md)。

***

## 1. 仓库总览

```text
mirrorstar-wallpaper/
├── .cargo/
│   └── config.toml              # Cargo 配置（不配镜像；windows 注入 /MANIFESTDEPENDENCY）
├── crates/
│   ├── mirrorstar-core/         # 共享核心库（配置/桌面/壁纸/音频/IPC/进程/调度/布局/性能/UI 协议）
│   ├── mirrorstar-ui/           # 原生主窗口 UI 子进程（Slint，独立二进制 mirrorstar-ui.exe）
│   └── mirrorstar-wp-proc/      # Web 壁纸逻辑库（壳 --wp-proc 模式复用，WebView2 隔离）
├── docs/                        # 项目文档
│   ├── 01-需求文档/
│   ├── 02-架构与技术栈/
│   ├── 03-实施与验证/            # 本文档所在目录
│   └── 04-优化文档/
├── mpv/                         # 捆绑的 mpv 播放器（mpv.exe + license）
├── src-tauri/                   # Tauri 壳（Rust + 配置 + 图标 + 测试）
├── Cargo.toml                   # Rust workspace 清单
├── Cargo.lock                   # 依赖锁定
├── LICENSE                      # 开源许可证
├── README.md                    # 仓库说明
├── rust-toolchain.toml          # 固定 Rust 工具链（stable / MSRV 1.80）
└── rustfmt.toml                 # rustfmt 配置
```

> 原前端目录/文件（`src/`、`index.html`、`package.json`、`vite.config.ts`、`tsconfig.json`、`eslint.config.js`、`.prettierrc`、`test-resources/`、`coverage/`）已随主窗口原生 UI 重构删除。主窗口 UI 现由 `crates/mirrorstar-ui/` 的 Slint 原生实现提供。

## 2. Rust workspace

根 `Cargo.toml` 声明**四个成员**（不含 watchdog crate）：

```text
[workspace]
resolver = "2"
members = [
    "src-tauri",
    "crates/mirrorstar-core",
    "crates/mirrorstar-wp-proc",
    "crates/mirrorstar-ui",
]
# [workspace.package] version = "0.1.3", edition = "2021", rust-version = "1.80"
# 依赖在 [workspace.dependencies] 统一声明，子 crate 以 workspace = true 引用
```

## 3. 目录职责概述

> 以下为布局与职责概述。各模块的设计细节、依赖关系与数据流见 [模块设计](../02-架构与技术栈/02-模块设计-Module-Design.md) 与 [依赖与数据流](../02-架构与技术栈/08-依赖与数据流-Dependency-and-Data-Flow.md)。

### 3.1 crates/mirrorstar-core（共享核心库）

`crates/mirrorstar-core/src/` 按业务子系统组织：

| 目录 / 文件 | 职责概述 |
|------|----------|
| `lib.rs`、`perf.rs` | crate 根与模块导出；性能计数器/指标采样 |
| `config/` | 配置加载/保存/热重载/校验、设置结构体、显示器检测、缩略图生成（`manager.rs`、`settings.rs`、`playback.rs`、`detect.rs`、`hot_reload.rs`、`thumbnail.rs`、`validation.rs`） |
| `wallpaper/` | 壁纸生命周期管理；图片/视频/GIF/网页四类渲染与快速路径、GDI 基础与缓存、模式分发、子进程基类（`manager.rs`、`image.rs`、`video.rs`、`gif*.rs`、`web.rs`、`fast_path.rs`、`mode_dispatch.rs`、`subprocess_base.rs`） |
| `desktop/` | 桌面窗口、原生壁纸后端、WorkerW 定位（`window.rs`、`native_wallpaper.rs`、`worker_w.rs`） |
| `audio/` | 音量控制（`volume.rs`） |
| `ipc/` | IPC 客户端、mpv JSON 协议、Web 子进程协议封装（`client.rs`、`mpv_protocol.rs`、`wp_proc.rs`） |
| `process/` | 子进程启动/监控/停止（`manager.rs`） |
| `scheduler/` | 壁纸轮换调度（`mod.rs`、`order.rs`、`boot.rs`） |
| `layout/` | 多屏排列/布局（`mod.rs`） |
| `pure_mode/` | 纯模式（`mod.rs`） |
| `ui_proto/` | 壳 ↔ 原生 UI 的数据契约（消息/命令/分帧/错误码，`mod.rs`、`message.rs`、`command_data.rs`、`framing.rs`、`error_code.rs`） |

`crates/mirrorstar-core/tests/`（集成测试）：`audio_integration.rs`、`ipc_timeout.rs`、`video_wallpaper_fullpath.rs`。

### 3.2 crates/mirrorstar-ui（原生主窗口 UI 子进程）

独立二进制 `mirrorstar-ui.exe`（Slint 1.18，编译期绑定软件渲染）。

| 内容 | 职责概述 |
|------|----------|
| `src/main.rs` | 子进程入口：`Hello` 握手、stdio NDJSON 收发、窗口生命周期 |
| `src/protocol.rs`、`src/store.rs` | 与壳通信的协议处理与本地状态存储 |
| `src/grid.rs`、`src/thumbs.rs`、`src/videothumb.rs` | 虚拟化壁纸网格与缩略图加载 |
| `src/settings.rs`、`src/focus.rs`、`src/keynav.rs` | 设置面板、焦点与键盘导航 |
| `src/dnd.rs`、`src/drop_target.rs` | 原生文件拖入（`IDropTarget`） |
| `src/desktop.rs`、`src/window_pos.rs`、`src/window_icon.rs` | 桌面联动、窗口定位与图标 |
| `src/preview/` | 四类预览：`photo.rs`（图片自绘）、`video.rs`（进程内 mpv 子窗口）、`gif.rs`（逐帧）、`web.rs`（按需临时 WebView2 子窗口）、`host_child.rs`（宿主子窗口） |
| `ui/main.slint`、`ui/widgets.slint` | Slint 声明式界面 |
| `build.rs`、`app.rc`、`app.manifest` | Slint 编译、图标/清单嵌入 |

驱动脚本（同目录）：`measure_grid.ps1`、`measure_scroll_reverify.ps1`、`measure_preview.ps1`、`keyboard_smoke.ps1`、`shell_measure.ps1`、`single_instance_check.ps1`。

### 3.3 crates/mirrorstar-wp-proc（Web 壁纸逻辑库）

库 crate（无独立二进制），由主壳 `mirrorstar-wallpaper.exe` 的 `--wp-proc` 模式复用：

| 文件 | 职责概述 |
|------|----------|
| `lib.rs` | 库入口与公共导出 |
| `webview.rs` | WebView2 环境创建与管理 |
| `ipc_server.rs` | 命名管道 IPC 服务端 |
| `command.rs` | 命令路由与实现 |
| `com.rs` | COM 初始化 |

### 3.4 src-tauri（Tauri 应用壳）

托盘、壁纸引擎、轮换调度、命令桥、UI 子进程管理、事件转发；**不创建任何 WebView2 主窗口**（`app.windows` 为 `[]`）。

| 目录 / 文件 | 职责概述 |
|------|----------|
| `src/lib.rs` | 应用构建 / 命令注册 / 托盘 / 单实例 / 事件回调 |
| `src/main.rs` | 二进制入口，调用 `lib::run()` |
| `src/state.rs` | `AppState`：config_manager / wallpaper_engine / desktop 共享状态 |
| `src/scheduler.rs` | 壳侧轮换调度驱动 |
| `src/commands/` | Tauri 命令：`wallpaper.rs`（壁纸生命周期）、`system.rs`（显示器/自启/对话框）、`config.rs`（配置读写）、`rotation.rs`（轮换） |
| `src/platform/` | 平台集成：`fullscreen.rs`（全屏检测/处置）、`power.rs`（电源）、`explorer.rs`、`workerw_check.rs`、`pure_mode.rs`、`pure_mode_input.rs`、`liveness_watchdog.rs` |
| `src/ui_process/` | UI 子进程管理：`paths.rs`（路径解析）、`bridge.rs`（stdio 桥接） |
| `build.rs`、`manifest.xml` | 构建脚本与 Windows 清单 |
| `capabilities/default.json` | 权限清单 |
| `gen/schemas/` | 构建期自动生成的 schema |
| `icons/` | 各尺寸应用图标 |
| `tests/` | 集成测试：`wallpaper_flow.rs`、`config_flow.rs`、`common/mod.rs` |

配置：`tauri.conf.json`（见 §5）。

## 4. 构建与发布

项目不使用 CI，全部检查在本地手工执行；构建/发布与质量门命令见 [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)。

## 5. 配置（tauri.conf.json）

- `productName`：`MirrorStar Wallpaper`；`identifier`：`com.mirrorstar.wallpaper`。
- `app.windows`：`[]`（壳不创建任何主窗口；主窗口由 `mirrorstar-ui.exe` 子进程按需启动，壁纸窗口运行时创建）。
- `app.withGlobalTauri`：`false`；`app.security.csp`：`default-src 'self'`。
- `bundle.targets`：`["nsis"]`。
- `bundle.resources`：`../mpv/mpv.exe` → `mpv/mpv.exe`、`../mpv/license` → `mpv/license`、`../target/release/mirrorstar-ui.exe` → `mirrorstar-ui.exe`。
- `bundle.icon`：`icons/` 下多尺寸图标（含 `icon.ico` / `icon.icns`）。
- `build`：`{}`（无 `beforeBuildCommand` / `frontendDist` / `devUrl`，不依赖前端产物）。
- `src-tauri/build.rs`：在 `tauri_build::try_build()` 前用**隔离 CARGO_TARGET_DIR** 单独构建 `mirrorstar-ui` 并复制到 `target/release/`；`mirrorstar-wp-proc` 作为库并入壳，不单独构建产物。

## 6. mpv 捆绑

仓库根 `mpv/` 包含 `mpv.exe`（视频播放器）与 `license`（许可证），通过 `bundle.resources` 随安装包分发（见 §5）。运行时查找策略为：捆绑路径优先，找不到时回退到系统 `PATH`。`mpv/` 被 `.gitignore` 忽略（体积超限），需本地手动放置 `mpv.exe`。

## 7. 已删除的历史结构

> **历史记录**：原基于 Vite + TypeScript 的前端（`src/`、`index.html`、`src/scripts/*`、`src/styles/main.css` 及 `*.test.ts`）与构建/测试配置（`package.json`、`vite.config.ts`、`tsconfig.json`、`eslint.config.js`、`.prettierrc`）已随主窗口原生 UI 重构**整体删除**；相关 Vitest/ESLint/Prettier 与 CDP e2e（`test-resources/`）测试体系亦已移除。原主窗口 WebView2 裁剪/挂起回收相关配置（`MAIN_WINDOW_BROWSER_ARGS`、`webview_memory`）与 `src-tauri/tests/csp_guard.rs`（CSP 策略守卫）随之移除。

***

## 相关文档

- [实施与验证总览](./01-实施与验证总览-Implementation-and-Verification-Overview.md)
- [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)
- [质量保障](./04-质量保障-Quality-Assurance.md)
- [测试报告](./05-测试报告-Test-Reports.md)
- [模块设计](../02-架构与技术栈/02-模块设计-Module-Design.md)
- [依赖与数据流](../02-架构与技术栈/08-依赖与数据流-Dependency-and-Data-Flow.md)
- [进程架构与 IPC](../02-架构与技术栈/03-进程架构与IPC-Process-Architecture-and-IPC.md)