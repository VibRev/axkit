//! One run's record, and the per-group aggregation `summary` prints.

use crate::metrics::Metrics;
use crate::suite::Group;
use serde::{Deserialize, Serialize};

/// The outcome of one (group, task, repeat) run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    /// The suite (engine) the run belongs to.
    #[serde(default)]
    pub suite: String,
    pub group: String,
    pub harness: String,
    pub task: String,
    pub rep: u32,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub wall_s: f64,
    /// Launches it took (rate-limit retries + 1).
    pub attempts: u32,
    pub model: Option<String>,
    /// Recomputed from tokens with the price table; `None` when the model's
    /// price is unknown.
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// History entries removed after the run.
    #[serde(default)]
    pub cleanup_removed: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_warnings: Vec<String>,
    #[serde(flatten)]
    pub metrics: Metrics,
}

impl Run {
    #[must_use]
    #[allow(clippy::too_many_arguments)] // a record constructor: every field is distinct and named at the one call site
    pub fn new(
        group: &Group,
        suite: &str,
        task: &str,
        rep: u32,
        wall_s: f64,
        model: Option<String>,
        cost_usd: Option<f64>,
        metrics: Metrics,
    ) -> Run {
        Run {
            suite: suite.to_owned(),
            group: group.name.clone(),
            harness: group.harness.clone(),
            task: task.to_owned(),
            rep,
            passed: false,
            reason: None,
            wall_s,
            attempts: 1,
            model,
            cost_usd,
            run_id: String::new(),
            session_id: None,
            cleanup_removed: 0,
            cleanup_warnings: Vec::new(),
            metrics,
        }
    }

    /// `suite/group`, the key runs are summarized under.
    #[must_use]
    pub fn key(&self) -> String {
        if self.suite.is_empty() { self.group.clone() } else { format!("{}/{}", self.suite, self.group) }
    }
}

/// One group's numbers across its runs: a pass rate and medians.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSummary {
    pub group: String,
    pub runs: usize,
    pub passed: usize,
    pub turns: f64,
    pub tool_calls: f64,
    pub tool_errors: f64,
    pub truncations: f64,
    pub tool_kb: f64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Every token the session spent, `Tokens::total`.
    pub tokens: f64,
    /// `None` when no run in the group had a known price.
    pub cost_usd: Option<f64>,
    pub reported_cost_usd: Option<f64>,
    pub wall_s: f64,
}

impl GroupSummary {
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        if self.runs == 0 { 0.0 } else { self.passed as f64 / self.runs as f64 }
    }
}

/// A dimension runs can be grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// `suite/group`.
    Group,
    Harness,
    /// `skill` or `noskill`.
    Skill,
    /// The engine's suite.
    Suite,
    Task,
}

impl std::str::FromStr for By {
    type Err = String;

    fn from_str(s: &str) -> Result<By, String> {
        match s {
            "group" => Ok(By::Group),
            "harness" => Ok(By::Harness),
            "skill" => Ok(By::Skill),
            "suite" | "engine" => Ok(By::Suite),
            "task" => Ok(By::Task),
            other => Err(format!("unknown dimension {other:?}; use group, harness, skill, suite or task")),
        }
    }
}

impl By {
    /// `bare`: the suite is a dimension of its own, so a group is named
    /// without it.
    fn of(self, r: &Run, bare: bool) -> String {
        match self {
            By::Group if bare => r.group.clone(),
            By::Group => r.key(),
            By::Harness => r.harness.clone(),
            By::Skill => if r.group.ends_with("-skill") { "skill" } else { "noskill" }.to_owned(),
            By::Suite => r.suite.clone(),
            By::Task => r.task.clone(),
        }
    }
}

/// Group the runs by `suite/group` (first-seen order) and reduce each group.
#[must_use]
pub fn summarize(runs: &[Run]) -> Vec<GroupSummary> {
    summarize_by(runs, &[By::Group])
}

/// Group the runs by the joined values of `by` (first-seen order).
#[must_use]
pub fn summarize_by(runs: &[Run], by: &[By]) -> Vec<GroupSummary> {
    let bare = by.contains(&By::Suite);
    let key = |r: &Run| by.iter().map(|b| b.of(r, bare)).collect::<Vec<_>>().join("/");
    let mut order: Vec<String> = Vec::new();
    for run in runs {
        if !order.contains(&key(run)) {
            order.push(key(run));
        }
    }
    order
        .into_iter()
        .map(|group| {
            let rs: Vec<&Run> = runs.iter().filter(|r| key(r) == group).collect();
            let med = |f: &dyn Fn(&Run) -> f64| median(rs.iter().map(|r| f(r)).collect());
            let med_opt = |f: &dyn Fn(&Run) -> Option<f64>| {
                let xs: Vec<f64> = rs.iter().filter_map(|r| f(r)).collect();
                if xs.is_empty() { None } else { Some(median(xs)) }
            };
            let t = |r: &Run| r.metrics.tokens;
            GroupSummary {
                group,
                runs: rs.len(),
                passed: rs.iter().filter(|r| r.passed).count(),
                turns: med(&|r| f64::from(r.metrics.turns)),
                tool_calls: med(&|r| f64::from(r.metrics.tool_calls)),
                tool_errors: med(&|r| f64::from(r.metrics.tool_errors)),
                truncations: med(&|r| f64::from(r.metrics.truncations)),
                tool_kb: med(&|r| r.metrics.tool_output_bytes as f64 / 1024.0),
                input: med(&|r| t(r).input as f64),
                output: med(&|r| t(r).output as f64),
                cache_read: med(&|r| t(r).cache_read as f64),
                cache_write: med(&|r| (t(r).cache_write_5m + t(r).cache_write_1h) as f64),
                tokens: med(&|r| t(r).total() as f64),
                cost_usd: med_opt(&|r| r.cost_usd),
                reported_cost_usd: med_opt(&|r| r.metrics.reported_cost_usd),
                wall_s: med(&|r| r.wall_s),
            }
        })
        .collect()
}

fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(f64::total_cmp);
    let mid = xs.len() / 2;
    if xs.len() % 2 == 1 { xs[mid] } else { (xs[mid - 1] + xs[mid]) / 2.0 }
}

fn money(x: Option<f64>) -> String {
    x.map_or_else(|| "unknown".to_owned(), |c| format!("{c:.4}"))
}

/// The per-group table (medians), `#` header, ` | ` columns (axkit §3).
#[must_use]
pub fn table(summaries: &[GroupSummary]) -> String {
    let mut out = String::from(
        "# group | pass | turns | tools | errs | trunc | tool_kb | in | out | cache_r | cache_w | cost_usd | reported_usd | wall_s\n",
    );
    for s in summaries {
        out.push_str(&format!(
            "{} | {}/{} ({:.0}%) | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.0} | {:.0} | {:.0} | {:.0} | {} | {} | {:.0}\n",
            s.group,
            s.passed,
            s.runs,
            s.pass_rate() * 100.0,
            s.turns,
            s.tool_calls,
            s.tool_errors,
            s.truncations,
            s.tool_kb,
            s.input,
            s.output,
            s.cache_read,
            s.cache_write,
            money(s.cost_usd),
            money(s.reported_cost_usd),
            s.wall_s,
        ));
    }
    out
}

/// Two runs' summaries side by side, one row per key in either (A's order,
/// then keys only B has): pass, turns, total tokens, tool output and cost
/// of A then B, and B's change in tokens. A key one side lacks shows `-`.
#[must_use]
pub fn compare(a: &[GroupSummary], b: &[GroupSummary]) -> String {
    let mut out = String::from(
        "# key | pass a | pass b | turns a | turns b | tokens a | tokens b | tokens b/a | tool_kb a | tool_kb b | cost a | cost b | wall_s a | wall_s b\n",
    );
    let keys = a.iter().map(|s| &s.group).chain(b.iter().map(|s| &s.group).filter(|k| !a.iter().any(|s| &s.group == *k)));
    for key in keys {
        let (x, y) = (a.iter().find(|s| &s.group == key), b.iter().find(|s| &s.group == key));
        let col = |s: Option<&GroupSummary>, f: &dyn Fn(&GroupSummary) -> String| s.map_or_else(|| "-".to_owned(), f);
        let pass = |s: &GroupSummary| format!("{}/{}", s.passed, s.runs);
        let k = |v: f64| format!("{:.1}k", v / 1000.0);
        let ratio = match (x, y) {
            (Some(x), Some(y)) if x.tokens > 0.0 => format!("{:.2}", y.tokens / x.tokens),
            _ => "-".to_owned(),
        };
        out.push_str(&format!(
            "{key} | {} | {} | {} | {} | {} | {} | {ratio} | {} | {} | {} | {} | {} | {}\n",
            col(x, &pass),
            col(y, &pass),
            col(x, &|s| format!("{:.0}", s.turns)),
            col(y, &|s| format!("{:.0}", s.turns)),
            col(x, &|s| k(s.tokens)),
            col(y, &|s| k(s.tokens)),
            col(x, &|s| format!("{:.1}", s.tool_kb)),
            col(y, &|s| format!("{:.1}", s.tool_kb)),
            col(x, &|s| money(s.cost_usd)),
            col(y, &|s| money(s.cost_usd)),
            col(x, &|s| format!("{:.0}", s.wall_s)),
            col(y, &|s| format!("{:.0}", s.wall_s)),
        ));
    }
    out
}

