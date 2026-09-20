use anyhow::{Context, Result};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// Colors come from the appearance-aware palette because non-ANSI lines are
// colorized at ingest in `parse_line`, not at render time.
use crate::theme::{cyan, green, magenta, red, text_dim, text_muted, yellow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct ParsedLine {
    pub plain: String,
    pub plain_lower: String,
    pub styled: Line<'static>,
    pub level: LogLevel,
    pub has_ansi: bool,
    /// Byte offset of this line's first byte in the log file. Stable across
    /// ring-buffer eviction and re-opened tails, so viewers can mark "seen
    /// through here" positions. Approximate only on invalid-UTF-8 input.
    pub file_offset: u64,
    /// Byte index in `plain` where a trailing JSON object/array begins, when
    /// the rest of the line parses as JSON. Drives syntax coloring at ingest
    /// and the viewer's pretty-print inspector.
    pub json_start: Option<usize>,
    /// Set when this line belongs to a multi-line pretty-printed JSON block
    /// (a bare `{`/`[` line through its balancing closer). Lines of one block
    /// share the id, carry the block's max severity, and inspect as a unit.
    pub block_id: Option<u64>,
}

fn classify_level(lower: &str) -> LogLevel {
    if lower.contains("error")
        || lower.contains(" err ")
        || lower.contains("err:")
        || lower.contains("err]")
        || lower.contains("fatal")
        || lower.contains("panic")
    {
        LogLevel::Error
    } else if lower.contains("warn") {
        LogLevel::Warn
    } else if lower.contains("debug") || lower.contains("trace") {
        LogLevel::Debug
    } else {
        LogLevel::Info
    }
}

fn has_ansi_codes(raw: &str) -> bool {
    raw.contains("\x1b[")
}

fn parse_line(raw: &str, file_offset: u64) -> ParsedLine {
    use ansi_to_tui::IntoText;
    let has_ansi = has_ansi_codes(raw);
    let plain = if has_ansi {
        strip_ansi(raw)
    } else {
        raw.to_string()
    };
    let plain_lower = plain.to_lowercase();
    let json_at = if has_ansi { None } else { json_start(&plain) };
    let styled = if has_ansi {
        raw.into_text()
            .ok()
            .and_then(|text| text.into_iter().next())
            .unwrap_or_else(|| Line::raw(raw.to_string()))
    } else if let Some(idx) = json_at {
        // Syntax-color the JSON; the severity gutter already carries the
        // level, so no whole-line wash. Any prefix (timestamp, tag) keeps
        // its pattern colors.
        let mut spans = if idx > 0 {
            rich_colorize(&plain[..idx]).spans
        } else {
            Vec::new()
        };
        spans.extend(colorize_json(&plain[idx..]));
        Line::from(spans)
    } else if looks_like_json_fragment(plain.trim()) {
        Line::from(colorize_json(&plain))
    } else {
        keyword_colorize(&plain, &plain_lower)
    };
    let level = classify_level(&plain_lower);
    ParsedLine {
        plain,
        plain_lower,
        styled,
        level,
        has_ansi,
        file_offset,
        json_start: json_at,
        block_id: None,
    }
}

/// Net brace/bracket depth change of one line, ignoring braces inside
/// double-quoted strings (`\"` escapes handled). Strings in stringify output
/// never span lines, so per-line state is enough.
fn brace_net(s: &str) -> isize {
    let mut net = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => net += 1,
            '}' | ']' if !in_string => net -= 1,
            _ => {}
        }
    }
    net
}

/// Stateless "is this an interior line of a pretty-printed JSON block"
/// check, so fragment coloring works even when the tailer seeked into the
/// middle of a block and never saw its opener.
fn looks_like_json_fragment(trimmed: &str) -> bool {
    matches!(trimmed, "{" | "}" | "[" | "]" | "}," | "],")
        || (trimmed.starts_with('"')
            && (trimmed.contains("\":") || trimmed.ends_with('"') || trimmed.ends_with("\",")))
}

/// How many `{`/`[` positions to try as the start of a trailing JSON blob.
/// Real structured logs put the JSON at the line start or right after a short
/// prefix, and the cap keeps brace-heavy garbage lines from N full parses.
const MAX_JSON_START_CANDIDATES: usize = 4;

/// Byte index where a trailing JSON object/array begins, if everything from
/// there to the end of the line parses as JSON.
fn json_start(plain: &str) -> Option<usize> {
    let trimmed = plain.trim_end();
    if !(trimmed.ends_with('}') || trimmed.ends_with(']')) {
        return None;
    }
    plain
        .char_indices()
        .filter(|(_, c)| *c == '{' || *c == '[')
        .take(MAX_JSON_START_CANDIDATES)
        .find(|(idx, _)| serde_json::from_str::<serde::de::IgnoredAny>(&plain[*idx..]).is_ok())
        .map(|(idx, _)| idx)
}

