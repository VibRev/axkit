//! Executing one (group, task, repeat): set up the workspace, run the agent,
//! read the stream, judge the result.

use crate::cleanup::FileLock;
use crate::harness::{self, Harness, Session};
use crate::judge;
use crate::price::Prices;
use crate::report::Run;
use crate::suite::{Group, Suite, Task, Vars};
use anyhow::{Context, Result};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What came back from one launch of the agent.
#[derive(Debug, Default)]
pub struct Launched {
    pub stream: String,
    pub stderr: String,
    /// Killed on the first line that showed an authentication failure.
    pub auth_failed: bool,
    pub timed_out: bool,
}

/// Starts an agent for a session. Injected so tests run without one.
pub type Launch<'a> = &'a (dyn Fn(&dyn Harness, &Session<'_>) -> Result<Launched> + Sync);

/// One run's inputs.
pub struct Plan<'a> {
    pub suite: &'a Suite,
    pub group: &'a Group,
    pub task: &'a Task,
    pub rep: u32,
    /// Where workspaces, streams and answers go.
    pub out: &'a Path,
    pub prices: &'a Prices,
    pub launch: Launch<'a>,
}

impl Plan<'_> {
    /// `<suite>.<group>.<task>.<rep>`: names the run's artifacts.
    #[must_use]
    pub fn stem(&self) -> String {
        format!("{}.{}.{}.{}", self.suite.suite.name, self.group.name, self.task.id, self.rep)
    }
}

/// A run, and whether it was refused for rate limiting (the scheduler then
/// retries it later instead of counting it).
pub struct Outcome {
    pub run: Run,
    pub rate_limited: bool,
}

/// Run one plan. Teardown and the harness-history cleanup always run; the
/// private tmp dir (tool runtime, daemon socket, cache, spill files) is
/// removed afterwards.
///
/// # Errors
///
/// The workspace could not be prepared, the agent could not be launched, or a
/// judge command could not run.
pub fn run_one(plan: &Plan<'_>, harness: &dyn Harness) -> Result<Outcome> {
    let stem = plan.stem();
    // A short path under /tmp (`cleanup::tmp_root`): unix socket paths are capped near 104 bytes,
    // and harness sandboxes let tools write there. Its random suffix is the
    // run id, which the workspace path carries so history keyed by the
    // workspace can be told apart from every other session's.
    let tmp = tempfile::Builder::new().prefix("axe-").tempdir_in(crate::cleanup::tmp_root()).context("creating the run's tmp dir")?;
    let run_id = tmp.path().file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("axe-")).context("tmp dir name")?.to_owned();
    let workspace = plan.out.join("ws").join(format!("{stem}.{run_id}"));
    fs::create_dir_all(&workspace).with_context(|| format!("creating {}", workspace.display()))?;

    let vars = Vars::default()
        .with("ws", workspace.to_string_lossy().into_owned())
        .with("tmp", tmp.path().to_string_lossy().into_owned())
        .with("suite", plan.suite.dir.to_string_lossy().into_owned());
    let vars = vars.clone().with("tool", vars.expand(plan.group.tool.as_deref().unwrap_or(&plan.suite.suite.tool_phrase)));
    // The judge always finds the tool under test; a bare group's agent does not.
    let mut judge_env = harness::merge_env(&plan.suite.suite.env, plan.group, |v| vars.expand(v));
    let mut env = judge_env.clone();
    prepend_path(&mut judge_env, plan.suite);
    if !plan.group.bare_path {
        prepend_path(&mut env, plan.suite);
    }
    let mcp = plan.group.mcp.as_ref().map(|m| m.expanded(|v| vars.expand(v)));

    for file in &plan.task.files {
        let src = plan.suite.resolve(file);
        let name = Path::new(file).file_name().context("task file has no name")?;
        fs::copy(&src, workspace.join(name)).with_context(|| format!("copying fixture {}", src.display()))?;
    }

    let mut prompt = vars.expand(&plan.task.prompt);
    for rules in [&plan.suite.suite.rules, &plan.group.rules] {
        if !rules.trim().is_empty() {
            prompt = format!("{}\n\n{}", prompt.trim_end(), vars.expand(rules.trim()));
        }
    }
    let session = Session {
        group: plan.group,
        workspace: &workspace,
        run_id: &run_id,
        tmp: tmp.path(),
        prompt: &prompt,
        timeout: plan.task.timeout,
        env: &env,
        mcp: mcp.as_ref(),
    };
    if let Some(skill) = &plan.group.skill {
        install_skill(harness, &session, &skill.command, &skill.name)?;
    }
    harness.prepare(&session)?;

    let started = Instant::now();
    let launched = (plan.launch)(harness, &session);
    let wall_s = started.elapsed().as_secs_f64();

    let stream = launched.as_ref().map(|l| l.stream.as_str()).unwrap_or_default();
    let session_id = harness.session_id(stream);
    let judged = launched.as_ref().ok().map(|launched| -> Result<Outcome> {
        fs::write(plan.out.join(format!("{stem}.stream.jsonl")), &launched.stream)?;
        fs::write(plan.out.join(format!("{stem}.stderr.txt")), &launched.stderr)?;
        let parsed = harness.parse(&launched.stream);
        let answer_file = plan.out.join(format!("{stem}.answer.txt"));
        fs::write(&answer_file, &parsed.answer)?;

        let mut metrics = parsed.metrics;
        metrics.auth_failed |= launched.auth_failed;
        if launched.timed_out {
            metrics.fail(format!("killed after {}s", plan.task.timeout));
        }
        let rate_limited = !metrics.auth_failed && harness.rate_limited(&launched.stream) && !metrics.completed;
        let model = metrics.model.clone().or_else(|| plan.group.model.clone());
        let cost = model.as_deref().and_then(|m| plan.prices.get(m)).map(|p| p.cost(metrics.tokens));
        let mut record = Run::new(plan.group, &plan.suite.suite.name, &plan.task.id, plan.rep, wall_s, model, cost, metrics);
        record.run_id.clone_from(&run_id);
        record.session_id.clone_from(&session_id);

        if record.metrics.auth_failed {
            record.reason = Some("authentication failed; eval stopped".to_owned());
        } else if !record.metrics.completed {
            record.reason = Some(record.metrics.error.clone().unwrap_or_else(|| "the session did not complete".to_owned()));
        } else {
            // Judged before teardown: a judge command may reopen the target
            // with the run's own daemon and runtime directory.
            let mut judge = plan.task.judge.clone();
            judge.command = judge.command.iter().map(|a| vars.expand(a)).collect();
            match judge::verdict(&judge, &parsed.answer, &workspace, &answer_file, &judge_env)? {
                Ok(()) => record.passed = true,
                Err(reason) => record.reason = Some(reason),
            }
        }
        Ok(Outcome { run: record, rate_limited })
    });

    for line in &plan.suite.suite.teardown {
        // Best effort: a daemon that is already gone is fine.
        let _ = sh(line, &workspace, &judge_env);
    }
    let cleaned = harness.cleanup(&session, session_id.as_deref());
    drop(tmp);

    launched?;
    let mut outcome = judged.context("a launched run is judged")??;
    outcome.run.cleanup_removed = cleaned.removed.len();
    outcome.run.cleanup_warnings = cleaned.warnings;
    Ok(outcome)
}

