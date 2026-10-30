//! Windows 原生串口界面：User32 窗口、系统 RichEdit 文本区和原生按钮/组合框。
//!
//! UI 线程独占 HWND、GDI 字体与 UTF-16 选区坐标；Tokio 只操作共享串口核心，
//! 通过通道传回快照/发送回执，绝不从工作线程直接改控件。串口广播按帧限量消费。
//! RichEdit 前半段为只读历史，末尾为可编辑命令；接收内容插在草稿之前并恢复选区、
//! 滚动位置，鼠标选取历史时暂停显示更新。文件日志由原有独立线程继续写入。
//! 这里不是完整终端模拟器；复用 ANSI 增量解析器，仅显示颜色与文本，不执行设备 CSI。

#![allow(unsafe_op_in_unsafe_fn)]

use crate::{
    ansi::AnsiState,
    codec::{decode_escapes, parse_hex},
    config::{LineEnding, Theme, TuiConfig},
    logger::Direction,
    serial_core::{SerialConfig, SerialCore, SerialEvent},
};
use ratatui::style::Color;
use std::{
    io,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{Arc, mpsc},
    time::SystemTime,
};
use tokio::{runtime::Handle, sync::broadcast};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*},
    System::LibraryLoader::*,
    UI::{Controls::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const PORT: usize = 101;
const BAUD: usize = 102;
const CONNECT: usize = 103;
const DISCONNECT: usize = 104;
const REFRESH: usize = 105;
const MODE: usize = 106;
const EOL: usize = 107;
const CLEAR: usize = 108;
const TERMINAL: usize = 109;
const EM_EXGETSEL_: u32 = WM_USER + 52;
const EM_EXSETSEL_: u32 = WM_USER + 55;
const EM_SETCHARFORMAT_: u32 = WM_USER + 68;
const EM_SETBKGNDCOLOR_: u32 = WM_USER + 67;
const EM_GETTEXTEX_: u32 = WM_USER + 94;
const EM_GETSCROLLPOS_: u32 = WM_USER + 221;
const EM_SETSCROLLPOS_: u32 = WM_USER + 222;

#[repr(C)]
#[derive(Default, Clone, Copy)]
/// RichEdit 原生 UTF-16 范围；与 Rust 字节偏移不同，不可混用。
struct Range {
    start: i32,
    end: i32,
}
#[repr(C)]
/// 与 Windows SDK CHARFORMATW ABI 对齐，只设置文字前景，不改变文件日志。
struct CharFormat {
    size: u32,
    mask: u32,
    effects: u32,
    height: i32,
    offset: i32,
    color: u32,
    charset: u8,
    pitch: u8,
    face: [u16; 32],
}
#[repr(C)]
struct GetText {
    bytes: u32,
    flags: u32,
    codepage: u32,
    default_char: *const u8,
    used_default: *mut i32,
}

/// 统一将系统 API 输入转成零结尾 UTF-16；内嵌 NUL 在串口显示中由解析器过滤。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn units(text: &str) -> i32 {
    text.encode_utf16().count() as i32
}
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
}

#[derive(Clone, Copy)]
/// COLORREF 为 0x00BBGGRR；时间戳用前景与背景中点，约降低一半对比度。
struct Palette {
    fg: u32,
    bg: u32,
    dim: u32,
}
impl Palette {
    fn new(theme: Theme) -> Self {
        let (fg, bg) = match theme {
            Theme::Light => (rgb(32, 36, 42), rgb(250, 250, 250)),
            Theme::Nord => (rgb(216, 222, 233), rgb(46, 52, 64)),
            Theme::Solarized => (rgb(131, 148, 150), rgb(0, 43, 54)),
            Theme::Dark => (rgb(220, 224, 232), rgb(20, 23, 29)),
        };
        let avg = |shift: u32| (((fg >> shift) & 255u32) + ((bg >> shift) & 255u32)) / 2u32;
        Self {
            fg,
            bg,
            dim: avg(0) | avg(8) << 8 | avg(16) << 16,
        }
    }
    fn color(self, color: Option<Color>) -> u32 {
        match color {
            Some(Color::Rgb(r, g, b)) => rgb(r, g, b),
            Some(Color::Red | Color::LightRed) => rgb(235, 105, 105),
            Some(Color::Green | Color::LightGreen) => rgb(110, 200, 140),
            Some(Color::Yellow | Color::LightYellow) => rgb(225, 195, 110),
            Some(Color::Blue | Color::LightBlue) => rgb(115, 165, 235),
            Some(Color::Magenta | Color::LightMagenta) => rgb(195, 140, 220),
            Some(Color::Cyan | Color::LightCyan) => rgb(110, 200, 215),
            Some(Color::DarkGray) => self.dim,
            Some(Color::Black) => rgb(30, 30, 30),
            Some(Color::Indexed(index)) => {
                let index = index as u32;
                if index >= 232 {
                    let v = (8 + (index - 232) * 10) as u8;
                    rgb(v, v, v)
                } else if index >= 16 {
                    let index = index - 16;
                    let channel = |v| if v == 0 { 0 } else { (55 + v * 40) as u8 };
                    rgb(
                        channel(index / 36),
                        channel(index / 6 % 6),
                        channel(index % 6),
                    )
                } else {
                    let level = if index >= 8 { 255 } else { 170 };
                    rgb(
                        if index & 1 != 0 { level } else { 0 },
                        if index & 2 != 0 { level } else { 0 },
                        if index & 4 != 0 { level } else { 0 },
                    )
                }
            }
            _ => self.fg,
        }
    }
}

