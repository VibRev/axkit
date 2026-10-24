//! `axkit-eval`: run task sets under groups, then summarize.

use anyhow::{Context, Result, bail};
use axkit_eval::cleanup::{self, Report};
use axkit_eval::harness::{self, EVAL_MARK, Harness};
use axkit_eval::price::Prices;
use axkit_eval::replay::{self, Case};
use axkit_eval::report::{self, By, Run};
use axkit_eval::runner::{self, Plan};
use axkit_eval::suite::{Group, Suite, Task};
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "axkit-eval", about = "Run agent harnesses against task sets and measure success, turns, tokens and cost")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run suites; one JSON line per run goes to <out>/results.jsonl.
    Run(RunArgs),
    /// Aggregate results files into one table.
    Summary {
        results: Vec<PathBuf>,
        /// Dimensions to group by, comma-separated: group, harness, skill, suite, task.
        #[arg(long, default_value = "group", value_delimiter = ',')]
        by: Vec<By>,
        /// Also print one line per run.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Two results files side by side, rows aligned by the `--by` key
    /// (`--by suite,group,task` lines up each task of each group).
    Compare {
        a: PathBuf,
        b: PathBuf,
        #[arg(long, default_value = "group", value_delimiter = ',')]
        by: Vec<By>,
    },
    /// Load suites and list their tasks and groups without running anything.
    Check { suites: Vec<PathBuf> },
    /// Remove every eval-made entry (path under `/target/eval-data/`) from
    /// the harnesses' history, and print the inventory before and after.
    Purge,
    /// Run the tool calls agents failed with again and sort them into ok,
    /// hinted and silent; the cases go to --out, one JSON line each.
    Replay(ReplayArgs),
    /// How the cases of replay file A fared in replay file B.
    ReplayDiff { a: PathBuf, b: PathBuf },
}

#[derive(Parser)]
struct ReplayArgs {
    /// The suites the streams came from; each names its tool (`[suite] tool`).
    #[arg(required = true)]
    suites: Vec<PathBuf>,
    /// Directories searched for `<suite>.<group>.<task>.<rep>.stream.jsonl`.
    #[arg(long = "streams", required_unless_present = "cases")]
    streams: Vec<PathBuf>,
    /// Replay the cases of an earlier replay file instead of reading streams.
    #[arg(long, conflicts_with = "streams")]
    cases: Option<PathBuf>,
    /// Also write the cases that did not come out ok here: run against the
    /// build that made the streams, that is the set of calls that really
    /// failed (a call that ran several commands names them all).
    #[arg(long)]
    failing: Option<PathBuf>,
    /// Put these directories in front of `PATH` instead of the suite's
    /// `path` (replay an older or newer build).
    #[arg(long = "bin")]
    bin: Vec<PathBuf>,
    /// Seconds one replayed call may take.
    #[arg(long, default_value_t = 180)]
    timeout: u64,
    /// Where the cases go, with what each came to.
    #[arg(long, default_value = "target/eval-data/replay/replay.jsonl")]
    out: PathBuf,
    /// Only list the cases found; run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print the cases as one JSON object instead of the tables.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct RunArgs {
    /// One or more suite files; their runs share one schedule.
    #[arg(required = true)]
    suites: Vec<PathBuf>,
    #[arg(long = "task")]
    tasks: Vec<String>,
    #[arg(long = "group")]
    groups: Vec<String>,
    #[arg(long, default_value_t = 1)]
    reps: u32,
    /// Sessions running at once, in total.
    #[arg(long, default_value_t = 1)]
    jobs: usize,
    /// Sessions running at once per harness.
    #[arg(long, default_value_t = 4)]
    per_harness: usize,
    /// Do not start a run while available memory is below this many MiB.
    #[arg(long, default_value_t = 1536)]
    min_free_mib: u64,
    /// Rate-limit retries per run; the harness backs off 30 s × attempt.
    #[arg(long, default_value_t = 4)]
    retries: u32,
    /// Where results go; each suite writes under `<out>/<suite name>`.
    #[arg(long, default_value = "target/eval-data")]
    out: PathBuf,
    #[arg(long)]
    price: Option<PathBuf>,
}

/// `print!` that ends the process quietly when stdout is a closed pipe
/// (`axkit-eval check | head`) instead of panicking.
macro_rules! out {
    ($($t:tt)*) => { emit(&format!($($t)*)) };
}

macro_rules! outln {
    ($($t:tt)*) => { emit(&format!("{}\n", format_args!($($t)*))) };
}

