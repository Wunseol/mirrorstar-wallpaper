# wallpaper 模块优化文档

> [← 返回文档索引](../index.md)

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
| mod.rs | 1434 | 模块入口、WallpaperState/ScalingMode/GifMemoryStrategy/PauseCommand/PauseSender/WallpaperRenderer trait、calculate_scaling、PauseReason 位图、OwnedProcHandle、spawn_proc_exit_monitor、屏幕分辨率缓存 |
| manager.rs | 4183 | WallpaperEngine 状态机、RendererConfig、三阶段 set_wallpaper 流程、construct_renderer、embed_and_register_renderer、close_wallpaper、纯净壁纸模式入口、原子交换、渲染器存活探针与自愈 |
| gif.rs | 2032 | GifRenderer GIF 渲染器（WM_TIMER 驱动帧推进 + 后台解码线程 + 4 种内存管理策略） |
| video.rs | 2080 | VideoRenderer 视频渲染器（mpv 子进程 + MpvIpcClient + WASAPI 音量控制 + pause 线程 COM MTA） |
| fast_path.rs | 1329 | WallpaperEngine 快速通道扩展（pause/resume/volume/mute 快速路径 + PauseReason 位图协调） |
| image.rs | 1393 | ImageRenderer 图片渲染器（GdiRendererBase + 专用线程消息循环 + WM_WALLPAPER_COMMAND） |
| gdi_base.rs | 866 | GdiRendererBase 共享基类（paint_with_double_buffer、spawn_pause_forwarder、register_window_class_once、create_wallpaper_window） |
| gif_decode.rs | 1511 | decode_gif 全帧解码（40MB 内存预算 + 降采样）、decode_gif_first_frame 首帧快速解码 |
| web.rs | 785 | WebRenderer 网页渲染器（wp-proc 子进程 + WpProcIpcClient） |
| mode_dispatch.rs | 436 | determine_wallpaper_mode 集中决策、WallpaperEngine::set_wallpaper 同步路径 |
| gif_memory.rs | 725 | GifRenderData（frames/current_frame/memory_strategy，handle_pause/handle_resume 4 策略） |
| subprocess_base.rs | 479 | SubprocessRendererBase（ProcessManager/state/hwnd/pipe_name/pause_sender，find_window_by_title/find_window_by_class） |
| gdi_cache.rs | 183 | GdiCache 共享 GDI 双缓冲缓存（mem_dc/mem_bitmap/old_bitmap） |

### 测试覆盖

测试分布广泛但存在倾斜：mod.rs（~547 行测试，覆盖 calculate_scaling、枚举 serde、PauseSender 状态/广播/版本号、spawn_proc_exit_monitor 集成测试）、manager.rs（~1724 行测试，含 MockRenderer 状态管理测试与 W-008 回退逻辑测试）、fast_path.rs（~871 行测试，覆盖快速路径与 PauseReason 位图协调）、gif_memory.rs（~322 行测试，4 种内存策略单元测试）、subprocess_base.rs（~44 行测试）、mode_dispatch.rs（~260 行测试，含模式判定与同步路径测试）、video.rs（~783 行测试，含 validate_renderer_speed 与 WASAPI 判断逻辑）、gif.rs（~616 行测试）、gif_decode.rs（~900 行测试）、image.rs（~521 行测试）、web.rs（~286 行测试）、gdi_base.rs（~75 行测试）。涉及 Win32 API 的失败分支普遍通过文档化注释标记验证，无法在 CI 中可靠触发真实失败分支。`paused_displays` 方法已被 v6.0 清理（见 W-TD-005）。

## 纯净壁纸模式（引擎侧入口）

> 实现来源：`manager.rs`（`enter_pure_mode` / `exit_pure_mode` / `abort_pure_mode` / `is_pure_mode_active` / `enter_pure_mode_temp_windows` / `revive_terminated` / `rollback_pure_mode_enter` / `shutdown`）+ `crates/mirrorstar-core/src/pure_mode/mod.rs`。模式在 src-tauri 侧的编排（托盘入口、输入钩子、状态守卫）见 `src-tauri/src/platform/pure_mode.rs` 与 `src-tauri/src/platform/pure_mode_input.rs`。

### 四个入口与幂等语义

| 入口 | 职责 | 幂等语义 |
|---|---|---|
| `enter_pure_mode()` | 把正在播放的壁纸窗口摘出 WorkerW 并置顶全屏（`HWND_TOPMOST`，保持当前矩形零 resize）；**视频显示器例外**（架构 D1）：不搬动原窗口，改由临时顶层覆盖渲染器呈现 | 已处于模式（`pure_mode.is_some()`）→ 直接 `Ok(())`；无任何壁纸（`wallpaper_mode` 为空）→ `Err(InvalidArgument)` 且**不置位**进程级标志 |
| `exit_pure_mode()` | 把 `lifted` 中的窗口按进入时记录的排列嵌回 WorkerW（视频显示器不在 `lifted`，仅销毁其临时覆盖窗、原窗口不搬动）、终止全部临时渲染器、按 `interaction_mode` 还原鼠标穿透。终止前遍历 `pure_mode_temp`，先对每个临时覆盖窗执行 `hide_window_for_pure_mode_exit`（best-effort 同步隐藏），再调用 `renderer.terminate()`，使桌面在 mpv 退出等待期间立即恢复可见 | `pure_mode` 为 `None` → 直接 `Ok(())`（连续两次退出第二次为 no-op） |
| `abort_pure_mode()` | 应用退出路径专用：只终止临时呈现渲染器 + 清置顶窗口注册表 + 清进程级标志，**不回嵌** | 无状态时为无副作用的 safe 调用（可重复调用） |
| `is_pure_mode_active()` | 供 src-tauri 编排层查询权威运行态（`pure_mode.is_some()`） | 只读 |

**标志时序（D7）**：进程级 `pure_mode::set_active(true)` 必须在任何置顶动作**之前**置位（使并发的轮换调度器 / 全屏-电源状态守卫立即开始抑制），`set_active(false)` 必须在全部回嵌**完成之后**清除；失败回滚路径亦在末尾清标志。

### 状态存放约定

| 存放 | 类型 | 含义 |
|---|---|---|
| `WallpaperEngine::pure_mode` | `Option<PureModeState>` | 是否处于模式的**权威状态**。`PureModeState { lifted: Vec<(String, HWND)>, arrangement: Arrangement }`：`lifted` 为本次摘出并置顶的（显示器ID, 窗口句柄），退出时逐个交 `DesktopIntegrator::reattach_after_pure_mode` 按 `arrangement` 嵌回；`arrangement` 为进入时刻的排列快照（模式内用户不可改配置，避免用退出时刻的排列错误重排） |
| `WallpaperEngine::pure_mode_temp` | `Vec<(String, Box<dyn WallpaperRenderer>)>` | **两类**临时呈现渲染器：**Native 静态图片**与 **Video 视频**（架构 D1 的"临时顶层覆盖渲染器"，见下"四类呈现路径"）。**绝不**注册进 `wallpapers` / `pause_senders` / `wallpaper_sources` / `wallpaper_mode` 等权威状态映射，也不写入 `lifted`（它们无需回嵌，只需 `terminate()`：视频覆盖窗销毁后原窗口即原样显露） |

