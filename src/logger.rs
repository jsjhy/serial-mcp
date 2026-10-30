//! 环形缓冲日志：防丢 + 异步落盘 + 按大小轮转 + TX/RX 方向 + 时间戳。
//!
//! 设计要点（对应需求）：
//! - 环形缓冲：UI 快照读取，超上限丢弃最旧并计数（绝不静默丢，记录 dropped）。
//! - 异步落盘：独立线程批量写，写盘慢不阻塞串口读写。
//! - 文件轮转：按单文件大小切分，按天数清理旧文件。
//! - 格式：接收为 `[时间戳] 文本`，发送保留 TX，固定 UTF-8 落盘。
//!
//! 两条独立的数据路径：push → 有界 VecDeque（供 MCP 快照）以及 push → 无界 mpsc
//! → 专用落盘线程 → TextLogFormatter → pending 文本批次 → FileWriter。
//! 内存快照保存原始字节，文件保存去除 ANSI 后的可读文本，两者不是同一种表示。
//! dropped 只统计 VecDeque 淘汰的历史记录；文件写入错误通过 tracing 单独报告。
//! 重启使用 shutdown 请求落盘线程结束并确认刷新；强制终止进程不保证尾部持久化。

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{DateTime, Local};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// 原始记录的来源分类。接收方向在界面/文本文件中省略 RX，但内部仍保留此枚举供过滤。
pub enum Direction {
    Tx,
    Rx,
    /// 状态消息（打开/关闭/重连/错误）：按文本落盘，不参与 hex 数据视图。
    Status,
}

impl Direction {
    /// 完整方向标签，用于 TX/STATUS 文本前缀和 MCP 摘要；是否省略 RX 由展示层决定。
    pub fn label(self) -> &'static str {
        match self {
            Direction::Tx => "TX",
            Direction::Rx => "RX",
            Direction::Status => "STATUS",
        }
    }
}

#[derive(Clone, Debug)]
/// 单次收发或状态记录，不要求 data 恰好是一行，亦不要求字节是合法 UTF-8。
pub struct LogRecord {
    /// 主机记录时间，不是设备内部的硬件时间戳。
    pub ts: SystemTime,
    pub dir: Direction,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LogConfig {
    /// 文件创建与轮转的目标目录，由落盘线程创建；不包含 application.log 的写入逻辑。
    pub dir: PathBuf,
    /// 单个日志文件的最大字节数，达到即轮转。
    pub rotate_size: u64,
    /// 保留天数，超期文件自动清理。
    pub keep_days: u32,
    /// 内存缓冲攒到多少字节才写一次盘（减少硬盘写动作）。
    pub flush_bytes: u64,
    /// 距上次写盘超过该时长强制落盘（保证实时性）。
    pub flush_interval: std::time::Duration,
}

/// 日志写入器：线程安全，可跨串口读写线程并发调用 `push`。
pub struct LogWriter {
    /// 环形缓冲，供 UI 等取快照。
    buffer: Arc<Mutex<VecDeque<LogRecord>>>,
    buffer_cap: usize,
    dropped: Arc<AtomicU64>,
    file_tx: Sender<FileCommand>,
    /// 跨线程公布实际已打开文件的路径；打开成功后才写入，轮转期间可暂时为 None。
    current_file: Arc<Mutex<Option<PathBuf>>>,
}

/// 单一文件队列保持记录与结束屏障的顺序；确认仅覆盖屏障前的记录。
enum FileCommand {
    Record(LogRecord),
    Shutdown(Sender<Result<(), String>>),
}

impl LogWriter {
    /// 启动日志系统，返回写入句柄。
    /// 文件采用延迟创建：首批待写文本出现前，current_file 为 None。
    /// buffer_cap 由上层提供，正常启动传入 4096；文件队列不使用此容量限制。
    pub fn start(config: LogConfig, buffer_cap: usize) -> Arc<Self> {
        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(buffer_cap)));
        let dropped = Arc::new(AtomicU64::new(0));
        let (file_tx, file_rx) = mpsc::channel::<FileCommand>();
        let writer = Arc::new(LogWriter {
            buffer,
            buffer_cap,
            dropped,
            file_tx,
            current_file: Arc::new(Mutex::new(None)),
        });

