# v6.0 技术债审查 - desktop 模块

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 模块路径：`crates/mirrorstar-core/src/desktop/`

## 1. 当前状态摘要

### 1.1 模块职责

desktop 模块负责将壁纸窗口嵌入 Windows 桌面的 WorkerW 层（位于桌面图标层之下），并管理显示器枚举、原生壁纸设置等与 Win32 桌面环境的交互。模块分为两层：`DesktopIntegrator`（状态层，持有 progman/workerw 句柄与活跃壁纸表，提供懒加载、嵌入、移除、失效检测等高级 API）与 `worker_w` 无状态操作层（提供纯函数式 Win32 操作），后者又包含 `window`（窗口样式操作）与 `native_wallpaper`（注册表 + SystemParametersInfoW 原生壁纸）两个子模块。

### 1.2 文件清单

| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 1033 | 模块导出与 DesktopIntegrator 状态层（懒加载/嵌入/移除/显示器缓存） |
| worker_w.rs | 1000 | WorkerW 窗口查找与壁纸嵌入、系统壁纸读写、重试节奏 |
| native_wallpaper.rs | 615 | 原生壁纸设置（注册表事务回滚 + SystemParametersInfoW） |
| window.rs | 232 | 窗口样式工具函数（去边框/移除任务栏/鼠标穿透） |

### 1.3 测试覆盖

测试分布存在明显倾斜：mod.rs（~440 行测试，含大量 `include_str!` 文档化断言）、worker_w.rs（~380 行测试，含纯函数测试与 Progman 烟雾测试）、native_wallpaper.rs（~345 行测试，含注册表 round-trip 与事务回滚集成测试）、window.rs（~90 行测试，全为 `#[ignore]` Progman 烟雾测试）。涉及 Win32 API 的失败分支普遍通过 `include_str!` 模式断言源码标记验证，无法在 CI 中可靠触发真实失败分支。

## 2. 技术债清单

### 2.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-001 | mod.rs:402-404 | `DesktopIntegrator::workerw_hwnd()` getter 仅在 mod.rs 自身的单元测试（:723, :743）中调用，生产代码（crates/ 与 src-tauri/）无任何调用点（Grep 验证） | 维护无用的公共 API 表面，增加读者认知负担 | 删除该 getter；若测试需访问句柄，改用 `#[cfg(test)]` 内部访问器或将测试改为通过 `is_workerw_valid()` 间接验证 | 低 |
| D-TD-002 | mod.rs:407-409 | `DesktopIntegrator::progman_hwnd()` getter 仅在 mod.rs 自身的单元测试（:719, :742）中调用，生产代码无任何调用点（Grep 验证） | 同上 | 同上，删除该 getter | 低 |

### 2.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-003 | mod.rs:435-437 | `check_and_reinitialize` 方法体仅一行 `self.ensure_workerw_ready()`，无任何附加逻辑。doc comment 长达 15 行描述其委托关系 | 单行间接层，读者需跳转两层才能理解行为；doc comment 与 `ensure_workerw_ready` 重复 | 将调用方（manager.rs:1101, 1119）直接改为调用 `ensure_workerw_ready`（需改为 pub），或在 `ensure_workerw_ready` doc 中明确"对外入口"语义后移除本方法 | 中 |
| D-TD-004 | mod.rs:440-442 | `DesktopIntegrator::enumerate_displays` 方法体仅一行 `enumerate_displays()`（调用同名独立函数），无附加逻辑 | thin wrapper，仅为面向对象风格提供方法形态 | 保留（system.rs:20 等生产调用依赖方法形态），但在 doc 中注明"等价于独立函数 `enumerate_displays()`"以避免误解 | 低 |

