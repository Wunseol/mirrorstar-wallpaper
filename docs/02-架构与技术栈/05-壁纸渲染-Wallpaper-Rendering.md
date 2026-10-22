[← 返回文档索引](../index.md) > 架构与技术栈 > 壁纸渲染

# MirrorStar Wallpaper（镜星壁纸）— 壁纸渲染

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.0                      |
| 更新日期 | 2026-10-11                |
| 文档状态 | 已实现（已按真实代码合并重组，2026-10）        |

> 本文由原《壁纸渲染》与《桌面集成》的渲染路径章节（Native / WorkerW 双路径）合并去重而成。**Native/WorkerW 渲染路径的选择与对比、原生壁纸 API 实现**统一收敛至本文，其它文档不再重复展开。WorkerW 嵌入的机制细节（SetParent / 坐标转换 / DPI / Explorer 重启等）见[桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md)。

---

## 1. 渲染路径选择（Native vs WorkerW）

对于每种壁纸类型，系统在两条路径中择一：

| 壁纸类型 | 渲染路径 | 资源占用 |
|----------|----------|----------|
| JPG / JPEG / PNG / BMP / TIF / TIFF / DIB | 原生壁纸 API（`SystemParametersInfoW` + 注册表缩放） | 零运行时资源 |
| WebP | image crate 解码 + WorkerW 嵌入（GDI 双缓冲） | 窗口 + GPU 上下文 |
| GIF | image crate 解码 + WorkerW 嵌入 | 窗口 + 帧缓冲 |
| 视频 | mpv.exe 子进程 + WorkerW 嵌入 | 窗口 + 解码器 |
| 网页 | WebView2（wp-proc 子进程）+ WorkerW 嵌入 | 窗口 + 浏览器运行时 |

**设计原则**：静态图片优先走 Native 路径（零资源）；WebP 及所有动态壁纸走 WorkerW 嵌入路径。从 Native 切换到 WorkerW（或反之）时，先调用 `clear_native_wallpaper()` 清除原生壁纸，再启动对应渲染。

### 1.1 WallpaperMode 双路径判定

```rust
/// 判断文件格式是否支持原生壁纸 API
pub fn is_native_supported(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "bmp" | "tif" | "tiff" | "dib")
}
```

> 实测自 `crates/mirrorstar-core/src/desktop/native_wallpaper.rs`，`NATIVE_SUPPORTED_EXTENSIONS = &["jpg", "jpeg", "png", "bmp", "tif", "tiff", "dib"]`。判定大小写不敏感。

| 文件格式 | WallpaperMode | 说明 |
|----------|---------------|------|
| jpg/jpeg/png/bmp/tif/tiff/dib | `Native` | 使用 `SystemParametersInfoW` + 注册表，零资源 |
| 其余（WebP/GIF/视频/网页等） | `WorkerW` | 不支持原生 API，回退到 GDI 双缓冲/子进程 + WorkerW 嵌入 |

### 1.2 原生模式 vs WorkerW 模式对比

| 维度 | Native 模式 | WorkerW 模式 |
|------|-------------|--------------|
| 窗口 | 无 | 需创建无边框窗口 |
| 线程 | 无 | 需渲染线程 |
| GDI 对象 | 无 | GDI 双缓冲表面 |
| 内存占用 | ~0MB | ~10-30MB |
| 暂停/恢复 | 空操作（no-op） | 需控制渲染循环 |
| 缩放模式 | 注册表 `WallPaperStyle`/`TileWallpaper` | GDI 双缓冲缩放算法 |
| 支持格式 | jpg/jpeg/png/bmp/tif/tiff/dib | 所有格式（含 WebP） |

---

## 2. 静态图片壁纸

### 选择：Native API（`SystemParametersInfoW`）+ WorkerW 回退

