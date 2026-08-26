# wp-proc 子进程模块优化文档

> [← 返回索引](./README.md)

## 模块概要

- **模块路径**：`crates/mirrorstar-wp-proc/src/`（v3.5 实测：5 源文件 / 2620 行 / 87 单元测试）
- **审查文件**：5 个（约 3,428 行，含测试代码）
  - `main.rs`（363 行）— 子进程入口 + Cli 解析（source/pipe_name/title/rect）+ ComGuard + 窗口创建 + WebView2 初始化 + 命名管道 + 消息循环 + 退出清理
  - `com.rs`（105 行）— COM 初始化 RAII guard（`CoInitializeEx`/`CoUninitialize`，RPC_E_CHANGED_MODE 处理）
  - `command.rs`（783 行）— 命令处理（Play/Navigate/Terminate/SetPosition/Pause/Resume）+ SetPosition 边界校验 + JS 注入错误传播
  - `ipc_server.rs`（1233 行）— 命名管道服务端（SDDL 安全属性）+ IPC 线程 + `process_line`/`format_response` 纯函数 + 1MB 行上限
  - `webview.rs`（944 行）— 窗口类注册 RAII + 窗口创建 + WebView2 环境与控制器创建 + `wait_with_pump_timeout` 超时泵 + `build_url` 协议白名单 + `percent_encode_path`
- **核心结构**：独立二进制，通过 clap 接收 CLI 参数；COM 初始化为 STA（`COINIT_APARTMENTTHREADED`，WebView2 要求）；双线程架构（主线程 Win32 消息循环处理 WebView2，IPC 线程读取管道命令）；线程间通信 `std::sync::mpsc` + `PostMessageW(WM_WEB_COMMAND)`
- **IPC 协议**：JSON + 换行分隔，`WpProcCommand` 枚举（Play/Terminate/SetPosition/Navigate/Pause/Resume），`WpProcResponse`（`ResponseStatus` 枚举）
- **设计模式**：
  - **OwnedHandle RAII**（WP01）：`OpenProcessToken` 等返回的内核句柄由 `OwnedHandle` 包装，Drop 时自动 `CloseHandle`
  - **read_line_with_limit**（WP02）：`fill_buf` + `consume` 增量读取，超限立即返回 `TooLong`，OOM 防护
  - **ComGuard RAII**（WP07）：`RPC_E_CHANGED_MODE` 返回 `Err` 退出子进程
  - **WindowClassGuard RAII**：Drop 时 `UnregisterClassW`
  - **SecurityAttributesGuard**：管道 SDDL 安全描述符限制为当前用户 SID
  - **wait_with_pump_timeout**（W-006）：替代 `webview2_com::wait_with_pump`（内部 `GetMessageA` 无限阻塞），加入截止时间检查
  - **URL 协议白名单**（SEC-004/WP-001）：`has_scheme_prefix` 用 `eq_ignore_ascii_case` 阻断 `JavaScript:`/`JAVASCRIPT:` 等大小写变体，仅允许 http/https 与本地路径
  - **WebView2 创建失败退出**（WP03）：不再进入 degraded 状态静默运行，让父进程通过子进程退出码感知渲染失败
  - **纯函数提取**（WP-006）：`process_line`/`format_response` 提取为纯函数，可独立单元测试
  - **退出清理**：`Controller.Close()` → drop cmd_rx → IPC 线程 join（1s 超时）→ `class_guard.Drop` → `com_guard.Drop`，RAII 逆序释放

## v4.0 审查发现（13 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/08-wp-proc.md`
> 严重级别分布：Critical 0 / High 3 / Medium 4 / Low 6
> 维度分布：逻辑 1 | 并发 1 | 资源 3 | 错误 2 | 安全 1 | 可维护性 5

### 审查重点说明

