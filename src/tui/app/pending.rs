//! Actions in flight on a worker thread, and what they report when done.

use anyhow::Result;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use crate::actions;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state::{self, Aggregate, Phase, WorktreeRecord};
use crate::worktree::PrInfo;

use super::App;
use super::background::{AppEvent, ask_through_ui, config_now};
use super::dialogs::Modal;

const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// What a start says when the config names no process. Matched loosely:
/// the wording after it may change, and a miss only means the TUI learns
/// it on the next start instead.
const NO_PROCESSES: &str = "no processes configured";

/// Whether a line an action said on its way is worth the header as well
/// as `m`: one that calls itself a warning, or says something could not
/// be done. Matched loosely too: a miss still leaves the line in `m`.
fn is_warning(line: &str) -> bool {
    line.starts_with("warning:") || line.contains("could not")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Create,
    Remove,
    Start,
    Stop,
    Restart,
    Share,
    Unshare,
    StopAll,
}

impl PendingKind {
    /// What the row and the status line say while it runs.
    pub fn verb(self) -> &'static str {
        match self {
            PendingKind::Create => "creating",
            PendingKind::Remove => "removing",
            PendingKind::Start => "starting",
            PendingKind::Stop => "stopping",
            PendingKind::Restart => "restarting",
            PendingKind::Share => "sharing",
            PendingKind::Unshare => "unsharing",
            PendingKind::StopAll => "stopping",
        }
    }

    /// What it was trying to do, for the line that says it did not.
    fn noun(self) -> &'static str {
        match self {
            PendingKind::Create => "create",
            PendingKind::Remove => "remove",
            PendingKind::Start => "start",
            PendingKind::Stop => "stop",
            PendingKind::Restart => "restart",
            PendingKind::Share => "share",
            PendingKind::Unshare => "unshare",
            PendingKind::StopAll => "stop",
        }
    }
}

pub enum PendingOutcome {
    Created(String),
    Removed(String),
    /// The worktree, its URL, and each process the call spawned with its
    /// pid.
    Started(String, Option<String>, Vec<(String, u32)>),
    Stopped(String),
    /// A stop that found nothing of the worktree up.
    NotRunning(String),
    /// The worktree, its public URL, and whether a proxy is injecting a
    /// header in front of it.
    Shared(String, String, bool),
    Unshared(String),
    /// Every worktree `X` stopped.
    StoppedAll(Vec<String>),
}

impl PendingOutcome {
    /// What a start or a restart of `name` reports.
    pub(super) fn started(name: String, report: &actions::StartReport) -> Self {
        let spawned = report
            .started
            .iter()
            .map(|p| (p.process.clone(), p.record.pid))
            .collect();
        PendingOutcome::Started(name, report.url.clone(), spawned)
    }
}

/// A start that returned before what it spawned was ready.
#[derive(Debug, Clone)]
pub struct AwaitingReady {
    pub name: String,
    /// The URL the start reported, for the line that says it is ready.
    pub url: Option<String>,
    /// Each process the start spawned, and its pid.
    pub spawned: Vec<(String, u32)>,
}

/// Where a start's processes are, as one read of the state has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Readiness {
    /// Still starting, or not in the read at all.
    Waiting,
    Ready,
    Failed,
    /// Nothing of the worktree is recorded any more.
    Gone,
}

impl AwaitingReady {
    /// What `record` says about the processes the start spawned.
    ///
    /// By pid, and only those: a read taken before the start still shows
    /// the run it replaced, every process running, and one taken between
    /// a restart's stop and its start shows only the siblings it left
    /// alone. Neither says anything about the new processes, so both are
    /// waited through. A start that spawned nothing, all of it already
    /// up, has the whole record's phase.
    pub(super) fn readiness(&self, record: Option<&WorktreeRecord>) -> Readiness {
        if self.spawned.is_empty() {
            return match record.and_then(state::aggregate_phase) {
                Some(Aggregate::Running { .. }) => Readiness::Ready,
                Some(Aggregate::Starting { .. }) => Readiness::Waiting,
                Some(Aggregate::Failed { .. }) => Readiness::Failed,
                None => Readiness::Gone,
            };
        }
        let ours: Vec<&Phase> = self
            .spawned
            .iter()
            .filter_map(|(process, pid)| {
                let p = record?.processes.get(process)?;
                (p.pid == *pid).then_some(&p.phase)
            })
            .collect();
        if ours
            .iter()
            .any(|phase| matches!(phase, Phase::Failed { .. }))
        {
            Readiness::Failed
        } else if ours.len() == self.spawned.len()
            && ours
                .iter()
                .all(|phase| matches!(phase, Phase::Running { .. }))
        {
            Readiness::Ready
        } else {
            Readiness::Waiting
        }
    }
}

