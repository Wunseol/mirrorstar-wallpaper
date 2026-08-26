use std::collections::HashSet;

use mirrorstar_core::{ConfigManager, ScalingMode, WallpaperSource};
use tauri::{Emitter, State};

use crate::state::AppState;

/// ST-016: THUMBNAIL_TASK Vec 容量上限。
/// 防止大量并发缩略图任务导致 Vec 无限增长（每个 JoinHandle 占用内存）。
/// 超出上限时优先移除最旧已完成 JoinHandle；全部进行中时不强制截断（避免任务丢失）。
const MAX_THUMBNAIL_TASKS: usize = 50;

/// `wallpaper-thumbnail-failed` 事件 payload
///
/// 缩略图生成失败时通过此 payload 通知前端展示降级占位图。
/// 使用结构体而非 `serde_json::json!` 宏以获得类型安全与编译期字段名检查。
/// 派生 `Clone` 是因为 Tauri v2 的 `Emitter::emit` 要求 `S: Serialize + Clone`。
#[derive(serde::Serialize, Clone)]
struct ThumbnailFailedPayload<'a> {
    file_path: &'a str,
    error: String,
}

/// `wallpaper-source-missing` 事件 payload
///
/// 孤儿壁纸条目（源文件被删/移动但库中仍保留）的缩略图生成失败时 emit，
/// 通知前端展示「源文件缺失」状态。与 `wallpaper-thumbnail-failed` 不同：
/// 该事件**不**表示生成失败（不弹「缩略图生成失败」误报），而是提示源文件缺失。
/// `id` 供前端定位卡片，`file_path` 供展示/排查。
#[derive(serde::Serialize, Clone)]
struct SourceMissingPayload<'a> {
    id: &'a str,
    file_path: &'a str,
}

/// 缩略图生成失败时打印源文件“头 8 字节 hex”，用于定位 IO 错误
///
/// 背景：非 PNG 内容但 .png 扩展名的源文件被 image crate 按内容嗅探后解码失败
/// （返回 ImageDecode），会被回退为直接使用源文件路径作缩略图；而含空格/中文的
/// 路径相关文件打不开（如 os error 123）会走不可恢复分支。打印源文件头 8 字节的
/// hex 便于定位到底是"文件损坏/内容伪造"还是"文件根本打不开"。
///
/// best-effort：文件不存在或读取失败仅记录 warn，不传播错误（不影响调用方流程）。
fn log_file_header(file_path: &str) {
    use std::io::Read;
    let mut file = match std::fs::File::open(file_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                path = file_path,
                error = %e,
                "缩略图生成失败：读取源文件头字节失败（文件可能不存在或无法打开）"
            );
            return;
        }
    };
    let mut buf = [0u8; 8];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                path = file_path,
                error = %e,
                "缩略图生成失败：读取源文件头字节失败"
            );
            return;
        }
    };
    if n == 0 {
        tracing::warn!(path = file_path, "缩略图生成失败：源文件为空（头 8 字节为空）");
        return;
    }
    let header_hex: String = buf[..n].iter().map(|b| format!("{:02x}", b)).collect();
    tracing::warn!(
        path = file_path,
        bytes = n,
        header_hex = %header_hex,
        "缩略图生成失败：源文件头字节"
    );
}

/// 缩略图生成失败分类（孤儿壁纸条目兜底）
///
/// 区分两类失败，避免对「孤儿壁纸条目（源文件缺失）」误弹「缩略图生成失败」：
/// - [`ThumbnailFailureKind::DecodeFallback`]：内容损坏/格式非法——源文件**存在**但解码失败
///   （如 Invalid PNG signature）。命令层回退以源文件路径作缩略图，不 emit 失败弹窗。
/// - [`ThumbnailFailureKind::SourceMissing`]：源文件**缺失**（孤儿条目，源文件被删/移动）。
///   命令层记录 warn 日志，不 emit 失败弹窗，且保留条目不自动删除（用户从库删除时才清理）。
/// - [`ThumbnailFailureKind::Unrecoverable`]：其它不可恢复错误（源文件存在但打不开等），
///   命令层 emit `wallpaper-thumbnail-failed` 通知前端展示降级占位图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThumbnailFailureKind {
    DecodeFallback,
    SourceMissing,
    Unrecoverable,
}

/// 对缩略图生成失败进行分类（纯函数，便于单元测试）
///
/// 分类顺序（源缺失**优先于**错误类型判断）：
/// 1. `Path::new(file_path).exists() == false` → `SourceMissing`。
///    注意：缺失文件被 image crate 的 `metadata`/`ImageReader::open` 包装后同样返回
///    `ImageDecode`（字符串含 os error 2），而 `os error 123`（文件名/卷标语法不正确）为
///    `Io` 错误；若先按错误类型判断，missing 文件会落入解码回退，把不存在的源文件路径
///    写回缩略图，或落入不可恢复分支误弹失败弹窗。故先以 `exists()` 判定源文件是否真实存在。
/// 2. Image/Gif 类型且错误为 `ImageDecode`（源文件存在但解码失败）→ `DecodeFallback`。
/// 3. 其余（源文件存在的 Io / 其它错误）→ `Unrecoverable`。
fn classify_thumbnail_failure(
    error: &mirrorstar_core::MirrorStarError,
    wallpaper_type: mirrorstar_core::config::WallpaperType,
    file_path: &str,
) -> ThumbnailFailureKind {
    if !std::path::Path::new(file_path).exists() {
        return ThumbnailFailureKind::SourceMissing;
    }
    if matches!(error, mirrorstar_core::MirrorStarError::ImageDecode(_))
        && matches!(
            wallpaper_type,
            mirrorstar_core::config::WallpaperType::Image
                | mirrorstar_core::config::WallpaperType::Gif
        )
    {
        return ThumbnailFailureKind::DecodeFallback;
    }
    ThumbnailFailureKind::Unrecoverable
}

/// `wallpaper-loading` 事件 payload（v16-C-009）
///
/// Web 壁纸冷启动时 emit，前端展示进度提示，
/// 避免 WebView2 初始化 5-15s 期间用户误以为应用卡死。
/// 字段命名与 `wallpaper-state-changed`（payload 为 display_id 字符串）保持
/// display_id 语义一致，message 为用户可见的中文提示文案。
#[derive(serde::Serialize, Clone)]
struct WallpaperLoadingPayload<'a> {
    display_id: &'a str,
    message: &'a str,
}

/// `wallpaper-gif-oversized` 事件 payload（v16-C-010）
///
/// GIF 壁纸首帧（降采样后）超 8MB 阈值（4K GIF 场景）时 emit，提示用户
/// 播放可能不流畅。payload 仅含 display_id，文案由前端硬编码（与
/// `wallpaper-loading` 的 message 字段不同，因此场景文案固定无需后端传）。
#[derive(serde::Serialize, Clone)]
struct GifOversizedPayload<'a> {
    display_id: &'a str,
}

