//! The full-screen log viewer's state: filter, search, inspect overlay,
//! and the helpers that walk its buffer.

use ratatui::text::Line;
use std::collections::VecDeque;

use crate::log_tail::{LogLevel, ParsedLine, colorize_json};

use super::merged::ViewTail;

/// Lines the full-screen viewer keeps for the log it has open. Two orders
/// of magnitude more than the detail pane's glance, because the viewer is
/// what a stack trace from ten minutes ago is read in.
pub const LOG_VIEWER_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Inactive,
    Typing,
    Active,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    /// Buffer indices of the lines that match, in order.
    pub matches: Vec<usize>,
    /// Which of those the viewer is sitting on.
    pub cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFilter {
    #[default]
    All,
    WarnPlus,
    ErrorOnly,
}

impl LogFilter {
    pub fn cycle(self) -> Self {
        match self {
            LogFilter::All => LogFilter::WarnPlus,
            LogFilter::WarnPlus => LogFilter::ErrorOnly,
            LogFilter::ErrorOnly => LogFilter::All,
        }
    }

    pub fn passes(self, level: LogLevel) -> bool {
        match self {
            LogFilter::All => true,
            LogFilter::WarnPlus => level >= LogLevel::Warn,
            LogFilter::ErrorOnly => level >= LogLevel::Error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LogFilter::All => "all",
            LogFilter::WarnPlus => "warn+",
            LogFilter::ErrorOnly => "errors",
        }
    }
}

/// Pretty-print overlay for one log line (`J`). `lines` are pre-styled;
/// `text` is the plain version `y` yanks.
pub struct LineInspect {
    pub lines: Vec<Line<'static>>,
    pub text: String,
    pub scroll: usize,
}

/// One worktree's log, open full screen.
pub struct LogView {
    pub name: String,
    /// Which log is on screen. A plain string, not an enum: a source is a
    /// process, a hook, or later a tunnel, and pando learns the set from
    /// the files that are there.
    pub source: String,
    /// Every source this worktree has a file for, in tab order. Rebuilt
    /// from disk each time the tab bar is drawn, so a hook that ran once
    /// appears and a source that never existed does not.
    pub available: Vec<String>,
    /// One source's log, or every process's merged for the `all` tab.
    pub tail: ViewTail,
    /// Viewport top, as a position in the *filtered* list.
    pub scroll: usize,
    /// The highlighted line, also a filtered position. The viewport follows
    /// it; while `follow` is on it rides the tail.
    pub cursor: usize,
    pub follow: bool,
    /// Lines the filter shows that arrived below the viewport while it was
    /// scrolled up. Cleared by any jump back to the live tail.
    pub new_below: usize,
    pub search_mode: SearchMode,
    pub search: SearchState,
    /// The file was not there when the viewer opened.
    pub missing: bool,
    /// The file was there and is not any more. Separate from `missing`
    /// because the buffer still holds everything that was read of it, and
    /// that is worth keeping on screen. Decided by the paint, which reads
    /// the whole log directory; the tick only notices when it has gone
    /// stale.
    pub gone: bool,
    pub log_filter: LogFilter,
    /// Collapse to the lines matching the search, grep-style (`&`).
    pub filter_to_matches: bool,
    /// When false (`w`), long lines truncate to one row instead of
    /// wrapping, so one noisy dump cannot fill the screen.
    pub wrap: bool,
}

impl LogView {
    pub(super) fn new(
        name: String,
        source: String,
        available: Vec<String>,
        mut tail: ViewTail,
    ) -> Self {
        let missing = !tail.exists();
        if !missing {
            tail.poll().ok();
        }
        Self {
            name,
            source,
            available,
            tail,
            // usize::MAX is "the bottom", clamped by the first paint: the
            // viewer opens on the newest line, following.
            scroll: usize::MAX,
            cursor: usize::MAX,
            follow: !missing,
            new_below: 0,
            search_mode: SearchMode::Inactive,
            search: SearchState::default(),
            missing,
            gone: false,
            log_filter: LogFilter::All,
            filter_to_matches: false,
            wrap: true,
        }
    }

    /// Whether `filter_to_matches` is actually collapsing anything: an
    /// empty query would otherwise hide every line.
    pub fn collapsed(&self) -> bool {
        self.filter_to_matches && !self.search.query.is_empty()
    }

    /// Buffer indices of the lines the viewer is currently showing.
    pub fn visible(&self) -> Vec<usize> {
        let query = self.search.query.to_lowercase();
        let collapse = self.collapsed();
        self.tail
            .lines()
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                self.log_filter.passes(p.level) && (!collapse || p.plain_lower.contains(&query))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// How many lines the viewer is showing.
    pub fn visible_len(&self) -> usize {
        if self.collapsed() {
            return self.visible().len();
        }
        self.tail
            .lines()
            .iter()
            .filter(|p| self.log_filter.passes(p.level))
            .count()
    }
}

/// The first http(s) URL in a line, ending at whitespace or a closing
/// delimiter. What `Y` yanks: a dev server's address is the one thing in a
/// log that is worth copying on its own.
pub(super) fn first_url(line: &str) -> Option<String> {
    // Earliest in the line, not first scheme tried: a line that mentions a
    // plain-http address before an https one means the http one.
    let (at, scheme_len) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| line.find(scheme).map(|at| (at, scheme.len())))
        .min_by_key(|(at, _)| *at)?;
    let rest = &line[at + scheme_len..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ')'))
        .unwrap_or(rest.len());
    Some(line[at..at + scheme_len + end].to_string())
}

