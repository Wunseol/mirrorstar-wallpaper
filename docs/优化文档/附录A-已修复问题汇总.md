# 附录 A：已修复问题汇总

> [← 返回索引](./README.md)

本附录汇总 v1.0 → v3.5 期间所有已修复的问题，按版本轮次组织。v3.0→v3.5 共 5 轮审计修复，合计 **231 项**（v1.0-v2.1 共 53 项 + v3.1 共 24 项 + v3.2 共 33 项 + v3.3 共 6 项 + v3.4 共 14 项 + v3.5 共 100 项 + 1 项部分修复）。

各模块的详细修复记录（含 v4.0 回退/不完整修复的 ⚠️ 标注）见对应模块文档的"v3.x 已修复问题"章节。

---

## A.1 v1.0 → v2.1 已修复问题（53 项 + 1 部分修复）

### A.1.1 config 模块（6 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 热重载完全失效 — watcher 移入线程闭包 | ✅ 已修复 |
| 2 | 防抖保存可能丢数据 — 新增 start_periodic_save | ✅ 已修复 |
| 3 | 类型检测仅按扩展名 — magic byte 内容嗅探 | ✅ 已修复 |
| 4 | 解析失败静默回退 — 返回 Result 错误传播 | ✅ 已修复 |
| 5 | 不必要的 DynamicImage 分配 — 使用 into_rgb8() | ✅ 已修复 |
| 6 | ID 长度文档不一致 — 注释修正 | ✅ 已修复 |

### A.1.2 desktop 模块（5 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 硬编码布局忽略用户配置 — 从 HashMap 取实际 arrangement | ✅ 已修复 |
| 2 | remove_wallpaper 空操作 — IsWindow 检查 + SetParent 分离 | ✅ 已修复 |
| 3 | DPI 获取失败静默默认 — 添加 tracing::warn! | ✅ 已修复 |
| 4 | DisplayInfo 字段冗余 — id 为 device_name，name 为"显示器 N" | ✅ 已修复 |
| 5 | unsafe Send/Sync 契约脆弱 — 移除冗余 unsafe impl，跨线程通过 Arc<Mutex> 保护 | ✅ 已修复 |

### A.1.3 wallpaper 模块（4 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 0 单元测试 — 现有 39 个测试 | ✅ 已修复 |
| 2 | 渲染器 GDI 缓存重复 — 提取为 gdi_cache.rs | ✅ 已修复 |
| 3 | WallpaperMode 选择逻辑分散 — 集中为 determine_wallpaper_mode() | ✅ 已修复 |
| 4 | 暂停时像素释放策略不统一 — 均使用 GdiCache.release_bitmap() | ✅ 已修复 |

### A.1.4 audio/ipc/process 模块（8 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | VolumeControl unsafe impl Sync — 改为 !Sync | ✅ 已修复 |
| 2 | 音频会话线性扫描 — 新增 session_cache PID 缓存 | ✅ 已修复 |
| 3 | IPC read_line 阻塞无超时 — read_line_with_timeout 5 秒超时 | ✅ 已修复 |
| 4 | 命令行参数转义不完整 — escape_windows_arg 遵循 MSVCRT 规则 | ✅ 已修复 |
| 5 | IPC 客户端重复代码 — 提取 NamedPipeIpcClient trait | ✅ 已修复 |
| 6 | WpProcResponse.status 是 String — 改为 ResponseStatus 枚举 | ✅ 已修复 |
| 7 | 缺少 CREATE_NO_WINDOW — 进程创建标志包含 | ✅ 已修复 |
| 8 | args 字段存储后不读取 — 结构体已无 args 字段 | ✅ 已修复 |

### A.1.5 Tauri 应用层（8 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 冗余 WorkerW 监控 — 30s 轮询改为 5 分钟兜底 | ✅ 已修复 |
| 2 | 电源监控使用轮询 — 改为 WM_POWERBROADCAST 事件驱动 | ✅ 已修复 |
| 3 | UUID 截断碰撞风险 — 使用完整 UUID（36 字符） | ✅ 已修复 |
| 4 | async 函数中同步 IO — 使用 tokio::fs::metadata | ✅ 已修复 |
| 5 | shutdown 错误被忽略 — shutdown() 返回 () 而非 Result | ✅ 已修复 |
| 6 | 10 个全局可变静态量 — 合并后 10→7 个 | ✅ 已修复 |
| 7 | 窗口关闭 50ms 魔数竞态 — 添加 MAIN_WINDOW_CLOSING AtomicBool 标志位 | ✅ 已修复 |
| 8 | COM 初始化错误处理脆弱 — 使用 RPC_E_CHANGED_MODE 常量 + match 表达式 | ✅ 已修复 |

