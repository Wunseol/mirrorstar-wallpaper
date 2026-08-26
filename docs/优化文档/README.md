# MirrorStar Wallpaper 优化文档

> 文档版本：6.0 | 最新审计日期：2026-07-25

本目录是 MirrorStar Wallpaper 项目的优化文档集，由原 `项目优化计划.md`（485KB / 5000+ 行）按模块重构而来。文档集整合了 v3.0→v3.5 共 5 轮审计修复记录（精简为表格）与 Phase 1 新发现的 125 项 findings（v4.0）、77 项性能 findings（v5.0）、201 项技术债 findings（v6.0）。

## v4.0 审计概要

- **Phase 1 深度代码审查**：125 findings（0 Critical / 18 High / 46 Medium / 61 Low）
- **Phase 2 文档审计**：18 偏差
- **审查范围**：23,199 行 Rust + 3,891 行 TypeScript + 8,653 行文档
- **审查维度**：架构设计 / 代码逻辑 / 并发安全 / 资源管理 / 错误处理 / 性能 / 安全 / 可维护性

### 关键观察

1. **零 Critical**：项目经过 5 轮修复后，无 Critical 级问题，基础安全稳固
2. **资源管理是重点**：18 个 High 中 6 个与资源泄漏相关（HANDLE/COM/线程/句柄）
3. **修复一致性不足**：多个同类问题在不同模块的修复策略不统一（速度校验、崩溃检测、错误处理等）
4. **测试副作用**：desktop 模块 4 个测试会修改系统壁纸，影响 CI 可靠性
5. **文档偏差**：audio 模块 `refresh_session_manager` 被文档标记为已实现但实际从未调用
6. **构建优化空间**：缺少 release profile 优化（LTO/strip）、tokio full feature 冗余

### v3.0→v3.5 修复质量评估

- **修复质量良好**：T05/T06/T08/T09/T12/T14/T15/T16（src-tauri 8 项）、C11 原子性测试、WP01 OwnedHandle RAII、WP02 限流读取、前端 F05/F09/F12、构建 CI 全流程覆盖等
- **修复引入的新问题**：C-001（N-005 save_mutex 引入 dirty 竞态）、C-002（C06 解压炸弹防护位置错误）、W-002（W07 未同步到 video.rs）、D-001（D02 测试副作用未处理）
- **修复不一致**：速度校验（gif.rs 有 / video.rs 无）、崩溃检测（web.rs 有 / video.rs 无）、请求序号取消（refreshWallpaperList 有 / updatePlaybackButtons 无）、错误处理（SetWindowPos 返回 Err / SetPosition 仅 warn）

## v6.0 技术债审查

> 审查日期：2026-07-25 | 聚焦技术债层面（死代码 / 冗余抽象 / 修复痕迹等），不重复 v4.0/v5.0 已覆盖的正确性与性能问题。

- **审查范围**：~83 个源文件，跨 8 个模块（config / desktop / wallpaper / audio-ipc-process / src-tauri / wp-proc / 前端 / 构建基础设施）
- **技术债总数**：201 项（P0 ~98 / P1-P2 ~80 / P3 ~23）
- **9 类技术债维度**：死代码 31 / 冗余抽象 13 / 重复实现 32 / 过时模式 9 / 未使用导入 2 / 过度设计 12 / 修复痕迹 56 / 命名一致性 20 / 注释陈旧 26
- **关键发现**：修复痕迹是最大类别（28%），多轮修复累积的历史标记散落全项目；跨模块死代码识别（`anyhow` 工作区依赖、`mockall` dev-dependency 等依赖级死代码）

### v6.0 文档集

| 文档 | 内容 | 技术债数 |
|------|------|----------|
| [v6-技术债审查/00-总览与路线图](./v6-技术债审查/00-总览与路线图.md) | 全项目技术债汇总、分级矩阵、Wave v6-A/B/C/D 实施计划 | 201 |
| [v6-技术债审查/01-config模块](./v6-技术债审查/01-config模块.md) | config 模块（manager / settings / hot_reload / detect / thumbnail） | 32 |
| [v6-技术债审查/02-desktop模块](./v6-技术债审查/02-desktop模块.md) | desktop 模块（worker_w / native_wallpaper / window / mod） | 26 |
| [v6-技术债审查/03-wallpaper模块](./v6-技术债审查/03-wallpaper模块.md) | wallpaper 模块（管理 / 渲染 / 子进程三类，13 文件） | 19 |
| [v6-技术债审查/04-audio-ipc-process模块](./v6-技术债审查/04-audio-ipc-process模块.md) | audio / ipc / process 三子模块合并文档 | 30 |
| [v6-技术债审查/05-src-tauri应用层](./v6-技术债审查/05-src-tauri应用层.md) | commands / platform / state / lib.rs / main.rs | 24 |
| [v6-技术债审查/06-wp-proc子进程](./v6-技术债审查/06-wp-proc子进程.md) | mirrorstar-wp-proc crate 全部源文件 | 15 |
| [v6-技术债审查/07-前端](./v6-技术债审查/07-前端.md) | src/scripts/ 与 src/styles/ | 25 |
| [v6-技术债审查/08-构建基础设施](./v6-技术债审查/08-构建基础设施.md) | Cargo.toml / package.json / vite/vitest/eslint 配置 / CI / tauri.conf.json | 30 |

