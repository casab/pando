//! Reading state: phases advanced, observed ports captured, failures
//! explained.

use std::collections::BTreeMap;
use std::path::Path;

use crate::paths::PandoPaths;
use crate::ports;
use crate::process as proc;
use crate::state::{self, Phase};

use super::share::sweep_dead_shares;

/// State as of right now: phases advanced, observed ports captured, and the
/// reason a dead process died written down.
///
/// This is the read path. It advances phases rather than reconciling,
/// because a crashed dev server has to stay visible as Failed until the
/// developer acts on it — `reconcile`, which drops dead records, would erase
/// exactly the thing worth showing. It never fails: a state file pando
/// cannot use becomes one warning line, the same one `rm` refuses with.
#[derive(Debug, Default, Clone)]
pub struct Refreshed {
    pub state: state::State,
    pub warning: Option<String>,
    /// What the refresh itself did, one line each. A share whose tunnel
    /// died is closed here rather than in silence: the URL a developer had
    /// open stops working, and they should be told once rather than
    /// discover it from a browser.
    pub notices: Vec<String>,
}

pub fn refresh(paths: &PandoPaths) -> Refreshed {
    // Nothing has ever been started here, so there is nothing to advance and
    // no reason for a read-only command to create a home.
    if !paths.state_file().exists() {
        return Refreshed::default();
    }
    if let Err(e) = paths.ensure_home() {
        return Refreshed {
            state: state::State::new(),
            warning: Some(format!("{e:#}")),
            notices: Vec::new(),
        };
    }
    let _lock = match state::lock(&paths.lock_file()) {
        Ok(lock) => lock,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
                notices: Vec::new(),
            };
        }
    };
    let mut store = match state::load(&paths.state_file()) {
        Ok(store) => store,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
                notices: Vec::new(),
            };
        }
    };

    // Before anything advances or reconciles: both of those drop a share
    // whose tunnel has died, and neither can signal what is left of it.
    // Signalling is not spawning — a read path must not start a process,
    // but a tunnel nothing can reach again is exactly what it must not
    // leave behind either.
    let notices = sweep_dead_shares(&mut store);

    // One scan of every live group, used for both questions this pass
    // answers: whether a starting process has opened its port yet, and what
    // every group is really listening on.
    let scans = scan_groups(&store);
    let mut changed = !notices.is_empty();
    changed |= advance_with(&mut store, &scans);
    changed |= capture_observed_ports(&mut store, &scans);
    if changed {
        // A read path that cannot write is still a read path: the phases are
        // right in memory either way, so a save that fails is not worth
        // failing the command the user actually ran.
        if let Err(e) = state::save(&paths.state_file(), &store) {
            return Refreshed {
                state: store,
                warning: Some(format!("{e:#}")),
                notices,
            };
        }
    }
    Refreshed {
        state: store,
        warning: None,
        notices,
    }
}

/// State as of right now, with nothing written and nothing signalled.
///
/// [`refresh`] is the read path every other command uses, and it takes the
/// lock, advances phases, sweeps a half-dead share — which *signals* the
/// surviving half — and saves. `doctor` may do none of those: it reports.
/// So this is the same advance, on a copy, with the sweep left out, so the
/// report can say a share is half dead rather than quietly finish it off.
pub fn inspect(paths: &PandoPaths) -> Refreshed {
    if !paths.state_file().exists() {
        return Refreshed::default();
    }
    let mut store = match state::load(&paths.state_file()) {
        Ok(store) => store,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
                notices: Vec::new(),
            };
        }
    };
    let scans = scan_groups(&store);
    advance_with(&mut store, &scans);
    capture_observed_ports(&mut store, &scans);
    Refreshed {
        state: store,
        warning: None,
        notices: Vec::new(),
    }
}