/// Tokenize a JSON fragment into syntax-colored spans: keys cyan, string
/// values dim (URLs underlined like bare links), numbers/booleans/null
/// magenta, punctuation muted. Works on unbalanced fragments too, so the
/// inspector can color pretty-printed JSON line by line.
pub(crate) fn colorize_json(s: &str) -> Vec<Span<'static>> {
    let bytes = s.as_bytes();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut other = String::new();
    let mut i = 0;
    while i < s.len() {
        let c = bytes[i];
        if c == b'"' {
            let mut j = i + 1;
            while j < s.len() {
                match bytes[j] {
                    b'\\' => j += 2,
                    b'"' => {
                        j += 1;
                        break;
                    }
                    _ => j += 1,
                }
            }
            let j = j.min(s.len());
            if !other.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut other),
                    Style::new().fg(text_muted()),
                ));
            }
            let content = &s[i..j];
            let inner = content.trim_matches('"');
            let is_key = s[j..].trim_start().starts_with(':');
            let style = if is_key {
                Style::new().fg(cyan())
            } else if inner.starts_with("https://") || inner.starts_with("http://") {
                Style::new().fg(cyan()).add_modifier(Modifier::UNDERLINED)
            } else {
                Style::new().fg(text_dim())
            };
            spans.push(Span::styled(content.to_string(), style));
            i = j;
        } else if c.is_ascii_digit()
            || (c == b'-' && bytes.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
        {
            let mut j = i + 1;
            while j < s.len() && matches!(bytes[j], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
            {
                j += 1;
            }
            if !other.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut other),
                    Style::new().fg(text_muted()),
                ));
            }
            spans.push(Span::styled(
                s[i..j].to_string(),
                Style::new().fg(magenta()),
            ));
            i = j;
        } else if let Some(word) = ["true", "false", "null"]
            .iter()
            .find(|w| s[i..].starts_with(**w))
        {
            if !other.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut other),
                    Style::new().fg(text_muted()),
                ));
            }
            spans.push(Span::styled(word.to_string(), Style::new().fg(magenta())));
            i += word.len();
        } else {
            // Punctuation, whitespace, and anything unexpected batch into one
            // muted run — span count matters at 10k buffered lines.
            let ch = s[i..].chars().next().unwrap_or('\u{FFFD}');
            other.push(ch);
            i += ch.len_utf8();
        }
    }
    if !other.is_empty() {
        spans.push(Span::styled(other, Style::new().fg(text_muted())));
    }
    spans
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() || nc == 'm' {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn keyword_colorize(line: &str, lower: &str) -> Line<'static> {
    if lower.contains("error")
        || lower.contains(" err ")
        || lower.contains("err:")
        || lower.contains("err]")
        || lower.contains("fatal")
        || lower.contains("panic")
    {
        return Line::styled(line.to_string(), Style::new().fg(red()));
    }
    if lower.contains("warn") {
        return Line::styled(line.to_string(), Style::new().fg(yellow()));
    }
    if lower.contains("ready on")
        || lower.contains("listening")
        || lower.contains("started")
        || lower.contains("compiled")
    {
        return Line::styled(line.to_string(), Style::new().fg(green()));
    }
    rich_colorize(line)
}

fn rich_colorize(line: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pos = 0;

    while pos < line.len() {
        if let Some(m) = find_pattern(&line[pos..]) {
            if m.start > 0 {
                spans.push(Span::styled(
                    line[pos..pos + m.start].to_string(),
                    Style::new().fg(text_dim()),
                ));
            }
            spans.push(Span::styled(
                line[pos + m.start..pos + m.end].to_string(),
                m.style,
            ));
            pos += m.end;
        } else {
            spans.push(Span::styled(
                line[pos..].to_string(),
                Style::new().fg(text_dim()),
            ));
            break;
        }
    }

    if spans.is_empty() {
        Line::styled(line.to_string(), Style::new().fg(text_dim()))
    } else {
        Line::from(spans)
    }
}

struct PatternMatch {
    start: usize,
    end: usize,
    style: Style,
}

