//! 合并输出（对应源项目 `CrawlerPostHandler` + `TxtMergeHandler` / `EpubMergeHandler`）。
//!
//! 章节缓存目录 → 单文件产物：txt（按 `txt_encoding` 编码 + 书籍信息头）/
//! epub（epub-builder：元数据 + 封面 + toc）。完成后按配置删除章节缓存目录。
//!
//! 正文 IO（读章节/写产物）在 `spawn_blocking` 内同步执行，封面经 HTTP 预先异步下载。

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use epub_builder::{EpubBuilder, EpubContent, ReferenceType, ZipLibrary};

use super::{Book, BoxError};
use crate::config::AppConfig;

/// 封面页 XHTML（源项目 `templates/chapter_cover.html`，epub4j 产物的对应物）
const COVER_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" lang="zh">
<head>
  <title>封面</title>
  <style type="text/css">
    body {
      margin: 0;
      display: flex;
      align-items: center;
      justify-content: center;
      height: 100vh;
      text-align: center;
    }

    img {
      max-width: 100%;
      max-height: 100%;
    }
  </style>
</head>
<body>
<div>
  <img src="cover.jpg" alt="封面图片"/>
</div>
</body>
</html>"#;

/// 合并并收尾：生成产物文件 + 下载封面（存于章节缓存目录）+ 按配置清缓存。
///
/// `返回产物文件名（download_path` 下的相对名，SSE `done` 事件与 `/book-download` 用）。
///
/// # Errors
/// txt/epub 生成失败时返回错误（缓存目录保留，便于排查重试）。
pub async fn merge_and_finalize(
    config: &AppConfig,
    client: &reqwest::Client,
    book: &Book,
    save_dir: &Path,
) -> Result<String, BoxError> {
    // 封面下载失败不中断（与源项目一致，仅告警）
    let cover = download_cover(client, book.cover_url.as_str()).await;

    let ext = config.download.extname.clone();
    let output_name = format!("{}({}).{}", book.book_name, book.author, ext);
    let output_path = Path::new(&config.download.download_path).join(&output_name);
    let book = book.clone();
    let txt_encoding = config.download.txt_encoding.clone();
    let save_dir = save_dir.to_path_buf();
    let save_dir_for_cleanup = save_dir.clone();

    // spawn_blocking 要求 'static：闭包内数据全部 owned
    let _ = tokio::task::spawn_blocking(move || match ext.as_str() {
        "txt" => merge_txt_sync(&output_path, &book, &save_dir, &txt_encoding),
        "epub" => merge_epub_sync(&output_path, &book, &save_dir, cover.as_deref()),
        other => Err(format!("暂不支持的下载格式: {other}").into()),
    })
    .await
    .map_err(|e| -> BoxError { format!("合并任务 join 失败: {e}").into() })??;

    if !config.download.preserve_chapter_cache {
        if let Err(e) = tokio::fs::remove_dir_all(&save_dir_for_cleanup).await {
            tracing::warn!(error = %e, "章节缓存目录删除失败");
        }
    }
    Ok(output_name)
}

/// 章节文件按文件名前缀序号排序（对应源项目 `FileUtils.sortFilesByName`）。
fn sorted_chapter_files(save_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(save_dir) else {
        return Vec::new();
    };
    let mut files: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| {
            // `{index}_{title}.{ext}`：取首个 _ 前的数字序号
            let stem = p.file_stem()?.to_string_lossy();
            let order = stem.split_once('_').and_then(|(num, _)| num.parse::<u64>().ok()).unwrap_or(u64::MAX);
            Some((order, p))
        })
        .collect();
    files.sort_by_key(|(order, _)| *order);
    files.into_iter().map(|(_, p)| p).collect()
}

