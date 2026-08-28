//! 文件系统工具（对应源项目 `FileUtils`）。

/// 替换文件名非法字符（仅用于文件名而非路径，对应源项目 `FileUtils.sanitizeFileName`）。
///
/// Windows：`:*?"<>` 替换为全角/近形字符，`/\|` 替换为下划线；
/// 其他平台：仅处理 `.` `:` `/` 与 NUL。
pub fn sanitize_file_name(file_name: &str) -> String {
    let mut out = String::with_capacity(file_name.len());
    for c in file_name.chars() {
        let c = if cfg!(windows) {
            match c {
                ':' => '：',
                '*' => '＊',
                '?' => '？',
                '"' => '\'',
                '<' => '＜',
                '>' => '＞',
                '/' | '\\' | '|' => '_',
                _ => c,
            }
        } else {
            match c {
                '.' => '。',
                ':' => '：',
                '/' => '／',
                '\0' => '_',
                _ => c,
            }
        };
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn sanitize_replaces_windows_illegal_chars() {
        // 与源项目一致：< > 替换为全角（Windows 文件名非法字符）
        assert_eq!(sanitize_file_name("a:b*c?d\"e<f>g/h\\i|j"), "a：b＊c？d'e＜f＞g_h_i_j");
    }

    #[test]
    fn sanitize_keeps_chinese_and_digits() {
        assert_eq!(sanitize_file_name("第1章 斗破苍穹"), "第1章 斗破苍穹");
    }
}
