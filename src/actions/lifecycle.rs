//! Start, stop, restart.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, Config, ProcessConfig};
use crate::detect;
use crate::paths::PandoPaths;
use crate::ports;
use crate::process::{self as proc, SpawnOptions};
use crate::state::{self, Phase, ProcessRecord, WorktreeRecord};
use crate::template;
use crate::worktree::Worktree;

use super::hooks::{HookContext, run_hooks, run_probes};
use super::refresh::advance_before_reconcile;
use super::runtime::with_prelude;
use super::services::{
    Fresh, bring_up_services, clear_native_sockets, compose_projects, forget_hooks_after_services,
    planned_services, remember_isolated, resolve_service_env, service_roles, stop_compose_projects,
    stop_service_pumps, worktree_url,
};
use super::share::{sweep_dead_shares_with, take_share_down};
// Only for the intra-doc link above `sweep_orphaned_groups`.
#[cfg(doc)]
use super::share::sweep_dead_shares;
use super::worktree::find_worktree;

/// How long a process group gets to exit on its own before SIGKILL.
pub(super) const STOP_GRACE: Duration = Duration::from_secs(5);

/// The role `share` and the browser-open key default to, and the role a
/// readiness rule watches when none is named.
pub(super) const DEFAULT_READY_ROLE: &str = "web";

/// What a start does about this worktree's services.
///
/// One value rather than two flags, because "isolated and shared" is not a
/// state: the command line refuses the pair, and nothing downstream has to
/// decide what it would have meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Whatever this worktree already does: private copies if it has them,
    /// the project's shared services if it does not. A plain `start`.
    #[default]
    Remembered,
    /// Private copies of the project's services, for this worktree alone.
    Isolated,
    /// The project's shared services, and the private copies stopped. The
    /// way back.
    Shared,
}

impl Mode {
    /// The mode two flags mean. `start` and `restart` refuse the pair
    /// before this is ever called.
    pub fn of(isolated: bool, shared: bool) -> Mode {
        match (isolated, shared) {
            (true, _) => Mode::Isolated,
            (_, true) => Mode::Shared,
            _ => Mode::Remembered,
        }
    }
}

/// One process a start brought up, or found already up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedProcess {
    /// The process's name in config — `dev` for the `[dev]` shorthand.
    pub process: String,
    pub record: ProcessRecord,
}

/// What one `start` did to a worktree.
///
/// A worktree has as many processes as its config declares, so a start is
/// never one thing: some come up, some were already running, and the ports
/// and the URL belong to the worktree rather than to any one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartReport {
    pub worktree: String,
    /// Spawned by this call, in the order they were spawned.
    pub started: Vec<StartedProcess>,
    /// Already up, and left exactly as they were.
    pub already_running: Vec<StartedProcess>,
    /// Every role of the worktree, whatever `--only` asked for: ports are
    /// reserved for the whole worktree at once, so starting one process
    /// never moves another one's port.
    pub ports: BTreeMap<String, u16>,
    /// The `web` role's URL when any process owns that role, else the
    /// first role of the alphabetically first process that owns one — the
    /// same rule, through the same function, that `status`, `ls` and the
    /// TUI use. See [`worktree_url`].
    pub url: Option<String>,
    /// Ports this worktree owned had been taken, so it moved. Worth saying
    /// out loud: a URL the developer had bookmarked just changed.
    pub reassigned: bool,
}

impl StartReport {
    /// Every process this call has something to say about.
    pub fn processes(&self) -> impl Iterator<Item = &StartedProcess> {
        self.started.iter().chain(self.already_running.iter())
    }

    /// Nothing was spawned: everything asked for was already up.
    pub fn started_nothing(&self) -> bool {
        self.started.is_empty()
    }
}

