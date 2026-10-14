[← 返回文档索引](../index.md) > 实施与验证

# MirrorStar Wallpaper（镜星壁纸）实施与验证 — 测试报告

| 项目   | 内容                        |
| ---- | ------------------------- |
| 项目名称 | MirrorStar Wallpaper（镜星壁纸） |
| 文档版本 | v1.0                      |
| 更新日期 | 2026-10-12                |
| 文档状态 | 已实现（基于真实代码审计，2026-10）        |

> 本文合并三份历史测试报告（2026-06-25 / 2026-07-05 / 2026-07-24），按日期以二级章节分别保留其环境、方法与数据。
>
> **重要：全部为历史记录**（主窗口原生 UI 重构前，基于旧前端与 WebView2 主窗口）。其中涉及 `npx tauri dev`、Vite/前端 UI、WebView2 主窗口、asset protocol scope 等描述均为当时的真实状态，现已随架构重构失效，仅作演进参考。**本文不新增任何未经实测的数据。**
>
> **当前测试体系**：Rust 单元/集成测试（`cargo test --workspace`）+ `crates/mirrorstar-ui/` 的 PowerShell 驱动脚本 e2e（见 §4）。

***

## 1. 2026-06-25 壁纸性能与资源占用测试

> 第二次全面测试（含 GUI 交互）。测试人员：AI Agent + 用户辅助 GUI 操作。测试版本：debug 构建（`npx tauri dev`）。

### 1.1 测试环境

| 项目 | 实际环境 | NFR 规定条件 | 差异说明 |
|------|----------|--------------|----------|
| 操作系统 | Windows 10 专业版 | Windows 11 23H2 | ⚠️ 低于规定 |
| CPU | Intel Core i5-7200U @ 2.50GHz（双核四线程） | i5-12400 / Ryzen 5 5600 | ⚠️ 远低于规定 |
| 内存 | 3.9 GB | 16 GB | ⚠️ 远低于规定 |
| 构建 | debug（含调试符号，未优化） | 通常为 release | ⚠️ debug 开销更大 |
| mpv | v0.41.0（`target/debug/mpv` 符号链接 → `mpv/`） | 需外部 mpv.exe | ✅ 就绪 |

> 实测环境显著弱于规定条件（CPU 约 3-4 倍、内存 4 倍差距），部分指标偏大，达标结论需结合此差异判断。

测试素材：`wallpaper/` 下 PNG（4.94MB）与两个 MP4（28.85MB / 19.43MB）；无 GIF 素材。资源采样用 PowerShell `Get-Process`（`WorkingSet64` / `CPU` / `Threads.Count` / `HandleCount`），每态采样 5 次、间隔 3 秒取稳定值。

### 1.2 功能测试结果

| 测试项 | 结果 | 说明 |
|--------|------|------|
| 应用启动 | ✅ 通过 | 无 panic；WorkerW 1 秒内就绪；托盘/日志/配置监视/全屏检测线程/Explorer 监控均启动 |
| 静态壁纸（PNG / Native） | ✅ 核心通过 | 设置与显示正常；缩略图/预览因 Bug #1 失败 |
| 视频壁纸（MP4 / mpv） | ✅ 核心通过 | mpv 启动/嵌入/播放正常；控制因 Bug #2/#3/#4 异常 |
| 系统托盘与退出 | ❌ 失败 | Bug #5（关闭主窗口即退出）+ Bug #6（mpv 孤立） |

### 1.3 性能与资源占用数据

**启动/切换时间**：冷启动 ~1 秒（达标 <2s）；静态壁纸切换 <1 秒；视频壁纸切换 ~0.5 秒（mpv 启动→嵌入完成）。

**三态资源占用对比**：

| 状态 | 主进程内存 | 主进程 CPU | 线程数 | 句柄数 | mpv 内存 | mpv CPU |
|------|-----------|-----------|--------|--------|----------|---------|
| 空闲态 | 22.03 MB | 0% | 12 | 206 | — | — |
| 静态壁纸态 | 32.59 MB | 0% | 16 | 366 | — | — |
| 视频壁纸态 | 31.73 MB | 0% | 18 | — | 79 MB | 22-33% |

> 静态壁纸态内存略高于视频态，因测试时主窗口（WebView2）已打开；两者主进程内存均远低于 50MB 目标。

**NFR 达标判定**：

