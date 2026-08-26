//! `tauri.conf.json` CSP 配置回归测试
//!
//! 用途：防止 `app.security.csp` 与前端 `convertFileSrc(path, "wpfile")` 生成的
//! wpfile URL scheme 再次失配（回归保护）。
//!
//! 背景：前端 `convertFileSrc(path, "wpfile")` 在 Windows 生成
//! `http://wpfile.localhost/<path>`（见 `src-tauri/src/lib.rs` 中 wpfile 协议
//! 注册处的注释）。生产模式下 Tauri 注入 `tauri.conf.json` 的 `app.security.csp`，
//! 该 CSP 的 `img-src` / `media-src` / `frame-src` 必须同时放行：
//!   - `http://wpfile.localhost`  —— Windows 上 convertFileSrc 实际生成的 URL 主机
//!   - `https://wpfile.localhost` —— http 被 WebView2 升级为 https 时的变体
//!   - `wpfile:`                  —— scheme 直连形式
//!
//! 任一来源缺失都会导致生产模式缩略图/预览被 CSP 拦截。
//!
//! 注意：本测试只读 `tauri.conf.json`，不会修改它；也不依赖 cargo test 的 CWD
//! （使用 `CARGO_MANIFEST_DIR` 定位 src-tauri 目录）。

use serde_json::Value;
use std::collections::HashMap;

/// 将 CSP 字符串解析为「指令名 → 来源列表」映射。
///
/// - 按 `;` 拆分指令
/// - 每条指令按首个空白切分出指令名与剩余来源串
/// - 来源串再按空白分隔为 `Vec<String>`
/// - 仅有指令名、无来源的指令解析为空列表
fn parse_directive_sources(csp: &str) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    for directive in csp.split(';') {
        let directive = directive.trim();
        if directive.is_empty() {
            continue;
        }
        if let Some((name, rest)) = directive.split_once(char::is_whitespace) {
            let sources = rest
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            map.insert(name.trim().to_string(), sources);
        } else {
            map.insert(directive.to_string(), Vec::new());
        }
    }
    map
}

/// 从 `tauri.conf.json` 读取 `app.security.csp` 字符串。
///
/// cargo test 的 CWD 不可靠，因此通过 `CARGO_MANIFEST_DIR`（cargo 注入的
/// 包清单目录，即 src-tauri）拼接出 `tauri.conf.json` 的绝对路径。
fn read_csp_from_tauri_conf() -> String {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("环境变量 CARGO_MANIFEST_DIR 未设置（应由 cargo 注入）");
    let conf_path = std::path::Path::new(&manifest_dir).join("tauri.conf.json");

    let raw = std::fs::read_to_string(&conf_path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", conf_path.display(), e));

    let v: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("解析 {} 失败: {}", conf_path.display(), e));

    v.get("app")
        .and_then(|app| app.get("security"))
        .and_then(|sec| sec.get("csp"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "{} 缺少 app.security.csp 字符串字段（当前为对象或缺失）",
                conf_path.display()
            )
        })
}

/// 回归测试：`img-src` / `media-src` / `frame-src` 必须同时放行
/// `http://wpfile.localhost`、`https://wpfile.localhost`、`wpfile:`。
///
/// 断言失败时会指出缺失来源与所属指令，便于定位 tauri.conf.json 中需要补全的位置。
#[test]
fn test_csp_allows_wpfile_url_schemes() {
    let csp = read_csp_from_tauri_conf();
    let directives = parse_directive_sources(&csp);

    const REQUIRED_DIRECTIVES: [&str; 3] = ["img-src", "media-src", "frame-src"];
    const REQUIRED_SOURCES: [&str; 3] = [
        "http://wpfile.localhost",
        "https://wpfile.localhost",
        "wpfile:",
    ];

    for dir_name in REQUIRED_DIRECTIVES {
        let sources = directives.get(dir_name).unwrap_or_else(|| {
            panic!(
                "CSP 缺少指令 `{dir_name}`；当前已解析指令: {:?}",
                directives.keys()
            )
        });

        for src in REQUIRED_SOURCES {
            assert!(
                sources.iter().any(|s| s == src),
                "CSP 指令 `{dir_name}` 缺少来源 `{src}`（当前来源: {:?}）。\
                 前端 convertFileSrc(path, \"wpfile\") 在 Windows 生成 \
                 http://wpfile.localhost/<path>，该来源缺失会导致生产模式 \
                 缩略图/预览被 CSP 拦截。",
                sources
            );
        }
    }
}

/// 单元级断言：验证 `parse_directive_sources` 解析逻辑本身正确。
///
/// 喂一段假 CSP，断言：
/// - `img-src` 的来源列表含预期各项；
/// - 指令名不会混入来源列表；
/// - 未声明的来源不会被误解析进去。
#[test]
fn test_parse_directive_sources_parses_fake_csp() {
    let fake = "default-src 'self'; img-src 'self' wpfile: http://wpfile.localhost https://wpfile.localhost; media-src 'self' wpfile:; frame-src 'self'";

    let map = parse_directive_sources(fake);

    // img-src：应解析出全部预期来源
    let img = map.get("img-src").expect("应解析出 img-src 指令");
    for expected in [
        "'self'",
        "wpfile:",
        "http://wpfile.localhost",
        "https://wpfile.localhost",
    ] {
        assert!(
            img.contains(&expected.to_string()),
            "img-src 来源列表应包含 `{expected}`，实际: {:?}",
            img
        );
    }
    // 指令名不应混入来源列表
    assert!(
        !img.contains(&"img-src".to_string()),
        "指令名 img-src 不应出现在来源列表中，实际: {:?}",
        img
    );

    // media-src：仅 'self' 与 wpfile:，不应包含 http/https 主机
    let media = map.get("media-src").expect("应解析出 media-src 指令");
    assert!(media.contains(&"wpfile:".to_string()));
    assert!(
        !media.contains(&"http://wpfile.localhost".to_string()),
        "media-src 不应包含未声明的 http 来源"
    );

    // frame-src：仅 'self'，不应包含 wpfile:
    let frame = map.get("frame-src").expect("应解析出 frame-src 指令");
    assert!(
        !frame.contains(&"wpfile:".to_string()),
        "frame-src 不应包含未声明的 wpfile: 来源"
    );
}
