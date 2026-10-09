[← 返回文档索引](../index.md) > [技术栈](./01-技术栈总览-Tech-Stack-Overview.md) > UI 框架

# MirrorStar Wallpaper（镜星壁纸）技术栈 — UI 框架

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. UI 框架

### 选择：Tauri 壳 + Slint 原生 UI 子进程

主窗口已从 Tauri 内嵌 WebView2 重构为**独立原生 UI 子进程**（`mirrorstar-ui.exe`），Tauri 仅保留为壳（托盘、壁纸引擎、轮换调度、命令桥、UI 子进程管理）。

**选择理由：**

- **内存优先**：WebView2 主窗口需拉起多进程 Chromium 基线（约 +150-300MB），与"极低资源占用"目标冲突；原生 Slint UI 子进程软件渲染后在主窗口打开并静置时，壳 + UI 合计 private ≈ **16.8MB**（验收门槛 ≤40MB）
- **Slint 1.18**：Rust 原生声明式 UI，无 Node/Chromium 运行时依赖，二进制体积小、启动快
- **壳-UI 进程解耦**：UI 崩溃不影响壳与壁纸引擎，可持续驻留托盘
- **Rust 后端**：业务逻辑继续用 Rust 编写（`src-tauri/` + `mirrorstar-core`），与核心业务代码无缝集成
- **安全**：UI 子进程不复制业务、不直接读写配置文件，所有业务经 `ui_proto` 请求壳侧处理，攻击面收敛

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **Tauri WebView2 主窗口** | Rust 后端 + Web UI 灵活 | 主窗口需 Chromium 多进程基线，内存高 | ❌ 与轻量化目标冲突，已移除 |
| **WPF** | 成熟稳定，XAML 数据绑定 | 需要 .NET Framework，体积大，样式定制复杂 | ❌ 运行时依赖过重 |
| **egui** | 纯 Rust，即时模式 | 不适合复杂 UI，无声明式布局 | ❌ 不适合富 UI |
| **WinUI3** | 微软官方，Fluent Design | 需要 Windows App SDK，Rust 绑定不成熟 | ❌ 依赖过重 |
| **Slint 1.18 原生子进程** ✅ | Rust 原生、无 Web 运行时、内存低、声明式 | 需自绘控件与虚拟化网格 | ✅ 最佳平衡 |

### UI 技术方案

**采用：Slint 1.18（Rust 原生）**

- **绑定软件渲染**：`slint = { version = "1.18", default-features = false, features = ["renderer-software"] }`，**不编译** femtovg/skia（GPU/OpenGL 后端），避免 winit 选择 OpenGL 后端导致 +67MB 内存
- **无前端构建产物**：不再使用 Vite / TypeScript / HTML / CSS，也不使用 npm；UI 布局写在 `.slint` 文件中，编译期生成 Rust 代码
- **进程边界**：UI 位于独立 crate `crates/mirrorstar-ui/`（workspace 成员），通过 `mirrorstar-core::ui_proto` 的 NDJSON 行协议 over stdio 与壳通信（消息类型 `Hello` / `Request` / `Response` / `Event` / `Control`）
- **虚拟化壁纸网格**：固定卡尺寸 + 固定列数，仅实例化可见区间 ±2 行缓冲；缩略图按本地文件路径直读（不经 IPC 传字节）
- **四类预览**：图片（自绘）、视频（UI 进程内 spawn mpv 子窗口）、GIF（逐帧动画）、网页（按需创建临时 WebView2 子窗口，关闭后 `msedgewebview2.exe` 计数回落 0）
- **拖拽排序 / 搜索 / 文件拖入 / 卡片操作**：均在原生命中实现

> **未使用** React / Vue / Angular 等前端框架，也不再使用 Tauri IPC（`invoke`）——前端层已被原生 UI 子进程取代。

### 主窗口按需启动（On-Demand）与空闲退出

MirrorStar 的主窗口不在应用启动时创建，也不由壳创建任何 WebView2 窗口（`tauri.conf.json` 的 `app.windows` 为 `[]`）：

- **按需启动**：用户点击托盘"打开主窗口"时，壳侧 UI 子进程管理模块 spawn `mirrorstar-ui.exe`，先握手（`Hello`）再显示
- **空闲退出**：窗口隐藏后按空闲超时（默认 300s，环境变量 `MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`）退出 UI 子进程回收内存（回落至壳内存量级 ≈7.6MB），再次请求可重新拉起
- **单实例**：同一时刻至多一个 UI 子进程实例
- **显隐不激活**：窗口显示时以 `WS_EX_NOACTIVATE` 瞬时覆盖 + `SWP_NOACTIVATE`，不调用任何抢前台 API，避免误触发全屏检测 / 纯净模式；窗口标题保持"镜星壁纸"

此策略的优势：

- 托盘常驻场景下，用户不操作 UI 时零 UI 进程开销，聚焦壁纸渲染性能
- 主窗口静置时壳 + UI 合计 private ≈ 16.8MB，远低于 WebView2 方案

### 系统托盘

`src-tauri/src/lib.rs` 通过 Tauri 2 内置 `tray-icon` feature 创建托盘，菜单共 3 项：

```
镜星壁纸
├── 打开主窗口
├── 暂停壁纸（点击切换为"恢复播放"）
└── 退出
```

托盘菜单 3 项的 handle 分别存储，暂停/恢复项文本随状态动态更新（`tray_pause_resume_item`）。托盘图标缺失时优雅降级（不因图标文件异常导致启动失败）。

***

**相关章节：** [← 总览](./01-技术栈总览-Tech-Stack-Overview.md) | [Windows 系统 API](./03-Windows系统API-Windows-System-API.md) | [壁纸渲染](./04-壁纸渲染-Wallpaper-Rendering.md)