### 2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-005 | window.rs:10-57, 60-89, 108-141 | `SetWindowLongPtrW` + `SetLastError(0)` + 调用 + `GetLastError` 非 0 判断 + `tracing::warn!` 模式在 4 处重复（make_borderless 的 GWL_STYLE/GWL_EXSTYLE、remove_from_taskbar、set_mouse_passthrough） | 4 段近乎相同的 12 行模式，错误处理风格分散；修改需同步 4 处 | 抽取 `fn set_window_long_with_error(hwnd, index, value, context) -> Result<(), MirrorStarError>` 辅助函数，统一 SetLastError/GetLastError/warn 模式 | 中 |
| D-TD-006 | window.rs:74-88, 126-140 | `SetWindowPos(hwnd, HWND::default(), 0,0,0,0, SWP_NOMOVE\|SWP_NOSIZE\|SWP_NOZORDER\|SWP_FRAMECHANGED)` + 失败 `warn!` 模式在 remove_from_taskbar 与 set_mouse_passthrough 中重复 | 2 段相同 14 行模式 | 抽取 `fn refresh_frame_changed(hwnd)` 辅助函数 | 低 |
| D-TD-007 | worker_w.rs:160-173, 204-216, 232-244 | 三次 EnumWindows 调用都采用 `if let Err(e) = EnumWindows(...) { if xxx.is_none() { warn!; return Err(...) } debug! }` 完全相同的 12 行模式（仅 Step 标识与变量名不同） | 3 段相同结构，添加新 EnumWindows 步骤时易遗漏 is_none 误报处理 | 抽取 `fn try_enum_windows(callback, lparam, step_label) -> Result<Option<HWND>, MirrorStarError>` 封装 Err 误报处理逻辑 | 中 |
| D-TD-008 | worker_w.rs:460-463, 480-482, 488-490 | `embed_wallpaper` 中 `GetWindowRect(workerw_hwnd, &mut workerw_rect).map_err(\|e\| MirrorStarError::DesktopIntegration(format!("GetWindowRect 失败: {}", e)))?` 在 Span 分支、PerMonitor 找到分支、PerMonitor fallback 分支重复 3 次 | 3 段相同 4 行错误转换 | 在 match 前统一获取一次 `workerw_rect`，三个分支共用 | 中 |
| D-TD-009 | mod.rs:543-546, worker_w.rs:550-553 | UTF-16 字符串提取逻辑：mod.rs 抽取了 `extract_utf16_string` 纯函数，worker_w.rs `get_system_wallpaper` 内联实现了相同的 `position(\|&c\| c == 0).unwrap_or(len)` + `String::from_utf16_lossy` 逻辑 | 同一逻辑两处实现，mod.rs 测试注释明确说"与 mod.rs extract_utf16_string 保持一致"，但未实际复用 | 在 worker_w.rs 中调用 `crate::desktop::extract_utf16_string`（需调整为 pub(crate)），或将其上移至共享位置 | 低 |

### 2.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-010 | worker_w.rs:359-378 | `check_sibling_workerw` 使用 `String::from_utf16_lossy` + `WORKERW_CLASS` 字符串比较（:371-372），而 `is_workerw_class`（:80-85）、`enum_windows_callback`（:267-269）、`fallback_enum_callback`（:310）均已迁移至 `eq_wide` 字节级比较（v5.0 D-PERF-001 优化） | 同一模块内类名比较存在两种风格；`check_sibling_workerw` 路径未受益于 D-PERF-001 的零分配优化，每次兄弟窗口检查都分配 String | 将 `check_sibling_workerw` 改为调用 `is_workerw_class(sibling)`，移除 `WORKERW_CLASS` 常量（:53，仅此处使用）与 `String::from_utf16_lossy` 调用 | 低 |
| D-TD-011 | worker_w.rs:381-415 | `find_child_by_class` 注释（:391）"原 loop 写法因找到即 return 永不真正循环（clippy::never_loop），改为 if let" — 修复痕迹，且当前实现首段 if let（:392-396）与次段 loop（:400-412）混用两种写法 | 修复注释保留在代码中，且同一函数内两种迭代写法不一致 | 移除修复注释，统一为 loop 写法（首段直接在 loop 内 find 后 return） | 低 |

### 2.5 未使用导入

