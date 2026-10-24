use super::{SpillDir, Window};
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

/// An output destination failed. A closed stdout (`| head`) is not one.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("cannot write to stdout: {0}")]
    Stdout(#[source] io::Error),
    #[error("cannot write {}: {source}", path.display())]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What a finished [`Sink`] wrote, for the caller's trace and exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub records: usize,
    /// Records that reached stdout, a cut one included.
    pub shown: usize,
    /// The window cut the output, or the producer stopped early.
    pub truncated: bool,
    /// The file holding the whole output, when the hint names one.
    pub full_output: Option<PathBuf>,
}

/// Record sink: stdout through a [`Window`], plus an optional `-o` file.
///
/// A record is one [`Sink::record`] call and may span lines (a function with
/// its body is one record). [`Sink::comment`] lines (`# ...` headers and
/// totals) are always shown and never counted.
///
/// A closed stdout silently stops the stdout copy while the file keeps being
/// written. Any other I/O error drops the failing destination, the first one
/// is returned by [`Sink::finish`].
pub struct Sink<'a, W: Write> {
    stdout: Option<W>,
    file: Option<(PathBuf, BufWriter<File>)>,
    spill: Option<&'a SpillDir>,
    window: Window,
    head: Budget,
    /// The whole output, kept while a window applies and no `-o` file holds it.
    full: Option<String>,
    phase: Phase,
    records: usize,
    shown: usize,
    shown_bytes: usize,
    cut_record: Option<CutRecord>,
    stopped_early: bool,
    error: Option<OutputError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Records go to stdout while they fit the budget.
    Head,
    /// Nothing more reaches stdout but comments.
    Cut,
}

/// Count and byte bounds of the window; `None` is unbounded.
#[derive(Clone, Copy, Debug)]
struct Budget {
    count: Option<usize>,
    bytes: Option<usize>,
}

impl Budget {
    fn admits(self, count: usize, bytes: usize) -> bool {
        self.count.is_none_or(|n| count <= n) && self.bytes.is_none_or(|n| bytes <= n)
    }
}

/// The one record that was shown only in part (CONVENTIONS §4.6).
#[derive(Clone, Copy, Debug)]
struct CutRecord {
    index: usize,
    lines_shown: usize,
    lines: usize,
}

impl<'a, W: Write> Sink<'a, W> {
    /// `spill` receives the whole output when the window cuts it; without
    /// one the hint only gives the `--offset` of the next records.
    pub fn new(stdout: W, window: Window, spill: Option<&'a SpillDir>) -> Sink<'a, W> {
        Sink {
            stdout: Some(stdout),
            file: None,
            spill,
            window,
            head: Budget { count: window.limit, bytes: window.max_bytes },
            full: (!window.is_unbounded()).then(String::new),
            phase: Phase::Head,
            records: 0,
            shown: 0,
            shown_bytes: 0,
            cut_record: None,
            stopped_early: false,
            error: None,
        }
    }

    /// Also write everything to `path` (`-o`); the window applies to stdout
    /// only, and a cut names this file instead of spilling.
    ///
    /// # Errors
    ///
    /// The file cannot be created.
    pub fn with_output_file(mut self, path: &Path) -> Result<Self, OutputError> {
        let file = File::create(path).map_err(|source| OutputError::File { path: path.to_path_buf(), source })?;
        self.file = Some((path.to_path_buf(), BufWriter::new(file)));
        self.full = None;
        Ok(self)
    }

    /// One record.
    pub fn record(&mut self, text: &str) {
        self.records += 1;
        let index = self.records;
        self.keep(text);
        if index <= self.window.offset {
            return;
        }
        let len = text.len() + 1;
        match self.phase {
            Phase::Head if self.head.admits(self.shown + 1, self.shown_bytes + len) => {
                self.shown += 1;
                self.shown_bytes += len;
                self.write_stdout(text);
            }
            Phase::Head if self.shown == 0 && self.head.count.is_none_or(|n| n > 0) => self.show_in_part(index, text),
            Phase::Head => self.phase = Phase::Cut,
            Phase::Cut => {}
        }
    }

    /// Each line of `text` as one record (a script's prints).
    pub fn lines(&mut self, text: &str) {
        text.lines().for_each(|l| self.record(l));
    }

    /// A JSON answer that is one object (CONVENTIONS §5): one line, whole,
    /// whatever the window. Cut, it would no longer be JSON.
    pub fn json(&mut self, value: &Value) {
        let line = value.to_string();
        self.records += 1;
        self.shown += 1;
        self.keep(&line);
        self.write_stdout(&line);
    }

    /// A `# ...` line that is always shown and is not a record.
    pub fn comment(&mut self, line: &str) {
        self.keep(line);
        self.write_stdout(line);
    }

    /// The producer stopped before the end (CONVENTIONS §4.7): the record
    /// count is a lower bound.
    pub fn stop_early(&mut self) {
        self.stopped_early = true;
    }

    /// Flush stdout so results stream while a long search runs.
    pub fn flush(&mut self) {
        if let Some(out) = self.stdout.as_mut()
            && let Err(e) = out.flush()
        {
            self.stdout = None;
            self.fail_stdout(e);
        }
    }

    /// Write the hint line, flush everything.
    ///
    /// # Errors
    ///
    /// The first failure of stdout (other than a closed pipe) or of the `-o`
    /// file.
    pub fn finish(mut self) -> Result<Summary, OutputError> {
        let truncated = self.phase == Phase::Cut || self.stopped_early;
        let (hint, full_output) = self.hint();
        if let Some(hint) = hint {
            self.write_stdout(&hint);
        }
        self.flush();
        if let Some((path, mut f)) = self.file.take()
            && let Err(source) = f.flush()
        {
            self.error.get_or_insert(OutputError::File { path, source });
        }
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(Summary { records: self.records, shown: self.shown, truncated, full_output }),
        }
    }

