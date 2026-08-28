//! 正文净化与渲染（对应源项目 `ChapterFilter` + `ChapterFormatter` + `ChapterRenderer`）。
//!
//! 流程：净化（不可见字符/HTML 实体/广告/重复标题/空标签）→ 排版（属性清除 +
//! 段落标签归一为 `<p>`）→ 按输出格式渲染（txt 纯文本 / epub XHTML 模板）。
//!
//! 章节级正则编译一次复用（常量用 `OnceLock`，规则正则按章编译开销可忽略）；
//! DOM 操作均为同步小片段，不跨 await。

use std::sync::OnceLock;

use regex::Regex;
use scraper::{Html, Node, Selector as CssSelector};

use crate::rule::ChapterRule;

/// 渲染产物（净化可能规范化标题，如 `1.章节名` → `第1章 章节名`）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rendered {
    pub title: String,
    pub content: String,
}

/// 处理单个章节：净化 → 排版 → 按格式渲染（对应 `ChapterRenderer.process`）。
///
/// `ext`：`txt` 纯文本；`epub` XHTML（章节缓存 `.html`，M4 合并用）。
pub(crate) fn render_chapter(r: &ChapterRule, ext: &str, title: &str, raw_content: &str) -> Rendered {
    let (title, content) = filter(r, title, raw_content);
    let content = format_paragraphs(r, &content);
    let content = match ext {
        "txt" => render_txt(&title, &content),
        "epub" => render_epub_html(&title, &content),
        _ => content,
    };
    Rendered { title, content }
}

// ==================== 净化（ChapterFilter） ====================

/// 净化正文并规范化标题（对应源项目 `ChapterFilter.filter` 全步骤）。
fn filter(r: &ChapterRule, title: &str, content: &str) -> (String, String) {
    let mut content = clean_invisible_chars(content);

    // 替换 &..;（HTML 字符实体引用，如 &nbsp;），可能会导致 ibooks 章节报错
    let entity = html_entity_pattern();
    content = entity.replace_all(&content, "").into_owned();

    // 广告过滤：filterTxt 正则替换 + filterTag 标签删除
    if !r.filter_txt.is_empty() {
        match Regex::new(&r.filter_txt) {
            Ok(re) => content = re.replace_all(&content, "").into_owned(),
            Err(e) => tracing::warn!(error = %e, "filterTxt 正则非法，跳过文字过滤"),
        }
    }
    content = remove_tags(&content, &r.filter_tag);

    // 删除正文开头的重复标题（regex::escape 等价 Pattern.quote：章节名按纯文本处理）
    let title_clean: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    if !title_clean.is_empty() {
        let pattern = format!(r"^(\s|<[^>]+>)*({}|{})", regex::escape(title), regex::escape(&title_clean));
        if let Ok(re) = Regex::new(&pattern) {
            content = re.replace(&content, "$1").into_owned();
        }
    }
    // 标题规范化：`1.章节名` → `第1章 章节名`（解决部分阅读器目录无法解析）
    let title = match title_number_pattern().captures(title) {
        Some(c) => format!("第{}章 {}", &c[1], &c[2]),
        None => title.to_owned(),
    };

    // 删除全部空 tag（如 <p></p>），置于最后
    content = clean_empty_tags(&content);
    (title, content)
}

/// 不可见字符清理（控制/格式/私有区/行段落分隔符 + U+200B/U+FEFF，
/// 对应源项目 `CrawlUtils.cleanInvisibleChars` 的 `[\p{C}\p{Cf}\p{Co}\p{Zl}\p{Zp}\u200B\uFEFF]`）。
/// Rust regex 不支持 `\p{Co}` 等两字母类别：`\p{C}`（Other，含 Cc/Cf/Co/Cs/Cn）+
/// Zl/U+2028、Zp/U+2029 以显式码位表达，语义等价。
fn clean_invisible_chars(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"[\p{C}\x{2028}\x{2029}\x{200B}\x{FEFF}]").expect("常量正则恒合法"));
    re.replace_all(text, "").into_owned()
}