### A.1.6 wp-proc 模块（6 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | Pause/Resume 空操作 — JS 注入暂停/恢复媒体 | ✅ 已修复 |
| 2 | 错误处理使用 panic/exit — 改为 return + 清理 | ✅ 已修复 |
| 3 | 未使用 tokio 依赖 — Cargo.toml 已移除 | ✅ 已修复 |
| 4 | create_webview 无超时 — rx.recv_timeout(10s) | ✅ 已修复 |
| 5 | WebView2 Controller 未 Close — 显式调用 ctrl.Close() | ✅ 已修复 |
| 6 | Play/Navigate 重复 — 提取 navigate_to_url 共享函数 | ✅ 已修复 |

### A.1.7 前端 UI（11 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | setVolume 缺少 displayId — 接受 displayId 参数 | ✅ 已修复 |
| 2 | 缺少 tsconfig.json — 存在，strict: true | ✅ 已修复 |
| 3 | 顺序缩略图生成 — Promise.all 并行 | ✅ 已修复 |
| 4 | 事件无防抖 — 300ms 防抖 | ✅ 已修复 |
| 5 | 重复 add-then-autoset 逻辑 — 提取 addAndAutoSetWallpaper | ✅ 已修复 |
| 6 | updateConfig 使用 any — 改为 AppConfig 类型 | ✅ 已修复 |
| 7 | 重复 .btn-primary CSS — 移除重复定义 | ✅ 已修复 |
| 8 | 死 CSS — 移除 .card-thumbnail | ✅ 已修复 |
| 9 | 硬编码版本号 — main.ts 动态覆盖 | ✅ 已修复 |
| 10 | 全局可变状态未封装 — 封装为 appState 对象 | ✅ 已修复 |
| 11 | 未使用 JS 依赖 — 移除 plugin-autostart 和 plugin-dialog | ✅ 已修复 |

### A.1.8 构建与基础设施（5 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 缺少 vite.config.ts — 配置 port 1420 | ✅ 已修复 |
| 2 | 缺少 tsconfig.json — strict: true | ✅ 已修复 |
| 3 | 未使用的依赖 — wp-proc tokio 已移除 | ✅ 已修复 |
| 4 | 缺少 CI/CD — .github/workflows/ci.yml 存在 | ✅ 已修复 |
| 5 | 测试覆盖不均 — wallpaper 0→39，wp-proc +9，合计 45→113 | ✅ 已修复 |

### A.1.9 部分修复（1 项，不计入 53 已修复）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 占位 repo URL — 已更新但可能需验证 | ⚠️ 部分修复 |

---

## A.2 v3.1 新增修复项（2026-07-04，24 项）

基于 `fix-v3-review-findings-2026-07-04` spec，修复 v3.0 审查发现的全部问题。

### A.2.1 安全与 v2.1 遗留（Batch 1，5 项）

| ID | 描述 | 状态 |
|----|------|------|
| WP-001 | 协议白名单大小写敏感绕过 — has_scheme_prefix + build_url 大小写不敏感（+6 测试） | ✅ 已修复 |
| C-112 | gif.rs `_e` 命名 → `e` | ✅ 已修复 |
| C-113 | ipc/client.rs 10ms 轮询 → Backoff 结构 + 指数退避 10ms→100ms（+8 测试） | ✅ 已修复 |
| C-107 | `let _ =` 错误忽略 → 22 处改为显式错误处理，~30 处保留并注释 | ✅ 已修复 |
| assetProtocol.scope | 从 `["**"]` 收紧为 `["$APPDATA/mirrorstar/**/*", "$HOME/**/*"]`（静态白名单） | ✅ 已修复 |

### A.2.2 代码质量（Batch 2，15 项）

| ID | 描述 | 状态 |
|----|------|------|
| N-001 | ensure_desktop_ready DRY → 提取 ensure_workerw_ready | ✅ 已修复 |
| N-002 | create_and_play_renderer dead code → 确认非 dead code，添加文档注释 | ✅ 已修复 |
| N-003 | prepare_for_wallpaper 不完整 → 补充 WorkerW 预检 for Video/Gif/Web | ✅ 已修复 |
| N-004 | set_wallpaper 重叠 → Native 分支委托 set_native_wallpaper_internal | ✅ 已修复 |
| N-005 | toggle_mute_fast 竞态 → toggle_mute_atomic 在写锁内原子操作 | ✅ 已修复 |
| N-006 | protocol.rs 命名 → 重命名为 mpv_protocol.rs | ✅ 已修复 |
| N-007 | WebM magic detection → 4 字节 EBML magic 1A 45 DF A3（+8 测试） | ✅ 已修复 |
| N-008 | HTML detection → 256 字节扫描 + 跳过 BOM（+16 测试） | ✅ 已修复 |
| N-009 | escape_windows_arg 换行符 → 返回 Result，含换行符返回 InvalidArgument（+8 测试） | ✅ 已修复 |
| WP-002 | _class_guard 命名 → 重命名为 class_guard | ✅ 已修复 |
| WP-003 | IPC 线程 JoinHandle → 保存 JoinHandle + 1s 超时 join | ✅ 已修复 |
| WP-004 | SetPosition 坐标校验 → width/height <= 0 返回 Error（+7 测试） | ✅ 已修复 |
| WP-006 | ipc_thread 错误路径测试 → 提取 process_line/format_response 纯函数（+11 测试） | ✅ 已修复 |
| WP-009 | GetModuleHandleW 缓存 → OnceLock 缓存 | ✅ 已修复 |
| WP-010 | parse_rect 错误传播 → 返回 Result，main 中 ? 传播（+7 测试） | ✅ 已修复 |