`mirrorstar-wp-proc` 子进程经过 v3.0→v3.5 共 5 轮修复后整体质量较高，多个关键修复体现了严谨的工程思维：OwnedHandle RAII（WP01）、限流读取（WP02）、URL 协议白名单大小写不敏感（WP-001/SEC-004）、管道 SDDL 安全描述符（WP-008）、create_webview 失败退出（WP03）、纯函数提取（WP-006）。本次审查聚焦于遗留债务：Pause/Resume 命令处理代码重复（约 70 行）、create_webview 失败路径资源清理不完整、SetPosition 错误处理不一致、安全降级路径可观测性不足。

### [WP-001] [High] [可维护性] command.rs:147-220, 221-294 — Pause/Resume 命令处理代码大量重复

**描述**：`Pause` 与 `Resume` 两个 match 分支的代码结构几乎完全相同（各约 74 行），仅 JS 脚本字符串、日志消息、错误前缀三处不同。完全重复的逻辑包括：`controller.CoreWebView2()` 获取与错误响应构造、`mpsc::channel` 创建 + `ExecuteScriptCompletedHandler::create` 回调构造、`webview.ExecuteScript` 调用、`wait_with_pump_timeout` 调用 + `script_result` 匹配、最终 `WpProcResponse` 构造。这种重复违反 DRY 原则，未来若需修改 ExecuteScript 的错误处理逻辑（如增加重试、修改超时、调整回调行为），必须同步修改两处，容易遗漏导致行为不一致。

**建议**：提取共享 helper 函数 `execute_script_and_report(controller, script, op_name, request_id) -> WpProcResponse`，将差异点（脚本、操作名）参数化。`Pause`/`Resume` 分支分别调用该 helper。

### [WP-002] [High] [资源管理] webview.rs:402-446 — create_webview 失败路径未显式调用 controller.Close()

**描述**：`create_webview` 在 controller 创建成功后，后续步骤若失败，`?` 提前返回 `Err`，`controller` 作为局部变量被 drop。windows-rs / webview2-com 的 COM 接口 `Drop` 仅调用 `Release()`（减少 COM 引用计数），**不**调用 `ICoreWebView2Controller::Close()`。`Close()` 是显式释放 WebView2 资源（包括关联的浏览器进程、渲染管线）的方法，与 `Release()` 语义不同。

受影响的失败路径（controller 已创建但 create_webview 返回 Err）：`SetIsVisible(true)` 失败、`CoreWebView2()` 失败、`build_url(source)` 失败、`Navigate(...)` 失败、`GetClientRect(hwnd, ...)` 失败、`SetBounds(rect)` 失败。

**对比**：main.rs 的其他路径正确调用了 `Close()`（成功退出路径 main.rs:192、create_pipe_server 失败路径 main.rs:119），唯独 create_webview 内部失败路径遗漏了这步。由于 create_webview 返回 Err 后进程立即 `std::process::exit(1)`（main.rs:100），系统会回收资源，故无永久泄漏。但这是不一致的资源清理模式，若未来 create_webview 失败不再 exit 而是重试，会累积未 `Close` 的 controller。

**建议**：在 create_webview 的失败路径显式调用 `Close()`，或用 RAII guard（`ControllerGuard`）确保 `Close()` 被调用，成功返回时 `guard.into_inner()` 取出 controller，失败时 Drop 调用 `Close()`。

### [WP-003] [High] [错误处理] command.rs:118-127 — SetPosition 中 GetClientRect/SetBounds 失败仅 warn 但返回 Ok

**描述**：`SetPosition` 命令的错误处理存在不一致。`SetWindowPos` 失败返回 `Error`（WP06 修复），但 `GetClientRect`/`SetBounds` 失败仅 `tracing::warn!`，仍返回 `Ok`。注释解释"GetClientRect/SetBounds 失败为非致命（窗口已移动，仅边界同步失败）"。但这个解释值得商榷：

1. **GetClientRect 失败**：`rect` 保持 `RECT::default()`（全零），`SetBounds((0,0,0,0))` 会让 WebView2 渲染区域为零尺寸（不可见）。窗口已移动但 WebView2 消失，用户看到空白区域。
2. **SetBounds 失败**：WebView2 边界保持旧值，与窗口新位置不匹配，渲染区域错误。

