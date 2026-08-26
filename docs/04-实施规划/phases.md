# MirrorStar Wallpaper（镜星壁纸）实施规划 — 开发阶段划分

[← 返回文档索引](../README.md) > [实施规划](./overview.md) > [开发阶段划分](./phases.md)

## 2. 开发阶段划分

> **说明**：本文档基于真实代码审计结果，仅列出尚未完成或需要改进的工作。已完成的核心功能（四种壁纸类型端到端、桌面集成、渲染优化、全屏检测、音量控制、IPC 通信、进程管理、配置管理、Tauri 应用层、Web 壁纸子进程）不再列入开发阶段。

---

### Phase 1: 前端 UI 补全（当前优先级）

**目标**：补全前端缺失的 UI 控件，接通已实现的后端命令

**背景**：后端 24 个 Tauri 命令已全部完整实现，前端 UI 完成度约 95%（壁纸预览/搜索/响应式/电池暂停/事件监听等已完成），剩余少量 UI（手动暂停/恢复按钮、播放速度、鼠标交互模式、删除源文件、静音按钮）待补全。

| 任务 | 描述 | 预计工时 |
|------|------|----------|
| 1.1 添加手动暂停/恢复壁纸 UI 按钮 | 后端 `pause_wallpaper`/`resume_wallpaper` 命令已实现，HTML 中按钮已存在，需验证 JS 事件绑定与状态切换 | 0.5 天 |
| 1.2 添加播放速度控制 UI（视频壁纸） | 后端 mpv 支持 `set_speed`，前端缺少速度控制滑块/输入框 | 0.5 天 |
| 1.3 添加鼠标穿透/交互模式切换 UI | 后端 `set_interaction_mode`/`toggle_interaction` 命令已实现，前端无切换控件 | 0.5 天 |
| 1.4 实现拖拽视觉反馈（over/leave 事件处理） | CSS 已有 `.drag-over` 样式，但 JS 未实现 dragover/dragleave 事件处理 | 0.5 天 |
| 1.5 补全事件监听（wallpaper-updated, wallpaper-state-changed） | 后端已 emit 4 个事件，前端未监听 `wallpaper-updated` 和 `wallpaper-state-changed`，导致状态不同步 | 0.5 天 |
| 1.6 添加删除确认对话框和删除源文件选项 | 当前 `remove_wallpaper` 的 `deleteFile` 参数硬编码为 false，需添加确认对话框和可选删除源文件 | 0.5 天 |
| 1.7 添加静音按钮 UI | 后端 `toggle_mute` 命令已实现，前端无静音按钮 | 0.5 天 |

**交付物**：前端 UI 与后端命令完全打通，所有已实现功能均可通过界面操作

**验收标准**：
- ✅ 手动暂停/恢复按钮可正常控制壁纸播放
- ✅ 视频壁纸播放速度可调节
- ✅ 鼠标穿透/交互模式可切换
- ✅ 拖拽文件时有视觉反馈
- ✅ 壁纸状态变化时前端自动更新
- ✅ 删除壁纸时有确认提示，可选删除源文件
- ✅ 静音按钮可切换全局静音

---

### Phase 2: 架构优化与代码清理

**目标**：移除死代码，修复已知问题，提升稳定性

| 任务 | 描述 | 预计工时 |
|------|------|----------|
| 2.1 清理未使用的 OnceLock（FULLSCREEN_ENGINE, FULLSCREEN_RT） | `src-tauri/src/lib.rs` 中 2 个 OnceLock 为 write-only 死代码，从未被读取，应移除（已完成） | 0.5 天 |
| 2.2 修复 main.rs 中 tracing 在 logging 初始化前调用的问题 | `main.rs` 中 tracing 日志在 `init_logging()` 之前调用，日志会丢失，改用 `eprintln!`（已完成） | 0.5 天 |
| 2.3 Explorer 重启检测升级（TaskbarCreated 已实现，保留 5min 轮询兜底） | 当前为 5 分钟轮询 + IsWindow 检查；TaskbarCreated 消息监听已实现，保留轮询作为兜底方案 | 0.5 天 |
| 2.4 系统托盘菜单增加"暂停/恢复壁纸"快捷操作（已实现） | 托盘菜单已有打开/退出等项，"暂停/恢复壁纸"快捷操作已实现，验证功能正常 | 0.5 天 |
| 2.5 修复前端 setVolume 缺少 displayId 参数的潜在 bug | 前端 `set_volume` 调用缺少 `displayId` 参数，需检查并修复多显示器场景下的音量控制（✅ 已修复，`src/scripts/ipc.ts` 已统一 `displayId: displayId || null` 转换） | 0.5 天 |

