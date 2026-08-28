//! 选择器执行：解析规则中的 selector 表达式并从 HTML 提取值。
//!
//! 语义与源项目 `HtmlExtractor` 一致：
//! - `sel` → 元素文本；
//! - `sel@href` / `sel@src` / `sel@content` → 属性值（`absUrl` 补全为绝对链接）；
//! - `sel@js:<code>[;@java:<code>]` → 先取文本/属性，再依次执行 DSL 步骤
//!   （`QuickJS` / 内建 Java 操作，见 [`dsl`] 模块）。
//!
//! XPath：源项目经 `selectXpath` 支持；scraper 无 XPath，对 main.json 中出现的
//! 根路径（`/html`、`/html/body`）做等价映射，其余 `XPath` 报 `InvalidCss`。

use scraper::{Html, Selector as CssSelector};

use super::dsl::{self, DslStep};

/// 选择器执行错误
#[derive(Debug, thiserror::Error)]
pub enum SelectorError {
    /// CSS 选择器语法非法
    #[error("CSS 选择器解析失败: {0}")]
    InvalidCss(String),
    /// DSL（`@js:`/`@java:`）后置处理失败
    #[error("DSL 后置处理失败: {0}")]
    Dsl(String),
}

/// 解析选择器表达式：`(css, Option<attr>, DSL 步骤列表)`。
///
/// `#content` → `("#content", None, [])`；
/// `#content@href` → `("#content", Some("href"), [])`；
/// `meta[property="og:image"]@js:r='http://x'+r` → `(meta..., None, [Js(...)])`。
pub(crate) fn parse_selector(expr: &str) -> (String, Option<String>, Vec<DslStep>) {
    let expr = expr.trim();
    // DSL 步骤先剥离（@js:/@java: 标记切分），选择器与属性后缀留在头部
    let (head, steps) = dsl::split_dsl(expr);
    // 从头部尾部找最后一个 @attr（CSS 属性选择器不含裸 @，安全）
    let head = head.trim();
    if let Some(pos) = head.rfind('@') {
        let attr = &head[pos + 1..];
        // 仅接受已知/单词字符属性名
        if !attr.is_empty() && attr.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return (head[..pos].trim().to_owned(), Some(attr.to_owned()), steps);
        }
    }
    (head.to_owned(), None, steps)
}

/// 从 HTML 文档提取：按选择器取首个匹配的文本或属性，再执行 DSL 步骤。
///
/// 属性语义与源项目 `HtmlExtractor.getContentType` 一致：
/// - `@href`/`@src` → 绝对化链接（`absUrl`）；
/// - 其他属性（`@content`/`@value` 等）→ 原值；
/// - 选择器以 `meta[` 开头时自动推断为 `@content`（规则文件中 meta 字段常不带后缀）。
///
/// # Errors
/// 选择器非法时返回 [`SelectorError::InvalidCss`]；DSL 执行失败返回 [`SelectorError::Dsl`]。
pub fn extract(html: &Html, expr: &str, base_uri: &str) -> Result<Option<String>, SelectorError> {
    let (css, attr, steps) = parse_selector(expr);
    let attr = effective_attr(&css, attr);
    let Some(element) = select_first(html, &css)? else {
        return Ok(None);
    };
    let Some(raw) = extract_element(element, attr.as_deref(), base_uri) else {
        return Ok(None);
    };
    Ok(Some(postprocess(&steps, raw)?))
}

/// 提取首个匹配元素的内部 HTML（对应源项目 `ContentType.HTML`：`el.html()`），
/// 再执行 DSL 步骤。
///
/// 用于目录容器（`toc.list`）与章节正文（`chapter.content`）提取。
///
/// # Errors
/// 选择器非法时返回 [`SelectorError::InvalidCss`]；DSL 执行失败返回 [`SelectorError::Dsl`]。
pub fn extract_html(html: &Html, expr: &str) -> Result<Option<String>, SelectorError> {
    let (css, _attr, steps) = parse_selector(expr);
    let Some(element) = select_first(html, &css)? else {
        return Ok(None);
    };
    let raw = element.inner_html();
    Ok(Some(postprocess(&steps, raw)?))
}

