//! 多行 Java 异常堆栈合并（显式状态机）。
//!
//! ## 真实日志形态
//! 异常在日志里通常长这样（异常头是消息行**之后**的独立行，首行无法预知堆栈要来）：
//!
//! ```text
//! 2024-05-01T10:00:00Z [ERROR] svc - create order failed
//! \tjava.lang.IllegalStateException: stock timeout
//! \tat com.foo.Order.create(Order.java:88)
//! Caused by: java.sql.SQLException: lock wait
//! \tat com.foo.Db.exec(Db.java:201)
//! \t... 12 more
//! ```
//!
//! ## 状态与策略
//! - 正常记录**滞后一行**输出（merger 里持有一个 pending 候选）；
//! - 下一行若是异常头（行首 FQCN(Exception|Error|Throwable)）或堆栈帧（缩进 `at`、
//!   `Caused by:`、`... N more`），就回溯挂到 pending 上并进入 `Tracing`；
//! - Tracing 中持续吞堆栈块行，直到出现一条普通日志行：先冲刷异常记录再处理新行；
//! - 没有前导记录的孤立帧/缩进异常头 → 不吞，交调用方隔离；
//! - 输入结束（EOF）必须 flush pending/Tracing 记录，避免丢最后一条。

use crate::model::{FieldValue, Record};
use regex::Regex;
use std::sync::OnceLock;

/// 堆栈帧/收尾行：缩进的 `at`、`Caused by:` / `Suppressed:`、`... N more`。
fn is_frame_line(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?x)
              ^\s+at\s+\S                                   # 缩进的 at 帧
            | ^\s*(?:Caused\s+by|Suppressed):\s*\S           # 因果链/抑制异常
            | ^\s*\.\.\.\s*\d+\s+
                (?:more|common\s+frames\s+omitted(?:\s+by\s+\S+)?)\s*$  # JVM/Spring 收尾
            ",
        )
        .expect("frame regex")
    });
    re.is_match(line)
}

/// 行首（允许缩进）是否是异常类头，如 `java.lang.NullPointerException: boom`。
/// 类名必须以 Exception/Error/Throwable 结尾，排除 ErrorCounter 这类普通词。
fn exception_header_at_start(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\s*[A-Za-z_][\w$.]*(?:Exception|Error|Throwable)(?:\s*:|\s*$)")
            .expect("header-at-start regex")
    });
    re.is_match(line)
}

/// 异常头是否带缩进（带缩进的头不可能独立成一条日志，只可能是某条堆栈的一部分）。
fn is_indented(line: &str) -> bool {
    line.starts_with(|c: char| c.is_whitespace())
}

/// 判断一行普通文本里是否包含异常类 token（用于兜底场景）。
fn contains_exception_token(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"[A-Za-z_][\w$.]*(?:Exception|Error|Throwable)(?:\s*:|$)")
            .expect("header token regex")
    });
    re.is_match(line)
}

/// 从单行文本中抽取内联堆栈（异常头与若干 `at` 帧被打印在同一行的情况）。
///
/// 例：`create order failed: java.lang.IllegalStateException: bad at a.b.c(C.java:1) at d.e(F.java:2)`
/// → ["java.lang.IllegalStateException: bad", "at a.b.c(C.java:1)", "at d.e(F.java:2)"]
/// 只有头、没有任何帧时返回 None（不算堆栈，避免误伤普通报错文本）。
pub fn extract_inline_stack(text: &str) -> Option<Vec<String>> {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let header_re = HEADER.get_or_init(|| {
        Regex::new(r"[A-Za-z_][\w$.]*(?:Exception|Error|Throwable)").expect("header token regex")
    });
    let m = header_re.find(text)?;
    let suffix = &text[m.start()..];

    // 头与首帧之间以 " at " 分隔
    let split = suffix.find(" at ")?;
    let head = suffix[..split].trim();
    if head.is_empty() {
        return None;
    }
    let mut lines = vec![head.to_string()];
    // split 指向 " at " 的空格，+4 起为第一个栈帧，之后各帧以 " at " 分隔
    for frame in suffix[split + 4..].split(" at ") {
        let f = frame.trim();
        // 类名风格字符开头才认为是栈帧，尾部非帧噪声由此被自然排除
        if !f.is_empty() && f.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '$') {
            lines.push(format!("at {f}"));
        }
    }
    if lines.len() < 2 {
        return None;
    }
    Some(lines)
}

/// 状态机对一行输入的处置结果。
pub struct MergeResult {
    /// 本次可以输出的记录（冲刷出的上一条/上一条堆栈），0~2 条。
    pub emit: Vec<Record>,
    /// 本行无法处理时的隔离原因（None 表示正常处理或被堆栈吞掉）。
    pub quarantine_reason: Option<&'static str>,
}

