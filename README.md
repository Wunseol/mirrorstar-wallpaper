# MirrorStar Wallpaper（镜星壁纸）

> 基于 Rust + Tauri v2 的轻量高性能 Windows 动态壁纸软件。

支持视频 / GIF / 网页 / 图片四种壁纸类型，采用原生 WorkerW 嵌入技术将壁纸渲染至桌面图标层下方，在保证桌面正常交互的同时实现低资源占用与流畅动态壁纸体验。

## 特性

- **四种壁纸类型**：视频（mpv）/ GIF / 网页（WebView2）/ 图片，覆盖主流动态与静态壁纸场景
- **WorkerW 嵌入**：壁纸渲染在桌面图标层下方，不影响桌面交互与右键菜单
- **多显示器支持**：独立设置每个屏幕的壁纸，适配多屏办公与娱乐环境
- **暂停/恢复**：全屏应用时自动暂停（事件驱动，非轮询），降低前台游戏/视频时的资源占用
- **电池策略**：交流供电恢复 / 电池供电暂停，兼顾移动设备的续航与体验
- **子进程隔离**：视频与网页壁纸在独立子进程运行，单个壁纸崩溃不影响主程序与其他壁纸

## 技术栈

| 层 | 技术 |
|---|---|
| 前端 | TypeScript + Vite + Vitest |
| 应用层 | Tauri v2 + Rust |
| 核心 | mirrorstar-core（Rust workspace） |
| 子进程 | mirrorstar-wp-proc（视频/网页隔离） |
| 渲染 | windows-rs 0.58 + WebView2 + mpv.exe |

项目顶层结构（Rust workspace 三个成员：`src-tauri` / `mirrorstar-core` / `mirrorstar-wp-proc`）：

```
mirrorstar-wallpaper/
├── src/                    # 前端源码（TypeScript + Vite）
├── src-tauri/              # Tauri 应用主体（Rust）
├── crates/
│   ├── mirrorstar-core/     # 核心逻辑（workspace 成员）
│   └── mirrorstar-wp-proc/  # 壁纸子进程（视频/网页隔离）
├── docs/                   # 项目文档
└── package.json            # 前端依赖与脚本
```

## 快速开始

### 环境要求

- Windows 10 / 11
- Node.js 18+（LTS）
- Rust 1.80+（edition 2021）
- WebView2 Runtime（Windows 11 已内置）

### 构建命令

```bash
# 安装依赖
npm install

# 开发模式
npm run dev        # 前端
cargo tauri dev    # 完整应用（需 Windows）

# 构建
npm run build      # 前端
cargo tauri build  # 完整应用

# 测试
npm run test       # 前端
cargo test --workspace  # Rust

# 类型检查
npm run typecheck  # TypeScript 类型校验
```

### 构建产物维护

`target/`（cargo 构建产物）已在 `.gitignore` 中忽略，不纳入版本控制。频繁执行 `cargo tauri dev` / `cargo test --workspace` / `cargo clippy --workspace --all-targets` 会使 `target/debug/incremental` 与 `target/debug/deps` 持续累积（实测可达数十 GB）。建议在重大里程碑后、或 `target/` 超过 5 GB 时执行：

```bash
cargo clean   # 删除整个 target/，下次构建自动复现
```

> 仅 `target/` 可安全删除；`mpv/`（视频壁纸运行时播放器）与 `node_modules/`（前端依赖）需保留。

## 文档入口

详细文档请参阅 [docs/index.md](./docs/index.md)。

`docs/` 目录按以下四大类组织，覆盖从需求到实施的完整设计资料：

- **01-需求文档**：项目概述、功能性/非功能性需求、用例、约束
- **02-架构设计**：系统架构、模块设计、进程架构、桌面集成、暂停/恢复、错误处理、性能
- **03-技术栈**：UI 框架、Windows API、壁纸渲染、基础设施、风险评估
- **04-实施规划**：开发环境、阶段、项目结构、甘特图、质量保障

## 项目状态

- **当前版本**：0.1.0（开发中）
- **目标平台**：Windows 10 / 11
- **项目状态**：积极开发中，API 与功能可能调整

## 构建配置说明（v4.1 文档化项）

> 本节记录 v4.1 build-infra 模块审查中经过评估后选择"文档化接受"的项，避免后续审查重复发现。

### 依赖版本策略（v41-B-005）

`package.json` 中 npm 依赖版本采用 `^` 范围（如 `"vite": "^5.4.0"`），允许次版本/补丁版本自动升级。具体安装版本由 `package-lock.json` 锁定，`npm ci` 在 CI 中按 lockfile 安装，保证可复现构建。

- **升级审查要求**：执行 `npm update` / `cargo update` 前需审查对应 `CHANGELOG`，确认无破坏性变更。
- **依赖新鲜度**：通过 `.github/dependabot.yml` 自动发起升级 PR（覆盖 cargo / npm / github-actions 三生态，weekly 调度）。
- **安全审计**：CI 中 `npm audit` + `cargo audit` 在每次构建时被动发现已知漏洞。