/// DSL 步骤执行（无步骤时原样返回）
fn postprocess(steps: &[DslStep], raw: String) -> Result<String, SelectorError> {
    if steps.is_empty() {
        return Ok(raw);
    }
    dsl::run_steps(steps, &raw).map_err(SelectorError::Dsl)
}

/// 推断生效属性：显式 `@attr` 优先；`meta[` 前缀选择器缺省取 `content`。
fn effective_attr(css: &str, attr: Option<String>) -> Option<String> {
    attr.or_else(|| css.starts_with("meta[").then(|| "content".to_owned()))
}

/// 首个匹配元素（CSS 或已知 `XPath` 根路径）。
///
/// # Errors
/// 选择器非法或不支持的 `XPath` 返回 [`SelectorError::InvalidCss`]。
fn select_first<'a>(html: &'a Html, css: &str) -> Result<Option<scraper::ElementRef<'a>>, SelectorError> {
    Ok(select_all(html, css)?.into_iter().next())
}

/// 元素级完整表达式提取：在给定元素内按 `expr`（可含 `@attr`）匹配首个子元素，
/// 再执行 DSL 步骤。搜索结果行字段提取用（对应源项目
/// `HtmlExtractor.extract(el, r.getBookName(), ...)`）。
///
/// # Errors
/// 同 [`extract`]。
pub fn extract_in_element(
    element: scraper::ElementRef<'_>,
    expr: &str,
    base_uri: &str,
) -> Result<Option<String>, SelectorError> {
    let (css, attr, steps) = parse_selector(expr);
    let attr = effective_attr(&css, attr);
    let selector = parse_css(&css)?;
    let Some(matched) = element.select(&selector).next() else {
        return Ok(None);
    };
    let Some(raw) = extract_element(matched, attr.as_deref(), base_uri) else {
        return Ok(None);
    };
    Ok(Some(postprocess(&steps, raw)?))
}

/// 元素级提取（搜索结果列表中每个条目的字段提取，对应源项目 `HtmlExtractor.extract(el, ...)`）。
/// 属性语义：`@href`/`@src` 绝对化（`absUrl`），其余属性原值。
fn extract_element(element: scraper::ElementRef<'_>, attr: Option<&str>, base_uri: &str) -> Option<String> {
    match attr {
        None => Some(element.text().collect::<String>().trim().to_owned()),
        Some(name) => element.value().attr(name).map(|v| match name {
            // 与 Jsoup absUrl 行为一致：href/src 补全为绝对链接
            "href" | "src" => absolutize(v, base_uri),
            // 切勿 absolutize（源项目 ATTR_CONTENT/ATTR_VALUE 用 attr）
            _ => v.to_owned(),
        }),
    }
}

/// 元素级文本提取（忽略 `@attr` 后缀；`meta[` 前缀仍推断 `content`）。
/// 搜索结果 bookName 用（源项目 `HtmlExtractor.extract(el, r.getBookName())` 的默认文本语义，
/// 即使规则带 `@href` 后缀也取文本——后缀仅对链接提取有意义）。
///
/// # Errors
/// 同 [`extract`]。
pub fn extract_text_in_element(
    element: scraper::ElementRef<'_>,
    expr: &str,
) -> Result<Option<String>, SelectorError> {
    let (css, _attr, steps) = parse_selector(expr);
    let attr = effective_attr(&css, None);
    let selector = parse_css(&css)?;
    let Some(matched) = element.select(&selector).next() else {
        return Ok(None);
    };
    // 文本/content 路径不做链接补全，base_uri 传空即可
    let Some(raw) = extract_element(matched, attr.as_deref(), "") else {
        return Ok(None);
    };
    Ok(Some(postprocess(&steps, raw)?))
}

