//! 日志保留与自动清理
//!
//! 解决日志目录无限增长：应用日志（按日轮转）、mpv 实例日志（每实例一个文件）
//! 与崩溃日志（追加写）分别设有保留上限。
//!
//! - 应用日志的保留上限由 `tracing-appender` 的 `max_log_files` 直接控制（见
//!   `lib.rs::init_logging`），本模块的 [`prune_logs`] 作为启动期兜底再兜一次。
//! - mpv 日志在每次构建新实例日志路径时增量剪枝（见 `wallpaper/video.rs`），
//!   启动期 [`prune_logs`] 再做一次全量兜底。
//! - 崩溃日志超过 [`CRASH_LOG_MAX_BYTES`] 时轮转为 `crash.log.1`（仅保留一份备份）。
//!
//! 所有操作均为 best-effort：失败（如 Windows 文件被占用/共享冲突）仅 `warn`
//! 记录并继续，绝不 `unwrap`/`panic`。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 按日轮转的应用日志保留个数
pub const APP_LOG_KEEP: usize = 7;

/// mpv 实例日志保留个数
pub const MPV_LOG_KEEP: usize = 20;

/// crash.log 大小上限（1 MiB），超过即轮转
pub const CRASH_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// 日志清理结果统计
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    /// 被删除的 mpv 日志文件数
    pub mpv_deleted: usize,
    /// 被删除的应用日志文件数
    pub app_deleted: usize,
    /// crash.log 是否被轮转
    pub crash_rotated: bool,
    /// 删除操作释放的字节数
    pub freed_bytes: u64,
}

/// 获取文件 mtime，取不到时回退 `SystemTime::UNIX_EPOCH` 以保证排序确定
fn mtime_of(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 获取文件大小，取不到时返回 0（仅用于统计释放字节数，best-effort）
fn file_len_best_effort(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// 扫描 `<root>/logs/` 下满足 `file_name_matches` 的普通文件，按 mtime 降序保留
/// 最新 `keep` 个，删除其余。返回 `(删除数, 释放字节数)`。
///
/// 目录不存在、读取失败、单个文件删除失败均被忽略（后者以 `warn` 记录）。
fn prune_matching(
    root: &Path,
    file_name_matches: impl Fn(&str) -> bool,
    keep: usize,
) -> (usize, u64) {
    let logs_dir = root.join("logs");
    let entries = match std::fs::read_dir(&logs_dir) {
        Ok(entries) => entries,
        // 目录不存在或不可读：视为无需清理
        Err(_) => return (0, 0),
    };

    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if !file_name_matches(name) || !path.is_file() {
            continue;
        }
        let mtime = mtime_of(&path);
        files.push((path, mtime));
    }

    if files.len() <= keep {
        return (0, 0);
    }

    // mtime 降序：最新在前；仅保留前 keep 个，删除其余
    files.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    let mut deleted = 0usize;
    let mut freed = 0u64;
    for (path, _) in files.iter().skip(keep) {
        let len = file_len_best_effort(path);
        match std::fs::remove_file(path) {
            Ok(()) => {
                deleted += 1;
                freed += len;
            }
            // Windows 下文件可能被占用/共享冲突，忽略并继续
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "删除日志文件失败，已忽略"),
        }
    }
    (deleted, freed)
}

/// 剪枝 mpv 实例日志（`<root>/logs/mpv-*.log`），保留最新 `keep` 个。
///
/// best-effort：目录不存在或删除失败均不报错、不 panic。
pub fn prune_mpv_logs(root: &Path, keep: usize) -> PruneReport {
    let (deleted, freed) = prune_matching(
        root,
        |name| name.starts_with("mpv-") && name.ends_with(".log"),
        keep,
    );
    PruneReport {
        mpv_deleted: deleted,
        freed_bytes: freed,
        ..PruneReport::default()
    }
}

/// 启动期全量日志清理：
/// - 剪枝 mpv 实例日志（保留 [`MPV_LOG_KEEP`] 个）；
/// - 剪枝按日轮转的应用日志 `mirrorstar.log.<日期>`（保留 [`APP_LOG_KEEP`] 个）；
/// - 超过 [`CRASH_LOG_MAX_BYTES`] 的 `crash.log` 轮转为 `crash.log.1`。
///
/// 全程 best-effort，失败仅 `warn`。
pub fn prune_logs(root: &Path) -> PruneReport {
    let mut report = prune_mpv_logs(root, MPV_LOG_KEEP);

    let (app_deleted, app_freed) = prune_matching(
        root,
        |name| name.starts_with("mirrorstar.log.") && name.len() > "mirrorstar.log.".len(),
        APP_LOG_KEEP,
    );
    report.app_deleted = app_deleted;
    report.freed_bytes += app_freed;

    report.crash_rotated = rotate_crash_log(root);
    report
}

