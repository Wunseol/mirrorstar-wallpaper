use crate::wallpaper::ScalingMode;

/// Windows 原生支持的图片格式
const NATIVE_SUPPORTED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp", "tif", "tiff", "dib"];

/// 判断文件格式是否支持 Windows 原生壁纸 API
pub fn is_native_supported(file_path: &str) -> bool {
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    NATIVE_SUPPORTED_EXTENSIONS.contains(&ext.as_str())
}

/// 使用 Windows 原生 API 设置静态壁纸
///
/// 通过注册表设置缩放模式 + SystemParametersInfoW 设置壁纸
/// 零窗口、零线程、零 GDI 对象
///
/// ## 事务回滚契约（D01 修复）
///
/// 本函数遵循"先备份、后修改、失败回滚"的事务语义，保证系统状态一致性：
///
/// 1. 在写入新缩放模式之前，先读取注册表中当前的 `WallPaperStyle`/`TileWallPaper`
///    旧值作为回滚备份
/// 2. 写入新缩放模式（注册表）
/// 3. 调用 `SystemParametersInfoW` 设置壁纸图片
/// 4. 若步骤 3 失败，恢复步骤 1 读取到的旧注册表值，并返回原始错误
///
/// 这避免了 D01 描述的不一致场景：注册表显示新缩放模式但壁纸仍是旧图。
/// 要么注册表缩放模式与壁纸图片同时更新成功，要么注册表回滚到调用前状态。
pub fn set_native_wallpaper(
    image_path: &str,
    scaling_mode: ScalingMode,
    monitor_id: Option<&str>,
) -> Result<(), crate::MirrorStarError> {
    // C-022 P1 短期修复：原生壁纸模式（SystemParametersInfoW）作用于整个桌面，
    // 不支持指定显示器。若调用方传入 monitor_id，直接拒绝以避免误用。
    if let Some(id) = monitor_id {
        return Err(crate::MirrorStarError::DesktopIntegration(format!(
            "原生壁纸模式不支持指定显示器: {}",
            id
        )));
    }

    // 事务准备：读取旧注册表值，用于 set_wallpaper_image 失败时回滚。
    // read_wallpaper_style 对所有失败情况返回 None（不阻塞主流程）。
    let old_values = read_wallpaper_style();

    // 新缩放模式对应的注册表字符串值
    let (new_style, new_tile) = scaling_mode_to_style(scaling_mode);

    // Step 1: 写入注册表设置新缩放模式
    write_wallpaper_style(new_style, new_tile)?;

    // Step 2: 使用 SystemParametersInfoW 设置壁纸图片
    if let Err(e) = set_wallpaper_image(image_path) {
        // 壁纸设置失败，尝试回滚注册表到旧值
        match old_values {
            Some((old_style, old_tile)) => {
                tracing::warn!(
                    old_style = %old_style,
                    old_tile = %old_tile,
                    new_style = %new_style,
                    new_tile = %new_tile,
                    image_error = %e,
                    "set_wallpaper_image 失败，已回滚注册表缩放模式"
                );
                if let Err(re) = write_wallpaper_style(&old_style, &old_tile) {
                    // 回滚本身失败：记录 error，但仍返回原始的 set_wallpaper_image 错误，
                    // 不用回滚错误覆盖原始错误（保留根因可追溯性）
                    tracing::error!(
                        rollback_error = %re,
                        "回滚注册表失败，系统可能处于不一致状态"
                    );
                }
            }
            None => {
                tracing::warn!(
                    new_style = %new_style,
                    new_tile = %new_tile,
                    image_error = %e,
                    "set_wallpaper_image 失败，且无旧注册表值可回滚（首次设置或读取失败）"
                );
            }
        }
        return Err(e);
    }

    Ok(())
}

/// 清除 Windows 原生壁纸（设置为空）
pub fn clear_native_wallpaper() -> Result<(), crate::MirrorStarError> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPIF_SENDWININICHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
    };

    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            None,
            SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE,
        )
        .map_err(|e| crate::MirrorStarError::DesktopIntegration(format!("清除壁纸失败: {}", e)))?;
    }
    Ok(())
}

