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

fn main() {
    let windows_attrs =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("manifest.xml"));
    let attrs = tauri_build::Attributes::new().windows_attributes(windows_attrs);
    tauri_build::try_build(attrs).expect("failed to run build script");
}
