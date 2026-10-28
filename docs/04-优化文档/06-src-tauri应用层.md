# src-tauri 应用层模块优化文档

> [← 返回文档索引](../index.md)

> **文档说明**：本文由 v4.0 模块审查文档（本文件旧版）与 v6.0 技术债审查（src-tauri 应用层）合并而成。
> - **v4.0 findings**：17 项（ST-001~ST-017），已逐项对照代码核验，全部修复或已评估接受。
> - **v6.0 技术债**：24 项（ST-TD-001~ST-TD-024），已按 `cleanup-v6-src-tauri-tech-debt-2026-07-26` spec 全部清理（7 P0 + 12 P1/P2 + 5 P3 保留现状）。
> - 审查日期：2026-07-25 | 模块路径：`src-tauri/src/` | 源文件数：12
> - **2026-10-11 深度审查复核**：实际源文件数已增至 **20**（约 14,115 行），本文件 §1~§5 的历史数字多为失真，详见新增 **§6 深度审查**。

> **注（主窗口原生 UI 重构后，2026-10-02）**：本文涉及的主窗口 WebView2 相关实现（`WebviewWindowBuilder`、`MAIN_WINDOW_BROWSER_ARGS`、`webview_memory` 挂起/裁剪、CSP、`wpfile://`）已随主窗口重构**移除**；壳不再创建 WebView2 主窗口，主窗口改由 `mirrorstar-ui.exe`（Slint）承载。本文保留为历史记录。

## 1. 模块概览 / 现状 与 文件清单

### 1.1 模块职责

`src-tauri` 应用层是镜星壁纸的 Windows 客户端壳，承担五项核心职责：

1. **Tauri 命令层**：通过 `#[tauri::command]` 把 36 个前端可调用命令（壁纸生命周期 / 配置管理 / 系统控制 / 轮播）封装为对 `mirrorstar-core`（`ConfigManager` / `WallpaperEngine` / `DesktopIntegrator`）的薄封装。（原文记 24 个为历史值，2026-10-11 复核为 36。）
2. **Win32 平台集成**：启动四类后台监控线程——全屏检测（`SetWinEventHook`）、Explorer 重启监控（`TaskbarCreated` 消息窗口）、WorkerW 兜底检查（5 分钟间隔）、视频卡帧存活看护（`liveness_watchdog`）；处理 `WM_POWERBROADCAST` 电源事件。
3. **进程生命周期管理**：`run()` 顺序初始化 COM（STA）→ DesktopIntegrator → WorkerW 预初始化线程 → VolumeControl → WallpaperEngine → ConfigManager → 监控线程 → Tauri Builder；另含新增的**轮播调度器（`scheduler.rs`）**与 **UI 子进程托管（`ui_process/`，管理 `mirrorstar-ui.exe`）**。`perform_shutdown_blocking` 按 LIFO 顺序 + 幂等守卫 + 多个超时上界统一清理。
4. **前端事件桥接**：通过 `app.emit(...)` 把后端状态变更（壁纸增删改 / 状态切换 / 桌面状态 / 配置变更 / 缩略图进度）通知前端；订阅 `WallpaperEngine::global_state_changed` broadcast 通道统一转发 `wallpaper-state-changed`。
5. ~~**自定义协议**：注册 `wpfile` URI scheme handler，支持 HTTP Range / HEAD 请求~~ —— **已消解**：`wpfile` / `register_uri_scheme` 在代码中已零命中，随 2026-10-02 原生主窗口重构整体移除。

该模块是 v3.x → v4.0 → v4.1 → v5.0 → v6 多轮修复的重灾区，积累了大量历史环评标记（`Bug #N` / `ST-NNN` / `v41-ST-NNN` / `v5.0 X-PERF-NNN`），是 v6 技术债审查的重点模块。

### 1.2 文件清单

> **2026-10-11 复核**：以下文件清单为**核实后的当前状态**（20 个源文件、约 14,115 行）；原文档仅记 12 文件约 5,630 行，已失真。

#### commands/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 123 | 命令模块导出（`pub use config::*; pub use rotation::*; pub use system::*; pub use wallpaper::*;`，4 个子模块） |
| config.rs | 252 | 配置命令（`get_config` / `update_config` / `get_arrangement` / `update_arrangement`）+ `validate_config_fields` 字段范围校验（委托 `mirrorstar-core::config::validation`，ST-TD-005） |
| rotation.rs | 260 | 轮播命令 12 个（`get_rotation_config` / `update_rotation_config` / `list_pools` / `create_pool` / `update_pool` / `delete_pool` / `reorder_pools` / `get_unit_states` / `set_active_pool` / `set_rotation_enabled` / `next_wallpaper` / `prev_wallpaper`）**（原文档缺失）** |
| system.rs | 277 | 系统级命令 8 个（`get_displays` / `get_memory_report` / `report_lifecycle_mark` / `check_desktop_status` / `set_interaction_mode` / `open_file_dialog` / `toggle_auto_start` / `get_auto_start_status`；原文档所列 `toggle_interaction` / `quit_app` **已不存在**）+ ST-002 回调式文件对话框 |
| wallpaper.rs | 2351 | 壁纸生命周期命令 12 个 + `ComGuard` RAII + `DisplaySettingGuard` 防竞态 + `validate_wallpaper_file_path` / `validate_volume` / `validate_speed` / `parse_scaling_mode` 纯函数 + 36 个单元测试 |

#### platform/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 11 | 平台模块导出（3 个 `pub(crate) use` + `pub(crate) mod liveness_watchdog / pure_mode / pure_mode_input`）|
| explorer.rs | 约 490 | `start_explorer_restart_monitor`：隐藏顶层窗口（`WS_EX_TOOLWINDOW \| WS_POPUP`，刻意不用 `HWND_MESSAGE`）+ `explorer_monitor_wndproc`（TaskbarCreated / WorkerW·Progman 销毁 / WM_WTSSESSION_CHANGE / WM_POWERBROADCAST / WM_DESTROY），统一转 `try_heal_desktop_layer` |
| fullscreen.rs | 1801 | `start_fullscreen_monitor`：`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + `foreground_event_callback` + 纯函数 + 56 个纯函数单元测试 |
| liveness_watchdog.rs | 490 | 视频卡帧常驻看护（`video_playback_stalled` / 采样确认 / 自愈），随 runtime 结束**（原文档缺失）** |
| power.rs | 246 | `handle_power_status_change`：`GetSystemPowerStatus` + `interpret_ac_line_status` + 电池↔AC 切换暂停/恢复 |
| pure_mode.rs | 878 | 纯净壁纸模式编排（进入/退出/自愈）**（原文档缺失）** |
| pure_mode_input.rs | 1112 | 纯净模式低级键鼠钩子 + 隐藏顶层窗口（刻意不用 `HWND_MESSAGE`）**（原文档缺失）** |
| workerw_check.rs | 245 | `start_workerw_check`：5 分钟 interval + `WORKERW_CHECK_NOTIFY` 即时唤醒 |

#### 顶层
| 文件 | 行数 | 主要内容 |
|---|---|---|
| lib.rs | 1292 | `run()` 入口：日志 + COM STA + WorkerW 预初始化 + VolumeControl + Engine + Config + Scheduler + UI 子进程 + 监控线程 + Tauri Builder + setup 闭包 |
| main.rs | 78 | `main()` + `--wp-proc` 双入口分派 + `ensure_single_instance()`（`CreateMutexW` 单实例互斥体） |
| scheduler.rs | 2114 | 轮播调度器：主循环 / 暂停抑制 / 原子交换 / 显示器热插拔 / 排列变更即时重建**（原文档缺失）** |
| state.rs | 1165 | `AppState` + 21 个全局静态量 + `try_pause_all_fast` / `try_resume_all_fast` + `signal_thread_exit` / `join_monitor_thread` + `try_lock_with_timeout<T>` + `perform_shutdown_blocking` |

#### ui_process/（原文档缺失）
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 629 | `UiProcessManager`（托管 `mirrorstar-ui.exe`）+ `UiJobGuard`（Job Object）+ 空闲退出 + 单实例槽位 |
| bridge.rs | 435 | 与 UI 子进程的事件转发 / 请求分派协议桥（复用 `mirrorstar_core::ui_proto::framing`） |
| paths.rs | 67 | `mirrorstar-ui.exe` 路径解析（同目录命中 / PATH 兜底） |

> **合计**：20 个源文件、约 **14,115** 行。

### 1.3 测试覆盖

- **单元测试**（**2026-10-11 复核**：全 `src-tauri/src/` 实测 `#[test]` 178 + `#[tokio::test]` 18 = **约 196 个**，原文档记 49 个已失真）：`commands/wallpaper.rs`（36）、`platform/fullscreen.rs`（56）、`scheduler.rs`（34）、`platform/pure_mode_input.rs`（12）、`state.rs`（10，含 2 个 `#[ignore]`）、`ui_process/bridge.rs`（10）、`liveness_watchdog.rs`（7）、`ui_process/{mod,paths}.rs`（6）、`platform/pure_mode.rs`（5）、`lib.rs`（5）、`commands/{config,rotation}.rs`（4）、`commands/system.rs`（4）、`platform/power.rs`（4）、`platform/workerw_check.rs`（3）。
- **集成测试**（**2026-10-11 复核**）：`tests/config_flow.rs`（6 个 ConfigManager 增删改流程测试，纯逻辑无 `#[ignore]`）、`tests/wallpaper_flow.rs`（**35 个**：33 `#[test]` + 2 `#[tokio::test]`，其中 **29 个 `#[ignore]`** 需 Windows COM/音频环境，**6 个**纯逻辑测试 CI 可执行）、`tests/common/mod.rs`（MockRenderer + 测试辅助函数，207 行）。原文档记「约 25 个 / 仅 4 个 CI 可执行」已失真。
- **测试盲区**：`mirrorstar-ui.exe` 子进程托管端到端 / Tauri Builder setup 闭包 / Win32 回调路径（`foreground_event_callback` 等）无法单元测试，依赖手动端到端验证。

