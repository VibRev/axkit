# axkit-eval

A domain-agnostic eval runner: real agent harnesses (claude, codex, grok) on task suites, measuring pass rate, turns, tokens by type, cost and tool output.

一个与领域无关的评测运行器，对应 [axkit 约定](https://github.com/VibRev/axkit/blob/main/docs/CONVENTIONS.md) §11：拿一组任务，用真实的 agent harness 跑多轮，量出成功率、轮次、**分类型 token 与成本**、工具输出字节和被截断的次数。idax 和 bnx 在各自的 `eval/` 下提供任务集与 fixture，这里只管「怎么跑、怎么量」。运行器只认识任务、分组、工作区、事件流、token 和成本，与逆向无关。

## 评测结论

完整数据见 [评测报告（2026-10-24）](https://github.com/VibRev/axkit/blob/main/eval/reports/2026-10-24-slim.md)。那次对比覆盖 idax / bnx 的 CLI、CLI + skill、自带 MCP，以及官方或第三方的 MCP server，同一台机器、同样 7 个任务、三个 harness，每组跑一次。

1. **省 token 的关键是代码模式加很小的工具面，CLI 还是 MCP 不是关键。** 自带 MCP 只有 5 个工具，定义约 2 KB。在 claude 上它比我们自己的 CLI 少约 1/3 token，比官方 ida-mcp 少 40–50%，只有大 schema MCP（ida-headless-mcp、bn-headless-mcp）的 1/4 到 1/7。
2. **prelude 不需要 helper。** 去掉 helper 之后，agent 直接写原生 API，成功率不变，大多数组的 token 持平或下降。codex 的 token 降了 17–30%，耗时减半。
3. **输出要有上限。** 官方 ida-mcp 不截断输出，codex 和 grok 拿到的工具输出是我们的 2–5 倍。
4. **API 交给 `ref <words>` 现查**：它直接检索安装目录里的 Python API，返回签名和 `文件:行号`，取代了 helper。

每个会话总 token 的中位数如下（右侧两列是官方或第三方 MCP）：

| 引擎 / harness | CLI | CLI + skill | 自带 MCP | 官方 / 第三方 MCP |
|---|---|---|---|---|
| IDA / claude | 47.3k | 48.0k | **32.3k** | ida-mcp 53.7k；ida-headless 226k |
| IDA / codex | 82.0k | 68.5k | 98.7k | ida-mcp 123.5k；ida-headless 237.5k |
| IDA / grok | 56.7k | 60.5k | **50.4k** | ida-mcp 107.6k；ida-headless 125.9k |
| BN / claude | 47.0k | 37.7k | **31.1k** | bn-headless 118k |
| BN / codex | 76.4k | 66.4k | 95.7k | bn-headless 130.2k |
| BN / grok | 52.0k | 54.3k | **49.9k** | bn-headless 不可用 |

通过率 125/126（官方和第三方 MCP 那一轮是 138/140）。这套任务几乎每种接法都能做对，所以拉开差距的是成本，而不是能力。

## 命令

```sh
axkit-eval check   a/suite.toml b/suite.toml           # 只加载校验，不运行
axkit-eval run     a/suite.toml b/suite.toml --reps 3 --jobs 8 --per-harness 4 [--group G] [--task T] [--out DIR]
axkit-eval summary DIR/results.jsonl --by harness,skill  # 维度：group | harness | skill | suite | task，可组合
axkit-eval compare a.jsonl b.jsonl --by suite,group,task   # 两份结果按 key 对齐成一张表：两边的通过、轮次、总 token、b/a、工具输出、成本、耗时；只有一边有的 key 另一边记 -
axkit-eval purge                                       # 删掉所有评测在 harness 历史里留下的条目，前后各打印一次清单
axkit-eval replay  a/suite.toml b/suite.toml --streams DIR [--bin DIR] [--out F] [--failing F]   # 重放失败的工具调用
axkit-eval replay  a/suite.toml b/suite.toml --cases F [--bin DIR] [--out F]                     # 重放一份已有的用例集
axkit-eval replay-diff A.jsonl B.jsonl                 # B 里每条用例相对 A 从哪一类变到哪一类
```

结果写在 `--out`（默认 `target/eval-data/`，不进仓库）：`results.jsonl` 每次运行一行；每个 suite 一个子目录，里面是每次运行的 `*.stream.jsonl`（原始事件流）、`*.stderr.txt`、`*.answer.txt` 和工作区 `ws/<suite>.<group>.<task>.<rep>.<run id>/`。

## 调度

- 多个 suite 共用一个调度。**车道**是 (harness, suite, 任务)：同一车道的运行串行，免得一个运行读到另一个进行中的会话历史；不同 harness 的历史分开存放，所以三个 harness 可以同时跑同一个任务。
- `--jobs` 是总并发，`--per-harness` 是每个 harness 的并发上限（grok 14 路被限流过，4 路安全）。
- **内存保护**：启动每个新运行前读可用内存，低于 `--min-free-mib`（默认 1536）就等，不启动。Linux 读 `/proc/meminfo` 的 `MemAvailable`；macOS 用 `vm_stat` 的 free + inactive + speculative + purgeable 页；其它平台读不到，跳过检查并在开头提示一次。
- **限流**按 harness 单独退避（30 s × 次数），这个 harness 冷却时其它 harness 照常调度；重试上限 `--retries`。
- 队列先按次数、再按固定伪随机顺序打散，每条车道先拿到第一次运行，结果可复现。

## replay：失败调用回放

改了工具的报错、提示或输入宽容之后，不必重跑整轮评测：把 agent 当时失败的工具调用原样重放一遍，几分钟就能看到效果。

- **抽取**：从 `--streams` 下的事件流（三个 harness 的格式都支持）里取出 agent 跑过的 shell 命令，保留调用了被测工具（suite 的 `[suite] tool`）的那些。一次调用算失败，当且仅当：输出里有 `Error:`（或 clap 的 `error:`）行——管道会让 harness 看不到退出码，这样也能抓到；或者 harness 标了失败，并且这条命令除了被测工具和 `cd` 没跑别的（失败只能是工具的）。
- **规整**：按顶层 `&&`、`||`、`;`、换行拆成语句，按 `|` 拆成阶段（引号、`$(...)`、注释、here-document 都认）。每个调用了工具的语句只重放工具那一段：去掉 `cd <ws> &&`、后面的过滤阶段（`| head`、`| grep`）和对输出的重定向（`2>&1`、`> file`），让工具的 stdout 和 stderr 全部可见；前面只是喂文本的阶段（`cat`、`echo`、`printf`）保留。旧工作区路径换成 `{ws}`，再按（命令、agent 当时看到的第一条错误）去重，记下出现次数和来自哪些 harness。
- **重放**：每个 suite 一个新工作区，放进全部任务的 fixture，用 suite 的环境（daemon 运行目录、cache 都是新的私有目录）和 `path`（或 `--bin` 指定的另一版构建）顺序执行，每条限时 `--timeout`。
- **归类**：退出码 0 是 `ok`；失败但输出里有 `hint:` 行、或有指向下一步的用法文本（`Usage:`、`--help`）是 `hinted`；其余是 `silent`。目标是 silent 为 0。命令读的 target 或 `-f` 脚本不在重放工作区里（例如 save 之后才有的 `.bndb`、agent 自己写的脚本），说明它依赖运行里前面的步骤，单独重放没有意义：标为 `skipped` 并写明原因，不计入前三类，表里单列；`--failing` 也不收它。
- **基线**：用产生这些事件流的那一版构建重放，`ok` 的那些就是「同一次调用里别的命令失败了」或不可复现的，`--failing F` 只把非 ok 的写进 F，作为以后对比用的用例集；新版本用 `--cases F` 重放，再 `replay-diff` 看每条的变化。

## 任务集格式（TOML）

```toml
[suite]
name = "demo"
tool = "demo"                   # 被测工具的命令名；replay 靠它挑出调用
setup = ["make -C fixtures"]      # 跑任何运行前执行一次（编译 fixture）
path = ["../target/debug"]        # 置于 PATH 之前，agent 以裸名字调用被测工具
teardown = ["tool daemon stop --force || true"]  # 每个运行后执行（停 daemon）
rules = "不准复制 license，不准改 HOME / IDAUSR。"     # 追加到每个 prompt 末尾
tool_phrase = "the demo CLI (already on PATH)"  # prompt 里 {tool} 的默认说法
[suite.env]                       # 每个运行的环境，值可用模板变量
TOOL_RUNTIME_DIR = "{tmp}/rt"

[[task]]
id = "password"
prompt = "在 {ws}/crackme 里找出比较的口令。"
files = ["fixtures/crackme"]      # 复制进工作区
timeout = 900                     # 整个会话的秒数
[task.judge]                      # 给出的检查全部满足才算通过
contains = ["s3cret_42"]          # 最终回答必含
regex = "0x[0-9a-f]+"             # 最终回答需匹配
excludes = ["wrong"]              # 最终回答不得含（错误候选）
command = ["sh", "{suite}/check.sh"]  # 退出 0 即通过；在工作区里跑，带 ANSWER_FILE

[[group]]
name = "skill"
harness = "claude"                # 适配层：claude | codex | grok
exe = "claude"                    # 可执行文件可配置
model = "sonnet"
skill = { command = "tool skills", name = "tool" }  # 命令输出即 SKILL.md，写进该 harness 的隔离 skill 位置
tools = []                        # 内置工具（harness 自己的名字）；空 = 适配层默认
permission_mode = "auto"          # 权限模式（claude；默认 auto，不设允许列表）
allow = []                        # 额外免问放行的规则（claude；grok 见下）
deny = []                         # 拒绝规则（claude / grok）
sandbox = "axeval"                # OS 沙箱 profile（codex / grok）
args = []                         # 原样追加给 harness 的参数
max_turns = 40
tool = "the tools of the MCP server `ida`"   # 本组 prompt 里 {tool} 的说法（覆盖 tool_phrase）
rules = "只用 MCP server `ida` 的工具。"       # 追加在 suite rules 之后
bare_path = true                  # agent 的 PATH 不放 suite.path（判定命令仍有）
mcp = { name = "ida", command = "{env:AXEVAL_IDA_MCP}", args = ["stdio"], env = { STATE = "{tmp}/s" } }
```

模板变量：`{ws}`（工作区，路径含本次 run id）、`{tmp}`（本次运行私有的 `/tmp/axe-<run id>`）、`{suite}`（suite 目录，判定命令引用 suite 里的脚本时用；判定命令在工作区里跑）、`{tool}`（本组怎么称呼被测工具，见 `tool_phrase` / `tool`）、`{env:NAME}`（运行器自己的环境变量，没设为空；用来给出因机器而异的路径，例如 MCP server 的安装位置，而不把它写进 suite）。判定在 teardown 之前进行，所以判定命令能用本次运行的 daemon 与 runtime 目录重新打开 target。其它 `{...}` 原样保留，所以 prompt 里可以写代码。

## 指标

每个运行从事件流读出：是否完成、轮次、工具调用数、工具报错数、工具输出字节、被截断次数（数工具输出里以 axkit `# records … shown … full output saved to` 结尾的提示）、分类型 token（input / output / cache write / cache read），以及流里自带的成本（留作对照）。**成本在本地按价目表从 token 重算**：流里的 `total_cost_usd` 经过网关时常为 0 或不准，所以不采信它。

价目表：`--price` 指定，否则取 suite 目录旁的 `prices.toml`，再否则用内置底价（haiku/sonnet/opus 三档；未知模型按 opus 计，宁高勿低）。价目随时间变动，内置值仅作兜底，正式评测应提供 `prices.toml`。

## harness 与真实环境

三个适配层：`claude`、`codex`、`grok`。`exe` 可换可执行文件，`args` 原样追加，`sandbox` 选 OS 沙箱 profile。

运行器自己的环境里还认两个变量，用来放因机器而异、不进 suite 的东西：`AXEVAL_<HARNESS>_EXE`（如 `AXEVAL_CLAUDE_EXE`）在组没写 `exe` 时换掉默认可执行文件；`AXEVAL_CREDENTIALS`（`:` 分隔）追加 grok 沙箱要拒绝读写的凭据文件。

**会话跑在用户的真实环境里**：用户自己的 skill、MCP、全局 CLAUDE.md / AGENTS.md 都是真实使用的一部分，不做隔离。

**铁律**：不 login/logout；凭据一律不复制、不移动、不改写；不覆盖 `HOME`；**认证失败即停**——运行器逐行读事件流，第一条认证失败（只看错误字段，不看工具输出）就立刻杀掉会话、停掉整轮。每个 prompt 末尾追加 suite 的 `rules`：不准复制或读取 license，不准改 `HOME`、`IDAUSR`；被测工具本身跑不起来（daemon 起不来、license 或权限错误）时，要报告出来，不能绕过；agent 曾自己把 license 拷到别处来绕过沙箱。脚本自己的错误（名字写错、抛异常）不算在内，rules 明确要求照提示改了继续；否则 agent 会把一次 `LookupError` 当成工具故障直接交卷。

| harness | 命令要点 | 凭据保护 | 每次运行后清理（只删本次的） |
|---|---|---|---|
| claude | `-p --output-format stream-json --verbose --permission-mode auto --no-session-persistence --tools …`（不设允许列表，与 codex 的 `danger-full-access` 对等） | 无 OS 沙箱，做不到内核级拒绝，靠 prompt 规则 | `~/.claude/projects/<工作区路径，非字母数字换成 ->`（目录名必须含本次 run id 才删）；`~/.claude` 下以本次 session id 命名的条目；`history.jsonl` 里 project 为本次工作区的行 |
| codex | `exec --json --ephemeral --skip-git-repo-check -s danger-full-access -c approval_policy="never" -c web_search="disabled"` | 无 OS 沙箱（`workspace-write` 会让被测工具的 daemon 起不来、引擎写不了状态，agent 随即拷 license 绕过），做不到 | `~/.codex/sessions` 下文件名含本次 thread id 的 rollout；`history.jsonl`、`session_index.jsonl` 里含该 id 的行。SQLite 状态库不改（没有精确安全的删法） |
| grok | `-p --output-format streaming-json --trust --always-approve --no-subagents --no-plan --disable-web-search --sandbox axeval --tools run_terminal_cmd,read_file,grep,list_dir` | 工作区里写一个 profile `axeval`：继承 `workspace`，放开 `~/.idapro` `~/.binaryninja` 读写，对**其它 harness 已存在的凭据文件**（`~/.claude/.credentials.json`、`~/.codex/config.toml` 等）做内核级读写拒绝。grok 自己的凭据和 IDA license 不能拒（grok 和引擎自己要读） | `~/.grok/sessions` 下本次 session id 的目录（其上层按工作区命名的目录变空且含 run id 时一并删）；`trusted_folders.toml` 里指向本次工作区的那一张表 |

- 改写追加式文件（`history.jsonl` 等）一律：只在确有匹配行时才写临时文件再 rename，rename 前文件变长就重读重来，不丢别的会话同时追加的行。删除失败只记 warning（`cleanup_warnings`），不影响结果。
- 整轮前后各记一次各存储的条目数（总数 / 评测条目数），结束时核对：评测条目残留必须为 0，评测前就存在的非评测条目一条都不能少；结果打在 `# history check` 一行。
- grok headless 下任何一条不被允许的命令都会取消整个会话，所以用 `--always-approve`。
- `--trust` 会写 `~/.grok/trusted_folders.toml`（沙箱拦不住），由上面的清理删掉。grok 自己读改写这个文件时不加锁，并发启动和清理会互相覆盖，所以所有评测会话（含不同的 `axkit-eval` 进程和 `purge`）共用一把 flock（`$TMPDIR/axeval-$USER-grok-trust.lock`，不放在 `~/.grok` 里）：启动时从 spawn 一直持有到文件里出现本工作区那张表（最多 15 s），清理时持有到改写完成。

**skill 组**：运行器在本次运行的环境里执行 `<tool> skills`，把输出作为**项目级 skill** 写进工作区：claude `<ws>/.claude/skills/<tool>/SKILL.md`，codex `<ws>/.agents/skills/<tool>/SKILL.md`，grok `<ws>/.grok/skills/<tool>/SKILL.md`。不带 skill 的组要求用户全局的 skill 目录（`~/.claude/skills`、`~/.codex/skills`、`~/.agents/skills`、`~/.grok/skills`）里没有被测工具的 skill。

**被测工具隔离**：每次运行有私有的 `/tmp/axe-<run id>`，suite 用它设 `IDAX_RUNTIME_DIR`/`BNX_RUNTIME_DIR` 和 `XDG_CACHE_HOME`，所以并发运行之间不共享 daemon，也不共享 spill 文件；运行结束执行 teardown（`<tool> daemon stop --force`），再删掉整个目录。

## MCP 组：同一套任务换成 MCP server

组里给了 `mcp`，会话就多一个 stdio MCP server，用来把 CLI 与 MCP 放在同一套任务上比：

| harness | 注入方式 | agent 还剩什么 |
|---|---|---|
| claude | 在 `{tmp}` 写 `mcp.json`，`--mcp-config <它> --strict-mcp-config`（只此一个 server，用户自己的不加载），允许 `mcp__<name>` | suite 里 MCP 组设 `tools = ["Read", "Grep", "Glob"]`：没有 Bash |
| codex | `-c mcp_servers.<name>.{command,args,env}`，`startup_timeout_sec=120`、`tool_timeout_sec=900`，`default_tools_approval_mode="approve"`（headless 下 `approval_policy=never` 会拒掉一切需要批准的 MCP 调用） | suite 里设 `sandbox = "read-only"`：shell 只读；MCP server 不受这个沙箱限制。用户 `config.toml` 里自己的 MCP server 照样加载（隔离不了） |
| grok | 工作区的 `.grok/config.toml` 写 `[mcp_servers.<name>]`（`grok mcp add -s project` 的格式；`--trust` 才会启动项目级 server） | suite 里设 `tools = ["read_file", "grep", "list_dir"]`：没有终端 |

- MCP 组的 prompt 不提 CLI（`{tool}` 换成组自己的 `tool`），`bare_path = true` 让 CLI 不在 agent 的 PATH 上；判定命令仍然用 CLI 打开保存出来的数据库。
- 被测工具自己的 MCP 组 `claude-mcp` / `codex-mcp` / `grok-mcp`：server 是 `<tool> mcp`（`{suite}/../target/debug/<tool>`），`<TOOL>_RUNTIME_DIR`、`XDG_CACHE_HOME`、`AXKIT_AGENT` 与 CLI 组相同，所以 suite 的 teardown 照样停掉它起的 daemon。
- **grok 会跳过名字含 `.` 的 MCP 工具**（`tool name ... must contain only letters, digits, underscores, and hyphens`，见 `--debug-file`）。bn-headless-mcp 的工具全是 `binary.functions` 这种名字，所以在 grok 下一个都用不了。
- 通过率之外的列照常统计。截断次数只认 axkit 的末行格式，MCP server 自己的分页或截断不计入。`replay` 只抽 shell 调用，MCP 调用不在其中。

## 价目

`prices.toml`（编进二进制）列出 Claude 各模型的官方价，来源与倍率写在文件头。**没有已核实官方价的模型成本记为 unknown，不编**：目前包括 codex 走的用户自定义 provider 模型和所有 grok 模型；grok 自报的成本另列为 `reported_usd`。`--price` 可叠加自己的行。

## 开发

```sh
cargo clippy --all-targets
cargo test
cargo fmt --check
```

单元测试覆盖套件加载、模板、判定、聚合中位数、价目匹配、命令构造与隔离，以及 replay 的命令拆分、抽取、去重、归类和对比（重放用一个假工具跑真实进程）。`tests/stream.rs` 用一份录制的 `stream-json` 会话（`tests/fixtures/session.jsonl`）端到端验证事件流解析、成本重算与最终回答提取；`tests/replay.rs` 用三个 harness 各一段录制的事件流（`tests/fixtures/calls-*.jsonl`）验证调用提取——都不调用任何真实 harness。

## 许可

Apache-2.0。
