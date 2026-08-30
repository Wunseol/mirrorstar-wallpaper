# audio / ipc / process 模块优化文档

> [← 返回索引](./README.md)

## 模块概要

本模块文档整合三个紧密协作的核心库子模块：`audio`（WASAPI 音量控制）、`ipc`（命名管道 IPC 协议）、`process`（子进程管理）。三者共同支撑视频壁纸（mpv）与网页壁纸（wp-proc）的进程级控制链路。

### audio 子模块

- **模块路径**：`crates/mirrorstar-core/src/audio/`
- **审查文件**：2 个（516 行）
  - `mod.rs`（1 行）— 模块声明
  - `volume.rs`（515 行）— `VolumeControl`：WASAPI 进程音量控制
- **核心结构**：`VolumeControl`（`device_enumerator`/`session_manager`/`session_cache: RefCell<HashMap<u32, IAudioSessionControl2>>`）
- **设计模式**：PID 缓存（cache hit 通过 `GetProcessId` 复核，cache miss 顺带清理失效项）、`new_disabled()` 优雅降级、`unsafe impl Send` + `!Sync`（WASAPI free-threaded 论证）

### ipc 子模块

- **模块路径**：`crates/mirrorstar-core/src/ipc/`
- **审查文件**：4 个（约 1,732 行）
  - `mod.rs`（5 行）— 模块声明与 re-export
  - `client.rs`（722 行）— `NamedPipeClient<T>` 泛型基类 + 连接/读取辅助函数 + `Backoff` 指数退避
  - `wp_proc.rs`（693 行）— `WpProcIpcClient` + `WpProcCommand` 枚举 + `WpProcResponse`（`ResponseStatus` 枚举）
  - `mpv_protocol.rs`（312 行）— `MpvIpcClient`（mpv JSON 协议）
- **核心结构**：`NamedPipeClient<T>`（PhantomData 区分协议）、`MpvIpcClient`、`WpProcIpcClient`
- **设计模式**：「泛型基类 + 协议薄封装」分层、`read_line_with_limit` OOM 防护（1MB 上限）、`Backoff` 指数退避（C-113）、总体 deadline（I01/I03）

### process 子模块

- **模块路径**：`crates/mirrorstar-core/src/process/`
- **审查文件**：2 个（826 行）
  - `mod.rs`（1 行）— 模块声明
  - `manager.rs`（825 行）— `ProcessManager`：子进程启动、监控、终止、Job Object
- **核心结构**：`ProcessManager`（`process_handle`/`job_handle`/`pid`/`args`）
- **设计模式**：Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 保证主进程崩溃时子进程自动终止）、`escape_windows_arg`（MSVCRT argv 规则转义 + 拒绝 `\n`/`\r`）、`wait_and_terminate` 共享等待+强杀+关闭逻辑、`is_running` 用 `WaitForSingleObject(handle, 0)` 消除 STILL_ACTIVE 误判

## v4.0 审查发现（22 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/04-audio.md`、`05-ipc.md`、`06-process.md`
> 严重级别分布：Critical 0 / High 3 / Medium 7 / Low 12
> 维度分布：架构 1 | 并发 4 | 资源 4 | 错误 4 | 性能 2 | 安全 3 | 可维护性 4

### 审查重点说明

三个模块经过 v3.0→v3.5 共 5 轮修复后整体质量较高：audio 的缓存失效防护（C-103/C-026/A03）与优雅降级（T-008）；ipc 的 OOM 防护（I02）和指数退避（C-113）；process 的 STILL_ACTIVE 误判修复（P01）与 Job Object 配置。本次审查聚焦于修复盲区（I-001 超时机制理论盲区、P-001 句柄泄漏）、跨文件不一致与遗留债务。

### audio 模块 findings（6 项）

#### [A-001] [High] [架构设计] volume.rs:92, 14 — `refresh_session_manager` 无生产调用方，音频设备变更完全无感知

**描述**：`VolumeControl` 持有 `device_enumerator: Option<IMMDeviceEnumerator>`（L14），其唯一 post-construction 消费者是 `refresh_session_manager`（L92-103）。但全仓库检索表明，`refresh_session_manager` **仅在单元测试中被调用**，生产代码从未调用它。

进一步地，模块未实现 `IMMNotificationClient`（`IMMDeviceEnumerator::RegisterEndpointNotificationCallback`）来监听默认音频端点变更事件。这意味着：
1. `session_manager`（L15）在 `new()` 时绑定到当时的默认渲染设备。当用户切换默认音频设备（插入/拔出耳机、蓝牙连接、在系统设置中切换输出设备）后，`session_manager` 仍指向旧设备的会话枚举器。
2. 后续 `with_session` 通过该 stale `session_manager` 枚举会话，可能枚举到旧设备的会话或 `GetSessionEnumerator` 返回错误/空列表，导致 `set_process_volume` 对新设备上的 mpv 进程失效。
3. `device_enumerator` 字段在构造后即成为「死字段」——占内存但不产生任何效果，其存在给人「设备变更已被处理」的错觉。

**影响**：用户在播放视频壁纸期间切换音频输出设备（极常见场景：插耳机、连蓝牙音箱），壁纸进程音量控制静默失效，且无任何日志或刷新机制自愈。`docs/02-架构设计/模块设计-Module-Design.md:147` 将「`refresh_session_manager` 音频设备变更处理」列为已实现能力，与实际不符（文档债务）。

**建议**：
- 方案 A（推荐）：实现 `IMMNotificationClient`，在 `OnDefaultDeviceChanged` 回调中触发 `refresh_session_manager`。注意回调来自 WASAPI 线程，需通过 channel/poster 转发到持有 `VolumeControl` 锁的线程，避免在回调中直接加锁导致死锁。
- 方案 B（轻量）：在 `with_session` 的 cache miss 路径或 `set_process_volume` 失败时，惰性调用 `refresh_session_manager` 重试一次（self-healing），无需长期监听。
- 无论哪种方案，若不打算实现设备变更监听，应移除 `device_enumerator` 字段和 `refresh_session_manager`，避免死代码与文档误导；或明确标注「TODO: 设备变更监听未实现」。

