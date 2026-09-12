//! 集成测试 2/5：字段抽取与类型推断。
//!
//! 覆盖：
//! - 主正则命名捕获组抽取；
//! - "JSON 混文本"：从普通日志行的 message 里扫描内嵌 JSON 抽 order_id；
//! - 字段级 regex 抽取（订单号：order id / orderId / 订单号 等写法）；
//! - 整行 JSON 的点分路径抽取；
//! - 多来源优先级（第一个成功的来源生效）；
//! - auto 推断（i64/f64/bool/string）与显式类型；
//! - 时间戳归一（时区换算、epoch、nginx CLF、毫秒精度保留）；
//! - level 别名归一；
//! - 坏类型值不污染记录（字段被丢弃，其他字段保留）。

use logparse::model::FieldValue;
use logparse::rules::Engine;
use logparse::service::load_engine;
use serde_json::json;
use serde_json::Value;

fn engine() -> Engine {
    load_engine(None).unwrap()
}

fn field<'a>(hit: &'a logparse::rules::Hit, name: &str) -> Option<&'a FieldValue> {
    hit.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn json_field(hit: &logparse::rules::Hit, name: &str) -> Value {
    field(hit, name).unwrap().to_json()
}

#[test]
fn named_groups_basic_extraction() {
    let line = "2024-05-01T10:00:00.123Z [ERROR] payment-svc - charge failed";
    let hit = engine().match_line(line).unwrap();
    assert_eq!(hit.rule_id, "java-text");
    assert_eq!(json_field(&hit, "timestamp"), json!("2024-05-01T10:00:00.123Z"));
    assert_eq!(json_field(&hit, "level"), json!("ERROR"));
    assert_eq!(json_field(&hit, "service"), json!("payment-svc"));
    assert_eq!(json_field(&hit, "message"), json!("charge failed"));
}

#[test]
fn embedded_json_inside_text_extracts_order_id() {
    // 典型"JSON 混文本"：日志头是文本，message 里嵌了一个 JSON 对象
    let line = r#"2024-05-01T10:00:01Z [INFO] order-service - create order {"order_id":"ORD-20240501-77","amount":99.8,"currency":"CNY"} ok"#;
    let hit = engine().match_line(line).unwrap();
    assert_eq!(json_field(&hit, "order_id"), json!("ORD-20240501-77"));
    // message 保留完整原文
    let msg = field(&hit, "message").unwrap();
    assert!(matches!(msg, FieldValue::Str(s) if s.contains("create order")));
}

#[test]
fn field_level_regex_order_variants() {
    // 文本写法：order id: xxx
    let l1 = "2024-05-01T10:00:00Z [INFO] svc - processing order id: AB-2024-99 now";
    assert_eq!(
        json_field(&engine().match_line(l1).unwrap(), "order_id"),
        json!("AB-2024-99")
    );
    // 中文"订单号"
    let l2 = "2024-05-01T10:00:00Z [INFO] svc - 收到请求 订单号=XK-77881 处理中";
    assert_eq!(
        json_field(&engine().match_line(l2).unwrap(), "order_id"),
        json!("XK-77881")
    );
    // 兜底纯文本行里也能抽
    let l3 = "WARNING order_no: ZZ-00042 retry later";
    let hit = engine().match_line(l3).unwrap();
    assert_eq!(json_field(&hit, "order_id"), json!("ZZ-00042"));
    assert_eq!(json_field(&hit, "level"), json!("WARN"));
}

#[test]
fn full_json_line_path_extraction_and_typed_fields() {
    let line = r#"{
        "ts": "2024-05-01T20:00:00+08:00",
        "level": "warning",
        "service": "payment-service",
        "order_id": "A-1001",
        "amount": 42.5,
        "currency": "USD",
        "duration_ms": "120",
        "message": "charged"
    }"#;
    let hit = engine().match_line(line).unwrap();
    assert_eq!(hit.rule_id, "json-payment-service");
    // +08:00 归一为 UTC
    assert_eq!(json_field(&hit, "timestamp"), json!("2024-05-01T12:00:00Z"));
    assert_eq!(json_field(&hit, "level"), json!("WARN"));
    assert_eq!(json_field(&hit, "amount"), json!(42.5));
    // 字符串形态的数字，声明 i64 → 转成数字
    assert_eq!(json_field(&hit, "duration_ms"), json!(120));
    assert_eq!(json_field(&hit, "currency"), json!("USD"));
}

