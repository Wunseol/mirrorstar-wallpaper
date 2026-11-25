[← 返回文档索引](../index.md) > 架构与技术栈 > 进程架构与 IPC

# MirrorStar Wallpaper（镜星壁纸）— 进程架构与 IPC

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.0                      |
| 更新日期 | 2026-11-24                |
| 文档状态 | 已实现（已按真实代码核对，2026-11-24）        |

> 本文由原《进程架构》与《基础设施》的 IPC/序列化章节、以及《UI 框架》的 UI 子进程边界/按需启动/空闲退出/单实例/托盘章节合并去重而成。

## 1. 进程模型

### 1.1 混合进程架构

采用**混合进程架构**——壳进程（`mirrorstar-wallpaper.exe`）内渲染 Image/Gif，视频经外部 `mpv.exe` 子进程渲染，并按需启动三类独立子进程：原生 UI 主窗口（`mirrorstar-ui.exe`）、网页壁纸（壳进程 `--wp-proc` 模式）、外部视频播放器（`mpv.exe`）：

```
┌──────────────────────────────────────────────────────────┐
│        mirrorstar-wallpaper.exe（壳进程，Tauri）          │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
│  │ Tauri    │ │ 图片线程  │ │ GIF 线程  │ │ 视频线程   │  │
│  │ 主线程   │ │ (GetMsg) │ │ (Timer)  │ │ (mpv IPC) │  │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────────────┐  │
│  │ 全屏检测 │ │ 配置监视 │ │ WebRenderer (代理层)     │  │
│  │ (Hook)   │ │ (notify) │ │  ├ ProcessManager       │  │
│  └──────────┘ └──────────┘ │  └ WpProcIpcClient       │  │
│  ┌────────────────────────────────────────────────────┐  │
│  │ UI 子进程管理（spawn / 握手 / 空闲退出 / 单实例）  │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
     │                 │                         │
     │ CreateProcessW  │ CreateProcessW + 命名管道│ CreateProcessW + stdio(NDJSON)
     ▼                 ▼                         ▼
┌──────────────┐  ┌──────────────────────┐  ┌──────────────────────────┐
│   mpv.exe    │  │mirrorstar-wallpaper.exe│  │ mirrorstar-ui.exe        │
│  (视频壁纸)  │  │ --wp-proc (Web 壁纸) │  │ (原生主窗口 UI，Slint)   │
│  按需启动    │  │  ┌────────────────┐  │  │ 软件渲染 · 虚拟化网格    │  │
└──────────────┘  │  │ WebView2 环境  │  │  │ 按需启动 · 空闲超时退出  │  │
                  │  └────────────────┘  │  └──────────────────────────┘
                  └──────────────────────┘
                     ↑ 仅 Web 壁纸时启动      ↑ 托盘打开/左键或二次启动时
                     ↑ 关闭 Web 壁纸时终止    ↑ 关闭即退出/隐藏超时后退出
```

**设计原则：**
- 壳进程（`mirrorstar-wallpaper.exe`）内通过 WallpaperEngine 管理所有壁纸渲染；**不再创建任何 WebView2 主窗口**（`app.windows` 为 `[]`）
- Image/Gif 渲染器在壳进程专用线程中运行（GDI + 双缓冲）
- Video 渲染器通过 ProcessManager 直接 spawn `mpv.exe` 子进程（按需）
- **Web 渲染器为代理层**：通过 ProcessManager 以 `--wp-proc` 模式 spawn 壳进程自身（`mirrorstar-wallpaper.exe --wp-proc`，按需），仍是独立进程（崩溃隔离不变），通过 WpProcIpcClient 命名管道通信
- **UI 主窗口为独立进程** `mirrorstar-ui.exe`（crate `crates/mirrorstar-ui/`，Slint 1.18 软件渲染）：由壳侧 UI 子进程管理模块按需 spawn，通过 `ui_proto` NDJSON 行协议 over stdio 通信；业务逻辑仅在壳侧，UI 不复制业务、不直接读写配置文件
- HWND 通过 FindWindowW 获取后，壳进程执行桌面壁纸层嵌入（与视频宿主窗口嵌入逻辑一致）

**内存优化：**
- 无 Web 壁纸、无主窗口时，无额外常驻渲染子进程；仅壳进程常驻（视频壁纸时另有按需的 mpv.exe，网页壁纸时另有按需的 --wp-proc 子进程）
- 壳进程不创建 WebView2 主窗口/环境；WebView2 仅在网页壁纸 `--wp-proc` 子进程（以及 UI 进程的临时网页预览）中按需加载
- 子进程在 Web 壁纸关闭时立即终止，释放全部内存
- UI 子进程在关闭主窗口（关闭按钮 / Alt+F4）时立即退出；托盘隐藏时保留进程，隐藏超时（默认 300s，环境变量 `MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`）后退出回收内存，再次请求可重新拉起；同一时刻至多一个实例
- UI 子进程由壳侧 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）兜底：壳退出/崩溃时内核回收 UI 及其后代（预览 mpv / WebView2），与 UI 自身的正常退出路径互补
- 无独立看门狗进程（mirrorstar-watchdog 已在阶段2移除）

