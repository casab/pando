//! Work that leaves the UI thread: the events it reports back with, the
//! repository snapshot, and the refresh, discovery and fetch workers.

use anyhow::Result;
use ratatui::crossterm::event::Event;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::actions;
use crate::cache;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state::State;
use crate::worktree::{self, BranchEntry, EnrichUpdate, PrInfo, Worktree};

use super::{App, ServiceHealth};

/// Everything the event loop can deliver. One enum so the loop blocks on a
/// single receive and redraws once per batch.
pub enum AppEvent {
    Input(Event),
    Tick,
    FsChange,
    Discovered(Box<Result<Snapshot, String>>),
    Enrich(EnrichUpdate),
    EnrichDone,
    BranchesReady(Vec<BranchEntry>),
    PrsReady(Result<Vec<PrInfo>, String>),
    /// Process state, advanced and saved off the UI thread.
    Refreshed(Box<Result<State, String>>),
    /// Whether the project's services are answering — the shared ones for
    /// the header chips, each worktree's private ones for its detail rows.
    /// Probed on the refresh worker: it is a TCP connect apiece.
    ServiceHealth(Box<ServiceHealth>),
    /// A worker needs an answer before it can go on. It is blocked on the
    /// other end of this channel until one arrives — or until the channel
    /// is dropped, which is how quitting aborts it.
    AskQuestion(Box<(actions::Question, Sender<Result<actions::Answer, String>>)>),
    /// Detection settled something, and this is the config with it applied.
    /// Sent as soon as it is resolved rather than with the outcome: the
    /// answer is on disk by then, so the UI thread's copy has to match
    /// whether or not the start that followed worked.
    ConfigResolved(Box<Config>),
}

/// One consistent read of the repository, taken off the UI thread.
pub struct Snapshot {
    pub main: Worktree,
    pub worktrees: Vec<Worktree>,
    pub created_by_pando: BTreeMap<String, bool>,
    /// Process state as of the same read, so the list and the detail pane
    /// never disagree about what is running.
    pub state: State,
    /// Why the ownership map may be wrong — the same one line `ls` prints
    /// and `rm` refuses with, so the TUI never shows "adopted" about a
    /// state file it could not read without saying so.
    pub warning: Option<String>,
    /// What the refresh itself had to do — a share whose tunnel died, most
    /// often. Shown once, as a status line.
    pub notices: Vec<String>,
    pub default_base: Option<String>,
}

/// Sends a question to the UI thread and waits for the answer.
///
/// Called from a worker, never from the UI thread. A dropped receiver — the
/// app quitting while the modal is open — comes back as an error, so the
/// worker unwinds instead of waiting for an answer nobody will give.
pub(super) fn ask_through_ui(
    tx: &Sender<AppEvent>,
    question: &actions::Question,
) -> Result<actions::Answer> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(AppEvent::AskQuestion(Box::new((
        question.clone(),
        reply_tx,
    ))))
    .map_err(|_| anyhow::anyhow!("pando is shutting down"))?;
    match reply_rx.recv() {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(reason)) => Err(anyhow::anyhow!(reason)),
        Err(_) => Err(anyhow::anyhow!("cancelled")),
    }
}

/// One consistent read of the repository: the worktrees, who owns them, and
/// the base a new branch would fork from. Runs off the UI thread.
pub fn snapshot(paths: &PandoPaths) -> Result<Snapshot> {
    let discovery = worktree::discover_all(&paths.project)?;
    // One read of state for both answers, so the list's ownership dots and
    // its status column cannot come from two different moments.
    let refreshed = actions::refresh(paths);
    Ok(Snapshot {
        created_by_pando: actions::ownership(&refreshed.state, &discovery.worktrees),
        main: discovery.main,
        worktrees: discovery.worktrees,
        state: refreshed.state,
        warning: refreshed.warning,
        notices: refreshed.notices,
        default_base: worktree::resolve_base_branch(paths.root()),
    })
}

impl App {
    /// Replaces the list, preserving enrichment already collected for names
    /// that are still there. Returns the names that are new, so only those
    /// get enriched.
    pub(super) fn apply_snapshot(&mut self, snapshot: Snapshot) -> Vec<String> {
        // Captured before the list is replaced: after that, the old row
        // index points at whatever worktree the refresh moved into it.
        let keep = self.selected_worktree().map(|w| w.name.clone());
        let known: HashMap<String, Worktree> = self
            .worktrees
            .drain(..)
            .map(|w| (w.name.clone(), w))
            .collect();
        let mut fresh = Vec::new();
        self.worktrees = snapshot
            .worktrees
            .into_iter()
            .map(|mut w| {
                match known.get(&w.name) {
                    Some(old) if old.head == w.head => {
                        w.head_sha = old.head_sha.clone();
                        w.head_subject = old.head_subject.clone();
                        w.head_age = old.head_age.clone();
                        w.dirty = old.dirty;
                        w.ahead_behind = old.ahead_behind;
                    }
                    _ => fresh.push(w.name.clone()),
                }
                w
            })
            .collect();
        self.main = Some(snapshot.main);
        self.default_base = snapshot.default_base;
        self.created_by_pando = snapshot.created_by_pando;
        self.state = snapshot.state;
        if snapshot.warning != self.state_warning {
            if let Some(message) = snapshot.warning.clone() {
                self.set_error(message);
            }
            self.state_warning = snapshot.warning;
        }
        // Shares that died since the last tick. Every one of them, once:
        // the records are already gone, so the next tick has nothing to
        // repeat, and showing only the first means the second worktree's
        // public URL closed in silence. The status line truncates rather
        // than wraps, like every other row.
        if !snapshot.notices.is_empty() {
            self.set_error(snapshot.notices.join(" · "));
        }
        self.refilter_keeping(keep);
        fresh
    }

