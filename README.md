# MirrorStar Wallpaper（镜星壁纸）

> 基于 Rust + Tauri v2 的轻量高性能 Windows 动态壁纸软件。

支持视频 / GIF / 网页 / 图片四种壁纸类型，采用原生 WorkerW 嵌入技术将壁纸渲染至桌面图标层下方，在保证桌面正常交互的同时实现低资源占用与流畅动态壁纸体验。

## 特性

- **四种壁纸类型**：视频（mpv.exe 外部子进程）/ GIF / 网页（WebView2）/ 图片
- **WorkerW 原生嵌入**：壁纸渲染在桌面图标层下方，不影响桌面交互与右键菜单
- **多显示器支持**：每屏独立设置壁纸，支持排列模式（per_monitor 每屏独立 / all_same 全员同图 / span 跨屏合并）
- **暂停/恢复**：全屏应用时自动暂停（事件驱动，非轮询）；电池供电时可暂停；支持播放控制（暂停/恢复/音量/静音/播放速度）
- **子进程隔离**：视频与网页壁纸运行在独立子进程，单个壁纸崩溃不影响主程序与其他壁纸
- **壁纸轮换**：定时轮换 + 采样算法（顺序循环 / 洗牌袋 / 纯随机）+ 轮换池
- **其他**：开机自启、缩略图管理、鼠标交互模式、托盘驻留（主窗口延迟创建，降低驻留资源占用）

## 技术栈

| 层 | 技术 |
|---|---|
| 前端 | TypeScript 5.5 + Vite 5.4 + Vitest 2.1.9（无前端框架） |
| 应用层 | Tauri v2.11 + Rust 1.80（edition 2021） |
| 核心 | mirrorstar-core（Rust workspace 成员） |
| 子进程 | mirrorstar-wp-proc（视频/网页壁纸隔离） |
| 渲染 | windows-rs 0.58 + WebView2 + mpv.exe |
| 配置 | TOML + serde |
| 日志 | tracing 系列 |
| IPC | Windows 命名管道（自研两套协议） |
| 资源加载 | 自定义 wpfile:// URI 协议（含路径 scope 校验） |

## 项目结构

Rust workspace 三个成员：`src-tauri` / `mirrorstar-core` / `mirrorstar-wp-proc`。

```
mirrorstar-wallpaper/
├── src/                      # 前端源码（TypeScript + Vite）
├── src-tauri/                # Tauri 应用主体（Rust）
├── crates/
│   ├── mirrorstar-core/      # 核心逻辑（workspace 成员）
│   └── mirrorstar-wp-proc/   # 壁纸子进程（视频/网页隔离）
├── docs/                     # 项目文档（入口：docs/index.md）
├── .github/                  # CI/CD 工作流与依赖升级配置
├── package.json              # 前端依赖与脚本
└── Cargo.lock                # Rust 依赖锁定（已提交入库）
```

## 快速开始

### 环境要求

- Windows 10 / 11
- Node.js 18+（LTS）
- Rust 1.80+
- WebView2 Runtime（Windows 11 已内置）

### 构建命令

```bash
# 安装依赖
npm install

# 开发模式
npm run dev        # 前端 dev（Vite）
cargo tauri dev    # 完整应用（需 Windows；tauri:dev 脚本会先构建 mirrorstar-wp-proc 子进程）

# 构建
npm run build      # 前端构建
cargo tauri build  # 完整应用构建

# 测试
npm run test            # 前端测试 + 覆盖率
cargo test --workspace  # Rust 测试

# 代码质量
npm run typecheck  # TypeScript 类型检查
npm run lint       # 代码规范
```

### 构建产物维护

`target/`（cargo 构建产物）已在 `.gitignore` 中忽略，不纳入版本控制。频繁执行构建/测试会使 `target/` 持续累积，建议在 `target/` 超过 5 GB 时执行：

```bash
cargo clean   # 删除整个 target/，下次构建自动复现
```

> 仅 `target/` 可安全删除；`mpv/`（视频壁纸运行时播放器，在 `.gitignore` 中，按指引单独下载解压）与 `node_modules/`（前端依赖）需保留。

## 构建配置说明

核心策略要点如下（完整背景与审查记录见 [docs/05-优化文档/09-构建基础设施.md](./docs/05-优化文档/09-构建基础设施.md)）：

- **依赖版本策略**：npm 依赖采用 `^` 范围，安装版本由 lockfile 锁定，保证可复现构建；升级（`npm update` / `cargo update`）前需审查对应 CHANGELOG 确认无破坏性变更；依赖新鲜度由 Dependabot（weekly）自动发起升级 PR，CI 中 `npm audit` + `cargo audit` 兜底发现已知漏洞。
- **wpfile:// 协议 scope 策略**：壁纸资源经自定义 `wpfile://` 协议加载（Windows 上 Tauri 内置 assetProtocol 的 verbatim 路径会导致 glob 匹配失败）；handler 内做路径 scope 校验，allow 仅 `$APPDATA/mirrorstar` 与 `$APPLOCALDATA/mirrorstar`，deny `$HOME` 下 7 个敏感目录，命中拒绝时返回 403；CSP 仅允许 `wpfile:` 与 `https://wpfile.localhost`，不放开 `$RESOURCE/*` 等全量根目录（最小权限原则）。
- **Cargo.lock 提交策略**：仓库提交 `Cargo.lock`（二进制应用，Cargo 官方建议），确保本地 / CI / release 构建使用完全一致的依赖组合；`cargo update` 前审查目标 crate 的 CHANGELOG，升级后运行 `cargo test --workspace` + clippy 验证，安全相关 crate 额外运行 `cargo audit`。
- **bundle / 窗口配置**：`bundle.targets: ["nsis"]`（当前产出 NSIS 安装器；如需同时产出 MSI 供企业部署，可配置为 `"all"`），icon 提供多分辨率；`app.windows` 为空数组，主窗口由代码动态创建（托盘触发延迟初始化、窗口复用不销毁），默认参数（label `main`、900×600、最小 700×500、居中、保留系统标题栏）在 `WebviewWindowBuilder` 中声明。

## 文档入口

详细文档请参阅 [docs/index.md](./docs/index.md)。

`docs/` 目录按以下分类组织，覆盖从需求到合规的完整设计资料：

- **01-需求文档**：项目概述、功能性/非功能性需求、用例、约束
- **02-架构设计**：系统架构、模块设计、进程架构、桌面集成、暂停/恢复、错误处理、性能
- **03-技术栈**：UI 框架、Windows API、壁纸渲染、基础设施、风险评估
- **04-实施规划**：开发环境、阶段、项目结构、质量保障
- **05-优化文档**：模块优化、构建基础设施、实施路线图、附录
- **06-测试报告**：性能与资源占用、运行时测试报告

## 项目状态

- **当前版本**：0.1.0（开发中）
- **目标平台**：Windows 10 / 11
- **项目状态**：积极开发中，API 与功能可能调整

## 许可证

项目采用 [GPLv2+（GNU GPL v2 或其后版本）](./LICENSE)。

- **mpv.exe（GPL-2.0+）**：与项目采用相同许可证，捆绑分发无兼容性问题。
- **WebView2（微软专有）**：系统内置组件（Windows 11 已内置），通过 WebView2 Runtime 使用。