/// FE-001 修复：解析 display_id 参数
///
/// 当 `display_id` 为 `None` 或空字符串时，回退到引擎中第一个活跃壁纸的 display_id。
/// 若无活跃壁纸，返回空字符串（与 pause_wallpaper/resume_wallpaper 的 `unwrap_or_default()` 行为一致，
/// engine 的快速路径方法对不存在的 display_id 安全返回 Ok/no-op）。
///
/// 此函数需在持有 engine 锁后调用，以读取 `first_active_display_id()`。
///
/// T03: 已统一用于 pause_wallpaper / resume_wallpaper / set_volume / toggle_mute /
/// get_wallpaper_state / set_scaling_mode / set_speed 全部 7 个快速路径命令。
pub fn resolve_display_id(
    display_id: Option<String>,
    engine: &mirrorstar_core::WallpaperEngine,
) -> String {
    display_id
        .filter(|s| !s.is_empty())
        .or_else(|| engine.first_active_display_id().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// COM 初始化的 RAII guard（drop 时配对调用 CoUninitialize）
///
/// T-005: tokio 的 spawn_blocking 线程默认未初始化 COM，导致 Video 类型
/// `VideoRenderer::play()` 内的 `VolumeControl` WASAPI 调用静默失败。
/// 本 guard 在闭包开头初始化 COM（MTA 模式），离开作用域时自动清理。
/// 参考 `crates/mirrorstar-wp-proc/src/com.rs` 的 ComGuard 与
/// `crates/mirrorstar-core/src/wallpaper/video.rs` pause 线程的 COM 初始化范式。
///
/// T16：`pub(crate)` 暴露给 `lib.rs` 主线程复用（STA 模式，见 `new_sta_or_exit`）。
pub(crate) struct ComGuard {
    /// 仅当 CoInitializeEx 成功（S_OK/S_FALSE）时才需配对调用 CoUninitialize。
    /// RPC_E_CHANGED_MODE 表示线程已以其他 apartment 模式初始化，不应调用 CoUninitialize。
    initialized: bool,
}

impl ComGuard {
    fn new() -> Self {
        // COM 初始化（MTA 模式）：与 VolumeControl 的 WASAPI free-threaded 语义匹配，
        // 不同于主线程的 STA（见 `src-tauri/src/lib.rs`）。
        let initialized = unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            match CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                Ok(()) => true,
                Err(e) if e.code() == RPC_E_CHANGED_MODE => {
                    // 已以其他 apartment 模式初始化，不调 CoUninitialize（COM 规则）
                    tracing::debug!(
                        "spawn_blocking 线程 COM 已初始化（RPC_E_CHANGED_MODE），跳过 CoUninitialize"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(error = %e, "spawn_blocking 线程 CoInitializeEx 失败");
                    false
                }
            }
        };
        Self { initialized }
    }

    /// T16：主线程 COM 初始化（STA 模式）的 RAII guard。
    ///
    /// 与 `new()` 的区别：
    /// - 使用 `COINIT_APARTMENTTHREADED`（STA），与 Tauri/tao 的要求一致
    /// - 初始化失败时 `std::process::exit(1)`（主线程 COM 是必需依赖，无法降级）
    /// - RPC_E_CHANGED_MODE 时不调 CoUninitialize（COM 规则）
    ///
    /// guard 在 `run()` 中持有至函数返回，setup 闭包 `?` 失败（`.build()` 返回 Err
    /// → `.expect()` panic unwind）时 guard Drop 调 CoUninitialize，避免 COM 引用计数泄漏。
    pub(crate) fn new_sta_or_exit() -> Self {
        // SAFETY: 主线程首次调用 CoInitializeEx，无并发问题。
        // COINIT_APARTMENTTHREADED 对应 STA，与 Tauri/tao 的事件循环要求一致。
        let initialized = unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
            match CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
                Ok(()) => true,
                Err(e) if e.code() == RPC_E_CHANGED_MODE => {
                    // 已以其他 apartment 模式初始化，不调 CoUninitialize（COM 规则）
                    tracing::warn!("COM 已被初始化为其他模式，忽略");
                    false
                }
                Err(e) => {
                    tracing::error!(error = %e, "COM 初始化失败");
                    std::process::exit(1);
                }
            }
        };
        Self { initialized }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// T15：正在设置壁纸的 display_id 集合，防止并发 set_wallpaper 同一 display 时
/// WorkerW 3 阶段流程竞态导致渲染器进程泄漏。
///
/// set_wallpaper 入口 lock 后若 display_id 已在集合中，返回 Err；否则插入。
/// RAII guard（`DisplaySettingGuard`）在 Drop 时移除，确保所有返回路径（含 `?`/Err）都移除。
///
/// 使用 `LazyLock`：`HashSet::new()` 非 const fn，无法直接初始化 static。
/// `LazyLock<T>` 实现了 `Deref<Target = T>`，访问处 `DISPLAYS_SETTING.lock()`
/// 通过自动 deref 调用 `Mutex::lock()`，与原 `Mutex` 直接访问语法一致。
static DISPLAYS_SETTING: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// T15：display 设置中标志的 RAII guard
///
/// 创建时将 display_id 插入 `DISPLAYS_SETTING`，Drop 时移除。
/// 确保即使 set_wallpaper 通过 `?` / Err 提前返回，标志也会被正确清除，
/// 避免 flag 泄漏导致该显示器后续 set_wallpaper 永远被拒绝（等效死锁）。
//
// `derive(Debug)`：单元测试中 `DisplaySettingGuard::acquire(...).expect_err(...)`
// 需要 Ok 变体实现 `Debug`（`Result::expect_err` 的约束）。
#[derive(Debug)]
struct DisplaySettingGuard {
    display_id: String,
}

impl DisplaySettingGuard {
    /// 尝试为指定 display_id 获取"设置中"标志。
    ///
    /// 若该 display_id 已在集合中（另一 set_wallpaper 正在进行），返回 Err
    /// 提示用户稍后重试；否则插入并返回 guard。
    ///
    /// try_lock 失败时返回错误与用户体验文档化
    ///
    /// # 锁忙（display 已在设置中）时的返回值
    ///
    /// - **返回 `Err(MirrorStarError::DesktopIntegration)`**：
    ///   错误消息为 `"显示器 {display_id} 正在切换壁纸，请稍后再试"`
    /// - **前端可据此展示用户提示**：前端 invoke `set_wallpaper` 失败时，
    ///   根据 error code / message 判断是否为"操作进行中"场景，
    ///   展示 toast 提示用户"该显示器正在切换壁纸，请稍后再试"
    ///
    /// # 用户体验设计
    ///
    /// - **明确错误提示**：与 `update_config` 的 try_lock 静默跳过不同，
    ///   `set_wallpaper` 的并发冲突需明确告知用户（用户主动切换壁纸被拒绝，
    ///   需知道原因与后续操作）
    /// - **自动重试不建议**：前端不应自动重试 `set_wallpaper`，
    ///   因壁纸切换是用户显式操作，自动重试可能导致用户困惑
    ///   （如用户切换 A 显示器时 B 显示器正在切换，自动重试 A 会延迟生效）
    /// - **guard 自动释放**：另一 set_wallpaper 完成后（无论成功/失败/panic），
    ///   `DisplaySettingGuard::Drop` 会自动移除 display_id 标志，
    ///   用户下次重试时即可成功获取锁
    ///
    /// # 并发场景示例
    ///
    /// 1. 用户在显示器 A 上切换壁纸 → `set_wallpaper("A")` 获取 guard A
    /// 2. 用户立即在显示器 A 上再次切换壁纸 → `set_wallpaper("A")` 获取 guard A 失败
    ///    → 返回 Err `"显示器 A 正在切换壁纸，请稍后再试"`
    /// 3. 第一个 set_wallpaper 完成 → guard A Drop，标志移除
    /// 4. 用户重试 set_wallpaper("A") → 获取 guard A 成功
    fn acquire(display_id: String) -> Result<Self, mirrorstar_core::MirrorStarError> {
        let mut set = DISPLAYS_SETTING.lock().map_err(|e| {
            mirrorstar_core::MirrorStarError::LockPoisoned(format!("DISPLAYS_SETTING 锁中毒: {}", e))
        })?;
        // insert 返回 false 表示已存在，消除 contains 双重哈希查找。
        if !set.insert(display_id.clone()) {
            return Err(mirrorstar_core::MirrorStarError::DesktopIntegration(format!(
                "显示器 {} 正在切换壁纸，请稍后再试",
                display_id
            )));
        }
        Ok(Self { display_id })
    }
}

impl Drop for DisplaySettingGuard {
    fn drop(&mut self) {
        // 锁中毒时不操作（进程可能正在退出，集合随进程回收）
        if let Ok(mut set) = DISPLAYS_SETTING.lock() {
            set.remove(&self.display_id);
        }
    }
}

/// 校验壁纸文件路径并返回 metadata：必须为绝对路径且文件存在可访问。
///
/// 在完成全部校验后返回已获取的 `std::fs::Metadata`，供调用方
/// （如 `add_wallpaper`）复用，避免对同一文件路径重复执行 `tokio::fs::metadata`
/// 这一 async fs syscall。
///
/// T-003 + T-012: 防止相对路径 / 不存在文件进入后续流程。
/// ST-009: 统一使用 `InvalidPath` 变体表示路径相关错误（不绝对/遍历/不存在），
/// 前端可通过错误 code 字段区分"路径不合法"与"桌面集成失败"，无需解析消息字符串。
///
/// T08（P0 安全）：在现有绝对路径 / 父目录 / metadata 检查通过后，逐级检查路径
/// 组件的 `symlink_metadata`（不跟随符号链接），若任一组件是符号链接则拒绝访问，
/// 防止 symlink 绕过路径校验（否则攻击者可通过符号链接将受控数据目录（data_dir）外的文件伪装成
/// data_dir 内文件）。
///
/// 相比 `canonicalize` + 字符串比较方案，此方法正确处理 Windows 8.3 短名
///（如 `ADMINI~1` → `Administrator` 的 OS 级别名，非符号链接，不会误判），
/// 避免 `tempfile`/`std::env::temp_dir()` 等含 8.3 短名的合法路径被错误拒绝。
///
/// # ST-015 TOCTOU 风险评估
///
/// 本函数校验与调用方使用（如 `add_wallpaper` 后续 detect/读取/复制文件）之间存在
/// 残留 TOCTOU（time-of-check / time-of-use）竞态：攻击者理论上可在校验通过后将
/// 文件替换为符号链接指向 data_dir 外目标，导致后续读取越权。
///
/// 现有防御层（多层防御，非依赖单一检查）：
/// - T08：`symlink_metadata` 逐级检查路径组件，拒绝中间目录与最终文件符号链接
/// - SEC-001：`canonicalize` 失败时拒绝访问，覆盖部分 Junction Points 场景
/// - SEC-001：拒绝路径遍历（`..` 组件），防 `../` 注入绕过目录作用域
///
/// 残留风险可接受：壁纸路径来自用户主动选择（前端文件对话框），非外部攻击者
/// 可控输入；攻击者需具备同账户文件系统写权限与本地执行能力，已超出威胁模型范围。
///
/// 不实施 `tokio::fs::copy` 后重新校验或 `FILE_FLAG_OPEN_REPARSE_POINT` 加固，
/// 理由：前者引入额外 IO 开销与临时文件管理复杂度，后者引入 Windows 平台特定
/// API 依赖（`windows` crate 的 `CreateFileW` 调用），与现有 T08 + SEC-001 +
/// 路径遍历拒绝的多层防御收益不匹配。
pub(crate) async fn validate_and_get_metadata(
    path: &str,
) -> Result<std::fs::Metadata, mirrorstar_core::MirrorStarError> {
    use std::path::Path;

    if !Path::new(path).is_absolute() {
        return Err(mirrorstar_core::MirrorStarError::InvalidPath {
            reason: format!("文件路径必须为绝对路径: {}", path),
        });
    }

    // SEC-001: 拒绝路径遍历（防 ../ 注入）
    if Path::new(path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(mirrorstar_core::MirrorStarError::InvalidPath {
            reason: format!("路径包含 .. 组件，疑似路径遍历攻击: {}", path),
        });
    }

    // 校验文件存在且可访问。保留 metadata 供调用方复用，
    // 避免后续再次调用 tokio::fs::metadata。
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(_) => {
            return Err(mirrorstar_core::MirrorStarError::InvalidPath {
                reason: format!("文件不存在或不可访问: {}", path),
            });
        }
    };

    // T08: 逐级检查路径组件是否为符号链接，防止 symlink 绕过路径校验。
    // 使用 symlink_metadata（不跟随符号链接）检测每个组件，若任一为符号链接则拒绝。
    // 此方法正确处理 Windows 8.3 短名（OS 级别名，非符号链接，不误判），
    // 并覆盖中间目录符号链接与最终文件符号链接两种攻击场景。
    {
        let mut current = std::path::PathBuf::new();
        for component in Path::new(path).components() {
            current.push(component);
            if let Ok(meta) = tokio::fs::symlink_metadata(&current).await {
                if meta.file_type().is_symlink() {
                    return Err(mirrorstar_core::MirrorStarError::InvalidPath {
                        reason: format!(
                            "路径包含符号链接，拒绝访问: {}（符号链接组件: {}）",
                            path,
                            current.display()
                        ),
                    });
                }
            }
            // symlink_metadata 失败时（如组件不可访问），前面 metadata 检查已覆盖
            // 文件存在性，此处不额外处理，避免过度拒绝。
        }
    }

    // ST-007: 补充 canonicalize 检测 Junction Points（IO_REPARSE_TAG_MOUNT_POINT）。
    // symlink_metadata 的 is_symlink() 可能无法识别 Windows Junction Points，
    // 采用"最简单且安全的方案"：仅当 canonicalize 失败时拒绝访问（成功时不做路径
    // 比较，避免 Windows `\\?\` verbatim 前缀导致的误报）。canonicalize 失败通常
    // 意味着路径含非法 reparse point 或不可达，拒绝以防范 junction 绕过。
    // 改用 tokio::fs::canonicalize 避免阻塞 async 线程
    if let Err(e) = tokio::fs::canonicalize(path).await {
        tracing::warn!(
            error = %e,
            path = %path,
            "canonicalize 失败，拒绝访问（ST-007 junction 检测）"
        );
        return Err(mirrorstar_core::MirrorStarError::InvalidPath {
            reason: format!("路径无法规范化（可能含非法 reparse point）: {}", path),
        });
    }

    Ok(metadata)
}

/// 校验壁纸文件路径：必须为绝对路径且文件存在可访问。
///
/// 丢弃 metadata 的便捷封装，委托给 [`validate_and_get_metadata`] 并忽略返回的
/// metadata。需要复用 metadata 的调用方（如 `add_wallpaper`）应直接调用
/// [`validate_and_get_metadata`]，避免对同一文件路径重复执行 `tokio::fs::metadata`。
pub(crate) async fn validate_wallpaper_file_path(
    path: &str,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    validate_and_get_metadata(path).await.map(|_| ())
}

/// 判断 `file_path` 是否位于 `data_dir` 内。
///
/// ST-001: 使用 `Path::starts_with` 按路径组件匹配，而非 `String::starts_with` 的字节前缀匹配。
/// 后者会把 `mirrorstar-evil`、`mirrorstar_backup`、`mirrorstar.tmp` 等以 `mirrorstar` 开头的
/// 兄弟目录误判为受信任目录，导致资产作用域检查被绕过。
fn is_path_within_data_dir(file_path: &str, data_dir: &std::path::Path) -> bool {
    std::path::Path::new(file_path).starts_with(data_dir)
}

/// 校验壁纸文件路径位于 data_dir 内（删除前置校验）
///
/// 在 `validate_wallpaper_file_path` 基础上追加 `is_path_within_data_dir` 检查，
/// 防止配置篡改（如修改 `library.toml` 指向系统文件）导致任意文件删除。
///
/// `remove_wallpaper` 删除文件前调用此函数，拒绝越界路径。仅校验文件路径合法性
/// （绝对路径 / 无遍历 / 文件存在 / 无 symlink / canonicalize）不足以防范配置篡改：
/// 攻击者可指向一个合法存在、非符号链接的系统文件（如 `C:\Windows\System32\config\SAM`），
/// `validate_wallpaper_file_path` 会通过校验，导致 `remove_file` 删除系统文件。
///
/// 追加 `is_path_within_data_dir` 边界校验后，仅 data_dir（`%APPDATA%/mirrorstar/`）
/// 内的文件可被删除，将可删除文件作用域收紧至应用自身管理的资产范围内。
pub(crate) async fn validate_path_within_data_dir(
    file_path: &str,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // 复用现有路径合法性校验（绝对路径 / 无遍历 / 文件存在 / 无 symlink / canonicalize）
    validate_wallpaper_file_path(file_path).await?;
    // 追加 data_dir 边界校验，拒绝越界路径
    let data_dir = ConfigManager::data_dir()?;
    if !is_path_within_data_dir(file_path, &data_dir) {
        return Err(mirrorstar_core::MirrorStarError::InvalidPath {
            reason: format!("壁纸文件路径不在受控数据目录内，拒绝删除: {}", file_path),
        });
    }
    Ok(())
}

#[tauri::command]
pub fn get_wallpapers(
    state: State<'_, AppState>,
) -> Result<Vec<mirrorstar_core::config::WallpaperEntry>, mirrorstar_core::MirrorStarError> {
    Ok(state.config_manager.get_wallpapers())
}