### A.2.3 改进与构建基础设施（Batch 3，8 项）

| ID | 描述 | 状态 |
|----|------|------|
| N-010 | gif_memory 无效赋值 → 移除并并入 tracing 日志 | ✅ 已修复 |
| WP-005 | JS 注射失败仅 warn → 返回 ResponseStatus::Error（+4 测试） | ✅ 已修复 |
| WP-007 | Disconnected continue 冗余 → 改为 return + warn 日志 | ✅ 已修复 |
| WP-008 | 管道安全属性 → 简化方案（注释 + TODO，v3.3 完整实现） | ⚠️ v3.1 简化方案 |
| Dead deps | 移除 anyhow、raw-window-handle、@vitest/ui | ✅ 已修复 |
| Win32_System_Pipes 重复 → 合并 | ✅ 已修复 |
| LICENSE 添加 → MIT 全文 | ✅ 已修复 |
| release.yml 改进 → 添加 cargo fmt --check + cargo audit 步骤 | ✅ 已修复 |

### A.2.4 测试缺口填补（Batch 4，2 项）

| ID | 描述 | 状态 |
|----|------|------|
| — | wallpaper/manager.rs 610L/0T → 1336L/39T（+39 测试） | ✅ 已修复 |
| — | wp-proc main.rs 145L/0T → 322L/10T（+10 测试，Cli 参数解析） | ✅ 已修复 |

### A.2.5 验证结果

- cargo test --workspace：**374 passed / 0 failed / 47 ignored**（mirrorstar-core 272 / wp-proc 86 / src-tauri 15 / doc-tests 1）
- cargo clippy --workspace --all-targets：0 warnings
- npm run test：86 passed

---

## A.3 v3.2 新增修复项（2026-07-04，33 项）

基于 `deep-review-src-tauri-frontend-2026-07-04` spec，深度审查 src-tauri 与前端。审查产出 findings/03（src-tauri，18 个问题）+ findings/04（前端，15 个问题），合计 33 个问题，全部已处理（28 修复 + 5 TODO，后者在 v3.3 完整修复）。

### A.3.1 src-tauri 审查修复（ST-001 ~ ST-018，18 项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| ST-001 | P3 | power.rs SHARED_ENGINE 未设置时仍更新电源状态 → 抽取 try_pause/resume_all_fast | ✅ 已修复 |
| ST-002 | P3 | lib.rs WorkerW 预初始化线程未保存 JoinHandle → 保存到 WORKERW_INIT_THREAD | ✅ 已修复 |
| ST-003 | P3 | workerw_check.rs 未保存 JoinHandle → 保存 + tokio::select! 即时唤醒 | ✅ 已修复 |
| ST-004 | P3 | fullscreen/power pause/resume DRY 违规（6 处重复） → 抽取公共辅助函数 | ✅ 已修复 |
| ST-005 | P3 | perform_shutdown_blocking PostThreadMessageW/join 逻辑重复 → 抽取辅助函数 | ✅ 已修复 |
| ST-006 | P3 | ensure_single_instance CreateMutexW 失败仍返回 true → 保留设计 + 注释说明 | ✅ 已修复 |
| ST-007 | P3 | wallpaper_flow.rs 27 个 #[ignore] 测试 → 提取纯函数 + 11 个非 ignore 测试 | ✅ 已修复 |
| ST-008 | P3 | create_test_config_manager 使用 ConfigManager::new() → 改用 new_in_dir + TempDir | ✅ 已修复 |
| ST-009 | P3 | validate_wallpaper_file_path 错误类型不一致 → 统一 InvalidPath 变体 | ✅ 已修复 |
| ST-010 | P3 | assetProtocol.scope 含 $HOME/**/* → 改对象形式 { allow, deny }，deny 排除敏感目录 | ✅ 已修复 |
| ST-011 | P4 | main.rs Box::leak 不必要 → 改为 let _ = _mutex + 注释 | ✅ 已修复 |
| ST-012 | P4 | is_foreground_fullscreen 窗口标题匹配不精确 → 精确匹配 + 3 个纯函数 + 12 测试 | ✅ 已修复 |
| ST-013 | P4 | GetModuleHandleW unwrap_or_default 静默吞错 → 显式 match + 提前返回 | ✅ 已修复 |
| ST-014 | P4 | add_wallpaper 缩略图 fire-and-forget → ⚠️ v3.2 TODO，v3.3 完整修复 | ⚠️→✅ |
| ST-015 | P4 | open_file_dialog blocking_pick_file 可能永久阻塞 → ⚠️ v3.2 TODO，v3.3 完整修复 | ⚠️→✅ |
| ST-016 | P4 | CSP 允许 'unsafe-inline' for style-src → ⚠️ v3.2 TODO，v3.3 完整修复 | ⚠️→✅ |
| ST-017 | P4 | wallpaper_flow.rs 测试仅模拟命令层逻辑 → ⚠️ v3.2 TODO，v3.3 完整修复 | ⚠️→✅ |
| ST-018 | P4 | explorer.rs WM_DESTROY PostQuitMessage 注释缺失 → 添加注释说明防御性代码 | ✅ 已修复 |

