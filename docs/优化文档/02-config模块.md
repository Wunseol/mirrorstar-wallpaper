# config 模块优化文档

> [← 返回索引](./README.md)

## 模块概要

- **模块路径**：`crates/mirrorstar-core/src/config/`
- **审查文件**：6 个（约 4,384 行）
  - `mod.rs`（16 行）
  - `settings.rs`（609 行）— 配置结构体定义
  - `manager.rs`（2014 行）— ConfigManager 配置读写与热重载
  - `thumbnail.rs`（627 行）— 缩略图生成（图片/视频）
  - `detect.rs`（576 行）— 类型检测（magic byte + 扩展名）
  - `hot_reload.rs`（542 行）— 文件监听与热重载
- **核心结构**：`WallpaperEntry`、`WallpaperMetadata`、`WallpaperLibrary`、`DisplayInfo`、`ConfigManager`
- **设计模式**：`Arc<RwLock>` 共享读写、`atomic_write` 原子写入（fs2 文件锁 + 临时文件 + rename）、`maybe_save_config` 300ms 防抖保存、`start_watching` notify 热重载（500ms 防抖）、`start_periodic_save` 30 秒周期性后台保存
- **依赖**：`serde` + `toml` + `notify` + `fs2` + `image`

### v3.4 新增能力

- **magic byte 内容嗅探类型检测**（`detect_wallpaper_type_by_content`）：读取文件前 12 字节判断真实类型
- **v3.4 Video 缩略图生成**（`generate_video_thumbnail`）：通过 ffmpeg CLI 抽取首帧，复用 `generate_thumbnail_from_image_file` 的 resize+JPEG 编码逻辑
- **v3.4 ffmpeg 可用性检测**（`is_ffmpeg_available`）：执行 `ffmpeg -version` 探测
- **v3.4 损坏文件清理**（`cleanup_corrupted_thumbnails`）：扫描 `thumbnails/` 目录删除 0 字节文件

