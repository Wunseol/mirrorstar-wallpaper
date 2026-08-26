# v6.0 技术债审查 - wallpaper 模块

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 模块路径：`crates/mirrorstar-core/src/wallpaper/`

## 1. 当前状态摘要

### 1.1 模块职责

wallpaper 模块负责壁纸渲染器抽象、状态管理与子进程句柄 RAII，是项目最大且最复杂的模块。模块通过 `WallpaperRenderer` trait 统一四种壁纸类型（`ImageRenderer` / `GifRenderer` / `VideoRenderer` / `WebRenderer`）的生命周期接口，由 `WallpaperEngine` 集中管理多显示器壁纸实例与状态机。模块分为以下几个层次：

- **核心抽象层**（`mod.rs`）：`WallpaperState` / `ScalingMode` / `GifMemoryStrategy` / `PauseCommand` / `PauseSender` / `WallpaperRenderer` trait、`calculate_scaling`、`create_pause_channel`、`PauseReason` 位图、`OwnedProcHandle` RAII、`spawn_proc_exit_monitor` 共享函数、屏幕分辨率缓存。
- **引擎层**（`manager.rs` + `mode_dispatch.rs` + `fast_path.rs`）：`WallpaperEngine` 状态机、三阶段 `set_wallpaper` 流程、Native/WorkerW 模式分发、快速控制路径（`pause_all_fast` / `resume_all_fast` / `set_volume_fast` / `toggle_mute_fast`）。
- **渲染器层**：GDI 渲染器（`gdi_base.rs` + `gdi_cache.rs` + `image.rs` + `gif.rs` + `gif_decode.rs` + `gif_memory.rs`）与子进程渲染器（`subprocess_base.rs` + `video.rs` + `web.rs`）。

### 1.2 文件清单

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

### 1.3 测试覆盖

测试分布广泛但存在倾斜：mod.rs（~660 行测试，覆盖 calculate_scaling、枚举 serde、PauseSender 状态/广播/版本号、spawn_proc_exit_monitor 集成测试）、manager.rs（~750 行测试，含 MockRenderer 状态管理测试与 W-008 回退逻辑测试）、fast_path.rs（~500 行测试，覆盖快速路径与 PauseReason 位图协调）、gif_memory.rs（~150 行测试，4 种内存策略单元测试）、subprocess_base.rs（~55 行测试）、mode_dispatch.rs（~200 行测试，含模式判定与同步路径测试）、video.rs（~280 行测试，含 validate_renderer_speed 与 WASAPI 判断逻辑）、gif.rs（~250 行测试）、gif_decode.rs（~190 行测试）、image.rs（~220 行测试）、web.rs（~180 行测试）、gdi_base.rs（~150 行测试）。涉及 Win32 API 的失败分支普遍通过文档化注释标记验证，无法在 CI 中可靠触发真实失败分支。`any_playing` / `paused_displays` 两个公共方法仅有测试调用，无生产调用方。

## 2. 技术债清单

