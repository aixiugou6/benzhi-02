//! 字段类型推断与规范化：
//! - `auto`：从原始字符串/JSON 值推断 i64 / f64 / bool / string；
//! - 显式类型：string / i64 / f64 / bool / timestamp / level / stack；
//! - timestamp 统一归一为 UTC RFC3339（秒级补 `:00`，毫秒/微秒/纳秒保留 3/6/9 位）；
//! - level 归一为 TRACE/DEBUG/INFO/WARN/ERROR/FATAL 大写枚举。

use crate::model::FieldValue;
use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};

/// 规则中声明的字段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Auto,
    String,
    I64,
    F64,
    Bool,
    Timestamp,
    Level,
    Stack,
}

impl FieldType {
    pub fn parse(s: &str) -> Option<FieldType> {
        Some(match s {
            "auto" => FieldType::Auto,
            "string" => FieldType::String,
            "i64" => FieldType::I64,
            "f64" => FieldType::F64,
            "bool" => FieldType::Bool,
            "timestamp" => FieldType::Timestamp,
            "level" => FieldType::Level,
            "stack" => FieldType::Stack,
            _ => return None,
        })
    }
}

/// 级别别名归一。无法识别时返回 None（调用方丢弃该字段，不臆造级别）。
pub fn normalize_level(raw: &str) -> Option<&'static str> {
    // 去空白与包围符号后按大写比较，兼容 "ERROR"、"[warn]"、"Warning" 等
    let s = raw.trim().trim_matches(|c: char| c == '[' || c == ']' || c == ':');
    Some(match s.to_ascii_uppercase().as_str() {
        "TRACE" | "TRC" | "FINER" | "FINEST" => "TRACE",
        "DEBUG" | "DBG" | "FINE" => "DEBUG",
        "INFO" | "INF" | "NOTICE" | "INFORMATION" => "INFO",
        "WARN" | "WARNING" | "WRN" => "WARN",
        "ERROR" | "ERR" | "SEVERE" | "FAIL" | "FAILURE" => "ERROR",
        "FATAL" | "CRITICAL" | "CRIT" | "EMERG" | "EMERGENCY" | "ALERT" | "PANIC" => "FATAL",
        _ => return None,
    })
}

/// 把可能带时区的时间值转成 UTC；无时区信息时按 UTC 处理。
fn to_utc(dt: DateTime<FixedOffset>) -> DateTime<Utc> {
    dt.with_timezone(&Utc)
}

/// 去掉时间部分（首个 `T` 之后）里的所有空白：
/// `2024-05-01T20:00:00.123 +0800` → `2024-05-01T20:00:00.123+0800`，
/// `2024-05-01T12:00:00 Z` → `2024-05-01T12:00:00Z`。
fn strip_space_before_offset(s: &str) -> String {
    match s.find('T') {
        Some(pos) => {
            let (head, tail) = s.split_at(pos + 1);
            let tail: String = tail.chars().filter(|&c| !c.is_whitespace()).collect();
            format!("{head}{tail}")
        }
        None => s.to_string(),
    }
}

