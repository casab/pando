//! Writing to a layer: the patch that keeps a developer's formatting, and
//! the provenance note beside each value pando wrote.

use super::schema::Config;
use crate::paths::PandoPaths;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use toml::Table;
use toml_edit::{DocumentMut, Item, Table as EditTable};

/// Only ever writes the pando-home copy. A committed `pando.toml` is never
/// touched, which is why this takes `PandoPaths` rather than a path.
///
/// Kept for tests and for writing a config from scratch. No user-facing path
/// calls it: detection answers go through [`patch`], which preserves whatever
/// the developer wrote around them.
pub fn write(paths: &PandoPaths, config: &Config) -> Result<()> {
    paths.ensure_home()?;
    let text = toml::to_string_pretty(config).context("serialize pando.toml")?;
    write_private_atomic(&paths.config_file(), &text)
}

/// Which of the files pando may write an answer to.
///
/// Two, and the difference is what the answer is *about*. What a project
/// needs goes in the project layer, because it is true of the repository
/// wherever it is checked out. How this machine provides it goes in the
/// user layer, because it is true of the laptop and of every project on
/// it. A slot says which of the two its answer belongs to; nothing else
/// decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Project,
    User,
}

impl Layer {
    pub fn file(self, paths: &PandoPaths) -> PathBuf {
        match self {
            Layer::Project => paths.config_file(),
            Layer::User => paths.user_config_file(),
        }
    }

    /// Header for a file pando is creating from nothing. Written once, so
    /// the first thing a developer opening it sees is that the comments
    /// below are pando's and deleting them costs nothing.
    fn header(self) -> &'static str {
        match self {
            Layer::Project => {
                "# pando.toml — pando's own config for this project.\n\
                 # Lines marked \"# detected:\" or \"# answered:\" were written by pando.\n\
                 # Everything here is yours to edit; pando only ever adds keys it is missing.\n\n"
            }
            // One file for every project on this machine, which is the
            // thing to say first: a version manager is installed once.
            Layer::User => {
                "# pando's config for this machine, under every project's own pando.toml.\n\
                 # Lines marked \"# detected:\" or \"# answered:\" were written by pando.\n\
                 # Everything here is yours to edit; pando only ever adds keys it is missing.\n\n"
            }
        }
    }
}

/// The mode pando's config is written with. It can carry a command line, and
/// later phases put service credentials next to it.
const CONFIG_MODE: u32 = 0o600;

/// Why pando wrote a key, rendered as the trailing comment on its line.
///
/// Every value pando puts in the file explains itself, which is what makes a
/// wrong guess one visible edit away rather than hidden state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A rule decided it, and this is the signal that decided: `# detected:
    /// package.json scripts.dev`.
    Detected(String),
    /// A human answered the question: `# answered: 2026-09-20`.
    Answered,
    /// `--yes` took the first of this many options. Deliberately not
    /// `Detected`: no rule decided this, a flag did, and the file has to
    /// say so or it claims a confidence nothing had.
    TookFirst(usize),
    /// The same, for a question whose answer is a *set*: `--yes` took the
    /// options the rules had already resolved and left the rest. "The
    /// first of N" would be a sentence about a list nobody picked from.
    TookRuled { taken: usize, offered: usize },
    /// A program answered, through `init --answers`: `# answered: a
    /// program, 2026-09-21`.
    ///
    /// Its own note rather than [`Note::Answered`], because a developer
    /// reading their config has to be able to see which decisions a
    /// machine made for them and go and check those first.
    Program,
}

impl Note {
    /// Two spaces so the comment does not crowd the value, matching what
    /// `toml_edit` leaves between a value and a hand-written comment.
    fn comment(&self) -> String {
        match self {
            Note::Detected(why) => format!("  # detected: {why}"),
            Note::Answered => format!("  # answered: {}", Utc::now().format("%Y-%m-%d")),
            Note::TookFirst(options) => {
                format!("  # answered: --yes took the first of {options} options")
            }
            Note::TookRuled { taken, offered } => {
                format!("  # answered: --yes took the {taken} of {offered} the rules resolved")
            }
            Note::Program => format!("  # answered: a program, {}", Utc::now().format("%Y-%m-%d")),
        }
    }
}

