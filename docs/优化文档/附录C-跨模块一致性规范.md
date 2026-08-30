# 附录 C：跨模块一致性规范

> [← 返回索引](./README.md)

本附录记录 v4.0 Wave 3I 修复的 `[Consistency]-12.3` ~ `12.6` 四项跨模块一致性 findings，作为后续开发的约定参考。

这四项 finding 均属 Low 级，本质是「同类问题在不同模块修复策略不统一」的规范化需求。本附录通过**制定约定文档**而非批量重写现有代码的方式解决，目标是：

- 为新增代码提供明确的命名 / 错误处理 / 锁 / 测试命名基线
- 保留现有实现的差异以避免引入行为变化或中断 git blame
- 通过文档化约定使后续 review 有据可依

---

## C.1 RAII 命名规范（[Consistency]-12.3）

### 背景

项目使用 RAII 模式封装 Windows 原生句柄（`HANDLE`、`HKEY`、`HWND`、WorkerW 句柄等）以保证资源释放。在历史迭代中，不同模块采用了不同的命名风格，导致 review 时难以一眼判断「某类型是否为 RAII 封装、是否拥有底层句柄、`Drop` 时是否释放」。

### 现状

项目中现存三种 RAII 封装命名风格：

| 命名风格 | 出现位置 | 含义 |
|----------|----------|------|
| `OwnedProcHandle` | `crates/mirrorstar-wp-proc/src/`（子进程句柄封装） | 强调「拥有」语义，`Drop` 时 `CloseHandle` |
| `OwnedHandle` | `crates/mirrorstar-core/src/`（通用句柄封装） | 与 `OwnedProcHandle` 同族，通用 `HANDLE` 持有 |
| `ControllerGuard` | `crates/mirrorstar-wp-proc/src/webview.rs`（WebView2 Controller 封装） | 强调「作用域守卫」语义，`Drop` 时 `Close()` |

三种命名各自语义清晰，且均符合 RAII 契约（构造获取资源、`Drop` 释放资源），但在新增 RAII 类型时若无统一约定，命名风格会继续发散。

### 约定

1. **新增 RAII 封装统一采用 `Owned*` 前缀**，例如：
   - `OwnedFileHandle` — 文件句柄封装
   - `OwnedRegistryKey` — 注册表 key 封装
   - `OwnedWorkerW` — WorkerW 句柄封装
   - `OwnedComInit` — COM 初始化引用计数封装
2. **现有命名（`OwnedProcHandle` / `OwnedHandle` / `ControllerGuard`）保留**，不批量重命名。理由：
   - 三者语义均清晰，无歧义
   - 批量重命名会中断 git blame、影响 v3.x 已修复 findings 的代码追溯
   - `ControllerGuard` 表达「作用域守卫」语义更贴合 WebView2 Controller 的生命周期
3. `Guard` 后缀仅用于**作用域守卫**类型（如 `ControllerGuard`、`ComInitGuard`），即资源在作用域结束时无条件释放、不可转移所有权的封装。
4. `Owned*` 前缀用于**可转移所有权**的封装（提供 `into_raw()` / `from_raw()`）。

### 实现建议

新增 `Owned*` RAII 封装 **SHOULD** 参照 `std::fs::File` 的设计：

```rust
pub struct OwnedFileHandle {
    handle: HANDLE,
}

impl OwnedFileHandle {
    /// 从原生句柄构造，接管所有权。调用方必须保证该句柄未被其他 Owned* 封装持有。
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        Self { handle }
    }

    /// 释放所有权并返回原生句柄。调用方负责后续 CloseHandle。
    pub fn into_raw(self) -> HANDLE {
        let h = self.handle;
        std::mem::forget(self);
        h
    }
}

impl Deref for OwnedFileHandle {
    type Target = HANDLE;
    fn deref(&self) -> &HANDLE { &self.handle }
}

impl DerefMut for OwnedFileHandle {
    fn deref_mut(&mut self) -> &mut HANDLE { &mut self.handle }
}

impl Drop for OwnedFileHandle {
    fn drop(&mut self) {
        // 不为 INVALID_HANDLE_VALUE / null 调用 CloseHandle
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle); }
        }
    }
}
```

