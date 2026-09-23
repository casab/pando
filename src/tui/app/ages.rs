//! Durations as the TUI writes them: one compact style for an uptime and
//! a commit's age alike, and a reading of git's own relative dates so a
//! commit's age keeps counting after enrichment reported it.

/// `42s`, `3m`, `5h`, `2d`, `7w`, `4mo`, `2y`: one unit, the largest that
/// is at least one. The same for everything the detail pane counts, so an
/// uptime and a commit age read the same way side by side.
pub fn compact_age(secs: i64) -> String {
    let secs = secs.max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    match secs {
        s if s < MINUTE => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 2 * WEEK => format!("{}d", s / DAY),
        s if s < 2 * MONTH => format!("{}w", s / WEEK),
        s if s < YEAR => format!("{}mo", s / MONTH),
        s => format!("{}y", s / YEAR),
    }
}

/// How long ago git's `%cr` says, in seconds: `82 seconds ago`, `3 hours
/// ago`, `2 years, 4 months ago`. `None` for anything else, which the
/// caller shows as git wrote it.
pub fn parse_git_relative(text: &str) -> Option<i64> {
    let text = text.trim().strip_suffix(" ago")?;
    let mut total = 0i64;
    let mut any = false;
    for part in text.split(',') {
        let mut words = part.split_whitespace();
        let count: i64 = words.next()?.parse().ok()?;
        let unit = words.next()?;
        if words.next().is_some() {
            return None;
        }
        let per = match unit.trim_end_matches('s') {
            "second" => 1,
            "minute" => 60,
            "hour" => 3600,
            "day" => 86_400,
            "week" => 7 * 86_400,
            "month" => 30 * 86_400,
            "year" => 365 * 86_400,
            _ => return None,
        };
        // Checked: the text comes from git or from the enrichment cache
        // on disk, and a number past what a date can mean is no date —
        // not an overflow panic in the paint.
        total = total.checked_add(count.checked_mul(per)?)?;
        any = true;
    }
    any.then_some(total)
}
