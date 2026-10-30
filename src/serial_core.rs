//! 串口核心：双向收发 + TX 队列 + DTR/RTS + Win11 自动重连 + 日志/事件分发。
//!
//! serial2 的 `SerialPort` 支持 `&self` 多线程并发读写（Windows 上亦然），
//! 因此读线程与写线程共享同一个 `Arc<SerialPort>`，无需互斥。
//! 收发走独立线程 + channel 解耦，互不阻塞。
//!
//! 数据链路：TUI/MCP → mpsc<TxRequest> → writer → 串口；串口 → reader → LogWriter。
//! 成功收发还会发到 broadcast，供界面实时显示。日志先直接入队，不能依赖广播接收端。
//! Arc 允许各层共享核心；后台线程只持有核心的 Weak，避免形成“线程让核心永不释放”的环。
//! port/config 使用 Tokio Mutex，但真正的阻塞 read/write 在取得 Arc 后执行，不持有这些锁。
//! TX 队列为无界队列，没有背压；写入成功仅说明驱动接受字节，不等于设备已执行命令。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::SystemTime;

use serial2::SerialPort;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;

use crate::logger::{Direction, LogWriter};

/// 一次串口收/发事件，供 UI 与 MCP 订阅。
#[derive(Clone, Debug)]
pub enum SerialEvent {
    /// 一次收/发数据（含方向与时间戳）。
    Data {
        ts: SystemTime,
        dir: Direction,
        data: Vec<u8>,
    },
    /// 状态变更（打开/关闭/重连/错误）。
    Status { msg: String },
}

#[derive(Clone, Debug)]
/// 当前请求使用的连接参数。只包含设备路径和波特率，其余串口设置由后端初始化。
pub struct SerialConfig {
    pub port: String,
    pub baud: u32,
}

/// 串口核心。构造即启动读写线程；`open`/`close` 控制连接。
pub struct SerialCore {
    /// TX 发送入口（人工输入、定时、MCP 都走这里）。
    tx: UnboundedSender<TxRequest>,
    mcp_status: std::sync::Mutex<String>,
    /// RX/TX/Status 广播（UI、MCP 各订阅一份）。
    events: broadcast::Sender<SerialEvent>,
    shared: Arc<Shared>,
}

/// 一次发送及其独立回执。多个调用者共享 FIFO 队列，但各自只等待自己的 oneshot 结果。
struct TxRequest {
    /// 拥有字节缓冲，避免排队期间引用调用方输入区。
    data: Vec<u8>,
    /// 写线程在成功、未连接或 I/O 失败路径发送一次；接收方取消等待不影响已排队发送。
    result: tokio::sync::oneshot::Sender<Result<(), String>>,
}

struct Shared {
    /// 当前串口句柄（None = 未连接/断开）。
    port: Mutex<Option<Arc<SerialPort>>>,
    /// 最近请求的参数，打开失败时也保留，供状态栏与后续重试使用。
    config: Mutex<SerialConfig>,
    log: Option<Arc<LogWriter>>,
    /// 每次 open/close 递增；旧的重连循环发现代次变化即放弃，避免互相覆盖。
    switch_gen: AtomicU64,
    /// 读线程是否正持有句柄阻塞读；切换串口时要等它释放，否则同口会占用冲突。
    reader_busy: AtomicUsize,
}

/// 读超时：读线程每 50ms 让出一次句柄，使 open/close 能快速接管。
const READ_TIMEOUT_MS: u64 = 50;
/// 等待读线程释放旧句柄的上限。
const READER_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

impl SerialCore {
    /// 启动核心。`log` 可选：传入则同时落盘日志。
    /// 必须在 tokio runtime 上下文中调用（获取 Handle 传给串口线程）。
    pub fn start(log: Option<Arc<LogWriter>>) -> Arc<Self> {
        let handle = tokio::runtime::Handle::current();
        let (tx, tx_rx) = tokio::sync::mpsc::unbounded_channel::<TxRequest>();
        let (events, _) = broadcast::channel::<SerialEvent>(1024);
        // 广播只保留有限事件；慢订阅者会得到 Lagged，而文件日志走独立通道。
        let shared = Arc::new(Shared {
            port: Mutex::new(None),
            config: Mutex::new(SerialConfig {
                port: String::new(),
                baud: 115200,
            }),
            log,
            switch_gen: AtomicU64::new(0),
            reader_busy: AtomicUsize::new(0),
        });

        let core = Arc::new(SerialCore { tx, events, shared, mcp_status: std::sync::Mutex::new("disabled".into()) });
        core.spawn_reader(handle.clone());
        core.spawn_writer(handle, tx_rx);
        core
    }

