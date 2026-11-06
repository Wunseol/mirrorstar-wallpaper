[← 返回文档索引](../index.md) > 架构与技术栈 > 错误处理

# MirrorStar Wallpaper（镜星壁纸）— 错误处理策略

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.0                      |
| 更新日期 | 2026-10-11                |
| 文档状态 | 已实现（已按真实代码合并重组，2026-10）        |

***

## 1. 错误库选择

### 选择：thiserror 2（库级）

workspace 使用 `thiserror = "2"`，**未引入 anyhow**（早期文档中的 anyhow 为虚构依赖）。

- **`mirrorstar-core`**：定义具体错误类型 `MirrorStarError`（`#[derive(thiserror::Error)]`），覆盖桌面集成、WorkerW 查找、子进程启动、IPC、音频控制、配置文件解析/写入、图片解码、文件监视、Win32、IO 等类别；`pub type Result<T> = std::result::Result<T, MirrorStarError>;`
- **应用层（`src-tauri` / `mirrorstar-ui` / `mirrorstar-wp-proc`）**：Tauri 命令边界将 `MirrorStarError` 序列化为 `{ code, message }` 结构返回前端，保持前端可判定的稳定错误码。

***

## 2. Rust Result-based 错误传播

MirrorStar 使用 Rust 的 `Result<T, E>` 类型系统进行错误处理，避免异常和 panic。

### 2.1 错误类型层次（实测自 `mirrorstar-core/src/lib.rs`）

```rust
/// MirrorStar 全局错误类型
#[derive(Debug, thiserror::Error)]
pub enum MirrorStarError {
    // ── 桌面集成 / 进程 / IPC ──
    #[error("桌面集成失败: {0}")]
    DesktopIntegration(String),
    #[error("未找到 WorkerW 窗口")]
    WorkerWNotFound,
    #[error("子进程启动失败: {0}")]
    ProcessSpawnFailed(String),
    #[error("IPC 通信失败: {0}")]
    IpcError(String),

    // ── 音频控制 ──
    #[error("音频控制失败: {0}")]
    AudioControl(String),
    #[error("未找到进程 {pid} 的音频会话")]
    AudioSessionNotFound { pid: u32 },

    // ── 配置 ──
    #[error("配置文件解析失败: {0}")]
    ConfigParse(String),
    #[error("配置文件写入失败: {0}")]
    ConfigWrite(String),

    // ── 图片 / 文件监视 ──
    #[error("图片解码失败: {0}")]
    ImageDecode(String),
    #[error("文件监视器失败: {0}")]
    FileWatcher(String),

    // ── 并发原语 ──
    #[error("锁中毒: {0}")]
    LockPoisoned(String),
    #[error("任务 join 失败: {0}")]
    TaskJoin(String),

    // ── IPC 细分 ──
    #[error("IPC 超时: {0}")]
    IpcTimeout(String),
    #[error("IPC 连接断开: {0}")]
    IpcDisconnected(String),
    #[error("IPC 未连接: {0}")]
    IpcNotConnected(String),

    // ── 底层 From 转换 ──
    #[error("Win32 错误: {0}")]
    Win32(#[from] windows::core::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    // ── 安全校验（SEC-001/002/004）与参数校验（N-009） ──
    #[error("无效的壁纸文件路径: {reason}")]
    InvalidPath { reason: String },
    #[error("无效的配置字段: {reason}")]
    InvalidConfig { reason: String },
    #[error("无效的 URL 协议: {scheme}")]
    InvalidUrl { scheme: String },
    #[error("无效参数: {reason}")]
    InvalidArgument { reason: String },

    // ── WebView2 ──
    #[error("WebView2 操作超时: {0}")]
    WebView2Timeout(String),
}

pub type Result<T> = std::result::Result<T, MirrorStarError>;
```

