//! The report as text: the verdict, every finding with its fix, one block
//! per section, and the verdict again.

use std::fmt::Write as _;

use crate::term::{Paint, Style};

use super::adopt::Adoptable;
use super::report::{
    ConfigReport, Finding, HookReport, KeyReport, ProjectReport, Report, RuntimeReport, Section,
    ServicesReport, Severity, ToolReport, WorktreeReport,
};

impl Report {
    /// Whether nothing found will break a command. The exit code, and the
    /// only thing that decides it.
    pub fn healthy(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| f.severity == Severity::Problem)
    }

    fn of(&self, section: Section) -> Vec<&Finding> {
        let mut out: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.section == section)
            .collect();
        // Problems first inside a section: the thing that is broken should
        // not be below three lines about something that merely is.
        out.sort_by(|a, b| b.severity.cmp(&a.severity));
        out
    }

    /// The report as plain text: no colour, paths as they are.
    pub fn render(&self) -> String {
        self.render_with(&Style::plain())
    }

    /// The report for a reader: what is wrong first, each finding tagged
    /// with its section and carrying its fix, then the facts section by
    /// section, then, when anything was found, the verdict again as the
    /// last line — the one left on screen after a long report scrolls.
    pub fn render_with(&self, style: &Style) -> String {
        let mut out = String::new();
        render_summary(&mut out, &self.findings, style);
        for (severity, title) in [(Severity::Problem, "problems"), (Severity::Note, "notes")] {
            let findings: Vec<&Finding> = Section::ALL
                .iter()
                .flat_map(|section| self.of(*section))
                .filter(|f| f.severity == severity)
                .collect();
            if findings.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n{}", style.paint(title, Paint::Heading));
            for finding in findings {
                render_finding(&mut out, finding, style);
            }
        }
        for section in Section::ALL {
            let _ = writeln!(out, "\n{}", style.paint(section.title(), Paint::Heading));
            match section {
                Section::Project => render_project(&mut out, &self.project, style),
                Section::Config => render_config(&mut out, &self.config, style),
                Section::Runtime => render_runtime(&mut out, &self.runtime),
                Section::Tools => render_tools(&mut out, &self.tools, style),
                Section::Worktrees => render_worktrees(&mut out, &self.worktrees, style),
                Section::Services => render_services(&mut out, &self.services),
                Section::Hooks => render_hooks(&mut out, &self.hooks),
                Section::Adoption => render_adoption(&mut out, &self.adoption, style),
            }
        }
        // The verdict again only when there is one worth scrolling back
        // to: "nothing to report" at both ends of a report that reported
        // nothing read as two reports.
        if !self.findings.is_empty() {
            out.push('\n');
            render_summary(&mut out, &self.findings, style);
        }
        out
    }
}

fn render_finding(out: &mut String, finding: &Finding, style: &Style) {
    let mark = match finding.severity {
        Severity::Problem => style.paint("!", Paint::Bad),
        Severity::Note => style.paint("-", Paint::Warn),
    };
    let section = style.paint(&format!("[{}]", finding.section.title()), Paint::Faint);
    let _ = writeln!(out, "  {mark} {section} {}", finding.message);
    let Some(fix) = &finding.fix else { return };
    // A fix can be several lines — the prelude candidates for a runtime
    // mismatch are one line each — and each of them is a line a developer
    // may want to copy, so none of them is folded into the one above.
    for (index, line) in fix.lines().enumerate() {
        match index {
            0 => {
                let _ = writeln!(out, "      {} {line}", style.paint("fix:", Paint::Good));
            }
            _ => {
                let _ = writeln!(out, "           {line}");
            }
        }
    }
}

fn render_summary(out: &mut String, findings: &[Finding], style: &Style) {
    let problems = findings
        .iter()
        .filter(|f| f.severity == Severity::Problem)
        .count();
    let notes = findings.len() - problems;
    if problems == 0 && notes == 0 {
        let _ = writeln!(out, "{}", style.paint("nothing to report", Paint::Good));
        return;
    }
    let problems_text = format!("{problems} {}", plural(problems, "problem"));
    let problems_text = match problems {
        0 => style.paint(&problems_text, Paint::Good),
        _ => style.paint(&problems_text, Paint::Bad),
    };
    let _ = writeln!(out, "{problems_text}, {notes} {}", plural(notes, "note"));
}

pub(super) fn plural(n: usize, word: &str) -> String {
    match n {
        1 => word.to_string(),
        _ => format!("{word}s"),
    }
}

