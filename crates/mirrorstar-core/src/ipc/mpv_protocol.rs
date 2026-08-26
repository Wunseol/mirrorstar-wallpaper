use std::time::Duration;

use crate::ipc::client::NamedPipeClient;

// 直接序列化结构体，跳过 json! 宏的中间 Value 树分配（详见各 send_command_* 内联实现）
#[derive(serde::Serialize)]
struct MpvCommand<'a> {
    command: &'a [&'a str],
    request_id: u64,
}

/// mpv IPC 命令响应
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MpvResponse {
    pub error: String,
    pub data: Option<serde_json::Value>,
    pub request_id: Option<u64>,
}

/// mpv IPC 客户端类型标记（用于 `NamedPipeClient<T>` 的类型参数）
struct MpvClient;

/// mpv IPC 客户端，通过命名管道控制 mpv 播放器
///
/// 基于 `NamedPipeClient<T>` 泛型基类的薄封装，仅实现 mpv 协议特定的
/// 命令构造与响应解析逻辑；连接/断开/读写等通用行为委托给基类。
pub struct MpvIpcClient {
    inner: NamedPipeClient<MpvClient>,
}

impl MpvIpcClient {
    /// 创建新的 IPC 客户端
    pub fn new(pipe_name: &str) -> Self {
        Self {
            inner: NamedPipeClient::new(pipe_name),
        }
    }

    /// 连接到 mpv 命名管道
    /// 重试最多 retry_count 次，每次间隔 retry_interval_ms 毫秒
    pub fn connect(
        &mut self,
        retry_count: u32,
        retry_interval_ms: u64,
    ) -> Result<(), crate::MirrorStarError> {
        self.inner.connect(retry_count, retry_interval_ms)
    }

    /// 发送命令到 mpv 并等待响应（自定义超时）
    ///
    /// 写入命令后逐行读取响应，直到找到匹配 `request_id` 的响应。
    /// 用于 get_property 等需要同步等待响应的场景下缩短超时（如 2s），
    /// 避免 mpv 端处理延迟时长时间阻塞 UI 线程（Bug #7）。
    ///
    /// I03 总体超时：`timeout` 是整个命令-响应往返（含多次读取循环）的总预算，
    /// 而非每行单次读取的超时。循环外记录 deadline，每轮将剩余时间传给
    /// `read_response_line_with_timeout`，剩余时间为 0 时返回 `IpcTimeout`，
    /// 防止 mpv 持续发送事件/不匹配响应导致累计无限阻塞。
    ///
    /// 与 `WpProcIpcClient::send_command_with_timeout` 结构对称，差异仅在命令/响应类型。
    pub fn send_command_with_timeout(
        &mut self,
        command: &[&str],
        timeout: Duration,
    ) -> Result<MpvResponse, crate::MirrorStarError> {
        let req_id = self.inner.next_request_id();
        let cmd = MpvCommand {
            command,
            request_id: req_id,
        };
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| crate::MirrorStarError::IpcError(format!("mpv 命令序列化失败: {}", e)))?;

        // 写入命令
        self.inner.send_line(&cmd_json)?;

        // I03 总体超时：deadline 为整个命令-响应往返的硬性截止时刻。
        // 每轮读取前检查剩余时间，剩余为 0 时立即返回 IpcTimeout，
        // 防止 mpv 持续发送事件或不匹配响应导致 read_response_line_with_timeout
        // 单次不超时但累计无限阻塞。
        let deadline = std::time::Instant::now() + timeout;

