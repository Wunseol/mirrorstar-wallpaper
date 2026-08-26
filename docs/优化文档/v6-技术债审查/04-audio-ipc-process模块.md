# v6.0 技术债审查 - audio / ipc / process 模块

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 三子模块合并文档 | 模块路径：`crates/mirrorstar-core/src/{audio,ipc,process}/`

## 1. 当前状态摘要

### 1.1 模块职责

- **audio**：基于 Windows WASAPI 的进程级音量控制。`VolumeControl` 持有 `IMMDeviceEnumerator` / `IAudioSessionManager2` 与 PID → `IAudioSessionControl2` 缓存，通过 `ISimpleAudioVolume` 控制指定进程的音量与静音。提供 `new_disabled()` 降级实例以支持无音频设备环境。
- **ipc**：命名管道 IPC 通信层。`NamedPipeClient<T>` 泛型基类封装连接/读写/超时；`MpvIpcClient` 与 `WpProcIpcClient` 以薄封装形式实现 mpv JSON 协议与 wp-proc JSON 协议的命令构造与响应解析。提供 OOM 防护（1MB 单行上限）、`Backoff` 指数退避、总体 deadline（I01/I03/I04）等机制。
- **process**：子进程管理。`ProcessManager` 基于 `CreateProcessW` + Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）启动并管理 mpv / wp-proc 子进程，确保主进程异常退出时子进程被内核自动终止。提供 `stop()`（优雅退出，最长 8s）与 `stop_immediate()`（立即终止，毫秒级）双路径，以及 `escape_windows_arg` MSVCRT argv 转义。

### 1.2 文件清单

#### audio 子模块
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 1 | 仅 `pub mod volume;` |
| volume.rs | 1030 | `VolumeControl` 实现 + `is_pid_running` / `collect_stale_pids` 辅助 + 19 个测试 |

#### ipc 子模块
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 32 | 模块级 `//!` 文档（含超时对照表 I-006）+ 子模块声明 + `pub use NamedPipeIpcClient` |
| client.rs | 1086 | `NamedPipeClient<T>` 泛型基类 + `NamedPipeIpcClient` trait + `connect_named_pipe` / `read_line_with_limit` / `read_line_with_timeout` / `Backoff` + 30+ 测试 |
| mpv_protocol.rs | 414 | `MpvIpcClient` + `MpvCommand` / `MpvResponse` / `MpvEvent` + 14 测试 |
| wp_proc.rs | 713 | `WpProcIpcClient` + `WpProcCommand` / `WpProcResponse` / `ResponseStatus` + 18 测试（含 I04 真实管道超时测试） |

#### process 子模块
| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 1 | 仅 `pub mod manager;` |
| manager.rs | 1156 | `ProcessManager` 实现 + `escape_windows_arg` MSVCRT 转义 + `wait_and_terminate` / `terminate_and_wait` 共享逻辑 + 18 测试 |

#### 集成测试
| 文件 | 行数 | 主要内容 |
|---|---|---|
| crates/mirrorstar-core/tests/ipc_timeout.rs | 239 | I01/I03 总体超时机制的真实命名管道集成测试（3 用例） |

### 1.3 测试覆盖

- **audio**：19 个单元测试，覆盖 `new_disabled` 降级路径、`set_process_volume` clamp、`is_pid_running` / `collect_stale_pids` 辅助函数、v41-A-001 COM 恢复路径。**核心 WASAPI 交互路径（`with_session` cache hit/miss + 真实会话枚举）仅在 `#[ignore]` 真机测试中覆盖**（`v41_a003_real_com_cache_hit_miss_e2e`），CI 中无端到端验证。
- **ipc**：30+ 单元测试覆盖 `Backoff` 序列、`read_line_with_limit` OOM 防护与 I-001 / v41-I-001 UTF-8 截断处理；3 个集成测试（`tests/ipc_timeout.rs`）通过真实命名管道验证 I01/I03 总体超时；`wp_proc.rs` 内嵌 2 个 I04 真实管道超时测试。覆盖较完整。
- **process**：18 个测试覆盖 `escape_windows_arg`（含 N-009/P-005 拒绝换行/NUL）、P01 STILL_ACTIVE 误判修复、P03 pid() 退出后返回 None、P-001 句柄泄漏量化测试。**两个关键量化测试（`v41_p001_start_does_not_leak_handles` / `v41_p003_start_after_stop_immediate_does_not_reuse_pid`）标 `#[ignore]`，CI 永不执行**。

## 2. 技术债清单

### 2.1 audio 子模块