fn html_entity_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"&[^;]+;").expect("常量正则恒合法"))
}

fn title_number_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s*\.\s*(.+)$").expect("常量正则恒合法"))
}

/// 删除配对空标签（可能含属性与空白，对应 hutool `HtmlUtil.cleanEmptyTag`；
/// Rust regex 无反向引用，以捕获组相等判定替代 `</\1>`）。
fn clean_empty_tags(html: &str) -> String {
    let re = Regex::new(r"(?i)<([a-z\d]+)([^<>]*)>\s*</([a-z\d]+)>").expect("常量正则恒合法");
    re.replace_all(html, |caps: &regex::Captures<'_>| {
        if caps[1].eq_ignore_ascii_case(&caps[3]) {
            String::new()
        } else {
            caps[0].to_owned()
        }
    })
    .into_owned()
}

// ==================== 排版（ChapterFormatter） ====================

/// 排版格式化（对应 `ChapterFormatter.format`）：清除全部属性与空白 → 段落归一 `<p>`。
fn format_paragraphs(r: &ChapterRule, content: &str) -> String {
    let content = clear_all_attributes(content);
    if r.paragraph_tag_closed {
        // 标签闭合（如 <span>段落</span>）：非 <p> 闭合标签替换为 <p>
        rewrite_non_p_to_p(&content)
    } else if r.paragraph_tag.is_empty() {
        // 防御：未配置分段标签时整体作为一段（源项目此分支依赖规则必填）
        format!("<p>{content}</p>")
    } else {
        // 标签不闭合（如 段落1<br><br>段落2）：按段落标签正则切分包裹
        let re = match Regex::new(&r.paragraph_tag) {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!(error = %e, "paragraphTag 正则非法，退化为单段");
                return format!("<p>{content}</p>");
            }
        };
        let mut out = String::new();
        for line in re.split(&content) {
            if !line.is_empty() {
                out.push_str("<p>");
                out.push_str(line);
                out.push_str("</p>");
            }
        }
        out
    }
}

/// 清除所有元素属性并去全部空白（对应 `HtmlUtils.clearAllAttributes` +
/// `StrUtil.cleanBlank`；正文不能提前 cleanBlank，否则 `<divclass=..>` 无法解析）。
fn clear_all_attributes(html: &str) -> String {
    let mut doc = Html::parse_document(html);
    let ids: Vec<_> =
        doc.tree.nodes().filter(|n| matches!(n.value(), Node::Element(_))).map(|n| n.id()).collect();
    for id in ids {
        if let Some(mut node) = doc.tree.get_mut(id) {
            if let Node::Element(el) = node.value() {
                el.attrs.clear();
            }
        }
    }
    let body = body_inner_html(&doc);
    body.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// 删除 CSS 选择器匹配的标签（逗号分隔多选择器，对应 `HtmlUtils.removeTags`）。
///
/// 规则文件中的属性选择器常为无引号形态（如 `p[style=font-size:12px;]`，jsoup 容忍），
/// cssparser 严格要求引号：解析前为无引号属性值补全引号。
fn remove_tags(html: &str, css_query: &str) -> String {
    if html.is_empty() || css_query.trim().is_empty() {
        return html.to_owned();
    }
    let normalized = quote_unquoted_attr_values(css_query);
    let Ok(selector) = CssSelector::parse(&normalized) else {
        tracing::warn!(css_query, "filterTag 选择器非法，跳过标签删除");
        return html.to_owned();
    };
    let mut doc = Html::parse_document(html);
    let ids: Vec<_> = doc.select(&selector).map(|el| el.id()).collect();
    // 倒序 detach：父节点先删后，子节点 detach 为无害空操作
    for id in ids.iter().rev() {
        if let Some(mut node) = doc.tree.get_mut(*id) {
            node.detach();
        }
    }
    body_inner_html(&doc)
}

/// 为 CSS 属性选择器的无引号值补全引号：`p[style=font-size:12px;]` → `p[style="font-size:12px;"]`。
/// 已带引号的值不匹配（值字符类排除引号），保持原样。
fn quote_unquoted_attr_values(css_query: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"\[([-\w]+)=([^\]\["]+)\]"#).expect("常量正则恒合法"));
    re.replace_all(css_query, r#"[$1="$2"]"#).into_owned()
}

