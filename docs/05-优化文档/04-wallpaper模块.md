# wallpaper 模块优化文档

> [← 返回索引](./README.md)

> 本文档合并自：顶层 v4.0 wallpaper 模块审查文档 + v6.0 技术债审查（wallpaper 模块）文档。

## 模块概览 / 现状

- **模块路径**：`crates/mirrorstar-core/src/wallpaper/`
- **职责**：壁纸渲染器抽象、状态管理与子进程句柄 RAII，是项目最大且最复杂的模块。模块通过 `WallpaperRenderer` trait 统一四种壁纸类型（`ImageRenderer` / `GifRenderer` / `VideoRenderer` / `WebRenderer`）的生命周期接口，由 `WallpaperEngine` 集中管理多显示器壁纸实例与状态机。模块分为以下几个层次：
  - **核心抽象层**（`mod.rs`）：`WallpaperState` / `ScalingMode` / `GifMemoryStrategy` / `PauseCommand` / `PauseSender` / `WallpaperRenderer` trait、`calculate_scaling`、`create_pause_channel`、`PauseReason` 位图、`OwnedProcHandle` RAII、`spawn_proc_exit_monitor` 共享函数、屏幕分辨率缓存。
  - **引擎层**（`manager.rs` + `mode_dispatch.rs` + `fast_path.rs`）：`WallpaperEngine` 状态机、三阶段 `set_wallpaper` 流程、Native/WorkerW 模式分发、快速控制路径（`pause_all_fast` / `resume_all_fast` / `set_volume_fast` / `toggle_mute_fast`）。
  - **渲染器层**：GDI 渲染器（`gdi_base.rs` + `gdi_cache.rs` + `image.rs` + `gif.rs` + `gif_decode.rs` + `gif_memory.rs`）与子进程渲染器（`subprocess_base.rs` + `video.rs` + `web.rs`）。
- **核心结构**：
  - `WallpaperEngine`：管理所有壁纸实例，提供 `set_wallpaper`/`pause_all_fast`/`resume_all_fast`/`shutdown` 等方法
  - `WallpaperRenderer` trait：统一渲染器接口
  - 4 种渲染器：`ImageRenderer`（原生 GDI + 双缓冲）、`GifRenderer`（image crate + GDI 双缓冲 + 40MB 内存预算）、`VideoRenderer`（mpv.exe 子进程 + 命名管道 IPC）、`WebRenderer`（wp-proc 子进程代理层）
  - `PauseSender`：`tokio::sync::mpsc::UnboundedSender`，快速通道绕过 engine Mutex
  - `WallpaperMode`：Native（`SystemParametersInfoW`）vs WorkerW（窗口嵌入）双路径
  - `ScalingMode`：Fill / Fit / Stretch / Center / Original
- **设计模式**：
  - 专用线程消息循环（`WallpaperCommand` 枚举 + channel + `PostMessageW` 唤醒）
  - GDI 对象缓存（通过 `GdiCache` 统一管理，仅在 resize 时重建）
  - 暂停时像素释放（Image/GIF 暂停时通过 `GdiCache.release_bitmap()` 释放，恢复时重新解码）
  - `PauseReason` 位图协调多暂停源
  - `GdiRendererBase`/`SubprocessRendererBase` 基类消除渲染器间重复逻辑

### 文件清单

| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 1084 | 模块入口、WallpaperState/ScalingMode/GifMemoryStrategy/PauseCommand/PauseSender/WallpaperRenderer trait、calculate_scaling、PauseReason 位图、OwnedProcHandle、spawn_proc_exit_monitor、屏幕分辨率缓存 |
| manager.rs | 1885 | WallpaperEngine 状态机、RendererConfig、三阶段 set_wallpaper 流程、construct_renderer、embed_and_register_renderer、close_wallpaper、update_positions |
| gif.rs | 1112 | GifRenderer GIF 渲染器（WM_TIMER 驱动帧推进 + 后台解码线程 + 4 种内存管理策略） |
| video.rs | 891 | VideoRenderer 视频渲染器（mpv 子进程 + MpvIpcClient + WASAPI 音量控制 + pause 线程 COM MTA） |
| fast_path.rs | 794 | WallpaperEngine 快速通道扩展（pause/resume/volume/mute 快速路径 + PauseReason 位图协调） |
| image.rs | 685 | ImageRenderer 图片渲染器（GdiRendererBase + 专用线程消息循环 + WM_WALLPAPER_COMMAND） |
| gdi_base.rs | 553 | GdiRendererBase 共享基类（paint_with_double_buffer、spawn_pause_forwarder、register_window_class_once、create_wallpaper_window） |
| gif_decode.rs | 503 | decode_gif 全帧解码（40MB 内存预算 + 降采样）、decode_gif_first_frame 首帧快速解码 |
| web.rs | 504 | WebRenderer 网页渲染器（wp-proc 子进程 + WpProcIpcClient） |
| mode_dispatch.rs | 407 | determine_wallpaper_mode 集中决策、WallpaperEngine::set_wallpaper 同步路径 |
| gif_memory.rs | 369 | GifRenderData（frames/current_frame/memory_strategy，handle_pause/handle_resume 4 策略） |
| subprocess_base.rs | 262 | SubprocessRendererBase（ProcessManager/state/hwnd/pipe_name/pause_sender，find_window_by_title/find_window_by_class） |
| gdi_cache.rs | 129 | GdiCache 共享 GDI 双缓冲缓存（mem_dc/mem_bitmap/old_bitmap） |

### 测试覆盖

测试分布广泛但存在倾斜：mod.rs（~660 行测试，覆盖 calculate_scaling、枚举 serde、PauseSender 状态/广播/版本号、spawn_proc_exit_monitor 集成测试）、manager.rs（~750 行测试，含 MockRenderer 状态管理测试与 W-008 回退逻辑测试）、fast_path.rs（~500 行测试，覆盖快速路径与 PauseReason 位图协调）、gif_memory.rs（~150 行测试，4 种内存策略单元测试）、subprocess_base.rs（~55 行测试）、mode_dispatch.rs（~200 行测试，含模式判定与同步路径测试）、video.rs（~280 行测试，含 validate_renderer_speed 与 WASAPI 判断逻辑）、gif.rs（~250 行测试）、gif_decode.rs（~190 行测试）、image.rs（~220 行测试）、web.rs（~180 行测试）、gdi_base.rs（~150 行测试）。涉及 Win32 API 的失败分支普遍通过文档化注释标记验证，无法在 CI 中可靠触发真实失败分支。`paused_displays` 方法已被 v6.0 清理（见 W-TD-005）。

## v4.0 审查发现（13 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/03-wallpaper.md`
> 严重级别分布：Critical 0 / High 4 / Medium 5 / Low 4
> 维度分布：架构 1 | 逻辑 2 | 并发 2 | 资源 2 | 错误 2 | 性能 1 | 安全 1 | 可维护性 2
> **综述：13 项 v4.0 findings 已全部核验为已修复/已决策处理，无遗留待修复项。**

### 审查重点说明

wallpaper 模块是项目最大且最复杂的模块，整体架构清晰：`WallpaperRenderer` trait 抽象统一了四种壁纸类型的生命周期接口；`WallpaperEngine` 集中管理多显示器壁纸实例与状态机。模块经过 v3.0→v3.5 共 5 轮修复（W01-W13、T09、N-004/N-005 等），并发设计成熟——`PauseReason` 位图协调多暂停源、W06 TOCTOU 修复、W07 子进程退出监听、W09 解码取消机制等均有对应单元测试覆盖。本次审查聚焦于修复盲区、跨文件不一致与遗留债务。

### [W-001] [High] [资源管理] web.rs:284-324 — `WebRenderer` spawn 监听线程失败时复制句柄泄漏

