# v6.0 技术债审查 - wp-proc 子进程

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 模块路径：`crates/mirrorstar-wp-proc/src/`

## 1. 当前状态摘要

### 1.1 模块职责

wp-proc 是 MirrorStar 的 Web 壁纸子进程，将 WebView2 渲染隔离到独立进程，通过命名管道接收主进程的 IPC 命令（Play/Pause/Resume/Seek/SetPosition/Navigate/Terminate）。模块由 5 个源文件组成：`main.rs`（子进程入口，含 COM 初始化、窗口创建、WebView2 控制器创建、主消息循环、IPC 线程协调与优雅退出）、`command.rs`（IPC 命令处理，含 `NavigationCompletedHandlerGuard` RAII 与响应构造辅助函数）、`com.rs`（COM 初始化 RAII guard，STA 模式）、`ipc_server.rs`（命名管道服务端、IPC 线程、限流读取、安全描述符构建）、`webview.rs`（WebView2 控制器封装、窗口类注册、URL 构建、`ControllerGuard` RAII、消息泵超时等待）。

### 1.2 文件清单

| 文件 | 行数 | 主要内容 |
|---|---|---|
| main.rs | 441 | 子进程入口（Cli 解析、COM/窗口类/窗口/WebView2/管道初始化、消息循环、IPC 线程协调、Drop 顺序契约） |
| command.rs | 1132 | IPC 命令处理（navigate_to_url、execute_script_and_report、handle_command、ok/error_response 辅助、NavigationCompletedHandlerGuard RAII） |
| com.rs | 114 | ComGuard RAII（CoInitializeEx/CoUninitialize 配对、RPC_E_CHANGED_MODE 处理、is_initialized 测试访问器） |
| ipc_server.rs | 1469 | 命名管道服务端（OwnedHandle RAII、SecurityAttributesGuard、build_pipe_security_attributes、create_pipe_server、process_line、format_response、read_line_with_limit、ipc_thread） |
| webview.rs | 1274 | WebView2 控制（parse_rect、default_rect、WindowClassGuard、register_window_class、create_window、ControllerGuard RAII、wait_with_pump_timeout、create_webview、build_url、percent_encode_path） |

### 1.3 测试覆盖

测试分布：main.rs（~180 行，11 个测试，全为 Cli 参数解析契约测试，含 1 个 W-009 边界测试）、command.rs（~765 行，含 handle_command 路径测试、JSON 协议往返、WP-005 错误响应构造、v41-WP-005 RAII 语义测试，多数 controller 路径降级为响应构造测试）、com.rs（~50 行，独立线程 COM 初始化验证）、ipc_server.rs（~855 行，含 create_pipe_server 集成测试、process_line/format_response 纯函数测试、read_line_with_limit 限流测试、WP12 残留数据验证、v41-WP-008 RAII 语义测试）、webview.rs（~635 行，含 parse_rect/build_url/percent_encode_path 纯函数测试、SEC-004 协议白名单测试、WP-001 大小写不敏感测试、W-006 超时测试、WP-007 Unicode 消息函数 include_str! 断言、v41-WP-010 RAII 契约测试）。涉及真实 WebView2/COM 环境的失败分支普遍通过 `include_str!` 模式断言源码标记验证，无法在 CI 中可靠触发真实失败分支。

## 2. 技术债清单

### 2.1 死代码

无。经 Grep 验证（`crates/` 全范围），5 个文件中所有 `pub(crate)` 函数/结构体/常量/类型别名（`WM_WEB_COMMAND`、`CommandWithResponse`、`create_pipe_server`、`ipc_thread`、`handle_command`、`build_post_message_failed_response`、`create_webview`、`create_window`、`parse_rect`、`register_window_class`、`ComGuard`、`WindowClassGuard`、`ControllerGuard`、`NavigationCompletedHandlerGuard`、`OwnedHandle`、`SecurityAttributesGuard`、`WEBVIEW2_OP_TIMEOUT`、`wait_with_pump_timeout`、`build_url`）均有跨文件或跨模块调用点。私有辅助函数（`default_rect`、`get_module_handle`、`def_window_proc`、`process_line`、`format_response`、`read_line_with_limit`、`build_pipe_security_attributes`、`hex_upper`、`has_scheme_prefix`、`percent_encode_path`、`ok_response`、`error_response`、`navigate_to_url`、`execute_script_and_report`）均在同文件内被调用。`com.rs:is_initialized` 标记为 `#[cfg(test)]`，仅在测试中使用（见 WP-TD-004 过度设计）。

