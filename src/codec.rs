//! 编码工具：hex 字符串解析 + ascii 转义解码（TUI 与 MCP 共用一份实现）。
//!
//! 所有函数只转换内存数据，不访问设备，不补行尾，也不修改调用者的输入文本。
//! Hex 严格校验；文本转义采取宽容策略，未知/不完整转义按原字节保留。
//! 因此同一个输入经过 TUI 或 MCP 解码后，除 TUI 显式的行尾追加外应得到相同字节。

/// 解析 hex 字符串（`01 03 00 00`、`01030000`、`0x01 0x03`）为字节。
/// 空白可分隔任意数量的 token；每个 token 只去掉小写 `0x` 前缀，再连接成连续数字。
/// 空输入返回空 Vec；奇数位或非十六进制字符返回 Err，不返回部分解析结果。
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let compact: String = s
        .split_whitespace()
        .flat_map(|t| t.strip_prefix("0x").unwrap_or(t).chars())
        .collect();
    if compact.is_empty() {
        return Ok(vec![]);
    }
    if compact.len() % 2 != 0 {
        return Err("odd number of hex digits".into());
    }
    let mut out = Vec::with_capacity(compact.len() / 2);
    let bytes = compact.as_bytes();
    // 按字节读取可避免按 UTF-8 下标切片引发 panic；非 ASCII 字节会被 hex_val 拒绝。
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_val(bytes[i]).ok_or("invalid hex char")?;
        let lo = hex_val(bytes[i + 1]).ok_or("invalid hex char")?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// 解码 ascii 输入中的转义序列：`\r` `\n` `\t` `\0` `\\` `\xNN`。
/// 未知组合（如 `\p`）原样保留反斜杠，不做静默改写。非 ASCII 字符按 UTF-8 原样透传。
/// `i` 始终指向尚未消费的输入字节。识别成功时跨过完整转义，失败时只消费反斜杠，
/// 让后续字符继续走普通分支。例如不完整 `\xZ1` 会逐字节恢复为原文。
pub fn decode_escapes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'r') => {
                out.push(b'\r');
                i += 2;
            }
            Some(b'n') => {
                out.push(b'\n');
                i += 2;
            }
            Some(b't') => {
                out.push(b'\t');
                i += 2;
            }
            Some(b'0') => {
                out.push(0);
                i += 2;
            }
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(b'x') | Some(b'X') => match (bytes.get(i + 2), bytes.get(i + 3)) {
                (Some(&hi), Some(&lo)) => match (hex_val(hi), hex_val(lo)) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 4;
                    }
                    _ => {
                        out.push(b'\\');
                        i += 1;
                    }
                },
                _ => {
                    out.push(b'\\');
                    i += 1;
                }
            },
            _ => {
                out.push(b'\\');
                i += 1;
            }
        }
    }
    out
}

/// 将单个 ASCII 十六进制数字映射到 0..15；同时服务于 Hex 帧和 \xNN 转义。
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
/// 同时覆盖合法编码、错误 Hex、未知转义、尾部反斜杠和中文 UTF-8 透传。
mod tests {
    use super::*;

    #[test]
    fn hex_parse_variants() {
        assert_eq!(parse_hex("01 03 00 00").unwrap(), vec![0x01, 0x03, 0x00, 0x00]);
        assert_eq!(parse_hex("01030000").unwrap(), vec![0x01, 0x03, 0x00, 0x00]);
        assert_eq!(parse_hex("0x01 0x03").unwrap(), vec![0x01, 0x03]);
        assert_eq!(parse_hex("01 03\tab").unwrap(), vec![0x01, 0x03, 0xAB]);
        assert_eq!(parse_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(parse_hex("0A0B0C").unwrap(), vec![0x0A, 0x0B, 0x0C]);
        assert!(parse_hex("01 0").is_err(), "odd digits must error");
        assert!(parse_hex("gg").is_err(), "invalid chars must error");
        assert!(parse_hex("0x1").is_err());
    }

    #[test]
    fn escapes_decode_to_bytes() {
        assert_eq!(decode_escapes("AT\\r\\n"), b"AT\r\n".to_vec());
        assert_eq!(decode_escapes("a\\tb\\0c"), vec![b'a', b'\t', b'b', 0, b'c']);
        assert_eq!(decode_escapes("\\\\"), vec![b'\\'], "escaped backslash");
        assert_eq!(decode_escapes("\\x0D\\x0a"), vec![0x0D, 0x0A]);
    }

    #[test]
    fn escapes_keep_unknown_and_trailing() {
        // 未知转义原样保留，不做静默丢字符
        assert_eq!(decode_escapes("\\p"), b"\\p".to_vec());
        assert_eq!(decode_escapes("x\\"), b"x\\".to_vec(), "trailing backslash kept");
        // 半截 \x 也原样保留
        assert_eq!(decode_escapes("\\xZ1"), b"\\xZ1".to_vec());
    }

    #[test]
    fn escapes_keep_utf8_and_spaces() {
        assert_eq!(decode_escapes(" 中 "), " 中 ".as_bytes().to_vec());
    }
}
