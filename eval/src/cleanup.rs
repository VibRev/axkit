//! Removing what one eval run left in a harness's own history, and nothing
//! else.
//!
//! Every deletion is keyed on something unique to the run: its workspace
//! path (which embeds a random run id) or the session id the stream reported.
//! Files other sessions append to are rewritten through a temp file and a
//! rename, and only when a matching line exists. A failure is a warning; it
//! never changes a run's result.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What a cleanup step did, for the run's log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub removed: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    fn removed(&mut self, what: impl Into<String>) {
        self.removed.push(what.into());
    }

    fn warn(&mut self, what: impl Into<String>) {
        self.warnings.push(what.into());
    }

    pub fn merge(&mut self, other: Report) {
        self.removed.extend(other.removed);
        self.warnings.extend(other.warnings);
    }
}

/// The user's home, where the harness stores live.
#[must_use]
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/nonexistent"), PathBuf::from)
}

/// Remove `dir` when its name contains `run_id`; refuse otherwise.
pub fn remove_run_dir(dir: &Path, run_id: &str, report: &mut Report) {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(run_id) {
        report.warn(format!("refused to remove {}: its name lacks the run id {run_id}", dir.display()));
        return;
    }
    if !dir.exists() {
        return;
    }
    match fs::remove_dir_all(dir) {
        Ok(()) => report.removed(dir.display().to_string()),
        Err(e) => report.warn(format!("removing {}: {e}", dir.display())),
    }
}

/// Drop the lines of an append-only file for which `matches` holds, through
/// a temp file and a rename. Does nothing when no line matches. If the file
/// grew while it was being filtered (another session appended), it is read
/// again, so a concurrent append is not lost.
pub fn drop_lines(path: &Path, matches: impl Fn(&str) -> bool, report: &mut Report) {
    for _ in 0..5 {
        let Ok(text) = fs::read_to_string(path) else { return };
        let kept: Vec<&str> = text.split_inclusive('\n').filter(|l| !matches(l)).collect();
        let dropped = text.split_inclusive('\n').count() - kept.len();
        if dropped == 0 {
            return;
        }
        let tmp = path.with_extension(format!("axeval-{}.tmp", std::process::id()));
        let write = fs::File::create(&tmp).and_then(|mut f| {
            for line in &kept {
                f.write_all(line.as_bytes())?;
            }
            f.sync_all()
        });
        if let Err(e) = write {
            report.warn(format!("rewriting {}: {e}", path.display()));
            let _ = fs::remove_file(&tmp);
            return;
        }
        // Rename only when nothing was appended meanwhile.
        if fs::metadata(path).map(|m| m.len()).ok() == Some(text.len() as u64) {
            match fs::rename(&tmp, path) {
                Ok(()) => report.removed(format!("{dropped} line(s) of {}", path.display())),
                Err(e) => report.warn(format!("replacing {}: {e}", path.display())),
            }
            return;
        }
        let _ = fs::remove_file(&tmp);
    }
    report.warn(format!("{} kept changing; its eval lines were left in place", path.display()));
}

/// An exclusive advisory lock (`flock`) on a file, held until dropped. Shared
/// state a harness rewrites without locking of its own (grok's
/// `trusted_folders.toml`) is only touched by eval sessions while they hold
/// it, so concurrent runs — threads or other `axkit-eval` processes — take
/// turns instead of overwriting each other's entries.
#[derive(Debug)]
pub struct FileLock {
    /// Closing the file releases the lock.
    _file: fs::File,
}

impl FileLock {
    /// Block until `path` (created when missing) is locked.
    ///
    /// # Errors
    ///
    /// The lock file cannot be opened or locked.
    pub fn acquire(path: &Path) -> std::io::Result<FileLock> {
        let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        file.lock()?;
        Ok(FileLock { _file: file })
    }
}

/// The lock file for one piece of a harness's shared state: in the system
/// temp dir, per user, never inside the harness's own config directory.
#[must_use]
pub fn lock_path(what: &str) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
    std::env::temp_dir().join(format!("axeval-{user}-{what}.lock"))
}

/// The lock every eval session holds while it lets grok write, or itself
/// rewrites, `trusted_folders.toml`.
#[must_use]
pub fn grok_trust_lock() -> PathBuf {
    lock_path("grok-trust")
}

/// Does a grok `trusted_folders.toml` hold the table of `path`?
#[must_use]
pub fn trusts_folder(text: &str, path: &str) -> bool {
    text.lines().any(|l| l.trim().strip_prefix("[folders.\"").and_then(|r| r.strip_suffix("\"]")) == Some(path))
}

