# axkit 约定 —— agent-first CLI 契约

> 版本：**v0.9（草案）**　｜　编制：2026-10-24　｜　v0.9：精简——删掉 head-tail 截断、集合的 JSON 信封（`--json` 只给单个对象）、退出码 2、`Fallback::Local`、按 harness 分的默认值表、未实现的 trace；环境变量只留 `<TOOL>_MAX_BYTES` 与 `<TOOL>_TIMEOUT`；进度第一行在约 3 秒后才打（§7）；v0.8：截断末行写成 `full output saved to <path>`，有文件时不再附 `grep it` / `--offset`（§4.4、§4.6）；v0.7：每个报错都带 hint（§6.3），clap 的用法错误没有提示时也补一条；v0.6：实测 Linux 与 macOS 两个平台；共用的兜底 socket 目录里 `status` / `stop` 只作用于本 family；v0.5：工具不再安装 skill，只把它原样交出（§9.1）；引擎自带的 API 参考指向安装目录（§9）；v0.4：skill 的打包、安装位置、marker 与状态；v0.3：JSON 信封受窗口约束、单个对象不截断；daemon 转发 stdin、不可达时的去向、短路径兜底、`stop` 等待退出、lock 清理
> 适用：任何依赖 axkit 的 CLI 工具（目前是 `idax`、`bnx`）。逆向工具共有、但与 agent 无关的约定（命令集、会话、exec、地址格式）不在这里，见 rex 的 `docs/CONVENTIONS.md`。
> 本文中 **必须** / **禁止** 是硬约定，由 axkit 的 conformance 测试强制；**应当** 是默认做法，偏离要在工具自己的文档里写明理由；**〔待定〕** 是还没定的点，见 §12。

---

## 0. 定位

- **axkit 只管与模型和 harness 一起演进的部分**：输出形态、截断、错误、进度、daemon 传输、渐进式披露的机制、agent 识别、eval。
- **与领域无关。** axkit 里没有「数据库」「地址」「脚本」这些概念；它们属于用 axkit 的工具。
- **不依赖任何引擎 SDK，不含任何脚本运行时**，测试在任何机器上都能跑。
- **CLI 是主入口，MCP 只是外壳。** 工具若提供 MCP，其 `content` 就是 CLI 的文本输出，不另写一套渲染。

---

## 1. 命令形态

| 规则 | 级别 |
|---|---|
| 命令名 kebab-case；不设 `tool` 之类的中间层 | 必须 |
| 全局 flag（`--limit`、`--max-bytes` 等）写在子命令前后都有效（clap `global = true`） | 必须 |
| 环境变量只给确实需要在 harness 配置里统一设置的少数几个，前缀为工具名大写（`BNX_MAX_BYTES=…`）；axkit 自身的开关用 `AXKIT_` 前缀。没人用的 flag 不配环境变量 | 必须 |
| 永远不交互：不提示、不等待 stdin（除非显式给了 `-`）、不开浏览器；stdin 是 `/dev/null` 时行为不变 | 必须 |
| 管理命令：`daemon status\|stop`、`skills [show [FILE]\|list]` | 必须 |

---

## 2. stdout / stderr

| 流 | 只放什么 |
|---|---|
| stdout | 数据，以及属于数据的 `#` 行（表头、合计、截断提示） |
| stderr | 诊断、警告、进度、`hint:`、错误 |

- **禁止**把进度、日志、横幅写进 stdout。
- 非 TTY 时**禁止**输出 ANSI 转义；遵守 `NO_COLOR`。
- stdout 被下游关闭（`| head`）时**必须**安静退出、退出码 0，不打 broken pipe 报错。
- 收到 SIGTERM / SIGINT 时把已写出的部分刷新、不留半行；SIGINT 退出码 130。

---

## 3. 文本输出格式

默认输出是给 agent 和 grep 用的行记录，不是给人看的表格。

```
# functions (3 of 1523, sorted by size)
0x401000 | main | 812 | 37 blocks
0x4013a0 | parse_header | 640 | 22 blocks
0x401720 | sub_401720 | 512 | 9 blocks
# records 1-3 of 1523 shown; full output saved to /home/u/.cache/idax/out/3f2a….txt
```