#### 2.1.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-001 | volume.rs:7-9, 18-22, 129-135 | `device_enumerator: RefCell<Option<IMMDeviceEnumerator>>` 字段及其 `refresh_session_manager` 主动调用入口在生产代码中从未被外部调用（仅 `with_session` 失败路径内部触发一次重试，且 v4.0 A-001 finding 已记录此点）。注释自承"未实现 IMMNotificationClient 主动设备变更监听"，字段占用内存但不产生主动监听效果。 | 死字段，给人"设备变更已处理"错觉。v4.0 A-001 finding 已记录但选择"方案 B 惰性重试 + 保留字段为未来方案 A 留余地"折衷，技术债持续未清理。 | 二选一：(a) 移除 `device_enumerator` 字段、`try_recover_device_enumerator` 方法与相关注释，明确为纯惰性重试方案；(b) 真正实现 `IMMNotificationClient` 主动监听。考虑到 v41-A-001 已实现 COM 重新初始化恢复路径，方案 (a) 更现实。 | 中 |

#### 2.1.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-002 | volume.rs:489-504, 295-304 | `collect_stale_pids` 私有函数仅被 `with_session_once` 一处调用（L297），抽出为独立 `fn` 仅为可测试性。函数体是 `cached_pids.iter().filter(|&&pid| !is_pid_running(pid)).copied().collect()` 的薄封装，且配套 6 个测试用例（L640-727）。 | 抽象层薄弱，独立函数 + 6 个测试覆盖一个 4 行闭包逻辑，测试维护成本高于逻辑本身。 | 评估后保留：测试可测试性收益可能高于冗余成本；若清理，可内联回 `with_session_once` 并删除 `test_collect_stale_pids_*` 系列测试，仅保留 `is_pid_running` 测试。 | 低 |

#### 2.1.3 重复实现

> audio 子模块未发现明显的跨文件重复实现。

#### 2.1.4 过时模式

> audio 子模块未发现明显的过时模式（与同类模块 `process::manager::is_running` 使用不同机制查询进程状态，但场景不同：audio 按 PID 查询、process 按句柄查询，属合理的功能并行而非过时）。

#### 2.1.5 未使用导入

> audio 子模块 `use` 语句均被使用，无未使用导入。

#### 2.1.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-003 | volume.rs:34-56 | `session_cache` 字段定义下方嵌入约 23 行「Drop 行为契约（v41-A-004）」注释段落，包含「未来如需改进」的示例代码（`impl Drop for VolumeControl { fn drop(&mut self) { self.session_cache.clear(); } }`，标 `ignore`），把"未来可能改动"的笔记嵌入字段文档。 | 核心字段定义被大段未来设想淹没，新读者难以快速识别字段本质职责。 | 将"Drop 契约"段落迁移至 `unsafe impl Send` 上方已有的 SAFETY 论证块（L59-97 已包含"Drop 契约（A-004）"段），删除字段定义下方的重复段，仅保留 1-2 行指针性注释。 | 低 |

#### 2.1.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-004 | volume.rs:7-9, 18-22, 34-56, 89-96, 126-128, 137-146, 172-177, 199-208, 219-225, 248-255, 273-294, 323-327, 343-349, 474-482 | 全文件散布「A-001 方案 B」「A02」「A03」「A-002」「A-004」「A-005」「C-103/C-026」「T-005」「T-008」「v41-A-001」「v41-A-002」「v41-A-003」「v41-A-004」「v41-A-005」共 14 种历史标记，覆盖字段定义、方法文档、内联注释三层。许多注释采用「原实现 X → 修复后 Y」对比叙事。 | 代码视觉密度过高，新读者需穿越历史叙事理解当前实现；多轮修复标记使"为什么这样写"的当代解释被淹没在"过去是什么样"中。 | 短期：保留标记但删除「原实现 X」对比段（只留当前实现说明）；中期：在 v6.0 后整理一轮，将历史标记替换为指向 v4.0/v5.0 finding 文档的链接（`// See: docs/优化文档/05-audio-ipc-process模块.md#A-001`）。 | 中 |

#### 2.1.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-005 | volume.rs:273, 292, 323, 327, 343, 474 | 修复批次标记前缀混用：`A01` / `A02` / `A03`（无前导零、无连字符）与 `A-001` / `A-002` / `A-003` / `A-004` / `A-005`（连字符 + 三位数字）与 `v41-A-001` / `v41-A-002` / `v41-A-003`（带版本前缀）三种格式在同一文件内并存，分别对应 v3.x / v4.0 / v4.1 三轮修复。 | 同类概念（修复批次标识）命名不一致，读者难以判断 `A01` 与 `A-001` 是否同一修复。 | 统一为 `A-001` 格式（连字符 + 三位数字），删除版本前缀 `v41-`（v6.0 后所有修复都已稳定）。或全部替换为指向 finding 文档的链接。 | 低 |

