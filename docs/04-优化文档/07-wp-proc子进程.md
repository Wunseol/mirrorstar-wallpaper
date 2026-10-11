# wp-proc 子进程模块优化文档

> [← 返回文档索引](../index.md)

## 1. 模块概览 / 现状

> 来源：v6.0 技术债审查（2026-07-25）| 模块路径：`crates/mirrorstar-wp-proc/src/`
> 增补：v8 深度审查（2026-10-11，见 §6）——校正 §1.1/§1.2/§1.3 失真条目

### 1.1 模块职责

wp-proc 是 MirrorStar 的 Web 壁纸子进程，由壳进程 `mirrorstar-wallpaper.exe` 的 `--wp-proc` 模式运行（同一 exe、双入口），将 WebView2 渲染隔离到独立进程，通过命名管道接收主进程的 IPC 命令（Play/Pause/Resume/Seek/SetPosition/Navigate/Terminate）。模块由 5 个源文件组成：

- **`lib.rs`** — 库入口（Cli 解析、COM/窗口类/窗口/WebView2/管道初始化、消息循环、IPC 线程协调、Drop 顺序契约、优雅退出）。注：该文件在 v6.0 文档中记为 `main.rs`，crate 重构为纯库 crate 后已更名为 `lib.rs`（不再产出独立 exe，由壳 exe `--wp-proc` 模式调用 `run_with_argv`）。
- **`command.rs`** — IPC 命令处理（`navigate_to_url`、`execute_script_and_report`、`handle_command`、`ok/error_response` 辅助、控制命令队列 `run_control_command`/`execute_control_command`/`flush_pending_control_commands`、`build_post_message_failed_response`、`NavigationCompletedHandlerGuard` RAII）
- **`com.rs`** — COM 初始化 RAII guard（STA 模式，`CoInitializeEx`/`CoUninitialize` 配对、`RPC_E_CHANGED_MODE` 处理、`is_initialized` 测试访问器）
- **`ipc_server.rs`** — 命名管道服务端（`OwnedHandle` RAII、`SecurityAttributesGuard`、`build_pipe_security_attributes`、`create_pipe_server`、`process_line`、`format_response`、`read_line_with_limit`、`notify_main_exit`、`ipc_thread`）
- **`webview.rs`** — WebView2 控制器封装（`parse_rect`、`default_rect`、`WindowClassGuard`、`register_window_class`、`create_window`、`ControllerGuard` RAII、`wait_with_pump_timeout`、`create_webview`、`build_webview2_user_data_folder`、`build_additional_browser_args`、`build_webview2_environment_options`、`apply_transparent_controller`、`harden_webview`、`install_crash_recovery`、`build_url`、`percent_encode_path`、`corewebview2_error`、URL 协议/域名白名单）

**核心结构**：作为壳进程 `mirrorstar-wallpaper.exe` 的 `--wp-proc` 模式运行（同一 exe、双入口，不产出独立 exe），通过 clap 接收 CLI 参数；COM 初始化为 STA（`COINIT_APARTMENTTHREADED`，WebView2 要求）；双线程架构（主线程 Win32 消息循环处理 WebView2，IPC 线程读取管道命令）；线程间通信 `std::sync::mpsc` + `PostMessageW(WM_WEB_COMMAND)`。

**设计模式**：`OwnedHandle` RAII、`read_line_with_limit` 限流读取、`ComGuard` RAII、`WindowClassGuard`/`ControllerGuard`/`SecurityAttributesGuard`/`NavigationCompletedHandlerGuard` RAII、`wait_with_pump_timeout` 超时泵（替代无限阻塞的 `GetMessageA`）、URL 协议白名单（`eq_ignore_ascii_case` 阻断大小写变体）、WebView2 创建失败退出（`return Err` 触发 RAII Drop）、纯函数提取（`process_line`/`format_response`）、退出清理（`Controller.Close()` → drop cmd_rx → IPC 线程 join → RAII 逆序释放）。

### 1.2 文件清单

| 文件 | 行数 | 主要内容 |
|---|---|---|
| lib.rs | 491 | 库入口（Cli 解析、COM/窗口类/窗口/WebView2/管道初始化、消息循环、IPC 线程协调、Drop 顺序契约） |
| command.rs | 1525 | IPC 命令处理（navigate_to_url、execute_script_and_report、handle_command、ok/error_response 辅助、控制命令队列、build_post_message_failed_response、NavigationCompletedHandlerGuard RAII） |
| com.rs | 103 | ComGuard RAII（CoInitializeEx/CoUninitialize 配对、RPC_E_CHANGED_MODE 处理、is_initialized 测试访问器） |
| ipc_server.rs | 1471 | 命名管道服务端（OwnedHandle RAII、SecurityAttributesGuard、build_pipe_security_attributes、create_pipe_server、process_line、format_response、read_line_with_limit、notify_main_exit、ipc_thread） |
| webview.rs | 2205 | WebView2 控制（parse_rect、default_rect、WindowClassGuard、register_window_class、create_window、ControllerGuard RAII、wait_with_pump_timeout、create_webview、build_webview2_*、harden_webview、install_crash_recovery、build_url、percent_encode_path、corewebview2_error） |

> 校正说明（v8 深度审查，2026-10-11）：原表记 `main.rs` 441 / `command.rs` 1132 / `com.rs` 114 / `ipc_server.rs` 1469 / `webview.rs` 1274，为 v6.0 时期快照。`main.rs` 已更名为 `lib.rs`（纯库 crate 化），各行数已按当前代码更新（见 §6.5）。§2 / §3 中历史 finding 记录的 `main.rs:NNN` 行号为当时快照，保留不改写。

### 1.3 测试覆盖

测试分布（v8 深度审查实测）：lib.rs（14 个测试，全为 Cli 参数解析契约测试，含 W-009 分离 argv 边界测试）、command.rs（33 个测试，含 handle_command 路径测试、JSON 协议往返、WP-005 错误响应构造、v41-WP-005 RAII 语义测试，多数 controller 路径降级为响应构造测试）、com.rs（2 个测试，独立线程 COM 初始化验证）、ipc_server.rs（30 个测试，含 create_pipe_server 集成测试、process_line/format_response 纯函数测试、read_line_with_limit 限流测试、WP12 残留数据验证、v41-WP-008 RAII 语义测试）、webview.rs（49 个测试，含 parse_rect/build_url/percent_encode_path 纯函数测试、SEC-004/SEC-005 协议与域名白名单测试、WP-001 大小写不敏感测试、W-006 超时测试、task5_1 additional browser args 契约测试、WP-007 Unicode 消息函数 include_str! 断言、v41-WP-010 RAII 契约测试）。**合计 128 个 `#[test]`，0 个 `#[ignore]`，`cargo test -p mirrorstar-wp-proc` 全绿。** 涉及真实 WebView2/COM 环境的失败分支普遍通过 `include_str!` 模式断言源码标记验证，无法在 CI 中可靠触发真实失败分支。

> 校正说明（v8 深度审查，2026-10-11）：原 §1.3 记「main.rs 11 个测试 / command.rs ~765 行 / ipc_server.rs ~855 行 / webview.rs ~635 行」等为 v6.0 时期估算值，且文件名为 `main.rs`。上表已按当前代码实测更新（见 §6.5）。

---

## 2. v4.0 审查发现与修复状态（13 项，**全部 ✅ 已修复**）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/08-wp-proc.md`
> 严重级别分布：Critical 0 / High 3 / Medium 4 / Low 6
> 维度分布：逻辑 1 | 并发 1 | 资源 3 | 错误 2 | 安全 1 | 可维护性 5
>
> **修复进展**：WP-004~WP-013 已标注于 v4.0 Wave 2D / 2I / 3G 完成；WP-001 / WP-002 / WP-003 三项 High 经**本次代码核验确认已修复**（具体见各条）。
> 说明：下述既作为 v4 原始 findings 的保留记录，也逐条补充了真实代码中的修复状态。

