# desktop 模块优化文档

> [← 返回文档索引](../index.md)

## 1. 模块概览与现状

> 现状摘要来源：v6.0 技术债审查（2026-07-25）。本文档合并 v4.0 审查发现与 v6.0 技术债清单及清理状态。

### 1.1 模块职责

desktop 模块负责将壁纸窗口嵌入 Windows 桌面的壁纸层（旧层级为 WorkerW，位于桌面图标层之下；raised desktop 下为 Progman 的 `WS_CHILD` + `WS_EX_LAYERED` 子窗口，位于 `SHELLDLL_DefView` 之下、WorkerW 之上），并管理显示器枚举、原生壁纸设置等与 Win32 桌面环境的交互。模块分为两层：`DesktopIntegrator`（状态层，持有 progman/workerw 句柄与活跃壁纸表，提供懒加载、嵌入、移除、失效检测等高级 API）与 `worker_w` 无状态操作层（提供纯函数式 Win32 操作），后者又包含 `window`（窗口样式操作）与 `native_wallpaper`（注册表 + SystemParametersInfoW 原生壁纸）两个子模块。

核心功能与设计模式：

- WorkerW 发现：`find_workerw_no_retry` 通过 `FindWindowW(Progman)` + Progman 直查快速路径（D-PERF-002）+ `EnumWindows`（`try_enum_windows` 封装）+ `SendMessageTimeoutW(0x052C)` 多策略查找；Windows 11 raised desktop（24H2/25H2）下额外以 `FindWindowExW(Progman, …)` 探测 Progman 的直接子 WorkerW（见 1.4）
- 窗口嵌入：旧层级 `SetParent` 将壁纸窗口挂到 WorkerW 下并以 `HWND_BOTTOM` 定位；raised desktop 下改为 Progman 的 `WS_CHILD` + `WS_EX_LAYERED`（alpha=255）子窗口，`SetWindowPos` 锚定 `SHELLDLL_DefView`（位于 DefView 之下、WorkerW 之上），嵌入后调用 `ensure_workerw_zorder` 将 WorkerW 置为 Progman 最底层子窗口；PerMonitor 分支使用 `calculate_per_monitor_child_coords` 处理负坐标偏移（D-002）
- 原生壁纸 API：`SystemParametersInfoW` + 注册表读写（`RegistryGuard` 事务回滚守卫，D01）
- 多显示器枚举：`EnumDisplayMonitors` 回调收集所有显示器，带 5s TTL 缓存（D-013）
- Explorer 重启检测：`TaskbarCreated` 消息 + 5 分钟兜底轮询；`is_workerw_valid` 同时校验 progman/workerw（D-006）
- DPI 感知：`PerMonitorV2`，支持高 DPI 显示器
- 设计模式：`Arc<Mutex<DesktopIntegrator>>` 跨线程保护、`RegistryGuard` 事务回滚守卫、`try_find_sibling_workerw` 双向兄弟窗口查找 helper

### 1.2 文件清单与行数

> 行数为核验当日实际值（约 2,885 行）。v6.0 文档记录值为 mod.rs 1033 / worker_w.rs 1000 / native_wallpaper.rs 615 / window.rs 232，因 v6.0 清理后代码变更略有出入（window.rs 因新增 D-010 烟雾测试增长，native_wallpaper.rs 因 read_wallpaper_style 移除 Result 包装减少）。

| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 1027 | 模块导出与 DesktopIntegrator 状态层（懒加载/嵌入/移除/显示器缓存） |
| worker_w.rs | 987 | WorkerW 窗口查找与壁纸嵌入、系统壁纸读写、重试节奏 |
| native_wallpaper.rs | 599 | 原生壁纸设置（注册表事务回滚 + SystemParametersInfoW） |
| window.rs | 272 | 窗口样式工具函数（去边框/移除任务栏/鼠标穿透） |

### 1.3 测试覆盖

测试分布存在明显倾斜：mod.rs（~440 行测试，含大量 `include_str!` 文档化断言）、worker_w.rs（~330 行测试，含纯函数测试与 Progman 烟雾测试）、native_wallpaper.rs（~290 行测试，含注册表 round-trip 与事务回滚集成测试）、window.rs（~95 行测试，全为 `#[ignore]` Progman 烟雾测试）。涉及 Win32 API 的失败分支普遍通过 `include_str!` 模式断言源码标记验证，无法在 CI 中可靠触发真实失败分支。

### 1.4 Windows 11 raised desktop 兼容（2026-10）

Windows 11 24H2/25H2 的 raised desktop 改变了桌面层级：`Progman` 带 `WS_EX_NOREDIRECTIONBITMAP` 扩展样式，承载壁纸的 `WorkerW` 变为 `Progman` 的直接子窗口，旧层级中的顶层兄弟 `WorkerW` 不存在。模块据此新增以下兼容行为（旧层级逻辑与行为保持不变）：

- **特性检测（不判断系统版本号）**：`progman_is_raised_desktop(progman)` 通过 `GetWindowLongPtrW(Progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP` 判定；`is_raised_desktop()` 内部 `FindWindowW("Progman")` 后调用前者，找不到 Progman 时返回 `false`。以窗口样式位识别层级形态，避免依据系统 build 号误判。
- **查找顺序（按层级分流）**：`find_workerw_no_retry` 在非 raised desktop 时旧层级路径逐行不变（`SHELLDLL_DefView` 父窗口为 WorkerW → Progman 兄弟 WorkerW → `EnumWindows` → `0x052C` → 再次 `EnumWindows` → fallback）；raised desktop 下只走 `find_progman_child_workerw`（`FindWindowExW(Progman, NULL, "WorkerW", NULL)` 探测 + `is_workerw_class` 校验类名），在发送 `0x052C` **之前**探测一次、**之后**探测一次，未命中返回 `WorkerWNotFound`，并跳过 `SHELLDLL_DefView` 直查块、顶层兄弟 WorkerW 及 Step 2/4/5 的顶层 `EnumWindows`。该探测与 `SHELLDLL_DefView` 是否存在无关。
- **`0x052C` 发送参数**：`SendMessageTimeoutW(Progman, 0x052C, WPARAM(0xD), LPARAM(0x1), SMTO_NORMAL | SMTO_ABORTIFHUNG, 1000)`；参数提为 `WM_SPAWN_WORK_WPARAM` / `WM_SPAWN_WORK_LPARAM` 常量，超时提为 `WM_SPAWN_WORK_TIMEOUT_MS = 1000`（raised desktop 下 Progman 创建子 WorkerW 较慢，原 200ms 不足）。
- **嵌入模型（raised desktop，Lively/Microsoft 模型）**：壁纸窗口为 `Progman` 的 `WS_CHILD` + `WS_EX_LAYERED`（alpha=255）子窗口，`SetWindowPos` 以 `SHELLDLL_DefView` 为 `hWndInsertAfter`（插入 DefView 正下方、WorkerW 之上），**不使用 `HWND_BOTTOM`**，也不再 `SetParent` 到 Progman 的直接子 WorkerW。时序：`make_borderless` + `remove_from_taskbar` → `apply_raised_desktop_style`（`WS_CHILD` → `WS_EX_LAYERED` + `SetLayeredWindowAttributes(0, 255, LWA_ALPHA)`，**必须先于 `SetParent`**）→ `set_parent_verified(hwnd, Some(progman))` → `SetWindowPos(…, SHELLDLL_DefView, …)` → `ShowWindow(SW_SHOW)` → `ensure_workerw_zorder`。找不到 `SHELLDLL_DefView` 时直接返回 `DesktopIntegration`（含 Progman/WorkerW 句柄），不静默回退旧层级。旧层级行为不变：`SetParent(WorkerW)` + `HWND_BOTTOM`，不改 `WS_CHILD`/`WS_EX_LAYERED`。
- **`SetParent` 二义性处理**：`SetParent` 成功时返回前一个父窗口，顶层窗口的前父可能为 `NULL`，故不能以 NULL 单独判失败。`window.rs::set_parent_verified` 调用前 `SetLastError(0)`，调用后以 `GetAncestor(GA_PARENT)` 与期望父窗口比对：父链正确即成功（NULL 返回记 `setparent_ambiguous_success` 并继续）；父链不符且 `GetLastError != 0` 返回含错误码的 `DesktopIntegration`，父链不符且错误码为 0 返回"未生效且无错误码"错误。嵌入、`remove_wallpaper` 分离、纯净模式摘出与半摘出恢复均使用该封装。
- **z-order 保障**：raised desktop 下 `SHELLDLL_DefView`、壁纸窗口与 WorkerW 同为 Progman 子窗口；壁纸插在 DefView 之下，WorkerW 必须是最后一个（最底层）子窗口，否则 WorkerW 会盖住壁纸。`DesktopIntegrator::embed_wallpaper` 在嵌入成功后调用 `ensure_workerw_zorder(progman, workerw)`：用 `GetWindow(Progman, GW_CHILD)` 取首个（最上层）直接子窗口、再以 `GetWindow(首个直接子窗口, GW_HWNDLAST)` 取最底层直接子窗口（只考察直接子窗口，不递归枚举子孙窗口），若 WorkerW 不是它则 `SetWindowPos(WorkerW, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)` 置底。该函数仅对 raised desktop 生效，best-effort，失败仅 `warn`。
- **SPI 早退**：raised desktop 下 `SPI_SETDESKWALLPAPER` 会销毁承载动态壁纸的 WorkerW（参考 Lively Wallpaper 的 `RefreshDesktop` 早退实现）。因此 `refresh_desktop()` 在 raised desktop 下记 debug 日志后直接返回；`clear_native_wallpaper()` 同样记 debug 后返回 `Ok(())`，保持调用方既有成功语义（注册表原值保留不影响后续静态壁纸覆盖）。
- **失效自愈不变**：`is_workerw_valid`（同时校验 progman/workerw，D-006）与 `check_and_reinitialize` / `ensure_workerw_ready` 的重试、重嵌入循环逻辑不变；raised desktop 下重新查找会经上述 Progman 子窗口探测得到新 WorkerW，再走既有 `embed_wallpaper`（raised 下即 layered/Progman 路径）重新嵌入，并在每次嵌入后重新应用 z-order 保障。
- **影响边界**：仅涉及 WorkerW 嵌入路径的查找、层级与桌面刷新；Native 静态图片路径（`SystemParametersInfoW` + 注册表）与旧层级桌面不受影响。