| 规则 | 级别 |
|---|---|
| 一行一条记录；列用 ` \| ` 分隔；**不做列宽对齐**（对齐的空格是纯 token 成本） | 必须 |
| `#` 开头的行是表头、合计、提示，**不计入记录数**，截断时始终显示 | 必须 |
| 一条记录可以跨多行（一个函数连同它的代码）；截断只截在记录边界，例外见 §4.6 | 必须 |
| 查不到结果时 stdout 为空，在 stderr 给出 `hint:`（§6.3） | 必须 |

---

## 4. 有界输出

**核心规则：禁止静默截断。** 任何被裁掉的输出，都必须在 stdout 末尾说明裁了什么、全文在哪、怎么接着读。

### 4.1 窗口参数

| flag | 环境变量 | 含义 |
|---|---|---|
| `--offset N` | — | 跳过前 N 条记录 |
| `--limit N` | — | 最多显示 N 条 |
| `--max-bytes N` | `<TOOL>_MAX_BYTES` | 最多显示 N 字节（只截在记录边界上） |
| `--full` | — | 关闭窗口，输出全部 |
| `-o FILE` | — | 全文写到 FILE；stdout 仍受窗口约束 |

### 4.2 默认值：人和 agent 不同

- **人（TTY，或未识别为 agent）**：默认不截断，与普通 Unix 工具一致。
- **agent 模式**（§10）：默认启用窗口。**`--max-bytes 8192`、不限条数**，所有 harness 相同，由 eval 实测调整（§11）。默认值必须**小于**宿主 harness 自己的截断阈值，否则我们的截断提示行会被宿主截掉。

### 4.3 截断策略

只有一种：显示前面放得下的记录，加末行提示。全文在文件里，要看尾部由 agent 自己读文件（评测里没有一次用到首尾截断）。

### 4.4 截断后的末行提示（格式固定，conformance 逐字校验）

```
# records <first>-<last> of <total> shown; full output saved to <path>
```

- 有文件可读时，末行只说全文存在哪里，不附读法，也不提 `--offset`：怎么读交给 agent 自己的工具（文件读取、`grep`），`--offset` 翻页写在 `--help` 里。末行越短，越不会被当成要照做的步骤。
- 记录总数未知（流式、提前停止）时写 `of at least <n>`；窗口没有截断但生产方提前停止时写 `# stopped after <n> records; there are more`。
- `--offset` 越过末尾：`# no records after --offset <n> (<total> in all)`。
- 无法写文件（没有 spill 目录、写入失败）：没有文件可读，末行改为给出继续读的办法，写成 `# records <first>-<last> of <total> shown; rerun with --offset <next>`；写入失败时在末尾追加 `; cannot write the full output to <dir>: <原因>`。

### 4.5 截断后写文件（spill）