### 审查重点说明

`mirrorstar-wp-proc` 子进程经过 v3.0→v3.5 共 5 轮修复后整体质量较高：OwnedHandle RAII（WP01）、限流读取（WP02）、URL 协议白名单大小写不敏感（WP-001/SEC-004）、管道 SDDL 安全描述符（WP-008）、create_webview 失败退出（WP03）、纯函数提取（WP-006）。v4.0 审查聚焦于遗留债务：Pause/Resume 命令处理代码重复（约 70 行）、create_webview 失败路径资源清理不完整、SetPosition 错误处理不一致、安全降级路径可观测性不足。**经核验，13 项全部已修复。**

### [WP-001] [High] [可维护性] command.rs:147-220, 221-294 — Pause/Resume 命令处理代码大量重复 ✅ 已修复

**描述**：`Pause` 与 `Resume` 两个 match 分支的代码结构几乎完全相同（各约 74 行），仅 JS 脚本字符串、日志消息、错误前缀三处不同。完全重复的逻辑包括：`controller.CoreWebView2()` 获取与错误响应构造、`mpsc::channel` 创建 + `ExecuteScriptCompletedHandler::create` 回调构造、`webview.ExecuteScript` 调用、`wait_with_pump_timeout` 调用 + `script_result` 匹配、最终 `WpProcResponse` 构造。这种重复违反 DRY 原则，未来若需修改 ExecuteScript 的错误处理逻辑，必须同步修改两处，容易遗漏导致行为不一致。

**修复状态**：✅ **已修复**（代码核验）——`execute_script_and_report` 辅助函数提取于 command.rs:173（参数化脚本、操作名、request_id），`Pause`/`Resume` 分支分别调用（command.rs:352 / :358），消除约 70 行重复。

**建议**：提取共享 helper 函数 `execute_script_and_report(controller, script, op_name, request_id) -> WpProcResponse`，将差异点（脚本、操作名）参数化。`Pause`/`Resume` 分支分别调用该 helper。

### [WP-002] [High] [资源管理] webview.rs:402-446 — create_webview 失败路径未显式调用 controller.Close() ✅ 已修复

**描述**：`create_webview` 在 controller 创建成功后，后续步骤若失败，`?` 提前返回 `Err`，`controller` 作为局部变量被 drop。windows-rs / webview2-com 的 COM 接口 `Drop` 仅调用 `Release()`（减少 COM 引用计数），**不**调用 `ICoreWebView2Controller::Close()`。`Close()` 是显式释放 WebView2 资源（包括关联的浏览器进程、渲染管线）的方法，与 `Release()` 语义不同。

受影响的失败路径（controller 已创建但 create_webview 返回 Err）：`SetIsVisible(true)` 失败、`CoreWebView2()` 失败、`build_url(source)` 失败、`Navigate(...)` 失败、`GetClientRect(hwnd, ...)` 失败、`SetBounds(rect)` 失败。由于 create_webview 返回 Err 后进程退出，系统会回收资源，故无永久泄漏；但这是不一致的资源清理模式。

**修复状态**：✅ **已修复**（代码核验）——`ControllerGuard`（webview.rs:368）RAII guard，`Drop` 时调用 `Close()`（webview.rs:402），`create_webview` 以 `ControllerGuard::new(controller)` 包装（webview.rs:602），成功路径 `into_inner()` 取出跳过 `Close()`（webview.rs:645）。与建议的方案一致。

**建议**：在 create_webview 的失败路径显式调用 `Close()`，或用 RAII guard（`ControllerGuard`）确保 `Close()` 被调用，成功返回时 `guard.into_inner()` 取出 controller，失败时 Drop 调用 `Close()`。

### [WP-003] [High] [错误处理] command.rs:118-127 — SetPosition 中 GetClientRect/SetBounds 失败仅 warn 但返回 Ok ✅ 已修复

**描述**：`SetPosition` 命令的错误处理存在不一致。`SetWindowPos` 失败返回 `Error`（WP06 修复），但 `GetClientRect`/`SetBounds` 失败仅 `tracing::warn!`，仍返回 `Ok`。两种情况都导致壁纸渲染异常，但父进程收到 `Ok` 误以为位置设置成功，不会重试或上报错误。边界不同步同样是严重故障。

**修复状态**：✅ **已修复**（代码核验）——command.rs:335 / :339 `GetClientRect`/`SetBounds` 失败时改为 `return error_response(request_id, ...)`（返回 `ResponseStatus::Error`），与 `SetWindowPos` 失败处理一致（command.rs:328）。

**建议**：`GetClientRect` 或 `SetBounds` 失败时返回 `ResponseStatus::Error`，让父进程感知边界同步失败并决定后续处理（如重试或上报 wallpaper-error）。

### [WP-004] [Medium] [安全/可观测性] ipc_server.rs:239-243 — build_pipe_security_attributes 失败时静默降级到默认安全描述符 ✅ 已修复

**描述**：`create_pipe_server` 调用 `build_pipe_security_attributes` 构建限制为当前用户 SID 的 SDDL 安全描述符。若该函数返回 `None`，代码静默回退到默认安全描述符，无 warn 日志。默认安全描述符下，管道的访问权限由系统默认 DACL 决定，可能允许同机其他用户连接。安全降级发生时无日志，运维无法感知管道使用了弱于预期的安全配置。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：在 `create_pipe_server` 降级路径记录 warn 日志，说明管道使用默认安全描述符（可能允许其他用户连接）。

### [WP-005] [Medium] [资源管理/安全] ipc_server.rs:114 — token_user 转换未校验 buffer 长度 ✅ 已修复

**描述**：`build_pipe_security_attributes` 中，将 `Vec<u8>` buffer 直接转换为 `TOKEN_USER` 引用时未校验长度。缺乏防御性校验：若 `GetTokenInformation` 实现行为异常，会导致 `token_user` 解引用读到 buffer 外的内存（越界读）。此外，`token_user.User.Sid` 的 `sid.0` 已检查 null，但未验证 SID 有效性（`IsValidSid`），后续 `ConvertSidToStringSidW(sid, ...)` 可能对无效 SID 产生未定义行为。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：添加防御性长度校验 `if buffer.len() < std::mem::size_of::<TOKEN_USER>() { return None; }`，并用 `IsValidSid` 验证 SID 有效性。

### [WP-006] [Medium] [逻辑] command.rs:20-33, 48-59, 135-146 — Play/Navigate 不等待导航完成即返回 Ok ✅ 已修复

**描述**：`navigate_to_url` 调用 `webview.Navigate(&url)` 后立即返回 `Ok(())`，`handle_command` 据此返回 `WpProcResponse { status: Ok }`。但 `Navigate` 是异步操作——仅触发导航流程并立即返回，导航是否真正完成（或失败）通过 `NavigationCompleted` 事件异步通知。导航失败时父进程已收到 `Ok` 误以为播放成功，用户看到空白窗口而父进程无法感知。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：方案 A（推荐）：注册 `NavigationCompletedEventHandler`，在导航完成（成功或失败）后再通过 channel 通知 `handle_command` 返回响应，需设置超时避免永久阻塞（可复用 `wait_with_pump_timeout`）。方案 B：在 `navigate_to_url` 注释和 IPC 协议文档中明确 `Play`/`Navigate` 返回 `Ok` 仅表示"导航已启动"，不保证内容已加载。