**raised desktop 层级结构**：

```text
Progman（WS_EX_NOREDIRECTIONBITMAP，覆盖虚拟桌面）
├─ SHELLDLL_DefView（WS_EX_LAYERED，桌面图标层）
├─ 壁纸窗口（WS_CHILD + WS_EX_LAYERED，alpha=255；视频为 MirrorStarVideoHost 宿主） ← 插入 DefView 正下方
└─ WorkerW（Progman 的最后一个/最底层子窗口）
```

- **桌面附着判定**：`is_attached_to_wallpaper_layer(hwnd)` 用 `GetAncestor(GA_PARENT)` 判定父窗口为缓存的 WorkerW（旧层级）或 Progman（raised）且 `IsWindow(hwnd)` 为真；用于原子交换 `commit_atomic_swap` 的 commit 前校验与 `recover_half_detached_window` 的脱离检测。缓存句柄或窗口失效时返回 `false`。
- **摘出 / 移除 / 重嵌**：`remove_wallpaper` 与 `lift_wallpaper_window` 分离后调用 `restore_top_level_style` 清除 `WS_CHILD`（保留 `WS_EX_LAYERED` / alpha=255），重嵌由 `embed_wallpaper` 重新套用 raised 样式。
- **鼠标穿透（raised）**：`set_mouse_passthrough(false)` 不清除 `WS_EX_LAYERED`，并重设 `SetLayeredWindowAttributes(alpha=255)`；旧层级仍幂等清除遗留的 `WS_EX_LAYERED`。
- **视频壁纸：宿主窗口 + mpv `--wid`**：`VideoRenderer::play` 由宿主线程创建隐藏的 `MirrorStarVideoHost`（`WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`、无 `WS_VISIBLE`），mpv 以 `--wid=<宿主>` 只创建 `WS_CHILD` vo 子窗口；`renderer.hwnd()` 返回宿主，桌面嵌入/摘出/重嵌均作用于宿主。宿主线程消息循环在 `WM_SIZE`/`WM_SHOWWINDOW` 时把 mpv 子窗口拉伸到客户区。mpv 无顶层窗口 → 任务栏/Alt+Tab 不会出现 mpv，也消除了 mpv 顶层窗口 `SetParent` 的二义性。
- **mpv 参数**：`--border=no`（Lively #2415 / commit 7cc4116，修复 Win11 24H2 顶部/右侧不刷新）；`--show-in-taskbar=no` 仅作防御（随包 mpv 0.41 创建期不应用该选项），主防护是宿主 + `--wid`。

## 2. v4.0 审查发现与修复状态（15 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/02-desktop.md`
> 严重级别分布：Critical 0 / High 2 / Medium 5 / Low 8
> 维度分布：架构 1 | 逻辑 3 | 并发 1 | 资源 1 | 错误 3 | 性能 2 | 安全 1 | 可维护性 3
> **核验结论（2026-09-01）：全部 15 项已修复。** 其中 D-003 ~ D-015 在 v4.0 各 Wave 修复，D-001 / D-002（原文案待修复）经核验亦已在代码中修复（见下）。

### 审查重点说明

desktop 模块经过 v3.0→v3.5 共 5 轮修复（D01-D12），事务回滚契约（D01）实现完整且配有 `RegistryGuard` 守卫测试；`GetLastError` 错误码记录规范（D08）；WorkerW 兄弟窗口双向查找（D11）已统一为 `try_find_sibling_workerw`。本次审查重点：修复完整性、修复引入的新问题、PerMonitor 多显示器坐标转换、`window.rs` 错误处理契约。

### [D-001] [High] [错误] worker_w.rs / mod.rs — Win32 副作用测试未标记 `#[ignore]` 会污染用户系统

**描述**：多个测试在默认 `cargo test` 运行时会真实修改用户系统壁纸，但未标记 `#[ignore]`，与 `native_wallpaper.rs` 中同类注册表测试严格标记 `#[ignore]` 的约定自相矛盾：
- `worker_w.rs::restore_system_wallpaper_empty_path_returns_ok` 调用 `restore_system_wallpaper("")`，`SPI_SETDESKWALLPAPER` 收到空路径会**清除**用户当前壁纸
- `worker_w.rs::restore_system_wallpaper_returns_result_type` 同样以空路径清除壁纸
- `worker_w.rs::restore_system_wallpaper_invalid_path_returns_result` 将壁纸设为 `":::nonexistent_invalid_path:::"`，Explorer 加载时报错
- `mod.rs::desktop_integrator_restore_original_wallpaper_propagates_err` 构造 `original_wallpaper = Some(":::nonexistent_invalid_path:::")` 后调用 `restore_original_wallpaper`，同样把用户壁纸设为无效路径

**修复状态**：✅ 已修复（核验于 v4.0 之后）。上述 4 处测试现均标记 `#[ignore = "会修改真实系统壁纸，仅本地手动运行"]`（worker_w.rs:766 / 856 / 865；mod.rs:752），与 `native_wallpaper.rs` 中 `wallpaper_style_round_trip` / `rollback_on_image_failure_live` 的 `#[ignore]` 约定一致。

### [D-002] [High] [逻辑] worker_w.rs — PerMonitor 分支忽略 WorkerW 原点偏移导致负坐标显示器壁纸定位错误

**描述**：`embed_wallpaper` 在 `Arrangement::PerMonitor` 分支中，SetParent 将壁纸窗口重定为 WorkerW 子窗口后，`SetWindowPos` 的坐标应使用相对 WorkerW 客户区左上角的子窗口坐标（`display.x - workerw_rect.left`、`display.y - workerw_rect.top`），而非虚拟屏幕坐标。原实现直接使用 `display.x, display.y`，在存在负坐标副显示器时会把壁纸放错位置。

**修复状态**：✅ 已修复（核验于 v4.0 之后）。坐标计算提取为纯函数 `calculate_per_monitor_child_coords`（worker_w.rs:418），在 PerMonitor 命中分支使用 `(display.x - workerw_rect.left, display.y - workerw_rect.top, display.width, display.height)`（worker_w.rs:474）；`workerw_rect` 经 D-TD-008 在 match 前统一获取一次供三分支共用。新增 3 个针对负坐标偏移的单元测试（`d002_*`，worker_w.rs）。

### [D-003] [Medium] [错误] window.rs — `make_borderless` 签名声明 `Result` 但恒返回 `Ok`

**描述**：`make_borderless` 签名声明 `-> Result<(), MirrorStarError>`，但函数体内对 `SetWindowLongPtrW`（GWL_STYLE 与 GWL_EXSTYLE 两次）的失败仅 `tracing::warn!`，恒返回 `Ok(())`，导致调用方无法感知边框剥离失败。

**修复状态**：✅ 已修复于 v4.0 Wave 2E。现在通过 `set_window_long_with_error` 辅助函数（window.rs:128，封装 `SetLastError(0)`→`SetWindowLongPtrW`→`GetLastError` 检测），在 `GetLastError != 0` 时返回 `Err(MirrorStarError::DesktopIntegration)`，`make_borderless` 以 `?` 传播（window.rs:22,40）。

### [D-004] [Medium] [并发] mod.rs — `ensure_workerw_ready` 锁内 re-embed 循环存在跨线程消息-锁依赖

**描述**：`ensure_workerw_ready` 在 Explorer 重启后的"重新初始化"场景中，持有 `Mutex` 锁遍历 `active_wallpapers` 调用 `embed_wallpaper`（含 `SetParent`/`SetWindowPos` 等跨线程窗口操作），存在跨线程消息-锁依赖死锁风险。

**修复状态**：✅ 已修复（决策保留现状 + 文档化契约，v4.0 Wave 2E）。re-embed 循环仍在锁内执行，但 D-004 注释（mod.rs:215-236）明确文档化锁内阻塞风险、阻塞规模上界（N×200ms，N 通常 ≤2），并说明不重构为"锁外逐个 embed"的理由（需 clone 字段引入接口复杂度，收益不匹配）；同时已通过 D-013 将显示器枚举移到循环外一次性获取，降低锁内耗时。重试 sleep 已在释放锁后由调用方执行。

### [D-005] [Medium] [资源] mod.rs — `remove_wallpaper` 调用 `SetParent(hwnd, None)` 后未隐藏窗口

**描述**：`remove_wallpaper` 在窗口仍有效时调用 `SetParent(hwnd, None)` 后未 `ShowWindow(SW_HIDE)`，若调用方未及时销毁，原壁纸窗口会以可见顶层窗口形式残留。

