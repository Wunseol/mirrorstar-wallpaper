[← 返回文档索引](../index.md) > 实施规划 > 实施规划总览

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 总览

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 实现概述

### 1.1 总体实现策略

本项目采用**增量式开发**策略，划分为 5 个开发阶段，每个阶段交付一个可运行的完整产品。各阶段交付物以前一阶段为基础，确保项目始终处于可编译、可运行状态。

核心原则：

- **每个里程碑交付可运行程序**
- **风险前置**：把技术风险最高的部分（桌面集成、视频播放）放在最前阶段尽早验证
- **垂直切片**：每个阶段实现从底层到 UI 的完整垂直功能切片
- **持续集成**：从第一天起建立 CI 流水线（`cargo test` / `clippy` / `fmt --check` / `cargo audit` / 前端 `tsc/vitest/eslint`），确保每次提交可编译可测试

### 1.2 开发方法论

迭代式开发，每个阶段为一个迭代周期，阶段内部短周期迭代，阶段结束时进行里程碑评审与验收，达标后进入下一阶段。

### 1.3 阶段依赖关系

```mermaid
graph LR
    P3[P3: Web 壁纸子进程 ✅] --> P4[P4: 体验优化 ✅]
    P4 --> P1[P1: 前端 UI 补全 ✅]
    P1 --> P2[P2: 架构优化与清理 ✅]
    P2 --> P5[P5: 打包发布 🔧 已配置]

    style P3 fill:#4CAF50,color:#fff
    style P4 fill:#4CAF50,color:#fff
    style P1 fill:#4CAF50,color:#fff
    style P2 fill:#4CAF50,color:#fff
    style P5 fill:#FF9800,color:#fff
```

> **当前状态（2026-08-29）**：Phase 1–4 均已实现完成；Phase 5（打包发布）已完成 NSIS 打包配置与 CSP/资源捆绑，并具备发布流水线。各阶段任务清单见 [开发阶段划分](./开发阶段划分-Development-Phases.md)，开发环境见 [开发环境搭建](./开发环境搭建-Development-Environment.md)。

***

## 2. 当前进度总结

> 本表基于对当前仓库源码与配置（2026-08-29）的实测，反映真实状态。

### 2.1 已完成

| 模块 | 完成度 | 说明 |
|------|--------|------|
| 四种壁纸类型端到端 | ✅ 100% | 图片 / GIF / 视频 / 网页（添加→设置→显示→退出恢复）完整流程 |
| 桌面集成 | ✅ 100% | WorkerW 三重查找嵌入、原生壁纸 API 双路径、Explorer 重启恢复、显示器枚举 |
| 渲染优化 | ✅ 100% | HALFTONE、双缓冲、专用线程消息循环、GIF 内存策略、图片降采样、暂停释放资源 |
| 全屏检测 | ✅ 100% | `SetWinEventHook` 事件驱动 + 状态去抖、面积比判定、自身窗口排除 |
| 音量控制 | ✅ 100% | WASAPI 进程级（mpv）+ 静音切换；无音频设备时降级 no-op |
| IPC 通信 | ✅ 100% | mpv 原生 JSON 管道 + wp-proc 自定义管道（`MpvIpcClient` / `WpProcIpcClient`） |
| 进程管理 | ✅ 100% | `ProcessManager` 子进程管理 + 崩溃检测（watchdog 死代码已清理） |
| 配置管理 | ✅ 100% | TOML + 热重载（500ms 防抖）+ 原子写入 + 校验 + 缩略图管理 |
| Tauri 应用层 | ✅ 100% | **20 个 Tauri 命令** + 托盘（3 项菜单）+ 单实例 + DPI 感知 + 电源监控 |
| Web 壁纸子进程 | ✅ 100% | `mirrorstar-wp-proc`（4 模块）+ `WpProcIpcClient` + `WebRenderer` 代理层 |
| 前端 UI 补全 | ✅ 100% | 暂停/恢复、速度、音量/静音、搜索、重新生成缩略图、电池暂停、预览/设为壁纸、拖拽、交互模式等均已实现 |
| 打包发布 | 🔧 已配置 | NSIS 安装包 + CSP + 资源（mpv.exe、wp-proc.exe）捆绑 + Release 流水线 |

### 2.2 主要源码规模（行数实测）

| 文件 | 行数 |
|------|------|
| `src-tauri/src/lib.rs` | 1314 |
| `src-tauri/src/state.rs` | 1102 |
| `src-tauri/src/commands/wallpaper.rs` | 2449 |
| `src-tauri/src/commands/system.rs` | 239 |
| `src-tauri/src/commands/config.rs` | 161 |
| `src-tauri/src/platform/fullscreen.rs` | 1239 |
| `crates/mirrorstar-core/src/config/manager.rs` | 2838 |
| `crates/mirrorstar-core/src/wallpaper/manager.rs` | 2591 |
| `crates/mirrorstar-core/src/wallpaper/gif.rs` | 2172 |
| `crates/mirrorstar-core/src/wallpaper/video.rs` | 1259 |
| `crates/mirrorstar-core/src/wallpaper/image.rs` | 1104 |
| `crates/mirrorstar-core/src/wallpaper/web.rs` | 652 |
| `crates/mirrorstar-core/src/ipc/mpv_protocol.rs` | 355 |
| `crates/mirrorstar-core/src/ipc/wp_proc.rs` | 699 |
| `crates/mirrorstar-wp-proc/src/ipc_server.rs` | 1466 |
| `crates/mirrorstar-wp-proc/src/webview.rs` | 1452 |
| `crates/mirrorstar-wp-proc/src/command.rs` | 1145 |
| 前端 `src/scripts/main.ts` | 731 |
| 前端 `src/scripts/ipc.ts` | 341 |
| 前端 `src/styles/main.css` | 699 |

> 说明：早期文档标注的 `lib.rs 310 行`、`main.ts 403 行`、`main.css 581 行`均为旧值，经多次重构后已增长；当前以本表实测为准。

***

## 3. 后续优化方向

> 以下为可选改进项（非当前阻塞项），按价值高低排序，立项时按需实施：

1. **精细鼠标交互**：参考 Lively `RawInputDX`，实现 RawInput 全局鼠标捕获 + `PostMessage` 转发，解决交互模式下桌面图标不可点击问题（当前 `set_interaction_mode` 已实现交互开关，但桌面图标点击交互尚未支持）
2. **应用规则系统**：实现 pause / ignore / kill 规则，支持按窗口标题/进程规则处置
3. **多显示器感知暂停**：仅暂停全屏显示器上的壁纸（当前全屏检测为全局暂停）
4. **Win7 兼容回退**：`Progman` 直接嵌入的回退方案
5. **高对比度模式**：检测系统高对比度并切换到 bottom-most 渲染
6. **duplicate 显示器模式**：同壁纸在多显示器上重复显示
7. **国际化**：多语言支持
8. **更多壁纸类型**：Unity / Godot 等外部程序壁纸

***

**相关章节：** [开发阶段划分](./开发阶段划分-Development-Phases.md) | [项目目录结构](./项目目录结构-Project-Structure.md) | [甘特图](./甘特图-Gantt-Chart.md) | [质量保障](./质量保障-Quality-Assurance.md)