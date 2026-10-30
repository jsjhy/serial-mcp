//! ratatui TUI：人工输入（TX）+ 日志视图（RX/TX，hex/ascii 切换）+ 历史记录 + 状态栏。
//! 在 Windows Terminal 下运行效果最佳（Win11）。
//!
//! 本模块是独立 OS 线程中的单线程状态机：键盘/鼠标事件改变 App，串口广播更新显示行，
//! render 只从当前状态生成一帧。只有调用 async 核心 API 时才通过 Handle::block_on 进入 Tokio。
//! App 的历史行与 logger 的原始环形缓冲相互独立：前者存带样式文本，后者存原始记录。
//! 界面只实现简单行输入，不把每个键立即发给设备；Enter 解码整段输入并等待写入回执。
//! 主题在创建 App 时固定，鼠标滚动或新数据不会改变主题，文件日志也不使用这些颜色。

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind, MouseButton, EnableMouseCapture, DisableMouseCapture};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use tokio::sync::broadcast;

use crate::ansi::AnsiState;
use crate::codec::{decode_escapes, parse_hex};
use crate::config::{load_file_config, LineEnding, Theme, TuiConfig};
use crate::logger::Direction;
use crate::serial_core::{SerialConfig, SerialCore, SerialEvent};
use crate::selection::{self, Selection};

/// 视图最大行数（默认值；可用 `[tui].max_lines` 覆盖）。
const DEFAULT_MAX_VIEW_LINES: usize = 500;

/// 状态栏刷新间隔：串口状态变化不频繁，没必要每帧都阻塞 runtime 取一次。
const STATE_REFRESH: Duration = Duration::from_millis(200);
/// 设备枚举比读取内存连接状态更慢，独立限频；拔插后的“不存在/断开”最多约一秒更新。
const PORT_SCAN_REFRESH: Duration = Duration::from_secs(1);

/// 界面语义色。使用 RGB 而非修改操作系统终端调色板，影响范围限于 TUI 绘制。
/// 设备自己设置的 ANSI 颜色保留；未显式设置样式的文本继承 text/background。
struct Palette {
    background: Color,
    text: Color,
    muted: Color,
    border: Color,
    accent: Color,
    success: Color,
    error: Color,
}

impl Palette {
    /// 从固定配置值选择完整配色，保证状态栏、输入、边框和滚动条使用同一套语义色。
    fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                background: Color::Rgb(16, 20, 28), text: Color::Rgb(220, 226, 235),
                muted: Color::Rgb(146, 157, 175), border: Color::Rgb(85, 102, 128),
                accent: Color::Rgb(99, 195, 237), success: Color::Rgb(125, 211, 165), error: Color::Rgb(255, 123, 134),
            },
            Theme::Light => Self {
                background: Color::Rgb(247, 249, 252), text: Color::Rgb(31, 41, 55),
                muted: Color::Rgb(84, 99, 120), border: Color::Rgb(116, 133, 155),
                accent: Color::Rgb(0, 94, 166), success: Color::Rgb(15, 112, 72), error: Color::Rgb(186, 34, 51),
            },
            Theme::Nord => Self {
                background: Color::Rgb(46, 52, 64), text: Color::Rgb(236, 239, 244),
                muted: Color::Rgb(171, 185, 204), border: Color::Rgb(101, 120, 146),
                accent: Color::Rgb(136, 192, 208), success: Color::Rgb(163, 190, 140), error: Color::Rgb(191, 97, 106),
            },
            Theme::Solarized => Self {
                background: Color::Rgb(0, 43, 54), text: Color::Rgb(147, 161, 161),
                muted: Color::Rgb(131, 148, 150), border: Color::Rgb(88, 110, 117),
                accent: Color::Rgb(42, 161, 152), success: Color::Rgb(133, 153, 0), error: Color::Rgb(220, 50, 47),
            },
        }
    }

    /// 时间戳使用正文与背景 RGB 的中间值，将相对背景的对比强度降低约一半。
    /// 浅色主题会向浅背景靠近，深色主题会向深背景靠近，而不是统一乘 0.5 变黑。
    fn timestamp(&self) -> Color {
        match (self.text, self.background) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => Color::Rgb(
                ((r as u16 + br as u16) / 2) as u8,
                ((g as u16 + bg as u16) / 2) as u8,
                ((b as u16 + bb as u16) / 2) as u8,
            ),
            _ => self.muted,
        }
    }
}

/// 输入模式：人工输入按 ascii 还是 hex 发送；Port 模式用于手输串口名。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InputMode {
    Ascii,
    Hex,
    /// 端口输入：Enter 按输入内容打开串口（Ctrl+O 进入）。
    Port,
}

/// 连接表示核心持有句柄；断开表示设备存在但未打开；不存在表示所选设备未被枚举到。
/// 枚举失败不能证明设备消失，因此保守显示“断开”，由操作提示保留具体错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionStatus { Connected, Disconnected, Missing }

impl ConnectionStatus {
    fn classify(connected: bool, port: &str, ports: &[String], known: bool) -> Self {
        if connected { return Self::Connected; }
        if !known { return Self::Disconnected; }
        // Windows 设备名允许带 \\.\ 前缀；比较最后一段并忽略 COM 名称大小写。
        let selected = port.rsplit('\\').next().unwrap_or(port);
        let exists = if selected.is_empty() { !ports.is_empty() } else {
            ports.iter().any(|candidate| candidate.rsplit('\\').next().unwrap_or(candidate).eq_ignore_ascii_case(selected))
        };
        if exists { Self::Disconnected } else { Self::Missing }
    }

    fn label(self) -> &'static str {
        match self { Self::Connected => "CONNECTED", Self::Disconnected => "DISCONNECTED", Self::Missing => "NOT_FOUND" }
    }
}

/// TUI 线程独占的界面状态，无需额外加锁。跨线程数据只通过 core 和 rx 获取。
struct App {
    core: Arc<SerialCore>,
    rx: broadcast::Receiver<SerialEvent>,
    handle: tokio::runtime::Handle,
    config_path: Option<std::path::PathBuf>,
    /// 尚未发送的原始输入，保留反斜杠转义文本供编辑和历史回显。
    input: String,
    /// 人工命令历史，和设备输出历史分开；上下键改变输入，不滚动终端。
    history: Vec<String>,
    /// history.len() 表示最新输入位置，否则是正在查看的历史索引。
    hist_idx: usize,
    input_mode: InputMode,
    ascii_view: bool,
    max_lines: usize,
    horizontal_scroll: u16,
    /// 距离最新视图的行数；0 表示自动跟随，正值表示暂停在历史位置。
    scroll_back: usize,
    /// 最近一次绘制计算的滚动范围，鼠标/键盘事件据此限幅。
    vertical_max: usize,
    horizontal_max: u16,
    /// 横向滚动条只使用下边框 MCP 信息右侧的剩余区域，点击状态文本不触发滚动。
    horizontal_track: ratatui::layout::Rect,
    terminal_area: ratatui::layout::Rect,
    /// Some(true)=拖右侧竖条；Some(false)=拖底部横条；None=没有拖动捕获。
    drag_vertical: Option<bool>,
    /// 最近一帧的内容区快照，鼠标按下时移交给选区；不包含状态栏和滚动条。
    screen: Option<ratatui::buffer::Buffer>,
    /// 选区冻结画面但不暂停后台接收，右键/Ctrl+C 复制后仍保留高亮。
    selection: Option<Selection>,
    line_ending: LineEnding,
    theme: Theme,
    /// 最后一条显示行是否是 RX 未换行快照，决定下个 RX 应替换哪行以及输入是否接在其后。
    rx_line_open: bool,
    /// 终端区行（ascii 视图的行已带 ANSI 颜色/样式）。
    view_lines: VecDeque<Line<'static>>,
    /// ascii 视图的 ANSI 解析状态（跨数据包保持颜色与未完成序列）。
    ansi: AnsiState,
    status: String,
    rx_bytes: u64,
    tx_bytes: u64,
    ports: Vec<String>,
    /// 最近枚举是否成功，避免系统查询失败时把“不知道”误报为“不存在”。
    ports_known: bool,
    ports_at: Instant,
    should_quit: bool,
    /// 状态栏缓存：(端口, 波特率, 是否连接)，按 STATE_REFRESH 间隔刷新。
    state: (String, u32, bool),
    state_at: Instant,
}