/// 解析时间字符串。覆盖日志里常见的形态，全部返回 UTC RFC3339。
///
/// 支持（有代表性的非穷举列表）：
/// - RFC3339：`2024-05-01T12:00:00Z`、`2024-05-01 12:00:00+08:00`、带毫秒/微秒；
/// - 无时区：`2024-05-01 12:00:00`、`2024-05-01T12:00:00,123`（逗号小数，Java 风格）；
/// - nginx：`10/Oct/2023:13:55:36 +0000`；
/// - 紧凑格式：`20240501T120000Z`；
/// - epoch 秒/毫秒：纯数字时按 10 位秒、13 位毫秒处理。
pub fn normalize_timestamp(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // epoch：10 位秒 / 13 位毫秒（浮点秒也支持）
    if let Ok(n) = s.parse::<i64>() {
        let (secs, millis) = match n.unsigned_abs().to_string().len() {
            10 => (n, 0),
            13 => (n.div_euclid(1000), n.rem_euclid(1000)),
            _ => return None,
        };
        return DateTime::from_timestamp(secs, (millis as u32) * 1_000_000)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
    }
    if let Ok(f) = s.parse::<f64>() {
        if (1_000_000_000.0..10_000_000_000.0).contains(&f) {
            let secs = f.trunc() as i64;
            let nanos = ((f.fract()) * 1e9).round() as u32;
            return DateTime::from_timestamp(secs, nanos)
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
        }
    }

    // nginx CLF：10/Oct/2023:13:55:36 +0000
    if let Some(r) = parse_nginx_time(s) {
        return Some(r);
    }

    // 紧凑：20240501T120000(Z|+0800)
    if let Some(r) = parse_compact(s) {
        return Some(r);
    }

    // 标准形态：把空格/T 统一、逗号小数换点号后交给 chrono 多格式尝试
    let normalized = s.replacen(' ', "T", 1).replace(',', ".");
    // 去掉时区偏移前的空白："2024-05-01T20:00:00.123 +0800" → "...123+0800"
    let normalized = strip_space_before_offset(&normalized);

    let formats: [&str; 8] = [
        "%Y-%m-%dT%H:%M:%S%.f%#z", // 带小数秒 + 偏移（%#z 兼容 +0800/+08:00）
        "%Y-%m-%dT%H:%M:%S%.fZ",
        "%Y-%m-%dT%H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%dT%H:%M:%S%.f", // 无时区：按 UTC
        "%Y-%m-%dT%H:%M:%S",
        "%Y/%m/%dT%H:%M:%S%.f",
        "%Y/%m/%dT%H:%M:%S",
    ];

    // 先按带偏移解析
    for fmt in formats {
        if let Ok(dt) = DateTime::parse_from_str(&normalized, fmt) {
            return Some(format_utc(to_utc(dt)));
        }
    }
    // 再按朴素时间解析（视为 UTC）
    let naive_formats = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y/%m/%dT%H:%M:%S%.f",
        "%Y/%m/%dT%H:%M:%S",
    ];
    for fmt in naive_formats {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(format_utc(Utc.from_utc_datetime(&ndt)));
        }
    }
    None
}

