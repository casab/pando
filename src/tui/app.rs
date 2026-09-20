//! TUI state and the key handlers.
//!
//! Two rules from the origin tool are carried unchanged, because breaking
//! either one is immediately visible to the user:
//!
//! - Nothing reachable from here calls a blocking `.status()` or a bare
//!   `.spawn()` that inherits the terminal. A child that writes to the
//!   terminal paints over the alternate screen.
//! - No git or network call runs inline in a key handler. They all run on a
//!   worker thread and report back as an `AppEvent`, or the frame freezes.

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::actions;
use crate::cache;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::worktree::{self, BranchEntry, EnrichUpdate, PrInfo, Worktree};

/// How long a status message stays on the header before the counts return.
const STATUS_TTL: Duration = Duration::from_secs(6);
/// Ticks between porcelain re-discoveries. The fs watcher only sees pando's
/// own worktrees directory, so adopted worktrees elsewhere arrive here.
pub const SLOW_TICK_EVERY: u32 = 20;
const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

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
}

/// One consistent read of the repository, taken off the UI thread.
pub struct Snapshot {
    pub main: Worktree,
    pub worktrees: Vec<Worktree>,
    pub created_by_pando: BTreeMap<String, bool>,
    pub default_base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Create {
        input: String,
        branches: BranchLoadState,
        selected: usize,
    },
    Remove {
        name: String,
        /// Why this removal would be refused, if it would be.
        blocker: Option<RemoveBlocker>,
        /// Whether pando created it; drives the extra warning line.
        created_by_pando: bool,
    },
    Help,
}

/// Stated before the user confirms, rather than discovered after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveBlocker {
    Locked(Option<String>),
    Dirty,
    NotOurs,
}

impl RemoveBlocker {
    pub fn line(&self) -> String {
        match self {
            RemoveBlocker::Locked(Some(reason)) => {
                format!("locked ({reason}) — pando always refuses; unlock it first")
            }
            RemoveBlocker::Locked(None) => {
                "locked — pando always refuses; unlock it first".to_string()
            }
            RemoveBlocker::Dirty => {
                "has modified or untracked files — confirm to remove them with --force".to_string()
            }
            RemoveBlocker::NotOurs => {
                "pando did not create this worktree — confirm to remove it anyway".to_string()
            }
        }
    }

    /// A locked worktree can never be removed; the others are confirmable.
    pub fn is_fatal(&self) -> bool {
        matches!(self, RemoveBlocker::Locked(_))
    }
}

#[derive(Debug, Clone)]
pub enum BranchLoadState {
    Loading,
    Ready(Vec<BranchEntry>),
}