#[test]
fn auto_type_inference_on_strings() {
    // nginx 规则里 status/bytes_sent 显式 i64；auto 路径用自定义小规则验证
    let rules = r#"{ "version": 1, "rules": [
        { "id":"auto", "priority":1, "match":"regex",
          "pattern":"^n=(?P<n>.*?) b=(?P<b>.*?) f=(?P<f>.*?) s=(?P<s>.*)$",
          "fields":[
            {"name":"n","sources":[{"type":"group","group":"n"}]},
            {"name":"b","sources":[{"type":"group","group":"b"}]},
            {"name":"f","sources":[{"type":"group","group":"f"}]},
            {"name":"s","sources":[{"type":"group","group":"s"}]}
          ]}
    ]}"#;
    let e = logparse::rules::compile_rules(rules).unwrap();
    let hit = e.match_line("n=123 b=true f=1.5 s=hello").unwrap();
    assert_eq!(field(&hit, "n"), Some(&FieldValue::I64(123)));
    assert_eq!(field(&hit, "b"), Some(&FieldValue::Bool(true)));
    assert_eq!(field(&hit, "f"), Some(&FieldValue::F64(1.5)));
    assert!(matches!(field(&hit, "s"), Some(FieldValue::Str(_))));
}

#[test]
fn nginx_fields_typed_and_order_from_path() {
    let line = r#"10.0.0.1 - - [10/Oct/2023:13:55:36 +0000] "POST /api/orders/ORD-9001/pay HTTP/1.1" 200 532 "-" "curl/8.0""#;
    let hit = engine().match_line(line).unwrap();
    assert_eq!(hit.rule_id, "nginx-text");
    assert_eq!(json_field(&hit, "timestamp"), json!("2023-10-10T13:55:36Z"));
    assert_eq!(json_field(&hit, "service"), json!("nginx"));
    assert_eq!(json_field(&hit, "method"), json!("POST"));
    assert_eq!(json_field(&hit, "status"), json!(200));
    assert_eq!(json_field(&hit, "bytes_sent"), json!(532));
    assert_eq!(json_field(&hit, "order_id"), json!("ORD-9001"));
    assert_eq!(json_field(&hit, "client_ip"), json!("10.0.0.1"));
}

#[test]
fn bad_typed_value_drops_only_that_field() {
    // amount 声明 f64，给不可解析值；order_id 正常 → amount 缺失、order_id 保留
    let rules = r#"{ "version": 1, "rules": [
        { "id":"j", "priority":1, "match":"json",
          "fields":[
            {"name":"order_id","sources":[{"type":"json_path","json_path":"order_id"}]},
            {"name":"amount","type":"f64","sources":[{"type":"json_path","json_path":"amount"}]}
          ]}
    ]}"#;
    let e = logparse::rules::compile_rules(rules).unwrap();
    let hit = e
        .match_line(r#"{"order_id":"A-1","amount":"not-a-number"}"#)
        .unwrap();
    assert!(field(&hit, "amount").is_none());
    assert_eq!(json_field(&hit, "order_id"), json!("A-1"));
}

#[test]
fn timestamp_variants_normalize() {
    use logparse::types::normalize_timestamp;
    assert_eq!(
        normalize_timestamp("2024-05-01 12:00:00,123").unwrap(),
        "2024-05-01T12:00:00.123Z"
    );
    assert_eq!(
        normalize_timestamp("20240501T120000Z").unwrap(),
        "2024-05-01T12:00:00Z"
    );
    assert_eq!(
        normalize_timestamp("1714564800123").unwrap(),
        "2024-05-01T12:00:00.123Z"
    );
    // 无法解析 → None，而不是抛错或瞎造
    assert!(normalize_timestamp("not a time").is_none());
}