无。经 Grep 验证，4 个文件的 `use` 语句引入的项（含 mod.rs 的 `GetLastError`/`BOOL`/`HWND`/`LPARAM`/`RECT`、worker_w.rs 的 `WPARAM`/`WIN32_ERROR`/`SetLastError` 等）均有实际调用点。`pub mod native_wallpaper`/`window`/`worker_w` 三个子模块导出均被外部 crate 与 src-tauri 引用。

### 2.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-012 | mod.rs:315-326, 313-314 | `get_cached_displays` 返回 `Result<&[DisplayInfo], MirrorStarError>`，但 doc comment 明确"当前 `enumerate_displays` 不会失败"，`Ok(self.cached_displays.as_ref().unwrap().0.as_slice())` 恒返回 Ok。Result 包装仅为"将来 enumerate_displays 改为 Result 时无需调整签名"保留 | 为不存在的失败场景保留 Result，调用方需写 `?` 处理永远不会到来的 Err；YAGNI 违规 | 移除 Result 包装，直接返回 `&[DisplayInfo]`；将来 enumerate_displays 改为 Result 时再调整签名（成本极低，仅 2 处调用） | 低 |
| D-TD-013 | native_wallpaper.rs:192-231 | `read_wallpaper_style` 返回 `Result<Option<(String, String)>, MirrorStarError>`，但 doc comment（:191）明确"外层 `Result` 保留 `Err` 变体以维持 API 一致性（当前实现不返回 `Err`）"。所有失败路径（打开注册表失败、读取 WallPaperStyle/TileWallpaper 失败）均 `return Ok(None)` | 为不存在的失败场景保留 Err 变体，调用方需处理永远不会到来的 Err | 移除外层 Result，直接返回 `Option<(String, String)>` | 低 |
| D-TD-014 | native_wallpaper.rs:50-59 | `set_native_wallpaper` 中 `match read_wallpaper_style() { Ok(opt) => opt, Err(e) => { warn!(...); None } }` 处理 Err 分支，但注释（:49）明确"此处额外处理 Err 变体以维持类型契约的完整匹配（防御性，当前不会触发）" | 防御性代码处理永远不会到来的 Err 分支，增加阅读负担 | 依赖 D-TD-013 清理后，本分支自然消失；或直接简化为 `let old_values = read_wallpaper_style().unwrap_or(None);` | 低 |

### 2.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-015 | mod.rs:6 | 模块级 doc comment 标题"## 职责划分（v41-D-017）"引用 v41-D-017 标记，但 Grep 全项目（crates/ + src-tauri/）未找到 v41-D-017 对应的 spec 文档或其他引用（.trae/specs/v41-deep-review-and-performance-optimization/v41-findings.md 中无 D-017 条目） | 引用已失效的 spec 编号，读者无法追溯 | 移除括号内的 v41-D-017 标记，保留"## 职责划分"标题 | 低 |
| D-TD-016 | mod.rs:167 | `ensure_workerw_ready` doc comment 中"消除 DRY 违规（N-001）"引用 N-001 标记，Grep 显示 N-001 仅此一处出现，无对应 spec | 引用已失效的 spec 编号 | 移除"（N-001）"标记，保留"消除 DRY 违规"描述 | 低 |
| D-TD-017 | mod.rs:158-160, 177-179, 266, 430-434 | 4 处 "T14：" 前缀注释描述 `ensure_workerw_ready` 返回 bool 的设计理由。T14 是历史 task 编号，对当前读者无意义 | 历史任务标记散落 4 处，增加注释噪音 | 移除"T14："前缀，保留设计理由描述 | 低 |
| D-TD-018 | mod.rs:191, 283; worker_w.rs:187; window.rs:107 | 4 处"参考 Phase 3 优化"/"参考 Phase 3 优化（锁外执行查找 / 异步初始化）"引用未实现的未来工作。Phase 3 在代码库中无对应实现或 spec | 引用不存在的未来工作，读者无法判断是否仍计划实施 | 评估 Phase 3 是否仍为路线图项；若否，移除引用；若是，在路线图文档中记录并在代码注释中链接 | 低 |
| D-TD-019 | worker_w.rs:903, 910; window.rs:154 | 测试注释中"Wave 2C/2D 风格统一使用 include_str! 模式"引用 Wave 2C/2D 历史标记，该标记在代码库中无对应 spec | 历史标记对当前读者无意义 | 移除"Wave 2C/2D"引用，保留"统一使用 include_str! 模式"描述 | 低 |
| D-TD-020 | worker_w.rs:6-19 | 模块级 doc comment "## EnumWindowsProc 捕获变量风格（v41-D-012）"用 14 行描述"将来若改用闭包封装时的统一风格"（Cell/RefCell 选择标准），但当前实现使用裸指针，且代码库无引入闭包封装层的计划 | 为不存在的未来重构预留风格指南，与当前实现脱节；v41-D-012 在 .trae/specs 中无明确对应 | 精简为"当前使用裸指针 + LPARAM 模拟捕获（EnumWindows 限制）"，移除未来 Cell/RefCell 风格指南 | 低 |

