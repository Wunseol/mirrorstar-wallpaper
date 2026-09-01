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
- **前端侧**：TypeScript 类型检查、ESLint 零警告、Vitest 单测覆盖、Vite 构建必须通过。
- **依赖安全**：`cargo audit` + `npm audit` 兜底。

## 2. 本地质量工具（命令）

### Rust（workspace）

| 命令 | 用途 |
|------|------|
| `cargo test --workspace` | 运行全部单元/集成测试 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint，零警告 |
| `cargo fmt --all --check` | 格式检查 |
| `cargo audit` | 依赖安全审计 |

### 前端

| npm 命令 | 用途 |
|----------|------|
| `npm run test` | Vitest run + 覆盖率（`@vitest/coverage-v8`） |
| `npm run typecheck` | `tsc --noEmit` 类型检查 |
| `npm run lint` | ESLint，`--max-warnings 0` |
| `npm run build` | Vite 生产构建 |

## 3. CI 流水线（真实流程）

项目通过 `.github/workflows/checks.yml` 定义**可复用检查**，供 `ci.yml`（push/PR）与 `release.yml`（tag 触发的发布前置门）共享。checks.yml 包含两个 job：

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

### 3.2 frontend-checks（windows-latest）

| 步骤 | 说明 |
|------|------|
| Setup Node 20 | `actions/setup-node@v4`（cache: npm） |
| Install | `npm ci` |
| npm audit | `npm audit --omit=dev --audit-level=high` |
| npm audit (dev) | `npm audit --audit-level=critical` |
| Build | `npm run build` |
| TypeScript | `npx tsc --noEmit` |
| ESLint | `npm run lint` |
| Tests | `npm run test` |

### 3.3 触发与发布门

- `ci.yml`：push / PR 触发，调用 checks。
- `release.yml`：`v*` tag 触发，发布前先运行 checks 作为前置门，通过后才执行 `npm ci` + `tauri-action` 构建发布（draft release）。正式发布需配置签名密钥。

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
| `video_mpv_diagnostic.rs` | 69 | mpv 诊断 |
| `video_wallpaper_fullpath.rs` | 124 | 视频壁纸全路径 |

### 前端单测（`src/scripts/**/*.test.ts` + `main.test.ts`）

覆盖 `ui/`、`utils/` 各模块及 IPC 封装；通过 `npm run test`（Vitest + coverage）在 CI 与本地执行。

## 5. 版本与变更控制

- 依赖由 `Cargo.lock` / `package-lock.json` 锁定，保证可复现构建。
- Rust 工具链由 `rust-toolchain.toml` 固定（stable / MSRV 1.80）。
- 仓库启用 `.github/dependabot.yml` 跟踪依赖更新。

## 6. 验收标准

各阶段验收时以「可通过第 3 节 CI 全部检查」为最低通过线，并补充人工功能验证（壁纸应用、托盘、全屏处置、电量暂停等）。

## 7. 相关章节

- [实施规划总览](01-实施规划总览-Implementation-Overview.md)
- [开发阶段划分](03-开发阶段划分-Development-Phases.md)
- [项目目录结构](05-项目目录结构-Project-Structure.md)
- [开发环境搭建](02-开发环境搭建-Development-Environment.md)