# src-tauri 应用层模块优化文档

> [← 返回索引](./README.md)

> **文档说明**：本文由 v4.0 模块审查文档（本文件旧版）与 v6.0 技术债审查（src-tauri 应用层）合并而成。
> - **v4.0 findings**：17 项（ST-001~ST-017），已逐项对照代码核验，全部修复或已评估接受。
> - **v6.0 技术债**：24 项（ST-TD-001~ST-TD-024），已按 `cleanup-v6-src-tauri-tech-debt-2026-07-26` spec 全部清理（7 P0 + 12 P1/P2 + 5 P3 保留现状）。
> - 审查日期：2026-07-25 | 模块路径：`src-tauri/src/` | 源文件数：12

## 1. 模块概览 / 现状 与 文件清单

### 1.1 模块职责

`src-tauri` 应用层是镜星壁纸的 Windows 客户端壳，承担五项核心职责：

1. **Tauri 命令层**：通过 `#[tauri::command]` 把 24 个前端可调用命令（壁纸生命周期 / 配置管理 / 系统控制）封装为对 `mirrorstar-core`（`ConfigManager` / `WallpaperEngine` / `DesktopIntegrator`）的薄封装。
2. **Win32 平台集成**：启动三类后台监控线程——全屏检测（`SetWinEventHook`）、Explorer 重启监控（`TaskbarCreated` 消息窗口）、WorkerW 兜底检查（5 分钟间隔）；处理 `WM_POWERBROADCAST` 电源事件。
3. **进程生命周期管理**：`run()` 顺序初始化 COM（STA）→ DesktopIntegrator → WorkerW 预初始化线程 → VolumeControl → WallpaperEngine → ConfigManager → 监控线程 → Tauri Builder；`perform_shutdown_blocking` 按 LIFO 顺序 + 幂等守卫 + 多个超时上界统一清理。
4. **前端事件桥接**：通过 `app.emit(...)` 把后端状态变更（壁纸增删改 / 状态切换 / 桌面状态 / 配置变更 / 缩略图进度）通知前端；订阅 `WallpaperEngine::global_state_changed` broadcast 通道统一转发 `wallpaper-state-changed`。
5. **自定义协议**：注册 `wpfile` URI scheme handler，支持 HTTP Range / HEAD 请求，绕过 Tauri asset protocol scope 限制直接读取壁纸文件。

该模块是 v3.x → v4.0 → v4.1 → v5.0 → v6 多轮修复的重灾区，积累了大量历史环评标记（`Bug #N` / `ST-NNN` / `v41-ST-NNN` / `v5.0 X-PERF-NNN`），是 v6 技术债审查的重点模块。

### 1.2 文件清单

#### commands/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 95 | 命令模块导出（`pub use config::*; pub use system::*; pub use wallpaper::*;`） |
| config.rs | 166 | `get_config` / `update_config` / `generate_thumbnail` 3 个命令 + SEC-002 `validate_config_fields` 字段范围校验（委托 `mirrorstar-core::config::validation`，ST-005） |
| system.rs | 255 | `get_displays` / `check_desktop_status` / `set_interaction_mode` / `toggle_interaction` / `quit_app` / `open_file_dialog` / `toggle_auto_start` / `get_auto_start_status` 8 个命令 + ST-002 文件对话框（回调式 + oneshot）+ 4 个 ST-002 单元测试 |
| wallpaper.rs | 1969 | `get_wallpapers` / `add_wallpaper` / `remove_wallpaper` / `set_wallpaper` / `pause_wallpaper` / `resume_wallpaper` / `set_volume` / `toggle_mute` / `get_wallpaper_state` / `set_scaling_mode` / `set_speed` / `update_positions` / `regenerate_thumbnails` 13 个命令 + `ComGuard` RAII + `DisplaySettingGuard` 防竞态 + `validate_wallpaper_file_path` / `validate_volume` / `validate_speed` / `parse_scaling_mode` 纯函数 + 18 个单元测试 |

