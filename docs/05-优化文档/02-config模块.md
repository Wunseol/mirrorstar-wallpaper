# config 模块优化文档

> [← 返回索引](./README.md)

## 1. 模块概览 / 现状

### 1.1 模块职责

config 模块是 mirrorstar-core 的配置与壁纸库管理中枢，聚合五类职责：① 应用配置 `AppConfig` 的结构定义与范围校验（`settings.rs`）；② 配置与壁纸库的加载、持久化、原子写入与增删改查（`manager.rs` 中的 `ConfigManager`）；③ 文件监视与热重载（`hot_reload.rs`，通过 `impl ConfigManager` 扩展）；④ 壁纸类型检测（`detect.rs`，扩展名 + 魔数双路径）；⑤ 缩略图生成（`thumbnail.rs`，image crate + ffmpeg CLI）。模块通过 `Arc<RwLock<...>>` + `AtomicBool` + `Mutex` 提供线程安全的并发访问，被 Tauri 命令层以 `Arc<ConfigManager>` 形式共享。该模块历经 v3.x→v4.0→v5.0→v6.0 多轮深度修复（C01-C11、C-001~C-018、v41-C-XXX、C-PERF-XXX、C-TD-001~C-TD-032 等数十个 spec），代码中保留了大量的修复痕迹注释。

### 1.2 文件清单与行数

> 行数为 2026-09-01 依据真实代码统计（v6.0 清理后状态；v6 审查文档报的行数为清理前版本，数字偏高，以本节为准）。

| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 45 | 模块导出与 `pub use` 重导出 |
| manager.rs | 2533 | `ConfigManager` 实现、`WallpaperEntry`/`WallpaperLibrary`/`DisplayInfo` 数据模型、`atomic_write`、`normalize_path`、`read_bounded_utf8_file`、`invoke_callback_safe`、`save_with_dirty_rollback`、配置加载错误类型、`#[cfg(test)] mod tests` |
| settings.rs | 735 | `AppConfig` 及子配置（`AudioConfig`/`VideoConfig`/`GifConfig`/`DisplayConfig`/`WebConfig`/`PauseConfig`/`GeneralConfig`）与 `validate()` 范围校验 |
| hot_reload.rs | 549 | `start_watching`/`reload_config_and_library`/`start_periodic_save`/`shutdown_periodic_save`/`stop_watching` |
| detect.rs | 738 | `detect_wallpaper_type`（扩展名）、`detect_wallpaper_type_by_content`（魔数，保留无调用方）、`detect_html` |
| thumbnail.rs | 1168 | `generate_thumbnail`/`generate_video_thumbnail`/`is_ffmpeg_available`/`TmpFrameGuard` RAII 守卫 |
| validation.rs | 33 | 校验相关辅助（v6.0 后新增） |

- **模块路径**：`crates/mirrorstar-core/src/config/`
- **核心结构**：`WallpaperEntry`、`WallpaperMetadata`、`WallpaperLibrary`、`DisplayInfo`、`ConfigManager`
- **设计模式**：`Arc<RwLock>` 共享读写、`atomic_write` 原子写入（fs2 文件锁 + 临时文件 + `sync_all` + rename）、`maybe_save_config` 300ms 防抖保存、`start_watching` notify 热重载（500ms 防抖）、`start_periodic_save` 30 秒周期性后台保存
- **依赖**：`serde` + `toml` + `notify` + `fs2` + `image`

### 1.3 测试覆盖

