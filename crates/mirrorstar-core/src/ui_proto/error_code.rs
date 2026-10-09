//! 稳定错误码表：把 [`MirrorStarError`] 的分类映射为对端可分支处理的字符串码。
//!
//! ## 稳定性契约
//!
//! 错误码是协议的一部分：**只增不改**。对端（UI 进程 / Tauri 壳）会据此码做分支，
//! 因此不得因实现重构而改动既有码值。需要新增语义时另起新码，不复用旧码。
//!
//! ## 覆盖保证
//!
//! [`code_of`] 使用**穷尽 `match`**（逐变体列出，无 `_` 通配分支）。将来给
//! [`MirrorStarError`] 新增变体时，编译器会强制在此补充映射，避免遗漏后静默落入
//! 某个默认码导致对端误判。

use crate::MirrorStarError;

use super::message::UiError;

/// 桌面集成失败。
pub const DESKTOP_INTEGRATION: &str = "desktop_integration";
/// 未找到 WorkerW 窗口。
pub const WORKER_W_NOT_FOUND: &str = "worker_w_not_found";
/// 子进程启动失败。
pub const PROCESS_SPAWN_FAILED: &str = "process_spawn_failed";
/// IPC 通信失败。
pub const IPC_ERROR: &str = "ipc_error";
/// 音频控制失败。
pub const AUDIO_CONTROL: &str = "audio_control";
/// 配置文件解析失败。
pub const CONFIG_PARSE: &str = "config_parse";
/// 配置文件写入失败。
pub const CONFIG_WRITE: &str = "config_write";
/// 图片解码失败。
pub const IMAGE_DECODE: &str = "image_decode";
/// 文件监视器失败。
pub const FILE_WATCHER: &str = "file_watcher";
/// 锁中毒。
pub const LOCK_POISONED: &str = "lock_poisoned";
/// 任务 join 失败。
pub const TASK_JOIN: &str = "task_join";
/// IPC 超时。
pub const IPC_TIMEOUT: &str = "ipc_timeout";
/// IPC 连接断开。
pub const IPC_DISCONNECTED: &str = "ipc_disconnected";
/// IPC 未连接。
pub const IPC_NOT_CONNECTED: &str = "ipc_not_connected";
/// Windows API 错误。
pub const WIN32: &str = "win32";
/// 通用 IO 错误。
pub const IO: &str = "io";
/// 壁纸文件路径校验失败（含路径遍历拒绝）。
pub const INVALID_PATH: &str = "invalid_path";
/// 配置字段范围校验失败。
pub const INVALID_CONFIG: &str = "invalid_config";
/// URL 协议不在白名单。
pub const INVALID_URL: &str = "invalid_url";
/// 无效参数。
pub const INVALID_ARGUMENT: &str = "invalid_argument";
/// WebView2 异步操作超时。
pub const WEBVIEW2_TIMEOUT: &str = "webview2_timeout";
/// 未找到指定进程的音频会话。
pub const AUDIO_SESSION_NOT_FOUND: &str = "audio_session_not_found";

/// 取 [`MirrorStarError`] 对应的稳定错误码。
///
/// 返回 `&'static str` 以避免每次映射都分配字符串；调用方（如 [`UiError::from`]）
/// 在需要拥有所有权时自行 `to_string()`。
pub fn code_of(error: &MirrorStarError) -> &'static str {
    match error {
        MirrorStarError::DesktopIntegration(_) => DESKTOP_INTEGRATION,
        MirrorStarError::WorkerWNotFound => WORKER_W_NOT_FOUND,
        MirrorStarError::ProcessSpawnFailed(_) => PROCESS_SPAWN_FAILED,
        MirrorStarError::IpcError(_) => IPC_ERROR,
        MirrorStarError::AudioControl(_) => AUDIO_CONTROL,
        MirrorStarError::ConfigParse(_) => CONFIG_PARSE,
        MirrorStarError::ConfigWrite(_) => CONFIG_WRITE,
        MirrorStarError::ImageDecode(_) => IMAGE_DECODE,
        MirrorStarError::FileWatcher(_) => FILE_WATCHER,
        MirrorStarError::LockPoisoned(_) => LOCK_POISONED,
        MirrorStarError::TaskJoin(_) => TASK_JOIN,
        MirrorStarError::IpcTimeout(_) => IPC_TIMEOUT,
        MirrorStarError::IpcDisconnected(_) => IPC_DISCONNECTED,
        MirrorStarError::IpcNotConnected(_) => IPC_NOT_CONNECTED,
        MirrorStarError::Win32(_) => WIN32,
        MirrorStarError::Io(_) => IO,
        MirrorStarError::InvalidPath { .. } => INVALID_PATH,
        MirrorStarError::InvalidConfig { .. } => INVALID_CONFIG,
        MirrorStarError::InvalidUrl { .. } => INVALID_URL,
        MirrorStarError::InvalidArgument { .. } => INVALID_ARGUMENT,
        MirrorStarError::WebView2Timeout(_) => WEBVIEW2_TIMEOUT,
        MirrorStarError::AudioSessionNotFound { .. } => AUDIO_SESSION_NOT_FOUND,
    }
}

