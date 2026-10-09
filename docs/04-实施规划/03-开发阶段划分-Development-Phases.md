[← 返回文档索引](../index.md) > [实施规划](./01-实施规划总览-Implementation-Overview.md) > [开发阶段划分](./03-开发阶段划分-Development-Phases.md)

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 开发阶段划分

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

> 本文档按真实代码审计记录各阶段完成情况。已实现的核心能力（四种壁纸类型端到端、桌面集成、渲染优化、全屏检测、音量控制、IPC、进程管理、配置管理、Tauri 应用层、Web 壁纸子进程、前端 UI）不再列入未完成任务。

> **架构变更说明**：本项目的**主窗口已重构为独立原生 UI 子进程**（`crates/mirrorstar-ui/`，Slint 1.18 软件渲染）。原来基于 Vite + TypeScript 的前端（`src/`、`index.html`、`package.json` 等）及相关测试（Vitest / ESLint / Prettier / CDP e2e）**已全部删除**，测试体系改为 Rust 单元/集成测试（`cargo test --workspace`）+ PowerShell 驱动脚本 e2e。下文 Phase 1 关于"前端 UI 补全"的描述为历史记录，仅作演进参考；当前主窗口能力见 Phase 6。

---

## 阶段总览

| 阶段 | 内容 | 状态 |
|------|------|------|
| Phase 1 | 前端 UI 补全 | ✅ 已完成（前端已删除，能力迁至 Phase 6 原生 UI 子进程） |
| Phase 2 | 架构优化与代码清理 | ✅ 已完成 |
| Phase 3 | Web 壁纸子进程 | ✅ 已完成 |
| Phase 4 | 体验优化 | ✅ 已完成 |
| Phase 5 | 打包发布 | 🔧 已配置（NSIS + 资源 + 发布流水线） |
| Phase 6 | 主窗口原生 UI 重构 | ✅ 已完成 |

---

### Phase 1: 前端 UI 补全 ✅ 已完成（历史）

> **历史记录**：本阶段实现的前端（`index.html`、`src/scripts/main.ts`、Vitest 单测等）已在"主窗口原生 UI 重构"中整体删除，UI 能力由 Phase 6 的原生 UI 子进程重新实现。以下内容保留作演进参考。

**目标**：补全前端 UI 控件，接通后端命令。

**当前状态（实测）**：`index.html`（仓库根）+ `src/scripts/main.ts`（731 行）已实现的 UI 控件与后端命令完整打通：

| UI 控件 | 关联后端命令 | 说明 |
|---------|-------------|------|
| 暂停 / 恢复按钮 | `pause_wallpaper` / `resume_wallpaper` | `#pause-btn` `#resume-btn` |
| 音量滑块 | `set_volume` | `#volume-slider`（带 `display_id` 参数） |
| 静音按钮 | `toggle_mute` | `#mute-btn` |
| 播放速度滑块 | `set_speed` | `#speed-slider`（0.25–4.0，mpv） |
| 目标显示器 | `get_displays` | `#display-select` |
| 多屏排列选择器 | `update_config` | `#arrangement-select`（per_monitor / all_same / span） |
| 缩放模式 | `set_scaling_mode` | `#scaling-mode-select`（fill/fit/stretch/center/original） |
| 全屏处置 | `update_config` | `#fullscreen-action-select`（terminate / pause / none） |
| 电池暂停 | `update_config` | `#pause-on-battery` |
| 交互模式 | `set_interaction_mode` | `#interaction-mode` |
| 开机自启 | `toggle_auto_start` / `get_auto_start_status` | `#auto-start` |
| 搜索 | — | `#wallpaper-search` 前端过滤 |
| 重新生成缩略图 | `regenerate_thumbnails` | `#regenerate-thumbs-btn` + 进度条 |
| 添加壁纸 | `add_wallpaper`（文件对话框 `open_file_dialog`） | `#add-wallpaper-btn` |
| 拖放添加 | `add_wallpaper` | `scripts/ui/drag-drop.ts`（enter/over/leave/drop，多文件串行） |
| 预览 / 设为壁纸 | `preview-modal` → `set_wallpaper` | `scripts/ui/preview-modal.ts` |

