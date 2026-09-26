//! Share and unshare.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::paths::PandoPaths;
use crate::ports;
use crate::process as proc;
use crate::share_proxy;
use crate::state::{self, Phase, ShareRecord, WorktreeRecord};
use crate::tunnel;
use crate::worktree::Worktree;

use super::hooks::pando_env;
use super::lifecycle::{STOP_GRACE, sweep_orphaned_groups};
use super::refresh::{advance_before_reconcile, refresh};
use super::runtime::with_prelude;
use super::services::{observed_port_for_role, resolved_env, url_role};
use super::worktree::find_worktree;

/// Signals both halves of a worktree's share and drops the record, as part
/// of stopping it. Returns whether there was one.
///
/// Signal first, drop second, exactly as the process records are handled: a
/// share record cleared for a tunnel that was never signalled is a
/// cloudflared nothing can find again.
pub(super) fn take_share_down(
    record: &mut WorktreeRecord,
    stop: &impl Fn(i32) -> Result<()>,
    failures: &mut Vec<String>,
) -> bool {
    let Some(share) = record.share.clone() else {
        return false;
    };
    match tunnel::stop_share_with(&share, stop) {
        Ok(()) => {
            record.share = None;
            true
        }
        Err(e) => {
            failures.push(format!(
                "its share (tunnel group {}): {e:#}",
                share.tunnel_pgid
            ));
            true
        }
    }
}

/// How long `[share].auth_cmd` may take before the share gives up on it.
///
/// A script that waits on something that never comes would otherwise hold
/// a share open forever, and in the TUI that is a pending slot nothing can
/// clear.
pub(super) const AUTH_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// How `auth_cmd` is told which port the proxy will listen on, in case it
/// wants to mint a session scoped to it.
pub const ENV_SHARE_PORT: &str = "PANDO_SHARE_PORT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    pub name: String,
    pub public_url: String,
    /// Whether a proxy is injecting a header in front of the application.
    pub pre_authed: bool,
    /// Whether the worktree was already shared when this was asked. A
    /// second `share` prints the URL rather than opening a second tunnel.
    pub already: bool,
}

/// Publishes a running worktree at a public URL.
///
/// The lock is held for the refusals and for recording the result, and
/// dropped for everything slow in between — the auth command, the proxy,
/// and a tunnel that takes up to thirty seconds to publish. That window is
/// why the second half re-checks: a `stop` in the meantime must not leave a
/// tunnel open onto nothing with no record of it.
pub fn share(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    progress: &dyn Fn(&str),
) -> Result<ShareOutcome> {
    let provider = tunnel::provider_for(config.share.provider.as_deref())?;
    share_with(
        paths,
        config,
        name,
        provider.as_ref(),
        &|paths, name, listen, upstream, cookie| {
            share_proxy::spawn(paths, name, listen, upstream, cookie)
        },
        progress,
    )
}

/// How a proxy is started. Injected so a test can drive the path where a
/// tunnel fails with a proxy already running: the real one re-execs the
/// running binary, which inside a library test is the test harness.
pub type SpawnProxy<'a> =
    &'a dyn Fn(&PandoPaths, &str, u16, u16, &str) -> Result<share_proxy::ProxySpawn>;

