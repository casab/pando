//! A name read from an app's config written as code, without running it:
//! the string literal a key is given.

/// A token of JavaScript or TypeScript source, as far as finding a key's
/// literal needs.
#[derive(Debug, PartialEq)]
enum Token {
    /// An identifier, a keyword or a number.
    Word(String),
    /// A string literal: quoted, or a template with no substitution.
    Str(String),
    /// A template literal with a substitution, which is code.
    Template,
    Punct(char),
}

/// `source` as tokens, its comments and whitespace dropped. Only as
/// careful as a config needs: a regular expression with a quote in it
/// can throw it off, which at worst finds no literal.
fn tokens(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if matches!(c, '"' | '\'' | '`') {
            i += 1;
            let mut text = String::new();
            let mut code = false;
            while i < chars.len() && chars[i] != c {
                match chars[i] {
                    '\\' => {
                        text.extend(chars.get(i + 1));
                        i += 1;
                    }
                    '$' if c == '`' && chars.get(i + 1) == Some(&'{') => code = true,
                    // A quote left open ends with its line.
                    '\n' if c != '`' => break,
                    other => text.push(other),
                }
                i += 1;
            }
            i += 1;
            out.push(match code {
                true => Token::Template,
                false => Token::Str(text),
            });
        } else if word(c) {
            let start = i;
            while i < chars.len() && word(chars[i]) {
                i += 1;
            }
            out.push(Token::Word(chars[start..i].iter().collect()));
        } else {
            out.push(Token::Punct(c));
            i += 1;
        }
    }
    out
}

/// The string literal `key` is given in `sources`, where every place that
/// gives it one gives the same: `slug: "shop"`, or `"slug": 'shop'`.
///
/// `None` when no place sets it, when two set different literals, or when
/// any sets it to something else, whether a variable, an expression, or a
/// literal with more after it: a config that computes the name anywhere
/// may compute it where it counts, and pando does not run it to see.
pub(super) fn literal<S: AsRef<str>>(sources: &[S], key: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for source in sources {
        let tokens = tokens(source.as_ref());
        for (i, token) in tokens.iter().enumerate() {
            let named = matches!(token, Token::Word(name) | Token::Str(name) if name == key);
            if !named || tokens.get(i + 1) != Some(&Token::Punct(':')) {
                continue;
            }
            // `ready ? slug : other`, `case slug:` and `config.slug` do
            // not set it.
            let before = i.checked_sub(1).map(|at| &tokens[at]);
            if matches!(before, Some(Token::Punct('?' | '.')))
                || matches!(before, Some(Token::Word(word)) if word == "case")
            {
                continue;
            }
            let value = match (tokens.get(i + 2), tokens.get(i + 3)) {
                (Some(Token::Str(value)), None | Some(Token::Punct(',' | '}' | ';'))) => value,
                _ => return None,
            };
            match &found {
                Some(seen) if seen != value => return None,
                _ => found = Some(value.clone()),
            }
        }
    }
    found
}
