//! 规则引擎：规则文件的加载、编译、按优先级匹配与字段抽取。
//!
//! 规则全部来自 JSON 配置（见 `rules/default.json`），代码里不写死任何业务规则。
//! 匹配顺序：规则按 `priority` 降序（同优先级按 id 字典序，保证确定性），首个命中生效。

use crate::types::{coerce_json, coerce_string, FieldType};
use crate::util_json::{embedded_json, query_path, DottedPath};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// 规则文件根结构。
#[derive(Debug, Deserialize)]
pub struct RuleFile {
    pub version: u32,
    pub rules: Vec<RuleDef>,
}

/// 单条规则的声明式定义（JSON 配置）。
#[derive(Debug, Deserialize)]
pub struct RuleDef {
    /// 规则唯一 id，会写入输出记录的 `_rule` 字段。
    pub id: String,
    /// 数值越大越先匹配；相同则按 id 字典序。
    #[serde(default)]
    pub priority: i32,
    /// 匹配方式：`regex`（整行正则）或 `json`（整行可解析为 JSON 对象）。
    #[serde(rename = "match")]
    pub match_kind: String,
    /// match=regex 时的整行正则，必须能锚定日志头；可用命名捕获组供字段抽取。
    #[serde(default)]
    pub pattern: Option<String>,
    /// match=json 时 JSON 来源，目前支持 `line`（整行就是 JSON）。
    #[serde(default)]
    pub json_source: Option<String>,
    /// match=json 时的额外条件，全部满足才命中；空数组表示"是 JSON 对象即可"。
    #[serde(rename = "where", default)]
    pub where_conds: Vec<Condition>,
    /// 命中后的字段抽取定义，按声明顺序写入。
    pub fields: Vec<FieldDef>,
}

/// json 规则的 where 条件。
#[derive(Debug, Deserialize)]
pub struct Condition {
    /// 点分 JSON 路径，如 `ctx.order_id`。
    pub path: String,
    /// 操作符：equals / not_equals / exists / contains / regex。
    pub op: String,
    /// equals/not_equals/contains/regex 的比较值。
    #[serde(default)]
    pub value: Option<Value>,
}

/// 一个输出字段的抽取定义。
#[derive(Debug, Deserialize)]
pub struct FieldDef {
    /// 输出字段名。
    pub name: String,
    /// 字段类型，默认 `auto`。
    #[serde(rename = "type", default = "default_field_type")]
    pub ty: String,
    /// 取值来源列表：按顺序尝试，第一个抽取出非空值的来源生效。
    #[serde(default)]
    pub sources: Vec<SourceDef>,
}

/// serde 缺省字段类型：auto（尽量推断 i64/f64/bool/string）。
fn default_field_type() -> String {
    "auto".to_string()
}

/// 字段值的单个来源。
#[derive(Debug, Deserialize)]
pub struct SourceDef {
    /// 来源类型：
    /// - `group`：主正则的命名捕获组；
    /// - `regex`：在 `group`（默认整行）文本上再跑一个小正则，取命名组 `value`（否则第 1 组）；
    /// - `json_path`：取 `group` 文本里内嵌 JSON 的某路径；无 group 时取整行 JSON；
    /// - `constant`：固定值（用于给某类日志补 service 等）；
    /// - `stack_inline`：从 `group` 文本中抽取单行内联的 Java 异常堆栈（后续行由状态机续接）。
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub json_path: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

/// 来源解析出的未经类型转换的值：文本或原生 JSON 值。
enum RawValue {
    Text(String),
    Json(Value),
}

/// 编译后的规则（正则已编译、类型已解析、路径已解析），可直接用于匹配。
pub struct CompiledRule {
    pub id: String,
    pub priority: i32,
    is_json: bool,
    main_regex: Option<Regex>,
    conds: Vec<CompiledCondition>,
    fields: Vec<CompiledField>,
}

struct CompiledCondition {
    path: DottedPath,
    op: CondOp,
    value: Option<Value>,
    regex: Option<Regex>,
}

#[derive(Clone, Copy)]
enum CondOp {
    Equals,
    NotEquals,
    Exists,
    Contains,
    Regex,
}

struct CompiledField {
    name: String,
    ty: FieldType,
    sources: Vec<CompiledSource>,
}

struct CompiledSource {
    kind: SourceKind,
    group: Option<String>,
    json_path: Option<DottedPath>,
    regex: Option<Regex>,
    value: Option<Value>,
}

#[derive(Clone, Copy)]
enum SourceKind {
    Group,
    Regex,
    JsonPath,
    Constant,
    StackInline,
}

/// 从规则 JSON 文本编译整套规则。
pub fn compile_rules(text: &str) -> Result<Engine, String> {
    let file: RuleFile =
        serde_json::from_str(text).map_err(|e| format!("规则文件 JSON 解析失败: {e}"))?;
    compile_rule_file(file)
}

/// 从已解析的 RuleFile 编译（便于测试/嵌入）。
pub fn compile_rule_file(file: RuleFile) -> Result<Engine, String> {
    if file.version != 1 {
        return Err(format!("不支持的规则版本: {}", file.version));
    }
    let mut compiled = Vec::with_capacity(file.rules.len());
    let mut seen = std::collections::HashSet::new();
    for r in file.rules {
        if !seen.insert(r.id.clone()) {
            return Err(format!("规则 id 重复: {}", r.id));
        }
        let id = r.id.clone();
        compiled.push(compile_one(r).map_err(|e| format!("规则[{id}]编译失败: {e}"))?);
    }
    // 优先级降序；并列时 id 字典序，保证匹配顺序确定
    compiled.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Engine { rules: compiled })
}

