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
use crate::log_tail::LogTail;
use crate::paths::PandoPaths;
use crate::state::{self, Aggregate, ProcessRecord, State, WorktreeRecord};
use crate::worktree::{self, BranchEntry, EnrichUpdate, PrInfo, Worktree};

/// How long a status message stays on the header before the counts return.
const STATUS_TTL: Duration = Duration::from_secs(6);
/// Ticks between porcelain re-discoveries. The fs watcher only sees pando's
/// own worktrees directory, so adopted worktrees elsewhere arrive here.
pub const SLOW_TICK_EVERY: u32 = 20;
/// Ticks between process-state refreshes. Much faster than discovery,
/// because a dev server that just died should turn red while the developer
/// is still looking at it — and far cheaper, because it reads one state file
/// rather than forking git.
pub const REFRESH_EVERY: u32 = 4;
/// How many worktrees' log tails are kept open at once. Each holds a file
/// handle and a ring buffer; the selected row is always one of them.
const MAX_LOG_TAILS: usize = 8;
/// Lines of the dev log the detail pane keeps.
const TAIL_CAPACITY: usize = 256;
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
    /// Process state, advanced and saved off the UI thread.
    Refreshed(Box<Result<State, String>>),
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
    /// Something pando needs to know before it can start. A worker thread is
    /// waiting on `reply`; closing this modal without answering has to send
    /// something, or that thread waits forever.
    Question {
        question: actions::Question,
        selected: usize,
        /// `Some` while a command is being typed instead of chosen.
        custom: Option<String>,
        reply: Sender<Result<actions::Answer, String>>,
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
    Start,
    Stop,
    Restart,
}

impl PendingKind {
    fn verb(self) -> &'static str {
        match self {
            PendingKind::Create => "creating",
            PendingKind::Remove => "removing",
            PendingKind::Start => "starting",
            PendingKind::Stop => "stopping",
            PendingKind::Restart => "restarting",
        }
    }
}

pub enum PendingOutcome {
    Created(String),
    Removed(String),
    Started(String, Option<String>),
    Stopped(String),
}

/// A bounded set of open log tails, evicted by least-recent use.
///
/// A tail holds a file handle and a ring buffer, and a project with fifty
/// worktrees would otherwise keep fifty of each. The selected row is touched
/// on every poll, so it never evicts itself.
#[derive(Default)]
pub struct LogTails {
    open: HashMap<String, (LogTail, u64)>,
    clock: u64,
}

