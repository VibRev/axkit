//! Exit codes and error reports (CONVENTIONS §6).

use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};

/// A process exit status with a meaning an agent can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Success,
    /// The result is unusable; read stderr, fix, retry.
    Error,
    /// The worker crashed or was killed; unsaved changes may be gone.
    SessionLost,
    /// SIGINT.
    Interrupted,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Error => 1,
            Exit::SessionLost => 3,
            Exit::Interrupted => 130,
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(exit: Exit) -> Self {
        std::process::ExitCode::from(exit.code())
    }
}

/// What went wrong, as the stable `kind` of an error report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    Usage,
    NotFound,
    TargetUnavailable,
    TargetLocked,
    AnalysisIncomplete,
    ScriptError,
    Timeout,
    SessionLost,
    EngineError,
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::NotFound => "not_found",
            ErrorKind::TargetUnavailable => "target_unavailable",
            ErrorKind::TargetLocked => "target_locked",
            ErrorKind::AnalysisIncomplete => "analysis_incomplete",
            ErrorKind::ScriptError => "script_error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::SessionLost => "session_lost",
            ErrorKind::EngineError => "engine_error",
            ErrorKind::Internal => "internal",
        }
    }

    /// The exit status this kind of error ends the process with. A timeout
    /// kills the worker, so it loses the session like a crash does.
    pub fn exit(self) -> Exit {
        match self {
            ErrorKind::Timeout | ErrorKind::SessionLost => Exit::SessionLost,
            ErrorKind::Usage
            | ErrorKind::NotFound
            | ErrorKind::TargetUnavailable
            | ErrorKind::TargetLocked
            | ErrorKind::AnalysisIncomplete
            | ErrorKind::ScriptError
            | ErrorKind::EngineError
            | ErrorKind::Internal => Exit::Error,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error report: what the process says on stderr before it exits.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub kind: ErrorKind,
    pub message: String,
    /// The next step, ideally a command that can be pasted as it is.
    pub hint: Option<String>,
    /// Boxed: a failure travels in every `Result` of a command, and a bare
    /// JSON value would make each of them large.
    pub details: Option<Box<Value>>,
}

impl Failure {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Failure {
        Failure { kind, message: message.into(), hint: None, details: None }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Failure {
        self.hint = Some(hint.into());
        self
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Failure {
        self.details = Some(Box::new(details));
        self
    }

    pub fn exit(&self) -> Exit {
        self.kind.exit()
    }

    /// The report for `--json`: one line, `{"error":{...}}`.
    pub fn to_json(&self) -> String {
        let mut error = Map::new();
        error.insert("kind".into(), json!(self.kind.as_str()));
        error.insert("message".into(), json!(self.message));
        if let Some(hint) = &self.hint {
            error.insert("hint".into(), json!(hint));
        }
        if let Some(details) = &self.details {
            error.insert("details".into(), (**details).clone());
        }
        json!({ "error": error }).to_string()
    }

    /// Write the report to `err` (stderr): text, or the JSON line as the last
    /// line when `json`.
    ///
    /// # Errors
    ///
    /// Writing to `err` failed.
    pub fn report(&self, err: &mut (impl Write + ?Sized), json: bool) -> io::Result<()> {
        if json { writeln!(err, "{}", self.to_json()) } else { write!(err, "{self}") }
    }

    /// clap's own parse error as a usage failure, so it reads and exits like
    /// every other error. `None` for `--help` / `--version`, which are not
    /// errors: let clap print those (`e.exit()`).
    #[cfg(feature = "clap")]
    pub fn from_clap(e: &clap::Error) -> Option<Failure> {
        use clap::error::ErrorKind as Clap;
        if matches!(e.kind(), Clap::DisplayHelp | Clap::DisplayVersion | Clap::DisplayHelpOnMissingArgumentOrSubcommand) {
            return None;
        }
        let rendered = e.render().to_string();
        let mut lines = rendered.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next().unwrap_or_default();
        let mut message = first.strip_prefix("error: ").unwrap_or(first).to_owned();
        // "the following required arguments were not provided:" names them on the next line.
        if message.ends_with(':')
            && let Some(what) = lines.next()
        {
            message = format!("{message} {what}");
        }
        let hint: Vec<&str> = lines.collect();
        let hint = if hint.is_empty() { "--help lists the commands and their flags".to_owned() } else { hint.join("; ") };
        Some(Failure::new(ErrorKind::Usage, message).with_hint(hint))
    }
}

/// Whether a command line asks for JSON, read without parsing it: for
/// reporting an error that parsing itself raised. `--json` counts before a
/// `--` only.
pub fn wants_json(args: &[OsString]) -> bool {
    args.iter().take_while(|a| *a != "--").any(|a| a == "--json")
}

/// The text report: `Error: <kind>: <message>`, then `hint: ...`.
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Error: {}: {}", self.kind, self.message)?;
        if let Some(hint) = &self.hint {
            writeln!(f, "hint: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_report_has_kind_message_and_hint() {
        let f = Failure::new(ErrorKind::NotFound, "no function named mian").with_hint("idax exec fw.i64 -c 'db.functions'");
        assert_eq!(f.to_string(), "Error: not_found: no function named mian\nhint: idax exec fw.i64 -c 'db.functions'\n");
        assert_eq!(f.exit(), Exit::Error);
    }

    #[test]
    fn json_report_is_one_line() {
        let f = Failure::new(ErrorKind::TargetLocked, "locked\nby pid 42").with_details(json!({"pid": 42}));
        let mut err = Vec::new();
        f.report(&mut err, true).unwrap();
        let text = String::from_utf8(err).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["kind"], "target_locked");
        assert_eq!(v["error"]["details"]["pid"], 42);
        assert!(v["error"].get("hint").is_none());
    }

    #[test]
    fn json_is_wanted_by_the_flag() {
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(wants_json(&args(&["idax", "exec", "t", "1", "--json"])));
        assert!(!wants_json(&args(&["idax", "exec", "t", "--", "--json"])), "past `--` it is someone else's");
        assert!(!wants_json(&args(&["idax", "list"])));
    }

    #[test]
    fn a_failure_stays_small() {
        assert!(std::mem::size_of::<Failure>() <= 64, "{}", std::mem::size_of::<Failure>());
    }

    #[test]
    fn a_timeout_loses_the_session() {
        assert_eq!(ErrorKind::Timeout.exit().code(), 3);
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_errors_become_usage_failures_and_help_does_not() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        #[command(name = "idax")]
        struct Cli {
            #[arg(long)]
            limit: Option<usize>,
        }
        let e = Cli::try_parse_from(["idax", "--limt", "3"]).unwrap_err();
        let f = Failure::from_clap(&e).unwrap();
        assert_eq!(f.kind, ErrorKind::Usage);
        assert!(f.message.contains("--limt"), "{}", f.message);
        assert!(f.hint.as_deref().is_some_and(|h| h.contains("--limit")), "{f:?}");
        assert_eq!(f.exit().code(), 1, "not clap's 2");

        let help = Cli::try_parse_from(["idax", "--help"]).unwrap_err();
        assert!(Failure::from_clap(&help).is_none());
    }
}
