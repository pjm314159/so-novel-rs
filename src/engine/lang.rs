//! 简繁转换（对应源项目 `ChineseConverter`，`HanLP` → opencc-fmmseg 纯 Rust）。
//!
//! 触发条件（与源项目一致）：`rule.language != config.language` 且方向受支持；
//! `config.language` 为空（自动）时不转换。
//!
//! 转换器全局懒加载（`OnceLock`）：词库仅在首次转换时解压，空闲零开销。

use std::sync::OnceLock;

use opencc_fmmseg::OpenCC;

use super::search::SearchResult;
use super::Book;

/// 转换方向（与源项目 `HanLP` 函数一一对应）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    /// 繁体（台湾/通用）→ 简体（HanLP `t2s`）
    T2s,
    /// 简体 → 台湾正体（HanLP `s2tw`）
    S2tw,
    /// 简体 → 传统繁体（HanLP `s2t`）
    S2t,
    /// 传统繁体 → 台湾正体（HanLP `t2tw`）
    T2tw,
}

/// 解析转换方向：不受支持的组合返回 `None`（与源项目 switch default 一致）。
pub fn conversion_for(source_lang: &str, target_lang: &str) -> Option<Conversion> {
    match (source_lang, target_lang) {
        ("zh-TW" | "zh-Hant", "zh-CN") => Some(Conversion::T2s),
        ("zh-CN", "zh-TW") => Some(Conversion::S2tw),
        ("zh-CN", "zh-Hant") => Some(Conversion::S2t),
        ("zh-Hant", "zh-TW") => Some(Conversion::T2tw),
        _ => None,
    }
}

/// 获取全局转换器（首次调用解压内嵌词库；`OpenCC` 字段均为 `OnceLock`/`Arc`，线程安全）
fn converter() -> &'static OpenCC {
    static CC: OnceLock<OpenCC> = OnceLock::new();
    CC.get_or_init(OpenCC::new)
}

/// 转换单个字符串（HanLP 不转换标点，故第二参数传 `false`）
pub fn convert_str(s: &str, c: Conversion) -> String {
    let cc = converter();
    match c {
        Conversion::T2s => cc.t2s(s, false),
        Conversion::S2tw => cc.s2tw(s, false),
        Conversion::S2t => cc.s2t(s, false),
        Conversion::T2tw => cc.t2tw(s),
    }
}

fn conv_field(s: &mut String, c: Conversion) {
    if !s.is_empty() {
        *s = convert_str(s, c);
    }
}

/// 书籍字段转换（字段集与源项目 `applyConversion` 的 Book 分支一致）
pub fn convert_book_fields(book: &mut Book, c: Conversion) {
    conv_field(&mut book.book_name, c);
    conv_field(&mut book.author, c);
    conv_field(&mut book.intro, c);
    conv_field(&mut book.category, c);
    conv_field(&mut book.latest_chapter, c);
    conv_field(&mut book.last_update_time, c);
    conv_field(&mut book.status, c);
}

/// 章节标题与正文转换（源项目 Chapter 分支：title + content）
pub fn convert_chapter_fields(title: &mut String, content: &mut String, c: Conversion) {
    conv_field(title, c);
    conv_field(content, c);
}

/// 搜索结果字段转换（源项目 `SearchResult` 分支，重写版字段集为现有字段）
pub fn convert_search_result(sr: &mut SearchResult, c: Conversion) {
    conv_field(&mut sr.book_name, c);
    conv_field(&mut sr.author, c);
    conv_field(&mut sr.category, c);
    conv_field(&mut sr.latest_chapter, c);
    conv_field(&mut sr.last_update_time, c);
    conv_field(&mut sr.status, c);
    conv_field(&mut sr.word_count, c);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_for_maps_supported_directions_only() {
        assert_eq!(conversion_for("zh-TW", "zh-CN"), Some(Conversion::T2s));
        assert_eq!(conversion_for("zh-Hant", "zh-CN"), Some(Conversion::T2s));
        assert_eq!(conversion_for("zh-CN", "zh-TW"), Some(Conversion::S2tw));
        assert_eq!(conversion_for("zh-CN", "zh-Hant"), Some(Conversion::S2t));
        assert_eq!(conversion_for("zh-Hant", "zh-TW"), Some(Conversion::T2tw));
        // 源项目不支持的方向与空语言（自动）
        assert_eq!(conversion_for("zh-TW", "zh-Hant"), None);
        assert_eq!(conversion_for("zh-CN", ""), None);
        assert_eq!(conversion_for("", "zh-CN"), None);
    }

    #[test]
    fn convert_str_roundtrips_simplified_to_traditional() {
        let s = "汉字转换测试";
        let t = convert_str(s, Conversion::S2t);
        assert_eq!(t, "漢字轉換測試");
        let back = convert_str(&t, Conversion::T2s);
        assert_eq!(back, s);
    }

    #[test]
    fn convert_book_fields_converts_all_fields() {
        let mut book = Book {
            book_name: "斗破苍穹".into(),
            author: "天蚕土豆".into(),
            intro: "萧炎的故事".into(),
            category: "玄幻".into(),
            latest_chapter: "第1章".into(),
            last_update_time: "2026-01-01".into(),
            status: "连载中".into(),
            ..Default::default()
        };
        convert_book_fields(&mut book, Conversion::S2t);
        assert_eq!(book.book_name, "鬥破蒼穹");
        assert_eq!(book.author, "天蠶土豆");
        assert_eq!(book.latest_chapter, "第1章");
    }
}