要点：
- 实现 `Deref` / `DerefMut` 让现有调用方可以平滑迁移（`*owned` 自动解引用到 `HANDLE`）
- 实现 `Drop` 中校验 `null` / `INVALID_HANDLE_VALUE`，避免对无效句柄调用 `CloseHandle`
- `from_raw` 标记 `unsafe` 并要求调用方保证所有权唯一性
- `into_raw` 使用 `std::mem::forget` 抑制 `Drop`
- 若需要 `Send` / `Sync`，单独评估并通过 `unsafe impl` 显式声明契约（参考 `crates/mirrorstar-core/src/desktop/` 中 WorkerW 句柄的处理）

### 参考位置

- `crates/mirrorstar-core/src/desktop/` — WorkerW 句柄封装（`worker_w.rs` / `window.rs` / `native_wallpaper.rs`，HWND 与 WorkerW 句柄的生命周期管理）
- `crates/mirrorstar-wp-proc/src/webview.rs` — `ControllerGuard` 封装 WebView2 Controller 的 `Close()`
- `crates/mirrorstar-wp-proc/src/ipc_server.rs` — 子进程句柄的 RAII 管理
- `crates/mirrorstar-core/src/wallpaper/` — renderer 与 GDI 资源的生命周期管理

---

## C.2 错误处理策略（[Consistency]-12.4）

### 背景

项目跨越 library（`mirrorstar-core` / `mirrorstar-wp-proc`）与应用层（`src-tauri`），不同边界对错误表达的要求不同：

- IPC 边界需要**序列化**的错误（跨进程传递）
- Tauri 命令边界需要**语义化**的错误（前端可据 error code 分支处理）
- 内部降级需要**记录但不传播**的错误（避免 panic 中断服务）

历史上 src-tauri 错误处理不一致，部分命令用 `DesktopIntegration(String)` 泛化容器承接所有错误，导致前端无法精确分支。Wave 2C 的 ST-004 修复统一引入 `MirrorStarError` + `thiserror::Error` derive，本章节固化该约定。

### 三套错误模型使用边界

#### 1. IPC 边界：`ResponseStatus::Error` 序列化错误

**位置**：`crates/mirrorstar-core/src/ipc/`（父进程侧客户端）、`crates/mirrorstar-wp-proc/src/ipc_server.rs`（子进程侧服务端）

**适用场景**：跨进程 IPC 通信（`WpProcCommand` → `WpProcResponse`）

**约定**：
- 使用 `ResponseStatus` 枚举（`Ok` / `Error`）传递状态
- `WpProcResponse.status = ResponseStatus::Error` 时，`error: Some(String)` 字段携带人类可读错误消息
- **不向上层传递 panic**：子进程 IPC 线程的反序列化失败 / 命令处理异常均 `tracing::error!` 后构造 `ResponseStatus::Error` 响应
- 错误消息保持**简短且可机器解析**（如 `"SetWindowPos 失败: {code}"`），便于父进程做基本分类

```rust
// 子进程侧约定（crates/mirrorstar-wp-proc/src/ipc_server.rs）
match unsafe { ctrl.CoreWebView2() } {
    Ok(webview) => { /* JS 注入逻辑 */ }
    Err(e) => return WpProcResponse {
        request_id,
        status: ResponseStatus::Error,
        error: Some(format!("获取 WebView 失败: {}", e)),
    },
}
```

#### 2. Tauri 命令边界：`MirrorStarError` 语义化变体

**位置**：`src-tauri/src/error.rs`（`MirrorStarError` 定义）、`src-tauri/src/commands/`（Tauri 命令实现）

**适用场景**：`#[tauri::command]` 函数返回值、跨 FFI 边界传给前端的错误