**交付物**：前端 UI 与后端命令完全打通（20 个命令均可通过界面操作）。

---

### Phase 2: 架构优化与代码清理 ✅ 已完成

**目标**：移除死代码、修复已知问题、提升稳定性。经多次重构迭代完成，主要包括：

- 移除未使用的 write-only OnceLock，收敛为 `SHARED_ENGINE` / `SHARED_CONFIG` / `SHARED_APP_HANDLE`
- `main.rs` 不再在日志初始化前调用 tracing（仅 `ensure_single_instance` + `run()`）
- Explorer 重启监控：`TaskbarCreated` 消息监听 + 5 分钟轮询兜底
- 系统托盘菜单增加"暂停/恢复壁纸"快捷项，文本随状态动态切换
- 前端 `set_volume` 统一 `displayId: displayId || null` 转换，修复多显示器音量
- 配置校验（`config/validation.rs`）、GIF 内存策略、进程/管道会话清理等多项重构

---

### Phase 3: Web 壁纸子进程 ✅ 已完成

**目标**：将 Web 壁纸渲染隔离到独立子进程，实现崩溃隔离 + 内存优化。

**已完成内容**：

- `mirrorstar-wp-proc` 库 crate（壳 `--wp-proc` 模式复用），模块划分：
  | 文件 | 职责 | 行数（实测） |
  |------|------|-------------|
  | `main.rs` | 入口 + 消息循环 | 422 |
  | `webview.rs` | WebView2 环境创建与管理 | 1452 |
  | `ipc_server.rs` | 命名管道 IPC 服务端 | 1466 |
  | `command.rs` | 命令处理 | 1145 |
  | `com.rs` | COM 初始化 | 103 |
- `WpProcIpcClient`（`mirrorstar-core/src/ipc/wp_proc.rs`，699 行）：JSON + 换行分隔协议，复用连接重试 + request_id 模式
- `WebRenderer` 代理层（`mirrorstar-core/src/wallpaper/web.rs`，652 行）：通过 `ProcessManager` 启动子进程 + `WpProcIpcClient` 通信
- HWND 通过 FindWindowW + PID 校验获取，主进程执行 WorkerW 嵌入
- 子进程崩溃检测：IPC 管道断开检测
- `mirrorstar-watchdog` 独立 crate 已移除（不再需要独立看门狗进程）

---

### Phase 4: 体验优化 ✅ 已完成

**目标**：完善用户体验。

**已完成内容**：

- 壁纸预览（点击放大查看）＋ "设为壁纸"
- 壁纸搜索功能（前端过滤）
- 响应式设计（媒体查询适配窄屏）
- 关于页面（动态版本号 + 技术栈）
- 加载动画（骨架屏 + 缩略图渐显）
- 电池供电暂停（后端电源监控 + 前端 UI 开关）
- 全屏处置选择（终止 / 暂停 / 持续）

---

### Phase 5: 打包发布 🔧 已配置

**目标**：制作可发布安装包。当前已完成配置，剩余发布动作（正式打 tag）未执行。

| 任务 | 状态 | 说明 |
|------|------|------|
| CSP 安全策略 | ✅ 已配置 | `app.security.csp`：`default-src 'self'`，允许 `wpfile:` 自定义协议加载图片/media/frame |
| NSIS 打包 | ✅ 已配置 | `bundle.targets=["nsis"]` |
| 资源捆绑 | ✅ 已配置 | `resources`：`mpv/mpv.exe`、`mpv/license`、`target/release/mirrorstar-ui.exe`（与主 exe 同目录） |
| 图标 / 版本 | ✅ 已配置 | `icons/*`，productName `镜星壁纸`，identifier `com.mirrorstar.wallpaper` |
| 发布流水线 | ✅ 已配置 | `.github/workflows/release.yml`：打 `v*` tag 触发，需签名密钥，构建 NSIS 并起草 Release（不再使用 Node/npm，改为 `cargo install tauri-cli` + `tauriScript: cargo tauri`） |
| 正式打 tag 发布 | ⬜ 待执行 | 在发布时机打 `v0.x.x` tag 触发 Release 工作流 |

