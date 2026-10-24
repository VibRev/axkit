# axkit

Agent-first CLI kit: bounded output with spill files, Error/hint reports, a per-user daemon, a stdio MCP server, and an eval runner for claude, codex and grok.

axkit 是面向 agent 的 CLI 组件库，提供输出与截断、错误、进度、daemon、agent 识别和 stdio MCP，与具体领域无关。[idax](https://github.com/VibRev/idax) 和 [bnx](https://github.com/VibRev/bnx) 共有的逆向部分在 [rex](https://github.com/VibRev/rex)。

契约写在 [`docs/CONVENTIONS.md`](https://github.com/VibRev/axkit/blob/main/docs/CONVENTIONS.md)，本 crate 是它的实现，代码注释里的 `§` 编号指向该文档。axkit 不依赖任何引擎 SDK，测试在任何机器上都能跑。

## 快速开始

```rust
use axkit::{agent::Mode, output::{Sink, SpillDir}};

let window = Mode::detect().default_window();   // agent 下约 8 KiB，人类终端不限
let spill = SpillDir::for_tool("idax");
let mut sink = Sink::new(std::io::stdout().lock(), window, Some(&spill));
sink.comment("# functions");
for f in functions {
    sink.record(&format!("{:#x} | {} | {}", f.addr, f.name, f.size));
}
let summary = sink.finish()?;   // 截断时最后一行：# records 1-40 of 1523 shown; full output saved to <file>
```

`examples/echo_tool.rs` 演示一个工具怎样接入 daemon：

```sh
cargo run --example echo_tool -- hello                  # 第一次调用自动拉起 daemon
AXECHO_DAEMON=off cargo run --example echo_tool -- hello  # 不经 daemon，在本进程执行
cargo run --example echo_tool -- daemon status
```

## 模块

| 模块 | 内容 | 约定 |
|---|---|---|
| `output` | `Window`（offset、limit、max-bytes）；`Sink`：记录与 `#` 注释、截断和末行提示，`json` 输出的单个对象不截断；`SpillDir`：截断后的全文文件，按内容命名，有总预算 | §3–§5 |
| `exit` | `Exit` 退出码、`ErrorKind`、`Failure`（文本或单行 JSON 两种报告，clap 解析错误统一为 usage）、`wants_json` | §6 |
| `agent` | `Mode::detect`：按 harness 的环境变量判断是不是 agent（`Human` / `Agent`），给出默认窗口；`AXKIT_AGENT=on\|off` 强制 | §10 |
| `skills` | 编译期打包的 skill（`Skill` / `SkillFile`），`Skill::version` 读 frontmatter 的 `metadata.version`。工具只把 skill 原样交出，不负责安装 | §9.1 |
| `mcp` | stdio MCP server：JSON-RPC 2.0 按行收发、协议版本协商；`Handler`（`tools` / `call`）和 `schema` 构造辅助 | §9.2 |
| `daemon` | 客户端 `forward` / `call`：连接 daemon，连不上就拉起；转发 argv、cwd、相关环境变量、TTY 标志和 stdin，回传 stdout、stderr、进度、退出码。服务端 `serve`（`Handler` trait）。另有 `status`、`stop` | §8 |

`daemon::Config` 里工具要自己选的项：

- `wants_stdin`：哪些命令要读 stdin。
- `idle`：空闲多久退出，默认 15 分钟。
- `start_wait`、`stop_wait`：等 daemon 启动、停止的时间，默认分别为 5 秒、30 秒。
- `runtime_dir`：socket 所在目录。

## 评测

[`eval/`](https://github.com/VibRev/axkit/tree/main/eval) 是独立的 crate `axkit-eval`，同样与领域无关。它用真实的 agent harness（claude、codex、grok）跑任务集，统计成功率、轮次、分类型 token、成本、工具输出量和截断次数。用法见 [`eval/README.md`](https://github.com/VibRev/axkit/blob/main/eval/README.md)，最近一次报告在 [`eval/reports/`](https://github.com/VibRev/axkit/tree/main/eval/reports)。

## 平台

- Linux、macOS：daemon 用 Unix socket。
- 其他平台：`forward` 返回 `None`，命令在本进程执行，没有 daemon。

## 待做

- Windows 上的 daemon 传输
- conformance 测试 helper

## 开发

```sh
cargo clippy --all-targets
cargo test
cargo fmt --check
```

## 许可

Apache-2.0。