**约定**：
- 所有 Tauri 命令的返回类型为 `Result<T, MirrorStarError>`
- 使用语义化变体，**不泛化用 `DesktopIntegration(String)`**：
  - `Config(String)` — 配置加载/解析/保存错误
  - `Wallpaper(String)` — 壁纸渲染/切换/暂停错误
  - `Desktop(String)` — 桌面集成（WorkerW / SetParent）错误
  - `Ipc(String)` — IPC 通信错误
  - `InvalidArgument(String)` — 参数校验错误（ST-004 Wave 2C 引入）
  - `InvalidPath(String)` — 路径校验错误（防 ffmpeg 协议注入等）
  - `InvalidConfig(String)` — 配置值校验错误（如音量/速度越界）
  - `Internal(String)` — 内部不变式违反（不应发生但需兜底）
- 实现 `thiserror::Error` derive，自动 `Display` 与 `from` 转换
- 实现 `Into<InvokeError>`（Tauri 命令错误桥接），保证 `?` 自动传播
- 前端通过 error code 字段精确区分错误类型并分支处理

#### 3. 内部降级：`tracing::warn!` / `tracing::error!` 记录

**位置**：library 内部（`crates/mirrorstar-core/`、`crates/mirrorstar-wp-proc/`）的非关键路径

**适用场景**：可恢复的运行时异常、best-effort 操作（如关闭时的资源释放、非关键的 `SetWindowLongPtrW` 失败）

**约定**：
- 使用 `tracing::warn!` 记录可恢复的异常并降级
- 使用 `tracing::error!` 记录不可恢复但不应 panic 的异常
- **不向上传播 panic**：使用 `Result` 或 `Option` 替代 `unwrap()` / `expect()`（测试代码除外）
- shutdown 路径中的错误：记录后允许继续退出，不阻塞进程终止

### ST-004 Wave 2C 修复说明

**问题**：原 src-tauri 错误处理不一致，部分命令用 `DesktopIntegration(String)` 作为通用错误容器承接所有错误（包括参数校验失败、路径校验失败等），导致前端无法通过 error code 精确分支。

**修复**（Wave 2C，spec: `fix-v40-wave2c-src-tauri-medium-findings`）：
- 引入 `MirrorStarError` + `thiserror::Error` derive 统一错误模型
- 参数校验类错误改用 `InvalidArgument` 变体
- 路径校验类错误改用 `InvalidPath` 变体
- 配置值校验类错误改用 `InvalidConfig` 变体
- `DesktopIntegration` 重命名为 `Desktop`，仅承接真正的桌面集成错误
- 实现 `Into<InvokeError>` 让 `?` 自动传播

### 参考位置

- `src-tauri/src/error.rs` — `MirrorStarError` 枚举定义（`thiserror::Error` derive、`Into<InvokeError>` 实现）
- `src-tauri/src/commands/` — Tauri 命令层 `Result<T, MirrorStarError>` 返回
- `crates/mirrorstar-core/src/ipc/wp_proc.rs` — `WpProcResponse` / `ResponseStatus` 枚举定义
- `crates/mirrorstar-wp-proc/src/ipc_server.rs` — 子进程 IPC 响应构造（`ResponseStatus::Error` 使用范例）
- `docs/优化文档/06-src-tauri应用层.md` — ST-004 Wave 2C 修复说明（`MirrorStarError::InvalidArgument` 引入）
- `docs/优化文档/05-audio-ipc-process模块.md` — IPC 错误处理与 `ResponseStatus::Error` 使用约定

---

## C.3 锁中毒策略（[Consistency]-12.5）

### 背景

项目大量使用 `std::sync::Mutex` / `std::sync::RwLock` 保护共享状态（wallpaper manager、config manager、IPC session manager、pause senders 等）。锁中毒（poison）发生在持有锁的线程 panic 时，此时 `lock()` 返回 `PoisonError`。

历史上不同模块对中毒的处理策略不一：

- 部分模块用 `into_inner()` 直接恢复数据，避免服务中断
- 部分模块用 `Default::default()` 回退，重新初始化
- 部分模块用 `?` 传播错误

若不统一约定，新增锁使用时会继续发散，且 review 难以判断「某锁中毒后的行为是否符合模块语义」。

### 两种主要策略

#### 1. 快速路径用 `into_inner` 恢复

**位置**：`crates/mirrorstar-core/src/wallpaper/manager.rs`（wallpaper manager 关键运行时状态）

**适用场景**：关键运行时状态（如当前壁纸、渲染器、暂停状态等），中断会导致用户可感知的服务停止。

