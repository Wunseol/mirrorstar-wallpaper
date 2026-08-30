[← 返回文档索引](../index.md) > [实施规划](./实施规划总览-Implementation-Overview.md) > 开发环境搭建

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
| **Rust** | stable（`rust-version` 1.80，`rust-toolchain.toml` 锁定 stable） | 核心开发语言（workspace 3 成员） | https://rustup.rs 或 `winget install Rustlang.Rustup` |
| **Visual Studio Build Tools** | 2022 | MSVC 编译工具链（`cargo build` 需要 link.exe） | 安装 "Desktop development with C++" 工作负载 |
| **Windows SDK** | 10.0.19041+ | Windows API（windows-rs 需要 Windows SDK） | 随 VS Build Tools 安装 |
| **Node.js** | 20 LTS（CI 使用 Node 20） | Tauri 前端构建（Vite） | `winget install OpenJS.NodeJS.LTS` |
| **Git** | latest | 版本控制 | `winget install Git.Git` |

> **包管理器**：仓库使用 **npm**（存在 `package-lock.json`）。CI（`.github/workflows/checks.yml`）使用 `npm ci`。仓库根还保留 `pnpm-lock.yaml` / `pnpm-workspace.yaml`（遗留文件，不参与构建，可忽略）。

### Rust 工具链

仓库根含 `rust-toolchain.toml`（锁定 stable channel），`rustup` 会自动读取并安装对应通道，无需手动 `rustup default`。

## 2. 前端依赖

`package.json` 前端工具链（与 03-技术栈 一致）：

- **TypeScript** `^5.5`、**Vite** `^5.4`、**Vitest** `^2.1.9`（含 `@vitest/coverage-v8`）
- **@tauri-apps/api** `^2`、**@tauri-apps/cli** `^2.11.2`
- **ESLint** `^9` + **typescript-eslint** `^8` + **Prettier** `^3` + **terser** `^5`

原生 TS（无 React/Vue/Angular）。

## 3. 环境搭建步骤

```powershell
# 1. 安装 Rust
winget install Rustlang.Rustup
rustup update

# 2. 安装 VS Build Tools（选择 "Desktop development with C++"，含 Windows SDK）
winget install Microsoft.VisualStudio.2022.BuildTools

# 3. 安装 Node.js 20 LTS
winget install OpenJS.NodeJS.LTS

# 4. 安装 Git
winget install Git.Git
```

### 验证环境

```powershell
rustc --version        # 1.8x stable
cargo --version
node --version         # v20.x
npm --version
```

### 安装依赖与运行

```powershell
# 拉取依赖并安装前端依赖
cargo build --workspace
npm install

# 开发运行（注意顺序：需先构建 wp-proc 子进程再启动 tauri dev）
npm run tauri:dev
```

`tauri:dev` 脚本已定义为 `cargo build -p mirrorstar-wp-proc && tauri dev`，会先构建 WP 子进程再启动开发环境。也可手动：

```powershell
cargo build -p mirrorstar-wp-proc
npm run tauri dev
```

> `tauri dev` 会调用 `beforeDevCommand: npm run dev`（Vite @ `http://localhost:1420`），等待 devUrl 就绪后加载。

## 4. 常用命令

### Rust（workspace）

| 命令 | 说明 |
|------|------|
| `cargo build --workspace` | 编译全部成员 |
| `cargo build -p mirrorstar-wp-proc` | 单独编译 WP 子进程 |
| `cargo test --workspace` | 运行全部单元/集成测试 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint（零警告策略） |
| `cargo fmt --all --check` | 格式检查 |
| `cargo audit` | 依赖安全审计 |

> `cargo test --workspace` 会跳过 `#[ignore]` 的共享资源类测试；如需执行用 `cargo test --workspace -- --ignored`（CI 中以 `continue-on-error` 运行）。

### 前端（package.json scripts）

| npm 命令 | 说明 |
|----------|------|
| `npm run dev` | Vite 开发服务器（tauri conf devUrl 1420） |
| `npm run build` | Vite 构建 → `dist/` |
| `npm run preview` | 预览构建产物 |
| `npm run test` | Vitest run + 覆盖率（`@vitest/coverage-v8`） |
| `npm run test:watch` | Vitest watch |
| `npm run lint` | ESLint（`--max-warnings 0`） |
| `npm run typecheck` | `tsc --noEmit` |
| `npm run format` | Prettier（`--write`） |
| `npm run tauri` | Tauri CLI 透传 |
| `npm run tauri:dev` | 先构建 wp-proc 再 `tauri dev` |

## 5. 打包

```powershell
npm run build              # 构建前端
cargo build -p mirrorstar-wp-proc
cargo tauri build          # 生成 NSIS 安装包（含资源）
```

产物位于 `target/release/bundle/`。打包配置详见 [项目目录结构](./项目目录结构-Project-Structure.md) 与 `src-tauri/tauri.conf.json`（`bundle.targets=["nsis"]`，resources 捆绑 `mpv/mpv.exe`、`mpv/license.txt`、`target/release/mirrorstar-wp-proc.exe`）。

> Release 构建由 GitHub Actions `.github/workflows/release.yml` 在打 `v*` tag 时自动执行（需配置签名密钥 `TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD`）。安装包签名密钥：Tauri v2 NSIS 默认启用签名，未配置密钥会失败，需在仓库 Secrets 中配置。

## 6. 常见问题

| 问题 | 解决方案 |
|------|----------|
| `link.exe not found` | 安装 VS Build Tools，勾选 "Desktop development with C++" |
| windows-rs 编译失败 | 确保 Windows SDK (≥ 10.0.19041) 已安装 |
| WebView2 相关错误 | WebView2 SDK 随 VS Build Tools 安装 |
| `npm run tauri:dev` 报 wp-proc 缺失 | 需先 `cargo build -p mirrorstar-wp-proc`（脚本已含该步骤） |
| 中文/空格路径 | 项目路径避免中文字符与空格 |

***

**相关章节：** [← 总览](./实施规划总览-Implementation-Overview.md) | [项目目录结构](./项目目录结构-Project-Structure.md) | [质量保障](./质量保障-Quality-Assurance.md)