    /// The first record does not fit the byte budget on its own: show its
    /// leading whole lines, never half a line.
    fn show_in_part(&mut self, index: usize, text: &str) {
        let budget = self.head.bytes.unwrap_or(usize::MAX);
        let mut bytes = 0;
        let mut lines_shown = 0;
        let mut lines = 0;
        for line in text.split('\n') {
            lines += 1;
            if lines_shown + 1 == lines && bytes + line.len() < budget {
                bytes += line.len() + 1;
                lines_shown += 1;
                self.write_stdout(line);
            }
        }
        self.shown = 1;
        self.shown_bytes = bytes;
        self.cut_record = Some(CutRecord { index, lines_shown, lines });
        self.phase = Phase::Cut;
    }

    /// The last stdout line (CONVENTIONS §4.4), and the file it names.
    fn hint(&mut self) -> (Option<String>, Option<PathBuf>) {
        let offset = self.window.offset;
        if self.phase != Phase::Cut {
            let line = if self.stopped_early {
                Some(format!("# stopped after {} records; there are more", self.records))
            } else {
                let past_end = self.shown == 0 && offset > 0 && self.records > 0;
                past_end.then(|| format!("# no records after --offset {offset} ({} in all)", self.records))
            };
            return (line, None);
        }
        let total = if self.stopped_early { format!("at least {}", self.records) } else { self.records.to_string() };
        let (first, last) = (offset + 1, offset + self.shown);
        let mut shown =
            if self.shown == 0 { format!("no records of {total} shown") } else { format!("records {first}-{last} of {total} shown") };
        if let Some(cut) = self.cut_record {
            shown.push_str(&format!(", record {} cut at line {} of {}", cut.index, cut.lines_shown, cut.lines));
        }
        let more = (last < self.records || self.stopped_early).then(|| format!("rerun with --offset {last}"));
        let place = match (&self.file, self.full.take(), self.spill) {
            (Some((p, _)), _, _) => Ok(p.clone()),
            (None, Some(full), Some(spill)) => {
                spill.write(&full).map_err(|e| format!("cannot write the full output to {}: {e}", spill.dir().display()))
            }
            _ => Err(String::new()),
        };
        // With a file to read, the line names only the file: `--offset` paging is in
        // `--help`, and a second way to go on only invites rereading the output.
        let line = match &place {
            Ok(path) => format!("# {shown}; full output saved to {}", path.display()),
            Err(why) => {
                let parts: Vec<String> =
                    [Some(format!("# {shown}")), more, (!why.is_empty()).then(|| why.clone())].into_iter().flatten().collect();
                parts.join("; ")
            }
        };
        (Some(line), place.ok())
    }

    /// Copy to the `-o` file and the full-output buffer.
    fn keep(&mut self, line: &str) {
        if let Some(full) = self.full.as_mut() {
            full.push_str(line);
            full.push('\n');
        }
        if let Some((path, f)) = self.file.as_mut()
            && let Err(source) = writeln!(f, "{line}")
        {
            let path = path.clone();
            self.file = None;
            self.error.get_or_insert(OutputError::File { path, source });
        }
    }

    fn write_stdout(&mut self, line: &str) {
        if let Some(out) = self.stdout.as_mut()
            && let Err(e) = writeln!(out, "{line}")
        {
            self.stdout = None;
            self.fail_stdout(e);
        }
    }

