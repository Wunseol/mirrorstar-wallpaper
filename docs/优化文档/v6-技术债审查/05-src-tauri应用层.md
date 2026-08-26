# v6.0 技术债审查 - src-tauri 应用层

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 模块路径：`src-tauri/src/` | 源文件数：12

## 1. 当前状态摘要

### 1.1 模块职责

`src-tauri` 应用层是镜星壁纸的 Windows 客户端壳，承担五项核心职责：

1. **Tauri 命令层**：通过 `#[tauri::command]` 把 24 个前端可调用命令（壁纸生命周期 / 配置管理 / 系统控制）封装为对 `mirrorstar-core`（`ConfigManager` / `WallpaperEngine` / `DesktopIntegrator`）的薄封装。
2. **Win32 平台集成**：启动三类后台监控线程——全屏检测（`SetWinEventHook`）、Explorer 重启监控（`TaskbarCreated` 消息窗口）、WorkerW 兜底检查（5 分钟间隔）；处理 `WM_POWERBROADCAST` 电源事件。
3. **进程生命周期管理**：`run()` 顺序初始化 COM（STA）→ DesktopIntegrator → WorkerW 预初始化线程 → VolumeControl → WallpaperEngine → ConfigManager → 监控线程 → Tauri Builder；`perform_shutdown_blocking` 按 LIFO 顺序 + 幂等守卫 + 多个超时上界统一清理。
4. **前端事件桥接**：通过 `app.emit(...)` 把后端状态变更（壁纸增删改 / 状态切换 / 桌面状态 / 配置变更 / 缩略图进度）通知前端；订阅 `WallpaperEngine::global_state_changed` broadcast 通道统一转发 `wallpaper-state-changed`。
5. **自定义协议**：注册 `wpfile://` URI scheme handler，支持 HTTP Range / HEAD 请求，绕过 Tauri asset protocol scope 限制直接读取壁纸文件。

该模块是 v3.x → v4.0 → v4.1 → v5.0 多轮修复的重灾区，积累了大量 `Bug #N` / `ST-NNN` / `v41-ST-NNN` / `v5.0 X-PERF-NNN` 历史标记，是本次技术债审查的重点模块。

### 1.2 文件清单

#### commands/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 95 | 命令模块导出（`pub use config::*; pub use system::*; pub use wallpaper::*;`）+ v41-ST-016 命令注册风格文档化 |
| config.rs | 166 | `get_config` / `update_config` / `generate_thumbnail` 3 个命令 + SEC-002 `validate_config_fields` 字段范围校验 + v41-ST-010 防抖/并发策略文档化 |
| system.rs | 255 | `get_displays` / `check_desktop_status` / `set_interaction_mode` / `toggle_interaction` / `quit_app` / `open_file_dialog` / `toggle_auto_start` / `get_auto_start_status` 8 个命令 + ST-002 文件对话框超时（5min）+ 4 个 ST-002 单元测试 |
| wallpaper.rs | 1969 | `get_wallpapers` / `add_wallpaper` / `remove_wallpaper` / `set_wallpaper` / `pause_wallpaper` / `resume_wallpaper` / `set_volume` / `toggle_mute` / `get_wallpaper_state` / `set_scaling_mode` / `set_speed` / `update_positions` / `regenerate_thumbnails` 13 个命令 + `ComGuard` RAII + `DisplaySettingGuard` 防竞态 + `validate_wallpaper_file_path` / `validate_volume` / `validate_speed` / `parse_scaling_mode` 纯函数 + 18 个单元测试 |

#### platform/
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 8 | 平台模块导出（3 个 `pub(crate) use`）|
| explorer.rs | 329 | `start_explorer_restart_monitor`：HWND_MESSAGE 消息窗口 + `explorer_monitor_wndproc` 处理 TaskbarCreated / WM_POWERBROADCAST / WM_DESTROY + v41-ST-008 UTF-8 BOM 处理策略文档化（约 50 行注释）+ C-014/C-015 二次调用 stop+start |
| fullscreen.rs | 563 | `start_fullscreen_monitor`：`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + `foreground_event_callback` + `is_foreground_fullscreen` + `is_self_window_title` / `is_system_window_class` / `is_rect_covering_monitor` 3 个纯函数 + v41-ST-011 算法文档化 + 11 个纯函数单元测试 |
| power.rs | 235 | `handle_power_status_change`：`GetSystemPowerStatus` + `interpret_ac_line_status` + 电池↔AC 切换暂停/恢复 + v41-ST-012 电源事件处理文档化（约 70 行注释）+ 4 个 ACLineStatus 单元测试 |
| workerw_check.rs | 141 | `start_workerw_check`：5 分钟 interval + `WORKERW_CHECK_NOTIFY` 即时唤醒 + ST-003 JoinHandle 保存 + T07 emit `desktop-status-changed` + v41-ST-005 async 锁使用场景文档化（约 50 行注释）|

#### 顶层
| 文件 | 行数 | 主要内容 |
|---|---|---|
| state.rs | 1035 | `AppState` 结构体 + 19 个全局静态量（Task 9.2 保守策略全部保留文档化）+ `try_pause_all_fast` / `try_resume_all_fast`（ST-004 抽取）+ `signal_thread_exit` / `join_monitor_thread`（ST-005 抽取）+ `try_lock_with_timeout<T>`（T04）+ `perform_shutdown_blocking`（Bug #6/T04/ST-002/ST-003/ST-006/ST-014）+ `create_or_show_main_window` + 12 个单元测试 |
| lib.rs | 773 | `run()` 入口：日志 + COM STA + WorkerW 预初始化 + VolumeControl + Engine + Config + 监控线程 + Tauri Builder + wpfile:// 协议 handler（Range/HEAD 支持）+ setup 闭包（SHARED_APP_HANDLE / 状态订阅 / 配置回调 / 托盘菜单 / WorkerW check）+ v41-ST-014 setup 顺序与失败回滚文档化（约 115 行注释）|
| main.rs | 61 | `main()` + `ensure_single_instance()`（CreateMutexW 单实例互斥体）+ ST-011 windows-rs 0.58 句柄无 Drop 文档化 |

### 1.3 测试覆盖

- **单元测试**：分布在 `commands/system.rs`（4 个 ST-002 超时机制测试）、`commands/wallpaper.rs`（18 个：路径校验 / DisplaySettingGuard / THUMBNAIL_TASK Vec / validate_volume/speed/parse_scaling_mode / ST-007 junction / ST-005 同步执行 / v41-ST-009 越界校验）、`platform/fullscreen.rs`（11 个纯函数测试）、`platform/power.rs`（4 个 ACLineStatus 测试）、`state.rs`（12 个：SHUTDOWN_DONE 守卫 / T04 try_lock_with_timeout / ST-006 flush 兜底）。共约 49 个单元测试，多数为纯逻辑测试可在任意平台 CI 执行。
- **集成测试**：`tests/config_flow.rs`（6 个 ConfigManager 增删改流程测试，纯逻辑无 #[ignore]）、`tests/wallpaper_flow.rs`（约 25 个测试，多数标记 `#[ignore]` 需 Windows COM/音频环境，仅 4 个纯逻辑测试 CI 可执行）、`tests/common/mod.rs`（MockRenderer + 测试辅助函数）。
- **测试盲区**：`create_or_show_main_window` / `wpfile://` handler / Tauri Builder setup 闭包 / Win32 回调路径无法单元测试，依赖手动端到端验证。

