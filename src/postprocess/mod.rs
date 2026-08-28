//! 合并输出（后处理）：txt 流式拼接 / EPUB 生成。
//!
//! TODO(M4): 实现合并（流式读一章写一章，峰值内存 = 单章 + 缓冲区）。

/// 后处理格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Txt,
    Epub,
}

impl OutputFormat {
    /// 从配置字符串解析（与源项目 config extname 一致）
    ///
    /// # Errors
    /// 不支持的格式（含已裁剪的 html/pdf）返回错误信息
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "txt" => Ok(Self::Txt),
            "epub" => Ok(Self::Epub),
            other => Err(format!("不支持的输出格式: {other}（仅支持 txt/epub）")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_txt_epub_case_insensitive() {
        assert_eq!(OutputFormat::parse("txt").unwrap(), OutputFormat::Txt);
        assert_eq!(OutputFormat::parse("EPUB").unwrap(), OutputFormat::Epub);
    }

    #[test]
    fn parse_rejects_removed_formats() {
        assert!(OutputFormat::parse("pdf").is_err());
        assert!(OutputFormat::parse("html").is_err());
        assert!(OutputFormat::parse("").is_err());
    }
}
