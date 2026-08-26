# v6.0 技术债审查 - config 模块

← [返回索引](./00-总览与路线图.md)

> 审查日期：2026-07-25 | 模块路径：`crates/mirrorstar-core/src/config/`

## 1. 当前状态摘要

### 1.1 模块职责

config 模块是 mirrorstar-core 的配置与壁纸库管理中枢，聚合五类职责：① 应用配置 `AppConfig` 的结构定义与范围校验（`settings.rs`）；② 配置与壁纸库的加载、持久化、原子写入与增删改查（`manager.rs` 中的 `ConfigManager`）；③ 文件监视与热重载（`hot_reload.rs`，通过 `impl ConfigManager` 扩展）；④ 壁纸类型检测（`detect.rs`，扩展名 + 魔数双路径）；⑤ 缩略图生成（`thumbnail.rs`，image crate + ffmpeg CLI）。模块通过 `Arc<RwLock<...>>` + `AtomicBool` + `Mutex` 提供线程安全的并发访问，被 Tauri 命令层以 `Arc<ConfigManager>` 形式共享。该模块历经 v3.x→v4.0→v5.0 三轮深度修复（C01-C11、v41-C-XXX、C-PERF-XXX 等数十个 spec），代码中保留了大量的修复痕迹注释。

### 1.2 文件清单

| 文件 | 行数 | 主要内容 |
|---|---|---|
| mod.rs | 45 | 模块导出与 `pub use` 重导出 |
| manager.rs | 2863 | `ConfigManager` 实现、`WallpaperEntry`/`WallpaperLibrary`/`DisplayInfo` 数据模型、`atomic_write`、`normalize_path`、配置加载错误类型、`#[cfg(test)] mod tests`（约 1650 行测试） |
| settings.rs | 808 | `AppConfig` 及子配置（`AudioConfig`/`VideoConfig`/`GifConfig`/`DisplayConfig`/`WebConfig`/`PauseConfig`/`GeneralConfig`）与 `validate()` 范围校验 |
| hot_reload.rs | 676 | `start_watching`/`reload_config_and_library`/`start_periodic_save`/`shutdown_periodic_save`/`stop_watching` |
| detect.rs | 849 | `detect_wallpaper_type`（扩展名）、`detect_wallpaper_type_by_content`（魔数）、`detect_html` |
| thumbnail.rs | 1297 | `generate_thumbnail`/`generate_video_thumbnail`/`is_ffmpeg_available`/`TmpFrameGuard` RAII 守卫 |

### 1.3 测试覆盖

测试代码占模块总行数约 50%（约 3300 行 / 6400 行），覆盖度高且与修复一一对应：
- `manager.rs` 含约 40 个 `#[test]`，覆盖序列化往返、`atomic_write`、`ConfigManager` 增删改查、`load_config`/`load_library` 损坏回退、dirty 标志竞态（C-001）、配置加载错误通知（C01）、validate clamp（C02/C-009）、cleanup 路径推导（C03/v41-C-003）、TOCTOU（C08）、路径规范化（C09）、reload 原子性（C11）、sync_all（v41-C-002）等。
- `hot_reload.rs` 含 6 个 `#[test]`，覆盖 spawn 失败清理（C10）、reload 原子性对称测试（C11）、注释一致性（C-004）、watcher_alive 标志（v41-C-004）。
- `detect.rs` 含约 30 个 `#[test]`，覆盖扩展名/魔数/HTML/BOM 检测（N-007/N-008/v41-C-015）。
- `thumbnail.rs` 含约 20 个 `#[test]`，覆盖缩略图生成、解压炸弹防护（C-002/C06）、文件名稳定性（C-006）、临时文件名唯一性（C-007/v41-C-006）、`TmpFrameGuard`（v41-C-007）。
- `settings.rs` 含约 15 个 `#[test]`，覆盖默认值、TOML 往返、validate clamp（C02/C-005/v41-W-012）。

测试盲区见第 4 节。

## 2. 技术债清单

