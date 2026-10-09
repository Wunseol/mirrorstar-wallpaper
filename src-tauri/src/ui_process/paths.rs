//! UI 子进程可执行文件路径解析（dev 与打包后统一）。
//!
//! # 为什么与 `find_bundled_executable` 做法不同
//!
//! mpv 走的是 `SubprocessRendererBase::find_bundled_executable`（应用目录 → 仓库根回溯
//! → PATH）；网页壁纸子进程已改为「同一 exe、双入口」，直接复用当前壳 exe 路径
//! （`--wp-proc` 模式）。但 `mirrorstar-ui.exe` 是**与主 exe 同目录的
//! workspace bin**：dev 模式下 cargo 把全部 bin 输出到 `target/<profile>/`，打包后安装器
//! 也把它与主 exe 同置（见 spec「应用清单」与打包资源约定）。因此这里只需要
//! "主 exe 同目录 → PATH 兜底"两步即可覆盖两种场景，无需仓库根回溯。

use std::path::{Path, PathBuf};

/// UI 子进程可执行文件名。
pub const UI_EXE_NAME: &str = "mirrorstar-ui.exe";

/// 从主 exe 所在目录解析 UI 进程路径。
///
/// - 同目录存在 `mirrorstar-ui.exe` → 返回其绝对路径（dev 的 `target/<profile>/` 与
///   打包后的安装目录都命中此分支）；
/// - 否则回退为裸文件名，交由操作系统在 `PATH` 中查找（便于手工放置 / 调试）。
pub fn resolve_from_exe_dir(exe_dir: &Path) -> PathBuf {
    let bundled = exe_dir.join(UI_EXE_NAME);
    if bundled.is_file() {
        bundled
    } else {
        PathBuf::from(UI_EXE_NAME)
    }
}

/// 运行时解析：取当前进程 exe 目录，按 [`resolve_from_exe_dir`] 解析。
pub fn resolve_ui_exe() -> PathBuf {
    match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        Some(dir) => resolve_from_exe_dir(&dir),
        None => PathBuf::from(UI_EXE_NAME),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bundled_exe_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(UI_EXE_NAME);
        std::fs::write(&exe, b"stub").unwrap();
        assert_eq!(resolve_from_exe_dir(dir.path()), exe);
    }

    #[test]
    fn falls_back_to_bare_name_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_from_exe_dir(dir.path()), PathBuf::from(UI_EXE_NAME));
    }

    #[test]
    fn ignores_directory_named_like_exe() {
        // 同名目录不应被当作可执行文件（is_file 判定）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(UI_EXE_NAME)).unwrap();
        assert_eq!(resolve_from_exe_dir(dir.path()), PathBuf::from(UI_EXE_NAME));
    }
}