/// [`share`] with the provider and the proxy injected, so a test can drive
/// a provider that is missing, and one whose tunnel fails after a proxy is
/// already running.
pub fn share_with(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    provider: &dyn tunnel::Provider,
    spawn_proxy: SpawnProxy<'_>,
    progress: &dyn Fn(&str),
) -> Result<ShareOutcome> {
    paths.ensure_home()?;
    let worktree = find_worktree(paths, name)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());

    // Outside the lock, because `refresh` takes it: a worktree `start`
    // returned from a moment ago is still `Starting`, and refusing it is
    // refusing the first thing anyone types.
    let shown = worktree.display_name();
    await_share_target(paths, name, &shown, progress);

    // Every refusal first, and the proxy's port, under the lock.
    let (target_port, share_port) = {
        let _lock = state::lock(&paths.lock_file())?;
        let mut store = state::load(&paths.state_file())?;
        for notice in sweep_orphaned_groups(&mut store)? {
            progress(&notice);
        }
        advance_before_reconcile(&mut store);
        state::reconcile(&mut store, proc::is_alive, proc::group_alive);
        // Before any of the refusals below, which leave without the save at
        // the end: the sweep marked every group it signalled, and a
        // refusal that forgot that signalled them all again on the next
        // call — an agent polling a share it already has, every time.
        state::save(&paths.state_file(), &store)?;

        let record = store
            .worktrees
            .get(name)
            .with_context(|| format!("pando has no record of {shown} — start it first"))?;
        if let Some(existing) = &record.share {
            return Ok(ShareOutcome {
                name: name.to_string(),
                public_url: existing.public_url.clone(),
                pre_authed: existing.proxy_pid.is_some(),
                already: true,
            });
        }
        let target_port = share_target_port(&shown, record)?;
        // Only when something will actually listen on it. A worktree that
        // is shared without an auth command needs no proxy and no port.
        let share_port = match config.share.auth_cmd {
            Some(_) => Some(ports::assign_share_port(paths, &mut store, name)?),
            None => None,
        };
        state::save(&paths.state_file(), &store)?;
        (target_port, share_port)
    };

    // Only now: a worktree that was never going to be shared must not be
    // told to install anything.
    provider.ensure_present(paths)?;

    // The cookie before anything is spawned, so a script that fails leaves
    // nothing behind to clean up.
    let cookie = match config.share.auth_cmd.as_deref() {
        Some(cmd) => {
            progress("running the auth command");
            Some(run_auth_cmd(
                paths,
                config,
                name,
                &worktree,
                &canonical,
                cmd,
                share_port.unwrap_or(target_port),
            )?)
        }
        None => None,
    };

    let proxy = match (&cookie, share_port) {
        (Some(cookie), Some(port)) => {
            progress("starting the share proxy");
            let proxy = spawn_proxy(paths, name, port, target_port, cookie)?;
            // Before a tunnel is published onto it, and the last moment
            // anything knows its process group if it never came up.
            if let Err(e) = share_proxy::await_listening(&proxy) {
                let _ = proc::stop(proxy.pgid, STOP_GRACE);
                return Err(e);
            }
            Some(proxy)
        }
        _ => None,
    };
    let upstream = proxy.as_ref().map_or(target_port, |p| p.listen_port);

    progress(&format!("opening a {} tunnel", provider.name()));
    let spawn = match provider.start(paths, name, upstream) {
        Ok(spawn) => spawn,
        Err(e) => {
            // Nothing has recorded the proxy yet, so this is the last
            // moment anything knows its process group.
            if let Some(proxy) = &proxy {
                let _ = proc::stop(proxy.pgid, STOP_GRACE);
            }
            return Err(e);
        }
    };

    let record = ShareRecord {
        tunnel_pid: spawn.pid,
        tunnel_pgid: spawn.pgid,
        public_url: spawn.public_url.clone(),
        local_port: target_port,
        started_at: Utc::now(),
        log_path: spawn.log_path,
        proxy_pid: proxy.as_ref().map(|p| p.pid),
        proxy_pgid: proxy.as_ref().map(|p| p.pgid),
        proxy_port: proxy.as_ref().map(|p| p.listen_port),
    };

    // The worktree may have been stopped, removed, or shared by somebody
    // else while the tunnel was coming up. Anything that was spawned goes
    // down here rather than becoming a process with no record.
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(existing_record) = store.worktrees.get(name) else {
        let _ = tunnel::stop_share(&record);
        bail!("{shown} was removed while its tunnel was starting; the tunnel was closed again");
    };
    if let Some(won) = &existing_record.share {
        let public_url = won.public_url.clone();
        let pre_authed = won.proxy_pid.is_some();
        let _ = tunnel::stop_share(&record);
        return Ok(ShareOutcome {
            name: name.to_string(),
            public_url,
            pre_authed,
            already: true,
        });
    }
    // Ours, once more, before it is recorded. Another share of this
    // worktree that started a moment earlier holds the proxy's port, so
    // this proxy can pass for listening and then lose the bind; recorded,
    // it is a share whose proxy is dead, and the other share, giving way
    // to it, stops the only proxy that worked.
    if let Some(proxy) = &proxy
        && !proc::is_alive(proxy.pid)
    {
        let said = share_proxy::last_words(proxy);
        let _ = tunnel::stop_share(&record);
        bail!(
            "the share proxy of {shown} exited while its tunnel was starting ({said}); the \
             tunnel was closed again"
        );
    }
    if let Err(e) = share_target_port(&shown, existing_record) {
        let _ = tunnel::stop_share(&record);
        return Err(e.context(format!(
            "{shown} stopped while its tunnel was starting; the tunnel was closed again"
        )));
    }
    store.worktrees.get_mut(name).expect("just read").share = Some(record.clone());
    if let Err(e) = state::save(&paths.state_file(), &store) {
        // A tunnel nothing has a record of is a tunnel nothing can close.
        let _ = tunnel::stop_share(&record);
        return Err(e);
    }
    Ok(ShareOutcome {
        name: name.to_string(),
        public_url: record.public_url,
        pre_authed: proxy.is_some(),
        already: false,
    })
}