### [WP-007] [Medium] [可维护性] webview.rs:305, 313 — wait_with_pump_timeout 使用 ANSI 消息泵，与主循环 Unicode 版本不一致 ✅ 已修复

**描述**：主消息循环用 `GetMessageW`/`DispatchMessageW`（Unicode），窗口类用 `RegisterClassW`（Unicode），但 `wait_with_pump_timeout` 用 `PeekMessageA`/`DispatchMessageA`（ANSI）。当前处理的纯数字消息无功能差异，但混合使用违反 Win32 最佳实践，是可维护性隐患：未来若需处理涉及字符串的消息，开发者可能不会注意到版本差异。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：统一使用 `W` 版本（`PeekMessageW`/`DispatchMessageW`），与主消息循环和窗口类注册一致。

### [WP-008] [Low] [资源管理] main.rs:89-101 — std::process::exit(1) 跳过 RAII Drop（class_guard/_com_guard） ✅ 已修复

**描述**：`create_webview` 失败时，main.rs 调用 `std::process::exit(1)` 立即退出进程，跳过所有 RAII guard 的 Drop：`class_guard`（`UnregisterClassW` 未调用）、`_com_guard`（`CoUninitialize` 未调用）。`std::process::exit` 跳过 Drop 是反模式，若未来添加需要显式清理的资源，可能因 `exit` 跳过 Drop 而泄漏。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（将 `std::process::exit(1)` 改为 `return Err(e.into())`，触发 RAII Drop：`_com_guard.drop()` 调用 `CoUninitialize`、`class_guard.drop()` 调用 `UnregisterClassW`；行为等价：main 返回 Err 时 runtime 自动调用 `std::process::exit(1)`，父进程仍能通过非零退出码检测子进程死亡；代码核验确认 main.rs:71-141 多处 `return Err(e.into())`，main.rs:117-121 附 WP-008 注释说明 RAII Drop 触发链与行为等价性）

**建议**：若任务约束允许，考虑用 `return Err(e.into())` 替代 `std::process::exit(1)`，让 RAII guard 正确 drop。若任务约束必须用 `std::process::exit`，则在注释中持续标注此约束的影响范围。

### [WP-009] [Low] [错误处理] ipc_server.rs:511-519 — PostMessageW 失败路径的 format_response 回退字符串丢失 request_id ✅ 已修复

**描述**：`PostMessageW(WM_WEB_COMMAND)` 失败时，`format_response` 序列化失败（概率极低）时回退到硬编码字符串 `r#"{"request_id":0,"status":"error","error":"PostMessageW failed"}"#`，用 `request_id=0`，丢失了已提取的实际 `request_id`。客户端收到 `request_id=0` 的响应无法与原始请求匹配。影响极低，但这是错误处理路径的瑕疵。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（抽离 `build_post_message_failed_response(request_id: u64) -> String` 辅助函数，回退字符串动态拼接实际 `request_id`；新增 `format_response_fallback_preserves_request_id` 单元测试覆盖非零 `request_id=42` 场景）

**建议**：回退字符串动态拼接 `request_id`：`format!(r#"{{"request_id":{},"status":"error","error":"PostMessageW failed"}}"#, request_id)`。

### [WP-010] [Low] [可维护性] webview.rs:90-101 — default_rect 硬编码 1920x1080 回退尺寸 ✅ 已修复

**描述**：`default_rect` 在 `GetSystemMetrics` 返回非正值时回退到硬编码的 1920x1080，是多显示器场景下的魔法数。兜底值未提取为命名常量，可读性差且不易维护。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（提取 `FALLBACK_SCREEN_WIDTH: i32 = 1920` 与 `FALLBACK_SCREEN_HEIGHT: i32 = 1080` 模块级常量，`default_rect` 使用常量替代魔法数，补充注释说明多显示器场景"仅次于合理默认"，多显示器应通过 `--rect` 显式传入）

**建议**：提取为命名常量 `const FALLBACK_SCREEN_WIDTH: i32 = 1920;` / `const FALLBACK_SCREEN_HEIGHT: i32 = 1080;` 并补充注释说明选择依据。

### [WP-011] [Low] [可维护性] webview.rs:452-476 — percent_encode_path 手写实现，未使用标准 crate ✅ 已修复

**描述**：`percent_encode_path` 手写 percent-encoding 实现，保留字符集为 `A-Za-z0-9-_.~/:=@`。(1) 保留字符集非标准（RFC 3986 §2.3 的 unreserved 集合是 `A-Za-z0-9-_.~`，额外保留了 `/:=@`）；(2) 性能——每个非保留字符调用 `format!("%{:02X}", byte)` 分配新 String；(3) 未使用标准 `percent-encoding` crate。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（新增 `hex_upper(byte: u8) -> [u8; 2]` 辅助函数使用 `HEX_CHARS` 查表；`percent_encode_path` 改用 `String::with_capacity(path.len() * 3)` 预分配，逐字符 `push('%')` + `hex_upper(byte)` 替代 `format!`；保留字符集与原实现一致，未新增依赖；既有 4 个测试零修改通过）

**建议**：使用 `percent-encoding` crate（若已添加为依赖）或改进手写实现避免 `format!` 分配（用 `hex_upper` 函数直接 push 字符）。

### [WP-012] [Low] [可维护性] com.rs:43-46 — is_initialized 方法标记 #[allow(dead_code)]，生产代码不调用 ✅ 已修复

**描述**：`ComGuard::is_initialized` 方法标记 `#[allow(dead_code)]`。WP07 修复后，`ComGuard::new()` 返回 `Ok` 时 `initialized` 恒为 `true`（RPC_E_CHANGED_MODE 返回 `Err`）。`is_initialized()` 在生产代码中无调用者，仅为单元测试提供内部状态可观测性。`#[allow(dead_code)]` 掩盖了"此方法无实际用途"的事实。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（采纳方案 B：`#[allow(dead_code)]` 改为 `#[cfg(test)]`，生产构建通过条件编译排除 `is_initialized` 方法，避免 dead_code 警告；`com::tests` 2 个测试零修改通过）

**建议**：方案 A：移除方法，将测试改为通过行为验证。方案 B：保留并改为 `#[cfg(test)]` 限定仅在测试编译时存在。

### [WP-013] [Low] [并发安全] main.rs:131-137 — HWND 跨线程通过 as usize 绕过 Send 约束 ✅ 已修复

**描述**：`HWND` 不是 `Send`。main.rs 通过 `as usize` 转换绕过类型系统，将 HWND 传递到 IPC 线程。当前用法安全（IPC 线程仅用 `hwnd` 调用线程安全的 `PostMessageW`），但绕过类型系统依赖开发者自律，`unsafe` 块没有附带 SAFETY 注释。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（在 `let hwnd_raw = hwnd.0 as usize;` 上方追加正式 `// SAFETY:` 注释块，包含 4 项安全性论证；并补充 `SendHwnd` 类型封装作为未来改进方向；既有功能测试零修改通过）

**建议**：封装一个安全的跨线程 HWND 传递类型 `SendHwnd(HWND)`（`unsafe impl Send`），仅暴露 `post_message` 方法，将 unsafe 约束从"每次使用"收敛到"类型构造时一次"。

---

## 3. v6.0 技术债清单及清理状态（合并）

