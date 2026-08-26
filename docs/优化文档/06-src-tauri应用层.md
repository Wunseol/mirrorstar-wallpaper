# src-tauri 应用层模块优化文档

> [← 返回索引](./README.md)

## 模块概要

- **模块路径**：`src-tauri/src/`（v3.5 实测：12 源文件 / 3109 行 + 3 测试文件 / 1515 行）
- **审查文件**：10 个（约 3,200 行）
  - `main.rs`（61 行）— 二进制入口，单实例互斥体（`ensure_single_instance`）
  - `lib.rs`（375 行）— 应用入口：COM 初始化、logging、WorkerW 预初始化、Tauri setup/tray/事件监听/shutdown
  - `state.rs`（625 行）— `AppState` + 19 个全局静态量 + `perform_shutdown_blocking` + 窗口辅助
  - `commands/mod.rs`（6 行）— 命令模块导出
  - `commands/wallpaper.rs`（882 行）— 13 个壁纸命令（add/remove/set/pause/resume/volume/scaling/speed 等）+ `validate_wallpaper_file_path` + `ComGuard`
  - `commands/config.rs`（73 行）— 3 个配置命令 + SEC-002 字段校验
  - `commands/system.rs`（124 行）— 7 个系统命令（explorer 重启、电源状态、文件对话框）
  - `platform/mod.rs`（8 行）— 平台模块导出
  - `platform/explorer.rs`（266 行）— Explorer 重启监控（`TaskbarCreated` 消息事件驱动）
  - `platform/fullscreen.rs`（473 行）— 全屏检测（`SetWinEventHook` + 精确匹配 + 矩形覆盖测试）
  - `platform/power.rs`（163 行）— 电源状态监控（`WM_POWERBROADCAST` 事件驱动）
  - `platform/workerw_check.rs`（53 行）— WorkerW 有效性兜底检查（5 分钟轮询 + `Notify` 即时唤醒）
- **核心结构**：`AppState`（`Arc<ConfigManager>` + `Arc<tokio::sync::Mutex<WallpaperEngine>>` + `Arc<Mutex<DesktopIntegrator>>` + `tray_paused: AtomicBool` + `tray_pause_resume_item: OnceLock<MenuItem>`）
- **24 个 Tauri 命令**：`get_wallpapers`/`add_wallpaper`/`generate_thumbnail`/`regenerate_thumbnails`/`remove_wallpaper`/`set_wallpaper`/`pause_wallpaper`/`resume_wallpaper`/`get_config`/`update_config`/`set_volume`/`toggle_mute`/`set_interaction_mode`/`toggle_interaction`/`get_displays`/`get_wallpaper_state`/`open_file_dialog`/`toggle_auto_start`/`get_auto_start_status`/`set_scaling_mode`/`set_speed`/`update_positions`/`check_desktop_status`/`quit_app`
- **19 个全局可变静态量**（state.rs）：`SHARED_ENGINE`/`SHARED_CONFIG`/`FULLSCREEN_WAS`/`EXPLORER_DESKTOP`/`TASKBAR_CREATED_MSG`/`POWER_WAS_ON_BATTERY`/`WIN_EVENT_HOOK`/`FULLSCREEN_MONITOR_RUNNING`/`FULLSCREEN_MONITOR_THREAD_ID`/`FULLSCREEN_MONITOR_THREAD`/`EXPLORER_MONITOR_RUNNING`/`EXPLORER_MONITOR_THREAD_ID`/`EXPLORER_MONITOR_THREAD`/`WORKERW_CHECK_RUNNING`/`WORKERW_CHECK_TASK`/`WORKERW_CHECK_NOTIFY`/`WORKERW_INIT_THREAD`/`THUMBNAIL_TASK`/`SHUTDOWN_DONE`
- **3 个系统监控线程**：`fullscreen_monitor`（`SetWinEventHook` 事件驱动）、`power_monitor`（`WM_POWERBROADCAST` 事件驱动，由 explorer.rs 转发）、`explorer_restart_monitor`（`TaskbarCreated` 消息事件驱动）
- **设计模式**：
  - **ComGuard RAII**（T16）：主线程 STA + spawn_blocking MTA 的 COM 初始化均通过 RAII guard 管理，Drop 时正确配对 `CoUninitialize`，RPC_E_CHANGED_MODE 正确跳过反初始化
  - **shutdown 流程**（Bug #6/T04/ST-002/ST-003/ST-014）：幂等守卫 + LIFO 清理顺序 + 3s engine 锁超时 + 全部后台线程/任务 join/abort + 5s 缩略图任务等待；`try_lock_with_timeout` 泛型设计支持无 Windows 环境的单元测试
  - **锁顺序约定**：state.rs 顶部详尽文档化 engine→desktop 锁顺序，逐命令审查锁获取路径，Win32 回调使用 try_lock 避免阻塞消息循环
  - **输入验证**（T08/T12/SEC-001/SEC-002）：壁纸路径校验覆盖绝对路径/路径遍历/文件存在/逐级符号链接检测；配置字段范围校验；音量/速度/缩放模式纯函数校验
  - **per-display 防竞态**（T15）：`DisplaySettingGuard` RAII guard 防止并发 `set_wallpaper` 同一 display 导致渲染器泄漏
  - **Win32 回调安全**：`SendWinEventHook` 的 unsafe Send/Sync 附带详尽论证；wndproc 使用 try_lock 非阻塞；`GetMessageW` 返回值 -1 显式 match（T02 修复）
  - **C-014/C-015 重启路径**：全屏/Explorer 监控支持二次调用 stop+start，先 take 旧句柄/线程 ID/JoinHandle 并 unhook/join，再重新 start
- **事件发射**：`wallpaper-added`/`wallpaper-updated`/`wallpaper-removed`/`wallpaper-state-changed`/`wallpaper-thumbnail-failed`/`desktop-status-changed`

## v4.0 审查发现（17 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/07-src-tauri.md`
> 严重级别分布：Critical 0 / High 2 / Medium 5 / Low 10
> 维度分布：逻辑 1 | 并发 4 | 资源 4 | 错误 2 | 性能 2 | 安全 3 | 可维护性 3

### 审查重点说明

