# desktop 模块优化文档

> [← 返回索引](./README.md)

## 模块概要

- **模块路径**：`crates/mirrorstar-core/src/desktop/`
- **审查文件**：4 个（约 1,854 行）
  - `mod.rs`（628 行）— `DesktopIntegrator`、`enumerate_displays`、`monitor_enum_callback`、DPI 获取
  - `native_wallpaper.rs`（543 行）— Windows 原生壁纸 API（注册表缩放模式 + `SystemParametersInfoW`）
  - `window.rs`（108 行）— 窗口样式工具（`make_borderless`、`remove_from_taskbar`、`set_mouse_passthrough`）
  - `worker_w.rs`（575 行）— WorkerW 发现（`find_workerw_no_retry`）、`embed_wallpaper`、系统壁纸读写
- **核心结构**：`DesktopIntegrator`（`progman_hwnd`、`workerw_hwnd`、`active_wallpapers` HashMap、`original_wallpaper`、`initialized`）
- **核心功能**：
  - WorkerW 发现：`find_workerw_no_retry` 通过 `FindWindowW(Progman)` + `EnumWindows` + `SendMessageTimeoutW(0x052C)` 三步策略查找
  - 窗口嵌入：`SetParent` 将壁纸窗口挂到 WorkerW 下
  - 原生壁纸 API：`SystemParametersInfoW` + 注册表读写
  - 多显示器枚举：`EnumDisplayMonitors` 回调收集所有显示器
  - Explorer 重启检测：`TaskbarCreated` 消息 + 5 分钟兜底轮询
  - DPI 感知：`PerMonitorV2`，支持高 DPI 显示器
- **设计模式**：`Arc<Mutex<DesktopIntegrator>>` 跨线程保护、`RegistryGuard` 事务回滚守卫、`try_find_sibling_workerw` 双向兄弟窗口查找 helper

## v4.0 审查发现（15 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/02-desktop.md`
> 严重级别分布：Critical 0 / High 2 / Medium 5 / Low 8
> 维度分布：架构 1 | 逻辑 3 | 并发 1 | 资源 1 | 错误 3 | 性能 2 | 安全 1 | 可维护性 3

### 审查重点说明

desktop 模块经过 v3.0→v3.5 共 5 轮修复（D01-D12），事务回滚契约（D01）实现完整且配有 `RegistryGuard` 守卫测试；`GetLastError` 错误码记录规范（D08）；WorkerW 兄弟窗口双向查找（D11）已统一为 `try_find_sibling_workerw`。本次审查重点：修复完整性、修复引入的新问题、PerMonitor 多显示器坐标转换、`window.rs` 错误处理契约。

### [D-001] [High] [错误] worker_w.rs:467 / 558 / mod.rs:546 — Win32 副作用测试未标记 `#[ignore]` 会污染用户系统

**描述**：多个测试在默认 `cargo test` 运行时会真实修改用户系统壁纸，但未标记 `#[ignore]`，与 `native_wallpaper.rs` 中同类注册表测试严格标记 `#[ignore]` 的约定自相矛盾：
- `worker_w.rs::restore_system_wallpaper_empty_path_returns_ok`（467 行）调用 `restore_system_wallpaper("")`，`SPI_SETDESKWALLPAPER` 收到空路径会**清除**用户当前壁纸
- `worker_w.rs::restore_system_wallpaper_returns_result_type`（551 行）同样以空路径清除壁纸
- `worker_w.rs::restore_system_wallpaper_invalid_path_returns_result`（559 行）将壁纸设为 `":::nonexistent_invalid_path:::"`，Explorer 加载时报错
- `mod.rs::desktop_integrator_restore_original_wallpaper_propagates_err`（546 行）构造 `original_wallpaper = Some(":::nonexistent_invalid_path:::")` 后调用 `restore_original_wallpaper`，同样把用户壁纸设为无效路径

这批测试在 CI/本地 `cargo test` 默认执行后，用户桌面壁纸被破坏且不会自动恢复，违反测试隔离原则。`native_wallpaper.rs` 中 `wallpaper_style_round_trip` / `rollback_on_image_failure_live` 已正确标记 `#[ignore = "需要 Windows 环境且修改真实注册表..."]`，说明项目已有规范，此处属遗漏。

**建议**：将上述 4 个测试统一标记 `#[ignore]` 并补充说明（如 `"会修改真实系统壁纸，仅本地手动运行"`）；或重构为通过 trait 抽象注入 Win32 调用以便 CI 中 mock。优先与 `native_wallpaper.rs` 既有约定对齐。

