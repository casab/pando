//! The runtime the project asks for.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, Config};
use crate::detect::{self, Slot};
use crate::paths::PandoPaths;
use crate::process as proc;

use super::questions::{
    Answer, Answerer, Ask, Pending, RefusedAnswer, answered_by, pending_decision, pick,
    question_for, slot_label,
};

/// What the runtime check needs from outside this process: a shell to ask,
/// and the home directory version managers install themselves into.
///
/// Injected rather than read, so a test can answer for a machine it does
/// not have.
pub struct Machine<'a> {
    pub shell: crate::runtime::Shell<'a>,
    pub home: PathBuf,
}

/// How long one probe gets. It is a login shell that may source a version
/// manager, so it is not instant — and it is cached against the
/// requirement, so it is not often either.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// A shell that runs what a spawn runs: `bash -lc`, in the main checkout.
/// Its output is captured rather than inherited, so nothing here can paint
/// over the TUI, and `run_captured` bounds the wait as well as the drain.
pub fn runtime_shell(cwd: &Path) -> impl Fn(&str) -> Option<String> {
    move |command: &str| {
        let captured = proc::run_captured(command, cwd, &[], PROBE_TIMEOUT).ok()?;
        Some(format!("{}\n{}", captured.stdout, captured.stderr))
    }
}

/// The developer's home, which is where version managers live.
///
/// Not pando's home: `~/.pando` is where pando writes, `~/.nvm` is where
/// nvm is.
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// What the probe found, and what there is to do about it.
pub(super) enum RuntimeOutcome {
    /// Nothing to say: nothing pinned, a match, or something this build
    /// cannot judge. Silence is the common case and the right one.
    Fine,
    /// A mismatch with no prelude set: a question, with the lines that
    /// would fix it and the report that makes it answerable.
    Ask {
        proposal: detect::Proposal,
        report: Vec<String>,
    },
    /// A mismatch with a prelude already set. The prelude is not working,
    /// and nothing but the developer can say what should replace it.
    Broken(String),
}

/// Asks the shell pando will spawn in what it resolves, and compares it to
/// what the repository asks for.
///
/// Three outcomes, and the one that matters is the second: a process
/// started under a runtime the project rejects dies of it, and the whole
/// point of asking first is not to start it.
pub(super) fn resolve_runtime(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
) -> Result<RuntimeOutcome> {
    let prelude = match config.runtime.prelude.as_deref() {
        // An answer, and the one that means "this machine needs nothing".
        // The same distinction `ports = []` makes: unset is nobody having
        // said, empty is somebody having said no.
        Some(prelude) if prelude.trim().is_empty() => return Ok(RuntimeOutcome::Fine),
        Some(prelude) => prelude.trim(),
        None => "",
    };
    let requirements =
        crate::runtime::requirements_for(paths.root(), &config.runtime.version_files);
    let Some(check) = first_mismatch(paths, config, &requirements, prelude, machine.shell)? else {
        return Ok(RuntimeOutcome::Fine);
    };
    if prelude.is_empty() {
        let report = runtime_report(&check, &requirements, prelude, &machine.home, None);
        Ok(RuntimeOutcome::Ask {
            proposal: prelude_proposal(&check, &machine.home),
            report,
        })
    } else {
        let origin = config::prelude_origin(paths);
        let report = runtime_report(&check, &requirements, prelude, &machine.home, origin);
        Ok(RuntimeOutcome::Broken(report.join("\n  ")))
    }
}

/// The first language whose requirement this machine definitely does not
/// meet, probing at most once per language and directory and remembering
/// the ones that passed.
///
/// Only passes are remembered: a cached failure would go on reporting a
/// problem the developer has just fixed, and a failure stops the start
/// anyway, so there is no spawn to save by keeping it.
fn first_mismatch(
    paths: &PandoPaths,
    config: &Config,
    requirements: &[crate::runtime::Requirement],
    prelude: &str,
    shell: crate::runtime::Shell<'_>,
) -> Result<Option<crate::runtime::Check>> {
    if requirements.is_empty() {
        return Ok(None);
    }
    let cache_file = paths.runtime_cache_file();
    let mut cache = crate::runtime::load_cache(&cache_file);
    let mut learned = false;
    let mut mismatch = None;
    // The pin if the project has one, else whatever range it stated: that
    // is what `for_language` sorted to the front — at the root, and in each
    // app directory whose version file config names.
    for requirement in crate::runtime::to_compare(requirements) {
        let Some(language) = crate::runtime::language(&requirement.language) else {
            continue;
        };
        let fingerprint = crate::runtime::fingerprint(requirement, prelude);
        if cache.holds(&fingerprint) {
            continue;
        }
        // Where its processes run, which is where a runner's lockfile is.
        let dir = match &requirement.dir {
            Some(dir) => paths.root().join(dir),
            None => paths.root().to_path_buf(),
        };
        if runs_through_runner(&dir, config, language, prelude, shell) {
            continue;
        }
        let check = crate::runtime::check(requirement, prelude, shell);
        match check.verdict {
            crate::runtime::Verdict::Satisfied => {
                cache.remember(
                    fingerprint,
                    check.resolved.version.clone().unwrap_or_default(),
                );
                learned = true;
            }
            crate::runtime::Verdict::Mismatch => {
                mismatch = Some(check);
                break;
            }
            // A spec this build cannot evaluate, a probe that could not
            // run, an output that was not a version: never a reason to
            // stop anything, and never cached either.
            crate::runtime::Verdict::Unknown => {}
        }
    }
    if learned {
        // The home is created through the one function that makes it 0700
        // and refuses it inside the repository, rather than by a cache
        // write that would know neither rule.
        paths.ensure_home()?;
        // Best effort. The cache only saves the next start a probe, and
        // this start's check has already passed: a save that fails must
        // not fail the start that learnt what it would have saved.
        let _ = crate::runtime::save_cache(&cache_file, &cache);
    }
    Ok(mismatch)
}

