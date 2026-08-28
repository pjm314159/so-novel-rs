//! 目录解析（对应源项目 `TocParser`）。
//!
//! 流程：详情页 URL 提取书籍 ID → 生成目录 URL → 分页 URL 收集（下拉菜单一次性 /
//! 下一页按钮递归）→ 并发抓取目录页（5 并发）→ 逐页提取章节条目（`isDesc` 倒序）→
//! 按章节名去重（后者替换前者）→ 顺序编号。

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

use super::{fetch_body_with_cf, random_interval, BoxError, Chapter};
use crate::config::AppConfig;
use crate::rule::{selector, Rule, TocRule};

/// 未解析顺序的目录条目（编号在去重后统一分配）
#[derive(Debug, Clone, PartialEq)]
struct RawItem {
    title: String,
    url: String,
}

/// 解析全部章节目录（对应源项目 `TocParser.parseAll`）。
///
/// # Errors
/// 规则正则非法、网络失败或目录页全部解析失败时返回错误。
pub async fn parse_all(
    config: &AppConfig,
    client: &reqwest::Client,
    rule: &Rule,
    book_url: &str,
) -> Result<Vec<Chapter>, BoxError> {
    let r = &rule.toc;
    let timeout = u64::from(r.timeout.unwrap_or(60));
    let cf_bypass = config.global.cf_bypass.as_str();

    // 从详情页 URL 提取书籍 ID（book.url 为捕获组正则，取 @js: 前部分）
    let id = extract_book_id(&rule.book.url, book_url);
    let mut base_uri = r.base_uri.clone();
    let mut url = book_url.to_owned();
    if let Some(id) = &id {
        base_uri = base_uri.replace("%s", id);
        if !r.url.is_empty() {
            url = r.url.replace("%s", id);
        }
    } else if !r.url.is_empty() {
        url = r.url.clone();
    }

    // 分页 URL 收集（保持插入顺序去重）
    let mut urls: Vec<String> = vec![url.clone()];
    if !r.next_page.is_empty() {
        let body = fetch_body_with_cf(client, &url, timeout, cf_bypass).await?;
        collect_pagination_urls(config, client, &mut urls, body, r, &base_uri).await?;
    }

    // 并发抓取目录页（5 并发，对应源项目 VirtualThreadLimiter(5)）
    let sem = Arc::new(Semaphore::new(5));
    let mut set: JoinSet<(usize, Result<Vec<RawItem>, BoxError>)> = JoinSet::new();
    for (i, page_url) in urls.iter().enumerate() {
        let permit = sem.clone().acquire_owned().await?;
        let client = client.clone();
        let page_url = page_url.clone();
        let r = r.clone();
        let base = base_uri.clone();
        let cf = cf_bypass.to_owned();
        set.spawn(async move {
            let result = fetch_body_with_cf(&client, &page_url, timeout, &cf)
                .await
                .map(|body| extract_items(&body, &r, &base));
            drop(permit);
            (i, result)
        });
    }
    let mut pages: Vec<Option<Vec<RawItem>>> = vec![None; urls.len()];
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((i, Ok(items))) => pages[i] = Some(items),
            Ok((i, Err(e))) => tracing::warn!(page = %urls[i], error = %e, "目录页解析失败"),
            Err(e) => tracing::warn!(error = %e, "目录页任务 join 失败"),
        }
    }

    // 逐页拼接（isDesc 时页内倒序），去重后统一编号
    let mut items: Vec<RawItem> = Vec::new();
    for page in pages.into_iter().flatten() {
        if r.is_desc {
            items.extend(page.into_iter().rev());
        } else {
            items.extend(page);
        }
    }
    let deduped = dedup_by_title(items);
    Ok(deduped
        .into_iter()
        .enumerate()
        .filter_map(|(i, raw)| {
            u32::try_from(i + 1).ok().map(|index| Chapter { index, title: raw.title, url: raw.url })
        })
        .collect())
}

