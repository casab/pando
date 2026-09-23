//! The log viewer's `all` tab: every process's log in one buffer, each line
//! prefixed with where it came from, in the order the lines arrived.
//!
//! Each process keeps its own [`LogTail`]; a poll reads all of them and
//! appends whatever each one grew by. What was already in the files when
//! the tab opened has no arrival order to recover — a log line carries no
//! clock pando can trust — so that backlog comes one source after another,
//! and everything after it is interleaved as it lands.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::path::PathBuf;

use crate::log_tail::{LogLevel, LogTail, ParsedLine};
use crate::theme::{blue, cyan, green, magenta, orange, text_muted, yellow};

/// The tab's name. Chosen so it cannot be mistaken for a process: the tab
/// is only offered when no source already has the name.
pub const ALL_SOURCE: &str = "all";

/// What stands between the source name and the line.
pub const SOURCE_SEPARATOR: &str = " │ ";

/// Colours the prefixes cycle through, so two processes side by side are
/// told apart before their names are read.
fn source_color(index: usize) -> Color {
    let palette = [cyan(), magenta(), blue(), green(), orange(), yellow()];
    palette[index % palette.len()]
}

struct Source {
    name: String,
    tail: LogTail,
    /// `tail.lines_seen()` as of the last merge.
    merged: u64,
}

/// Several logs read as one.
pub struct MergedTail {
    sources: Vec<Source>,
    buffer: VecDeque<ParsedLine>,
    capacity: usize,
    evicted: Vec<LogLevel>,
    /// The widest source name, so every prefix is the same width and the
    /// lines start in one column.
    name_width: usize,
}

impl MergedTail {
    pub fn new(sources: Vec<(String, PathBuf)>, capacity: usize) -> Self {
        let name_width = sources
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        Self {
            sources: sources
                .into_iter()
                .map(|(name, path)| Source {
                    name,
                    tail: LogTail::new(path, capacity),
                    merged: 0,
                })
                .collect(),
            buffer: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
            evicted: Vec::new(),
            name_width,
        }
    }

    pub fn lines(&self) -> &VecDeque<ParsedLine> {
        &self.buffer
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn evicted_levels(&self) -> &[LogLevel] {
        &self.evicted
    }

    /// The processes merged, in tab order.
    pub fn source_names(&self) -> Vec<&str> {
        self.sources.iter().map(|s| s.name.as_str()).collect()
    }

    /// Whether any of the logs exists yet.
    pub fn exists(&self) -> bool {
        self.sources.iter().any(|s| s.tail.path().exists())
    }

    /// Reads every source and appends what each grew by. Whether anything
    /// arrived.
    pub fn poll(&mut self) -> anyhow::Result<bool> {
        self.evicted.clear();
        let mut grew = false;
        for index in 0..self.sources.len() {
            let source = &mut self.sources[index];
            // One unreadable log is not a reason to stop showing the rest.
            let _ = source.tail.poll();
            let seen = source.tail.lines_seen();
            let new = (seen - source.merged).min(source.tail.lines().len() as u64) as usize;
            source.merged = seen;
            if new == 0 {
                continue;
            }
            let start = source.tail.lines().len() - new;
            let fresh: Vec<ParsedLine> = source.tail.lines().range(start..).cloned().collect();
            let name = source.name.clone();
            for parsed in fresh {
                let line = self.prefixed(index, &name, parsed);
                self.push(line);
            }
            grew = true;
        }
        Ok(grew)
    }

    fn push(&mut self, line: ParsedLine) {
        if self.buffer.len() == self.capacity
            && let Some(old) = self.buffer.pop_front()
        {
            self.evicted.push(old.level);
        }
        self.buffer.push_back(line);
    }

    /// One source's line as the merged buffer holds it: `api │ …`, with
    /// every offset that points into the text moved past the prefix, and a
    /// JSON block's id made unique across sources.
    fn prefixed(&self, index: usize, name: &str, mut parsed: ParsedLine) -> ParsedLine {
        let prefix = format!("{name:<width$}{SOURCE_SEPARATOR}", width = self.name_width);
        parsed.plain = format!("{prefix}{}", parsed.plain);
        parsed.plain_lower = parsed.plain.to_lowercase();
        parsed.json_start = parsed.json_start.map(|at| at + prefix.len());
        parsed.block_id = parsed
            .block_id
            .map(|id| id * (self.sources.len() as u64 + 1) + index as u64);
        let mut spans = vec![
            Span::styled(
                format!("{name:<width$}", width = self.name_width),
                Style::new().fg(source_color(index)),
            ),
            Span::styled(SOURCE_SEPARATOR, Style::new().fg(text_muted())),
        ];
        spans.extend(parsed.styled.spans);
        parsed.styled = Line::from(spans);
        parsed
    }
}

/// A line of the `all` tab without its source prefix: what `y` copies and
/// what a JSON block is re-parsed from.
pub fn strip_source(plain: &str) -> &str {
    match plain.find(SOURCE_SEPARATOR) {
        Some(at) => &plain[at + SOURCE_SEPARATOR.len()..],
        None => plain,
    }
}

/// What the viewer reads: one log, or every process's at once.
pub enum ViewTail {
    One(LogTail),
    All(MergedTail),
}

impl From<LogTail> for ViewTail {
    fn from(tail: LogTail) -> Self {
        ViewTail::One(tail)
    }
}

impl ViewTail {
    pub fn lines(&self) -> &VecDeque<ParsedLine> {
        match self {
            ViewTail::One(tail) => tail.lines(),
            ViewTail::All(merged) => merged.lines(),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            ViewTail::One(tail) => tail.capacity(),
            ViewTail::All(merged) => merged.capacity(),
        }
    }

    pub fn evicted_levels(&self) -> &[LogLevel] {
        match self {
            ViewTail::One(tail) => tail.evicted_levels(),
            ViewTail::All(merged) => merged.evicted_levels(),
        }
    }

    pub fn poll(&mut self) -> anyhow::Result<bool> {
        match self {
            ViewTail::One(tail) => tail.poll(),
            ViewTail::All(merged) => merged.poll(),
        }
    }

    /// Whether there is a file behind it — any of them, for `all`.
    pub fn exists(&self) -> bool {
        match self {
            ViewTail::One(tail) => tail.path().exists(),
            ViewTail::All(merged) => merged.exists(),
        }
    }

    /// The file behind a single-source tail.
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            ViewTail::One(tail) => Some(tail.path()),
            ViewTail::All(_) => None,
        }
    }

    /// A line's text as the source wrote it, without the `all` prefix.
    pub fn unprefixed<'a>(&self, plain: &'a str) -> &'a str {
        match self {
            ViewTail::One(_) => plain,
            ViewTail::All(_) => strip_source(plain),
        }
    }
}