**描述**：`WebRenderer::create_pause_sender` 中通过 `self.base.duplicate_process_handle()` 复制了子进程句柄（W07 修复），随后将句柄转换为 `isize` 传入监听线程闭包。但当 `std::thread::Builder::spawn` 失败时（如系统线程数达上限），闭包未执行，`CloseHandle` 永不调用，导致 **Win32 进程句柄泄漏**。

`HANDLE` 是 `*mut c_void` 的透明包装，没有 `Drop` 实现自动关闭。当 spawn 成功时线程负责 `CloseHandle`；spawn 失败时无人关闭。

**修复状态**：✅ **已修复**（核验 2026-07-25）：引入 `OwnedProcHandle` RAII 包装器（`mod.rs`），`Drop` 时调用 `CloseHandle`。spawn 成功时闭包内 `take()` 取出句柄，spawn 失败时闭包被 drop 自动关闭句柄。web.rs 与 video.rs 均改用该封装。

### [W-002] [High] [并发安全] video.rs:302-436 — `VideoRenderer` 未启动 mpv 子进程退出监听线程

**描述**：`VideoRenderer::create_pause_sender` 未启动子进程退出监听线程。`web.rs` 的 W07 修复为 wp-proc 子进程添加了 `WaitForSingleObject` 监听线程以检测异常退出并更新 engine 状态，但 `video.rs` 的 mpv 子进程没有对应机制。若 mpv 进程崩溃，engine 状态仍为 `Playing`，前端 UI 显示"播放中"但壁纸实际已停止。

**修复状态**：✅ **已修复**（核验 2026-07-25）：`VideoRenderer::create_pause_sender` 复用共享函数 `spawn_proc_exit_monitor`（mod.rs），通过 `duplicate_process_handle` 复制 mpv 句柄并 spawn 监听线程，异常退出时通过 `notify_state_changed` 更新 engine 状态。web.rs 与 video.rs 实现统一（[Consistency]-12.2 收敛）。

### [W-003] [High] [错误处理] image.rs:178-186, 488-506 / gif.rs:202-208, 524-536 — Image/GIF `resume()` 失败后状态不一致

**描述**：Image 和 GIF 渲染器的 `resume()` 方法先发送 Resume 命令再设置状态为 Playing。但壁纸线程处理 Resume 命令时，若重新加载/解码图片失败，仅记录 error 日志，render 数据保持 paused/空白像素状态，导致 engine 显示 Playing 但壁纸黑屏/冻结。

**修复状态**：✅ **已修复**（核验 2026-07-25）：`play()` 中提前创建 `pre_shared_state`/`pre_state_changed`/`display_id_lock` 三个字段，壁纸线程 Resume 加载失败时回滚 `shared_state.state = Paused` 并通过 `state_changed` 通知前端刷新 UI（image.rs:615-633、gif.rs 对应逻辑）。注：v6.0 技术债 W-TD-008/009 标记该修复模式在 image.rs 与 gif.rs 之间存在跨渲染器重复（已决策保留）。

### [W-004] [High] [逻辑] fast_path.rs:92-123, 132-163 — `pause_all_fast`/`resume_all_fast` 部分发送失败导致状态机无法自愈

**描述**：`pause_all_fast` / `resume_all_fast` 在发送命令阶段遍历所有 `pause_senders` 发送 Pause/Resume。若部分 sender 发送成功、部分失败，原实现回滚 `pause_reasons` 位图，但已成功发送 Pause 的渲染器已进入 Paused 状态。后续 `resume_all_fast` 因 bit 已回滚而 early-return，成功发送 Pause 的渲染器永久卡在 Paused 状态，无恢复路径。

**修复状态**：✅ **已修复**（核验 2026-07-25）：`pause_all_fast` 部分失败时**不回滚 bit**（保留 bit set），使 `resume_all_fast` 能观察到 bit 并发送 Resume 恢复已暂停的渲染器；`resume_all_fast` 增加幂等兜底发送 Resume。完成 v41-W-004 契约文档化。