**当前实测基线（2026-11-24，Debug 构建）**

| 场景 | Private | Working Set |
|------|---------|-------------|
| 壳，UI 关闭、暂停态 | 8.7MB | 22.9MB |
| 壳，UI 关闭、活跃视频态 | 7.7–8.6MB | 21.7–22.8MB |
| 壳 + UI（不含 mpv） | 14.1–16.7MB | 43.2–46.9MB |
| mpv 子进程 | 430.8–432.2MB | 202.5–204.1MB |

- 目标（壳进程 Private）：暂停态 <25MB、活跃视频态 <30MB；上表两项均低于目标
- 壳进程（UI 关闭、视频播放）采样约 **22 线程 / 270 句柄**
- 启动：冷启动 `setup_ready` 43ms；热启动首次 441ms、后续 133–164ms（目标均 <0.5s）
- 关闭主窗口：UI 子进程 93ms 内退出；空闲回收默认 300s（`DEFAULT_IDLE_TIMEOUT_SECS = 300`，`src-tauri/src/ui_process/mod.rs`）
- 视频切换：379–431ms（目标 <0.5s）
- 上述为单机 Debug 实测值，仅作基线参考，随机器与构建配置变化

**关键澄清：** wp-proc **仅**处理 Web 壁纸。视频壁纸使用外部 `mpv.exe`，GIF/图片在壳进程内渲染，主窗口 UI 由 `mirrorstar-ui.exe` 负责。这**不是**一个统一处理所有壁纸类型的子进程。

### 1.2 单实例保护

MirrorStar 使用 Windows 命名互斥体（`CreateMutexW`）确保同一时间只有一个实例运行。该机制在 `src-tauri/src/main.rs` 的 `ensure_single_instance()` 中实现：

```rust
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONINFORMATION};

fn ensure_single_instance() -> bool {
    unsafe {
        match CreateMutexW(
            None,
            false,
            windows::core::w!("MirrorStarWallpaper_SingleInstance"),
        ) {
            Ok(_mutex) => {
                // CreateMutexW 返回 Ok 即使 mutex 已存在，需通过 GetLastError 判断
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    // 主流行为：通知已运行实例显示主窗口（隐藏→显示/最小化→还原/已可见→置前）
                    if mirrorstar_wallpaper_lib::single_instance::signal_primary_instance() {
                        return false;
                    }
                    // 信号无法送达（主实例仍在早期启动或事件创建失败）：回退提示
                    MessageBoxW(
                        None,
                        windows::core::w!("镜星壁纸已在运行中。"),
                        windows::core::w!("提示"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                    return false;
                }
                // windows-rs 0.58 的 HANDLE 无 Drop 行为，进程退出时由系统回收
                let _ = _mutex;
                true
            }
            Err(e) => {
                eprintln!("创建互斥体失败: {}", e);
                true // 互斥体创建失败时仍继续运行
            }
        }
    }
}
```

**要点：**
- 互斥体名称：`MirrorStarWallpaper_SingleInstance`
- `CreateMutexW` 即使 mutex 已存在也会返回 `Ok`，必须通过 `GetLastError() == ERROR_ALREADY_EXISTS` 判断是否已有实例运行
- 互斥体句柄无 `Drop` 行为，进程退出时由系统自动释放
- 检测到已有实例时，优先通过命名事件 `MirrorStarWallpaper_ShowMainWindow` 通知主实例显示主窗口（`single_instance::signal_primary_instance`，25ms 间隔重试至 1s），主实例由等待线程 `single_instance::install_listener` 接收后走 `open_main_window`；事件不可用时回退 `MessageBoxW` 提示并退出

### 1.3 子进程架构

壁纸渲染层面存在**两种**按需启动的子进程，由 `ProcessManager`（`crates/mirrorstar-core/src/process/manager.rs`，以实测为准）统一管理：`CreateProcessW` 启动 + 3 秒等待 + `TerminateProcess` 强制终止，并把子进程关联到 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job Object（管理器析构/显式停止时由内核兜底终止，避免孤儿进程），提供 `is_running()` / `pid()` / `handle()` 状态查询。此外，主窗口 UI 是**第三种**按需子进程（`mirrorstar-ui.exe`），由壳侧 UI 子进程管理模块负责（见下文）。

#### mpv.exe（视频壁纸子进程）

- **启动方式**：由 `VideoRenderer`（`crates/mirrorstar-core/src/wallpaper/video.rs`）通过 `ProcessManager` spawn
- **控制方式**：`MpvIpcClient`（`crates/mirrorstar-core/src/ipc/mpv_protocol.rs`）通过 mpv 原生 JSON IPC 控制
- **命名管道**：`\\.\pipe\mirrorstar-mpv-{uuid}`（UUID 由壳进程生成，通过 `--input-ipc-server` 参数传给 mpv）
- **窗口标题**：`MirrorStarVideo`（通过 `--title=MirrorStarVideo` 参数设置，用于 `FindWindowW` 查找）
- **find_mpv() 路径查找策略**：
  1. 优先检查捆绑路径 `<exe_dir>/mpv/mpv.exe`
  2. 回退到系统 PATH 中的 `mpv.exe`
