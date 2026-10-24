//! What one agent session spent and did, in harness-neutral terms. Each
//! harness adapter fills a `Metrics` from its own event stream.

use serde::{Deserialize, Serialize};

/// Token counts, each kind kept apart: the kinds are priced differently and
/// collapsing them hides that cache reads cost a tenth of input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Uncached input.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    /// Reasoning tokens, where the harness reports them apart. Informational:
    /// providers bill them as output, and `output` already includes them.
    pub reasoning: u64,
}

impl Tokens {
    /// Every billed token: input, output and both cache kinds.
    #[must_use]
    pub fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// The model the stream says served the session. Not written: a run
    /// record flattens these metrics beside its own `model`, which holds
    /// this or the group's, and two `model` keys make the record unreadable.
    #[serde(skip)]
    pub model: Option<String>,
    pub turns: u32,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub tool_output_bytes: u64,
    /// Tool outputs whose last line was an axkit truncation hint.
    pub truncations: u32,
    pub tokens: Tokens,
    /// The cost the stream itself reported, kept for comparison only.
    pub reported_cost_usd: Option<f64>,
    /// A terminal event arrived; a crash or a kill leaves this false.
    pub completed: bool,
    pub error: Option<String>,
    /// The harness could not authenticate; the whole eval must stop.
    pub auth_failed: bool,
}

impl Metrics {
    /// Count one tool result.
    pub fn tool_result(&mut self, text: &str, is_error: bool) {
        self.tool_errors += u32::from(is_error);
        self.tool_output_bytes += text.len() as u64;
        if is_truncation_tail(text) {
            self.truncations += 1;
        }
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.error.get_or_insert_with(|| message.into());
    }
}

/// Does the text end with an axkit `# records … shown … full output saved to`
/// hint? Streams recorded before axkit 0.8 say `full output in`; both count.
#[must_use]
pub fn is_truncation_tail(text: &str) -> bool {
    text.trim_end().lines().next_back().is_some_and(|last| {
        let last = last.trim_start();
        last.starts_with("# records ")
            && last.contains("shown")
            && (last.contains("full output saved to ") || last.contains("full output in "))
    })
}

/// A JSON number field as `u64`, 0 when absent.
pub(crate) fn count(value: &serde_json::Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// Every JSON object line of a stream, in order; other lines are skipped.
pub(crate) fn events(stream: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    stream.lines().map(str::trim).filter(|l| l.starts_with('{')).filter_map(|l| serde_json::from_str(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncation_hint_is_recognized_only_as_the_last_line() {
        assert!(is_truncation_tail("a\n# records 1-2 of 9 shown; full output saved to /x\n"));
        assert!(is_truncation_tail("# records 1-1 of 1 shown, record 1 cut at line 3 of 9; full output saved to /x"));
        assert!(is_truncation_tail("a\n# records 1-2 of 9 shown; full output in /x (grep it)\n"), "the pre-0.8 form");
        assert!(is_truncation_tail("# records 1-1 of 1 shown, record 1 cut at line 3 of 9; full output in /x (grep it)"));
        assert!(!is_truncation_tail("# records 1-2 of 9 shown; full output saved to /x\nmore"));
        assert!(!is_truncation_tail(""));
    }

    #[test]
    fn totals_count_every_billed_kind_but_not_reasoning_twice() {
        let t = Tokens { input: 1, output: 2, cache_read: 3, cache_write_5m: 4, cache_write_1h: 5, reasoning: 100 };
        assert_eq!(t.total(), 15);
    }

    #[test]
    fn tool_results_add_up() {
        let outputs = [("ok", false), ("Error: x\n", true), ("a\n# records 1-1 of 2 shown; full output saved to /f", false)];
        let mut m = Metrics::default();
        for (text, is_error) in outputs {
            m.tool_result(text, is_error);
        }
        assert_eq!((m.tool_errors, m.truncations), (1, 1));
        assert_eq!(m.tool_output_bytes, outputs.iter().map(|(t, _)| t.len() as u64).sum::<u64>());
    }
}