src-tauri 应用层经过 v3.0→v3.5 共 5 轮修复（T01-T16、ST-001-ST-018、C-001-C-015、Bug #1-#7），整体工程质量较高，尤其在 ComGuard RAII、shutdown 流程、锁顺序约定、per-display 防竞态、Win32 回调安全方面表现优异。本次审查聚焦于遗留债务：(1) `add_wallpaper` 路径前缀字符串比较的边界缺陷；(2) `open_file_dialog` 超时后阻塞线程不可取消；(3) 错误类型 `DesktopIntegration` 被泛化用作多种非桌面集成错误的容器；(4) `workerw_check` 在异步任务中阻塞式获取 std Mutex。

### [ST-001] [High] [逻辑/安全] wallpaper.rs:295-298 — `add_wallpaper` 使用字符串前缀比较判断文件是否在受控目录，兄弟目录路径误判

**描述**：`add_wallpaper` 在判断壁纸文件是否已在受控目录（`$APPDATA/mirrorstar/`）下时，使用 `String::starts_with` 做字符串前缀比较，而非 `Path::starts_with` 做路径组件比较。当 `data_dir` 为 `C:\Users\X\AppData\Roaming\mirrorstar` 时，任何位于兄弟目录（如 `C:\Users\X\AppData\Roaming\mirrorstar-evil\file.mp4`、`C:\Users\X\AppData\Roaming\mirrorstar_backup\file.mp4`）的文件均会通过 `starts_with` 检查，被误判为"已在受控目录下"，跳过复制步骤。

**影响**：
- **数据完整性**：被误判的文件不会被复制到受控目录，配置中存储的 `file_path` 指向外部路径。若用户后续移动/删除/重命名该外部文件，壁纸将无法播放，且 `remove_wallpaper(delete_file=true)` 会删除外部文件（虽经路径校验，但非用户预期行为）。
- **asset scope 失效**：B01 修复收紧了 asset scope（仅 `$APPDATA/mirrorstar/**/*` 可通过 `asset://` 访问），但此分支跳过复制后，前端通过 `convertFileSrc` 加载该文件会因 scope 限制失败，导致预览异常。

**建议**：改用 `std::path::Path::starts_with` 做路径组件级比较。`Path::starts_with` 按 OS 路径分隔符分割组件后逐级比较，`mirrorstar-evil` 与 `mirrorstar` 是不同组件，不会误匹配。

```rust
let data_dir = ConfigManager::data_dir()?;
if std::path::Path::new(&file_path).starts_with(&data_dir) {
    file_path
} else {
    // 复制到受控目录
}
```

### [ST-002] [High] [资源管理] system.rs:73-93 — `open_file_dialog` 超时后 `spawn_blocking` 任务不可取消，阻塞线程泄漏

**描述**：`open_file_dialog` 使用 `tokio::task::spawn_blocking` 运行 `blocking_pick_file()`，并用 `tokio::time::timeout` 包装 10 分钟超时。但 `spawn_blocking` 返回的 `JoinHandle` 在超时后被 drop，**不会**中止底层阻塞任务——`spawn_blocking` 任务只能通过完成自然退出，无法被外部取消。超时后命令返回 `None`，但 `blocking_pick_file()` 仍在 tokio 阻塞线程池的一个线程上运行，直到用户关闭文件对话框。文件对话框是模态 Win32 窗口，若用户不关闭，线程将永久阻塞。

**影响**：
- 每次超时泄漏 1 个 tokio 阻塞线程池线程。若用户反复打开文件对话框但不关闭（10 分钟超时后再次打开），阻塞线程逐渐累积。
- tokio 阻塞线程池默认上限 512 线程，实际场景中用户不太可能触发 512 次未关闭的对话框，但这是设计层面的资源泄漏。
- 超时后用户看到命令返回 `None`（文件选择失败），但对话框仍显示在屏幕上，UX 不一致。

**建议**：`tauri-plugin-dialog` v2 的 `pick_file()` 是回调式 API，可在超时后通过 Tauri 窗口机制关闭对话框。或改为使用非阻塞的 `pick_file()` 回调 API + `tokio::sync::oneshot` 通道组合，超时时 drop sender 取消回调注册。若 `blocking_pick_file` 无法被取消，应在超时时记录 warn 并在文档中明确说明此限制，同时考虑降低超时上限（如 5 分钟）以减少线程占用时长。

### [ST-003] [Medium] [并发] workerw_check.rs:38-44 — `workerw_check` 异步任务中使用 `std::sync::Mutex::lock()` 阻塞 tokio worker 线程

**描述**：`start_workerw_check` 在 `tauri::async_runtime::spawn` 的异步任务中，通过 `desktop.lock()` 获取 `std::sync::Mutex` 守卫，并同步调用 `is_workerw_valid()` 与 `check_and_reinitialize()`。`std::sync::Mutex::lock()` 是阻塞式系统调用（`WaitForSingleObjectEx`），在 tokio 异步任务中调用会阻塞整个 tokio worker 线程。`check_and_reinitialize()` 内部涉及 `EnumWindows` 遍历所有顶层窗口查找 WorkerW，可能耗时数十毫秒。期间该 tokio worker 线程无法调度其他异步任务。

**影响**：
- 虽然检查间隔为 5 分钟，单次阻塞影响有限，但 `check_and_reinitialize` 在 WorkerW 失效时涉及窗口枚举 + `SendMessageTimeoutW` 通信，极端情况下（系统窗口数量多、响应慢）可能阻塞数百毫秒。
- 若 tokio runtime 仅配置少量 worker 线程（默认 = CPU 核心数），此阻塞可能延迟其他异步任务（如 `set_speed` fire-and-forget 任务、broadcast 通道订阅任务）的调度。
- 违反了 tokio 官方「不要在 async 任务中执行阻塞操作」的指导原则。

**建议**：将 `desktop.lock()` + 同步操作包装在 `tokio::task::spawn_blocking` 中，使阻塞操作运行在专用阻塞线程上。或保持当前实现但添加注释明确说明此处阻塞的可接受性（5 分钟间隔 + WorkerW 操作通常 <50ms）。

> ✅ **已修复于 v4.0 Wave 2C**（spec: `fix-v40-wave2c-src-tauri-medium-findings`）：采用方案 ②，添加注释明确说明 workerw_check 阻塞可接受性（5min 间隔 + WorkerW 操作通常 <50ms）。

### [ST-004] [Medium] [错误处理] wallpaper.rs/system.rs/config.rs 多处 — `DesktopIntegration` 错误变体被泛化用作多种非桌面集成错误的容器