### 2.1 死代码

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-001 | manager.rs:147-150 | `WallpaperLibrary::new()` 仅返回 `Self::default()`，是 `Default::default()` 的薄 wrapper。Grep 验证：在 `crates/` 与 `src-tauri/` 中仅本文件测试 `wallpaper_library_new_is_empty` 调用，无生产调用点 | 维护负担：新增字段需同步修改 `new()` 与 `Default`，易遗漏 | 删除 `new()` 方法，测试改用 `WallpaperLibrary::default()` | 低 |
| C-TD-002 | manager.rs:898-911 | `maybe_save_library` 已标注 `#[allow(dead_code)]`，文档注释承认"当前周期保存线程因无 `&self` 直接内联了等价逻辑...本方法作为 `pub(crate)` API 保留，供未来批量场景或外部模块调用"。v5.0 已引入 `batch_update_thumbnails`/`flush_library` 覆盖批量与显式落盘场景，"未来"已到但未使用此方法 | 误导后续开发者以为存在调用方；`#[allow(dead_code)]` 掩盖了真实死代码 | 删除 `maybe_save_library`，更新注释引用 | 低 |
| C-TD-003 | thumbnail.rs:49-52 | `TmpFrameGuard::disarm` 已标注 `#[allow(dead_code)]`，注释承认"当前 `generate_video_thumbnail` 始终在函数退出时清理临时帧文件...本方法未被使用，保留供未来需要保留临时文件的场景使用" | 同上，`#[allow(dead_code)]` 掩盖死代码；`disarm` 测试 `v41_c007_tmp_frame_guard_disarm_preserves_file` 仅验证死代码自身行为 | 删除 `disarm` 方法及对应测试，`armed` 字段改为直接 `drop` 时清理（无 disarm 路径则 `armed` 字段可省略，直接在 `drop` 中清理） | 中 |
| C-TD-004 | detect.rs:70-107 + mod.rs:40 | `detect_wallpaper_type_by_content`（pub fn，117 行实现 + 40 行文档）在 `crates/` 与 `src-tauri/` 中均无调用方，仅在 `mod.rs:40` 被 `pub use` 重导出。模块文档（detect.rs:6）声称其"被壁纸库管理命令 add_wallpaper 用于在添加壁纸前判断类型"，但实际命令路径只用 `detect_wallpaper_type`。v3.4 引入的"内容嗅探"能力从未接入命令层 | 117 行代码 + 40 行文档 + ~25 个测试（detect.rs 中 `detect_wallpaper_type_by_content` 相关测试）维护成本；误导开发者以为存在内容嗅探调用链 | 评估是否接入命令层（如 `add_wallpaper` 用魔数纠错扩展名篡改场景），若不接入则删除函数、重导出与相关测试 | 中 |
| C-TD-005 | manager.rs:390-392 | `is_watcher_alive`（pub fn）Grep 验证：在 `src-tauri/` 中无调用方，仅在 `crates/mirrorstar-core` 内部的 `manager.rs`/`hot_reload.rs` 测试中调用。文档注释声称"调用方通过 `is_watcher_alive` 查询热重载状态，决定是否需要重试启动或提示用户热重载已不可用"，但无实际调用方 | pub API 暴露但无消费者，破坏 API 表面积最小化原则；与 C-TD-015（watcher_alive 字段机制过度设计）关联 | 若不计划接入 Tauri 层热重载状态查询，则降级为 `pub(crate)` 或删除；同时清理 `watcher_alive` 字段（见 C-TD-015） | 中 |
| C-TD-006 | settings.rs:234-239 | `WebConfig` 结构体及其 `cache_path` 字段 Grep 验证：在 `crates/` 与 `src-tauri/` 中均无使用（无 `web.cache_path`、`web: WebConfig` 模式匹配）。`AppConfig::web` 字段虽参与序列化但前端从未读取 `cache_path` | 配置文件 `config.toml` 中 `[web]` 表空字段长期存在但无意义；新增 Web 相关配置时可能误用此结构 | 删除 `WebConfig` 与 `AppConfig::web` 字段，或补充实际接入逻辑（如 WebView2 缓存路径设置） | 中 |

### 2.2 冗余抽象

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-007 | thumbnail.rs:526-538 | `guess_image_format` 标注为 `pub fn`，但注释明确说明"公开此辅助函数以便测试覆盖格式映射逻辑"。实际仅被 `generate_thumbnail_from_image_file`（thumbnail.rs:215）内部调用一次，无外部消费者 | 仅为测试而 `pub`，违反 API 表面积最小化；与 detect.rs 内部的扩展名匹配逻辑（`VIDEO_EXTS`/`IMAGE_EXTS`/`WEB_EXTS`）存在概念重复但实现独立 | 改为 `pub(crate)` 或 `#[cfg(test)] pub`，测试通过 `#[cfg(test)] mod tests` 内 `use super::*` 访问 | 低 |

### 2.3 重复实现

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-008 | manager.rs:939-996 vs 1006-1064 | `load_config` 与 `load_library` 的"File::open → take(MAX+1) → read_to_end → 大小检查 → String::from_utf8 → match toml::from_str"逻辑高度重复（约 30 行几乎逐行对应），仅反序列化类型与错误消息不同。两函数的注释段（"SEC-003 / C08 修复：消除 TOCTOU 窗口..."）也几乎完全重复 | 修改读取逻辑需同步两处（如调整大小上限策略、改用 mmap 等）；注释重复增加维护成本 | 抽取 `fn read_bounded_utf8_file(path: &Path, max_size: u64) -> Result<String, MirrorStarError>` 辅助函数，两函数仅保留反序列化 + `ConfigLoadError` 构造 | 中 |
| C-TD-009 | manager.rs:758-783, 788-840, 899-911, 918-928; hot_reload.rs:260-300 | "dirty.swap(false) → 读取状态 → save → 失败时 store(true) 回滚"模式在 `flush`、`maybe_save_config`、`maybe_save_library`、`flush_library`、`start_periodic_save`（config 与 library 两段）共 5 处重复出现，注释多次标注"与 flush 一致，C-101"。每处都包含 `unwrap_or_else(|e| e.into_inner())` 锁错误处理样板 | 5 处重复约 50 行样板代码；C-101 回滚语义需在每处重新实现，易遗漏（如新增落盘路径忘记回滚 dirty） | 抽取 `fn save_with_dirty_rollback(dirty: &AtomicBool, save_fn: impl FnOnce() -> Result<(), MirrorStarError>) -> Result<(), MirrorStarError>` 或宏 `save_with_rollback!(self.dirty, self.save_config())` | 中 |
| C-TD-010 | manager.rs:447-455 vs hot_reload.rs:202-211 | `catch_unwind(AssertUnwindSafe(|| callback(...)))` 包裹用户回调的模式在 `notify_config_error`（manager.rs:447-455）与 `reload_config_and_library` 的 `on_config_changed` 调用（hot_reload.rs:202-211）重复，两处注释都提到"避免回调 panic 导致 watcher 线程终止"，且都使用 `tracing::error!` 记录 panic payload | 回调隔离策略分散在两处，若需调整（如改为记录更多信息、限制回调执行时间）需同步修改 | 抽取 `fn invoke_callback_safe<F: FnOnce()>(callback: F, context: &str)` 辅助函数，统一 catch_unwind + 错误日志 | 低 |
| C-TD-011 | manager.rs:1433-1471 vs hot_reload.rs:376-385 | `make_temp_config_manager` 测试辅助函数在 `manager.rs` 与 `hot_reload.rs` 各定义一份，前者直接构造结构体（17 个字段），后者调用 `ConfigManager::new_in_dir`。两份辅助函数目的相同（避免污染用户数据目录）但实现路径不同 | 测试辅助函数重复；`manager.rs` 版本直接构造结构体绕过 `new_in_dir` 的 `load_config`/`load_library` 路径，与 `hot_reload.rs` 版本构造的实例状态可能不一致（如 `config_path` 是否指向真实文件） | 提取到 `#[cfg(test)] pub(crate) mod test_support` 模块统一实现，两文件 `use` 引用 | 低 |

