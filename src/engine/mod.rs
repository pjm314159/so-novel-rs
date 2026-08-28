//! 下载流水线（Crawler 对应物）：多 Job 并发调度、进度注册表与公共抓取助手。
//!
//! M2：聚合搜索（`search`）；M3：Toc/详情/章节并发抓取/SSE 进度/章节缓存落盘；
//! M4：简繁转换（`lang`）+ txt/epub 合并输出（`merge`，`pipeline` 收尾接入）。

pub mod book;
pub mod chapter;
pub mod lang;
pub mod merge;
pub mod pipeline;
pub mod render;
pub mod search;
pub mod toc;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use dashmap::DashMap;
use rand::Rng;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::config::AppConfig;
use crate::rule::Rule;

/// 下载任务 ID（短随机串，公开类型别名）
pub type JobId = String;

/// 生成任务 ID（16 位十六进制随机串；终态任务长期保留，64 位随机避免碰撞）。
pub fn new_job_id() -> JobId {
    use rand::Rng as _;
    format!("{:016x}", rand::rng().random::<u64>())
}

/// 动态错误类型（抓取/解析链路逐层传递上下文）
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 章节目录条目（对应源项目 `Chapter`；content 为落盘前的瞬时数据，不入目录模型）
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// 全书顺序号（1 起，去重后的最终位置）
    pub index: u32,
    pub title: String,
    pub url: String,
}

/// 书籍详情（对应源项目 `Book`，M5 接入简繁转换后扩展语言处理）
#[derive(Debug, Clone, Default)]
pub struct Book {
    pub book_name: String,
    pub author: String,
    pub intro: String,
    pub cover_url: String,
    pub category: String,
    pub latest_chapter: String,
    pub latest_chapter_url: String,
    pub last_update_time: String,
    pub status: String,
}

/// Job 生命周期阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// 解析目录与详情
    Fetching,
    /// 并发下载章节
    Downloading,
    /// 合并输出（txt/epub）
    Merging,
    /// 完成（产物文件名见 `filename`）
    Done,
    /// 失败（错误信息见 `failed_reason`）
    Failed,
}

impl Phase {
    /// 是否终态（Done/Failed）
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }
}

/// 下载任务状态（`SSE` 推送的数据源，camelCase 保持前端兼容）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobState {
    pub job_id: JobId,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    /// 当前正在抓取的章节标题
    pub current: String,
    pub phase: Phase,
    pub failed_reason: Option<String>,
    /// 完成后的产物文件名（SSE `event: done` 推送）
    pub filename: Option<String>,
    /// 创建序号（取最近活跃 Job 用，不出现在 SSE 载荷中）
    #[serde(skip)]
    pub seq: u64,
}

#[derive(Debug)]
struct JobEntry {
    state: JobState,
    tx: broadcast::Sender<JobState>,
}

/// 全局下载任务注册表：`DashMap<JobId, JobEntry>`。
///
/// 设计约束（见设计文档 §4/§5.2）：
/// - 活跃（非终态）Job 数达到 `crawl.max_jobs` 时，新 `/book-fetch` 返回 409；
/// - 每次状态更新向该 Job 的 `broadcast` 通道推送全量快照（SSE 每章推送）；
/// - 完成的任务状态保留（SSE 可回放终态），后续由 TTL/上限清理。
#[derive(Debug, Default)]
pub struct JobRegistry {
    jobs: DashMap<JobId, JobEntry>,
    seq: AtomicU64,
    /// 创建临界区：check-then-insert 原子化（并发 `/book-fetch` 不会超发）
    create_lock: Mutex<()>,
}

impl JobRegistry {
    /// 注册新任务，返回初始订阅（SSE 建立时先回放当前快照）
    pub fn create(&self, job_id: JobId) -> broadcast::Receiver<JobState> {
        let (tx, rx) = broadcast::channel(64);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let state = JobState {
            job_id: job_id.clone(),
            total: 0,
            done: 0,
            failed: 0,
            current: String::new(),
            phase: Phase::Fetching,
            failed_reason: None,
            filename: None,
            seq,
        };
        self.jobs.insert(job_id, JobEntry { state, tx });
        rx
    }

