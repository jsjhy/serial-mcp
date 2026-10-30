//! 极简 ANSI 转义解析：把串口输出里的 SGR 颜色序列转成 ratatui 带样式文本。
//!
//! - 只解析 CSI SGR（`ESC [ ... m`）；其它 CSI（清屏/移动光标）与 OSC 序列直接丢弃，不影响显示。
//! - 跨 chunk 保持状态：设备常把 `ESC [` 与 `32m` 拆在两个数据包里。
//! - 未完成的转义序列/半截 UTF-8 缓存在 `pending`，等下一包数据到达再继续。
//! - 控制字符：`\n` 换行、`\t` 补齐 8 列、`\b` 回退一格，其余丢弃。
//!
//! 这是日志查看器用的增量解析器，不是完整的终端模拟器：不执行清屏/光标定位，
//! CR 被忽略，Tab 列宽按字符数估算，不能完整模拟 CJK/组合字符的终端列宽。
//! feed 返回“完整行 + 可能的最后一行快照”；最后一行若未结束，由 partial_line 标记。
//! 调用方收到后应替换旧残行，而不是把整个快照再次追加，否则持续拆包时会重复文本。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Default)]
/// 每个接收字节流持有一份。SGR 样式、文本残行、半截 UTF-8 和 ANSI 尾部都跨包保持。
pub struct AnsiState {
    /// 上一包未解析完的尾部字节（不完整转义序列或半截 UTF-8）。
    pending: Vec<u8>,
    /// 当前 SGR 样式（跨 chunk 持久，设备通常只在开头设一次颜色）。
    cur: Style,
    /// 当前行已收集的文本片段（相同样式合并）。
    spans: Vec<(String, Style)>,
    /// 当前行列号，用于 \t 对齐与 \b 回退。
    col: usize,
    /// 已解析完成的行。
    lines: Vec<Line<'static>>,
    partial_line: bool,
}

impl AnsiState {
    /// 创建默认样式和空解析状态。样式未指定的部分由 TUI 当前主题提供。
    pub fn new() -> Self {
        Self::default()
    }

    /// 最近一次返回值的末行是否为未换行快照；返回空列表时调用方应保留旧显示行状态。
    pub fn partial_line(&self) -> bool { self.partial_line }

    /// 丢弃正在拼接的文本/控制序列，但保留 cur 颜色样式。
    /// 用于 TX/STATUS 打断接收展示；广播丢事件时需新建整个 AnsiState，连样式也复位。
    pub fn reset_line(&mut self) {
        self.pending.clear();
        self.spans.clear();
        self.lines.clear();
        self.col = 0;
        self.partial_line = false;
    }

    /// 喂入一段字节，返回解析出的行（行内 span 已带颜色/样式）。
    /// 末尾没有换行的残行也会返回一行，避免数据滞留到下一包。
    /// 输入可以在任意字节处拆包，包含中文字符内部或 ESC 序列内部。
    /// 对未完整的转义序列延迟返回对应残行；其前面已经完成的行仍可立即显示。
    pub fn feed(&mut self, data: &[u8]) -> Vec<Line<'static>> {
        // 损坏的未终止控制序列不能无限积累，恢复后仍由解析器过滤控制字节。
        if self.pending.len() > 4096 {
            self.reset_line();
            self.cur = Style::default();
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        // 不在这里清 spans：上一包若停在半截转义序列上，当前行的文本要接着往下拼。
        // lines 每次都由 take_lines 取空，col 也随当前行延续。

        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            if b == 0x1b {
                match buf.get(i + 1).copied() {
                    None => {
                        self.pending = buf[i..].to_vec();
                        return self.take_lines();
                    }
                    Some(b'[') => {
                        // CSI：参数 0x30-0x3f，中间 0x20-0x2f，结束 0x40-0x7e
                        let mut j = i + 2;
                        while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                            j += 1;
                        }
                        if j >= buf.len() {
                            self.pending = buf[i..].to_vec();
                            return self.take_lines();
                        }
                        if buf[j] == b'm' {
                            self.apply_sgr(&buf[i + 2..j]);
                        }
                        i = j + 1;
                    }
                    Some(b']') => {
                        // OSC：到 BEL 或 ESC \ 结束
                        let mut j = i + 2;
                        loop {
                            match buf.get(j) {
                                None => {
                                    self.pending = buf[i..].to_vec();
                                    return self.take_lines();
                                }
                                Some(0x07) => {
                                    j += 1;
                                    break;
                                }
                                Some(0x1b) if buf.get(j + 1) == Some(&b'\\') => {
                                    j += 2;
                                    break;
                                }
                                _ => j += 1,
                            }
                        }
                        i = j;
                    }
                    // 其它双字节转义：跳过
                    Some(_) => i += 2,
                }
                continue;
            }