### 2.4 过时模式

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-012 | manager.rs:761, 802, 816, 828（`Ordering::Relaxed`）vs 885, 901, 907, 919, 924（`Ordering::SeqCst`） | config 系列 dirty 标志操作（`flush`/`maybe_save_config`/`update_config`）使用 `Ordering::Relaxed`，而 library 系列（`mark_library_dirty`/`maybe_save_library`/`flush_library`）使用 `Ordering::SeqCst`。同类 dirty 标志操作使用不同 Ordering，无明确设计理由。`is_watcher_alive`（manager.rs:391）也用 `SeqCst`，注释解释"保证所有线程观察到的状态一致"，但 library dirty 用 `SeqCst` 无对应注释 | 命名/语义对称的操作 Ordering 不一致，开发者无法判断何时该用哪个；可能存在隐藏的内存顺序 bug（或过度保守） | 统一 dirty 标志操作的 Ordering（推荐 `Relaxed`，因 dirty 标志本身只关心原子性而非顺序性，`swap`/`store` 已保证原子读写）；或补充注释说明 library 系列用 `SeqCst` 的原因 | 中 |

### 2.5 未使用导入

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-013 | mod.rs:40 | `pub use detect::{detect_wallpaper_type, detect_wallpaper_type_by_content};` 中 `detect_wallpaper_type_by_content` 是未使用的重导出（与 C-TD-004 关联，但此处从 import 角度）。Grep 验证：在 `crates/` 与 `src-tauri/` 中均无 `mirrorstar_core::config::detect_wallpaper_type_by_content` 调用路径 | 重导出暴露了死代码 API，外部调用方可能误以为这是稳定 API 而依赖它 | 与 C-TD-004 一并处理：若保留函数则降级为 `pub(crate)`，重导出改为 `pub use detect::detect_wallpaper_type;`；若删除函数则同步移除重导出 | 低 |

### 2.6 过度设计

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-014 | thumbnail.rs:34-62 | `TmpFrameGuard<'a>` RAII 守卫类型含 `armed: bool` 字段与 `disarm` 方法，但 `disarm` 已标注 `#[allow(dead_code)]`（C-TD-003）。整个类型仅为清理一个临时文件而设计，且 `generate_video_thumbnail` 始终在函数退出时清理（无 disarm 调用路径）。原 v41-C-007 修复引入此类型是为了替代"分散在多处的手动清理"，但当前手动清理已不存在（仅 `rename` 失败 fallback 路径有显式 `remove_file`，与守卫职责重叠） | 类型 + 字段 + 方法 + Drop impl 共约 30 行，仅为管理一个临时文件的删除；`armed` 字段在无 `disarm` 调用下恒为 `true`，可省略 | 简化为 `struct TmpFrameGuard<'a>(&'a Path);` + `Drop` 直接 `remove_file`，删除 `armed` 字段与 `disarm` 方法（与 C-TD-003 一并处理）；或改用 `scopeguard` crate 的 `defer!` 宏 | 中 |
| C-TD-015 | manager.rs:250（字段定义）, 325（构造初始化）, 47（start_watching 置 true）, 362（stop_watching 置 false）, 390-392（is_watcher_alive 读取） | `watcher_alive: Arc<AtomicBool>` 字段 + 在 `start_watching`/`stop_watching`/`Drop` 中维护 + `is_watcher_alive` pub 查询 API，整套机制为 v41-C-004 修复引入，目的是"让调用方感知热重载是否失效"。但 `is_watcher_alive` 在 `src-tauri/` 中无调用方（C-TD-005），整套机制的字段被写入但无外部读取，实际效果为零 | 4 处 `store`/`load` 维护一个无人读取的标志；pub API 暴露但无消费者；修复引入的代码从未产生预期价值 | 评估 v41-C-004 修复目标是否仍有效：若 Tauri 层未来需要查询热重载状态，则保留字段降级 `is_watcher_alive` 为 `pub(crate)`；若不再需要，则删除整个机制（字段 + 4 处维护点 + 查询 API + 2 个测试） | 中 |

### 2.7 修复痕迹

