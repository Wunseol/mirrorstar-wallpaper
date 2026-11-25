[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 质量保障

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-11-24                |
| 文档状态 | 已实现（基于真实代码审计与 2026-11-24 真机补测）        |

> 本文由原《质量保障》改写，聚焦质量目标、真实测试命令与验收标准；历史测试数据与实测值见 [测试报告](./05-测试报告-Test-Reports.md)，2026-11-24 补测的过程与结论仅记录在本文 §4.4。

***

## 1. 质量目标

项目以**零警告、可测试、可发布**为核心质量目标，保障手段落在「本地工具链 + 单元/集成测试 + e2e 脚本」多层。

- **Rust 侧**：`cargo test / clippy / fmt` 全部零告警（clippy 用 `-D warnings`）。
- **依赖安全**：`cargo audit` 兜底。
- **e2e**：PowerShell 驱动脚本（`crates/mirrorstar-ui/`）做真机内存/CPU/交互冒烟验证。

> 原前端质量门槛（TypeScript 类型检查、ESLint、Vitest、Vite 构建、npm audit）已随前端删除而取消。

## 2. 本地质量工具（命令）

### Rust（workspace）

| 命令 | 用途 |
|------|------|
| `cargo check --workspace` | 编译检查（不含测试目标） |
| `cargo test --workspace` | 运行全部单元/集成测试（默认跳过 `#[ignore]`） |
| `cargo test --workspace -- --ignored` | 运行共享资源类串行测试 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint，零警告 |
| `cargo fmt --all --check` | 格式检查 |
| `cargo audit` | 依赖安全审计 |

### e2e（PowerShell 驱动脚本，`crates/mirrorstar-ui/`）

| 脚本 | 用途 |
|------|------|
| `measure_grid.ps1` | 网格内存/CPU 测量 |
| `measure_scroll_reverify.ps1` | 滚动性能复测 |
| `measure_preview.ps1` | 四类预览测量 |
| `keyboard_smoke.ps1` | 键盘交互冒烟 |
| `shell_measure.ps1` | 壳内存测量 |
| `single_instance_check.ps1` | 单实例校验 |

## 3. 质量门（本地手工执行）

项目不使用 CI，全部检查在本地手工执行。提交前依次运行以下命令（零警告为准）：

| 命令 | 说明 |
|------|------|
| `cargo check --workspace` | 编译检查 |
| `cargo test --workspace` | 全部单元/集成测试（隐含编译） |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint，零警告 |
| `cargo fmt --all --check` | 格式检查 |
| `cargo test --workspace -- --ignored` | 共享资源类串行测试（本地按需执行） |
| `cargo audit` | 依赖安全审计 |

> 打包与发布由本地执行 `cargo tauri build` 生成 NSIS 安装包后人工发布。

## 4. 测试体系与覆盖

### 4.1 Rust 单元测试

`cargo test --workspace` 覆盖全部四个成员的内联 `#[cfg(test)]` 单元测试（`mirrorstar-core` / `mirrorstar-ui` / `mirrorstar-wp-proc` / `mirrorstar-wallpaper` 壳）。部分共享资源类测试（如涉及 mpv 后端）标记 `#[ignore]`，由 `--ignored` 串行执行。

### 4.2 Rust 集成测试

| 位置 | 文件 | 覆盖内容 |
|------|------|----------|
| `src-tauri/tests/` | `wallpaper_flow.rs` | 壁纸生命周期流程 |
| `src-tauri/tests/` | `config_flow.rs` | 配置读改写流程 |
| `src-tauri/tests/` | `common/mod.rs` | 测试公共工具 |
| `crates/mirrorstar-core/tests/` | `audio_integration.rs` | 音频集成 |
| `crates/mirrorstar-core/tests/` | `ipc_timeout.rs` | IPC 超时与容错 |
| `crates/mirrorstar-core/tests/` | `video_wallpaper_fullpath.rs` | 视频壁纸全路径 |

### 4.3 原生 UI / e2e

`crates/mirrorstar-ui/` 的单元/集成测试纳入 `cargo test --workspace`；真机端到端验证由 PowerShell 驱动脚本完成（见 §2 脚本表）。

> **历史记录**：原前端单测（`src/scripts/**/*.test.ts`）通过 `npm run test`（Vitest + coverage）执行；`src-tauri/tests/csp_guard.rs`（CSP 策略守卫）针对已移除的 WebView2 主窗口。二者均已随前端/主窗口重构删除。

### 4.4 2026-11-24 真机补测方法与结果

本轮针对主窗口原生 UI 点击、mpv 视频控制、托盘与 GPU 指标做真机补充验证，均由临时 PowerShell 脚本驱动，未新增入库 e2e 脚本。

**方法**

- **UI 点击**：调用 `EnumWindows` 取目标进程画面面积最大的可见窗口，再按窗口相对坐标定位并点击，不依赖屏幕绝对坐标。
- **暂停/恢复**：以「鼠标释放 → mpv `pause` 属性事件」的时间差为准，即从输入动作到播放器状态真正生效的完整链路耗时。
- **托盘**：右键任务栏溢出面板中的托盘图标，以 `#32768`（托盘上下文菜单）窗口变为可见作为成功与计时的判据。
- **GPU**：使用性能计数器 `Get-Counter '\GPU Engine(*)\Utilization Percentage'` 与 `\GPU Process Memory(*)\Dedicated Usage` 采样。

**结果**

| 项目 | 实测结果 | 判定 |
|------|----------|------|
| 图片壁纸切换（视频→5115×2826 JPG、JPG→7500×5000 PNG、再回切） | 825–1943 ms | 未达 <0.5 s 目标，待优化 |
| 视频暂停 / 恢复 | 1.7–10.9 ms / 1.9–4.3 ms | 通过 |
| 托盘菜单弹出 | 35.3–48.7 ms | 通过 |
| GPU 采样 | 可用：mpv 3D 27.77%、videodecode 22.89%、专用显存 571,658,240 B | 已采集（无门槛） |

**差距与边界**

- GIF 壁纸切换、GIF/图片场景下的暂停均未测，暂停/恢复结论仅对视频壁纸成立。
- 未执行 Release 构建的完整验收，上述数据来自开发构建的真机手动补测。
- 本轮为临时脚本补测，仓库内 §2 的 6 个 e2e 脚本未改动；若需回归固化，应另行新增脚本。

临时证据（截图、原始计数器导出等）保存在系统临时目录 `%TEMP%\mirrorstar-probe\`，不入库。

## 5. 版本与变更控制

- 依赖由 `Cargo.lock` 锁定，保证可复现构建。
- Rust 工具链由 `rust-toolchain.toml` 固定（stable / MSRV 1.80）。

## 6. 验收标准

各阶段验收以「可通过 §3 质量门全部检查」为最低通过线，并补充人工功能验证：

- 壁纸应用/显示/退出恢复；
- 托盘与单实例；
- 全屏处置（none / pause / terminate）；
- 电量暂停；
- 主窗口原生 UI 的网格滚动、设置、四类预览、键盘/拖放交互；
- 2026-11-24 补测项（详见 §4.4）：
  - 图片壁纸切换耗时——当前 825–1943 ms，**未达标**，需优化至 <0.5 s 后重新验证；
  - 视频暂停/恢复延迟——1.7–10.9 ms / 1.9–4.3 ms，达标；
  - 托盘菜单弹出——35.3–48.7 ms，达标；
  - GPU 引擎利用率与专用显存采样——`Get-Counter` 可正常取数，已采集（无门槛）。

***

## 相关文档

- [实施与验证总览](./01-实施与验证总览-Implementation-and-Verification-Overview.md)
- [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)
- [项目目录结构](./03-项目目录结构-Project-Structure.md)
- [测试报告](./05-测试报告-Test-Reports.md)
- [非功能性需求](../01-需求文档/03-非功能性需求-Non-Functional-Requirements.md)
- [优化演进摘要](../04-优化文档/01-优化演进摘要.md)
- [附录A-已修复问题汇总](../04-优化文档/09-附录A-已修复问题汇总.md)