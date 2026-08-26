[← 返回文档索引](../README.md) > [架构设计](./overview.md) > 进程架构

# 进程架构

> 基于真实代码审计 + 混合架构实现 + Lively 对比

## 1. 进程模型

### 1.1 混合进程架构

采用**混合进程架构**——主进程内渲染 Image/Gif/Video，仅 Web 壁纸按需启动独立子进程：

```
┌──────────────────────────────────────────────────────────┐
│              mirrorstar-wallpaper.exe（主进程）             │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
│  │ Tauri    │ │ 图片线程  │ │ GIF 线程  │ │ 视频线程   │  │
│  │ 主线程   │ │ (GetMsg) │ │ (Timer)  │ │ (mpv IPC) │  │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────────────┐  │
│  │ 全屏检测 │ │ 配置监视 │ │ WebRenderer (代理层)     │  │
│  │ (Hook)   │ │ (notify) │ │  ├ ProcessManager       │  │
│  └──────────┘ └──────────┘ │  └ WpProcIpcClient       │  │
│                              └──────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
       │                              │
       │ CreateProcessW               │ CreateProcessW + 命名管道 IPC
       ▼                              ▼
┌─────────────────┐         ┌──────────────────────────┐
│   mpv.exe       │         │ mirrorstar-wp-proc.exe    │
│   (视频壁纸)    │         │ (Web 壁纸子进程)          │
│   按需启动      │         │  ┌────────────────────┐  │
└─────────────────┘         │  │ WebView2 环境      │  │
                            │  │  └ 窗口 + 消息循环  │  │
                            │  └────────────────────┘  │
                            └──────────────────────────┘
                               ↑ 仅 Web 壁纸时启动
                               ↑ 关闭 Web 壁纸时终止
```

**设计原则：**
- 主进程（`mirrorstar-wallpaper.exe`）内通过 WallpaperEngine 管理所有壁纸渲染
- Image/Gif 渲染器在主进程专用线程中运行（GDI + 双缓冲）
- Video 渲染器通过 ProcessManager 直接 spawn `mpv.exe` 子进程（按需）
- **Web 渲染器已重构为代理层**：通过 ProcessManager spawn `mirrorstar-wp-proc.exe` 子进程（按需），通过 WpProcIpcClient 命名管道通信
- HWND 通过 FindWindowW 获取后，主进程执行 WorkerW 嵌入（与 mpv 窗口嵌入逻辑一致）

**内存优化：**
- 无 Web 壁纸时：零子进程开销（仅主进程 + 可选的 mpv）
- WebView2 运行时仅加载在子进程中，主进程不加载 webview2-com
- 子进程在 Web 壁纸关闭时立即终止，释放全部内存
- 无独立看门狗进程（mirrorstar-watchdog 已在阶段2移除）