## 2. 技术债清单

### 2.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-001 | wallpaper.rs:485-490 + 525-527 | `add_wallpaper` 在第 485-490 行用 `if matches!(wallpaper_type, Image\|Gif\|Video)` 外层过滤掉 Web 类型，进入块后内层 `match wallpaper_type_spawn` 仍写了 `WallpaperType::Web => { return; }` 分支（525-527）。该分支永远不可达——外层 `matches!` 已保证进入块时类型只能是 Image/Gif/Video。对比 `regenerate_thumbnails`（wallpaper.rs:1119）的 `WallpaperType::Web => continue` 是可达的（无外层过滤），两处 Web 处理的可达性不对称。 | 无功能影响（防御性死代码）。维护者可能误以为 Web 类型会进入该分支而修改逻辑。编译器不警告（match 穷尽性要求覆盖所有变体）。 | 两种方案任选：① 删除 525-527 行 Web 分支并在外层 `matches!` 处补注释说明 Web 已被过滤；② 保留分支但加 `unreachable!("Web 已在外层 matches! 过滤")` 显式标注不可达。推荐方案 ② 以保留 match 穷尽性。 | 低 |
| ST-TD-002 | tests/common/mod.rs:10, 66-80 | `MockRenderer` 的 `pub fn was_played` / `was_terminated` / `speed` / `scaling_mode` 4 个访问器方法在整个 `src-tauri/tests/` 目录中无任何调用点（Grep 验证仅声明处命中）。文件顶部 `#![allow(dead_code)]`（第 10 行）显式抑制了 dead_code 警告，注释说明"每个集成测试文件作为独立 crate 编译，只会使用 common 模块的部分函数"——但这 4 个方法在**所有**测试文件中均未使用。 | 无功能影响。增加 MockRenderer 表面积，维护者可能误以为这些方法有对应测试覆盖。 | 删除 `was_played` / `was_terminated` / `speed` / `scaling_mode` 4 个未使用方法及对应字段 `played` / `terminated` / `speed` / `scaling_mode`（如字段亦无内部使用）。删除后可考虑收紧 `#![allow(dead_code)]` 范围到具体项。 | 低 |

### 2.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-003 | wallpaper.rs:245-330 | `validate_and_get_metadata`（v5.0 I-PERF-003 引入，返回 `Metadata` 供 `add_wallpaper` 复用）与 `validate_wallpaper_file_path`（326-330，"向后兼容包装，委托给 `validate_and_get_metadata` 并丢弃返回的 metadata"）并存。`validate_wallpaper_file_path` 仅 3 个内部调用点（config.rs:157 `generate_thumbnail`、wallpaper.rs:357 `validate_path_within_data_dir`、wallpaper.rs:660 `set_wallpaper`），无外部 API 稳定性约束——"向后兼容"的"后"是 v5.0 之前的内部调用方。 | 轻微：多一层间接，每个调用方多一次 `.map(\|_| ())`。读代码者需跳转两层才能理解校验逻辑。 | 两种方案任选：① 保留现状（3 个调用点 justify 命名 helper，且 `validate_path_within_data_dir` 内部调用时确实不需要 metadata）；② 让 3 个调用方直接调 `validate_and_get_metadata(path).await.map(\|_| ())`，删除 `validate_wallpaper_file_path`。推荐方案 ①（保留），但把 doc 中"向后兼容包装"措辞改为"丢弃 metadata 的便捷封装"，避免暗示存在外部兼容承诺。 | 低 |
| ST-TD-004 | state.rs:493-498 | `try_lock_with_timeout<T>` 是泛型函数（`mutex: &Arc<tokio::sync::Mutex<T>>`），但生产代码仅有一处实例化（state.rs:694，`T = WallpaperEngine`）。泛型化的唯一理由是单元测试用 `tokio::sync::Mutex<()>` 验证超时行为（见 state.rs:480-498 doc 注释"泛型设计：不绑定具体类型 WallpaperEngine，便于在单元测试中用 `tokio::sync::Mutex<()>` 验证超时行为"）。 | 无功能影响。泛型签名略增阅读复杂度，但 doc 已充分说明理由。 | 保留现状（testability-driven generality 是合理权衡）。仅记录为"冗余抽象"维度的事实项，无需清理。 | 低 |

### 2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-005 | config.rs:56-82 vs wallpaper.rs:908-927 | **音量与速度的范围校验逻辑重复实现两处**：<br>① `config.rs::validate_config_fields`（update_config 命令调用）校验 `config.audio.volume`（[0.0, 1.0] 有限值，58-62 行）与 `config.video.speed`（(0.0, 10.0] 有限值，65-69 行）；<br>② `wallpaper.rs::validate_volume`（set_volume 命令调用，908-915）与 `validate_speed`（set_speed 命令调用，920-927）使用**完全相同的范围与有限性约束**。<br>两处实现的错误消息文案、范围边界、is_finite 检查均一致，但分别返回 `MirrorStarError::InvalidConfig` 与 `MirrorStarError::InvalidArgument` 不同变体。 | 维护风险：若未来调整音量/速度合法范围（如 speed 上限改为 20.0），需同步修改两处，遗漏其一会导致配置写入与命令运行时校验不一致。错误变体不一致（InvalidConfig vs InvalidArgument）已是既成事实，前端需分别处理。 | 在 `mirrorstar-core` 中提取 `Volume::validate(f32) -> Result<(), MirrorStarError>` 与 `Speed::validate(f32) -> Result<(), MirrorStarError>` 共享校验函数（或新增 `validate_volume_range` / `validate_speed_range` 自由函数），`config.rs` 与 `wallpaper.rs` 均委托调用。错误变体差异由调用方包装（如 `core_validate.map_err(\|_| InvalidConfig{...})`）。需评估跨模块依赖是否值得。 | 中 |

