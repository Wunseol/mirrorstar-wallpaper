[← 返回文档索引](../index.md) > 架构与技术栈 > 桌面集成与 Windows 系统 API

# MirrorStar Wallpaper（镜星壁纸）— 桌面集成与 Windows 系统 API

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v3.0                      |
| 更新日期 | 2026-10-11                |
| 文档状态 | 已实现（已按真实代码合并重组，2026-10）        |

> 本文由原《桌面集成》与《Windows 系统 API》合并去重而成。**Native/WorkerW 渲染路径的选择与对比**统一收敛至[壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)，本文不再重复展开，仅保留桌面集成与系统 API 的机制细节。

---

## 1. 桌面集成详细设计

桌面集成模块负责：WorkerW 窗口查找与嵌入、窗口样式修改、多显示器定位、DPI 感知、Explorer 重启检测与重建、纯净壁纸模式（窗口层级临时提升）以及鼠标输入处理。

> **渲染路径说明**：静态图片（JPG/JPEG/PNG/BMP/TIF/TIFF/DIB）使用 Windows 原生壁纸 API（`SystemParametersInfoW` + 注册表）实现零资源占用，其余格式走 WorkerW 嵌入。两条路径的选择规则、对比表与原生 API 实现详见[壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)。

### 1.1 WorkerW 嵌入完整流程

```mermaid
sequenceDiagram
    participant Engine as 壁纸引擎
    participant Desktop as DesktopIntegrator
    participant User32 as user32.dll
    participant Progman as Progman 窗口
    participant WorkerW as WorkerW 窗口
    participant Wp as 壁纸窗口

    rect rgb(230, 245, 255)
        Note over Engine,WorkerW: 阶段1: 异步初始化桌面集成（后台线程预初始化）
        Engine->>Desktop: new() — 不调用 find_workerw()
        Note over Desktop: DesktopIntegrator::new() 仅初始化字段<br/>不执行 WorkerW 查找

        par 后台线程预初始化
            Desktop->>Desktop: std::thread::spawn → find_workerw()
            Desktop->>User32: FindWindowW("Progman", None)
            User32-->>Desktop: progman_hwnd

            Desktop->>User32: FindWindowExW(Progman, NULL, "WorkerW", NULL)
            Note over User32,WorkerW: raised desktop（Progman 带 WS_EX_NOREDIRECTIONBITMAP）下只探测 Progman 的直接子 WorkerW：<br/>发送 0x052C 之前一次、之后一次；<br/>不执行顶层 EnumWindows/兄弟 WorkerW 路径

            Desktop->>User32: SendMessageTimeoutW(progman, 0x052C, 0xD, 0x1, SMTO_NORMAL | SMTO_ABORTIFHUNG, 1000)
            Note over User32,Progman: 0x052C 消息（wParam=0xD / lParam=0x1）触发 Progman<br/>创建新的 WorkerW<br/>超时 1000ms（raised desktop 下创建较慢）
            Progman->>WorkerW: 创建 WorkerW 窗口

            Desktop->>User32: EnumWindows(callback, 0)
            Note over User32: 非 raised desktop 下旧层级遍历顶层窗口：<br/>1. 查找包含 SHELLDLL_DefView 的窗口<br/>2. 找到 SHELLDLL_DefView 的父窗口后，<br/>通过 GetWindow(GW_HWNDNEXT) 获取下一个<br/>兄弟 WorkerW 窗口作为嵌入目标
            User32-->>Desktop: workerw_hwnd

            Desktop->>Desktop: 缓存 progman_hwnd, workerw_hwnd
        end

        Engine->>Desktop: ensure_initialized() — 懒等待初始化完成
        Desktop-->>Engine: 初始化完成
    end

    rect rgb(255, 245, 230)
        Note over Engine,Wp: 阶段2: 嵌入壁纸窗口
        Engine->>Desktop: embed_wallpaper(wp_hwnd, display_id, arrangement)
        Desktop->>Desktop: ensure_initialized() → 读取缓存的 progman_hwnd / workerw_hwnd<br/>并取显示器缓存列表（5s TTL）

        Desktop->>User32: make_borderless(wp_hwnd) + remove_from_taskbar(wp_hwnd)
        Note over User32,Wp: 步骤1: 去边框并移出任务栏<br/>读-改-写 GWL_STYLE / GWL_EXSTYLE

        Desktop->>User32: GetWindowRect(workerw_hwnd, &mut workerw_rect)
        User32-->>Desktop: workerw_rect
        Note over User32,WorkerW: 步骤2: 读取 WorkerW 的屏幕矩形<br/>（各分支共用一次调用）

        Desktop->>Desktop: 计算子窗口坐标
        Note over Desktop: 步骤3: PerMonitor / AllSame 用<br/>display.x - workerw_rect.left、display.y - workerw_rect.top<br/>（calculate_per_monitor_child_coords）；<br/>Span 用 (0, 0, WorkerW 宽, WorkerW 高)；<br/>找不到显示器时回退 WorkerW 全尺寸并 warn

        Desktop->>User32: SetParent(wp_hwnd, workerw_hwnd)
        Note over User32,Wp: 步骤4: 将壁纸窗口设为 WorkerW 的子窗口

        Desktop->>User32: SetWindowPos(wp_hwnd, HWND_BOTTOM, x, y, w, h,<br/>SWP_NOACTIVATE | SWP_FRAMECHANGED)
        Note over User32,Wp: 步骤5: 按子窗口坐标定位并置于 z-order 底部

        Desktop->>User32: ShowWindow(wp_hwnd, SW_SHOW)
        Note over User32,Wp: 步骤6: 显示窗口<br/>（创建时不带 WS_VISIBLE，避免闪屏）

        Desktop->>Desktop: worker_w::ensure_workerw_zorder(progman_hwnd, workerw_hwnd)
        Note over Desktop,WorkerW: 步骤7: z-order 保障（仅 raised desktop 生效，旧层级直接返回）<br/>用 GetWindow(Progman, GW_CHILD) 取首个（最上层）直接子窗口，<br/>再用 GetWindow(首个直接子窗口, GW_HWNDLAST) 取最底层直接子窗口，<br/>与 WorkerW 比对；已是底层则返回，否则 SetWindowPos(WorkerW, HWND_BOTTOM,<br/>0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)<br/>best-effort，失败仅 warn

        Desktop-->>Engine: 嵌入完成
    end
```

