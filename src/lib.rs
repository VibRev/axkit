//! Agent-first CLI building blocks shared by `idax` and `bnx`.
//!
//! The contract these implement is `docs/CONVENTIONS.md`; section numbers in
//! the docs here refer to it. axkit depends on no engine SDK, so everything
//! in it is tested on any machine.

pub mod agent;
pub mod daemon;
pub mod exit;
pub mod mcp;
pub mod output;
pub mod skills;