/// txt 合并（同步）：书籍信息头 + 逐章拼接，按 `txt_encoding` 编码写出。
fn merge_txt_sync(
    output_path: &Path,
    book: &Book,
    save_dir: &Path,
    txt_encoding: &str,
) -> Result<PathBuf, BoxError> {
    // 删除旧的同名 txt（源项目 FileUtil.del）
    let _ = fs::remove_file(output_path);
    let content = build_txt_content(book, save_dir);

    let (bytes, _, _) = match txt_encoding.to_ascii_uppercase().as_str() {
        "GBK" | "GB18030" => encoding_rs::GBK.encode(&content),
        _ => encoding_rs::UTF_8.encode(&content),
    };
    let mut file = fs::File::create(output_path)?;
    file.write_all(&bytes)?;
    Ok(output_path.to_path_buf())
}

/// 组装 txt 全文：首部书籍信息 + 章节顺序拼接（章节缓存恒为 UTF-8）。
fn build_txt_content(book: &Book, save_dir: &Path) -> String {
    let intro = if book.intro.is_empty() { "暂无".to_owned() } else { clean_html_tags(&book.intro) };
    let mut out = format!("书名：{}\n作者：{}\n简介：{}\n", book.book_name, book.author, intro);
    for f in sorted_chapter_files(save_dir) {
        if let Ok(s) = fs::read_to_string(&f) {
            out.push_str(&s);
        }
    }
    out
}

/// epub 合并（同步）：epub-builder 组包（元数据 + 封面 + 章节正文）。
fn merge_epub_sync(
    output_path: &Path,
    book: &Book,
    save_dir: &Path,
    cover: Option<&[u8]>,
) -> Result<PathBuf, BoxError> {
    let files = sorted_chapter_files(save_dir);
    if files.is_empty() {
        return Err("下载章节数为 0，取消生成 EPUB".into());
    }

    let mut builder = EpubBuilder::new(ZipLibrary::new()?)?;
    builder.metadata("title", &book.book_name)?.metadata("author", &book.author)?.metadata("lang", "zh")?;
    if !book.intro.is_empty() {
        builder.metadata("description", clean_html_tags(&book.intro))?;
    }
    builder.metadata("generator", "so-novel-rs")?;
    builder.metadata(
        "license",
        "本电子书由 so-novel(https://github.com/freeok/so-novel) 制作生成。仅供交流使用，不得用于商业用途。",
    )?;

    // 封面图 + 封面页（下载失败仅告警，不中断：与源项目一致）
    if let Some(bytes) = cover {
        builder.add_cover_image("cover.jpg", bytes, "image/jpeg")?;
    }
    builder.add_content(
        EpubContent::new("cover.html", COVER_PAGE.as_bytes()).title("封面").reftype(ReferenceType::Cover),
    )?;

    // 正文页：`{序号}_{章节名}.html` → toc 条目 + spine
    let digit_count = files.len().to_string().len();
    for (i, file) in files.iter().enumerate() {
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = stem.split_once('_').map_or(stem.clone(), |(_, t)| t.to_owned());
        let id = format!("{:0width$}", i + 1, width = digit_count);
        let bytes = fs::read(file)?;
        builder.add_content(EpubContent::new(format!("{id}.html"), bytes.as_slice()).title(title))?;
    }

    let mut output = BufWriter::new(fs::File::create(output_path)?);
    builder.generate(&mut output)?;
    Ok(output_path.to_path_buf())
}

/// 异步下载封面字节（失败返回 `None`；源项目 HttpUtil.downloadBytes 同语义）。
async fn download_cover(client: &reqwest::Client, cover_url: &str) -> Option<Vec<u8>> {
    if cover_url.is_empty() {
        return None;
    }
    match client.get(cover_url).timeout(std::time::Duration::from_secs(15)).send().await {
        Ok(resp) => match resp.bytes().await {
            Ok(bytes) => Some(bytes.to_vec()),
            Err(e) => {
                tracing::warn!(url = cover_url, error = %e, "封面下载失败");
                None
            }
        },
        Err(e) => {
            tracing::warn!(url = cover_url, error = %e, "封面下载失败");
            None
        }
    }
}