- **启动参数**（部分）：`--idle=yes`、`--loop-file`、`--hwdec=auto`、`--vo=gpu`、`--force-window=yes`、`--input-ipc-server=\\.\pipe\mirrorstar-mpv-{uuid}`、`--title=MirrorStarVideo`。mpv 以 `--idle=yes` 空窗口启动，**视频文件不作为命令行参数传入**，而是在窗口创建后经 IPC `loadfile` 加载（见 `video.rs`），因此启动参数中不得包含视频路径

#### 壳进程 `--wp-proc` 模式（Web 壁纸子进程）

- **启动方式**：由 `WebRenderer`（`crates/mirrorstar-core/src/wallpaper/web.rs`，代理层）通过 `ProcessManager` 以 `--wp-proc` 模式 spawn 壳进程自身（`mirrorstar-wallpaper.exe --wp-proc`）
- **控制方式**：`WpProcIpcClient`（`crates/mirrorstar-core/src/ipc/wp_proc.rs`）通过自定义 `WpProcCommand` 协议控制
- **命名管道**：`\\.\pipe\mirrorstar-wp-{uuid}`（管道名 `mirrorstar-wp-{uuid}`，通过 `--pipe-name` CLI 参数传入）
- **代码规模**：`crates/mirrorstar-wp-proc/`（5 个文件：com / command / ipc_server / lib / webview；已由独立 exe 改为**库 crate**，随壳 exe 分发，由 `lib.rs` 的 `run` / `run_with_argv` 入口调用），使用 `webview2-com` crate
- **CLI 参数**（`--wp-proc` 之后）：
  - `--source`：初始网页源（URL 或 file:// 路径）
  - `--pipe-name`：命名管道名称
  - `--title`：窗口标题（UUID，用于 `FindWindowW` 查找）
  - `--rect`：初始窗口位置和大小（`x,y,width,height`；缺失或非法时按解析错误退出）
  - `--cdp-port`：可选 Chrome DevTools 调试端口，**仅测试用**；设置 `MIRRORSTAR_WP_CDP_PORT` 时由壳追加，生产环境不传
- **实现要点**：
  - 命名管道服务端：`CreateNamedPipeW` + `ConnectNamedPipe` + `BufReader`/`BufWriter`
  - IPC 线程：读取 JSON 命令行 → 反序列化 `WpProcCommand` → mpsc 发送到主线程 → `PostMessageW` 唤醒 → 等待响应 → 序列化写回管道
  - WebView2 环境：`CreateCoreWebView2Environment` + `CreateCoreWebView2Controller` + URL 导航（支持 http/https 和 file://）
  - 窗口类注册：固定类名 `MirrorStarWebWallpaperCls`，窗口标题使用 `--title` 参数
  - 主消息循环：`GetMessageW` + `WM_WEB_COMMAND`（`WM_USER + 20`）处理
  - 命令处理：Play（导航到源）、Terminate（销毁窗口+退出）、SetPosition（`SetWindowPos` + `SetBounds`）、Pause/Resume（状态标记）、SetVolume、ToggleMute

#### mirrorstar-ui.exe（原生 UI 子进程）

- **启动方式**：托盘菜单"打开主窗口"、托盘左键切换或二次启动唤起时，由壳侧 UI 子进程管理模块按需 spawn（`crates/mirrorstar-ui/`，workspace 成员，二进制与主 exe 同目录）；拉起+握手在独立线程执行，不阻塞托盘事件线程
- **UI 技术**：Slint 1.18，**绑定软件渲染**（`default-features=false` + 仅 `renderer-software`），不编译 femtovg/skia，避免 winit 选择 OpenGL 后端带来的额外内存
- **通信方式**：与壳进程通过 `mirrorstar-core::ui_proto` 的 **NDJSON 行协议 over stdio** 通信，消息类型 `Hello` / `Request` / `Response` / `Event` / `Control`
- **职责边界**：仅负责界面呈现与用户交互（壁纸库展示、设置面板、拖放、搜索、虚拟化网格、四类预览）；**不复制业务逻辑、不直接读写配置文件**，所有业务经 `Request` 转发至壳侧处理
- **生命周期**：按需启动 → 握手（`Hello`）→ 关闭主窗口立即退出 / 托盘隐藏后按空闲超时（默认 300s，环境变量 `MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`）退出回收内存 → 再次请求可重新拉起；**同一时刻至多一个实例**
- **用户主动唤起时置前聚焦**：托盘 / 菜单 / 二次启动触发的显示、还原、置前会调用 `SetForegroundWindow`（壳侧先 `AllowSetForegroundWindow(ui_pid)`），使任务栏按钮恢复「一次点击最小化、再一次还原」；受 Windows 前台锁限制时仅记日志。纯净模式等非用户主动场景仍禁止抢前台；窗口标题保持"镜星壁纸"，被全屏检测按标题精确排除
- **网格与预览**：原生 Slint 虚拟化壁纸网格（固定卡尺寸 + 固定列数，仅实例化可见区间 ±2 行缓冲）；缩略图按本地文件路径直读（不经 IPC 传字节）；图片/GIF 自绘，视频在 UI 进程内 spawn mpv 子窗口，网页按需创建临时 WebView2 子窗口（关闭后 `msedgewebview2.exe` 计数回落 0）
- **事件转发**：壳侧事件桥以 `bridge::FORWARDED_EVENTS`（14 个：壁纸增/删/更新、缩略图失败、源缺失、状态变更、轮换、加载、GIF 超大、配置变更、配置加载错误、桌面状态、音频禁用、显示器热插拔）为唯一事实来源，仅在 UI 进程运行时惰性转发为 `Event`（无 UI 进程时零开销、零报错）