/// Moves every process to the phase it is really in, and writes the reason
/// for each failure that is new. The one implementation of "advance", used
/// by the read path and by every mutation that is about to `reconcile`.
fn advance_with(store: &mut state::State, scans: &BTreeMap<i32, Option<Vec<u16>>>) -> bool {
    let failed_before = failed_processes(store);
    let mut changed =
        state::advance_phases(store, proc::is_alive, proc::group_alive, |pgid, port| {
            port_is_bound(scans, pgid, port)
        });
    changed |= explain_new_failures(store, &failed_before);
    changed
}

/// Advances phases before `reconcile` drops anything.
///
/// `reconcile` keeps a record that is already `Failed`, because a crash has
/// to stay visible until the developer acts on it. That only works if
/// something marked it failed first — and between a process dying and the
/// next read path, nothing has. Without this, a `start`, a `stop`, or a
/// `stop --only` of a *sibling* silently drops the crash it was about to
/// make visible. So every mutation advances with the same inputs the read
/// path uses, and only then reconciles.
///
/// The observed ports the same scan could capture are deliberately not
/// recorded here: `start` clears them for the processes it replaces, and
/// the next refresh says what is really listening.
pub(super) fn advance_before_reconcile(store: &mut state::State) {
    let scans = scan_groups(store);
    advance_with(store, &scans);
}

/// `(worktree, process)` pairs already in `Failed`, so a reason is explained
/// once — when it happens — rather than re-read from the log on every tick.
fn failed_processes(store: &state::State) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, record) in &store.worktrees {
        for (process, p) in &record.processes {
            if matches!(p.phase, Phase::Failed { .. }) {
                out.push((name.clone(), process.clone()));
            }
        }
    }
    out
}

/// The ports every live process group is listening on, scanned once.
///
/// `Starting` groups are scanned too, not only `Running` ones: the scan is
/// how a process *becomes* Running, and scanning only what is already
/// running is a deadlock. `None` against a pgid means the scan itself could
/// not run — no `lsof`, denied, or timed out — which is a different answer
/// from "listening on nothing".
pub(super) fn scan_groups(store: &state::State) -> BTreeMap<i32, Option<Vec<u16>>> {
    let mut scans: BTreeMap<i32, Option<Vec<u16>>> = BTreeMap::new();
    for record in store.worktrees.values() {
        for p in record.processes.values() {
            if !matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }) {
                continue;
            }
            scans
                .entry(p.pgid)
                .or_insert_with(|| crate::observe::observed_ports_checked(p.pgid));
        }
    }
    scans
}

/// Whether the process group has opened `port` yet.
///
/// Never by binding it: a probe that takes the port to find out whether it
/// is taken is one an `EADDRINUSE` away from killing the very server it is
/// waiting for, and it answers about the port rather than about *this*
/// process. The scan of the group's own sockets answers both properly; a
/// connection is the fallback for a machine where the scan cannot run.
pub(super) fn port_is_bound(scans: &BTreeMap<i32, Option<Vec<u16>>>, pgid: i32, port: u16) -> bool {
    match scans.get(&pgid) {
        Some(Some(ports)) => ports.contains(&port),
        _ => ports::something_is_listening(port),
    }
}

