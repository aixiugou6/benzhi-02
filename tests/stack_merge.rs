//! 集成测试 3/5：多行 Java 堆栈合并（状态机）。
//!
//! 覆盖：
//! - 异常头 + 多行 at 帧 + Caused by + ... N more 合并为一条记录；
//! - 合并后的记录携带完整 stack / stack_lines，且 source 行号连续可溯源；
//! - 堆栈进行中到来的新日志行会先冲刷上一条，再正常解析，不会粘连；
//! - EOF flush：最后一条堆栈在输入结束时输出，不丢失；
//! - 超长堆栈截断并在尾部标注；
//! - 开头就是孤立续行行（Idle 下）不会被吞掉，而进入隔离区；
//! - 单行内联堆栈（异常与帧打印在同一行）抽取；
//! - 堆栈进行中的空行不打断帧序列。

use logparse::model::FieldValue;
use logparse::pipeline::Pipeline;
use logparse::service::load_engine;
use std::sync::Arc;

fn pipeline() -> Pipeline {
    Pipeline::new(Arc::new(load_engine(None).unwrap()), 200)
}

fn stack_lines(r: &logparse::model::Record) -> Vec<String> {
    match r.fields.get("stack") {
        Some(FieldValue::Stack(l)) => l.clone(),
        other => panic!("期望 stack 字段，得到 {other:?}"),
    }
}

#[test]
fn multiline_stack_merged_into_one_record() {
    let input = [
        "2024-05-01T10:00:00Z [ERROR] order-svc - create order failed",
        "\tjava.lang.IllegalStateException: stock lock timeout",
        "\tat com.shop.order.OrderService.create(OrderService.java:88)",
        "\tat com.shop.order.OrderController.post(OrderController.java:42)",
        "Caused by: java.sql.SQLTransientException: lock wait timeout",
        "\tat com.shop.db.Lock.acquire(Lock.java:201)",
        "\t... 12 more",
        "2024-05-01T10:00:02Z [INFO] order-svc - recovered",
    ];

    let mut p = pipeline();
    let mut records = Vec::new();
    for (i, line) in input.iter().enumerate() {
        let out = p.process_line(line, (i + 1) as u64);
        assert!(out.quarantined.is_none(), "堆栈续行不应被隔离");
        records.extend(out.records);
    }
    records.extend(p.finish());

    assert_eq!(records.len(), 2, "异常 1 条 + 恢复 1 条");
    let err = &records[0];
    assert_eq!(err.rule_id, "java-text");
    assert_eq!(err.source_line_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

    let stack = stack_lines(err);
    assert!(stack[0].contains("IllegalStateException"));
    assert!(stack.iter().any(|l| l.contains("OrderService.java:88")));
    assert!(stack.iter().any(|l| l.starts_with("Caused by:")));
    assert!(stack.iter().any(|l| l.trim() == "... 12 more"));
    // 头日志自身的字段仍在
    assert!(matches!(err.fields.get("level"), Some(FieldValue::Str(s)) if s == "ERROR"));

    let ok = &records[1];
    assert!(ok.fields.get("stack").is_none(), "恢复日志不应带堆栈");
    assert_eq!(ok.source_line_numbers, vec![8]);
}

#[test]
fn stack_flushed_at_eof_without_trailing_log() {
    // 输入以堆栈帧结尾、没有后续正常日志：finish() 必须冲刷
    let input = [
        "2024-05-01T10:00:00Z [ERROR] svc - boom",
        "java.lang.RuntimeException: boom",
        "\tat a.B.c(B.java:1)",
    ];
    let mut p = pipeline();
    let mut n = 0;
    for (i, line) in input.iter().enumerate() {
        n += p.process_line(line, (i + 1) as u64).records.len();
    }
    assert_eq!(n, 0, "EOF 前不应提前输出");
    let tail = p.finish();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].source_line_numbers, vec![1, 2, 3]);
}

#[test]
fn orphan_continuation_at_start_is_quarantined() {
    // Idle 状态下第一行就是 "at ..."：它不是任何异常的续行，必须隔离而不是吞掉
    let mut p = pipeline();
    let out = p.process_line("\tat com.foo.Bar.run(Bar.java:1)", 1);
    assert!(out.records.is_empty());
    let q = out.quarantined.expect("孤立续行行应进入隔离区");
    assert_eq!(q.line_number, 1);
    assert!(q.raw.contains("Bar.run"));
}

#[test]
fn inline_single_line_stack_is_extracted() {
    let line = "2024-05-01T10:00:00Z [ERROR] rpc-svc - call failed: java.lang.IllegalStateException: bad at a.b.C.m(C.java:1) at d.e.F.n(F.java:2)";
    let mut p = pipeline();
    let out = p.process_line(line, 1);
    // 内联堆栈进入暂存（等可能的后续帧），EOF 冲刷
    assert!(out.records.is_empty());
    let recs = p.finish();
    assert_eq!(recs.len(), 1);
    let stack = stack_lines(&recs[0]);
    assert_eq!(stack[0], "java.lang.IllegalStateException: bad");
    assert!(stack.iter().any(|l| l.contains("C.java:1")));
    assert!(stack.iter().any(|l| l.contains("F.java:2")));
}

#[test]
fn blank_line_during_stack_does_not_break_sequence() {
    let input = [
        "2024-05-01T10:00:00Z [ERROR] svc - boom",
        "java.lang.RuntimeException: boom",
        "",
        "\tat a.B.c(B.java:1)",
    ];
    let mut p = pipeline();
    for (i, line) in input.iter().enumerate() {
        let out = p.process_line(line, (i + 1) as u64);
        assert!(out.quarantined.is_none());
    }
    let recs = p.finish();
    assert_eq!(recs.len(), 1);
    let stack = stack_lines(&recs[0]);
    assert!(stack.iter().any(|l| l.contains("B.java:1")));
    // 空行本身不进 stack
    assert!(stack.iter().all(|l| !l.trim().is_empty()));
}

#[test]
fn overlong_stack_is_truncated_with_marker() {
    let mut p = Pipeline::new(Arc::new(load_engine(None).unwrap()), 5);
    p.process_line("2024-05-01T10:00:00Z [ERROR] svc - boom", 1);
    p.process_line("java.lang.RuntimeException: boom", 2);
    for i in 3..20 {
        let out = p.process_line(&format!("\tat a.B.c{}(B.java:{i})", i), i);
        assert!(out.records.is_empty(), "未结束前不应冲刷");
    }
    // 用一条正常日志结束堆栈
    let out = p.process_line("2024-05-01T10:00:01Z [INFO] svc - ok", 20);
    let err = out.records.first().expect("应先冲刷异常记录");
    let stack = stack_lines(err);
    assert!(stack.len() <= 6, "上限 5 行 + 至多 1 行截断提示");
    assert!(
        stack.iter().any(|l| l.contains("truncated")),
        "应包含截断提示，实际: {stack:?}"
    );
}

#[test]
fn continuation_inside_normal_text_is_not_mismerged() {
    // 一行正常日志里恰好出现 " at " 但行首没有缩进 → 不是续行
    let mut p = pipeline();
    let out = p.process_line(
        "2024-05-01T10:00:00Z [INFO] svc - meet at noon in lobby",
        1,
    );
    assert!(out.quarantined.is_none());
    let recs = p.finish();
    assert_eq!(recs.len(), 1);
    assert!(recs[0].fields.get("stack").is_none());
}