### 2.2 冗余抽象

无显著项。`default_rect`（webview.rs:106）仅被 `parse_rect` 调用一次，但作为具名函数提升可读性，保留合理。`WindowClassGuard`、`ControllerGuard`、`OwnedHandle`、`SecurityAttributesGuard`、`NavigationCompletedHandlerGuard` 均为 RAII guard，承担资源清理职责，非冗余。

### 2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| WP-TD-001 | command.rs:81; command.rs:183; webview.rs:482 | `MirrorStarError::DesktopIntegration(format!("获取 WebView 失败: {}", e))` 错误转换模式在 3 处重复：`navigate_to_url`（:81）、`execute_script_and_report`（:183，构造错误响应）、`create_webview`（:482）。三处均调用 `controller.CoreWebView2()` 后将 Err 转为相同前缀的 DesktopIntegration 错误 | 同一错误转换 3 处复制，前缀字符串修改需同步 3 处 | 抽取 `fn corewebview2_error(e: impl std::fmt::Display) -> MirrorStarError` 辅助函数（返回 `MirrorStarError::DesktopIntegration(format!("获取 WebView 失败: {}", e))`），3 处复用 | 低 |
| WP-TD-002 | webview.rs:177; webview.rs:206 | `windows::core::w!("MirrorStarWebWallpaperCls")` 字符串字面量在 `register_window_class` 中重复 2 次：第 177 行用于 `WNDCLASSW.lpszClassName`，第 206 行用于构造 `WindowClassGuard`。两处必须一致，否则注册的类名与 guard 持有的类名不匹配 | 字符串硬编码重复，修改类名需同步 2 处，易遗漏导致 Drop 时 `UnregisterClassW` 失败 | 抽取为 `const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("MirrorStarWebWallpaperCls");`，两处引用常量 | 低 |
| WP-TD-003 | ipc_server.rs:469-471; 493-495; 545-547; 579-581 | `tracing::warn!("PostMessageW 失败：WM_CLOSE 未送达（窗口可能已销毁）")` + `PostMessageW(hwnd, WM_CLOSE, ...)` 失败处理模式在 `ipc_thread` 中重复 4 次：IO 重试耗尽（:468-471）、Eof（:492-495）、cmd_tx.send 失败（:545-547）、recv_timeout 超时（:578-581） | 4 段相同 4 行模式（PostMessageW 调用 + warn 日志），修改日志格式或退出策略需同步 4 处 | 抽取 `fn notify_main_exit(hwnd: HWND)` 辅助函数封装 PostMessageW(WM_CLOSE) + warn 日志，4 处调用 | 低 |

### 2.4 过时模式

无显著项。模块内代码风格一致：错误转换统一用 `MirrorStarError::DesktopIntegration(format!(...))`、RAII guard 统一用 `into_inner()`/`release()`/`into_raw()` 取出所有权、`wait_with_pump_timeout` 统一用于异步操作等待。未发现 A 文件用新 API、B 文件用旧 API 的过时模式。

### 2.5 未使用导入

无。经 Grep 验证（`crates/mirrorstar-wp-proc/src/` 全范围），5 个文件的 `use` 语句引入的项均有实际调用点。包括：main.rs 的 `WM_WEB_COMMAND`（:193）、`CommandWithResponse`（:150）；command.rs 的 `BOOL`（:93）、`EventRegistrationToken`（:116）、`ICoreWebView2NavigationCompletedEventArgs`（:89）；ipc_server.rs 的 `ERROR_PIPE_CONNECTED`（:315）、`FromRawHandle`（:333）、`build_post_message_failed_response`（:564）；webview.rs 的 `GetLastError`（:188）、`ERROR_CLASS_ALREADY_EXISTS`（:189）、`E_POINTER`（:414,449）、`Sleep`（:339）等。