两种情况都导致壁纸渲染异常，但父进程收到 `Ok` 误以为位置设置成功，不会重试或上报错误。这与 WP06 的修复精神（"SetWindowPos 失败意味着窗口位置/尺寸未变，是严重故障，返回 Error"）不一致——边界不同步同样是严重故障。

**建议**：`GetClientRect` 或 `SetBounds` 失败时返回 `ResponseStatus::Error`，让父进程感知边界同步失败并决定后续处理（如重试或上报 wallpaper-error）。

### [WP-004] [Medium] [安全/可观测性] ipc_server.rs:239-243 — build_pipe_security_attributes 失败时静默降级到默认安全描述符

**描述**：`create_pipe_server` 调用 `build_pipe_security_attributes` 构建限制为当前用户 SID 的 SDDL 安全描述符。若该函数返回 `None`（如 `OpenProcessToken`/`GetTokenInformation`/`ConvertSidToStringSidW` 任一步骤失败），代码静默回退到 `None`（默认安全描述符），无 warn 日志。默认安全描述符下，管道的访问权限由系统默认 DACL 决定，可能允许同机其他用户连接。虽然管道名含高熵 UUID 难以猜中，且 wp-proc 仅渲染壁纸无敏感数据，但安全降级发生时无日志，运维无法感知管道使用了弱于预期的安全配置。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：在 `create_pipe_server` 降级路径记录 warn 日志，说明管道使用默认安全描述符（可能允许其他用户连接）。

### [WP-005] [Medium] [资源管理/安全] ipc_server.rs:114 — token_user 转换未校验 buffer 长度

**描述**：`build_pipe_security_attributes` 中，将 `Vec<u8>` buffer 直接转换为 `TOKEN_USER` 引用时未校验长度：`let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);`。虽然 `GetTokenInformation` 第二次调用成功后应填充完整数据，理论上 `buffer.len() >= size_of::<TOKEN_USER>()`。但缺乏防御性校验：若 `GetTokenInformation` 实现行为异常（如返回 `return_length` 小于 `TOKEN_USER` 大小但 `ok()` 仍成功），会导致 `token_user` 解引用读到 buffer 外的内存（越界读）。此外，`token_user.User.Sid` 的 `sid.0` 已检查 null，但未验证 SID 有效性（`IsValidSid`），后续 `ConvertSidToStringSidW(sid, ...)` 可能对无效 SID 产生未定义行为。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：添加防御性长度校验 `if buffer.len() < std::mem::size_of::<TOKEN_USER>() { return None; }`，并用 `IsValidSid` 验证 SID 有效性。

### [WP-006] [Medium] [逻辑] command.rs:20-33, 48-59, 135-146 — Play/Navigate 不等待导航完成即返回 Ok

**描述**：`navigate_to_url` 调用 `webview.Navigate(&url)` 后立即返回 `Ok(())`，`handle_command` 据此返回 `WpProcResponse { status: Ok }`。但 `Navigate` 是异步操作——它仅触发导航流程并立即返回。导航是否真正完成（或失败）通过 `NavigationCompleted` 事件异步通知，当前代码未监听该事件。导航可能因 URL 对应的本地文件不存在、HTTP 4xx/5xx 错误、网络超时、目标页面加载脚本错误等原因失败，但父进程已收到 `Ok` 误以为播放成功。

父进程（`WebRenderer::play`）收到 `Ok` 后会设置 `WallpaperState::Playing` 并报告壁纸已就绪，但实际壁纸可能未成功加载，用户看到空白窗口。由于无 `NavigationCompleted` 监听，wp-proc 无法上报导航失败，父进程无法感知。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：方案 A（推荐）：注册 `NavigationCompletedEventHandler`，在导航完成（成功或失败）后再通过 channel 通知 `handle_command` 返回响应，需设置超时避免永久阻塞（可复用 `wait_with_pump_timeout`）。方案 B：在 `navigate_to_url` 注释和 IPC 协议文档中明确 `Play`/`Navigate` 返回 `Ok` 仅表示"导航已启动"，不保证内容已加载。