## v4.0 审查发现（18 项）

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/01-config.md`
> 严重级别分布：Critical 0 / High 3 / Medium 6 / Low 9
> 维度分布：逻辑 2 | 并发 3 | 资源 2 | 错误 3 | 性能 1 | 安全 3 | 可维护性 4

### 审查重点说明

项目经过 v3.0→v3.5 共 5 轮修复（C01-C11、N-005/N-006/N-007/N-008 等）。本次审查重点关注：修复完整性、修复引入的新问题、遗留债务。

### [C-001] [High] [并发安全] manager.rs:561-598 — `maybe_save_config` dirty 标志竞态导致配置丢失

**描述**：`maybe_save_config` 中 `dirty` 标志存在 check-then-act 竞态。问题场景（线程 A 持有 `save_mutex` 保存，线程 B 并发调用 `update_config`）：
1. 线程 A：`config.read().clone()` 读取配置 V1
2. 线程 B：`config.write()` 写入 V2，释放写锁，`dirty.store(true)`
3. 线程 A：`save_config_to_file(&V1)` 将 V1 写入磁盘
4. 线程 A：`dirty.store(false)` —— 清除了线程 B 刚设置的 `dirty=true`
5. 线程 B：`maybe_save_config` 获取 `save_mutex`，看到 `dirty=false`，直接返回

结果：内存中为 V2，磁盘上为 V1，`dirty=false`。V2 不会落盘，应用退出时 `flush` 检查 `dirty`（false）不保存，V2 在重启后丢失。对比 `flush`（line 534）使用 `swap` 原子地 check-and-clear，`maybe_save_config` 使用 `load` + `store` 非原子模式，是 N-005 修复引入 save_mutex 后遗留的竞态。

**建议**：将 `maybe_save_config` 改为与 `flush` 一致的 `swap` 模式，debounce 检查通过后执行 `self.dirty.swap(false, Ordering::Relaxed)`，保存失败时回滚 `dirty.store(true)`。

### [C-002] [High] [资源管理] thumbnail.rs:118-133 — C06 解压炸弹防护不完整（检查在解码之后）

**描述**：`image::open` 在尺寸/缓冲区检查前已将整图解码到内存。`image::open` 内部调用 `load` → `decode`，将完整像素数据读入 `DynamicImage`。对于压缩格式（PNG with deflate、TIFF with LZW），100MB 的压缩文件可解码为数 GB 的像素缓冲区。`MAX_DECODED_PIXEL_BUFFER_SIZE`（200MB）和 `MAX_IMAGE_DIMENSION`（20000）的检查发生在解码之后，此时内存已分配，OOM 已发生。文件大小检查（`MAX_THUMBNAIL_FILE_SIZE = 100MB`）仅限制压缩后大小，无法阻止高压缩比的解压炸弹。

**建议**：在 `image::open` 前使用 `image::ImageReader::new(File::open(path)?)` 设置 `set_limits`（image crate 0.25+ 提供 `ImageFormatLimits`），限制解码时的最大尺寸与最大分配内存。

### [C-003] [High] [错误处理] manager.rs:362-383 — `notify_config_error` 回调未用 `catch_unwind` 包裹

**描述**：`notify_config_error` 调用回调时未用 `catch_unwind` 包裹，与 `reload_config_and_library` 中 `on_config_changed` 回调的处理不一致（后者在 hot_reload.rs:175-184 用 `catch_unwind` 包裹）。`notify_config_error` 被 `reload_config_and_library`（hot_reload.rs:156, 159）调用，运行在 watcher 线程中。若 Tauri 层设置的 `on_config_error` 回调 panic，watcher 线程会退出，此后配置文件变更不再被检测，热重载功能静默失效。T12 修复已为 `on_config_changed` 回调添加 `catch_unwind` 防护，但 `on_config_error` 回调被遗漏。

**建议**：与 `on_config_changed` 保持一致，用 `catch_unwind` 包裹 `on_config_error` 回调调用。

### [C-004] [Medium] [并发安全] hot_reload.rs:144-151 — C11 原子 reload 实际非原子（窗口期不一致）

**描述**：C11 修复声称"原子地热重载 config 与 library"，但实际替换操作非原子 —— config 与 library 在两个独立的锁获取中依次替换。在两者均成功替换之间存在窗口期，其他线程可能观察到 new_config + old_library 的不一致状态。实际影响有限（窗口期极短，且多数 Tauri 命令只读取其中之一），但与注释声称的"原子性"不符。

**建议**：若一致性至关重要，引入统一的 `RwLock<(AppConfig, WallpaperLibrary)>`；否则修正注释，明确说明"加载失败时原子回滚，但成功替换时有短暂窗口期"。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：采用方案 ②，修正 `reload_config_and_library` 注释，明确区分"加载失败时原子回滚"与"成功替换时存在短暂窗口期"，移除"保证 config 与 library 始终一致"等误导性表述。

### [C-005] [Medium] [安全] settings.rs:222-230 — `GifConfig.validate()` 缺少 `balanced_keep_frames` 上限校验

**描述**：`GifConfig.validate()` 仅检查 `balanced_keep_frames < 1`，无上限校验。用户可通过手动编辑 `config.toml` 设置 `balanced_keep_frames = 999999999`。过大的值会导致 GIF 解码后保留大量帧在内存中，可能触发 OOM。C02 修复覆盖了下限（`< 1`），但遗漏了上限。

**建议**：添加合理上限（如 1000）。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：在 `GifConfig::validate()` 中增加 `MAX_BALANCED_KEEP_FRAMES = 1000` 上限校验，越界时回退到 `default_gif_keep_frames()` 并记录 `tracing::warn!`。

### [C-006] [Medium] [可维护性] thumbnail.rs:150-153 — 使用 `DefaultHasher` 生成持久化文件名（跨版本不稳定）

**描述**：缩略图文件名使用 `std::collections::hash_map::DefaultHasher` 生成哈希。`DefaultHasher` 文档明确指出算法不保证跨版本稳定。Rust 版本升级后，旧缩略图文件名不再匹配新哈希，导致：① 旧缩略图成为孤儿文件；② 同一文件重复生成缩略图。同样问题存在于 `generate_video_thumbnail`（thumbnail.rs:274-277）的临时帧文件名。

**建议**：使用稳定的哈希算法（如 `xxhash`、`sha2::Sha256` 截断，或简单的 FNV-1a）生成持久化文件名。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：抽取 `thumbnail_name_from_path(source_path: &Path) -> String` 辅助函数，将路径字节转 hex 字符串作为文件名（如 `thumb_{hex}.jpg`），替代 `DefaultHasher`，无新依赖。

### [C-007] [Medium] [并发安全] thumbnail.rs:273-277 — `generate_video_thumbnail` 临时帧文件名竞争

**描述**：`generate_video_thumbnail` 的临时帧文件名基于源视频路径哈希，同一视频的并发调用会使用相同的临时文件路径，导致竞争。注释称"基于源路径哈希，避免并发冲突"，但这与实际相反 —— 不同视频哈希不同（避免冲突），但同一视频的并发调用哈希相同，会竞争同一个 `_tmp_frame_{hash}.jpg` 文件。

**建议**：使用唯一临时文件名（如加入线程 ID、PID 或 `SystemTime::now().as_nanos()`）。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：抽取 `tmp_frame_name_from_path` 辅助函数，在临时帧文件名后追加 `SystemTime::now().as_nanos()` 高熵时间戳，确保同一视频并发调用生成不同临时文件名。

### [C-008] [Medium] [错误处理] detect.rs:73-76 — `f.read(&mut buf).unwrap_or(0)` 静默吞掉读取错误

**描述**：`detect_wallpaper_type_by_content` 中 `f.read(&mut buf).unwrap_or(0)` 静默吞掉读取错误。若 `File::open` 成功但 `read` 失败（如网络路径中断、磁盘 IO 错误），错误被完全隐藏，无日志输出。

**建议**：记录读取失败的警告。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：将 `f.read(&mut buf).unwrap_or(0)` 改为 `match` 分支，读取失败时记录 `tracing::warn!(error, path, "读取文件头部失败，回退到扩展名检测")` 并显式 `return detect_wallpaper_type(file_path)` 回退检测。

### [C-009] [Medium] [逻辑] manager.rs:306-313 — C02 修复不完整（`update_config` 不调用 `validate()`）

**描述**：C02 修复的 `validate()` 仅在 `load_config`（manager.rs:670）调用，但 `update_config`（manager.rs:306-313）直接替换内存配置而不调用 `validate()`。若前端 Tauri 命令收到用户输入的配置（如 `volume=1.5`、`speed=-2.0`、`balanced_keep_frames=0`），`update_config` 会直接存入内存并落盘，不经过校验。C02 修复的 clamp 逻辑仅在下次 `load_config`（重启后）才会生效。

**建议**：在 `update_config` 中调用 `validate()`。

> ✅ **已修复于 v4.0 Wave 2B**（spec: `fix-v40-wave2b-config-medium-findings`）：在 `update_config` 签名中改为 `mut config: AppConfig`，写入内存前调用 `config.validate()`，与 `load_config` 入口校验对齐。

### [C-010] [Low] [可维护性] manager.rs:639, 703 — `MAX_CONFIG_SIZE` 常量重复定义

**描述**：`MAX_CONFIG_SIZE` 常量在 `load_config`（line 639）和 `load_library`（line 703）中重复定义，两处定义相同。

**建议**：提取为模块级常量。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：提取 `MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024` 模块级常量，替换 `load_config`/`load_library` 中重复定义。

### [C-011] [Low] [逻辑] detect.rs:94 — GIF 魔数检测仅检查前 3 字节（应校验完整 6 字节）

**描述**：GIF 魔数检测仅检查前 3 字节 `GIF`，未校验完整 6 字节魔数（`GIF87a` / `GIF89a`）。注释声称检测 `GIF87a` / `GIF89a`，但实际只比较了前 3 字节。任何以 `GIF` 开头的文件都会被误判为 Gif 类型。

**建议**：校验完整 6 字节。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：GIF 魔数检测改为完整 6 字节校验 `GIF87a`/`GIF89a`（原仅检查前 3 字节 `GIF`）。

### [C-012] [Low] [性能] detect.rs:155-159 — `detect_html` 分配 `Vec<u8>` 小写化（可避免分配）

**描述**：`detect_html` 分配 `Vec<u8>` 对整个 body 做小写化后逐窗口比较。`body` 最多 256 字节，分配开销很小，但批量添加壁纸场景下会产生数百次小分配。

**建议**：使用 `windows().any(|w| w.eq_ignore_ascii_case(pattern))` 避免分配。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`detect_html` 改用 `windows().any(|w| w.eq_ignore_ascii_case(pattern))` 消除 `Vec<u8>` 小写化分配。

### [C-013] [Low] [安全] thumbnail.rs:219-225 — `escape_ffmpeg_input` 防护范围有限

**描述**：`escape_ffmpeg_input` 仅处理以 `-` 开头的路径，未覆盖所有 ffmpeg 参数注入向量（如 `:`、`@`、`|` 可能被解释为协议/过滤器语法）。C04 修复了 `-` 前缀导致的选项注入，但若未来支持 URL 输入，此防护不足。

**建议**：在文档中明确防护范围，并在 `generate_video_thumbnail` 入口校验 `file_path` 为本地文件路径。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`generate_video_thumbnail` 入口校验拒绝含 `://` 的路径（防 ffmpeg 协议注入），返回 `MirrorStarError::InvalidPath`。