### 2.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-006 | system.rs:23-28, 57-69 | `check_desktop_status` 命令标注为 `pub async fn` 但函数体内无任何 `.await`（仅 `state.desktop.lock()` 同步阻塞调用）。ST-013 注释（23-28 行）明确记载保留 async 的理由："保留 async 前瞻性：未来若 `desktop` 改用 `tokio::sync::Mutex`（如需在持有锁时 await 其他异步操作），可平滑过渡无需修改命令签名"。该"未来"自 v4.0 文档化至今未发生，且 workerw_check.rs v41-ST-005 文档化明确论证了 `desktop` 保持 `std::sync::Mutex` 的理由（Win32 同步调用、性能更优）。 | 轻微运行时开销（async state machine 1-2 次状态转换）+ 阅读误导（看到 async 以为内部有 .await）。函数签名与实现风格不一致。 | 改为 `pub fn check_desktop_status(...) -> Result<...>`（同步函数）。若未来真的需要 await，再改回 async 成本极低（单文件单函数）。当前 6 个调用方（前端 invoke）不关心 async/sync（Tauri 统一处理）。但需验证前端契约无破坏。 | 低 |

### 2.5 未使用导入

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-007 | lib.rs:17-22 | `pub use commands::wallpaper::{parse_scaling_mode, validate_speed, validate_volume};`（19 行）与 `pub use commands::wallpaper::resolve_display_id;`（22 行）在 crate 根（`mirrorstar_wallpaper_lib`）重导出 4 个函数。Grep 验证：这 4 个函数的**唯一外部消费方**是 `tests/wallpaper_flow.rs`（27-29 行 `use mirrorstar_wallpaper_lib::{parse_scaling_mode, resolve_display_id, validate_speed, validate_volume};`）。生产代码（`src-tauri/src/` 内部）通过 `crate::commands::wallpaper::...` 私有路径访问，不经过此 `pub use`。即：crate 的公共 API 表面被 4 个测试专用函数污染。 | 轻微：crate 公共 API 表面扩大 4 项，外部消费者（如未来第三方集成）可见这些测试辅助函数。`validate_volume` 等在 `wallpaper.rs` 内必须为 `pub fn`（因集成测试是外部 crate，需经 `pub use` 访问），但 `pub use` 本身可限制可见性。 | 两种方案任选：① 用 `#[doc(hidden)] pub use ...` 标记为内部 API（最小改动，保留测试访问）；② 改用 `#[cfg(test)] pub use ...` 或将测试辅助移入 `pub mod testing { ... }` 子模块（更彻底，但需调整测试 import 路径）。推荐方案 ①。 | 低 |

### 2.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-008 | config.rs:71-79 | `validate_config_fields` 中 `if config.gif.balanced_keep_frames > i32::MAX as usize` 检查 + 注释"usize 类型保证非负，此处显式校验以防未来类型变更"。`balanced_keep_frames` 语义上是 GIF 帧保留数，业务上限远低于 i32::MAX（21 亿）；`usize` 在 64 位平台为 u64，但帧数物理不可能接近 i32::MAX。该校验针对"未来类型变更"（usize → 有符号？）这一假设性场景。 | 无功能影响。多一条永不可触发的校验分支，编译器无法消除（运行时比较）。维护者可能困惑为何 i32::MAX 是合理上限。 | 删除该检查分支（71-79 行）。若担心未来类型变更，应依赖类型系统（保持 `usize`）而非运行时校验。或改为 `debug_assert!` 仅在调试构建检查。 | 低 |
| ST-TD-009 | state.rs:126-183 | `state.rs` 顶部约 58 行注释（126-183 行）详尽论证"15 个全局静态量保守策略：全部保留，仅文档化"。Task 9.2 评估显式承认 B 类 8 个静态量（`FULLSCREEN_MONITOR_RUNNING/THREAD_ID/THREAD`、`EXPLORER_MONITOR_*`、`WORKERW_CHECK_RUNNING`、`POWER_WAS_ON_BATTERY`）"理论上可收敛进 `MonitorRegistry` 结构放入 AppState"，但因"收益低、风险高"选择保留。这是**已记录并接受的过度设计**——为 Win32 回调的函数指针约束保留进程级全局可访问性，B 类静态量本可收敛但未收敛。 | 维护成本：15 个全局静态量散落在 `state.rs`，新增监控线程需手动添加 3-4 个配套静态量（RUNNING/THREAD_ID/THREAD）。`perform_shutdown_blocking` 需逐一引用。 | 保留现状（已评估并接受的权衡）。仅记录为"过度设计"维度的事实项。若未来新增第 4 类监控线程，建议重新评估 B 类收敛为 `MonitorRegistry` 的成本收益。 | 高 |