- 目录：`$XDG_CACHE_HOME/<tool>/out/`（Windows：`%LOCALAPPDATA%\<tool>\out\`），权限 0700，文件 0600。
- 文件名取内容 hash，相同输出不重复写。
- 目录有总字节预算（默认 64 MiB），超出时按 LRU 淘汰。
- 文件内容就是不截断时 stdout 的全部内容，可以直接 grep。

### 4.6 单条超长记录

**第一条要显示的记录**本身就超过字节预算时：显示它开头若干完整行，全文照常写文件，末行在 §4.4 的格式里插入截断位置：

```
# records 1-1 of 1 shown, record 1 cut at line 120 of 2048; full output saved to <path>
```

之后的记录不再显示，`--offset` 指向这条被截断记录的下一条。第一条之后的记录放不下时，按 §4.4 整条不显示，不截一半。

后续怎么读由 agent 自己决定（harness 的文件读取、`grep`、`sed -n`），工具不提供专门的续读参数。**禁止**把一行截成半行。

### 4.7 提前停止

扫描型操作超过硬上限（默认 10 万条记录）时停止，总数标为下界，并在 stderr 说明怎么收窄条件。

---

## 5. JSON 模式

- 只在确实有程序要解析结果的命令上提供 `--json`（idax / bnx 只有 `exec`），输出**一个**对象，整行，**不受窗口截断**：截断后就不再是 JSON。列表类命令不提供 JSON 信封（评测里没有一次用到）。
- 字段名 snake_case，**跨版本稳定**；新增字段可以，改名或删除算破坏性变更（§13）。

---

## 6. 错误、退出码、提示

### 6.1 退出码

| 码 | 含义 | agent 该怎么做 |
|---|---|---|
| 0 | 成功（含「查无结果」） | — |
| 1 | 出错，结果不可用 | 读 stderr，修正后重试 |
| 3 | 会话丢失：持有状态的进程崩溃或被杀，**未保存的修改可能已丢失** | 按工具的说明重新建立会话 |
| 130 | 被 SIGINT 中断 | — |

- clap 的用法错误也是 **1**（不用 clap 默认的 2），并且必须走与其他错误相同的格式。**参数解析阶段的错误不得绕过本节**。
- 不再新增退出码；需要更细的区分时，用 `kind` 表达。

### 6.2 错误格式

文本模式，stderr：

```
Error: <kind>: <message>
hint: <下一步可以怎么做，最好是一条能直接执行的命令>
```

JSON 模式：stderr 最后一行是单行 JSON，stdout 不写错误：

```json
{"error":{"kind":"target_locked","message":"…","hint":"…","details":{…}}}
```

`kind` 集合：`usage` · `not_found` · `target_unavailable` · `target_locked` · `analysis_incomplete` · `script_error` · `timeout` · `session_lost` · `engine_error` · `internal`。`timeout` 与 `session_lost` 以 3 退出，其余以 1 退出。

### 6.3 hint

- **每个报错都必须带 `hint:`**，写下一步：能粘贴的命令、要看的文件、能确定的事实。依据是失败调用回放：有 hint 的失败 agent 一步就改对，没有的会反复重试（asc-rs 回放：92 次失败中无提示的从 15 降到 0）。`Failure::from_clap` 在 clap 没给提示时也补一条。
- 查无结果、名字拼错、参数可能用错时，**应当**在 stderr 给出 `hint:`。
- hint 里出现的命令必须**能直接粘贴执行**。
- hint 只写在 stderr，不影响 stdout 的可解析性。

---

## 7. 进度与长操作

- 操作超过约 **3 秒**还没结束时在 stderr 打第一行进度，之后**每 5 秒**一行；3 秒内结束的操作不打进度：

```
# progress: auto-analysis 41% (12,031 / 29,400 functions), 38s elapsed
```

- 进度只写 stderr。daemon 用专门的进度帧传给客户端，客户端原样写到 stderr。
- 目的：避免 agent 以为卡死而触发 harness 的命令超时，或者重复发起同一个请求。

---

## 8. daemon

| 规则 | 级别 |
|---|---|
| 每个用户、每个工具、每个构建一个 daemon；**第一次调用时自动拉起**，agent 不需要知道它的存在 | 必须 |
| 客户端把 argv、cwd 和相关环境变量转发给 daemon；daemon 以帧的形式回传 stdout、stderr、进度、退出码 | 必须 |
| socket 名对「二进制身份（路径、大小、mtime、版本）+ 协议版本 + 影响输出的配置」取 hash | 必须 |
| Unix：socket 放在只有本用户可访问的目录，两端校验对方 uid；Windows：named pipe，ACL 只授予当前用户 | 必须 |
| 客户端断开（Ctrl-C）即取消该请求 | 必须 |
| 空闲超时后 daemon 退出，默认 15 分钟 | 必须 |
| `<TOOL>_DAEMON=off` 关闭 daemon，每次调用都在本进程执行（慢，但结果相同） | 必须 |
| 新构建启动时让同一路径的旧构建 daemon **退役**：没有会话的立即退出，有会话的等会话全部关闭后退出；`daemon stop --force` 不等，会话直接丢弃 | 必须 |
| daemon 只用客户端随请求转发的环境变量（`<TOOL>_*`、`AXKIT_*`、harness 标记、`NO_COLOR`）和 TTY 标志判断输出方式，不用自己启动时的环境 | 必须 |
| 命令执行到一半 daemon 消失：`session_lost`，退出码 3 | 必须 |
| 命令需要读 stdin 时（如 `exec -`），由客户端读完、随请求发给 daemon；工具声明哪些命令行需要（`Config::wants_stdin`） | 必须 |
| 连不上 daemon（socket 目录不私有、路径过长、拉起失败、拉起后未在 `start_wait` 内就绪、连接后未作答）**不得静默**：报 `target_unavailable`，写明原因，hint 给出修法和 `<TOOL>_DAEMON=off`；不换个进程悄悄执行（会话只在 daemon 里） | 必须 |
| socket 目录：`<TOOL>_RUNTIME_DIR`，否则 `$XDG_RUNTIME_DIR/<tool>`；放不下 socket 路径（`sockaddr_un` 按 104 字节算）时依次退到 `<temp>/<tool>-<uid>`、`/tmp/<tool>-<uid>`；选中的目录必须属于本用户、权限 0700。兜底目录由所有放不下的 runtime 目录共用，所以 `status` / `stop` 只认本 family（可执行文件、HOME / 缓存目录、umask 相同）的 socket，不碰别人的 daemon | 必须 |
| `daemon stop` 在被停的 daemon 真正退出（socket 已删、lock 已释放，即会话已全部关闭）后才返回，超过 `stop_wait`（默认 30 秒）报错 | 必须 |
| 被新构建取代的 daemon 退出时删除自己的 lock 文件；daemon 启动时删除同一可执行文件其他构建的、无人持有的 lock 与 socket | 必须 |
| `daemon status` 每个 daemon 一行，含 pid、运行时长、请求数、还剩多少秒空闲就退出，后面跟工具给的会话行 | 应当 |
| Handler 在每个连接自己的线程里运行，线程随连接结束：不得依赖绑定到线程的进程属性（例如 `PR_SET_PDEATHSIG` 会在那个线程结束时、而不是 daemon 退出时杀掉子进程） | 必须 |

- **daemon 只管传输和生命周期。** 会话是什么、怎么打开、怎么保存，归用它的工具。
- **daemon 可以有状态。** 正因如此，「daemon 处理不了就拒绝、由客户端自己跑」只适用于不碰会话的命令（`--help` 之类）——会话只在 daemon 里，换个进程就看不到它。这是与 asc-rs 那种纯加速 daemon 的根本差异。

---

## 9. 发现与文档

**渐进披露，由浅到深：**

| 层 | 入口 | 内容 |
|---|---|---|
| L0 | skill（harness 按需加载） | 硬规则 + 最小工作流 + 按任务路由到 reference |
| L1 | `<tool> --help` | 命令列表，每条一句话，**附 3～5 个可直接执行的示例** |
| L2 | `<tool> <command> --help` | 完整参数、默认值、示例、退出码 |
| L3 | 文档检索命令（工具自定，如 `ref <query>`） | 检索（BM25）打包进二进制的语料，返回签名 + 一句说明 + 一个最短示例，默认最多 10 条 |

- `--help` 与文档检索**不得**需要会话、license 或网络。
- 语料在编译期打包进二进制；语料内容由工具提供，只放工具自己的东西（helper、会话语义、headless 下的坑）。工具背后的引擎自带的 API 参考与官方示例**不**打包：文档检索命令在无参数时给出运行时实际加载的那份安装里的路径，附可直接粘贴的 grep 命令。那里的文件与运行的版本精确对应；找不到安装时一行 `hint:` 说明原因和该设置的变量，退出码不变。
- skill 遵循 Agent Skills 规范（YAML frontmatter，`name` 与 `description`；description 写清**什么时候用**）。
- skill 结构：**不可违反的规则** → **最小工作流** → **问题→命令 / 片段 对照表** → **输出与截断的读法** → **错误与退出码怎么处理** → **按任务路由到 reference**。篇幅应当在 150 行以内。
- 所有示例必须能直接执行，由工具的测试逐条执行并检查退出码；预期失败的示例在行尾写 `# exit N`。