/// The names of a list of processes, for a message.
pub fn process_names(list: &[StartedProcess]) -> String {
    list.iter()
        .map(|p| p.process.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The processes whose groups were signalled.
    Stopped(Vec<String>),
    /// Nothing was running. Not an error: `stop` is how you make sure.
    NotRunning,
}

/// Everything a start needs to know about one process before anything is
/// spawned.
///
/// Planned in full first, and spawned only afterwards: a second process
/// with an unrenderable command or a missing `cwd` must not leave the first
/// one running behind a failed start.
struct Planned {
    process: String,
    shell_cmd: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    log_file: PathBuf,
    ready_port: Option<u16>,
    ready_timeout_s: Option<u64>,
}

/// Starts a worktree's processes — every one its config declares, or the
/// one `only` names.
///
/// The shape is `new`'s: take the lock, decide, act, record, save. A
/// process that is already running is reported rather than started twice; a
/// record left over from one that died is signalled and cleared first,
/// because a dead leader does not mean a dead process group.
///
/// Every process is spawned before any of them is waited on. Dependencies
/// between them are the app's problem: a web server that needs its api up
/// first retries, as every dev server does.
pub fn start(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    mode: Mode,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    let worktree = find_worktree(paths, name)?;
    // Before anything is installed, signalled or spawned: a `--only` naming
    // a process that does not exist must have no side effects at all.
    let selection = selected_processes(config, only)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());

    // A project with nothing to isolate is not an error: `--isolated` on a
    // Go service with no compose file runs shared and says so, because the
    // flag is a wish about this project and the project has no services.
    let isolatable = !service_roles(config).is_empty();
    if mode == Mode::Isolated && !isolatable {
        progress("no services are configured for this project — starting in shared mode");
    }
    // A start that will only report what is already up must not install
    // first: `npm ci` inside a worktree whose dev server is live is a
    // surprise nobody asked for. Starting one process beside a live one is
    // not that case — it is about to run code. Read without the lock, like
    // the hook's own fingerprint: the decision it guards is "can this step
    // be skipped", and the authoritative one is made under the lock below.
    let mut everything_up = every_process_running(paths, name, &selection);

    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;

    // A record only vouches for the worktree it was written for; a stale
    // one at another path is replaced rather than inherited. Every group it
    // recorded is signalled first: those processes are running somewhere
    // else entirely, and dropping the record would leave nothing able to
    // find them again.
    let stale = store.worktrees.get(name).is_some_and(|record| {
        crate::paths::resolve_for_compare(&record.path)
            != crate::paths::resolve_for_compare(&canonical)
    });
    if stale {
        let groups: Vec<i32> = store.worktrees[name]
            .processes
            .values()
            .map(|p| p.pgid)
            .collect();
        for pgid in groups {
            proc::stop(pgid, STOP_GRACE)?;
        }
        store.worktrees.remove(name);
    }

    // What this worktree does about services now, against what it did
    // before. Decided here rather than with the ports below, because a
    // start that changes the answer is a start that has to replace the
    // processes: an application still pointed at a database that is going
    // away is not running in the mode it was asked for.
    let was_isolated = store.worktrees.get(name).is_some_and(|r| r.isolated);
    let isolate = isolatable
        && match mode {
            Mode::Shared => false,
            Mode::Isolated => true,
            Mode::Remembered => was_isolated,
        };
    let mode_changed = was_isolated != isolate;
    if mode_changed {
        refuse_only_across_a_mode_change(name, only, isolate)?;
    }
    // And the lifecycle runs again for it: a worktree whose services are
    // being swapped underneath it is not one where "everything is already
    // up" means there is nothing to do.
    everything_up = everything_up && !mode_changed;

    // Decided before anything is touched. A process that is alive is
    // reported and left exactly as it is; one whose leader is gone is
    // signalled before its record goes, because a dead leader is not a dead
    // process group and its child may still hold the port.
    let mut already: Vec<String> = Vec::new();
    let mut clear: Vec<(String, i32)> = Vec::new();
    if let Some(record) = store.worktrees.get(name) {
        for (process, existing) in &record.processes {
            let selected = selection.iter().any(|(n, _)| n == process);
            let live = matches!(
                existing.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            ) && proc::is_alive(existing.pid);
            if selected && live && !mode_changed {
                already.push(process.clone());
            } else if selected || only.is_none() {
                // Selected and not live: this start replaces it. Not
                // selected, with nothing asking for a subset: a name config
                // no longer has — state a newer pando wrote, or a process
                // the developer renamed — whose live child would otherwise
                // be left holding a port nothing could find again.
                //
                // This is also where a `Failed` record goes: it survives
                // `reconcile` on purpose, so the mutation that acts on that
                // process is the one that has to clear it.
                //
                // A `--only` start leaves every other record alone. One
                // whose leader is dead is still signalled, by the sweep
                // below, before `reconcile` drops it.
                clear.push((process.clone(), existing.pgid));
            }
        }
    }
    for process in &already {
        progress(&format!("{process} is already running"));
    }
    if !clear.is_empty() {
        progress(match mode_changed {
            true => "the services this worktree talks to are changing, so its processes restart",
            false => "clearing what is left of the last run",
        });
    }
    for (_, pgid) in &clear {
        proc::stop(*pgid, STOP_GRACE)?;
    }
    if let Some(record) = store.worktrees.get_mut(name) {
        for (process, _) in &clear {
            record.processes.remove(process);
        }
    }
    // And every *other* worktree's dead-leader group, because `reconcile`
    // drops those records too — a half-dead share among them.
    for notice in sweep_orphaned_groups(&mut store)? {
        progress(&notice);
    }
    advance_before_reconcile(&mut store);
    state::reconcile(&mut store, proc::is_alive, proc::group_alive);

    let record = store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(canonical.clone(), false));
    // A whole-worktree start replaces everything that was observed; a
    // `--only` start leaves the sibling's ports alone and lets the next
    // refresh say what is really listening.
    if only.is_none() {
        record.observed_ports.clear();
    }
    // Isolation is per start and remembered per worktree: `--isolated`
    // turns it on, and a later plain `start` of the same worktree keeps
    // the services it already has rather than quietly pointing its
    // processes back at the shared database.
    //
    // Turned *on* only once the containers exist — see `remember_isolated`
    // below. Turned off here and now, because a worktree pando can no
    // longer isolate is one whose next start is a shared one, and there is
    // no container to contradict that.
    if !isolate {
        record.isolated = false;
    }
    // `--shared` is the way back, and taking it means taking the private
    // copies down. The records stay: they carry the compose project, which
    // is the only thing that can find those containers and their volumes
    // again. Their *ports* do not, because the window this start is about
    // to re-derive no longer has room for them, and a status line claiming
    // a service is on a port another worktree now owns is worse than one
    // that says nothing.
    let mut going_shared: Vec<String> = Vec::new();
    if mode == Mode::Shared && was_isolated {
        let failures = stop_service_pumps(record, &|pgid| proc::stop(pgid, STOP_GRACE));
        if !failures.is_empty() {
            bail!("{name}: {}", failures.join("; "));
        }
        for service in record.services.iter_mut() {
            service.port = None;
        }
        clear_native_sockets(paths, name, record);
        going_shared = compose_projects(record);
    }
    // Service roles are reserved with the process roles, in one window, so
    // `{port:postgres}` resolves in any template and the number is the
    // same on every restart.
    let roles = worktree_roles(config, isolate);
    // Ports this worktree's own surviving processes are holding. They will
    // not pass a freeness probe, and they are not somebody else's either.
    let mut keep: Vec<u16> = record
        .processes
        .keys()
        .filter_map(|process| config.processes.get(process))
        .flat_map(|process| process.roles())
        .filter_map(|role| record.ports.get(&role).copied())
        .collect();
    // And the ports its own *containers* are holding, for exactly the same
    // reason. Without these, a second isolated start of a running worktree
    // reads its own database as somebody else's listener, decides the
    // window was taken, and moves every port — web included — leaving the
    // live application pointed at ports nothing is on.
    if isolate {
        keep.extend(record.services.iter().filter_map(|service| service.port));
    }

    let assignment = ports::assign_keeping(paths, &mut store, name, &roles, &keep)?;

    // Which process owns which role, for the whole worktree and whatever
    // `--only` asked for, recorded beside the ports themselves. The URL
    // rule has to be answerable from the record alone — `status` and the
    // TUI never see config — and it has to give the same answer after a
    // stop as `start` gave, so this outlives the processes exactly as the
    // ports do.
    let owners: BTreeMap<String, Vec<String>> = config
        .processes
        .iter()
        .map(|(process, config)| (process.clone(), config.roles()))
        .filter(|(_, roles)| !roles.is_empty())
        .collect();
    if let Some(record) = store.worktrees.get_mut(name) {
        record.roles = owners;
        if isolate {
            record.services = planned_services(config, paths, name, &assignment.ports, record);
        }
    }
    // Written down before anything is brought up: a start that fails
    // halfway must still leave `rm` able to name the compose project and
    // take its volumes with it.
    state::save(&paths.state_file(), &store)?;
    // And unlocked from here to the spawn. An install takes minutes, a
    // database takes seconds to become ready, and a migration takes as
    // long as it takes; holding the state lock through any of them would
    // freeze `pando ls` and the TUI's tick.
    drop(_lock);

    // Outside the lock, like every other compose call: `docker compose
    // stop` takes seconds, and holding the state lock through it would
    // freeze `pando ls` and the TUI's tick.
    if !going_shared.is_empty() {
        progress(&format!(
            "{name} is going back to the project's shared services — stopping its own"
        ));
        stop_compose_projects(paths, &going_shared, |compose| compose.stop())?;
    }

    // Everything the app is told about where its services are. Computed
    // from the allocated ports alone, so a hook that runs before the
    // containers exist sees exactly what the processes will.
    let service_env = if isolate {
        resolve_service_env(paths, config, &canonical, &assignment.ports)?
    } else {
        BTreeMap::new()
    };

    // The lifecycle in order: create (which is where the install step
    // lives), install, the services coming up, then services. Every hook
    // is gated by its own fingerprint, so a start that changes nothing
    // runs none of them.
    //
    // A worktree that is already running every process it was asked for
    // is not starting anything, so nothing is re-run for it either: `npm
    // ci` inside a live worktree is a surprise nobody asked for.
    let hook_ctx = HookContext {
        name,
        branch: worktree.branch.as_deref(),
        worktree: &canonical,
        ports: &assignment.ports,
        service_env: &service_env,
    };
    if !everything_up {
        run_hooks(
            paths,
            config,
            config::HookPoint::Create,
            &hook_ctx,
            progress,
        )?;
        run_hooks(
            paths,
            config,
            config::HookPoint::Install,
            &hook_ctx,
            progress,
        )?;
    }

    let mut fresh = Fresh(false);
    if isolate {
        fresh = bring_up_services(paths, config, name, &canonical, &assignment.ports, progress)?;
        remember_isolated(paths, name)?;
    }

    // The database the hooks after this point run against has changed, and
    // a fingerprint cannot see a database. A mode change swaps it in either
    // direction; a data directory initialised just now is a new and empty
    // one even without a mode change, which is what a developer who
    // deleted it by hand gets. Either way the recorded answer to "have the
    // inputs changed" is wrong, so it is discarded rather than trusted —
    // and the hooks are let through even on a worktree that was otherwise
    // fully up, because an empty schema is not "nothing to do".
    let why = match (mode_changed, fresh.0) {
        (true, _) if isolate => Some("this worktree now runs its own services"),
        (true, _) => Some("this worktree is back on the project's shared services"),
        (_, true) => Some("a service here was given a new, empty data directory"),
        _ => None,
    };
    if let Some(why) = why {
        forget_hooks_after_services(paths, config, name, why, progress)?;
        everything_up = false;
    }

    if !everything_up {
        run_hooks(
            paths,
            config,
            config::HookPoint::Services,
            &hook_ctx,
            progress,
        )?;
        // The last gate before anything is spawned: a probe that
        // recognises the failure stops the start and says how to fix it,
        // rather than letting the dev server die of it thirty seconds
        // later with the reason buried in a log.
        run_probes(paths, config, &hook_ctx, progress)?;
    }

    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // The record this call left behind, unless something removed the
    // worktree while the services were coming up.
    store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(canonical.clone(), false));

    let mut planned: Vec<Planned> = Vec::new();
    for (process_name, process) in &selection {
        if already.contains(process_name) {
            continue;
        }
        let process_roles = process.roles();
        let ready_role = ready_role(process, &process_roles)
            .with_context(|| format!("in process {process_name}"))?;
        let ready_port = ready_role
            .as_deref()
            .and_then(|r| assignment.ports.get(r))
            .copied();
        let log_file = paths.log_file(name, process_name);
        let ctx = template::Context {
            name,
            branch: worktree.branch.as_deref(),
            worktree: &canonical,
            root: paths.root(),
            project: paths.project_id(),
            // Every role of the worktree, not only this process's own:
            // `{port:api}` inside the web process's env is how one process
            // is told where another one is listening.
            ports: &assignment.ports,
            default_role: ready_role.as_deref(),
            log: Some(&log_file),
        };
        let cmd = template::render(&process.cmd, &ctx)
            .with_context(|| format!("in the command for process {process_name}"))?;
        let cwd = process_cwd(&canonical, process_name, process, &ctx)?;
        let env = process_env(paths, name, &worktree, process, &service_env, &ctx)?;
        planned.push(Planned {
            process: process_name.clone(),
            shell_cmd: with_prelude(config, &cmd),
            cwd,
            env,
            log_file,
            ready_port,
            ready_timeout_s: process.ready.as_ref().and_then(|r| r.timeout_s),
        });
    }

    let mut started: Vec<StartedProcess> = Vec::new();
    let mut failure: Option<anyhow::Error> = None;
    for plan in planned {
        progress(&format!("starting {}", plan.process));
        // Truncated, not appended: the classifier reads the tail of this
        // file to explain a failure, and the closing lines of the
        // *previous* run would be a confident wrong answer.
        let spawned = reset_log(&plan.log_file).and_then(|()| {
            proc::spawn_detached(SpawnOptions {
                shell_cmd: &plan.shell_cmd,
                cwd: &plan.cwd,
                log_file: &plan.log_file,
                env: &plan.env,
                status_file: Some(&crate::paths::exit_status_file(&plan.log_file)),
            })
        });
        let spawn = match spawned {
            Ok(spawn) => spawn,
            // Whatever came up before this one is already running and
            // already in the map. The state is saved below either way, so
            // the failure never leaves a process nothing can stop.
            Err(e) => {
                failure = Some(e.context(format!("starting process {}", plan.process)));
                break;
            }
        };
        let now = Utc::now();
        let record = ProcessRecord {
            pid: spawn.pid,
            pgid: spawn.pgid,
            started_at: now,
            log_path: plan.log_file,
            ready_port: plan.ready_port,
            ready_timeout_s: plan.ready_timeout_s,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Starting { since: now },
        };
        store
            .worktrees
            .get_mut(name)
            .expect("the record was just inserted")
            .processes
            .insert(plan.process.clone(), record.clone());
        started.push(StartedProcess {
            process: plan.process,
            record,
        });
    }
    state::save(&paths.state_file(), &store)?;
    if let Some(e) = failure {
        return Err(e);
    }

    let already_running: Vec<StartedProcess> = already
        .iter()
        .filter_map(|process| {
            let record = store.worktrees.get(name)?.processes.get(process)?;
            Some(StartedProcess {
                process: process.clone(),
                record: record.clone(),
            })
        })
        .collect();
    // The last point, and the only one that runs with the processes up.
    // Outside the lock, because a `dev` hook is a command like any other
    // and holding the lock through it would freeze the TUI's tick.
    drop(_lock);
    if !everything_up {
        run_hooks(paths, config, config::HookPoint::Dev, &hook_ctx, progress)?;
    }
    // From the record, through the one function every read path uses, so
    // that `pando start` and a `pando status` a second later cannot
    // disagree about the URL of the same worktree.
    let url = store.worktrees.get(name).and_then(worktree_url);
    Ok(StartReport {
        worktree: name.to_string(),
        started,
        already_running,
        url,
        ports: assignment.ports,
        reassigned: assignment.reassigned,
    })
}