### [WP-007] [Medium] [可维护性] webview.rs:305, 313 — wait_with_pump_timeout 使用 ANSI 消息泵，与主循环 Unicode 版本不一致

**描述**：消息泵 API 使用存在 ANSI/Unicode 不一致：主消息循环用 `GetMessageW`/`DispatchMessageW`（Unicode），窗口类用 `RegisterClassW`（Unicode），但 `wait_with_pump_timeout` 用 `PeekMessageA`/`DispatchMessageA`（ANSI）。对于当前代码处理的纯数字消息（`WM_WEB_COMMAND`、`WM_DESTROY` 等），ANSI/Unicode 版本无功能差异。但混合使用违反 Win32 最佳实践——`A` 版本会在涉及字符串参数的消息上执行 ANSI↔Unicode 转换，与 `W` 版本注册的窗口类不匹配，理论上可能在边缘消息上产生编码问题。这种不一致是可维护性隐患：未来若 `wait_with_pump_timeout` 需要处理涉及字符串的消息，开发者可能不会注意到版本差异。

**修复状态**：✅ 已修复于 v4.0 Wave 2D

**建议**：统一使用 `W` 版本（`PeekMessageW`/`DispatchMessageW`），与主消息循环和窗口类注册一致。

### [WP-008] [Low] [资源管理] main.rs:89-101 — std::process::exit(1) 跳过 RAII Drop（class_guard/_com_guard）

**描述**：`create_webview` 失败时，main.rs:100 调用 `std::process::exit(1)` 立即退出进程。`std::process::exit` 跳过所有 RAII guard 的 Drop：`class_guard`（`WindowClassGuard`）的 Drop 不会执行 → `UnregisterClassW` 未调用；`_com_guard`（`ComGuard`）的 Drop 不会执行 → `CoUninitialize` 未调用。注释承认这一点，解释"COM 初始化和窗口类注册在进程退出时由系统自动回收"。这个解释是正确的，但 `std::process::exit` 跳过 Drop 是反模式：与代码其他部分的 RAII 风格不一致；若未来添加需要显式清理的资源，可能因 `exit` 跳过 Drop 而泄漏。注释明确说"任务要求使用 `std::process::exit(1)` 而非 `return Err`"，故这是设计约束而非缺陷。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（将 `std::process::exit(1)` 改为 `return Err(e.into())`，触发 RAII Drop：`_com_guard.drop()` 调用 `CoUninitialize` 平衡 COM 初始化、`class_guard.drop()` 调用 `UnregisterClassW` 注销窗口类；行为等价：main 返回 Err 时 runtime 自动调用 `std::process::exit(1)`，父进程仍能通过非零退出码检测子进程死亡；删除已不再适用的"任务要求使用 std::process::exit(1) 而非 return Err"段落，新增 WP-008 注释段说明 RAII Drop 触发链与行为等价性）

**建议**：若任务约束允许，考虑用 `return Err(e.into())` 替代 `std::process::exit(1)`，让 RAII guard 正确 drop。若任务约束必须用 `std::process::exit`，则在注释中持续标注此约束的影响范围。

### [WP-009] [Low] [错误处理] ipc_server.rs:511-519 — PostMessageW 失败路径的 format_response 回退字符串丢失 request_id

**描述**：`PostMessageW(WM_WEB_COMMAND)` 失败时，代码先用提取的 `request_id` 构造 `WpProcResponse` 并调用 `format_response`。但 `format_response` 失败（`serde_json::Error`，概率极低）时，回退到硬编码字符串 `r#"{"request_id":0,"status":"error","error":"PostMessageW failed"}"#`，用 `request_id=0`，丢失了已提取的实际 `request_id`。客户端收到 `request_id=0` 的响应无法与原始请求匹配，可能误判为协议错误或忽略该响应。

