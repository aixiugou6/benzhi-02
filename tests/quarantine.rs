//! 集成测试 5/5：坏数据隔离。
//!
//! 覆盖：
//! - 纯乱码行 → quarantine，原因 "no rule matched"，且不会产出错误结构的记录；
//! - 没有归属异常头的孤立堆栈帧 → quarantine，原因 "orphan stack line"；
//! - 截断的伪 JSON（`{"broken`）→ quarantine；
//! - 空行跳过、不隔离；
//! - QuarantineSink 落盘为 NDJSON，raw 原样保留、带行号与原因（追加模式不覆盖历史）；
//! - 混合流下统计账平：lines_in = records_out + quarantined + blank + 合并续行。

use logparse::pipeline::Pipeline;
use logparse::service::{load_engine, QuarantineSink};
use serde_json::json;
use std::sync::Arc;

fn pipeline() -> Pipeline {
    Pipeline::new(Arc::new(load_engine(None).unwrap()), 200)
}

#[test]
fn garbage_line_is_quarantined() {
    for bad in ["@@@###$$$%%%", "qwlkjqwelkqj  zzzzz", "    \t\t  --- ??? "] {
        let mut p = pipeline();
        let out = p.process_line(bad, 1);
        assert!(out.records.is_empty(), "乱码不应产出记录: {bad}");
        let q = out.quarantined.expect("乱码必须被隔离");
        assert_eq!(q.reason, "no rule matched");
        assert_eq!(q.raw, bad);
        assert_eq!(q.line_number, 1);
    }
}

#[test]
fn truncated_pseudo_json_is_quarantined() {
    let mut p = pipeline();
    let out = p.process_line(r#"{"broken": "json without end"#, 7);
    assert!(out.records.is_empty());
    let q = out.quarantined.unwrap();
    assert_eq!(q.line_number, 7);
    assert_eq!(q.reason, "no rule matched");
}

#[test]
fn orphan_stack_frame_is_quarantined() {
    let mut p = pipeline();
    let out = p.process_line("\tat com.ghost.Foo.bar(Foo.java:9)", 1);
    assert!(out.records.is_empty());
    let q = out.quarantined.unwrap();
    assert_eq!(q.reason, "orphan stack line");
    assert!(q.raw.contains("Foo.bar"));
}

#[test]
fn blank_lines_are_skipped_not_quarantined() {
    let mut p = pipeline();
    for (i, line) in ["", "   ", "\t\t"].iter().enumerate() {
        let out = p.process_line(line, (i + 1) as u64);
        assert!(out.records.is_empty());
        assert!(out.quarantined.is_none());
    }
    assert_eq!(p.stats.blank_skipped, 3);
    assert_eq!(p.stats.quarantined, 0);
}

#[test]
fn sink_persists_ndjson_and_appends() {
    let dir = std::env::temp_dir().join(format!("logparse-q-{}-{}", std::process::id(), "sink"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("q.ndjson");

    {
        let sink = QuarantineSink::open(path.to_str().unwrap()).unwrap();
        sink.write(&logparse::model::QuarantineEntry {
            raw: "@@@".into(),
            line_number: 1,
            reason: "no rule matched".into(),
        })
        .unwrap();
    }
    // 再次打开（模拟进程重启）：追加，不覆盖
    {
        let sink = QuarantineSink::open(path.to_str().unwrap()).unwrap();
        sink.write(&logparse::model::QuarantineEntry {
            raw: "\tat x.Y.z(Y.java:1)".into(),
            line_number: 2,
            reason: "orphan stack line".into(),
        })
        .unwrap();
    }

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "两条坏数据都应保留（追加模式）");
    assert_eq!(lines[0]["raw"], json!("@@@"));
    assert_eq!(lines[0]["line_number"], json!(1));
    assert_eq!(lines[1]["reason"], json!("orphan stack line"));
    assert_eq!(lines[1]["raw"], json!("\tat x.Y.z(Y.java:1)"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_stream_stats_balance() {
    let input = [
        "2024-05-01T10:00:00Z [INFO] svc - start",          // 1 普通
        "@@@ garbage @@@",                                    // 2 隔离
        "",                                                   // 3 空行
        "2024-05-01T10:00:01Z [ERROR] svc - boom",          // 4 异常头日志
        "java.lang.RuntimeException: boom",                 // 5 续行
        "\tat a.B.c(B.java:1)",                             // 6 续行
        "2024-05-01T10:00:02Z [INFO] svc - done",           // 7 普通（冲刷异常）
    ];
    let mut p = pipeline();
    let mut recs = 0;
    let mut q = 0;
    for (i, line) in input.iter().enumerate() {
        let out = p.process_line(line, (i + 1) as u64);
        recs += out.records.len();
        q += out.quarantined.iter().count();
    }
    recs += p.finish().len();
    let s = &p.stats;
    assert_eq!(recs as u64, s.records_out);
    assert_eq!(q as u64, s.quarantined);
    assert_eq!(s.lines_in, 7);
    assert_eq!(s.records_out, 3); // start / 异常合并 / done
    assert_eq!(s.quarantined, 1);
    assert_eq!(s.blank_skipped, 1);
    assert_eq!(s.stack_records, 1);
    assert_eq!(s.stack_lines_merged, 2);
    // 账平：输入行 = 输出记录 + 隔离 + 空行 + 被合并的续行
    assert_eq!(
        s.lines_in,
        s.records_out + s.quarantined + s.blank_skipped + s.stack_lines_merged
    );
}

#[test]
fn bad_lines_do_not_corrupt_following_good_lines() {
    // 乱码之后的正常日志必须照常解析，状态不被污染（普通记录滞后一行，EOF 时冲刷）
    let mut p = pipeline();
    assert!(p.process_line("@@@@", 1).quarantined.is_some());
    assert!(p.process_line("\tat x.Y.z(Y.java:1)", 2).quarantined.is_some());
    let out = p.process_line("2024-05-01T10:00:00Z [INFO] svc - alive", 3);
    assert!(out.quarantined.is_none());
    let tail = p.finish();
    let all: Vec<_> = out.records.iter().chain(tail.iter()).collect();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].fields.get("message").unwrap().to_json(),
        json!("alive")
    );
}