            match b {
                b'\n' => {
                    self.end_line();
                    i += 1;
                }
                // 回车：进度条类刷新自行覆盖，这里按“忽略”处理
                b'\r' => i += 1,
                b'\t' => {
                    let pad = 8 - (self.col % 8);
                    self.push_text(&" ".repeat(pad));
                    i += 1;
                }
                0x08 => {
                    self.backspace();
                    i += 1;
                }
                0x00..=0x1f | 0x7f => i += 1,
                _ => {
                    // 文本段：连续的可打印字节（控制字符/ESC 另行处理，不能整段吞掉）
                    let mut j = i;
                    while j < buf.len() && buf[j] >= 0x20 && buf[j] != 0x7f {
                        j += 1;
                    }
                    match std::str::from_utf8(&buf[i..j]) {
                        Ok(s) => {
                            self.push_text(s);
                            i = j;
                        }
                        Err(e) => {
                            let valid = e.valid_up_to();
                            if valid > 0 {
                                let s = std::str::from_utf8(&buf[i..i + valid]).expect("valid_up_to");
                                self.push_text(s);
                                i += valid;
                            } else if e.error_len().is_none() && j == buf.len() {
                                // 半截 UTF-8（只可能出现在本包末尾）：留到下一包
                                self.pending = buf[i..].to_vec();
                                return self.take_lines();
                            } else {
                                self.push_text("?");
                                i += 1;
                            }
                        }
                    }
                }
            }
        }
        self.take_lines()
    }

    // ---- 内部 ----

    /// 合并同样式相邻片段，减少 Span 数量。约 16KiB 以上的当前行会被分段展示，
    /// 限制不换行输出的内存增长；这只影响显示，不修改串口原始记录。
    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        // 设备长期不换行时也限制单行内存；原始字节仍由日志完整保存。
        let current_bytes: usize = self.spans.iter().map(|(text, _)| text.len()).sum();
        if current_bytes + s.len() > 16 * 1024 {
            self.end_line();
        }
        self.col += s.chars().count();
        if let Some((text, style)) = self.spans.last_mut() {
            if *style == self.cur {
                text.push_str(s);
                return;
            }
        }
        self.spans.push((s.to_string(), self.cur));
    }

    /// 从最后文本片段移除一个 Unicode 标量值；不支持字素簇或光标跨行回退。
    fn backspace(&mut self) {
        let Some((text, _)) = self.spans.last_mut() else {
            return;
        };
        text.pop();
        self.col = self.col.saturating_sub(1);
        if text.is_empty() {
            self.spans.pop();
        }
    }

    /// 将当前 spans 的所有权移动到完成行，清空列位置，但继续保留 SGR 样式。
    fn end_line(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        self.col = 0;
        let line: Line<'static> = spans
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect();
        self.lines.push(line);
    }

    /// 取走已完成行，必要时附上当前残行的副本。原残行必须保留用于下一包退格/Tab。
    fn take_lines(&mut self) -> Vec<Line<'static>> {
        // pending 里是半截转义序列时先不落地当前行：下一包会接上同一行内容，
        // 否则 `ESC[3` / `2m` 被拆包时同一行文本会被切成两行。
        let mid_escape = self.pending.first() == Some(&0x1b);
        self.partial_line = !mid_escape && !self.spans.is_empty();
        if !mid_escape && !self.spans.is_empty() {
            // 未换行内容保留为当前行，返回快照，下一包可以继续处理退格、Tab 和样式。
            self.lines.push(self.spans.iter()
                .map(|(text, style)| Span::styled(text.clone(), *style)).collect());
        }
        std::mem::take(&mut self.lines)
    }

    /// fg=true 修改前景，false 修改背景；不覆盖其他样式位。
    fn set_color(&mut self, fg: bool, color: Color) {
        self.cur = if fg {
            self.cur.fg(color)
        } else {
            self.cur.bg(color)
        };
    }

    /// 解析以分号分隔的 SGR 参数；未知控制码忽略，空项按 0（复位）处理。
    /// 扩展颜色消费额外参数后，循环统一再跨过主控制码，避免跳过后续 reset 等参数。
    fn apply_sgr(&mut self, params: &[u8]) {
        let text = String::from_utf8_lossy(params);
        let mut nums: Vec<Option<u16>> = text.split(';').map(|p| p.parse::<u16>().ok()).collect();
        // ESC[m 与 ESC[0m 等价：全部复位
        if nums == [None] {
            nums = vec![Some(0)];
        }
        let mut idx = 0;
        while idx < nums.len() {
            let n = nums[idx].unwrap_or(0);
            match n {
                0 => self.cur = Style::default(),
                1 => self.cur = self.cur.add_modifier(Modifier::BOLD),
                2 => self.cur = self.cur.add_modifier(Modifier::DIM),
                3 => self.cur = self.cur.add_modifier(Modifier::ITALIC),
                4 => self.cur = self.cur.add_modifier(Modifier::UNDERLINED),
                5 | 6 => {}
                7 => self.cur = self.cur.add_modifier(Modifier::REVERSED),
                8 => self.cur = self.cur.add_modifier(Modifier::HIDDEN),
                9 => self.cur = self.cur.add_modifier(Modifier::CROSSED_OUT),
                21 | 22 => self.cur = self.cur.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => self.cur = self.cur.remove_modifier(Modifier::ITALIC),
                24 => self.cur = self.cur.remove_modifier(Modifier::UNDERLINED),
                25 => {}
                27 => self.cur = self.cur.remove_modifier(Modifier::REVERSED),
                28 => self.cur = self.cur.remove_modifier(Modifier::HIDDEN),
                29 => self.cur = self.cur.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => {
                    let c = basic_color(n - 30);
                    self.set_color(true, c);
                }
                38 => idx += self.extended_color(&nums[idx + 1..], true),
                39 => self.cur = self.cur.fg(Color::Reset),
                40..=47 => {
                    let c = basic_color(n - 40);
                    self.set_color(false, c);
                }
                48 => idx += self.extended_color(&nums[idx + 1..], false),
                49 => self.cur = self.cur.bg(Color::Reset),
                90..=97 => {
                    let c = bright_color(n - 90);
                    self.set_color(true, c);
                }
                100..=107 => {
                    let c = bright_color(n - 100);
                    self.set_color(false, c);
                }
                _ => {}
            }
            idx += 1;
        }
    }

    /// 解析 256 色 / 真彩色子参数，返回消耗的参数个数。
    /// 返回值不包含主控制码 38/48；5;n 消费 2 项，2;r;g;b 消费 4 项。
    /// 参数不足不设置颜色，返回 0；数值转 u8 按当前实现截断，不进行范围校验。
    fn extended_color(&mut self, rest: &[Option<u16>], fg: bool) -> usize {
        match rest.first().copied().flatten() {
            Some(5) if rest.len() >= 2 => {
                self.set_color(fg, Color::Indexed(rest[1].unwrap_or(0) as u8));
                2
            }
            Some(2) if rest.len() >= 4 => {
                let r = rest[1].unwrap_or(0) as u8;
                let g = rest[2].unwrap_or(0) as u8;
                let b = rest[3].unwrap_or(0) as u8;
                self.set_color(fg, Color::Rgb(r, g, b));
                4
            }
            _ => 0,
        }
    }
}

