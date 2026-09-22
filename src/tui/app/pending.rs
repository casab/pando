//! Actions in flight on a worker thread, and what they report when done.

use anyhow::Result;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use crate::actions;

use super::App;

const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Create,
    Remove,
    Start,
    Stop,
    Restart,
    Share,
    Unshare,
}

impl PendingKind {
    fn verb(self) -> &'static str {
        match self {
            PendingKind::Create => "creating",
            PendingKind::Remove => "removing",
            PendingKind::Start => "starting",
            PendingKind::Stop => "stopping",
            PendingKind::Restart => "restarting",
            PendingKind::Share => "sharing",
            PendingKind::Unshare => "unsharing",
        }
    }
}

pub enum PendingOutcome {
    Created(String),
    Removed(String),
    Started(String, Option<String>),
    Stopped(String),
    /// The worktree, its public URL, and whether a proxy is injecting a
    /// header in front of it.
    Shared(String, String, bool),
    Unshared(String),
}

pub struct PendingAction {
    pub name: String,
    pub kind: PendingKind,
    pub rx: Receiver<Result<PendingOutcome, String>>,
    pub started_at: Instant,
    pub spinner_frame: u8,
    pub progress_rx: Option<Receiver<String>>,
    pub stage: Option<String>,
}

impl App {
    /// Returns whether the action started; the create modal only closes (and
    /// throws away what was typed) once it did.
    pub(super) fn spawn_create(&mut self, branch: String) -> bool {
        let dir_name = actions::sanitize_branch_to_dir(&branch);
        let paths = self.paths.clone();
        let config = self.config.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(dir_name, PendingKind::Create, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            actions::new(&paths, &config, &branch, None, &progress)
                .map(PendingOutcome::Created)
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
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
        if let Some(p) = &self.pending {
            let busy = p.name.clone();
            self.set_error(format!("already busy with {busy}"));
            return false;
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.set_status(format!("{} {} {name}…", SPINNER_FRAMES[0], kind.verb()));
        self.pending = Some(PendingAction {
            name,
            kind,
            rx,
            started_at: Instant::now(),
            spinner_frame: 0,
            progress_rx: None,
            stage: None,
        });
        true
    }

    pub fn poll_pending(&mut self) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if let Some(prx) = pending.progress_rx.as_ref() {
            let mut latest = None;
            while let Ok(msg) = prx.try_recv() {
                latest = Some(msg);
            }
            if latest.is_some() {
                pending.stage = latest;
            }
        }
        match pending.rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.pending = None;
                match outcome {
                    PendingOutcome::Created(name) => {
                        self.set_status(format!("created {name}"));
                        self.spawn_discovery();
                    }
                    PendingOutcome::Removed(name) => {
                        self.set_status(format!("removed {name}"));
                        self.spawn_discovery();
                    }
                    PendingOutcome::Started(name, url) => {
                        match url {
                            Some(url) => self.set_status(format!("started {name} — {url}")),
                            None => self.set_status(format!("started {name}")),
                        }
                        // The log is new, so whatever was tailed for this
                        // worktree is about the run that just ended.
                        self.log_tails.forget_worktree(&name);
                        self.tail_scroll = 0;
                        self.spawn_refresh();
                    }
                    PendingOutcome::Stopped(name) => {
                        self.set_status(format!("stopped {name}"));
                        self.spawn_refresh();
                    }
                    PendingOutcome::Shared(name, url, pre_authed) => {
                        let how = if pre_authed { " (pre-authed)" } else { "" };
                        self.set_status(format!("{name} is at {url}{how} — O opens it"));
                        self.spawn_refresh();
                    }
                    PendingOutcome::Unshared(name) => {
                        self.set_status(format!("{name} is no longer public"));
                        self.spawn_refresh();
                    }
                }
            }
            Ok(Err(e)) => {
                self.pending = None;
                self.set_error(e);
            }
            Err(mpsc::TryRecvError::Empty) => {
                pending.spinner_frame = pending.spinner_frame.wrapping_add(1);
                let glyph = SPINNER_FRAMES[pending.spinner_frame as usize % SPINNER_FRAMES.len()];
                let label = format!("{} {}", pending.kind.verb(), pending.name);
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
                self.set_status(message);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let kind = pending.kind;
                self.pending = None;
                self.set_error(format!("{} worker stopped unexpectedly", kind.verb()));
            }
        }
    }
}
