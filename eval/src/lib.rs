//! Run agent harnesses against a task set and measure what each run costs.
//!
//! The runner is domain-agnostic: it knows tasks, groups, workspaces, event
//! streams, tokens and cost — not reverse engineering. idax and bnx supply the
//! task sets and fixtures under their own `eval/` directories.

pub mod cleanup;
pub mod harness;
pub mod judge;
pub mod memory;
pub mod metrics;
pub mod price;
pub mod replay;
pub mod report;
pub mod runner;
pub mod suite;