/// Whether every command this project runs in `language` goes through a
/// runner that resolves the interpreter itself, and this shell has it.
///
/// `uv run` reads `.python-version` and finds or installs that Python, so
/// what `bash -lc` resolves on its own is beside the point — and asking
/// for a prelude line to fix it was a dead end on any machine with no
/// pyenv. Only when the project uses the runner (its lockfile is there),
/// every configured process and hook goes through it (nothing configured yet counts,
/// since detection proposes the runner's form), and the shell finds it.
///
/// Public because `doctor` reports a mismatch only where a start would act
/// on one, and a start skips the language this answers true for.
pub fn runs_through_runner(
    root: &Path,
    config: &Config,
    language: &crate::runtime::Language,
    prelude: &str,
    shell: crate::runtime::Shell<'_>,
) -> bool {
    let Some(runner) = language.runner(root) else {
        return false;
    };
    let Some(prefix) = runner.run_prefix.map(str::trim) else {
        return false;
    };
    // Hooks too, fallbacks included: a migration hook that runs `python`
    // bare gets whatever interpreter the shell resolves, exactly as a
    // process would, and it runs first.
    // Not one switched off: `on = "never"` is how the schema question's
    // "no" is written down, and a command that never runs needs no
    // interpreter.
    let hooks = config
        .hooks
        .iter()
        .filter(|hook| hook.on != Some(config::HookScope::Never))
        .flat_map(|hook| std::iter::once(hook.cmd.as_str()).chain(hook.fallback.as_deref()));
    let through = config
        .processes
        .values()
        .map(|process| process.cmd.as_str())
        .chain(hooks)
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
        .all(|cmd| cmd.starts_with(prefix));
    if !through {
        return false;
    }
    const OK: &str = "pando-runner-ok";
    let probe = format!("command -v {} >/dev/null && echo {OK}", runner.program);
    let probe = match prelude {
        "" => probe,
        prelude => format!("{prelude} && {probe}"),
    };
    shell(&probe).is_some_and(|out| out.lines().any(|line| line.trim() == OK))
}