#### [A-002] [Medium] [代码逻辑] volume.rs:161 — `with_session` 枚举路径 `GetSession`/`cast` 失败用 `?` 中止，与 `GetProcessId` 的 `continue` 容错策略不一致 ✅ 已修复于 v4.0 Wave 2H

**描述**：`with_session` 的会话枚举循环对三类 WASAPI 调用采取了不一致的容错策略：
- `GetSession(i)` 失败 → `?` 立即传播错误，中止整个枚举
- `cast::<IAudioSessionControl2>()` 失败 → `?` 立即传播错误，中止枚举
- `GetProcessId()` 失败 → `continue` 跳过该会话（A01 修复，注释 L162-164 明确说明「此前用 `?` 传播会导致整个枚举中止，进而丢失后续可能匹配的会话」）

A01 修复已认识到「单一会话异常不应中止枚举」，但仅对 `GetProcessId` 应用了该原则，`GetSession`/`cast` 仍保留 `?`。在实践中，会话列表可能在枚举过程中发生变更（会话被创建/销毁），`GetSession(i)` 对已失效的索引可能返回错误；某些会话也可能不支持 `IAudioSessionControl2` 查询接口。这些情况下当前实现会丢失后续可能匹配目标 PID 的会话。

**建议**：将 `GetSession`/`cast` 也改为 `continue` 容错，与 `GetProcessId` 保持一致策略，使枚举具有完整的单会话容错能力。

#### [A-003] [Medium] [可维护性] volume.rs（整体） — 核心 WASAPI 交互逻辑无任何集成测试覆盖 ✅ 已修复于 v4.0 Wave 2H

**描述**：`volume.rs` 的测试共 12 个用例，但全部围绕 `new_disabled()`（降级 no-op 实例）或纯辅助函数（`is_pid_running`/`collect_stale_pids`）展开。**完全没有覆盖**的核心生产路径：
- `with_session` 的 cache hit + `GetProcessId` 校验路径
- `with_session` 的 cache miss + 枚举匹配路径
- `set_process_volume`/`set_process_mute`/`get_process_volume`/`get_process_mute` 对真实音频会话的端到端操作
- cache miss 路径的 stale PID 清理与 cache hit 路径的失效移除的交互

这意味着 v3.0→v3.5 期间针对缓存校验（C-103/C-026）、枚举容错（A01）、缓存清理（A03）的修复，其正确性仅靠代码审查保证，无自动化回归防护。一旦未来重构破坏这些路径，测试套件仍会全绿。

**建议**：增加集成测试（在测试线程初始化 COM MTA，启动会播放音频的辅助进程，用真实 `VolumeControl::new()` 验证 `set_process_volume`/`get_process_volume` 的往返一致性，无音频设备的 CI 环境跳过）；至少为 `with_session` 的 cache hit/miss 分支添加 mock 层测试。

#### [A-004] [Low] [资源管理] volume.rs:52, 13-21 — `VolumeControl` 无显式 `Drop`，COM 接口 Release 跨 apartment 发生

**描述**：`VolumeControl` 无 `impl Drop`，依赖 `windows` crate 自动生成的 `Drop`（调用 `IUnknown::Release`）。结构体 `unsafe impl Send` 后，实例被 `Arc<std::sync::Mutex<VolumeControl>>` 包裹并 clone 到多处（`VideoRenderer`、`WallpaperEngine`、app_state）。最后一个 `Arc` clone 可能在任意线程被 drop（主线程 STA、tokio worker MTA、pause 线程 MTA）。

`Send` 的 SAFETY 论证引用了 WASAPI free-threaded 文档，论证 `Release` 跨 apartment 安全。该论证对 WASAPI 接口本身成立。但存在两个隐患：
1. 论证依赖「使用 VolumeControl 的线程已初始化 COM」。若最后一个 `Arc` clone 在一个**未初始化 COM** 的线程 drop，`Release` 在无 COM 环境下调用。
2. `session_cache` 中缓存的 `IAudioSessionControl2` 在 `refresh_session_manager`/cache 清理时被 drop，其 `Release` 时机与 apartment 依赖调用方清空缓存的线程，缺乏集中化释放点。

**修复状态**：✅ 已修复于 v4.0 Wave 3D（方案 ②：在 `unsafe impl Send` 上方 SAFETY 注释末尾追加"Drop 契约（A-004）"段落，文档化"不实现显式 `impl Drop`、依赖 COM 自动 `Release()`、最后一个 `Arc clone` 的 drop 必须发生在已初始化 COM 的线程"调用方契约；未新增 `impl Drop`，避免跨 apartment 释放复杂度）

**建议**：增加 `impl Drop for VolumeControl`，在 drop 时显式清空 `session_cache`，并 `take()` 出 `session_manager`/`device_enumerator`，集中化释放点便于审计。或在 `Send` 的 SAFETY 注释中补充说明「最后一个 Arc clone 的 drop 必须发生在已初始化 COM 的线程」这一调用方契约。

#### [A-005] [Low] [错误处理] volume.rs:287 — `is_pid_running` 对 `WAIT_FAILED` 按「已退出」处理，可能误清有效缓存项

**描述**：`is_pid_running` 的返回值仅判断 `wait_result == WAIT_TIMEOUT`（仍在运行），其余（含 `WAIT_OBJECT_0`、`WAIT_FAILED`、其他意外值）均返回 `false`（已退出）。`WAIT_FAILED` 可能由瞬态错误触发（如内存压力下临时句柄表查询失败、访问权限瞬态拒绝）。此时进程实际仍在运行，但 `is_pid_running` 返回 `false`，导致 `collect_stale_pids` 将其收集为「失效 PID」，`with_session` 随后从 `session_cache` 移除该有效缓存项。下次访问该 PID 时走 cache miss 重新枚举（功能正确但产生一次冗余枚举）。

