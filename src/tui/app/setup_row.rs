//! The dashboard's setup line: where the project's setup stands, said in
//! one header row with the key that moves it on, and `a` and `v` from
//! the list.
//!
//! Kept current by the setup screen's own watch on the four files: a
//! changed config file is re-read on a worker and the setup read against
//! it, so an agent's `init --answers` and the check after it read as
//! ready, not as settings changed. That config is the line's alone: the
//! session starts worktrees on its own until it adopts one.

use chrono::{DateTime, Utc};
use std::time::Instant;

use crate::config::Config;
use crate::setup::{self, CheckOutcome, Setup, SetupState};

use super::App;
use super::setup::{CHECK_START_TIMEOUT, check_log_file, has_settings};

/// The setup as the dashboard last read it.
#[derive(Default)]
pub struct SetupRow {
    /// `None` until it is first read: the TUI reads it as it starts, and
    /// an app a test builds has none until the test asks.
    pub setup: Option<Setup>,
    /// A `v` whose check has not yet said anything, on both clocks.
    pub check_requested: Option<(Instant, DateTime<Utc>)>,
    /// The config as its files said at the last re-read, when one has run
    /// since the session last adopted a config: what the line is read
    /// against.
    pub files_config: Option<Config>,
}

/// What the header's setup line says, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupHint {
    /// A check runs: its latest line, if it has said one.
    Testing(Option<String>),
    /// A `v` whose check has not started yet.
    Starting,
    /// Anything else to say, and whether it is a failure.
    Note { text: String, failed: bool },
}

impl SetupRow {
    /// Whether a check runs or a `v` waits for one.
    pub(super) fn waiting_on_a_check(&self) -> bool {
        self.testing() || self.check_requested.is_some()
    }

    fn testing(&self) -> bool {
        self.setup
            .as_ref()
            .is_some_and(|s| s.state == SetupState::Testing)
    }

    /// The header's line, or none: a ready project, and a project not
    /// yet read, say nothing.
    pub fn hint(&self) -> Option<SetupHint> {
        let setup = self.setup.as_ref()?;
        let record = setup.last_check.as_ref();
        let note = |text: &str| {
            Some(SetupHint::Note {
                text: text.to_string(),
                failed: false,
            })
        };
        match setup.state {
            SetupState::Testing => Some(SetupHint::Testing(
                record.and_then(|r| r.progress.last().cloned()),
            )),
            _ if self.check_requested.is_some() => Some(SetupHint::Starting),
            SetupState::Ready => None,
            SetupState::Untested => note("not tested yet · v tests it"),
            SetupState::Stale => note("settings changed since the last test · v tests it"),
            SetupState::Interrupted => note("the last test was interrupted · v tests again"),
            SetupState::New { .. } => note("not set up · a copies the setup prompt"),
            SetupState::Failing => match record.map(|r| &r.outcome) {
                Some(CheckOutcome::NotSetUp { slot }) => {
                    note(&format!("not set up: {slot} is open · a copies the prompt"))
                }
                Some(CheckOutcome::Failed { reason, .. }) => Some(SetupHint::Note {
                    text: format!("the test failed: {reason} · a copies the prompt"),
                    failed: true,
                }),
                _ => note("not tested yet · v tests it"),
            },
        }
    }
}

impl App {
    /// Reads the setup for the dashboard's line, against the session's
    /// config, which has just been adopted or loaded. Two small files and
    /// a lock asked without waiting.
    pub fn read_setup_row(&mut self) {
        self.setup_row.files_config = None;
        let setup = setup::read(&self.paths, &self.config);
        self.adopt_setup_row(setup);
    }

    /// The line's spinner, and a `v` that waited too long. The files are
    /// the setup screen's poll's.
    pub(super) fn poll_setup_row(&mut self) -> bool {
        if self.setup_screen.is_some() || self.setup_row.setup.is_none() {
            return false;
        }
        let spinning = matches!(
            self.setup_row.hint(),
            Some(SetupHint::Testing(_) | SetupHint::Starting)
        );
        spinning | self.expire_row_check_request()
    }

    /// Takes a fresh read. A check that ends while the dashboard watches
    /// says how it ended, where `m` keeps it too.
    pub(super) fn adopt_setup_row(&mut self, setup: Setup) -> bool {
        let row = &mut self.setup_row;
        if let Some((_, pressed)) = row.check_requested {
            let recorded = setup
                .last_check
                .as_ref()
                .is_some_and(|r| r.started_at >= pressed - chrono::Duration::seconds(1));
            if setup.state == SetupState::Testing || recorded {
                row.check_requested = None;
            }
        }
        let was_testing = row.testing();
        let changed = row.setup.as_ref() != Some(&setup);
        let state = setup.state;
        row.setup = Some(setup);
        if was_testing && state != SetupState::Testing {
            let project = self.paths.project.display_name.clone();
            match state {
                SetupState::Ready => {
                    self.set_success(format!("the test passed: pando is ready for {project}"))
                }
                SetupState::Failing => {
                    self.set_error("the test failed — the header says why, a copies the prompt")
                }
                _ => {}
            }
        }
        changed
    }

    fn expire_row_check_request(&mut self) -> bool {
        if self
            .setup_row
            .check_requested
            .is_none_or(|(at, _)| at.elapsed() < CHECK_START_TIMEOUT)
        {
            return false;
        }
        self.setup_row.check_requested = None;
        let log = crate::tui::render::home_relative(&check_log_file(&self.paths));
        self.set_error(format!("the test did not start — {log} says why"));
        true
    }

    /// `a` on the list: the setup prompt, for the developer's agent.
    pub(super) fn copy_setup_prompt(&mut self) {
        self.copy_to_clipboard(setup::SETUP_PROMPT);
        self.set_success("copied the setup prompt — paste it into your coding agent");
    }

    /// `v` on the list: tests the settings with `pando check`, apart from
    /// the TUI, when there are settings and no check runs.
    pub(super) fn test_setup_from_list(&mut self) {
        let config = self.setup_row.files_config.as_ref().unwrap_or(&self.config);
        if !has_settings(config) {
            self.set_error("nothing to test yet — a copies the setup prompt for your coding agent");
            return;
        }
        if self.setup_row.waiting_on_a_check() {
            self.set_status("a test is already running — the header follows it");
            return;
        }
        if let Err(e) = self.start_check() {
            self.set_error(format!("could not start the test: {e:#}"));
            return;
        }
        self.setup_row.check_requested = Some((Instant::now(), Utc::now()));
    }
}