#### 2.1.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| A-TD-006 | volume.rs:7-9 | 文件顶部 TODO 注释："未实现 IMMNotificationClient 主动设备变更监听，当前为惰性重试方案"。但 v41-A-001 修复后 `refresh_session_manager` 已具备 COM 重新初始化 + 设备重连重试能力（L147-170），不再是单纯的"惰性重试"。注释未反映此恢复路径。 | 读者误以为设备切换完全无自愈能力，低估当前实现。 | 更新注释为："未实现 IMMNotificationClient 主动监听；v41-A-001 后 `refresh_session_manager` 在 WASAPI 调用失败时尝试 COM 重新初始化 + 设备重连，作为惰性自愈方案。" | 低 |

### 2.2 ipc 子模块

#### 2.2.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| I-TD-001 | client.rs:135-137 | `NamedPipeClient::read_response_line(&mut self)`（默认 5s 超时版本）仅在 mod.rs:18 的文档表格中被引用，无任何生产或测试调用方。所有调用方（mpv_protocol.rs:127、wp_proc.rs:134、tests/ipc_timeout.rs:127/217）均使用 `read_response_line_with_timeout(...)`。方法注释自承「向后兼容入口」（L134）。 | 死方法占公共 API 表面，且文档表格引用使其看似活跃。 | 移除 `read_response_line` 方法；同步更新 mod.rs:18 与 client.rs:9-15 超时对照表，删除该行。 | 低 |
| I-TD-002 | client.rs:34-42, 188-200; mod.rs:31 | `NamedPipeIpcClient` trait 公共接口仅 3 方法（`pipe_path`/`connect`/`disconnect`），被 `NamedPipeClient<T>`、`MpvIpcClient`、`WpProcIpcClient` 实现，但**全仓库无任何 `dyn NamedPipeIpcClient` 或 `T: NamedPipeIpcClient` 形式的多态使用**（已 Grep 验证）。每次调用都通过具体类型的 inherent method。`pub use client::NamedPipeIpcClient;`（mod.rs:31）的重导出也未被外部使用。 | Trait + 3 个 impl 块 + pub use 重导出共约 50 行死代码，给人"存在多态抽象"的错误印象。 | 移除 `NamedPipeIpcClient` trait、3 个 impl 块、`pub use` 重导出。各客户端的 inherent 方法（`pipe_path`/`connect`/`disconnect`）已是公共 API，调用方无需 trait 即可使用。 | 中 |
| I-TD-003 | client.rs:309-316 | `Backoff::current()` 与 `Backoff::reset()` 方法仅被自身单元测试调用（client.rs:599, 636-638），生产代码无任何调用。`read_line_with_timeout`（L471）只用 `Backoff::default()` + `next_delay()`。 | 两个方法 + 配套测试（`backoff_reset_restores_initial_value` 等）属测试驱动出的死 API。 | 评估后删除：移除 `current()` 与 `reset()` 方法及其测试；若保留为公共 API 则添加 `#[allow(dead_code)]` 并注明"预留用于未来重置场景"。 | 低 |
| I-TD-004 | mpv_protocol.rs:38-44 | `MpvEvent` 结构体已加 `#[allow(dead_code)]` 自承是死代码（"预留用于未来 mpv 事件处理"）。v4.0 I-004 finding 已记录此问题并选择保留。当前是 v4.0 已覆盖项的延续，未发生新调用也未删除。 | v4.0 评估后保留的"未来扩展预留"，但 v5.0/v6.0 仍未使用，持续占代码体积。 | 评估后决定：若 v6.0 仍无 mpv 事件处理需求，移除 `MpvEvent` 结构体 + 2 个反序列化测试（mpv_event_property_change / mpv_event_end_file）。 | 低 |
| I-TD-005 | mpv_protocol.rs:81-83 | `MpvIpcClient::send_command(&mut self, command: &[&str])`（默认 5s 超时版本）注释自承「向后兼容入口」（L77）。全仓库生产代码（`wallpaper/video.rs:174, 257, 269, 314, 327, 363, 391, 427, 439, 594` 等）均使用 `pause()`/`resume()`/`quit()`/`set_speed()`/`get_property()`/`disconnect()` 等具体方法（间接通过 `send_command_with_timeout` 或 `send_command_no_wait`），**无任何外部直接调用 `send_command`**（已 Grep 验证 `.send_command\b` 在 `wallpaper/` 与 `src-tauri/` 无匹配）。 | 死方法占公共 API 表面。wp_proc 侧 `WpProcIpcClient::send_command`（wp_proc.rs:104-109）被 `play` 调用一次，但 mpv 侧完全无调用方，两个同名方法行为不一致（mpv 5s / wp-proc 15s）。 | 移除 `MpvIpcClient::send_command`；调用方若有需要应显式选择 `send_command_with_timeout`（指定超时）或 `send_command_no_wait`。 | 低 |