fn emit(text: &str) {
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush()) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("Error: writing stdout: {e}");
        std::process::exit(1);
    }
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Run(args) => cmd_run(&args),
        Cmd::Summary { results, by, verbose } => {
            let mut runs = Vec::new();
            for path in &results {
                runs.extend(load_runs(path)?);
            }
            if verbose {
                out!("{}", report::runs_table(&runs));
            }
            out!("{}", report::table(&report::summarize_by(&runs, &by)));
            Ok(())
        }
        Cmd::Compare { a, b, by } => {
            outln!("# a = {}; b = {}", a.display(), b.display());
            out!("{}", report::compare(&report::summarize_by(&load_runs(&a)?, &by), &report::summarize_by(&load_runs(&b)?, &by)));
            Ok(())
        }
        Cmd::Check { suites } => {
            for path in &suites {
                let suite = Suite::load(path)?;
                outln!("# suite {} | {} tasks | {} groups", suite.suite.name, suite.tasks.len(), suite.groups.len());
                for group in &suite.groups {
                    harness::by_name(&group.harness)?;
                    let skill = group.skill.as_ref().map_or("-", |s| s.command.as_str());
                    outln!(
                        "group | {} | {} | model {} | skill {skill}",
                        group.name,
                        group.harness,
                        group.model.as_deref().unwrap_or("default")
                    );
                }
            }
            Ok(())
        }
        Cmd::Replay(args) => cmd_replay(&args),
        Cmd::ReplayDiff { a, b } => {
            out!("{}", replay::diff(&replay::load(&a)?, &replay::load(&b)?));
            Ok(())
        }
        Cmd::Purge => {
            print_inventory("before");
            let report = purge();
            for line in &report.removed {
                eprintln!("removed | {line}");
            }
            for line in &report.warnings {
                eprintln!("warning | {line}");
            }
            print_inventory("after");
            Ok(())
        }
    }
}

const HARNESSES: [&str; 3] = ["claude", "codex", "grok"];

fn print_inventory(when: &str) {
    outln!("# inventory {when} | store | entries | eval-made");
    for name in HARNESSES {
        if let Ok(h) = harness::by_name(name) {
            for (store, total, eval) in h.inventory() {
                outln!("{when} | {store} | {total} | {eval}");
            }
        }
    }
}

/// The names of every history entry, to prove afterwards that nothing which
/// was not eval-made disappeared.
fn entry_names() -> BTreeSet<String> {
    let home = cleanup::home();
    let mut names = BTreeSet::new();
    for dir in [".claude/projects", ".grok/sessions"] {
        for entry in fs::read_dir(home.join(dir)).into_iter().flatten().flatten() {
            names.insert(format!("{dir}/{}", entry.file_name().to_string_lossy()));
        }
    }
    for path in cleanup::find_named(&home.join(".codex/sessions"), "rollout-", 4) {
        names.insert(path.display().to_string());
    }
    let trusted = fs::read_to_string(home.join(".grok/trusted_folders.toml")).unwrap_or_default();
    for line in trusted.lines().filter(|l| l.trim_start().starts_with("[folders.")) {
        names.insert(format!("trusted {}", line.trim()));
    }
    names
}

fn is_eval_entry(name: &str) -> bool {
    name.contains(EVAL_MARK) || name.contains(&EVAL_MARK.replace('/', "%2F")) || name.contains(&EVAL_MARK.replace(['/', '.', '_'], "-"))
}

/// Remove every eval-made history entry, by its eval-only path marker.
/// grok can write a session directory after its process has exited, too
/// late for that run's own cleanup: remove the ones this eval's runs made.
/// Only this eval's: another eval may be running.
fn sweep_late(runs: &[Run]) -> Report {
    let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).filter(|id| !id.is_empty()).collect();
    let mut report = Report::default();
    let dir = cleanup::home().join(".grok/sessions");
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_eval_entry(&name) && ids.iter().any(|id| name.contains(id)) {
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => report.removed.push(entry.path().display().to_string()),
                Err(e) => report.warnings.push(format!("removing {}: {e}", entry.path().display())),
            }
        }
    }
    report
}

