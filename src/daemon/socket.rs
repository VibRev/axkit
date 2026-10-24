use super::frame::{PROTOCOL, Tag, fields, put, read_frame, write_frame};
use super::sys::{self, SUN_PATH_MAX, UnixListener, UnixStream};
use super::{Cancel, Config, Handler, Io, Outcome, Request};
use crate::exit::{ErrorKind, Exit, Failure};
use crate::output::cache_dir;
use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Bytes of stdout the daemon buffers before relaying them.
const FRAME_BYTES: usize = 64 << 10;
/// How often the daemon checks whether to exit.
const IDLE_POLL: Duration = Duration::from_millis(200);

pub(super) fn forward(config: &Config) -> Option<u8> {
    let request = match Request::capture(config) {
        Ok(r) => r,
        Err(e) => {
            let json = crate::exit::wants_json(&std::env::args_os().collect::<Vec<_>>());
            return unreachable(
                config,
                json,
                &Unreachable::new(format!("cannot read the working directory or stdin: {e}"), ""),
                &mut io::stderr(),
            );
        }
    };
    let (stdout, stderr) = (io::stdout(), io::stderr());
    call(config, &request, &mut stdout.lock(), &mut stderr.lock())
}

pub(super) fn call(config: &Config, request: &Request, out: &mut impl Write, err: &mut impl Write) -> Option<u8> {
    let json = request.wants_json();
    let (id, stream) = match reach(config) {
        Ok(reached) => reached,
        Err(why) => return unreachable(config, json, &why, err),
    };
    match relay(stream, &encode(&id, request), out, err, config.tool) {
        Relayed::Exit(code) => Some(code),
        Relayed::Declined => None,
        Relayed::Vanished => {
            unreachable(config, json, &Unreachable::new("the daemon closed the connection before answering", &log_hint(config)), err)
        }
    }
}

/// Why no daemon could be reached, and what to do about it.
struct Unreachable {
    why: String,
    hint: String,
}

impl Unreachable {
    fn new(why: impl Into<String>, hint: &str) -> Unreachable {
        Unreachable { why: why.into(), hint: hint.to_owned() }
    }
}

fn log_hint(config: &Config) -> String {
    format!("its log is {}", cache_dir(config.tool).join("daemon.log").display())
}

/// A connection to this build's daemon, started if none is up.
fn reach(config: &Config) -> Result<(Identity, UnixStream), Unreachable> {
    let id = Identity::of(config).map_err(|e| Unreachable::new(format!("cannot identify this executable: {e}"), ""))?;
    let dir = socket_dir(config).map_err(|e| {
        let example = if cfg!(windows) {
            format!("a folder under %LOCALAPPDATA%, say %LOCALAPPDATA%\\{}", config.tool)
        } else {
            format!("mkdir -m 700 /tmp/{}-dir", config.tool)
        };
        let hint = format!("set {} to a short directory only you can enter ({example})", config.env_name("RUNTIME_DIR"));
        Unreachable::new(e.to_string(), &hint)
    })?;
    let sock = dir.join(id.socket_name());
    if let Some(stream) = connect(&sock) {
        return Ok((id, stream));
    }
    let child = spawn(config).map_err(|e| Unreachable::new(format!("cannot start the daemon: {e}"), ""))?;
    match wait_for(&sock, child, config.start_wait) {
        Some(stream) => Ok((id, stream)),
        None => Err(Unreachable::new(format!("the daemon did not come up within {} s", config.start_wait.as_secs()), &log_hint(config))),
    }
}

/// No daemon: the command fails, as its sessions live there (CONVENTIONS §8).
fn unreachable(config: &Config, json: bool, why: &Unreachable, err: &mut impl Write) -> Option<u8> {
    let off = format!("{}=off runs commands without the daemon, and without sessions between them", config.env_name("DAEMON"));
    let hint = if why.hint.is_empty() { off } else { format!("{}; {off}", why.hint) };
    let failure = Failure::new(ErrorKind::TargetUnavailable, format!("no {} daemon: {}", config.tool, why.why)).with_hint(hint);
    let _ = failure.report(err, json);
    Some(failure.exit().code())
}

