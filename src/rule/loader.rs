//! 规则加载：`rules/*.json` 读取、ID 分配、热重载（ArcSwap 读侧无锁）。

use std::path::{Path, PathBuf};

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use super::Rule;

/// 规则存储：加载后的规则表 + 在线更新互斥。
///
/// - 读路径（搜索/下载匹配规则）经 `rules()` 无锁读取；
/// - 写路径（启动加载 / `/rules/update` 热重载）经 `swap` 原子替换。
#[derive(Debug)]
pub struct RuleStore {
    inner: ArcSwap<Vec<Rule>>,
    /// 规则目录（`active_rules` 相对此目录解析，在线更新写回同目录）
    rules_dir: PathBuf,
    /// 激活规则文件路径（相对 rules 目录）
    active_rules: String,
    /// 在线更新互斥（防止并发拉取）
    update_lock: RwLock<()>,
}

/// 规则加载错误
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// 规则文件不存在
    #[error("规则文件不存在: {0}")]
    NotFound(PathBuf),
    /// JSON 解析失败
    #[error("解析规则文件失败: {0}")]
    Parse(#[from] serde_json::Error),
    /// 文件读取失败
    #[error("读取规则文件失败: {0}")]
    Io(#[from] std::io::Error),
}

impl RuleStore {
    /// 创建存储并从 `rules_dir/active_rules` 加载。
    ///
    /// # Errors
    /// 规则文件缺失或解析失败时返回 [`RuleError`]。
    pub fn load(rules_dir: &Path, active_rules: &str) -> Result<Self, RuleError> {
        let rules = Self::parse_rules_file(&rules_dir.join(active_rules))?;
        tracing::info!(count = rules.len(), active_rules, "规则加载完成");
        Ok(Self {
            inner: ArcSwap::from_pointee(rules),
            rules_dir: rules_dir.to_path_buf(),
            active_rules: active_rules.to_owned(),
            update_lock: RwLock::new(()),
        })
    }

    /// 解析单个规则文件：反序列化并按顺序分配自增 ID（与源项目 `SourceUtils` 一致）。
    fn parse_rules_file(path: &Path) -> Result<Vec<Rule>, RuleError> {
        if !path.exists() {
            return Err(RuleError::NotFound(path.to_path_buf()));
        }
        let content = std::fs::read_to_string(path)?;
        Self::parse_rules_from_str(&content)
    }

    /// 解析规则 JSON 文本：反序列化、默认值填充、自增 ID 分配。
    ///
    /// 供启动加载与 `/rules/update` 在线更新共用（更新侧先校验再落盘）。
    ///
    /// # Errors
    /// JSON 解析失败时返回 [`RuleError::Parse`]。
    pub fn parse_rules_from_str(content: &str) -> Result<Vec<Rule>, RuleError> {
        let mut rules: Vec<Rule> = serde_json::from_str(content)?;
        for rule in &mut rules {
            apply_rule_defaults(rule);
        }
        for (i, rule) in rules.iter_mut().enumerate() {
            // ID 从 1 开始，与源项目一致；规则数不可能超出 u32 范围，饱和处理即可
            rule.id = u32::try_from(i + 1).unwrap_or(u32::MAX);
        }
        Ok(rules)
    }

    /// 按书籍详情页 URL 匹配规则（对应源项目 `SourceUtils.getRule(bookUrl)`：前缀匹配）。
    pub fn rule_for_url(&self, book_url: &str) -> Option<Rule> {
        self.rules().iter().find(|r| book_url.starts_with(&r.url)).cloned()
    }

    /// 当前规则快照（Arc 克隆，读侧无锁）
    pub fn rules(&self) -> std::sync::Arc<Vec<Rule>> {
        self.inner.load_full()
    }

    /// 规则文件绝对路径（用于在线更新写回）
    pub fn active_rules_path(&self) -> PathBuf {
        self.rules_dir.join(&self.active_rules)
    }

    /// 在线更新互斥锁（`/rules/update` 持锁期间阻止并发拉取）
    pub fn update_lock(&self) -> &RwLock<()> {
        &self.update_lock
    }

    /// 原子替换规则（热重载）：新规则已校验可解析且分配 ID。
    pub fn swap(&self, rules: Vec<Rule>) {
        self.inner.store(std::sync::Arc::new(rules));
    }
}

/// meta 默认选择器（对应源项目 `SourceUtils` 常量：详情字段规则缺省时兜底）。
mod meta {
    pub const BOOK_NAME: &str = r#"meta[property="og:novel:book_name"]"#;
    pub const AUTHOR: &str = r#"meta[property="og:novel:author"]"#;
    pub const INTRO: &str = r#"meta[name="description"]"#;
    pub const CATEGORY: &str = r#"meta[property="og:novel:category"]"#;
    pub const COVER_URL: &str = r#"meta[property="og:image"]"#;
    pub const LATEST_CHAPTER: &str = r#"meta[property="og:novel:latest_chapter_name"]"#;
    pub const LATEST_CHAPTER_URL: &str = r#"meta[property="og:novel:latest_chapter_url"]"#;
    pub const LAST_UPDATE_TIME: &str = r#"meta[property="og:novel:update_time"]"#;
    pub const STATUS: &str = r#"meta[property="og:novel:status"]"#;
}

/// 填充规则默认值（对应源项目 `SourceUtils.applyDefaultRule`）：
/// language、各段 baseUri/timeout、详情页 meta 选择器兜底。
fn apply_rule_defaults(rule: &mut Rule) {
    // TODO(M5): 简繁转换时按系统语言判定；当前固定 zh-CN
    if rule.language.is_empty() {
        "zh-CN".clone_into(&mut rule.language);
    }
    // 各段 baseUri 缺省为书源首页；timeout 缺省 search/book/chapter 15s、toc 60s（与源项目一致）
    if rule.search.base_uri.is_empty() {
        rule.search.base_uri = rule.url.clone();
    }
    rule.search.timeout.get_or_insert(15);
    if rule.book.base_uri.is_empty() {
        rule.book.base_uri = rule.url.clone();
    }
    rule.book.timeout.get_or_insert(15);
    if rule.toc.base_uri.is_empty() {
        rule.toc.base_uri = rule.url.clone();
    }
    rule.toc.timeout.get_or_insert(60);
    if rule.chapter.base_uri.is_empty() {
        rule.chapter.base_uri = rule.url.clone();
    }
    rule.chapter.timeout.get_or_insert(15);
    let book = &mut rule.book;
    let fields = [
        (&mut book.book_name, meta::BOOK_NAME),
        (&mut book.author, meta::AUTHOR),
        (&mut book.intro, meta::INTRO),
        (&mut book.cover_url, meta::COVER_URL),
        (&mut book.category, meta::CATEGORY),
        (&mut book.latest_chapter, meta::LATEST_CHAPTER),
        (&mut book.latest_chapter_url, meta::LATEST_CHAPTER_URL),
        (&mut book.last_update_time, meta::LAST_UPDATE_TIME),
        (&mut book.status, meta::STATUS),
    ];
    for (field, default) in fields {
        if field.is_empty() {
            default.clone_into(field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules_file_assigns_sequential_ids() {
        let dir = std::env::temp_dir().join("sonovel-rs-test-rules");
        std::fs::create_dir_all(&dir).expect("创建目录失败");
        let path = dir.join("test.json");
        std::fs::write(
            &path,
            r#"[{"url":"http://a/","name":"源A","search":{"url":"http://a/s","result":"tr"}},
                {"url":"http://b/","name":"源B"}]"#,
        )
        .expect("写入失败");

        let rules = RuleStore::parse_rules_file(&path).expect("解析失败");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, 1);
        assert_eq!(rules[1].id, 2);
        assert_eq!(rules[0].search.result, "tr");
        // camelCase 字段映射
        assert_eq!(rules[0].search.url, "http://a/s");
    }

    #[test]
    fn parse_rules_from_str_applies_defaults_and_ids() {
        let rules = RuleStore::parse_rules_from_str(
            r#"[{"url":"http://a/","name":"源A","book":{"bookName":"自定义书名"}}]"#,
        )
        .expect("解析失败");
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        // ID 从 1 自增分配（JSON 中不存在该字段）
        assert_eq!(r.id, 1);
        // 默认值：语言 / 各段 baseUri 兜底为书源首页 / timeout 缺省
        assert_eq!(r.language, "zh-CN");
        assert_eq!(r.search.base_uri, "http://a/");
        assert_eq!(r.book.base_uri, "http://a/");
        assert_eq!(r.book.timeout, Some(15));
        assert_eq!(r.toc.timeout, Some(60));
        assert_eq!(r.chapter.timeout, Some(15));
        // 显式字段保留；缺省详情字段兜底 meta 选择器
        assert_eq!(r.book.book_name, "自定义书名");
        assert_eq!(r.book.author, meta::AUTHOR);
        assert_eq!(r.book.intro, meta::INTRO);
    }

    #[test]
    fn parse_rules_from_str_invalid_json_errors() {
        assert!(RuleStore::parse_rules_from_str("不是 JSON").is_err());
    }

    #[test]
    fn parse_rules_from_str_empty_array_yields_empty() {
        let rules = RuleStore::parse_rules_from_str("[]").expect("解析失败");
        assert!(rules.is_empty());
    }

    #[test]
    fn parse_real_main_json_succeeds() {
        // 回归：仓库自带 rules/main.json 必须可完整解析（选择器兼容性另测）
        let path = Path::new("rules").join("main.json");
        if !path.exists() {
            // 测试从 crate 根运行时路径存在；跳过 IDE 内其他 cwd
            return;
        }
        let rules = RuleStore::parse_rules_file(&path).expect("main.json 解析失败");
        assert!(!rules.is_empty(), "main.json 不应为空");
        for r in &rules {
            assert!(!r.url.is_empty(), "规则 {} 缺少 url", r.id);
        }
    }
}
