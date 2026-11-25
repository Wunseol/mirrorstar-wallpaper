[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 总览

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-11-24                |
| 文档状态 | 已实现（基于真实代码审计与 2026-11-24 实测）        |

> 本文合并原《实施规划总览》与《开发阶段划分》，将项目阶段与里程碑收敛为单一路径。
>
> **架构基线**：主窗口已重构为独立原生 UI 子进程（`crates/mirrorstar-ui/`，Slint 1.18 软件渲染）；原基于 Vite + TypeScript 的前端（`src/`、`index.html`、`package.json` 等）及其测试（Vitest / ESLint / Prettier / CDP e2e）已在 2026-10-02 的「主窗口原生 UI 重构」中**整体删除**。当前测试体系为 Rust 单元/集成测试（`cargo test --workspace`）+ PowerShell 驱动脚本 e2e（`crates/mirrorstar-ui/`）。

***

## 1. 实现概述

### 1.1 总体实现策略

项目采用**增量式开发**策略，每个阶段交付一个可运行的完整产品，各阶段以前一阶段为基础，确保仓库始终处于可编译、可运行状态。

核心原则：

- **每个里程碑交付可运行程序**
- **风险前置**：把技术风险最高的部分（桌面集成、视频播放）放在最前阶段尽早验证
- **垂直切片**：每个阶段实现从底层到呈现层的完整垂直功能切片
- **质量门**：提交前在本地执行 `cargo check` / `clippy` / `fmt --check` / `test` / `cargo audit`，确保可编译可测试

### 1.2 开发方法论

迭代式开发，每个阶段为一个迭代周期，阶段内部短周期迭代，阶段结束进行里程碑评审与验收，达标后进入下一阶段。

### 1.3 阶段依赖关系

```mermaid
graph LR
    P3[P3: Web 壁纸子进程 ✅] --> P4[P4: 体验优化 ✅]
    P4 --> P2[P2: 架构优化与清理 ✅]
    P2 --> P5[P5: 打包发布 ✅ 已打包 0.1.6]
    P5 --> P6[P6: 主窗口原生 UI 重构 ✅]

    style P3 fill:#4CAF50,color:#fff
    style P4 fill:#4CAF50,color:#fff
    style P2 fill:#4CAF50,color:#fff
    style P5 fill:#4CAF50,color:#fff
    style P6 fill:#4CAF50,color:#fff
```

> 说明：原 Phase 1「前端 UI 补全」随主窗口原生 UI 重构失效，其 UI 能力已由 Phase 6 的原生 UI 子进程重新实现；故本文不再单列独立阶段，仅在 §2 以历史条目保留。

***

## 2. 阶段与里程碑

阶段与里程碑合并为单一视图（真实状态）。

### 2.1 阶段状态

| 阶段 | 内容 | 状态 |
|------|------|------|
| Phase 3 | Web 壁纸子进程 | ✅ 已完成 |
| Phase 4 | 体验优化 | ✅ 已完成 |
| Phase 2 | 架构优化与代码清理 | ✅ 已完成 |
| Phase 5 | 打包发布 | ✅ 已打包 0.1.6（2026-11-24） |
| Phase 6 | 主窗口原生 UI 重构 | ✅ 已完成 |
| Phase 1 | 前端 UI 补全 | ⚫ 已废弃（历史，能力迁至 Phase 6） |

### 2.2 里程碑

| 里程碑 | 对应阶段 | 状态 |
|--------|----------|------|
| M3 Web 壁纸子进程稳定运行 | Phase 3 | ✅ 达成 |
| M4 用户体验完善 | Phase 4 | ✅ 达成 |
| M2 架构整洁、可维护 | Phase 2 | ✅ 达成 |
| M5 可分发安装包 | Phase 5 | ✅ 已生成安装包；正式发布待定 |
| M6 主窗口原生 UI 低内存 | Phase 6 | ✅ 达成 |
| M1（历史）前端交互完整可用 | Phase 1 | ⚫ 已废弃（由 M6 取代） |

### 2.3 阶段要点

**Phase 3：Web 壁纸子进程 ✅**

- `mirrorstar-wp-proc` 为**库 crate**，由主壳 `mirrorstar-wallpaper.exe` 的 `--wp-proc` 模式复用（同一 exe、双入口），不再是独立二进制。
- 通过命名管道 IPC 将 WebView2 渲染隔离到子进程，实现崩溃隔离与内存优化；主进程负责 WorkerW 嵌入。
- 配套 `WpProcIpcClient`（`mirrorstar-core/src/ipc/wp_proc.rs`）与 `WebRenderer` 代理层（`mirrorstar-core/src/wallpaper/web.rs`）。
- 独立的 `mirrorstar-watchdog` crate 已移除。

**Phase 4：体验优化 ✅**

- 壁纸预览与「设为壁纸」、搜索、关于页面、加载动画、电池供电暂停、全屏处置选择（终止/暂停/持续）。

**Phase 2：架构优化与代码清理 ✅**

- 移除未使用的 write-only `OnceLock`，收敛为 `SHARED_ENGINE` / `SHARED_CONFIG` / `SHARED_APP_HANDLE`。
- Explorer 桌面自愈：事件驱动（`TaskbarCreated` / WorkerW·Progman 销毁 / 会话解锁 / 睡眠恢复，统一经 `try_heal_desktop_layer`，销毁类事件 1s 去抖）+ `workerw_check` 每 300s 兜底。
- 托盘菜单增加「暂停/恢复壁纸」快捷项，文本随状态动态切换。
- 配置校验、GIF 内存策略、进程/管道会话清理等多项重构。

**Phase 5：打包发布 ✅ 已打包 0.1.6（2026-11-24）**

- `bundle.targets = ["nsis"]`；资源捆绑 `mpv.exe` 与 `mirrorstar-ui.exe`；图标/标识配置就绪。
- 发布由本地执行 `cargo tauri build` 打包 NSIS 安装包后人工发布（不依赖 Node/npm）。
- 0.1.6 NSIS 安装包已生成：`target/release/bundle/nsis/MirrorStar Wallpaper_0.1.6_x64-setup.exe`（2026-11-24）。
- 待办：正式发布（发布渠道与时间待定）。

**Phase 6：主窗口原生 UI 重构 ✅**

- 新增 crate `crates/mirrorstar-ui/`（Slint 1.18，编译期绑定软件渲染：`default-features = false` + `renderer-software`）。
- 壳（`src-tauri/`）不再创建任何 WebView2 主窗口（`app.windows: []`），仅保留托盘、壁纸引擎、轮换调度、命令桥与 UI 子进程管理。
- 壳 ↔ UI 通过 `mirrorstar-core::ui_proto` 的 NDJSON 行协议 over stdio（`Hello` / `Request` / `Response` / `Event` / `Control`）；业务逻辑仅在壳侧。
- UI 生命周期：按需启动 + 握手 + 窗口隐藏超时退出 + 单实例；用户主动唤起时置前聚焦（`AllowSetForegroundWindow` + `SetForegroundWindow`），纯净模式等非用户主动场景仍禁止抢前台。
- 原生虚拟化壁纸网格；四类预览（图片自绘 / 视频进程内 mpv 子窗口 / GIF 逐帧 / 网页按需临时 WebView2 子窗口）。
- 2026-11-24 实测（Debug 0.1.5）：壳 + UI（不含 mpv）私有工作集 14.1–16.7MB / 工作集 43.2–46.9MB；关闭 UI 93ms；热打开首次 441ms、后续 133–164ms；冷启动 `setup_ready` 43ms。历史基线（原生 UI 重构初期，**历史/待复测**）：合计私有工作集 ≈16.8MB、关闭后回落 ≈7.6MB、idle CPU ≈0.4%。测试细节见 [性能优化](../02-架构与技术栈/09-性能优化-Performance.md) §21、§22.4。

***

## 3. 当前进度总结

> 基于当前仓库源码与配置的实测。

### 3.1 已完成能力

| 模块 | 完成度 | 说明 |
|------|--------|------|
| 四种壁纸类型端到端 | ✅ 已实现 | 图片 / GIF / 视频 / 网页（添加→设置→显示→退出恢复） |
| 桌面集成 | ✅ 已实现 | WorkerW 三重查找嵌入、原生壁纸 API 双路径、Explorer 重启恢复、显示器枚举 |
| 渲染优化 | ✅ 已实现 | HALFTONE、双缓冲、专用线程消息循环、GIF 内存策略、图片降采样；GIF/图片暂停释放资源，视频暂停保留当前帧 |
| 全屏检测 | ✅ 已实现 | 系统/对象 WinEvent + 500ms QUNS/前台根探针 + 2s 全量兜底 + 400ms 会话确认 |
| 音量控制 | ✅ 已实现 | 视频经 WASAPI 进程级（mpv）控制，网页经 wp-proc IPC 控制音量/静音；无音频设备时降级 no-op |
| IPC 通信 | ✅ 已实现 | mpv 原生 JSON 管道 + wp-proc 自定义管道（`MpvIpcClient` / `WpProcIpcClient`）+ `ui_proto` NDJSON over stdio（壳 ↔ `mirrorstar-ui`） |
| 进程管理 | ✅ 已实现 | `ProcessManager` 子进程管理 + 退出监听/崩溃检测；异常退出不自动 respawn（仅标记 `Terminated` 并通知 UI） |
| 配置管理 | ✅ 已实现 | TOML + 热重载 + 原子写入 + 校验 + 缩略图管理 |
| Tauri 应用层 | ✅ 已实现 | 命令注册 + 托盘 + 单实例 + DPI 感知 + 电源监控 |
| Web 壁纸子进程 | ✅ 已实现 | 壳 `--wp-proc` 模式（`mirrorstar-wp-proc` 库 crate）+ `WpProcIpcClient` + `WebRenderer` |
| 主窗口原生 UI | ✅ 已实现 | `crates/mirrorstar-ui/`（Slint）虚拟化网格、设置、四类预览、键盘/拖放交互 |
| 壁纸轮换调度 | ✅ 已实现 | 调度器与多屏排列（`mirrorstar-core/src/scheduler/`，壳侧 `scheduler.rs`） |
| 打包发布 | ✅ 已打包 0.1.6（2026-11-24），正式发布待定 | NSIS 安装包 + 资源捆绑，产物 `target/release/bundle/nsis/MirrorStar Wallpaper_0.1.6_x64-setup.exe` |

> 各模块的职责与规模细节见 [模块设计](../02-架构与技术栈/02-模块设计-Module-Design.md) 与 [项目目录结构](./03-项目目录结构-Project-Structure.md)。

### 3.2 后续优化方向

> 以下为可选改进项（非当前阻塞项），按价值高低排序，立项时按需实施：

1. **精细鼠标交互**：RawInput 全局鼠标捕获 + 消息转发，解决交互模式下桌面图标不可点击问题。
2. **应用规则系统**：pause / ignore / kill 规则，支持按窗口标题/进程规则处置。
3. **多显示器感知暂停**：仅暂停全屏显示器上的壁纸（当前全屏检测为全局暂停）。
4. **高对比度模式**：检测系统高对比度并切换到 bottom-most 渲染。
5. **国际化**：多语言支持。
6. **更多壁纸类型**：Unity / Godot 等外部程序壁纸。

### 3.3 2026-11-24 验证快照

> 环境与判定口径同《02 架构与技术栈 / 09 性能优化》§22.4 与《01 需求文档 / 03 非功能性需求》§6.2。以下均为 Debug 0.1.5 实测（2026-11-24）。

| 项目 | 实测值 | 目标 / 判定 |
|------|--------|-------------|
| 暂停态（主进程） | 私有 8.7MB / 工作集 22.9MB；CPU 0.00% | 目标 <25MB / <0.1% → ✅ |
| 活跃视频（主进程） | 私有 7.7–8.6MB / 工作集 21.7–22.8MB；CPU 0.00% | 目标 <30MB / <1% → ✅ |
| 活跃视频（mpv） | 私有 430.8–432.2MB / 工作集 202.5–204.1MB；CPU 15.4–25.5% 单核 / 1.9–3.2% 全机 | 记录值（无独立门槛） |
| 壳 + UI（不含 mpv） | 私有 14.1–16.7MB / 工作集 43.2–46.9MB | 历史门槛 ≤40MB → ✅（当前 NFR 仅约束主进程，此项仅记录） |
| 关闭 UI / 热打开 / 冷启动 | 关闭 93ms；热打开首次 441ms、后续 133–164ms；冷启动 `setup_ready` 43ms | ✅ |
| 视频壁纸切换 | 379–431ms | 目标 <0.5s → ✅ |
| 图片切换 | 825–1943ms（5115×2826 JPG、7500×5000 PNG） | 目标 <0.5s → ❌ 未达标；瓶颈为解码/降采样 |
| 视频暂停 / 恢复 | 手动与托盘命令路径 1.7–10.9ms / 1.9–4.3ms；UI 按钮更新 10.6–13.3ms | ✅ |
| 托盘菜单响应 | 35.3–48.7ms | ✅ |
| GPU 计数器 | 本机可用；mpv 3s 采样 3D 27.77%、videodecode 22.89%、专用显存 571,658,240 B | 记录值 |

**未测项**：GIF 切换、GIF/图片暂停、Release 验收。

> 详细数据与复现步骤见 [性能优化](../02-架构与技术栈/09-性能优化-Performance.md) §22.4；验收条件见 [非功能性需求](../01-需求文档/03-非功能性需求-Non-Functional-Requirements.md) §6.2。

***

## 相关文档

- [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)
- [项目目录结构](./03-项目目录结构-Project-Structure.md)
- [质量保障](./04-质量保障-Quality-Assurance.md)
- [测试报告](./05-测试报告-Test-Reports.md)
- [架构与技术栈总览](../02-架构与技术栈/01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [模块设计](../02-架构与技术栈/02-模块设计-Module-Design.md)
- [优化演进摘要](../04-优化文档/01-优化演进摘要.md)
- [附录B-版本历史](../04-优化文档/10-附录B-版本历史.md)