        // 读取响应，逐行读取直到找到匹配 request_id 的响应
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(crate::MirrorStarError::IpcTimeout(format!(
                    "mpv 命令等待响应超时 (request_id={})",
                    req_id
                )));
            }
            let line = self.inner.read_response_line_with_timeout(remaining)?;

            // 尝试解析为响应
            if let Ok(response) = serde_json::from_str::<MpvResponse>(&line) {
                if response.request_id == Some(req_id) {
                    if response.error != "success" {
                        return Err(crate::MirrorStarError::IpcError(format!(
                            "mpv 命令失败: {}",
                            response.error
                        )));
                    }
                    return Ok(response);
                }
                // request_id 不匹配，继续读取（可能是之前命令的延迟响应）
            }
            // 无法解析为响应（可能是事件），跳过
        }
    }

    /// 发送命令到 mpv，不等待响应（fire-and-forget）
    ///
    /// 仅构造 JSON（含 request_id）写入管道即返回，不读取 mpv 的 ack 响应。
    /// 适用于 set_property / pause / resume / set_volume / set_loop_file / set_speed / quit
    /// 等无需返回数据的命令，避免 mpv 端处理延迟时同步等待 5s 超时导致 UI 卡顿（Bug #7）。
    ///
    /// 注意：调用方无法感知 mpv 是否成功执行命令。若命令失败，mpv 仍会异步返回 error 响应，
    /// 但本方法不读取该响应——后续的 `send_command_with_timeout` 调用可能会读到这条延迟 error 响应
    /// 并因 request_id 不匹配而跳过。对于关键命令（如 get_property），仍应使用同步路径。
    pub fn send_command_no_wait(&mut self, command: &[&str]) -> Result<(), crate::MirrorStarError> {
        let req_id = self.inner.next_request_id();
        let cmd = MpvCommand {
            command,
            request_id: req_id,
        };
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| crate::MirrorStarError::IpcError(format!("mpv 命令序列化失败: {}", e)))?;

        // 仅写入管道即返回，不读取响应
        self.inner.send_line(&cmd_json)?;
        Ok(())
    }

    /// 获取属性值（同步等待响应，2s 超时）
    ///
    /// 保留同步路径以读取 mpv 返回的属性值；超时缩短为 2s（默认 5s 的一半），
    /// 因为 get_property 的调用方（如状态查询）通常期望快速响应。
    pub fn get_property(&mut self, name: &str) -> Result<serde_json::Value, crate::MirrorStarError> {
        self.send_command_with_timeout(&["get_property", name], Duration::from_secs(2))
            .map(|r| r.data.unwrap_or(serde_json::Value::Null))
    }

    /// 设置属性值（fire-and-forget）
    ///
    /// 改用 [`send_command_no_wait`](Self::send_command_no_wait) 避免 ack 等待（Bug #7）。
    pub fn set_property(&mut self, name: &str, value: &str) -> Result<(), crate::MirrorStarError> {
        self.send_command_no_wait(&["set_property", name, value])
    }

    /// 暂停播放（fire-and-forget）
    pub fn pause(&mut self) -> Result<(), crate::MirrorStarError> {
        self.set_property("pause", "yes")
    }

    /// 恢复播放（fire-and-forget）
    pub fn resume(&mut self) -> Result<(), crate::MirrorStarError> {
        self.set_property("pause", "no")
    }

    /// 设置音量 (0-100)（fire-and-forget）
    ///
    /// I-005：在发送给 mpv 前校验输入。`volume` 必须为有限值（非 NaN/inf）且在
    /// `[0.0, 100.0]` 范围内，越界返回 `MirrorStarError::InvalidArgument`，不发送命令。
    pub fn set_volume(&mut self, volume: f32) -> Result<(), crate::MirrorStarError> {
        if !volume.is_finite() || !(0.0..=100.0).contains(&volume) {
            return Err(crate::MirrorStarError::InvalidArgument {
                reason: format!(
                    "volume 必须在 [0.0, 100.0] 范围内且为有限值，实际: {}",
                    volume
                ),
            });
        }
        self.set_property("volume", &volume.to_string())
    }

    /// 设置循环播放（fire-and-forget）
    pub fn set_loop_file(&mut self, enabled: bool) -> Result<(), crate::MirrorStarError> {
        self.set_property("loop-file", if enabled { "yes" } else { "no" })
    }

    /// 加载视频文件（fire-and-forget）
    ///
    /// 根因 E 修复：mpv 现以 `--idle=yes` 启动（不加载任何文件），主程序将空窗口
    /// 嵌入 WorkerW 壁纸层后，再通过 IPC `loadfile` 加载视频。这确保视频纹理
    /// 在窗口已稳定嵌入后才创建，避免嵌入前创建纹理时窗口 `SetParent` 重父化 +
    /// `SetWindowPos` 缩放触发 mpv D3D11 视频输出重配置，导致 4K 纹理创建失败
    /// （`E_OUTOFMEMORY 0x8007000e`）→ 桌面黑屏。
    ///
    /// `path` 经 `serde_json` 序列化，含中文/空格/反斜杠的路径会被正确转义，
    /// 不存在命令行参数注入风险（原 v41-W-014 防护迁移到 IPC 层）。
    pub fn loadfile(&mut self, path: &str) -> Result<(), crate::MirrorStarError> {
        self.send_command_no_wait(&["loadfile", path, "replace"])
    }

    /// 设置播放速度 (0.25-4.0)（fire-and-forget）
    ///
    /// I-005：在发送给 mpv 前校验输入。`speed` 必须为有限值（非 NaN/inf）且在
    /// `[0.25, 4.0]` 范围内（与文档注释一致），越界返回 `MirrorStarError::InvalidArgument`，不发送命令。
    pub fn set_speed(&mut self, speed: f32) -> Result<(), crate::MirrorStarError> {
        if !speed.is_finite() || !(0.25..=4.0).contains(&speed) {
            return Err(crate::MirrorStarError::InvalidArgument {
                reason: format!("speed 必须在 [0.25, 4.0] 范围内且为有限值，实际: {}", speed),
            });
        }
        self.set_property("speed", &speed.to_string())
    }

    /// 请求 mpv 退出（fire-and-forget）
    ///
    /// 退出命令无需等待 ack——调用方（如 `VideoRenderer::terminate`）会在
    /// 发送后通过 `stop_process` 等待进程退出并兜底强杀，ack 等待反而可能
    /// 在 mpv 已崩溃时拖慢清理流程。
    pub fn quit(&mut self) -> Result<(), crate::MirrorStarError> {
        self.send_command_no_wait(&["quit"])
    }

    /// 断开连接
    pub fn disconnect(&mut self) {
        self.inner.disconnect();
        tracing::info!("已断开 mpv IPC 连接: {}", self.inner.pipe_path());
    }

    /// 获取管道路径
    pub fn pipe_path(&self) -> &str {
        self.inner.pipe_path()
    }
}