    /// 原子创建任务：活跃数达 `max_active` 时返回 `None`（`/book-fetch` 回 409）。
    ///
    /// check-then-insert 在同一临界区完成；终态任务不占槽位。
    pub fn try_create(&self, job_id: JobId, max_active: usize) -> Option<broadcast::Receiver<JobState>> {
        let _guard = self.create_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.active_count() >= max_active {
            return None;
        }
        Some(self.create(job_id))
    }

    /// 当前活跃（非终态）Job 数
    pub fn active_count(&self) -> usize {
        self.jobs.iter().filter(|e| !e.value().state.phase.is_terminal()).count()
    }

    /// 查询任务快照（SSE 按 jobId 订阅）
    pub fn get(&self, id: &str) -> Option<JobState> {
        let entry = self.jobs.get(id)?;
        let mut state = entry.state.clone();
        id.clone_into(&mut state.job_id);
        Some(state)
    }

    /// 就地更新状态并广播快照（任务不存在时忽略）
    pub fn update(&self, id: &str, f: impl FnOnce(&mut JobState)) {
        if let Some(mut entry) = self.jobs.get_mut(id) {
            f(&mut entry.state);
            let _ = entry.tx.send(entry.state.clone());
        }
    }

    /// 订阅任务进度（初始快照 + 增量通道）
    pub fn subscribe(&self, id: &str) -> Option<(JobState, broadcast::Receiver<JobState>)> {
        let entry = self.jobs.get(id)?;
        let mut state = entry.state.clone();
        id.clone_into(&mut state.job_id);
        Some((state, entry.tx.subscribe()))
    }

    /// 最近创建的活跃 Job（SSE 无 id 参数时的兼容行为）
    pub fn latest_active(&self) -> Option<JobId> {
        self.jobs
            .iter()
            .filter(|e| !e.value().state.phase.is_terminal())
            .max_by_key(|e| e.value().state.seq)
            .map(|e| e.key().clone())
    }
}

/// Cloudflare 拦截标题（对应源项目 `CF_STRONG_TITLES`）。
pub(crate) fn has_cf(document: &scraper::Html) -> bool {
    const CF_TITLES: &[&str] = &[
        "Just a moment...",
        "403 Forbidden",
        "Attention Required",
        "Checking your browser before accessing",
    ];
    let title = document
        .select(&scraper::Selector::parse("title").expect("title 选择器恒合法"))
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();
    CF_TITLES.contains(&title.as_str())
}

/// 抓取页面正文并按需经 cf-bypass 绕过（对应源项目各 Parser 的 `handleCloudflareBypass`）。
///
/// # Errors
/// 网络失败/响应超限时透传 [`crate::util::http::HttpError`]；检测到 CF 验证但未配置
/// cf-bypass 时返回错误（调用方跳过该页/章节）。
pub(crate) async fn fetch_body_with_cf(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    cf_bypass: &str,
) -> Result<String, BoxError> {
    let mut body = crate::util::http::fetch_page(
        client,
        url,
        &crate::util::http::PageRequest {
            timeout_secs,
            referer: crate::util::http::origin_of(url).as_deref(),
            ..Default::default()
        },
    )
    .await?;
    let needs_bypass = {
        let document = scraper::Html::parse_document(&body);
        has_cf(&document)
    };
    if needs_bypass {
        if cf_bypass.is_empty() {
            return Err(format!("页面 {url} 存在 Cloudflare 真人验证，且未配置 cf-bypass").into());
        }
        let bypass_url = format!("{cf_bypass}/html?url={url}");
        body = crate::util::http::fetch_page(
            client,
            &bypass_url,
            &crate::util::http::PageRequest { timeout_secs: 30, ..Default::default() },
        )
        .await?;
    }
    Ok(body)
}

/// 抓取行为参数（`[crawl]` 配置与规则 `rule.crawl` 覆盖合并，见设计文档 §5.2.3）
#[derive(Debug, Clone)]
pub(crate) struct CrawlParams {
    pub concurrency: u32,
    pub min_interval: u64,
    pub max_interval: u64,
    pub enable_retry: bool,
    pub max_retries: u32,
    pub retry_min_interval: u64,
    pub retry_max_interval: u64,
}