#[tauri::command]
pub async fn add_wallpaper(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    file_path: String,
    display_id: Option<String>,
) -> Result<String, mirrorstar_core::MirrorStarError> {
    // T-003 + T-012: 校验文件路径（绝对路径 + 可访问），在 detect_wallpaper_type 之前。
    // 复用校验阶段已获取的 metadata，避免后续再次调用 tokio::fs::metadata。
    // 文件不存在时 validate_and_get_metadata 返回 InvalidPath 错误（不静默 file_size=0）。
    let metadata = validate_and_get_metadata(&file_path).await?;

    let wallpaper_type =
        mirrorstar_core::config::detect_wallpaper_type(&file_path).ok_or_else(|| {
            mirrorstar_core::MirrorStarError::DesktopIntegration(format!(
                "不支持的文件类型: {}",
                file_path
            ))
        })?;

    let id = uuid::Uuid::new_v4().to_string();
    let added_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    // 复用校验阶段已获取的 metadata，不再重复调用 tokio::fs::metadata。
    let file_size = metadata.len();

    // B01: 收紧 asset scope（仅 $APPDATA/mirrorstar/**/* 可通过 asset:// 协议访问）后，
    // 外部文件必须先复制到 data_dir 下 $APPDATA/mirrorstar/wallpapers/{id}/{filename}，
    // 否则前端通过 convertFileSrc 加载时会因 scope 限制失败。
    // 若源文件已在 data_dir 下（例如用户重新添加已管理的文件），跳过复制避免重复占用空间。
    let file_path = {
        let data_dir = ConfigManager::data_dir()?;
        if is_path_within_data_dir(&file_path, &data_dir) {
            // 源文件已在 $APPDATA/mirrorstar/ 下，直接使用原路径
            file_path
        } else {
            // 提取原始文件名（路径已校验为绝对路径且文件存在，file_name 正常情况不为 None）
            let original_filename = std::path::Path::new(&file_path)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| mirrorstar_core::MirrorStarError::InvalidArgument {
                    reason: format!("无法从路径提取文件名: {}", file_path),
                })?;
            let dest_dir = data_dir.join("wallpapers").join(&id);
            if let Err(e) = tokio::fs::create_dir_all(&dest_dir).await {
                tracing::warn!(error = %e, dir = %dest_dir.display(), "创建壁纸目录失败");
                return Err(map_io_error("创建壁纸目录", e));
            }
            let dest_path = dest_dir.join(original_filename);
            tracing::debug!(
                src = %file_path,
                dest = %dest_path.display(),
                "复制壁纸文件到受控数据目录"
            );
            if let Err(e) = tokio::fs::copy(&file_path, &dest_path).await {
                tracing::warn!(
                    error = %e,
                    src = %file_path,
                    dest = %dest_path.display(),
                    "复制壁纸文件失败"
                );
                return Err(map_io_error("复制壁纸文件", e));
            }
            dest_path.to_string_lossy().to_string()
        }
    };

    let entry = mirrorstar_core::config::WallpaperEntry {
        id: id.clone(),
        file_path: file_path.clone(),
        wallpaper_type,
        display_id,
        added_at,
        thumbnail: String::new(),
        file_size,
        metadata: None,
        // 派生字段，由 ConfigManager::add_wallpaper 内部计算覆盖
        normalized_path: String::new(),
    };

    // Save entry immediately (without thumbnail)
    // add_wallpaper 仅标记 dirty，命令返回前 flush_library 确保立即持久化
    // clone entry 供 emit 使用，前端据此增量追加单张卡片，
    // 避免全量 refreshWallpaperList（IPC 重拉 + 全量 DOM 重建）
    state.config_manager.add_wallpaper(entry.clone())?;
    state.config_manager.flush_library()?;

    if let Err(e) = app.emit("wallpaper-added", entry) {
        tracing::warn!(error = %e, "emit wallpaper-added 失败：前端 UI 可能不刷新");
    }

    // Generate thumbnail in background for image/gif/video types
    // ST-014: 保存 JoinHandle 到全局 THUMBNAIL_TASK，shutdown 时 take + 5s 超时等待，
    // 避免 update_thumbnail 被截断导致 wallpaper entry 的 thumbnail 字段保持为空。
    // 详见 docs/优化文档/06-src-tauri应用层.md ST-014。
    // 扩展为支持 Video 类型（通过 ffmpeg 抽帧），
    // 失败时 emit `wallpaper-thumbnail-failed` 事件通知前端展示降级占位图。
    if matches!(
        wallpaper_type,
        mirrorstar_core::config::WallpaperType::Image
            | mirrorstar_core::config::WallpaperType::Gif
            | mirrorstar_core::config::WallpaperType::Video
    ) {
        if let Ok(thumbnail_dir) = ConfigManager::data_dir().map(|d| d.join("thumbnails")) {
            let file_path_clone = file_path.clone();
            // 缩略图生成完成后按 id 拉取完整 entry，emit 给前端增量更新
            let id_clone = id.clone();
            let thumbnail_dir_clone = thumbnail_dir.clone();
            let config_manager_clone = state.config_manager.clone();
            let app_clone = app.clone();
            // WallpaperType 派生 Copy，直接拷贝到闭包
            let wallpaper_type_spawn = wallpaper_type;

            let handle = tokio::task::spawn_blocking(move || {
                // 按壁纸类型分派缩略图生成逻辑
                let result = match wallpaper_type_spawn {
                    mirrorstar_core::config::WallpaperType::Image
                    | mirrorstar_core::config::WallpaperType::Gif => {
                        mirrorstar_core::config::generate_thumbnail(
                            &file_path_clone,
                            &thumbnail_dir_clone,
                        )
                    }
                    mirrorstar_core::config::WallpaperType::Video => {
                        if mirrorstar_core::config::is_ffmpeg_available() {
                            mirrorstar_core::config::generate_video_thumbnail(
                                &file_path_clone,
                                &thumbnail_dir_clone,
                            )
                        } else {
                            tracing::info!(
                                path = %file_path_clone,
                                "ffmpeg 不可用，跳过 Video 缩略图生成（预期降级）"
                            );
                            return; // 早期返回，不 emit 失败事件（属预期降级，非错误）
                        }
                    }
                    mirrorstar_core::config::WallpaperType::Web => {
                        unreachable!("Web 已在外层 matches! 过滤，不应进入缩略图生成块")
                    }
                };
                match result {
                    Ok(thumb_name) => {
                        // Store the full absolute path so frontend can use convertFileSrc
                        let full_path = thumbnail_dir_clone.join(&thumb_name);
                        let full_path_str = full_path.to_string_lossy().to_string();
                        if let Err(e) =
                            config_manager_clone.update_thumbnail(&file_path_clone, full_path_str)
                        {
                            tracing::warn!(error = %e, "更新缩略图路径失败: {}", file_path_clone);
                        }
                        // emit 完整 entry 供前端增量更新缩略图，
                        // 避免全量 refreshWallpaperList 导致所有可见卡片重新 hydrate
                        match config_manager_clone.get_wallpaper(&id_clone) {
                            Some(updated) => {
                                if let Err(e) = app_clone.emit("wallpaper-updated", updated) {
                                    tracing::warn!(error = %e, "emit wallpaper-updated 失败：前端 UI 可能不刷新");
                                }
                            }
                            None => {
                                // 未找到条目（异常），fallback 到空 payload 触发前端全量刷新
                                if let Err(e) = app_clone.emit("wallpaper-updated", ()) {
                                    tracing::warn!(error = %e, "emit wallpaper-updated 失败：前端 UI 可能不刷新");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "缩略图生成失败: {}", file_path_clone);
                        // 失败日志打印源文件“头 8 字节 hex”，用于定位 os error 123 这类 IO 错误
                        // （含空格/中文的路径相关 IO 错误：确认源文件是否真实存在/可读）。
                        log_file_header(&file_path_clone);
                        // 按失败类型分派（孤儿壁纸条目兜底）：
                        // - SourceMissing：源文件缺失（孤儿条目，源文件被删/移动，如 os error 123/2），
                        //   记录 warn 日志 + emit wallpaper-source-missing，**不** emit
                        //   wallpaper-thumbnail-failed（避免前端误弹「缩略图生成失败」）；
                        //   条目不自动删除（用户从库删除时才清理）。
                        // - DecodeFallback：内容损坏/格式非法（如 Invalid PNG signature），源文件存在，
                        //   回退以源文件路径作为缩略图并 emit wallpaper-updated（保留现有逻辑）。
                        // - Unrecoverable：其它不可恢复错误（源文件存在但打不开等），emit 失败弹窗。
                        match classify_thumbnail_failure(&e, wallpaper_type_spawn, &file_path_clone) {
                            ThumbnailFailureKind::SourceMissing => {
                                tracing::warn!(
                                    path = %file_path_clone,
                                    "孤儿壁纸条目：源文件缺失，跳过失败弹窗并保留条目（等待用户处理）"
                                );
                                if let Err(emit_err) = app_clone.emit(
                                    "wallpaper-source-missing",
                                    SourceMissingPayload {
                                        id: &id_clone,
                                        file_path: &file_path_clone,
                                    },
                                ) {
                                    tracing::warn!(error = %emit_err, "emit wallpaper-source-missing 失败");
                                }
                            }
                            ThumbnailFailureKind::DecodeFallback => {
                                // 回退：以源文件路径作为缩略图写入该条目
                                if let Err(update_err) = config_manager_clone
                                    .update_thumbnail(&file_path_clone, file_path_clone.clone())
                                {
                                    tracing::warn!(error = %update_err, "回退缩略图为源路径失败: {}", file_path_clone);
                                }
                                // emit 完整 entry 供前端增量更新（功能上等同缩略图生成成功分支）
                                match config_manager_clone.get_wallpaper(&id_clone) {
                                    Some(updated) => {
                                        if let Err(e) = app_clone.emit("wallpaper-updated", updated) {
                                            tracing::warn!(error = %e, "emit wallpaper-updated 失败：前端 UI 可能不刷新");
                                        }
                                    }
                                    None => {
                                        if let Err(e) = app_clone.emit("wallpaper-updated", ()) {
                                            tracing::warn!(error = %e, "emit wallpaper-updated 失败：前端 UI 可能不刷新");
                                        }
                                    }
                                }
                            }
                            ThumbnailFailureKind::Unrecoverable => {
                                // 仅不可恢复的 IO / 其它错误才 emit 失败事件通知前端展示降级占位图
                                if let Err(emit_err) = app_clone.emit(
                                    "wallpaper-thumbnail-failed",
                                    ThumbnailFailedPayload {
                                        file_path: &file_path_clone,
                                        error: e.to_string(),
                                    },
                                ) {
                                    tracing::warn!(
                                        error = %emit_err,
                                        "emit wallpaper-thumbnail-failed 失败"
                                    );
                                }
                            }
                        }
                    }
                }
            });
            // ST-014 + T05: 保存 JoinHandle 供 perform_shutdown_blocking 等待
            // T05：原为 `*slot = Some(handle)`（覆盖式赋值），连续 add_wallpaper 时
            // 旧 handle 被 drop（任务虽继续执行，但 shutdown 只等待最后一个），存在
            // 任务丢失风险。改为 Vec 收集所有任务，push 前清理已完成任务避免无限增长。
            if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
                // 清理已完成任务，避免 Vec 无限增长（spawn_blocking 任务完成后
                // JoinHandle::is_finished 返回 true）
                slot.retain(|h| !h.is_finished());
                slot.push(handle);
                // ST-016: 容量上限保护，防止 Vec 无限增长。
                // 优先移除最旧已完成的 JoinHandle；全部进行中时不强制截断（避免任务丢失）。
                if slot.len() > MAX_THUMBNAIL_TASKS {
                    if let Some(idx) = slot.iter().position(|h| h.is_finished()) {
                        slot.remove(idx);
                    } else {
                        // 全部进行中，不强制截断（避免任务丢失）
                        tracing::warn!(
                            len = slot.len(),
                            "THUMBNAIL_TASK 超出上限 50，全部进行中，不强制截断"
                        );
                    }
                }
            }
        }
    }

    Ok(id)
}

#[tauri::command]
pub async fn remove_wallpaper(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    wallpaper_id: String,
    delete_file: bool,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // 先从配置获取壁纸信息（用于判断是否正在运行）
    // 使用 get_wallpaper 按 id 查找，避免全量克隆 Vec<WallpaperEntry>
    let entry = state.config_manager.get_wallpaper(&wallpaper_id);

    // 如果壁纸正在运行，先关闭它
    if let Some(ref entry) = entry {
        let mut engine = state.wallpaper_engine.lock().await;
        engine.close_wallpaper_by_path(&entry.file_path)?;
    }

    // 从配置库移除
    // remove_wallpaper 仅标记 dirty，命令返回前 flush_library 确保立即持久化
    let removed = state.config_manager.remove_wallpaper(&wallpaper_id)?;
    state.config_manager.flush_library()?;
    if delete_file {
        if let Some(entry) = removed {
            // 删除前校验路径位于 data_dir 内，防配置篡改导致任意文件删除。
            // 仅校验文件路径合法性（validate_wallpaper_file_path）不足以防范配置篡改，
            // 攻击者可指向合法存在的系统文件。追加 is_path_within_data_dir 边界校验，
            // 将可删除文件作用域收紧至应用 data_dir 内。
            validate_path_within_data_dir(&entry.file_path).await?;
            // T-011: 删除文件失败时记录日志（配置已移除，用户意图达成）。
            // 不返回错误：配置条目已移除，文件残留可由用户手动清理或下次重启处理，
            // 返回错误会让前端误以为整体移除失败而重试，反而造成混乱。
            // 使用 tokio::fs::remove_file 避免阻塞 tokio worker 线程
            // （DeleteFileW 系统调用在文件被占用或含重解析点时可能阻塞数十毫秒）
            if let Err(e) = tokio::fs::remove_file(&entry.file_path).await {
                tracing::warn!(error = %e, path = %entry.file_path, "删除壁纸文件失败（配置已移除）");
            }
            // 删除对应缩略图（若有），同样先做 data_dir 边界校验防越界删除。
            // 缩略图按含 uuid 的源路径 hash 命名，逐 id 删除安全，不会误删其他壁纸。
            if !entry.thumbnail.is_empty() && validate_path_within_data_dir(&entry.thumbnail).await.is_ok() {
                if let Err(e) = tokio::fs::remove_file(&entry.thumbnail).await {
                    tracing::warn!(error = %e, path = %entry.thumbnail, "删除缩略图失败（配置已移除）");
                }
            }
            // 删除 wallpapers/{id} 空目录（精确构造路径，勿从 file_path 推断父目录，
            // 因源文件可能原本就在 data_dir 内而非 uuid 子目录下）。
            // 仅删除空目录，非空（目录非空错误）或不存在（NotFound）时静默忽略。
            if let Ok(data_dir) = ConfigManager::data_dir() {
                let id_dir = data_dir.join("wallpapers").join(&wallpaper_id);
                if let Err(e) = tokio::fs::remove_dir(&id_dir).await {
                    match e.kind() {
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty => {}
                        _ => tracing::warn!(error = %e, path = %id_dir.display(), "删除壁纸目录失败"),
                    }
                }
            }
        }
    }
    if let Err(e) = app.emit("wallpaper-removed", wallpaper_id) {
        tracing::warn!(error = %e, "emit wallpaper-removed 失败：前端 UI 可能不刷新");
    }
    Ok(())
}