**修复状态**：✅ 已修复于 v4.0 Wave 3D（采纳"追求更稳健"方案：导入 `WAIT_FAILED`，将单一 `== WAIT_TIMEOUT` 判定改为 match 表达式，`WAIT_FAILED` 分支记录 `tracing::trace!(pid, ?wait_result, "...")` 并返回 `true` 保守保留缓存项；`WAIT_TIMEOUT`/`WAIT_OBJECT_0` 与其他值保持原行为）

**建议**：可接受现状（功能性降级，无数据损坏，对壁纸场景 1 个 mpv 进程影响可忽略）。若追求更稳健，可对 `WAIT_FAILED` 记录 `tracing::trace!` 并返回 `true`（保守保留缓存项）。

#### [A-006] [Low] [可维护性] volume.rs:290-298 — `collect_stale_pids` 文档自相矛盾（「纯函数」 vs 「调用 Win32 API」）

**描述**：`collect_stale_pids` 的文档注释前后矛盾：L293 称「此纯函数从 `with_session` 的枚举路径抽离"过滤失效 PID"逻辑」，L296-298 立即否定「注意：本函数调用 `is_pid_running`（内部使用 Win32 `OpenProcess` / `WaitForSingleObject`），因此并非纯逻辑函数」。`collect_stale_pids` 确实调用了 `is_pid_running`，后者执行 Win32 syscall 并依赖外部进程状态，具有副作用且非确定性，不是纯函数。

**修复状态**：✅ 已修复于 v4.0 Wave 3D（删除"纯函数"表述，改为"此辅助函数从 ..."；保留"对 Win32 进程查询的薄封装，非纯函数"表述不变）

**建议**：删除 L293 的「纯函数」表述，改为「此辅助函数从 `with_session` 的枚举路径抽离"过滤失效 PID"逻辑...注意：内部调用 `is_pid_running`（Win32 `OpenProcess`/`WaitForSingleObject`），是对进程查询的薄封装，非纯函数。」

### ipc 模块 findings（9 项）

#### [I-001] [High] [并发安全] client.rs:312-350, 380 — `read_line_with_timeout` 在部分行数据场景下 `fill_buf` 无限阻塞，超时机制失效

**描述**：`read_line_with_timeout` 在 `PeekNamedPipe` 报告有数据后，将整行读取委托给 `read_line_with_limit`。但 `read_line_with_limit` 内部循环调用 `BufReader::fill_buf()`，该方法在内部缓冲区耗尽后会调用底层 `File::read()`。对于处于 `PIPE_WAIT`（阻塞模式）的命名管道，当已 peek 的数据被消费完毕但仍未遇到 `\n` 时，下一次 `fill_buf()` → `File::read()` 会**无限阻塞**，直到新数据到达或管道关闭。

具体流程：
1. `PeekNamedPipe` 报告 `total_avail = N` 字节可读（N > 0）
2. `read_line_with_limit` 首次 `fill_buf()` 读取 N 字节，不含 `\n`，`consume(N)` 后继续循环
3. 第二次 `fill_buf()`：BufReader 内部缓冲已空，调用 `File::read()` —— 管道无新数据但未关闭 → **阻塞**
4. `read_line_with_timeout` 外层循环的 deadline 检查**永远不会被执行**，因为控制流已进入 `read_line_with_limit` 的阻塞 `fill_buf` 中

这直接破坏了 I01/I03/I04 修复所建立的「超时保证」：超时机制在「服务端发送部分行数据（无换行符）且保持管道打开」的场景下完全失效，可能导致 UI 线程无限冻结。

**实际触发概率**：低。mpv 和 wp-proc 写入的 JSON 行通常 < 200 字节，通过单次 `write_all` 原子写入。但子进程在写入中途崩溃但未关闭管道句柄、恶意/异常子进程故意发送无换行数据、系统资源耗尽导致 `write_all` 部分写入等场景可能触发。

**建议**：
- 方案 A（推荐）：将管道设置为 `PIPE_NOWAIT` 非阻塞模式，在 `read_line_with_limit` 中处理 `ERROR_NO_DATA` 时返回当前已读取的部分，由外层 `read_line_with_timeout` 循环重新 peek + 累积，使 deadline 检查始终保持有效
- 方案 B：在 `read_line_with_limit` 中传入 `total_avail` 参数，每次 `fill_buf` 仅消费不超过 `total_avail` 的字节数
- 方案 C：使用 Windows overlapped I/O（`ReadFileEx` + `CancelIoEx`）实现真正的超时取消
- 补充测试：添加「服务端发送部分行（无 `\n`）后静默」的测试用例

#### [I-002] [Medium] [错误处理] client.rs:384-387 — `PeekNamedPipe` 失败时错误被完全丢弃返回空字符串 ✅ 已修复于 v4.0 Wave 2H

**描述**：`read_line_with_timeout` 中 `PeekNamedPipe` 失败时，错误被完全丢弃，返回 `Ok(String::new())`。调用方 `read_response_line_with_timeout` 将空字符串解释为 `IpcDisconnected("管道已关闭")`。但 `PeekNamedPipe` 失败的原因可能包括：句柄无效、访问被拒绝、管道状态异常等，并非一定是「管道已关闭」。静默吞错导致：① 真实错误原因无法追踪（无日志、无错误传播）；② 调用方收到误导性的「管道已关闭」错误，可能触发不恰当的断连重连逻辑。

**建议**：至少记录 `tracing::debug!` 或 `tracing::warn!` 日志，包含原始 Windows 错误码；考虑区分「管道已关闭」（`ERROR_BROKEN_PIPE` / `ERROR_NO_DATA`）与其他错误，前者返回空字符串（EOF 语义），后者返回 `IpcError`。

