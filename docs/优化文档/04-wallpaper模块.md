# wallpaper 模块优化文档

> [← 返回索引](./README.md)

## 模块概要

- **模块路径**：`crates/mirrorstar-core/src/wallpaper/`
- **审查文件**：13 个（约 7,900 行）
  - `mod.rs`（624 行）— 模块入口、`WallpaperState`/`ScalingMode`/`GifMemoryStrategy`/`PauseCommand`/`PauseSender`、`WallpaperRenderer` trait、`calculate_scaling`、`create_pause_channel`、`PauseReason` 位图
  - `manager.rs`（1416 行）— `WallpaperEngine`、`prepare_for_wallpaper`、`embed_and_register_renderer`、`close_wallpaper`、`update_positions`、`create_and_play_renderer`（自由函数）、`ensure_desktop_ready`（自由函数）
  - `gdi_cache.rs`（152 行）— `GdiCache` 共享 GDI 双缓冲缓存（mem_dc/mem_bitmap/old_bitmap）
  - `image.rs`（605 行）— `ImageRenderer` 图片渲染器（GdiRendererBase + 专用线程消息循环 + WM_WALLPAPER_COMMAND）
  - `gif.rs`（786 行）— `GifRenderer` GIF 渲染器（WM_TIMER 驱动帧推进 + 后台解码线程 + 4 种内存管理策略）
  - `video.rs`（473 行）— `VideoRenderer` 视频渲染器（mpv 子进程 + MpvIpcClient + WASAPI 音量控制 + pause 线程 COM MTA）
  - `web.rs`（247 行）— `WebRenderer` 网页渲染器（wp-proc 子进程 + WpProcIpcClient）
  - `fast_path.rs`（475 行）— `WallpaperEngine` 快速通道扩展（`pause_all_fast`/`resume_all_fast`/`set_volume_fast`/`toggle_mute_fast`，`PauseReason` 位图协调）
  - `gdi_base.rs`（488 行）— `GdiRendererBase` 共享基类（`paint_with_double_buffer`、`spawn_pause_forwarder`、`register_window_class_once`、`create_wallpaper_window`）
  - `gif_decode.rs`（239 行）— `decode_gif` 全帧解码（40MB 内存预算 + 降采样）、`decode_gif_first_frame` 首帧快速解码
  - `gif_memory.rs`（381 行）— `GifRenderData`（frames/current_frame/memory_strategy，handle_pause/handle_resume 4 策略）
  - `mode_dispatch.rs`（463 行）— `determine_wallpaper_mode` 集中决策、`WallpaperEngine::set_wallpaper` 同步路径
  - `subprocess_base.rs`（243 行）— `SubprocessRendererBase`（ProcessManager/state/hwnd/pipe_name/pause_sender，`find_window_by_title`/`find_window_by_class`）
- **单元测试**：140 个
- **核心结构**：
  - `WallpaperEngine`：管理所有壁纸实例，提供 `set_wallpaper`/`pause_all_fast`/`resume_all_fast`/`shutdown` 等方法
  - `WallpaperRenderer` trait：统一渲染器接口（`play`/`pause`/`resume`/`set_volume`/`set_position`/`set_scaling_mode`/`set_speed`/`state`）
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

## v4.0 审查发现（13 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/03-wallpaper.md`
> 严重级别分布：Critical 0 / High 4 / Medium 5 / Low 4
> 维度分布：架构 1 | 逻辑 2 | 并发 2 | 资源 2 | 错误 2 | 性能 1 | 安全 1 | 可维护性 2

### 审查重点说明

wallpaper 模块是项目最大且最复杂的模块，整体架构清晰：`WallpaperRenderer` trait 抽象统一了四种壁纸类型的生命周期接口；`WallpaperEngine` 集中管理多显示器壁纸实例与状态机。模块经过 v3.0→v3.5 共 5 轮修复（W01-W13、T09、N-004/N-005 等），并发设计成熟——`PauseReason` 位图协调多暂停源、W06 TOCTOU 修复、W07 子进程退出监听、W09 解码取消机制等均有对应单元测试覆盖。本次审查聚焦于修复盲区、跨文件不一致与遗留债务。

### [W-001] [High] [资源管理] web.rs:284-324 — `WebRenderer` spawn 监听线程失败时复制句柄泄漏