### 1.4 核心结构与设计模式（v4 概要浓缩）

- **核心结构**：`AppState`（`Arc<ConfigManager>` + `Arc<tokio::sync::Mutex<WallpaperEngine>>` + `Arc<Mutex<DesktopIntegrator>>` + `tray_paused: AtomicBool` + `tray_pause_resume_item: OnceLock<MenuItem>`）。
- **21 个全局可变静态量**（state.rs，**2026-10-11 复核**：原文档记 19 个，新增 `SHARED_APP_HANDLE`(187) / `SHARED_SCHEDULER`(198)）：`SHARED_ENGINE`/`SHARED_CONFIG`/`SHARED_APP_HANDLE`/`SHARED_SCHEDULER`/`FULLSCREEN_WAS`/`EXPLORER_DESKTOP`/`TASKBAR_CREATED_MSG`/`POWER_WAS_ON_BATTERY`/`WIN_EVENT_HOOK`/`FULLSCREEN_MONITOR_RUNNING`/`FULLSCREEN_MONITOR_THREAD_ID`/`FULLSCREEN_MONITOR_THREAD`/`EXPLORER_MONITOR_RUNNING`/`EXPLORER_MONITOR_THREAD_ID`/`EXPLORER_MONITOR_THREAD`/`WORKERW_CHECK_RUNNING`/`WORKERW_CHECK_TASK`/`WORKERW_CHECK_NOTIFY`/`WORKERW_INIT_THREAD`/`THUMBNAIL_TASK`/`SHUTDOWN_DONE`。
- **4 类系统监控线程**（**2026-10-11 复核**：原文档记 3 类，补 `liveness_watchdog`）：`fullscreen_monitor`（`SetWinEventHook` 事件驱动）、`power_monitor`（`WM_POWERBROADCAST` 事件驱动）、`explorer_restart_monitor`（`TaskbarCreated` 消息事件驱动）、`liveness_watchdog`（视频卡帧常驻看护，5s 周期）。
- **事件发射**：`wallpaper-added`/`wallpaper-updated`/`wallpaper-removed`/`wallpaper-state-changed`/`wallpaper-thumbnail-failed`/`desktop-status-changed`/`wallpaper-regenerate-progress`。
- **设计模式**：ComGuard RAII（T16）｜shutdown 流程幂等守卫 + LIFO + 3s 引擎锁超时（Bug #6/T04/ST-002/ST-003/ST-014）｜锁顺序约定（engine→desktop，顶部文档化）｜输入验证（T08/T12/SEC-001/SEC-002）｜per-display 防竞态（`DisplaySettingGuard`）｜Win32 回调安全（unsafe Send/Sync 论证、try_lock 非阻塞）。

## 2. v4.0 审查发现与修复状态（17 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/07-src-tauri.md`
> 严重级别分布：Critical 0 / High 2 / Medium 5 / Low 10
> 维度分布：逻辑 1 | 并发 4 | 资源 4 | 错误 2 | 性能 2 | 安全 3 | 可维护性 3
>
> **2026-07-26 代码核验结论**：v4 findings 共 17 项，已 **17/17 全部关闭**——11 项实际代码修复（✅ 修复），6 项为已评估并接受的文档化权衡（✅ 接受）。其中 ST-001 / ST-002 两处此前位列"v4.0 优先修复 TODO"，经代码核验现已修复。

### [ST-001] [High] [逻辑/安全] `add_wallpaper` 使用字符串前缀比较判断受控目录，兄弟目录路径误判