### 1.4 COM 初始化策略

MirrorStar **显式**初始化 COM，而非依赖 Tauri 内部初始化。实现上以 RAII guard（`ComGuard`）配对 `CoInitializeEx` / `CoUninitialize`，按线程职责选择 apartment：

**壳进程主线程**（`src-tauri/src/lib.rs` 的 `run()`，STA）：

```rust
// 主线程 STA，与 Tauri/tao 要求一致；必须在任何 COM 调用之前完成。
let _com_guard = commands::wallpaper::ComGuard::new_sta_or_exit();
```

- `new_sta_or_exit()` 调用 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`
- `S_OK` / `S_FALSE` → 标记为已初始化，guard 在 `run()` 返回时 Drop 并调用一次 `CoUninitialize`
- `RPC_E_CHANGED_MODE` → 记录 warn 并跳过 `CoUninitialize`（COM 规则：不允许在别的 apartment 模式下反初始化）
- 其他错误 → `std::process::exit(1)`（主线程 COM 是必需依赖）

**WASAPI 音量路径**（`commands::wallpaper::ComGuard::new()`，MTA）：

```rust
let _com_guard = ComGuard::new(); // CoInitializeEx(None, COINIT_MULTITHREADED)
```

- 在 `spawn_blocking` 线程上按 MTA 初始化，与 `VolumeControl` 的 WASAPI free-threaded 语义匹配
- `RPC_E_CHANGED_MODE` 时跳过 `CoUninitialize`，失败仅记录 warn

**wp-proc 子进程**（壳 `--wp-proc` 模式，`crates/mirrorstar-wp-proc/src/com.rs`，STA）：

```rust
ComGuard::new() // CoInitializeEx(None, COINIT_APARTMENTTHREADED)
```

- 返回 `Err` 时由 `run()` 向上传播，壳入口以退出码 1 结束子进程
- `RPC_E_CHANGED_MODE` 视为致命错误（WebView2 要求 STA，MTA 下不可用），不再静默继续

**要点：**
- 壳主线程显式 STA，确保在 `VolumeControl::new()` 缓存 COM 接口、Tauri/tao 初始化之前 COM 已就绪
- `ComGuard` 的 Drop 统一配对 `CoUninitialize`，并区分"是否真的由本 guard 初始化"（`S_FALSE` 也需配对，`RPC_E_CHANGED_MODE` 不配对）
- 不存在"依赖 Tauri 内部 COM 初始化"的路径

### 1.5 启动时序

壳进程 `run()`（`src-tauri/src/lib.rs`）的实际初始化顺序如下。**注意：无看门狗进程 spawn 步骤。**（`pause_senders` 已移入 `WallpaperEngine`，不再是独立的初始化项。）

**Phase 1：Tauri Builder 之前**

1. **启动埋点** — `perf::mark_boot_stage("process_start")`
2. **数据根初始化** — `config::init_data_root(config::resolve_data_root())`（便携化：数据根 = exe 所在目录），必须早于日志初始化
3. **panic hook** — `install_panic_hook()`（追加写 `crash.log` + tracing 日志），必须早于其它线程 spawn
4. **`init_logging()`** — tracing + tracing-subscriber + tracing-appender（按日滚动文件 + 标准输出双通道）；随后记录 `log_init` 生命周期/启动打点并建立 `boot_start` 基线
5. **COM 初始化（STA）** — `ComGuard::new_sta_or_exit()`，详见 1.4
6. **`DesktopIntegrator::new()`** — 保存原始系统壁纸，WorkerW 延迟初始化（此时不查找 WorkerW）
7. **后台线程 `mirrorstar-workerw-init`** — 预初始化 WorkerW，调用 `ensure_initialized()`；失败仅告警并在首次使用时重试，JoinHandle 存 `WORKERW_INIT_THREAD` 供退出 join
8. **`VolumeControl::new()`** — WASAPI 音量控制（缓存 COM 接口）；失败降级为 `new_disabled()` no-op，并记录 `audio_disabled`
9. **`WallpaperEngine::new()`** — 传入 desktop 与 volume_control
10. **`ConfigManager::new()`** — 失败 `std::process::exit(1)`
11. **后台线程 `mirrorstar-boot-cleanup`** — 清理损坏缩略图、清空 WebView2 缓存目录（`webview2-cache/<pid>/`）、全量日志剪枝；均 best-effort
12. **应用 GIF 内存策略 / 缩放模式 / 播放设置** — 从配置种入引擎
13. **`config_manager.start_watching()`** — notify 文件监视（热重载，500ms 防抖）
14. **`start_fullscreen_monitor()`** — `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 事件驱动全屏检测 + 状态去抖 + 自身窗口排除
15. **`start_explorer_restart_monitor()`** — 处理 `TaskbarCreated`、WorkerW/Progman 销毁、会话解锁、睡眠恢复，统一经 `try_heal_desktop_layer` 自愈；同一监控窗口的 wndproc 处理 `WM_POWERBROADCAST`（`PBT_APMPOWERSTATUSCHANGE` 时一次性 `GetSystemPowerStatus`，非轮询）
16. **`cleanup_stale_child_processes()`** — 清理上次崩溃遗留的孤儿 `mpv.exe` / `mirrorstar-wp-proc.exe` / `mirrorstar-ui.exe`（仅限当前 exe 目录内）
17. **`ui_process::init()`** — 解析 `mirrorstar-ui.exe` 路径与空闲阈值，仅初始化管理器，不主动拉起
18. **`SchedulerHandle::new()`** — 构造轮换调度器共享句柄，存入 `SHARED_SCHEDULER`
19. **`AppState` 构造** — 组装 config_manager / wallpaper_engine / desktop / scheduler / 托盘状态
20. **自定义 Tokio runtime** — `worker_threads(2)` + 512KB 栈 + 8 个 blocking 线程，`tauri::async_runtime::set(handle)`
21. **`liveness_watchdog`** — 必须在 `async_runtime::set()` **之后**启动（5s 采样 / 1.3s 确认 / 60s 冷却）