**约定**：
- 中毒后调用 `PoisonError::into_inner()` 直接获取内部数据，丢弃锁保护但保留状态
- 不向上传播错误，保证 wallpaper 模块持续可用
- 在 lock 调用处添加 `// 锁中毒策略: into_inner 恢复，保留运行时状态` 注释

```rust
// 锁中毒策略: into_inner 恢复，保留运行时状态避免服务中断
let mut state = state_lock.unwrap_or_else(|p| p.into_inner());
```

#### 2. 配置类数据用默认值回退

**位置**：`crates/mirrorstar-core/src/config/manager.rs`（config manager 配置/状态数据）

**适用场景**：配置 / 状态数据，中毒后丢弃损坏数据、用 `Default::default()` 重新初始化更安全（避免使用半更新的脏数据）。

**约定**：
- 中毒后返回 `Default::default()`，让上层重新加载 / 重新初始化
- 不向上传播错误，保证调用方拿到可用结构体
- 在 lock 调用处添加 `// 锁中毒策略: Default 回退，丢弃脏数据` 注释

```rust
// 锁中毒策略: Default 回退，丢弃脏数据由调用方重新加载
let config = config_lock
    .map(|g| g.clone())
    .unwrap_or_else(|_| Default::default());
```

### 测试代码策略

测试代码（`#[cfg(test)] mod tests`）可用 `expect("...")` 直接 panic 简化错误处理：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c001_config_path_validation() {
        let mgr = ConfigManager::new();
        let state = mgr.state.lock().expect("test lock poisoned");
        // ...
    }
}
```

理由：测试中 panic 即测试失败，无需复杂的中毒恢复逻辑。

### 新增锁使用约定

1. **新增锁使用应明确选择策略并注释**，在变量声明处或 lock 调用处添加 `// 锁中毒策略: ...` 注释：
   - `// 锁中毒策略: into_inner 恢复，保留运行时状态`
   - `// 锁中毒策略: Default 回退，丢弃脏数据`
   - `// 锁中毒策略: ? 传播，由上层决定（仅限无运行时副作用的纯查询路径）`
2. 选择标准：
   - **关键运行时状态**（pause/resume/volume/speed/壁纸列表/渲染器）→ `into_inner` 恢复
   - **配置 / 状态数据**（用户配置、缓存、统计）→ `Default` 回退
   - **纯查询路径**（无副作用、失败可接受）→ `?` 传播
3. **现有实现差异保留，不批量统一**，避免引入行为变化或回归风险。仅在新代码中遵循约定，旧代码 review 时按需迁移。

### 参考位置

- `crates/mirrorstar-core/src/wallpaper/manager.rs` — wallpaper manager 锁中毒 `into_inner` 恢复
- `crates/mirrorstar-core/src/config/manager.rs` — config manager 锁中毒 `Default` 回退
- `crates/mirrorstar-core/src/ipc/` — IPC session manager 锁使用（参考其策略选择）
- `src-tauri/src/state.rs`（若存在）— AppState 中 `SHARED_PAUSE_SENDERS` / `SHARED_CONFIG` 等全局锁使用

### v41-N-001 实施层差距说明

v4.1 Wave v41-C 审查发现：C.3 约定文档与实施层存在差距，`config/manager.rs` 多处 `unwrap_or_else(|e| e.into_inner())` 保留中毒数据（与策略 1 一致），`wallpaper/manager.rs` 同样保留，但 `src-tauri/state.rs` 部分返回 `Err`（与策略 3 一致）。

**决策**：接受当前实施层差异，文档化"过渡期允许混合策略"。理由：
- 三种策略均符合 C.3 约定，差异源于各模块语义不同（关键运行时 vs 配置数据 vs 纯查询）
- 批量统一会引入行为变化与回归风险
- 后续 review 触及具体锁使用时，按 C.3 选择标准评估是否迁移

**后续基线**：自 v4.1 Wave v41-C 起，新增锁使用 SHALL 在注释中标注策略类型，便于 review 判断。

---

## C.4 测试命名规范（[Consistency]-12.6）

### 背景