pub struct PendingAction {
    pub name: String,
    pub kind: PendingKind,
    pub rx: Receiver<Result<PendingOutcome, String>>,
    pub started_at: Instant,
    pub spinner_frame: u8,
    pub progress_rx: Option<Receiver<String>>,
    pub stage: Option<String>,
    /// Every line the worker has said, in order. The spinner shows only
    /// the newest, and a line that arrives with the outcome is never on
    /// screen at all, so `m` gets them once the action is done.
    pub said: Vec<String>,
    /// What the messages call it: the branch, which is what a developer
    /// knows it by, rather than the directory name derived from it.
    pub label: String,
}

impl PendingAction {
    /// Takes whatever the worker has said since the last poll.
    fn take_progress(&mut self) {
        let Some(prx) = self.progress_rx.as_ref() else {
            return;
        };
        while let Ok(line) = prx.try_recv() {
            self.stage = Some(line.clone());
            self.said.push(line);
        }
    }

    /// Everything it said, once the outcome is in: a line sent just before
    /// the outcome may have arrived after this poll's first read.
    fn all_it_said(&mut self) -> Vec<String> {
        self.take_progress();
        std::mem::take(&mut self.said)
    }
}

impl App {
    /// Returns whether the action started; the create modal only closes (and
    /// throws away what was typed) once it did.
    pub(super) fn spawn_create(&mut self, branch: String, base: Option<String>) -> bool {
        let (label, dir_name) = (branch.clone(), actions::sanitize_branch_to_dir(&branch));
        self.spawn_create_with(label, dir_name, move |paths, config, progress| {
            actions::new(paths, config, &branch, base.as_deref(), progress)
        })
    }

    /// A worktree for a pull request, `p` then ⏎: its branch, or for a
    /// fork's, one fetched from `origin`'s copy of its head.
    pub(super) fn spawn_create_pr(&mut self, pr: PrInfo) -> bool {
        let branch = pr.local_branch();
        let label = format!("#{} {branch}", pr.number);
        let dir_name = actions::sanitize_branch_to_dir(&branch);
        self.spawn_create_with(label, dir_name, move |paths, config, progress| {
            actions::new_for_pr(paths, config, &pr, progress)
        })
    }

    /// Settles what `new` needs to know, asking through the modal, then
    /// runs `create` with the config that settled.
    fn spawn_create_with<F>(&mut self, label: String, dir_name: String, create: F) -> bool
    where
        F: FnOnce(&PandoPaths, &Config, &dyn Fn(&str)) -> Result<String> + Send + 'static,
    {
        let paths = self.paths.clone();
        let tx = self.event_tx.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(dir_name, PendingKind::Create, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            // What `pando new` settles before it creates anything — the
            // install command, the files to provision, the version files —
            // settled here the same way, asking through the modal where
            // detection cannot decide. Without it a worktree made with `n`
            // has no dependencies installed and its first start fails.
            let ask = |question: &actions::Question| ask_through_ui(&tx, question);
            let config = config_now(&paths)?;
            let config = actions::resolve_for_new(&paths, &config, &ask, &progress)
                .map_err(|e| format!("{e:#}"))?;
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            create(&paths, &config, &progress)
                .map(PendingOutcome::Created)
                .map_err(|e| {
                    // With where to fix it, as `pando new` says it.
                    let text = format!("{e:#}");
                    actions::install_remedy(&paths, &text).unwrap_or(text)
                })
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
            p.label = label;
        }
        started
    }