**Phase 2：Tauri Builder + setup**

22. **Builder** — 注册 `tauri-plugin-dialog` / `tauri-plugin-autostart`，`manage(app_state)`；setup 内依次：
    - 设置 `SHARED_APP_HANDLE`
    - 安装 UI 事件桥 `install_event_forwarding` + 启动空闲检查 `spawn_idle_tick`
    - 安装二次启动监听 `single_instance::install_listener`
    - 需要时 emit `audio-disabled`；启动内存归因采样（60s）与启动期内存时间线（前 60s，1s 间隔）
    - 全局壁纸状态订阅任务、配置变更/配置加载错误回调
    - 创建托盘菜单（5 项）与托盘图标
    - `start_workerw_check()`（300s 兜底）
    - `scheduler.start(app.handle())` 启动轮换主循环
23. **`setup_ready` 打点** — 记录 `PERF-BOOT` 与启动阶段汇总

**Phase 3：事件循环**

24. **`.build(context).run(...)`** — `RunEvent::ExitRequested` 中：`code=None` → `api.prevent_exit()` 保持托盘驻留；`code=Some(_)` → 先 `ui_process::manager().shutdown()` 回收 UI，再 `perform_shutdown_blocking` 统一清理

**关键说明：**
- 启动序列中**无**看门狗进程 spawn 步骤（`mirrorstar-watchdog` crate 已在阶段2移除）
- 壳进程崩溃时操作系统自动回收子进程（父子进程关系 + Job Object），不需要独立看门狗进程清理资源
- 主窗口 UI 子进程按需启动：启动时仅创建系统托盘图标，主窗口在用户点击托盘"打开主窗口"时由壳侧按需 spawn `mirrorstar-ui.exe`；关闭主窗口时 UI 子进程立即退出；托盘隐藏超时（默认 300s）后退出回收内存，壳侧不再创建任何 WebView2 主窗口

### 1.6 崩溃恢复（退出监听）

针对壁纸子进程（mpv / wp-proc）的崩溃，仅通过 `spawn_proc_exit_monitor()`（`crates/mirrorstar-core/src/wallpaper/mod.rs`）监听其退出：当子进程以异常状态退出时，更新壁纸状态为 `Terminated` 并调用 `notify_state_changed` 通知壁纸引擎。

- **无自动 respawn**：壁纸子进程崩溃后不会被自动重启，由引擎/用户在收到终止通知后决定后续处理
- 资源回收由操作系统自动完成（父子进程关系 + `ProcessManager` 的 `KILL_ON_JOB_CLOSE` Job Object），无需独立看门狗进程

UI 子进程的回收路径独立于壁纸渲染器：

- **正常关闭**：UI 主动退出事件循环 → stdio EOF → 壳侧协议泵 `on_pump_exit` 清理运行槽位（非 0 退出码才记 warn）
- **壳退出/崩溃**：壳侧 `UiJobGuard`（`KILL_ON_JOB_CLOSE`）由内核回收 UI 及其后代
- **异常残留**：下次启动 `cleanup_stale_child_processes()` 按进程名 + 映像路径清理
- UI 不自动重启，仅在用户再次请求显示主窗口时按需拉起

---

## 2. IPC 通信设计

### 2.1 三套独立协议（两套命名管道 + 一套 stdio）

MirrorStar 在壳进程与子进程之间使用**三套独立**的通信协议：mpv 原生 JSON IPC、wp-proc 自定义 `WpProcCommand` 协议、以及壳↔UI 子进程的 `ui_proto` NDJSON over stdio。**其中 mpv 与 wp-proc 采用命名管道 + JSON + 换行分隔格式**，并复用相同的连接重试 + request_id 响应匹配模式；**`ui_proto` 走标准输入输出流（非命名管道）**。