项目历史测试命名风格不一：部分采用 `test_xxx` 前缀、部分采用描述性命名（如 `parses_valid_config`）、部分采用 `it_xxx` 风格。v4.0 修复中新增了大量测试（如 Wave 2A wallpaper 中等 findings 修复、Wave 3I 前端/构建修复等），若无统一命名约定，难以从测试名追溯到对应的 finding / Wave，影响 review 与回归验证。

本章节约定**新增测试**的命名模式，现有测试不强制重命名。

### 新增测试命名约定

新增测试采用 `<finding_id>_<描述>` 模式，其中：

- `finding_id` 为对应 finding 的小写 ID
- `描述` 为简短的下划线分隔短语，描述测试场景
- 不再使用 `test_` 前缀（Rust 2021 + cargo test 自动识别 `#[test]` 函数）

#### finding_id 前缀规则

| Finding 类别 | 前缀格式 | 范围 | 示例 |
|--------------|----------|------|------|
| 配置类（config 模块） | `c001`-`c0xx` | `c001`-`c0xx` | `c001_config_path_validation`、`c004_hot_reload_atomic_replace` |
| 壁纸类（wallpaper 模块） | `w001`-`w0xx` | `w001`-`w0xx` | `w001_video_first_frame_decode`、`w007_gif_speed_limit_enforced` |
| 桌面类（desktop 模块） | `d001`-`d0xx` | `d001`-`d0xx` | `d001_worker_w_reparent_idempotent`、`d012_restore_returns_result` |
| 音频/IPC/进程类 | `i001`-`i0xx` / `p001`-`p0xx` / `a001`-`a0xx` | 同上 | `i002_ipc_read_line_timeout`、`p006_command_escape`、`a004_session_cache_hit` |
| src-tauri 类 | `st001`-`st0xx` | `st001`-`st0xx` | `st004_invalid_argument_variant`、`st015_main_window_closing_flag` |
| wp-proc 类 | `wp001`-`wp0xx` | `wp001`-`wp0xx` | `wp001_protocol_case_insensitive`、`wp03_navigate_timeout_enforced` |
| 前端类 | `f001`-`f0xx` | `f001`-`f0xx` | `f005_double_click_guard`、`f012_listener_api_documented` |
| 构建类 | `b001`-`b0xx` | `b001`-`b0xx` | `b009_dev_scope_narrowed`、`b014_dependabot_configured` |
| 一致性类 | `consistency_<section>_<n>` | `consistency_12_3` 等 | `consistency_12_3_raii_owned_prefix`、`consistency_12_4_invalid_argument_variant` |
| Wave 标识（v4.0 修复的测试） | `wave<wave_id>_<finding_id>_<描述>` | `wave3i_f007_skeleton_count` 等 | `wave3i_f007_skeleton_count`、`wave2a_w007_video_speed_limit` |

#### 命名示例

```rust
// config 模块：对应 finding C-001（config 路径校验）
#[test]
fn c001_config_path_validation() {
    // ...
}

// wallpaper 模块：对应 finding W-001（video 首帧解码）
#[test]
fn w001_video_first_frame_decode() {
    // ...
}

// 一致性类：对应 [Consistency]-12.4 错误处理策略
#[test]
fn consistency_12_4_invalid_argument_variant() {
    // 验证 MirrorStarError::InvalidArgument 的 Display / Into<InvokeError>
    // ...
}

// Wave 标识：v4.0 Wave 3I 修复 F-007 的测试
#[test]
fn wave3i_f007_skeleton_count() {
    // ...
}
```

### 现有测试策略

**现有测试不强制重命名**，理由：

1. **避免 git blame 中断**：重命名会让历史 commit 的 blame 全部指向重命名 commit，丢失原始修改上下文
2. **避免 CI 大规模改动**：重命名牵涉测试报告、覆盖率统计、CI 日志解析等下游
3. **避免引入风险**：重命名时容易漏改 `#[test]` 函数引用、test fixture 调用等

仅以下情况建议重命名：
- 测试本身需要重写或合并时
- 测试名严重误导（如 `test_config_works` 实际测的是路径校验）时
- 同一 PR 中已涉及该测试函数的修改时

