# MirrorStar Wallpaper（镜星壁纸）技术栈 — 壁纸渲染

[← 返回文档索引](../README.md) > [技术栈](./overview.md) > 壁纸渲染

## 4. 静态图片壁纸

### 选择：Native API（SystemParametersInfoW）+ WorkerW 回退

**选择理由：**

- **零资源占用**：JPG/JPEG/PNG/BMP/TIF/TIFF/DIB 使用 Windows 原生壁纸 API，无需创建渲染窗口、无需持续 GPU/CPU 开销
- **系统原生体验**：通过 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` 设置壁纸，与 Windows 系统设置完全一致
- **注册表缩放**：通过 `WallPaperStyle` / `TileWallpaper` 注册表键值控制缩放模式，精确可靠
- **WebP 回退**：WebP 格式不受原生 API 支持，自动回退到 image crate + WorkerW 嵌入方案

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **image crate + GDI 双缓冲/WorkerW** | 统一渲染路径 | 持续占用资源（窗口、GPU 上下文），静态图片无需动画 | ⚠️ 回退方案 |
| **SystemParametersInfoW** ✅ | 零资源，系统原生，~10ms 完成 | 不支持 WebP 格式 | ✅ 最佳选择 |

### 设计方案

```
文件格式检测 → is_native_supported()?
  ├─ Yes (JPG/JPEG/PNG/BMP/TIF/TIFF/DIB) → set_native_wallpaper(): 注册表写入缩放模式 + SystemParametersInfoW 设置壁纸
  └─ No  (WebP)            → image crate 解码 → GDI 双缓冲渲染 → WorkerW 嵌入
```

#### 核心函数

- **`is_native_supported()`** — 检测文件扩展名是否支持原生壁纸 API（JPG/JPEG/PNG/BMP/TIF/TIFF/DIB，共 7 种）
- **`set_native_wallpaper()`** — 先写入注册表缩放模式，再调用 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` 设置壁纸
- **`clear_native_wallpaper()`** — 调用 `SystemParametersInfoW` 传入空路径清除壁纸

#### 缩放模式映射

| ScalingMode | WallPaperStyle | TileWallpaper | 效果 |
|-------------|---------------|---------------|------|
| Center | 0 | 0 | 居中显示 |
| Stretch | 2 | 0 | 拉伸填充 |
| Fit | 6 | 0 | 等比适应 |
| Fill | 10 | 0 | 等比裁切填充 |
| Original | 0 | 0 | 原始尺寸（同 Center） |

> **注意**：注册表路径为 `HKEY_CURRENT_USER\Control Panel\Desktop`，`WallPaperStyle` 和 `TileWallpaper` 均为 `REG_SZ` 类型。使用 `winreg` crate 进行安全的注册表读写操作。

---

## 5. 视频播放

### 选择：外部 mpv.exe 子进程

**选择理由：**

- **轻量高效**：mpv 是最轻量的全功能播放器，内存占用低，CPU 占用小
- **硬件加速**：通过 D3D11VA / DXVA2 自动启用 GPU 硬件解码，4K 视频流畅播放
- **全格式支持**：基于 FFmpeg，支持 MP4、MKV、WebM、AVI 等所有常见格式
- **生产验证**：mpv 被广泛用于各类播放器项目（IINA、Celluloid 等），稳定性极高
- **进程隔离**：以独立子进程方式运行，崩溃不影响主应用；通过命名管道 JSON IPC 控制
- **无 FFI 依赖**：不嵌入 libmpv2 / mpv-2.dll，避免 FFI 绑定与 DLL 版本耦合问题

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **DirectShow** | Windows 原生 | 已过时，格式支持有限，API 复杂 | ❌ 过时 |
| **MediaFoundation** | Windows 原生，硬件加速 | 格式支持有限（无 MKV/WebM），API 繁琐 | ❌ 格式受限 |
| **ffmpeg 直调** | 最灵活 | 过于底层，需自行处理渲染/同步/音频，开发量大 | ❌ 过于底层 |
| **libmpv2 FFI 嵌入** | 进程内调用 | 需捆绑 mpv-2.dll，FFI 绑定与版本耦合，崩溃影响主进程 | ❌ 耦合风险 |
| **外部 mpv.exe 子进程** ✅ | 轻量，全格式，硬件加速，进程隔离，无 FFI 依赖 | 需确保 mpv.exe 在 PATH 或捆绑分发 | ✅ 最佳选择 |

### 集成方式

```
ProcessManager → 启动 mpv.exe 子进程（带 IPC 管道参数）→ MpvIpcClient 通过命名管道 JSON IPC 控制 → FindWindowW 获取窗口 HWND → SetParent 嵌入 WorkerW
```