**修复状态**：✅ 已修复于 v4.0 Wave 2E。`SetParent(hwnd, None)` 成功后立即 `ShowWindow(hwnd, SW_HIDE)`（mod.rs:380），D-005 注释明确所有权契约"调用方负责销毁窗口（DestroyWindow）"。

### [D-006] [Low] [逻辑] mod.rs — `is_workerw_valid` 仅校验 `workerw_hwnd` 未校验 `progman_hwnd`

**描述**：`is_workerw_valid` 仅校验 `workerw_hwnd` 有效性，未校验 `progman_hwnd`，Explorer 重启后句柄复用可能误判有效。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。`is_workerw_valid` 现同时校验 `progman_hwnd` 与 `workerw_hwnd`（mod.rs:406-412），两者任一无效即返回 `false`，触发调用方 re-embed。

### [D-007] [Medium] [错误] worker_w.rs — `find_workerw_no_retry` 三次 `EnumWindows` 返回值被 `let _ =` 丢弃

**描述**：三次 `EnumWindows` 调用返回值均被 `let _ =` 丢弃，错误被吞掉，诊断信息丢失。

**修复状态**：✅ 已修复于 v4.0 Wave 2E。现抽取 `try_enum_windows` 辅助函数（worker_w.rs:219），在 `EnumWindows` 返回 `Err` 且 callback 未写入结果时 `tracing::warn!(error = ?e, ...)` 并返回 `Err(MirrorStarError::DesktopIntegration)`；三处调用点传入 Step 标识复用。错误处理风格与 `FindWindowW`/`GetWindowRect` 的 `?` 传播一致。

### [D-008] [Low] [可维护性] worker_w.rs — `is_invalid() || == HWND::default()` 条件语义重复

**描述**：`if progman.is_invalid() || progman == HWND::default()` 中两个条件语义重复。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。现仅保留单一判断 `if progman_hwnd.is_invalid()`（worker_w.rs:91），冗余的 `== HWND::default()` 已删除；同文件其他位置的重复模式亦已统一。

### [D-009] [Low] [性能] worker_w.rs — `get_system_wallpaper` 栈上分配 64 KiB 缓冲接近受限栈上限

**描述**：`get_system_wallpaper` 在栈上分配 `[0u16; 32767]`（约 64 KiB）。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。现改为堆分配 `vec![0u16; UNICODE_STRING_MAX_CHARS]`（worker_w.rs:529），避免受限栈线程上接近栈上限；行为不变（缓冲区仍为 32767，覆盖长路径）。

### [D-010] [Low] [可维护性] window.rs — 三个公开函数无任何单元测试

**描述**：`make_borderless` / `remove_from_taskbar` / `set_mouse_passthrough` 均无单元测试。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。现补充 3 个对真实 Progman 窗口的 no-panic 烟雾测试（window.rs:239-271，均标记 `#[ignore = "需要 Windows 环境且修改 Progman 窗口样式"]`），参照 `worker_w.rs::try_find_sibling_workerw_helper_no_panic` 范式。

### [D-011] [High] [逻辑] window.rs — `set_mouse_passthrough` 写入 `WS_EX_LAYERED` 导致壁纸呈现停止

**描述**：`set_mouse_passthrough` 曾对 `enabled=true` 分支 OR 上 `WS_EX_LAYERED | WS_EX_TRANSPARENT`，`enabled=false` 分支仅移除 `WS_EX_TRANSPARENT` 而保留 `WS_EX_LAYERED`。壁纸窗口（mpv 视频输出窗口、图片 GDI 窗口）从未调用 `SetLayeredWindowAttributes`，一旦被写入 `WS_EX_LAYERED` 即进入「分层但无分层属性」的非合成状态，画面停止更新——表现为开关「鼠标交互模式」时视频壁纸被"暂停"，且关闭后因 `WS_EX_LAYERED` 未清除而不恢复。真实呈现状态可在 mpv 日志中确认无 `pause="yes"`（即解码未真正暂停）。

**修复状态**：✅ 已修复于 v41-D-013。此前 v4.0 Wave 3B 仅补齐 doc comment 声明"分层属性契约"，未消除非合成态风险。现 `set_mouse_passthrough` 绝不写入 `WS_EX_LAYERED`：`enabled=true` 仅 OR `WS_EX_TRANSPARENT`；`enabled=false` 幂等清除 `WS_EX_TRANSPARENT` 与遗留的 `WS_EX_LAYERED`（并输出 info 诊断日志）。样式计算抽取为纯函数 `passthrough_ex_style` 并补单测（window.rs）。raised desktop 例外：关闭穿透时保留 `WS_EX_LAYERED` 并重设 `SetLayeredWindowAttributes(alpha=255)`，避免壁纸失去桌面合成（见 1.4）。

### [D-012] [Low] [可维护性] worker_w.rs — `SendMessageTimeoutW` 超时参数 200ms 为内联魔法数

**描述**：`SendMessageTimeoutW` 超时参数 `200`（ms）为内联魔法数。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。现提取为命名常量 `WM_SPAWN_WORK_TIMEOUT_MS`（worker_w.rs:35），并有消除歧义注释。该常量值已在 2026-10 raised desktop 兼容修复中由 `200` 调整为 `1000`（见 1.4）。

### [D-013] [Low] [性能] worker_w.rs — `embed_wallpaper` PerMonitor 分支每次调用都全量枚举显示器

**描述**：`embed_wallpaper` 在 `PerMonitor` 分支每次调用都执行 `enumerate_displays()` 全量枚举。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。`DesktopIntegrator` 层增加 `cached_displays` 缓存（5s TTL，`get_cached_displays`，mod.rs:314-325），`embed_wallpaper` 通过传入的 `displays` 参数使用缓存列表；`ensure_workerw_ready` re-embed 循环也在循环外一次性获取显示器列表（mod.rs:241）。

### [D-014] [Medium] [架构] mod.rs — `SAFETY` 注释声称"仅存储和传递 HWND"与实际不符

**描述**：`DesktopIntegrator` 的 SAFETY 注释声称方法仅存储和传递 HWND，但实际直接执行 `SetParent`/`SetWindowPos` 等窗口操作，注释与实现不符。

**修复状态**：✅ 已修复于 v4.0 Wave 2E。SAFETY 注释（mod.rs:106-126）以 `D-014 修订` 明确说明：窗口操作并非仅做句柄存储与传递，而是在持有 Mutex 的任意线程上以跨线程同步消息方式执行；并引用 D-004 说明锁内阻塞风险、列明接受此风险的 3 条理由及 v41-D-009 锁内阻塞上界分析。移除"仅存储和传递 HWND 值"的不实表述。

### [D-015] [Low] [安全] native_wallpaper.rs / worker_w.rs — 路径转宽字符未校验嵌入式 NUL

**描述**：`set_wallpaper_image` 与 `restore_system_wallpaper` 将路径转宽字符串传给 `SystemParametersInfoW`，Rust `String` 允许含 `'\0'`，会被 Win32 的 null-terminator 约定截断。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。两处均增加嵌入式 NUL 校验：`worker_w.rs::restore_system_wallpaper`（:560）与 `native_wallpaper.rs::set_wallpaper_image`（:231）在转宽字符前 `if path.contains('\0')` 返回 `Err(MirrorStarError::InvalidPath)`。

## 3. v3.x 已修复问题（历史背景）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| D01 | High | `set_native_wallpaper` 先写注册表缩放模式再设壁纸，失败时注册表残留 | ✅ 已修复（v3.5.1）— `RegistryGuard` 事务回滚守卫 |
| D02 | Medium | `get_system_wallpaper` 固定 260 缓冲区，长路径/UNC 路径被截断 | ✅ 已修复（v3.5.2；后续 v4.0 D-009 已改堆分配） |
| D03 | Medium | `set_mouse_passthrough` 修改 GWL_EXSTYLE 后未调用 `SetWindowPos(SWP_FRAMECHANGED)` 刷新 | ✅ 已修复（v3.5.2）— 现经 `refresh_frame_changed` 辅助函数（D-TD-006） |
| D04 | Medium | `embed_wallpaper` PerMonitor 未匹配 display_id 时静默回退无 warning | ✅ 已修复（v3.5.2） |
| D05 | Medium | `ensure_workerw_ready` 重新嵌入失败条目残留导致状态不一致 | ✅ 已修复（v3.5.2） |
| D06 | Medium | `find_workerw()` 含 10 次 sleep 重试为死代码，pub 易被误用于 async 上下文 | ✅ 已修复（v3.5.2） |
| D07 | Medium | `SetWindowLongPtrW` 返回值 0 歧义无法可靠检测失败 | ✅ 已修复（v3.5.2；后续 v4.0 D-003 已修复 `make_borderless` 恒返回 Ok） |
| D08 | Low | `EnumDisplayMonitors`/`GetMonitorInfoW` 失败静默无日志 | ✅ 已修复（v3.5.3） |
| D09 | Low | `SendMessageTimeoutW` 的 `result` 变量 dead store | ✅ 已修复（v3.5.3） |
| D10 | Low | 魔法数字（260/0x052C/3/10）未定义为命名常量 | ✅ 已修复（v3.5.3；后续 v4.0 D-012 已修复 200ms 超时常量） |
| D11 | Low | `enum_windows_callback` 与 `fallback_enum_callback` 兄弟窗口检查逻辑不一致 | ✅ 已修复（v3.5.3）— 统一为 `try_find_sibling_workerw` |
| D12 | Low | `restore_original_wallpaper`/`restore_system_wallpaper` 返回 `()` 调用方无法感知失败 | ✅ 已修复（v3.5.3）— 返回 `Result` |
| 硬编码布局忽略用户配置 | — | `check_and_reinitialize` 从 HashMap 取实际 arrangement | ✅ 已修复（v1.0） |
| remove_wallpaper 空操作 | — | `IsWindow` 检查 + `SetParent(hwnd, None)` 分离窗口 | ✅ 已修复（v1.0；后续 v4.0 D-005 已修复未隐藏窗口） |
| unsafe Send/Sync 契约脆弱 | — | 移除冗余 `unsafe impl Sync`，用 `Arc<Mutex>` 保护 | ✅ 已修复（v1.0） |
| DPI 获取失败静默默认 | — | 添加 `tracing::warn!` 日志 | ✅ 已修复（v1.0） |
| DisplayInfo 字段冗余 | — | `id` 为 device_name，`name` 为"显示器 N" | ✅ 已修复（v1.0） |

