# MirrorStar Wallpaper（镜星壁纸）实施规划 — 实现概述

[← 返回文档索引](../README.md) > [实施规划](./overview.md)

| 项目 | 内容 |
|------|------|
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0 |
| 创建日期 | 2026-06-10 |
| 文档状态 | 已实现 |

---

## 1. 实现概述

### 1.1 总体实现策略

MirrorStar Wallpaper 采用**增量式开发**策略，将整个项目划分为 5 个开发阶段，每个阶段都交付一个**可运行的完整产品**。前一阶段的交付物是后一阶段的基础，确保项目始终处于可验证、可演示的状态。

核心原则：

- **每个里程碑交付可运行程序**：不是交付半成品模块，而是交付能跑、能看、能用的最小完整产品
- **风险前置**：将技术风险最高的部分（桌面集成、视频播放）放在最前面的阶段，尽早验证可行性
- **垂直切片**：每个阶段实现从底层到 UI 的完整垂直功能切片，而非水平分层实现
- **持续集成**：从第一天起建立 CI/CD 流水线，确保每次提交都可编译、可测试

### 1.2 开发方法论

采用**迭代式开发**方法，每个阶段为一个迭代周期：

- 每个阶段有明确的**里程碑（Milestone）**和**验收标准**
- 阶段内部采用短周期迭代（1-2 周一个小迭代）
- 每个小迭代结束时进行代码审查和功能验证
- 阶段结束时进行里程碑评审，确认验收标准达标后进入下一阶段

### 1.3 阶段依赖关系

```mermaid
graph LR
    P3[P3: Web 壁纸子进程 ✅] --> P4[P4: 体验优化 ✅]
    P4 --> P1[P1: 前端 UI 补全]
    P1 --> P2[P2: 架构优化与清理]
    P2 --> P5[P5: 打包发布]

    style P3 fill:#4CAF50,color:#fff
    style P4 fill:#4CAF50,color:#fff
    style P1 fill:#FF9800,color:#fff
    style P2 fill:#2196F3,color:#fff
    style P5 fill:#F44336,color:#fff
```

> **说明**：Phase 3（Web 壁纸子进程）和 Phase 4（体验优化）已完成。当前优先级为 Phase 1（前端 UI 补全），随后进行 Phase 2（架构优化与清理），最后 Phase 5（打包发布）。

各阶段的详细任务划分参见 [开发阶段划分](./phases.md)，开发环境搭建参见 [开发环境搭建指南](./dev-environment.md)。

---

## 2. 当前进度总结

> 基于真实代码审计（2026-06-18），反映项目当前实际状态。

### 2.1 已完成

| 模块 | 完成度 | 说明 |
|------|--------|------|
| 四种壁纸类型端到端 | ✅ 100% | 图片/GIF/视频/网页（拖拽→添加→设置→显示→退出恢复）完整流程 |
| 桌面集成 | ✅ 100% | WorkerW 三重查找嵌入、原生壁纸 API 双路径、Explorer 重启恢复、显示器枚举 |
| 渲染优化 | ✅ 100% | HALFTONE、双缓冲、专用线程消息循环、GIF 内存预算、图片降采样、暂停释放资源 |
| 全屏检测 | ✅ 100% | SetWinEventHook 事件驱动、状态去抖、自身窗口排除 |
| 音量控制 | ✅ 95% | WASAPI 进程级、PauseSender 快速通道 |
| IPC 通信 | ✅ 100% | mpv 命名管道 + Web 子进程命名管道（WpProcIpcClient） |
| 进程管理 | ✅ 95% | ProcessManager（阶段 2 已清理 watchdog/monitor 死代码） |
| 配置管理 | ✅ 100% | TOML + 热重载 + 原子写入 + 缩略图生成 |
| Tauri 应用层 | ✅ 95% | 24 命令 + 托盘 + 单实例 + DPI 感知 + 电源监控 |
| Web 壁纸子进程 | ✅ 100% | wp-proc 子进程（3144 行）+ WpProcIpcClient + WebRenderer 代理层重构 |

### 2.2 待完成

| 模块 | 完成度 | 说明 |
|------|--------|------|
| 前端 UI 补全 | ✅ 95% | 已完成壁纸预览/搜索/响应式/电池暂停/事件监听/拖拽反馈/删除确认；剩余：手动暂停/恢复按钮、播放速度、鼠标交互模式、删除源文件、静音按钮 |
| 打包发布 | ⬜ 0% | CSP、MSI/NSIS 打包、WebView2 Runtime 依赖、mpv.exe 分发 |

### 2.3 已知问题

> 以下问题均已修复，保留记录供历史追溯：

- ✅ `src-tauri/src/lib.rs` 中 2 个未使用的 OnceLock（FULLSCREEN_ENGINE, FULLSCREEN_RT）为 write-only 死代码（已修复，OnceLock 已移除并收敛为 `SHARED_ENGINE` / `SHARED_CONFIG` 等）
- ✅ `main.rs` 中 tracing 在 logging 初始化前调用，日志会丢失（已修复，main.rs 已无 tracing 调用，仅 `ensure_single_instance` + `run()`）
- ✅ 前端 `set_volume` 调用缺少 `displayId` 参数（已修复，`src/scripts/ipc.ts` 已统一 `displayId: displayId || null` 转换）

---

## 3. 改进方向

> 基于架构对比文档（与 Lively Wallpaper 对比）识别的改进方向，按优先级分级。

### 3.1 高优先级

1. **前端 UI 补全**：添加手动暂停/恢复按钮、播放速度控制、鼠标交互 UI 等缺失的前端功能
2. **精细鼠标交互**：参考 Lively 的 RawInputDX，实现 RawInput 全局鼠标捕获 + PostMessage 转发，支持桌面判断，解决交互模式下桌面图标不可点击的问题
3. **全屏检测增强**：添加面积比判定（>95% 屏幕），添加应用规则支持，添加多显示器感知暂停
4. **Explorer 重启检测升级**：TaskbarCreated 消息监听已实现，保留 5 分钟轮询作为兜底方案

### 3.2 中优先级

5. **Win7 兼容**：添加 Progman 直接嵌入的回退方案
6. **高对比度模式**：检测系统高对比度模式，切换到 bottom-most 渲染
7. **duplicate 显示器模式**：支持同壁纸在多显示器上重复显示
8. **应用规则系统**：实现 pause/ignore/kill 规则系统
9. **多显示器感知暂停**：仅暂停全屏显示器上的壁纸
10. **电池供电暂停 UI**：添加电池供电时自动暂停的 UI 开关（后端已实现 GetSystemPowerStatus）
11. **DPI 完善**：参考 Lively 的 DpiHelper，完善 DPI 缩放处理
12. **显示器变更事件**：监听 `SystemEvents.DisplaySettingsChanged` 事件自动重新布局

### 3.3 低优先级

13. **更多壁纸类型**：支持 Unity/Godot 等外部程序壁纸
14. **国际化**：支持多语言
15. **壁纸预览**：点击放大预览（✅ 已完成）
16. **壁纸搜索**：壁纸库搜索功能（✅ 已完成）