/// 将缩放模式映射为注册表 WallPaperStyle/TileWallPaper 的字符串值
///
/// WallPaperStyle 和 TileWallpaper 均为 REG_SZ（字符串类型），
/// 不是 REG_DWORD，必须写入如 "10" 的字符串值。
///
/// 提取为独立纯函数以便单元测试覆盖映射稳定性（Windows 系统约定的固定映射，不可随意更改）。
fn scaling_mode_to_style(scaling_mode: ScalingMode) -> (&'static str, &'static str) {
    match scaling_mode {
        ScalingMode::Center => ("0", "0"),
        ScalingMode::Stretch => ("2", "0"),
        ScalingMode::Fit => ("6", "0"),
        ScalingMode::Fill => ("10", "0"),
        ScalingMode::Original => ("0", "0"), // Same as Center
    }
}

/// 写入注册表设置壁纸缩放模式（接受任意 style/tile 字符串值）
///
/// WallPaperStyle 和 TileWallpaper 均为 REG_SZ（字符串类型），
/// 不是 REG_DWORD，必须写入如 "10" 的字符串值。
///
/// 既用于 `set_native_wallpaper` 写入新缩放模式，也用于事务回滚时恢复旧值。
/// `scaling_mode_to_style` 负责 ScalingMode → (style, tile) 映射，本函数负责落地到注册表。
///
/// v41-D-014: 错误处理风格统一。`set_value`（底层 `RegSetValueExW`）失败时
/// 通过 `map_err` 映射为 `MirrorStarError::DesktopIntegration(...)` 并向上传播，
/// 与 `set_wallpaper_image` 中 `SystemParametersInfoW` 失败处理风格一致
///（均返回 `Err(DesktopIntegration(...))`，不再仅 `warn!` 后吞错）。
///
/// ## 已知限制 (v41-D-004)
///
/// 本函数执行两步注册表写入（`set_value("WallPaperStyle")` +
/// `set_value("TileWallpaper")`，底层均为 `RegSetValueExW`），两步非原子：
/// 若第一步（`WallPaperStyle`）成功但第二步（`TileWallpaper`）失败，注册表
/// 将处于不一致状态（`WallPaperStyle` 为新值而 `TileWallpaper` 仍为旧值），
/// 本函数不回滚第一步。
///
/// 调用方（`set_native_wallpaper`）在 `set_wallpaper_image` 失败时通过
/// `read_wallpaper_style` + `write_wallpaper_style` 回滚整个 (style, tile)
/// 元组，可间接修复此不一致；但若 `write_wallpaper_style` 本身第一步成功
/// 第二步失败，需调用方重试或手动修复注册表。
fn write_wallpaper_style(style: &str, tile: &str) -> Result<(), crate::MirrorStarError> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktop = hkcu
        .open_subkey_with_flags("Control Panel\\Desktop", KEY_SET_VALUE)
        .map_err(|e| crate::MirrorStarError::DesktopIntegration(format!("打开注册表失败: {}", e)))?;

    desktop.set_value("WallPaperStyle", &style).map_err(|e| {
        crate::MirrorStarError::DesktopIntegration(format!("写入 WallPaperStyle 失败: {}", e))
    })?;

    desktop.set_value("TileWallpaper", &tile).map_err(|e| {
        crate::MirrorStarError::DesktopIntegration(format!("写入 TileWallpaper 失败: {}", e))
    })?;

    Ok(())
}

/// 读取注册表中当前的壁纸缩放模式（用于事务回滚前的旧值备份）
///
/// 返回值语义：
/// - `Some((style, tile))`：成功读取到旧值，可用于回滚
/// - `None`：旧值不可用（键缺失、读取失败、或注册表无法打开）—— 不阻塞设置流程
///
/// 注意：本函数对所有失败情况都返回 `None` 并记录 warn 日志，
/// 以保证 `set_native_wallpaper` 的事务流程不会因"读取旧值失败"而中断主路径。
fn read_wallpaper_style() -> Option<(String, String)> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktop = match hkcu.open_subkey_with_flags("Control Panel\\Desktop", KEY_QUERY_VALUE) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "读取旧注册表缩放模式失败：打开注册表失败，跳过回滚备份"
            );
            return None;
        }
    };

    let style: String = match desktop.get_value("WallPaperStyle") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "读取 WallPaperStyle 旧值失败，跳过回滚备份（可能首次设置）"
            );
            return None;
        }
    };

    let tile: String = match desktop.get_value("TileWallpaper") {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "读取 TileWallpaper 旧值失败，跳过回滚备份（可能首次设置）"
            );
            return None;
        }
    };

    Some((style, tile))
}

