//! JSON 辅助：点分路径查询，以及从"文本里混着 JSON"的日志行中扫描出内嵌 JSON 对象。
//!
//! 内嵌 JSON 的扫描不依赖写死的格式：从每个 `{` 起始位置做带字符串感知的括号配对，
//! 凡是能被 serde 解析成对象的候选都收集，调用方按需取路径。

use serde_json::Value;

/// 点分路径的一段：对象键或数组下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    Key(String),
    Idx(usize),
}

/// 预解析的点分路径，如 `ctx.trace.0.id`。
#[derive(Debug, Clone)]
pub struct DottedPath(pub Vec<PathSeg>);

impl DottedPath {
    pub fn parse(path: &str) -> DottedPath {
        let segs = path
            .split('.')
            .filter(|s| !s.is_empty())
            .map(|s| match s.parse::<usize>() {
                // 纯数字段按下标处理（日志 JSON 的键极少是纯数字，歧义可接受）
                Ok(i) => PathSeg::Idx(i),
                Err(_) => PathSeg::Key(s.to_string()),
            })
            .collect();
        DottedPath(segs)
    }
}

/// 按点分路径取值；任一层缺失返回 None。
pub fn query_path<'a>(root: &'a Value, path: &DottedPath) -> Option<&'a Value> {
    let mut cur = root;
    for seg in &path.0 {
        cur = match seg {
            PathSeg::Key(k) => cur.as_object()?.get(k)?,
            PathSeg::Idx(i) => cur.as_array()?.get(*i)?,
        };
    }
    Some(cur)
}

/// 从可能混着普通文本的字符串中扫描全部内嵌 JSON 对象候选（按出现顺序、去重嵌套）。
///
/// 例：`order created {"order_id":"A-1","amount":42.5} ok`
/// 返回该对象。若存在外层包裹内层，外层先被发现且解析成功时，调用方直接用外层即可。
pub fn embedded_json_candidates(text: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut starts: Vec<usize> = Vec::new();

    // 记录每个 '{' 的配对终点；解析成功的区间不再被内层重复收集
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' {
            starts.push(i);
        }
    }

    let mut accepted_ranges: Vec<(usize, usize)> = Vec::new();

    for &start in &starts {
        let end = match match_braces(bytes, start) {
            Some(e) => e,
            None => continue,
        };
        // 已被某个成功的外层区间包含，则跳过（避免同值重复）
        if accepted_ranges
            .iter()
            .any(|&(s, e)| s <= start && end <= e)
        {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&text[start..=end]) {
            if v.is_object() {
                accepted_ranges.push((start, end));
                out.push(v);
            }
        }
    }
    out
}

/// 便捷封装：取第一个能解析的内嵌 JSON 对象。
pub fn embedded_json(text: &str) -> Option<Value> {
    embedded_json_candidates(text).into_iter().next()
}

/// 带字符串/转义感知的花括号配对：返回与 `start` 处 `{` 配对的 `}` 的字节下标。
fn match_braces(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_query_and_index() {
        let v = json!({"ctx": {"trace": [{"id": "x"}]}});
        let p = DottedPath::parse("ctx.trace.0.id");
        assert_eq!(query_path(&v, &p).unwrap(), &json!("x"));
        assert!(query_path(&v, &DottedPath::parse("ctx.nope")).is_none());
    }

    #[test]
    fn scan_embedded_in_mixed_text() {
        let line = r#"2024-05-01 [INFO] pay - order created {"order_id":"A-1001","amount":42.5} ret=0"#;
        let v = embedded_json(line).unwrap();
        assert_eq!(v["order_id"], json!("A-1001"));
        assert_eq!(v["amount"], json!(42.5));
    }

    #[test]
    fn scan_skips_braces_inside_strings() {
        // 字符串里含 } {，配对不能被干扰
        let line = r#"noise {"msg": "weird } char {", "ok": true} tail"#;
        let v = embedded_json(line).unwrap();
        assert_eq!(v["ok"], json!(true));
    }
}