- mpv.exe 作为独立子进程运行，窗口嵌入 WorkerW
- 通过 `--vo=gpu` + `--hwdec=auto` 启用硬件加速
- 通过 `--loop-file` 实现循环播放
- 通过 `--input-ipc-server` 暴露命名管道，由 `MpvIpcClient` 发送 mpv 原生 JSON 协议命令控制

### mpv.exe 启动参数

`ProcessManager` 使用以下参数启动 mpv.exe：

```
mpv.exe <video_path> \
  --idle=no \
  --loop-file \
  --hwdec=auto \
  --vo=gpu \
  --no-osc \
  --title=MirrorStarVideo \
  --force-window=yes \
  --input-ipc-server=\\.\pipe\mirrorstar-mpv-{uuid} \
  <scaling-mode-args>
```

- `--title=MirrorStarVideo`：固定窗口标题，用于后续 `FindWindowW` 查找
- `--input-ipc-server`：指定命名管道路径，主进程通过该管道与 mpv 通信
- 缩放模式参数根据 `ScalingMode` 动态追加

### mpv.exe 查找：find_mpv()

`find_mpv()` 按以下顺序查找 mpv.exe：

1. **捆绑路径优先**：`<exe_dir>/mpv/mpv.exe`（随应用分发的捆绑版本）
2. **PATH 回退**：在系统 PATH 环境变量中查找 `mpv.exe`

若两处均未找到，则返回错误，提示用户安装 mpv 或将其加入 PATH。

### 窗口查找：find_mpv_window(pid)

获取 mpv 渲染窗口 HWND 的流程：

1. `FindWindowW` 按窗口标题 `"MirrorStarVideo"` 查找
2. 重试机制：20 × 100ms（总计 2s），等待 mpv 创建窗口
3. **PID 校验**：通过 `GetWindowThreadProcessId` 验证窗口所属进程与 mpv.exe 子进程 PID 一致，避免误匹配同名窗口

### 音量控制：WASAPI（非 mpv IPC）

> **重要**：音量控制通过 WASAPI（`VolumeControl`）实现，**不使用** mpv 的 IPC `set_volume` 命令。`set_volume` 仅调用 `VolumeControl`，对 mpv 子进程的会话音量进行系统级控制。

- `VolumeControl` 基于 Core Audio API（`ISimpleAudioVolume`），按 PID 定位 mpv 进程的音频会话
- 优势：与系统音量混合器统一，避免 mpv 内部音量与系统音量不同步

### VideoRenderer

- **VideoRenderer** 共 555 行，负责视频壁纸的完整生命周期管理
- 构造函数：`VideoRenderer::new(volume_control: Option<Arc<Mutex<VolumeControl>>>)`
  - `volume_control` 为 `Option` 类型，允许在无音量控制需求时传入 `None`
  - 音量控制通过 `Arc<Mutex<VolumeControl>>` 共享，支持跨线程安全访问

**超时参数：**

| 参数 | 当前值 | 原值 | 说明 |
|------|--------|------|------|
| IPC 重试 | 5 × 200ms | 10 × 500ms | IPC 通道连接超时，总计 1s |
| 窗口查找 | 20 × 100ms | 50 × 100ms | mpv 窗口查找超时，总计 2s |

### 依赖管理

- **mpv.exe**：需在 PATH 中或捆绑于 `<exe_dir>/mpv/` 目录
- `find_mpv()` 支持上述两种查找方式
- 无 Rust crate 依赖（不使用 libmpv2 / mpv-2.dll）

---

## 6. GIF 播放

### 选择：image crate (GifDecoder) + GDI 双缓冲

**选择理由：**

- **纯 Rust**：无外部 C 依赖，编译简单，跨平台潜力
- **低内存**：GIF 逐帧解码，无需一次性加载全部帧到内存
- **轻量**：对于 GIF 这种简单格式，无需引入 mpv 的重量级依赖
- **GDI 双缓冲**：Win32 原生渲染到窗口，无需 GPU 上下文

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **mpv 播放 GIF** | 统一播放器 | 对简单 GIF 过于重量，额外内存开销 | ⚠️ 备选 |
| **XamlAnimatedGif** (Lively) | WPF 集成 | C# 专属，Rust 不可用 | ❌ 不可用 |
| **image crate + GDI 双缓冲** ✅ | 纯 Rust，无外部依赖，低内存 | 需自行实现帧定时 | ✅ 最佳选择 |

### 设计方案

```
image crate (GifDecoder 解码) → 帧缓冲区 → GDI 双缓冲 (渲染到 HWND) → 精确定时器 (帧率控制)
```

1. 使用 `image::codecs::gif::GifDecoder` 逐帧解码 GIF
2. 将解码后的帧数据通过 GDI 设备上下文渲染到窗口
3. 使用 `std::time::Instant` 实现精确帧定时
4. 支持循环播放、暂停/恢复