### [W-005] [Medium] [性能] manager.rs:637-671 — `update_positions` Span 模式每次迭代重复调用 `GetSystemMetrics`

**描述**：`update_positions` 在 `Span` 模式下对每个渲染器都调用 4 次 `GetSystemMetrics`，多显示器场景下产生 4N 次系统调用。

**修复状态**：✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：`span_metrics` 缓存系统调用，4N→4 次。

### [W-006] [Medium] [逻辑] gif.rs:524-536 — GIF Resume 分支未检查 `frames.is_empty()` 直接索引

**描述**：GIF 渲染器处理 `Resume` 命令时，调用 `render.handle_resume()` 后直接索引 `render.frames[render.current_frame]` 计算定时器延迟，未做 `frames.is_empty()` 检查，若未来实现变化导致 `frames` 为空将触发 panic。

**修复状态**：✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：GIF Resume 增加 `frames.is_empty()` 守卫 + warn 日志。

### [W-007] [Medium] [逻辑] video.rs:283-290 — `VideoRenderer::set_speed` 未做有效性校验

**描述**：`VideoRenderer::set_speed` 直接将 `speed` 传递给 mpv IPC，未做有效性校验。`GifRenderer::set_speed` 已校验 `speed` 必须为正有限数，而 `VideoRenderer` 缺少对称校验。

**修复状态**：✅ **已修复**（核验 2026-07-25）：提取共享函数 `validate_renderer_speed`（合并 [Consistency]-12.1），`VideoRenderer::set_speed`（video.rs:457-461）与 `GifRenderer::set_speed` 一致调用该校验，并有完整单测覆盖（video.rs:1079-1135）。

### [W-008] [Medium] [错误处理] manager.rs:236-247 — `set_scaling_mode` 视频壁纸重新设置失败时壁纸丢失

**描述**：`set_scaling_mode` 对视频壁纸采用"先关闭再重新设置"策略。若重新设置失败，壁纸已被 `close_wallpaper` 关闭但未能恢复，该显示器壁纸丢失，engine 状态不可逆。

**修复状态**：✅ **已修复**（核验 2026-07-25）：`manager.rs` 增加失败回退逻辑——记录 `original_mode` → 关闭原壁纸 → 尝试 `set_wallpaper(new_mode)` → 失败时尝试 `set_wallpaper(original_mode)` → 回退也失败时返回双重错误提示"壁纸已停止，需手动恢复"。`wallpaper_scaling_modes` 字段记录最近成功模式用于回退。含 W-008 回退/双重失败单测。

### [W-009] [Medium] [安全] web.rs:57-73 — `build_wp_proc_args` source 参数拼接存在参数注入风险

**描述**：`WebRenderer::build_wp_proc_args` 将用户提供的 `source` 以 `--source={value}` 格式拼接到命令行参数中。若 `source` 以 `--` 开头，参数变为 `--source=--pipe-name=evil`，可能导致参数注入。

**修复状态**：✅ **已修复**（核验 2026-07-25）：`--source` 与值分离为两个独立 argv 元素（web.rs:65-66），配合 wp-proc `allow_hyphen_values = true`。含 W-009 参数注入防护单测（`--malicious` 值作为 source 值而非独立 flag）。

### [W-010] [Low] [可维护性] video.rs:27 — 函数名 `should_invoke_wasispi` 拼写错误

**描述**：函数名 `should_invoke_wasispi` 存在拼写错误，应为 `should_invoke_wasapi`（Windows Audio Session API）。

**修复状态**：✅ **已修复于 v4.0 Wave 3C**：重命名为 `should_invoke_wasapi`，同步更新所有调用点与测试函数名。

### [W-011] [Low] [并发安全] mod.rs:120-148, manager.rs:188, video.rs:149 等 — `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁可能使用不一致数据