/// 从详情页 URL 提取书籍 ID（对应源项目 `ReUtil.getGroup1(subBefore(bookUrl, "@js:"), url)`）。
fn extract_book_id(book_url_pattern: &str, url: &str) -> Option<String> {
    let pattern = book_url_pattern.split_once("@js:").map_or(book_url_pattern, |(head, _)| head);
    if pattern.is_empty() {
        return None;
    }
    let re = regex::Regex::new(pattern).ok()?;
    re.captures(url).and_then(|c| c.get(1)).map(|m| m.as_str().to_owned())
}

/// 收集目录分页 URL：优先下拉菜单一次性获取，否则按"下一页"递归翻页。
async fn collect_pagination_urls(
    config: &AppConfig,
    client: &reqwest::Client,
    urls: &mut Vec<String>,
    first_body: String,
    r: &TocRule,
    base_uri: &str,
) -> Result<(), BoxError> {
    let timeout = u64::from(r.timeout.unwrap_or(60));
    let cf_bypass = config.global.cf_bypass.as_str();

    let mut is_recursive = false;
    {
        let doc = scraper::Html::parse_document(&first_body);
        if let Some(list) = dropdown_urls(&doc, &r.next_page, base_uri) {
            // remove + add 语义：后加入的覆盖前面已有元素并保持顺序（个别书源 toc.url 不一定是首个 option）
            for u in list {
                urls.retain(|x| x != &u);
                urls.push(u);
            }
        } else {
            is_recursive = true;
        }
    }

    if is_recursive {
        let mut body = first_body;
        loop {
            let next = {
                let doc = scraper::Html::parse_document(&body);
                next_page_url(&doc, &r.next_page, base_uri)
            };
            let Some(next) = next.filter(|u| is_http_url(u)) else { break };
            urls.push(next.clone());
            sleep(random_interval(config.crawl.min_interval, config.crawl.max_interval)).await;
            body = fetch_body_with_cf(client, &next, timeout, cf_bypass).await?;
        }
    }
    Ok(())
}

/// 下拉菜单式分页：匹配元素含 `value` 属性时一次性收集全部分页 URL（覆盖绝大多数书源）。
fn dropdown_urls(doc: &scraper::Html, next_page: &str, base_uri: &str) -> Option<Vec<String>> {
    let elements = selector::select_all(doc, next_page).ok()?;
    if elements.is_empty() || !elements.iter().any(|el| el.value().attr("value").is_some()) {
        return None;
    }
    let attr = if elements.iter().any(|el| el.value().attr("href").is_some()) { "href" } else { "value" };
    Some(
        elements
            .iter()
            .filter_map(|el| el.value().attr(attr).map(|v| selector::absolutize(v, base_uri)))
            .collect(),
    )
}

/// 下一页按钮式分页：取首页元素的 href（缺省 value）。
fn next_page_url(doc: &scraper::Html, next_page: &str, base_uri: &str) -> Option<String> {
    let href = selector::extract(doc, &format!("{next_page}@href"), base_uri)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    if href.is_some() {
        return href;
    }
    selector::extract(doc, &format!("{next_page}@value"), base_uri).ok().flatten().filter(|s| !s.is_empty())
}

/// 解析单页目录条目（对应源项目 `TocParser.extractElements` + `addChapter`）。
///
/// `toc.list` 非空时先取容器内部 HTML（执行 `@js:` 预处理）再解析条目；
/// 内层文档继承外层 `base_uri`：item 的 `href` 通常为相对路径
/// （如 wxsy 的 `/novel/x/read_y.html`），不绝对化则下载器无法请求。
/// `list` 为空时直接在页面文档上选条目。
fn extract_items(body: &str, r: &TocRule, base_uri: &str) -> Vec<RawItem> {
    let doc = scraper::Html::parse_document(body);
    let to_items = |elements: Vec<scraper::ElementRef<'_>>, item_base: &str| {
        elements
            .into_iter()
            .map(|el| RawItem {
                title: el.text().collect::<String>().trim().to_owned(),
                url: el.value().attr("href").map(|v| selector::absolutize(v, item_base)).unwrap_or_default(),
            })
            .collect()
    };
    if r.list.is_empty() {
        to_items(selector::select_all(&doc, &r.item).unwrap_or_default(), base_uri)
    } else {
        let toc_html = selector::extract_html(&doc, &r.list).ok().flatten().unwrap_or_default();
        let inner = scraper::Html::parse_document(&toc_html);
        to_items(selector::select_all(&inner, &r.item).unwrap_or_default(), base_uri)
    }
}

