//! TUI 可见文字选区与本机剪贴板。
//!
//! 选区持有按下鼠标时的屏幕快照，持续收数不会让正在选取的字符移位。
//! 坐标使用终端字符格而非 UTF-8 字节；复制按显示格读取，跳过宽字符的占位格。
//! 本模块不读取原始串口记录，也不执行串口发送，复制的仅是当前可见文本。

use ratatui::{buffer::Buffer, layout::{Position, Rect}, style::{Color, Style}, text::Span};

/// 流式多行选区：首尾行选取部分文字，中间行选取整行；反向拖动得到相同文本。
pub struct Selection {
    pub snapshot: Buffer,
    anchor: Position,
    cursor: Position,
    pub dragging: bool,
    /// 复制成功/失败反馈，独立于串口状态，避免下一包 RX 覆盖操作提示。
    pub notice: Option<String>,
}

impl Selection {
    /// 消费最新屏幕快照，不复制整段历史；快照区域只能是终端内容，不含边框和状态栏。
    pub fn new(snapshot: Buffer, position: Position) -> Self {
        Self { snapshot, anchor: position, cursor: position, dragging: true, notice: None }
    }

    /// 鼠标拖出内容区时将端点限制在快照边缘，避免访问不存在的字符格。
    pub fn update(&mut self, position: Position) {
        let area = self.snapshot.area;
        self.cursor = Position::new(
            position.x.clamp(area.x, area.right().saturating_sub(1)),
            position.y.clamp(area.y, area.bottom().saturating_sub(1)),
        );
    }

    /// 排序按“行、列”，不能使用 Position 的字段顺序来判断屏幕阅读顺序。
    fn bounds(&self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.cursor.y, self.cursor.x) {
            (self.anchor, self.cursor)
        } else { (self.cursor, self.anchor) }
    }

    fn row_bounds(&self, y: u16) -> (u16, u16) {
        let (start, end) = self.bounds();
        (if y == start.y { start.x } else { self.snapshot.area.x },
         if y == end.y { end.x } else { self.snapshot.area.right() - 1 })
    }

    /// 从显示快照提取 UTF-8 文本；去掉每行末尾用于填满视口的空格，保留行间换行。
    /// 若端点落在中文等宽字符的后半格，则复制完整字符，避免产生半个字。
    pub fn text(&self) -> String {
        let (start, end) = self.bounds();
        let mut rows = Vec::new();
        for y in start.y..=end.y {
            let (left, right) = self.row_bounds(y);
            let mut row = String::new();
            let mut x = self.snapshot.area.x;
            while x < self.snapshot.area.right() {
                let symbol = self.snapshot[(x, y)].symbol();
                let width = Span::raw(symbol).width().max(1) as u16;
                if x <= right && x.saturating_add(width) > left { row.push_str(symbol); }
                x = x.saturating_add(width);
            }
            rows.push(row.trim_end_matches(' ').to_string());
        }
        rows.join("\n")
    }

    /// 先还原冻结画面，再在选区范围内覆盖高亮；原快照样式不被修改，复制内容也不含样式码。
    pub fn render(&self, buffer: &mut Buffer, foreground: Color, background: Color) {
        let area = self.snapshot.area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() { buffer[(x, y)].clone_from(&self.snapshot[(x, y)]); }
        }
        let (start, end) = self.bounds();
        for y in start.y..=end.y {
            let (left, right) = self.row_bounds(y);
            let mut x = area.x;
            while x < area.right() {
                let width = Span::raw(self.snapshot[(x, y)].symbol()).width().max(1) as u16;
                if x <= right && x.saturating_add(width) > left {
                    // 宽字符两格一起高亮，防止只选中后半格时终端绘制出残缺中文。
                    buffer.set_style(Rect::new(x, y, width.min(area.right() - x), 1), Style::default().fg(foreground).bg(background));
                }
                x = x.saturating_add(width);
            }
        }
    }
}