#### 1.1.1 raised desktop 兼容分支（Windows 11 24H2/25H2）

Windows 11 24H2/25H2 启用 raised desktop 后，桌面层级发生变化：`Progman` 带 `WS_EX_NOREDIRECTIONBITMAP` 扩展样式，承载壁纸的 `WorkerW` 变为 `Progman` 的**直接子窗口**，不再存在旧层级的顶层兄弟 `WorkerW`。若沿用旧查找逻辑会找不到 WorkerW 或嵌错层，因此实现中增加了以下兼容分支：

- **特性检测（不判断系统版本号）**：`progman_is_raised_desktop(progman)` 读取 `GetWindowLongPtrW(Progman, GWL_EXSTYLE) & WS_EX_NOREDIRECTIONBITMAP`；`is_raised_desktop()` 在内部查找 Progman 后调用前者。以此识别层级形态，避免因系统 build 号差异误判。
- **查找顺序（按层级分流）**：
  - **非 raised desktop**：保持旧层级顺序不变——`SHELLDLL_DefView` 父窗口为 WorkerW → `Progman` 顶层兄弟 WorkerW → `EnumWindows` → 发送 `0x052C` → 再次 `EnumWindows` → fallback。
  - **raised desktop**：只探测 `FindWindowExW(Progman, NULL, "WorkerW", NULL)` 得到的 Progman 直接子 WorkerW，由 `find_progman_child_workerw` 用 `is_workerw_class` 校验类名；发送 `0x052C` **之前**探测一次、**之后**探测一次，未命中即返回 `WorkerWNotFound`；**不执行**顶层兄弟 WorkerW 查找与任何顶层 `EnumWindows`（含 Step 2/4/5），避免误选第三方应用的顶层 WorkerW。
- **嵌入方式不变**：仍通过 `SetParent` 将壁纸窗口挂到 WorkerW（raised desktop 下即 Progman 的直接子 WorkerW），不引入 `WS_EX_LAYERED`。
- **z-order 保障**：raised desktop 下桌面图标层 `SHELLDLL_DefView` 与 WorkerW 同为 `Progman` 的子窗口，WorkerW 必须是 Progman 的最后一个（最底层）子窗口，否则壁纸窗口会遮挡桌面图标。`DesktopIntegrator::embed_wallpaper` 在 `worker_w::embed_wallpaper` 成功后调用 `ensure_workerw_zorder(progman_hwnd, workerw_hwnd)`：判定只考察 `Progman` 的直接子窗口——用 `GetWindow(Progman, GW_CHILD)` 取首个（最上层）直接子窗口、再以 `GetWindow(首个直接子窗口, GW_HWNDLAST)` 取最底层直接子窗口（不递归枚举子孙窗口），仅当该窗口不是 WorkerW 时才执行 `SetWindowPos(WorkerW, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)` 置底。该函数仅对 raised desktop 生效（旧层级直接返回），best-effort，失败仅 `warn`。
- **SPI 早退**：raised desktop 下 `SPI_SETDESKWALLPAPER` 会销毁承载动态壁纸的 WorkerW（参考 Lively Wallpaper 的 `RefreshDesktop` 早退实现），因此 `refresh_desktop()` 在 raised desktop 下直接返回并记 debug 日志；`clear_native_wallpaper()` 同样记 debug 后返回 `Ok(())`（保持调用方既有语义，注册表原值保留不影响后续静态壁纸覆盖）。
- **失效自愈不变**：`is_workerw_valid` / `check_and_reinitialize` / `ensure_workerw_ready` 的失效检测与重试逻辑保持不变；raised desktop 下句柄失效后重新查找会经上述子窗口探测得到新的 Progman 子 WorkerW，再按既有流程重新嵌入（每次嵌入同样经过 z-order 保障）。

> 该分支只影响 WorkerW 嵌入路径的查找与层级保持；Native 静态图片路径（`SystemParametersInfoW` + 注册表）与旧层级桌面行为不变。

