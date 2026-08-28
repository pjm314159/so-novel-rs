//! 书源规则模型：与源项目 Java `Rule.java` 字段一一对应。
//!
//! 规则文件为 `rules/*.json`，字段为 camelCase（`baseUri`/`bookName`/`filterTxt` 等），
//! 与源项目 `bundle/rules/main.json` 完全一致。

mod dsl;
pub mod loader;
pub mod selector;

use serde::{Deserialize, Serialize};

/// 书源规则（对应 Java `com.pcdd.sonovel.model.Rule`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Rule {
    /// 自增 ID，加载时按顺序分配（JSON 文件中不存在）
    #[serde(skip_deserializing)]
    pub id: u32,
    /// 书源首页
    pub url: String,
    /// 书源名称
    pub name: String,
    /// 备注
    pub comment: String,
    /// 源站内容语言（zh-CN / zh-TW / zh-Hant），用于简繁转换判断
    pub language: String,
    /// 是否需要代理
    pub need_proxy: bool,
    /// 是否禁用
    pub disabled: bool,
    /// 搜索规则
    pub search: SearchRule,
    /// 书籍详情规则
    pub book: BookRule,
    /// 目录规则
    pub toc: TocRule,
    /// 章节规则
    pub chapter: ChapterRule,
    /// 抓取行为覆盖（并发/间隔/重试）
    pub crawl: CrawlRule,
}

/// 搜索规则（对应 Java `Rule.Search`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchRule {
    /// 是否纳入聚合搜索
    pub disabled: bool,
    /// 基础 URI（相对链接补全）
    pub base_uri: String,
    /// 超时（毫秒）
    pub timeout: Option<u32>,
    /// 搜索请求 URL
    pub url: String,
    /// 请求方法（get/post）
    pub method: String,
    /// POST 请求体模板，形如 `{searchkey: %s}`
    pub data: String,
    /// 附加 cookie 头
    pub cookies: String,
    /// 结果列表选择器
    pub result: String,
    pub book_name: String,
    pub author: String,
    pub category: String,
    pub latest_chapter: String,
    pub last_update_time: String,
    pub status: String,
    pub word_count: String,
    /// 搜索结果下一页选择器
    pub next_page: String,
}

/// 书籍详情规则（对应 Java `Rule.Book`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BookRule {
    pub base_uri: String,
    pub timeout: Option<u32>,
    /// 详情页 URL 模式（多数书源即书页本身，可空）
    pub url: String,
    pub book_name: String,
    pub author: String,
    pub intro: String,
    pub category: String,
    pub cover_url: String,
    pub latest_chapter: String,
    pub latest_chapter_url: String,
    pub last_update_time: String,
    pub status: String,
}

/// 目录规则（对应 Java `Rule.Toc`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct TocRule {
    pub base_uri: String,
    pub timeout: Option<u32>,
    pub url: String,
    /// 目录容器选择器
    pub list: String,
    /// 章节条目选择器
    pub item: String,
    /// 目录是否倒序
    pub is_desc: bool,
    /// 目录分页下一页选择器
    pub next_page: String,
}

/// 章节规则（对应 Java `Rule.Chapter`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ChapterRule {
    pub base_uri: String,
    pub timeout: Option<u32>,
    /// 章节标题选择器（测试用）
    pub title: String,
    /// 正文选择器
    pub content: String,
    /// 段落标签是否闭合（决定切段正则）
    pub paragraph_tag_closed: bool,
    /// 段落标签（如 `<br>+`）
    pub paragraph_tag: String,
    /// 文字过滤正则（`|` 分隔）
    pub filter_txt: String,
    /// 标签/选择器过滤（`,` 分隔，删除节点）
    pub filter_tag: String,
    /// 章节分页：下一页 HTML 元素选择器
    pub next_page: String,
    /// 章节分页：位于 JS 中的下一页链接
    pub next_page_in_js: String,
    /// 下一章链接的正则
    pub next_chapter_link: String,
}

/// 抓取行为覆盖（对应 Java `Rule.Crawl`）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct CrawlRule {
    pub concurrency: Option<u32>,
    pub min_interval: Option<u64>,
    pub max_interval: Option<u64>,
    pub max_attempts: Option<u32>,
    pub retry_min_interval: Option<u64>,
    pub retry_max_interval: Option<u64>,
}
