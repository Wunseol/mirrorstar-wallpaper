use std::path::PathBuf;

fn main() {
    slint_build::compile("ui/main.slint").expect("编译 ui/main.slint 失败");

    // 兄弟 crate `mirrorstar-core` 的 `ui_proto` / `config` 定义了 UI 的数据契约
    // （事件、命令、壁纸条目、配置结构）。其源码或清单变化会影响本 crate 的编译产物，
    // 因此必须触发重跑（平台无关，不随 Windows 清单逻辑一起开关）。
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mirrorstar-core");
    println!("cargo:rerun-if-changed={}", core.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        core.join("Cargo.toml").display()
    );

    // comctl32 v6 的 TaskDialogIndirect 需要应用清单声明 side-by-side 依赖，
    // 否则 Windows 会载入系统目录下的 comctl32 v5.82（不含该导出），
    // release 构建在加载期即失败：STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139)。
    //
    // 与 Tauri 壳不同：壳由 `tauri-build` 自动注入等价清单，而 `mirrorstar-ui`
    // 是普通 crate，必须自行用链接参数把清单嵌进 PE（`/MANIFEST:EMBED` +
    // `/MANIFESTINPUT:<file>`）。此写法已由一次性 PoC 验证可在 release 下正常加载。
    #[cfg(windows)]
    {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );

        // 把 `src-tauri/icons/icon.ico` 作为图标资源嵌入 `mirrorstar-ui.exe`：
        // Windows 在窗口与窗口类都未显式持有图标时，会回退取用可执行文件中**最低 id** 的
        // RT_GROUP_ICON 作为「进程默认图标」（任务栏 / 标题栏 / Alt-Tab / 资源管理器）。
        // 这样即使 Slint 1.18 没有窗口图标 API，主窗口也能自动拿到应用图标。
        //
        // `manifest_optional()`：图标是装饰性资源，环境缺少资源编译器（`rc.exe`）时
        // `embed-resource` 返回 `NotAttempted`，此时**不应中断构建**；只有真正编译失败
        // （`Failed`）才 panic。
        //
        // 生成的 `.res` 只含 `app.rc` 里的 ICON 及其派生的 RT_GROUP_ICON / RT_ICON，
        // **不含 RT_MANIFEST**，因此与上面 `/MANIFESTINPUT:` 的清单方案互不冲突。
        //
        // `icon.ico` 位于本包目录之外，Cargo 默认不监视它，故显式对**绝对路径**声明
        // `rerun-if-changed`（否则换图标不重建）。
        let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src-tauri/icons/icon.ico");
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed={}", icon.display());
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("嵌入应用图标资源失败");
    }
}