> 注：`PureModeState` 因含 `HWND` 而 `unsafe impl Send`（句柄为不透明整数令牌、不指向 Rust 拥有的内存，且所有访问经 `Arc<tokio::sync::Mutex<WallpaperEngine>>` 串行化）。
>
> 置顶窗口注册：`collect_pure_mode_hwnds(&lifted)` 汇总 `lifted` 与 `pure_mode_temp` **两处**的窗口句柄后登记给 `pure_mode::register_lifted`，供 `TaskbarCreated` 广播后重断言 Z 序——即**临时窗口（含架构 D1 的视频覆盖窗）也在重断言覆盖范围内**。

### 四类呈现路径（D3 / D4 / D8，视频为架构 D1）

| 壁纸类型 | 进入动作 |
|---|---|
| **视频**（`WallpaperType::Video`，**架构 D1**） | **不搬动原窗口**（不 `detach_for_pure_mode`）：新建**临时顶层覆盖渲染器**——`construct_renderer(...)`（内部 `VideoRenderer::new` + `play()`；配置快照复用无副作用的 `renderer_config()`）→ `make_borderless(hwnd)`（失败仅 `warn`）+ `remove_from_taskbar(hwnd)`（与 `embed_wallpaper` Step 1 同源）→ `VideoRenderer::disable_auto_window_resize()`（IPC best-effort，早于 `lift_window_to_rect` / `loadfile`）→ `crate::pure_mode::lift_window_to_rect` 置顶全屏（1920×1080、无边框）→ `after_embed()` 从头 `loadfile`；登记进 `pure_mode_temp`，原窗口留在 WorkerW 全程继续播放。若原窗口已被终止（`hwnd().is_none()`）先 `revive_terminated` 重建后再覆盖。任一步失败即 best-effort `terminate()` 该临时渲染器并放弃本显示器覆盖（`error!`），**不回滚整个进入流程** |
| 网页 / GIF / 非原生格式图片（`hwnd().is_some()`） | 直接 `detach_for_pure_mode` 摘出 + 置顶，**不重载、不重启子进程**，画面继续播放 |
| 同上但 `hwnd().is_none()`（被全屏终止 / 子进程崩溃） | 先 `revive_terminated(display_id)` 重建（`play()` + `reembed_and_register_renderer`，**不触碰** `pause_reasons` 位图），成功后再摘出置顶 |
| Native 静态图片（`WallpaperMode::Native`，根本没有窗口） | 由 `enter_pure_mode_temp_windows` 构造临时 `ImageRenderer::new(path, scaling_mode_for(display_id))` + `play()`（**不**走 `construct_renderer`，避免其 `ensure_desktop_ready` 对 WorkerW 的重试依赖与副作用）后经 `lift_window_to_rect` 置顶 |

临时窗口的覆盖范围（D8）：`arrangement == Span` 时创建 **1 个**跨虚拟屏窗口（矩形取 `pure_mode::virtual_screen_rect()`）；否则逐屏为"**有 Native 记录**且**没有 WorkerW 壁纸窗口**"的显示器各建一个；两者皆无的显示器保持桌面原样（不覆盖）。

> **视频为何不再走摘出置顶（架构 D1）**：实测"把正在播放的 D3D11（bitblt）视频窗口摘出 WorkerW 并置顶"会**毒化本会话的 WorkerW 子层**（此后该层不再产出可呈现帧，退出侧的重建 / 回嵌 / 窗口级唤醒手段均被证伪），而顶层窗口呈现正常。故视频改为"原窗口不搬动 + 临时顶层覆盖"，退出仅 `pure_mode_temp.drain(..)` 销毁覆盖窗、原 mpv 从不重启。根因、已证伪路径与验证结论见 [`03-desktop模块.md`](./03-desktop模块.md) 的"退出回嵌的防呈现冻结契约（架构 D1）"。

> **覆盖窗全屏尺寸契约（spec `fix-pure-mode-exit-presentation-freeze`）**：视频覆盖分支必须在 `lift_window_to_rect` **之前**、且**早于 `after_embed()`（`loadfile`）**调用 `VideoRenderer::disable_auto_window_resize()`（经 mpv IPC `set_property("auto-window-resize","no")`，best-effort）——否则 mpv 默认 `auto-window-resize=yes` 会按**工作区**与视频宽高比自适配覆盖窗（实测被改成 1848×1040 @ (36,20)），覆写 `lift_window_to_rect` 请求的 1920×1080。该属性**仅由视频覆盖窗经 IPC 设置**，boot / 轮换路径与全局 mpv 启动参数均未改动。根因、实测对照与已验证结果（覆盖窗稳定 1920×1080 @ (0,0)、原 mpv 从未重启）见 [`03-desktop模块.md`](./03-desktop模块.md) 的"退出回嵌的防呈现冻结契约（架构 D1）"。

### 部分失败整体回滚

进入过程逐屏执行，**摘出路径**任一步失败（`revive_terminated` 失败、`detach_for_pure_mode` 失败、Native 临时窗口创建 / 置顶失败）即调用 `rollback_pure_mode_enter`：把**已摘出的全部窗口**逐个 `reattach_after_pure_mode` 嵌回、`terminate()` 并清空 `pure_mode_temp`、最后 `set_active(false)`，随后返回原错误——不留"半进入"状态。回滚为 best-effort（不阻断）：单个嵌回失败时对该窗口执行隐藏兜底并记录 `error`（见下"嵌回失败的隐藏降级"），终止失败仅 `warn`。

> **视频覆盖失败不回滚整体（架构 D1）**：视频显示器走"原窗口不搬动 + 临时顶层覆盖"，其覆盖渲染器的创建 / 置顶 / `loadfile` 任一步失败只 best-effort `terminate()` 该临时渲染器并**放弃本显示器覆盖**（`error!`），继续处理其他显示器、**不触发** `rollback_pure_mode_enter`——因为原窗口从未被搬动，失败不产生"半进入"状态。仅其前置的 `revive_terminated` 失败仍按摘出路径回滚。

### 嵌回失败的隐藏降级（F1）

退出与进入回滚两条路径共用同一降级语义：`reattach_after_pure_mode` 失败时不止记录日志，还对窗口做 best-effort 隐藏。

| 路径 | 位置 | 降级动作 |
|---|---|---|
| 退出 | `exit_pure_mode`（逐窗嵌回循环内） | `IsWindow(hwnd)` 为真则 `ShowWindow(hwnd, SW_HIDE)`；隐藏失败再补一条 `warn`，随后记录带 `display_id` 与失败原因的 `error!` 日志 |
| 进入回滚 | `rollback_pure_mode_enter` | 同上，`error!` 文案前缀为"纯净壁纸模式进入失败回滚" |

**理由**：嵌回失败时模式标志（`pure_mode::set_active(false)`）与输入钩子均已撤除，而该窗口仍带 `HWND_TOPMOST`；若继续置顶，用户将没有任何输入退出路径，桌面被永久遮挡。隐藏是"保证桌面恢复可用"的最后一道兜底。