### 2.7 修复痕迹（**重点**：Bug #N 注释、v3/v4/v5 历史标记）

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-010 | lib.rs:361,726,763; state.rs:169,170,172,500,726; system.rs:91; fullscreen.rs:30,46,109,140,159,339,380,438; wallpaper.rs:747,782 | **`Bug #N` 修复标记遍布 5 个文件**：共 7 个不同 Bug 编号（Bug #1 / #2 / #4 / #5 / #6 / #7，缺 Bug #3），约 18 处注释。这些是 v3.x 时期引入的 bug 编号方案，每个 Bug 对应一次修复，注释形态如 `// Bug #7 修复：移除直接 emit，依赖 pause 线程通过 global_state_changed 通道...`。Bug #3 在 src-tauri 中无任何引用（可能属于其他模块或编号缺口）。 | 注释噪音：维护者需在多文件间跳转才能拼凑某个 Bug 的完整修复上下文。Bug 编号无对应 CHANGELOG 索引，新成员无法查询"Bug #7 到底是什么"。 | ① 短期：保留注释（移除会丢失修复历史上下文）；② 中期：在 `docs/优化文档/附录A-已修复问题汇总.md` 中补充 Bug #1-#7 的完整索引（描述 / 影响文件 / 修复 commit），代码注释改为 `// Bug #7（详见附录A）`；③ 长期：稳定后的 Bug #N 注释可逐步删除，仅保留 CHANGELOG 索引。 | 中 |
| ST-TD-011 | state.rs:104,194,308,362,514; system.rs:30; config.rs:6,104; wallpaper.rs:155,341,981; fullscreen.rs:11; power.rs:5; workerw_check.rs:23; explorer.rs:11; lib.rs:26; commands/mod.rs:3 | **`v41-ST-NNN` 标记遍布 8 个文件**：14 个不同标记（v41-ST-003 / -004 / -005 / -006 / -007 / -008 / -009 / -010 / -011 / -012 / -013 / -014 / -015 / -016），约 20+ 处。这些是 v4.1 spec（`comprehensive-project-review-and-doc-restructure-2026-07-15`）的修复标记，形态如 `v41-ST-007: try_lock 失败时返回 Ok(()) 的行为与用户体验文档化`。多数 v41-ST-NNN 标记后跟大段文档化注释（如 config.rs:6-45 的防抖策略、wallpaper.rs:155-183 的 DisplaySettingGuard 用户体验）。 | 注释体量膨胀：v41-ST-NNN 标记的文档化注释累计约 300+ 行，部分已超出"修复痕迹"范畴成为永久设计文档（如 v41-ST-014 setup 顺序、v41-ST-005 锁类型混用）。修复标记与设计文档混杂，难以区分"已修复的过渡注释"与"应长期保留的设计说明"。 | ① 区分两类：纯修复痕迹（如 `v41-ST-007: try_lock 失败时...` 解释为何返回 Ok）可考虑移除标记前缀保留说明；长期设计文档（如 v41-ST-014 setup 顺序）应移除 `v41-ST-` 前缀，作为独立 `## Design` 章节保留；② 将 v41 spec 路径引用更新为 `docs/优化文档/06-src-tauri应用层.md` 对应章节。 | 高 |
| ST-TD-012 | wallpaper.rs:188,301,306,358,405,472,493,539,620,654,686,698,703,728,730,753,785,803,818,833,845,861,888; config.rs:31 | **`v5.0 X-PERF-NNN` 性能标记遍布 wallpaper.rs 与 config.rs**：3 类前缀（`I-PERF` 性能改进 / `C-PERF` 性能正确性 / `A-PERF` 架构性能），约 12 个不同编号（I-PERF-001/002/003/004/006/009/010、C-PERF-002/003/005、A-PERF-001/003），30+ 处内联注释。形态如 `// v5.0 I-PERF-003: 复用校验阶段已获取的 metadata`、`// v5.0 A-PERF-001: emit 完整 entry 供前端增量更新`。 | 注释体量大，但多数 v5.0 PERF 标记描述的是**当前代码行为**（如"复用 metadata 避免重复 syscall"），属有效设计文档，非纯修复痕迹。 | 保留有效设计文档，移除纯历史标记：① 删除 `v5.0 I-PERF-003:` 前缀，保留"复用校验阶段已获取的 metadata"说明；② 在 `docs/优化文档/附录B-版本历史.md` 中补充 v5.0 PERF 改动索引。 | 中 |
| ST-TD-013 | explorer.rs:11-60, 115-118 | **多版本历史标记混杂**：explorer.rs 顶部 v41-ST-008 注释块（11-60 行，约 50 行 UTF-8 BOM 处理策略）内含三套历史版本标记：<br>① `ST-014 v4.0 修复时发现 explorer.rs 首 3 字节曾为 UTF-8 BOM`（20 行）——引用 v4.0；<br>② `ST-014 修复后 BOM 已不存在（可能在前序 Wave 中由编辑器自动移除）`（29 行）——引用"Wave"术语（v3.x 时期的修复批次命名）；<br>③ `ST-018: ... v2.1 T-016 曾误判为死代码，实为外部 DestroyWindow 场景的必要兜底`（115-118 行）——引用 v2.1。<br>三套标记（v2.1 / Wave / v4.0）反映命名方案演变史，当前文档已统一为 v4.0/v5.0/v6 命名。 | 历史标记难以追溯：新成员无法查询"Wave"是什么、"v2.1 T-016"在哪个文档。ST-018 注释提到的"曾误判为死代码"是已修复的判断失误，长期保留会误导（让人以为当前代码仍可疑）。 | ① ST-018 注释（115-118 行）保留核心说明"WM_DESTROY 处理是外部 DestroyWindow 场景的必要兜底"，删除"v2.1 T-016 曾误判为死代码"历史叙述；② UTF-8 BOM 注释块（11-60 行）压缩为 5-10 行核心策略说明，删除 v4.0/Wave 历史背景（BOM 已不存在，策略已稳定）。 | 中 |
| ST-TD-014 | wallpaper.rs:482 | `// 详见 findings/03-rust-src-tauri.md ST-014。` 注释引用 spec findings 路径 `findings/03-rust-src-tauri.md`。该路径是 v4.0 spec 内部 findings 目录，已重组为 `docs/优化文档/06-src-tauri应用层.md`（v4.0 已覆盖 ST-014 finding）。 | 路径失效：维护者按注释查找会找不到文件。 | 更新引用为 `// 详见 docs/优化文档/06-src-tauri应用层.md ST-014`，或直接删除（v4.0 文档已是权威索引）。 | 低 |
| ST-TD-015 | wallpaper.rs:15,483,947,959,970; state.rs:353; lib.rs:333 | **`fix-wallpaper-preview-blank` 特性分支名作为修复标记**出现 7 处。形态如 `// fix-wallpaper-preview-blank: 缩略图生成失败时通过此 payload 通知前端展示降级占位图`、`// 启动时清理 0 字节损坏缩略图文件（fix-wallpaper-preview-blank Task 5）`。该名称是 v4.0 时期的特性分支名（git branch name），非稳定标识。 | 特性分支名随 git 历史淡出后难以追溯。`fix-wallpaper-preview-blank Task 5/6` 的任务编号无对应文档索引。 | 将 `fix-wallpaper-preview-blank` 替换为稳定的 finding ID（如 v4.0 ST-014）或直接删除前缀保留说明（如"缩略图生成失败时通知前端展示降级占位图"）。在附录B补充该特性分支的 commit 范围索引。 | 低 |
| ST-TD-016 | explorer.rs:114-121 | `WM_DESTROY` 处理注释 `ST-018: 防御性处理——若外部代码（如系统或另一线程）通过 DestroyWindow 销毁本窗口...（v2.1 T-016 曾误判为死代码，实为外部 DestroyWindow 场景的必要兜底。）` 记录了"死代码被误删后恢复"的历史。该注释本身是必要的（说明 WM_DESTROY 非死代码），但 `v2.1 T-016 曾误判为死代码` 是已过时的版本引用。 | 轻微：历史叙述无当前价值，核心说明（WM_DESTROY 是必要兜底）应保留。 | 保留 ST-018 核心说明，删除括号内 `v2.1 T-016 曾误判为死代码，实为外部 DestroyWindow 场景的必要兜底` 历史叙述。 | 低 |
| ST-TD-017 | fullscreen.rs:109-113, 140-141, 159-161; lib.rs:512-525, 663-668; wallpaper.rs:747-752, 782-789 | **`Bug #7 修复：移除直接 emit，依赖 pause 线程通过 global_state_changed 通道触发 setup 中 spawn 的订阅任务 emit wallpaper-state-changed` 近乎相同的注释文本重复约 6 次**。fullscreen.rs 三处（109-113 / 140-141 / 159-161）说明全屏 pause/resume 不 emit；lib.rs 两处（512-525 / 663-668）说明状态订阅任务与托盘菜单不 emit；wallpaper.rs 两处（747-752 / 782-789）说明 pause_wallpaper/resume_wallpaper 命令的 emit 兜底逻辑。6 处注释解释的是同一架构决策（Bug #7 引入的 broadcast 通道统一 emit 模式）。 | 注释冗余：同一架构说明重复 6 次，维护时需同步 6 处。部分注释含 8-15 行详细解释（如 lib.rs:512-525），体量大。 | 在 `state.rs` 或 `lib.rs` 的状态订阅任务处保留一份完整的架构说明（如 lib.rs:512-525），其他 5 处改为简短引用 `// 状态变更 emit 由全局订阅任务统一处理（详见 lib.rs setup 闭包）`。 | 低 |