fn compile_one(r: RuleDef) -> Result<CompiledRule, String> {
    let is_json = match r.match_kind.as_str() {
        "regex" => false,
        "json" => true,
        other => return Err(format!("未知 match 类型: {other}")),
    };

    let main_regex = if !is_json {
        let p = r
            .pattern
            .as_deref()
            .ok_or_else(|| "regex 规则缺少 pattern".to_string())?;
        Some(Regex::new(p).map_err(|e| e.to_string())?)
    } else {
        // json_source 缺省按 "line" 处理（整行就是 JSON）
        if let Some(src) = r.json_source.as_deref() {
            if src != "line" {
                return Err("json 规则目前只支持 json_source=\"line\"".to_string());
            }
        }
        None
    };

    let mut conds = Vec::new();
    for c in r.where_conds {
        let op = match c.op.as_str() {
            "equals" => CondOp::Equals,
            "not_equals" => CondOp::NotEquals,
            "exists" => CondOp::Exists,
            "contains" => CondOp::Contains,
            "regex" => CondOp::Regex,
            other => return Err(format!("未知 where 操作符: {other}")),
        };
        let regex = match op {
            CondOp::Regex => Some(Regex::new(
                c.value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "regex 条件需要字符串 value".to_string())?,
            ).map_err(|e| e.to_string())?),
            _ => None,
        };
        conds.push(CompiledCondition {
            path: DottedPath::parse(&c.path),
            op,
            value: c.value,
            regex,
        });
    }

    let mut fields = Vec::new();
    for f in r.fields {
        let ty = FieldType::parse(&f.ty)
            .ok_or_else(|| format!("字段[{}]未知类型: {}", f.name, f.ty))?;
        let mut sources = Vec::new();
        for s in f.sources {
            let kind = match s.kind.as_str() {
                "group" => SourceKind::Group,
                "regex" => SourceKind::Regex,
                "json_path" => SourceKind::JsonPath,
                "constant" => SourceKind::Constant,
                "stack_inline" => SourceKind::StackInline,
                other => {
                    return Err(format!("字段[{}]未知来源类型: {other}", f.name))
                }
            };
            let regex = match kind {
                SourceKind::Regex => Some(Regex::new(
                    s.pattern
                        .as_deref()
                        .ok_or_else(|| format!("字段[{}]的 regex 来源缺少 pattern", f.name))?,
                ).map_err(|e| e.to_string())?),
                _ => None,
            };
            sources.push(CompiledSource {
                kind,
                group: s.group,
                json_path: s.json_path.map(|p| DottedPath::parse(&p)),
                regex,
                value: s.value,
            });
        }
        fields.push(CompiledField {
            name: f.name,
            ty,
            sources,
        });
    }

    Ok(CompiledRule {
        id: r.id,
        priority: r.priority,
        is_json,
        main_regex,
        conds,
        fields,
    })
}

/// 一次匹配的命中结果：规则 id + 每个输出字段的强类型值。
pub struct Hit {
    pub rule_id: String,
    pub fields: Vec<(String, crate::model::FieldValue)>,
}

/// 规则引擎：持有全部已编译规则，提供行级匹配。
pub struct Engine {
    rules: Vec<CompiledRule>,
}

impl Engine {
    /// 已编译规则按优先级排序后的 id 列表（测试/诊断用）。
    pub fn rule_ids_in_order(&self) -> Vec<&str> {
        self.rules.iter().map(|r| r.id.as_str()).collect()
    }