`MirrorStarError` 实现了 `serde::Serialize`，序列化输出 `{ "code": "<枚举变体名>", "message": "<Display 文本>" }`，供 Tauri 命令边界透传给前端。同时实现了若干 `From` 转换（`serde_json::Error` / `toml::de::Error` / `toml::ser::Error` → `ConfigParse` / `ConfigWrite`，`image::ImageError` → `ImageDecode`，`notify::Error` → `FileWatcher`）。

### 2.2 错误处理原则

| 原则          | 说明                 | 示例                               |
| ----------- | ------------------ | -------------------------------- |
| **不 panic** | 所有可恢复错误使用 `Result` | 文件不存在返回 `Err`，不 `unwrap`         |
| **尽早返回**    | 使用 `?` 运算符传播错误     | `let hwnd = find_window()?;`     |
| **上下文丰富**   | 错误消息包含足够诊断信息       | `"未找到进程 1234 的音频会话"`    |
| **降级处理**    | 非关键错误不阻断主流程        | 配置热重载失败保留旧配置                     |
| **日志记录**    | 错误发生时记录完整上下文       | `tracing::error!(pid, "子进程崩溃");` |

***

## 3. 进程崩溃恢复

### 3.1 子进程退出监听

MirrorStar **不实现**子进程/渲染线程的自动重启（respawn）。当前只有单一的退出监听器：

- 实现位置：`crates/mirrorstar-core/src/wallpaper/mod.rs` 中的 `spawn_proc_exit_monitor`（L155-186）。
- 工作方式：通过 `WaitForSingleObject` 等待子进程句柄进入退出状态，获取退出码。

### 3.2 退出处理流程

```mermaid
flowchart TD
    A[spawn_proc_exit_monitor 等待子进程退出] --> B[调用 WaitForSingleObject<br/>等待进程句柄进入退出状态]
    B --> C{进程是否按预期终止?<br/>exit_code == Terminated 状态目标值?}
    C -->|正常退出| D[渲染器状态标记为 Terminated]
    C -->|非预期退出 state != Terminated| E[更新渲染器状态为 Terminated]
    E --> F[通过 PauseSender::notify_state_changed 通知引擎]
    F --> G[引擎触发 wallpaper-state-changed 事件<br/>告知前端（web.rs / video.rs）]
    D --> H[结束监听]
    G --> H
```

上图的判断要点：

- `spawn_proc_exit_monitor`（wallpaper/mod.rs L155-186）是**唯一**的子进程退出监听器，无重试循环；**无独立看门狗进程**，但存在进程内 `liveness_watchdog`（5s 采样 / 1.3s 确认 / 60s 冷却）做视频停滞自愈。子进程异常退出**不自动 respawn**。
- 当子进程**异常退出**（即以非 `Terminated` 状态退出）时，将渲染器状态更新为 `Terminated`，并通过 `PauseSender::notify_state_changed` 通知引擎。
- 引擎据此触发 `wallpaper-state-changed` 事件，通知前端（web.rs / video.rs）刷新状态。

### 3.3 恢复策略现状

> **重要说明**：自动重启（respawn）**未实现**，处于观察中。不存在"重新 spawn 子进程 3 次""重启 GIF 渲染线程 3 次""连续崩溃(3 次/分钟)放弃恢复并切换静态壁纸"等自动重启/重试流程。全屏 `terminate` 后的子进程由 `resume_all_fast` / 后台复查线程重建，不属于异常崩溃 respawn。

| 场景        | 现状                          | 后续方向      |
| ---------- | --------------------------- | --------- |
| 子进程异常退出  | 仅监听并更新状态为 Terminated，通知前端；**不自动 respawn**    | 自动重启（respawn）待评估，处于观察中 |
| 主进程崩溃    | 操作系统自动回收子进程（父子进程关系），**无独立看门狗进程**；进程内 `liveness_watchdog` 仅做视频停滞自愈（5s 采样 / 1.3s 确认 / 60s 冷却）；Web 子进程崩溃可通过 IPC 管道断开检测 | - |

***

## 4. 优雅降级

