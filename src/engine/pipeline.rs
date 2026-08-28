//! 下载流水线（对应源项目 `Crawler`）：Job 编排、章节并发抓取与缓存落盘、进度更新。
//!
//! 每 `/book-fetch` 请求生成一个 Job task：解析目录/详情 → 创建书籍目录 →
//! `Semaphore(concurrency)` 章节并发（每章：抓取 → 净化渲染 → 落盘）→ Merging → Done。
//! 章节失败不中断整本书（设计文档 §10）；合并输出（txt/epub）在 M4 接入。

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::{book, chapter, crawl_params, lang, merge, render, toc, BoxError, JobId, JobRegistry};
use crate::config::AppConfig;
use crate::rule::Rule;
use crate::util::file::sanitize_file_name;
use crate::util::http::HttpClients;

/// Job 运行环境（handler 侧组装，全部 owned 便于 spawn）
pub struct JobEnv {
    /// 已应用请求级覆盖（format/language/concurrency）的配置副本
    pub config: AppConfig,
    /// 按 bookUrl 前缀匹配到的规则
    pub rule: Rule,
    pub clients: HttpClients,
    pub jobs: Arc<JobRegistry>,
}

/// Job 顶层任务：终态统一回写注册表。
pub async fn run_job(env: JobEnv, job_id: JobId, book_url: String) {
    let result = run(&env, &job_id, &book_url).await;
    match result {
        Ok(()) => env.jobs.update(&job_id, |s| s.phase = super::Phase::Done),
        Err(e) => {
            tracing::error!(job = %job_id, error = %e, "下载任务失败");
            env.jobs.update(&job_id, |s| {
                s.phase = super::Phase::Failed;
                s.failed_reason = Some(e.to_string());
            });
        }
    }
}

async fn run(env: &JobEnv, job_id: &str, book_url: &str) -> Result<(), BoxError> {
    let config = &env.config;
    let rule = &env.rule;
    let client = env.clients.for_rule(rule.need_proxy).clone();
    let registry = &env.jobs;

    // 1. 目录 + 详情（Phase::Fetching）
    let toc = toc::parse_all(config, &client, rule, book_url).await?;
    if toc.is_empty() {
        return Err("源站章节目录为空，中止下载".into());
    }
    let mut book = book::parse(config, &client, rule, book_url).await?;
    // 简繁转换（对应源项目 BookParser 末尾的 ChineseConverter.convert）
    let conversion = lang::conversion_for(rule.language.as_str(), config.source.language.as_str());
    if let Some(c) = conversion {
        lang::convert_book_fields(&mut book, c);
    }
    tracing::info!(book = %book.book_name, author = %book.author, chapters = toc.len(), "开始下载");

    // 2. 下载临时目录名：书名 (作者) EXT（对应源项目 bookDir）
    let ext = config.download.extname.as_str();
    let book_dir =
        sanitize_file_name(&format!("{} ({}) {}", book.book_name, book.author, ext.to_uppercase()));
    let dir = Path::new(&config.download.download_path).join(&book_dir);
    tokio::fs::create_dir_all(&dir).await?;

    // 3. 章节并发下载（Phase::Downloading）
    let params = crawl_params(config, rule);
    let max_concurrent = usize::try_from(params.concurrency).unwrap_or(usize::MAX).min(toc.len());
    registry.update(job_id, |s| {
        s.phase = super::Phase::Downloading;
        s.total = u32::try_from(toc.len()).unwrap_or(u32::MAX);
        s.current.clone_from(&book.book_name);
    });
    tracing::info!(job = job_id, %book_dir, max_concurrent, "章节并发下载开始");

    // 文件名序号补零位数（全本下载：index ≤ toc.len() 恒可补齐）
    let digit_count = toc.len().to_string().len();
    // txt 落盘 .txt；epub 转换前格式为 html（对应源项目 generateChapterPath）
    let file_ext = if ext == "epub" { "html" } else { "txt" };

    let sem = Arc::new(Semaphore::new(max_concurrent));
    let mut set: JoinSet<()> = JoinSet::new();
    for ch in toc {
        let permit = sem.clone().acquire_owned();
        let client = client.clone();
        let rule = rule.clone();
        let params = params.clone();
        let cf_bypass = config.global.cf_bypass.clone();
        let registry = Arc::clone(registry);
        let job_id = job_id.to_owned();
        let dir = dir.clone();
        let ext = ext.to_owned();
        let file_ext = file_ext.to_owned();
        set.spawn(async move {
            let _permit = permit.await.expect("信号灯在任务运行期间不会被关闭");
            match chapter::fetch_chapter(&client, &rule, &params, &cf_bypass, &ch.url, &ch.title).await {
                Ok(raw) => {
                    let mut rendered = render::render_chapter(&rule.chapter, &ext, &ch.title, &raw);
                    // 简繁转换（对应源项目 ChapterParser 的 ChineseConverter.convert）
                    if let Some(c) = conversion {
                        lang::convert_chapter_fields(&mut rendered.title, &mut rendered.content, c);
                    }
                    let path = dir.join(format!(
                        "{:0width$}_{}.{}",
                        ch.index,
                        sanitize_file_name(&rendered.title),
                        file_ext,
                        width = digit_count,
                    ));
                    if let Err(e) = tokio::fs::write(&path, rendered.content.as_bytes()).await {
                        tracing::error!(chapter = %ch.title, error = %e, "章节缓存落盘失败");
                        registry.update(&job_id, |s| s.failed += 1);
                        return;
                    }
                    registry.update(&job_id, |s| {
                        s.done += 1;
                        s.current.clone_from(&rendered.title);
                    });
                }
                Err(e) => {
                    tracing::error!(chapter = %ch.title, url = %ch.url, error = %e, "章节下载失败（含重试）");
                    registry.update(&job_id, |s| s.failed += 1);
                }
            }
        });
    }
    while set.join_next().await.is_some() {}

    // 4. 合并输出（Phase::Merging → Done）
    registry.update(job_id, |s| s.phase = super::Phase::Merging);
    let output_name = merge::merge_and_finalize(config, &client, &book, &dir).await?;
    registry.update(job_id, |s| s.filename = Some(output_name.clone()));
    let failed = registry.get(job_id).map(|s| s.failed).unwrap_or(0);
    tracing::info!(job = job_id, file = %output_name, failed, "下载完成");
    Ok(())
}