**恢复时机**：被隐藏的窗口在下一次设置壁纸或重启应用后恢复。

### 空模式拒绝（F5）

`enter_pure_mode` 在摘出窗口与临时呈现窗口均为空（`lifted.is_empty() && self.pure_mode_temp.is_empty()`）时判定为"空模式"并拒绝进入：

- 记录 `warn`，调用 `rollback_pure_mode_enter` 回滚（best-effort 隐藏兜底 + 终止临时渲染器 + `set_active(false)`）
- 返回 `Err(MirrorStarError::InvalidArgument { reason: "没有可覆盖的壁纸窗口，拒绝进入纯净壁纸模式" })`

**理由**：否则会置位标志、安装输入钩子并抑制轮换调度，却没有任何窗口被覆盖——用户进入了一个"看不见的模式"，只能再次触发退出。该不一致通常源于 `wallpaper_mode` 有记录，但既无活跃窗口也无 Native 资源。

### D1 渲染器层同步的豁免登记（A1）

设计契约（spec `add-pure-wallpaper-mode` 的 D1）第 3 步曾要求进入置顶后调用 `renderer.set_position(x, y, w, h)` 在渲染器层同步几何（网页经 IPC 触发 WebView2 重排重绘）。第三轮审计确认**该步骤未落地**：置顶路径无此调用，全仓该调用只出现在 `update_positions` 的几何更新与各渲染器实现内部。经评估，登记为**经实测豁免的设计步骤**，不新增调用：

- **零 resize ⇒ 无信息增量**：进入过程保持当前屏幕矩形（零 resize，见本节"四类呈现路径（D3 / D4 / D8）"与 D2），渲染器侧几何与进入前完全一致，再同步一次不引入任何变化。
- **GDI 路径已有保险重绘**：置顶路径（`lift_wallpaper_window` / `lift_window_to_rect`）末尾的 `InvalidateRect` 已覆盖静态图片 / GIF 等 GDI 呈现路径的重绘。
- **实测证据**：落地 E2E 与第三轮 E2E 均实测"置顶后视频 / 网页持续出画"——对两帧截图取哈希，哈希不同，证明画面在持续更新。
- **补调用反而有害**：额外的 `set_position` 会引入无谓的 `SetWindowPos`（video 路径会以 `HWND_TOP` 重排 Z 序）与一次 IPC 往返，且与"零 resize"目标相悖。

### 锁屏（Win+L）实际行为（D8 修订，A6）

设计契约 D8 曾写"锁屏（Win+L）→ 保持模式"。该表述与实现冲突：模式内键盘钩子在 `WM_KEYDOWN`（LWIN 按下）即触发退出并进入 300ms 吞掉窗口，随后的 L 键 `WM_KEYDOWN` 与 keyup 一并被吞——LWIN-down 从未进入系统输入队列，因此 **Win+L 锁定热键大概率不生效**。

**实际行为**：模式内任意键（含 Win 组合键）都会退出模式，即 **Win+L = 退出模式，而非锁屏**。

**修订表述**：如需锁屏，请在模式外执行（先退出模式再 Win+L），或使用不受低级钩子影响的 SAS 路径（**Ctrl+Alt+Del**）——该路径锁屏期间模式保持，解锁后任意键退出。

### 输入钩子生命周期契约（A2/A3）

进入 / 退出编排位于 src-tauri 侧（`src-tauri/src/platform/pure_mode.rs` 与 `src-tauri/src/platform/pure_mode_input.rs`），遵循以下四条契约：

- **进入前有界等待在途退出落位（A2）**：进入编排在安装输入钩子前会轮询 `EXIT_IN_PROGRESS`，有界等待在途退出编排（含 join 旧捕获线程的卸载步骤）落位，上界约 10s（`EXIT_WAIT_TRIES = 200`，50ms × 200）；超时则**拒绝进入并保持托盘项未勾选**，避免新旧会话钩子并发 / 误投 `HookFailed`。
- **存储句柄前复核模式活跃（A2）**：安装新钩子后、存入句柄前复核模式仍活跃（`is_pure_mode_active`）；若不活跃（并发退出已在期间完成）则**卸载刚安装的新钩子且不存储**，避免残留句柄。
- **显式卸载不投递 `HookFailed`（A3）**：由**显式卸载**（`InputHook::shutdown`，即退出编排 / 应用退出路径）引发的线程收尾**不投递** `ExitReason::HookFailed`，即使模式标志仍活跃也不投递——否则会在应用退出或已放弃的回调上产生误报。
- **收尾释放退出通道（A3）**：线程收尾释放退出通道（仅同一通道才清空），使退出监听任务及时结束、不再挂在已失效的钩子会话上。

### `shutdown()` 首部 `abort_pure_mode()` 的原因

`shutdown()` 先调用 `abort_pure_mode()` 再走既有 `cleanup_internal()`：退出路径**不回嵌**——已摘出的窗口随后由 `cleanup_internal` 经 `close_wallpaper` 统一终止，其内部 `remove_wallpaper` 的 `SetParent(None) + SW_HIDE` 对已摘出窗口幂等安全，此时再嵌回桌面纯属浪费且会触发一次多余的 Explorer 交互。`abort_pure_mode()` 因此只做三件事：终止全部临时呈现渲染器、`clear_lifted()`、`set_active(false)`。

## 渲染器存活探针与停滞自愈契约

> 实现来源：`crates/mirrorstar-core/src/wallpaper/mod.rs`（`LivenessProbe` / `WallpaperRenderer::liveness_probe`）+ `video.rs`（`liveness_probe_from_status` / `video_playback_stalled`）+ `manager.rs`（`probe_playing_videos` / `heal_stalled_renderer`）+ `src-tauri/src/platform/pure_mode.rs`（退出纯净壁纸模式时的即时校验）+ `src-tauri/src/platform/liveness_watchdog.rs`（常驻看护）。

**问题背景**：mpv 视频输出偶发在 `--loop-file` 循环点停滞——"最后一帧 drain 未完成"，`time-pos` 钉在最后一帧不再前进。修复前应用**没有任何卡帧检测与自愈**，该冻结会一直保留；纯净壁纸模式把它"暴露"给用户（用户此刻正盯着画面），且原退出编排只回嵌窗口、从不校验渲染器是否真的在播放。

> **该停滞属上游 mpv 输出的偶发问题**：本次实测一次发生在进入纯净壁纸模式约 **10.08s** 后仍未 drain 完成；用 **5 组隔离条件复现均未复现**。本修复是"检测 + 自愈"（重建渲染器），**不消除上游停滞本身**。

### 探针原语（core 侧）

