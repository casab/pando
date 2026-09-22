//! `doctor`: what pando found, from where, and what is wrong.
//!
//! Read-only. It is the command a developer runs on a machine nobody else
//! can see, so it is written to be read by a stranger: every fact names the
//! file or the path it came from, and every problem says what to do about
//! it. It exits 0 when nothing found will break a command and 1 when
//! something will, and it never fails the shell for a reason it has not
//! printed.
//!
//! **Where this sits.** Above `actions`, not beneath it:
//! `paths → … → actions → doctor → cli · tui`. doctor reports what the rest
//! of pando already knows — the slots the resolver would ask about, the
//! services a record holds, the shell the start path probes — so a module
//! below `actions` would have to keep a second copy of all of it.
//!
//! **What it must never do.** Write. Not a config, not a cache, not a state
//! file, not pando's home. `actions::refresh` is therefore out of bounds
//! here: it takes the lock, advances phases and saves. doctor loads state,
//! advances a *copy* in memory, and reports the difference.
//!
//! **Where things are.** `report` holds the types the report is made of
//! (`Finding`, `Section`, `Severity`, and one `*Report` per section) and
//! `render` turns them into text. Each section is built in its own file:
//! `config` (the project and every config layer), `runtime`, `tools`,
//! `worktrees`, `services`, `hooks`, and `adopt` (project folders left
//! behind by a moved repository, and `--adopt` itself). `stale` compares
//! detected values with what detection would write now, and `validate`
//! checks the merged config. This file only gathers them: [`run`] and
//! [`run_on`].

use crate::actions;
use crate::actions::Machine;
use crate::paths::PandoPaths;

mod adopt;
mod config;
mod hooks;
mod render;
mod report;
mod runtime;
mod services;
mod stale;
#[cfg(test)]
mod tests;
mod tools;
mod validate;
mod worktrees;

use adopt::adoptable;
pub use adopt::{AdoptPlan, Adoptable, Adoption, adopt};
use config::{config_report, project_report};
use hooks::hooks_report;
pub use report::{
    ComposeEntryReport, ConfigReport, EngineBinary, Finding, HookReport, HookRunReport,
    IncludedService, IsolationReport, KeyReport, LanguageReport, LayerReport, NativeInstance,
    NativeServiceReport, ProcessReport, ProjectReport, Report, RuntimeReport, Section,
    ServicesReport, Severity, ToolReport, Unhealthy, WorktreeReport, WorktreeServiceReport,
};
use runtime::runtime_report;
use services::services_report;
use stale::stale_detection_findings;
use tools::tools_report;
use validate::validate_config;
use worktrees::worktrees_report;

/// Everything doctor has to say about this project, gathered without
/// writing anything anywhere.
///
/// The shell is the one a real spawn uses — `bash -lc`, in the main
/// checkout — built here the way `actions::resolve_silencing` builds it,
/// so what doctor reports about this machine is what the start path would
/// have found.
pub fn run(paths: &PandoPaths) -> Report {
    let shell = actions::runtime_shell(paths.root());
    let machine = Machine {
        shell: &shell,
        home: actions::user_home(),
    };
    run_on(paths, &machine)
}

/// [`run`] with the machine injected, so a test can report on a laptop it
/// does not have.
pub fn run_on(paths: &PandoPaths, machine: &Machine<'_>) -> Report {
    let mut findings: Vec<Finding> = Vec::new();

    // Its own load, not the one `main` did: `main` hands every command the
    // merged config and throws away the error, and the error is the fact
    // doctor exists to report. `Command::needs_config` is false for
    // `Doctor` for the same reason — a project layer pando cannot read is
    // exactly when this command is worth running.
    let (config, error, warnings) = match crate::config::load(paths) {
        Ok(loaded) => (loaded.config, None, loaded.warnings),
        Err(e) => {
            let fallback = crate::config::load_without_home(paths);
            (fallback.config, Some(format!("{e:#}")), fallback.warnings)
        }
    };

    let config_report = config_report(paths, error, warnings, &mut findings);
    validate_config(paths, &config, &mut findings);
    stale_detection_findings(paths, &config_report, &mut findings);
    let project = project_report(paths, &config, &mut findings);
    let runtime = runtime_report(paths, &config, machine, &mut findings);
    let tools = tools_report(paths, &config, machine, &mut findings);
    // Once, and shared: every group's listening sockets are scanned to
    // advance a phase, and that is a real cost to pay twice.
    let view = actions::inspect(paths);
    let worktrees = worktrees_report(paths, &config, &view, &mut findings);
    let services = services_report(paths, &config, machine, &mut findings);
    let hooks = hooks_report(paths, &config, &view, &worktrees, &mut findings);
    let adoption = adoption_report(paths, &mut findings);

    Report {
        project,
        config: config_report,
        runtime,
        tools,
        worktrees,
        services,
        hooks,
        adoption,
        findings,
    }
}

fn adoption_report(paths: &PandoPaths, findings: &mut Vec<Finding>) -> Vec<Adoptable> {
    let found = adoptable(paths);
    for entry in &found {
        findings.push(
            Finding::note(
                Section::Adoption,
                format!(
                    "{} holds a project folder for a repository called {:?} that is no longer \
                     where it was{} — the id is a hash of the path, so this repository moving \
                     gave it a new one and left that folder behind",
                    entry.id,
                    paths.project.display_name,
                    match &entry.old_root {
                        Some(root) => format!(" ({root})"),
                        None => String::new(),
                    }
                ),
            )
            .with_fix(format!(
                "`pando doctor --adopt {}` moves it under this repository's id, with its \
                 config, its state and its worktrees",
                entry.id
            )),
        );
    }
    found
}