### 2.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-001 | mod.rs:279 | `WallpaperState::Error` 枚举变体仅在 mod.rs:865 的 serde roundtrip 测试中出现，全项目（crates/ + src-tauri/）无任何代码将状态设置为 Error（Grep 验证：仅 1 处匹配，为测试） | 维护未使用的枚举变体，增加状态机复杂度；前端需处理永远不会出现的 Error 状态 | 删除 `Error` 变体；同步更新 serde 测试；前端 TS 类型定义同步移除 | 低 |
| W-TD-002 | mod.rs:575 | `PauseReason::USER` 常量仅在 `src-tauri/tests/wallpaper_flow.rs:492,496` 的集成测试中使用，生产代码（fullscreen.rs / power.rs / lib.rs tray）仅使用 FULLSCREEN / BATTERY / TRAY（Grep 验证：生产路径无 USER 调用） | 维护未使用的暂停原因变体；测试使用 USER 而生产使用 TRAY，测试与生产 reason 路径未对齐（与 ST-TD-018 关联） | 评估是否保留：若保留用于测试，在 doc 中注明"仅测试使用"；若移除，测试改用 TRAY 对齐生产 | 低 |
| W-TD-003 | mod.rs:663-664 | `WallpaperRenderer::navigate` trait 方法默认 no-op，全项目无任何代码通过 trait 对象调用 `renderer.navigate()`（Grep 验证：`.navigate(` 仅匹配 `ipc.navigate(url)` in web.rs:192，为 IPC 客户端调用，非 trait 方法） | 维护永远不会被调用的 trait 方法，增加 trait 表面；WebRenderer 通过 IPC 直接导航，trait 方法形同虚设 | 移除 `navigate` trait 方法；若未来需要可通过 trait 扩展重新添加 | 低 |
| W-TD-004 | fast_path.rs:274-278 | `WallpaperEngine::any_playing` 方法仅在 fast_path.rs:620,628,636,640 的单元测试中调用，生产代码（crates/ + src-tauri/src/）无任何调用点（Grep 验证：src-tauri/src/ 无匹配） | 维护无用的公共 API 表面，增加读者认知负担 | 删除该方法；若测试需验证播放状态，改用 `get_wallpaper_state_fast` 逐个查询 | 低 |
| W-TD-005 | fast_path.rs:286-292 | `WallpaperEngine::paused_displays` 方法仅在 fast_path.rs:652,674 的单元测试中调用，生产代码无任何调用点（Grep 验证：src-tauri/src/ 无匹配） | 同上 | 删除该方法；若测试需验证暂停状态，改用 `get_wallpaper_state_fast` 逐个查询 | 低 |

### 2.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-006 | manager.rs:1075-1077 | `ensure_desktop_ready` 自由函数方法体仅一行 `ensure_desktop_ready_impl(desktop)`，与 `WallpaperEngine::ensure_desktop_ready_with_retry`（manager.rs:679-681）完全相同的委托模式。两者都委托给同一 `ensure_desktop_ready_impl`，仅为调用方提供不同入口形态（自由函数 vs 方法） | 两个 1 行间接层委托同一实现，读者需理解为何存在两个入口；W05 修复提取 `ensure_desktop_ready_impl` 后两个 wrapper 的存在意义减弱 | 评估是否合并：保留自由函数版本（construct_renderer 锁外调用需 `&Arc` 而非 `&self`），将方法版本改为内联调用 `ensure_desktop_ready_impl(&self.desktop)`，或在 `ensure_desktop_ready_impl` doc 中明确两个入口的适用场景 | 中 |
| W-TD-007 | manager.rs:1054-1061 | `create_and_play_renderer` 自由函数方法体仅一行 `construct_renderer(source, wallpaper_type, scaling_mode, config, true)`，`clear_native` 硬编码为 `true`。doc comment 长达 30 行解释其与 `construct_renderer` 的关系 | 单行间接层，doc comment 与 `construct_renderer` 重复描述；调用方（src-tauri 命令层）可直接调用 `construct_renderer(..., true)` | 保留（异步路径调用方依赖此函数的明确语义），但在 doc 中注明"等价于 `construct_renderer(..., true)`"以避免误解；或直接内联到调用方 | 低 |