### A.3.2 前端审查修复（FE-001 ~ FE-015，15 项）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| FE-001 | **P2** | Bug #3 根因 — displayId 空字符串传递不一致 → 前后端三层统一 + 7 ipc 测试 + 2 集成测试 | ✅ 已修复 |
| FE-002 | P3 | appState 未实现真正封装 → statusTimer 移入 utils，selectedDisplayId 加 getter/setter | ✅ 已修复 |
| FE-003 | P3 | 4 处 bare addEventListener 违反 F-010 → 改为 addEventListenerWithCleanup（+11 测试） | ✅ 已修复 |
| FE-004 | P3 | 音量/速度滑块缺少防抖 → 150ms debounce + parseInt 显式 radix | ✅ 已修复 |
| FE-005 | P3 | mute 按钮初始状态未同步 → loadConfig 中同步图标 | ✅ 已修复 |
| FE-006 | P3 | wallpaper-list DRY 违规 — 重复 extractFileName → 复用 utils.ts | ✅ 已修复 |
| FE-007 | P3 | preview-modal CSS 选择器注入风险 → 属性比对避免模板字符串注入 | ✅ 已修复 |
| FE-008 | P3 | refreshWallpaperList 失败时骨架屏残留 → catch 块清空 + 错误提示 | ✅ 已修复 |
| FE-009 | P3 | 6 个模块无单元测试（部分修复） → 新增 preview-modal.test.ts 14 + drag-drop.test.ts 11 | ✅ 已修复 |
| FE-010 | P4 | isSupportedFile 不支持 Web (HTML) 类型 → 补充 HTML_EXTENSIONS（+11 测试） | ✅ 已修复 |
| FE-011 | P4 | parseInt 缺少 radix 参数 → 随 FE-004 修复 | ✅ 已修复 |
| FE-012 | P4 | GIF_EXTENSIONS 冗余导出 → 移除 export，新增 HTML_EXTENSIONS 导出 | ✅ 已修复 |
| FE-013 | P4 | loadConfig 中 arrangement 默认值为 dead code → 移除 \|\| "per_monitor" | ✅ 已修复 |
| FE-014 | P4 | displayId 传递 || "" 与 || undefined 混用 → 随 FE-001 统一为直接透传 | ✅ 已修复 |
| FE-015 | P4 | preview-web iframe 缺少 sandbox → 添加 sandbox="allow-scripts" | ✅ 已修复 |

### A.3.3 验证结果

- cargo test --workspace：**400 passed / 0 failed / 49 ignored**
- npm run test：**151 passed**（v3.1: 86 → v3.2: 151，+65）
- 测试总数：460 → 551（+91）

---

## A.4 v3.3 新增修复项（2026-07-05，6 项）

基于 `cleanup-remaining-todos-2026-07-04` spec，清理 v3.2 遗留的 6 个 TODO 项，项目达到"零已知 TODO"状态。全部 6 项已完整修复（非简化方案）。

| ID | 描述 | 状态 |
|----|------|------|
| ST-014 | add_wallpaper 缩略图 fire-and-forget → JoinHandle 保存到 THUMBNAIL_TASK，shutdown 时 5s 超时等待 | ✅ 已修复 |
| ST-015 | open_file_dialog blocking_pick_file 可能永久阻塞 → tokio::time::timeout 600s 包装 | ✅ 已修复 |
| ST-016 | CSP 允许 'unsafe-inline' for style-src → 收紧为 style-src 'self'，内联样式改 CSS 类 | ✅ 已修复 |
| ST-017 | wallpaper_flow.rs 测试仅模拟命令层逻辑 → 重构为直接调用纯函数测试实际校验逻辑 | ✅ 已修复 |
| WP-008 | 命名管道安全属性 → 完整实现 SDDL 安全描述符（OpenProcessToken + ConvertSidToStringSidW + DACL） | ✅ 已修复 |
| W-006 | WebView2 创建无显式超时 → wait_with_pump_timeout 30s 超时 + PeekMessageA 消息泵 | ✅ 已修复 |

---

## A.5 v3.4 新增修复项（2026-07-07，14 项）