> **表述校正**：本套协议是**三套独立协议**，其中仅 mpv/wp-proc 走命名管道，`ui_proto` 走 stdio。早期文档中"三套命名管道"等表述不准确，以本处为准。

#### 协议一：mpv 原生 IPC（视频壁纸）

- **客户端**：`MpvIpcClient`（`crates/mirrorstar-core/src/ipc/mpv_protocol.rs`）
- **管道**：`\\.\pipe\mirrorstar-mpv-{uuid}`（UUID 由壳进程生成，通过 mpv `--input-ipc-server` 参数创建）
- **格式**：mpv 原生 JSON 协议（JSON 行，request_id 匹配响应）
- **连接重试**：40 次尝试 × 50ms 间隔（`ipc.connect(MPV_CONNECT_RETRIES, MPV_CONNECT_INTERVAL_MS) = 2s` 总超时）
- **命令**：`pause` / `resume` / `set_volume` / `set_speed` / `quit` / `get_property` / `set_property`
- **响应结构**：`{ error, data?, request_id }`（mpv 原生格式）

#### 协议二：wp-proc WpProcCommand（Web 壁纸）

- **客户端**：`WpProcIpcClient`（`crates/mirrorstar-core/src/ipc/wp_proc.rs`）
- **管道**：`\\.\pipe\mirrorstar-wp-{uuid}`（pipe_name 通过 `--pipe-name` CLI 参数传入子进程）
- **格式**：自定义 JSON + 换行分隔，request_id 匹配响应
- **连接重试**：160 次尝试 × 50ms 间隔（`ipc.connect(WP_PROC_CONNECT_RETRIES, WP_PROC_CONNECT_INTERVAL_MS) = 8s` 总超时，覆盖 WebView2 冷启动）
- **命令**：`Play{source}` / `Terminate` / `SetPosition{x,y,w,h}` / `Pause` / `Resume` / `SetVolume{volume}` / `ToggleMute`

**命令示例**（壳进程 → 子进程）：

```json
{"command":"play","request_id":1,"source":"https://..."}
{"command":"terminate","request_id":2}
{"command":"set_position","request_id":3,"x":0,"y":0,"width":1920,"height":1080}
{"command":"pause","request_id":5}
{"command":"resume","request_id":6}
{"command":"set_volume","request_id":7,"volume":0.8}
{"command":"toggle_mute","request_id":8}
```

**响应示例**（子进程 → 壳进程）：

```json
{"request_id":1,"status":"ok"}
{"request_id":1,"status":"error","error":"WebView2 未初始化"}
```

#### 协议三：`ui_proto` NDJSON over stdio（壳 ↔ UI 子进程）

- **定义位置**：`mirrorstar-core::ui_proto`（crate `crates/mirrorstar-core/src/ui_proto/`，壳与 `mirrorstar-ui` 共享）
- **传输通道**：UI 子进程的标准输入 / 标准输出（**非**命名管道）
- **格式**：一行一条 JSON（NDJSON 行协议）
- **消息类型**：
  - `Hello`：子进程启动后的握手（版本 / 能力协商）
  - `Request`：壳 → UI 或 UI → 壳的业务请求（携带 request_id 匹配 `Response`）
  - `Response`：对 `Request` 的应答
  - `Event`：壳 → UI 的异步状态推送（壁纸列表、播放状态、进度等）
  - `Control`：生命周期控制（`Show` / `Hide` / `Toggle` / `Exit`）。`Toggle`（壳 → UI）按主窗口**可见性**分派：可见且未最小化 → 隐藏；最小化 → 还原并置前聚焦；已隐藏或 UI 子进程未运行 → 拉起并显示（居中到鼠标所在显示器）并置前聚焦。判定不查询 `GetForegroundWindow`（点击托盘会改变系统前台，跨进程前台判定必然失真）。置前聚焦由壳侧在托盘事件线程上、仅对已运行的 UI 进程按 pid 精确调用 `AllowSetForegroundWindow(ui_pid)` + UI 侧 `SetForegroundWindow` 协作完成（二次启动由新实例以 `ASFW_ANY` 授权）；仍被前台锁拒绝时用 `AttachThreadInput` 临时附加到前台线程后重试（成功即解除），仍失败仅记日志；`Hide` 不改变前台

**关键澄清：** 这是**三套独立的协议**，不是一个统一协议。mpv 使用 mpv 原生 JSON 协议，wp-proc 使用自定义 `WpProcCommand` 协议（二者走命名管道），壳与 UI 子进程使用 `ui_proto` NDJSON 行协议（走 stdio）。前两者复用了相同的连接重试 + request_id 响应匹配设计模式。

### 2.2 HWND 获取方式

两种壁纸子进程（mpv / wp-proc）的窗口 HWND 均在壳进程通过 `FindWindowW` 获取，然后由 `DesktopIntegrator` 执行 WorkerW 嵌入。**HWND 不通过 IPC 回传。**（UI 主窗口不属于桌面层嵌入对象，由 UI 进程自行管理。）

