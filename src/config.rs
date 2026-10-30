//! 配置文件支持：TOML 解析 + 优先级（命令行 > 配置文件 > 内置默认）合并。
//!
//! 配置文件字段与 CLI 参数同名，全部可选。未设置则回退到 CLI / 默认值。
//! 典型用法：`serial_mcp_tui.exe --config stm32.toml`。
//!
//! 本模块只负责反序列化，不打开串口、不启动线程，也不执行 CLI 优先级合并。
//! Option 的 None 表示“文件未指定”，不能在这里提前填默认值，否则 main 无法区分来源。
//! 未知 TOML 字段由 serde 忽略；字段类型错误和未知枚举值会返回解析错误。
//! theme 只在 TUI 创建时读取；热加载的生效范围由 main/tui 的调用方决定。

use std::path::PathBuf;

use serde::Deserialize;

/// 配置文件内容（全部可选字段）。
#[derive(Deserialize, Default, Debug, Clone)]
pub struct FileConfig {
    /// 设备名，例如 COM9；None 允许启动流程尝试连接系统唯一可用串口。
    pub port: Option<String>,
    /// 串口波特率；省略时由装配层回退到 115200。
    pub baud: Option<u32>,
    /// gui / tui / mcp / both（非法值回退 both，both 优先使用编译启用的 GUI）
    pub mode: Option<String>,
    /// 日志目录；相对路径按进程工作目录解析，不自动相对配置文件目录。
    pub log_dir: Option<PathBuf>,
    /// 文本日志单文件轮转阈值，单位字节。
    pub log_rotate: Option<u64>,
    /// 按文件修改时间清理旧 .log 文件；0 表示不按保留天数删除。
    pub log_keep_days: Option<u32>,
    /// 格式化后的待写文本达到此字节数时触发写盘。
    pub log_flush_bytes: Option<u64>,
    /// 两次批量写盘之间的时间阈值，单位毫秒。
    pub log_flush_ms: Option<u64>,
    /// MCP server HTTP 端口（扁平字段，兼容旧配置）
    pub mcp_port: Option<u16>,
    /// 界面配置（GUI 兼容沿用 `[tui]` 子表）
    pub tui: Option<TuiConfig>,
}

/// 界面配置（`[tui]` 子表，字段全部可选）；GUI 同步编码模式，input_hex 优先。
#[derive(Deserialize, Default, Debug, Clone)]
pub struct TuiConfig {
    /// 界面配色：dark / light / nord / solarized，默认 dark。
    pub theme: Option<Theme>,
    /// 文本命令行尾：none / cr / lf / crlf，默认 crlf。
    pub line_ending: Option<LineEnding>,
    /// 数据视图默认模式：true=hex，false=ascii（默认）
    pub hex_view: Option<bool>,
    /// 输入编码默认模式：true=hex，false=ascii（默认）
    pub input_hex: Option<bool>,
    /// TUI 最大行数；GUI 换算为每行 1024 个 UTF-16 单位的历史字符预算（默认 500）。
    pub max_lines: Option<usize>,
}

/// 固定界面配色的配置值。此处不依赖 ratatui，便于仅 MCP 构建继续解析同一份配置。
/// 实际 RGB 颜色映射位于 tui::Palette，名称使用小写以对应 TOML 字符串。
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Nord,
    Solarized,
}

impl Theme {
    /// 返回用于界面标题及配置说明的稳定名称，不执行动态切换。
    pub fn name(self) -> &'static str {
        match self { Self::Dark => "dark", Self::Light => "light", Self::Nord => "nord", Self::Solarized => "solarized" }
    }

}

#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LineEnding {
    None,
    Cr,
    Lf,
    #[default]
    Crlf,
}

impl LineEnding {
    /// 面向用户显示的名称；None 的含义是完全不追加字节。
    pub fn label(self) -> &'static str {
        match self { Self::None => "none", Self::Cr => "CR", Self::Lf => "LF", Self::Crlf => "CRLF" }
    }
    /// Ctrl+E 的循环顺序；主题没有相同的动态切换功能。
    pub fn next(self) -> Self {
        match self { Self::None => Self::Cr, Self::Cr => Self::Lf, Self::Lf => Self::Crlf, Self::Crlf => Self::None }
    }
    /// 仅由 TUI 文本发送路径调用；Hex 与 MCP 的显式字节发送不使用此规则。
    /// 已有 CR 或 LF 结尾时保持原样，不尝试补齐/转换 CRLF，避免重复回车。
    /// 空 Vec 也可以追加行尾，因此空输入 Enter 能唤出设备提示符。
    pub fn append(self, bytes: &mut Vec<u8>) {
        if matches!(bytes.last(), Some(b'\r' | b'\n')) { return; }
        bytes.extend_from_slice(match self { Self::None => b"", Self::Cr => b"\r", Self::Lf => b"\n", Self::Crlf => b"\r\n" });
    }
}

