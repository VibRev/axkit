//! `codex exec --json`.
//!
//! The session runs in the user's real codex setup (their config, skills,
//! MCP servers and AGENTS.md). `--ephemeral` keeps it from writing a rollout;
//! after the run, anything still keyed by its thread id is removed: rollout
//! files under `~/.codex/sessions` and its lines in `history.jsonl` and
//! `session_index.jsonl`. The SQLite state databases are not edited (no exact,
//! safe way to delete one thread's rows), so a run may remain listed there.
//! No OS sandbox (`danger-full-access`): `workspace-write`'s seccomp filter
//! stops the tool's daemon and the engine's own state writes, and agents then
//! work around it by copying license files elsewhere.

use super::{Call, EVAL_MARK, Harness, Parsed, Session, exe, mentions_auth_failure, mentions_rate_limit};
use crate::cleanup::{self, Report};
use crate::metrics::{Metrics, count, events};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

pub struct Codex;

/// `-c` overrides that add `mcp` as a stdio server. Values are TOML; a JSON
/// string or array of strings is valid TOML too. Engine calls can take
/// minutes, past codex's 60 s default tool timeout. A headless run cannot
/// answer an approval prompt (`approval_policy=never` refuses it), so the
/// server's tools are approved up front, as the shell is under
/// `danger-full-access`.
fn mcp_overrides(mcp: &crate::suite::McpServer) -> Vec<String> {
    let key = format!("mcp_servers.{}", mcp.name);
    let quote = |v: &str| serde_json::Value::from(v).to_string();
    let env: Vec<String> = mcp.env.iter().map(|(k, v)| format!("{}={}", quote(k), quote(v))).collect();
    vec![
        format!("{key}.command={}", quote(&mcp.command)),
        format!("{key}.args={}", serde_json::Value::from(mcp.args.clone())),
        format!("{key}.env={{{}}}", env.join(",")),
        format!("{key}.startup_timeout_sec=120"),
        format!("{key}.tool_timeout_sec=900"),
        format!("{key}.default_tools_approval_mode=\"approve\""),
    ]
}

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn default_exe(&self) -> &'static str {
        "codex"
    }

    fn command(&self, s: &Session<'_>) -> Command {
        let mut cmd = Command::new(exe(self, s.group));
        cmd.current_dir(s.workspace);
        cmd.arg("exec");
        cmd.args(["--json", "--skip-git-repo-check", "--ephemeral", "--color", "never"]);
        cmd.arg("-C").arg(s.workspace);
        cmd.args(["-s", s.group.sandbox.as_deref().unwrap_or("danger-full-access")]);
        cmd.arg("--add-dir").arg(s.tmp);
        cmd.args(["-c", "approval_policy=\"never\"", "-c", "web_search=\"disabled\""]);
        if let Some(model) = &s.group.model {
            cmd.args(["-m", model]);
        }
        if let Some(mcp) = s.mcp {
            for setting in mcp_overrides(mcp) {
                cmd.arg("-c").arg(setting);
            }
        }
        cmd.args(&s.group.args);
        cmd.envs(s.env.iter().map(|(k, v)| (k, v)));
        cmd.arg(s.prompt);
        cmd
    }

    fn skill_file(&self, s: &Session<'_>, name: &str) -> PathBuf {
        s.workspace.join(".agents").join("skills").join(name).join("SKILL.md")
    }

    fn parse(&self, stream: &str) -> Parsed {
        let mut m = Metrics::default();
        let mut answer = String::new();
        for e in events(stream) {
            match e.get("type").and_then(Value::as_str) {
                Some("thread.started") => {
                    m.model = e.get("model").and_then(Value::as_str).map(str::to_owned);
                }
                Some("item.completed") => {
                    let Some(item) = e.get("item") else { continue };
                    match item.get("type").and_then(Value::as_str) {
                        Some("agent_message") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                answer = text.to_owned();
                            }
                        }
                        Some("command_execution" | "mcp_tool_call") => {
                            m.tool_calls += 1;
                            let failed = item.get("status").and_then(Value::as_str) == Some("failed")
                                || item.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
                            let output =
                                item.get("aggregated_output").and_then(Value::as_str).map_or_else(|| item.to_string(), str::to_owned);
                            m.tool_result(&output, failed);
                        }
                        _ => {}
                    }
                }
                Some("turn.completed") => {
                    m.completed = true;
                    let usage = e.get("usage").cloned().unwrap_or(Value::Null);
                    let (input, cached) = (count(&usage, "/input_tokens"), count(&usage, "/cached_input_tokens"));
                    // `input_tokens` includes the cached part; keep the kinds apart.
                    m.tokens.input += input.saturating_sub(cached);
                    m.tokens.cache_read += cached;
                    m.tokens.cache_write_5m += count(&usage, "/cache_write_input_tokens");
                    m.tokens.output += count(&usage, "/output_tokens");
                    m.tokens.reasoning += count(&usage, "/reasoning_output_tokens");
                }
                Some("turn.failed") => {
                    let msg = e.pointer("/error/message").and_then(Value::as_str).unwrap_or("turn failed");
                    m.auth_failed |= mentions_auth_failure(msg);
                    m.fail(msg);
                }
                Some("error") => {
                    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    m.auth_failed |= mentions_auth_failure(msg);
                    // A reconnect is codex retrying a dropped stream, not a failure.
                    if !msg.starts_with("Reconnecting") {
                        m.fail(msg);
                    }
                }
                _ => {}
            }
        }
        // codex reports no turn count; every tool call took one model response,
        // and the final answer one more.
        m.turns = m.tool_calls + u32::from(m.completed);
        Parsed { metrics: m, answer }
    }

    fn calls(&self, stream: &str) -> Vec<Call> {
        events(stream)
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("item.completed"))
            .filter_map(|e| e.get("item").cloned())
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("command_execution"))
            .filter_map(|item| {
                let command = item.get("command").and_then(Value::as_str)?;
                Some(Call {
                    command: unwrap_shell(command),
                    output: item.get("aggregated_output").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    failed: item.get("status").and_then(Value::as_str) == Some("failed")
                        || item.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0),
                })
            })
            .collect()
    }

    fn auth_failure(&self, line: &str) -> bool {
        let Ok(e) = serde_json::from_str::<Value>(line) else { return false };
        match e.get("type").and_then(Value::as_str) {
            Some("error") => e.get("message").and_then(Value::as_str).is_some_and(mentions_auth_failure),
            Some("turn.failed") => e.pointer("/error/message").and_then(Value::as_str).is_some_and(mentions_auth_failure),
            _ => false,
        }
    }

    fn session_id(&self, stream: &str) -> Option<String> {
        events(stream).find_map(|e| e.get("thread_id").and_then(Value::as_str).map(str::to_owned))
    }

    fn cleanup(&self, _s: &Session<'_>, session_id: Option<&str>) -> Report {
        let root = cleanup::home().join(".codex");
        let mut report = Report::default();
        let Some(id) = session_id.filter(|id| id.len() >= 8) else { return report };
        for path in cleanup::find_named(&root.join("sessions"), id, 4).into_iter().filter(|p| p.is_file()) {
            match std::fs::remove_file(&path) {
                Ok(()) => report.removed.push(path.display().to_string()),
                Err(e) => report.warnings.push(format!("removing {}: {e}", path.display())),
            }
        }
        for file in ["history.jsonl", "session_index.jsonl"] {
            cleanup::drop_lines(&root.join(file), |l| l.contains(id), &mut report);
        }
        report
    }

    fn inventory(&self) -> Vec<(String, usize, usize)> {
        let root = cleanup::home().join(".codex");
        let rollouts = cleanup::find_named(&root.join("sessions"), "rollout-", 4);
        // A rollout's first line holds its cwd.
        let eval = rollouts
            .iter()
            .filter(|p| std::fs::read_to_string(p).ok().and_then(|t| t.lines().next().map(|l| l.contains(EVAL_MARK))).unwrap_or(false))
            .count();
        let mut rows = vec![("codex rollouts".to_owned(), rollouts.len(), eval)];
        for file in ["history.jsonl", "session_index.jsonl"] {
            let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
            rows.push((format!("codex {file} lines"), text.lines().count(), text.lines().filter(|l| l.contains(EVAL_MARK)).count()));
        }
        rows
    }

    fn rate_limited(&self, stream: &str) -> bool {
        events(stream)
            .any(|e| matches!(e.get("type").and_then(Value::as_str), Some("error" | "turn.failed")) && mentions_rate_limit(&e.to_string()))
    }
}

