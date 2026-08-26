// COM 初始化的 RAII guard
// 将 CoInitializeEx / CoUninitialize 配对，确保任何返回路径都正确清理 COM。
use mirrorstar_core::MirrorStarError;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

pub(crate) struct ComGuard {
    // 仅当 CoInitializeEx 成功（含 S_FALSE 即本线程已初始化）时才需配对调用 CoUninitialize。
    initialized: bool,
}

impl ComGuard {
    pub(crate) fn new() -> Result<Self, MirrorStarError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            // S_OK 或 S_FALSE：COM 初始化成功，需配对调用 CoUninitialize
            Ok(ComGuard { initialized: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // WP07: RPC_E_CHANGED_MODE 表示 COM 已以其他模式（通常为 MTA）初始化，
            // WebView2 要求 STA，MTA 下无法工作。返回 Err 让 main 以非零退出码退出
            // （WP03 已实现退出码传播），避免静默进入不可用状态。
            Err(MirrorStarError::DesktopIntegration(format!(
                "COM 初始化失败: {:?} (RPC_E_CHANGED_MODE，已以不兼容模式初始化，WebView2 要求 STA)",
                hr
            )))
        } else {
            // 其他真正的初始化失败，返回 Err 让 main 提前退出
            Err(MirrorStarError::DesktopIntegration(format!(
                "COM 初始化失败: {:?}",
                hr
            )))
        }
    }

    /// 返回 COM 是否已成功初始化（仅供测试可观测）。
    ///
    /// `true` 表示 CoInitializeEx 返回 S_OK/S_FALSE（需配对调用 CoUninitialize）。
    /// WP07 修复后 RPC_E_CHANGED_MODE 会返回 Err（不再构造 `initialized=false` 的 ComGuard），
    /// 故本方法在 `ComGuard::new()` 返回 Ok 时恒为 `true`。生产构建通过 `#[cfg(test)]` 排除，
    /// 仅保留为单元测试提供 COM 初始化状态的内部可观测点（WP-012）。
    #[cfg(test)]
    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 ComGuard::new() 在独立线程中初始化 COM
    ///
    /// COM 初始化是线程局部状态，测试在独立线程运行以避免与其他测试相互干扰。
    /// 独立线程首次调用 CoInitializeEx(None, COINIT_APARTMENTTHREADED) 应返回 S_OK，
    /// is_initialized() 应为 true。
    ///
    /// WP07: RPC_E_CHANGED_MODE 现返回 Err（不再构造 initialized=false 的 ComGuard），
    /// 若测试线程已以 MTA 模式初始化，ComGuard::new() 会返回 Err，expect 会 panic。
    #[test]
    fn test_comguard_new_initializes_com() {
        let handle = std::thread::spawn(|| {
            let guard = ComGuard::new().expect("ComGuard::new 在独立线程应成功");
            let initialized = guard.is_initialized();
            assert!(
                initialized,
                "is_initialized 应为 true（ComGuard::new 返回 Ok 时 initialized 恒为 true）"
            );
            // guard 离开作用域时 drop，调用 CoUninitialize，不应 panic
        });
        handle.join().expect("线程应正常退出");
    }

    /// 测试 ComGuard::is_initialized() 访问器与 Drop 不 panic
    ///
    /// 在独立线程中创建并销毁多个 ComGuard，验证 CoInitializeEx/CoUninitialize
    /// 配对正确，无 panic 或资源泄漏。
    #[test]
    fn test_comguard_drop_no_panic() {
        let handle = std::thread::spawn(|| {
            // 创建第一个 guard 并显式 drop
            {
                let guard = ComGuard::new().expect("ComGuard::new 应成功");
                let _ = guard.is_initialized();
                drop(guard);
            }
            // 创建第二个 guard（验证 CoUninitialize 后可再次 CoInitializeEx）
            {
                let guard = ComGuard::new().expect("第二次 ComGuard::new 应成功");
                let _ = guard.is_initialized();
                // 隐式 drop
            }
        });
        handle.join().expect("线程应正常退出");
    }
}
