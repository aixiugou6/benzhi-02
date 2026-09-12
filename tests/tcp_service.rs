//! TCP 服务模式集成测试：绑定随机端口，真实收发，验证每连接独立管线与 EOF 冲刷。

use logparse::service::{run_tcp_listener, QuarantineSink};
use logparse::{pipeline::Stats, rules::Engine};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

fn start_server() -> (u16, std::path::PathBuf) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let engine: Arc<Engine> = Arc::new(logparse::service::load_engine(None).unwrap());
    let dir = std::env::temp_dir().join(format!(
        "logparse-tcp-{}-{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let qpath = dir.join("q.ndjson");
    let sink = QuarantineSink::open(qpath.to_str().unwrap()).unwrap();

    std::thread::spawn(move || {
        let _: Result<(), _> = run_tcp_listener(listener, engine, sink, 200);
    });
    std::thread::sleep(Duration::from_millis(100));
    (port, qpath)
}

fn send(port: u16, payload: &str) -> Vec<serde_json::Value> {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    sock.write_all(payload.as_bytes()).unwrap();
    // shutdown 写端触发服务端 EOF → 冲刷暂存记录
    sock.shutdown(std::net::Shutdown::Write).unwrap();

    let reader = BufReader::new(sock);
    reader
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect()
}

#[test]
fn tcp_parses_single_and_multiline_records() {
    let (port, qpath) = start_server();
    let payload = "\
2024-05-01T10:00:00Z [INFO] order-svc - create order {\"order_id\":\"TCP-1\"} ok
2024-05-01T10:00:01Z [ERROR] order-svc - boom
java.lang.RuntimeException: boom
\tat a.B.c(B.java:1)
";
    let recs = send(port, payload);
    assert_eq!(recs.len(), 2, "TCP 连接关闭时应冲刷出 2 条记录");
    assert_eq!(recs[0]["order_id"], serde_json::json!("TCP-1"));
    assert!(recs[1]["stack"].as_str().unwrap().contains("RuntimeException"));
    assert_eq!(recs[1]["_source_lines"], serde_json::json!([2, 3, 4]));

    // 坏数据落隔离文件（给服务端一点刷盘时间）
    std::thread::sleep(Duration::from_millis(100));
    let _ = qpath; // 本连接无坏数据
}

#[test]
fn tcp_quarantines_garbage_and_connections_are_independent() {
    let (port, qpath) = start_server();

    // 连接 A：先发半条堆栈的异常头，然后直接断开——连接私有状态，不能影响连接 B
    let recs_a = send(
        port,
        "2024-05-01T10:00:00Z [ERROR] s - boom\njava.lang.RuntimeException: x\n",
    );
    assert_eq!(recs_a.len(), 1, "异常记录在连接 A 关闭时冲刷");

    // 连接 B：全新状态，第一行孤立帧必须隔离，而不是并到 A 的异常上
    let recs_b = send(port, "\tat ghost.X.y(X.java:1)\n");
    assert!(recs_b.is_empty(), "孤立帧不应产出记录");

    std::thread::sleep(Duration::from_millis(150));
    let q = std::fs::read_to_string(&qpath).unwrap();
    let qlines: Vec<serde_json::Value> = q
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        qlines.iter().any(|v| v["reason"] == "orphan stack line"),
        "连接 B 的孤立帧应被隔离: {q}"
    );

    // 正常行（含一个乱码）走第三个连接，确认服务仍正常
    let recs_c = send(port, "@@@@\n2024-05-01T10:00:02Z [INFO] s - alive\n");
    assert_eq!(recs_c.len(), 1);
    assert_eq!(recs_c[0]["message"], serde_json::json!("alive"));
}

#[allow(dead_code)]
fn assert_stats_shape(_s: &Stats) {}