/// codex runs every command as `/bin/zsh -lc '<command>'` (or with the
/// command in double quotes): the command, with the shell's quoting undone.
/// Anything else is kept as it is.
fn unwrap_shell(command: &str) -> String {
    let inner = command
        .split_once(' ')
        .filter(|(shell, _)| shell.ends_with("sh"))
        .and_then(|(_, rest)| ["-lc ", "-c "].iter().find_map(|flag| rest.strip_prefix(flag)));
    let Some(rest) = inner else { return command.to_owned() };
    if let Some(quoted) = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return quoted.replace("'\\''", "'");
    }
    let Some(quoted) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { return rest.to_owned() };
    // Inside double quotes a backslash escapes only `"`, `\`, `$` and `` ` ``.
    let mut out = String::with_capacity(quoted.len());
    let mut chars = quoted.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&n @ ('"' | '\\' | '$' | '`'))) => {
                out.push(n);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{args, group, session};
    use super::*;

    #[test]
    fn the_command_is_ephemeral_and_keeps_the_user_setup() {
        let g = group("codex");
        let cmd = Codex.command(&session(&g, &[]));
        let a = args(&cmd);
        assert_eq!(a[0], "exec");
        for flag in ["--json", "--ephemeral", "--skip-git-repo-check"] {
            assert!(a.contains(&flag.to_owned()), "{flag}");
        }
        assert!(!a.contains(&"--ignore-rules".to_owned()) && !a.contains(&"--disable".to_owned()), "no isolation switches");
        assert!(!a.contains(&"--ignore-user-config".to_owned()), "the provider and its token live in config.toml");
        assert!(a.windows(2).any(|w| w == ["-s", "danger-full-access"]));
        assert!(a.windows(2).any(|w| w == ["--add-dir", "/tmp/axe-x"]));
        assert_eq!(a.last().map(String::as_str), Some("do it"));
    }

    #[test]
    fn an_mcp_server_is_added_with_its_tools_approved() {
        let g = group("codex");
        let server = crate::harness::test_support::mcp();
        let mut s = session(&g, &[]);
        s.mcp = Some(&server);
        let a = args(&Codex.command(&s));
        for want in [
            r#"mcp_servers.ida.command="/opt/ida-mcp""#,
            r#"mcp_servers.ida.args=["stdio","--agent=x"]"#,
            r#"mcp_servers.ida.env={"IDA_MCP_STATE_DIR"="/tmp/axe-x/m"}"#,
            r#"mcp_servers.ida.default_tools_approval_mode="approve""#,
        ] {
            assert!(a.iter().any(|x| x == want), "{want} in {a:?}");
        }
    }

    #[test]
    fn a_stream_yields_tokens_tools_and_answer() {
        let stream = r#"
{"type":"thread.started","thread_id":"019a-thread"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"thinking"}}
{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"idax exec crackme -c x","aggregated_output":"a\n# records 1-1 of 4 shown; full output in /c (grep it, or rerun with --offset 1)\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"idax nope","aggregated_output":"Error: usage","exit_code":1,"status":"failed"}}
{"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"The password is s3cret_42."}}
{"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion: error sending request)"}
{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_tokens":3000,"cache_write_input_tokens":0,"output_tokens":400,"reasoning_output_tokens":250}}
"#;
        let p = Codex.parse(stream);
        let m = &p.metrics;
        assert!(m.completed && m.error.is_none(), "{:?}", m.error);
        assert_eq!((m.tool_calls, m.tool_errors, m.truncations, m.turns), (2, 1, 1, 3));
        assert_eq!((m.tokens.input, m.tokens.cache_read, m.tokens.output, m.tokens.reasoning), (2000, 3000, 400, 250));
        assert_eq!(p.answer, "The password is s3cret_42.");
        assert_eq!(Codex.session_id(stream).as_deref(), Some("019a-thread"));
    }

    #[test]
    fn an_unauthorized_error_is_an_auth_failure() {
        assert!(Codex.auth_failure(r#"{"type":"error","message":"unexpected status 401 Unauthorized: invalid api key"}"#));
        assert!(!Codex.auth_failure(r#"{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"0x401000"}}"#));
        let p = Codex.parse(r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#);
        assert!(p.metrics.auth_failed && !p.metrics.completed);
    }
}