/// Whether every process a start was asked for is already up. Best effort
/// and lock-free: every caller re-decides under the lock.
fn every_process_running(
    paths: &PandoPaths,
    name: &str,
    selection: &[(String, &ProcessConfig)],
) -> bool {
    let Ok(store) = state::load(&paths.state_file()) else {
        return false;
    };
    let Some(record) = store.worktrees.get(name) else {
        return false;
    };
    selection.iter().all(|(process, _)| {
        record.processes.get(process).is_some_and(|p| {
            matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. })
                && proc::is_alive(p.pid)
        })
    })
}

/// What an `--only` that names nothing the worktree is running means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MissingOnly {
    /// An error naming what the worktree *is* running. `stop` cannot see
    /// config — it has to work when `pando.toml` is broken — so the record
    /// is the only thing it can check a name against, and "not running"
    /// would read as "nothing to do" for a typo.
    IsAnError,
    /// Nothing to do. `restart` has already checked the name against
    /// config, and bringing a process that is down back up is exactly what
    /// `restart --only` is for.
    IsNothingToDo,
}

/// Stops a worktree's processes — every one it is running, or the one
/// `only` names. The worktree, its ports, and its logs survive.
pub fn stop(
    paths: &PandoPaths,
    name: &str,
    only: Option<&str>,
    progress: &dyn Fn(&str),
) -> Result<StopOutcome> {
    stop_missing(paths, name, only, MissingOnly::IsAnError, progress)
}