/// One line per run, for the smoke report and `summary -v`.
#[must_use]
pub fn runs_table(runs: &[Run]) -> String {
    let mut out = String::from(
        "# suite/group | task | rep | pass | model | turns | tools | errs | trunc | in | out | cache_r | cache_w5m | cache_w1h | reasoning | cost_usd | reported_usd | wall_s | reason\n",
    );
    for r in runs {
        let t = r.metrics.tokens;
        out.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.0} | {}\n",
            r.key(),
            r.task,
            r.rep,
            if r.passed { "pass" } else { "FAIL" },
            r.model.as_deref().unwrap_or("-"),
            r.metrics.turns,
            r.metrics.tool_calls,
            r.metrics.tool_errors,
            r.metrics.truncations,
            t.input,
            t.output,
            t.cache_read,
            t.cache_write_5m,
            t.cache_write_1h,
            t.reasoning,
            money(r.cost_usd),
            money(r.metrics.reported_cost_usd),
            r.wall_s,
            r.reason.as_deref().unwrap_or("")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Tokens;

    #[test]
    fn a_run_record_reads_back() {
        let mut r = run("g", true, 3, Some(0.5));
        r.model = Some("claude-sonnet-5-5".into());
        r.metrics.model = r.model.clone();
        let line = serde_json::to_string(&r).unwrap();
        assert_eq!(line.matches("\"model\"").count(), 1, "{line}");
        let back: Run = serde_json::from_str(&line).unwrap();
        assert_eq!((back.model, back.metrics.turns), (Some("claude-sonnet-5-5".into()), 3));
    }

    fn run(group: &str, passed: bool, turns: u32, cost: Option<f64>) -> Run {
        let metrics = Metrics { turns, tokens: Tokens { input: 100, output: 20, ..Tokens::default() }, ..Metrics::default() };
        Run {
            suite: String::new(),
            group: group.into(),
            harness: "claude".into(),
            task: "t".into(),
            rep: 0,
            passed,
            reason: None,
            wall_s: 1.0,
            attempts: 1,
            model: None,
            cost_usd: cost,
            run_id: String::new(),
            session_id: None,
            cleanup_removed: 0,
            cleanup_warnings: Vec::new(),
            metrics,
        }
    }

    #[test]
    fn medians_and_pass_rate() {
        let runs = [run("a", true, 2, Some(0.1)), run("a", false, 4, Some(0.3)), run("a", true, 6, Some(0.2))];
        let s = &summarize(&runs)[0];
        assert_eq!((s.runs, s.passed), (3, 2));
        assert!((s.turns - 4.0).abs() < 1e-9);
        assert_eq!(s.cost_usd, Some(0.2));
        assert!((s.pass_rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn an_even_count_averages_the_middles_and_unknown_costs_stay_unknown() {
        let runs = [run("a", true, 2, None), run("a", true, 4, None)];
        let s = &summarize(&runs)[0];
        assert!((s.turns - 3.0).abs() < 1e-9);
        assert_eq!(s.cost_usd, None);
        assert!(table(std::slice::from_ref(s)).contains("| unknown |"));
    }

    #[test]
    fn runs_group_by_any_dimension() {
        let mut a = run("claude-skill", true, 1, None);
        a.harness = "claude".into();
        let mut b = run("codex", false, 1, None);
        b.harness = "codex".into();
        let by_skill: Vec<String> = summarize_by(&[a.clone(), b.clone()], &[By::Skill]).into_iter().map(|s| s.group).collect();
        assert_eq!(by_skill, ["skill", "noskill"]);
        let both: Vec<String> = summarize_by(&[a, b], &["harness".parse().unwrap(), By::Skill]).into_iter().map(|s| s.group).collect();
        assert_eq!(both, ["claude/skill", "codex/noskill"]);
        assert!("engine".parse::<By>().is_ok() && "x".parse::<By>().is_err());
    }

    #[test]
    fn a_group_beside_the_suite_does_not_repeat_it() {
        let mut r = run("claude", true, 1, None);
        r.suite = "idax-re".into();
        let key = |by: &[By]| summarize_by(std::slice::from_ref(&r), by).remove(0).group;
        assert_eq!(key(&[By::Suite, By::Group]), "idax-re/claude");
        assert_eq!(key(&[By::Group]), "idax-re/claude", "alone, the group still names its suite");
    }

    #[test]
    fn compare_aligns_keys_across_two_runs() {
        let tag = |mut r: Run, harness: &str, task: &str| {
            r.suite = "idax-re".into();
            r.harness = harness.into();
            r.task = task.into();
            r
        };
        let a = [tag(run("claude", true, 4, Some(0.04)), "claude", "password"), tag(run("codex", true, 9, None), "codex", "password")];
        let b = [tag(run("codex", false, 6, None), "codex", "password"), tag(run("claude-mcp", true, 3, Some(0.03)), "claude", "password")];
        let by = [By::Suite, By::Group, By::Task];
        let text = compare(&summarize_by(&a, &by), &summarize_by(&b, &by));
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 3, "{text}");
        assert!(rows[0].starts_with("idax-re/claude/password | 1/1 | - | 4 | - |"), "{text}");
        assert!(rows[1].starts_with("idax-re/codex/password | 1/1 | 0/1 | 9 | 6 | 0.1k | 0.1k | 1.00 |"), "{text}");
        assert!(rows[2].starts_with("idax-re/claude-mcp/password | - | 1/1 |"), "{text}");
    }

    #[test]
    fn groups_keep_first_seen_order() {
        let runs = [run("b", true, 1, None), run("a", true, 1, None), run("b", true, 1, None)];
        let names: Vec<String> = summarize(&runs).into_iter().map(|s| s.group).collect();
        assert_eq!(names, ["b", "a"]);
        assert!(runs_table(&runs).lines().count() == 4);
    }
}