### 2.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-018 | tests/wallpaper_flow.rs:492,496 vs lib.rs:636,638 | **`PauseReason` 变体在测试与生产中不一致**：测试 `test_pause_all_and_resume_all_fast`（wallpaper_flow.rs:492,496）使用 `PauseReason::USER` 调用 `pause_all_fast` / `resume_all_fast`；生产托盘菜单路径（lib.rs:636,638）使用 `PauseReason::TRAY` 调用相同方法。Grep 验证：`PauseReason::USER` 仅在测试中出现，生产代码无任何使用。测试用 `USER` 验证 `pause_all_fast` 行为，但生产托盘走 `TRAY` 路径，测试未覆盖生产实际使用的 reason 变体。 | 测试覆盖缺口：`TRAY` reason 路径未被任何测试验证（虽然 `pause_all_fast` 内部对 reason 仅做记录不分支，功能上等价，但语义上测试应覆盖生产实际 reason）。 | 测试改用 `PauseReason::TRAY` 对齐生产路径（wallpaper_flow.rs:492,496）。或若 `USER` 是历史变体（v3.x 托盘用 USER，v4.0 改 TRAY 但测试未同步），删除 `USER` 变体（需确认 core 中无其他引用）。 | 低 |
| ST-TD-019 | lib.rs:361 (注释) vs 381,387 (代码) | **`wpfile://` 协议名在注释与代码中不一致**：注释（lib.rs:361）写 `注册 wpfile:// 自定义 URI scheme protocol`，代码（lib.rs:381 `register_uri_scheme_protocol("wpfile", ...)`、387 `uri_path = request.uri().path()`）注册的 scheme 名是 `wpfile`（无 `://` 后缀）。Tauri 的 `register_uri_scheme_protocol` 接受 bare scheme name（如 `"wpfile"`），前端通过 `convertFileSrc(path, "wpfile")` 生成 `http://wpfile.localhost/...` URL。 | 轻微：注释读起来像 scheme 是 `wpfile://`，但实际 scheme 是 `wpfile`。可能误导维护者在前端用 `convertFileSrc(path, "wpfile://")`（错误）。 | 注释统一改为 `wpfile`（无 `://`），或显式说明"scheme 名为 `wpfile`，URL 形式为 `http://wpfile.localhost/...`"。 | 低 |
| ST-TD-020 | wallpaper.rs:332,351,410,625,628; config.rs:12; state.rs 等多处 | **"受控数据目录"概念在注释中使用 3 种术语混称**：① `受控数据目录`（wallpaper.rs:351, 628）；② `受控目录`（wallpaper.rs:410, 625）；③ `data_dir` / `数据目录`（config.rs:12, 代码标识符）。同一概念（`%APPDATA%/mirrorstar/`）在不同位置用不同中文译名。 | 轻微：Grep 难以定位所有相关注释（需 3 个查询词）。新成员可能误以为"受控数据目录"与"受控目录"是不同概念。 | 统一为 `受控数据目录（data_dir）`，首次出现时给出全称 + 代码标识符，后续用 `data_dir`。 | 低 |

### 2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| ST-TD-021 | explorer.rs:129 | `start_explorer_restart_monitor` doc 注释写 `作为 30 秒轮询的补充机制`，但实际 `workerw_check` 任务间隔为 **300 秒（5 分钟）**（workerw_check.rs:73 `Duration::from_secs(300)`）。Grep 验证：所有其他位置（state.rs:36 / lib.rs:717 / system.rs:35 / power.rs:35 / workerw_check.rs:9,46,90）均正确写为"5 分钟"或"300s"。explorer.rs:129 是唯一遗留的"30 秒"错误。推测 v3.x 时期 workerw_check 间隔曾为 30 秒，后改为 5 分钟，explorer.rs 的注释未同步。 | 维护误导：维护者可能以为还存在 30 秒间隔的轮询任务，在性能评估或定时逻辑设计时产生误判。 | 将 explorer.rs:129 的 `作为 30 秒轮询的补充机制` 改为 `作为 5 分钟轮询的补充机制`（与 workerw_check.rs:9 一致）。 | 低 |
| ST-TD-022 | state.rs:169-183 | `state.rs` 顶部 Global Statics 注释块中 `历史清理（Bug #7 修复）` 段落（169-183 行）叙述 `SHARED_APP_HANDLE` 的演变史：Bug #4 引入 → Bug #7 移除 → T07 重新引入。当前状态是 `SHARED_APP_HANDLE` 存在且仅用于 workerw_check emit。Bug #4/#7 的历史叙述已无当前价值——该静态量现已稳定用于 T07 场景，不存在"是否应存在"的争议。 | 注释体量：约 15 行历史叙述对理解当前代码无帮助。新成员读完可能误以为 `SHARED_APP_HANDLE` 仍有争议。 | 压缩为 3-5 行：保留"`SHARED_APP_HANDLE` 供 workerw_check 任务 emit `desktop-status-changed`（T07）；其他回调走 broadcast 通道"，删除 Bug #4/#7 演变史。 | 低 |
| ST-TD-023 | config.rs:71 | `// gif.balanced_keep_frames: usize，不能为负（usize 类型保证非负，此处显式校验以防未来类型变更）` 注释为 ST-TD-008 的过度设计校验辩护。"以防未来类型变更"是假设性场景，从未发生（类型一直是 usize）。 | 轻微：注释为不必要的校验辩护，维护者可能误以为该校验有现实必要性。 | 与 ST-TD-008 一并处理：删除校验则注释同步删除；保留校验则注释改为说明实际业务上限（如"GIF 帧保留数业务上限远低于 i32::MAX，此处仅为防御性边界"）。 | 低 |
| ST-TD-024 | state.rs:726; lib.rs:726 | `// Bug #5 修复后窗口使用 hide() 而非 destroy()，窗口始终存在，show() 即可恢复` 注释中"Bug #5 修复后"是修复痕迹前缀，但描述的行为（hide 而非 destroy）是当前稳定行为。 | 轻微：修复痕迹前缀无当前价值，核心说明应保留。 | 删除"Bug #5 修复后"前缀，保留"窗口使用 hide() 而非 destroy()，窗口始终存在，show() 即可恢复"。 | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0）