    fn fail_stdout(&mut self, e: io::Error) {
        if e.kind() != ErrorKind::BrokenPipe {
            self.error.get_or_insert(OutputError::Stdout(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(window: Window, spill: Option<&SpillDir>, feed: impl FnOnce(&mut Sink<'_, &mut Vec<u8>>)) -> (String, Summary) {
        let mut out = Vec::new();
        let mut sink = Sink::new(&mut out, window, spill);
        feed(&mut sink);
        let summary = sink.finish().unwrap();
        (String::from_utf8(out).unwrap(), summary)
    }

    fn head(offset: usize, limit: Option<usize>, max_bytes: Option<usize>) -> Window {
        Window { offset, limit, max_bytes }
    }

    /// The path a hint line names.
    fn named_file(hint: &str) -> PathBuf {
        PathBuf::from(hint.split("full output saved to ").nth(1).unwrap())
    }

    #[test]
    fn unbounded_window_passes_everything_through() {
        let (text, summary) = run(Window::default(), None, |s| {
            s.comment("# head");
            s.record("a");
            s.record("b");
        });
        assert_eq!(text, "# head\na\nb\n");
        assert!(!summary.truncated);
    }

    #[test]
    fn window_shows_whole_records_and_always_the_comments() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(dir.path().join("out"), 1 << 20);
        let (text, summary) = run(head(1, Some(2), None), Some(&spill), |s| {
            s.comment("# head");
            for r in ["a", "b\n  b2", "c", "d"] {
                s.record(r);
            }
            s.comment("# 4 results");
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..5], ["# head", "b", "  b2", "c", "# 4 results"]);
        assert!(lines[5].starts_with("# records 2-3 of 4 shown; full output saved to "), "{text}");
        let file = named_file(lines[5]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# head\na\nb\n  b2\nc\nd\n# 4 results\n");
        assert_eq!(summary.full_output, Some(file));
        assert!(summary.truncated);
    }

    #[test]
    fn without_a_spill_directory_the_hint_gives_the_next_offset() {
        let (text, _) = run(head(0, None, Some(4)), None, |s| {
            for r in ["ab", "cdef", "g"] {
                s.record(r);
            }
        });
        assert_eq!(text, "ab\n# records 1-1 of 3 shown; rerun with --offset 1\n");
    }

    #[test]
    fn an_offset_past_the_end_says_so() {
        let (text, _) = run(head(5, None, None), None, |s| s.record("a"));
        assert_eq!(text, "# no records after --offset 5 (1 in all)\n");
    }

    #[test]
    fn an_oversized_first_record_shows_its_leading_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(dir.path().join("out"), 1 << 20);
        let body = "int main() {\n  return 0;\n}";
        let (text, summary) = run(head(0, None, Some(26)), Some(&spill), |s| {
            s.record(body);
            s.record("next");
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..2], ["int main() {", "  return 0;"]);
        assert!(lines[2].starts_with("# records 1-1 of 2 shown, record 1 cut at line 2 of 3; full output saved to "), "{text}");
        assert!(!lines[2].contains("--offset"), "{text}");
        assert_eq!(summary.shown, 1);
    }

    #[test]
    fn a_line_longer_than_the_budget_is_never_shown_in_half() {
        let (text, _) = run(head(0, None, Some(4)), None, |s| s.record("abcdefgh"));
        assert_eq!(text, "# records 1-1 of 1 shown, record 1 cut at line 0 of 1\n");
    }

    #[test]
    fn stopping_early_makes_the_total_a_lower_bound() {
        let (text, _) = run(head(0, Some(1), None), None, |s| {
            s.record("a");
            s.record("b");
            s.stop_early();
        });
        assert_eq!(text, "a\n# records 1-1 of at least 2 shown; rerun with --offset 1\n");
    }

    #[test]
    fn output_file_gets_everything_and_is_named_by_the_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("all.txt");
        let mut out = Vec::new();
        let mut sink = Sink::new(&mut out, head(0, Some(1), None), None).with_output_file(&path).unwrap();
        sink.record("a");
        sink.record("b");
        sink.finish().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
        assert_eq!(named_file(text.lines().last().unwrap()), path);
    }

    #[test]
    fn a_json_object_is_never_cut() {
        let value = json!({ "text": "x".repeat(100) });
        let (text, _) = run(head(0, Some(0), Some(10)), None, |s| s.json(&value));
        assert_eq!(text, format!("{value}\n"));
    }

    #[test]
    fn a_closed_stdout_is_not_an_error_and_the_file_keeps_going() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut sink = Sink::new(Closed, Window::default(), None).with_output_file(&path).unwrap();
        sink.record("a");
        sink.record("b");
        sink.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    #[test]
    fn other_stdout_errors_are_reported() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink::new(Full, Window::default(), None);
        sink.record("a");
        let err = sink.finish().unwrap_err();
        assert!(matches!(err, OutputError::Stdout(_)), "{err}");
    }
}