**描述**：`MirrorStarError` 枚举提供了 `InvalidPath`、`InvalidConfig`、`InvalidArgument` 等语义化变体，但 src-tauri 命令层大量使用 `DesktopIntegration(String)` 作为通用错误容器，导致前端无法通过 error code 字段精确区分错误类型：

| 位置 | 实际错误语义 | 使用的变体 | 应使用的变体 |
|------|-------------|-----------|-------------|
| wallpaper.rs:262-268 | 不支持的文件类型 | `DesktopIntegration` | `InvalidArgument` |
| wallpaper.rs:307-312 | 无法提取文件名 | `DesktopIntegration` | `InvalidPath` |
| wallpaper.rs:762-770 | 音量越界 | `DesktopIntegration` | `InvalidArgument` |
| wallpaper.rs:775-783 | 速度越界 | `DesktopIntegration` | `InvalidArgument` |
| wallpaper.rs:795-799 | 未知缩放模式 | `DesktopIntegration` | `InvalidArgument` |
| system.rs:105-110 | 自启动启用失败 | `DesktopIntegration` | `IpcError` 或新增变体 |

**影响**：
- 前端错误处理精度降低，所有非路径/配置错误都被归为"桌面集成失败"。前端无法区分"用户输入错误"与"系统故障"，无法提供针对性的 UI 反馈（如：输入校验错误应高亮输入框，系统故障应显示 toast）。
- 错误日志中 `DesktopIntegration` 变体的消息文本混杂多种语义，排查时需阅读 message 字符串内容才能判断根因。
- `validate_volume`/`validate_speed`/`parse_scaling_mode` 已提取为纯函数，但错误变体未同步优化，改进仅覆盖了可测试性，未覆盖错误分类。

**建议**：将参数校验类错误改用 `InvalidArgument` 变体，对 `parse_scaling_mode`、`add_wallpaper` 文件类型判断等同理。需同步更新对应的单元测试（匹配 `InvalidArgument` 而非 `DesktopIntegration`）。

> ✅ **已修复于 v4.0 Wave 2C**（spec: `fix-v40-wave2c-src-tauri-medium-findings`）：`validate_volume`/`validate_speed`/`parse_scaling_mode` 及"无法提取文件名"等错误改用 `InvalidArgument` 变体，同步更新单元测试。

### [ST-005] [Medium] [并发/资源] wallpaper.rs:730-744 — `set_speed` fire-and-forget 任务未被 shutdown 跟踪，退出时可能在 engine 已 shutdown 后访问 engine

**描述**：`set_speed` 命令采用 fire-and-forget 模式（T01 修复引入），短暂持锁解析 display_id 后立即释放锁，再 `tauri::async_runtime::spawn` 后台任务执行 `engine.set_speed()`。但此 spawned 任务的 `JoinHandle` 被 drop（未保存到任何全局静态量）。`perform_shutdown_blocking` 不跟踪此任务。退出时 `try_lock_with_timeout(engine, 3s)` 获取 engine 锁后调用 `engine.shutdown()`。若 `set_speed` 后台任务在此期间或之后尝试 `engine_arc.lock().await`：

- **shutdown 期间**：`try_lock_with_timeout` 持有 engine 锁最多 3s。`set_speed` 任务 `.await` 等待锁。3s 超时后 shutdown 释放锁，`set_speed` 获取到锁，但 engine 已 shutdown（渲染器已终止），`set_speed` 内部 `wallpapers.get_mut(display_id)` 返回 `None`，no-op。安全但无意义。
- **shutdown 之后**：若 `set_speed` 任务在 `engine.shutdown()` 之后才获取到锁，engine 内部状态可能不一致。`set_speed` 调用 no-op。安全但理论上是 use-after-shutdown 语义。

实际影响有限，因为 `set_speed` 对不存在的 display_id 安全返回 no-op，且 tokio runtime 在 App 退出时 abort 所有 spawned 任务。但架构上，这是 shutdown 路径中未被管理的异步任务，与 `THUMBNAIL_TASK`/`WORKERW_CHECK_TASK` 的显式跟踪形成对比。

**影响**：退出期间 `set_speed` 后台任务可能竞争 engine 锁，延迟 shutdown 的 3s 超时获取；理论上若未来 `set_speed` 实现变化（如访问已释放资源），可能触发 UB。

**建议**：若 `set_speed` 的 IPC 写入通常很快（<100ms），可改为在命令层持锁同步执行（移除 fire-and-forget），与 `set_volume` 保持一致。若需保持 fire-and-forget，应将 JoinHandle 存入全局 `Mutex<Vec<JoinHandle>>`（类似 `THUMBNAIL_TASK`），`perform_shutdown_blocking` 中带短超时等待。

> ✅ **已修复于 v4.0 Wave 2C**（spec: `fix-v40-wave2c-src-tauri-medium-findings`）：采用方案 ①，`set_speed` 改为命令层同步执行（与 `set_volume` 一致），移除 fire-and-forget spawn。

### [ST-006] [Medium] [资源管理] state.rs:458-523 — shutdown 中缩略图任务可能在 `config_manager.flush()` 之后完成写入，导致缩略图路径丢失

**描述**：`perform_shutdown_blocking` 的清理顺序中，缩略图任务等待（5s 超时）在 `config_manager.flush()` 之前。但超时后未完成的任务**继续在后台执行**（`spawn_blocking` 任务即使 JoinHandle drop 也继续至完成）。超时的缩略图任务在 `flush()` 之后完成时，其 `config_manager_clone.update_thumbnail(...)` 写入的缩略图路径可能：

- 若 `update_thumbnail` 内部立即写入磁盘 → 写入在 flush 之后，但数据已持久化，下次启动可读到。无丢失。
- 若 `update_thumbnail` 内部缓冲写入（依赖 flush 刷盘）→ 写入在 flush 之后进入缓冲，但进程已退出，缓冲数据丢失。**缩略图路径丢失**，下次启动时该壁纸的 `thumbnail` 字段为空，需 `regenerate_thumbnails` 补救。

**影响**：
- 大视频文件（ffmpeg 抽帧耗时 >5s）的缩略图路径可能在退出时丢失，用户下次启动看到空白预览。`regenerate_thumbnails` 命令可修复，但需用户手动触发或前端检测到空缩略图后自动调用。
- 这是 T05 修复（Vec 收集所有任务）的遗留边界：T05 解决了"任务丢失"问题（Vec 不覆盖），但未解决"超时后写入丢失"问题。