> 来源：v6.0 技术债审查（2026-07-25）| 清理 spec：`cleanup-v6-wp-proc-tech-debt-2026-07-26`（2026-07-26 完成）
> 以下为原「技术债清单（3.1）」与「清理成果（3.2）」合并后的规范化分类表，每个 WP-TD 项仅保留一行且带唯一清理状态。类型标注对应原清单分类。行号反映 v6.0 清理前代码状态。
> 总技术债：15 项 | 已清理：13 项（86.7%）| 保留现状：2 项（13.3%，WP-TD-008/009，独立 Wave v6-A）| 完成率：100%

### 3.1 死代码 / 冗余抽象 / 过时模式 / 未使用导入

无技术债。经 Grep 验证（`crates/` 全范围），5 个文件中所有 `pub(crate)` 函数/结构体/常量/类型别名均有调用点；`com.rs:is_initialized` 标记为 `#[cfg(test)]`，仅在测试中使用（见 WP-TD-004）。`default_rect` 虽仅被 `parse_rect` 调用一次，但作为具名函数提升可读性，保留合理；各 RAII guard 承担资源清理职责，非冗余。模块内错误转换、RAII 取所有权、异步等待命名风格一致；5 文件的 `use` 引入项均有调用点。

### 3.2 重复实现（3 项，全部已清理）

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| WP-TD-001 | 重复实现 | command.rs:81; command.rs:183; webview.rs:482 | `MirrorStarError::DesktopIntegration(format!("获取 WebView 失败: {}", e))` 错误转换模式在 3 处重复（`navigate_to_url` :81 / `execute_script_and_report` :183 / `create_webview` :482），前缀字符串修改需同步 3 处 | 抽取 `fn corewebview2_error(e: impl std::fmt::Display) -> MirrorStarError` 辅助函数，3 处复用（低） | ✅ 已清理 | webview.rs 新增 `pub(crate) fn corewebview2_error`，command.rs:81 / :183 与 webview.rs:482 三处复用 |
| WP-TD-002 | 重复实现 | webview.rs:177; webview.rs:206 | `MirrorStarWebWallpaperCls` 字面量在 `register_window_class` 重复 2 处（:177 `lpszClassName`、:206 `WindowClassGuard`），修改类名需同步 2 处 | 抽取 `const CLASS_NAME: windows::core::PCWSTR = ...`，两处引用（低） | ✅ 已清理 | 抽取 `const CLASS_NAME` 常量，:177 与 :206 两处字面量改用常量 |
| WP-TD-003 | 重复实现 | ipc_server.rs:469-471; 493-495; 545-547; 579-581 | `PostMessageW(hwnd, WM_CLOSE, ...)` 失败 + warn 模式在 `ipc_thread` 重复 4 次，修改需同步 4 处 | 抽取 `fn notify_main_exit(hwnd: HWND)` 辅助函数，4 处调用（低） | ✅ 已清理 | 新增 `notify_main_exit` 封装 `PostMessageW(WM_CLOSE)` + warn 日志，4 处复用 |

### 3.3 过度设计（1 项，已清理）

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| WP-TD-004 | 过度设计 | com.rs:35-55 | `is_initialized` 的 doc 17 行描述返回 `self.initialized` 的 1 行 `#[cfg(test)]` 方法，含 WP07 修复历史、WP-012 标注、v41-WP-009“未来改进路径”（YAGNI 违规） | 精简为 3-4 行：说明 `#[cfg(test)]` 限制、WP07 后恒为 true，移除投机段（低） | ✅ 已清理 | 精简为 ~7 行，保留 `#[cfg(test)]` 限制说明与 WP07 后恒为 true 事实，移除 v41-WP-009 投机性“未来改进路径”段 |

### 3.4 修复痕迹（4 项，3 已清理 / 1 保留）

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| WP-TD-005 | 修复痕迹 | main.rs:39-61 | `main` doc 含 v41-WP-001 Drop 顺序契约文档化（23 行），前缀对当前读者无意义 | 移除“v41-WP-001”前缀标题，改“Drop 顺序契约”中性标题（低） | ✅ 已清理 | 移除 v41-WP-001 前缀标题，改“Drop 顺序契约”，保留论证内容 |
| WP-TD-006 | 修复痕迹 | main.rs:158-170 | `hwnd_raw` SAFETY 注释后 10 行 v41-WP-002 SendHwnd 现状块，与上方 :158-159 “未来可封装 SendHwnd”重复 | 合并 `:158-159` 与 `:161-170`，移除 v41-WP-002 前缀（低） | ✅ 已清理 | 合并为一段，移除 v41-WP-002 前缀，保留三段式说明 |
| WP-TD-007 | 修复痕迹 | command.rs:230; 239; 253; 1094 | 4 处 `[New]-11.3` / `Wave 2I` 历史标记散落 doc/注释，读者无法判断是否仍为有效 spec 引用 | 移除 `[New]-11.3` 和 `Wave 2I` 标记，保留描述性文字（低） | ✅ 已清理 | 4 处历史标记移除，保留“消除 WpProcResponse 字面量重复”等描述性文字 |
| WP-TD-008 | 修复痕迹 | 全 5 文件 | WPxxx 历史标记大量散落（`WP01`-`WP14`、`WP-001`-`WP-012`、`W-005`-`W-011`、`SEC-003`-`SEC-004`），5 文件共 100+ 处构成注释噪音 | 分批清理：保留标记关联的设计理由描述，移除前缀；建议作为独立 Wave v6-A 任务批量处理（中） | ⚠️ 保留现状 | 作为独立 Wave v6-A 任务处理，与本 spec 小步迭代节奏不匹配 |

### 3.5 命名一致性（2 项，1 已清理 / 1 保留）

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| WP-TD-009 | 命名一致性 | 全 5 文件 | 同类“历史修复标记”概念存在 5 种前缀风格（`WP01`、`WP-001`、`v41-WP-001`、`W-005`、`SEC-003`），无法统一检索且 `WP01`/`WP-001` 易混淆 | 依赖 WP-TD-008 批量清理时统一移除前缀，或统一为单一风格（中） | ⚠️ 保留现状 | 依赖 WP-TD-008 决策，若 WP-TD-008 选择移除前缀则本项自动消解 |
| WP-TD-010 | 命名一致性 | ipc_server.rs:248; 250; 251 | 命名管道实体在 `create_pipe_server` 中 3 次更名（`pipe_name` 不含前缀 / `pipe_path` 含 `\\.\pipe\` / `pipe_path_h` HSTRING），语义边界模糊 | 在 `pipe_name` 参数 doc 明确“不含 `\\.\pipe\` 前缀”，或重命名（低） | ✅ 已清理 | `pipe_name` 参数 doc 明确“不含 `\\.\pipe\` 前缀的管道基础名称” |

### 3.6 注释陈旧（5 项，全部已清理）

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| WP-TD-011 | 注释陈旧 | main.rs:127-128 | **STALE**：注释称 create_webview 失败 `std::process::exit(1)` 提前退出，实际（:121）已改 `return Err(e.into())`（WP-008），注释自相矛盾 | 将“std::process::exit(1) 提前退出”改为“return Err 提前退出”（低） | ✅ 已清理 | 改为 “return Err 提前退出”，与 WP-008 实际退出机制一致 |
| WP-TD-012 | 注释陈旧 | webview.rs:499-500 | **STALE**：注释称 main 通过 `std::process::exit(1)` 退出，实际为 `return Err(e.into())`，与 WP-TD-011 同类 | 改“main 通过 return Err 退出子进程”（低） | ✅ 已清理 | 改为 “main 通过 return Err 退出子进程” |
| WP-TD-013 | 注释陈旧 | main.rs:410-415 | **STALE**：测试模块“已知限制”注释称未修改 wp-proc 解析逻辑，实际 :24 已设 `allow_hyphen_values = true`；称测试 ignored 但实际无 `#[ignore]` | 移除整段“已知限制”注释，改简短说明（低） | ✅ 已清理 | 移除“已知限制”段落，改为简洁说明 |
| WP-TD-014 | 注释陈旧 | main.rs:425-430 | **STALE**：测试 `w009_...` doc 称“当前为已知限制...标记为 `#[ignore]`”，实际已支持且无 `#[ignore]` | 移除陈旧 doc，改为说明 W-009 核心场景已支持（低） | ✅ 已清理 | 移除陈旧 doc，改为说明支持场景 |
| WP-TD-015 | 注释陈旧 | com.rs:39-41 | **MISLEADING**：doc 称“保留方法以维持 API 兼容性”，但方法为 `#[cfg(test)]`，仅测试编译存在，不构成“API” | 移除误导论证，改“为单元测试提供 COM 初始化状态可观测点”（低） | ✅ 已清理 | 移除误导论证，改为说明 `#[cfg(test)]` 内部可观测点 |