| 原语 | 位置 | 行为契约 |
|---|---|---|
| `LivenessProbe` | `wallpaper/mod.rs` | 一次查询即冻结的不可变快照：`time_pos` / `paused` / `idle_active` / `eof_reached`。经 crate 根导出为 `mirrorstar_core::LivenessProbe` |
| `WallpaperRenderer::liveness_probe()` | `wallpaper/mod.rs`（trait 默认方法） | 默认返回 `None`（图片 / GIF / 网页渲染器不参与判定，**零副作用**）；仅 `VideoRenderer` 覆盖 |
| `VideoRenderer::liveness_probe()` | `video.rs` | 经私有 `liveness_probe_status()` 查询 mpv：**只查判定所需的 4 个属性**（`time-pos` / `pause` / `idle-active` / `eof-reached`），逐属性容错；IPC 未连接时返回空 JSON 对象（`{}`）⇒ `liveness_probe_from_status` 返回 `None` ⇒ 本轮不判定停滞。IPC 失败仅记 `debug` 并返回 `None` |
| `MpvIpcClient::get_property_with_timeout(name, timeout)` | `ipc/mpv_protocol.rs` | 带自定义超时的属性查询；既有 `get_property` 改为委托它并保留固定 **2s** 语义（行为逐字等价）。探针经它使用 `PROBE_PROPERTY_TIMEOUT = 500ms` 短超时 |
| `liveness_probe_from_status(&Value)` | `video.rs` | 解析 mpv 属性 JSON。**任一必需字段缺失或为 `{"error": ...}` 对象即返回 `None`**——不把 IPC / 解析故障误判为停滞；布尔解析兼容 `true`/`false` 与 `"yes"`/`"no"` |
| `video_playback_stalled(prev, curr)` | `video.rs`（纯函数，经 crate 根导出为 `mirrorstar_core::video_playback_stalled`） | 停滞判据，见下 |
| `probe_playing_videos()` | `manager.rs` | 仅收集 `state() == Playing` 且探针返回 `Some` 的显示器（Terminated / Paused 不入集）；`display_id` 升序 |
| `heal_stalled_renderer(display_id, prev_time_pos, curr_time_pos, source)` | `manager.rs` | 自愈原语，见下 |

**探针查询成本上界**：探针**不再复用**"诊断用"的 `diagnostic_playback_status()`（7 属性 × 2s 超时，最坏 ≈14s），改为只查 4 个属性并使用 500ms 短超时 ⇒ 每次取锁**最坏 ≈ 4 × 500ms ≈ 2s**。`diagnostic_playback_status()` 本身**保持原样**（仍 7 属性 / 2s 超时），其调用方（原子交换就绪判断等）不受影响。

### 停滞判据

`video_playback_stalled(prev, curr)` 的判据（三者同时成立即判定停滞）：

1. `!curr.paused`（当前非暂停）；
2. `!curr.idle_active`（已加载文件、非 idle）；
3. `(curr.time_pos - prev.time_pos).abs() < STALL_EPSILON_SECS`，其中 `STALL_EPSILON_SECS = 0.001`（1ms）。

**为什么用"变化绝对值 < 1ms"而非 `curr <= prev`**：循环播放跨越循环点时 `time-pos` 会**大幅回退**（如 11.2 → 0.05），用 `<=` 会把这种正常循环误判为停滞；取"变化量绝对值"后回退量远大于阈值，因而不命中。正常播放即使 0.25x 倍速，两次采样（间隔 ≥500ms）间前进量也远大于 1ms，故阈值取 1ms 量级不会漏检。

**`eof_reached` 不参与判定**：停滞时 mpv 仍处于 DRAINING（尚未进入 EOF），若以 `eof_reached` 为判据会漏检。

### 自愈语义

`heal_stalled_renderer` 固定顺序：**先 `terminate()`（失败仅 `warn` 并继续）→ 再 `revive_terminated(display_id)`**（`play()` + `reembed_and_register_renderer()` + PauseSender 状态同步）。

- **禁止**在渲染器仍存活时直接 `play()`：`VideoRenderer::play()` 无条件 spawn 新 mpv 进程 / 窗口，会产生第二个 mpv。故必须先 `terminate()` 再重建。
- **不触碰 `pause_reasons` 位图**：自愈只重建渲染器，暂停语义由调用方保证。
- 日志携带 `display_id` / `prev_time_pos` / `curr_time_pos` / `source`，便于区分触发来源（`pure-mode-exit` / `watchdog`）。

### 两级触发（分级检测）

| 触发 | 位置 | 语义 |
|---|---|---|
| **① 退出纯净壁纸模式即时校验** | `src-tauri/src/platform/pure_mode.rs::liveness_check_and_heal` | 模式退出瞬间的强一致兜底（用户此刻正在看壁纸）。采样 #1 →（无 Playing 视频渲染器则**不 sleep 直接返回**，零额外等待）→ sleep 500ms → 采样 #2 → 命中疑似停滞才 sleep 400ms → 采样 #3 确认；两段区间都钉住（`video_playback_stalled` 均命中）才自愈，避免极低帧率内容误判。逐屏独立，失败仅 `warn`，不阻断后续编排 |
| **② 常驻看护** | `src-tauri/src/platform/liveness_watchdog.rs` | 覆盖模式外的长时运行（用户没盯着看时冻结也会被静默自愈）。每 `LIVENESS_WATCH_INTERVAL = 5s` 采样一次（`MissedTickBehavior::Skip`），与上一轮"完整完成采样"的快照比较：首段命中**疑似停滞**后，**在锁外**等待 `CONFIRM_INTERVAL = 1.3s` 确认采样，由纯函数 `confirmed_stalls` 判定——**两段区间都"钉住"才自愈**（`source = "watchdog"`，并写入自愈冷却时间戳）；否则记为周期混叠、仅记 `debug`、不自愈（也不消耗自愈冷却）。首轮无基线不判定 |

**为什么追加确认采样（周期混叠根因）**：单周期判据 `|Δtime_pos| < 1ms` 在"视频循环时长**整除**采样周期"（如 5s / 2.5s / 1.25s…）且采样抖动 < 1ms 时会出现**假阳性**——每次采样恰好落在循环的同一相位，`time-pos` 看起来"钉住"，实际在前进。确认采样间隔取 `1.3s`（与 5s **非整除**），可打乱相位：视频若确实在播，确认区间的 `time-pos` 必然恢复前进，据此**证伪混叠**；只有两段区间都钉住才判定真实停滞并自愈。

**为什么需要常驻看护**：实测根因表明 mpv 输出可能在**任意时刻**停滞（本次是循环点 drain 未完成），退出编排只能覆盖"退出模式"这一个时间点。两者互补——模式内画面由置顶窗口呈现、且退出编排会即时校验，故常驻看护在模式内跳过。

### 看护跳过条件（任一命中即本轮不动作，且**不更新采样基线**）

| # | 跳过条件 | 原因 |
|---|---|---|
| 0 | 应用退出清理已开始（`crate::state::SHUTDOWN_DONE` 为真） | 退出中不再采样 / 自愈，避免与 `perform_shutdown_blocking` 的 3s 引擎锁超时竞争、避免在 `engine.shutdown()` 之外 spawn 孤儿 mpv |
| 1 | 纯净壁纸模式活跃（`pure_mode::is_active()`） | 模式内画面由置顶窗口呈现，且退出编排会即时校验；模式内采样 / 自愈会与置顶编排竞争 |
| 2 | 轮换原子交换进行中（`atomic_swap_in_progress()`） | 渲染器正在换槽，探针会读到过渡态（旧渲染器 Terminated / 新渲染器未就绪），据此判定会误自愈 |
| 3 | `pause_reasons_snapshot()` 非空 | 暂停态（用户 / 全屏 / 电池）下时钟本就不前进，属预期，绝不能判为停滞 |
| 4 | engine `try_lock` 失败（锁忙） | 如正在 `set_wallpaper`；用非阻塞 `try_lock`，下一轮再试，不阻塞 |

