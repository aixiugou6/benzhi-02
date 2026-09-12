//! 集成测试 4/5：输出规范化。
//!
//! 覆盖：
//! - stdout 每条记录是一行合法 JSON（NDJSON），可被独立解析；
//! - 核心字段固定顺序：timestamp, level, service, order_id, message, stack；
//! - stack 同时输出字符串与 stack_lines 数组；
//! - 时间戳统一 RFC3339 UTC、level 统一大写枚举；
//! - 类型化字段输出为 JSON number/bool，而非全字符串；
//! - 每条带 _rule（命中规则）与 _source_lines（溯源行号）；
//! - 同一输入输出确定（重复运行字节一致）。

use logparse::model::{FieldValue, Record};
use logparse::pipeline::Pipeline;
use logparse::service::load_engine;
use serde_json::json;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

fn run(input: &[&str]) -> Vec<Value> {
    let mut p = Pipeline::new(Arc::new(load_engine(None).unwrap()), 200);
    let mut out = Vec::new();
    for (i, line) in input.iter().enumerate() {
        for r in p.process_line(line, (i + 1) as u64).records {
            out.push(r.to_json());
        }
    }
    for r in p.finish() {
        out.push(r.to_json());
    }
    out
}

#[test]
fn ndjson_is_valid_and_deterministic() {
    let input = ["2024-05-01T10:00:00Z [INFO] svc - hello {\"order_id\":\"A-1\"}"];
    let a = run(&input);
    let b = run(&input);
    assert_eq!(a, b, "重复运行结果必须一致");
    for v in &a {
        assert!(v.is_object());
    }
}

#[test]
fn core_field_order_is_fixed() {
    let input = [r#"{"timestamp":"2024-05-01T10:00:00Z","level":"info","service":"s","order_id":"A-1","message":"m"}"#];
    let v = run(&input).remove(0);
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    // 前 5 个必须是存在的核心字段且顺序固定（_rule/_source_lines 在末尾）
    assert_eq!(
        &keys[..5],
        &["timestamp", "level", "service", "order_id", "message"]
    );
    let positions = |name: &str| keys.iter().position(|k| *k == name).unwrap();
    assert!(positions("timestamp") < positions("level"));
    assert!(positions("level") < positions("service"));
    assert!(positions("service") < positions("order_id"));
    assert!(positions("order_id") < positions("message"));
    assert!(positions("message") < positions("_rule"));
}

#[test]
fn stack_emits_string_and_line_array() {
    let input = [
        "2024-05-01T10:00:00Z [ERROR] svc - create failed",
        "\tjava.lang.IllegalStateException: x",
        "\tat a.B.c(B.java:1)",
        "\t... 2 more",
    ];
    let v = run(&input).remove(0);
    let stack = v.get("stack").unwrap().as_str().unwrap();
    assert!(stack.contains("IllegalStateException"));
    assert!(stack.contains("\n"));
    let lines = v.get("stack_lines").unwrap().as_array().unwrap();
    assert_eq!(lines.len(), 3);
    // 帧行保留原始缩进（仅去除行尾空白），故用 trim 比较
    assert_eq!(lines[2].as_str().unwrap().trim(), "... 2 more");
}

#[test]
fn timestamp_and_level_canonicalized() {
    let input = [
        r#"{"ts":"2024-05-01T20:00:00+08:00","level":"warning","service":"payment-service","amount":1}"#,
    ];
    let v = run(&input).remove(0);
    assert_eq!(v["timestamp"], json!("2024-05-01T12:00:00Z"));
    assert_eq!(v["level"], json!("WARN"));
}

#[test]
fn typed_fields_are_json_natives() {
    let line = r#"10.0.0.1 - - [10/Oct/2023:13:55:36 +0000] "GET / HTTP/1.1" 404 12 "-" "x""#;
    let v = run(&[line]).remove(0);
    assert!(v["status"].is_i64());
    assert_eq!(v["status"], json!(404));
    assert_eq!(v["bytes_sent"], json!(12));
}

#[test]
fn metadata_rule_and_source_lines_present() {
    let input = [
        "2024-05-01T10:00:00Z [ERROR] svc - boom",
        "java.lang.RuntimeException: boom",
        "\tat a.B.c(B.java:1)",
        "2024-05-01T10:00:01Z [INFO] svc - ok",
    ];
    let out = run(&input);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["_rule"], json!("java-text"));
    let src: HashSet<i64> = out[0]["_source_lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(src, HashSet::from([1, 2, 3]));
    assert_eq!(out[1]["_source_lines"], json!([4]));
}

#[test]
fn record_without_optional_fields_still_serializes() {
    let mut r = Record::new("x");
    r.fields.insert("message".to_string(), FieldValue::Str("hi".into()));
    let v = r.to_json();
    // 缺字段不出现在输出里，不输出 null 占位
    assert!(v.get("timestamp").is_none());
    assert!(v.get("level").is_none());
    assert_eq!(v["message"], json!("hi"));
}