#### platform/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 8 | 平台模块导出（3 个 `pub(crate) use`）|
| explorer.rs | 329 | `start_explorer_restart_monitor`：HWND_MESSAGE 消息窗口 + `explorer_monitor_wndproc` 处理 TaskbarCreated / WM_POWERBROADCAST / WM_DESTROY（ST-018）+ C-014/C-015 二次调用 stop+start |
| fullscreen.rs | 563 | `start_fullscreen_monitor`：`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + `foreground_event_callback` + `is_foreground_fullscreen` + 3 个纯函数 + 11 个纯函数单元测试 |
| power.rs | 235 | `handle_power_status_change`：`GetSystemPowerStatus` + `interpret_ac_line_status` + 电池↔AC 切换暂停/恢复 + 4 个 ACLineStatus 单元测试 |
| workerw_check.rs | 141 | `start_workerw_check`：5 分钟 interval + `WORKERW_CHECK_NOTIFY` 即时唤醒 + ST-003 JoinHandle 保存 + T07 emit `desktop-status-changed` |

#### 顶层
| 文件 | 行数 | 主要内容 |
|---|---|---|
| state.rs | 1035 | `AppState` + 19 个全局静态量（保守策略全部保留文档化）+ `try_pause_all_fast` / `try_resume_all_fast`（ST-004 抽取）+ `signal_thread_exit` / `join_monitor_thread`（ST-005 抽取）+ `try_lock_with_timeout<T>`（T04）+ `perform_shutdown_blocking`（Bug #6/T04/ST-002/ST-003/ST-006/ST-014）+ `create_or_show_main_window` + 12 个单元测试 |
| lib.rs | 773 | `run()` 入口：日志 + COM STA + WorkerW 预初始化 + VolumeControl + Engine + Config + 监控线程 + Tauri Builder + wpfile:// 协议 handler（Range/HEAD 支持）+ setup 闭包 |
| main.rs | 61 | `main()` + `ensure_single_instance()`（CreateMutexW 单实例互斥体）+ ST-011 句柄无 Drop 文档化 |

### 1.3 测试覆盖

- **单元测试**：分布在 `commands/system.rs`（4 个 ST-002 超时机制测试）、`commands/wallpaper.rs`（18 个：路径校验 / DisplaySettingGuard / THUMBNAIL_TASK Vec / validate_volume/speed/parse_scaling_mode / ST-007 junction / ST-005 同步执行 / v41-ST-009 越界校验）、`platform/fullscreen.rs`（11 个纯函数测试）、`platform/power.rs`（4 个 ACLineStatus 测试）、`state.rs`（12 个：SHUTDOWN_DONE 守卫 / T04 try_lock_with_timeout / ST-006 flush 兜底）。共约 49 个单元测试，多数为纯逻辑测试可在任意平台 CI 执行。
- **集成测试**：`tests/config_flow.rs`（6 个 ConfigManager 增删改流程测试，纯逻辑无 #[ignore]）、`tests/wallpaper_flow.rs`（约 25 个测试，多数标记 `#[ignore]` 需 Windows COM/音频环境，仅 4 个纯逻辑测试 CI 可执行）、`tests/common/mod.rs`（MockRenderer + 测试辅助函数）。
- **测试盲区**：`create_or_show_main_window` / `wpfile://` handler / Tauri Builder setup 闭包 / Win32 回调路径无法单元测试，依赖手动端到端验证。

### 1.4 核心结构与设计模式（v4 概要浓缩）