fn stop_missing(
    paths: &PandoPaths,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    progress: &dyn Fn(&str),
) -> Result<StopOutcome> {
    paths.ensure_home()?;
    let mut projects: Vec<String> = Vec::new();
    let outcome = {
        let _lock = state::lock(&paths.lock_file())?;
        let mut store = state::load(&paths.state_file())?;
        let outcome = stop_recorded(&mut store, name, only, missing, &mut projects)?;
        // `reconcile` drops dead-leader records for every worktree in the
        // project, not only this one, so every one is signalled first — and
        // a sibling's half-dead share along with them.
        for notice in sweep_orphaned_groups(&mut store)? {
            progress(&notice);
        }
        advance_before_reconcile(&mut store);
        state::reconcile(&mut store, proc::is_alive, proc::group_alive);
        if let Some(record) = store.worktrees.get(name) {
            clear_native_sockets(paths, name, record);
        }
        state::save(&paths.state_file(), &store)?;
        outcome
    };
    // Outside the lock: `docker compose stop` takes as long as the
    // containers take to shut down, and the records that name the project
    // are already saved, so a failure here is recoverable by running
    // `stop` again.
    stop_compose_projects(paths, &projects, |compose| compose.stop())?;
    Ok(outcome)
}

/// Stops every worktree pando has a process for, returning their names.
pub fn stop_all(paths: &PandoPaths, progress: &dyn Fn(&str)) -> Result<Vec<String>> {
    stop_all_with(paths, |pgid| proc::stop(pgid, STOP_GRACE), progress)
}