### 1.2 窗口样式修改

壁纸窗口嵌入 WorkerW 前需要修改窗口样式，去除边框和任务栏显示：

#### 1.2.1 去除边框（Borderless Window）

```rust
fn make_borderless(hwnd: HWND) -> Result<()> {
    unsafe {
        // 移除标准窗口样式中的标题栏、边框、系统菜单等
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let new_style = WINDOW_STYLE(style)
            & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX);
        let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, new_style.0 as isize);

        // 移除扩展样式中的对话框边框、窗口边缘等
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let new_ex_style = WINDOW_EX_STYLE(ex_style)
            & !(WS_EX_DLGMODALFRAME | WS_EX_COMPOSITED | WS_EX_WINDOWEDGE
                | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE
                | WS_EX_TOOLWINDOW | WS_EX_APPWINDOW);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex_style.0 as isize);
    }
    Ok(())
}
```

#### 1.2.2 从任务栏移除

```rust
fn remove_from_taskbar(hwnd: HWND) {
    unsafe {
        // 读-改-写模式：保留必要样式，添加 TOOLWINDOW 和 NOACTIVATE
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex_style = (ex_style as usize)
            & !(WS_EX_APPWINDOW.0 as usize)   // 移除任务栏显示标志
            | (WS_EX_TOOLWINDOW.0 as usize)    // 添加工具窗口标志（不在任务栏显示）
            | (WS_EX_NOACTIVATE.0 as usize);   // 添加不激活标志
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex_style as isize);

        // 需要调用 SetWindowPos 刷新窗口样式
        let _ = SetWindowPos(hwnd, HWND::default(), 0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
    }
}
```

### 1.3 多显示器定位详解

多显示器环境下，每个显示器的壁纸窗口需要精确定位到对应的屏幕区域。关键挑战是 Windows 的多显示器坐标系——副显示器的坐标可能为负值（在主显示器左侧）或大于主显示器分辨率（在右侧/下方）。

```rust
impl DesktopIntegrator {
    /// 将壁纸窗口嵌入到指定显示器
    fn embed_to_display(&self, hwnd: HWND, display_id: &str) -> Result<()> {
        // 1. 获取目标显示器的边界
        let monitor = self.get_monitor_info(display_id)?;
        let bounds = monitor.monitor;

        // 2. 将壁纸窗口定位到屏幕绝对坐标
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_BOTTOM,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
        }

        // 3. 坐标转换：绝对坐标 → 相对于 WorkerW 的坐标
        let mut rect = RECT {
            left: bounds.left,
            top: bounds.top,
            right: bounds.right,
            bottom: bounds.bottom,
        };
        unsafe {
            // HWND::default() (NULL) 作为 hwndFrom 表示源坐标是屏幕绝对坐标
            MapWindowPoints(HWND::default(), self.workerw, &mut rect as *mut _ as *mut POINT, 2)?;
        }

        // 4. 设为 WorkerW 子窗口
        unsafe {
            SetParent(hwnd, self.workerw)?;
        }

        // 5. 设置相对坐标
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_BOTTOM,
                rect.left,
                rect.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
        }

        Ok(())
    }
}
```

### 1.4 跨显示器拉伸模式

当壁纸排列方式为 `span` 时，单个壁纸窗口覆盖所有显示器：

```rust
fn span_all_displays(&self, hwnd: HWND) -> Result<()> {
    // 获取 WorkerW 的完整区域
    let mut workerw_rect = RECT::default();
    unsafe {
        GetWindowRect(self.workerw, &mut workerw_rect)?;
    }

    // 设为 WorkerW 子窗口
    unsafe {
        SetParent(hwnd, self.workerw)?;
    }

    // 填充整个 WorkerW 区域
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            workerw_rect.right - workerw_rect.left,
            workerw_rect.bottom - workerw_rect.top,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
    }

    Ok(())
}
```

### 1.5 DPI 感知处理

MirrorStar 声明为 Per-Monitor DPI Aware V2 应用，确保在不同 DPI 缩放比例的显示器上正确显示壁纸。

#### 1.5.1 DPI 感知声明

在应用程序 manifest 中声明 Per-Monitor DPI Aware V2：

```xml
<application xmlns="urn:schemas-microsoft-com:asm.v3">
  <windowsSettings>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
  </windowsSettings>
</application>
```

#### 1.5.2 DPI 缩放处理策略

| 场景 | 处理方式 |
|------|----------|
| 壁纸窗口定位 | 使用物理像素坐标（GetMonitorInfo 返回的 rcMonitor 已是物理像素） |
| 壁纸窗口尺寸 | 使用物理像素尺寸，与显示器实际分辨率匹配 |
| DPI 变更 | 监听 WM_DPICHANGED 消息，重新计算壁纸窗口位置和尺寸 |
| 多显示器不同 DPI | 每个显示器的壁纸窗口独立处理，使用该显示器的 DPI 值 |

#### 1.5.3 WM_DPICHANGED 处理

```rust
/// 处理 DPI 变更消息
fn on_dpi_changed(hwnd: HWND, new_rect: &RECT) {
    unsafe {
        // 按系统建议的新矩形调整窗口大小
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            new_rect.left,
            new_rect.top,
            new_rect.right - new_rect.left,
            new_rect.bottom - new_rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}
```

