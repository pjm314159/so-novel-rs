//! 书籍详情解析（对应源项目 `BookParser`）。

use super::{fetch_body_with_cf, Book, BoxError};
use crate::config::AppConfig;
use crate::rule::{selector, Rule};

/// 解析书籍详情页（对应源项目 `BookParser.parse`，网络抓取 + 解析）。
///
/// # Errors
/// 网络失败或书名/作者为空（源项目断言语义）时返回错误。
pub async fn parse(
    config: &AppConfig,
    client: &reqwest::Client,
    rule: &Rule,
    url: &str,
) -> Result<Book, BoxError> {
    let r = &rule.book;
    let timeout = u64::from(r.timeout.unwrap_or(15));
    let body = fetch_body_with_cf(client, url, timeout, config.global.cf_bypass.as_str()).await?;
    parse_body(rule, &body)
}

/// 解析详情页正文（同步：`scraper::Html` 不跨 await）。
///
/// # Errors
/// 书名或作者为空时返回错误。
pub(crate) fn parse_body(rule: &Rule, body: &str) -> Result<Book, BoxError> {
    let r = &rule.book;
    let doc = scraper::Html::parse_document(body);
    let field = |expr: &str| -> String {
        selector::extract(&doc, expr, &r.base_uri).ok().flatten().unwrap_or_default()
    };

    let book_name = field(&r.book_name);
    let author = field(&r.author);
    if book_name.is_empty() || author.is_empty() {
        return Err(format!("详情页书名或作者不能为空（bookName={book_name:?}, author={author:?}）").into());
    }

    // TODO(M5): 简繁转换（ChineseConverter.convert）；封面升级（CoverUpdater，needProxy 源用源站封面）
    Ok(Book {
        book_name,
        // 源项目对作者字段的固定清洗
        author: author.replace("作者：", ""),
        // StrUtil.cleanBlank：去全部空白
        intro: field(&r.intro).chars().filter(|c| !c.is_whitespace()).collect(),
        cover_url: field(&r.cover_url),
        category: field(&r.category),
        latest_chapter: field(&r.latest_chapter),
        latest_chapter_url: field(&r.latest_chapter_url),
        last_update_time: field(&r.last_update_time).replace("更新时间：", "").replace("最后更新：", ""),
        status: field(&r.status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"<html><head>
        <meta property="og:novel:book_name" content="斗破苍穹"/>
        <meta property="og:novel:author" content="作者：天蚕土豆"/>
        <meta name="description" content=" 萧炎的故事  "/>
        <meta property="og:image" content="https://cdn.x.cc/cover.jpg"/>
        <meta property="og:novel:category" content="玄幻"/>
        <meta property="og:novel:status" content="连载中"/>
        <meta property="og:novel:update_time" content="更新时间：2026-01-01"/>
        </head><body></body></html>"#;

    #[test]
    fn parse_body_extracts_meta_fields() {
        let mut rule = Rule::default();
        rule.book.book_name = r#"meta[property="og:novel:book_name"]"#.into();
        rule.book.author = r#"meta[property="og:novel:author"]"#.into();
        rule.book.intro = r#"meta[name="description"]"#.into();
        rule.book.cover_url = r#"meta[property="og:image"]"#.into();
        rule.book.category = r#"meta[property="og:novel:category"]"#.into();
        rule.book.status = r#"meta[property="og:novel:status"]"#.into();
        rule.book.last_update_time = r#"meta[property="og:novel:update_time"]"#.into();
        let book = parse_body(&rule, BODY).expect("解析失败");
        assert_eq!(book.book_name, "斗破苍穹");
        assert_eq!(book.author, "天蚕土豆");
        assert_eq!(book.intro, "萧炎的故事");
        assert_eq!(book.cover_url, "https://cdn.x.cc/cover.jpg");
        assert_eq!(book.last_update_time, "2026-01-01");
    }

    #[test]
    fn parse_body_requires_book_name_and_author() {
        let rule = Rule::default();
        let err = parse_body(&rule, "<html><body></body></html>").expect_err("应报错");
        assert!(err.to_string().contains("书名或作者"));
    }
}