#### [I-003] [Medium] [并发安全] client.rs:191-195, 223 — `connect_named_pipe` 使用 `std::thread::sleep` 阻塞，文档约定无编译期保障 ✅ 已修复于 v4.0 Wave 2H

**描述**：`connect_named_pipe` 使用 `std::thread::sleep` 进行重试等待，是阻塞操作。文档注释说明「必须通过 `tokio::task::spawn_blocking` 包裹」，但这仅是**文档约定**，无编译期保障。实际上，整个 `NamedPipeClient` API 都是同步阻塞的：`connect`、`read_response_line_with_timeout`、`send_line`。如果调用方在 tokio async 上下文中直接调用任何方法而未包裹 `spawn_blocking`，会阻塞 tokio worker 线程，严重时冻结整个 runtime。当前调用方确实在 `spawn_blocking` 中调用，但 API 层面无防护，新增调用方可能违反约定。

**建议**：通过类型系统约束（将 `NamedPipeClient` 标记为 `!Send` 配合 `spawn_blocking`，或提供 async 封装层）；或在文档中更醒目地标注「ALL METHODS ARE BLOCKING」；长期考虑提供 async 版本（基于 `tokio::io::AsyncRead/AsyncWrite` 或 Windows overlapped I/O）。

#### [I-004] [Low] [可维护性] mpv_protocol.rs:14-19 — `MpvEvent` 结构体为死代码

**描述**：`MpvEvent` 结构体定义为 `pub`，但在整个工作区内仅在 `mpv_protocol.rs` 自身的单元测试中使用，生产代码中无任何引用。当前 `send_command_with_timeout` 在读取到非响应行（mpv 事件）时直接跳过，从不反序列化为 `MpvEvent`。如果未来需要处理事件（如 `end-file`、`property-change`），此结构已就绪；但当前属于死代码。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（方案 ①：保留 `MpvEvent` 结构体，添加 `#[allow(dead_code)]` 属性 + 注释说明"预留用于未来 mpv 事件处理（如 `end-file` / `property-change`），当前 `send_command_with_timeout` 仅跳过非响应行不反序列化为 `MpvEvent`，保留此结构与测试是为事件处理能力就绪，避免未来重复定义"）

**建议**：若计划未来支持事件处理：添加 `// #[allow(dead_code)]` 并补充注释说明预留意图；若无计划：移除该结构及其测试。

#### [I-005] [Low] [安全] mpv_protocol.rs:166-178 — `set_volume(f32)`/`set_speed(f32)` 未做范围校验

**描述**：`set_volume(f32)` 和 `set_speed(f32)` 将 `f32` 直接 `to_string()` 后作为属性值发送给 mpv。文档注释标注了有效范围（volume: 0-100，speed: 0.25-4.0），但代码未做任何校验。`f32::NAN.to_string()` = `"NaN"`，`f32::INFINITY.to_string()` = `"inf"` —— mpv 会返回 error 响应，但因 `set_property` 走 fire-and-forget 路径（`send_command_no_wait`），错误被静默丢弃。负数音量、超大速度等越界值同样无校验，依赖 mpv 端拒绝。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（`set_volume` / `set_speed` 入口添加 `is_finite()` + `RangeInclusive::contains()` 范围校验（volume `[0.0, 100.0]` / speed `[0.25, 4.0]`），越界返回 `MirrorStarError::InvalidArgument { reason }`；新增 4 个单元测试 `set_volume_rejects_nan` / `set_volume_rejects_out_of_range` / `set_speed_rejects_nan` / `set_speed_rejects_out_of_range` 覆盖 NaN/越界场景；clippy `manual_range_contains` 提示下用 `!(0.0..=100.0).contains(&volume)` 替代 `volume < 0.0 || volume > 100.0`）

**建议**：在 `set_volume` / `set_speed` 中添加范围校验，越界时返回 `MirrorStarError::InvalidConfig`；添加 `f32::is_finite()` 检查，拒绝 NaN/inf。

#### [I-006] [Low] [可维护性] client.rs:117, wp_proc.rs:12, mpv_protocol.rs:57,144 — 超时默认值分散且不一致

**描述**：IPC 模块中各协议的超时默认值分散且不一致：`read_response_line` 5s（通用读取）、`PLAY_COMMAND_TIMEOUT` 15s（wp-proc play 命令）、`send_command` 5s（mpv 同步命令）、`get_property` 2s（mpv 属性查询）。每个超时值都有合理的业务理由，但值分散在各文件中，调整时需跨文件搜索。`read_response_line` 的 5s 与 `wp_proc` 的 15s 差异容易导致调用方误用错误入口。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（方案 ②：在 `crates/mirrorstar-core/src/ipc/mod.rs` 顶部追加模块级 `//!` 文档，含模块概述、文件结构（client.rs / mpv_protocol.rs / wp_proc.rs）与超时对照表（4 行：`read_response_line` 5s 通用读取 / `PLAY_COMMAND_TIMEOUT` 15s wp-proc play / `send_command` 5s mpv 同步命令 / `get_property` 2s mpv 属性查询）+ 设计说明"各超时值反映业务语义差异，并非不一致；调整时需跨文件搜索 `Duration::from_secs` 避免遗漏"。未集中常量到独立 config.rs 以避免引入额外抽象。）

**建议**：考虑将超时常量集中到 `ipc/mod.rs` 或独立的 `ipc/config.rs` 中，按协议分组；或至少在 `mod.rs` 中添加超时对照表注释。

#### [I-007] [Low] [资源管理] wp_proc.rs:261-265, mpv_protocol.rs:219-223 — `Drop` 双重 disconnect 冗余清理