### [D-002] [High] [逻辑] worker_w.rs:268-292 — PerMonitor 分支忽略 WorkerW 原点偏移导致负坐标显示器壁纸定位错误

**描述**：`embed_wallpaper` 在 `Arrangement::PerMonitor` 分支中，找到显示器后直接使用 `display.x, display.y`（虚拟桌面屏幕坐标）作为 `SetWindowPos` 的坐标。但在 Step 3 已通过 `SetParent(wp_hwnd, workerw_hwnd)` 将壁纸窗口重定为 WorkerW 的子窗口，此时 `SetWindowPos` 的坐标是**相对 WorkerW 客户区左上角**的子窗口坐标，而非虚拟屏幕坐标。

当 WorkerW 左上角不位于虚拟屏幕原点 (0,0) 时——即存在位于主显示器左侧/上方的副显示器（其坐标为负，如 `-1920`）——会产生偏移：
- 正确子窗口坐标应为 `display.x - workerw_rect.left`、`display.y - workerw_rect.top`
- 当前代码使用 `display.x`、`display.y`，会把该显示器上的壁纸向左/上偏移 `|workerw_rect.left|` / `|workerw_rect.top|`，落到错误位置甚至虚拟屏幕外

佐证：同函数 `Span` 分支（257-266 行）与 `PerMonitor` 的 fallback 分支（281-290 行）都先读取 `workerw_rect` 再计算，唯独 `PerMonitor` 命中分支（271-277 行）忽略了 WorkerW 原点偏移，三条路径不一致。

**建议**：在 `PerMonitor` 命中分支也读取 `GetWindowRect(workerw_hwnd)`，使用 `(display.x - workerw_rect.left, display.y - workerw_rect.top, display.width, display.height)`；或将 WorkerW 原点偏移在函数开头统一计算一次供两个分支共用。需在多显示器（含负坐标）环境下回归验证。

### [D-003] [Medium] [错误] window.rs:7-40 — `make_borderless` 签名声明 `Result` 但恒返回 `Ok`

**描述**：`make_borderless` 签名声明 `-> Result<(), MirrorStarError>`，但函数体内对 `SetWindowLongPtrW`（GWL_STYLE 与 GWL_EXSTYLE 两次）的失败仅 `tracing::warn!`，从未返回 `Err`，函数恒返回 `Ok(())`。这使 `worker_w.rs::embed_wallpaper` 中的 `crate::desktop::window::make_borderless(wp_hwnd)?` 的 `?` 永远不会触发，调用方无法感知边框剥离失败。

GWL_STYLE 设置失败（例如窗口句柄在调用间失效）会导致壁纸窗口仍带标题栏/边框嵌入 WorkerW，视觉异常但不会被上层捕获为错误。

**修复状态**：✅ 已修复于 v4.0 Wave 2E

**建议**：要么在 `GetLastError() != 0` 时返回 `Err(MirrorStarError::DesktopIntegration(...))`（与已有 `SetLastError(0)` + `GetLastError` 检测模式一致），要么将签名改为 `-> ()` 并文档化"best-effort，失败仅日志"。推荐前者以保持与 `embed_wallpaper` 其他步骤（SetParent/SetWindowPos 均返回 Err）的错误传播一致性。

### [D-004] [Medium] [并发] mod.rs:122-148 — `ensure_workerw_ready` 锁内 re-embed 循环存在跨线程消息-锁依赖

**描述**：`ensure_workerw_ready` 在 Explorer 重启后的"重新初始化"场景中，会遍历 `active_wallpapers` 对每个条目调用 `worker_w::embed_wallpaper`。该调用链包含 `make_borderless`（`SetWindowLongPtrW`）、`SetParent`、`SetWindowPos`、`ShowWindow` 等 Win32 调用，且整个循环持有 `DesktopIntegrator` 的 `Mutex` 锁。

虽然注释正确地将重试 `sleep` 移出了锁外，但 re-embed 循环本身仍在锁内执行跨线程窗口操作。`SetParent`/`SetWindowPos` 对由其他线程创建的窗口会同步向目标线程消息队列发送消息；若目标渲染线程此刻正阻塞等待 desktop 锁（例如要调用 `check_and_reinitialize` 或 `embed_wallpaper`），就会形成跨线程消息-锁依赖，存在死锁或长时间阻塞其他 desktop 访问者的风险。多个活跃壁纸时锁持有时间可能从 5ms 量级放大到数百毫秒。