### 9.1 skill 的打包与交付

- skill（`SKILL.md` 加它链接的文件）在编译期打包进二进制。查看不需要 daemon、会话、license 或网络，在客户端进程执行。
- 命令：`skills`（等同 `skills show`）把 `SKILL.md` 原样打到 stdout；`skills show <file>` 打出其中一个文件；`skills list` 列出全部文件（`<path> | <bytes>`）。输出**不截断**，不受 agent 默认窗口与 `--max-bytes` 影响：skill 是要被整份装下的。
- **工具不安装 skill。** 没有 `install`、没有 harness → 目录的表、没有 marker。理由：各家 harness 的 skill 目录不同，而且一直在变（用户级、项目级、配置目录可改），工具维护这张表必然过时；agent 知道自己在哪个 harness 里，由它按所在 harness 的规则安装最可靠。`<tool> --help` 里有一句：`` `<tool> skills` prints the agent skill (SKILL.md); install it where your harness loads skills. ``
- 版本写在 frontmatter 的 `metadata.version`（Agent Skills 规范：`metadata` 是 string → string 的映射，<https://agentskills.io/specification>），与 `<tool> --version` 相同；agent 拿已装副本的版本和它比，就知道是否过期。由工具的测试断言二者一致。

### 9.2 MCP

- 已经是 CLI 的工具可以另外提供一个 stdio MCP server（`axkit::mcp`）：用 JSON-RPC 2.0，每行一条消息；只实现 `initialize`、`ping`、`tools/list`、`tools/call`。协议版本支持 2025-06-18、2025-03-26、2024-11-05，客户端请求的版本在其中就用它，否则回最新的那个。请求按到达顺序逐个处理。
- 工具面保持最小（code mode）：用一个执行代码的工具代替一组细粒度工具。每个工具调用就是一条命令行，走 CLI 的同一条路径（同一个 daemon、同样的渲染、窗口和 spill）。返回文本是 stdout 后接 stderr（去掉进度行），退出码非 0 时置 `isError`。
- 工具名只用字母、数字、`_`、`-`：有的客户端不接受 `.`。