**描述**：`WpProcIpcClient` 和 `MpvIpcClient` 都实现了 `Drop`，在 `drop` 中调用 `self.disconnect()`。而 `disconnect()` 内部调用 `self.inner.disconnect()`。随后 `self.inner`（`NamedPipeClient`）被 drop 时，其 `Drop` 实现再次调用 `self.disconnect()`。由于 `disconnect` 使用 `Option::take()` 实现幂等性，第二次调用是安全的 no-op，但这是冗余的双重清理，增加了阅读理解成本。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（方案 ②：保留 `WpProcIpcClient` / `MpvIpcClient` 的外层 `Drop` 实现，在 `impl Drop` 上方追加注释说明"外层 Drop 调用 `disconnect()` 后，`self.inner`（`NamedPipeClient`）drop 时会再次调用 `disconnect()`，因 `disconnect` 使用 `Option::take()` 实现幂等性，第二次调用是安全 no-op；保留外层 Drop 是为记录 `tracing::info!` 断开日志（`NamedPipeClient::Drop` 不记录日志）"。未移除外层 Drop 以保留断开日志。）

**建议**：移除 `WpProcIpcClient` / `MpvIpcClient` 的 `Drop` 实现，改为在 `NamedPipeClient::Drop` 中统一清理；或保留现状但添加注释说明「inner Drop 会再次调用 disconnect，因 take() 幂等故安全」。

#### [I-008] [Low] [可维护性] wp_proc.rs:163-171 — `send_command_no_wait` 文档未说明延迟响应副作用

**描述**：`WpProcIpcClient::send_command_no_wait` 的文档注释未说明 fire-and-forget 命令的延迟响应可能留在管道缓冲区中，被后续同步命令的 `send_command_with_timeout` 读取到并因 `request_id` 不匹配而跳过。对比 `MpvIpcClient::send_command_no_wait`（mpv_protocol.rs:124-126），mpv 侧有明确文档说明此副作用，wp_proc 侧缺少同等说明。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（`WpProcIpcClient::send_command_no_wait` 文档注释追加两段延迟响应副作用说明：① "调用方无法感知子进程是否成功执行命令，若命令失败子进程仍会异步返回 error 响应，但本方法不读取该响应"；② "后续的 `send_command_with_timeout` 调用可能会读到这条延迟 error 响应并因 `request_id` 不匹配而跳过"。与 mpv 侧 mpv_protocol.rs:124-126 文档一致，保留原有"对于 `play` 等需要确认 WebView2 初始化成功的命令，仍应使用同步路径"提示。）

**建议**：在 `WpProcIpcClient::send_command_no_wait` 文档中补充与 mpv 侧一致的延迟响应说明。

#### [I-009] [Low] [安全] wp_proc.rs:174, 211 — `play(source)`/`navigate(url)` 未在 IPC 层做 URL 协议白名单校验

**描述**：`WpProcIpcClient::play(source)` 和 `navigate(url)` 接受任意字符串，未在 IPC 层进行 URL 协议白名单校验或路径校验，直接序列化为 JSON 发送给子进程。`MirrorStarError` 已定义 `InvalidUrl { scheme }` 变体，表明项目存在 URL 协议白名单机制，但该校验位于上层（配置加载/命令处理），IPC 层作为最后一道防线未做防御性校验。当前调用链中，`source` 和 `url` 来自用户配置，上层已做校验，IPC 层的校验属于纵深防御。

**修复状态**：✅ 已修复于 v4.0 Wave 3E（方案 ②：在 `play` / `navigate` 方法文档注释追加"调用方契约"段落，明确"URL/源路径校验由调用方负责（上层 `WallpaperEngine` / 命令处理层已做协议白名单校验），本方法作为 IPC 层薄封装不做防御性校验，直接序列化 source/url 为 JSON 发送给子进程；若未来需要在 IPC 层做纵深防御，可参考 `MirrorStarError::InvalidUrl { scheme }` 变体添加协议白名单"。未在 IPC 层添加重复校验逻辑以避免与上层冗余。）

**建议**：在 `play` / `navigate` 中添加防御性校验（至少拒绝 `javascript:` / `file:` 等危险协议），与上层校验形成纵深防御；或在文档中明确标注「调用方负责 URL 校验，本方法不做校验」。

### process 模块 findings（7 项）

#### [P-001] [High] [资源管理] manager.rs:82-85, 187-189 — `start()` 在前一进程已自行退出时跳过 `stop()`，直接覆盖句柄导致内核句柄泄漏

**描述**：`start()` 开头清理前一进程的逻辑：`if self.is_running() { self.stop()?; }`。`is_running()` 在以下情况返回 `false`：`process_handle` 为 `None`（正常）、`WaitForSingleObject(handle, 0)` 返回 `WAIT_OBJECT_0`（进程已退出）——**问题路径**、返回 `WAIT_FAILED`——问题路径。

当上一进程**自行退出**（mpv 播放结束、崩溃、被外部 kill）时，`is_running()` 返回 `false`，`start()` 跳过 `stop()` 调用。但此时 `self.process_handle`、`self.job_handle`、`self.pid` 仍持有上一进程的旧值。随后直接用新句柄覆盖：
- `self.process_handle = Some(proc_info.hProcess)`：旧 `Some(old_handle)` 被 drop。但 `HANDLE` 是 `Copy` 的 newtype，`Option<HANDLE>` 的 drop **不会**调用 `CloseHandle`，旧进程句柄泄漏。
- `self.job_handle = Some(job)`：同理，旧 Job Object 句柄泄漏。

每发生一次「上一进程自退出后重启」即泄漏 2 个内核句柄（1 进程句柄 + 1 Job Object 句柄）。

**影响**：`ProcessManager` 的 `start()` 被设计为可重入重启。在壁纸应用场景中，切换视频壁纸时若 `SubprocessRendererBase` 被复用且上一 mpv 已退出，每次切换泄漏 2 个句柄。长时间运行会话下句柄持续累积（Windows 默认每进程句柄上限 10000）。

