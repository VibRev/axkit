//! Replaying the tool calls agents failed with against a build, so a change
//! to the tool's errors and leniency is measured in minutes instead of a
//! whole eval: every call is run again and sorted into ok (it works now),
//! hinted (it fails, saying what to do next) or silent (it fails and does
//! not). The aim is zero silent. A call whose target an earlier step made
//! (a saved `.bndb`, a script the agent wrote) cannot be run alone and is
//! skipped, outside the three.

use crate::harness::Call;
use crate::runner;
use crate::suite::{Suite, Vars};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Where an eval run's workspace was: `…/target/eval-data/<out>/<suite>/ws/<run>`.
static WORKSPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[^\s'"`]*/target/eval-data/[^\s'"`]*?/ws/[^/\s'"`\\]+"#).expect("the workspace regex is valid"));

/// What a replayed call came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Ok,
    Hinted,
    Silent,
    /// Not run: it reads a file the run made before it.
    Skipped,
}

impl Class {
    pub const ALL: [Class; 4] = [Class::Ok, Class::Hinted, Class::Silent, Class::Skipped];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Ok => "ok",
            Class::Hinted => "hinted",
            Class::Silent => "silent",
            Class::Skipped => "skipped",
        }
    }
}

/// One failed call, deduplicated across runs: what to run again and what the
/// agent saw the first time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Case {
    pub suite: String,
    /// The harnesses whose agents made this call.
    pub harnesses: Vec<String>,
    /// The first run it came from: `<suite>.<group>.<task>.<rep>`.
    pub source: String,
    /// How many failed calls it stands for.
    pub seen: usize,
    /// One invocation of the tool, with `{ws}` for the run's workspace.
    pub command: String,
    /// The error the agent saw: its first `Error:` line, or its first line.
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed: Option<Replayed>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Replayed {
    pub class: Class,
    pub exit: Option<i32>,
    /// The first error line, or the first line of output.
    pub line: String,
    pub hint: Option<String>,
}

impl Case {
    fn key(&self) -> (&str, &str) {
        (&self.suite, &self.command)
    }
}

/// The failed calls of one run that invoked `tool`, one case per invocation.
///
/// A call counts as failed when what came back holds an `Error:` (or clap's
/// `error:`) line, which also catches failures a pipe hid from the harness;
/// or when the harness marked it failed and it ran nothing but the tool (and
/// `cd`), so the failure can only be the tool's.
#[must_use]
pub fn extract(suite: &str, harness: &str, source: &str, tool: &str, calls: &[Call]) -> Vec<Case> {
    let mut cases = Vec::new();
    for call in calls {
        let command = normalize(&call.command);
        let statements = statements(&command);
        let invoking: Vec<&Vec<String>> = statements.iter().filter(|s| s.iter().any(|stage| invokes(stage, tool))).collect();
        if invoking.is_empty() {
            continue;
        }
        let output = normalize(&call.output);
        let error = error_line(&output);
        let only_tool = statements.iter().all(|s| s.iter().any(|stage| invokes(stage, tool)) || is_cd(s));
        if error.is_none() && !(call.failed && only_tool) {
            continue;
        }
        let error = error.unwrap_or_else(|| first_line(&output));
        for statement in invoking {
            if let Some(command) = replayable(statement, tool) {
                cases.push(Case {
                    suite: suite.to_owned(),
                    harnesses: vec![harness.to_owned()],
                    source: source.to_owned(),
                    seen: 1,
                    command,
                    error: error.clone(),
                    replayed: None,
                });
            }
        }
    }
    cases
}

/// Cases merged by (suite, command, error), first seen first.
#[must_use]
pub fn dedupe(cases: Vec<Case>) -> Vec<Case> {
    let mut merged: Vec<Case> = Vec::new();
    let mut index: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for case in cases {
        let key = (case.suite.clone(), case.command.clone(), case.error.clone());
        match index.get(&key) {
            Some(&i) => {
                let kept = &mut merged[i];
                kept.seen += case.seen;
                for h in case.harnesses {
                    if !kept.harnesses.contains(&h) {
                        kept.harnesses.push(h);
                    }
                }
            }
            None => {
                index.insert(key, merged.len());
                merged.push(case);
            }
        }
    }
    merged
}

/// Every eval workspace path replaced by `{ws}`.
#[must_use]
pub fn normalize(text: &str) -> String {
    WORKSPACE.replace_all(text, "{ws}").into_owned()
}

fn error_line(output: &str) -> Option<String> {
    output.lines().map(str::trim).find(|l| l.starts_with("Error:") || l.starts_with("error:")).map(|l| clip(l, 200))
}

fn first_line(output: &str) -> String {
    output.lines().map(str::trim).find(|l| !l.is_empty() && !is_exit_banner(l)).map_or_else(|| "(no output)".to_owned(), |l| clip(l, 200))
}

/// `exit: 1` (grok) or `Exit code 1` (Claude Code): the harness's, not the tool's.
fn is_exit_banner(line: &str) -> bool {
    ["exit: ", "Exit code "].iter().any(|p| line.strip_prefix(p).is_some_and(|n| n.trim().parse::<i64>().is_ok()))
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

fn is_cd(statement: &[String]) -> bool {
    statement.len() == 1 && words(&statement[0]).first().is_some_and(|w| w == "cd")
}

/// Does this pipeline stage run `tool` (bare, by a path ending in it, or
/// behind `VAR=value`, `env` or `timeout N`)?
fn invokes(stage: &str, tool: &str) -> bool {
    let words = words(stage);
    let mut rest = words.iter().map(String::as_str).skip_while(|w| w.contains('=') && !w.starts_with('-'));
    let mut first = rest.next();
    while let Some(w) = first {
        match w {
            "env" | "command" | "exec" => first = rest.next(),
            "timeout" => {
                let _duration = rest.next();
                first = rest.next();
            }
            _ => break,
        }
    }
    first.is_some_and(|w| w == tool || w.ends_with(&format!("/{tool}")))
}

/// What to run again for a statement that invokes the tool: the tool's own
/// stage, without the filters after it or redirections of its output (so its
/// whole stdout and stderr are seen); with the stages before it when they
/// only feed it text (`cat`, `echo`, `printf`). `None` when other programs
/// feed it.
fn replayable(statement: &[String], tool: &str) -> Option<String> {
    let at = statement.iter().position(|stage| invokes(stage, tool))?;
    let feeders = &statement[..at];
    if !feeders.iter().all(|s| words(s).first().is_some_and(|w| ["cat", "echo", "printf"].contains(&w.as_str()))) {
        return None;
    }
    let mut stages: Vec<String> = feeders.iter().map(|s| s.trim().to_owned()).collect();
    stages.push(strip_output_redirections(statement[at].trim()));
    Some(stages.join(" | "))
}

/// The statements of a shell command line (split at top-level `&&`, `||`,
/// `;`, `&` and newlines), each a pipeline of stages (split at `|`). Quotes,
/// `$(...)`, backticks, comments and here-documents are respected; a
/// here-document's body stays with the stage that reads it.
#[must_use]
pub fn statements(command: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut pipeline: Vec<String> = Vec::new();
    let mut stage = String::new();
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let (mut single, mut double, mut backtick, mut depth) = (false, false, false, 0usize);
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let end_stage = |stage: &mut String, pipeline: &mut Vec<String>| {
        if !stage.trim().is_empty() {
            pipeline.push(std::mem::take(stage));
        }
        stage.clear();
    };
    let end_statement = |stage: &mut String, pipeline: &mut Vec<String>, out: &mut Vec<Vec<String>>| {
        end_stage(stage, pipeline);
        if !pipeline.is_empty() {
            out.push(std::mem::take(pipeline));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if single {
            single = c != '\'';
            stage.push(c);
            i += 1;
            continue;
        }
        if c == '\\' {
            stage.push(c);
            if let Some(n) = next {
                stage.push(n);
            }
            i += 2;
            continue;
        }
        if double {
            double = c != '"';
            stage.push(c);
            i += 1;
            continue;
        }
        let nested = depth > 0 || backtick;
        match c {
            '\'' => single = true,
            '"' => double = true,
            '`' => backtick = !backtick,
            '$' if next == Some('(') => {
                depth += 1;
                stage.push_str("$(");
                i += 2;
                continue;
            }
            ')' if depth > 0 => depth -= 1,
            _ => {}
        }
        if nested || matches!(c, '\'' | '"' | '`') {
            stage.push(c);
            i += 1;
            continue;
        }
        match c {
            '#' if stage.is_empty() || stage.ends_with(char::is_whitespace) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' if next == Some('<') && chars.get(i + 2) != Some(&'<') => {
                let (delimiter, strip_tabs, len) = heredoc_delimiter(&chars[i + 2..]);
                stage.extend(&chars[i..i + 2 + len]);
                i += 2 + len;
                if !delimiter.is_empty() {
                    heredocs.push((delimiter, strip_tabs));
                }
            }
            '\n' if !heredocs.is_empty() => {
                // The bodies follow this line, one after another.
                stage.push('\n');
                i += 1;
                for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                    while i < chars.len() {
                        let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..line_end].iter().collect();
                        stage.push_str(&line);
                        i = line_end;
                        let done = if strip_tabs { line.trim_start_matches('\t') == delimiter } else { line == delimiter };
                        if i < chars.len() && !done {
                            stage.push('\n');
                            i += 1;
                        }
                        if done {
                            break;
                        }
                    }
                }
            }
            '&' | '|' if next == Some(c) => {
                end_statement(&mut stage, &mut pipeline, &mut out);
                i += 2;
            }
            '|' => {
                end_stage(&mut stage, &mut pipeline);
                i += 1;
            }
            '&' if stage.ends_with('>') || next == Some('>') => {
                stage.push(c);
                i += 1;
            }
            ';' | '&' | '\n' => {
                end_statement(&mut stage, &mut pipeline, &mut out);
                i += 1;
            }
            _ => {
                stage.push(c);
                i += 1;
            }
        }
    }
    end_statement(&mut stage, &mut pipeline, &mut out);
    out
}