> 本节重点列出引用 v3/v4/v5 历史 spec 的注释。config 模块历经 v3.x（C01-C11、N-005~N-008）、v4.0（C-001~C-018、v41-C-001~v41-C-007、v41-W-012）、v5.0（C-PERF-001~C-PERF-010）多轮修复，代码中保留了大量历史标记，部分注释更像 changelog 而非设计文档。

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-016 | manager.rs:1102-1142 | `normalize_path` 的文档注释长达 40 行，引用 "C09 修复"、"W13"（wallpaper 模块的 spec）、"C-015"、"ST-007 Wave 2C" 等多个跨模块历史 spec。注释中"ST-007 Wave 2C 策略"引用了一个外部修复计划，但当前代码已整合完毕，注释更像 changelog | 函数文档膨胀，新读者难以快速理解当前行为；跨模块 spec 引用（W13/ST-007）增加上下文负担 | 精简为：函数行为（lowercase + 分隔符统一）+ 跨平台说明 + 安全策略（不调用 canonicalize）三段，移除 C09/W13/ST-007 等历史标记 | 低 |
| C-TD-017 | manager.rs:1144-1205 | `atomic_write` 函数体内含 "C07 修复"（rename 失败清理）、"v41-C-001 修复"（write 失败清理 .tmp）、"v41-C-002 修复"（sync_all 替代 std::fs::write）三处历史修复注释，外加函数级文档引用 "C-055"、"C-101"、"N-006"。6 处历史标记使函数可读性下降 | 函数体被历史修复注释分割为多段，新读者难以把握整体逻辑 | 将历史修复说明合并到函数级文档的"历史"段，函数体内仅保留当前行为的简洁注释 | 低 |
| C-TD-018 | manager.rs:430-463 | `notify_config_error` 注释引用 "C01 修复"、"C-003 修复"、"C-019"，且注释中"参考 `on_config_changed` 的克隆-释放锁-调用模式，避免回调内重入取写锁导致死锁"引用了已修复的 C-019，但 C-019 的上下文对新读者不透明（需查阅 v4.0 文档才能理解） | 跨 spec 引用增加理解成本；C-019 已修复但注释仍以"避免...导致死锁"表述，暗示问题仍存在 | 改为描述当前行为："先克隆 Arc<callback> 再释放读锁，回调内可重入 set_on_config_error 而不会同线程死锁"；移除 C-019 引用 | 低 |
| C-TD-019 | manager.rs:638-751 | `cleanup_corrupted_thumbnails` 函数长达 113 行，混合两个不同职责：① 0 字节损坏缩略图清理（manager.rs:649-681）；② 孤儿 `.lock` 文件清理（manager.rs:694-748）。注释含 "fix-wallpaper-preview-blank Task 5"、"C03 修复"、"C-018"、"v41-C-003 修复"、"v5.0 C-PERF-010" 五个历史标记。函数名仅反映职责①，职责②未体现在函数名中 | 单函数承担两个职责，修改一个职责可能影响另一个；函数名与实际职责不符；113 行长度超出单屏幕 | 拆分为 `cleanup_zero_byte_thumbnails` 与 `cleanup_orphan_lock_files` 两个函数，`cleanup_corrupted_thumbnails` 作为聚合入口调用两者；或重命名为 `cleanup_data_dir_residuals` | 中 |
| C-TD-020 | manager.rs:939-996 vs 1006-1064 | `load_config` 与 `load_library` 的注释段（"SEC-003 / C08 修复：消除 TOCTOU 窗口..."）几乎完全重复，两段各约 12 行，描述同一修复策略。`load_library` 注释中"C-010：MAX_CONFIG_SIZE 提取为模块级常量 MAX_CONFIG_FILE_SIZE，load_config / load_library 共用"重复了 `load_config` 已说过的内容 | 重复注释增加维护成本；修改 TOCTOU 策略需同步两处 | 与 C-TD-008 一并处理：抽取公共读取函数后，注释也合并到公共函数 | 中 |
| C-TD-021 | manager.rs:382-392 | `is_watcher_alive` 注释引用 "v41-C-004 修复"，但该 pub API 实际无外部调用方（C-TD-005）。注释描述了完整的"start_watching 成功后置 true，stop_watching/Drop 置 false，调用方查询决定是否重试"流程，但"调用方查询"环节从未发生 | 修复注释描述了一个未实际接入的功能链，误导开发者以为存在调用方 | 与 C-TD-005/C-TD-015 一并处理：删除 API 或降级可见性时同步精简注释 | 低 |
| C-TD-022 | detect.rs:63, 68, 69, 86, 115, 142, 155, 178-186 | `detect.rs` 中 `detect_wallpaper_type_by_content`/`detect_wallpaper_type_by_magic_bytes`/`detect_html` 的注释含 "C-011 修复"、"N-007 修复"、"N-008 修复"、"C-008"、"C-012 修复"、"v41-C-005 修复"、"v41-C-015" 七个历史标记，部分注释（如 detect.rs:115-117 "C-011 修复：校验完整 6 字节魔数，原仅校验前 3 字节"）描述的是"原实现错误 + 修复后正确"的对比，对新读者是噪音 | 注释中"原仅校验前 3 字节"等历史对比对新读者无价值，且暗示当前实现可能仍有问题 | 将历史对比改为当前行为描述：如"GIF 魔数为完整 6 字节 `GIF87a`/`GIF89a`"，移除"原仅校验前 3 字节" | 低 |
| C-TD-023 | thumbnail.rs:158-190 | `tmp_frame_name_from_path` 注释含 "C-007 修复"与"v41-C-006 修复"两层历史：先描述 C-007 Finding（"修复前临时帧文件名仅基于源路径哈希，同一视频并发调用会使用相同路径"），再描述 v41-C-006 修复（"在某些 Windows 系统上分辨率仅 100ns...追加进程内全局 AtomicU64 计数器"）。当前实现已是最终状态（counter + nanos），历史描述可精简 | 两层历史修复描述使注释膨胀至 33 行，函数实际逻辑仅 12 行 | 精简为：文件名格式 + 唯一性保证（counter 单调递增 + nanos 跨进程区分）两段，移除 C-007/v41-C-006 历史对比 | 低 |
| C-TD-024 | settings.rs:252-256, 321-330 | `GifConfig.max_memory_mb` 字段注释 "v41-W-012: 原为 gif_decode.rs 中硬编码常量 MAX_GIF_MEMORY_MB = 40，现提取为配置项"，引用了已不在本模块的历史代码位置（gif_decode.rs）。`DEFAULT_GIF_MEMORY_MB`/`MIN_GIF_MEMORY_MB`/`MAX_GIF_MEMORY_MB_LIMIT` 三个常量注释也都以 "v41-W-012" 开头 | 跨模块历史引用（gif_decode.rs）对理解当前配置项无帮助；读者需查找 gif_decode.rs 才能理解"原硬编码常量"的上下文 | 移除"原为 gif_decode.rs 中硬编码常量"引用，保留当前配置项的语义说明 | 低 |

