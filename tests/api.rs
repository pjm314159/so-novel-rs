//! API 契约测试：起真实 axum 服务（tower oneshot，无需端口），覆盖每个 endpoint 的
//! 成功路径与参数校验失败（linter.md §5.3）。

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use so_novel_rs::config::AppConfig;
use so_novel_rs::rule::loader::RuleStore;
use so_novel_rs::web::{router, AppState};
use tower::ServiceExt;

fn test_state() -> Arc<AppState> {
    let config = AppConfig::default();
    // 测试工作目录为 crate 根，rules/ 与 static/ 均在
    let rules =
        RuleStore::load(Path::new("rules"), &config.source.active_rules).expect("测试前置：规则加载失败");
    let http = so_novel_rs::util::http::HttpClients::new(&config.proxy).expect("测试前置：客户端构建失败");
    Arc::new(AppState {
        config,
        rules,
        jobs: Arc::new(so_novel_rs::engine::JobRegistry::default()),
        http,
        static_dir: Path::new("static").to_path_buf(),
        shutdown: {
            // 发送端必须保活（forget 泄漏）：drop 会立刻关闭 SSE 流
            let (tx, rx) = tokio::sync::watch::channel(false);
            std::mem::forget(tx);
            rx
        },
    })
}

/// 离线规则状态：唯一书源指向本机不可达端口（受理成功但任务即刻失败，不触外网）。
fn offline_state() -> Arc<AppState> {
    // 多个测试并发调用本函数：临时规则文件只初始化一次。
    // 否则 `fs::write` 的 truncate 瞬间会被并发 load 读到空文件（CI 暴露的竞态）。
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let dir = std::env::temp_dir().join("sonovel-rs-api-test-rules");
    INIT.get_or_init(|| {
        std::fs::create_dir_all(&dir).expect("创建临时规则目录失败");
        std::fs::write(dir.join("offline.json"), r#"[{"url":"http://127.0.0.1:9/","name":"离线源"}]"#)
            .expect("写入临时规则失败");
    });
    let mut config = AppConfig::default();
    config.source.active_rules = "offline.json".into();
    // 在线规则更新指向本机不可达端口（/rules/update 拉取失败测试，不触外网）
    config.source.rules_url = "http://127.0.0.1:9/main.json".into();
    let rules = RuleStore::load(&dir, &config.source.active_rules).expect("测试前置：规则加载失败");
    let http = so_novel_rs::util::http::HttpClients::new(&config.proxy).expect("测试前置：客户端构建失败");
    Arc::new(AppState {
        config,
        rules,
        jobs: Arc::new(so_novel_rs::engine::JobRegistry::default()),
        http,
        static_dir: Path::new("static").to_path_buf(),
        shutdown: {
            // 发送端必须保活（forget 泄漏）：drop 会立刻关闭 SSE 流
            let (tx, rx) = tokio::sync::watch::channel(false);
            std::mem::forget(tx);
            rx
        },
    })
}

/// 指定下载目录的状态（`/local-books`、`/book-download`、`/book-delete` 测试用）
fn state_with_download_path(download_path: &Path) -> Arc<AppState> {
    let mut config = AppConfig::default();
    config.download.download_path = download_path.display().to_string();
    let rules =
        RuleStore::load(Path::new("rules"), &config.source.active_rules).expect("测试前置：规则加载失败");
    let http = so_novel_rs::util::http::HttpClients::new(&config.proxy).expect("测试前置：客户端构建失败");
    Arc::new(AppState {
        config,
        rules,
        jobs: Arc::new(so_novel_rs::engine::JobRegistry::default()),
        http,
        static_dir: Path::new("static").to_path_buf(),
        shutdown: {
            // 发送端必须保活（forget 泄漏）：drop 会立刻关闭 SSE 流
            let (tx, rx) = tokio::sync::watch::channel(false);
            std::mem::forget(tx);
            rx
        },
    })
}

async fn get_json(path: &str) -> (StatusCode, String) {
    let app = router(test_state());
    let resp =
        app.oneshot(Request::get(path).body(Body::empty()).expect("请求构造失败")).await.expect("请求失败");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    (status, body)
}

#[tokio::test]
async fn config_returns_full_config_json() {
    let (status, body) = get_json("/config").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"web\""), "应包含 web 段: {body}");
    assert!(body.contains("7765"), "应包含默认端口: {body}");
}

