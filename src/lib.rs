//! # logparse —— 异构日志统一解析库
//!
//! 模块划分：
//! - [`model`]：结构化记录与隔离记录的数据模型、输出 JSON 规范化；
//! - [`types`]：字段类型推断、时间戳/级别归一；
//! - [`util_json`]：点分路径与内嵌 JSON 扫描；
//! - [`rules`]：规则文件加载/编译、优先级匹配、字段抽取；
//! - [`stack`]：多行 Java 堆栈合并状态机；
//! - [`pipeline`]：把以上模块串起来的流式管线；
//! - [`service`]：stdin / TCP 两种流式接入方式与隔离落盘。
//!
//! 典型用法见 `src/main.rs` 或 `tests/` 下的集成测试。

pub mod model;
pub mod pipeline;
pub mod rules;
pub mod service;
pub mod stack;
pub mod types;
pub mod util_json;

/// 内置默认规则集（`rules/default.json`，编译期嵌入，开箱即用）。
pub const DEFAULT_RULES_JSON: &str = include_str!("../rules/default.json");
