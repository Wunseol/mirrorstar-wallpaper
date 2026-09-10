//! 壁纸轮换调度的持久化播放状态（设计 §4.3 / DR-32）。
//!
//! [`PlaybackState`] 是调度器的运行状态快照：以调度单元 key 为粒度，记录每个单元当前
//! 壁纸、生效池、顺序游览标与洗牌袋剩余量。持久化到 `data_root()` 下的 `playback.toml`，
//! 采用与 [`crate::config::ConfigManager`] 一致的"临时文件 + fsync + rename"原子写。
//!
//! 损坏或版本不符的 `playback.toml` 在加载时回退为默认空状态（DR-32），保证应用可启动；
//! 多实例写入（Web 壁纸 | 常规壁纸可能同时调度）由 fs2 文件锁（`.lock`）串行化（DR-37）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::MirrorStarError;

/// `playback.toml` 的 schema 版本（DR-32）。
pub const PLAYBACK_SCHEMA_VERSION: u32 = 1;

/// 播放状态（设计 §4.3）。
///
/// - `units`: key → [`Unit`]。`key` 即调度单元 key（如 `per_monitor` 下的显示器 id、
///   `all_same` / `span` 下的全局单元 key）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackState {
    pub version: u32,
    #[serde(default)]
    pub units: HashMap<String, Unit>,
}

/// 单个调度单元的播放状态（设计 §4.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    /// 调度单元 key
    pub key: String,
    /// 当前壁纸 id；None = 尚未设置
    #[serde(default)]
    pub current_wallpaper_id: Option<String>,
    /// 生效池 id；None = 回退"全部"池
    #[serde(default)]
    pub active_pool: Option<String>,
    /// 顺序游览标（id 锚定，DR-5）
    #[serde(default)]
    pub order_cursor: Option<String>,
    /// 洗牌袋剩余量（DR-18，袋空重洗）
    #[serde(default)]
    pub bag_remaining: Vec<String>,
    /// 单元是否启用
    #[serde(default)]
    pub enabled: bool,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            version: PLAYBACK_SCHEMA_VERSION,
            units: HashMap::new(),
        }
    }
}

impl PlaybackState {
    /// 按 key 建/取调度单元（不存在则插入默认单元并返回其可变引用）。
    pub fn ensure_unit(&mut self, key: String) -> &mut Unit {
        self.units.entry(key.clone()).or_insert_with(|| Unit {
            key,
            current_wallpaper_id: None,
            active_pool: None,
            order_cursor: None,
            bag_remaining: Vec::new(),
            enabled: false,
        })
    }
}

/// `playback.toml` 的存取 store。
///
/// 默认路径为 `data_root()/playback.toml`（`new()`）；测试可用 `new_in_dir` 指定目录。
/// store 不自持线程、不做防抖合并；由上层调度器决定何时 `save`（节流/定期）与 `load`。
#[derive(Debug, Clone)]
pub struct PlaybackStore {
    path: PathBuf,
}

impl Default for PlaybackStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackStore {
    /// 使用 `data_root()` 下的 `playback.toml` 构造 store。
    pub fn new() -> Self {
        Self::new_in_dir(crate::config::manager::data_root())
    }

    /// 指定数据目录下的 `playback.toml`（供集成测试使用临时目录，避免污染用户数据）。
    pub fn new_in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            path: dir.into().join("playback.toml"),
        }
    }

    /// 返回 `playback.toml` 的绝对路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载播放状态；文件不存在 / 解析失败 / 版本不符时 `tracing::warn!` 并回退默认（DR-32）。
    pub fn load(&self) -> PlaybackState {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    return PlaybackState::default();
                }
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "playback.toml 读取失败，回退默认状态（DR-32）"
                );
                return PlaybackState::default();
            }
        };

        match toml::from_str::<PlaybackState>(&content) {
            Ok(state) => {
                // 版本不符 → 回退重建（未来版本迁移在此扩展）
                if state.version != PLAYBACK_SCHEMA_VERSION {
                    tracing::warn!(
                        version = state.version,
                        expected = PLAYBACK_SCHEMA_VERSION,
                        "playback.toml 版本不符，回退重建默认状态（DR-32）"
                    );
                    return PlaybackState::default();
                }
                state
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "playback.toml 解析失败，回退默认状态（DR-32）"
                );
                PlaybackState::default()
            }
        }
    }

    /// 将播放状态原子写盘（临时文件 + fsync + rename）。失败仅记录 `tracing::warn!`，不 panic（PS1）。
    pub fn save(&self, state: &PlaybackState) -> Result<(), MirrorStarError> {
        let content = toml::to_string_pretty(state)?;
        atomic_write(&self.path, &content)
    }
}