impl App {
    /// 在 TUI 线程中构建状态并订阅后续事件；不回放订阅前收到的设备内容。
    /// max_lines 至少为 1，避免配置 0 导致每次追加都没有可用的历史行。
    fn new(
        handle: tokio::runtime::Handle,
        core: Arc<SerialCore>,
        config_path: Option<std::path::PathBuf>,
        tui_cfg: &TuiConfig,
    ) -> Self {
        let rx = core.subscribe();
        let state = handle.block_on(core.state());
        Self {
            core,
            rx,
            handle,
            config_path,
            input: String::new(),
            history: Vec::new(),
            hist_idx: 0,
            input_mode: if tui_cfg.input_hex.unwrap_or(false) {
                InputMode::Hex
            } else {
                InputMode::Ascii
            },
            ascii_view: !tui_cfg.hex_view.unwrap_or(false),
            max_lines: tui_cfg.max_lines.unwrap_or(DEFAULT_MAX_VIEW_LINES).max(1),
            horizontal_scroll: 0,
            scroll_back: 0,
            vertical_max: 0,
            horizontal_max: 0,
            horizontal_track: ratatui::layout::Rect::default(),
            terminal_area: ratatui::layout::Rect::default(),
            drag_vertical: None,
            screen: None,
            selection: None,
            line_ending: tui_cfg.line_ending.unwrap_or_default(),
            theme: tui_cfg.theme.unwrap_or_default(),
            rx_line_open: false,
            view_lines: VecDeque::new(),
            ansi: AnsiState::new(),
            status: String::new(),
            rx_bytes: 0,
            tx_bytes: 0,
            ports: Vec::new(),
            ports_known: false,
            ports_at: Instant::now(),
            should_quit: false,
            state,
            state_at: Instant::now(),
        }
    }
}

/// 运行 TUI。`handle` 用于 Ctrl+R 等需要调用 async 串口 API 的场景。
pub fn run(
    handle: &tokio::runtime::Handle,
    core: Arc<SerialCore>,
    config_path: Option<std::path::PathBuf>,
    tui_cfg: &TuiConfig,
) -> io::Result<()> {
    // TUI 显式使用配色；启动器可能继承 NO_COLOR=1，不能让其屏蔽界面颜色。
    // 仅影响本进程的终端渲染，不修改环境变量，文件日志仍为纯文本。
    crossterm::style::force_color_output(true);
    let mut app = App::new(handle.clone(), core, config_path, tui_cfg);
    // 启动即枚举并显示可用串口
    list_ports(&mut app);

    let mut terminal = ratatui::init();
    // 启用鼠标捕获后，滚轮事件交给应用，而不是由宿主终端滚动屏幕缓冲区。
    crossterm::execute!(io::stdout(), EnableMouseCapture)?;
    let result = event_loop(&mut terminal, &mut app);
    let mouse_result = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result.and(mouse_result)
}

/// 每轮最多阻塞 10ms 等输入，再限量消费串口事件并绘制，兼顾交互延迟与收数吞吐。
/// 绘制/键盘 I/O 错误返回给 run，由它恢复终端模式，再通知 main 结束。
fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        // MCP 重启也通过正常返回恢复终端，不从网络线程强行终止绘制。
        if crate::restart::ready() { break; }
        // 键盘事件（非阻塞轮询）
        if event::poll(Duration::from_millis(10))? {
            let ev = event::read()?;
            handle_key(app, ev)?;
            if app.should_quit {
                break;
            }
        }
        // 限制每帧事件数，持续高流量时仍要响应键盘和重绘。
        drain_serial(app, 256);
        terminal.draw(|f| render(f, app))?;
    }
    Ok(())
}

/// budget 是本帧事件处理上限，防止持续高速数据让 while-drain 永远无法结束。
/// Lagged 说明显示广播被覆盖：明确提示，并重置 ANSI 状态，不能用残缺转义继续解析。
fn drain_serial(app: &mut App, budget: usize) {
    for _ in 0..budget {
        match app.rx.try_recv() {
            Ok(ev) => handle_serial(app, ev),
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                app.ansi = AnsiState::new();
                app.rx_line_open = false;
                let msg = format!("view skipped {count} events; raw log is independent");
                push_line(app, Line::raw(format!("[status] {msg}")));
                app.status = msg;
            }
            Err(_) => break,
        }
    }
}

// ---- 事件处理 ----