fn purge() -> Report {
    let home = cleanup::home();
    let mut report = Report::default();
    for dir in [home.join(".claude/projects"), home.join(".grok/sessions")] {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_eval_entry(&name) {
                match fs::remove_dir_all(entry.path()) {
                    Ok(()) => report.removed.push(entry.path().display().to_string()),
                    Err(e) => report.warnings.push(format!("removing {}: {e}", entry.path().display())),
                }
            }
        }
    }
    // A running eval's grok launch may be writing the file; take turns with it.
    match cleanup::FileLock::acquire(&cleanup::grok_trust_lock()) {
        Ok(_held) => cleanup::drop_trusted_folders(&home.join(".grok/trusted_folders.toml"), |p| p.contains(EVAL_MARK), &mut report),
        Err(e) => report.warnings.push(format!("locking grok's trusted folders: {e}; left in place")),
    }
    cleanup::drop_lines(&home.join(".claude/history.jsonl"), |l| l.contains(EVAL_MARK), &mut report);
    for file in ["history.jsonl", "session_index.jsonl"] {
        cleanup::drop_lines(&home.join(".codex").join(file), |l| l.contains(EVAL_MARK), &mut report);
    }
    report
}

/// One queued run.
struct Job<'a> {
    suite: &'a Suite,
    group: &'a Group,
    task: &'a Task,
    rep: u32,
    attempts: u32,
    out: PathBuf,
}

impl Job<'_> {
    /// Runs of one lane never overlap: no run can read another's in-progress
    /// history. Harnesses keep separate stores, so they get separate lanes.
    fn lane(&self) -> (String, String, String) {
        (self.group.harness.clone(), self.suite.suite.name.clone(), self.task.id.clone())
    }
}

/// What the workers share: the queue, which lanes and how many sessions per
/// harness are busy, and each harness's rate-limit cooldown.
#[derive(Default)]
struct Board<'a> {
    pending: Vec<Job<'a>>,
    lanes: BTreeSet<(String, String, String)>,
    per_harness: BTreeMap<String, usize>,
    cooldown: BTreeMap<String, Instant>,
    running: usize,
}

impl<'a> Board<'a> {
    /// The first queued job that may start now, or `None`.
    fn take(&mut self, limit: usize) -> Option<Job<'a>> {
        let now = Instant::now();
        let i = self.pending.iter().position(|j| {
            let h = &j.group.harness;
            !self.lanes.contains(&j.lane())
                && self.per_harness.get(h).copied().unwrap_or(0) < limit
                && self.cooldown.get(h).is_none_or(|until| *until <= now)
        })?;
        let job = self.pending.remove(i);
        self.lanes.insert(job.lane());
        *self.per_harness.entry(job.group.harness.clone()).or_default() += 1;
        self.running += 1;
        Some(job)
    }

    fn release(&mut self, job: &Job<'_>) {
        self.lanes.remove(&job.lane());
        if let Some(n) = self.per_harness.get_mut(&job.group.harness) {
            *n -= 1;
        }
        self.running -= 1;
    }
}

/// Interleave the jobs so lanes, groups and harnesses mix in the queue: rep
/// first (every lane gets its first run before any second), then a fixed
/// pseudo-random order, so a run is reproducible.
fn interleave(jobs: &mut [Job<'_>]) {
    let hash = |j: &Job<'_>| {
        let key = format!("{}|{}|{}|{}", j.suite.suite.name, j.group.name, j.task.id, j.group.harness);
        key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    };
    jobs.sort_by_key(|j| (j.rep, hash(j)));
}