    pub(super) fn spawn_remove(&mut self, name: String, yes: bool, force: bool) -> bool {
        let paths = self.paths.clone();
        let worker_name = name.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(name, PendingKind::Remove, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            actions::rm(&paths, &worker_name, yes, force, &progress)
                .map(|()| PendingOutcome::Removed(worker_name))
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
        started
    }

    /// Runs `work` on a worker thread and parks the receiver. Single slot:
    /// a second action while one is in flight is refused rather than queued.
    pub(super) fn spawn_pending<F>(&mut self, name: String, kind: PendingKind, work: F) -> bool
    where
        F: FnOnce() -> Result<PendingOutcome, String> + Send + 'static,
    {
        // By what the spinner calls it: a stop-all has no worktree's name,
        // and a create's name is the directory, not the branch typed.
        if let Some(p) = &self.pending {
            let busy = format!("{} {}", p.kind.verb(), p.label);
            self.set_error(format!("already busy {busy}"));
            return false;
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        let label = self.label_of(&name);
        self.set_progress(format!("{} {} {label}…", SPINNER_FRAMES[0], kind.verb()));
        self.pending = Some(PendingAction {
            label,
            name,
            kind,
            rx,
            started_at: Instant::now(),
            spinner_frame: 0,
            progress_rx: None,
            stage: None,
            said: Vec::new(),
        });
        true
    }

    /// The action in flight on `name`, if there is one — so its row and
    /// its detail pane say `creating…` or `waiting for your answer` rather
    /// than `stopped` while it runs.
    ///
    /// A stop-all is on every worktree that has anything up.
    pub fn pending_on(&self, name: &str) -> Option<&PendingAction> {
        self.pending
            .as_ref()
            .filter(|p| p.name == name || (p.kind == PendingKind::StopAll && self.is_live(name)))
    }

    /// Whether the action in flight is blocked on the question modal.
    pub fn awaiting_answer(&self) -> bool {
        self.pending.is_some()
            && (matches!(self.modal, Some(Modal::Question { .. }))
                || self.queued_question.is_some())
    }

    /// What an action said on its way, as one entry in `m`, before the
    /// outcome: the CLI prints every line, and the spinner here showed
    /// each only until the next.
    fn keep_what_it_said(&mut self, doing: &str, said: &[String]) {
        if !said.is_empty() {
            self.set_status(format!("{doing}: {}", said.join(" · ")));
        }
    }

    pub fn poll_pending(&mut self) {
        let asking = self.awaiting_answer();
        let may_spin = self.spinner_may_take_header();
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        pending.take_progress();
        match pending.rx.try_recv() {
            Ok(Ok(outcome)) => {
                let label = pending.label.clone();
                let said = pending.all_it_said();
                let doing = format!("{} {label}", pending.kind.verb());
                self.pending = None;
                self.keep_what_it_said(&doing, &said);
                match outcome {
                    PendingOutcome::Created(name) => {
                        self.set_success(format!("created {label} — s starts it"));
                        // The cursor goes to it as soon as discovery lists
                        // it: the next key is almost always meant for it.
                        self.select_on_arrival = Some(name);
                        self.spawn_discovery();
                    }
                    PendingOutcome::Removed(_) => {
                        self.set_success(format!("removed {label}"));
                        self.spawn_discovery();
                    }
                    PendingOutcome::Started(name, url, spawned) => {
                        // A start returns once the processes are spawned,
                        // which is not the same as ready: a `✓ started`
                        // beside a row that still says `starting` is a
                        // claim the row contradicts. Ready is announced by
                        // the refresh that sees what it spawned running.
                        let at = url
                            .as_ref()
                            .map(|url| format!(" — {url}"))
                            .unwrap_or_default();
                        let awaited = AwaitingReady {
                            name: name.clone(),
                            url,
                            spawned,
                        };
                        if awaited.readiness(self.record_for(&name)) == Readiness::Ready {
                            self.set_success(format!("{label} is ready{at}"));
                        } else {
                            self.set_status(format!(
                                "started {label}, waiting for it to be ready{at}"
                            ));
                            self.awaiting_ready = Some(awaited);
                        }
                        // The log is new, so whatever was tailed for this
                        // worktree is about the run that just ended.
                        self.log_tails.forget_worktree(&name);
                        self.tail_scroll = 0;
                        self.spawn_refresh();
                    }
                    PendingOutcome::Stopped(_) => {
                        self.set_success(format!("stopped {label}"));
                        self.spawn_refresh();
                    }
                    // Not a tick: nothing was stopped, and `pando stop`
                    // says the same.
                    PendingOutcome::NotRunning(_) => {
                        self.set_status(format!("{label} was not running"));
                        self.spawn_refresh();
                    }
                    PendingOutcome::Shared(_, url, pre_authed) => {
                        let how = if pre_authed { " (pre-authed)" } else { "" };
                        // Long enough to read out; `m` keeps it after that.
                        self.set_lasting(format!(
                            "{label} is public at {url}{how} — O opens it, C copies it"
                        ));
                        self.spawn_refresh();
                    }
                    PendingOutcome::StoppedAll(names) => {
                        match names.len() {
                            0 => self.set_success("nothing was running"),
                            1 => self.set_success(format!("stopped {}", self.label_of(&names[0]))),
                            n => self.set_success(format!("stopped {n} worktrees")),
                        }
                        self.spawn_refresh();
                    }
                    PendingOutcome::Unshared(_) => {
                        self.set_success(format!("{label} is no longer public"));
                        self.spawn_refresh();
                    }
                }
                // After the outcome, so the header has them: a hook that
                // changed the worktree is what Invariant 1 promises is
                // said, and the one command that removes the volumes an
                // `rm` could not is not left to be found in `m`.
                let warnings: Vec<&str> = said
                    .iter()
                    .map(String::as_str)
                    .filter(|line| is_warning(line))
                    .collect();
                if !warnings.is_empty() {
                    self.set_error(warnings.join(" · "));
                }
            }
            Ok(Err(e)) => {
                let (kind, label, name) =
                    (pending.kind, pending.label.clone(), pending.name.clone());
                let said = pending.all_it_said();
                self.pending = None;
                self.keep_what_it_said(&format!("{} {label}", kind.verb()), &said);
                // The one start failure that is not about this worktree but
                // about the project: from now on `⏎` and `s` say so up
                // front.
                if matches!(kind, PendingKind::Start | PendingKind::Restart)
                    && e.contains(NO_PROCESSES)
                {
                    self.nothing_to_run = true;
                    self.set_error_about(&name, self.nothing_to_run_line());
                    return;
                }
                // A create whose install step failed kept its worktree, and
                // says so itself: "could not create" in front of it
                // contradicted it, and the cursor goes to it as to any
                // worktree `n` made.
                if kind == PendingKind::Create && e.contains(actions::CREATED_BUT_INSTALL_FAILED) {
                    self.set_error_about(&name, e);
                    self.select_on_arrival = Some(name);
                    self.spawn_discovery();
                    return;
                }
                self.set_error_about(&name, format!("could not {} {label}: {e}", kind.noun()));
            }
            // A worker blocked on the question modal is not doing anything
            // a spinner could stand for, and its clock is the reader's.
            Err(mpsc::TryRecvError::Empty) if asking => {
                let message = format!("? {} is waiting for your answer", pending.label);
                if may_spin {
                    self.set_progress(message);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                pending.spinner_frame = pending.spinner_frame.wrapping_add(1);
                let glyph = SPINNER_FRAMES[pending.spinner_frame as usize % SPINNER_FRAMES.len()];
                let label = format!("{} {}", pending.kind.verb(), pending.label);
                let elapsed = pending.started_at.elapsed().as_secs();
                let suffix = if elapsed >= 3 {
                    format!(" {elapsed}s")
                } else {
                    String::new()
                };
                let message = match &pending.stage {
                    Some(stage) => format!("{glyph} {label} · {stage}{suffix}"),
                    None => format!("{glyph} {label}…{suffix}"),
                };
                if may_spin {
                    self.set_progress(message);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let kind = pending.kind;
                self.pending = None;
                self.set_error(format!("{} worker stopped unexpectedly", kind.verb()));
            }
        }
    }
}