**两层判定（锁外前置 + 锁内复核）**：跳过条件 0 ~ 2 在**锁外**先判一次（避免常见跳过路径白白产生一次 `spawn_blocking`）；随后 `watch_once` **两次取锁后都复核** `pure_mode::is_active()` 与 `atomic_swap_in_progress()`（TOCTOU 收口：堵住"锁外判非模式，取得引擎锁前模式刚进入 / 刚换槽"的窗口期），命中即跳过、不更新基线。条件 3 / 4 本身就必须在锁内 / 取锁时判定。

**"跳过不更新基线"是正确性关键**：跳过轮次若把"当前（可能已冻结的）快照"写入基线，则"暂停 5s 后恢复"会在恢复后的首轮因基线过期而误判——把暂停期间的静止当成停滞。保持基线不变 ⇒ 判定始终比较"两个都真实在播放的时刻"，暂停 / 锁忙 / 模式切换都不会污染判定。

### 自愈冷却

常驻看护对**每个显示器**设有 `HEAL_COOLDOWN = 60s` 冷却（静态量 `LAST_HEAL_TS: LazyLock<Mutex<HashMap<String, Instant>>>`）。冷却期内再次判定停滞只记 `warn`、不重复自愈，避免"重启风暴"。退出编排（触发 ①）为一次性即时校验，不走该冷却。

### 锁与阻塞

- `probe_playing_videos`（含 mpv IPC）与 `heal_stalled_renderer`（含进程启停）均为阻塞调用：看护与退出编排的"采样 → 判定 → 自愈"整体在 `tokio::task::spawn_blocking` 内执行，不阻塞 tokio 工作线程与主线程。**采样与自愈在锁内**（`watch_once` 分段取锁，判定 / 自愈完成后立即释放），**确认采样的 `CONFIRM_INTERVAL = 1.3s` 等待在锁外**（先释放锁再 sleep，避免持锁等待）；探针每次取锁的上界 ≈ 4 属性 × `PROBE_PROPERTY_TIMEOUT = 500ms` ≈ **2s**（原诊断查询 7×2s ≈ 14s 已消除）。
- 退出编排把该步骤放在补跑全屏 / 电源判定**之后**（避免"刚自愈又被终止"）、唤醒轮换调度器**之前**（保证退出完成时画面是活的）；自愈失败仅 `warn`，不阻断调度器唤醒。进入侧等待在途退出的窗口 ≈ **10s**（覆盖自愈时长）。
- 常驻看护任务随 runtime 结束，**不做显式停止 / join**（`try_lock` 非阻塞、不持 desktop 锁等 shutdown 关键资源，与 `workerw_check` 不同）；`SHUTDOWN_DONE` 置位后下一轮即命中**跳过条件 0**，不会在退出清理开始后继续自愈。

### 已知限制（本轮审查确认）

> 来源：`.trae/specs/audit-pure-mode-liveness-hardening/`（"全方位审查与加固"）。以下为本轮确认、**不改代码**的边界事项，逐条说明现状与"为什么不修"。本机制是"检测 + 自愈"，**不消除上游 mpv 停滞本身**。

- **F6 IPC 完全无响应时不判定**：mpv 核心线程卡死但进程仍存活时，探针询问属性无响应 ⇒ 返回 `None` ⇒ 不判定、不自愈。**为什么不修**：这里选择"不误 heal"优先（无法区分"确认在播"与"问不到"）；进程真正退出由既有的 `spawn_proc_exit_monitor` 路径处理。
- **F7 倍速不持久**：`VideoRenderer` 不持有 speed（`set_speed` 仅发 IPC），自愈 / 换槽 / 全屏恢复后倍速回 1.0x，而 UI / 配置仍显示旧值。**为什么不修**：pre-existing（rotation 与全屏恢复同病），且当前配置 speed=1.0 无实际影响；建议单独 spec 处理。
- **F8 `terminate()` 失败仍继续 revive**：极端情况下 `heal_stalled_renderer` 在 `terminate()` 失败后仍 `revive_terminated()`，可能出现第二个 mpv 进程。**为什么不修**：孤儿进程由下次启动的 `cleanup_stale_child_processes` 兜底（失败路径收敛改动面大于收益）。
- **F9 自愈窗口期 `active_wallpapers` 短暂陈旧**：`terminate()` 不清理 desktop 条目，恰逢 WorkerW 兜底检查 / Explorer 重启会走"无效 hwnd → 删条目"，随后重嵌再登记。**为什么不修**：最终一致、无残留（自愈窗口为秒级）。
- **F10 `InputHook` 无 `Drop`**：极端 panic 路径可能残留低级钩子且槽位已空。**为什么不修**：既有多处显式 `shutdown`；加 `Drop` 涉及阻塞与重复卸载语义，需单独评估。
- **F11 模式内"位图暂停 + Terminated"渲染器会被复活为 Playing**：`enter_pure_mode` 经 `revive_terminated` 会把该渲染器恢复为 Playing。**为什么不修**：这是"沉浸优先"的设计取舍（进入模式需呈现画面），退出后由补跑判定纠正，**非缺陷**。
- **F12 全屏 terminate 在引擎锁忙时被跳过且不置 `FULLSCREEN_WAS`**：仅靠事件重复兜底。**为什么不修**：既有弱点；本轮缩短看护持锁时长（≈14s → ≈2s）后风险已下降。

### 覆盖边界：呈现层冻结不可见（spec `fix-pure-mode-exit-presentation-freeze`，2026-09-30）

> 上文探针 / 自愈的全部判据都只读 mpv 属性（`time-pos` / `pause` / `idle-active` / `eof-reached`），即**时钟层**。2026-09-30 实测确认存在一种本机制**天然不可见**的失效模式——**"呈现层冻结"**：`time-pos` **继续前进**（mpv 从未收到 `pause`、存活校验与 5s 看护均判"时钟前进"），但**窗口画面定格**（停在最后一帧不再刷新）。此时 `video_playback_stalled` 因 `time-pos` 在变而**永不命中**，探针与自愈都不会触发。

**本机制的盲区**：探针**不做**截图级 / 画面哈希级检测（成本高、需每轮截屏比对），故对"时钟前进 + 画面定格"**运行时不可见**——这是本轮确认的**已知限制**，非本轮改动项。该失效模式的根因与修复（架构 D1：视频进入纯净壁纸模式不再搬动原窗口，改由临时顶层覆盖渲染器呈现）见 [`03-desktop模块.md`](./03-desktop模块.md) 的"退出回嵌的防呈现冻结契约（架构 D1）"。

> 两种失效模式对照：**① 时钟层停滞**（循环点 drain 未完成、`time-pos` 钉住）= 本机制覆盖，检测 + 自愈；**② 呈现层冻结**（微时钟前进、画面定格）= 本机制盲区，改由架构 D1 规避。二者互不替代。

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