**验收标准**：

- ✅ NSIS 安装包可正常安装/卸载，含 mpv.exe 与 mirrorstar-ui.exe
- ✅ WebView2 Runtime 缺失时提示（仅网页壁纸/预览需要）
- ✅ mpv.exe 随包分发
- ✅ 应用图标、版本信息正确
- ⬜ 完成一次正式 tag 发布

> **说明**：原表中"CSP 安全策略"行（`app.security.csp`）针对 Tauri 内嵌 WebView2 主窗口，该主窗口已移除，CSP 约束不再适用于主窗口；前端已无 `wpfile://` 协议与页面入口。

---

### Phase 6: 主窗口原生 UI 重构 ✅ 已完成

**目标**：将主窗口从 Tauri 内嵌 WebView2 改为独立原生 UI 子进程，大幅降低内存占用。

**已完成内容**：

- 新增 crate `crates/mirrorstar-ui/`（Slint 1.18，**绑定软件渲染**：`default-features=false` + 仅 `renderer-software`）
- 壳（`src-tauri/`）不再创建任何 WebView2 主窗口（`app.windows: []`），仅保留托盘、壁纸引擎、轮换调度、命令桥与 UI 子进程管理
- 壳↔UI 通过 `mirrorstar-core::ui_proto` 的 NDJSON 行协议 over stdio（`Hello` / `Request` / `Response` / `Event` / `Control`）；业务逻辑仅在壳侧
- UI 生命周期：按需启动 + 握手 + 窗口隐藏超时（默认 300s，`MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`）退出 + 单实例
- 窗口显隐不激活（`WS_EX_NOACTIVATE` + `SWP_NOACTIVATE`，不抢前台）
- 原生虚拟化壁纸网格（固定卡尺寸 + 固定列数，仅实例化可见区间 ±2 行缓冲）；缩略图按本地文件路径直读
- 四类预览：图片（自绘）、视频（UI 进程内 mpv 子窗口）、GIF（逐帧动画）、网页（按需临时 WebView2 子窗口）
- 删除旧前端与 `mirrorstar-ui-poc` MLP；删除 WebView2 主窗口裁剪（`MAIN_WINDOW_BROWSER_ARGS`）与挂起回收（`webview_memory`）
- **行为变更**：移除"全屏应用时自动最小化主窗口 / 退出全屏恢复"逻辑（主窗口已是独立进程，无需最小化/恢复）；`fullscreen_action`（none/pause/terminate，作用于壁纸）不受影响
- 验收实测：主窗口打开并静置时壳 + UI 合计 private ≈ **16.8MB**（门槛 ≤40MB）；关闭后回落至 ≈7.6MB；idle CPU ≈0.4%；网格滚动 CPU 10.5%

---

其他验收细节与测试策略见 [质量保障](./06-质量保障-Quality-Assurance.md)。

---

## 时间视图与里程碑

> 原独立的《甘特图》文档已并入本文档。各阶段相对执行顺序：Phase 3（Web 壁纸子进程）→ Phase 4（体验优化）→ Phase 1（前端 UI 补全）→ Phase 2（架构优化与清理）→ Phase 5（打包发布）。甘特图日期为规划期预估，实际开发随迭代演进，当前仅保留相对顺序与达成状态。

| 里程碑 | 对应阶段 | 状态 |
|--------|----------|------|
| M1 前端交互完整可用 | Phase 1 | ✅ 达成 |
| M2 架构整洁、可维护 | Phase 2 | ✅ 达成 |
| M3 Web 壁纸子进程稳定运行 | Phase 3 | ✅ 达成 |
| M4 用户体验完善 | Phase 4 | ✅ 达成 |
| M5 可分发安装包 | Phase 5 | 🔧 等待正式发布 |
| M6 主窗口原生 UI 低内存 | Phase 6 | ✅ 达成 |

**相关章节：** [← 总览](./01-实施规划总览-Implementation-Overview.md) | [项目目录结构](./05-项目目录结构-Project-Structure.md)