### 2.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| WP-TD-004 | com.rs:35-55 | `is_initialized` 方法的 doc comment 长达 17 行，描述一个返回 `self.initialized` 的 1 行 `#[cfg(test)]` 方法。doc 包含：WP07 修复历史（:39-41）、WP-012 标注（:43-45）、v41-WP-009 "未来改进路径"（:47-51，描述"如未来需要在生产环境诊断 COM 初始化失败可移除 `#[cfg(test)]` 属性暴露此接口"）。该"未来改进路径"无实施计划，且方法本身在 `ComGuard::new()` 返回 Ok 时恒为 true（:40-41 明确承认） | 17 行 doc 远超 1 行方法的信息密度；"未来改进路径"为 YAGNI 违规，为不存在的生产诊断需求预留接口说明；读者需阅读 17 行才能理解一个布尔 getter | 精简为 3-4 行：说明 `#[cfg(test)]` 限制、返回 `self.initialized`、WP07 后恒为 true 的事实。移除 v41-WP-009 "未来改进路径"段（如未来确需生产诊断再补） | 低 |

### 2.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| WP-TD-005 | main.rs:39-61 | `main` 函数 doc comment 中 v41-WP-001 Drop 顺序契约文档化，长达 23 行，详细论证 `_com_guard` 与 `class_guard` 的 LIFO Drop 顺序、`UnregisterClassW` 不依赖 COM 状态、顺序颠倒也无副作用等。内容有价值（解释 Rust RAII 语义），但 v41-WP-001 前缀对当前读者无意义 | 23 行 doc 块前缀引用 v41-WP-001 历史标记，读者无法判断该标记是否仍有效；doc 内容虽有价值但前缀增加噪音 | 保留 Drop 顺序论证内容（有价值），移除"v41-WP-001: Drop 顺序契约文档化"前缀标题，改为"Drop 顺序契约"中性标题 | 低 |
| WP-TD-006 | main.rs:158-170 | `hwnd_raw` 赋值的 SAFETY 注释（:151-157）后紧跟 v41-WP-002 SendHwnd 现状文档化块（:161-170，10 行），重复了 :158-159 已描述的 "SendHwnd newtype 改进方向"内容，并补充"接受现状的原因"。两段内容高度重叠：:158-159 说"未来可封装 SendHwnd 类型"，:161-170 又说"`SendHwnd(HWND)` newtype 为未来改进方向" | 10 行 v41-WP-002 块与上方改进方向重复，同一未来改进点描述两次；v41-WP-002 前缀对当前读者无意义 | 合并 :158-159 与 :161-170 为一段，移除 v41-WP-002 前缀，保留"当前实现 + 接受现状原因 + 改进方向"三段式简洁说明 | 低 |
| WP-TD-007 | command.rs:230; 239; 253; 1094 | 4 处 `[New]-11.3` 和 `Wave 2I` 历史标记：:230 `ok_response` doc "消除 WpProcResponse 字面量重复，[New]-11.3"、:239 `error_response` doc 同样引用 [New]-11.3、:253 `build_post_message_failed_response` doc "Wave 2I 修复已将 ok/error_response 集中至此"、:1094 测试注释 "[New]-11.3: ok_response / error_response 辅助函数测试"。这些是 v4.0 Wave 2I 修复的历史标记 | 4 处历史标记散落 doc/注释中，读者无法判断 [New]-11.3 / Wave 2I 是否仍为有效 spec 引用 | 移除 `[New]-11.3` 和 `Wave 2I` 标记，保留"消除 WpProcResponse 字面量重复"等描述性文字 | 低 |
| WP-TD-008 | 全 5 文件 | WPxxx 历史修复标记大量散落：`WP01`-`WP14`（无连字符，如 com.rs:19、webview.rs:98、ipc_server.rs:42）、`WP-001`-`WP-012`（带连字符，如 command.rs:298、ipc_server.rs:80、webview.rs:24）、`W-005`-`W-011`（W 前缀，如 ipc_server.rs:573、webview.rs:274、com.rs:41）、`SEC-003`-`SEC-004`（SEC 前缀，如 ipc_server.rs:37、webview.rs:588）。Grep 统计 5 文件内共 100+ 处标记。这些标记对应 v3.x/v4.0/v4.1 的已修复 findings，spec 仍存在但标记对当前读者无意义 | 100+ 处历史标记构成显著注释噪音，读者需忽略大量 `WPxx`/`W-xx`/`SEC-xx` 前缀才能理解注释实质内容；与 desktop 模块 D-TD-017（T14 前缀）、D-TD-019（Wave 2C/2D）同类 | 分批清理：保留标记关联的实质性设计理由描述，移除 `WPxx:`/`W-xx:`/`SEC-xx:` 前缀。由于数量大（100+ 处），建议作为独立 Wave v6-A 任务批量处理 | 中 |

