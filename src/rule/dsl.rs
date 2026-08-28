//! DSL 后置处理：`@js:` / `@java:` 步骤的解析与执行。
//!
//! 对应源项目 `dsl/` 包实际被规则文件用到的子集（`JsCaller` + `JavaExecutor`）：
//! - `@js:<code>` → `QuickJS` 执行 `function func(r){ <code>; return r; }`，`r` 为输入字符串；
//! - `@java:base64.decode()` → 逐行 Base64 解码后拼接（源项目 `JavaExecutor`）；
//! - `@java:string.replace('a','b')` → 正则替换（源项目 `JavaExecutor`）。
//!
//! 语法：`selector@js:<code>;@java:<code>`，提取结果依次经各步骤变换。
//! `QuickJS` context 按调用创建销毁（无常驻池，见设计文档 §3）。

use rquickjs::{Context, Runtime};

/// DSL 步骤（`@js:` 或 `@java:`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DslStep {
    /// `@js:<code>`：`QuickJS` 片段（源项目 `JsCaller.call`）
    Js(String),
    /// `@java:<code>`：内建 Java 步骤（源项目 `JavaExecutor`）
    Java(String),
}

/// 拆分 DSL 表达式：`(选择器部分, 步骤列表)`。
///
/// 以首个 `@js:` / `@java:` 标记为界，其后按标记切分步骤。
/// JS 代码体内的 `@`（非标记形态）不影响切分（比源项目朴素扫描更稳健，
/// main.json 全部规则经此解析正确）。
pub(crate) fn split_dsl(expr: &str) -> (&str, Vec<DslStep>) {
    let mut steps = Vec::new();
    let Some(first) = find_marker(expr, 0) else { return (expr, steps) };
    let head = &expr[..first];
    let rest = &expr[first..];
    let mut pos = 0;
    while let Some(mark) = find_marker(rest, pos) {
        let colon = mark + rest[mark..].find(':').expect("标记必含冒号");
        let code_start = colon + 1;
        // 步骤代码延伸至下一个标记（或表达式末尾）
        let code_end = find_marker(rest, code_start).unwrap_or(rest.len());
        let code = rest[code_start..code_end].trim().trim_end_matches(';').trim();
        let step = if rest[mark..].starts_with("@js:") {
            DslStep::Js(code.to_owned())
        } else {
            DslStep::Java(code.to_owned())
        };
        steps.push(step);
        pos = code_end;
    }
    (head, steps)
}