/// 一次可增量应用的界面补丁：删除上次未换行快照，再插入带颜色的最新文本。
#[derive(Default)]
struct Patch {
    remove: i32,
    spans: Vec<(String, u32)>,
}
/// 跨串口包保留 ANSI/UTF-8 解析状态；partial 以 RichEdit 的 UTF-16 单位计数。
struct DisplayStream {
    ansi: AnsiState,
    partial: i32,
    partial_time: Option<SystemTime>,
}
impl Default for DisplayStream {
    fn default() -> Self {
        Self {
            ansi: AnsiState::new(),
            partial: 0,
            partial_time: None,
        }
    }
}
impl DisplayStream {
    fn event(&mut self, event: SerialEvent, hex: bool, palette: Palette) -> Patch {
        let mut patch = Patch::default();
        let SerialEvent::Data { ts, dir, data } = event else {
            return patch;
        };
        let timestamp = |time| {
            format!(
                "[{}] ",
                chrono::DateTime::<chrono::Local>::from(time).format("%H:%M:%S%.3f")
            )
        };
        if dir == Direction::Rx && !hex {
            let lines = self.ansi.feed(&data);
            if lines.is_empty() {
                return patch;
            }
            patch.remove = self.partial;
            let last = lines.len() - 1;
            for (i, line) in lines.into_iter().enumerate() {
                let time = if i == 0 {
                    self.partial_time.unwrap_or(ts)
                } else {
                    ts
                };
                let mut count = 0;
                let prefix = timestamp(time);
                count += units(&prefix);
                patch.spans.push((prefix, palette.dim));
                for span in line.spans {
                    count += units(&span.content);
                    patch
                        .spans
                        .push((span.content.into_owned(), palette.color(span.style.fg)));
                }
                if i == last && self.ansi.partial_line() {
                    self.partial = count;
                    self.partial_time = Some(time);
                } else {
                    patch.spans.push(("\r".into(), palette.fg));
                    self.partial = 0;
                    self.partial_time = None;
                }
            }
        } else {
            if self.partial > 0 {
                patch.spans.push(("\r".into(), palette.fg));
            }
            self.ansi.reset_line();
            self.partial = 0;
            self.partial_time = None;
            patch.spans.push((timestamp(ts), palette.dim));
            let prefix = if dir == Direction::Tx { "TX: " } else { "" };
            let text = if hex {
                data.iter()
                    .map(|v| format!("{v:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                String::from_utf8_lossy(&data)
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .collect()
            };
            patch.spans.push((format!("{prefix}{text}\r"), palette.fg));
        }
        patch
    }
}

/// 工作线程只发送拥有数据的消息，绝不跨线程共享窗口指针或 GDI 资源。
enum Reply {
    State(String, u32, bool),
    Ports(Result<Vec<String>, String>),
    Sent(Result<(), String>),
    Notice(String),
}
struct App {
    core: Arc<SerialCore>,
    handle: Handle,
    events: broadcast::Receiver<SerialEvent>,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
    window: HWND,
    terminal: HWND,
    port: HWND,
    baud: HWND,
    status: HWND,
    log_status: HWND,
    mcp: HWND,
    buttons: Vec<(usize, HWND)>,
    font: HFONT,
    mono: HFONT,
    old_edit_proc: WNDPROC,
    /// 历史与草稿的 UTF-16 边界；所有程序插入必须调整它，并禁止用户跨边界删除。
    input_start: i32,
    stream: DisplayStream,
    palette: Palette,
    hex: bool,
    eol: LineEnding,
    ports: Vec<String>,
    ports_known: bool,
    current_port: String,
    current_baud: u32,
    connected: bool,
    notice: String,
    pending: bool,
    max_chars: i32,
}

impl App {
    /// 控件创建后设置子类过程。系统拥有窗口，App 拥有两支字体；销毁窗口后才释放字体。
    unsafe fn create(&mut self) -> io::Result<()> {
        self.font = CreateFontW(
            -16,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Segoe UI").as_ptr(),
        );
        self.mono = CreateFontW(
            -17,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Cascadia Mono").as_ptr(),
        );
        self.port = self.control(
            "COMBOBOX",
            "",
            CBS_DROPDOWN as u32 | WS_VSCROLL | WS_TABSTOP,
            PORT,
        )?;
        self.baud = self.control(
            "COMBOBOX",
            "115200",
            CBS_DROPDOWN as u32 | WS_VSCROLL | WS_TABSTOP,
            BAUD,
        )?;
        for value in [
            "9600", "19200", "38400", "57600", "115200", "230400", "460800", "921600",
        ] {
            SendMessageW(self.baud, CB_ADDSTRING, 0, wide(value).as_ptr() as isize);
        }
        set_text(self.baud, &self.current_baud.to_string());
        set_text(self.port, &self.current_port);
        for (id, label) in [
            (CONNECT, "Connect"),
            (DISCONNECT, "Disconnect"),
            (REFRESH, "Refresh"),
            (MODE, "ASCII"),
            (EOL, "EOL: CRLF"),
            (CLEAR, "Clear"),
        ] {
            let hwnd = self.control("BUTTON", label, WS_TABSTOP | BS_PUSHBUTTON as u32, id)?;
            self.buttons.push((id, hwnd));
        }
        // SS_LEFTNOWORDWRAP：连接错误再长也不挤掉独立的日志文件名一行。
        self.status = self.control("STATIC", "", 0x000c, 0)?;
        self.log_status = self.control("STATIC", "", 0x000c, 0)?;
        self.mcp = self.control("STATIC", "", 0, 0)?;
        self.terminal = self.control(
            "RICHEDIT50W",
            "",
            WS_TABSTOP
                | WS_VSCROLL
                | WS_HSCROLL
                | ES_MULTILINE as u32
                | ES_AUTOVSCROLL as u32
                | ES_AUTOHSCROLL as u32
                | ES_NOHIDESEL as u32,
            TERMINAL,
        )?;
        SendMessageW(self.terminal, WM_SETFONT, self.mono as usize, 0);
        SendMessageW(self.terminal, EM_SETLIMITTEXT, 4_000_000, 0);
        SendMessageW(
            self.terminal,
            EM_SETBKGNDCOLOR_,
            0,
            self.palette.bg as isize,
        );
        // 关闭自动换行，保留原串口行与水平滚动条；单位是 RichEdit twips。
        SendMessageW(self.terminal, WM_USER + 72, 0, 1);
        set_color(self.terminal, self.palette.fg);
        SetWindowLongPtrW(self.terminal, GWLP_USERDATA, self as *mut Self as isize);
        self.old_edit_proc = std::mem::transmute::<isize, WNDPROC>(SetWindowLongPtrW(
            self.terminal,
            GWLP_WNDPROC,
            edit_proc as *const () as isize,
        ));
        self.labels();
        SetTimer(self.window, 1, 33, None);
        self.layout();
        SetFocus(self.terminal);
        Ok(())
    }

    unsafe fn control(&self, class: &str, text: &str, style: u32, id: usize) -> io::Result<HWND> {
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            100,
            28,
            self.window,
            id as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        SendMessageW(hwnd, WM_SETFONT, self.font as usize, 0);
        Ok(hwnd)
    }

    /// 逻辑布局使用客户区像素；窗口最小尺寸保证工具栏与两条状态栏有固定空间。
    unsafe fn layout(&self) {
        let mut rect: RECT = zeroed();
        GetClientRect(self.window, &mut rect);
        let w = rect.right;
        let h = rect.bottom;
        MoveWindow(self.port, 12, 12, 180, 300, 1);
        MoveWindow(self.baud, 202, 12, 110, 300, 1);
        let mut x = 322;
        for (id, hwnd) in &self.buttons {
            let width = if *id == DISCONNECT { 100 } else { 92 };
            MoveWindow(*hwnd, x, 10, width, 30, 1);
            x += width + 8;
        }
        MoveWindow(self.status, 12, 50, w - 24, 24, 1);
        MoveWindow(self.log_status, 12, 76, w - 24, 24, 1);
        MoveWindow(self.terminal, 12, 106, w - 24, (h - 148).max(20), 1);
        MoveWindow(self.mcp, 12, h - 32, w - 24, 24, 1);
    }

    unsafe fn labels(&self) {
        for (id, hwnd) in &self.buttons {
            if *id == MODE {
                set_text(*hwnd, if self.hex { "HEX" } else { "ASCII" });
            }
            if *id == EOL {
                set_text(*hwnd, &format!("EOL: {}", self.eol.label()));
            }
        }
        let state = if self.connected {
            "CONNECTED"
        } else if self.ports_known
            && !self
                .ports
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&self.current_port))
        {
            "NOT_FOUND"
        } else {
            "DISCONNECTED"
        };
        set_text(
            self.status,
            &format!(
                "{} @ {} | {} {}",
                self.current_port, self.current_baud, state, self.notice
            ),
        );
        set_text(
            self.log_status,
            &format!(
                "log:{} | input/view:{} | dropped:{}",
                self.core.log_file_name(),
                if self.hex { "hex" } else { "ascii" },
                self.core.dropped_hint()
            ),
        );
        set_text(self.mcp, &format!("MCP: {}", self.core.mcp_status()));
    }