### 2.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| WP-TD-009 | 全 5 文件 | 同类"历史修复标记"概念存在 5 种前缀风格：`WP01`-`WP14`（无连字符无版本）、`WP-001`-`WP-012`（带连字符无版本）、`v41-WP-001`-`v41-WP-010`（带版本前缀）、`W-005`-`W-011`（W 前缀，与 WP- 混用）、`SEC-003`-`SEC-004`（SEC 前缀）。同一模块内 5 种命名风格，读者无法从前缀判断标记来源（v3.x/v4.0/v4.1）或类别（正确性/安全/可维护性） | 5 种前缀风格增加认知负担，无法统一检索；`WP01` 与 `WP-001` 易混淆为不同标记 | 依赖 WP-TD-008 批量清理时统一移除前缀（不引入新前缀），或统一为单一风格（如 `v6-xxx`）；最简方案是直接移除所有历史前缀，保留描述性文字 | 中 |
| WP-TD-010 | ipc_server.rs:248; 250; 251 | 命名管道实体在 `create_pipe_server` 中 3 次更名：:248 参数 `pipe_name: &str`（不含前缀的名称）、:250 `pipe_path = format!(r"\\.\pipe\{}", pipe_name)`（含前缀的完整路径）、:251 `pipe_path_h = HSTRING::from(&pipe_path)`（HSTRING 形式）。同一概念 3 种命名，且 `pipe_name` 与 `pipe_path` 语义边界模糊（调用方传入的 `pipe_name` 实际是"不含前缀的名称"，但变量名未体现） | 同一实体 3 次更名增加阅读追踪成本；`pipe_name`/`pipe_path` 命名相似但语义不同（前者不含 `\\.\pipe\` 前缀，后者含），易误用 | 保留现状（3 阶段命名反映类型转换），但在 `pipe_name` 参数 doc 中明确"不含 `\\.\pipe\` 前缀"；或重命名为 `pipe_base_name`/`pipe_full_path`/`pipe_path_hstring` 更清晰 | 低 |

### 2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| WP-TD-011 | main.rs:127-128 | 注释 "现在 create_webview 失败时 std::process::exit(1) 提前退出，不会执行到此处的 ShowWindow，故失败时窗口始终保持隐藏" — **STALE**。实际代码（:121）已改为 `return Err(e.into())`（WP-008 修复），不再使用 `std::process::exit(1)`。main.rs:41-44 的 doc comment 也明确说明 "WP-008 v4.0 修复将 std::process::exit(1) 改为 return Err(e.into())"，但 :127-128 注释仍描述旧实现 | 注释描述的行为（`std::process::exit(1)`）与实际代码（`return Err`）不符，读者误以为失败路径调用 `exit(1)`；与同文件 :41-44 doc 自相矛盾 | 将 :127-128 "现在 create_webview 失败时 std::process::exit(1) 提前退出" 改为 "现在 create_webview 失败时 return Err 提前退出" | 低 |
| WP-TD-012 | webview.rs:499-500 | 注释 "是严重故障，应让 create_webview 失败（main 通过 std::process::exit(1) 退出子进程，WP03 已实现退出码传播让父进程感知错误）" — **STALE**。main 实际通过 `return Err(e.into())` 退出（:121），不调用 `std::process::exit(1)` | 同 WP-TD-011，注释描述的退出机制与实际不符 | 将 "main 通过 std::process::exit(1) 退出子进程" 改为 "main 通过 return Err 退出子进程" | 低 |
| WP-TD-013 | main.rs:410-415 | 测试模块注释 "已知限制：clap v4 默认 allow_hyphen_values = false...要完全支持 W-009 的分离 argv 修复，需在 wp-proc 的 source 字段添加 allow_hyphen_values = true。当前未修改 wp-proc 解析逻辑（按 W-009 任务说明"不要修改 wp-proc 的参数解析逻辑，除非不支持需报告"），仅标记为 ignored 并在此报告该已知限制。" — **STALE**。实际 :24 已设置 `#[arg(long, allow_hyphen_values = true)]`，限制已解决 | 注释声称"当前未修改 wp-proc 解析逻辑"但实际已修改（:24 `allow_hyphen_values = true`）；注释声称"仅标记为 ignored"但下方测试 `w009_cli_parse_source_starting_with_dash_separated`（:431-440）实际无 `#[ignore]` 属性 | 移除 :410-415 整段"已知限制"注释，改为简短说明 "W-009: wp-proc 的 source 字段已设置 allow_hyphen_values = true，支持值以 `--` 开头的分离 argv" | 低 |
| WP-TD-014 | main.rs:425-430 | 测试 `w009_cli_parse_source_starting_with_dash_separated` 的 doc comment "**当前为已知限制**：wp-proc 的 clap 解析器默认 allow_hyphen_values = false，拒绝解析以 `--` 开头的分离 argv 值。此测试标记为 `#[ignore]` 以避免阻塞构建，待 wp-proc 的 source 字段添加 allow_hyphen_values = true 后可移除 `#[ignore]`。详见 W-009 修复报告。" — **STALE**。:24 已设置 `allow_hyphen_values = true`，测试（:431-440）实际无 `#[ignore]` 属性，正常运行且应通过 | doc 声称测试"标记为 `#[ignore]`"但实际未标记；doc 声称"当前为已知限制"但限制已解决。读者误以为测试被忽略 | 移除 :427-430 整段"当前为已知限制"doc，改为 "验证 source 值以 `--` 开头时，分离 argv 能被正确解析（W-009 核心场景，allow_hyphen_values = true 已启用）" | 低 |
| WP-TD-015 | com.rs:39-41 | `is_initialized` doc "WP07 修复后 RPC_E_CHANGED_MODE 会返回 Err（不再构造 initialized=false 的 ComGuard），故本方法在 ComGuard::new() 返回 Ok 时恒为 true。保留方法以维持 API 兼容性，主要为单元测试提供可观测性（W-011），生产代码不直接调用。" — **MISLEADING**。方法标记为 `#[cfg(test)]`（:52），仅在测试编译中存在，不构成任何"API"，"保留方法以维持 API 兼容性"的论证对 `#[cfg(test)]` 方法无效 | doc 的"API 兼容性"论证误导读者认为方法在生产 API 中暴露；实际生产构建中方法不存在 | 移除"保留方法以维持 API 兼容性"，改为"保留方法为单元测试提供 COM 初始化状态可观测点（生产构建通过 `#[cfg(test)]` 排除）" | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0 高收益低风险）