/// 缓存当前可见内容；复用 Cell/String 的容量，避免每帧重新分配整屏历史。
pub fn capture(buffer: &Buffer, area: Rect, target: &mut Option<Buffer>) {
    let target = target.get_or_insert_with(|| Buffer::empty(area));
    target.resize(area);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() { target[(x, y)].clone_from(&buffer[(x, y)]); }
    }
}

/// Windows 使用原生 Unicode 剪贴板，不依赖宿主终端是否支持 OSC52。
/// 系统剪贴板被其他程序占用时返回错误，调用方保留选区便于再次复制。
#[cfg(windows)]
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::{ffi::c_void, io, ptr};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, memory: *mut c_void) -> *mut c_void;
        fn GetDesktopWindow() -> *mut c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(memory: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(memory: *mut c_void) -> i32;
        fn GlobalFree(memory: *mut c_void) -> *mut c_void;
        fn GetConsoleWindow() -> *mut c_void;
    }
    let utf16: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect();
    // SAFETY: 分配足够的移动内存，锁定后复制带 NUL 结尾的 UTF-16；所有失败路径释放本方内存。
    // SetClipboardData 成功后内存所有权转移给系统，本函数不得再 GlobalFree。
    unsafe {
        let memory = GlobalAlloc(0x0002, std::mem::size_of_val(utf16.as_slice())); // GMEM_MOVEABLE
        if memory.is_null() { return Err(io::Error::last_os_error()); }
        let data = GlobalLock(memory);
        if data.is_null() { let error = io::Error::last_os_error(); GlobalFree(memory); return Err(error); }
        ptr::copy_nonoverlapping(utf16.as_ptr(), data.cast::<u16>(), utf16.len());
        GlobalUnlock(memory);
        let owner = GetConsoleWindow();
        if OpenClipboard(if owner.is_null() { GetDesktopWindow() } else { owner }) == 0 {
            let error = io::Error::last_os_error(); GlobalFree(memory); return Err(error);
        }
        let result = if EmptyClipboard() == 0 || SetClipboardData(13, memory).is_null() { // CF_UNICODETEXT
            let error = io::Error::last_os_error(); GlobalFree(memory); Err(error)
        } else { Ok(()) };
        CloseClipboard();
        result
    }
}

/// 非 Windows 构建明确报告不支持，不假装已写入剪贴板。
#[cfg(not(windows))]
pub fn copy_to_clipboard(_text: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "clipboard copy requires Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非零起点覆盖真实终端的边框偏移；同时验证反向选取、跨行与剪去填充空格。
    #[test]
    fn selection_is_direction_independent_and_uses_screen_coordinates() {
        let mut buffer = Buffer::empty(Rect::new(2, 5, 10, 2));
        buffer.set_string(2, 5, "hello", Style::default());
        buffer.set_string(2, 6, "world", Style::default());
        let mut selection = Selection::new(buffer.clone(), Position::new(3, 5));
        selection.update(Position::new(4, 6));
        assert_eq!(selection.text(), "ello\nwor");
        let mut reverse = Selection::new(buffer, Position::new(4, 6));
        reverse.update(Position::new(3, 5));
        assert_eq!(reverse.text(), selection.text());
        reverse.update(Position::new(0, 0));
        assert_eq!(reverse.cursor, Position::new(2, 5));
    }

    /// 中文占两个显示格，但只复制一次；端点处在其第二格时也不能丢失完整字符。
    #[test]
    fn unicode_copy_skips_wide_character_padding_and_preserves_snapshot() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(0, 0, "中文A", Style::default());
        let mut selection = Selection::new(buffer.clone(), Position::new(1, 0));
        selection.update(Position::new(4, 0));
        assert_eq!(selection.text(), "中文A");
        buffer.set_string(0, 0, "new data", Style::default());
        selection.render(&mut buffer, Color::Black, Color::Cyan);
        assert_eq!(buffer[(0, 0)].symbol(), "中");
        assert_eq!(buffer[(2, 0)].bg, Color::Cyan);
        assert_eq!(selection.text(), "中文A");
    }
}
