# MirrorStar Wallpaper（镜星壁纸）技术栈 — Windows API 绑定

[← 返回文档索引](../README.md) > [技术栈](./overview.md) > Windows API 绑定

## 4. Windows API 绑定

### 选择：windows-rs

**选择理由：**

- **微软官方维护**：由 Microsoft Windows 开发团队开发维护，API 覆盖最全
- **惯用 Rust 风格**：COM 接口自动生成 Rust trait，错误处理使用 `Result<T, E>`
- **完整 API 覆盖**：覆盖 Win32、WinRT、COM 等全部 Windows API
- **零成本**：编译时生成绑定，无运行时开销
- **活跃更新**：随 Windows SDK 同步更新

### 关键 API 使用

#### 桌面集成

**WorkerW 嵌入路径：**

```rust
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SendMessageTimeoutW, EnumWindows, FindWindowExW,
};
```

- `FindWindowW("Progman", ...)` — 定位桌面窗口
- `SendMessageTimeout(0x052C)` — 触发 WorkerW 创建
- `EnumWindows` / `FindWindowExW` — 查找 Shell DefView 和 WorkerW

**Native 壁纸 API 路径：**

```rust
use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;
use windows::Win32::UI::WindowsAndMessaging::SPI_SETDESKWALLPAPER;
use windows::Win32::UI::WindowsAndMessaging::SPI_GETDESKWALLPAPER;
use windows::Win32::UI::WindowsAndMessaging::SPIF_UPDATEINIFILE;
use windows::Win32::UI::WindowsAndMessaging::SPIF_SENDWININICHANGE;
```

- `SPI_SETDESKWALLPAPER` (0x0014) — 设置桌面壁纸
  - `uiParam`：0
  - `pvParam`：宽字符串路径（null-terminated wide string），传入空指针则清除壁纸
  - `fWinIni`：`SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE`（持久化到用户配置文件 + 通知系统）
  - 性能：约 10ms 完成，零运行时资源占用
- `SPI_GETDESKWALLPAPER` — 读取当前壁纸路径

#### 窗口管理

```rust
use windows::Win32::UI::WindowsAndMessaging::{
    SetParent, SetWindowPos, MapWindowPoints,
};
```

- `SetParent` — 将壁纸窗口设为 WorkerW 子窗口
- `SetWindowPos` — 控制壁纸窗口位置与层级
- `MapWindowPoints` — 坐标系转换

#### 事件驱动进程监控

```rust
use windows::Win32::UI::Accessibility::SetWinEventHook;
```

- `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ...)` — 监听前台窗口切换
- `SetWinEventHook(EVENT_OBJECT_DESTROY, ...)` — 监听窗口销毁
- 替代 Lively 的 `DispatcherTimer` 轮询方案，事件驱动更高效、延迟更低

#### 窗口状态检测

```rust
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsZoomed, GetWindowRect,
};
```

- `GetForegroundWindow` — 获取当前前台窗口
- `IsZoomed` — 检测窗口是否最大化
- `GetWindowRect` — 获取窗口位置与大小

#### 进程挂起/恢复（未使用）

> **注意**：以下 `SuspendThread` / `ResumeThread` 方案为 Lively Wallpaper 的实现思路，**MirrorStar 未采用**。MirrorStar 通过 `PauseSender` 进行逻辑暂停（向子进程发送暂停命令，由子进程自行停止渲染/解码），而非挂起线程。线程挂起可能导致子进程内部锁状态不一致、COM 调用中断等问题，因此未使用。

```rust
use windows::Win32::System::Threading::{
    OpenThread, SuspendThread, ResumeThread,
};
```

- `OpenThread` — 打开目标进程线程
- `SuspendThread` / `ResumeThread` — 挂起/恢复壁纸进程（节省 CPU/内存）
- **状态**：未使用（Lively 方案，MirrorStar 未采用）

#### 桌面刷新 / 原生壁纸设置

```rust
use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;
```

- `SPI_SETDESKWALLPAPER` — 设置桌面壁纸（详见上方"Native 壁纸 API 路径"）
- `SPI_SETDESKWALLPAPER`（空路径）— 清除壁纸 / 重置桌面（修复桌面闪烁等问题）

#### 注册表操作（壁纸缩放模式）

```rust
use winreg::RegKey;
use winreg::enums::*;
```

- **注册表路径**：`HKEY_CURRENT_USER\Control Panel\Desktop`
- **`WallPaperStyle`**（REG_SZ）：控制壁纸缩放模式
  - `0` — 居中（Center）
  - `2` — 拉伸（Stretch）
  - `6` — 适应（Fit）
  - `10` — 填充（Fill）
  - `22` — 跨区（Span，多显示器）