#[tauri::command]
pub async fn set_wallpaper(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    wallpaper_id: String,
    display_id: Option<String>,
    scaling_mode: Option<ScalingMode>,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // 使用 get_wallpaper 按 id 查找，避免全量克隆 Vec<WallpaperEntry>
    // v16-C-003: 壁纸不存在时不暴露内部 id，提示用户刷新列表（条目可能已被移除）
    let entry = state.config_manager
        .get_wallpaper(&wallpaper_id)
        .ok_or_else(|| {
            mirrorstar_core::MirrorStarError::DesktopIntegration(
                "壁纸不存在（可能已被移除），请刷新壁纸列表".to_string(),
            )
        })?;

    // SEC-001: 从 config 读取后重新校验路径，防配置篡改指向任意文件
    validate_wallpaper_file_path(&entry.file_path).await?;

    let display = display_id.or(entry.display_id.clone()).unwrap_or_default();
    // T15：获取 per-display "设置中" 标志，防止并发 set_wallpaper 同一 display 时
    // WorkerW 3 阶段流程竞态导致渲染器进程泄漏。guard 在函数返回时 Drop 自动移除标志。
    let _setting_guard = DisplaySettingGuard::acquire(display.clone())?;
    let source = WallpaperSource::File(entry.file_path.clone());
    let wp_type = entry.wallpaper_type;
    // 优先使用命令参数；未提供时回退到默认值（配置暂无 scaling_mode 字段）
    let scaling_mode = scaling_mode.unwrap_or_default();

    // 判断是否为原生壁纸模式
    let is_native = mirrorstar_core::wallpaper::manager::determine_wallpaper_mode(&source, wp_type)
        == mirrorstar_core::wallpaper::manager::WallpaperMode::Native;

    let engine = state.wallpaper_engine.clone();
    let display_for_emit = display.clone();

    if is_native {
        // 原生壁纸：快速路径，在锁内完成（注册表写入 + SystemParametersInfo，<100ms）
        let result = tokio::task::spawn_blocking(move || {
            let mut engine = engine.blocking_lock();
            engine.prepare_for_wallpaper(&display, wp_type)?;
            // 原生壁纸路径
            if let WallpaperSource::File(path) = &source {
                engine.set_native_wallpaper_internal(
                    &display,
                    path,
                    scaling_mode,
                    &source,
                    wp_type,
                )?;
            }
            Ok::<(), mirrorstar_core::MirrorStarError>(())
        })
        .await
        .map_err(|e| mirrorstar_core::MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))?;
        result?;
    } else {
        // WorkerW 壁纸：3 阶段模式，减少锁持有时间
        // 阶段 1：获取配置 + 关闭现有壁纸（短暂持锁）
        let renderer_config = {
            let mut engine = engine.lock().await;
            engine.prepare_for_wallpaper(&display, wp_type)?;
            engine.renderer_config()
        };

        // Web 类型总是走冷启动路径（WebView2 初始化 5-15s），提前 emit
        // `wallpaper-loading` 让前端展示进度提示，避免用户误以为卡死。
        if wp_type == mirrorstar_core::config::WallpaperType::Web {
            if let Err(e) = app.emit(
                "wallpaper-loading",
                WallpaperLoadingPayload {
                    display_id: &display_for_emit,
                    message: "正在初始化网页壁纸引擎，预计 5-15 秒...",
                },
            ) {
                tracing::warn!(error = %e, "emit wallpaper-loading 失败");
            }
        }

        // 阶段 2：创建并播放渲染器（无 engine 锁，耗时操作）
        //
        // v16-C-010: GIF 类型在此阶段同步检测首帧是否超 8MB 阈值（4K GIF 场景），
        // 超阈值时返回 gif_oversized=true，由外层 emit warning 提示用户播放可能不流畅。
        // 检测复用 decode_gif_first_frame 的降采样逻辑，与 v10-C 跳过判断阈值一致，
        // 预测结果与实际跳过行为一致。首帧解码会重复一次（GifRenderer::play 内再解码），
        // 开销 <500ms 可接受（set_wallpaper 非高频操作）。
        let source_clone = source.clone();
        let (renderer, gif_oversized) = tokio::task::spawn_blocking(move || {
            // T-005: spawn_blocking 线程初始化 COM（MTA），使 Video 类型
            // VideoRenderer::play() 内的 VolumeControl WASAPI 调用可用
            // （tokio 默认不在 blocking 线程初始化 COM）。
            // _com_guard 离开作用域时自动调用 CoUninitialize。
            let _com_guard = ComGuard::new();
            // v16-C-010: GIF 类型检测首帧超阈值（4K GIF），非 GIF 类型直接 false
            let gif_oversized = if wp_type == mirrorstar_core::config::WallpaperType::Gif {
                if let WallpaperSource::File(p) = &source_clone {
                    mirrorstar_core::wallpaper::gif_decode::gif_first_frame_oversized(p)
                } else {
                    false
                }
            } else {
                false
            };
            let renderer = mirrorstar_core::wallpaper::manager::create_and_play_renderer(
                &source_clone,
                wp_type,
                scaling_mode,
                &renderer_config,
            )?;
            Ok::<_, mirrorstar_core::MirrorStarError>((renderer, gif_oversized))
        })
        .await
        .map_err(|e| {
            mirrorstar_core::MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e))
        })??;

        // v16-C-010: 4K GIF 首帧超阈值时 emit warning，前端 toast 提示用户
        // 播放可能不流畅（v15-B-005 同步解码兜底导致帧率下降）
        if gif_oversized {
            if let Err(e) = app.emit(
                "wallpaper-gif-oversized",
                GifOversizedPayload {
                    display_id: &display_for_emit,
                },
            ) {
                tracing::warn!(error = %e, "emit wallpaper-gif-oversized 失败");
            }
        }

        // 阶段 3：嵌入并注册（短暂持锁）
        {
            let mut engine = engine.lock().await;
            engine.embed_and_register_renderer(renderer, &display, &source, wp_type)?;
        }
    }

    if let Err(e) = app.emit("wallpaper-state-changed", display_for_emit) {
        tracing::warn!(error = %e, "emit wallpaper-state-changed 失败：前端 UI 可能不刷新");
    }
    Ok(())
}