/// Where the errors are, as positions in the visible list: one target per
/// error *block*, and one per run of consecutive standalone error lines.
///
/// Stepping through all forty lines of one stack trace is not jumping to
/// the next error — but two blocks that happen to be adjacent are two
/// errors, so a run only merges while the block id stays the same.
pub(super) fn error_ranks(view: &LogView) -> Vec<usize> {
    let query = view.search.query.to_lowercase();
    let collapse = view.collapsed();
    let mut ranks = Vec::new();
    let mut rank = 0usize;
    let mut previous: Option<(bool, Option<u64>)> = None;
    for parsed in view.tail.lines() {
        let visible = view.log_filter.passes(parsed.level)
            && (!collapse || parsed.plain_lower.contains(&query));
        if !visible {
            continue;
        }
        let is_error = parsed.level >= LogLevel::Error;
        if is_error {
            let same_run = matches!(previous, Some((true, block)) if block == parsed.block_id);
            if !same_run {
                ranks.push(rank);
            }
        }
        previous = Some((is_error, parsed.block_id));
        rank += 1;
    }
    ranks
}

/// The contiguous range of the block containing `at`.
pub(super) fn block_bounds(buffer: &VecDeque<ParsedLine>, at: usize, id: u64) -> (usize, usize) {
    let mut start = at;
    while start > 0 && buffer[start - 1].block_id == Some(id) {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < buffer.len() && buffer[end + 1].block_id == Some(id) {
        end += 1;
    }
    (start, end)
}

/// The block's lines rejoined, each passed through `text` first — which
/// takes the `all` tab's source prefix off, so the JSON parses again.
pub(super) fn joined_block<'a>(
    buffer: &'a VecDeque<ParsedLine>,
    start: usize,
    end: usize,
    text: impl Fn(&'a str) -> &'a str,
) -> String {
    buffer
        .range(start..=end)
        .map(|parsed| text(parsed.plain.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Overlay content for a single line: the JSON it carries, pretty-printed
/// and syntax-coloured with any prefix kept above it, or the raw line —
/// which the overlay wraps, so this is also the long-line reader.
pub(super) fn inspect_content(parsed: &ParsedLine) -> (String, Vec<Line<'static>>) {
    let pretty = parsed.json_start.and_then(|at| {
        serde_json::from_str::<serde_json::Value>(&parsed.plain[at..])
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .map(|pretty| (at, pretty))
    });
    let Some((at, pretty)) = pretty else {
        return (parsed.plain.clone(), vec![parsed.styled.clone()]);
    };
    let prefix = parsed.plain[..at].trim_end();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !prefix.is_empty() {
        lines.push(Line::styled(
            prefix.to_string(),
            ratatui::style::Style::new().fg(crate::theme::text_muted()),
        ));
        lines.push(Line::raw(""));
    }
    lines.extend(pretty.lines().map(|line| Line::from(colorize_json(line))));
    let text = if prefix.is_empty() {
        pretty
    } else {
        format!("{prefix}\n\n{pretty}")
    };
    (text, lines)
}

/// Rebuilds the match set for the current query, under the current level
/// filter. Resets the ordinal, so callers that care about staying on the
/// same *line* have to put it back.
pub(super) fn recompute_matches(view: &mut LogView) {
    view.search.cursor = 0;
    if view.search.query.is_empty() {
        view.search.matches.clear();
        return;
    }
    let lowered = view.search.query.to_lowercase();
    view.search.matches = view
        .tail
        .lines()
        .iter()
        .enumerate()
        .filter(|(_, parsed)| {
            view.log_filter.passes(parsed.level) && parsed.plain_lower.contains(&lowered)
        })
        .map(|(at, _)| at)
        .collect();
}

/// Where absolute buffer index `at` sits in the *visible* list: how many
/// lines before it the filter keeps. `scroll` and `cursor` are positions in
/// that list, so a match has to be translated before it can be jumped to.
pub(super) fn filtered_rank(view: &LogView, at: usize) -> usize {
    view.tail
        .lines()
        .iter()
        .take(at)
        .filter(|parsed| view.log_filter.passes(parsed.level))
        .count()
}

/// Moves to the next (or previous) match, wrapping at both ends, and
/// centres the viewport on it.
pub(super) fn step_match(view: &mut LogView, forward: bool, viewer_height: usize) {
    let len = view.search.matches.len();
    if len == 0 {
        return;
    }
    view.search.cursor = if forward {
        (view.search.cursor + 1) % len
    } else {
        (view.search.cursor + len - 1) % len
    };

    let at = view.search.matches[view.search.cursor];
    // Collapsed, the visible list *is* the match list, so the ordinal is
    // already the rank.
    let rank = if view.collapsed() {
        view.search.cursor
    } else {
        filtered_rank(view, at)
    };
    view.cursor = rank;
    view.scroll = rank.saturating_sub(viewer_height / 2);
    view.follow = false;
}
