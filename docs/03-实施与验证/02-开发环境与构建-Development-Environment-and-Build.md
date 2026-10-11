[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 开发环境与构建

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-10-12                |
| 文档状态 | 已实现（基于真实代码审计，2026-10）        |

> 本文合并原《开发环境搭建》与《构建基础设施》，统一工具链与命令，并以真实 CI 工作流为准重写构建/发布流程。
>
> **不依赖 Node/npm**：主窗口为原生 Slint UI，仓库无前端构建产物，CI 亦不安装 Node；原有 npm/vitest/eslint/前端 CI 等表述已删除。

***

## 1. 必要工具

| 工具 | 版本要求 | 用途 | 安装方式 |
|------|----------|------|----------|
| **Rust** | stable（`rust-version` 1.80，`rust-toolchain.toml` 锁定 stable 通道） | 核心开发语言（workspace 4 成员） | https://rustup.rs 或 `winget install Rustlang.Rustup` |
| **Visual Studio Build Tools** | 2022 | MSVC 编译工具链（`cargo build` 需要 `link.exe`） | 安装 "Desktop development with C++" 工作负载 |
| **Windows SDK** | 10.0.19041+ | Windows API（windows-rs） | 随 VS Build Tools 安装 |
| **Tauri CLI** | 2.11 | `cargo tauri dev/build` | `cargo install tauri-cli --version "^2.11" --locked` |
| **Git** | latest | 版本控制 | `winget install Git.Git` |

> **包管理器**：仅使用 **Cargo**（`Cargo.lock` 已提交，保证可复现构建）。原 `package.json` / `package-lock.json` 已随前端删除。

### Rust 工具链

仓库根 `rust-toolchain.toml` 锁定 stable 通道并声明 `components = ["rustfmt", "clippy"]`，`rustup` 会自动读取并安装，无需手动 `rustup default`。MSRV 由根 `Cargo.toml` 的 `rust-version = "1.80"` 声明。

## 2. UI 依赖

主窗口 UI 由 `crates/mirrorstar-ui/` 使用 **Slint 1.18** 实现，并在**编译期**绑定软件渲染：`default-features = false` + `features = ["renderer-software", "std", "compat-1-2", "backend-winit", "raw-window-handle-06"]`（不编译 femtovg/skia）。无需任何前端工具链。

## 3. 环境搭建步骤

```powershell
# 1. 安装 Rust
winget install Rustlang.Rustup
rustup update

# 2. 安装 VS Build Tools（选择 "Desktop development with C++"，含 Windows SDK）
winget install Microsoft.VisualStudio.2022.BuildTools

# 3. 安装 Tauri CLI
cargo install tauri-cli --version "^2.11" --locked

# 4. 安装 Git
winget install Git.Git
```

### 验证环境

```powershell
rustc --version        # 1.8x stable
cargo --version
cargo tauri --version  # Tauri CLI 2.11
```

### 安装依赖与运行

```powershell
# 拉取依赖并构建全部 workspace 成员
cargo build --workspace

# 开发运行（build.rs 会在构建时以隔离 CARGO_TARGET_DIR 单独构建
# mirrorstar-ui 子进程；wp-proc 已并入壳的 --wp-proc 模式）
cargo tauri dev
```

## 4. 常用命令

### Rust（workspace）

| 命令 | 说明 |
|------|------|
| `cargo build --workspace` | 编译全部成员 |
| `cargo build -p mirrorstar-ui` | 单独编译原生 UI 子进程 |
| `cargo build -p mirrorstar-wp-proc` | 单独编译 WP 库 crate |
| `cargo test --workspace` | 运行全部单元/集成测试 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint（零警告策略） |
| `cargo fmt --all --check` | 格式检查 |
| `cargo audit` | 依赖安全审计 |

> `cargo test --workspace` 默认跳过 `#[ignore]` 的共享资源类测试；如需执行用 `cargo test --workspace -- --ignored`（CI 中以 `continue-on-error` 运行）。

### e2e（PowerShell 驱动脚本，`crates/mirrorstar-ui/`）

| 脚本 | 说明 |
|------|------|
| `measure_grid.ps1` | 网格内存/CPU 测量 |
| `measure_scroll_reverify.ps1` | 滚动性能复测 |
| `measure_preview.ps1` | 四类预览测量 |
| `keyboard_smoke.ps1` | 键盘交互冒烟 |
| `shell_measure.ps1` | 壳内存测量 |
| `single_instance_check.ps1` | 单实例校验 |

## 5. 打包

```powershell
# build.rs 会在进行 tauri 构建时自动以隔离 CARGO_TARGET_DIR
# 单独构建 mirrorstar-ui，并复制到 target/release/（wp-proc 已并入壳）
cargo tauri build          # 生成 NSIS 安装包（含资源）
```

产物位于 `target/release/bundle/`。真实打包配置见 `src-tauri/tauri.conf.json`：

- `productName`：`MirrorStar Wallpaper`；`identifier`：`com.mirrorstar.wallpaper`。
- `bundle.targets`：`["nsis"]`。
- `bundle.resources`：`../mpv/mpv.exe` → `mpv/mpv.exe`、`../mpv/license` → `mpv/license`、`../target/release/mirrorstar-ui.exe` → `mirrorstar-ui.exe`（与主 exe 同目录，`src-tauri/src/ui_process/paths.rs` 依此解析）。
- `bundle.icon`：`icons/32x32.png`、`icons/128x128.png`、`icons/128x128@2x.png`、`icons/icon.icns`、`icons/icon.ico`。
- `build`：空对象（无 `beforeBuildCommand` / `frontendDist` / `devUrl`，不依赖前端产物）。
- `app.security.csp`：`default-src 'self'`；`app.withGlobalTauri`: `false`；`app.windows`: `[]`。

> 发布由 GitHub Actions `.github/workflows/release.yml` 在打 `v*` tag 时自动执行（需配置签名密钥 `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`）。Tauri v2 NSIS 默认启用签名，未配置密钥会失败，需在仓库 Secrets 中配置。

## 6. 构建工具链版本

| 工具 | 版本 | 配置位置 |
|------|------|----------|
| Rust (MSRV) | 1.80 | 根 `Cargo.toml` `rust-version = "1.80"` |
| Rust (CI) | stable | `checks.yml` `dtolnay/rust-toolchain@stable`（`components: clippy, rustfmt`） |
| Rust (本地) | stable | `rust-toolchain.toml` `channel = "stable"` |
| Tauri | 2.11 | 根 `Cargo.toml` `tauri = { version = "2.11", features = ["tray-icon"] }`；`tauri.conf.json` `$schema` 锁定 `tauri-v2.11.2` |
| Tauri CLI | ^2.11 | `release.yml` `cargo install tauri-cli --version "^2.11" --locked` |
| Slint | 1.18 | `crates/mirrorstar-ui/Cargo.toml`（`default-features = false` + `renderer-software`） |
| webview2-com | 0.34 | 根 `Cargo.toml`（评估保持 0.34：0.38 传递依赖 windows 0.61 与 workspace windows 0.58 不兼容） |
| mpv（运行时） | 20260903 | `checks.yml` / `release.yml` 固定 release + SHA256 下载 |

### 依赖与版本要点

- `[workspace.dependencies]` 统一声明依赖版本，子 crate 以 `workspace = true` 引用，版本一致性好。
- `tokio` 按需启用（`rt`/`rt-multi-thread`/`macros`/`time`/`fs`/`sync`/`io-util`），非 `full`，并收紧到 minor 级（1.52）。
- `image` 关闭默认特性，仅启用实际使用格式（png/jpeg/gif/bmp/webp/tiff/ico + rayon）。
- `[profile.release]`：`lto = true` / `codegen-units = 1` / `strip = "symbols"` / `panic = "abort"`；`[profile.dev] opt-level = 1`（改善 dev 下 GIF 解码等重计算场景）。
- `src-tauri/Cargo.toml` `[lib] crate-type = ["lib"]`（桌面专用场景裁剪，移除无用的 cdylib/staticlib）；`tauri-build = "2"`。

## 7. CI/CD 现状

项目通过 `.github/workflows/checks.yml` 定义**可复用检查工作流**（`workflow_call`），供 `ci.yml`（push/PR）与 `release.yml`（tag 触发的发布前置门）共享。**仅含 `rust-checks` 单一 job**，运行于 `windows-latest`。

### 7.1 rust-checks 步骤（真实顺序）

| 步骤 | 说明 |
|------|------|
| Checkout | `actions/checkout@v4` |
| Install Rust stable | `dtolnay/rust-toolchain@stable`（`components: clippy, rustfmt`），兼容 MSRV 1.80 |
| Cache cargo | `Swatinem/rust-cache@v2`（cache-key 含 Cargo.lock / Cargo.toml / runner.os） |
| Provision mpv runtime | `pwsh` 固定 release（20260903）+ SHA256 校验下载 `mpv.exe`（`mpv/` 在 `.gitignore`，`tauri_build::try_build` 编译期校验其存在） |
| Test | `cargo test --workspace`（隐含编译，无独立 Build 步骤） |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format check | `cargo fmt --all --check` |
| Run ignored tests | `cargo test --workspace -- --ignored`（`continue-on-error: true`，不阻断流程） |
| Install cargo-audit | `taiki-e/install-action@cargo-audit` |
| Security audit | `cargo audit` |

### 7.2 触发与发布门

- `ci.yml`：push / PR（`main` / `master`）触发，`checks: uses: ./.github/workflows/checks.yml`（仅 13 行）。
- `release.yml`：tag `v*` 触发，`permissions: contents: write`，三 job 结构 `verify-secrets → checks → build-and-release`：
  - `verify-secrets`（`ubuntu-latest`）：对 `TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD` 判空 fail-fast。
  - `checks`（`needs: verify-secrets`）：复用 checks.yml，质量门与 CI 完全一致。
  - `build-and-release`（`windows-latest`）：checkout → Rust stable → rust-cache → `cargo install tauri-cli` → 再次 Provision mpv runtime → `tauri-apps/tauri-action@v0`（`tauriScript: cargo tauri`，`releaseDraft: true`）。
- `.github/dependabot.yml`：weekly（周一）调度，覆盖 **cargo / github-actions 两生态**（无 npm），每生态 `open-pull-requests-limit: 5`，含同生态 minor/patch 分组与 `chore(deps)` 提交前缀。

## 8. 构建配置要点与构建机制

### 8.1 版本与安全纵深

- 版本号唯一来源为 `Cargo.toml` 的 `workspace.package.version`（`tauri.conf.json` 已移除独立 `version` 字段，避免双处硬编码漂移）。
- `Cargo.lock` 已提交（二进制应用的正确做法）。
- CSP 使用严格策略（`default-src 'self'`），capabilities 权限最小化，`withGlobalTauri: false`，assetProtocol 已整体移除（`tauri` 依赖亦不含 `protocol-asset` feature）。
- `.cargo/config.toml` 不配置 crates.io 镜像替换（仅保留 `[net] git-fetch-with-cli = true`）；并对 `cfg(windows)` 注入 `/MANIFESTDEPENDENCY`（Common-Controls v6），修复 `cargo test` 生成的可执行文件因缺 manifest 而 `0xC0000139` 崩溃的问题。

### 8.2 主窗口 UI sidecar 构建机制

`src-tauri/build.rs` 在调用 `tauri_build::try_build()` **之前**，以**隔离的 `CARGO_TARGET_DIR`**（`env::temp_dir()/mirrorstar-ui-cargo-target`）单独执行一次 `cargo build -p mirrorstar-ui --release`，避免与主构建争用同一 target 目录锁而 deadlock，并避免 slint feature 合并破坏「编译期软件渲染」不变量。产物以**双 profile 复制**落到 `target/<PROFILE>/mirrorstar-ui.exe`（dev 加载）与 `target/release/mirrorstar-ui.exe`（`bundle.resources` 校验）。`mirrorstar-wp-proc` 已并入壳的 `--wp-proc` 模式，不再单独产出 exe。

`crates/mirrorstar-ui/build.rs`：`slint_build::compile("ui/main.slint")` 编译 Slint 界面；对 `mirrorstar-core` 源码变化声明 `rerun-if-changed`；通过 `/MANIFEST:EMBED` + `/MANIFESTINPUT:app.manifest` 嵌入 comctl32 v6 清单，并用 `embed-resource` 将 `src-tauri/icons/icon.ico` 嵌入 `mirrorstar-ui.exe`。

### 8.3 已修正的过时表述

- 移除 Node / npm / Vite / Vitest / TypeScript / ESLint / Prettier 及 `package.json` 相关内容（宿主文件已随前端删除）。
- `frontend-checks` CI job 已删除，CI 仅 `rust-checks` 单 job。
- Dependabot 覆盖为 cargo / github-actions **两生态**（非三生态）。
- `rust-toolchain.toml` **存在且已入库**（此前"未提交"描述失真）。
- workspace 结构为 **4 个成员**（含 `crates/mirrorstar-ui`）。
- `clippy.toml` 已删除（Rust ≥1.74 clippy 自动读取 `Cargo.toml` 的 rust-version）。

### 8.4 待确认项（未实施，登记备查）

- `mirrorstar-ui` / `mirrorstar-wp-proc` 声明了直接依赖 `serde` 但源码仅用 `serde_json`（属依赖图声明变更，待单独处理）。
- `checks.yml` 与 `release.yml` 的 mpv provisioning 脚本逐字重复，可抽 composite action（会改动 workflow step 结构）。
- `capabilities/default.json` 中 `dialog:allow-open` 已被 `dialog:default` 覆盖；`gen/schemas/` 下 `desktop-schema.json` 与 `windows-schema.json` 字节相同且入库；`rustfmt.toml` 多项为默认值。

## 9. 常见问题

| 问题 | 解决方案 |
|------|----------|
| `link.exe not found` | 安装 VS Build Tools，勾选 "Desktop development with C++" |
| windows-rs 编译失败 | 确保 Windows SDK (≥ 10.0.19041) 已安装 |
| WebView2 相关错误 | WebView2 SDK 随 VS Build Tools 安装（仅网页壁纸/预览需要） |
| 找不到 `mirrorstar-ui.exe` | 正常构建会自动生成并复制到 `target/release/`；若手动运行，先 `cargo build -p mirrorstar-ui` |
| 缺少 `mpv.exe` 导致 `src-tauri` 编译失败 | 将 mpv.exe 放入 `mpv/`（`tauri_build::try_build` 编译期校验其存在） |
| 中文/空格路径 | 项目路径避免中文字符与空格 |

***

## 相关文档

- [实施与验证总览](./01-实施与验证总览-Implementation-and-Verification-Overview.md)
- [项目目录结构](./03-项目目录结构-Project-Structure.md)
- [质量保障](./04-质量保障-Quality-Assurance.md)
- [测试报告](./05-测试报告-Test-Reports.md)
- [架构与技术栈总览](../02-架构与技术栈/01-架构与技术栈总览-Architecture-and-Tech-Stack-Overview.md)
- [依赖与数据流](../02-架构与技术栈/08-依赖与数据流-Dependency-and-Data-Flow.md)
- [优化演进摘要](../04-优化文档/01-优化演进摘要.md)
- [附录A-已修复问题汇总](../04-优化文档/09-附录A-已修复问题汇总.md)