---

## 10. agent 识别

满足以下全部条件时进入 **agent 模式**：

1. 设置了以下任一环境变量：`CLAUDECODE`、`CODEX_CI`、`CODEX_SANDBOX`、`CODEX_THREAD_ID`、`CURSOR_AGENT`、`GEMINI_CLI`、`COPILOT_AGENT`、`COPILOT_CLI`、`COPILOT_AGENT_SESSION_ID`、`AI_AGENT`、`AGENT`；
2. stdout 不是 TTY；
3. 没有显式给出 `--full`。

`AXKIT_AGENT=on|off`（或 `--agent` / `--no-agent`）强制开关。

agent 模式只改**默认值**（窗口），**不改输出格式**：同一条命令在人和 agent 两种模式下，看到的记录和格式完全一样，只是截断与否不同。只区分「是不是 agent」，不区分是哪个 harness：所有 harness 用同一个默认值（§4.2），实测最小的宿主截断（codex，约 10 KiB）也在它之上。

---

## 11. eval

- axkit 提供 `axkit-eval` 运行器：给定任务集（问题 + 判定脚本）和工具版本，用真实 harness 跑多轮。
- **必须同时统计**：成功率、轮次、**总 token 与总成本**、工具输出 token。只看输出 token 会高估收益（DuckDB 的实测：输出 token 降了 59%，总成本几乎没变）。
- 约定里的任何默认值（§4.2）调整，都应当附一次 eval 对比。
- **harness**：支持 claude（Claude Code）、codex、grok。各 harness 的隔离程度不同，见 `eval/README.md`。
- **成本**：按价目表（`eval/prices.toml`）从分类型 token（input / output / cache read / cache write）重算；流里自报的成本另列，不采信为准。没有官方价的模型记为「未知」，不估。
- **凭据**：运行器不登录、不登出，不复制、移动或改写任何 harness 的凭据；认证失败即停止整轮并报告。

---

## 12. 待定

| 项 | 选项 | 倾向 |
|---|---|---|
| agent 模式的默认预算 | 8 KiB 起步 | 由 eval 决定 |
| 是否提供 `schema` 自描述命令 | 提供 / 不提供 | 命令很少，先不提供，`--help` 已足够 |

