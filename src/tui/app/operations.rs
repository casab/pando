//! What a key does to the selected worktree: start, stop, restart, share,
//! open its URL, copy its path.

use std::sync::mpsc;
#[cfg(not(test))]
use std::thread;

use crate::actions;
use crate::state::ServiceMode;

use super::background::{AppEvent, ask_through_ui};
use super::dialogs::Modal;
use super::pending::{PendingKind, PendingOutcome};
use super::{ARM_TTL, App, Armed};

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

/// The commands `o` and `O` try, in order, to open `url`.
///
/// `$BROWSER` is read the way `pando open` and other tools read it: a
/// `:`-separated list of browsers, tried until one works, each a command
/// with arguments where `%s` stands for the URL — which goes last when
/// there is no `%s`. An entry that is a file as written is one program,
/// even with a space in its path. Unset or empty, the desktop's opener.
pub(super) fn browser_commands(browser: Option<&str>, url: &str) -> Vec<Vec<String>> {
    let entries: Vec<&str> = browser
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        return vec![vec![opener.to_string(), url.to_string()]];
    }
    entries
        .into_iter()
        .map(|entry| {
            let mut words: Vec<String> = if std::path::Path::new(entry).is_file() {
                vec![entry.to_string()]
            } else {
                entry.split_whitespace().map(str::to_string).collect()
            };
            if words.iter().any(|word| word.contains("%s")) {
                for word in &mut words {
                    *word = word.replace("%s", url);
                }
            } else {
                words.push(url.to_string());
            }
            words
        })
        .collect()
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
        self.start_selected_in(ServiceMode::Isolated, 'i')
    }

    /// Back to the project's own services, the private copies stopped:
    /// `start --shared`.
    pub(super) fn start_selected_shared(&mut self) {
        self.start_selected_in(ServiceMode::Shared, 'S')
    }

    /// A mode key on a worktree that is already up is a restart in that
    /// mode: a start would find its processes running and leave them on
    /// the services they were started against.
    ///
    /// Pressed on one that runs in another mode, it asks in a dialog,
    /// because that restart swaps the services under every process. In
    /// the mode it already runs in it is a plain restart, and asks for the
    /// key twice like `r` does.
    fn start_selected_in(&mut self, to: ServiceMode, key: char) {
        let mode = actions::Mode::from(to);
        let Some(name) = self.selected_worktree().map(|wt| wt.name.clone()) else {
            return self.start_selected_with(mode);
        };
        if self.phase_of(&name).is_none() {
            return self.start_selected_with(mode);
        }
        let runs = self.record_for(&name).map(|r| r.mode()).unwrap_or_default();
        if self.is_up(&name) && to != runs {
            self.modal = Some(Modal::SwitchMode { name, to });
            return;
        }
        let label = self.label_of(&name);
        if self.pressed_twice(key, &name, &format!("restart {label} {}", to.word())) {
            self.restart_selected_with(mode)
        }
    }

    /// Whether anything of a worktree is up — starting or running, not
    /// only failed. Only then is there something a key would interrupt.
    pub fn is_up(&self, name: &str) -> bool {
        self.record_for(name).is_some_and(|record| {
            record
                .processes
                .values()
                .any(|p| !matches!(p.phase, crate::state::Phase::Failed { .. }))
        })
    }

    /// Whether a key that interrupts a worktree goes ahead now. On one with
    /// nothing up it always does. On one that runs, the first press only
    /// says what the second would do, and the second — the same key, the
    /// same worktree, within `ARM_TTL` — does it. A dialog for these
    /// would be read once and then answered without reading; a second
    /// press is cheap on purpose and still catches a stray one.
    pub(super) fn pressed_twice(&mut self, key: char, name: &str, what: &str) -> bool {
        if !self.is_up(name) {
            return true;
        }
        if let Some(armed) = self.armed.take()
            && armed.key == key
            && armed.name == name
            && armed.at.elapsed() < ARM_TTL
        {
            return true;
        }
        self.armed = Some(Armed {
            key,
            name: name.to_string(),
            at: std::time::Instant::now(),
        });
        self.set_prompt(format!("{what}? {key} again to confirm · esc cancels"));
        false
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
        let label = self.label_of(&name);
        if !self.pressed_twice('x', &name, &format!("stop {label}")) {
            return;
        }
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

    /// One key for both directions, and both ask first: sharing puts the
    /// dev server on the internet, and unsharing takes a URL away from
    /// somebody who may be looking at it right now.
    pub(super) fn toggle_share(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        self.modal = Some(match self.public_url_of(&name) {
            Some(url) => Modal::Unshare { name, url },
            None => Modal::Share { name },
        });
    }

    pub(super) fn share_selected(&mut self, name: String) {
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
            return self.start_selected();
        }
        let Some(name) = self.selected_name() else {
            return;
        };
        let label = self.label_of(&name);
        if self.pressed_twice('r', &name, &format!("restart {label}")) {
            self.restart_selected_with(actions::Mode::Remembered)
        }
    }

    pub(super) fn restart_selected_with(&mut self, mode: actions::Mode) {
        self.restart_selected_only(mode, None);
    }

    /// `P`: only the process the detail pane's `▸` marks — `restart
    /// --only`. The others keep running, and so do their logs.
    pub(super) fn restart_selected_process(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let processes = self.processes_of(&name);
        let label = self.label_of(&name);
        match processes.len() {
            0 => {
                self.set_error(format!("{label} is running nothing — s starts it"));
            }
            1 => {
                if self.pressed_twice('P', &name, &format!("restart {label}")) {
                    self.restart_selected_with(actions::Mode::Remembered)
                }
            }
            n => {
                let process = processes[self.tail_index.min(n - 1)].0.clone();
                if self.pressed_twice('P', &name, &format!("restart {process} of {label}")) {
                    self.restart_selected_only(actions::Mode::Remembered, Some(process));
                }
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

    /// Hands a URL to the browser — `$BROWSER` when it is set, as
    /// `pando open` reads it, the desktop's opener otherwise — on a worker
    /// thread with every stream captured: a browser launcher that writes
    /// to the terminal would paint over the alternate screen. One that
    /// fails says so, rather than leaving `opened …` on screen alone.
    #[cfg(not(test))]
    fn open_url(&mut self, url: &str) {
        let commands = browser_commands(self.launch_env.browser.as_deref(), url);
        let url = url.to_string();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let mut failure = String::new();
            for command in commands {
                let Some((program, args)) = command.split_first() else {
                    continue;
                };
                let ran = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output();
                failure = match ran {
                    Ok(out) if out.status.success() => return,
                    Ok(out) => format!("{program} could not open {url} ({})", out.status),
                    Err(e) => format!("could not run {program} to open {url}: {e}"),
                };
            }
            let _ = tx.send(AppEvent::LaunchFailed(failure));
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

    /// `c`: the local URL, shared or not. One key that copied the public
    /// URL whenever there was one pasted a tunnel address where a
    /// localhost one was wanted; each URL has its own key instead.
    pub(super) fn copy_selected_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.url_of(&name) else {
            let label = self.label_of(&name);
            self.set_error(format!("{label} has no URL yet — start it first"));
            return;
        };
        self.copy_to_clipboard(&url);
        self.set_success(format!("copied {url}"));
    }

    /// `C`: the public URL, the one handed to somebody else.
    pub(super) fn copy_selected_public_url(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(url) = self.public_url_of(&name) else {
            let label = self.label_of(&name);
            self.set_error(format!("{label} is not shared — t shares it"));
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