### 1.6 Explorer 重启检测与 WorkerW 重新嵌入

Windows Explorer（explorer.exe）可能在运行期间重启（如用户通过任务管理器重启、系统更新等），导致 WorkerW 窗口句柄失效。MirrorStar 需要检测 Explorer 重启并重新嵌入壁纸窗口。

#### 1.6.1 检测方案

通过 `start_workerw_check()`（`src-tauri/src/platform/workerw_check.rs`）实现：使用 `tokio interval(300s)` + `Notify` 事件触发结构，事件驱动为主（`TaskbarCreated` 消息），并在超时（5 分钟）后周期性地调用 `check_and_reinitialize()` 校验/重建 WorkerW。辅助地通过定时验证 WorkerW 句柄有效性来兜底检测 Explorer 重启：

```rust
impl DesktopIntegrator {
    /// 检测 Explorer 重启并重新初始化
    fn check_and_reinitialize(&mut self) -> Result<()> {
        // 验证缓存的 WorkerW 句柄是否仍然有效
        if self.is_workerw_valid() {
            return Ok(());
        }

        tracing::warn!("WorkerW 句柄失效，可能是 Explorer 重启，正在重新初始化...");

        // 重新查找 WorkerW
        self.find_workerw()?;

        // 重新嵌入所有活跃壁纸
        for (display_id, hwnd) in &self.active_wallpapers {
            self.embed_to_display(*hwnd, display_id)?;
        }

        tracing::info!("WorkerW 重新初始化完成，壁纸已重新嵌入");
        Ok(())
    }

    /// 验证 WorkerW 句柄是否有效
    fn is_workerw_valid(&self) -> bool {
        if self.workerw.is_invalid() {
            return false;
        }
        unsafe {
            // 检查窗口是否仍然存在
            IsWindow(self.workerw).as_bool()
        }
    }
}
```

#### 1.6.2 检测触发方式

| 方式 | 优点 | 缺点 |
|------|------|------|
| TaskbarCreated 消息（事件驱动） | 精确，Explorer 重启时系统必定广播此消息 | 需要注册消息接收 |
| 5 分钟（300 秒）定时兜底 | 简单可靠 | 有延迟（最多 5 分钟） |
| 监听 Progman 窗口重建 | 精确 | 实现复杂 |
| SetWinEventHook 监听 | 事件驱动，即时响应 | Explorer 重启不一定触发此事件 |

**推荐方案**：使用 `TaskbarCreated` 消息作为主要检测方式（Explorer 重启时系统必定广播此消息），辅以 `start_workerw_check()`（将 Explorer 重启事件通知触发 + tokio interval 300 秒 + Notify，`is_workerw_valid()` + `check_and_reinitialize()`）作为兜底。

#### 1.6.3 重嵌入流程

```mermaid
flowchart TD
    A[检测到 WorkerW 句柄失效] --> B[记录警告日志]
    B --> C[重新查找 Progman 窗口]
    C --> D{找到 Progman?}
    D -->|否| E[延迟 1 秒后重试<br/>最多重试 3 次]
    E --> D
    D -->|是| F[发送 0x052C 消息]
    F --> G[重新查找 WorkerW]
    G --> H{找到 WorkerW?}
    H -->|否| I[记录错误，等待下次检测]
    H -->|是| J[重新嵌入所有活跃壁纸窗口]
    J --> K[刷新桌面]
    K --> L[记录恢复日志]
```

### 1.7 纯净壁纸模式：窗口层级临时提升

正常模式下壁纸窗口被 `SetParent` 挂到 WorkerW 层，其 Z 序位于**桌面图标之下**，因此无法覆盖任何应用窗口。纯净壁纸模式通过"临时把壁纸窗口提升为顶层置顶窗口"实现全屏纯壁纸沉浸态（唯一入口：托盘勾选项「纯净壁纸（按任意键退出）」），退出后再用 `embed_wallpaper` 嵌回。

#### 1.7.1 层级对比

| 状态 | Z 序（自顶向下） | 说明 |
|------|------------------|------|
| 正常模式 | 应用窗口 > 桌面图标（`SHELLDLL_DefView`）> 壁纸窗口（WorkerW 子窗口）> Progman | 壁纸位于图标层下方，桌面交互（右键菜单、图标点击）不受影响 |
| 纯净壁纸模式 | 壁纸窗口（`HWND_TOPMOST`）> 其他顶层窗口 + 任务栏 > Progman | 壁纸窗口置顶覆盖全部（含桌面图标、应用窗口与任务栏） |

**图片 / GIF / 网页**路径的置顶实现（`crates/mirrorstar-core/src/pure_mode/mod.rs::lift_wallpaper_window`）：

```text
GetWindowRect(hwnd) → (x, y, w, h)                                    // ① 先读当前屏幕矩形
SetParent(hwnd, None)                                                  // ② 从 WorkerW 摘出
SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h,
             SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED)       // ③ 原矩形置顶
InvalidateRect(hwnd, None, false)                                      // ④ GDI 渲染器保险重绘
```

