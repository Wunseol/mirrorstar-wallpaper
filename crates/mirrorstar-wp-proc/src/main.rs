// mirrorstar-wp-proc: Web 壁纸子进程
// 将 WebView2 渲染隔离到独立进程，通过命名管道接收控制命令
mod com;
mod command;
mod ipc_server;
mod webview;

use clap::Parser;
use com::ComGuard;
use command::handle_command;
use ipc_server::{create_pipe_server, ipc_thread, CommandWithResponse, WM_WEB_COMMAND};
use mirrorstar_core::ipc::wp_proc::WpProcCommand;
use std::sync::mpsc;
use webview::{create_webview, create_window, parse_rect, register_window_class};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Parser, Debug)]
#[command(name = "mirrorstar-wp-proc", about = "MirrorStar Web 壁纸子进程")]
struct Cli {
    /// 初始网页源（URL 或本地文件路径）
    // W-009: allow_hyphen_values 确保 `--source --malicious` 形式的分离 argv 能将 `--malicious` 解析为 source 值而非独立 flag
    #[arg(long, allow_hyphen_values = true)]
    source: String,
    /// 命名管道名称（不含 \\.\pipe\ 前缀）
    #[arg(long = "pipe-name")]
    pipe_name: String,
    /// 窗口标题（用于 FindWindowW 查找）
    #[arg(long)]
    title: String,
    /// 初始窗口位置和大小，格式 "x,y,width,height"
    #[arg(long)]
    rect: Option<String>,
}

