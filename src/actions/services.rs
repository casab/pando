//! Private services: planning them, bringing them up and down, and what
//! they resolve to.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, Config};
use crate::native;
use crate::paths::PandoPaths;
use crate::ports;
use crate::process::{self as proc, SpawnOptions};
use crate::services;
use crate::state::{self, Phase, WorktreeRecord};
use crate::template;

use super::lifecycle::{DEFAULT_READY_ROLE, STOP_GRACE, process_env, reset_log};
use super::worktree::find_worktree;

/// Stops the containers of every compose project named, leaving their
/// volumes.
///
/// A Docker daemon that is not running is a note, not a failure: its
/// containers cannot be running either, so there is nothing left to stop,
/// and a `stop` that errors because Docker is off is a worktree the
/// developer cannot stop. The records that name the projects are kept by
/// every caller, so a later `stop` or `rm` can still find them.
pub(super) fn stop_containers(
    paths: &PandoPaths,
    projects: &[String],
    progress: &dyn Fn(&str),
) -> Result<()> {
    stop_compose_projects(
        paths,
        projects,
        |compose| compose.stop(),
        |project| {
            format!(
                "Docker is not running, so the services of {project} are not either — nothing \
                 to stop"
            )
        },
        progress,
    )
}

/// Removes the containers, network and volumes of every compose project
/// named — what `rm` does.
///
/// A daemon that is not running is a note here too, because `rm` must
/// always be able to remove a worktree. It is a loud one: the volumes
/// outlive the worktree, and the note is the only place the command that
/// removes them is written down.
pub(super) fn remove_containers(
    paths: &PandoPaths,
    projects: &[String],
    progress: &dyn Fn(&str),
) -> Result<()> {
    stop_compose_projects(
        paths,
        projects,
        |compose| compose.down_with_volumes(),
        |project| {
            format!(
                "Docker is not running, so the services of {project} could not be removed — their \
                 data volumes survive; once Docker is up, `docker compose -p {project} down -v` \
                 removes them"
            )
        },
        progress,
    )
}

/// The first of these compose projects Docker cannot be asked about, if
/// any, with how it failed: "is not running" for a daemon that is down,
/// "is not answering" for one that did not reply in time. Any other
/// failure is left to the command that follows to report, with its own
/// words.
///
/// A hung daemon counts: the `down -v` that follows would hang the same
/// way, under the state lock.
pub(super) fn docker_down_for(
    paths: &PandoPaths,
    projects: &[String],
) -> Option<(String, &'static str)> {
    if projects.is_empty() {
        return None;
    }
    let program = services::docker_program(paths);
    projects.iter().find_map(|project| {
        let e = services::Compose::by_project(&program, project.as_str())
            .reachable()
            .err()?;
        let how = match () {
            _ if services::is_daemon_down(&e) => "is not running",
            _ if services::is_daemon_hung(&e) => "is not answering",
            _ => return None,
        };
        Some((project.clone(), how))
    })
}

/// Runs one compose verb against every project a worktree owns, reporting
/// the failures together. A daemon that is down is reported through
/// `daemon_down` and `progress` instead of failing.
fn stop_compose_projects(
    paths: &PandoPaths,
    projects: &[String],
    run: impl Fn(&services::Compose) -> Result<()>,
    daemon_down: impl Fn(&str) -> String,
    progress: &dyn Fn(&str),
) -> Result<()> {
    if projects.is_empty() {
        return Ok(());
    }
    let program = services::docker_program(paths);
    let mut failures = Vec::new();
    for project in projects {
        let compose = services::Compose::by_project(&program, project.as_str());
        match run(&compose) {
            Ok(()) => {}
            Err(e) if services::is_daemon_down(&e) => progress(&daemon_down(project)),
            Err(e) => failures.push(format!("{project}: {e:#}")),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    bail!("could not reach the services of {}", failures.join("; "))
}

/// One `[[services]]` entry of `kind = "compose"`, flattened.
struct ComposeEntry<'a> {
    file: &'a str,
    include: &'a [String],
    env: &'a BTreeMap<String, String>,
    ready_timeout_s: Option<u64>,
}

fn compose_entries(config: &Config) -> Vec<ComposeEntry<'_>> {
    config
        .services
        .iter()
        .filter_map(|service| match service {
            config::ServiceConfig::Compose {
                file,
                include,
                env,
                ready_timeout_s,
            } => Some(ComposeEntry {
                file,
                include,
                env,
                ready_timeout_s: *ready_timeout_s,
            }),
            // Native entries are this list's opposite number; see
            // `native::Entry::all`.
            config::ServiceConfig::Native { .. } => None,
        })
        .collect()
}