**视频**路径（**架构 D1，例外**）：视频壁纸**原窗口留在 WorkerW 全程不搬动**（不 `detach_for_pure_mode`），改为**新建临时顶层覆盖渲染器窗口**并置顶到同一屏幕矩形——`construct_renderer` 新建 `VideoRenderer`（配置快照复用无副作用的 `renderer_config()`）→ `make_borderless`（失败仅 `warn`）+ `remove_from_taskbar` → `VideoRenderer::disable_auto_window_resize()`（IPC best-effort，须早于置顶 / `loadfile`）→ `lift_window_to_rect` 置顶全屏 → `after_embed()` 从头 `loadfile`；**退出时销毁该临时窗，原窗口原样显露**（原 mpv 从不重启）。任一步失败即 best-effort `terminate()` 该临时渲染器并放弃本显示器覆盖，不回滚整体进入。措辞与契约详见 [`04-wallpaper模块.md`](../04-优化文档/04-wallpaper模块.md) 的「四类呈现路径（D3 / D4 / D8，视频为架构 D1）」与 [`03-desktop模块.md`](../04-优化文档/03-desktop模块.md) 的「退出回嵌的防呈现冻结契约（架构 D1）」。

Native 静态图片没有窗口，改为创建临时 `ImageRenderer` 窗口后用 `lift_window_to_rect`（**不做**重定父级）置顶到同一矩形。

#### 1.7.2 关键约束

- **双路径呈现（架构 D1）**：**图片 / GIF / 网页**路径为「摘出 WorkerW + 原窗口置顶」；**视频**路径为「原窗口留在 WorkerW 不搬动 + 新建临时顶层覆盖渲染器置顶」（退出仅销毁覆盖窗、原窗口原样显露）。二者并非所有类型都搬动原窗口——勿据单一描述推断。
- **保持当前屏幕矩形，零 resize**：摘出 / 置顶复用第 ① 步读到的矩形，宽度高度不做任何调整。mpv 的 D3D11 输出在"窗口重父化 + resize"时会重配置（项目已用 `--d3d11-flip=no` 做双保险），零尺寸变化是规避该路径的核心降险手段。
- **不抢前台**：置顶只做"重定父级 + 设置 `HWND_TOPMOST`"，全程带 `SWP_NOACTIVATE`；窗口在嵌入阶段已由 `remove_from_taskbar` 设置 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`（`make_borderless` 不会清除），因此置顶后不会成为前台窗口、不进 Alt+Tab，从根上避免触发全屏检测。
- **鼠标穿透在模式内强制关闭**（D2）：置顶窗若保持 `WS_EX_TRANSPARENT`，点击会穿透到下层桌面图标。进入时对每个置顶窗 `set_mouse_passthrough(false)`（仅去 `WS_EX_TRANSPARENT`，并幂等清除旧版本可能遗留的 `WS_EX_LAYERED`；该函数绝不写入 `WS_EX_LAYERED`，避免壁纸窗口进入「分层但无分层属性」的非合成态导致呈现停止），退出时按 `interaction_mode` 还原（`set_mouse_passthrough(!interaction_mode)`）。
- **摘出即从 `active_wallpapers` 移除条目**：避免 Explorer 重启 / `workerw_check` 的重新初始化把已摘出的窗口又嵌回桌面（契约详见 [`03-desktop模块.md`](../04-优化文档/03-desktop模块.md) 第 5 节）。
- **退出回嵌**：`DesktopIntegrator::reattach_after_pure_mode` 委托既有 `embed_wallpaper`，零新增几何算法，自动使用 Explorer 重启后的新 WorkerW；**并在 `embed_wallpaper` 成功之后追加一次 `InvalidateRect(hwnd, None, false)` 保险重绘**（失败仅 `warn`），与进入路径 `pure_mode::lift_wallpaper_window` 末尾的保险重绘同源。该顺序契约由 `crates/mirrorstar-core/src/desktop/mod.rs` 的契约测试 `reattach_after_pure_mode_invalidates_after_embed` 守护。
- **退出方式**：模式内**任意键** / **鼠标左、中、右键点击**（宽限期外）/（任务栏可达时）**托盘二次点击**均触发退出。**Win+L 不锁屏**：模式内任意键（含 Win 组合键）先退出模式，键盘钩子在 LWIN 按下即吞掉整段按键序列，系统锁定热键不生效——如需锁屏请在模式外执行，或使用不受低级钩子影响的 SAS 路径（Ctrl+Alt+Del，锁屏期间模式保持、解锁后任意键退出）。

#### 1.7.3 进入 / 退出时序（简化）

```mermaid
sequenceDiagram
    participant Tray as 托盘
    participant Orch as platform::pure_mode
    participant Input as 输入捕获线程（低级钩子 + 隐藏顶层窗口）
    participant Engine as WallpaperEngine
    participant Desktop as DesktopIntegrator

    Tray->>Orch: 勾选「纯净壁纸（按任意键退出）」
    Note over Orch: 进入互斥守卫（F2）+ 幂等检查 + 等待在途退出落位（≈10s 上界）
    Orch->>Input: install（WH_KEYBOARD_LL / WH_MOUSE_LL，键盘钩子失败即拒绝进入）
    Input-->>Orch: 安装结果
    Orch->>Orch: ④b 关闭托盘溢出浮出窗（best-effort；必须在壁纸窗口置顶之前）
    Orch->>Engine: enter_pure_mode()
    Engine->>Desktop: detach_for_pure_mode(display_id)（图片 / GIF / 网页：摘出 + 置顶，移除条目）
    Engine->>Engine: 关闭鼠标穿透 + register_lifted + pure_mode = Some(..)
    Note over Engine,Desktop: 视频（架构 D1）：原窗口不搬动，新建临时顶层覆盖渲染器窗口置顶
    Orch->>Tray: set_checked(true)（置勾选早于启动退出监听）

    Note over Input,Engine: 任意键 / 鼠标左中右键点击 / 托盘二次点击（宽限期外）
    Input->>Orch: ExitReason::Input（吞掉触发输入）
    Orch->>Engine: exit_pure_mode()
    Engine->>Desktop: reattach_after_pure_mode(..)（embed_wallpaper 嵌回 + InvalidateRect）
    Engine->>Engine: 终止临时渲染器（视频覆盖窗销毁后原窗口原样显露）+ 还原穿透 + set_active(false)
    Orch->>Input: shutdown（PostThreadMessageW(WM_QUIT) + join）
    Orch->>Tray: set_checked(false)（③ 取消勾选，提前到回嵌 + 卸载钩子之后）
    Orch->>Orch: 补跑全屏 / 电源判定
    Orch->>Orch: 存活校验 + 自愈（liveness_check_and_heal，含暂停短路）
    Orch->>Orch: 唤醒轮换调度器