    /// 限制每帧广播消费量；选中历史时保持字符位置，后台日志和 MCP 继续运行。
    unsafe fn tick(&mut self) {
        if crate::restart::ready() {
            PostMessageW(self.window, WM_CLOSE, 0, 0);
            return;
        }
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                Reply::State(port, baud, connected) => {
                    self.current_port = port;
                    self.current_baud = baud;
                    self.connected = connected;
                }
                Reply::Ports(result) => match result {
                    Ok(ports) => {
                        self.ports_known = true;
                        if self.ports != ports {
                            let selected = text(self.port);
                            SendMessageW(self.port, CB_RESETCONTENT, 0, 0);
                            for port in &ports {
                                SendMessageW(
                                    self.port,
                                    CB_ADDSTRING,
                                    0,
                                    wide(port).as_ptr() as isize,
                                );
                            }
                            set_text(
                                self.port,
                                if selected.is_empty() {
                                    ports.first().map(String::as_str).unwrap_or("")
                                } else {
                                    &selected
                                },
                            );
                            self.ports = ports;
                        }
                    }
                    Err(error) => {
                        self.ports_known = false;
                        self.notice = error;
                    }
                },
                Reply::Sent(result) => {
                    self.pending = false;
                    match result {
                        Ok(()) => {
                            self.replace_draft("");
                            self.notice = "written to serial driver".into();
                        }
                        Err(error) => self.notice = error,
                    }
                }
                Reply::Notice(notice) => self.notice = notice,
            }
        }
        self.labels();
        let selection = selection(self.terminal);
        if selection.start != selection.end && selection.start < self.input_start {
            return;
        }
        for _ in 0..256 {
            match self.events.try_recv() {
                Ok(SerialEvent::Status { msg }) => self.notice = msg,
                Ok(event) => {
                    let patch = self.stream.event(event, self.hex, self.palette);
                    self.apply(patch);
                }
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    self.stream = DisplayStream::default();
                    self.apply(Patch {
                        remove: 0,
                        spans: vec![(
                            format!("\r[display skipped {count} events; see file log]\r"),
                            self.palette.dim,
                        )],
                    });
                }
                Err(_) => break,
            }
        }
    }

    /// 程序插入不进入撤销栈；恢复选区和滚动位置，输入尾部则自动跟随。
    unsafe fn apply(&mut self, patch: Patch) {
        if patch.spans.is_empty() && patch.remove == 0 {
            return;
        }
        let old = self.input_start;
        let selected = selection(self.terminal);
        let mut scroll: POINT = zeroed();
        SendMessageW(
            self.terminal,
            EM_GETSCROLLPOS_,
            0,
            &mut scroll as *mut _ as isize,
        );
        let follow = selected.start >= old;
        SendMessageW(self.terminal, WM_SETREDRAW, 0, 0);
        select(self.terminal, old - patch.remove.min(old), old);
        self.input_start -= patch.remove.min(old);
        for (content, color) in patch.spans {
            set_color(self.terminal, color);
            SendMessageW(
                self.terminal,
                EM_REPLACESEL,
                0,
                wide(&content).as_ptr() as isize,
            );
            self.input_start += units(&content);
        }
        set_color(self.terminal, self.palette.fg);
        let delta = self.input_start - old;
        let shift = |v: i32| {
            if v >= old {
                v + delta
            } else {
                v.min(self.input_start)
            }
        };
        select(self.terminal, shift(selected.start), shift(selected.end));
        // 限制历史保留字符数；按整行截断，不拆 UTF-16 代理对或当前未结束行。
        if self.input_start > self.max_chars {
            let all = rich_text(self.terminal);
            let limit = (self.input_start - self.max_chars / 2) as usize;
            if let Some(cut) = all
                .iter()
                .enumerate()
                .skip(limit)
                .find_map(|(i, ch)| (*ch == 13).then_some(i + 1))
            {
                let saved = selection(self.terminal);
                select(self.terminal, 0, cut as i32);
                SendMessageW(self.terminal, EM_REPLACESEL, 0, wide("").as_ptr() as isize);
                self.input_start -= cut as i32;
                select(
                    self.terminal,
                    (saved.start - cut as i32).max(0),
                    (saved.end - cut as i32).max(0),
                );
            }
        }
        SendMessageW(self.terminal, EM_EMPTYUNDOBUFFER, 0, 0);
        if follow {
            SendMessageW(self.terminal, EM_SCROLLCARET, 0, 0);
        } else {
            SendMessageW(
                self.terminal,
                EM_SETSCROLLPOS_,
                0,
                &scroll as *const _ as isize,
            );
        }
        SendMessageW(self.terminal, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.terminal, null(), 0);
    }

    unsafe fn draft(&self) -> String {
        let all = rich_text(self.terminal);
        String::from_utf16_lossy(&all[(self.input_start as usize).min(all.len())..])
    }
    unsafe fn replace_draft(&self, value: &str) {
        select(self.terminal, self.input_start, -1);
        set_color(self.terminal, self.palette.fg);
        SendMessageW(
            self.terminal,
            EM_REPLACESEL,
            0,
            wide(value).as_ptr() as isize,
        );
        SendMessageW(self.terminal, EM_EMPTYUNDOBUFFER, 0, 0);
    }
    unsafe fn clear(&mut self) {
        let draft = self.draft();
        set_text(self.terminal, "");
        self.input_start = 0;
        self.stream = DisplayStream::default();
        self.replace_draft(&draft);
    }

    /// 回车只入核心统一发送队列；等待真实写入回执期间保留草稿，失败不清空。
    unsafe fn send(&mut self) {
        if self.pending {
            return;
        }
        let draft = self.draft();
        let bytes = match command_bytes(&draft, self.hex, self.eol) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        if bytes.is_empty() {
            return;
        }
        if !self.connected {
            self.notice = "not connected; choose a port and Connect".into();
            return;
        }
        self.pending = true;
        let core = self.core.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let _ = tx.send(Reply::Sent(core.send(bytes).await));
        });
    }

    unsafe fn command(&mut self, id: usize) {
        match id {
            CONNECT => {
                let port = text(self.port);
                let baud = text(self.baud).parse::<u32>().ok().filter(|v| *v > 0);
                let Some(baud) = baud else {
                    self.notice = "invalid baud rate".into();
                    return;
                };
                if port.trim().is_empty() {
                    self.notice = "select a serial port".into();
                    return;
                }
                let core = self.core.clone();
                let tx = self.tx.clone();
                self.handle.spawn(async move {
                    let result = core.open(SerialConfig { port, baud }).await;
                    let _ = tx.send(Reply::Notice(
                        result.err().unwrap_or_else(|| "connected".into()),
                    ));
                });
            }
            DISCONNECT => {
                let core = self.core.clone();
                self.handle.spawn(async move {
                    core.close().await;
                });
            }
            REFRESH => {
                let tx = self.tx.clone();
                self.handle.spawn_blocking(move || {
                    let _ = tx.send(Reply::Ports(
                        SerialCore::available_ports().map_err(|e| e.to_string()),
                    ));
                });
            }
            MODE => {
                self.hex = !self.hex;
                self.stream = DisplayStream::default();
                self.apply(Patch {
                    remove: 0,
                    spans: vec![("\r".into(), self.palette.fg)],
                });
            }
            EOL => self.eol = self.eol.next(),
            CLEAR => self.clear(),
            _ => {}
        }
        self.labels();
        SetFocus(self.terminal);
    }
}