**mpv（视频壁纸）**：
- 通过窗口标题 `MirrorStarVideo` 查找（`FindWindowW`）
- 重试：20 次 × 100ms = 2 秒（`WINDOW_FIND_RETRIES`，窗口查找参数）
- PID 验证：`GetWindowThreadProcessId` 确认窗口属于 mpv 子进程

**wp-proc（Web 壁纸）**：
- 通过 `--title` 参数传入的 UUID 标题查找（`FindWindowW`）
- 重试：20 次 × 100ms = 2 秒（`WINDOW_FIND_RETRIES`）
- PID 验证：`GetWindowThreadProcessId` 确认窗口属于 wp-proc 子进程

**嵌入流程**：HWND 获取后，壳进程的 `DesktopIntegrator` 执行桌面壁纸层嵌入（raised 下为 `WS_CHILD`/layered/Progman + DefView z-order；旧层级为 `SetParent` + `SetWindowPos(HWND_BOTTOM)`），与视频宿主窗口嵌入逻辑一致。

### 2.3 进程模型要点总结

- **混合进程架构**：Image/Gif 渲染器在壳进程专用线程中运行，Video（mpv）与 Web（wp-proc 模式）通过独立子进程渲染，均按需启动
- **崩溃隔离**：Web 壁纸经壳进程 `--wp-proc` 模式子进程渲染，仍是独立进程，崩溃不影响壳进程
- **起进程监控**：通过 `spawn_proc_exit_monitor` 监听子进程退出 + 操作系统自动回收，无需独立看门狗进程（watchdog crate 已在阶段2移除）
- **逻辑暂停**：通过 PauseSender 快速通道经 mpsc 直接投递暂停/恢复/音量命令（命令层短暂持 engine 锁定位 sender，不等待渲染器处理）
- **原生 UI 子进程**：主窗口由 `mirrorstar-ui.exe`（Slint 软件渲染）承载，按需启动、空闲超时退出、单实例，经 `ui_proto` NDJSON（stdio）与壳通信；业务逻辑仅在壳侧
- **内存优化**：无 Web 壁纸、无主窗口时，无额外常驻渲染子进程；仅壳进程常驻（视频壁纸时另有按需的 mpv.exe，网页壁纸时另有按需的 --wp-proc 子进程），WebView2 运行时仅按需加载，关闭 Web 壁纸即终止子进程释放全部内存；UI 子进程在关闭主窗口时立即退出、托盘隐藏超时后退出回收内存

---

## 3. UI 子进程管理（边界 / 生命周期 / 托盘）

> 本节由《UI 框架》的进程边界与生命周期章节并入。

### 3.1 按需启动（On-Demand）与空闲退出

MirrorStar 的主窗口不在应用启动时创建，也不由壳创建任何 WebView2 窗口（`tauri.conf.json` 的 `app.windows` 为 `[]`）：

- **按需启动**：用户左键单击托盘图标或选择"打开主窗口"时，壳侧 UI 子进程管理模块 spawn `mirrorstar-ui.exe`，先握手（`Hello`）再显示
- **关闭即退出**：关闭按钮 / Alt+F4 时 UI 关闭预览、上报 WindowHidden 后主动退出事件循环，进程立即结束，壳侧协议泵经 stdio EOF 清理运行槽位（2026-11-24 实测 93ms 内退出）
- **隐藏空闲退出**：托盘左键隐藏 / 壳侧 Hide 时保留进程，隐藏满空闲超时（默认 300s，环境变量 `MIRRORSTAR_UI_IDLE_TIMEOUT_SECS`；常量 `DEFAULT_IDLE_TIMEOUT_SECS`）后壳侧退出并回收 UI 子进程，再次请求可重新拉起。回收后回落至壳内存量级（2026-11-24 实测暂停态 Private 8.7MB / WS 22.9MB；历史记录中的 ≈7.6MB 为旧口径）
- **单实例**：同一时刻至多一个 UI 子进程实例
- **用户主动唤起时置前聚焦**：显示 / 还原 / 置前路径调用 `SetForegroundWindow`（壳侧先 `AllowSetForegroundWindow(ui_pid)`），使任务栏按钮恢复「一次点击最小化、再一次还原」；受前台锁限制时仅记日志。纯净模式等非用户主动场景仍禁止抢前台；窗口标题保持"镜星壁纸"，被全屏检测按标题精确排除

此策略的优势：

- 托盘常驻场景下，用户不操作 UI 时零 UI 进程开销，聚焦壁纸渲染性能
- 主窗口静置时壳 + UI 的内存占用显著低于旧 WebView2 方案：2026-11-24 实测（不含 mpv）Private 14.1–16.7MB / WS 43.2–46.9MB；历史记录中的合计 private ≈16.8MB 为旧口径

### 3.2 系统托盘

`src-tauri/src/lib.rs` 通过 Tauri 2 内置 `tray-icon` feature 创建托盘，菜单共 5 项（菜单 ID 依次为 `open` / `next_wallpaper` / `pause_resume` / `pure_mode` / `quit`）：