/// [`stop_all`] with the signal injected, so a test can drive the path
/// where a group refuses to die without needing one that really does.
pub fn stop_all_with(
    paths: &PandoPaths,
    stop: impl Fn(i32) -> Result<()>,
    progress: &dyn Fn(&str),
) -> Result<Vec<String>> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // Services as well as processes: a worktree whose dev server crashed
    // still has a database up, and `stop` with no name is how you make
    // sure nothing of pando's is left running.
    let names: Vec<String> = store
        .worktrees
        .iter()
        .filter(|(_, r)| !r.processes.is_empty() || !r.services.is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    for name in names {
        // One worktree that will not die must not leave the rest running —
        // and must not lose its record either. The failures are collected
        // and reported once every other group has been signalled.
        match stop_recorded_with(
            &mut store,
            &name,
            None,
            MissingOnly::IsAnError,
            &stop,
            &mut projects,
        ) {
            Ok(StopOutcome::Stopped(_)) => stopped.push(name),
            Ok(StopOutcome::NotRunning) => {}
            Err(e) => failures.push(format!("stopping {name}: {e:#}")),
        }
    }
    // Nothing is dropped while a group is still unaccounted for: the pgid
    // in that record is the only way back to it.
    let mut sweep_failed = None;
    if failures.is_empty() {
        match sweep_orphaned_groups_with(&mut store, &stop) {
            Ok(notices) => {
                for notice in notices {
                    progress(&notice);
                }
                advance_before_reconcile(&mut store);
                state::reconcile(&mut store, proc::is_alive, proc::group_alive);
            }
            Err(e) => sweep_failed = Some(e),
        }
    }
    for name in &stopped {
        if let Some(record) = store.worktrees.get(name) {
            clear_native_sockets(paths, name, record);
        }
    }
    // Saved either way, so the groups that *were* signalled do not come
    // back as phantom records on the next read.
    state::save(&paths.state_file(), &store)?;
    drop(_lock);
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    if let Some(e) = sweep_failed {
        return Err(e);
    }
    stop_compose_projects(paths, &projects, |compose| compose.stop())?;
    Ok(stopped)
}

/// Signals the process groups recorded for `name` and drops their records.
/// The caller holds the lock and saves.
///
/// The signal is unconditional, Failed records included. pando is not the
/// process's parent by then, so "failed" only ever meant "its leader is
/// gone" — the group can still be serving. Clearing them is this path's job
/// too: a `Failed` record survives `reconcile` on purpose, and a stop of
/// the process it belongs to is one of the three things that ends it (the
/// others being a start of it and an `rm` of its worktree).
pub(super) fn stop_recorded(
    store: &mut state::State,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    services_to_stop: &mut Vec<String>,
) -> Result<StopOutcome> {
    stop_recorded_with(
        store,
        name,
        only,
        missing,
        |pgid| proc::stop(pgid, STOP_GRACE),
        services_to_stop,
    )
}

/// What `--only <name>` gets when the worktree is not running that
/// process: an error naming what it *is* running, because "not running"
/// would read as "nothing to do" for a typo — and a typo must never be
/// answered by taking the database down.
fn missing_only(
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    record: &WorktreeRecord,
) -> Result<StopOutcome> {
    if missing == MissingOnly::IsNothingToDo {
        return Ok(StopOutcome::NotRunning);
    }
    let running: Vec<&str> = record.processes.keys().map(String::as_str).collect();
    bail!(
        "{name} is not running a process named {:?} — it is running: {}",
        only.unwrap_or_default(),
        if running.is_empty() {
            "nothing".to_string()
        } else {
            running.join(", ")
        }
    )
}