- `manager.rs` 含约 40 个 `#[test]`，覆盖序列化往返、`atomic_write`、`ConfigManager` 增删改查、`load_config`/`load_library` 损坏回退、dirty 标志竞态（C-001）、配置加载错误通知（C01）、validate clamp（C02/C-009）、cleanup 路径推导（C03）、TOCTOU（C08）、路径规范化（C09）、reload 原子性（C11）、sync_all 等。
- `hot_reload.rs` 含 6 个 `#[test]`，覆盖 spawn 失败清理（C10）、reload 原子性对称测试（C11）、注释一致性（C-004）、watcher_alive 标志。
- `detect.rs` 含约 30 个 `#[test]`，覆盖扩展名/魔数/HTML/BOM 检测、读取错误回退与 warn 日志（C-008）。
- `thumbnail.rs` 含约 20 个 `#[test]`，覆盖缩略图生成、解压炸弹防护（C-002）、文件名稳定性（C-006）、临时文件名唯一性（C-007）、`TmpFrameGuard`。
- `settings.rs` 含约 15 个 `#[test]`，覆盖默认值、TOML 往返、validate clamp（C-005/C-017 等）。

测试盲区见第 5 节。

## 2. v4.0 审查发现（18 项）与修复状态

> 来源：`.trae/specs/comprehensive-project-review-and-doc-restructure-2026-07-15/findings/01-config.md`
> 严重级别分布：Critical 0 / High 3 / Medium 6 / Low 9
> 维度分布：逻辑 2 | 并发 3 | 资源 2 | 错误 3 | 性能 1 | 安全 3 | 可维护性 4
>
> ✅ 全部 18 项已核验为 ✅ 已修复。真实代码位置为 2026-09-01 核验结果（v6.0 清理后坐标）。

### [C-001] [High] [并发安全] manager.rs:561-598 — `maybe_save_config` dirty 标志竞态导致配置丢失

**描述**：`maybe_save_config` 中 `dirty` 标志存在 check-then-act 竞态。问题场景（线程 A 持有 `save_mutex` 保存，线程 B 并发调用 `update_config`）：
1. 线程 A：`config.read().clone()` 读取配置 V1
2. 线程 B：`config.write()` 写入 V2，释放写锁，`dirty.store(true)`
3. 线程 A：`save_config_to_file(&V1)` 将 V1 写入磁盘
4. 线程 A：`dirty.store(false)` —— 清除了线程 B 刚设置的 `dirty=true`
5. 线程 B：`maybe_save_config` 获取 `save_mutex`，看到 `dirty=false`，直接返回

结果：内存中为 V2，磁盘上为 V1，`dirty=false`。V2 不会落盘，应用退出时 `flush` 检查 `dirty`（false）不保存，V2 在重启后丢失。

**修复状态**：✅ 已修复 — 改为与 `flush` 一致的 `swap` 原子 check-and-clear（`save_with_dirty_rollback`，manager.rs:1091;`maybe_save_config` manager.rs:833），保存失败时回滚 `dirty`。含 `c001_dirty_flag_race` 系列测试。

### [C-002] [High] [资源管理] thumbnail.rs:118-133 — C06 解压炸弹防护不完整（检查在解码之后）

**描述**：`image::open` 在尺寸/缓冲区检查前已将整图解码到内存，解压炸弹会导致 OOM。

**修复状态**：✅ 已修复 — 改用 `image::ImageReader::open` + `limits()`（`max_image_width`/`max_image_height`/`max_alloc`），在解码阶段即拒绝超限图像（thumbnail.rs:237-245）。

### [C-003] [High] [错误处理] manager.rs:362-383 — `notify_config_error` 回调未用 `catch_unwind` 包裹

**描述**：`notify_config_error` 调用回调时未用 `catch_unwind` 包裹，回调 panic 会导致 watcher 线程退出、热重载静默失效。

**修复状态**：✅ 已修复 — 与 `on_config_changed` 一致，用 `catch_unwind` 包裹回调（`invoke_callback_safe`，manager.rs:1066-1067;调用点 manager.rs:458-461）。

### [C-004] [Medium] [并发安全] hot_reload.rs:144-151 — C11 原子 reload 实际非原子（窗口期不一致）

**描述**：config 与 library 在两个独立锁中依次替换，替换成功间存在短暂窗口期，其他线程可能观察到 `new_config + old_library` 的不一致状态。

**修复状态**：✅ 已修复 — 采用方案 ②，修正 `reload_config_and_library` 注释，明确区分"加载失败时原子回滚"与"成功替换时存在短暂窗口期"（hot_reload.rs:116-129）。