    /// 列出系统可用串口（如 COM3 / COM5）。Win11 下未占用才能打开。
    pub fn available_ports() -> std::io::Result<Vec<String>> {
        let ports = SerialPort::available_ports()?;
        Ok(ports
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    }

    /// 打开/切换串口。返回错误信息供上层展示。
    ///
    /// 切换顺序：先摘掉旧句柄 → 等读线程结束当前这轮读（≤1s）→ 再打开新句柄。
    /// 否则旧句柄未关就和 driver 抢占，同一 COM 口必然报占用错误（Windows sharing violation）。
    pub async fn open(&self, cfg: SerialConfig) -> Result<(), String> {
        // 先更新期望配置，再释放旧端口；失败不会回滚到旧连接。
        // switch_gen 用于取消过期切换/重连，不是覆盖整个打开过程的互斥锁。
        {
            let mut c = self.shared.config.lock().await;
            c.port = cfg.port.clone();
            c.baud = cfg.baud;
        }
        let gen_at = self.shared.switch_gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.shared.port.lock().await = None;
        let deadline = std::time::Instant::now() + READER_RELEASE_TIMEOUT;
        while self.shared.reader_busy.load(Ordering::SeqCst) != 0
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // 等待期间若有并发的 open/close，放弃本次，由最新请求接管
        if self.shared.switch_gen.load(Ordering::SeqCst) != gen_at {
            return Err("open aborted: another port switch in progress".into());
        }

        match open_port(&cfg) {
            Ok(port) => {
                *self.shared.port.lock().await = Some(Arc::new(port));
                let msg = format!("opened {} @ {} baud", cfg.port, cfg.baud);
                let _ = self.events.send(SerialEvent::Status { msg: msg.clone() });
                log_status(&self.shared, &msg);
                Ok(())
            }
            Err(e) => {
                let msg = format!("open {} failed: {e}", cfg.port);
                let _ = self.events.send(SerialEvent::Status { msg: msg.clone() });
                log_status(&self.shared, &msg);
                Err(msg)
            }
        }
    }

    /// 关闭当前串口。
    pub async fn close(&self) {
        // 递增代次使已在运行的自动重连失效，避免“用户关闭后又自动连回来”。
        // take 只释放共享句柄；读写线程已克隆的 Arc 要到其本轮 I/O 结束后才释放。
        self.shared.switch_gen.fetch_add(1, Ordering::SeqCst);
        let was = { self.shared.port.lock().await.take() };
        if was.is_some() {
            let msg = "closed".to_string();
            let _ = self.events.send(SerialEvent::Status { msg: msg.clone() });
            log_status(&self.shared, &msg);
        }
    }

    /// 入 TX 队列，并等待串口实际写入结果（不代表设备已执行命令）。
    /// 队列关闭或回执通道关闭时返回 writer stopped；调用方可据此保留未发送输入。
    /// 不在入队前承诺已连接：排队期间连接可能变化，最终检查发生在 writer 中。
    pub async fn send(&self, data: Vec<u8>) -> Result<(), String> {
        let (result, rx) = tokio::sync::oneshot::channel();
        self.tx.send(TxRequest { data, result }).map_err(|_| "serial writer stopped".to_string())?;
        rx.await.map_err(|_| "serial writer stopped".to_string())?
    }

    /// 后台 MCP 任务发布启动、监听、失败或停止状态；不会产生串口数据事件。
    pub fn set_mcp_status(&self, status: String) {
        *self.mcp_status.lock().unwrap() = status;
    }

    /// 用短临界区复制状态文本，TUI 绘制时不持锁执行格式化/终端输出。
    pub fn mcp_status(&self) -> String {
        self.mcp_status.lock().unwrap().clone()
    }

    /// 订阅所有串口事件。
    /// 只能收到订阅后的事件，不回放历史；历史读取应调用 log_snapshot。
    pub fn subscribe(&self) -> broadcast::Receiver<SerialEvent> {
        self.events.subscribe()
    }

    /// 当前串口状态三元组：(端口, 波特率, 是否已连接)。供 TUI 顶部状态栏轮询。
    pub async fn state(&self) -> (String, u32, bool) {
        let cfg = self.shared.config.lock().await;
        let connected = self.shared.port.lock().await.is_some();
        (cfg.port.clone(), cfg.baud, connected)
    }

    /// 环形缓冲因满丢弃的记录数（供 UI 状态栏展示）。
    /// 这是内存历史淘汰计数，不代表文件日志写盘失败或串口驱动丢包。
    pub fn dropped_hint(&self) -> u64 {
        self.shared.log.as_ref().map(|l| l.dropped()).unwrap_or(0)
    }

    /// 只取实际成功打开的日志文件名，轮转时由落盘线程更新共享路径。
    /// 未启用日志为 disabled；启用但还未创建/正在重新创建文件时为 pending。
    pub fn log_file_name(&self) -> String {
        match &self.shared.log {
            None => "disabled".into(),
            Some(log) => log.current_file()
                .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "pending".into()),
        }
    }