#### 2.2.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| I-TD-006 | client.rs:225-256 | `pub fn connect_named_pipe(...)` 公共自由函数仅被 `NamedPipeClient::connect` 内部调用一次（L84）。`pub` 可见性不必要，函数从未被外部直接调用（已 Grep 验证 `connect_named_pipe` 在 `crates/` 与 `src-tauri/` 仅 3 处匹配：1 处文档、1 处定义、1 处内部调用）。 | 公共 API 表面被未使用的入口污染，外部可绕过 `NamedPipeClient::connect` 直接调用 `connect_named_pipe`，破坏封装。 | 将 `pub fn connect_named_pipe` 改为 `pub(crate) fn` 或私有 `fn`；保留为 `pub(crate)` 便于 `tests/ipc_timeout.rs` 等内部测试使用（实际测试通过 `NamedPipeClient::connect` 间接调用，可直接私有）。 | 低 |

#### 2.2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| I-TD-007 | mpv_protocol.rs:251-267; wp_proc.rs:258-274 | `impl NamedPipeIpcClient for MpvIpcClient` 与 `impl NamedPipeIpcClient for WpProcIpcClient` 两块代码结构完全相同：3 方法（`pipe_path`/`connect`/`disconnect`）全部委托给 inherent method。共约 30 行样板代码。 | 与 I-TD-002 关联：trait 本身是冗余抽象，这两块 impl 是配套的重复样板。 | 与 I-TD-002 一并清理：移除 trait 后这两块 impl 自动消失。 | 低 |
| I-TD-008 | mpv_protocol.rs:275-279; wp_proc.rs:282-286 | `impl Drop for MpvIpcClient` 与 `impl Drop for WpProcIpcClient` 结构完全相同（`fn drop(&mut self) { self.disconnect(); }`），仅 `tracing::info!` 日志消息不同（"mpv IPC" vs "wp-proc IPC"）。两处上方均有相同的「I-007：Drop 双重 disconnect 幂等性说明」注释。 | 样板代码重复，注释也复制两份。 | 与 I-TD-002 一并清理：若移除外层 Drop，将断开日志合并到 `NamedPipeClient::disconnect` 内（需让 `NamedPipeClient` 持有协议名称字符串）；或保留现状但提取注释到共享位置。 | 中 |
| I-TD-009 | mpv_protocol.rs:81-83, 95-144, 155-168; wp_proc.rs:104-109, 118-153, 168-176 | 两个客户端的「向后兼容入口方法」+ `send_command_with_timeout` + `send_command_no_wait` 三组方法结构高度相似：构造命令 → 序列化 → send_line → 循环读取匹配 request_id 的响应。差异仅在命令类型（`&[&str]` vs `WpProcCommand`）与响应类型。 | 「薄封装」模式合理但样板代码可减少。 | 评估后决定：可考虑用宏 `define_ipc_client!` 生成公共循环逻辑；或保持现状接受重复（当前重复程度可接受，重构收益有限）。 | 高 |

#### 2.2.4 过时模式

> ipc 子模块未发现明显的过时模式（v4.0/v5.0 修复已同步到 mpv 与 wp-proc 两侧）。

#### 2.2.5 未使用导入

> ipc 子模块 `use` 语句均被使用。`pub use client::NamedPipeIpcClient;`（mod.rs:31）的重导出未使用，归入 I-TD-002 一并处理。

#### 2.2.6 过度设计

> ipc 子模块的 `Backoff` 结构（含 `initial`/`max`/`current` 三字段）相对其使用场景（仅 `read_line_with_timeout` 一处用 `Backoff::default()` + `next_delay()`）略重，但属合理的可配置设计，不构成过度设计。