### 3.7 验证结果

- **编译**：`cargo build -p mirrorstar-wp-proc` 通过
- **测试**：`cargo test -p mirrorstar-wp-proc` 65 个逻辑测试通过（WebView2 环境依赖测试因 STATUS_ILLEGAL_INSTRUCTION 跳过，与代码变更无关）
- **clippy**：`cargo clippy -p mirrorstar-wp-proc -- -D warnings` 零警告
- **Grep 残留验证**：无 `[New]-11.3` / `Wave 2I` / `v41-WP-001` / `v41-WP-002` / `v41-WP-009` / `API 兼容性` / `已知限制` / `MirrorStarWebWallpaperCls` 字面量重复（仅常量定义 1 处）/ `获取 WebView 失败` 字面量重复（仅辅助函数定义 1 处）/ `WM_CLOSE 未送达` 字面量重复（仅辅助函数定义 1 处）

### 3.8 衍生收益

- **错误转换统一**：`corewebview2_error` 辅助函数消除 3 处“获取 WebView 失败”前缀字符串重复，未来调整错误前缀仅需修改 1 处
- **WM_CLOSE 通知统一**：`notify_main_exit` 辅助函数消除 4 处 PostMessageW + warn 日志模式重复
- **窗口类名集中**：`CLASS_NAME` 常量消除 `MirrorStarWebWallpaperCls` 字面量重复，避免注册类名与 guard 持有类名不匹配导致 `UnregisterClassW` 失败的风险
- **注释体量压缩**：`is_initialized` doc（17 → ~7 行）+ main.rs Drop 顺序契约前缀清理 + v41-WP-002 块合并 + W-009 已知限制注释移除，累计减少约 30-40 行注释噪音

### 3.9 与 v4.0 / v5.0 文档的关联

- **v4.0 已覆盖项**：v4.0 Wave 2I 修复了 `[New]-11.3`（`ok_response`/`error_response` 构造函数抽取）和 B-004，本审查不重复记录已修复项，仅记录其修复痕迹本身（WP-TD-007）；v4.0 的 WP01-WP14、WP-001-WP-012、W-005-W-011、SEC-003-SEC-004 系列修复已在代码中通过标记固化，本审查 WP-TD-008 标记了这 100+ 处历史标记的批量清理需求；v4.0 Wave 2D 的 W-009 修复（`allow_hyphen_values = true`，main.rs:24）已实施，本审查 WP-TD-013/014 标记了其中未同步更新的陈旧注释。
- **v5.0 已覆盖项**：v5.0 未针对 wp-proc 模块进行性能优化（v5.0 性能 findings 集中在 desktop/wallpaper 模块）。wp-proc 的 `wait_with_pump_timeout`、`read_line_with_limit`、`OwnedHandle`/`ControllerGuard` RAII 等均为 v4.0 修复引入，v5.0 未变动。v4.1 的 v41-WP-001 至 v41-WP-010 已全部修复完成，本审查 WP-TD-005/006/004 标记了其修复痕迹清理需求。
- **v6 新发现**：注释陈旧（WP-TD-011/012，`std::process::exit(1)` 未随 WP-008 更新）、W-009 限制已解决但注释未更新（WP-TD-013/014）、`is_initialized`"API 兼容性"误导性注释（WP-TD-015）、"获取 WebView 失败"错误转换重复（WP-TD-001）、"MirrorStarWebWallpaperCls" 字面量重复（WP-TD-002）、PostMessageW(WM_CLOSE) 失败处理重复 4 次（WP-TD-003）——本次首次发现。

---

## 4. 优化机会与交集汇总

### 4.1 v4.0 优化机会（13 项，均已实现）

> 13 项 v4.0 优化机会（优先修复 High 3 / 系统性修复 Medium 4 / 收尾修复 1 / 渐进优化 Low 6）已全部落实（WP-001~WP-013），逐项修复状态与落实说明见 §2「v4.0 审查发现与修复状态」。

### 4.2 v6.0 非技术债优化机会（3 项，评估价值保留）

| 优化点 | 现状评估 | 建议 |
|---|---|---|
| `navigate_to_url` 与 `execute_script_and_report` 的错误响应模式统一 | 两者均调用 controller 方法并构造错误响应，但签名风格不同（前者返回 `Result<(), _>`，后者直接返回 `WpProcResponse`）。当前设计有其合理性：`execute_script_and_report` 内部需区分 CoreWebView2 失败与 JS 注入失败两种错误前缀，直接返回响应更直接 | 可选统一为 Result 风格。低优先，暂保留现状 |
| `ipc_thread` 的 30s 响应超时硬编码（ipc_server.rs:574 `recv_timeout(Duration::from_secs(30))`） | 30s 硬编码在函数体内，与 `WEBVIEW2_OP_TIMEOUT` 风格不一致 | 可考虑提取为常量（如 `IPC_RESPONSE_TIMEOUT: Duration`），便于后续调优 |
| `read_line_with_limit` 的 `LineReadError::Io` 变体通过 `unreachable!` 处理（ipc_server.rs:519-522） | 依赖"Io 错误在重试循环内已被处理"的不变量，当前实现正确，但 `unreachable!` 在未来重构时易被破坏 | 可考虑将 Io 错误在重试循环内彻底消费，让类型变为 `Result<String, Eof \| TooLong>`，消除 `unreachable!` 依赖 |

### 4.3 其它版本交集/关联项

- **I-001（ipc 模块）read_line 超时盲区**：属于 `mirrorstar-ipc` 客户端模块（`05-audio-ipc-process模块.md`），与本模块的 `read_line_with_limit`（OOM 防护，见 WP02）是不同模块的同类概念。I-001 已在路线图标记 ⚡ 完成于 v4.0 Wave 1C，后续 v41-I-001（UTF-8 截断）标记 ⚡ 完成于 v4.1 Wave v41-A。
- **预热池 / WarmWpProc**：该机制已由代码审查确认**移除**（不属于 wp-proc 当前模块的组件），不作为待修复项。
- **遗留待处理（WP-TD-008/009）**：v6 技术债清理唯一保留项——5 文件 100+ 处历史修复标记（WPxx/W-xx/SEC-xx/v41-WP-xx 前缀）的批量清理，作为独立 Wave v6-A 任务处理。

## 5. v17.0 内存优化（2026-10-02）

本模块涉及 **Task 5：网页壁纸 WebView2 内存裁剪与环境共享评估**。

### 5.1 Task 5.1：AdditionalBrowserArguments 裁剪（`webview.rs`）

从 `build_webview2_environment_options` 中抽出**纯函数** `build_additional_browser_args()`（不触碰 COM，便于单测断言参数契约），并新增两项裁剪参数：