```

> 退出时**必须先恢复桌面（`exit_pure_mode` 回嵌）再卸载输入钩子**，否则钩子卸载后到回嵌完成之间，用户输入会落到仍处于置顶全屏状态的壁纸窗口上。退出编排完整的 ①-⑥ 步骤表（含 ③ 取消勾选**提前**、⑤ 存活校验/自愈的位置与顺序理由）见 [`03-desktop模块.md`](../04-优化文档/03-desktop模块.md) §5.4。

> 进入侧 **④b 关闭托盘溢出浮出窗必须在壁纸窗口置顶（`enter_pure_mode`）之前**：进入编排刻意不抢前台，浮出窗收不到"失去激活"关闭信号，若在置顶之后才关闭会被置顶窗覆盖而无法再关闭。

***

### 1.8 壁纸缩放模式设计

FR-034 定义了五种壁纸缩放模式，各模式渲染逻辑如下：

| 模式 | 说明 | 视频实现 | GIF/图片实现 | 网页实现 | 原生壁纸实现 |
|------|------|----------|-------------|----------|-------------|
| Fill | 填满屏幕，保持比例，裁剪溢出 | mpv --panscan=1.0 | GDI 双缓冲缩放+裁剪 | CSS object-fit: cover | 注册表 WallPaperStyle=10, TileWallpaper=0 |
| Fit | 适配屏幕，保持比例，留黑边 | mpv --keepaspect | GDI 双缓冲缩放+居中 | CSS object-fit: contain | 注册表 WallPaperStyle=6, TileWallpaper=0 |
| Stretch | 拉伸填满，不保持比例 | mpv --no-keepaspect | GDI 双缓冲拉伸 | CSS object-fit: fill | 注册表 WallPaperStyle=2, TileWallpaper=0 |
| Center | 原始大小居中，超出裁剪 | mpv --no-keepaspect --panscan=0 | GDI 双缓冲居中+裁剪 | CSS object-fit: none | 注册表 WallPaperStyle=0, TileWallpaper=0 |
| Original | 原始大小，左上角对齐 | mpv 默认 | GDI 双缓冲原始大小 | CSS 无缩放 | 注册表 WallPaperStyle=0, TileWallpaper=0 |

缩放模式通过 IPC `set_scaling_mode` 命令传递给子进程，子进程的 WallpaperRenderer 实现类根据模式调整渲染参数。对于原生壁纸模式（jpg/jpeg/png/bmp/tif/tiff/dib），缩放模式通过注册表 `WallPaperStyle` 和 `TileWallpaper` 值控制。

### 1.9 鼠标输入处理设计

FR-031（鼠标穿透）和 FR-033（鼠标交互模式）的实现机制：

#### 1.9.1 鼠标穿透模式（FR-031）

通过 `WS_EX_TRANSPARENT` 扩展窗口样式实现鼠标点击穿透：

```rust
fn set_mouse_passthrough(hwnd: HWND, enabled: bool) {
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = if enabled {
            ex_style | WS_EX_TRANSPARENT.0 as isize
        } else {
            ex_style & !(WS_EX_TRANSPARENT.0 as isize)
        };
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
    }
}
```

启用 `WS_EX_TRANSPARENT` 后，鼠标事件会穿透壁纸窗口到达下方的桌面图标。

#### 1.9.2 鼠标交互模式（FR-033）

交互模式用于网页壁纸，允许用户直接与网页内容交互（如点击链接）：
- 交互模式开启：壁纸窗口正常接收鼠标消息，WebView2 处理输入
- 交互模式关闭：同鼠标穿透模式，使用 `WS_EX_TRANSPARENT`

两种模式通过 IPC `set_mouse_passthrough` 和 `set_interaction_mode` 命令控制，互为补充。

***

### 1.10 WorkerW 异步初始化

`DesktopIntegrator::new()` 不再同步调用 `find_workerw()`，而是通过后台线程预初始化，避免阻塞壳进程启动。

```rust
impl DesktopIntegrator {
    /// 创建 DesktopIntegrator 实例（不执行 WorkerW 查找）
    pub fn new() -> Self {
        let initialized = Arc::new(AtomicBool::new(false));
        let workerw = Arc::new(Mutex::new(None));

        // 后台线程预初始化
        let init_flag = initialized.clone();
        let workerw_handle = workerw.clone();
        std::thread::spawn(move || {
            if let Ok((progman, wkw)) = Self::find_workerw() {
                *workerw_handle.lock().unwrap() = Some((progman, wkw));
                init_flag.store(true, Ordering::Release);
            }
        });

        Self {
            progman: None,
            workerw: None,
            initialized,
            workerw_handle: workerw,
        }
    }

