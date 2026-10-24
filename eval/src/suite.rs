//! A task set: what to ask, which files to hand over, how to judge the answer,
//! and the groups (harness + model + settings) to ask it under.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub suite: Settings,
    #[serde(rename = "task")]
    pub tasks: Vec<Task>,
    #[serde(rename = "group")]
    pub groups: Vec<Group>,
    /// The directory the suite file is in; relative paths resolve against it.
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub name: String,
    /// Run once before any run, in the suite directory (builds fixtures).
    #[serde(default)]
    pub setup: Vec<String>,
    /// Directories put in front of `PATH` for the agent and the judge.
    #[serde(default)]
    pub path: Vec<String>,
    /// Environment of every run; values may use the template variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Run after each run with the run's environment (stops daemons).
    #[serde(default)]
    pub teardown: Vec<String>,
    /// Appended to every task's prompt (rules every run must follow).
    #[serde(default)]
    pub rules: String,
    /// The command under test, as agents call it; `replay` picks its calls.
    pub tool: Option<String>,
    /// What `{tool}` says in a prompt: how the agent is told to reach the
    /// tool under test. A group's `tool` overrides it.
    #[serde(default)]
    pub tool_phrase: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    /// Copied into the workspace under their file names.
    #[serde(default)]
    pub files: Vec<String>,
    /// Seconds for the whole agent session.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    pub judge: Judge,
}

fn default_timeout() -> u64 {
    900
}

/// How a run passes: every check that is given must hold.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judge {
    /// Strings the final answer must all contain.
    #[serde(default)]
    pub contains: Vec<String>,
    /// A regex the final answer must match.
    pub regex: Option<String>,
    /// Strings the final answer must not contain (a wrong candidate).
    #[serde(default)]
    pub excludes: Vec<String>,
    /// A command that exits 0 when the workspace is in the expected state;
    /// it runs in the workspace with `ANSWER_FILE` and the run's environment.
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    #[serde(default = "default_harness")]
    pub harness: String,
    /// The harness executable; the adapter's default when absent.
    pub exe: Option<String>,
    pub model: Option<String>,
    /// A skill to install into the isolated session, produced by a command.
    pub skill: Option<SkillSource>,
    /// Built-in tools the session has, in the harness's own names; empty
    /// means the harness default.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The harness's permission mode, where it has one (claude: `auto` when
    /// absent).
    pub permission_mode: Option<String>,
    /// Permission rules that run without asking, on top of the mode.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Permission rules that are always denied, on top of the adapter's own.
    #[serde(default)]
    pub deny: Vec<String>,
    /// The harness's OS sandbox profile, where it has one.
    pub sandbox: Option<String>,
    pub max_turns: Option<u32>,
    /// Extra arguments passed to the harness as they are.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// An MCP server the session is given instead of the suite's command.
    pub mcp: Option<McpServer>,
    /// `{tool}` in prompts for this group; the suite's `tool_phrase` when absent.
    pub tool: Option<String>,
    /// Appended to the suite's rules for this group.
    #[serde(default)]
    pub rules: String,
    /// Keep the suite's `path` off the agent's `PATH` (the judge still has it),
    /// so a group given an MCP server does not also find the CLI.
    #[serde(default)]
    pub bare_path: bool,
}

/// A stdio MCP server; strings may use the template variables.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// The server's name, as tool names show it (`mcp__<name>__...`).
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl McpServer {
    /// Every string templated with `expand`.
    #[must_use]
    pub fn expanded(&self, expand: impl Fn(&str) -> String) -> McpServer {
        McpServer {
            name: self.name.clone(),
            command: expand(&self.command),
            args: self.args.iter().map(|a| expand(a)).collect(),
            env: self.env.iter().map(|(k, v)| (k.clone(), expand(v))).collect(),
        }
    }
}

/// Where a skill comes from: a command whose stdout is the `SKILL.md`, run
/// with the run's environment (so the tool under test is on `PATH`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSource {
    pub command: String,
    /// The skill's directory name in the harness's skill root.
    pub name: String,
}

fn default_harness() -> String {
    "claude".to_owned()
}

impl Suite {
    /// # Errors
    ///
    /// The file cannot be read, is not a valid suite, or names a task or
    /// group twice.
    pub fn load(path: &Path) -> Result<Suite> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut suite: Suite = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Absolute, so setup and fixture paths work from any directory (a bare
        // `suite.toml` has an empty parent).
        let path = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
        suite.dir = path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        suite.validate()?;
        Ok(suite)
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.tasks.is_empty(), "the suite has no [[task]]");
        ensure!(!self.groups.is_empty(), "the suite has no [[group]]");
        let mut seen = std::collections::BTreeSet::new();
        for task in &self.tasks {
            ensure!(seen.insert(&task.id), "task {:?} appears twice", task.id);
            task.judge.validate().with_context(|| format!("task {:?}", task.id))?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for group in &self.groups {
            ensure!(seen.insert(&group.name), "group {:?} appears twice", group.name);
        }
        Ok(())
    }