**建议**：在 `start()` 开头无条件清理旧句柄，无论 `is_running()` 返回什么：
```rust
if self.process_handle.is_some() || self.job_handle.is_some() {
    self.stop()?;  // stop() 对已退出进程会立即返回
}
```
或将 `is_running()` 分支与「已退出但句柄未释放」分支统一交给 `stop()` 处理。

#### [P-002] [Medium] [性能] manager.rs:370-371, 206-227 — `Drop` 调用 `stop()` 阻塞最长 8s，拖慢应用退出 ✅ 已修复于 v4.0 Wave 2H

**描述**：`impl Drop for ProcessManager` 在进程仍运行时调用 `self.stop()`。`stop()` 调用 `wait_and_terminate(handle, self.pid, 3000)`，其流程：① `WaitForSingleObject(handle, 3000)` 等待最多 3 秒期望进程优雅退出；② 若 `WAIT_TIMEOUT`（未退出）→ `terminate_and_wait`：`TerminateProcess` + `WaitForSingleObject(handle, 5000)` 再等 5 秒确认。最坏情况：`Drop` 阻塞 3s + 5s = **8 秒**。

在应用退出路径中，`ProcessManager` 的 drop 会同步阻塞主线程/退出流程最多 8 秒，用户感知为「关闭应用卡住」。在崩溃 unwind 路径（panic）下 `Drop` 同样阻塞 8s，影响崩溃恢复。

**对比**：`stop()` 的优雅等待语义适用于「主动停止」场景。但 `Drop` 场景下应用已在退出，无需优雅等待——Job Object 的 `KILL_ON_JOB_CLOSE` 本就保证 job 句柄关闭时内核 kill 子进程。

**建议**：`Drop` 中不调用完整 `stop()`，而是直接 `TerminateProcess` 后立即关闭句柄（不等 5s 确认），或仅关闭 job 句柄依赖 `KILL_ON_JOB_CLOSE` 内核语义终止子进程。若仍希望在 `stop()`（主动调用）保留优雅等待，可将 `wait_and_terminate` 的 timeout 参数化，`Drop` 传入 0 或极小值。

#### [P-003] [Medium] [性能] manager.rs:83-85 — `start()` 检测到旧进程仍在运行时同步等待 8s 才启动新进程，壁纸切换延迟 ✅ 已修复于 v4.0 Wave 2H

**描述**：`start()` 在 `is_running()` 为 true 时调用 `self.stop()?`，如 P-002 所述，`stop()` 最坏阻塞 8 秒。在壁纸切换场景下（从一个视频壁纸切换到另一个），若上一 mpv 仍在播放，`start()` 会先同步等待其退出/强杀（最长 8s）再创建新进程，用户感知为「切换视频壁纸卡顿最多 8 秒」。

此阻塞特性已在方法文档中标注为「P04 阻塞方法」，建议调用方用 `spawn_blocking` 包裹。但即便在 `spawn_blocking` 中，8 秒延迟仍传递给用户（切换操作 8 秒才完成），仅避免了阻塞 tokio runtime，未解决 UX 延迟。

**建议**：切换场景下无需优雅等待旧 mpv（切换即意味着立即替换），可直接 `TerminateProcess` 立即终止后启动新进程。或将 `stop()` 拆分为 `stop_graceful(timeout)`（主动停止，保留优雅等待）与 `stop_immediate()`（切换/退出，立即终止），`start()` 内部用 `stop_immediate()`。

#### [P-004] [Medium] [错误处理] manager.rs:255, 252-254, 371 — `wait_and_terminate` 始终返回 `Ok(())`，`Result` 返回类型误导且产生死代码分支 ✅ 已修复于 v4.0 Wave 2H

**描述**：`wait_and_terminate` 签名返回 `Result<(), MirrorStarError>`，但实现中所有错误路径（`TerminateProcess` 失败、强杀后等待 `WAIT_FAILED`、`CloseHandle` 失败）均仅记录日志或忽略，**始终返回 `Ok(())`**。文档明确说明「返回值始终为 `Ok(())`...`Result` 返回类型保留以供未来扩展」。

这一设计产生两个问题：
1. **API 误导**：`stop()` 签名 `Result<(), MirrorStarError>` 让调用方认为可能失败并需处理错误，但实际 `stop()` 永远返回 `Ok`。调用方的 `if let Err(e) = ...` 分支（如 `Drop` L371）成为死代码。
2. **静默吞错**：`TerminateProcess` 失败（进程未能强杀）、`CloseHandle` 失败（句柄泄漏）等真实错误被降级为日志，调用方无法据返回值采取补救措施。

**建议**：方案 A（消除误导）：将 `wait_and_terminate`/`stop()`/`stop_handle` 返回类型改为 `()`，明确「终止失败仅记录日志」的契约；方案 B（真正上报）：让 `wait_and_terminate` 在 `TerminateProcess` 失败或最终 `WaitForSingleObject` 仍为 `WAIT_TIMEOUT` 时返回 `Err`。推荐方案 A（当前实现本就无意上报，签名应反映真实语义）。

#### [P-005] [Low] [安全] manager.rs:407-416 — `escape_windows_arg` 拒绝 `\n`/`\r` 但未拒绝 `\0`（NUL 截断命令行风险）

**描述**：`escape_windows_arg` 在 N-009 修复中拒绝了含 `\n`/`\r` 的参数，但未拒绝 `\0`（NUL 字符）。`start()` 构造命令行时 `cmdline.encode_utf16().chain(std::iter::once(0))`，若某参数含 `\0`，`encode_utf16()` 会将其编码为 `0x0000`，与结尾的 NUL 终止符无法区分。`CreateProcessW` 的 `lpCommandLine` 按 NUL 终止读取，遇到参数中的 NUL 会**提前截断**命令行，导致后续参数丢失或截断点恰好落在某参数中间时 mpv 收到残缺参数。