/// The lines that make a mismatch answerable.
///
/// Deliberately concrete about the path: pando's shell is not the
/// developer's shell, and "node 24" is not a diagnosis when everything
/// works when they type it by hand. Where the binary came from is.
fn runtime_report(
    check: &crate::runtime::Check,
    requirements: &[crate::runtime::Requirement],
    prelude: &str,
    home: &Path,
    origin: Option<PathBuf>,
) -> Vec<String> {
    let requirement = &check.requirement;
    let language = &requirement.language;
    let mut lines: Vec<String> = Vec::new();

    if !prelude.is_empty() {
        lines.push(match &origin {
            Some(path) => format!("the prelude in {} is not working", path.display()),
            None => "that line does not work".to_string(),
        });
        lines.push(format!("prelude: {prelude}"));
    }
    lines.push(format!(
        "this project asks for {language} {} ({})",
        requirement.spec, requirement.source
    ));
    for other in requirements
        .iter()
        .filter(|r| r.language == *language && r.source != requirement.source)
    {
        lines.push(format!("{} also asks for {}", other.source, other.spec));
    }
    if !check.resolved.ran {
        lines.push(match &check.resolved.failure {
            Some(failure) => format!("the prelude itself failed: {failure}"),
            None => "the prelude itself failed".to_string(),
        });
    } else {
        lines.push(
            match (
                &check.resolved.version,
                &check.resolved.path,
                &check.resolved.failure,
            ) {
                (Some(version), Some(path), _) => {
                    format!("`bash -lc` here resolves {language} {version}, from {path}")
                }
                (None, Some(path), Some(failure)) => {
                    format!("`bash -lc` here finds {language} at {path}, and it fails: {failure}")
                }
                _ => format!("`bash -lc` here has no {language} at all"),
            },
        );
    }
    lines.push(
        "pando runs every command with `bash -lc`, which is not your interactive shell".to_string(),
    );

    let Some(entry) = crate::runtime::language(language) else {
        return lines;
    };
    let installed = crate::runtime::installed(entry, home);
    lines.push(match installed.as_slice() {
        [] => format!("no version manager pando knows about is installed for {language}"),
        managers => format!(
            "version managers installed here: {}",
            managers
                .iter()
                .map(|m| m.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    });
    // Printed, never run. Installing a toolchain is the developer's
    // decision to make on their own machine.
    if let Some(command) = installed
        .first()
        .and_then(|manager| manager.install_command(entry, &requirement.spec))
    {
        lines.push(format!(
            "if {language} {} is not installed yet: {command} — pando never installs one",
            requirement.spec
        ));
    }
    lines
}

/// The prelude lines worth offering, for the managers this machine has.
fn prelude_proposal(check: &crate::runtime::Check, home: &Path) -> detect::Proposal {
    let requirement = &check.requirement;
    let candidates = crate::runtime::language(&requirement.language)
        .map(|language| crate::runtime::fixes(language, home, requirement))
        .unwrap_or_default()
        .into_iter()
        .map(|fix| detect::Candidate {
            value: fix.line,
            why: fix.why,
            ..detect::Candidate::default()
        })
        .collect();
    // Never decided. What one machine needs is not something a rule gets to
    // settle on a developer's behalf, and the answer lands in a file every
    // project on that machine shares.
    detect::Proposal::of(Slot::Prelude, candidates, false)
}

/// Asks the prelude question, checks the answer, and writes it to the user
/// layer.
///
/// Checked *before* it is written, not after: this line applies to every
/// project on the machine, and `--yes` must not be able to persist one
/// that does not work into a file nobody looked at.
pub(super) fn answer_prelude(
    paths: &PandoPaths,
    config: &mut Config,
    proposal: &detect::Proposal,
    report: &[String],
    ask: Ask<'_>,
    machine: &Machine<'_>,
) -> Result<Option<Pending>> {
    let question = question_for(proposal, report).at(paths);
    let offered = question.options.len();
    let (answer, by) = answered_by(ask(&question)?);
    // Before the match takes it apart. The caller writes it down once the
    // line is on disk — and only if it gets there, since a prelude that
    // fails its own probe is refused below.
    let pending = pending_decision(&question, proposal, &answer, by);
    let (line, note) = match answer {
        Answer::Choice(index) => {
            let candidate = pick(proposal, index)?;
            let why = candidate.why.clone();
            (candidate.value, by.note(config::Note::Detected(why)))
        }
        Answer::Auto(index) => (
            pick(proposal, index)?.value,
            config::Note::TookFirst(offered),
        ),
        Answer::Custom(value) => (value.trim().to_string(), by.note(config::Note::Answered)),
        // "This machine needs nothing." Recorded as an empty prelude
        // rather than left unset, so it is never asked again — the shape
        // the port slot already uses for a process with no ports.
        Answer::None => (String::new(), by.note(config::Note::Answered)),
        Answer::Many(_) | Answer::Processes(_) => bail!(
            "{} is one line, not several of them",
            slot_label(Slot::Prelude)
        ),
        Answer::Program(_) => unreachable!("answered_by peels every wrapper"),
    };
    if !line.is_empty() {
        let mut proposed = config.clone();
        proposed.runtime.prelude = Some(line.clone());
        if let RuntimeOutcome::Broken(report) = resolve_runtime(paths, &proposed, machine)? {
            // A program handed this line in, so it is the program's input
            // that is wrong: the usage-error shape, not a failure.
            if by == Answerer::Program {
                return Err(anyhow::Error::new(RefusedAnswer(report.to_string())));
            }
            bail!("{report}");
        }
    }
    let (table, key) = Slot::Prelude
        .key()
        .expect("the prelude slot writes one key");
    config::set_detected(paths, Slot::Prelude.layer(), table, key, line.clone(), note)?;
    config.runtime.prelude = Some(line);
    Ok(pending)
}

/// `[runtime].prelude`, when set, runs before every command pando starts, so
/// a version manager sourced from a login profile is in effect. The shell is
/// `bash -lc`, so `.bash_profile` is read and `.zshrc` is not.
pub fn with_prelude(config: &Config, cmd: &str) -> String {
    match config.runtime.prelude.as_deref().map(str::trim) {
        // Grouped, so the `&&` guards the whole command: `a; b` after a
        // failed prelude ran `b` anyway. The newline before `}` lets a
        // command end in a comment. A group, not a subshell, so `exec`
        // still replaces the shell pando records.
        Some(prelude) if !prelude.is_empty() => format!("{prelude} && {{\n{cmd}\n}}"),
        _ => cmd.to_string(),
    }
}