/// Edits the pando-home `pando.toml` in place, preserving comments, key
/// order, and formatting.
///
/// Re-serialising the struct would be simpler and would throw away everything
/// the developer wrote: their comments, their ordering, and any key a newer
/// pando understands and this one does not. So the document is parsed as a
/// document, edited, and written back.
pub fn patch<F>(paths: &PandoPaths, layer: Layer, edit: F) -> Result<()>
where
    F: FnOnce(&mut DocumentMut) -> Result<()>,
{
    paths.ensure_home()?;
    let path = layer.file(paths);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("read {}", path.display())));
        }
    };
    let mut doc: DocumentMut = match &existing {
        // A file pando cannot parse is a file someone is editing. Overwriting
        // it with a document built from the half of it that parsed would lose
        // their work; refusing costs them one fix.
        Some(text) => text
            .parse()
            .map_err(|e: toml_edit::TomlError| {
                anyhow::anyhow!("{}", super::suggest::toml_error_line(&e.to_string()))
            })
            .with_context(|| {
                format!(
                    "{} is not valid TOML — fix it, or move it aside, and run again",
                    path.display()
                )
            })?,
        None => DocumentMut::new(),
    };
    // What the document looks like before the edit, not what the file's own
    // bytes look like: `toml_edit` normalises some things on the way through
    // (a file with no trailing newline gains one), and comparing against the
    // raw bytes would make an edit that changed nothing rewrite the file
    // anyway.
    let before = doc.to_string();
    edit(&mut doc)?;
    let body = doc.to_string();
    if existing.is_some() && body == before {
        return Ok(());
    }
    // A header parsed into an empty document becomes its *trailing* trivia,
    // so the first table pando adds would land above it. Prepending the text
    // is the one way it stays at the top; every later patch then carries it
    // as the first table's leading comment and preserves it untouched.
    let rendered = match &existing {
        Some(_) => body,
        // Nothing to say and nothing to write: a patch that added no keys to
        // a project with no config leaves it without one.
        None if body.trim().is_empty() => return Ok(()),
        None => format!("{}{body}", layer.header()),
    };
    // And a file whose bytes already say exactly this is left alone too, so
    // mtime-driven watchers stay quiet and a re-run stays quiet with them.
    if existing.as_deref() == Some(rendered.as_str()) {
        return Ok(());
    }
    write_private_atomic(&path, &rendered)
}

/// Writes one key with the note explaining where it came from. The
/// convenience every detection answer uses.
///
/// `table_path` is the table the key belongs to, for example `["dev"]` or
/// `["project"]`; missing tables are created.
pub fn set_detected(
    paths: &PandoPaths,
    layer: Layer,
    table_path: &[&str],
    key: &str,
    value: impl Into<toml_edit::Value>,
    note: Note,
) -> Result<()> {
    let value = value.into();
    let comment = note.comment();
    let elsewhere = other_layer_declares_processes(paths, layer);
    patch(paths, layer, move |doc| {
        let target = write_target(doc, elsewhere, table_path);
        let table = ensure_table(doc, &target)?;
        table.insert(key, Item::Value(value));
        if let Some(v) = table.get_mut(key).and_then(Item::as_value_mut) {
            v.decor_mut().set_suffix(comment);
        }
        Ok(())
    })
}

/// Writes every key of one table at once, with the note on the table's own
/// header rather than repeated on each key.
///
/// A whole `[processes.<app>]` table is one answer to one question. Ten
/// identical `# answered: --yes took the first of 2 options` lines for a
/// two-app workspace say that ten times; one on the header says it once,
/// and says it about the table rather than about a key.
pub fn set_detected_table(
    paths: &PandoPaths,
    layer: Layer,
    table_path: &[&str],
    entries: Vec<(String, toml_edit::Value)>,
    note: Note,
) -> Result<()> {
    let comment = note.comment();
    let elsewhere = other_layer_declares_processes(paths, layer);
    patch(paths, layer, move |doc| {
        let target = write_target(doc, elsewhere, table_path);
        let table = ensure_table(doc, &target)?;
        for (key, value) in entries {
            table.insert(&key, Item::Value(value));
        }
        table.decor_mut().set_suffix(comment);
        Ok(())
    })
}

/// Appends one entry to an array of tables — `[[services]]`, `[[hooks]]`
/// — with the note on the entry's own header.
///
/// Appended, never replaced: an array of tables a developer wrote is a
/// list they curated, and a detection that rewrote it would silently
/// delete an entry pando has no opinion about. The caller has already
/// decided there is nothing there to conflict with; `still_needed` is what
/// makes that call.
pub fn set_detected_array_entry(
    paths: &PandoPaths,
    layer: Layer,
    array: &str,
    entries: Vec<(String, toml_edit::Value)>,
    note: Note,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let comment = note.comment();
    let array = array.to_string();
    patch(paths, layer, move |doc| {
        let item = doc
            .entry(&array)
            .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
        let tables = item.as_array_of_tables_mut().with_context(|| {
            format!(
                "[[{array}]] in pando.toml is not an array of tables — pando will not \
                     overwrite it"
            )
        })?;
        let mut table = EditTable::new();
        for (key, value) in entries {
            table.insert(&key, Item::Value(value));
        }
        table.decor_mut().set_suffix(comment);
        tables.push(table);
        Ok(())
    })
}

