//! Tauri 构建脚本
//!
//! 本文件由 Cargo 在 `cargo build` / `cargo test` / `cargo tauri dev` /
//! `cargo tauri build` 时自动执行（构建脚本约定，无需手动调用）。
//!
//! 作用：
//! - 调用 `tauri_build::try_build()` 执行 Tauri 构建期工作：
//!   1. 解析并校验 `tauri.conf.json`（若存在 `tauri.dev.conf.json` 则深合并），
//!      配置错误时 fail build（编译期发现配置问题，而非运行时）
//!   2. 生成 `src-tauri/gen/schemas/` 下的 JSON Schema 文件
//!      （`desktop-schema.json` / `windows-schema.json` / `capabilities.json` /
//!      `acl-manifests.json`），供 IDE 与权限审计工具读取
//!   3. 生成 `Context` 类型定义（`tauri::generate_context!()` 宏消费），
//!      将 `tauri.conf.json` 中的 bundle/capability/window 配置内嵌到二进制
//! - 通过 `WindowsAttributes::app_manifest()` 嵌入 `manifest.xml`（Windows
//!   应用清单，声明 DPI 感知 / Windows 版本兼容性 / 受 elevate 权限要求）
//!
//! 副作用：
//! - 写入 `OUT_DIR`（Cargo 自动管理的构建产物目录）下的生成文件
//! - 触发 `tauri-codegen` 重生成 `gen/schemas/` 目录
//! - 失败时 panic 导致整个 build 失败（fail-fast，避免生成错误的应用二进制）
//!
//! 详见 https://v2.tauri.app/develop/configuration-files/build-script/
//!
//! 必须在本脚本调用 `tauri_build::try_build()` **之前**先产出 UI 外设进程
//! `mirrorstar-ui.exe`：`tauri.conf.json` 的 `bundle.resources` 硬编码引用了
//! `../target/release/*.exe`，构建期需要它已存在（debug 与 release、`cargo build`
//! 与 `tauri build` 均适用）。若不主动产出，干净环境下 Cargo 与该 crate 并行编译
//! 可能让主 crate 的构建脚本先运行而报 “resource path doesn't exist”。注意：Cargo
//! 对 build-dependency 只编译 lib、不会产出 `[[bin]]`，故必须显式构建。
//!
//! （网页壁纸子进程 wp-proc 已改为「同一 exe、双入口」——即主壳 `mirrorstar-wallpaper.exe`
//! 的 `--wp-proc` 模式，不再产出独立 exe，故此处无需构建。)
//!
//! 外设进程产物需**同时**落到两个 profile 目录（`target/<PROFILE>/` 与 `target/release/`）：
//! - `target/<PROFILE>/<exe>`：dev 模式下主壳运行于 `target/debug/mirrorstar-wallpaper.exe`，
//!   并按「主 exe 同目录」解析 UI 子进程，故 debug 目录必须有最新 exe，否则 dev 一直加载旧 UI；
//! - `target/release/<exe>`：维持 `tauri.conf.json` 的 `bundle.resources` 与
//!   `tauri_build` 资源存在性校验所硬编码引用的路径。

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // 1) 显式构建 UI 外设进程，并落入 tauri.conf.json bundle.resources 声明的路径。
    //    （wp-proc 已并入主壳 exe 的 `--wp-proc` 模式，不再单独构建。）
    // mirrorstar-ui 必须**单独一次 cargo 调用**：它启用 slint 的 `renderer-software`
    // 并关闭默认特性；软件渲染的唯一性依赖"无任何 workspace 成员启用 femtovg"这一不变量，
    // 与其它成员混编会合并特性、破坏软件渲染。
    build_sidecar(
        &cargo,
        &workspace_root,
        "mirrorstar-ui",
        "mirrorstar-ui-cargo-target",
        "mirrorstar-ui.exe",
    );

    // 2) 外设进程源码变化时，本脚本需重跑，以便重新构建并复制最新 exe。
    //    覆盖整个 crate 目录（而非仅 `src/`）：`mirrorstar-ui` 的 Slint 界面在
    //    `crates/mirrorstar-ui/ui/*.slint`，不在 `src/` 下，只监听 `src/` 会漏掉界面改动，
    //    导致 `target/release/mirrorstar-ui.exe` 与新代码不一致（打包出旧 UI）。
    //    另需监听 sidecar 依赖的 `mirrorstar-core`，否则 core 改动同样不触发重拷贝。
    for package in ["mirrorstar-ui", "mirrorstar-core"] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join("crates").join(package).display()
        );
    }

    // 3) 执行 Tauri 构建期工作（会读取上述资源）。
    let windows_attrs =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("manifest.xml"));
    let attrs = tauri_build::Attributes::new().windows_attributes(windows_attrs);
    tauri_build::try_build(attrs).expect("failed to run build script");
}

/// 用**隔离的** `CARGO_TARGET_DIR` 构建某个外设进程，并把产物复制到两个目标目录：
/// - `target/<PROFILE>/<exe>`：dev 模式下主壳运行于 `target/debug/`，按「主 exe 同目录」
///   解析 UI 子进程，故 debug 目录必须持有最新 exe（否则 dev 一直加载旧 UI）；
/// - `target/release/<exe>`：`tauri.conf.json` 的 `bundle.resources` 与 `tauri_build`
///   的资源存在性校验硬编码引用该路径。
///
/// 隔离 target 目录是为了避免内层 cargo 与当前外层 build 争用同一 target 目录的全局锁
/// 而彼此 deadlock（内层构建若写同一 target，会与外层持有的锁互等）。
fn build_sidecar(cargo: &str, workspace_root: &Path, package: &str, isolated_dir: &str, exe: &str) {
    let isolated_target = env::temp_dir().join(isolated_dir);
    let status = Command::new(cargo)
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", &isolated_target)
        .args(["build", "-p", package, "--release"])
        .status()
        .unwrap_or_else(|_| panic!("failed to launch cargo build for {package}"));
    assert!(
        status.success(),
        "cargo build -p {package} --release failed"
    );

    // 隔离 target 恒以 `--release` 构建，产物固定落在 `<isolated>/release/`。
    let built_exe = isolated_target.join("release").join(exe);

    // 双 profile 复制：`PROFILE` 由 Cargo 注入（`debug` 或 `release`）。
    let profile = env::var("PROFILE").unwrap_or_else(|_| "release".into());
    let target_root = workspace_root.join("target");
    let profile_dir = target_root.join(&profile);
    let release_dir = target_root.join("release");
    copy_artifact(&built_exe, &profile_dir, exe);
    // `PROFILE == "release"` 时两个目录实为同一路径，跳过重复复制。
    if profile_dir != release_dir {
        copy_artifact(&built_exe, &release_dir, exe);
    }
}

/// 把已构建的 exe 复制到 `dest_dir`（必要时先 `create_dir_all`），任一失败即 panic（fail-fast）。
fn copy_artifact(built_exe: &Path, dest_dir: &Path, exe: &str) {
    std::fs::create_dir_all(dest_dir)
        .unwrap_or_else(|e| panic!("create {} failed: {e}", dest_dir.display()));
    let dest_exe = dest_dir.join(exe);
    std::fs::copy(built_exe, &dest_exe)
        .unwrap_or_else(|e| panic!("copy {exe} to {} failed: {e}", dest_exe.display()));
}