基于 `fix-wallpaper-preview-blank` + `fix-static-wallpaper-card-blank` 两个 spec，修复壁纸卡片预览空白的复合根因与 Image/Gif 静态壁纸卡片空白问题。按模块细分共 14 项修复。

### A.5.1 前端模块（3 项）

| # | 描述 | 状态 |
|---|------|------|
| 1 | 缩略图加载失败 onerror 回退 → fallbackToEmoji helper | ✅ 已修复 |
| 2 | Video 类型无缩略图时首帧预览 → 创建 `<video>` preload="metadata" | ✅ 已修复 |
| 3 | Image/Gif 无缩略图时直接加载源图片 → convertFileSrc(wp.file_path) | ✅ 已修复 |

### A.5.2 后端 config 模块（3 项）

| # | 描述 | 状态 |
|---|------|------|
| 4 | Video 类型缩略图生成（ffmpeg） → generate_video_thumbnail + is_ffmpeg_available | ✅ 已修复 |
| 5 | 损坏缩略图文件清理 → cleanup_corrupted_thumbnails 扫描删除 0 字节文件 | ✅ 已修复 |
| 6 | mod.rs re-export 更新 → 新增 generate_video_thumbnail, is_ffmpeg_available | ✅ 已修复 |

### A.5.3 后端 src-tauri 模块（4 项）

| # | 描述 | 状态 |
|---|------|------|
| 7 | 批量重生成缩略图命令 → regenerate_thumbnails 返回 RegenerateResult | ✅ 已修复 |
| 8 | ThumbnailFailedPayload 结构体 → emit wallpaper-thumbnail-failed 事件 | ✅ 已修复 |
| 9 | add_wallpaper 扩展支持 Video 类型 → matches! 宏扩展为 Image \| Gif \| Video | ✅ 已修复 |
| 10 | 启动清理钩子 → lib.rs setup 调用 cleanup_corrupted_thumbnails | ✅ 已修复 |

### A.5.4 前端 UI 模块（4 项）

| # | 描述 | 状态 |
|---|------|------|
| 11 | wallpaper-thumbnail-failed 事件监听 → main.ts 显示 toast | ✅ 已修复 |
| 12 | regenerateThumbnails IPC 包装 → ipc.ts 新增包装函数 | ✅ 已修复 |
| 13 | 设置面板"重新生成缩略图"按钮 → index.html + main.ts 点击处理 | ✅ 已修复 |
| 14 | showStatus 扩展 info 类型 → utils.ts + main.css .status-message.info | ✅ 已修复 |

---

## A.6 v3.5 深度审计发现（2026-07-12，100 项）

2026-07-11 ~ 2026-07-12 完成对 10 个模块的 8 维度深度代码审计。8 维度涵盖：正确性、健壮性、安全性、性能、可维护性、可测试性、并发安全、资源管理。与 v3.4 已识别项去重后，新增 **100 项 findings**（Critical 0 / **High 7** / Medium 36 / Low 57），全部已修复。

### A.6.1 config 模块（11 项，C01-C11）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| C01 | **High** | load_config/load_library TOML 解析失败静默回退默认配置 → on_config_error 回调通知 | ✅ 已修复 |
| C02 | Medium | volume/speed/balanced_keep_frames 缺少范围校验 → validate() clamp | ✅ 已修复 |
| C03 | Medium | cleanup_corrupted_thumbnails 使用 Self::data_dir() → 改为实例路径 | ✅ 已修复 |
| C04 | Medium | generate_video_thumbnail 参数注入（`-` 开头路径） → `./` 前缀 | ✅ 已修复 |
| C05 | Medium | is_ffmpeg_available 同步阻塞调用 → spawn_blocking 包裹 | ✅ 已修复 |
| C06 | Medium | 解压炸弹防护不完整（基于压缩后大小） → 解码后像素缓冲区校验 | ✅ 已修复 |
| C07 | Low | atomic_write 临时文件残留 → rename 失败时清理 | ✅ 已修复 |
| C08 | Low | load_config/load_library TOCTOU 窗口 → File + take(MAX_SIZE) | ✅ 已修复 |
| C09 | Low | update_thumbnail 路径匹配不一致 → 路径规范化后比较 | ✅ 已修复 |
| C10 | Low | start_periodic_save 线程启动失败状态不一致 → spawn 失败重置标志 | ✅ 已修复 |
| C11 | Low | start_watching config 与 library 非原子 reload → 均成功后替换或回滚 | ✅ 已修复 |

