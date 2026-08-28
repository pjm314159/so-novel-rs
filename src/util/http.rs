//! HTTP 客户端：直连/代理双客户端 + 页面抓取（UA/referer/cookie/编码探测/大小上限）。

use std::time::Duration;

use rand::Rng;

use super::charset::decode_bytes;
use crate::config::ProxyConfig;

/// HTTP 抓取错误
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// 网络/请求失败
    #[error("请求失败: {0}")]
    Request(#[from] reqwest::Error),
    /// 响应体超过大小上限（防恶意书源，见设计文档 §7.3）
    #[error("响应体超过 10MB 上限")]
    TooLarge,
}

/// 直连 + 代理双客户端（对应源项目 `HttpClientContext`，按规则 `need_proxy` 选择）。
#[derive(Debug, Clone)]
pub struct HttpClients {
    direct: reqwest::Client,
    proxy: Option<reqwest::Client>,
}

impl HttpClients {
    /// 按代理配置构建（代理未启用时仅直连客户端）。
    ///
    /// # Errors
    /// 代理客户端构建失败时返回 [`reqwest::Error`]。
    pub fn new(proxy_cfg: &ProxyConfig) -> Result<Self, reqwest::Error> {
        let direct = build_client(None)?;
        let proxy = if proxy_cfg.enabled && proxy_cfg.port != 0 {
            let addr = format!("http://{}:{}", proxy_cfg.host, proxy_cfg.port);
            Some(build_client(Some(&addr))?)
        } else {
            None
        };
        Ok(Self { direct, proxy })
    }

    /// 按规则是否需要代理选择客户端（无代理客户端可用时回退直连）。
    pub fn for_rule(&self, need_proxy: bool) -> &reqwest::Client {
        if need_proxy {
            self.proxy.as_ref().unwrap_or(&self.direct)
        } else {
            &self.direct
        }
    }

    /// 直连客户端（Suggestion 转发等外部 API 用）。
    pub fn direct(&self) -> &reqwest::Client {
        &self.direct
    }
}

/// 构建客户端：UA 池由请求级注入；连接池随并发放开（见设计文档 §7.2）。
fn build_client(proxy: Option<&str>) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder().tcp_nodelay(true).pool_idle_timeout(Duration::from_secs(90));
    if let Some(p) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(p)?);
    }
    builder.build()
}

/// 单次页面抓取的请求描述。
#[derive(Debug, Default)]
pub struct PageRequest<'a> {
    /// POST form 表单（`application/x-www-form-urlencoded`）；None 则 GET
    pub form: Option<Vec<(String, String)>>,
    /// 附加 cookie 头
    pub cookies: Option<&'a str>,
    /// 超时（秒；规则未配置时由调用方给默认值）
    pub timeout_secs: u64,
    /// referer（源项目取 <scheme://authority>）
    pub referer: Option<&'a str>,
}

/// 抓取页面并解码为字符串（不检查 HTTP 状态码——与源项目一致，错误页交给选择器解析出空结果）。
///
/// # Errors
/// 网络失败返回 [`HttpError::Request`]；响应体超 10MB 返回 [`HttpError::TooLarge`]。
pub async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
    req: &PageRequest<'_>,
) -> Result<String, HttpError> {
    let mut builder = client.get(url);
    if let Some(form) = &req.form {
        builder = client.post(url).form(form);
    }
    if let Some(cookies) = req.cookies {
        builder = builder.header("Cookie", cookies);
    }
    if let Some(referer) = req.referer {
        builder = builder.header("Referer", referer);
    }
    let resp = builder
        .header("User-Agent", random_user_agent())
        .timeout(Duration::from_secs(req.timeout_secs))
        .send()
        .await?;

    let content_type =
        resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let bytes = resp.bytes().await?;
    if bytes.len() > 10 * 1024 * 1024 {
        return Err(HttpError::TooLarge);
    }
    Ok(decode_bytes(&bytes, content_type.as_deref()))
}

/// 从 URL 提取 `scheme://authority`（源项目搜索 referer 的计算方式）。
pub fn origin_of(url: &str) -> Option<String> {
    let idx = url.find("://")?;
    let after = &url[idx + 3..];
    let end = after.find('/').unwrap_or(after.len());
    Some(url[..idx + 3 + end].to_owned())
}

/// 随机 UA 池（对应源项目 `RandomUA.generate`）。
pub fn random_user_agent() -> String {
    const OS: &[&str] = &[
        "Windows NT 6.1; Win64; x64",
        "Windows NT 10.0; Win64; x64",
        "Macintosh; Intel Mac OS X 10_15_7",
        "X11; Linux x86_64",
        "X11; Ubuntu; Linux x86_64",
    ];
    const BROWSERS: &[&str] = &["Chrome", "Firefox", "Safari", "Edge"];

    let mut rng = rand::rng();
    let os = OS[rng.random_range(0..OS.len())];
    let browser = BROWSERS[rng.random_range(0..BROWSERS.len())];
    let major = rng.random_range(86..=145);
    let build = rng.random_range(0..1000);
    match browser {
        "Firefox" => format!("Mozilla/5.0 ({os}; rv:{major}.0) Gecko/20100101 Firefox/{major}.0"),
        "Safari" => format!(
            "Mozilla/5.0 ({os}) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/{major}.0 Safari/605.1.15"
        ),
        "Edge" => format!(
            "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.{build}.0 Safari/537.36 Edg/{major}.0"
        ),
        _ => format!(
            "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.{build}.0 Safari/537.36"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_extracts_scheme_authority() {
        assert_eq!(
            origin_of("http://www.xbiqugu.la/modules/article/search.php"),
            Some("http://www.xbiqugu.la".into())
        );
        assert_eq!(origin_of("https://a.b/"), Some("https://a.b".into()));
        assert_eq!(origin_of("not-a-url"), None);
    }

    #[test]
    fn random_user_agent_has_common_prefix() {
        let ua = random_user_agent();
        assert!(ua.starts_with("Mozilla/5.0 ("), "UA 应为标准格式: {ua}");
    }
}
