# MirrorStar Wallpaper（镜星壁纸）需求文档 — 非功能性需求 / Non-Functional Requirements

[← 返回文档索引](../index.md) > [需求文档](./01-项目概述-Overview.md) > 非功能性需求

> 相关文档：[项目概述](./01-项目概述-Overview.md) | [功能性需求](./02-功能性需求-Functional-Requirements.md) | [用例](./04-用例-Use-Cases.md) | [约束与术语](./05-约束与术语-Constraints-and-Terminology.md)

## 6. 非功能性需求

### 6.1 性能需求

| 需求 ID | 需求名称 | 指标 | 验收标准 |
|---------|----------|------|----------|
| NFR-001 | 暂停态内存占用 | 主进程 < 20MB | 壁纸暂停时，主进程工作集内存不超过 20MB。全屏处置为 `terminate` 时无 mpv 子进程、无 `--wp-proc` 子进程（若配置为 `pause`，mpv 驻留并计入子进程开销，不计入主进程指标）；主窗口 UI 子进程（`mirrorstar-ui`）随窗口隐藏超时（默认 300s）退出，不再常驻；GIF 解码内存预算由配置项 `gif.max_memory_mb` 控制（默认 **40MB**，允许 10–500MB）；平衡策略默认保留 **10 帧**（`DEFAULT_BALANCED_KEEP_FRAMES = 10`）；暂停时按策略处理帧数据：Aggressive 仅保留当前帧、Balanced 保留当前帧附近 10 帧、Performance 保留全部帧、Adaptive 按内存决定。⚠️ 历史基线约 17.14MB（2026-07-24，未覆盖大 GIF 暂停态）；GIF 暂停时 Balanced 默认保留 10 帧、总帧预算上限 40MB，主进程可能超过 20MB，需按壁纸类型复测/后续优化。暂停态实测基线与完整资源占用穷举统一维护在《09-性能优化》§22 资源占用穷举清单 |
| NFR-002 | 活跃态内存占用 | 主进程 < 50MB | 视频壁纸播放时，主进程工作集内存不超过 50MB（历史数据口径：仅计主进程，不含 mpv / WebView2 等子进程；最新实测待复测） |
| NFR-003 | 暂停态 CPU 占用 | < 0.1% | 壁纸暂停时，主进程 CPU 占用 < 0.1%（无后台轮询，仅事件驱动；事件驱动全屏检测 SetWinEventHook + PauseSender 快速通道，无轮询线程） |
| NFR-004 | 活跃态 CPU 占用 | < 5% | 视频壁纸播放时，主进程 CPU 占用不超过 5%（硬件解码 mpv --hwdec=auto） |
| NFR-005 | 冷启动时间 | < 2 秒 | 从双击图标到托盘图标出现的时间不超过 2 秒。启动时**不创建主窗口 UI 子进程**（`mirrorstar-ui` 按需 spawn），仅初始化托盘图标和壁纸引擎；WorkerW 异步初始化（后台线程），不阻塞启动流程。✅ 托盘图标出现时间 < 1 秒（架构总览；Debug 弱环境运行时测试约 2s，Release 待复测） |
| NFR-006 | 热启动时间 | 待复测 | 从托盘打开主窗口的时间。主窗口由壳侧**按需 spawn `mirrorstar-ui` 子进程**承载（而非 show() 恢复既有 WebView 实例），首次打开需进程创建 + Slint 初始化，子进程驻留期间再次打开为已运行实例复用。**当前无实测数据，待复测** |
| NFR-007 | 壁纸切换时间 | < 1 秒 | 从选择壁纸到新壁纸开始显示的时间不超过 1 秒。调度器轮换的单屏换图采用原子交换（`prepare_atomic_swap` → `embed_atomic_into` → `wait_new_ready` → `commit_atomic_swap`），旧壁纸保持显示直至新壁纸就绪；手动“设为壁纸”的三阶段流程（`prepare_for_wallpaper` 先关闭旧壁纸）不属于原子交换，失败时可能留下无壁纸状态。⚠️ 历史值约 3s（子进程启动 + 窗口嵌入开销），**需复测**；2026-06-25 报告记静态 <1s / 视频 ~0.5s，但该测试先于后续多轮优化（v4.1/v5/v6 等），当前真实耗时以最新实测为准 |
| NFR-019 | 暂停响应时间 | < 500ms | 从检测到全屏应用到壁纸完全暂停的时间不超过 500ms。暂停/恢复命令通过 PauseSender 快速通道发送，绕过引擎 Mutex 直接到达渲染线程，响应时间：GIF/图片 < 10ms，视频 < 50ms（含 IPC 往返） |
| NFR-020 | 托盘菜单响应时间 | < 200ms | 从点击托盘图标到菜单显示的时间不超过 200ms |

