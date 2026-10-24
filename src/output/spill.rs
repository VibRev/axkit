//! Where output that did not fit the window goes: one private directory,
//! files named by content, kept under a byte budget.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Default disk budget of a spill directory.
pub const DEFAULT_BUDGET: u64 = 64 << 20;

/// A directory of spilled outputs, least recently used out first.
#[derive(Debug)]
pub struct SpillDir {
    dir: PathBuf,
    budget: u64,
    /// Serializes write + prune, so a prune never removes a file another
    /// caller has just written and is about to name.
    lock: Mutex<()>,
}

impl SpillDir {
    pub fn new(dir: PathBuf, budget: u64) -> SpillDir {
        SpillDir { dir, budget, lock: Mutex::new(()) }
    }

    /// `out/` in the tool's cache directory (CONVENTIONS §4.5), with the
    /// default budget.
    pub fn for_tool(tool: &str) -> SpillDir {
        SpillDir::new(cache_dir(tool).join("out"), DEFAULT_BUDGET)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write `text`, or find it already written, and return its path.
    ///
    /// # Errors
    ///
    /// The directory cannot be created or the file cannot be written.
    ///
    /// # Panics
    ///
    /// Another writer panicked while holding the directory lock.
    pub fn write(&self, text: &str) -> io::Result<PathBuf> {
        let _guard = self.lock.lock().expect("spill directory mutex poisoned");
        create_private_dir(&self.dir)?;
        let name = content_name(text);
        let path = self.dir.join(format!("{name}.txt"));
        if path.is_file() {
            // Reused: mark it recently used so the prune keeps it.
            fs::File::options().append(true).open(&path)?.set_modified(SystemTime::now())?;
        } else {
            let tmp = self.dir.join(format!("{name}.{}.tmp", std::process::id()));
            let mut f = private_file(&tmp)?;
            f.write_all(text.as_bytes())?;
            drop(f);
            fs::rename(&tmp, &path)?;
        }
        self.prune(&path);
        Ok(path)
    }

    /// Remove the least recently used files until the directory fits its
    /// budget; `keep` (just written) always stays.
    fn prune(&self, keep: &Path) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(SystemTime, PathBuf, u64)> = entries
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                m.is_file().then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), e.path(), m.len()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.2).sum();
        files.sort();
        for (_, path, len) in files {
            if total <= self.budget {
                break;
            }
            if path != keep && fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }
}

/// First 128 bits of the SHA-256 of `text`, in hex.
fn content_name(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..16].iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `$XDG_CACHE_HOME/<tool>`, `~/.cache/<tool>`, `%LOCALAPPDATA%\<tool>`, or a
/// per-user directory under the temporary directory when none is set.
pub fn cache_dir(tool: &str) -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) { var("LOCALAPPDATA") } else { var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|h| h.join(".cache"))) };
    base.map(|b| b.join(tool)).unwrap_or_else(|| {
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into());
        std::env::temp_dir().join(format!("{tool}-{user}"))
    })
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.is_dir() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // Output quotes the analysed binary's strings and code: this user only.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    // %LOCALAPPDATA% is already private to the user.
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::File::options().write(true).create(true).truncate(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_text_lands_in_the_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(tmp.path().join("out"), DEFAULT_BUDGET);
        let a = spill.write("hello\n").unwrap();
        let b = spill.write("hello\n").unwrap();
        assert_eq!(a, b);
        assert_eq!(fs::read_to_string(&a).unwrap(), "hello\n");
        assert_ne!(spill.write("other\n").unwrap(), a);
    }

    #[test]
    fn prune_keeps_the_file_just_written() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(tmp.path().join("out"), 8);
        let old = spill.write("0123456789\n").unwrap();
        let new = spill.write("abcdefghij\n").unwrap();
        assert!(!old.exists(), "over budget: the older file goes");
        assert!(new.exists(), "the file just written always stays");
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(tmp.path().join("out"), DEFAULT_BUDGET);
        let path = spill.write("x\n").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(spill.dir()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }
}
