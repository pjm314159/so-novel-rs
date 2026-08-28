//! 章节正文抓取（对应源项目 `ChapterParser`）：限速、重试（间隔递增）、分页正文拼接。
//!
//! `scraper::Html` 不跨 await：每页 body 到手后在同步块内完成解析与提取。

use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::sleep;

use super::{fetch_body_with_cf, random_interval, BoxError, CrawlParams};
use crate::rule::{selector, ChapterRule, Rule};

/// 抓取单个章节正文（含重试，对应源项目 `ChapterParser.parse` + `retry`）。
///
/// # Errors
/// 首次与全部重试均失败时返回最后一次错误；未启用重试时直接返回首次错误。
pub(crate) async fn fetch_chapter(
    client: &reqwest::Client,
    rule: &Rule,
    params: &CrawlParams,
    cf_bypass: &str,
    url: &str,
    title: &str,
) -> Result<String, BoxError> {
    let r = &rule.chapter;
    let interval = random_interval(params.min_interval, params.max_interval);
    match fetch_content(client, r, cf_bypass, url, interval).await {
        Ok(content) => Ok(content),
        Err(first) => {
            // 设计文档 §10：章节失败不中断整本书；未启用重试时仅记录失败
            if !params.enable_retry || params.max_retries == 0 {
                return Err(first);
            }
            let mut last = first;
            for attempt in 1_u32..=params.max_retries {
                // 重试间隔递增（对应源项目 randomInterval(retry) * attempt）
                let retry_interval =
                    random_interval(params.retry_min_interval, params.retry_max_interval) * attempt;
                match fetch_content(client, r, cf_bypass, url, retry_interval).await {
                    Ok(content) => {
                        tracing::info!(chapter = title, attempt, "章节重试成功");
                        return Ok(content);
                    }
                    Err(e) => {
                        tracing::warn!(chapter = title, attempt, error = %e, "章节重试失败");
                        last = e;
                    }
                }
            }
            Err(last)
        }
    }
}

/// 抓取正文（单页或分页，对应源项目 `fetchContent`）。
async fn fetch_content(
    client: &reqwest::Client,
    r: &ChapterRule,
    cf_bypass: &str,
    url: &str,
    interval: Duration,
) -> Result<String, BoxError> {
    // 请求间隔（源项目在每次请求前 sleep）
    sleep(interval).await;
    let timeout = u64::from(r.timeout.unwrap_or(15));
    if r.next_page.is_empty() {
        let body = fetch_body_with_cf(client, url, timeout, cf_bypass).await?;
        extract_page_content(r, &body, url)
    } else {
        fetch_paginated_content(client, r, cf_bypass, url, timeout, interval).await
    }
}

/// 提取单页正文（同步块）。
///
/// # Errors
/// 正文选择器无匹配（源项目断言语义）时返回错误。
pub(crate) fn extract_page_content(r: &ChapterRule, body: &str, url: &str) -> Result<String, BoxError> {
    let doc = scraper::Html::parse_document(body);
    // DSL 前先删 filterTag 杂质（如 wxsy：h3/div 与 base64 同行会导致解码失败）
    let content =
        selector::extract_html_with_filter(&doc, &r.content, &r.filter_tag).ok().flatten().unwrap_or_default();
    if content.is_empty() {
        return Err(format!("正文内容为空: {url}").into());
    }
    Ok(content)
}

/// 分页正文：循环抓取拼接，直至判定最后一页（对应源项目 `fetchPaginatedContent`）。
async fn fetch_paginated_content(
    client: &reqwest::Client,
    r: &ChapterRule,
    cf_bypass: &str,
    start_url: &str,
    timeout: u64,
    interval: Duration,
) -> Result<String, BoxError> {
    let mut next_url = start_url.to_owned();
    let mut content = String::new();
    loop {
        let body = fetch_body_with_cf(client, &next_url, timeout, cf_bypass).await?;
        // 同步块：提取正文 + 下一页候选链接与按钮文本
        let (page_content, candidate, next_text) = {
            let doc = scraper::Html::parse_document(&body);
            let page_content = selector::extract_html_with_filter(&doc, &r.content, &r.filter_tag)
                .ok()
                .flatten()
                .unwrap_or_default();
            let next_els = selector::select_all(&doc, &r.next_page).unwrap_or_default();
            let candidate = if !r.next_page_in_js.is_empty() {
                // 下一页链接位于脚本中（对应源项目 resolveNextUrl 的 JS 分支）
                selector::extract_html(&doc, &r.next_page_in_js).ok().flatten()
            } else if next_els.is_empty() {
                tracing::error!(url = %next_url, "分页章节正文获取为空，可能被限流");
                None
            } else {
                Some(
                    next_els[0]
                        .value()
                        .attr("href")
                        .map_or_else(String::new, |v| selector::absolutize(v, &r.base_uri)),
                )
            };
            let next_text: String =
                next_els.iter().map(|el| el.text().collect::<String>()).collect::<String>();
            (page_content, candidate, next_text)
        };
        if page_content.is_empty() {
            return Err(format!("正文内容为空: {next_url}").into());
        }
        content.push_str(&page_content);

        if is_last_page(r, candidate.as_deref(), &next_text) {
            break;
        }
        // 空候选但未判定为最后一页：视为异常终止，避免死循环
        let Some(candidate) = candidate.filter(|s| !s.is_empty()) else {
            return Err(format!("分页正文无法获取下一页链接: {next_url}").into());
        };
        next_url = candidate;
        // 获取下一分页的间隔
        sleep(interval).await;
    }
    Ok(content)
}