pub(super) fn serve(config: &Config, handler: &impl Handler) -> io::Result<()> {
    let id = Identity::of(config)?;
    let dir = socket_dir(config)?;
    // One daemon per identity: the lock is held for the daemon's lifetime.
    let lock_path = dir.join(format!("{}.lock", id.name()));
    let lock = File::create(&lock_path)?;
    if !try_lock(&lock) {
        return Ok(());
    }
    let sock = dir.join(id.socket_name());
    let _ = fs::remove_file(&sock); // a dead daemon's: we hold the lock
    let listener = UnixListener::bind(&sock)?;
    sys::restrict(&sock)?;
    retire_older_builds(&dir, &id);
    let daemon = Daemon {
        id,
        sock,
        idle: config.idle,
        started: Instant::now(),
        last: Mutex::new(Instant::now()),
        active: AtomicUsize::new(0),
        served: AtomicUsize::new(0),
        quitting: AtomicBool::new(false),
        retiring: AtomicBool::new(false),
        superseded: AtomicBool::new(false),
    };
    std::thread::scope(|scope| {
        scope.spawn(|| daemon.watch_idle(config, handler));
        for stream in listener.incoming() {
            if daemon.quitting.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            if !sys::same_user(&stream) {
                continue;
            }
            let daemon = &daemon;
            scope.spawn(move || daemon.handle(stream, handler));
        }
    });
    handler.shutdown();
    // A newer build has the executable's path now, so no daemon of this
    // identity can start again: its lock file would only pile up.
    if daemon.superseded.load(Ordering::SeqCst) {
        let _ = fs::remove_file(&lock_path);
    }
    drop(lock);
    Ok(())
}

pub(super) fn status(config: &Config) -> io::Result<Vec<String>> {
    let dir = socket_dir(config)?;
    Ok(own_sockets(&dir, config)?.iter().filter_map(|s| ask(s, Tag::Status, &[])).collect())
}

/// The sockets of this family's daemons, whatever their build. A fallback
/// directory (`<temp>/<tool>-<uid>`) is shared by every runtime directory
/// too long for a socket, so it can hold other families' daemons: those of
/// another HOME or cache, or another copy of the executable.
fn own_sockets(dir: &Path, config: &Config) -> io::Result<Vec<PathBuf>> {
    let family = Identity::of(config)?.family;
    Ok(sockets(dir).into_iter().filter(|s| Identity::of_socket(s).is_some_and(|o| o.family == family)).collect())
}

pub(super) fn stop(config: &Config, force: bool) -> io::Result<usize> {
    let dir = socket_dir(config)?;
    let stopping: Vec<PathBuf> = own_sockets(&dir, config)?
        .into_iter()
        .filter(|sock| ask(sock, Tag::Quit, &[Quit::with(force).byte()]).as_deref() == Some(STOPPING))
        .collect();
    // Gone once the socket is removed (it stops taking requests) and the
    // lock released (its sessions are closed and it is exiting).
    let t0 = Instant::now();
    for sock in &stopping {
        let lock = sock.with_extension("lock");
        while sock.exists() || is_locked(&lock) {
            if t0.elapsed() >= config.stop_wait {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the daemon of {} was still running after {} s", sock.display(), config.stop_wait.as_secs()),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(stopping.len())
}

/// Why a daemon is asked to quit: the body of [`Tag::Quit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quit {
    /// `stop`: retire while sessions are open.
    Stop,
    /// `stop --force`: exit now, sessions dropped.
    Force,
    /// A newer build of the executable started: retire, and leave no lock.
    Superseded,
}

impl Quit {
    fn with(force: bool) -> Quit {
        if force { Quit::Force } else { Quit::Stop }
    }

    fn byte(self) -> u8 {
        match self {
            Quit::Stop => 0,
            Quit::Force => 1,
            Quit::Superseded => 2,
        }
    }

    fn from_byte(b: Option<&u8>) -> Quit {
        match b {
            Some(1) => Quit::Force,
            Some(2) => Quit::Superseded,
            _ => Quit::Stop,
        }
    }
}

/// Take the lock on `file` without waiting; held until the file is closed.
fn try_lock(file: &File) -> bool {
    file.try_lock().is_ok()
}

/// Some process holds the lock at `path`.
fn is_locked(path: &Path) -> bool {
    File::open(path).is_ok_and(|f| !try_lock(&f))
}

/// Replies to a quit request.
const STOPPING: &str = "stopping";
const RETIRING: &str = "retiring once its sessions are closed";

/// Send one request that is answered with text.
fn ask(sock: &Path, tag: Tag, body: &[u8]) -> Option<String> {
    let mut stream = connect(sock)?;
    write_frame(&mut stream, tag, body).ok()?;
    match read_frame(&mut stream).ok()?? {
        (Tag::StatusReply, text) => Some(String::from_utf8_lossy(&text).into_owned()),
        _ => None,
    }
}

/// What a daemon is for: one build of one executable with one fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    /// The executable's path and everything that shapes its output.
    family: u64,
    /// Version, size, mtime and protocol of the build at that path.
    build: u64,
}

impl Identity {
    fn of(config: &Config) -> io::Result<Identity> {
        let exe = std::env::current_exe()?.canonicalize()?;
        let meta = fs::metadata(&exe)?;
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let mut build = Fnv::default();
        build.write(config.version.as_bytes());
        build.write(&meta.len().to_le_bytes());
        build.write(&mtime.to_le_bytes());
        build.write(&PROTOCOL.to_le_bytes());
        let mut family = Fnv::default();
        family.write(&sys::os_to_wire(exe.as_os_str()));
        family.write(&config.fingerprint);
        // Where the daemon writes spill files and traces, and with which
        // mode: the client's would be the same in its own process.
        for var in sys::PLACE_VARS {
            family.write(&sys::os_to_wire(&std::env::var_os(var).unwrap_or_default()));
        }
        family.write(&sys::umask().to_le_bytes());
        Ok(Identity { family: family.finish(), build: build.finish() })
    }

    fn name(&self) -> String {
        format!("{:016x}-{:016x}", self.family, self.build)
    }

    fn socket_name(&self) -> String {
        format!("{}.sock", self.name())
    }

    /// The identity a socket is named after.
    fn of_socket(sock: &Path) -> Option<Identity> {
        let (family, build) = sock.file_stem()?.to_str()?.split_once('-')?;
        Some(Identity { family: u64::from_str_radix(family, 16).ok()?, build: u64::from_str_radix(build, 16).ok()? })
    }
}

/// 64-bit FNV-1a: stable across builds, unlike `DefaultHasher`.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        // Field separator, so ("ab", "c") and ("a", "bc") differ.
        self.0 = (self.0 ^ 0xff).wrapping_mul(0x100_0000_01b3);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// `<runtime dir>/daemon`, created private: the configured directory, or a
/// short one when that is unset or would make socket paths too long.
///
/// # Errors
///
/// The directory (or its parent) belongs to someone else or others may enter
/// it, or no candidate leaves room for a socket path in `sockaddr_un`.
fn socket_dir(config: &Config) -> io::Result<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let preferred = config.runtime_dir.clone().or_else(|| var(&config.env_name("RUNTIME_DIR"))).or_else(|| sys::session_base(config.tool));
    let candidates: Vec<PathBuf> = preferred.into_iter().chain(sys::shared_bases(config.tool)).collect();
    let Some(base) = candidates.iter().find(|base| fits(&base.join("daemon"))) else {
        let tried: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("no socket directory short enough (tried {})", tried.join(", "))));
    };
    let dir = base.join("daemon");
    sys::create_private_dir(&dir)?;
    if sys::private(base) && sys::private(&dir) {
        Ok(dir)
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not private to this user", dir.display())))
    }
}

