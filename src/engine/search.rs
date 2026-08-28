//! 聚合搜索（对应源项目 `SearchParser` + `AggregatedSearchAction` + `SearchResultsHandler`）。
//!
//! 流程：并发查询全部激活书源 → 各源解析搜索结果（含分页）→ 合并 → 低相似度过滤 + 相似度降序。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::has_cf;
use crate::config::AppConfig;
use crate::rule::{selector, Rule};
use crate::util::http::{fetch_page, origin_of, HttpClients, PageRequest};

/// 搜索结果（对应源项目 `SearchResult`，JSON 字段 camelCase 保持前端兼容）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchResult {
    pub source_id: u32,
    pub source_name: String,
    pub url: String,
    pub book_name: String,
    pub author: String,
    pub category: String,
    pub latest_chapter: String,
    pub last_update_time: String,
    pub status: String,
    pub word_count: String,
}

/// 聚合搜索：并发查询全部可搜索书源，合并后过滤排序。
pub async fn aggregated_search(
    config: &AppConfig,
    clients: &HttpClients,
    rules: &[Rule],
    kw: &str,
) -> Vec<SearchResult> {
    let searchable: Vec<&Rule> =
        rules.iter().filter(|r| !r.disabled && !r.search.disabled && !r.search.url.is_empty()).collect();

    let mut set: JoinSet<Vec<SearchResult>> = JoinSet::new();
    for rule in searchable {
        let rule = Arc::new(rule.clone());
        let config = config.clone();
        let client = clients.for_rule(rule.need_proxy).clone();
        let kw = kw.to_owned();
        set.spawn(async move {
            match search_source(&config, &client, &rule, &kw).await {
                Ok(res) => {
                    if !res.is_empty() {
                        tracing::info!(source = %rule.name, count = res.len(), "书源搜索完成");
                    }
                    res
                }
                Err(e) => {
                    tracing::warn!(source = %rule.name, error = %e, "书源 {} 搜索出错", rule.name);
                    Vec::new()
                }
            }
        });
    }

    let mut all = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(res) = joined {
            all.extend(res);
        }
    }
    filter_and_sort(all, kw, config.source.search_filter)
}

/// 单书源搜索（对应源项目 `SearchParser.parse`）。
async fn search_source(
    config: &AppConfig,
    client: &reqwest::Client,
    rule: &Rule,
    kw: &str,
) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
    let r = &rule.search;
    // TODO(M5): search.url 含 @js: 时经 QuickJS 生成实际 URL
    if r.url.contains("@js:") {
        tracing::warn!(source = %rule.name, "搜索 URL 含 @js:（M5 支持），跳过该源");
        return Ok(Vec::new());
    }
    let search_url = r.url.replace("%s", kw);
    let referer = origin_of(&search_url);
    let form = if r.method.eq_ignore_ascii_case("post") { Some(build_form(&r.data, kw)) } else { None };
    let page_req = PageRequest {
        form,
        cookies: (!r.cookies.is_empty()).then_some(r.cookies.as_str()),
        timeout_secs: r.timeout.map_or(15, u64::from),
        referer: referer.as_deref(),
    };

    let mut body = fetch_page(client, &search_url, &page_req).await?;

    // Cloudflare 真人验证检测（对应源项目 CrawlUtils.hasCf + cf-bypass 绕过）。
    // 注意：scraper::Html 非 Send，仅允许在同步块作用域内存在，绝不跨 await。
    let needs_bypass = {
        let document = scraper::Html::parse_document(&body);
        has_cf(&document)
    };
    if needs_bypass {
        if config.global.cf_bypass.is_empty() {
            return Err(
                format!("搜索页 {search_url} 存在 Cloudflare 真人验证，且未配置 cf-bypass，跳过").into()
            );
        }
        let bypass_url = format!("{}/html?url={}", config.global.cf_bypass, search_url);
        body =
            fetch_page(client, &bypass_url, &PageRequest { timeout_secs: 30, ..Default::default() }).await?;
    }

    // 首页解析 + 分页 URL 收集（同步块，Html 不逃逸）
    let first_page: Vec<SearchResult>;
    let page_urls: Vec<String> = {
        let document = scraper::Html::parse_document(&body);
        first_page = parse_results(config, rule, &document);
        if r.next_page.is_empty() {
            Vec::new()
        } else {
            let mut seen = std::collections::HashSet::new();
            selector::extract_attrs(&document, &r.next_page, "href", &r.base_uri)
                .unwrap_or_default()
                .into_iter()
                .filter(|u| seen.insert(u.clone()))
                .collect()
        }
    };

    // 并行抓取分页正文（仅 String，无 Html 跨 await）
    let timeout = r.timeout.map_or(15, u64::from);
    let mut fetches: JoinSet<Result<String, crate::util::http::HttpError>> = JoinSet::new();
    for url in &page_urls {
        let client = client.clone();
        let referer = origin_of(url);
        let url = url.clone();
        fetches.spawn(async move {
            fetch_page(
                &client,
                &url,
                &PageRequest { timeout_secs: timeout, referer: referer.as_deref(), ..Default::default() },
            )
            .await
        });
    }
    let mut page_bodies = Vec::new();
    while let Some(joined) = fetches.join_next().await {
        match joined {
            Ok(Ok(b)) => page_bodies.push(b),
            Ok(Err(e)) => tracing::warn!(source = %rule.name, error = %e, "分页抓取失败"),
            Err(e) => tracing::warn!(source = %rule.name, error = %e, "分页任务 join 失败"),
        }
    }

    // 同步收尾：全部 body 到齐后统一解析（对应源项目 parallelStream 后合并）
    let mut results = first_page;
    for b in page_bodies {
        let doc = scraper::Html::parse_document(&b);
        results.extend(parse_results(config, rule, &doc));
    }
    // 全局条数上限（0 视为不限，对应源项目 searchLimit 语义）
    let limit = config.source.search_limit;
    if limit != 0 && limit != u32::MAX {
        results.truncate(limit as usize);
    }
    Ok(results)
}