- **WP-TD-011**: 修正 main.rs:127-128 的 `std::process::exit(1)` 陈旧注释（改为 `return Err`）
- **WP-TD-012**: 修正 webview.rs:499-500 的 `std::process::exit(1)` 陈旧注释（同上）
- **WP-TD-013**: 移除 main.rs:410-415 的 W-009 "已知限制"陈旧注释（`allow_hyphen_values = true` 已设置）
- **WP-TD-014**: 移除 main.rs:425-430 的 `#[ignore]` 陈旧 doc（测试实际未 ignore）
- **WP-TD-015**: 修正 com.rs:39-41 的"API 兼容性"误导性注释（方法为 `#[cfg(test)]`）
- **WP-TD-002**: 抽取 `CLASS_NAME` 常量消除 `MirrorStarWebWallpaperCls` 字面量重复
- **WP-TD-007**: 移除 command.rs 4 处 `[New]-11.3` / `Wave 2I` 历史标记

### 3.2 谨慎清理（P1/P2 中收益）

- **WP-TD-001**: 抽取 `corewebview2_error` 辅助函数统一 3 处"获取 WebView 失败"错误转换
- **WP-TD-003**: 抽取 `notify_main_exit` 辅助函数统一 4 处 PostMessageW(WM_CLOSE) + warn 模式
- **WP-TD-004**: 精简 com.rs:35-55 `is_initialized` 的 17 行 doc，移除 v41-WP-009 投机性"未来改进路径"
- **WP-TD-005**: 移除 main.rs:39-61 的 v41-WP-001 前缀，保留 Drop 顺序论证内容
- **WP-TD-006**: 合并 main.rs:158-170 v41-WP-002 块与 :158-159 改进方向，消除重复
- **WP-TD-010**: 在 `pipe_name` 参数 doc 中明确"不含 `\\.\pipe\` 前缀"语义