#### 2.2.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| I-TD-010 | client.rs:117-128 | `send_line` 方法注释含「v5.0 I-PERF-011: 分两次 write_all（json + `\n`）替代 `format!` 分配新 String」标记，描述 v5.0 性能优化的实现细节。注释与代码一致但属 v5.0 修复痕迹。 | 注释准确但属历史叙事，新读者需理解"为什么不用 format!"的历史背景。 | 简化注释为「分两次 write_all 避免 format! 分配」，删除 `v5.0 I-PERF-011` 标记。 | 低 |
| I-TD-011 | client.rs:325-389, 463-560 | `read_line_with_limit` 与 `read_line_with_timeout` 含大量历史标记：「I-001」「I02」「v41-I-001」「C-113」「SEC-003」「I-002」「I-003」，描述「原实现 X → 现实现 Y」对比。`read_line_with_limit` 单函数注释段长约 65 行（L325-389），超过函数体本身。 | 函数注释密度过高，核心逻辑被历史叙事淹没。 | 保留当前实现的说明（如 total_avail 限制、UTF-8 截断处理、OOM 防护），删除「原实现 X」对比段；将历史背景链接到 v4.0 finding 文档。 | 中 |
| I-TD-012 | mpv_protocol.rs:269-279; wp_proc.rs:276-286 | 两处相同的「I-007：Drop 双重 disconnect 幂等性说明」注释，明确说"第二次调用是安全 no-op；保留外层 Drop 是为记录 tracing::info! 断开日志"。这是 v4.0 I-007 修复后的妥协方案留下的痕迹，注释本身承认这是冗余的双重清理。 | 注释承认设计冗余但选择保留，是典型的修复痕迹。 | 与 I-TD-008 一并处理。 | 低 |
| I-TD-013 | mpv_protocol.rs:5, 101, 157 | 3 处 `// v5.0 I-PERF-008: 直接序列化结构体，跳过 json! 宏的中间 Value 树分配` 注释，描述同一 v5.0 性能优化。 | 三处重复同一历史标记。 | 在 `MpvCommand` 结构体定义处（L6-10）保留一处完整说明，删除内联注释中的重复标记。 | 低 |
| I-TD-014 | wp_proc.rs:485-487, 519, 538-539, 619-620, 637 | 测试代码内含「windows 0.58 返回 HANDLE」「HANDLE 内部为 *mut c_void 未实现 Send」「主线程完成 HANDLE → File 转换」等版本特定注释，详细解释 `HANDLE.0 as RawHandle` 转换。属适配 windows 0.58 API 变化的修复痕迹。 | 注释准确但与当前 windows 版本应已对齐，过度详细的版本特定解释增加阅读负担。 | 简化为单处说明（如 `// HANDLE → File 转换以实现 Send，详见 spawn_silent_pipe_server`），删除多处重复的版本说明。 | 低 |

#### 2.2.8 命名一致性

> ipc 子模块命名一致性较好，`send_command` / `send_command_with_timeout` / `send_command_no_wait` 三组方法在 mpv 与 wp-proc 两侧命名一致。

#### 2.2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| I-TD-015 | client.rs:9-15; ipc/mod.rs:12-25 | 两处「超时对照表」内容重叠且列结构不同：`client.rs:9-15` 是 4 列表（协议/命令超时/连接重试间隔/备注），`ipc/mod.rs:12-25` 是 5 列表（入口/默认值/用途/使用场景/位置）。`client.rs:14` 写 `wp-proc | 15s | 100ms` 将"命令超时 15s"与"连接重试间隔 100ms"并列，但 wp-proc 客户端的连接重试间隔并非 100ms 固定值（由调用方传入），表格准确性存疑。 | 两处描述同一信息但格式不一致，且 `client.rs` 版本含误导性数据。 | 删除 `client.rs:9-15` 的简化表格，统一引用 `ipc/mod.rs:12-25` 的权威表格；或在 `client.rs` 顶部改为 `// 超时对照表见 ipc/mod.rs`。 | 低 |

### 2.3 process 子模块

#### 2.3.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-001 | manager.rs:1032-1069, 1097-1155 | `v41_p001_start_does_not_leak_handles` 与 `v41_p003_start_after_stop_immediate_does_not_reuse_pid` 两个测试均标 `#[ignore]`，CI 中永不执行。前者量化验证 P-001 修复（100 轮循环 + 句柄数差值 ≤ 5），后者验证 v41-P-003（PID 不复用）。 | 永不执行的测试代码（约 120 行），且文档注明需本地手动运行（`cargo test -- --ignored ...`）。属"潜在死代码"。 | 评估后决定：(a) 移除两个 `#[ignore]` 测试，依赖 `p001_start_after_exit_no_handle_leak`（10 轮 + CI 可执行）覆盖 P-001；(b) 在 CI 中增加 `--ignored` 作业运行它们（需 Windows 真机 + 句柄计数稳定性）。 | 中 |

#### 2.3.2 冗余抽象

> process 子模块未发现明显的冗余抽象。`wait_and_terminate` / `terminate_and_wait` 的层级提取（P05）已被 `stop` / `stop_handle` 两处共享，是合理的去重。

#### 2.3.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-002 | manager.rs:944-1017, 1032-1069 | `p001_start_after_exit_no_handle_leak`（L944，10 轮 + 阈值 8）与 `v41_p001_start_does_not_leak_handles`（L1032，100 轮 + 阈值 5）覆盖同一 P-001 修复路径，逻辑高度重叠（启动 cmd → 等待退出 → 下一轮 → 监测句柄数）。后者是前者的量化加强版。 | 两个测试覆盖同一修复路径，存在重复。 | 评估后保留一个：建议保留 `p001_start_after_exit_no_handle_leak`（CI 可执行）并删除 `v41_p001_start_does_not_leak_handles`（`#[ignore]` 永不执行）；或合并为一个参数化测试。 | 低 |

