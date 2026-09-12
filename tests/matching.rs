//! 集成测试 1/5：模式匹配与优先级。
//!
//! 覆盖：
//! - 规则按 priority 降序命中（更具体的规则先于兜底规则）；
//! - 同优先级确定性（id 字典序）；
//! - regex / json 两类 matcher；
//! - json 规则的 where 条件（equals / exists / not_equals / contains / regex）；
//! - 无任何规则命中 → None（由管线送隔离区）；
//! - 规则文件编译期校验（重复 id、坏正则、未知类型/操作符直接报错）。

use logparse::rules::{compile_rules, RuleFile};

fn engine() -> logparse::rules::Engine {
    logparse::service::load_engine(None).expect("内置规则编译失败")
}

fn field<'a>(hit: &'a logparse::rules::Hit, name: &str) -> Option<&'a logparse::model::FieldValue> {
    hit.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn as_str(v: &logparse::model::FieldValue) -> &str {
    match v {
        logparse::model::FieldValue::Str(s) => s,
        other => panic!("期望字符串，得到 {other:?}"),
    }
}

#[test]
fn priority_more_specific_rule_wins() {
    // 同一行既满足 java-text(100) 又满足 plain-text-fallback(10)，必须命中 java-text
    let line = "2024-05-01T10:00:00Z [INFO] order-service - create order ok";
    let hit = engine().match_line(line).expect("应当命中");
    assert_eq!(hit.rule_id, "java-text");
    assert_eq!(as_str(field(&hit, "service").unwrap()), "order-service");
    assert_eq!(as_str(field(&hit, "level").unwrap()), "INFO");
}

#[test]
fn plain_fallback_only_when_nothing_else_matches() {
    // 含级别信号但不符合任何头部格式 → 兜底规则
    let line = "WARNING: something happened without any known header";
    let hit = engine().match_line(line).expect("兜底规则应命中");
    assert_eq!(hit.rule_id, "plain-text-fallback");
    assert_eq!(as_str(field(&hit, "message").unwrap()), line);
}

#[test]
fn pure_garbage_matches_nothing() {
    // 没有任何结构化信号的乱码：所有规则都不命中 → 由管线隔离
    assert!(engine().match_line("@@@###$$$%%%").is_none());
    assert!(engine().match_line("qwlkjqwelkqj  zzzzz").is_none());
}

#[test]
fn json_where_conditions_select_specific_rule() {
    // payment 规则要求 service=payment-service 且 amount 存在
    let pay = r#"{"service":"payment-service","amount":42.5,"order_id":"A-1001","level":"warn"}"#;
    let hit = engine().match_line(pay).unwrap();
    assert_eq!(hit.rule_id, "json-payment-service");

    // 同为 JSON 但不满足 where → 落到 json-generic
    let other = r#"{"service":"gateway","message":"hello"}"#;
    let hit = engine().match_line(other).unwrap();
    assert_eq!(hit.rule_id, "json-generic");
}

#[test]
fn json_rule_skipped_for_non_json_line() {
    // 非 JSON 行不能被 json 规则"吞掉"后就停止，应继续落到 regex 规则
    let line = "2024-05-01 10:00:00 - svc - INFO - started";
    let hit = engine().match_line(line).unwrap();
    assert_eq!(hit.rule_id, "python-text");
}

#[test]
fn no_match_returns_none() {
    // 全空白行会被管线跳过；这里验证引擎对"只有标点"的行仍可能兜底，
    // 而真正无法解析的形态由管线 quarantine 测试覆盖。
    // 构造一个不含任何非空白字符之外内容的行：兜底正则要求 \S，空串不命中
    assert!(engine().match_line("").is_none());
    assert!(engine().match_line("    ").is_none());
}

#[test]
fn compile_order_and_diagnostics() {
    let rules = r#"{
        "version": 1,
        "rules": [
            {"id":"low","priority":1,"match":"regex","pattern":"^x$","fields":[]},
            {"id":"high","priority":9,"match":"regex","pattern":"^y$","fields":[]},
            {"id":"same-b","priority":5,"match":"regex","pattern":"^z$","fields":[]},
            {"id":"same-a","priority":5,"match":"regex","pattern":"^w$","fields":[]}
        ]
    }"#;
    let e = compile_rules(rules).unwrap();
    assert_eq!(
        e.rule_ids_in_order(),
        vec!["high", "same-a", "same-b", "low"]
    );
}

#[test]
fn invalid_rule_files_are_rejected() {
    // 重复 id
    let dup = r#"{"version":1,"rules":[
        {"id":"r","match":"regex","pattern":"^a$","fields":[]},
        {"id":"r","match":"regex","pattern":"^b$","fields":[]}]}"#;
    assert!(compile_rules(dup).is_err());

    // 坏正则
    let bad_re = r#"{"version":1,"rules":[{"id":"r","match":"regex","pattern":"([0-9","fields":[]}]}"#;
    assert!(compile_rules(bad_re).is_err());

    // 未知字段类型
    let bad_type = r#"{"version":1,"rules":[{"id":"r","match":"regex","pattern":"^(?P<m>.*)$",
        "fields":[{"name":"m","type":"quantum","sources":[{"type":"group","group":"m"}]}]}]}"#;
    assert!(compile_rules(bad_type).is_err());

    // 未知 where 操作符
    let bad_op = r#"{"version":1,"rules":[{"id":"r","match":"json",
        "where":[{"path":"a","op":"near","value":1}],"fields":[]}]}"#;
    assert!(compile_rules(bad_op).is_err());

    // 版本不符
    let v: RuleFile = serde_json::from_str(r#"{"version":9,"rules":[]}"#).unwrap();
    assert!(logparse::rules::compile_rule_file(v).is_err());
}