fn find_pattern(s: &str) -> Option<PatternMatch> {
    let mut best: Option<PatternMatch> = None;

    // HTTP methods
    for method in &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        if let Some(idx) = s.find(method) {
            let end = idx + method.len();
            let at_boundary = (idx == 0 || !s.as_bytes()[idx - 1].is_ascii_alphanumeric())
                && (end >= s.len() || !s.as_bytes()[end].is_ascii_alphanumeric());
            if at_boundary && best.as_ref().is_none_or(|b| idx < b.start) {
                best = Some(PatternMatch {
                    start: idx,
                    end,
                    style: Style::new().fg(cyan()).add_modifier(Modifier::BOLD),
                });
            }
        }
    }

    // Durations (`85ms`, `450 ms`, `1.2s`) colored by speed so slow requests
    // and builds pop while scanning. Registered before status codes so a
    // number that is both (e.g. `450 ms`) reads as a duration. A bare `s`
    // unit requires a decimal point — decades like "1980s" aren't durations.
    {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_digit()
                || (i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.'))
            {
                i += 1;
                continue;
            }
            let start = i;
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut has_decimal = false;
            if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
                has_decimal = true;
                j += 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            let mut k = j;
            let spaced = k < b.len() && b[k] == b' ';
            if spaced {
                k += 1;
            }
            let unit_len = if s[k..].starts_with("ms") {
                2
            } else if !spaced && has_decimal && s[k..].starts_with('s') {
                1
            } else {
                0
            };
            let end = k + unit_len;
            let at_boundary = end >= b.len() || !b[end].is_ascii_alphanumeric();
            if unit_len > 0 && at_boundary {
                let value: f64 = s[start..j].parse().unwrap_or(0.0);
                let ms = if unit_len == 2 { value } else { value * 1000.0 };
                let color = if ms < 300.0 {
                    green()
                } else if ms < 1000.0 {
                    yellow()
                } else {
                    red()
                };
                if best.as_ref().is_none_or(|bm| start < bm.start) {
                    best = Some(PatternMatch {
                        start,
                        end,
                        style: Style::new().fg(color),
                    });
                }
                break;
            }
            i = j.max(i + 1);
        }
    }

    // HTTP status codes (3-digit numbers at word boundary)
    let mut i = 0;
    while i + 2 < s.len() {
        let b = s.as_bytes();
        if b[i].is_ascii_digit() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() {
            let at_start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            let at_end = i + 3 >= s.len() || !b[i + 3].is_ascii_alphanumeric();
            if at_start && at_end {
                let code: u16 = (b[i] - b'0') as u16 * 100
                    + (b[i + 1] - b'0') as u16 * 10
                    + (b[i + 2] - b'0') as u16;
                let style = match code {
                    200..=299 => Some(Style::new().fg(green())),
                    300..=399 => Some(Style::new().fg(cyan())),
                    400..=499 => Some(Style::new().fg(yellow())),
                    500..=599 => Some(Style::new().fg(red())),
                    _ => None,
                };
                if let Some(style) = style
                    && best.as_ref().is_none_or(|b| i < b.start)
                {
                    best = Some(PatternMatch {
                        start: i,
                        end: i + 3,
                        style,
                    });
                }
            }
        }
        i += 1;
    }

    // URLs (http:// or https://)
    for prefix in &["https://", "http://"] {
        if let Some(idx) = s.find(prefix) {
            let rest = &s[idx + prefix.len()..];
            let url_end = rest
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>' || c == ')')
                .unwrap_or(rest.len());
            let end = idx + prefix.len() + url_end;
            if best.as_ref().is_none_or(|b| idx < b.start) {
                best = Some(PatternMatch {
                    start: idx,
                    end,
                    style: Style::new().fg(cyan()).add_modifier(Modifier::UNDERLINED),
                });
            }
        }
    }

    // File paths with line numbers (e.g., src/foo.ts:42)
    if let Some(idx) = s.find("src/") {
        let rest = &s[idx..];
        let path_end = rest
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"' || c == '\'')
            .unwrap_or(rest.len());
        if path_end > 4 && best.as_ref().is_none_or(|b| idx < b.start) {
            best = Some(PatternMatch {
                start: idx,
                end: idx + path_end,
                style: Style::new().fg(cyan()),
            });
        }
    }

    // Timestamps (HH:MM:SS or ISO-ish prefix)
    if s.len() >= 8 {
        let b = s.as_bytes();
        for t in 0..s.len().saturating_sub(7) {
            if b[t].is_ascii_digit()
                && b[t + 1].is_ascii_digit()
                && b[t + 2] == b':'
                && b[t + 3].is_ascii_digit()
                && b[t + 4].is_ascii_digit()
                && b[t + 5] == b':'
                && b[t + 6].is_ascii_digit()
                && b[t + 7].is_ascii_digit()
            {
                // A date prefix (2024-01-02T) puts an alphanumeric `T` right
                // before the time, so anchor the word-boundary check on the
                // start of the full stamp rather than on the bare time.
                let has_date_prefix = t >= 11
                    && b[t - 1] == b'T'
                    && b[t - 2].is_ascii_digit()
                    && b[t - 4] == b'-'
                    && b[t - 7] == b'-';
                let ts_start = if has_date_prefix { t - 11 } else { t };
                let at_start = ts_start == 0 || !b[ts_start - 1].is_ascii_alphanumeric();
                if at_start {
                    // Extend for milliseconds (.123) and timezone (Z or +00:00)
                    let mut end = t + 8;
                    if end < s.len() && b[end] == b'.' {
                        end += 1;
                        while end < s.len() && b[end].is_ascii_digit() {
                            end += 1;
                        }
                    }
                    if end < s.len() && b[end] == b'Z' {
                        end += 1;
                    }
                    if best.as_ref().is_none_or(|b| ts_start < b.start) {
                        best = Some(PatternMatch {
                            start: ts_start,
                            end,
                            style: Style::new().fg(text_muted()),
                        });
                    }
                    break;
                }
            }
        }
    }

    // Stack trace frames: "at " prefix
    if let Some(idx) = s.find("    at ")
        && best.as_ref().is_none_or(|b| idx < b.start)
    {
        best = Some(PatternMatch {
            start: idx,
            end: s.len(),
            style: Style::new().fg(red()).add_modifier(Modifier::DIM),
        });
    }

    best
}

