//! HTTP handlers：请求参数解析与 JSON 响应。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_util::io::ReaderStream;

use super::AppState;
use crate::config::AppConfig;
use crate::engine::pipeline::{self, JobEnv};
use crate::engine::{new_job_id, search};
use crate::rule::loader::RuleStore;
use crate::util::http::random_user_agent;

/// 统一成功响应体 `{ code, message, data }`（源项目 `JsonResponse.ok`，前端 api.js 依赖此包装）
#[derive(Serialize)]
pub struct ApiOk<T> {
    pub code: u16,
    pub message: &'static str,
    pub data: T,
}

impl<T> ApiOk<T> {
    /// 成功包装（HTTP 200 + code 200）
    pub fn wrap(data: T) -> Self {
        Self { code: 200, message: "OK", data }
    }
}

/// 统一错误响应体 `{ code, message, data: null }`（linter.md §10 / 源项目 `JsonResponse.error`）
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    /// 参数校验失败（4xx）
    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    /// 非法路径（403，与源项目 BookDownloadServlet/BookDeleteServlet 一致）
    fn forbidden(message: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: message.into() }
    }

    /// 资源不存在（404）
    fn not_found(message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    /// 服务端操作失败（5xx）
    fn internal(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: message.into() }
    }

    /// 任务/资源冲突（409）
    fn conflict(message: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, message: message.into() }
    }

    /// 上游依赖失败（502）
    fn bad_gateway(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_GATEWAY, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorBody { code: self.status.as_u16(), message: self.message, data: () });
        (self.status, body).into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    code: u16,
    message: String,
    /// 源项目 error 分支 data 恒为 null
    data: (),
}

/// GET `/config`：只读返回运行配置（与源项目 `ConfigServlet` 一致，无 POST 修改）
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<ApiOk<AppConfig>> {
    Json(ApiOk::wrap(state.config.clone()))
}

/// 书源列表项（`/sources` 响应）
#[derive(Serialize)]
pub struct SourceItem {
    pub id: u32,
    pub name: String,
    pub url: String,
    pub comment: String,
    pub disabled: bool,
}

/// GET `/sources`：书源列表（id/name/url/comment/disabled）
pub async fn list_sources(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiOk<Vec<SourceItem>>>, ApiError> {
    if state.config.source.active_rules.is_empty() {
        return Err(ApiError::bad_request("未配置激活规则文件"));
    }
    let rules = state.rules.rules();
    let items = rules
        .iter()
        .filter(|r| !r.disabled)
        .map(|r| SourceItem {
            id: r.id,
            name: r.name.clone(),
            url: r.url.clone(),
            comment: r.comment.clone(),
            disabled: r.disabled,
        })
        .collect();
    Ok(Json(ApiOk::wrap(items)))
}

/// GET `/search` 查询参数
#[derive(Deserialize)]
pub struct SearchParams {
    /// 搜索关键字（书名/作者）
    kw: String,
    /// 每源结果条数上限（不可超过配置上限）
    search_limit: Option<u32>,
}

/// GET `/search`：聚合搜索（并发全部书源，合并 + 相似度过滤排序）
pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<ApiOk<Vec<search::SearchResult>>>, ApiError> {
    let kw = params.kw.trim();
    if kw.is_empty() {
        return Err(ApiError::bad_request("参数 kw 不能为空"));
    }
    let rules = state.rules.rules();
    let mut results = search::aggregated_search(&state.config, &state.http, &rules, kw).await;

    // 客户端限流：不可超过配置上限（对应源项目 AggregatedSearchServlet）
    if let Some(client_limit) = params.search_limit {
        let config_limit = state.config.source.search_limit;
        let limit = if config_limit > 0 && client_limit > config_limit { config_limit } else { client_limit };
        if limit > 0 {
            results.truncate(limit as usize);
        }
    }
    Ok(Json(ApiOk::wrap(results)))
}

/// GET `/suggestion` 查询参数
#[derive(Deserialize)]
pub struct SuggestionParams {
    kw: String,
}

/// 百度搜索建议响应结构（`g[].q`）
#[derive(Deserialize, Default)]
struct BaiduSugrec {
    g: Option<Vec<BaiduSug>>,
}

#[derive(Deserialize)]
struct BaiduSug {
    q: String,
}

/// GET `/suggestion`：搜索建议（转发百度 sugrec API，取前 10 条，与源项目 `SuggestionServlet` 一致）
pub async fn suggestion(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SuggestionParams>,
) -> Result<Json<ApiOk<Vec<String>>>, ApiError> {
    let kw = params.kw.trim();
    if kw.is_empty() {
        return Ok(Json(ApiOk::wrap(Vec::new())));
    }
    let url = format!("https://www.baidu.com/sugrec?prod=pc&wd={}", urlencode(kw));
    let resp = state
        .http
        .direct()
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126.0.0.0")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("搜索建议服务不可用: {e}"),
        })?;
    let body = resp.text().await.map_err(|e| ApiError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("读取搜索建议失败: {e}"),
    })?;
    let parsed: BaiduSugrec = serde_json::from_str(&body).unwrap_or_default();
    let items = parsed.g.unwrap_or_default().into_iter().take(10).map(|s| s.q).collect();
    Ok(Json(ApiOk::wrap(items)))
}

