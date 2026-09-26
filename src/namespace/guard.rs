//! Whether pando may drop a database, or empty a slot, on a server it does
//! not own.

use anyhow::{Result, bail};

use crate::state::{NamespaceKind, NamespaceRecord, State};

use super::name::{MARKER, MAX_NAME, is_plain};

/// Whether `rm` — or a start freeing a slot — may drop this namespace.
///
/// The server is the developer's own, and the database next to this one
/// is their main checkout's: a wrong answer here destroys real work, and
/// nothing in pando can bring it back. So every one of these has to hold,
/// and each refusal names the one that did not:
///
/// - **State records it, for this worktree.** A namespace is written down
///   the moment the server made it for pando and not before, so a record
///   is pando's word that pando created it. A database that was already
///   there is never recorded, and never dropped.
/// - **It is never the main checkout's own**, whatever the record says: not
///   a name recorded beside it, and not any name the main checkout's env
///   files give today (`main_now`) — a queue's slot beside a cache's is
///   main's as much as the first. Compared without case, because MariaDB
///   on macOS does. For a slot, never slot 0 either, which is where every
///   app that says nothing about slots puts its keys.
/// - **A database carries the marker after the main name** —
///   `<main>__<something>` — is a plain identifier, and fits the server's
///   limit, so the statement that drops it cannot be made to say anything
///   else. A slot is a number.
/// - **No other worktree's record names the same one** on the same server,
///   in this project or in any other pando keeps state for on this machine
///   (`others`, each by its id) — and no other project's record names it
///   among that project's main checkout's own. Two records claiming one
///   database are a state file that is wrong about something, and the
///   drop that would empty the other worktree's data is not the way to
///   find out which. A project whose state cannot be read (`Err`, with
///   why) may name it too, so nothing is dropped while one cannot.
///
/// The grant a developer gives the app's login is a second wall on the
/// server's side: it covers `<main>__%` and nothing else.
pub fn may_drop(
    state: &State,
    worktree: &str,
    namespace: &NamespaceRecord,
    main_now: &[&str],
    others: &[(String, std::result::Result<State, String>)],
) -> Result<()> {
    let what = describe(namespace);
    let recorded = state
        .worktrees
        .get(worktree)
        .is_some_and(|record| record.namespaces.contains(namespace));
    if !recorded {
        bail!(
            "{what} is not recorded as {worktree}'s, so pando did not make it and will not drop it"
        );
    }
    let mains = namespace.every_main().chain(main_now.iter().copied());
    match namespace.kind {
        NamespaceKind::Database => {
            for main in mains {
                if namespace.name.eq_ignore_ascii_case(main) {
                    bail!("{what} is the main checkout's own database, and pando never drops that");
                }
            }
            // Before anything slices it: a plain name is ASCII, so every
            // byte below is a character boundary.
            if !is_plain(&namespace.name) || namespace.name.len() > MAX_NAME {
                bail!(
                    "{what} is not a plain name of at most {MAX_NAME} characters, so pando will \
                     not put it in a statement"
                );
            }
            let prefix = format!("{}{MARKER}", namespace.main);
            let marked = namespace.name.len() > prefix.len()
                && namespace.name.as_bytes()[..prefix.len()]
                    .eq_ignore_ascii_case(prefix.as_bytes());
            if !marked {
                bail!(
                    "{what} does not start with `{prefix}`, so it is not a worktree's database \
                     named after {:?}",
                    namespace.main
                );
            }
        }
        NamespaceKind::Slot => {
            let Ok(slot) = namespace.name.parse::<u32>() else {
                bail!("{what} is not a slot number");
            };
            if slot == 0 {
                bail!("{what} is slot 0, where an app that names no slot keeps its keys");
            }
            for main in mains {
                if main.trim().parse::<u32>() == Ok(slot) {
                    bail!("{what} is the main checkout's own slot, and pando never empties that");
                }
            }
        }
    }
    // Saying nothing is not saying it holds nothing.
    if let Some((project, why)) = others
        .iter()
        .find_map(|(project, other)| Some((project, other.as_ref().err()?)))
    {
        bail!(
            "{what} may be recorded by project {project} as well, whose state could not be read \
             — {why} — so pando drops nothing until it can be"
        );
    }
    let others: Vec<(&String, &State)> = others
        .iter()
        .filter_map(|(project, other)| Some((project, other.as_ref().ok()?)))
        .collect();
    let mut elsewhere: Vec<String> = state
        .worktrees
        .iter()
        .filter(|(name, _)| name.as_str() != worktree)
        .filter(|(_, record)| {
            record
                .namespaces
                .iter()
                .any(|other| same_namespace(other, namespace))
        })
        .map(|(name, _)| name.clone())
        .collect();
    for (project, other) in &others {
        for (name, record) in &other.worktrees {
            if record
                .namespaces
                .iter()
                .any(|ns| same_namespace(ns, namespace))
            {
                elsewhere.push(format!("{name} of project {project}"));
            }
        }
    }
    if !elsewhere.is_empty() {
        bail!(
            "{what} is recorded for {} as well, so pando cannot tell whose it is and drops \
             nothing",
            elsewhere.join(", ")
        );
    }
    // Another project's main checkout on the same server, as its records
    // name it.
    let theirs = others.iter().find(|(_, other)| {
        other
            .worktrees
            .values()
            .flat_map(|record| &record.namespaces)
            .any(|ns| {
                ns.every_main().any(|main| {
                    let main = NamespaceRecord {
                        name: main.to_string(),
                        ..ns.clone()
                    };
                    same_namespace(&main, namespace)
                })
            })
    });
    if let Some((project, _)) = theirs {
        bail!(
            "{what} is the main checkout's own in project {project}, and pando never drops or \
             empties that"
        );
    }
    Ok(())
}

/// Whether two records name the same database or slot on the same server.
pub fn same_namespace(a: &NamespaceRecord, b: &NamespaceRecord) -> bool {
    a.kind == b.kind
        && a.port == b.port
        && same_host(&a.host, &b.host)
        && match a.kind {
            NamespaceKind::Database => a.name.eq_ignore_ascii_case(&b.name),
            NamespaceKind::Slot => a.name.trim() == b.name.trim(),
        }
}

/// `localhost` and `127.0.0.1` are one server to an app on this machine.
fn same_host(a: &str, b: &str) -> bool {
    let local = |host: &str| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    a.eq_ignore_ascii_case(b) || (local(a) && local(b))
}

/// `database northwind_traders__feat_x`, `slot 3` — what a sentence about
/// one names it as.
pub fn describe(namespace: &NamespaceRecord) -> String {
    match namespace.kind {
        NamespaceKind::Database => format!("database {}", namespace.name),
        NamespaceKind::Slot => format!("{} slot {}", namespace.service, namespace.name),
    }
}
