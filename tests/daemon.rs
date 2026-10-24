//! The daemon as a tool runs it: a real executable that starts its own
//! daemon on first use (`examples/echo_tool.rs`).
#![cfg(unix)]
// `allow-unwrap-in-tests` covers `#[test]` functions, not the helpers below.
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Env {
    exe: PathBuf,
    runtime: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        // `target/<profile>/deps/daemon-<hash>` -> `target/<profile>/examples/echo_tool`
        let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let exe = deps.parent().unwrap().join("examples").join("echo_tool");
        assert!(exe.is_file(), "{} not built; `cargo test` builds examples", exe.display());
        // Short and private: the socket path must fit `sockaddr_un`.
        let runtime = tempfile::Builder::new().prefix("axe").tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Env { exe, runtime, cache: tempfile::tempdir().unwrap() }
    }

    fn run(&self, daemon: bool, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args)
            .env("AXECHO_RUNTIME_DIR", self.runtime.path())
            .env("XDG_CACHE_HOME", self.cache.path())
            .env("AXECHO_DAEMON", if daemon { "on" } else { "off" });
        for var in ["CLAUDECODE", "CODEX_CI", "CODEX_SANDBOX", "CODEX_THREAD_ID", "AI_AGENT", "AGENT", "AXKIT_AGENT"] {
            cmd.env_remove(var);
        }
        cmd.envs(env.iter().copied());
        cmd.output().unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(true, &["daemon", "stop"], &[]);
    }
}

#[test]
fn daemon_output_is_byte_identical_to_a_local_run() {
    let env = Env::new();
    let agent = [("CLAUDECODE", "1")];
    let remote = env.run(true, &["hello", "wörld", ""], &agent);
    let local = env.run(false, &["hello", "wörld", ""], &agent);
    assert_eq!(String::from_utf8_lossy(&remote.stdout), "hello wörld \n");
    assert_eq!(String::from_utf8_lossy(&remote.stderr), "# progress: echoed\nmode Agent\n");
    assert_eq!((remote.status.code(), &remote.stdout, &remote.stderr), (local.status.code(), &local.stdout, &local.stderr));
    assert_eq!(remote.status.code(), Some(1));

    let status = String::from_utf8(env.run(true, &["daemon", "status"], &[]).stdout).unwrap();
    assert!(status.contains("1 requests"), "the first command ran in the daemon it started: {status}");
}

#[test]
fn each_request_carries_its_own_environment() {
    let env = Env::new();
    let first = env.run(true, &["x"], &[("CLAUDECODE", "1")]);
    let second = env.run(true, &["x"], &[]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("mode Agent"));
    assert!(String::from_utf8_lossy(&second.stderr).contains("mode Human"), "not the environment the daemon was started with");
    let status = String::from_utf8(env.run(true, &["daemon", "status"], &[]).stdout).unwrap();
    assert_eq!(status.lines().filter(|l| l.starts_with("pid ")).count(), 1, "one daemon served both: {status}");
}

#[test]
fn a_daemon_that_cannot_be_reached_fails_with_the_reason() {
    let env = Env::new();
    // Others may enter it: no socket goes there.
    std::fs::set_permissions(env.runtime.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = env.run(true, &["x"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("Error: target_unavailable: no axecho daemon: ") && stderr.contains("not private"), "{stderr}");
    assert!(stderr.contains("AXECHO_DAEMON=off"), "{stderr}");
    assert!(out.stdout.is_empty(), "the command did not run elsewhere");
    std::fs::set_permissions(env.runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
}