/// 输入编码纯函数，与 GUI 消息循环分开测试；Hex 不追加行尾，ASCII 保持 UTF-8 字节。
fn command_bytes(input: &str, hex: bool, eol: LineEnding) -> Result<Vec<u8>, String> {
    if hex {
        parse_hex(input).map_err(|e| format!("hex parse: {e}"))
    } else {
        let mut bytes = decode_escapes(input);
        eol.append(&mut bytes);
        Ok(bytes)
    }
}

unsafe fn set_text(hwnd: HWND, value: &str) {
    SetWindowTextW(hwnd, wide(value).as_ptr());
}
unsafe fn text(hwnd: HWND) -> String {
    let mut value = vec![0u16; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
    let len = GetWindowTextW(hwnd, value.as_mut_ptr(), value.len() as i32);
    String::from_utf16_lossy(&value[..len.max(0) as usize])
}
unsafe fn rich_text(hwnd: HWND) -> Vec<u16> {
    let mut value = vec![0u16; GetWindowTextLengthW(hwnd).max(0) as usize + 2];
    let request = GetText {
        bytes: (value.len() * 2) as u32,
        flags: 0,
        codepage: 1200,
        default_char: null(),
        used_default: null_mut(),
    };
    let len = SendMessageW(
        hwnd,
        EM_GETTEXTEX_,
        &request as *const _ as usize,
        value.as_mut_ptr() as isize,
    )
    .max(0) as usize;
    value.truncate(len);
    value
}
unsafe fn selection(hwnd: HWND) -> Range {
    let mut range = Range::default();
    SendMessageW(hwnd, EM_EXGETSEL_, 0, &mut range as *mut _ as isize);
    range
}
unsafe fn select(hwnd: HWND, start: i32, end: i32) {
    let range = Range { start, end };
    SendMessageW(hwnd, EM_EXSETSEL_, 0, &range as *const _ as isize);
}
unsafe fn set_color(hwnd: HWND, color: u32) {
    let mut format: CharFormat = zeroed();
    format.size = size_of::<CharFormat>() as u32;
    format.mask = 0x40000000;
    format.color = color; // CFM_COLOR，SCF_SELECTION。
    SendMessageW(hwnd, EM_SETCHARFORMAT_, 1, &format as *const _ as isize);
}

/// 子类过程保护历史，保留系统鼠标选区/滚动/复制。输入历史时自动将插入点移至草稿。
unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    // 先放行 RichEdit 查询/格式化消息；否则 selection() 的同步查询会再次进入本过程。
    if !matches!(
        msg,
        WM_KEYDOWN | WM_CHAR | WM_CONTEXTMENU | WM_PASTE | WM_CUT | WM_CLEAR | WM_UNDO
    ) {
        return CallWindowProcW((*ptr).old_edit_proc, hwnd, msg, wp, lp);
    }
    let app = &mut *ptr;
    let range = selection(hwnd);
    if msg == WM_CONTEXTMENU {
        if range.start != range.end {
            SendMessageW(hwnd, WM_COPY, 0, 0);
        }
        return 0;
    }
    if msg == WM_KEYDOWN {
        if wp == VK_RETURN as usize {
            app.send();
            return 0;
        }
        if wp == VK_DELETE as usize && (range.start < app.input_start || app.pending) {
            return 0;
        }
    }
    if msg == WM_CHAR {
        // 只保留原生编辑控制字符，不把旧 TUI 功能快捷键映射到 GUI 操作。
        if wp == 13 || (wp < 32 && !matches!(wp, 1 | 3 | 8 | 9 | 22 | 24 | 26)) {
            return 0;
        }
        if app.pending && wp != 3 {
            return 0;
        }
        if wp == 127 {
            return 0;
        } // Ctrl+Backspace 的系统“删词”可能越过设备提示符边界。
        if wp == 8
            && (range.start < app.input_start
                || (range.start == app.input_start && range.start == range.end))
        {
            return 0;
        }
        if wp >= 32 && range.start < app.input_start {
            select(hwnd, -1, -1);
        }
    }
    if matches!(msg, WM_PASTE | WM_CUT | WM_CLEAR | WM_UNDO) {
        if app.pending {
            return 0;
        }
        if msg == WM_UNDO {
            return 0;
        } // 历史区与草稿共用控件，禁用跨接收批次撤销。
        if range.start < app.input_start {
            if msg == WM_PASTE {
                select(hwnd, -1, -1);
            } else {
                return 0;
            }
        }
    }
    CallWindowProcW(app.old_edit_proc, hwnd, msg, wp, lp)
}