**影响**：实际影响极低——`WpProcResponse` 序列化几乎不会失败（结构简单，字段为基本类型）。但这是错误处理路径的瑕疵，且修复简单。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（抽离 `build_post_message_failed_response(request_id: u64) -> String` 辅助函数，PostMessageW 失败路径回退字符串从硬编码 `request_id=0` 改为动态拼接实际 `request_id`；新增 `format_response_fallback_preserves_request_id` 单元测试覆盖非零 `request_id=42` 场景，断言回退字符串包含 `"request_id":42` 而非 `"request_id":0`）

**建议**：回退字符串动态拼接 `request_id`：`format!(r#"{{"request_id":{},"status":"error","error":"PostMessageW failed"}}"#, request_id)`。

### [WP-010] [Low] [可维护性] webview.rs:90-101 — default_rect 硬编码 1920x1080 回退尺寸

**描述**：`default_rect` 在 `GetSystemMetrics` 返回非正值时回退到硬编码的 1920x1080。1920x1080 是硬编码的魔法数，且在多显示器场景下可能不正确（如主显示器为 4K 2560x1440，或副显示器尺寸不同）。注释 WP14 承认多显示器场景应通过 `--rect` 显式传入目标显示器尺寸，本函数仅作为兜底。但兜底值未提取为命名常量，可读性差且不易维护。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（提取 `FALLBACK_SCREEN_WIDTH: i32 = 1920` 与 `FALLBACK_SCREEN_HEIGHT: i32 = 1080` 模块级常量，`default_rect` 函数体使用常量替代魔法数，补充注释说明"多显示器场景回退尺寸语义"——不针对特定显示器分辨率，仅作"合理默认"占位，多显示器场景应通过 `--rect` 显式传入目标显示器尺寸）

**建议**：提取为命名常量 `const FALLBACK_SCREEN_WIDTH: i32 = 1920;` / `const FALLBACK_SCREEN_HEIGHT: i32 = 1080;` 并补充注释说明选择依据。

### [WP-011] [Low] [可维护性] webview.rs:452-476 — percent_encode_path 手写实现，未使用标准 crate

**描述**：`percent_encode_path` 手写 percent-encoding 实现，保留字符集为 `A-Za-z0-9-_.~/:=@`。问题：(1) 保留字符集非标准——RFC 3986 §2.3 的 unreserved 集合是 `A-Za-z0-9-_.~`，本实现额外保留了 `/:=@`（reserved 字符，在 file URL 路径中保留是为了保持 Windows 盘符和路径分隔符可读性，但 `@` 和 `=` 在 file URL 路径中保留可能导致 URL 解析器误解）；(2) 性能——每个非保留字符调用 `format!("%{:02X}", byte)` 分配新 String，对含大量特殊字符的路径有性能开销；(3) 未使用标准 crate——`percent-encoding` crate 提供更可靠且经过充分测试的实现。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（新增 `hex_upper(byte: u8) -> [u8; 2]` 辅助函数使用 `HEX_CHARS` 查表返回大写十六进制字符；`percent_encode_path` 改用 `String::with_capacity(path.len() * 3)` 预分配最大可能长度，非保留字符改用 `result.push('%')` + `hex_upper(byte)` 双 `push(hex[i] as char)` 替代 `format!("%{:02X}", byte)` 逐字符分配；保留字符集与原实现一致（不引入 `percent-encoding` crate 避免新增依赖）；既有 4 个测试 `test_percent_encode_path` / `test_percent_encode_path_chinese` / `test_percent_encode_path_japanese` / `test_percent_encode_path_mixed` 零修改通过）

**建议**：使用 `percent-encoding` crate（若已添加为依赖）或改进手写实现避免 `format!` 分配（用 `hex_upper` 函数直接 push 字符）。

### [WP-012] [Low] [可维护性] com.rs:43-46 — is_initialized 方法标记 #[allow(dead_code)]，生产代码不调用