pub(super) fn stop_recorded_with(
    store: &mut state::State,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    stop: impl Fn(i32) -> Result<()>,
    services_to_stop: &mut Vec<String>,
) -> Result<StopOutcome> {
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(StopOutcome::NotRunning);
    };
    // A worktree whose processes are all down may still have containers
    // up: it was started isolated and then every process crashed. `stop`
    // is how you make sure, so the services are taken down either way —
    // but only when nothing asked for a subset. `--only dev` is about one
    // process, and the database its siblings use is not that process; a
    // name the worktree is not running is the same error it is when
    // something *is* running, not a silent whole-worktree stop.
    if record.processes.is_empty() {
        if only.is_some() {
            return missing_only(name, only, missing, record);
        }
        // Read before anything is signalled: a native service *is* its
        // process, so a worktree whose only service is a database has
        // something to stop even though it has no compose project and no
        // process record at all.
        let had_native = record
            .services
            .iter()
            .any(|s| s.kind == state::ServiceKind::Native && s.pgid.is_some());
        let mut failures = stop_service_pumps(record, &stop);
        // A worktree whose every process crashed can still be shared: the
        // tunnel outlives them, and a public URL onto nothing is the worst
        // of both worlds.
        let was_shared = take_share_down(record, &stop, &mut failures);
        let projects = compose_projects(record);
        if !failures.is_empty() {
            bail!("{name}: {}", failures.join("; "));
        }
        if projects.is_empty() && !was_shared && !had_native {
            return Ok(StopOutcome::NotRunning);
        }
        services_to_stop.extend(projects);
        return Ok(StopOutcome::Stopped(Vec::new()));
    }
    let groups: Vec<(String, i32)> = record
        .processes
        .iter()
        .filter(|(process, _)| only.is_none_or(|wanted| process.as_str() == wanted))
        .map(|(process, p)| (process.clone(), p.pgid))
        .collect();
    if groups.is_empty() {
        return missing_only(name, only, missing, record);
    }
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    for (process, pgid) in groups {
        // Signal first, drop second. A record cleared for a group that was
        // never signalled is a process nothing can find again.
        match stop(pgid) {
            Ok(()) => {
                record.processes.remove(&process);
                stopped.push(process);
            }
            Err(e) => failures.push(format!("{process} (group {pgid}): {e:#}")),
        }
    }
    if record.processes.is_empty() {
        record.observed_ports.clear();
    }
    // A worktree-wide stop takes its services with it; `--only` is about
    // one process and leaves the database its siblings are still using.
    if only.is_none() {
        failures.extend(stop_service_pumps(record, &stop));
        services_to_stop.extend(compose_projects(record));
    }
    // And the public URL, once nothing is left for it to point at. A
    // `--only` stop of one process of several leaves the share up, because
    // its siblings are still serving; a `--only` stop of the last one does
    // not, because a tunnel onto nothing is worse than no tunnel.
    let still_serving = record
        .processes
        .values()
        .any(|p| matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }));
    if only.is_none() || !still_serving {
        take_share_down(record, &stop, &mut failures);
    }
    if !failures.is_empty() {
        bail!("{name}: {}", failures.join("; "));
    }
    Ok(StopOutcome::Stopped(stopped))
}

/// Signals every process group in the project whose leader is dead, so that
/// no record is ever dropped without being signalled first.
///
/// `reconcile` drops dead-leader records for *every* worktree in the state
/// file, while an action only signals the worktree it was asked about. That
/// seam is how a sibling worktree — one whose `bash -lc` exited while a
/// child it backgrounded still holds a port — loses its record and leaves a
/// process nothing can find again. So the sweep is global, and runs before
/// anything that drops records.
///
/// A half-dead share is the same failure with a public URL attached, so it
/// is swept here too: [`sweep_dead_shares`] is the only thing that can
/// signal one, and `reconcile` would otherwise drop the record holding the
/// surviving half's pgid. Its notices come back to the caller, which is the
/// only place that knows where to print them.
///
/// One group that will not die does not stop the sweep: the rest are still
/// signalled and the failures are reported together. A caller that gets an
/// error must not go on to drop records.
pub(super) fn sweep_orphaned_groups(store: &mut state::State) -> Result<Vec<String>> {
    sweep_orphaned_groups_with(store, |pgid| proc::stop(pgid, STOP_GRACE))
}

/// [`sweep_orphaned_groups`] with the signal injected, so a test can watch
/// which groups it decides to signal without needing real ones.
pub(super) fn sweep_orphaned_groups_with(
    store: &mut state::State,
    stop: impl Fn(i32) -> Result<()>,
) -> Result<Vec<String>> {
    // First, because a share is the one record whose survivor is a public
    // door: a tunnel nobody can name again is worse than a dev server
    // nobody can name again.
    let notices = sweep_dead_shares_with(store, proc::is_alive, &stop);
    let mut failures = Vec::new();
    for (name, record) in &mut store.worktrees {
        for (process, p) in &mut record.processes {
            // Once, not on every mutation. A dead leader is not a dead
            // group, so the group is signalled — but a `Failed` record now
            // survives `reconcile` until its own worktree is started,
            // stopped or removed (each of which signals and clears it on
            // its own path), and re-sending SIGTERM/SIGKILL to that pgid
            // on every later mutation in the project is how a pid that has
            // since wrapped around onto an unrelated session leader gets
            // killed. One signal per record bounds that to the window
            // between the leader dying and the first mutation after it.
            if p.swept || proc::is_alive(p.pid) {
                continue;
            }
            match stop(p.pgid) {
                // Recorded only once the signal actually went out: a group
                // that could not be signalled has to be tried again.
                Ok(()) => p.swept = true,
                Err(e) => failures.push(format!("{name}/{process} (group {}): {e:#}", p.pgid)),
            }
        }
        // A log pump is a process group like any other, and `reconcile`
        // forgets its pid the moment its leader dies — so it is signalled
        // here first, or a `docker compose logs -f` whose leader exited
        // keeps a child attached to the daemon with nothing able to name
        // it again.
        for service in record.services.iter_mut() {
            let (Some(pid), Some(pgid)) = (service.pid, service.pgid) else {
                continue;
            };
            if proc::is_alive(pid) {
                continue;
            }
            match stop(pgid) {
                Ok(()) => {
                    service.pid = None;
                    service.pgid = None;
                }
                Err(e) => failures.push(format!(
                    "{name}/{} log pump (group {pgid}): {e:#}",
                    service.name
                )),
            }
        }
    }
    if failures.is_empty() {
        return Ok(notices);
    }
    bail!(
        "could not signal {} process group(s) before dropping their records: {}",
        failures.len(),
        failures.join("; ")
    )
}

