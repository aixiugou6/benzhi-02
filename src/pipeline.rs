//! 流式处理管线：逐行串联 规则匹配 → 字段抽取 → 堆栈合并 → 输出/隔离。
//!
//! 设计为"喂一行、吐 0~N 条结果"，不把整个输入读进内存，可直接挂在任意流式输入源上
//! （stdin / TCP 连接 / 采集器 SDK）。输入结束务必调用 [`Pipeline::finish`] 冲刷滞后记录。

use crate::model::{QuarantineEntry, Record};
use crate::rules::Engine;
use crate::stack::StackMerger;
use std::sync::Arc;

/// 运行期计数器，结束时打到 stderr，便于运维核对"进/出/隔离"账平。
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub lines_in: u64,
    pub records_out: u64,
    pub quarantined: u64,
    pub blank_skipped: u64,
    /// 由多行合并成一条的记录数（source 行数 > 1）。
    pub stack_records: u64,
    /// 被合并进堆栈记录的续行总数（不含头日志行本身）。
    pub stack_lines_merged: u64,
}

/// 单行处理结果。
pub struct LineResult {
    /// 本次新产出可输出的结构化记录（可能含上一条滞后记录的冲刷）。
    pub records: Vec<Record>,
    /// 若本行无法解析/为孤立堆栈行，产生一条隔离记录。
    pub quarantined: Option<QuarantineEntry>,
}

pub struct Pipeline {
    // Arc：TCP 模式下所有连接线程共享同一套已编译规则，规则编译只发生一次
    engine: Arc<Engine>,
    merger: StackMerger,
    pub stats: Stats,
}

impl Pipeline {
    /// `max_stack_lines`：单条堆栈保留的最大行数，超出截断并在尾部标注。
    pub fn new(engine: Arc<Engine>, max_stack_lines: usize) -> Self {
        Self {
            engine,
            merger: StackMerger::new(max_stack_lines),
            stats: Stats::default(),
        }
    }

    /// 处理一行原始日志（不含行尾换行）。行号 1 起始，用于溯源与隔离记录。
    pub fn process_line(&mut self, raw: &str, line_number: u64) -> LineResult {
        self.stats.lines_in += 1;

        // 空行：直接跳过、不隔离；堆栈进行中也不打断帧序列（状态不变）
        if raw.trim().is_empty() {
            self.stats.blank_skipped += 1;
            return LineResult {
                records: Vec::new(),
                quarantined: None,
            };
        }

        let hit = self.engine.match_line(raw);
        let result = self.merger.accept(hit, raw, line_number);

        let mut records = Vec::new();
        for r in result.emit {
            self.count_record(&r);
            records.push(r);
        }

        let quarantined = if let Some(reason) = result.quarantine_reason {
            self.stats.quarantined += 1;
            Some(QuarantineEntry {
                raw: raw.to_string(),
                line_number,
                reason: reason.to_string(),
            })
        } else {
            None
        };

        LineResult {
            records,
            quarantined,
        }
    }

    /// 输入结束：冲刷状态机里滞后的普通记录或最后一条堆栈记录。
    pub fn finish(&mut self) -> Vec<Record> {
        let mut out = Vec::new();
        if let Some(r) = self.merger.finish() {
            self.count_record(&r);
            out.push(r);
        }
        out
    }

    fn count_record(&mut self, r: &Record) {
        self.stats.records_out += 1;
        if r.source_line_numbers.len() > 1 {
            self.stats.stack_records += 1;
            self.stats.stack_lines_merged +=
                (r.source_line_numbers.len() as u64).saturating_sub(1);
        }
    }
}