### 2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-008 | image.rs:76-85, gif.rs:112-121 | `ImageRenderer` 和 `GifRenderer` 均持有完全相同的三个 W-003 修复字段：`pre_shared_state: Option<Arc<RwLock<RendererState>>>`、`pre_state_changed: Option<tokio::sync::broadcast::Sender<String>>`、`display_id_lock: Arc<std::sync::OnceLock<String>>`，且 `play()` 中创建这些字段的逻辑几乎逐行相同（image.rs:118-161 vs gif.rs:166-239） | 两个 GDI 渲染器的 W-003 修复模式完全重复，修改一处需同步另一处；`create_pause_sender` 中复用逻辑也几乎逐行相同 | 评估抽取共享结构体（如 `GdiRendererPreState`）封装三个字段 + `play()` 中的创建逻辑 + `create_pause_sender` 中的复用逻辑，由 `GdiRendererBase` 持有或作为 mixin | 中 |
| W-TD-009 | image.rs:276-284, gif.rs:358-366 | `pre_shared_state` / `pre_state_changed` 的 fallback 模式在两个渲染器中完全重复：`take().unwrap_or_else(\|\| { warn!(...); create_pause_channel()/broadcast::channel(16).0 })` 各 8 行，仅 warn 消息中的渲染器名称不同 | 同一 fallback 逻辑两处实现，修改需同步；fallback 本身是防御性代码（pre_* 正常情况下已被 play() 设置） | 依赖 W-TD-008 抽取共享结构体后，fallback 逻辑自然收敛到一处；或抽取 `fn take_pre_state_or_fallback(name: &str) -> (Arc<...>, broadcast::Sender<...>)` 辅助函数 | 低 |
| W-TD-010 | image.rs:24-35, gif.rs:45-57 | `WallpaperCommand`（image.rs）和 `GifCommand`（gif.rs）枚举结构几乎相同：均含 `Terminate` / `SetPosition{x,y,width,height}` / `SetScalingMode(ScalingMode)` / `Pause` / `Resume` 变体，`GifCommand` 额外含 `SetSpeed(f32)` 变体。两个枚举的变体名与字段名完全一致 | 两个高度相似的命令枚举，新增命令变体时需同步修改两处；`send_command` / `terminate` 已通过泛型抽象到 `GdiRendererBase`，但命令类型本身仍重复 | 评估统一为单一 `GdiCommand` 枚举（含 `SetSpeed` 变体，ImageRenderer 的 `send_command` 忽略该变体）；或在 `GdiRendererBase` 中定义通用命令 trait | 中 |

### 2.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-011 | mod.rs:648-651 | `WallpaperRenderer::set_volume` trait 方法默认 no-op 返回 `Ok(())`，`VideoRenderer` 覆盖实现（通过 WASAPI 设置音量）。但全项目无任何代码通过 trait 对象调用 `renderer.set_volume()`（Grep 验证：crates/ + src-tauri/ 中 `.set_volume(` 仅匹配 `sender.set_volume`（PauseSender）与 `client.set_volume`（mpv IPC），无 trait 方法调用）。音量控制完全通过快速路径 `set_volume_fast` → `PauseCommand::SetVolume` 实现，trait 方法形同虚设 | 维护永远不会被调用的 trait 方法与 VideoRenderer 覆盖实现，增加 trait 表面与维护负担；与 `navigate`（W-TD-003）同属"trait 方法未被使用"模式 | 移除 `set_volume` trait 方法与 VideoRenderer 覆盖；若未来需要非快速路径的音量设置，可重新添加。或保留但在 doc 中明确"当前音量通过快速路径控制，此方法保留供未来直接调用" | 低 |

### 2.5 未使用导入

无。经 Grep 验证，13 个文件的 `use` 语句引入的项均有实际调用点。`pub mod` 声明的子模块（`fast_path` / `gdi_base` / `gdi_cache` / `gif` / `gif_decode` / `gif_memory` / `image` / `manager` / `mode_dispatch` / `subprocess_base` / `video` / `web`）均被外部 crate 与 src-tauri 引用。`pub use super::mode_dispatch::{determine_wallpaper_mode, WallpaperMode}` 重导出在 mode_dispatch.rs 测试与 src-tauri 命令层均有使用。