- **`TileWallpaper`**（REG_SZ）：控制平铺
  - `0` — 不平铺
  - `1` — 平铺
- 使用 `winreg` crate 进行类型安全的注册表读写操作，替代原始 `RegSetValueExW` / `RegGetValueW` 调用
- **调用顺序**：先写入注册表缩放模式，再调用 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` 使设置生效

#### Core Audio API

```rust
use windows::Win32::Media::Audio::{
    IMMDevice, IAudioSessionManager2, ISimpleAudioVolume,
};
```

- `IMMDevice` — 获取音频端点设备
- `IAudioSessionManager2` — 管理音频会话
- `ISimpleAudioVolume` — 控制壁纸进程音量（独立音量控制）

#### COM 初始化

> **重要**：主进程显式调用 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`（STA，单线程单元），在任何 COM 调用前完成初始化，并处理 `RPC_E_CHANGED_MODE`（0x80010106）错误（表示线程已被以其他并发模型初始化）。wp-proc 子进程同样使用 `CoInitializeEx(COINIT_APARTMENTTHREADED)`（STA），因为 WebView2 要求 STA 模型。子进程 COM 初始化后保持到进程退出。

使用 Core Audio API 和 WebView2 前需初始化 COM：

```rust
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::core::RPC_E_CHANGED_MODE;

// 主进程与 wp-proc 子进程均使用 STA（COINIT_APARTMENTTHREADED）
unsafe {
    match CoInitializeEx(None, COINIT_APARTMENTTHREADED) {
        Ok(()) => {},
        Err(e) if e.code() == RPC_E_CHANGED_MODE => {
            // 线程已被以其他并发模型初始化，忽略并继续
        },
        Err(e) => return Err(e.into()),
    }
}
// 注意：COM 初始化后应保持到进程退出，不要调用 CoUninitialize 过早释放
```

主进程与 wp-proc 子进程均使用单线程单元（STA，`COINIT_APARTMENTTHREADED`）。WebView2 要求 STA 模型；Core Audio API 在 STA 下亦可正常工作。

#### Raw Input API（鼠标转发）

```rust
use windows::Win32::UI::Input::RawInput::{
    GetRawInputData, RAWINPUT, HRAWINPUT,
};
```

- 将桌面点击事件转发到壁纸窗口（如网页壁纸交互）
#### 必需 Feature Flags

```toml
[dependencies.windows]
version = "0.58"
features = [
    "Win32_Foundation",                 # HWND, RECT, BOOL, LPARAM, WPARAM
    "Win32_UI_WindowsAndMessaging",     # FindWindow, SetParent, SetWindowPos, EnumWindows, SetWinEventHook, SystemParametersInfoW
    "Win32_UI_Input_KeyboardAndMouse",  # 键盘鼠标输入
    "Win32_UI_HiDpi",                   # 高 DPI 支持
    "Win32_UI_Accessibility",           # SetWinEventHook, EVENT_SYSTEM_FOREGROUND
    "Win32_Graphics_Gdi",               # MonitorFromWindow, GetMonitorInfoW
    "Win32_System_DataExchange",        # 剪贴板等数据交换
    "Win32_System_Com",                 # CoInitializeEx, COM 接口
    "Win32_System_Pipes",               # CreateNamedPipeW, ConnectNamedPipe, 命名管道 IPC
    "Win32_System_Diagnostics_ToolHelp", # 进程快照/枚举
    "Win32_System_SystemInformation",   # GetSystemInfo
    "Win32_System_Registry",            # 注册表操作（winreg 替代原始 API）
    "Win32_System_LibraryLoader",       # 模块/函数加载
    "Win32_System_Security",            # 安全相关 API
    "Win32_System_Ole",                # OLE 相关
    "Win32_System_Power",               # 电源管理
    "Win32_System_IO",                  # IO 操作
    "Win32_System_Threading",           # CreateMutexW, CreateProcessW, 进程/线程管理
    "Win32_Media_Audio",                # IMMDevice, IAudioSessionManager2, ISimpleAudioVolume
    "Win32_Media_Audio_Endpoints",      # 音频端点
    "Win32_Storage_FileSystem",         # 文件系统操作
]
```

---

**相关章节：** [← 总览](./overview.md) | [UI 框架](./ui-framework.md) | [壁纸渲染](./wallpaper-rendering.md)