/// 简单百分号编码（仅查询参数值场景：中文与保留字符）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0xF)]));
            }
        }
    }
    out
}

/// GET `/book-fetch` 查询参数（与源项目 `BookFetchServlet` 一致）
#[derive(Deserialize)]
pub struct BookFetchParams {
    /// 书籍详情页 URL
    url: String,
    /// 输出格式覆盖：txt | epub（重写版已裁剪 html/pdf）
    format: Option<String>,
    /// 目标语言覆盖：zh-CN | zh-TW | zh-Hant
    language: Option<String>,
    /// Job 内章节并发覆盖
    concurrency: Option<u32>,
}

/// `/book-fetch` 受理响应
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobAccepted {
    pub job_id: String,
}

/// GET `/book-fetch`：受理下载任务（异步执行，进度经 `/download-progress` 订阅）。
///
/// 与源项目差异（设计文档 §5.4）：立即受理返回 `{ jobId }`（202）而非同步阻塞至下载完成；
/// 活跃 Job 数达 `crawl.max_jobs` 时返回 409。
pub async fn book_fetch(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BookFetchParams>,
) -> Result<(StatusCode, Json<ApiOk<JobAccepted>>), ApiError> {
    let url = params.url.trim().to_owned();
    if url.is_empty() {
        return Err(ApiError::bad_request("参数 url 不能为空"));
    }

    // 请求级覆盖（对应源项目 downloadFileToServer 的配置副本）
    let mut config = state.config.clone();
    if let Some(format) = non_empty(params.format.as_deref()) {
        let f = format.to_ascii_lowercase();
        if f != "txt" && f != "epub" {
            return Err(ApiError::bad_request(format!("不支持的下载格式: {format}，可选: txt, epub")));
        }
        config.download.extname = f;
    }
    if let Some(lang) = non_empty(params.language.as_deref()) {
        let l = lang.to_ascii_lowercase();
        if !matches!(l.as_str(), "zh-cn" | "zh-tw" | "zh-hant") {
            return Err(ApiError::bad_request(format!("不支持的语言: {lang}，可选: zh-CN, zh-TW, zh-Hant")));
        }
        config.source.language = l;
    }
    if let Some(concurrency) = params.concurrency {
        let max = config.crawl.concurrency.max(1);
        if !(1..=max).contains(&concurrency) {
            return Err(ApiError::bad_request(format!("并发数须在 1~{max} 之间")));
        }
        config.crawl.concurrency = concurrency;
    }

    let rule = state
        .rules
        .rule_for_url(&url)
        .ok_or_else(|| ApiError::bad_request(format!("未找到 URL 对应的书源规则: {url}")))?;

    let max_jobs = state.config.crawl.max_jobs.max(1) as usize;
    let job_id = new_job_id();
    state
        .jobs
        .try_create(job_id.clone(), max_jobs)
        .ok_or_else(|| ApiError::conflict(format!("当前活跃下载任务已达上限（{max_jobs}），请稍后再试")))?;

    let env = JobEnv { config, rule, clients: state.http.clone(), jobs: Arc::clone(&state.jobs) };
    tokio::spawn(pipeline::run_job(env, job_id.clone(), url));
    tracing::info!(job = %job_id, "下载任务已受理");
    Ok((StatusCode::ACCEPTED, Json(ApiOk::wrap(JobAccepted { job_id }))))
}

/// 本地图书项（`/local-books` 响应，与源项目 `LocalBookItem` 字段一致）
#[derive(Serialize)]
pub struct LocalBookItem {
    pub name: String,
    pub size: u64,
    /// 修改时间（Unix 毫秒，与 Java `File.lastModified` 语义一致）
    pub timestamp: u64,
}