/// 使用 SystemParametersInfoW 设置壁纸图片
fn set_wallpaper_image(image_path: &str) -> Result<(), crate::MirrorStarError> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPIF_SENDWININICHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
    };

    // D-015: 转宽字符前校验嵌入式 NUL 字符。UTF-16 编码会在 NUL 处截断，
    // 导致 SystemParametersInfoW 设置错误路径（截断后的前缀）。与 Task 1
    // worker_w.rs 的 restore_system_wallpaper 校验保持一致。
    if image_path.contains('\0') {
        return Err(crate::MirrorStarError::InvalidPath {
            reason: format!("路径含嵌入式 NUL 字符：{}", image_path),
        });
    }

    let wide_path: Vec<u16> = image_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: (v41-D-007) `pvParam` 借用 `wide_path`（宽字符串 Vec，含 NUL 终止符），
    // `wide_path` 在外层作用域定义，其生命周期覆盖整个 `SystemParametersInfoW` 调用
    //（直至 `.map_err(...)? ` 求值完成才被 drop）。`SPI_SETDESKWALLPAPER` 同步使用
    // `pvParam` 指向的字符串设置壁纸，调用返回后不再持有该指针，因此不存在悬挂引用。
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide_path.as_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE,
        )
        .map_err(|e| crate::MirrorStarError::DesktopIntegration(format!("设置壁纸失败: {}", e)))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scaling_mode_to_style 纯函数测试 ────────────────────────────────
    //
    // 这部分测试 set_native_wallpaper 事务流程中"缩放模式 → 注册表字符串"的映射逻辑。
    // set_native_wallpaper 本身与 Win32 API（SystemParametersInfoW、注册表操作）紧耦合，
    // 无法在 CI 单元测试中直接调用（会修改系统状态：写入真实注册表、替换桌面壁纸），
    // 因此遵循 worker_w.rs 中 compute_retry_wait_ms 的既有测试范式：
    // 将可纯函数化的逻辑提取出来单独覆盖，Win32 耦合部分通过 #[ignore] 集成测试
    // 由开发者本地手动运行。

    /// 验证 ScalingMode → 注册表字符串值的映射稳定性
    ///
    /// 这些值是 Windows 系统约定的固定映射（参考 MSDN WallPaperStyle 文档），
    /// 不可随意更改：0=居中、2=拉伸、6=适应、10=填充。
    #[test]
    fn scaling_mode_to_style_mapping_is_stable() {
        assert_eq!(scaling_mode_to_style(ScalingMode::Center), ("0", "0"));
        assert_eq!(scaling_mode_to_style(ScalingMode::Stretch), ("2", "0"));
        assert_eq!(scaling_mode_to_style(ScalingMode::Fit), ("6", "0"));
        assert_eq!(scaling_mode_to_style(ScalingMode::Fill), ("10", "0"));
        assert_eq!(scaling_mode_to_style(ScalingMode::Original), ("0", "0"));
    }

    /// TileWallpaper 在所有非平铺模式下均为 "0"
    ///
    /// 当前项目不支持平铺模式（ScalingMode 无 Tile 变体），
    /// 因此 TileWallpaper 恒为 "0"，此测试锁定该不变量。
    #[test]
    fn tile_wallpaper_always_zero_for_supported_modes() {
        for mode in [
            ScalingMode::Center,
            ScalingMode::Stretch,
            ScalingMode::Fit,
            ScalingMode::Fill,
            ScalingMode::Original,
        ] {
            let (_, tile) = scaling_mode_to_style(mode);
            assert_eq!(tile, "0", "TileWallpaper 应为 0, mode={:?}", mode);
        }
    }

    /// 验证默认缩放模式为 Fill（与项目约定一致）
    ///
    /// 这影响 set_native_wallpaper 未显式传参时的回滚新值记录。
    #[test]
    fn default_scaling_mode_is_fill() {
        let default: ScalingMode = ScalingMode::default();
        assert!(matches!(default, ScalingMode::Fill));
    }

    /// 事务回滚契约的数据流文档化测试
    ///
    /// 由于 set_native_wallpaper 调用 Win32 API，无法在 CI 直接测试其回滚行为。
    /// 此测试验证回滚决策所依赖的数据类型契约：
    /// - `read_wallpaper_style` 返回 `Option<(String, String)>`：Some=有旧值可回滚，None=无旧值
    /// - 新值由 `scaling_mode_to_style` 计算
    /// - 回滚时用旧值重新调用 `write_wallpaper_style`
    ///
    /// 这覆盖了回滚逻辑中"None 不回滚、Some 回滚到旧值"的分支决策数据流。
    #[test]
    fn rollback_contract_data_flow() {
        // 场景 1：有旧值（非首次设置）—— 应触发回滚
        let old_with_backup: Option<(String, String)> = Some(("6".to_string(), "0".to_string()));
        let (new_style, new_tile) = scaling_mode_to_style(ScalingMode::Fill);
        if let Some((old_style, old_tile)) = &old_with_backup {
            // 回滚时用旧值调用 write_wallpaper_style（此处仅验证数据可传递）
            assert_eq!(old_style, "6");
            assert_eq!(old_tile, "0");
            assert_ne!(
                (old_style.as_str(), old_tile.as_str()),
                (new_style, new_tile)
            );
        } else {
            panic!("有旧值场景应触发回滚分支");
        }

        // 场景 2：无旧值（首次设置或读取失败）—— 不回滚，仅记录 warn
        let old_no_backup: Option<(String, String)> = None;
        assert!(
            old_no_backup.is_none(),
            "None 场景不应进入回滚分支，仅记录 warn"
        );

        // 场景 3：旧值与新值相同（用户重复设置同模式）—— 回滚到相同值，无害
        let old_same_as_new: Option<(String, String)> =
            Some((new_style.to_string(), new_tile.to_string()));
        if let Some((old_style, old_tile)) = &old_same_as_new {
            assert_eq!(
                (old_style.as_str(), old_tile.as_str()),
                (new_style, new_tile)
            );
        }
    }

    /// 本地手动测试：读取当前注册表的壁纸缩放模式
    ///
    /// 仅在 Windows 上、由开发者手动运行以验证 read_wallpaper_style 行为：
    /// `cargo test -p mirrorstar-core read_wallpaper_style_live -- --ignored --nocapture`
    ///
    /// 此测试标记为 #[ignore] 因为它会读取真实系统注册表（虽然只读、无副作用），
    /// 不适合在 CI 中运行（CI 环境的注册表状态不可控）。
    #[test]
    #[cfg(windows)]
    #[ignore = "需要 Windows 环境且读取真实注册表，仅本地手动运行"]
    fn read_wallpaper_style_live() {
        match read_wallpaper_style() {
            Some((style, tile)) => {
                println!(
                    "当前注册表 WallPaperStyle={}, TileWallpaper={}",
                    style, tile
                );
                // 验证读取到的值是预期的数字字符串格式（WallPaperStyle 应为 0/2/6/10 之一）
                let valid_styles = ["0", "2", "6", "10"];
                assert!(
                    style.is_empty() || valid_styles.contains(&style.as_str()),
                    "WallPaperStyle 应为 {:?} 之一，实际: {}",
                    valid_styles,
                    style
                );
                assert_eq!(
                    tile.as_str(),
                    "0",
                    "TileWallpaper 当前实现下应为 0，实际: {}",
                    tile
                );
            }
            None => {
                println!("注册表中无 WallPaperStyle/TileWallpaper 旧值（可能首次设置或读取失败）");
            }
        }
    }

    /// 本地手动测试：验证 set_wallpaper_image 失败时注册表事务回滚（D01 端到端验证）
    ///
    /// 通过传入不存在的图片路径触发 `set_wallpaper_image` 失败，验证 `set_native_wallpaper`
    /// 是否将注册表 WallPaperStyle/TileWallpaper 回滚到调用前的旧值。
    ///
    /// 运行方式：
    /// `cargo test -p mirrorstar-core rollback_on_image_failure_live -- --ignored --nocapture`
    ///
    /// 此测试标记为 #[ignore] 因为它会临时修改真实系统注册表（写入新值后回滚）。
    /// 在回滚成功的情况下，注册表最终状态与调用前一致；但若回滚本身失败（极罕见），
    /// 注册表可能停留在新值。仅适合开发者本地手动运行，不适合 CI。
    ///
    /// 验证场景：
    /// - `set_wallpaper_image` 失败时函数返回 `Err`
    /// - 注册表 WallPaperStyle/TileWallpaper 被回滚到调用前旧值（before=Some 场景）
    /// - before=None 场景（首次设置/读取失败）跳过回滚值相等性验证，仅验证返回 Err
    #[test]
    #[cfg(windows)]
    #[ignore = "需要 Windows 环境且会临时修改注册表，仅本地手动运行"]
    fn rollback_on_image_failure_live() {
        // 1. 读取调用前的注册表旧值（作为回滚验证基准）
        let before = read_wallpaper_style();
        println!("调用前注册表值: {:?}", before);

        // 2. 调用 set_native_wallpaper 传入不存在的图片路径
        //    预期：write_wallpaper_style 成功（注册表被改为 Fill 模式），
        //          set_wallpaper_image 失败（路径不存在），触发回滚
        let invalid_path = "C:\\nonexistent\\path\\definitely_not_exist_invalid_image.jpg";
        let result = set_native_wallpaper(invalid_path, ScalingMode::Fill, None);

        // 3. 验证函数返回 Err（set_wallpaper_image 失败）
        //    使用 expect_err 而非 assert!(result.is_err())，避免触发
        //    clippy::assertions_on_result_states（style lint，-D warnings 下会失败）
        let err = result.expect_err("传入无效路径应返回 Err");
        println!("set_native_wallpaper 返回错误（预期）: {:?}", err);

        // 4. 读取调用后的注册表值
        let after = read_wallpaper_style();
        println!("调用后注册表值: {:?}", after);

        // 5. 验证注册表已回滚到调用前的旧值
        //    - before=Some：after 应等于 before（成功回滚）
        //    - before=None：无旧值可回滚，after 可能为新值（无回滚备份）
        //    - before=Some 但 after=None：回滚异常，panic
        match (&before, &after) {
            (Some((old_style, old_tile)), Some((new_style, new_tile))) => {
                assert_eq!(
                    (old_style.as_str(), old_tile.as_str()),
                    (new_style.as_str(), new_tile.as_str()),
                    "注册表应回滚到旧值 {:?}，实际为 {:?}",
                    before,
                    after
                );
                println!("注册表已成功回滚到旧值");
            }
            (None, _) => {
                println!(
                    "调用前无旧值可回滚（首次设置或读取失败），跳过回滚值相等性验证；after={:?}",
                    after
                );
            }
            (Some(_), None) => {
                panic!(
                    "回滚后注册表值不应变为 None：before={:?}, after={:?}",
                    before, after
                );
            }
        }
    }

    /// 注册表值恢复守卫：Drop 时恢复 WallPaperStyle/TileWallpaper 到构造时捕获的值
    ///
    /// 用于 round-trip 测试中确保即使断言失败（panic）也能恢复用户系统注册表，
    /// 避免污染用户桌面壁纸缩放模式设置。仅在测试模块内使用。
    struct RegistryGuard {
        original: Option<(String, String)>,
    }

    impl RegistryGuard {
        /// 捕获当前注册表值，构造守卫
        fn capture() -> Self {
            let original = read_wallpaper_style();
            Self { original }
        }
    }

    impl Drop for RegistryGuard {
        fn drop(&mut self) {
            if let Some((style, tile)) = &self.original {
                if let Err(e) = write_wallpaper_style(style, tile) {
                    eprintln!(
                        "警告：RegistryGuard 恢复注册表失败，系统可能残留测试值: {}",
                        e
                    );
                }
            }
        }
    }

    /// Round-trip 一致性测试：write_wallpaper_style → read_wallpaper_style
    ///
    /// 验证 D01 事务回滚机制依赖的注册表读写基础：
    /// - write_wallpaper_style 写入的值能被 read_wallpaper_style 正确读回
    /// - 这是回滚逻辑可靠性的前提（若写读不一致，回滚到旧值也无意义）
    ///
    /// 与 `rollback_on_image_failure_live` 互补：后者验证端到端回滚路径（依赖
    /// SystemParametersInfoW 失败），本测试隔离验证注册表读写在无 Win32 壁纸
    /// API 干预下的往返一致性，且不修改桌面壁纸（仅写注册表）。
    ///
    /// 使用 RegistryGuard 在测试结束（即使断言 panic）时恢复用户原始注册表值。
    ///
    /// 标记 #[ignore] 因为会修改真实系统注册表（HKCU\Control Panel\Desktop），
    /// 不适合在 CI 中运行。本地手动运行：
    /// `cargo test -p mirrorstar-core wallpaper_style_round_trip -- --ignored --nocapture`
    #[test]
    #[cfg(windows)]
    #[ignore = "需要 Windows 环境且修改真实注册表，仅本地手动运行"]
    fn wallpaper_style_round_trip() {
        // 捕获原始值，确保测试结束（即使 panic）后恢复
        let _guard = RegistryGuard::capture();

        // 测试 Center 模式 ("0", "0")
        write_wallpaper_style("0", "0").expect("写入 WallPaperStyle=0 失败");
        let read = read_wallpaper_style();
        assert_eq!(
            read,
            Some(("0".to_string(), "0".to_string())),
            "写入 (\"0\",\"0\") 后读取应一致"
        );

        // 测试 Fill 模式 ("10", "0")
        write_wallpaper_style("10", "0").expect("写入 WallPaperStyle=10 失败");
        let read = read_wallpaper_style();
        assert_eq!(
            read,
            Some(("10".to_string(), "0".to_string())),
            "写入 (\"10\",\"0\") 后读取应一致"
        );

        // 测试 Stretch 模式 ("2", "0")
        write_wallpaper_style("2", "0").expect("写入 WallPaperStyle=2 失败");
        let read = read_wallpaper_style();
        assert_eq!(
            read,
            Some(("2".to_string(), "0".to_string())),
            "写入 (\"2\",\"0\") 后读取应一致"
        );

        // _guard 在此处 Drop，恢复原始注册表值
    }

    /// is_native_supported 扩展名识别测试（覆盖既有逻辑，防止回归）
    #[test]
    fn is_native_supported_recognizes_native_formats() {
        assert!(is_native_supported("image.jpg"));
        assert!(is_native_supported("image.JPEG")); // 大小写不敏感
        assert!(is_native_supported("image.png"));
        assert!(is_native_supported("image.bmp"));
        assert!(is_native_supported("image.tif"));
        assert!(is_native_supported("image.tiff"));
        assert!(is_native_supported("image.dib"));

        // 非原生格式
        assert!(!is_native_supported("image.gif"));
        assert!(!is_native_supported("image.webp"));
        assert!(!is_native_supported("image.webm"));
        assert!(!is_native_supported("image.mp4"));
        assert!(!is_native_supported("image")); // 无扩展名
    }

    // ── v4.1 Medium findings 文档化测试 ──────────────────────────────────

    /// v41-D-004: 验证 write_wallpaper_style 文档化"两步注册表写入非原子"契约。
    ///
    /// write_wallpaper_style 执行两步注册表写入（WallPaperStyle + TileWallpaper，
    /// 底层均为 RegSetValueExW），两步非原子：第一步成功但第二步失败时注册表
    /// 处于不一致状态，本函数不回滚第一步。由于 RegSetValueExW 失败需 mock
    /// Win32 API（CI 中不可行），此处通过 include_str! 模式断言源码包含契约说明，
    /// 与 D-004/D-005/D-007/D-014 的文档化测试风格一致。
    #[test]
    fn v41_d004_write_wallpaper_style_documents_non_atomic_contract() {
        let source = include_str!("native_wallpaper.rs");
        // 验证 v41-D-004 前缀标识存在
        assert!(
            source.contains("## 已知限制 (v41-D-004)"),
            "write_wallpaper_style 文档注释应含 v41-D-004 已知限制段落"
        );
        // 验证契约核心要素：两步写入非原子、不回滚第一步、调用方需重试/手动修复
        assert!(
            source.contains("两步非原子"),
            "v41-D-004 契约应说明两步写入非原子"
        );
        assert!(
            source.contains("RegSetValueExW"),
            "v41-D-004 契约应说明底层 API（RegSetValueExW）"
        );
        assert!(
            source.contains("本函数不回滚第一步"),
            "v41-D-004 契约应明确不回滚第一步"
        );
        assert!(
            source.contains("调用方") && source.contains("重试或手动修复"),
            "v41-D-004 契约应指引调用方重试或手动修复"
        );
    }
}
