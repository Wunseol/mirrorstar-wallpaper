# MirrorStar Wallpaper（镜星壁纸）技术栈 — 基础设施

[← 返回文档索引](../README.md) > [技术栈](./overview.md) > 基础设施

## 8. 配置管理

### 选择：TOML + serde + notify

**选择理由：**

- **TOML 格式**：比 JSON 更适合人工编辑，支持注释，Rust 生态标准配置格式（Cargo.toml）
- **serde**：Rust 序列化标准，零拷贝反序列化，derive 宏自动实现
- **notify**：跨平台文件系统监控，支持热重载

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **JSON** (Lively 的选择) | 通用，解析快 | 不支持注释，人工编辑不友好 | ❌ 不适合配置 |
| **YAML** | 支持注释，可读性好 | 解析复杂，缩进敏感，Rust 解析器较慢 | ⚠️ 可选 |
| **RON** | Rust 原生，支持注释 | 生态小，非标准 | ❌ 生态不足 |
| **TOML** ✅ | 支持注释，可读性好，Rust 标准 | 嵌套结构不如 YAML 灵活 | ✅ 最佳选择 |

### 配置文件结构设计

```toml
# config.toml

[general]
# 开机自启
auto_start = false
# 关闭窗口时最小化到托盘
minimize_to_tray = true

[audio]
# 全局音量 (0.0 ~ 1.0)
volume = 0.8
# 是否静音
muted = false

[pause]
# 全屏应用时自动暂停
pause_on_fullscreen = true
# 电池供电时暂停
pause_on_battery = false

[display]
# 壁纸排列方式: "per_monitor" | "span"
arrangement = "per_monitor"

[video]
# 硬件解码
hwdec = true
# 播放速度
speed = 1.0

[web]
# WebView2 缓存路径
cache_path = ""
```

### 热重载机制

```
notify (文件监控) → 文件变更事件 → 重新读取 TOML → serde 反序列化 → 应用新配置
```

- 使用 `notify::RecommendedWatcher` 监控配置文件变更
- 防抖处理：变更后延迟 500ms 再重新加载，避免频繁重载
- 配置验证：反序列化后校验字段合法性，非法值保留旧配置

---

## 9. 日志系统

### 选择：tracing + tracing-subscriber + tracing-appender

**选择理由：**

- **结构化日志**：支持 span（上下文追踪）、event（事件记录），比纯文本日志更强大
- **异步兼容**：与 tokio 无缝集成，不阻塞异步任务
- **Rust 生态标准**：几乎所有主流异步库都使用 tracing，统一日志体系
- **灵活输出**：支持同时输出到控制台、文件、自定义目标
- **日志轮转**：tracing-appender 支持按日期/大小自动轮转

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **log** crate | 简单， facade 模式 | 无 span，无结构化，功能太弱 | ❌ 功能不足 |
| **NLog** (Lively) | C# 生态成熟 | C# 专属，Rust 不可用 | ❌ 不可用 |
| **slog** | 结构化，功能强 | API 复杂，社区向 tracing 迁移 | ❌ 社区迁移 |
| **tracing** ✅ | 结构化，异步兼容，生态标准 | 学习曲线略高 | ✅ 最佳选择 |

### 日志配置

```rust
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

// 日志目录：使用系统本地数据目录的绝对路径
// （%LOCALAPPDATA%\mirrorstar\logs），避免依赖工作目录
let log_dir = dirs::data_local_dir()
    .unwrap_or_else(|| std::path::PathBuf::from("."))
    .join("mirrorstar")
    .join("logs");

std::fs::create_dir_all(&log_dir)?;

// 控制台 + 文件双输出，按日期轮转
let file_appender = tracing_appender::rolling::daily(log_dir, "mirrorstar.log");
let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

// 优先读取 RUST_LOG 环境变量，失败时回退到 "info" 级别
let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

tracing_subscriber::fmt()
    .with_env_filter(env_filter)
    .with_writer(std::io::stdout.and(non_blocking))
    .with_ansi(true)
    .init();

// guard 必须由调用方持有至程序退出，提前 drop 会丢失后续日志
```

---

## 10. 系统托盘

### 选择：Tauri 2 内置 tray-icon feature flag

**选择理由：**

- **Tauri 生态**：直接复用 Tauri 2 内置的 `tray-icon` feature flag，无需引入独立 crate，与 Tauri 事件循环无缝集成
- **跨平台**：Tauri tray-icon 支持底层 Windows Shell_NotifyIcon（虽然本项目仅面向 Windows）
- **功能完整**：图标、提示文字、右键菜单、点击事件
- **轻量**：仅依赖 Windows Shell_NotifyIcon API

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **NotifyIcon** (Lively/WinForms) | .NET 原生 | C# 专属 | ❌ 不可用 |
| **systray** crate | 纯 Rust | 维护不活跃，功能有限 | ❌ 维护不足 |
| **Tauri 2 tray-icon feature** ✅ | Tauri 生态，功能完整，活跃维护 | - | ✅ 最佳选择 |