N-009 已识别换行符注入风险，但 NUL 截断是更直接的命令行完整性破坏，未被同一防线拦截。实际风险低（NTFS 不允许文件名含 NUL，TOML/JSON 解析器通常拒绝含 NUL 的字符串），但作为纵深防御应一致拒绝。

**修复状态**：✅ 已修复于 v4.0 Wave 3F（在 `escape_windows_arg` 的 `if` 条件追加 `|| arg.contains('\0')` 检查；错误消息从"包含换行符（\\n/\\r）"改为"包含不可用控制字符（\\n / \\r / \\0）"；新增 2 个单元测试 `escape_windows_arg_rejects_nul`（中间含 `\0`）与 `escape_windows_arg_rejects_nul_at_end`（末尾含 `\0`），参照既有 `\n` 拒绝测试模式编写）

**建议**：在 `if` 条件中增加 NUL 检查：`if arg.contains('\n') || arg.contains('\r') || arg.contains('\0')`，并补充对应测试。

#### [P-006] [Low] [可维护性] manager.rs:116 — `CREATE_UNICODE_ENVIRONMENT` 标志在 `lpEnvironment=None` 时为无效标志

**描述**：`CreateProcessW` 调用传入 `CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`，但 `lpEnvironment` 为 `None`（继承父进程环境）。按 Win32 文档，`CREATE_UNICODE_ENVIRONMENT` 指示 `lpEnvironment` 参数指向的是 Unicode 环境块。当 `lpEnvironment` 为 `None`（NULL）时，子进程继承父进程环境块，**该标志被忽略**。因此 `CREATE_UNICODE_ENVIRONMENT` 在此调用中是 no-op 死标志，不产生功能影响，但可能误导读者以为「显式指定了 Unicode 环境」。

**修复状态**：✅ 已修复于 v4.0 Wave 3F（从 `use windows::Win32::System::Threading::{...}` 删除 `CREATE_UNICODE_ENVIRONMENT` import（避免 unused import 警告），`start()` 的 `CreateProcessW` 调用 `dwCreationFlags` 改为 `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`，加 3 行注释说明"lpEnvironment=None 时该 flag 无效"）

**建议**：移除 `CREATE_UNICODE_ENVIRONMENT`，保留 `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`。若未来需要显式传递自定义 Unicode 环境块，再添加该标志并配套传入 `lpEnvironment`。

#### [P-007] [Low] [错误处理] manager.rs:266-275, 291-303 — `wait_and_terminate` 的 WAIT_FAILED 路径对可能无效的句柄调用多个 API 产生日志噪声

**描述**：`wait_and_terminate` 在 `WAIT_FAILED` 分支调用 `terminate_and_wait(handle, pid)`。`terminate_and_wait` 对该（可能无效的）句柄依次调用 `TerminateProcess(handle, 1)`（失败记录 error）、`WaitForSingleObject(handle, 5000)`（返回 `WAIT_FAILED` 时记录 warn）、回到 `wait_and_terminate` 后 `CloseHandle(handle)`（失败忽略）。

`WAIT_FAILED` 的常见根因正是句柄无效（如已关闭、权限不足、重复终止）。此时对同一无效句柄连续调用三个 API，每个都失败并产生日志，形成日志噪声（一次终止失败产生 3 条错误/警告日志）。P02 修复的本意是「WAIT_FAILED 时兜底终止」，但未区分「句柄无效」（终止无意义，应直接关闭并放弃）与「其他可重试错误」。

**修复状态**：✅ 已修复于 v4.0 Wave 3F（采纳"简化"方案：`WAIT_FAILED` 分支不再调用 `Self::terminate_and_wait(handle, pid)`，改为仅记录一条 `tracing::warn!(pid, error = ?std::io::Error::last_os_error(), "WaitForSingleObject 返回 WAIT_FAILED，跳过 terminate_and_wait 直接 CloseHandle")`，控制流自然落入末尾 `CloseHandle` 清理路径，函数仍返回 `Ok(())`（与 P-004 Wave 2H 契约一致）；`WAIT_TIMEOUT` 和 `WAIT_OBJECT_0` 分支保持不变；`wait_and_terminate_handles_wait_failed` 测试 doc comment 更新为 3 阶段演进史，测试体保持不变）

**建议**：在 `WAIT_FAILED` 分支记录 `GetLastError()` 并判断错误码：若为 `ERROR_INVALID_HANDLE`（6），说明句柄已无效，跳过 `terminate_and_wait` 直接 `CloseHandle`；或简化为 `WAIT_FAILED` 时仅记录一次包含 `GetLastError` 的警告，跳过 `terminate_and_wait` 直接进入 `CloseHandle`。

## v3.x 已修复问题

### audio 模块（3 项 + v1.0 修复项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| A01 | Medium | `with_session` 枚举时 `GetProcessId()?` 失败即中止整个枚举 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 A-002 发现 `GetSession`/`cast` 仍用 `?` 中止 |
| A02 | Low | `set_process_volume` 不校验 volume 参数范围（0.0~1.0） | ✅ 已修复（v3.5.3）— `clamp(0.0, 1.0)` |
| A03 | Low | `session_cache` 无主动过期机制，COM 对象泄漏 | ✅ 已修复（v3.5.3）— 枚举路径顺带清理失效项 |
| VolumeControl unsafe impl Sync | — | 移除 `Sync`，仅保留 `Send`，变为 `!Sync` | ✅ 已修复（v1.0） |
| 音频会话线性扫描 | — | 新增 `session_cache` PID 缓存 | ✅ 已修复（v1.0） |

