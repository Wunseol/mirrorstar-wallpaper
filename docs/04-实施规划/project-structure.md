# MirrorStar Wallpaper（镜星壁纸）实施规划 — 项目目录结构

[← 返回文档索引](../README.md) > [实施规划](./overview.md) > [项目目录结构](./project-structure.md)

## 4. 项目目录结构

> **说明**：以下目录结构基于真实代码审计，反映项目当前实际状态。Workspace 包含 3 个成员（`src-tauri`、`crates/mirrorstar-core`、`crates/mirrorstar-wp-proc`），无 watchdog crate。核心业务模块位于 `crates/mirrorstar-core/src/`，`src-tauri/src/` 包含应用入口（`lib.rs`、`main.rs`、`state.rs`）、`commands/` 命令层与 `platform/` 平台集成子目录。

```
mirrorstar-wallpaper/
├── .cargo/
│   └── config.toml
├── crates/
│   ├── mirrorstar-core/           # 核心库
│   │   ├── src/
│   │   │   ├── audio/
│   │   │   │   ├── mod.rs
│   │   │   │   └── volume.rs     # WASAPI 音量控制
│   │   │   ├── config/
│   │   │   │   ├── mod.rs        # ConfigManager + WallpaperLibrary
│   │   │   │   ├── settings.rs   # AppConfig 结构定义
│   │   │   │   ├── detect.rs     # 配置探测
│   │   │   │   ├── hot_reload.rs # 配置热重载
│   │   │   │   └── thumbnail.rs  # 缩略图管理
│   │   │   ├── desktop/
│   │   │   │   ├── mod.rs        # DesktopIntegrator
│   │   │   │   ├── native_wallpaper.rs  # 原生壁纸 API
│   │   │   │   ├── window.rs     # 窗口样式工具
│   │   │   │   └── worker_w.rs   # WorkerW 查找与嵌入
│   │   │   ├── ipc/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── protocol.rs   # MpvIpcClient (mpv IPC)
│   │   │   │   ├── wp_proc.rs    # WpProcIpcClient (wp-proc IPC)
│   │   │   │   └── client.rs     # IPC 客户端封装
│   │   │   ├── process/
│   │   │   │   ├── mod.rs
│   │   │   │   └── manager.rs    # ProcessManager
│   │   │   ├── wallpaper/
│   │   │   │   ├── mod.rs        # WallpaperRenderer trait + 共享类型
│   │   │   │   ├── manager.rs    # WallpaperEngine
│   │   │   │   ├── image.rs      # ImageRenderer (GDI)
│   │   │   │   ├── gif.rs        # GifRenderer (GDI + Timer)
│   │   │   │   ├── video.rs      # VideoRenderer (mpv.exe)
│   │   │   │   ├── web.rs        # WebRenderer (wp-proc proxy)
│   │   │   │   ├── fast_path.rs  # 快速路径优化
│   │   │   │   ├── gdi_base.rs   # GDI 基础设施
│   │   │   │   ├── gdi_cache.rs  # GDI 资源缓存
│   │   │   │   ├── gif_decode.rs # GIF 帧解码
│   │   │   │   ├── gif_memory.rs # GIF 内存管理
│   │   │   │   ├── mode_dispatch.rs # 渲染模式分发
│   │   │   │   └── subprocess_base.rs # 子进程渲染基础
│   │   │   └── lib.rs            # 模块声明 + MirrorStarError + init_logging
│   │   └── Cargo.toml
│   └── mirrorstar-wp-proc/        # Web 壁纸子进程
│       ├── src/
│       │   ├── main.rs           # 入口：消息循环 + 生命周期
│       │   ├── webview.rs        # WebView2 接入
│       │   ├── ipc_server.rs     # 命名管道 IPC 服务端
│       │   ├── command.rs        # 命令处理
│       │   └── com.rs            # COM 初始化
│       └── Cargo.toml
├── src-tauri/                    # Tauri 应用层
│   ├── src/
│   │   ├── lib.rs                # 应用入口：Tauri 构建/托盘/插件注册
│   │   ├── main.rs               # 入口 + 单实例检测
│   │   ├── state.rs              # 应用共享状态
│   │   ├── commands/             # Tauri 命令层
│   │   │   ├── mod.rs
│   │   │   ├── config.rs         # 配置相关命令
│   │   │   ├── system.rs         # 系统相关命令
│   │   │   └── wallpaper.rs      # 壁纸相关命令
│   │   └── platform/             # 平台集成粘合层
│   │       ├── mod.rs
│   │       ├── explorer.rs       # Explorer 重启监听
│   │       ├── fullscreen.rs     # 全屏检测
│   │       ├── power.rs          # 电源事件监控
│   │       └── workerw_check.rs  # WorkerW 健康检查
│   ├── capabilities/
│   │   └── default.json
│   ├── icons/
│   ├── Cargo.toml
│   ├── build.rs
│   ├── manifest.xml              # DPI 感知 PerMonitorV2
│   └── tauri.conf.json
├── src/                          # 前端
│   ├── scripts/
│   │   ├── main.ts               # 前端逻辑 (403 行)
│   │   ├── ipc.ts                # IPC 通信封装
│   │   ├── state.ts              # 前端状态
│   │   ├── types.ts              # 类型定义
│   │   ├── ui/                   # UI 模块
│   │   │   ├── mod.ts
│   │   │   ├── wallpaper-list.ts # 壁纸列表
│   │   │   ├── preview-modal.ts  # 预览弹窗
│   │   │   ├── drag-drop.ts      # 拖拽支持
│   │   │   ├── display-list.ts   # 显示器列表
│   │   │   ├── config-panel.ts   # 配置面板
│   │   │   └── utils.ts          # UI 工具
│   │   └── utils/                # 通用工具
│   │       ├── listeners.ts      # 事件监听
│   │       └── logger.ts         # 日志
│   ├── styles/
│   │   └── main.css              # 样式 (581 行)
│   └── vite-env.d.ts
├── docs/                         # 项目文档
├── dist/                         # 前端构建产物
├── Cargo.toml                    # workspace (3 members)
├── Cargo.lock
├── package.json
├── index.html
└── rustfmt.toml
```