/// 通用分页结束判定正则（`.*[-_]\d\.html` 全串匹配，Java `String.matches` 语义需锚定）
fn generic_page_end() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^(?:.*[-_]\d\.html)$").expect("常量正则恒合法"))
}

/// 分页结束判定（对应源项目 `ChapterParser.isLastPage`）。
fn is_last_page(r: &ChapterRule, next_url: Option<&str>, next_text: &str) -> bool {
    let Some(next) = next_url.filter(|s| !s.is_empty()) else {
        return true;
    };
    // 规则级：下一章链接正则（全串匹配）
    if !r.next_chapter_link.is_empty() {
        if let Ok(re) = regex::Regex::new(&format!("^(?:{})$", r.next_chapter_link)) {
            if re.is_match(next) {
                return true;
            }
        }
    }
    // 通用规则：URL 不以 "_个位数字.html" 结尾，且按钮文本为 下一章/没有了/>>/书末页
    let not_numbered_page = !generic_page_end().is_match(next);
    let end_text = next_text.contains("下一章")
        || next_text.contains("没有了")
        || next_text.contains(">>")
        || next_text.contains("书末页");
    not_numbered_page && end_text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_last_page_none_or_empty_url() {
        let r = ChapterRule::default();
        assert!(is_last_page(&r, None, "下一页"));
        assert!(is_last_page(&r, Some(""), "下一页"));
    }

    #[test]
    fn is_last_page_numbered_url_continues() {
        let r = ChapterRule::default();
        // URL 以 _2.html 结尾 → 未到最后一页
        assert!(!is_last_page(&r, Some("https://x.cc/c1_2.html"), "下一页"));
    }

    #[test]
    fn is_last_page_generic_end_text() {
        let r = ChapterRule::default();
        // 非 _数字.html 结尾 + “下一章”文本 → 最后一页
        assert!(is_last_page(&r, Some("https://x.cc/c2.html"), "下一章"));
        assert!(is_last_page(&r, Some("https://x.cc/c2.html"), "没有了"));
        // 文本为“下一页”不算结束
        assert!(!is_last_page(&r, Some("https://x.cc/c2.html"), "下一页"));
    }

    #[test]
    fn is_last_page_rule_regex_anchored_full_match() {
        let r = ChapterRule { next_chapter_link: r".*next_chapter.*".to_owned(), ..Default::default() };
        // Java matches 为全串匹配："other_next_chapter" 不应命中 .*next_chapter.*？
        // 锚定后 .* 前缀可覆盖全串，故包含即可命中（与 Java 语义一致）
        assert!(is_last_page(&r, Some("https://x.cc/next_chapter/9"), ""));
    }

    #[test]
    fn extract_page_content_extracts_inner_html() {
        let r = ChapterRule { content: "#content".to_owned(), ..Default::default() };
        let body = r#"<html><body><div id="content"><p>第一段</p><p>第二段</p></div></body></html>"#;
        let content = extract_page_content(&r, body, "https://x.cc/c1").expect("提取失败");
        assert!(content.contains("<p>第一段</p>"), "应保留内部 HTML: {content}");
    }

    #[test]
    fn extract_page_content_errors_when_empty() {
        let r = ChapterRule { content: "#not-exist".to_owned(), ..Default::default() };
        assert!(extract_page_content(&r, "<html><body></body></html>", "https://x.cc/c1").is_err());
    }
}
