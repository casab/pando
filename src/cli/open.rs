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
    let opener = match std::env::var("BROWSER") {
        Ok(browser) if !browser.trim().is_empty() => browser,
        _ if cfg!(target_os = "macos") => "open".to_string(),
        _ => "xdg-open".to_string(),
    };
    let status = std::process::Command::new(&opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| format!("could not run {opener:?} to open {url} — open it by hand"))?;
    if !status.success() {
        bail!("{opener} could not open {url} ({status}) — open it by hand");
    }
    Ok(())
}
