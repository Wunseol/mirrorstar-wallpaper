[← 返回文档索引](../index.md) > [技术栈](./技术栈总览-Tech-Stack-Overview.md) > UI 框架

# MirrorStar Wallpaper（镜星壁纸）技术栈 — UI 框架

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v2.0                      |
| 更新日期 | 2026-08-29                |
| 文档状态 | 已实现（基于最新代码审计）        |

***

## 1. UI 框架

### 选择：Tauri v2

**选择理由：**

- **系统 WebView2**：Windows 10/11 已预装 WebView2 Runtime，无需额外安装
- **Rust 后端**：后端逻辑用 Rust 编写（`src-tauri/` + `mirrorstar-core`），与核心业务代码无缝集成
- **小二进制**：与 Electron 相比，无捆绑 Node/Chromium
- **安全模型**：Tauri 的权限系统限制前端对系统 API 的访问，减少攻击面
- **活跃社区**：插件生态丰富，版本适配良好（`tauri 2.11` + `tauri-plugin-dialog 2.7` + `tauri-plugin-autostart 2.5`）

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **WPF** (Lively 的选择) | 成熟稳定，XAML 数据绑定 | 需要 .NET Framework，体积大，样式定制复杂 | ❌ 运行时依赖过重 |
| **egui** | 纯 Rust，即时模式 | 不适合复杂 UI，无 CSS 样式 | ❌ 不适合富 UI |
| **WinUI3** | 微软官方，Fluent Design | 需要 Windows App SDK，Rust 绑定不成熟 | ❌ 依赖过重 |
| **Tauri v2** ✅ | 系统 WebView2，Rust 后端，Web UI 灵活 | 需要 WebView2 Runtime（Win10/11 已预装） | ✅ 最佳平衡 |

### 前端方案

**采用：原生 HTML + CSS + TypeScript（轻量方案）**

`package.json` 实际前端工具链：

- **TypeScript** `^5.5`：类型安全，配合 `tsc --noEmit` 做类型检查
- **Vite** `^5.4`：开发服务器与构建
- **Vitest** `^2.1.9`：单元测试（含 `@vitest/coverage-v8`，与 jsdom 环境）
- **ESLint** `^9` + **Prettier** `^3` + **terser** `^5`：规范与压缩
- **@tauri-apps/api** `^2`：Tauri 前端 API
- **@tauri-apps/cli** `^2.11.2`：Tauri CLI

> **未使用** React / Vue / Angular 等重型框架。壁纸应用 UI（设置面板、壁纸列表、托盘菜单）相对简单，原生 TS 足够，避免引入虚拟 DOM / 响应式系统的体积与复杂度。

前后端通信通过 Tauri IPC（`invoke` + 事件 `emit`/`listen`）完成，前端通过 `@tauri-apps/api` 调用 `src-tauri/src/commands/` 下注册的 command。

### 主窗口懒创建（Lazy Creation）策略

MirrorStar 采用 WebView2 主窗口懒创建策略，而非在 `tauri.conf.json` 中静态定义窗口：

- **`tauri.conf.json` 的 `windows` 配置不预建主窗口**：避免应用启动时自动创建窗口
- **动态创建**：通过 `WebviewWindowBuilder`（`tauri::WebviewWindowBuilder`）在用户点击托盘菜单"打开主窗口"时按需创建
- **托盘先行**：应用启动时仅初始化系统托盘（`TrayIconBuilder`）并常驻后台，主窗口在需要时懒创建
- **关闭即隐藏**：窗口关闭时隐藏而非销毁，下次打开时显示（见 `create_or_show_main_window`）

此策略的优势：

- 减少应用启动时的资源占用，WebView2 运行时仅在用户打开主窗口时才初始化
- 托盘常驻场景下，用户不操作 UI 时零 WebView2 开销，聚焦壁纸渲染性能

### 系统托盘

`src-tauri/src/lib.rs` 通过 Tauri 2 内置 `tray-icon` feature 创建托盘，菜单共 3 项：

```
镜星壁纸
├── 打开主窗口
├── 暂停壁纸（点击切换为"恢复播放"）
└── 退出
```

托盘菜单 3 项的 handle 分别存储，暂停/恢复项文本随状态动态更新（`tray_pause_resume_item`）。托盘图标缺失时优雅降级（不因图标文件异常导致启动失败）。

***

**相关章节：** [← 总览](./技术栈总览-Tech-Stack-Overview.md) | [Windows 系统 API](./Windows系统API-Windows-System-API.md) | [壁纸渲染](./壁纸渲染-Wallpaper-Rendering.md)