**建议**：在 `flush()` 之后增加一次额外的 `flush()` 或在 `update_thumbnail` 内部确保同步写入磁盘（不依赖全局 flush）。或将缩略图任务超时从 5s 提高到 10s（平衡退出速度与数据完整性）。更彻底的方案是将 `update_thumbnail` 改为同步写入（`fsync` 确保持久化），使其不依赖 `flush()` 的刷盘时机。

> ✅ **已修复于 v4.0 Wave 2C**（spec: `fix-v40-wave2c-src-tauri-medium-findings`）：采用方案 ①，`perform_shutdown_blocking` 在原 `flush()` 之后增加一次额外的 `flush()` 兜底，确保超时后完成的缩略图写入能被持久化。

### [ST-007] [Medium] [安全] wallpaper.rs:222-240 — 符号链接检测未覆盖 Windows Junction Points（目录联接），存在绕过路径校验的潜在缺口

**描述**：`validate_wallpaper_file_path` 的 T08 修复通过逐级 `symlink_metadata` 检测符号链接组件。但 Windows 特有的 **Junction Points**（目录联接，通过 `fsutil` 或 `mklink /J` 创建）是一种 reparse point，`std::fs::symlink_metadata` 的 `file_type().is_symlink()` 在 Windows 上**可能不将 junction 识别为 symlink**（取决于 Rust 标准库版本与 Windows reparse point tag 的处理）。

Rust 标准库在 Windows 上通过 `GetFileAttributesW` 检测 `FILE_ATTRIBUTE_REPARSE_POINT`，再通过 `FindFirstFileW` 的 `dwReserved0` 判断 reparse tag。`IO_REPARSE_TAG_SYMLINK` 被识别为 symlink，但 `IO_REPARSE_TAG_MOUNT_POINT`（junction）的 `is_symlink()` 返回值在 Rust 不同版本中行为不一致（早期版本返回 false）。

**影响**：
- 若中间目录是 junction point 指向受控目录外，`is_symlink()` 可能返回 false，绕过 T08 的符号链接检测。攻击者可通过 junction 将受控目录内的子目录指向外部目录，使 `validate_wallpaper_file_path` 通过校验，但实际文件读取的是外部目录中的文件。
- 实际利用难度较高：需在受控目录（`$APPDATA/mirrorstar/`）内创建 junction，通常需要用户权限或恶意程序已入侵受控目录。属深度防御缺口而非直接漏洞。

**建议**：补充检测 `IO_REPARSE_TAG_MOUNT_POINT`（junction）。可通过 `windows` crate 的 `GetFileAttributesW` + `FindFirstFileW` 直接检查 reparse tag，或使用 `std::fs::canonicalize` 作为补充校验：若 `canonicalize(path)` 与 `path` 的规范化形式不一致，拒绝访问。或在注释中明确记录此限制与残留风险评估。

> ✅ **已修复于 v4.0 Wave 2C**（spec: `fix-v40-wave2c-src-tauri-medium-findings`）：采用方案 ②，`validate_wallpaper_file_path` 增加 `canonicalize` 校验，若 canonicalize 失败则拒绝访问路径。

### [ST-008] [Low] [性能] wallpaper.rs:697-699 — `set_scaling_mode` 在持有 engine 锁后才解析缩放模式字符串，解析失败时锁获取浪费

**描述**：`set_scaling_mode` 先获取 engine 锁（`state.wallpaper_engine.lock().await`），再调用 `parse_scaling_mode(&mode)?`（可能失败）。`parse_scaling_mode` 是纯字符串匹配，不依赖 engine 状态，但在 engine 锁获取之后才调用。若前端传入非法缩放模式（如 `"invalid"`），engine 锁被无意义地获取并释放，期间阻塞其他快速路径命令。

**影响**：实际影响极小：锁持有时间极短（parse_scaling_mode 是 O(1) 字符串匹配），且非法输入是异常路径。但在高频调用场景下（前端 bug 导致频繁传入非法值），会增加锁竞争。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（将 `parse_scaling_mode(&mode)?` 移到 `state.wallpaper_engine.lock().await` 之前，解析失败时直接返回 `Err` 不占用 engine 锁，避免阻塞其他快速路径命令；既有 `set_scaling_mode` 测试零修改通过，行为不变仅执行顺序调整）

**建议**：将 `parse_scaling_mode` 移到锁获取之前：

```rust
let scaling_mode = parse_scaling_mode(&mode)?;  // 先解析
let mut engine = state.wallpaper_engine.lock().await;
let display = resolve_display_id(display_id, &engine);
engine.set_scaling_mode(&display, scaling_mode)?;
```

### [ST-009] [Low] [性能] wallpaper.rs:850-908 — `regenerate_thumbnails` 所有缩略图生成在单个 `spawn_blocking` 任务中顺序执行

**描述**：`regenerate_thumbnails` 将所有待处理壁纸的缩略图生成放在一个 `spawn_blocking` 闭包中的 `for` 循环内顺序执行。若有 N 个壁纸需要重新生成缩略图，总耗时 = N × 单个生成时间。视频类型（ffmpeg 抽帧）单个可能耗时 1-3s，10 个视频壁纸需 10-30s，期间 `spawn_blocking` 线程被占用，命令不返回，前端可能超时。

**影响**：
- 批量重新生成时前端长时间无响应（命令未返回）。
- 单个 `spawn_blocking` 线程被长时间占用（但 tokio 阻塞线程池可容纳，不影响 async 调度）。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ②：保持顺序执行但每完成一项 emit `wallpaper-regenerate-progress` 事件，payload 含 `processed/success/failed/total` 字段供前端显示进度条；新增 `RegenerateProgressPayload` 结构体（`Clone + serde::Serialize`）；将 Ok arm 原 `failed += 1; continue` 重构为 if/else 以保证进度 emit 在所有路径执行；空列表不 emit；emit 失败时 `tracing::warn!` 不阻塞循环；既有测试零修改通过）

**建议**：可并行化处理：将每个壁纸的缩略图生成 spawn 为独立 `spawn_blocking` 任务，用 `futures::future::join_all` 或 `tokio::task::JoinSet` 收集结果。但需注意 ffmpeg 并发进程数限制（建议并发 2-4，避免 CPU 过载）。或保持顺序但定期 emit 进度事件（`wallpaper-regenerate-progress`）供前端显示进度条。当前实现可接受（属维护操作，不频繁），但若壁纸库增大应考虑优化。

