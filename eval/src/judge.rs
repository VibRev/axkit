//! Deciding whether a run passed, from the agent's final answer and the
//! workspace it left behind.

use crate::suite::Judge;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// The verdict on one run: `Ok(())` passes, `Err(reason)` fails with why.
pub type Verdict = Result<(), String>;

/// Check `answer` and the `workspace` against the judge. Every check that is
/// given must hold; the first failure is the reason.
///
/// # Errors
///
/// A judge command could not be launched (distinct from exiting non-zero,
/// which is a plain failure).
pub fn verdict(judge: &Judge, answer: &str, workspace: &Path, answer_file: &Path, env: &[(String, String)]) -> Result<Verdict> {
    for needle in &judge.contains {
        if !answer.contains(needle) {
            return Ok(Err(format!("answer lacks {needle:?}")));
        }
    }
    for needle in &judge.excludes {
        if answer.contains(needle) {
            return Ok(Err(format!("answer contains the wrong value {needle:?}")));
        }
    }
    if let Some(pattern) = &judge.regex {
        let re = regex::Regex::new(pattern).with_context(|| format!("judge regex {pattern:?}"))?;
        if !re.is_match(answer) {
            return Ok(Err(format!("answer does not match /{pattern}/")));
        }
    }
    if !judge.command.is_empty() {
        let (program, args) = judge.command.split_first().context("empty judge command")?;
        let output = Command::new(program)
            .args(args)
            .current_dir(workspace)
            .env("ANSWER_FILE", answer_file)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .with_context(|| format!("running judge command {program:?}"))?;
        if !output.status.success() {
            let tail: String = String::from_utf8_lossy(&output.stderr).lines().next_back().unwrap_or_default().to_owned();
            return Ok(Err(format!(
                "judge command failed ({}){}",
                output.status,
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            )));
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn judge(contains: &[&str], excludes: &[&str], regex: Option<&str>, command: &[&str]) -> Judge {
        Judge {
            contains: contains.iter().map(|s| (*s).to_owned()).collect(),
            excludes: excludes.iter().map(|s| (*s).to_owned()).collect(),
            regex: regex.map(str::to_owned),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn contains_regex_and_excludes_all_apply() {
        let dir = tempfile::tempdir().unwrap();
        let j = judge(&["s3cret"], &["wrong"], Some(r"0x[0-9a-f]+"), &[]);
        let v = verdict(&j, "the password is s3cret at 0x401000", dir.path(), dir.path(), &[]).unwrap();
        assert!(v.is_ok(), "{v:?}");
        let miss = verdict(&j, "found it at 0x401000", dir.path(), dir.path(), &[]).unwrap();
        assert!(miss.unwrap_err().contains("s3cret"));
        let bad = verdict(&j, "s3cret but also wrong at 0x1", dir.path(), dir.path(), &[]).unwrap();
        assert!(bad.unwrap_err().contains("wrong"));
    }

    #[test]
    fn a_command_judges_the_workspace_and_sees_answer_file() {
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer.txt");
        fs::write(&answer, "renamed_ok").unwrap();
        let pass = judge(&[], &[], None, &["sh", "-c", "grep -q renamed_ok \"$ANSWER_FILE\""]);
        assert!(verdict(&pass, "renamed_ok", dir.path(), &answer, &[]).unwrap().is_ok());
        let fail = judge(&[], &[], None, &["sh", "-c", "exit 1"]);
        assert!(verdict(&fail, "", dir.path(), &answer, &[]).unwrap().unwrap_err().contains("judge command failed"));
    }
}