**描述**：`WebRenderer::create_pause_sender` 中通过 `self.base.duplicate_process_handle()` 复制了子进程句柄（W07 修复），随后将句柄转换为 `isize` 传入监听线程闭包。但当 `std::thread::Builder::spawn` 失败时（如系统线程数达上限），闭包未执行，`CloseHandle` 永不调用，导致 **Win32 进程句柄泄漏**。

`HANDLE` 是 `*mut c_void` 的透明包装，没有 `Drop` 实现自动关闭。`proc_handle.0 as isize` 仅复制了指针值，原 `HANDLE` 不会因离开作用域而关闭。当 spawn 成功时线程负责 `CloseHandle`；spawn 失败时无人关闭。

**建议**：在 spawn 失败的 `Err` 分支中显式调用 `CloseHandle(proc_handle)` 关闭已复制的句柄；或使用 RAII 包装器（如 `OwnedHandle`）管理句柄生命周期，确保无论 spawn 成败都不泄漏。

### [W-002] [High] [并发安全] video.rs:302-436 — `VideoRenderer` 未启动 mpv 子进程退出监听线程

**描述**：`VideoRenderer::create_pause_sender` 未启动子进程退出监听线程。`web.rs` 的 W07 修复为 wp-proc 子进程添加了 `WaitForSingleObject` 监听线程以检测异常退出并更新 engine 状态，但 `video.rs` 的 mpv 子进程没有对应机制。注释（web.rs:283）提到"与 video.rs 的 mpv 退出恢复机制统一"，但 video.rs 实际并未实现该机制。

若 mpv 进程崩溃（GPU 驱动崩溃、视频文件解码异常等），engine 状态仍为 `Playing`，`pause_senders` 中仍保留该显示器的 sender，前端 UI 显示"播放中"但壁纸实际已停止。用户无法感知壁纸已停止，也无法通过 UI 恢复。

**建议**：参照 `web.rs:284-324` 的 W07 实现，在 `VideoRenderer::create_pause_sender` 中使用 `duplicate_process_handle()` 复制 mpv 进程句柄并 spawn 监听线程，检测异常退出后通过 `notify_state_changed` 通知 engine 更新状态。同时修复 W-001 中句柄泄漏问题。

### [W-003] [High] [错误处理] image.rs:178-186, 488-506 / gif.rs:202-208, 524-536 — Image/GIF `resume()` 失败后状态不一致

**描述**：Image 和 GIF 渲染器的 `resume()` 方法**先发送 Resume 命令再设置状态为 Playing**，pause 转发线程收到 Resume 后也设置 `shared_state.state = Playing`。但壁纸线程处理 Resume 命令时，若**重新加载/解码图片失败**（image.rs:500-503）或 **GIF `handle_resume` 解码失败**（gif.rs:529-531），仅记录 error 日志，render 数据保持 paused/空白像素状态。

此时存在**状态不一致**：
- engine `base.state()` = `Playing`（resume() 已设置）
- `shared_state.state` = `Playing`（pause 转发线程已设置）
- 实际渲染：暂停态，pixels 为空（image）或帧未重新加载（gif）

前端 UI 显示"播放中"，但壁纸黑屏/冻结。用户需手动暂停再恢复才能重试。

**建议**：Resume 处理失败时，通过 `PauseSender::set_state` 将 `shared_state.state` 回滚为 `Paused`，并通过 `notify_state_changed` 通知前端刷新 UI，使前端状态与实际渲染一致。或在 pause 转发线程中增加 Resume 结果反馈机制（如双向通道），仅在实际成功后才更新 shared_state。

### [W-004] [High] [逻辑] fast_path.rs:92-123, 132-163 — `pause_all_fast`/`resume_all_fast` 部分发送失败导致状态机无法自愈

**描述**：`pause_all_fast` / `resume_all_fast` 在发送命令阶段（锁已释放）遍历所有 `pause_senders` 发送 Pause/Resume。若**部分 sender 发送成功、部分失败**，当前实现：回滚 `pause_reasons` 位图（`*reasons &= !reason`），但**已成功发送 Pause 的渲染器已进入 Paused 状态**。