### 内存管理

- **帧缓冲预算**：40MB（原 200MB），严格控制内存占用
- **暂停帧释放**：暂停时释放除当前帧外的所有已解码帧，内存占用降至单帧级别
- **恢复重新解码**：恢复播放时从文件重新解码帧数据，避免长期持有大量帧缓冲

---

## 7. 网页壁纸

### 选择：WebView2（webview2-com 0.34 crate，子进程创建）

**选择理由：**

- **硬件加速**：Chromium 渲染引擎，GPU 合成，流畅渲染复杂网页
- **现代引擎**：基于 Chromium，支持 ES2024+、CSS3、WebGL、WebAssembly
- **安全沙箱**：WebView2 运行在独立进程中，崩溃不影响主应用
- **进程隔离**：WebView2 环境在 `mirrorstar-wp-proc.exe` 子进程中创建，主进程仅作代理层

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **CefSharp** (Lively 的选择) | 完整 Chromium 控制 | 需捆绑 CEF 二进制（~200MB），极重 | ❌ 过于庞大 |
| **Electron 嵌入** | 完整 Node.js 环境 | 体积巨大（~150MB），内存占用高 | ❌ 过于庞大 |
| **Tauri 内置 WebView2** | 已随 Tauri 包含 | 与主窗口共享 WebView2 运行时，壁纸生命周期管理受限 | ⚠️ 不适用壁纸 |
| **webview2-com 0.34（子进程）** ✅ | 系统预装，硬件加速，轻量，进程隔离 | 需 WebView2 Runtime（Win10/11 已预装） | ✅ 最佳选择 |

### 设计方案

```
WebRenderer（代理层，307 行）→ 启动 mirrorstar-wp-proc.exe 子进程 → 子进程用 webview2-com 创建 WebView2 环境 → FindWindowW + PID 校验获取 HWND → SetParent 嵌入 WorkerW → WpProcIpcClient 通过命名管道控制
```

1. `WebRenderer`（307 行）是代理层，负责启动并管理 `mirrorstar-wp-proc.exe` 子进程
2. **WebView2 环境在子进程中创建**（非主进程），使用 `webview2-com 0.34` crate
3. 子进程创建渲染窗口后，主进程通过 `FindWindowW` + **PID 校验**获取窗口 HWND
4. 使用 `SetParent` 将窗口嵌入 WorkerW
5. `WpProcIpcClient` 通过命名管道与子进程通信，使用 `WpProcCommand` JSON 协议控制网页壁纸（URL 加载、刷新、暂停、定位）

### WebView2 懒创建

- **按需创建**：网页壁纸启用时才启动 `mirrorstar-wp-proc.exe` 子进程并创建 WebView2 环境，而非启动时预创建
- **关闭即销毁**：壁纸关闭时终止子进程，释放所有资源
- **优势**：避免闲置时占用 WebView2 运行时资源（约 50-100MB 内存），仅在活跃壁纸期间分配

---

## 8. WallpaperMode 双路径选择

### WallpaperMode 枚举

```rust
enum WallpaperMode {
    Native,   // SystemParametersInfoW 原生壁纸 API
    WorkerW,  // WorkerW 窗口嵌入渲染
}
```

### 选择逻辑

| 壁纸类型 | WallpaperMode | 渲染路径 | 资源占用 |
|----------|--------------|----------|----------|
| JPG/JPEG/PNG/BMP/TIF/TIFF/DIB | `Native` | `SystemParametersInfoW` + 注册表 | 零运行时资源 |
| WebP | `WorkerW` | image crate + GDI 双缓冲 + WorkerW | 窗口 + GPU 上下文 |
| GIF | `WorkerW` | image crate + GDI 双缓冲 + WorkerW | 窗口 + 帧缓冲 |
| 视频 | `WorkerW` | mpv.exe 子进程 + WorkerW | 窗口 + 解码器 |
| 网页 | `WorkerW` | WebView2 + WorkerW | 窗口 + 浏览器运行时 |

### 设计原则

- **Native 优先**：对于原生支持的静态图片格式，优先使用 `WallpaperMode::Native`，实现零资源占用
- **WorkerW 回退**：对于 WebP 等不受原生 API 支持的格式，以及所有动态壁纸类型，使用 `WallpaperMode::WorkerW` 嵌入渲染
- **统一切换**：从 `Native` 切换到 `WorkerW` 时，需先调用 `clear_native_wallpaper()` 清除原生壁纸，再启动 WorkerW 渲染；反之亦然

---

**相关章节：** [← 总览](./overview.md) | [Windows API 绑定](./windows-api.md) | [基础设施](./infrastructure.md) | [风险评估](./risk-assessment.md)