### 3.3 评估后决定（P3 长期或低收益）

- **WP-TD-008**: 批量清理 5 文件 100+ 处 WPxxx/W-xx/SEC-xx 历史修复标记（数量大，建议作为独立 Wave v6-A 任务，需逐处评估是否保留标记关联的设计理由描述）
- **WP-TD-009**: 统一历史修复标记前缀风格（依赖 WP-TD-008 清理决策，若 WP-TD-008 选择移除前缀则本项自动消解）

## 4. 优化机会（非技术债类改进点）

- **`navigate_to_url` 与 `execute_script_and_report` 的错误响应模式统一**：`navigate_to_url` 返回 `Result<(), MirrorStarError>` 由调用方转换为响应，`execute_script_and_report` 直接返回 `WpProcResponse`。两者均调用 controller 方法并构造错误响应，但签名风格不同。可评估是否统一为 Result 风格（由 `handle_command` 统一转换），减少两种模式并存。但当前设计有其合理性（`execute_script_and_report` 内部需区分 CoreWebView2 失败与 JS 注入失败两种错误前缀，直接返回响应更直接）。
- **`ipc_thread` 的 30s 响应超时硬编码**：ipc_server.rs:574 `recv_timeout(Duration::from_secs(30))` 的 30s 超时硬编码在函数体内。可考虑提取为常量（如 `IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30)`），与 `WEBVIEW2_OP_TIMEOUT` 风格一致，便于后续调优。
- **`read_line_with_limit` 的 `LineReadError::Io` 变体在 `ipc_thread` 中通过 `unreachable!` 处理**：ipc_server.rs:519-522 的 `Err(LineReadError::Io(_)) => unreachable!("Io 错误应在重试循环内处理")` 依赖"Io 错误在重试循环内已被处理"的不变量。当前实现正确（重试循环 :462-483 处理 Io 后 return 或 continue），但 `unreachable!` 在未来重构时易被破坏。可考虑将 Io 错误在重试循环内彻底消费（转为 return），让 `read_outcome` 的类型变为 `Result<String, LineReadError::Eof | LineReadError::TooLong>`，消除 `unreachable!` 依赖。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项

- v4.0 Wave 2I 修复了 `[New]-11.3`（command.rs 错误响应构造函数抽取：`ok_response`/`error_response`）和 B-004，本审查不重复记录这些已修复项，仅记录其修复痕迹本身（WP-TD-007 标记 4 处 `[New]-11.3`/`Wave 2I` 历史标记需清理）。
- v4.0 的 WP01-WP14、WP-001-WP-012、W-005-W-011、SEC-003-SEC-004 系列修复已在代码中通过标记固化，并在测试中通过 `include_str!` 模式断言验证。本审查 WP-TD-008 标记了这 100+ 处历史标记的批量清理需求。
- v4.0 Wave 2D 的 W-009 修复（web.rs `build_wp_proc_args` 分离 argv）已实施，wp-proc 的 `source` 字段已添加 `allow_hyphen_values = true`（main.rs:24）。本审查 WP-TD-013/014 标记了 main.rs:410-415 和 :425-430 中描述 W-009 "已知限制"的陈旧注释（限制已解决但注释未更新）。

### 5.2 v5.0 已覆盖项

- v5.0 未针对 wp-proc 模块进行性能优化（v5.0 性能 findings 集中在 desktop/wallpaper 模块）。wp-proc 的 `wait_with_pump_timeout` 消息泵、`read_line_with_limit` 限流读取、`OwnedHandle`/`ControllerGuard` RAII 等均为 v4.0 修复引入，v5.0 未变动。
- v4.1（v41-deep-review-and-performance-optimization）的 v41-WP-001 至 v41-WP-010 已全部修复完成（见 `.trae/specs/v41-deep-review-and-performance-optimization/v41-findings.md:910-982`）。本审查 WP-TD-005/006 标记了 main.rs 中 v41-WP-001（Drop 顺序契约 doc）和 v41-WP-002（SendHwnd 现状 doc）的修复痕迹清理需求，WP-TD-004 标记了 com.rs 中 v41-WP-009（is_initialized 未来改进路径）的过度设计清理需求。