fn cmd_run(args: &RunArgs) -> Result<()> {
    let suites: Vec<Suite> = args.suites.iter().map(|p| Suite::load(p)).collect::<Result<_>>()?;
    let prices = match &args.price {
        Some(path) => Prices::with_overrides(path)?,
        None => Prices::builtin(),
    };
    fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let out = fs::canonicalize(&args.out)?;

    let mut jobs = Vec::new();
    for suite in &suites {
        let tasks = Suite::select(&suite.tasks, &args.tasks, |t| &t.id)?;
        let groups = Suite::select(&suite.groups, &args.groups, |g| &g.name)?;
        for group in &groups {
            harness::by_name(&group.harness)?;
        }
        runner::setup(suite)?;
        let suite_out = out.join(&suite.suite.name);
        fs::create_dir_all(&suite_out)?;
        for group in &groups {
            for task in &tasks {
                for rep in 0..args.reps {
                    jobs.push(Job { suite, group, task, rep, attempts: 0, out: suite_out.clone() });
                }
            }
        }
    }
    interleave(&mut jobs);
    let total = jobs.len();

    print_inventory("before");
    let names_before = entry_names();
    if memory_gate_unavailable() {
        eprintln!("note: this platform's available memory cannot be read; the --min-free-mib check is skipped");
    }

    let results_path = out.join("results.jsonl");
    let results = Mutex::new(fs::OpenOptions::new().create(true).append(true).open(&results_path)?);
    let board = Mutex::new(Board { pending: jobs, ..Board::default() });
    let all: Mutex<Vec<Run>> = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let launch = |h: &dyn Harness, s: &harness::Session<'_>| runner::launch_real(h, s);
    let min_free = args.min_free_mib * 1024 * 1024;

    std::thread::scope(|scope| {
        for _ in 0..args.jobs.max(1) {
            scope.spawn(|| {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let job = {
                        let mut b = board.lock().expect("board mutex poisoned");
                        if b.pending.is_empty() {
                            break;
                        }
                        // Check memory under the lock so two workers cannot both
                        // pass the gate for the last free gigabyte.
                        if axkit_eval::memory::available().is_some_and(|m| m < min_free) { None } else { b.take(args.per_harness.max(1)) }
                    };
                    let Some(mut job) = job else {
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    };
                    job.attempts += 1;
                    let h = harness::by_name(&job.group.harness).expect("checked before the run");
                    let plan = Plan {
                        suite: job.suite,
                        group: job.group,
                        task: job.task,
                        rep: job.rep,
                        out: &job.out,
                        prices: &prices,
                        launch: &launch,
                    };
                    let outcome = runner::run_one(&plan, h);
                    let mut b = board.lock().expect("board mutex poisoned");
                    b.release(&job);
                    match outcome {
                        Ok(o) if o.rate_limited && job.attempts <= args.retries => {
                            // Back off this harness only; the others keep running.
                            let wait = Duration::from_secs(30 * u64::from(job.attempts));
                            b.cooldown.insert(job.group.harness.clone(), Instant::now() + wait);
                            eprintln!("rate limited | {} | retry in {}s", plan.stem(), wait.as_secs());
                            b.pending.insert(0, job);
                        }
                        Ok(o) => {
                            drop(b);
                            let mut record = o.run;
                            record.attempts = job.attempts;
                            log_run(&record, all.lock().expect("results mutex poisoned").len() + 1, total);
                            if record.metrics.auth_failed {
                                // No other session may try the same credentials.
                                stop.store(true, Ordering::SeqCst);
                            }
                            let line = serde_json::to_string(&record).expect("a run serializes");
                            let mut f = results.lock().expect("results file mutex poisoned");
                            let _ = writeln!(f, "{line}").and_then(|()| f.flush());
                            all.lock().expect("results mutex poisoned").push(record);
                        }
                        Err(e) => {
                            stop.store(true, Ordering::SeqCst);
                            failure.lock().expect("failure mutex poisoned").get_or_insert(e);
                        }
                    }
                }
            });
        }
    });

    let all = all.into_inner().expect("results mutex poisoned");
    out!("{}", report::runs_table(&all));
    out!("{}", report::table(&report::summarize(&all)));
    let late = sweep_late(&all);
    if !late.removed.is_empty() || !late.warnings.is_empty() {
        outln!("# late entries swept | removed {} | warnings {}", late.removed.len(), late.warnings.join("; "));
    }
    print_inventory("after");
    verify_history(&names_before, &entry_names(), &all);
    eprintln!("# results: {}", results_path.display());
    if let Some(e) = failure.into_inner().expect("failure mutex poisoned") {
        return Err(e);
    }
    if all.iter().any(|r| r.metrics.auth_failed) {
        bail!("a harness failed to authenticate; the eval was stopped. Do not log in again from here: ask the user");
    }
    Ok(())
}

fn memory_gate_unavailable() -> bool {
    axkit_eval::memory::available().is_none()
}

fn log_run(r: &Run, done: usize, total: usize) {
    eprintln!(
        "[{done}/{total}] {} | {} | {} | {} | {:.0}s | ${} | cleaned {}{}{}",
        r.key(),
        r.task,
        r.rep,
        if r.passed { "pass" } else { "FAIL" },
        r.wall_s,
        r.cost_usd.map_or_else(|| "unknown".to_owned(), |c| format!("{c:.4}")),
        r.cleanup_removed,
        if r.cleanup_warnings.is_empty() { String::new() } else { format!(" | cleanup warnings: {}", r.cleanup_warnings.join("; ")) },
        r.reason.as_deref().map(|x| format!(" | {x}")).unwrap_or_default(),
    );
}