/// Takes a worktree's public URL down.
pub fn unshare(paths: &PandoPaths, name: &str) -> Result<()> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let record = store
        .worktrees
        .get_mut(name)
        .with_context(|| format!("pando has no record of {name}"))?;
    let Some(share) = record.share.take() else {
        bail!("{name} is not shared");
    };
    if let Err(e) = tunnel::stop_share(&share) {
        // Still alive, so the record goes back: state has to match what is
        // really running, or a retry has no pgid to signal.
        record.share = Some(share);
        state::save(&paths.state_file(), &store)?;
        return Err(e.context(format!("could not take {name}'s share down")));
    }
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

/// Clears a share whose tunnel or proxy has died, signalling whatever is
/// left of it *before* the record that names it is dropped.
///
/// The only place that signals. `state::reconcile` and
/// `state::advance_phases` also drop a dead share, and neither can signal
/// anything — `state` knows nothing about processes — so this runs first on
/// every path that reaches them, and they find the record already gone.
///
/// A share is only useful while both halves live: a tunnel whose proxy died
/// serves the wrong thing, and a proxy whose tunnel died is unreachable.
/// Either way both groups are signalled, because a dead leader is not a
/// dead group.
pub(super) fn sweep_dead_shares(store: &mut state::State) -> Vec<String> {
    sweep_dead_shares_with(store, proc::is_alive, |pgid| proc::stop(pgid, STOP_GRACE))
}

/// [`sweep_dead_shares`] with liveness and the signal injected, so a test
/// can drive it without real process groups.
pub(super) fn sweep_dead_shares_with(
    store: &mut state::State,
    is_alive: impl Fn(u32) -> bool,
    stop: impl Fn(i32) -> Result<()>,
) -> Vec<String> {
    let mut notices = Vec::new();
    for (name, record) in store.worktrees.iter_mut() {
        let Some(share) = record.share.clone() else {
            continue;
        };
        let tunnel_dead = !is_alive(share.tunnel_pid);
        let proxy_dead = share.proxy_pid.is_some_and(|pid| !is_alive(pid));
        // `stop` and `rm` unshare first, so a *stopped* worktree never
        // keeps a public URL. A crashed one is the same thing without the
        // announcement: the URL answers, and a proxy in front of it keeps
        // injecting the auth cookie into requests aimed at a port whose
        // owner is gone.
        let nothing_serving = !share_target_is_up(record, &is_alive);
        if !tunnel_dead && !proxy_dead && !nothing_serving {
            continue;
        }
        let reason = if tunnel_dead {
            "the share's tunnel exited"
        } else if proxy_dead {
            "the share's proxy exited"
        } else {
            "nothing is serving what its public URL pointed at"
        };
        match tunnel::stop_share_with(&share, &stop) {
            Ok(()) => {
                record.share = None;
                notices.push(format!("{name}: {reason}, so the public URL is closed"));
            }
            // Not cleared: the record holds the only pgid anything can use
            // to try again.
            Err(e) => notices.push(format!(
                "{name}: {reason} and the rest of it would not stop ({e:#}) — \
                 `pando unshare {name}` to try again"
            )),
        }
    }
    notices
}

/// How often the readiness wait asks whether the target is up yet. The
/// same poll a developer does by hand, and `refresh` is cheap.
const SHARE_READY_POLL: Duration = Duration::from_millis(250);