/// GET `/local-books`：列出下载目录内的产物文件（仅顶层文件，对应源项目 `LocalBookListServlet`）
pub async fn local_books(State(state): State<Arc<AppState>>) -> Json<ApiOk<Vec<LocalBookItem>>> {
    let mut items = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&state.config.download.download_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                if meta.is_file() {
                    items.push(LocalBookItem {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        size: meta.len(),
                        timestamp: millis_since_epoch(meta.modified()),
                    });
                }
            }
        }
    }
    Json(ApiOk::wrap(items))
}

/// 修改时间 → Unix 毫秒（早于 epoch 或不可用时记 0）
fn millis_since_epoch(t: Result<SystemTime, std::io::Error>) -> u64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// 文件类端点（`/book-download`、`/book-delete`）共用查询参数
#[derive(Deserialize)]
pub struct FileParams {
    filename: String,
}

/// 解析 filename → 下载目录内的规范文件路径（防路径穿越，对应源项目 canonical 前缀校验）。
///
/// 错误码与源项目一致：空参 400 / 词法越界 403 / 不存在 404。
async fn resolve_download_file(download_path: &str, filename: &str) -> Result<PathBuf, ApiError> {
    let filename = filename.trim();
    if filename.is_empty() {
        return Err(ApiError::bad_request("参数 filename 不能为空"));
    }
    // 词法校验先行：绝对路径 / 上跳目录（`..`）直接拒绝
    let rel = Path::new(filename);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ApiError::forbidden("非法路径"));
    }
    let base = tokio::fs::canonicalize(download_path).await.map_err(|_| ApiError::not_found("文件不存在"))?;
    // 规范化展开符号链接/`.`；不存在 → 404
    let file =
        tokio::fs::canonicalize(base.join(rel)).await.map_err(|_| ApiError::not_found("文件不存在"))?;
    // 防御：解析后（含符号链接）仍须位于下载目录内且是普通文件
    if !file.starts_with(&base) || !file.is_file() {
        return Err(ApiError::forbidden("非法路径"));
    }
    Ok(file)
}

/// GET `/book-download`：产物文件流（octet-stream + Content-Disposition，对应源项目 `BookDownloadServlet`）。
///
/// `ReaderStream` 流式发送，不整本读入内存。
pub async fn book_download(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileParams>,
) -> Result<Response, ApiError> {
    let file = resolve_download_file(&state.config.download.download_path, &params.filename).await?;
    let filename = params.filename.trim();
    let len = tokio::fs::metadata(&file)
        .await
        .map_err(|e| ApiError::internal(format!("读取文件信息失败: {e}")))?
        .len();
    let stream =
        tokio::fs::File::open(&file).await.map_err(|e| ApiError::internal(format!("文件打开失败: {e}")))?;

    let mut resp = Response::new(Body::from_stream(ReaderStream::new(stream)));
    let headers = resp.headers_mut();
    headers.insert(CONTENT_TYPE, "application/octet-stream".parse().expect("常量头值恒合法"));
    // RFC 5987：非 ASCII 文件名用 filename*（源项目 URLUtil.encode 等价语义）
    headers.insert(
        CONTENT_DISPOSITION,
        format!("attachment; filename*=UTF-8''{}", urlencode(filename))
            .parse()
            .expect("Content-Disposition 恒合法"),
    );
    headers.insert(CONTENT_LENGTH, len.into());
    Ok(resp)
}

/// GET `/book-delete`：删除产物文件（对应源项目 `BookDeleteServlet`，成功返回 `data: null`）
pub async fn book_delete(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileParams>,
) -> Result<Json<ApiOk<()>>, ApiError> {
    let file = resolve_download_file(&state.config.download.download_path, &params.filename).await?;
    tokio::fs::remove_file(&file).await.map_err(|e| ApiError::internal(format!("文件删除失败: {e}")))?;
    Ok(Json(ApiOk::wrap(())))
}

/// 书源可用性（`/sources/check` 响应，与源项目 `SourceInfo` 字段一致；失败 delay/code 为 -1）
#[derive(Serialize)]
pub struct SourceStatus {
    pub id: u32,
    pub name: String,
    pub url: String,
    pub comment: String,
    pub disabled: bool,
    pub delay: i64,
    pub code: i64,
}