### [C-014] [Low] [错误处理] thumbnail.rs:200-208 — `is_ffmpeg_available` 静默返回 false（不区分错误原因）

**描述**：`is_ffmpeg_available` 在 `Command::new("ffmpeg")` 失败时静默返回 `false`，不记录错误原因。`Err` 分支可能是 `NotFound`（未安装）或 `PermissionDenied`（无执行权限），静默返回使调用方无法区分。

**建议**：记录非 NotFound 错误。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`is_ffmpeg_available` 对非 `NotFound` 错误记录 `tracing::warn!`（错误分级）。

### [C-015] [Low] [可维护性] manager.rs:792-794 — `normalize_path` Windows 专属逻辑不可移植

**描述**：`normalize_path` 将 `/` 替换为 `\`，是 Windows 专属逻辑。若项目未来需要支持 Linux/macOS，此函数会产生错误的规范化结果。

**建议**：使用 `std::path::MAIN_SEPARATOR` 或 `cfg!(windows)` 条件编译。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`normalize_path` 用 `cfg!(windows)` 条件编译包裹 `/` → `\` 替换（保留 `to_lowercase()`），非 Windows 保留原分隔符。

### [C-016] [Low] [可维护性] manager.rs:275 — `periodic_save_running` 初始状态与实际不符

**描述**：`new_in_dir` 中 `periodic_save_running` 初始化为 `true`，但此时周期保存线程尚未启动（`start_watching` 未调用），初始状态与实际不符。

**建议**：初始化为 `false`，在 `start_periodic_save` 中设为 `true`。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`new_in_dir` 中 `periodic_save_running` 初始化为 `false`，`start_periodic_save` 成功后设 `true`。

### [C-017] [Low] [安全] settings.rs:175-184 — `VideoConfig.validate()` 缺少 `speed` 上限校验

**描述**：`VideoConfig.validate()` 仅检查 `speed <= 0.0 || is_nan()`，无上限校验。用户可设置 `speed = 99999.0`，过大的播放速度可能导致播放器行为异常、CPU 占用激增。

**建议**：添加上限（如 10.0）。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`VideoConfig::validate` 新增 `MAX_VIDEO_SPEED: f32 = 10.0` 模块级常量与上限校验，越界回退 `1.0`。

### [C-018] [Low] [资源管理] manager.rs:810-816 — `atomic_write` 锁文件永不清理

**描述**：`atomic_write` 创建的锁文件（`<path>.lock`）永不清理，会残留在数据目录中。实际影响极小（每个锁文件 0 字节，最多 2 个）。

**建议**：可在 `cleanup_corrupted_thumbnails` 或启动时清理无对应配置文件的孤儿 `.lock` 文件（可选，低优先级）。

**修复状态**：✅ 已修复于 v4.0 Wave 3A（spec: `fix-v40-wave3a-config-low-findings`）：`cleanup_corrupted_thumbnails` 末尾追加孤儿 `.lock` 文件清理逻辑。

## v3.x 已修复问题

| ID | 严重级别 | 描述 | 状态 |
|----|---------|------|------|
| C01 | High | `load_config`/`load_library` TOML 解析失败静默回退默认配置 | ✅ 已修复（v3.5.1） |
| C02 | Medium | `AudioConfig.volume`/`VideoConfig.speed`/`GifConfig.balanced_keep_frames` 缺少范围校验 | ✅ 已修复（v3.5.2）— ⚠️ v4.0 C-009 发现 `update_config` 入口仍遗漏 |
| C03 | Medium | `cleanup_corrupted_thumbnails` 使用 `Self::data_dir()` 而非实例路径 | ✅ 已修复（v3.5.2） |
| C04 | Medium | `generate_video_thumbnail` 参数注入（`-` 开头路径） | ✅ 已修复（v3.5.2）— ⚠️ v4.0 C-013 发现防护范围有限 |
| C05 | Medium | `is_ffmpeg_available` 同步阻塞调用 | ✅ 已修复（v3.5.2） |
| C06 | Medium | 解压炸弹防护不完整（基于压缩后大小） | ✅ 已修复（v3.5.2）— ⚠️ v4.0 C-002 发现检查在解码之后 |
| C07 | Low | `atomic_write` 临时文件残留 | ✅ 已修复（v3.5.3） |
| C08 | Low | `load_config`/`load_library` TOCTOU 窗口 | ✅ 已修复（v3.5.3） |
| C09 | Low | `update_thumbnail` 路径匹配不一致 | ✅ 已修复（v3.5.3） |
| C10 | Low | `start_periodic_save` 线程启动失败状态不一致 | ✅ 已修复（v3.5.3）— ⚠️ v4.0 C-016 发现初始状态仍有问题 |
| C11 | Low | `start_watching` config 与 library 非原子 reload | ✅ 已修复（v3.5.3）— ⚠️ v4.0 C-004 发现实际仍非原子 |
| N-005 | — | save_mutex 串行化 | ✅ 已修复 — ⚠️ v4.0 C-001 发现引入 dirty 竞态 |
| N-006 | — | library 落盘串行化 | ✅ 已修复 |
| N-007 | — | WebM/EBML 魔数检测 | ✅ 已修复 |
| N-008 | — | HTML 检测扩展 | ✅ 已修复 |
| 热重载失效 | — | watcher 移入线程闭包 | ✅ 已修复（v1.0） |
| 防抖保存丢数据 | — | 新增 start_periodic_save | ✅ 已修复（v1.0） |
| 类型检测仅扩展名 | — | magic byte 嗅探 | ✅ 已修复（v1.0） |
| 解析失败静默回退 | — | 返回 Result 错误传播 | ✅ 已修复（v1.0） |
| DynamicImage 分配 | — | 使用 into_rgb8() | ✅ 已修复（v1.0） |

## 优化目标与方案

### v4.0 优先修复（High，3 项）

1. **C-001 dirty 标志竞态**：将 `maybe_save_config` 改为 `swap` 原子 check-and-clear 模式，与 `flush` 保持一致。保存失败时回滚 `dirty`。
2. **C-002 解压炸弹防护前置**：使用 `image::ImageReader::set_limits` 在解码前限制最大尺寸与分配内存。
3. **C-003 `notify_config_error` catch_unwind**：与 `on_config_changed` 一致，用 `catch_unwind` 包裹回调调用。

### v4.0 系统性修复（Medium，6 项）

4. **C-004 原子 reload**：引入统一 `RwLock<(AppConfig, WallpaperLibrary)>` 或修正注释
5. **C-005 balanced_keep_frames 上限**：添加 `MAX_BALANCED_KEEP_FRAMES` 常量
6. **C-006 稳定哈希**：替换 `DefaultHasher` 为 `xxhash` 或 `sha2`
7. **C-007 唯一临时文件名**：加入时间戳或 PID
8. **C-008 读取错误日志**：`unwrap_or(0)` 改为 match + warn
9. **C-009 `update_config` 校验**：入口调用 `validate()`

### v4.0 渐进优化（Low，9 项）

10-18. 常量提取（C-010）、GIF 魔数完整校验（C-011）、HTML 检测避免分配（C-012）、ffmpeg 防护文档化（C-013）、ffmpeg 探测错误日志（C-014）、路径规范化跨平台（C-015）、periodic_save_running 初始值（C-016）、speed 上限校验（C-017）、锁文件清理（C-018）。