/// Write the stdout of `command` (run with the session's environment, so the
/// tool under test is on `PATH`) where the harness looks for skill `name`.
fn install_skill(harness: &dyn Harness, session: &Session<'_>, command: &str, name: &str) -> Result<PathBuf> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(session.workspace)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .output()
        .with_context(|| format!("running skill command {command:?}"))?;
    anyhow::ensure!(output.status.success(), "skill command {command:?} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    anyhow::ensure!(!output.stdout.is_empty(), "skill command {command:?} printed nothing");
    let path = harness.skill_file(session, name);
    fs::create_dir_all(path.parent().context("skill path has a parent")?)?;
    fs::write(&path, &output.stdout).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// How long a launch may hold its harness's startup lock.
const STARTUP_CAP: Duration = Duration::from_secs(15);

/// Launch the real agent. Stdout is read line by line; the first line that
/// shows an authentication failure kills the process at once, before the
/// harness can retry or refresh anything.
///
/// # Errors
///
/// The process could not be spawned.
pub fn launch_real(harness: &dyn Harness, session: &Session<'_>) -> Result<Launched> {
    let mut cmd = harness.command(session);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut startup = match harness.startup_lock() {
        Some(path) => Some(FileLock::acquire(&path).with_context(|| format!("locking {}", path.display()))?),
        None => None,
    };
    let settle_by = Instant::now() + STARTUP_CAP;
    let mut child = cmd.spawn().with_context(|| format!("spawning {program}; is it on PATH?"))?;
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let mut stderr = child.stderr.take().context("no stderr pipe")?;

    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });

    let deadline = Instant::now() + Duration::from_secs(session.timeout);
    let mut out = Launched::default();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if harness.auth_failure(&line) {
                    out.auth_failed = true;
                    let _ = child.kill();
                }
                out.stream.push_str(&line);
                out.stream.push('\n');
                if out.auth_failed {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if startup.is_some() && (Instant::now() >= settle_by || harness.started(session)) {
            startup = None;
        }
        if Instant::now() >= deadline {
            out.timed_out = true;
            let _ = child.kill();
            break;
        }
    }
    let _ = child.wait();
    let _ = reader.join();
    out.stderr = err_reader.join().unwrap_or_default();
    Ok(out)
}