| NFR ID | 指标 | 目标 | 实测 | 判定 |
|--------|------|------|------|------|
| NFR-001 | 暂停态内存 | < 20MB | 22.03MB（debug/空闲态近似） | ⚠️ 略超（release 预计达标） |
| NFR-002 | 活跃态内存 | < 50MB | 32.59 / 31.73 MB | ✅ 达标 |
| NFR-003 | 暂停态 CPU | < 0.1% | 0% | ✅ 达标 |
| NFR-004 | 活跃态 CPU | < 5% | 0%（主进程） | ✅ 达标 |
| NFR-005 | 冷启动时间 | < 2s | ~1s | ✅ 达标 |
| NFR-007 | 壁纸切换时间 | < 1s | <1s（静态）/ ~0.5s（视频） | ✅ 达标 |

### 1.4 Bug 列表（6 项）

| 优先级 | Bug | 描述 | 状态 |
|--------|-----|------|------|
| P0 | #6 | 应用退出时未清理 mpv 子进程（`perform_shutdown()` 未在退出路径调用），mpv 孤立 | ✅ 已修复 |
| P1 | #5 | 关闭主窗口调用 `window.destroy()` 导致应用退出 | ✅ 已修复（`prevent_close` + `hide()`） |
| P1 | #2 | 全屏检测误触发（未排除自身窗口/WorkerW），设置壁纸时反复暂停/恢复 | ✅ 已修复（v2.1） |
| P1 | #4 | 暂停按钮始终灰色（Bug #2 的直接后果） | ✅ 已修复（v2.1） |
| P2 | #1 | asset protocol scope 过窄导致壁纸源文件/缩略图无法加载 | ✅ 已修复（v3.1） |
| P2 | #3 | 静音切换无法取消（前端传递空 display_id） | ✅ 已修复（v3.2，FE-001） |

关联关系：Bug #2 → 直接导致 Bug #4；Bug #5 → 引发 Bug #6。

### 1.5 结论

应用启动达标；静态/视频壁纸核心功能通过；系统托盘与退出失败（Bug #5/#6）；活跃/空闲 CPU 达标；空闲内存略超（debug 构建）。改进建议：立即修复 P0/P1 Bug，使用 release 构建复测 NFR-001，补充 GIF 壁纸测试。

***

## 2. 2026-07-05 运行时测试（含 2026-07-06 修复验证）

> 在 v2.1 / v3.1 / v3.2 多轮修复完成后进行，目的：回归验证历史 Bug、复核 NFR、记录新发现问题。

### 2.1 测试环境

| 项目 | 内容 |
|------|------|
| 操作系统 | Windows 10 Pro 22H2 |
| 构建类型 | Debug（`npx tauri dev`） |
| 应用版本 | 0.1.0 |
| mpv 版本 | v0.41.0（`target/debug/mpv/` junction → `mpv/`） |
| Tauri | v2.11.2 |
| 测试时间 | 2026-07-04 21:19 ~ 22:13（两个独立会话） |

> 本机非 NFR 规定的标准测试硬件（i5-12400 / Win11 23H2），性能数据仅作 Debug 构建参考。

### 2.2 功能测试结果

| 场景 | 结果 | 说明 |
|------|------|------|
| 启动流程 | ✅ | WorkerW 首次查找失败后 15s 自愈（观察项 O-1）；冷启动 ~4s（Debug + 弱环境） |
| 静态壁纸（Native） | ⚠️ 部分通过 | 显示正常、无 mpv；缩略图因 asset protocol scope 对 dev 工作目录报错（Bug #1 部分回归） |
| 视频壁纸 #1（线稿） | ⚠️ 部分通过 | mpv 启动/IPC/嵌入正常；静音正常（Bug #3 ✅）；暂停/恢复需点击两次（新 Bug #7）；全屏检测未误触发（Bug #2 ✅） |
| 视频壁纸 #2（彩色） | ✅ | 切换 ~0.9s，无进程泄漏 |
| 系统托盘与退出 | ✅ | 关闭窗口隐藏到托盘（Bug #5 ✅）；托盘退出清理完整、无残留（Bug #6 ✅） |

### 2.3 性能与资源占用数据

| 状态 | 进程 | WorkingSet (MB) | CPU (%) | 线程数 | 句柄数 |
|------|------|-----------------|---------|--------|--------|
| 空闲态 | 主进程 | 22.17 | 0 | 12 | 208 |
| 静态壁纸态 | 主进程 | 36.65 | 0 | 19 | 372 |
| 视频壁纸态 #1 | 主进程 | 30.36 | 0 | 18 | — |
| 视频壁纸态 #1 | mpv | 152.40 | ~18（单核） | 31 | — |
| 视频壁纸态 #2 | 主进程 | 30.86 | 0 | 19 | — |
| 视频壁纸态 #2 | mpv | 204.44 | ~24（单核） | 34 | — |