### 2.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-021 | worker_w.rs:153, 199, 227, 246; mod.rs:139, 211, 402 | WorkerW 句柄变量命名不一致：worker_w.rs 内部用 `workerw`（:153, :199, :227, :246），mod.rs 用 `workerw_hwnd`（:139, :211, :402）；同样 progman 在 worker_w.rs 用 `progman`（:106），mod.rs 用 `progman_hwnd`（:138, :407） | 同一概念两种命名，跨文件阅读时需心理映射 | 统一为 `workerw_hwnd`/`progman_hwnd`（更明确表类型），或统一为 `workerw`/`progman`（更简洁） | 中 |
| D-TD-022 | window.rs:10, 60, 108 | 错误处理风格不一致：`make_borderless` 返回 `Result<(), MirrorStarError>`（D-003 修复后），但 `remove_from_taskbar` 与 `set_mouse_passthrough` 返回 `()`，失败仅 `warn!` 吞错。.trae/specs/fix-review-2026-07-02-batch3h-remaining-issues/checklist.md:75 明确保留此不一致 | 三个同类窗口样式函数错误处理风格分裂，调用方无法感知 remove_from_taskbar/set_mouse_passthrough 失败 | 评估是否统一为 Result（与 make_borderless 一致）；若保留 `()`，在 doc 中明确"失败仅 warn，调用方不感知"契约 | 中 |
| D-TD-023 | worker_w.rs:259, 301 | 回调函数命名风格不一致：`enum_windows_callback`（无业务前缀）vs `fallback_enum_callback`（有 fallback 前缀 + 不同语序）。两者均为 EnumWindows 回调，仅查找策略不同 | 命名模式不统一，读者需思考为何一个用 enum_windows_ 前缀另一个用 _enum_ 后缀 | 统一为 `find_workerw_callback`/`find_workerw_fallback_callback`，或 `primary_callback`/`fallback_callback` | 低 |