| 新增参数 | 作用 / 预期效果 |
|---|---|
| `--renderer-process-limit=1` | wp-proc 只渲染一个页面，限制渲染进程数为 1，省去多余 renderer 进程内存基线。风险低：跨域 iframe 会与主 frame 共用 renderer（牺牲站点隔离、单 renderer 内存略升），但本进程是 fail-closed 沙箱面，隔离损失可接受 |
| `--js-flags=--max-old-space-size=512` | 把 V8 老生代堆上限压到 512MB（64 位默认约 2-4GB），约束失控增长。取值保守（普通网页堆通常 <100MB、WebGL 纹理不占 V8 堆），避免 OOM/白屏 |

**既有参数全部保留**：`--autoplay-policy=no-user-gesture-required`（E2E 免手势播放必需）、`--disk-cache-size=10485760`、`--disable-gpu-program-cache`、`--disable-gpu-shader-disk-cache`、`--disable-features=MediaCache,FontCache,GpuMemDiscardable,BackForwardCache`。

**测试**：`task5_1_additional_browser_args_contract` 断言必须存在 / 必须保留 / 不得引入的参数契约，防止后续维护误删 `--autoplay-policy=no-user-gesture-required` 或改动内存裁剪参数。

### 5.2 Task 5.2/5.3：环境共享评估结论 —— **放弃**

目标：让主窗口（Tauri/wry 托管的 WebView2）与 wp-proc 共用同一 UDF + 环境选项，以省下一份 `msedgewebview2` 进程基线（约 5-6 进程 / 150-300MB）。

- **可行性核实**：WebView2 官方文档（Manage user data folders）明确跨进程共享同一 UDF 是**被支持的**（「A WebView2 control shares its WebView2 session with any other WebView2 control that uses the same UDF...」），且默认 `ExclusiveUserDataFolderAccess=false` 不禁止其他进程加入同一 UDF。因此现有 per-PID UDF **并非**因「不允许共享」而存在，而是**主动选择隔离**；放弃共享是**架构取舍，不是技术不可行**。
- **放弃理由（均为架构级回退，收益不抵代价）**：
  1. **崩溃隔离回退（决定性）**：wp-proc 存在的首要理由就是崩溃隔离（见 `wallpaper/web.rs` 的 `WebRenderer` 文档）；壁纸页面是不受信任的第三方网页（fail-closed 沙箱面），崩溃概率最高。共享 browser 进程后其崩溃会一并拖垮主窗口设置 UI。
  2. **多显示器能力回退（决定性）**：per-PID UDF 让每个 wp-proc 成为独立进程集合，多显示器可同时开多个 Web 壁纸、各自独立 terminate/重启；共享单一 UDF 后所有实例落进同一 browser 进程集合，且 `cleanup_wp_proc_user_data_folder` 会在实例退出时 `remove_dir_all` 整个 UDF，破坏仍在运行的环境（文档明确 UDF 只能在无活动 session 时删除/移动）。
  3. **环境选项无法对齐**：共享要求「同一 UDF + 等价环境配置」，但主窗口由 Tauri/wry 托管，wp-proc 无法控制其 UDF 与参数；壁纸侧需要主窗口不应有/不应受影响的进程级配置（`--autoplay-policy=no-user-gesture-required`、`WEBVIEW2_DEFAULT_BACKGROUND_COLOR` 透明注入、CDP 注入等）。
  4. **净收益不确定**：即便省下一份基线，也需把壁纸移入主进程或引入集中式 UDF 生命周期管理 + 全参数对齐，复杂度与风险远超预期收益。
- **结论落点**：已写入 `webview.rs` 中 `build_webview2_user_data_folder` 的文档注释，含「若未来仍要共享」的前置条件（移入主进程、UDF 生命周期集中管理、接受崩溃隔离降级并依赖 `ProcessFailed` 重建、用 `MEM-ATTRIB` 确认收益）。

**预期效果**：约束单个网页壁纸子进程的 renderer 进程数与 JS 堆峰值，同时不因共享环境而回退崩溃隔离与多显示器能力。**优化前后实测数值待真机补测**（本环境无 GUI，不伪造数据）。

---

## 6. 深度审查（v8，2026-10-11）

> 维度：死代码 / 冗余代码 / 过度抽象 | 范围：`crates/mirrorstar-wp-proc/src/` 全部 5 个源文件（491 + 1525 + 103 + 1471 + 2205 = 5795 行）
> 方法：逐文件审查 + 全仓（`crates/`、`src-tauri/src/`）引用搜索 + 跨 crate 契约核对；判定口径见 §6.7
> 结论：**无生产死代码**；主要问题为跨文件/跨 crate 的骨架重复与注释陈旧；已实施 6 项零行为变更清理（§6.7），涉及公开 API 与跨模块契约的项一律标记「待确认」（§6.8）

### 6.1 死代码（无）

5 个文件中所有 `pub` / `pub(crate)` / 私有符号均有生产或测试调用方，全仓无 `#[allow(dead_code)]` 抑制（`com.rs:is_initialized` 为 `#[cfg(test)]` 门控，仅测试编译存在）。

| 核查项 | 结论 |
|---|---|
| `lib.rs`：`WpProcArgs` 5 个字段 | 全部被读取（`source`/`pipe_name`/`title` 于日志与窗口创建、`rect` 于 `parse_rect`、`cdp_port` 于 `create_webview`） |
| `lib.rs`：`run_with_argv` / `run` / `From<Cli>` | `run_with_argv` 被壳 exe `src-tauri/src/main.rs:64` 调用；`run` 被 `run_with_argv` 调用；`From<Cli>` 被 `run_with_argv` 使用。唯一外部生产引用为 `run_with_argv` |
| `command.rs`：`handle_command` 分支穷尽性 | `WpProcCommand`（`mirrorstar-core/src/ipc/wp_proc.rs`）共 7 变体（Play/Terminate/SetPosition/Pause/Resume/SetVolume/ToggleMute），`handle_command` 逐一覆盖，无 `_` 兜底、无遗漏 |
| `command.rs`：私有 helper | `navigate_to_url`/`execute_script_and_report`/`run_control_command`/`execute_control_command`/`flush_pending_control_commands`/`build_set_volume_script`/`build_resume_script`/`ok_response`/`error_response` 调用方均齐全 |
| `ipc_server.rs`：`LineReadError` 3 变体 | `Io`/`Eof`/`TooLong` 均被构造与匹配消费，无未消费变体 |
| `ipc_server.rs`：RAII 与纯函数 | `OwnedHandle`/`SecurityAttributesGuard`/`process_line`/`format_response`/`read_line_with_limit`/`notify_main_exit`/`ipc_thread` 调用方齐全 |
| `webview.rs`：全部函数/常量 | `parse_rect`/`default_rect`/`get_module_handle`/`register_window_class`/`create_window`/`ControllerGuard`/`wait_with_pump_timeout`/`build_webview2_*`/`apply_transparent_controller`/`harden_webview`/`install_crash_recovery`/`create_webview`/`percent_encode_path`/`hex_upper`/`has_scheme_prefix`/`extract_host`/`is_remote_domain_allowed(_with)`/`build_url`/`corewebview2_error` 及常量 `WEBVIEW2_OP_TIMEOUT`/`FALLBACK_SCREEN_*`/`CLASS_NAME`/`ALLOWED_REMOTE_DOMAINS`/`HEX_CHARS`/`INJECT_CSP_SCRIPT` 均有引用 |
| `com.rs`：`ComGuard`/`new`/`is_initialized` | 均被调用（`is_initialized` 仅测试引用且 `#[cfg(test)]` 门控） |