### [C-005] [Medium] [安全] settings.rs:222-230 — `GifConfig.validate()` 缺少 `balanced_keep_frames` 上限校验

**描述**：仅检查下限，无上限校验，用户可设 `balanced_keep_frames = 999999999` 触发 OOM。

**修复状态**：✅ 已修复 — 新增 `MAX_BALANCED_KEEP_FRAMES = 1000` 上限校验，越界回退 `default_gif_keep_frames()` 并 `tracing::warn!`（settings.rs:275,290）。

### [C-006] [Medium] [可维护性] thumbnail.rs:150-153 — 使用 `DefaultHasher` 生成持久化文件名（跨版本不稳定）

**描述**：`DefaultHasher` 算法不保证跨版本稳定，Rust 升级后旧缩略图成为孤儿、同一文件重复生成。

**修复状态**：✅ 已修复 — 抽取 `thumbnail_name_from_path`，用路径字节 hex 编码作文件名（`thumb_{hex}.jpg`），替代 `DefaultHasher`（thumbnail.rs:152）。

### [C-007] [Medium] [并发安全] thumbnail.rs:273-277 — `generate_video_thumbnail` 临时帧文件名竞争

**描述**：同一视频并发调用会竞争同一临时帧文件。

**修复状态**：✅ 已修复 — `tmp_frame_name_from_path` 追加纳秒时间戳 + 进程内全局 `AtomicU64` 计数器，保证进程内单调唯一（thumbnail.rs:174,176）。

### [C-008] [Medium] [错误处理] detect.rs:73-76 — `f.read(&mut buf).unwrap_or(0)` 静默吞掉读取错误

**描述**：读取失败被完全隐藏，无日志输出。

**修复状态**：✅ 已修复 — 改为 `match` 分支，读取失败记录 `tracing::warn!` 并回退 `detect_wallpaper_type`（detect.rs:73-101）。

### [C-009] [Medium] [逻辑] manager.rs:306-313 — C02 修复不完整（`update_config` 不调用 `validate()`）

**描述**：`update_config` 直接替换内存配置而不校验，用户输入非法值会直接落盘。

**修复状态**：✅ 已修复 — `update_config` 改为 `mut config` 并在写入内存前调用 `config.validate()`（manager.rs:388-389）。

### [C-010] [Low] [可维护性] manager.rs:639, 703 — `MAX_CONFIG_SIZE` 常量重复定义

**描述**：`MAX_CONFIG_SIZE` 在 `load_config`/`load_library` 中重复定义。

**修复状态**：✅ 已修复 — 提取 `MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024` 模块级常量（manager.rs:39）。

### [C-011] [Low] [逻辑] detect.rs:94 — GIF 魔数检测仅检查前 3 字节（应校验完整 6 字节）

**描述**：仅比较前 3 字节 `GIF`，任何以 `GIF` 开头的文件都会被误判。

**修复状态**：✅ 已修复 — 改为完整 6 字节校验 `GIF87a`/`GIF89a`（detect.rs:110）。

### [C-012] [Low] [性能] detect.rs:155-159 — `detect_html` 分配 `Vec<u8>` 小写化（可避免分配）

**描述**：对整个 body 做小写化分配，批量场景产生多次小分配。

**修复状态**：✅ 已修复 — 改用 `windows().any(|w| w.eq_ignore_ascii_case(pattern))` 消除分配（detect.rs:171）。

### [C-013] [Low] [安全] thumbnail.rs:219-225 — `escape_ffmpeg_input` 防护范围有限

**描述**：仅处理 `-` 开头路径，未覆盖 `:`/`@`/`|` 等协议注入向量。

**修复状态**：✅ 已修复 — `generate_video_thumbnail` 入口拒绝含 `://` 的路径（`MirrorStarError::InvalidPath`），并配合 `escape_ffmpeg_input`（thumbnail.rs:411,434,365）。

### [C-014] [Low] [错误处理] thumbnail.rs:200-208 — `is_ffmpeg_available` 静默返回 false（不区分错误原因）