/// 去 HTML 标签（对应源项目 `HtmlUtil.cleanHtmlTag`，简介清洗用）。
fn clean_html_tags(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"<[^>]+>").expect("常量正则恒合法"));
    re.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Book {
        Book {
            book_name: "斗破苍穹".into(),
            author: "天蚕土豆".into(),
            intro: "<p>萧炎的故事</p>".into(),
            ..Default::default()
        }
    }

    fn write_chapters(dir: &Path, chapters: &[(&str, &str)]) {
        fs::create_dir_all(dir).expect("创建目录");
        for (name, content) in chapters {
            fs::write(dir.join(name), content).expect("写章节");
        }
    }

    #[test]
    fn sorted_chapter_files_orders_by_numeric_prefix() {
        let dir = std::env::temp_dir().join("sn_test_sort_chapters");
        let _ = fs::remove_dir_all(&dir);
        write_chapters(&dir, &[("10_x.txt", ""), ("2_x.txt", ""), ("1_x.txt", "")]);
        let files = sorted_chapter_files(&dir);
        let names: Vec<_> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["1_x.txt", "2_x.txt", "10_x.txt"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_txt_sync_writes_info_header_and_chapters() {
        let dir = std::env::temp_dir().join("sn_test_merge_txt");
        let _ = fs::remove_dir_all(&dir);
        write_chapters(
            &dir,
            &[
                ("01_第1章 开篇.txt", "第1章 开篇\n\n　　正文一\n"),
                ("02_第2章 继续.txt", "第2章 继续\n\n　　正文二\n"),
            ],
        );
        let out = dir.join("out.txt");
        merge_txt_sync(&out, &book(), &dir, "").expect("合并失败");
        let content = fs::read_to_string(&out).expect("读取产物");
        assert!(content.starts_with("书名：斗破苍穹\n作者：天蚕土豆\n简介：萧炎的故事\n"));
        assert!(content.contains("正文一"));
        assert!(content.contains("正文二"));
        // GBK 编码路径：写出后再以 GBK 解码校验
        let out_gbk = dir.join("out-gbk.txt");
        merge_txt_sync(&out_gbk, &book(), &dir, "GBK").expect("GBK 合并失败");
        let raw = fs::read(&out_gbk).expect("读取 GBK 产物");
        let (decoded, _, _) = encoding_rs::GBK.decode(&raw);
        assert!(decoded.contains("书名：斗破苍穹"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_epub_sync_generates_valid_zip() {
        let dir = std::env::temp_dir().join("sn_test_merge_epub");
        let _ = fs::remove_dir_all(&dir);
        let chapter = r#"<?xml version="1.0" encoding="UTF-8" ?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head>
<body><p>正文</p></body></html>"#;
        write_chapters(&dir, &[("01_第1章.html", chapter), ("02_第2章.html", chapter)]);
        let out = dir.join("book.epub");
        merge_epub_sync(&out, &book(), &dir, None).expect("epub 生成失败");
        let bytes = fs::read(&out).expect("读取 epub");
        // epub 为 zip 容器：魔数 PK；mimetype 首条目为 application/epub+zip
        assert_eq!(&bytes[..2], b"PK");
        assert!(bytes.windows(20).any(|w| w == b"application/epub+zip"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_epub_sync_empty_dir_errors() {
        let dir = std::env::temp_dir().join("sn_test_merge_epub_empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建目录");
        let err = merge_epub_sync(&dir.join("b.epub"), &book(), &dir, None).expect_err("空目录应报错");
        assert!(err.to_string().contains("章节数为 0"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_html_tags_strips_tags_only() {
        assert_eq!(clean_html_tags("<p>简介<b>加粗</b></p>"), "简介加粗");
        assert_eq!(clean_html_tags("无标签"), "无标签");
    }
}