### 6.2 冗余代码

| ID | 位置 | 描述 / 影响 | 级别 | 状态 |
|---|---|---|---|---|
| V8-WP-R1 | `webview.rs` `create_webview` 环境创建 3 分支 | `.map_err(webview2_com::Error::WindowsError).map_err(\|e\| MirrorStarError::DesktopIntegration(format!("创建 WebView2 环境失败: {}", e)))` **逐字重复 3 次** | Medium | ✅ 已清理（抽取 `env_create_error`） |
| V8-WP-R2 | `ipc_server.rs:383-384` | `v41-WP-004` 迁移指针注释，与同文件 `:27-28` 的 `use` 前置注释**完全同义**（函数迁走后遗留占位） | Low | ✅ 已清理 |
| V8-WP-R3 | `lib.rs` `Terminate` 分支 | `handle_command` 的 Terminate 已 `DestroyWindow` → `def_window_proc` 的 `WM_DESTROY` 已 `PostQuitMessage(0)`；lib.rs 又显式再调一次 `PostQuitMessage(0)`，且紧随 `break` 后 `WM_QUIT` 不会被 `GetMessageW` 消费 | Low | ⚠️ 待确认（绑 WP10 编号与 WM_DESTROY 契约） |
| V8-WP-R4 | `lib.rs:114`/`:123`/`:132`/`:139`/`:179` | 5 处 `tracing::error!("{}", e)` + `return Err(e.into())` 样板（各日志文案不同，提取需参数化） | Low | ⚠️ 待确认 |
| V8-WP-R5 | `ipc_server.rs` 错误响应写入 | `:507-521` 与 `:565-576` 的「`format_response(...).unwrap_or_else(...)` + `write_all` + `flush` + warn」样板重复 | Medium | ⚠️ 待确认（涉错误回退语义差异） |
| V8-WP-R6 | `ipc_server.rs:159-165` / `:229-235` | `GetProcessHeap()` + `HeapFree(...)` 释放样板重复 2 次（分别释放 SID 串与 SD） | Low | ⚠️ 待确认（涉 `unsafe` 与内存归属） |
| V8-WP-R7 | `command.rs` 控制命令 JS 包装 | `document.querySelectorAll('video, audio').forEach(...)` 片段 6 处（4 个 inline/脚本构造点 + 2 个 `build_*_script`），各 body 不同 | Low | ⚠️ 待确认（提取收益低） |
| V8-WP-R8 | `command.rs:219` `_ => (op_name, "ExecuteScript(Unknown)")` | 4 个调用点 `op_name` 恒为 4 个固定值，该 arm 运行时不可达；但 `op_name` 为 `&str`，**删除后 match 非穷尽无法编译**，属语法必需兜底 | Low | 维持现状（不可删除，非死代码） |

### 6.3 过度抽象

| ID | 位置 | 描述 / 影响 | 级别 | 状态 |
|---|---|---|---|---|
| V8-WP-A1 | `lib.rs:46-69` | `WpProcArgs` 为 `Cli` 的逐字段镜像 + `From<Cli>`，`run` 仅消费这些字段；`run`/`WpProcArgs` 无外部调用方（仅 `run_with_argv` 被壳 exe 使用） | Low | ⚠️ 待确认（属公开 API 表面） |
| V8-WP-A2 | `lib.rs:75` / `:110` | `run` 与 `run_with_argv` 双层入口，`run` 无外部调用方 | Low | ⚠️ 待确认（公开 API） |
| V8-WP-A3 | `webview.rs:323` + `:243` | `create_window` 的 `class_name` 形参唯一实参即模块常量 `CLASS_NAME`（经 `WindowClassGuard::class_name()` 绕行），为同一不变量多造一层间接 | Low | ⚠️ 待确认（收窄 `pub(crate)` surface） |
| V8-WP-A4 | `webview.rs:128` | `corewebview2_error(e: impl Display)` 泛型未被任何非 `windows::core::Error` 类型实例化 | Low | ⚠️ 待确认（`pub(crate)` 跨模块契约） |
| V8-WP-A5 | `webview.rs:948-1054` | 环境完成回调与控制器完成回调的 `tx.send(Err/send(controller.ok_or(E_POINTER)))` 结构近乎重复（handler 类型强绑定，抽象成本高） | Low | ⚠️ 待确认 |
| V8-WP-A6 | `webview.rs:198` / `:1169` / `:1252` | `default_rect`（单调用点） / `hex_upper`（免分配路径） / `is_remote_domain_allowed`（默认 vs 显式白名单可测性分离） | Low | 复核后**维持保留**（与 v6 §3.1 判定一致） |
| V8-WP-A7 | `ipc_server.rs:524-527` | `LineReadError::Io` 经 `unreachable!` 处理，依赖「Io 在重试循环内已消费」不变量（当前正确） | Low | ⚠️ 待确认（v6 §4.2 已登记候选；改类型有行为风险） |
| V8-WP-A8 | `ipc_server.rs:580` | `recv_timeout(Duration::from_secs(30))` 硬编码（与 `WEBVIEW2_OP_TIMEOUT` 风格不一致） | Low | ⚠️ 待确认（v6 §4.2 候选；`:559`/`:583` 日志亦硬编码「30s」） |
| V8-WP-A9 | `com.rs:41-44` | `#[cfg(test)] is_initialized`，`new()` 返回 Ok 时恒为 `true`（断言恒真，观测价值低） | Low | ⚠️ 待确认（WP-012 登记项，测试有引用） |
| V8-WP-A10 | `command.rs:334` | `PendingControlCommand` 的 `#[derive(Debug)]` 全仓无 `{:?}` 使用 | Low | ⚠️ 待确认（保留以便调试） |

### 6.4 跨 crate 重复实现归口结论：`read_line_with_limit`

| 位置 | 可见性 | 签名 | 语义 |
|---|---|---|---|
| `crates/mirrorstar-core/src/ipc/client.rs:412` | `pub` | `(reader: &mut R, max_bytes: usize, total_avail: usize) -> Result<Vec<u8>, MirrorStarError>` | **客户端**读响应；容忍多字节 UTF-8 在 `total_avail` 边界处被截断（v41-I-001），返回原始字节由外层解码；须与 `peek` 报告量协同 |
| `crates/mirrorstar-wp-proc/src/ipc_server.rs:411` | 私有 | `(reader: &mut R) -> Result<String, LineReadError>` | **服务端**逐行读命令；按 `MAX_LINE_BYTES`（1MB）行内上限防 OOM，返回 lossy 解码后的 `String`；含 `Eof`/`TooLong`/`Io` 三态以驱动重试与残留清理（WP02/WP12） |

**结论：各自保留并注明差异，不合并。** 理由：两者返回类型（`Vec<u8>` vs `String`）、限流语义（跨块 `total_avail` 协同 vs 单行硬上限）、错误类型（`MirrorStarError` vs 三态 `LineReadError`）与调用场景（客户端读响应 vs 服务端读命令）均不同；强行合并会引入跨 crate 契约变更与行为差异风险（WP02/WP12 的测试契约亦依赖后者语义）。本项至此**归口完成**（关闭 ipc 模块审查登记的遗留项）。

### 6.5 文档一致性（doc 07 逐条判定）

