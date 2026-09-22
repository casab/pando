//! Reading a version out of a tool's output, and whether it satisfies a
//! spec such as `22`, `>=18 <21` or `^3.11`.

use super::normalize_spec;
use super::probe::Verdict;

/// The first `1.2.3`-shaped run in a line of version output.
///
/// One parser for every language in the table: `v22.14.0`, `Python
/// 3.11.5`, `go version go1.22.0 darwin/arm64`, `ruby 3.2.2p53 (…)` and
/// `openjdk version "21.0.1"` all answer the same way.
pub fn first_version(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|c| c.is_ascii_digit())?;
    let end = chars[start..]
        .iter()
        .position(|c| !c.is_ascii_digit() && *c != '.')
        .map(|offset| start + offset)
        .unwrap_or(chars.len());
    let version: String = chars[start..end].iter().collect();
    let version = version.trim_end_matches('.').to_string();
    (!version.is_empty()).then_some(version)
}

/// Whether a resolved version satisfies a spec.
///
/// Deliberately small: the comparators npm, pyenv and friends actually
/// write, and `Unknown` for everything else. Blocking a start on a range
/// this build only half understands would be worse than not checking.
pub fn satisfies(spec: &str, version: &str) -> Verdict {
    let Some(resolved) = parse_version(normalize_spec(version)) else {
        return Verdict::Unknown;
    };
    let mut unknown = false;
    let mut any = false;
    for alternative in spec.split("||") {
        any = true;
        match satisfies_all(alternative, &resolved) {
            Verdict::Satisfied => return Verdict::Satisfied,
            Verdict::Unknown => unknown = true,
            Verdict::Mismatch => {}
        }
    }
    match (any, unknown) {
        (false, _) | (_, true) => Verdict::Unknown,
        _ => Verdict::Mismatch,
    }
}

/// One `||` alternative: every comparator in it has to hold.
fn satisfies_all(clause: &str, resolved: &[u64]) -> Verdict {
    let comparators: Vec<&str> = clause
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .collect();
    if comparators.is_empty() {
        return Verdict::Unknown;
    }
    let mut verdict = Verdict::Satisfied;
    for comparator in comparators {
        match compare(comparator, resolved) {
            // A comparator that definitely fails fails the clause, however
            // little is understood about the others.
            Verdict::Mismatch => return Verdict::Mismatch,
            Verdict::Unknown => verdict = Verdict::Unknown,
            Verdict::Satisfied => {}
        }
    }
    verdict
}

fn compare(comparator: &str, resolved: &[u64]) -> Verdict {
    let comparator = comparator.trim();
    let (op, rest) = split_op(comparator);
    let rest = normalize_spec(rest);
    if rest.is_empty() || rest == "*" {
        // `*`, `latest`, an empty comparator: anything goes, and only when
        // nothing was asked of it.
        return match op {
            Op::Exact => Verdict::Satisfied,
            _ => Verdict::Unknown,
        };
    }
    let Some(pattern) = parse_pattern(rest) else {
        return Verdict::Unknown;
    };
    match op {
        // Every component the pattern names has to match; the ones it does
        // not name are free, which is what makes `.nvmrc` 22 satisfied by
        // 22.14.0 and not by 24.
        Op::Exact => {
            for (index, part) in pattern.iter().enumerate() {
                let Some(want) = part else { continue };
                if component(resolved, index) != *want {
                    return Verdict::Mismatch;
                }
            }
            Verdict::Satisfied
        }
        Op::Caret | Op::Tilde => {
            let Some(floor) = concrete(&pattern) else {
                return Verdict::Unknown;
            };
            let ceiling = match op {
                Op::Caret => caret_ceiling(&floor),
                _ => tilde_ceiling(&floor, pattern.len()),
            };
            if cmp_versions(resolved, &floor).is_lt() || cmp_versions(resolved, &ceiling).is_ge() {
                Verdict::Mismatch
            } else {
                Verdict::Satisfied
            }
        }
        _ => {
            let Some(against) = concrete(&pattern) else {
                return Verdict::Unknown;
            };
            let ordering = cmp_versions(resolved, &against);
            let held = match op {
                Op::Ge => ordering.is_ge(),
                Op::Gt => ordering.is_gt(),
                Op::Le => ordering.is_le(),
                Op::Lt => ordering.is_lt(),
                _ => unreachable!("every other operator is handled above"),
            };
            if held {
                Verdict::Satisfied
            } else {
                Verdict::Mismatch
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Exact,
    Ge,
    Gt,
    Le,
    Lt,
    Caret,
    Tilde,
}

fn split_op(text: &str) -> (Op, &str) {
    for (prefix, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("^", Op::Caret),
        ("~", Op::Tilde),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (op, rest.trim());
        }
    }
    (Op::Exact, text)
}

/// Components, where `x`, `X` and `*` stand for "any".
fn parse_pattern(text: &str) -> Option<Vec<Option<u64>>> {
    let mut out = Vec::new();
    for part in text.split('.') {
        if matches!(part, "x" | "X" | "*") {
            out.push(None);
            continue;
        }
        out.push(Some(part.parse::<u64>().ok()?));
    }
    (!out.is_empty()).then_some(out)
}

/// A pattern with no wildcards in it, for the comparisons that need one.
fn concrete(pattern: &[Option<u64>]) -> Option<Vec<u64>> {
    pattern.iter().copied().collect()
}

fn parse_version(text: &str) -> Option<Vec<u64>> {
    parse_pattern(text).and_then(|pattern| concrete(&pattern))
}

fn component(version: &[u64], index: usize) -> u64 {
    version.get(index).copied().unwrap_or(0)
}

fn cmp_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for index in 0..a.len().max(b.len()) {
        let ordering = component(a, index).cmp(&component(b, index));
        if ordering.is_ne() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// `^1.2.3` is `< 2.0.0`; `^0.2.3` is `< 0.3.0`; `^0.0.3` is `< 0.0.4`.
/// The leading zero rule is semver's, and npm's `engines` uses it.
fn caret_ceiling(floor: &[u64]) -> Vec<u64> {
    let first_nonzero = floor.iter().position(|part| *part > 0).unwrap_or(0);
    let mut ceiling: Vec<u64> = floor.iter().take(first_nonzero + 1).copied().collect();
    let last = ceiling.len() - 1;
    ceiling[last] += 1;
    ceiling
}

/// `~1.2.3` and `~1.2` are `< 1.3.0`; `~1` is `< 2.0.0`.
fn tilde_ceiling(floor: &[u64], stated: usize) -> Vec<u64> {
    let bump = if stated >= 2 { 1 } else { 0 };
    let mut ceiling: Vec<u64> = floor.iter().take(bump + 1).copied().collect();
    let last = ceiling.len() - 1;
    ceiling[last] += 1;
    ceiling
}