/// 隔离原因常量。
pub const ORPHAN_STACK_LINE: &str = "orphan stack line";
pub const NO_RULE_MATCHED: &str = "no rule matched";

/// 多行堆栈合并状态机。
pub struct StackMerger {
    state: State,
    /// 单条堆栈保留的最大行数（不含截断提示行）。
    max_stack_lines: usize,
}

enum State {
    /// 上一条普通记录滞后持有，等待看下一行是不是堆栈；None 表示没有候选。
    Idle(Option<Record>),
    /// 正在累积堆栈，record 即最终要输出的那条记录。
    Tracing(Pending),
}

struct Pending {
    record: Record,
    dropped: usize,
}

impl StackMerger {
    pub fn new(max_stack_lines: usize) -> Self {
        assert!(max_stack_lines > 0, "max_stack_lines 必须大于 0");
        Self {
            state: State::Idle(None),
            max_stack_lines,
        }
    }

    /// 当前是否处于堆栈续接状态（诊断用）。
    pub fn is_tracing(&self) -> bool {
        matches!(self.state, State::Tracing(_))
    }

    /// 送入一行已被规则引擎处理过的输入。
    ///
    /// - `hit`：规则引擎命中结果（None 表示无规则匹配）；
    /// - `raw`：原始行文本；
    /// - `line_number`：1 起始行号。
    pub fn accept(
        &mut self,
        hit: Option<crate::rules::Hit>,
        raw: &str,
        line_number: u64,
    ) -> MergeResult {
        // 1) Tracing：堆栈块行继续吞
        if let State::Tracing(_) = &self.state {
            if is_frame_line(raw) || exception_header_at_start(raw) {
                self.append_frame(raw.trim_end().to_string(), line_number);
                return MergeResult {
                    emit: Vec::new(),
                    quarantine_reason: None,
                };
            }
            // 普通行：堆栈结束，冲刷异常记录后继续处理本行
            let closed = self.close_tracing();
            let mut r = self.dispatch_new_line(hit, raw, line_number);
            r.emit.splice(0..0, closed);
            return r;
        }

        // 2) Idle 且持有上一条记录：下一行以异常头开头 → 回溯挂接。
        //    只认异常头（真实 JVM 堆栈中帧前必有异常类头行）；裸帧不挂接，走第 3 步隔离，
        //    避免把普通日志（尤其 JSON 行）后巧合出现的缩进 "at" 行误并成堆栈。
        if let State::Idle(Some(_)) = &self.state {
            if exception_header_at_start(raw) {
                self.promote_pending(raw.trim_end().to_string(), line_number);
                return MergeResult {
                    emit: Vec::new(),
                    quarantine_reason: None,
                };
            }
        }

        // 3) Idle 且没有候选：孤立的堆栈块行（帧，或缩进的异常头）→ 隔离
        if is_frame_line(raw) || (exception_header_at_start(raw) && is_indented(raw)) {
            return MergeResult {
                emit: Vec::new(),
                quarantine_reason: Some(ORPHAN_STACK_LINE),
            };
        }

        // 4) 普通新行：冲刷旧候选，持有新候选（滞后一行）
        self.dispatch_new_line(hit, raw, line_number)
    }

    /// 输入结束：冲刷最后持有的记录（普通候选或进行中的堆栈）。
    pub fn finish(&mut self) -> Option<Record> {
        match std::mem::replace(&mut self.state, State::Idle(None)) {
            State::Idle(r) => r,
            State::Tracing(p) => Some(finalize(p, self.max_stack_lines)),
        }
    }

    /// 处理一条普通新日志行：冲刷旧候选，新记录作为候选滞后持有。
    fn dispatch_new_line(
        &mut self,
        hit: Option<crate::rules::Hit>,
        _raw: &str,
        line_number: u64,
    ) -> MergeResult {
        let prev = match &self.state {
            State::Idle(r) => r.clone(),
            State::Tracing(_) => None,
        };
        let emit: Vec<Record> = prev.into_iter().collect();

        match hit {
            Some(hit) => {
                let mut record = Record::new(hit.rule_id);
                record.source_line_numbers.push(line_number);
                let mut inline_stack = None;
                for (k, v) in hit.fields {
                    if let FieldValue::Stack(lines) = &v {
                        inline_stack = Some(lines.clone());
                    }
                    record.fields.insert(k, v);
                }
                if let Some(lines) = inline_stack {
                    // 内联堆栈：直接进入 Tracing，等后续帧
                    let dropped = lines.len().saturating_sub(self.max_stack_lines);
                    record
                        .fields
                        .insert("stack".to_string(), FieldValue::Stack(truncate(lines, self.max_stack_lines)));
                    self.state = State::Tracing(Pending { record, dropped });
                } else {
                    self.state = State::Idle(Some(record));
                }
                MergeResult {
                    emit,
                    quarantine_reason: None,
                }
            }
            None => {
                // 无规则匹配：不产生候选，交调用方隔离
                self.state = State::Idle(None);
                MergeResult {
                    emit,
                    quarantine_reason: Some(NO_RULE_MATCHED),
                }
            }
        }
    }