## v3.x 已修复问题

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| W01 | Medium | `update_positions` Span 模式负坐标 clamp 到 0 导致跨屏定位错误 | ✅ 已修复（v3.5.2） |
| W02 | Medium | `ImageRenderer` trait `pause()`/`resume()` 仅设状态不发命令，与 GifRenderer 不一致 | ✅ 已修复（v3.5.2） |
| W03 | Low | `decode_gif` max_frames 使用屏幕分辨率而非实际帧尺寸，小帧 GIF 帧数被过度限制 | ✅ 已修复（v3.5.3） |
| W04 | Medium | `set_wallpaper` 同步路径持锁期间调用 `play()` 阻塞 engine（最长 20s） | ✅ 已修复（v3.5.2）— play 移出锁外 |
| W05 | Low | `ensure_desktop_ready_with_retry` 方法与 `ensure_desktop_ready` 自由函数 ~40 行重复 | ✅ 已修复（v3.5.3）— 提取 `ensure_desktop_ready_impl` |
| W06 | Medium | `pause_all_fast` TOCTOU 窗口导致并发 resume 丢失 | ✅ 已修复（v3.5.2；后续 v4.0 W-004 已修复部分失败回滚） |
| W07 | Medium | `WebRenderer::play` 不监听 wp-proc 退出事件，状态不一致 | ✅ 已修复（v3.5.2；后续 v4.0 W-001/W-002 已补齐句柄 RAII 与 video.rs 退出监听） |
| W08 | Low | `register_window_class_once` 丢弃 `RegisterClassW` 返回值，真实失败被掩盖 | ✅ 已修复（v3.5.3） |
| W09 | Low | GIF 后台解码线程无取消机制，窗口销毁后仍继续解码 | ✅ 已修复（v3.5.3）— `AtomicBool` 取消标志 |
| W10 | Low | `GifRenderer::set_speed` 不校验 speed > 0，speed=0 导致极快播放 | ✅ 已修复（v3.5.3；后续 v4.0 W-007 已补齐 video.rs 对称校验） |
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
| W-TD-002 | 死代码 | mod.rs:575 | `PauseReason::USER` 仅测试用，生产仅用 FULLSCREEN/BATTERY/TRAY | 评估保留（doc 注明）或移除改 TRAY（低） | ✅ 已修复于 v6.0 | 移除 `PauseReason::USER` 变体（生产仅 FULLSCREEN/BATTERY/TRAY；测试改用 TRAY） |
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

## v17.0 内存优化（2026-10-02）

本轮针对「设置动态壁纸后应用组内存偏高」，新增内存归因埋点并实施 6 项手段；本模块涉及 **Task 4：mpv 进一步限流**。

### Task 4：mpv 解码限流（`wallpaper/video.rs`）

`build_mpv_args` 在 v10-A / v11-A 限流基础上追加两项，用于压低视频壁纸（尤其 hwdec 失败回退软解）的内存：

| 新增参数 | 作用 |
|---|---|
| `--vd-lavc-threads=4` | 限制视频解码线程数（mpv 默认 0 = 按 CPU 核数自动上探，最多 16）。帧级多线程解码为每条线程保留参考帧缓冲（1080p 单帧 ≈3MB、4K ≈12MB）。固定 4 对 1080p/4K 软解仍流畅，同时显著压低线程缓冲上限；`--hwdec=auto` 硬件解码时不参与软解线程创建，无副作用 |
| `--demuxer-readahead-secs=0.5` | demuxer 前向预读时间窗由默认 1s 减半。`--cache=no` 下无 stream 缓存层，但 demuxer 仍按本参数预读数据包进入其包队列；降低该值直接压缩该队列内存，本地文件循环播放 0.5s 预读绰绰有余 |

**既有限流参数全部保留**：`--cache=no`、`--vd-queue-max-bytes=16777216`、`--ad-queue-max-bytes=4194304`、`--demuxer-max-back-bytes=0`、`--cache-secs=0`、`--hwdec=auto`、`--vo=gpu` 等。

> **硬约束（勿破坏）**：**禁止新增 `--demuxer-max-bytes=`**。该参数会把解复用器缓冲上限清零，导致 demuxer 无法缓冲任何数据包（`Too many packets in the demuxer packet queues: 0 packets`），mpv 立即 video EOF + `No media data to loop` 退出（约 0.2s），已嵌入壁纸层的空窗口被销毁 → 桌面黑屏 + 设置失败。

**测试**：`build_mpv_args_contains_task4_memory_limiting_args` 断言新增参数存在；`build_mpv_args_contains_common_args` / `build_mpv_args_does_not_contain_file_path` 及五种缩放模式用例保证既有必需参数与禁止项约束仍成立。

**预期效果**：降低视频壁纸软解场景的线程参考帧缓冲与 demuxer 前向预读包队列占用。**优化前后实测数值待真机补测**（用 `get_memory_report` / `MEM-ATTRIB` 采集 `mpv.exe` 归类，本环境无 GUI 不伪造数据）。

## 深度审查：死代码 / 冗余 / 过度抽象（2026-10-10）

> **审查范围**：`crates/mirrorstar-core/src/wallpaper/`（13 文件：mod / manager / mode_dispatch / fast_path / gdi_base / gdi_cache / image / gif / gif_decode / gif_memory / subprocess_base / video / web）。
> **方法**：全仓引用搜索（排除 `target/`），逐项区分「无任何引用 / 仅测试引用 / 内部引用 / 外部引用」。
> **基线**：`cargo check --workspace` ✅ 通过；`cargo clippy --workspace --all-targets` 0 warning；`cargo test -p mirrorstar-core -- --skip mpv_backend` = 716 passed / 0 failed / 33 ignored。

### 8.1 死代码

**结论**：本模块**无零引用符号**——所有枚举变体（`WallpaperState`/`ScalingMode`/`GifMemoryStrategy`/`PauseCommand`）、`WallpaperRenderer` 实现、`PauseReason` 运算符重载、解码/绘制辅助函数均有实际调用方。以下为「仅测试引用 / 无生产调用点」层面的观察项：

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| D-1 | mod.rs:693（trait `WallpaperRenderer::set_position`） | 必实现方法全仓无任何 trait 对象调用点，4 个实现体不可达 | `.set_position(` 仅命中 `WpProcClient` 内部调用与 impl/mock 定义 | Medium | ⏳ 待确认（公开 trait 契约） |
| D-2 | mod.rs:691（trait `resume`） | 无生产调用点（生产 pause/resume 走 fast_path 的 `PauseCommand`） | 仅 gif.rs 单测直接调用 | Medium | ⏳ 待确认 |
| D-3 | mod.rs:705（trait `process_pid`） | 无 trait 对象调用点；实际 PID 读取均走 `SubprocessRendererBase::process_pid` 固有方法 | web.rs:274/303、video.rs:724/935/1069 | Medium | ⏳ 待确认 |
| D-4 | mod.rs:472（`PauseSender::state_version`） | 方法仅测试引用；字段由 `set_state` 写入但无生产读取者 | 仅 mod.rs 测试；登记契约 v41-W-005（前端集成为 v6.0 优化机会） | Low | ⏳ 待确认（公开 API） |
| D-5 | manager.rs:31（`RendererConfig.arrangement`） | 字段生产零读取（嵌入用引擎字段 `self.arrangement`） | 读取仅 tests 2830/2844 | Low | ⏳ 待确认（pub 字段 + 测试读取） |
| D-6 | manager.rs:1316-1321（`prepare_atomic_swap` 的 `_source`/`_wallpaper_type`） | 下划线前缀死参数，函数体未使用 | scheduler.rs:1159 调用点 | Low | ⏳ 待确认（公开签名） |
| D-7 | gdi_base.rs:120（`GdiRendererBase::scaling_mode()`） | 生产零调用，仅本文件单测 | image/gif 直接读 `base.scaling_mode` 字段 | Low | ⏳ 待确认（pub API） |
| D-8 | subprocess_base.rs:280-282（`SubprocessRendererBase::scaling_mode()`） | 生产零调用，仅单测 | video.rs:446 用字段 | Low | ⏳ 待确认（pub API） |
| D-9 | mode_dispatch.rs:140-144 | Native 分支 `else` 不可达（`mode==Native ⟹ source 必为 File`） | `determine_wallpaper_mode`（:63-71）仅 Image + File + 原生支持才返 Native | Low | ✅ 已清理（移除不可达分支） |