**描述**：模块大量使用 `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁。该模式会静默忽略数据竞争导致的锁中毒，恢复后可能产生不一致数据。

**修复状态**：✅ **已修复于 v4.0 Wave 3C**（文档化决策权衡，未改动锁恢复逻辑）：`mod.rs` 顶部文档化该决策的权衡（保留中毒数据 vs 默认值回退），并注明未来对配置类数据评估默认回退。

### [W-012] [Low] [架构] manager.rs:318-349 — `prepare_set_wallpaper` 中 `clear_native` 字段恒为 false 增加认知负担

**描述**：`SetWallpaperPending.clear_native` 字段在 `prepare_set_wallpaper` 路径中始终为 false，仅 `create_and_play_renderer`（异步路径）硬编码传 `true`。字段存在但恒为某值增加认知负担。

**修复状态**：✅ **已修复于 v4.0 Wave 3C**（注释增强方案 ②，标注 W-012 + "此路径下恒为 false"）。

### [W-013] [Low] [可维护性] subprocess_base.rs:165-192, 205-252 — 窗口查找轮询参数为魔法数字

**描述**：`find_window_by_title` 和 `find_window_by_class` 中轮询 20 次、间隔 100ms 的参数以魔法数字硬编码。

**修复状态**：✅ **已修复于 v4.0 Wave 3C**：提取 `WINDOW_FIND_RETRIES`/`WINDOW_FIND_INTERVAL_MS` 常量。

### v4.0 findings 修复汇总

| ID | 级别 | 状态 |
|----|------|------|
| W-001 | High | ✅ 已修复（OwnedProcHandle RAII） |
| W-002 | High | ✅ 已修复（spawn_proc_exit_monitor） |
| W-003 | High | ✅ 已修复（Resume 失败回滚）+ 记录重复模式 |
| W-004 | High | ✅ 已修复（部分失败不回滚 bit + 兜底 Resume） |
| W-005 | Medium | ✅ 已修复（Wave 2A） |
| W-006 | Medium | ✅ 已修复（Wave 2A） |
| W-007 | Medium | ✅ 已修复（validate_renderer_speed） |
| W-008 | Medium | ✅ 已修复（original_mode 回退） |
| W-009 | Medium | ✅ 已修复（拆分 argv） |
| W-010 | Low | ✅ 已修复（Wave 3C） |
| W-011 | Low | ✅ 已修复（文档化决策） |
| W-012 | Low | ✅ 已修复（Wave 3C 注释） |
| W-013 | Low | ✅ 已修复（Wave 3C） |

## v3.x 已修复问题

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| W01 | Medium | `update_positions` Span 模式负坐标 clamp 到 0 导致跨屏定位错误 | ✅ 已修复（v3.5.2） |
| W02 | Medium | `ImageRenderer` trait `pause()`/`resume()` 仅设状态不发命令，与 GifRenderer 不一致 | ✅ 已修复（v3.5.2） |
| W03 | Low | `decode_gif` max_frames 使用屏幕分辨率而非实际帧尺寸，小帧 GIF 帧数被过度限制 | ✅ 已修复（v3.5.3） |
| W04 | Medium | `set_wallpaper` 同步路径持锁期间调用 `play()` 阻塞 engine（最长 20s） | ✅ 已修复（v3.5.2）— play 移出锁外 |
| W05 | Low | `ensure_desktop_ready_with_retry` 方法与 `ensure_desktop_ready` 自由函数 ~40 行重复 | ✅ 已修复（v3.5.3）— 提取 `ensure_desktop_ready_impl` |
| W06 | Medium | `pause_all_fast` TOCTOU 窗口导致并发 resume 丢失 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 W-004 发现部分失败回滚不完整 |
| W07 | Medium | `WebRenderer::play` 不监听 wp-proc 退出事件，状态不一致 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 W-001 发现 spawn 失败句柄泄漏；W-002 发现 video.rs 缺失同机制 |
| W08 | Low | `register_window_class_once` 丢弃 `RegisterClassW` 返回值，真实失败被掩盖 | ✅ 已修复（v3.5.3） |
| W09 | Low | GIF 后台解码线程无取消机制，窗口销毁后仍继续解码 | ✅ 已修复（v3.5.3）— `AtomicBool` 取消标志 |
| W10 | Low | `GifRenderer::set_speed` 不校验 speed > 0，speed=0 导致极快播放 | ✅ 已修复（v3.5.3）— ⚠️ v4.0 W-007 发现 video.rs 缺失对称校验 |
| W11 | Low | `VideoRenderer` pause 线程 COM 失败后 WASAPI 音量操作静默失败 | ✅ 已修复（v3.5.3）— `com_initialized: Arc<AtomicBool>` 降级标志 |
| W12 | Low | `decode_gif` 使用 `std::fs::read` 全文件入内存，大 GIF 峰值内存高 | ✅ 已修复（v3.5.3）— 改用 `BufReader` 流式读取 |
| W13 | Medium | `close_wallpaper_by_path` 精确字符串比较，Windows 路径格式差异导致匹配失败 | ✅ 已修复（v3.5.2）— 路径规范化比较 |
| 0 单元测试 | — | 增加 wallpaper 模块单元测试 | ✅ 已修复（v1.0） |
| GDI 缓存重复 | — | 提取为 `gdi_cache.rs` 共享组件 | ✅ 已修复（v1.0） |
| WallpaperMode 选择分散 | — | 集中为 `determine_wallpaper_mode()` | ✅ 已修复（v1.0） |
| 暂停像素释放不统一 | — | Image/Gif 均使用 `GdiCache.release_bitmap()` | ✅ 已修复（v1.0） |

## v6.0 技术债清单及清理状态（合并）

> 审查日期：2026-07-25 | 清理日期：2026-07-25
> 下表为原「技术债清单（2.1-2.9）」与「清理状态（第 6 节）」合并后的规范化表，每个 W-TD 项仅保留一行且带唯一清理状态。类型标注对应原清单分类。

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| W-TD-001 | 死代码 | mod.rs:279 | `WallpaperState::Error` 变体仅在 serde roundtrip 测试出现，生产无代码置 Error；前端需处理永不出现的状态 | 删除 `Error` 变体 + 同步 serde 测试与前端 TS 类型（低） | ✅ 已修复于 v6.0 | 删除 `WallpaperState::Error` 变体，前端类型同步 |
| W-TD-002 | 死代码 | mod.rs:575 | `PauseReason::USER` 仅测试用，生产仅用 FULLSCREEN/BATTERY/TRAY | 评估保留（doc 注明）或移除改 TRAY（低） | ✅ 已决策保留 | `PauseReason::USER` 保留，doc 注明"仅测试使用" |
| W-TD-003 | 死代码 | mod.rs:663-664 | `navigate` trait 方法默认 no-op，全项目无 trait 对象调用 | 移除 `navigate` trait 方法（低） | ✅ 已修复于 v6.0 | 移除 `navigate` trait 方法，WebRenderer 保留为非 trait 方法 |
| W-TD-004 | 死代码 | fast_path.rs:274-278 | `any_playing` 仅测试调用，生产无调用点 | 删除该方法（低） | ✅ 已修复于 v6.0 | 删除 `any_playing` 方法及测试 |
| W-TD-005 | 死代码 | fast_path.rs:286-292 | `paused_displays` 仅测试调用，生产无调用点 | 删除该方法（低） | ✅ 已修复于 v6.0 | 删除 `paused_displays` 方法及测试 |
| W-TD-006 | 冗余抽象 | manager.rs:1075-1077 | `ensure_desktop_ready` 单行委托，两个 1 行入口理解成本高 | 评估合并或 doc 明确两入口适用场景（中） | ✅ 已决策保留 | `ensure_desktop_ready` 保留，doc 明确两入口场景 |
| W-TD-007 | 冗余抽象 | manager.rs:1054-1061 | `create_and_play_renderer` 单行 `construct_renderer(...,true)`，doc 重复 | 保留注明等价关系，或内联（低） | ✅ 已决策保留 | `create_and_play_renderer` 保留，doc 注明等价关系 |
| W-TD-008 | 重复实现 | image.rs:76-85; gif.rs:112-121 | W-003 修复三字段（`pre_shared_state`/`pre_state_changed`/`display_id_lock`）两个 GDI 渲染器完全重复 | 抽取 `GdiRendererPreState` 共享结构体（中） | ✅ 已决策保留 | 抽取成本高，保留现状并补注释 |
| W-TD-009 | 重复实现 | image.rs:276-284; gif.rs:358-366 | `pre_shared_state`/`pre_state_changed` fallback 模式两处重复 | 依赖 W-TD-008 收敛，或抽辅助函数（低） | ✅ 已决策保留 | 依赖 W-TD-008，同上 |
| W-TD-010 | 重复实现 | image.rs:24-35; gif.rs:45-57 | `WallpaperCommand`/`GifCommand` 枚举几乎相同，仅 GifCommand 多 SetSpeed | 评估统一为单一 `GdiCommand` 枚举（中） | ✅ 已决策保留 | 命令枚举统一成本高，保留现状并补注释 |
| W-TD-011 | 过时模式 | mod.rs:648-651 | `set_volume` trait no-op，仅 VideoRenderer 覆盖，全项目无 trait 对象调用 | 移除 `set_volume` trait 方法及覆盖（低） | ✅ 已修复于 v6.0 | 移除 `set_volume` trait 方法，音量走快速路径 |
| W-TD-012 | 过度设计 | manager.rs:227-229,420-434 | `set_wallpaper_results` `#[cfg(test)]` 测试钩子字段 | 评估提取为纯函数，或保留精简 doc（中） | ✅ 已决策保留 | `set_wallpaper_results` 测试钩子保留，精简 doc |
| W-TD-013 | 修复痕迹 | manager.rs:34,36,40,167,263,265,285,294,474,1319,1329 | 11 处 "T09：" 前缀注释，无对应 spec | 移除前缀，保留设计理由（低） | ✅ 已修复于 v6.0 | 移除 11 处 `T09：` 前缀 |
| W-TD-014 | 修复痕迹 | manager.rs:1100,1118 | 2 处 "T14：" 前缀注释 | 移除前缀（低） | ✅ 已修复于 v6.0 | 移除 2 处 `T14：` 前缀 |
| W-TD-015 | 修复痕迹 | mode_dispatch.rs:118 | `set_wallpaper` doc 引用未实施的 Phase 3 方案 | 评估 Phase 3 是否仍为路线图项，否则移除（低） | ✅ 已决策移除 | Phase 3 引用为历史残留，移除 |
| W-TD-016 | 修复痕迹 | mod.rs,web.rs,video.rs,gif.rs,gdi_base.rs,image.rs,manager.rs | "Bug #2/#3/#5/#7"、"Wave 1 W-001/W-002" 历史标记散布 26 处 | 评估保留/移除，无 issue 链接则移除前缀（低） | ✅ 已修复于 v6.0 | 移除 26 处 `Bug #`/`Wave 1` 历史标记前缀 |
| W-TD-017 | 命名一致性 | image.rs:24; gif.rs:45 | 命令枚举命名不一致：`WallpaperCommand` vs `GifCommand` | 统一为 `GdiCommand`；依赖 W-TD-010（中） | ✅ 已决策保留 | 依赖 W-TD-010，同上 |
| W-TD-018 | 命名一致性 | image.rs:21; gif.rs:23 | WM 消息常量命名不一致（`WM_WALLPAPER_COMMAND` vs `WM_GIF_COMMAND`）+ 不同偏移 | 统一命名并在 `GdiRendererBase` 定义共用常量（低） | ✅ 已决策保留 | WM 常量偏移量已隔离，统一风险高于收益 |
| W-TD-019 | 注释陈旧 | manager.rs:54 | `SetWallpaperPending` doc "最长达 20s" 已过时（实际 8s） | 将 "20s" 修正为 "8s"（低） | ✅ 已修复于 v6.0 | "20s" 修正为 "8s" |