impl BranchLoadState {
    pub fn as_slice(&self) -> &[BranchEntry] {
        match self {
            BranchLoadState::Loading => &[],
            BranchLoadState::Ready(b) => b.as_slice(),
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, BranchLoadState::Loading)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRow {
    /// Fork a new branch of this name from the default base.
    NewBranch(String),
    /// Check out a branch that already exists.
    Existing(BranchEntry),
}

/// Rows of the create picker: the typed name as a new branch (unless it
/// exactly matches an existing one), then every branch that contains it.
pub fn create_rows(input: &str, branches: &[BranchEntry]) -> Vec<CreateRow> {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();
    let exact_match = !trimmed.is_empty() && branches.iter().any(|b| b.name == trimmed);
    let mut rows = Vec::new();
    if !trimmed.is_empty() && !exact_match {
        rows.push(CreateRow::NewBranch(trimmed.to_string()));
    }
    for b in branches {
        if !lower.is_empty() && !b.name.to_lowercase().contains(&lower) {
            continue;
        }
        rows.push(CreateRow::Existing(b.clone()));
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Create,
    Remove,
}

impl PendingKind {
    fn verb(self) -> &'static str {
        match self {
            PendingKind::Create => "creating",
            PendingKind::Remove => "removing",
        }
    }
}

pub enum PendingOutcome {
    Created(String),
    Removed(String),
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

pub struct Status {
    pub message: String,
    pub is_error: bool,
    pub at: Instant,
}

pub struct App {
    pub paths: PandoPaths,
    pub config: Config,
    pub main: Option<Worktree>,
    pub default_base: Option<String>,
    pub worktrees: Vec<Worktree>,
    pub created_by_pando: BTreeMap<String, bool>,
    pub prs: HashMap<String, PrInfo>,
    pub list_state: ListState,
    pub filter: String,
    pub filtered_indices: Vec<usize>,
    pub mode: Mode,
    pub modal: Option<Modal>,
    pub help_scroll: usize,
    pub status: Option<Status>,
    pub pending: Option<PendingAction>,
    pub enriching: bool,
    pub should_quit: bool,
    pub tick: u32,
    pub list_area: Option<Rect>,
    pub event_tx: Sender<AppEvent>,
    pub event_rx: Option<Receiver<AppEvent>>,
    /// Set instead of touching the terminal when tests drive the app.
    #[cfg(test)]
    pub clipboard: Option<String>,
}

impl App {
    pub fn new(paths: PandoPaths, config: Config) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let mut app = Self {
            paths,
            config,
            main: None,
            default_base: None,
            worktrees: Vec::new(),
            created_by_pando: BTreeMap::new(),
            prs: HashMap::new(),
            list_state: ListState::default(),
            filter: String::new(),
            filtered_indices: Vec::new(),
            mode: Mode::Normal,
            modal: None,
            help_scroll: 0,
            status: None,
            pending: None,
            enriching: false,
            should_quit: false,
            tick: 0,
            list_area: None,
            event_tx,
            event_rx: Some(event_rx),
            #[cfg(test)]
            clipboard: None,
        };
        // One synchronous read so the first frame has rows, hydrated from the
        // disk cache so those rows already carry sha, age, and dirty state.
        app.apply_snapshot(snapshot(&app.paths)?);
        app.hydrate_from_cache();
        app.spawn_enrichment(None);
        app.spawn_pr_fetch();
        Ok(app)
    }

    /// Returns whether the frame needs repainting.
    pub fn handle_event(&mut self, ev: AppEvent) -> bool {
        match ev {
            AppEvent::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                self.handle_key(key);
                true
            }
            AppEvent::Input(Event::Resize(..)) => true,
            AppEvent::Input(_) => false,
            AppEvent::Tick => {
                self.tick = self.tick.wrapping_add(1);
                let spinning = self.pending.is_some();
                self.poll_pending();
                if self.tick.is_multiple_of(SLOW_TICK_EVERY) {
                    self.spawn_discovery();
                }
                spinning || self.expire_status()
            }
            AppEvent::FsChange => {
                self.spawn_discovery();
                false
            }
            AppEvent::Discovered(result) => match *result {
                Ok(snapshot) => {
                    let fresh = self.apply_snapshot(snapshot);
                    if !fresh.is_empty() {
                        self.hydrate_from_cache();
                        self.spawn_enrichment(Some(fresh));
                    }
                    true
                }
                Err(e) => {
                    self.set_error(format!("refresh failed: {e}"));
                    true
                }
            },
            AppEvent::Enrich(update) => {
                if let Some(wt) = self.worktrees.iter_mut().find(|w| w.name == update.name) {
                    worktree::apply_update(wt, update);
                }
                true
            }
            AppEvent::EnrichDone => {
                self.enriching = false;
                self.save_enrich_cache();
                true
            }
            AppEvent::BranchesReady(branches) => {
                if let Some(Modal::Create {
                    branches: state, ..
                }) = self.modal.as_mut()
                {
                    *state = BranchLoadState::Ready(branches);
                    return true;
                }
                false
            }
            AppEvent::PrsReady(Ok(prs)) => {
                self.prs = prs.into_iter().map(|p| (p.branch.clone(), p)).collect();
                self.save_pr_cache();
                true
            }
            // A missing or unauthenticated `gh` just means no chips.
            AppEvent::PrsReady(Err(_)) => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.modal.take() {
            Some(Modal::Help) => {
                // Any key dismisses help, except scrolling it.
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.help_scroll += 1;
                        self.modal = Some(Modal::Help);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                        self.modal = Some(Modal::Help);
                    }
                    _ => self.help_scroll = 0,
                }
                return;
            }
            Some(Modal::Remove {
                name,
                blocker,
                created_by_pando,
            }) => {
                self.handle_remove_key(key, name, blocker, created_by_pando);
                return;
            }
            Some(Modal::Create {
                input,
                branches,
                selected,
            }) => {
                self.handle_create_key(key, input, branches, selected);
                return;
            }
            None => {}
        }
        if self.mode == Mode::Filter {
            self.handle_filter_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select_index(0),
            KeyCode::Char('G') | KeyCode::End => {
                self.select_index(self.filtered_indices.len().saturating_sub(1))
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('n') => self.open_create(),
            KeyCode::Char('d') => self.open_remove(),
            KeyCode::Char('y') => self.copy_selected_path(),
            KeyCode::Char('r') => {
                self.spawn_discovery();
                self.set_status("refreshing…");
            }
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.modal = Some(Modal::Help);
            }
            KeyCode::Enter => self.set_status("detail and logs arrive with start/stop"),
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.refilter();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.refilter();
            }
            _ => {}
        }
    }

    fn handle_create_key(
        &mut self,
        key: KeyEvent,
        mut input: String,
        branches: BranchLoadState,
        mut selected: usize,
    ) {
        let rows = create_rows(&input, branches.as_slice());
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Down => selected = (selected + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Backspace => {
                input.pop();
                selected = 0;
            }
            KeyCode::Char(c) => {
                input.push(c);
                selected = 0;
            }
            KeyCode::Enter => {
                let Some(row) = rows.get(selected) else {
                    self.set_error("type a branch name first");
                    self.modal = Some(Modal::Create {
                        input,
                        branches,
                        selected,
                    });
                    return;
                };
                let branch = match row {
                    CreateRow::NewBranch(name) => name.clone(),
                    CreateRow::Existing(entry) => entry.name.clone(),
                };
                let dir_name = actions::sanitize_branch_to_dir(&branch);
                if self.worktrees.iter().any(|w| w.name == dir_name) {
                    self.set_error(format!("{dir_name} already exists"));
                    self.modal = Some(Modal::Create {
                        input,
                        branches,
                        selected,
                    });
                    return;
                }
                if self.spawn_create(branch) {
                    return; // modal closes only once the work is under way
                }
                self.modal = Some(Modal::Create {
                    input,
                    branches,
                    selected,
                });
                return;
            }
            _ => {}
        }
        self.modal = Some(Modal::Create {
            input,
            branches,
            selected,
        });
    }

    fn handle_remove_key(
        &mut self,
        key: KeyEvent,
        name: String,
        blocker: Option<RemoveBlocker>,
        created_by_pando: bool,
    ) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Some(b) = &blocker
                    && b.is_fatal()
                {
                    self.set_error(format!("{name}: {}", b.line()));
                    return;
                }
                let force = blocker == Some(RemoveBlocker::Dirty);
                self.spawn_remove(name, !created_by_pando, force);
            }
            _ => {}
        }
    }

    fn open_create(&mut self) {
        self.modal = Some(Modal::Create {
            input: String::new(),
            branches: BranchLoadState::Loading,
            selected: 0,
        });
        self.spawn_branch_fetch();
    }

    fn open_remove(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            self.set_error("nothing selected");
            return;
        };
        let created_by_pando = self
            .created_by_pando
            .get(&wt.name)
            .copied()
            .unwrap_or(false);
        let blocker = if wt.locked {
            Some(RemoveBlocker::Locked(wt.lock_reason.clone()))
        } else if !created_by_pando {
            Some(RemoveBlocker::NotOurs)
        } else if wt.dirty == Some(true) {
            Some(RemoveBlocker::Dirty)
        } else {
            None
        };
        self.modal = Some(Modal::Remove {
            name: wt.name.clone(),
            blocker,
            created_by_pando,
        });
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        let row = self.list_state.selected()?;
        let idx = *self.filtered_indices.get(row)?;
        self.worktrees.get(idx)
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let last = self.filtered_indices.len() as isize - 1;
        self.select_index((current + delta).clamp(0, last) as usize);
    }

    fn select_index(&mut self, index: usize) {
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state
                .select(Some(index.min(self.filtered_indices.len() - 1)));
        }
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        let keep_name = self.selected_worktree().map(|w| w.name.clone());
        self.filtered_indices = self
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                needle.is_empty()
                    || w.name.to_lowercase().contains(&needle)
                    || w.branch
                        .as_deref()
                        .is_some_and(|b| b.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        // Keep the cursor on the same worktree when it survives the filter.
        let row = keep_name.and_then(|name| {
            self.filtered_indices
                .iter()
                .position(|&i| self.worktrees[i].name == name)
        });
        match row {
            Some(r) => self.list_state.select(Some(r)),
            None => self.select_index(0),
        }
    }

    /// Replaces the list, preserving enrichment already collected for names
    /// that are still there. Returns the names that are new, so only those
    /// get enriched.
    fn apply_snapshot(&mut self, snapshot: Snapshot) -> Vec<String> {
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
        self.refilter();
        fresh
    }

    fn hydrate_from_cache(&mut self) {
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

    fn save_enrich_cache(&self) {
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

    fn save_pr_cache(&self) {
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

    pub fn spawn_discovery(&self) {
        let paths = self.paths.clone();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let result = snapshot(&paths).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Discovered(Box::new(result)));
        });
    }

    /// Enriches `only` when given, every worktree otherwise.
    fn spawn_enrichment(&mut self, only: Option<Vec<String>>) {
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

    fn spawn_branch_fetch(&self) {
        let root = self.paths.root().to_path_buf();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(AppEvent::BranchesReady(worktree::list_branches(&root)));
        });
    }

    /// Returns whether the action started; the create modal only closes (and
    /// throws away what was typed) once it did.
    fn spawn_create(&mut self, branch: String) -> bool {
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

    fn spawn_remove(&mut self, name: String, yes: bool, force: bool) -> bool {
        let paths = self.paths.clone();
        let worker_name = name.clone();
        self.spawn_pending(name, PendingKind::Remove, move || {
            actions::rm(&paths, &worker_name, yes, force)
                .map(|()| PendingOutcome::Removed(worker_name))
                .map_err(|e| format!("{e:#}"))
        })
    }

    /// Runs `work` on a worker thread and parks the receiver. Single slot:
    /// a second action while one is in flight is refused rather than queued.
    fn spawn_pending<F>(&mut self, name: String, kind: PendingKind, work: F) -> bool
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

    fn copy_selected_path(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            self.set_error("nothing selected");
            return;
        };
        let path = wt.path.display().to_string();
        self.copy_to_clipboard(&path);
        self.set_status(format!("copied {path}"));
    }

    #[cfg(test)]
    fn copy_to_clipboard(&mut self, text: &str) {
        self.clipboard = Some(text.to_string());
    }

    /// OSC 52 first: it works on Linux and macOS, and through tmux with
    /// `set-clipboard on`. `pbcopy` is a macOS-only fallback for terminals
    /// that ignore the sequence; it is spawned with every stdio redirected,
    /// never inheriting the alternate screen.
    #[cfg(not(test))]
    fn copy_to_clipboard(&mut self, text: &str) {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = stdout.flush();

        if cfg!(target_os = "macos")
            && let Ok(mut child) = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(Status {
            message: message.into(),
            is_error: false,
            at: Instant::now(),
        });
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = Some(Status {
            message: message.into(),
            is_error: true,
            at: Instant::now(),
        });
    }

    /// The status message while it is still fresh.
    pub fn active_status(&self) -> Option<(&str, bool)> {
        let status = self.status.as_ref()?;
        (status.at.elapsed() < STATUS_TTL).then_some((status.message.as_str(), status.is_error))
    }

    fn expire_status(&mut self) -> bool {
        let expired = self
            .status
            .as_ref()
            .is_some_and(|s| s.at.elapsed() >= STATUS_TTL);
        if expired {
            self.status = None;
        }
        expired
    }

    #[cfg(test)]
    pub fn new_for_test(paths: PandoPaths, config: Config, worktrees: Vec<Worktree>) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let mut app = Self {
            paths,
            config,
            main: None,
            default_base: Some("main".into()),
            worktrees,
            created_by_pando: BTreeMap::new(),
            prs: HashMap::new(),
            list_state: ListState::default(),
            filter: String::new(),
            filtered_indices: Vec::new(),
            mode: Mode::Normal,
            modal: None,
            help_scroll: 0,
            status: None,
            pending: None,
            enriching: false,
            should_quit: false,
            tick: 0,
            list_area: None,
            event_tx,
            event_rx: Some(event_rx),
            clipboard: None,
        };
        app.refilter();
        app
    }
}