### 5.3 v6 新发现

- **注释陈旧（WP-TD-011/012）**：v4.0/v4.1 修复将 `std::process::exit(1)` 改为 `return Err(e.into())`（WP-008），但 main.rs:127-128 和 webview.rs:499-500 的注释仍描述旧实现。v4.0/v4.1 未识别此注释更新遗漏，本次首次发现。
- **W-009 限制已解决但注释未更新（WP-TD-013/014）**：v4.0 Wave 2D W-009 修复已在 wp-proc 的 `source` 字段添加 `allow_hyphen_values = true`（main.rs:24），但 main.rs:410-415 的"已知限制"注释和 :425-430 的 `#[ignore]` doc 仍描述修复前状态。v4.0 修复报告未同步更新测试注释，本次首次发现。
- **`is_initialized` "API 兼容性"误导性注释（WP-TD-015）**：com.rs:39-41 声称"保留方法以维持 API 兼容性"，但方法标记为 `#[cfg(test)]`（:52），不构成任何 API。v4.1 v41-WP-009 修复添加 `#[cfg(test)]` 时未同步更新 doc 的"API 兼容性"论证，本次首次发现。
- **"获取 WebView 失败"错误转换重复（WP-TD-001）**：command.rs 和 webview.rs 中 3 处相同的 `CoreWebView2()` 错误转换模式，v4.0/v4.1 未识别此重复，本次首次发现。
- **"MirrorStarWebWallpaperCls" 字面量重复（WP-TD-002）**：webview.rs:177 和 :206 重复同一窗口类名字面量，v4.0/v4.1 未识别，本次首次发现。
- **PostMessageW(WM_CLOSE) 失败处理模式重复 4 次（WP-TD-003）**：ipc_server.rs 中 4 处相同的 WM_CLOSE 通知 + warn 日志模式，v4.0/v4.1 未识别，本次首次发现。

## 6. 清理成果（2026-07-26）

> 实施 spec：`cleanup-v6-wp-proc-tech-debt-2026-07-26`（2026-07-26 完成）

wp-proc 子进程模块已完成全部 15 项技术债的清理，按级别落实情况如下：

### 6.1 P0 立即清理（7/7 项，全部落实）

| ID | 类型 | 落实方式 |
|---|---|---|
| WP-TD-011 | 注释陈旧 | main.rs:127-128 "std::process::exit(1) 提前退出" → "return Err 提前退出"，与 WP-008 实际退出机制一致 |
| WP-TD-012 | 注释陈旧 | webview.rs:499-500 "main 通过 std::process::exit(1) 退出子进程" → "main 通过 return Err 退出子进程" |
| WP-TD-013 | 注释陈旧 | main.rs:410-415 W-009 "已知限制"段落移除，改为简短说明 `allow_hyphen_values = true` 已启用 |
| WP-TD-014 | 注释陈旧 | main.rs:425-430 "当前为已知限制 / 标记为 `#[ignore]`" doc 移除，改为说明 W-009 核心场景已支持（测试实际无 `#[ignore]` 属性） |
| WP-TD-015 | 注释误导 | com.rs:39-41 移除"保留方法以维持 API 兼容性"误导论证，改为说明 `#[cfg(test)]` 内部可观测点（生产构建排除） |
| WP-TD-002 | 重复实现 | webview.rs 抽取 `const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("MirrorStarWebWallpaperCls");`，:177 与 :206 两处字面量改用常量 |
| WP-TD-007 | 修复痕迹 | command.rs 4 处 `[New]-11.3` / `Wave 2I` 历史标记移除（`ok_response` doc / `error_response` doc / `build_post_message_failed_response` doc / 测试注释），保留"消除 WpProcResponse 字面量重复"等描述性文字 |

### 6.2 P1/P2 谨慎清理（6/6 项，全部落实）