**描述**：原实现用 `String::starts_with` 判断文件是否已在 `$APPDATA/mirrorstar/` 下。当 data_dir 为 `...\mirrorstar` 时，兄弟目录（`...\mirrorstar-evil\`、`...\mirrorstar_backup\`）下的文件会通过字节前缀校验被误判为"已在受控目录"，跳过复制，导致配置中的 `file_path` 指向外部路径并绕过 asset scope。

**影响**：数据完整性（外部文件移动后壁纸失效、`remove_wallpaper(delete_file=true)` 误删外部文件）+ asset scope 失效（`convertFileSrc` 预览异常）。

> ✅ **已修复**（代码核验：wallpaper.rs:460-464 `is_path_within_data_dir` 使用 `std::path::Path::starts_with` 按路径组件匹配；`add_wallpaper`/`remove_wallpaper`/`set_wallpaper` 均经 `validate_path_within_data_dir` 校验）。

### [ST-002] [High] [资源管理] `open_file_dialog` 超时后 `spawn_blocking` 线程不可取消，阻塞线程泄漏

**描述**：原实现 `spawn_blocking` + 10 分钟超时，超时后 drop JoinHandle 不中止阻塞任务，tokio 阻塞线程池线程被永久占用直至用户关闭模态对话框。

**影响**：每次超时泄漏 1 个阻塞线程池线程 + 超时后对话框仍显示 UX 不一致。

> ✅ **已修复**（代码核验：system.rs:85-113 改用 `tauri-plugin-dialog` v2 回调式 `pick_file()` + `tokio::sync::oneshot` 通道，不再占用 tokio 阻塞线程池；超时 drop receiver，对话框关闭时 send 返回 Err；超时上限 `FILE_DIALOG_TIMEOUT = 300s`（5 分钟）；含 4 个 ST-002 单元测试）。

### [ST-003] [Medium] [并发] `workerw_check` 异步任务中 `std::sync::Mutex::lock()` 阻塞 tokio worker 线程

**描述**：`start_workerw_check` 异步任务中 `desktop.lock()` + 同步 `check_and_reinitialize()`（含 `EnumWindows`，极端 <50ms）。

**影响**：单次最长数百 ms 阻塞 tokio worker；违反 tokio"async 任务不阻塞"原则。

> ✅ **已接受（文档化）**（代码核验：workerw_check.rs:109-120 注释论证低频（5 分钟）+ 短阻塞 + 无 await 持有锁，可接受）。

### [ST-004] [Medium] [错误处理] `DesktopIntegration` 错误变体被泛化用作多种非桌面集成错误容器

**描述**：文件类型不支持 / 文件名提取失败 / 音量 / 速度 / 缩放模式越界 / 自启动失败等多类错误被归为 `DesktopIntegration`，前端无法按 error code 区分"用户输入错误"与"系统故障"。

**影响**：前端错误处理精度降低、日志语义混杂。

> ✅ **已修复**（代码核验：`validate_volume`/`validate_speed`/`parse_scaling_mode`/文件类型判断等改用 `InvalidArgument` 变体，wallpaper.rs:545,1149 等；`config.rs::validate_config_fields` 委托共享校验并将 `InvalidArgument` 映射为 `InvalidConfig`；st004 系列测试断言 `InvalidArgument`）。

### [ST-005] [Medium] [并发/资源] `set_speed` fire-and-forget 任务未被 shutdown 跟踪

**描述**：`set_speed` 采用 fire-and-forget spawn，JoinHandle 未被保存，退出时可能在 engine shutdown 后访问 engine（use-after-shutdown 语义风险）。

**影响**：退出期间锁竞争延迟 3s 超时获取；未来实现变化可能触发 UB。

> ✅ **已修复**（代码核验：wallpaper.rs:1104-1118 `set_speed` 改为命令层持锁同步执行，与 `set_volume` 一致，移除 fire-and-forget；st005 测试断言函数体不含 `tauri::async_runtime::spawn`）。选项①。

### [ST-006] [Medium] [资源管理] shutdown 中缩略图任务可能在 `flush()` 之后完成写入，导致缩略图路径丢失

**描述**：缩略图任务等待（5s 超时）在 `config_manager.flush()` 之前，超时未完成的任务继续后台执行，可能在 flush 之后写入缩略图路径，依赖缓冲刷盘时数据丢失。

**影响**：大视频缩略图路径退出时丢失，下次启动空白预览。

> ✅ **已修复**（代码核验：state.rs:748-752 `perform_shutdown_blocking` 在原 `flush()` 之后追加一次额外 `flush()` 兜底，日志"配置二次刷写失败（ST-006 兜底）"；st006 测试断言 flush 次数 ≥ 2）。选项①。

### [ST-007] [Medium] [安全] 符号链接检测未覆盖 Windows Junction Points（目录联接）

**描述**：T08 逐级 `symlink_metadata` 检测在 Windows 上可能不将 junction（`IO_REPARSE_TAG_MOUNT_POINT`）识别为 symlink，存在绕过路径校验的凹陷。

**影响**：攻击者可通过 junction 将受控目录内子目录指向外部；深度防御缺口而非直接漏洞。

> ✅ **已修复**（代码核验：wallpaper.rs:427-437 `validate_wallpaper_file_path` 追加 `tokio::fs::canonicalize` 补充校验，canonicalize 失败时拒绝访问；ST-007 junction 检测注释 + st007 测试）。选项②。

### [ST-008] [Low] [性能] `set_scaling_mode` 在持有 engine 锁后才解析缩放模式，解析失败时锁获取浪费

**描述**：原实现先获取 engine 锁再 `parse_scaling_mode`，非法输入时无意义占锁。

> ✅ **已消除（实现演进）**：代码核验发现 `set_scaling_mode` 的命令签名已改为直接接收 `ScalingMode` 枚举参数（wallpaper.rs:1083），字符串解析在 Tauri serde 反序列化层已完成，函数内不再存在 `parse_scaling_mode` 调用——ST-008 的"锁内解析"关注点已被前置彻底消除。

### [ST-009] [Low] [性能] `regenerate_thumbnails` 所有缩略图生成在单个 `spawn_blocking` 任务中顺序执行

**描述**：N 个壁纸顺序生成，总耗时 = N × 单个时间，命令长时间不返回。

> ✅ **已修复（采纳方案②）**：代码核验 wallpaper.rs:1167-1188 每完成一项 emit `wallpaper-regenerate-progress` 事件，payload 为 `RegenerateProgressPayload`（`processed/success/failed/total`），空列表不 emit，emit 失败 `tracing::warn!` 不阻塞循环。

### [ST-010] [Low] [可维护性] shutdown 超时魔法数（5s/3s）未提取为命名常量

**描述**：`perform_shutdown_blocking` 中 `Duration::from_secs(5)` / `from_secs(3)` 内联。

> ✅ **已修复**：代码核验 state.rs:361-362 提取 `SHUTDOWN_THUMBNAIL_TIMEOUT`（5s）/ `SHUTDOWN_ENGINE_LOCK_TIMEOUT`（3s）模块级常量，附语义注释。

### [ST-011] [Low] [资源管理] 单实例互斥体句柄未调用 `CloseHandle`

**描述**：`ensure_single_instance` 中 `CreateMutexW` 返回的 HANDLE 依赖进程退出回收。

**影响**：单实例互斥体需进程生命周期存活，"不关闭"是正确行为，非缺陷。

> ✅ **已接受（无需修改）**：代码核验 main.rs:27-32 已保留 ST-011 注释说明 windows-rs 0.58 HANDLE 无 Drop、进程退出自动回收、互斥体需存活至进程结束。

### [ST-012] [Low] [并发] 状态变更订阅任务依赖 tokio runtime 关闭退出

**描述**：setup 中状态变更订阅任务持有 `engine` Arc 克隆形成自引用，退出依赖 runtime 强制 abort 而非通道关闭。

> ✅ **已接受（文档化）**：代码核验 state.rs:660 在 `WORKERW_CHECK_TASK` abort 块上方追加 ST-012 注释段记录自引用 + runtime abort 依赖 + 已评估设计权衡。

### [ST-013] [Low] [可维护性] `check_desktop_status` 标注 `async` 但体内无 `.await`

**描述**：async 标注带来轻微调度开销，签名与实现风格不一致。

> ✅ **已接受（文档化）**：代码核验 system.rs:23-28 保留 async + ST-013 注释段说明保留前瞻性（未来可接 tokio Mutex）。

### [ST-014] [Low] [可维护性] explorer.rs 文件首行包含 UTF-8 BOM 字符

> ✅ **已修复（实测无 BOM）**：代码核验 explorer.rs 首字节为 `use mirrorstar_core...`，UTF-8 BOM 已不存在（前序 Wave 由编辑器自动移除）。

### [ST-015] [Low] [安全] `validate_wallpaper_file_path` 存在 TOCTOU 竞态

**描述**：校验与使用之间文件可被替换为符号链接/恶意文件。

**影响**：利用难度高，T08 + SEC-001 多层防御已缩小攻击面。

> ✅ **已接受（文档化）**：代码核验 wallpaper.rs:353 追加 ST-015 TOCTOU 风险评估段：残留竞态 + 多层防御 + 风险可接受（路径来自用户主动选择）+ 不引入平台特定 API。

### [ST-016] [Low] [并发] `THUMBNAIL_TASK` Vec 仅在 push 前 retain，大量并发长任务时 Vec 无上限增长

**描述**：Vec 中 JoinHandle 持续增长，内存占用（实际影响极小）+ shutdown 5s 超时难等。

> ✅ **已修复**：代码核验 wallpaper.rs:11 `const MAX_THUMBNAIL_TASKS: usize = 50;`，add_wallpaper push 后追加容量上限处理（优先移除最旧已完成 handle，全部进行中不强制截断 + `tracing::warn!`），wallpaper.rs:750-752。

### [ST-017] [Low] [错误处理] `update_config` 对 engine 使用 `try_lock` 更新 GIF 策略，锁忙时跳过

**描述**：`set_gif_memory_strategy` 已改为 `&self + 内部 Mutex`，理论上无需 engine 锁。

> ✅ **已接受（文档化）**：代码核验 config.rs:104-150 ST-017 注释段说明保留 try_lock 路径避免重构 engine 公共 API、锁忙跳过可接受（配置已写入，下次创建壁纸应用）。

### v3.x / v1.0~v2.1 已修复问题（背景记录）

#### v3.5 已修复 findings（T01-T16，16 项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| T01 | Medium | `set_speed` 命令持锁期间 `.await` 造成串行化 | ✅ 已修复（现为命令层同步执行，见 ST-005） |
| T02 | High | `fullscreen.rs` `GetMessageW(-1)` 误判 | ✅ 已修复（loop+mach ret） |
| T03 | Medium | `pause/resume` 与其他命令回退行为不一致 | ✅ 已修复（统一 `resolve_display_id`） |
| T04 | Medium | `blocking_lock()` 无超时 | ✅ 已修复（`try_lock_with_timeout(engine, 3s)`） |
| T05 | Low | `THUMBNAIL_TASK` 覆盖旧 handle | ✅ 已修复（`Mutex<Vec<JoinHandle>>` + 容量上限，见 ST-016） |
| T06 | Low | 托盘菜单文本更新频繁 spawn 线程 | ✅ 已修复（改 `spawn_blocking`） |
| T07 | Low | 重初始化后不 emit 事件 | ✅ 已修复（emit `desktop-status-changed`） |
| T08 | Low | 符号链接可绕过路径校验 | ✅ 已修复（追加 canonicalize 二次校验，见 ST-007） |
| T09 | Low | `update_config` 持 engine 锁阻塞 | ✅ 已修复（`&self + 内部 Mutex`，保留 try_lock 路径，见 ST-017） |
| T10 | Low | 标题/类名缓冲区截断比较 | ✅ 已修复（`TITLE_BUF_LEN` 常量） |
| T11 | Low | `EXPLORER_DESKTOP.set` Err 被丢弃 | ✅ 已修复（match + debug 日志） |
| T12 | Low | 回调 panic 致热重载失效 | ✅ 已修复（`catch_unwind`） |
| T13 | Low | 电源状态获取失败静默 | ✅ 已修复（warn 日志） |
| T14 | Low | `was_reinitialized` 语义错误 | ✅ 已修复（返回实际重初始化 bool） |
| T15 | Low | 阶段 2 不持锁致 renderer 泄漏 | ✅ 已修复（`DisplaySettingGuard` per-display RAII） |
| T16 | Low | 主线程 COM 无 RAII guard | ✅ 已修复（主线程 `ComGuard`） |

#### v3.2 已修复 findings（ST-001~ST-018，14 修复 + 4 TODO）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| ST-001 | P3 | `power.rs` SHARED_ENGINE 未设置时仍更新 | ✅ 已修复 |
| ST-002 | P3 | WorkerW 预初始化线程未保存 JoinHandle | ✅ 已修复 |
| ST-003 | P3 | `workerw_check.rs` 未保存 JoinHandle | ✅ 已修复 |
| ST-004 | P3 | fullscreen/power pause-resume DRY 违规 | ✅ 已修复（state.rs 抽取 `try_pause_all_fast` 等） |
| ST-005 | P3 | shutdown 中 PostThreadMessageW/join 逻辑重复 | ✅ 已修复（state.rs 抽取 `signal_thread_exit`/`join_monitor_thread`） |
| ST-006 | P3 | CreateMutexW 失败仍返回 true | ✅ 已修复（main.rs 处理 GetLastError） |
| ST-007 | P3 | 27 个 `#[ignore]` 测试 CI 不执行 | ✅ 已修复 |
| ST-008 | P3 | 测试辅助用 `ConfigManager::new()` | ✅ 已修复 |
| ST-009 | P3 | 路径校验错误类型不一致 | ✅ 已修复（v4.0 ST-004 泛化到 `InvalidArgument`） |
| ST-010 | P3 | assetProtocol.scope 含 `$HOME/**/*` | ✅ 已修复 |
| ST-011 | P4 | `Box::leak` 不必要 | ✅ 已修复 |
| ST-012 | P4 | 全屏标题匹配不精确 | ✅ 已修复 |
| ST-013 | P4 | `GetModuleHandleW` 静默吞错 | ✅ 已修复 |
| ST-014 | P4 | 缩略图生成 fire-and-forget | ✅ 已解决（v4.0 ST-006/ST-016） |
| ST-015 | P4 | `blocking_pick_file` 可能永久阻塞 | ✅ 已解决（v4.0 ST-002 升级为 High 后修复） |
| ST-016 | P4 | CSP 允许 `'unsafe-inline'` style-src | ⚠️ TODO（前端模块） |
| ST-017 | P4 | 部分测试仅模拟命令层 | ⚠️ TODO（ST-007 缓解） |
| ST-018 | P4 | WM_DESTROY PostQuitMessage 注释缺失 | ✅ 已修复 |

