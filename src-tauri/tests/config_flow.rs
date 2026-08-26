//! 配置/壁纸库管理集成测试
//!
//! 对应 Task 5.2.2：测试 add_wallpaper → get_wallpapers → remove_wallpaper 流程。
//!
//! Tauri 命令层（`commands/wallpaper.rs`）的壁纸库管理命令是 `ConfigManager` 的薄封装：
//! - `get_wallpapers` 命令 → `config_manager.get_wallpapers()`
//! - `add_wallpaper` 命令 → `detect_wallpaper_type` + `config_manager.add_wallpaper(entry)`
//! - `remove_wallpaper` 命令 → `config_manager.remove_wallpaper(&id)`（+ engine.close_wallpaper_by_path）
//!
//! 因此直接测试 ConfigManager 方法即可覆盖命令背后的逻辑。

mod common;

use common::{create_test_config_manager, make_test_entry};
use mirrorstar_core::config::{detect_wallpaper_type, WallpaperEntry};
use mirrorstar_core::WallpaperType;

// ── SubTask 5.2.2: add → get → remove 流程 ───────────────────────────────────

/// 测试完整的壁纸库管理流程：
/// add_wallpaper（等价）→ get_wallpapers（等价）→ remove_wallpaper（等价）
#[test]
fn test_wallpaper_library_add_get_remove_flow() {
    let (cm, _temp_dir) = create_test_config_manager();

    // 记录初始状态
    let initial_count = cm.get_wallpapers().len();

    // ── add_wallpaper 命令等价 ──
    let entry1 = make_test_entry("test-add-1", "/test/wallpaper1.mp4", WallpaperType::Video);
    let add_result1 = cm.add_wallpaper(entry1);
    assert!(add_result1.is_ok(), "add_wallpaper 应成功");

    let entry2 = make_test_entry("test-add-2", "/test/wallpaper2.gif", WallpaperType::Gif);
    let add_result2 = cm.add_wallpaper(entry2);
    assert!(add_result2.is_ok(), "add_wallpaper 应成功");

    // ── get_wallpapers 命令等价 ──
    let wallpapers = cm.get_wallpapers();
    assert_eq!(wallpapers.len(), initial_count + 2, "应新增 2 个壁纸条目");

    // 验证条目存在
    let found1 = wallpapers.iter().find(|w| w.id == "test-add-1");
    assert!(found1.is_some(), "应找到 test-add-1");
    assert_eq!(found1.unwrap().file_path, "/test/wallpaper1.mp4");
    assert_eq!(found1.unwrap().wallpaper_type, WallpaperType::Video);

    let found2 = wallpapers.iter().find(|w| w.id == "test-add-2");
    assert!(found2.is_some(), "应找到 test-add-2");
    assert_eq!(found2.unwrap().file_path, "/test/wallpaper2.gif");
    assert_eq!(found2.unwrap().wallpaper_type, WallpaperType::Gif);

    // ── remove_wallpaper 命令等价 ──
    let remove_result1 = cm.remove_wallpaper("test-add-1");
    assert!(remove_result1.is_ok(), "remove_wallpaper 应成功");
    assert!(remove_result1.unwrap().is_some(), "应返回被移除的条目");

    let wallpapers_after_remove = cm.get_wallpapers();
    assert_eq!(
        wallpapers_after_remove.len(),
        initial_count + 1,
        "移除后应少 1 个条目"
    );
    assert!(
        wallpapers_after_remove
            .iter()
            .find(|w| w.id == "test-add-1")
            .is_none(),
        "test-add-1 应已移除"
    );

    // 清理：移除第二个测试条目
    let remove_result2 = cm.remove_wallpaper("test-add-2");
    assert!(remove_result2.is_ok());
    assert!(remove_result2.unwrap().is_some());

    let wallpapers_final = cm.get_wallpapers();
    assert_eq!(
        wallpapers_final.len(),
        initial_count,
        "清理后应恢复初始数量"
    );
}

/// 测试 remove_wallpaper 移除不存在的 ID
#[test]
fn test_remove_wallpaper_nonexistent_id() {
    let (cm, _temp_dir) = create_test_config_manager();

    // 移除不存在的 ID 应返回 Ok(None)
    let result = cm.remove_wallpaper("nonexistent-id-12345");
    assert!(result.is_ok(), "remove_wallpaper 对不存在的 ID 应返回 Ok");
    assert!(result.unwrap().is_none(), "应返回 None（未找到）");
}