/// 统一分派鼠标和键盘。Release 不重复输入，未绑定的 Ctrl/Alt 字符不进入命令文本。
/// 鼠标拖动只在滚动条按下后捕获；按下内容区不会被当作拖动进度条。
fn handle_key(app: &mut App, ev: Event) -> io::Result<()> {
    if matches!(ev, Event::Resize(_, _)) {
        // 窗口大小改变后旧快照坐标不再可靠，取消选区并在下一帧重建。
        app.selection = None;
        app.screen = None;
    }
    if let Event::Mouse(mouse) = ev {
        let area = app.terminal_area;
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
            copy_selection(app);
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight) {
            app.selection = None;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let snapshot = app.screen.take().or_else(|| app.selection.take().map(|selection| selection.snapshot));
            app.selection = snapshot.filter(|buffer| buffer.area.contains(position))
                .map(|buffer| Selection::new(buffer, position));
            if app.selection.is_some() {
                app.drag_vertical = None;
                return Ok(());
            }
        }
        if let Some(selection) = app.selection.as_mut() {
            if selection.dragging && matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)) {
                selection.update(position);
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) { selection.dragging = false; }
                return Ok(());
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_back = app.scroll_back.saturating_add(3).min(app.vertical_max),
            MouseEventKind::ScrollDown => app.scroll_back = app.scroll_back.saturating_sub(3),
            MouseEventKind::ScrollLeft => app.horizontal_scroll = app.horizontal_scroll.saturating_sub(8),
            MouseEventKind::ScrollRight => app.horizontal_scroll = app.horizontal_scroll.saturating_add(8).min(app.horizontal_max),
            MouseEventKind::Down(MouseButton::Left) => {
                app.drag_vertical = if mouse.column == area.right().saturating_sub(1) && mouse.row > area.y && mouse.row < area.bottom().saturating_sub(1) {
                    Some(true)
                } else if app.horizontal_track.contains(position) {
                    Some(false)
                } else { None };
            }
            MouseEventKind::Up(MouseButton::Left) => app.drag_vertical = None,
            _ => {}
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)) {
            // 轨道像素在此处是字符格坐标，线性映射到滚动范围；先限幅再计算避免越界。
            // 竖向从顶部的绝对位置转换为“距底部行数”，横向直接使用距左列数。
            match app.drag_vertical {
                Some(true) => {
                    let track = area.height.saturating_sub(3).max(1) as usize;
                    let pos = mouse.row.saturating_sub(area.y + 1) as usize;
                    app.scroll_back = app.vertical_max.saturating_sub(pos.min(track) * app.vertical_max / track);
                }
                Some(false) => {
                    let track = app.horizontal_track.width.saturating_sub(1).max(1) as usize;
                    let pos = mouse.column.saturating_sub(app.horizontal_track.x) as usize;
                    app.horizontal_scroll = (pos.min(track) * app.horizontal_max as usize / track) as u16;
                }
                None => {}
            }
        }
        return Ok(());
    }
    if let Event::Key(key) = ev {
        // Windows 下 crossterm 每个按键会分别发 Press 和 Release 两个事件，
        // 不过滤 Release 会导致每个字符输入两次（输入 "help" 得到 "hheellpp"）。
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            copy_selection(app);
            return Ok(());
        }
        // 输入/翻页等操作恢复实时内容；Esc 只取消选区时保留尚未发送的输入。
        let had_selection = app.selection.take().is_some();
        if had_selection && key.code == KeyCode::Esc { return Ok(()); }
        match key.code {
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                clear_display(app);
            }
            KeyCode::PageUp => {
                app.scroll_back = app.scroll_back.saturating_add(app.terminal_area.height.saturating_sub(3).max(1) as usize).min(app.vertical_max);
            }
            KeyCode::PageDown => {
                app.scroll_back = app.scroll_back.saturating_sub(app.terminal_area.height.saturating_sub(3).max(1) as usize);
            }
            KeyCode::End => app.scroll_back = 0,
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.line_ending = app.line_ending.next();
            }
            KeyCode::Left => {
                app.horizontal_scroll = app.horizontal_scroll.saturating_sub(8);
            }
            KeyCode::Right => {
                app.horizontal_scroll = app.horizontal_scroll.saturating_add(8);
            }
            KeyCode::Home => {
                app.horizontal_scroll = 0;
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+R：重新连接串口（有配置则重读应用，无配置用当前/可用端口重连）
                reload_config(app);
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+O：手动输入串口名后回车打开（配置文件里没写 port 时用）
                app.input_mode = InputMode::Port;
                app.input.clear();
                app.hist_idx = app.history.len();
                app.status = "enter port name (e.g. COM9), Enter to open, Esc to cancel".into();
            }
            // 功能快捷键一律带 Ctrl，避免抢占输入字符（字母直接进输入区）
            KeyCode::Char('h' | 'H') if key.modifiers.contains(KeyModifiers::CONTROL) && key.kind == KeyEventKind::Press => {
                // Ctrl+H/h 同步切换数据视图与命令编码；以当前 view 为依据收敛到相同模式。
                // 长按不反复切换；端口名输入态必须保持 Port，避免把端口名当 Hex 命令发送。
                app.ascii_view = !app.ascii_view;
                if app.input_mode != InputMode::Port {
                    app.input_mode = if app.ascii_view { InputMode::Ascii } else { InputMode::Hex };
                }
                app.rx_line_open = false;
                app.ansi = AnsiState::new();
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+L：重新枚举并显示可用串口
                list_ports(app);
            }
            KeyCode::Enter => {
                app.scroll_back = 0;
                if app.input_mode == InputMode::Port {
                    open_input_port(app);
                } else {
                    send_input(app);
                }
            }
            KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                app.scroll_back = 0;
                app.input.push(ch);
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Up => {
                nav_history(app, true);
            }
            KeyCode::Down => {
                nav_history(app, false);
            }
            KeyCode::Esc => {
                app.input.clear();
                if app.input_mode == InputMode::Port {
                    app.input_mode = InputMode::Ascii;
                    app.status = "port input cancelled".into();
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Ctrl+K 只清除显示历史及其派生状态，不清空输入、命令历史、收发计数或日志缓冲/文件。
/// 必须丢弃未完成 ANSI/UTF-8、选区快照和滚动位置，否则旧残行可能在下一包重新出现。
/// 串口及 MCP 连接不受影响，后续收到的数据继续显示。
fn clear_display(app: &mut App) {
    app.view_lines.clear();
    app.ansi = AnsiState::new();
    app.rx_line_open = false;
    app.selection = None;
    app.screen = None;
    app.drag_vertical = None;
    app.scroll_back = 0;
    app.horizontal_scroll = 0;
    app.vertical_max = 0;
    app.horizontal_max = 0;
    app.horizontal_track = ratatui::layout::Rect::default();
    app.status = "display cleared (logs kept)".into();
}

/// 两个复制入口共用同一路径。剪贴板写入失败时保留选区，避免需要重新拖选。
fn copy_selection(app: &mut App) {
    let Some(selection) = app.selection.as_mut() else { return; };
    let text = selection.text();
    if text.is_empty() { return; }
    app.status = match selection::copy_to_clipboard(&text) {
        Ok(()) => "selection copied (Esc to cancel)".into(),
        Err(error) => format!("copy failed: {error}"),
    };
    selection.notice = Some(app.status.clone());
}

/// Ctrl+O 输入态下回车：把输入的串口名当作要打开的端口。
fn open_input_port(app: &mut App) {
    let port = app.input.trim().to_ascii_uppercase();
    app.input.clear();
    app.input_mode = InputMode::Ascii;
    if port.is_empty() {
        app.status = "port name required".into();
        return;
    }
    let (_, baud, _) = app.handle.block_on(app.core.state());
    let sc = SerialConfig {
        port: port.clone(),
        baud,
    };
    match app.handle.block_on(app.core.open(sc)) {
        Ok(()) => app.status = format!("opened {port} @ {baud} baud"),
        Err(e) => app.status = format!("open failed: {e}"),
    }
}

/// Ctrl+R：重新连接串口。
/// - 有 `--config`：重读配置文件并应用 port/baud（热重连，日志参数仍重启生效）
/// - 无配置文件：用当前串口参数（--port / 上次连接）直接重连
/// - 已连接：关闭旧句柄并重新打开（读写线程每轮取新句柄，自动切换）
fn reload_config(app: &mut App) {
    if let Some(path) = &app.config_path {
        match load_file_config(path) {
            Ok(cfg) => {
                if let Some(p) = cfg.port {
                    let baud = cfg.baud.unwrap_or(115200);
                    let sc = SerialConfig { port: p.clone(), baud };
                    match app.handle.block_on(app.core.open(sc)) {
                        Ok(()) => app.status = format!("reloaded: {p} @ {baud} baud"),
                        Err(e) => app.status = format!("reload failed: {e}"),
                    }
                    return;
                }
                // 配置无 port：落到下方用当前参数重连
            }
            Err(e) => {
                app.status = format!("config error: {e}");
                return;
            }
        }
    }
    // 无配置（或配置无 port）：优先用当前参数重连
    let (port, baud, _) = app.handle.block_on(app.core.state());
    if port.is_empty() {
        // 未配置过端口：只剩一个可用串口就直接连（双击启动的常见情况），
        // 多个则提示用 Ctrl+O 手输，避免连错设备。
        match SerialCore::available_ports() {
            Ok(ports) if ports.len() == 1 => {
                let p = ports[0].clone();
                let sc = SerialConfig {
                    port: p.clone(),
                    baud,
                };
                match app.handle.block_on(app.core.open(sc)) {
                    Ok(()) => app.status = format!("connected {p} @ {baud} baud"),
                    Err(e) => app.status = format!("connect {p} failed: {e}"),
                }
                return;
            }
            Ok(ports) if ports.is_empty() => {
                app.status = "no serial ports found (plug in device, then Ctrl+L)".into();
                return;
            }
            Ok(ports) => {
                app.status = format!(
                    "no port set; available: {}  (Ctrl+O to type one)",
                    ports.join(" ")
                );
                return;
            }
            Err(e) => {
                app.status = format!("list ports failed: {e}");
                return;
            }
        }
    }
    let sc = SerialConfig { port: port.clone(), baud };
    match app.handle.block_on(app.core.open(sc)) {
        Ok(()) => app.status = format!("reconnected: {port} @ {baud} baud"),
        Err(e) => app.status = format!("reconnect failed: {e}"),
    }
}

/// 发送人工输入：按当前编码模式把输入字节化，走统一 TX 队列。
/// ascii 模式支持转义（`\r` `\n` `\t` `\0` `\\` `\xNN`），输入原样保留（不 trim 空格）。
fn send_input(app: &mut App) {
    let raw = app.input.clone();
    if raw.is_empty() && app.input_mode != InputMode::Ascii {
        return;
    }
    // 未连接时直接提示，并把输入留在输入区（否则会被写线程丢弃且内容已清空）
    let (port, _, connected) = app.handle.block_on(app.core.state());
    if !connected {
        app.status = if port.is_empty() {
            "not connected: Ctrl+R to auto-connect, Ctrl+O to type port".into()
        } else {
            format!("not connected ({port}): Ctrl+R to reconnect")
        };
        return;
    }
    let bytes = match app.input_mode {
        InputMode::Port => return, // Port 模式的 Enter 走 open_input_port
        InputMode::Ascii => {
            let mut bytes = decode_escapes(&raw);
            app.line_ending.append(&mut bytes);
            bytes
        }
        InputMode::Hex => match parse_hex(&raw) {
            Ok(b) => b,
            Err(e) => {
                app.status = format!("hex parse error: {e}");
                return;
            }
        },
    };
    if bytes.is_empty() {
        return;
    }
    if !raw.is_empty() && app.history.last().map(|h| h != &raw).unwrap_or(true) {
        app.history.push(raw);
    }
    app.hist_idx = app.history.len();
    // 等实际串口写入结束才清空输入；失败保留输入方便重试。
    // 此等待发生在 TUI 线程，可能短暂延迟界面刷新，串口读取与文件日志线程仍独立运行。
    match app.handle.block_on(app.core.send(bytes)) {
        Ok(()) => {
            app.status = "written to serial port".into();
            app.input.clear();
        }
        Err(e) => app.status = format!("send failed (input kept): {e}"),
    }
    // 不做本地回显：写线程成功后会广播 TX 事件，再回显会重复显示两遍
}

/// 有界移动历史索引；回到 history.len() 时清空输入，当前实现不保存未发送草稿。
fn nav_history(app: &mut App, up: bool) {
    if app.history.is_empty() {
        return;
    }
    if up {
        if app.hist_idx > 0 {
            app.hist_idx -= 1;
        }
    } else if app.hist_idx < app.history.len() {
        app.hist_idx += 1;
    }
    if app.hist_idx < app.history.len() {
        app.input = app.history[app.hist_idx].clone();
    } else {
        app.input.clear();
    }
}

/// 将广播事件转换为显示状态。字节计数只统计本订阅者实际收到的 Data 事件，
/// 因而订阅前/广播 Lagged 的数据不会体现在计数中，不能当成设备链路总计数。
fn handle_serial(app: &mut App, ev: SerialEvent) {
    match ev {
        SerialEvent::Data { ts, dir, data } => {
            // 字节计数（顶部状态栏）
            match dir {
                Direction::Rx => app.rx_bytes += data.len() as u64,
                Direction::Tx => app.tx_bytes += data.len() as u64,
                Direction::Status => {}
            }
            if app.ascii_view {
                push_ansi(app, ts, dir, &data);
            } else {
                push_line(app, Line::raw(format_data_hex(ts, dir, &data)));
            }
            app.status = "connected".into();
        }
        SerialEvent::Status { msg } => {
            app.rx_line_open = false;
            app.ansi.reset_line();
            // 状态消息也进终端区（打开/关闭/重连/错误可见）
            let ts = std::time::SystemTime::now();
            let t = chrono::DateTime::<chrono::Local>::from(ts);
            push_line(
                app,
                Line::raw(format!("[{}] [status] {msg}", t.format("%H:%M:%S%.3f"))),
            );
            app.status = msg;
        }
    }
}

/// ascii 视图：把数据交给 ANSI 解析器，按解析出的行（带颜色）追加到终端区。
/// 设备输出里的 `ESC[32m` 等颜色序列会真正上色；一包数据里含多个换行时逐行显示。
fn push_ansi(app: &mut App, ts: std::time::SystemTime, dir: Direction, data: &[u8]) {
    let t = chrono::DateTime::<chrono::Local>::from(ts);
    let prefix = if dir == Direction::Rx {
        format!("[{}] ", t.format("%H:%M:%S%.3f"))
    } else {
        format!("[{}] {}> ", t.format("%H:%M:%S%.3f"), dir.label())
    };
    // RX 必须持久化解析状态；TX 是独立发送片段，不能让命令中的颜色改变设备后续显示。
    let lines = if dir == Direction::Rx {
        app.ansi.feed(data)
    } else {
        app.rx_line_open = false;
        app.ansi.reset_line();
        AnsiState::new().feed(data)
    };
    if lines.is_empty() {
        return;
    }
    let partial = dir == Direction::Rx && app.ansi.partial_line();
    for (i, mut line) in lines.into_iter().enumerate() {
        if i == 0 && app.rx_line_open {
            if let Some(previous) = app.view_lines.back_mut() {
                // 解析器返回完整的当前行快照，用它替换旧残行，避免重复追加。
                if let Some(prefix) = previous.spans.first().cloned() {
                    line.spans.insert(0, prefix);
                }
                *previous = line;
                continue;
            }
        }
        line.spans.insert(0, Span::raw(prefix.clone()));
        push_line(app, line);
    }
    app.rx_line_open = partial;
}

/// 追加完整显示行并淘汰最旧行；正在看历史时增加距底偏移，让新数据不顶走阅读位置。
/// 若最旧内容本身被容量上限淘汰，下一次 render 会把滚动位置限制在剩余内容范围内。
fn push_line(app: &mut App, line: Line<'static>) {
    if app.scroll_back > 0 { app.scroll_back = app.scroll_back.saturating_add(1); }
    if app.view_lines.len() >= app.max_lines {
        app.view_lines.pop_front();
    }
    app.view_lines.push_back(line);
}

/// 枚举可用串口并显示到终端区（启动时自动调用一次，按 `Ctrl+L` 重新枚举）。
fn list_ports(app: &mut App) {
    app.ports_at = Instant::now();
    match SerialCore::available_ports() {
        Ok(ports) => {
            app.ports = ports.clone();
            app.ports_known = true;
            if ports.is_empty() {
                push_line(app, Line::raw("[ports] none found"));
                app.status = "no serial ports found".into();
            } else {
                let names = ports.join(" ");
                push_line(app, Line::raw(format!("[ports] {names}")));
                app.status = format!("{} port(s): {names}", ports.len());
            }
        }
        Err(e) => {
            app.ports_known = false;
            app.status = format!("list ports failed: {e}");
        }
    }
}

// ---- 渲染 ----

/// 生成一帧：顶部快捷键帮助 → 串口状态 → 终端内容，MCP 信息嵌入终端下边框。
/// 渲染会更新布局缓存与滚动限幅；不进行磁盘 I/O，也不直接读取串口。
fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let palette = Palette::for_theme(app.theme);
    f.render_widget(Block::default().style(Style::default().fg(palette.text).bg(palette.background)), area);

    // ---- 顶部状态栏：串口状态（缓存，按 STATE_REFRESH 间隔刷新）----
    if app.state_at.elapsed() >= STATE_REFRESH {
        app.state = app.handle.block_on(app.core.state());
        app.state_at = Instant::now();
    }
    if app.ports_at.elapsed() >= PORT_SCAN_REFRESH {
        app.ports_at = Instant::now();
        match SerialCore::available_ports() {
            Ok(ports) => { app.ports = ports; app.ports_known = true; }
            Err(error) => {
                app.ports_known = false;
                app.status = format!("list ports failed: {error}");
            }
        }
    }
    let (port, baud, connected) = app.state.clone();
    let connection = ConnectionStatus::classify(connected, &port, &app.ports, app.ports_known);
    let connection_color = match connection {
        ConnectionStatus::Connected => palette.success,
        ConnectionStatus::Disconnected => palette.accent,
        ConnectionStatus::Missing => palette.error,
    };
    let conn_span = Span::styled(connection.label(), Style::default().fg(connection_color).add_modifier(ratatui::style::Modifier::BOLD));
    // 原来第三行的提示并入连接状态旁边；普通 connected 消息不重复显示。
    let status_note = if app.status.is_empty() || app.status.eq_ignore_ascii_case("connected") {
        String::new()
    } else { format!(" ({})", app.status) };
    let state_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{port} @ {baud} baud"), Style::default().fg(palette.text)),
        Span::raw("  |  "),
        conn_span,
        Span::styled(status_note, Style::default().fg(palette.accent)),
        Span::raw("  |  "),
        Span::raw(format!(
            "view:{}  input:{}  eol:{}",
            if app.ascii_view { "ascii" } else { "hex" },
            match app.input_mode {
                InputMode::Ascii => "ascii",
                InputMode::Hex => "hex",
                InputMode::Port => "port",
            },
            app.line_ending.label()
        )),
        Span::styled(
            format!("  |  rx:{}B  tx:{}B  dropped:{}", app.rx_bytes, app.tx_bytes, app.core.dropped_hint()),
            Style::default().fg(palette.muted),
        ),
    ]);
    // 上下栏先按显示格宽预换行，再用实际行数分配高度，不能把额外信息交给边框裁剪。
    let width = area.width.saturating_sub(2).max(1);
    let serial_lines = wrap_status_lines(vec![state_line,
        Line::raw(format!("log: {}", app.core.log_file_name()))], width);
    let mcp_status = app.core.mcp_status();
    let mcp_color = if mcp_status.starts_with("CONNECTED") { palette.success }
        else if mcp_status.starts_with("failed:") { palette.error } else { palette.accent };
    // MCP 状态与会话、客户端、地址固定同行，宽度不足时提示扩窗，不折行也不静默截断。
    let mcp_line = Line::styled(format!("MCP: {mcp_status}"), Style::default().fg(mcp_color));
    let mcp_width = mcp_line.width();
    let minimum_width = (mcp_width + 4).max(40); // 两端边框和状态文本留白。
    let normal_help = "Enter=send  Ctrl+X=quit  Ctrl+K=clear  Ctrl+H=input/view ascii/hex  Ctrl+E=eol  Ctrl+R=connect  Ctrl+O=port  Ctrl+L=scan  Drag=select  Right-click/Ctrl+C=copy  Wheel/PgUp/PgDn=history  End=live  Left/Right=scroll  Home=left";
    let help_text = if let Some(selection) = &app.selection {
        match &selection.notice {
            Some(notice) => format!("{notice}  |  Right-click/Ctrl+C=copy  Esc=cancel  Ctrl+K=clear"),
            None => "Right-click/Ctrl+C=copy  Esc=cancel selection  Ctrl+K=clear  Ctrl+X=quit".into(),
        }
    } else { normal_help.into() };
    let help_lines = wrap_status_lines(vec![Line::styled(help_text, Style::default().fg(palette.muted))], area.width.max(1));
    // 选区帮助可能更短，仍保留普通帮助所需空间，避免选取时布局变化使坐标失效。
    let help_height = help_lines.len().max(wrap_status_lines(vec![Line::raw(normal_help)], area.width.max(1)).len());
    let serial_height = serial_lines.len() + 2;
    let minimum_height = serial_height + help_height + 3;
    if (area.width as usize) < minimum_width || (area.height as usize) < minimum_height {
        app.terminal_area = ratatui::layout::Rect::default();
        app.selection = None;
        app.screen = None;
        app.drag_vertical = None;
        app.horizontal_track = ratatui::layout::Rect::default();
        let warning = format!("Window too small\nCurrent: {}x{}\nNeed: >={minimum_width} columns, >={minimum_height} rows at this width\nResize window. Ctrl+X=quit", area.width, area.height);
        let lines = wrap_status_lines(warning.lines().map(|line| Line::raw(line.to_string())).collect(), area.width.max(1));
        f.render_widget(Paragraph::new(lines).style(Style::default().fg(palette.error)), area);
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(help_height as u16), // 快捷键置顶，选区提示也使用同一区域。
        Constraint::Length(serial_height as u16),
        Constraint::Min(3), // 状态和帮助完整显示后，剩余区域留给数据，至少保留一行输入。
    ]).split(area);
    f.render_widget(Paragraph::new(help_lines), chunks[0]);
    f.render_widget(
        Paragraph::new(serial_lines).block(Block::bordered().border_style(Style::default().fg(palette.border)).title(" serial ")),
        chunks[1],
    );

    // 输入接在最后一个未换行的设备输出后；没有设备提示符时使用本地提示符。
    let terminal_area = chunks[2];
    app.terminal_area = terminal_area;
    let inner_w = terminal_area.width.saturating_sub(2);
    let inner_h = terminal_area.height.saturating_sub(2) as usize;
    let inline = app.ascii_view && app.rx_line_open && app.input_mode == InputMode::Ascii
        && !app.view_lines.is_empty();
    let history_len = app.view_lines.len().saturating_sub(usize::from(inline));
    app.vertical_max = (history_len + 1).saturating_sub(inner_h);
    app.scroll_back = app.scroll_back.min(app.vertical_max);
    let start = app.vertical_max.saturating_sub(app.scroll_back);
    // 只复制当前视口中的行，主题相关时间戳样式在副本上应用，不修改原始历史内容。
    let lines: Vec<Line> = app.view_lines.iter().take(history_len.min(start + inner_h)).skip(start).cloned()
        .map(|mut line| { dim_timestamp(&mut line, palette.timestamp()); line }).collect();
    let visible_history = lines.len() as u16;
    let prompt = match app.input_mode {
        InputMode::Hex => "# ",
        InputMode::Port => "port> ",
        InputMode::Ascii => "> ",
    };
    let mut input_line = if inline {
        app.view_lines.back().unwrap().clone()
    } else {
        Line::from(Span::styled(prompt, Style::default().fg(palette.success)))
    };
    let prompt_w = input_line.width();
    input_line.spans.push(Span::styled(app.input.clone(), Style::default().fg(palette.success)));
    dim_timestamp(&mut input_line, palette.timestamp());
    let max_scroll = app.view_lines.iter().map(Line::width).max().unwrap_or(0)
        .saturating_sub(inner_w as usize).min(u16::MAX as usize) as u16;
    app.horizontal_scroll = app.horizontal_scroll.min(max_scroll);
    app.horizontal_max = max_scroll;
    f.render_widget(
        Block::bordered().border_style(Style::default().fg(palette.border))
            .title(format!(" terminal  col:{}  theme:{} ", app.horizontal_scroll as usize + 1, app.theme.name()))
            .title_bottom(mcp_line),
        terminal_area,
    );
    let history_area = ratatui::layout::Rect::new(
        terminal_area.x + 1, terminal_area.y + 1, inner_w, visible_history,
    );
    f.render_widget(Paragraph::new(lines).scroll((0, app.horizontal_scroll)), history_area);
    // 光标跟随内容，长输入自动水平跟随末尾，不固定在窗口底部。
    if app.selection.is_none() && app.scroll_back == 0 && inner_h > 0 && inner_w > 0 {
        // 仅实时视图显示输入光标；阅读历史时不在旧日志行上画可编辑光标。
        let input_y = terminal_area.y + 1 + visible_history;
        let cursor_col = prompt_w + Line::raw(app.input.as_str()).width();
        let input_scroll = cursor_col.saturating_sub(inner_w.saturating_sub(1) as usize)
            .min(u16::MAX as usize) as u16;
        f.render_widget(Paragraph::new(input_line).scroll((0, input_scroll)), ratatui::layout::Rect::new(
            terminal_area.x + 1, input_y, inner_w, 1,
        ));
        let x_off = cursor_col.saturating_sub(input_scroll as usize).min(inner_w.saturating_sub(1) as usize) as u16;
        f.set_cursor_position((terminal_area.x + 1 + x_off, input_y));
    }

    if inner_h > 0 && inner_w > 0 {
        // 滚动条覆盖边框而不额外占用数据行/列；端点箭头关闭，点击映射与完整轨道一致。
        let mut vertical = ScrollbarState::new(app.vertical_max + 1).position(start);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None)
                .thumb_style(Style::default().fg(palette.accent)).track_style(Style::default().fg(palette.border)),
            ratatui::layout::Rect::new(terminal_area.right() - 1, terminal_area.y + 1, 1, inner_h as u16),
            &mut vertical,
        );
        // MCP 下边框标题优先，横条仅占右侧余量；没有空间时保留左右键滚动。
        let track_x = terminal_area.x + 1 + mcp_width as u16 + 2;
        let track_width = terminal_area.right().saturating_sub(1).saturating_sub(track_x);
        app.horizontal_track = if track_width >= 3 {
            ratatui::layout::Rect::new(track_x, terminal_area.bottom() - 1, track_width, 1)
        } else { ratatui::layout::Rect::default() };
        if track_width >= 3 {
            let mut horizontal = ScrollbarState::new(max_scroll as usize + 1).position(app.horizontal_scroll as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::HorizontalBottom).begin_symbol(None).end_symbol(None)
                    .thumb_style(Style::default().fg(palette.accent)).track_style(Style::default().fg(palette.border)),
                app.horizontal_track, &mut horizontal,
            );
        }
    }

    let content_area = ratatui::layout::Rect::new(terminal_area.x + 1, terminal_area.y + 1, inner_w, inner_h as u16);
    if app.selection.as_ref().is_some_and(|selection| selection.snapshot.area != content_area) {
        app.selection = None;
    }
    if let Some(selection) = &app.selection {
        selection.render(f.buffer_mut(), palette.background, palette.accent);
    } else if inner_w > 0 && inner_h > 0 {
        selection::capture(f.buffer_mut(), content_area, &mut app.screen);
    } else {
        app.screen = None;
    }

}

