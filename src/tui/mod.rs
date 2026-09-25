//! The ratatui front end: terminal setup, the event loop, and the watchers
//! that feed it. Like `cli`, a thin wrapper — behaviour lives in `actions`.

pub mod app;
mod handoff;
mod modal;
mod render;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, Debouncer, RecommendedCache, new_debouncer};
use ratatui::crossterm::event;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::paths::PandoPaths;
use app::AppEvent;
use handoff::InputGate;

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const FS_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long the input thread waits for a key before it checks whether a
/// suspended program wants the terminal. Short enough that a suspend
/// starts at once.
const INPUT_POLL: Duration = Duration::from_millis(50);

pub fn run(paths: PandoPaths, config: Config) -> Result<()> {
    // Before anything creates a directory under it: the watcher and the
    // enrichment cache both `create_dir_all` their way down, and a home made
    // by one of those would carry the umask instead of 0700.
    paths.ensure_home()?;
    let worktrees_dir = config.worktrees_dir(&paths);
    let mut app = app::App::new(paths, config)?;
    let mut terminal = ratatui::init();
    let result = main_loop(&mut terminal, &mut app, &worktrees_dir);
    ratatui::restore();
    result
}

fn main_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut app::App,
    worktrees_dir: &Path,
) -> Result<()> {
    let event_rx = app
        .event_rx
        .take()
        .expect("event_rx is present until the loop takes it");

    let gate = Arc::new(InputGate::default());
    spawn_input_thread(app.event_tx.clone(), Arc::clone(&gate));
    spawn_tick_thread(app.event_tx.clone());
    // The watcher is an optimisation for pando's own worktrees; the slow
    // tick's porcelain refresh is the source of truth, and it sees adopted
    // worktrees anywhere on disk.
    let _watcher = spawn_fs_watcher(worktrees_dir, app.event_tx.clone());

    terminal.draw(|f| render::render(f, app))?;
    loop {
        let Ok(ev) = event_rx.recv() else {
            return Ok(());
        };
        let mut redraw = app.handle_event(ev);
        // Coalesce whatever piled up while drawing, so a burst (typing, a
        // resize, an enrichment flood) costs one redraw.
        loop {
            match event_rx.try_recv() {
                Ok(more) => redraw |= app.handle_event(more),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // A shell or editor `!` or `e` asked for. Run here because this is
        // where the terminal is.
        if let Some(request) = app.launch.take() {
            match handoff::carry_out(request, terminal, &gate, &app.event_tx) {
                Ok(Some(done)) => app.set_success(done),
                Ok(None) => {}
                Err(e) => app.set_error(e),
            }
            // Whatever changed while it ran — a commit, a server stopped
            // from the shell, a file edited — shows at once rather than on
            // the slow tick.
            app.spawn_discovery();
            app.refresh_git(None);
            redraw = true;
        }
        if redraw {
            terminal.draw(|f| render::render(f, app))?;
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn spawn_input_thread(tx: Sender<AppEvent>, gate: Arc<InputGate>) {
    thread::spawn(move || {
        loop {
            // Polled rather than a bare `read`, so the thread comes back
            // here often enough to step aside for a suspended program.
            gate.wait_if_paused();
            match event::poll(INPUT_POLL) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => return,
            }
            match event::read() {
                Ok(ev) => {
                    if tx.send(AppEvent::Input(ev)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

fn spawn_tick_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            thread::sleep(TICK_INTERVAL);
            if tx.send(AppEvent::Tick).is_err() {
                return;
            }
        }
    });
}

/// Watches the worktrees directory non-recursively. The returned debouncer
/// owns the watcher; dropping it stops the thread. `None` when the watcher
/// cannot start — the slow tick still refreshes.
fn spawn_fs_watcher(
    dir: &Path,
    app_tx: Sender<AppEvent>,
) -> Option<Debouncer<RecommendedWatcher, RecommendedCache>> {
    // notify fails on a path that does not exist, and this one does not
    // until the first `new`.
    std::fs::create_dir_all(dir).ok()?;
    let (raw_tx, raw_rx) = mpsc::channel();
    let mut debouncer = new_debouncer(FS_DEBOUNCE, None, raw_tx).ok()?;
    debouncer.watch(dir, RecursiveMode::NonRecursive).ok()?;
    // FSEvents reports resolved paths, so relevance checks compare against
    // the canonicalized watched directory.
    let watched = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    thread::spawn(move || {
        while let Ok(batch) = raw_rx.recv() {
            let relevant = match &batch {
                Ok(events) => fs_batch_is_relevant(&watched, events),
                Err(_) => true,
            };
            if relevant && app_tx.send(AppEvent::FsChange).is_err() {
                return;
            }
        }
    });
    Some(debouncer)
}

/// True when a debounced batch touches something discovery would list: a
/// path whose first component under the watched directory is not
/// dot-prefixed. Fails open on anything ambiguous.
fn fs_batch_is_relevant(watched: &Path, events: &[DebouncedEvent]) -> bool {
    if events.is_empty() {
        return true;
    }
    events.iter().any(|ev| {
        if ev.paths.is_empty() {
            return true;
        }
        ev.paths.iter().any(|path| {
            let Ok(relative) = path.strip_prefix(watched) else {
                return true;
            };
            let Some(first) = relative.components().next() else {
                return true;
            };
            !first.as_os_str().to_string_lossy().starts_with('.')
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Event;
    use notify::event::{CreateKind, EventKind};
    use std::path::PathBuf;
    use std::time::Instant;

    fn event_with_paths(paths: &[PathBuf]) -> DebouncedEvent {
        let mut event = Event::new(EventKind::Create(CreateKind::Any));
        for path in paths {
            event = event.add_path(path.clone());
        }
        DebouncedEvent::new(event, Instant::now())
    }

    fn watched() -> PathBuf {
        PathBuf::from("/home/.pando/projects/acme-3f9a2c1d/worktrees")
    }

    #[test]
    fn a_new_worktree_directory_is_relevant() {
        let batch = [event_with_paths(&[watched().join("feat+one")])];
        assert!(fs_batch_is_relevant(&watched(), &batch));
    }

    #[test]
    fn dot_prefixed_artifacts_are_not_relevant() {
        let batch = [
            event_with_paths(&[watched().join(".scratch")]),
            event_with_paths(&[watched().join(".hidden/thing.log")]),
        ];
        assert!(!fs_batch_is_relevant(&watched(), &batch));
    }

    #[test]
    fn a_mixed_batch_is_relevant() {
        let batch = [
            event_with_paths(&[watched().join(".scratch")]),
            event_with_paths(&[watched().join("feat+one")]),
        ];
        assert!(fs_batch_is_relevant(&watched(), &batch));
    }

    #[test]
    fn anything_ambiguous_fails_open() {
        assert!(fs_batch_is_relevant(&watched(), &[]));
        assert!(fs_batch_is_relevant(&watched(), &[event_with_paths(&[])]));
        assert!(fs_batch_is_relevant(
            &watched(),
            &[event_with_paths(&[watched()])]
        ));
        assert!(fs_batch_is_relevant(
            &watched(),
            &[event_with_paths(&[PathBuf::from("/somewhere/else")])]
        ));
    }

    #[test]
    fn the_watcher_creates_the_directory_it_watches() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("projects").join("p").join("worktrees");
        let (tx, _rx) = mpsc::channel();
        let watcher = spawn_fs_watcher(&target, tx);
        assert!(target.is_dir(), "notify cannot watch a missing directory");
        drop(watcher);
    }
}
