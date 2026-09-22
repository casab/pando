//! `doctor`'s front end: printing the report, or JSON, and `--adopt`.

use super::JSON_VERSION;
use super::notice;
use crate::paths::PandoPaths;
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

// ---- doctor ---------------------------------------------------------------

/// Prints the report and turns it into an exit code.
///
/// The failing case carries no message of its own: every problem is
/// already on stdout with its fix, and `pando: <something>` beneath the
/// report would be a reason the command did not give.
pub fn adopt_project<W: Write>(
    paths: &PandoPaths,
    old_id: &str,
    yes: bool,
    out: &mut W,
) -> Result<()> {
    let adoption = crate::doctor::adopt(paths, old_id, &|plan| {
        if yes {
            return Ok(true);
        }
        // On stderr, like every other thing pando narrates, so a piped
        // stdout is still only what the command was asked for.
        let mut err = std::io::stderr();
        writeln!(err, "pando: move {}", plan.from.display())?;
        writeln!(err, "           to {}", plan.to.display())?;
        if let Some(root) = &plan.old_root {
            writeln!(
                err,
                "       its repository was at {}, and is not there now",
                root.display()
            )?;
        }
        if !plan.worktrees.is_empty() {
            writeln!(
                err,
                "       {} {} with it: {}",
                plan.worktrees.len(),
                match plan.worktrees.len() {
                    1 => "worktree moves",
                    _ => "worktrees move",
                },
                plan.worktrees.join(", ")
            )?;
        }
        write!(err, "pando: go ahead? [y/N] ")?;
        err.flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            // No terminal to ask. `--yes` is the way to say yes without
            // one, and taking silence for consent is not.
            return Ok(false);
        }
        Ok(matches!(line.trim(), "y" | "Y" | "yes"))
    })?;
    for line in &adoption.notices {
        notice(line);
    }
    if adoption.rewritten > 0 {
        notice(&format!(
            "{} recorded {} moved with it",
            adoption.rewritten,
            match adoption.rewritten {
                1 => "path",
                _ => "paths",
            }
        ));
    }
    writeln!(out, "adopted {} as {}", old_id, paths.project_id())?;
    Ok(())
}

/// The whole report, versioned like `ls --json` and `status --json` are.
///
/// `ok` is the exit code as a value: an agent reading this should not have
/// to count severities to learn what the shell already told it.
#[derive(Serialize)]
struct DoctorOutput<'a> {
    version: u32,
    ok: bool,
    #[serde(flatten)]
    report: &'a crate::doctor::Report,
}

pub fn doctor<W: Write>(paths: &PandoPaths, json: bool, out: &mut W) -> Result<()> {
    let report = crate::doctor::run(paths);
    let healthy = report.healthy();
    match json {
        true => writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&DoctorOutput {
                version: JSON_VERSION,
                ok: healthy,
                report: &report,
            })?
        )?,
        false => write!(out, "{}", report.render())?,
    }
    match healthy {
        true => Ok(()),
        false => Err(crate::doctor::Unhealthy.into()),
    }
}
