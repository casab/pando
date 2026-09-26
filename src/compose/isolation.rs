//! The per-worktree override: the compose project name, the ports and
//! volumes an isolated worktree publishes, and what refuses isolation.

use super::ComposeFile;
use super::Mount;
use super::Service;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

// ---- the per-worktree override -------------------------------------------

/// The compose project name for one worktree: what isolates its
/// containers, its network, and — because compose prefixes named volumes
/// with it — its data.
///
/// Compose allows `[a-z0-9][a-z0-9_-]*`, and a worktree directory is named
/// after a branch, so `feat+checkout` has to be folded. Folding can
/// collide: `feat+one` and `feat-one` are two worktrees and would become
/// one set of containers and one database. So a name that had to change
/// carries four hex characters of its own hash, and a name that did not is
/// left readable.
pub fn project_name(project_id: &str, worktree: &str) -> String {
    let project = fold(project_id);
    let name = fold(worktree);
    if name == worktree {
        return format!("pando-{project}-{name}");
    }
    let digest = format!("{:x}", md5::compute(worktree.as_bytes()));
    format!("pando-{project}-{name}-{}", &digest[..4])
}

fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect()
}

/// One included service, resolved: the port inside the container, and the
/// port on the host pando allocated for this worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub service: String,
    pub container: u16,
    pub host: u16,
}

/// The container port of every included service, or the reason this
/// worktree cannot be isolated.
///
/// Every refusal names the service and says what to change in the
/// project's own file, because that is the only place it can be fixed:
/// Invariant 1 forbids pando to edit a compose file, and rewriting a bind
/// mount underneath the developer would put the isolated database
/// somewhere they did not ask for.
/// `repository` is every directory a bind mount must not land in: the main
/// checkout, and the worktrees pando keeps for it. A path pando can test is
/// tested rather than waved through — an absolute source does not move with
/// the worktree, so it is either a write into the repository or one
/// directory shared by every worktree.
pub fn resolve_included(
    file: &ComposeFile,
    include: &[String],
    repository: &[PathBuf],
) -> Result<Vec<(String, u16)>> {
    let mut out = Vec::new();
    for name in include {
        let service = file.services.get(name).with_context(|| {
            let known: Vec<&str> = file.services.keys().map(String::as_str).collect();
            format!(
                "the compose file has no service named {name:?} — it declares: {}{}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                },
                caveat(file)
            )
        })?;
        // A mount this reader never saw is not one it can refuse, and an
        // alias, a merge key or `extends:` can carry one in. Compose
        // resolves them when it can; when it could not, half a file
        // approves nothing.
        if file.unresolved.extends.contains(name) {
            bail!(
                "service {name:?} is not isolated: it uses `extends:`, which pando's own reader \
                 does not follow, and `docker compose config` could not resolve it here — a bind \
                 mount the service it extends declares would be invisible to pando, and an \
                 isolated copy could write its data into your repository. Make `docker compose \
                 config` work for this file, or write out in {name:?} what it extends"
            );
        }
        if file.unresolved.aliases {
            bail!(
                "service {name:?} is not isolated: the compose file uses YAML aliases or merge \
                 keys (`*`, `<<:`), which pando's own reader does not follow, and `docker \
                 compose config` could not resolve them here — a bind mount brought in through \
                 one would be invisible to pando, and an isolated copy could write its data into \
                 your repository. Make `docker compose config` work for this file, or write out \
                 in {name:?} what the alias brings in"
            );
        }
        check_mounts(name, service, file, repository)?;
        check_depends_on(name, service, include)?;
        let port = service.container_port().with_context(|| {
            format!(
                "service {name:?} publishes no port and its image ({}) is not one pando knows a \
                 port for — add a `ports:` entry to it in the compose file, or drop it from \
                 `include`{}",
                service.image.as_deref().unwrap_or("none"),
                caveat(file)
            )
        })?;
        out.push((name.clone(), port));
    }
    Ok(out)
}

/// The sentence every refusal gains when this reader knew it was working
/// from half a file.
///
/// Without it a service that `extends` one declaring `ports: ["5432:5432"]`
/// is refused with "add a `ports:` entry", which tells the developer to add
/// something their file already has, somewhere they cannot usefully add it.
fn caveat(file: &ComposeFile) -> String {
    match file.unresolved.describe() {
        None => String::new(),
        Some(keys) => format!(
            ". Note that this file uses {keys}, which pando's own reader does not follow — it \
             asks `docker compose config` to resolve them when Docker is available, and could \
             not here"
        ),
    }
}

