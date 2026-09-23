//! "Did you mean": the name nearest to one that was mistyped, and a
//! config key error that says so on one line, with its file.

use std::path::Path;

/// Levenshtein distance, by characters.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != cb);
            row[j + 1] = substitute.min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

/// The one of `options` a typo or two from `typed`, when there is one: a
/// typo in a short word, a few more in a long one — two for a five-letter
/// one, since a swapped pair (`prots`) is two. The nearest wins; a tie
/// goes to the first listed.
pub fn closest<'a>(typed: &str, options: &[&'a str]) -> Option<&'a str> {
    let allowed = typed.chars().count().div_ceil(3).clamp(1, 3);
    options
        .iter()
        .map(|option| (edit_distance(typed, option), *option))
        .filter(|(distance, _)| *distance <= allowed)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, option)| option)
}

/// A key or a value a config table does not take, as serde found it, with
/// where it was and what was probably meant.
///
/// toml's own message puts the table on a line of its own —
/// "unknown field `prots`, expected one of …" then "in `processes.api2`" —
/// and the loader's " — in <file>" then started a third. This is the same
/// fact as one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError {
    /// "unknown field `prots`".
    pub what: String,
    /// "`processes.api2`", when toml said.
    pub table: Option<String>,
    /// "expected one of `cmd`, `ports`, …", when toml said.
    pub expected: Option<String>,
    /// The expected name nearest to the mistyped one.
    pub suggestion: Option<String>,
}

impl KeyError {
    /// Reads toml's message. `None` for anything that is not about a key
    /// or a variant a table does not take.
    pub fn parse(message: &str) -> Option<KeyError> {
        let mut lines = message.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next()?;
        let table = lines
            .find_map(|l| l.strip_prefix("in "))
            .map(str::to_string);
        let (what, expected) = match first.split_once(", expected ") {
            Some((what, expected)) => (what, Some(format!("expected {expected}"))),
            None => (first, None),
        };
        if !(what.starts_with("unknown field `") || what.starts_with("unknown variant `")) {
            return None;
        }
        let typed = what.split('`').nth(1)?;
        let options: Vec<&str> = expected
            .as_deref()
            .map(|e| e.split('`').skip(1).step_by(2).collect())
            .unwrap_or_default();
        let suggestion = closest(typed, &options).map(str::to_string);
        Some(KeyError {
            what: what.to_string(),
            table,
            expected,
            suggestion,
        })
    }

    /// One line, naming `file` when it is known.
    pub fn render(&self, file: Option<&Path>) -> String {
        let mut out = self.what.clone();
        if let Some(table) = &self.table {
            out.push_str(&format!(" in {table}"));
        }
        if let Some(file) = file {
            out.push_str(&format!(" of {}", file.display()));
        }
        if let Some(suggestion) = &self.suggestion {
            out.push_str(&format!(" — did you mean `{suggestion}`?"));
        }
        if let Some(expected) = &self.expected {
            out.push_str(&format!(" ({expected})"));
        }
        out
    }
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render(None))
    }
}

impl std::error::Error for KeyError {}