/// Room in `sockaddr_un` for the longest socket name in `dir`.
fn fits(dir: &Path) -> bool {
    dir.as_os_str().len() + "/0123456789abcdef-0123456789abcdef.sock".len() < SUN_PATH_MAX
}

/// A daemon's socket, if one answers there as this user.
fn connect(sock: &Path) -> Option<UnixStream> {
    let stream = UnixStream::connect(sock).ok()?;
    sys::same_user(&stream).then_some(stream)
}

fn sockets(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "sock")).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Start the daemon detached from this process and its terminal. It inherits
/// the environment; its stderr goes to `daemon.log` in the cache directory.
fn spawn(config: &Config) -> io::Result<std::process::Child> {
    let log = cache_dir(config.tool).join("daemon.log");
    let stderr = log
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| sys::open_private_append(&log))
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(&config.serve_args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
    sys::detach(&mut cmd);
    // Not reaped: it outlives this process (adopted by init on Unix).
    cmd.spawn()
}

/// Connect to the socket a just spawned daemon binds: `None` once it exits
/// without (another daemon holds the lock, the socket cannot be bound) or
/// after `wait`.
fn wait_for(sock: &Path, mut child: std::process::Child, wait: Duration) -> Option<UnixStream> {
    let t0 = Instant::now();
    loop {
        if let Some(stream) = connect(sock) {
            return Some(stream);
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return connect(sock);
        }
        if t0.elapsed() > wait {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Request fields: identity, cwd, flags (terminals, stdin present), stdin,
/// argc, argv, env pairs.
fn encode(id: &Identity, req: &Request) -> Vec<u8> {
    let mut buf = Vec::new();
    put(&mut buf, id.name().as_bytes());
    put(&mut buf, &sys::os_to_wire(req.cwd.as_os_str()));
    let flags = u8::from(req.stdout_is_terminal) | u8::from(req.stderr_is_terminal) << 1 | u8::from(req.stdin.is_some()) << 2;
    put(&mut buf, &[flags]);
    put(&mut buf, req.stdin.as_deref().unwrap_or_default());
    let argc = u32::try_from(req.args.len()).expect("fewer than 4G arguments");
    put(&mut buf, &argc.to_le_bytes());
    for a in &req.args {
        put(&mut buf, &sys::os_to_wire(a));
    }
    for (k, v) in &req.env {
        put(&mut buf, &sys::os_to_wire(k));
        put(&mut buf, &sys::os_to_wire(v));
    }
    buf
}

/// The request and the identity it was sent for.
fn decode(body: &[u8]) -> Option<(String, Request)> {
    let fields = fields(body)?;
    let [name, cwd, flags, stdin, argc, rest @ ..] = &fields[..] else { return None };
    let argc = usize::try_from(u32::from_le_bytes((*argc).try_into().ok()?)).ok()?;
    let (args, env) = (rest.get(..argc)?, rest.get(argc..)?);
    let os = |b: &[u8]| sys::os_from_wire(b);
    let flags = *flags.first()?;
    let request = Request {
        cwd: PathBuf::from(os(cwd)?),
        args: args.iter().map(|a| os(a)).collect::<Option<_>>()?,
        env: env.as_chunks::<2>().0.iter().map(|[k, v]| Some((os(k)?, os(v)?))).collect::<Option<_>>()?,
        stdout_is_terminal: flags & 1 != 0,
        stderr_is_terminal: flags & 2 != 0,
        stdin: (flags & 4 != 0).then(|| stdin.to_vec()),
    };
    Some((String::from_utf8(name.to_vec()).ok()?, request))
}

/// How a relayed command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Relayed {
    Exit(u8),
    /// The daemon will not run it; run it here.
    Declined,
    /// The daemon went away before any output.
    Vanished,
}

/// Send the request and copy the reply to `out` / `err`. A daemon lost
/// after some output is a lost session: reported on `err`, exit 3.
fn relay(mut stream: UnixStream, req: &[u8], out: &mut impl Write, err: &mut impl Write, tool: &str) -> Relayed {
    if write_frame(&mut stream, Tag::Run, req).is_err() {
        return Relayed::Vanished;
    }
    let mut out_open = true;
    let mut started = false;
    loop {
        match read_frame(&mut stream) {
            Ok(Some((Tag::Out, body))) => {
                started = true;
                // A closed stdout (`| head`) drops the rest, as a local run does.
                if out_open && out.write_all(&body).and_then(|()| out.flush()).is_err() {
                    out_open = false;
                }
            }
            Ok(Some((Tag::Err | Tag::Progress, body))) => {
                started = true;
                let _ = err.write_all(&body).and_then(|()| err.flush());
            }
            Ok(Some((Tag::Exit, body))) => return Relayed::Exit(body.first().copied().unwrap_or(Exit::Error.code())),
            Ok(Some((Tag::Decline, _))) if !started => return Relayed::Declined,
            Ok(_) | Err(_) if !started => return Relayed::Vanished,
            Ok(_) | Err(_) => {
                let lost = Failure::new(ErrorKind::SessionLost, format!("the {tool} daemon went away mid-command"))
                    .with_hint(format!("its sessions are gone; run '{tool} list' and open the target again"));
                let _ = write!(err, "{lost}");
                return Relayed::Exit(lost.exit().code());
            }
        }
    }
}

/// Ask the daemons of other builds of this executable to retire: the binary
/// they ran has been replaced. Their locks that nobody holds any more, and
/// sockets nobody serves, are removed.
fn retire_older_builds(dir: &Path, id: &Identity) {
    let older = |path: &Path| Identity::of_socket(path).is_some_and(|other| other.family == id.family && other.build != id.build);
    for sock in sockets(dir).iter().filter(|s| older(s)) {
        let _ = ask(sock, Tag::Quit, &[Quit::Superseded.byte()]);
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for lock in entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "lock") && older(p)) {
        if File::open(&lock).is_ok_and(|f| try_lock(&f)) {
            let _ = fs::remove_file(lock.with_extension("sock"));
            let _ = fs::remove_file(&lock);
        }
    }
}

/// The resident state of a serving daemon.
struct Daemon {
    id: Identity,
    sock: PathBuf,
    idle: Duration,
    started: Instant,
    last: Mutex<Instant>,
    /// Requests running now.
    active: AtomicUsize,
    /// Commands run, declined ones aside.
    served: AtomicUsize,
    /// Set once the daemon stops taking requests.
    quitting: AtomicBool,
    /// Told to exit while sessions are open: exits once they are closed.
    retiring: AtomicBool,
    /// Retiring because a newer build started.
    superseded: AtomicBool,
}

impl Daemon {
    fn handle(&self, mut stream: UnixStream, handler: &impl Handler) {
        let Ok(Some((tag, body))) = read_frame(&mut stream) else { return };
        match tag {
            Tag::Run => {
                self.active.fetch_add(1, Ordering::SeqCst);
                if self.run(stream, &body, handler) {
                    self.served.fetch_add(1, Ordering::Relaxed);
                }
                *self.last.lock().expect("idle clock mutex poisoned") = Instant::now();
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            Tag::Status => {
                let _ = write_frame(&mut stream, Tag::StatusReply, self.status(handler).as_bytes());
            }
            Tag::Quit => {
                let quit = Quit::from_byte(body.first());
                if quit == Quit::Superseded {
                    self.superseded.store(true, Ordering::SeqCst);
                }
                let reply = if quit == Quit::Force || !handler.holds_sessions() {
                    STOPPING
                } else {
                    self.retiring.store(true, Ordering::SeqCst);
                    RETIRING
                };
                let _ = write_frame(&mut stream, Tag::StatusReply, reply.as_bytes());
                drop(stream);
                if reply == STOPPING {
                    self.quit();
                }
            }
            Tag::Out | Tag::Err | Tag::Progress | Tag::Exit | Tag::Decline | Tag::StatusReply => {}
        }
    }

    /// Run a client's command; `false` if it was declined.
    fn run(&self, stream: UnixStream, body: &[u8], handler: &impl Handler) -> bool {
        let decline = |mut s: &UnixStream, why: &str| {
            let _ = write_frame(&mut s, Tag::Decline, why.as_bytes());
            false
        };
        let Some((name, request)) = decode(body) else { return decline(&stream, "malformed request") };
        if name != self.id.name() {
            return decline(&stream, "another build or configuration");
        }
        let cancel = Cancel::default();
        watch_disconnect(&stream, cancel.clone());
        let frames = RefCell::new(Frames { stream, out: Vec::new(), sent: false, failed: false });
        let (mut out, mut err, mut progress) =
            (FrameWriter(&frames, Tag::Out), FrameWriter(&frames, Tag::Err), FrameWriter(&frames, Tag::Progress));
        let outcome = {
            let mut io = Io { out: &mut out, err: &mut err, progress: Some(&mut progress), cancel };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.run(&request, &mut io)))
        };
        let mut frames = frames.borrow_mut();
        let code = match outcome {
            Ok(Outcome::Exit(exit)) => exit.code(),
            Ok(Outcome::Decline(why)) if !frames.sent => return decline(&frames.stream, &why),
            Ok(Outcome::Decline(why)) => {
                let failure = Failure::new(ErrorKind::Internal, format!("declined after writing output: {why}"))
                    .with_hint("rerun the command; if it repeats, `daemon stop` restarts the daemon");
                let _ = frames.send(Tag::Err, failure.to_string().as_bytes());
                failure.exit().code()
            }
            Err(panic) => {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let failure = Failure::new(ErrorKind::Internal, format!("the daemon's handler panicked: {what}"))
                    .with_hint("a bug: rerun the command, and `daemon stop --force` restarts the daemon if it repeats");
                let _ = frames.flush_out().and_then(|()| frames.send(Tag::Err, failure.to_string().as_bytes()));
                failure.exit().code()
            }
        };
        if frames.flush_out().is_ok() {
            let _ = frames.send(Tag::Exit, &[code]);
        }
        let _ = frames.stream.shutdown(std::net::Shutdown::Both);
        true
    }

    fn status(&self, handler: &impl Handler) -> String {
        let retiring = if self.retiring.load(Ordering::SeqCst) { ", retiring" } else { "" };
        let active = self.active.load(Ordering::SeqCst);
        // The idle clock runs only while no request does.
        let idle_left =
            if active > 0 { self.idle } else { self.idle.saturating_sub(self.last.lock().expect("idle clock mutex poisoned").elapsed()) };
        let mut text = format!(
            "pid {}, up {} s, {} requests ({active} running){retiring}, exits after {} s idle, {}",
            std::process::id(),
            self.started.elapsed().as_secs(),
            self.served.load(Ordering::Relaxed),
            idle_left.as_secs(),
            self.id.name(),
        );
        let extra = handler.status(idle_left);
        if !extra.is_empty() {
            text.push('\n');
            text.push_str(extra.trim_end());
        }
        text
    }

    /// Exit when idle, or when retiring and the last session has closed.
    fn watch_idle(&self, config: &Config, handler: &impl Handler) {
        loop {
            std::thread::sleep(IDLE_POLL.min(config.idle));
            if self.quitting.load(Ordering::SeqCst) {
                return;
            }
            let idle = self.last.lock().expect("idle clock mutex poisoned").elapsed();
            let retired = self.retiring.load(Ordering::SeqCst) && !handler.holds_sessions();
            if self.active.load(Ordering::SeqCst) == 0 && (idle >= config.idle || retired) {
                self.quit();
                return;
            }
        }
    }

    /// Stop taking requests: wake the accept loop, then remove the socket.
    /// Requests already running finish first.
    fn quit(&self) {
        if !self.quitting.swap(true, Ordering::SeqCst) {
            drop(UnixStream::connect(&self.sock));
            let _ = fs::remove_file(&self.sock);
        }
    }
}