**描述**：`Command::new` 失败静默返回 false，无法区分 `NotFound` 与 `PermissionDenied`。

**修复状态**：✅ 已修复 — 对非 `NotFound` 错误记录 `tracing::warn!`，`NotFound` 静默返回（thumbnail.rs:341-351）。

### [C-015] [Low] [可维护性] manager.rs:792-794 — `normalize_path` Windows 专属逻辑不可移植

**描述**：将 `/` 替换为 `\` 是 Windows 专属逻辑，不可移植。

**修复状态**：✅ 已修复 — 用 `cfg!(windows)` 条件编译包裹 `/`→`\` 替换（manager.rs:1036,1046）。

### [C-016] [Low] [可维护性] manager.rs:275 — `periodic_save_running` 初始状态与实际不符

**描述**：初始化为 `true`，但周期线程尚未启动。

**修复状态**：✅ 已修复 — `new_in_dir` 初始化为 `false`，`start_periodic_save` 成功后再置 `true`（manager.rs:350）。

### [C-017] [Low] [安全] settings.rs:175-184 — `VideoConfig.validate()` 缺少 `speed` 上限校验

**描述**：仅检查 `speed <= 0 || NaN`，无上限，过大的播放速度可能导致播放器异常。

**修复状态**：✅ 已修复 — 新增 `MAX_VIDEO_SPEED: f32 = 10.0` 上限校验，越界回退 `1.0`（settings.rs:240,223）。

### [C-018] [Low] [资源管理] manager.rs:810-816 — `atomic_write` 锁文件永不清理

**描述**：`<path>.lock` 锁文件永不清理。

**修复状态**：✅ 已修复 — `cleanup_orphan_lock_files` 清理无对应主文件的孤儿 `.lock` 文件（manager.rs:688-701）。

## 3. v6.0 技术债清单（C-TD-001 ~ C-TD-032）

> 来源：v6.0 技术债审查（2026-07-25）config 模块技术债清单（第 2 节全量）及清理状态（第 6 节），已并入本文档。

### 3.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-001 | manager.rs:147-150 | `WallpaperLibrary::new()` 是 `Default::default()` 的薄 wrapper，无生产调用点 | 维护负担 | 删除 `new()`，测试改用 `Default::default()` | 低 |
| C-TD-002 | manager.rs:898-911 | `maybe_save_library` 标注 `#[allow(dead_code)]`，v5.0 已引入覆盖的 API，"未来"已到但未使用 | 误导开发者，掩盖死代码 | 删除 `maybe_save_library` | 低 |
| C-TD-003 | thumbnail.rs:49-52 | `TmpFrameGuard::disarm` 标注 `#[allow(dead_code)]`，无调用路径 | 掩盖死代码 | 删除 `disarm` 及对应测试，`armed` 字段省略 | 中 |
| C-TD-004 | detect.rs:70-107 + mod.rs:40 | `detect_wallpaper_type_by_content`（pub fn，117 行 + 文档 + ~25 测试）无调用方，仅在 mod.rs 重导出 | 维护成本，误导调用链 | 评估是否接入命令层，否则删除函数、重导出与测试 | 中 |
| C-TD-005 | manager.rs:390-392 | `is_watcher_alive`（pub fn）无外部调用方 | pub API 无消费者 | 降级 `pub(crate)` 或删除 | 中 |
| C-TD-006 | settings.rs:234-239 | `WebConfig` 及其 `cache_path` 字段无使用 | 无意义配置字段 | 删除 `WebConfig` 与 `AppConfig::web` 或接入实际逻辑 | 中 |

### 3.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-007 | thumbnail.rs:526-538 | `guess_image_format` 仅为测试而 `pub fn` | API 表面积最小化原则 | 改 `pub(crate)` 或 `#[cfg(test)] pub` | 低 |

