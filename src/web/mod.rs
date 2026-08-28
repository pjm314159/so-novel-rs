//! Web 层：axum `Router` 组装与共享状态。
//!
//! 路由表与源项目 `WebServer` 注册的 Servlet 一一对应（见设计文档 §5.4）：
//! `/config`、`/sources(/check)`、`/search(/aggregated)`、`/suggestion`、
//! `/book-fetch`、`/download-progress`(SSE)、`/local-books`、`/book-download`、
//! `/book-delete`、静态页。

mod handlers;
mod sse;

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tower_http::services::ServeDir;

use crate::config::AppConfig;
use crate::engine::JobRegistry;
use crate::rule::loader::RuleStore;
use crate::util::http::HttpClients;

/// 全局共享状态
#[derive(Debug)]
pub struct AppState {
    pub config: AppConfig,
    pub rules: RuleStore,
    /// 下载任务注册表（Job task 持 Arc 副本）
    pub jobs: Arc<JobRegistry>,
    /// 直连/代理 HTTP 客户端
    pub http: HttpClients,
    /// 静态资源目录（static/）
    pub static_dir: PathBuf,
}

/// 构建 Router（所有 handler 挂载于此）
pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = state.static_dir.clone();
    Router::new()
        .route("/config", get(handlers::get_config))
        .route("/sources", get(handlers::list_sources))
        .route("/sources/check", get(handlers::sources_check))
        .route("/search", get(handlers::search))
        .route("/search/aggregated", get(handlers::search))
        .route("/suggestion", get(handlers::suggestion))
        .route("/book-fetch", get(handlers::book_fetch))
        .route("/download-progress", get(sse::download_progress))
        .route("/local-books", get(handlers::local_books))
        .route("/book-download", get(handlers::book_download))
        .route("/book-delete", get(handlers::book_delete))
        .route("/rules/update", get(handlers::rules_update))
        .with_state(state)
        // 前端静态资源（复用源项目 static/，下载进度已按 jobId 订阅 SSE）
        .fallback_service(ServeDir::new(static_dir))
}