### @types/node 与 @types/bun 兼容性（v41-B-006）

v4.1 审查原发现 `devDependencies` 中 `@types/node` 与 `@types/bun` 可能同时存在导致类型定义冲突。实际复查当前 `package.json`：两者**均未声明**（`devDependencies` 仅含 eslint / vite / vitest / typescript / prettier 等工具链依赖，无任何 `@types/*` 运行时类型包），冲突前提不成立，无需移除。本项标记为"不适用"，仅文档化保留审查痕迹。

### TypeScript strict 模式 noUncheckedIndexedAccess（v41-B-009）

v4.1 审查原发现 `tsconfig.json` 启用 `strict: true` 但未启用 `noUncheckedIndexedAccess`，存在索引访问返回 `T` 而非 `T | undefined` 的运行时错误风险。实际复查当前 `tsconfig.json:17` 已配置 `"noUncheckedIndexedAccess": true`，审查基于过时信息，无需调整。本项标记为"不适用"。

### TypeScript paths 别名（v41-B-010）

`tsconfig.json` 未配置 `paths` 别名（如 `@/*` → `src/*`），所有导入使用相对路径（如 `../../utils/xxx`）。考虑到当前前端源码规模较小（`src/scripts/` 目录层级仅 2~3 层），相对路径可读性可接受，引入别名会额外增加 vite/tsconfig 配置复杂度与对齐成本。v41-B-010 选择接受当前相对路径配置。如未来源码规模增长导致路径维护困难，再引入 `@/*` 别名。

### mirrorstar-wp-proc crate-type 显式声明（v41-B-013）

v4.1 审查建议在 `crates/mirrorstar-wp-proc/Cargo.toml` 显式声明 `crate-type = ["bin"]`。实际复查：`mirrorstar-wp-proc` 为纯 binary crate，仅有 `src/main.rs` 入口（无 `src/lib.rs`），`Cargo.toml` 仅声明 `[[bin]]` 段落（无 `[lib]` 段落）。`crate-type` 字段仅在 `[lib]` 段落生效，binary crate 无需也无法声明 `crate-type`。v41-B-013 标记为"不适用"，按任务要求跳过。

### wpfile 自定义协议 scope 配置策略（v41-B-017 / v16-D-009 / v16-D-011）

壁纸图片/GIF/视频/网页资源经自定义 `wpfile://` 协议加载到 WebView。由于 JSON 不支持注释，此处说明策略：

- **背景**：原方案使用 Tauri 内置 `assetProtocol.scope` 配置允许路径，但 Windows 上 `try_resolve_symlink_and_canonicalize` 会给路径加 `\\?\` 前缀（verbatim），导致 glob 匹配失败，即使配置 `**` 也无法通过。故改用自定义 `wpfile://` 协议（`src-tauri/src/lib.rs` 中 `register_uri_scheme_protocol("wpfile", ...)`），在 handler 中直接读取文件并返回。
- **scope 校验**（v16-D-009 修复）：wpfile handler 内实现路径 scope 校验（`wpfile_path_allowed` 纯函数 + `check_wpfile_scope` 封装），复用原 assetProtocol.scope 的 allow/deny 语义：
  - **allow**：`$APPDATA/mirrorstar/**/*` + `$APPLOCALDATA/mirrorstar/**/*`（应用自身写入的数据目录）。
  - **deny**（优先于 allow）：`$HOME` 下 7 个敏感目录（`.ssh` / `.aws` / `.gnupg` / `.config/ssh` / `.password-store` / `.kube` / `.docker`），作为防御纵深。
  - 不在 allow 范围内或命中 deny 列表时返回 `403 FORBIDDEN`。
- **CSP 配置**（`tauri.conf.json`）：`img-src` / `media-src` / `frame-src` 仅允许 `wpfile:` 与 `https://wpfile.localhost`，不再保留 `asset:` 协议（v16-D-011 移除了死代码 `assetProtocol` 配置与 `protocol-asset` feature）。
- **不放开 `$RESOURCE/*` 与 `$APPDATA/*` 全量根目录**：避免恶意壁纸通过 `wpfile://` 协议读取应用资源清单或同目录其他应用数据，遵循最小权限原则。

### tauri.conf.json bundle 配置选择理由（v41-B-018）

`tauri.conf.json` 中 `bundle` 段落控制安装包产物的形态与图标资源：

