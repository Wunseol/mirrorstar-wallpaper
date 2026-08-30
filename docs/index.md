# MirrorStar Wallpaper（镜星壁纸）项目文档

基于 Rust + Tauri v2 的轻量高性能 Windows 动态壁纸软件。

> 文档基于真实代码审计，已整合原根目录项目文档和架构对比文档内容，实现文档单一来源。

## 文档目录

### [01-需求文档](./01-需求文档/项目概述-Overview.md)

| 文档 | 说明 |
|------|------|
| [项目概述](./01-需求文档/项目概述-Overview.md) | 项目背景、愿景、核心目标 |
| [功能性需求](./01-需求文档/功能性需求-Functional-Requirements.md) | FR-001 ~ FR-044 功能需求规格 |
| [非功能性需求](./01-需求文档/非功能性需求-Non-Functional-Requirements.md) | NFR-001 ~ NFR-020 性能、资源、可靠性需求 |
| [用例](./01-需求文档/用例-Use-Cases.md) | 系统用例图 + UC-001 ~ UC-017 用例描述 |
| [约束与术语](./01-需求文档/约束与术语-Constraints-and-Terminology.md) | 约束条件、假设、术语表 |

### [02-架构设计](./02-架构设计/架构概述-Architecture-Overview.md)

| 文档 | 说明 |
|------|------|
| [架构概述](./02-架构设计/架构概述-Architecture-Overview.md) | 架构哲学、设计原则、与 Lively 对比 |
| [系统架构](./02-架构设计/系统架构-System-Architecture.md) | 分层架构图、层次职责说明 |
| [模块设计](./02-架构设计/模块设计-Module-Design.md) | 10 个模块实际状态（完成度/代码行数/已实现功能）+ Lively 模块对比 |
| [依赖与数据流](./02-架构设计/依赖与数据流-Dependency-and-Data-Flow.md) | 模块依赖图、数据流图、通信机制对比 |
| [进程架构](./02-架构设计/进程架构-Process-Architecture.md) | 混合进程架构、两套 IPC 协议、Lively 进程模型对比 |
| [桌面集成](./02-架构设计/桌面集成-Desktop-Integration.md) | WorkerW 嵌入、原生壁纸 API、窗口样式、多显示器、DPI |
| [暂停/恢复机制](./02-架构设计/暂停恢复机制-Pause-Resume.md) | 事件驱动全屏检测、PauseSender 快速通道、电池事件驱动 |
| [错误处理](./02-架构设计/错误处理-Error-Handling.md) | 错误传播、崩溃恢复、优雅降级 |
| [性能优化](./02-架构设计/性能优化-Performance.md) | 优化策略、Lively 性能对比、API 参考 |

### [03-技术栈](./03-技术栈/技术栈总览-Tech-Stack-Overview.md)

| 文档 | 说明 |
|------|------|
| [技术栈总览](./03-技术栈/技术栈总览-Tech-Stack-Overview.md) | 依赖总览表、核心语言与运行时 |
| [UI 框架](./03-技术栈/UI框架-UI-Framework.md) | Tauri v2 选型与前端方案 |
| [Windows 系统 API](./03-技术栈/Windows系统API-Windows-System-API.md) | windows-rs 绑定、Core Audio、COM 初始化 |
| [壁纸渲染](./03-技术栈/壁纸渲染-Wallpaper-Rendering.md) | 图片/GIF/视频（mpv.exe）/网页（WebView2）壁纸技术方案 |
| [基础设施](./03-技术栈/基础设施-Infrastructure.md) | 配置、日志、两套 IPC 协议、异步、序列化、错误处理 |
| [风险评估](./03-技术栈/风险评估-Risk-Assessment.md) | 技术栈全景图、Lively 对比、依赖风险 |

### [04-实施规划](./04-实施规划/实施规划总览-Implementation-Overview.md)

| 文档 | 说明 |
|------|------|
| [实施规划总览](./04-实施规划/实施规划总览-Implementation-Overview.md) | 总体策略、阶段依赖、当前进度总结、改进方向 |
| [开发环境搭建](./04-实施规划/开发环境搭建-Development-Environment.md) | 环境搭建指南、常用命令 |
| [开发阶段划分](./04-实施规划/开发阶段划分-Development-Phases.md) | 基于真实缺口的实施计划（前端 UI 补全/架构优化/打包发布） |
| [项目目录结构](./04-实施规划/项目目录结构-Project-Structure.md) | 实际目录结构（3 个 workspace 成员） |
| [甘特图](./04-实施规划/甘特图-Gantt-Chart.md) | 时间线与里程碑 |
| [质量保障](./04-实施规划/质量保障-Quality-Assurance.md) | 验收标准、测试策略、CI 流程 |

### [优化文档](./优化文档/项目优化计划.md)

| 文档 | 说明 |
|------|------|
| [项目优化计划](./优化文档/项目优化计划.md) | 架构优化路线图、已修复问题记录、变更日志（v3.0） |

### [测试报告](./测试报告/壁纸性能与资源占用测试报告.md)

| 文档 | 说明 |
|------|------|
| [壁纸性能与资源占用测试报告](./测试报告/壁纸性能与资源占用测试报告.md) | 壁纸渲染性能、CPU/内存/GPU 资源占用实测数据 |