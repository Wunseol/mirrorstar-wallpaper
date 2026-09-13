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
//! 必须在本脚本调用 `tauri_build::try_build()` **之前**先产出外设进程
//! `mirrorstar-wp-proc.exe`：`tauri.conf.json` 的 `bundle.resources` 硬编码引用了
//! `../target/release/mirrorstar-wp-proc.exe`，构建期需要它已存在（debug 与 release、
//! `cargo build` 与 `tauri build` 均适用）。若不主动产出，干净环境下 Cargo 与 wp-proc
//! 并行编译可能让主 crate 的构建脚本先运行而报 “resource path doesn't exist”。
//! 注意：Cargo 对 build-dependency 只编译 lib、不会产出 `[[bin]]`，故必须显式构建。

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // 1) 显式构建 wp-proc。使用独立 CARGO_TARGET_DIR，避免内层 cargo 与当前外层
    //    build 争用同一 target 目录的全局锁而彼此 deadlock。
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let isolated_target = env::temp_dir().join("mirrorstar-wp-proc-cargo-target");
    let status = Command::new(&cargo)
        .current_dir(&workspace_root)
        .env("CARGO_TARGET_DIR", &isolated_target)
        .args(["build", "-p", "mirrorstar-wp-proc", "--release"])
        .status()
        .expect("failed to launch cargo build for mirrorstar-wp-proc");
    assert!(status.success(), "cargo build -p mirrorstar-wp-proc --release failed");

    // 2) 把 exe 落入资源路径声明的位置，供 try_build() 读取。
    let built_exe = isolated_target.join("release").join("mirrorstar-wp-proc.exe");
    let dest_dir = workspace_root.join("target").join("release");
    let dest_exe = dest_dir.join("mirrorstar-wp-proc.exe");
    std::fs::create_dir_all(&dest_dir).expect("create target/release");
    std::fs::copy(&built_exe, &dest_exe).expect("copy mirrorstar-wp-proc.exe to target/release");

    // 3) wp-proc 源码变化时，本脚本需重跑，以便 try_build() 重新复制最新 exe。
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("crates/mirrorstar-wp-proc/src").display()
    );

    // 4) 执行 Tauri 构建期工作（会读取上述资源）。
    let windows_attrs =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("manifest.xml"));
    let attrs = tauri_build::Attributes::new().windows_attributes(windows_attrs);
    tauri_build::try_build(attrs).expect("failed to run build script");
}
