//! Grok headless, `--output-format streaming-json`.
//!
//! The session runs in the user's real grok setup (their config, skills, MCP
//! servers and compatibility scans). The OS sandbox is a profile written
//! into the workspace's own `.grok/sandbox.toml`: `workspace` plus write
//! access to the engines' state dirs, and a kernel-enforced deny on the
//! *other* harnesses' credential files that exist (grok's own stay readable:
//! it needs them; the IDA license stays readable: the engine needs it).
//! After the run, its session (keyed by session id, under a directory named
//! after the run's unique workspace) and the trust entry `--trust` wrote for
//! that workspace are removed.
//!
//! `--trust` rewrites `~/.grok/trusted_folders.toml` at startup without a
//! lock of its own, and cleanup rewrites it too; concurrent sessions take
//! turns through one [`FileLock`]: a launch holds it until its workspace's
//! table is in the file, cleanup while it drops that table.

use super::{Call, EVAL_MARK, Harness, Parsed, Session, exe, mentions_auth_failure, mentions_rate_limit};
use crate::cleanup::{self, FileLock, Report};
use crate::metrics::{Metrics, count, events};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

pub struct Grok;

/// The custom sandbox profile `prepare` writes into the workspace.
const PROFILE: &str = "axeval";

/// Directories the engines under test write their own state to; everything
/// else outside the workspace and /tmp stays read-only.
const ENGINE_STATE: &[&str] = &["~/.idapro", "~/.binaryninja", "~/Library/Application Support/Binary Ninja"];

/// Other harnesses' credential files, kernel-denied when they exist (a deny
/// entry that cannot be bound stops grok from starting).
const OTHER_CREDENTIALS: &[&str] = &["~/.codex/config.toml", "~/.codex/auth.json", "~/.claude/.credentials.json"];

/// A project `.grok/config.toml` with `mcp` as its one stdio server, in the
/// form `grok mcp add -s project` writes. Strings are JSON-quoted, which is
/// valid TOML.
fn mcp_config(mcp: &crate::suite::McpServer) -> String {
    let quote = |v: &str| serde_json::Value::from(v).to_string();
    let mut out = format!(
        "[mcp_servers.{}]\ncommand = {}\nargs = {}\nenabled = true\n",
        mcp.name,
        quote(&mcp.command),
        serde_json::Value::from(mcp.args.clone())
    );
    if !mcp.env.is_empty() {
        out.push_str(&format!("\n[mcp_servers.{}.env]\n", mcp.name));
        for (k, v) in &mcp.env {
            out.push_str(&format!("{} = {}\n", quote(k), quote(v)));
        }
    }
    out
}

fn trusted_folders() -> PathBuf {
    cleanup::home().join(".grok").join("trusted_folders.toml")
}

/// Files grok keeps per working directory rather than per session.
const PER_CWD: &[&str] = &["prompt_history.jsonl"];

/// `dir` holds nothing, or only [`PER_CWD`] files.
fn only_bookkeeping(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().all(|e| PER_CWD.iter().any(|f| e.file_name() == *f)))
}