/// A bind mount relative to the compose file lands inside the worktree,
/// and a named volume the project pins by name or borrows from outside is
/// one every worktree would share.
fn check_mounts(
    name: &str,
    service: &Service,
    file: &ComposeFile,
    repository: &[PathBuf],
) -> Result<()> {
    for mount in &service.volumes {
        match mount {
            Mount::Bind(source) if source.starts_with('$') => bail!(
                "service {name:?} mounts {source:?}, and pando cannot tell whether that is \
                 inside the repository — isolating it could write into your worktree. Use a \
                 named volume for it, or drop {name:?} from `include`"
            ),
            Mount::Bind(source) if source.starts_with('/') || source.starts_with('~') => {
                // A path pando *can* test is tested. An absolute source
                // does not move with the worktree, so it is one of two
                // things, and neither of them is isolation.
                let resolved = crate::paths::resolve_for_compare(&expand_home(source));
                if let Some(inside) = repository
                    .iter()
                    .find(|dir| resolved.starts_with(crate::paths::resolve_for_compare(dir)))
                {
                    bail!(
                        "service {name:?} mounts {source:?}, which is inside {} — an isolated \
                         copy would write its data into your repository, which pando never \
                         does. Change it to a named volume in the compose file, or drop \
                         {name:?} from `include`",
                        inside.display()
                    );
                }
                bail!(
                    "service {name:?} mounts {source:?}, an absolute path that does not move \
                     with the worktree — every worktree would bind that one directory and \
                     share the data in it, which is not an isolated copy. Change it to a \
                     named volume in the compose file, or drop {name:?} from `include`"
                );
            }
            Mount::Bind(source) => bail!(
                "service {name:?} mounts {source:?}, which is inside the repository — an \
                 isolated copy would write its data into your worktree, which pando never \
                 does. Change it to a named volume in the compose file, or drop {name:?} \
                 from `include`"
            ),
            Mount::Named(volume) => {
                let Some(declared) = file.volumes.get(volume) else {
                    continue;
                };
                if declared.external {
                    bail!(
                        "service {name:?} uses the volume {volume:?}, which is declared \
                         `external: true` — compose does not prefix it with the project name, \
                         so every worktree would share one copy of that data"
                    );
                }
                if let Some(literal) = &declared.name {
                    bail!(
                        "service {name:?} uses the volume {volume:?}, which pins its name to \
                         {literal:?} — compose does not prefix a pinned name with the project \
                         name, so every worktree would share one copy of that data"
                    );
                }
            }
            Mount::Anonymous => {}
        }
    }
    Ok(())
}

/// `~` and `~/…` replaced with the home directory, which is what docker
/// itself does with a bind source before it mounts it. Anything else is
/// returned as written.
fn expand_home(source: &str) -> PathBuf {
    let Some(rest) = source.strip_prefix('~') else {
        return PathBuf::from(source);
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return PathBuf::from(source);
    };
    match rest.strip_prefix('/') {
        Some(tail) => home.join(tail),
        // `~user/…` is a different user's home and not one pando can
        // resolve; left as written, so it is judged as an absolute path.
        None if rest.is_empty() => home,
        None => PathBuf::from(source),
    }
}

/// `up -d <included>` starts anything an included service depends on, and
/// those come up on the ports the project hardcoded — which the second
/// worktree then collides with. Refusing is honest; `--no-deps` would
/// start a service whose dependency is missing.
fn check_depends_on(name: &str, service: &Service, include: &[String]) -> Result<()> {
    for needed in &service.depends_on {
        if !include.iter().any(|included| included == needed) {
            bail!(
                "service {name:?} depends_on {needed:?}, which is not in `include` — compose \
                 would start {needed:?} too, on the port the project hardcoded, and the second \
                 worktree would collide with the first. Add {needed:?} to `include`, or drop \
                 {name:?}"
            );
        }
    }
    Ok(())
}

/// The override file for one worktree: the published ports replaced, not
/// appended to, and any `container_name` removed.
///
/// `!override` because compose *merges* `ports` lists across files, which
/// would keep the project's own hardcoded port beside the allocated one
/// and collide across worktrees the moment a second one started.
/// `container_name: !reset` because a fixed container name is unique per
/// daemon, so the second worktree's `up` would fail on it.
pub fn render_override(worktree: &str, published: &[Published]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# generated by pando for the worktree {worktree}\n\
         # regenerated on every isolated start; edits here are lost\n\
         # the project's own compose file is never modified\n"
    ));
    if published.is_empty() {
        out.push_str("services: {}\n");
        return out;
    }
    let mut sorted: Vec<&Published> = published.iter().collect();
    sorted.sort_by(|a, b| a.service.cmp(&b.service));
    out.push_str("services:\n");
    for entry in sorted {
        out.push_str(&format!("  {}:\n", entry.service));
        out.push_str(&format!(
            "    ports: !override [\"127.0.0.1:{}:{}\"]\n",
            entry.host, entry.container
        ));
        out.push_str("    container_name: !reset\n");
    }
    out
}
