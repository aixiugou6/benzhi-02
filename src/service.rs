//! 流式接入层（只用 Rust 标准库，不引入任何运行时/框架）：
//! - stdin 模式：采集器把原始日志行 pipe 进进程，结构化 NDJSON 写到 stdout；
//! - TCP 模式：监听端口，采集器直接发文本流（一行一条），同连接回结构化 NDJSON；
//! - 坏数据统一追加写入隔离文件（NDJSON），绝不静默丢弃。
//!
//! 两种模式都是**逐行**处理、逐行写出，不做整文件缓冲，日志量再大内存占用恒定。

use crate::model::QuarantineEntry;
use crate::pipeline::Pipeline;
use crate::rules::{compile_rules, Engine};
use crate::DEFAULT_RULES_JSON;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 加载规则引擎：`path` 为 None 时使用编译期内置的默认规则集。
pub fn load_engine(path: Option<&Path>) -> Result<Engine, String> {
    let text = match path {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| format!("读取规则文件 {} 失败: {e}", p.display()))?,
        None => DEFAULT_RULES_JSON.to_string(),
    };
    compile_rules(&text)
}

/// 隔离区写入器：多线程（TCP 模式）下共享同一个追加文件。
#[derive(Clone)]
pub struct QuarantineSink {
    inner: Arc<Mutex<BufWriter<File>>>,
    path: String,
}

impl QuarantineSink {
    /// 打开（不存在则创建）隔离文件，统一使用追加模式，多次运行不覆盖历史坏数据。
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(BufWriter::new(file))),
            path: path.to_string(),
        })
    }

    /// 追加一条隔离记录（NDJSON）：行号、原因、原始行。
    pub fn write(&self, entry: &QuarantineEntry) -> std::io::Result<()> {
        let line = json!({
            "line_number": entry.line_number,
            "reason": entry.reason,
            "raw": entry.raw,
        });
        let mut w = self.inner.lock().expect("quarantine lock poisoned");
        writeln!(w, "{line}")?;
        w.flush()
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

/// stdin → stdout 流式处理。返回结束统计。
/// stdout 只输出结构化 NDJSON；统计信息走 stderr，方便直接管道给检索库导入器。
pub fn run_stdin(
    engine: Arc<Engine>,
    quarantine: &QuarantineSink,
    max_stack_lines: usize,
) -> std::io::Result<crate::pipeline::Stats> {
    let stdin = std::io::stdin();
    let input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let mut pipeline = Pipeline::new(engine, max_stack_lines);
    let mut line_number = 0u64;

    for raw in BufReader::new(input).lines() {
        line_number += 1;
        let raw = raw?; // 读到非法 UTF-8 会报错返回，不伪装成正常数据
        let result = pipeline.process_line(&raw, line_number);
        if let Some(q) = result.quarantined {
            quarantine.write(&q)?;
        }
        for record in result.records {
            writeln!(out, "{}", record.to_json())?;
        }
        // 逐行 flush：下游采集/检索管道能立刻收到结果
        out.flush()?;
    }

    for record in pipeline.finish() {
        writeln!(out, "{}", record.to_json())?;
    }
    out.flush()?;
    Ok(pipeline.stats)
}

/// TCP 服务模式：每个连接一个线程、一条独立管线（堆栈状态不跨连接串扰）。
///
/// 协议极简：建立连接后直接发送 UTF-8 文本，`\n` 分隔，每行一条原始日志；
/// 解析后的结构化记录以 NDJSON 逐条写回同一连接；对端关闭写端即结束本次会话。
pub fn run_tcp(
    addr: &str,
    engine: Arc<Engine>,
    quarantine: QuarantineSink,
    max_stack_lines: usize,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    run_tcp_listener(listener, engine, quarantine, max_stack_lines)
}

/// 在已绑定的 listener 上接受连接（测试可绑端口 0 后注入）。
pub fn run_tcp_listener(
    listener: TcpListener,
    engine: Arc<Engine>,
    quarantine: QuarantineSink,
    max_stack_lines: usize,
) -> std::io::Result<()> {
    let local = listener.local_addr()?;
    eprintln!("[logparse] listening on {local}");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".into());
                let engine = Arc::clone(&engine);
                let quarantine = quarantine.clone();
                std::thread::spawn(move || {
                    if let Err(e) =
                        handle_connection(stream, engine, quarantine, max_stack_lines)
                    {
                        eprintln!("[logparse] connection {peer} error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[logparse] accept failed: {e}"),
        }
    }
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    engine: Arc<Engine>,
    quarantine: QuarantineSink,
    max_stack_lines: usize,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    let writer = stream;
    // 每连接独立管线：多行堆栈的状态只在单连接内有效
    let mut pipeline = Pipeline::new(engine, max_stack_lines);
    let mut reader = BufReader::new(reader);
    let mut out = BufWriter::new(writer);
    let mut line_number = 0u64;
    let mut buf = String::new();

    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break, // 对端关闭
            Ok(_) => {
                let raw = buf.trim_end_matches(['\n', '\r']).to_string();
                line_number += 1;
                let result = pipeline.process_line(&raw, line_number);
                if let Some(q) = result.quarantined {
                    quarantine.write(&q)?;
                }
                for record in result.records {
                    writeln!(out, "{}", record.to_json())?;
                }
                out.flush()?;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // 非法 UTF-8：无法还原原始字节，记录明确原因后继续服务
                line_number += 1;
                quarantine.write(&QuarantineEntry {
                    raw: "<non-utf8 bytes>".to_string(),
                    line_number,
                    reason: "invalid utf-8".to_string(),
                })?;
            }
            Err(e) => return Err(e),
        }
    }

    for record in pipeline.finish() {
        writeln!(out, "{}", record.to_json())?;
    }
    out.flush()?;
    eprintln!("[logparse] connection done: {:#?}", pipeline.stats);
    Ok(())
}

/// 把统计结果打到 stderr（stdout 保持纯净 NDJSON）。
pub fn print_stats(stats: &crate::pipeline::Stats, quarantine_path: &str) {
    eprintln!(
        "[logparse] lines_in={lines_in} records_out={records_out} quarantined={quarantined} \
         blank_skipped={blank_skipped} stack_records={stack_records} stack_lines_merged={stack_lines_merged}",
        lines_in = stats.lines_in,
        records_out = stats.records_out,
        quarantined = stats.quarantined,
        blank_skipped = stats.blank_skipped,
        stack_records = stats.stack_records,
        stack_lines_merged = stats.stack_lines_merged,
    );
    eprintln!("[logparse] quarantine file: {quarantine_path}");
}