    /// 懒等待初始化完成（首次设置壁纸时调用）
    pub fn ensure_initialized(&self) -> Result<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        // 后台线程仍在初始化中，由调用方按需重试或等待
        Ok(())
    }
}
```

**关键变更：**

| 项目 | 旧方案 | 新方案 |
|------|--------|--------|
| `new()` 行为 | 同步调用 `find_workerw()` | 仅初始化字段，后台线程预初始化 |
| 初始化时机 | 构造时阻塞 | 后台线程异步，`ensure_initialized()` 懒等待 |
| `SendMessageTimeoutW` 超时 | 500ms | 1000ms（`WM_SPAWN_WORK_TIMEOUT_MS`，raised desktop 下创建较慢） |
| 壳进程启动影响 | 阻塞直到 WorkerW 查找完成 | 不阻塞，启动更快 |

***

### 1.11 包结构规模参考

- `desktop` 模块：`mod.rs` / `native_wallpaper.rs` / `window.rs` / `worker_w.rs`（行数以实测为准）
- 全屏检测：`fullscreen.rs`，纯 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 事件驱动，失败仅记录并退出监控线程（无轮询回退）

***

## 2. Windows 系统 API

### 2.1 选择：windows-rs 0.58

**选择理由：**

- **微软官方维护**：由 Microsoft Windows 开发团队维护，API 覆盖全
- **惯用 Rust 风格**：COM 接口自动生成 Rust trait，错误处理使用 `windows::core::Result`
- **零成本**：编译时生成绑定，无运行时开销
- **活跃更新**：随 Windows SDK 同步更新（workspace 锁定 `0.58`）

### 2.2 workspace windows-rs 实际 features

以下为 workspace `Cargo.toml` 中 `windows = "0.58"` 实际启用的 feature 清单，与代码一致：

```toml
windows = { version = "0.58", features = [
    "Win32_Foundation",                 # HWND, RECT, BOOL, LPARAM, WPARAM 等基础类型
    "Win32_Graphics_Gdi",               # MonitorFromWindow, GetMonitorInfoW 等 GDI 查询
    "Win32_Media_Audio",                # IMMDevice, IAudioSessionManager2, ISimpleAudioVolume (WASAPI)
    "Win32_Media_Audio_Endpoints",      # 音频端点枚举
    "Win32_Security",                   # 安全/SDDL 相关
    "Win32_Security_Authorization",     # 授权/令牌相关
    "Win32_Storage_FileSystem",         # 文件系统操作
    "Win32_System_Com",                 # CoInitializeEx, COM 接口
    "Win32_System_Diagnostics_ToolHelp",# 进程快照/线程枚举 (ToolHelp32)
    "Win32_System_ProcessStatus",       # GetModuleBaseName 等进程状态
    "Win32_System_JobObjects",          # 作业对象 (Job Object) 子进程管理
    "Win32_System_IO",                  # IO 相关
    "Win32_System_LibraryLoader",       # GetModuleHandle 等加载
    "Win32_System_Memory",              # 内存相关 API
    "Win32_System_Pipes",               # CreateNamedPipeW, ConnectNamedPipe (命名管道 IPC)
    "Win32_System_Power",               # 电源状态/电池事件
    "Win32_System_SystemInformation",   # GetSystemInfo 等
    "Win32_System_Threading",           # CreateProcessW 等进程/线程管理
    "Win32_UI_Accessibility",           # SetWinEventHook (前台/窗口事件监控)
    "Win32_UI_HiDpi",                   # 高 DPI 支持
    "Win32_UI_WindowsAndMessaging",     # FindWindowW, SetParent, SetWindowPos, EnumWindows, SystemParametersInfoW 等
] }
```

> 说明：早期文档中列出的 `Win32_System_Registry`、`Win32_System_Ole`、`Win32_UI_Input_KeyboardAndMouse` 等 feature 未在 workspace 实际启用。注册表操作由 `winreg 0.55` 承担，不通过 windows-rs 原始 `RegSetValueExW` 调用。

### 2.3 关键能力与 API

#### 桌面集成（WorkerW 嵌入）

位于 `crates/mirrorstar-core/src/desktop/`，使用：

- `FindWindowW` — 定位桌面窗口（Progman）
- `SendMessageTimeoutW` — 触发 WorkerW 创建
- `EnumWindows` / `FindWindowExW` — 查找 Shell DefView / WorkerW（raised desktop 下用 `FindWindowExW(Progman, …)` 探测 Progman 的直接子 WorkerW）
- 支持多显示器、DPI 感知，桌面/Explorer 重启后重新嵌入（Explorer 重启监控）

#### 原生壁纸 API

- `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` — 设置桌面壁纸（需要与 `SPIF_UPDATEINIFILE | SPIF_SENDCHANGE` 配合持久化）
- `SystemParametersInfoW(SPI_GETDESKWALLPAPER)` — 读取当前壁纸路径
- 用于静态图片壁纸的 Native 路径，零运行时资源占用（实现细节见[壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)）

#### 窗口管理与嵌入

- `SetParent` / `SetWindowPos` — 将壁纸窗口设为 WorkerW 子窗口并控制位置、层级
- `SetWindowPos` 驱动 mpv / WebView2 窗口尺寸与位置同步

#### 事件驱动监控

- `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ...)` — 全屏应用检测（替代轮询）
- `SetWinEventHook`（窗口创建/销毁事件）— 桌面 / WorkerW 状态监控

#### 进程与作业管理

- `CreateProcessW` — 启动子进程（mpv.exe / 壳进程 `--wp-proc` 模式实例）
- 作业对象（`Win32_System_JobObjects`）— 约束子进程
- `OpenProcess` / `GetWindowThreadProcessId` — PID 校验、窗口归属校验

> 说明：MirrorStar 采用**逻辑暂停**（`PauseSender` 发暂停命令，子进程自行停止渲染/解码），不使用 `SuspendThread` / `ResumeThread` 线程挂起方案。

#### 注册表操作（使用 winreg）

`winreg 0.55` 提供类型安全的注册表读写：

- **开机自启**：写入启动项
- **壁纸缩放模式**：`HKEY_CURRENT_USER\Control Panel\Desktop` 下的 `WallPaperStyle` / `TileWallpaper`（REG_SZ），先写注册表再调用 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` 使设置生效