### 2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| D-TD-024 | native_wallpaper.rs:189-191, 377-380, 404-407 | `read_wallpaper_style` doc comment（:191）"外层 `Result` 保留 `Err` 变体以维持 API 一致性（当前实现不返回 `Err`）"；测试 `read_wallpaper_style_live`（:377-380）也注释"按契约：read_wallpaper_style 的 Err 变体当前不返回...但保留 Err 分支以维持类型契约的完整匹配"；测试 `rollback_on_image_failure_live`（:404-407）也保留 `Err(e) => panic!(...)` 分支 | 三处注释明确承认当前不返回 Err 但保留 Err 处理，注释与代码行为不符（代码不会进入 Err 分支） | 依赖 D-TD-013 清理后，这些注释自然消失 | 低 |
| D-TD-025 | mod.rs:312-314 | `get_cached_displays` doc comment "返回 `Result` 以便将来 `enumerate_displays` 改为 `Result` 时无需调整签名（当前 `enumerate_displays` 不会失败）" — 注释明确承认当前不会失败但保留 Result 包装 | 注释与代码行为不符（代码恒返回 Ok），YAGNI 违规的文档化 | 依赖 D-TD-012 清理后，此注释自然消失 | 低 |
| D-TD-026 | worker_w.rs:96-102 | `find_workerw_no_retry` doc comment "v41-D-015: 长期重构方向为将'未找到 WorkerW'语义从 `Err(WorkerWNotFound)` 改为 `Option<HWND>`...当前接受 `Result<..., MirrorStarError>` 现状" — 描述长期重构方向但无实施计划，v41-D-015 在 .trae/specs 中无明确对应 | 引用无实施计划的长期重构方向，读者无法判断是否仍将实施 | 评估 v41-D-015 是否仍为路线图项；若否，移除该段；若是，在路线图中记录 | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0 高收益低风险）

- D-TD-001: 删除 `workerw_hwnd()` getter（仅测试使用，生产无调用）
- D-TD-002: 删除 `progman_hwnd()` getter（仅测试使用，生产无调用）
- D-TD-009: worker_w.rs `get_system_wallpaper` 复用 mod.rs `extract_utf16_string`，消除重复实现
- D-TD-010: `check_sibling_workerw` 改用 `is_workerw_class`，移除 `WORKERW_CLASS` 常量，统一类名比较风格
- D-TD-011: 移除 `find_child_by_class` 修复注释，统一迭代写法
- D-TD-012: 移除 `get_cached_displays` 的 Result 包装（当前恒返回 Ok）
- D-TD-013: 移除 `read_wallpaper_style` 的外层 Result（当前不返回 Err）
- D-TD-014: 简化 `set_native_wallpaper` 中 `read_wallpaper_style` 的 Err 处理（依赖 D-TD-013）
- D-TD-015: 移除 mod.rs:6 的 v41-D-017 失效 spec 引用
- D-TD-016: 移除 mod.rs:167 的 N-001 失效 spec 引用
- D-TD-017: 移除 4 处 "T14：" 历史任务前缀
- D-TD-019: 移除 3 处 "Wave 2C/2D" 历史标记
- D-TD-023: 统一 `enum_windows_callback`/`fallback_enum_callback` 命名风格
- D-TD-024: 依赖 D-TD-013 清理后消除注释与代码不符
- D-TD-025: 依赖 D-TD-012 清理后消除注释与代码不符

### 3.2 谨慎清理（P1/P2 中收益）

- D-TD-003: 评估 `check_and_reinitialize` 是否可内联到调用方（需 `ensure_workerw_ready` 改为 pub）
- D-TD-005: 抽取 `set_window_long_with_error` 辅助函数统一 4 处 SetWindowLongPtrW 模式
- D-TD-006: 抽取 `refresh_frame_changed` 辅助函数统一 2 处 SetWindowPos 模式
- D-TD-007: 抽取 `try_enum_windows` 封装 EnumWindows 误报处理逻辑
- D-TD-008: 在 `embed_wallpaper` match 前统一获取 `workerw_rect`
- D-TD-018: 评估 "Phase 3 优化" 引用是否仍为路线图项
- D-TD-020: 精简 worker_w.rs:6-19 的未来闭包封装风格指南
- D-TD-021: 统一 workerw/workerw_hwnd 变量命名
- D-TD-022: 评估 remove_from_taskbar/set_mouse_passthrough 是否统一为 Result
- D-TD-026: 评估 v41-D-015 长期重构方向是否仍计划实施

### 3.3 评估后决定（P3 长期或低收益）

- D-TD-004: `enumerate_displays` 方法保留（有生产调用），仅需补 doc 说明等价关系

## 4. 优化机会（非技术债类改进点）