### 2.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-012 | manager.rs:227-229, 420-434 | `set_wallpaper_results` 测试钩子字段（`#[cfg(test)]`）通过 `std::sync::Mutex<std::collections::VecDeque<Result<(), MirrorStarError>>>` 注入 `set_wallpaper` 调用结果，用于测试 W-008 回退逻辑。`call_set_wallpaper_for_scaling` 方法在生产代码中直接调用 `self.set_wallpaper`，测试代码预填结果队列代替真实调用 | 为测试注入目的在 `WallpaperEngine` 结构体上保留 `#[cfg(test)]` 字段，增加结构体复杂度；测试钩子模式与项目其他测试（如 MockRenderer）风格不一致 | 评估替代方案：将 `set_wallpaper` 的回退逻辑提取为可独立测试的纯函数（输入 original_mode / new_mode / 两个 Result，输出最终 Result），消除对结构体测试钩子的需求。或保留但精简 doc 说明 | 中 |

### 2.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-013 | manager.rs:34,36,40,167,263,265,285,294,474,1319,1329 | 11 处 "T09：" 前缀注释描述 `GifMemoryConfig` 从 `&mut self` 改为 `&self` + 内部 Mutex 的设计理由。T09 是历史 task 编号，对当前读者无意义 | 历史任务标记散落 11 处，增加注释噪音；T09 在代码库中无对应 spec 文档 | 移除"T09："前缀，保留设计理由描述（如"内部 Mutex 加锁，允许 `&self` 更新"） | 低 |
| W-TD-014 | manager.rs:1100, 1118 | 2 处 "T14：" 前缀注释描述 `check_and_reinitialize` 返回 bool 的设计理由（"返回 bool 表示是否实际重初始化，此处不关心"）。T14 是历史 task 编号 | 同上，历史任务标记无意义 | 移除"T14："前缀，保留"返回 bool 表示是否实际重初始化"描述 | 低 |
| W-TD-015 | mode_dispatch.rs:118 | `set_wallpaper` doc comment 中"**完整修复方案**（未实施，参考 Phase 3 性能优化）：将 `construct_renderer` 拆分为 prepare（持锁）+ confirm（释放锁）"引用未实施的未来工作。Phase 3 在代码库中无对应实现或 spec | 引用不存在的未来工作，读者无法判断是否仍计划实施；与 desktop 模块 D-TD-018 同类 | 评估 Phase 3 是否仍为路线图项；若否，移除该段；若是，在路线图文档中记录并在代码注释中链接 | 低 |
| W-TD-016 | mod.rs:143,160; web.rs:212,237,252; video.rs:398,435,447; gif.rs:383; gdi_base.rs:472,517,542; image.rs:301; manager.rs:195,238,317,329,566,584,721; mod.rs:347,380,393,463,511,675 | "Bug #2/#3/#5/#7" 与 "Wave 1 W-001/W-002" 历史标记散落 26 处，引用历史 bug 编号与 wave 标记。这些标记在代码库中无对应 spec 文档（Bug #2/#3/#5/#7 未在 .trae/specs 中找到对应条目） | 历史标记对当前读者无意义，增加注释噪音；Bug # 编号无法追溯 | 评估是否保留：若 Bug # 有对应 issue tracker 条目，在 doc 中链接；若否，移除"Bug #x"前缀，保留修复描述。Wave 1 标记同理 | 低 |

### 2.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-017 | image.rs:24, gif.rs:45 | GDI 渲染器命令枚举命名不一致：`ImageRenderer` 使用 `WallpaperCommand`，`GifRenderer` 使用 `GifCommand`。两者结构几乎相同（见 W-TD-010），仅 GifCommand 多 `SetSpeed` 变体 | 同类概念两种命名，跨文件阅读时需心理映射；`WallpaperCommand` 名称过于宽泛（与 trait `WallpaperRenderer` 同前缀），`GifCommand` 名称过于具体 | 统一为 `GdiCommand`（表明归属 GDI 渲染器基类），或 `ImageCommand` / `GifCommand` 对称命名；依赖 W-TD-010 统一枚举后自然解决 | 中 |
| W-TD-018 | image.rs:21, gif.rs:23 | 窗口消息常量命名不一致：`ImageRenderer` 使用 `WM_WALLPAPER_COMMAND = WM_USER + 1`，`GifRenderer` 使用 `WM_GIF_COMMAND = WM_USER + 10`。两者功能完全相同（唤醒壁纸线程消息循环），仅偏移量与命名不同 | 同类消息常量两种命名 + 两种偏移量，增加阅读负担；偏移量 1 vs 10 无明确语义 | 统一命名风格（如 `WM_COMMAND`）并在 `GdiRendererBase` 中定义共用常量；偏移量需确保不冲突（当前 1 vs 10 已隔离，统一后需调整） | 低 |