impl UiError {
    /// 从核心错误构造协议错误：`code` 取稳定错误码，`message` 取错误展示文本。
    pub fn from_core(error: &MirrorStarError) -> Self {
        Self {
            code: code_of(error).to_string(),
            message: error.to_string(),
        }
    }
}

impl From<&MirrorStarError> for UiError {
    fn from(error: &MirrorStarError) -> Self {
        UiError::from_core(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_proto::message::UiError;

    /// 构造全部 `MirrorStarError` 变体（新增变体时此列表需同步补充）。
    fn all_error_variants() -> Vec<(MirrorStarError, &'static str)> {
        vec![
            (
                MirrorStarError::DesktopIntegration("x".to_string()),
                DESKTOP_INTEGRATION,
            ),
            (MirrorStarError::WorkerWNotFound, WORKER_W_NOT_FOUND),
            (
                MirrorStarError::ProcessSpawnFailed("x".to_string()),
                PROCESS_SPAWN_FAILED,
            ),
            (MirrorStarError::IpcError("x".to_string()), IPC_ERROR),
            (
                MirrorStarError::AudioControl("x".to_string()),
                AUDIO_CONTROL,
            ),
            (MirrorStarError::ConfigParse("x".to_string()), CONFIG_PARSE),
            (MirrorStarError::ConfigWrite("x".to_string()), CONFIG_WRITE),
            (MirrorStarError::ImageDecode("x".to_string()), IMAGE_DECODE),
            (MirrorStarError::FileWatcher("x".to_string()), FILE_WATCHER),
            (
                MirrorStarError::LockPoisoned("x".to_string()),
                LOCK_POISONED,
            ),
            (MirrorStarError::TaskJoin("x".to_string()), TASK_JOIN),
            (MirrorStarError::IpcTimeout("x".to_string()), IPC_TIMEOUT),
            (
                MirrorStarError::IpcDisconnected("x".to_string()),
                IPC_DISCONNECTED,
            ),
            (
                MirrorStarError::IpcNotConnected("x".to_string()),
                IPC_NOT_CONNECTED,
            ),
            (
                MirrorStarError::Win32(windows::core::Error::from(
                    windows::Win32::Foundation::E_POINTER,
                )),
                WIN32,
            ),
            (MirrorStarError::Io(std::io::Error::other("x")), IO),
            (
                MirrorStarError::InvalidPath {
                    reason: "x".to_string(),
                },
                INVALID_PATH,
            ),
            (
                MirrorStarError::InvalidConfig {
                    reason: "x".to_string(),
                },
                INVALID_CONFIG,
            ),
            (
                MirrorStarError::InvalidUrl {
                    scheme: "x".to_string(),
                },
                INVALID_URL,
            ),
            (
                MirrorStarError::InvalidArgument {
                    reason: "x".to_string(),
                },
                INVALID_ARGUMENT,
            ),
            (
                MirrorStarError::WebView2Timeout("x".to_string()),
                WEBVIEW2_TIMEOUT,
            ),
            (
                MirrorStarError::AudioSessionNotFound { pid: 1 },
                AUDIO_SESSION_NOT_FOUND,
            ),
        ]
    }

    #[test]
    fn every_variant_maps_to_expected_code() {
        for (error, expected) in all_error_variants() {
            assert_eq!(code_of(&error), expected, "错误码映射不符: {:?}", error);
        }
    }

    #[test]
    fn every_variant_maps_to_non_empty_code() {
        for (error, _) in all_error_variants() {
            let code = code_of(&error);
            assert!(!code.is_empty(), "错误码不可为空: {:?}", error);
        }
    }

    #[test]
    fn codes_are_unique_across_variants() {
        // 不同变体必须映射到不同码，否则对端无法区分。
        let variants = all_error_variants();
        for (i, (_, a)) in variants.iter().enumerate() {
            for (_, b) in variants.iter().skip(i + 1) {
                assert_ne!(a, b, "存在重复错误码: {}", a);
            }
        }
    }

    #[test]
    fn ui_error_from_core_uses_code_and_message() {
        let error = MirrorStarError::WorkerWNotFound;
        let ui_error = UiError::from(&error);
        assert_eq!(ui_error.code, WORKER_W_NOT_FOUND);
        assert_eq!(ui_error.message, error.to_string());
        // 便捷构造与 From 实现一致
        assert_eq!(UiError::from_core(&error), ui_error);
    }
}
