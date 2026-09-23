//! `open`: a worktree's URL in the browser — the TUI's `o` and `O` keys,
//! from a shell.

use super::names::Named;
use crate::actions::{self, worktree_url};
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state::{self, Aggregate};
use anyhow::{Context, Result, bail};

/// The URL `open` would hand the browser: the local one while something is
/// up to answer it, or the public one with `public`.
///
/// Messages name the worktree as a person knows it, and the commands they
/// suggest spell it the way it was typed.
pub(super) fn url_to_open(
    paths: &PandoPaths,
    config: &Config,
    named: &Named,
    public: bool,
) -> Result<String> {
    let Named { dir, shown, typed } = named;
    let refreshed = actions::refresh(paths);
    let record = refreshed.state.worktrees.get(dir.as_str());
    if public {
        return match record.and_then(|r| r.share.as_ref()) {
            Some(share) => Ok(share.public_url.clone()),
            None => bail!("{shown} is not shared — `pando share {typed}` publishes it"),
        };
    }
    let not_running = || -> anyhow::Error {
        // "`pando start` starts it" is no advice for a project with nothing
        // to start.
        if config.processes.is_empty() {
            return anyhow::anyhow!(
                "{shown} is not running, and this project has nothing to run — {}",
                crate::doctor::nothing_to_run_fix(&paths.config_file())
            );
        }
        anyhow::anyhow!("{shown} is not running — `pando start {typed}` starts it")
    };
    let Some(record) = record else {
        return Err(not_running());
    };
    match state::aggregate_phase(record) {
        None => Err(not_running()),
        Some(Aggregate::Failed { .. }) => bail!(
            "{shown} has failed — `pando status {typed}` says which process, and \
             `pando restart {typed}` tries again"
        ),
        Some(_) => worktree_url(record).with_context(|| {
            format!("{shown} is running and holds no port, so it has no URL to open")
        }),
    }
}

/// Hands `url` to the browser: `$BROWSER` when it is set, the desktop's
/// own opener otherwise — `open` on macOS, `xdg-open` elsewhere.
pub(super) fn launch(url: &str) -> Result<()> {
    let browser = std::env::var("BROWSER").ok();
    let mut failure = String::new();
    for command in openers(browser.as_deref(), url) {
        let Some((program, args)) = command.split_first() else {
            continue;
        };
        let ran = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        failure = match ran {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => format!("{program} could not open {url} ({status}) — open it by hand"),
            Err(e) => format!("could not run {program:?} to open {url} — open it by hand: {e}"),
        };
    }
    bail!("{failure}")
}

/// The commands `launch` tries, in order, to open `url`.
///
/// `$BROWSER` is read the way other tools read it: a `:`-separated list
/// of browsers, tried until one works, each a command with arguments —
/// `firefox --new-window` — where `%s` stands for the URL, which goes
/// last when there is no `%s`. It used to be run as one program name, so
/// any argument made it "No such file or directory". An entry that is a
/// file as written is one program, even with a space in its path.
pub(super) fn openers(browser: Option<&str>, url: &str) -> Vec<Vec<String>> {
    let entries: Vec<&str> = browser
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() {
        let opener = match cfg!(target_os = "macos") {
            true => "open",
            false => "xdg-open",
        };
        return vec![vec![opener.to_string(), url.to_string()]];
    }
    entries
        .into_iter()
        .map(|entry| {
            let mut words: Vec<String> = match std::path::Path::new(entry).is_file() {
                true => vec![entry.to_string()],
                false => entry.split_whitespace().map(str::to_string).collect(),
            };
            match words.iter().any(|w| w.contains("%s")) {
                true => {
                    for word in &mut words {
                        *word = word.replace("%s", url);
                    }
                }
                false => words.push(url.to_string()),
            }
            words
        })
        .collect()
}
