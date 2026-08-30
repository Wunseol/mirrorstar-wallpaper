[← 返回文档索引](../index.md) > [技术栈](./技术栈总览-Tech-Stack-Overview.md) > 基础设施

# MirrorStar Wallpaper（镜星壁纸）技术栈 — 基础设施

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 配置管理

### 选择：TOML + serde + notify

- **TOML**：支持注释，适合人工编辑，Rust 生态标准
- **serde**（`serde 1` + `toml 0.8`）：反序列化，`#[serde(default)]` 支持字段默认值
- **notify 7**：跨平台文件系统监控，用于热重载

### 配置文件与数据根

应用所有用户数据收束于**数据根目录**（`mirrorstar-core::config::data_root()`），解析顺序：

1. `MIRRORSTAR_DATA_ROOT` 环境变量（如果设置且非空）
2. **exe 所在目录**（便携模式默认，portable-data-root）
3. 回退 `%APPDATA%\mirrorstar`

数据根下存储：

```
<data_root>/
├── config.toml        # 应用配置
├── wallpapers.toml    # 壁纸库（条目 + 元数据）
├── wallpapers/        # 壁纸文件
├── thumbnails/        # 缩略图
├── logs/              # 日志（mirrorstar.log + mpv-*.log）
└── config.lock        # 写锁
```

### AppConfig 结构（实测自 settings.rs）

`AppConfig`（`mirrorstar-core/src/config/settings.rs`）分 6 段：`general` / `audio` / `pause` / `display` / `video` / `gif`。

```toml
# config.toml（示例，含默认值）
[general]
auto_start = false          # 开机自启
minimize_to_tray = true     # 关闭窗口最小化到托盘

[audio]
volume = 0.8                # 全局音量 (0.0~1.0)
muted = false

[pause]
fullscreen_action = "terminate"  # 全屏处置 none/pause/terminate（默认 terminate）
pause_on_battery = false         # 电池供电时暂停

[display]
arrangement = "per_monitor"      # 壁纸排列 per_monitor/span

[video]
hwdec = true                # 硬件解码
speed = 1.0                 # 播放速度

[gif]
memory_strategy = "balanced"     # Aggressive/Balanced/Performance/Adaptive
balanced_keep_frames = 10        # 平衡模式保留帧数（[1,1000]）
max_memory_mb = 40               # 帧像素内存预算（[10,500]）
```

### 校验（validate 策略）

各子 config 的 `validate(&mut self)` 采用**"内部修正 + warn 日志，调用方透明"**策略（不返回 `Result`）：越界/非法值就地 clamp 或回退默认，通过 `tracing::warn!` 记录原值与修正后值。

- `AudioConfig.volume`：clamp 到 `[0.0, 1.0]`（NaN / 越界回退 0.8）
- `VideoConfig.speed`：`> 0` 且 ≤ `MAX_VIDEO_SPEED`（10.0），非法回退 1.0
- `GifConfig.balanced_keep_frames`：`≥ 1` 且 ≤ `MAX_BALANCED_KEEP_FRAMES`（1000）
- `GifConfig.max_memory_mb`：`[10, 500]`

### 热重载与持久化

- `ConfigManager` 管理 `config.toml` 与 `wallpapers.toml` 的读写（`mirrorstar-core/src/config/manager.rs`）
- 使用 `notify::RecommendedWatcher` 监控，**500ms 防抖**（hot_reload.rs）
- 配置加载失败（损坏/缺失）：记录 warn 并回退到默认配置，**保证应用可启动**；构造时的加载错误暂存 `pending_config_errors`，待 Tauri setup 后作为 `config-load-error` 事件通知前端
- 写路径使用 `fs2` 锁 + 防抖落盘 + 定期保存，避免频繁写入与并发冲突

***

## 2. 日志系统

### 选择：tracing + tracing-subscriber + tracing-appender

### 初始化（实测自 core::lib.rs::init_logging）

```rust
let log_dir = crate::config::data_root().join("logs");   // 数据根/logs
std::fs::create_dir_all(&log_dir)?;

let file_appender = tracing_appender::rolling::daily(log_dir, "mirrorstar.log"); // 按日轮转
let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

let env_filter = EnvFilter::try_from_default_env()        // 优先 RUST_LOG
    .unwrap_or_else(|_| EnvFilter::new("info"));          // 回退 info

tracing_subscriber::fmt()
    .with_env_filter(env_filter)
    .init();
// guard 由 run() 持有至程序退出，early drop 会丢失日志
```

要点：

- **日志目录**：`<data_root>/logs/mirrorstar.log`，`rolling::daily` 按日轮转
- **级别控制**：`RUST_LOG` 环境变量，默认 `info`
- **`guard` 生命周期**：`init_logging()` 返回 `WorkerGuard`，由 Tauri `run()` 持有至退出，提前 drop 会丢失尾部日志
- **mpv 日志**：写入同目录 `mpv-<uuid>.log`（见壁纸渲染）
- **性能埋点**：`mirrorstar::perf` target（perf.rs），可用 `RUST_LOG=mirrorstar::perf=info` 单独过滤