    /// 取最近 N 条日志记录快照（供 MCP 的 read/export 工具使用）。
    pub fn log_snapshot(&self, limit: usize) -> Vec<crate::logger::LogRecord> {
        self.shared
            .log
            .as_ref()
            .map(|l| l.snapshot(limit))
            .unwrap_or_default()
    }

    /// 设置 DTR/RTS 信号线（ESP32/STM32 复位常用）。
    /// 两个电平按顺序写入，不是原子操作；若 RTS 失败，DTR 可能已生效，不自动回滚。
    pub async fn set_dtr_rts(&self, dtr: bool, rts: bool) -> Result<(), String> {
        let port = { self.shared.port.lock().await.clone() };
        let Some(port) = port else {
            return Err("not connected".into());
        };
        port.set_dtr(dtr)
            .and_then(|_| port.set_rts(rts))
            .map_err(|e| e.to_string())
    }

    // ---- 内部：读写线程 ----

    /// 创建单一读取者，维护设备输入字节的先后次序。
    /// 超时只表示暂时无数据；其他读取错误触发断开和带退避的自动重连。
    /// 返回的每个数据块是一次 read 的结果，并不保证与设备的一行或协议帧对齐。
    fn spawn_reader(self: &Arc<Self>, handle: tokio::runtime::Handle) {
        let shared = self.shared.clone();
        let events = self.events.clone();
        let core_weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("serial-reader".into())
            .spawn(move || {
                let rt = handle;
                loop {
                    // 若核心已被 drop，退出
                    if core_weak.upgrade().is_none() {
                        break;
                    }
                    // 获取当前端口（clone Arc）
                    let port = rt.block_on(shared.port.lock()).clone();
                    let Some(port) = port else {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    };
                    // 阻塞读（短超时：50ms 一轮，便于 open/close 切换句柄）
                    let mut buf = [0u8; 1024];
                    shared.reader_busy.fetch_add(1, Ordering::SeqCst);
                    let res = port.read(&mut buf);
                    // 先丢弃本地句柄引用，再清 reader_busy：
                    // 否则 open() 以为串口已让出而立刻重开同一 COM 口，
                    // 此时旧句柄还没关，Windows 上会报“拒绝访问”导致 Ctrl+R 连不上。
                    drop(port);
                    shared.reader_busy.fetch_sub(1, Ordering::SeqCst);
                    match res {
                        Ok(n) if n > 0 => {
                            let data = buf[..n].to_vec();
                            let ts = SystemTime::now();
                            // 直接送往日志系统，避免 UI 广播积压导致日志也遗漏。
                            // push 是内存入队，不表示这时磁盘已持久化。
                            if let Some(log) = &shared.log {
                                log.push(Direction::Rx, data.clone());
                            }
                            let _ = events.send(SerialEvent::Data { ts, dir: Direction::Rx, data });
                        }
                        // 读超时不是故障（50ms 轮询的正常返回）；0 字节同理忽略
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                        // BrokenPipe 在 Windows 上表示设备已消失（EOF）
                        Err(e) => {
                            let msg = format!("read error: {e}");
                            let _ = events.send(SerialEvent::Status { msg: msg.clone() });
                            log_status(&shared, &msg);
                            // 置空端口，进入重连（若期间有新 open/close 请求，重连会自动放弃）
                            rt.block_on(shared.port.lock()).take();
                            reconnect(&rt, &shared, &events);
                        }
                    }
                }
            })
            .expect("spawn serial reader");
    }

