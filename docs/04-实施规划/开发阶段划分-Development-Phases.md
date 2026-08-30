[← 返回文档索引](../index.md) > [实施规划](./实施规划总览-Implementation-Overview.md) > [开发阶段划分](./开发阶段划分-Development-Phases.md)

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 开发阶段划分

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

> 本文档按真实代码审计记录各阶段完成情况。已实现的核心能力（四种壁纸类型端到端、桌面集成、渲染优化、全屏检测、音量控制、IPC、进程管理、配置管理、Tauri 应用层、Web 壁纸子进程、前端 UI）不再列入未完成任务。

---

## 阶段总览

| 阶段 | 内容 | 状态 |
|------|------|------|
| Phase 1 | 前端 UI 补全 | ✅ 已完成 |
| Phase 2 | 架构优化与代码清理 | ✅ 已完成 |
| Phase 3 | Web 壁纸子进程 | ✅ 已完成 |
| Phase 4 | 体验优化 | ✅ 已完成 |
| Phase 5 | 打包发布 | 🔧 已配置（NSIS + 资源 + 发布流水线） |

---

### Phase 1: 前端 UI 补全 ✅ 已完成

**目标**：补全前端 UI 控件，接通后端命令。

**当前状态（实测）**：`index.html`（仓库根）+ `src/scripts/main.ts`（731 行）已实现的 UI 控件与后端命令完整打通：

| UI 控件 | 关联后端命令 | 说明 |
|---------|-------------|------|
| 暂停 / 恢复按钮 | `pause_wallpaper` / `resume_wallpaper` | `#pause-btn` `#resume-btn` |
| 音量滑块 | `set_volume` | `#volume-slider`（带 `display_id` 参数） |
| 静音按钮 | `toggle_mute` | `#mute-btn` |
| 播放速度滑块 | `set_speed` | `#speed-slider`（0.25–4.0，mpv） |
| 目标显示器 | `get_displays` | `#display-select` |
| 排列模式 | `update_config` | `#arrangement-select`（per_monitor / span） |
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

- `mirrorstar-wp-proc` 子进程（crate），模块划分：
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
| 资源捆绑 | ✅ 已配置 | `resources`：`mpv/mpv.exe`、`mpv/license.txt`、`target/release/mirrorstar-wp-proc.exe` |
| 图标 / 版本 | ✅ 已配置 | `icons/*`，productName `镜星壁纸`，identifier `com.mirrorstar.wallpaper` |
| 发布流水线 | ✅ 已配置 | `.github/workflows/release.yml`：打 `v*` tag 触发，需签名密钥，构建 NSIS 并起草 Release |
| 正式打 tag 发布 | ⬜ 待执行 | 在发布时机打 `v0.x.x` tag 触发 Release 工作流 |

**验收标准**：

- ✅ CSP 安全策略生效
- ✅ NSIS 安装包可正常安装/卸载，含 mpv.exe 与 mirrorstar-wp-proc.exe
- ✅ WebView2 Runtime 缺失时提示
- ✅ mpv.exe 随包分发
- ✅ 应用图标、版本信息正确
- ⬜ 完成一次正式 tag 发布

---

其他验收细节与测试策略见 [质量保障](./质量保障-Quality-Assurance.md)。

**相关章节：** [← 总览](./实施规划总览-Implementation-Overview.md) | [甘特图](./甘特图-Gantt-Chart.md) | [项目目录结构](./项目目录结构-Project-Structure.md)