/// Byte budget per buffered line for the first read of an existing file.
/// A fresh tailer only ever displays the last `capacity` lines, so it seeks
/// to `capacity * this` bytes before EOF instead of parsing the whole file —
/// dev logs grow to hundreds of MB and the first poll runs on the UI thread.
const MAX_INITIAL_BYTES_PER_LINE: u64 = 512;

/// Bookkeeping for a multi-line JSON block currently being ingested.
struct OpenBlock {
    id: u64,
    depth: isize,
    level: LogLevel,
    line_count: usize,
}

/// Runaway backstop: interleaved non-JSON output (raw SQL dumps, another
/// process writing mid-block) can keep the brace depth positive forever.
const MAX_BLOCK_LINES: usize = 500;

pub struct LogTail {
    path: PathBuf,
    buffer: VecDeque<ParsedLine>,
    capacity: usize,
    offset: u64,
    leftover: String,
    /// File offset where the next (possibly still incomplete) line begins —
    /// i.e. where `leftover`'s first byte lives. Source of each pushed line's
    /// `file_offset`.
    line_start_offset: u64,
    /// Levels of lines evicted from the front during the most recent `poll`.
    /// Lets the viewer realign absolute indices (search matches, scroll) after
    /// the ring buffer drops old lines. Cleared at the top of every `poll`.
    evicted_levels: Vec<LogLevel>,
    /// Every line ever pushed, counted. Monotonic: it survives both
    /// eviction and truncation, which is what a follower needs in order to
    /// know how many lines are new since it last looked. `lines().len()`
    /// cannot say — it stops at `capacity` and never moves again.
    lines_seen: u64,
    block_id_counter: u64,
    open_block: Option<OpenBlock>,
}

impl LogTail {
    pub fn new(path: PathBuf, capacity: usize) -> Self {
        assert!(capacity > 0, "LogTail capacity must be > 0");
        Self {
            path,
            buffer: VecDeque::with_capacity(capacity),
            capacity,
            offset: 0,
            leftover: String::new(),
            line_start_offset: 0,
            evicted_levels: Vec::new(),
            lines_seen: 0,
            block_id_counter: 0,
            open_block: None,
        }
    }

    pub fn lines(&self) -> &VecDeque<ParsedLine> {
        &self.buffer
    }

    /// How many lines this tail keeps before it starts evicting. The
    /// detail pane's glance and the full viewer are two orders of
    /// magnitude apart, so which one a tail is matters.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Levels of lines evicted by the most recent `poll`, oldest first.
    pub fn evicted_levels(&self) -> &[LogLevel] {
        &self.evicted_levels
    }

    /// How many lines this tail has ever read, across evictions and
    /// truncations. A follower prints `lines_seen() - printed_so_far` from
    /// the back of the buffer and is never silently left behind.
    pub fn lines_seen(&self) -> u64 {
        self.lines_seen
    }