impl Harness for Grok {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn default_exe(&self) -> &'static str {
        "grok"
    }

    fn command(&self, s: &Session<'_>) -> Command {
        let mut cmd = Command::new(exe(self, s.group));
        cmd.current_dir(s.workspace);
        cmd.arg("-p").arg(s.prompt);
        cmd.arg("--cwd").arg(s.workspace);
        cmd.args(["--output-format", "streaming-json", "--trust", "--no-subagents", "--no-plan", "--disable-web-search"]);
        // A headless run cancels the whole session on the first command it may not
        // run (it cannot ask), so allowlists make sessions die on `ls`. Approve
        // everything; the kernel deny in the sandbox profile and the deny rules
        // below still hold.
        cmd.arg("--always-approve");
        cmd.args(["--sandbox", s.group.sandbox.as_deref().unwrap_or(PROFILE)]);
        let tools = if s.group.tools.is_empty() { "run_terminal_cmd,read_file,grep,list_dir".to_owned() } else { s.group.tools.join(",") };
        cmd.arg("--tools").arg(tools);
        for rule in &s.group.allow {
            cmd.arg("--allow").arg(rule);
        }
        for rule in &s.group.deny {
            cmd.arg("--deny").arg(rule);
        }
        if let Some(model) = &s.group.model {
            cmd.args(["-m", model]);
        }
        if let Some(turns) = s.group.max_turns {
            cmd.args(["--max-turns", &turns.to_string()]);
        }
        cmd.args(&s.group.args);
        cmd.envs(s.env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    fn prepare(&self, s: &Session<'_>) -> anyhow::Result<()> {
        let dir = s.workspace.join(".grok");
        if let Some(mcp) = s.mcp {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("config.toml"), mcp_config(mcp))?;
        }
        if s.group.sandbox.as_deref().is_some_and(|p| p != PROFILE) {
            return Ok(());
        }
        // Absolute paths: a project profile does not expand `~` in
        // `read_write` (the grant silently matched nothing).
        let home = std::env::var("HOME").unwrap_or_default();
        let absolute = |x: &&str| x.replacen('~', &home, 1);
        let quote = |xs: &[String]| xs.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(", ");
        let state: Vec<String> = ENGINE_STATE.iter().map(absolute).filter(|p| std::path::Path::new(p).exists()).collect();
        let extra = super::extra_credentials();
        let deny: Vec<String> = OTHER_CREDENTIALS
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
            .map(|x| absolute(&x))
            .filter(|p| std::path::Path::new(p).exists())
            .collect();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("sandbox.toml"),
            format!("[profiles.{PROFILE}]\nextends = \"workspace\"\nread_write = [{}]\ndeny = [{}]\n", quote(&state), quote(&deny)),
        )?;
        Ok(())
    }

    fn startup_lock(&self) -> Option<PathBuf> {
        Some(cleanup::grok_trust_lock())
    }

    fn started(&self, s: &Session<'_>) -> bool {
        let text = std::fs::read_to_string(trusted_folders()).unwrap_or_default();
        cleanup::trusts_folder(&text, &s.workspace.to_string_lossy())
    }

    fn skill_file(&self, s: &Session<'_>, name: &str) -> PathBuf {
        // The working directory's skill root has the highest priority.
        s.workspace.join(".grok").join("skills").join(name).join("SKILL.md")
    }

    fn parse(&self, stream: &str) -> Parsed {
        let mut m = Metrics::default();
        let mut answer = String::new();
        let mut responses = 0;
        for e in events(stream) {
            match e.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(chunk) = e.get("data").and_then(Value::as_str) {
                        answer.push_str(chunk);
                    }
                }
                Some("tool_call") => {
                    m.tool_calls += 1;
                    // Text before a tool call was narration, not the answer.
                    answer.clear();
                }
                Some("tool_call_update") => {
                    if let Some(status @ ("completed" | "failed")) = e.get("status").and_then(Value::as_str) {
                        m.tool_result(&tool_output(&e), status == "failed");
                    }
                }
                Some("usage") => responses += 1,
                Some("end") => {
                    m.completed = true;
                    let usage = e.get("usage").cloned().unwrap_or(Value::Null);
                    m.tokens.input = count(&usage, "/input_tokens");
                    m.tokens.output = count(&usage, "/output_tokens");
                    m.tokens.cache_read = count(&usage, "/cache_read_input_tokens");
                    m.tokens.cache_write_5m = count(&usage, "/cache_creation_input_tokens");
                    m.tokens.reasoning = count(&usage, "/reasoning_tokens");
                    m.turns = e.get("num_turns").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                    m.model = e.get("modelUsage").and_then(Value::as_object).and_then(|o| o.keys().next().cloned());
                    m.reported_cost_usd = e.get("total_cost_usd").and_then(Value::as_f64).filter(|c| *c > 0.0);
                    match e.get("stopReason").and_then(Value::as_str) {
                        Some("end_turn") | None => {}
                        Some(other) => m.fail(format!("stopped: {other}")),
                    }
                }
                Some("error") => {
                    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    m.auth_failed |= mentions_auth_failure(msg);
                    m.fail(msg);
                }
                _ => {}
            }
        }
        if m.turns == 0 {
            m.turns = responses;
        }
        Parsed { metrics: m, answer: answer.trim().to_owned() }
    }

    fn calls(&self, stream: &str) -> Vec<Call> {
        // `tool_call` carries the command, the last `tool_call_update` with
        // the same id its result; the shell tool's text for the model starts
        // `exit: N`.
        let mut calls: Vec<(String, Call)> = Vec::new();
        for e in events(stream) {
            let id = e.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
            match e.get("type").and_then(Value::as_str) {
                Some("tool_call") => {
                    if let Some(command) = e.pointer("/rawInput/command").and_then(Value::as_str) {
                        calls.push((id.to_owned(), Call { command: command.to_owned(), ..Call::default() }));
                    }
                }
                Some("tool_call_update") => {
                    let Some(status @ ("completed" | "failed")) = e.get("status").and_then(Value::as_str) else { continue };
                    let Some((_, call)) = calls.iter_mut().find(|(i, _)| i == id) else { continue };
                    let shown = e.pointer("/rawOutput/output_for_prompt").and_then(Value::as_str);
                    let output = shown.map_or_else(|| tool_output(&e), str::to_owned);
                    let exit =
                        output.strip_prefix("exit: ").and_then(|r| r.split_once('\n')).and_then(|(n, _)| n.trim().parse::<i64>().ok());
                    call.failed = status == "failed" || exit.is_some_and(|c| c != 0);
                    call.output = output;
                }
                _ => {}
            }
        }
        calls.into_iter().map(|(_, c)| c).collect()
    }

    fn auth_failure(&self, line: &str) -> bool {
        let Ok(e) = serde_json::from_str::<Value>(line) else { return false };
        e.get("type").and_then(Value::as_str) == Some("error")
            && e.get("message").and_then(Value::as_str).is_some_and(mentions_auth_failure)
    }

    fn session_id(&self, stream: &str) -> Option<String> {
        events(stream).find_map(|e| e.get("sessionId").and_then(Value::as_str).map(str::to_owned))
    }

    fn cleanup(&self, s: &Session<'_>, session_id: Option<&str>) -> Report {
        let root = cleanup::home().join(".grok");
        let mut report = Report::default();
        if let Some(id) = session_id.filter(|id| id.len() >= 8) {
            for path in cleanup::find_named(&root.join("sessions"), id, 1) {
                let parent = path.parent().map(std::path::Path::to_path_buf);
                let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                match removed {
                    Ok(()) => report.removed.push(path.display().to_string()),
                    Err(e) => report.warnings.push(format!("removing {}: {e}", path.display())),
                }
                // The per-cwd directory is the run's own once it holds nothing
                // but grok's per-directory bookkeeping (remove_run_dir still
                // insists its name carries the run id).
                if let Some(parent) = parent.filter(|p| only_bookkeeping(p)) {
                    cleanup::remove_run_dir(&parent, s.run_id, &mut report);
                }
            }
        }
        let ws = s.workspace.to_string_lossy().into_owned();
        match FileLock::acquire(&cleanup::grok_trust_lock()) {
            Ok(_held) => cleanup::drop_trusted_folders(&trusted_folders(), |p| p == ws, &mut report),
            Err(e) => report.warnings.push(format!("locking {}: {e}; trust entry left in place", cleanup::grok_trust_lock().display())),
        }
        report
    }

    fn inventory(&self) -> Vec<(String, usize, usize)> {
        let root = cleanup::home().join(".grok");
        let encoded = EVAL_MARK.replace('/', "%2F");
        let names: Vec<String> = std::fs::read_dir(root.join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let trusted = std::fs::read_to_string(root.join("trusted_folders.toml")).unwrap_or_default();
        let tables: Vec<&str> = trusted.lines().filter(|l| l.trim_start().starts_with("[folders.")).collect();
        vec![
            ("grok session dirs".to_owned(), names.len(), names.iter().filter(|n| n.contains(&encoded)).count()),
            ("grok trusted folders".to_owned(), tables.len(), tables.iter().filter(|l| l.contains(EVAL_MARK)).count()),
        ]
    }

    fn rate_limited(&self, stream: &str) -> bool {
        events(stream).any(|e| e.get("type").and_then(Value::as_str) == Some("error") && mentions_rate_limit(&e.to_string()))
    }
}