## 4. v6.0 技术债清单及清理状态（合并）

> 来源：v6.0 技术债审查（2026-07-25）desktop 模块。以下为原「技术债清单」与「清理状态汇总」合并后的规范化表，每个 D-TD 项仅保留一行且带唯一清理状态。堆类型标注对应原清单分类。以下行号反映 v6.0 清理前代码状态。
> 清理日期：2026-07-25 | 衍生 spec：`cleanup-v6-desktop-tech-debt-2026-07-25`
> 总技术债：26 项 | 已修复：21 项（80.8%）| 已决策保留/移除：5 项（19.2%，D-TD-003/004/018/022/026）| 完成率：100%

| ID | 类型 | 位置 | 描述/影响 | 清理建议（复杂度） | 清理状态 | 落实说明 |
|---|---|---|---|---|---|---|
| D-TD-001 | 死代码 | mod.rs:402-404 | `workerw_hwnd()` getter 仅在自身单元测试（:723,:743）调用，生产无调用点 | 删除 getter，测试改 `#[cfg(test)]` 内部访问器或间接验证（低） | ✅ 已修复于 v6.0 | 删除 `workerw_hwnd()` getter，测试改用直接字段访问 |
| D-TD-002 | 死代码 | mod.rs:407-409 | `progman_hwnd()` getter 仅在自身单元测试（:719,:742）调用，生产无调用点 | 同上，删除 getter（低） | ✅ 已修复于 v6.0 | 删除 `progman_hwnd()` getter，测试改用直接字段访问 |
| D-TD-003 | 冗余抽象 | mod.rs:435-437 | `check_and_reinitialize` 仅一行委托 `ensure_workerw_ready`，doc 长达 15 行 | 保留为对外入口，doc 补充"等价于私有 `ensure_workerw_ready`"说明（中） | ✅ 已决策保留 | 保留，doc 补充"对外入口（等价于私有 `ensure_workerw_ready`）"说明 |
| D-TD-004 | 冗余抽象 | mod.rs:440-442 | `enumerate_displays` 仅一行调用同名独立函数，为 OO 风格 thin wrapper | 保留（有生产调用），doc 注明等价关系（低） | ✅ 已决策保留 | thin wrapper 保留，doc 补充"等价于独立函数 `enumerate_displays()`" |
| D-TD-005 | 重复实现 | window.rs:10-57,60-89,108-141 | `SetWindowLongPtrW`+错误检测模式 4 处重复，修改需同步 4 处 | 抽取 `set_window_long_with_error` 辅助函数（中） | ✅ 已修复于 v6.0 | 抽取 `set_window_long_with_error` 统一 4 处 SetWindowLongPtrW 模式 |
| D-TD-006 | 重复实现 | window.rs:74-88,126-140 | `SetWindowPos(SWP_FRAMECHANGED)`+warn! 模式 2 处重复 | 抽取 `refresh_frame_changed` 辅助函数（低） | ✅ 已修复于 v6.0 | 抽取 `refresh_frame_changed` 统一 2 处 SetWindowPos 模式 |
| D-TD-007 | 重复实现 | worker_w.rs:160-173,204-216,232-244 | 三次 EnumWindows 相同的 12 行模式，添加步骤易遗漏 | 抽取 `try_enum_windows` 封装误报处理（中） | ✅ 已修复于 v6.0 | 抽取 `try_enum_windows` 封装 EnumWindows 误报处理 |
| D-TD-008 | 重复实现 | worker_w.rs:460-463,480-482,488-490 | `GetWindowRect` 错误转换在三分支重复 3 次 | match 前统一获取 `workerw_rect` 三分支共用（中） | ✅ 已修复于 v6.0 | `workerw_rect` 在 match 前统一获取 |
| D-TD-009 | 重复实现 | mod.rs:543-546, worker_w.rs:550-553 | UTF-16 字符串提取逻辑两处实现 | 复用 `extract_utf16_string`（`pub(crate)`）（低） | ✅ 已修复于 v6.0 | `get_system_wallpaper` 复用 `extract_utf16_string` |
| D-TD-010 | 过时模式 | worker_w.rs:359-378 | `check_sibling_workerw` 用 `from_utf16_lossy`，其他已迁移 `eq_wide` | 改调用 `is_workerw_class`，移除 `WORKERW_CLASS` 常量（低） | ✅ 已修复于 v6.0 | 改用 `is_workerw_class` 字节级比较，移除 `WORKERW_CLASS` 常量 |
| D-TD-011 | 过时模式 | worker_w.rs:381-415 | `find_child_by_class` 注释为修复痕迹，if let 与 loop 混用 | 移除修复注释，统一为 loop 写法（低） | ✅ 已修复于 v6.0 | 移除修复注释，统一为 loop 写法 |
| D-TD-012 | 过度设计 | mod.rs:315-326 | `get_cached_displays` 恒返回 Ok 的 Result 包装，YAGNI | 移除 Result，直接返回 `&[DisplayInfo]`（低） | ✅ 已修复于 v6.0 | 移除 Result，直接返回 `&[DisplayInfo]` |
| D-TD-013 | 过度设计 | native_wallpaper.rs:192-231 | `read_wallpaper_style` 从不返回 Err 的多余 Err 变体 | 移除外层 Result（低） | ✅ 已修复于 v6.0 | 移除外层 Result，直接返回 `Option<(String,String)>` |
| D-TD-014 | 过度设计 | native_wallpaper.rs:50-59 | 防御性处理永不出现的 Err 分支 | 依赖 D-TD-013 清理后简单化（低） | ✅ 已修复于 v6.0 | `set_native_wallpaper` 简化 Err 处理 |
| D-TD-015 | 修复痕迹 | mod.rs:6 | doc comment 引用失效的 `v41-D-017` spec 编号 | 移除失效 spec 引用（低） | ✅ 已修复于 v6.0 | 移除 `v41-D-017` 失效 spec 引用 |
| D-TD-016 | 修复痕迹 | mod.rs:167 | `N-001` 引用无对应 spec | 移除"（N-001）"标记（低） | ✅ 已修复于 v6.0 | 移除 `N-001` 失效 spec 引用 |
| D-TD-017 | 修复痕迹 | mod.rs:158-160,177-179,266,430-434 | 4 处 "T14：" 历史 task 前缀 | 移除前缀，保留设计理由（低） | ✅ 已修复于 v6.0 | 移除 4 处 `T14：` 历史任务前缀 |
| D-TD-018 | 修复痕迹 | mod.rs:191,283; worker_w.rs:187; window.rs:107 | 4 处 "参考 Phase 3 优化" 引用未实现的未来工作 | 移除引用（历史规划残留）（低） | ✅ 已决策移除 | Phase 3 引用为历史规划残留，移除 4 处 |
| D-TD-019 | 修复痕迹 | worker_w.rs:903,910; window.rs:154 | 测试注释 "Wave 2C/2D" 历史标记 | 移除引用（低） | ✅ 已修复于 v6.0 | 移除 3 处 `Wave 2C/2D` 历史标记 |
| D-TD-020 | 修复痕迹 | worker_w.rs:6-19 | doc comment 用 14 行描述未来 Cell/RefCell 风格，当前用裸指针 | 精简为"当前使用裸指针 + LPARAM"（低） | ✅ 已修复于 v6.0 | 模块文档精简，移除 `v41-D-012` 标记与未来 Cell/RefCell 风格指南 |
| D-TD-021 | 命名一致性 | worker_w.rs / mod.rs 多处 | `workerw` vs `workerw_hwnd` 句柄命名不一致 | 统一为 `workerw_hwnd`/`progman_hwnd`（中） | ✅ 已修复于 v6.0 | `workerw`→`workerw_hwnd`、`progman`→`progman_hwnd` 命名统一 |
| D-TD-022 | 命名一致性 | window.rs:10,60,108 | `make_borderless` 返回 Result 但另两函数返回 `()`，风格分裂 | 决策保留 `()`，doc 明确契约（中） | ✅ 已决策保留 | 保留 `()` 返回，doc 补充"失败仅 warn，调用方不感知"契约 |
| D-TD-023 | 命名一致性 | worker_w.rs:259,301 | `enum_windows_callback` vs `fallback_enum_callback` 命名不一致 | 统一为 `find_workerw_callback`/`find_workerw_fallback_callback`（低） | ✅ 已修复于 v6.0 | 重命名为 `find_workerw_callback`/`find_workerw_fallback_callback` |
| D-TD-024 | 注释陈旧 | native_wallpaper.rs:189-191,377-380,404-407 | 注释承认"不返回 Err 但保留 Err 分支" | 依赖 D-TD-013 清理后自然消失（低） | ✅ 已修复于 v6.0 | 依赖 D-TD-013 清理后消除注释与代码不符 |
| D-TD-025 | 注释陈旧 | mod.rs:312-314 | doc comment 承认"不会失败"但保留 Result 包装 | 依赖 D-TD-012 清理后自然消失（低） | ✅ 已修复于 v6.0 | 依赖 D-TD-012 清理后消除注释与代码不符 |
| D-TD-026 | 注释陈旧 | worker_w.rs:96-102 | doc comment 描述无实施计划的长期重构方向 | 移除 doc comment 该段（低） | ✅ 已决策移除 | `v41-D-015` 长期重构方向为历史规划残留，移除该段 |