/// After a whole eval: no eval-made entry may remain, and no entry that
/// existed before may be gone. Entries the user's own sessions added in the
/// meantime are fine.
fn verify_history(before: &BTreeSet<String>, after: &BTreeSet<String>, runs: &[Run]) {
    let leaked: Vec<&String> =
        after.iter().filter(|n| is_eval_entry(n) || runs.iter().any(|r| !r.run_id.is_empty() && n.contains(&r.run_id))).collect();
    let lost: Vec<&String> = before.difference(after).filter(|n| !is_eval_entry(n)).collect();
    outln!(
        "# history check | entries before {} | after {} | eval leftovers {} | non-eval entries lost {}",
        before.len(),
        after.len(),
        leaked.len(),
        lost.len()
    );
    for n in leaked {
        outln!("leftover | {n}");
    }
    for n in lost {
        outln!("LOST | {n}");
    }
}

fn load_runs(path: &Path) -> Result<Vec<Run>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Run>(l).with_context(|| format!("parsing a run in {}", path.display())))
        .collect()
}

/// Gather the failed calls of every stream under `--streams` whose suite is
/// given, replay each suite's cases, save and print them.
fn cmd_replay(args: &ReplayArgs) -> Result<()> {
    let suites: Vec<Suite> = args.suites.iter().map(|p| Suite::load(p)).collect::<Result<_>>()?;
    let mut cases = match &args.cases {
        Some(path) => replay::load(path)?.into_iter().map(|c| Case { replayed: None, ..c }).collect(),
        None => cases_from_streams(&suites, &args.streams)?,
    };
    if !args.dry_run {
        let work = args.out.parent().unwrap_or_else(|| Path::new(".")).join("ws");
        for suite in &suites {
            let mut mine: Vec<Case> = cases.iter().filter(|c| c.suite == suite.suite.name).cloned().collect();
            eprintln!("# replaying {} calls of {}", mine.len(), suite.suite.name);
            replay::run(suite, &mut mine, &args.bin, &work, Duration::from_secs(args.timeout))?;
            for done in mine {
                if let Some(c) = cases.iter_mut().find(|c| c.suite == done.suite && c.command == done.command && c.error == done.error) {
                    *c = done;
                }
            }
        }
        replay::save(&args.out, &cases)?;
        eprintln!("# cases: {}", args.out.display());
        if let Some(path) = &args.failing {
            let failing: Vec<Case> = cases
                .iter()
                .filter(|c| c.replayed.as_ref().is_some_and(|r| matches!(r.class, replay::Class::Hinted | replay::Class::Silent)))
                .cloned()
                .collect();
            replay::save(path, &failing)?;
            eprintln!("# {} still failing: {}", failing.len(), path.display());
        }
    }
    if args.json {
        outln!("{}", serde_json::json!({ "cases": cases }));
    } else if args.dry_run {
        outln!("# suite | harnesses | seen | command | error");
        for c in &cases {
            outln!("{} | {} | {} | {} | {}", c.suite, c.harnesses.join(","), c.seen, c.command.replace('\n', "\\n"), c.error);
        }
    } else {
        out!("{}", replay::table(&cases));
    }
    Ok(())
}

/// The failed calls of every stream under `dirs` whose suite is given.
fn cases_from_streams(suites: &[Suite], dirs: &[PathBuf]) -> Result<Vec<Case>> {
    let mut found: Vec<Case> = Vec::new();
    let mut streams = Vec::new();
    for dir in dirs {
        collect_streams(dir, &mut streams)?;
    }
    streams.sort();
    let mut skipped = BTreeSet::new();
    for path in &streams {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let stem = name.trim_end_matches(".stream.jsonl");
        let mut parts = stem.splitn(3, '.');
        let (Some(suite_name), Some(group_name)) = (parts.next(), parts.next()) else { continue };
        let Some(suite) = suites.iter().find(|s| s.suite.name == suite_name) else {
            skipped.insert(format!("suite {suite_name}"));
            continue;
        };
        let tool = suite.suite.tool.as_deref().with_context(|| format!("suite {suite_name} names no `tool`; replay needs it"))?;
        let Some(group) = suite.groups.iter().find(|g| g.name == group_name) else {
            skipped.insert(format!("group {suite_name}/{group_name}"));
            continue;
        };
        let harness = harness::by_name(&group.harness)?;
        let stream = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        found.extend(replay::extract(suite_name, harness.name(), stem, tool, &harness.calls(&stream)));
    }
    for what in &skipped {
        eprintln!("note: skipped streams of {what}, which no given suite has");
    }
    let cases = replay::dedupe(found);
    eprintln!("# {} streams, {} distinct failed calls", streams.len(), cases.len());
    Ok(cases)
}

