# so-novel-rs

English | [简体中文](./README.zh-CN.md)

A Rust rewrite of [so-novel](https://github.com/freeok/so-novel): a multi-source novel search and download web service.

Ships as a single self-contained binary with no JRE dependency, uses **≤ 15MB** idle memory (the original JVM-based project idles at 300MB+), and **shares exactly the same book-source rule files** as the original project.

## Features

- **Aggregated search**: queries all book sources concurrently, then merges and de-duplicates results, filters low-similarity matches, and provides suggestions (`/suggestion`)
- **Full download pipeline**: detail → table of contents (paged / reversed) → concurrent chapter fetching (rate-limit jitter + retry on failure + paged content stitching) → content sanitization → Simplified/Traditional Chinese conversion
- **Concurrent jobs**: configurable `max_jobs` (default 3), each job has its own rate limit and progress; returns 409 when the limit is exceeded
- **Real-time SSE progress**: one event per chapter, with terminal-state replay (a page refresh does not break the stream)
- **Output formats**: txt (optional GBK encoding) / epub (with cover and TOC)
- **Local bookshelf**: list, download, and delete downloaded files
- **Rule engine**: CSS selectors plus `@js:` (QuickJS) / `@java:` built-in operations — 100% compatible with the original project's `main.json`
- **Hot rule update**: `/rules/update` pulls the latest rules online (gh-proxy acceleration supported), validates them, writes atomically, and hot-reloads without a restart
- **Cloudflare bypass**: integrates with an external cf-bypass service (same as the original project)
- **Logging**: dual output to stderr and a daily-rotating file, with expired logs cleaned up on startup
- **Encoding detection**: automatic detection for GBK / GB18030 / Big5 sites

## Quick Start

### Option 1: Download a prebuilt binary

Grab the archive for your platform from [Releases](../../releases), extract it, and run:

```bash
./so-novel-rs          # so-novel-rs.exe on Windows
```

A default `config.toml` is generated on first startup. Open <http://127.0.0.1:7765/> in your browser to get started.

### Option 2: Build from source

Requires Rust 1.85+ (see [rust-toolchain.toml](rust-toolchain.toml)):

```bash
cargo build --release
```

At runtime it expects `rules/` (book-source rules) and `static/` (frontend pages) in the working directory; both are included in this repository.

## Configuration

`config.toml` (generated on first startup, changes take effect after a restart):

```toml
[download]
download_path = "downloads"     # output directory
extname = "epub"                # output format: txt | epub
txt_encoding = ""               # set to "GBK" for legacy devices (default UTF-8)
preserve_chapter_cache = false  # keep the chapter cache directory after download

[source]
language = ""                   # zh-CN | zh-TW | zh-Hant (empty = follow source site)
active_rules = "main.json"      # active rule file
search_limit = 30               # max search results per book source

[crawl]
max_jobs = 3                    # global max concurrent download jobs
concurrency = 50                # max chapter concurrency per job
min_interval = 200              # minimum request interval (ms)
max_interval = 400              # maximum request interval (ms)
max_retries = 3                 # retry count on failure

[web]
port = 7765

[global]
cf_bypass = ""                  # Cloudflare bypass service URL
gh_proxy = ""                   # GitHub acceleration proxy (for rule updates)
```

See [config.rs](src/config.rs) for the full list of options. Any option can be overridden with a `SN_`-prefixed environment variable (e.g. `SN_WEB_PORT=8080`).

## API

All JSON responses are wrapped uniformly as `{code, message, data}`.

| Path | Description |
| --- | --- |
| `GET /` | Web UI (static page) |
| `GET /config` | Runtime configuration (read-only) |
| `GET /search/aggregated?kw=` | Aggregated search |
| `GET /suggestion?kw=` | Search suggestions |
| `GET /book-fetch?url=&format=` | Create a download job, returns `{jobId}` (202) |
| `GET /download-progress?id=` | **SSE** download progress; the `done` event carries the output filename |
| `GET /local-books` | Local bookshelf list |
| `GET /book-download?filename=` | Download a produced file |
| `GET /book-delete?filename=` | Delete a produced file |
| `GET /sources` | Book source list |
| `GET /sources/check` | Book source availability probe |
| `GET /rules/update` | Update book-source rules online and hot-reload (409 while downloading) |

Examples:

```bash
# Search
curl "http://127.0.0.1:7765/search/aggregated?kw=Battle%20Through%20the%20Heavens"

# Create a download job (url comes from the search results)
curl "http://127.0.0.1:7765/book-fetch?url=https://example.com/book/1.html&format=epub"

# Subscribe to progress (curl -N disables buffering)
curl -N "http://127.0.0.1:7765/download-progress?id=<jobId>"
```

## Relationship with the original project

| Dimension | so-novel (Java) | so-novel-rs |
| --- | --- | --- |
| Interface | CLI / TUI / Web | Web only |
| Runtime | JRE + V8 engine pool | Single binary (embedded QuickJS, created and destroyed on demand) |
| Idle memory | 300MB+ | ≤ 15MB |
| Book-source rules | JSON | **Same JSON, fully compatible** |
| Output formats | txt / epub / html / pdf | txt / epub |
| Config format | config.ini | config.toml (fields map one-to-one) |

## Development

```bash
cargo test                      # 90 unit tests + 27 API contract tests
cargo clippy --all-targets      # zero warnings on stable/nightly
cargo fmt --all -- --check
```

## Disclaimer

This project is for study and exchange purposes only; do not use it commercially. Please support official releases and delete any downloaded content within 24 hours. Book-source rules come from the open-source community and are unrelated to the author of this project.