    pub fn poll(&mut self) -> Result<bool> {
        self.evicted_levels.clear();
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(format!("open log {}", self.path.display()))
                );
            }
        };

        let size = file
            .metadata()
            .with_context(|| format!("stat log {}", self.path.display()))?
            .len();

        if size < self.offset {
            self.offset = 0;
            self.leftover.clear();
            self.line_start_offset = 0;
            self.open_block = None;
        }
        let mut skip_partial_first_line = false;
        if self.offset == 0 {
            let max_initial = (self.capacity as u64).saturating_mul(MAX_INITIAL_BYTES_PER_LINE);
            if size > max_initial {
                self.offset = size - max_initial;
                skip_partial_first_line = true;
            }
        }
        if size == self.offset {
            return Ok(false);
        }

        let seek_pos = self.offset;
        file.seek(SeekFrom::Start(self.offset))
            .context("seek log")?;
        let mut raw = Vec::new();
        let read_bytes = file.read_to_end(&mut raw).context("read log")?;
        self.offset += read_bytes as u64;
        let mut chunk = String::from_utf8_lossy(&raw).into_owned();
        if skip_partial_first_line {
            chunk = match chunk.find('\n') {
                Some(newline) => {
                    self.line_start_offset = seek_pos + newline as u64 + 1;
                    chunk[newline + 1..].to_string()
                }
                None => {
                    self.line_start_offset = self.offset;
                    String::new()
                }
            };
        }

        if chunk.is_empty() {
            return Ok(false);
        }

        let mut combined = std::mem::take(&mut self.leftover);
        combined.push_str(&chunk);

        let ends_with_newline = combined.ends_with('\n');
        let mut parts: Vec<&str> = combined.split('\n').collect();
        if !ends_with_newline {
            self.leftover = parts.pop().unwrap_or("").to_string();
        } else {
            parts.pop();
        }

        let added = !parts.is_empty();
        let mut cursor = self.line_start_offset;
        for line in parts {
            self.push_line(line, cursor);
            cursor += line.len() as u64 + 1;
        }
        self.line_start_offset = cursor;
        Ok(added)
    }

    fn push_line(&mut self, raw: &str, file_offset: u64) {
        if self.buffer.len() == self.capacity
            && let Some(evicted) = self.buffer.pop_front()
        {
            self.evicted_levels.push(evicted.level);
        }
        let mut parsed = parse_line(raw, file_offset);
        self.track_block(&mut parsed);
        self.buffer.push_back(parsed);
        self.lines_seen += 1;
    }

    /// Multi-line JSON block bookkeeping: tag member lines with the block id,
    /// keep the whole block at its max severity (retroactively raising lines
    /// already pushed — the `"level": "error"` member arrives after the `{`),
    /// and close on the balancing brace or the runaway cap.
    fn track_block(&mut self, parsed: &mut ParsedLine) {
        let trimmed = parsed.plain.trim();
        if self.open_block.is_none() && (trimmed == "{" || trimmed == "[") {
            self.block_id_counter += 1;
            self.open_block = Some(OpenBlock {
                id: self.block_id_counter,
                depth: 0,
                level: LogLevel::Debug,
                line_count: 0,
            });
        }
        let Some(block) = &mut self.open_block else {
            return;
        };
        parsed.block_id = Some(block.id);
        block.line_count += 1;
        block.depth += brace_net(&parsed.plain);
        if parsed.level > block.level {
            block.level = parsed.level;
            let (id, level) = (block.id, block.level);
            for line in self.buffer.iter_mut().rev() {
                if line.block_id != Some(id) {
                    break;
                }
                line.level = line.level.max(level);
            }
        }
        parsed.level = parsed.level.max(block.level);
        if block.depth <= 0 || block.line_count >= MAX_BLOCK_LINES {
            self.open_block = None;
        }
    }
}