> **纠正前置结论**：`PauseReason::is_empty`（mod.rs:598）经复核**有生产调用**（fast_path.rs:270/337、manager.rs:1458），非死代码。
> **有意保留**：`set_screen_size_for_test`（mod.rs:94）、`SCREEN_SIZE_TEST_MUTEX`（mod.rs:106）为测试辅助，非死代码。

### 8.2 冗余代码

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| R-1 | manager.rs:785 & 800 | 同一表达式 `old_mode_was_native && mode == WallpaperMode::WorkerW` 在函数内计算两次 | 逐字相同 | Low | ✅ 已清理（提出一次复用） |
| R-2 | subprocess_base.rs:96/116-117/157 | 注释称「256MB」，实际常量 `SUBPROCESS_MEMORY_LIMIT_BYTES = 2048MB`（:139） | 注释与代码不符 | Low | ✅ 已清理（注释同步 2048MB） |
| R-3 | 04-wallpaper模块.md:30-44 / :48 | 「文件清单」与「测试覆盖」行数严重陈旧（普遍为实测 50%~60%） | 见 8.4 | Medium | ✅ 已清理（按实测更新） |
| R-4 | manager.rs:1008 vs 1075 | `embed_and_register_renderer` 与 `reembed_and_register_renderer` 的嵌入→失败回滚→after_embed 逻辑近乎逐行重复 | 1015-1052 ≡ 1087-1122 | Medium | ⏳ 待确认 |
| R-5 | manager.rs:509 & 512 | `source_clone = source.clone()` 后紧跟 `original_source = source_clone.clone()` 多余克隆 | W-008 代码段 | Low | ⏳ 待确认（W-008 区） |
| R-6 | manager.rs:646-651 | `set_speed` 声明 `async` 但函数体无 `.await`（伪异步） | commands/wallpaper.rs:1305 | Low | ⏳ 待确认（签名影响调用方） |
| R-7 | manager.rs:2215/2236/2262 | `construct_renderer` 三分支重复「`if clear_native { clear_native_wallpaper() }` + warn」样板 | — | Low | ⏳ 待确认 |
| R-8 | fast_path.rs:39-50 / 63-74 | `pause_wallpaper_fast`/`resume_wallpaper_fast` 逐行同构（仅命令与文案不同）；`*_fast` 单屏函数重复同一 warn 文案 | — | Low | ⏳ 待确认（v41-W-004 区） |
| R-9 | fast_path.rs:266-269/329-332/385-388/417-420 | `pause_reasons.lock().map_err(...)` 防中毒样板 ×4 重复 | — | Low | ⏳ 待确认（W-004 区） |
| R-10 | fast_path.rs:166-229 | `broadcast_set_volume_fast`/`broadcast_toggle_mute_fast` 各自手写同一遍历聚合骨架 | — | Low | ⏳ 待确认（收益边际） |
| R-11 | mode_dispatch.rs:117-174 | 同步 `set_wallpaper` 与 `src-tauri/commands/wallpaper.rs` 三阶段异步路径并行重复 | 文件头自述「两条路径独立维护，互不调用」 | Medium | ⏳ 待确认（N-004 / v41-W-006） |
| R-12 | fast_path.rs:265-293 ↔ manager.rs:1456-1466 | `pause_all_fast` 与 `apply_pause_reasons_to` 的 sender 遍历/补发逻辑概念重叠 | — | Low | ⏳ 待确认（DR-20 契约） |
| R-13 | gdi_base.rs:335 / 511 | `paint_with_double_buffer` 与 `paint_image_with_double_buffer` 骨架逐行重复（仅上屏方式不同） | 344-376≡520-550、407-438≡572-602 | Medium | ⏳ 待确认 |
| R-14 | image.rs:47-58 ↔ gif.rs:72-84 | `WallpaperCommand` 与 `GifCommand` 仅差一个 `SetSpeed` 变体 | — | Medium | ⏳ 待确认（W-TD-010/017） |
| R-15 | image.rs:119-125 ↔ gif.rs:183-189 | `pre_shared_state`/`pre_state_changed`/`display_id_lock` 同名字段逐字段重复 | — | Medium | ⏳ 待确认（W-TD-008/009，W-003） |
| R-16 | image.rs:323-375 ↔ gif.rs:433-485 | `create_pause_sender` 近乎逐行重复 | — | Medium | ⏳ 待确认 |
| R-17 | image.rs:240-250 ↔ gif.rs:344-354 | `Drop` 实现完全相同 | — | Low | ⏳ 待确认 |
| R-18 | image.rs:252-385 ↔ gif.rs:356-402 | `WallpaperRenderer` trait 实现重复 | — | Medium | ⏳ 待确认 |
| R-19 | gif.rs:1311-1318 / gif_decode.rs:80-202 / gif_memory.rs:123-152 | 「窗口外帧像素清理」三处实现同一语义 | — | Medium | ⏳ 待确认 |
| R-20 | gif_memory.rs:226-257↔278-304；314-343↔358-386 | `handle_pause`/`handle_resume` 分支间 drain 保留区间与重解码块逐行重复 | — | Medium | ⏳ 待确认 |
| R-21 | video.rs:892-928 ↔ web.rs:253-296 | 两渲染器 `terminate()` 近乎逐行重复（可下沉基类） | — | Medium | ⏳ 待确认（W-001/W-002 关联） |
| R-22 | video.rs:1221-1247 ↔ web.rs:446-473 | 子进程退出监听闭包体重复 | — | Medium | ⏳ 待确认（W-001/W-002 关联） |
| R-23 | video.rs 39 处 / web.rs 18 处 | `unwrap_or_else(\|e\| e.into_inner())` 中毒锁恢复模式散落 | — | Low | ⏳ 待确认（机械替换） |
| R-24 | video.rs:47-57↔610-619；488-520↔531-555 | mpv 布尔解析 / 属性查询循环骨架重叠（缺省语义不同） | — | Low | ⏳ 待确认 |
| R-25 | subprocess_base.rs:346-433 | `find_window_by_title`/`find_window_by_class` 共享同一轮询骨架 | — | Low | ⏳ 待确认 |
| R-26 | subprocess_base.rs:89 ↔ src-tauri/platform/fullscreen.rs:555 | `"MirrorStarWallpaper"` 窗口属性字面量跨 crate 重复定义 | — | Medium | ⏳ 待确认（跨 crate 契约） |
| R-27 | video.rs:319 / 670 ↔ ipc/client.rs:53 | 命名管道完整路径 `\\.\pipe\{}` 拼接重复 | — | Low | ⏳ 待确认 |
| R-28 | video.rs:318 ↔ mirrorstar-ui/src/preview/video.rs:533 | `build_mpv_args` 跨 crate 部分重复（用途已分叉） | — | Low | ⏳ 待确认 |