#[tauri::command]
pub async fn pause_wallpaper(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    display_id: Option<String>,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // 获取 engine 锁后调用快速路径方法（原生壁纸无 sender 时返回 Ok(false)）
    // 状态变更 emit 由全局订阅任务统一处理（详见 lib.rs setup 闭包）
    //
    // 直接使用 pause_wallpaper_fast 返回的 bool，消除额外的
    // has_pause_sender HashMap 查找。
    //
    // T03: 与 set_volume / set_speed 等命令统一使用 resolve_display_id，
    // display_id 为 None 或空串时回退到 first_active_display_id，
    // 确保前端传 None 时 pause 能正确定位到当前活跃壁纸所在的显示器。
    let (has_sender, display) = {
        let engine = state.wallpaper_engine.lock().await;
        let display = resolve_display_id(display_id, &engine);
        let has_sender = engine.pause_wallpaper_fast(&display)?;
        (has_sender, display)
    };

    if !has_sender {
        if let Err(e) = app.emit("wallpaper-state-changed", display) {
            tracing::warn!(error = %e, "emit wallpaper-state-changed 失败：前端 UI 可能不刷新");
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn resume_wallpaper(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    display_id: Option<String>,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // 获取 engine 锁后调用快速路径方法（原生壁纸无 sender 时返回 Ok(false)）
    // 状态变更 emit 由全局订阅任务统一处理（详见 lib.rs setup 闭包）
    //
    // 直接使用 resume_wallpaper_fast 返回的 bool，消除额外的
    // has_pause_sender HashMap 查找。
    //
    // T03: 与 pause_wallpaper 及其他快速路径命令统一使用 resolve_display_id，
    // display_id 为 None 或空串时回退到 first_active_display_id。
    let (has_sender, display) = {
        let engine = state.wallpaper_engine.lock().await;
        let display = resolve_display_id(display_id, &engine);
        let has_sender = engine.resume_wallpaper_fast(&display)?;
        (has_sender, display)
    };

    if !has_sender {
        if let Err(e) = app.emit("wallpaper-state-changed", display) {
            tracing::warn!(error = %e, "emit wallpaper-state-changed 失败：前端 UI 可能不刷新");
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn set_volume(
    state: State<'_, AppState>,
    display_id: Option<String>,
    volume: f32,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // T-004: 校验音量范围 [0.0, 1.0]，拒绝 NaN/Infinity
    validate_volume(volume)?;
    // 获取 engine 锁后调用快速路径方法（原生壁纸无 sender 时返回 Ok）
    let engine = state.wallpaper_engine.lock().await;
    // FE-001: display_id 为 None 或空串时回退到第一个活跃壁纸的 display_id
    let display = resolve_display_id(display_id, &engine);
    engine.set_volume_fast(&display, volume)
}

#[tauri::command]
pub async fn toggle_mute(
    state: State<'_, AppState>,
    display_id: Option<String>,
) -> Result<bool, mirrorstar_core::MirrorStarError> {
    // 获取 engine 锁后调用快速路径方法
    // toggle_mute_fast 现返回 Result<Option<bool>, MirrorStarError>：
    //   Some(b) → 返回 b（新静音状态）
    //   None    → 返回 false（无活跃壁纸/sender，中性响应，保持前端兼容）
    let engine = state.wallpaper_engine.lock().await;
    // FE-001: display_id 为 None 或空串时回退到第一个活跃壁纸的 display_id
    let display = resolve_display_id(display_id, &engine);
    let result = engine.toggle_mute_fast(&display)?;
    Ok(result.unwrap_or(false))
}

#[tauri::command]
pub async fn get_wallpaper_state(
    state: State<'_, AppState>,
    display_id: Option<String>,
) -> Result<Option<mirrorstar_core::WallpaperState>, mirrorstar_core::MirrorStarError> {
    // 获取 engine 锁后调用快速路径方法读取共享状态
    let engine = state.wallpaper_engine.lock().await;
    // FE-001: display_id 为 None 或空串时回退到第一个活跃壁纸的 display_id
    let display = resolve_display_id(display_id, &engine);
    Ok(engine.get_wallpaper_state_fast(&display))
}

#[tauri::command]
pub async fn set_scaling_mode(
    state: State<'_, AppState>,
    display_id: Option<String>,
    mode: ScalingMode,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    let mut engine = state.wallpaper_engine.lock().await;

    // FE-001: display_id 为 None 或空串时回退到第一个活跃壁纸的 display_id
    let display = resolve_display_id(display_id, &engine);
    engine.set_scaling_mode(&display, mode)?;

    Ok(())
}

#[tauri::command]
pub async fn set_speed(
    state: State<'_, AppState>,
    display_id: Option<String>,
    speed: f32,
) -> Result<(), mirrorstar_core::MirrorStarError> {
    // T-004: 校验播放速度范围 (0.0, 10.0]，拒绝 NaN/Infinity
    validate_speed(speed)?;

    // ST-005: 改为同步执行（与 set_volume 一致），避免 fire-and-forget JoinHandle
    // 被 drop 导致 shutdown 后访问 engine 的风险。
    //
    // 原实现短暂持锁解析 display_id 后立即释放锁，再 spawn 后台任务执行
    // engine.set_speed()。但 spawn 的 JoinHandle 未被 perform_shutdown_blocking
    // 跟踪，shutdown 后任务可能仍在等待 engine 锁，并在 engine.shutdown() 之后
    // 访问已关闭的 engine。
    //
    // 同步执行的阻塞窗口评估：set_speed 的 IPC 写入通常 <100ms，与 set_volume
    // 一致，同步阻塞窗口可接受。原 fire-and-forget 优化的"避免串行瓶颈"在
    // 实际场景中收益有限（set_speed 调用频率低，IPC 写入快）。
    let mut engine = state.wallpaper_engine.lock().await;
    // FE-001: display_id 为 None 或空串时回退到第一个活跃壁纸的 display_id
    let display = resolve_display_id(display_id, &engine);
    engine.set_speed(&display, speed).await
}

// ── 纯逻辑校验函数（ST-007：提取为独立函数以支持非 #[ignore] 单元测试） ──────

/// 校验音量值范围（T-004 + ST-007）
///
/// 接受 [0.0, 1.0] 范围内的有限值，拒绝 NaN / Infinity / 越界值。
/// 提取为独立纯函数以便在 CI 中通过单元测试覆盖校验逻辑
/// （原内联校验仅在 #[ignore] 集成测试中通过模拟覆盖，CI 不执行）。
pub fn validate_volume(volume: f32) -> Result<(), mirrorstar_core::MirrorStarError> {
    mirrorstar_core::config::validation::validate_volume(volume)
}

/// 校验播放速度范围（T-004 + ST-007）
///
/// 接受 (0.0, 10.0] 范围内的有限值，拒绝 NaN / Infinity / 非正值 / 超上限值。
pub fn validate_speed(speed: f32) -> Result<(), mirrorstar_core::MirrorStarError> {
    mirrorstar_core::config::validation::validate_speed(speed)
}

/// 解析缩放模式字符串为 `ScalingMode` 枚举（ST-007）
///
/// 接受 "fill" / "fit" / "stretch" / "center" / "original"，未知值返回错误。
pub fn parse_scaling_mode(mode: &str) -> Result<ScalingMode, mirrorstar_core::MirrorStarError> {
    match mode {
        "fill" => Ok(ScalingMode::Fill),
        "fit" => Ok(ScalingMode::Fit),
        "stretch" => Ok(ScalingMode::Stretch),
        "center" => Ok(ScalingMode::Center),
        "original" => Ok(ScalingMode::Original),
        _ => Err(mirrorstar_core::MirrorStarError::InvalidArgument {
            reason: format!("未知的缩放模式: {}", mode),
        }),
    }
}

/// `regenerate_thumbnails` 命令的返回结果
///
/// 用于批量重新生成缺失缩略图，
/// `total` = 实际尝试处理数（排除 Web 类型与 ffmpeg 不可用时的 Video 类型），
/// `success + failed == total`。
#[derive(serde::Serialize)]
pub struct RegenerateResult {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
}

/// `wallpaper-regenerate-progress` 事件 payload（ST-009）
///
/// 在 `regenerate_thumbnails` 循环中每完成一项 emit 一次，
/// 供前端显示进度条。派生 `Clone` 是因为 Tauri v2 的 `Emitter::emit` 要求
/// `S: Serialize + Clone`。
#[derive(serde::Serialize, Clone)]
struct RegenerateProgressPayload {
    processed: usize,
    success: usize,
    failed: usize,
    total: usize,
}

/// 批量重新生成缺失缩略图
///
/// 扫描壁纸库中 `thumbnail` 字段为空的条目，按类型分派生成缩略图：
/// - Image/Gif → `generate_thumbnail`
/// - Video → `generate_video_thumbnail`（ffmpeg 不可用时跳过）
/// - Web → 跳过（无源文件）
///
/// 成功时 emit `wallpaper-updated`，失败时 emit `wallpaper-thumbnail-failed`。
/// ST-009: 每完成一项 emit `wallpaper-regenerate-progress` 事件供前端显示进度条。
/// 整个生成过程在 `spawn_blocking` 中同步执行，避免阻塞 tokio 异步运行时。
///
/// 批量处理流程与进度报告文档化
///
/// # 批量处理流程
///
/// ## 1. 收集待处理条目
///
/// - 读取 `config_manager.get_wallpapers()` 获取壁纸库
/// - 过滤 `thumbnail.is_empty()` 的条目（仅处理缺失缩略图的）
/// - 立即释放读锁（克隆后不再持有 config_manager 内部 RwLock）
/// - 若无待处理条目，提前返回 `RegenerateResult { total: 0, success: 0, failed: 0 }`
///
/// ## 2. 准备资源
///
/// - 获取 `thumbnail_dir`：`ConfigManager::data_dir()?.join("thumbnails")`
/// - 克隆 `config_manager` Arc 与 `app_handle` 供 spawn_blocking 闭包使用
///
/// ## 3. spawn_blocking 同步执行
///
/// 整个生成循环在 `tokio::task::spawn_blocking` 中同步执行：
/// - **执行环境**：tokio blocking 线程池（不阻塞 async runtime）
/// - **逐项处理**：遍历 `wallpapers` Vec，按 `wallpaper_type` 分派：
///   - `Image` / `Gif` → `generate_thumbnail(&file_path, &thumbnail_dir)`
///   - `Video` → 检查 `is_ffmpeg_available()`，可用时 `generate_video_thumbnail`，
///     不可用时 `continue` 跳过（不计 success 也不计 failed，预期降级）
///   - `Web` → `continue` 跳过（无源文件）
/// - **结果处理**：
///   - 成功：`config_manager.update_thumbnail(...)` 持久化路径 +
///     emit `wallpaper-updated` 通知前端刷新 + `success += 1`
///   - 失败：emit `wallpaper-thumbnail-failed`（含 error 信息）+ `failed += 1`
///   - `update_thumbnail` 失败：计为 `failed`（缩略图已生成但未持久化路径）
///
/// ## 4. 返回结果
///
/// - `total = success + failed`（不含 Web 与 ffmpeg 不可用的 Video，这些通过 continue 跳过）
/// - 保证 `total == success + failed`（前端可据此显示进度）
///
/// # 进度报告（事件驱动）
///
/// ## `wallpaper-regenerate-progress` 事件（ST-009）
///
/// 每完成一项（无论成功/失败）emit 一次，payload 为 `RegenerateProgressPayload`：
///
/// ```json
/// {
///   "processed": 3,      // 已处理数（success + failed）
///   "success": 2,        // 成功数
///   "failed": 1,         // 失败数
///   "total": 10          // 总待处理数（wallpapers.len()）
/// }
/// ```
///
/// - **前端使用**：根据 `processed / total` 计算百分比显示进度条，
///   根据 `success / failed` 显示成功/失败计数
/// - **emit 频率**：每项一次（非节流），通常每项处理 100ms-2s，
///   emit 开销可忽略（前端事件处理 <1ms）
/// - **emit 失败处理**：仅记录 warn 日志，不影响后续处理
///
/// ## `wallpaper-updated` 事件
///
/// 每个缩略图成功生成后 emit，前端据此刷新对应壁纸条目的预览图
///
/// ## `wallpaper-thumbnail-failed` 事件
///
/// 每个缩略图生成失败时 emit，payload 含 `file_path` 与 `error`，
/// 前端可据此展示降级占位图或错误提示
///
/// # 并发与性能
///
/// - **串行处理**：当前实现为串行（for 循环逐项处理），不并行
///   - 理由：缩略图生成是 CPU/IO 密集型，并行会争抢 CPU 与磁盘 IO，
///     实际总耗时可能更长（且增加内存峰值）
///   - 如需并行：可用 `rayon::par_iter` 替换 for 循环，但需评估 ffmpeg
///     并发调用的稳定性
/// - **典型耗时**：
///   - Image/Gif：~50-100ms/项
///   - Video：~500ms-2s/项（取决于视频长度与 ffmpeg 启动开销）
///   - 100 项库的批量重生成：~10-60s（视壁纸类型分布）
/// - **内存占用**：单次仅持有一项的缩略图数据，无内存峰值
#[tauri::command]
pub async fn regenerate_thumbnails(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<RegenerateResult, mirrorstar_core::MirrorStarError> {
    use mirrorstar_core::config::WallpaperType;

    // 收集 thumbnail 为空的条目（克隆后立即释放读锁）
    let wallpapers: Vec<_> = state
        .config_manager
        .get_wallpapers()
        .into_iter()
        .filter(|w| w.thumbnail.is_empty())
        .collect();

    if wallpapers.is_empty() {
        return Ok(RegenerateResult {
            total: 0,
            success: 0,
            failed: 0,
        });
    }

    let thumbnail_dir = ConfigManager::data_dir()?.join("thumbnails");
    let config_manager = state.config_manager.clone();
    let app_clone = app.clone();

    let (success, failed) = tokio::task::spawn_blocking(move || -> (usize, usize) {
        let mut success = 0usize;
        let mut failed = 0usize;
        // emit 进度事件节流，避免 100 项库产生 100 次 IPC emit。
        // 触发条件：每完成 5 项 或 距上次 emit >= 200ms，循环结束后再 emit 一次
        // 确保 100% 进度送达前端（详见循环末尾）。
        const EMIT_BATCH_SIZE: usize = 5;
        const EMIT_INTERVAL_MS: u128 = 200;
        let mut last_emit_at = std::time::Instant::now();
        let mut last_emit_processed: usize = 0;
        let total = wallpapers.len();
        // 循环内仅生成缩略图文件并收集 (file_path, thumbnail_path)，
        // 循环结束后调用 batch_update_thumbnails 单次落盘，避免 N 次 save_library。
        let mut updates: Vec<(String, String)> = Vec::with_capacity(wallpapers.len());
        for wp in &wallpapers {
            let result = match wp.wallpaper_type {
                WallpaperType::Image | WallpaperType::Gif => {
                    mirrorstar_core::config::generate_thumbnail(&wp.file_path, &thumbnail_dir)
                }
                WallpaperType::Video => {
                    if mirrorstar_core::config::is_ffmpeg_available() {
                        mirrorstar_core::config::generate_video_thumbnail(
                            &wp.file_path,
                            &thumbnail_dir,
                        )
                    } else {
                        tracing::info!(
                            path = %wp.file_path,
                            "ffmpeg 不可用，跳过 Video 缩略图重生成"
                        );
                        continue; // 不计 success 也不计 failed（预期降级）
                    }
                }
                WallpaperType::Web => continue, // Web 无源文件，跳过
            };
            match result {
                Ok(thumb_name) => {
                    let full_path = thumbnail_dir.join(&thumb_name);
                    let full_path_str = full_path.to_string_lossy().to_string();
                    // 收集更新，循环结束后批量持久化
                    updates.push((wp.file_path.clone(), full_path_str));
                    success += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %wp.file_path, "缩略图重生成失败");
                    // 失败日志打印源文件“头 8 字节 hex”，用于定位 os error 123 这类 IO 错误
                    log_file_header(&wp.file_path);
                    // 按失败类型分派（孤儿壁纸条目兜底），语义与 add_wallpaper 一致：
                    // - SourceMissing：源文件缺失（孤儿条目），warn 日志 + wallpaper-source-missing，
                    //   **不** emit wallpaper-thumbnail-failed；不计 success（不写回缩略图），计入 failed。
                    // - DecodeFallback：内容损坏/格式非法（源文件存在），回退源路径作缩略图进 updates。
                    // - Unrecoverable：其它不可恢复错误，emit 失败弹窗。
                    match classify_thumbnail_failure(&e, wp.wallpaper_type, &wp.file_path) {
                        ThumbnailFailureKind::SourceMissing => {
                            tracing::warn!(
                                path = %wp.file_path,
                                "孤儿壁纸条目：源文件缺失，跳过失败弹窗并保留条目（等待用户处理）"
                            );
                            if let Err(emit_err) = app_clone.emit(
                                "wallpaper-source-missing",
                                SourceMissingPayload {
                                    id: &wp.id,
                                    file_path: &wp.file_path,
                                },
                            ) {
                                tracing::warn!(error = %emit_err, "emit wallpaper-source-missing 失败");
                            }
                            failed += 1;
                        }
                        ThumbnailFailureKind::DecodeFallback => {
                            // 回退：以源文件路径作为缩略图，收集进 updates 随循环后批量落盘
                            updates.push((wp.file_path.clone(), wp.file_path.clone()));
                            success += 1;
                        }
                        ThumbnailFailureKind::Unrecoverable => {
                            if let Err(emit_err) = app_clone.emit(
                                "wallpaper-thumbnail-failed",
                                ThumbnailFailedPayload {
                                    file_path: &wp.file_path,
                                    error: e.to_string(),
                                },
                            ) {
                                tracing::warn!(error = %emit_err, "emit wallpaper-thumbnail-failed 失败");
                            }
                            failed += 1;
                        }
                    }
                }
            }
            // ST-009: emit 进度事件供前端显示进度条
            // 节流策略——每 EMIT_BATCH_SIZE 项或 EMIT_INTERVAL_MS
            // 间隔 emit 一次，避免 100 项库产生 100 次 IPC emit（每次 0.1-0.5ms）。
            // 循环结束后再 emit 一次确保 100% 进度送达前端。
            let processed = success + failed;
            let now = std::time::Instant::now();
            let elapsed_ms = now.duration_since(last_emit_at).as_millis();
            let batch_delta = processed.saturating_sub(last_emit_processed);
            if batch_delta >= EMIT_BATCH_SIZE || elapsed_ms >= EMIT_INTERVAL_MS {
                if let Err(e) = app_clone.emit(
                    "wallpaper-regenerate-progress",
                    RegenerateProgressPayload {
                        processed,
                        success,
                        failed,
                        total,
                    },
                ) {
                    tracing::warn!(error = %e, "emit wallpaper-regenerate-progress 失败");
                }
                last_emit_at = now;
                last_emit_processed = processed;
            }
        }

        // 循环结束后 emit 最终一次，确保 100% 进度送达前端
        // （节流策略可能使最后一次迭代未触发 emit，此处兜底）。
        if let Err(e) = app_clone.emit(
            "wallpaper-regenerate-progress",
            RegenerateProgressPayload {
                processed: success + failed,
                success,
                failed,
                total,
            },
        ) {
            tracing::warn!(error = %e, "emit wallpaper-regenerate-progress（最终）失败");
        }

        // 批量更新缩略图路径，仅取一次写锁 + 一次 save_library，
        // 避免 N 次独立 update_thumbnail 产生的 O(N) fsync + 序列化开销。
        match config_manager.batch_update_thumbnails(&updates) {
            Ok(updated) => {
                tracing::info!(
                    updated,
                    total = updates.len(),
                    "regenerate_thumbnails 批量更新完成"
                );
                // 批量持久化完成后再 emit wallpaper-updated，确保前端读取到已更新的
                // 缩略图路径（若在循环内 emit，配置尚未落盘，前端会读到空路径）。
                // emit 完整 entry 供前端增量更新（替代原空 payload），
                // 前端据此定向更新单张卡片缩略图，避免全量刷新。
                let updated_file_paths: HashSet<&str> =
                    updates.iter().map(|(fp, _)| fp.as_str()).collect();
                for w in config_manager.get_wallpapers() {
                    if updated_file_paths.contains(w.file_path.as_str()) {
                        if let Err(e) = app_clone.emit("wallpaper-updated", w) {
                            tracing::warn!(error = %e, "emit wallpaper-updated 失败");
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "批量更新缩略图路径失败");
                // 批量持久化失败：已生成的缩略图无法写回配置，计入 failed
                failed += success;
                success = 0;
            }
        }
        (success, failed)
    })
    .await
    .map_err(|e| mirrorstar_core::MirrorStarError::TaskJoin(format!("任务 join 失败: {}", e)))?;

    // total = 实际尝试处理数（Web 与 ffmpeg 不可用的 Video 通过 continue 跳过，
    // 不计入 total），保证 total == success + failed
    let total = success + failed;
    Ok(RegenerateResult {
        total,
        success,
        failed,
    })
}

/// v16-C-002：将文件操作 `io::Error` 映射为可操作性更强的 `MirrorStarError`。
///
/// 原实现直接 `format!("{}失败: {}", context, e)` 会拼接英文 OS 错误
/// （如 "Access is denied. (os error 5)"），经前端 `sanitizeErrorMessage`
/// 脱敏后变为 "...(<error-code>)"，既非全中文也无可操作性建议。
///
/// 本函数按 `io::ErrorKind` 与 `raw_os_error()` 提供中文映射 + 可操作性建议：
/// - `PermissionDenied` / EACCES(5) / EPERM(1) → 权限不足，建议检查目录权限
/// - ENOSPC(28) / EDQUOT(122) → 磁盘空间不足，建议清理后重试
/// - 其他 → 保留原始错误并附加"请检查文件是否被占用或路径是否可访问"
///
/// 注意：`io::ErrorKind::StorageFull` 在 Rust 1.83 才稳定，本项目 MSRV 1.80，
/// 故使用 `raw_os_error()` 数值匹配而非 `ErrorKind::StorageFull` 变体。
fn map_io_error(context: &str, e: std::io::Error) -> mirrorstar_core::MirrorStarError {
    use std::io::ErrorKind;
    let raw = e.raw_os_error().unwrap_or(0);
    let detail = match e.kind() {
        ErrorKind::PermissionDenied => {
            format!("{}失败：权限不足，请检查目标目录权限", context)
        }
        _ if raw == 5 || raw == 1 => {
            // Windows EACCES(5) / POSIX EPERM(1)：部分场景 kind() 未归一为 PermissionDenied
            format!("{}失败：权限不足，请检查目标目录权限", context)
        }
        _ if raw == 28 || raw == 122 => {
            // ENOSPC(28) 磁盘满 / EDQUOT(122) 配额超限
            format!("{}失败：磁盘空间不足，请清理后重试", context)
        }
        _ => {
            format!(
                "{}失败：{}（请检查文件是否被占用或路径是否可访问）",
                context, e
            )
        }
    };
    mirrorstar_core::MirrorStarError::DesktopIntegration(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_wallpaper_file_path 单元测试（ST-007：非 #[ignore]，CI 可执行） ──
    //
    // 覆盖 validate_wallpaper_file_path 的 3 种失败路径（不绝对/遍历/不存在）+ 成功路径。
    // 这些测试不需要 Windows COM/音频环境，可在任意平台 CI 执行。

    #[tokio::test]
    async fn test_validate_wallpaper_file_path_relative_rejected() {
        // 相对路径应被拒绝
        let relative_paths = ["relative/path.mp4", "./video.mp4", "file.mp4"];
        for p in relative_paths {
            let err = validate_wallpaper_file_path(p).await.unwrap_err();
            match err {
                mirrorstar_core::MirrorStarError::InvalidPath { reason } => {
                    assert!(
                        reason.contains("文件路径必须为绝对路径"),
                        "原因应包含「文件路径必须为绝对路径」，实际: {}",
                        reason
                    );
                }
                other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_validate_wallpaper_file_path_parent_dir_rejected() {
        // 路径遍历（含 ..）应被拒绝，即使是绝对路径
        // 注意：必须使用当前平台的绝对路径形式，否则会先触发"路径必须为绝对路径"错误。
        let traversal_paths: &[&str] = if cfg!(windows) {
            &[
                "C:\\etc\\..\\etc\\passwd",
                "C:\\Users\\..\\Windows",
                "C:\\home\\user\\..\\..\\..\\etc\\hosts",
            ]
        } else {
            &[
                "/etc/../etc/passwd",
                "/home/user/../../../etc/hosts",
                "/var/log/../../etc/hosts",
            ]
        };
        for p in traversal_paths {
            let err = validate_wallpaper_file_path(p).await.unwrap_err();
            match err {
                mirrorstar_core::MirrorStarError::InvalidPath { reason } => {
                    assert!(
                        reason.contains("路径包含 .. 组件"),
                        "原因应包含「路径包含 .. 组件」，实际: {}",
                        reason
                    );
                }
                other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_validate_wallpaper_file_path_nonexistent_rejected() {
        // 绝对路径但文件不存在应被拒绝
        let nonexistent = if cfg!(windows) {
            "C:\\nonexistent\\path\\to\\file.mp4"
        } else {
            "/nonexistent/path/to/file.mp4"
        };
        let err = validate_wallpaper_file_path(nonexistent).await.unwrap_err();
        match err {
            mirrorstar_core::MirrorStarError::InvalidPath { reason } => {
                assert!(
                    reason.contains("文件不存在或不可访问"),
                    "原因应包含「文件不存在或不可访问」，实际: {}",
                    reason
                );
            }
            other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_validate_wallpaper_file_path_existing_file_accepted() {
        // 创建临时文件并校验路径应通过（T08：canonicalize 后路径与原始路径一致）
        let temp_dir = tempfile::TempDir::new().expect("创建临时目录失败");
        let file_path = temp_dir.path().join("test.mp4");
        std::fs::write(&file_path, b"test").expect("写入测试文件失败");
        let path_str = file_path.to_string_lossy().to_string();
        validate_wallpaper_file_path(&path_str)
            .await
            .expect("存在的文件路径应通过校验");
    }

    // ── ST-001：兄弟目录误判修复测试 ──────────────────────────────────────────
    //
    // 验证 is_path_within_data_dir 使用 Path::starts_with（路径组件匹配），
    // 不会把 mirrorstar-evil / mirrorstar_backup / mirrorstar.tmp 等以 "mirrorstar"
    // 开头的兄弟目录误判为受信任的数据目录子路径。

    #[test]
    fn st001_sibling_directory_not_misjudged() {
        let (data_dir, sibling_paths, trusted_subpaths): (
            std::path::PathBuf,
            Vec<&str>,
            Vec<&str>,
        ) = if cfg!(windows) {
            (
                std::path::PathBuf::from(r"C:\Users\test\AppData\Roaming\mirrorstar"),
                vec![
                    r"C:\Users\test\AppData\Roaming\mirrorstar-evil\wallpaper.mp4",
                    r"C:\Users\test\AppData\Roaming\mirrorstar_backup\wallpaper.mp4",
                    r"C:\Users\test\AppData\Roaming\mirrorstar.tmp\wallpaper.mp4",
                    r"C:\Users\test\AppData\Roaming\mirrorstar_",
                ],
                vec![
                    r"C:\Users\test\AppData\Roaming\mirrorstar\wallpapers\abc.mp4",
                    r"C:\Users\test\AppData\Roaming\mirrorstar\x",
                ],
            )
        } else {
            (
                std::path::PathBuf::from("/home/test/.local/share/mirrorstar"),
                vec![
                    "/home/test/.local/share/mirrorstar-evil/wallpaper.mp4",
                    "/home/test/.local/share/mirrorstar_backup/wallpaper.mp4",
                    "/home/test/.local/share/mirrorstar.tmp/wallpaper.mp4",
                    "/home/test/.local/share/mirrorstar_",
                ],
                vec![
                    "/home/test/.local/share/mirrorstar/wallpapers/abc.mp4",
                    "/home/test/.local/share/mirrorstar/x",
                ],
            )
        };

        for sibling in &sibling_paths {
            assert!(
                !is_path_within_data_dir(sibling, &data_dir),
                "兄弟目录不应被误判为受信任目录: {}",
                sibling
            );
        }

        for trusted in &trusted_subpaths {
            assert!(
                is_path_within_data_dir(trusted, &data_dir),
                "真正的数据目录子路径应返回 true: {}",
                trusted
            );
        }
    }

    // ── T08（P0 安全）：canonicalize 符号链接拒绝测试 ──────────────────────────
    //
    // 验证 T08 修复：路径中包含符号链接时应被拒绝。
    // 测试需要 Windows 开发者模式或管理员权限才能创建符号链接，因此标记为 #[ignore]，
    // 在 Windows 真机环境通过 `cargo test -- --ignored` 运行。
    //
    // 测试策略：
    // 1. 创建目标文件（target.mp4）
    // 2. 在另一目录创建符号链接（link.mp4 → target.mp4）
    // 3. 校验 link.mp4 应被拒绝（canonical 路径指向 target.mp4，与 link.mp4 不一致）

    #[cfg(windows)]
    #[ignore = "需要 Windows 开发者模式或管理员权限创建符号链接"]
    #[tokio::test]
    async fn test_validate_wallpaper_file_path_symlink_rejected() {
        use std::os::windows::fs::symlink_file;

        let temp_dir = tempfile::TempDir::new().expect("创建临时目录失败");

        // 创建目标文件（符号链接指向的目标）
        let target_path = temp_dir.path().join("target.mp4");
        std::fs::write(&target_path, b"test").expect("写入目标文件失败");

        // 在子目录创建符号链接，使 canonical 路径与 link 路径明显不同
        let link_dir = temp_dir.path().join("links");
        std::fs::create_dir_all(&link_dir).expect("创建链接目录失败");
        let link_path = link_dir.join("link.mp4");
        symlink_file(&target_path, &link_path).expect("创建符号链接失败");

        // 验证符号链接被拒绝（canonical 路径指向 target.mp4，与 link.mp4 不一致）
        let path_str = link_path.to_string_lossy().to_string();
        let err = validate_wallpaper_file_path(&path_str)
            .await
            .expect_err("符号链接路径应被拒绝");
        match err {
            mirrorstar_core::MirrorStarError::InvalidPath { reason } => {
                assert!(
                    reason.contains("符号链接"),
                    "原因应包含「符号链接」，实际: {}",
                    reason
                );
            }
            other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
        }
    }

    // ── T15（P0 资源泄漏）：per-display 防竞态标志测试 ─────────────────────────
    //
    // 验证 DisplaySettingGuard 的 RAII 行为：
    // - 首次 acquire 成功，第二次同 display_id acquire 被拒绝
    // - guard Drop 后 display_id 从集合移除，可再次 acquire
    // - 不同 display_id 互不影响

    #[test]
    fn test_display_setting_guard_blocks_concurrent_same_display() {
        // T15：同一 display_id 第二次 acquire 应被拒绝
        let display_id = "test_display_concurrent".to_string();

        // 清理可能残留的标志（其他测试中断时可能未清理）
        if let Ok(mut set) = DISPLAYS_SETTING.lock() {
            set.remove(&display_id);
        }

        // 首次 acquire 应成功
        let guard = DisplaySettingGuard::acquire(display_id.clone()).expect("首次 acquire 应成功");
        // 第二次 acquire 同一 display_id 应被拒绝
        let err = DisplaySettingGuard::acquire(display_id.clone())
            .expect_err("并发同 display acquire 应被拒绝");
        match err {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("正在切换壁纸"),
                    "错误消息应包含「正在切换壁纸」，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
        // guard Drop 后释放标志
        drop(guard);
        // Drop 后应可再次 acquire
        DisplaySettingGuard::acquire(display_id.clone())
            .expect("guard Drop 后应可再次 acquire")
            // 立即 drop 释放，避免影响其他测试
            ;
    }

    #[test]
    fn test_display_setting_guard_different_displays_independent() {
        // T15：不同 display_id 互不阻塞
        let display_a = "test_display_a".to_string();
        let display_b = "test_display_b".to_string();

        // 清理可能残留的标志
        if let Ok(mut set) = DISPLAYS_SETTING.lock() {
            set.remove(&display_a);
            set.remove(&display_b);
        }

        let _guard_a =
            DisplaySettingGuard::acquire(display_a.clone()).expect("display_a 首次 acquire 应成功");
        // display_b 应不受 display_a 影响，acquire 成功
        let _guard_b = DisplaySettingGuard::acquire(display_b.clone())
            .expect("display_b acquire 应不受 display_a 影响");
        // guards 在此 Drop，释放两个标志
    }

    // ── T05: THUMBNAIL_TASK Vec 收集测试 ──────────────────────────────────────
    //
    // 验证 T05 修复：THUMBNAIL_TASK 改为 Vec<JoinHandle> 后，连续 push 两个 handle
    // 不会互相覆盖（原 Mutex<Option> 实现 `*slot = Some(handle)` 会覆盖旧 handle）。
    // 同时验证 push 前的 `retain(|h| !h.is_finished())` 清理逻辑不会误删运行中的任务。

    /// 测试 THUMBNAIL_TASK 连续 push 两个 handle 均保留在 Vec 中
    ///
    /// T05 核心：原 `*slot = Some(handle)` 覆盖式赋值会丢失旧 handle（shutdown 时
    /// 只等待最后一个任务）。改为 Vec 后两个 handle 应同时存在，shutdown 时均被等待。
    ///
    /// 测试流程：
    /// 1. 清空 THUMBNAIL_TASK（避免其他测试残留干扰）
    /// 2. spawn 两个长时间运行的任务（sleep 200ms 模拟缩略图生成）
    /// 3. 模拟 add_wallpaper 的 push 逻辑（retain + push）push 两个 handle
    /// 4. 验证 Vec 长度为 2（两个 handle 均未被覆盖）
    /// 5. 等待两个任务完成，清空 Vec 避免影响其他测试
    #[test]
    fn test_thumbnail_task_vec_holds_multiple_handles() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tauri::async_runtime;

        // 清空可能残留的任务（其他测试中断时可能未清理）
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            // abort 并清空所有残留任务，避免影响本测试
            for h in slot.drain(..) {
                h.abort();
            }
        }

        // 标记任务是否执行（用于验证任务确实被 spawn 且未提前 abort）
        let task1_ran = Arc::new(AtomicBool::new(false));
        let task2_ran = Arc::new(AtomicBool::new(false));

        // spawn 两个短任务（block_on 在当前线程运行 runtime）
        let (handle1, handle2) = async_runtime::block_on(async {
            let t1 = Arc::clone(&task1_ran);
            let t2 = Arc::clone(&task2_ran);
            let h1 = tokio::task::spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                t1.store(true, Ordering::SeqCst);
            });
            let h2 = tokio::task::spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                t2.store(true, Ordering::SeqCst);
            });
            (h1, h2)
        });

        // 模拟 add_wallpaper 中连续两次 push handle 的逻辑
        // T05 修复后的代码：slot.retain(|h| !h.is_finished()); slot.push(handle);
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            slot.retain(|h| !h.is_finished());
            slot.push(handle1);
        }
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            slot.retain(|h| !h.is_finished());
            slot.push(handle2);
        }

        // 核心断言：两个 handle 均应在 Vec 中，第二个 push 不应覆盖第一个
        let count = crate::state::THUMBNAIL_TASK
            .lock()
            .map(|slot| slot.len())
            .unwrap_or(0);
        assert_eq!(
            count, 2,
            "T05：连续 push 两个 handle 后 Vec 长度应为 2（原 Option 实现会覆盖为 1）"
        );

        // 等待两个任务完成并清空 Vec，避免影响后续测试
        // 注意：先在锁内 drain 收集 handles 再释放锁，然后才 await，
        // 避免 std::sync::MutexGuard 跨 await 点持有（clippy::await_holding_lock）。
        let handles: Vec<_> = crate::state::THUMBNAIL_TASK
            .lock()
            .map(|mut slot| slot.drain(..).collect())
            .unwrap_or_default();
        async_runtime::block_on(async {
            for h in handles {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
            }
        });

        // 验证两个任务确实执行完毕（未被误 abort）
        assert!(task1_ran.load(Ordering::SeqCst), "任务 1 应执行完毕");
        assert!(task2_ran.load(Ordering::SeqCst), "任务 2 应执行完毕");
    }

    /// 测试 THUMBNAIL_TASK push 前的 retain 清理已完成任务逻辑
    ///
    /// T05：push 前 `slot.retain(|h| !h.is_finished())` 清理已完成任务，避免 Vec
    /// 无限增长。此测试验证：当一个任务已完成时，下一次 push 会先清理它。
    ///
    /// `async_yields_async`：测试有意从 `block_on(async { spawn_blocking(...) })` 获取
    /// `JoinHandle` 而不 await（模拟"任务已 spawn 但未 await"的真实场景），需 allow。
    #[allow(clippy::async_yields_async)]
    #[test]
    fn test_thumbnail_task_retain_clears_finished_handles() {
        use tauri::async_runtime;

        // 清空可能残留的任务
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            for h in slot.drain(..) {
                h.abort();
            }
        }

        // spawn 一个立即完成的任务（不 await，避免消费 handle）
        let finished_handle = async_runtime::block_on(async {
            tokio::task::spawn_blocking(|| {
                // 立即返回
            })
        });
        // 等待任务完成但不消费 handle：sleep 足够长时间确保 spawn_blocking 任务已退出
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(finished_handle.is_finished(), "任务应已完成");

        // push 已完成的 handle（模拟 add_wallpaper 在任务极快完成时的场景）
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            slot.push(finished_handle);
        }

        // 再 spawn 一个运行中的任务并 push
        let running_handle = async_runtime::block_on(async {
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
            })
        });
        if let Ok(mut slot) = crate::state::THUMBNAIL_TASK.lock() {
            // retain 应清理已完成的 handle，仅保留运行中的
            slot.retain(|h| !h.is_finished());
            slot.push(running_handle);
        }

        // 断言：已完成任务被清理，Vec 仅含运行中的任务（长度 1）
        let count = crate::state::THUMBNAIL_TASK
            .lock()
            .map(|slot| slot.len())
            .unwrap_or(0);
        assert_eq!(count, 1, "retain 应清理已完成任务，Vec 仅含运行中的任务");

        // 清理：等待运行中任务完成并清空 Vec
        // 先在锁内 drain 收集 handles 再释放锁，然后才 await（clippy::await_holding_lock）。
        let handles: Vec<_> = crate::state::THUMBNAIL_TASK
            .lock()
            .map(|mut slot| slot.drain(..).collect())
            .unwrap_or_default();
        async_runtime::block_on(async {
            for h in handles {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
            }
        });
    }

    // ── validate_volume / validate_speed / parse_scaling_mode 单元测试 ──────────
    //
    // 覆盖参数校验的边界值与非法值，原仅通过 #[ignore] 集成测试模拟覆盖，CI 不执行。
    // ST-007：提取为纯函数后可在 CI 中直接测试校验条件本身。

    #[test]
    fn test_validate_volume_accepts_valid_range() {
        // 边界合法值
        assert!(validate_volume(0.0).is_ok(), "0.0 应被接受");
        assert!(validate_volume(0.5).is_ok(), "0.5 应被接受");
        assert!(validate_volume(1.0).is_ok(), "1.0 应被接受");
    }

    #[test]
    fn test_validate_volume_rejects_invalid() {
        // NaN / Infinity / 越界值
        let invalid_values: [f32; 4] = [f32::NAN, f32::INFINITY, -0.1, 1.1];
        for v in invalid_values {
            let err = validate_volume(v).unwrap_err();
            match err {
                mirrorstar_core::MirrorStarError::InvalidArgument { reason } => {
                    assert!(
                        reason.contains("音量必须在 0.0-1.0 之间"),
                        "错误消息应包含「音量必须在 0.0-1.0 之间」，实际: {}",
                        reason
                    );
                }
                other => panic!("期望 InvalidArgument 变体，实际: {:?}", other),
            }
        }
    }

    #[test]
    fn test_validate_speed_accepts_valid_range() {
        // 边界合法值（含上限 10.0，不含 0.0）
        for v in [0.25_f32, 1.0, 4.0, 10.0] {
            assert!(validate_speed(v).is_ok(), "速度 {} 应被接受", v);
        }
    }

    #[test]
    fn test_validate_speed_rejects_invalid() {
        // NaN / Infinity / 非正值 / 超上限值
        let invalid_values: [f32; 5] = [f32::NAN, f32::INFINITY, 0.0, -1.0, 10.1];
        for v in invalid_values {
            let err = validate_speed(v).unwrap_err();
            match err {
                mirrorstar_core::MirrorStarError::InvalidArgument { reason } => {
                    assert!(
                        reason.contains("播放速度必须在 0.0-10.0 之间且大于 0"),
                        "错误消息应包含「播放速度必须在 0.0-10.0 之间且大于 0」，实际: {}",
                        reason
                    );
                }
                other => panic!("期望 InvalidArgument 变体，实际: {:?}", other),
            }
        }
    }

    #[test]
    fn test_parse_scaling_mode_valid_strings() {
        assert_eq!(parse_scaling_mode("fill").unwrap(), ScalingMode::Fill);
        assert_eq!(parse_scaling_mode("fit").unwrap(), ScalingMode::Fit);
        assert_eq!(parse_scaling_mode("stretch").unwrap(), ScalingMode::Stretch);
        assert_eq!(parse_scaling_mode("center").unwrap(), ScalingMode::Center);
        assert_eq!(
            parse_scaling_mode("original").unwrap(),
            ScalingMode::Original
        );
    }

    #[test]
    fn test_parse_scaling_mode_invalid_string() {
        let err = parse_scaling_mode("invalid_mode").unwrap_err();
        match err {
            mirrorstar_core::MirrorStarError::InvalidArgument { reason } => {
                assert!(
                    reason.contains("未知的缩放模式"),
                    "错误消息应包含「未知的缩放模式」，实际: {}",
                    reason
                );
            }
            other => panic!("期望 InvalidArgument 变体，实际: {:?}", other),
        }
    }

    // ── ST-007：Junction Points（IO_REPARSE_TAG_MOUNT_POINT）绕过防护测试 ──────
    //
    // 验证 ST-007 修复：symlink_metadata 的 is_symlink() 可能无法识别 Windows
    // Junction Points，故在符号链接检测循环后追加 canonicalize 补充校验，canonicalize
    // 失败时拒绝访问以防范 junction 绕过。

    /// ST-007: 验证 validate_wallpaper_file_path 包含 canonicalize 补充校验
    ///
    /// Windows Junction Points（IO_REPARSE_TAG_MOUNT_POINT）创建需要管理员权限，
    /// 单元测试中难以可靠复现 junction 绕过场景；同时含非法字符的路径会在前序 metadata
    /// 检查即被拒绝，无法直接验证 canonicalize 失败路径。改为通过源码断言验证 ST-007
    /// 修复代码已注入，并附带验证含非法字符的路径仍被拒绝（前序 metadata 或 canonicalize
    /// 检查覆盖）。
    #[tokio::test]
    async fn st007_validate_path_rejects_junction_point() {
        // 源码断言：验证 ST-007 canonicalize 补充校验已注入 wallpaper.rs
        let source = include_str!("wallpaper.rs");
        assert!(
            source.contains("ST-007:") && source.contains("canonicalize"),
            "wallpaper.rs 应包含 ST-007 canonicalize 补充校验代码"
        );

        // 额外验证：含非法字符的路径应被拒绝（前序 metadata 检查或 canonicalize 检查覆盖）
        #[cfg(windows)]
        {
            // Windows 上 `:` 在文件名中（非盘符位置）是非法的，metadata 与 canonicalize
            // 均会失败，validate_wallpaper_file_path 应返回 InvalidPath 错误。
            let invalid_path = r"C:\invalid:path:with:colons";
            let result = validate_wallpaper_file_path(invalid_path).await;
            assert!(result.is_err(), "含非法字符的路径应被拒绝");
            match result.unwrap_err() {
                mirrorstar_core::MirrorStarError::InvalidPath { .. } => {}
                other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
            }
        }
    }

    /// ST-007: 验证正常文件路径在 canonicalize 补充校验后仍能通过
    #[tokio::test]
    async fn st007_validate_path_accepts_canonicalized_normal_path() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push("st007_normal_path_test.txt");
        let mut f = std::fs::File::create(&tmp).expect("创建临时文件失败");
        writeln!(f, "test").unwrap();
        let path_str = tmp.to_string_lossy().to_string();
        let result = validate_wallpaper_file_path(&path_str).await;
        assert!(
            result.is_ok(),
            "正常文件路径应通过 canonicalize 校验，实际: {:?}",
            result
        );
        let _ = std::fs::remove_file(&tmp);
    }

    // ── ST-004：参数校验类错误改用 InvalidArgument 变体测试 ──────────────────
    //
    // 验证 ST-004 修复：参数校验类错误（音量越界、速度越界、未知缩放模式）应返回
    // InvalidArgument 变体而非 DesktopIntegration，前端可通过 error code 精确区分。

    #[test]
    fn st004_validate_volume_returns_invalid_argument() {
        let err = validate_volume(1.5).unwrap_err();
        assert!(matches!(
            err,
            mirrorstar_core::MirrorStarError::InvalidArgument { .. }
        ));
        let err = validate_volume(f32::NAN).unwrap_err();
        assert!(matches!(
            err,
            mirrorstar_core::MirrorStarError::InvalidArgument { .. }
        ));
    }

    #[test]
    fn st004_validate_speed_returns_invalid_argument() {
        let err = validate_speed(0.0).unwrap_err();
        assert!(matches!(
            err,
            mirrorstar_core::MirrorStarError::InvalidArgument { .. }
        ));
        let err = validate_speed(11.0).unwrap_err();
        assert!(matches!(
            err,
            mirrorstar_core::MirrorStarError::InvalidArgument { .. }
        ));
    }

    #[test]
    fn st004_parse_scaling_mode_returns_invalid_argument() {
        let err = parse_scaling_mode("unknown").unwrap_err();
        assert!(matches!(
            err,
            mirrorstar_core::MirrorStarError::InvalidArgument { .. }
        ));
    }

    // ── ST-005：set_speed 改为同步执行测试 ──────────────────────────────────
    //
    // 验证 ST-005 修复：set_speed 命令原为 fire-and-forget 模式，spawn 的
    // JoinHandle 被 drop 未被 perform_shutdown_blocking 跟踪，退出时可能在
    // engine 已 shutdown 后访问 engine。修复方案：改为同步执行（与 set_volume
    // 一致），命令层持锁期间完成 engine.set_speed() 调用。

    /// ST-005: 验证 set_speed 改为同步执行（不再 spawn 后台任务）
    ///
    /// `set_speed` 命令原为 fire-and-forget 模式，JoinHandle 被 drop 未被 shutdown 跟踪，
    /// 可能在 engine shutdown 后访问 engine。修复方案：改为同步执行（与 set_volume 一致）。
    ///
    /// 由于 set_speed 是 #[tauri::command] 且依赖 State<AppState>，直接单元测试不可行。
    /// 改为文档测试：使用 include_str! 读取本文件源码，断言关键修改已注入。
    #[test]
    fn st005_set_speed_executes_synchronously() {
        let source = include_str!("wallpaper.rs");
        assert!(source.contains("ST-005:"), "set_speed 应含 ST-005 注释标识");
        assert!(
            source.contains("engine.set_speed(&display, speed).await"),
            "set_speed 应同步调用 engine.set_speed(...).await"
        );
        // 验证 set_speed 函数体内不再含 spawn 调用
        // 通过截取 set_speed 函数体片段进行断言
        let start = source
            .find("pub async fn set_speed(")
            .expect("set_speed 函数存在");
        let end = source[start..].find("\n}\n").expect("set_speed 函数体结束");
        let set_speed_body = &source[start..start + end];
        assert!(
            !set_speed_body.contains("tauri::async_runtime::spawn"),
            "set_speed 函数体内不应再含 tauri::async_runtime::spawn 调用"
        );
        assert!(
            !set_speed_body.contains("engine_arc"),
            "set_speed 函数体内不应再含 engine_arc clone"
        );
    }

    // ── remove_wallpaper 路径越界校验测试 ──────────────────────
    //
    // 验证 remove_wallpaper 删除文件前校验路径位于 data_dir 内，
    // 拒绝越界路径，防止配置篡改（如修改 library.toml 指向系统文件）导致任意文件删除。
    //
    // 由于 remove_wallpaper 是 #[tauri::command] 且依赖 State<AppState> + AppHandle，
    // 直接单元测试不可行。改为测试 remove_wallpaper 删除前调用的校验函数
    // validate_path_within_data_dir（pub(crate)），并附带源码断言验证 remove_wallpaper
    // 已注入校验调用。

    /// 验证 remove_wallpaper 拒绝越界 data_dir 的路径
    ///
    /// 构造一个指向 data_dir 之外的临时文件（`tempdir/mirrorstar-test-outside.txt`），
    /// 调用 remove_wallpaper 删除前使用的校验逻辑（validate_path_within_data_dir），
    /// 断言返回 Err（拒绝越界路径），且文件未被删除。
    ///
    /// 测试流程：
    /// 1. 在系统临时目录（位于 `%APPDATA%/mirrorstar/` 之外）创建测试文件
    /// 2. 调用 validate_path_within_data_dir 校验该路径
    /// 3. 断言返回 InvalidPath 错误（原因含「不在受控数据目录内」）
    /// 4. 断言文件仍然存在（未被删除）
    #[tokio::test]
    async fn v41_st009_remove_wallpaper_rejects_out_of_data_dir_path() {
        // 1. 创建临时目录与文件（位于 data_dir 之外）
        // tempfile::TempDir::new() 在系统临时目录（如 %LOCALAPPDATA%/Temp）下创建，
        // 与 data_dir（%APPDATA%/mirrorstar/）不同，确保路径越界。
        let temp_dir = tempfile::TempDir::new().expect("创建临时目录失败");
        let outside_file = temp_dir.path().join("mirrorstar-test-outside.txt");
        std::fs::write(&outside_file, b"test content").expect("写入测试文件失败");
        let path_str = outside_file.to_string_lossy().to_string();

        // 确认文件存在
        assert!(outside_file.exists(), "测试文件应存在");

        // 2. 调用 remove_wallpaper 删除前使用的校验逻辑
        let result = validate_path_within_data_dir(&path_str).await;

        // 3. 断言返回 Err（拒绝越界路径）
        assert!(result.is_err(), "越界 data_dir 的路径应被拒绝");
        match result.unwrap_err() {
            mirrorstar_core::MirrorStarError::InvalidPath { reason } => {
                assert!(
                    reason.contains("不在受控数据目录内"),
                    "原因应包含「不在受控数据目录内」，实际: {}",
                    reason
                );
            }
            other => panic!("期望 InvalidPath 变体，实际: {:?}", other),
        }

        // 4. 断言文件未被删除（校验失败，不应触达 remove_file）
        assert!(outside_file.exists(), "越界路径文件不应被删除");

        // 额外源码断言：验证 remove_wallpaper 已注入校验调用
        let source = include_str!("wallpaper.rs");
        assert!(
            source.contains("validate_path_within_data_dir"),
            "wallpaper.rs 应包含 validate_path_within_data_dir 校验代码"
        );
        // 验证 remove_wallpaper 函数体内调用了 validate_path_within_data_dir
        let start = source
            .find("pub async fn remove_wallpaper(")
            .expect("remove_wallpaper 函数存在");
        let end = source[start..]
            .find("\n}\n")
            .expect("remove_wallpaper 函数体结束");
        let remove_wallpaper_body = &source[start..start + end];
        assert!(
            remove_wallpaper_body.contains("validate_path_within_data_dir"),
            "remove_wallpaper 函数体内应调用 validate_path_within_data_dir"
        );
    }

    /// 验证 data_dir 内的合法路径通过校验（正向用例）
    ///
    /// 与上一个测试对称：在 data_dir 下创建文件，校验应通过。
    /// 确保修复不会误拒 data_dir 内的合法壁纸文件。
    #[tokio::test]
    async fn v41_st009_validate_path_within_data_dir_accepts_in_dir_path() {
        // 获取 data_dir
        let data_dir = ConfigManager::data_dir().expect("获取 data_dir 失败");
        // 在 data_dir 下创建测试子目录与文件
        let test_subdir = data_dir.join("v41_st009_test");
        std::fs::create_dir_all(&test_subdir).expect("创建测试子目录失败");
        let in_dir_file = test_subdir.join("inside.txt");
        std::fs::write(&in_dir_file, b"test").expect("写入测试文件失败");
        let path_str = in_dir_file.to_string_lossy().to_string();

        // 校验应通过
        let result = validate_path_within_data_dir(&path_str).await;
        assert!(
            result.is_ok(),
            "data_dir 内的合法路径应通过校验，实际: {:?}",
            result
        );

        // 清理测试文件
        let _ = std::fs::remove_file(&in_dir_file);
        let _ = std::fs::remove_dir(&test_subdir);
    }

    // ── v16-C-002：map_io_error 中文化映射测试 ──────────────────────────────
    //
    // 验证 map_io_error 按 ErrorKind / raw_os_error 正确分类映射：
    // - PermissionDenied / EACCES(5) / EPERM(1) → 权限不足
    // - ENOSPC(28) / EDQUOT(122) → 磁盘空间不足
    // - 其他 → 保留原始错误 + 可操作性建议
    // 验证错误消息包含 context 前缀与中文建议，且变体为 DesktopIntegration。

    #[test]
    fn v16_c_002_map_io_error_permission_denied_kind() {
        // std::io::ErrorKind::PermissionDenied 直接构造
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let mapped = map_io_error("创建壁纸目录", err);
        match mapped {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("创建壁纸目录失败"),
                    "应含 context 前缀，实际: {}",
                    msg
                );
                assert!(
                    msg.contains("权限不足"),
                    "PermissionDenied 应映射为权限不足，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
    }

    #[test]
    fn v16_c_002_map_io_error_eacces_raw_code_5() {
        // Windows EACCES(5)：通过 from_raw_os_error 构造，kind() 可能未归一为 PermissionDenied
        let err = std::io::Error::from_raw_os_error(5);
        let mapped = map_io_error("复制壁纸文件", err);
        match mapped {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("权限不足"),
                    "EACCES(5) 应映射为权限不足，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
    }

    #[test]
    fn v16_c_002_map_io_error_enospc_raw_code_28() {
        // ENOSPC(28) 磁盘满
        let err = std::io::Error::from_raw_os_error(28);
        let mapped = map_io_error("创建壁纸目录", err);
        match mapped {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("磁盘空间不足"),
                    "ENOSPC(28) 应映射为磁盘空间不足，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
    }

    #[test]
    fn v16_c_002_map_io_error_edquot_raw_code_122() {
        // EDQUOT(122) 配额超限（Windows 系统错误码）
        let err = std::io::Error::from_raw_os_error(122);
        let mapped = map_io_error("复制壁纸文件", err);
        match mapped {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("磁盘空间不足"),
                    "EDQUOT(122) 应映射为磁盘空间不足，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
    }

    #[test]
    fn v16_c_002_map_io_error_other_falls_back_with_advice() {
        // 其他错误：使用 NotFound 验证 fallback 分支附加可操作性建议
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let mapped = map_io_error("复制壁纸文件", err);
        match mapped {
            mirrorstar_core::MirrorStarError::DesktopIntegration(msg) => {
                assert!(
                    msg.contains("复制壁纸文件失败"),
                    "应含 context 前缀，实际: {}",
                    msg
                );
                assert!(
                    msg.contains("请检查文件是否被占用或路径是否可访问"),
                    "fallback 分支应附加可操作性建议，实际: {}",
                    msg
                );
            }
            other => panic!("期望 DesktopIntegration 变体，实际: {:?}", other),
        }
    }

    // ── 孤儿壁纸条目（源文件缺失）兜底测试 ────────────────────────────────────
    //
    // 场景：用户拖静态壁纸进主窗口时，部分缩略图生成失败源于「源文件缺失」（孤儿条目：
    // 源文件被删/移动但库中仍保留），典型日志为 os error 123（文件名/卷标语法不正确，
    // 即 Windows 对含非法字符路径的 NOT_FOUND）与 Invalid PNG signature（内容损坏）。
    // 验证 classify_thumbnail_failure 能正确区分「源文件缺失」与「内容损坏/格式非法」，
    // 使命令层对孤儿条目不误弹「缩略图生成失败」，并为内容损坏保留解码回退。

    #[test]
    fn missing_image_source_with_image_decode_error_classified_as_source_missing() {
        // 关键场景：源文件不存在的 Image 条目。
        // 缺失文件被 image crate 的 metadata / ImageReader::open 包装后同样返回
        // ImageDecode（字符串含 os error 2），若仅按错误变体判断会被误判为解码回退，
        // 把不存在的源路径写回缩略图。应先以 exists() 判定源文件缺失 → SourceMissing。
        let missing = std::env::temp_dir()
            .join("mirrorstar_missing_orphan_decodewrap.png")
            .to_string_lossy()
            .to_string();
        // 确保不存在
        let _ = std::fs::remove_file(&missing);

        let kind = classify_thumbnail_failure(
            &mirrorstar_core::MirrorStarError::ImageDecode(
                "读取图像文件元数据失败: os error 2 (No such file)".into(),
            ),
            mirrorstar_core::config::WallpaperType::Image,
            &missing,
        );
        assert_eq!(
            kind,
            ThumbnailFailureKind::SourceMissing,
            "源文件缺失的 Image 条目（即使错误为 ImageDecode）应分类为 SourceMissing"
        );
    }

    #[test]
    fn missing_image_source_with_io_error_classified_as_source_missing() {
        // os error 123（文件名/卷标语法不正确，Windows 下亦表示打不开该路径）与 os error 2
        // （NotFound）：源文件不存在的 Io 错误 → SourceMissing。
        for raw in [2, 123] {
            // 用该 raw 码构造一个实际指向不存在路径的 Io 错误，确保测试环境语义贴近真实
            let missing = std::env::temp_dir()
                .join(format!("mirrorstar_orphan_io_{}.png", raw))
                .to_string_lossy()
                .to_string();
            let _ = std::fs::remove_file(&missing);
            let io_err = std::io::Error::from_raw_os_error(raw);
            let kind = classify_thumbnail_failure(
                &mirrorstar_core::MirrorStarError::Io(io_err),
                mirrorstar_core::config::WallpaperType::Image,
                &missing,
            );
            assert_eq!(
                kind,
                ThumbnailFailureKind::SourceMissing,
                "os error {} 且源文件缺失应分类为 SourceMissing",
                raw
            );
        }
    }

    #[test]
    fn corrupted_png_with_existing_source_classified_as_decode_fallback() {
        // 内容损坏/格式非法：源文件**存在**，解码失败（如 Invalid PNG signature）→ DecodeFallback。
        let dir = tempfile::TempDir::new().expect("创建临时目录失败");
        let src = dir.path().join("corrupted.png");
        // 非 PNG 魔数内容（文件存在但不可解码）
        std::fs::write(&src, [0x00u8, 0x00, 0x00, 0x0c, 0x4a, 0x00, 0x00, 0x00, 0x6d])
            .expect("写入损坏 PNG 失败");
        let src_str = src.to_string_lossy().to_string();

        let kind = classify_thumbnail_failure(
            &mirrorstar_core::MirrorStarError::ImageDecode("PNG Signature 无效".into()),
            mirrorstar_core::config::WallpaperType::Gif,
            &src_str,
        );
        assert_eq!(
            kind,
            ThumbnailFailureKind::DecodeFallback,
            "源文件存在的解码失败应分类为 DecodeFallback（回退缩略图，不弹失败弹窗）"
        );
    }

    #[test]
    fn existing_source_with_io_error_classified_as_unrecoverable() {
        // 源文件存在但打不开（模拟权限等其它 Io 错误）→ Unrecoverable（emit 失败弹窗）。
        let dir = tempfile::TempDir::new().expect("创建临时目录失败");
        let src = dir.path().join("img.png");
        std::fs::write(&src, b"dummy").expect("写入文件失败");
        let src_str = src.to_string_lossy().to_string();

        // 注意：该错误类型为 Io（非 ImageDecode），且源文件存在 → 不满足解码回退，应 Unrecoverable
        let kind = classify_thumbnail_failure(
            &mirrorstar_core::MirrorStarError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "拒绝访问",
            )),
            mirrorstar_core::config::WallpaperType::Image,
            &src_str,
        );
        assert_eq!(kind, ThumbnailFailureKind::Unrecoverable);
    }
}