### A.6.2 desktop 模块（12 项，D01-D12）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| D01 | **High** | set_native_wallpaper 注册表/壁纸设置顺序不一致 → 调整顺序或回滚 | ✅ 已修复 |
| D02 | Medium | get_system_wallpaper 固定 260 缓冲区 → 扩大至 32767 | ✅ 已修复 |
| D03 | Medium | set_mouse_passthrough 缺少 SWP_FRAMECHANGED 刷新 → 追加 SetWindowPos | ✅ 已修复 |
| D04 | Medium | embed_wallpaper PerMonitor 未匹配时静默回退 → 增加 warn 日志 | ✅ 已修复 |
| D05 | Medium | ensure_workerw_ready 重新嵌入失败条目残留 → 移除无效条目 | ✅ 已修复 |
| D06 | Medium | find_workerw() 死代码但 pub 易误用 → 删除或 deprecated | ✅ 已修复 |
| D07 | Medium | SetWindowLongPtrW 返回值 0 歧义 → SetLastError(0) + GetLastError 区分 | ✅ 已修复 |
| D08 | Low | EnumDisplayMonitors/GetMonitorInfoW 失败静默 → 增加 warn | ✅ 已修复 |
| D09 | Low | SendMessageTimeoutW result dead store → 移除或记录 | ✅ 已修复 |
| D10 | Low | 魔法数字未定义为命名常量 → 提取 const | ✅ 已修复 |
| D11 | Low | enum_windows_callback 与 fallback_enum_callback 逻辑不一致 → 统一 | ✅ 已修复 |
| D12 | Low | restore_original/system_wallpaper 返回 () → 改为 Result | ✅ 已修复 |

### A.6.3 wallpaper 模块（13 项，W01-W13）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| W01 | Medium | update_positions Span 模式负坐标 clamp 到 0 → 移除 clamp | ✅ 已修复 |
| W02 | Medium | ImageRenderer trait pause/resume 不发送命令 → 与 GifRenderer 一致 | ✅ 已修复 |
| W03 | Low | decode_gif max_frames 计算使用屏幕尺寸 → 改用实际帧尺寸 | ✅ 已修复 |
| W04 | Medium | set_wallpaper 同步路径持锁期间 play() 阻塞 → play() 移出锁范围 | ✅ 已修复 |
| W05 | Low | ensure_desktop_ready 重复实现 → 提取公共逻辑 | ✅ 已修复 |
| W06 | Medium | pause_all_fast TOCTOU 窗口 → 设置 bit 后再释放锁 | ✅ 已修复 |
| W07 | Medium | WebRenderer::play 不监听子进程退出 → spawn 监听任务 | ✅ 已修复 |
| W08 | Low | register_window_class_once 丢弃返回值 → 区分已注册与失败 | ✅ 已修复 |
| W09 | Low | gif_wallpaper_thread 后台解码无取消机制 → CancellationToken | ✅ 已修复 |
| W10 | Low | GifRenderer::set_speed 不校验 speed > 0 → 入口校验 | ✅ 已修复 |
| W11 | Low | VideoRenderer pause 线程 COM 失败静默 → 设置标志 + 降级 | ✅ 已修复 |
| W12 | Low | decode_gif 全文件读入内存 → File + BufReader 流式 | ✅ 已修复 |
| W13 | Medium | close_wallpaper_by_path 精确字符串比较 → 路径规范化后比较 | ✅ 已修复 |

### A.6.4 audio 模块（3 项，A01-A03）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| A01 | Medium | with_session GetProcessId 失败中止枚举 → continue 跳过 | ✅ 已修复 |
| A02 | Low | set_process_volume 不校验范围 → clamp(0.0, 1.0) | ✅ 已修复 |
| A03 | Low | session_cache 无主动过期 → 枚举路径顺带清理 | ✅ 已修复 |

### A.6.5 ipc 模块（5 项，I01-I05）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| I01 | Medium | read_response_line_with_timeout 循环无总体截止时间 → deadline 检查 | ✅ 已修复 |
| I02 | **High** | read_line_with_timeout MAX_LINE_BYTES 检查在 read_line 之后 → read_until 手动检查 | ✅ 已修复 |
| I03 | Medium | send_command_with_timeout 响应匹配循环无总体截止时间 → deadline 检查 | ✅ 已修复 |
| I04 | Low | WpProcIpcClient send_command 5s 超时不一致 → 15s 与 connect 20s 匹配 | ✅ 已修复 |
| I05 | Low | connect_named_pipe sleep 阻塞 async → 文档标注或 async 变体 | ✅ 已修复 |

### A.6.6 process 模块（5 项，P01-P05）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| P01 | Medium | is_running 使用 STILL_ACTIVE (259) 判断 → 改用 WaitForSingleObject | ✅ 已修复 |
| P02 | Low | stop/stop_handle WAIT_FAILED 静默吞错 → 增加 warn + TerminateProcess 兜底 | ✅ 已修复 |
| P03 | Low | pid() 进程退出后仍返回旧 PID → 检查 is_running | ✅ 已修复 |
| P04 | Low | start/stop 阻塞 Win32 API 未标注 → 文档标注 blocking | ✅ 已修复 |
| P05 | Low | stop/stop_handle 逻辑重复 → 提取 wait_and_terminate | ✅ 已修复 |