### 8.3 过度抽象

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| O-1 | manager.rs:797（`SetWallpaperPending.clear_native`） | **W-012** 恒为 false（786 先关闭→797 `is_native_mode` 必 false），恒值字段增加认知负担 | 消费点 mode_dispatch.rs:161 | Low | ⏳ 待确认（W-012） |
| O-2 | manager.rs:2300-2307（`create_and_play_renderer`） | `construct_renderer(.., true)` 的单行包装 | 唯一外部调用 commands/wallpaper.rs:1076 | Low | ⏳ 待确认（公开 API） |
| O-3 | manager.rs:146-150（`SwapOutcome`） | 单变体枚举（仅 `Committed`），信息量等价 `()` | src-tauri/scheduler 使用 | Low | ⏳ 待确认（跨模块契约） |
| O-4 | manager.rs:4144 / 4303 | `include_str!` 源码文本契约测试（脆弱文本断言） | — | Low | ⏳ 待确认 |
| O-5 | gdi_base.rs:713-721 | `spawn_pause_forwarder` 11 参数；`make_pause`/`make_resume` 恒为无捕获闭包 | image.rs:362-363、gif.rs:472-473 | Low | ⏳ 待确认 |
| O-6 | image.rs:521-532 | `wallpaper_thread` 9 参数可聚合为单一结构体 | — | Low | ⏳ 待确认 |
| O-7 | gif.rs:513-527 | `gif_wallpaper_thread` 13 参数可聚合 | — | Low | ⏳ 待确认 |
| O-8 | mod.rs:320-330（`GifMemoryStrategy`） | `Adaptive` 语义与 Performance/Balanced 分支重叠，可简化为 3 策略 | 为 pub serde 枚举 | Low | ⏳ 待确认 |
| O-9 | gif_decode.rs:59-77 | `decode_gif_with_cancel_streaming`/`decode_gif_streaming` 为 `decode_gif_inner` 薄封装（均有用） | — | Low | ⏳ 待确认（保留） |

**必要抽象（复核确认，不建议改动）**：`create_pause_channel` vs `create_pause_sender_with_state`（职责不同，各有生产调用方）、`RendererState`（字段互不可派生）、`SubprocessRendererBase`（抽象适度，问题在**利用不足**而非过度）、`validate_renderer_speed`（两渲染器共用的模块级函数）、`GdiCache`（唯一缓存槽，被 gdi_base/image 有效复用）、`gif_memory.rs` 独立文件（约 402 行内聚逻辑，拆分合理）、`#![allow(clippy::too_many_arguments)]` 三处（均已列入 O-5/O-6/O-7 归类）。

### 8.4 文档一致性

| # | 位置 | 问题 | 级别 | 处置 |
|---|---|---|---|---|
| F-1 | 04-wallpaper模块.md:30-44 | 「文件清单」表 13 项行数全部失真（manager.rs 1885→4183、video.rs 891→2080、gif_decode.rs 503→1511、gif.rs 1112→2032 …） | Medium | ✅ 已清理（按实测更新） |
| F-2 | wallpaper/ 下 11 个文件缺 `//!` 文件头 | 仅 mod.rs、mode_dispatch.rs 有文件级 `//!`；其余仅 item 级 `///`，模块层次职责无代码内说明 | Low | ⏳ 待确认（注释补充） |
| F-3 | 04-wallpaper模块.md:33 | 文档对 manager.rs 描述不全（漏述 pure_mode / 原子交换 / liveness 自愈大块） | Low | ✅ 已清理（并入 F-1） |

### 8.5 已实施的安全清理（零行为变更）

1. **R-1** manager.rs `prepare_set_wallpaper` 内重复表达式提出一次复用
2. **R-2** subprocess_base.rs 陈旧「256MB」注释同步为 2048MB（3 处）
3. **D-9** mode_dispatch.rs 移除 Native 分支不可达 `else`
4. **R-3 / F-1 / F-3** 04-wallpaper模块.md 文件清单行数与描述按实测更新

### 8.6 待确认项（涉及公开 API / 跨模块契约 / W-xxx 与 v41-D-xxx 契约 / 测试策略，本轮不实施）

D-1 ~ D-8、R-4 ~ R-28、O-1 ~ O-9、F-2。

> - **D-1/D-2/D-3**：`WallpaperRenderer` trait 三个方法无生产 trait 对象调用点，属公开 trait 契约，删除将改变 trait 面。
> - **R-4/R-13/R-16/R-17/R-18/R-19/R-20/R-21/R-22**：均为跨渲染器骨架重复，收敛需重构生命周期/失败回滚路径，应逐项登记后单独实施。
> - **R-11**：同步 `set_wallpaper` 与异步三阶段路径的重复为 N-004/v41-W-006 已登记权衡，合并需大重构。
> - **R-26/R-27/R-28**：跨 crate 契约/部分重复，语义已分叉，合并需谨慎。
> - **O-1（W-012）**：恒值字段为已登记历史 finding，改公开结构体需同步调用方。

### 8.7 已消解 / 失效的历史 finding

- **W-005**（`update_positions` Span 每迭代重复 `GetSystemMetrics`）：全仓已无 `update_positions` 定义/调用，该方法已整体移除，finding **失效**（建议同步登记表）。
- **W-010**（`should_invoke_wasispi` 拼写错误）：已修正为 `should_invoke_wasapi`。
- **W-002**（Video 子进程退出监听缺失）：已由 `spawn_proc_exit_monitor` + `OwnedProcHandle` 修复。
- **W-013**（窗口查找轮询魔法数字）：已提取为 `WINDOW_FIND_RETRIES`/`WINDOW_FIND_INTERVAL_MS`（subprocess_base.rs:20-22），finding **已消解**。
- **已核实非重复**：扩展名清单（wallpaper 未引入第 4 处，`determine_wallpaper_mode` 委托 `native_wallpaper::is_native_supported`）；`ScalingMode` 语义（唯一定义于 wallpaper，desktop 侧为注册表映射）；子进程连接常量（仅 subprocess_base 集中定义）；窗口查找（`find_window_by_*` 定位自家渲染器窗口 vs `worker_w` 定位 shell 桌面窗口，用途不同）。
