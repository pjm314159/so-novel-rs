//! so-novel-rs：多书源小说下载 Web 服务（so-novel 的 Rust 重写，仅 Web 形态）。

use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use so_novel_rs::config::AppConfig;
use so_novel_rs::engine::JobRegistry;
use so_novel_rs::rule::loader::RuleStore;
use so_novel_rs::web::{self, AppState};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// 进程级错误（仅不可恢复错误才退出进程，见 linter.md §4.1）
type BoxError = Box<dyn Error>;

fn main() -> Result<(), BoxError> {
    // 配置先行（日志配置来自 config.toml）
    let config = AppConfig::load_or_default(Path::new("config.toml"))?;
    let port = config.web.port;

    // 日志：双输出 —— stderr（人类可读）+ 本地文件按日滚动（设计文档 §6）
    // 优先级：RUST_LOG 环境变量 > config.toml [log].level
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.log.level))
        .or_else(|_| EnvFilter::try_new("info"))
        .map_err(|e| -> BoxError { format!("日志过滤器初始化失败: {e}").into() })?;
    let file_appender = tracing_appender::rolling::daily(&config.log.dir, "so-novel-rs");
    // guard 必须存活到进程退出，否则日志可能丢失（drop 时 flush）
    let (file_writer, _log_guard) = tracing_appender::non_blocking(file_appender);
    // NonBlocking 可 Clone（内部为 Arc 句柄）；两个分支各自构建 layer 避免泛型推断冲突
    if config.log.stdout {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false))
            .with(tracing_subscriber::fmt::layer().with_writer(file_writer))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(file_writer))
            .init();
    }

    // 启动时清理过期日志（无周期任务，见设计文档 §4 并发模型）
    if config.log.max_age_days > 0 {
        let removed = cleanup_old_logs(&config.log.dir, config.log.max_age_days)?;
        if removed > 0 {
            tracing::info!(removed, days = config.log.max_age_days, "已清理过期日志文件");
        }
    }

    // 规则：rules/{active_rules}（缺失则报错退出——规则是核心依赖）
    let rules = RuleStore::load(Path::new("rules"), &config.source.active_rules)?;

    // HTTP 客户端：直连 + 可选代理
    let http = so_novel_rs::util::http::HttpClients::new(&config.proxy)
        .map_err(|e| -> BoxError { format!("HTTP 客户端构建失败: {e}").into() })?;

    let state = Arc::new(AppState {
        config,
        rules,
        jobs: Arc::new(JobRegistry::default()),
        http,
        static_dir: Path::new("static").to_path_buf(),
    });

    // 单进程单端口，仅绑定本机（单人服务）
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let app = web::router(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| -> BoxError { format!("tokio 运行时创建失败: {e}").into() })?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| -> BoxError { format!("端口 {port} 绑定失败: {e}").into() })?;
        tracing::info!(%addr, "so-novel-rs 启动完成，浏览器访问 http://{addr}/");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| -> BoxError { format!("HTTP 服务异常退出: {e}").into() })
    })
}

/// Ctrl-C 优雅停机（linter.md §5.5：停止接收新请求，完成进行中的响应）
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("收到停机信号，正在优雅退出…");
}

/// 清理超过 N 天的日志文件（按修改时间判定，启动时执行一次）。
///
/// # Errors
/// 日志目录无法读取时返回错误（不中断启动：仅记录告警由调用方决定）。
fn cleanup_old_logs(dir: &str, max_age_days: u32) -> Result<usize, BoxError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // 目录尚不存在：appender 首次写入时创建
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("日志目录读取失败: {e}").into()),
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(max_age_days) * 24 * 3600))
        .ok_or_else(|| -> BoxError { "max_age_days 溢出".into() })?;
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let expired =
            entry.metadata().ok().and_then(|m| m.modified().ok()).is_some_and(|modified| modified < cutoff);
        if expired {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %e, "过期日志删除失败");
            } else {
                removed += 1;
            }
        }
    }
    Ok(removed)
}