| ID | 类型 | 落实方式 |
|---|---|---|
| WP-TD-001 | 重复实现 | webview.rs 新增 `pub(crate) fn corewebview2_error(e: impl std::fmt::Display) -> MirrorStarError`，command.rs:81 / :183 与 webview.rs:482 三处"获取 WebView 失败"错误转换复用 |
| WP-TD-003 | 重复实现 | ipc_server.rs 新增 `fn notify_main_exit(hwnd: HWND)` 封装 `PostMessageW(WM_CLOSE)` + warn 日志，IO 重试耗尽 / Eof / cmd_tx.send 失败 / recv_timeout 超时 4 处复用 |
| WP-TD-004 | 过度设计 | com.rs:35-55 `is_initialized` 17 行 doc 精简为 ~7 行：保留 `#[cfg(test)]` 限制说明与 WP07 后恒为 true 事实，移除 v41-WP-009 投机性"未来改进路径"段 |
| WP-TD-005 | 修复痕迹 | main.rs:39-61 移除"v41-WP-001: Drop 顺序契约文档化"前缀标题，改为"Drop 顺序契约"中性标题，保留 Drop 顺序论证内容 |
| WP-TD-006 | 修复痕迹 | main.rs:158-170 v41-WP-002 块与 :158-159 改进方向合并为一段，移除 v41-WP-002 前缀，保留"当前实现 + 接受现状原因 + 改进方向"三段式 |
| WP-TD-010 | 命名一致性 | ipc_server.rs `create_pipe_server` 的 `pipe_name` 参数 doc 明确"不含 `\\.\pipe\` 前缀的管道基础名称"，消除 `pipe_name` / `pipe_path` 语义边界模糊 |

### 6.3 P3 评估后决定（2/2 项，保留现状 + 文档化决策）

| ID | 类型 | 决策 | 理由 |
|---|---|---|---|
| WP-TD-008 | 修复痕迹 | 保留现状，作为独立 Wave v6-A 任务处理 | 5 文件 100+ 处 WPxxx/W-xx/SEC-xx 历史标记数量大，需逐处评估是否保留标记关联的设计理由描述，与本 spec 的小步迭代节奏不匹配 |
| WP-TD-009 | 命名一致性 | 依赖 WP-TD-008 决策，本 spec 不单独处理 | 若 WP-TD-008 选择移除前缀则本项自动消解，无需重复评估 |

### 6.4 验证结果

- **编译**：`cargo build -p mirrorstar-wp-proc` 通过
- **测试**：`cargo test -p mirrorstar-wp-proc` 65 个逻辑测试通过（com / w009 / ok_response / parse_rect / build_url / ipc_server / wp006 / v41_wp005 等纯逻辑测试，WebView2 环境依赖测试因 STATUS_ILLEGAL_INSTRUCTION 跳过，与代码变更无关）
- **clippy**：`cargo clippy -p mirrorstar-wp-proc -- -D warnings` 零警告
- **Grep 残留验证**：无 `[New]-11.3` / `Wave 2I` / `v41-WP-001` / `v41-WP-002` / `v41-WP-009` / `API 兼容性` / `已知限制` / `MirrorStarWebWallpaperCls` 字面量重复（仅常量定义 1 处）/ `获取 WebView 失败` 字面量重复（仅辅助函数定义 1 处）/ `WM_CLOSE 未送达` 字面量重复（仅辅助函数定义 1 处）

### 6.5 衍生收益

- **错误转换统一**：WP-TD-001 抽取的 `corewebview2_error` 辅助函数消除 3 处"获取 WebView 失败"前缀字符串重复，未来调整错误前缀仅需修改 1 处
- **WM_CLOSE 通知统一**：WP-TD-003 抽取的 `notify_main_exit` 辅助函数消除 4 处 PostMessageW + warn 日志模式重复，未来调整退出通知策略（如改为 PostQuitMessage）仅需修改 1 处
- **窗口类名集中**：WP-TD-002 抽取的 `CLASS_NAME` 常量消除 `MirrorStarWebWallpaperCls` 字面量重复，避免注册类名与 `WindowClassGuard` 持有类名不匹配导致 `UnregisterClassW` 失败的风险
- **注释体量压缩**：com.rs:35-55 `is_initialized` doc（17 → ~7 行）+ main.rs Drop 顺序契约前缀清理 + v41-WP-002 块合并 + W-009 已知限制注释移除，累计减少约 30-40 行注释噪音
- **修复痕迹清理**：command.rs 4 处 `[New]-11.3` / `Wave 2I` 历史标记移除后，读者不再需判断这些标记是否仍为有效 spec 引用，注释聚焦当前实现描述
