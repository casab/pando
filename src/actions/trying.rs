//! Trying on its own: the setup screen's `⏎` with no agent. pando settles
//! what `new` and a shared start settle, and the base a check asks for, with
//! its own first choices, and nothing more — or, where it cannot, writes
//! nothing at all.

use anyhow::Result;
use chrono::Utc;

use crate::config::Config;
use crate::detect::Slot;
use crate::paths::PandoPaths;
use crate::setup::SetupMemory;

use super::init::{Scratch, seed_scratch};
use super::questions::{
    Answer, Answering, NEW_SLOTS, NeedsAnswer, Question, SILENT_UNLESS_ISOLATED, START_SLOTS,
    recommended, resolve_on,
};
use super::runtime::{Machine, RuntimeOutcome, resolve_runtime, runtime_shell};

/// What pando's own guess came to.
#[derive(Debug)]
pub enum OwnGuess {
    /// Every slot it tried is answered and on disk, and `setup.json` says
    /// whose guess it was: the config with the answers in it.
    Saved(Box<Config>),
    /// A question pando has no option for, such as a dev command it
    /// found nothing to offer. Nothing was written.
    NoOption(Slot),
    /// Every question had an answer and there is still nothing to run.
    /// Nothing was written.
    NothingToRun,
    /// The runtime the project asks for is not the one pando's shell
    /// finds, and what fixes that is a prelude line: machine-wide, so
    /// never taken on the developer's behalf. Also a prelude that is set
    /// and does not work. Nothing was written; these are the lines the
    /// prelude question would have carried.
    NeedsPrelude(Vec<String>),
}

/// [`try_on_its_own_on`], on this machine.
pub fn try_on_its_own(
    paths: &PandoPaths,
    config: &Config,
    progress: &dyn Fn(&str),
) -> Result<OwnGuess> {
    let shell = runtime_shell(paths.root());
    let machine = Machine::here(&shell);
    try_on_its_own_on(paths, config, progress, &machine)
}

/// Settles the slots `new` and a shared start settle, and the base the
/// check asks for, each with pando's first choice, so `pando check` has
/// something to test.
///
/// Only those: the services and the schema step are silenced as a shared
/// start silences them, and the prelude is never answered here. It is
/// all or nothing. The pass runs first against a scratch copy of pando's
/// own files, so a question with no option stops it before a single key
/// is written; the runtime is checked against what that pass would
/// write; only then does the same pass run for real, under pando's home
/// alone. Nothing here creates a worktree.
pub fn try_on_its_own_on(
    paths: &PandoPaths,
    config: &Config,
    progress: &dyn Fn(&str),
    machine: &Machine<'_>,
) -> Result<OwnGuess> {
    let slots = tried_slots();
    let quiet = |_: &str| {};

    let scratch = Scratch::new(paths)?;
    let previewed = PandoPaths::new(scratch.dir.clone(), paths.project.clone());
    seed_scratch(
        &[
            (paths.config_file(), previewed.config_file()),
            (paths.user_config_file(), previewed.user_config_file()),
        ],
        &previewed,
    )?;
    let first_choices = |question: &Question| first_choice(question, &quiet);
    let guessed = match resolve_on(
        &previewed,
        config,
        &slots,
        &SILENT_UNLESS_ISOLATED,
        &Answering::asking(&first_choices),
        &quiet,
        machine,
    ) {
        Ok(guessed) => guessed,
        Err(e) => return stopped(e),
    };
    drop(scratch);
    if guessed.runnable_processes().next().is_none() {
        return Ok(OwnGuess::NothingToRun);
    }
    // Of the settings the guess would write, as the start that tests them
    // will ask it.
    match resolve_runtime(paths, &guessed, machine)? {
        RuntimeOutcome::Fine => {}
        RuntimeOutcome::Ask { report, .. } => return Ok(OwnGuess::NeedsPrelude(report)),
        RuntimeOutcome::Broken(report) => {
            let lines = report.lines().map(|line| line.trim().to_string());
            return Ok(OwnGuess::NeedsPrelude(lines.collect()));
        }
    }

    let first_choices = |question: &Question| first_choice(question, progress);
    let config = match resolve_on(
        paths,
        config,
        &slots,
        &SILENT_UNLESS_ISOLATED,
        &Answering::asking(&first_choices),
        progress,
        machine,
    ) {
        Ok(config) => config,
        // Only if the project changed between the two passes.
        Err(e) => return stopped(e),
    };
    // Never fatal: the answers are on disk, and all that is lost is the
    // ready view saying they were pando's guess.
    let mut memory = SetupMemory::load(paths);
    memory.tried_by_pando_at = Some(Utc::now());
    if let Err(e) = memory.save(paths) {
        progress(&format!("could not remember that pando guessed: {e:#}"));
    }
    Ok(OwnGuess::Saved(Box::new(config)))
}

/// `new`'s slots, then a start's, without the prelude, then the base: a
/// check stops at an open one, so a guess that left it would hand the
/// check a question nobody is there to answer.
pub(super) fn tried_slots() -> Vec<Slot> {
    NEW_SLOTS
        .iter()
        .chain(START_SLOTS.iter())
        .copied()
        .filter(|slot| *slot != Slot::Prelude)
        .chain([Slot::Base])
        .collect()
}

/// The rules' first choice, said as a start says it; a question with none
/// stops the pass.
fn first_choice(question: &Question, progress: &dyn Fn(&str)) -> Result<Answer> {
    match recommended(question) {
        Some((answer, line)) => {
            progress(&line);
            Ok(answer)
        }
        None => Err(NeedsAnswer {
            question: question.clone(),
        }
        .into()),
    }
}

/// A pass that stopped: at a question, which is an outcome, or on an
/// error, which is not.
fn stopped(e: anyhow::Error) -> Result<OwnGuess> {
    match e.downcast_ref::<NeedsAnswer>() {
        Some(needs) => Ok(OwnGuess::NoOption(needs.question.slot)),
        None => Err(e),
    }
}