#### Core Audio API（音量控制）

位于 `crates/mirrorstar-core/src/audio/volume.rs`，基于 WASAPI：

- `IMMDevice` — 获取音频端点设备
- `IAudioSessionManager2` — 管理音频会话
- `ISimpleAudioVolume` — 按 PID 精确控制壁纸子进程（mpv）的音量
- 无音频设备时优雅降级为 no-op（`VolumeControl::new_disabled`）

#### COM 初始化

壳进程在启动阶段显式初始化 COM（单线程单元 STA，`CoInitializeEx(COINIT_APARTMENTTHREADED)`），并随 `run()` 返回时 `CoUninitialize`。WASAPI 相关线程按需在线程内完成 COM 初始化（如暂停线程使用 `COINIT_MULTITHREADED`）。WebView2 环境在 wp-proc 子进程内创建。详见[进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md) 的 COM 初始化策略。

### 2.4 关键 Windows API 速查表

| API                                           | 用途              | 模块   |
| --------------------------------------------- | --------------- | ---- |
| `FindWindowW("Progman", None)`                | 查找桌面管理器窗口       | 桌面集成 |
| `SendMessageTimeoutW(0x052C)`                 | 触发创建 WorkerW（wParam=0xD / lParam=0x1，超时 1000ms） | 桌面集成 |
| `EnumWindows`                                 | 遍历查找 WorkerW    | 桌面集成 |
| `SetParent`                                   | 嵌入壁纸窗口到 WorkerW | 桌面集成 |
| `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`    | 监听前台窗口切换        | 进程监控 |
| `IsZoomed`                                    | 检查窗口是否最大化       | 进程监控 |
| `MonitorFromWindow`                           | 获取窗口所在显示器       | 全屏检测 |
| `GetSystemPowerStatus`                        | 获取电源状态（ACLineStatus） | 电池监测 |
| `IMMDeviceEnumerator`                         | 获取音频设备          | 音频控制 |
| `ISimpleAudioVolume::SetMute`                 | 设置进程静音          | 音频控制 |
| `SystemParametersInfoW(SPI_SETDESKWALLPAPER)` | 设置静态壁纸 / 刷新桌面；`refresh_desktop` 与 `clear_native_wallpaper` 在 raised desktop 下跳过（避免销毁 WorkerW），`set_wallpaper_image` 设置静态图仍使用 | 桌面集成 |

***

**相关文档：**

- [架构与技术栈总览](./01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [模块设计](./02-模块设计-Module-Design.md)
- [进程架构与 IPC](./03-进程架构与IPC-Process-Architecture-and-IPC.md)
- [壁纸渲染](./05-壁纸渲染-Wallpaper-Rendering.md)
- [暂停恢复机制](./06-暂停恢复机制-Pause-Resume.md)
- [性能优化](./09-性能优化-Performance.md)