后续调用 `resume_all_fast(reason)` 时，`reasons.contains(reason)` 返回 `false`（bit 已回滚），**直接 early-return 不发送 Resume**。结果：成功发送 Pause 的渲染器永久卡在 Paused 状态，engine 认为未暂停（bit clear），无恢复路径。

虽然 `unbounded_sender.send` 仅在 receiver drop（pause 转发线程 panic/退出）时失败，概率较低，但一旦发生部分失败，状态机无法自愈。

**建议**：部分失败时，在回滚 bit 前向**已成功发送 Pause 的渲染器**发送 Resume 命令回滚其状态；或不在部分失败时回滚 bit，而是保留 bit 并将失败的 display_id 从 `pause_senders` 中清理（因为 sender 失败意味着渲染器已不可用）。同时建议 `resume_all_fast` 增加兜底：即便 bit 未设置也向所有 sender 发送 Resume（幂等操作，无害）。

### [W-005] [Medium] [性能] manager.rs:637-671 — `update_positions` Span 模式每次迭代重复调用 `GetSystemMetrics`

**描述**：`update_positions` 在 `Span` 模式下，对**每个**渲染器都调用 4 次 `GetSystemMetrics`（SM_XVIRTUALSCREEN / SM_YVIRTUALSCREEN / SM_CXVIRTUALSCREEN / SM_CYVIRTUALSCREEN）。虚拟屏幕尺寸对所有显示器相同，应在循环外缓存。

多显示器场景下（N 个渲染器），产生 4N 次系统调用。虽然 `GetSystemMetrics` 开销低，但在显示器配置变化频繁时（如热插拔）会累积。

**建议**：在 `for` 循环前根据 `self.arrangement` 缓存 `GetSystemMetrics` 的返回值，循环内直接使用缓存值。

> ✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：`span_metrics` 缓存系统调用，4N→4 次

### [W-006] [Medium] [逻辑] gif.rs:524-536 — GIF Resume 分支未检查 `frames.is_empty()` 直接索引

**描述**：GIF 渲染器处理 `Resume` 命令时，调用 `render.handle_resume()` 后**直接索引 `render.frames[render.current_frame]`**（第 532 行）计算定时器延迟。虽然当前 `handle_resume` 的错误路径不修改 `frames`（失败时保留原有帧），但未做 `frames.is_empty()` 检查，若未来 `handle_resume` 实现变化导致 `frames` 为空，将触发 panic。

类似地，`WM_TIMER` 处理（gif.rs:670-674）在 `!render.paused` 分支中也有 `render.frames.is_empty()` 检查（第 671-672 行），但 Resume 分支缺少对称的守卫。

**建议**：在 `handle_resume()` 后增加 `if render.frames.is_empty() { return LRESULT(0); }` 守卫，与 `WM_TIMER` 分支保持一致。或使用 `render.frames.get(render.current_frame)` 安全索引。

> ✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：GIF Resume 增加 `frames.is_empty()` 守卫 + warn 日志

### [W-007] [Medium] [逻辑] video.rs:283-290 — `VideoRenderer::set_speed` 未做有效性校验

**描述**：`VideoRenderer::set_speed` 直接将 `speed` 传递给 mpv IPC，**未做有效性校验**。`GifRenderer::set_speed`（gif.rs:237-251，W10 修复）已校验 `speed` 必须为正有限数（拒绝 0、负数、NaN、Infinity），但 `VideoRenderer` 缺少对称校验。

`speed=0` 在 mpv 中等同于暂停（可能引发与 pause 命令的状态冲突），NaN 会被 mpv JSON 解析器拒绝（返回错误），但仍产生了无效 IPC 往返。

**建议**：在 `VideoRenderer::set_speed` 开头增加与 `GifRenderer` 一致的校验（`!speed.is_finite() || speed <= 0.0`），保持两个渲染器的行为一致。

> ✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：提取 `validate_renderer_speed` 共享函数（合并 [Consistency]-12.1）

### [W-008] [Medium] [错误处理] manager.rs:236-247 — `set_scaling_mode` 视频壁纸重新设置失败时壁纸丢失

**描述**：`set_scaling_mode` 对视频壁纸采用"先关闭再重新设置"策略以应用新缩放模式（需重启 mpv 进程）。若 `self.set_wallpaper(...)` 重新设置失败（如 mpv 启动失败、IPC 连接超时），壁纸已被 `close_wallpaper` 关闭但未能恢复，**该显示器壁纸丢失**。