### 2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| W-TD-019 | manager.rs:54 | `SetWallpaperPending` doc comment "传给 `construct_renderer` 在 engine 锁外执行耗时操作（含 `play()`，最长达 20s 的 IPC 连接）"中的"20s"已过时：v41-W-006 将 `WP_PROC_CONNECT_RETRIES` 从 100 次缩减到 40 次（40 * 200ms = 8s 兜底），mpv 为 30 * 200ms = 6s。当前最长 IPC 等待为 8s（web.rs WebView2 冷启动），非 20s | 注释与当前代码行为不符（实际 8s，注释说 20s），误导读者评估锁持有时间 | 将"20s"修正为"8s"（web 类型 wp-proc 连接最长 8s）；或在 doc 中分别列出 mpv 6s / wp-proc 8s 的实际超时 | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0 高收益低风险）

- W-TD-001: 删除 `WallpaperState::Error` 变体（仅 serde 测试使用，生产无设置点）
- W-TD-002: 评估 `PauseReason::USER`（仅测试使用，生产无调用；测试改用 TRAY 对齐生产或保留注明"仅测试"）
- W-TD-004: 删除 `WallpaperEngine::any_playing`（仅测试调用，生产无调用点）
- W-TD-005: 删除 `WallpaperEngine::paused_displays`（仅测试调用，生产无调用点）
- W-TD-013: 移除 11 处 "T09：" 历史任务前缀
- W-TD-014: 移除 2 处 "T14：" 历史任务前缀
- W-TD-015: 评估 "Phase 3" 引用是否仍为路线图项，若否移除
- W-TD-019: 修正 manager.rs:54 "20s" → "8s"（v41-W-006 已缩减超时）

### 3.2 谨慎清理（P1 高收益中风险）

- W-TD-003: 移除 `WallpaperRenderer::navigate` trait 方法（无 trait 调用点，WebRenderer 通过 IPC 直接导航）
- W-TD-016: 评估 26 处 "Bug #2/#3/#5/#7" 与 "Wave 1" 历史标记的保留/移除

### 3.3 评估后决定（P2 中收益）

- W-TD-006: 评估 `ensure_desktop_ready` 与 `ensure_desktop_ready_with_retry` 是否可合并入口
- W-TD-007: 评估 `create_and_play_renderer` 是否可内联到调用方
- W-TD-008: 抽取 `GdiRendererPreState` 共享结构体，收敛 ImageRenderer/GifRenderer 的 W-003 修复模式
- W-TD-009: 依赖 W-TD-008 收敛 fallback 模式
- W-TD-010: 评估统一 `WallpaperCommand`/`GifCommand` 为单一枚举
- W-TD-011: 评估移除 `WallpaperRenderer::set_volume` trait 方法（音量通过快速路径控制）
- W-TD-012: 评估 `set_wallpaper_results` 测试钩子是否可替换为纯函数测试
- W-TD-017: 统一 `WallpaperCommand`/`GifCommand` 命名风格（依赖 W-TD-010）
- W-TD-018: 统一 `WM_WALLPAPER_COMMAND`/`WM_GIF_COMMAND` 命名风格

### 3.4 长期或低收益（P3）

无。

## 4. 优化机会（非技术债类改进点）