/// 非 `<p>` 闭合标签改名为 `<p>`（对应 `<(?!p\b)([^>]+)>(.*?)</\1>` → `<p>$2</p>`）。
///
/// Rust regex 无反向引用，改为 DOM 改名：仅改名「祖先链（html/body 除外）全为 p」
/// 的非 p 元素——与原正则「外层匹配吞掉内层」的语义一致。
fn rewrite_non_p_to_p(html: &str) -> String {
    let mut doc = Html::parse_document(html);
    // 祖先链（跳过 html/body/文档根）全为 p 的非 p 元素才改名（类型经闭包推断，无需命名）。
    // 注意：html/body/head 自身必须排除——源正则只作用于正文片段，文档骨架标签不在其列；
    // 否则骨架被改名为 p 后 select("body") 落空，输出为空串。
    let is_content_element = |name: &str| name != "p" && name != "html" && name != "body" && name != "head";
    let targets: Vec<_> = doc
        .tree
        .nodes()
        .filter(|n| matches!(n.value(), Node::Element(el) if is_content_element(el.name())))
        .filter(|n| {
            let mut cur = n.parent();
            while let Some(p) = cur {
                if let Node::Element(el) = p.value() {
                    let name = el.name();
                    if name != "p" && name != "html" && name != "body" {
                        return false;
                    }
                }
                cur = p.parent();
            }
            true
        })
        .map(|n| n.id())
        .collect();
    // p 名克隆模板（scraper 未重导出 html5ever 类型，无法直接构造 QualName；
    // 注意 root_element() 是 <html>，须 select 到模板中的 <p> 元素取 name）
    let p_name = Html::parse_document("<p></p>")
        .select(&CssSelector::parse("p").expect("p 选择器恒合法"))
        .next()
        .expect("模板文档恒含 p 元素")
        .value()
        .name
        .clone();
    for id in &targets {
        if let Some(mut node) = doc.tree.get_mut(*id) {
            if let Node::Element(el) = node.value() {
                el.name = p_name.clone();
            }
        }
    }
    body_inner_html(&doc)
}

/// 文档 body 内部 HTML（文档解析恒生成 body）
fn body_inner_html(doc: &Html) -> String {
    let selector = CssSelector::parse("body").expect("body 选择器恒合法");
    doc.select(&selector).next().map_or_else(String::new, |el| el.inner_html())
}

// ==================== 渲染（ChapterRenderer） ====================

/// txt 渲染：`<p>(.*?)</p>` 逐段全角缩进两字符换行，首部标题（对应 `renderTxtFormat`）。
fn render_txt(title: &str, html_content: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?s)<p>(.*?)</p>").expect("常量正则恒合法"));
    let indent = "\u{3000}\u{3000}";
    let mut sb = String::new();
    for caps in re.captures_iter(html_content) {
        sb.push_str(indent);
        sb.push_str(&caps[1]);
        sb.push('\n');
    }
    format!("{title}\n\n{sb}")
}

