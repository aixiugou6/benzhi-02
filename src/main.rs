//! logparse 命令行入口。
//!
//! 用法：
//! ```text
//! logparse [--rules <规则文件>] [--quarantine <隔离文件>]
//!          [--max-stack-lines <N>] [--serve <监听地址>] [--check]
//! ```
//! - 默认（无 --serve）：stdin 读原始日志行，stdout 逐行写结构化 NDJSON，统计写 stderr；
//! - `--serve 0.0.0.0:9000`：TCP 服务模式；
//! - `--check`：只编译规则、打印优先级顺序并退出（用于上线前校验规则文件）。

use logparse::service::{load_engine, print_stats, run_stdin, run_tcp, QuarantineSink};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

struct Args {
    rules: Option<PathBuf>,
    quarantine: String,
    max_stack_lines: usize,
    serve: Option<String>,
    check: bool,
}

const USAGE: &str = "\
logparse — 异构日志统一解析服务

USAGE:
    logparse [OPTIONS]

OPTIONS:
        --rules <PATH>             规则文件（默认使用编译内置的 rules/default.json）
        --quarantine <PATH>        坏数据隔离文件（NDJSON，追加写入）[默认: quarantine.ndjson]
        --max-stack-lines <N>      单条堆栈保留的最大行数 [默认: 200]
        --serve <ADDR>             TCP 服务模式，监听 ADDR，如 0.0.0.0:9000
                                   缺省时为 stdin → stdout 流式模式
        --check                    只校验并编译规则，打印优先级顺序后退出
    -h, --help                     显示本帮助
";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        rules: None,
        quarantine: "quarantine.ndjson".to_string(),
        max_stack_lines: 200,
        serve: None,
        check: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--rules" => args.rules = Some(PathBuf::from(next_value(&mut it, "--rules")?)),
            "--quarantine" => args.quarantine = next_value(&mut it, "--quarantine")?,
            "--max-stack-lines" => {
                let v = next_value(&mut it, "--max-stack-lines")?;
                args.max_stack_lines = v
                    .parse()
                    .map_err(|_| format!("--max-stack-lines 需要正整数，得到: {v}"))?;
                if args.max_stack_lines == 0 {
                    return Err("--max-stack-lines 必须大于 0".into());
                }
            }
            "--serve" => args.serve = Some(next_value(&mut it, "--serve")?),
            "--check" => args.check = true,
            other => return Err(format!("未知参数: {other}\n\n{USAGE}")),
        }
    }
    Ok(args)
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} 缺少值"))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误: {e}");
            return ExitCode::from(2);
        }
    };

    let engine = match load_engine(args.rules.as_deref()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("规则加载失败: {e}");
            return ExitCode::from(2);
        }
    };

    // --check：规则自检后退出，不读任何数据
    if args.check {
        println!("规则编译成功，按优先级（高→低）顺序：");
        for (i, id) in engine.rule_ids_in_order().iter().enumerate() {
            println!("  {:2}. {}", i + 1, id);
        }
        return ExitCode::SUCCESS;
    }

    let quarantine = match QuarantineSink::open(&args.quarantine) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("打开隔离文件 {} 失败: {e}", args.quarantine);
            return ExitCode::from(2);
        }
    };

    match args.serve {
        Some(addr) => {
            if let Err(e) = run_tcp(&addr, engine, quarantine, args.max_stack_lines) {
                eprintln!("TCP 服务退出: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => match run_stdin(engine, &quarantine, args.max_stack_lines) {
            Ok(stats) => print_stats(&stats, quarantine.path()),
            Err(e) => {
                eprintln!("处理失败: {e}");
                return ExitCode::FAILURE;
            }
        },
    }
    ExitCode::SUCCESS
}