#### v1.0~v2.1 已修复问题（9 项）

| 问题 | 状态 |
|------|------|
| 冗余 WorkerW 监控（30s 轮询→5 分钟兜底） | ✅ 已修复 |
| 10 个全局可变静态量 | ✅ 已修复（v3.2 演化为 19 个） |
| 电源监控使用轮询→`WM_POWERBROADCAST` | ✅ 已修复 |
| COM 初始化错误处理脆弱 | ✅ 已修复 |
| 窗口关闭 50ms 魔数竞态 | ✅ 已修复（`MAIN_WINDOW_CLOSING` 标志位） |
| UUID 截断碰撞风险 | ✅ 已修复（完整 36 字符） |
| async 函数中同步 IO | ✅ 已修复（`tokio::fs::metadata`） |
| shutdown 错误被忽略 | ✅ 已修复（`shutdown()` 返回 `()`） |
| `unwrap` 可能 panic | ✅ 已修复（`new_disabled()` 优雅降级，v3.2） |

## 3. v6.0 技术债清单及清理状态（24 项）

> v6 审查（2026-07-25）在 v4.0（正确性）与 v5.0（性能）之外新增发现 24 项技术债（ST-TD-001~ST-TD-024）。
> 已按 `cleanup-v6-src-tauri-tech-debt-2026-07-26` spec 于 2026-07-26 全部清理：P0 7/7、P1/P2 12/12、P3 5/5（保留现状）。

### 3.1 技术债清单（保留完整记录）

**死代码（2）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-001 | wallpaper.rs:485-490, 525-527 | `add_wallpaper` 内层 Web 分支不可达（外层 `matches!` 已过滤），与 `regenerate_thumbnails` 的可达 Web 分支不对称 | 删除或 `unreachable!` | ✅ 已清理：改 `unreachable!("Web 已在外层 matches! 过滤")` |
| ST-TD-002 | tests/common/mod.rs:10,66-80 | `MockRenderer` 的 `was_played`/`was_terminated`/`speed`/`scaling_mode` 4 方法 + 对应字段无调用点，`#![allow(dead_code)]` 抑制警告 | 删除未用方法与字段 | ✅ 已清理：删除 4 方法及字段，`#![allow(dead_code)]` 保留+补注释 |

**冗余抽象（2）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-003 | wallpaper.rs:245-330 | `validate_wallpaper_file_path`（丢弃 metadata 的"向后兼容包装"）与 `validate_and_get_metadata` 并存 | 保留，doc 措辞改为"便捷封装" | ✅ P3 保留+调整 doc 措辞 |
| ST-TD-004 | state.rs:493-498 | `try_lock_with_timeout<T>` 泛型仅一处实例化，为测试用泛型 | 保留（testability-driven generality） | ✅ P3 保留 |

**重复实现（1）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-005 | config.rs:56-82 vs wallpaper.rs:908-927 | 音量/速度范围校验（范围、is_finite、错误文案一致但分别返回 `InvalidConfig`/`InvalidArgument`）重复两处 | 提取 volume/speed `validate()` 共享函数 | ✅ 已清理：提取 `mirrorstar-core::config::validation::validate_volume` / `validate_speed`，两处委托、错误变体由调用方包装 |

**过时模式（1）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-006 | system.rs:23-28,57-69 | `check_desktop_status` async 无 await（ST-013 已记录"未来"至今未发生），签名与实现风格不一致 | 改 sync 或受文档化权衡保留 | ✅ P3 保留（已文档化的前瞻性权衡） |

**未使用导入（1）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-007 | lib.rs:17-22 | 4 个测试专用函数（`parse_scaling_mode`/`validate_speed`/`validate_volume`/`resolve_display_id`）`pub use` 污染 crate 公共 API 表面 | `#[doc(hidden)]` | ✅ 已清理：4 处 `pub use` 加 `#[doc(hidden)]`（lib.rs:18,22） |

**过度设计（2）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-008 | config.rs:71-79 | `balanced_keep_frames > i32::MAX` 针对"未来类型变更"的永不可触发校验 | 删除或改 `debug_assert!` | ✅ P3 保留；注释联动改为业务上限说明 |
| ST-TD-009 | state.rs:126-183 | 15 个全局静态量保守策略（B 类 8 个可收敛进 MonitorRegistry 但因收益低风险高保留） | 保留，记录重评估条件 | ✅ P3 保留（已评估并接受的权衡） |

**修复痕迹（8，重点）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-010 | lib/state/system/fullscreen/wallpaper.rs | `Bug #N`（#1/#2/#4/#5/#6/#7，约 18 处）标记遍布 5 文件，无 CHANGELOG 索引 | 精简/附录A 索引化 | ✅ 已清理：18 处注释精简，核心说明保留、编号前缀移除 |
| ST-TD-011 | 8 个文件 | `v41-ST-NNN`（14 个标记，约 20+ 处）文档化注释累计约 300+ 行 | 区分修复痕迹与长期设计文档 | ✅ 已清理：纯修复痕迹移除前缀保留说明，长期设计文档移除前缀作独立叙述 |
| ST-TD-012 | wallpaper.rs, config.rs | `v5.0 X-PERF-NNN`（I/C/A-PERF，12 编号 30+ 处）标记多为有效设计文档 | 前缀清理 + 附录B 索引 | ✅ 已清理：前缀清理、设计说明保留 |
| ST-TD-013 | explorer.rs:11-60,115-118 | 多版本（v2.1/Wave/v4.0）历史标记混杂，ST-018"曾误判为死代码"误导 | 压缩 BOM 注释块 + 删历史叙述 | ✅ 已清理：BOM 注释块从 ~30 行压缩至 9 行 |
| ST-TD-014 | wallpaper.rs:482 | `// 详见 findings/03-rust-src-tauri.md ST-014` findings 路径失效 | 更新为 docs 文档路径 | ✅ 已清理：改为 `docs/04-优化文档/06-src-tauri应用层.md` ST-014 |
| ST-TD-015 | wallpaper.rs:15,483,947,959,970; state.rs:353; lib.rs:333 | `fix-wallpaper-preview-blank` 特性分支名作为修复标记 7 处 | 替换为稳定 ID 或删前缀 | ✅ 已清理 |
| ST-TD-016 | explorer.rs:114-121 | WM_DESTROY 注释保留必要核心说明，但含"v2.1 T-016 曾误判为死代码"历史叙述 | 删历史叙述保留核心说明 | ✅ 已清理：删除历史叙述，保留"WM_DESTROY 是外部 DestroyWindow 必要兜底"（explorer.rs:74） |
| ST-TD-017 | fullscreen/lib/wallpaper.rs | "Bug #7 统一 emit"近乎相同注释重复约 6 处 | 保留一份完整说明，其余简短引用 | ✅ 已清理：保留 lib.rs setup 处完整说明，其余 5 处改简短引用 |

