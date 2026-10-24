//! Launching an agent against one task in an isolated session, and reading
//! its event stream back. One adapter per harness family.
//!
//! The rules every adapter follows (README §harness):
//! - the session runs in the user's real environment (their skills, MCP
//!   config and global instructions are part of real use);
//! - never run login or logout, and never copy, move or rewrite credentials;
//! - after the run, remove exactly the history it created and nothing else;
//! - an authentication failure stops the whole eval at once.

mod claude;
mod codex;
mod grok;

pub use claude::Claude;
pub use codex::Codex;
pub use grok::Grok;

use crate::cleanup::Report;
use crate::metrics::Metrics;
use crate::suite::{Group, McpServer};
use anyhow::Result;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What one session needs.
pub struct Session<'a> {
    pub group: &'a Group,
    /// Unique per run: its path embeds `run_id`.
    pub workspace: &'a Path,
    /// A random id, unique to this run, in the workspace path.
    pub run_id: &'a str,
    /// A short private directory for the tool's runtime and cache.
    pub tmp: &'a Path,
    pub prompt: &'a str,
    pub timeout: u64,
    /// Environment for the run (suite + group, templated, `PATH` included).
    pub env: &'a [(String, String)],
    /// The group's MCP server, templated, when it has one.
    pub mcp: Option<&'a McpServer>,
}

/// A harness's reading of its own stream.
#[derive(Debug, Default)]
pub struct Parsed {
    pub metrics: Metrics,
    pub answer: String,
}

/// One shell command the agent ran, as the stream recorded it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Call {
    /// The command line, unwrapped from any `sh -lc '...'` the harness adds.
    pub command: String,
    /// What the agent saw: stdout and stderr as the harness captured them.
    pub output: String,
    /// The harness marked the call failed or saw a non-zero exit.
    pub failed: bool,
}