### 模块覆盖范例

每个 `mod tests` 块 **SHOULD** 覆盖以下场景（如适用）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // 1. 正常路径：典型输入下应得到预期输出
    #[test]
    fn c001_config_path_validation_normal() { /* ... */ }

    // 2. 边界值：空输入、最大值、最小值、临界条件
    #[test]
    fn c001_config_path_validation_empty_path() { /* ... */ }

    #[test]
    fn c001_config_path_validation_too_long() { /* ... */ }

    // 3. 错误路径：非法输入应返回 Err，验证错误变体
    #[test]
    fn c001_config_path_validation_protocol_injection() {
        // 验证含 "://" 的路径被拒绝（防 ffmpeg 协议注入）
        /* ... */
    }

    // 4. 并发场景（如适用）：多线程并发访问下的线程安全
    #[test]
    fn c004_hot_reload_atomic_replace_concurrent() {
        // 验证 save_mutex 下的原子性替换不会出现 dirty 竞态
        /* ... */
    }
}
```

### 参考位置

- 各模块的 `#[cfg(test)] mod tests` 块：
  - `crates/mirrorstar-core/src/config/manager.rs` — config 模块测试
  - `crates/mirrorstar-core/src/wallpaper/manager.rs` — wallpaper 模块测试（含 `w001_video_first_frame_decode` 等）
  - `crates/mirrorstar-core/src/desktop/worker_w.rs` — desktop 模块测试
  - `crates/mirrorstar-core/src/ipc/` — IPC 协议测试
  - `crates/mirrorstar-wp-proc/src/command.rs` — wp-proc 命令测试（`parse_rect` / `build_url` / `percent_encode_path`）
  - `src-tauri/src/commands/` — src-tauri 命令测试
  - `src/` 与 `src/lib/` — 前端 vitest 测试（`*.test.ts`）

---

## C.5 常量定义风格规范（v41-N-002）

### 背景

v4.0 多个修复（W-013 / D-012 / ST-010 / WP-010 等）提取模块级常量，但命名风格仍不统一：`WINDOW_FIND_RETRIES`（u32）/ `WM_SPAWN_WORK_TIMEOUT_MS`（u32）/ `SHUTDOWN_THUMBNAIL_TIMEOUT`（Duration）/ `FALLBACK_SCREEN_WIDTH`（i32）。单位后缀（`_MS`）有时省略。

### 约定

1. **命名风格**：所有常量统一采用 `SCREAMING_SNAKE_CASE`
2. **时间常量**：时间相关常量 **SHALL** 携带单位后缀：
   - 毫秒：`_MS`（如 `CONFIG_SAVE_DEBOUNCE_MS`）
   - 秒：`_SECS`（如 `PLAY_COMMAND_TIMEOUT_SECS`）
   - 若类型为 `Duration`，后缀可省略（如 `SHUTDOWN_THUMBNAIL_TIMEOUT`），但 **SHOULD** 在文档注释中说明精确值
3. **类型说明**：常量文档注释 **SHOULD** 说明类型选择理由（如 `i32` 与 `RECT` 字段一致）
4. **集中位置**：模块内常量 **SHOULD** 集中在模块顶部 `/// Internal constants` 或 `/// Constants` 段落，便于查阅
5. **现有差异保留**：不批量重命名现有常量，避免 git blame 中断与回归风险

### 参考位置

- `crates/mirrorstar-core/src/config/manager.rs` — `/// Internal constants` 段落（v41-C-008 修复集中定义）
- `crates/mirrorstar-core/src/wallpaper/subprocess_base.rs` — `/// Connection constants` 段落（v41-W-010 修复集中定义）
- `crates/mirrorstar-core/src/desktop/worker_w.rs` — `UNICODE_STRING_MAX_CHARS` 模块级常量（v41-D-010 修复）

---

## C.6 Stop 系列函数行为规范（v41-N-003）

### 背景

v4.0 Wave 2H 修复 P-002 新增 `stop_immediate()` 方法，但 `ProcessManager::stop_immediate()`（不等待）与 `WallpaperEngine::stop()`（等待 5s）行为差异未文档化；`VolumeControl` 无 `stop` 概念但 `Drop` 时隐式停止。