/// wp-proc 子进程入口。
///
/// # Drop 顺序契约
///
/// WP-008 v4.0 修复将 `std::process::exit(1)` 改为 `return Err(e.into())`，让 RAII
/// guard 在函数返回时被 Drop，确保 COM 反初始化与窗口类注销被正确执行。
/// main 返回 `Err` 时 runtime 会调用 `exit(1)`，行为与原 `std::process::exit(1)`
/// 等价（父进程仍能通过非零退出码检测子进程死亡），但 Drop 顺序契约需明确：
///
/// **返回 Err 时的 Drop 顺序**（按声明逆序）：
/// 1. `class_guard` → `WindowClassGuard::drop` → 调用 `UnregisterClassW` 注销窗口类
/// 2. `_com_guard` → `ComGuard::drop` → 调用 `CoUninitialize` 平衡 COM 初始化
///
/// 该顺序由 Rust 变量声明的 LIFO（后进先出）语义保证：`_com_guard` 先声明，
/// `class_guard` 后声明，函数返回时 `class_guard` 先于 `_com_guard` 被 Drop。
///
/// 顺序正确性论证：
/// - `UnregisterClassW` 不依赖 COM 状态（仅基于 hInstance 与 class name），
///   可在 `CoUninitialize` 之前安全调用。
/// - 若顺序颠倒（先 `CoUninitialize` 后 `UnregisterClassW`），理论上也无副作用
///   （`UnregisterClassW` 不要求 COM 初始化），但当前顺序更符合"先释放资源使用者
///   再释放基础设施"的 RAII 习惯。
///
/// 注意：`controller`（`ICoreWebView2Controller`）与 `hwnd` 在错误路径已显式
/// `Close()` / `DestroyWindow()` 清理，不依赖 RAII Drop。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    tracing::info!(source = %cli.source, pipe_name = %cli.pipe_name, title = %cli.title, "mirrorstar-wp-proc 启动");
    // COM 初始化由 ComGuard 管理：任何返回路径都会通过 Drop 自动调用 CoUninitialize。
    let _com_guard = match ComGuard::new() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::error!("{}", e);
            return Err(e.into());
        }
    };

    // WP-010: parse_rect 失败时返回 Err（段数不足/非数字/尺寸非正），不再静默回退到 0x0 窗口
    let (x, y, w, h) = match parse_rect(&cli.rect) {
        Ok(rect) => rect,
        Err(e) => {
            tracing::error!("{}", e);
            return Err(e.into());
        }
    };
    // WP-002: class_guard 在下方被 .class_name() 调用且其 Drop 负责调用 UnregisterClassW，
    // 是必须保活的 RAII guard，不应使用下划线前缀（下划线前缀在 Rust 中表示"有意不使用"）。
    let class_guard = match register_window_class() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("{}", e);
            return Err(e.into());
        }
    };
    let hwnd = match create_window(class_guard.class_name(), &cli.title, x, y, w, h) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("{}", e);
            return Err(e.into());
        }
    };

    // WP03: WebView2 创建失败时退出子进程，让父进程检测到子进程死亡并上报错误，
    // 不再进入 degraded 状态静默运行（原实现 controller=None 继续运行 IPC 服务，
    // 父进程无法感知渲染失败，用户无反馈且子进程占用资源无法履行壁纸渲染职责）。
    // controller 类型为非 Option：失败时已退出，成功后必然存在，无需 Option 包装。
    // 采用方案 A：修改 handle_command 签名为 &ICoreWebView2Controller（去掉 Option），
    // 反映不变量：子进程要么有 controller 要么已退出。
    let controller: ICoreWebView2Controller = match create_webview(hwnd, &cli.source) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "WebView2 创建失败，子进程退出");
            // 清理已创建的资源：销毁窗口。
            // WP13: ShowWindow 已移到 create_webview 成功之后（见下方），此处窗口从未显示，
            // DestroyWindow 不会引起任何视觉闪现。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            // WP03: 子进程退出，让父进程检测到子进程死亡并上报错误（W07 实施后父进程可检测非零退出码）。
            // WP-008: 使用 `return Err(e.into())` 替代 `std::process::exit(1)`，触发 RAII Drop：
            // - `_com_guard.drop()` 调用 `CoUninitialize` 平衡 COM 初始化
            // - `class_guard.drop()` 调用 `UnregisterClassW` 注销窗口类
            // 行为等价：main 返回 Err 时 runtime 自动调用 `std::process::exit(1)`，父进程仍能通过非零退出码检测子进程死亡。
            return Err(e.into());
        }
    };

    // WP13: 窗口显示改由父进程 embed_wallpaper 负责（调用 ShowWindow SW_SHOW）。
    // 预热阶段（WebView2 创建、命名管道连接等）子进程不再调用 ShowWindow，窗口保持隐藏，
    // 避免窗口在内容就绪前闪现；create_webview 失败时窗口同样始终保持隐藏（测试要求 WP13）。
    // 原实现由子进程在 create_webview 成功之后调用 ShowWindow，若 create_webview 失败
    // 窗口会先显示再被 DestroyWindow 销毁，造成视觉闪现。现在该职责已移交父进程。

    let (reader, writer) = match create_pipe_server(&cli.pipe_name) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!("{}", e);
            // WP03: controller 现为非 Option，直接 Close（错误清理路径：尽力关闭，错误无实际影响）
            let _ = unsafe { controller.Close() };
            unsafe {
                // 错误清理路径：尽力销毁窗口，错误无实际影响
                let _ = DestroyWindow(hwnd);
            }
            return Err(e.into());
        }
    };
    tracing::info!("命名管道已连接，开始消息循环");

    let (cmd_tx, cmd_rx) = mpsc::channel::<CommandWithResponse>();
    // SAFETY: HWND 不是 `Send`，因为 Win32 窗口句柄与创建线程的消息循环绑定，跨线程操作可能引发未定义行为。
    // 此处通过 `hwnd.0 as usize` 绕过 `Send` 约束将 HWND 传递给 IPC 线程，安全性论证如下：
    // 1. `PostMessageW` 是线程安全的 Win32 API，可在任意线程对任意 HWND 调用，内部通过消息队列异步派发；
    // 2. 子进程内 HWND 生命周期与进程一致，主线程与 IPC 线程共享同一地址空间，无跨进程句柄复用风险；
    // 3. 主线程与 IPC 线程均只通过 `PostMessageW` 操作该 HWND，不调用需在创建线程执行的 API（如 `DestroyWindow`、`SetWindowLongPtrW` 等）；
    // 4. `DestroyWindow` 仅在主线程的 `std::process::exit` 清理路径（WP-008 已改为 `return Err`）或正常退出路径调用，IPC 线程不参与窗口销毁。
    //
    // 改进方向：未来可封装 `SendHwnd(HWND)` newtype（实现 `Send` / `Sync`），将 `unsafe`
    // 收敛到类型构造时一次，仅暴露 `post_message` 等线程安全 API，消除当前 `as usize`
    // 绕过的开发者自律依赖。当前接受现状：wp-proc 作为主进程的子进程，HWND 仅在子进程内
    // 主线程与 IPC 线程之间共享（共享地址空间，无跨进程句柄复用风险），且 IPC 线程仅通过
    // 线程安全的 `PostMessageW` 操作 HWND，不调用需在创建线程执行的 API（如 `DestroyWindow`），
    // 风险可控。在父子进程关系清晰、操作受限的场景下，开发者自律的 `as usize` 绕过可接受。
    let hwnd_raw = hwnd.0 as usize;
    // WP-003: 保存 JoinHandle 以便主线程退出时等待 IPC 线程优雅退出，
    // 避免管道写入被中途切断导致父进程读到半截 JSON。
    let ipc_handle: std::thread::JoinHandle<()> = std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        ipc_thread(reader, writer, cmd_tx, hwnd);
    });

    // 主消息循环
    let mut msg = MSG::default();
    loop {
        // GetMessageW 返回值：ret.0 == 0 为 WM_QUIT，ret.0 == -1 为错误，其他为正常消息
        // （BOOL.as_bool() 对 -1 返回 true，不能用 as_bool 判断，须显式 match ret.0）
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            0 => break, // WM_QUIT
            -1 => {
                tracing::error!("GetMessageW 返回 -1（错误），退出消息循环");
                break;
            }
            _ => {}
        }
        if msg.message == WM_WEB_COMMAND {
            let mut should_exit = false;
            while let Ok((command, resp_tx)) = cmd_rx.try_recv() {
                let is_terminate = matches!(command, WpProcCommand::Terminate { .. });
                // WP03: controller 为 ICoreWebView2Controller（非 Option），create_webview 失败时
                // 已 return Err 退出，不会进入消息循环，故到达此处时 controller 必然有效。
                // handle_command 接受 &ICoreWebView2Controller，直接传 &controller 引用。
                let response = handle_command(command, &hwnd, &controller);
                if let Err(e) = resp_tx.send(response) {
                    tracing::warn!(error = %e, "resp_tx 已关闭，响应未送达（调用方将超时）");
                }
                if is_terminate {
                    // WP10: handle_command 的 Terminate 分支已调用 DestroyWindow(hwnd)，
                    // 当 DestroyWindow 由拥有窗口的同线程调用时会同步触发 WM_DESTROY，
                    // def_window_proc 中 WM_DESTROY 处理已调用 PostQuitMessage(0)。
                    // 此处显式再次调用 PostQuitMessage(0) 作为防御性措施：确保无论
                    // WM_DESTROY 是否被同步分发，消息队列都已包含 WM_QUIT，
                    // 状态一致地退出主消息循环（break 后不再调用 GetMessageW/DispatchMessageW）。
                    unsafe {
                        PostQuitMessage(0);
                    }
                    should_exit = true;
                    break;
                }
            }
            if should_exit {
                break;
            }
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // 清理 WebView2 资源（WP03: controller 现为非 Option，直接 Close）
    tracing::debug!("关闭 WebView2 Controller");
    // 退出清理路径：进程即将退出，Controller 关闭失败无实际影响
    let _ = unsafe { controller.Close() };

    // WP-003: 通知 IPC 线程退出并等待其结束。
    // drop cmd_rx 让 ipc_thread 在下次 cmd_tx.send 时失败（若未阻塞在 read_line）；
    // 若 ipc_thread 正阻塞在 read_line（客户端未断开），drop cmd_rx 无法唤醒它，
    // 故再用带超时的 join 避免 main 永久等待——超时后由内核在进程退出时回收线程。
    drop(cmd_rx);
    // std::thread::JoinHandle 没有 timeout 方法，使用 helper thread + channel 模拟超时 join。
    let (join_tx, join_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = join_tx.send(ipc_handle.join());
    });
    match join_rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(Ok(())) => tracing::debug!("IPC 线程已正常退出"),
        Ok(Err(_panic_payload)) => tracing::warn!("IPC 线程 panic 退出"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!("IPC 线程 join 超时（1s），可能仍阻塞在管道读取；进程退出时由内核回收")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::warn!("IPC 线程 join helper 异常退出")
        }
    }

    // COM 清理由 _com_guard 的 Drop 自动完成，无需手动调用 CoUninitialize。
    tracing::info!("mirrorstar-wp-proc 退出");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cli 命令行参数解析测试 ───────────────────────────────────────────
    //
    // main() 函数本身依赖完整 Win32 消息循环（COM 初始化、窗口类注册、窗口创建、
    // WebView2 控制器创建、命名管道连接、GetMessageW 循环），无法在单元测试中
    // 隔离运行。但其使用的 `Cli` 结构体（clap::Parser derive）是纯逻辑：参数
    // 解析不依赖任何 Win32 API。
    //
    // 这些测试通过 `Cli::try_parse_from` 验证命令行参数契约，确保父进程
    // （mirrorstar 主进程）启动 wp-proc 子进程时使用的参数格式被正确解析。
    // 这本质上是 wp-proc 子进程与父进程之间的"接口契约"测试。
    //
    // main() 中其余逻辑（COM/WebView2/管道/消息循环）的覆盖应通过 src-tauri
    // 集成测试完成，不在本测试块范围内。
    //
    // WP03 验证说明：create_webview 失败时 main 返回 Err（退出码 1）的行为
    // 无法在单元测试中验证（main 依赖完整 Win32 消息循环，无法隔离运行）。其正确性
    // 依赖集成测试：父进程启动子进程 + 模拟 WebView2 创建失败 + 检测子进程退出码为非零。
    // 详见 spec SubTask 7.4（依赖 W07 子进程退出监听）。

    /// 构造最小必填参数列表（不含 --rect）
    fn minimal_args() -> Vec<String> {
        vec![
            "mirrorstar-wp-proc".to_string(),
            "--source".to_string(),
            "https://example.com".to_string(),
            "--pipe-name".to_string(),
            "test-pipe".to_string(),
            "--title".to_string(),
            "TestTitle".to_string(),
        ]
    }

    #[test]
    fn test_cli_parse_all_required_fields() {
        // 全部必填字段（source/pipe-name/title）应解析成功，rect 默认为 None
        let cli = Cli::try_parse_from(minimal_args()).expect("必填参数齐全应解析成功");
        assert_eq!(cli.source, "https://example.com");
        assert_eq!(cli.pipe_name, "test-pipe");
        assert_eq!(cli.title, "TestTitle");
        assert!(cli.rect.is_none(), "未提供 --rect 时应为 None");
    }

    #[test]
    fn test_cli_parse_with_rect_option() {
        // 提供 --rect 时应解析为 Some(String)，值原样保留（解析由 webview::parse_rect 负责）
        let mut args = minimal_args();
        args.push("--rect".to_string());
        args.push("0,0,1920,1080".to_string());
        let cli = Cli::try_parse_from(args).expect("含 --rect 应解析成功");
        assert_eq!(cli.rect.as_deref(), Some("0,0,1920,1080"));
    }

    #[test]
    fn test_cli_rect_defaults_to_none() {
        // 不提供 --rect 时，rect 字段应为 None（Option<String> 默认值）
        let cli = Cli::try_parse_from(minimal_args()).unwrap();
        assert!(cli.rect.is_none());
    }

    #[test]
    fn test_cli_missing_source_fails() {
        // 缺少 --source 应解析失败（clap 对必填字段缺失返回 Err）
        let args = vec![
            "mirrorstar-wp-proc".to_string(),
            "--pipe-name".to_string(),
            "p".to_string(),
            "--title".to_string(),
            "t".to_string(),
        ];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err(), "缺少 --source 应解析失败");
    }

    #[test]
    fn test_cli_missing_pipe_name_fails() {
        // 缺少 --pipe-name 应解析失败
        let args = vec![
            "mirrorstar-wp-proc".to_string(),
            "--source".to_string(),
            "s".to_string(),
            "--title".to_string(),
            "t".to_string(),
        ];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err(), "缺少 --pipe-name 应解析失败");
    }

    #[test]
    fn test_cli_missing_title_fails() {
        // 缺少 --title 应解析失败
        let args = vec![
            "mirrorstar-wp-proc".to_string(),
            "--source".to_string(),
            "s".to_string(),
            "--pipe-name".to_string(),
            "p".to_string(),
        ];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err(), "缺少 --title 应解析失败");
    }

    #[test]
    fn test_cli_source_accepts_url() {
        // source 字段可接收 http/https URL（父进程启动壁纸子进程的常见场景）
        let mut args = minimal_args();
        args[2] = "http://localhost:8080/wallpaper".to_string();
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.source, "http://localhost:8080/wallpaper");
    }

    #[test]
    fn test_cli_source_accepts_file_path() {
        // source 字段可接收本地文件路径（父进程启动壁纸子进程的常见场景）
        let mut args = minimal_args();
        args[2] = r"C:\Users\test\wallpaper.html".to_string();
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.source, r"C:\Users\test\wallpaper.html");
    }

    #[test]
    fn test_cli_unknown_arg_fails() {
        // 未知参数应解析失败（clap 默认拒绝未知参数）
        let mut args = minimal_args();
        args.push("--unknown-flag".to_string());
        args.push("value".to_string());
        let result = Cli::try_parse_from(args);
        assert!(result.is_err(), "未知参数应解析失败");
    }

    #[test]
    fn test_cli_rect_value_passed_through_unparsed() {
        // rect 字段仅作为字符串原样保留，格式校验由 webview::parse_rect 完成。
        // 此测试验证即使是"看起来非法"的 rect 值，Cli 也会接受（解析与校验职责分离）。
        let mut args = minimal_args();
        args.push("--rect".to_string());
        args.push("invalid,rect,format".to_string());
        let cli = Cli::try_parse_from(args).expect("Cli 不校验 rect 格式，应解析成功");
        assert_eq!(cli.rect.as_deref(), Some("invalid,rect,format"));
    }

    // ── W-009 参数注入防护：分离 argv 兼容性验证 ──────────────────────────
    //
    // W-009 修复（web.rs `build_wp_proc_args`）将 `--source=value` 拼接形式改为
    // 分离 argv（`--source` 与值作为两个独立元素）。以下测试验证 wp-proc 的 clap
    // 解析器能否正确处理分离 argv，包括值以 `--` 开头的边界场景。
    //
    // wp-proc 的 `source` 字段已设置 `allow_hyphen_values = true`（见上方 Cli 定义），
    // 支持 `--source --malicious` 这类"值以 -- 开头"的分离 argv。

    /// 验证分离 argv 形式（`--source` 与值分开）能被正确解析（W-009）。
    #[test]
    fn w009_cli_parse_source_separated_argv() {
        // `minimal_args()` 已使用分离 argv 形式，验证正常值能被解析
        let cli = Cli::try_parse_from(minimal_args()).expect("分离 argv 应解析成功");
        assert_eq!(cli.source, "https://example.com");
    }

    /// 验证 source 值以 `--` 开头时，分离 argv 能被正确解析（W-009 核心场景，
    /// `allow_hyphen_values = true` 已启用）。
    #[test]
    fn w009_cli_parse_source_starting_with_dash_separated() {
        let mut args = minimal_args();
        args[2] = "--malicious".to_string();
        let cli = Cli::try_parse_from(args).expect(
            "source 以 `--` 开头的分离 argv 应被解析为 source 值；\
             若失败需在 wp-proc 的 source 字段添加 `allow_hyphen_values = true`",
        );
        assert_eq!(cli.source, "--malicious");
    }
}