pub fn snapshot(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    let mut tail = LogTail::new(path.to_path_buf(), max_lines);
    tail.poll()?;
    // A one-shot read won't get a terminating newline for the last line of a
    // file that doesn't end in one; flush it so the final line isn't dropped.
    if !tail.leftover.is_empty() {
        let leftover = std::mem::take(&mut tail.leftover);
        let offset = tail.line_start_offset;
        tail.push_line(&leftover, offset);
    }
    Ok(tail.lines().iter().map(|l| l.plain.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_all(path: &Path, data: &str) {
        std::fs::write(path, data).unwrap();
    }

    fn append(path: &Path, data: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(data.as_bytes()).unwrap();
    }

    fn plain_lines(tail: &LogTail) -> Vec<String> {
        tail.lines().iter().map(|l| l.plain.clone()).collect()
    }

    #[test]
    fn poll_on_missing_file_returns_false_with_empty_buffer() {
        let dir = tempdir().unwrap();
        let mut tail = LogTail::new(dir.path().join("nope.log"), 10);
        assert!(!tail.poll().unwrap());
        assert!(tail.lines().is_empty());
    }

    #[test]
    fn poll_reads_complete_lines_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "first\nsecond\nthird\n");

        let mut tail = LogTail::new(path, 10);
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail), vec!["first", "second", "third"]);
    }

    #[test]
    fn poll_retains_partial_trailing_line_until_newline_arrives() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "hello wor");

        let mut tail = LogTail::new(path.clone(), 10);
        assert!(!tail.poll().unwrap());
        assert!(tail.lines().is_empty());

        append(&path, "ld\nnext\n");
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail), vec!["hello world", "next"]);
    }

    #[test]
    fn poll_handles_incremental_appends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "one\n");

        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();
        append(&path, "two\nthree\n");
        tail.poll().unwrap();

        assert_eq!(plain_lines(&tail), vec!["one", "two", "three"]);
    }

    #[test]
    fn buffer_discards_oldest_when_capacity_exceeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "a\nb\nc\nd\ne\n");

        let mut tail = LogTail::new(path, 3);
        tail.poll().unwrap();
        assert_eq!(plain_lines(&tail), vec!["c", "d", "e"]);
    }

    #[test]
    fn first_poll_of_large_file_starts_at_tail_with_complete_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.log");
        // 100 lines x ~100 bytes >> capacity(4) * 512-byte initial budget,
        // so the first poll must seek near EOF. The seek lands mid-line;
        // the fragment must be dropped, not shown as a bogus line.
        let lines: Vec<String> = (0..100).map(|i| format!("line-{i:0>96}")).collect();
        write_all(&path, &(lines.join("\n") + "\n"));

        let mut tail = LogTail::new(path.clone(), 4);
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail), lines[96..].to_vec());

        append(&path, "after\n");
        assert!(tail.poll().unwrap());
        let mut expected = lines[97..].to_vec();
        expected.push("after".to_string());
        assert_eq!(plain_lines(&tail), expected);
    }

    #[test]
    fn first_poll_of_giant_single_line_keeps_buffer_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("oneline.log");
        // No newline inside the capped initial window: nothing complete to
        // show yet, and the poll must not panic or fabricate a fragment.
        write_all(&path, &"x".repeat(5000));

        let mut tail = LogTail::new(path.clone(), 4);
        assert!(!tail.poll().unwrap());
        assert!(tail.lines().is_empty());

        append(&path, "\ntail-line\n");
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail).last().unwrap(), "tail-line");
    }

    #[test]
    fn truncation_resets_reader_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "original\ncontent\n");

        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();
        assert_eq!(tail.lines().len(), 2);

        write_all(&path, "fresh\n");
        tail.poll().unwrap();
        assert_eq!(plain_lines(&tail), vec!["original", "content", "fresh"]);
    }

    #[test]
    fn snapshot_returns_last_n_lines_of_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "x\ny\nz\nw\n");
        let lines = snapshot(&path, 2).unwrap();
        assert_eq!(
            lines,
            vec!["y".to_string(), "z".to_string(), "w".to_string()]
                .into_iter()
                .rev()
                .take(2)
                .rev()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn snapshot_keeps_unterminated_final_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "alpha\nbeta\ngamma"); // no trailing newline
        let lines = snapshot(&path, 10).unwrap();
        assert_eq!(lines, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn ansi_codes_are_stripped_from_plain() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "\x1b[32mOK\x1b[0m normal\n");

        let mut tail = LogTail::new(path, 10);
        tail.poll().unwrap();
        let line = &tail.lines()[0];
        assert_eq!(line.plain, "OK normal");
        assert!(line.has_ansi);
    }

    #[test]
    fn panic_lines_are_colored_red() {
        // keyword_colorize must agree with classify_level: a panic is an error.
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "thread 'main' panicked at boom\n");
        let mut tail = LogTail::new(path, 10);
        tail.poll().unwrap();
        let line = &tail.lines()[0];
        assert_eq!(line.level, LogLevel::Error);
        assert_eq!(line.styled.style.fg, Some(red()));
    }

    #[test]
    fn iso_timestamp_prefix_is_styled_as_one_stamp() {
        // The full ISO stamp (date + T + time) gets the muted style, not just
        // the bare HH:MM:SS — the date-prefix back-extension must be reached.
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "2024-01-02T15:04:05 handled request\n");
        let mut tail = LogTail::new(path, 10);
        tail.poll().unwrap();
        let first = &tail.lines()[0].styled.spans[0];
        assert_eq!(first.content.as_ref(), "2024-01-02T15:04:05");
        assert_eq!(first.style.fg, Some(text_muted()));
    }

    #[test]
    fn classify_level_detects_errors() {
        // classify_level expects pre-lowered input (parse_line lowers once).
        let classify = |s: &str| classify_level(&s.to_lowercase());
        assert_eq!(classify("ERROR: something broke"), LogLevel::Error);
        assert_eq!(classify("this has a warn in it"), LogLevel::Warn);
        assert_eq!(classify("debug: verbose"), LogLevel::Debug);
        assert_eq!(classify("server listening on 3000"), LogLevel::Info);
    }

    fn span_style(line: &Line<'static>, text: &str) -> Option<Style> {
        line.spans
            .iter()
            .find(|s| s.content.as_ref() == text)
            .map(|s| s.style)
    }

    #[test]
    fn parsed_lines_carry_file_offsets() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "aa\nbbbb\n");
        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();
        let offsets: Vec<u64> = tail.lines().iter().map(|l| l.file_offset).collect();
        assert_eq!(offsets, vec![0, 3]);

        // A line split across polls starts where its first byte landed (8),
        // not where the completing poll began reading.
        append(&path, "cc");
        tail.poll().unwrap();
        append(&path, "c\ndd\n");
        tail.poll().unwrap();
        let offsets: Vec<u64> = tail.lines().iter().map(|l| l.file_offset).collect();
        assert_eq!(offsets, vec![0, 3, 8, 12]);
    }

    #[test]
    fn file_offsets_reset_on_truncation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "original\n");
        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();

        write_all(&path, "x\ny\n");
        tail.poll().unwrap();
        let offsets: Vec<u64> = tail.lines().iter().map(|l| l.file_offset).collect();
        // The pre-truncation line keeps its stale offset; the fresh file's
        // lines restart at 0.
        assert_eq!(offsets, vec![0, 0, 2]);
    }

    fn parse_plain(raw: &str) -> ParsedLine {
        parse_line(raw, 0)
    }

    fn tail_of(lines: &[&str]) -> LogTail {
        let dir = tempdir().unwrap();
        let path = dir.path().join("block.log");
        write_all(&path, &(lines.join("\n") + "\n"));
        let mut tail = LogTail::new(path, 100);
        tail.poll().unwrap();
        tail
    }

    // The shape a dev server prints for a pretty-printed object:
    // JSON.stringify(x, null, 2) written line by line.
    const BLOCK: [&str; 6] = [
        "ready on port 3000",
        "{",
        r#"  "timestamp": "2026-07-21T16:14:16.709Z","#,
        r#"  "level": "error","#,
        r#"  "message": "unhandled_rejection""#,
        "}",
    ];

    #[test]
    fn multi_line_json_block_lines_share_a_block_id() {
        let tail = tail_of(&BLOCK);
        let ids: Vec<Option<u64>> = tail.lines().iter().map(|l| l.block_id).collect();
        assert_eq!(ids[0], None, "prefix line is not part of the block");
        let id = ids[1].expect("bare {{ opens a block");
        assert!(
            ids[1..].iter().all(|i| *i == Some(id)),
            "every line through the closing }} carries the same block id: {ids:?}"
        );

        // A following unrelated line starts fresh.
        let tail = tail_of(&["{", "}", "after"]);
        assert_eq!(tail.lines()[2].block_id, None);
    }

    #[test]
    fn block_level_is_raised_retroactively_for_the_whole_block() {
        let tail = tail_of(&BLOCK);
        for line in tail.lines().iter().skip(1) {
            assert_eq!(
                line.level,
                LogLevel::Error,
                "the \"level\": \"error\" member must paint the whole block, got {:?} on {:?}",
                line.level,
                line.plain
            );
        }
        assert_eq!(
            tail.lines()[0].level,
            LogLevel::Info,
            "prefix line untouched"
        );
    }

    #[test]
    fn braces_inside_strings_do_not_close_the_block() {
        let tail = tail_of(&[
            "{",
            r#"  "msg": "brace } inside a string","#,
            r#"  "n": 1"#,
            "}",
            "after",
        ]);
        let id = tail.lines()[0].block_id.unwrap();
        assert_eq!(
            tail.lines()[3].block_id,
            Some(id),
            "closing }} still in block"
        );
        assert_eq!(
            tail.lines()[4].block_id,
            None,
            "block closed at the real }}"
        );
    }

    #[test]
    fn interior_fragment_lines_are_syntax_colored_without_block_context() {
        // Stateless: a viewer seeking into the middle of a block still colors.
        let line = parse_plain(r#"  "code": "ER_BAD_FIELD_ERROR","#);
        assert_eq!(
            span_style(&line.styled, "\"code\"").unwrap().fg,
            Some(cyan()),
            "fragment keys colored like full-line JSON"
        );
        let plain = parse_plain("plain text line");
        assert!(
            plain
                .styled
                .spans
                .iter()
                .all(|s| s.style.fg != Some(cyan())),
            "non-fragment lines keep their normal coloring"
        );
    }

    #[test]
    fn pure_json_line_is_syntax_colored() {
        let line = parse_plain(r#"{"level":"info","msg":"hi","count":3}"#);
        assert_eq!(line.json_start, Some(0));
        let styled = &line.styled;
        assert_eq!(
            span_style(styled, "\"level\"").unwrap().fg,
            Some(cyan()),
            "keys are cyan"
        );
        assert_eq!(
            span_style(styled, "\"hi\"").unwrap().fg,
            Some(text_dim()),
            "string values dim"
        );
        assert_eq!(
            span_style(styled, "3").unwrap().fg,
            Some(magenta()),
            "numbers magenta"
        );
        assert!(
            styled.style.fg.is_none(),
            "JSON lines get span colors, not a whole-line wash"
        );
    }

    #[test]
    fn prefixed_json_keeps_prefix_patterns_and_colors_the_json() {
        let line = parse_plain(r#"12:04:05 {"a":1}"#);
        assert_eq!(line.json_start, Some(9));
        assert_eq!(
            span_style(&line.styled, "12:04:05").unwrap().fg,
            Some(text_muted()),
            "timestamp prefix keeps its pattern color"
        );
        assert_eq!(span_style(&line.styled, "\"a\"").unwrap().fg, Some(cyan()));
    }

    #[test]
    fn url_string_values_render_like_links() {
        let line = parse_plain(r#"{"url":"https://x.dev/a"}"#);
        let style = span_style(&line.styled, "\"https://x.dev/a\"").unwrap();
        assert_eq!(style.fg, Some(cyan()));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn booleans_and_null_are_magenta() {
        let line = parse_plain(r#"{"ok":true,"err":null}"#);
        assert_eq!(
            span_style(&line.styled, "true").unwrap().fg,
            Some(magenta())
        );
        assert_eq!(
            span_style(&line.styled, "null").unwrap().fg,
            Some(magenta())
        );
    }

    #[test]
    fn non_json_braces_do_not_trigger_json_coloring() {
        for raw in ["use {} braces here", "[web] starting", r#"{"broken": }"#] {
            let line = parse_plain(raw);
            assert_eq!(line.json_start, None, "{raw:?} is not JSON");
        }
    }

    #[test]
    fn json_error_line_still_classifies_as_error() {
        // The severity gutter relies on classify_level seeing the raw text —
        // JSON syntax coloring must not bypass level detection.
        let line = parse_plain(r#"{"level":"error","msg":"boom"}"#);
        assert_eq!(line.level, LogLevel::Error);
        assert_eq!(line.json_start, Some(0));
    }

    #[test]
    fn durations_are_colored_by_speed() {
        let fast = rich_colorize("GET /api/a 200 in 85ms");
        assert_eq!(span_style(&fast, "85ms").unwrap().fg, Some(green()));

        let medium = rich_colorize("built in 450 ms");
        assert_eq!(span_style(&medium, "450 ms").unwrap().fg, Some(yellow()));

        let slow = rich_colorize("ready in 2.1s");
        assert_eq!(span_style(&slow, "2.1s").unwrap().fg, Some(red()));
    }

    #[test]
    fn duration_boundaries_reject_decades_and_glued_words() {
        // "1980s" is a decade (bare `s` needs a decimal point) and "4msx" has
        // no word boundary after the unit — both must stay plain dim text.
        for line in ["popular in the 1980s", "token 4msx here"] {
            let styled = rich_colorize(line);
            assert!(
                styled.spans.iter().all(|s| s.style.fg == Some(text_dim())),
                "{line:?} must not match any pattern: {:?}",
                styled.spans
            );
        }
    }

    #[test]
    fn non_ansi_line_is_colorized_at_ingest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "ERROR boom\n");

        let mut tail = LogTail::new(path, 10);
        tail.poll().unwrap();
        let line = &tail.lines()[0];
        assert!(!line.has_ansi);
        // Uppercase ERROR must still classify — classification runs on the
        // single lowered copy parse_line makes.
        assert_eq!(line.level, LogLevel::Error);
        // keyword_colorize tags an error line with a line-level red style;
        // a plain Line::raw would leave fg unset — proving colorize ran at ingest.
        assert_eq!(line.styled.style.fg, Some(red()));
    }
}