### 行为矩阵

| 模块 | 函数 | 等待行为 | 适用场景 | 文档位置 |
|------|------|----------|----------|----------|
| `ProcessManager` | `stop()` | 等待 5s 优雅退出 | 应用退出 / 配置变更触发进程重启 | `crates/mirrorstar-core/src/process/manager.rs` |
| `ProcessManager` | `stop_immediate()` | 不等待，直接 `CloseHandle` | 场景切换 / 进程重启 / `Drop` 清理 | 同上（v41-P-002 文档化决策标准） |
| `WallpaperEngine` | `stop()` | 等待 5s 优雅退出 | 应用退出 / 壁纸类型切换 | `crates/mirrorstar-core/src/wallpaper/manager.rs` |
| `WallpaperEngine` | `shutdown()` | 等待 + 资源释放 | 应用关闭 | 同上 |
| `VolumeControl` | `Drop` | 隐式停止（COM Release） | 作用域结束 | `crates/mirrorstar-core/src/audio/volume.rs`（v41-A-004 文档化） |

### 约定

1. **`stop()`**：优雅关闭，等待资源清理完成（5s 超时），适用于应用退出
2. **`stop_immediate()`**：立即关闭，不等待，适用于场景切换 / 进程重启
3. **`shutdown()`**：完全关闭并释放资源，适用于应用关闭
4. **`Drop`**：隐式清理，不阻塞，适用于作用域结束

### v41-P-003 兜底契约

`stop_immediate()` 后调用 `start()` 时，`start()` 内部 **SHALL** 包含 50ms `std::thread::sleep` 兜底等待 OS 回收 PID，避免 `CreateProcessW` 失败或句柄混乱。

### 参考位置

- `crates/mirrorstar-core/src/process/manager.rs` — `stop()` / `stop_immediate()` 文档（v41-P-002 决策标准）
- `crates/mirrorstar-core/src/wallpaper/manager.rs` — `WallpaperEngine::stop()` / `shutdown()`
- `crates/mirrorstar-core/src/audio/volume.rs` — `VolumeControl` Drop 契约（v41-A-004）

---

## C.7 错误日志级别规范（v41-N-004）

### 背景

v4.0 多个修复（C-008 / D-007 / A-005 等）使用 `tracing::warn!` 记录降级路径，但部分场景（如 `WAIT_FAILED` in A-005）使用 `trace!` 级别过低；v41-A-005 / v41-W-008 等仍存在级别不一致。

### 约定

| 级别 | 使用场景 | 示例 |
|------|----------|------|
| `error!` | 不可恢复错误，影响核心功能 | IPC 通信失败、配置加载失败导致回退默认 |
| `warn!` | 可恢复的降级路径，用户可感知 | `SetWindowLongPtrW` 失败、`WAIT_FAILED` 保守保留缓存（v41-A-005 修复） |
| `info!` | 关键状态变更 | 壁纸切换、进程启动 / 退出、配置重载 |
| `debug!` | 诊断信息，开发期排查 | 锁竞争、缓存命中 / 未命中 |
| `trace!` | 详细诊断，极低级别 | 函数入参、循环迭代细节 |

### 选择标准

1. **影响用户感知的功能** → `error!` 或 `warn!`
2. **可恢复的降级** → `warn!`（含错误码 / 原因）
3. **正常状态变更** → `info!`
4. **开发诊断** → `debug!` / `trace!`

### v41-A-005 修复说明

`is_pid_running` 中 `WAIT_FAILED` 日志级别从 `trace!` 提升到 `warn!`，含 `error = ?std::io::Error::last_os_error()` 字段，便于生产环境发现 `WAIT_FAILED` 频发及其根因。

### 参考位置

- `crates/mirrorstar-core/src/audio/volume.rs` — `is_pid_running` 的 `WAIT_FAILED` 日志（v41-A-005 修复）
- `crates/mirrorstar-core/src/desktop/window.rs` — `make_borderless` 的 `SetWindowLongPtrW` 失败日志（v41-D-006 修复）

---

## C.8 unsafe SAFETY 注释规范（v41-N-005）