#### 2.3.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-003 | manager.rs:880-902 | `stop_handle_delegates_to_wait_and_terminate` 测试通过 `pm.process_handle.take()` 取走句柄后调用 `stop_handle`，但需手动 `pm.pid = None` 避免重复关闭（L900-901）。这是 v4.0 P05 提取 `wait_and_terminate` 共享逻辑时的验证方式，暴露了 `stop_handle` 作为私有 helper 在测试中直接调用时的内部状态管理脆弱性。 | 测试需手动清理被测对象的内部状态，模式过时（应通过公共 API 测试，而非绕过封装操作私有字段）。 | 评估后保留：`stop_handle` 是 private fn，测试需访问内部字段是 Rust 测试常见模式；若重构，可让 `stop_handle` 不依赖 `self` 状态（已如此，签名是 `fn stop_handle(handle: HANDLE, pid: Option<u32>)`），测试无需 `pm.pid = None` 清理。 | 低 |

#### 2.3.5 未使用导入

> process 子模块 `use` 语句均被使用。

#### 2.3.6 过度设计

> process 子模块未发现明显的过度设计。

#### 2.3.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-004 | manager.rs:80-97 | `start()` 方法开头注释混合 4 个不同修复的解释：P-001（句柄泄漏）、P-003（用 stop_immediate 替代 stop）、v41-P-003（50ms sleep）、P-006（移除 CREATE_UNICODE_ENVIRONMENT）。注释段落长（约 17 行），且采用「原实现 X → 修复后 Y」对比叙事。 | `start()` 核心逻辑（约 130 行）被 17 行历史叙事包围，新读者难以快速识别当前实现要点。 | 保留当前实现说明（如"无条件清理旧句柄"+"50ms sleep 等 OS 回收 PID"），删除「原实现 X」对比段；将历史背景链接到 v4.0 finding 文档。 | 低 |
| P-TD-005 | manager.rs:215-233, 257-290 | `stop()` 与 `stop_immediate()` 的文档注释各含「## 调用方决策标准 (v41-P-002)」段落，互相引用对方，重复描述决策矩阵。两段文档信息冗余约 20 行。 | 同一决策矩阵在两处描述，维护时易不同步。 | 将决策矩阵集中到 `stop_immediate` 文档（作为主要决策点），`stop` 文档简化为「正常关闭，详见 `stop_immediate` 的决策标准」。 | 低 |
| P-TD-006 | manager.rs:32-60 | `unsafe impl Send/Sync` 的 SAFETY 论证注释段落长（约 28 行），含「Task 9.1.2 soundness 论证」标记，详细论证 `HANDLE` 的跨线程安全性。论证本身必要，但段落较长且混合了 Send / !Sync / Drop 契约三类论证。 | 单块 SAFETY 注释过长，混合多个论证维度。 | 拆分为三个独立 `// SAFETY:` 块：Send 论证、!Sync 原因、Drop 契约引用。或保留现状（单块更易于完整阅读）。 | 低 |
| P-TD-007 | manager.rs:817-831 | `wait_and_terminate_handles_wait_failed` 测试文档含「演进历史」段落，描述「P02 修复前 → P02 修复后 → P-007 优化」三阶段。这是测试文档中的修复痕迹，记录了同一段代码的多轮修复。 | 测试文档包含历史叙事，新读者需穿越三阶段演进理解当前测试目的。 | 简化为「验证 WAIT_FAILED 时记录警告并跳过 terminate_and_wait（P-007）」，删除 P02 修复前/后的对比段。 | 低 |

#### 2.3.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-008 | manager.rs:745, 786, 833, 848, 880, 912, 944, 971, 1034, 1099 | 测试命名风格不一致：部分测试带修复批次前缀（`p001_start_after_exit_no_handle_leak`、`v41_p001_start_does_not_leak_handles`、`v41_p003_start_after_stop_immediate_does_not_reuse_pid`），部分不带前缀（`is_running_returns_false_for_exit_code_259`、`stop_delegates_to_wait_and_terminate`、`wait_and_terminate_handles_wait_failed`、`pid_returns_none_after_process_exits`）。前缀格式也不统一：`p001`（小写无连字符）vs `v41_p001`（带版本前缀）。 | 测试命名风格混杂，难以从名称判断测试对应的修复批次。 | 统一为不带修复批次前缀的描述性命名（如 `start_after_exit_does_not_leak_handles`），修复批次信息移至测试文档注释。或全部统一为带 `vXX-YYY_` 前缀。 | 低 |

#### 2.3.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| P-TD-009 | manager.rs:269, 1085 | `stop_immediate` 文档注释（L269）与测试注释（L1085）均提到「调用 `wait_for_exit(100ms)` 兜底」，但**全仓库无 `wait_for_exit` 方法的定义或调用**（已 Grep 验证 `wait_for_exit` 仅 2 处匹配，均为注释引用）。这是 v4.0 P-003 修复时设想的 API 但未实现。 | 注释引用了不存在的方法，读者误以为存在 `wait_for_exit` 兜底入口，实际需调用方自行 `std::thread::sleep(100ms)`。 | 更新注释：将「调用 `wait_for_exit(100ms)` 兜底」改为「调用方需自行 `std::thread::sleep(100ms)` 或确保已过 100ms」；或实现 `wait_for_exit` 方法以兑现文档承诺。 | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0）

