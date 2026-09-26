//! The log viewer's `all` tab: every process's log in one buffer, each line
//! prefixed with where it came from, in the order the lines arrived.
//!
//! Each process keeps its own [`LogTail`]; a poll reads all of them and
//! appends whatever each one grew by. What was already in the files when
//! the tab opened has no arrival order to recover — a log line carries no
//! clock pando can trust — so that backlog comes one source after another,
//! and everything after it is interleaved as it lands. What one poll
//! brings is shared out so that it fits the buffer, and no source's lines
//! push out another's that arrived with them.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::path::PathBuf;

use crate::log_tail::{LogLevel, LogTail, ParsedLine};
use crate::theme::{blue, cyan, green, magenta, orange, text_muted, yellow};

/// The tab's name. Chosen so it cannot be mistaken for a process: the tab
/// is only offered when no source already has the name.
pub const ALL_SOURCE: &str = "all";

/// Low bits of a merged block id that say which source it came from.
const SOURCE_BITS: u32 = 16;

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

    /// Starts merging a process that was not there when the tab opened —
    /// a process added to the config and started, or one whose log
    /// appeared only now. Its backlog arrives with the next poll, after
    /// what is already merged, and no more of it than its share of the
    /// buffer. A name already merged is left alone.
    pub fn add_source(&mut self, name: String, path: PathBuf) {
        if self.sources.iter().any(|s| s.name == name) {
            return;
        }
        // Wider prefixes from here on; the lines already merged keep theirs.
        self.name_width = self.name_width.max(name.chars().count());
        let capacity = self.capacity;
        self.sources.push(Source {
            name,
            tail: LogTail::new(path, capacity),
            merged: 0,
        });
    }

    /// Whether any of the logs exists yet.
    pub fn exists(&self) -> bool {
        self.sources.iter().any(|s| s.tail.path().exists())
    }

    /// Reads every source and appends what each grew by. Whether anything
    /// arrived.
    pub fn poll(&mut self) -> anyhow::Result<bool> {
        self.evicted.clear();
        // Every source is read before any of it is pushed, so what they
        // brought together can be shared out first.
        let mut reads = Vec::with_capacity(self.sources.len());
        for source in &mut self.sources {
            let last = source.tail.lines().back().map(|line| line.file_offset);
            let backlog = source.merged == 0;
            // One unreadable log is not a reason to stop showing the rest.
            let _ = source.tail.poll();
            let seen = source.tail.lines_seen();
            let lines = source.tail.lines();
            let new = (seen - source.merged).min(lines.len() as u64) as usize;
            source.merged = seen;
            // The file was emptied and written again — a restart — and
            // what follows is a new run, not more of the old one. Judged
            // on the first line that arrived, whether it is taken or not.
            let restarted = new > 0 && restarted_at(last, &lines[lines.len() - new]);
            reads.push(SourceRead {
                backlog,
                restarted,
                new,
            });
        }
        let takes = self.takes(&reads);
        let mut grew = false;
        for (index, (read, take)) in reads.into_iter().zip(takes).enumerate() {
            if read.new == 0 {
                continue;
            }
            let source = &self.sources[index];
            let len = source.tail.lines().len();
            let fresh: Vec<ParsedLine> = source.tail.lines().range(len - take..).cloned().collect();
            let name = source.name.clone();
            if read.restarted {
                let line = self.restart_marker(index, &name);
                self.push(line);
            }
            for parsed in fresh {
                let line = self.prefixed(index, &name, parsed);
                self.push(line);
            }
            grew = true;
        }
        // What a source brought is copied into the buffer now. It keeps
        // only its newest line, which the next poll's restart check reads,
        // rather than a second copy of its log the size of the whole tab.
        for source in &mut self.sources {
            source.tail.keep_newest(1);
        }
        Ok(grew)
    }

    /// How many lines each source's own tail holds, in tab order.
    #[cfg(test)]
    pub fn held_by_sources(&self) -> Vec<usize> {
        self.sources.iter().map(|s| s.tail.lines().len()).collect()
    }

    /// How many of each source's new lines a poll adds, the newest ones.
    ///
    /// Together they fit the buffer, with the restart markers the poll
    /// adds, shared evenly between the sources that brought more than
    /// their share: pushed one source after another, a busy log's lines
    /// evicted every line of the sources before it. A backlog — what a log
    /// held before any of it was merged — is held to its source's share
    /// of the buffer, so a process that joins an open tab cannot push out
    /// what is already there.
    fn takes(&self, reads: &[SourceRead]) -> Vec<usize> {
        let joining = reads.iter().filter(|read| read.backlog).count();
        let backlogs: Vec<usize> = reads
            .iter()
            .map(|read| if read.backlog { read.new } else { 0 })
            .collect();
        let backlogs = fair_shares(&backlogs, self.capacity * joining / reads.len().max(1));
        let wants: Vec<usize> = reads
            .iter()
            .zip(backlogs)
            .map(|(read, backlog)| if read.backlog { backlog } else { read.new })
            .collect();
        let markers = reads.iter().filter(|read| read.restarted).count();
        fair_shares(&wants, self.capacity.saturating_sub(markers))
    }

    fn push(&mut self, line: ParsedLine) {
        if self.buffer.len() == self.capacity
            && let Some(old) = self.buffer.pop_front()
        {
            self.evicted.push(old.level);
        }
        self.buffer.push_back(line);
    }

    /// The line that says a source's log started over, where it did.
    fn restart_marker(&self, index: usize, name: &str) -> ParsedLine {
        let text = RESTART_MARKER;
        let parsed = ParsedLine {
            plain: text.to_string(),
            plain_lower: text.to_string(),
            styled: Line::from(Span::styled(text, Style::new().fg(text_muted()))),
            level: LogLevel::Info,
            has_ansi: false,
            file_offset: 0,
            json_start: None,
            block_id: None,
        };
        self.prefixed(index, name, parsed)
    }

    /// One source's line as the merged buffer holds it: `api │ …`, with
    /// every offset that points into the text moved past the prefix, and a
    /// JSON block's id made unique across sources.
    fn prefixed(&self, index: usize, name: &str, mut parsed: ParsedLine) -> ParsedLine {
        let prefix = format!("{name:<width$}{SOURCE_SEPARATOR}", width = self.name_width);
        parsed.plain = format!("{prefix}{}", parsed.plain);
        parsed.plain_lower = parsed.plain.to_lowercase();
        parsed.json_start = parsed.json_start.map(|at| at + prefix.len());
        // Unique across sources whatever their number, which can grow
        // while the tab is open: an id that depended on the count would
        // give a block after a source joined the id of an earlier one.
        parsed.block_id = parsed.block_id.map(|id| (id << SOURCE_BITS) | index as u64);
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

/// What one poll read from a source.
struct SourceRead {
    /// Whether nothing of the source had been merged before, so what it
    /// brought is its log's backlog.
    backlog: bool,
    /// Whether its log started over, which the tab marks.
    restarted: bool,
    /// How many of the tail's lines are new since the last merge.
    new: usize,
}

/// Splits `room` between `wants` as evenly as they allow: each gets what
/// it wants or an equal share of what the smaller wants left, whichever
/// is less.
fn fair_shares(wants: &[usize], room: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..wants.len()).collect();
    order.sort_by_key(|&at| wants[at]);
    let mut left = room;
    let mut shares = vec![0; wants.len()];
    for (served, at) in order.into_iter().enumerate() {
        let share = (left / (wants.len() - served)).min(wants[at]);
        shares[at] = share;
        left -= share;
    }
    shares
}