/// Whether a layer other than the one being patched already declares a
/// process: the `pando.toml` a team committed, or the machine-wide file.
///
/// The conflict [`normalize`] refuses is between the *merged* layers, so
/// looking only at the document being edited misses a `[processes]` table
/// in another file — and that is precisely the shape 2b invites, a
/// long-form `[processes.dev]` with a `cwd` and no `cmd` for detection to
/// fill in. Read as bare tables rather than through `load`: this is a
/// question about which *tables* exist, and it has to be answerable even
/// when the merged config would not validate.
fn other_layer_declares_processes(paths: &PandoPaths, layer: Layer) -> bool {
    [
        paths.root().join("pando.toml"),
        paths.user_config_file(),
        paths.config_file(),
    ]
    .iter()
    .filter(|path| **path != layer.file(paths))
    .any(|path| declares_top_level(path, "processes"))
}

/// Which layer's file sets `[runtime].prelude`, highest precedence first.
///
/// A report that says a prelude is not working has to say where to change
/// it, and with three layers that is a real question. Read as a bare table
/// rather than through `load`, for the same reason the process check is:
/// it has to be answerable even when the merged config would not build.
pub fn prelude_origin(paths: &PandoPaths) -> Option<PathBuf> {
    [
        Layer::Project.file(paths),
        Layer::User.file(paths),
        paths.root().join("pando.toml"),
    ]
    .into_iter()
    .find(|path| declares_prelude(path))
}

fn declares_prelude(path: &Path) -> bool {
    declares(path, "runtime", "prelude")
}

/// Which layer's file sets `[project].install`, highest precedence first:
/// the file a failed install has to be fixed in. pando's own when none of
/// them says, since that is where an answer would be written.
pub fn install_origin(paths: &PandoPaths) -> PathBuf {
    [
        Layer::Project.file(paths),
        Layer::User.file(paths),
        paths.root().join("pando.toml"),
    ]
    .into_iter()
    .find(|path| declares(path, "project", "install"))
    .unwrap_or_else(|| Layer::Project.file(paths))
}

/// Which layer's file declares `[[services]]`, highest precedence first:
/// a higher layer replaces the whole list, so the entries in force are
/// that one file's. pando's own when none of them does, since that is
/// where an answer would be written.
pub fn services_origin(paths: &PandoPaths) -> PathBuf {
    [
        Layer::Project.file(paths),
        Layer::User.file(paths),
        paths.root().join("pando.toml"),
    ]
    .into_iter()
    .find(|path| declares_top_level(path, "services"))
    .unwrap_or_else(|| Layer::Project.file(paths))
}

fn declares(path: &Path, table: &str, key: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Table>(&text).ok())
        .and_then(|t| Some(t.get(table)?.get(key).is_some()))
        .unwrap_or(false)
}

fn declares_top_level(path: &Path, key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    toml::from_str::<Table>(&text)
        .map(|table| table.contains_key(key))
        .unwrap_or(false)
}

/// Where a key really goes in *this* document.
///
/// `[dev]` is shorthand for `[processes.dev]`, and the two forms may not
/// both appear in one file — or in two layers of one config. Whenever
/// anything already declares a process, the long form is what gets
/// written: the shorthand beside it produces a config pando's own loader
/// refuses, taking `start`, `new`, `restart` and the TUI down with it until
/// a human edits the file pando wrote.
fn write_target<'a>(
    doc: &DocumentMut,
    other_layer_has_processes: bool,
    path: &'a [&'a str],
) -> Vec<&'a str> {
    if path == ["dev"] && (other_layer_has_processes || doc.as_table().contains_key("processes")) {
        return vec!["processes", "dev"];
    }
    path.to_vec()
}

/// The table at `path`, creating any level that is missing. A path that runs
/// into a non-table (`dev = 3`) is an error naming it rather than a silent
/// overwrite of whatever was there.
///
/// A level pando creates only to hold the next one is implicit, so the file
/// gets `[processes.web]` and not a bare `[processes]` line above it — a
/// line that is valid TOML and that no human would have written. A level
/// that was already there keeps whatever the developer made it.
fn ensure_table<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut EditTable> {
    let mut table = doc.as_table_mut();
    let leaf = path.len().saturating_sub(1);
    for (depth, part) in path.iter().enumerate() {
        let existed = table.contains_key(part);
        let entry = table
            .entry(part)
            .or_insert_with(|| Item::Table(EditTable::new()));
        table = entry.as_table_mut().with_context(|| {
            format!(
                "[{}] in pando.toml is not a table — pando will not overwrite it",
                path[..=depth].join(".")
            )
        })?;
        if !existed && depth < leaf {
            table.set_implicit(true);
        }
    }
    Ok(table)
}

/// Atomic, and 0600: the temp file is locked down before the rename, so there
/// is never a moment where a world-readable config exists at the final path.
fn write_private_atomic(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(CONFIG_MODE))
        .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename tmp → {}", path.display()))?;
    Ok(())
}
