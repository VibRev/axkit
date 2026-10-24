# axkit

agent-first CLI 组件：输出、截断、错误、进度、daemon、agent 识别。与领域无关，`idax` 与 `bnx` 共有的逆向部分在 [rex](../rex)。契约写在 [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md)，本 crate 是它的实现；代码注释里的 `§` 编号指向该文档。

axkit 不依赖任何引擎 SDK，测试在任何机器上都能跑。

## 模块

| 模块 | 内容 | 约定 |
|---|---|---|
| `output` | `Window`（offset、limit、max-bytes）、`Sink`（记录与 `#` 注释、截断、末行提示；`json` 输出不截断的单个对象）、`SpillDir`（截断后的全文文件，按内容命名，有总预算） | §3–§5 |
| `exit` | `Exit` 退出码、`ErrorKind`、`Failure`（文本与单行 JSON 两种报告，clap 解析错误统一为 usage）、`wants_json`（参数解析失败时判断要不要 JSON） | §6 |
| `agent` | `Mode::detect`：按 harness 环境变量识别是不是 agent（`Human` / `Agent`），给出默认窗口（agent 为 8 KiB）；`AXKIT_AGENT=on\|off` 强制 | §10 |
| `skills` | `Skill` / `SkillFile`（编译期打包的 skill）、`Skill::file`、`Skill::version`（frontmatter 的 `metadata.version`）。工具只把 skill 原样交出，不安装：各家 harness 的目录不同且常变，由 agent 自己装 | §9.1 |
| `mcp` | stdio MCP server：JSON-RPC 2.0 按行收发、协议版本协商；`Handler`（`tools` / `call`）、`schema` 构造辅助 | §9.2 |
| `daemon` | `forward` / `call`（客户端：连接或拉起 daemon，转发 argv / cwd / 相关环境变量 / TTY 标志，需要时连同 stdin，回传 stdout、stderr、进度、退出码；连不上时以 `target_unavailable` 报错并说明原因）、`serve`（服务端，`Handler` trait，`status` 能拿到剩余空闲时间）、`status`、`stop`（等被停的 daemon 退出后返回）；`Request::canonical_path` 把客户端给的路径规范化 | §8 |
| `eval/`（crate `axkit-eval`） | 评测运行器：任务集 TOML、harness 适配（claude / codex / grok）、分类型 token 与重算成本、判定、`run` / `summary` / `compare` / `check`；见 [`eval/README.md`](eval/README.md) | §11 |

## 用法

```rust
use axkit::{agent::Mode, output::{Sink, SpillDir}};

let window = Mode::detect().default_window();
let spill = SpillDir::for_tool("idax");
let mut sink = Sink::new(std::io::stdout().lock(), window, Some(&spill));
sink.comment("# functions");
for f in functions {
    sink.record(&format!("{:#x} | {} | {}", f.addr, f.name, f.size));
}
let summary = sink.finish()?;
```

`Config` 里与工具相关的选择：

- `wants_stdin`：哪些命令行要读 stdin，客户端读完后随请求发给 daemon，`Request::stdin` 里取；
- `idle`（默认 15 分钟）、`start_wait`（5 秒）、`stop_wait`（30 秒）、`runtime_dir`。

## 待做

- `daemon`：Windows named pipe（目前非 Unix 平台上 `forward` 返回 `None`，命令在本进程执行）
- conformance 测试 helper
- `axkit-eval`：codex 的用户 skill 隔离、grok 的 `trusted_folders.toml` 写入

示例工具 `examples/echo_tool.rs` 演示了一个工具如何接入 daemon：

```sh
cargo run --example echo_tool -- hello        # 第一次调用自动拉起 daemon
AXECHO_DAEMON=off cargo run --example echo_tool -- hello
cargo run --example echo_tool -- daemon status
```

## 开发

```sh
cargo clippy --all-targets
cargo test
cargo fmt --check
```

## 许可

Apache-2.0。