> 补充：未使用导入为「无」，经 Grep 验证 13 文件 `use` 项均有实际调用点；`determine_wallpaper_mode`/`WallpaperMode` 重导出均在测试与命令层使用。

#### 清理统计

- **总技术债**：19 项
- **已修复**：11 项（57.9%）
- **已决策保留**：8 项（42.1%）
- **完成率**：100%

#### 验证结果

- `cargo test -p mirrorstar-core wallpaper::`：全部通过
- `cargo clippy --workspace --all-targets -- -D warnings`：零警告
- `cargo check --workspace`：编译通过
- `npx vitest run` + `npm run lint` + `npm run typecheck`：全部通过

## 优化机会与交集汇总

### v6.0 优化机会（非技术债类改进点）

- **`spawn_proc_exit_monitor` 的可观测性**：当前子进程退出监听仅记录 `info!`/`warn!` 日志，可考虑增加 metric counter（`wp_proc_exit_total{type="normal|abnormal"}` / `mpv_proc_exit_total`）量化子进程崩溃率，验证 W-001/W-002 修复效果。
- **`PauseSender::state_version` 的前端集成**：`state_version` 计数器目前仅在后端 `PauseSender` 中维护，前端通知 payload 未携带 version。可评估将 version 编入 payload，使前端无需旁路查询即可丢弃旧版本事件。
- **`GdiRendererBase` 的进一步抽象**：ImageRenderer 和 GifRenderer 的 `play()`/`pause()`/`resume()`/`create_pause_sender()` 实现高度相似（W-TD-008/009），可评估将更多公共逻辑下沉到 `GdiRendererBase`，类似 `SubprocessRendererBase` 对 VideoRenderer/WebRenderer 的抽象程度。
- **`WallpaperEngine::set_wallpaper` 同步路径的锁竞争**：`set_wallpaper` 同步路径在 engine 锁内调用 `construct_renderer`（含 IPC 等待），与异步三阶段路径形成两套并行实现。可评估统一为三阶段路径，移除同步便利方法。