    /// 串行处理所有来源的发送请求，防止人工和 MCP 在字节级交叉写入。
    /// 使用 write_all 处理短写；报错时可能已有部分字节到达设备，因此不自动重发整帧。
    /// 仅成功完整写入后记录 TX 并广播数据；失败只记录/广播状态并返回 Err 回执。
    fn spawn_writer(self: &Arc<Self>, handle: tokio::runtime::Handle, mut rx: tokio::sync::mpsc::UnboundedReceiver<TxRequest>) {
        let shared = self.shared.clone();
        let events = self.events.clone();
        let core_weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("serial-writer".into())
            .spawn(move || {
                let rt = handle;
                loop {
                    if core_weak.upgrade().is_none() {
                        break;
                    }
                    let Some(TxRequest { data, result }) = rt.block_on(rx.recv()) else {
                        break;
                    };
                    let port = rt.block_on(shared.port.lock()).clone();
                    let Some(port) = port else {
                        // 未连接：丢弃并提示（避免静默丢）
                        let msg = "send dropped: not connected".to_string();
                        log_status(&shared, &msg);
                        let _ = events.send(SerialEvent::Status { msg: msg.clone() });
                        let _ = result.send(Err(msg));
                        continue;
                    };
                    let res = port.write_all(&data);
                    match res {
                        Ok(()) => {
                            let ts = SystemTime::now();
                            if let Some(log) = &shared.log {
                                log.push(Direction::Tx, data.clone());
                            }
                            let _ = events.send(SerialEvent::Data { ts, dir: Direction::Tx, data });
                            let _ = result.send(Ok(()));
                        }
                        Err(e) => {
                            let msg = format!("write error: {e}");
                            let _ = events.send(SerialEvent::Status { msg: msg.clone() });
                            log_status(&shared, &msg);
                            let _ = result.send(Err(msg));
                        }
                    }
                }
            })
            .expect("spawn serial writer");
    }
}

/// 同步打开串口。读取/重连线程和 async open 都会调用，函数本身不启动任务。
/// Windows 兼容：serial2 固定用 `\\\\.\` 前缀打开，部分 CH340 类驱动不接受该前缀
/// （报 ERROR_FILE_NOT_FOUND 2 / ERROR_PATH_NOT_FOUND 3 / ERROR_BAD_PATHNAME 161），
/// 此时回退裸名 `COMx` 重试（Go 版 go.bug.st/serial 即裸名打开，无此问题）。
fn open_port(cfg: &SerialConfig) -> std::io::Result<SerialPort> {
    let mut port = match SerialPort::open(&cfg.port, cfg.baud) {
        Ok(p) => p,
        Err(e) if is_path_error(&e) => open_port_bare_name(cfg)?,
        Err(e) => return Err(e),
    };
    // 短读超时（50ms 轮询，见 READ_TIMEOUT_MS）：读线程能及时让出句柄给 open/close。
    port.set_read_timeout(std::time::Duration::from_millis(READ_TIMEOUT_MS))?;
    port.set_write_timeout(std::time::Duration::from_millis(3000))?;
    Ok(port)
}

/// 是否为 Windows 路径类错误（设备命名空间不可用）。
#[cfg(windows)]
fn is_path_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(2 | 3 | 161))
}

#[cfg(not(windows))]
fn is_path_error(_: &std::io::Error) -> bool {
    false
}