/// 合并配置与规则级抓取覆盖。
pub(crate) fn crawl_params(config: &AppConfig, rule: &Rule) -> CrawlParams {
    let c = &config.crawl;
    let r = &rule.crawl;
    CrawlParams {
        concurrency: r.concurrency.unwrap_or(c.concurrency).clamp(1, 100),
        min_interval: r.min_interval.unwrap_or(c.min_interval),
        max_interval: r.max_interval.unwrap_or(c.max_interval),
        enable_retry: c.enable_retry,
        max_retries: r.max_attempts.unwrap_or(c.max_retries),
        retry_min_interval: r.retry_min_interval.unwrap_or(c.retry_min_interval),
        retry_max_interval: r.retry_max_interval.unwrap_or(c.retry_max_interval),
    }
}

/// 随机请求间隔（对应源项目 `CrawlUtils.randomInterval`；区间非法时退化为下界）。
pub(crate) fn random_interval(min_ms: u64, max_ms: u64) -> Duration {
    let ms = if max_ms > min_ms { rand::rng().random_range(min_ms..max_ms) } else { min_ms };
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: &str, phase: Phase, seq: u64) -> JobState {
        JobState {
            job_id: id.into(),
            total: 100,
            done: 0,
            failed: 0,
            current: String::new(),
            phase,
            failed_reason: None,
            filename: None,
            seq,
        }
    }

    #[test]
    fn active_count_counts_only_non_terminal_jobs() {
        let reg = JobRegistry::default();
        reg.jobs.insert(
            "a".into(),
            JobEntry { state: state("a", Phase::Downloading, 1), tx: broadcast::channel(1).0 },
        );
        reg.jobs.insert(
            "b".into(),
            JobEntry { state: state("b", Phase::Merging, 2), tx: broadcast::channel(1).0 },
        );
        reg.jobs
            .insert("c".into(), JobEntry { state: state("c", Phase::Done, 3), tx: broadcast::channel(1).0 });
        reg.jobs.insert(
            "d".into(),
            JobEntry { state: state("d", Phase::Failed, 4), tx: broadcast::channel(1).0 },
        );
        assert_eq!(reg.active_count(), 2);
    }

    #[test]
    fn get_returns_inserted_state() {
        let reg = JobRegistry::default();
        reg.jobs.insert(
            "x".into(),
            JobEntry { state: state("x", Phase::Fetching, 1), tx: broadcast::channel(1).0 },
        );
        assert_eq!(reg.get("x").expect("任务应存在").phase, Phase::Fetching);
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn update_broadcasts_snapshot_to_subscribers() {
        let reg = JobRegistry::default();
        let mut rx = reg.create("j".into());
        reg.update("j", |s| {
            s.total = 10;
            s.done = 3;
        });
        let snapshot = rx.try_recv().expect("应收到广播快照");
        assert_eq!((snapshot.total, snapshot.done), (10, 3));
        assert_eq!(reg.get("j").expect("任务应存在").done, 3);
    }

    #[test]
    fn latest_active_picks_most_recent_non_terminal() {
        let reg = JobRegistry::default();
        reg.jobs
            .insert("a".into(), JobEntry { state: state("a", Phase::Done, 1), tx: broadcast::channel(1).0 });
        reg.jobs.insert(
            "b".into(),
            JobEntry { state: state("b", Phase::Downloading, 5), tx: broadcast::channel(1).0 },
        );
        reg.jobs.insert(
            "c".into(),
            JobEntry { state: state("c", Phase::Downloading, 2), tx: broadcast::channel(1).0 },
        );
        assert_eq!(reg.latest_active().as_deref(), Some("b"));
    }

    #[test]
    fn crawl_params_merge_rule_overrides() {
        let mut config = AppConfig::default();
        config.crawl.concurrency = 50;
        let mut rule = Rule::default();
        rule.crawl.concurrency = Some(30);
        rule.crawl.max_attempts = Some(9);
        let p = crawl_params(&config, &rule);
        assert_eq!(p.concurrency, 30);
        assert_eq!(p.max_retries, 9);
    }

    #[test]
    fn try_create_enforces_max_active_and_releases_on_terminal() {
        let reg = JobRegistry::default();
        assert!(reg.try_create("a".into(), 1).is_some());
        assert!(reg.try_create("b".into(), 1).is_none(), "活跃数达上限应拒绝");
        reg.update("a", |s| s.phase = Phase::Done);
        assert!(reg.try_create("c".into(), 1).is_some(), "终态应释放槽位");
    }

    #[test]
    fn new_job_id_is_hex_and_unique() {
        let a = new_job_id();
        let b = new_job_id();
        assert_eq!(a.len(), 16, "应为 16 位十六进制: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
