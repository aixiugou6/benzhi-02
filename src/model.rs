//! 数据模型：规范化后的结构化日志记录、字段值类型与隔离记录。

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// 输出字段经过类型推断后的强类型值。
///
/// - 未显式声明类型（`auto`）的字段：数字尽量推断为 i64/f64，布尔推断为 bool，其余为字符串；
/// - 显式声明的类型按声明转换，转换失败则该字段被丢弃（坏字段不污染整条记录）。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Str(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    /// 多行堆栈：按行保存（已 trim 右侧空白、去空行），输出时既给拼接字符串也给数组。
    Stack(Vec<String>),
}

impl FieldValue {
    /// 转成检索库里落库的 JSON 形态。stack 输出为字符串（见 [`Record::to_json`] 的 stack_lines）。
    pub fn to_json(&self) -> Value {
        match self {
            FieldValue::Str(s) => Value::String(s.clone()),
            FieldValue::I64(n) => Value::from(*n),
            FieldValue::F64(n) => serde_json::Number::from_f64(*n)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            FieldValue::Bool(b) => Value::Bool(*b),
            FieldValue::Stack(lines) => Value::String(lines.join("\n")),
        }
    }
}

/// 一条解析完成、等待输出的结构化日志记录。
#[derive(Debug, Clone)]
pub struct Record {
    /// 规范化后的字段表（顺序输出时按 key 排序，保证输出稳定、便于 diff/测试）。
    pub fields: BTreeMap<String, FieldValue>,
    /// 命中的规则 id（可观测：知道这条是被谁解析的）。
    pub rule_id: String,
    /// 该记录由哪些原始行合并而来（多行堆栈场景 >1），用于溯源。
    pub source_line_numbers: Vec<u64>,
}

impl Record {
    pub fn new(rule_id: impl Into<String>) -> Self {
        Self {
            fields: BTreeMap::new(),
            rule_id: rule_id.into(),
            source_line_numbers: Vec::new(),
        }
    }

    /// 输出规范化：
    /// 1. 字段名固定集合顺序，其余字段按名字排序追加；
    /// 2. `timestamp` 统一 RFC3339 UTC；`level` 统一大写枚举；
    /// 3. `stack` 同时输出字符串 `stack` 与行数组 `stack_lines`，方便全文检索与逐帧分析；
    /// 4. 附加 `_rule` / `_source_lines` 元信息（下划线前缀，避免与业务字段冲突）。
    pub fn to_json(&self) -> Value {
        // 核心字段的固定输出顺序
        const CORE_ORDER: [&str; 6] =
            ["timestamp", "level", "service", "order_id", "message", "stack"];

        let mut out = Map::new();

        for name in CORE_ORDER {
            if let Some(v) = self.fields.get(name) {
                out.insert(name.to_string(), v.to_json());
                if let FieldValue::Stack(lines) = v {
                    out.insert(
                        "stack_lines".to_string(),
                        Value::Array(lines.iter().map(|l| Value::String(l.clone())).collect()),
                    );
                }
            }
        }

        // 其余字段按字典序追加，输出确定性
        for (k, v) in &self.fields {
            if CORE_ORDER.contains(&k.as_str()) || k == "stack_lines" {
                continue;
            }
            out.insert(k.clone(), v.to_json());
        }

        out.insert("_rule".to_string(), Value::String(self.rule_id.clone()));
        out.insert(
            "_source_lines".to_string(),
            Value::Array(
                self.source_line_numbers
                    .iter()
                    .map(|n| Value::from(*n))
                    .collect(),
            ),
        );
        Value::Object(out)
    }
}

/// 无法解析的原始行：进入隔离区而不是静默丢弃。
#[derive(Debug, Clone)]
pub struct QuarantineEntry {
    /// 原始行内容（原样保留，不做任何改写）。
    pub raw: String,
    /// 1 起始的输入行号。
    pub line_number: u64,
    /// 隔离原因，如 "no rule matched"。
    pub reason: String,
}