**命名一致性（3）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-018 | tests/wallpaper_flow.rs:492,496 vs lib.rs:636,638 | `PauseReason::USER`（仅测试用）vs 生产 `TRAY`，TRAY 路径未被测试覆盖 | 测试改 `TRAY` 或删除 USER 变体 | ✅ 已清理：测试改 `PauseReason::TRAY`；评估后删除 `USER` 变体 |
| ST-TD-019 | lib.rs:361 注释 vs 381,387 代码 | `wpfile://`（注释）vs `wpfile`（代码注册名）不一致 | 注释统一为 `wpfile` | ✅ 已清理：注释统一为 `wpfile`，补充 URL 形式说明 |
| ST-TD-020 | 多处 | "受控数据目录"/"受控目录"/"数据目录"三种术语混称 | 统一术语 | ✅ 已清理：统一为 `受控数据目录（data_dir）` |

**注释陈旧（4）**

| ID | 位置 | 描述 | 清理建议 | 状态 |
|---|---|---|---|---|
| ST-TD-021 | explorer.rs:129 | "作为 30 秒轮询的补充机制"但实际 5 分钟 | 改"5 分钟" | ✅ 已清理：explorer.rs:87 改"作为 5 分钟轮询的补充机制" |
| ST-TD-022 | state.rs:169-183 | Bug #4/#7 演变史叙述约 15 行对理解当前无帮助 | 压缩为 3-5 行 | ✅ 已清理：压缩至 15 行，删除演变史 |
| ST-TD-023 | config.rs:71 | "以防未来类型变更"假设性注释 | 与 ST-TD-008 联动 | ✅ 已清理：改为业务上限说明 |
| ST-TD-024 | state.rs:726; lib.rs:726 | "Bug #5 修复后"前缀属修复痕迹 | 删前缀保留说明 | ✅ 已清理：删除前缀，保留"hide() 而非 destroy()" |

### 3.2 验证与衍生收益（2026-07-26）

> 24 项（P0 7 / P1-P2 12 / P3 5）逐项落实结果见 §3.1 的「状态」列，均已于 2026-07-26 通过 `cleanup-v6-src-tauri-tech-debt-2026-07-26` spec 落实，无遗留项。

- **验证结果**：`cargo build` 通过；`cargo test` 62 个测试通过（36 ignored）；`cargo clippy -D warnings` 零警告；Grep 验证无 `PauseReason::USER` / `向后兼容包装` / `v2.1 T-016` / `Bug #5 修复后` / `findings/03-rust-src-tauri` / `fix-wallpaper-preview-blank` / `v41-ST-` / `v5.0 X-PERF-` 残留（本次合并核验已复验通过）。
- **衍生收益**：跨模块共享校验（`mirrorstar-core::config::validation`）、`PauseReason` 精简（FLU/FULLSCREEN/BATTERY/TRAY 三生产变体）、注释体量累计减少约 200-300 行、术语统一。

### 3.3 与 v4.0/v5.0 文档的关联

**v4.0 已覆盖项**：ST-001~ST-018 均由 Section 2 覆盖。v6 不再重复 v4 的正确性问题，但识别出 v4 修复残留技术债（ST-TD-003/005/006/010~017 关联 ST-005/006/007/013/014/018 与 Bug #1-#7）。ST-014 失效 findings 路径已在 ST-TD-014 清理。

**v5.0 已覆盖项**：v5.0 X-PERF-NNN 标记（I-PERF-001/002/003/004/006/009/010、C-PERF-002/003/005、A-PERF-001/003）均已实施，标记噪音在 ST-TD-012 清理。

## 4. 优化机会与交集汇总

### 4.1 当前优化机会

1. **注释体量压缩**：**2026-10-11 复核**：20 源文件约 14115 行（原文档记 12 文件约 5630 行已失真）；explorer.rs BOM 块（~30→9 行）、state.rs Bug #7 叙述（~30→15 行）已收敛，剩余文档化注释多为有效设计文档。
2. **测试覆盖对齐**：ST-TD-018 后 `PauseReason` 生产变体（`FULLSCREEN`/`BATTERY`/`TRAY`）应确保均在测试中被覆盖。
3. **历史标记索引化**：Bug #N / ST-NNN / v41-ST-NNN / v5.0 X-PERF-NNN / fix-wallpaper-preview-blank 五套标记已统一清理由本文档与附录索引承载，代码注释从"内联完整说明"过渡到"引用 ID + 简短说明"。
4. **校验逻辑下沉**：ST-TD-005 已提取 `mirrorstar-core::config::validation`，未来新增校验场景可直接复用。
5. **Web 分支可达性对称**：ST-TD-001 修复后，建议今后审查所有按 `WallpaperType` 分派的代码路径，确保 Web 处理统一（外层过滤或内层统一处理）。

### 4.2 v4 / v6 交集汇总

| 交集项 | v4 状态 | v6 关联 | 当前净状态 |
|---|---|---|---|
| `add_wallpaper` 路径比较（ST-001） | ✅ 修复（`Path::starts_with`） | ST-TD-011 记录 v41-ST-009 修复痕迹 | 修复 |
| `open_file_dialog` 线程泄漏（ST-002） | ✅ 修复（回调+oneshot） | ST-TD-012 相关 v5.0 PERF 标记 | 修复 |
| `workerw_check` std Mutex 阻塞（ST-003） | ✅ 接受 | v6 无新债 | 接受 |
| 错误变体泛化（ST-004） | ✅ 修复（`InvalidArgument`） | ST-TD-005 校验重复实现 | 修复 + 共享化 |
| `set_speed` fire-and-forget（ST-005） | ✅ 修复（同步执行） | ST-TD-012 修复痕迹 | 修复 |
| shutdown 缩略图写入（ST-006） | ✅ 修复（二次 flush） | ST-TD-011 修复痕迹 | 修复 |
| Junction 检测（ST-007） | ✅ 修复（canonicalize） | ST-TD-011 修复痕迹 | 修复 |
| `check_desktop_status` async（ST-013） | ✅ 接受 | ST-TD-006 重评估为"过时模式" | 接受（保留） |
| 缩略图 JoinHandle 未保存（ST-014 v3.2） | ✅ 修复 | ST-TD-014 findings 路径失效 | 修复 + 引用更新 |
| WM_DESTROY 误判死代码（ST-018） | ✅ 修复 | ST-TD-013/016 历史叙述清理 | 修复 |
| Bug #1-#7 | ✅ 全部修复 | ST-TD-010/017 标记清理 | 修复 + 注释收敛 |

**核心结论**：src-tauri 应用层经 v3.x → v4.0 → v4.1 → v5.0 → v6 多轮修复后，工程质量与正确性已达高水平（**约 196 个**单元测试、clippy 零警告、详尽的锁顺序/COM/shutdown 文档化）。v4 findings 17 项全部关闭；v6 技术债 24 项全部清理（8 项修复痕迹为最大类别，均已收敛）。

## 5. v17.0 内存优化（2026-10-02）

本轮针对「设置动态壁纸后应用组内存偏高」，新增内存归因埋点并实施 6 项手段；本模块涉及 **Task 1（归因埋点）、Task 2（主窗口参数裁剪）、Task 3（非活跃态挂起/降级）**。

### 5.1 Task 1：内存归因埋点（`mirrorstar-core/src/perf.rs` + `commands/system.rs` + `lib.rs`）

`mirrorstar-core` 新增逐子进程内存归因（`MemoryUsageReport` / `ProcessCategory` / `attribute_processes` / `aggregate` / `sample_memory_report` / `spawn_memory_attribution_sampler`），src-tauri 侧接入两处：

| 入口 | 位置 | 说明 |
|---|---|---|
| Tauri 命令 `get_memory_report` | `commands/system.rs` | 按需同步返回分类别明细与总计（主进程 / `mpv.exe` / 壳同名 `--wp-proc` 子进程 / `msedgewebview2.exe` / other），开销小、无需 async |
| 周期采样 | `lib.rs` setup | `spawn_memory_attribution_sampler(Duration::from_secs(60))`，每 60s 输出一行 `MEM-ATTRIB: 应用组逐子进程内存归因`（target `mirrorstar::perf`，含工作集与私有提交 MB） |

归因限定在当前进程进程树内（以当前 pid 为根按父 pid 建树），不会误算系统上其他应用的 `msedgewebview2.exe`。命令已在 `invoke_handler` 注册。

### 5.2 Task 2：主窗口 WebView2 启动参数裁剪（`state.rs`）—— ⚠️ 历史记录，代码已移除

