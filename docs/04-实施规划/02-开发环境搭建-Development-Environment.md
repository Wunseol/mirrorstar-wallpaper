[← 返回文档索引](../index.md) > [实施规划](./01-实施规划总览-Implementation-Overview.md) > 开发环境搭建

# MirrorStar Wallpaper（镜星壁纸）实施规划 — 开发环境搭建

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. 必要工具

| 工具 | 版本要求 | 用途 | 安装方式 |
|------|----------|------|----------|
| **Rust** | stable（`rust-version` 1.80，`rust-toolchain.toml` 锁定 stable） | 核心开发语言（workspace 4 成员） | https://rustup.rs 或 `winget install Rustlang.Rustup` |
| **Visual Studio Build Tools** | 2022 | MSVC 编译工具链（`cargo build` 需要 link.exe） | 安装 "Desktop development with C++" 工作负载 |
| **Windows SDK** | 10.0.19041+ | Windows API（windows-rs 需要 Windows SDK） | 随 VS Build Tools 安装 |
| **Tauri CLI** | 2.x | `cargo tauri dev/build` | `cargo install tauri-cli`（仓库不再使用 npm 版 CLI） |
| **Git** | latest | 版本控制 | `winget install Git.Git` |

> **不需要 Node.js / npm**：项目已无前端构建产物，主窗口为原生 Slint UI；CI 亦不再安装 Node。
>
> **包管理器**：仅使用 **Cargo**（`Cargo.lock` 锁定）。原 `package.json` / `package-lock.json` 已随前端删除。

### Rust 工具链

仓库根含 `rust-toolchain.toml`（锁定 stable channel），`rustup` 会自动读取并安装对应通道，无需手动 `rustup default`。

## 2. UI 依赖

主窗口 UI 由 `crates/mirrorstar-ui/` 使用 **Slint 1.18** 实现，绑定软件渲染（`default-features = false` + `features = ["renderer-software"]`）。无需任何前端工具链（无 TypeScript / Vite / Vitest / ESLint / Prettier）。

## 3. 环境搭建步骤

```powershell
# 1. 安装 Rust
winget install Rustlang.Rustup
rustup update

# 2. 安装 VS Build Tools（选择 "Desktop development with C++"，含 Windows SDK）
winget install Microsoft.VisualStudio.2022.BuildTools

# 3. 安装 Tauri CLI
cargo install tauri-cli

# 4. 安装 Git
winget install Git.Git
```

### 验证环境

```powershell
rustc --version        # 1.8x stable
cargo --version
cargo tauri --version  # Tauri CLI 2.x
```

### 安装依赖与运行

```powershell
# 拉取依赖并构建全部 workspace 成员
cargo build --workspace

# 开发运行（build.rs 会在构建时以隔离 CARGO_TARGET_DIR
# 单独构建 mirrorstar-ui 子进程；wp-proc 已并入壳）
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

> `cargo test --workspace` 会跳过 `#[ignore]` 的共享资源类测试；如需执行用 `cargo test --workspace -- --ignored`（CI 中以 `continue-on-error` 运行）。

### e2e（PowerShell 驱动脚本，`crates/mirrorstar-ui/`）

| 脚本 | 说明 |
|------|------|
| `measure_grid.ps1` | 网格内存/CPU 测量 |
| `measure_scroll_reverify.ps1` | 滚动性能复测 |
| `measure_preview.ps1` | 四类预览测量 |
| `keyboard_smoke.ps1` | 键盘交互冒烟 |
| `shell_measure.ps1` / `single_instance_check.ps1` | 壳内存测量 / 单实例校验 |

> 原 `package.json` scripts（`npm run dev/build/test/lint/typecheck/format`、`tauri:dev` 等）已随前端删除**全部移除**。

## 5. 打包

```powershell
# build.rs 会在进行 tauri 构建时自动以隔离 CARGO_TARGET_DIR
# 单独构建 mirrorstar-ui，并复制到 target/release/（wp-proc 已并入壳）
cargo tauri build          # 生成 NSIS 安装包（含资源）
```

产物位于 `target/release/bundle/`。打包配置详见 [项目目录结构](./05-项目目录结构-Project-Structure.md) 与 `src-tauri/tauri.conf.json`（`bundle.targets=["nsis"]`，resources 捆绑 `mpv/mpv.exe`、`mpv/license`、`target/release/mirrorstar-ui.exe`）。

> Release 构建由 GitHub Actions `.github/workflows/release.yml` 在打 `v*` tag 时自动执行（需配置签名密钥 `TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD`），不再使用 Node/npm。安装包签名密钥：Tauri v2 NSIS 默认启用签名，未配置密钥会失败，需在仓库 Secrets 中配置。

## 6. 常见问题

| 问题 | 解决方案 |
|------|----------|
| `link.exe not found` | 安装 VS Build Tools，勾选 "Desktop development with C++" |
| windows-rs 编译失败 | 确保 Windows SDK (≥ 10.0.19041) 已安装 |
| WebView2 相关错误 | WebView2 SDK 随 VS Build Tools 安装（仅网页壁纸/预览需要） |
| 找不到 `mirrorstar-ui.exe` | 正常构建会自动生成并复制到 `target/release/`；若手动运行，先 `cargo build -p mirrorstar-ui` |
| 中文/空格路径 | 项目路径避免中文字符与空格 |

***

**相关章节：** [← 总览](./01-实施规划总览-Implementation-Overview.md) | [项目目录结构](./05-项目目录结构-Project-Structure.md) | [质量保障](./06-质量保障-Quality-Assurance.md)