- **`spawn_proc_exit_monitor` 的可观测性**：当前子进程退出监听仅记录 `info!`/`warn!` 日志，可考虑增加 metric counter（`wp_proc_exit_total{type="normal|abnormal"}` / `mpv_proc_exit_total`）量化子进程崩溃率，验证 W-001/W-002 修复效果。
- **`PauseSender::state_version` 的前端集成**：v41-W-005 引入的 `state_version` 计数器目前仅在后端 `PauseSender` 中维护，前端通知 payload（`display_id` 字符串）未携带 version。可评估将 version 编入 payload（如 `"{display_id}:{version}"` 或 JSON 结构），使前端无需旁路查询即可丢弃旧版本事件。
- **`GdiRendererBase` 的进一步抽象**：ImageRenderer 和 GifRenderer 的 `play()` / `pause()` / `resume()` / `create_pause_sender()` 实现高度相似（W-TD-008/009），可评估将更多公共逻辑下沉到 `GdiRendererBase`，类似 `SubprocessRendererBase` 对 VideoRenderer/WebRenderer 的抽象程度。
- **`WallpaperEngine::set_wallpaper` 同步路径的锁竞争**：mode_dispatch.rs:121 的 `set_wallpaper` 同步路径在 engine 锁内调用 `construct_renderer`（含 IPC 等待），与异步三阶段路径（`prepare_set_wallpaper` → 锁外 `construct_renderer` → `complete_set_wallpaper`）形成两套并行实现。可评估统一为三阶段路径，移除同步便利方法。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项

- W-001 ~ W-013、T09、N-004/N-005/N-010、Bug #2/#3/#5/#7 等 v3.x→v4.x 系列修复已在代码中通过注释标记固化。本审查不重复记录这些已修复项，仅记录其修复痕迹本身（见 2.7 W-TD-013/014/016）。
- v4.0 的 W-007 修复（提取 `validate_renderer_speed` 共享函数）已在 video.rs:359 与 gif.rs:331 中实施，本审查确认其为有效复用，非技术债。
- v4.0 的 W-003 修复（提前创建 shared_state/state_changed 供壁纸线程回滚）在 image.rs 与 gif.rs 中引入了 `pre_shared_state`/`pre_state_changed`/`display_id_lock` 模式，本审查 W-TD-008/009 标记了该模式的跨渲染器重复问题。

### 5.2 v5.0 已覆盖项

- v5.0 W-PERF-003（屏幕分辨率缓存）已在 mod.rs:40-83 中实施，`get_screen_size` / `invalidate_screen_size_cache` 被 image.rs / gif_decode.rs / web.rs / gdi_base.rs 复用。本审查未将其列为技术债。
- v5.0 D-PERF-007（WorkerW 重试节奏优化）将重试次数从 10 减至 6（manager.rs:1111），本审查确认其已实施。
- v5.0 I-PERF-010（`pause_wallpaper_fast`/`resume_wallpaper_fast` 返回 bool）已在 fast_path.rs:39/63 中实施，本审查确认其为有效优化，非技术债。
- v41-W-006（WP_PROC_CONNECT_RETRIES 从 100 缩减到 40）已在 subprocess_base.rs:51 中实施，但 manager.rs:54 的 doc comment 仍引用旧值"20s"，本审查 W-TD-019 标记了该注释陈旧问题。

### 5.3 v6 新发现

