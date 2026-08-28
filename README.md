# so-novel-rs

[so-novel](https://github.com/freeok/so-novel) 的 Rust 重写版：多书源小说搜索与下载 Web 服务。

单文件二进制、无 JRE 依赖、空闲内存 ≤ 15MB（源项目 JVM 常驻 300MB+），与源项目**完全共用同一套书源规则文件**。

## 特性

- **聚合搜索**：并发查询全部书源，合并去重、低相似度过滤、搜索建议（`/suggestion`）
- **完整下载流水线**：详情 → 目录（分页/倒序）→ 章节并发抓取（限速抖动 + 失败重试 + 分页正文拼接）→ 正文净化 → 简繁转换
- **多任务并发**：`max_jobs` 可配置（默认 3），每 Job 独立限速与进度，超限返回 409
- **SSE 实时进度**：每章推送一次，支持终态回放（刷新页面不断流）
- **输出格式**：txt（可选 GBK 编码）/ epub（含封面与目录）
- **本地书架**：已下载列表、文件下载、删除
- **规则引擎**：CSS 选择器 + `@js:`（QuickJS）/ `@java:` 内建操作，与源项目 main.json 100% 兼容
- **规则热更新**：`/rules/update` 在线拉取最新规则（支持 gh-proxy 加速），校验 + 原子写 + 无重启热重载
- **Cloudflare 绕过**：对接外部 cf-bypass 服务（与源项目一致）
- **日志**：stderr + 按日滚动文件双输出，启动时自动清理过期日志
- **编码探测**：GBK / GB18030 / Big5 书站自动识别

## 快速开始

### 方式一：下载预编译二进制

从 [Releases](../../releases) 下载对应平台的压缩包，解压后运行：

```bash
./so-novel-rs          # Windows 为 so-novel-rs.exe
```

首次启动自动生成默认 `config.toml`，浏览器访问 <http://127.0.0.1:7765/> 即可使用。

### 方式二：从源码构建

需要 Rust 1.85+（见 [rust-toolchain.toml](rust-toolchain.toml)）：

```bash
cargo build --release
./target/release/so-novel-rs
```

运行时依赖工作目录下的 `rules/`（书源规则）与 `static/`（前端页面），仓库已自带。

## 配置

`config.toml`（首次启动自动生成，修改后重启生效）：

```toml
[download]
download_path = "downloads"     # 下载路径
extname = "epub"                # 输出格式：txt | epub
txt_encoding = ""               # 可设 "GBK" 兼容旧设备（默认 UTF-8）
preserve_chapter_cache = false  # 下载完成后保留章节缓存目录

[source]
language = ""                   # zh-CN | zh-TW | zh-Hant（空 = 跟随源站）
active_rules = "main.json"      # 激活规则文件
search_limit = 30               # 每书源搜索结果条数上限

[crawl]
max_jobs = 3                    # 全局最大同时下载任务数
concurrency = 50                # 每个 Job 的章节并发上限
min_interval = 200              # 请求最小间隔 (ms)
max_interval = 400              # 请求最大间隔 (ms)
max_retries = 3                 # 失败重试次数

[web]
port = 7765

[global]
cf_bypass = ""                  # Cloudflare 绕过服务地址
gh_proxy = ""                   # GitHub 加速代理（规则更新用）
```

完整配置项见 [config.rs](src/config.rs)。环境变量 `SN_` 前缀可覆盖任意配置（如 `SN_WEB_PORT=8080`）。

## API

所有 JSON 响应统一 `{code, message, data}` 包装。

| 路径 | 说明 |
| --- | --- |
| `GET /` | Web UI（静态页） |
| `GET /config` | 运行配置（只读） |
| `GET /search/aggregated?kw=` | 聚合搜索 |
| `GET /suggestion?kw=` | 搜索建议词 |
| `GET /book-fetch?url=&format=` | 创建下载任务，返回 `{jobId}`（202） |
| `GET /download-progress?id=` | **SSE** 下载进度，`done` 事件携带产物文件名 |
| `GET /local-books` | 本地书架列表 |
| `GET /book-download?filename=` | 下载产物文件 |
| `GET /book-delete?filename=` | 删除产物文件 |
| `GET /sources` | 书源列表 |
| `GET /sources/check` | 书源可用性探测 |
| `GET /rules/update` | 在线更新书源规则并热重载（下载中返回 409） |

示例：

```bash
# 搜索
curl "http://127.0.0.1:7765/search/aggregated?kw=斗破苍穹"

# 创建下载任务（url 来自搜索结果）
curl "http://127.0.0.1:7765/book-fetch?url=https://example.com/book/1.html&format=epub"

# 订阅进度（curl -N 不缓冲）
curl -N "http://127.0.0.1:7765/download-progress?id=<jobId>"
```

## 与源项目的关系

| 维度 | so-novel（Java） | so-novel-rs |
| --- | --- | --- |
| 形态 | CLI / TUI / Web | 仅 Web |
| 运行时 | JRE + V8 引擎池 | 单文件二进制（QuickJS 内嵌，按需创建销毁） |
| 空闲内存 | 300MB+ | ≤ 15MB |
| 书源规则 | JSON | **同一套 JSON，完全兼容** |
| 输出格式 | txt / epub / html / pdf | txt / epub |
| 配置格式 | config.ini | config.toml（字段语义一一对应） |

明确裁剪：CLI/TUI、自动更新、匿名上报、捐赠页、html/pdf 输出。

## 开发

```bash
cargo test            # 90 单元测试 + 27 API 契约测试
cargo clippy --all-targets   # stable/nightly 零警告
cargo fmt --all -- --check
```

架构与设计决策见 [docs/so-novel-rust-rewrite-design.md](../docs/so-novel-rust-rewrite-design.md)（上级仓库目录）。

## 发布

推送 `v*` 标签触发自动发布：

```bash
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions 自动构建 Windows / Linux / macOS（x64 + ARM64）产物，打包书源规则与前端资源，校验和后发布到 Releases。

## 免责声明

本项目仅供学习交流，请勿用于商业用途；请支持正版，下载内容请在 24 小时内删除。书源规则来自开源社区，与本项目作者无关。
