//! Leaving pando for a moment: a shell or an editor in the selected
//! worktree.
//!
//! This file only decides *what* to run. The key handler puts a [`Launch`]
//! on the app, and the event loop (`tui::handoff`) carries it out: the
//! loop owns the terminal, and a program that needs the terminal can only
//! have it once the loop has let go. Tests read the request instead, so
//! nothing is spawned under `cargo test`.

use std::path::{Path, PathBuf};

use super::App;
use crate::env_command;
use crate::platform::{Host, desktop};

/// What the environment says about how to leave pando, read once at
/// startup rather than in a key handler, and injected by tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv {
    /// `$TMUX` is set: a new tmux window beats suspending the whole TUI.
    pub tmux: bool,
    pub shell: Option<String>,
    pub visual: Option<String>,
    pub editor: Option<String>,
    /// `$BROWSER`: what `o` and `O` open a URL with, as `pando open` does.
    pub browser: Option<String>,
    /// The machine: what its desktop opens and copies with, and the shell
    /// to fall back on.
    pub host: Host,
}

impl LaunchEnv {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        LaunchEnv {
            tmux: var("TMUX").is_some(),
            shell: var("SHELL"),
            visual: var("VISUAL"),
            editor: var("EDITOR"),
            browser: var("BROWSER"),
            host: Host::here().clone(),
        }
    }
}

/// A program to run on the worktree's behalf, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// `tmux <args>`, detached from pando's terminal: tmux opens the
    /// window and pando keeps running beside it. Run in `cwd`, the
    /// directory the window opens in, because tmux opens a window whose
    /// `-c` is not there in $HOME and says nothing.
    Tmux { args: Vec<String>, cwd: PathBuf },
    /// Give the terminal to `program` until it exits, then take it back.
    Suspend {
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
    },
    /// A program with its own window (a GUI editor): started with every
    /// stream redirected, never waited on by the UI thread.
    Detached {
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
    },
}

/// A request waiting for the event loop, with what to say once it is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub launch: Launch,
    /// Said when the program returns (suspend) or has been handed off.
    pub done: String,
}

/// Editors that open a window of their own. Anything else is assumed to
/// want the terminal: a GUI editor given the terminal just returns, while
/// a terminal editor started without one fails with nothing to show.
const GUI_EDITORS: &[&str] = &[
    "code",
    "code-insiders",
    "codium",
    "cursor",
    "windsurf",
    "zed",
    "subl",
    "mate",
    "bbedit",
    "nova",
    "atom",
    "gvim",
    "mvim",
    "gedit",
    "kate",
    "idea",
    "webstorm",
    "goland",
    "fleet",
    "open",
];

/// Splits `$EDITOR` the way a shell would, so a program whose path has a
/// space in it can be quoted: `"/Applications/My Editor.app/…" -w`.
/// `None` when there is nothing to run, or no telling where it ends.
fn split_command(command: &str) -> Option<(String, Vec<String>)> {
    let mut words = env_command::words(command).ok()?.into_iter();
    let program = words.next()?;
    Some((program, words.collect()))
}

/// Whether an editor command wants the terminal.
pub fn is_terminal_editor(command: &str) -> bool {
    split_command(command).is_some_and(|(program, _)| program_wants_terminal(&program))
}

fn program_wants_terminal(program: &str) -> bool {
    let base = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    !GUI_EDITORS.contains(&base.as_str())
}

/// The longest a tmux window name gets before it is cut.
const WINDOW_NAME_MAX: usize = 30;

/// The name a tmux window gets: the whole branch — tmux takes a `/` in a
/// window name, and `m` alone could be any of `feat/m`, `fix/m` — cut
/// with an ellipsis past the length a status line can show.
pub fn window_name(label: &str) -> String {
    if label.chars().count() <= WINDOW_NAME_MAX {
        return label.to_string();
    }
    let kept: String = label.chars().take(WINDOW_NAME_MAX - 1).collect();
    format!("{kept}…")
}

/// `s` as tmux reads it back, as it is. tmux expands `-c` and `-n` as
/// formats, where `#H` is the host name and `#(…)` runs a command, and a
/// branch can hold either — a fork's, picked by whoever opened the pull
/// request. `##` is how a format says `#`.
fn tmux_literal(s: &str) -> String {
    s.replace('#', "##")
}

/// What to say once the event loop has carried `request` out, if anything.
/// A suspend says it on the way back; a hand-off was already announced by
/// the key that asked for it, so it says nothing more — or it is said
/// twice.
pub fn said_after(request: &LaunchRequest) -> Option<String> {
    matches!(request.launch, Launch::Suspend { .. }).then(|| request.done.clone())
}