### A.6.7 src-tauri 模块（16 项，T01-T16）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| T01 | Medium | set_speed 命令持锁期间 await → fire-and-forget | ✅ 已修复 |
| T02 | **High** | fullscreen.rs GetMessageW 返回 -1 误判 → match ret.0 模式 | ✅ 已修复 |
| T03 | Medium | pause/resume 使用 unwrap_or_default → resolve_display_id 统一 | ✅ 已修复 |
| T04 | Medium | perform_shutdown_blocking engine 锁无超时 → 3s 超时强制退出 | ✅ 已修复 |
| T05 | Low | THUMBNAIL_TASK 连续调用覆盖 handle → Vec 或 JoinSet | ✅ 已修复 |
| T06 | Low | 托盘菜单每次 spawn 新线程 → spawn_blocking 复用线程池 | ✅ 已修复 |
| T07 | Low | workerw_check 重新初始化不 emit 事件 → emit desktop-status-changed | ✅ 已修复 |
| T08 | Low | validate_wallpaper_file_path 不解析符号链接 → canonicalize | ✅ 已修复 |
| T09 | Low | update_config 持 engine 锁调用 set_gif_memory_strategy → 内部加锁 | ✅ 已修复 |
| T10 | Low | is_foreground_fullscreen 缓冲区 256 字符 → 常量 + 注释 | ✅ 已修复 |
| T11 | Low | EXPLORER_DESKTOP.set 二次调用 Err 被丢弃 → match + debug | ✅ 已修复 |
| T12 | Low | set_on_config_changed 回调无 catch_unwind → 包裹 catch_unwind | ✅ 已修复 |
| T13 | Low | power.rs GetSystemPowerStatus 失败静默退出 → 增加 warn | ✅ 已修复 |
| T14 | Low | check_desktop_status 返回值语义不清 → 返回实际重初始化 bool | ✅ 已修复 |
| T15 | Low | set_wallpaper 3 阶段锁并发 renderer 泄漏 → per-display 标志 | ✅ 已修复 |
| T16 | Low | 主线程 COM 初始化无 RAII guard → 引入 ComGuard | ✅ 已修复 |

### A.6.8 wp-proc 模块（14 项，WP01-WP14）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| WP01 | **High** | build_pipe_security_attributes token_handle 未 CloseHandle → OwnedHandle RAII | ✅ 已修复 |
| WP02 | **High** | ipc_thread read_line 无读取上限 → read_line_with_limit 增量检查 | ✅ 已修复 |
| WP03 | **High** | WebView2 创建失败仍启动 IPC 服务 → 返回 Err 退出子进程 | ✅ 已修复 |
| WP04 | Medium | create_webview 回调 error_code? 提前返回 tx.send 不执行 → 先 send | ✅ 已修复 |
| WP05 | Medium | Pause/Resume CoreWebView2 失败静默跳过 → 返回 Error | ✅ 已修复 |
| WP06 | Medium | SetPosition SetWindowPos 失败仅 warn → 返回 Error | ✅ 已修复 |
| WP07 | Medium | ComGuard RPC_E_CHANGED_MODE 仅 warn 继续 → 返回 Err 或明确日志 | ✅ 已修复 |
| WP08 | Medium | create_webview GetClientRect 失败用默认 0x0 → 用 cli.rect 构造 | ✅ 已修复 |
| WP09 | Medium | ipc_thread PostMessageW 失败仅 warn → 直接构造错误响应 | ✅ 已修复 |
| WP10 | Low | Terminate 命令 break 后消息流不完整 → 显式 PostQuitMessage | ✅ 已修复 |
| WP11 | Low | wait_with_pump_timeout Sleep(1) 注释误导 → 更新为 ~15ms | ✅ 已修复 |
| WP12 | Low | ipc_thread read_line 重试未 line.clear() → 重试前清空 | ✅ 已修复 |
| WP13 | Low | ShowWindow 在 create_webview 之前 → 移到成功之后 | ✅ 已修复 |
| WP14 | Low | default_rect 用主显示器尺寸 → 接收 display_id 或文档注释 | ✅ 已修复 |

### A.6.9 前端 UI 模块（12 项，F01-F12）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| F01 | Medium | #preview-video/#preview-web 内联 style 违反 CSP → 改为 class | ✅ 已修复 |
| F02 | Medium | 滑块 parseInt/parseFloat 未校验 NaN → Number.isNaN 校验 | ✅ 已修复 |
| F03 | Medium | loadConfig 失败静默吞错 → showStatus 提示 | ✅ 已修复 |
| F04 | Medium | getErrorMessage 向用户泄露内部细节 → 脱敏映射表 | ✅ 已修复 |
| F05 | Low | renderWallpaperList 无虚拟化/分页 → IntersectionObserver 懒渲染 | ✅ 已修复 |
| F06 | Low | Web 预览 iframe 执行不可信 HTML → 静态截图或移除 allow-scripts | ✅ 已修复 |
| F07 | Low | 外链缺少 rel="noopener noreferrer" → 添加属性 | ✅ 已修复 |
| F08 | Low | populateDisplaySelect 未处理空数组 → showStatus + 禁用控件 | ✅ 已修复 |
| F09 | Low | invoke 包装无超时机制 → invokeWithTimeout 工具函数 | ✅ 已修复 |
| F10 | Low | debounce 使用 any[] 弱化类型 → unknown[] + narrowing | ✅ 已修复 |
| F11 | Low | beforeunload 注册位置过晚 → 提前到 init() 开头 | ✅ 已修复 |
| F12 | Low | refreshWallpaperList 无请求取消机制 → 请求序号校验 | ✅ 已修复 |