> 说明：本节只定义**需求指标**（「指标」列）。各项指标的实测/基线数值，以及「壁纸类型 × 运行状态 × 叠加态」下的完整资源占用穷举，统一维护在《09-性能优化》§22 资源占用穷举清单；本节不再重复列数值，避免同一事实重复维护。

### 6.2 性能测试条件

以上性能指标的测试条件如下：

| 条件 | 说明 |
|------|------|
| 测试硬件 | Intel i5-12400 / AMD Ryzen 5 5600，16GB RAM，SSD，独立显卡或集成显卡 |
| 操作系统 | Windows 11 23H2，无其他重度应用运行 |
| 测试素材 | 视频：H.264 1080p 30fps MP4（10分钟循环）；GIF：500x300 15fps；网页：简单 CSS 动画页面 |
| 测量工具 | 任务管理器（工作集内存）、Process Explorer（CPU 占用）、自定义计时器（启动/切换时间） |
| 测量方法 | 每项指标测量 5 次取平均值；CPU/内存指标在壁纸运行 30 秒后采样，持续观察 60 秒 |
| 暂停态定义 | 壁纸已暂停（非首次启动后未设置壁纸的状态），无全屏应用运行。主进程指标口径：`fullscreen_action = terminate` 时无 mpv、无 `--wp-proc` 子进程；若为 `pause`，mpv 驻留但计入子进程开销。主窗口 UI 子进程（`mirrorstar-ui`）随窗口隐藏超时（默认 300s）已退出，无 WebView2 实例 |
| 活跃态定义 | 视频壁纸正在播放，主窗口已关闭（`mirrorstar-ui` 子进程在隐藏超时后退出）；mpv 子进程运行中（不计入主进程指标） |

### 6.3 资源需求

| 需求 ID | 需求名称 | 指标 | 验收标准 |
|---------|----------|------|----------|
| NFR-008 | 二进制体积 | 主进程 < 10MB | Release 编译后的主进程可执行文件不超过 10MB（历史数据口径：仅主进程 exe，不含外部 mpv.exe 约 15-25MB 和系统 WebView2 Runtime 组件）。**当前无最新 Release 体积数据，待复测** |
| NFR-009 | 无运行时依赖 | 无需 .NET/JRE | 不依赖 .NET Runtime、Java Runtime 等大型运行时 |
| NFR-010 | WebView2 依赖 | 系统自带 | WebView2 使用 Windows 10/11 系统自带的 Edge WebView2 Runtime |
| NFR-011 | mpv 依赖 | 外部可执行文件 | mpv 以外部可执行文件（mpv.exe）形式随程序分发，通过命名管道 IPC 通信（非 libmpv 动态链接库） |

### 6.4 可靠性需求