### 托盘菜单设计

```
镜星壁纸
├── 📋 打开主窗口
├── ▶ 恢复播放 / ⏸ 暂停壁纸
└── ❌ 退出
```

> **注意**：当前托盘菜单仅含 3 项（打开、暂停/恢复壁纸、退出）。更丰富的托盘菜单（音量子菜单、显示器子菜单等）为未来改进项，届时可在此菜单中扩展。

---

## 11. 进程间通信

### 选择：Windows 命名管道（同步 Win32 API）

**选择理由：**

- **Windows 原生**：命名管道是 Windows 上最可靠的 IPC 机制
- **双向通信**：支持全双工，主进程与子进程可同时读写
- **可靠有序**：字节流模式保证消息顺序和完整性
- **无外部依赖**：Windows 内核原生支持，无需额外安装
- **同步实现**：使用 `CreateNamedPipeW` / `ConnectNamedPipe`（基于 `std::fs`，非 tokio 异步），简单可靠，避免异步运行时与 COM/窗口消息循环的交互复杂性

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **stdin/stdout** (Lively 的选择) | 简单 | 单向，无法同时读写，缓冲问题 | ❌ 功能受限 |
| **共享内存** | 高吞吐 | 同步复杂，需额外锁机制 | ❌ 过于复杂 |
| **Socket** | 跨平台 | 端口冲突风险，防火墙问题 | ❌ 不适合本地 IPC |
| **tokio named_pipe** | 异步读写 | 与 COM STA、窗口消息循环交互复杂 | ❌ 不适用 |
| **Named Pipes（同步 Win32）** ✅ | 双向，可靠，Windows 原生，实现简单 | Windows 专属（本项目可接受） | ✅ 最佳选择 |

### 两套独立 IPC 协议

> **重要**：MirrorStar 使用**两套独立的 IPC 协议**，分别对应视频壁纸与网页壁纸，而非统一协议。两者均基于 Windows 命名管道，但协议格式、管道路径与命令集完全不同。

#### 1. mpv 原生 IPC（视频壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `MpvIpcClient`（246 行） |
| 管道路径 | `\\.\pipe\mirrorstar-mpv-{uuid}`（由 mpv `--input-ipc-server` 参数创建） |
| 协议格式 | mpv 原生 JSON 协议（mpv 内置协议，非自定义） |
| 通信方式 | 主进程作为客户端连接 mpv 创建的管道服务端 |
| 命令集 | `pause` / `resume` / `set_volume` / `set_loop_file` / `set_speed` / `quit` |

mpv 原生 IPC 示例（主进程 → mpv）：

```jsonc
{"command": ["set_property", "pause", true]}   // 暂停
{"command": ["set_property", "pause", false]}  // 恢复
{"command": ["set_property", "loop-file", true]}  // 循环
{"command": ["quit"]}
```