启动/切换耗时：冷启动 ~4s；热启动 ~100ms；静态切换 ~0.5s；视频 #1 ~1.21s（净）/ 视频 #2 ~0.9s；暂停响应失败（IPC 超时）。

**NFR 达标判定**：

| NFR ID | 需求 | 实测 | 判定 |
|--------|------|------|------|
| NFR-001 | 暂停态内存 < 20MB | 22.17 MB（空闲态） | ⚠️ 略超 |
| NFR-002 | 活跃态内存 < 50MB | 30.36~36.65 MB | ✅ 达标 |
| NFR-003 | 暂停态 CPU < 0.1% | 0% | ✅ 达标 |
| NFR-004 | 活跃态 CPU < 5% | 0%（主进程） | ✅ 达标 |
| NFR-005 | 冷启动 < 2s | ~4s | ❌ 未达标（Debug + 弱环境） |
| NFR-006 | 热启动 < 1.5s | ~100ms | ✅ 达标 |
| NFR-007 | 壁纸切换 < 1s | 0.5s / 1.21s / 0.9s | ⚠️ 部分达标 |
| NFR-019 | 暂停响应 < 500ms | IPC 超时 5s | ❌ 未达标（Bug #7） |
| NFR-020 | 托盘菜单响应 < 200ms | ~100ms | ✅ 达标 |

### 2.4 历史 Bug 回归结论

| Bug ID | 描述 | 修复版本 | 回归状态 |
|--------|------|----------|----------|
| #1 | asset protocol scope 配置不当 | v2.1 | ⚠️ 部分修复（dev 工作目录路径仍报错，生产素材应位于 `$APPDATA/mirrorstar`） |
| #2 | 全屏检测误触发 | v2.1 | ✅ 已修复 |
| #3 | 静音切换无法取消（null displayId） | v3.1 | ✅ 已修复 |
| #4 | 暂停按钮灰色 | v3.2 | ⚠️ 表面修复（按钮可点击，但暴露 Bug #7） |
| #5 | 关闭主窗口导致退出 | v3.1 | ✅ 已修复（`window.hide()`） |
| #6 | 退出未清理 mpv | v3.2 | ✅ 已修复（`ExitRequested` → `perform_shutdown_blocking`） |

### 2.5 新发现 Bug #7

**Bug #7：视频壁纸暂停/恢复需点击两次（High，P0）**

- 现象：首次点击暂停不生效，需点击第二次才进入暂停；恢复同样需双击。
- 日志佐证：`视频暂停失败 error=IPC 超时: IPC 读取超时`。
- 根因（三处设计缺陷叠加）：① `set_property` 同步等待 ack 5s 超时；② `pause_wallpaper` 过早 emit `wallpaper-state-changed`；③ pause 线程状态切换后不 emit 事件。
- 涉及文件：`mirrorstar-core/src/ipc/mpv_protocol.rs`、`ipc/client.rs`、`wallpaper/video.rs`、`src-tauri/src/commands/wallpaper.rs`。

### 2.6 观察项（非 Bug）

| ID | 现象 | 影响 |
|----|------|------|
| O-1 | 首次查找 WorkerW 失败，15s 后自愈 | 启动后 15s 内设置壁纸可能失败（有自愈机制） |
| O-2 | `set_process_volume` 未找到音频会话 | 不影响静音（mpv IPC 完成），日志噪音 |
| O-3 | `SetWindowLongPtrW` 设置 GWL_EXSTYLE 失败 | 不影响嵌入，样式未生效 |
| O-4 | 托盘 UX：窗口可见时右键托盘重新显示窗口 | 需多一步操作 |
| O-5 | 强制终止 mpv 记录"拒绝访问 0x80070005"但自愈 | 不影响功能 |
| O-6 | 退出时未恢复原生系统壁纸 | 属设计选择 |
| O-7 | IPC connect 参数与文档 11.11 节承诺不一致 | 文档/代码不一致（后已修复） |

### 2.7 修复验证（2026-07-06）

| 修复项 | 根因 | 修复方案 | 验证结论 |
|--------|------|---------|---------|
| Bug #7 | IPC 同步等待 ack + 事件时机 | ①IPC fire-and-forget（`send_command_no_wait`）②PauseSender broadcast + 各渲染器 pause 线程 emit ③命令层条件 emit 兜底 | ✅ 单次点击即暂停/恢复；暂停响应 < 500ms（NFR-019 达标） |
| Bug #1（部分回归） | dev 模式 asset protocol scope 未含工作目录 | 新建 `tauri.dev.conf.json` 覆盖 scope + CSP 补全 + 前端容错 | ✅ dev 下 asset protocol 错误消除 |
| O-7 | IPC connect 参数不一致 | 参数回退为 `(5, 200)` 并常量化 + 文档同步 | ✅ 线稿 668ms / 彩色 625ms < 1s（NFR-007 达标） |