        // 落盘线程
        let current_file = writer.current_file.clone();
        let cfg2 = config.clone();
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || {
                file_writer_loop(file_rx, &cfg2, current_file);
            })
            .expect("spawn log writer");

        writer
    }

    /// 同步复制记录到内存历史并发送到落盘队列，不等待磁盘 I/O 完成。
    /// 缓冲满时淘汰最旧记录，文件线程退出时 send 失败目前不返回给调用方。
    pub fn push(&self, dir: Direction, data: Vec<u8>) {
        let rec = LogRecord {
            ts: SystemTime::now(),
            dir,
            data,
        };
        // 1) 环形缓冲
        {
            let mut buf = self.buffer.lock().unwrap();
            if buf.len() == self.buffer_cap {
                buf.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            buf.push_back(rec.clone());
        }
        // 2) 异步落盘
        let _ = self.file_tx.send(FileCommand::Record(rec));
    }

    /// 等待屏障前记录写入并刷新文件；有界等待避免磁盘故障使退出永久阻塞。
    /// 调用前应停止数据来源；后续 push 不再保证落盘。flush 不等同于断电持久化。
    pub fn shutdown(&self, timeout: std::time::Duration) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.file_tx.send(FileCommand::Shutdown(tx)).map_err(|e| e.to_string())?;
        rx.recv_timeout(timeout).map_err(|e| e.to_string())?
    }

    /// 当前已丢弃（因环形缓冲满）的记录数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 返回路径副本，避免 UI 拿着共享锁去读取文件名或绘制终端。
    pub fn current_file(&self) -> Option<PathBuf> {
        self.current_file.lock().unwrap().clone()
    }

    /// 取最近 N 条记录的快照（供 UI 显示）。
    /// 结果按旧到新排列；limit=0 返回空列表，超过容量则返回当前全部记录。
    pub fn snapshot(&self, limit: usize) -> Vec<LogRecord> {
        let buf = self.buffer.lock().unwrap();
        let skip = buf.len().saturating_sub(limit);
        buf.iter().skip(skip).cloned().collect()
    }
}