/// 按原始精度输出 UTC RFC3339（chrono 会按纳秒值自动选 0/3/6/9 位）。
fn format_utc(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

fn parse_nginx_time(s: &str) -> Option<String> {
    // 10/Oct/2023:13:55:36 +0000
    let (date, tz) = s.split_once(' ')?;
    let (d, clock) = date.split_once(':')?;
    // clock 里还可能含冒号（时:分:秒），split_once 只切第一个，正确
    let mut di = d.split('/');
    let day: u32 = di.next()?.parse().ok()?;
    let mon = match di.next()? {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i32 = di.next()?.parse().ok()?;
    let mut ci = clock.split(':');
    let hour: u32 = ci.next()?.parse().ok()?;
    let min: u32 = ci.next()?.parse().ok()?;
    let sec: u32 = ci.next()?.parse().ok()?;
    let ndt = NaiveDate::from_ymd_opt(year, mon, day)?
        .and_hms_opt(hour, min, sec)?;
    let offset = parse_tz_hhmm(tz.trim())?;
    let dt = offset.from_local_datetime(&ndt).single()?;
    Some(format_utc(to_utc(dt)))
}

fn parse_compact(s: &str) -> Option<String> {
    // 20240501T120000 或带 Z / +0800
    let bytes = s.as_bytes();
    if bytes.len() < 15 || &s[8..9] != "T" {
        return None;
    }
    let parse = |r: &str| -> Option<chrono::NaiveDateTime> {
        let y: i32 = r[0..4].parse().ok()?;
        let mo: u32 = r[4..6].parse().ok()?;
        let d: u32 = r[6..8].parse().ok()?;
        let h: u32 = r[9..11].parse().ok()?;
        let mi: u32 = r[11..13].parse().ok()?;
        let se: u32 = r[13..15].parse().ok()?;
        NaiveDate::from_ymd_opt(y, mo, d).and_then(|d| d.and_hms_opt(h, mi, se))
    };
    let rest = &s[15..];
    let ndt = parse(s)?;
    if rest.is_empty() {
        return Some(format_utc(Utc.from_utc_datetime(&ndt)));
    }
    if rest == "Z" || rest == "z" {
        return Some(format_utc(Utc.from_utc_datetime(&ndt)));
    }
    if rest.starts_with('+') || rest.starts_with('-') {
        let offset = parse_tz_hhmm(rest)?;
        let dt = offset.from_local_datetime(&ndt).single()?;
        return Some(format_utc(to_utc(dt)));
    }
    None
}

/// 解析 `Z` / `+0800` / `+08:00` 形式的偏移。
fn parse_tz_hhmm(tz: &str) -> Option<FixedOffset> {
    if tz.eq_ignore_ascii_case("z") || tz.is_empty() {
        return FixedOffset::east_opt(0);
    }
    let neg = tz.starts_with('-');
    let body = tz.trim_start_matches(['+', '-']).replace(':', "");
    if body.len() != 4 {
        return None;
    }
    let hh: i32 = body[0..2].parse().ok()?;
    let mm: i32 = body[2..4].parse().ok()?;
    let secs = (hh * 3600 + mm * 60) * if neg { -1 } else { 1 };
    FixedOffset::east_opt(secs)
}

/// auto 推断：字符串形态的字段值。
fn infer_from_str(raw: &str) -> FieldValue {
    let s = raw.trim();
    if let Ok(b) = s.to_ascii_lowercase().parse::<bool>() {
        return FieldValue::Bool(b);
    }
    if let Ok(n) = s.parse::<i64>() {
        return FieldValue::I64(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        if f.is_finite() {
            return FieldValue::F64(f);
        }
    }
    FieldValue::Str(raw.to_string())
}

/// 把一个已经抽取出的原始字符串值按声明类型转换。
/// 返回 None 表示类型转换失败（坏字段，调用方跳过，不写入记录）。
pub fn coerce_string(raw: &str, ty: FieldType) -> Option<FieldValue> {
    let s = raw.trim();
    Some(match ty {
        FieldType::Auto => infer_from_str(s),
        FieldType::String => FieldValue::Str(raw.trim().to_string()),
        FieldType::I64 => FieldValue::I64(s.parse().ok()?),
        FieldType::F64 => {
            let f: f64 = s.parse().ok()?;
            if !f.is_finite() {
                return None;
            }
            FieldValue::F64(f)
        }
        FieldType::Bool => FieldValue::Bool(s.to_ascii_lowercase().parse().ok()?),
        FieldType::Timestamp => FieldValue::Str(normalize_timestamp(s)?),
        FieldType::Level => FieldValue::Str(normalize_level(s)?.to_string()),
        FieldType::Stack => FieldValue::Stack(split_stack(s)),
    })
}

/// 把 JSON 值按声明类型转换（auto 时保留 JSON 原生标量类型）。
pub fn coerce_json(value: &serde_json::Value, ty: FieldType) -> Option<FieldValue> {
    use serde_json::Value::*;
    match (ty, value) {
        (FieldType::Auto, String(s)) => Some(infer_from_str(s)),
        (FieldType::Auto, Number(n)) => {
            n.as_i64().map(FieldValue::I64).or_else(|| n.as_f64().map(FieldValue::F64))
        }
        (FieldType::Auto, Bool(b)) => Some(FieldValue::Bool(*b)),
        (FieldType::Auto, Null) => None,
        // 复杂值：以紧凑 JSON 字符串承载（例如嵌套对象），不丢信息
        (FieldType::Auto, other) => Some(FieldValue::Str(other.to_string())),

        (FieldType::String, v) => Some(FieldValue::Str(match v {
            String(s) => s.clone(),
            other => other.to_string(),
        })),
        (FieldType::I64, Number(n)) => Some(FieldValue::I64(n.as_i64()?)),
        (FieldType::I64, String(s)) => Some(FieldValue::I64(s.trim().parse().ok()?)),
        (FieldType::F64, Number(n)) => Some(FieldValue::F64(n.as_f64()?)),
        (FieldType::F64, String(s)) => Some(FieldValue::F64(s.trim().parse().ok()?)),
        (FieldType::Bool, Bool(b)) => Some(FieldValue::Bool(*b)),
        (FieldType::Bool, String(s)) => {
            Some(FieldValue::Bool(s.trim().to_ascii_lowercase().parse().ok()?))
        }
        (FieldType::Timestamp, String(s)) => Some(FieldValue::Str(normalize_timestamp(s)?)),
        (FieldType::Timestamp, Number(n)) => {
            // 与字符串 epoch 规则一致：10 位秒 / 13 位毫秒
            let s = n.to_string();
            Some(FieldValue::Str(normalize_timestamp(&s)?))
        }
        (FieldType::Level, String(s)) => {
            Some(FieldValue::Str(normalize_level(s)?.to_string()))
        }
        (FieldType::Stack, String(s)) => Some(FieldValue::Stack(split_stack(s))),
        // JSON 里堆栈可能本身就是数组
        (FieldType::Stack, Array(arr)) => Some(FieldValue::Stack(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim_end().to_string()))
                .filter(|s| !s.trim().is_empty())
                .collect(),
        )),
        _ => None,
    }
}

/// 拆分堆栈文本为行：按行拆分、去右侧空白、去空行。
pub fn split_stack(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_aliases() {
        assert_eq!(normalize_level("WARNING").unwrap(), "WARN");
        assert_eq!(normalize_level("[err]").unwrap(), "ERROR");
        assert_eq!(normalize_level("critical").unwrap(), "FATAL");
        assert!(normalize_level("whatever").is_none());
    }

    #[test]
    fn timestamp_rfc3339_and_offset() {
        assert_eq!(
            normalize_timestamp("2024-05-01T12:00:00Z").unwrap(),
            "2024-05-01T12:00:00Z"
        );
        // +08:00 归一成 UTC（减 8 小时）
        assert_eq!(
            normalize_timestamp("2024-05-01 20:00:00,123 +0800").unwrap(),
            "2024-05-01T12:00:00.123Z"
        );
        // 无时区按 UTC
        assert_eq!(
            normalize_timestamp("2024-05-01 12:00:00").unwrap(),
            "2024-05-01T12:00:00Z"
        );
    }

    #[test]
    fn timestamp_nginx_and_epoch() {
        assert_eq!(
            normalize_timestamp("10/Oct/2023:13:55:36 +0000").unwrap(),
            "2023-10-10T13:55:36Z"
        );
        assert_eq!(
            normalize_timestamp("1714564800").unwrap(),
            "2024-05-01T12:00:00Z"
        );
        assert_eq!(
            normalize_timestamp("1714564800123").unwrap(),
            "2024-05-01T12:00:00.123Z"
        );
    }

    #[test]
    fn inference_basics() {
        assert_eq!(coerce_string(" 42 ", FieldType::Auto).unwrap(), FieldValue::I64(42));
        assert_eq!(
            coerce_string("true", FieldType::Auto).unwrap(),
            FieldValue::Bool(true)
        );
        assert_eq!(
            coerce_string("1.5", FieldType::F64).unwrap(),
            FieldValue::F64(1.5)
        );
        // 坏数据：不是数字
        assert!(coerce_string("abc", FieldType::I64).is_none());
    }
}