低风险、高收益的清理项，可在 v6-A Wave 立即执行：

| ID | 描述 | 复杂度 | 理由 |
|---|---|---|---|
| ST-TD-001 | 删除/标注 `add_wallpaper` 不可达的 Web 分支 | 低 | 死代码，单文件单函数改动 |
| ST-TD-021 | 修正 explorer.rs:129 "30 秒" → "5 分钟" | 低 | 单行注释修正，与全项目其他位置对齐 |
| ST-TD-014 | 更新 wallpaper.rs:482 失效的 findings 路径引用 | 低 | 单行注释修正 |
| ST-TD-016 | 删除 explorer.rs:118 "v2.1 T-016 曾误判为死代码" 历史叙述 | 低 | 单行注释精简 |
| ST-TD-024 | 删除 state.rs:726 / lib.rs:726 "Bug #5 修复后" 前缀 | 低 | 单行注释精简 |
| ST-TD-019 | 统一 lib.rs:361 注释为 `wpfile`（无 `://`） | 低 | 单行注释修正 |
| ST-TD-018 | 测试改用 `PauseReason::TRAY` 对齐生产 | 低 | 测试单文件改动 |

### 3.2 谨慎清理（P1/P2）

需评估跨文件影响或与团队对齐的清理项：

| ID | 描述 | 复杂度 | 理由 |
|---|---|---|---|
| ST-TD-005 | 提取 volume/speed 范围校验到 mirrorstar-core 共享函数 | 中 | 消除重复实现，需跨模块（core ↔ src-tauri）协调 |
| ST-TD-017 | 合并 6 处 Bug #7 boilerplate 注释为单一引用 | 低 | 跨 3 文件注释重构，需确保引用目标稳定 |
| ST-TD-010 | Bug #N 标记归档到附录A 已修复问题汇总 | 中 | 需补全 Bug #1-#7 索引文档 |
| ST-TD-011 | 区分 v41-ST-NNN 修复痕迹与长期设计文档 | 高 | 14 个标记逐个评估去留，需理解每个 v41 spec 上下文 |
| ST-TD-012 | v5.0 PERF 标记前缀清理 + 附录B 索引补充 | 中 | 30+ 处注释精简，需区分有效设计文档与纯历史标记 |
| ST-TD-013 | explorer.rs 多版本标记（v2.1/Wave/v4.0）清理 | 中 | UTF-8 BOM 注释块压缩 + ST-018 历史叙述删除 |
| ST-TD-015 | `fix-wallpaper-preview-blank` 标记替换为稳定 ID | 低 | 7 处注释前缀替换，需附录B 补充分支索引 |
| ST-TD-002 | 删除 MockRenderer 4 个未使用方法 | 低 | 测试辅助清理，需确认无反射/宏使用 |
| ST-TD-007 | `pub use` 加 `#[doc(hidden)]` 限制测试专用 API 可见性 | 低 | crate 公共 API 表面收敛 |
| ST-TD-020 | 统一"受控数据目录"术语 | 低 | 跨文件注释统一，批量替换 |
| ST-TD-022 | 压缩 state.rs:169-183 Bug #7 历史叙述 | 低 | 单文件注释压缩 |
| ST-TD-023 | config.rs:71 注释与 ST-TD-008 一并处理 | 低 | 与 ST-TD-008 联动 |

### 3.3 评估后决定（P3）

已记录并接受的权衡或低收益清理项，建议长期观察：

| ID | 描述 | 复杂度 | 理由 |
|---|---|---|---|
| ST-TD-003 | `validate_wallpaper_file_path` wrapper 保留或内联 | 低 | 3 个调用点 justify helper 存在，doc 措辞调整即可 |
| ST-TD-004 | `try_lock_with_timeout<T>` 泛型保留 | 低 | testability-driven generality 合理 |
| ST-TD-006 | `check_desktop_status` async 改 sync | 低 | 已文档化的前瞻性权衡，改回成本低但收益小 |
| ST-TD-008 | `balanced_keep_frames > i32::MAX` 校验删除 | 低 | 防御性边界，删除风险极低但收益有限 |
| ST-TD-009 | 15 个全局静态量收敛为 MonitorRegistry | 高 | Task 9.2 已评估并接受的权衡，B 类 8 个可收敛但风险高 |

## 4. 优化机会

1. **注释体量压缩**：src-tauri/src/ 12 个源文件约 5630 行，其中文档化注释占比估计 30-40%（部分文件如 explorer.rs v41-ST-008 BOM 策略 50 行、lib.rs v41-ST-014 setup 顺序 115 行、workerw_check.rs v41-ST-005 锁类型 50 行、state.rs Global Statics 58 行）。v6-A/B 清理后预计可减少 400-600 行注释体量，同时保留核心设计文档。

2. **测试覆盖对齐**：ST-TD-018 暴露的 `PauseReason::USER` vs `TRAY` 不一致提示测试与生产 reason 路径未对齐。建议审查所有 `PauseReason` 变体在测试中的覆盖，确保生产路径的 reason 被至少一个测试使用。

3. **历史标记索引化**：Bug #N / ST-NNN / v41-ST-NNN / v5.0 X-PERF-NNN / fix-wallpaper-preview-blank 五套历史标记应统一索引到 `docs/优化文档/附录A-已修复问题汇总.md` 与 `附录B-版本历史.md`，代码注释逐步从"内联完整说明"过渡到"引用附录 ID + 简短说明"。

4. **校验逻辑下沉**：ST-TD-005 的 volume/speed 范围校验重复提示，配置字段校验（`validate_config_fields`）与命令参数校验（`validate_volume`/`validate_speed`）应共享底层范围定义。可在 `mirrorstar-core` 的 `config` 模块为 `Volume` / `Speed` newtype 提供统一的 `validate()` 方法。

5. **Web 分支可达性对称**：ST-TD-001 的 `add_wallpaper` 不可达 Web 分支与 `regenerate_thumbnails` 的可达 Web 分支不对称。建议审查所有按 `WallpaperType` 分派的代码路径，确保 Web 处理逻辑一致（要么外层统一过滤，要么内层统一处理）。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项（引用 `docs/优化文档/06-src-tauri应用层.md` 中的 findings ID）

v4.0 文档（`06-src-tauri应用层.md`）记录了 17 项 findings（ST-001 至 ST-018，缺部分编号），本次 v6 审查不重复 v4.0 已覆盖的正确性问题，但识别出 v4.0 修复过程遗留的技术债：