> **注意**：虽然 `MpvIpcClient` 提供 `set_volume` 命令，但视频壁纸的实际音量控制通过 WASAPI（`VolumeControl`）实现，不使用 mpv IPC 音量命令（详见 [壁纸渲染 — 视频播放](./wallpaper-rendering.md#音量控制wasapi非-mpv-ipc)）。

#### 2. wp-proc IPC（网页壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `WpProcIpcClient`（385 行） |
| 管道路径 | 自定义命名管道（由 wp-proc 子进程创建） |
| 协议格式 | `WpProcCommand` JSON + 换行符分隔（自定义协议） |
| 通信方式 | 主进程作为客户端连接 wp-proc 子进程创建的管道服务端 |
| 命令集 | `Play` / `Terminate` / `SetPosition` / `Navigate` / `Pause` / `Resume` |

`WpProcCommand` 协议示例（主进程 → wp-proc 子进程）：

```jsonc
{"command":"play","request_id":1,"source":"https://example.com/wallpaper.html"}
{"command":"terminate","request_id":2}
{"command":"set_position","request_id":3,"x":0,"y":0,"width":1920,"height":1080}
{"command":"navigate","request_id":4,"url":"https://example.com/"}
{"command":"pause","request_id":5}
{"command":"resume","request_id":6}
```

### 实现说明

- 两套 IPC 均使用同步 `CreateNamedPipeW` / `ConnectNamedPipe`（`std::fs`），**不使用** `tokio::net::windows::named_pipe` 异步 API
- 管道服务端由子进程（mpv.exe / mirrorstar-wp-proc.exe）创建，主进程作为客户端连接
- 消息以 JSON 文本传输，每条消息以换行符分隔
- 连接重试：主进程在子进程启动后重试连接管道（100 × 200ms）

> **注意**：完整 IPC 协议定义（含所有命令和消息类型）参见 [进程架构 — IPC 通信设计](../02-架构设计/process-architecture.md)

---

## 12. 异步运行时

### 选择：tokio（多线程运行时）

**选择理由：**

- **Rust 异步标准**：tokio 是 Rust 异步生态的事实标准，几乎所有异步库都基于 tokio
- **多线程调度**：work-stealing 调度器，充分利用多核 CPU
- **功能丰富**：内置 TCP/UDP/Unix Socket/Named Pipe/文件 IO/定时器/信号处理
- **高性能**：epoll/IOCP 后端，零拷贝 IO，高效的定时器轮
- **活跃维护**：每 6 周发布一个新版本，社区活跃

### 使用场景

| 场景 | 说明 |
|------|------|
| 文件监控 | 配置文件热重载 |
| 进程监控 | SetWinEventHook 回调异步处理 |
| UI 通信 | Tauri 前后端异步消息 |
| 定时任务 | 壁纸切换调度、状态轮询 |

> **注意**：IPC 通信（命名管道）使用同步 Win32 API（`CreateNamedPipeW` / `ConnectNamedPipe`），**不**经过 tokio 异步运行时（详见 [进程间通信](#11-进程间通信)）。

### 配置

```rust
#[tokio::main]
async fn main() {
    // 多线程运行时，自动检测 CPU 核心数
}
```

---

## 13. 序列化

### 选择：serde + serde_json + toml

**选择理由：**

- **serde**：Rust 序列化事实标准，零拷贝反序列化，derive 宏自动实现
- **serde_json**：JSON 序列化，用于 IPC 消息和数据存储
- **toml**：TOML 序列化，用于配置文件

### 核心特性

- **derive 宏**：`#[derive(Serialize, Deserialize)]` 自动实现，无需手写
- **零拷贝**：`&'a str` 直接引用输入数据，无需分配
- **泛型支持**：`Vec<T>`, `HashMap<K, V>`, 嵌套结构体等自动处理
- **默认值**：`#[serde(default)]` 支持字段默认值

---

## 14. 错误处理

### 选择：thiserror（库级） + anyhow（应用级）

**选择理由：**

- **thiserror**：为库定义具体错误类型，提供 `#[derive(Error)]` 宏，自动实现 `std::error::Error`
- **anyhow**：为应用提供灵活错误处理，`?` 操作符自动转换，附带上下文信息
- **Rust 生态标准**：几乎所有 Rust 项目都采用此组合

### 使用原则

```rust
// 库级错误：定义具体错误类型
#[derive(Debug, thiserror::Error)]
pub enum MirrorStarError {
    /// 桌面集成错误
    #[error("桌面集成失败: {0}")]
    DesktopIntegration(String),

    /// 未找到 WorkerW 窗口
    #[error("未找到 WorkerW 窗口")]
    WorkerWNotFound,

    /// 子进程启动失败
    #[error("子进程启动失败: {0}")]
    ProcessSpawnFailed(String),

    /// IPC 通信失败
    #[error("IPC 通信失败: {0}")]
    IpcError(String),

    /// 音频控制错误
    #[error("音频控制失败: {0}")]
    AudioControl(String),

    /// 配置文件解析错误
    #[error("配置文件解析失败: {0}")]
    ConfigParse(String),

    /// 配置文件写入错误
    #[error("配置文件写入失败: {0}")]
    ConfigWrite(String),

    /// 图片解码失败
    #[error("图片解码失败: {0}")]
    ImageDecode(String),

    /// 文件监视器失败
    #[error("文件监视器失败: {0}")]
    FileWatcher(String),

    /// 锁中毒
    #[error("锁中毒: {0}")]
    LockPoisoned(String),

    /// 任务 join 失败
    #[error("任务 join 失败: {0}")]
    TaskJoin(String),

    /// IPC 超时
    #[error("IPC 超时: {0}")]
    IpcTimeout(String),

    /// IPC 连接断开
    #[error("IPC 连接断开: {0}")]
    IpcDisconnected(String),

    /// IPC 未连接
    #[error("IPC 未连接: {0}")]
    IpcNotConnected(String),

    /// Windows API 错误
    #[error("Win32 错误: {0}")]
    Win32(#[from] windows::core::Error),

    /// IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    // SEC-001: 路径校验失败（含路径遍历拒绝）
    #[error("无效的壁纸文件路径: {reason}")]
    InvalidPath { reason: String },

    // SEC-002: 配置字段范围校验失败
    #[error("无效的配置字段: {reason}")]
    InvalidConfig { reason: String },

    // SEC-004: URL 协议不在白名单
    #[error("无效的 URL 协议: {scheme}")]
    InvalidUrl { scheme: String },
}

pub type Result<T> = std::result::Result<T, MirrorStarError>;

// 应用级错误：使用 anyhow
fn main() -> anyhow::Result<()> {
    let config = load_config().context("Failed to load configuration")?;
    Ok(())
}
```

---

**相关章节：** [← 总览](./overview.md) | [壁纸渲染](./wallpaper-rendering.md) | [风险评估](./risk-assessment.md)
