[← 返回文档索引](../index.md) > [技术栈](./01-技术栈总览-Tech-Stack-Overview.md) > 风险评估

# MirrorStar Wallpaper（镜星壁纸）技术栈 — 风险评估

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 技术栈全景图

```mermaid
graph TB
    subgraph Frontend["前端层 (WebView2)"]
        UI["HTML + CSS + TypeScript<br/>用户界面"]
        TS["Tauri IPC<br/>前后端通信"]
    end

    subgraph Tauri["Tauri v2 框架"]
        TauriCore["Tauri Core 2.11<br/>事件系统 / 命令系统"]
        WebView2["WebView2 Runtime<br/>Chromium 渲染"]
        TrayIcon["tray-icon feature<br/>系统托盘"]
    end

    subgraph Backend["后端层 (Rust)"]
        AppCore["应用核心 mirrorstar-core<br/>壁纸管理 / 生命周期"]

        subgraph Players["播放器 / 渲染模块"]
            MpvPlayer["mpv.exe 子进程<br/>视频播放"]
            GifPlayer["image crateg<br/>GIF / 图片渲染"]
            WebPlayer["WebView2 (wp-proc)<br/>网页壁纸"]
        end

        subgraph SystemAPI["系统 API 层 (windows-rs 0.58)"]
            DesktopAPI["桌面集成<br/>WorkerW 嵌入"]
            EventAPI["事件监控<br/>SetWinEventHook"]
            AudioAPI["音频控制<br/>WASAPI"]
            ProcessAPI["进程 / 作业管理<br/>CreateProcessW / JobObjects"]
        end

        subgraph Infrastructure["基础设施层"]
            Tokio["tokio<br/>异步运行时"]
            IPC["Named Pipe (同步 Win32)<br/>mpv IPC + wp-proc IPC"]
            Config["serde + toml + notify<br/>配置管理"]
            Logging["tracing<br/>日志系统"]
            Error["thiserror<br/>错误处理"]
        end
    end

    subgraph External["外部依赖"]
        MpvExe["mpv.exe<br/>外部视频播放器"]
        WebView2RT["WebView2 Runtime<br/>系统预装"]
    end

    UI --> TS
    TS --> TauriCore
    TauriCore --> WebView2
    TauriCore --> TrayIcon
    TauriCore --> AppCore

    AppCore --> Players
    AppCore --> SystemAPI
    AppCore --> Infrastructure

    MpvPlayer --> MpvExe
    WebPlayer --> WebView2RT
    MpvPlayer --> DesktopAPI
    GifPlayer --> DesktopAPI
    WebPlayer --> DesktopAPI
```

> 说明：早期文档中出错的基础设施"错误处理（thiserror + anyhow）"已更正为仅 `thiserror`；`anyhow` / `bitflags` / `softbuffer` / `raw-window-handle` 均为虚构依赖，未引入。

***

## 2. 与 Lively Wallpaper 技术栈对比

| 维度 | Lively Wallpaper | MirrorStar Wallpaper |
|------|-----------------|-------------------|
| **语言** | C# (.NET Framework 4.7.2) | Rust (stable, 1.80) |
| **UI 框架** | WPF + MahApps.Metro | Tauri v2 + WebView2 |
| **视频播放** | MediaFoundation / DirectShow / mpv | mpv.exe 子进程 |
| **GIF 播放** | XamlAnimatedGif / DirectShow | image crate + GDI 双缓冲 |
| **网页渲染** | CefSharp (Chromium) | WebView2 (Chromium, wp-proc 子进程) |
| **配置格式** | JSON (Newtonsoft.Json) | TOML (serde + toml) |
| **日志** | NLog | tracing |
| **进程监控** | DispatcherTimer 轮询 | SetWinEventHook 事件驱动 |
| **IPC** | stdin/stdout | Windows 命名管道（两套协议） |
| **音频控制** | Core Audio COM | WASAPI (windows-rs Core Audio) |
| **系统托盘** | NotifyIcon (WinForms) | Tauri 2 tray-icon feature |
| **运行时依赖** | .NET Framework 4.7.2 | 无（静态链接） |
| **GC 暂停** | 有 | 无 |

> **看门狗说明**：Lively 使用看门狗进程监控壁纸子进程。MirrorStar 无看门狗，主进程崩溃时依赖作业对象与操作系统清理子进程（mpv.exe / mirrorstar-wp-proc.exe），不引入额外监控进程。

### 关键优势总结

1. **零运行时依赖**：无需 .NET Framework，无需安装任何运行时
2. **低内存占用**：Rust 无 GC，壁纸播放更流畅；静态图片走 Native 路径零资源
3. **事件驱动**：`SetWinEventHook` 替代轮询，全屏检测更及时
4. **双向 IPC**：命名管道替代 stdin/stdout，通信更可靠
5. **轻量网页渲染**：WebView2 替代 CefSharp，无需捆绑 ~200MB CEF

***

## 3. 依赖风险与缓解

### mpv.exe 外部依赖

| 风险 | 说明 | 缓解措施 |
|------|------|----------|
| **可执行文件缺失** | mpv.exe 需捆绑或位于 PATH | `find_mpv()` 优先查找捆绑资源，回退 PATH；未找到返回错误 |
| **许可证** | mpv 使用 GPL-2.0 | mpv.exe 作为独立外部进程调用（非链接），不影响主应用许可证 |
| **版本兼容** | mpv IPC 协议可能变更 | 锁定兼容版本随应用更新；使用 mpv 稳定属性接口 |

### WebView2 Runtime

| 风险 | 说明 | 缓解措施 |
|------|------|----------|
| **运行时缺失** | 极少数旧版系统未安装 | Windows 10 (2004+) / Windows 11 已预装；缺失时引导安装 |
| **进程隔离** | 独立进程，崩溃不影响主应用 | 已是优势，无需额外处理 |

### windows-rs

| 风险 | 说明 | 缓解措施 |
|------|------|----------|
| **API 覆盖** | 部分 API 可能未覆盖 | workspace 锁定 0.58，按需启用 feature |
| **API 稳定性** | windows-rs 仍可能变更 API | 锁定版本，关注 changelog；跨 minor 升级需评估（如 webview2-com 0.38 依赖 windows 0.61 的不兼容已评估为 Low 风险不升） |
| **COM 交互** | 部分 COM 接口较复杂 | 封装为 Rust 风格安全 API |

### image crate

| 风险 | 说明 | 缓解措施 |
|------|------|----------|
| **格式支持** | 高动态范围 / 少用格式 | `default-features=false` 仅启用实际格式（png/jpeg/gif/bmp/webp/tiff），减体积与编译时间 |

### 依赖更新策略

- **锁定策略**：workspace 依赖采用 minor 精确版本（如 tokio 1.52、tauri 2.11），避免 dependabot 在 minor 内引入 breaking change
- **可复现构建**：`Cargo.lock` 纳入版本控制
- **最小依赖**：仅引入必要依赖；已移除 `protocol-asset` 等无用 feature 与虚构依赖

***

> **文档维护说明**：本技术栈文档随项目演进持续更新。任何技术选型变更需经评审并同步更新本文档。

**相关章节：** [← 总览](./01-技术栈总览-Tech-Stack-Overview.md) | [UI 框架](./02-UI框架-UI-Framework.md) | [Windows 系统 API](./03-Windows系统API-Windows-System-API.md) | [壁纸渲染](./04-壁纸渲染-Wallpaper-Rendering.md) | [基础设施](./05-基础设施-Infrastructure.md)