    /// 把堆栈块行挂到上一条候选记录上，状态切到 Tracing。
    fn promote_pending(&mut self, line: String, line_number: u64) {
        let State::Idle(Some(mut record)) =
            std::mem::replace(&mut self.state, State::Idle(None))
        else {
            return;
        };
        // 若消息文本本身含异常 token，用消息作为堆栈头更可读；否则直接以本行起头
        let head_from_message = record
            .fields
            .get("message")
            .and_then(|v| match v {
                FieldValue::Str(s) if contains_exception_token(s) => Some(s.clone()),
                _ => None,
            });
        let initial = match head_from_message {
            Some(head) => vec![head, line],
            None => vec![line],
        };
        record.source_line_numbers.push(line_number);
        record
            .fields
            .insert("stack".to_string(), FieldValue::Stack(initial));
        self.state = State::Tracing(Pending {
            record,
            dropped: 0,
        });
    }

    fn append_frame(&mut self, line: String, line_number: u64) {
        let State::Tracing(pending) = &mut self.state else {
            return;
        };
        pending.record.source_line_numbers.push(line_number);
        let Some(FieldValue::Stack(stack)) = pending.record.fields.get_mut("stack") else {
            return;
        };
        if stack.len() < self.max_stack_lines {
            stack.push(line);
        } else {
            // 超限：挤掉一条最早的普通 at 帧（保住头与 Caused by/... N more），
            // 压入新帧；每挤掉一帧都计入截断数，收尾时统一标注
            if let Some(pos) = stack
                .iter()
                .position(|l| l.trim_start().starts_with("at "))
            {
                stack.remove(pos);
                stack.push(line);
                pending.dropped += 1;
            } else {
                // 没有可挤的普通帧（全是收尾行）→ 直接丢弃本行
                pending.dropped += 1;
            }
        }
    }

    fn close_tracing(&mut self) -> Option<Record> {
        let state = std::mem::replace(&mut self.state, State::Idle(None));
        match state {
            State::Tracing(p) => Some(finalize(p, self.max_stack_lines)),
            State::Idle(r) => r,
        }
    }
}

/// 收尾：追加截断提示行。
fn finalize(mut p: Pending, max_stack_lines: usize) -> Record {
    if p.dropped > 0 {
        if let Some(FieldValue::Stack(stack)) = p.record.fields.get_mut("stack") {
            stack.push(format!(
                "... {} more stack line(s) truncated (limit {})",
                p.dropped, max_stack_lines
            ));
        }
    }
    p.record
}

fn truncate(mut lines: Vec<String>, max: usize) -> Vec<String> {
    if lines.len() > max {
        lines.truncate(max);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_classification() {
        assert!(is_frame_line("\tat com.foo.Bar.run(Bar.java:42)"));
        assert!(is_frame_line("Caused by: java.io.IOException: reset by peer"));
        assert!(is_frame_line("\t... 12 more"));
        assert!(is_frame_line("   ... 5 common frames omitted"));
        // 正常日志行不是帧
        assert!(!is_frame_line(
            "2024-05-01T10:00:00Z [INFO] svc - at noon we started"
        ));
        assert!(!is_frame_line("{\"level\":\"INFO\",\"msg\":\"x\"}"));
    }

    #[test]
    fn header_at_start_classification() {
        assert!(exception_header_at_start(
            "\tjava.lang.NullPointerException: boom"
        ));
        assert!(exception_header_at_start("java.lang.FooError"));
        assert!(!exception_header_at_start("ErrorCounter incremented"));
        assert!(!exception_header_at_start(
            "2024-05-01 [ERROR] svc - java.lang.FooException: x"
        ));
    }

    #[test]
    fn inline_stack_extraction() {
        let lines = extract_inline_stack(
            "rpc failed: java.lang.IllegalStateException: bad state at a.b.C.m(C.java:1) at d.e.F.n(F.java:2)",
        )
        .unwrap();
        assert_eq!(lines[0], "java.lang.IllegalStateException: bad state");
        assert_eq!(lines[1], "at a.b.C.m(C.java:1)");
        assert_eq!(lines[2], "at d.e.F.n(F.java:2)");
        // 没有帧时不算内联堆栈
        assert!(extract_inline_stack("got java.lang.FooException: no frames").is_none());
    }
}