### 3.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-008 | manager.rs:939-996 vs 1006-1064 | `load_config`/`load_library` 的"有界读取"逻辑高度重复 | 需同步修改，注释重复 | 抽取 `read_bounded_utf8_file` 辅助函数 | 中 |
| C-TD-009 | manager.rs:758-783, 788-840, 899-911, 918-928; hot_reload.rs:260-300 | "dirty.swap → save → 失败回滚"模式 5 处重复 | 样板代码，易遗漏回滚 | 抽取 `save_with_dirty_rollback` 辅助函数或宏 | 中 |
| C-TD-010 | manager.rs:447-455 vs hot_reload.rs:202-211 | `catch_unwind(AssertUnwindSafe(...))` 回调模式两处重复 | 回调隔离策略分散 | 抽取 `invoke_callback_safe` 辅助函数 | 低 |
| C-TD-011 | manager.rs:1433-1471 vs hot_reload.rs:376-385 | `make_temp_config_manager` 测试辅助函数两文件各定义一份 | 状态可能不一致 | 提取到 `#[cfg(test)] pub(crate) mod test_support` | 低 |

### 3.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-012 | manager.rs:761 等（Relaxed）vs 885 等（SeqCst） | config 系列用 `Relaxed`、library 系列用 `SeqCst`，无设计理由 | 命名/语义对称操作 Ordering 不一致 | 统一为 `Relaxed` 或补充注释 | 中 |

### 3.5 未使用导入

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-013 | mod.rs:40 | `detect_wallpaper_type_by_content` 未使用的重导出 | 暴露死代码 API | 移除重导出 | 低 |

### 3.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-014 | thumbnail.rs:34-62 | `TmpFrameGuard<'a>` RAII 守卫含 `armed` 字段与 `disarm` 方法（前提是 C-TD-003 移除 disarm） | 类型+字段+方法+Drop 约 30 行 | 简化为 `struct TmpFrameGuard<'a>(&'a Path);` + Drop 直接 `remove_file` | 中 |
| C-TD-015 | manager.rs:250,325,47,362,390-392 | `watcher_alive` 整套机制被写入无外部读取 | 实际效果为零 | 保留降级或删除整套机制 | 中 |

### 3.7 修复痕迹

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-016 | manager.rs:1102-1142 | `normalize_path` 注释 40 行引用多个跨模块历史 spec（C09/W13/ST-007） | 文档膨胀 | 精简为当前行为描述 | 低 |
| C-TD-017 | manager.rs:1144-1205 | `atomic_write` 函数体含 6 处历史修复注释 | 可读性下降 | 合并进函数级文档"历史"段 | 低 |
| C-TD-018 | manager.rs:430-463 | `notify_config_error` 注释引用 C01/C-003/C-019 | 跨 spec 引用负担 | 改写为当前行为描述，移除 C-019 引用 | 低 |
| C-TD-019 | manager.rs:638-751 | `cleanup_corrupted_thumbnails` 单一函数承担零字节清理 + 孤儿 lock 两个职责 | 单函数两职责，113 行 | 拆分为 `cleanup_zero_byte_thumbnails` + `cleanup_orphan_lock_files` | 中 |
| C-TD-020 | manager.rs:939-996 vs 1006-1064 | `load_config`/`load_library` 注释段几乎重复 | 重复注释 | 与 C-TD-008 一并合并 | 中 |
| C-TD-021 | manager.rs:382-392 | `is_watcher_alive` 注释描述未实际接入的功能链 | 误导开发者 | 与 C-TD-005/015 一并处理 | 低 |
| C-TD-022 | detect.rs:63 等 | `detect.rs` 七个历史标记（C-011/N-007/N-008/C-008/C-012/v41-C-005/v41-C-015） | 历史对比噪音 | 改为当前行为描述 | 低 |
| C-TD-023 | thumbnail.rs:158-190 | `tmp_frame_name_from_path` 注释两层历史修复描述（C-007 + v41-C-006），33 行 | 注释膨胀 | 精简为当前格式 + 唯一性保证 | 低 |
| C-TD-024 | settings.rs:252-256, 321-330 | `GifConfig` 常量注释引用已不存在的 `gif_decode.rs` | 跨模块历史引用 | 移除 `gif_decode.rs` 引用 | 低 |