| 需求 ID | 需求名称 | 描述 |
|---------|----------|------|
| NFR-012 | 内存安全 | 利用 Rust 所有权系统，杜绝内存泄漏、悬垂指针、缓冲区溢出等内存安全问题 |
| NFR-013 | 子进程隔离 | 三类子进程以独立进程方式运行：① `mpv.exe`（视频壁纸）；② 壳 exe 的 `--wp-proc` 模式（网页壁纸，同一 exe 的独立子进程，经命名管道 IPC）；③ `mirrorstar-ui.exe`（主窗口 UI，Slint 软件渲染，经 `ui_proto` NDJSON over stdio 通信）。任一类子进程崩溃不导致主进程退出。GIF 与走 WorkerW 路径的静态图片在主进程内渲染（GDI 双缓冲）；原生静态图片无渲染器、无子进程 |
| NFR-014 | 子进程恢复 | **部分实现**。① **视频停滞自愈（已实现）**：`src-tauri/src/platform/liveness_watchdog.rs` 常驻看护每 5s 采样 `time-pos`，经 `wallpaper/video.rs::video_playback_stalled` 判定时钟停滞（含 1.3s 确认采样消除周期混叠、60s 自愈冷却），命中后调用 `WallpaperEngine::heal_stalled_renderer`（固定顺序 `terminate()` → `revive_terminated()`）。② **渲染器终止后恢复（已实现）**：全屏进入/退出经 `pause_all_fast` / `resume_all_fast` 处置，对 `Terminated` 渲染器执行 `revive_terminated` 重建；失败项由后台复查线程（`mirrorstar-fullscreen-resume`，2s 间隔）重试。③ **子进程意外崩溃（部分实现）**：`spawn_proc_exit_monitor` 监听 mpv / wp-proc 退出，异常退出时置状态 `Terminated` 并通知 UI 刷新，但**不自动重启**（需用户重新设置壁纸）。④ **桌面层自愈（已实现）**：Explorer 重启、WorkerW/Progman 销毁、会话解锁、睡眠恢复等由事件驱动 `try_heal_desktop_layer`（销毁类事件 1s 去抖），`src-tauri/src/platform/workerw_check.rs` 每 **300s（5 分钟）** 兜底轮询（`tokio::select!` + `Notify` 支持即时唤醒）。注：停滞自愈**不做**画面级检测，对"时钟前进 + 画面定格"的呈现层冻结不可见（已知限制，见 04-wallpaper模块.md） |
| NFR-015 | 配置容错 | 配置文件损坏时使用默认值启动，不崩溃。其中 `playback.toml`（轮换播放状态，独立于 config 热重载）读取失败、解析失败或版本不符时，回退为默认空状态（`DR-32`），不影响应用启动 |
| NFR-021 | 轮换调度可靠性 | 定时轮换 apply 失败时保持旧壁纸不动、游标推进跳过（不原地卡死重试）；暂停原因非空（`pause_reasons`）时抑制定时轮换；单屏换图采用原子交换（prepare → embed → wait_ready → commit），交换期间全屏处置（`pause_all_fast` / `resume_all_fast` / 桌面层自愈）跳过，由后续事件或 2s 复查线程兜底；轮换间隔下限 1 分钟（60s clamp，防反复 spawn/kill 视频进程），UI 上限 1440 分钟 |

### 6.5 可维护性需求

| 需求 ID | 需求名称 | 描述 |
|---------|----------|------|
| NFR-016 | 模块化架构 | 壁纸渲染器通过 `WallpaperRenderer` trait 统一抽象，4 种渲染器（ImageRenderer/GifRenderer/VideoRenderer/WebRenderer）实现同一 trait（play/pause/resume/set_volume/set_speed/set_position/terminate 等）；音视频专属能力按类型实际实现或使用 trait 默认 no-op（如 GIF 无音量、GIF/Web 不切换鼠标穿透），便于扩展新壁纸类型 |
| NFR-017 | 日志记录 | 记录关键操作和错误日志（tracing），便于问题排查。日志留存策略：应用日志按日轮转并保留 **7** 个历史文件（`APP_LOG_KEEP = 7`）；`mpv-*.log` 按实例剪枝，保留最新 N 个（`MPV_LOG_KEEP = 20`）；`crash.log` 超过 **1MB**（`CRASH_LOG_MAX_BYTES`）轮转为 `crash.log.1`（仅保留一份备份）。所有清理均为 best-effort，失败仅 `warn` 不中断 |
| NFR-018 | 代码文档 | 公共 API 和关键模块有完整的文档注释（rustdoc） |
| NFR-022 | 诊断可观测性 | `get_memory_report` 提供进程树内存归因（主进程 / `mpv` / `wp-proc` / `msedgewebview2` / other）；`report_lifecycle_mark` 在环境变量 `MIRRORSTAR_MEM_TRACE` 开启时记录生命周期埋点；日志包含错误码与关键状态，便于问题定位 |