---

## 13. 版本与变更

- 本文有版本号。axkit 的 conformance 测试对应其中的**必须**条目，工具在 CI 里跑，失败即不合格。
- **破坏性变更**：改输出格式、末行提示格式、退出码含义、JSON 字段名或删除字段、环境变量名。需要升 minor 版本（1.0 之前）并写迁移说明。
- daemon 帧协议单独编号，写进 socket 身份 hash，版本不匹配时直接换 daemon，不做协商。当前为 2（请求里加了 stdin 字段）。
- 修改流程：先改本文，再改 axkit，最后改工具。conformance 测试和本文必须在同一个提交里更新。

---

## 附：conformance 断言清单（初版）

| ID | 断言 | 对应章节 |
|---|---|---|
| C-01 | 非 TTY 时 stdout / stderr 不含 ANSI 转义 | §2 |
| C-02 | stdout 被关闭（`\| head -1`）时退出码 0，stderr 为空 | §2 |
| C-03 | stdin 为 `/dev/null` 时不阻塞 | §1 |
| C-04 | 全局 flag 写在子命令前后结果相同 | §1 |
| C-05 | 截断时末行匹配 §4.4 的正则；有全文文件时末行以 `full output saved to <path>` 结尾、不含 `--offset`，文件存在且等于不截断时的 stdout | §4 |
| C-06 | `--offset` 越过末尾时给出规定的提示 | §4.4 |
| C-07 | 单条超长记录只显示完整行，末行提示匹配 §4.6 的格式，全文文件存在且完整 | §4.6 |
| C-09 | 用法错误退出码为 1，格式与其他错误相同（文本与 JSON 两种模式） | §6 |
| C-10 | `--json` 模式下错误是 stderr 最后一行的单行 JSON，stdout 为空 | §6.2 |
| C-11 | 进度只出现在 stderr | §7 |
| C-12 | 走 daemon 与 `<TOOL>_DAEMON=off` 的输出逐字节相同（stdout、stderr 去掉进度行后、退出码） | §8 |
| C-16 | 连不上 daemon 时以 `target_unavailable` 报错并说明原因，不在本进程悄悄执行 | §8 |
| C-17 | `daemon stop` 返回时被停的 daemon 已退出 | §8 |
| C-18 | `--json` 的对象在 agent 默认窗口下仍是完整 JSON | §5 |
| C-13 | 客户端被 SIGINT 后，daemon 中对应请求被取消 | §8 |
| C-14 | `--help` 与文档检索在无 license、无网络、无会话时可用 | §9 |
| C-15 | agent 模式与人模式下，相同输入的记录内容相同，只有截断与否不同 | §10 |
| C-19 | `skills` 的输出与打包的 `SKILL.md` 逐字节相同，agent 模式和很小的 `--max-bytes` 下也不截断；frontmatter 的 `metadata.version` 等于 `<tool> --version` | §9.1 |
| C-20 | skill 里的每条示例都按标注的退出码跑通 | §9 |

---

## 附：来源

本文吸收了以下实现和资料，**均为参考，不作为依赖**：

| 来源 | 吸收的内容 |
|---|---|
| asc-rs（droidasc） | 行记录格式、截断窗口与写文件、末行提示格式、透明 daemon（帧协议、身份 hash、uid、取消）、clap 用法错误改为 1、hint |
| DuckDB CLI agent mode（2026-10） | 按环境变量识别 agent、提前停止与下界计数、进度、eval 要量总成本 |
| ida_rpc | 错误消息里直接给可执行的修复命令、skill 的「硬规则 + 路由」结构 |
| ida-pro-mcp | 分页加截断写文件的组合 |
| The CLI Spec | 禁止静默截断、错误与「非错误的非零结果」分开编码、JSON 错误为 stderr 最后一行 |
| agentnative-cli | SIGPIPE / NO_COLOR / SIGTERM / 非交互的行为检查项、skill 安装命令 |
| Clerk CLI issue #559 | 参数解析阶段的错误不得绕过统一错误格式 |