**描述**：`ComGuard::is_initialized` 方法标记 `#[allow(dead_code)]`。WP07 修复后，`ComGuard::new()` 返回 `Ok` 时 `initialized` 恒为 `true`（RPC_E_CHANGED_MODE 返回 `Err`）。`is_initialized()` 在生产代码中无调用者，仅为单元测试（`test_comguard_new_initializes_com`）提供内部状态可观测性。这是技术债务：保留了一个语义上恒为 `true` 的方法，仅为测试可观测性服务。`#[allow(dead_code)]` 抑制了编译器警告，但掩盖了"此方法无实际用途"的事实。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（采纳方案 B：`#[allow(dead_code)]` 改为 `#[cfg(test)]`，生产构建通过条件编译排除 `is_initialized` 方法，避免 dead_code 警告；保留方法为测试提供 COM 初始化状态内部可观测点，无需重构测试改用行为验证；新增 WP-012 文档注释说明设计意图"is_initialized 仅用于测试可观测性，生产构建通过 `#[cfg(test)]` 排除"；`com::tests` 2 个测试零修改通过）

**建议**：方案 A：移除方法，将测试改为通过行为验证（如"ComGuard::new() 返回 Ok 且 Drop 不 panic"）。方案 B：保留并改为 `#[cfg(test)]` 限定仅在测试编译时存在。

### [WP-013] [Low] [并发安全] main.rs:131-137 — HWND 跨线程通过 as usize 绕过 Send 约束

**描述**：`HWND` 不是 `Send`（windows-rs 标记为 `!Send`）。main.rs 通过 `as usize` 转换绕过类型系统，将 HWND 传递到 IPC 线程：`let hwnd_raw = hwnd.0 as usize;`。注释说明"HWND 不是 Send，转换为 usize 传递（PostMessageW 是线程安全的）"。这个转换在当前用法下是安全的——IPC 线程仅用 `hwnd` 调用 `PostMessageW`（Win32 文档明确线程安全），不调用 `DestroyWindow`/`SetWindowPos` 等必须由创建线程调用的 API。但绕过类型系统依赖开发者自律：`hwnd_raw` 是 `usize`（`Send`），可以自由传递到任何线程，未来维护者可能在 IPC 线程中用 `hwnd` 调用非线程安全的窗口 API，类型系统无法阻止。`unsafe` 块没有附带 SAFETY 注释。

**修复状态**：✅ 已修复于 v4.0 Wave 3G（在 `let hwnd_raw = hwnd.0 as usize;` 上方追加正式 `// SAFETY:` 注释块，包含 4 项安全性论证：① `PostMessageW` 是线程安全的 Win32 API，可在任意线程对任意 HWND 调用，内部通过消息队列异步派发；② 子进程内 HWND 生命周期与进程一致，主线程与 IPC 线程共享同一地址空间，无跨进程句柄复用风险；③ 主线程与 IPC 线程均只通过 `PostMessageW` 操作该 HWND，不调用需在创建线程执行的 API（如 `DestroyWindow`、`SetWindowLongPtrW` 等）；④ `DestroyWindow` 仅在主线程的正常退出清理路径调用，IPC 线程不参与窗口销毁；并补充 `SendHwnd` 类型封装作为未来改进方向——`struct SendHwnd(HWND); unsafe impl Send for SendHwnd {}`，将 `unsafe` 收敛到类型构造时一次，仅暴露 `post_message` 等线程安全 API；既有功能测试零修改通过）

**建议**：封装一个安全的跨线程 HWND 传递类型 `SendHwnd(HWND)`（`unsafe impl Send`），仅暴露 `post_message` 方法，将 unsafe 约束从"每次使用"收敛到"类型构造时一次"。

## v3.x 已修复问题