/// 解析搜索结果页（对应源项目 `SearchParser.getSearchResults`）。
fn parse_results(config: &AppConfig, rule: &Rule, document: &scraper::Html) -> Vec<SearchResult> {
    let r = &rule.search;
    let Ok(elements) = selector::select_all(document, &r.result) else {
        return Vec::new();
    };
    // 只取前 N 条且有书名的记录（对应源项目 filter + limit）
    let limit = config.source.search_limit;
    let limit = if limit == 0 || limit == u32::MAX { usize::MAX } else { limit as usize };
    let mut list = Vec::new();
    for el in elements {
        // bookName 文本语义（源项目 HtmlExtractor.extract(el, r.getBookName())，
        // 即使规则带 @href 后缀也取文本）；url 取同一元素的 href（ATTR_HREF 语义）
        let book_name =
            selector::extract_text_in_element(el, &r.book_name).ok().flatten().unwrap_or_default();
        if book_name.is_empty() {
            continue;
        }
        if list.len() >= limit {
            break;
        }
        let field = |expr: &str| -> String {
            selector::extract_in_element(el, expr, &r.base_uri).ok().flatten().unwrap_or_default()
        };
        let url = selector::extract_href_in_element(el, &r.book_name, &r.base_uri)
            .ok()
            .flatten()
            .unwrap_or_default();
        list.push(SearchResult {
            source_id: rule.id,
            source_name: rule.name.clone(),
            url,
            book_name,
            author: field(&r.author),
            category: field(&r.category),
            latest_chapter: field(&r.latest_chapter),
            last_update_time: field(&r.last_update_time),
            status: field(&r.status),
            word_count: field(&r.word_count),
        });
    }
    list
}

/// 解析 `{searchkey: %s, ...}` 宽松 JSON 为 form 键值对（对应源项目 `CrawlUtils.buildData`，
/// hutool 宽松 JSON 的 key 无引号，故手写解析而非 serde）。
fn build_form(data: &str, kw: &str) -> Vec<(String, String)> {
    let trimmed = data.trim().trim_start_matches('{').trim_end_matches('}');
    let mut form = Vec::new();
    for pair in trimmed.split(',') {
        let Some((k, v)) = pair.split_once(':') else { continue };
        let key = k.trim().trim_matches('"').trim_matches('\'');
        let value = v.trim().trim_matches('"').trim_matches('\'');
        form.push((key.to_owned(), value.replace("%s", kw)));
    }
    form
}

// Cloudflare 检测复用 engine::has_cf（见 mod.rs）

/// 过滤低相似度结果并按相似度降序（完整移植源项目 `SearchResultsHandler.filterAndSort`）。
pub fn filter_and_sort(list: Vec<SearchResult>, kw: &str, search_filter: bool) -> Vec<SearchResult> {
    if list.is_empty() {
        return list;
    }

    // 预计算书名/作者相似度
    let book_sim: Vec<f64> = list.iter().map(|sr| similar(kw, &sr.book_name)).collect();
    let author_sim: Vec<f64> = list.iter().map(|sr| similar(kw, &sr.author)).collect();

    // 判断关键字是书名还是作者（权重求和比较）
    let is_author_search = compute_weight(&book_sim, kw) < compute_weight(&author_sim, kw);

    let sims: Vec<f64> = if is_author_search { author_sim } else { book_sim };

    // 索引按（相似度降序，次级键升序）排序
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by(|&a, &b| {
        sims[b].partial_cmp(&sims[a]).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
            if is_author_search {
                list[a].book_name.cmp(&list[b].book_name)
            } else {
                list[a].author.cmp(&list[b].author)
            }
        })
    });

    let take = |min_sim: f64| -> Vec<SearchResult> {
        order.iter().filter(|&&i| sims[i] > min_sim).map(|&i| list[i].clone()).collect()
    };

    if search_filter {
        let filtered = take(0.25);
        // 过滤后为空则退化为仅排序（保留相似度 > 0 的结果）
        if filtered.is_empty() {
            take(0.0)
        } else {
            filtered
        }
    } else {
        take(0.0)
    }
}