// I-007：Drop 双重 disconnect 幂等性说明
//
// 外层 Drop 调用 `disconnect()` 后，`self.inner`（`NamedPipeClient`）drop 时会
// 再次调用 `disconnect()`。因 `disconnect` 使用 `Option::take()` 实现幂等性，
// 第二次调用是安全 no-op。保留外层 Drop 是为记录 `tracing::info!` 断开日志
// （`NamedPipeClient::Drop` 不记录日志）。
impl Drop for MpvIpcClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MpvResponse deserialization tests ---

    #[test]
    fn mpv_response_success_null_data() {
        let json = r#"{"error":"success","data":null,"request_id":1}"#;
        let resp: MpvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "success");
        assert_eq!(resp.data, None);
        assert_eq!(resp.request_id, Some(1));
    }

    #[test]
    fn mpv_response_success_with_object_data() {
        let json = r#"{"error":"success","data":{"volume":100},"request_id":2}"#;
        let resp: MpvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "success");
        assert!(resp.data.is_some());
        let data = resp.data.unwrap();
        assert_eq!(data["volume"], 100);
        assert_eq!(resp.request_id, Some(2));
    }

    #[test]
    fn mpv_response_error_response() {
        let json = r#"{"error":"property not found","request_id":3}"#;
        let resp: MpvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "property not found");
        assert_eq!(resp.data, None);
        assert_eq!(resp.request_id, Some(3));
    }

    #[test]
    fn mpv_response_missing_request_id() {
        let json = r#"{"error":"success","data":null}"#;
        let resp: MpvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "success");
        assert_eq!(resp.data, None);
        assert_eq!(resp.request_id, None);
    }

    #[test]
    fn mpv_response_data_with_string() {
        let json = r#"{"error":"success","data":"playing","request_id":4}"#;
        let resp: MpvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "success");
        assert!(resp.data.is_some());
        let data = resp.data.unwrap();
        assert_eq!(data, serde_json::Value::String("playing".to_string()));
        assert_eq!(resp.request_id, Some(4));
    }

    // --- I-005: set_volume / set_speed 输入校验测试 ---

    #[test]
    fn set_volume_rejects_nan() {
        let mut client = MpvIpcClient::new("test-pipe");
        let result = client.set_volume(f32::NAN);
        assert!(
            matches!(result, Err(crate::MirrorStarError::InvalidArgument { .. })),
            "I-005: set_volume(NaN) 应返回 InvalidArgument，实际: {:?}",
            result
        );
    }

    #[test]
    fn set_volume_rejects_out_of_range() {
        let mut client = MpvIpcClient::new("test-pipe");
        let result = client.set_volume(150.0);
        assert!(
            matches!(result, Err(crate::MirrorStarError::InvalidArgument { .. })),
            "I-005: set_volume(150.0) 应返回 InvalidArgument，实际: {:?}",
            result
        );
    }

    #[test]
    fn set_speed_rejects_nan() {
        let mut client = MpvIpcClient::new("test-pipe");
        let result = client.set_speed(f32::NAN);
        assert!(
            matches!(result, Err(crate::MirrorStarError::InvalidArgument { .. })),
            "I-005: set_speed(NaN) 应返回 InvalidArgument，实际: {:?}",
            result
        );
    }

    #[test]
    fn set_speed_rejects_out_of_range() {
        let mut client = MpvIpcClient::new("test-pipe");
        let result = client.set_speed(10.0);
        assert!(
            matches!(result, Err(crate::MirrorStarError::InvalidArgument { .. })),
            "I-005: set_speed(10.0) 应返回 InvalidArgument，实际: {:?}",
            result
        );
    }

    // --- MpvIpcClient::new tests ---

    #[test]
    fn ipc_client_new_pipe_path() {
        let client = MpvIpcClient::new("test-pipe");
        assert_eq!(client.pipe_path(), r"\\.\pipe\test-pipe");
    }

    #[test]
    fn ipc_client_pipe_path_accessor() {
        let client = MpvIpcClient::new("my-mpv-socket");
        assert_eq!(client.pipe_path(), r"\\.\pipe\my-mpv-socket");
    }
}