- **`is_native_supported()`**：检测扩展名是否受原生壁纸 API 支持（JPG/JPEG/PNG/BMP/TIF/TIFF/DIB 共 7 种；WebP 不受支持）
- **`set_native_wallpaper()`**：先写入注册表缩放模式，再调用 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` 设置壁纸，约 10ms 完成、零运行时资源
- **WebP 回退**：执行 image crate 解码 + GDI 双缓冲 + WorkerW 嵌入

### 2.1 SystemParametersInfoW 方案

```rust
/// 使用原生 API 设置静态壁纸
fn set_native_wallpaper(file_path: &str, scaling_mode: ScalingMode) -> Result<()> {
    // 1. 设置注册表缩放模式
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktop_key = hkcu.open_subkey_with_flags(
        r"Control Panel\Desktop",
        KEY_SET_VALUE,
    )?;

    match scaling_mode {
        ScalingMode::Fill => {
            desktop_key.set_value("WallPaperStyle", &"10")?;  // 填充
            desktop_key.set_value("TileWallpaper", &"0")?;
        }
        ScalingMode::Stretch => {
            desktop_key.set_value("WallPaperStyle", &"2")?;   // 拉伸
            desktop_key.set_value("TileWallpaper", &"0")?;
        }
        ScalingMode::Fit => {
            desktop_key.set_value("WallPaperStyle", &"6")?;   // 适配
            desktop_key.set_value("TileWallpaper", &"0")?;
        }
        ScalingMode::Center => {
            desktop_key.set_value("WallPaperStyle", &"0")?;   // 居中
            desktop_key.set_value("TileWallpaper", &"0")?;
        }
        ScalingMode::Original => {
            desktop_key.set_value("WallPaperStyle", &"0")?;   // 原始大小（同居中）
            desktop_key.set_value("TileWallpaper", &"0")?;
        }
    }

    // 2. 调用 SystemParametersInfoW 设置壁纸
    let wide_path = encode_wide(file_path);
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide_path.as_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )?;
    }

    Ok(())
}
```

### 2.2 缩放模式 → 注册表映射

注册表路径 `HKEY_CURRENT_USER\Control Panel\Desktop`，`ScalingMode` 枚举（`Fill`/`Fit`/`Stretch`/`Center`/`Original`）映射：

| ScalingMode | WallPaperStyle | TileWallpaper | 效果 |
|-------------|---------------|---------------|------|
| Center | 0 | 0 | 居中显示 |
| Stretch | 2 | 0 | 拉伸填充 |
| Fit | 6 | 0 | 等比适应 |
| Fill | 10 | 0 | 等比裁切填充 |
| Original | 0 | 0 | 原始尺寸（等同 Center） |

***

## 3. 视频播放（mpv.exe 子进程）

### 选择：外部 mpv.exe 子进程

- **轻量高效 + 硬件加速**：`--hwdec=auto`，4K 视频流畅播放
- **全格式支持**：基于 FFmpeg，支持 mp4/mkv/webm/avi 等常见格式
- **进程隔离**：独立子进程，崩溃不影响主应用；通过命名管道 JSON IPC 控制
- **无 FFI 依赖**：不嵌入 libmpv2 / mpv-2.dll，避免 FFI 绑定与 DLL 版本耦合

### 集成方式

```
VideoRenderer → 启动 mpv.exe（--idle=yes + IPC 管道） → MpvIpcClient 通过命名管道 JSON IPC 加载视频 → 查找窗口 HWND + PID 校验 → SetParent 嵌入 WorkerW
```

### 启动策略：先 idle 后 loadfile（避免黑屏竞态）

mpv **不以视频路径作为命令行参数**启动，而是先 `--idle=yes` 启动空窗口，稳定嵌入 WorkerW 后通过 IPC `loadfile` 加载视频。这避免了"启动即解码 + 窗口重父化 resize"时 D3D11 纹理创建的竞态（曾导致桌面黑屏）。

### mpv 启动参数（实测自 video.rs::build_mpv_args）

```
mpv.exe --idle=yes \
  --no-input-default-bindings \
  --loop-file \
  --hwdec=auto \
  --vo=gpu \
  --input-vo-keyboard=no \
  --window-dragging=no \
  --no-osc \
  --no-osd-bar \
  --title=MirrorStarVideo \
  --input-media-keys=no \
  --no-input-terminal \
  --no-terminal \
  --keep-open=no \
  --force-window=yes \
  --input-ipc-server=\\.\pipe\mirrorstar-mpv-{uuid} \
  --cache=no \
  --vd-queue-max-bytes=16777216 \
  --ad-queue-max-bytes=4194304 \
  --demuxer-max-back-bytes=0 \
  --cache-secs=0 \
  --d3d11-flip=no \
  --log-file=<data_root>/logs/mpv-<suffix>.log