/// 元素级 href 提取（bookName 规则选中元素的 href，absUrl 补全）。
/// 搜索结果链接用（源项目 `HtmlExtractor.extract(el, r.getBookName(), ContentType.ATTR_HREF)`）。
///
/// # Errors
/// 选择器非法时返回 [`SelectorError::InvalidCss`]；DSL 执行失败返回 [`SelectorError::Dsl`]。
pub fn extract_href_in_element(
    element: scraper::ElementRef<'_>,
    expr: &str,
    base_uri: &str,
) -> Result<Option<String>, SelectorError> {
    let (css, _attr, steps) = parse_selector(expr);
    let selector = parse_css(&css)?;
    let Some(matched) = element.select(&selector).next() else {
        return Ok(None);
    };
    let raw = matched.value().attr("href").map(|v| absolutize(v, base_uri));
    match raw {
        None => Ok(None),
        Some(v) => Ok(Some(postprocess(&steps, v)?)),
    }
}

/// 选中所有匹配元素（剥离 `@attr`/DSL 后缀，仅用 CSS 部分）。
///
/// 用于搜索结果列表与分页链接（对应源项目 `HtmlExtractor.select`）。
/// `XPath` 根路径（`/html`、`/html/body`）映射为根/正文元素。
///
/// # Errors
/// 选择器非法时返回 [`SelectorError::InvalidCss`]。
pub fn select_all<'a>(html: &'a Html, expr: &str) -> Result<Vec<scraper::ElementRef<'a>>, SelectorError> {
    let (css, _attr, _steps) = parse_selector(expr);
    if let Some(root) = xpath_root(html, &css) {
        return Ok(vec![root]);
    }
    let selector = parse_css(&css)?;
    Ok(html.select(&selector).collect())
}

/// CSS 解析（统一错误包装）
fn parse_css(css: &str) -> Result<CssSelector, SelectorError> {
    CssSelector::parse(css).map_err(|e| SelectorError::InvalidCss(e.to_string()))
}

/// `XPath` 根路径等价映射：`/html` → 根元素，`/html/body` → 正文元素。
/// main.json 中唯一 `XPath` 形态为 `toc.list = "/html@js:..."`。
fn xpath_root<'a>(html: &'a Html, css: &str) -> Option<scraper::ElementRef<'a>> {
    match css {
        "/html" => Some(html.root_element()),
        "/html/body" => {
            let selector = parse_css("body").ok()?;
            html.select(&selector).next()
        }
        _ => None,
    }
}

/// 提取元素集合中所有元素的指定属性（分页链接收集，对应源项目 `e.absUrl("href")`）。
///
/// # Errors
/// 选择器非法时返回 [`SelectorError::InvalidCss`]。
pub fn extract_attrs(
    html: &Html,
    expr: &str,
    attr: &str,
    base_uri: &str,
) -> Result<Vec<String>, SelectorError> {
    Ok(select_all(html, expr)?
        .iter()
        .filter_map(|el| el.value().attr(attr).map(|v| absolutize(v, base_uri)))
        .collect())
}