pub trait Harness: Sync {
    fn name(&self) -> &'static str;
    /// The executable when the group names none.
    fn default_exe(&self) -> &'static str;
    /// The process that runs `session` and writes its event stream to stdout.
    fn command(&self, session: &Session<'_>) -> Command;
    /// Where a project-level skill named `name` goes in the workspace.
    fn skill_file(&self, session: &Session<'_>, name: &str) -> PathBuf;
    /// Write anything the session needs in its workspace before launch.
    ///
    /// # Errors
    ///
    /// A file could not be written.
    fn prepare(&self, _session: &Session<'_>) -> Result<()> {
        Ok(())
    }
    /// A lock on global state the harness writes while it starts, without
    /// locking of its own; the runner holds it from spawn until
    /// [`Harness::started`] (or a short cap), so no other eval session
    /// starts or cleans up in between.
    fn startup_lock(&self) -> Option<PathBuf> {
        None
    }
    /// Has the launched session finished its startup writes?
    fn started(&self, _session: &Session<'_>) -> bool {
        true
    }
    /// Read a whole stream into metrics and the final answer.
    fn parse(&self, stream: &str) -> Parsed;
    /// The shell commands the agent ran, in order, with what they printed.
    fn calls(&self, stream: &str) -> Vec<Call>;
    /// Does this one stream line show the harness failed to authenticate? The
    /// runner kills the session on the first such line, before any retry.
    fn auth_failure(&self, line: &str) -> bool;
    /// Was the session refused for rate limiting (worth a delayed retry)?
    fn rate_limited(&self, stream: &str) -> bool;
    /// The session id the stream reported, the key of its history entries.
    fn session_id(&self, stream: &str) -> Option<String>;
    /// Remove the history this run left in the harness's own store: entries
    /// named by `session_id` or by the run's unique workspace, nothing else.
    fn cleanup(&self, session: &Session<'_>, session_id: Option<&str>) -> Report;
    /// Counts of the harness's history entries, total and eval-made (their
    /// path holds `/target/eval-data/`), to check nothing leaked or was
    /// lost across a whole eval.
    fn inventory(&self) -> Vec<(String, usize, usize)>;
}

/// Marks paths that only an eval workspace has.
pub const EVAL_MARK: &str = "/target/eval-data/";

/// Pick an adapter by family name.
///
/// # Errors
///
/// The family is not one this build knows.
pub fn by_name(name: &str) -> Result<&'static dyn Harness> {
    match name {
        "claude" => Ok(&Claude),
        "codex" => Ok(&Codex),
        "grok" => Ok(&Grok),
        other => anyhow::bail!("unknown harness {other:?}; known: claude, codex, grok"),
    }
}

/// The suite and group environment, templated and merged (group wins).
#[must_use]
pub fn merge_env(suite_env: &BTreeMap<String, String>, group: &Group, expand: impl Fn(&str) -> String) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in suite_env.iter().chain(&group.env) {
        merged.insert(k.clone(), expand(v));
    }
    merged.into_iter().collect()
}

/// The group's executable, else `AXEVAL_<HARNESS>_EXE` from the runner's
/// environment (a machine's own launcher, kept out of the suite), else the
/// adapter's default.
pub(crate) fn exe(harness: &dyn Harness, group: &Group) -> OsString {
    group
        .exe
        .as_ref()
        .map(OsString::from)
        .or_else(|| std::env::var_os(format!("AXEVAL_{}_EXE", harness.name().to_ascii_uppercase())))
        .unwrap_or_else(|| harness.default_exe().into())
}

/// Extra credential files to keep agents away from, from the runner's
/// `AXEVAL_CREDENTIALS` (`:`-separated): ones only this machine has.
pub(crate) fn extra_credentials() -> Vec<String> {
    std::env::var("AXEVAL_CREDENTIALS").map(|v| v.split(':').filter(|p| !p.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
}

/// Does an *error message* (never tool output, which can hold `0x401000`)
/// carry one of the auth-failure markers?
pub(crate) fn mentions_auth_failure(message: &str) -> bool {
    let l = message.to_ascii_lowercase();
    [
        "401",
        "403 forbidden",
        "unauthorized",
        "authentication_failed",
        "authentication failed",
        "failed to authenticate",
        "invalid api key",
        "invalid_api_key",
        "not logged in",
        "oauth session expired",
        "please log in",
        "please run /login",
        "login required",
    ]
    .iter()
    .any(|m| l.contains(m))
}

pub(crate) fn mentions_rate_limit(stream: &str) -> bool {
    let l = stream.to_ascii_lowercase();
    l.contains("rate_limit") || l.contains("rate limit") || l.contains("\"429\"") || l.contains("status 429") || l.contains("overloaded")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn group(harness: &str) -> Group {
        Group {
            name: "g".into(),
            harness: harness.into(),
            exe: None,
            model: Some("m".into()),
            skill: None,
            tools: vec![],
            permission_mode: None,
            allow: vec!["Bash(idax *)".into()],
            deny: vec![],
            sandbox: None,
            max_turns: Some(20),
            args: vec![],
            env: BTreeMap::new(),
            mcp: None,
            tool: None,
            rules: String::new(),
            bare_path: false,
        }
    }

    pub fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    pub fn env(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs().find(|(k, _)| *k == key).map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    pub fn mcp() -> McpServer {
        McpServer {
            name: "ida".into(),
            command: "/opt/ida-mcp".into(),
            args: vec!["stdio".into(), "--agent=x".into()],
            env: BTreeMap::from([("IDA_MCP_STATE_DIR".to_owned(), "/tmp/axe-x/m".to_owned())]),
        }
    }

    pub fn session<'a>(group: &'a Group, env: &'a [(String, String)]) -> Session<'a> {
        Session {
            group,
            workspace: Path::new("/x/target/eval-data/ws/g.t.0.rid1"),
            run_id: "rid1",
            tmp: Path::new("/tmp/axe-x"),
            prompt: "do it",
            timeout: 60,
            env,
            mcp: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_resolves_and_unknown_ones_are_named() {
        for name in ["claude", "codex", "grok"] {
            assert_eq!(by_name(name).unwrap().name(), name);
        }
        assert!(by_name("gemini").err().unwrap().to_string().contains("known: claude, codex, grok"));
    }

    #[test]
    fn auth_markers_are_recognized() {
        assert!(mentions_auth_failure("Failed to authenticate: OAuth session expired"));
        assert!(mentions_auth_failure("unexpected status 401 Unauthorized"));
        assert!(!mentions_auth_failure("the password is s3cret_42"));
    }

    #[test]
    fn env_merge_lets_the_group_win() {
        let mut suite = BTreeMap::new();
        suite.insert("A".to_owned(), "{tmp}/a".to_owned());
        suite.insert("B".to_owned(), "keep".to_owned());
        let mut g = test_support::group("claude");
        g.env.insert("A".to_owned(), "override".to_owned());
        let env = merge_env(&suite, &g, |v| v.replace("{tmp}", "/t"));
        assert_eq!(env, [("A".to_owned(), "override".to_owned()), ("B".to_owned(), "keep".to_owned())]);
    }
}