/// Put the suite's `path` directories (resolved against the suite) in front
/// of the inherited `PATH`.
fn prepend_path(env: &mut Vec<(String, String)>, suite: &Suite) {
    let dirs: Vec<PathBuf> = suite.suite.path.iter().map(|p| suite.resolve(p)).collect();
    prepend_dirs(env, &dirs);
}

/// Put `dirs` in front of the inherited `PATH`.
pub(crate) fn prepend_dirs(env: &mut Vec<(String, String)>, dirs: &[PathBuf]) {
    if dirs.is_empty() {
        return;
    }
    let dirs: Vec<String> = dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let full = if inherited.is_empty() { dirs.join(":") } else { format!("{}:{inherited}", dirs.join(":")) };
    env.retain(|(k, _)| k != "PATH");
    env.push(("PATH".to_owned(), full));
}

pub(crate) fn sh(line: &str, dir: &Path, env: &[(String, String)]) -> Result<std::process::ExitStatus> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        // stdout carries the tables; what setup and teardown print is a log.
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("running {line:?}"))
}

/// Run the suite's one-time `setup` lines in the suite directory.
///
/// # Errors
///
/// A setup line fails.
pub fn setup(suite: &Suite) -> Result<()> {
    for line in &suite.suite.setup {
        let status = sh(line, &suite.dir, &[])?;
        anyhow::ensure!(status.success(), "setup step failed: {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{Judge, Settings, SkillSource};
    use std::collections::BTreeMap;

    const PASS: &str = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-haiku-5-5\",\"session_id\":\"sess-0001\"}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"it is s3cret\"}]}}\n",
        "{\"type\":\"result\",\"num_turns\":1,\"is_error\":false,\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}\n",
    );

    fn suite(dir: &Path, judge: Judge, files: Vec<String>, skill: Option<SkillSource>) -> Suite {
        Suite {
            suite: Settings {
                name: "t".into(),
                path: vec!["bin".into()],
                rules: "Do not copy license files.".into(),
                ..Settings::default()
            },
            tasks: vec![Task { id: "task".into(), prompt: "find it in {ws}".into(), files, timeout: 5, judge }],
            groups: vec![Group {
                name: "g".into(),
                harness: "claude".into(),
                exe: None,
                model: None,
                skill,
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
            }],
            dir: dir.to_path_buf(),
        }
    }

    fn run(suite: &Suite, out: &Path, launch: Launch<'_>) -> Outcome {
        let prices = Prices::builtin();
        let plan = Plan { suite, group: &suite.groups[0], task: &suite.tasks[0], rep: 0, out, prices: &prices, launch };
        run_one(&plan, harness::by_name("claude").unwrap()).unwrap()
    }

    fn canned(stream: &'static str) -> impl Fn(&dyn Harness, &Session<'_>) -> Result<Launched> + Sync {
        move |_: &dyn Harness, _: &Session<'_>| Ok(Launched { stream: stream.to_owned(), ..Launched::default() })
    }

    #[test]
    fn a_passing_run_has_a_unique_workspace_a_project_skill_and_priced_tokens() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fix.bin"), b"data").unwrap();
        let skill = SkillSource { command: "printf -- '---\\nname: t\\n---\\nuse t'".into(), name: "t".into() };
        let s = suite(dir.path(), Judge { contains: vec!["s3cret".into()], ..Judge::default() }, vec!["fix.bin".into()], Some(skill));
        let launch = |_: &dyn Harness, s: &Session<'_>| {
            assert!(s.workspace.join("fix.bin").exists());
            assert!(s.workspace.to_string_lossy().ends_with(s.run_id) && s.run_id.len() >= 6, "the workspace carries the run id");
            assert!(s.prompt.contains(&*s.workspace.to_string_lossy()) && s.prompt.ends_with("Do not copy license files."));
            assert!(s.workspace.join(".claude/skills/t/SKILL.md").is_file(), "a project-level skill");
            assert!(s.tmp.starts_with(crate::cleanup::tmp_root()) && s.tmp.is_dir());
            assert!(s.env.iter().any(|(k, v)| k == "PATH" && v.contains("bin")));
            Ok(Launched { stream: PASS.to_owned(), ..Launched::default() })
        };
        let o = run(&s, &dir.path().join("out"), &launch);
        let r = &o.run;
        assert!(r.passed && !o.rate_limited, "{:?}", r.reason);
        assert_eq!((r.model.as_deref(), r.session_id.as_deref()), (Some("claude-haiku-5-5"), Some("sess-0001")));
        assert!((r.cost_usd.unwrap() - 0.10).abs() < 1e-9, "1M uncached haiku input tokens cost $0.10");
        assert!(dir.path().join("out/t.g.task.0.stream.jsonl").is_file());
        assert!(!crate::cleanup::tmp_root().join(format!("axe-{}", r.run_id)).exists(), "the tmp dir is gone");
    }

    #[test]
    fn a_wrong_answer_fails_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let s = suite(dir.path(), Judge { contains: vec!["hunter2".into()], ..Judge::default() }, vec![], None);
        let o = run(&s, &dir.path().join("out"), &canned(PASS));
        assert!(!o.run.passed);
        assert!(o.run.reason.unwrap().contains("hunter2"));
    }

    #[test]
    fn an_auth_failure_is_flagged_and_not_judged() {
        let dir = tempfile::tempdir().unwrap();
        let s = suite(dir.path(), Judge { contains: vec!["x".into()], ..Judge::default() }, vec![], None);
        let launch = |_: &dyn Harness, _: &Session<'_>| Ok(Launched { auth_failed: true, ..Launched::default() });
        let o = run(&s, &dir.path().join("out"), &launch);
        assert!(o.run.metrics.auth_failed && !o.run.passed && !o.rate_limited);
        assert!(o.run.reason.unwrap().contains("authentication"));
    }

    #[test]
    fn a_rate_limited_session_is_reported_for_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let s = suite(dir.path(), Judge { contains: vec!["x".into()], ..Judge::default() }, vec![], None);
        let o = run(&s, &dir.path().join("out"), &canned("{\"type\":\"system\",\"subtype\":\"api_retry\",\"error_status\":429}\n"));
        assert!(o.rate_limited && !o.run.passed);
    }

    #[test]
    fn an_unknown_model_has_no_cost() {
        let dir = tempfile::tempdir().unwrap();
        let s = suite(dir.path(), Judge { contains: vec!["s3cret".into()], ..Judge::default() }, vec![], None);
        let o = run(
            &s,
            &dir.path().join("out"),
            &canned(
                "{\"type\":\"system\",\"model\":\"gpt-6-astra\"}\n{\"type\":\"result\",\"result\":\"s3cret\",\"usage\":{\"input_tokens\":5}}\n",
            ),
        );
        assert!(o.run.passed);
        assert_eq!(o.run.cost_usd, None);
    }

    /// Starts `sh -c script`; started once `flag` exists.
    struct Slow {
        lock: PathBuf,
        flag: PathBuf,
        script: String,
    }

    impl Harness for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn default_exe(&self) -> &'static str {
            "sh"
        }
        fn command(&self, _: &Session<'_>) -> std::process::Command {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(&self.script);
            cmd
        }
        fn skill_file(&self, s: &Session<'_>, name: &str) -> PathBuf {
            s.workspace.join(name)
        }
        fn startup_lock(&self) -> Option<PathBuf> {
            Some(self.lock.clone())
        }
        fn started(&self, _: &Session<'_>) -> bool {
            self.flag.exists()
        }
        fn parse(&self, _: &str) -> harness::Parsed {
            harness::Parsed::default()
        }
        fn calls(&self, _: &str) -> Vec<harness::Call> {
            Vec::new()
        }
        fn auth_failure(&self, _: &str) -> bool {
            false
        }
        fn rate_limited(&self, _: &str) -> bool {
            false
        }
        fn session_id(&self, _: &str) -> Option<String> {
            None
        }
        fn cleanup(&self, _: &Session<'_>, _: Option<&str>) -> crate::cleanup::Report {
            crate::cleanup::Report::default()
        }
        fn inventory(&self) -> Vec<(String, usize, usize)> {
            Vec::new()
        }
    }

    #[test]
    fn the_startup_lock_is_held_until_the_harness_has_started_and_no_longer() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("trusted");
        let h = Slow {
            lock: dir.path().join("x.lock"),
            flag: flag.clone(),
            script: format!("sleep 0.5; touch '{}'; sleep 3; echo done", flag.display()),
        };
        let s = suite(dir.path(), Judge::default(), vec![], None);
        let session = Session {
            group: &s.groups[0],
            workspace: dir.path(),
            run_id: "r",
            tmp: dir.path(),
            prompt: "",
            timeout: 30,
            env: &[],
            mcp: None,
        };
        std::thread::scope(|scope| {
            let launch = scope.spawn(|| launch_real(&h, &session).unwrap());
            std::thread::sleep(Duration::from_millis(250));
            let probe = fs::File::open(&h.lock).unwrap();
            assert!(probe.try_lock().is_err(), "held while the harness starts");
            let begin = Instant::now();
            while probe.try_lock().is_err() {
                assert!(begin.elapsed() < Duration::from_secs(2), "released once started, not at exit");
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(!launch.is_finished(), "the session is still running");
            assert_eq!(launch.join().unwrap().stream, "done\n");
        });
    }
}