impl LogTails {
    pub fn get(&self, name: &str) -> Option<&LogTail> {
        self.open.get(name).map(|(tail, _)| tail)
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    pub fn forget(&mut self, name: &str) {
        self.open.remove(name);
    }

    /// Forgets every tail of a worktree, whatever process it belonged to.
    /// A start replaces the log files, so what was read of them is gone.
    pub fn forget_worktree(&mut self, name: &str) {
        let prefix = format!("{name}/");
        self.open.retain(|key, _| !key.starts_with(&prefix));
    }

    /// The tail for `name`, opening it if it is not already, and marking it
    /// as the most recently used.
    pub fn touch(&mut self, name: &str, path: std::path::PathBuf) -> &mut LogTail {
        self.clock += 1;
        if !self.open.contains_key(name)
            && self.open.len() >= MAX_LOG_TAILS
            && let Some(coldest) = self
                .open
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(name, _)| name.clone())
        {
            self.open.remove(&coldest);
        }
        let clock = self.clock;
        let entry = self
            .open
            .entry(name.to_string())
            .or_insert_with(|| (LogTail::new(path, TAIL_CAPACITY), clock));
        entry.1 = clock;
        &mut entry.0
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
    /// Process state: what is running, on which ports, and why anything
    /// failed. Replaced wholesale by each refresh.
    pub state: State,
    /// One refresh at a time. A scan of listening sockets can take a moment,
    /// and stacking them up behind a slow one helps nobody.
    pub refreshing: bool,
    /// Open log tails, keyed by worktree, for the detail pane.
    pub log_tails: LogTails,
    /// Which of the selected worktree's processes the tail is showing, by
    /// index into its process list. Clamped on every read, because the
    /// list changes under it as processes start and stop.
    pub tail_index: usize,
    /// Whether the detail pane's tail has the arrow keys.
    pub tail_focus: bool,
    /// Lines scrolled back from the end of the tail.
    pub tail_scroll: usize,
    /// The last ownership warning shown, so a standing one is reported when
    /// it appears or changes rather than on every refresh.
    pub state_warning: Option<String>,
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
    /// The URL a test asked to open, instead of handing it to the desktop.
    #[cfg(test)]
    pub opened: Option<String>,
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
            state: State::new(),
            refreshing: false,
            log_tails: LogTails::default(),
            tail_index: 0,
            tail_focus: false,
            tail_scroll: 0,
            state_warning: None,
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
            #[cfg(test)]
            opened: None,
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
                let grew = self.poll_log_tail();
                if self.tick.is_multiple_of(SLOW_TICK_EVERY) {
                    self.spawn_discovery();
                } else if self.tick.is_multiple_of(REFRESH_EVERY) {
                    self.spawn_refresh();
                }
                spinning || grew || self.expire_status()
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
            AppEvent::Refreshed(result) => {
                self.refreshing = false;
                match *result {
                    Ok(state) => {
                        let changed = state != self.state;
                        self.state = state;
                        changed
                    }
                    Err(e) => {
                        self.set_error(format!("refresh failed: {e}"));
                        true
                    }
                }
            }
            AppEvent::ConfigResolved(config) => {
                // The one place the session's config changes while it runs.
                // Without it the modal reopens on the next `s`, with the
                // rule's first candidate preselected rather than the answer
                // just given.
                self.config = *config;
                false
            }
            AppEvent::AskQuestion(boxed) => {
                let (question, reply) = *boxed;
                let selected = question.preselect.unwrap_or(0);
                // No candidates to choose between means the answer can only
                // be typed, so the input line opens straight away.
                let custom = question.options.is_empty().then(String::new);
                self.modal = Some(Modal::Question {
                    question,
                    selected,
                    custom,
                    reply,
                });
                true
            }
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
            Some(Modal::Question {
                question,
                selected,
                custom,
                reply,
            }) => {
                self.handle_question_key(key, question, selected, custom, reply);
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
            KeyCode::Char('s') | KeyCode::Enter => self.start_selected(),
            KeyCode::Char('x') => self.stop_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('o') => self.open_selected_url(),
            KeyCode::Char('l') => self.toggle_tail_focus(),
            KeyCode::Tab => self.cycle_tail(),
            KeyCode::Char('R') => {
                self.spawn_discovery();
                self.set_status("refreshing…");
            }
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.modal = Some(Modal::Help);
            }
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

    // ---- processes -------------------------------------------------------

    /// The state record for a worktree, when there is one.
    pub fn record_for(&self, name: &str) -> Option<&WorktreeRecord> {
        self.state.worktrees.get(name)
    }

    /// The phase of a whole worktree: failed if any of its processes is,
    /// then starting, then running. One row, one answer, however many
    /// processes it runs.
    pub fn phase_of(&self, name: &str) -> Option<Aggregate> {
        state::aggregate_phase(self.record_for(name)?)
    }

    /// The selected worktree's processes, in config order, with the log
    /// each one writes.
    pub fn processes_of(&self, name: &str) -> Vec<(String, ProcessRecord)> {
        self.record_for(name)
            .map(|record| {
                record
                    .processes
                    .iter()
                    .map(|(process, p)| (process.clone(), p.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The process whose log the tail is showing, and the key its tail is
    /// kept under. `None` when the selected worktree is running nothing.
    pub fn tail_target(&self) -> Option<(String, String, std::path::PathBuf)> {
        let name = self.selected_worktree().map(|w| w.name.clone())?;
        let processes = self.processes_of(&name);
        if processes.is_empty() {
            return None;
        }
        let (process, record) = &processes[self.tail_index.min(processes.len() - 1)];
        Some((
            format!("{name}/{process}"),
            process.clone(),
            record.log_path.clone(),
        ))
    }

    /// Moves the tail to the next process of the selected worktree.
    fn cycle_tail(&mut self) {
        let Some(name) = self.selected_worktree().map(|w| w.name.clone()) else {
            self.set_error("nothing selected");
            return;
        };
        let processes = self.processes_of(&name);
        if processes.len() < 2 {
            self.set_error(match processes.len() {
                0 => format!("{name} is running nothing to tail"),
                _ => format!("{name} runs one process: {}", processes[0].0),
            });
            return;
        }
        self.tail_index = (self.tail_index + 1) % processes.len();
        self.tail_scroll = 0;
        self.set_status(format!("log: {}", processes[self.tail_index].0));
    }

    /// The URL a worktree serves on.
    ///
    /// The one shared rule, so the row, the detail pane and `o` open the
    /// address `pando status` prints — including when a framework ignored
    /// the port it was given.
    pub fn url_of(&self, name: &str) -> Option<String> {
        actions::worktree_url(self.record_for(name)?)
    }

    fn selected_name(&mut self) -> Option<String> {
        match self.selected_worktree() {
            Some(wt) => Some(wt.name.clone()),
            None => {
                self.set_error("nothing selected");
                None
            }
        }
    }

    fn start_selected(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let paths = self.paths.clone();
        let config = self.config.clone();
        let worker_name = name.clone();
        let tx = self.event_tx.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(name, PendingKind::Start, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            // Detection may have a question; it goes back to the UI thread
            // and this worker waits for the answer.
            let ask = |question: &actions::Question| ask_through_ui(&tx, question);
            let config = actions::resolve_process(&paths, &config, &ask, &progress)
                .map_err(|e| format!("{e:#}"))?;
            // Back to the UI thread at once: an answer written to
            // `pando.toml` that this session's own copy does not have is
            // one the next keypress asks all over again.
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            actions::start(&paths, &config, &worker_name, None, &progress)
                .map(|report| PendingOutcome::Started(worker_name.clone(), report.url.clone()))
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
    }

    fn stop_selected(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let paths = self.paths.clone();
        let worker_name = name.clone();
        self.spawn_pending(name, PendingKind::Stop, move || {
            actions::stop(&paths, &worker_name, None)
                .map(|_| PendingOutcome::Stopped(worker_name))
                .map_err(|e| format!("{e:#}"))
        });
    }

    fn restart_selected(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let paths = self.paths.clone();
        let config = self.config.clone();
        let worker_name = name.clone();
        let tx = self.event_tx.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(name, PendingKind::Restart, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            let ask = |question: &actions::Question| ask_through_ui(&tx, question);
            let config = actions::resolve_process(&paths, &config, &ask, &progress)
                .map_err(|e| format!("{e:#}"))?;
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            actions::restart(&paths, &config, &worker_name, None, &progress)
                .map(|report| PendingOutcome::Started(worker_name.clone(), report.url.clone()))
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
    }

    fn open_selected_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.url_of(&name) else {
            self.set_error(format!("{name} has no port yet — start it first"));
            return;
        };
        self.open_url(&url);
        self.set_status(format!("opened {url}"));
    }

    /// Hands a URL to the desktop, on a worker thread with every stream
    /// captured: a browser launcher that writes to the terminal would paint
    /// over the alternate screen.
    #[cfg(not(test))]
    fn open_url(&mut self, url: &str) {
        let url = url.to_string();
        thread::spawn(move || {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener)
                .arg(&url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
        });
    }

    #[cfg(test)]
    fn open_url(&mut self, url: &str) {
        self.opened = Some(url.to_string());
    }

    fn toggle_tail_focus(&mut self) {
        if self.selected_worktree().is_none() {
            self.set_error("nothing selected");
            return;
        }
        self.tail_focus = !self.tail_focus;
        self.tail_scroll = 0;
        if self.tail_focus {
            self.set_status("log: j/k scroll, l or esc to leave");
        }
    }

    fn scroll_tail(&mut self, delta: isize) {
        let lines = self
            .tail_target()
            .and_then(|(key, _, _)| self.log_tails.get(&key).map(|t| t.lines().len()))
            .unwrap_or(0);
        let max = lines.saturating_sub(1);
        // Negative is "up", which in a tail means further back.
        let next = self.tail_scroll as isize - delta;
        self.tail_scroll = next.clamp(0, max as isize) as usize;
    }

    /// Reads whatever the selected worktree's log has grown by. Cheap: an
    /// offset-based read of one file, nothing forked.
    fn poll_log_tail(&mut self) -> bool {
        let Some((key, _, path)) = self.tail_target() else {
            return false;
        };
        self.log_tails.touch(&key, path).poll().unwrap_or(false)
    }

    // ---- background work -------------------------------------------------
    fn handle_question_key(
        &mut self,
        key: KeyEvent,
        question: actions::Question,
        mut selected: usize,
        mut custom: Option<String>,
        reply: Sender<Result<actions::Answer, String>>,
    ) {
        // Whatever happens, the worker gets an answer: it is blocked on this
        // channel, and a modal that closes without sending would leave it
        // there forever.
        if let Some(text) = custom.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    // Back to the list of options, unless there were none.
                    if question.options.is_empty() {
                        let _ = reply.send(Err("cancelled".to_string()));
                        return;
                    }
                    custom = None;
                }
                KeyCode::Enter => {
                    let answer = text.trim().to_string();
                    if answer.is_empty() {
                        self.set_error("type a command, or esc to go back");
                    } else {
                        let _ = reply.send(Ok(actions::Answer::Custom(answer)));
                        return;
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    let _ = reply.send(Err("cancelled".to_string()));
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(question.options.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('c') => custom = Some(String::new()),
                KeyCode::Enter => {
                    if question.options.is_empty() {
                        custom = Some(String::new());
                    } else {
                        let _ = reply.send(Ok(actions::Answer::Choice(selected)));
                        return;
                    }
                }
                _ => {}
            }
        }
        self.modal = Some(Modal::Question {
            question,
            selected,
            custom,
            reply,
        });
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
        // The tail belongs to a row, so moving off it starts at the end of
        // the next one's log rather than wherever the last one was
        // scrolled — and at that worktree's first process, not at whatever
        // index the last one happened to be showing.
        if self.tail_focus {
            self.scroll_tail(delta);
            return;
        }
        self.tail_scroll = 0;
        self.tail_index = 0;
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
        let keep = self.selected_worktree().map(|w| w.name.clone());
        self.refilter_keeping(keep);
    }

    /// `keep` is the worktree the cursor was on, passed in rather than read
    /// here: `apply_snapshot` has already replaced the list by the time it
    /// refilters, and resolving the old row against the new list would pick
    /// whichever worktree now happens to sit at that index.
    fn refilter_keeping(&mut self, keep: Option<String>) {
        let needle = self.filter.to_lowercase();
        let keep_name = keep;
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
        self.refilter_keeping(keep);
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

    /// Advances phases, captures observed ports, and saves — off the UI
    /// thread, because it takes the state lock and scans listening sockets.
    pub fn spawn_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let paths = self.paths.clone();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let refreshed = actions::refresh(&paths);
            let result = match refreshed.warning {
                Some(warning) => Err(warning),
                None => Ok(refreshed.state),
            };
            let _ = tx.send(AppEvent::Refreshed(Box::new(result)));
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
            state: State::new(),
            refreshing: false,
            log_tails: LogTails::default(),
            tail_index: 0,
            tail_focus: false,
            tail_scroll: 0,
            state_warning: None,
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
            opened: None,
        };
        app.refilter();
        app
    }
}

/// Sends a question to the UI thread and waits for the answer.
///
/// Called from a worker, never from the UI thread. A dropped receiver — the
/// app quitting while the modal is open — comes back as an error, so the
/// worker unwinds instead of waiting for an answer nobody will give.
fn ask_through_ui(tx: &Sender<AppEvent>, question: &actions::Question) -> Result<actions::Answer> {
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
    use crate::state::Phase;
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
        // A path that cannot exist: these tests drive the app's own logic,
        // and a worker thread that wandered into a real repository would be
        // exactly the thing the testing policy forbids.
        let paths = PandoPaths::new(
            "/pando-test-does-not-exist/home",
            ProjectRef {
                id: "acme-shop-3f9a2c1d".into(),
                root: PathBuf::from("/pando-test-does-not-exist/acme-shop"),
                display_name: "acme-shop".into(),
            },
        );
        let worktrees: Vec<Worktree> = names.iter().map(|n| wt(n)).collect();
        let mut app = App::new_for_test(paths, Config::default(), worktrees);
        app.created_by_pando = names.iter().map(|n| (n.to_string(), true)).collect();
        app
    }

    use crate::state::{ProcessRecord, WorktreeRecord};
    use chrono::Utc;

    /// Gives a worktree a process in `phase`, as a refresh would have.
    pub fn with_process(app: &mut App, name: &str, phase: Phase) {
        let mut record = WorktreeRecord::new(format!("/trees/{name}"), true);
        record.ports.insert("web".to_string(), 17_342);
        record
            .roles
            .insert("dev".to_string(), vec!["web".to_string()]);
        record.processes.insert(
            "dev".to_string(),
            ProcessRecord {
                pid: 4242,
                pgid: 4242,
                started_at: Utc::now(),
                log_path: PathBuf::from("/does/not/exist/dev.log"),
                ready_port: Some(17_342),
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                phase,
            },
        );
        app.state.worktrees.insert(name.to_string(), record);
    }

    /// Gives a worktree a second process, as a workspace start would have.
    pub fn with_second_process(app: &mut App, name: &str, process: &str, phase: Phase) {
        let record = app
            .state
            .worktrees
            .get_mut(name)
            .expect("the worktree has a record");
        let port = 17_343 + record.processes.len() as u16;
        record.ports.insert(process.to_string(), port);
        record
            .roles
            .insert(process.to_string(), vec![process.to_string()]);
        record.processes.insert(
            process.to_string(),
            ProcessRecord {
                pid: 4343,
                pgid: 4343,
                started_at: Utc::now(),
                log_path: PathBuf::from(format!("/does/not/exist/{process}.log")),
                ready_port: Some(port),
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                phase,
            },
        );
    }

    pub fn running_phase() -> Phase {
        Phase::Running { since: Utc::now() }
    }

    fn a_question() -> actions::Question {
        actions::Question {
            slot: crate::detect::Slot::DevCmd,
            prompt: "Which command starts the local development server?".to_string(),
            options: vec![
                (
                    "pnpm dev".to_string(),
                    "package.json scripts.dev".to_string(),
                ),
                (
                    "pnpm dev:web".to_string(),
                    "package.json scripts.dev:web".to_string(),
                ),
            ],
            preselect: Some(0),
            allow_custom: true,
            allow_none: false,
        }
    }

    /// Opens the question modal the way a worker would, and hands back the
    /// end of the channel that worker would be blocked on.
    fn open_question(
        app: &mut App,
        question: actions::Question,
    ) -> Receiver<Result<actions::Answer, String>> {
        let (tx, rx) = mpsc::channel();
        app.handle_event(AppEvent::AskQuestion(Box::new((question, tx))));
        rx
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
            state: State::new(),
            warning: None,
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
            state: State::new(),
            warning: None,
            default_base: None,
        });
        assert_eq!(fresh, vec!["feat+one"]);
    }

    // The ownership map is what the Remove modal's "pando did not create
    // this" warning reads, so a state file the refresh could not use has to
    // reach the user rather than turning every row silently adopted.
    #[test]
    fn a_state_warning_from_a_refresh_reaches_the_status_line() {
        let mut app = test_app(&["feat+one"]);
        let snapshot = |warning: Option<&str>| Snapshot {
            main: wt("acme-shop"),
            worktrees: vec![wt("feat+one")],
            created_by_pando: BTreeMap::new(),
            state: State::new(),
            warning: warning.map(str::to_string),
            default_base: None,
        };

        app.apply_snapshot(snapshot(Some("state file /s is version 3")));
        let (message, is_error) = app.active_status().unwrap();
        assert!(message.contains("version 3"), "{message}");
        assert!(is_error, "a state file pando cannot use is an error");

        // A standing warning is not re-announced on every refresh.
        app.status = None;
        app.apply_snapshot(snapshot(Some("state file /s is version 3")));
        assert!(app.active_status().is_none());

        app.apply_snapshot(snapshot(None));
        assert_eq!(app.state_warning, None);
    }

    // The cursor follows the worktree, not the row it happened to be on:
    // a refresh that reorders the list (a new worktree is newest-first)
    // must not move the selection to a different one.
    #[test]
    fn a_refresh_keeps_the_cursor_on_the_same_worktree_when_the_order_changes() {
        let mut app = test_app(&["feat+one", "feat+two", "fix+three"]);
        app.select_index(2);
        assert_eq!(app.selected_worktree().unwrap().name, "fix+three");

        app.apply_snapshot(Snapshot {
            main: wt("acme-shop"),
            worktrees: vec![wt("fix+three"), wt("feat+one"), wt("feat+two")],
            created_by_pando: BTreeMap::new(),
            state: State::new(),
            warning: None,
            default_base: None,
        });
        assert_eq!(
            app.selected_worktree().unwrap().name,
            "fix+three",
            "the cursor must stay on the worktree it was on"
        );
        assert_eq!(app.list_state.selected(), Some(0));
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
    // ---- processes -------------------------------------------------------

    #[test]
    fn the_process_keys_each_start_their_own_work() {
        for (key, kind) in [
            (KeyCode::Char('s'), PendingKind::Start),
            (KeyCode::Char('x'), PendingKind::Stop),
            (KeyCode::Char('r'), PendingKind::Restart),
        ] {
            let mut app = test_app(&["feat+one"]);
            press(&mut app, key);
            let pending = app.pending.as_ref().expect("the key started something");
            assert_eq!(pending.kind, kind, "{key:?}");
            assert_eq!(pending.name, "feat+one");
            // And the work is on a worker thread, not this one: the frame
            // is still answering keys.
            assert!(!app.should_quit);
        }
    }

    // The answers a session writes have to be in that session's own copy of
    // the config. They were written to `pando.toml` and nowhere else, so
    // the next `s` re-ran detection against a config that still had
    // nothing and asked the same question again — with the rule's first
    // candidate preselected rather than the answer just given.
    #[test]
    fn what_a_start_resolves_is_applied_to_this_session_in_memory() {
        // A real repository, because detection reads one. Nothing is
        // started: the worktree in the list does not exist, so the worker
        // resolves, sends the config, and then fails to find it — which is
        // exactly the case the config must survive.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        crate::testutil::init_repo(&root);
        std::fs::write(
            root.join("package.json"),
            "{\n  \"name\": \"x\",\n  \"scripts\": { \"dev\": \"next dev\" }\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(root.join(".env.example"), "PORT=3000\n").unwrap();
        let paths = PandoPaths::new(
            dir.path().join("pando-home"),
            crate::project::ProjectRef::from_root(&root).unwrap(),
        );
        let mut app = App::new_for_test(paths, Config::default(), vec![wt("feat+one")]);
        assert!(app.config.processes.is_empty(), "nothing is known yet");

        press(&mut app, KeyCode::Char('s'));
        let rx = app.event_rx.take().expect("the app owns its receiver");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut applied = false;
        while Instant::now() < deadline && !applied {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(event) => {
                    let is_config = matches!(event, AppEvent::ConfigResolved(_));
                    app.handle_event(event);
                    applied = is_config;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        app.event_rx = Some(rx);

        assert!(applied, "the worker never sent what it resolved");
        assert_eq!(
            app.config.processes["dev"].cmd, "pnpm dev",
            "the session knows what it just answered, so the next `s` asks nothing"
        );
        assert_eq!(app.config.processes["dev"].roles(), vec!["web"]);
    }

    #[test]
    fn enter_starts_the_selected_worktree() {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.pending.as_ref().map(|p| p.kind),
            Some(PendingKind::Start)
        );
    }

    #[test]
    fn a_process_key_with_nothing_selected_says_so() {
        let mut app = test_app(&[]);
        press(&mut app, KeyCode::Char('s'));
        assert!(app.pending.is_none());
        assert_eq!(
            app.active_status().map(|(m, _)| m),
            Some("nothing selected")
        );
    }

    #[test]
    fn open_needs_a_port_before_it_has_a_url() {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, KeyCode::Char('o'));
        assert_eq!(app.opened, None);
        let (message, is_error) = app.active_status().unwrap();
        assert!(message.contains("no port yet"), "{message}");
        assert!(is_error);

        with_process(&mut app, "feat+one", running_phase());
        press(&mut app, KeyCode::Char('o'));
        assert_eq!(app.opened.as_deref(), Some("http://localhost:17342"));
    }

    // Phase 2b review, finding 6. `url_of` was a third implementation of
    // the URL rule: the alphabetically first *role* rather than the first
    // role of the first process, and it never looked at what the process
    // was really listening on. `o` opened a different address from the one
    // `pando status` had just printed.
    #[test]
    fn open_uses_the_same_url_rule_as_status() {
        let mut app = test_app(&["feat+url2"]);
        let mut state = State::new();
        let mut record = WorktreeRecord::new("/trees/feat+url2", true);
        record.ports.insert("srv".to_string(), 19_056);
        record.ports.insert("admin".to_string(), 19_057);
        record
            .roles
            .insert("alpha".to_string(), vec!["srv".to_string()]);
        record
            .roles
            .insert("beta".to_string(), vec!["admin".to_string()]);
        // And `alpha` ignored the port it was given, which the TUI never
        // noticed at all.
        record.processes.insert(
            "alpha".to_string(),
            ProcessRecord {
                pid: 4242,
                pgid: 4242,
                started_at: Utc::now(),
                log_path: PathBuf::from("/does/not/exist/alpha.log"),
                ready_port: Some(19_056),
                ready_timeout_s: None,
                observed_ports: vec![3000],
                phase: running_phase(),
            },
        );
        record.observed_ports = vec![3000];
        state.worktrees.insert("feat+url2".to_string(), record);
        app.handle_event(AppEvent::Refreshed(Box::new(Ok(state.clone()))));

        assert_eq!(
            app.url_of("feat+url2"),
            crate::actions::worktree_url(&state.worktrees["feat+url2"]),
            "one rule, wherever it is asked"
        );
        press(&mut app, KeyCode::Char('o'));
        assert_eq!(app.opened.as_deref(), Some("http://localhost:3000"));
    }

    #[test]
    fn a_refresh_replaces_the_process_state() {
        let mut app = test_app(&["feat+one"]);
        let mut state = State::new();
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record.ports.insert("web".to_string(), 17_342);
        state.worktrees.insert("feat+one".to_string(), record);
        assert!(app.handle_event(AppEvent::Refreshed(Box::new(Ok(state)))));
        assert_eq!(
            app.url_of("feat+one").as_deref(),
            Some("http://localhost:17342")
        );
        assert!(!app.refreshing, "the single-flight slot is free again");
    }

    #[test]
    fn a_refresh_that_failed_reaches_the_status_line() {
        let mut app = test_app(&["feat+one"]);
        app.handle_event(AppEvent::Refreshed(Box::new(Err("state is v3".into()))));
        let (message, is_error) = app.active_status().unwrap();
        assert!(message.contains("state is v3"), "{message}");
        assert!(is_error);
    }

    // ---- the question modal ----------------------------------------------

    #[test]
    fn a_question_opens_a_modal_with_the_recommendation_preselected() {
        let mut app = test_app(&["feat+one"]);
        let _rx = open_question(&mut app, a_question());
        match app.modal.as_ref() {
            Some(Modal::Question {
                question,
                selected,
                custom,
                ..
            }) => {
                assert_eq!(*selected, 0, "the rules' own pick is preselected");
                assert!(custom.is_none());
                assert_eq!(question.options.len(), 2);
            }
            other => panic!("expected a question modal, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn choosing_an_option_answers_the_worker_and_closes_the_modal() {
        let mut app = test_app(&["feat+one"]);
        let rx = open_question(&mut app, a_question());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), Ok(actions::Answer::Choice(1)));
        assert!(app.modal.is_none(), "the modal closes once it is answered");
    }

    // Every slot accepts a shell command, so there is never a dead end.
    #[test]
    fn a_typed_command_is_sent_as_written() {
        let mut app = test_app(&["feat+one"]);
        let rx = open_question(&mut app, a_question());
        press(&mut app, KeyCode::Char('c'));
        type_str(&mut app, "./serve.sh");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Ok(actions::Answer::Custom("./serve.s".to_string()))
        );
        assert!(app.modal.is_none());
    }

    // The worker is blocked on the reply channel. A modal that closed
    // without sending would leave it there for the life of the process.
    #[test]
    fn cancelling_a_question_always_tells_the_worker() {
        let mut app = test_app(&["feat+one"]);
        let rx = open_question(&mut app, a_question());
        press(&mut app, KeyCode::Esc);
        assert_eq!(rx.try_recv().unwrap(), Err("cancelled".to_string()));
        assert!(app.modal.is_none());
    }

    #[test]
    fn escape_from_the_typing_line_goes_back_to_the_options() {
        let mut app = test_app(&["feat+one"]);
        let rx = open_question(&mut app, a_question());
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(
            app.modal.as_ref(),
            Some(Modal::Question { custom: None, .. })
        ));
        assert!(rx.try_recv().is_err(), "nothing was answered yet");
        press(&mut app, KeyCode::Esc);
        assert_eq!(rx.try_recv().unwrap(), Err("cancelled".to_string()));
    }

    #[test]
    fn a_question_with_no_options_opens_straight_into_typing() {
        let mut app = test_app(&["feat+one"]);
        let mut question = a_question();
        question.options.clear();
        question.preselect = None;
        let rx = open_question(&mut app, question);
        assert!(matches!(
            app.modal.as_ref(),
            Some(Modal::Question {
                custom: Some(_),
                ..
            })
        ));
        type_str(&mut app, "node server.js");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Ok(actions::Answer::Custom("node server.js".to_string()))
        );
    }

    #[test]
    fn an_empty_typed_answer_is_refused_rather_than_sent() {
        let mut app = test_app(&["feat+one"]);
        let rx = open_question(&mut app, a_question());
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Enter);
        assert!(rx.try_recv().is_err());
        assert!(app.modal.is_some());
        assert!(app.active_status().unwrap().0.contains("type a command"));
    }

    // ---- several processes -----------------------------------------------

    #[test]
    fn tab_switches_the_tail_between_processes() {
        let mut app = test_app(&["feat+one"]);
        with_process(&mut app, "feat+one", running_phase());
        with_second_process(&mut app, "feat+one", "api", running_phase());

        let (_, first, _) = app.tail_target().expect("a process to tail");
        assert_eq!(first, "api", "config order, which is the row order");

        press(&mut app, KeyCode::Tab);
        let (key_of, second, path) = app.tail_target().expect("a process to tail");
        assert_eq!(second, "dev");
        assert_eq!(
            key_of, "feat+one/dev",
            "one tail per process, not per worktree"
        );
        assert!(path.ends_with("dev.log"));

        // And round it goes.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tail_target().unwrap().1, "api");
    }

    #[test]
    fn tab_on_a_worktree_with_one_process_says_so_rather_than_cycling() {
        let mut app = test_app(&["feat+one"]);
        with_process(&mut app, "feat+one", running_phase());
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tail_target().unwrap().1, "dev");
        let (message, is_error) = app.active_status().expect("something was said");
        assert!(message.contains("one process"), "{message}");
        assert!(is_error);
    }