/// Every service this project can run a private copy of, in config order.
/// Each one is a role, so `{port:postgres}` resolves like any other.
///
/// Compose and native entries share one role space and are listed in the
/// order the file writes them, so a project that has both hands out ports
/// in a stable order whichever kind comes first.
pub(super) fn service_roles(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for service in &config.services {
        match service {
            config::ServiceConfig::Compose { include, .. } => {
                for name in include {
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                }
            }
            config::ServiceConfig::Native { name, .. } => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    out
}

/// The names of the services this project runs as pando's own processes
/// rather than as containers.
fn native_names(config: &Config) -> Vec<String> {
    native::Entry::all(config)
        .iter()
        .map(|entry| entry.name.to_string())
        .collect()
}

/// The service records an isolated start writes, before anything is up.
///
/// A record that already exists keeps its log pump, so a plain `start`
/// beside live services does not lose the pid that stops it. A record for
/// a service config no longer includes is kept too: it still names the
/// compose project, and dropping it would leave a container and its volume
/// with nothing in pando able to take them down — but its *port* is
/// blanked, because the window has moved on and another service has that
/// number now. `status` would otherwise show two services on one port, and
/// only one of them would be telling the truth.
pub(super) fn planned_services(
    config: &Config,
    ports: &BTreeMap<String, u16>,
    record: &WorktreeRecord,
) -> Vec<state::ServiceRecord> {
    let native = native_names(config);
    let mut out: Vec<state::ServiceRecord> = Vec::new();
    for service in service_roles(config) {
        let existing = record.services.iter().find(|s| s.name == service);
        // A native record's pid *is* the server, and `reconcile` drops the
        // record when it dies; a compose record's pid is only the log
        // pump in front of a container. Same field, and the two must not
        // be confused — a record written with the wrong kind would either
        // lose a container pando can never find again, or keep a database
        // record for a process that exited.
        let is_native = native.contains(&service);
        out.push(state::ServiceRecord {
            name: service.clone(),
            kind: match is_native {
                true => state::ServiceKind::Native,
                false => state::ServiceKind::Compose,
            },
            port: ports.get(&service).copied(),
            pid: existing.and_then(|s| s.pid),
            pgid: existing.and_then(|s| s.pgid),
            // Not named until compose has been asked to create something
            // under it — see `name_compose_project`. A record with no
            // project is one whose containers were never brought up, so
            // `stop` and `rm` have nothing to ask Docker about, and a
            // failed start can drop it.
            compose_project: match is_native {
                true => None,
                false => existing.and_then(|s| s.compose_project.clone()),
            },
        });
    }
    for service in &record.services {
        if !out.iter().any(|kept| kept.name == service.name) {
            out.push(state::ServiceRecord {
                port: None,
                ..service.clone()
            });
        }
    }
    out
}

/// The env every process and hook of an isolated worktree is given, so it
/// talks to its own services rather than the shared ones.
///
/// Compose and native entries go through the same rewriter: a URL keeps
/// its credentials and its database and has its port replaced, a bare
/// number becomes the port. The one difference is whose idea the key was.
/// A key the *config* names is an instruction, and a worktree that cannot
/// satisfy it fails the start. A key a *recipe* supplied is a default —
/// `DATABASE_URL` is what a Postgres recipe expects an app to read — and
/// a project that has no such variable anywhere has not asked for one, so
/// it is dropped rather than turned into a refusal.
pub(super) fn resolve_service_env(
    paths: &PandoPaths,
    config: &Config,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in compose_entries(config) {
        out.extend(services::app_env(worktree, entry.env, ports)?);
    }
    let native = native::Entry::all(config);
    if native.is_empty() {
        return Ok(out);
    }
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    for entry in &native {
        let recipe = native::resolve(&recipes, entry).ok().map(|r| r.recipe);
        let (mapping, from_config) = entry.env_map(recipe.as_ref());
        if mapping.is_empty() {
            continue;
        }
        match services::app_env(worktree, &mapping, ports) {
            Ok(resolved) => out.extend(resolved),
            Err(e) if from_config => return Err(e),
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// The URL each native service's app env points at, so a recipe's
/// `create` step knows which database and role to make.
fn native_urls(
    paths: &PandoPaths,
    config: &Config,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> BTreeMap<String, String> {
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    let mut out = BTreeMap::new();
    for entry in native::Entry::all(config) {
        let recipe = native::resolve(&recipes, &entry).ok().map(|r| r.recipe);
        let (mapping, _) = entry.env_map(recipe.as_ref());
        // Only the keys that address this service: an entry's env map can
        // name another one, and its URL says nothing about this database.
        let mapping: BTreeMap<String, String> = mapping
            .into_iter()
            .filter(|(_, service)| service == entry.name)
            .collect();
        let Ok(resolved) = services::app_env(worktree, &mapping, ports) else {
            continue;
        };
        if let Some(value) = identity_url(&resolved) {
            out.insert(entry.name.to_string(), value);
        }
    }
    out
}

/// Which of several values an app reads for one service says who it
/// connects as and to which database.
///
/// A service reached through more than one variable — a `DATABASE_URL`
/// beside a bare `PGPORT` — used to hand over whichever key sorted first,
/// so renaming an unrelated variable changed which database the recipe
/// created. The one that names the most wins: a database, then a user;
/// among equals, the first key alphabetically, so two runs always agree.
pub(super) fn identity_url(resolved: &BTreeMap<String, String>) -> Option<String> {
    resolved
        .iter()
        .max_by_key(|(key, value)| {
            let (user, database) = services::url_identity(value);
            // `max_by_key` keeps the *last* maximum, so the key is
            // reversed to make the first one alphabetically win a tie.
            (
                database.is_some(),
                user.is_some(),
                std::cmp::Reverse(key.as_str()),
            )
        })
        .map(|(_, value)| value.clone())
}

/// Brings this worktree's private services up, of either kind, and waits
/// for them.
///
/// Compose first, then native, and a native service that fails takes the
/// containers down with it — "a service that never comes up leaves
/// nothing running" is the rule the compose half already follows, and a
/// worktree half in one mode and half in another is worse than either.
pub(super) fn bring_up_services(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
    progress: &dyn Fn(&str),
) -> Result<Fresh> {
    bring_up_compose_services(paths, config, name, worktree, ports, progress)?;
    let e = match bring_up_native_services(paths, config, name, worktree, ports, progress) {
        Ok(fresh) => return Ok(fresh),
        Err(e) => e,
    };
    if !compose_entries(config).is_empty() {
        let project = crate::compose::project_name(paths.project_id(), name);
        let program = services::docker_program(paths);
        let _ = services::Compose::by_project(&program, &project).stop();
    }
    Err(e)
}

/// Everything a compose bring-up needs, decided before anything runs.
struct ComposePlan {
    project: String,
    program: PathBuf,
    files: Vec<PathBuf>,
    published: Vec<crate::compose::Published>,
    wanted: Vec<services::Wanted>,
    include: Vec<String>,
    /// The image each included service runs, beside it, when the file
    /// names one: what a native recipe could stand in for.
    images: Vec<(String, Option<String>)>,
    timeout: u64,
}

/// Reads the compose files, resolves the included services, and refuses
/// what cannot be isolated — a bind mount into the repository, a service
/// the file does not have — without writing or starting anything.
///
/// `None` when this project has no compose services to bring up.
fn plan_compose(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> Result<Option<ComposePlan>> {
    let entries = compose_entries(config);
    if entries.is_empty() {
        return Ok(None);
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut published: Vec<crate::compose::Published> = Vec::new();
    let mut wanted: Vec<services::Wanted> = Vec::new();
    let mut include: Vec<String> = Vec::new();
    let mut images: Vec<(String, Option<String>)> = Vec::new();
    let mut timeout = services::DEFAULT_READY_TIMEOUT_S;
    // Everywhere a bind mount must not land: the main checkout, the
    // directory every worktree lives under, and this worktree itself for
    // the adopted case, where it is somewhere else entirely.
    let repository = vec![
        paths.root().to_path_buf(),
        config.worktrees_dir(paths),
        worktree.to_path_buf(),
    ];
    let project = crate::compose::project_name(paths.project_id(), name);
    let program = services::docker_program(paths);
    for entry in &entries {
        let file = crate::compose::file_in(worktree, entry.file)?;
        let mut parsed = crate::compose::read(&file)?;
        // `extends:` and a top-level `include:` put the real definition in
        // a file this reader does not follow, so what it read is not what
        // compose would run. Compose can say; it is already the thing
        // about to bring the services up.
        if parsed.unresolved.any()
            && let Ok(resolved) =
                services::Compose::new(&program, &project, vec![file.clone()], worktree).config()
        {
            parsed = resolved;
        }
        for (service, container) in
            crate::compose::resolve_included(&parsed, entry.include, &repository)?
        {
            let host = *ports
                .get(&service)
                .with_context(|| format!("no port was allocated for the service {service:?}"))?;
            wanted.push(services::Wanted {
                service: service.clone(),
                port: host,
                healthcheck: parsed.services[&service].healthcheck,
            });
            published.push(crate::compose::Published {
                service: service.clone(),
                container,
                host,
            });
            images.push((service.clone(), parsed.services[&service].image.clone()));
            include.push(service);
        }
        files.push(file);
    }
    for entry in &entries {
        if let Some(configured) = entry.ready_timeout_s {
            timeout = configured;
        }
    }
    // A `[[services]]` entry with an empty `include` is the written-down
    // answer "none of them". `docker compose up -d` with no service named
    // brings up *everything* in the file, on the ports the project
    // hardcoded, which is the opposite of what was asked for.
    if include.is_empty() {
        return Ok(None);
    }
    Ok(Some(ComposePlan {
        project,
        program,
        files,
        published,
        wanted,
        include,
        images,
        timeout,
    }))
}

/// Brings this worktree's private *containers* up, waits for them, and
/// starts a log pump in front of each one.
///
/// The override that remaps the ports is regenerated every time: the
/// ports can move, the compose file can change under a rebase, and a
/// stale override would publish a port nothing is on.
fn bring_up_compose_services(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let Some(ComposePlan {
        project,
        program,
        mut files,
        published,
        wanted,
        include,
        timeout,
        ..
    }) = plan_compose(paths, config, name, worktree, ports)?
    else {
        return Ok(());
    };

    let override_file = paths.compose_override_file(name);
    if let Some(parent) = override_file.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(
        &override_file,
        crate::compose::render_override(name, &published),
    )
    .with_context(|| format!("write {}", override_file.display()))?;
    files.push(override_file);

    let compose = services::Compose::new(&program, &project, files, worktree);
    // Written down before compose is asked, not after: from here on there
    // may be a container and a volume under this project, and the record
    // is the only thing `rm` can find them by.
    name_compose_project(paths, name, &project, &include)?;
    progress(&format!("starting services: {}", include.join(", ")));

    // A service that never comes up leaves nothing running: the ones that
    // did are stopped again, so a failed start does not leave half an
    // environment holding ports. That covers an `up` that failed partway —
    // it may have created some containers before the one that failed —
    // and a log pump that could not be started in front of containers
    // that are up.
    //
    // By project, not by files. `up -d <name>` enables that service's
    // profile implicitly; a `stop` with the same `-f` files does not, and
    // leaves a profiled container running on the port it was allocated.
    // Compose finds every container it created by label, which is why
    // `stop` and `rm` use this form too.
    let brought_up = compose
        .up(&include)
        .and_then(|()| {
            services::wait_ready(&compose, &wanted, Duration::from_secs(timeout), progress)
        })
        .and_then(|()| pump_service_logs(paths, name, &compose, &include, worktree));
    if let Err(e) = brought_up {
        let _ = services::Compose::by_project(&program, &project).stop();
        return Err(e);
    }
    Ok(())
}

/// Records the compose project on the service records it is about to
/// create containers for.
fn name_compose_project(
    paths: &PandoPaths,
    name: &str,
    project: &str,
    include: &[String],
) -> Result<()> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(());
    };
    let mut changed = false;
    for service in record.services.iter_mut() {
        if service.kind == state::ServiceKind::Compose
            && include.contains(&service.name)
            && service.compose_project.as_deref() != Some(project)
        {
            service.compose_project = Some(project.to_string());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    state::save(&paths.state_file(), &store)
}

/// Every native service this project runs, resolved and planned, with
/// its engine checked — nothing created or spawned.
fn plan_native(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> Result<Vec<native::Native>> {
    let entries = native::Entry::all(config);
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    let urls = native_urls(paths, config, worktree, ports);
    let mut planned: Vec<native::Native> = Vec::new();
    for entry in &entries {
        let resolved = native::resolve(&recipes, entry)?;
        let port = *ports
            .get(entry.name)
            .with_context(|| format!("no port was allocated for the service {:?}", entry.name))?;
        let service = native::Native::plan(
            paths,
            name,
            entry.name,
            resolved.recipe,
            port,
            urls.get(entry.name).map(String::as_str),
        )?;
        let missing = service.missing_binaries();
        if !missing.is_empty() {
            return Err(service.missing_binaries_error(&missing));
        }
        planned.push(service);
    }
    Ok(planned)
}

/// Everything an isolated start needs that can be checked without
/// changing anything: the env the app will be given, the compose files
/// and what they include, every native recipe and its engine, and — last,
/// because it is the one question that leaves the machine — whether the
/// Docker daemon answers.
///
/// Asked *before* a start stops anything. A start that takes a running
/// worktree down and only then discovers Docker is off has destroyed a
/// working environment over a request that could never succeed. `roles`
/// is every role the isolated worktree would have; the numbers are
/// placeholders, because nothing here depends on which port is which.
pub(super) fn preflight_isolation(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    roles: &[String],
) -> Result<()> {
    let ports = placeholder_ports(roles);
    resolve_service_env(paths, config, worktree, &ports)?;
    plan_native(paths, config, name, worktree, &ports)?;
    backends_reachable(paths, config, name, worktree, &ports)
}

/// The part of [`preflight_isolation`] that asks the machine rather than
/// the config: whether Docker answers, for a project with compose
/// services.
///
/// Also run before a start's questions, so a start that cannot happen
/// asks nothing first — "which command brings the schema up?" answered,
/// and only then "Docker is not running", is a question wasted on an
/// impossible request.
pub(super) fn backends_reachable(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> Result<()> {
    let Some(plan) = plan_compose(paths, config, name, worktree, ports)? else {
        return Ok(());
    };
    match services::Compose::by_project(&plan.program, &plan.project).reachable() {
        Err(e) if services::is_daemon_down(&e) => {
            Err(match native_instead(paths, config, &plan.images) {
                Some(offer) => anyhow::anyhow!("{e} — {offer}"),
                None => e,
            })
        }
        reached => reached,
    }
}

/// Placeholder ports for the roles, for a check that plans the services
/// without reserving anything.
pub(super) fn placeholder_ports(roles: &[String]) -> BTreeMap<String, u16> {
    roles
        .iter()
        .enumerate()
        .map(|(i, role)| (role.clone(), ports::PORT_MIN.saturating_add(i as u16)))
        .collect()
}

/// With Docker down, the other way this machine could isolate these
/// services, when it has one: every one of them is something a recipe
/// runs, and every recipe's engine is installed here.
///
/// Said, never done. Which mechanism a developer wants is a preference
/// about their laptop, recorded once in the user layer, and a start that
/// switched on its own because Docker happened to be off would write a
/// decision nobody made. So this names the setting and the file it lives
/// in — and the `[[services]]` entry to delete, because the services
/// question is only asked while config has none, and the preference only
/// decides it when it is asked.
fn native_instead(
    paths: &PandoPaths,
    config: &Config,
    images: &[(String, Option<String>)],
) -> Option<String> {
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    let mut names: Vec<String> = Vec::new();
    for (service, image) in images {
        // By image first — a service called `db` running `postgres:16` is
        // a postgres — then by the service's own name.
        let recipe = image
            .as_deref()
            .map(crate::catalog::images::image_name)
            .into_iter()
            .chain(std::iter::once(service.as_str()))
            .find_map(|name| {
                recipes
                    .get(name)
                    .ok()
                    .filter(|l| l.recipe.service().is_some())
            })?;
        if !names.contains(&recipe.recipe.name) {
            names.push(recipe.recipe.name.clone());
        }
    }
    if names.is_empty() {
        return None;
    }
    // One login shell, and only on this path: Docker is already known to
    // be down, and the start is already failing.
    let evidence = super::init::machine_evidence(paths, &recipes);
    if !names
        .iter()
        .all(|name| evidence.can_run(name) == Some(true))
    {
        return None;
    }
    Some(format!(
        "this machine can also run {} natively, and two edits switch it: set `[isolation] \
         prefer = \"native\"` in {}, and delete {} from {} so the next isolated start asks again",
        names.join(", "),
        paths.user_config_file().display(),
        compose_entries_named(config),
        paths.config_file().display()
    ))
}

/// The compose `[[services]]` entries, counted and named by the services
/// each includes: what a developer has to find in the file to delete.
pub(super) fn compose_entries_named(config: &Config) -> String {
    // An entry that includes nothing is named by its file instead.
    let named = |entry: &ComposeEntry<'_>| match entry.include.is_empty() {
        true => entry.file.to_string(),
        false => entry.include.join(", "),
    };
    match compose_entries(config).as_slice() {
        [one] => format!("the compose `[[services]]` entry ({})", named(one)),
        many => format!(
            "the {} compose `[[services]]` entries ({})",
            many.len(),
            many.iter().map(named).collect::<Vec<_>>().join("; ")
        ),
    }
}

/// Forgets every service record whose service was never brought up: a
/// compose record no compose project was ever named on, and a native
/// record with no server behind it.
///
/// What a shared worktree and a failed isolated start both have to do. A
/// record that points at nothing puts a `postgres down` row on the screen
/// of a worktree that has no postgres of its own, and a port in its list
/// it does not own. A compose record that *does* name a project is kept:
/// its volume may exist, and only that record can take it down.
pub(super) fn forget_unstarted_services(record: &mut WorktreeRecord) {
    record.services.retain(|service| match service.kind {
        state::ServiceKind::Compose => service.compose_project.is_some() || service.pid.is_some(),
        state::ServiceKind::Native => service.pid.is_some(),
    });
}

/// Undoes what a start that was switching a worktree to isolated wrote
/// before it failed: the service records, and the service roles in its
/// port list. The worktree goes back to looking like the shared one it
/// still is, and the next plain start is a clean shared one.
///
/// And whatever came up is taken down again: the start may have failed
/// *after* the services were ready — a migration hook, a probe, a command
/// that would not render — and the worktree's processes, which kept
/// running on the shared services all along, must not be left beside
/// private copies nothing uses. Containers compose may have created keep
/// their records — without a port — so `rm` can still take their volumes.
///
/// `shared_ports` is the port list from before the switch re-derived it:
/// the processes kept serving through the switch are on those numbers, so
/// the record says so again.
pub(super) fn undo_failed_isolation(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    shared_ports: &BTreeMap<String, u16>,
) {
    let projects = forget_failed_isolation(paths, config, name, shared_ports);
    // Outside the lock, like every other compose call. By project, so a
    // container compose created before `up` itself failed is found too.
    // Best effort: the error being reported is the start's, not this one.
    if !projects.is_empty() {
        let program = services::docker_program(paths);
        for project in projects {
            let _ = services::Compose::by_project(&program, project.as_str()).stop();
        }
    }
}

/// The record half of [`undo_failed_isolation`], under the lock. Returns
/// the compose projects that still have to be stopped.
fn forget_failed_isolation(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    shared_ports: &BTreeMap<String, u16>,
) -> Vec<String> {
    let Ok(_lock) = state::lock(&paths.lock_file()) else {
        return Vec::new();
    };
    let Ok(mut store) = state::load(&paths.state_file()) else {
        return Vec::new();
    };
    let Some(record) = store.worktrees.get_mut(name) else {
        return Vec::new();
    };
    // Another start of this worktree finished the same switch while this
    // one was failing: the record is isolated and its processes, spawned
    // against those services, are live. Tearing the services down now
    // would pull the database out from under a start that worked. A
    // failing start never sets the flag itself — it is set only right
    // before the spawn — so a set flag is always the other start's.
    if switched_by_another_start(record) {
        return Vec::new();
    }
    // Every log pump, and every native server — whose pid *is* the
    // service — that came up before the failure.
    let _ = stop_service_pumps(record, &|pgid| proc::stop(pgid, STOP_GRACE));
    clear_native_sockets(paths, name, record);
    let projects = compose_projects(record);
    for service in record.services.iter_mut() {
        service.port = None;
    }
    forget_unstarted_services(record);
    let owned_by_processes: Vec<String> = config
        .processes
        .values()
        .flat_map(|process| process.roles())
        .collect();
    let roles = service_roles(config);
    record
        .ports
        .retain(|role, _| !roles.contains(role) || owned_by_processes.contains(role));
    for role in &owned_by_processes {
        if let Some(port) = shared_ports.get(role) {
            record.ports.insert(role.clone(), *port);
        }
    }
    record.isolated = false;
    let _ = state::save(&paths.state_file(), &store);
    projects
}

/// Whether a record is isolated with a live process running against its
/// services — a switch some other start completed.
fn switched_by_another_start(record: &WorktreeRecord) -> bool {
    record.isolated
        && record.processes.values().any(|p| {
            matches!(
                p.phase,
                state::Phase::Starting { .. } | state::Phase::Running { .. }
            ) && proc::is_alive(p.pid)
        })
}

/// Brings this worktree's native services up: initialise once, spawn
/// detached, wait for the recipe's own check, then let the recipe create
/// whatever the app's own URL names.
///
/// Planned in full before anything is created or spawned, the way
/// processes are: a second service whose recipe does not resolve, or
/// whose engine is not installed, must not leave the first one's data
/// directory behind a failed start. The engine check is part of planning
/// for the same reason — a start that initialises a cluster and *then*
/// discovers there is no server to run against it has done work for
/// nothing.
fn bring_up_native_services(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
    progress: &dyn Fn(&str),
) -> Result<Fresh> {
    let planned = plan_native(paths, config, name, worktree, ports)?;
    if planned.is_empty() {
        return Ok(Fresh(false));
    }
    for service in &planned {
        // Said before it is started, not after it fails: a recipe nobody
        // has run against a real server is the first thing to suspect,
        // and a developer cannot guess that from a timeout.
        if service.recipe.untested {
            progress(&format!(
                "{}: the {:?} recipe has never been run against a real server — if this does \
                 not work, the recipe is the first thing to suspect",
                service.service, service.recipe.name
            ));
        }
    }

    // A server that is already running is left exactly as it is. Starting
    // a second one on the same data directory is how a database gets
    // corrupted, and it is what a plain `start` beside a live isolated
    // worktree would otherwise do.
    let live = live_native_services(paths, name)?;

    let mut started: Vec<native::Native> = Vec::new();
    let mut fresh = Fresh(false);
    for service in planned {
        if live.contains(&service.service) {
            continue;
        }
        match start_one_native(paths, name, &service, progress) {
            Ok(native::Init::Ran) => fresh = Fresh(true),
            Ok(_) => {}
            Err(e) => {
                stop_native_services(paths, name, &started);
                return Err(e);
            }
        }
        started.push(service);
    }
    for service in &started {
        let Some(pid) = recorded_native_pid(paths, name, &service.service)? else {
            continue;
        };
        let waited = service
            .wait_ready(pid, service.ready_timeout(), progress)
            .and_then(|()| service.create(progress));
        if let Err(e) = waited {
            stop_native_services(paths, name, &started);
            return Err(e);
        }
    }
    Ok(fresh)
}

/// Whether a start built a data directory that was not there before, and
/// so handed the application an empty database.
///
/// Carried out of the bring-up rather than worked out again, because the
/// only place that knows is the one that ran the init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fresh(pub(super) bool);

/// Initialises, spawns, and records one native service — recorded under
/// the lock before anything waits on it, because a readiness failure has
/// to leave a pgid the orphan sweep can find.
fn start_one_native(
    paths: &PandoPaths,
    name: &str,
    service: &native::Native,
    progress: &dyn Fn(&str),
) -> Result<native::Init> {
    let init = service.ensure_init(progress)?;
    reset_log(&service.log_file)?;
    progress(&format!("starting services: {}", service.service));
    let spawned = service.spawn()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    if let Some(record) = store
        .worktrees
        .get_mut(name)
        .and_then(|r| r.services.iter_mut().find(|s| s.name == service.service))
    {
        record.pid = Some(spawned.pid);
        record.pgid = Some(spawned.pgid);
    }
    state::save(&paths.state_file(), &store)?;
    Ok(init)
}

/// The native services of this worktree whose server is still alive.
fn live_native_services(paths: &PandoPaths, name: &str) -> Result<Vec<String>> {
    let store = state::load(&paths.state_file())?;
    Ok(store
        .worktrees
        .get(name)
        .map(|record| {
            record
                .services
                .iter()
                .filter(|s| s.kind == state::ServiceKind::Native)
                .filter(|s| s.pid.is_some_and(proc::is_alive))
                .map(|s| s.name.clone())
                .collect()
        })
        .unwrap_or_default())
}

fn recorded_native_pid(paths: &PandoPaths, name: &str, service: &str) -> Result<Option<u32>> {
    let store = state::load(&paths.state_file())?;
    Ok(store
        .worktrees
        .get(name)
        .and_then(|record| record.services.iter().find(|s| s.name == service))
        .and_then(|s| s.pid))
}

/// Signals every native service this call started and forgets its pid, so
/// a failed start leaves no server holding a port and no record claiming
/// one is there.
fn stop_native_services(paths: &PandoPaths, name: &str, started: &[native::Native]) {
    if started.is_empty() {
        return;
    }
    let Ok(_lock) = state::lock(&paths.lock_file()) else {
        return;
    };
    let Ok(mut store) = state::load(&paths.state_file()) else {
        return;
    };
    for service in started {
        let Some(record) = store
            .worktrees
            .get_mut(name)
            .and_then(|r| r.services.iter_mut().find(|s| s.name == service.service))
        else {
            continue;
        };
        if let Some(pgid) = record.pgid {
            let _ = proc::stop(pgid, STOP_GRACE);
        }
        record.pid = None;
        record.pgid = None;
        let _ = std::fs::remove_dir_all(&service.socket_dir);
    }
    let _ = state::save(&paths.state_file(), &store);
}

/// Removes the socket directories of a worktree's native services.
///
/// The one thing pando puts outside its own home, so the one thing a stop
/// has to clean up by hand. Harmless if it is already gone: a server that
/// shut down cleanly took its own socket with it.
pub(super) fn clear_native_sockets(paths: &PandoPaths, name: &str, record: &WorktreeRecord) {
    for service in &record.services {
        if service.kind != state::ServiceKind::Native {
            continue;
        }
        let dir = paths.service_socket_dir(name, &service.name);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Forgets the recorded fingerprints of every hook that runs *after* the
/// services, so the next gate lets them through.
///
/// The bug this exists for is the worst shape a bug can have. Switching a
/// worktree to isolated hands it a brand new, empty database; the
/// migration hook's fingerprint — a hash of the migration files and the
/// command — has not changed, so the hook is skipped, and the application
/// meets an empty schema. Nothing fails at `start`. It fails later, in the
/// app, somewhere else entirely.
///
/// The fingerprint answers "have this hook's *inputs* changed", and the
/// database it runs against is an input it cannot see. So when the
/// database changes underneath it — a mode change in either direction, or
/// a data directory that was just initialised — the recorded answer is
/// discarded rather than trusted.
///
/// Only the points that run after the services: `create` and `install`
/// are about the worktree and its dependencies, and a new database is no
/// reason to install again.
pub(super) fn forget_hooks_after_services(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    why: &str,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let after_services: Vec<String> = config
        .hooks
        .iter()
        .filter(|hook| {
            matches!(
                hook.after,
                config::HookPoint::Services | config::HookPoint::Dev
            )
        })
        .map(|hook| hook.name.clone())
        .collect();
    if after_services.is_empty() {
        return Ok(());
    }
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(());
    };
    let mut forgotten: Vec<String> = Vec::new();
    for hook in &after_services {
        if record.hooks.remove(hook).is_some() {
            forgotten.push(hook.clone());
        }
    }
    if forgotten.is_empty() {
        return Ok(());
    }
    // Said out loud: a hook running that a developer expected to be
    // skipped is a surprise, and the reason for it is not guessable.
    progress(&format!(
        "{why}, so {} will run again",
        forgotten.join(", ")
    ));
    state::save(&paths.state_file(), &store)
}

/// One detached `docker compose logs -f` per service, writing into the
/// worktree's log directory so the viewer has a tab for it.
///
/// Recorded as the service's pid and pgid, which is what makes it a
/// process `stop` signals and the orphan sweep covers.
fn pump_service_logs(
    paths: &PandoPaths,
    name: &str,
    compose: &services::Compose,
    include: &[String],
    worktree: &Path,
) -> Result<()> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    for service in include {
        let running = store
            .worktrees
            .get(name)
            .and_then(|r| r.services.iter().find(|s| &s.name == service))
            .and_then(|s| s.pid)
            .is_some_and(proc::is_alive);
        if running {
            continue;
        }
        let log_file = paths.log_file(name, service);
        reset_log(&log_file)?;
        let spawned = proc::spawn_detached(SpawnOptions {
            shell_cmd: &compose.logs_shell_cmd(service),
            cwd: worktree,
            log_file: &log_file,
            env: &[],
            status_file: None,
        })
        .with_context(|| format!("start the log pump for the service {service:?}"))?;
        if let Some(record) = store
            .worktrees
            .get_mut(name)
            .and_then(|r| r.services.iter_mut().find(|s| &s.name == service))
        {
            record.pid = Some(spawned.pid);
            record.pgid = Some(spawned.pgid);
        }
    }
    state::save(&paths.state_file(), &store)
}

/// Signals every log pump of a worktree and forgets it, leaving the rest
/// of the service record — the port and the compose project — in place.
pub(super) fn stop_service_pumps(
    record: &mut WorktreeRecord,
    stop: &impl Fn(i32) -> Result<()>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for service in record.services.iter_mut() {
        let Some(pgid) = service.pgid else { continue };
        match stop(pgid) {
            Ok(()) => {
                service.pid = None;
                service.pgid = None;
            }
            Err(e) => failures.push(format!(
                "the log pump for {} (group {pgid}): {e:#}",
                service.name
            )),
        }
    }
    failures
}

/// Whether any of a worktree's service records has a live process behind
/// it: a compose log pump, or a native server.
pub(super) fn has_live_services(record: &WorktreeRecord) -> bool {
    record
        .services
        .iter()
        .any(|service| service.pid.is_some_and(proc::is_alive))
}

/// The compose projects a worktree's records name, each once.
pub(super) fn compose_projects(record: &WorktreeRecord) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for service in &record.services {
        if service.kind != state::ServiceKind::Compose {
            continue;
        }
        if let Some(project) = &service.compose_project
            && !out.contains(project)
        {
            out.push(project.clone());
        }
    }
    out
}

/// One of a worktree's private services, as `status` and the TUI show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub port: Option<u16>,
    /// Whether something answers on that port right now.
    pub up: bool,
    /// Whether a log pump is running in front of it, so this worktree's
    /// log tab for the service is still being filled.
    ///
    /// A read path only ever *reports* this: the pump comes back on the
    /// next `start` or `restart`, and nothing that merely looks at state
    /// is allowed to spawn a process.
    pub logging: bool,
}

/// Every service a worktree owns, with whether it is answering.
///
/// A TCP connect, never a bind — the same rule readiness follows, for the
/// same reason: a bind probe would take the port from the container that
/// owns it. It says nothing about *whose* listener answered, which is
/// exactly as much as a status line needs to claim.
///
/// A worktree on the shared services runs none of its own, so a record it
/// still carries with no port is not a service: it is what `start
/// --shared`, a failed switch to isolated, or 0.2.0 left behind to name a
/// compose project whose volumes `rm` has to take down. It is kept for
/// that — pando cannot find those volumes again without it, and asking
/// Docker from a read path to decide whether they exist is not a read
/// path's job — and it is not shown, because a `postgres  no port` row on
/// a worktree that has no postgres of its own is a service that is not
/// there. A record that has a port is shown whatever the flag says: that
/// is a start bringing the services up before it switches the worktree
/// over to them.
pub fn service_statuses(record: &WorktreeRecord) -> Vec<ServiceStatus> {
    recorded_service_statuses(record)
        .into_iter()
        .filter(|status| record.isolated || status.port.is_some())
        .collect()
}

/// Every service record a worktree carries, leftovers included — what
/// `status --json` publishes. The shape was published with them in it, and
/// a program reads `project` there to find the volumes; hiding a row is a
/// decision about a screen, not about the contract.
pub fn recorded_service_statuses(record: &WorktreeRecord) -> Vec<ServiceStatus> {
    record
        .services
        .iter()
        .map(|service| ServiceStatus {
            name: service.name.clone(),
            port: service.port,
            up: service.port.map(ports::something_is_listening) == Some(true),
            logging: service.pid.is_some_and(proc::is_alive),
        })
        .collect()
}

/// The project's *shared* services and whether each is answering.
///
/// Shared mode runs no containers of pando's, so there is nothing in state
/// to look at: the port comes from the main checkout's own env files,
/// through the key the `[[services]]` entry maps to that service. That is
/// the number the developer's own `docker compose up` published, which is
/// exactly what the header chip is claiming to know about.
pub fn shared_service_statuses(paths: &PandoPaths, config: &Config) -> Vec<ServiceStatus> {
    let mut out: Vec<ServiceStatus> = Vec::new();
    let mut add = |key: &str, service: &str| {
        if out.iter().any(|status| status.name == service) {
            return;
        }
        let port = services::port_in_env(paths.root(), key);
        out.push(ServiceStatus {
            name: service.to_string(),
            port,
            up: port.map(ports::something_is_listening) == Some(true),
            // Shared services are the developer's own `docker compose up`
            // or their own `brew services start`; pando runs no pump in
            // front of anything it did not start.
            logging: false,
        });
    };
    for entry in compose_entries(config) {
        for (key, service) in entry.env {
            add(key, service);
        }
    }
    let native = native::Entry::all(config);
    if !native.is_empty() {
        let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
        for entry in &native {
            let recipe = native::resolve(&recipes, entry).ok().map(|r| r.recipe);
            let (mapping, _) = entry.env_map(recipe.as_ref());
            for (key, service) in &mapping {
                add(key, service);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The environment a command run by hand inside a worktree needs, so that
/// it reaches the same database and the same ports the dev processes do.
///
/// This is what `status --env` prints. It replaces materialising a
/// rewritten `.env` inside the worktree, which Invariant 1 forbids unless
/// the project already ignores that path — and which would be wrong the
/// moment two worktrees disagreed about it.
///
/// Merged in one fixed order — the services, then each process by name,
/// then pando's own variables — so two runs of the same command print the
/// same thing.
pub fn resolved_env(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let worktree = find_worktree(paths, name)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());
    let store = state::load(&paths.state_file())?;
    let record = store
        .worktrees
        .get(name)
        .with_context(|| format!("pando has no record of {name}"))?;
    if record.ports.is_empty() {
        bail!(
            "{name} has no ports yet — start it once, and `pando status --env {name}` can say \
             where everything is"
        );
    }
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if record.isolated {
        out.extend(resolve_service_env(
            paths,
            config,
            &canonical,
            &record.ports,
        )?);
    }
    for (process_name, process) in &config.processes {
        let log_file = paths.log_file(name, process_name);
        let ctx = template::Context {
            name,
            branch: worktree.branch.as_deref(),
            worktree: &canonical,
            root: paths.root(),
            project: paths.project_id(),
            ports: &record.ports,
            default_role: None,
            log: Some(&log_file),
        };
        // Through the same function the processes are started with, so a
        // developer who evals this gets exactly what the dev server got.
        for (var, value) in process_env(paths, name, &worktree, process, &out.clone(), &ctx)
            .with_context(|| format!("in process {process_name}"))?
        {
            out.insert(var, value);
        }
    }
    Ok(out)
}

/// `export KEY='value'` lines a shell can `eval`.
///
/// Single quotes with the close-escape-reopen trick, because a value can
/// hold anything: a password with a `$` in it must not be expanded, and a
/// value with a quote in it must not end the string early.
pub fn export_lines(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str(&format!(
            "export {key}='{}'\n",
            value.replace('\'', "'\\''")
        ));
    }
    out
}

/// The URL a worktree serves on. One worktree, one URL, however many
/// processes it runs — and one function, because `start`, `status`, `ls`,
/// the TUI's row and the TUI's `o` key all have to hand out the same one.
///
/// The role is `web` whenever any process owns it, because that is the role
/// everything else defaults to; otherwise the first role of the
/// alphabetically first process that owns one. The port is what that
/// process is really listening on when that is known, and the port pando
/// assigned it otherwise.
///
/// From the record alone: `status` and the TUI never see config, and the
/// answer has to survive a stop unchanged.
pub fn worktree_url(record: &WorktreeRecord) -> Option<String> {
    let role = url_role(record)?;
    let assigned = *record.ports.get(&role)?;
    Some(format!(
        "http://localhost:{}",
        observed_port_for_role(record, &role).unwrap_or(assigned)
    ))
}

/// The role a worktree's URL points at: `web` wherever anything owns it,
/// else the first role of the alphabetically first process that owns one.
pub(super) fn url_role(record: &WorktreeRecord) -> Option<String> {
    if record.ports.contains_key(DEFAULT_READY_ROLE) {
        return Some(DEFAULT_READY_ROLE.to_string());
    }
    record
        .roles
        .values()
        .find_map(|roles| roles.first())
        .filter(|role| record.ports.contains_key(*role))
        .cloned()
        // A record written before pando kept track of who owns what: the
        // ports are all there is to go on.
        .or_else(|| record.ports.keys().next().cloned())
}

/// The port the process that owns `role` is really listening on.
///
/// A framework that ignores `PORT`, or one that picked the next free
/// number, is followed: the URL is what is really serving, not what pando
/// asked for. But only within the owning process's own group. An observed
/// port belongs to whichever group opened it, and with several processes a
/// port nobody claimed is far more often the *other* process's HMR socket
/// or debugger than this role's server — which is how a worktree whose web
/// server was stopped handed out the api's second port as its URL.
pub(super) fn observed_port_for_role(record: &WorktreeRecord, role: &str) -> Option<u16> {
    let assigned = *record.ports.get(role)?;
    let owner = record
        .roles
        .iter()
        .find(|(_, roles)| roles.iter().any(|owned| owned == role))
        .map(|(process, _)| process)?;
    let process = record.processes.get(owner)?;
    // A process that is not up is not listening on anything, and what it
    // was last seen holding says nothing about now.
    if !matches!(
        process.phase,
        Phase::Starting { .. } | Phase::Running { .. }
    ) {
        return None;
    }
    if process.observed_ports.contains(&assigned) {
        return Some(assigned);
    }
    process
        .observed_ports
        .iter()
        .copied()
        .find(|observed| !record.ports.values().any(|port| port == observed))
}
