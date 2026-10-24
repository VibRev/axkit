//! Bounded output (CONVENTIONS §3–§4).
//!
//! A command writes records and `#` comments into a [`Sink`]; the [`Window`]
//! decides which records reach stdout. Whatever the window cuts is never
//! lost silently: the whole output goes to a file ([`SpillDir`]) and the last
//! stdout line says what was shown, where the rest is and how to page on.

mod sink;
mod spill;

pub use sink::{OutputError, Sink, Summary};
pub use spill::{DEFAULT_BUDGET, SpillDir, cache_dir};

/// Which records of a command's output reach stdout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    /// Records skipped before the first one shown.
    pub offset: usize,
    /// At most this many records shown.
    pub limit: Option<usize>,
    /// At most this many bytes of records shown; comments do not count.
    pub max_bytes: Option<usize>,
}

impl Window {
    /// No bound given: everything goes to stdout, exactly as without a window.
    pub fn is_unbounded(&self) -> bool {
        self.offset == 0 && self.limit.is_none() && self.max_bytes.is_none()
    }
}