/// epub 章节渲染（对应源项目 `templates/chapter_epub.flt` 模板）。
fn render_epub_html(title: &str, content: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="zh">
<head>
  <meta http-equiv="Content-Type" content="application/xhtml+xml; charset=utf-8" />
  <title>{title}</title>
  <style type="text/css">
    p {{
      text-indent: 2em;
      letter-spacing: 1px;
    }}
  </style>
</head>
<body>
  <h2>{title}</h2>
  <div>
    {content}
  </div>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_invisible_chars_removes_control_and_bom() {
        assert_eq!(clean_invisible_chars("a\u{200B}b\u{FEFF}c"), "abc");
        assert_eq!(clean_invisible_chars("正常中文"), "正常中文");
    }

    #[test]
    fn filter_removes_entities_ads_and_normalizes_title() {
        let r = ChapterRule {
            filter_txt: "本章未完|点击下一页".to_owned(),
            filter_tag: "div.ad".to_owned(),
            ..Default::default()
        };
        let (title, content) =
            filter(&r, "1.开篇", r#"1.开篇<div class="ad">广告</div><p>&nbsp;正文点击下一页本章未完</p>"#);
        assert_eq!(title, "第1章 开篇");
        assert!(!content.contains("广告"), "filterTag 应删除广告节点: {content}");
        assert!(!content.contains("&nbsp;"));
        assert!(!content.contains("点击下一页"));
        assert!(!content.contains("1.开篇"), "正文开头重复标题应删除: {content}");
    }

    #[test]
    fn clean_empty_tags_removes_paired_empty() {
        assert_eq!(clean_empty_tags("<p></p><p>有字</p>"), "<p>有字</p>");
        assert_eq!(clean_empty_tags("<div><p></p></div>"), "<div></div>");
    }

    #[test]
    fn clear_all_attributes_strips_attrs_and_whitespace() {
        let out = clear_all_attributes(r#"<div class="x"> <p id="p1">文 字</p></div>"#);
        assert_eq!(out, r"<div><p>文字</p></div>");
    }

    #[test]
    fn rewrite_non_p_to_p_keeps_nested_inner_tag() {
        // <div><span>x</span></div> → 仅外层改名（正则 (.*?)</\1> 吞掉内层）
        let out = rewrite_non_p_to_p("<div><span>x</span></div>");
        assert_eq!(out, "<p><span>x</span></p>");
        // 顶层并列的多个非 p 标签都改名
        let out = rewrite_non_p_to_p("<span>a</span><em>b</em>");
        assert_eq!(out, "<p>a</p><p>b</p>");
        // p 内的 span 也改名（祖先为 p）
        let out = rewrite_non_p_to_p("<p><span>x</span></p>");
        assert_eq!(out, "<p><p>x</p></p>");
    }

    #[test]
    fn format_paragraphs_closed_and_unclosed() {
        let closed = ChapterRule { paragraph_tag_closed: true, ..Default::default() };
        assert_eq!(format_paragraphs(&closed, "<span>段落</span>"), "<p>段落</p>");

        let unclosed = ChapterRule { paragraph_tag: r"<br>+".to_owned(), ..Default::default() };
        assert_eq!(format_paragraphs(&unclosed, "段落一<br><br>段落二"), "<p>段落一</p><p>段落二</p>");
    }

    #[test]
    fn render_txt_indents_paragraphs() {
        let out = render_txt("第1章", "<p>第一段</p><p>第二段</p>");
        assert!(out.starts_with("第1章\n\n"));
        assert!(out.contains("\u{3000}\u{3000}第一段\n"));
    }

    #[test]
    fn remove_tags_quotes_unquoted_attr_values() {
        // 无引号属性值（jsoup 容忍形态）应补引号后正常解析删除，不再告警跳过
        let out = remove_tags(
            r#"<p style="font-size:12px;">广告</p><p>正文</p>"#,
            "h3, div, p[style=font-size:12px;]",
        );
        assert_eq!(out, "<p>正文</p>");
        // 已带引号的写法不受影响
        let out = remove_tags(r#"<p style="a">x</p>"#, r#"p[style="a"]"#);
        assert_eq!(out, "");
    }

    #[test]
    fn render_epub_html_wraps_template() {
        let out = render_epub_html("标题", "<p>正文</p>");
        assert!(out.contains("<h2>标题</h2>"));
        assert!(out.contains("application/xhtml+xml"));
    }

    #[test]
    fn render_chapter_full_pipeline_txt() {
        let r = ChapterRule {
            content: "#content".to_owned(),
            paragraph_tag: r"<br>+".to_owned(),
            ..Default::default()
        };
        let out = render_chapter(&r, "txt", "第1章 试炼", "第1章 试炼<br>第一行<br><br>第二行");
        assert_eq!(out.title, "第1章 试炼");
        assert!(out.content.starts_with("第1章 试炼\n\n"));
        assert!(out.content.contains("第一行"));
    }
}