- **`targets: "all"`**：构建当前平台支持的全部安装包格式。在 Windows 平台等价于同时产出 `nsis`（`.exe` NSIS 安装器，体积小、安装快、支持自定义页面）与 `msi`（`.msi` Windows Installer，企业 IT 部署友好、支持组策略分发）。接受两种格式共存的理由：① 满足个人用户（偏好 NSIS）与企业用户（偏好 MSI）的不同部署习惯；② CI 已通过 `rust-cache` 缓存编译产物，增量构建第二种格式的额外耗时较小；③ Tauri 官方默认值即为 `"all"`，避免在多平台开发时遗漏 macOS（`.dmg`/`.app`）与 Linux（`.deb`/`.AppImage`）格式。
- **`icon` 多分辨率**：包含 `32x32.png` / `128x128.png` / `128x128@2x.png`（PNG 位图，DPI 感知，覆盖高 DPI 显示与任务栏小图标）/ `icon.icns`（macOS 应用包图标，当前 Windows-only 项目保留以备跨平台）/ `icon.ico`（Windows 主图标，含多尺寸位图）。多分辨率保证资源管理器、任务栏、应用窗口、安装器、SmartScreen 等不同场景下均有合适尺寸，避免缩放锯齿。
- **`active: true`**：bundle 段落始终生效；release.yml 通过 `tauri-apps/tauri-action@v0` 调用 `tauri build` 时自动应用此配置。

### tauri.conf.json window 默认配置（v41-B-019）

`tauri.conf.json` 中 `app.windows` 为空数组 `[]`，未在配置文件中预声明静态窗口，主窗口由代码动态创建。设计理由与窗口默认配置说明如下：

- **延迟初始化（lazy init）**：用户启动应用后，主窗口由托盘菜单点击或托盘图标点击触发 `create_or_show_main_window`（`src-tauri/src/state.rs`）动态创建。窗口不可见时不占用 WebView2 进程内存，降低托盘驻留期间的资源占用。
- **窗口存在则复用，否则动态创建**：`create_or_show_main_window` 先 `app.get_webview_window("main")`；命中则 `show() + set_focus()`，未命中则通过 `WebviewWindowBuilder` 构建新窗口。Bug #5 修复后关闭窗口走 `hide()` 而非 `destroy()`，主窗口在应用生命周期内仅构建一次，后续打开复用已有 WebView2 实例。
- **窗口默认参数**（在代码而非 conf.json 中声明）：
  - `label: "main"`：唯一标识符，用于 `get_webview_window` 复用查找。
  - `url: WebviewUrl::App("index.html")`：加载前端入口（Vite 构建产物 `dist/index.html`）。
  - `title: "镜星壁纸"`：窗口标题（任务栏显示）。
  - `inner_size(900.0, 600.0)`：默认尺寸 900×600，适配壁纸列表 + 配置面板布局，保证主要控件无横向滚动。
  - `min_inner_size(700.0, 500.0)`：最小尺寸 700×500，防止用户拖拽过小导致布局错乱。
  - `center()`：创建时居中显示。
- **未设置 `decorations: false` / `transparent: true`**：当前保留系统原生标题栏与窗口边框（最大化/最小化/关闭按钮由系统绘制），便于用户熟悉的窗口操作；窗口背景非透明，避免合成器开销与视觉叠加问题。若未来需要自定义标题栏（如自绘壁纸预览区域延伸至标题栏），可在 `WebviewWindowBuilder` 链中追加 `.decorations(false)` 并相应实现拖拽与最大化按钮的 IPC 命令。

### Cargo.lock 提交策略（v41-B-020）

仓库根目录 `Cargo.lock` 已提交入库，作为应用构建可复现性的关键保障。策略说明如下：

- **提交理由**：本项目为二进制应用（Tauri 桌面应用，非 library crate）。Cargo 官方建议二进制应用提交 `Cargo.lock`，确保开发者本地、CI、release 构建使用完全一致的依赖版本组合，避免因间接依赖版本漂移导致"在 CI 通过但本地失败"或"release 包意外行为"。
- **可复现构建**：CI 中 `cargo test --workspace` / `cargo clippy --workspace` / release.yml 的 `tauri build` 均基于 `Cargo.lock` 锁定的版本编译，配合 `rust-cache` 缓存，clean build 与增量 build 产物一致。
- **`cargo update` 审查要求**：执行 `cargo update` 前必须审查目标依赖的 `CHANGELOG` 与 release notes，确认无破坏性变更（API 移除、feature 默认值变更等）。建议按以下流程：
  1. `cargo update --dry-run` 查看将被升级的 crate 列表；
  2. 对每个升级的 crate 检查官方 CHANGELOG，重点关注 major/minor 版本变更与 deprecations；
  3. 升级后本地运行 `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` 验证；
  4. 安全相关 crate（如 `tauri`、`tokio`、`windows`、`webview2-com`）升级后额外运行 `cargo audit`。
- **配合 Dependabot**：`.github/dependabot.yml` 已配置 cargo 生态 weekly PR，自动创建依赖升级 PR。Dependabot 升级 PR 中会同时修改 `Cargo.toml` 版本声明与 `Cargo.lock` 锁定版本，PR 描述中含官方 release notes 链接便于审查。
- **`cargo audit` 兜底**：CI 中 `cargo audit` 步骤在每次构建时检查已知漏洞（RustSec advisory database），即使依赖未及时升级也能被动发现安全问题。
