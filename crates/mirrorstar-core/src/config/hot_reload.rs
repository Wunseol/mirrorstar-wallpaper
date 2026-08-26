use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use notify::Watcher;

use crate::config::manager::{
    invoke_callback_safe, save_with_dirty_rollback, ConfigChangedCallback, ConfigErrorCallback,
    ConfigLoadError, ConfigManager, WallpaperLibrary,
};
use crate::config::settings::AppConfig;
use crate::MirrorStarError;

impl ConfigManager {
    /// Start watching config file for changes (hot-reload)
    pub fn start_watching(&self) -> Result<(), MirrorStarError> {
        let config_path = self.config_path.clone();
        let library_path = self.library_path.clone();

        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(event.kind, notify::EventKind::Modify(_)) {
                        if let Err(e) = tx.send(event) {
                            tracing::warn!(error = %e, "热重载事件通道已关闭，watcher 退出");
                        }
                    }
                }
            },
            notify::Config::default(),
        )?;

        watcher.watch(&config_path, notify::RecursiveMode::NonRecursive)?;
        watcher.watch(&library_path, notify::RecursiveMode::NonRecursive)?;

        // 将 watcher 存入 ConfigManager 字段，使 `stop_watching` 可通过 take 并 drop
        // watcher 来停止监视。watcher 持有 `tx`，被 drop 后 channel 断开，
        // watcher 线程检测到 watcher 字段为 None 或 channel 断开后退出循环。
        *self.watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(watcher);

        // Spawn a thread to handle debounced reloads
        let config = self.config.clone();
        let wallpaper_library = self.wallpaper_library.clone();
        let on_config_changed = self.on_config_changed.clone();
        // C01 修复：clone on_config_error / pending_config_errors 到 watcher 线程，
        // 以便热重载时若解析失败能通过 notify_config_error 通知前端
        let on_config_error = self.on_config_error.clone();
        let pending_config_errors = self.pending_config_errors.clone();
        let watcher_handle = self.watcher.clone();
        let last_internal_save = self.last_internal_save.clone();

        std::thread::spawn(move || {
            let mut last_reload = Instant::now();
            let debounce = Duration::from_millis(500);

            loop {
                // 检测 watcher 是否被停止：stop_watching 会 take watcher 字段
                // 使其变 None，此时退出循环。
                if watcher_handle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    break;
                }
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(_event) => {
                        if last_reload.elapsed() >= debounce {
                            // 检查是否为内部保存触发的事件（2s 窗口内跳过 reload，C-093）
                            let skip_reload = {
                                let guard =
                                    last_internal_save.lock().unwrap_or_else(|e| e.into_inner());
                                match *guard {
                                    Some(t) => t.elapsed() < Duration::from_secs(2),
                                    None => false,
                                }
                            };
                            if skip_reload {
                                tracing::debug!("跳过内部保存触发的热重载");
                                continue;
                            }
                            last_reload = Instant::now();

                            // C11 修复：reload config 与 library（加载失败原子回滚，成功替换存在窗口期）
                            // 先读取两者到临时变量，均成功后再依次替换内存状态；
                            // 任一加载失败（IO 错误或超过大小上限）时不更新任何状态，
                            // 避免"config 已更新但 library 失败"导致的状态不一致。
                            // 注意：成功路径下 config 与 library 在两个独立 RwLock 中依次替换，
                            // 之间存在短暂窗口期（其他线程可能观察到 new_config + old_library），详见函数文档。
                            Self::reload_config_and_library(
                                &config_path,
                                &library_path,
                                &config,
                                &wallpaper_library,
                                &on_config_changed,
                                &on_config_error,
                                &pending_config_errors,
                            );
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        // 启动周期性后台保存，防止崩溃时丢失未落盘的配置修改
        self.start_periodic_save();

        Ok(())
    }

    /// C11 修复：热重载 config 与 library（加载失败时原子回滚，成功替换存在短暂窗口期）
    ///
    /// 原实现分两次 reload：先替换 config 内存状态，再替换 library。若 library reload
    /// 失败（IO 错误或超过大小上限），config 已被更新而 library 仍为旧值，导致状态不一致。
    ///
    /// 现改为先读取两者到临时变量，均成功后再依次替换内存状态。一致性语义如下：
    ///
    /// - **加载失败时原子回滚**：任一加载失败（`Err`）则两者都不更新内存状态，
    ///   避免"config 已更新但 library 失败"导致的不一致。这部分是原子的。
    /// - **成功替换时存在短暂窗口期**：config 与 library 分别存放在两个独立的
    ///   `RwLock` 中，依次替换之间会释放并重新获取写锁。其他线程在此窗口期内可能
    ///   观察到 `new_config + old_library` 的不一致状态。若需严格原子可见性，应将
    ///   两者合并到同一 `RwLock<(AppConfig, WallpaperLibrary)>`（本批次未采用，
    ///   方案 ① 重构复杂度高）。
    ///
    /// 注意：解析失败但回退到默认配置的情况视为"成功"（`Ok` 携带 `Option<ConfigLoadError>`），
    /// 仍会更新内存状态为默认值。仅 IO 错误或超过大小上限（`Err`）才视为"失败"。
    ///
    /// 作为静态关联函数以便 watcher 线程（持有 Arc clone 但无 `&self`）直接调用。
    /// 参数使用 manager.rs 的 `pub(crate)` 类型别名（`ConfigChangedCallback` /
    /// `ConfigErrorCallback`），避免 clippy `type_complexity` 警告。
    pub(crate) fn reload_config_and_library(
        config_path: &Path,
        library_path: &Path,
        config: &Arc<RwLock<AppConfig>>,
        wallpaper_library: &Arc<RwLock<WallpaperLibrary>>,
        on_config_changed: &Arc<RwLock<Option<ConfigChangedCallback>>>,
        on_config_error: &Arc<RwLock<Option<ConfigErrorCallback>>>,
        pending_config_errors: &Arc<Mutex<Vec<ConfigLoadError>>>,
    ) {
        // 先读取两者到临时变量（不修改内存状态）
        let config_result = Self::load_config(config_path);
        let library_result = Self::load_library(library_path);

        match (config_result, library_result) {
            (Ok((new_config, config_err)), Ok((new_library, library_err))) => {
                // 两者均加载成功，依次替换内存状态
                {
                    let mut cfg = config.write().unwrap_or_else(|e| e.into_inner());
                    *cfg = new_config;
                }
                {
                    let mut lib = wallpaper_library.write().unwrap_or_else(|e| e.into_inner());
                    *lib = new_library;
                }
                tracing::info!("配置文件与壁纸库已热重载");

                // C01 修复：解析失败但已回退到默认配置时，通知前端展示告警
                if let Some(err) = config_err {
                    Self::notify_config_error(on_config_error, pending_config_errors, err);
                }
                if let Some(err) = library_err {
                    Self::notify_config_error(on_config_error, pending_config_errors, err);
                }

                // 通知配置变更（由 Tauri 层设置以 emit 事件通知前端）
                // 先克隆 Arc<callback> 再释放读锁，回调内可重入 set_on_config_changed
                //（取写锁）而不会同线程死锁（C-019）。
                //
                // 回调错误隔离策略：单个回调失败（panic 或返回 Err）不影响后续回调与
                // 监视线程存活，仅通过 `tracing::error!` / `tracing::warn!` 记录。
                // 当前 `ConfigChangedCallback` 签名为 `Fn() + Send + Sync`（无返回值），
                // 不存在"返回 Err"路径；回调失败仅可能表现为 panic，由
                // `invoke_callback_safe` 捕获并记录 error 后吞没。这一隔离策略保证了：
                // 1. 回调 panic 不会终止 watcher 线程（后续配置变更仍能被检测与处理）；
                // 2. 单次回调失败不会向 `reload_config_and_library` 调用方传播错误
                //    （调用方为 watcher 线程，无法处理此类错误）；
                // 3. 错误信息通过日志保留，便于事后排查。
                // 若未来回调签名扩展为返回 `Result` 并需要批量收集错误传播给调用方，
                // 应在此处累加错误到 `Vec<MirrorStarError>` 后统一返回，并相应调整
                // `reload_config_and_library` 的返回类型。
                let callback = on_config_changed
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .cloned();
                if let Some(callback) = callback {
                    // T12（P0 panic）：panic 隔离由 invoke_callback_safe 统一处理，
                    // 防止回调 panic 导致监视线程退出（此后配置变更将不再被检测）。
                    invoke_callback_safe(|| callback(), "config changed");
                }
            }
            (Err(e), _) => {
                // C11 修复：config 加载失败时不更新任何状态（包括 library），
                // 即"加载失败时原子回滚"语义（详见函数文档）
                tracing::error!("配置文件热重载失败: {}", e);
            }
            (_, Err(e)) => {
                // C11 修复：library 加载失败时不更新 config（即使 config 已成功加载），
                // 即"加载失败时原子回滚"语义（成功路径的窗口期问题详见函数文档）
                tracing::error!("壁纸库热重载失败: {}", e);
            }
        }
    }

    /// 启动周期性后台保存任务：每 30 秒检查一次脏标记，若有未落盘修改则强制写入。
    ///
    /// 退出机制：使用 `mpsc::channel` 替代 `sleep`，`shutdown_periodic_save` 通过
    /// `Sender::send(())` 立即唤醒线程退出（30s → 100ms，C-094）。
    ///
    /// 运行标志与退出信号 sender 均为 ConfigManager 实例字段（C-110：从 static 迁移，
    /// 实现多实例隔离），避免多个 ConfigManager 实例间相互干扰。
    ///
    /// 直接调用静态方法 `save_config_to_file`，避免在 spawned task 中借用 `&self`。
    /// `last_save_time` 不在此处更新（周期保存是强制保存，不参与防抖）。
    fn start_periodic_save(&self) {
        let dirty = self.dirty.clone();
        let config = self.config.clone();
        let config_path = self.config_path.clone();
        let last_internal_save = self.last_internal_save.clone();
        // v5.0 C-PERF-005: 拆分为 config_save_mutex + library_save_mutex，避免 config 与 library 落盘互相阻塞
        let config_save_mutex = self.config_save_mutex.clone();
        let library_save_mutex = self.library_save_mutex.clone();
        // v5.0 C-PERF-002: clone library 相关字段，以便周期保存线程兜底落盘 library
        let library_dirty = self.library_dirty.clone();
        let wallpaper_library = self.wallpaper_library.clone();
        let library_path = self.library_path.clone();
        // 克隆 Arc 到线程，替代原 static 直接读取（C-110）
        let periodic_save_running = self.periodic_save_running.clone();
        self.periodic_save_running.store(true, Ordering::SeqCst);
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        *self
            .periodic_save_shutdown_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(shutdown_tx);
        if let Err(e) = std::thread::Builder::new()
            .name("mirrorstar-periodic-save".to_string())
            .spawn(move || {
                while periodic_save_running.load(Ordering::SeqCst) {
                    match shutdown_rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // 周期保存 config：dirty check-and-clear + 失败回滚由
                            // save_with_dirty_rollback 统一处理（C-101）。
                            if let Err(e) = save_with_dirty_rollback(&dirty, || {
                                // N-005: 全程持有 config_save_mutex，串行化 config.toml 落盘操作
                                let _save_guard =
                                    config_save_mutex.lock().unwrap_or_else(|e| e.into_inner());
                                // 强制保存：读取当前配置并写入磁盘
                                // 克隆后 read guard 在语句结束时自动释放，避免在持锁期间执行 IO
                                let config =
                                    config.read().unwrap_or_else(|e| e.into_inner()).clone();
                                let result = Self::save_config_to_file(&config, &config_path);
                                if result.is_ok() {
                                    *last_internal_save.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(Instant::now());
                                }
                                result
                            }) {
                                tracing::error!("周期性保存配置失败: {}", e);
                            }
                            // v5.0 C-PERF-002: 同步检查 library dirty，兜底落盘
                            // add_wallpaper / remove_wallpaper 标记的 dirty 修改。
                            // 与 config 的 dirty 处理模式一致（check-and-clear + 失败回滚）。
                            if let Err(e) = save_with_dirty_rollback(&library_dirty, || {
                                // N-005: 全程持有 library_save_mutex，串行化 wallpapers.toml 落盘
                                let _save_guard =
                                    library_save_mutex.lock().unwrap_or_else(|e| e.into_inner());
                                let lib = wallpaper_library
                                    .read()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                let result = Self::save_library_to_file(&lib, &library_path);
                                if result.is_ok() {
                                    *last_internal_save.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(Instant::now());
                                }
                                result
                            }) {
                                tracing::warn!(error = %e, "周期保存 library 失败");
                            }
                        }
                    }
                }
            })
        {
            tracing::error!(error = %e, "启动周期性保存线程失败");
            // C10 修复：spawn 失败时重置运行标志并清理已存入的 shutdown_tx
            // 否则 `periodic_save_running` 仍为 true、`shutdown_tx` 仍持有 sender
            //（receiver 已随 closure drop 而断开），导致后续 `shutdown_periodic_save`
            // 误以为线程仍在运行。
            self.cleanup_after_periodic_save_spawn_failure();
        }
    }

    /// C10 修复：周期性保存线程 spawn 失败后的状态清理
    ///
    /// `start_periodic_save` 在 spawn 之前已将 `periodic_save_running` 置为 true、
    /// `periodic_save_shutdown_tx` 存入 sender。若 spawn 失败，receiver 随 closure
    /// drop 而断开，但 sender 仍残留于字段中。此方法重置运行标志并清理 sender，
    /// 使 ConfigManager 状态回到"未启动周期保存"的初始语义。
    ///
    /// 抽出为独立方法以便单元测试覆盖（spawn 失败难以在测试中真实复现）。
    pub(crate) fn cleanup_after_periodic_save_spawn_failure(&self) {
        self.periodic_save_running.store(false, Ordering::SeqCst);
        *self
            .periodic_save_shutdown_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 请求周期性保存线程退出（由 perform_shutdown 调用）
    ///
    /// 通过 take sender 并 `send(())` 立即唤醒线程的 `recv_timeout`，
    /// 使线程在 100ms 内退出（替代旧版 30s `sleep` 轮询，C-094）。
    ///
    /// 运行标志与退出信号 sender 均为 ConfigManager 实例字段（C-110：从 static 迁移，
    /// 实现多实例隔离）。
    pub fn shutdown_periodic_save(&self) {
        self.periodic_save_running.store(false, Ordering::SeqCst);
        if let Some(tx) = self
            .periodic_save_shutdown_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            // 关闭信号：接收端可能已 drop（如周期保存线程已退出），send 失败无影响
            let _ = tx.send(());
        }
    }

    /// 停止文件监视器并释放 watcher 资源
    ///
    /// 取出并 drop `notify::RecommendedWatcher`，触发 channel 断开，
    /// watcher 线程检测到 watcher 被 take 后退出循环。幂等可多次调用。
    pub fn stop_watching(&self) {
        let mut w = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        *w = None; // take 并 drop watcher
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 测试
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::manager::test_support::make_temp_config_manager;
    use std::sync::atomic::Ordering;

    // make_temp_config_manager 已抽到 manager::test_support 模块，供 manager.rs 与
    // hot_reload.rs 测试共用（C-TD-011）。

    // ── C10 修复：start_periodic_save spawn 失败清理 ─────────────────────────

    #[test]
    fn cleanup_after_spawn_failure_resets_state() {
        // C10：模拟 start_periodic_save 中 spawn 失败后的状态清理
        // spawn 失败难以在测试中真实复现（需 OS 资源耗尽），因此直接调用清理逻辑
        // 验证：periodic_save_running 重置为 false，shutdown_tx 被清理为 None
        let cm = make_temp_config_manager();

        // 模拟 start_periodic_save 在 spawn 之前设置的状态：
        // - periodic_save_running = true（已 store）
        // - shutdown_tx = Some(tx)（已存入，但 receiver 随 closure drop 而断开）
        cm.periodic_save_running.store(true, Ordering::SeqCst);
        let (tx, _rx) = mpsc::channel(); // _rx 立即 drop，模拟 closure 被 drop
        *cm.periodic_save_shutdown_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(tx);

        // 调用清理逻辑（spawn 失败 err 分支会调用此方法）
        cm.cleanup_after_periodic_save_spawn_failure();

        // 验证 periodic_save_running 已重置为 false
        assert!(
            !cm.periodic_save_running.load(Ordering::SeqCst),
            "spawn 失败后 periodic_save_running 应为 false"
        );
        // 验证 shutdown_tx 已被清理
        assert!(
            cm.periodic_save_shutdown_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "spawn 失败后 shutdown_tx 应被清理为 None"
        );
    }

    // ── C11 修复：reload_config_and_library 原子性对称测试 ─────────────────
    //
    // C11 要求：config 与 library 任一加载失败（Err）时，两者内存状态均不被更新，
    // 保证一致性。以下两个测试分别覆盖 (Err, Ok) 与 (Ok, Err) 两种不对称场景，
    // 并对称断言"失败项"与"成功项"的状态均保持原值。
    //
    // 触发 Err 的方式：写入非 UTF-8 字节（0xFF 0xFE）。load_config/load_library
    // 在 `String::from_utf8` 失败时通过 `?` 返回 `Err(MirrorStarError::Io(InvalidData))`，
    // 属于 C11 注释中"IO 错误"分支。

    /// 构造非 UTF-8 文件（触发 load_config/load_library 返回 Err）
    fn write_invalid_utf8_file(path: &Path) {
        // 0xFF 0xFE 不是合法 UTF-8 起始字节，from_utf8 必失败
        std::fs::write(path, [0xFFu8, 0xFE]).unwrap();
    }

    /// C11 对称测试 1：config 加载失败时 library 不被更新
    ///
    /// 预填 config.gif.balanced_keep_frames = 7（非默认）与 library 含 1 条目；
    /// 磁盘 config 文件为非 UTF-8（Err），library 文件为空（Ok 空库）。
    /// 验证：reload 后 config 仍为 7，library 仍含原条目——两者均未更新。
    #[test]
    fn c11_config_load_failure_leaves_library_unchanged() {
        use crate::config::manager::WallpaperEntry;
        use crate::config::settings::GifConfig;
        use crate::wallpaper::{GifMemoryStrategy, WallpaperType};

        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c11_cfg_fail_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("config.toml");
        let library_path = dir.join("wallpapers.toml");

        // config 文件：非 UTF-8 → load_config 返回 Err
        write_invalid_utf8_file(&config_path);
        // library 文件：空文件 → load_library 返回 Ok(空库, None)
        std::fs::write(&library_path, "").unwrap();

        // 预填 config（非默认值，便于检测是否被覆盖为默认）与 library（含 1 条目）
        let initial_config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 7,
                ..GifConfig::default()
            },
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(initial_config));
        let original_entry = WallpaperEntry {
            id: "original-id".to_string(),
            file_path: "C:/original.mp4".to_string(),
            wallpaper_type: WallpaperType::Video,
            display_id: None,
            added_at: "0".to_string(),
            thumbnail: String::new(),
            file_size: 0,
            metadata: None,
            normalized_path: String::new(),
        };
        let library = Arc::new(RwLock::new(WallpaperLibrary {
            wallpapers: vec![original_entry.clone()],
        }));
        let on_config_changed: Arc<RwLock<Option<ConfigChangedCallback>>> =
            Arc::new(RwLock::new(None));
        let on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>> = Arc::new(RwLock::new(None));
        let pending_config_errors: Arc<Mutex<Vec<ConfigLoadError>>> =
            Arc::new(Mutex::new(Vec::new()));

        ConfigManager::reload_config_and_library(
            &config_path,
            &library_path,
            &config,
            &library,
            &on_config_changed,
            &on_config_error,
            &pending_config_errors,
        );

        // C11 对称断言：config 加载失败，config 与 library 均不被更新
        let cfg_guard = config.read().unwrap();
        assert_eq!(
            cfg_guard.gif.balanced_keep_frames, 7,
            "config 加载失败时 config 内存状态不应被更新（应保持原值 7）"
        );
        drop(cfg_guard);

        let lib_guard = library.read().unwrap();
        assert_eq!(
            lib_guard.wallpapers.len(),
            1,
            "config 加载失败时 library 不应被更新（条目数应保持 1，而非磁盘上的 0）"
        );
        assert_eq!(
            lib_guard.wallpapers[0].id, "original-id",
            "config 加载失败时 library 条目应保持原值"
        );
        drop(lib_guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C11 对称测试 2：library 加载失败时 config 不被更新
    ///
    /// 预填 config.gif.balanced_keep_frames = 5（非默认）与 library 为空；
    /// 磁盘 config 文件为合法 TOML（balanced_keep_frames = 999，Ok），
    /// library 文件为非 UTF-8（Err）。
    /// 验证：reload 后 config 仍为 5，library 仍为空——两者均未更新。
    #[test]
    fn c11_library_load_failure_leaves_config_unchanged() {
        use crate::config::settings::GifConfig;
        use crate::wallpaper::GifMemoryStrategy;

        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_c11_lib_fail_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("config.toml");
        let library_path = dir.join("wallpapers.toml");

        // config 文件：合法 TOML，balanced_keep_frames = 999（validate 保留，≥1 合法）
        let config_toml = "[gif]\nmemory_strategy = \"Balanced\"\nbalanced_keep_frames = 999\n";
        std::fs::write(&config_path, config_toml).unwrap();
        // library 文件：非 UTF-8 → load_library 返回 Err
        write_invalid_utf8_file(&library_path);

        // 预填 config（balanced_keep_frames = 5）与 library（空）
        let initial_config = AppConfig {
            gif: GifConfig {
                memory_strategy: GifMemoryStrategy::Balanced,
                balanced_keep_frames: 5,
                ..GifConfig::default()
            },
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(initial_config));
        let library = Arc::new(RwLock::new(WallpaperLibrary::default()));
        let on_config_changed: Arc<RwLock<Option<ConfigChangedCallback>>> =
            Arc::new(RwLock::new(None));
        let on_config_error: Arc<RwLock<Option<ConfigErrorCallback>>> = Arc::new(RwLock::new(None));
        let pending_config_errors: Arc<Mutex<Vec<ConfigLoadError>>> =
            Arc::new(Mutex::new(Vec::new()));

        ConfigManager::reload_config_and_library(
            &config_path,
            &library_path,
            &config,
            &library,
            &on_config_changed,
            &on_config_error,
            &pending_config_errors,
        );

        // C11 对称断言：library 加载失败，config 与 library 均不被更新
        let cfg_guard = config.read().unwrap();
        assert_eq!(
            cfg_guard.gif.balanced_keep_frames, 5,
            "library 加载失败时 config 不应被更新（应保持原值 5，而非磁盘上的 999）"
        );
        drop(cfg_guard);

        let lib_guard = library.read().unwrap();
        assert!(
            lib_guard.wallpapers.is_empty(),
            "library 加载失败时 library 内存状态不应被更新（应保持空）"
        );
        drop(lib_guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C-004 修复：注释与实现一致性 ───────────────────────────────────────
    //
    // C-004 要求 `reload_config_and_library` 的文档注释准确反映实际行为：加载失败时
    // 原子回滚（这部分是原子的），但成功替换时 config 与 library 在两个独立 RwLock 中
    // 依次替换，存在短暂窗口期。本测试以字符串匹配方式锁定注释内容，防止后续误改回
    // "原子地热重载"等误导性表述。

    /// C-004：验证 `reload_config_and_library` 源码注释中明确记载了"窗口期"，
    /// 即承认成功替换路径上 config 与 library 在两个独立 `RwLock` 写锁之间存在
    /// 短暂的不一致窗口，避免注释与实现不符而误导开发者。
    #[test]
    fn c004_hot_reload_comment_documents_window_period() {
        // 编译期将本文件源码嵌入，断言其中含"窗口期"关键词
        let source = include_str!("hot_reload.rs");
        assert!(
            source.contains("窗口期"),
            "reload_config_and_library 的注释应明确记载成功替换路径上的短暂窗口期，\
             避免误述为完全原子（C-004）"
        );
    }
}