### A.6.10 构建基础设施模块（9 项，B01-B09）

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| B01 | Medium | asset scope allow 含 $HOME/**/* → 收紧为 $APPDATA/mirrorstar/**/* | ✅ 已修复 |
| B02 | Medium | release.yml 未配置代码签名 → 配置 TAURI_SIGNING_PRIVATE_KEY | ✅ 已修复 |
| B03 | Low | withGlobalTauri: true → 改为 false | ✅ 已修复 |
| B04 | Low | no-explicit-any off → 改为 warn | ✅ 已修复 |
| B05 | Low | no-unused-vars warn → 改为 error | ✅ 已修复 |
| B06 | Low | ci.yml 缺 npm audit → 增加 npm audit --omit=dev --audit-level=high | ✅ 已修复 |
| B07 | Low | tauri.dev.conf.json 硬编码绝对路径 → 改用相对路径 | ✅ 已修复 |
| B08 | Low | vitest.config.ts 无 coverage threshold → 增加 thresholds | ✅ 已修复 |
| B09 | Low | capabilities core:default 宽泛 → 拆分最小必要权限 | ✅ 已修复 |

---

## A.7 汇总统计

### A.7.1 各轮次修复数量

| 轮次 | 日期 | 修复数 | 累计 |
|------|------|--------|------|
| v1.0 → v2.1 | — | 53 + 1 部分修复 | 53 |
| v3.1 | 2026-07-04 | 24 | 77 |
| v3.2 | 2026-07-04 | 33（含 5 个 TODO，v3.3 修复） | 110 |
| v3.3 | 2026-07-05 | 6（TODO 清理） | 116 |
| v3.4 | 2026-07-07 | 14 | 130 |
| v3.5 | 2026-07-12 | 100 | 230 |
| **合计** | | **231**（含 1 部分修复） | |

### A.7.2 v3.5 各模块 findings 分布

| 模块 | 编号前缀 | 总数 | Critical | **High** | Medium | Low |
|------|---------|------|----------|----------|--------|-----|
| config | C | 11 | 0 | **1**（C01） | 5 | 5 |
| desktop | D | 12 | 0 | **1**（D01） | 6 | 5 |
| wallpaper | W | 13 | 0 | 0 | 6 | 7 |
| audio | A | 3 | 0 | 0 | 1 | 2 |
| ipc | I | 5 | 0 | **1**（I02） | 2 | 2 |
| process | P | 5 | 0 | 0 | 1 | 4 |
| src-tauri | T | 16 | 0 | **1**（T02） | 3 | 12 |
| wp-proc | WP | 14 | 0 | **3**（WP01/WP02/WP03） | 6 | 5 |
| 前端 | F | 12 | 0 | 0 | 4 | 8 |
| 构建 | B | 9 | 0 | 0 | 2 | 7 |
| **合计** | | **100** | **0** | **7** | **36** | **57** |

### A.7.3 测试演进

| 版本 | Rust 测试 | 前端测试 | 合计 |
|------|----------|---------|------|
| v3.0 基线 | ~113 | ~45 | ~158 |
| v3.1 | 374 | 86 | 460 |
| v3.2 | 400 | 151 | 551 |
| v3.5 后 | 400+ | 184 | 584+ |

### A.7.4 v4.0 回退/不完整修复关联

v4.0 审查发现部分 v3.x 修复存在回退或不完整，已在各模块文档的"v3.x 已修复问题"表格中以 ⚠️ 标注。详见各模块文档：

- config 模块：C-001~C-018 中 7 项 ⚠️ 标注（见 [02-config模块.md](./02-config模块.md)）
- desktop 模块：见 [03-desktop模块.md](./03-desktop模块.md)
- wallpaper 模块：见 [04-wallpaper模块.md](./04-wallpaper模块.md)
- audio/ipc/process 模块：见 [05-audio-ipc-process模块.md](./05-audio-ipc-process模块.md)
- src-tauri 模块：见 [06-src-tauri应用层.md](./06-src-tauri应用层.md)
- wp-proc 模块：见 [07-wp-proc子进程.md](./07-wp-proc子进程.md)
- 前端模块：见 [08-前端.md](./08-前端.md)
- 构建基础设施模块：见 [09-构建基础设施.md](./09-构建基础设施.md)
