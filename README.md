# logparse — 异构日志统一解析服务

各种服务吐出的日志格式互不相同（纯文本、JSON、文本里混 JSON、多行 Java 异常堆栈……）。
本服务把它们**流式**解析成统一的结构化字段（时间、级别、服务名、订单号、堆栈等），
输出 NDJSON，直接管道进检索库（Elasticsearch / OpenSearch / ClickHouse / Loki 等均可）。

- **语言/技术栈**：Rust 2021 edition；模式匹配与字段抽取用 **正则（[`regex`](https://crates.io/crates/regex)，NFA 引擎 + 命名捕获组）**；多行堆栈合并用显式**状态机**（`src/stack.rs`）。
- **无框架、无运行时依赖**：网络接入只用 Rust 标准库（每连接一个线程），不引 async 运行时；规则全部来自 JSON 配置，代码里不写死任何业务日志格式。
- **依赖**：仅 4 个 crates.io 官方发行版 crate（`regex` / `serde` / `serde_json` / `chrono`），由 cargo 自动拉取并校验校验和，无需预装任何中间件。

---

## 1. 安装

### 1.1 前置：安装官方 Rust 工具链

不需要 Docker、不需要数据库。只需要 Rust 1.74+（开发验证用的是 1.98.1）。

**Linux / macOS**（[官方 rustup 发行版](https://www.rust-lang.org/tools/install)）：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"
cargo --version
```

Windows 或离线环境：从 https://www.rust-lang.org/tools/install 下载官方 rustup-init，
或用系统官方发行版包管理器安装 `cargo`（Debian/Ubuntu：`apt install cargo`，版本可能偏旧，推荐 rustup）。

### 1.2 构建

在仓库根目录（含本 README 的目录）：

```bash
cargo build --release
# 二进制位置：target/release/logparse
```

cargo 会自动从 crates.io 下载并编译 `regex` / `serde` / `serde_json` / `chrono`。
若机器不能直连 crates.io，可先在联网机器上 `cargo vendor vendor/`，再配置
`.cargo/config.toml` 的 `source.crates-io.replace-with = "vendored-sources"`（离线标准做法）。

---

## 2. 运行

### 2.1 stdin 流式模式（默认，适合容器 sidecar / 管道）

输入：stdin，每行一条原始日志。输出：stdout，每行一条结构化 JSON（NDJSON）。
坏数据写入隔离文件（NDJSON，追加），运行统计写 stderr。

```bash
# 用内置默认规则集（rules/default.json 在编译期嵌入二进制）
cat samples/mixed.log | ./target/release/logparse \
    --quarantine quarantine.ndjson
```

输出示例（节选）：

```json
{"timestamp":"2024-05-01T10:00:00Z","level":"INFO","service":"order-svc","order_id":"ORD-1","message":"create order ... ok", ...}
{"timestamp":"2024-05-01T10:00:01Z","level":"ERROR","service":"order-svc","message":"create failed","stack":"java.lang.IllegalStateException: ...\n\tat ...","stack_lines":[...],"_rule":"java-text","_source_lines":[3,4,5,6,7,8]}
```

stderr 示例：

```text
[logparse] lines_in=11 records_out=4 quarantined=2 blank_skipped=0 stack_records=1 stack_lines_merged=5
[logparse] quarantine file: quarantine.ndjson
```

### 2.2 TCP 服务模式（采集器直接发数据）

```bash
./target/release/logparse --serve 0.0.0.0:9000 --quarantine quarantine.ndjson
```

协议是极简的**行分隔文本**，采集器不需要任何 SDK。用 Python 验证（不依赖 `nc`；有 `nc` 时也可直接 `printf '...\n' | nc 127.0.0.1 9000`）：

```bash
# 终端 A：起服务
./target/release/logparse --serve 127.0.0.1:9000 --quarantine /tmp/q.ndjson

# 终端 B：发两行日志（含一个 JSON 混文本行），同连接回收结构化 NDJSON
python3 - <<'PY'
import socket
s = socket.create_connection(("127.0.0.1", 9000))
s.sendall(b'2024-05-01T10:00:00Z [INFO] order-svc - hi {"order_id":"ORD-9"}\n')
s.sendall(b'2024-05-01T10:00:01Z [WARN] order-svc - retry order_id=ORD-9\n')
s.shutdown(socket.SHUT_WR)   # 关闭写端，触发服务端冲刷可能暂存的记录
print(s.recv(65536).decode())
PY
```

每个 TCP 连接一条独立解析管线（堆栈状态不跨连接串扰），连接关闭时冲刷暂存记录。

### 2.3 命令行参数

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `--rules <PATH>` | 内置 `rules/default.json` | 规则文件（JSON，见第 4 节） |
| `--quarantine <PATH>` | `quarantine.ndjson` | 坏数据隔离文件，**追加**写入，多次运行不覆盖 |
| `--max-stack-lines <N>` | `200` | 单条堆栈保留的最大行数，超出截断并在尾部标注截断数 |
| `--serve <ADDR>` | 无（stdin 模式） | TCP 监听地址，如 `0.0.0.0:9000` |
| `--check` | — | 只编译规则、打印优先级顺序后退出（上线前自检/CI 用） |
| `-h, --help` | — | 帮助 |

---

## 3. 验证（照做即可跑通）

```bash
# 1) 全部单元测试 + 5 个方面的集成测试 + CLI 端到端测试（会真实启动二进制）
cargo test

# 2) 规则自检
./target/debug/logparse --check   # release 构建后用 target/release/logparse

# 3) 手工跑样例
cargo run -- --quarantine /tmp/q.ndjson < samples/mixed.log
cat /tmp/q.ndjson                 # 查看被隔离的坏数据

# 4) TCP 模式（终端 A 起服务，终端 B 用 Python 发数据；无需 nc）
cargo run --release -- --serve 127.0.0.1:9000 --quarantine /tmp/q.ndjson &
python3 - <<'PY'
import socket
s = socket.create_connection(("127.0.0.1", 9000))
s.sendall(b'2024-05-01T10:00:00Z [INFO] s - hi {"order_id":"X-1"}\n')
s.shutdown(socket.SHUT_WR)
print(s.recv(65536).decode())
PY
```

测试覆盖对应需求的 5 块（`tests/` 目录）：

| 测试文件 | 覆盖方面 |
| --- | --- |
| `tests/matching.rs` | ① 模式匹配与优先级：priority 降序、同优先级确定性、regex/json 两类 matcher、where 条件、规则编译期校验 |
| `tests/extraction.rs` | ② 字段抽取与类型推断：命名捕获组、内嵌 JSON 扫描、字段级正则、多来源回退、auto/显式类型、时间戳/级别归一 |
| `tests/stack_merge.rs` | ③ 多行堆栈合并：异常头回溯挂接、`Caused by`/`... N more`、EOF 冲刷、超长截断、孤立帧不误吞、内联堆栈 |
| `tests/output.rs` | ④ 输出规范化：NDJSON 合法性、核心字段固定顺序、stack+stack_lines、RFC3339/UTC、数字原生类型、溯源元信息 |
| `tests/quarantine.rs` | ⑤ 坏数据隔离：乱码/伪 JSON/孤立帧隔离、空行跳过、落盘追加、混合流统计账平、坏数据不污染后续好数据 |
| `tests/tcp_service.rs` | TCP 服务：真实端口收发、多行堆栈连接关闭时冲刷、连接间状态隔离、坏数据落隔离文件 |
| `tests/cli_e2e.rs` | 端到端：真实二进制 + 混合流，校验 stdout / 隔离文件 / stderr 统计 / `--check` / 坏规则非零退出 |

---

## 4. 规则文件说明（如何加你自己的服务格式）

规则全部是声明式 JSON，**不改代码、不重新设计接口即可支持新日志格式**。
完整示例见 [`rules/default.json`](rules/default.json)（内置规则：Java 文本日志、单行异常、
Python logging、nginx access log、支付服务 JSON、通用 JSON、含信号的纯文本兜底）。

```jsonc
{
  "version": 1,
  "rules": [
    {
      "id": "my-service",            // 唯一 id，写入输出的 _rule
      "priority": 100,               // 越大越先匹配；相同按 id 字典序，保证确定性
      "match": "regex",              // "regex" 或 "json"
      "pattern": "^(?P<timestamp>\\S+)\\s+\\[(?P<level>[A-Z]+)\\]\\s+(?P<message>.*)$",
      "fields": [
        {
          "name": "timestamp",
          "type": "timestamp",       // auto|string|i64|f64|bool|timestamp|level|stack
          "sources": [               // 按顺序尝试，第一个抽取出非空值且类型转换成功的来源生效
            { "type": "group", "group": "timestamp" }
          ]
        },
        {
          "name": "order_id",
          "sources": [
            // 从 message 捕获组文本里扫描内嵌 JSON，取点分路径
            { "type": "json_path", "group": "message", "json_path": "order_id" },
            // 再退化为字段级小正则（必须有名为 value 的捕获组，否则取第 1 组）
            { "type": "regex", "group": "message",
              "pattern": "订单号[:：]?\\s*(?P<value>[A-Z0-9-]{4,})" }
          ]
        }
      ]
    },
    {
      "id": "my-json-service",
      "priority": 50,
      "match": "json",               // 整行是 JSON 对象；json_source 目前只支持 "line"（可省略）
      "where": [                     // 全部满足才命中：equals|not_equals|exists|contains|regex
        { "path": "service", "op": "equals", "value": "my-service" },
        { "path": "trace.id", "op": "exists" }
      ],
      "fields": [
        { "name": "timestamp", "type": "timestamp",
          "sources": [ { "type": "json_path", "json_path": "ts" } ] }
      ]
    }
  ]
}
```

**字段来源（source）类型**：

- `group`：主正则命名捕获组；
- `regex`：在 `group`（省略则整行）文本上再匹配一个小正则，取命名组 `value`（否则第 1 组）；
- `json_path`：点分路径（支持数组下标，如 `items.0.id`）；指定 `group` 时会在该文本里
  用**括号配对扫描**内嵌 JSON 对象（正确处理字符串内的 `{}` 和转义），不依赖写死的包裹格式；
- `constant`：常量（如给 nginx 日志补 `"service": "nginx"`）；
- `stack_inline`：从文本里抽取异常头与打印在同一行的 `at` 帧。

**类型转换规则**：转换失败的字段被丢弃（其余字段不受影响）；时间戳统一输出 UTC RFC3339
（支持 RFC3339、空格分隔、逗号毫秒、`+0800/+08:00` 偏移、nginx CLF、`20240501T120000Z`
紧凑格式、10 位秒/13 位毫秒 epoch；无时区信息按 UTC）；级别统一为
`TRACE/DEBUG/INFO/WARN/ERROR/FATAL`（兼容 WARNING/ERR/CRITICAL/SEVERE 等别名）。

规则文件的错误（JSON 非法、重复 id、坏正则、未知类型/操作符、版本不符）会在启动或
`--check` 时直接报错退出，不会带着坏规则运行。

---

## 5. 输出格式（规范化）

每行一个 JSON 对象，字段约定：

- 核心字段固定顺序：`timestamp` → `level` → `service` → `order_id` → `message` → `stack`，
  缺失的字段**不输出**（不造 null）；其余字段按字典序追加，输出字节确定、便于 diff/测试；
- `stack` 是换行拼接的字符串，同时额外输出 `stack_lines` 数组（全文检索与逐帧分析两用）；
- 数字字段输出为 JSON number（i64/f64），布尔为 bool，不是全部字符串化；
- 元信息：`_rule`（命中规则 id）、`_source_lines`（该记录由原始输入的哪些 1 起始行合并而来，用于溯源）。

隔离文件每行：`{"line_number":N,"reason":"...","raw":"原始行原样"}`。
reason 目前有 `no rule matched`（无规则可解析）与 `orphan stack line`（无归属异常头的孤立堆栈帧）。

---

## 6. 多行堆栈合并的工作方式（状态机）

异常头在真实日志里是消息行**之后**的独立行，首行无法预知堆栈要来，因此状态机
（`src/stack.rs`）采用"正常记录滞后一行"策略：

1. 普通解析出的记录先暂存（Idle），看下一行是什么；
2. 下一行以异常类头开头（`^\s*xxxException|xxxError|xxxThrowable`，会排除 `ErrorCounter`
   这类普通词）→ 回溯挂到暂存记录，进入 Tracing，开始吞堆栈块；
3. Tracing 中识别缩进的 `at 帧`、`Caused by:` / `Suppressed:`、`... N more` /
   `... N common frames omitted`；空行不打断；
4. 任一普通日志行到来 → 先冲刷异常记录，再正常解析新行；
5. 输入 EOF → 冲刷最后一条暂存记录；
6. 没有前导记录的裸帧/缩进异常头 → **隔离而不是吞掉**（它们不属于任何已知异常）；
7. 超过 `--max-stack-lines` 时优先挤掉普通 `at` 帧、尽量保住 `Caused by`/`... N more`，
   并在堆栈尾部标注截断行数。

---

## 7. 目录结构

```text
Cargo.toml            依赖声明（regex / serde / serde_json / chrono，均为 crates.io 官方包）
rules/default.json    内置解析规则集（编译期 include_str! 嵌入，也可用 --rules 外部覆盖）
samples/mixed.log     手工验证用的混合样例
src/
  main.rs             CLI 参数与启动
  lib.rs              库入口（模块说明 + 内置规则常量）
  model.rs            结构化记录/隔离记录模型、输出 JSON 规范化
  types.rs            类型推断与 timestamp/level 归一
  util_json.rs        点分路径 + "JSON 混文本"内嵌对象扫描
  rules.rs            规则加载/编译、优先级匹配、字段抽取
  stack.rs            多行堆栈合并状态机
  pipeline.rs         流式管线（串联各模块 + 统计）
  service.rs          stdin / TCP 接入、隔离文件
tests/                5 块集成测试 + CLI 端到端测试
```

## 8. 接入检索库

stdout 是纯 NDJSON，常见接法：

```bash
# 例：管道给任意 NDJSON 消费方（Filebeat/Vector 的 exec source、Fluentd in_tail 一个 FIFO、
# 或直接用检索库的 bulk 导入器包一层）
./target/release/logparse < raw.log > structured.ndjson
```

本服务职责边界是"原始行 → 结构化 JSON"，不内置具体检索库客户端，避免把部署绑死在某一家；
输出 schema 稳定（第 5 节），任意下游均可消费。