### v4.0 ↔ v6.0 交集

- **W-003 ⇒ W-TD-008/009**：v4.0 W-003 修复（提前创建 shared_state/state_changed 供壁纸线程回滚）在 image.rs 与 gif.rs 中引入了 `pre_shared_state`/`pre_state_changed`/`display_id_lock` 模式，v6.0 标记了该模式的跨渲染器重复问题（已决策保留，成本高于收益）。
- **W-001/W-002 ⇒ spawn_proc_exit_monitor**：v4.0 的 W-001（句柄泄漏）与 W-002（video 缺失退出监听）通过 `OwnedProcHandle` RAII + 共享函数 `spawn_proc_exit_monitor` 一并解决，且收敛了 web.rs 与 video.rs 的实现差异（[Consistency]-12.2）。优化机会中的可观测性改进即基于此。
- **W-005 v5.0 屏幕分辨率缓存**：v5.0 W-PERF-003（屏幕分辨率缓存）已在 mod.rs:40-83 实施，非技术债。
- **W-011 ⇒ 锁中毒策略文档化**：v4.0 W-011 与 v5.0 的锁中毒恢复策略在 mod.rs/manager.rs 顶部一致性文档化，非技术债。

### v4.0 优先修复清单回顾

> 以下 v4.0 优先修复项均已核验为完成，仅作历史归档勿再实施。

1. **W-001 句柄泄漏**：✅ 已修复 — `OwnedProcHandle` RAII 封装。
2. **W-002 video 子进程退出监听**：✅ 已修复 — 复用 `spawn_proc_exit_monitor`。
3. **W-003 Resume 失败状态回滚**：✅ 已修复 — 壁纸线程回滚 `shared_state.state = Paused` + 前端通知。
4. **W-004 pause_all 部分失败回滚**：✅ 已修复 — 部分失败不回滚 bit + `resume_all_fast` 兜底。