//! Tauri 命令模块
//!
//! # 命令注册风格
//!
//! 本 crate 的 Tauri 命令注册遵循以下约定：
//!
//! ## 模块组织
//!
//! 命令按业务子系统拆分为三个子模块，每个子模块对应一个前端功能域：
//!
//! - `config`：配置管理命令（`get_config` / `update_config`）
//! - `system`：系统级命令（`get_displays` / `check_desktop_status` /
//!   `open_file_dialog` / `toggle_auto_start` / `get_auto_start_status`）
//! - `wallpaper`：壁纸生命周期命令（`get_wallpapers` / `add_wallpaper` /
//!   `regenerate_thumbnails` / `remove_wallpaper` / `set_wallpaper` /
//!   `pause_wallpaper` / `resume_wallpaper` / `set_volume` / `toggle_mute` /
//!   `set_interaction_mode` / `get_wallpaper_state` / `set_scaling_mode` /
//!   `set_speed`）
//!
//! 每个子模块通过 `pub use <module>::*;` 在 `commands` 模块根重导出全部公开项，
//! 使 `lib.rs` 可直接通过 `commands::set_wallpaper` 引用，无需写完整路径
//! `commands::wallpaper::set_wallpaper`。
//!
//! ## 集中注册
//!
//! 所有 `#[tauri::command]` 函数集中在 `lib.rs` 的 `run()` 函数中通过
//! `tauri::generate_handler![...]` 一次性注册到 Tauri Builder：
//!
//! ```rust,ignore
//! .invoke_handler(tauri::generate_handler![
//!     get_wallpapers,
//!     add_wallpaper,
//!     regenerate_thumbnails,
//!     remove_wallpaper,
//!     set_wallpaper,
//!     pause_wallpaper,
//!     resume_wallpaper,
//!     get_config,
//!     update_config,
//!     set_volume,
//!     toggle_mute,
//!     set_interaction_mode,
//!     get_displays,
//!     get_wallpaper_state,
//!     open_file_dialog,
//!     toggle_auto_start,
//!     get_auto_start_status,
//!     set_scaling_mode,
//!     set_speed,
//!     check_desktop_status,
//! ])
//! ```
//!
//! 新增命令时必须同时：
//! 1. 在对应子模块中实现 `#[tauri::command]` 函数；
//! 2. 在此处的 `tauri::generate_handler!` 列表中追加命令名，否则前端
//!    `invoke()` 调用会因找不到命令而失败（Tauri v2 不报编译错误，仅在
//!    运行时返回 reject）。
//!
//! ## 命名风格
//!
//! - **命令名**：Rust 函数名使用 `snake_case`，Tauri v2 默认以 Rust 函数名
//!   （`snake_case`）注册命令，前端 `invoke` 使用相同的 `snake_case`
//!   （如 `set_wallpaper` → 前端 `invoke('set_wallpaper')`）。如需在前端
//!   使用 `camelCase`，需在 `#[tauri::command(rename_all = "camelCase")]`
//!   显式指定，本 crate 不使用此选项。
//! - **参数名**：Rust 端 `snake_case`，前端 invoke 时使用 `camelCase`，Tauri
//!   自动将前端 `camelCase` 反序列化为后端 `snake_case` 形参（如 Rust
//!   `file_path: String` 对应前端 `{ filePath: '...' }`）。
//! - **`display_id` 参数**：标识目标显示器，一律为命令首位参数，类型
//!   `Option<String>`，传 `None` / 空串时由 `resolve_display_id` 回退到
//!   首个活跃壁纸所在的显示器。
//!
//! ## 命令签名约定
//!
//! - **`State<'_, AppState>` 参数**：需要访问 `config_manager` /
//!   `wallpaper_engine` / `desktop` 的命令通过 `state: State<'_, AppState>`
//!   注入。`State` 守卫在命令返回时自动释放，无需手动管理。
//! - **`AppHandle` 参数**：需要 emit 事件 / 创建窗口等 Tauri API 的命令
//!   通过 `app: tauri::AppHandle` 注入。
//! - **`async` 命令**：涉及 `.await`（如 tokio fs、engine 锁 `.lock().await`）
//!   的命令使用 `async fn`，Tauri 自动在异步运行时上调度。同步命令（仅读
//!   原子变量 / 持有同步锁）使用 `fn`，避免不必要的 async 开销。
//! - **返回类型**：统一使用 `Result<T, mirrorstar_core::MirrorStarError>`，
//!   `MirrorStarError` 实现了 `Serialize`，前端 invoke 失败时收到结构化
//!   错误对象（含 code/message），便于按错误类型分支处理。
//! - **布尔属性 toggle/set 配对**：布尔型属性的切换/设置统一用 `toggle_xxx`
//!   （无参，返回切换后的新状态）与 `set_xxx`（传 `bool`，显式设置）配对。
//!   本 crate 命名存在差异：`toggle_mute` 因频繁切换用 toggle，而
//!   `set_interaction_mode` 因状态语义明确用 set。新增布尔属性命令时按
//!   使用频率选择：高频切换用 toggle，状态明确用 set。
//! - **作用对象为当前活跃壁纸的命令**：`set_volume` / `set_speed` 作用于
//!   `display_id` 上当前活跃壁纸，`display_id` 为 `None` / 空串时由
//!   `resolve_display_id` 回退到首个活跃壁纸。

pub mod config;
pub mod system;
pub mod wallpaper;

pub use config::*;
pub use system::*;
pub use wallpaper::*;