```
镜星壁纸
├── 打开主窗口（open）
├── 下一张壁纸（next_wallpaper）
├── 暂停壁纸（pause_resume，点击切换为"恢复播放"）
├── 纯净壁纸（按任意键退出）（pure_mode，勾选项 CheckMenuItem）
└── 退出（quit）
```

托盘菜单中可动态更新的 2 个 handle 分别存储于 `tray_pause_resume_item`（`OnceLock<MenuItem>`，暂停/恢复项文本随状态动态更新）与 `tray_pure_mode_item`（`OnceLock<CheckMenuItem>`，勾选态与真实运行态双向同步）；`next_wallpaper` 触发手动换图（不受暂停限制）。托盘图标缺失时优雅降级（不因图标文件异常导致启动失败）。

> 底层基于 `Shell_NotifyIcon`；托盘不作为独立 crate 依赖，直接启用 workspace `tauri = { version = "2.11", features = ["tray-icon"] }`。

---

## 4. IPC 基础设施

> 本节由《基础设施》的 IPC 与序列化章节并入。

### 4.1 IPC 选型：Windows 命名管道（同步 Win32 API）

- 使用 `CreateNamedPipeW` / `ConnectNamedPipe`（`std::fs` 读写），**不使用** `tokio::net::windows::named_pipe`
- 双向、可靠有序，Windows 原生，无外部依赖
- 管道服务端由子进程（mpv.exe / 壳进程 `--wp-proc` 模式实例）创建，壳进程作为客户端连接
- 消息为 JSON 文本，换行分隔

### 4.2 协议细节

#### 1. mpv 原生 JSON IPC（视频壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `MpvIpcClient`（`mirrorstar-core/src/ipc/mpv_protocol.rs`） |
| 管道路径 | `\\.\pipe\mirrorstar-mpv-{uuid}`（由 mpv `--input-ipc-server` 创建） |
| 协议格式 | mpv 原生 JSON 协议 |
| 通信方式 | 壳进程作为客户端连接 mpv 管道服务端 |
| 命令 | mpv 原生属性命令（`loadfile` / `set_property` `pause` / `loop-file` / `speed` / `quit` 等） |

#### 2. wp-proc JSON IPC（网页壁纸）

| 项 | 说明 |
|----|------|
| 客户端 | `WpProcIpcClient`（`mirrorstar-core/src/ipc/wp_proc.rs`） |
| 管道路径 | `\\.\pipe\mirrorstar-wp-{uuid}`（由 wp-proc 子进程创建） |
| 协议格式 | `WpProcCommand` JSON + 换行分隔（自定义协议） |
| 通信方式 | 壳进程作为客户端连接 wp-proc 管道服务端 |
| 命令 | `Play` / `Terminate` / `SetPosition` / `Pause` / `Resume` / `SetVolume` / `ToggleMute` |

#### 3. `ui_proto` NDJSON over stdio（壳 ↔ 原生 UI 子进程）

| 项 | 说明 |
|----|------|
| 定义位置 | `mirrorstar-core::ui_proto`（壳与 `mirrorstar-ui` 共享） |
| 传输方式 | UI 子进程标准输入 / 标准输出（**非**命名管道；同步 stdio 行读写） |
| 协议格式 | 一行一条 JSON（NDJSON 行协议） |
| 消息类型 | `Hello`（握手）/ `Request`（业务请求 + request_id）/ `Response`（应答）/ `Event`（壳→UI 异步推送，源为 `FORWARDED_EVENTS` 的 14 个事件）/ `Control`（生命周期控制：壳→UI 的 `Show` / `Hide` / `Toggle` / `Exit`，UI→壳的 `WindowHidden` / `WindowShown` 供空闲退出计时（关闭主窗口时 UI 在退出前上报 `WindowHidden`）） |

### 4.3 连接重试（实测自 subprocess_base.rs）

| 目标 | 重试参数 | 总时长 |
|------|----------|--------|
| mpv IPC | 40 × 50ms | 2s |
| wp-proc IPC | 160 × 50ms | 8s（子进程启动耗时更长） |
| 子进程渲染窗口查找 | 20 × 100ms | 2s |

---

## 5. 序列化

### 选择：serde + serde_json + toml

- **serde 1**（`derive`）：`#[derive(Serialize, Deserialize)]`
- **serde_json 1**：IPC 消息（mpv JSON / wp-proc JSON / `ui_proto` NDJSON）
- **toml 0.8**：配置文件
- `#[serde(default)]` 支持字段默认值；`#[serde(rename = "...")]` 控制序列化键名（如 `per_monitor` / `span`）

> 异步运行时（tokio）与配置/日志基础设施见[依赖与数据流](./08-依赖与数据流-Dependency-and-Data-Flow.md)。

---

**相关文档：**

- [架构与技术栈总览](./01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [模块设计](./02-模块设计-Module-Design.md)
- [桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md)
- [壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)
- [暂停恢复机制](./06-暂停恢复机制-Pause-Resume.md)
- [依赖与数据流](./08-依赖与数据流-Dependency-and-Data-Flow.md)