| v4.0 finding | v4.0 描述 | v6 关联技术债 |
|---|---|---|
| ST-001 | `add_wallpaper` 字符串前缀比较兄弟目录误判 | 已修复（wallpaper.rs:337-339 `Path::starts_with`）；ST-TD-011 记录 v41-ST-009 在此基础上追加 `validate_path_within_data_dir` 的修复痕迹 |
| ST-002 | `open_file_dialog` 超时后 spawn_blocking 不可取消 | 已修复（system.rs:118-144 回调式 + oneshot）；ST-TD-012 记录相关 v5.0 PERF 标记 |
| ST-003 | `workerw_check` 异步任务中 std Mutex 阻塞 | 已评估并接受（workerw_check.rs:87-98 注释论证）；v6 无新技术债 |
| ST-004 | `DesktopIntegration` 错误变体泛化 | 已修复（validate_volume/speed/parse_scaling_mode 改用 InvalidArgument）；ST-TD-005 记录校验逻辑重复实现 |
| ST-005 | `set_speed` fire-and-forget JoinHandle 未跟踪 | 已修复（wallpaper.rs:887-890 同步执行）；ST-TD-012 记录 ST-005 修复痕迹 |
| ST-006 | shutdown 后缩略图任务写入丢失 | 已修复（state.rs:710-715 二次 flush）；ST-TD-011 记录 ST-006 修复痕迹 |
| ST-007 | symlink_metadata 无法识别 Junction Points | 已修复（wallpaper.rs:301-316 canonicalize 补充校验）；ST-TD-011 记录 ST-007 修复痕迹 |
| ST-013 | `check_desktop_status` async 标注保留前瞻性 | 已文档化（system.rs:23-28）；ST-TD-006 在 v6 重新评估为"过时模式" |
| ST-014 | 缩略图 JoinHandle 未保存致 shutdown 截断 | 已修复（THUMBNAIL_TASK Vec + 5s 等待）；ST-TD-014 记录失效的 `findings/03-rust-src-tauri.md` 路径引用 |
| ST-018 | WM_DESTROY 误判为死代码 | 已修复（explorer.rs:114-121 保留）；ST-TD-013 / ST-TD-016 记录"曾误判为死代码"历史叙述应清理 |
| Bug #1-#7 | 7 个 Bug 修复 | 已全部修复；ST-TD-010 / ST-TD-017 记录 Bug #N 标记的清理需求 |

### 5.2 v5.0 已覆盖项

v5.0 文档（性能优化）记录了 77 项性能 findings，其中 src-tauri 相关的 v5.0 X-PERF-NNN 标记（I-PERF-001/002/003/004/006/009/010、C-PERF-002/003/005、A-PERF-001/003）均已实施。本次 v6 不重复 v5.0 性能问题，但识别出 v5.0 修复过程遗留的标记噪音：

| v5.0 finding | v5.0 描述 | v6 关联技术债 |
|---|---|---|
| I-PERF-003 | `validate_and_get_metadata` 复用 metadata 避免重复 syscall | 已实施（wallpaper.rs:245-319）；ST-TD-003 记录 `validate_wallpaper_file_path` 作为"向后兼容包装"的冗余抽象 |
| I-PERF-009 | `DisplaySettingGuard::acquire` 用 insert 返回值消除 contains 双重哈希 | 已实施（wallpaper.rs:189）；ST-TD-011 记录 v5.0 标记前缀 |
| A-PERF-001 | emit 完整 entry 供前端增量更新 | 已实施（wallpaper.rs:475, 539-553）；ST-TD-012 记录 v5.0 PERF 标记清理需求 |
| A-PERF-003 | `batch_update_thumbnails` 单次落盘 | 已实施（wallpaper.rs:1184-1211）；ST-TD-012 同上 |
| C-PERF-005 | config_save_mutex / library_save_mutex 拆分 | 已实施（config.rs:31 文档化）；ST-TD-011 记录 v41-ST 标记与 v5.0 标记混杂 |

### 5.3 v6 新发现

v6 审查在 v4.0（正确性）与 v5.0（性能）之外，新发现 24 项技术债，按类型分布：

| 类型 | 数量 | 主要新发现 |
|---|---|---|
| 死代码 | 2 | ST-TD-001（Web 不可达分支）、ST-TD-002（MockRenderer 未使用方法） |
| 冗余抽象 | 2 | ST-TD-003（validate_wallpaper_file_path wrapper）、ST-TD-004（try_lock_with_timeout 泛型） |
| 重复实现 | 1 | ST-TD-005（volume/speed 范围校验在 config.rs 与 wallpaper.rs 重复） |
| 过时模式 | 1 | ST-TD-006（check_desktop_status async 无 await） |
| 未使用导入 | 1 | ST-TD-007（lib.rs pub use 测试专用重导出） |
| 过度设计 | 2 | ST-TD-008（balanced_keep_frames > i32::MAX 校验）、ST-TD-009（15 全局静态量保守策略） |
| 修复痕迹 | 8 | ST-TD-010 至 ST-TD-017（Bug #N / v41-ST-NNN / v5.0 PERF / Wave / v2.1 / fix-wallpaper-preview-blank / findings 路径 / Bug #7 boilerplate） |
| 命名一致性 | 3 | ST-TD-018（PauseReason::USER vs TRAY）、ST-TD-019（wpfile:// vs wpfile）、ST-TD-020（受控数据目录术语） |
| 注释陈旧 | 4 | ST-TD-021（30 秒→5 分钟）、ST-TD-022（Bug #7 历史叙述）、ST-TD-023（以防未来类型变更）、ST-TD-024（Bug #5 修复后前缀） |
| **合计** | **24** | **修复痕迹（8 项）为最大类别，反映 src-tauri 是多轮 Bug 修复重灾区** |

**按级别分布**：

| 级别 | 数量 | 处理策略 |
|---|---|---|
| P0（高收益低风险） | 7 | Wave v6-A 立即清理（多为单行注释修正 + 死代码删除） |
| P1/P2（高收益中风险 / 中收益） | 12 | Wave v6-B/C 谨慎清理（含跨文件注释重构 + 校验逻辑下沉 + 标记索引化） |
| P3（长期或低收益） | 5 | Wave v6-D 择机处理（已记录并接受的权衡，如全局静态量收敛） |

**核心结论**：src-tauri 应用层经 v3.x → v4.0 → v4.1 → v5.0 多轮修复后，工程质量与正确性已达高水平（569+ 测试、clippy 零警告、详尽的锁顺序/COM/shutdown 文档化）。主要技术债集中在**修复痕迹维度**（8 项，占 33%）：5 套历史标记体系（Bug #N / ST-NNN / v41-ST-NNN / v5.0 X-PERF-NNN / fix-wallpaper-preview-blank）散落在代码注释中，累计约 600-800 行注释体量。建议 v6-A/B Wave 优先清理 P0 项（7 项单行修正）与 P1 的注释索引化（ST-TD-010 / ST-TD-011 / ST-TD-012），可显著降低注释噪音而不影响功能正确性。

