//! A per-user daemon that keeps a tool's sessions resident (CONVENTIONS §8).
//!
//! A command is one short-lived process; the database it asks about stays
//! open in the daemon between commands. The client sends its argv, working
//! directory, the environment variables that shape output and whether its
//! stdout is a terminal; the daemon runs the command through the tool's
//! [`Handler`] and relays stdout, stderr, progress and the exit status back.
//! The same handler runs the command in-process when there is no daemon
//! (`<TOOL>_DAEMON=off`), so both print the same bytes.
//!
//! Unlike a cache, the daemon holds state: open databases and their unsaved
//! changes. So a daemon of a replaced build is only told to retire — it exits
//! once its sessions are gone — and only `stop --force` (or idling out) ends
//! one that still holds sessions.
//!
//! One daemon serves one build of one executable: the socket is named by a
//! hash of the executable's path and the settings that shape output
//! ([`Config::fingerprint`]), then of its version, size, mtime and the
//! protocol version. The socket lives in a directory only the user can enter,
//! and both ends check the other's uid. A client that disconnects (Ctrl-C)
//! cancels its command.
//!
//! When no daemon can be reached the command fails, as its sessions live
//! there: running it elsewhere would lose them without a word.
//!
//! Unix and Windows (10 1803+, for its AF_UNIX sockets); elsewhere
//! [`forward`] returns `None` and the command runs in-process.

mod frame;
#[cfg(any(unix, windows))]
mod socket;
#[cfg(any(unix, windows))]
mod sys;

use crate::agent::{self, Mode};
use crate::exit::{self, Exit};
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// How a tool runs its daemon.
#[derive(Clone, Debug)]
pub struct Config {
    /// Lower-case tool name: names directories, and in upper case the
    /// `<TOOL>_*` environment variables.
    pub tool: &'static str,
    /// The tool's version, part of the build identity.
    pub version: &'static str,
    /// Arguments that make the executable serve as the daemon.
    pub serve_args: Vec<OsString>,
    /// Settings that shape output and are fixed for a daemon's lifetime
    /// (resolved configuration file, say). Daemons with different
    /// fingerprints run side by side.
    pub fingerprint: Vec<u8>,
    /// Exit after this long without requests; open sessions are dropped
    /// unsaved.
    pub idle: Duration,
    /// How long a client waits for a daemon it started to come up.
    pub start_wait: Duration,
    /// How long `stop` waits for a stopped daemon to exit: it closes its
    /// sessions first.
    pub stop_wait: Duration,
    /// Directory of the sockets; default `$<TOOL>_RUNTIME_DIR`, else
    /// `$XDG_RUNTIME_DIR/<tool>` (Windows: `%LOCALAPPDATA%\<tool>`). When
    /// that is unset or too long for a socket path, `<temp>/<tool>-<uid>`,
    /// then `/tmp/<tool>-<uid>` (Windows: `<temp>\<tool>-<USERNAME>`).
    pub runtime_dir: Option<PathBuf>,
    /// Whether a command line reads stdin (`exec -`, say): then the client
    /// reads it and sends it with the request ([`Request::stdin`]), as only
    /// the client has that stdin.
    pub wants_stdin: fn(&[OsString]) -> bool,
}

impl Config {
    pub fn new(tool: &'static str, version: &'static str) -> Config {
        Config {
            tool,
            version,
            serve_args: vec!["daemon".into(), "serve".into()],
            fingerprint: Vec::new(),
            idle: Duration::from_secs(15 * 60),
            start_wait: Duration::from_secs(5),
            stop_wait: Duration::from_secs(30),
            runtime_dir: None,
            wants_stdin: |_| false,
        }
    }

    /// `<TOOL>_<suffix>`.
    pub fn env_name(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.tool.to_ascii_uppercase())
    }

    /// `<TOOL>_DAEMON=off`: every command runs in its own process.
    pub fn is_disabled(&self) -> bool {
        std::env::var_os(self.env_name("DAEMON")).is_some_and(|v| v == "off")
    }

    /// Variables a command may read, so the client sends them along: the
    /// tool's own (`<TOOL>_*` but the daemon's switches), axkit's, harness
    /// markers and `NO_COLOR`.
    fn forwards(&self, name: &OsStr) -> bool {
        let Some(name) = name.to_str() else { return false };
        let own = self.env_name("");
        let switch = [self.env_name("DAEMON"), self.env_name("RUNTIME_DIR")];
        (name.starts_with(&own) && !switch.iter().any(|s| s == name))
            || name.starts_with("AXKIT_")
            || name == "NO_COLOR"
            || agent::is_marker(name)
    }
}