**修复状态**：✅ 已修复于 v4.0 Wave 2E

**建议**：评估将 re-embed 循环移出锁外（先在锁内收集 `(hwnd, display_id, arrangement)` 列表并更新句柄，释放锁后再逐个 embed）；或在 `WallpaperEngine` 层用 per-display 渲染器线程自主重嵌入来取代集中式循环。至少应在注释中明确说明锁内 re-embed 的阻塞窗口，并限定 `active_wallpapers` 规模上界。

### [D-005] [Medium] [资源] mod.rs:172-188 — `remove_wallpaper` 调用 `SetParent(hwnd, None)` 后未隐藏窗口

**描述**：`remove_wallpaper` 在窗口仍有效时调用 `SetParent(hwnd, None)` 将壁纸窗口重定向到无父窗口（即变为桌面顶层窗口或桌面子窗口，取决于 Win32 版本语义）。但函数既未 `ShowWindow(SW_HIDE)` 也未销毁窗口，仅依赖调用方后续清理。若调用方未及时销毁，原壁纸窗口会以可见的顶层窗口形式残留在桌面上（仍带 `WS_VISIBLE`），用户会看到一个无标题的浮动窗口。

此外 `SetParent(hwnd, None)` 对原为 `WS_POPUP` 的窗口语义模糊：Win32 中 `NULL` 新父会使窗口成为桌面子窗口，可能继承桌面窗口的裁剪/坐标语义，行为不可预期。

**修复状态**：✅ 已修复于 v4.0 Wave 2E

**建议**：在 `SetParent` 之后立即 `ShowWindow(hwnd, SW_HIDE)` 隐藏窗口，避免残留可见状态；或直接由调用方在 `remove_wallpaper` 返回后销毁窗口并在文档中明确所有权契约。建议复核 `WallpaperEngine::close_wallpaper` 的调用链确认窗口最终被销毁。

### [D-006] [Low] [逻辑] mod.rs:201-207 — `is_workerw_valid` 仅校验 `workerw_hwnd` 未校验 `progman_hwnd`

**描述**：`is_workerw_valid` 仅校验 `workerw_hwnd` 的有效性（`IsWindow`），未校验 `progman_hwnd`。若 Explorer 重启导致 Progman 被销毁重建但旧 WorkerW 句柄碰巧仍有效（句柄复用），`is_workerw_valid` 会返回 `true`，使 `ensure_workerw_ready` 跳过重新初始化，后续 `embed_wallpaper` 使用失效的 `progman_hwnd`（虽仅用于 `SetForegroundWindow`，best-effort，影响有限）。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：在 `is_workerw_valid` 中同时校验 `progman_hwnd`：`self.progman_hwnd.is_some_and(|h| unsafe { IsWindow(h).as_bool() }) && self.workerw_hwnd.is_some_and(...)`，保持两个句柄有效性状态一致。

### [D-007] [Medium] [错误] worker_w.rs:48 / 73 / 87 — `find_workerw_no_retry` 三次 `EnumWindows` 返回值被 `let _ =` 丢弃

**描述**：`find_workerw_no_retry` 中三次 `EnumWindows` 调用的返回值均被 `let _ =` 丢弃。`EnumWindows` 在极端情况（如 GDI 资源耗尽、回调内异常）会返回 `Err`，此时 callback 不会执行，`workerw` 保持 `None`，流程静默进入下一步或最终返回 `WorkerWNotFound`。

错误被吞掉导致：① 调用方收到 `WorkerWNotFound` 但根因是 `EnumWindows` 失败（如资源耗尽），诊断信息丢失；② 与同文件其他 API（`FindWindowW` 用 `?` 传播、`GetWindowRect` 用 `?` 传播）的错误处理风格不一致。注释虽说明"EnumWindows 失败时 callback 不会填充 workerw，下方 None 分支自然处理"，但忽略了错误码本身的价值。

**修复状态**：✅ 已修复于 v4.0 Wave 2E

**建议**：至少在 `EnumWindows` 返回 `Err` 时 `tracing::warn!(error = ?e, "EnumWindows 调用失败")` 记录错误码（注意需立即取 `GetLastError`，避免被后续 Win32 调用覆盖），与 mod.rs D08 修复的 `EnumDisplayMonitors` 失败处理风格对齐；或直接 `?` 传播并映射为 `DesktopIntegration` 错误以区分"未找到 WorkerW"与"枚举失败"。