/// 与 `ConfigManager` 一致的原子写：临时文件 → `sync_all` → rename，配合 fs2 文件锁串行化。
#[allow(clippy::incompatible_msrv)]
fn atomic_write(path: &Path, content: &str) -> Result<(), MirrorStarError> {
    let lock_path = path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;

    let temp_path = path.with_extension("tmp");
    let result = (|| -> Result<(), MirrorStarError> {
        use std::io::Write;
        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            // 显式 drop 句柄，确保 Windows 上 rename 前文件不被占用
            drop(file);
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e.into());
        }
        Ok(())
    })();

    if let Err(e) = lock_file.unlock() {
        tracing::warn!(error = %e, "释放文件锁失败");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (PlaybackStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mirrorstar_playback_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (PlaybackStore::new_in_dir(&dir), dir)
    }

    #[test]
    fn default_state_is_empty_with_schema_version() {
        let state = PlaybackState::default();
        assert_eq!(state.version, PLAYBACK_SCHEMA_VERSION);
        assert!(state.units.is_empty());
    }

    #[test]
    fn ensure_unit_creates_and_reuses_by_key() {
        let mut state = PlaybackState::default();
        {
            let unit = state.ensure_unit("m0".to_string());
            unit.current_wallpaper_id = Some("w1".to_string());
            unit.active_pool = Some("p1".to_string());
        }
        assert_eq!(state.units.len(), 1);

        // 同一 key 复用已建单元
        let unit = state.ensure_unit("m0".to_string());
        assert_eq!(unit.key, "m0");
        assert_eq!(unit.current_wallpaper_id.as_deref(), Some("w1"));
        assert_eq!(unit.active_pool.as_deref(), Some("p1"));
        assert_eq!(state.units.len(), 1, "同一 key 不应重复建单元");

        // 新 key 建默认单元
        let unit = state.ensure_unit("global".to_string());
        assert_eq!(unit.key, "global");
        assert!(unit.current_wallpaper_id.is_none());
        assert!(unit.active_pool.is_none());
        assert!(!unit.enabled);
    }

    #[test]
    fn roundtrip_persists_state() {
        let (store, dir) = temp_store();
        let mut state = PlaybackState::default();
        let unit = state.ensure_unit("m0".to_string());
        unit.current_wallpaper_id = Some("w2".to_string());
        unit.active_pool = Some("p9".to_string());
        unit.order_cursor = Some("w2".to_string());
        unit.bag_remaining = vec!["w3".to_string(), "w1".to_string()];
        unit.enabled = true;

        store.save(&state).expect("save playback state");

        let loaded = store.load();
        assert_eq!(loaded.version, PLAYBACK_SCHEMA_VERSION);
        let u = &loaded.units["m0"];
        assert_eq!(u.current_wallpaper_id.as_deref(), Some("w2"));
        assert_eq!(u.active_pool.as_deref(), Some("p9"));
        assert_eq!(u.order_cursor.as_deref(), Some("w2"));
        assert_eq!(u.bag_remaining, vec!["w3".to_string(), "w1".to_string()]);
        assert!(u.enabled);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_fallback_default_for_missing_file() {
        let (store, dir) = temp_store();
        let state = store.load();
        assert_eq!(state.version, PLAYBACK_SCHEMA_VERSION);
        assert!(state.units.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_fallback_default_for_corrupt_file() {
        let (store, dir) = temp_store();
        std::fs::write(&store.path, "not a toml [[[").unwrap();
        let state = store.load();
        assert_eq!(state.version, PLAYBACK_SCHEMA_VERSION);
        assert!(state.units.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_fallback_default_for_old_version() {
        let (store, dir) = temp_store();
        // 构造旧版本号（version=0）的文件
        let state = PlaybackState {
            version: 0,
            units: HashMap::new(),
        };
        std::fs::write(&store.path, toml::to_string(&state).unwrap()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.version, PLAYBACK_SCHEMA_VERSION, "旧版本应回退重建（DR-32）");
        assert!(loaded.units.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}