## 模块文档

| 模块 | 文档 | Findings | 严重级别分布 |
|------|------|----------|--------------|
| 架构总览 | [01-架构总览.md](./01-架构总览.md) | — | — |
| config | [02-config模块.md](./02-config模块.md) | 18 | 0C / 3H / 6M / 9L |
| desktop | [03-desktop模块.md](./03-desktop模块.md) | 15 | 0C / 2H / 5M / 8L |
| wallpaper | [04-wallpaper模块.md](./04-wallpaper模块.md) | 13 | 0C / 4H / 5M / 4L |
| audio / ipc / process | [05-audio-ipc-process模块.md](./05-audio-ipc-process模块.md) | 22 | 0C / 3H / 7M / 12L |
| src-tauri 应用层 | [06-src-tauri应用层.md](./06-src-tauri应用层.md) | 17 | 0C / 2H / 6M / 9L |
| wp-proc 子进程 | [07-wp-proc子进程.md](./07-wp-proc子进程.md) | 13 | 0C / 3H / 4M / 6L |
| 前端 UI | [08-前端.md](./08-前端.md) | 12 | 0C / 0H / 6M / 6L |
| 构建与基础设施 | [09-构建基础设施.md](./09-构建基础设施.md) | 15 | 0C / 1H / 7M / 7L |
| **实施路线图** | [10-实施路线图.md](./10-实施路线图.md) | — | v4.0 Wave 1/2/3 优先级矩阵 + 依赖关系 + 并行计划 |
| **合计** | — | **125** | **0C / 18H / 46M / 61L** |

> 注：audio 6 项 / ipc 9 项 / process 7 项，合并为 audio-ipc-process 模块文档。

## 附录

- [附录A-已修复问题汇总](./附录A-已修复问题汇总.md) — v1.0→v3.5 已修复 findings 的精简表格（231 项，按版本轮次组织，含汇总统计与 v4.0 回退关联）
- [附录B-版本历史](./附录B-版本历史.md) — v1.0→v3.5.3 版本变更日志合并（11 个版本段，含版本总览表）
- [附录C-跨模块一致性规范](./附录C-跨模块一致性规范.md) — v4.0 Wave 3I 跨模块一致性 findings [Consistency]-12.3~12.6 的约定文档（RAII 命名、错误处理策略、锁中毒策略、测试命名）

## 版本历史概要

| 版本 | 日期 | 主要内容 |
|------|------|----------|
| v3.0 | 2026-07-03 | 全项目代码复审，数据/状态勘误，新增 WP-001 安全发现 |
| v3.1 | 2026-07-04 | 修复 v3.0 审查发现的 24 项问题（WP-001 安全、C-112/C-113/C-107 遗留、N-001~N-010、WP-002~WP-010、assetProtocol.scope 收紧） |
| v3.2 | 2026-07-04 | 深度审查 src-tauri 与前端，修复 33 个问题（ST-001~ST-018 + FE-001~FE-015），Bug #3 修复 |
| v3.3 | 2026-07-05 | 清理 v3.2 遗留的 6 个 TODO 项（ST-014~017 + WP-008 + W-006），项目达到"零已知 TODO"状态 |
| v3.4 | 2026-07-07 | 修复壁纸卡片预览空白的 5 个复合根因（14 项修复，含 Video 缩略图生成、损坏文件清理、4 分支渲染） |
| v3.5 | 2026-07-12 | 完成 10 模块 8 维度深度审计，新增 100 项 findings（0C / 7H / 36M / 57L） |
| v3.5.1 | 2026-07-12 | 修复 7 项 High 级 findings（C01/D01/I02/T02/WP01/WP02/WP03） |
| v3.5.2 | 2026-07-13 | 修复 36 项 Medium 级 findings |
| v3.5.3 | 2026-07-14 | 修复 57 项 Low 级 findings（含 review 后 23 项返工），v3.5 深度审计 100 项 findings 全部修复完成 |
| v4.0 | 2026-07-15 | 全项目深度代码复审 + 文档重构（本文档集），新增 125 项 findings |
| v5.0 | 2026-07-23 | 深度性能优化审查，77 项性能 findings（29 项已实施 / 3 项评估后维持 / 1 项长期 / 40 项 P3 保持现状） |
| v6.0 | 2026-07-25 | 技术债深度审查，产出 9 份模块文档与路线图，201 项技术债（P0 ~98 / P1-P2 ~80 / P3 ~23） |

## 文档导航

- 各模块文档顶部均有 `← 返回索引` 链接
- 已修复 findings 以精简表格呈现（详见附录 A），保留 ID + 状态用于追溯
- v4.0 新 findings 在各模块文档中提供详情（含代码位置、描述、建议）
- v5.0 性能 findings 详见 [10-实施路线图.md](./10-实施路线图.md) §10.9
- v6.0 技术债 findings 详见 [v6-技术债审查/](./v6-技术债审查/) 目录下 9 份文档
- 原始完整文档（v3.5 版本，含 vibecoding 提示词等历史内容）保留为 `项目优化计划.md`（已归档）