/// Cancel once the client hangs up (it sends nothing after the request).
fn watch_disconnect(stream: &UnixStream, cancel: Cancel) {
    let Ok(mut reader) = stream.try_clone() else { return };
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while matches!(reader.read(&mut byte), Ok(n) if n > 0) {}
        cancel.cancel();
    });
}

/// The reply stream of one request: buffered stdout, unbuffered stderr and
/// progress, in the order the command wrote them.
struct Frames {
    stream: UnixStream,
    out: Vec<u8>,
    /// Some output has gone to the client.
    sent: bool,
    failed: bool,
}

impl Frames {
    fn send(&mut self, tag: Tag, body: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.sent = true;
        write_frame(&mut self.stream, tag, body).inspect_err(|_| self.failed = true)
    }

    fn flush_out(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let out = std::mem::take(&mut self.out);
        self.send(Tag::Out, &out)
    }
}

/// One of the command's streams: `Tag::Out` is buffered, the others are
/// sent at once, after the stdout written before them.
struct FrameWriter<'f>(&'f RefCell<Frames>, Tag);

impl Write for FrameWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut f = self.0.borrow_mut();
        if f.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.1 == Tag::Out {
            f.out.extend_from_slice(buf);
            if f.out.len() >= FRAME_BYTES {
                f.flush_out()?;
            }
        } else {
            f.flush_out()?;
            f.send(self.1, buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn requests_round_trip() {
        let id = Identity { family: 1, build: 2 };
        let req = Request {
            cwd: PathBuf::from("/a b"),
            args: vec!["idax".into(), "exec".into(), "".into()],
            env: vec![("CLAUDECODE".into(), "1".into()), ("IDAX_LIMIT".into(), "".into())],
            stdout_is_terminal: false,
            stderr_is_terminal: true,
            stdin: None,
        };
        assert_eq!(decode(&encode(&id, &req)), Some((id.name(), req.clone())));
        for stdin in [Vec::new(), b"print(1)\n\0\xff".to_vec()] {
            let req = Request { stdin: Some(stdin), ..req.clone() };
            assert_eq!(decode(&encode(&id, &req)), Some((id.name(), req)), "an empty stdin is not a missing one");
        }
    }

    #[test]
    fn a_runtime_directory_too_long_for_a_socket_gives_way_to_a_short_one() {
        let mut config = Config::new("axlong", "0");
        config.runtime_dir = Some(PathBuf::from(format!("/tmp/{}", "x".repeat(SUN_PATH_MAX))));
        let dir = socket_dir(&config).unwrap();
        assert!(fits(&dir), "{}", dir.display());
        assert!(dir.ends_with(Path::new(&format!("axlong-{}", sys::user_tag())).join("daemon")), "{}", dir.display());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_socket_name_gives_back_its_identity() {
        let id = Identity { family: 0xabc, build: u64::MAX };
        assert_eq!(Identity::of_socket(Path::new(&id.socket_name())), Some(id));
        assert_eq!(Identity::of_socket(Path::new("junk.sock")), None);
    }

    #[test]
    fn hanging_up_cancels_the_request() {
        let (daemon_end, client_end) = UnixStream::pair().unwrap();
        let cancel = Cancel::default();
        watch_disconnect(&daemon_end, cancel.clone());
        assert!(!cancel.is_cancelled());
        drop(client_end);
        let t0 = Instant::now();
        while !cancel.is_cancelled() {
            assert!(t0.elapsed() < Duration::from_secs(5), "cancel not set after the client hung up");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn stderr_and_progress_follow_the_stdout_written_before_them() {
        let (daemon_end, mut client_end) = UnixStream::pair().unwrap();
        let frames = RefCell::new(Frames { stream: daemon_end, out: Vec::new(), sent: false, failed: false });
        let (mut out, mut err, mut progress) =
            (FrameWriter(&frames, Tag::Out), FrameWriter(&frames, Tag::Err), FrameWriter(&frames, Tag::Progress));
        out.write_all(b"row\n").unwrap();
        err.write_all(b"warn\n").unwrap();
        out.write_all(b"tail\n").unwrap();
        progress.write_all(b"# progress: 50%\n").unwrap();
        out.flush().unwrap();
        drop(frames);
        let got: Vec<_> = std::iter::from_fn(|| read_frame(&mut client_end).unwrap()).collect();
        let want = [(Tag::Out, "row\n"), (Tag::Err, "warn\n"), (Tag::Out, "tail\n"), (Tag::Progress, "# progress: 50%\n")];
        assert_eq!(got, want.map(|(t, s)| (t, s.as_bytes().to_vec())));
    }

    /// Echoes its arguments, reports progress, and can pretend to hold a
    /// session.
    #[derive(Default)]
    struct Echo {
        sessions: AtomicUsize,
        shut_down: AtomicBool,
    }

    impl Handler for Echo {
        fn run(&self, request: &Request, io: &mut Io<'_>) -> Outcome {
            let args: Vec<String> = request.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            match args.get(1).map(String::as_str) {
                Some("help") => Outcome::Decline("run locally".into()),
                Some("open") => {
                    self.sessions.fetch_add(1, Ordering::SeqCst);
                    Outcome::Exit(Exit::Success)
                }
                Some("close") => {
                    self.sessions.fetch_sub(1, Ordering::SeqCst);
                    Outcome::Exit(Exit::Success)
                }
                Some("panic") => panic!("boom"),
                Some("cat") => {
                    let _ = io.out.write_all(request.stdin.as_deref().unwrap_or(b"<no stdin>"));
                    Outcome::Exit(Exit::Success)
                }
                _ => {
                    let _ = writeln!(io.out, "{}", args.join(" "));
                    io.progress("# progress: done");
                    let _ = writeln!(io.err, "mode {:?}", request.mode());
                    Outcome::Exit(Exit::Error)
                }
            }
        }

        fn status(&self, idle_left: Duration) -> String {
            format!("{} sessions, {} s left", self.sessions.load(Ordering::SeqCst), idle_left.as_secs())
        }

        fn holds_sessions(&self) -> bool {
            self.sessions.load(Ordering::SeqCst) > 0
        }

        fn shutdown(&self) {
            self.shut_down.store(true, Ordering::SeqCst);
        }
    }

    struct Harness {
        config: Config,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Harness {
            // Short: the socket path must fit `sockaddr_un`.
            #[cfg(unix)]
            let dir = {
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::Builder::new().prefix("ax").tempdir_in("/tmp").unwrap();
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
                dir
            };
            #[cfg(windows)]
            let dir = tempfile::Builder::new().prefix("ax").tempdir().unwrap();
            let mut config = Config::new("axtest", "0");
            config.runtime_dir = Some(dir.path().to_path_buf());
            config.idle = Duration::from_secs(60);
            Harness { config, _dir: dir }
        }

        fn sock(&self) -> PathBuf {
            let id = Identity::of(&self.config).unwrap();
            socket_dir(&self.config).unwrap().join(id.socket_name())
        }

        fn wait_up(&self) {
            let t0 = Instant::now();
            while connect(&self.sock()).is_none() {
                assert!(t0.elapsed() < Duration::from_secs(5), "daemon did not come up");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Run `args` in the daemon: how it ended, stdout, stderr.
        fn run(&self, args: &[&str], env: &[(&str, &str)]) -> (Relayed, String, String) {
            self.run_with(args, env, None)
        }

        fn run_with(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> (Relayed, String, String) {
            let id = Identity::of(&self.config).unwrap();
            let req = Request {
                cwd: PathBuf::from("/"),
                args: args.iter().map(OsString::from).collect(),
                env: env.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect(),
                stdout_is_terminal: false,
                stderr_is_terminal: false,
                stdin: stdin.map(<[u8]>::to_vec),
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let relayed = relay(connect(&self.sock()).unwrap(), &encode(&id, &req), &mut out, &mut err, "axtest");
            (relayed, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
        }
    }

    #[test]
    fn a_daemon_runs_commands_and_stops() {
        let h = Harness::new();
        let echo = Echo::default();
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &echo).unwrap());
            h.wait_up();

            let (code, out, err) = h.run(&["axtest", "hello", "world"], &[("CLAUDECODE", "1")]);
            assert_eq!(code, Relayed::Exit(Exit::Error.code()));
            assert_eq!(out, "axtest hello world\n");
            assert_eq!(err, "# progress: done\nmode Agent\n", "the client's environment, not the daemon's");

            assert_eq!(h.run(&["axtest", "help"], &[]).0, Relayed::Declined, "declined: the client runs it");

            let (code, _, err) = h.run(&["axtest", "panic"], &[]);
            assert_eq!(code, Relayed::Exit(Exit::Error.code()));
            assert!(err.starts_with("Error: internal: the daemon's handler panicked: boom"), "{err}");

            assert_eq!(h.run_with(&["axtest", "cat"], &[], Some(b"from the client")).1, "from the client");
            assert_eq!(h.run(&["axtest", "cat"], &[]).1, "<no stdin>");

            let status = status(&h.config).unwrap();
            assert_eq!(status.len(), 1);
            assert!(status[0].contains("4 requests (0 running)") && status[0].contains("exits after 59 s idle"), "{status:?}");
            assert!(status[0].ends_with("0 sessions, 59 s left"), "the handler hears the idle time left: {status:?}");

            assert_eq!(stop(&h.config, false).unwrap(), 1);
        });
        assert!(echo.shut_down.load(Ordering::SeqCst));
        assert!(!h.sock().exists());
    }

    #[test]
    fn a_daemon_with_sessions_only_retires_unless_forced() {
        let h = Harness::new();
        let echo = Echo::default();
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &echo).unwrap());
            h.wait_up();
            assert_eq!(h.run(&["axtest", "open"], &[]).0, Relayed::Exit(0));

            assert_eq!(stop(&h.config, false).unwrap(), 0, "a session is open");
            assert!(status(&h.config).unwrap()[0].contains("retiring"));

            assert_eq!(h.run(&["axtest", "close"], &[]).0, Relayed::Exit(0));
            let t0 = Instant::now();
            while h.sock().exists() {
                assert!(t0.elapsed() < Duration::from_secs(5), "a retiring daemon exits once its sessions are gone");
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let echo = Echo::default();
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &echo).unwrap());
            h.wait_up();
            assert_eq!(h.run(&["axtest", "open"], &[]).0, Relayed::Exit(0));
            assert_eq!(stop(&h.config, true).unwrap(), 1, "forced");
            assert!(!is_locked(&h.sock().with_extension("lock")), "stop returns once the daemon has let go of its lock");
        });
    }

    /// Shuts down slowly, as a tool closing its workers does.
    struct Slow;

    impl Handler for Slow {
        fn run(&self, _: &Request, _: &mut Io<'_>) -> Outcome {
            Outcome::Exit(Exit::Success)
        }

        fn shutdown(&self) {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    #[test]
    fn status_and_stop_leave_other_families_in_a_shared_directory_alone() {
        let h = Harness::new();
        let mut other = h.config.clone();
        other.fingerprint = b"another cache".to_vec();
        let (mine, theirs) = (Echo::default(), Echo::default());
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &mine).unwrap());
            s.spawn(|| serve(&other, &theirs).unwrap());
            h.wait_up();
            let their_sock = socket_dir(&other).unwrap().join(Identity::of(&other).unwrap().socket_name());
            while connect(&their_sock).is_none() {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(status(&h.config).unwrap().len(), 1, "only this family's daemon reports");
            assert_eq!(stop(&h.config, true).unwrap(), 1);
            assert!(connect(&their_sock).is_some(), "the other family's daemon is still serving");
            assert_eq!(stop(&other, true).unwrap(), 1);
        });
    }

    #[test]
    fn stop_waits_until_the_daemon_has_exited() {
        let h = Harness::new();
        std::thread::scope(|s| {
            let served = s.spawn(|| serve(&h.config, &Slow).unwrap());
            h.wait_up();
            assert_eq!(stop(&h.config, true).unwrap(), 1);
            assert!(served.is_finished(), "the daemon was still closing its sessions when stop returned");
        });
    }

    #[test]
    fn a_superseded_daemon_leaves_no_lock_behind() {
        let h = Harness::new();
        let dir = socket_dir(&h.config).unwrap();
        let id = Identity::of(&h.config).unwrap();
        // A dead daemon of an older build: an unheld lock and a stale socket.
        let old = Identity { build: id.build ^ 1, ..id };
        fs::write(dir.join(format!("{}.lock", old.name())), "").unwrap();
        fs::write(dir.join(old.socket_name()), "").unwrap();
        // Another tool's lock stays.
        let other = Identity { family: id.family ^ 1, ..id };
        fs::write(dir.join(format!("{}.lock", other.name())), "").unwrap();
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &Echo::default()).unwrap());
            h.wait_up();
            stop(&h.config, true).unwrap();
        });
        let mut left: Vec<String> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mut want = [format!("{}.lock", id.name()), format!("{}.lock", other.name())];
        want.sort();
        assert_eq!(left, want);
    }

    #[test]
    fn a_second_daemon_of_the_same_identity_steps_aside() {
        let h = Harness::new();
        let echo = Echo::default();
        std::thread::scope(|s| {
            s.spawn(|| serve(&h.config, &echo).unwrap());
            h.wait_up();
            serve(&h.config, &Echo::default()).unwrap();
            assert!(connect(&h.sock()).is_some(), "the first one still serves");
            stop(&h.config, true).unwrap();
        });
    }

    #[test]
    fn a_daemon_idles_out() {
        let mut h = Harness::new();
        h.config.idle = Duration::from_millis(50);
        let echo = Echo::default();
        let t0 = Instant::now();
        serve(&h.config, &echo).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(echo.shut_down.load(Ordering::SeqCst));
    }

    #[test]
    fn a_daemon_lost_mid_command_is_a_lost_session() {
        let (mut daemon_end, client_end) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let _ = read_frame(&mut daemon_end);
            write_frame(&mut daemon_end, Tag::Out, b"partial\n").unwrap();
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = relay(client_end, b"", &mut out, &mut err, "idax");
        assert_eq!(code, Relayed::Exit(Exit::SessionLost.code()));
        assert_eq!(out, b"partial\n");
        assert!(String::from_utf8(err).unwrap().starts_with("Error: session_lost: the idax daemon went away mid-command"));
    }
}