    /// A path from the suite file, relative to the suite directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let path = expand_home(path);
        if path.is_absolute() { path } else { self.dir.join(path) }
    }

    /// The tasks and groups named, or all of them when `names` is empty.
    ///
    /// # Errors
    ///
    /// A name matches nothing.
    pub fn select<'a, T>(items: &'a [T], names: &[String], name: impl Fn(&T) -> &str) -> Result<Vec<&'a T>> {
        if names.is_empty() {
            return Ok(items.iter().collect());
        }
        names
            .iter()
            .map(|n| {
                items.iter().find(|i| name(i) == n).with_context(|| {
                    let known: Vec<&str> = items.iter().map(&name).collect();
                    format!("no {n:?}; known: {}", known.join(", "))
                })
            })
            .collect()
    }
}

impl Judge {
    fn validate(&self) -> Result<()> {
        if self.contains.is_empty() && self.regex.is_none() && self.command.is_empty() {
            bail!("the judge checks nothing: give contains, regex or command");
        }
        if let Some(re) = &self.regex {
            regex::Regex::new(re).with_context(|| format!("bad judge regex {re:?}"))?;
        }
        Ok(())
    }
}

/// `~/x` as a path under `$HOME`.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => crate::cleanup::home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Values a template may refer to as `{name}`.
#[derive(Clone, Debug, Default)]
pub struct Vars(BTreeMap<&'static str, String>);

impl Vars {
    #[must_use]
    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Vars {
        self.0.insert(name, value.into());
        self
    }

    /// `text` with every `{name}` of a known variable replaced, and every
    /// `{env:NAME}` by that variable of the runner's environment (empty when
    /// unset); other braces are left alone, so prompts can still hold code.
    pub fn expand(&self, text: &str) -> String {
        let text = self.0.iter().fold(text.to_owned(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value));
        expand_env(&text)
    }
}

/// `{env:NAME}` replaced by `$NAME`, so a suite can name machine-specific
/// paths (an MCP server's install) without writing them into the file.
fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{env:") {
        let Some(len) = rest[start..].find('}') else { break };
        out.push_str(&rest[..start]);
        out.push_str(&std::env::var(&rest[start + 5..start + len]).unwrap_or_default());
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUITE: &str = r#"
[suite]
name = "demo"
setup = ["make"]
[suite.env]
TOOL_RUNTIME_DIR = "{tmp}/rt"

[[task]]
id = "password"
prompt = "Find the password in {ws}/crackme. Code like {x} stays."
files = ["fixtures/crackme"]
[task.judge]
contains = ["s3cret"]

[[group]]
name = "base"
model = "haiku"
allow = ["Bash(tool *)"]
"#;

    fn load(text: &str) -> Result<Suite> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suite.toml");
        fs::write(&path, text).unwrap();
        Suite::load(&path)
    }

    #[test]
    fn a_suite_loads_with_defaults() {
        let suite = load(SUITE).unwrap();
        let group = &suite.groups[0];
        assert_eq!(group.harness, "claude");
        assert!(group.tools.is_empty() && group.skill.is_none());
        assert_eq!(suite.tasks[0].timeout, 900);
        assert!(suite.resolve("fixtures/crackme").ends_with("fixtures/crackme"));
    }

    #[test]
    fn templates_replace_known_names_only() {
        let vars = Vars::default().with("ws", "/w");
        assert_eq!(vars.expand("{ws}/crackme {x}"), "/w/crackme {x}");
        let path = std::env::var("PATH").unwrap();
        assert_eq!(vars.expand("{env:PATH}/a {env:AXEVAL_SURELY_UNSET} {env:"), format!("{path}/a  {{env:"));
    }

    #[test]
    fn a_judge_must_check_something_and_names_must_be_unique() {
        let empty = SUITE.replace("contains = [\"s3cret\"]", "");
        assert!(load(&empty).unwrap_err().to_string().contains("password"));
        let twice = format!("{SUITE}\n[[group]]\nname = \"base\"\n");
        assert!(format!("{:#}", load(&twice).unwrap_err()).contains("twice"));
        let bad = SUITE.replace("contains = [\"s3cret\"]", "regex = \"(\"");
        assert!(format!("{:#}", load(&bad).unwrap_err()).contains("regex"));
    }

    #[test]
    fn selecting_an_unknown_name_lists_the_known_ones() {
        let suite = load(SUITE).unwrap();
        let err = Suite::select(&suite.tasks, &["pasword".to_owned()], |t| &t.id).unwrap_err();
        assert!(err.to_string().contains("known: password"), "{err}");
        assert_eq!(Suite::select(&suite.tasks, &[], |t| &t.id).unwrap().len(), 1);
    }
}
