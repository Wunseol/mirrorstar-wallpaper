# MirrorStar Wallpaper（镜星壁纸）实施规划 — 开发环境搭建指南

[← 返回文档索引](../README.md) > [实施规划](./overview.md) > 开发环境搭建

## 1a. 开发环境搭建指南

### 1a.1 必要工具

| 工具 | 版本要求 | 用途 | 安装方式 |
|------|----------|------|----------|
| **Rust** | stable 1.80+ | 核心开发语言 | `winget install Rustlang.Rustup` 或访问 https://rustup.rs |
| **Visual Studio Build Tools** | 2022+ | MSVC 编译工具链 | 安装 "Desktop development with C++" 工作负载 |
| **Windows SDK** | 10.0.19041+ | Windows API 头文件和库 | 随 VS Build Tools 安装 |
| **Node.js** | 18+ LTS | Tauri 前端构建 | `winget install OpenJS.NodeJS.LTS` |
| **Git** | latest | 版本控制 | `winget install Git.Git` |

### 1a.2 推荐工具

| 工具 | 用途 | 安装方式 |
|------|------|----------|
| **Tauri CLI** | Tauri 项目管理和构建 | `cargo install tauri-cli` |
| **rust-analyzer** | Rust IDE 支持 | VS Code 扩展 |
| **cargo-watch** | 文件变更自动编译 | `cargo install cargo-watch` |
| **cargo-edit** | 依赖版本管理 | `cargo install cargo-edit` |

### 1a.3 环境搭建步骤

```powershell
# 1. 安装 Rust 工具链
winget install Rustlang.Rustup
rustup default stable
rustup update

# 2. 安装 VS Build Tools（需手动安装或使用 winget）
winget install Microsoft.VisualStudio.2022.BuildTools
# 安装时选择 "Desktop development with C++" 工作负载
# 确保 Windows SDK (10.0.19041+) 已勾选

# 3. 安装 Node.js
winget install OpenJS.NodeJS.LTS

# 4. 安装 Git
winget install Git.Git

# 5. 安装 Tauri CLI
cargo install tauri-cli

# 6. 验证环境
rustc --version          # 应显示 1.80+
cargo --version
node --version           # 应显示 v18+
npm --version

# 7. 克隆项目并构建
git clone <repo-url>
cd mirrorstar-wallpaper
cargo build
cargo tauri dev
```

### 1a.4 常见问题

| 问题 | 解决方案 |
|------|----------|
| `link.exe not found` | 安装 VS Build Tools 并选择 "Desktop development with C++" |
| `windows-sys` 编译失败 | 确保 Windows SDK 已安装，版本 >= 10.0.19041 |
| `WebView2` 相关编译错误 | 确保 WebView2 SDK 头文件可用（随 VS Build Tools 安装） |
| `cargo tauri dev` 启动失败 | 检查 Node.js 是否安装，运行 `npm install` 安装前端依赖 |
| 中文路径编译问题 | 确保项目路径不含中文字符和空格 |