/// Remove the `[folders."<path>"]` tables of a grok `trusted_folders.toml`
/// whose path satisfies `matches`; every other byte is kept as it is. The
/// caller holds the file's [`FileLock`].
pub fn drop_trusted_folders(path: &Path, matches: impl Fn(&str) -> bool, report: &mut Report) {
    let Ok(text) = fs::read_to_string(path) else { return };
    let (kept, dropped) = filter_tables(&text, &matches);
    if dropped == 0 {
        return;
    }
    let tmp = path.with_extension(format!("axeval-{}.tmp", std::process::id()));
    let result = fs::write(&tmp, &kept).and_then(|()| fs::rename(&tmp, path));
    match result {
        Ok(()) => report.removed(format!("{dropped} trusted folder(s) in {}", path.display())),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            report.warn(format!("rewriting {}: {e}", path.display()));
        }
    }
}

/// `text` without the `[folders."…"]` tables whose path matches, and how many
/// were dropped. A table runs from its header to the next header.
fn filter_tables(text: &str, matches: &impl Fn(&str) -> bool) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    let mut dropped = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = trimmed.strip_prefix("[folders.\"").and_then(|r| r.strip_suffix("\"]")).is_some_and(matches);
            dropped += usize::from(skipping);
        }
        if !skipping {
            out.push_str(line);
        }
    }
    (out, dropped)
}

/// Files and directories under `root` (recursively) whose name contains
/// `needle`.
#[must_use]
pub fn find_named(root: &Path, needle: &str, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().contains(needle) {
                found.push(path.clone());
            }
            if depth < max_depth && entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((path, depth + 1));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_dir_is_removed_only_when_named_for_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("-ws-g-task-0-r1a2b3c");
        let other = dir.path().join("-home-user-project");
        fs::create_dir_all(&mine).unwrap();
        fs::create_dir_all(&other).unwrap();
        let mut r = Report::default();
        remove_run_dir(&mine, "r1a2b3c", &mut r);
        remove_run_dir(&other, "r1a2b3c", &mut r);
        assert!(!mine.exists() && other.exists());
        assert_eq!(r.removed.len(), 1);
        assert!(r.warnings[0].contains("refused"));
    }

    #[test]
    fn only_matching_lines_are_dropped_and_the_rest_is_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        fs::write(&path, "{\"s\":\"a\"}\n{\"s\":\"run-x\"}\n{\"s\":\"b\"}\n").unwrap();
        let mut r = Report::default();
        drop_lines(&path, |l| l.contains("run-x"), &mut r);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"s\":\"a\"}\n{\"s\":\"b\"}\n");
        let before = fs::read_to_string(&path).unwrap();
        drop_lines(&path, |l| l.contains("nothing"), &mut r);
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "no match, no rewrite");
        assert_eq!(r.removed.len(), 1);
    }

    #[test]
    fn trusted_folder_tables_are_removed_by_path() {
        let text =
            "[folders.\"/home/u/proj\"]\ntrusted = true\n\n[folders.\"/x/eval-data/ws/g.t.0.r9\"]\ntrusted = true\n\n[other]\nk = 1\n";
        let (kept, dropped) = filter_tables(text, &|p: &str| p.contains("r9"));
        assert_eq!(dropped, 1);
        assert_eq!(kept, "[folders.\"/home/u/proj\"]\ntrusted = true\n\n[other]\nk = 1\n");
    }

    #[test]
    fn a_folder_is_trusted_only_by_its_own_table() {
        let text = "[folders.\"/w/a.r1\"]\ntrusted = true\n[folders.\"/w/a.r10\"]\n";
        assert!(trusts_folder(text, "/w/a.r1"));
        assert!(!trusts_folder(text, "/w/a.r"));
        assert!(!trusts_folder("", "/w/a.r1"));
    }

    #[test]
    fn a_file_lock_excludes_other_holders_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = FileLock::acquire(&path).unwrap();
        // flock belongs to the open file description, so a second open in
        // the same process contends like another process would.
        let other = fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(matches!(other.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(held);
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn find_named_walks_to_the_depth_given() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("2026/10/24")).unwrap();
        fs::write(dir.path().join("2026/10/24/rollout-x-abc123.jsonl"), "").unwrap();
        assert_eq!(find_named(dir.path(), "abc123", 3).len(), 1);
        assert!(find_named(dir.path(), "abc123", 1).is_empty());
    }
}
