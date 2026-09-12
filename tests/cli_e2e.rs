//! 端到端测试：真实启动 logparse 二进制（stdin 模式），喂入混合流，
//! 校验 stdout 的 NDJSON、隔离文件与 stderr 统计。这是最接近采集器接入的验证。

use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_logparse")
}

struct CliRun {
    stdout: String,
    stderr: String,
    status_ok: bool,
    qpath: String,
    tmpdir: std::path::PathBuf,
}

fn run_stdin(input: &str, max_stack: Option<usize>) -> CliRun {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    );
    let tmpdir = std::env::temp_dir().join(format!("logparse-e2e-{unique}"));
    std::fs::create_dir_all(&tmpdir).unwrap();
    let qpath = tmpdir.join("quarantine.ndjson");

    let mut cmd = Command::new(bin());
    cmd.arg("--quarantine").arg(&qpath);
    if let Some(n) = max_stack {
        cmd.arg("--max-stack-lines").arg(n.to_string());
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();

    CliRun {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        status_ok: out.status.success(),
        qpath: qpath.to_string_lossy().to_string(),
        tmpdir,
    }
}

impl CliRun {
    fn records(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("stdout 每行必须是合法 JSON"))
            .collect()
    }
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

const MIXED: &str = "\
2024-05-01T10:00:00Z [INFO] order-svc - create order {\"order_id\":\"ORD-1\",\"amount\":99.8} ok
@@@@ totally broken @@@@
2024-05-01T10:00:01Z [ERROR] order-svc - create failed
\tjava.lang.IllegalStateException: stock timeout
\tat com.shop.order.OrderService.create(OrderService.java:88)
Caused by: java.sql.SQLException: lock wait
\tat com.shop.db.Lock.acquire(Lock.java:201)
\t... 12 more
2024-05-01T10:00:02Z [INFO] order-svc - recovered
{\"ts\":\"2024-05-01T20:00:00+08:00\",\"level\":\"warning\",\"service\":\"payment-service\",\"order_id\":\"A-1001\",\"amount\":42.5,\"currency\":\"CNY\"}
\tat com.ghost.Orphan.nope(Orphan.java:1)
";

#[test]
fn end_to_end_mixed_stream() {
    let run = run_stdin(MIXED, None);
    assert!(run.status_ok, "进程应正常退出，stderr={}", run.stderr);

    let recs = run.records();
    // 1) order JSON 混文本  2) 异常合并  3) recovered  4) 整行 JSON
    assert_eq!(recs.len(), 4, "stdout 应有 4 条记录，stdout={}", run.stdout);

    // 记录 1：文本 + 内嵌 JSON
    assert_eq!(recs[0]["_rule"], json!("java-text"));
    assert_eq!(recs[0]["order_id"], json!("ORD-1"));
    assert_eq!(recs[0]["level"], json!("INFO"));

    // 记录 2：多行堆栈合并
    assert_eq!(recs[1]["_rule"], json!("java-text"));
    assert_eq!(recs[1]["level"], json!("ERROR"));
    let stack_lines = recs[1]["stack_lines"].as_array().unwrap();
    assert!(stack_lines.iter().any(|l| l
        .as_str()
        .unwrap()
        .contains("IllegalStateException")));
    assert!(stack_lines
        .iter()
        .any(|l| l.as_str().unwrap().starts_with("Caused by:")));
    assert!(stack_lines
        .iter()
        .any(|l| l.as_str().unwrap().trim() == "... 12 more"));
    let src = recs[1]["_source_lines"].as_array().unwrap();
    assert_eq!(src.len(), 6, "异常记录合并了 6 个原始行");

    // 记录 3：恢复
    assert_eq!(recs[2]["message"], json!("recovered"));

    // 记录 4：整行 JSON，时区/级别归一
    assert_eq!(recs[3]["_rule"], json!("json-payment-service"));
    assert_eq!(recs[3]["timestamp"], json!("2024-05-01T12:00:00Z"));
    assert_eq!(recs[3]["level"], json!("WARN"));
    assert_eq!(recs[3]["amount"], json!(42.5));

    // 隔离文件：乱码 + 孤立堆栈帧，共 2 条
    let qcontent = std::fs::read_to_string(&run.qpath).unwrap();
    let qlines: Vec<serde_json::Value> = qcontent
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(qlines.len(), 2);
    assert_eq!(qlines[0]["reason"], json!("no rule matched"));
    assert!(qlines[0]["raw"].as_str().unwrap().contains("broken"));
    assert_eq!(qlines[1]["reason"], json!("orphan stack line"));

    // stderr 统计账平：11 行输入 = 4 记录 + 2 隔离 + 0 空行 + 5 合并续行
    assert!(run.stderr.contains("lines_in=11"), "{}", run.stderr);
    assert!(run.stderr.contains("records_out=4"), "{}", run.stderr);
    assert!(run.stderr.contains("quarantined=2"), "{}", run.stderr);
    assert!(run.stderr.contains("stack_lines_merged=5"), "{}", run.stderr);

    run.cleanup();
}

#[test]
fn check_flag_validates_rules() {
    let out = Command::new(bin()).arg("--check").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("java-text"));
    // java-text(100) 必须排在 plain-text-fallback(10) 前面
    assert!(s.find("java-text").unwrap() < s.find("plain-text-fallback").unwrap());
}

#[test]
fn bad_rules_file_fails_cleanly() {
    let unique = std::process::id();
    let tmpdir = std::env::temp_dir().join(format!("logparse-e2e-badrules-{unique}"));
    std::fs::create_dir_all(&tmpdir).unwrap();
    let bad = tmpdir.join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    let out = Command::new(bin())
        .arg("--rules")
        .arg(&bad)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!out.status.success(), "坏规则文件必须非零退出");
    let _ = std::fs::remove_dir_all(&tmpdir);
}