低风险高收益，可在 Wave v6-A 一次性清理：

| ID | 子模块 | 清理动作 | 预估工作量 |
|---|---|---|---|
| I-TD-001 | ipc | 移除 `read_response_line`（默认 5s）死方法 + 同步更新超时对照表 | 30 min |
| I-TD-003 | ipc | 移除 `Backoff::current` / `Backoff::reset` 死方法及测试 | 30 min |
| I-TD-005 | ipc | 移除 `MpvIpcClient::send_command`（默认 5s）死方法 | 30 min |
| I-TD-006 | ipc | `connect_named_pipe` 由 `pub` 改 `pub(crate)` 或私有 | 15 min |
| I-TD-010 | ipc | 简化 `send_line` 的 `v5.0 I-PERF-011` 注释 | 5 min |
| I-TD-013 | ipc | 合并 `MpvCommand` 的 3 处 `v5.0 I-PERF-008` 注释为 1 处 | 10 min |
| I-TD-015 | ipc | 删除 `client.rs:9-15` 重复超时表，统一引用 `mod.rs` | 10 min |
| A-TD-005 | audio | 统一修复批次标记前缀为 `A-001` 格式 | 30 min |
| A-TD-006 | audio | 更新 `volume.rs:7-9` TODO 注释反映 v41-A-001 恢复路径 | 10 min |
| P-TD-004 | process | 简化 `start()` 开头注释，删除「原实现 X」对比段 | 20 min |
| P-TD-005 | process | 合并 `stop` / `stop_immediate` 重复的决策矩阵文档 | 20 min |
| P-TD-007 | process | 简化 `wait_and_terminate_handles_wait_failed` 测试文档 | 10 min |
| P-TD-009 | process | 修正 `wait_for_exit` 陈旧注释引用 | 10 min |
| P-TD-008 | process | 统一测试命名风格 | 30 min |

### 3.2 谨慎清理（P1/P2）

需评估影响或涉及多文件改动：

| ID | 子模块 | 清理动作 | 预估工作量 | 风险 |
|---|---|---|---|---|
| I-TD-002 + I-TD-007 + I-TD-008 | ipc | 移除 `NamedPipeIpcClient` trait + 3 个 impl 块 + `pub use` + 关联 Drop 重复 | 2 h | 中（影响公共 API 表面） |
| I-TD-004 | ipc | 评估并移除 `MpvEvent` 死结构体 | 30 min | 低（仅 mpv 侧） |
| A-TD-001 | audio | 二选一：移除 `device_enumerator` 字段或真正实现 IMMNotificationClient | 1 day | 中（影响 v41-A-001 恢复路径） |
| A-TD-003 | audio | 迁移 `session_cache` 字段下的「Drop 契约」段到 SAFETY 块 | 30 min | 低 |
| P-TD-001 + P-TD-002 | process | 评估 `#[ignore]` 测试去留，合并重复的 P-001 句柄泄漏测试 | 1 h | 低 |

### 3.3 评估后决定（P3）

长期或低收益，可在 Wave v6-D 择机处理：

| ID | 子模块 | 清理动作 | 备注 |
|---|---|---|---|
| A-TD-002 | audio | `collect_stale_pids` 内联回 `with_session_once` | 测试可测试性收益可能高于冗余成本 |
| A-TD-004 | audio | 整理 volume.rs 全文件历史标记（14 种） | 中等工作量，收益分散 |
| I-TD-008 | ipc | `Drop` 重复实现统一 | 与 I-TD-002 关联，依赖 trait 清理 |
| I-TD-009 | ipc | `send_command_*` 三组方法的宏提取 | 重构成本高，当前重复可接受 |
| I-TD-011 | ipc | `read_line_with_limit` / `read_line_with_timeout` 注释瘦身 | 涉及核心 IPC 逻辑，谨慎 |
| I-TD-014 | ipc | 简化 wp_proc.rs 测试的 windows 0.58 版本注释 | 注释准确，仅冗余 |
| P-TD-003 | process | `stop_handle_delegates_to_wait_and_terminate` 测试模式优化 | Rust 测试常见模式 |
| P-TD-006 | process | 拆分 `unsafe impl Send/Sync` SAFETY 注释 | 单块更易完整阅读，可不拆 |

## 4. 优化机会

### 4.1 统一 IPC 客户端抽象层