/// One consistent read of the repository: the worktrees, who owns them, and
/// the base a new branch would fork from. Runs off the UI thread.
pub fn snapshot(paths: &PandoPaths) -> Result<Snapshot> {
    let discovery = worktree::discover_all(&paths.project)?;
    Ok(Snapshot {
        main: discovery.main,
        worktrees: discovery.worktrees,
        created_by_pando: actions::created_by_pando(paths),
        default_base: worktree::resolve_base_branch(paths.root()),
    })
}

/// Standard base64, for the OSC 52 payload. A dependency would be a lot of
/// machinery for one escape sequence.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::worktree::BranchSource;
    use std::path::PathBuf;

    pub fn wt(name: &str) -> Worktree {
        Worktree {
            name: name.to_string(),
            path: PathBuf::from("/trees").join(name),
            head: Some("abc123".into()),
            branch: Some(name.replace('+', "/")),
            detached: false,
            prunable: false,
            prunable_reason: None,
            locked: false,
            lock_reason: None,
            bare: false,
            created_at: None,
            head_sha: Some("abc1234".into()),
            head_subject: Some("do the thing".into()),
            head_age: Some("2 hours ago".into()),
            dirty: Some(false),
            ahead_behind: Some((1, 0)),
        }
    }

    pub fn test_app(names: &[&str]) -> App {
        let paths = PandoPaths::new(
            "/tmp/pando-test-home",
            ProjectRef {
                id: "acme-shop-3f9a2c1d".into(),
                root: PathBuf::from("/tmp/acme-shop"),
                display_name: "acme-shop".into(),
            },
        );
        let worktrees: Vec<Worktree> = names.iter().map(|n| wt(n)).collect();
        let mut app = App::new_for_test(paths, Config::default(), worktrees);
        app.created_by_pando = names.iter().map(|n| (n.to_string(), true)).collect();
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn the_cursor_moves_and_clamps_at_both_ends() {
        let mut app = test_app(&["a", "b", "c"]);
        assert_eq!(app.list_state.selected(), Some(0));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.list_state.selected(), Some(2));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.list_state.selected(), Some(2), "clamped at the end");
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.list_state.selected(), Some(0));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.list_state.selected(), Some(0), "clamped at the start");
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[test]
    fn an_empty_list_has_no_selection_and_ignores_movement() {
        let mut app = test_app(&[]);
        assert_eq!(app.list_state.selected(), None);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn filtering_narrows_the_list_and_escape_restores_it() {
        let mut app = test_app(&["feat+one", "feat+two", "fix+three"]);
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.mode, Mode::Filter);
        type_str(&mut app, "fix");
        assert_eq!(app.filtered_indices.len(), 1);
        assert_eq!(app.selected_worktree().unwrap().name, "fix+three");

        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.filtered_indices.len(), 1, "\"fi\" still matches one");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.filter.is_empty());
        assert_eq!(app.filtered_indices.len(), 3);
    }

    #[test]
    fn filtering_matches_the_branch_as_well_as_the_name() {
        let mut app = test_app(&["odd+name"]);
        app.worktrees[0].branch = Some("release/1.2".into());
        press(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "release");
        assert_eq!(app.filtered_indices.len(), 1);
    }

    #[test]
    fn enter_leaves_filter_mode_but_keeps_the_filter() {
        let mut app = test_app(&["feat+one", "fix+two"]);
        press(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "fix");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, "fix");
        assert_eq!(app.filtered_indices.len(), 1);
    }

    #[test]
    fn q_quits_and_ctrl_c_quits_from_anywhere() {
        let mut app = test_app(&["a"]);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);

        let mut app = test_app(&["a"]);
        press(&mut app, KeyCode::Char('?'));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "ctrl-c quits even with a modal open");
    }

    #[test]
    fn the_help_modal_opens_scrolls_and_closes() {
        let mut app = test_app(&["a"]);
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.modal, Some(Modal::Help)));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.help_scroll, 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.help_scroll, 0);
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
        assert_eq!(app.help_scroll, 0, "scroll resets for the next open");
    }

    #[test]
    fn the_create_modal_opens_accepts_typing_and_closes_on_escape() {
        let mut app = test_app(&["a"]);
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.modal, Some(Modal::Create { .. })));
        type_str(&mut app, "feat/new");
        match &app.modal {
            Some(Modal::Create { input, .. }) => assert_eq!(input, "feat/new"),
            other => panic!("expected the create modal, got {other:?}"),
        }
        press(&mut app, KeyCode::Backspace);
        match &app.modal {
            Some(Modal::Create { input, .. }) => assert_eq!(input, "feat/ne"),
            other => panic!("expected the create modal, got {other:?}"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    // Validation happens in the modal, before any worker starts, so a
    // colliding name is refused with what was typed still on screen.
    #[test]
    fn the_create_modal_refuses_a_name_that_already_exists() {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feat/one");
        press(&mut app, KeyCode::Enter);

        assert!(
            matches!(app.modal, Some(Modal::Create { .. })),
            "modal stays open"
        );
        assert!(app.pending.is_none(), "no worker should have started");
        let (message, is_error) = app.active_status().unwrap();
        assert!(is_error);
        assert!(message.contains("already exists"), "{message}");
    }

    #[test]
    fn enter_on_an_empty_create_modal_asks_for_a_name() {
        let mut app = test_app(&[]);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, Some(Modal::Create { .. })));
        assert!(app.active_status().unwrap().0.contains("branch name"));
    }

    #[test]
    fn branches_arriving_populate_an_open_create_modal() {
        let mut app = test_app(&[]);
        press(&mut app, KeyCode::Char('n'));
        let branches = vec![BranchEntry {
            name: "main".into(),
            source: BranchSource::Local,
        }];
        app.handle_event(AppEvent::BranchesReady(branches));
        match &app.modal {
            Some(Modal::Create { branches, .. }) => {
                assert!(!branches.is_loading());
                assert_eq!(branches.as_slice().len(), 1);
            }
            other => panic!("expected the create modal, got {other:?}"),
        }
    }

    #[test]
    fn the_remove_modal_names_its_target_and_closes_on_escape() {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, KeyCode::Char('d'));
        match &app.modal {
            Some(Modal::Remove { name, blocker, .. }) => {
                assert_eq!(name, "feat+one");
                assert!(blocker.is_none(), "a clean pando worktree has no blocker");
            }
            other => panic!("expected the remove modal, got {other:?}"),
        }
        press(&mut app, KeyCode::Char('n'));
        assert!(app.modal.is_none());
    }

    #[test]
    fn the_remove_modal_states_why_a_removal_would_be_refused() {
        let mut app = test_app(&["locked+one", "dirty+one", "adopted+one"]);
        app.worktrees[0].locked = true;
        app.worktrees[0].lock_reason = Some("benchmarking".into());
        app.worktrees[1].dirty = Some(true);
        app.created_by_pando.insert("adopted+one".into(), false);

        press(&mut app, KeyCode::Char('d'));
        let blocker = match &app.modal {
            Some(Modal::Remove { blocker, .. }) => blocker.clone().unwrap(),
            other => panic!("expected the remove modal, got {other:?}"),
        };
        assert!(blocker.is_fatal(), "a locked worktree can never be removed");
        assert!(blocker.line().contains("benchmarking"));

        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        match &app.modal {
            Some(Modal::Remove { blocker, .. }) => {
                assert_eq!(blocker, &Some(RemoveBlocker::Dirty));
                assert!(!blocker.as_ref().unwrap().is_fatal());
            }
            other => panic!("expected the remove modal, got {other:?}"),
        }

        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        match &app.modal {
            Some(Modal::Remove { blocker, .. }) => {
                assert_eq!(blocker, &Some(RemoveBlocker::NotOurs))
            }
            other => panic!("expected the remove modal, got {other:?}"),
        }
    }

    // Confirming a locked worktree must not start a worker that git would
    // refuse anyway.
    #[test]
    fn confirming_a_locked_removal_refuses_without_starting_work() {
        let mut app = test_app(&["locked+one"]);
        app.worktrees[0].locked = true;
        app.worktrees[0].lock_reason = Some("benchmarking".into());
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));

        assert!(app.pending.is_none(), "no worker for a refusal");
        assert!(app.modal.is_none());
        let (message, is_error) = app.active_status().unwrap();
        assert!(is_error);
        assert!(message.contains("locked"), "{message}");
    }

    #[test]
    fn d_with_nothing_selected_says_so() {
        let mut app = test_app(&[]);
        press(&mut app, KeyCode::Char('d'));
        assert!(app.modal.is_none());
        assert!(app.active_status().unwrap().1, "should be an error");
    }

    #[test]
    fn y_copies_the_selected_worktree_path() {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.clipboard.as_deref(), Some("/trees/feat+one"));
        assert!(app.active_status().unwrap().0.contains("/trees/feat+one"));
    }

    #[test]
    fn create_rows_offers_the_typed_name_then_matching_branches() {
        let branches = vec![
            BranchEntry {
                name: "main".into(),
                source: BranchSource::Local,
            },
            BranchEntry {
                name: "feat/one".into(),
                source: BranchSource::Remote,
            },
        ];
        let rows = create_rows("", &branches);
        assert_eq!(rows.len(), 2, "empty input offers no new-branch row");
        assert!(matches!(rows[0], CreateRow::Existing(_)));

        let rows = create_rows("feat", &branches);
        assert_eq!(rows[0], CreateRow::NewBranch("feat".into()));
        assert!(matches!(&rows[1], CreateRow::Existing(b) if b.name == "feat/one"));

        let rows = create_rows("feat/one", &branches);
        assert_eq!(
            rows.len(),
            1,
            "an exact match suppresses the new-branch row: {rows:?}"
        );
        assert!(matches!(&rows[0], CreateRow::Existing(b) if b.name == "feat/one"));

        let rows = create_rows("nothing-matches", &branches);
        assert_eq!(rows, vec![CreateRow::NewBranch("nothing-matches".into())]);

        assert!(create_rows("", &[]).is_empty());
    }

    #[test]
    fn create_rows_matching_is_case_insensitive_and_input_is_trimmed() {
        let branches = vec![BranchEntry {
            name: "Feat/One".into(),
            source: BranchSource::Local,
        }];
        let rows = create_rows("  feat  ", &branches);
        assert_eq!(rows[0], CreateRow::NewBranch("feat".into()));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_snapshot_keeps_enrichment_for_unchanged_worktrees() {
        let mut app = test_app(&["feat+one"]);
        let mut refreshed = wt("feat+one");
        refreshed.head_sha = None;
        refreshed.dirty = None;
        let fresh = app.apply_snapshot(Snapshot {
            main: wt("acme-shop"),
            worktrees: vec![refreshed, wt("feat+two")],
            created_by_pando: BTreeMap::new(),
            default_base: Some("main".into()),
        });

        assert_eq!(fresh, vec!["feat+two"], "only new entries need enriching");
        let kept = app.worktrees.iter().find(|w| w.name == "feat+one").unwrap();
        assert_eq!(
            kept.head_sha.as_deref(),
            Some("abc1234"),
            "enrichment already collected must survive a refresh"
        );
    }

    #[test]
    fn a_snapshot_re_enriches_a_worktree_whose_head_moved() {
        let mut app = test_app(&["feat+one"]);
        let mut moved = wt("feat+one");
        moved.head = Some("def456".into());
        let fresh = app.apply_snapshot(Snapshot {
            main: wt("acme-shop"),
            worktrees: vec![moved],
            created_by_pando: BTreeMap::new(),
            default_base: None,
        });
        assert_eq!(fresh, vec!["feat+one"]);
    }

    #[test]
    fn the_status_message_expires() {
        let mut app = test_app(&[]);
        app.set_status("hello");
        assert!(app.active_status().is_some());
        app.status.as_mut().unwrap().at = Instant::now() - STATUS_TTL - Duration::from_secs(1);
        assert!(app.active_status().is_none());
        assert!(app.expire_status());
        assert!(app.status.is_none());
    }

    #[test]
    fn base64_matches_the_reference_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("/trees/feat+one".as_bytes()), "L3RyZWVzL2ZlYXQrb25l");
    }
}