/// A shell in `path`: a tmux window inside tmux, the TUI suspended outside.
pub fn plan_shell(env: &LaunchEnv, path: &Path, label: &str) -> LaunchRequest {
    let where_ = path.display().to_string();
    if env.tmux {
        return LaunchRequest {
            launch: Launch::Tmux {
                args: vec![
                    "new-window".into(),
                    "-c".into(),
                    tmux_literal(&where_),
                    "-n".into(),
                    tmux_literal(&window_name(label)),
                ],
                cwd: path.to_path_buf(),
            },
            done: format!(
                "opened a shell in {label} — tmux window {}",
                window_name(label)
            ),
        };
    }
    let shell = env
        .shell
        .clone()
        .unwrap_or_else(|| desktop::fallback_shell(&env.host).to_string());
    LaunchRequest {
        launch: Launch::Suspend {
            program: shell,
            args: Vec::new(),
            cwd: path.to_path_buf(),
        },
        done: format!("back from the shell in {label}"),
    }
}

/// `$VISUAL`, then `$EDITOR`, on `path`. `Err` says what is missing: with
/// neither set there is nothing to guess from.
pub fn plan_editor(env: &LaunchEnv, path: &Path, label: &str) -> Result<LaunchRequest, String> {
    let Some(command) = env.visual.clone().or_else(|| env.editor.clone()) else {
        return Err("set $VISUAL or $EDITOR to open a worktree in your editor".to_string());
    };
    let Some((program, mut args)) = split_command(&command) else {
        if env_command::words(&command).is_err() {
            return Err(format!(
                "$VISUAL / $EDITOR has a quote that never closes: {command}"
            ));
        }
        return Err("$VISUAL / $EDITOR is empty".to_string());
    };
    args.push(path.display().to_string());
    let shown = Path::new(&program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    if !program_wants_terminal(&program) {
        return Ok(LaunchRequest {
            launch: Launch::Detached {
                program,
                args,
                cwd: path.to_path_buf(),
            },
            done: format!("opened {label} in {shown}"),
        });
    }
    if env.tmux {
        // The program and its arguments are not formats: tmux runs them
        // as they are, the path included.
        let mut tmux = vec![
            "new-window".to_string(),
            "-c".into(),
            tmux_literal(&path.display().to_string()),
            "-n".into(),
            tmux_literal(&window_name(label)),
            program,
        ];
        tmux.extend(args);
        return Ok(LaunchRequest {
            launch: Launch::Tmux {
                args: tmux,
                cwd: path.to_path_buf(),
            },
            done: format!(
                "editing {label} in {shown} — tmux window {}",
                window_name(label)
            ),
        });
    }
    Ok(LaunchRequest {
        launch: Launch::Suspend {
            program,
            args,
            cwd: path.to_path_buf(),
        },
        done: format!("back from {shown} in {label}"),
    })
}

impl App {
    fn selected_path_and_label(&mut self) -> Option<(PathBuf, String)> {
        let name = self.selected_name()?;
        // Not checked for existence here: a missing directory is reported
        // by whatever runs in it, off this thread — tmux included, which
        // is run in it for that reason.
        let path = self.selected_worktree()?.path.clone();
        Some((path, self.label_of(&name)))
    }

    /// `!`: a shell in the selected worktree.
    pub(super) fn open_shell(&mut self) {
        let Some((path, label)) = self.selected_path_and_label() else {
            return;
        };
        self.open_shell_in(&path, &label);
    }

    /// A shell in `path`: `!`, and the git menu's way to do it by hand.
    pub(super) fn open_shell_in(&mut self, path: &Path, label: &str) {
        let request = plan_shell(&self.launch_env, path, label);
        self.request_launch(request);
    }

    /// `e`: the selected worktree in the developer's editor.
    pub(super) fn open_editor(&mut self) {
        let Some((path, label)) = self.selected_path_and_label() else {
            return;
        };
        match plan_editor(&self.launch_env, &path, &label) {
            Ok(request) => self.request_launch(request),
            Err(message) => self.set_error(message),
        }
    }

    fn request_launch(&mut self, request: LaunchRequest) {
        // A suspend says its message on the way back; the others are
        // handed off at once, so they say it now, and only now.
        if said_after(&request).is_none() {
            self.set_success(request.done.clone());
        }
        self.launch = Some(request);
    }
}