/// 落盘线程主循环：内存缓冲攒批，双阈值（字节 / 超时）触发写盘，减少硬盘写动作。
/// formatter 的解析状态独立于 pending：即使本轮写盘清空 pending，跨包 UTF-8/ANSI
/// 与未换行文本仍可继续拼接。方向切换会结束旧行，避免 TX 和 RX 粘到同一行。
fn file_writer_loop(
    rx: Receiver<FileCommand>,
    config: &LogConfig,
    current_file: Arc<Mutex<Option<PathBuf>>>,
) {
    let _ = fs::create_dir_all(&config.dir);
    let mut writer = FileWriter::new(config, current_file);
    let mut pending: Vec<u8> = Vec::new();
    let mut formatter = TextLogFormatter::default();
    let mut last_flush = std::time::Instant::now();
    let mut completion = None;
    loop {
        // 根据“上次触发写盘”的时间计算剩余等待，持续来包也不会不断重置超时。
        // saturating_sub 避免调度延迟导致负时长；到期时 recv_timeout(0) 立即检查队列。
        let wait = config.flush_interval.saturating_sub(last_flush.elapsed());
        match rx.recv_timeout(wait) {
            Ok(FileCommand::Shutdown(reply)) => { completion = Some(reply); break; }
            Ok(FileCommand::Record(rec)) => {
                pending.extend_from_slice(formatter.feed(&rec).as_bytes());
                // 字节阈值：高频场景攒够批量一次写，避免频繁小写
                if pending.len() as u64 >= config.flush_bytes || last_flush.elapsed() >= config.flush_interval {
                    flush_pending(&mut writer, config, &mut pending);
                    last_flush = std::time::Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 超时阈值：低频场景滞留数据延迟可控
                flush_pending(&mut writer, config, &mut pending);
                last_flush = std::time::Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // channel 关闭：flush 尾部
    pending.extend_from_slice(formatter.finish().as_bytes());
    flush_pending(&mut writer, config, &mut pending);
    let result = writer.flush().map_err(|e| e.to_string()).and_then(|_| {
        if pending.is_empty() { Ok(()) } else { Err("log write failed: pending data remains".into()) }
    });
    if let Err(e) = &result { tracing::error!("[log] flush error: {e}"); }
    if let Some(reply) = completion { let _ = reply.send(result); }
}

/// 内存缓冲写盘 + 轮转（写动作合并到阈值触发，减少磁盘写次数）。
/// 写盘失败时保留 pending（下轮重试），绝不静默丢弃记录。
/// 按整个批次判断轮转，不拆分单条/单批文本，因此一个较大的批次可超过 rotate_size。
/// write_all 失败前可能已写入部分字节，保留整批重试存在重复前缀的可能，并非事务写入。
fn flush_pending(writer: &mut FileWriter, config: &LogConfig, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    if writer.current_size + pending.len() as u64 > config.rotate_size {
        writer.rotate();
    }
    if let Err(e) = writer.write_bytes(pending) {
        tracing::error!("[log] write error: {e}");
        return; // 失败保留 pending，下轮 flush 重试
    }
    pending.clear();
}

/// 仅由一个落盘线程使用的文件状态，不需要为 File 本身加锁。
/// 唯一跨线程共享的是 published_path，UI 无法直接操作当前文件句柄。
struct FileWriter {
    published_path: Arc<Mutex<Option<PathBuf>>>,
    config: LogConfig,
    current: Option<File>,
    current_path: PathBuf,
    current_size: u64,
}

impl FileWriter {
    /// 只初始化状态，不做文件系统操作；真正创建发生在 write_bytes/open_new。
    fn new(config: &LogConfig, published_path: Arc<Mutex<Option<PathBuf>>>) -> Self {
        FileWriter {
            published_path,
            config: config.clone(),
            current: None,
            current_path: PathBuf::new(),
            current_size: 0,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // current_size 只在整批写成功后累加；这里的 write_all 不等于 fsync 持久化。
        if self.current.is_none() {
            self.open_new()?;
        }
        let f = self.current.as_mut().unwrap();
        f.write_all(bytes)?;
        self.current_size += bytes.len() as u64;
        Ok(())
    }

    /// 以纳秒时间命名新文件，发布路径后清理旧日志。
    /// 文件名使用本地时间，保留天数比较使用绝对时间，避免时区显示差异影响判断。
    fn open_new(&mut self) -> std::io::Result<()> {
        // 文件名纳秒精度：防止快速轮转同毫秒同名文件互相追加/覆盖
        let name = format!(
            "serial_{}.log",
            Local::now().format("%Y%m%d_%H%M%S_%9f")
        );
        let path = self.config.dir.join(name);
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        self.current = Some(f);
        self.current_path = path;
        *self.published_path.lock().unwrap() = Some(self.current_path.clone());
        self.current_size = 0;
        // 清理超期文件
        self.cleanup();
        Ok(())
    }

    /// 尽力同步并关闭旧文件，再创建新文件；创建失败由后续 write_bytes 再次尝试。
    /// 轮转函数本身不返回错误，因此 UI 的 pending 不区分首次未写与暂时创建失败。
    fn rotate(&mut self) {
        if let Some(f) = self.current.take() {
            let _ = f.sync_all();
        }
        self.current_path = PathBuf::new();
        *self.published_path.lock().unwrap() = None;
        self.current_size = 0;
        let _ = self.open_new();
    }

    /// 仅在创建新日志时执行。遍历目录中的 .log 文件，依据修改时间删除过期项。
    /// 不限于 serial_ 前缀，因此同目录 application.log 也在此规则范围内。
    /// 删除失败不阻断串口记录，keep_days=0 时完全跳过清理。
    fn cleanup(&self) {
        if self.config.keep_days == 0 {
            return;
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(self.config.keep_days as i64);
        if let Ok(entries) = fs::read_dir(&self.config.dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("log") {
                    continue;
                }
                if let Ok(meta) = fs::metadata(&p) {
                    if let Ok(mtime) = meta.modified() {
                        let mt: DateTime<Local> = mtime.into();
                        if mt < cutoff {
                            let _ = fs::remove_file(p);
                        }
                    }
                }
            }
        }
    }

    /// 刷新 Rust 文件写入层；File::flush 不承诺断电恢复，区别于 sync_all。
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.current.as_mut() {
            f.flush()
        } else {
            Ok(())
        }
    }
}

/// 用于文件文本的 ANSI 过滤状态机，不渲染颜色或执行设备清屏/移动光标指令。
/// OSC 可由 BEL 或 ESC + 反斜杠终止；状态跨原始记录保留，允许控制序列拆包。
#[derive(Default)]
enum EscapeState {
    #[default]
    Text,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// 按设备换行而非读取包边界写文本，UTF-8 与 ANSI 状态跨包保留。
#[derive(Default)]
struct TextLogFormatter {
    /// 当前输出方向。方向变化时先结束旧行、处理未完成 UTF-8 并重置控制序列状态。
    dir: Option<Direction>,
    /// 当前记录时间；仅在真正开始新行时输出一次时间戳。
    ts: Option<SystemTime>,
    /// 文件中是否已写出当前行的前缀；写盘批次结束不改变这个值。
    line_open: bool,
    /// 尚未解码的文本字节；合法前缀及时输出，只有不完整 UTF-8 尾部会跨包保留。
    utf8: Vec<u8>,
    escape: EscapeState,
    escape_len: usize,
    /// 已处理 CR 后忽略紧接的 LF，使跨包 CRLF 也只生成一个换行。
    after_cr: bool,
}

impl TextLogFormatter {
    /// 延迟输出时间戳和方向前缀，纯 ANSI 包不会单独产生一行空日志。
    fn prefix(&mut self, out: &mut String) {
        if !self.line_open {
            let ts: DateTime<Local> = self.ts.unwrap().into();
            out.push_str(&format!("[{}] ", ts.format("%Y-%m-%d %H:%M:%S%.6f")));
            let dir = self.dir.unwrap();
            if dir != Direction::Rx {
                out.push_str(dir.label());
                out.push_str(": ");
            }
            self.line_open = true;
        }
    }

    /// 尽可能消费 utf8 字节。final_chunk=false 时保留半个字符等待下一记录；
    /// true 用于方向切换、控制字符和结束，无法完成的字节转为 \xNN，避免替换成乱码。
    fn flush_utf8(&mut self, out: &mut String, final_chunk: bool) {
        while !self.utf8.is_empty() {
            match std::str::from_utf8(&self.utf8) {
                Ok(_) => {
                    self.prefix(out);
                    out.push_str(std::str::from_utf8(&self.utf8).unwrap());
                    self.utf8.clear();
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let valid = e.valid_up_to();
                    self.prefix(out);
                    out.push_str(std::str::from_utf8(&self.utf8[..valid]).unwrap());
                    self.utf8.drain(..valid);
                }
                Err(e) if e.error_len().is_none() && !final_chunk => break,
                Err(_) => {
                    self.prefix(out);
                    out.push_str(&format!("\\x{:02X}", self.utf8.remove(0)));
                }
            }
        }
    }

    /// 完成当前逻辑片段并重置过滤器；保留 dir/ts 以便调用方处理后续记录。
    /// 未终止的 ANSI 序列被丢弃，而已开始的文本行补一个换行。
    fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush_utf8(&mut out, true);
        if self.line_open { out.push('\n'); }
        self.line_open = false;
        self.escape = EscapeState::Text;
        self.escape_len = 0;
        self.after_cr = false;
        out
    }

    /// 将一条原始记录增量转换为可追加文本，不强制在记录尾部换行。
    /// 接收前缀不显示 RX；TX/STATUS 保留标签。STATUS 每条独立结束，避免影响下一状态。
    /// CR/LF 统一成 LF，Tab 保留，其余非打印控制字节使用 \xNN 表示。
    fn feed(&mut self, rec: &LogRecord) -> String {
        let mut out = String::new();
        if self.dir != Some(rec.dir) {
            out.push_str(&self.finish());
            self.dir = Some(rec.dir);
        }
        self.ts = Some(rec.ts);
        for &byte in &rec.data {
            if !matches!(self.escape, EscapeState::Text) {
                // 处于 ANSI 序列中时只推进状态，不把控制序列本体写入文件。
                // 长度上限用于从损坏/未结束序列恢复，防止永久吞掉后续设备输出。
                self.escape_len += 1;
                self.escape = match self.escape {
                    EscapeState::Esc => match byte { b'[' => EscapeState::Csi, b']' => EscapeState::Osc, _ => EscapeState::Text },
                    EscapeState::Csi if (0x40..=0x7e).contains(&byte) => EscapeState::Text,
                    EscapeState::Osc if byte == 7 => EscapeState::Text,
                    EscapeState::Osc if byte == 0x1b => EscapeState::OscEsc,
                    EscapeState::OscEsc if byte == b'\\' => EscapeState::Text,
                    EscapeState::OscEsc => EscapeState::Osc,
                    ref state => match state { EscapeState::Csi => EscapeState::Csi, _ => EscapeState::Osc },
                };
                if self.escape_len > 4096 { self.escape = EscapeState::Text; }
                continue;
            }
            if byte == 0x1b {
                self.flush_utf8(&mut out, true);
                self.escape = EscapeState::Esc;
                self.escape_len = 0;
                continue;
            }
            if byte == b'\n' && self.after_cr { self.after_cr = false; continue; }
            self.after_cr = byte == b'\r';
            match byte {
                b'\r' | b'\n' => {
                    self.flush_utf8(&mut out, true);
                    self.prefix(&mut out);
                    out.push('\n');
                    self.line_open = false;
                }
                b'\t' => {
                    self.flush_utf8(&mut out, true);
                    self.prefix(&mut out);
                    out.push('\t');
                }
                0..=0x1f | 0x7f => {
                    self.flush_utf8(&mut out, true);
                    self.prefix(&mut out);
                    out.push_str(&format!("\\x{byte:02X}"));
                }
                _ => self.utf8.push(byte),
            }
        }
        self.flush_utf8(&mut out, false);
        if rec.dir == Direction::Status { out.push_str(&self.finish()); }
        out
    }
}

#[cfg(test)]
/// 覆盖两层契约：纯格式化器的字节/换行语义，以及后台文件线程的写盘与轮转。
/// 文件线程异步运行，轮转/阈值测试轮询到截止时间，不假设 push 返回时已经落盘。
mod tests {
    use super::*;

    #[test]
    fn text_log_joins_packets_and_strips_split_ansi_without_breaking_utf8() {
        let ts = SystemTime::UNIX_EPOCH;
        let bytes = "\x1b[32m你好\x1b[0m help\r\nnext\x1b]0;title\x07> ".as_bytes();
        let mut formatter = TextLogFormatter::default();
        let mut text = String::new();
        for byte in bytes {
            text.push_str(&formatter.feed(&LogRecord { ts, dir: Direction::Rx, data: vec![*byte] }));
        }
        text.push_str(&formatter.finish());
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("] 你好 help\n"), "{text}");
        assert!(text.contains("] next> \n"), "{text}");
        assert!(!text.contains("RX"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('�'));
    }

    #[test]
    fn text_log_separates_directions_and_preserves_binary_as_escapes() {
        let ts = SystemTime::UNIX_EPOCH;
        let mut formatter = TextLogFormatter::default();
        let mut text = formatter.feed(&LogRecord { ts, dir: Direction::Rx, data: b"device> ".to_vec() });
        text.push_str(&formatter.feed(&LogRecord { ts, dir: Direction::Tx, data: b"help\r\n".to_vec() }));
        text.push_str(&formatter.feed(&LogRecord { ts, dir: Direction::Rx, data: vec![0, 0xff, 0xe4] }));
        text.push_str(&formatter.finish());
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains("] device> \n"));
        assert!(text.contains("TX: help\n"));
        assert!(text.contains("] \\x00\\xFF\\xE4\n"));
    }

    /// 使用进程号和测试名隔离目录；删除目标只限这个测试自己的临时目录。
    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("serial_mcp_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// 构造小阈值测试配置，使轮转/超时可在短时间内触发。
    fn cfg(dir: PathBuf, rotate_size: u64, flush_bytes: u64, flush_ms: u64) -> LogConfig {
        LogConfig {
            dir,
            rotate_size,
            keep_days: 1,
            flush_bytes,
            flush_interval: std::time::Duration::from_millis(flush_ms),
        }
    }

    /// 环形缓冲：满时丢最旧并计数，快照返回最新 N 条。
    #[test]
    fn ring_buffer_drops_oldest_and_counts() {
        let dir = temp_dir("ring");
        let l = LogWriter::start(cfg(dir, 1024 * 1024, 4096, 10), 3);
        for i in 0..10u8 {
            l.push(Direction::Tx, vec![i]);
        }
        let snap = l.snapshot(3);
        assert_eq!(snap.len(), 3, "snapshot should hold latest 3");
        assert_eq!(snap[2].data, vec![9u8], "newest record last");
        assert!(l.dropped() >= 7, "dropped count should reflect overflows, got {}", l.dropped());
    }

    /// 落盘：内容完整、带时间戳与 TX/RX 方向；字节阈值未达时由超时阈值触发。
    #[test]
    fn log_file_written_with_timestamp_and_direction() {
        let dir = temp_dir("file");
        let l = LogWriter::start(cfg(dir.clone(), 1024 * 1024, 1024 * 1024, 20), 1024);
        l.push(Direction::Tx, b"AB".to_vec());
        l.push(Direction::Rx, vec![0x01, 0x02]);
        // flush_bytes=1MB 未达 → 由 flush_interval=20ms 超时触发。
        // 按内容条件等待，避免静态依赖编译并行占用 CPU 时固定 200ms 产生误报。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ready = fs::read_dir(&dir).ok().into_iter().flatten().filter_map(Result::ok)
                .any(|entry| fs::read_to_string(entry.path()).unwrap_or_default().contains("] \\x01\\x02"));
            if ready || std::time::Instant::now() >= deadline { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let files: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(!files.is_empty(), "log file should exist");
        let content = fs::read_to_string(files[0].path()).unwrap();
        assert!(content.contains("TX: AB"), "missing TX line: {content}");
        assert!(content.contains("] \\x01\\x02"), "missing received text: {content}");
        assert!(content.contains('[') && content.contains(']'), "missing timestamp");
        // 时间戳精度：微秒级
        assert!(content.contains("] TX"), "bad format: {content}");
        assert_eq!(l.current_file(), Some(files[0].path()));
    }

    /// 结束屏障验证队列内尾部文本已写入并刷新，不涉及真实串口或断电恢复。
    #[test]
    fn shutdown_flushes_unterminated_tail_before_ack() {
        let dir = temp_dir("shutdown");
        let log = LogWriter::start(cfg(dir.clone(), 1024 * 1024, 1024 * 1024, 60_000), 16);
        log.push(Direction::Rx, "unfinished tail 中文".as_bytes().to_vec());
        log.shutdown(std::time::Duration::from_secs(3)).unwrap();
        let path = log.current_file().expect("shutdown must create the pending log");
        assert!(fs::read_to_string(path).unwrap().contains("unfinished tail 中文"));
        assert!(log.shutdown(std::time::Duration::from_millis(100)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    /// 轮转：小 rotate_size 下生成多个文件，且总记录数不丢。
    #[test]
    fn rotation_creates_multiple_files_no_loss() {
        let dir = temp_dir("rot");
        let l = LogWriter::start(cfg(dir.clone(), 100, 64, 10), 1024);
        for i in 0..60u8 {
            l.push(Direction::Rx, format!("record {i:02} {}\n", "A".repeat(20)).into_bytes());
        }
        // 条件轮询等待全部记录落盘（rotate=100 时每条记录轮转一个新文件，
        // 落盘线程逐个创建文件，需要时间；固定 sleep 会因机器负载 flaky）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut files: Vec<_> = vec![];
        let mut total = 0usize;
        while std::time::Instant::now() < deadline {
            files = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
            total = 0;
            for f in &files {
                total += fs::read_to_string(f.path()).unwrap_or_default().lines().count();
            }
            if total >= 60 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        files.sort_by_key(|e| e.file_name());
        assert!(files.len() >= 2, "expected rotation, got {} file(s)", files.len());
        assert_eq!(total, 60, "no records should be lost across rotation");
        assert_eq!(l.current_file(), files.last().map(|file| file.path()));
    }

    /// 字节阈值：攒够 flush_bytes 立即写盘（高频场景批量写）。
    #[test]
    fn flush_bytes_threshold_triggers_write() {
        let dir = temp_dir("fb");
        let l = LogWriter::start(cfg(dir.clone(), 1024 * 1024, 128, 10_000), 1024);
        // 每条格式化后都超过 128 字节，全部由字节阈值触发写盘。
        for i in 0..3u8 {
            l.push(Direction::Tx, format!("{i}{}\n", "A".repeat(160)).into_bytes());
        }
        // flush_interval=10s 不给超时机会：落盘只能由字节阈值路径触发。
        // 并行跑测试时落盘线程调度受负载影响，因此轮询等待而非固定 sleep。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = 0usize;
        while std::time::Instant::now() < deadline {
            lines = fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .map(|e| fs::read_to_string(e.path()).unwrap_or_default().lines().count())
                        .sum()
                })
                .unwrap_or(0);
            if lines >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(lines, 3, "byte threshold should have flushed all 3 records");
    }
}
