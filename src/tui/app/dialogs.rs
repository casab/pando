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
    },
    Remove {
        name: String,
        /// Why this removal would be refused, if it would be.
        blocker: Option<RemoveBlocker>,
        /// Whether pando created it; drives the extra warning line.
        created_by_pando: bool,
    },
    /// Confirming that a public URL goes away. Somebody may be looking at
    /// it right now, so it is not a bare keypress.
    Unshare {
        name: String,
        url: String,
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

impl App {
    pub(super) fn handle_create_key(
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

    pub(super) fn handle_remove_key(
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

    pub(super) fn open_create(&mut self) {
        self.modal = Some(Modal::Create {
            input: String::new(),
            branches: BranchLoadState::Loading,
            selected: 0,
        });
        self.spawn_branch_fetch();
    }

    pub(super) fn open_remove(&mut self) {
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
}
