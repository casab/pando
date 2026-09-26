//! What of a config file is never printed: a namespace login's password.

use toml_edit::{DocumentMut, Item, Table, Value};

/// What a password reads as wherever pando prints a config.
pub const HIDDEN: &str = "(hidden)";

/// `namespaced.<service>.password`: a login pando keeps for namespaced
/// starts.
pub fn is_password(key: &str) -> bool {
    key.starts_with("namespaced.") && key.ends_with(".password")
}

/// A config file's text with every password [`is_password`] names read as
/// [`HIDDEN`], and everything else — comments, layout, the notes beside
/// each value — as it was.
///
/// For a file pando prints whole. Text that does not parse as TOML has
/// every line that mentions a password hidden from its first `=` on
/// instead: which table a line is in cannot be known then, and a preview
/// is not a reason to print a login.
pub fn hide_passwords(text: &str) -> String {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return hide_password_lines(text);
    };
    match hide_in_table(doc.as_table_mut(), "") {
        true => doc.to_string(),
        false => text.to_string(),
    }
}

/// Hides every password below `table`, whose own key path is `prefix`.
/// Whether it hid anything.
fn hide_in_table(table: &mut Table, prefix: &str) -> bool {
    let mut hid = false;
    for (key, item) in table.iter_mut() {
        let path = joined(prefix, key.get());
        hid |= match item {
            Item::Value(value) => hide_in_value(value, &path),
            Item::Table(inner) => hide_in_table(inner, &path),
            Item::ArrayOfTables(entries) => {
                let mut hid = false;
                for (index, entry) in entries.iter_mut().enumerate() {
                    hid |= hide_in_table(entry, &format!("{path}[{index}]"));
                }
                hid
            }
            Item::None => false,
        };
    }
    hid
}

/// A value that is a password is replaced, keeping the space and comment
/// around it; a login written inline is walked key by key.
fn hide_in_value(value: &mut Value, path: &str) -> bool {
    if is_password(path) {
        let decor = value.decor().clone();
        *value = Value::from(HIDDEN);
        *value.decor_mut() = decor;
        return true;
    }
    let Value::InlineTable(inline) = value else {
        return false;
    };
    let mut hid = false;
    for (key, value) in inline.iter_mut() {
        hid |= hide_in_value(value, &joined(path, key.get()));
    }
    hid
}

fn joined(prefix: &str, key: &str) -> String {
    match prefix.is_empty() {
        true => key.to_string(),
        false => format!("{prefix}.{key}"),
    }
}

fn hide_password_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match line.split_once('=') {
            Some((key, rest)) if line.contains("password") => {
                out.push_str(key);
                out.push_str(&format!("= \"{HIDDEN}\""));
                if rest.ends_with('\n') {
                    out.push('\n');
                }
            }
            _ => out.push_str(line),
        }
    }
    out
}