#[tokio::test]
async fn sources_returns_rule_list_with_ids() {
    let (status, body) = get_json("/sources").await;
    assert_eq!(status, StatusCode::OK);
    // main.json 首个书源为香书小说
    assert!(body.contains("香书小说"), "应包含书源名称: {body}");
    assert!(body.contains("\"id\":1"), "ID 从 1 开始: {body}");
}

#[tokio::test]
async fn static_index_served_at_root() {
    let (status, body) = get_json("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty(), "首页不应为空");
}

#[tokio::test]
async fn unknown_api_path_returns_404() {
    let (status, _) = get_json("/not-exist-api").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_without_kw_returns_400() {
    let (status, body) = get_json("/search").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("kw"), "错误信息应指明 kw 参数: {body}");
}

#[tokio::test]
async fn search_with_empty_kw_returns_400() {
    let (status, _) = get_json("/search?kw=%20%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn suggestion_with_empty_kw_returns_empty_array() {
    let (status, body) = get_json("/suggestion?kw=").await;
    assert_eq!(status, StatusCode::OK);
    // { code, message, data } 包装（前端 api.js 依赖）
    assert_eq!(body, r#"{"code":200,"message":"OK","data":[]}"#, "空关键字应返回空数组（不发外部请求）");
}

#[tokio::test]
async fn book_fetch_without_url_returns_400() {
    let (status, body) = get_json("/book-fetch").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("url"), "错误信息应指明 url 参数: {body}");
}

#[tokio::test]
async fn book_fetch_with_blank_url_returns_400() {
    let (status, _) = get_json("/book-fetch?url=%20%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn book_fetch_with_invalid_format_returns_400() {
    let (status, body) = get_json("/book-fetch?url=http://127.0.0.1:9/b&format=pdf").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("pdf"), "错误信息应包含格式名: {body}");
}

#[tokio::test]
async fn book_fetch_with_invalid_language_returns_400() {
    let (status, body) = get_json("/book-fetch?url=http://127.0.0.1:9/b&language=en").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("语言"), "错误信息应指明语言校验: {body}");
}

#[tokio::test]
async fn book_fetch_with_invalid_concurrency_returns_400() {
    let (status, body) = get_json("/book-fetch?url=http://127.0.0.1:9/b&concurrency=0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("并发"), "错误信息应指明并发校验: {body}");
}

#[tokio::test]
async fn book_fetch_with_unknown_source_url_returns_400() {
    let (status, body) = get_json("/book-fetch?url=http://unknown.invalid/book/1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("书源规则"), "错误信息应指明规则未匹配: {body}");
}

#[tokio::test]
async fn book_fetch_accepts_job_and_returns_job_id() {
    // 离线源：受理后后台任务对本机不可达端口即刻失败，不影响断言
    let app = router(offline_state());
    let resp = app
        .oneshot(
            Request::get("/book-fetch?url=http://127.0.0.1:9/book/1.html")
                .body(Body::empty())
                .expect("请求构造失败"),
        )
        .await
        .expect("请求失败");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.contains("jobId"), "受理响应应含 jobId: {body}");
}

#[tokio::test]
async fn book_fetch_returns_409_when_max_jobs_reached() {
    let state = test_state(); // 默认 max_jobs = 3
    for i in 0..state.config.crawl.max_jobs {
        state.jobs.create(format!("occupied{i}"));
    }
    // 规则 URL 取首个书源（仅做前缀匹配，槽位占满后不会真正发起下载）
    let rule_url = state.rules.rules()[0].url.clone();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/book-fetch?url={rule_url}")).body(Body::empty()).expect("请求构造失败"),
        )
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains("上限"), "409 信息应说明任务数上限: {body}");
}