/// A fact row: a label, padded, and its value.
///
/// A label longer than the column — a project id is — still gets two
/// spaces after it rather than running into what it is labelling.
fn row(out: &mut String, label: &str, value: &str) {
    const COLUMN: usize = 14;
    match label.chars().count() < COLUMN {
        true => {
            let _ = writeln!(out, "  {label:<COLUMN$}{value}");
        }
        false => {
            let _ = writeln!(out, "  {label}  {value}");
        }
    }
}

fn render_project(out: &mut String, project: &ProjectReport, style: &Style) {
    row(out, "id", &project.id);
    row(out, "root", &style.tilde(&project.root));
    row(out, "home", &style.tilde(&project.home));
    row(out, "worktrees", &style.tilde(&project.worktrees_dir));
    row(
        out,
        "home mode",
        &match &project.home_mode {
            Some(mode) => mode.clone(),
            None => "not created yet".to_string(),
        },
    );
    row(
        out,
        "ports",
        &format!(
            "{}-{} in windows of {}; {} bases on this machine, {} held here",
            project.port_min,
            project.port_max,
            project.base_step,
            project.bases_in_range,
            project.windows_held
        ),
    );
}

/// How wide a key column is allowed to get before the notes stop lining up
/// and start pushing the line over a terminal's edge.
const KEY_COLUMN_MAX: usize = 56;

fn render_config(out: &mut String, config: &ConfigReport, style: &Style) {
    for layer in &config.layers {
        let suffix = match (&layer.error, layer.present) {
            (Some(e), _) => style.paint(&format!(" — {e}"), Paint::Bad),
            (None, false) => style.paint(" — not there", Paint::Faint),
            (None, true) if layer.keys.is_empty() => " — empty".to_string(),
            (None, true) => String::new(),
        };
        let _ = writeln!(
            out,
            "  {:<10}{}{suffix}",
            layer.layer,
            style.tilde(&layer.path)
        );
        let width = layer
            .keys
            .iter()
            .map(|k| lhs(k).chars().count())
            .max()
            .unwrap_or(0)
            .min(KEY_COLUMN_MAX);
        for key in &layer.keys {
            let text = lhs(key);
            match &key.note {
                Some(note) => {
                    let _ = writeln!(out, "      {text:<width$}  {note}");
                }
                None => {
                    let _ = writeln!(out, "      {text}");
                }
            }
        }
    }
}

fn render_runtime(out: &mut String, runtime: &RuntimeReport) {
    row(
        out,
        "prelude",
        &match (&runtime.prelude, &runtime.prelude_from) {
            (None, _) => "nobody has answered that question yet".to_string(),
            (Some(line), _) if line.trim().is_empty() => {
                "\"\" — answered: this machine needs nothing in front of a command".to_string()
            }
            (Some(line), Some(from)) => format!("{line}  (from {from})"),
            (Some(line), None) => line.clone(),
        },
    );
    if runtime.languages.is_empty() && runtime.requirements.is_empty() {
        row(out, "pinned", "nothing — this repository states no runtime");
        return;
    }
    for language in &runtime.languages {
        row(
            out,
            &language.language,
            &format!("wants {} ({})", language.spec, language.source),
        );
        let has = match (
            &language.resolved,
            &language.resolved_from,
            &language.failure,
        ) {
            (Some(version), Some(path), _) => {
                format!("`bash -lc` resolves {version}, from {path}")
            }
            (Some(version), None, _) => format!("`bash -lc` resolves {version}"),
            (None, Some(path), Some(failure)) => {
                format!("`bash -lc` finds {path}, and it fails: {failure}")
            }
            (None, _, Some(failure)) => format!("the probe never ran: {failure}"),
            (None, _, None) => format!("`bash -lc` here has no {} at all", language.language),
        };
        row(out, "", &has);
        row(
            out,
            "",
            &match language.managers.as_slice() {
                [] => "no version manager pando knows about is installed for it".to_string(),
                managers => format!("version managers here: {}", managers.join(", ")),
            },
        );
    }
    // A requirement with no language entry behind it is still a fact the
    // repository states, and an agent reading this should see it.
    for requirement in &runtime.requirements {
        if runtime
            .languages
            .iter()
            .any(|l| l.language == requirement.language)
        {
            continue;
        }
        row(
            out,
            &requirement.language,
            &format!(
                "wants {} ({}) — nothing here probes it",
                requirement.spec, requirement.source
            ),
        );
    }
}