/// 若 `crash.log` 存在且大小超过 [`CRASH_LOG_MAX_BYTES`]，轮转为 `crash.log.1`
/// （旧备份先 best-effort 删除）。返回是否实际轮转。
///
/// 除启动期 [`prune_logs`] 兜底调用外，本函数亦由 panic hook
/// （`src-tauri/src/lib.rs::trace_panic_info`）在每次追加写前调用，用于会话内自限幅。
pub fn rotate_crash_log(root: &Path) -> bool {
    let crash = root.join("crash.log");
    let len = match std::fs::metadata(&crash) {
        Ok(meta) => meta.len(),
        // 不存在或不可读：无需轮转
        Err(_) => return false,
    };
    if len <= CRASH_LOG_MAX_BYTES {
        return false;
    }

    // 先删除已存在的旧备份（NotFound 属正常，忽略）
    let rotated = root.join("crash.log.1");
    if let Err(e) = std::fs::remove_file(&rotated) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %rotated.display(), error = %e, "删除旧崩溃日志备份失败，已忽略");
        }
    }

    match std::fs::rename(&crash, &rotated) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "轮转崩溃日志失败，已忽略");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    /// 在 `path` 写入 `content` 并设置确定性 mtime（UNIX_EPOCH + secs），返回字节数
    fn create_log(path: &Path, content: &[u8], secs: u64) -> u64 {
        let mut file = std::fs::File::create(path).expect("创建测试文件失败");
        file.write_all(content).expect("写入测试文件失败");
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("设置 mtime 失败");
        content.len() as u64
    }

    fn logs_dir(root: &Path) -> PathBuf {
        let dir = root.join("logs");
        std::fs::create_dir_all(&dir).expect("创建 logs 目录失败");
        dir
    }

    #[test]
    fn prune_mpv_keeps_newest_and_deletes_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let logs = logs_dir(root);
        // 5 个 mpv 日志，mtime 递增（mpv-5 最新）
        for i in 1..=5u64 {
            create_log(&logs.join(format!("mpv-{i}.log")), b"x", i * 10);
        }

        let report = prune_mpv_logs(root, 3);
        assert_eq!(report.mpv_deleted, 2);
        assert_eq!(report.freed_bytes, 2);

        // 仅保留最新的 mpv-3/4/5
        for i in 1..=5u64 {
            let exists = logs.join(format!("mpv-{i}.log")).exists();
            assert_eq!(exists, i >= 3, "mpv-{i}.log 存在性不符预期");
        }
    }

    #[test]
    fn prune_app_logs_keeps_newest_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let logs = logs_dir(root);
        // 9 个应用日志，mtime 递增
        for i in 1..=9u64 {
            create_log(
                &logs.join(format!("mirrorstar.log.2026-10-{:02}", i)),
                b"y",
                i * 10,
            );
        }

        let report = prune_logs(root);
        assert_eq!(report.app_deleted, 2);
        assert_eq!(report.freed_bytes, 2);

        for i in 1..=9u64 {
            let exists = logs
                .join(format!("mirrorstar.log.2026-10-{:02}", i))
                .exists();
            assert_eq!(exists, i > 2, "应用日志 i={i} 存在性不符预期");
        }
    }

    #[test]
    fn oversized_crash_log_is_rotated_overwriting_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        logs_dir(root);

        let crash = root.join("crash.log");
        let big_len = create_log(&crash, &vec![b'a'; (CRASH_LOG_MAX_BYTES + 1) as usize], 100);
        // 预置旧备份，应被覆盖
        let old_backup = root.join("crash.log.1");
        create_log(&old_backup, b"old", 50);

        let report = prune_logs(root);
        assert!(report.crash_rotated);
        assert!(!crash.exists(), "crash.log 轮转后应不存在");

        let backup = std::fs::read(&old_backup).expect("读取 crash.log.1 失败");
        assert_eq!(backup.len() as u64, big_len);
        assert_ne!(backup, b"old", "旧备份应被新内容覆盖");
    }

    #[test]
    fn missing_or_empty_dir_is_noop() {
        let dir = tempfile::tempdir().unwrap();

        // 目录不存在
        let missing = dir.path().join("does-not-exist");
        assert_eq!(prune_logs(&missing), PruneReport::default());

        // 空目录
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(prune_logs(&empty), PruneReport::default());
    }

    #[test]
    fn prune_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let logs = logs_dir(root);
        for i in 1..=5u64 {
            create_log(&logs.join(format!("mpv-{i}.log")), b"x", i * 10);
        }

        let first = prune_mpv_logs(root, 3);
        assert_eq!(first.mpv_deleted, 2);

        // 第二次调用不应再删除任何文件
        let second = prune_mpv_logs(root, 3);
        assert_eq!(second, PruneReport::default());
    }

    #[test]
    fn rotate_is_noop_when_under_cap_and_repeated_rotate_keeps_single_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let crash = root.join("crash.log");
        // 未超限：不轮转
        create_log(&crash, b"small", 10);
        assert!(!rotate_crash_log(root), "未超限不应轮转");
        assert!(crash.exists(), "未轮转时 crash.log 应仍存在");
        // 超限：轮转出 crash.log.1
        create_log(&crash, &vec![b'b'; (CRASH_LOG_MAX_BYTES + 1) as usize], 20);
        assert!(rotate_crash_log(root), "超限应轮转");
        assert!(root.join("crash.log.1").exists(), "应产生 crash.log.1");
        // 再次超限：仍只保留一份 .1（不累积）
        create_log(&crash, &vec![b'c'; (CRASH_LOG_MAX_BYTES + 1) as usize], 30);
        assert!(rotate_crash_log(root), "再次超限应轮转");
        let backups = std::fs::read_dir(root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("crash.log."))
            .count();
        assert_eq!(backups, 1, "应仅保留一份 crash.log.1");
    }
}
