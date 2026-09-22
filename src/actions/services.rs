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

/// Runs one compose verb against every project a worktree owns, reporting
/// the failures together.
pub(super) fn stop_compose_projects(
    paths: &PandoPaths,
    projects: &[String],
    run: impl Fn(&services::Compose) -> Result<()>,
) -> Result<()> {
    if projects.is_empty() {
        return Ok(());
    }
    let program = services::docker_program(paths);
    let mut failures = Vec::new();
    for project in projects {
        let compose = services::Compose::by_project(&program, project.as_str());
        if let Err(e) = run(&compose) {
            failures.push(format!("{project}: {e:#}"));
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
    paths: &PandoPaths,
    name: &str,
    ports: &BTreeMap<String, u16>,
    record: &WorktreeRecord,
) -> Vec<state::ServiceRecord> {
    let project = crate::compose::project_name(paths.project_id(), name);
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
            compose_project: (!is_native).then(|| project.clone()),
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
        let Ok(resolved) = services::app_env(worktree, &mapping, ports) else {
            continue;
        };
        if let Some(value) = resolved.into_values().next() {
            out.insert(entry.name.to_string(), value);
        }
    }
    out
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
    let entries = compose_entries(config);
    if entries.is_empty() {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut published: Vec<crate::compose::Published> = Vec::new();
    let mut wanted: Vec<services::Wanted> = Vec::new();
    let mut include: Vec<String> = Vec::new();
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
        return Ok(());
    }

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
    progress(&format!("starting services: {}", include.join(", ")));
    compose.up(&include)?;

    // A service that never comes up leaves nothing running: the ones that
    // did are stopped again, so a failed start does not leave half an
    // environment holding ports.
    //
    // By project, not by files. `up -d <name>` enables that service's
    // profile implicitly; a `stop` with the same `-f` files does not, and
    // leaves a profiled container running on the port it was allocated.
    // Compose finds every container it created by label, which is why
    // `stop` and `rm` use this form too.
    if let Err(e) = services::wait_ready(&compose, &wanted, Duration::from_secs(timeout), progress)
    {
        let _ = services::Compose::by_project(&program, &project).stop();
        return Err(e);
    }

    pump_service_logs(paths, name, &compose, &include, worktree)
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
    let entries = native::Entry::all(config);
    if entries.is_empty() {
        return Ok(Fresh(false));
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
        // Said before it is started, not after it fails: a recipe nobody
        // has run against a real server is the first thing to suspect,
        // and a developer cannot guess that from a timeout.
        if service.recipe.untested {
            progress(&format!(
                "{}: the {:?} recipe has never been run against a real server — if this does \
                 not work, the recipe is the first thing to suspect",
                entry.name, service.recipe.name
            ));
        }
        planned.push(service);
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

/// Remembers that this worktree runs private services, once they are up.
///
/// Written *after* `bring_up_services` rather than with the rest of the
/// record, because the flag is what a later plain `start` reads to keep
/// using them: a start that failed before any container existed — a
/// mapping the env rewriter cannot satisfy, a service the compose file
/// cannot isolate — would otherwise leave the worktree unable to start at
/// all, isolated or shared, until `pando.toml` was edited by hand.
///
/// A worktree that was already isolated keeps the flag through such a
/// failure, because its containers are real and nothing here clears it.
pub(super) fn remember_isolated(paths: &PandoPaths, name: &str) -> Result<()> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(());
    };
    if record.isolated {
        return Ok(());
    }
    record.isolated = true;
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
pub fn service_statuses(record: &WorktreeRecord) -> Vec<ServiceStatus> {
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