- **死代码**（W-TD-001/002/004/005）：v4/v5 未识别 `WallpaperState::Error` 变体、`PauseReason::USER` 常量、`any_playing`/`paused_displays` 方法仅测试使用的问题，本次通过 Grep 跨 crates/ + src-tauri/ 验证确认。
- **trait 方法未使用**（W-TD-003/011）：v4/v5 未识别 `WallpaperRenderer::navigate` 与 `set_volume` trait 方法从未通过 trait 对象调用的问题（音量通过快速路径控制、导航通过 IPC 直接调用），本次首次识别。
- **GDI 渲染器 W-003 修复模式重复**（W-TD-008/009）：v4.0 W-003 修复在 image.rs 与 gif.rs 中引入了相同的 `pre_shared_state`/`pre_state_changed`/`display_id_lock` 模式，本次首次识别其跨渲染器重复问题。
- **"20s" 注释陈旧**（W-TD-019）：v41-W-006 缩减 IPC 超时后未同步更新 manager.rs:54 的 doc comment，本次首次识别。
- **T09/T14 历史标记**（W-TD-013/014）：v4.x 系列修复引入的 T09/T14 任务编号标记散落 13 处，本次首次识别其清理需求。

## 6. v6.0 清理状态汇总

> 清理日期：2026-07-25 | 衍生 spec：`cleanup-v6-wallpaper-tech-debt-2026-07-25`

### 6.1 P0 项（8 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| W-TD-001 | 死代码 | ✅ 已修复于 v6.0 | 删除 `WallpaperState::Error` 变体，前端类型同步 |
| W-TD-002 | 死代码 | ✅ 已决策保留 | `PauseReason::USER` 保留，doc 注明"仅测试使用" |
| W-TD-004 | 死代码 | ✅ 已修复于 v6.0 | 删除 `any_playing` 方法及测试 |
| W-TD-005 | 死代码 | ✅ 已修复于 v6.0 | 删除 `paused_displays` 方法及测试 |
| W-TD-013 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 11 处 `T09：` 前缀 |
| W-TD-014 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 2 处 `T14：` 前缀 |
| W-TD-015 | 修复痕迹 | ✅ 已决策移除 | Phase 3 引用为历史残留，移除 |
| W-TD-019 | 注释陈旧 | ✅ 已修复于 v6.0 | "20s" 修正为 "8s" |

### 6.2 P1 项（2 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| W-TD-003 | 过时模式 | ✅ 已修复于 v6.0 | 移除 `navigate` trait 方法，WebRenderer 保留为非 trait 方法 |
| W-TD-016 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 26 处 `Bug #`/`Wave 1` 历史标记前缀 |

### 6.3 P2 项（9 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| W-TD-006 | 冗余抽象 | ✅ 已决策保留 | `ensure_desktop_ready` 保留，doc 明确两个入口适用场景 |
| W-TD-007 | 冗余抽象 | ✅ 已决策保留 | `create_and_play_renderer` 保留，doc 注明等价关系 |
| W-TD-008 | 重复实现 | ✅ 已决策保留 | `GdiRendererPreState` 抽取成本高，保留现状并补注释 |
| W-TD-009 | 重复实现 | ✅ 已决策保留 | 依赖 W-TD-008，同上 |
| W-TD-010 | 重复实现 | ✅ 已决策保留 | 命令枚举统一成本高，保留现状并补注释 |
| W-TD-011 | 过度设计 | ✅ 已修复于 v6.0 | 移除 `set_volume` trait 方法，音量通过快速路径控制 |
| W-TD-012 | 过度设计 | ✅ 已决策保留 | `set_wallpaper_results` 测试钩子保留，精简 doc |
| W-TD-017 | 命名一致性 | ✅ 已决策保留 | 依赖 W-TD-010，同上 |
| W-TD-018 | 命名一致性 | ✅ 已决策保留 | WM 常量偏移量已隔离，统一风险高于收益 |

### 6.4 清理统计

- **总技术债**：19 项
- **已修复**：11 项（57.9%）
- **已决策保留**：8 项（42.1%）
- **完成率**：100%

### 6.5 验证结果

- `cargo test -p mirrorstar-core wallpaper::`：全部通过
- `cargo clippy --workspace --all-targets -- -D warnings`：零警告
- `cargo check --workspace`：编译通过
- `npx vitest run` + `npm run lint` + `npm run typecheck`：全部通过