fn render_tools(out: &mut String, tools: &[ToolReport], style: &Style) {
    if tools.is_empty() {
        row(out, "", "nothing to look for");
        return;
    }
    // Two spaces past the longest name, so `docker compose` does not run
    // into its own version number.
    let width = tools
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12)
        + 2;
    for tool in tools {
        let mut text = match (&tool.version, &tool.path) {
            // `command -v` prints a bare word for a shell builtin, which
            // is not a path and is not a version to ask for either.
            (_, Some(path)) if !path.starts_with('/') => format!("a shell builtin ({path})"),
            (Some(version), Some(path)) => format!("{version}  ({})", style.tilde(path)),
            (None, Some(path)) => format!("found, and said nothing  ({})", style.tilde(path)),
            // A shell that never answered has not said it is missing.
            _ if !tool.asked => style.paint(
                &format!("no answer from the shell — {}", tool.needed_for),
                Paint::Faint,
            ),
            _ => style.paint(&format!("not found — {}", tool.needed_for), Paint::Warn),
        };
        if let Some(detail) = &tool.detail {
            text.push_str(&format!("  {detail}"));
        }
        let _ = writeln!(out, "  {:<width$}{text}", tool.name);
    }
}

fn render_worktrees(out: &mut String, worktrees: &[WorktreeReport], style: &Style) {
    if worktrees.is_empty() {
        row(out, "", "none — `pando new <branch>` makes one");
        return;
    }
    for worktree in worktrees {
        let mut flags: Vec<&str> = vec![match (worktree.main, worktree.created_by_pando) {
            (true, _) => "main checkout",
            (false, true) => "pando-created",
            (false, false) => "adopted",
        }];
        if worktree.mode != crate::state::ServiceMode::Shared {
            flags.push(worktree.mode.word());
        }
        if worktree.locked {
            flags.push("locked");
        }
        if worktree.prunable {
            flags.push("prunable");
        }
        if !worktree.known_to_git {
            flags.push("git does not list it");
        }
        row(
            out,
            &worktree.name,
            &format!(
                "{}  [{}]",
                style.paint(worktree.phase, phase_paint(worktree.phase)),
                flags.join(", ")
            ),
        );
        row(out, "", &style.tilde(&worktree.path));
        for process in &worktree.processes {
            row(
                out,
                "",
                &match &process.reason {
                    Some(reason) => format!("{}: {} — {reason}", process.name, process.phase),
                    None => format!("{}: {}", process.name, process.phase),
                },
            );
        }
        for service in &worktree.services {
            let port = match service.port {
                Some(port) => port.to_string(),
                None => "no port".to_string(),
            };
            let up = match service.up {
                true => "answering",
                false => "not answering",
            };
            let pump = match service.logging {
                true => "",
                false => ", no log pump",
            };
            let dropped = match service.declared {
                true => "",
                false => ", config no longer includes it",
            };
            row(
                out,
                "",
                &format!("{}: {port}, {up}{pump}{dropped}", service.name),
            );
        }
    }
}

