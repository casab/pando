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
//!
//! `App` and its core key dispatch live here; the rest of its methods are
//! split by concern across the submodules, each adding its own `impl App`.

mod background;
mod dialogs;
mod log_keys;
mod log_view;
mod operations;
mod pending;
mod tails;

pub use background::{AppEvent, Snapshot, snapshot};
pub use dialogs::{BranchLoadState, CreateRow, Modal, RemoveBlocker, create_rows};
pub use log_view::{LOG_VIEWER_CAPACITY, LineInspect, LogFilter, LogView, SearchMode, SearchState};
pub use pending::{PendingAction, PendingKind, PendingOutcome};
pub use tails::LogTails;

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::actions;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state::{self, Aggregate, ProcessRecord, State, WorktreeRecord};
use crate::worktree::{self, PrInfo, Worktree};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
}

/// What the whole screen is showing: the list with its detail pane, or one
/// worktree's log full screen.
pub enum View {
    List,
    Log(Box<LogView>),
}

/// What the last probe said about the project's services: the shared ones
/// the header chips show, and each worktree's private ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceHealth {
    pub shared: Vec<actions::ServiceStatus>,
    pub worktrees: BTreeMap<String, Vec<actions::ServiceStatus>>,
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
    /// Whether the project's services are answering. Computed on the
    /// refresh worker, never in a paint or a key handler.
    pub service_health: ServiceHealth,
    /// Which options of an open multi-select question are ticked. Beside
    /// the modal rather than inside it, so the modal stays the plain
    /// `Clone` value the renderer takes.
    pub question_checked: Vec<usize>,
    /// Open log tails, keyed by worktree, for the detail pane.
    pub log_tails: LogTails,
    /// Which of the selected worktree's processes the tail is showing, by
    /// index into its process list. Clamped on every read, because the
    /// list changes under it as processes start and stop.
    pub tail_index: usize,
    /// Lines scrolled back from the end of the tail.
    pub tail_scroll: usize,
    /// How many rows the detail pane last gave the tail, so PgUp/PgDn move
    /// by a screenful of whatever is actually on screen.
    pub tail_rows: usize,
    /// The list, or one worktree's log full screen.
    pub view: View,
    /// The pretty-print overlay for one line of the open log (`J`).
    pub inspect: Option<LineInspect>,
    /// Rows the viewer's body last had. Set by the paint, read by the key
    /// handler, so a half-page is half of what is really on screen.
    pub viewer_height: usize,
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
            service_health: ServiceHealth::default(),
            question_checked: Vec::new(),
            refreshing: false,
            log_tails: LogTails::default(),
            tail_index: 0,
            tail_scroll: 0,
            tail_rows: 0,
            view: View::List,
            inspect: None,
            viewer_height: 0,
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
                let grew = self.poll_logs();
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
            AppEvent::ServiceHealth(health) => {
                let changed = *health != self.service_health;
                self.service_health = *health;
                changed
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
                // The rules' own answer, ticked, so enter accepts it.
                self.question_checked = question.checked.clone();
                // No candidates to choose between means the answer can only
                // be typed, so the input line opens straight away. Never
                // for a set question, which has nothing to type.
                let custom = (question.options.is_empty() && !question.multi).then(String::new);
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
            Some(Modal::Unshare { name, url }) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => self.unshare_selected(name),
                    // Anything else leaves it up, and the modal with it
                    // closed — the same shape the remove confirmation has.
                    KeyCode::Esc | KeyCode::Char('n') => {}
                    _ => self.modal = Some(Modal::Unshare { name, url }),
                }
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
        // The viewer owns the whole screen and the whole keyboard, so it
        // comes before the list's keys — but after the modals, because a
        // worker can still raise a question while it is open.
        if matches!(self.view, View::Log(_)) {
            self.handle_log_key(key);
            return;
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
            // The isolated start. Its own key rather than a mode, because
            // isolation is remembered on the worktree: pressing it once is
            // what turns it on, and `s` from then on keeps it.
            KeyCode::Char('i') => self.start_selected_isolated(),
            KeyCode::Char('x') => self.stop_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('o') => self.open_selected_url(),
            // Shift-O, because the public one is the URL you give away and
            // `o` is the one you open fifty times a day.
            KeyCode::Char('O') => self.open_selected_public_url(),
            KeyCode::Char('t') => self.toggle_share(),
            KeyCode::Char('l') | KeyCode::Char('L') => self.open_log_viewer(),
            // PgUp is older, PgDn is newer — j/k stay on the list.
            KeyCode::PageUp => self.scroll_tail(-(self.tail_rows.max(1) as isize)),
            KeyCode::PageDown => self.scroll_tail(self.tail_rows.max(1) as isize),
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

    /// One worktree's private services as the last probe found them.
    ///
    /// From the probe rather than from the record, because "is it up" is
    /// the one thing the record cannot say — and asking here would put a
    /// TCP connect inside a paint.
    pub fn services_of(&self, name: &str) -> Vec<actions::ServiceStatus> {
        self.service_health
            .worktrees
            .get(name)
            .cloned()
            .unwrap_or_default()
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
            service_health: ServiceHealth::default(),
            question_checked: Vec::new(),
            refreshing: false,
            log_tails: LogTails::default(),
            tail_index: 0,
            tail_scroll: 0,
            tail_rows: 0,
            view: View::List,
            inspect: None,
            viewer_height: 0,
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

#[cfg(test)]
pub mod tests;