/// One command as the client gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub cwd: PathBuf,
    /// The full argv, program name first.
    pub args: Vec<OsString>,
    /// The forwarded variables only (see [`Config`]); read them with
    /// [`Request::var`], never from the daemon's own environment.
    pub env: Vec<(OsString, OsString)>,
    pub stdout_is_terminal: bool,
    pub stderr_is_terminal: bool,
    /// The client's stdin, read whole, for a command line that
    /// [`Config::wants_stdin`]; `None` otherwise.
    pub stdin: Option<Vec<u8>>,
}

impl Request {
    /// This process's command; its stdin too when the command line
    /// [`Config::wants_stdin`].
    ///
    /// # Errors
    ///
    /// The working directory or stdin cannot be read.
    pub fn capture(config: &Config) -> io::Result<Request> {
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(k, _)| config.forwards(k)).collect();
        env.sort();
        let args: Vec<OsString> = std::env::args_os().collect();
        let stdin = if (config.wants_stdin)(&args) { Some(read_stdin()?) } else { None };
        Ok(Request {
            cwd: std::env::current_dir()?,
            args,
            env,
            stdout_is_terminal: io::stdout().is_terminal(),
            stderr_is_terminal: io::stderr().is_terminal(),
            stdin,
        })
    }

    /// A command this process makes up rather than was given (an MCP tool
    /// call): `args` in this process's directory and forwarded
    /// environment, no terminal, no stdin.
    ///
    /// # Errors
    ///
    /// The working directory cannot be read.
    pub fn new(config: &Config, args: Vec<OsString>) -> io::Result<Request> {
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(k, _)| config.forwards(k)).collect();
        env.sort();
        Ok(Request { cwd: std::env::current_dir()?, args, env, stdout_is_terminal: false, stderr_is_terminal: false, stdin: None })
    }

    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_os_str())
    }

    /// Whether the command asks for JSON (`--json`), read without parsing
    /// it: for an error that parsing raised.
    pub fn wants_json(&self) -> bool {
        exit::wants_json(&self.args)
    }

    /// Who reads this command's output.
    pub fn mode(&self) -> Mode {
        Mode::from_env(|name| self.var(name).map(OsStr::to_os_string), self.stdout_is_terminal)
    }

    /// `path` as the client means it: relative to its working directory.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        self.cwd.join(path)
    }

    /// A file the client named, resolved and canonical: every spelling of one
    /// file gives the same path, so it can key per-file state.
    ///
    /// # Errors
    ///
    /// The file does not exist or cannot be resolved.
    pub fn canonical_path(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(self.resolve(path))
    }
}

/// This process's stdin, read whole once: a command that falls back from
/// the daemon to running here captures its request again.
fn read_stdin() -> io::Result<Vec<u8>> {
    static STDIN: OnceLock<Result<Vec<u8>, (io::ErrorKind, String)>> = OnceLock::new();
    let read = STDIN.get_or_init(|| {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf).map(|_| buf).map_err(|e| (e.kind(), e.to_string()))
    });
    read.clone().map_err(|(kind, message)| io::Error::new(kind, message))
}

/// Set once the client has gone; a long command polls it and stops.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Where a command writes: the process's own streams, or a daemon connection.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// `None`: progress lines go to `err`.
    progress: Option<&'a mut dyn Write>,
    cancel: Cancel,
}

impl<'a> Io<'a> {
    pub fn new(out: &'a mut dyn Write, err: &'a mut dyn Write, cancel: Cancel) -> Io<'a> {
        Io { out, err, progress: None, cancel }
    }

    /// One progress line (CONVENTIONS §7). It goes to stderr in the end
    /// either way; a failing stderr is ignored, there is nowhere left to
    /// report it.
    pub fn progress(&mut self, line: &str) {
        let sink: &mut dyn Write = match self.progress.as_mut() {
            Some(p) => &mut **p,
            None => &mut *self.err,
        };
        let _ = writeln!(sink, "{line}").and_then(|()| sink.flush());
    }

    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }
}

/// What a handler did with a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Exit(Exit),
    /// Not run; the client runs it in its own process. Only for commands
    /// that do not touch a session (`--help`, `ref`): a session lives in
    /// the daemon, and running elsewhere would miss it.
    Decline(String),
}