### 2.8 命名一致性

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-025 | manager.rs:1133（`normalize_path`）vs wallpaper 模块（`normalize_path_for_compare`） | config 模块的路径规范化函数命名为 `normalize_path`（manager.rs:1133），wallpaper 模块的对应函数命名为 `normalize_path_for_compare`（注释 manager.rs:1107 提到"规范化方向与 `wallpaper::manager::normalize_path_for_compare` (W13) 保持一致"）。两个函数行为相同（lowercase + 分隔符统一），但命名风格不一致：一个强调"规范化"，一个强调"用于比较" | 跨模块同名概念不同名，开发者需记忆两个函数的等价性；调用方可能误以为两者行为不同 | 统一命名为 `normalize_path_for_compare`（更准确描述用途）或都改为 `normalize_path`；建议 config 模块改为 `normalize_path_for_compare` 以匹配用途语义 | 中 |
| C-TD-026 | manager.rs:758（`flush`）vs 918（`flush_library`） | config 落盘方法命名为 `flush`（无后缀），library 落盘方法命名为 `flush_library`（有 `_library` 后缀）。两者行为对称（都是 check dirty + save + 失败回滚），但命名风格不对称。同理 `maybe_save_config`（无后缀）vs `maybe_save_library`（有后缀）也存在不对称 | 命名不对称使调用方难以直觉判断 config 与 library 方法的对应关系；可能误以为 `flush` 是通用方法 | 二选一：① 都加后缀（`flush_config`/`flush_library`，破坏现有 `flush` 调用方）；② 都不加后缀（`flush`/`flush_lib`，语义不清）。推荐方案①但需评估调用方迁移成本 | 中 |
| C-TD-027 | manager.rs:861（`save_library_locked`） | `save_library_locked` 的 `_locked` 后缀暗示存在对应的非 locked 版本（即 `save_library`，manager.rs:849）。实际两者关系是：`save_library` 获取锁后调用 `save_library_locked`（注释 manager.rs:857 说明"假设调用方已持有 `library_save_mutex`"）。命名暗示了"已持锁"的契约，但缺少对应的 `save_config_locked`（config 路径不需要 locked 版本，因 `maybe_save_config`/`flush` 直接持锁）。命名上 config 与 library 的内部辅助函数不对称 | library 有 `_locked` 内部版本而 config 无，开发者可能误以为 config 也需要类似抽象但缺失 | 可接受现状（`save_library_locked` 命名准确描述契约），或补充注释说明 config 路径为何不需要 `_locked` 版本 | 低 |

### 2.9 注释陈旧

| ID | 位置 | 描述 | 影响 | 清理建议 | 复杂度 |
|---|---|---|---|---|---|
| C-TD-028 | mod.rs:20-22 | 模块文档注释"注意：`AppConfig` 及其子配置（`AudioConfig` / `VideoConfig` / `GifConfig` 等）目前仅通过 `pub mod settings` 暴露，需通过 `mirrorstar_core::config::settings::AppConfig` 引用；如需添加为顶层重导出，应在下方 `pub use` 段补充 `pub use settings::AppConfig;`"。这是 TODO 风格的注释，但"如需添加"暗示尚未决策，状态模糊：是否需要重导出未明确，且注释未说明为何不重导出 | 模块导出约定不明确；新增 `AppConfig` 重导出的决策被推迟，注释长期处于"待定"状态 | 决策是否重导出 `AppConfig`：若重导出则补充 `pub use settings::AppConfig;` 并更新注释；若保持 `pub mod settings` 暴露则删除 TODO 注释，明确说明设计理由 | 低 |
| C-TD-029 | manager.rs:221, 232 | `last_internal_save` 字段注释引用 "C-093"（"watcher 线程检测到 2s 窗口内的文件事件时跳过 reload，避免'内部保存 → 触发文件事件 → 重新加载'的循环（C-093）"）。C-093 是一个外部 spec ID，但代码中未说明其上下文，新读者无法追溯。同理 manager.rs:232 `periodic_save_shutdown_tx` 注释引用 "C-110"，manager.rs:225 `periodic_save_running` 注释引用 "C-016" | 跨 spec ID 引用增加上下文负担；新读者需查阅 v4.0 文档才能理解 C-093/C-110/C-016 的修复背景 | 评估这些 spec ID 是否仍被引用：若 v6.0 文档已归档这些修复，则移除 spec ID 引用，改为描述当前行为；或建立 spec ID 索引文档供查阅 | 低 |
| C-TD-030 | hot_reload.rs:1-13 | 模块文档注释"`ConfigManager`...写入采用临时文件 + rename 的原子写入方案（配合 fs2 文件锁串行化并发写入），避免写入中断导致配置文件损坏"。但实际 `atomic_write`（manager.rs:1180-1188）已改为 `File::create + write_all + sync_all + rename`（v41-C-002 修复），文档未提及 `sync_all` 这一关键数据持久化保证 | 模块文档与实现不符：开发者通过文档理解的原子写入方案缺少 `sync_all` 环节，可能误判异常退出时的数据持久性 | 更新模块文档："写入采用临时文件 + `sync_all` fsync + rename 的原子写入方案（配合 fs2 文件锁串行化并发写入）" | 低 |
| C-TD-031 | detect.rs:1-7 | 模块文档注释"`detect_wallpaper_type` 被壁纸库管理命令 `add_wallpaper` 用于在添加壁纸前判断类型；不支持的扩展名返回 `None`，由命令层转换为错误返回前端。"但未提及 `detect_wallpaper_type_by_content` 的调用方（实际无调用方，C-TD-004）。文档暗示两个函数都被命令层使用，但 `detect_wallpaper_type_by_content` 实际是死代码 | 模块文档误导开发者以为 `detect_wallpaper_type_by_content` 接入了命令路径 | 与 C-TD-004 一并处理：若删除函数则同步更新文档；若保留则说明其"为未来扩展预留，当前无调用方" | 低 |
| C-TD-032 | thumbnail.rs:1-9 | 模块文档注释"Video 类型：通过 ffmpeg CLI 抽取首帧后再生成缩略图（`generate_video_thumbnail`）"。但 v5.0 C-PERF-004 修复后，`generate_video_thumbnail`（thumbnail.rs:488-494）已改为直接 `rename` 临时帧文件为最终缩略图，跳过 image crate 的 decode + re-encode 周期。文档中"抽取首帧后再生成缩略图"的描述已过时 | 模块文档与实现不符：开发者通过文档理解的 Video 路径包含 image crate 解码步骤，实际已跳过 | 更新文档："Video 类型：通过 ffmpeg CLI 抽取首帧并直接作为缩略图（`generate_video_thumbnail`，v5.0 跳过 re-encode）" | 低 |