### [ST-010] [Low] [可维护性] state.rs:465,508 — shutdown 超时魔法数（5s/3s）未提取为命名常量

**描述**：`perform_shutdown_blocking` 中使用了两个内联超时常量：`Duration::from_secs(5)`（缩略图任务等待）、`Duration::from_secs(3)`（engine 锁等待）。同文件中 `system.rs` 的 `FILE_DIALOG_TIMEOUT` 和 `fullscreen.rs` 的 `TITLE_BUF_LEN` 已提取为命名常量，但这两个 shutdown 超时仍为内联字面量。

**影响**：调整超时需在代码中搜索字面量，可能误改其他 `from_secs(5)` 或 `from_secs(3)`；超时值的语义（为何 5s？为何 3s？）未在常量名中体现。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（提取 `SHUTDOWN_THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(5)` 与 `SHUTDOWN_ENGINE_LOCK_TIMEOUT: Duration = Duration::from_secs(3)` 模块级常量，定义于 `state.rs` 顶部 `THUMBNAIL_TASK` 与 `SHUTDOWN_DONE` 静态之间，附文档注释说明两个超时的语义；`perform_shutdown_blocking` 中 `Duration::from_secs(5)` 替换为 `SHUTDOWN_THUMBNAIL_TIMEOUT`，`Duration::from_secs(3)` 替换为 `SHUTDOWN_ENGINE_LOCK_TIMEOUT`；既有测试零修改通过，行为不变）

**建议**：

```rust
const SHUTDOWN_THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_ENGINE_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
```

### [ST-011] [Low] [资源管理] main.rs:9-32 — 单实例互斥体句柄未调用 `CloseHandle`，依赖进程退出回收

**描述**：`ensure_single_instance` 中 `CreateMutexW` 返回的 `HANDLE` 被赋值给 `_mutex`，随后 `let _ = _mutex;` 丢弃变量名。注释说明 windows-rs 0.58 中 `HANDLE` 是 `Copy` 的 newtype，不实现 `Drop`，因此不会自动 `CloseHandle`。句柄在进程生命周期内始终打开但未被显式关闭。

**影响**：单实例互斥体需在整个进程生命周期内保持打开（否则互斥体被释放，失去单实例检测能力），因此"不关闭"是**正确行为**，非缺陷。进程退出时操作系统自动回收所有句柄，无实际泄漏。但从资源管理规范角度，应通过 RAII guard 显式管理句柄生命周期，使意图更清晰。

**建议**：当前实现正确且文档完善，无需修改。若追求代码规范性，可创建 `OwnedHandle` RAII wrapper 在 Drop 时 `CloseHandle`，但考虑到互斥体需进程生命周期存活，此改动仅是形式上的规范，无实际收益。

### [ST-012] [Low] [并发] lib.rs:188-201 — 状态变更订阅任务依赖 tokio runtime 关闭退出，未通过通道关闭实现优雅退出

**描述**：`setup` 中 spawn 的全局状态变更订阅任务持有 `engine` Arc 克隆，循环等待 `rx.recv().await`。注释说明任务退出的条件是 `broadcast sender drop`（WallpaperEngine drop 时）。但任务自身持有 `engine` Arc 克隆，形成**自引用**：engine 的最后一个 Arc 由任务持有 → engine 不 drop → sender 不释放 → `recv()` 不返回 `Err(Closed)` → 任务不退出。

实际退出依赖 `tauri::async_runtime` 在 App 退出时 abort 所有 spawned 任务（runtime 关闭强制取消）。这是隐式的运行时行为，而非显式的优雅退出通道。

**影响**：
- 正常退出路径（`app.exit(0)` → runtime shutdown）能正确终止任务，无实际泄漏。
- 但任务退出依赖 runtime 强制 abort，而非通道关闭的协作式退出。若未来 tokio/tauri runtime 行为变化（如延迟 abort），任务可能短暂残留并访问已 shutdown 的 engine。
- 自引用结构（任务持 Arc → Arc 保 engine 活 → engine 保 sender 活 → sender 阻止 recv 返回 Closed）是潜在的设计隐患。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ②：接受当前实现，在 `state.rs::perform_shutdown_blocking` 的 `WORKERW_CHECK_TASK` abort 块上方追加 ST-012 注释段，明确记录三要点：①订阅任务持有 `engine: Arc<WallpaperEngine>` 克隆形成自引用；②任务退出依赖 runtime 强制 abort 而非通道关闭协作式退出；③此为已评估并接受的设计权衡，避免引入 CancellationToken 复杂度；既有功能测试零修改通过，仅注释增强）

**建议**：在 `perform_shutdown_blocking` 中显式 drop engine（或将 Arc 计数降至任务之外无其他持有者）前，通过某种机制（如额外的 shutdown `Notify` 或 `CancellationToken`）通知订阅任务主动退出。或接受当前实现（runtime abort 保证退出），在注释中明确记录此自引用关系与 runtime abort 依赖。

### [ST-013] [Low] [可维护性] system.rs:21-32 — `check_desktop_status` 标记为 `async` 但函数体内无 `.await`，徒增调度开销

**描述**：`check_desktop_status` 函数为 `async fn` 但内部仅同步操作（`std::sync::Mutex::lock` + 同步方法调用），无任何 `.await`。Tauri v2 的 async 命令会被调度到 tokio runtime 执行，相比 sync 命令多一次任务调度开销。

**影响**：每次调用多一次 tokio 任务调度（微秒级），实际影响可忽略。`check_desktop_status` 调用频率低（用户手动触发或前端轮询），无性能问题。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ②：保留 `async` 标注，在 `system.rs::check_desktop_status` 函数上方追加 ST-013 注释段说明三要点：①当前函数体内无 `.await`，async 标注带来轻微调度开销；②保留 async 前瞻性，未来若 `desktop` 改用 tokio Mutex 可平滑过渡；③不改为 sync 以避免未来重复修改签名；既有功能测试零修改通过，仅注释增强）

**建议**：可改为 `pub fn check_desktop_status(...) -> Result<...>`（sync 命令），Tauri 会在 IPC 线程直接执行。但若未来可能改为异步操作（如 tokio Mutex），保持 async 前瞻性也可接受。