| 文档位置 | 判定 | 说明 |
|---|---|---|
| §1.1 模块职责 | **描述失真** | 文件清单以 `main.rs` 为主；`webview.rs` 职责列表缺失 `build_webview2_*`/`harden_webview`/`install_crash_recovery`。已就地校正 |
| §1.2 文件清单 | **描述失真** | `main.rs`(441) → `lib.rs`(491)；command 1132→1525、com 114→103、ipc_server 1469→1471、webview 1274→2205。已就地校正 |
| §1.3 测试覆盖 | **描述失真** | 「main.rs 11 个测试」及各行数估算过时；实测合计 128 个 `#[test]`、0 `#[ignore]`。已就地校正 |
| §2 `WP-001`~`WP-013` | **仍存在（已修复）** | 逐条代码核验：WP-001（`execute_script_and_report` 已提取）、WP-002（`ControllerGuard` RAII）、WP-003（`GetClientRect`/`SetBounds` 返回 Error）、WP-004（降级 warn）、WP-005（buffer 长度 + `IsValidSid`）、WP-006（`NavigationCompleted` 等待）、WP-007（`PeekMessageW`/`DispatchMessageW`）、WP-008（`return Err` 触发 RAII）、WP-009（回退串保留 request_id）、WP-010（`FALLBACK_SCREEN_*` 常量）、WP-011（`hex_upper` 免分配）、WP-012（`#[cfg(test)]`）、WP-013（SAFETY 注释 + SendHwnd 方向）——全部与当前代码一致 |
| §3 `WP-TD-001`~`WP-TD-007`、`WP-TD-010`~`WP-TD-015` | **仍存在（已清理）** | 核验一致：`corewebview2_error`/`CLASS_NAME`/`notify_main_exit` 抽取仍在；`is_initialized` doc 精简版仍在（未含 v41-WP-009 投机段）；lib.rs 无 `v41-WP-001`/`v41-WP-002` 前缀；command.rs 无 `[New]-11.3`/`Wave 2I`；ipc_server.rs 无 `std::process::exit(1)` 陈旧串；W-009 陈旧 doc 已移除 |
| §3 `WP-TD-008` / `WP-TD-009` | **仍存在（保留现状）** | 5 文件仍有 100+ 处历史标记（`WPxx`/`WP-xxx`/`v41-WP-xxx`/`W-xxx`/`SEC-xxx`）；且 `WP-008` 在 `ipc_server.rs:80`（安全属性）与 `lib.rs:88`（exit→return Err）含义冲突，即 WP-TD-009 登记项 |
| §4.1 v4 优化机会 13 项 | **仍存在（已解决）** | 与 §2 一致 |
| §4.2 v6 非技术债 3 项 | **仍存在（未变）** | ①错误响应模式统一 → 维持现状；②30s 硬编码 → 见 V8-WP-A8（待确认）；③`unreachable!` → 见 V8-WP-A7（待确认） |
| §5 v17 内存优化 | **仍存在（描述准确）** | `build_additional_browser_args` 在 `webview.rs:644`，契约测试 `task5_1_additional_browser_args_contract` 在 `webview.rs:1343`；`build_webview2_user_data_folder` 的环境共享"放弃"结论文档仍在（`webview.rs:559-574`，已标注历史理由） |
| §4.3 其它交集/I-001 | **仍存在（描述准确）** | I-001 属 core ipc 客户端模块，与本模块 `read_line_with_limit` 为同类不同模块概念；`read_line_with_limit` 归口见 §6.4 |

**清理红线（`include_str!` 源码断言锁定，不得改写/删除）**：本模块共 10 处 `include_str!` 断言（webview.rs ×2、command.rs ×5、ipc_server.rs ×3），其 needle 中含若干**注释文本**，例如 `WP-006:`、`v41-WP-005`、`handler_guard.release()`、`WP-004:`、`WP-005:`、`if sa_guard.is_none()`、`v41-WP-008`、`handle_guard.into_raw()`、`/// 取出内部的`、`no-op`、`v41-WP-010`。本轮所有注释清理均已先行核验未触碰这些 needle。

### 6.6 已实施清理 vs 待确认（口径）

**判定口径**：**可清理** = 零行为变更 + 全仓无调用方或语义严格等价 + 非公开 API 表面 + 不涉跨模块契约/已登记编号（`WP-*`/`WP-TD-*`/`W-*`/`SEC-*`）；否则一律「待确认」，不实施。

### 6.7 已实施的零行为变更清理（6 项）

| # | 文件 | 变更 | 类型 |
|---|---|---|---|
| 1 | `webview.rs` | 新增 `env_create_error` 辅助，合并 `create_webview` 中 3 处逐字重复的环境创建错误映射闭包（错误前缀与类型不变） | 冗余合并 |
| 2 | `webview.rs:33-34` | WP-010 回退尺寸注释校正：删除「rect 解析异常时使用」的失真表述（`--rect` 解析失败现返回 Err，不再回退） | 注释修正 |
| 3 | `webview.rs:1692` | 陈旧注释校正：`about:blank` 用例指向的测试名与方向修正（`allows` → `rejects`，实测测试为 `test_build_url_rejects_about_blank_case_insensitive`） | 注释修正 |
| 4 | `command.rs:174/183-186/538-540` | `execute_script_and_report` doc 与 WP-001 注释的范围校正：由「Pause/Resume 共用」更正为 4 个控制命令共用（Pause/Resume/SetVolume/ToggleMute） | 注释修正 |
| 5 | `command.rs:621/745/1009/1013/1082-1083/635` | 测试注释措辞校正：`test_controller()` 实际返回 `NonNull::dangling()`（非 null），「null controller」→「dangling controller」 | 注释修正 |
| 6 | `ipc_server.rs:383-384` | 删除与同文件 `:27-28` 完全同义的 `v41-WP-004` 迁移指针注释（函数迁走后遗留占位） | 冗余删除 |

### 6.8 待确认项汇总（不实施，需单独评审）

- **公开 API 表面**：V8-WP-A1（`WpProcArgs`/`From<Cli>` 镜像）、V8-WP-A2（`run`/`run_with_argv` 双层入口）、V8-WP-A3（`create_window` 的 `class_name` 形参 + `WindowClassGuard::class_name`）、V8-WP-A4（`corewebview2_error` 泛型收窄）——均涉及 `pub`/`pub(crate)` 签名或跨模块契约。
- **行为/契约**：V8-WP-R3（Terminate 防御性 `PostQuitMessage`）、V8-WP-R5（错误响应写入样板抽取）、V8-WP-R6（`HeapFree` 样板，`unsafe`）、V8-WP-A7（`unreachable!` 类型级重构）、V8-WP-A8（30s 硬编码）、V8-WP-A9（`is_initialized` 移除需改测试）。
- **低收益/保留**：V8-WP-R7（JS 包装片段）、V8-WP-A5（完成回调抽象）、V8-WP-A6（`default_rect`/`hex_upper`/`is_remote_domain_allowed` 复核保留）、V8-WP-A10（`PendingControlCommand` 的 `Debug` derive 保留）。
- **登记编号**：`WP-TD-008`/`WP-TD-009`（100+ 处历史标记批量清理）——按原登记作为独立 Wave 处理，本轮保持现状。

### 6.9 验证结果

- **编译**：`cargo check -p mirrorstar-wp-proc --all-targets` 通过（Finished，0 error / 0 warning）
- **测试**：`cargo test -p mirrorstar-wp-proc` = **128 passed / 0 failed / 0 ignored**，与审查前基线完全一致
- **全仓**：`cargo check --workspace` 通过（Finished，0 error）；`cargo clippy --workspace --all-targets` 0 warning；`cargo test -p mirrorstar-core --lib -- --skip mpv_backend` = 712 passed / 0 failed / 30 ignored；`cargo test -p mirrorstar-wallpaper` 全绿（0 failed）；均与基线一致
- **API 面**：`git diff` 核对无 `pub` / `pub(crate)` 签名增删（仅新增 1 个私有 `fn env_create_error`）