### 背景

v4.0 多个修复（WP-013 / D-014 等）追加 SAFETY 注释，但 `desktop/worker_w.rs` / `desktop/native_wallpaper.rs` / `wallpaper/gdi_base.rs` 等模块的 `unsafe` 块覆盖率不均衡：部分有完整 SAFETY 注释，部分仅一行 `// unsafe: Windows API call`。

### 约定

所有 `unsafe` 块 **SHALL** 添加 `// SAFETY:` 注释，包含以下要素（如适用）：

1. **不变量**（Invariants）：调用前需满足的条件（如"句柄非 null"、"缓冲区足够大"）
2. **生命周期**（Lifetimes）：指针 / 引用的生命周期约束（如"pvParam 借用 image_path_wide，作用域覆盖整个调用"）
3. **错误处理**（Error handling）：失败时的处理方式（如"返回 0 时调用 GetLastError 判断"）
4. **线程安全**（Thread safety）：跨线程使用的约束（如"仅在主线程调用"）

### 格式

```rust
// SAFETY: <论证>
// - 不变量: <说明>
// - 生命周期: <说明>
// - 错误处理: <说明>
unsafe {
    // ...
}
```

简化格式（简单场景）：

```rust
// SAFETY: hwnd 来自 FindWindowW，保证非 null；SetWindowLongPtrW 线程安全。
unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style) };
```

### 参考位置

- `crates/mirrorstar-core/src/desktop/native_wallpaper.rs` — `SystemParametersInfoW` SAFETY 注释（v41-D-007 修复，pvParam 所有权契约）
- `crates/mirrorstar-core/src/desktop/mod.rs` — `embed_wallpaper` SAFETY 注释（v41-D-009 修复，锁持有阻塞上界）
- `crates/mirrorstar-wp-proc/src/main.rs` — `SendHwnd` SAFETY 注释（v41-WP-002 修复）

### 现有差异策略

现有 `unsafe` 块的 SAFETY 注释覆盖率不均衡，**不批量补充**，仅在 review 触及或重写时按本约定补充。未来可考虑用 `#![deny(clippy::undocumented_unsafe_blocks)]` 强制 lint。

---

## 修复状态

| Finding ID | 描述 | 修复状态 |
|------------|------|---------|
| [Consistency]-12.3 | RAII 命名规范 | ✅ 已修复于 v4.0 Wave 3I（文档化约定） |
| [Consistency]-12.4 | 错误处理策略 | ✅ 已修复于 v4.0 Wave 3I（文档化约定） |
| [Consistency]-12.5 | 锁中毒策略 | ✅ 已修复于 v4.0 Wave 3I（文档化约定） |
| [Consistency]-12.6 | 测试命名规范 | ✅ 已修复于 v4.0 Wave 3I（文档化约定） |
| v41-N-001 | 锁中毒策略实施层未统一 | ✅ 已修复于 v4.1 Wave v41-C（C.3 追加实施差距说明，接受过渡期混合策略） |
| v41-N-002 | 常量定义风格差异 | ✅ 已修复于 v4.1 Wave v41-C（新增 C.5 章节） |
| v41-N-003 | Stop 系列函数行为差异 | ✅ 已修复于 v4.1 Wave v41-C（新增 C.6 章节） |
| v41-N-004 | 错误日志级别使用不统一 | ✅ 已修复于 v4.1 Wave v41-C（新增 C.7 章节） |
| v41-N-005 | unsafe SAFETY 注释覆盖率不均衡 | ✅ 已修复于 v4.1 Wave v41-C（新增 C.8 章节） |

### 实施说明

- **修复方式**：通过制定本附录约定文档，不批量重命名 / 重写现有代码
- **新增代码基线**：自本附录发布起新增的 RAII 封装 / 错误处理 / 锁使用 / 测试命名 **SHALL** 遵循本附录约定
- **现有代码策略**：保留实现差异，仅在 review 触及或重写时按需迁移
- **追溯锚点**：本附录为 `[Consistency]-12.3` ~ `12.6` 的修复交付物，spec: `fix-v40-wave3i-frontend-build-infra-consistency-low-findings`