    #[test]
    fn moving_to_another_worktree_starts_at_its_first_process() {
        let mut app = test_app(&["feat+one", "feat+two"]);
        with_process(&mut app, "feat+one", running_phase());
        with_second_process(&mut app, "feat+one", "api", running_phase());
        with_process(&mut app, "feat+two", running_phase());

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tail_target().unwrap().1, "dev");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.tail_target().unwrap().0, "feat+two/dev");
        assert_eq!(app.tail_index, 0, "the index belongs to the row");
    }

    // ---- the log tail ----------------------------------------------------

    #[test]
    fn the_tail_lru_keeps_the_newest_and_drops_the_coldest() {
        let mut tails = LogTails::default();
        for i in 0..MAX_LOG_TAILS + 2 {
            tails.touch(&format!("w{i}"), PathBuf::from("/does/not/exist.log"));
        }
        assert_eq!(tails.len(), MAX_LOG_TAILS);
        assert!(tails.get("w0").is_none(), "the coldest went");
        assert!(tails.get(&format!("w{}", MAX_LOG_TAILS + 1)).is_some());
    }

    #[test]
    fn the_selected_tail_never_evicts_itself() {
        let mut tails = LogTails::default();
        tails.touch("keep", PathBuf::from("/does/not/exist.log"));
        for i in 0..MAX_LOG_TAILS + 4 {
            tails.touch("keep", PathBuf::from("/does/not/exist.log"));
            tails.touch(&format!("w{i}"), PathBuf::from("/does/not/exist.log"));
        }
        assert!(tails.get("keep").is_some());
    }

    #[test]
    fn focusing_the_log_moves_the_arrow_keys_onto_it() {
        let mut app = test_app(&["feat+one", "feat+two"]);
        press(&mut app, KeyCode::Char('l'));
        assert!(app.tail_focus);
        let before = app.list_state.selected();
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.list_state.selected(), before, "the cursor stays put");
        press(&mut app, KeyCode::Char('l'));
        assert!(!app.tail_focus);
        press(&mut app, KeyCode::Char('j'));
        assert_ne!(app.list_state.selected(), before);
    }

    #[test]
    fn moving_the_cursor_returns_the_tail_to_the_end() {
        let mut app = test_app(&["feat+one", "feat+two"]);
        app.tail_scroll = 12;
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.tail_scroll, 0);
    }
}