> **2026-10-11 复核：已消解**。`MAIN_WINDOW_BROWSER_ARGS` / `additional_browser_args` 在代码中已**零命中**，随 2026-10-02 主窗口原生 UI 重构移除（壳不再创建 WebView2 主窗口）。以下为历史记录。

新增常量 `MAIN_WINDOW_BROWSER_ARGS` 并挂到主窗口 `WebviewWindowBuilder::additional_browser_args`：

```
--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,BackForwardCache,MediaCache,FontCache,GpuMemDiscardable
--autoplay-policy=no-user-gesture-required
--renderer-process-limit=1
--js-flags=--max-old-space-size=512
--disk-cache-size=52428800
```

- **关键点（替换语义必须保留 wry 默认项）**：wry 0.55.1 在 `create_environment`（`wry-0.55.1/src/webview2/mod.rs:294`）对 `additional_browser_args` 使用 `unwrap_or_else(...)`——一旦设置，wry 默认参数串会被整体**替换**。故常量内**显式保留**了默认的 `msWebOOUI,msPdfOOUI,msSmartScreenProtection` 与 `--autoplay-policy=no-user-gesture-required`，否则会静默丢失默认 UI/SmartScreen 裁剪与自动播放策略。所有禁用特性合并进同一个 `--disable-features=` 列表。
- **GPU 降级结论：暂不启用 `--disable-gpu`**：无真实 GUI 实测依据，软件合成（SwiftShader/CPU 光栅化）可能反而推高 renderer 内存与 CPU，且主窗口含 CSS 过渡/拖拽排序动效。后续验证方法见 `state.rs` 常量后的注释。
- **降级**：Chromium 对未知 flag 静默忽略；`additional_browser_args` 仅写入字符串、不参与 `.build()` 成败判定。构建前以 `tracing::info!(args = MAIN_WINDOW_BROWSER_ARGS, ...)` 记录参数便于真机核对。

常量文档注释与守卫测试已固化；`state.rs` 内另有测试注释说明「替换语义」与默认项保留契约。

### 5.3 Task 3：非活跃态 WebView2 挂起 / 目标等级降级（`platform/webview_memory.rs`）—— ⚠️ 历史记录，代码已移除

> **2026-10-11 复核：已消解**。`platform/webview_memory.rs` 文件与 `suspend_main_webview` / `resume_main_webview` / `TrySuspend` / `SetMemoryUsageTargetLevel` 等符号在代码中**全部零命中**，随主窗口重构移除。以下为历史记录。

新增模块封装 best-effort 回收：

- `suspend_main_webview`：`ICoreWebView2_3::IsSuspended` 幂等判定 + `TrySuspend` + `ICoreWebView2_19::SetMemoryUsageTargetLevel(Low)`；
- `resume_main_webview`：目标等级置回 `Normal` + 确定已挂起时 `Resume`；
- `sync_main_webview_active_state`：`Resized` 边沿触发，仅状态变化时投递；
- `reset_main_webview_suspended_state`：窗口重建后重置记录，避免漏挂起。

接入点：`minimize_main_window_on_fullscreen`（最小化成功后挂起）、`create_or_show_main_window` 已存在分支（恢复）与 build 分支（重置）、`lib.rs` `on_window_event` 的 `CloseRequested`（关闭前 best-effort Resume，缓解项）与 `Resized`（通用最小化/恢复同步）。

- **`TrySuspend` 前置条件与降级策略**：文档要求 `controller.IsVisible == false` 否则返回 `ERROR_INVALID_STATE`（best-effort，完成回调 `isSuccessful=false`）。本模块不主动置 `IsVisible=false`（避免恢复闪失导致白屏），故最小化场景下 `TrySuspend` 很可能失败；但**目标等级降级无此前置条件，始终生效**。所有失败仅 `warn!`，不 panic、不影响窗口操作；旧运行时缺接口时优雅跳过。
- **依赖对齐**：`Cargo.toml` 新增 `webview2-com = "0.38"` 与 `windows 0.61`（别名 `windows61`），必须与 wry 0.55.1 / tauri-runtime-wry 2.11.2 同源。

### 5.4 关闭主窗口语义（核实修正）

`lib.rs` 的 `on_window_event` 主窗口 `CloseRequested` 分支**不调用 `api.prevent_close()`**，让 Tauri 默认销毁主窗口、释放 WebView2 子进程树——即「关闭即销毁」而非「关闭即隐藏（hide）」。文档 `09-性能优化-Performance.md` §11.7 已据此修正。

**验证结果**：`cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` 见本轮质量门槛报告；**优化前后内存实测数值待真机补测**（本环境无 GUI，不伪造数据）。

## 6. 深度审查（2026-10-11，第 7 个模块）

> **审查范围**：`src-tauri` 应用层 `src-tauri/src/`（20 文件约 14,115 行）+ `src-tauri/tests/`（3 文件约 1,487 行）。
> **审查维度**：死代码 / 冗余代码 / 过度抽象 / 文档一致性。
> **判定口径**：`可清理` = 零行为变更 + 全仓无调用方 + 非公开 API 表面 + 不涉跨 crate 契约与已登记编号；`待确认` = 涉及运行时行为 / 公开 API 语义 / 跨 crate 契约 / `ST-*`、`ST-TD-*`、`Bug #N` 等已登记编号 / 测试依赖。
> **方法**：20 文件全文通读 + 全仓（`src-tauri/src/`、`src-tauri/tests/`、`crates/`）词边界引用搜索 + 对照基线。

### 6.1 基线快照（本轮，零行为变更清理前）

| 命令 | 结果 |
|---|---|
| `cargo check --workspace` | Finished，0 error / 0 warning |
| `cargo clippy --workspace --all-targets` | 0 warning（说明无 `dead_code` / `unused` 告警；源码亦无 `#[allow(dead_code)]`） |
| `cargo test -p mirrorstar-core --lib -- --skip mpv_backend` | **712 passed / 0 failed / 30 ignored** |
| `cargo test -p mirrorstar-wallpaper` | lib 193 passed / 3 ignored；config_flow 6 passed；wallpaper_flow 6 passed / 29 ignored |

**公开 API 证据表**：从 `src-tauri/src/` 采集 `pub` / `pub(crate)` 的 `fn`/`struct`/`enum`/`const`/`static`/`type` 共 **122 个符号**，逐个全仓引用搜索，结果 **内部引用 122（100%） / 仅测试引用 0 / 仅文档引用 0 / 无任何引用 0**。命令注册 **36 定义 / 36 注册**（`lib.rs:1124-1161`），无未注册命令。

### 6.2 死代码

**结论：未发现生产死代码。** 与 clippy `--all-targets` 零告警、122 符号 100% 内部引用相互印证。以下为「有调用方但值得记录」的候选：

| 编号 | 级别 | 位置 | 证据 | 处置 |
|---|---|---|---|---|
| FD-PLAT-1 | 低 | `platform/fullscreen.rs:810-817` | `#[cfg(test)] should_trigger_resume(...)` 全仓引用**仅**本文件 `mod tests`（1221-1244）；生产路径已改用短路判断 `is_previous_fullscreen_window_still_active()`（:281/:515） | 待确认（有测试引用，非「全仓无调用方」） |
| FD-PLAT-2 | 低 | `platform/liveness_watchdog.rs:103-105,149` + `lib.rs:643` | `start_liveness_watchdog` 返回 `Option<JoinHandle<()>>`，唯一调用方 `let _ =` 丢弃 | 待确认（模块文档明示「随 runtime 结束、不显式停止」，非缺陷） |
| FD-SCHED-1 | 低 | `scheduler.rs:254` | `pub(crate) fn flush_playback` 仅本文件内部调用（175/187/206/248/406/429/505/525/537/564），无跨模块调用方 | 待确认（可见性可收窄） |
| FD-SCHED-2 | 低 | `scheduler.rs:98` | `pub store: PlaybackStore` 字段全仓仅 `self.store.save(...)`（:259）使用 | 待确认（涉及 `pub` 字段布局） |
| FD-SCHED-3 | 低 | `scheduler.rs:88-91` | `pub struct RotatedPayload` 及 `pub key`/`pub wallpaper_id` 仅本文件 `emit_rotated`（1276-1286）构造 | 待确认（事件负载契约，稳妥保留） |
| FD-CMD-1/2 | 信息 | `commands/*` | 36 个 `#[tauri::command]` 全部注册；私有/`pub(crate)` helper 调用方齐全 | 无死代码 |
| — | 信息 | `create_or_show_main_window` | `src-tauri/src/` 与 `tests/` 内**已无定义与调用**（被 UI 子进程方案取代），仅残留 `docs/` | 文档陈旧（见 6.5） |