/// WM_NCCREATE 绑定 App，其他无关消息不借用状态，避免控件通知造成重入借用。
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    if msg == WM_NCCREATE {
        let create = &*(lp as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        (*(create.lpCreateParams as *mut App)).window = hwnd;
        return 1;
    }
    if msg == WM_GETMINMAXINFO {
        (*(lp as *mut MINMAXINFO)).ptMinTrackSize = POINT { x: 1040, y: 480 };
        return 0;
    }
    if msg == WM_CLOSE {
        DestroyWindow(hwnd);
        return 0;
    }
    if msg == WM_DESTROY {
        KillTimer(hwnd, 1);
        PostQuitMessage(0);
        return 0;
    }
    // 组合框编辑通知与 RichEdit EN_CHANGE 无需处理，避免程序更新控件时重入 App。
    if !matches!(msg, WM_TIMER | WM_SIZE | WM_SETFOCUS | WM_COMMAND)
        || (msg == WM_COMMAND && wp >> 16 != 0)
    {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let app = &mut *ptr;
    match msg {
        WM_TIMER => app.tick(),
        WM_SIZE => app.layout(),
        WM_SETFOCUS => {
            SetFocus(app.terminal);
        }
        WM_COMMAND => app.command(wp & 0xffff),
        _ => {}
    }
    0
}

/// 仅 GUI 构建显示启动失败；不使用控制台 stderr 作为用户唯一错误反馈。
pub fn show_error(message: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("Serial Monitor").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 创建但不显示窗口，供运行入口和原生控件测试复用；返回值持有所有 UI 资源。
unsafe fn create_app(
    handle: Handle,
    core: Arc<SerialCore>,
    config: TuiConfig,
) -> io::Result<NativeWindow> {
    SetProcessDPIAware();
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES,
    };
    InitCommonControlsEx(&controls);
    let rich = LoadLibraryW(wide("Msftedit.dll").as_ptr());
    if rich.is_null() {
        return Err(io::Error::last_os_error());
    }
    let (tx, rx) = mpsc::channel();
    let (port, baud, connected) = handle.block_on(core.state());
    let notice = core
        .log_snapshot(20)
        .into_iter()
        .rev()
        .find(|r| r.dir == Direction::Status)
        .map(|r| String::from_utf8_lossy(&r.data).into_owned())
        .unwrap_or_default();
    let mut app = Box::new(App {
        events: core.subscribe(),
        core: core.clone(),
        handle: handle.clone(),
        tx: tx.clone(),
        rx,
        window: null_mut(),
        terminal: null_mut(),
        port: null_mut(),
        baud: null_mut(),
        status: null_mut(),
        log_status: null_mut(),
        mcp: null_mut(),
        buttons: Vec::new(),
        font: null_mut(),
        mono: null_mut(),
        old_edit_proc: None,
        input_start: 0,
        stream: DisplayStream::default(),
        palette: Palette::new(config.theme.unwrap_or_default()),
        hex: config.input_hex.or(config.hex_view).unwrap_or(false),
        eol: config.line_ending.unwrap_or_default(),
        ports: Vec::new(),
        ports_known: false,
        current_port: port,
        current_baud: baud,
        connected,
        notice,
        pending: false,
        max_chars: (config.max_lines.unwrap_or(500).clamp(50, 2000) * 1024) as i32,
    });
    let class_name = wide("SerialMonitorWin32");
    let mut class: WNDCLASSEXW = zeroed();
    class.cbSize = size_of::<WNDCLASSEXW>() as u32;
    class.lpfnWndProc = Some(window_proc);
    class.hInstance = GetModuleHandleW(null());
    class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    class.hbrBackground = (COLOR_WINDOW + 1) as HBRUSH;
    class.lpszClassName = class_name.as_ptr();
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        wide(&format!(
            "Serial Monitor — WinAPI | {}",
            config.theme.unwrap_or_default().name()
        ))
        .as_ptr(),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        1260,
        800,
        null_mut(),
        null_mut(),
        class.hInstance,
        (&mut *app) as *mut App as *const _,
    );
    let mut native = NativeWindow { app, rich };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    native.app.create()?;
    // Win11 系统标题栏与圆角，不引入第三方皮肤或运行时 DLL。
    let dark = i32::from(config.theme.unwrap_or_default() != Theme::Light);
    DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const _, 4);
    let corner = 2i32;
    DwmSetWindowAttribute(hwnd, 33, &corner as *const _ as *const _, 4);
    Ok(native)
}

