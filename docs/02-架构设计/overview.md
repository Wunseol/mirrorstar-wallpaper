[← 返回文档索引](../README.md) > [架构设计](./overview.md)

# MirrorStar Wallpaper（镜星壁纸）架构设计 — 架构概述

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 创建日期 | 2026-06-10                |
| 文档状态 | 初稿                        |

***

## 1. 架构概述

### 1.1 架构哲学

MirrorStar Wallpaper 的架构设计遵循 **"少即是多"** 的哲学——通过精简功能集、选择高效的技术方案、采用事件驱动模型，实现极致的轻量化与高性能。架构设计的每一个决策都围绕以下核心问题展开：**如何在提供完整动态壁纸体验的同时，将系统资源占用降到最低？**

与参考实现 Lively Wallpaper（C# WPF）相比，MirrorStar 的架构在以下关键方面进行了根本性重构：

| 维度   | Lively Wallpaper              | MirrorStar Wallpaper  |
| ---- | ----------------------------- | -------------------- |
| 进程监控 | DispatcherTimer 轮询（500ms）     | SetWinEventHook 事件驱动 |
| 壁纸类型 | 9 种（含 Unity/Godot/模拟器等）       | 4 种（视频/GIF/网页/静态图），静态图支持 Native/WorkerW 双路径 |
| IPC  | CefSharp stdin/stdout，其他无     | 两套独立命名管道协议（mpv 原生 + wp-proc 自定义）             |
| 配置格式 | JSON（Newtonsoft.Json）         | TOML（serde）          |
| 内存安全 | GC 管理，存在泄漏风险                  | Rust 所有权模型，编译期保证     |
| 进程模型 | 主进程 + livelySubProcess + 外部进程 | 主进程 + 壁纸子进程（无看门狗进程，watchdog 已移除，依赖 OS 自动回收子进程）  |

### 1.2 设计原则

#### 1.2.1 轻量化（Lightweight）

* **功能精简**：仅保留视频、GIF、网页、静态图片四种壁纸类型，去除 Unity/Godot/模拟器/YouTube/壁纸创建器等非核心功能

* **依赖最小化**：无 .NET Runtime 依赖，仅依赖系统自带的 WebView2 Runtime 和随程序分发的外部 mpv.exe

* **体积控制**：目标二进制 < 10MB，单文件部署

* **静态壁纸零资源**：JPG/JPEG/PNG/BMP/TIF/TIFF/DIB 使用 Windows 原生壁纸 API（`SystemParametersInfoW`），无需创建窗口、线程或 GDI 对象

* **WebView2 懒创建**：主窗口不在启动时创建，仅创建系统托盘图标；窗口关闭时隐藏（hide），保留 WebView2 实例

#### 1.2.2 高性能（High Performance）

* **事件驱动**：以 SetWinEventHook 替代轮询，暂停态 CPU 占用目标为 0%

* **零成本抽象**：利用 Rust 的 trait 系统实现多态，编译期单态化，无虚函数表开销

* **硬件加速**：视频播放利用 GPU 硬件解码，GIF 渲染利用 CPU 软件渲染

* **PauseSender 快速通道**：暂停/恢复/音量/静音操作绕过引擎 Mutex，通过 `tokio::sync::mpsc::UnboundedSender<PauseCommand>` 直接发送到渲染器线程，消除锁竞争

* **WorkerW 异步初始化**：`DesktopIntegrator::new()` 通过后台线程预初始化 WorkerW，不阻塞主进程启动

#### 1.2.3 内存安全（Memory Safety）

* **所有权模型**：Rust 编译器在编译期保证无数据竞争、无悬垂指针、无缓冲区溢出

* **无 GC**：确定性析构，无垃圾回收停顿

* **进程隔离**：壁纸渲染在独立子进程中运行，崩溃不影响主进程

#### 1.2.4 模块化（Modularity）

* **Trait 抽象**：壁纸后端通过 `WallpaperRenderer` trait 统一抽象，便于扩展

* **松耦合**：模块间通过消息传递和事件总线通信，减少直接依赖

* **可测试性**：核心逻辑与平台 API 解耦，便于单元测试

***

**相关文档：**

- [系统架构图](./system-architecture.md)
- [模块设计](./module-design.md)
- [进程架构](./process-architecture.md)