`ui_process/` 全部 `pub`/`pub(crate)` API 调用方齐全（`IDLE_TIMEOUT_ENV`/`init`/`manager`/`open_main_window`/`is_running`/`request_show`/`send`/`tick_idle`/`shutdown`/`FORWARDED_EVENTS`/`install_event_forwarding`/`handle_request` 等）；`#[cfg(test)]` 辅助（`bridge.rs:325`、`paths.rs:42`、`mod.rs:574`）均有实际测试引用。

### 6.3 冗余代码

| 编号 | 级别 | 位置 | 证据 | 处置 |
|---|---|---|---|---|
| RD-CORE-1 | 中 | `state.rs:404-411/439-444/488-493/517-523` | 四个共享 helper 开头「纯净壁纸模式守卫」块逐字重复，`warn!` 文案完全相同 | 待确认（单测 `pure_mode_guards_present_in_all_shared_helpers`:1124 用 `include_str!` 断言每个函数体内含 `mirrorstar_core::pure_mode::is_active()`，重构须同步改测试） |
| RD-CORE-2 | 低 | `state.rs:828-837` | `perform_shutdown_blocking` 末尾两次 `config_manager.flush()` 结构近重复 | 待确认（ST-006 双 flush 契约；单测断言 `二次刷写` 字符串，不可删） |
| RD-SCHED-1 | 中 | `scheduler.rs:438-441 / 495-498` | 暂停抑制样板 `pure_mode::is_active() || { engine.lock().await; !pause_reasons_snapshot().is_empty() }` 逐字重复两次 | 待确认（测试 `rotation_gates_check_pure_mode`:1920-1956 以 `include_str!` 断言纯模式门控精确出现 3 处） |
| RD-SCHED-2 | 中 | `scheduler.rs:225-233/628-637`（+`commands/rotation.rs:210-215`） | 「取 arrangement → 枚举显示器 → `LayoutMonitor::from` → `layout::plan`」样板重复三处；`enumerate_displays` helper 与内联写法并存 | 待确认（可抽 core 纯函数，跨 crate 且涉 rotation.rs） |
| RD-SCHED-3 | 低 | `scheduler.rs:333-339/738-744/988-1002` | `SamplerState` 快照 → `invalidate_sampler`/`sample_next` → 写回 unit 模式重复三处 | 待确认 |
| RD-PLAT-1 | 中 | `explorer.rs:248-262`、`fullscreen.rs:454-475`、`pure_mode_input.rs:448-461` | `GetMessageW` 消息循环骨架（`match ret.0 {0=>break,-1=>{…;break},_=>{}}` + `TranslateMessage`/`DispatchMessageW`）三处近乎逐字重复 | 待确认（涉运行期行为） |
| RD-PLAT-2 | 中 | `explorer.rs:93-123`、`fullscreen.rs:355-401` | 「二次调用先 stop 旧监控」块同构（take hook/tid → `PostThreadMessageW(WM_QUIT)` → take JoinHandle → `join()`）；`state.rs:552-566` 已抽 `signal_thread_exit`/`join_monitor_thread` 但两处未复用 | 待确认 |
| RD-PLAT-3 | 中 | `explorer.rs:166-273`、`pure_mode_input.rs:339-398` | 「注册窗口类 + `CreateWindowExW` + 失败 `UnregisterClassW` + 退出 `DestroyWindow`/`UnregisterClassW`」骨架重复 | 待确认（两者窗口语义不同） |
| RD-PLAT-4 | 中 | `explorer.rs:155-164` vs `pure_mode_input.rs:157-159/340-345/596-602` | `TaskbarCreated` 消息 ID 存在两套机制：explorer 用 `state::TASKBAR_CREATED_MSG`（OnceLock），pure_mode_input 用本模块 `AtomicU32` 再 `RegisterWindowMessageW` | 待确认（跨模块契约） |
| RD-PLAT-5 | 中 | `pure_mode.rs:464-587` vs `liveness_watchdog.rs:165-270` | 退出存活校验与常驻看护的编排骨架重复（共享纯函数已抽出，外层「采样+复核+self-heal」各写一份；间隔/锁语义不同） | 待确认（差异大，合并风险高） |
| RD-PLAT-6 | 低 | `power.rs:105-113/154-162/172-180` | `failed.is_empty()` → 清标志 / else warn 结构三处近同构 | 待确认（收益低） |
| RD-PLAT-7 | 中 | `explorer.rs:29-82` | Win32 回调 `catch_unwind` 包裹在 fullscreen / pure_mode_input 有，唯独 `explorer_monitor_wndproc`（内部调 `desktop.check_and_reinitialize()`）**无** | 待确认（建议补齐，涉运行期行为） |
| RD-UIP-1 | 中 | `ui_process/mod.rs:172-225` | `UiJobGuard` 与 core `process/manager.rs:465-554` `JobObjectGuard` 并行实现同一套 Win32 Job 样板 | 待确认（**已在 `05-audio-ipc-process模块.md:647` 登记为跨模块重复、归口 src-tauri、本轮不改**） |
| RD-UIP-2 | 低 | `ui_process/mod.rs:353/367/382/391/413/499/517` | 退出清理 `let _ = child.kill(); let _ = child.wait();` 重复约 7 处 | 待确认（涉 `Child` 所有权与运行期清理） |
| RD-CMD-1..6 | 低 | `commands/wallpaper.rs` 等 | `validate_volume`/`validate_speed`/`resolve_display_id` 薄封装（受 `#[doc(hidden)]` 重导出契约保护）；`as_deref()==Some("all")` 广播分支 6 处；锁处理样板 2+2 处；`emit` 错误处理 6 处；`config.rs` 错误映射 2 处 | 待确认（各分支错误/日志语义不同，合并会改变语义） |

> **判定为非冗余**：`scheduler.rs:1234-1256` `resolve_target_key` 与 `commands/rotation.rs:208-221` `layout::validate_unit_key` 表面相似但差异属有意（命令层返回 `Err`、调度器缺省 `None`=主/唯一单元），doc 已声明。

### 6.4 过度抽象

| 编号 | 级别 | 位置 | 证据 | 处置 |
|---|---|---|---|---|
| OA-CORE-1 | 低 | `state.rs:609-614` | `try_lock_with_timeout<T>` 泛型仅 2 个实例化类型（生产 `WallpaperEngine`+测试 `Mutex<()>`），泛型唯一动机是测试便利（:596-597 自述） | 待确认（现设计可接受） |
| OA-CORE-2 | 中 | `lib.rs:31-145` | `run()` 顶部「启动顺序+失败回滚+设计权衡」文档块约 115 行，与正文注释多处重复（COM / 音频降级 / ConfigManager exit 论证） | 待确认（承载回滚策略论证） |
| OA-CORE-3 | 低 | `state.rs:135-180` | 全局静态量 A/B/C 分类说明块较长且计数陈旧 | 待确认（ST-TD-003/004/009 已评估保留） |
| OA-SCHED-1 | 低 | `scheduler.rs:853-855` | `arrangement_changed(applied,current) -> bool { applied != current }` 平凡包装，生产仅 1 调用点 | 待确认（测试 `arrangement_changed_short_circuits_on_same_value`:1960 依赖） |
| OA-SCHED-2 | 低 | `scheduler.rs:860-862` | `display_ids(displays) -> Vec<String>` 平凡包装，2 调用点 | 待确认（收益低） |
| OA-SCHED-3 | 低 | `scheduler.rs:276-312` | `pop_history_until_applied<F,Fut>` 泛型闭包仅服务 1 个生产调用点，泛型唯一目的是注入假 apply 供 5 个单测 | 待确认（doc 已说明：构造 Tauri App 会触发 rustc LLVM ICE，属测试驱动的必要抽象） |
| OA-PLAT-1 | 低 | `fullscreen.rs:801-817` | `should_trigger_resume` 测试专用纯函数，同一判定逻辑生产/测试各写一份 | 待确认（删它会减少单元覆盖） |
| OA-PLAT-2 | 低 | `fullscreen.rs:88-94` vs `state.rs:224-246` | `SendHwnd` / `SendWinEventHook` 两个独立裸句柄 `Send`/`Sync` 包装器，各自重复 `unsafe impl` + soundness 论证 | 待确认（类型不同，合并收益有限） |
| OA-UIP-1/2 | 低 | `ui_process/paths.rs:22-29`、`bridge.rs:309-323` | `resolve_from_exe_dir`、`snake_to_camel` 单调用点薄封装 | 澄清保留（前者为 tempdir 测试 seam，后者提供 snake/camel 双键兼容） |
| OA-CMD-1 | 低 | `commands/wallpaper.rs:378-463` | `validate_and_get_metadata`（保留 metadata）与 `validate_wallpaper_file_path`（丢弃 metadata）并存，调用点清晰 | 待确认（ST-TD-003 已评估保留） |
| OA-CMD-2 | 低 | `lib.rs:22-27` | `#[doc(hidden)] pub use commands::wallpaper::{parse_scaling_mode, validate_speed, validate_volume, resolve_display_id}` 纯测试专用重导出（被 `tests/wallpaper_flow.rs:27-29` 引用） | 待确认（跨测试契约，不可动） |
| OA-CMD-3 | 低 | `commands/wallpaper.rs:1329-1340` | `parse_scaling_mode(&str)` **生产无调用方**（`set_scaling_mode` 形参已是 `ScalingMode` 枚举），仅测试引用 | 待确认（受 OA-CMD-2 重导出契约保护） |
| OA-CMD-4 | 信息 | `wallpaper.rs:184-253/276-339` | `ComGuard`、`DisplaySettingGuard` 均多调用方 | 非过度抽象 |