**关键澄清：** wp-proc **仅**处理 Web 壁纸。视频壁纸使用外部 `mpv.exe`，GIF/图片在主进程内渲染。这**不是**一个统一处理所有壁纸类型的子进程。

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
                    // 已有实例运行，提示用户并退出
                    MessageBoxW(
                        None,
                        windows::core::w!("镜星壁纸已在运行中。"),
                        windows::core::w!("提示"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                    return false;
                }
                // 互斥体句柄在进程生命周期内保持，进程退出时自动释放
                let _ = Box::leak(Box::new(_mutex));
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
- 互斥体句柄通过 `Box::leak` 在进程生命周期内保持，进程退出时系统自动释放
- 检测到已有实例时弹出 `MessageBoxW` 提示用户并退出

### 1.3 壁纸子进程架构

实际架构中存在**两种**按需启动的子进程，由 `ProcessManager`（`crates/mirrorstar-core/src/process/`，458 行）统一管理：`CreateProcessW` 启动 + 3 秒等待 + `TerminateProcess` 强制终止，提供 `is_running()` / `pid()` / `handle()` 状态查询。

#### mpv.exe（视频壁纸子进程）

- **启动方式**：由 `VideoRenderer`（`crates/mirrorstar-core/src/wallpaper/video.rs`，511 行）通过 `ProcessManager` spawn
- **控制方式**：`MpvIpcClient`（`crates/mirrorstar-core/src/ipc/protocol.rs`）通过 mpv 原生 JSON IPC 控制
- **命名管道**：`\\.\pipe\mirrorstar-mpv-{uuid}`（UUID 由主进程生成，通过 `--input-ipc-server` 参数传给 mpv）
- **窗口标题**：`MirrorStarVideo`（通过 `--title=MirrorStarVideo` 参数设置，用于 `FindWindowW` 查找）
- **find_mpv() 路径查找策略**：
  1. 优先检查捆绑路径 `<exe_dir>/mpv/mpv.exe`
  2. 回退到系统 PATH 中的 `mpv.exe`
- **启动参数**（部分）：`--idle=no`、`--loop-file`、`--hwdec=auto`、`--vo=gpu`、`--force-window=yes`、`--input-ipc-server=\\.\pipe\mirrorstar-mpv-{uuid}`、`--title=MirrorStarVideo`

#### mirrorstar-wp-proc.exe（Web 壁纸子进程）

- **启动方式**：由 `WebRenderer`（`crates/mirrorstar-core/src/wallpaper/web.rs`，255 行，代理层）通过 `ProcessManager` spawn
- **控制方式**：`WpProcIpcClient`（`crates/mirrorstar-core/src/ipc/wp_proc.rs`，385 行）通过自定义 `WpProcCommand` 协议控制
- **命名管道**：`\\.\pipe\{pipe_name}`（pipe_name 通过 `--pipe-name` CLI 参数传入）
- **代码规模**：1665 行（`crates/mirrorstar-wp-proc/`），使用 `webview2-com` crate
- **CLI 参数**：
  - `--source`：初始网页源（URL 或 file:// 路径）
  - `--pipe-name`：命名管道名称
  - `--title`：窗口标题（UUID，用于 `FindWindowW` 查找）
  - `--rect`：初始窗口位置和大小
- **实现要点**：
  - 命名管道服务端：`CreateNamedPipeW` + `ConnectNamedPipe` + `BufReader`/`BufWriter`
  - IPC 线程：读取 JSON 命令行 → 反序列化 `WpProcCommand` → mpsc 发送到主线程 → `PostMessageW` 唤醒 → 等待响应 → 序列化写回管道
  - WebView2 环境：`CreateCoreWebView2Environment` + `CreateCoreWebView2Controller` + URL 导航（支持 http/https 和 file://）
  - 窗口类注册：固定类名 `MirrorStarWebWallpaperCls`，窗口标题使用 `--title` 参数
  - 主消息循环：`GetMessageW` + `WM_WEB_COMMAND`（`WM_USER + 20`）处理
  - 命令处理：Play（导航到源）、Terminate（销毁窗口+退出）、SetPosition（`SetWindowPos` + `SetBounds`）、Navigate（`webview.Navigate`）、Pause/Resume（状态标记）

### 1.4 COM 初始化策略

MirrorStar **显式**初始化 COM，而非依赖 Tauri 内部初始化。两个进程均使用 STA（单线程单元）模式：

**主进程**（`src-tauri/src/lib.rs` 的 `run()`）：

```rust
// 在主线程初始化 COM 为 STA 模式，与 Tauri/tao 的要求一致。
// 必须在任何 COM 调用（如 VolumeControl::new）之前完成，
// 否则 Tauri 的 OleInitialize 会因 COM 已被初始化为 MTA 而失败。
unsafe {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    // S_FALSE 表示已初始化，这是正常的
    if hr.is_err() && hr != windows::core::HRESULT(0x80010106u32 as i32) {
        // RPC_E_CHANGED_MODE = 0x80010106，如果已初始化为其他模式则忽略
        hr.ok().expect("COM 初始化失败");
    }
}
```

**wp-proc 子进程**（`crates/mirrorstar-wp-proc/src/main.rs`）：

```rust
let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
```

**要点：**
- 主进程在 `run()` 开头、任何 COM 调用（如 `VolumeControl::new`）之前显式调用 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA）
- 处理 `RPC_E_CHANGED_MODE`（`0x80010106`）：如果已初始化为其他模式则忽略，避免因重复初始化导致 panic
- wp-proc 子进程同样调用 `CoInitializeEx(COINIT_APARTMENTTHREADED)`（STA），为 WebView2 提供正确的 COM 环境
- **不是**"依赖 Tauri 内部 COM 初始化"——这是显式初始化，确保在 `VolumeControl::new()` 缓存 COM 接口之前 COM 已就绪

### 1.5 启动时序

主进程 `run()`（`src-tauri/src/lib.rs`）的实际初始化顺序如下。**注意：无看门狗进程 spawn 步骤。**

1. **`init_logging()`** — tracing + tracing-subscriber + tracing-appender（按日滚动文件 + 标准输出双通道）
2. **COM 初始化（STA）** — `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`，必须在任何 COM 调用之前完成，处理 `RPC_E_CHANGED_MODE`
3. **`DesktopIntegrator::new()`** — 保存原始系统壁纸，WorkerW 延迟初始化（此时不查找 WorkerW）
4. **后台线程 `mirrorstar-workerw-init`** — `std::thread::Builder::spawn` 预初始化 WorkerW，调用 `ensure_initialized()`，失败时记录警告并在首次使用时重试
5. **`VolumeControl::new()`** — WASAPI 音量控制，缓存 COM 接口（`IMMDeviceEnumerator` + `IAudioSessionManager2`）
6. **`WallpaperEngine::new()`** — 创建壁纸引擎，传入 desktop 和 volume_control
7. **`pause_senders` HashMap** — 初始化 `Arc<RwLock<HashMap<String, PauseSender>>>` 快速通道映射
8. **`ConfigManager::new()` + `start_watching()`** — 配置管理器初始化 + 启动 notify crate 文件监视（热重载，500ms 防抖）
9. **`start_fullscreen_monitor()`** — `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 事件驱动全屏检测 + AtomicBool 状态去抖 + 自身窗口排除
10. **`WM_POWERBROADCAST` 事件驱动** — 在 explorer 监控窗口的 wndproc 中处理 `PBT_APMPOWERSTATUSCHANGE` 消息，事件触发时一次性调用 `GetSystemPowerStatus` 读取状态（非轮询），电池供电时暂停壁纸
11. **`start_explorer_restart_monitor()`** — 监听 `TaskbarCreated` 消息（事件驱动，Explorer 重启后重建 WorkerW）；**独立函数 `start_workerw_check()`** — 5 分钟轮询兜底（`is_workerw_valid()` + `check_and_reinitialize()`）

**关键说明：**
- 启动序列中**无**看门狗进程 spawn 步骤（`mirrorstar-watchdog` crate 已在阶段2移除）
- 主进程崩溃时操作系统自动回收子进程（父子进程关系），不需要独立看门狗进程清理资源
- 主窗口懒创建：启动时仅创建系统托盘图标，主窗口在用户点击托盘"打开主窗口"时通过 `WebviewWindowBuilder` 动态创建，关闭即隐藏（hide）以保留 WebView2 实例

---

## 2. IPC 通信设计

### 2.1 两套独立 IPC 协议

MirrorStar 使用**两套完全独立**的 IPC 协议，分别对应两种子进程。两者均采用命名管道 + JSON + 换行分隔格式，并复用相同的连接重试 + request_id 响应匹配模式。

#### 协议一：mpv 原生 IPC（视频壁纸）

- **客户端**：`MpvIpcClient`（`crates/mirrorstar-core/src/ipc/protocol.rs`，246 行）
- **管道**：`\\.\pipe\mirrorstar-mpv-{uuid}`（UUID 由主进程生成，通过 mpv `--input-ipc-server` 参数创建）
- **格式**：mpv 原生 JSON 协议（JSON 行，request_id 匹配响应）
- **连接重试**：5 次尝试 × 200ms 间隔（`ipc.connect(MPV_CONNECT_RETRIES, MPV_CONNECT_INTERVAL_MS) = 1s`）
- **命令**：`pause` / `resume` / `set_volume` / `set_loop_file` / `set_speed` / `quit` / `get_property` / `set_property`
- **响应结构**：`{ error, data?, request_id }`（mpv 原生格式）

#### 协议二：wp-proc WpProcCommand（Web 壁纸）

- **客户端**：`WpProcIpcClient`（`crates/mirrorstar-core/src/ipc/wp_proc.rs`，385 行）
- **管道**：`\\.\pipe\{pipe_name}`（pipe_name 通过 `--pipe-name` CLI 参数传入子进程）
- **格式**：自定义 JSON + 换行分隔，request_id 匹配响应
- **连接重试**：100 次尝试 × 200ms 间隔（`ipc.connect(WP_PROC_CONNECT_RETRIES, WP_PROC_CONNECT_INTERVAL_MS) = 20s`，覆盖 WebView2 冷启动）
- **命令**：`Play{source}` / `Terminate` / `SetPosition{x,y,w,h}` / `Navigate{url}` / `Pause` / `Resume`

**命令示例**（主进程 → 子进程）：

```json
{"command":"play","request_id":1,"source":"https://..."}
{"command":"terminate","request_id":2}
{"command":"set_position","request_id":3,"x":0,"y":0,"width":1920,"height":1080}
{"command":"navigate","request_id":4,"url":"https://..."}
{"command":"pause","request_id":5}
{"command":"resume","request_id":6}
```

**响应示例**（子进程 → 主进程）：

```json
{"request_id":1,"status":"ok"}
{"request_id":1,"status":"error","error":"WebView2 未初始化"}
```

**关键澄清：** 这是**两套独立的协议**，不是一个统一协议。mpv 使用 mpv 原生 JSON 协议，wp-proc 使用自定义 `WpProcCommand` 协议。两者只是复用了相同的连接重试 + request_id 响应匹配设计模式。

### 2.2 HWND 获取方式

两种子进程的窗口 HWND 均在主进程通过 `FindWindowW` 获取，然后由 `DesktopIntegrator` 执行 WorkerW 嵌入。**HWND 不通过 IPC 回传。**

**mpv（视频壁纸）**：
- 通过窗口标题 `MirrorStarVideo` 查找（`FindWindowW`）
- 重试：20 次 × 100ms = 2 秒
- PID 验证：`GetWindowThreadProcessId` 确认窗口属于 mpv 子进程
- 实现位置：`VideoRenderer::find_mpv_window()`

**wp-proc（Web 壁纸）**：
- 通过 `--title` 参数传入的 UUID 标题查找（`FindWindowW`）
- 重试：20 次 × 100ms = 2 秒
- PID 验证：`GetWindowThreadProcessId` 确认窗口属于 wp-proc 子进程
- 实现位置：`WebRenderer` 中对应查找逻辑

**嵌入流程**：HWND 获取后，主进程的 `DesktopIntegrator` 执行 WorkerW 嵌入（`SetParent` + `SetWindowPos(HWND_BOTTOM)` + `ShowWindow`），与 mpv 窗口嵌入逻辑一致。

---

## 3. Lively 进程模型对比

### MirrorStar 进程模型（混合架构：主进程 + 按需 Web 子进程）

```
┌──────────────────────────────────────────────────────────┐
│              mirrorstar-wallpaper.exe（主进程）             │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
│  │ Tauri    │ │ 图片线程  │ │ GIF 线程  │ │ 视频线程   │  │
│  │ 主线程   │ │ (GetMsg) │ │ (Timer)  │ │ (mpv IPC) │  │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────────────┐  │
│  │ 全屏检测 │ │ 配置监视 │ │ WebRenderer (代理层)     │  │
│  │ (Hook)   │ │ (notify) │ │  ├ ProcessManager       │  │
│  └──────────┘ └──────────┘ │  └ WpProcIpcClient       │  │
│                              └──────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
       │                              │
       │ CreateProcessW               │ CreateProcessW + 命名管道 IPC
       ▼                              ▼
┌─────────────────┐         ┌──────────────────────────┐
│   mpv.exe       │         │ mirrorstar-wp-proc.exe    │
│   (视频壁纸)    │         │ (Web 壁纸子进程)          │
│   按需启动      │         │  ┌────────────────────┐  │
└─────────────────┘         │  │ WebView2 环境      │  │
                            │  │  └ 窗口 + 消息循环  │  │
                            │  └────────────────────┘  │
                            └──────────────────────────┘
                               ↑ 仅 Web 壁纸时启动
                               ↑ 关闭 Web 壁纸时终止
```

**特点：**
- Image/Gif 渲染器在主进程专用线程中运行（GDI + 双缓冲）
- Video 渲染器通过 ProcessManager 启动 mpv.exe 子进程（按需）
- **Web 渲染器已重构为代理层**：通过 ProcessManager 启动 mirrorstar-wp-proc.exe 子进程（按需），通过 WpProcIpcClient 命名管道通信
- HWND 通过 FindWindowW + PID 验证获取后，主进程执行 WorkerW 嵌入（与 mpv 窗口嵌入逻辑一致）
- 线程间通过 mpsc 通道 + PostMessageW 通信
- 全屏检测使用 SetWinEventHook 事件驱动
- PauseSender 快速通道绕过引擎互斥锁
- watchdog crate 已在阶段2移除（不再需要独立看门狗进程）

**内存优化：**
- 无 Web 壁纸时：零子进程开销（仅主进程 + 可选的 mpv）
- WebView2 运行时仅加载在子进程中，主进程不加载 webview2-com
- 子进程在 Web 壁纸关闭时立即终止，释放全部内存

### Lively 进程模型

```
┌─────────────────────────────────────────────────┐
│              livelywpf.exe（主进程）              │
│  ┌──────────┐ ┌──────────┐ ┌──────────────────┐ │
│  │ WPF      │ │ 定时器   │ │ CefSharp stdout  │ │
│  │ UI 线程  │ │ (轮询)   │ │ 消息处理         │ │
│  └──────────┘ └──────────┘ └──────────────────┘ │
└─────────────────────────────────────────────────┘
    │                    │                │
    │ SetParent()        │ WaitForExit()  │ stdin/stdout
    ▼                    ▼                ▼
┌────────┐  ┌────────────────┐  ┌─────────────────┐
│WPF窗口 │  │livelySubProcess│  │LivelyCefSharp   │
│(内嵌)  │  │(看门狗进程)    │  │(CefSharp子进程) │
└────────┘  └────────────────┘  └─────────────────┘
    │
    │ SetParent() + SuspendThread/ResumeThread
    ▼
┌────────────────┐
│ 外部进程        │  ← Unity/Godot/mpv 等
│ (mpv/Unity等)  │
└────────────────┘
```

**特点：**
- 所有壁纸窗口都是 WPF Window（内嵌在主进程）或外部进程
- CefSharp 运行在独立子进程，通过 stdin/stdout 通信
- livelySubProcess 看门狗进程监控主进程，崩溃时清理资源
- 外部进程通过 SuspendThread/ResumeThread 暂停/恢复

### 对比总结

| 维度 | MirrorStar | Lively |
|------|-----------|--------|
| 壁纸渲染位置 | 主进程专用线程（Image/Gif/Video）+ 独立子进程（Web） | 主进程 WPF 窗口 + 外部进程 |
| 子进程数量 | 0-2（mpv + wp-proc，均按需启动） | 2+（CefSharp + 看门狗 + 外部程序） |
| 崩溃隔离 | Web 壁纸崩溃隔离（子进程），Image/Gif/Video 在主进程 | 子进程崩溃不影响主进程 + 看门狗进程 |
| 看门狗机制 | 无独立看门狗进程（watchdog crate 已在阶段2移除） | 有（livelySubProcess 完整实现） |
| 暂停机制 | 逻辑暂停（PauseSender 快速通道） | 线程挂起（SuspendThread） |
| 内存优化 | 无 Web 壁纸时零子进程开销，WebView2 仅按需加载 | CefSharp 启动即加载 |

**关键差异：**
- MirrorStar 采用混合进程架构——Image/Gif/Video 渲染器在主进程专用线程中运行，仅 Web 壁纸通过独立子进程（mirrorstar-wp-proc）渲染，按需启动。Video 壁纸也启动 mpv 子进程（按需）。watchdog crate 已在阶段2移除（不再需要独立看门狗进程）。
- Lively 使用 `SuspendThread/ResumeThread` 挂起外部进程的所有线程来实现暂停，这是一种粗暴但有效的方式。MirrorStar 则通过 PauseSender 快速通道绕过引擎互斥锁，直接发送暂停/恢复/音量命令，实现优雅暂停。
- MirrorStar 的混合架构在内存占用上更优：无 Web 壁纸时仅主进程运行（+ 可选的 mpv），而 Lively 的 CefSharp 子进程启动即加载。

---

**相关文档：**
- [模块设计](./module-design.md)
- [依赖图与数据流](./dependency-graph.md)
- [暂停/恢复机制](./pause-resume.md)
- [错误处理策略](./error-handling.md)
