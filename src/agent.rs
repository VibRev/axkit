//! Telling an agent from a person (CONVENTIONS §10).
//!
//! Agent mode changes defaults only — the window, progress cadence — never
//! the format: the same command shows an agent and a person the same records.

use crate::output::Window;
use std::ffi::OsString;
use std::io::IsTerminal;

/// Forces agent mode on (`on`) or off (`off`), over detection.
pub const OVERRIDE_ENV: &str = "AXKIT_AGENT";

/// Default `--max-bytes` for an agent. It has to stay below every harness's
/// own cut (the smallest seen is codex's ~10 KiB), or that cut swallows our
/// hint line.
pub const AGENT_MAX_BYTES: usize = 8192;

/// Variables some harness sets in the commands it runs.
const MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CODEX_CI",
    "CODEX_SANDBOX",
    "CODEX_THREAD_ID",
    "CURSOR_AGENT",
    "GEMINI_CLI",
    "COPILOT_AGENT",
    "COPILOT_CLI",
    "COPILOT_AGENT_SESSION_ID",
    "AI_AGENT",
    "AGENT",
];

/// Whether `name` is a variable some harness sets.
pub(crate) fn is_marker(name: &str) -> bool {
    MARKERS.contains(&name)
}

/// Who reads the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Human,
    Agent,
}

impl Mode {
    /// Detect from this process's environment and stdout.
    pub fn detect() -> Mode {
        Mode::from_env(|name| std::env::var_os(name), std::io::stdout().is_terminal())
    }

    /// Agent mode when a harness marker is set and stdout is not a terminal;
    /// [`OVERRIDE_ENV`] decides over both.
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>, stdout_is_terminal: bool) -> Mode {
        let set = |name: &str| var(name).is_some_and(|v| !v.is_empty() && v != "0");
        match var(OVERRIDE_ENV).as_ref().and_then(|v| v.to_str()) {
            Some("on" | "1") => Mode::Agent,
            Some("off" | "0") => Mode::Human,
            _ if stdout_is_terminal => Mode::Human,
            _ if MARKERS.iter().any(|m| set(m)) => Mode::Agent,
            _ => Mode::Human,
        }
    }

    /// The window a command uses when no window flag is given.
    pub fn default_window(self) -> Window {
        match self {
            Mode::Human => Window::default(),
            Mode::Agent => Window { max_bytes: Some(AGENT_MAX_BYTES), ..Window::default() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn a_harness_marker_on_a_pipe_is_an_agent() {
        assert_eq!(Mode::from_env(env(&[("CLAUDECODE", "1")]), false), Mode::Agent);
        assert_eq!(Mode::from_env(env(&[("CODEX_SANDBOX", "seatbelt")]), false), Mode::Agent);
    }

    #[test]
    fn a_terminal_or_no_marker_is_a_person() {
        assert_eq!(Mode::from_env(env(&[("CLAUDECODE", "1")]), true), Mode::Human);
        assert_eq!(Mode::from_env(env(&[]), false), Mode::Human);
        assert_eq!(Mode::from_env(env(&[("CLAUDECODE", "0")]), false), Mode::Human);
    }

    #[test]
    fn the_override_wins() {
        assert_eq!(Mode::from_env(env(&[("AXKIT_AGENT", "on")]), true), Mode::Agent);
        assert_eq!(Mode::from_env(env(&[("AXKIT_AGENT", "off"), ("CLAUDECODE", "1")]), false), Mode::Human);
    }

    #[test]
    fn only_agents_get_a_default_window() {
        assert!(Mode::Human.default_window().is_unbounded());
        assert_eq!(Mode::Agent.default_window().max_bytes, Some(AGENT_MAX_BYTES));
    }
}
