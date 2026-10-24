//! What the daemon's socket transport needs from the platform: Unix, or
//! Windows 10 1803+, whose AF_UNIX sockets keep the same protocol and the
//! same socket-per-identity layout.
//!
//! The socket directory is private on both: mode 0700 and the owner's uid
//! on Unix, where both ends also check the peer's uid. On Windows it lives
//! under `%LOCALAPPDATA%`, whose ACL admits only the user (and SYSTEM and
//! administrators), and AF_UNIX there reports no peer credentials: the
//! directory's ACL is the check.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
pub(super) use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub(super) use uds_windows::{UnixListener, UnixStream};

/// The smallest socket path room of the supported systems: 104 on macOS,
/// 108 on Linux and Windows.
#[cfg(unix)]
pub(super) const SUN_PATH_MAX: usize = 104;
#[cfg(windows)]
pub(super) const SUN_PATH_MAX: usize = 108;

/// Environment variables that decide where the daemon writes spill files
/// and state: a client with others gets a daemon of its own.
#[cfg(unix)]
pub(super) const PLACE_VARS: &[&str] = &["HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"];
#[cfg(windows)]
pub(super) const PLACE_VARS: &[&str] = &["USERPROFILE", "LOCALAPPDATA"];

/// A path or argument as bytes for the wire, read back by [`os_from_wire`]
/// in a daemon of the same build (the request names its build).
#[cfg(unix)]
pub(super) fn os_to_wire(s: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(unix)]
pub(super) fn os_from_wire(b: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(b.to_vec()))
}

/// UTF-16 code units, little-endian: any Windows string, unpaired
/// surrogates included, round-trips without unsafe code.
#[cfg(windows)]
pub(super) fn os_to_wire(s: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(windows)]
pub(super) fn os_from_wire(b: &[u8]) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    let (units, rest) = b.as_chunks::<2>();
    rest.is_empty().then(|| OsString::from_wide(&units.iter().map(|u| u16::from_le_bytes(*u)).collect::<Vec<_>>()))
}

/// Who this user is, for naming a shared fallback directory.
#[cfg(unix)]
pub(super) fn user_tag() -> String {
    uid().to_string()
}

#[cfg(windows)]
pub(super) fn user_tag() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".to_owned())
}

/// The process's umask, part of the identity: it shapes the modes of the
/// files the daemon writes. Linux reports it; elsewhere it is read by
/// setting and restoring it, which races with threads creating files.
#[cfg(unix)]
pub(super) fn umask() -> u32 {
    let reported = fs::read_to_string("/proc/self/status").ok().and_then(|s| {
        let line = s.lines().find_map(|l| l.strip_prefix("Umask:"))?;
        u32::from_str_radix(line.trim(), 8).ok()
    });
    reported.unwrap_or_else(|| {
        #[allow(clippy::useless_conversion)] // `mode_t` is u32 on Linux but u16 on macOS
        // SAFETY: umask only swaps the process's file mode mask, restored at once.
        unsafe {
            let m = libc::umask(0o022);
            libc::umask(m);
            u32::from(m)
        }
    })
}

#[cfg(windows)]
pub(super) fn umask() -> u32 {
    0
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// The process at the other end of `stream` runs as this user.
#[cfg(unix)]
pub(super) fn same_user(stream: &UnixStream) -> bool {
    peer_uid(stream) == Some(uid())
}

/// No peer credentials on Windows' AF_UNIX: the socket directory's ACL
/// decides who connects (module docs).
#[cfg(windows)]
pub(super) fn same_user(_stream: &UnixStream) -> bool {
    true
}

#[cfg(unix)]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).ok()?;
        // SAFETY: `cred` and `len` are valid for writes of the sizes passed.
        let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len) };
        (rc == 0).then_some(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: both out-pointers are valid for writes.
        let rc = unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) };
        (rc == 0).then_some(uid)
    }
}

/// The session's own runtime directory for `tool`, when it has one.
pub(super) fn session_base(tool: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    let var = "XDG_RUNTIME_DIR";
    #[cfg(windows)]
    let var = "LOCALAPPDATA";
    std::env::var_os(var).filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join(tool))
}

/// Shared temporary directories named after the user: where the socket
/// directory goes when the preferred one would make socket paths too long.
pub(super) fn shared_bases(tool: &str) -> Vec<PathBuf> {
    let own = format!("{tool}-{}", user_tag());
    let bases = [std::env::temp_dir()];
    #[cfg(unix)]
    let bases = [bases[0].clone(), PathBuf::from("/tmp")];
    bases.iter().map(|b| b.join(&own)).collect()
}

/// Create `dir` and its missing parents, closed to other users.
pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Owned by this user, closed to everyone else, and not a symlink.
#[cfg(unix)]
pub(super) fn private(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && m.uid() == uid() && m.mode() & 0o077 == 0)
}

/// A directory and not a link; who may enter is the inherited ACL's call.
#[cfg(windows)]
pub(super) fn private(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

/// Close a file the daemon created (its socket) to other users.
pub(super) fn restrict(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

/// Open `path` for appending, created closed to other users.
pub(super) fn open_private_append(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::File::options();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Detach `cmd` from this process's session and terminal, so it outlives
/// both, and start it in a directory that no unmount waits on.
pub(super) fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.current_dir("/");
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console, so closing the client's window does not end it.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        /// Out of the client's group: its Ctrl-C does not reach the daemon.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.current_dir(std::env::temp_dir()).creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn peer_uid_is_ours_on_a_socket_pair() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a), Some(uid()));
    }

    #[cfg(unix)]
    #[test]
    fn only_private_directories_hold_the_socket() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("d");
        fs::DirBuilder::new().mode(0o700).create(&d).unwrap();
        assert!(private(&d));
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!private(&d), "others may enter it");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(!private(&link), "a symlink is not followed");
    }

    #[test]
    fn strings_round_trip_over_the_wire() {
        for s in ["", "plain", "ünïcödé/路径", "a b\nc"] {
            assert_eq!(os_from_wire(&os_to_wire(OsStr::new(s))).as_deref(), Some(OsStr::new(s)));
        }
    }
}
