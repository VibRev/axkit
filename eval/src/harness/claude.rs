//! Claude Code headless.
//!
//! The session runs in the user's real `~/.claude` (their skills, MCP config
//! and CLAUDE.md are part of real use). `--no-session-persistence` keeps it
//! from writing a transcript; whatever it still leaves is removed after the
//! run: the project dir named after the run's unique workspace, entries named
//! by its session id, and its lines in `history.jsonl`. Credentials are never
//! touched. The executable is `claude` unless the group's `exe` or
//! `AXEVAL_CLAUDE_EXE` names another launcher.

use super::{Call, EVAL_MARK, Harness, Parsed, Session, exe, mentions_auth_failure, mentions_rate_limit};
use crate::cleanup::{self, Report};
use crate::metrics::{Metrics, count, events};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

pub struct Claude;

/// The `--mcp-config` file, in the run's private tmp dir (removed with it).
fn mcp_config_path(s: &Session<'_>) -> PathBuf {
    s.tmp.join("mcp.json")
}

/// Variables of the session this runner itself may be inside of; removed so
/// the agent does not think it is nested. Credential variables are kept.
const DROP: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn default_exe(&self) -> &'static str {
        "claude"
    }

    fn command(&self, s: &Session<'_>) -> Command {
        let mut cmd = Command::new(exe(self, s.group));
        cmd.current_dir(s.workspace);
        cmd.arg("-p").arg(s.prompt);
        cmd.args(["--output-format", "stream-json", "--verbose"]);
        // No allow list by default: the session runs as an agent in the user's own
        // setup would, like codex's `danger-full-access`.
        cmd.arg("--permission-mode").arg(s.group.permission_mode.as_deref().unwrap_or("auto"));
        cmd.arg("--no-session-persistence");
        let tools = if s.group.tools.is_empty() { "Bash,Read,Grep,Glob".to_owned() } else { s.group.tools.join(",") };
        cmd.arg("--tools").arg(tools);
        let mut allow = s.group.allow.clone();
        if let Some(mcp) = s.mcp {
            // Only this server: the user's own MCP servers would muddy a tool comparison.
            cmd.arg("--mcp-config").arg(mcp_config_path(s)).arg("--strict-mcp-config");
            allow.push(format!("mcp__{}", mcp.name));
        }
        if !allow.is_empty() {
            cmd.arg("--allowedTools").args(&allow);
        }
        if !s.group.deny.is_empty() {
            cmd.arg("--disallowedTools").args(&s.group.deny);
        }
        if let Some(model) = &s.group.model {
            cmd.args(["--model", model]);
        }
        if let Some(turns) = s.group.max_turns {
            cmd.args(["--max-turns", &turns.to_string()]);
        }
        cmd.args(&s.group.args);
        for key in DROP {
            cmd.env_remove(key);
        }
        cmd.envs(s.env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    fn skill_file(&self, s: &Session<'_>, name: &str) -> PathBuf {
        s.workspace.join(".claude").join("skills").join(name).join("SKILL.md")
    }

    fn prepare(&self, s: &Session<'_>) -> anyhow::Result<()> {
        let Some(mcp) = s.mcp else { return Ok(()) };
        let server = serde_json::json!({ "type": "stdio", "command": mcp.command, "args": mcp.args, "env": mcp.env });
        let config = serde_json::json!({ "mcpServers": { &mcp.name: server } });
        std::fs::write(mcp_config_path(s), serde_json::to_string_pretty(&config)?)?;
        Ok(())
    }

    fn parse(&self, stream: &str) -> Parsed {
        let mut m = Metrics::default();
        let mut answer = String::new();
        let mut summed = crate::metrics::Tokens::default();
        let mut assistant_turns = 0;
        for e in events(stream) {
            match e.get("type").and_then(Value::as_str) {
                Some("system") => {
                    if let Some(model) = e.get("model").and_then(Value::as_str) {
                        m.model.get_or_insert_with(|| model.to_owned());
                    }
                    if e.get("subtype").and_then(Value::as_str) == Some("api_retry") && is_auth_retry(&e) {
                        m.auth_failed = true;
                    }
                }
                Some("assistant") => {
                    assistant_turns += 1;
                    if let Some(usage) = e.pointer("/message/usage") {
                        add_usage(&mut summed, usage);
                    }
                    let blocks = e.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                    m.tool_calls += u32::try_from(blocks.iter().filter(|b| kind(b) == Some("tool_use")).count()).unwrap_or(u32::MAX);
                    let text: String =
                        blocks.iter().filter(|b| kind(b) == Some("text")).filter_map(|b| b.get("text").and_then(Value::as_str)).collect();
                    if !text.is_empty() {
                        if e.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>") && mentions_auth_failure(&text) {
                            m.auth_failed = true;
                        }
                        answer = text;
                    }
                }
                Some("user") => {
                    for b in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                        if kind(b) == Some("tool_result") {
                            m.tool_result(&flatten(b.get("content")), b.get("is_error").and_then(Value::as_bool).unwrap_or(false));
                        }
                    }
                }
                Some("result") => {
                    m.completed = true;
                    m.turns = e.get("num_turns").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                    if let Some(usage) = e.get("usage") {
                        let mut total = crate::metrics::Tokens::default();
                        add_usage(&mut total, usage);
                        if total.total() > 0 {
                            m.tokens = total;
                        }
                    }
                    if m.model.is_none() {
                        m.model = e.get("modelUsage").and_then(Value::as_object).and_then(|o| o.keys().next().cloned());
                    }
                    m.reported_cost_usd = e.get("total_cost_usd").and_then(Value::as_f64).filter(|c| *c > 0.0);
                    if let Some(result) = e.get("result").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                        answer = result.to_owned();
                    }
                    if e.get("is_error").and_then(Value::as_bool) == Some(true) {
                        m.fail(e.get("terminal_reason").and_then(Value::as_str).unwrap_or("result reported is_error"));
                    }
                }
                _ => {}
            }
        }
        if m.tokens.total() == 0 {
            m.tokens = summed;
        }
        if m.turns == 0 {
            m.turns = assistant_turns;
        }
        Parsed { metrics: m, answer }
    }

    fn calls(&self, stream: &str) -> Vec<Call> {
        // A Bash `tool_use` names the command; its `tool_result`, matched by
        // id, holds what came back. A piped command's failure is not an
        // error to Claude Code, so `failed` alone misses most of them.
        let mut calls: Vec<(String, Call)> = Vec::new();
        for e in events(stream) {
            for b in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                match kind(b) {
                    Some("tool_use") if b.get("name").and_then(Value::as_str) == Some("Bash") => {
                        let (Some(id), Some(command)) =
                            (b.get("id").and_then(Value::as_str), b.pointer("/input/command").and_then(Value::as_str))
                        else {
                            continue;
                        };
                        calls.push((id.to_owned(), Call { command: command.to_owned(), ..Call::default() }));
                    }
                    Some("tool_result") => {
                        let id = b.get("tool_use_id").and_then(Value::as_str);
                        if let Some((_, call)) = calls.iter_mut().find(|(i, _)| Some(i.as_str()) == id) {
                            call.output = flatten(b.get("content"));
                            call.failed = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        }
                    }
                    _ => {}
                }
            }
        }
        calls.into_iter().map(|(_, c)| c).collect()
    }

    fn auth_failure(&self, line: &str) -> bool {
        let Ok(e) = serde_json::from_str::<Value>(line) else { return false };
        match e.get("type").and_then(Value::as_str) {
            Some("system") => e.get("subtype").and_then(Value::as_str) == Some("api_retry") && is_auth_retry(&e),
            Some("assistant") => {
                e.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
                    && e.pointer("/message/content/0/text").and_then(Value::as_str).is_some_and(mentions_auth_failure)
            }
            _ => false,
        }
    }

    fn session_id(&self, stream: &str) -> Option<String> {
        events(stream).find_map(|e| e.get("session_id").and_then(Value::as_str).map(str::to_owned))
    }

    fn cleanup(&self, s: &Session<'_>, session_id: Option<&str>) -> Report {
        let root = cleanup::home().join(".claude");
        let mut report = Report::default();
        cleanup::remove_run_dir(&root.join("projects").join(Claude::project_dir_name(s.workspace)), s.run_id, &mut report);
        if let Some(id) = session_id.filter(|id| id.len() >= 8) {
            for path in cleanup::find_named(&root, id, 2) {
                let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                match removed {
                    Ok(()) => report.removed.push(path.display().to_string()),
                    Err(e) => report.warnings.push(format!("removing {}: {e}", path.display())),
                }
            }
        }
        let ws = format!("\"{}\"", s.workspace.display());
        cleanup::drop_lines(&root.join("history.jsonl"), |l| l.contains(&ws), &mut report);
        report
    }

    fn inventory(&self) -> Vec<(String, usize, usize)> {
        let root = cleanup::home().join(".claude");
        let mark = Claude::project_dir_name(std::path::Path::new(EVAL_MARK));
        let names: Vec<String> = std::fs::read_dir(root.join("projects"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let history = std::fs::read_to_string(root.join("history.jsonl")).unwrap_or_default();
        vec![
            ("claude projects".to_owned(), names.len(), names.iter().filter(|n| n.contains(&mark)).count()),
            ("claude history.jsonl lines".to_owned(), history.lines().count(), history.lines().filter(|l| l.contains(EVAL_MARK)).count()),
        ]
    }

    fn rate_limited(&self, stream: &str) -> bool {
        events(stream).any(|e| {
            e.get("subtype").and_then(Value::as_str) == Some("api_retry")
                && (e.get("error_status").and_then(Value::as_u64) == Some(429)
                    || e.get("error").and_then(Value::as_str).is_some_and(mentions_rate_limit))
        }) && !events(stream)
            .any(|e| e.get("type").and_then(Value::as_str) == Some("result") && e.get("is_error").and_then(Value::as_bool) == Some(false))
    }
}

impl Claude {
    /// `~/.claude/projects/<name>`: Claude Code names a project dir after its
    /// cwd with every character outside `[A-Za-z0-9]` turned into `-`.
    fn project_dir_name(workspace: &std::path::Path) -> String {
        workspace.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
    }
}

fn is_auth_retry(e: &Value) -> bool {
    matches!(e.get("error_status").and_then(Value::as_u64), Some(401 | 403))
        || e.get("error").and_then(Value::as_str).is_some_and(mentions_auth_failure)
}

fn kind(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// Usage as Claude reports it: uncached input, output, cache reads, and cache
/// writes split by TTL when `cache_creation` is present (else all 5-minute).
fn add_usage(t: &mut crate::metrics::Tokens, usage: &Value) {
    t.input += count(usage, "/input_tokens");
    t.output += count(usage, "/output_tokens");
    t.cache_read += count(usage, "/cache_read_input_tokens");
    let written = count(usage, "/cache_creation_input_tokens");
    let (m5, h1) = (count(usage, "/cache_creation/ephemeral_5m_input_tokens"), count(usage, "/cache_creation/ephemeral_1h_input_tokens"));
    if m5 + h1 > 0 {
        t.cache_write_5m += m5;
        t.cache_write_1h += h1;
    } else {
        t.cache_write_5m += written;
    }
    t.reasoning += count(usage, "/output_tokens_details/thinking_tokens");
}

/// A tool result's `content`: a string, or text blocks joined.
fn flatten(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{args, env, group, session};
    use super::*;

    #[test]
    fn the_command_runs_in_the_real_environment_and_never_touches_credentials() {
        let mut g = group("claude");
        g.allow.clear();
        let e = [("IDAX_RUNTIME_DIR".to_owned(), "/rt".to_owned())];
        let cmd = Claude.command(&session(&g, &e));
        assert_eq!(cmd.get_program(), "claude");
        let a = args(&cmd);
        assert!(a.windows(2).any(|w| w == ["--model", "m"]));
        assert!(a.windows(2).any(|w| w == ["--tools", "Bash,Read,Grep,Glob"]));
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "auto"]));
        assert!(!a.contains(&"--allowedTools".to_owned()));
        assert!(!a.contains(&"--strict-mcp-config".to_owned()) && !a.contains(&"--disallowedTools".to_owned()));
        assert_eq!(env(&cmd, "CLAUDE_CONFIG_DIR"), None, "the user's own ~/.claude");
        assert_eq!(env(&cmd, "CLAUDECODE"), Some(None), "removed");
        assert_eq!(env(&cmd, "CLAUDE_SECURESTORAGE_CONFIG_DIR"), None, "credential store untouched");
        assert_eq!(env(&cmd, "HOME"), None);
    }

    #[test]
    fn a_skill_is_a_project_skill_in_the_workspace() {
        let g = group("claude");
        assert_eq!(
            Claude.skill_file(&session(&g, &[]), "idax"),
            PathBuf::from("/x/target/eval-data/ws/g.t.0.rid1/.claude/skills/idax/SKILL.md")
        );
    }

    #[test]
    fn the_project_dir_name_replaces_every_non_alphanumeric() {
        let name = Claude::project_dir_name(std::path::Path::new("/home/u/VibRev/idax/target/eval-data/ws/smoke-claude.password.0.r1a2"));
        assert_eq!(name, "-home-u-VibRev-idax-target-eval-data-ws-smoke-claude-password-0-r1a2");
    }

    #[test]
    fn the_401_retry_and_the_synthetic_auth_message_are_auth_failures() {
        assert!(
            Claude
                .auth_failure(r#"{"type":"system","subtype":"api_retry","attempt":1,"error_status":401,"error":"authentication_failed"}"#)
        );
        assert!(Claude.auth_failure(
            r#"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"Failed to authenticate: OAuth session expired and could not be refreshed"}]}}"#
        ));
        // A tool result naming 0x401000 is not an auth failure.
        assert!(!Claude.auth_failure(r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"0x401000 main"}]}}"#));
        assert!(!Claude.auth_failure(r#"{"type":"system","subtype":"api_retry","error_status":529}"#));
    }

    #[test]
    fn an_mcp_server_is_the_only_one_and_its_tools_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let g = group("claude");
        let server = crate::harness::test_support::mcp();
        let mut s = session(&g, &[]);
        s.tmp = dir.path();
        s.mcp = Some(&server);
        Claude.prepare(&s).unwrap();
        let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["ida"]["command"], "/opt/ida-mcp");
        assert_eq!(config["mcpServers"]["ida"]["env"]["IDA_MCP_STATE_DIR"], "/tmp/axe-x/m");
        let a = args(&Claude.command(&s));
        assert!(a.iter().any(|x| x == "--strict-mcp-config"), "{a:?}");
        assert!(a.iter().any(|x| x == "mcp__ida"), "{a:?}");
    }
}
