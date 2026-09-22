//! The worktrees section: each worktree's processes, their phase, and why one
//! failed.

use std::path::Path;

use crate::config::{Config, ServiceConfig};
use crate::paths::PandoPaths;
use crate::process as proc;
use crate::{actions, state};

use super::report::{Finding, ProcessReport, Section, WorktreeReport, WorktreeServiceReport};

pub(super) fn worktrees_report(
    paths: &PandoPaths,
    config: &Config,
    view: &actions::Refreshed,
    findings: &mut Vec<Finding>,
) -> Vec<WorktreeReport> {
    if let Some(warning) = &view.warning {
        findings.push(Finding::problem(
            Section::Worktrees,
            format!("pando's state file cannot be used: {warning}"),
            "move it aside to start over — worktrees pando created will then read as adopted",
        ));
    }
    let listed = crate::worktree::discover(&paths.project).unwrap_or_default();
    let declared = declared_services(config);

    let mut out = Vec::new();
    for (name, record) in &view.state.worktrees {
        let git = listed.iter().find(|w| &w.name == name);
        let phase = state::aggregate_phase(record);
        let mut processes = Vec::new();
        for (process, p) in &record.processes {
            let reason = match &p.phase {
                state::Phase::Failed { reason, .. } => Some(reason.clone()),
                _ => None,
            };
            let hint = failure_hint(&p.log_path, reason.as_deref());
            if let Some(reason) = &reason {
                // Where to look is not always the log. Sending a developer
                // to `pando logs` over a file with nothing in it is the
                // dead end a first run walked into — `doctor` said failed,
                // pointed here, and here said nothing — and the record
                // already knows the difference.
                let where_to_look = match log_has_output(&p.log_path) {
                    true => format!("`pando logs {name} --source {process}` has the last of it"),
                    false => "its log is empty, so there is nothing to read there".to_string(),
                };
                findings.push(Finding::problem(
                    Section::Worktrees,
                    format!("{name}: the process {process:?} failed — {reason}"),
                    match &hint {
                        Some(hint) => format!("{hint}\n{where_to_look}"),
                        None => format!("{where_to_look}; `pando start {name}` tries again"),
                    },
                ));
            }
            processes.push(ProcessReport {
                name: process.clone(),
                phase: match p.phase {
                    state::Phase::Starting { .. } => "starting",
                    state::Phase::Running { .. } => "running",
                    state::Phase::Failed { .. } => "failed",
                },
                reason,
                hint,
                log: p.log_path.display().to_string(),
            });
        }

        let mut services = Vec::new();
        for status in actions::service_statuses(record) {
            let is_declared = declared.contains(&status.name);
            if status.up && !status.logging {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: the service {:?} is up and nothing is filling its log — \
                             its log pump died",
                            status.name
                        ),
                    )
                    .with_fix(format!(
                        "`pando start {name}` puts it back; a read path never respawns one"
                    )),
                );
            }
            if !is_declared {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: pando still has a record for the service {:?}, which this \
                             project's config no longer includes — its container and its volume \
                             are still there",
                            status.name
                        ),
                    )
                    .with_fix(format!(
                        "`pando rm {name}` takes them down with the worktree, or `pando start \
                         {name} --shared` stops them and leaves the data"
                    )),
                );
            }
            services.push(WorktreeServiceReport {
                name: status.name,
                port: status.port,
                up: status.up,
                logging: status.logging,
                declared: is_declared,
            });
        }

        match git {
            None => findings.push(
                Finding::note(
                    Section::Worktrees,
                    format!(
                        "{name}: pando has a record for it and git does not list it — the \
                         directory is gone, or `git worktree prune` has been run"
                    ),
                )
                .with_fix(format!("`pando rm {name}` forgets it")),
            ),
            Some(git) => {
                if git.prunable {
                    findings.push(
                        Finding::note(
                            Section::Worktrees,
                            format!(
                                "{name}: git calls this worktree prunable{}",
                                match &git.prunable_reason {
                                    Some(reason) => format!(" — {reason}"),
                                    None => String::new(),
                                }
                            ),
                        )
                        .with_fix(format!("`pando rm {name}`, or `git worktree prune`")),
                    );
                }
                if git.locked {
                    findings.push(
                        Finding::note(
                            Section::Worktrees,
                            format!(
                                "{name}: git has this worktree locked, so `pando rm` refuses it"
                            ),
                        )
                        .with_fix(format!("`git worktree unlock {}`", git.path.display())),
                    );
                }
            }
        }
        if let Some(share) = &record.share {
            let tunnel = proc::is_alive(share.tunnel_pid);
            let proxy = share.proxy_pid.map(proc::is_alive).unwrap_or(true);
            if !tunnel || !proxy {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: half of its share is gone — {} is not running, and {} is \
                             still published",
                            if tunnel { "the proxy" } else { "the tunnel" },
                            share.public_url
                        ),
                    )
                    .with_fix(format!("`pando unshare {name}` takes the rest of it down")),
                );
            }
        }

        out.push(WorktreeReport {
            name: name.clone(),
            path: record.path.display().to_string(),
            phase: phase
                .as_ref()
                .map(state::Aggregate::word)
                .unwrap_or("stopped"),
            created_by_pando: record.created_by_pando,
            isolated: record.isolated,
            locked: git.is_some_and(|g| g.locked),
            prunable: git.is_some_and(|g| g.prunable),
            prunable_reason: git.and_then(|g| g.prunable_reason.clone()),
            known_to_git: git.is_some(),
            processes,
            services,
        });
    }
    // A worktree git lists that pando has no record of is not reported
    // here: `pando ls` shows those, and this section is about what pando
    // is carrying.
    out
}

/// What the tail of a failed process's log says now, when the record's own
/// reason does not already say it.
///
/// The reason usually carries the hint already — the read path writes it in
/// when the failure is first seen. A record whose log only became
/// explanatory afterwards has nothing, and that is the case worth reading
/// the file for.
/// Whether the log has anything in it to send anyone to.
///
/// Bytes, not lines: a process killed mid-line wrote something worth
/// reading, and `pando logs` prints it.
fn log_has_output(log: &Path) -> bool {
    std::fs::metadata(log).is_ok_and(|m| m.len() > 0)
}

fn failure_hint(log: &Path, reason: Option<&str>) -> Option<String> {
    let reason = reason?;
    let lines =
        crate::log_tail::snapshot(log, crate::log_tail::FAILURE_TAIL_LINES).unwrap_or_default();
    let hint = crate::observe::classify_failure(&lines)?;
    (!reason.contains(&hint.hint)).then_some(hint.hint)
}

/// Every service name the config declares, whatever kind it is.
pub(super) fn declared_services(config: &Config) -> Vec<String> {
    config
        .services
        .iter()
        .flat_map(|service| match service {
            ServiceConfig::Compose { include, .. } => include.clone(),
            ServiceConfig::Native { name, .. } => vec![name.clone()],
        })
        .collect()
}