/// A finished tool call's output as text: `rawOutput` when it is a string, or
/// holds one (or a byte array, as the shell tool does) under a usual key;
/// else the text blocks of `content`.
fn tool_output(e: &Value) -> String {
    let raw = e.get("rawOutput");
    if let Some(s) = raw.and_then(Value::as_str) {
        return s.to_owned();
    }
    for key in ["output", "stdout", "content", "text", "result"] {
        match raw.and_then(|r| r.get(key)) {
            Some(Value::String(s)) => return s.clone(),
            Some(Value::Array(bytes)) => {
                let bytes: Option<Vec<u8>> = bytes.iter().map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok())).collect();
                if let Some(bytes) = bytes {
                    return String::from_utf8_lossy(&bytes).into_owned();
                }
            }
            _ => {}
        }
    }
    let blocks: String = e
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.pointer("/content/text").or_else(|| b.get("text")).and_then(Value::as_str))
        .collect();
    if blocks.is_empty() { raw.map(Value::to_string).unwrap_or_default() } else { blocks }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{args, env, group, session};
    use super::*;

    #[test]
    fn the_command_is_headless_sandboxed_and_hides_other_homes() {
        let g = group("grok");
        let cmd = Grok.command(&session(&g, &[]));
        let a = args(&cmd);
        for flag in ["--trust", "--no-subagents", "--disable-web-search", "streaming-json", "--always-approve"] {
            assert!(a.contains(&flag.to_owned()), "{flag}");
        }
        assert!(a.windows(2).any(|w| w == ["--sandbox", "axeval"]));
        assert!(a.windows(2).any(|w| w == ["--tools", "run_terminal_cmd,read_file,grep,list_dir"]), "internal tool ids");
        assert!(!a.contains(&"--deny".to_owned()), "no isolation deny rules");
        assert!(a.windows(2).any(|w| w == ["--allow", "Bash(idax *)"]));
        assert_eq!(env(&cmd, "GROK_CLAUDE_SKILLS_ENABLED"), None, "compat scans stay as the user set them");
        assert_eq!(env(&cmd, "HOME"), None, "GROK_HOME and the login stay where they are");
    }

    #[test]
    fn a_cwd_directory_with_only_prompt_history_is_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        assert!(only_bookkeeping(dir.path()));
        std::fs::write(dir.path().join("prompt_history.jsonl"), "{}").unwrap();
        assert!(only_bookkeeping(dir.path()));
        std::fs::create_dir(dir.path().join("another-session")).unwrap();
        assert!(!only_bookkeeping(dir.path()), "another session's directory keeps it");
    }

    #[test]
    fn prepare_writes_a_profile_that_grants_engine_state_and_denies_other_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let g = group("grok");
        let mut s = session(&g, &[]);
        s.workspace = dir.path();
        Grok.prepare(&s).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".grok/sandbox.toml")).unwrap();
        let profile: toml::Value = toml::from_str(&text).unwrap();
        let p = &profile["profiles"]["axeval"];
        assert_eq!(p["extends"].as_str(), Some("workspace"));
        let home = std::env::var("HOME").unwrap();
        let granted = p["read_write"].as_array().unwrap();
        if std::path::Path::new(&format!("{home}/.idapro")).exists() {
            assert!(granted.iter().any(|v| v.as_str() == Some(&*format!("{home}/.idapro"))), "absolute, not ~");
        }
        assert!(granted.iter().all(|v| std::path::Path::new(v.as_str().unwrap()).exists()), "only existing state dirs");
        for denied in p["deny"].as_array().unwrap() {
            let denied = denied.as_str().unwrap();
            assert!(std::path::Path::new(denied).exists(), "only existing paths: {denied}");
            assert!(!denied.contains("/.grok") && !denied.contains(".hexlic"), "grok's own and the engine's license stay readable");
        }
    }

    #[test]
    fn a_stream_yields_tokens_tools_and_the_text_after_the_last_tool() {
        let stream = r#"
{"type":"text","data":"Let me look."}
{"type":"tool_call","toolCallId":"c1","toolName":"run_terminal_command","status":"in_progress","rawInput":{"command":"idax exec crackme"}}
{"type":"tool_call_update","toolCallId":"c1","status":"in_progress"}
{"type":"tool_call_update","toolCallId":"c1","status":"completed","rawOutput":{"type":"Bash","output":[115,51,99,114,101,116,95,52,50,10,35,32,114,101,99,111,114,100,115,32,49,45,49,32,111,102,32,51,32,115,104,111,119,110,59,32,102,117,108,108,32,111,117,116,112,117,116,32,105,110,32,47,99,32,40,103,114,101,112,32,105,116,41]}}
{"type":"usage","stopReason":"tool_use","usage":{"input_tokens":100,"output_tokens":10}}
{"type":"text","data":"The password is "}
{"type":"text","data":"s3cret_42."}
{"type":"usage","stopReason":"end_turn","usage":{"input_tokens":50,"output_tokens":8}}
{"type":"end","stopReason":"end_turn","sessionId":"01a1-sess","num_turns":2,"usage":{"input_tokens":150,"cache_read_input_tokens":900,"cache_creation_input_tokens":0,"output_tokens":18,"reasoning_tokens":5,"total_tokens":1068},"modelUsage":{"grok-4.7":{"costUSD":0.0021}},"total_cost_usd":0.0021}
"#;
        let p = Grok.parse(stream);
        let m = &p.metrics;
        assert!(m.completed && m.error.is_none());
        assert_eq!((m.tool_calls, m.tool_errors, m.truncations, m.turns), (1, 0, 1, 2));
        assert_eq!((m.tokens.input, m.tokens.cache_read, m.tokens.output, m.tokens.reasoning), (150, 900, 18, 5));
        assert_eq!(m.model.as_deref(), Some("grok-4.7"));
        assert_eq!(m.reported_cost_usd, Some(0.0021));
        assert_eq!(p.answer, "The password is s3cret_42.");
        assert_eq!(Grok.session_id(stream).as_deref(), Some("01a1-sess"));
    }

    #[test]
    fn an_auth_error_event_is_an_auth_failure() {
        assert!(Grok.auth_failure(r#"{"type":"error","message":"Not logged in. Please run grok login"}"#));
        assert!(!Grok.auth_failure(r#"{"type":"text","data":"401"}"#));
    }

    #[test]
    fn an_mcp_server_goes_into_the_project_config() {
        let toml = super::mcp_config(&crate::harness::test_support::mcp());
        assert_eq!(
            toml,
            "[mcp_servers.ida]\ncommand = \"/opt/ida-mcp\"\nargs = [\"stdio\",\"--agent=x\"]\nenabled = true\n\n[mcp_servers.ida.env]\n\"IDA_MCP_STATE_DIR\" = \"/tmp/axe-x/m\"\n"
        );
        let parsed: toml::Value = toml::from_str(&toml).unwrap();
        assert_eq!(parsed["mcp_servers"]["ida"]["args"][1].as_str(), Some("--agent=x"));
    }
}
