//! MCP server（streamable HTTP）：让 AI（Claude Code / Cursor / Cline 等）读写串口。
//!
//! 走独立 HTTP 端口，不与 TUI 抢终端；人工输入与 AI 发送共用同一 TX 队列。
//! 端点为 `http://<host>:<port>/mcp`。
//!
//! 工具集：list_ports / open / close / send / read / set_dtr_rts / export_log / restart。
//!
//! 每个 HTTP 会话创建轻量 SerialServer 包装，但它们共享同一个 Arc<SerialCore>：
//! 打开/关闭端口是整个应用的操作，不是单个客户端私有连接。
//! schemars 生成工具输入 JSON Schema，Parameters<T> 由 rmcp 完成反序列化。
//! 当前工具以普通文本返回结果，包括 `error: ...`；并未显式设置 MCP 的 isError 字段。
//! 本模块不会向 stdout 输出协议帧，HTTP 与 TUI 的终端绘制互不争抢。

use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

use axum::Router;
use rmcp::{
    ServerHandler, RoleServer,
    service::{NotificationContext, Peer},
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::codec::{decode_escapes, parse_hex};
use crate::logger::Direction;
use crate::serial_core::{SerialConfig, SerialCore};

#[derive(Clone)]
/// MCP 会话适配层，不保存第二份串口句柄或独立收发缓存。
pub struct SerialServer {
    core: Arc<SerialCore>,
    /// 共享会话登记表；所有 HTTP 会话更新同一条 TUI MCP 状态。
    sessions: Arc<ClientSessions>,
    /// rmcp 包装可能被克隆，重复 initialized 通知也只能为同一会话登记一次。
    initialized: Arc<AtomicBool>,
}

impl SerialServer {
    /// 将共享核心注入会话，克隆包装不会重新打开设备。
    fn new(core: Arc<SerialCore>, sessions: Arc<ClientSessions>) -> Self {
        Self { core, sessions, initialized: Arc::new(AtomicBool::new(false)) }
    }
}

/// 已完成 MCP initialized 握手的逻辑会话，不将 TCP 端口监听误认为客户端连接。
/// Streamable HTTP 允许请求之间没有 TCP 连接，故不能以 HTTP 请求结束判定会话离线。
/// 显式 DELETE 或 rmcp 传输关闭后清理登记；客户端异常消失且没有关闭会话时，
/// 服务端无法立即确认其存活，这个计数只表示尚未关闭的已初始化会话。
struct ClientSessions {
    core: Arc<SerialCore>,
    endpoint: String,
    clients: Mutex<Vec<(Peer<RoleServer>, String)>>,
}

impl ClientSessions {
    /// 同步发布快照，不在持锁期间进行网络 I/O。移除已关闭的 Peer，避免计数只增不减。
    fn publish(&self) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|(peer, _)| !peer.is_transport_closed());
        let status = if clients.is_empty() {
            format!("LISTENING | waiting for client | {}", self.endpoint)
        } else {
            let names = clients.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>().join(", ");
            format!("CONNECTED | sessions:{} | {} | {}", clients.len(), names, self.endpoint)
        };
        self.core.set_mcp_status(status);
    }

    /// 名称来自客户端声明，只作显示标签；限制长度并过滤控制字符，不能用作身份认证。
    fn register(&self, peer: Peer<RoleServer>) {
        let name = peer.peer_info().map(|info| info.client_info.name.clone())
            .unwrap_or_else(|| "unknown client".into());
        let name = name.chars().filter(|ch| !ch.is_control()).take(40).collect();
        self.clients.lock().unwrap().push((peer, name));
        self.publish();
    }
}

// ---- 工具参数结构 ----