### [D-008] [Low] [可维护性] worker_w.rs:38 — `is_invalid() || == HWND::default()` 条件语义重复

**描述**：`if progman.is_invalid() || progman == HWND::default()` 中两个条件语义重复——`HWND::is_invalid()` 内部即判断句柄为 0/默认值，`HWND::default()` 即 `HWND(0)`，二者等价。同文件 186、216、227 行也存在 `is_invalid() || == HWND::default()` 的重复模式。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：统一保留 `progman.is_invalid()` 单一判断，删除冗余的 `== HWND::default()`，减少阅读噪音并保持与 windows crate 推荐用法一致。

### [D-009] [Low] [性能] worker_w.rs:334 — `get_system_wallpaper` 栈上分配 64 KiB 缓冲接近受限栈上限

**描述**：`get_system_wallpaper` 在栈上分配 `[0u16; 32767]`（约 64 KiB）作为路径缓冲。这是 D02 修复从 `MAX_PATH(260)` 扩大而来以支持长路径，扩大本身正确。但 64 KiB 栈分配在受限栈线程（如某些 tokio worker 配置 256 KiB 栈）上接近上限；该函数会被 `refresh_desktop`、`DesktopIntegrator::new` 在初始化路径上调用。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：改为 `vec![0u16; UNICODE_STRING_MAX_CHARS]` 堆分配（一次性，影响可忽略），或使用 `MaybeUninit<[u16; 32767]>` 避免零初始化开销。当前实现对默认 8 MiB 主线程栈无问题，Low 优先级。

### [D-010] [Low] [可维护性] window.rs:1-108 — `window.rs` 三个公开函数无任何单元测试

**描述**：`window.rs` 三个公开函数 `make_borderless` / `remove_from_taskbar` / `set_mouse_passthrough` 均无任何单元测试。虽然它们与 Win32 强耦合，但同项目 `worker_w.rs` 对同类 Win32 回调（`enum_windows_callback` 等）提供了 "no panic" 烟雾测试作为最低保障，`window.rs` 缺失对应覆盖。

`set_mouse_passthrough` 的 enabled/disabled 两条分支、`remove_from_taskbar` 的 `WS_EX_APPWINDOW`/`WS_EX_TOOLWINDOW`/`WS_EX_NOACTIVATE` 位运算尤其值得有回归保护，防止未来重构破坏样式位组合。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：补充至少一个 `#[test]` 对真实 Progman 窗口调用三个函数验证不 panic（参照 `worker_w.rs::try_find_sibling_workerw_helper_no_panic` 范式），锁定样式位运算不回归。

### [D-011] [Low] [逻辑] window.rs:75-91 — `set_mouse_passthrough(false)` 保留 `WS_EX_LAYERED` 但契约未文档化

**描述**：`set_mouse_passthrough(enabled=false)` 分支仅移除 `WS_EX_TRANSPARENT`，注释说明"保守保留 `WS_EX_LAYERED`（避免影响其它绘制行为）"。但 `WS_EX_LAYERED` 窗口必须调用过 `SetLayeredWindowAttributes` 或 `UpdateLayeredWindow` 才会正常渲染；若该壁纸窗口此前从未设置分层属性，仅保留 `WS_EX_LAYERED` 而无 alpha 配置可能导致窗口不可见或渲染异常。

反之 `enabled=true` 分支会同时 OR 上 `WS_EX_LAYERED | WS_EX_TRANSPARENT`，但本函数不负责调用 `SetLayeredWindowAttributes`，隐含假设由调用方（渲染器）另行配置分层属性。该契约未在文档中说明。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：在函数文档注释中显式声明"调用方需确保窗口已通过 `SetLayeredWindowAttributes` 配置分层属性，本函数仅切换 `WS_EX_TRANSPARENT` 位"；或评估禁用穿透时一并移除 `WS_EX_LAYERED`（需确认不影响渲染器绘制路径）。

### [D-012] [Low] [可维护性] worker_w.rs:59-67 — `SendMessageTimeoutW` 超时参数 200ms 为内联魔法数

