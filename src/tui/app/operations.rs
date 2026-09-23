//! What a key does to the selected worktree: start, stop, restart, share,
//! open its URL, copy its path.

use std::sync::mpsc;
#[cfg(not(test))]
use std::thread;

use crate::actions;

use super::App;
use super::background::{AppEvent, ask_through_ui};
use super::dialogs::Modal;
use super::pending::{PendingKind, PendingOutcome};

/// Standard base64, for the OSC 52 payload. A dependency would be a lot of
/// machinery for one escape sequence.
pub(super) fn base64(input: &[u8]) -> String {
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

impl App {
    /// The URL a worktree serves on.
    ///
    /// The one shared rule, so the row, the detail pane and `o` open the
    /// address `pando status` prints — including when a framework ignored
    /// the port it was given.
    pub fn url_of(&self, name: &str) -> Option<String> {
        actions::worktree_url(self.record_for(name)?)
    }

    pub(super) fn selected_name(&mut self) -> Option<String> {
        match self.selected_worktree() {
            Some(wt) => Some(wt.name.clone()),
            None => {
                self.set_error("nothing selected");
                None
            }
        }
    }

    pub(super) fn start_selected(&mut self) {
        self.start_selected_with(actions::Mode::Remembered)
    }

    /// Start with private copies of the project's services. The same path
    /// as `start_selected`; the flag reaches detection too, because the
    /// services question is only worth asking when it is being answered.
    pub(super) fn start_selected_isolated(&mut self) {
        self.start_selected_in(actions::Mode::Isolated)
    }

    /// Back to the project's own services, the private copies stopped:
    /// `start --shared`.
    pub(super) fn start_selected_shared(&mut self) {
        self.start_selected_in(actions::Mode::Shared)
    }

    /// A mode key on a worktree that is already up is a restart in that
    /// mode: a start would find its processes running and leave them on
    /// the services they were started against.
    fn start_selected_in(&mut self, mode: actions::Mode) {
        let running = self
            .selected_worktree()
            .is_some_and(|wt| self.phase_of(&wt.name).is_some());
        if running {
            self.restart_selected_with(mode)
        } else {
            self.start_selected_with(mode)
        }
    }

    /// Enter: the log of a worktree that runs, or has failed — the log is
    /// what says why — and a start for one that is stopped.
    pub(super) fn enter_selected(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        if self.phase_of(&name).is_some() {
            self.open_log_viewer();
        } else {
            self.start_selected();
        }
    }

    /// Whether the project has nothing to run, said on the status line if
    /// so. The config is read again first — a `[dev]` added while the TUI
    /// is open is what somebody reading that line goes and does — so the
    /// next key after the edit starts it.
    fn refuses_nothing_to_run(&mut self) -> bool {
        if !self.nothing_to_run {
            return false;
        }
        if let Ok(loaded) = crate::config::load(&self.paths)
            && !loaded.config.processes.is_empty()
        {
            self.config = loaded.config;
            self.nothing_to_run = false;
            return false;
        }
        self.set_status(self.nothing_to_run_line());
        true
    }

    fn start_selected_with(&mut self, mode: actions::Mode) {
        let Some(name) = self.selected_name() else {
            return;
        };
        if self.refuses_nothing_to_run() {
            return;
        }
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
            let config =
                actions::resolve_for_start(&paths, &config, &worker_name, mode, &ask, &progress)
                    .map_err(|e| format!("{e:#}"))?;
            // Back to the UI thread at once: an answer written to
            // `pando.toml` that this session's own copy does not have is
            // one the next keypress asks all over again.
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            actions::start(&paths, &config, &worker_name, None, mode, &progress)
                .map(|report| PendingOutcome::Started(worker_name.clone(), report.url.clone()))
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
    }

    pub(super) fn stop_selected(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let paths = self.paths.clone();
        let worker_name = name.clone();
        // A stop reaches the sweep that closes a *sibling's* half-dead
        // share, and that notice is the only warning its public URL has
        // gone — so `stop` narrates like `start` does.
        let (ptx, prx) = mpsc::channel::<String>();
        let started = self.spawn_pending(name, PendingKind::Stop, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            actions::stop(&paths, &worker_name, None, &progress)
                .map(|_| PendingOutcome::Stopped(worker_name))
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
    }

    /// The public URL of a worktree, when it has one.
    pub fn public_url_of(&self, name: &str) -> Option<String> {
        Some(self.record_for(name)?.share.as_ref()?.public_url.clone())
    }

    pub(super) fn open_selected_public_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.public_url_of(&name) else {
            self.set_error(format!("{name} is not shared — t shares it"));
            return;
        };
        self.open_url(&url);
        self.set_success(format!("opened {url}"));
    }

    /// One key for both directions. Sharing is a plain action; unsharing
    /// asks first, because somebody may be looking at that URL right now.
    pub(super) fn toggle_share(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        match self.public_url_of(&name) {
            Some(url) => self.modal = Some(Modal::Unshare { name, url }),
            None => self.share_selected(name),
        }
    }

    fn share_selected(&mut self, name: String) {
        let paths = self.paths.clone();
        let config = self.config.clone();
        let worker_name = name.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        // A tunnel takes up to thirty seconds to publish, and an auth
        // command as long as it takes: all of it on the worker, with the
        // sub-steps coming back as progress.
        let started = self.spawn_pending(name, PendingKind::Share, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            actions::share(&paths, &config, &worker_name, &progress)
                .map(|outcome| {
                    PendingOutcome::Shared(outcome.name, outcome.public_url, outcome.pre_authed)
                })
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
        }
    }

    pub(super) fn unshare_selected(&mut self, name: String) {
        let paths = self.paths.clone();
        let worker_name = name.clone();
        self.spawn_pending(name, PendingKind::Unshare, move || {
            actions::unshare(&paths, &worker_name)
                .map(|()| PendingOutcome::Unshared(worker_name))
                .map_err(|e| format!("{e:#}"))
        });
    }

    /// `r`. On a worktree that is not running there is nothing to
    /// restart, and what somebody pressing it wants is for it to run.
    pub(super) fn restart_selected(&mut self) {
        let stopped = self
            .selected_worktree()
            .is_some_and(|wt| self.phase_of(&wt.name).is_none());
        if stopped {
            self.start_selected()
        } else {
            self.restart_selected_with(actions::Mode::Remembered)
        }
    }

    fn restart_selected_with(&mut self, mode: actions::Mode) {
        self.restart_selected_only(mode, None);
    }

    /// `p`: only the process the detail pane's `▸` marks — `restart
    /// --only`. The others keep running, and so do their logs.
    pub(super) fn restart_selected_process(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let processes = self.processes_of(&name);
        match processes.len() {
            0 => {
                let label = self.label_of(&name);
                self.set_error(format!("{label} is running nothing — s starts it"));
            }
            1 => self.restart_selected_with(actions::Mode::Remembered),
            n => {
                let process = processes[self.tail_index.min(n - 1)].0.clone();
                self.restart_selected_only(actions::Mode::Remembered, Some(process));
            }
        }
    }

    fn restart_selected_only(&mut self, mode: actions::Mode, only: Option<String>) {
        let Some(name) = self.selected_name() else {
            return;
        };
        if self.phase_of(&name).is_none() && self.refuses_nothing_to_run() {
            return;
        }
        let label = only
            .as_ref()
            .map(|process| format!("{process} of {}", self.label_of(&name)));
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
            let config =
                actions::resolve_for_start(&paths, &config, &worker_name, mode, &ask, &progress)
                    .map_err(|e| format!("{e:#}"))?;
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            actions::restart(
                &paths,
                &config,
                &worker_name,
                only.as_deref(),
                mode,
                &progress,
            )
            .map(|report| PendingOutcome::Started(worker_name.clone(), report.url.clone()))
            .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
            if let Some(label) = label {
                p.label = label;
            }
        }
    }

    /// Whether anything of a worktree's is up: a process that has not
    /// failed, a service, or a tunnel. A crashed dev server's database
    /// counts; a record left behind by a stop, or one whose only process
    /// has exited, does not.
    pub fn is_live(&self, name: &str) -> bool {
        self.record_for(name).is_some_and(|record| {
            record
                .processes
                .values()
                .any(|p| !matches!(p.phase, crate::state::Phase::Failed { .. }))
                || !record.services.is_empty()
                || record.share.is_some()
        })
    }

    /// The worktrees `X` would stop: every one with something up.
    pub fn stop_all_targets(&self) -> Vec<String> {
        self.state
            .worktrees
            .keys()
            .filter(|name| self.is_live(name))
            .cloned()
            .collect()
    }

    /// `X`: asks first, listing what goes down. With nothing up there is
    /// nothing to confirm.
    pub(super) fn confirm_stop_all(&mut self) {
        let names = self.stop_all_targets();
        if names.is_empty() {
            self.set_status("nothing is running");
            return;
        }
        self.modal = Some(Modal::StopAll { names });
    }

    pub(super) fn stop_everything(&mut self) {
        let paths = self.paths.clone();
        let (ptx, prx) = mpsc::channel::<String>();
        // No worktree's name: the rows it covers are found through
        // `pending_on`, and an empty name matches no row by itself.
        let started = self.spawn_pending(String::new(), PendingKind::StopAll, move || {
            let progress = |msg: &str| {
                let _ = ptx.send(msg.to_string());
            };
            actions::stop_all(&paths, &progress)
                .map(PendingOutcome::StoppedAll)
                .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
            p.label = "everything".to_string();
            self.set_progress("stopping everything…");
        }
    }

    pub(super) fn open_selected_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.url_of(&name) else {
            self.set_error(format!("{name} has no port yet — start it first"));
            return;
        };
        self.open_url(&url);
        self.set_success(format!("opened {url}"));
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

    pub(super) fn copy_selected_path(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            self.set_error("nothing selected");
            return;
        };
        let path = wt.path.display().to_string();
        self.copy_to_clipboard(&path);
        self.set_success(format!("copied {path}"));
    }

    /// The URL a worktree is reached at, for pasting: the public one when
    /// it is shared, because that is the one being handed to somebody.
    pub(super) fn copy_selected_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.public_url_of(&name).or_else(|| self.url_of(&name)) else {
            self.set_error(format!("{name} has no URL yet — start it first"));
            return;
        };
        self.copy_to_clipboard(&url);
        self.set_success(format!("copied {url}"));
    }

    #[cfg(test)]
    pub(super) fn copy_to_clipboard(&mut self, text: &str) {
        self.clipboard = Some(text.to_string());
    }

    /// OSC 52 first: it works on Linux and macOS, and through tmux with
    /// `set-clipboard on`. `pbcopy` is a macOS-only fallback for terminals
    /// that ignore the sequence; it is spawned with every stdio redirected,
    /// never inheriting the alternate screen.
    ///
    /// The escape sequence is written from here — it is one `write` to the
    /// terminal pando already owns — but the child is not. `y` is a key
    /// handler, and waiting for `pbcopy` to drain its stdin there is a
    /// blocking wait on a child inside the frame, so it goes to a detached
    /// thread exactly as the browser opener does.
    #[cfg(not(test))]
    pub(super) fn copy_to_clipboard(&mut self, text: &str) {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = stdout.flush();

        if !cfg!(target_os = "macos") {
            return;
        }
        let text = text.to_string();
        thread::spawn(move || {
            let Ok(mut child) = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            // Taking the handle above closed pando's end of the pipe, so
            // `pbcopy` sees EOF; this reaps it rather than leaving a
            // zombie behind every yank. It blocks a worker thread, which
            // is what worker threads are for.
            let _ = child.wait_with_output();
        });
    }
}
