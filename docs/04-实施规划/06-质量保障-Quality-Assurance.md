[← 返回文档索引](../index.md) > 实施规划 > 质量保障

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 质量保障

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 质量目标

本项目以**零警告、可测试、可发布**为核心质量目标，保障手段落在「本地工具链 + CI 流水线 + 集成测试」三层。

- **Rust 侧**：`cargo test / clippy / fmt` 全部零告警（clippy 用 `-D warnings`）。
- **依赖安全**：`cargo audit` 兜底。
- **e2e**：PowerShell 驱动脚本（`crates/mirrorstar-ui/`）做真机内存/CPU/交互冒烟验证。

> 原前端质量门槛（TypeScript 类型检查、ESLint、Vitest、Vite 构建、npm audit）已随前端删除而取消。

## 2. 本地质量工具（命令）

### Rust（workspace）

| 命令 | 用途 |
|------|------|
| `cargo test --workspace` | 运行全部单元/集成测试 |
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
| `shell_measure.ps1` / `single_instance_check.ps1` | 壳内存测量 / 单实例校验（可能位于该目录） |

## 3. CI 流水线（真实流程）

项目通过 `.github/workflows/checks.yml` 定义**可复用检查**，供 `ci.yml`（push/PR）与 `release.yml`（tag 触发的发布前置门）共享。checks.yml **仅包含 `rust-checks` 一个 job**（原 `frontend-checks` 已删除）。

### 3.1 rust-checks（windows-latest）

| 步骤 | 说明 |
|------|------|
| Setup Rust stable | `dtolnay/rust-toolchain@stable`（components: clippy, rustfmt），兼容 MSRV 1.80 |
| Cache cargo | `Swatinem/rust-cache@v2`（key 含 Cargo.lock / Cargo.toml / runner.os） |
| Test | `cargo test --workspace`（隐含编译） |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format check | `cargo fmt --all --check` |
| Run ignored tests | `cargo test --workspace -- --ignored`（`continue-on-error: true`，共享资源类串行，不阻断流程） |
| Security audit | `cargo audit`（经 `install-action` 安装） |

### 3.2 frontend-checks（已删除）

原 `frontend-checks` job（Setup Node 20 / `npm ci` / npm audit / Vite 构建 / `tsc` / ESLint / Vitest）已随前端删除**整体移除**，CI 不再使用 Node/npm。

### 3.3 触发与发布门

- `ci.yml`：push / PR 触发，调用 checks（仅 rust-checks）。
- `release.yml`：`v*` tag 触发，发布前先运行 checks 作为前置门，通过后执行 `cargo install tauri-cli` + `tauriScript: cargo tauri` 构建发布（draft release），不再使用 Node/npm。正式发布需配置签名密钥。

## 4. 测试覆盖（实测量）

### Rust 集成测试

`src-tauri/tests/`：

| 文件 | 行数 | 覆盖内容 |
|------|------|----------|
| `wallpaper_flow.rs` | 1231 | 壁纸生命周期流程 |
| `config_flow.rs` | 231 | 配置读改写流程 |
| `csp_guard.rs` | 160 | CSP 策略守卫 |
| `common/mod.rs` | 222 | 测试公共工具 |

`crates/mirrorstar-core/tests/`：

| 文件 | 行数 | 覆盖内容 |
|------|------|----------|
| `audio_integration.rs` | 61 | 音频集成 |
| `ipc_timeout.rs` | 239 | IPC 超时与容错 |
| `video_wallpaper_fullpath.rs` | 124 | 视频壁纸全路径 |

### 原生 UI / e2e（`crates/mirrorstar-ui/`）

主窗口重构后新增 `mirrorstar-ui` crate，其单元/集成测试纳入 `cargo test --workspace`；真机端到端验证由 PowerShell 驱动脚本完成（见第 2 节 e2e 脚本表：`measure_grid.ps1`、`measure_scroll_reverify.ps1`、`measure_preview.ps1`、`keyboard_smoke.ps1` 等）。

> **历史记录**：原前端单测（`src/scripts/**/*.test.ts`、`main.test.ts`）通过 `npm run test`（Vitest + coverage）执行；`src-tauri/tests/csp_guard.rs`（CSP 策略守卫）针对已移除的 WebView2 主窗口。二者均已随前端/主窗口重构删除。

## 5. 版本与变更控制

- 依赖由 `Cargo.lock` 锁定，保证可复现构建（原 `package-lock.json` 已随前端删除）。
- Rust 工具链由 `rust-toolchain.toml` 固定（stable / MSRV 1.80）。
- 仓库启用 `.github/dependabot.yml` 跟踪依赖更新。

## 6. 验收标准

各阶段验收时以「可通过第 3 节 CI 全部检查」为最低通过线，并补充人工功能验证（壁纸应用、托盘、全屏处置、电量暂停等）。

## 7. 相关章节

- [实施规划总览](01-实施规划总览-Implementation-Overview.md)
- [开发阶段划分](03-开发阶段划分-Development-Phases.md)
- [项目目录结构](05-项目目录结构-Project-Structure.md)
- [开发环境搭建](02-开发环境搭建-Development-Environment.md)