/// 按章节名去重：同标题后者替换前者（对应源项目 `TocList`，解决目录乱序重复）。
fn dedup_by_title(items: Vec<RawItem>) -> Vec<RawItem> {
    let mut out: Vec<RawItem> = Vec::with_capacity(items.len());
    let mut pos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for item in items {
        if let Some(&p) = pos.get(&item.title) {
            out.remove(p);
            for v in pos.values_mut() {
                if *v > p {
                    *v -= 1;
                }
            }
        }
        pos.insert(item.title.clone(), out.len());
        out.push(item);
    }
    out
}

/// 是否为合法 http(s) URL（对应源项目 `Validator.isUrl` 的宽松判定）。
fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_book_id_takes_first_capture_group() {
        assert_eq!(extract_book_id(r"/book/(\d+)", "https://x.cc/book/1234/"), Some("1234".to_owned()));
        // @js: 后缀剥离
        assert_eq!(
            extract_book_id(r"/book/(\d+)@js:return 'id'", "https://x.cc/book/99"),
            Some("99".to_owned())
        );
        assert_eq!(extract_book_id("", "https://x.cc/book/1"), None);
        // 无捕获组
        assert_eq!(extract_book_id(r"/book/\d+", "https://x.cc/book/1"), None);
    }

    #[test]
    fn dedup_by_title_keeps_last_and_order() {
        let items = vec![
            RawItem { title: "第1章".into(), url: "a".into() },
            RawItem { title: "第2章".into(), url: "b".into() },
            RawItem { title: "第1章".into(), url: "a2".into() },
        ];
        let out = dedup_by_title(items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], RawItem { title: "第2章".into(), url: "b".into() });
        assert_eq!(out[1], RawItem { title: "第1章".into(), url: "a2".into() });
    }

    #[test]
    fn extract_items_direct_selector_absolutizes_href() {
        let r = TocRule { item: "li a".into(), ..Default::default() };
        let items = extract_items(
            r#"<html><body><ul><li><a href="/c1.html">第1章 起点</a></li><li><a href="/c2.html">第2章</a></li></ul></body></html>"#,
            &r,
            "https://x.cc/book/",
        );
        assert_eq!(items[0].title, "第1章 起点");
        assert_eq!(items[0].url, "https://x.cc/c1.html");
    }

    #[test]
    fn extract_items_list_container_absolutizes_with_outer_base() {
        let r = TocRule { list: "#list".into(), item: "a".into(), ..Default::default() };
        let items = extract_items(
            r#"<html><body><div id="list"><a href="/rel.html">第1章</a><a href="https://x.cc/abs.html">第2章</a></div></body></html>"#,
            &r,
            "https://x.cc/book/",
        );
        // 内层文档继承外层 base_uri：相对链接按外层补全（wxsy 目录即此形态），绝对链接保留
        assert_eq!(items[0].url, "https://x.cc/rel.html");
        assert_eq!(items[1].url, "https://x.cc/abs.html");
    }

    #[test]
    fn dropdown_urls_collects_when_value_attr_present() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><select class="pagelist">
                <option value="https://x.cc/toc/1.html">1</option>
                <option value="https://x.cc/toc/2.html" selected>2</option>
            </select></body></html>"#,
        );
        let list = dropdown_urls(&doc, ".pagelist option", "https://x.cc/").expect("应识别为下拉菜单");
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], "https://x.cc/toc/2.html");
    }

    #[test]
    fn next_page_url_prefers_href() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><a id="next" href="/toc/2.html">下一页</a></body></html>"#,
        );
        assert_eq!(
            next_page_url(&doc, "#next", "https://x.cc/toc/1.html"),
            Some("https://x.cc/toc/2.html".to_owned())
        );
    }
}