/// 相对 URL 补全（简化版 url.join：处理以 / 开头与完整 http 链接两种主要形态）。
/// TODO(M2): 换 `url` crate 的 `Url::join` 完整实现（处理 ../ 与无协议 // 情况）。
pub(crate) fn absolutize(v: &str, base_uri: &str) -> String {
    if v.starts_with("http://") || v.starts_with("https://") || base_uri.is_empty() {
        return v.to_owned();
    }
    // 提取 scheme://host[:port]
    let Some(idx) = base_uri.find("://") else { return v.to_owned() };
    let after = &base_uri[idx + 3..];
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let origin = &base_uri[..idx + 3 + host_end];
    if v.starts_with('/') {
        format!("{origin}{v}")
    } else if v.starts_with("//") {
        // 无协议形式 //host/path
        format!("http:{v}")
    } else {
        // 相对于 base 路径（目录级）
        let dir_end = base_uri.rfind('/').map_or(base_uri.len(), |p| p + 1);
        format!("{}/{}", &base_uri[..dir_end.max(idx + 3)], v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Html {
        Html::parse_document(
            r#"<html><body>
                <div id="content">正文文本</div>
                <a id="link" href="/book/123.html">链接</a>
                <meta property="og:image" content="https://cdn.example.com/cover.jpg">
            </body></html>"#,
        )
    }

    #[test]
    fn parse_selector_text_attr_and_js() {
        assert_eq!(parse_selector("#content"), ("#content".into(), None, vec![]));
        assert_eq!(parse_selector("a@href"), ("a".into(), Some("href".into()), vec![]));
        let (css, attr, js) = parse_selector("meta@js:r='http://x'+r");
        assert_eq!(css, "meta");
        assert_eq!(attr, None);
        assert_eq!(js, vec![DslStep::Js("r='http://x'+r".into())]);
    }

    #[test]
    fn extract_text_returns_first_match() {
        let d = doc();
        let v = extract(&d, "#content", "").unwrap();
        assert_eq!(v.as_deref(), Some("正文文本"));
    }

    #[test]
    fn extract_attr_absolutizes_relative_link() {
        let d = doc();
        let v = extract(&d, "#link@href", "http://www.xbiqugu.la/modules/").unwrap();
        assert_eq!(v.as_deref(), Some("http://www.xbiqugu.la/book/123.html"));
    }

    #[test]
    fn extract_missing_element_returns_none() {
        let d = doc();
        let v = extract(&d, "#not-exist", "").unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn extract_js_postprocesses_input() {
        // main.json coverUrl 规则形态：r='http://www.mcxs.la'+r
        let d = doc();
        let v = extract(&d, r#"meta[property="og:image"]@js:r='http://www.mcxs.la'+r"#, "").unwrap();
        assert_eq!(v.as_deref(), Some("http://www.mcxs.lahttps://cdn.example.com/cover.jpg"));
    }

    #[test]
    fn extract_js_syntax_error_returns_dsl_error() {
        let d = doc();
        let err = extract(&d, "#content@js:r=???", "").unwrap_err();
        assert!(matches!(err, SelectorError::Dsl(_)), "{err}");
    }

    #[test]
    fn extract_meta_selector_infers_content_attr() {
        let d = doc();
        let v = extract(&d, r#"meta[property="og:image"]"#, "").unwrap();
        assert_eq!(v.as_deref(), Some("https://cdn.example.com/cover.jpg"));
    }

    #[test]
    fn extract_html_returns_inner_html() {
        let d = doc();
        let v = extract_html(&d, "body").unwrap().expect("body 应存在");
        assert!(v.contains("<div id=\"content\">"), "应含子元素原文: {v}");
    }

    #[test]
    fn extract_html_root_xpath_maps_to_document() {
        // main.json wxsy.net toc.list 规则形态：/html@js:...
        let d = doc();
        let v = extract_html(&d, "/html@js:r=r.replace('正文文本','替换后')").unwrap();
        assert!(v.expect("根元素应存在").contains("替换后"));
    }

    #[test]
    fn extract_html_js_and_java_chained() {
        // main.json chapter.content 规则形态：js + @java:base64.decode()
        let d = Html::parse_document(r#"<html><body><div id="htmlContent">5Lit5paH</div></body></html>"#);
        let v = extract_html(&d, "#htmlContent@js:r=r.trim();@java:base64.decode()").unwrap();
        assert_eq!(v.as_deref(), Some("中文"));
    }

    #[test]
    fn select_all_root_xpath_returns_root_element() {
        let d = doc();
        let els = select_all(&d, "/html").unwrap();
        assert_eq!(els.len(), 1);
        assert_eq!(els[0].value().name(), "html");
    }
}
