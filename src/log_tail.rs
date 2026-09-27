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
    // Any ESC, not only a CSI introducer: an OSC 8 hyperlink carries
    // `ESC ]` and no `ESC [` at all, and a line that is not classified as
    // ANSI keeps its escape bytes as text.
    raw.contains('\x1b')
}

/// Whether a parsed line's spans join back to `plain`, byte for byte.
///
/// The viewer finds search matches in `plain` and then paints them by
/// walking the spans with those same offsets, so a sequence one of the two
/// removes and the other keeps would highlight the wrong bytes. When they
/// disagree, `plain` is the one to trust.
fn spans_join_to(line: &Line<'static>, plain: &str) -> bool {
    let mut rest = plain;
    for span in &line.spans {
        match rest.strip_prefix(span.content.as_ref()) {
            Some(tail) => rest = tail,
            None => return false,
        }
    }
    rest.is_empty()
}

/// Tabs to spaces at stops of eight columns. An escape sequence takes no
/// column, so a coloured prefix does not push the stops along.
fn expand_tabs(raw: &str) -> String {
    const STOP: usize = 8;
    let mut out = String::with_capacity(raw.len() + 8);
    let mut column = 0;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => {
                let pad = STOP - column % STOP;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            '\x1b' => {
                out.push(c);
                // CSI: ESC [ … final byte in @..~.
                if chars.peek() == Some(&'[') {
                    out.push(chars.next().unwrap_or('['));
                    for next in chars.by_ref() {
                        out.push(next);
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
            }
            _ => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

fn parse_line(raw: &str, file_offset: u64) -> ParsedLine {
    use ansi_to_tui::IntoText;
    // A terminal draws a tab as a jump to the next stop; ratatui drops it
    // as a control character, so `\tat com.Foo` lost its indent. Expanded
    // here, once, so the text searched and the text drawn stay the same.
    let expanded;
    let raw = if raw.contains('\t') {
        expanded = expand_tabs(raw);
        expanded.as_str()
    } else {
        raw
    };
    let has_ansi = has_ansi_codes(raw);
    let plain = if has_ansi {
        strip_ansi(raw)
    } else {
        raw.to_string()
    };
    let plain_lower = plain.to_lowercase();
    let json_at = if has_ansi { None } else { json_start(&plain) };
    let styled = if has_ansi {
        // Falling back to `plain` rather than to `raw`: the old fallback
        // put the escape bytes of a line the parser could not read
        // straight onto the screen.
        raw.into_text()
            .ok()
            .and_then(|text| text.into_iter().next())
            .filter(|line| spans_join_to(line, &plain))
            .unwrap_or_else(|| keyword_colorize(&plain, &plain_lower))
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
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: parameter bytes, then one final byte. Ending at the
            // first ASCII letter instead ate the first letter of the text
            // after an unterminated colour sequence. ECMA-48 also allows
            // intermediate bytes (0x20-0x2f) before the final one, but in
            // log text a space after the parameters means a broken
            // sequence followed by prose — `\x1b[38;2;1;2 unterminated`
            // is not a request to paint nothing and print `nterminated`.
            Some('[') => {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    if ('\u{30}'..='\u{3f}').contains(&nc) {
                        chars.next();
                        continue;
                    }
                    if ('\u{40}'..='\u{7e}').contains(&nc) {
                        chars.next();
                    }
                    // Anything else never terminated the sequence: leave
                    // it for the outer loop to keep as text.
                    break;
                }
            }
            // OSC: a string parameter (a hyperlink, a window title) that
            // runs to BEL or to the ST pair `ESC \`.
            Some(']') => {
                chars.next();
                for nc in chars.by_ref() {
                    if nc == '\x07' {
                        break;
                    }
                    if nc == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // A bare ESC, or one of the two-character forms (charset
            // select and friends): drop the ESC and keep what follows.
            _ => {}
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

/// The nearest char boundary at or below `i`, `i` clamped to the string.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The nearest char boundary at or above `i`, `i` clamped to the string.
fn ceil_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn rich_colorize(line: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pos = 0;

    while pos < line.len() {
        if let Some(m) = next_pattern(&line[pos..]) {
            // Belt and braces. A pattern that reported a start or an end
            // inside a multi-byte character would panic the whole process
            // on the slices below — `pando logs` and the TUI with it — so
            // every offset is widened to the character that contains it
            // before it is used. `find_pattern` is careful, but it is one
            // pattern away from not being.
            let rest = &line[pos..];
            let start = floor_boundary(rest, m.start);
            let end = ceil_boundary(rest, m.end).max(start);
            if start > 0 {
                spans.push(Span::styled(
                    line[pos..pos + start].to_string(),
                    Style::new().fg(text_dim()),
                ));
            }
            spans.push(Span::styled(
                line[pos + start..pos + end].to_string(),
                m.style,
            ));
            // Every pattern ends at least one byte in, so this makes
            // progress and the loop terminates.
            if end == 0 {
                break;
            }
            pos += end;
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

/// How far into the rest of a line the search for its next pattern looks
/// first.
const FIRST_PATTERN_WINDOW: usize = 64;

/// The earliest pattern in `s`, searched for in windows that double from
/// [`FIRST_PATTERN_WINDOW`] until one holds a match or the whole of `s`.
///
/// A search costs about what lies before the match rather than the rest
/// of the line: every token used to scan to the end, so colouring a long
/// line dense with URLs or status codes cost the square of its length.
fn next_pattern(s: &str) -> Option<PatternMatch> {
    let mut limit = FIRST_PATTERN_WINDOW;
    loop {
        let found = find_pattern(s, limit);
        if found.is_some() || limit >= s.len() {
            return found;
        }
        limit = limit.saturating_mul(2);
    }
}

/// Where `needle` first occurs in `s`, when that is before `limit`. Only
/// the bytes an occurrence starting there could cover are searched.
fn find_before(s: &str, needle: &str, limit: usize) -> Option<usize> {
    let end = ceil_boundary(s, limit.saturating_add(needle.len()));
    s[..end].find(needle).filter(|&idx| idx < limit)
}

/// The earliest pattern in `s`, when it starts before `limit`; none when
/// it starts at or after it. What each pattern matches is decided on the
/// whole of `s`, so a window that holds the match finds the one a search
/// of the whole would.
fn find_pattern(s: &str, limit: usize) -> Option<PatternMatch> {
    let mut best: Option<PatternMatch> = None;

    // HTTP methods
    for method in &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        if let Some(idx) = find_before(s, method, limit) {
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
        while i < b.len().min(limit) {
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
    while i + 2 < s.len() && i < limit {
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
                if let Some(style) = style {
                    if best.as_ref().is_none_or(|b| i < b.start) {
                        best = Some(PatternMatch {
                            start: i,
                            end: i + 3,
                            style,
                        });
                    }
                    // Scanned from the left, the first code is the
                    // earliest one.
                    break;
                }
            }
        }
        i += 1;
    }

    // URLs (http:// or https://)
    for prefix in &["https://", "http://"] {
        if let Some(idx) = find_before(s, prefix, limit) {
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
    if let Some(idx) = find_before(s, "src/", limit) {
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
        // A stamp with its date starts eleven bytes before its time.
        for t in 0..s.len().saturating_sub(7).min(limit.saturating_add(11)) {
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
                //
                // Four of those eleven bytes are checked, so a shorter,
                // date-*ish* token (`21-09-26T`) preceded by a multi-byte
                // character lands `t - 11` inside that character. Slicing
                // there panics, and a binary blob in a log is enough to
                // produce one through `from_utf8_lossy`. When the eleven
                // bytes back are not a character boundary, this is not a
                // date prefix: colour the bare time instead.
                let has_date_prefix = t >= 11
                    && b[t - 1] == b'T'
                    && b[t - 2].is_ascii_digit()
                    && b[t - 4] == b'-'
                    && b[t - 7] == b'-'
                    && s.is_char_boundary(t - 11);
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
                    if ts_start < limit && best.as_ref().is_none_or(|b| ts_start < b.start) {
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
    if let Some(idx) = find_before(s, "    at ", limit)
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

/// Byte budget per buffered line for the first read of an existing file,
/// and for a poll that finds more than that unread. A tailer only ever
/// displays the last `capacity` lines, so it seeks to `capacity * this`
/// bytes before EOF instead of parsing the whole file — dev logs grow to
/// hundreds of MB and the polls run on the UI thread.
const MAX_INITIAL_BYTES_PER_LINE: u64 = 512;

/// The most a read of the last lines grows its window to while it holds
/// too few whole lines: room for a final line a dev server dumps whole — a
/// large JSON response, a state object — and small enough that a log which
/// is one giant line cannot stall a poll on the UI thread.
const MAX_INITIAL_WINDOW: u64 = 8 * 1024 * 1024;

/// The most bytes a read of the last `capacity` lines spans, and so the
/// longest line an unbounded poll of a tail of that capacity holds: a line
/// longer than this is the fragment a window starting inside it would drop.
fn window_ceiling(capacity: usize) -> u64 {
    (capacity as u64)
        .saturating_mul(MAX_INITIAL_BYTES_PER_LINE)
        .max(MAX_INITIAL_WINDOW)
}

/// How many of its reads a bounded poll holds one unfinished line across:
/// past that it keeps what it holds and drops the rest up to the newline.
/// `logs -f` reads a megabyte at a time, so it holds a line of up to
/// 256 MiB whole: far longer than any response a dev server dumps on one
/// line, and a limit on a progress line redrawn with `\r` for hours.
const MAX_READS_PER_LINE: u64 = 256;

/// Where a read of the last lines of a file of `size` bytes starts, no
/// earlier than `from`, and whether that is partway through a line. The
/// first read of a file starts from 0; a tail that fell behind starts
/// from where it stopped.
///
/// The window starts `capacity` × [`MAX_INITIAL_BYTES_PER_LINE`] bytes
/// before the end and doubles while it holds fewer than `capacity` whole
/// lines, up to [`MAX_INITIAL_WINDOW`]: `logs -n 50` of a log whose lines
/// are longer than the budget still prints fifty, and a log that ends in
/// a line longer than the window still shows that line. Newlines are
/// counted, never parsed, and each byte is counted once.
fn window_start(
    file: &mut File,
    from: u64,
    size: u64,
    capacity: usize,
) -> std::io::Result<(u64, bool)> {
    let first = (capacity as u64).saturating_mul(MAX_INITIAL_BYTES_PER_LINE);
    if size - from <= first {
        return Ok((from, false));
    }
    let ceiling = window_ceiling(capacity);
    // `capacity` whole lines take one newline more: the first one ends the
    // line the window starts partway through.
    let wanted = (capacity as u64).saturating_add(1);
    let mut start = size - first;
    let mut newlines = count_newlines(file, start, size)?;
    while newlines < wanted && start > from && size - start < ceiling {
        let next = size - (2 * (size - start)).min(ceiling).min(size - from);
        newlines += count_newlines(file, next, start)?;
        start = next;
    }
    if start == from {
        return Ok((from, false));
    }
    // A newline just before the window means it starts on a line, and that
    // line is whole. One that cannot be read is taken as a partial line:
    // dropping a whole line is safer than showing a fragment.
    let mut before = [0u8; 1];
    file.seek(SeekFrom::Start(start - 1))?;
    let partial = file.read_exact(&mut before).is_err() || before[0] != b'\n';
    Ok((start, partial))
}

/// How many newlines `file` holds in `from..to`.
fn count_newlines(file: &mut File, from: u64, to: u64) -> std::io::Result<u64> {
    file.seek(SeekFrom::Start(from))?;
    let mut range = Read::by_ref(file).take(to - from);
    let mut buf = vec![0u8; 64 * 1024];
    let mut count = 0;
    loop {
        let read = range.read(&mut buf)?;
        if read == 0 {
            return Ok(count);
        }
        count += buf[..read].iter().filter(|&&b| b == b'\n').count() as u64;
    }
}

/// The most lines a new tail reserves room for before it has read any:
/// past this the buffer grows as lines arrive, up to its capacity.
const PREALLOCATE_LINES: usize = 4096;

/// How many bytes at the end of `raw` are the start of a UTF-8 character
/// whose rest has not been written yet.
///
/// A process writing to a file rather than a terminal flushes in blocks,
/// and a block boundary lands inside a multi-byte character as readily as
/// anywhere else. Decoding each read on its own turned every such
/// character into two replacement characters; holding its first bytes
/// back until the next read decodes it whole.
fn incomplete_utf8_suffix(raw: &[u8]) -> usize {
    // A character is at most four bytes, so an unfinished one started in
    // the last three.
    let from = raw.len().saturating_sub(3);
    for start in (from..raw.len()).rev() {
        // Continuation bytes are 0b10xx_xxxx; anything else starts one.
        if raw[start] & 0xC0 != 0x80 {
            return match std::str::from_utf8(&raw[start..]) {
                // `error_len` is `None` exactly when the input ended
                // partway through a character that could still be valid.
                Err(e) if e.error_len().is_none() => raw.len() - start,
                _ => 0,
            };
        }
    }
    0
}

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

/// How many bytes before `offset` a tail remembers, to prove on the next
/// poll that the file was appended to rather than rewritten underneath it.
const ANCHOR_BYTES: usize = 64;

/// What makes this the same file: a new inode (or device, or creation time
/// where the platform records one) is a different file behind one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    created: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            created: meta.created().ok(),
        }
    }
}

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
    /// Whether the line being read grew past its ceiling, so the rest of it
    /// is dropped up to its newline rather than held. What `leftover` still
    /// holds of it is pushed as the line when that newline comes.
    overlong: bool,
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
    /// The file this tail read `offset` bytes of, as of the last poll.
    /// `None` until the first successful one.
    identity: Option<FileIdentity>,
    /// The last [`ANCHOR_BYTES`] bytes before `offset`, as this tail read
    /// them. Logs are appended to, so bytes before `offset` never change —
    /// unless the file was rewritten.
    anchor: Vec<u8>,
    /// Whether the last poll left bytes it saw in the file unread, other
    /// than the start of a character that has not been written whole.
    unread: bool,
}

impl LogTail {
    pub fn new(path: PathBuf, capacity: usize) -> Self {
        assert!(capacity > 0, "LogTail capacity must be > 0");
        Self {
            path,
            // A bound, not a size: `logs -n` takes any number, and
            // reserving `usize::MAX` lines up front panics.
            buffer: VecDeque::with_capacity(capacity.min(PREALLOCATE_LINES)),
            capacity,
            offset: 0,
            leftover: String::new(),
            line_start_offset: 0,
            overlong: false,
            evicted_levels: Vec::new(),
            lines_seen: 0,
            block_id_counter: 0,
            open_block: None,
            identity: None,
            anchor: Vec::new(),
            unread: false,
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

    /// The file this tail is following.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Levels of lines evicted by the most recent `poll`, oldest first.
    pub fn evicted_levels(&self) -> &[LogLevel] {
        &self.evicted_levels
    }

    /// How many lines this tail has ever read, across evictions and
    /// truncations. A follower prints `lines_seen() - printed_so_far` from
    /// the back of the buffer and is never silently left behind. Lines an
    /// unbounded poll skips without reading — all of a long gap but its
    /// last window — are not counted, and none of them is in the buffer.
    pub fn lines_seen(&self) -> u64 {
        self.lines_seen
    }

    /// Whether the last poll stopped short of the end of the file, so the
    /// next one reads more at once. Only a bounded poll stops short.
    pub fn has_unread(&self) -> bool {
        self.unread
    }

    pub fn poll(&mut self) -> Result<bool> {
        self.read_on(None)
    }

    /// [`poll`](Self::poll) for a follower, which prints every line: it
    /// reads on from where the last poll stopped, at most `max_bytes` and
    /// at most `capacity` whole lines, so no line is evicted before the
    /// follower has seen it and a backlog is never held in memory whole.
    /// The rest stays in the file, and [`has_unread`](Self::has_unread)
    /// says so. A line longer than [`MAX_READS_PER_LINE`] reads is cut to
    /// that.
    ///
    /// Only after a first [`poll`](Self::poll): this never skips to the
    /// end, which is how that one shows only the last lines.
    pub fn poll_bounded(&mut self, max_bytes: u64) -> Result<bool> {
        self.read_on(Some(max_bytes))
    }

    fn read_on(&mut self, bound: Option<u64>) -> Result<bool> {
        self.evicted_levels.clear();
        self.unread = false;
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(format!("open log {}", self.path.display()))
                );
            }
        };

        let meta = file
            .metadata()
            .with_context(|| format!("stat log {}", self.path.display()))?;
        let size = meta.len();
        let identity = FileIdentity::of(&meta);

        // A log is rewritten under a tail far more often than it is
        // truncated to something shorter: `start` empties it in place and
        // the new run can pass the old size before the next poll. Reading
        // on from the stale offset then dropped the head of the new
        // content and spliced a fragment of it onto a line that no longer
        // exists. Three signals, cheapest first:
        //
        // - the file is shorter than what was read of it;
        // - a different file is behind the path (a rotation, a recreate);
        // - the bytes just before `offset` are no longer the bytes this
        //   tail read there — the only one that catches an in-place
        //   truncate, where inode and creation time are both unchanged.
        //
        // A rewrite that lands on exactly the old size between two polls
        // is not detectable this way and is left alone; the anchor is only
        // consulted when there is something new to read.
        let rewritten = size < self.offset
            || self.identity.is_some_and(|known| known != identity)
            || (size > self.offset && !self.anchor_still_matches(&mut file));
        self.identity = Some(identity);
        if rewritten {
            self.offset = 0;
            self.leftover.clear();
            self.overlong = false;
            self.line_start_offset = 0;
            self.open_block = None;
            self.anchor.clear();
        }
        let mut skip_partial_first_line = false;
        // An unbounded poll keeps only the last `capacity` lines, so it
        // reads no more of the file than a first read would. A tail left
        // unpolled — another row selected, the viewer open, the TUI
        // suspended — read the whole gap into memory on the UI thread to
        // keep the end of it. A bounded poll never skips: it reads from
        // the start of a file that was rewritten, and every line is new
        // to a follower.
        if bound.is_none() {
            let (start, partial) = window_start(&mut file, self.offset, size, self.capacity)
                .with_context(|| format!("read log {}", self.path.display()))?;
            if start > self.offset {
                if self.offset > 0 {
                    // What is buffered lies before lines that are never
                    // read, so it goes, as reading them would have pushed
                    // it out.
                    self.evicted_levels
                        .extend(self.buffer.drain(..).map(|line| line.level));
                    self.leftover.clear();
                    self.overlong = false;
                    self.open_block = None;
                    self.anchor.clear();
                }
                self.offset = start;
                self.line_start_offset = start;
                skip_partial_first_line = partial;
            }
        }
        if size == self.offset {
            return Ok(false);
        }

        let seek_pos = self.offset;
        file.seek(SeekFrom::Start(self.offset))
            .context("seek log")?;
        let mut raw = Vec::new();
        // At least a character's four bytes, or a bound read might never
        // hold a whole one and never move on.
        Read::by_ref(&mut file)
            .take(bound.map_or(u64::MAX, |max| max.max(4)))
            .read_to_end(&mut raw)
            .context("read log")?;
        // Left in the file, to be read again whole on the next poll.
        let unfinished = incomplete_utf8_suffix(&raw);
        raw.truncate(raw.len() - unfinished);
        if bound.is_some() {
            // Each newline read completes one line, and more than the
            // buffer holds would evict some before a follower saw them.
            if let Some((last, _)) = raw
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .nth(self.capacity - 1)
            {
                raw.truncate(last + 1);
            }
        }
        self.offset += raw.len() as u64;
        // A character still being written at the end of the file cannot be
        // read yet, so it is not unread: a follower that read on at once
        // would spin on it until the writer finished it, or for ever if the
        // writer died partway through.
        self.unread = self.offset + (unfinished as u64) < size;
        self.remember_anchor(&raw);
        let mut chunk = String::from_utf8_lossy(&raw).into_owned();
        let mut cut = false;
        if skip_partial_first_line || self.overlong {
            chunk = match chunk.find('\n') {
                Some(newline) => {
                    self.overlong = false;
                    // A bounded poll's line ends here, cut to what it held.
                    if !self.leftover.is_empty() {
                        let held = std::mem::take(&mut self.leftover);
                        self.push_line(&held, self.line_start_offset);
                        cut = true;
                    }
                    self.line_start_offset = seek_pos + newline as u64 + 1;
                    chunk[newline + 1..].to_string()
                }
                None => {
                    // What is held still starts where its line does.
                    if self.leftover.is_empty() {
                        self.line_start_offset = self.offset;
                    }
                    String::new()
                }
            };
        }

        if chunk.is_empty() {
            return Ok(cut);
        }

        // Only the new bytes are searched for the end of a line: `leftover`
        // holds no newline, and splitting it again with every read made
        // each poll cost as much as the unfinished line had grown.
        let Some(last) = chunk.rfind('\n') else {
            self.leftover.push_str(&chunk);
            self.cap_leftover(bound);
            return Ok(cut);
        };
        let mut combined = std::mem::take(&mut self.leftover);
        combined.push_str(&chunk[..last]);
        self.leftover = chunk[last + 1..].to_string();
        self.cap_leftover(bound);
        let parts: Vec<&str> = combined.split('\n').collect();

        let added = !parts.is_empty();
        let mut cursor = self.line_start_offset;
        // Lines that could not survive this very poll — more arrived than
        // the buffer holds — are counted but never parsed: parsing is the
        // whole cost of opening a large log, and they would be evicted
        // before anything could show them. Everything already buffered
        // goes with them, as pushing would have evicted it.
        let unseen = parts.len().saturating_sub(self.capacity);
        if unseen > 0 {
            self.evicted_levels
                .extend(self.buffer.drain(..).map(|line| line.level));
            self.open_block = None;
            for line in &parts[..unseen] {
                cursor += line.len() as u64 + 1;
            }
            self.lines_seen += unseen as u64;
        }
        for line in parts.into_iter().skip(unseen) {
            self.push_line(line, cursor);
            cursor += line.len() as u64 + 1;
        }
        self.line_start_offset = cursor;
        Ok(added)
    }

    /// Stops holding the unfinished line once it is longer than its
    /// ceiling, and drops the rest of it up to its newline as it arrives. A
    /// process that redraws a progress line with `\r` never ends that line,
    /// and holding it whole grew the tail for as long as the process ran.
    ///
    /// An unbounded poll drops the line whole past [`window_ceiling`], as a
    /// window starting inside it would. A bounded poll keeps the first
    /// [`MAX_READS_PER_LINE`] reads of it and pushes those as the line when
    /// its newline comes: a follower prints every line, and one dropped
    /// here went missing from `logs -f` without a sign.
    fn cap_leftover(&mut self, bound: Option<u64>) {
        let ceiling = match bound {
            None => window_ceiling(self.capacity),
            Some(max) => max.saturating_mul(MAX_READS_PER_LINE),
        };
        if self.leftover.len() as u64 <= ceiling {
            return;
        }
        self.overlong = true;
        if bound.is_none() {
            self.leftover = String::new();
            return;
        }
        let mut end = ceiling as usize;
        while !self.leftover.is_char_boundary(end) {
            end -= 1;
        }
        self.leftover.truncate(end);
    }

    /// Whether the bytes before `offset` are still the ones this tail read
    /// there. A read that cannot be made at all counts as a rewrite: the
    /// safe direction is to start over rather than to splice.
    fn anchor_still_matches(&self, file: &mut File) -> bool {
        if self.anchor.is_empty() {
            return true;
        }
        let Some(at) = self.offset.checked_sub(self.anchor.len() as u64) else {
            return false;
        };
        let mut seen = vec![0u8; self.anchor.len()];
        if file.seek(SeekFrom::Start(at)).is_err() {
            return false;
        }
        if file.read_exact(&mut seen).is_err() {
            return false;
        }
        seen == self.anchor
    }

    /// Keeps the last [`ANCHOR_BYTES`] bytes read, which always end at
    /// `offset`.
    fn remember_anchor(&mut self, read: &[u8]) {
        self.anchor.extend_from_slice(read);
        if self.anchor.len() > ANCHOR_BYTES {
            let extra = self.anchor.len() - ANCHOR_BYTES;
            self.anchor.drain(..extra);
        }
    }

    /// Lets go of every buffered line but the newest `keep`, for a reader
    /// that copies lines out as each poll brings them: the next poll still
    /// knows where this one ended, and nothing is held twice.
    pub fn keep_newest(&mut self, keep: usize) {
        let extra = self.buffer.len().saturating_sub(keep);
        self.buffer.drain(..extra);
    }

    /// Pushes the unterminated final line, when there is one, as a line.
    /// Whether it pushed anything.
    ///
    /// A follower must never call this: more of that line is still coming,
    /// and pushing it now would print it again when its newline arrives.
    /// A one-shot read has no "still coming" — the file is what it is,
    /// which is what `tail` does and what [`snapshot`] has always done.
    /// Without it, `pando logs` withheld a crash's last words for want of
    /// a newline while the failure classifier, which reads through
    /// `snapshot`, could see them.
    pub fn flush_pending(&mut self) -> bool {
        if self.leftover.is_empty() {
            return false;
        }
        let leftover = std::mem::take(&mut self.leftover);
        let offset = self.line_start_offset;
        self.push_line(&leftover, offset);
        true
    }

    fn push_line(&mut self, raw: &str, file_offset: u64) {
        if self.buffer.len() == self.capacity
            && let Some(evicted) = self.buffer.pop_front()
        {
            self.evicted_levels.push(evicted.level);
        }
        // A CRLF log's lines end in `\r` once split on `\n`: carried into
        // `--json` as text, and onto the screen as a control character.
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
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

/// How many log lines pando reads to explain why a process failed. One
/// budget for every reader, so `status` and `doctor` explain a failure from
/// the same lines.
pub const FAILURE_TAIL_LINES: usize = 40;

pub fn snapshot(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    let mut tail = LogTail::new(path.to_path_buf(), max_lines);
    tail.poll()?;
    tail.flush_pending();
    Ok(tail.lines().iter().map(|l| l.plain.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    // A stack trace's `\tat` lines kept no indent at all: ratatui drops
    // the tab as a control character.
    #[test]
    fn a_tab_is_drawn_as_the_spaces_to_the_next_stop() {
        let line = parse_line("Exception\n".trim_end(), 0);
        assert_eq!(line.plain, "Exception");
        let line = parse_line("\tat com.Foo(Foo.java:1)", 0);
        assert_eq!(line.plain, "        at com.Foo(Foo.java:1)");
        let drawn: String = line
            .styled
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(drawn, line.plain);
        assert_eq!(expand_tabs("ab\tc"), "ab      c");
        assert_eq!(
            expand_tabs("\x1b[31mab\x1b[0m\tc"),
            "\x1b[31mab\x1b[0m      c",
            "escape sequences take no column"
        );
    }

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

    // The first read was a fixed `capacity * 512` bytes, so `logs -n 50`
    // of a log with 1 KB lines printed about 25.
    #[test]
    fn the_first_poll_reads_as_many_long_lines_as_it_has_room_for() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("json.log");
        let lines: Vec<String> = (0..200).map(|i| format!("{i:0>1000}")).collect();
        write_all(&path, &(lines.join("\n") + "\n"));

        let mut tail = LogTail::new(path, 50);
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail), lines[150..].to_vec());
    }

    // A last line longer than the whole window left nothing after the
    // partial first line was dropped, and `logs` said a log of many lines
    // held "not one complete line yet".
    #[test]
    fn the_first_poll_reads_a_last_line_longer_than_its_window() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dump.log");
        let dump = "{".to_string() + &"x".repeat(30_000) + "}";
        write_all(&path, &format!("{}{dump}\n", "short\n".repeat(1000)));

        let mut tail = LogTail::new(path, 50);
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail).last(), Some(&dump));
        assert_eq!(tail.lines().len(), 50);
    }

    // A follower's poll read everything since the last one at once, and a
    // backlog longer than the buffer — a pager that stopped reading, a
    // Ctrl-Z — lost its oldest lines without a word.
    #[test]
    fn bounded_polls_read_a_backlog_longer_than_the_buffer_line_by_line() {
        for bound in [u64::MAX, 5] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("log.txt");
            write_all(&path, "start\n");
            let mut tail = LogTail::new(path.clone(), 4);
            tail.poll().unwrap();
            let backlog: Vec<String> = (0..10).map(|i| format!("line-{i}")).collect();
            append(&path, &(backlog.join("\n") + "\n"));

            let mut printed = Vec::new();
            let mut seen = tail.lines_seen();
            loop {
                tail.poll_bounded(bound).unwrap();
                let fresh = (tail.lines_seen() - seen) as usize;
                seen = tail.lines_seen();
                let skip = tail.lines().len() - fresh;
                printed.extend(plain_lines(&tail).into_iter().skip(skip));
                if !tail.has_unread() {
                    break;
                }
            }
            assert_eq!(printed, backlog, "reading at most {bound} bytes at once");
        }
    }

    // A tail left unpolled — another row selected, the viewer open, the
    // TUI suspended — read the whole gap into memory on the UI thread to
    // keep the last few lines of it.
    #[test]
    fn a_tail_that_fell_far_behind_reads_only_the_end_of_the_gap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "old\n");
        let mut tail = LogTail::new(path.clone(), 4);
        tail.poll().unwrap();
        let before = tail.lines_seen();

        let gap: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        append(&path, &gap);
        tail.poll().unwrap();
        assert_eq!(
            plain_lines(&tail),
            ["line 9996", "line 9997", "line 9998", "line 9999"]
        );
        let read = tail.lines_seen() - before;
        assert!(
            read < 1_000,
            "only the end of the gap is read: {read} lines"
        );
        assert_eq!(tail.evicted_levels().len(), 1, "the old line goes");

        // It reads on from there like any other poll, not as if the file
        // had been rewritten.
        append(&path, "next\n");
        tail.poll().unwrap();
        assert_eq!(tail.lines_seen() - before, read + 1);
        assert_eq!(plain_lines(&tail).last().unwrap(), "next");
    }

    // A window that starts right after a newline starts on a whole line.
    // It was dropped as if it were a fragment, and a log of two lines
    // showed neither.
    #[test]
    fn a_first_window_that_starts_on_a_line_keeps_that_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("exact.log");
        // Two lines, the second exactly the window for a capacity of one.
        let second = "y".repeat(MAX_INITIAL_BYTES_PER_LINE as usize - 1);
        write_all(&path, &format!("first\n{second}\n"));

        let mut tail = LogTail::new(path, 1);
        assert!(tail.poll().unwrap());
        assert_eq!(plain_lines(&tail), vec![second]);
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

    // A progress bar redrawn with `\r` never ends its line. Every poll
    // held the whole of it and split it again, so the tail grew, and each
    // poll on the UI thread took longer, for as long as the process ran.
    #[test]
    fn an_unfinished_line_is_held_no_longer_than_a_window_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("progress.log");
        write_all(&path, "start\n");
        let mut tail = LogTail::new(path.clone(), 4);
        tail.poll().unwrap();

        let redraw = "\r".to_string() + &"x".repeat(1024 * 1024 - 1);
        for _ in 0..9 {
            append(&path, &redraw);
            tail.poll().unwrap();
            assert!(
                tail.leftover.len() as u64 <= MAX_INITIAL_WINDOW,
                "held {} bytes of one line",
                tail.leftover.len()
            );
        }
        // The line that outgrew it goes whole, as the fragment a window
        // starting inside it would; what follows is read as ever, at its
        // own offset.
        append(&path, "\nafter\n");
        tail.poll().unwrap();
        assert_eq!(plain_lines(&tail), ["start", "after"]);
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(tail.lines().back().unwrap().file_offset, size - 6);
    }

    // `logs -f` reads on a megabyte at a time. A line longer than a
    // window of the last lines spans was held across those reads until
    // it passed the ceiling, then dropped: nothing printed, nothing
    // counted, and a follower reading `--json` had no sign of the gap.
    #[test]
    fn a_follower_prints_a_line_longer_than_a_window_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dump.log");
        write_all(&path, "start\n");
        let mut tail = LogTail::new(path.clone(), 4);
        tail.poll().unwrap();

        let long = "x".repeat(MAX_INITIAL_WINDOW as usize + 1024 * 1024);
        append(&path, &long);
        append(&path, "\nafter\n");
        loop {
            tail.poll_bounded(1024 * 1024).unwrap();
            if !tail.has_unread() {
                break;
            }
        }
        assert_eq!(tail.lines_seen(), 3);
        let lines: Vec<_> = tail.lines().iter().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].plain.len(), long.len(), "the long line whole");
        assert_eq!(lines[1].file_offset, 6);
        assert_eq!(lines[2].plain, "after");
    }

    // A follower held a line that never ends whole, so `logs -f` beside a
    // progress bar redrawn with `\r` grew for as long as the process ran,
    // and printed all of it at once when a newline finally came.
    #[test]
    fn a_follower_holds_no_more_of_a_line_than_its_reads_allow() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("progress.log");
        write_all(&path, "start\n");
        let mut tail = LogTail::new(path.clone(), 4);
        tail.poll().unwrap();

        let read = 4096;
        let ceiling = (read * MAX_READS_PER_LINE) as usize;
        // Redraws of one read each, after a byte, each ending in a
        // character of two bytes: the ceiling lands inside one.
        append(&path, "\r");
        let redraw = "x".repeat(read as usize - 2) + "é";
        for _ in 0..MAX_READS_PER_LINE + 16 {
            append(&path, &redraw);
            loop {
                tail.poll_bounded(read).unwrap();
                if !tail.has_unread() {
                    break;
                }
            }
            assert!(
                tail.leftover.len() <= ceiling,
                "held {} bytes of one line",
                tail.leftover.len()
            );
        }
        assert_eq!(tail.lines_seen(), 1, "nothing printed while it runs");

        // It is printed as far as it was held, and what follows is read as
        // ever, at its own offset.
        append(&path, "\nafter\n");
        loop {
            tail.poll_bounded(read).unwrap();
            if !tail.has_unread() {
                break;
            }
        }
        assert_eq!(tail.lines_seen(), 3);
        let lines: Vec<_> = tail.lines().iter().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].plain.len() <= ceiling);
        assert!(lines[1].plain.len() > ceiling - 4, "cut at the ceiling");
        assert_eq!(lines[1].file_offset, 6);
        assert_eq!(lines[2].plain, "after");
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(lines[2].file_offset, size - 6);
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

    // A date-*ish* token eleven bytes after a multi-byte character used to
    // put the timestamp match's start inside that character, and
    // `rich_colorize` sliced the line there — panicking the whole process,
    // `pando logs` and the TUI alike. Every shape in the review's table,
    // including the one a binary blob produces through `from_utf8_lossy`.
    /// Every span of a line, joined back into the text it paints.
    fn painted(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    // `pando start` empties the log in place (`fs::write`, so the same
    // inode) and the new run can write past the old size before the next
    // 250 ms poll. Detecting truncation only by `size < offset` then
    // resumed at a stale byte offset: the head of the new content was
    // silently dropped and a fragment of it spliced onto the last old
    // line.
    #[test]
    fn a_log_rewritten_in_place_between_polls_is_reread_from_the_start() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("dev.log");
        let old: String = (0..10).map(|n| format!("old line {n}\n")).collect();
        write_all(&path, &old);
        let mut tail = LogTail::new(path.clone(), 200);
        tail.poll().unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        let new: String = (0..60).map(|n| format!("brand new line {n}\n")).collect();
        assert!(new.len() > old.len(), "the new run outgrows the old file");
        write_all(&path, &new);
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            before,
            "an in-place truncate keeps the inode, so file identity alone \
             cannot see this one"
        );
        tail.poll().unwrap();

        let lines = plain_lines(&tail);
        let expected: Vec<String> = (0..60).map(|n| format!("brand new line {n}")).collect();
        assert_eq!(
            &lines[10..],
            expected.as_slice(),
            "every new line, in order, with no spliced fragment"
        );
        assert_eq!(lines.len(), 70, "and the old ones kept above them");
    }

    // The other shape of the same thing: the path is the same and the file
    // behind it is not. Its first bytes match, so only the inode says so.
    #[test]
    fn a_log_replaced_by_a_different_file_is_reread_from_the_start() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("dev.log");
        let old: String = (0..10).map(|n| format!("old line {n}\n")).collect();
        write_all(&path, &old);
        let mut tail = LogTail::new(path.clone(), 200);
        tail.poll().unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        // Written beside the old one and renamed over it, the way a log is
        // rotated: removing the old file first lets ext4 hand the new one
        // the same inode at once.
        let rotated: String = (0..30).map(|n| format!("old line {n}\n")).collect();
        let fresh = dir.path().join("dev.log.new");
        write_all(&fresh, &rotated);
        std::fs::rename(&fresh, &path).unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            before,
            "a recreated file is a different inode"
        );
        tail.poll().unwrap();

        assert_eq!(
            plain_lines(&tail).len(),
            40,
            "the new file is read from its start, not appended to blindly"
        );
    }

    // An OSC 8 hyperlink carries no `ESC [` at all, so the line was not
    // classified as ANSI and `strip_ansi` dropped the lone ESC and left the
    // payload on screen as text.
    #[test]
    fn osc_sequences_are_stripped_and_never_reach_the_screen() {
        for raw in [
            // ST-terminated (ESC \) and BEL-terminated hyperlinks.
            "\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\",
            "\x1b]8;;https://example.com\x07click\x1b]8;;\x07",
        ] {
            let parsed = parse_plain(raw);
            assert_eq!(parsed.plain, "click", "{raw:?}");
            assert!(parsed.has_ansi, "an OSC line is an ANSI line: {raw:?}");
            assert_eq!(painted(&parsed.styled), parsed.plain, "{raw:?}");
        }
    }

    // `strip_ansi` ended a CSI at the first ASCII letter, so an
    // unterminated colour sequence ate the first letter of the text after
    // it. A CSI ends at its final byte, and a byte that can be neither a
    // parameter nor a final byte means the sequence was never one.
    #[test]
    fn an_unterminated_csi_does_not_eat_the_next_character() {
        let parsed = parse_plain("\x1b[38;2;1;2 unterminated");
        assert_eq!(parsed.plain, " unterminated");
        assert_eq!(painted(&parsed.styled), parsed.plain);
    }

    // Whatever the shape, no escape byte may reach the buffer, and the
    // styled spans have to join back to `plain` byte for byte — the
    // viewer's search highlighting maps offsets found in one onto the
    // other.
    #[test]
    fn malformed_escape_sequences_leave_no_escape_byte_and_stay_in_lockstep() {
        for raw in [
            "\x1b]8;;https://example.com",     // unterminated OSC
            "plain text then an escape\x1b",   // a bare ESC at the end
            "\x1b[38;5;2mcoloured\x1b[0m end", // a well-formed one
            "\x1b(Bcharset select",
            "\x1b[?25lhidden cursor",
            "\x1b[1;2;3",
        ] {
            let parsed = parse_plain(raw);
            assert!(
                !parsed.plain.contains('\x1b'),
                "an escape byte survived into {:?} from {raw:?}",
                parsed.plain
            );
            assert_eq!(
                painted(&parsed.styled),
                parsed.plain,
                "spans and plain must agree for {raw:?}"
            );
        }
    }

    #[test]
    fn a_date_ish_token_after_a_multibyte_character_does_not_panic() {
        let panicky = [
            // CJK + a two-digit-year ISO stamp.
            "\u{65e5} 21-09-26T10:00:00 \u{8d77}\u{52d5}",
            // Emoji + a US-shaped date.
            "\u{2705} 9-21-24T10:00:00 build ok",
            // And the shapes that were always fine, so the fallback to the
            // bare time does not lose the full stamp.
            "\u{1f680} 2026-09-21T10:00:00",
            "\u{e9}2026-09-21T10:00:00",
        ];
        for raw in panicky {
            let parsed = parse_plain(raw);
            assert_eq!(parsed.plain, raw, "the text survives verbatim");
            let painted: String = parsed
                .styled
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(painted, raw, "and every span joins back to it: {raw:?}");
        }
    }

    // The same crash, reached the way a dev server really reaches it: raw
    // bytes that are not UTF-8 become U+FFFD in `poll`'s `from_utf8_lossy`,
    // and a two-digit-year stamp eleven-ish bytes later did the rest.
    #[test]
    fn invalid_utf8_before_a_two_digit_year_stamp_does_not_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("binary.log");
        // No level keyword in the text: `keyword_colorize` would otherwise
        // paint the whole line in one span and never reach the pattern
        // scanner this is about.
        std::fs::write(&path, b"\xff 21-09-26T10:00:00 hello\n").unwrap();
        let mut tail = LogTail::new(path, 10);
        tail.poll().unwrap();
        let line = &tail.lines()[0];
        assert!(
            line.plain.starts_with('\u{FFFD}'),
            "the invalid byte is replaced, not dropped: {:?}",
            line.plain
        );
        let painted: String = line
            .styled
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(painted, line.plain);
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

    fn found(m: Option<PatternMatch>) -> Option<(usize, usize, Style)> {
        m.map(|m| (m.start, m.end, m.style))
    }

    // Every token rescanned the rest of the line, so a long line dense
    // with tokens cost the square of its length. The search now looks in
    // a window first, and a window must find what the whole would.
    #[test]
    fn a_pattern_search_in_a_window_finds_what_a_search_of_the_whole_would() {
        let lines = [
            "GET /api/a 200 in 85ms",
            "GETTING there GET /x 404",
            "see src/ then src/app.ts:12 at 12:00:01",
            "2024-01-02T10:20:30.123Z POST https://example.test/a) done",
            "req 7 http://a.test/b 301 took 1.2s and 450 ms",
            "Error: boom\n    at run (src/main.ts:4:2)",
            "é21-09-26T10:00:00 ü 12:34:56Z OPTIONS",
            "no tokens in this one at all",
            "999 1234 600 250",
        ];
        for line in lines {
            let whole = found(find_pattern(line, usize::MAX));
            for limit in 0..line.len() + 12 {
                let expected = whole.filter(|(start, _, _)| *start < limit);
                assert_eq!(
                    found(find_pattern(line, limit)),
                    expected,
                    "{line:?} within {limit}"
                );
            }
            assert_eq!(found(next_pattern(line)), whole, "{line:?}");
        }
    }

    #[test]
    fn a_long_line_dense_with_tokens_colours_every_one() {
        let line = "GET /a 200 ".repeat(2000);
        let styled = rich_colorize(&line);
        let text: String = styled.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, line);
        let methods = styled
            .spans
            .iter()
            .filter(|s| s.content == "GET" && s.style.fg == Some(cyan()))
            .count();
        let codes = styled
            .spans
            .iter()
            .filter(|s| s.content == "200" && s.style.fg == Some(green()))
            .count();
        assert_eq!((methods, codes), (2000, 2000));
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

    // A block-buffered writer splits a multi-byte character across two
    // flushes as readily as anything else. Each read was decoded alone,
    // so "café" became "caf\u{FFFD}\u{FFFD}" for good.
    #[test]
    fn a_character_split_across_two_writes_is_read_whole() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        std::fs::write(&path, b"caf\xC3").unwrap();
        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"\xA9 \xE2\x82").unwrap();
        tail.poll().unwrap();
        f.write_all(b"\xAC\nnext\n").unwrap();
        tail.poll().unwrap();
        assert_eq!(plain_lines(&tail), vec!["café €", "next"]);
        assert_eq!(tail.lines()[1].file_offset, 10, "offsets are still bytes");

        // Bytes that are simply invalid are not held back for ever.
        assert_eq!(incomplete_utf8_suffix(b"ok\xFF"), 0);
        assert_eq!(incomplete_utf8_suffix(b"ok\xF0\x9F\x98"), 3);
        assert_eq!(incomplete_utf8_suffix("ok😀".as_bytes()), 0);
        assert_eq!(incomplete_utf8_suffix(b""), 0);
    }

    // The first bytes of a character held back at the end of the file
    // counted as unread, and a follower, which reads on at once while
    // there is more, re-read the file without pause until the writer
    // finished the character, or for ever if it died partway through.
    #[test]
    fn a_log_that_ends_partway_through_a_character_has_nothing_unread() {
        for bound in [u64::MAX, 5] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("log.txt");
            write_all(&path, "start\n");
            let mut tail = LogTail::new(path.clone(), 10);
            tail.poll().unwrap();
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"a\n\xE2").unwrap();

            tail.poll_bounded(bound).unwrap();
            assert!(!tail.has_unread(), "reading at most {bound} bytes at once");
            tail.poll_bounded(bound).unwrap();
            assert!(!tail.has_unread(), "reading at most {bound} bytes at once");

            f.write_all(b"\x82\xAC\n").unwrap();
            tail.poll_bounded(bound).unwrap();
            assert!(!tail.has_unread());
            assert_eq!(plain_lines(&tail), vec!["start", "a", "€"]);
        }
    }

    #[test]
    fn crlf_line_endings_leave_no_carriage_return_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "one\r\ntwo\r\nthree\r");
        let mut tail = LogTail::new(path.clone(), 10);
        tail.poll().unwrap();
        tail.flush_pending();
        assert_eq!(plain_lines(&tail), vec!["one", "two", "three"]);
        let offsets: Vec<u64> = tail.lines().iter().map(|l| l.file_offset).collect();
        assert_eq!(offsets, vec![0, 5, 10], "offsets count the \\r");
    }

    // `pando logs -n <huge>` builds a tail with that capacity.
    #[test]
    fn a_huge_capacity_is_a_bound_and_not_an_allocation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.txt");
        write_all(&path, "a\nb\n");
        let mut tail = LogTail::new(path, usize::MAX);
        tail.poll().unwrap();
        assert_eq!(plain_lines(&tail), vec!["a", "b"]);
    }
}
