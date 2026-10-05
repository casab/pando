//! The boundary: nothing outside this layer talks to the OS, and this layer
//! imports nothing above it. Then what [`Host`] promises.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

/// Files outside this layer that still talk to the OS, while the migration
/// moves what they do into it. The list only shrinks: a file here that no
/// longer offends is a stale row, and fails as surely as a new offender.
const NOT_YET: &[&str] = &[
    "actions/check/interrupt.rs",
    "actions/checkout.rs",
    "actions/init.rs",
    "actions/installed.rs",
    "actions/opening.rs",
    "actions/runtime.rs",
    "actions/update.rs",
    "actions/worktree.rs",
    "cli/prompt.rs",
    "compose/isolation.rs",
    "config/edit.rs",
    "config/schema.rs",
    "cow.rs",
    "doctor/config.rs",
    "env_command.rs",
    "hooks.rs",
    "log_tail.rs",
    "native.rs",
    "observe.rs",
    "paths.rs",
    "process.rs",
    "project.rs",
    "services.rs",
    "state.rs",
    "term.rs",
    "theme/select.rs",
    "tui/app/launch.rs",
    "tui/app/operations.rs",
    "tui/render/mod.rs",
    "tunnel.rs",
];

/// Where [`Host::here`] may be read: where pando meets the outside. Every
/// module below them is handed a `&Host`.
const HOST_EDGES: &[&str] = &[
    "actions/runtime.rs",
    "tui/app/launch.rs",
    "cli/open.rs",
    "theme/select.rs",
];

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, as its path below `src/` with `/`.
fn rust_files() -> Vec<String> {
    let root = src();
    let mut out = Vec::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let relative = path.strip_prefix(&root).unwrap();
                let parts: Vec<_> = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    out
}

/// The lines of `text` that are production code, numbered from 1: none of a
/// `tests.rs` or of `testutil.rs`, nothing from an inline test module on,
/// and no comment, so a doc may name what the code may not use.
fn production_lines<'t>(file: &str, text: &'t str) -> Vec<(usize, &'t str)> {
    let name = file.rsplit('/').next().unwrap_or(file);
    if name == "tests.rs" || name == "testutil.rs" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("mod tests") || trimmed.starts_with("pub(crate) mod tests"))
            && trimmed.ends_with('{')
        {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        out.push((index + 1, line));
    }
    out
}

/// Whether `line` names `path` as a path of its own: `nix::` is the crate,
/// not the end of `std::os::unix::`.
fn names(line: &str, path: &str) -> bool {
    line.match_indices(path).any(|(at, _)| {
        !line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':')
    })
}

/// What in one production line outside this layer talks to the OS.
fn os_talk(file: &str, line: &str) -> Option<&'static str> {
    const PATHS: &[&str] = &["std::os::", "nix::", "libc::", "windows_sys::"];
    const CALLS: &[&str] = &[
        "extern \"C\"",
        "var_os(\"HOME\")",
        "var(\"HOME\")",
        "Command::new(\"bash\")",
        "Command::new(\"sh\")",
        "\"/bin/sh\"",
    ];
    if let Some(path) = PATHS.iter().find(|path| names(line, path)) {
        return Some(path);
    }
    if let Some(call) = CALLS.iter().find(|call| line.contains(*call)) {
        return Some(call);
    }
    let conditional = ["cfg(", "cfg!(", "cfg_attr("]
        .iter()
        .any(|cfg| line.contains(cfg));
    if conditional
        && ["unix", "windows", "target_"]
            .iter()
            .any(|os| line.contains(os))
    {
        return Some("a cfg on the OS");
    }
    if line.contains("Host::here()") && !HOST_EDGES.contains(&file) {
        return Some("Host::here() below the edges");
    }
    None
}

#[test]
fn only_the_platform_layer_talks_to_the_os() {
    let mut offending: BTreeMap<String, String> = BTreeMap::new();
    for file in rust_files() {
        if file.starts_with("platform/") {
            continue;
        }
        let text = std::fs::read_to_string(src().join(&file)).unwrap();
        if let Some((number, what)) = production_lines(&file, &text)
            .into_iter()
            .find_map(|(number, line)| os_talk(&file, line).map(|what| (number, what)))
        {
            offending.insert(file, format!("{number}: {what}"));
        }
    }
    let new: Vec<String> = offending
        .iter()
        .filter(|(file, _)| !NOT_YET.contains(&file.as_str()))
        .map(|(file, at)| format!("src/{file}:{at}"))
        .collect();
    let stale: Vec<&&str> = NOT_YET
        .iter()
        .filter(|file| !offending.contains_key(**file))
        .collect();
    assert!(
        new.is_empty(),
        "only src/platform talks to the OS; move these into it: {new:#?}"
    );
    assert!(
        stale.is_empty(),
        "no longer talks to the OS; take it off NOT_YET: {stale:#?}"
    );
}

#[test]
fn the_platform_layer_imports_nothing_above_it() {
    let mut above = Vec::new();
    for file in rust_files() {
        if !file.starts_with("platform/") {
            continue;
        }
        let text = std::fs::read_to_string(src().join(&file)).unwrap();
        for (number, line) in production_lines(&file, &text) {
            for (at, _) in line.match_indices("crate::") {
                let rest = &line[at + "crate::".len()..];
                if !rest.starts_with("platform::") && !rest.starts_with("testutil::") {
                    above.push(format!("src/{file}:{number}: {}", line.trim()));
                }
            }
        }
    }
    assert!(
        above.is_empty(),
        "src/platform sits below everything else: {above:#?}"
    );
}

#[test]
fn no_test_sees_the_machine_it_runs_on() {
    assert_eq!(Host::here(), &Host::default());
    assert_eq!(Host::here().os, Os::HERE);
}