错误向上传播，调用方收到错误，但 engine 状态已不可逆（该显示器无壁纸）。用户需手动重新设置壁纸。

**建议**：考虑在重新设置失败时回退到原缩放模式（记录原 mode，重新 set_wallpaper 使用原 mode），或至少在错误信息中提示用户壁纸已停止需手动恢复。更优方案是记录原状态并在失败时尝试恢复。

> ✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：`set_scaling_mode` 失败回退到原 mode + 双重错误信息

### [W-009] [Medium] [安全] web.rs:57-73 — `build_wp_proc_args` source 参数拼接存在参数注入风险

**描述**：`WebRenderer::build_wp_proc_args` 将用户提供的 `source`（URL 或文件路径）以 `--source={value}` 格式拼接到命令行参数中。若 `source` 以 `--` 开头（如 `--pipe-name=evil`），参数变为 `--source=--pipe-name=evil`，wp-proc 的参数解析器可能将其中的 `--pipe-name=evil` 解析为独立参数，导致参数注入。

虽然 `source` 通常来自用户配置（可信度较高），但若用户粘贴恶意 URL 或文件名，可能影响 wp-proc 的参数解析。`pipe_name` 和 `title` 由 `uuid::Uuid` 生成，无注入风险。

**建议**：将 `--source` 与值分离为两个独立参数（`--source`, &self.source`），依赖 wp-proc 的参数解析器将后续值视为位置参数而非 flag。或对 source 做前缀校验（拒绝以 `-` 开头的值）。

> ✅ **已修复于 v4.0 Wave 2A**（spec: `fix-v40-wave2a-wallpaper-medium-findings`）：分离 argv + wp-proc `allow_hyphen_values = true`

### [W-010] [Low] [可维护性] video.rs:27 — 函数名 `should_invoke_wasispi` 拼写错误

**描述**：函数名 `should_invoke_wasispi` 存在拼写错误，应为 `should_invoke_wasapi`（Windows Audio Session API）。该函数在 video.rs 中被调用 2 次（第 379、411 行），测试中调用 6 次（第 574-622 行），且测试函数名也使用了错误拼写（如 `w11_skip_wasispi_when_com_not_initialized`）。

注释中正确使用了"WASAPI"（第 18、21、26 行等），但标识符与注释不一致，可能误导维护者。

**修复状态**：✅ 已修复于 v4.0 Wave 3C

**建议**：将函数名重命名为 `should_invoke_wasapi`，同步更新所有调用点与测试函数名。由于该函数为 `fn`（非 `pub`），重命名仅影响当前文件，无跨模块影响。

### [W-011] [Low] [并发安全] mod.rs:120-148, manager.rs:188, video.rs:149 等 — `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁可能使用不一致数据

**描述**：模块中大量使用 `unwrap_or_else(|e| e.into_inner())` 恢复中毒的 `std::sync::Mutex` / `RwLock`。这是**有意设计**（避免 panic 传播，保证可用性），在 mod.rs:122-124、manager.rs:188、video.rs:149 等处一致应用。但该模式会**静默忽略数据竞争导致的锁中毒**：若 Mutex 因 panic 中毒，内部数据可能处于不一致状态，恢复后继续使用可能产生错误结果。

此模式在快速路径（pause/resume/volume）中合理（优先保证壁纸控制可用），但在 `gif_config`（manager.rs:188）等配置数据上使用时，若中毒原因是配置写入 panic，恢复的可能是半写入的策略值。

**修复状态**：✅ 已修复于 v4.0 Wave 3C（文档化决策权衡，未改动锁恢复逻辑）

**建议**：当前设计可接受（优先可用性），建议在文档中明确记录该决策的权衡。对于配置类数据（如 `gif_config`），可考虑中毒时回退到默认值而非恢复中毒数据。

### [W-012] [Low] [架构] manager.rs:318-349 — `prepare_set_wallpaper` 中 `clear_native` 字段恒为 false 增加认知负担