/// Waits for the process a share would publish to leave `Starting`,
/// bounded by that process's own readiness budget.
///
/// `start` records `Starting` and returns; the phase advances on the next
/// read path, once the port is really bound. So `pando start x && pando
/// share x` — the first thing anyone types — used to answer "x is not
/// running, start it first", which is advice the developer had just
/// followed. This is what they would do instead: look at `status` until
/// it says running.
///
/// Not a refusal of its own: the wait ends when the phase changes or the
/// budget runs out, and whatever the state is then goes through the same
/// refusals as before. `refresh` is a read path — it signals, it never
/// spawns — so polling it is safe from here.
fn await_share_target(paths: &PandoPaths, name: &str, shown: &str, progress: &dyn Fn(&str)) {
    let Some(budget) = share_ready_budget(&refresh(paths).state, name) else {
        return;
    };
    progress(&format!(
        "waiting up to {}s for {shown} to be ready",
        budget.as_secs().max(1)
    ));
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        std::thread::sleep(SHARE_READY_POLL);
        if share_ready_budget(&refresh(paths).state, name).is_none() {
            return;
        }
    }
}

/// How long the process a share would publish still has to become ready,
/// or `None` when there is nothing to wait for.
///
/// The budget `advance_phases` itself uses, so the wait ends when that
/// function gives up rather than a moment before or a minute after.
pub(super) fn share_ready_budget(store: &state::State, name: &str) -> Option<Duration> {
    let record = store.worktrees.get(name)?;
    let starting = |p: &state::ProcessRecord| match p.phase {
        Phase::Starting { since } => Some((p.ready_timeout_s, since)),
        _ => None,
    };
    let (timeout_s, since) = match share_owner(record) {
        ShareOwner::Recorded(process) => starting(process)?,
        // Nothing is coming up to serve it, whatever its siblings are doing.
        ShareOwner::Absent(_) => return None,
        ShareOwner::Unknown => {
            if record
                .processes
                .values()
                .any(|p| matches!(p.phase, Phase::Running { .. }))
            {
                return None;
            }
            record.processes.values().find_map(starting)?
        }
    };
    // The window *and* the grace an unanswerable port scan earns, because
    // that is how long the phase can really stay `Starting`. The wait still
    // ends as soon as the phase changes, so a conclusive scan costs
    // nothing extra; the window alone gave up on a healthy server whose
    // scan was merely slow.
    let budget = state::longest_starting_secs(
        timeout_s
            .map(|s| s as i64)
            .unwrap_or(state::START_TIMEOUT_SECS),
    );
    let left = budget - Utc::now().signed_duration_since(since).num_seconds();
    Some(Duration::from_secs(left.max(0) as u64) + SHARE_READY_POLL)
}

/// Whether anything is still up to serve what a share of this worktree
/// points at.
///
/// Liveness as well as phase, because this runs *before* `advance_phases`
/// on every path: a process that died a second ago still says `Running`,
/// and waiting a tick to notice is a tick of a public door onto nothing.
/// `Starting` counts — a worktree that is coming up is not one with
/// nothing serving, and tearing its share down would be the same mistake
/// inverted.
///
/// A sibling does not count. The URL is one process's port, so when that
/// process was stopped with `--only`, or never started by a `start --only`
/// of something else, nothing is serving it however much else is up.
pub(super) fn share_target_is_up(record: &WorktreeRecord, is_alive: &impl Fn(u32) -> bool) -> bool {
    let up = |p: &state::ProcessRecord| {
        matches!(p.phase, Phase::Running { .. } | Phase::Starting { .. }) && is_alive(p.pid)
    };
    match share_owner(record) {
        ShareOwner::Recorded(process) => up(process),
        ShareOwner::Absent(_) => false,
        // A record written before pando tracked who owns what: anything up
        // is as much as it can say.
        ShareOwner::Unknown => record.processes.values().any(up),
    }
}

/// Who serves the port a share of this worktree publishes.
enum ShareOwner<'a> {
    /// The process that owns the URL's role, as its record stands.
    Recorded(&'a state::ProcessRecord),
    /// The process that owns the URL's role, named, with no record: it was
    /// stopped on its own, or a `start --only` never started it.
    Absent(&'a str),
    /// A record written before pando tracked who owns what.
    Unknown,
}

/// The owner of the URL's role, told apart from a record that names none:
/// the two need opposite answers, and one `None` for both read a stopped
/// owner as "anything running will do".
fn share_owner(record: &WorktreeRecord) -> ShareOwner<'_> {
    let Some(role) = url_role(record) else {
        return ShareOwner::Unknown;
    };
    let Some(owner) = record
        .roles
        .iter()
        .find(|(_, roles)| roles.contains(&role))
        .map(|(process, _)| process)
    else {
        return ShareOwner::Unknown;
    };
    match record.processes.get(owner) {
        Some(process) => ShareOwner::Recorded(process),
        None => ShareOwner::Absent(owner),
    }
}