/// Windows 兼容回退：用裸名 `COMx` 打开（COM1-9 无需 `\\\\.\` 前缀）。
/// 句柄必须以 FILE_FLAG_OVERLAPPED 创建（serial2 的读写依赖 overlapped），
/// 并补上波特率/8N1（超时统一由 `open_port` 设置）。
#[cfg(windows)]
fn open_port_bare_name(cfg: &SerialConfig) -> std::io::Result<SerialPort> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::OwnedHandle;

    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(&cfg.port)?; // 裸名，如 COM9

    let port: SerialPort = OwnedHandle::from(file).into();
    let mut settings = port.get_configuration()?;
    settings.set_raw();
    settings.set_baud_rate(cfg.baud)?;
    let mut port = port;
    port.set_configuration(&settings)?;
    Ok(port)
}

/// 重连：读到错误（设备拔出/休眠恢复）后循环尝试 reopen，带退避。
/// 期间若发生 open/close（代次变化），立即放弃，避免和新请求争抢句柄。
fn reconnect(
    rt: &tokio::runtime::Handle,
    shared: &Arc<Shared>,
    events: &broadcast::Sender<SerialEvent>,
) {
    // 固定本轮重连的配置和代次；外部 open/close 变化后不能再使用旧配置覆盖连接。
    // 退避从 300ms 开始倍增到 3s，避免设备离线时持续占满 CPU/刷屏。
    let cfg = rt.block_on(shared.config.lock()).clone();
    let gen_at = shared.switch_gen.load(Ordering::SeqCst);
    let mut backoff = 300u64;
    loop {
        if shared.switch_gen.load(Ordering::SeqCst) != gen_at {
            return;
        }
        let msg = format!("reconnecting {} ...", cfg.port);
        let _ = events.send(SerialEvent::Status { msg });
        std::thread::sleep(std::time::Duration::from_millis(backoff));
        match open_port(&cfg) {
            Ok(port) => {
                if shared.switch_gen.load(Ordering::SeqCst) != gen_at {
                    return; // 新请求已接管，丢弃本次重连句柄
                }
                *rt.block_on(shared.port.lock()) = Some(Arc::new(port));
                let msg = format!("reconnected {} @ {} baud", cfg.port, cfg.baud);
                let _ = events.send(SerialEvent::Status { msg: msg.clone() });
                log_status(shared, &msg);
                return;
            }
            Err(_) => {
                backoff = (backoff * 2).min(3000);
            }
        }
    }
}

/// 状态事件也落盘（文本形态，Direction::Status）。
fn log_status(shared: &Arc<Shared>, msg: &str) {
    if let Some(log) = &shared.log {
        log.push(Direction::Status, msg.as_bytes().to_vec());
    }
}

#[cfg(test)]
/// 不连接真实硬件：测试未连接发送回执及 Windows 路径错误分类。
/// 这些测试不代替真实 COM 口上的收发、拔插和休眠恢复验证。
mod tests {
    use super::is_path_error;

    #[tokio::test]
    async fn disconnected_send_reports_actual_failure() {
        let core = super::SerialCore::start(None);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), core.send(b"help\r\n".to_vec())).await.unwrap();
        assert!(result.unwrap_err().contains("not connected"));
    }

    #[test]
    fn windows_path_errors_detected() {
        // 设备命名空间不可用的典型错误码
        assert!(is_path_error(&std::io::Error::from_raw_os_error(2)), "FILE_NOT_FOUND");
        assert!(is_path_error(&std::io::Error::from_raw_os_error(3)), "PATH_NOT_FOUND");
        assert!(is_path_error(&std::io::Error::from_raw_os_error(161)), "BAD_PATHNAME");
    }

    #[test]
    fn other_errors_not_matched() {
        // 占用/共享冲突等不应触发裸名回退（避免掩盖真实错误）
        assert!(!is_path_error(&std::io::Error::from_raw_os_error(5)), "ACCESS_DENIED");
        assert!(!is_path_error(&std::io::Error::from_raw_os_error(32)), "SHARING_VIOLATION");
        assert!(!is_path_error(&std::io::Error::from_raw_os_error(0)));
    }
}
