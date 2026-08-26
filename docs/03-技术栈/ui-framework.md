# MirrorStar Wallpaper（镜星壁纸）技术栈 — UI 框架

[← 返回文档索引](../README.md) > [技术栈](./overview.md) > UI 框架

## 3. UI 框架

### 选择：Tauri v2

**选择理由：**

- **系统 WebView2**：Windows 10/11 已预装 WebView2 Runtime，无需额外安装，零额外体积
- **Rust 后端**：后端逻辑完全用 Rust 编写，与核心业务代码无缝集成
- **小二进制**：Tauri 应用典型体积 2-5MB（不含资源），远小于 Electron（~150MB）
- **Web 前端灵活性**：HTML/CSS/JS 前端允许快速构建现代化 UI，支持动画、主题、响应式布局
- **安全模型**：Tauri 的权限系统限制前端对系统 API 的访问，减少攻击面
- **活跃社区**：GitHub 80k+ stars，文档完善，插件生态丰富

### 与替代方案对比

| 方案 | 优势 | 劣势 | 结论 |
|------|------|------|------|
| **WPF** (Lively 的选择) | 成熟稳定，XAML 数据绑定 | 需要 .NET Framework，体积大，样式定制复杂 | ❌ 运行时依赖过重 |
| **egui** | 纯 Rust，即时模式，简单 | 即时模式不适合复杂 UI，无 CSS 样式，外观简陋 | ❌ 不适合富 UI |
| **WinUI3** | 微软官方，Fluent Design | 需要 Windows App SDK，依赖链长，Rust 绑定不成熟 | ❌ 依赖过重 |
| **Iced** | 纯 Rust，Elm 架构 | 仍不成熟，复杂 UI 实现困难，社区小 | ❌ 不够成熟 |
| **Slint** | 纯 Rust，声明式 UI | 商业许可限制，社区较小 | ❌ 许可证风险 |
| **Tauri v2** ✅ | 系统 WebView2，Rust 后端，小体积，Web UI 灵活 | 需要 WebView2 Runtime（Win10/11 已预装） | ✅ 最佳平衡 |

### 前端方案

**推荐：原生 HTML + CSS + TypeScript（轻量方案）**

不使用 React/Vue/Angular 等重型框架，理由：

- 壁纸应用的 UI 相对简单（设置面板、壁纸列表、托盘菜单），无需虚拟 DOM 或响应式系统
- 减少前端打包体积和加载时间
- 降低构建复杂度，无需 Node.js 构建链
- 可选 Preact（3KB）作为最小化的组件化方案，如需组件复用

前端技术栈：

- **TypeScript**：类型安全，IDE 支持好
- **CSS Custom Properties**：主题切换（亮色/暗色）
- **Fetch API / Tauri IPC**：前后端通信

### WebView2 懒创建策略

MirrorStar 采用 WebView2 窗口懒创建（Lazy Creation）策略，而非在 `tauri.conf.json` 中静态定义窗口：

- **tauri.conf.json windows 数组为空**：主窗口不在配置文件中预定义，避免应用启动时自动创建窗口
- **动态创建**：通过 `WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))` 在需要时按需创建主窗口
- **关闭即隐藏**：窗口关闭时调用 `prevent_close + hide()` 隐藏窗口（保留 WebView2 实例），下次打开时 `show()` 恢复
- **托盘先行**：应用启动时仅创建系统托盘图标，主窗口在用户点击托盘图标时按需创建

**窗口生命周期**：Create → Show → Hide → Show（隐藏/恢复模式，保留 WebView2 实例）

此策略的优势：
- 减少应用启动时的资源占用，WebView2 运行时仅在需要时初始化
- 窗口关闭后完全释放 WebView2 资源，避免内存泄漏
- 托盘常驻场景下，用户不操作 UI 时零 WebView2 开销

---

**相关章节：** [← 总览](./overview.md) | [Windows API 绑定](./windows-api.md) | [壁纸渲染](./wallpaper-rendering.md)