### ipc 模块（5 项 + v1.0 修复项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| I01 | Medium | `read_response_line_with_timeout` 循环跳过空行无总体截止时间 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 I-001 发现 deadline 在 fill_buf 阻塞时失效 |
| I02 | High | `read_line_with_timeout` 的 `MAX_LINE_BYTES` 检查在 `read_line` 返回之后执行，OOM 防护失效 | ✅ 已修复（v3.5.1）— `read_line_with_limit` 增量读取 — ⚠️ v4.0 I-001 发现 fill_buf 仍可阻塞 |
| I03 | Medium | `send_command_with_timeout` 响应匹配循环无总体截止时间 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 I-001 同样影响此路径 |
| I04 | Low | `WpProcIpcClient::send_command` 使用 5s 超时，WebView2 初始化可能超时 | ✅ 已修复（v3.5.3）— `PLAY_COMMAND_TIMEOUT = 15s` |
| I05 | Low | `connect_named_pipe` 使用 `std::thread::sleep` 阻塞未标注 | ✅ 已修复（v3.5.3）— 文档标注 blocking — ⚠️ v4.0 I-003 发现无编译期保障 |
| IPC read_line 阻塞无超时 | — | `read_line_with_timeout` 使用 `PeekNamedPipe`，5 秒超时 | ✅ 已修复（v1.0） |
| IPC 客户端重复代码 | — | 提取 `NamedPipeIpcClient` trait | ✅ 已修复（v1.0） |
| WpProcResponse.status 是 String | — | 改为 `ResponseStatus` 枚举 | ✅ 已修复（v1.0） |

### process 模块（5 项 + v1.0 修复项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| P01 | Medium | `is_running` 使用 `exit_code == STILL_ACTIVE (259)` 误判 | ✅ 已修复（v3.5.2）— 改用 `WaitForSingleObject(handle, 0)` — ⚠️ v4.0 P-001 发现自退出路径句柄泄漏 |
| P02 | Low | `stop`/`stop_handle` 中 `WAIT_FAILED` 静默吞错 | ✅ 已修复（v3.5.3）— ⚠️ v4.0 P-007 发现日志噪声 |
| P03 | Low | `pid()` 进程退出后仍返回旧 PID | ✅ 已修复（v3.5.3）— `is_running` 校验 |
| P04 | Low | `start`/`stop` 阻塞语义未标注 | ✅ 已修复（v3.5.3）— 文档标注 blocking — ⚠️ v4.0 P-002/P-003 发现 Drop/start 阻塞影响 UX |
| P05 | Low | `stop`/`stop_handle` ~25 行重复 | ✅ 已修复（v3.5.3）— 提取 `wait_and_terminate` — ⚠️ v4.0 P-004 发现返回类型误导 |
| 命令行参数转义不完整 | — | `escape_windows_arg` 遵循 MSVCRT argv 规则 | ✅ 已修复（v1.0）— ⚠️ v4.0 P-005 发现未拒绝 `\0` |
| 缺少 CREATE_NO_WINDOW | — | 创建进程标志包含 `CREATE_NO_WINDOW` | ✅ 已修复（v1.0）— ⚠️ v4.0 P-006 发现 `CREATE_UNICODE_ENVIRONMENT` 为死标志 |
| args 字段存储后不读取 | — | 结构体已无 args 字段 | ✅ 已修复（v1.0） |

## 优化目标与方案

### v4.0 优先修复（High，3 项）

1. **A-001 音频设备变更监听**：实现 `IMMNotificationClient` 或 `with_session` 失败时惰性 `refresh_session_manager`；或移除死字段 `device_enumerator` 避免文档误导。
2. **I-001 超时机制理论盲区**：将管道设置为 `PIPE_NOWAIT` 非阻塞模式，`fill_buf` 处理 `ERROR_NO_DATA` 返回部分数据，外层循环重新 peek + 检查 deadline；补充「部分行数据后静默」测试。
3. **P-001 句柄泄漏**：`start()` 开头无条件清理旧句柄（`if process_handle.is_some() || job_handle.is_some() { stop()?; }`），`stop()` 对已退出进程立即返回无阻塞。

### v4.0 系统性修复（Medium，7 项）

4. **A-002 枚举容错一致**：`GetSession`/`cast` 改为 `continue`，与 `GetProcessId` 一致。
5. **A-003 集成测试覆盖**：增加 WASAPI 真实会话的端到端测试 + cache hit/miss mock 测试。
6. **I-002 `PeekNamedPipe` 错误日志**：记录 Windows 错误码，区分「管道已关闭」与其他错误。
7. **I-003 阻塞 API 类型约束**：`NamedPipeClient` 标记 `!Send` 或提供 async 封装层。
8. **P-002 `Drop` 阻塞优化**：`Drop` 中直接 `TerminateProcess` + 立即关闭句柄，或仅依赖 `KILL_ON_JOB_CLOSE`，不等 5s 确认。
9. **P-003 切换场景立即终止**：`start()` 内部用 `stop_immediate()`，或将 `stop()` 拆分为 `stop_graceful`/`stop_immediate`。
10. **P-004 返回类型修正**：`wait_and_terminate`/`stop()` 返回类型改为 `()`，消除死代码分支。

### v4.0 渐进优化（Low，12 项）

11-22. `VolumeControl` 显式 `Drop`（A-004）、`is_pid_running` WAIT_FAILED 保守保留（A-005）、`collect_stale_pids` 文档修正（A-006）、`MpvEvent` 死代码清理（I-004）、mpv `set_volume`/`set_speed` 校验（I-005）、超时常量集中（I-006）、`Drop` 双重 disconnect 文档化（I-007）、`send_command_no_wait` 延迟响应文档（I-008）、IPC 层 URL 白名单纵深防御（I-009）、`escape_windows_arg` 拒绝 `\0`（P-005）、移除 `CREATE_UNICODE_ENVIRONMENT` 死标志（P-006）、`WAIT_FAILED` 日志噪声优化（P-007）。