### 目录结构设计原则

- **Workspace 分离**：核心逻辑（`mirrorstar-core`）与 Tauri 应用（`src-tauri`）分离，核心库不依赖 UI 框架
- **子进程独立**：Web 壁纸渲染子进程（`mirrorstar-wp-proc`）作为独立二进制，实现进程隔离与崩溃隔离
- **模块化组织**：每个功能领域（desktop、wallpaper、process、audio、config、ipc）独立模块，位于 `crates/mirrorstar-core/src/` 下
- **前端分离**：Tauri 前端代码（`src/`）与后端代码（`src-tauri/`）物理隔离
- **应用层精简**：`src-tauri/src/` 仅保留 Tauri 应用入口与平台粘合层（`lib.rs` 310 行、`main.rs` 45 行、`state.rs`、`commands/`、`platform/`），核心业务逻辑下沉到 `mirrorstar-core`

### 关键说明

- **无 watchdog crate**：`mirrorstar-watchdog` 独立 crate 已在阶段 2 移除，不再需要独立看门狗进程（主进程崩溃时操作系统自动回收子进程）
- **核心模块不在 src-tauri**：核心业务模块（desktop、wallpaper、process、audio、config、ipc）均位于 `crates/mirrorstar-core/src/`；`src-tauri/src/` 下的 `commands/` 与 `platform/` 仅为 Tauri 命令与平台集成粘合层，不包含业务实现
- **视频壁纸使用外部 mpv.exe**：通过 ProcessManager 直接 spawn `mpv.exe` 子进程（按需启动），不使用 libmpv2 绑定
- **Web 壁纸子进程**：仅 Web 壁纸时启动 `mirrorstar-wp-proc.exe` 子进程，关闭 Web 壁纸即终止，释放全部内存

### 项目卫生决策

- **lively-reference/ 已加入 .gitignore**：该目录仅用于本地参考 Lively Wallpaper 项目源码，不属于本项目构建的一部分。如需在团队间共享参考代码，应改为外链（GitHub 仓库链接）或 git submodule 管理，避免将第三方源码提交到本仓库。
- **assetProtocol.scope 收紧为 `["$APPDATA/mirrorstar/**/*", "$HOME/**/*"]`**：Tauri v2 的 `assetProtocol.scope` 为静态配置，无法在运行时动态添加 dialog 返回的用户选定目录路径。前端通过 `convertFileSrc()` 访问壁纸文件和缩略图。scope 从 `["**"]`（允许任意路径）收紧为覆盖应用数据目录（缩略图存储位置）与用户主目录（壁纸文件常见存放位置）的白名单。**已知限制**：非 $HOME 路径（如 `D:\Wallpapers\`）的壁纸预览会因不在 scope 内而无法加载，但壁纸设置功能（使用原生 OS API，不经过 asset protocol）不受影响。如需支持任意路径，可考虑引入 `tauri-plugin-persisted-scope` 插件动态持久化用户选定目录，或在添加壁纸时将文件复制到 `$APPDATA/mirrorstar/wallpapers/`（方案 B，需评估磁盘占用与存量数据迁移成本）。