- **EnumWindows 三步查找的可观测性**：worker_w.rs Step 2/4/5 三次 EnumWindows 调用目前仅在 debug 日志区分，可考虑增加 metric counter（find_workerw_attempt_total{step="2|4|5"}）量化 Progman 直查快速路径命中率，验证 D-PERF-002 优化效果。
- **`embed_wallpaper` 的 `displays` 参数语义**：当前 Span 分支不使用 displays 参数（worker_w.rs:442-443 注释明确），但 PerMonitor 分支必须传入。可考虑用枚举类型在类型层表达"Span 不需要 displays"的契约，避免调用方构造无用 Vec。
- **`SetWindowPos` Z-order 处理**：worker_w.rs:508 使用 `HWND_BOTTOM`，但 mod.rs:275 doc comment 提到"半嵌入"风险时未涉及 Z-order 回滚。可评估 SetWindowPos 失败时是否需重置 Z-order。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项

- D-002 ~ D-015、D08、D11、D12 等 v4.x 系列修复已在代码中通过 `D-xxx` 注释标记固化，并在测试中通过 `include_str!` 模式断言验证。本审查不重复记录这些已修复项，仅记录其修复痕迹本身（见 2.7）。
- v4.0 的 `D-004`/`D-005`/`D-007`/`D-014` 等修复引入的"文档化测试"模式（`include_str!` + 源码字符串断言）是当前测试主体形态，本审查未将其列为技术债，但 D-TD-019 标记了其中"Wave 2C/2D"历史标记的清理需求。

### 5.2 v5.0 已覆盖项

- v5.0 D-PERF-001（UTF-16 字节级比较）、D-PERF-002（Progman 直查快速路径）、D-PERF-007（重试节奏优化）已实施。本审查 D-TD-010 标记了 D-PERF-001 未同步到 `check_sibling_workerw` 的过时模式。
- v5.0 优化后 `compute_retry_wait_ms` 重试次数从 10 次降为 6 次（worker_w.rs:616），相关测试已同步更新。

### 5.3 v6 新发现

- **死代码**（D-TD-001/002）：v4/v5 未识别 `workerw_hwnd()`/`progman_hwnd()` getter 仅测试使用的问题，本次通过 Grep 跨 crates/ + src-tauri/ 验证确认。
- **过度设计的 Result 包装**（D-TD-012/013/014）：v4/v5 的 D12 修复将 `restore_original_wallpaper`/`restore_system_wallpaper` 改为返回 Result，但 `get_cached_displays`/`read_wallpaper_style` 的 Result 包装是同期"为未来一致性"保留的过度设计，本次首次识别。
- **v41-D-017 失效引用**（D-TD-015）：mod.rs:6 引用的 v41-D-017 在 spec 文档中无对应条目，本次通过跨项目 Grep 验证确认。
- **`check_sibling_workerw` 过时模式**（D-TD-010）：v5.0 D-PERF-001 优化未覆盖该函数，本次首次识别。

## 6. v6.0 清理状态汇总

> 清理日期：2026-07-25 | 衍生 spec：`cleanup-v6-desktop-tech-debt-2026-07-25`