/// 先销毁子控件，使其不再引用字体/模块，再释放 GDI 与 DLL；错误返回也走同一清理路径。
struct NativeWindow {
    app: Box<App>,
    rich: HMODULE,
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.app.window) != 0 {
                DestroyWindow(self.app.window);
            }
            DeleteObject(self.app.font);
            DeleteObject(self.app.mono);
            FreeLibrary(self.rich);
        }
    }
}

/// UI 不拥有 Runtime。worker 检测接收端关闭即退出；主程序负责关闭串口与刷新日志。
pub fn run(handle: Handle, core: Arc<SerialCore>, config: TuiConfig) -> io::Result<()> {
    unsafe {
        let native = create_app(handle.clone(), core.clone(), config)?;
        let hwnd = native.app.window;
        let tx = native.app.tx.clone();
        handle.spawn(async move {
            let mut ticks = 0u32;
            loop {
                let (port, baud, connected) = core.state().await;
                if tx.send(Reply::State(port, baud, connected)).is_err() {
                    break;
                }
                if ticks.is_multiple_of(5) {
                    let ports = tokio::task::spawn_blocking(SerialCore::available_ports).await;
                    let ports = ports
                        .map_err(|e| e.to_string())
                        .and_then(|p| p.map_err(|e| e.to_string()));
                    if tx.send(Reply::Ports(ports)).is_err() {
                        break;
                    }
                }
                ticks = ticks.wrapping_add(1);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetFocus(native.app.terminal);
        let mut msg: MSG = zeroed();
        loop {
            let code = GetMessageW(&mut msg, null_mut(), 0, 0);
            if code == -1 {
                return Err(io::Error::last_os_error());
            }
            if code == 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static NATIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 真实窗口/GDI 资源重复创建销毁；预热排除系统控件缓存，不访问串口或剪贴板。
    /// 计数仅覆盖本进程的这条窗口路径；容许全量并行测试中少量系统临时句柄波动。
    #[test]
    fn native_resource_counts_stay_bounded_over_100_windows() {
        let _lock = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe extern "system" {
            fn GetCurrentProcess() -> HANDLE;
            fn GetProcessHandleCount(process: HANDLE, count: *mut u32) -> i32;
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetGuiResources(process: HANDLE, flags: u32) -> u32;
        }
        let counts = || unsafe {
            let process = GetCurrentProcess();
            let mut handles = 0;
            assert_ne!(GetProcessHandleCount(process, &mut handles), 0);
            (
                GetGuiResources(process, 0),
                GetGuiResources(process, 1),
                handles,
            )
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let core = {
            let _guard = runtime.enter();
            SerialCore::start(None)
        };
        let cycle = || unsafe {
            let mut window =
                create_app(runtime.handle().clone(), core.clone(), TuiConfig::default()).unwrap();
            window.app.replace_draft("resource test 中文😀");
            window.app.apply(Patch {
                remove: 0,
                spans: vec![("test line\r".repeat(100), window.app.palette.fg)],
            });
            window.app.clear();
            drop(window);
            // DestroyWindow 投递的 WM_QUIT 不应留给下一个窗口的消息循环。
            let mut msg: MSG = zeroed();
            while PeekMessageW(&mut msg, null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE) != 0 {}
        };
        for _ in 0..5 {
            cycle();
        }
        let before = counts();
        for _ in 0..100 {
            cycle();
        }
        let after = counts();
        println!(
            "100 window cycles: GDI {} -> {}, USER {} -> {}, handles {} -> {}",
            before.0, after.0, before.1, after.1, before.2, after.2
        );
        assert!(
            after.0 <= before.0 + 4,
            "GDI objects grow with window cycles"
        );
        assert!(
            after.1 <= before.1 + 4,
            "USER objects grow with window cycles"
        );
        assert!(
            after.2 <= before.2 + 32,
            "process handles grow with window cycles"
        );
    }
    /// 在真实 RichEdit 控件上验证 UTF-16 输入边界、历史保护、样式和滚动条。
    /// 窗口不显示，不连接串口，不改系统剪贴板；这不是长期运行或设备收发测试。
    #[test]
    fn native_richedit_preserves_draft_and_history_boundaries() {
        let _lock = NATIVE_TEST_LOCK.lock().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let core = {
            let _guard = runtime.enter();
            SerialCore::start(None)
        };
        unsafe {
            let mut native =
                create_app(runtime.handle().clone(), core, TuiConfig::default()).unwrap();
            let app = &mut native.app;
            app.replace_draft("help中文😀");
            let patch = app.stream.event(
                SerialEvent::Data {
                    ts: SystemTime::now(),
                    dir: Direction::Rx,
                    data: "prompt中文😀".as_bytes().to_vec(),
                },
                false,
                app.palette,
            );
            app.apply(patch);
            assert_eq!(app.draft(), "help中文😀");
            assert_eq!(
                selection(app.terminal).start,
                app.input_start + units("help中文😀")
            );
            let before = rich_text(app.terminal);
            select(app.terminal, app.input_start, app.input_start);
            SendMessageW(app.terminal, WM_CHAR, 8, 0);
            assert_eq!(
                rich_text(app.terminal),
                before,
                "backspace must not remove the prompt"
            );
            select(app.terminal, 0, 3);
            SendMessageW(app.terminal, WM_CLEAR, 0, 0);
            assert_eq!(
                rich_text(app.terminal),
                before,
                "history cannot be cut/deleted"
            );
            select(app.terminal, 0, 1);
            SendMessageW(app.terminal, WM_CHAR, 'X' as usize, 0);
            assert_eq!(
                app.draft(),
                "help中文😀X",
                "typing over history moves to the draft tail"
            );
            select(app.terminal, 0, 1);
            let mut format: CharFormat = zeroed();
            format.size = size_of::<CharFormat>() as u32;
            SendMessageW(
                app.terminal,
                WM_USER + 58,
                1,
                &mut format as *mut _ as isize,
            );
            assert_eq!(
                format.color, app.palette.dim,
                "timestamp color is dimmed in the native control"
            );
            app.clear();
            assert_eq!(app.input_start, 0);
            assert_eq!(app.draft(), "help中文😀X");
            app.send();
            assert!(!app.pending, "disconnected send does not discard the draft");
            let mut limits: MINMAXINFO = zeroed();
            SendMessageW(
                app.window,
                WM_GETMINMAXINFO,
                0,
                &mut limits as *mut _ as isize,
            );
            assert!(limits.ptMinTrackSize.x >= 1040 && limits.ptMinTrackSize.y >= 480);
            app.apply(Patch {
                remove: 0,
                spans: vec![(
                    format!("{}\r{}", "X".repeat(3000), "line\r".repeat(200)),
                    app.palette.fg,
                )],
            });
            // 空内容时系统可隐藏滚动条；构造真正超出视口的内容后检查可滚动范围。
            for bar in [SB_HORZ, SB_VERT] {
                let mut info: SCROLLINFO = zeroed();
                info.cbSize = size_of::<SCROLLINFO>() as u32;
                info.fMask = SIF_RANGE | SIF_PAGE;
                assert_ne!(GetScrollInfo(app.terminal, bar, &mut info), 0);
                assert!(
                    info.nMax > info.nPage as i32,
                    "native scrollbar must cover overflowing content: {bar}"
                );
            }
        }
    }
    /// 编码测试只验证字节契约，不代表驱动写入或设备执行。
    #[test]
    fn gui_command_encoding() {
        assert_eq!(
            command_bytes("help", false, LineEnding::Crlf).unwrap(),
            b"help\r\n"
        );
        assert_eq!(
            command_bytes(r"help\r\n", false, LineEnding::Crlf).unwrap(),
            b"help\r\n"
        );
        assert_eq!(command_bytes("", false, LineEnding::Cr).unwrap(), b"\r");
        assert_eq!(
            command_bytes("01 ff", true, LineEnding::Crlf).unwrap(),
            [1, 255]
        );
        assert!(command_bytes("zz", true, LineEnding::None).is_err());
    }
    /// UTF-16 代理对计数和跨包快照替换用于防止中文/表情导致输入边界错位。
    #[test]
    fn stream_replaces_partial_and_dims_timestamp() {
        let mut stream = DisplayStream::default();
        let palette = Palette::new(Theme::Dark);
        let event = |data: &[u8]| SerialEvent::Data {
            ts: SystemTime::now(),
            dir: Direction::Rx,
            data: data.to_vec(),
        };
        let first = stream.event(event("中😀".as_bytes()), false, palette);
        assert_eq!(first.spans[0].1, palette.dim);
        let count: i32 = first.spans.iter().map(|(s, _)| units(s)).sum();
        let second = stream.event(event(b"!\n"), false, palette);
        assert_eq!(second.remove, count);
        assert!(second.spans.iter().any(|(s, _)| s.contains("中😀!")));
        assert_eq!(stream.partial, 0);
    }
}