**描述**：`SendMessageTimeoutW` 的超时参数 `200`（毫秒）为内联魔法数，而同文件其他常量（`UNICODE_STRING_MAX_CHARS`、`WM_SPAWN_WORK`、`MAX_CHILD_DEPTH`、`WORKERW_CLASS`）均提取为命名常量。该 200ms 与 `compute_retry_wait_ms` 的首次 200ms 重试间隔数值相同但语义不同，易混淆。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：提取为 `const WM_SPAWN_WORK_TIMEOUT_MS: u32 = 200;` 并加注释说明"超过此值未返回则放弃等待 WorkerW 创建，进入 EnumWindows 二次查找"。

### [D-013] [Low] [性能] worker_w.rs:270 — `embed_wallpaper` PerMonitor 分支每次调用都全量枚举显示器

**描述**：`embed_wallpaper` 在 `PerMonitor` 分支每次调用都执行 `crate::desktop::enumerate_displays()`（即 `EnumDisplayMonitors` 全量枚举 + per-monitor `GetMonitorInfoW` + `GetDpiForMonitor`）。在多显示器 + 多壁纸场景下（如 3 屏 3 壁纸），嵌入流程会重复枚举 3 次。`EnumDisplayMonitors` 持有 GDI 锁并同步回调，频繁调用有一定开销。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：在 `DesktopIntegrator` 层缓存最近一次 `enumerate_displays` 结果并按短 TTL（如 5s）失效，或由调用方在批量嵌入前传入显示器列表；单次嵌入场景保持现状即可。优先级低。

### [D-014] [Medium] [架构] mod.rs:33-48 — `SAFETY` 注释声称"仅存储和传递 HWND"与实际不符

**描述**：`DesktopIntegrator` 的 `SAFETY` 注释声称："DesktopIntegrator 的方法仅存储和传递 HWND 值，实际的窗口操作由调用方在正确的线程上执行。" 但实际并非如此——`DesktopIntegrator::embed_wallpaper`（155 行）直接调用 `worker_w::embed_wallpaper`，后者执行 `make_borderless`/`SetParent`/`SetWindowPos`/`ShowWindow`/`SetForegroundWindow` 等窗口操作；`ensure_workerw_ready`（122 行）在锁内执行 re-embed 循环。这些操作发生在持有 `Mutex` 的任意线程上，而非"创建窗口的线程"。

Win32 窗口操作跨线程调用通过同步消息发送实现，技术上可行，但：① 违反"窗口操作应在创建线程执行"的最佳实践；② 注释与实现不符会误导后续维护者对线程安全边界的判断；③ 与 D-004 的死锁风险叠加。

**修复状态**：✅ 已修复于 v4.0 Wave 2E

**建议**：修订 SAFETY 注释，明确说明"窗口操作以跨线程同步消息方式执行，依赖 Win32 的消息路由机制；调用方需确保目标窗口线程可响应消息且不持有 desktop 锁"，移除"仅存储和传递 HWND 值"的不实表述。或在架构层面将窗口操作改为向渲染器线程投递任务执行。

### [D-015] [Low] [安全] native_wallpaper.rs:221-224 / worker_w.rs:361 — 路径转宽字符未校验嵌入式 NUL

**描述**：`set_wallpaper_image` 与 `restore_system_wallpaper` 将路径通过 `image_path.encode_utf16().chain(std::iter::once(0)).collect()` 转为宽字符串后传给 `SystemParametersInfoW`。Rust 的 `String` 允许包含 `'\0'`（合法 UTF-8），若 `image_path` 含嵌入式 NUL 字符，生成的宽字符串会被 Win32 的 null-terminator 约定截断，实际设置的壁纸路径短于预期，可能指向非预期文件。

输入来源为用户配置文件与壁纸库路径，非完全不可信，但缺乏输入校验。`is_native_supported` 仅检查扩展名，不校验路径合法性。

**修复状态**：✅ 已修复于 v4.0 Wave 3B

**建议**：在转宽字符前校验 `image_path` 不含 `'\0'` 与控制字符：`if image_path.contains('\0') { return Err(...) }`；或使用 `String::from_utf16` round-trip 一致性校验。优先级低，但符合防御性输入校验原则。

