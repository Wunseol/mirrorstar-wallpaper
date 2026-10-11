[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 质量保障

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-10-12                |
| 文档状态 | 已实现（基于真实代码审计，2026-10）        |

> 本文由原《质量保障》改写，聚焦质量目标、真实测试命令、CI 流程与验收标准；具体测试数据与实测值见 [测试报告](./05-测试报告-Test-Reports.md)。

***

## 1. 质量目标

项目以**零警告、可测试、可发布**为核心质量目标，保障手段落在「本地工具链 + CI 流水线 + 单元/集成测试 + e2e 脚本」多层。

- **Rust 侧**：`cargo test / clippy / fmt` 全部零告警（clippy 用 `-D warnings`）。
- **依赖安全**：`cargo audit` 兜底。
- **e2e**：PowerShell 驱动脚本（`crates/mirrorstar-ui/`）做真机内存/CPU/交互冒烟验证。

> 原前端质量门槛（TypeScript 类型检查、ESLint、Vitest、Vite 构建、npm audit）已随前端删除而取消。

## 2. 本地质量工具（命令）

### Rust（workspace）

| 命令 | 用途 |
|------|------|
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

## 3. CI 流水线（真实流程）

项目通过 `.github/workflows/checks.yml` 定义**可复用检查工作流**（`workflow_call`），供 `ci.yml`（push/PR）与 `release.yml`（tag 触发的发布前置门）共享，**仅含 `rust-checks` 单一 job**（运行于 `windows-latest`）。

### 3.1 rust-checks 步骤

| 步骤 | 说明 |
|------|------|
| Setup Rust stable | `dtolnay/rust-toolchain@stable`（components: clippy, rustfmt），兼容 MSRV 1.80 |
| Cache cargo | `Swatinem/rust-cache@v2`（key 含 Cargo.lock / Cargo.toml / runner.os） |
| Provision mpv runtime | 固定 release（20260903）+ SHA256 校验下载 `mpv.exe`（编译期资源校验需要） |
| Test | `cargo test --workspace`（隐含编译，无独立 Build 步骤） |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format check | `cargo fmt --all --check` |
| Run ignored tests | `cargo test --workspace -- --ignored`（`continue-on-error: true`，不阻断流程） |
| Security audit | `cargo audit`（经 `taiki-e/install-action@cargo-audit` 安装） |

### 3.2 触发与发布门

- `ci.yml`：push / PR（`main` / `master`）触发，复用 checks。
- `release.yml`：`v*` tag 触发，三 job 结构 `verify-secrets → checks → build-and-release`；发布前先运行 checks 作为前置门，通过后 `cargo install tauri-cli` + `tauriScript: cargo tauri` 构建并起草 Release（draft）。正式发布需配置签名密钥。

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

## 5. 版本与变更控制

- 依赖由 `Cargo.lock` 锁定，保证可复现构建。
- Rust 工具链由 `rust-toolchain.toml` 固定（stable / MSRV 1.80）。
- 仓库启用 `.github/dependabot.yml` 跟踪依赖更新（cargo / github-actions 两生态）。
- 发布通过 `release.yml` 的 `verify-secrets` 前置门 + 复用 checks，保证发版与 CI 质量门一致。

## 6. 验收标准

各阶段验收以「可通过 §3 CI 全部检查」为最低通过线，并补充人工功能验证：

- 壁纸应用/显示/退出恢复；
- 托盘与单实例；
- 全屏处置（none / pause / terminate）；
- 电量暂停；
- 主窗口原生 UI 的网格滚动、设置、四类预览、键盘/拖放交互。

***

## 相关文档

- [实施与验证总览](./01-实施与验证总览-Implementation-and-Verification-Overview.md)
- [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)
- [项目目录结构](./03-项目目录结构-Project-Structure.md)
- [测试报告](./05-测试报告-Test-Reports.md)
- [非功能性需求](../01-需求文档/03-非功能性需求-Non-Functional-Requirements.md)
- [优化演进摘要](../04-优化文档/01-优化演进摘要.md)
- [附录A-已修复问题汇总](../04-优化文档/09-附录A-已修复问题汇总.md)