***

## 3. 系统托盘

### 选择：Tauri 2 内置 `tray-icon` feature

- 直接复用 Tauri 2 的 `tray-icon` feature（workspace `tauri = { version = "2.11", features = ["tray-icon"] }`），非独立 crate
- 底层基于 Shell_NotifyIcon

### 托盘菜单（实测自 src-tauri/src/lib.rs）

```
镜星壁纸
├── 打开主窗口（open）
├── 暂停壁纸（pause_resume，点击切换为"恢复播放"）
└── 退出（quit）
```

- 菜单项 handle 存储于 `AppState`（`tray_pause_resume_item`，`OnceLock`），暂停/恢复文本随状态动态更新
- 托盘图标缺失时优雅降级，不因图标文件异常导致启动失败

***

## 4. 进程间通信（IPC）

### 选择：Windows 命名管道（同步 Win32 API）

- 使用 `CreateNamedPipeW` / `ConnectNamedPipe`（`std::fs` 读写），**不使用** `tokio::net::windows::named_pipe`
- 双向、可靠有序，Windows 原生，无外部依赖
- 管道服务端由子进程（mpv.exe / mirrorstar-wp-proc.exe）创建，主进程作为客户端连接
- 消息为 JSON 文本，换行分隔

### 两套独立 IPC 协议

#### 1. mpv 原生 JSON IPC（视频壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `MpvIpcClient`（`mirrorstar-core/src/ipc/mpv_protocol.rs`，355 行） |
| 管道路径 | `\\.\pipe\mirrorstar-mpv-{uuid}`（由 mpv `--input-ipc-server` 创建） |
| 协议格式 | mpv 原生 JSON 协议 |
| 通信方式 | 主进程作为客户端连接 mpv 管道服务端 |
| 命令 | mpv 原生属性命令（`loadfile` / `set_property` `pause` / `loop-file` / `speed` / `quit` 等） |

#### 2. wp-proc JSON IPC（网页壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `WpProcIpcClient`（`mirrorstar-core/src/ipc/wp_proc.rs`，699 行） |
| 管道路径 | 由 wp-proc 子进程创建的自定义命名管道 |
| 协议格式 | `WpProcCommand` JSON + 换行分隔（自定义协议） |
| 通信方式 | 主进程作为客户端连接 wp-proc 管道服务端 |
| 命令 | `Play` / `Terminate` / `SetPosition` / `Navigate` / `Pause` / `Resume` |

### 连接重试（实测自 subprocess_base.rs）

| 目标 | 重试参数 | 总时长 |
|------|----------|--------|
| mpv IPC | 40 × 50ms | 2s |
| wp-proc IPC | 160 × 50ms | 8s（子进程启动耗时更长） |
| 子进程渲染窗口查找 | 20 × 100ms | 2s |

> 完整 IPC 协议定义见 [进程架构 — IPC 通信设计](../02-架构设计/进程架构-Process-Architecture.md)。

***

## 5. 异步运行时

### 选择：tokio 1.52

workspace 配置：`tokio = { version = "1.52", features = ["rt", "rt-multi-thread", "macros", "time", "fs", "sync", "io-util"] }`

### 使用场景

| 场景 | 说明 |
|------|------|
| 广播通道 | WallpaperEngine 状态变更 broadcast 订阅（`sync`） |
| 事件监听 | 全屏 / 电池事件驱动处理 |
| 前端通信 | Tauri 命令与事件异步消息 |

> **注意**：命名管道 IPC 使用同步 Win32 API，**不经过** tokio 异步运行时；文件监控热重载均在独立线程完成。

***

## 6. 序列化

### 选择：serde + serde_json + toml

- **serde 1**（`derive`）：`#[derive(Serialize, Deserialize)]`
- **serde_json 1**：IPC 消息（mpv JSON / wp-proc JSON）
- **toml 0.8**：配置文件
- `#[serde(default)]` 支持字段默认值；`#[serde(rename = "...")]` 控制序列化键名（如 `per_monitor` / `span`）

***

## 7. 错误处理

### 选择：thiserror 2（库级）

workspace 使用 `thiserror = "2"`，**未引入 anyhow**（早期文档中的 anyhow 为虚构依赖）。

- **`mirrorstar-core`**：定义具体错误类型 `MirrorStarError`（`#[derive(thiserror::Error)]`），覆盖桌面集成、WorkerW 查找、子进程启动、IPC、音频控制、配置文件解析/写入、图片解码、文件监视、Win32、IO 等类别；`pub type Result<T> = Result<T, MirrorStarError>`
- **配置加载**：`ConfigLoadError` 区分 `config.toml` 与 `wallpapers.toml`，损坏时回退默认并记录 warn，保证应用可启动
- **优雅降级**：关键组件失败时降级而非崩溃（如无音频设备时 `VolumeControl` 降级为 no-op、托盘图标缺失时跳过）

***

**相关章节：** [← 总览](./技术栈总览-Tech-Stack-Overview.md) | [壁纸渲染](./壁纸渲染-Wallpaper-Rendering.md) | [风险评估](./风险评估-Risk-Assessment.md)