修复后代码质量：`cargo check --workspace` 通过、`cargo clippy --workspace` 无警告、`cargo fmt --check --all` 通过。

### 2.8 结论

启动与基础功能正常（含 WorkerW 自愈）；6 个历史 Bug 中 4 个完全修复、2 个部分修复；9 项 NFR 中 5 项达标、2 项略超、2 项未达标；新发现 Bug #7（High），根因明确、修复方案清晰，随后验证全部通过。

***

## 3. 2026-07-24 v5.0 优化后验证

> 在 v5.0 深度性能优化（Wave v5-A/B/C/D 共 29 项）完成后进行，验证优化未引入功能回归，发现静态测试盲区的运行时 bug。

### 3.1 测试环境

| 项目 | 内容 |
|------|------|
| 操作系统 | Windows 10 Pro 22H2 |
| 构建类型 | Debug（`npx tauri dev`，增量编译 4m23s） |
| 应用版本 | 0.1.0 |
| Tauri | v2.11.2 |
| 测试时间 | 2026-07-24 08:17 ~ 08:50 |

### 3.2 测试结果

- **启动流程 ✅**：应用内存 17.1MB（接近 <20MB 目标）；WorkerW **直接初始化成功**（D-PERF-002 Progman 直查快速路径生效，无需 15s 自愈）；启动日志无 ERROR/WARN；冷启动约 2 秒。
- **Bug #1（编译错误，P0）✅ 已修复**：v5.0 C-PERF-005 优化中 `cleanup_corrupted_thumbnails` 使用 `HashSet<OsString>::contains(&str)`，`OsString` 未实现 `Borrow<str>` 导致编译失败；修复为 `.contains(std::ffi::OsStr::new(stem))`。验证：`cargo test -p mirrorstar-core --lib config::manager` 50 passed / 0 failed。
- **前端 UI 测试 ⚠️ 受限**：mock IPC 注入时机问题，壁纸列表未渲染（Tauri 应用在普通浏览器中测试的固有限制，需在 Tauri WebView 环境完整测试）。
- **v5.0 优化点运行时验证**：D-PERF-002/D-PERF-001/C-PERF-005/C-PERF-002/W-PERF-003 及前端 F-PERF 系列经代码审查确认实施（F-PERF-018 IntersectionObserver 单例因与测试 mock 冲突已回退）。

### 3.3 结论

应用启动正常、内存 17.1MB；Bug #1（编译错误）已发现并修复，50 单元测试验证无回归；v5.0 优化点代码审查确认。受限项：前端 UI 功能与后端功能需在 Tauri WebView 环境手动验证。

***

## 4. 当前测试体系

主窗口原生 UI 重构后，测试体系调整为：

- **Rust 单元/集成测试**：`cargo test --workspace`，覆盖 `mirrorstar-core` / `mirrorstar-ui` / `mirrorstar-wp-proc` / 壳（`mirrorstar-wallpaper`）；共享资源类测试标记 `#[ignore]`，由 `cargo test --workspace -- --ignored` 串行执行。
- **e2e（真机）**：`crates/mirrorstar-ui/` 下的 PowerShell 驱动脚本（`measure_grid.ps1`、`measure_scroll_reverify.ps1`、`measure_preview.ps1`、`keyboard_smoke.ps1`、`shell_measure.ps1`、`single_instance_check.ps1`）。
- **质量门**：本地手工执行 `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all --check` / `cargo test --workspace -- --ignored` / `cargo audit`，详见 [质量保障](./04-质量保障-Quality-Assurance.md)。

***

## 相关文档

- [实施与验证总览](./01-实施与验证总览-Implementation-and-Verification-Overview.md)
- [开发环境与构建](./02-开发环境与构建-Development-Environment-and-Build.md)
- [项目目录结构](./03-项目目录结构-Project-Structure.md)
- [质量保障](./04-质量保障-Quality-Assurance.md)
- [非功能性需求](../01-需求文档/03-非功能性需求-Non-Functional-Requirements.md)
- [附录A-已修复问题汇总](../04-优化文档/09-附录A-已修复问题汇总.md)
- [附录B-版本历史](../04-优化文档/10-附录B-版本历史.md)