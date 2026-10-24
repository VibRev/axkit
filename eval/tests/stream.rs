//! Parse recorded harness streams and check metrics, recomputed cost and the
//! final answer end to end. No real harness is launched.

use axkit_eval::harness;
use axkit_eval::metrics::Tokens;
use axkit_eval::price::Prices;

const CLAUDE: &str = include_str!("fixtures/session.jsonl");

#[test]
fn a_recorded_claude_session_yields_the_expected_metrics() {
    let p = harness::by_name("claude").unwrap().parse(CLAUDE);
    let m = &p.metrics;
    assert!(m.completed && !m.auth_failed);
    assert_eq!((m.turns, m.tool_calls, m.tool_errors, m.truncations), (3, 2, 0, 1));
    assert_eq!(m.tokens, Tokens { input: 2550, output: 118, cache_read: 6600, cache_write_5m: 1200, cache_write_1h: 0, reasoning: 0 });
    assert_eq!(m.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(m.reported_cost_usd, Some(0.0142));
    assert!(p.answer.contains("s3cret_42") && !p.answer.contains("I'll open"));
}

#[test]
fn cost_is_recomputed_from_tokens_with_the_builtin_table() {
    let m = harness::by_name("claude").unwrap().parse(CLAUDE).metrics;
    let price = Prices::builtin().get("claude-sonnet-5-5").unwrap();
    let expected = (2550.0 * 2.0 + 118.0 * 10.0 + 1200.0 * 2.5 + 6600.0 * 0.20) / 1e6;
    assert!((price.cost(m.tokens) - expected).abs() < 1e-12);
}