/// A tool's commands, run by the daemon or in-process.
///
/// `run` is called from one thread per connection, concurrently. It must
/// never exit the process: render clap's `--help` into `io.out` instead of
/// calling `clap::Error::exit`.
///
/// Nor may it rely on process attributes bound to the calling thread, since
/// that thread ends with the connection: a child spawned with
/// `PR_SET_PDEATHSIG` is killed then, not when the daemon exits.
pub trait Handler: Sync {
    fn run(&self, request: &Request, io: &mut Io<'_>) -> Outcome;

    /// Lines for `daemon status`: open sessions, say. `idle_left` is how
    /// long the daemon goes on without a request before it exits.
    fn status(&self, idle_left: Duration) -> String {
        let _ = idle_left;
        String::new()
    }

    /// Whether sessions are open; a retiring daemon waits for none.
    fn holds_sessions(&self) -> bool {
        false
    }

    /// Called once when the daemon exits, after the last request; open
    /// sessions are dropped without saving.
    fn shutdown(&self) {}
}

/// Run this process's command in the daemon, starting one if none is up:
/// the exit status, or `None` to run it here — the daemon is off
/// (`<TOOL>_DAEMON=off`) or declined. An unreachable daemon is reported on
/// stderr and its exit status returned.
pub fn forward(config: &Config) -> Option<u8> {
    if config.is_disabled() {
        return None;
    }
    #[cfg(any(unix, windows))]
    {
        socket::forward(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Run `request` in the daemon as [`forward`] does, its output written to
/// `out` and `err` instead of this process's: the exit status, or `None`
/// to run it here.
pub fn call(config: &Config, request: &Request, out: &mut impl Write, err: &mut impl Write) -> Option<u8> {
    if config.is_disabled() {
        return None;
    }
    #[cfg(any(unix, windows))]
    {
        socket::call(config, request, out, err)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (request, out, err);
        None
    }
}

/// Serve as the daemon until idle or stopped. Returns at once when another
/// daemon of the same identity is already up.
///
/// # Errors
///
/// No private socket directory, or the socket cannot be bound.
pub fn serve(config: &Config, handler: &impl Handler) -> io::Result<()> {
    #[cfg(any(unix, windows))]
    {
        socket::serve(config, handler)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, handler);
        Err(io::Error::new(io::ErrorKind::Unsupported, "the daemon needs Unix sockets"))
    }
}

/// One status line per running daemon of this tool.
///
/// # Errors
///
/// No private socket directory.
pub fn status(config: &Config) -> io::Result<Vec<String>> {
    #[cfg(any(unix, windows))]
    {
        socket::status(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = config;
        Ok(Vec::new())
    }
}

/// Stop every daemon of this tool; `force` also those holding sessions,
/// whose unsaved changes are lost. Returns how many were stopped, once they
/// have exited: their socket is gone and their lock released.
///
/// # Errors
///
/// No private socket directory, or a stopped daemon was still running after
/// [`Config::stop_wait`].
pub fn stop(config: &Config, force: bool) -> io::Result<usize> {
    #[cfg(any(unix, windows))]
    {
        socket::stop(config, force)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, force);
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_variables_that_shape_output_are_forwarded() {
        let c = Config::new("idax", "0");
        let fwd = |n: &str| c.forwards(OsStr::new(n));
        assert!(fwd("IDAX_LIMIT") && fwd("AXKIT_AGENT") && fwd("CLAUDECODE") && fwd("NO_COLOR"));
        assert!(!fwd("IDAX_DAEMON") && !fwd("IDAX_RUNTIME_DIR"), "the daemon's own switches stay with the client");
        assert!(!fwd("PATH") && !fwd("BNX_LIMIT"));
    }

    #[test]
    fn a_request_reads_its_own_environment_and_terminal() {
        let req = Request {
            cwd: PathBuf::from("/work"),
            args: vec!["idax".into()],
            env: vec![("CLAUDECODE".into(), "1".into())],
            stdout_is_terminal: false,
            stderr_is_terminal: false,
            stdin: None,
        };
        assert_eq!(req.mode(), Mode::Agent);
        assert_eq!(Request { stdout_is_terminal: true, ..req.clone() }.mode(), Mode::Human);
        assert_eq!(req.resolve(Path::new("a.i64")), Path::new("/work/a.i64"));
        assert_eq!(req.resolve(Path::new("/abs/a.i64")), Path::new("/abs/a.i64"));
    }

    #[test]
    fn local_progress_goes_to_stderr() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io::new(&mut out, &mut err, Cancel::default());
        io.progress("# progress: 10%");
        assert!(out.is_empty());
        assert_eq!(err, b"# progress: 10%\n");
    }
}