/// The word after `<<`: its text, whether `<<-` strips tabs, and how many
/// characters (dash, spaces, quotes) it took.
fn heredoc_delimiter(rest: &[char]) -> (String, bool, usize) {
    let mut i = 0;
    let strip_tabs = rest.first() == Some(&'-');
    if strip_tabs {
        i += 1;
    }
    while rest.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
        i += 1;
    }
    let mut word = String::new();
    let quote = rest.get(i).copied().filter(|c| *c == '\'' || *c == '"');
    if let Some(q) = quote {
        i += 1;
        while let Some(&c) = rest.get(i) {
            i += 1;
            if c == q {
                break;
            }
            word.push(c);
        }
    } else {
        while let Some(&c) = rest.get(i).filter(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | '<' | '>' | ')')) {
            word.push(c.to_owned());
            i += 1;
        }
        word = word.replace(['\\', '\'', '"'], "");
    }
    (word, strip_tabs, i)
}

/// A stage's words, unquoted, up to its first here-document body.
fn words(stage: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut single, mut double, mut started) = (false, false, false);
    for c in stage.lines().next().unwrap_or_default().chars() {
        match c {
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            c if c.is_whitespace() && !single && !double => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// The stage without `>file`, `>>file`, `2>file`, `&>file`, `2>&1` and the
/// like outside quotes. Input redirections and here-documents stay.
fn strip_output_redirections(stage: &str) -> String {
    let chars: Vec<char> = stage.chars().collect();
    let mut out = String::new();
    let (mut single, mut double, mut i) = (false, false, 0);
    while i < chars.len() {
        let c = chars[i];
        if single || double || c == '\'' || c == '"' {
            match c {
                '\'' if !double => single = !single,
                '"' if !single => double = !double,
                _ => {}
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\n' {
            // A here-document body: kept as it is.
            out.extend(&chars[i..]);
            break;
        }
        let at_word = out.is_empty() || out.ends_with(char::is_whitespace);
        let fd = matches!(c, '1' | '2' | '&') && chars.get(i + 1) == Some(&'>');
        if at_word && (c == '>' || fd) {
            let mut j = i + usize::from(fd) + 1;
            if chars.get(j) == Some(&'>') {
                j += 1;
            }
            if chars.get(j) == Some(&'&') {
                j += 1;
            }
            while chars.get(j).is_some_and(|c| *c == ' ') {
                j += 1;
            }
            while chars.get(j).is_some_and(|c| !c.is_whitespace()) {
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ").replace(" \n", "\n")
}

/// Subcommands whose first positional is the target.
const TARGETED: [&str; 5] = ["open", "status", "save", "close", "exec"];

/// Options whose value is the next word.
const VALUED: [&str; 12] =
    ["--offset", "--limit", "--max-bytes", "--truncate", "-o", "--output", "--timeout", "--to", "-c", "--code", "-f", "--file"];

/// Why `command` cannot be replayed in a workspace holding only the fixtures:
/// the target or `-f` script it names is not in `ws`, so an earlier step of
/// the run made it. `None` when everything it reads is there.
#[must_use]
pub fn missing_input(command: &str, tool: &str, ws: &Path) -> Option<String> {
    for statement in statements(command) {
        let Some(stage) = statement.iter().find(|stage| invokes(stage, tool)) else { continue };
        let words = words(stage);
        let Some(at) = words.iter().position(|w| w == tool || w.ends_with(&format!("/{tool}"))) else { continue };
        let mut rest = words[at + 1..].iter().skip_while(|w| w.starts_with('-'));
        if !rest.next().is_some_and(|sub| TARGETED.contains(&sub.as_str())) {
            continue;
        }
        let (mut target, mut script) = (None, None);
        while let Some(w) = rest.next() {
            if VALUED.contains(&w.as_str()) {
                let value = rest.next();
                if matches!(w.as_str(), "-f" | "--file") {
                    script = value.filter(|v| *v != "-");
                }
            } else if !w.starts_with('-') && target.is_none() {
                target = Some(w);
            }
        }
        for (what, file) in [("target", target), ("script", script)] {
            if let Some(file) = file
                && !ws.join(file).exists()
            {
                return Some(format!("the {what} {file} is not in the workspace: an earlier step of the run made it"));
            }
        }
    }
    None
}

/// Sort what a replayed call printed: exit 0 is ok; otherwise a `hint:` line,
/// or usage text that names the next step (`Usage:`, `--help`), makes it
/// hinted; anything else is silent.
#[must_use]
pub fn classify(exit: Option<i32>, stdout: &str, stderr: &str) -> Replayed {
    let text = format!("{stderr}\n{stdout}");
    let line = error_line(&text).unwrap_or_else(|| first_line(if stderr.trim().is_empty() { stdout } else { stderr }));
    let hint = text.lines().map(str::trim).find(|l| l.starts_with("hint:")).map(|l| clip(l, 200));
    let class = if exit == Some(0) {
        Class::Ok
    } else if hint.is_some() || text.lines().any(|l| l.trim_start().starts_with("Usage:") || l.contains("--help")) {
        Class::Hinted
    } else {
        Class::Silent
    };
    Replayed { class, exit, line, hint }
}

/// Run `suite`'s cases again: in one fresh workspace holding every task's
/// files, with the suite's environment and `path` (or `bin` instead) in
/// front of `PATH`, one after another, each within `timeout`.
///
/// # Errors
///
/// The suite's setup fails, or the workspace cannot be made.
pub fn run(suite: &Suite, cases: &mut [Case], bin: &[PathBuf], work: &Path, timeout: Duration) -> Result<()> {
    runner::setup(suite)?;
    fs::create_dir_all(work).with_context(|| format!("creating {}", work.display()))?;
    let ws =
        tempfile::Builder::new().prefix(&format!("{}.replay.", suite.suite.name)).tempdir_in(work).context("creating the workspace")?;
    // Short, under /tmp: the tool's daemon socket lives in it.
    let tmp = tempfile::Builder::new().prefix("axr-").tempdir_in("/tmp").context("creating the tmp dir")?;
    for file in suite.tasks.iter().flat_map(|t| &t.files) {
        let src = suite.resolve(file);
        let name = Path::new(file).file_name().context("task file has no name")?;
        fs::copy(&src, ws.path().join(name)).with_context(|| format!("copying fixture {}", src.display()))?;
    }
    let vars = Vars::default()
        .with("ws", ws.path().to_string_lossy().into_owned())
        .with("tmp", tmp.path().to_string_lossy().into_owned())
        .with("suite", suite.dir.to_string_lossy().into_owned());
    let mut env: Vec<(String, String)> = suite.suite.env.iter().map(|(k, v)| (k.clone(), vars.expand(v))).collect();
    let dirs: Vec<PathBuf> = if bin.is_empty() { suite.suite.path.iter().map(|p| suite.resolve(p)).collect() } else { bin.to_vec() };
    runner::prepend_dirs(&mut env, &dirs);

    let ws_path = ws.path().to_string_lossy().into_owned();
    for case in cases.iter_mut() {
        let command = case.command.replace("{ws}", &ws_path);
        if let Some(why) = suite.suite.tool.as_deref().and_then(|tool| missing_input(&command, tool, ws.path())) {
            case.replayed = Some(Replayed { class: Class::Skipped, exit: None, line: why.replace(&ws_path, "{ws}"), hint: None });
            continue;
        }
        let mut replayed = execute(&command, ws.path(), &env, timeout);
        // Read like the case: the workspace is `{ws}` there too.
        replayed.line = replayed.line.replace(&ws_path, "{ws}");
        replayed.hint = replayed.hint.map(|h| h.replace(&ws_path, "{ws}"));
        case.replayed = Some(replayed);
    }
    for line in &suite.suite.teardown {
        let _ = runner::sh(line, ws.path(), &env);
    }
    Ok(())
}

fn execute(command: &str, dir: &Path, env: &[(String, String)], timeout: Duration) -> Replayed {
    let spawned = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Replayed { class: Class::Silent, exit: None, line: format!("could not run bash: {e}"), hint: None },
    };
    let pipe = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    };
    let out = pipe(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = pipe(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (stdout, stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    match status {
        Some(status) => classify(status.code(), &stdout, &stderr),
        None => Replayed { class: Class::Silent, exit: None, line: format!("timed out after {} s", timeout.as_secs()), hint: None },
    }
}

/// Counts per harness and suite, then the silent and hinted cases.
#[must_use]
pub fn table(cases: &[Case]) -> String {
    let mut s = String::new();
    let count = |cases: &mut dyn Iterator<Item = &Case>| {
        let mut n = BTreeMap::new();
        for c in cases {
            if let Some(r) = &c.replayed {
                *n.entry(r.class).or_insert(0usize) += 1;
            }
        }
        n
    };
    let cell = |n: &BTreeMap<Class, usize>, class| n.get(&class).copied().unwrap_or(0);
    let all = count(&mut cases.iter());
    let _ = writeln!(
        s,
        "# replay | {} calls | ok {} | hinted {} | silent {} | skipped {}",
        cases.len(),
        cell(&all, Class::Ok),
        cell(&all, Class::Hinted),
        cell(&all, Class::Silent),
        cell(&all, Class::Skipped)
    );
    let _ = writeln!(s, "# harness | suite | calls | ok | hinted | silent | skipped");
    let mut rows: Vec<(&str, &str)> = cases.iter().flat_map(|c| c.harnesses.iter().map(move |h| (h.as_str(), c.suite.as_str()))).collect();
    rows.sort_unstable();
    rows.dedup();
    for (harness, suite) in rows {
        let of = || cases.iter().filter(move |c| c.suite == suite && c.harnesses.iter().any(|h| h == harness));
        let n = count(&mut of());
        let _ = writeln!(
            s,
            "{harness} | {suite} | {} | {} | {} | {} | {}",
            of().count(),
            cell(&n, Class::Ok),
            cell(&n, Class::Hinted),
            cell(&n, Class::Silent),
            cell(&n, Class::Skipped)
        );
    }
    for class in [Class::Silent, Class::Hinted, Class::Skipped] {
        let listed: Vec<&Case> = cases.iter().filter(|c| c.replayed.as_ref().is_some_and(|r| r.class == class)).collect();
        if listed.is_empty() {
            continue;
        }
        let column = match class {
            Class::Silent => "line",
            Class::Skipped => "why",
            Class::Ok | Class::Hinted => "hint",
        };
        let _ = writeln!(s, "# {} | harnesses | exit | command | {column}", class.as_str());
        for c in listed {
            let Some(r) = &c.replayed else { continue };
            let said = if class == Class::Hinted { r.hint.as_ref().unwrap_or(&r.line) } else { &r.line };
            let exit = r.exit.map_or_else(|| "-".to_owned(), |e| e.to_string());
            let _ =
                writeln!(s, "{} | {} | {exit} | {} | {}", c.suite, c.harnesses.join(","), one_line(&c.command, 120), one_line(said, 160));
        }
    }
    s
}

/// How each case of `a` fared in `b`, matched by suite and command.
#[must_use]
pub fn diff(a: &[Case], b: &[Case]) -> String {
    let before: BTreeMap<(&str, &str), Class> = a.iter().filter_map(|c| c.replayed.as_ref().map(|r| (c.key(), r.class))).collect();
    let mut moves: BTreeMap<(Option<Class>, Class), usize> = BTreeMap::new();
    let mut changed = Vec::new();
    for c in b {
        let Some(r) = &c.replayed else { continue };
        let from = before.get(&c.key()).copied();
        *moves.entry((from, r.class)).or_insert(0) += 1;
        if from != Some(r.class) {
            changed.push((from, r.class, c));
        }
    }
    let name = |c: Option<Class>| c.map_or("new", Class::as_str);
    let mut s = String::from("# from | to | calls\n");
    for ((from, to), n) in &moves {
        let _ = writeln!(s, "{} | {} | {n}", name(*from), to.as_str());
    }
    if !changed.is_empty() {
        let _ = writeln!(s, "# changed: from | to | suite | command | now");
        for (from, to, c) in changed {
            let now = c.replayed.as_ref().map(|r| r.hint.clone().unwrap_or_else(|| r.line.clone())).unwrap_or_default();
            let _ = writeln!(s, "{} | {} | {} | {} | {}", name(from), to.as_str(), c.suite, one_line(&c.command, 100), one_line(&now, 140));
        }
    }
    s
}

fn one_line(s: &str, max: usize) -> String {
    clip(&s.replace('\n', "\\n"), max)
}

/// Read a replay file: one case per line.
///
/// # Errors
///
/// The file cannot be read or holds a line that is not a case.
pub fn load(path: &Path) -> Result<Vec<Case>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).with_context(|| format!("a line of {}", path.display())))
        .collect()
}

/// Write cases one per line.
///
/// # Errors
///
/// The file cannot be written.
pub fn save(path: &Path, cases: &[Case]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    for c in cases {
        text.push_str(&serde_json::to_string(c)?);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(command: &str, output: &str, failed: bool) -> Call {
        Call { command: command.into(), output: output.into(), failed }
    }

    #[test]
    fn statements_split_at_operators_but_not_inside_quotes_or_heredocs() {
        let s = statements("cd /w && bnx exec t -c 'a; b && c | d' 2>&1 | head -5; echo x\nbnx close t");
        assert_eq!(
            s,
            vec![
                vec!["cd /w ".to_owned()],
                vec![" bnx exec t -c 'a; b && c | d' 2>&1 ".into(), " head -5".into()],
                vec![" echo x".into()],
                vec!["bnx close t".into()]
            ]
        );
        let h = statements("bnx exec t - <<'EOF'\nfor f in x:\n    print(f) # c; d\nEOF\nbnx close t");
        assert_eq!(h.len(), 2, "{h:?}");
        assert!(h[0][0].ends_with("print(f) # c; d\nEOF"), "{h:?}");
        assert_eq!(statements("a $(b | c) | d # e | f").concat(), ["a $(b | c) ", " d "]);
    }

    #[test]
    fn the_tool_is_found_behind_env_and_paths() {
        for stage in ["idax exec t", "IDAX_LIMIT=5 idax list", "/x/target/debug/idax ref", "timeout 60 idax open t", "env idax status"] {
            assert!(invokes(stage, "idax"), "{stage}");
        }
        for stage in ["file idax", "grep idax log", "idaxx exec"] {
            assert!(!invokes(stage, "idax"), "{stage}");
        }
    }

    #[test]
    fn only_the_tool_stage_is_replayed_without_output_redirections() {
        let s = statements("bnx exec t -c 'print(1 > 0)' 2>&1 > /tmp/o.txt | grep x");
        assert_eq!(replayable(&s[0], "bnx").unwrap(), "bnx exec t -c 'print(1 > 0)'");
        let fed = statements("cat s.py | bnx exec t -");
        assert_eq!(replayable(&fed[0], "bnx").unwrap(), "cat s.py | bnx exec t -");
        assert_eq!(replayable(&statements("python gen.py | bnx exec t -")[0], "bnx"), None);
    }

    #[test]
    fn failures_are_errors_seen_even_through_a_pipe_and_paths_become_ws() {
        let ws = "/home/u/x/target/eval-data/full/bnx-re/ws/bnx-re.claude.t.0.AbC123";
        let calls = [
            call(
                &format!("cd {ws} && bnx exec manyfuncs 'for s in bv.strings: print(s)' 2>&1 | head"),
                &format!("Error: not_found: cannot read the script {ws}/for s"),
                false,
            ),
            call("bnx exec t -c 'functions()'", "# addr | name\n0x1 | f", false),
            call("file t && bnx open t", "exit: 1\nt: cannot open", true),
            call("bnx help", "error: unrecognized subcommand 'help'", true),
            call("bnx exec t -c 'x'", "exit: 3\n", true),
        ];
        let cases = extract("bnx-re", "claude", "bnx-re.claude.t.0", "bnx", &calls);
        let got: Vec<(&str, &str)> = cases.iter().map(|c| (c.command.as_str(), c.error.as_str())).collect();
        assert_eq!(
            got,
            [
                ("bnx exec manyfuncs 'for s in bv.strings: print(s)'", "Error: not_found: cannot read the script {ws}/for s"),
                ("bnx help", "error: unrecognized subcommand 'help'"),
                ("bnx exec t -c 'x'", "(no output)"),
            ],
            "a call with no tool error that also ran another program is not the tool's failure"
        );
    }

    #[test]
    fn duplicates_merge_and_keep_every_harness() {
        let a = extract("s", "claude", "a", "bnx", &[call("bnx help", "error: x", true)]);
        let b = extract("s", "grok", "b", "bnx", &[call("bnx help", "error: x", true), call("bnx help", "error: x", true)]);
        let merged = dedupe([a, b].concat());
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].seen, merged[0].harnesses.clone()), (3, vec!["claude".to_owned(), "grok".to_owned()]));
    }

    #[test]
    fn classes_follow_exit_and_hints() {
        assert_eq!(classify(Some(0), "x", "").class, Class::Ok);
        let hinted = classify(Some(1), "", "Error: script_error: NameError: x\nhint: did you mean: y?\n");
        assert_eq!(
            (hinted.class, hinted.hint.as_deref(), hinted.line.as_str()),
            (Class::Hinted, Some("hint: did you mean: y?"), "Error: script_error: NameError: x")
        );
        let usage = classify(Some(1), "", "Error: usage: unexpected argument 'x'\n\nFor more information, try '--help'.\n");
        assert_eq!(usage.class, Class::Hinted);
        let silent = classify(Some(1), "", "Error: not_found: cannot read the script /w/for s\n");
        assert_eq!((silent.class, silent.line.as_str()), (Class::Silent, "Error: not_found: cannot read the script /w/for s"));
    }

    // The fake tool is a shell script with an executable bit.
    #[cfg(unix)]
    #[test]
    fn replaying_runs_in_a_fresh_workspace_with_the_fixtures() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fix.bin"), b"data").unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        let tool = dir.path().join("bin/faketool");
        fs::write(&tool, "#!/bin/sh\ncase \"$1\" in ok) cat fix.bin;; hint) echo 'Error: x: y' >&2; echo 'hint: try ok' >&2; exit 1;; *) echo 'Error: z' >&2; exit 1;; esac\n").unwrap();
        fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let suite_toml = "[suite]\nname = \"s\"\npath = [\"bin\"]\n[[task]]\nid = \"t\"\nprompt = \"p\"\nfiles = [\"fix.bin\"]\n[task.judge]\ncontains = [\"x\"]\n[[group]]\nname = \"g\"\n";
        fs::write(dir.path().join("suite.toml"), suite_toml).unwrap();
        let suite = Suite::load(&dir.path().join("suite.toml")).unwrap();
        let mut cases: Vec<Case> = ["faketool ok", "faketool hint", "faketool nope", "cat {ws}/fix.bin | faketool ok"]
            .iter()
            .map(|c| Case {
                suite: "s".into(),
                harnesses: vec!["claude".into()],
                source: "x".into(),
                seen: 1,
                command: (*c).into(),
                error: "e".into(),
                replayed: None,
            })
            .collect();
        run(&suite, &mut cases, &[], &dir.path().join("work"), Duration::from_secs(10)).unwrap();
        let classes: Vec<Class> = cases.iter().map(|c| c.replayed.as_ref().unwrap().class).collect();
        assert_eq!(classes, [Class::Ok, Class::Hinted, Class::Silent, Class::Ok]);
        let shown = table(&cases);
        assert!(shown.starts_with("# replay | 4 calls | ok 2 | hinted 1 | silent 1 | skipped 0\n"), "{shown}");
        assert!(shown.contains("claude | s | 4 | 2 | 1 | 1 | 0"), "{shown}");
        assert!(shown.contains("# silent") && shown.contains("faketool nope | Error: z"), "{shown}");

        let mut later = cases.clone();
        later[2].replayed.as_mut().unwrap().class = Class::Hinted;
        let d = diff(&cases, &later);
        assert!(d.contains("silent | hinted | 1") && d.contains("ok | ok | 2"), "{d}");
        assert!(d.contains("# changed") && d.contains("faketool nope"), "{d}");
    }

    #[test]
    fn a_call_on_a_file_an_earlier_step_made_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        fs::write(ws.join("renameme"), b"x").unwrap();
        for ok in ["bnx exec renameme 'x'", "bnx --timeout 5 exec renameme -c x", "bnx exec renameme - <<'E'\nx\nE", "bnx help", "bnx list"]
        {
            assert_eq!(missing_input(ok, "bnx", ws), None, "{ok}");
        }
        let why = missing_input("cd /w && bnx exec ./renameme.bndb 'x' | head", "bnx", ws).unwrap();
        assert!(why.contains("target ./renameme.bndb is not in the workspace"), "{why}");
        assert_eq!(missing_input("idax exec renameme -f s.py", "bnx", ws), None, "another tool's call is not looked at");
        let why = missing_input("bnx exec --timeout 9 renameme -f s.py", "bnx", ws).unwrap();
        assert!(why.contains("script s.py"), "{why}");

        let suite_toml = "[suite]\nname = \"s\"\ntool = \"bnx\"\n[[task]]\nid = \"t\"\nprompt = \"p\"\nfiles = [\"renameme\"]\n[task.judge]\ncontains = [\"x\"]\n[[group]]\nname = \"g\"\n";
        fs::write(ws.join("suite.toml"), suite_toml).unwrap();
        let suite = Suite::load(&ws.join("suite.toml")).unwrap();
        let mut cases = vec![Case {
            suite: "s".into(),
            harnesses: vec!["claude".into()],
            source: "x".into(),
            seen: 1,
            command: "bnx exec {ws}/renameme.bndb 'x'".into(),
            error: "e".into(),
            replayed: None,
        }];
        run(&suite, &mut cases, &[], &ws.join("work"), Duration::from_secs(10)).unwrap();
        let r = cases[0].replayed.as_ref().unwrap();
        assert_eq!((r.class, r.exit), (Class::Skipped, None));
        assert!(r.line.contains("{ws}/renameme.bndb"), "{}", r.line);
        let shown = table(&cases);
        assert!(shown.contains("skipped 1") && shown.contains("# skipped | harnesses | exit | command | why"), "{shown}");
    }

    #[test]
    fn a_replay_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let case = Case {
            suite: "s".into(),
            harnesses: vec!["grok".into()],
            source: "x".into(),
            seen: 2,
            command: "bnx help".into(),
            error: "error: y".into(),
            replayed: Some(Replayed { class: Class::Hinted, exit: Some(1), line: "l".into(), hint: Some("hint: h".into()) }),
        };
        let path = dir.path().join("r.jsonl");
        save(&path, std::slice::from_ref(&case)).unwrap();
        assert_eq!(load(&path).unwrap(), [case]);
    }
}