/// 测试 add_wallpaper 后条目字段正确持久化
#[test]
fn test_add_wallpaper_persists_fields() {
    let (cm, _temp_dir) = create_test_config_manager();

    let entry = WallpaperEntry {
        id: "test-persist-1".to_string(),
        file_path: "/test/persistent.jpg".to_string(),
        wallpaper_type: WallpaperType::Image,
        display_id: Some("monitor_0".to_string()),
        added_at: "1234567890".to_string(),
        thumbnail: "thumb_1.jpg".to_string(),
        file_size: 4096,
        metadata: None,
        normalized_path: String::new(),
    };

    // 添加
    let add_result = cm.add_wallpaper(entry);
    assert!(add_result.is_ok());

    // 验证字段
    let wallpapers = cm.get_wallpapers();
    let found = wallpapers.iter().find(|w| w.id == "test-persist-1");
    assert!(found.is_some(), "应找到刚添加的条目");
    let found = found.unwrap();
    assert_eq!(found.file_path, "/test/persistent.jpg");
    assert_eq!(found.wallpaper_type, WallpaperType::Image);
    assert_eq!(found.display_id, Some("monitor_0".to_string()));
    assert_eq!(found.added_at, "1234567890");
    assert_eq!(found.thumbnail, "thumb_1.jpg");
    assert_eq!(found.file_size, 4096);

    // 清理
    let _ = cm.remove_wallpaper("test-persist-1");
}

/// 测试 detect_wallpaper_type（add_wallpaper 命令内部使用）
#[test]
fn test_detect_wallpaper_type_for_add_wallpaper() {
    // add_wallpaper 命令首先调用 detect_wallpaper_type 判断类型
    // 不支持的类型会返回错误

    // 支持的类型
    assert_eq!(
        detect_wallpaper_type("/test/video.mp4"),
        Some(WallpaperType::Video)
    );
    assert_eq!(
        detect_wallpaper_type("/test/anim.gif"),
        Some(WallpaperType::Gif)
    );
    assert_eq!(
        detect_wallpaper_type("/test/image.jpg"),
        Some(WallpaperType::Image)
    );
    assert_eq!(
        detect_wallpaper_type("/test/page.html"),
        Some(WallpaperType::Web)
    );

    // 不支持的类型 → add_wallpaper 命令会返回错误
    assert_eq!(detect_wallpaper_type("/test/doc.pdf"), None);
    assert_eq!(detect_wallpaper_type("/test/music.mp3"), None);
    assert_eq!(detect_wallpaper_type("/test/noext"), None);
}

/// 测试添加多个壁纸条目并验证列表完整性
#[test]
fn test_add_multiple_wallpapers() {
    let (cm, _temp_dir) = create_test_config_manager();

    let initial_count = cm.get_wallpapers().len();

    // 批量添加
    let test_ids: Vec<&str> = vec!["test-batch-1", "test-batch-2", "test-batch-3"];
    for (i, id) in test_ids.iter().enumerate() {
        let entry = make_test_entry(id, &format!("/test/batch_{}.mp4", i), WallpaperType::Video);
        assert!(cm.add_wallpaper(entry).is_ok(), "添加 {} 应成功", id);
    }

    // 验证全部存在
    let wallpapers = cm.get_wallpapers();
    assert_eq!(wallpapers.len(), initial_count + 3, "应新增 3 个条目");

    for id in &test_ids {
        assert!(wallpapers.iter().any(|w| w.id == *id), "应找到 {}", id);
    }

    // 清理
    for id in &test_ids {
        assert!(cm.remove_wallpaper(id).is_ok());
    }
    assert_eq!(
        cm.get_wallpapers().len(),
        initial_count,
        "清理后应恢复初始数量"
    );
}

/// 测试重复添加相同 ID 的条目（ConfigManager 允许重复 ID，因为不做唯一性校验）
#[test]
fn test_add_duplicate_id_allowed() {
    let (cm, _temp_dir) = create_test_config_manager();

    let initial_count = cm.get_wallpapers().len();

    // 添加第一个
    let entry1 = make_test_entry("test-dup-id", "/test/dup1.mp4", WallpaperType::Video);
    assert!(cm.add_wallpaper(entry1).is_ok());

    // 添加相同 ID 的第二个（ConfigManager 不校验唯一性）
    let entry2 = make_test_entry("test-dup-id", "/test/dup2.mp4", WallpaperType::Video);
    assert!(cm.add_wallpaper(entry2).is_ok());

    // 应有 2 个条目
    let wallpapers = cm.get_wallpapers();
    let count = wallpapers.iter().filter(|w| w.id == "test-dup-id").count();
    assert_eq!(count, 2, "应允许重复 ID");

    // remove_wallpaper 只移除第一个匹配
    let removed = cm.remove_wallpaper("test-dup-id").unwrap();
    assert!(removed.is_some());

    // 清理：移除剩余的
    let _ = cm.remove_wallpaper("test-dup-id");
    let remaining = cm
        .get_wallpapers()
        .iter()
        .filter(|w| w.id == "test-dup-id")
        .count();
    assert_eq!(remaining, 0, "清理后不应有残留");

    // 确保恢复初始数量
    assert_eq!(cm.get_wallpapers().len(), initial_count);
}