### [ST-014] [Low] [可维护性] explorer.rs:1 — 文件首行包含 UTF-8 BOM 字符

**描述**：`platform/explorer.rs` 文件第一行以 UTF-8 BOM（`﻿`，U+FEFF）开头。其他文件（如 `lib.rs`、`state.rs`）均无 BOM。Rust 源文件不需要 BOM，BOM 在某些工具链中可能导致问题（如 shell 脚本处理、diff 噪音）。

**影响**：无功能影响，Rust 编译器正确处理 BOM。但与项目其他文件风格不一致，可能在 code review 或合并时产生疑惑。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（实测 `explorer.rs` 首 3 字节为 `117 115 101`（ASCII "use"），UTF-8 BOM 已不存在，标记为"已自动修复"——可能在前序 Wave 的某次提交中由编辑器自动移除；无代码改动，仅在文档中标记状态）

**建议**：移除 BOM 字符，使文件编码与项目其他 Rust 文件一致（UTF-8 without BOM）。

### [ST-015] [Low] [安全] wallpaper.rs:190-243 — `validate_wallpaper_file_path` 存在 TOCTOU 竞态：校验与使用之间文件可被替换

**描述**：`validate_wallpaper_file_path` 校验路径（绝对/无遍历/存在/无符号链接）后，调用方在后续操作（`add_wallpaper` 的文件复制、`set_wallpaper` 的渲染器创建）中再次访问文件。在校验与使用之间存在时间窗口（TOCTOU），攻击者可在此窗口内将文件替换为符号链接或删除后重建为恶意文件：

- `add_wallpaper`：校验 → `detect_wallpaper_type` → `tokio::fs::metadata` → `tokio::fs::copy`。copy 之间文件可能被替换为符号链接，copy 跟随符号链接复制外部文件。
- `set_wallpaper`：校验 → `create_and_play_renderer`。mpv 打开文件时可能读取到已被替换的文件。

**影响**：
- 实际利用难度高：攻击者需在毫秒级窗口内替换文件，且文件路径已在用户明确选择后传入。
- T08 的逐级符号链接检测已大幅缩小攻击面（中间目录符号链接被拒绝），仅最终文件级的 TOCTOU 仍存在。
- 这是基于文件系统校验的固有局限，非代码缺陷。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ③：接受当前 T08 + SEC-001 多层防御，在 `wallpaper.rs::validate_wallpaper_file_path` 函数文档注释中追加 ST-015 风险评估段，明确四要点：①残留 TOCTOU 竞态（校验与使用之间文件可被替换为符号链接）；②现有 T08（symlink_metadata 逐级检查）+ SEC-001（canonicalize junction 检测）+ 路径遍历拒绝构成多层防御；③残留风险可接受（壁纸路径来自用户主动选择，非外部攻击者可控输入）；④不实施 `tokio::fs::copy` 后重新校验或 `FILE_FLAG_OPEN_REPARSE_POINT` 加固，避免引入 Windows 平台特定 API 依赖与额外 IO 开销；既有测试零修改通过，仅注释增强）

**建议**：在 `add_wallpaper` 的 `tokio::fs::copy` 之后对目标文件重新校验，或在 `copy` 时使用 `O_NOFOLLOW`（Windows 上通过 `FILE_FLAG_OPEN_REPARSE_POINT` 拒绝跟随 reparse point）。对于 `set_wallpaper`，可在 `create_and_play_renderer` 前用 `canonicalize` 获取真实路径并校验仍在受控目录内。当前 T08 + SEC-001 的多层防护已提供合理的深度防御，此 TOCTOU 标记为 Low 供风险评估参考。

### [ST-016] [Low] [并发] wallpaper.rs:445-450 — `THUMBNAIL_TASK` Vec 仅在 push 前 retain 清理已完成任务，大量并发长任务时 Vec 无上限增长

**描述**：`THUMBNAIL_TASK`（T05 修复后改为 `Mutex<Vec<JoinHandle>>`）在 push 前调用 `slot.retain(|h| !h.is_finished())` 清理已完成任务。`retain` 仅清理 `is_finished()` 返回 true 的任务。若用户连续添加大量视频壁纸（ffmpeg 抽帧耗时 >1s），多个缩略图任务同时运行，均未完成，Vec 持续增长。Vec 中的 `JoinHandle` 占用内存（每个约 40-80 字节），100 个并发任务约 4-8KB，实际影响极小。

**影响**：
- 内存增长可忽略（除非用户批量添加上千个视频壁纸）。
- `perform_shutdown_blocking` 中 5s 超时等待所有任务，大量并发任务可能导致超时后多数任务未完成（但 spawn_blocking 任务继续后台执行至完成）。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ①：在 `wallpaper.rs` 顶部新增 `const MAX_THUMBNAIL_TASKS: usize = 50;` 模块级常量，附文档注释说明容量上限设计意图；`add_wallpaper` 中 `THUMBNAIL_TASK.lock()` 块内 push 后追加容量上限处理：超出上限时优先通过 `slot.iter().position(|h| h.is_finished())` 找到最旧已完成 JoinHandle 并 `slot.remove(idx)`；全部进行中时不强制截断（避免任务丢失），记录 `tracing::warn!(len = slot.len(), "THUMBNAIL_TASK 超出上限 50，全部进行中，不强制截断")`；既有测试零修改通过，容量上限不影响正常路径）

**建议**：可添加容量上限（如 50），超过时 `retain` 后若仍超限则 drop 最旧的已完成 JoinHandle（已完成的任务不受影响，仅释放 Vec 中的 handle 跟踪）。或限制并发缩略图任务数（信号量）。当前实现可接受。

### [ST-017] [Low] [错误处理] config.rs:56-63 — `update_config` 对 engine 使用 `try_lock` 更新 GIF 策略，但 `set_gif_memory_strategy` 若为 `&self` 则无需 engine 锁

**描述**：`update_config` 中 `if let Ok(engine) = state.wallpaper_engine.try_lock() { engine.set_gif_memory_strategy(...) }`，锁忙时跳过实时更新并记录 warn。注释说明 `set_gif_memory_strategy` 已改为 `&self + 内部 Mutex`。若如此，该方法不需要持有 engine tokio Mutex——它通过自身内部 Mutex 保证线程安全。但此处仍 `try_lock` engine 锁，锁忙时跳过实时更新。