/// 打开端口的显式参数；baud 字段仍是 JSON 必填项，传 0 才触发 115200 回退。
#[derive(Deserialize, schemars::JsonSchema, Default)]
struct OpenParams {
    #[schemars(description = "串口号，如 COM3")]
    port: String,
    #[schemars(description = "波特率")]
    baud: u32,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct SendParams {
    #[schemars(description = "要发送的文本（ascii 模式）")]
    #[serde(default)]
    // 允许只传 hex；不能仅依赖 derive(Default)，serde 需要字段级 default 才允许省略。
    ascii: String,
    #[schemars(description = "要发送的 hex（如 '01 03 00 00'），非空时优先")]
    hex: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ReadParams {
    #[schemars(description = "返回最近 RX 数据的最大记录条数")]
    limit: usize,
    #[schemars(description = "true 返回 ascii，false 返回 hex")]
    ascii: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct DtrRtsParams {
    /// 目标电平，不是“是否切换”；连续请求相同值保持同一电平。
    dtr: bool,
    rts: bool,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ExportParams {
    #[schemars(description = "返回的最大记录条数")]
    limit: usize,
}

// ---- 工具定义 ----

#[tool_router]
impl SerialServer {
    /// 重启整个应用而非单个会话；返回表示已安排，客户端必须重新连接新实例。
    /// 门闩在进程级共享，防止不同会话并发请求产生多个辅助进程。
    #[tool(description = "Restart this serial tool with its original arguments and working directory. MCP disconnects; reconnect after restart.")]
    fn restart(&self) -> String {
        crate::restart::request().map(str::to_owned).unwrap_or_else(|e| format!("error: {e}"))
    }

    /// 列出系统可用串口（Win11 下未占用才能打开）。
    /// 枚举成功不代表端口此刻可打开，其他进程仍可能占用设备。
    #[tool(description = "列出当前系统所有可用串口（如 COM3、COM5）")]
    fn list_ports(&self) -> String {
        match SerialCore::available_ports() {
            Ok(p) if p.is_empty() => "no serial ports found".to_string(),
            Ok(p) => format!("available: {}", p.join(", ")),
            Err(e) => format!("error: {e}"),
        }
    }

    /// 打开串口。
    /// 会替换整个应用当前连接，TUI 与其他 MCP 会话会同时看到连接变化。
    #[tool(description = "打开指定串口（如 COM3），设置波特率")]
    async fn open(&self, Parameters(p): Parameters<OpenParams>) -> String {
        let cfg = SerialConfig {
            port: p.port,
            baud: if p.baud == 0 { 115200 } else { p.baud },
        };
        match self.core.open(cfg).await {
            Ok(()) => "opened".to_string(),
            Err(e) => format!("error: {e}"),
        }
    }

    /// 关闭串口。
    /// 幂等调用；核心同时取消旧代次的自动重连。
    #[tool(description = "关闭当前串口")]
    async fn close(&self) -> String {
        self.core.close().await;
        "closed".to_string()
    }

    /// 发送数据（ascii 或 hex），走统一 TX 队列，与人工输入并行。
    /// ascii 支持转义：`\r` `\n` `\t` `\0` `\\` `\xNN`（如 `AT\r\n`）。
    /// 非空 hex 优先于 ascii；这里不追加 CRLF，调用者必须明确提供协议所需字节。
    /// 等到核心写入回执才回复成功；没有解析设备应答，因此不能声称命令已经执行。
    #[tool(description = "向串口发送数据：ascii 文本（支持 \\r \\n \\t \\0 \\xNN 转义）或 hex 帧。hex 非空时优先")]
    async fn send(&self, Parameters(p): Parameters<SendParams>) -> String {
        let bytes = match &p.hex {
            Some(h) if !h.trim().is_empty() => match parse_hex(h) {
                Ok(b) => b,
                Err(e) => return format!("error: hex parse {e}"),
            },
            _ => decode_escapes(&p.ascii),
        };
        let len = bytes.len();
        match self.core.send(bytes).await {
            Ok(()) => format!("written {len} bytes to serial port (device execution not confirmed)"),
            Err(e) => format!("error: {e}"),
        }
    }

    /// 读取最近收到的 RX 数据（从环形缓冲日志）。
    /// 这是非破坏性快照，不等待新数据，也不会清空记录；反复调用可能返回重复内容。
    /// limit 先限制所有方向的最近记录，再过滤 RX，返回的 RX 条数可能小于 limit。
    /// ascii 是按字节的可打印 ASCII 预览，中文/换行等会显示为点，并非 UTF-8 文本日志。
    #[tool(description = "读取最近从串口收到的数据（hex 或 ascii）")]
    fn read(&self, Parameters(p): Parameters<ReadParams>) -> String {
        let limit = if p.limit == 0 { 20 } else { p.limit };
        let ascii = p.ascii.unwrap_or(false);
        let recs = self.core.log_snapshot(limit);
        let mut out = String::new();
        for r in recs {
            if r.dir != Direction::Rx {
                continue; // 状态消息（Direction::Status）不进数据视图
            }
            let t = chrono::DateTime::<chrono::Local>::from(r.ts);
            if ascii {
                let text: String = r
                    .data
                    .iter()
                    .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                    .collect();
                out.push_str(&format!("[{}] {}\n", t.format("%H:%M:%S%.3f"), text));
            } else {
                let hex: Vec<String> = r.data.iter().map(|b| format!("{b:02X}")).collect();
                out.push_str(&format!("[{}] {}\n", t.format("%H:%M:%S%.3f"), hex.join(" ")));
            }
        }
        if out.is_empty() {
            "no data".to_string()
        } else {
            out
        }
    }

    /// 设置 DTR/RTS（ESP32/STM32 复位常用）。
    #[tool(description = "设置 DTR/RTS 信号线电平")]
    async fn set_dtr_rts(&self, Parameters(p): Parameters<DtrRtsParams>) -> String {
        match self.core.set_dtr_rts(p.dtr, p.rts).await {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("error: {e}"),
        }
    }

    /// 返回最近日志的摘要（时间、方向、字节数）。
    /// 当前实现不创建导出文件、不读取磁盘日志，也不返回目录；数据仍来自内存快照。
    #[tool(description = "导出日志信息：日志目录路径与最近记录摘要")]
    fn export_log(&self, Parameters(p): Parameters<ExportParams>) -> String {
        let limit = if p.limit == 0 { 10 } else { p.limit };
        let recs = self.core.log_snapshot(limit);
        let mut out = String::new();
        out.push_str("log records (recent):\n");
        for r in recs {
            let t = chrono::DateTime::<chrono::Local>::from(r.ts);
            out.push_str(&format!("[{}] {}: {} bytes\n", t.format("%Y-%m-%d %H:%M:%S%.3f"), r.dir.label(), r.data.len()));
        }
        out
    }
}

#[tool_handler]
impl ServerHandler for SerialServer {
    /// 只有客户端发送 notifications/initialized 后才算完成连接握手。
    /// 单纯探测端口或只发 initialize 请求不会显示 CONNECTED。
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if !self.initialized.swap(true, Ordering::SeqCst) {
            self.sessions.register(context.peer);
        }
    }

    /// 向客户端声明工具能力。工具路由由 tool_router/tool_handler 宏生成。
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("串口调试 MCP：可用 list_ports / open / close / send / read / set_dtr_rts / export_log / restart。")
    }
}

/// 启动并持续服务 MCP HTTP；正常情况下直到服务停止才返回，不返回地址值。
/// 监听所有 IPv4 网卡（0.0.0.0），本机客户端可用 http://127.0.0.1:<port>/mcp。
/// 只有 bind 成功后才发布 listening；端口占用等错误向上传播，由 main 发布 failed。
pub async fn run(core: Arc<SerialCore>, port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    serve(core, listener).await
}

/// 接受已绑定监听器，便于测试使用系统分配的空闲端口；正式运行仍由 run 绑定配置端口。
async fn serve(core: Arc<SerialCore>, listener: tokio::net::TcpListener) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = listener.local_addr()?;
    let sessions = Arc::new(ClientSessions {
        core: core.clone(), endpoint: format!("http://127.0.0.1:{}/mcp", addr.port()),
        clients: Mutex::new(Vec::new()),
    });
    sessions.publish();
    let service_sessions = sessions.clone();
    let service = StreamableHttpService::new(
        // 工厂按会话构造包装，共享同一核心；会话协议状态由 LocalSessionManager 管理。
        move || Ok(SerialServer::new(core.clone(), service_sessions.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = Router::new().nest_service("/mcp", service);
    tracing::info!("MCP server listening on http://{addr}/mcp");
    // 每 500ms 检查已关闭会话；与服务 future 在同一作用域，无脱离生命周期的监视任务。
    // 此检查不发送心跳，也不修改串口，取消服务时会同时停止状态检查。
    let server = async { axum::serve(listener, router).await };
    tokio::pin!(server);
    loop {
        tokio::select! {
            result = &mut server => { result?; return Ok(()); }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => sessions.publish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 测试真实本机 HTTP 协议，不操作串口；超时防止 SSE 或连接关闭错误挂住整个测试。
    async fn http(addr: std::net::SocketAddr, method: &str, session: Option<&str>, body: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let session_header = session.map(|id| format!("Mcp-Session-Id: {id}\r\n")).unwrap_or_default();
        let request = format!("{method} /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-03-26\r\n{session_header}Content-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_end(&mut response)).await.unwrap().unwrap();
        String::from_utf8(response).unwrap()
    }

    /// 只完成 initialize 响应不算已连接；该函数显式返回服务器分配的会话标识。
    async fn initialize(addr: std::net::SocketAddr) -> String {
        let response = http(addr, "POST", None, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"session-test","version":"1.0"}}}"#).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        response.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("mcp-session-id").then(|| value.trim().to_string())
        }).expect("initialize response must include session ID")
    }

    async fn wait_status(core: &SerialCore, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !core.mcp_status().contains(expected) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap_or_else(|_| panic!("expected {expected}, actual {}", core.mcp_status()));
    }

    /// 覆盖监听、未完成握手、完整握手、重复通知、多会话、DELETE 关闭后的状态回收。
    /// 这里验证逻辑会话状态，不声称能检测客户端无通知退出时的实时网络存活。
    #[tokio::test]
    async fn http_sessions_distinguish_listening_connected_and_closed() {
        let core = SerialCore::start(None);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_core = core.clone();
        let server = tokio::spawn(async move { serve(server_core, listener).await.unwrap(); });
        wait_status(&core, "waiting for client").await;
        let first = initialize(addr).await;
        assert!(core.mcp_status().starts_with("LISTENING"));
        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(http(addr, "POST", Some(&first), initialized).await.starts_with("HTTP/1.1 202"));
        wait_status(&core, "sessions:1").await;
        assert!(core.mcp_status().contains("session-test"));
        http(addr, "POST", Some(&first), initialized).await;
        let second = initialize(addr).await;
        http(addr, "POST", Some(&second), initialized).await;
        wait_status(&core, "sessions:2").await;
        assert!(http(addr, "DELETE", Some(&first), "").await.starts_with("HTTP/1.1 202"));
        wait_status(&core, "sessions:1").await;
        http(addr, "DELETE", Some(&second), "").await;
        wait_status(&core, "waiting for client").await;
        server.abort();
        let _ = server.await;
    }
}
