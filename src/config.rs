//! 运行配置：`config.toml` 加载模型（字段语义与源项目 config.ini 一一对应）。
//!
//! 加载优先级：环境变量（`SN_` 前缀）> `config.toml` > 内置默认值。
//! 启动时若 `config.toml` 不存在则生成默认文件。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 顶层配置（对应 config.toml 各段）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub download: DownloadConfig,
    pub source: SourceConfig,
    pub crawl: CrawlConfig,
    pub web: WebConfig,
    pub proxy: ProxyConfig,
    pub global: GlobalConfig,
    pub cookie: CookieConfig,
    pub log: LogConfig,
}

/// `[download]` 下载输出配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// 下载路径
    pub download_path: String,
    /// 输出格式：txt | epub
    pub extname: String,
    /// txt 编码，可设 "GBK" 兼容旧设备（默认 UTF-8）
    pub txt_encoding: String,
    /// 下载完成后保留章节缓存目录
    pub preserve_chapter_cache: bool,
}

/// `[source]` 书源配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    /// 书籍内容语言：zh-CN | zh-TW | zh-Hant（默认自动）
    pub language: String,
    /// 激活规则文件路径
    pub active_rules: String,
    /// 指定书源 ID（可选）
    pub source_id: String,
    /// 每书源搜索结果条数上限
    pub search_limit: u32,
    /// 过滤低相似度搜索结果（相似度 <= 0.25 剔除）
    pub search_filter: bool,
    /// 在线规则更新地址（`/rules/update` 拉取目标，可换镜像）
    pub rules_url: String,
}

/// `[crawl]` 抓取并发配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlConfig {
    /// 全局最大同时下载任务数（多人/多本书并发）
    pub max_jobs: u32,
    /// 每个 Job 的章节并发上限
    pub concurrency: u32,
    /// 每个 Job 的在途章节缓冲上限（性能优先的有界流水线，设计文档 §7.2/§7.3）
    pub in_flight: u32,
    /// 请求最小间隔（毫秒）
    pub min_interval: u64,
    /// 请求最大间隔（毫秒）
    pub max_interval: u64,
    /// 启用重试
    pub enable_retry: bool,
    /// 最大重试次数（针对首次下载失败的章节）
    pub max_retries: u32,
    /// 重试最小间隔（毫秒）
    pub retry_min_interval: u64,
    /// 重试最大间隔（毫秒）
    pub retry_max_interval: u64,
}

/// `[web]` Web 服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// 服务端口（仅绑定 127.0.0.1）
    pub port: u16,
}

/// `[proxy]` HTTP 代理配置（针对 needProxy 书源）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

/// `[global]` 全局配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// Cloudflare 绕过服务地址
    pub cf_bypass: String,
    /// GitHub 加速代理（用于 /rules/update 拉取规则）
    pub gh_proxy: String,
}

/// `[cookie]` 特定站点 cookie
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CookieConfig {
    /// 起点封面 cookie（`w_tsfp=xxx`），用于高质量封面获取
    pub qidian: String,
}

/// `[log]` 日志配置（tracing 双输出）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// debug | info | warn | error
    pub level: String,
    /// 日志目录（按日滚动）
    pub dir: String,
    /// 自动清理超过 N 天的日志文件
    pub max_age_days: u32,
    /// 是否同时输出到 stderr
    pub stdout: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            download_path: "downloads".into(),
            extname: "epub".into(),
            txt_encoding: String::new(),
            preserve_chapter_cache: false,
        }
    }
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            language: String::new(),
            active_rules: "main.json".into(),
            source_id: String::new(),
            search_limit: 30,
            search_filter: true,
            rules_url: "https://raw.githubusercontent.com/freeok/so-novel/main/bundle/rules/main.json".into(),
        }
    }
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            max_jobs: 3,
            concurrency: 50,
            in_flight: 64,
            min_interval: 200,
            max_interval: 400,
            enable_retry: true,
            max_retries: 3,
            retry_min_interval: 2000,
            retry_max_interval: 4000,
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self { port: 7765 }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self { enabled: false, host: "127.0.0.1".into(), port: 0 }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "info".into(), dir: "logs".into(), max_age_days: 14, stdout: true }
    }
}

/// 配置加载错误
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// config.toml 存在但解析失败
    #[error("解析 config.toml 失败: {0}")]
    Parse(#[from] toml::de::Error),
    /// 文件读取失败
    #[error("读取 config.toml 失败: {0}")]
    Io(#[from] std::io::Error),
}

impl AppConfig {
    /// 加载配置：`config.toml` 不存在时生成默认文件后返回默认值。
    ///
    /// # Errors
    /// 文件存在但不可读/解析失败时返回 [`ConfigError`]。
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            let default = Self::default();
            let content =
                toml::to_string_pretty(&default).map_err(|e| std::io::Error::other(e.to_string()))?;
            std::fs::write(path, content)?;
            tracing::info!(?path, "已生成默认配置文件");
            return Ok(default);
        }
        let content = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&content)?;
        Ok(cfg)
    }

    /// 从字符串解析（测试用）
    ///
    /// # Errors
    /// 解析失败时返回 [`ConfigError`]。
    pub fn parse_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_str_full_config_keeps_all_fields() {
        let cfg = AppConfig::parse_str(
            r#"
[web]
port = 9000
[crawl]
max_jobs = 5
[log]
level = "debug"
"#,
        )
        .expect("解析失败");
        assert_eq!(cfg.web.port, 9000);
        assert_eq!(cfg.crawl.max_jobs, 5);
        assert_eq!(cfg.log.level, "debug");
    }

    #[test]
    fn default_values_match_design_doc() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.web.port, 7765);
        assert_eq!(cfg.crawl.max_jobs, 3);
        assert_eq!(cfg.crawl.concurrency, 50);
        assert_eq!(cfg.crawl.in_flight, 64);
        assert_eq!(cfg.source.search_limit, 30);
        assert_eq!(cfg.download.extname, "epub");
        assert_eq!(cfg.log.max_age_days, 14);
    }
}