    pub(super) fn hydrate_from_cache(&mut self) {
        let cached = cache::load(&self.paths.enrich_cache_file());
        for wt in self.worktrees.iter_mut() {
            if wt.head_sha.is_some() {
                continue;
            }
            if let Some(entry) = cached.entries.get(&wt.name) {
                cache::apply_to_worktree(wt, entry);
            }
        }
    }

    pub(super) fn save_enrich_cache(&self) {
        let mut file = cache::CacheFile::new();
        for wt in &self.worktrees {
            file.entries.insert(
                wt.name.clone(),
                cache::CachedEnrich {
                    branch: wt.branch.clone(),
                    prunable: wt.prunable,
                    head_sha: wt.head_sha.clone(),
                    head_subject: wt.head_subject.clone(),
                    head_age: wt.head_age.clone(),
                    dirty: wt.dirty,
                    ahead_behind: wt.ahead_behind,
                },
            );
        }
        let _ = cache::save(&self.paths.enrich_cache_file(), &file);
    }

    pub(super) fn save_pr_cache(&self) {
        let mut file = cache::PrCacheFile::new();
        file.prs = self
            .prs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let _ = cache::save_prs(&self.paths.pr_cache_file(), &file);
    }

    pub fn pr_for(&self, wt: &Worktree) -> Option<&PrInfo> {
        self.prs.get(wt.branch.as_deref()?)
    }

    // ---- background work -------------------------------------------------
    //
    // Everything below leaves the UI thread immediately. A key handler that
    // shells out to git freezes the frame for as long as git takes.

    /// Advances phases, captures observed ports, and saves — off the UI
    /// thread, because it takes the state lock and scans listening sockets.
    pub fn spawn_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let paths = self.paths.clone();
        let config = self.config.clone();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let refreshed = actions::refresh(&paths);
            // Before the state is handed over: probing is a TCP connect
            // per service, and it has no business on the UI thread or in
            // a paint.
            let health = ServiceHealth {
                shared: actions::shared_service_statuses(&paths, &config),
                worktrees: refreshed
                    .state
                    .worktrees
                    .iter()
                    .filter(|(_, record)| !record.services.is_empty())
                    .map(|(name, record)| (name.clone(), actions::service_statuses(record)))
                    .collect(),
            };
            let result = match refreshed.warning {
                Some(warning) => Err(warning),
                None => Ok(refreshed.state),
            };
            let _ = tx.send(AppEvent::Refreshed(Box::new(result)));
            if !health.shared.is_empty() || !health.worktrees.is_empty() {
                let _ = tx.send(AppEvent::ServiceHealth(Box::new(health)));
            }
        });
    }

    pub fn spawn_discovery(&self) {
        let paths = self.paths.clone();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let result = snapshot(&paths).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Discovered(Box::new(result)));
        });
    }

    /// Enriches `only` when given, every worktree otherwise.
    pub(super) fn spawn_enrichment(&mut self, only: Option<Vec<String>>) {
        let items: Vec<(String, std::path::PathBuf)> = self
            .worktrees
            .iter()
            .filter(|w| only.as_ref().is_none_or(|names| names.contains(&w.name)))
            .map(|w| (w.name.clone(), w.path.clone()))
            .collect();
        if items.is_empty() {
            return;
        }
        self.enriching = true;
        let root = self.paths.root().to_path_buf();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let (etx, erx) = mpsc::channel();
            let forward = tx.clone();
            let pump = thread::spawn(move || {
                while let Ok(update) = erx.recv() {
                    if forward.send(AppEvent::Enrich(update)).is_err() {
                        break;
                    }
                }
            });
            // Narrow pool: every job forks `git status` against a full tree,
            // and startup enrichment must not starve the UI.
            worktree::enrich_stream(&root, items, etx, 4);
            let _ = pump.join();
            let _ = tx.send(AppEvent::EnrichDone);
        });
    }

    pub fn spawn_pr_fetch(&self) {
        let root = self.paths.root().to_path_buf();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let result = worktree::list_prs(&root).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::PrsReady(result));
        });
    }

    pub(super) fn spawn_branch_fetch(&self) {
        let root = self.paths.root().to_path_buf();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(AppEvent::BranchesReady(worktree::list_branches(&root)));
        });
    }
}