/// SGR 基础 8 色（30..37/40..47 的偏移），颜色枚举再由 ratatui 后端输出。
fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

/// SGR 高亮 8 色（90..97/100..107 的偏移）。
fn bright_color(n: u16) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

#[cfg(test)]
/// 纯解析测试：以显式 ESC 字节和拆包序列验证样式，不依赖当前终端是否支持颜色。
mod tests {
    use super::*;

    /// 将样式片段还原成纯文本，便于同时断言字符结果和单独的颜色属性。
    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn parses_basic_colors() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"\x1b[32mOK\x1b[0m done\r\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "OK done");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].spans[1].style.fg, None, "reset 后回到默认色");
    }

    #[test]
    fn keeps_style_across_chunks() {
        let mut a = AnsiState::new();
        // 设备把 ESC[ 和 32m 拆在两包里，颜色仍须生效
        let first = a.feed(b"\x1b[3");
        assert!(first.is_empty(), "不完整序列不应产生行");
        let second = a.feed(b"2mOK\r\n");
        assert_eq!(text_of(&second[0]), "OK");
        assert_eq!(second[0].spans[0].style.fg, Some(Color::Green));
        // 颜色状态跨包保持：第二行不重新设置也应还是绿色
        let third = a.feed(b"still green\r\n");
        assert_eq!(third[0].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn splits_lines_and_handles_controls() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"a\tb\nc\x08d\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(text_of(&lines[0]), "a       b", "tab 补齐到 8 列");
        assert_eq!(text_of(&lines[1]), "d", "\\b 回退掉 c 再覆盖");
    }

    #[test]
    fn parses_256_and_truecolor() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"\x1b[38;5;196mA\x1b[48;2;1;2;3mB\x1b[0m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(196)));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn strips_osc_and_non_sgr_csi() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"\x1b[2J\x1b]0;title\x07hello");
        assert_eq!(text_of(&lines[0]), "hello");
    }

    #[test]
    fn modifiers_apply_and_clear() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"\x1b[1;4mX\x1b[22;24mY");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(lines[0].spans[1].style.add_modifier, Modifier::empty());
    }

    #[test]
    fn incomplete_osc_waits_for_next_chunk() {
        let mut a = AnsiState::new();
        assert!(a.feed(b"pre\x1b]0;tit").is_empty());
        let lines = a.feed(b"le\x07post\r\n");
        assert_eq!(text_of(&lines[0]), "prepost");
    }

    #[test]
    fn partial_line_keeps_tab_and_backspace_state_across_packets() {
        let mut a = AnsiState::new();
        assert_eq!(text_of(&a.feed(b"ab")[0]), "ab");
        assert_eq!(text_of(&a.feed(b"\x08c\tX")[0]), "ac      X");
        assert_eq!(text_of(&a.feed(b"\n")[0]), "ac      X");
        assert!(!a.partial_line());
    }

    #[test]
    fn extended_color_does_not_skip_following_reset() {
        let mut a = AnsiState::new();
        let lines = a.feed(b"\x1b[38;5;196;0mX");
        assert_eq!(lines[0].spans[0].style, Style::default());
    }
}