### v3.5 已修复 findings（WP01-WP14，14 项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| WP01 | High | `build_pipe_security_attributes` 中 `OpenProcessToken` 成功后 `token_handle` 未 `CloseHandle`，每次调用泄漏一个句柄 | ✅ 已修复（fix-v35-high-findings-2026-07-12）— `OwnedHandle` RAII guard — ⚠️ v4.0 WP-005 发现 token_user 转换未校验 buffer 长度 |
| WP02 | High | `ipc_thread` 中 `reader.read_line` 无读取上限，`MAX_LINE_BYTES` 检查在读取后，无法防止 OOM | ✅ 已修复（fix-v35-high-findings-2026-07-12）— `read_line_with_limit` 增量读取 |
| WP03 | High | WebView2 创建失败时 `controller = None`，子进程仍启动 IPC 服务继续运行（degraded 状态），父进程无感知 | ✅ 已修复（fix-v35-high-findings-2026-07-12）— 失败时 `exit(1)` 退出子进程 — ⚠️ v4.0 WP-002 发现失败路径未显式 `controller.Close()` |
| WP04 | Medium | WebView2 回调中 `error_code?` 提前返回 Err 时 `tx.send` 不执行，`rx` 永远收不到结果，等满 30s 超时 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— `error_code?` 前先 `tx.send(Err(...))` |
| WP05 | Medium | Pause/Resume 命令在 `CoreWebView2()` 返回 Err 时静默跳过 JS 注入，返回 `Ok` | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 返回 `ResponseStatus::Error` — ⚠️ v4.0 WP-001 发现 Pause/Resume 代码大量重复 |
| WP06 | Medium | SetPosition 中 `SetWindowPos`/`GetClientRect`/`SetBounds` 任一失败时仅 warn，响应仍为 `Ok` | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— `SetWindowPos` 失败返回 Error — ⚠️ v4.0 WP-003 发现 `GetClientRect`/`SetBounds` 失败仍仅 warn 返回 Ok |
| WP07 | Medium | `ComGuard::new` 在 `RPC_E_CHANGED_MODE` 时仅 warn 后返回 `Ok(initialized=false)`，继续执行 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 返回 `Err` 退出子进程 — ⚠️ v4.0 WP-012 发现 `is_initialized` 沦为 dead code |
| WP08 | Medium | `create_webview` 中 `GetClientRect` 失败时使用 `RECT::default()`（0,0,0,0），WebView2 边界为 0x0 不可见 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 使用 `cli.rect` 尺寸 — ⚠️ v4.0 WP-003 发现失败时仍返回 Ok 而非 Error |
| WP09 | Medium | `ipc_thread` 中 `PostMessageW(WM_WEB_COMMAND)` 失败时仅 warn，IPC 线程仍 `recv_timeout(30s)` 等待 | ✅ 已修复（fix-v35-medium-findings-2026-07-13）— 失败时构造错误响应发送给父进程 — ⚠️ v4.0 WP-009 发现回退字符串丢失 request_id |
| WP10 | Low | Terminate 命令 `break` 后 `WM_DESTROY` 不被分发，`PostQuitMessage(0)` 不执行 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— break 前显式 `PostQuitMessage(0)` |
| WP11 | Low | `wait_with_pump_timeout` 中 `Sleep(1)` 注释说"延迟 ~1ms"，实际 Windows 默认时钟粒度约 15.6ms | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 注释更新为 ~15ms |
| WP12 | Low | `ipc_thread` 的 read_line 重试逻辑中，重试前未 `line.clear()`，`line` 含混合半截数据 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 重试前 `line.clear()` |
| WP13 | Low | `ShowWindow(hwnd, SW_SHOW)` 在 `create_webview` 之前执行，WebView2 创建失败时窗口已可见但无内容 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— `ShowWindow` 移到 `create_webview` 成功后 |
| WP14 | Low | `default_rect` 使用 `GetSystemMetrics` 获取主显示器尺寸，多显示器场景下副显示器尺寸不匹配 | ✅ 已修复（fix-v35-low-findings-2026-07-13）— 文档注释"仅用于未传 --rect 的回退场景" — ⚠️ v4.0 WP-010 发现 1920x1080 仍为硬编码魔法数 |

### v1.0~v3.4 已修复问题（8 项）