## 3. 清理建议汇总

### 3.1 立即清理（P0 高收益低风险）

- **C-TD-001**: 删除 `WallpaperLibrary::new()`，测试改用 `Default::default()`（1 行删除 + 1 处测试改写）
- **C-TD-002**: 删除 `maybe_save_library` 及 `#[allow(dead_code)]`（13 行删除）
- **C-TD-007**: `guess_image_format` 改为 `pub(crate)`（1 处可见性修改）
- **C-TD-013**: 移除 `mod.rs:40` 中 `detect_wallpaper_type_by_content` 重导出（与 C-TD-004 决策联动）
- **C-TD-016**: 精简 `normalize_path` 注释，移除 C09/W13/ST-007 历史标记（注释改写）
- **C-TD-017**: 合并 `atomic_write` 历史修复注释到函数级文档（注释改写）
- **C-TD-018**: 改写 `notify_config_error` 注释，移除 C-019 引用（注释改写）
- **C-TD-022**: 精简 `detect.rs` 历史修复标记，改为当前行为描述（注释改写）
- **C-TD-023**: 精简 `tmp_frame_name_from_path` 历史描述（注释改写）
- **C-TD-024**: 移除 `v41-W-012` 对 `gif_decode.rs` 的历史引用（注释改写）
- **C-TD-028**: 决策 `AppConfig` 重导出并更新 TODO 注释（注释改写 + 可能 1 行 `pub use`）
- **C-TD-030**: 更新 `hot_reload.rs` 模块文档补充 `sync_all`（注释改写）
- **C-TD-031**: 更新 `detect.rs` 模块文档（与 C-TD-004 联动）
- **C-TD-032**: 更新 `thumbnail.rs` 模块文档反映 v5.0 跳过 re-encode（注释改写）

### 3.2 谨慎清理（P1/P2 中收益）

- **C-TD-003**: 删除 `TmpFrameGuard::disarm` 及 `armed` 字段，简化为 `struct TmpFrameGuard<'a>(&'a Path);`（与 C-TD-014 合并处理）
- **C-TD-004**: 决策 `detect_wallpaper_type_by_content` 是否接入命令层，若不接入则删除函数 + 重导出 + ~25 个测试（117 + 40 + ~200 行删除）
- **C-TD-005 + C-TD-015 + C-TD-021**: 评估 `watcher_alive` 机制是否需要保留，若不需要则删除字段 + 4 处维护点 + `is_watcher_alive` API + 2 个测试（约 30 行删除）
- **C-TD-006**: 决策 `WebConfig` 是删除还是接入实际逻辑（涉及前端 `config.toml` 兼容性）
- **C-TD-008 + C-TD-020**: 抽取 `read_bounded_utf8_file` 辅助函数，合并 `load_config`/`load_library` 重复逻辑与注释
- **C-TD-009**: 抽取 dirty swap+回滚模式的辅助函数或宏（5 处统一）
- **C-TD-010**: 抽取 `invoke_callback_safe` 辅助函数统一 catch_unwind 模式
- **C-TD-011**: 提取 `make_temp_config_manager` 到 `test_support` 模块
- **C-TD-012**: 统一 dirty 标志 Ordering（需评估 SeqCst 是否必要）
- **C-TD-019**: 拆分 `cleanup_corrupted_thumbnails` 为两个职责函数
- **C-TD-025**: 统一 `normalize_path` 跨模块命名（需协调 wallpaper 模块）
- **C-TD-026**: 评估 `flush`/`flush_library` 命名对称性改造的调用方迁移成本

### 3.3 评估后决定（P3 长期或低收益）

- **C-TD-014**: `TmpFrameGuard` 整体简化（与 C-TD-003 合并，需评估是否引入 `scopeguard` crate）
- **C-TD-027**: `save_library_locked` 命名现状可接受，仅需补充注释说明 config 路径为何不需要 `_locked` 版本
- **C-TD-029**: spec ID 引用（C-093/C-110/C-016 等）是否建立索引文档供查阅，或统一移除

## 4. 优化机会（非技术债类改进点）

- **可读性提升**：
  - `ConfigManager` 结构体含 17 个字段（manager.rs:188-260），多轮修复累积导致字段含义混杂（config 状态、library 状态、watcher 状态、周期保存状态、回调、错误缓冲等）。可考虑按职责分组为嵌套结构体（如 `config_state`/`library_state`/`watcher_state`），提升可读性。
  - `manager.rs` 单文件 2863 行（含测试），非测试代码约 1200 行，建议拆分为 `manager.rs`（核心 CRUD + 持久化）、`atomic_write.rs`（原子写入）、`load.rs`（load_config/load_library）等子模块。