### 6.5 文档一致性（doc 06 ↔ 代码）

对照 `docs/04-优化文档/06-src-tauri应用层.md` §1~§5 逐条核对，判定「已消解 / 仍存在 / 描述失真」：

| 编号 | 章节 | 判定 | 说明 |
|---|---|---|---|
| DOC-1 | §1.2 文件清单 | 描述失真 | 文档记 12 文件，实际 **20** 文件；缺 `scheduler.rs`、`ui_process/{mod,bridge,paths}.rs`、`platform/{liveness_watchdog,pure_mode,pure_mode_input}.rs`、`commands/rotation.rs` |
| DOC-2 | §1.2 行数 | 描述失真 | 差异最大 `fullscreen.rs` 563→**1801**（+1238）、`lib.rs` 773→**1292**（+519） |
| DOC-3 | §1.1/§1.2 命令数 | 描述失真 | 文档记 24 个，实际 `generate_handler!` 注册 **36** 个；清单含已不存在的 `quit_app`/`toggle_interaction`/`regenerate_thumbnails`/`update_positions` |
| DOC-4 | §1.1 职责 5（`wpfile`） | 已消解 | `wpfile`/`register_uri_scheme` 零命中，已随 2026-10-02 重构移除（正文已补删除线标注） |
| DOC-5 | §1.3 单元测试数 | 描述失真 | 文档记约 49，实测 **约 196**（`#[test]` 178 + `#[tokio::test]` 18） |
| DOC-6 | §1.3 集成测试 | 描述失真 | `wallpaper_flow` 文档记约 25 / 仅 4 个 CI 可执行，实测 **35 / 29 ignore / 6 个 CI 可执行** |
| DOC-7 | §1.4 全局静态量 | 描述失真 | 文档记 19，实测 **21**（新增 `SHARED_APP_HANDLE`、`SHARED_SCHEDULER`） |
| DOC-8 | §1.1/§1.4 监控线程 | 描述失真 | 文档记 3 类，实际 **4 类**（补 `liveness_watchdog`） |
| DOC-9 | §2 `ST-001`~`ST-017` | 核心判定成立，行号失真 | 抽查 ST-001/002/010 判定仍成立，但所引行号普遍漂移（如 `state.rs:361-362`→**374-375**） |
| DOC-10 | §3.1 `ST-TD-001`~`ST-TD-024` | 多数已消解，叙述偏乐观 | ST-TD-011/012 声称「前缀清理」，`v41-ST-`/`X-PERF-` 确已零命中，但 `ST-`/`T` 前缀注释与测试标识符仍大面积残留 |
| DOC-11 | §5.2 / §5.3 | 已消解 | `MAIN_WINDOW_BROWSER_ARGS`、`platform/webview_memory.rs`、`TrySuspend`、`SetMemoryUsageTargetLevel` 全部零命中（两节标题已加「历史记录，代码已移除」） |
| DOC-12 | §5.1 / §5.4 | 仍存在 | `get_memory_report`、`spawn_memory_attribution_sampler` 均在；「不调用 `api.prevent_close()`」与代码一致 |
| DOC-13 | §1.1 职责 3 / §1.4 | 描述失真 | `run()` 序列未含新增的 `SchedulerHandle` 与 `ui_process` |
| DOC-14 | §1.2 mod.rs | 描述失真 | `platform/mod.rs` 8→**11** 行；`commands/mod.rs` 95→**123** 行、3→**4** 个 `pub use` |
| DOC-15 | §1.2 explorer.rs / system.rs | 描述失真 | `explorer.rs` 329→**289** 行；`system.rs` 含 `toggle_interaction`/`quit_app` 已不存在 |

**跨 crate 契约核验**：`use mirrorstar_core::` 共 26 处 import（16 文件）；`grep "^(pub )?(struct|enum|trait) (ConfigManager|WallpaperEngine|DesktopIntegrator|VolumeControl|...)"` **零命中**，src-tauri 未重定义任何 core 核心类型；`validate_volume`/`validate_speed` 确已下沉 `mirrorstar_core::config::validation`（`commands/config.rs:99/111`、`wallpaper.rs:1316/1323` 委托调用）；`scheduler.rs` 纯决策逻辑全部来自 `mirrorstar_core::scheduler`，本模块仅做「决策串联 + 单后台任务」。**结论：无逻辑重复实现**（唯一交叠点为 Job Object 样板 RD-UIP-1，已登记）。

### 6.6 本轮零行为变更清理（已实施）

以下均为**注释文本 / 文档文字 / 可见性收窄**类改动，零行为变更：

| 编号 | 文件 | 处置 |
|---|---|---|
| C-CORE-1 | `state.rs:625` | 笔误函数名 `try_lock_engine_with_timeout` → `try_lock_with_timeout` |
| C-CORE-2 | `state.rs`（9 处）、`lib.rs`（7 处） | 剥离 `Task N.N.N` / `SubTask N.N.N` 前缀（保留说明文案；已核验不被任何 `include_str!` 断言） |
| RD-CORE-3 | `lib.rs:898-900` | 三行重复 `set_text` 说明压成一行（理由已在 878-880 处说明） |
| C-SCHED-1 | `scheduler.rs`（18 处） | 剥离 `Task N` 前缀 |
| C-SCHED-2 | `scheduler.rs:812` | 去 `v17 ` 版本前缀 |
| C-CMD-1 | `commands/mod.rs:13-17` | 修正 `//!` 模块文档命令清单（system 补 3 命令；wallpaper 删误列的 `set_interaction_mode`） |
| C-CMD-2 | `commands/mod.rs:53` | `generate_handler!` 示例块补 `get_memory_report`/`report_lifecycle_mark` |
| FD-UIP-1 | `ui_process/mod.rs:84` | `pub use paths::resolve_ui_exe;` → `use paths::resolve_ui_exe;`（模块私有，收窄冗余 `pub`） |
| — | doc 06 §0/§1.1/§1.2/§1.3/§1.4/§4.1/§4.2/§5.2/§5.3 | 按 DOC-1~15 校正文件清单 / 行数 / 命令数 / 测试数 / 静态量数 / 监控线程数，并对 §5.2/§5.3 标注「历史记录，代码已移除」 |

### 6.7 待确认项（未实施）

涉运行时行为、公开 API 语义、跨 crate/跨测试契约或已登记编号，**本轮一律不改动**：

- **测试契约锁定**：RD-CORE-1、RD-CORE-2、RD-SCHED-1（`include_str!` 源码断言）、OA-CMD-2、OA-CMD-3（`#[doc(hidden)]` 重导出 + `tests/` 依赖）、C-CMD-3（`ST-007:`/`ST-005:` 源码断言标记）。
- **跨 crate 契约**：RD-UIP-1（与 core `JobObjectGuard` 重复，已在 doc 05 登记）、RD-SCHED-2（需抽 core 纯函数并涉 rotation.rs）、RD-PLAT-4（`TaskbarCreated` 双机制）。
- **运行期行为**：RD-PLAT-1/2/3/5/6/7、RD-UIP-2、RD-CMD-3/4/5/6、OA-CORE-1、OA-SCHED-1/2。
- **已登记编号**：`ST-*`/`ST-TD-*`/`Bug #N`/`T##`/`C-##`/`DR-##` 等修复痕迹注释。
- **文档范围**：C-CORE-5（`docs/` 仍引用已删除的 `create_or_show_main_window` 等）。

### 6.8 审查小结

`src-tauri` 应用层经多轮重构后**无生产死代码**（122 公开符号 100% 内部引用、clippy 零告警、36/36 命令注册），**无对 core/wp-proc 的逻辑重复实现**。主要问题集中在三类：①跨渲染器/监控线程 Win32 骨架重复（RD-PLAT-*，多为运行期路径，保守保留）；②受 `include_str!` 测试契约保护的样板重复（RD-CORE-1、RD-SCHED-1）；③文档严重滞后（§1.2 仅记 12/20 文件、§1.3 测试数差 4 倍、§5.2/§5.3 整节已消解）。本轮完成 9 项零行为变更清理（注释修正 + 一处 `pub` 收窄 + doc 06 全面校正），全部待确认项维持现状。