**交付物**：无死代码、无已知缺陷的干净代码库

**验收标准**：
- ✅ 无未使用的 OnceLock 死代码
- ✅ main.rs 启动日志正常输出
- ✅ Explorer 重启后壁纸自动恢复（事件驱动 + 轮询兜底）
- ✅ 托盘菜单暂停/恢复功能正常
- ✅ 多显示器音量控制正常工作

---

### Phase 3: Web 壁纸子进程实现 ✅ 已完成

**目标**：将 Web 壁纸渲染隔离到独立子进程，实现崩溃隔离 + 内存优化

**状态**：✅ 已完成

**已完成内容**：
- `mirrorstar-wp-proc` 子进程（3144 行）：WebView2 环境 + 窗口创建 + 命名管道 IPC 服务端 + 消息循环 + 命令处理
- `WpProcIpcClient`（`mirrorstar-core/src/ipc/wp_proc.rs`）：JSON + 换行分隔协议，复用 MpvIpcClient 的连接重试 + request_id 模式
- `WebRenderer` 代理层重构（255 行）：通过 ProcessManager 启动子进程 + WpProcIpcClient 通信
- HWND 通过 FindWindowW + PID 验证获取，主进程执行 WorkerW 嵌入
- 子进程崩溃检测：IPC 管道断开检测
- `mirrorstar-watchdog` crate 已移除（不再需要独立看门狗进程）

---

### Phase 4: 体验优化 ✅ 已完成

**目标**：完善用户体验

**状态**：✅ 已完成

**已完成内容**：
- ✅ 壁纸预览（点击放大查看）
- ✅ 壁纸搜索功能
- ✅ 响应式设计（媒体查询适配窄屏）
- ✅ 关于页面完善（动态版本号 + 技术栈）
- ✅ 加载动画（骨架屏 + 缩略图渐显）
- ✅ 电池供电暂停（后端 GetSystemPowerStatus + 前端 UI）

---

### Phase 5: 打包发布

**目标**：准备正式发布，制作安装包

| 任务 | 描述 | 预计工时 |
|------|------|----------|
| 5.1 启用 CSP 安全策略 | 配置 Tauri Content Security Policy，限制前端资源加载来源 | 0.5 天 |
| 5.2 配置 Tauri 打包选项（MSI/NSIS），包含 mirrorstar-wp-proc.exe | 配置 `tauri.conf.json` 打包选项，将 `mirrorstar-wp-proc.exe` 子进程二进制包含在安装包中 | 1 天 |
| 5.3 应用图标、版本信息、安装路径配置 | 配置应用图标、版本号、安装目录等元信息 | 0.5 天 |
| 5.4 验证 WebView2 Runtime 依赖 | Web 壁纸子进程依赖 WebView2 Runtime，打包时检测或提示用户安装 | 0.5 天 |
| 5.5 捆绑 mpv.exe 或提供下载提示 | 视频壁纸依赖外部 mpv.exe，需随安装包分发或首次运行时提示下载 | 1 天 |

**交付物**：可正式发布的安装包

**验收标准**：
- ✅ CSP 安全策略生效
- ✅ MSI/NSIS 安装包可正常安装和卸载
- ✅ 安装包含 mirrorstar-wp-proc.exe 子进程
- ✅ WebView2 Runtime 缺失时提示用户
- ✅ mpv.exe 可用（随包分发或下载提示）
- ✅ 应用图标、版本信息显示正确

各阶段的详细任务划分参见本文档，交付物与验收标准详情参见 [质量保障](./quality.md)。
