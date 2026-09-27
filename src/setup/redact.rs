//! What of a failed process's log never goes into `check.json`: a
//! password, a token, a secret.
//!
//! The record's closing lines are read into the job `pando init --agent`
//! prints, and from there into an agent's context and whatever it keeps.
//! A dev server that dies printing its connection string would otherwise
//! hand its password on. Hidden rather than dropped, so the line still
//! says what failed.

use crate::config::HIDDEN;

/// Words that make a key's value a secret, matched anywhere in the key and
/// in any case: `DB_PASSWORD`, `stripeSecretKey`, `x-auth-token`.
const SECRET_WORDS: [&str; 13] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "auth",
    "credential",
    "private_key",
    "privatekey",
    "access_key",
    "accesskey",
    "cookie",
];

/// `line` with every secret it carries read as [`HIDDEN`]: the password of
/// a URL's login (`postgres://app:s3cret@db`), the value of a key whose
/// name says it is one (`API_TOKEN=…`, `"password": "…"`,
/// `Authorization: …`), and a bearer token.
pub fn redact_line(line: &str) -> String {
    let line = hide_url_passwords(line);
    let line = hide_bearer_tokens(&line);
    hide_secret_values(&line)
}

/// `scheme://user:password@host` → `scheme://user:(hidden)@host`.
fn hide_url_passwords(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                let login = &authority[..at];
                match login.split_once(':') {
                    Some((user, password)) if !password.is_empty() => {
                        out.push_str(&format!("{user}:{HIDDEN}"));
                    }
                    _ => out.push_str(login),
                }
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

/// `Bearer <token>` → `Bearer (hidden)`.
fn hide_bearer_tokens(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let Some(at) = lower.find("bearer ") else {
        return line.to_string();
    };
    let start = at + "bearer ".len();
    let end = line[start..]
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
        .map_or(line.len(), |n| start + n);
    if end == start {
        return line.to_string();
    }
    format!(
        "{}{HIDDEN}{}",
        &line[..start],
        hide_bearer_tokens(&line[end..])
    )
}

/// The value after `=` or `:` of any key whose name has a
/// [`SECRET_WORDS`] word in it, quoted or bare, up to the next space,
/// comma, quote or brace.
fn hide_secret_values(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(['=', ':']) {
        let (before, after) = rest.split_at(at);
        let key: String = before
            .chars()
            .rev()
            .skip_while(|c| *c == '"' || *c == '\'' || c.is_whitespace())
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect::<String>()
            .to_ascii_lowercase();
        out.push_str(before);
        out.push_str(&after[..1]);
        let value = &after[1..];
        // `://` is a URL's, whose login the pass above has seen to.
        let secret = !value.starts_with("//")
            && !key.is_empty()
            && SECRET_WORDS.iter().any(|word| key.contains(word));
        if !secret {
            rest = value;
            continue;
        }
        let spaces = value.len() - value.trim_start().len();
        out.push_str(&value[..spaces]);
        let mut value = &value[spaces..];
        // An authorization scheme stays, so the line still says which:
        // `Authorization: Basic (hidden)`.
        if let Some((scheme, rest)) = value.split_once(' ')
            && ["bearer", "basic", "token"].contains(&scheme.to_ascii_lowercase().as_str())
        {
            out.push_str(scheme);
            out.push(' ');
            value = rest;
        }
        if value.starts_with(HIDDEN) {
            rest = value;
            continue;
        }
        let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'');
        let (open, body) = match quote {
            Some(q) => (&value[..q.len_utf8()], &value[q.len_utf8()..]),
            None => ("", value),
        };
        let end = match quote {
            Some(q) => body.find(q).unwrap_or(body.len()),
            None => body
                .find(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ';'))
                .unwrap_or(body.len()),
        };
        out.push_str(open);
        if end > 0 {
            out.push_str(HIDDEN);
        }
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}