### 6.1 P0 项（15 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| D-TD-001 | 死代码 | ✅ 已修复于 v6.0 | 删除 `workerw_hwnd()` getter，测试改用直接字段访问 |
| D-TD-002 | 死代码 | ✅ 已修复于 v6.0 | 删除 `progman_hwnd()` getter，测试改用直接字段访问 |
| D-TD-009 | 重复实现 | ✅ 已修复于 v6.0 | `get_system_wallpaper` 复用 `extract_utf16_string`（可见性调整为 `pub(crate)`） |
| D-TD-010 | 过时模式 | ✅ 已修复于 v6.0 | `check_sibling_workerw` 改用 `is_workerw_class` 字节级比较，移除 `WORKERW_CLASS` 常量 |
| D-TD-011 | 修复痕迹 | ✅ 已修复于 v6.0 | `find_child_by_class` 移除修复注释，统一为 loop 写法 |
| D-TD-012 | 过度设计 | ✅ 已修复于 v6.0 | `get_cached_displays` 移除 Result 包装，直接返回 `&[DisplayInfo]` |
| D-TD-013 | 过度设计 | ✅ 已修复于 v6.0 | `read_wallpaper_style` 移除外层 Result，直接返回 `Option<(String, String)>` |
| D-TD-014 | 过度设计 | ✅ 已修复于 v6.0 | `set_native_wallpaper` 简化 Err 处理（依赖 D-TD-013） |
| D-TD-015 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 `v41-D-017` 失效 spec 引用 |
| D-TD-016 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 `N-001` 失效 spec 引用 |
| D-TD-017 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 4 处 `T14：` 历史任务前缀 |
| D-TD-019 | 修复痕迹 | ✅ 已修复于 v6.0 | 移除 3 处 `Wave 2C/2D` 历史标记 |
| D-TD-023 | 命名一致性 | ✅ 已修复于 v6.0 | `enum_windows_callback`/`fallback_enum_callback` 重命名为 `find_workerw_callback`/`find_workerw_fallback_callback` |
| D-TD-024 | 注释陈旧 | ✅ 已修复于 v6.0 | 依赖 D-TD-013 清理后消除注释与代码不符 |
| D-TD-025 | 注释陈旧 | ✅ 已修复于 v6.0 | 依赖 D-TD-012 清理后消除注释与代码不符 |

### 6.2 P1/P2 项（10 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| D-TD-003 | 冗余抽象 | ✅ 已决策保留 | `check_and_reinitialize` 保留，doc 补充"对外入口（等价于私有 `ensure_workerw_ready`）"说明 |
| D-TD-005 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `set_window_long_with_error` 辅助函数统一 4 处 SetWindowLongPtrW 模式 |
| D-TD-006 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `refresh_frame_changed` 辅助函数统一 2 处 SetWindowPos 模式 |
| D-TD-007 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `try_enum_windows` 辅助函数封装 EnumWindows 误报处理 |
| D-TD-008 | 重复实现 | ✅ 已修复于 v6.0 | `embed_wallpaper` 中 `workerw_rect` 在 match 前统一获取 |
| D-TD-018 | 修复痕迹 | ✅ 已决策移除 | Phase 3 引用为历史规划残留，移除 4 处引用 |
| D-TD-020 | 修复痕迹 | ✅ 已修复于 v6.0 | `worker_w.rs` 模块文档精简，移除 `v41-D-012` 标记与未来 Cell/RefCell 风格指南 |
| D-TD-021 | 命名一致性 | ✅ 已修复于 v6.0 | `workerw`→`workerw_hwnd`、`progman`→`progman_hwnd` 变量命名统一 |
| D-TD-022 | 命名一致性 | ✅ 已决策保留 | `remove_from_taskbar`/`set_mouse_passthrough` 保留 `()` 返回，doc 补充"失败仅 warn，调用方不感知"契约 |
| D-TD-026 | 注释陈旧 | ✅ 已决策移除 | `v41-D-015` 长期重构方向为历史规划残留，移除 doc comment 该段 |

### 6.3 P3 项（1 项）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| D-TD-004 | 冗余抽象 | ✅ 已决策保留 | `enumerate_displays` thin wrapper 保留（有生产调用），doc 补充"等价于独立函数 `enumerate_displays()`"说明 |

### 6.4 清理统计

- **总技术债**：26 项
- **已修复**：21 项（80.8%）
- **已决策保留**：5 项（19.2%，D-TD-003/004/018/022/026）
- **完成率**：100%

### 6.5 验证结果

- `cargo test -p mirrorstar-core desktop::`：57 tests passed / 0 failed / 部分 ignored（预期行为）
- `cargo clippy --workspace --all-targets -- -D warnings`：零警告
- `cargo test --workspace`：全部通过
- `cargo test -p mirrorstar-core config::`：150 passed / 0 failed（回归验证）
- `npx vitest run`：208 passed / 0 failed
- `npm run lint` + `npm run typecheck`：零错误