- **核心结构**：`AppState`（`Arc<ConfigManager>` + `Arc<tokio::sync::Mutex<WallpaperEngine>>` + `Arc<Mutex<DesktopIntegrator>>` + `tray_paused: AtomicBool` + `tray_pause_resume_item: OnceLock<MenuItem>`）。
- **19 个全局可变静态量**（state.rs）：`SHARED_ENGINE`/`SHARED_CONFIG`/`FULLSCREEN_WAS`/`EXPLORER_DESKTOP`/`TASKBAR_CREATED_MSG`/`POWER_WAS_ON_BATTERY`/`WIN_EVENT_HOOK`/`FULLSCREEN_MONITOR_RUNNING`/`FULLSCREEN_MONITOR_THREAD_ID`/`FULLSCREEN_MONITOR_THREAD`/`EXPLORER_MONITOR_RUNNING`/`EXPLORER_MONITOR_THREAD_ID`/`EXPLORER_MONITOR_THREAD`/`WORKERW_CHECK_RUNNING`/`WORKERW_CHECK_TASK`/`WORKERW_CHECK_NOTIFY`/`WORKERW_INIT_THREAD`/`THUMBNAIL_TASK`/`SHUTDOWN_DONE`。
- **3 个系统监控线程**：`fullscreen_monitor`（`SetWinEventHook` 事件驱动）、`power_monitor`（`WM_POWERBROADCAST` 事件驱动）、`explorer_restart_monitor`（`TaskbarCreated` 消息事件驱动）。
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
| ST-TD-014 | wallpaper.rs:482 | `// 详见 findings/03-rust-src-tauri.md ST-014` findings 路径失效 | 更新为 docs 文档路径 | ✅ 已清理：改为 `docs/05-优化文档/06-src-tauri应用层.md` ST-014 |
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

### 3.2 清理成果（2026-07-26，spec `cleanup-v6-src-tauri-tech-debt-2026-07-26`）

- **P0 立即清理（7/7 全部落实）**：ST-TD-001、ST-TD-021、ST-TD-014、ST-TD-016、ST-TD-024、ST-TD-019、ST-TD-018。
- **P1/P2 谨慎清理（12/12 全部落实）**：ST-TD-002、ST-TD-005、ST-TD-007、ST-TD-010、ST-TD-011、ST-TD-012、ST-TD-013、ST-TD-015、ST-TD-017、ST-TD-020、ST-TD-022、ST-TD-023。
- **P3 保留现状 + 补充注释（5/5）**：ST-TD-003、ST-TD-004、ST-TD-006、ST-TD-008、ST-TD-009。
- **验证结果**：`cargo build` 通过；`cargo test` 62 个测试通过（36 ignored）；`cargo clippy -D warnings` 零警告；Grep 验证无 `PauseReason::USER` / `向后兼容包装` / `v2.1 T-016` / `Bug #5 修复后` / `findings/03-rust-src-tauri` / `fix-wallpaper-preview-blank` / `v41-ST-` / `v5.0 X-PERF-` 残留（本次合并核验已复验通过）。
- **衍生收益**：跨模块共享校验（`mirrorstar-core::config::validation`）、`PauseReason` 精简（FLU/FULLSCREEN/BATTERY/TRAY 三生产变体）、注释体量累计减少约 200-300 行、术语统一。

### 3.3 与 v4.0/v5.0 文档的关联

**v4.0 已覆盖项**：ST-001~ST-018 均由 Section 2 覆盖。v6 不再重复 v4 的正确性问题，但识别出 v4 修复残留技术债（ST-TD-003/005/006/010~017 关联 ST-005/006/007/013/014/018 与 Bug #1-#7）。ST-014 失效 findings 路径已在 ST-TD-014 清理。

**v5.0 已覆盖项**：v5.0 X-PERF-NNN 标记（I-PERF-001/002/003/004/006/009/010、C-PERF-002/003/005、A-PERF-001/003）均已实施，标记噪音在 ST-TD-012 清理。

## 4. 优化机会与交集汇总

### 4.1 当前优化机会

1. **注释体量压缩**：12 源文件约 5630 行，v6-A/B 清理后已减少约 200-300 行；explorer.rs BOM 块（~30→9 行）、state.rs Bug #7 叙述（~30→15 行）已收敛，剩余文档化注释多为有效设计文档。
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

**核心结论**：src-tauri 应用层经 v3.x → v4.0 → v4.1 → v5.0 → v6 多轮修复后，工程质量与正确性已达高水平（49+ 单元测试、clippy 零警告、详尽的锁顺序/COM/shutdown 文档化）。v4 findings 17 项全部关闭；v6 技术债 24 项全部清理（8 项修复痕迹为最大类别，均已收敛）。