- **测试覆盖盲区**：
  - `reload_config_and_library` 成功路径的"窗口期"不一致状态（hot_reload.rs:158-165 两个独立 RwLock 依次替换）未被测试覆盖，仅测试了失败回滚路径（C11 对称测试）。建议补充测试验证窗口期内其他线程能观察到 `new_config + old_library` 状态。
  - `start_periodic_save` 周期保存线程的实际 30s 触发行为未被测试覆盖（仅测试了 spawn 失败清理 C10），建议用 mock 时间或缩短周期测试。
  - `atomic_write` 的锁文件复用行为（manager.rs:1159-1164 `OpenOptions::create(true).truncate(false)`）未被测试覆盖，建议补充测试验证多次写入复用同一 `.lock` 文件不累积。
- **文档完善建议**：
  - `settings.rs` 的 `validate` 校验策略文档（settings.rs:7-24）已较完善，但未说明"为何不返回 Result"的设计权衡（仅在注释中提及"调用方透明"）。建议补充独立的设计决策记录。
  - `ConfigManager` 的并发安全设计（哪些字段用 RwLock、哪些用 Mutex、哪些用 AtomicBool）缺少整体性文档，开发者需逐字段阅读注释才能理解并发模型。

## 5. 与 v4.0/v5.0 文档的关联

### 5.1 v4.0 已覆盖项（不再列入 v6）

> 来源：`docs/优化文档/02-config模块.md`（v4.0 审查发现 18 项）

- **C-001**: `maybe_save_config` dirty 标志竞态 → 已修复（改用 `swap` 原子 check-and-clear，manager.rs:816）
- **C-002**: C06 解压炸弹防护不完整 → 已修复（`ImageReader::limits`，thumbnail.rs:242-248）
- **C-003**: `notify_config_error` 未用 `catch_unwind` 包裹 → 已修复（manager.rs:447-455）
- **C-004**: C11 原子 reload 非原子（窗口期）→ 已修复（注释修正，hot_reload.rs:121-141）
- **C-005**: `balanced_keep_frames` 缺上限校验 → 已修复（`MAX_BALANCED_KEEP_FRAMES = 1000`，settings.rs:273）
- **C-006**: `DefaultHasher` 跨版本不稳定 → 已修复（hex 编码，thumbnail.rs:133-156）
- **C-007**: 临时帧文件名竞争 → 已修复（纳秒时间戳 + counter，thumbnail.rs:178-190）
- **C-008**: `f.read().unwrap_or(0)` 静默吞错 → 已修复（detect.rs:73-97）
- **C-009**: `update_config` 不调用 `validate` → 已修复（manager.rs:362）
- **C-010**: `MAX_CONFIG_SIZE` 重复定义 → 已修复（`MAX_CONFIG_FILE_SIZE` 模块级常量，manager.rs:38）
- **C-011**: GIF 魔数仅检查 3 字节 → 已修复（完整 6 字节，detect.rs:117）
- **C-012~C-018**: 其他 v4.0 修复（HTML 检测优化、`.lock` 文件清理、`sync_all`、watcher_alive 标志等）均已修复

### 5.2 v5.0 已覆盖项

> 来源：`.trae/specs/deep-performance-optimization-v5-2026-07-23/findings-config.md`

- **C-PERF-001**: `is_ffmpeg_available` 未缓存 → 已修复（`OnceLock`，thumbnail.rs:305-356）
- **C-PERF-002**: `save_library` 无防抖 → 已修复（`library_dirty` + `flush_library` + `mark_library_dirty`，manager.rs:259, 884-928）
- **C-PERF-003**: `update_thumbnail` O(n) 搜索 + `normalize_path` 分配 → 已修复（`normalized_path` 派生字段，manager.rs:113-116, 498, 555）
- **C-PERF-004**: 视频 re-encode → 已修复（直接 rename，thumbnail.rs:488-494）
- **C-PERF-005**: config 与 library 落盘互相阻塞 → 已修复（拆分 `config_save_mutex` + `library_save_mutex`，manager.rs:238-242）
- **C-PERF-007**: Triangle vs Lanczos3 → 已修复（`FilterType::Triangle`，thumbnail.rs:271）
- **C-PERF-010**: 孤儿 `.lock` 检查 O(N×M) → 已修复（`HashSet`，manager.rs:702-736）

### 5.3 v6 新发现

> 以下为本文档新增的、v4.0/v5.0 未覆盖的技术债 ID：

- **死代码类**：C-TD-001、C-TD-002、C-TD-003、C-TD-004、C-TD-005、C-TD-006
- **冗余抽象类**：C-TD-007
- **重复实现类**：C-TD-008、C-TD-009、C-TD-010、C-TD-011
- **过时模式类**：C-TD-012
- **未使用导入类**：C-TD-013
- **过度设计类**：C-TD-014、C-TD-015
- **修复痕迹类**：C-TD-016、C-TD-017、C-TD-018、C-TD-019、C-TD-020、C-TD-021、C-TD-022、C-TD-023、C-TD-024
- **命名一致性类**：C-TD-025、C-TD-026、C-TD-027
- **注释陈旧类**：C-TD-028、C-TD-029、C-TD-030、C-TD-031、C-TD-032

共 32 项新增技术债，其中 P0（立即清理）14 项、P1/P2（谨慎清理）14 项、P3（评估后决定）3 项（部分项跨级别，按主要清理建议归类）。

## 6. v6.0 清理状态汇总

> 本章节记录 v6.0 技术债清理 spec（`cleanup-v6-config-tech-debt-2026-07-25`）对 config 模块 32 项技术债的清理结果。所有项均已落实（修复或决策保留），状态截至 2026-07-25。

