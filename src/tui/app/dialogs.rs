//! The modals: create, remove, and the question a worker asks.

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::sync::mpsc::Sender;

use crate::actions;
use crate::worktree::BranchEntry;

use super::App;

#[derive(Debug, Clone)]
pub enum Modal {
    Create {
        input: String,
        branches: BranchLoadState,
        selected: usize,
        /// What a new branch forks from, when it is not the default base.
        /// Tab walks the choices.
        base: Option<String>,
    },
    /// What would stop the removal, or what it would take with it, is not
    /// kept here: it is read off the app at every paint and every key, so
    /// a git re-read that lands while the dialog is open is what it shows.
    Remove {
        name: String,
        /// Whether pando created it; drives the extra warning line.
        created_by_pando: bool,
    },
    /// Confirming that a public URL goes away. Somebody may be looking at
    /// it right now, so it is not a bare keypress.
    Unshare {
        name: String,
        url: String,
    },
    /// `X`: every worktree with anything up, stopped at once, as
    /// `pando stop --all` does. Lists what goes down before it does.
    StopAll {
        names: Vec<String>,
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
    /// What pando has said this session, in full and newest first: the
    /// header has one row, and a long error does not fit in it.
    Messages,
}

/// Stated before the user confirms, rather than discovered after: what
/// would refuse the removal, and what it would take down with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveBlocker {
    Locked(Option<String>),
    /// Uncommitted changes or untracked files: git refuses without force,
    /// and with it they are gone.
    Dirty,
    /// Git has not been read yet, so nobody knows whether it is dirty.
    DirtyUnknown,
    /// Something runs in it, which the removal stops first.
    Running,
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
                "it has uncommitted changes or untracked files — F removes it and discards them"
                    .to_string()
            }
            RemoveBlocker::DirtyUnknown => {
                "git has not been read yet — uncommitted changes, if any, stop y; F removes anyway"
                    .to_string()
            }
            RemoveBlocker::Running => "it is running — removing stops it first".to_string(),
            RemoveBlocker::NotOurs => {
                "pando did not create this worktree — confirming removes it anyway".to_string()
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

/// What `n` may fork a new branch from: the default base first, then every
/// branch the picker read, each once.
pub fn base_choices(default_base: Option<&str>, branches: &[BranchEntry]) -> Vec<String> {
    let mut choices: Vec<String> = default_base.map(str::to_string).into_iter().collect();
    for entry in branches {
        let name = match entry.source {
            crate::worktree::BranchSource::Local => entry.name.clone(),
            crate::worktree::BranchSource::Remote => format!("origin/{}", entry.name),
        };
        if !choices.contains(&name) {
            choices.push(name);
        }
    }
    choices
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

impl App {
    pub(super) fn handle_create_key(
        &mut self,
        key: KeyEvent,
        mut input: String,
        branches: BranchLoadState,
        mut selected: usize,
        mut base: Option<String>,
    ) {
        let rows = create_rows(&input, branches.as_slice());
        match key.code {
            KeyCode::Esc => return,
            // The base a new branch forks from, walked in place: the
            // default first, then every branch the picker read.
            KeyCode::Tab | KeyCode::BackTab => {
                let choices = base_choices(self.default_base.as_deref(), branches.as_slice());
                if !choices.is_empty() {
                    let current = base
                        .as_ref()
                        .or(self.default_base.as_ref())
                        .and_then(|b| choices.iter().position(|c| c == b))
                        .unwrap_or(0);
                    let step: isize = if key.code == KeyCode::Tab { 1 } else { -1 };
                    let next = (current as isize + step).rem_euclid(choices.len() as isize);
                    let chosen = choices[next as usize].clone();
                    base = (Some(&chosen) != self.default_base.as_ref()).then_some(chosen);
                }
            }
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
                        base,
                    });
                    return;
                };
                let (branch, new_branch) = match row {
                    CreateRow::NewBranch(name) => (name.clone(), true),
                    CreateRow::Existing(entry) => (entry.name.clone(), false),
                };
                // Git keeps one checkout per branch, and the main checkout
                // has this one. Said here, not by a worker that fails.
                if self.is_main_branch(&branch) {
                    self.set_error(format!(
                        "{branch} is checked out in the main checkout — pick another branch, \
                         or type a new name to fork one from it"
                    ));
                    self.modal = Some(Modal::Create {
                        input,
                        branches,
                        selected,
                        base,
                    });
                    return;
                }
                // The picker already said this one has a worktree; enter
                // goes to it, which is what somebody typing its name wants.
                if let Some(existing) = self.worktree_for_branch(&branch) {
                    self.filter.clear();
                    self.mode = super::Mode::Normal;
                    self.refilter_keeping(Some(existing));
                    self.tail_index = 0;
                    self.tail_scroll = 0;
                    self.set_status(format!("{branch} already has a worktree — selected it"));
                    return;
                }
                // The base only means something to a branch being made.
                let fork_from = if new_branch { base.clone() } else { None };
                if self.spawn_create(branch, fork_from) {
                    return; // modal closes only once the work is under way
                }
                self.modal = Some(Modal::Create {
                    input,
                    branches,
                    selected,
                    base,
                });
                return;
            }
            _ => {}
        }
        self.modal = Some(Modal::Create {
            input,
            branches,
            selected,
            base,
        });
    }

    /// Whether `branch` is the one the main checkout has.
    pub fn is_main_branch(&self, branch: &str) -> bool {
        self.main
            .as_ref()
            .and_then(|m| m.branch.as_deref())
            .is_some_and(|main| main == branch)
    }

    /// `y` removes what git would let go of; `F` removes it whatever it
    /// holds — `rm --force`. Anything else closes the dialog.
    pub(super) fn handle_remove_key(
        &mut self,
        key: KeyEvent,
        name: String,
        created_by_pando: bool,
    ) {
        let force = match key.code {
            KeyCode::Char('y') | KeyCode::Enter => false,
            KeyCode::Char('F') => true,
            _ => return,
        };
        let blockers = self.remove_blockers(&name);
        if let Some(fatal) = blockers.iter().find(|b| b.is_fatal()) {
            let label = self.label_of(&name);
            self.set_error(format!("{label}: {}", fatal.line()));
            return;
        }
        // Known to be dirty: git would refuse, so the dialog stays and says
        // which key does it.
        if !force && blockers.contains(&RemoveBlocker::Dirty) {
            let label = self.label_of(&name);
            self.set_error_about(
                &name,
                format!("{label} has uncommitted changes — F removes it anyway, esc keeps it"),
            );
            self.modal = Some(Modal::Remove {
                name,
                created_by_pando,
            });
            return;
        }
        self.spawn_remove(name, !created_by_pando, force);
    }

    /// Everything the remove dialog states up front, most serious first.
    pub fn remove_blockers(&self, name: &str) -> Vec<RemoveBlocker> {
        let mut out = Vec::new();
        let Some(wt) = self.worktrees.iter().find(|w| w.name == name) else {
            return out;
        };
        if wt.locked {
            out.push(RemoveBlocker::Locked(wt.lock_reason.clone()));
        }
        match wt.dirty {
            Some(true) => out.push(RemoveBlocker::Dirty),
            Some(false) => {}
            None => out.push(RemoveBlocker::DirtyUnknown),
        }
        if self.phase_of(name).is_some() {
            out.push(RemoveBlocker::Running);
        }
        if !self.created_by_pando.get(name).copied().unwrap_or(false) {
            out.push(RemoveBlocker::NotOurs);
        }
        out
    }

    // ---- background work -------------------------------------------------
    pub(super) fn handle_question_key(
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
        //
        // A set question is its own little mode: space ticks the row under
        // the cursor, enter takes whatever is ticked, and there is no
        // command to type in place of "which of these containers".
        if question.multi {
            let mut checked = std::mem::take(&mut self.question_checked);
            match key.code {
                KeyCode::Esc => {
                    let _ = reply.send(Err("cancelled".to_string()));
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(question.options.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char(' ') | KeyCode::Char('x') => {
                    if checked.contains(&selected) {
                        checked.retain(|i| *i != selected);
                    } else {
                        checked.push(selected);
                        checked.sort_unstable();
                    }
                }
                KeyCode::Char('n') => checked.clear(),
                KeyCode::Enter => {
                    let _ = reply.send(Ok(if checked.is_empty() {
                        actions::Answer::None
                    } else {
                        actions::Answer::Many(checked)
                    }));
                    return;
                }
                _ => {}
            }
            self.question_checked = checked;
            self.modal = Some(Modal::Question {
                question,
                selected,
                custom,
                reply,
            });
            return;
        }
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
                        self.set_error(format!(
                            "type the {}, or esc to go back",
                            question.slot.custom_noun()
                        ));
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
                // The CLI prompt's `n`: a slot that may have no answer —
                // no schema hook, no port variable — records that, the
                // same `Answer::None` the prompt sends.
                KeyCode::Char('n') if question.allow_none => {
                    let _ = reply.send(Ok(actions::Answer::None));
                    return;
                }
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

    /// The worktree a branch already has, by its branch or by the name a
    /// new one would be given — `new` refuses both.
    pub fn worktree_for_branch(&self, branch: &str) -> Option<String> {
        let dir_name = actions::sanitize_branch_to_dir(branch);
        self.worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(branch) || w.name == dir_name)
            .map(|w| w.name.clone())
    }

    pub(super) fn open_create(&mut self) {
        self.modal = Some(Modal::Create {
            input: String::new(),
            branches: BranchLoadState::Loading,
            selected: 0,
            base: None,
        });
        self.spawn_branch_fetch();
    }

    pub(super) fn open_remove(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            self.set_error("nothing selected");
            return;
        };
        let name = wt.name.clone();
        let created_by_pando = self.created_by_pando.get(&name).copied().unwrap_or(false);
        self.modal = Some(Modal::Remove {
            name: name.clone(),
            created_by_pando,
        });
        // What git said may be minutes old; the dialog is where it matters.
        // The answer repaints the dialog when it lands.
        self.refresh_git(Some(vec![name]));
    }
}