/// The port a share points at: the one the worktree's own URL uses, and
/// only while the process that owns it is running.
///
/// The same rule and the same words as the TUI's open key, because
/// "shareable" and "openable" have to mean the same thing.
///
/// A refusal says which of the three states it found. They need different
/// answers — wait, read the log, start it — and one message for all three
/// sent a developer who had just run `start` back to run it again.
pub(super) fn share_target_port(name: &str, record: &WorktreeRecord) -> Result<u16> {
    let Some(role) = url_role(record) else {
        bail!("{name} has no port yet — start it first");
    };
    let Some(assigned) = record.ports.get(&role).copied() else {
        bail!("{name} has no port yet — start it first");
    };
    match share_state(record) {
        Some(Phase::Running { .. }) => {}
        Some(Phase::Starting { .. }) => bail!(
            "{name} is still starting — `pando status {name}` says when it is ready, and \
             `pando share {name}` then works"
        ),
        Some(Phase::Failed { reason, .. }) => bail!(
            "{name} failed to start ({reason}) — `pando logs {name}` says why; there is nothing \
             for a public URL to point at yet"
        ),
        None => match share_owner(record) {
            ShareOwner::Absent(owner) if !record.processes.is_empty() => bail!(
                "{name} is not running {owner}, the process its URL points at — start it, then \
                 share it"
            ),
            _ => bail!("{name} is not running — start it first, then share it"),
        },
    }
    // What is really serving, not what pando asked for.
    Ok(observed_port_for_role(record, &role).unwrap_or(assigned))
}

/// The phase of the process a share would publish: the owner of the URL's
/// role, or — for a record written before pando tracked who owns what —
/// the best of what the worktree is running. `None` when the owner is not
/// running, whatever its siblings are.
fn share_state(record: &WorktreeRecord) -> Option<Phase> {
    match share_owner(record) {
        ShareOwner::Recorded(process) => return Some(process.phase.clone()),
        ShareOwner::Absent(_) => return None,
        ShareOwner::Unknown => {}
    }
    let best = |wanted: fn(&Phase) -> bool| {
        record
            .processes
            .values()
            .find(|p| wanted(&p.phase))
            .map(|p| p.phase.clone())
    };
    best(|p| matches!(p, Phase::Running { .. }))
        .or_else(|| best(|p| matches!(p, Phase::Starting { .. })))
        .or_else(|| best(|_| true))
}

/// Runs `[share].auth_cmd` and returns the `Cookie` header value it printed.
///
/// It runs in the worktree, with the same environment the processes get
/// plus the proxy's port, so a script can mint a session against the very
/// database the application is using.
fn run_auth_cmd(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Worktree,
    canonical: &Path,
    cmd: &str,
    share_port: u16,
) -> Result<String> {
    let mut env = pando_env(paths, name, worktree.branch.as_deref(), canonical);
    // Best effort: a worktree with no ports never reaches here, and a
    // template that will not render is not a reason to refuse a share the
    // script may not even need it for.
    if let Ok(resolved) = resolved_env(paths, config, name) {
        env.extend(resolved);
    }
    env.push((ENV_SHARE_PORT.to_string(), share_port.to_string()));

    let captured = proc::run_captured(
        &with_prelude(config, cmd),
        canonical,
        &env,
        AUTH_CMD_TIMEOUT,
    )
    .with_context(|| format!("the [share].auth_cmd of {name}"))?;
    if !captured.success() {
        let reason = match captured.last_stderr_line() {
            Some(line) => format!(" — {line}"),
            None => String::new(),
        };
        bail!(
            "[share].auth_cmd exited {}{reason} — it runs in {}, and its stdout is the Cookie \
             header pando injects",
            captured.code.unwrap_or(-1),
            canonical.display()
        );
    }
    let cookie = captured.stdout.trim().to_string();
    if cookie.is_empty() {
        bail!(
            "[share].auth_cmd printed nothing — its stdout is the Cookie header value pando \
             injects, so an empty one has nothing to inject"
        );
    }
    // A header is one line. A value carrying a newline would let a script
    // append headers of its own to every proxied request.
    if cookie.contains(|c: char| c.is_control()) {
        bail!(
            "[share].auth_cmd printed a value with a control character in it — a Cookie header \
             is a single line"
        );
    }
    Ok(cookie)
}