## v3.x 已修复问题

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| D01 | High | `set_native_wallpaper` 先写注册表缩放模式再设壁纸，失败时注册表残留 | ✅ 已修复（v3.5.1）— `RegistryGuard` 事务回滚守卫 |
| D02 | Medium | `get_system_wallpaper` 固定 260 缓冲区，长路径/UNC 路径被截断 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 D-009 发现扩大至 32767 后栈分配接近上限 |
| D03 | Medium | `set_mouse_passthrough` 修改 GWL_EXSTYLE 后未调用 `SetWindowPos(SWP_FRAMECHANGED)` 刷新 | ✅ 已修复（v3.5.2） |
| D04 | Medium | `embed_wallpaper` PerMonitor 未匹配 display_id 时静默回退无 warning | ✅ 已修复（v3.5.2） |
| D05 | Medium | `ensure_workerw_ready` 重新嵌入失败条目残留导致状态不一致 | ✅ 已修复（v3.5.2） |
| D06 | Medium | `find_workerw()` 含 10 次 sleep 重试为死代码，pub 易被误用于 async 上下文 | ✅ 已修复（v3.5.2） |
| D07 | Medium | `SetWindowLongPtrW` 返回值 0 歧义无法可靠检测失败 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 D-003 发现 `make_borderless` 仍恒返回 Ok |
| D08 | Low | `EnumDisplayMonitors`/`GetMonitorInfoW` 失败静默无日志 | ✅ 已修复（v3.5.3） |
| D09 | Low | `SendMessageTimeoutW` 的 `result` 变量 dead store | ✅ 已修复（v3.5.3） |
| D10 | Low | 魔法数字（260/0x052C/3/10）未定义为命名常量 | ✅ 已修复（v3.5.3）— ⚠️ v4.0 D-012 发现 200ms 超时仍为魔法数 |
| D11 | Low | `enum_windows_callback` 与 `fallback_enum_callback` 兄弟窗口检查逻辑不一致 | ✅ 已修复（v3.5.3）— 统一为 `try_find_sibling_workerw` |
| D12 | Low | `restore_original_wallpaper`/`restore_system_wallpaper` 返回 `()` 调用方无法感知失败 | ✅ 已修复（v3.5.3）— 返回 `Result` |
| 硬编码布局忽略用户配置 | — | `check_and_reinitialize` 从 HashMap 取实际 arrangement | ✅ 已修复（v1.0） |
| remove_wallpaper 空操作 | — | `IsWindow` 检查 + `SetParent(hwnd, None)` 分离窗口 | ✅ 已修复（v1.0）— ⚠️ v4.0 D-005 发现未隐藏窗口 |
| unsafe Send/Sync 契约脆弱 | — | 移除冗余 `unsafe impl Sync`，用 `Arc<Mutex>` 保护 | ✅ 已修复（v1.0） |
| DPI 获取失败静默默认 | — | 添加 `tracing::warn!` 日志 | ✅ 已修复（v1.0） |
| DisplayInfo 字段冗余 | — | `id` 为 device_name，`name` 为"显示器 N" | ✅ 已修复（v1.0） |

## 优化目标与方案

### v4.0 优先修复（High，2 项）

1. **D-001 测试隔离**：将 4 个会修改真实系统壁纸的测试统一标记 `#[ignore]`，与 `native_wallpaper.rs` 既有约定对齐。
2. **D-002 PerMonitor 坐标偏移**：在 `PerMonitor` 命中分支读取 `GetWindowRect(workerw_hwnd)`，使用 `(display.x - workerw_rect.left, display.y - workerw_rect.top)` 计算子窗口坐标，需在多显示器（含负坐标）环境下回归验证。

### v4.0 系统性修复（Medium，5 项）

3. **D-003 `make_borderless` 错误传播**：`GetLastError() != 0` 时返回 `Err`，与 `embed_wallpaper` 其他步骤的错误传播一致。
4. **D-004 re-embed 移出锁外**：先在锁内收集 `(hwnd, display_id, arrangement)` 列表，释放锁后再逐个 embed。
5. **D-005 `remove_wallpaper` 隐藏窗口**：`SetParent` 后立即 `ShowWindow(hwnd, SW_HIDE)`。
6. **D-007 `EnumWindows` 错误日志**：返回 `Err` 时 `tracing::warn!` 记录错误码。
7. **D-014 SAFETY 注释修订**：移除"仅存储和传递 HWND 值"的不实表述，明确跨线程同步消息机制。

### v4.0 渐进优化（Low，8 项）

8-15. `is_workerw_valid` 校验 progman（D-006）、删除冗余条件判断（D-008）、`get_system_wallpaper` 改堆分配（D-009）、`window.rs` 补充烟雾测试（D-010）、`set_mouse_passthrough` 文档化分层属性契约（D-011）、`WM_SPAWN_WORK_TIMEOUT_MS` 常量提取（D-012）、`enumerate_displays` 结果缓存（D-013）、路径 NUL 字符校验（D-015）。