/// Records the ports each live group is really listening on, per process.
///
/// Configured ports are what pando asked for; these are what happened. A
/// framework that ignores `PORT`, or one that opens a second socket for hot
/// reload, shows up here and nowhere else.
///
/// Per process, because a worktree-wide list cannot say which group opened
/// which socket: an `--inspect` port the api opened reads exactly like a
/// port the web server opened, and the worktree's URL then follows it.
/// The worktree's own list stays as the union of them, which is the shape
/// `status --json` publishes.
pub(super) fn capture_observed_ports(
    store: &mut state::State,
    scans: &BTreeMap<i32, Option<Vec<u16>>>,
) -> bool {
    let mut changed = false;
    for record in store.worktrees.values_mut() {
        let mut union: Vec<u16> = Vec::new();
        let mut groups = 0usize;
        let mut scanned = false;
        for proc in record.processes.values_mut() {
            if !matches!(proc.phase, Phase::Starting { .. } | Phase::Running { .. }) {
                // Not up, so listening on nothing: its last sighting went
                // stale the moment it stopped.
                if !proc.observed_ports.is_empty() {
                    proc.observed_ports.clear();
                    changed = true;
                }
                continue;
            }
            groups += 1;
            let Some(Some(ports)) = scans.get(&proc.pgid) else {
                // This group could not be scanned: its last good answer
                // stands rather than being cleared by a missing `lsof`.
                union.extend(proc.observed_ports.iter().copied());
                continue;
            };
            scanned = true;
            let mut observed = ports.clone();
            observed.sort_unstable();
            observed.dedup();
            if proc.observed_ports != observed {
                proc.observed_ports = observed.clone();
                changed = true;
            }
            union.extend(observed);
        }
        union.sort_unstable();
        union.dedup();
        // Nothing could be scanned at all, so there is nothing to say.
        if groups > 0 && !scanned {
            continue;
        }
        if record.observed_ports != union {
            record.observed_ports = union;
            changed = true;
        }
    }
    changed
}

/// Appends the classifier's one-line hint to a failure that just happened.
///
/// pando is not the process's parent by the time it dies, so there is no
/// exit status to read. The last lines of its log are the only evidence, and
/// four patterns cover most of what actually goes wrong.
fn explain_new_failures(store: &mut state::State, failed_before: &[(String, String)]) -> bool {
    let mut changed = false;
    for (name, record) in store.worktrees.iter_mut() {
        for (process, p) in record.processes.iter_mut() {
            let Phase::Failed { at, reason } = &p.phase else {
                continue;
            };
            if failed_before
                .iter()
                .any(|(w, pr)| w == name && pr == process)
            {
                continue;
            }
            let explained = explain_failure(reason, &p.log_path, proc::group_alive(p.pgid));
            if &explained == reason {
                continue;
            }
            p.phase = Phase::Failed {
                at: *at,
                reason: explained,
            };
            changed = true;
        }
    }
    changed
}

/// Everything that can be said about one failure, in one line.
///
/// Three sources, in the order a reader wants them. What the phase knew —
/// that the process is gone, or that nothing bound the port it was waiting
/// for. Then the status its shell recorded on the way out, which is the
/// only thing that separates a crash from a command that did its job and
/// returned. Then either what the log says, or — when the log is empty —
/// that it is empty, which is a fact about the command and not an absence
/// of information.
///
/// The empty half is why this exists. A first run proposed a Makefile
/// guard as a dev command; it exited 0 in a millisecond because the tool
/// it guarded was installed, and `status`, `doctor` and the TUI all said
/// "failed — process exited" over a log file with nothing in it.
///
/// `group_alive` is the guard on the one claim here that is a judgement
/// rather than a fact: a command that backgrounds the server and returns
/// looks identical from the leader's exit status, and is not the wrong
/// command. Read after the phases are advanced, so the leader has already
/// been reaped and a zombie cannot hold its own group open.
pub(super) fn explain_failure(reason: &str, log_path: &Path, group_alive: bool) -> String {
    let mut out = reason.to_string();
    let code = proc::recorded_exit_status(&crate::paths::exit_status_file(log_path));
    // Only onto the phase's own "it is gone": a timeout is a process that
    // is still running, and has no status to report.
    if out == state::EXITED
        && let Some(code) = code
    {
        out = format!("{out} with status {code}");
    }
    let lines = crate::log_tail::snapshot(log_path, crate::log_tail::FAILURE_TAIL_LINES)
        .unwrap_or_default();
    let printed_anything = std::fs::metadata(log_path).is_ok_and(|m| m.len() > 0);
    if let Some(note) = crate::observe::exit_note(code, printed_anything, group_alive) {
        out = format!("{out} — {note}");
    }
    if let Some(hint) = crate::observe::classify_failure(&lines) {
        out = format!("{out} — {}", hint.hint);
    }
    out
}