/// 定位 `@js:` / `@java:` 标记的起始偏移
fn find_marker(s: &str, from: usize) -> Option<usize> {
    let tail = s.get(from..)?;
    let js = tail.find("@js:").map(|p| p + from);
    let java = tail.find("@java:").map(|p| p + from);
    match (js, java) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// 依次执行步骤变换输入（无步骤时原样返回）。
///
/// # Errors
/// JS 执行异常或 Java 步骤解码失败时返回错误消息。
pub(crate) fn run_steps(steps: &[DslStep], input: &str) -> Result<String, String> {
    let mut out = input.to_owned();
    for step in steps {
        out = match step {
            DslStep::Js(code) => run_js(code, &out)?,
            DslStep::Java(code) => run_java(code, &out)?,
        };
    }
    Ok(out)
}

/// `QuickJS` 执行 `@js:` 片段（源项目 `JsCaller.call`：包装为 `func(r)` 再调用）。
fn run_js(code: &str, input: &str) -> Result<String, String> {
    let rt = Runtime::new().map_err(|e| format!("QuickJS 运行时创建失败: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("QuickJS 上下文创建失败: {e}"))?;
    ctx.with(|ctx| {
        // 输入以 JSON 字符串字面量内嵌（保证引号/换行/控制字符正确转义）
        let input_json = serde_json::to_string(input).expect("&str 序列化为 JSON 字符串不会失败");
        let script = format!("String((function(r){{ {code}; return r; }})({input_json}))");
        ctx.eval::<String, _>(script.as_str()).map_err(|e| format!("JS 执行失败: {e}"))
    })
}

/// 执行 `@java:` 内建步骤（源项目 `JavaExecutor`：仅两个已知操作，未知原样返回）。
fn run_java(code: &str, input: &str) -> Result<String, String> {
    if code == "base64.decode()" {
        // 按行切分 → 过滤空行 → 分别解码 → 拼接（与源项目逐行 Base64::decodeStr 一致）。
        // 额外兼容：同一行内多段 base64 连排（如 html5ever 序列化后 <script> 间无换行，
        // 前段的 `==` padding 落在行中间）时按 padding 边界分段解码。
        use base64::Engine as _;
        let token = regex::Regex::new(r"[A-Za-z0-9+/]+={0,2}").expect("常量正则恒合法");
        let mut out = String::new();
        for line in input.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            for seg in token.find_iter(line) {
                let s = seg.as_str();
                // 长度非 4 倍数的 token 是杂质片段（如残留标签字母 `p`），跳过；
                // 有效正文段均为标准 padded base64（长度为 4 的倍数）
                if s.len() % 4 != 0 {
                    continue;
                }
                // 解码失败的段同样视为杂质跳过（对齐源项目宽松解码的实际效果）
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(s) {
                    out.push_str(&String::from_utf8_lossy(&decoded));
                }
            }
        }
        return Ok(out);
    }
    // string.replace('正则','替换')（源项目 ReUtil 提取两个参数后 replaceAll）
    let re = regex::Regex::new(r"^string\.replace\('(.*)','(.*)'\)$").expect("常量正则恒合法");
    if let Some(caps) = re.captures(code) {
        let pattern = caps.get(1).expect("捕获组 1 恒存在").as_str();
        let replacement = caps.get(2).expect("捕获组 2 恒存在").as_str();
        let regex = regex::Regex::new(pattern).map_err(|e| format!("string.replace 正则无效: {e}"))?;
        return Ok(regex.replace_all(input, replacement).into_owned());
    }
    // 未知 Java 步骤原样返回（与源项目一致）
    Ok(input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_dsl_plain_selector_has_no_steps() {
        let (head, steps) = split_dsl("#content");
        assert_eq!(head, "#content");
        assert_eq!(steps, Vec::new());
    }

    #[test]
    fn split_dsl_js_only() {
        let (head, steps) = split_dsl("meta[property=\"og:image\"]@js:r='http://x'+r");
        assert_eq!(head, r#"meta[property="og:image"]"#);
        assert_eq!(steps, vec![DslStep::Js("r='http://x'+r".into())]);
    }

    #[test]
    fn split_dsl_js_then_java() {
        let (head, steps) = split_dsl("#htmlContent@js:r=r.replace(/a/g,'b');@java:base64.decode()");
        assert_eq!(head, "#htmlContent");
        assert_eq!(
            steps,
            vec![DslStep::Js("r=r.replace(/a/g,'b')".into()), DslStep::Java("base64.decode()".into()),]
        );
    }

    #[test]
    fn split_dsl_attr_suffix_is_not_step() {
        let (head, steps) = split_dsl("a@href");
        assert_eq!(head, "a@href");
        assert!(steps.is_empty(), "@href 无冒号不构成 DSL 步骤");
    }

    #[test]
    fn run_js_concats_prefix() {
        // main.json coverUrl 规则形态：r='http://www.mcxs.la'+r
        let out = run_js("r='http://www.mcxs.la'+r", "/files/article/image/1/2.jpg").unwrap();
        assert_eq!(out, "http://www.mcxs.la/files/article/image/1/2.jpg");
    }

    #[test]
    fn run_js_supports_replace_with_function_callback() {
        // main.json toc.list / chapter.content 规则形态：replace 回调
        let code = "r=r.replace(/<b>(\\w+)<\\/b>/g,function(m,p1){return p1.toUpperCase()})";
        let out = run_js(code, "前<b>abc</b>后<b>xy</b>尾").unwrap();
        assert_eq!(out, "前ABC后XY尾");
    }

    #[test]
    fn run_js_multi_statement_mutates_r() {
        let code = "var a='foo'; r=a+r";
        let out = run_js(code, "bar").unwrap();
        assert_eq!(out, "foobar");
    }

    #[test]
    fn run_js_syntax_error_returns_err() {
        let err = run_js("r=undefined syntax", "x").unwrap_err();
        assert!(err.contains("JS 执行失败"), "{err}");
    }

    #[test]
    fn run_java_base64_decode_multiline() {
        // "aGVsbG8=" → hello；"d29ybGQ=" → world（空行与空白行应被跳过）
        let input = "aGVsbG8=\n\n  d29ybGQ=  \n";
        let out = run_java("base64.decode()", input).unwrap();
        assert_eq!(out, "helloworld");
    }

    #[test]
    fn run_java_base64_decode_joined_segments_with_junk() {
        // 同一行多段 base64 连排（html5ever 序列化后 <script> 间无换行）+ 残留标签字母杂质
        // （wxsy 场景：`<p>请勿开启浏览器阅读模式</p>` 明文标签混在 base64 流之后）
        let out = run_java("base64.decode()", "aGVsbG8=d29ybGQ=<p>x</p>").unwrap();
        assert_eq!(out, "helloworld");
    }

    #[test]
    fn run_java_base64_all_invalid_yields_empty() {
        // 全部 token 无效 → 空输出，由上层"正文内容为空"兜底报错
        let out = run_java("base64.decode()", "!!!not-base64!!!").unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn run_java_string_replace() {
        let out = run_java("string.replace('foo\\d','X')", "foo1 foo2 bar").unwrap();
        assert_eq!(out, "X X bar");
    }

    #[test]
    fn run_java_unknown_code_passthrough() {
        let out = run_java("unknown.op()", "原样").unwrap();
        assert_eq!(out, "原样");
    }

    #[test]
    fn run_steps_chains_js_then_java() {
        // main.json mcxs chapter.content 规则形态：js replace + java base64
        let b64 = "5Lit5paH";
        let steps = vec![
            DslStep::Js("r=r.replace(/^\\s+|\\s+$/g,'')".into()),
            DslStep::Java("base64.decode()".into()),
        ];
        let out = run_steps(&steps, &format!("  {b64} \n")).unwrap();
        assert_eq!(out, "中文");
    }

    #[test]
    fn run_steps_empty_returns_input() {
        assert_eq!(run_steps(&[], "x").unwrap(), "x");
    }
}
