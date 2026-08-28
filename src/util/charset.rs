//! 编码探测与解码：Content-Type charset 优先，其次 chardetng 探测（GBK/Big5 书站必备）。

use encoding_rs::Encoding;

/// 解码响应字节为字符串。
///
/// 优先级：`Content-Type` 头中的 charset > chardetng 探测 > UTF-8（有损）。
pub fn decode_bytes(bytes: &[u8], content_type: Option<&str>) -> String {
    // 1. Content-Type 头声明的 charset
    if let Some(encoding) = content_type.and_then(charset_from_header) {
        let (decoded, _, _) = encoding.decode(bytes);
        return decoded.into_owned();
    }
    // 2. chardetng 探测（top_hint 置 true：面向中文场景优先 CJK 编码）
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    let (decoded, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        tracing::debug!(?encoding, "探测编码存在非法字节（已替换）");
    }
    decoded.into_owned()
}

/// 从 `Content-Type` 头解析 charset 标签。
fn charset_from_header(content_type: &str) -> Option<&'static Encoding> {
    let idx = content_type.to_ascii_lowercase().find("charset=")?;
    let label = content_type[idx + "charset=".len()..].trim();
    // 去掉可能的引号与后续参数
    let label = label.split([';', ',']).next().unwrap_or(label).trim_matches('"');
    Encoding::for_label(label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GBK_BYTES: &[u8] = &[0xd6, 0xd0, 0xce, 0xc4]; // "中文" 的 GBK 编码

    #[test]
    fn decode_gbk_without_header_detects_encoding() {
        let s = decode_bytes(GBK_BYTES, None);
        assert_eq!(s, "中文");
    }

    #[test]
    fn decode_gbk_with_header_uses_declared_charset() {
        let s = decode_bytes(GBK_BYTES, Some("text/html; charset=GBK"));
        assert_eq!(s, "中文");
    }

    #[test]
    fn decode_utf8_passthrough() {
        let s = decode_bytes("中文".as_bytes(), Some("text/html; charset=utf-8"));
        assert_eq!(s, "中文");
    }
}