/// Stop, then start. The ports come back from the record `stop` left
/// behind, so a restart keeps the URL — and `--only` restarts one process
/// while the rest keep serving on the ports they already have.
pub fn restart(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    mode: Mode,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    // Against config, and before anything is signalled: `stop` can only
    // check a name against what is running, so a typo at `--only` used to
    // get an error about the record — a different message depending on
    // unrelated state, and never the one that lists the names config
    // declares.
    selected_processes(config, only)?;
    // Before the stop below, not after it: a refusal that has already
    // taken the process down is not a refusal.
    if mode_would_change(paths, config, name, mode) {
        refuse_only_across_a_mode_change(name, only, mode == Mode::Isolated)?;
    }
    // And a name config does know, whose process is simply not up, is a
    // no-op to stop rather than a refusal. Bringing a stopped process back
    // is the one thing `restart --only` exists for, and it used to be the
    // one thing it could not do — but only while a sibling was still
    // running, which made the failure look random.
    stop_missing(paths, name, only, MissingOnly::IsNothingToDo, progress)?;
    start(paths, config, name, only, mode, progress)
}

/// Refuses `--only` on a start that would change which services the
/// worktree talks to.
///
/// A mode change replaces every process's environment: the database
/// address they were given is about to point somewhere else. `--only dev`
/// through that change restarts `dev` against the new services and leaves
/// `api` running against the old ones — two halves of one application
/// talking to two different databases, with nothing saying so. The other
/// way out is to carry every process across, but that restarts processes
/// the developer did not name, which is its own surprise; refusing says
/// what is true and costs one word on the command line.
///
/// Checked before anything is stopped or spawned, and again under the
/// lock where the decision is actually made.
fn refuse_only_across_a_mode_change(
    name: &str,
    only: Option<&str>,
    going_isolated: bool,
) -> Result<()> {
    let Some(only) = only else { return Ok(()) };
    bail!(
        "{name} is switching to {} services, and `--only {only}` cannot do that for one \
         process: the others would keep talking to the services that are going away. Run it \
         without `--only`, or leave the mode as it is",
        match going_isolated {
            true => "its own",
            false => "the project's shared",
        }
    )
}

/// Whether a start in this mode would change what the worktree's
/// processes talk to, read without the lock.
///
/// The answer the refusal above needs *before* `restart` stops anything.
/// `start` makes the same decision again under the lock, where it is
/// authoritative; this one only has to be right often enough to refuse
/// before a side effect, and a mode that is flipping under a concurrent
/// command is caught there.
fn mode_would_change(paths: &PandoPaths, config: &Config, name: &str, mode: Mode) -> bool {
    if !matches!(mode, Mode::Isolated | Mode::Shared) {
        return false;
    }
    let was_isolated = state::load(&paths.state_file())
        .ok()
        .and_then(|store| store.worktrees.get(name).map(|r| r.isolated))
        .unwrap_or(false);
    let isolate = !service_roles(config).is_empty() && mode == Mode::Isolated;
    was_isolated != isolate
}

/// The processes a start, stop or restart acts on: every one config
/// declares, or the one `only` names.
///
/// Alphabetically by process name, which is the order they are spawned in:
/// nothing in the loader preserves the file's own order.
fn selected_processes<'a>(
    config: &'a Config,
    only: Option<&str>,
) -> Result<Vec<(String, &'a ProcessConfig)>> {
    if config.processes.is_empty() {
        bail!("no processes configured; add [dev] to pando.toml");
    }
    let chosen: Vec<(String, &ProcessConfig)> = match only {
        Some(wanted) => {
            let process = config.processes.get(wanted).with_context(|| {
                let known: Vec<&str> = config.processes.keys().map(String::as_str).collect();
                format!(
                    "no process named {wanted:?} in pando.toml — it configures: {}",
                    known.join(", ")
                )
            })?;
            vec![(wanted.to_string(), process)]
        }
        None => config
            .processes
            .iter()
            .map(|(name, process)| (name.clone(), process))
            .collect(),
    };
    // `cmd` is optional so that a half-written process table does not take
    // every other command down with it; this is where it has to be there.
    // Refused before anything starts: half a worktree is worse than none.
    for (name, process) in &chosen {
        if process.cmd.trim().is_empty() {
            let table = if name == detect::DEV && config.processes.len() == 1 {
                "[dev]".to_string()
            } else {
                format!("[processes.{name}]")
            };
            bail!("{table} in pando.toml has no cmd — add the command that starts this process");
        }
    }
    Ok(chosen)
}