| 问题 | 状态 | 修复说明 |
|------|------|---------|
| Pause/Resume 空操作 | ✅ 已修复 | JS 注入暂停/恢复 video/audio 媒体 |
| 错误处理使用 panic/exit | ✅ 已修复 | 全文无 panic!/process::exit/expect，使用 return + 清理 |
| 未使用 tokio 依赖 | ✅ 已修复 | Cargo.toml 已移除 tokio |
| create_webview 无超时 | ✅ 已修复 | `rx.recv_timeout` + `wait_with_pump_timeout`（30s 超时） |
| ipc_thread 无重试 | ✅ 已修复 | JSON 反序列化失败改为 error + continue，不再终止进程 |
| WebView2 Controller 未 Close | ✅ 已修复 | 显式调用 `ctrl.Close()` |
| Play/Navigate 重复 | ✅ 已修复 | 提取 `navigate_to_url` 共享函数 |
| build_url 反斜杠替换脆弱 | ✅ 已修复 | 重写 build_url，剥离 `\\?\` 前缀、处理 UNC 路径、添加 `percent_encode_path` — ⚠️ v4.0 WP-011 发现 percent_encode_path 手写实现可改进 |

## 优化目标与方案

### v4.0 优先修复（High，3 项）

1. **WP-001 Pause/Resume 代码重复**：提取共享 helper 函数 `execute_script_and_report(controller, script, op_name, request_id)`，将差异点（脚本、操作名）参数化，消除约 70 行重复代码。
2. **WP-002 create_webview 失败路径未 Close**：在 controller 创建成功后的失败路径显式调用 `controller.Close()`，或用 `ControllerGuard` RAII 包装确保 `Close()` 被调用。
3. **WP-003 SetPosition 错误处理不一致**：`GetClientRect`/`SetBounds` 失败时返回 `ResponseStatus::Error`，与 `SetWindowPos` 失败处理一致，让父进程感知边界同步失败。

### v4.0 系统性修复（Medium，4 项）

4. **WP-004 安全降级可观测性**：`build_pipe_security_attributes` 失败降级时记录 warn 日志，说明管道使用默认安全描述符。
5. **WP-005 token_user buffer 长度校验**：添加 `buffer.len() < size_of::<TOKEN_USER>()` 防御性校验 + `IsValidSid` 验证。
6. **WP-006 Play/Navigate 导航完成监听**：注册 `NavigationCompletedEventHandler`，导航完成后再返回响应（含超时），或文档明确"Ok 仅表示导航已启动"。
7. **WP-007 消息泵 Unicode 统一**：`PeekMessageA`/`DispatchMessageA` 改为 `PeekMessageW`/`DispatchMessageW`，与主循环一致。

### v4.0 收尾修复（Medium，1 项 — Wave 2I）

8. **[New]-11.3 `execute_script_and_report` 可扩展性**：Wave 1 WP-001 修复提取了 `execute_script_and_report` 公共函数，但仅 Pause/Resume 复用，其他命令（Play/Terminate/SetPosition/Navigate）仍直接构造 `WpProcResponse` 字面量，重复且不一致。Wave 2I 提取 `ok_response(request_id)` 与 `error_response(request_id, error: impl Into<String>)` 两个模块级私有辅助函数，重构 `handle_command` 5 个分支与 `execute_script_and_report` 内部共 8 处字面量。`impl Into<String>` 泛型同时支持 `&str`/`String`/`format!(...)` 结果，避免调用方显式 `.to_string()`。新增 3 个单元测试覆盖辅助函数（含 `format!` 结果完整性验证）。**修复状态**：✅ 已修复于 v4.0 Wave 2I

### v4.0 渐进优化（Low，6 项）

8-13. `std::process::exit` 改为 `return Err`（WP-008）、PostMessageW 回退字符串保留 request_id（WP-009）、`default_rect` 魔法数提取常量（WP-010）、`percent_encode_path` 改用标准 crate 或避免 `format!` 分配（WP-011）、`is_initialized` dead code 清理（WP-012）、HWND 跨线程封装 `SendHwnd` 类型（WP-013）。