/// 权重计算（对应源项目 `SearchResultsHandler.computeWeight/weight`，短查询 ≤4 / 长查询 ≥10 分档）。
fn compute_weight(sims: &[f64], kw: &str) -> f64 {
    let is_short = kw.chars().count() <= 4;
    let is_long = kw.chars().count() >= 10;
    sims.iter().map(|&s| weight(s, is_short, is_long)).sum()
}

/// 分档权重（与源项目数值完全一致）。
fn weight(s: f64, is_short: bool, is_long: bool) -> f64 {
    if is_short {
        if (s - 1.0).abs() < f64::EPSILON {
            return 12.0;
        }
        if s >= 0.8 {
            return s * s * s * 8.0;
        }
        if s >= 0.7 {
            return s * 5.0;
        }
        0.0
    } else if is_long {
        if (s - 1.0).abs() < f64::EPSILON {
            return 10.0;
        }
        if s >= 0.85 {
            return s * s * s * 8.0;
        }
        if s >= 0.7 {
            return s * s * 5.0;
        }
        if s >= 0.5 {
            return s * 3.0;
        }
        s * 1.2
    } else {
        if (s - 1.0).abs() < f64::EPSILON {
            return 10.0;
        }
        if s >= 0.85 {
            return s * s * s * 8.0;
        }
        if s >= 0.7 {
            return s * s * 5.0;
        }
        if s >= 0.5 {
            return s * 3.0;
        }
        0.0
    }
}

/// 字符串相似度（对应 hutool `StrUtil.similar`：1 - Levenshtein 距离 / 较长串长度）。
pub fn similar(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let dist = levenshtein(&a, &b);
    1.0 - f64::from(dist) / f64::from(u32::try_from(longest).unwrap_or(u32::MAX))
}

/// Levenshtein 编辑距离（滚动数组，长度以 char 计，usize 计算后收敛为 u32）。
fn levenshtein(a: &[char], b: &[char]) -> u32 {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    u32::try_from(prev[b.len()]).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_identical_and_disjoint() {
        assert!((similar("斗破苍穹", "斗破苍穹") - 1.0).abs() < 1e-9);
        assert!(similar("斗破苍穹", "完全无关") < 0.1);
    }

    #[test]
    fn similar_empty_pair_is_one() {
        assert!((similar("", "") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn build_form_replaces_placeholder_and_keeps_literals() {
        let form = build_form("{searchkey: %s, searchtype: all}", "斗罗");
        assert_eq!(form[0], ("searchkey".into(), "斗罗".into()));
        assert_eq!(form[1], ("searchtype".into(), "all".into()));
    }

    #[test]
    fn filter_and_sort_removes_low_similarity_and_sorts_desc() {
        let mk = |name: &str| SearchResult { book_name: name.into(), ..Default::default() };
        let list = vec![mk("完全无关的书"), mk("斗破苍穹"), mk("斗破苍穹之无上之境")];
        let out = filter_and_sort(list, "斗破苍穹", true);
        // 低相似度剔除
        assert!(out.iter().all(|r| similar("斗破苍穹", &r.book_name) > 0.25));
        // 精确匹配置顶
        assert_eq!(out[0].book_name, "斗破苍穹");
    }

    #[test]
    fn filter_and_sort_fallback_when_all_filtered() {
        let mk = |name: &str| SearchResult { book_name: name.into(), ..Default::default() };
        // sim("abcdefgh", "ab") = 2/8 = 0.25，不满足 > 0.25 阈值但 > 0
        let list = vec![mk("ab"), mk("ac")];
        let out = filter_and_sort(list, "abcdefgh", true);
        // 全部低于阈值时退化为相似度 > 0（保留全部；与源项目 sim > 0 语义一致）
        assert_eq!(out.len(), 2);
        // 相似度全为 0 时源项目同样返回空列表
        let list = vec![mk("甲"), mk("乙")];
        let out = filter_and_sort(list, "完全不同关键词", true);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_results_extracts_fields_from_fixture() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><table>
                <tr><td><a href="/book/1.html">斗破苍穹</a></td><td>天蚕土豆</td><td>玄幻</td></tr>
                <tr><td><a href="/book/2.html">斗罗大陆</a></td><td>唐家三少</td></tr>
                <tr><td><a>无链接的书</a></td></tr>
            </table></body></html>"#,
        );
        let mut rule = Rule { id: 1, name: "测试源".into(), ..Default::default() };
        rule.search.result = "tr".into();
        rule.search.book_name = "td:nth-child(1) a@href".into();
        rule.search.author = "td:nth-child(2)".into();
        rule.search.category = "td:nth-child(3)".into();

        let out = parse_results(&AppConfig::default(), &rule, &doc);
        // 有书名文本即入选（源项目仅以 bookName 判空，与有无链接无关）
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].book_name, "斗破苍穹");
        assert_eq!(out[0].url, "/book/1.html");
        assert_eq!(out[0].author, "天蚕土豆");
        assert_eq!(out[0].category, "玄幻");
        assert_eq!(out[1].book_name, "斗罗大陆");
        assert_eq!(out[1].category, "");
        // 第三行有书名但无 href 属性 → url 为空
        assert_eq!(out[2].book_name, "无链接的书");
        assert_eq!(out[2].url, "");
    }
}