/// 按终端显示格折行并保留样式/字素边界，测量与实际渲染共用结果，避免行高估算不一致。
/// 仅用于状态栏和帮助；串口数据仍保持单行并通过横向滚动查看。
fn wrap_status_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut output = Vec::new();
    for line in lines {
        let mut row = Line::default();
        let mut columns = 0;
        for span in line.spans {
            for grapheme in span.styled_graphemes(line.style) {
                let cells = Span::raw(grapheme.symbol).width();
                if columns > 0 && columns + cells > width {
                    output.push(row);
                    row = Line::default();
                    columns = 0;
                }
                if let Some(last) = row.spans.last_mut().filter(|last| last.style == grapheme.style) {
                    last.content.to_mut().push_str(grapheme.symbol);
                } else { row.spans.push(Span::styled(grapheme.symbol.to_string(), grapheme.style)); }
                columns += cells;
            }
        }
        output.push(row);
    }
    output
}

/// 将显示行开头的 [HH:MM:SS.mmm] 拆成独立 Span，只淡化时间戳。
/// 不处理设备正文中间的时间文本，也不修改原始日志字节或设备 ANSI 样式。
fn dim_timestamp(line: &mut Line<'static>, color: Color) {
    let Some(first) = line.spans.first() else { return; };
    let bytes = first.content.as_bytes();
    if bytes.len() < 14 || bytes[0] != b'[' || bytes[3] != b':' || bytes[6] != b':'
        || bytes[9] != b'.' || bytes[13] != b']' { return; }
    if ![1, 2, 4, 5, 7, 8, 10, 11, 12].iter().all(|&i| bytes[i].is_ascii_digit()) { return; }
    let first = line.spans.remove(0);
    let text = first.content.into_owned();
    let mut prefix = vec![Span::styled(text[..14].to_string(), first.style.fg(color))];
    if text.len() > 14 { prefix.push(Span::styled(text[14..].to_string(), first.style)); }
    line.spans.splice(0..0, prefix);
}

