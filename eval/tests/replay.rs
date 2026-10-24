//! Read the shell calls out of recorded streams of each harness and pick the
//! failed ones the way `axkit-eval replay` does. No harness or tool runs.

use axkit_eval::harness;
use axkit_eval::replay;

fn calls(name: &str, stream: &str) -> Vec<harness::Call> {
    harness::by_name(name).expect("a known harness").calls(stream)
}

#[test]
fn claude_calls_pair_commands_with_results_and_a_piped_failure_is_still_found() {
    let calls = calls("claude", include_str!("fixtures/calls-claude.jsonl"));
    assert_eq!(calls.len(), 3);
    assert!(calls[1].command.contains("bnx exec manyfuncs 'for s in bv.strings:"), "{:?}", calls[1].command);
    assert!(calls[1].output.starts_with("Error: not_found: cannot read the script"));
    assert!(!calls[1].failed, "the pipe into head hid the exit status from Claude Code");

    let cases = replay::extract("bnx-re", "claude", "src", "bnx", &calls);
    assert_eq!(cases.len(), 1, "{cases:?}");
    assert!(cases[0].command.starts_with("bnx exec manyfuncs 'for s in bv.strings:"), "cd and the pipe are gone: {}", cases[0].command);
    assert!(!cases[0].command.contains("| head") && !cases[0].command.contains("2>&1"));
    assert!(cases[0].error.contains("{ws}/for s in bv.strings"), "the old workspace became {{ws}}: {}", cases[0].error);
}

#[test]
fn codex_commands_are_unwrapped_from_the_login_shell() {
    let calls = calls("codex", include_str!("fixtures/calls-codex.jsonl"));
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].command, "command -v idax");
    assert!(calls[1].failed && calls[1].command.starts_with("pwd; rg --files"), "{:?}", calls[1].command);
    assert_eq!(calls[2].command, "idax exec ./addcipher -c 'decompile(\"main\")'", "double-quoted: the escapes are undone");
    let cases = replay::extract("idax-re", "codex", "src", "idax", &calls);
    assert_eq!(cases.len(), 1, "rg failing is not the tool's failure: {cases:?}");
    assert_eq!(cases[0].error, "Error: script_error: LookupError: nothing is named 'main' (at <exec>:1)");
}

#[test]
fn grok_calls_take_the_exit_status_from_the_text_shown_to_the_model() {
    let calls = calls("grok", include_str!("fixtures/calls-grok.jsonl"));
    assert_eq!(calls.len(), 3);
    assert!(calls[0].command.starts_with("bnx --help && echo"));
    assert_eq!(calls[2].command, "bnx exec crackme -c 'functions()'");
    assert!(calls.iter().all(|c| !c.failed && c.output.starts_with("exit: 0")));
    assert!(replay::extract("bnx-re", "grok", "src", "bnx", &calls).is_empty(), "nothing failed");
}