### 3.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-025 | manager.rs:1133 vs wallpaper 模块 | `normalize_path` 与 `normalize_path_for_compare` 行为相同命名不同 | 跨模块概念不同名 | 统一命名 | 中 |
| C-TD-026 | manager.rs:758 vs 918 | `flush`（无后缀）vs `flush_library`（有后缀）命名不对称 | 对应关系不直观 | 推荐统一加后缀但需评估迁移成本 | 中 |
| C-TD-027 | manager.rs:861 | `save_library_locked` `_locked` 后缀暗示存在非 locked 版本，config 无对应 | 命名不对称 | 可接受现状，补充注释说明 | 低 |

### 3.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-028 | mod.rs:20-22 | TODO 风格注释长期"待定"，`AppConfig` 重导出状态模糊 | 导出约定不明确 | 决策是否重导出并更新注释 | 低 |
| C-TD-029 | manager.rs:221, 232 | `last_internal_save`/`periodic_save_shutdown_tx`/`periodic_save_running` 字段注释引用 C-093/C-110/C-016 外部 spec ID | 跨 spec 引用负担 | 评估是否建立索引或移除 | 低 |
| C-TD-030 | hot_reload.rs:1-13 | 模块文档未提及 `sync_all` 环节 | 文档与实现不符 | 补充 `sync_all` fsync | 低 |
| C-TD-031 | detect.rs:1-7 | 模块文档暗示 `detect_wallpaper_type_by_content` 接入命令路径 | 文档误导 | 与 C-TD-004 联动更新 | 低 |
| C-TD-032 | thumbnail.rs:1-9 | 模块文档仍描述"抽取首帧后再生成"（v5.0 已跳过 re-encode） | 文档过时 | 更新反映 v5.0 直接 rename | 低 |

### 3.10 清理状态汇总（v6.0）

> 清理 spec：`cleanup-v6-config-tech-debt-2026-07-25`，状态截至 2026-07-25。

- **P0（14 项）全部已落实**：C-TD-001、C-TD-002、C-TD-007、C-TD-013、C-TD-016、C-TD-017、C-TD-018、C-TD-022、C-TD-023、C-TD-024、C-TD-028（决策保留现状）、C-TD-030、C-TD-031、C-TD-032。
- **P1（7 项）全部已落实**：C-TD-003、C-TD-004（决策保留函数）、C-TD-005、C-TD-006、C-TD-014、C-TD-015、C-TD-021。
- **P2（9 项）全部已落实**：C-TD-008、C-TD-009、C-TD-010、C-TD-011、C-TD-012、C-TD-019、C-TD-020、C-TD-025（决策保留命名）、C-TD-026（决策保留命名）。
- **P3（2 项）全部已评估**：C-TD-027（补充注释）、C-TD-029（决策不建索引）。

| 状态 | 数量 | 占比 | 说明 |
|---|---|---|---|
| 已修复 | 27 | 84% | 直接修改源码落实清理建议 |
| 已决策保留现状 | 5 | 16% | C-TD-004、C-TD-025、C-TD-026、C-TD-028、C-TD-029 |
| 未处理 | 0 | 0% | — |
| **合计** | **32** | **100%** | config 模块技术债清理完成 |

## 4. 与 v5.0 文档的关联（已覆盖项）

> 来源：`.trae/specs/deep-performance-optimization-v5-2026-07-23/findings-config.md`

- **C-PERF-001**: `is_ffmpeg_available` 未缓存 → ✅ 已修复（`OnceLock`，thumbnail.rs:334-354）
- **C-PERF-002**: `save_library` 无防抖 → ✅ 已修复（`library_dirty` + `flush_library` + `mark_library_dirty`）
- **C-PERF-003**: `update_thumbnail` O(n) 搜索 + `normalize_path` 分配 → ✅ 已修复（`normalized_path` 派生字段）
- **C-PERF-004**: 视频 re-encode → ✅ 已修复（直接 rename，thumbnail.rs:488-494）
- **C-PERF-005**: config 与 library 落盘互相阻塞 → ✅ 已修复（拆分 `config_save_mutex` + `library_save_mutex`）
- **C-PERF-007**: Triangle vs Lanczos3 → ✅ 已修复（`FilterType::Triangle`）
- **C-PERF-010**: 孤儿 `.lock` 检查 O(N×M) → ✅ 已修复（`HashSet`）

