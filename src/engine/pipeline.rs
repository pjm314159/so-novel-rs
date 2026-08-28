//! 下载流水线（对应源项目 `Crawler`）：Job 编排、章节并发抓取与缓存落盘、进度更新。
//!
//! 每 `/book-fetch` 请求生成一个 Job task：解析目录/详情 → 创建书籍目录 →
//! `Semaphore(concurrency)` 章节并发（每章：抓取 → 净化渲染 → 落盘）→ Merging → Done。
//! 某章最终失败即中止剩余章节（fail-fast），任务转入 Failed 且不产出文件；合并输出（txt/epub）在 M4 接入。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::{
    book, chapter, crawl_params, lang, merge, render, toc, BoxError, Chapter, CrawlParams, JobId, JobRegistry,
};
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
    let ctx = Arc::new(ChapterCtx {
        client: client.clone(),
        rule: rule.clone(),
        params,
        cf_bypass: config.global.cf_bypass.clone(),
        registry: Arc::clone(registry),
        job_id: job_id.to_owned(),
        dir: dir.clone(),
        ext: ext.to_owned(),
        file_ext,
        digit_count,
        conversion,
    });
    download_chapters(ctx, toc, max_concurrent).await;

    // 4. 合并输出（Phase::Merging → Done）
    let failed = registry.get(job_id).map_or(0, |s| s.failed);
    if failed > 0 {
        // 存在失败章节：输出必然残缺，不生成最终文件（章节缓存保留，重下可命中）
        return Err(format!("{failed} 章下载失败，已中止剩余请求，未生成输出文件").into());
    }
    registry.update(job_id, |s| s.phase = super::Phase::Merging);
    let output_name = merge::merge_and_finalize(config, &client, &book, &dir).await?;
    registry.update(job_id, |s| s.filename = Some(output_name.clone()));
    tracing::info!(job = job_id, file = %output_name, "下载完成");
    Ok(())
}

/// 章节任务共享上下文（owned 字段便于 `spawn`，Arc 共享）
struct ChapterCtx {
    client: reqwest::Client,
    rule: Rule,
    params: CrawlParams,
    cf_bypass: String,
    registry: Arc<JobRegistry>,
    job_id: JobId,
    dir: std::path::PathBuf,
    ext: String,
    file_ext: &'static str,
    digit_count: usize,
    conversion: Option<lang::Conversion>,
}

/// 章节并发下载（fail-fast：任一章最终失败即置位中止标志，排队任务静默跳过不再发请求；
/// panic 任务计入 failed）。失败数实时累加进 `registry.failed`。
async fn download_chapters(ctx: Arc<ChapterCtx>, toc: Vec<Chapter>, max_concurrent: usize) {
    let sem = Arc::new(Semaphore::new(max_concurrent));
    let aborted = Arc::new(AtomicBool::new(false));
    let mut set: JoinSet<()> = JoinSet::new();
    for ch in toc {
        let permit = sem.clone().acquire_owned();
        let ctx = Arc::clone(&ctx);
        let aborted = Arc::clone(&aborted);
        set.spawn(async move {
            let _permit = permit.await.expect("信号灯在任务运行期间不会被关闭");
            if aborted.load(Ordering::Relaxed) {
                // 已有失败章节：静默跳过（计 failed，不重复刷日志）
                ctx.registry.update(&ctx.job_id, |s| s.failed += 1);
                return;
            }
            match chapter::fetch_chapter(
                &ctx.client,
                &ctx.rule,
                &ctx.params,
                &ctx.cf_bypass,
                &ch.url,
                &ch.title,
            )
            .await
            {
                Ok(raw) => {
                    let mut rendered = render::render_chapter(&ctx.rule.chapter, &ctx.ext, &ch.title, &raw);
                    // 简繁转换（对应源项目 ChapterParser 的 ChineseConverter.convert）
                    if let Some(c) = ctx.conversion {
                        lang::convert_chapter_fields(&mut rendered.title, &mut rendered.content, c);
                    }
                    let path = ctx.dir.join(format!(
                        "{:0width$}_{}.{}",
                        ch.index,
                        sanitize_file_name(&rendered.title),
                        ctx.file_ext,
                        width = ctx.digit_count,
                    ));
                    if let Err(e) = tokio::fs::write(&path, rendered.content.as_bytes()).await {
                        tracing::error!(chapter = %ch.title, error = %e, "章节缓存落盘失败");
                        aborted.store(true, Ordering::Relaxed);
                        ctx.registry.update(&ctx.job_id, |s| s.failed += 1);
                        return;
                    }
                    ctx.registry.update(&ctx.job_id, |s| {
                        s.done += 1;
                        s.current.clone_from(&rendered.title);
                    });
                }
                Err(e) => {
                    // 仅首个失败记录详细错误，随后中止剩余章节请求
                    if !aborted.swap(true, Ordering::Relaxed) {
                        tracing::error!(
                            chapter = %ch.title, url = %ch.url, error = %e,
                            "章节下载失败（含重试），中止剩余章节请求"
                        );
                    }
                    ctx.registry.update(&ctx.job_id, |s| s.failed += 1);
                }
            }
        });
    }
    let mut panicked = 0u32;
    while let Some(res) = set.join_next().await {
        if res.is_err() {
            // 任务 panic：章节静默缺失，必须计入 failed（否则 failed=0 假象）
            aborted.store(true, Ordering::Relaxed);
            panicked += 1;
        }
    }
    if panicked > 0 {
        ctx.registry.update(&ctx.job_id, |s| s.failed += panicked);
    }
}