/// GET `/sources/check`：各书源可用性（并发 HEAD 探测，3s 超时，按完成顺序返回；
/// 对应源项目 `SourceUtils.getActivatedSourcesWithAvailabilityCheck`）
pub async fn sources_check(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiOk<Vec<SourceStatus>>>, ApiError> {
    if state.config.source.active_rules.is_empty() {
        return Err(ApiError::bad_request("未配置激活规则文件"));
    }
    let rules: Vec<_> = state.rules.rules().iter().filter(|r| !r.disabled).cloned().collect();
    if rules.is_empty() {
        return Ok(Json(ApiOk::wrap(Vec::new())));
    }

    let mut set: JoinSet<SourceStatus> = JoinSet::new();
    for rule in rules {
        let client = state.http.for_rule(rule.need_proxy).clone();
        let url = rule.url.clone();
        let base = SourceStatus {
            id: rule.id,
            name: rule.name.clone(),
            url: rule.url.clone(),
            comment: rule.comment.clone(),
            disabled: rule.disabled,
            delay: -1,
            code: -1,
        };
        set.spawn(async move {
            let start = std::time::Instant::now();
            match client
                .head(&url)
                .header("User-Agent", random_user_agent())
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                Ok(resp) => SourceStatus {
                    delay: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
                    code: i64::from(resp.status().as_u16()),
                    ..base
                },
                Err(_) => base,
            }
        });
    }

    // 与源项目 CompletionService 一致：按完成顺序收集（快者在前）
    let mut items = Vec::new();
    while let Some(item) = set.join_next().await {
        if let Ok(item) = item {
            items.push(item);
        }
    }
    Ok(Json(ApiOk::wrap(items)))
}

/// `/rules/update` 结果
#[derive(Serialize)]
pub struct RulesUpdateResult {
    /// 更新后的规则条数
    pub count: usize,
}

/// GET `/rules/update`：在线规则更新（设计文档 §5.1）。
///
/// 流程：拉取最新规则（`gh-proxy` 前缀加速可选）→ 校验 JSON 可解析且非空 →
/// 原子写回 `rules/main.json`（临时文件 + rename，失败不影响现有规则）→
/// `ArcSwap` 热重载生效，无需重启。下载 Job 运行中拒绝更新（409，避免规则中途变更）。
pub async fn rules_update(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiOk<RulesUpdateResult>>, ApiError> {
    // 下载 Job 运行中拒绝更新（规则中途变更会导致解析行为不一致）
    if state.jobs.active_count() > 0 {
        return Err(ApiError::conflict("下载任务运行中，请完成后再更新规则"));
    }
    // 持写锁防并发更新（等待而非拒绝：并发请求最终串行完成，语义简单）
    let _guard = state.rules.update_lock().write().await;

    // 拉取：gh_proxy 非空时前缀拼接（与源项目 getGhProxy + url 拼接一致）
    let url = if state.config.global.gh_proxy.is_empty() {
        state.config.source.rules_url.clone()
    } else {
        format!("{}{}", state.config.global.gh_proxy, state.config.source.rules_url)
    };
    let resp = state
        .http
        .direct()
        .get(&url)
        .header("User-Agent", random_user_agent())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| ApiError::bad_gateway(format!("规则拉取失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::bad_gateway(format!("规则拉取失败：上游返回 {}", resp.status().as_u16())));
    }
    let body = resp.text().await.map_err(|e| ApiError::bad_gateway(format!("规则响应读取失败: {e}")))?;

    // 校验：可解析且非空（失败不影响现有规则）
    let rules = RuleStore::parse_rules_from_str(&body)
        .map_err(|e| ApiError::bad_gateway(format!("规则内容校验失败: {e}")))?;
    if rules.is_empty() {
        return Err(ApiError::bad_gateway("规则内容为空，已取消更新"));
    }

    // 原子写回：临时文件 + rename
    let path = state.rules.active_rules_path();
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, &body).await.map_err(|e| ApiError::internal(format!("规则文件写入失败: {e}")))?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| ApiError::internal(format!("规则文件替换失败: {e}")))?;

    // 热重载
    let count = rules.len();
    state.rules.swap(rules);
    tracing::info!(count, "规则在线更新完成并已热重载");
    Ok(Json(ApiOk::wrap(RulesUpdateResult { count })))
}

/// trim 后非空才返回 `Some`
fn non_empty(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_encodes_chinese_and_keeps_unreserved() {
        assert_eq!(urlencode("斗破"), "%E6%96%97%E7%A0%B4");
        assert_eq!(urlencode("a-b_~"), "a-b_~");
    }

    #[test]
    fn non_empty_trims_and_filters_blank() {
        assert_eq!(non_empty(Some("  txt ")).as_deref(), Some("txt"));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }
}