    /// 对一行原始日志做匹配与字段抽取。无规则命中返回 None（进入隔离区）。
    pub fn match_line(&self, line: &str) -> Option<Hit> {
        // 整行仅在"看起来像 JSON"时才尝试解析，避免每行都付解析成本
        let line_json = try_parse_object(line);

        for rule in &self.rules {
            if rule.is_json {
                // 非 JSON 行跳过 json 规则，继续尝试后面的 regex 规则
                let obj = match line_json.as_ref() {
                    Some(o) => o,
                    None => continue,
                };
                if rule.conds.iter().all(|c| eval_condition(c, obj)) {
                    return Some(extract(rule, line, None, Some(obj)));
                }
            } else if let Some(caps) = rule
                .main_regex
                .as_ref()
                .expect("regex 规则必有主正则")
                .captures(line)
            {
                return Some(extract(rule, line, Some(&caps), line_json.as_ref()));
            }
        }
        None
    }
}

/// 尝试把整行解析为 JSON 对象。
fn try_parse_object(line: &str) -> Option<Value> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(t)
        .ok()
        .filter(|v| v.is_object())
}

fn eval_condition(c: &CompiledCondition, obj: &Value) -> bool {
    let actual = query_path(obj, &c.path);
    match c.op {
        CondOp::Exists => actual.is_some_and(|v| !v.is_null()),
        CondOp::Equals => actual.map(|v| Some(v) == c.value.as_ref()).unwrap_or(false),
        CondOp::NotEquals => actual
            .map(|v| Some(v) != c.value.as_ref())
            .unwrap_or(true),
        CondOp::Contains => match (actual, &c.value) {
            (Some(Value::String(s)), Some(Value::String(needle))) => s.contains(needle),
            _ => false,
        },
        CondOp::Regex => match (actual, &c.regex) {
            (Some(Value::String(s)), Some(re)) => re.is_match(s),
            _ => false,
        },
    }
}

/// 依据命中规则抽取全部字段，并按字段声明类型做强转。
fn extract(
    rule: &CompiledRule,
    line: &str,
    caps: Option<&regex::Captures<'_>>,
    line_json: Option<&Value>,
) -> Hit {
    let mut fields = Vec::new();
    // 内嵌 JSON 解析缓存：同一捕获文本的多个 json_path 来源只扫一次
    let mut embedded_cache: HashMap<String, Option<Value>> = HashMap::new();

    'fields: for field in &rule.fields {
        for src in &field.sources {
            let raw = match resolve_source(src, line, caps, line_json, &mut embedded_cache) {
                Some(v) => v,
                None => continue,
            };
            // 按字段声明类型做转换；转换失败（坏字段）则尝试下一个来源
            let typed = match &raw {
                RawValue::Text(t) => coerce_string(t, field.ty),
                RawValue::Json(v) => coerce_json(v, field.ty),
            };
            if let Some(fv) = typed {
                fields.push((field.name.clone(), fv));
                continue 'fields;
            }
        }
    }
    Hit {
        rule_id: rule.id.clone(),
        fields,
    }
}

/// 解析单个来源 → 未类型化的原始值。失败/空值返回 None，引擎继续尝试下一个来源。
fn resolve_source(
    src: &CompiledSource,
    line: &str,
    caps: Option<&regex::Captures<'_>>,
    line_json: Option<&Value>,
    embedded_cache: &mut HashMap<String, Option<Value>>,
) -> Option<RawValue> {
    match src.kind {
        SourceKind::Constant => src.value.clone().map(RawValue::Json),
        SourceKind::Group => {
            let text = caps?.name(src.group.as_deref()?)?.as_str();
            if text.trim().is_empty() {
                return None;
            }
            Some(RawValue::Text(text.to_string()))
        }
        SourceKind::Regex => {
            let base = match src.group.as_deref() {
                Some(g) => caps?.name(g)?.as_str(),
                None => line,
            };
            let re = src.regex.as_ref()?;
            let c = re.captures(base)?;
            let text = c.name("value").or_else(|| c.get(1)).map(|m| m.as_str())?;
            if text.trim().is_empty() {
                return None;
            }
            Some(RawValue::Text(text.to_string()))
        }
        SourceKind::JsonPath => {
            let path = src.json_path.as_ref()?;
            let value = if let Some(g) = src.group.as_deref() {
                // 在捕获组文本里找内嵌 JSON（"JSON 混文本"场景）
                let base = caps?.name(g)?.as_str().to_string();
                let entry = embedded_cache
                    .entry(base.clone())
                    .or_insert_with(|| embedded_json(&base));
                query_path(entry.as_ref()?, path)?
            } else {
                query_path(line_json?, path)?
            };
            if value.is_null() {
                return None;
            }
            Some(RawValue::Json(value.clone()))
        }
        SourceKind::StackInline => {
            let base = match src.group.as_deref() {
                Some(g) => caps?.name(g)?.as_str(),
                None => line,
            };
            let lines = crate::stack::extract_inline_stack(base)?;
            // 作为文本交给 stack 类型转换（按行拆分、清洗）
            Some(RawValue::Text(lines.join("\n")))
        }
    }
}