`MpvIpcClient` 与 `WpProcIpcClient` 高度同构（newtype 持有 `NamedPipeClient<T>` + 薄封装方法 + Drop 日志），当前通过 trait + 重复 impl 实现"统一接口"（I-TD-002）。可考虑：
- 移除 trait 后，让两个客户端直接持有 `NamedPipeClient<T>`，公共方法（`pipe_path`/`connect`/`disconnect`）由 `NamedPipeClient` inherent method 提供，客户端仅添加协议特定方法。
- 或用宏 `define_ipc_client!(MpvIpcClient, MpvClient)` 生成样板代码。

### 4.2 超时常量集中化

当前超时默认值分散在 4 处（`client.rs:136` 5s、`wp_proc.rs:12` 15s、`mpv_protocol.rs:82` 5s、`mpv_protocol.rs:175` 2s），v4.0 I-006 选择"文档对照表"方案而非集中常量。v6.0 可评估将常量集中到 `ipc/mod.rs` 的 `pub mod timeouts` 子模块，便于跨文件引用与调整。

### 4.3 修复批次标记统一化

三子模块均存在修复批次标记前缀混用问题（audio: `A01`/`A-001`/`v41-A-001`；ipc: `I-001`/`I02`/`v41-I-001`；process: `P-001`/`p001`/`v41-P-001`）。可在 v6.0 后整理一轮，统一为 `<MODULE>-<NNN>` 格式（如 `A-001`、`I-001`、`P-001`），删除版本前缀 `v41-`，将历史背景链接到 finding 文档。

### 4.4 `#[ignore]` 测试策略

process 子模块的 2 个 `#[ignore]` 测试（P-TD-001）从未在 CI 中执行。可在 CI 中增加 `cargo test -- --ignored` 作业（限定 Windows 真机 + 句柄计数稳定性），或承认这些测试为文档化样例并移除。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项（引用 docs/优化文档/05-audio-ipc-process模块.md 中的 findings ID）

以下 v6 发现与 v4.0 findings 关联，v4.0 已识别并选择保留/修复，v6 仅记录其作为技术债的当前状态：

| v6 ID | v4.0 finding ID | v4.0 决策 | v6 状态 |
|---|---|---|---|
| A-TD-001 | A-001（High 架构设计） | 方案 B 惰性重试 + 保留 `device_enumerator` 字段为未来方案 A 留余地 | 字段仍未使用，技术债持续 |
| I-TD-004 | I-004（Low 可维护性） | 保留 `MpvEvent` + `#[allow(dead_code)]` | 仍未使用，技术债持续 |
| I-TD-012 | I-007（Low 资源管理） | 保留外层 Drop + 注释说明幂等性 | 注释承认冗余但保留 |
| I-TD-015 | I-006（Low 可维护性） | 在 `ipc/mod.rs` 添加超时对照表 | 表格重复且部分失准 |
| P-TD-009 | P-003（v41-P-003 修复） | 文档承诺 `wait_for_exit(100ms)` 兜底 | 方法从未实现，注释陈旧 |
| P-TD-001 | P-001（High 资源管理） + v41-P-001 | 修复 + 量化测试（`#[ignore]`） | 测试 CI 永不执行 |

### 5.2 v5.0 已覆盖项

以下 v6 发现与 v5.0 性能优化 findings 关联：

| v6 ID | v5.0 finding ID | 描述 |
|---|---|---|
| I-TD-010 | I-PERF-011 | `send_line` 分两次 `write_all` 替代 `format!`（client.rs:117-128） |
| I-TD-013 | I-PERF-008 | `MpvCommand` 直接序列化结构体替代 `json!` 宏（mpv_protocol.rs:6-10, 102-105, 158-161） |

v5.0 性能优化已应用，v6 仅记录其修复痕迹（注释中的历史标记）。

### 5.3 v6 新发现

以下为 v6.0 首次识别的技术债，未在 v4.0/v5.0 中记录：

- **audio**：A-TD-002（`collect_stale_pids` 冗余抽象）、A-TD-003（Drop 契约段嵌入字段定义）、A-TD-004（14 种历史标记散布）、A-TD-005（标记前缀混用）、A-TD-006（TODO 注释陈旧）
- **ipc**：I-TD-001（`read_response_line` 死方法）、I-TD-002（`NamedPipeIpcClient` trait 死抽象）、I-TD-003（`Backoff::current`/`reset` 死方法）、I-TD-005（`MpvIpcClient::send_command` 死方法）、I-TD-006（`connect_named_pipe` 可见性过宽）、I-TD-007/I-TD-008/I-TD-009（trait impl + Drop + send_command 三组重复）、I-TD-011（`read_line_with_limit` 注释密度过高）
- **process**：P-TD-002（P-001 句柄泄漏测试重复）、P-TD-004（`start()` 注释混合 4 个修复）、P-TD-005（`stop`/`stop_immediate` 决策矩阵重复）、P-TD-008（测试命名风格不一致）