```

要点：

- `--idle=yes`：初始不加载文件（配合 IPC `loadfile`，见上方竞态说明）
- `--loop-file`：循环播放；`--hwdec=auto` + `--vo=gpu`：硬件加速
- `--input-ipc-server`：暴露命名管道，供 `MpvIpcClient` 发送 mpv 原生 JSON 协议命令
- `--window-dragging=no`：禁用 mpv 自带的窗口拖动；否则用户在视频区域按下并拖动会进入 Win32 模态移动循环，导致 mpv 命令处理停摆、退出延迟（纯净模式退出时临时覆盖窗仍置顶，表现为"壁纸可被拖动"）
- **内存优化（5 个参数）**：`--cache=no`（禁用 demuxer 缓存，节省 ~75MB）、`--vd-queue-max-bytes=16777216`（视频解码队列上限 16MB）、`--ad-queue-max-bytes=4194304`（音频解码队列上限 4MB）、`--demuxer-max-back-bytes=0`（禁用后退缓冲）、`--cache-secs=0`（禁用时间缓存）
  > 注意：勿加 `--demuxer-max-bytes=0`，该参数会导致 demuxer 无法缓冲而立即 EOF 退出（桌面黑屏）。
- `--d3d11-flip=no`：强制 bitblt 呈现模式，与 SetParent 重父化兼容，降低嵌入后 swapchain 重配置失败概率（双保险）
- `--log-file`：mpv 日志写入数据根 `logs/mpv-<uuid>.log`，v5.2 引入用于崩溃诊断

### mpv.exe 查找：find_mpv()

`find_mpv()` 委托 `SubprocessRendererBase::find_bundled_executable(Some("mpv"), "mpv.exe", "mpv")`，查找顺序：

1. **捆绑路径优先**：随应用分发（资源目录下的 mpv）
2. **PATH 回退**：在系统 PATH 中查找 `mpv.exe`

两处均未找到则返回错误，提示用户安装 mpv 或加入 PATH。

### 窗口查找：find_mpv_window(pid)

- `FindWindowW` 按标题 `"MirrorStarVideo"` 查找
- **重试**：20 × 100ms（总 2s）等待 mpv 创建窗口
- **PID 校验**：`GetWindowThreadProcessId` 验证窗口所属进程与 mpv 子进程 PID 一致，避免误匹配同名窗口

### 音量控制：WASAPI（非 mpv IPC）

音量控制通过 Core Audio WASAPI（`VolumeControl`，`ISimpleAudioVolume`）按 PID 定位 mpv 音频会话实现，**不使用** mpv IPC 音量命令，与系统音量混合器统一。

### 超时 / 重试参数

mpv / wp-proc 的 IPC 连接重试与窗口查找重试参数（`MPV_CONNECT_RETRIES` / `WP_PROC_CONNECT_RETRIES` / `WINDOW_FIND_RETRIES`）统一权威定义与完整参数表见[进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)§4.3，本文不再重复列出。

### VideoRenderer（video.rs）

- 位于 `crates/mirrorstar-core/src/wallpaper/video.rs`，负责视频壁纸完整生命周期（行数以实测为准）
- 构造函数接收 `file_path`、`ScalingMode`、`Option<Arc<Mutex<VolumeControl>>>`
- 管道名：`mirrorstar-mpv-{uuid}`（每个实例独立命名管道）

***

## 4. GIF 播放

### 选择：image crate (GifDecoder) + GDI 双缓冲

### 设计方案

```
image crate 解码 → 帧缓冲 → GDI 双缓冲渲染到 HWND → 精确帧定时
```

使用 `image::codecs::gif::GifDecoder` 逐帧解码；`image 0.25` 以 `default-features=false` 仅启用实际格式（png/jpeg/gif/bmp/webp/tiff + rayon）。

### 内存管理（GifMemoryStrategy）

位于 `mirrorstar-core/src/wallpaper/gif_memory.rs`，按暂停时的内存/恢复权衡分四档：

| 策略 | 行为 | 内存占用 | 恢复速度 |
|------|------|----------|----------|
| `Aggressive` | 暂停时释放所有帧，仅保留当前帧 | 最低 | 慢（需重新解码） |
| `Balanced`（默认） | 暂停时保留最近 N 帧（默认 10） | 折中 | 快 |
| `Performance` | 暂停时保留所有帧 | 最高 | 最快 |
| `Adaptive` | 根据系统可用内存 / GIF 大小自动选择 | 自适应 | 自适应 |

- **帧像素内存预算**：`GifConfig.max_memory_mb` 默认 **40MB**（合法范围 `[10, 500]`），解码后帧总内存不超过该值
- **平衡保留帧数**：`GifConfig.balanced_keep_frames` 默认 **10**（合法范围 `[1, 1000]`）
- 上述配置项由 `ConfigManager` 在启动时注入引擎（`set_gif_memory_strategy` / `gif_max_memory_mb`），并支持热重载

***

## 5. 网页壁纸

### 选择：WebView2（webview2-com 0.34，子进程创建）

- **硬件加速**：Chromium 引擎，GPU 合成
- **进程隔离**：WebView2 环境在壳进程 `--wp-proc` 模式子进程中创建，壳进程仅作代理层
- **轻量**：系统预装 WebView2 Runtime，无需捆绑 CEF

### 设计方案

```
WebRenderer（web.rs）→ 以 `--wp-proc` 模式启动壳进程自身 → 子进程用 webview2-com 创建 WebView2 环境 → 查找窗口 HWND + PID 校验 → SetParent 嵌入 WorkerW → WpProcIpcClient 命名管道控制
```

- `WebRenderer` 位于 `mirrorstar-core/src/wallpaper/web.rs`，为代理层（行数以实测为准）
- 子进程接口通过 `mirrorstar-core/src/ipc/wp_proc.rs` 的 `WpProcIpcClient` 通信
- 命令集：`Play` / `Terminate` / `SetPosition` / `Navigate` / `Pause` / `Resume`（JSON + 换行分隔）
- **按需创建 / 关闭即销毁**：仅启用网页壁纸时才启动 wp-proc 子进程，避免闲置时占用 WebView2 运行时资源
- 命名管道：`\\.\pipe\mirrorstar-wp-{uuid}`；进程与 IPC 细节详见[进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)

***

**相关文档：**

- [架构与技术栈总览](./01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)
- [桌面集成与 Windows 系统 API](./04-桌面集成与Windows系统API-Desktop-Integration-and-Windows-API.md)
- [暂停恢复机制](./06-暂停恢复机制-Pause-Resume.md)
- [性能优化](./09-性能优化-Performance.md)