**描述**：`prepare_set_wallpaper` 计算 `clear_native` 标志（第 342 行），但由于此前 `close_wallpaper`（第 326 行）已移除 `wallpaper_mode` 记录，`is_native_mode` 始终返回 `false`。代码注释（第 337-341 行）已说明此行为，但 `SetWallpaperPending.clear_native` 字段在 `prepare_set_wallpaper` 路径中**始终为 false**，仅 `create_and_play_renderer`（异步路径，第 861 行硬编码 `true`）会传入 `true`。

这种"字段存在但恒为某值"的模式增加了认知负担——读者需追踪 `close_wallpaper` 副作用才能理解为何 `clear_native` 总是 false。

**修复状态**：✅ 已修复于 v4.0 Wave 3C（注释增强方案 ②，标注 W-012 + "此路径下恒为 false"）

**建议**：考虑在 `prepare_set_wallpaper` 中在调用 `close_wallpaper` **前**记录原始 `is_native_mode` 值作为 `clear_native`，使该字段在两条路径中均有意义；或在注释中更明确地标注"此路径下 clear_native 恒为 false，仅 create_and_play_renderer 使用 true"。

### [W-013] [Low] [可维护性] subprocess_base.rs:165-192, 205-252 — 窗口查找轮询参数为魔法数字

**描述**：`find_window_by_title` 和 `find_window_by_class` 中的轮询参数（轮询 20 次、间隔 100ms）以魔法数字形式硬编码在循环中，未提取为常量。注释中说明了"轮询 20 次，每次间隔 100ms（最大 2 秒）"，但值本身内联在 `for _ in 0..20` 和 `Duration::from_millis(100)` 中。

对比 video.rs/web.rs 中 IPC 重试参数已提取为命名常量（`MPV_CONNECT_RETRIES=5`、`WP_PROC_CONNECT_RETRIES=100`），窗口查找的重试参数应保持一致风格。

**修复状态**：✅ 已修复于 v4.0 Wave 3C

**建议**：提取 `WINDOW_FIND_RETRIES: u32 = 20` 和 `WINDOW_FIND_INTERVAL_MS: u64 = 100` 常量，并在函数文档中说明总超时时间（2s）的计算方式。

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
| 0 单元测试 | — | 增加 wallpaper 模块单元测试 | ✅ 已修复（v1.0）— 140 个测试 |
| GDI 缓存重复 | — | 提取为 `gdi_cache.rs` 共享组件 | ✅ 已修复（v1.0） |
| WallpaperMode 选择分散 | — | 集中为 `determine_wallpaper_mode()` | ✅ 已修复（v1.0） |
| 暂停像素释放不统一 | — | Image/Gif 均使用 `GdiCache.release_bitmap()` | ✅ 已修复（v1.0） |

## 优化目标与方案

### v4.0 优先修复（High，4 项）

1. **W-001 句柄泄漏**：spawn 失败的 `Err` 分支显式 `CloseHandle(proc_handle)`，或用 `OwnedHandle` RAII 包装。
2. **W-002 video 子进程退出监听**：参照 web.rs W07 实现，复制 mpv 句柄并 spawn 监听线程，异常退出时 `notify_state_changed`。
3. **W-003 Resume 失败状态回滚**：Resume 处理失败时通过 `PauseSender::set_state` 回滚为 `Paused`，`notify_state_changed` 通知前端。
4. **W-004 pause_all 部分失败回滚**：回滚 bit 前向已成功发送 Pause 的渲染器发送 Resume；或 `resume_all_fast` 增加兜底（幂等发送 Resume）。

### v4.0 系统性修复（Medium，5 项）

5. **W-005 `GetSystemMetrics` 缓存**：循环外缓存虚拟屏幕尺寸。
6. **W-006 GIF Resume 空帧守卫**：`handle_resume()` 后增加 `frames.is_empty()` 检查。
7. **W-007 video `set_speed` 校验**：与 GifRenderer 一致的 `!speed.is_finite() || speed <= 0.0` 校验。
8. **W-008 `set_scaling_mode` 失败回退**：重新设置失败时回退原缩放模式。
9. **W-009 source 参数注入防护**：`--source` 与值分离为两个独立参数，或前缀校验。

### v4.0 渐进优化（Low，4 项）

10-13. `should_invoke_wasispi` 重命名（W-010）、中毒锁恢复策略文档化（W-011）、`clear_native` 字段语义澄清（W-012）、窗口查找轮询常量提取（W-013）。