> 补充：4.5 未使用导入为「无」，经 Grep 验证 4 文件 `use` 项均有实际调用点；`pub mod` 三子模块均被外部 crate 与 src-tauri 引用。

### 4.10 验证结果

- `cargo test -p mirrorstar-core desktop::`：57 tests passed / 0 failed / 部分 ignored（预期行为）
- `cargo clippy --workspace --all-targets -- -D warnings`：零警告
- `cargo test --workspace`：全部通过
- `cargo test -p mirrorstar-core config::`：150 passed / 0 failed（回归验证）
- `npx vitest run`：208 passed / 0 failed
- `npm run lint` + `npm run typecheck`：零错误

## 5. 纯净壁纸模式：窗口摘出与回嵌契约

> 实现来源：`crates/mirrorstar-core/src/desktop/mod.rs`（`detach_for_pure_mode` / `reattach_after_pure_mode`）+ `crates/mirrorstar-core/src/pure_mode/mod.rs`（`lift_wallpaper_window`）。

### 5.1 两个新原语的契约

纯净壁纸模式需要把"正在播放的壁纸窗口"从壁纸层（旧层级 WorkerW / raised desktop 的 Progman）**临时摘出**并置顶全屏覆盖桌面（含图标与任务栏），退出后再嵌回。desktop 层为此新增两个方法，Win32 操作与几何全部复用既有原语 / 算法，零新增布局代码（**视频显示器例外**：架构 D1 后视频不再搬动原窗口，改由临时顶层覆盖渲染器呈现，见 [5.5](#55-退出回嵌的防呈现冻结契约架构-d1)）：

| 方法 | 行为 | 调用方 |
|---|---|---|
| `detach_for_pure_mode(display_id)` | 读当前矩形 → `lift_wallpaper_window`（`set_parent_verified(hwnd, None)` 摘出 + `restore_top_level_style` 清 `WS_CHILD`（保留 `WS_EX_LAYERED` / alpha=255）+ `SetWindowPos(HWND_TOPMOST, 原矩形, SWP_NOACTIVATE \| SWP_SHOWWINDOW \| SWP_FRAMECHANGED)` + `InvalidateRect`）→ **移除 `active_wallpapers` 条目** → 返回摘出的 `HWND` | `WallpaperEngine::enter_pure_mode` |
| `reattach_after_pure_mode(display_id, hwnd, arrangement)` | 委托现有 `embed_wallpaper`：重新查找 WorkerW 并重新写回 `active_wallpapers` 条目（raised desktop 下重新套用 `WS_CHILD` + `WS_EX_LAYERED` alpha=255、Progman 父级、DefView z-order）；**成功后再补一次 `InvalidateRect(hwnd, None, false)`** 保险重绘（失败仅 `warn`，见 5.4） | `WallpaperEngine::exit_pure_mode` / `rollback_pure_mode_enter` |

契约细节：

- **不隐藏窗口**：摘出后窗口必须保持可见以继续出画，故 `detach_for_pure_mode` **不**调用 `ShowWindow(SW_HIDE)`。
- **保持当前屏幕矩形（零 resize）**：`lift_wallpaper_window` 先 `GetWindowRect` 读当前屏幕矩形，再经 `set_parent_verified(hwnd, None)` 摘出、`restore_top_level_style` 清除 `WS_CHILD`（保留 `WS_EX_LAYERED` / alpha=255），随后原样复用该矩形置顶（宽高不做任何调整）——规避 mpv D3D11 输出在"重父化 + resize"时重配置。
- **不抢前台**：只做"重定父级 + 设置 `HWND_TOPMOST`"，置顶带 `SWP_NOACTIVATE`；窗口在嵌入阶段已由 `remove_from_taskbar` 设置 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`，因此置顶后不会成为前台窗口、不进 Alt+Tab。
- **失败语义**：`active_wallpapers` 无该 `display_id` → 返回 `Err`；`IsWindow(hwnd)` 为假（窗口已消失）→ 移除条目并返回 `Err`；`lift_wallpaper_window` 失败 → **保留条目**并返回 `Err`（见下条"半摘出恢复"）。
- **失败语义新增项：半摘出恢复（A5）**：`lift_wallpaper_window` 的失败可能发生在 `SetParent(None)` **之后**（如 `SetWindowPos` / `restore_top_level_style` 失败）——此时窗口已脱离 WorkerW/Progman 成为普通顶层窗口，即**"半摘出"状态**（若不恢复会漂浮在桌面层遮挡图标，而引擎回滚只处理已计入 `lifted` 的窗口，覆盖不到本次失败窗口）。失败后 `detach_for_pure_mode` 先做 **best-effort 恢复**：若 `is_attached_to_wallpaper_layer` 为假（`GetAncestor(GA_PARENT)` 父窗口既非 WorkerW 也非 Progman，确认已脱离）→ 用进入时的 `arrangement` 经 `embed_wallpaper` 嵌回；嵌回仍失败 → 对有效句柄 `ShowWindow(SW_HIDE)` 隐藏兜底并记录 `error!`。恢复为 best-effort，**不改返回值**：随后仍**保留 `active_wallpapers` 条目并返回原 `Err`**（进入回滚语义不变）。未脱离（失败发生在 `GetWindowRect` / `SetParent` 阶段）则窗口仍在壁纸层内，无需恢复。
- **回嵌失败不由本层兜底**：`reattach_after_pure_mode` 复用 `embed_wallpaper`，**失败时由调用方（引擎）按降级语义隐藏窗口**——即本方法自身**不隐藏、不销毁**窗口（隐藏兜底属于引擎侧的失败处理契约，见 `docs/04-优化文档/04-wallpaper模块.md` 的"嵌回失败的隐藏降级"）。

### 5.2 为什么必须移除 `active_wallpapers` 条目

`active_wallpapers` 是"当前已嵌入 WorkerW 的壁纸窗口"注册表，被 `check_and_reinitialize` / `ensure_workerw_ready` 的重初始化循环与 `workerw_check` 任务作为"需要重新嵌入的窗口集合"消费。若摘出后**保留**条目，Explorer 重启或 WorkerW 兜底轮询触发重新初始化时会遍历该表，把**已摘出、已置顶**的窗口再次 `SetParent` 回新 WorkerW——置顶效果被撤销、纯净模式被破坏（且摘出后窗口坐标语义已变，重新嵌入结果不可预期）。因此：

- 摘出成功即移除条目 → 重初始化循环不再触达该窗口 → 置顶状态保持（配套：收到 `TaskbarCreated` 广播时由输入钩子线程调用 `pure_mode::assert_topmost_all()` 重断言 Z 序，防任务栏抢 Z 序）。
- 条目的回写只发生在退出（或进入失败回滚）时，由 `embed_wallpaper` 重新查找新 WorkerW 后写回（Explorer 重启后自动使用新句柄）。

> 本节只适用于**走摘出路径**的显示器。**视频显示器例外**（架构 D1，见 [5.5](#55-退出回嵌的防呈现冻结契约架构-d1)）：原窗口从不摘出，其 `active_wallpapers` 条目**全程保留**，因此重初始化循环会照常维护它（这正是期望行为——它仍嵌在壁纸层中）。

### 5.3 与 `remove_wallpaper` 的语义差异

| 维度 | `remove_wallpaper` | `detach_for_pure_mode` |
|---|---|---|
| 目的 | 彻底移除某显示器壁纸（销毁前的分离） | 纯净壁纸模式临时摘出，稍后回嵌 |
| `SetParent(hwnd, None)` | 是（经 `set_parent_verified` 校验父链） | 是（在 `lift_wallpaper_window` 内，经 `set_parent_verified`） |
| 分离后样式 | `restore_top_level_style` 清 `WS_CHILD`（保留 `WS_EX_LAYERED` / alpha=255），失败仅 `warn` | `restore_top_level_style` 清 `WS_CHILD`（保留 `WS_EX_LAYERED` / alpha=255），失败返回 `Err` 并走半摘出恢复 |
| 置顶 | 否（分离后为普通顶层窗口） | 是（`HWND_TOPMOST`，保持原矩形） |
| `ShowWindow(SW_HIDE)` | **是**（D-005：分离后立即隐藏，防残留可见顶层窗口） | **否**（窗口须继续可见出画） |
| `active_wallpapers` 条目 | 移除 | 移除（见 5.2） |
| 窗口所有权 | 调用方负责 `DestroyWindow`，本方法只分离 + 隐藏 | 窗口**不销毁**，退出时由 `embed_wallpaper` 嵌回（raised desktop 下重新套用 raised 样式） |
| `SetParent` 失败时条目 | 保留条目并返回 `Err` | 保留条目并返回 `Err`：lift 失败（含半摘出）先做 best-effort 恢复（嵌回 / 隐藏兜底），随后仍保留条目并返回 `Err` |

> **文档断言 → 代码位置**：A5 半摘出恢复函数 `recover_half_detached_window` 定义于 `crates/mirrorstar-core/src/desktop/mod.rs` 中 `detach_for_pure_mode` 与 `is_workerw_valid` 之间。

### 5.4 退出编排中的渲染器存活校验与回嵌保险重绘

退出纯净壁纸模式时，除"回嵌窗口"外还需保证"带回桌面的画面是活的"：mpv 视频输出偶发在 `--loop-file` 循环点停滞（"最后一帧 drain 未完成"，`time-pos` 钉在最后一帧），原退出编排只回嵌窗口、**从不校验渲染器是否真的在播放**，会把冻结"带"到退出之后。为此退出编排（`src-tauri/src/platform/pure_mode.rs::exit_orchestration`）在回嵌之后新增一步视频渲染器存活校验 + 自愈：

| 步骤 | 动作 | 顺序理由 |
|---|---|---|
| ① | `exit_pure_mode()` 回嵌 WorkerW；其内部先对临时覆盖窗调用 `hide_window_for_pure_mode_exit` 同步隐藏，再逐窗调用 `renderer.terminate()` | 必须在卸载输入钩子之前（否则出现"无钩子但壁纸仍置顶"的窗口） |
| ② | 卸载输入钩子 | — |
| ③ | **取消托盘勾选（`set_checked(false)`）** | 回嵌 + 卸载钩子完成后模式已实际退出；**提前**同步使用户可见态尽早一致，且后续存活校验 / 自愈（可能持续数秒）失败也不影响勾选态 |
| ④ | 补跑全屏 / 电源判定 | 纠正模式期间冻结的状态漂移 |
| ⑤ | **`liveness_check_and_heal(&app)`（含暂停短路）** | 放在 ④ 之后：补跑判定可能按用户配置（`fullscreen_action`）终止壁纸，先判定再校验，避免"刚自愈又被终止"；放在 ⑥ 之前：保证退出完成时画面是活的。校验前先读 `pause_reasons_snapshot()`，**非空即跳过**（与看护任务一致，符合 D6"用户主动暂停 ⇒ 永不判定停滞、永不自愈"） |
| ⑥ | 唤醒轮换调度器重算 deadline | 模式期间排期被抑制，退出后需重新计算 |

`liveness_check_and_heal` 的判定与自愈语义（采样窗口、1ms 停滞判据、`terminate()` + `revive_terminated()` 自愈）与 core 侧契约详见 [`04-wallpaper模块.md`](./04-wallpaper模块.md) 的"渲染器存活探针与停滞自愈契约"；本节只记录其在退出编排中的**位置与顺序**。校验 / 自愈失败仅 `warn`，不阻断 ⑥。

进入侧（`enter_orchestration`）等待在途退出的窗口由 3s 提升到 **≈10s**（`EXIT_WAIT_TRIES = 200`，50ms × 200），以覆盖"退出含存活校验与自愈"的时长，避免用户在退出尾段点击「进入」被误拒；超时仍拒绝进入并保持未勾选。配套地，`MIRRORSTAR_TEST_PURE_MODE` 测试钩子的退出等待窗口同步由 10s 提升到 **20s**，避免自愈场景下钩子误报超时、中止后续轮次。

与本节窗口契约直接相关的一处配套改动：`reattach_after_pure_mode` 在 `embed_wallpaper(...)` **成功之后**追加一次 `InvalidateRect(hwnd, None, false)` 保险重绘（失败仅 `warn`）。它与进入路径 `pure_mode::lift_wallpaper_window` 末尾的保险重绘**同源**，用于消除"重嵌完成但客户区仍显示旧内容"的观感风险。

> 覆盖边界：以上存活校验与自愈只覆盖**时钟层停滞**（`time-pos` 不再前进，详见 [`04-wallpaper模块.md`](./04-wallpaper模块.md) 的"渲染器存活探针与停滞自愈契约"）；**呈现层冻结**（时钟前进、画面定格）不在其覆盖范围，改由 5.5 的架构 D1 规避。

### 5.5 退出回嵌的防呈现冻结契约（架构 D1）

> 来源：spec `fix-pure-mode-exit-presentation-freeze`（2026-09-30）。本节记录"退出纯净壁纸后视频画面定格"这一失效模式的根因、已证伪路径与最终修复契约；它与此前"循环点时钟停滞"（见 5.4 与 [`04-wallpaper模块.md`](./04-wallpaper模块.md) 的"渲染器存活探针与停滞自愈契约"）是**两种不同的失效模式**。

**现象与根因（2026-09-30 实测）**：退出纯净壁纸后视频**画面定格**（停在最后一帧不再刷新），但**播放时钟继续前进**——该次会话 mpv 全程只收到 `loadfile` 与 `quit` 两条命令（**从未收到 `pause`**）；退出编排的存活校验（≈0.57s，两次 500ms 采样）与常驻 5s 看护均**判定"时钟前进"、未触发自愈**。故失效点在**呈现层**：把正在播放的 D3D11（bitblt 呈现）视频窗口"摘出 WorkerW 并置顶"后，**本会话的 WorkerW 子层不再产出可呈现帧**；而同一窗口在**顶层**呈现正常（置顶期间画面哈希持续变化）。即"**时钟前进 + 画面定格 = 呈现冻结**"，与"循环点时钟钉住"不同。

**已证伪路径（保留结论，避免重复踩坑）**：

| 类别 | 手段 | 结果 |
|---|---|---|
| ① 窗口级解冻 / 预防 | IPC `pause=yes→no`、1px 位移复原（`SWP_NOSIZE\|NOZORDER\|NOACTIVATE`）、显式 `WM_SIZE`（原尺寸）、`ShowWindow(SW_HIDE)→SW_SHOW`、1px 真 resize 复原 | 既**不能解冻**也不能**预防**（退出后 ~0.5s 内施加同样无效） |
| ② 退出侧"重建渲染器" | 销毁旧渲染器 → 新建并嵌入 WorkerW → `loadfile` | 产出**全黑**：运行期新建并嵌入 WorkerW 的 mpv 窗口不再出画，`PrintWindow(PW_RENDERFULLCONTENT)` 像素哈希恒为 `d7489c5f…` |
| ③ 唤醒探针（7 项） | "跳过回嵌"、WorkerW / Progman 层 `RedrawWindow(RDW_*)`、`DwmFlush`、完整重嵌循环、WorkerW 内提层、尺寸扰动等 | **全部无效**（含完整重嵌循环 `SetParent(None)→SetParent(workerw)` 成功但画面不变） |

结论：进入侧"摘出并置顶"**毒化本会话 WorkerW 子层**，窗口级手段已用尽。

**最终契约（架构 D1）**：**视频显示器在进入纯净壁纸模式时不再搬动原窗口**（不 `detach_for_pure_mode`、不 `SetParent` / `SetWindowPos` / `ShowWindow`），改为新建**"临时顶层覆盖渲染器"**：

1. `construct_renderer(...)`（内部 `VideoRenderer::new` + `play()`；配置快照复用无副作用的 `renderer_config()`）；
2. `hwnd()` 取句柄 → `make_borderless(hwnd)`（失败仅 `warn` 继续）+ `remove_from_taskbar(hwnd)`（均与 `embed_wallpaper` Step 1 同源）；
3. `VideoRenderer::disable_auto_window_resize()`（经 mpv IPC `set_property("auto-window-resize","no")`，best-effort；见下"覆盖窗全屏尺寸契约"）；
4. `crate::pure_mode::lift_window_to_rect` 置顶全屏（**请求** 1920×1080、无边框；尺寸须由上一条保证，非 mpv 自适配的天然结果）；
5. `after_embed()` 从头 `loadfile`（与 boot"窗口稳定嵌入后 loadfile"语义一致）。

该临时渲染器登记进既有 `pure_mode_temp`。退出**仅 `pure_mode_temp.drain(..)` 销毁覆盖窗**（原窗口显露、继续播放；原 mpv **从不重启**）。原窗口的 `wallpapers` / `wallpaper_sources` / `wallpaper_mode` / `active_wallpapers` / `pause_senders` 条目**全程不变**。GDI(Image/Gif) / Web / Native 路径不变（仍走摘出置顶 + 回嵌）。每个视频覆盖步骤失败即 best-effort `terminate()` 该临时渲染器并放弃本显示器覆盖（`error!`），**不回滚整个进入流程**，原窗口不受影响。

**覆盖窗全屏尺寸契约**：mpv 默认 `auto-window-resize=yes`，会在 `loadfile` 载入视频时按**工作区**尺寸（例：1920×1040，任务栏占 40px）与视频宽高比（16:9）自适配窗口，覆写 `lift_window_to_rect` 请求的 1920×1080——实测覆盖窗被改成 **1848×1040 @ (36,20)**。因此视频覆盖分支必须在 `lift_window_to_rect` **之前**、且**早于 `after_embed()`（即早于 `loadfile`）**调用 `VideoRenderer::disable_auto_window_resize()`（经 mpv IPC `set_property("auto-window-resize","no")`）；**在 `loadfile` 之后再设置无效**（实测）。该调用为 **best-effort**：IPC 不可用仅 `warn`，不阻断覆盖流程。

> 决策依据（实测对照）：`keepaspect-window=no` 无效；`keepaspect=no` 会拉伸（禁用）；运行时 `set_property geometry` 会破坏正确状态（禁用）；启动参数 `--auto-window-resize=no` 亦有效但影响面更大——最终选用"仅覆盖窗经 IPC 设置"以隔离影响（boot / 轮换路径与全局 mpv 启动参数均未改动）。

**已知限制**：

- 模式内**双解码**：原窗口（WorkerW 内继续播放）与覆盖窗口同时解码同一视频；
- 进入时多约 **0.3–0.5s** 构建覆盖窗口；
- 运行时不检测"呈现冻结"（存活探针只看时钟，成本高的截图级检测不做）。

**验证结论（D1 后，可引用）**：单轮与三轮回归均通过——模式内覆盖窗稳定 **1920×1080 @ (0,0)**、无边框（`style=0x14000000`，`WS_CAPTION` / `WS_SYSMENU` 已清）、非黑且在动（连续 5 帧互异）；退出后原窗非黑且在动、原 mpv 进程**从未重启**；0 自愈误报、0 失败；进程健康（mpv=1、wp-proc=0、无孤儿）。

### 5.6 进入编排中的托盘溢出浮出窗关闭（"隐藏栏"残留）

> 来源：spec `fix-tray-overflow-flyout-on-pure-mode`（2026-09-30）。实机复现：**打开托盘"隐藏栏"（溢出浮出窗）→ 右键本应用托盘图标 → 点击「纯净壁纸」**，进入模式后浮出窗仍停留在屏幕上，破坏"桌面被壁纸完全覆盖"的沉浸目标。修复入口：`src-tauri/src/platform/pure_mode.rs`（`dismiss_tray_overflow_flyout()` + `enter_orchestration` 中的调用点）。

**根因**：溢出浮出窗由 Explorer 拥有，正常依靠"失去激活 / 用户点击别处"自动关闭；而纯净壁纸模式的进入编排**刻意不抢前台**（D1 契约，见 [5.5](#55-退出回嵌的防呈现冻结契约架构-d1) 与 5.1 的"不抢前台"条目：只做摘出 + `HWND_TOPMOST` + `SWP_NOACTIVATE`，壁纸窗口带 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`，覆盖窗同样 `NOACTIVATE`）。菜单点选后**没有任何窗口接管激活** ⇒ 浮出窗收不到关闭信号 ⇒ 残留并叠在壁纸之上。

**进入编排步骤表**（`enter_orchestration`，新增步骤为 ④b）：

| 步骤 | 动作 | 顺序理由 |
|---|---|---|
| ① | 进入互斥守卫（`ENTER_IN_PROGRESS`） | 并发进入（快速双击托盘）只允许一个真正执行 |
| ② | 幂等守卫（进程级标志已置位则跳过） | 模式运行中 / 正在进入时重复点击直接返回 |
| ③ | 串行化等待在途退出编排落位 | 必须先等旧捕获线程 join 完成再安装新钩子（跨代复位约束 A2） |
| ④ | 安装输入捕获钩子（`pure_mode_input::install`） | 失败即拒绝进入（避免"进得去出不来"） |
| ④b | **关闭托盘溢出浮出窗（best-effort）** | 必须在 ⑤ 置顶**之前**：浮出窗一旦被壁纸覆盖就无法再被用户点掉；且此时进入前置校验均已通过，失败不影响既有回滚语义 |
| ⑤ | 引擎进入（`enter_pure_mode`：视频走临时顶层覆盖渲染器，其余路径走摘出置顶） | 失败回滚 ④ 的钩子并保持未勾选 |
| ⑥ | 存句柄（先 take 旧句柄再 shutdown；存储前复核模式活跃） | 保证槽位唯一、钩子线程不泄漏、不被在途退出"漏卸" |
| ⑦ | 启动退出监听任务 | 一轮 install 只服务一次退出 |
| ⑧ | 置托盘勾选 | 用户可见态最后一致 |

**手段与语义（best-effort）**：

1. `EnumWindows` 枚举顶层窗口，按**类名精确匹配**（`const` 常量，便于契约测试引用）：

   | 常量 | 类名 | 覆盖平台 |
   |---|---|---|
   | `TRAY_OVERFLOW_CLASS_WIN11` | `TopLevelWindowForOverflowXamlIsland` | Win11（XAML island 承载） |
   | `TRAY_OVERFLOW_CLASS_LEGACY` | `NotifyIconOverflowWindow` | Win10 及部分旧版（经典窗口） |

   只处理**当前可见**（`IsWindowVisible`）的命中窗口；无命中时零等待直接返回（隐藏栏未打开的常见路径无额外开销）。

2. 先 `PostMessageW(hwnd, WM_CLOSE)`：走 Explorer 的正常关闭流程，避免留下"窗口仍在、只是被隐藏"的残留态；
3. 对**每个命中窗口**约 **120ms** 后复核 `IsWindowVisible`，仍可见则 `ShowWindow(hwnd, SW_HIDE)` 兜底；
4. 枚举失败 / 投递失败 / 兜底后仍可见，均只记日志（`info` 记录所用手段、`warn` 记录未生效），**不阻断进入**；不抢前台、不模拟键鼠输入、不触碰主托盘与任务栏。

调用点经 `spawn_blocking` 执行（helper 的等待时长随命中窗口数线性增长——逐个命中窗口各约 120ms；无命中则零等待），不阻塞异步运行时线程。与"抢前台"方案的差异：本方案只针对**溢出浮出窗**做定向关闭，主托盘菜单与任务栏均为 Explorer 正常状态，无需任何激活权转移。

**实机验证结论（2026-09-30，Windows 10 专业版 22H2 / build 19045）**：

- **类名取证**：在"隐藏栏打开"状态枚举顶层窗口，实取证类名为 `NotifyIconOverflowWindow`（与 Win10- 预期一致；Win11 常量一并保留以适配新系统）。
- **真实浮出窗**：隐藏栏打开时经测试钩子进入，日志记录 `INFO … 托盘溢出浮出窗已关闭（WM_CLOSE 生效） class=NotifyIconOverflowWindow`，枚举确认该窗口随即不可见。
- **伪造浮出窗**（`NotifyIconOverflowWindow` 类名的可见顶层窗口）：进入编排完成后该窗口被销毁（枚举不到），日志含同一条命中记录。
- **回归**：`MIRRORSTAR_TEST_PURE_MODE=3` 三轮往返全部完成（3/3），0 失败、0 自愈误报；退出后对 mpv 窗口 `PrintWindow` 连续 5 帧采样指纹**互异**（画面在动）、窗口为 1920×1080 全屏可见。

## 6. 优化机会与交集汇总

### 6.1 v4.0 仍可关注的渐进优化方向

> v4.0 原文案"优先修复/系统性修复/渐进优化"中 D-001 ~ D-015 现均已修复（见第 2 节）。以下为 v4.0 审查中提出、部分属于长期重构方向、按严重级别整理的残余优化建议（已修复项的原始建议仅作归档，不再待办）：

- **High（已修复）**：D-001 测试隔离、D-002 PerMonitor 坐标偏移 —— 均已修复，无待办。
- **Medium（已修复）**：D-003 错误传播、D-004 re-embed 移出锁外（接受现状 + 文档化）、D-005 隐藏窗口、D-007 EnumWindows 错误日志、D-014 SAFETY 注释修订 —— 均已修复，无待办。
- **Low（已修复）**：D-006/008/009/010/011/012/013/015 —— 均已修复，无待办。
- *归档建议（无需再实施）*：D-004 建议的"锁外逐 embed"、"per-display 渲染器线程自主重嵌入"；D-012 的单处常量提取；D-013 的短 TTL 缓存（已实现 5s TTL）。

### 6.2 v6.0 优化机会（非技术债类改进点）

来源于 v6.0 审查第 4 节，均为可选的工程改进，非必修：

- **EnumWindows 三步查找的可观测性**：Step 2/4/5 三次 EnumWindows 调用目前仅在 debug 日志区分，可考虑增加 metric counter（`find_workerw_attempt_total{step="2|4|5"}`）量化 Progman 直查快速路径命中率，验证 D-PERF-002 优化效果。
- **`embed_wallpaper` 的 `displays` 参数语义**：Span 分支不使用 displays 参数，PerMonitor 分支必须传入。可考虑用枚举类型在类型层表达"Span 不需要 displays"的契约，避免调用方构造无用 Vec。
- **`SetWindowPos` Z-order 处理**：嵌入时使用 `HWND_BOTTOM`，但 `embed_wallpaper` 的"半嵌入"风险文档未涉及 Z-order 回滚。可评估 `SetWindowPos` 失败时是否需重置 Z-order。

### 6.3 v4.0 / v5.0 / v6.0 交集说明

- **v5.0 已覆盖项**：D-PERF-001（UTF-16 字节级比较）、D-PERF-002（Progman 直查快速路径）、D-PERF-007（重试节奏优化）已实施。v6 的 D-TD-010 曾标记 D-PERF-001 未同步到 `check_sibling_workerw` 的过时模式，现已修复。D-PERF-007 后 `compute_retry_wait_ms` 重试次数从 10 次降为 6 次。
- **v6 新发现（均已在 v6.0 清理）**：死代码 getter（D-TD-001/002）、过度设计的 Result 包装（D-TD-012/013/014）、失效 spec 引用（D-TD-015/016/026）、过时模式（D-TD-010）、命名一致性（D-TD-021/022/023）等。
- **清理后遗留的长期方向**（已决策保留、非技术债）：`check_and_reinitialize`（D-TD-003）、`enumerate_displays` thin wrapper（D-TD-004）、`remove_from_taskbar`/`set_mouse_passthrough` 返回 `()` 契约（D-TD-022）。

### 6.4 结论

desktop 模块的 v4.0 审查发现（15 项）与 v6.0 技术债（26 项）均已于 2026-09-01 前全部关闭（100% 完成率）。当前模块无待修复的 v4 findings，也无未清理的 v6 技术债；剩余建议均为可选的长期重构或可观测性改进。

---

## 7. 深度审查：死代码 / 冗余 / 过度抽象（2026-10-10）

> **审查范围**：`crates/mirrorstar-core/src/desktop/`（4 个文件，非空 2959 行 / 含空行 3198 行）。
> **方法**：全仓引用搜索（排除 `target/`），逐项区分「无任何引用 / 仅测试引用 / 内部引用 / 外部引用」。
> **基线**：`cargo check --workspace` 通过；`cargo clippy --workspace --all-targets` 0 warning；`cargo test -p mirrorstar-core -- --skip mpv_backend` = 712 passed / 0 failed / 30 ignored（+3 passed / doc-test 1 passed）。

### 7.1 死代码

**结论：本模块未发现真正的死代码**——所有 `pub` / `pub(crate)` 符号均有实际调用方（含测试）。以下为「可见性/构造方式」层面的观察项：

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| D-1 | mod.rs:104 | `#[derive(Default)]` 仅测试使用 | 生产一律 `new()`（lib.rs:481、state.rs:1057）；`DesktopIntegrator::default()` 仅本文件测试（943/1133/1143/1281/1290） | Low | ⏳ 待确认（删除会破坏测试） |
| D-2 | native_wallpaper.rs:477 | `RegistryGuard` 仅测试引用 | 仅 round-trip 测试使用 | Low | 保留（有意为之） |
| D-3 | mod.rs:55-57 | `pub mod window` / `native_wallpaper` 对 `src-tauri` 零直接引用 | src-tauri 全域无匹配；仅 core 内部使用 | Low | ⏳ 待确认（公开面收窄） |
| D-4 | worker_w.rs | 历史死代码 `find_workerw()`（含 sleep 重试）已不存在 | 全仓无定义/引用 | — | 无残留，无需处理 |

### 7.2 冗余代码

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| R-1 | mod.rs:183-187 vs 224 | `ensure_initialized` 与 `ensure_workerw_ready` 重复做 `is_workerw_valid()` 早退 | 185 早退后 190 仍调 `ensure_workerw_ready`，后者 224 再判同一条件 | Low | ✅ 已清理（委托 `ensure_workerw_ready`） |
| R-2 | mod.rs:115/229/242 | `initialized` 字段可由 `workerw_hwnd.is_some()` 派生 | 仅 229 读、242 写；`is_workerw_valid`（558）已不检查该字段；556-557 自述"句柄均为 Some 隐含已初始化" | Low | ✅ 已清理（删除冗余状态字段） |
| R-3 | worker_w.rs:379-388 | `find_child_by_class` 首段为 never-loop（等价单个 `if`），被 `#[allow(clippy::never_loop)]` 掩盖 | 循环体内 `child` 从不推进 | Low | ✅ 已清理（改为单次 `if let` 并移除 allow） |
| R-4 | native_wallpaper.rs:161/190、166/170/201/212 | 注册表路径 `"Control Panel\\Desktop"` 与值名 `WallPaperStyle`/`TileWallpaper` 多处硬编码 | 同文件 2+4 处字面量重复 | Low | ✅ 已清理（提为模块常量） |
| R-5 | worker_w.rs:557-579 ↔ native_wallpaper.rs:227-261 | `restore_system_wallpaper` 与 `set_wallpaper_image` 同构（`SPI_SETDESKWALLPAPER` + UTF-16 + 双 flag + 内嵌 NUL 校验） | 两函数体逐行对应，仅错误文案不同 | Medium | ⏳ 待确认（跨模块错误语义） |
| R-6 | native_wallpaper.rs:95-110 | `clear_native_wallpaper` 是第三处 `SPI_SETDESKWALLPAPER` 调用 | 同 R-5 家族 | Low | ⏳ 待确认（同 R-5） |
| R-7 | worker_w.rs:252-290 ↔ 294-326 | 两个 EnumWindows 回调骨架同构（差异仅是否前置 WorkerW 类名过滤） | 结构对照 | Low | ⏳ 待确认（枚举语义） |
| R-8 | worker_w.rs:443-445 ↔ manager.rs:1698-1705、1918-1925 | `make_borderless`+`remove_from_taskbar` Step1 在 manager.rs 两处复制 | manager.rs 注释自述"与 embed_wallpaper Step 1 同源" | Low | ⏳ 待确认（跨模块） |
| R-9 | mod.rs:954/965/993、875/894/900、907/941 | 三组重复冒烟测试（enumerate_displays ×3、未初始化 ×3、恢复 None ×2） | 均只做等价断言 | Low | ⏳ 待确认（测试策略） |
| R-10 | mod.rs:1006/1021/1039/1078/1110/1162/1201/1232/1306 | 9 个 `include_str!` 文档化测试断言「源码注释文本存在」，脆弱低价值 | 重排/改写注释即红；契约已在 docs 重复记录 | Medium | ⏳ 待确认（`manager.rs:4180` 显式引用 `pure_mode_half_detach_recovery_markers`） |
| R-11 | native_wallpaper.rs:4 / config/detect.rs:15 / src-tauri/commands/system.rs:139 | 图片扩展名清单三处重复且内容不一 | 三份清单 dib/webp/ico 增删不同 | Medium | ⏳ 待确认（跨模块单一真源） |

### 7.3 过度抽象

| # | 位置 | 问题 | 证据 | 级别 | 处置 |
|---|---|---|---|---|---|
| O-1 | mod.rs:593-595 | `check_and_reinitialize` 单行间接层，委托私有 `ensure_workerw_ready` | 外部调用 5 处（system.rs:92、workerw_check.rs:132、explorer.rs:48、manager.rs:2419/2437） | Low | ⏳ 待确认（D-TD-003 已决策保留；公开 API 名） |
| O-2 | mod.rs:602-604 | `DesktopIntegrator::enumerate_displays` thin wrapper，等价自由函数 | 方法与自由函数均被 src-tauri 大量引用 | Low | ⏳ 待确认（D-TD-004 已决策保留） |
| O-3 | native_wallpaper.rs:33-45 | `set_native_wallpaper` 的 `monitor_id: Option<&str>` 无调用者传非 None | 全部调用点传 None（manager.rs:495/979、测试 431） | Medium | ⏳ 待确认（公开签名变更） |
| O-4 | worker_w.rs:435-441 | `embed_wallpaper` 的 `displays` 参数在 `Arrangement::Span` 分支未使用 | 文档自述；调用方需构造无用 `&[]` | Low | ⏳ 待确认（公开签名；见 6.2） |

**说明**：`calculate_per_monitor_child_coords`（worker_w.rs:418，单调用但承担 D-002 回归测试）、`compute_retry_wait_ms`（worker_w.rs:603，跨模块可测契约）、`hide_window_on_reattach_failure`（mod.rs:82，3 处复用）均为**必要抽象，不建议改动**。

### 7.4 文档一致性（并入冗余清理）

| # | 位置 | 问题 | 级别 | 处置 |
|---|---|---|---|---|
| F-1 | mod.rs:6-53 | 模块头「职责划分」仅列两层，漏述模块级自由函数（`enumerate_displays`/`hide_window_on_reattach_failure`/`extract_utf16_string`）；「并发约束」最坏阻塞 ≈220ms 与 `ensure_workerw_ready` 注释「N×200ms」口径不一 | Low | ✅ 已清理（注释同步） |

### 7.5 已实施的安全清理（零行为变更）

- R-1 `ensure_initialized` 去除重复 `is_workerw_valid()` 早退，改为委托 `ensure_workerw_ready`
- R-2 删除可派生的冗余状态字段 `initialized`，改由 `workerw_hwnd.is_some()` 表达
- R-3 `find_child_by_class` 首段 never-loop 改为单次 `if let`，移除 `#[allow(clippy::never_loop)]`
- R-4 注册表路径/值名字面量提为模块常量
- F-1 模块头文档与实现口径对齐

### 7.6 待确认项（涉及公开 API / 跨模块契约 / 测试策略 / v41-D-xxx 契约，本轮不实施）

D-1、D-3、R-5、R-6、R-7、R-8、R-9、R-10、R-11、O-1、O-2、O-3、O-4。

> - **O-1/O-2** 为 D-TD-003/D-TD-004 已决策保留项，本次复核确认其「单行/薄包装」性质成立，维持保留。
> - **R-5/R-6** 涉及跨模块错误文案与三处 `SPI_SETDESKWALLPAPER` 收敛，需先确认错误映射契约。
> - **R-10** 文档化测试牵涉 `manager.rs:4180` 的跨模块测试名引用，需与测试策略一并决策。
> - **O-3/O-4** 为公开签名收敛，需改调用方。