## 5. 优化机会（v6 第 4 节，非技术债类改进点）

- **可读性提升**：
  - `ConfigManager` 结构体含 17 个字段，多轮修复累积导致字段含义混杂（config/library/watcher/周期保存/回调/错误缓冲）。可考虑按职责分组为嵌套结构体（如 `config_state`/`library_state`/`watcher_state`）。
  - `manager.rs` 单文件 2533 行（含测试），非测试代码约 1200 行，建议拆分为 `manager.rs`（核心 CRUD + 持久化）、`atomic_write.rs`（原子写入）、`load.rs`（load_config/load_library）等子模块。
- **测试覆盖盲区**：
  - `reload_config_and_library` 成功路径的"窗口期"不一致状态未被测试覆盖，仅测试了失败回滚路径（C11 对称测试）。
  - `start_periodic_save` 周期线程的实际 30s 触发行为未被测试覆盖（仅测试 spawn 失败清理 C10）。
  - `atomic_write` 锁文件复用行为未被测试覆盖。
- **文档完善建议**：
  - `settings.rs` 的 `validate` 校验策略文档未说明"为何不返回 Result"的设计权衡。
  - `ConfigManager` 的并发安全设计（哪些字段用 RwLock/Mutex/AtomicBool）缺少整体性文档。

## 6. 交集汇总（v4 findings × v6 技术债）

| 交集项 | v4 finding | v6 技术债 | 关系说明 |
|---|---|---|---|
| dirty 落盘逻辑 | C-001（dirty 竞态，已修复） | C-TD-009（dirty swap+回滚 5 处重复） | v4 修复 C-001 后，`swap` 模式在多处落盘路径复制，催生了 C-TD-009（v6 已抽取 `save_with_dirty_rollback` 统一） |
| 临时帧文件名 | C-007（并发竞争，已修复） | C-TD-023 / C-TD-003 / C-TD-014（`tmp_frame_name_from_path` 注释 + `TmpFrameGuard`） | v4 修复 C-007 引入唯一文件名与 `TmpFrameGuard`，后者成为 v6 死代码/过度设计（已简化） |
| 回调隔离 | C-003（catch_unwind，已修复） | C-TD-010（catch_unwind 两处重复） | v4 修复 C-003 后模式复制，催生 C-TD-010（v6 已抽取 `invoke_callback_safe`） |
| ffmpeg 探测缓存 | C-014（错误分级，已修复） | C-PERF-001（缓存） | 均落在 `is_ffmpeg_available`，v4 逻辑修复 + v5 性能缓存叠加 |
| GIF 魔数检测 | C-011（6 字节校验，已修复） | C-TD-022（detect 历史注释） | v4 修复后残留历史注释成为 v6 C-TD-022 清理目标（已清理） |
| `.lock` 文件 | C-018（孤儿清理，已修复） | C-TD-019（cleanup 双职责） | v4 修复新增孤儿 lock 清理，与此前 0 字节清理合并于 `cleanup_corrupted_thumbnails`，变成 v6 C-TD-019 的拆分对象（已拆分） |
| dirty 标志 Ordering | C-001（swap 原子） | C-TD-012（Relaxed/SeqCst 不一致） | v6 统一为 `Relaxed`，使 C-001 的 swap 实现与 library 系列保持一致 |

**结论**：v4 的 18 项 findings 全部确认 ✅ 已修复；v6 的 32 项技术债（27 修复 + 5 决策保留）全部落实，其中 7 项与 v4 修复存在承接关系——v6 清理的主要对象正是 v4 修复遗留的"修复痕迹/重复实现"类新债。两条审查线在代码层面无未决重叠。