fn collect_streams(dir: &Path, into: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            // Workspaces hold no streams; skip their fixtures and databases.
            if path.file_name().is_some_and(|n| n != "ws") {
                collect_streams(&path, into)?;
            }
        } else if path.to_string_lossy().ends_with(".stream.jsonl") {
            into.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axkit_eval::suite::{Judge, Settings};

    fn group(name: &str, harness: &str) -> Group {
        Group {
            name: name.into(),
            harness: harness.into(),
            exe: None,
            model: None,
            skill: None,
            tools: vec![],
            permission_mode: None,
            allow: vec![],
            deny: vec![],
            sandbox: None,
            max_turns: None,
            args: vec![],
            env: BTreeMap::new(),
            mcp: None,
            tool: None,
            rules: String::new(),
            bare_path: false,
        }
    }

    #[test]
    fn a_lane_never_runs_twice_and_harnesses_are_capped() {
        let suite =
            Suite { suite: Settings { name: "idax".into(), ..Settings::default() }, tasks: vec![], groups: vec![], dir: PathBuf::new() };
        let task = Task { id: "password".into(), prompt: String::new(), files: vec![], timeout: 1, judge: Judge::default() };
        let other = Task {
            id: "xor".into(),
            ..Task { id: String::new(), prompt: String::new(), files: vec![], timeout: 1, judge: Judge::default() }
        };
        let (g1, g2, g3) = (group("claude", "claude"), group("claude-skill", "claude"), group("codex", "codex"));
        let job = |g, t, rep| Job { suite: &suite, group: g, task: t, rep, attempts: 0, out: PathBuf::new() };
        let mut b =
            Board { pending: vec![job(&g1, &task, 0), job(&g2, &task, 0), job(&g1, &other, 0), job(&g3, &task, 0)], ..Board::default() };

        let first = b.take(2).unwrap();
        assert_eq!((first.group.name.as_str(), first.task.id.as_str()), ("claude", "password"));
        // claude/idax/password is busy: the claude-skill password run must wait.
        let second = b.take(2).unwrap();
        assert_eq!((second.group.name.as_str(), second.task.id.as_str()), ("claude", "xor"));
        // claude is at its cap of 2; codex shares the task but has its own lane.
        let third = b.take(2).unwrap();
        assert_eq!(third.group.harness, "codex");
        assert!(b.take(2).is_none(), "the remaining claude job is blocked by lane and cap");
        b.release(&first);
        assert_eq!(b.take(2).map(|j| j.group.name.clone()).as_deref(), Some("claude-skill"));
    }

    #[test]
    fn a_cooling_harness_is_skipped_but_others_start() {
        let suite =
            Suite { suite: Settings { name: "bnx".into(), ..Settings::default() }, tasks: vec![], groups: vec![], dir: PathBuf::new() };
        let task = Task { id: "t".into(), prompt: String::new(), files: vec![], timeout: 1, judge: Judge::default() };
        let (grok, claude) = (group("grok", "grok"), group("claude", "claude"));
        let job = |g| Job { suite: &suite, group: g, task: &task, rep: 0, attempts: 0, out: PathBuf::new() };
        let mut b = Board { pending: vec![job(&grok), job(&claude)], ..Board::default() };
        b.cooldown.insert("grok".into(), Instant::now() + Duration::from_secs(60));
        assert_eq!(b.take(4).map(|j| j.group.harness.clone()).as_deref(), Some("claude"));
        assert!(b.take(4).is_none());
    }

    #[test]
    fn eval_entries_are_recognized_in_every_store_spelling() {
        assert!(is_eval_entry("trusted [folders.\"/h/VibRev/idax/target/eval-data/ws/x.r1\"]"));
        assert!(is_eval_entry(".grok/sessions/%2Fh%2Fidax%2Ftarget%2Feval-data%2Fws%2Fx"));
        assert!(is_eval_entry(".claude/projects/-h-idax-target-eval-data-ws-x-r1"));
        // The old smoke layout had a group dir between eval-data and ws.
        assert!(is_eval_entry(".grok/sessions/%2Fh%2Fidax%2Ftarget%2Feval-data%2Fsmoke-grok%2Fws%2Fsmoke-grok.password.0"));
        assert!(!is_eval_entry(".claude/projects/-home-ylarod-workspace-Projects-VibRev"));
    }
}