## 6. 清理成果（2026-07-26）

> 实施 spec：`cleanup-v6-src-tauri-tech-debt-2026-07-26`（2026-07-26 完成）

src-tauri 应用层模块已完成全部 24 项技术债的清理，按级别落实情况如下：

### 6.1 P0 立即清理（7/7 项，全部落实）

| ID | 类型 | 落实方式 |
|---|---|---|
| ST-TD-001 | 死代码 | `add_wallpaper` 不可达 Web 分支改 `unreachable!("Web 已在外层 matches! 过滤")` |
| ST-TD-021 | 注释陈旧 | explorer.rs:129 "30 秒轮询" → "5 分钟轮询"，与 workerw_check.rs:9 一致 |
| ST-TD-014 | 失效引用 | wallpaper.rs:482 `findings/03-rust-src-tauri.md` → `docs/优化文档/06-src-tauri应用层.md` |
| ST-TD-016 | 修复痕迹 | explorer.rs:118 删除 "v2.1 T-016 曾误判为死代码" 历史叙述，保留核心说明 |
| ST-TD-024 | 修复痕迹 | state.rs:726 / lib.rs:726 删除 "Bug #5 修复后" 前缀，保留 "hide() 而非 destroy()" 说明 |
| ST-TD-019 | 命名一致性 | lib.rs:361 注释 `wpfile://` → `wpfile`，补充 URL 形式说明 |
| ST-TD-018 | 测试覆盖 | tests/wallpaper_flow.rs:492,496 `PauseReason::USER` → `PauseReason::TRAY`；评估后删除 `USER` 变体（见 wallpaper 模块） |

### 6.2 P1/P2 谨慎清理（12/12 项，全部落实）

| ID | 类型 | 落实方式 |
|---|---|---|
| ST-TD-002 | 死代码 | MockRenderer 删除 `was_played`/`was_terminated`/`speed`/`scaling_mode` 4 个方法及对应字段，`#![allow(dead_code)]` 保留并补注释说明跨测试文件用途 |
| ST-TD-005 | 重复实现 | 提取 `validate_volume` / `validate_speed` 到 `mirrorstar-core::config::validation`，config.rs 与 wallpaper.rs 均委托调用，错误变体差异由调用方包装 |
| ST-TD-007 | 未使用导入 | lib.rs:17-22 4 个测试专用 `pub use` 加 `#[doc(hidden)]` |
| ST-TD-010 | 修复痕迹 | 18 处 Bug #N 注释精简，核心说明保留，编号前缀移除 |
| ST-TD-011 | 修复痕迹 | v41-ST-NNN 标记区分处理：纯修复痕迹移除前缀保留说明，长期设计文档移除 `v41-ST-` 前缀作为独立说明 |
| ST-TD-012 | 修复痕迹 | v5.0 I-PERF / C-PERF / A-PERF 标记前缀清理，设计说明保留 |
| ST-TD-013 | 修复痕迹 | explorer.rs UTF-8 BOM 注释块从 ~30 行压缩至 9 行，删除 v4.0/Wave 历史背景 |
| ST-TD-015 | 修复痕迹 | `fix-wallpaper-preview-blank` 7 处标记替换为稳定 finding ID 或删除前缀 |
| ST-TD-017 | 修复痕迹 | 6 处 Bug #7 boilerplate 合并，lib.rs setup 闭包保留一份完整架构说明，其他 5 处改为简短引用 |
| ST-TD-020 | 命名一致性 | "受控数据目录" 术语统一，首次出现为 `受控数据目录（data_dir）`，后续统一用 `data_dir` |
| ST-TD-022 | 注释压缩 | state.rs:169-183 Bug #7 历史叙述从 ~30 行压缩至 15 行，删除 Bug #4/#7 演变史 |
| ST-TD-023 | 注释陈旧 | config.rs:71 注释与 ST-TD-008 决策联动，改为说明业务上限与防御性边界校验用途 |

### 6.3 P3 评估后决定（5/5 项，全部保留现状 + 补充注释）

| ID | 类型 | 决策 | 理由 |
|---|---|---|---|
| ST-TD-003 | 冗余抽象 | 保留 + 调整 doc | 3 个调用点 justify helper 存在；doc 措辞改为 "丢弃 metadata 的便捷封装" |
| ST-TD-004 | 冗余抽象 | 保留 | testability-driven generality 合理，未来新增测试场景可复用 |
| ST-TD-006 | 过时模式 | 保留 | 已文档化的前瞻性权衡，未来可能接入异步桌面 API |
| ST-TD-008 | 过度设计 | 保留 | 防御性边界校验，业务上限远低于 i32::MAX，注释已联动更新说明用途 |
| ST-TD-009 | 过度设计 | 保留 | 15 个全局静态量保守策略，已记录何时重新评估的条件（如回调机制重构） |

### 6.4 验证结果

- **编译**：`cargo build --package mirrorstar-wallpaper` 通过
- **测试**：`cargo test --package mirrorstar-wallpaper` 62 个测试通过（36 ignored，含 COM/桌面环境依赖项）
- **clippy**：`cargo clippy --package mirrorstar-wallpaper -- -D warnings` 零警告
- **Grep 残留验证**：无 `PauseReason::USER` / `向后兼容包装` / `v2.1 T-016` / `Bug #5 修复后` / `findings/03-rust-src-tauri` / `fix-wallpaper-preview-blank` / `v41-ST-` / `v5.0 X-PERF-` 残留

### 6.5 衍生收益

- **跨模块共享校验**：ST-TD-005 提取的 `mirrorstar-core::config::validation::validate_volume` / `validate_speed` 消除了 config.rs 与 wallpaper.rs 的范围定义重复，未来新增校验场景可直接复用
- **测试变体清理**：ST-TD-018 删除 `PauseReason::USER` 变体后，wallpaper 模块的 `PauseReason` 枚举更精简（仅 `FULLSCREEN` / `BATTERY` / `TRAY` 三个生产变体），减少误导性测试代码
- **注释体量压缩**：explorer.rs BOM 注释块（~30 → 9 行）+ state.rs Bug #7 历史叙述（~30 → 15 行）+ Bug #N/v41-ST/v5.0 PERF 标记批量清理，累计减少约 200-300 行注释噪音
- **术语统一**：`受控数据目录（data_dir）` 术语统一后，安全相关注释的语义更清晰，避免 "受控目录" / "数据目录" 混用导致的歧义