**影响**：
- 功能正确：锁忙时跳过，下次创建壁纸时从配置读取正确值。
- 但 `try_lock` 是不必要的：若 `set_gif_memory_strategy` 是 `&self`，可直接通过 `Arc` 引用调用，无需 engine tokio Mutex。
- 当前实现的副作用：engine 锁忙时 GIF 策略更新被延迟，用户改配置后已播放的壁纸可能不会立即应用新策略（需等下次创建壁纸）。

**修复状态**：✅ 已修复于 v4.0 Wave 3H（采纳方案 ②：接受当前实现，在 `config.rs::update_config` 函数的 `try_lock` 块上方追加 ST-017 注释段说明四要点：①`set_gif_memory_strategy` 改为 `&self + 内部 Mutex` 后，理论上无需 engine 锁，可通过 `Arc<WallpaperEngine>` 直接调用（方案 ①）；②保留 `try_lock` 路径避免重构 engine 公共 API（方案 ① 涉及 engine 重构，复杂度中）；③锁忙时跳过实时更新可接受：配置已写入 ConfigManager，下次创建壁纸时从配置读取正确值；④此为已评估并接受的设计权衡；既有 T09 注释段保留（描述历史背景），ST-017 注释段追加在其后作为正式设计权衡说明；既有测试零修改通过，仅注释增强）

**建议**：若 `WallpaperEngine` 可暴露 `&self` 的 `set_gif_memory_strategy`（通过 `Arc<WallpaperEngine>` 直接调用，不经过 tokio Mutex），则移除 `try_lock`，改为直接调用。若 engine 的 `&self` 方法需通过 `Arc` 访问而当前 `wallpaper_engine` 字段是 `Arc<tokio::sync::Mutex<WallpaperEngine>>`，则需重构为 `Arc<WallpaperEngine>` + 内部 Mutex 模式。当前实现可接受，供架构演进参考。

## v3.x 已修复问题

### v3.5 已修复 findings（T01-T16，16 项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| T01 | Medium | `set_speed` 命令在持有 engine tokio Mutex 锁期间 `.await`，造成命令串行化延迟 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 改为 fire-and-forget — ⚠️ v4.0 ST-005 发现 fire-and-forget 任务未被 shutdown 跟踪 |
| T02 | High | `fullscreen.rs` 消息循环仍使用 `while GetMessageW(...).as_bool()`，未修复 N-002 bug（`BOOL(-1).as_bool() == true`，GetMessageW 返回 -1 时误判为消息到达） | ✅ 已修复（fix-v35-high-findings-2026-07-12）— 改为 `loop { match ret.0 { 0 \| -1 => break, _ => {} } }` |
| T03 | Medium | `pause_wallpaper`/`resume_wallpaper` 使用 `display_id.unwrap_or_default()`，与其他 5 个命令的 `resolve_display_id` 回退行为不一致 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 统一使用 `resolve_display_id` |
| T04 | Medium | `perform_shutdown_blocking` 中 `engine.blocking_lock()` 无超时等待，shutdown 可能无限阻塞 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 改为 `try_lock_with_timeout(engine, 3s)` |
| T05 | Low | `THUMBNAIL_TASK` 全局 `Mutex<Option<JoinHandle>>` 连续调用时覆盖旧 handle，shutdown 只等待最后一个 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 改为 `Mutex<Vec<JoinHandle>>` — ⚠️ v4.0 ST-006 发现超时后写入仍可能丢失；ST-016 发现 Vec 无上限增长 |
| T06 | Low | 托盘菜单文本更新每次 `std::thread::spawn` 新线程，频繁切换创建大量短命线程 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 改用 `spawn_blocking` |
| T07 | Low | `workerw_check.rs` 重新初始化后不 emit 事件通知前端，前端无法感知 WorkerW 状态变化 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— emit `desktop-status-changed` |
| T08 | Low | `validate_wallpaper_file_path` 仅检查字面 `..` 与文件存在，不解析符号链接，指向敏感文件的符号链接可绕过校验 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 逐级 `symlink_metadata` 检测 — ⚠️ v4.0 ST-007 发现未覆盖 Windows Junction Points |
| T09 | Low | `update_config` 持有 engine 锁调用 `set_gif_memory_strategy`，阻塞快速路径命令 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— `set_gif_memory_strategy` 改为 `&self + 内部 Mutex` — ⚠️ v4.0 ST-017 发现仍残留不必要的 `try_lock` |
| T10 | Low | `is_foreground_fullscreen` 标题与类名缓冲区仅 256 字符，超长标题窗口被截断比较 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 提取 `TITLE_BUF_LEN` 常量 + 注释 |
| T11 | Low | `explorer.rs` `EXPLORER_DESKTOP.set(desktop)` 二次调用返回 Err 被 `let _ =` 丢弃，接口契约不清晰 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 改为 match + debug 日志 |
| T12 | Low | `set_on_config_changed` 回调在监视线程中执行，回调 panic 会导致热重载失效 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 包裹 `catch_unwind` |
| T13 | Low | `power.rs` `GetSystemPowerStatus` 失败时静默退出，无日志 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 增加 warn 日志 |
| T14 | Low | `check_desktop_status` 返回值 `was_reinitialized` 语义错误（实际为"进入时 WorkerW 是否无效"） | ✅ 已修复（fix-v35-low-findings-2026-07-13）— `check_and_reinitialize` 返回实际重初始化 bool |
| T15 | Low | `set_wallpaper` 3 阶段锁模式中阶段 2 不持锁，同 display 并发时 renderer 可能泄漏 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— `DisplaySettingGuard` per-display RAII |
| T16 | Low | 主线程 COM 初始化无 RAII guard，setup 失败 panic 时 `CoUninitialize` 永不调用 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 引入主线程 `ComGuard` |