/// Every role every process of the worktree owns, alphabetically by
/// process name — which is the order `Config.processes` keeps them in, and
/// the order they are spawned in.
///
/// Ports are reserved for all of them at once, whatever `--only` asked
/// for: a role belongs to the worktree, and starting one process must
/// never move another one's port. `config::validate` has already refused
/// two processes claiming one role, so this only deduplicates defensively.
/// Every role the worktree needs a port for: the processes' first, then
/// the services' when this worktree runs private copies of them.
///
/// Services last, deliberately. Ports are handed out in role order from
/// one window, so putting them after the processes means a worktree that
/// switches from shared to isolated keeps the web port it already had and
/// simply grows the window — no bookmarked URL changes for turning
/// isolation on.
fn worktree_roles(config: &Config, isolated: bool) -> Vec<String> {
    let mut roles: Vec<String> = Vec::new();
    for process in config.processes.values() {
        for role in process.roles() {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
    }
    if isolated {
        for role in service_roles(config) {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
    }
    roles
}

/// The role whose port has to bind before the process counts as running.
///
/// `web` when it owns one, because that is the role everything else defaults
/// to; otherwise its first role. A process with no ports has none, and is
/// running as soon as it is alive.
fn ready_role(process: &ProcessConfig, roles: &[String]) -> Result<Option<String>> {
    if let Some(named) = process.ready.as_ref().and_then(|r| r.role.as_deref()) {
        if !roles.iter().any(|r| r == named) {
            bail!(
                "ready.role = {named:?} names a role this process does not own — it owns {}",
                if roles.is_empty() {
                    "none".to_string()
                } else {
                    roles.join(", ")
                }
            );
        }
        return Ok(Some(named.to_string()));
    }
    Ok(roles
        .iter()
        .find(|r| r.as_str() == DEFAULT_READY_ROLE)
        .or_else(|| roles.first())
        .cloned())
}

/// The directory the process runs in: the worktree, or the subdirectory
/// config names. A monorepo app sets `cwd = "apps/web"`.
fn process_cwd(
    worktree: &Path,
    name: &str,
    process: &ProcessConfig,
    ctx: &template::Context<'_>,
) -> Result<PathBuf> {
    let Some(relative) = process.cwd.as_deref() else {
        return Ok(worktree.to_path_buf());
    };
    let rendered = template::render(relative, ctx).context("in cwd")?;
    let dir = worktree.join(&rendered);
    if !dir.is_dir() {
        bail!(
            "cwd {rendered:?} does not exist in this worktree ({}) — check the cwd of process \
             {name:?}",
            dir.display()
        );
    }
    // `config::validate` refuses the literal ways out — an absolute path, a
    // `..` — but a template renders at start time and a symlink resolves
    // later still, so the directory that will really be entered is compared
    // against the worktree that owns it. A process that ran outside its own
    // worktree would be writing into a repository, which Invariant 1
    // forbids.
    let resolved = crate::paths::resolve_for_compare(&dir);
    let owner = crate::paths::resolve_for_compare(worktree);
    if !resolved.starts_with(&owner) {
        bail!(
            "cwd {rendered:?} for process {name:?} resolves to {}, which is outside the worktree \
             ({})",
            resolved.display(),
            owner.display()
        );
    }
    Ok(dir)
}

/// The environment the process is started with: the ports the map form of
/// `ports` is sugar for, then whatever `env` sets, then pando's own
/// variables.
///
/// `PANDO_*` last and unconditional: a hook or a script needs to be able to
/// find out which worktree it is in, and config cannot be allowed to lie
/// about that.
pub(super) fn process_env(
    paths: &PandoPaths,
    name: &str,
    worktree: &Worktree,
    process: &ProcessConfig,
    service_env: &BTreeMap<String, String>,
    ctx: &template::Context<'_>,
) -> Result<Vec<(String, String)>> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (var, tmpl) in process.port_env() {
        env.insert(
            var.clone(),
            template::render(&tmpl, ctx).with_context(|| format!("in ports.{var}"))?,
        );
    }
    // Between the port sugar and the process's own `env`, so a developer
    // who spells a service URL out by hand still wins: pando's rewrite is
    // the default, not the law.
    for (var, value) in service_env {
        env.insert(var.clone(), value.clone());
    }
    for (var, tmpl) in &process.env {
        env.insert(
            var.clone(),
            template::render(tmpl, ctx).with_context(|| format!("in env.{var}"))?,
        );
    }
    env.insert("PANDO_NAME".into(), name.to_string());
    env.insert(
        "PANDO_BRANCH".into(),
        worktree.branch.clone().unwrap_or_else(|| name.to_string()),
    );
    env.insert("PANDO_WORKTREE".into(), ctx.worktree.display().to_string());
    env.insert("PANDO_ROOT".into(), paths.root().display().to_string());
    env.insert("PANDO_PROJECT".into(), paths.project_id().to_string());
    Ok(env.into_iter().collect())
}

/// Empties a log file before a run, creating its directory.
pub(super) fn reset_log(log_file: &Path) -> Result<()> {
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    // The exit status beside it belongs to the run being replaced. A run
    // that dies before its own shell can record one would otherwise be
    // explained by the previous run's status.
    let _ = std::fs::remove_file(crate::paths::exit_status_file(log_file));
    std::fs::write(log_file, b"").with_context(|| format!("truncate {}", log_file.display()))
}