/// What the `all` tab says where a source's log was emptied and written
/// again — a restart of that process.
pub(super) const RESTART_MARKER: &str = "── log restarted ──";

/// Whether a poll that brought `first_new` started the file over: a log
/// is only ever appended to, so a new line that begins at or before where
/// the last one read began is from a file that was emptied and rewritten.
fn restarted_at(last: Option<u64>, first_new: &ParsedLine) -> bool {
    last.is_some_and(|last| first_new.file_offset <= last)
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
    /// The merged tab's tail, when that is what this is.
    pub fn merged_mut(&mut self) -> Option<&mut MergedTail> {
        match self {
            ViewTail::One(_) => None,
            ViewTail::All(merged) => Some(merged),
        }
    }

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

    /// Polls, and says whether a single source's file started over — a
    /// restart empties it in place. The tail keeps what it read of the old
    /// run, so it is opened afresh on the new one: the old run's lines
    /// above the new ones read as one log, and they are not. The `all`
    /// tab marks the place in its own buffer instead, where the other
    /// sources' lines still belong.
    pub fn poll_for_restart(&mut self) -> (bool, bool) {
        let ViewTail::One(tail) = self else {
            return (self.poll().unwrap_or(false), false);
        };
        let last = tail.lines().back().map(|line| line.file_offset);
        let seen = tail.lines_seen();
        let grew = tail.poll().unwrap_or(false);
        let new = ((tail.lines_seen() - seen) as usize).min(tail.lines().len());
        let restarted = new > 0 && restarted_at(last, &tail.lines()[tail.lines().len() - new]);
        if restarted {
            let mut fresh = LogTail::new(tail.path().to_path_buf(), tail.capacity());
            let _ = fresh.poll();
            *tail = fresh;
        }
        (grew, restarted)
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