### v3.2 已修复 findings（ST-001~ST-018，14 修复 + 4 TODO）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| ST-001 | P3 | `power.rs` SHARED_ENGINE 未设置时仍更新电源状态 | ✅ 已修复 |
| ST-002 | P3 | `lib.rs` WorkerW 预初始化线程未保存 JoinHandle（T-014 遗留） | ✅ 已修复 |
| ST-003 | P3 | `workerw_check.rs` 未保存 JoinHandle | ✅ 已修复 |
| ST-004 | P3 | `fullscreen.rs` + `power.rs` pause/resume 逻辑 DRY 违规（6 处重复） | ✅ 已修复 |
| ST-005 | P3 | `state.rs` `perform_shutdown_blocking` 中 PostThreadMessageW/join 逻辑重复 | ✅ 已修复 |
| ST-006 | P3 | `main.rs` `ensure_single_instance` CreateMutexW 失败时仍返回 true | ✅ 已修复 |
| ST-007 | P3 | `wallpaper_flow.rs` 27 个 `#[ignore]` 测试在 CI 中不执行 | ✅ 已修复 |
| ST-008 | P3 | `state.rs` 测试辅助 `create_test_config_manager` 使用 `ConfigManager::new()`（T-017 遗留） | ✅ 已修复 |
| ST-009 | P3 | `validate_wallpaper_file_path` 错误类型不一致 | ✅ 已修复 — ⚠️ v4.0 ST-004 发现 `DesktopIntegration` 错误变体仍被泛化使用 |
| ST-010 | P3 | `tauri.conf.json` assetProtocol.scope 包含 `$HOME/**/*` | ✅ 已修复 |
| ST-011 | P4 | `main.rs` `Box::leak` 不必要 | ✅ 已修复 |
| ST-012 | P4 | `fullscreen.rs` `is_foreground_fullscreen` 窗口标题匹配不精确 | ✅ 已修复 |
| ST-013 | P4 | `explorer.rs` `GetModuleHandleW` `unwrap_or_default` 静默吞错 | ✅ 已修复 |
| ST-014 | P4 | `add_wallpaper` 缩略图生成 fire-and-forget | ⚠️ TODO — v4.0 ST-006/ST-016 进一步细化 |
| ST-015 | P4 | `open_file_dialog` `blocking_pick_file` 可能永久阻塞 | ⚠️ TODO — v4.0 ST-002 升级为 High 发现超时线程泄漏 |
| ST-016 | P4 | `tauri.conf.json` CSP 允许 `'unsafe-inline'` for style-src | ⚠️ TODO（前端模块） |
| ST-017 | P4 | `wallpaper_flow.rs` 部分测试仅模拟命令层逻辑无实际断言价值 | ⚠️ TODO（ST-007 部分缓解） |
| ST-018 | P4 | `explorer.rs` WM_DESTROY PostQuitMessage 注释缺失 | ✅ 已修复 |

### v1.0~v2.1 已修复问题（9 项）

| 问题 | 状态 | 修复说明 |
|------|------|---------|
| 冗余 WorkerW 监控 | ✅ 已修复 | 30s 轮询改为 5 分钟兜底（workerw_check.rs） |
| 10 个全局可变静态量 | ✅ 已修复 | 合并 PAUSE_SENDERS/CONFIG，TRAY 字段移入 AppState（v3.2 进一步演化为 19 个含线程跟踪与退出守卫） |
| 电源监控使用轮询 | ✅ 已修复 | 改为 `WM_POWERBROADCAST` 事件驱动（explorer.rs 转发） |
| COM 初始化错误处理脆弱 | ✅ 已修复 | 使用 `RPC_E_CHANGED_MODE` 常量替代硬编码值，match + tracing |
| 窗口关闭 50ms 魔数竞态 | ✅ 已修复 | 添加 `MAIN_WINDOW_CLOSING` AtomicBool 标志位 |
| UUID 截断碰撞风险 | ✅ 已修复 | 使用完整 UUID（36 字符） |
| async 函数中同步 IO | ✅ 已修复 | 使用 `tokio::fs::metadata` |
| shutdown 错误被忽略 | ✅ 已修复 | `shutdown()` 返回 `()` 而非 `Result`，API 设计变更 |
| `unwrap` 可能 panic | ✅ 已修复（v3.2） | `VolumeControl::new` 失败时改用 `new_disabled()` 优雅降级 |

## 优化目标与方案

### v4.0 优先修复（High，2 项）

1. **ST-001 `add_wallpaper` 路径前缀比较**：将 `String::starts_with` 改为 `std::path::Path::starts_with` 做路径组件级比较，避免兄弟目录（如 `mirrorstar-evil`、`mirrorstar_backup`）误判为受控目录。
2. **ST-002 `open_file_dialog` 超时线程泄漏**：改用 `tauri-plugin-dialog` v2 回调式 `pick_file()` API + `tokio::sync::oneshot` 通道，超时时可取消；或降低超时上限（5 分钟）并在文档中明确说明 `spawn_blocking` 不可取消的限制。

### v4.0 系统性修复（Medium，5 项）

3. **ST-003 `workerw_check` 阻塞 tokio worker**：将 `desktop.lock()` + 同步操作包装在 `tokio::task::spawn_blocking` 中，或添加注释明确说明此处阻塞的可接受性（5 分钟间隔 + WorkerW 操作通常 <50ms）。
4. **ST-004 `DesktopIntegration` 错误变体泛化**：将参数校验类错误（文件类型/音量越界/速度越界/未知缩放模式）改用 `InvalidArgument` 变体，文件名提取改用 `InvalidPath`，同步更新单元测试。
5. **ST-005 `set_speed` fire-and-forget 未跟踪**：改为命令层持锁同步执行（与 `set_volume` 一致），或将 JoinHandle 存入全局 `Mutex<Vec<JoinHandle>>` 供 shutdown 等待。
6. **ST-006 缩略图任务超时后写入丢失**：在 `flush()` 之后增加额外 `flush()`，或将 `update_thumbnail` 改为同步写入（`fsync`），或将超时从 5s 提高到 10s。
7. **ST-007 Junction Points 检测**：补充检测 `IO_REPARSE_TAG_MOUNT_POINT`，或使用 `canonicalize` 作为补充校验，或在注释中明确记录此限制与残留风险评估。

### v4.0 渐进优化（Low，10 项）

8-17. `set_scaling_mode` 锁顺序优化（ST-008）、`regenerate_thumbnails` 并行化/进度事件（ST-009）、shutdown 超时魔法数提取常量（ST-010）、单实例互斥体 RAII 规范化（ST-011，可选）、状态订阅任务显式退出通道（ST-012）、`check_desktop_status` 改为 sync 命令（ST-013）、explorer.rs 移除 BOM（ST-014）、TOCTOU 风险评估记录（ST-015）、`THUMBNAIL_TASK` Vec 容量上限（ST-016）、`update_config` 移除不必要 `try_lock`（ST-017）。