### 6.1 P0 项（14 项，全部已落实）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| C-TD-001 | 死代码 | ✅ 已修复于 v6.0 | 删除 `WallpaperLibrary::new()`，测试改用 `WallpaperLibrary::default()` |
| C-TD-002 | 死代码 | ✅ 已修复于 v6.0 | 删除 `maybe_save_library` 及 `#[allow(dead_code)]` 标注 |
| C-TD-007 | 冗余抽象 | ✅ 已修复于 v6.0 | `guess_image_format` 从 `pub fn` 降级为 `pub(crate) fn` |
| C-TD-013 | 未使用导入 | ✅ 已修复于 v6.0 | 移除 `mod.rs` 中 `detect_wallpaper_type_by_content` 重导出 |
| C-TD-016 | 修复痕迹 | ✅ 已修复于 v6.0 | `normalize_path` 注释精简至 12 行，移除 C09/W13/ST-007 历史标记 |
| C-TD-017 | 修复痕迹 | ✅ 已修复于 v6.0 | `atomic_write` 历史修复注释合并到函数级文档 |
| C-TD-018 | 修复痕迹 | ✅ 已修复于 v6.0 | `notify_config_error` 注释改为当前行为描述，移除 C-019 引用 |
| C-TD-022 | 修复痕迹 | ✅ 已修复于 v6.0 | `detect.rs` 七个历史标记移除，改为当前行为描述 |
| C-TD-023 | 修复痕迹 | ✅ 已修复于 v6.0 | `tmp_frame_name_from_path` 注释精简至 10 行 |
| C-TD-024 | 修复痕迹 | ✅ 已修复于 v6.0 | `settings.rs` 中 `v41-W-012` 对 `gif_decode.rs` 历史引用移除 |
| C-TD-028 | 注释陈旧 | ✅ 已决策于 v6.0 | 保持现状，crate 根已重导出 `AppConfig`，TODO 注释已更新 |
| C-TD-030 | 注释陈旧 | ✅ 已修复于 v6.0 | `hot_reload.rs` 模块文档补充 `sync_all` fsync 环节 |
| C-TD-031 | 注释陈旧 | ✅ 已修复于 v6.0 | `detect.rs` 模块文档更新，反映 `detect_wallpaper_type_by_content` 重导出已移除 |
| C-TD-032 | 注释陈旧 | ✅ 已修复于 v6.0 | `thumbnail.rs` 模块文档反映 v5.0 跳过 re-encode |

### 6.2 P1 项（7 项，全部已落实）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| C-TD-003 | 死代码 | ✅ 已修复于 v6.0 | 删除 `TmpFrameGuard::disarm` 方法及对应测试 |
| C-TD-004 | 死代码 | ✅ 已决策于 v6.0 | 保留 `detect_wallpaper_type_by_content` 函数（重导出已移除），为未来扩展预留 |
| C-TD-005 | 死代码 | ✅ 已修复于 v6.0 | 删除 `is_watcher_alive` pub API |
| C-TD-006 | 死代码 | ✅ 已修复于 v6.0 | 删除 `WebConfig` 结构体及前端引用清理 |
| C-TD-014 | 过度设计 | ✅ 已修复于 v6.0 | 简化 `TmpFrameGuard` 为 `struct TmpFrameGuard<'a>(&'a Path);`，不引入 scopeguard |
| C-TD-015 | 过度设计 | ✅ 已修复于 v6.0 | 删除 `watcher_alive` 整套机制（字段 + 4 处维护点 + 测试） |
| C-TD-021 | 修复痕迹 | ✅ 已修复于 v6.0 | 注释中 v41-C-004 引用已移除 |

### 6.3 P2 项（9 项，全部已落实）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| C-TD-008 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `read_bounded_utf8_file` 辅助函数 |
| C-TD-009 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `save_with_dirty_rollback` 辅助函数 |
| C-TD-010 | 重复实现 | ✅ 已修复于 v6.0 | 抽取 `invoke_callback_safe` 辅助函数 |
| C-TD-011 | 重复实现 | ✅ 已修复于 v6.0 | 提取 `make_temp_config_manager` 到 `test_support` 模块 |
| C-TD-012 | 过时模式 | ✅ 已修复于 v6.0 | 统一 dirty 标志 Ordering 为 `Relaxed` |
| C-TD-019 | 修复痕迹 | ✅ 已修复于 v6.0 | 拆分为 `cleanup_zero_byte_thumbnails` + `cleanup_orphan_lock_files` |
| C-TD-020 | 修复痕迹 | ✅ 已修复于 v6.0 | 重复注释合并到 `read_bounded_utf8_file` |
| C-TD-025 | 命名一致性 | ✅ 已决策于 v6.0 | 保持 `normalize_path` 命名，wallpaper 模块迁移留待后续 |
| C-TD-026 | 命名一致性 | ✅ 已决策于 v6.0 | 保持 `flush`/`flush_library` 命名，补充注释说明 |

### 6.4 P3 项（2 项，全部已评估）

| ID | 类型 | 修复状态 | 落实说明 |
|---|---|---|---|
| C-TD-027 | 命名一致性 | ✅ 已修复于 v6.0 | 补充 `save_library_locked` 注释，说明 config 路径无需 `_locked` 版本 |
| C-TD-029 | 注释陈旧 | ✅ 已决策于 v6.0 | 不建立 spec ID 索引文档，测试函数名保留可追溯性 |

### 6.5 汇总统计

| 状态 | 数量 | 占比 | 说明 |
|---|---|---|---|
| 已修复 | 27 | 84% | 直接修改源码落实清理建议 |
| 已决策保留现状 | 5 | 16% | C-TD-004、C-TD-025、C-TD-026、C-TD-028、C-TD-029 |
| 未处理 | 0 | 0% | — |
| **合计** | **32** | **100%** | config 模块技术债清理完成 |

config 模块技术债清理 spec（`cleanup-v6-config-tech-debt-2026-07-25`）已全部落实，本模块技术债清理完成。
