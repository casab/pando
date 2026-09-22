//! `logs`: printing and following a worktree's logs.

use super::JSON_VERSION;
use crate::actions;
use crate::log_tail::{self, LogLevel};
use crate::paths::PandoPaths;
use crate::state::Phase;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Write;
use std::time::Duration;

// ---- logs -----------------------------------------------------------------

/// How often `-f` looks for new lines. Fast enough to feel live, slow
/// enough not to spin a core.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// Lines a follower keeps room for between two polls. Big enough that a
/// dev server's startup burst is printed in full rather than clipped to
/// whatever `--tail` happened to be.
const FOLLOW_CAPACITY: usize = 4096;

/// One line of one log, as the machine-readable shape.
///
/// The one published shape that is a *stream* rather than a document: one
/// object per line, so `pando logs -f --json | …` stays useful while it
/// runs. It carries `version` on every line for exactly that reason — a
/// consumer reading a pipe has no other object to learn the shape from,
/// and running a second command to ask is not something a follower can do.
#[derive(Serialize)]
struct LogLineOut<'a> {
    version: u32,
    /// The line's own timestamp when it has one pando can read, else null.
    ts: Option<String>,
    level: &'static str,
    line: &'a str,
}

fn level_word(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

#[allow(clippy::too_many_arguments)]
/// Prints the tail of one log, and — when there is nothing to print —
/// says so.
///
/// `notice` is where "nothing to print" goes, and it is not `out`:
/// `pando logs > file` and `pando logs --json | …` both take stdout
/// literally, so a sentence about the log cannot be on it. Printing
/// nothing and exiting 0 was the third dead end of the same first run —
/// `doctor` said the process failed and pointed here, here said nothing,
/// and there was no way to tell an empty log from a broken command.
pub fn logs<W: Write>(
    paths: &PandoPaths,
    name: &str,
    source: &str,
    tail: usize,
    follow: bool,
    json: bool,
    out: &mut W,
    notice: &dyn Fn(&str),
) -> Result<()> {
    // `source` is a path component of the file about to be read, so a
    // traversal here reads outside the worktree's log directory entirely —
    // a file `available_sources` would never list.
    crate::paths::validate_log_source("log source", source)?;
    let path = paths.log_file(name, source);
    if !path.exists() {
        let available = available_sources(paths, name);
        if available.is_empty() {
            anyhow::bail!(
                "no logs for {name} yet — `pando start {name}` writes them to {}",
                paths.logs_dir(name).display()
            );
        }
        anyhow::bail!(
            "no {source} log for {name} — this worktree has: {}",
            available.join(", ")
        );
    }
    // A one-shot read keeps only what it prints. A follower needs room for
    // whatever arrives between two polls, which has nothing to do with how
    // many lines the reader asked to see first.
    let capacity = if follow {
        tail.max(FOLLOW_CAPACITY)
    } else {
        tail.max(1)
    };
    let mut tailer = log_tail::LogTail::new(path.clone(), capacity);
    tailer
        .poll()
        .with_context(|| format!("read {}", path.display()))?;
    // A one-shot read takes the file as it stands, unterminated last line
    // and all. A follower may not: the rest of that line is still coming.
    if !follow {
        tailer.flush_pending();
    }
    let first = tailer.lines().len().saturating_sub(tail);
    let mut printed = 0usize;
    for line in tailer.lines().iter().skip(first) {
        write_log_line(out, &line.plain, line.level, json)?;
        printed += 1;
    }
    if printed == 0 {
        for line in silence_notes(paths, name, source, &path) {
            notice(&line);
        }
    }
    if !follow {
        return Ok(());
    }
    // How many lines have gone past, not how many are in the buffer: the
    // buffer stops growing at `capacity`, and comparing against its length
    // is how a follower goes silent forever a few seconds into a run.
    let mut printed = tailer.lines_seen();
    // Ends on Ctrl-C, which is what `-f` means everywhere else.
    loop {
        std::thread::sleep(FOLLOW_INTERVAL);
        let grew = tailer
            .poll()
            .with_context(|| format!("read {}", path.display()))?;
        if !grew {
            continue;
        }
        let seen = tailer.lines_seen();
        // Truncation resets the file's offsets but not this count, so a
        // restarted process picks up where the follower left off.
        let fresh = seen.saturating_sub(printed) as usize;
        printed = seen;
        let lines: Vec<(String, LogLevel)> = tailer
            .lines()
            .iter()
            .skip(tailer.lines().len().saturating_sub(fresh))
            .map(|l| (l.plain.clone(), l.level))
            .collect();
        for (plain, level) in lines {
            write_log_line(out, &plain, level, json)?;
        }
        out.flush()?;
    }
}

/// Why `logs` printed nothing: what the file is, and — when the record
/// knows — what the process did.
///
/// Three states that used to be one silence. An empty file is a fact about
/// the run: the process wrote nothing, which is itself the strongest thing
/// anyone can say about a dev command that exited at once. A file with
/// bytes but no complete line is a different fact and is not pando's doing
/// either. Neither is "the command is broken", which is the only thing a
/// silent exit 0 leaves a developer free to conclude.
///
/// The second line comes from the state record, read without writing
/// anything: the process's own phase, which for a failure is the reason
/// `explain_failure` already composed. That is the sentence the developer
/// came here looking for.
pub(super) fn silence_notes(
    paths: &PandoPaths,
    name: &str,
    source: &str,
    path: &std::path::Path,
) -> Vec<String> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut out = vec![match size {
        0 => format!("the {source} log for {name} is empty — nothing was ever written to it"),
        n => format!("the {source} log for {name} holds {n} bytes and not one complete line yet"),
    }];
    // `inspect`, not `refresh`: reading a log may not signal anything or
    // write state.
    let state = actions::inspect(paths).state;
    let Some(process) = state
        .worktrees
        .get(name)
        .and_then(|record| record.processes.get(source))
    else {
        return out;
    };
    out.push(match &process.phase {
        Phase::Failed { reason, .. } => format!("{source} failed: {reason}"),
        Phase::Running { .. } => {
            format!("{source} is running and has not printed anything yet")
        }
        Phase::Starting { .. } => format!("{source} is still starting"),
    });
    out
}

fn write_log_line<W: Write>(out: &mut W, plain: &str, level: LogLevel, json: bool) -> Result<()> {
    if json {
        let entry = LogLineOut {
            version: JSON_VERSION,
            ts: leading_timestamp(plain),
            level: level_word(level),
            line: plain,
        };
        writeln!(out, "{}", serde_json::to_string(&entry)?)?;
    } else {
        writeln!(out, "{plain}")?;
    }
    Ok(())
}

/// Every log source this worktree has, from the files that exist. Nothing
/// enumerates the set: a hook adds one by writing one.
fn available_sources(paths: &PandoPaths, name: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(paths.logs_dir(name)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension()?.to_str()? == "log")
                .then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    out.sort();
    out
}

/// The timestamp a line starts with, when it has one pando can read.
///
/// Dev servers disagree about log formats, so this is deliberately narrow:
/// an RFC 3339 stamp, optionally in brackets, at the very start. Anything
/// else is `null` rather than a guess.
pub(super) fn leading_timestamp(line: &str) -> Option<String> {
    let first = line.split_whitespace().next()?;
    let trimmed = first.trim_start_matches('[').trim_end_matches(']');
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|ts| ts.with_timezone(&Utc).to_rfc3339())
}