fn render_services(out: &mut String, services: &ServicesReport) {
    let isolation = &services.isolation;
    match &isolation.mechanism {
        Some(mechanism) => row(
            out,
            "isolation",
            &match isolation.answered {
                true => format!("{mechanism}, as this project's config already says"),
                false => format!("{mechanism}, if this worktree is started isolated"),
            },
        ),
        None => row(out, "isolation", "nothing here to run a private copy of"),
    }
    // Never a bare verdict: the facts that decided it, in the order they
    // were weighed.
    for line in &isolation.evidence {
        row(out, "", line);
    }
    if services.compose.is_empty() && services.native.is_empty() {
        // Not "none configured" after a line that just explained what
        // *would* run: the two together read as a contradiction.
        row(
            out,
            "",
            match isolation.mechanism.is_some() {
                true => "nothing is written down yet — an isolated start is what writes it",
                false => "none configured",
            },
        );
        return;
    }
    for entry in &services.compose {
        row(
            out,
            "compose",
            &match (&entry.error, entry.file_exists) {
                (Some(e), _) => format!("{} — {e}", entry.file),
                (None, false) => format!("{} — not in this repository", entry.file),
                (None, true) => entry.file.clone(),
            },
        );
        for service in &entry.services {
            let env = match &service.env_key {
                Some(key) => format!(", addressed by {key}"),
                None => ", nothing in the env example points at it".to_string(),
            };
            row(
                out,
                "",
                &match service.declared {
                    true => format!("{}: ready by {}{env}", service.name, service.ready),
                    false => format!("{}: this compose file does not declare it", service.name),
                },
            );
        }
        if !entry.extends.is_empty() {
            row(
                out,
                "",
                &format!("`extends:` on {} — not followed", entry.extends.join(", ")),
            );
        }
        if entry.include {
            row(out, "", "a top-level `include:` — not followed");
        }
    }
    for native in &services.native {
        row(
            out,
            "native",
            &match (&native.error, &native.source) {
                (Some(e), _) => format!("{}: {e}", native.name),
                (None, Some(source)) => format!(
                    "{}: the {:?} recipe, {source}{}",
                    native.name,
                    native.preset,
                    match native.overrides.is_empty() {
                        true => String::new(),
                        false => format!(" ({} from this entry)", native.overrides.join(", ")),
                    }
                ),
                (None, None) => native.name.clone(),
            },
        );
        if native.error.is_some() {
            continue;
        }
        for binary in &native.engine {
            row(
                out,
                "",
                &match &binary.path {
                    Some(path) => format!("{} at {path}", binary.name),
                    None if !native.engine_asked => {
                        format!("{}: no answer from the shell", binary.name)
                    }
                    None => format!("{} is not on PATH", binary.name),
                },
            );
        }
        if let Some(version) = &native.version {
            row(out, "", version);
        }
        if let Some(install) = &native.install
            && native.engine_asked
            && native.engine.iter().any(|b| b.path.is_none())
        {
            row(
                out,
                "",
                &format!("install it with: {install} — pando never will"),
            );
        }
        row(out, "", &format!("data in {}", native.datadir));
        row(out, "", &format!("sockets under {}", native.socket_root));
        match &native.env_key {
            Some(key) => row(out, "", &format!("addressed by {key}")),
            None => row(out, "", "nothing in the env points at it"),
        }
        if native.untested {
            row(
                out,
                "",
                "this recipe has never been run against a real server",
            );
        }
        if let Some(notes) = &native.notes {
            row(out, "", notes);
        }
        for instance in &native.instances {
            row(
                out,
                "",
                &format!(
                    "{}: {}",
                    instance.worktree,
                    match &instance.initialised {
                        Some(how) => format!("{how}, socket in {}", instance.socket_dir),
                        None =>
                            "a data directory with no marker — the next start adopts it as it is"
                                .to_string(),
                    }
                ),
            );
        }
    }
}

fn render_hooks(out: &mut String, hooks: &[HookReport]) {
    if hooks.is_empty() {
        row(out, "", "none configured");
        return;
    }
    for hook in hooks {
        row(
            out,
            &hook.name,
            &format!("after {}: {}", hook.after, hook.cmd),
        );
        row(
            out,
            "",
            &match (&hook.matches, hook.fingerprint.is_empty()) {
                (_, true) => "keyed on nothing, so it runs on every start".to_string(),
                (Some(0) | None, false) => format!(
                    "keyed on {}, which matches nothing here",
                    hook.fingerprint.join(", ")
                ),
                (Some(n), false) => format!(
                    "keyed on {}, matching {n} {}",
                    hook.fingerprint.join(", "),
                    plural(*n, "file")
                ),
            },
        );
        for run in &hook.runs {
            row(
                out,
                "",
                &format!(
                    "{}: ran {}{}",
                    run.worktree,
                    run.ran_at.format("%Y-%m-%d %H:%M"),
                    match run.will_run_again {
                        true => ", and will run again — its inputs changed",
                        false => "",
                    }
                ),
            );
        }
    }
}

fn render_adoption(out: &mut String, adoption: &[Adoptable], style: &Style) {
    if adoption.is_empty() {
        row(out, "", "nothing to adopt");
        return;
    }
    for entry in adoption {
        row(out, &entry.id, &style.tilde(&entry.path));
        row(
            out,
            "",
            &match &entry.old_root {
                Some(root) => format!("its repository was at {root}, and is not there now"),
                None => "nothing in it says which repository it belonged to".to_string(),
            },
        );
        if !entry.worktrees.is_empty() {
            row(
                out,
                "",
                &format!(
                    "{} {} still in it: {}",
                    entry.worktrees.len(),
                    plural(entry.worktrees.len(), "worktree"),
                    entry.worktrees.join(", ")
                ),
            );
        }
    }
}

/// How a worktree's phase word looks: the same colours `ls` and `status`
/// use for the same words.
fn phase_paint(phase: &str) -> Paint {
    match phase {
        "running" => Paint::Good,
        "starting" => Paint::Warn,
        "failed" => Paint::Bad,
        _ => Paint::Faint,
    }
}

/// A key row's left-hand side: `key = value`, or a bare table header.
fn lhs(key: &KeyReport) -> String {
    let ignored = if key.ignored { "  (ignored)" } else { "" };
    match &key.value {
        Some(value) => format!("{} = {value}{ignored}", key.key),
        None => format!("{}{ignored}", key.key),
    }
}
