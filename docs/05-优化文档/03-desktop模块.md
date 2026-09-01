# desktop 模块优化文档

> [← 返回索引](./README.md)

## 1. 模块概览与现状

> 现状摘要来源：v6.0 技术债审查（2026-07-25）。本文档合并 v4.0 审查发现与 v6.0 技术债清单及清理状态。

### 1.1 模块职责

desktop 模块负责将壁纸窗口嵌入 Windows 桌面的 WorkerW 层（位于桌面图标层之下），并管理显示器枚举、原生壁纸设置等与 Win32 桌面环境的交互。模块分为两层：`DesktopIntegrator`（状态层，持有 progman/workerw 句柄与活跃壁纸表，提供懒加载、嵌入、移除、失效检测等高级 API）与 `worker_w` 无状态操作层（提供纯函数式 Win32 操作），后者又包含 `window`（窗口样式操作）与 `native_wallpaper`（注册表 + SystemParametersInfoW 原生壁纸）两个子模块。

核心功能与设计模式：

- WorkerW 发现：`find_workerw_no_retry` 通过 `FindWindowW(Progman)` + Progman 直查快速路径（D-PERF-002）+ `EnumWindows`（`try_enum_windows` 封装）+ `SendMessageTimeoutW(0x052C)` 多策略查找
- 窗口嵌入：`SetParent` 将壁纸窗口挂到 WorkerW 下；PerMonitor 分支使用 `calculate_per_monitor_child_coords` 处理负坐标偏移（D-002）
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

### [D-011] [Low] [逻辑] window.rs — `set_mouse_passthrough(false)` 保留 `WS_EX_LAYERED` 但契约未文档化

**描述**：`set_mouse_passthrough(enabled=false)` 分支仅移除 `WS_EX_TRANSPARENT`、保守保留 `WS_EX_LAYERED`，但窗口需要 `SetLayeredWindowAttributes` 配置才能正常渲染，契约未文档化。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。函数 doc comment（window.rs:73-93）现显式声明"分层属性契约"：调用方需确保窗口已通过 `SetLayeredWindowAttributes` 配置分层属性，本函数仅切换 `WS_EX_TRANSPARENT` 位；`false` 时保留 `WS_EX_LAYERED`（D-011）。

### [D-012] [Low] [可维护性] worker_w.rs — `SendMessageTimeoutW` 超时参数 200ms 为内联魔法数

**描述**：`SendMessageTimeoutW` 超时参数 `200`（ms）为内联魔法数。

**修复状态**：✅ 已修复于 v4.0 Wave 3B。现提取为命名常量 `WM_SPAWN_WORK_TIMEOUT_MS: u32 = 200`（worker_w.rs:35），并有消除歧义注释。

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
| D02 | Medium | `get_system_wallpaper` 固定 260 缓冲区，长路径/UNC 路径被截断 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 D-009 发现扩大至 32767 后栈分配接近上限（现 D-009 已改堆分配） |
| D03 | Medium | `set_mouse_passthrough` 修改 GWL_EXSTYLE 后未调用 `SetWindowPos(SWP_FRAMECHANGED)` 刷新 | ✅ 已修复（v3.5.2）— 现经 `refresh_frame_changed` 辅助函数（D-TD-006） |
| D04 | Medium | `embed_wallpaper` PerMonitor 未匹配 display_id 时静默回退无 warning | ✅ 已修复（v3.5.2） |
| D05 | Medium | `ensure_workerw_ready` 重新嵌入失败条目残留导致状态不一致 | ✅ 已修复（v3.5.2） |
| D06 | Medium | `find_workerw()` 含 10 次 sleep 重试为死代码，pub 易被误用于 async 上下文 | ✅ 已修复（v3.5.2） |
| D07 | Medium | `SetWindowLongPtrW` 返回值 0 歧义无法可靠检测失败 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 D-003 发现 `make_borderless` 仍恒返回 Ok（现 D-003 已修复） |
| D08 | Low | `EnumDisplayMonitors`/`GetMonitorInfoW` 失败静默无日志 | ✅ 已修复（v3.5.3） |
| D09 | Low | `SendMessageTimeoutW` 的 `result` 变量 dead store | ✅ 已修复（v3.5.3） |
| D10 | Low | 魔法数字（260/0x052C/3/10）未定义为命名常量 | ✅ 已修复（v3.5.3）— ⚠️ v4.0 D-012 发现 200ms 超时仍为魔法数（现 D-012 已修复） |
| D11 | Low | `enum_windows_callback` 与 `fallback_enum_callback` 兄弟窗口检查逻辑不一致 | ✅ 已修复（v3.5.3）— 统一为 `try_find_sibling_workerw` |
| D12 | Low | `restore_original_wallpaper`/`restore_system_wallpaper` 返回 `()` 调用方无法感知失败 | ✅ 已修复（v3.5.3）— 返回 `Result` |
| 硬编码布局忽略用户配置 | — | `check_and_reinitialize` 从 HashMap 取实际 arrangement | ✅ 已修复（v1.0） |
| remove_wallpaper 空操作 | — | `IsWindow` 检查 + `SetParent(hwnd, None)` 分离窗口 | ✅ 已修复（v1.0）— ⚠️ v4.0 D-005 发现未隐藏窗口（现 D-005 已修复） |
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