#[tokio::test]
async fn download_progress_replays_terminal_state_with_compat_fields() {
    let state = test_state();
    state.jobs.create("job1".into());
    state.jobs.update("job1", |s| {
        s.total = 10;
        s.done = 10;
        s.phase = so_novel_rs::engine::Phase::Done;
        s.filename = Some("斗破苍穹 (天蚕土豆) EPUB".into());
    });
    let app = router(state);
    let resp = app
        .oneshot(Request::get("/download-progress?id=job1").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type =
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    assert!(content_type.starts_with("text/event-stream"), "应为 SSE 流: {content_type}");
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains("retry: 10000"), "握手应含重连间隔: {body}");
    assert!(body.contains(": connected"), "握手应含 connected 注释: {body}");
    assert!(body.contains("event: done"), "终态应推送 done 事件: {body}");
    // 旧前端兼容字段
    assert!(body.contains(r#""type":"download-progress""#), "载荷应含 type: {body}");
    assert!(body.contains(r#""index":10"#), "载荷应含 index: {body}");
    assert!(body.contains("斗破苍穹"), "终态载荷应含文件名: {body}");
}

#[tokio::test]
async fn download_progress_without_jobs_keeps_connection_pending() {
    let state = test_state();
    let app = router(state);
    let resp = app
        .oneshot(Request::get("/download-progress").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    // 无任务时流必须挂起（keep-alive 保活）而非立即结束：
    // 立即结束会触发浏览器 EventSource 自动重连，造成"连接异常/成功"循环提示
    let result =
        tokio::time::timeout(std::time::Duration::from_millis(500), resp.into_body().collect()).await;
    assert!(result.is_err(), "无任务时 SSE 流不应在短窗口内结束，应保持挂起");
}

// ---------- /local-books、/book-download、/book-delete、/sources/check（M4） ----------

/// 建临时下载目录并写入一个产物文件，返回 (目录, 文件名, 内容)。
/// `tag` 用于测试间隔离（并发执行时互不清理对方的目录）。
fn temp_download_dir(tag: &str) -> (std::path::PathBuf, &'static str, &'static [u8]) {
    let dir = std::env::temp_dir().join(format!("sonovel-rs-api-download-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时下载目录失败");
    let (name, content): (&'static str, &'static [u8]) = ("斗破苍穹(天蚕土豆).txt", b"PK\x03\x04");
    std::fs::write(dir.join(name), content).expect("写入产物失败");
    // 目录不应出现在列表中
    std::fs::create_dir_all(dir.join("章节缓存")).expect("创建子目录失败");
    (dir, name, content)
}

#[tokio::test]
async fn local_books_lists_files_with_envelope() {
    let (dir, name, content) = temp_download_dir("list");
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(Request::get("/local-books").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains(r#""code":200,"message":"OK""#), "应有成功包装: {body}");
    assert!(body.contains(name), "应包含产物文件名: {body}");
    assert!(body.contains(&format!(r#""size":{}"#, content.len())), "应包含文件大小: {body}");
    assert!(body.contains(r#""timestamp":"#), "应包含时间戳: {body}");
    assert!(!body.contains("章节缓存"), "目录不应出现在列表中: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn local_books_missing_dir_returns_empty_data() {
    let dir = std::env::temp_dir().join("sonovel-rs-api-download-absent");
    let _ = std::fs::remove_dir_all(&dir);
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(Request::get("/local-books").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert_eq!(body, r#"{"code":200,"message":"OK","data":[]}"#);
}

#[tokio::test]
async fn book_download_streams_file_with_headers() {
    let (dir, name, content) = temp_download_dir("dl");
    let app = router(state_with_download_path(&dir));
    let path = format!("/book-download?filename={}", urlencode(name));
    let resp =
        app.oneshot(Request::get(path).body(Body::empty()).expect("请求构造失败")).await.expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers();
    assert_eq!(headers.get("content-type").and_then(|v| v.to_str().ok()), Some("application/octet-stream"));
    let disposition =
        headers.get("content-disposition").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    assert!(disposition.contains("attachment"), "应为附件下载: {disposition}");
    let encoded_name = urlencode(name);
    assert!(
        disposition.contains(&format!("filename*=UTF-8''{encoded_name}")),
        "文件名应百分号编码: {disposition}"
    );
    let expected_len = content.len().to_string();
    assert_eq!(headers.get("content-length").and_then(|v| v.to_str().ok()), Some(expected_len.as_str()));
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    assert_eq!(bytes.as_ref(), content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn book_download_rejects_path_traversal() {
    let (dir, _, _) = temp_download_dir("traversal");
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(
            Request::get("/book-download?filename=..%2Fsecret.txt")
                .body(Body::empty())
                .expect("请求构造失败"),
        )
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains("非法路径"), "403 应说明非法路径: {body}");
    assert!(body.contains(r#""data":null"#), "错误分支 data 恒为 null: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn book_download_missing_file_returns_404() {
    let (dir, _, _) = temp_download_dir("miss-dl");
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(
            Request::get("/book-download?filename=no-such.txt").body(Body::empty()).expect("请求构造失败"),
        )
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn book_delete_removes_file_and_returns_null_data() {
    let (dir, name, _) = temp_download_dir("del");
    let app = router(state_with_download_path(&dir));
    let path = format!("/book-delete?filename={}", urlencode(name));
    let resp =
        app.oneshot(Request::get(path).body(Body::empty()).expect("请求构造失败")).await.expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert_eq!(body, r#"{"code":200,"message":"OK","data":null}"#);
    assert!(!dir.join(name).exists(), "文件应已删除");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn book_delete_missing_file_returns_404() {
    let (dir, _, _) = temp_download_dir("miss-del");
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(Request::get("/book-delete?filename=no-such.txt").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn book_delete_without_filename_returns_400() {
    let (dir, _, _) = temp_download_dir("del-400");
    let app = router(state_with_download_path(&dir));
    let resp = app
        .oneshot(Request::get("/book-delete").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rules_update_returns_409_when_job_running() {
    // 下载 Job 运行中拒绝更新（规则中途变更会导致解析行为不一致）
    let state = test_state();
    state.jobs.create("running-job".into());
    let app = router(state);
    let resp = app
        .oneshot(Request::get("/rules/update").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains("下载任务"), "409 信息应说明任务运行中: {body}");
}

#[tokio::test]
async fn rules_update_fetch_failure_returns_502_and_keeps_rules() {
    // 离线状态：规则拉取不可达 → 502，且现有规则不受影响（不触外网）
    let state = offline_state();
    let before = state.rules.rules().len();
    let app = router(state.clone());
    let resp = app
        .oneshot(Request::get("/rules/update").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains("规则拉取失败"), "错误信息应说明拉取失败: {body}");
    // 失败不影响现有规则（无热重载）
    assert_eq!(state.rules.rules().len(), before);
}

#[tokio::test]
async fn sources_check_offline_rule_reports_unavailable() {
    // 离线源：HEAD 对本机不可达端口立即失败 → delay/code = -1（不触外网）
    let app = router(offline_state());
    let resp = app
        .oneshot(Request::get("/sources/check").body(Body::empty()).expect("请求构造失败"))
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("读取响应体失败").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("响应非 UTF-8");
    assert!(body.contains(r#""code":200,"message":"OK""#), "应有成功包装: {body}");
    assert!(body.contains("离线源"), "应包含书源名称: {body}");
    assert!(body.contains(r#""delay":-1"#), "不可达书源 delay 应为 -1: {body}");
    assert!(body.contains(r#""code":-1"#), "不可达书源 code 应为 -1: {body}");
}

/// 测试侧百分号编码（与 handler 的 urlencode 语义一致）
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