/// 读取并解析配置文件。文件不存在或 TOML 非法返回 Err。
pub fn load_file_config(path: &PathBuf) -> Result<FileConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read {} failed: {e}", path.display()))?;
    parse_config(&text).map_err(|e| format!("parse {} failed: {e}", path.display()))
}

/// 解析 TOML 文本（独立出便于单元测试）。
pub fn parse_config(text: &str) -> Result<FileConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// 文件修改时间（None = 文件不存在/不可读），用于热加载轮询。
pub fn file_mtime(path: &PathBuf) -> Option<std::time::SystemTime> {
    // 不区分“文件不存在”和“无法读取元数据”，监视器只关心它是否发生变化。
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
/// 直接解析内存 TOML，验证可选字段、未知键兼容性、枚举错误与行尾追加规则。
mod tests {
    use super::*;

    #[test]
    fn theme_config_accepts_each_theme_and_rejects_typos() {
        for name in ["dark", "light", "nord", "solarized"] {
            let cfg = parse_config(&format!("[tui]\ntheme = '{name}'\n")).unwrap();
            assert_eq!(cfg.tui.unwrap().theme.unwrap().name(), name);
        }
        assert!(parse_config("[tui]\ntheme = 'typo'\n").is_err());
    }

    #[test]
    fn command_line_endings_are_configurable_without_duplicates() {
        let mut command = b"help".to_vec();
        LineEnding::default().append(&mut command);
        assert_eq!(command, b"help\r\n");
        let mut enter = Vec::new();
        LineEnding::default().append(&mut enter);
        assert_eq!(enter, b"\r\n");
        LineEnding::default().append(&mut command);
        assert_eq!(command, b"help\r\n");
        for (ending, suffix) in [(LineEnding::None, &b""[..]), (LineEnding::Cr, &b"\r"[..]), (LineEnding::Lf, &b"\n"[..])] {
            let mut bytes = b"help".to_vec();
            ending.append(&mut bytes);
            assert_eq!(&bytes[4..], suffix);
        }
        assert_eq!(parse_config("[tui]\nline_ending = 'cr'\n").unwrap().tui.unwrap().line_ending, Some(LineEnding::Cr));
    }

    #[test]
    fn parse_toml_all_fields() {
        let cfg = parse_config(
            r#"
            port = "COM3"
            baud = 9600
            mode = "mcp"
            log_dir = "mylogs"
            log_rotate = 1000
            log_keep_days = 7
            log_flush_bytes = 128
            log_flush_ms = 50
            mcp_port = 9000
            "#,
        )
        .unwrap();
        assert_eq!(cfg.port.as_deref(), Some("COM3"));
        assert_eq!(cfg.baud, Some(9600));
        assert_eq!(cfg.mode.as_deref(), Some("mcp"));
        assert_eq!(cfg.log_dir.as_deref(), Some(PathBuf::from("mylogs").as_path()));
        assert_eq!(cfg.log_rotate, Some(1000));
        assert_eq!(cfg.log_keep_days, Some(7));
        assert_eq!(cfg.log_flush_bytes, Some(128));
        assert_eq!(cfg.log_flush_ms, Some(50));
        assert_eq!(cfg.mcp_port, Some(9000));
    }

    #[test]
    fn partial_toml_defaults_to_none() {
        let cfg = parse_config("baud = 115200\n").unwrap();
        assert_eq!(cfg.baud, Some(115200));
        assert!(cfg.port.is_none());
        assert!(cfg.mcp_port.is_none());
        assert!(cfg.log_dir.is_none());
    }

    #[test]
    fn invalid_toml_reports_error() {
        assert!(parse_config("port = \n").is_err());
        assert!(parse_config("baud = \"not-a-number\"\n").is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg = parse_config("port = \"COM5\"\nfoo = 42\n").unwrap();
        assert_eq!(cfg.port.as_deref(), Some("COM5"));
    }

    #[test]
    fn parse_tui_subtable() {
        let cfg = parse_config(
            r#"
            port = "COM3"
            mcp_port = 8010

            [tui]
            hex_view = true
            input_hex = true
            max_lines = 1000
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mcp_port, Some(8010), "flat mcp_port keeps working");
        let tui = cfg.tui.expect("[tui] table should parse");
        assert_eq!(tui.hex_view, Some(true));
        assert_eq!(tui.input_hex, Some(true));
        assert_eq!(tui.max_lines, Some(1000));
    }

    #[test]
    fn tui_table_optional() {
        let cfg = parse_config("baud = 9600\n").unwrap();
        assert!(cfg.tui.is_none(), "no [tui] table -> None");
        let cfg2 = parse_config("[tui]\nmax_lines = 200\n").unwrap();
        let tui = cfg2.tui.unwrap();
        assert_eq!(tui.max_lines, Some(200));
        assert_eq!(tui.hex_view, None, "missing keys default to None");
    }
}