| 故障场景               | 降级方案                              |
| ------------------ | --------------------------------- |
| WorkerW 未找到        | Native 模式下不影响静态图片壁纸；WorkerW 模式下嵌入失败并返回 `DesktopIntegration`/`WorkerWNotFound` 错误（不自动降级为普通底层窗口） |
| WebView2 不可用       | 网页壁纸/预览不可用，程序无内置引导安装，需用户自行安装 WebView2 Runtime |
| mpv.exe 缺失/启动失败   | 视频壁纸不可用，提示用户检查 mpv.exe（随程序分发或 PATH 查找）         |
| 配置文件损坏             | 使用默认配置启动，记录警告日志                   |
| 音频控制失败             | 壁纸正常播放，仅音频控制不可用                   |
| SetWinEventHook 失败 | **系统事件钩子**（返回 0）：记录 `error` 并退出全屏监控线程，不再检测新的进入/退出；**对象事件钩子**（返回 0）：记录 `warn` 并继续——前台切换事件仍可用，500ms 轻量探针 + 每 2s 全量兜底同时覆盖进入与退出 |
| Native 壁纸 API 失败  | 返回错误，**不回退** WorkerW 嵌入模式，记录错误日志           |

> **SetWinEventHook 降级说明**：全屏检测注册**两个** `WINEVENT_OUTOFCONTEXT` 钩子——系统事件（`EVENT_SYSTEM_FOREGROUND..EVENT_SYSTEM_MINIMIZEEND`）与对象事件（`EVENT_OBJECT_DESTROY..EVENT_OBJECT_UNCLOAKED`，附加 `WINEVENT_SKIPOWNPROCESS`）。**对象事件用于发现"独占全屏在同一个前台窗口内部切换"**：D3D `ChangeDisplaySettings` / borderless→exclusive 不产生前台事件，可依靠对象事件中的 `LOCATIONCHANGE` / `STATECHANGE` 识别，并由 500ms QUNS 轻量探针兜底。
>
> - **系统事件钩子失败（返回 0）**：`fullscreen.rs` 记录 `error` 并停止全屏监控线程（置 `FULLSCREEN_MONITOR_RUNNING=false`），不再检测新的全屏进入/退出；已启动的 500ms 探针线程仍会运行。
> - **对象事件钩子失败（返回 0）**：记录 `warn` 并继续运行；前台切换（`EVENT_SYSTEM_FOREGROUND`）与最小化/还原事件仍可用，独立线程 `mirrorstar-fullscreen-probe` 的 500ms 轻量探针在快照变化时触发调和，并每 2s 执行一次全量兜底，**同时覆盖漏检的进入与退出**（不再是"仅退出"），但独占全屏的即时性会下降。
> - **`SHQueryUserNotificationState` 调用失败**：`fullscreen.rs` 仅 `debug` 记录（"SHQueryUserNotificationState 调用失败，回退矩形判定"）并按"无全屏证据"处理，不影响事件钩子与暂停/恢复主流程。级别解析为**会话优先**模型，QUNS 只负责触发 / 维持 / 确认证据，因此调用失败时仍可由「可信且 100% 覆盖的前台窗口 + 已有会话」正常解析；**不会因 QUNS 失败创建错误的全屏会话**（覆盖层不会被当成全屏身份）。
> - **`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` 失败或返回空矩形**：回退 `GetWindowRect`；两者都失败时本次级别判定返回 `None`。
> - **`DwmGetWindowAttribute(DWMWA_CLOAKED)` 失败**：按"未 Cloak"处理，避免因查询失败而误判为非全屏并提前恢复壁纸。
> - 以上三者均为只读查询，失败只影响单次全屏级别判定的精度，**不影响壁纸显示与暂停/恢复主流程**。

***

**相关文档：**

- [01-架构与技术栈总览](./01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [02-模块设计](./02-模块设计-Module-Design.md)
- [03-进程架构与IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)
- [06-暂停恢复机制](./06-暂停恢复机制-Pause-Resume.md)
- [08-依赖与数据流](./08-依赖与数据流-Dependency-and-Data-Flow.md)
- [09-性能优化](./09-性能优化-Performance.md)