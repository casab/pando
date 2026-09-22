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
        self.start_selected_with(actions::Mode::Isolated)
    }

    fn start_selected_with(&mut self, mode: actions::Mode) {
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
            let config = actions::resolve_process(&paths, &config, mode, &ask, &progress)
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
        self.set_status(format!("opened {url}"));
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

    pub(super) fn restart_selected(&mut self) {
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
            let config = actions::resolve_process(
                &paths,
                &config,
                actions::Mode::Remembered,
                &ask,
                &progress,
            )
            .map_err(|e| format!("{e:#}"))?;
            let _ = tx.send(AppEvent::ConfigResolved(Box::new(config.clone())));
            actions::restart(
                &paths,
                &config,
                &worker_name,
                None,
                actions::Mode::Remembered,
                &progress,
            )
            .map(|report| PendingOutcome::Started(worker_name.clone(), report.url.clone()))
            .map_err(|e| format!("{e:#}"))
        });
        if started && let Some(p) = self.pending.as_mut() {
            p.progress_rx = Some(prx);
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

    pub(super) fn copy_selected_path(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            self.set_error("nothing selected");
            return;
        };
        let path = wt.path.display().to_string();
        self.copy_to_clipboard(&path);
        self.set_status(format!("copied {path}"));
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