// ---- 格式化 ----

/// 不经过 ANSI 解析，逐字节生成原始 Hex 展示，便于检查控制字节和二进制协议。
/// RX 不带方向标签，TX/STATUS 保留标签；日志文件格式由 logger 独立决定。
fn format_data_hex(ts: std::time::SystemTime, dir: Direction, data: &[u8]) -> String {
    let t = chrono::DateTime::<chrono::Local>::from(ts);
    let mut s = if dir == Direction::Rx {
        format!("[{}] ", t.format("%H:%M:%S%.3f"))
    } else {
        format!("[{}] {}: ", t.format("%H:%M:%S%.3f"), dir.label())
    };
    for b in data {
        s.push_str(&format!("{b:02X} "));
    }
    s
}

#[cfg(test)]
/// 使用内存 TestBackend 验证布局、交互和样式；不打开实际终端，也不依赖串口硬件。
/// SerialCore::start(None) 只提供测试所需状态，断开场景不会向真实设备发送字节。
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    #[test]
    fn timestamps_have_half_contrast_without_dimming_message() {
        for theme in [Theme::Dark, Theme::Light, Theme::Nord, Theme::Solarized] {
            let palette = Palette::for_theme(theme);
            let mut line = Line::from(vec![
                Span::raw("[12:34:56.789] TX> "),
                Span::styled("正文", Style::default().fg(Color::Green)),
            ]);
            let text = line.to_string();
            dim_timestamp(&mut line, palette.timestamp());
            assert_eq!(line.to_string(), text);
            assert_eq!(line.spans[0].content, "[12:34:56.789]");
            assert_eq!(line.spans[0].style.fg, Some(palette.timestamp()));
            assert_eq!(line.spans[1].style.fg, None);
            assert_eq!(line.spans[2].style.fg, Some(Color::Green));
            assert_ne!(palette.timestamp(), palette.text);
            assert_ne!(palette.timestamp(), palette.background);
        }
        let mut plain = Line::raw("[status] no timestamp");
        dim_timestamp(&mut plain, Color::DarkGray);
        assert_eq!(plain.spans.len(), 1);
    }

    #[test]
    fn tui_color_commands_emit_rgb_even_when_launcher_disables_color() {
        crossterm::style::force_color_output(true);
        let mut output = String::new();
        crossterm::Command::write_ansi(
            &crossterm::style::SetBackgroundColor(crossterm::style::Color::Rgb { r: 247, g: 249, b: 252 }),
            &mut output,
        ).unwrap();
        assert!(output.contains("48;2;247;249;252"), "{output:?}");
    }

    /// 读取测试帧指定屏幕行，用于断言截断、布局和提示符是否正确。
    fn row_text(buf: &Buffer, y: u16) -> String {
        row_range(buf, y, 0, buf.area.width)
    }

    /// 跳过中文等宽字符的占位格，按可见字符还原区间，避免把“连接”误读为“连 接 ”。
    fn row_range(buf: &Buffer, y: u16, start: u16, end: u16) -> String {
        let mut result = String::new();
        let mut x = start;
        while x < end {
            let symbol = buf[(x, y)].symbol();
            result.push_str(symbol);
            x += Span::raw(symbol).width().max(1) as u16;
        }
        result
    }

    /// 小窗口 + 超长数据行：输入行与帮助行必须仍然可见（不能被折行挤掉/截到窗口外）。
    #[test]
    fn narrow_window_keeps_input_and_help_visible() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());

        // 一条 600 字符的 hex 行：开 Wrap 时会被折成多行，把底部的输入行挤出去
        app.view_lines.push_back(Line::raw(format!(
            "[12:00:00.000] RX> {}",
            "AA ".repeat(200)
        )));
        for i in 0..20 {
            app.view_lines.push_back(Line::raw(format!("line {i}")));
        }
        app.input = "AT".into();

        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let input_row = row_text(&buf, app.terminal_area.bottom() - 2);
        assert!(
            input_row.contains("> AT"),
            "输入行必须可见，实际: {input_row:?}"
        );
        let help_row = row_text(&buf, 0);
        assert!(
            help_row.contains("Enter=send"),
            "帮助行必须可见，实际: {help_row:?}"
        );
    }

    /// 极窄窗口给出尺寸提示，不继续绘制被截断的状态栏或访问越界单元格。
    #[test]
    fn tiny_window_does_not_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        app.input = "hello".into();

        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(row_text(&buf, 5).chars().count(), 20);
        assert!(row_text(&buf, 0).contains("Window too small"));
        assert_eq!(app.terminal_area.height, 0);
    }

    /// 合成无修饰键事件，显式区分 Press/Release 以覆盖 Windows 的双事件行为。
    fn key_event(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(crossterm::event::KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    /// 三态基于句柄和设备枚举共同判断；“枚举成功但不存在”与“枚举失败”必须区分。
    #[test]
    fn connection_status_distinguishes_connected_disconnected_and_missing() {
        let ports = vec!["COM8".to_string()];
        assert_eq!(ConnectionStatus::classify(true, "COM8", &[], true), ConnectionStatus::Connected);
        assert_eq!(ConnectionStatus::classify(false, "com8", &ports, true), ConnectionStatus::Disconnected);
        assert_eq!(ConnectionStatus::classify(false, r"\\.\COM8", &ports, true), ConnectionStatus::Disconnected);
        assert_eq!(ConnectionStatus::classify(false, "COM9", &ports, true), ConnectionStatus::Missing);
        assert_eq!(ConnectionStatus::classify(false, "COM8", &[], true), ConnectionStatus::Missing);
        assert_eq!(ConnectionStatus::classify(false, "COM8", &[], false), ConnectionStatus::Disconnected);
        assert_eq!(ConnectionStatus::classify(false, "", &ports, true), ConnectionStatus::Disconnected);
        assert_eq!(ConnectionStatus::classify(false, "", &[], true), ConnectionStatus::Missing);
    }

    /// 宽窗口只保留状态/日志两行，操作提示紧邻“断开”，不再有单独第三行或预留空白。
    #[test]
    fn serial_message_is_inline_with_connection_status() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        app.state = ("COM8".into(), 115200, false);
        app.ports = vec!["COM8".into()];
        app.ports_known = true;
        app.status = "Access is denied".into();
        let mut terminal = Terminal::new(TestBackend::new(200, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let top = (0..app.terminal_area.y).find(|&y| row_text(buf, y).contains(" serial ")).unwrap();
        assert!(row_text(buf, top + 1).contains("DISCONNECTED (Access is denied)"));
        assert!(row_text(buf, top + 2).contains("log: disabled"));
        assert_eq!(app.terminal_area.y - top, 4);
    }

    /// 大小写 Ctrl+H 同步收敛 input/view；释放、长按和旧 Tab 不应意外改变模式。
    #[test]
    fn ctrl_h_switches_input_and_view_together_for_both_letter_cases() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        app.input = "help".into();
        let ctrl_h = |letter, kind| Event::Key(crossterm::event::KeyEvent {
            code: KeyCode::Char(letter), modifiers: KeyModifiers::CONTROL,
            kind, state: crossterm::event::KeyEventState::NONE,
        });
        handle_key(&mut app, ctrl_h('h', KeyEventKind::Press)).unwrap();
        assert_eq!(app.input_mode, InputMode::Hex);
        assert!(!app.ascii_view);
        handle_key(&mut app, ctrl_h('h', KeyEventKind::Release)).unwrap();
        handle_key(&mut app, ctrl_h('h', KeyEventKind::Repeat)).unwrap();
        handle_key(&mut app, key_event(KeyCode::Tab, KeyEventKind::Press)).unwrap();
        assert_eq!(app.input_mode, InputMode::Hex);
        assert!(!app.ascii_view);
        handle_key(&mut app, ctrl_h('H', KeyEventKind::Press)).unwrap();
        assert_eq!(app.input_mode, InputMode::Ascii);
        assert!(app.ascii_view);
        assert_eq!(app.input, "help");
        app.input_mode = InputMode::Port;
        handle_key(&mut app, ctrl_h('h', KeyEventKind::Press)).unwrap();
        assert_eq!(app.input_mode, InputMode::Port);
        assert!(!app.ascii_view);
    }

    /// 不同宽度下所有状态字段和帮助文字都必须保留；这里只验证内存帧布局，不操作宿主窗口。
    #[test]
    fn status_and_help_wrap_without_losing_fields_at_multiple_widths() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        app.core.set_mcp_status("CONNECTED | sessions:2 | client-alpha, client-beta | http://127.0.0.1:8010/mcp".into());
        app.state = ("COM8".into(), 115200, true);
        app.rx_bytes = 1234567890123;
        app.tx_bytes = 9876543210;
        app.status = "open COM8 failed: Access is denied. (os error 5)".into();
        app.input = "draft".into();
        for width in [40, 60, 80, 120, 200] {
            app.state_at = Instant::now();
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
            if (width as usize) < format!("MCP: {}", app.core.mcp_status()).len() + 4 {
                // 完整 MCP 信息放不进单行时明确提示扩窗，不能让内容折行或被裁掉。
                assert_eq!(app.terminal_area.height, 0);
                assert!(row_text(terminal.backend().buffer(), 0).contains("Window too small"));
                continue;
            }
            assert!(app.terminal_area.height >= 3);
            let buf = terminal.backend().buffer();
            let header_top = (0..app.terminal_area.y).find(|&y| row_text(buf, y).contains(" serial ")).unwrap();
            let serial_text: String = (header_top + 1..app.terminal_area.y - 1).map(|y| {
                row_range(buf, y, 1, width - 1).trim_end().to_string()
            }).collect();
            for field in ["COM8 @ 115200", "CONNECTED", "input:ascii", "eol:CRLF", "rx:1234567890123B", "tx:9876543210B", "log: disabled", "Access is denied.", "(os error 5)"] {
                assert!(serial_text.contains(field), "width={width}, missing {field}: {serial_text}");
            }
            let mcp_row = row_text(buf, app.terminal_area.bottom() - 1);
            assert!(mcp_row.contains("sessions:2") && mcp_row.contains("client-beta") && mcp_row.contains("http://127.0.0.1:8010/mcp"));
            let help: String = (0..header_top).map(|y| row_text(buf, y).trim_end().to_string()).collect();
            assert!(row_text(buf, 0).starts_with("Enter=send"));
            for key in ["Enter=send", "Ctrl+X=quit", "Ctrl+K=clear", "Ctrl+H=input/view", "Ctrl+R=connect", "Ctrl+L=scan", "Home=left"] {
                assert!(help.contains(key), "width={width}, missing {key}: {help}");
            }
            assert_eq!(app.input, "draft");
        }
    }

    /// 状态栏换行按显示宽度计算，不能切开中文/emoji 字素或丢弃原有样式。
    #[test]
    fn status_wrapping_preserves_graphemes_and_styles() {
        let source = "日志文件 中文🙂 e\u{301} serial_20261029.log";
        let lines = wrap_status_lines(vec![Line::from(Span::styled(source, Style::default().fg(Color::Cyan)))], 10);
        assert_eq!(lines.iter().map(Line::to_string).collect::<String>(), source);
        assert!(lines.iter().all(|line| line.width() <= 10));
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| span.style.fg == Some(Color::Cyan)));
    }

    /// 清屏时丢弃拆包残留与旧选区，同时验证输入、历史和计数未被误清空。
    #[test]
    fn ctrl_k_clears_view_and_parser_without_clearing_input_or_counters() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        let now = std::time::SystemTime::now();
        push_ansi(&mut app, now, Direction::Rx, b"old display\nold partial\x1b[3");
        app.input = "unsent".into();
        app.history.push("help".into());
        app.rx_bytes = 123;
        app.tx_bytes = 45;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let snapshot = app.screen.take().unwrap();
        app.selection = Some(Selection::new(snapshot, ratatui::layout::Position::new(1, app.terminal_area.y + 1)));
        app.scroll_back = 5;
        app.horizontal_scroll = 16;
        handle_key(&mut app, Event::Key(crossterm::event::KeyEvent {
            code: KeyCode::Char('k'), modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press, state: crossterm::event::KeyEventState::NONE,
        })).unwrap();
        assert!(app.view_lines.is_empty());
        assert!(app.selection.is_none());
        assert!(app.screen.is_none());
        assert!(!app.rx_line_open);
        assert_eq!((app.scroll_back, app.horizontal_scroll), (0, 0));
        assert_eq!(app.input, "unsent");
        assert_eq!(app.history, vec!["help"]);
        assert_eq!((app.rx_bytes, app.tx_bytes), (123, 45));
        push_ansi(&mut app, now, Direction::Rx, b"new display\n");
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let data_y = app.terminal_area.y + 1;
        assert!(row_text(terminal.backend().buffer(), data_y).contains("new display"));
        assert!(!row_text(terminal.backend().buffer(), data_y).contains("old"));
        assert!(row_text(terminal.backend().buffer(), data_y + 1).contains("> unsent"));
    }

    /// 合成拖选而不触碰真实系统剪贴板；验证持续收数时选区固定及 Esc 保留输入。
    #[test]
    fn mouse_selection_freezes_visible_text_and_escape_preserves_input() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        push_line(&mut app, Line::raw("hello world"));
        app.input = "draft".into();
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let mouse = |kind, column, row| Event::Mouse(crossterm::event::MouseEvent {
            kind, column, row, modifiers: KeyModifiers::NONE,
        });
        let data_y = app.terminal_area.y + 1;
        handle_key(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 1, data_y)).unwrap();
        handle_key(&mut app, mouse(MouseEventKind::Drag(MouseButton::Left), 5, data_y)).unwrap();
        handle_key(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), 5, data_y)).unwrap();
        assert_eq!(app.selection.as_ref().unwrap().text(), "hello");
        push_line(&mut app, Line::raw("new incoming text"));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), data_y).contains("hello world"));
        assert!(!row_text(terminal.backend().buffer(), data_y + 1).contains("new incoming"));
        assert!(row_text(terminal.backend().buffer(), 0).contains("Right-click/Ctrl+C=copy"));
        handle_key(&mut app, key_event(KeyCode::Esc, KeyEventKind::Press)).unwrap();
        assert!(app.selection.is_none());
        assert_eq!(app.input, "draft");
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), data_y + 1).contains("new incoming"));
    }

    #[test]
    fn configured_themes_render_background_and_preserve_data() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        push_ansi(&mut app, std::time::SystemTime::now(), Direction::Rx, b"\x1b[32mdevice> ");
        app.input = "help".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        for expected in [Theme::Dark, Theme::Light, Theme::Nord, Theme::Solarized] {
            app.theme = expected;
            terminal.draw(|f| render(f, &mut app)).unwrap();
            let buf = terminal.backend().buffer();
            assert_eq!(buf[(70, app.terminal_area.y + 1)].bg, Palette::for_theme(expected).background);
            assert!(row_text(buf, app.terminal_area.y).contains(&format!("theme:{}", expected.name())));
            assert!(row_text(buf, app.terminal_area.y + 1).contains("device> help"));
            assert_eq!(app.view_lines[0].spans[1].style.fg, Some(Color::Green));
        }
        assert_eq!(app.input, "help");
        assert_eq!(app.view_lines.len(), 1);
    }

    #[test]
    fn former_theme_shortcuts_do_not_change_theme() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        handle_key(&mut app, Event::Key(crossterm::event::KeyEvent {
            code: KeyCode::Char('t'), modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press, state: crossterm::event::KeyEventState::NONE,
        })).unwrap();
        assert_eq!(app.theme, Theme::Dark);
        assert!(app.input.is_empty());
        handle_key(&mut app, key_event(KeyCode::F(3), KeyEventKind::Repeat)).unwrap();
        handle_key(&mut app, key_event(KeyCode::F(3), KeyEventKind::Release)).unwrap();
        assert_eq!(app.theme, Theme::Dark);
        handle_key(&mut app, key_event(KeyCode::F(3), KeyEventKind::Press)).unwrap();
        assert_eq!(app.theme, Theme::Dark);
    }

    #[test]
    fn only_ctrl_x_quits_and_plain_x_remains_input() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        handle_key(&mut app, key_event(KeyCode::Char('x'), KeyEventKind::Press)).unwrap();
        assert_eq!(app.input, "x");
        for ch in ['c', 'q', 'x'] {
            handle_key(&mut app, Event::Key(crossterm::event::KeyEvent {
                code: KeyCode::Char(ch), modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press, state: crossterm::event::KeyEventState::NONE,
            })).unwrap();
            assert_eq!(app.should_quit, ch == 'x');
            assert_eq!(app.input, "x");
        }
    }

    #[test]
    fn scrollbar_browses_history_preserves_position_and_returns_live() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        for i in 0..20 { push_line(&mut app, Line::raw(format!("line {i}"))); }
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        handle_key(&mut app, key_event(KeyCode::PageUp, KeyEventKind::Press)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let data_y = app.terminal_area.y + 1;
        let before = row_text(terminal.backend().buffer(), data_y);
        let first_visible = app.vertical_max - app.scroll_back;
        assert!(before.contains(&format!("line {first_visible}")));
        push_line(&mut app, Line::raw("line 20"));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert_eq!(row_text(terminal.backend().buffer(), data_y), before);
        let mouse = |kind, column, row| Event::Mouse(crossterm::event::MouseEvent {
            kind, column, row, modifiers: KeyModifiers::NONE,
        });
        handle_key(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 59, data_y)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), data_y).contains("line 0"));
        let last_y = app.terminal_area.bottom() - 2;
        handle_key(&mut app, mouse(MouseEventKind::Drag(MouseButton::Left), 59, last_y)).unwrap();
        assert_eq!(app.scroll_back, 0);
        handle_key(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), 59, last_y)).unwrap();
        handle_key(&mut app, mouse(MouseEventKind::ScrollUp, 20, data_y)).unwrap();
        assert_eq!(app.scroll_back, 3);
        handle_key(&mut app, key_event(KeyCode::End, KeyEventKind::Press)).unwrap();
        assert_eq!(app.scroll_back, 0);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), app.terminal_area.bottom() - 3).contains("line 20"));
    }

    #[test]
    fn sustained_fragmented_stream_keeps_lines_and_frame_stable() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        let stream: String = (0..3000).map(|i| format!("row {i} \x1b[32m中文\x1b[0m OK\r\n")).collect();
        let now = std::time::SystemTime::now();
        let mut offset = 0;
        let mut packet = 0;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        while offset < stream.len() {
            let end = (offset + 1 + packet % 17).min(stream.len());
            push_ansi(&mut app, now, Direction::Rx, &stream.as_bytes()[offset..end]);
            offset = end;
            packet += 1;
            if packet % 100 == 0 { terminal.draw(|f| render(f, &mut app)).unwrap(); }
            assert!(app.view_lines.len() <= 500);
        }
        assert_eq!(app.view_lines.len(), 500);
        for (i, line) in app.view_lines.iter().enumerate() {
            assert!(line.to_string().ends_with(&format!("row {} 中文 OK", 2500 + i)), "{line}");
        }
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), app.terminal_area.bottom() - 1).contains("MCP:"));
        assert!(row_text(terminal.backend().buffer(), 0).contains("Enter=send"));
    }

    #[test]
    fn broadcast_lag_resets_parser_and_resumes_reading() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        let (tx, rx) = broadcast::channel(2);
        app.rx = rx;
        push_ansi(&mut app, std::time::SystemTime::now(), Direction::Rx, b"old\x1b[3");
        for i in 0..20 { tx.send(SerialEvent::Data { ts: std::time::SystemTime::now(), dir: Direction::Rx, data: format!("new {i}\n").into_bytes() }).unwrap(); }
        drain_serial(&mut app, 256);
        assert!(app.view_lines.iter().any(|l| l.to_string().contains("skipped 18 events")));
        assert!(app.view_lines.back().unwrap().to_string().ends_with("new 19"));
    }

    #[test]
    fn prompt_is_joined_across_packets_without_newline() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        let now = std::time::SystemTime::now();
        push_ansi(&mut app, now, Direction::Rx, b"device");
        push_ansi(&mut app, now, Direction::Rx, b"> ");
        assert_eq!(app.view_lines.len(), 1);
        assert!(app.view_lines[0].to_string().ends_with("device> "));
        push_ansi(&mut app, now, Direction::Rx, b"help\r\nOK\r\n");
        assert_eq!(app.view_lines.len(), 2);
        assert!(app.view_lines[0].to_string().ends_with("device> help"));
        assert!(app.view_lines[1].to_string().ends_with("OK"));
        app.core.set_mcp_status("listening http://127.0.0.1:8010/mcp".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), app.terminal_area.bottom() - 1).contains("MCP: listening"));
        app.input = "help".into();
        push_ansi(&mut app, now, Direction::Rx, b"device> ");
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(row_text(buf, app.terminal_area.y + 3).contains("device> help"));
        assert!(!row_text(buf, app.terminal_area.bottom() - 2).contains("> "));
    }

    #[test]
    fn horizontal_scroll_reveals_tail_and_keeps_input_fixed() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());
        app.view_lines.push_back(Line::raw(format!("{}TAIL", "A".repeat(64))));
        app.input = "AT".into();
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(!row_text(terminal.backend().buffer(), app.terminal_area.y + 1).contains("TAIL"));
        for _ in 0..10 {
            handle_key(&mut app, key_event(KeyCode::Right, KeyEventKind::Press)).unwrap();
        }
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(row_text(terminal.backend().buffer(), app.terminal_area.y + 1).contains("TAIL"));
        assert!(row_text(terminal.backend().buffer(), app.terminal_area.y + 2).contains("> AT"));
        assert_eq!(app.horizontal_scroll, 30);
        handle_key(&mut app, key_event(KeyCode::Left, KeyEventKind::Press)).unwrap();
        assert_eq!(app.horizontal_scroll, 22);
        handle_key(&mut app, key_event(KeyCode::Home, KeyEventKind::Press)).unwrap();
        assert_eq!(app.horizontal_scroll, 0);
    }

    /// Windows 下 Press + Release 两个事件只应插入一个字符（否则输入变 "hheellpp"）。
    #[test]
    fn release_events_do_not_duplicate_typing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());

        for ch in ['h', 'e', 'l', 'p'] {
            handle_key(&mut app, key_event(KeyCode::Char(ch), KeyEventKind::Press)).unwrap();
            handle_key(&mut app, key_event(KeyCode::Char(ch), KeyEventKind::Release)).unwrap();
        }
        assert_eq!(app.input, "help");
    }

    /// Ctrl+O 进入串口名输入态：提示符与状态提示都要切过去，Esc 可取消。
    #[test]
    fn port_input_mode_toggles() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let core = SerialCore::start(None);
        let mut app = App::new(rt.handle().clone(), core, None, &TuiConfig::default());

        let ctrl = |ch| {
            Event::Key(crossterm::event::KeyEvent {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            })
        };
        handle_key(&mut app, ctrl('o')).unwrap();
        assert_eq!(app.input_mode, InputMode::Port);
        handle_key(&mut app, key_event(KeyCode::Esc, KeyEventKind::Press)).unwrap();
        assert_eq!(app.input_mode, InputMode::Ascii);
    }
}
