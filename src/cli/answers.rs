//! `--answers`: the file a program writes instead of a developer typing,
//! and the usage errors it can earn.

use super::notice;
use super::prompt::asker;
use crate::actions;
use crate::config::Config;
use anyhow::Result;
use std::collections::BTreeMap;

// ---- an answers file ------------------------------------------------------

/// A mistake in what was asked for, rather than a failure of what pando
/// tried to do. Exit 2, the code clap's own argument errors use.
///
/// Its own type because `--answers` is a contract a program writes
/// against: "you named a question pando does not ask" has to be
/// distinguishable from "the command failed" without reading English.
#[derive(Debug, Clone)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(message.into()))
}

/// The name a slot goes by in an answers file and in `signals` — derived
/// from the type itself, so the file a program writes and the JSON it read
/// cannot drift apart.
pub fn slot_name(slot: crate::detect::Slot) -> String {
    serde_json::to_value(slot)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .expect("every slot serialises to its name")
}

/// The question an answers file names, among the ones `init` asks. A
/// login is asked by a namespaced start alone, so a file naming it would
/// be a program believing it had answered something nothing reads.
pub(super) fn slot_named(name: &str) -> Option<crate::detect::Slot> {
    serde_json::from_value(serde_json::Value::String(name.to_string()))
        .ok()
        .filter(|slot| actions::ALL_SLOTS.contains(slot))
}

/// Every name an answers file may use, in the order the questions come.
pub(super) fn slot_names() -> Vec<String> {
    actions::ALL_SLOTS.iter().copied().map(slot_name).collect()
}

/// What a program answered, slot by slot.
///
/// The write path an agent uses, so it is a contract rather than a
/// convenience. Nothing here writes TOML: every value goes through the
/// same `resolve` a person's answer does — the same candidates, the same
/// checks, the same file — and lands with a comment saying a program chose
/// it.
#[derive(Debug)]
pub struct Answers {
    by_slot: BTreeMap<crate::detect::Slot, serde_json::Value>,
    /// Which slots a question actually reached, so the ones nothing asked
    /// about can be reported instead of silently dropped.
    asked: std::cell::RefCell<std::collections::BTreeSet<crate::detect::Slot>>,
}

impl Answers {
    /// Parses the file. A name pando does not ask about is a usage error
    /// naming it: a silent skip would leave a program believing it had
    /// answered something.
    pub fn parse(text: &str) -> Result<Answers> {
        let parsed: serde_json::Value =
            serde_json::from_str(text).map_err(|e| usage(format!("--answers is not JSON: {e}")))?;
        let serde_json::Value::Object(object) = parsed else {
            return Err(usage(
                "--answers takes a JSON object of question name to answer",
            ));
        };
        let mut by_slot = BTreeMap::new();
        for (name, value) in object {
            let slot = slot_named(&name).ok_or_else(|| {
                usage(format!(
                    "--answers names {name:?}, which is not a question pando asks — it asks \
                     about: {}",
                    slot_names().join(", ")
                ))
            })?;
            // Checked here, before a single key is written: a shape this
            // slot cannot take is knowable from the slot alone, and a
            // program should learn it from the file it just sent rather
            // than from a half-written config.
            check_shape(slot, &value)?;
            by_slot.insert(slot, value);
        }
        Ok(Answers {
            by_slot,
            asked: std::cell::RefCell::new(std::collections::BTreeSet::new()),
        })
    }

    /// The answer for this question, when the file has one for its slot.
    pub(super) fn for_question(
        &self,
        question: &actions::Question,
    ) -> Option<Result<actions::Answer>> {
        let value = self.by_slot.get(&question.slot)?;
        self.asked.borrow_mut().insert(question.slot);
        Some(answer_from(question, value))
    }

    /// Every slot the file answers.
    pub(super) fn slots(&self) -> Vec<crate::detect::Slot> {
        self.by_slot.keys().copied().collect()
    }

    /// Slots the file answered that no question ever asked about.
    pub(super) fn unasked(&self) -> Vec<crate::detect::Slot> {
        let asked = self.asked.borrow();
        self.by_slot
            .keys()
            .copied()
            .filter(|slot| !asked.contains(slot))
            .collect()
    }
}

/// Which shapes a slot's answer may take, decided from the slot alone.
///
/// Three, and each one means the same thing everywhere: the text of an
/// option or a command of your own, a list for an answer that is a set or
/// a list of files, and null for "none of them". Anything else is a usage
/// error naming the question, never a silent skip — and it is caught when
/// the file is read, so a bad shape on a slot no rule ends up asking
/// about is still reported.
fn check_shape(slot: crate::detect::Slot, value: &serde_json::Value) -> Result<()> {
    let name = slot_name(slot);
    match value {
        serde_json::Value::Null if !slot.allows_none() => Err(usage(format!(
            "{name} has no \"none\" answer — null answers only the questions that have one"
        ))),
        serde_json::Value::Null => Ok(()),
        serde_json::Value::String(text) if text.trim().is_empty() => Err(usage(format!(
            "{name} was answered with an empty string — null is how you say none"
        ))),
        serde_json::Value::String(_) if slot.is_multi() => Err(usage(format!(
            "{name} is answered with a list of the options, or null for none of them"
        ))),
        serde_json::Value::String(_) => Ok(()),
        serde_json::Value::Array(items) => {
            if !slot.is_multi() && !slot.is_list() {
                return Err(usage(format!(
                    "{name} takes one answer, not a list of them"
                )));
            }
            if items.iter().any(|item| !item.is_string()) {
                return Err(usage(format!("{name} takes a list of strings")));
            }
            // An empty list is the recorded answer "none of them" at the
            // set question, and nothing at all at a list of files.
            if !slot.is_multi() && items.is_empty() {
                return Err(usage(format!(
                    "{name} was answered with an empty list — null is how you say none"
                )));
            }
            Ok(())
        }
        other => Err(usage(format!(
            "{name} takes a string, a list of strings, or null — not {other}"
        ))),
    }
}

/// One JSON value, as an answer to one question.
///
/// The shapes are [`check_shape`]'s; what is left here is matching what a
/// program sent against what the rules actually offered.
pub(super) fn answer_from(
    question: &actions::Question,
    value: &serde_json::Value,
) -> Result<actions::Answer> {
    check_shape(question.slot, value)?;
    let name = slot_name(question.slot);
    // Everything from here is a program's answer, which is what the note
    // beside the key it fills has to say.
    let program = |answer| Ok(actions::Answer::Program(Box::new(answer)));
    match value {
        serde_json::Value::Null => program(actions::Answer::None),
        serde_json::Value::String(text) => {
            let text = text.trim();
            match option_named(question, text) {
                // By value, not by index: the option carries more than its
                // text — the roles a command owns, the whole process table
                // a workspace answer is — and a list of indexes is a
                // contract that breaks the day a rule finds one more
                // candidate.
                Some(index) => program(actions::Answer::Choice(index)),
                None if question.allow_custom => program(actions::Answer::Custom(text.to_string())),
                None => Err(unknown_option(question, &name, text)),
            }
        }
        serde_json::Value::Array(items) => {
            let chosen: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.trim().to_string()))
                .collect();
            if question.multi {
                let mut indexes = Vec::new();
                for text in &chosen {
                    let index = option_named(question, text)
                        .ok_or_else(|| unknown_option(question, &name, text))?;
                    indexes.push(index);
                }
                // The empty set is the recorded answer "none of them",
                // which is a different thing from never having been asked.
                return match indexes.is_empty() {
                    true => program(actions::Answer::None),
                    false => program(actions::Answer::Many(indexes)),
                };
            }
            // A list slot's answer is one value with the files in it, and
            // a program should not have to know how they are joined.
            let joined = crate::detect::join_list(&chosen);
            match option_named(question, &joined) {
                Some(index) => program(actions::Answer::Choice(index)),
                None => program(actions::Answer::Custom(joined)),
            }
        }
        other => Err(usage(format!(
            "{name} takes a string, a list of strings, or null — not {other}"
        ))),
    }
}

fn option_named(question: &actions::Question, text: &str) -> Option<usize> {
    question
        .options
        .iter()
        .position(|(value, _)| value.trim() == text)
}

/// Naming what was on offer, because a set answer has no other way in: a
/// service pando did not find is not a service it can run.
fn unknown_option(question: &actions::Question, name: &str, text: &str) -> anyhow::Error {
    let offered: Vec<&str> = question
        .options
        .iter()
        .map(|(value, _)| value.as_str())
        .collect();
    usage(format!(
        "{name} was answered {text:?}, which is not one of its options — they are: {}",
        match offered.is_empty() {
            true => "none at all".to_string(),
            false => offered.join(", "),
        }
    ))
}

/// Reads an answers file, or stdin for `-`.
pub(super) fn read_answers(path: &str) -> Result<Answers> {
    let text = match path {
        "-" => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| usage(format!("--answers -: {e}")))?;
            buf
        }
        path => {
            std::fs::read_to_string(path).map_err(|e| usage(format!("--answers {path}: {e}")))?
        }
    };
    Answers::parse(&text)
}

/// The answers file as the volunteered channel, for the slots no rule
/// proposed anything for.
///
/// Separate from the asker on purpose: a volunteered answer is only ever
/// *offered*, never demanded, so `None` here means "this pass has no
/// program behind it" and a slot the rules are silent about stays silent.
pub(super) fn volunteered_from(
    answers: Option<&Answers>,
) -> Option<impl Fn(&actions::Question) -> Option<Result<actions::Answer>> + '_> {
    let answers = answers?;
    Some(move |question: &actions::Question| answers.for_question(question))
}

/// The `init` asker: the answers file where it has something to say, and
/// the ordinary one — a terminal, `--yes`, or exit 3 — everywhere else.
pub(super) fn init_asker(
    answers: Option<&Answers>,
    yes: bool,
) -> impl Fn(&actions::Question) -> Result<actions::Answer> + '_ {
    let fallback = asker(yes);
    move |question: &actions::Question| {
        if let Some(answers) = answers
            && let Some(answer) = answers.for_question(question)
        {
            return answer;
        }
        fallback(question)
    }
}

/// What the answers file said that nothing used.
///
/// Never silent: a program that answered a question pando did not ask has
/// to learn that from the run rather than from a config that looks nothing
/// like what it sent.
///
/// Under `--replace` an answered slot is not a reason for an answer to go
/// unused, so a slot nothing asked about is just that.
pub(super) fn report_unused(answers: &Answers, before: &Config, replace: bool) {
    for slot in answers.unasked() {
        let name = slot_name(slot);
        if actions::settled(slot, before) && !replace {
            notice(&format!(
                "{name} is already answered — the answers file was not applied to it"
            ));
        } else {
            notice(&format!(
                "nothing asked about {name} in this run — the answers file's value for it was \
                 not used"
            ));
        }
    }
}

/// How a preview marks a slot nobody here could answer, in its value.
const UNANSWERED: &str = "(unanswered)";

/// What `init` prints: where the answers went, and what they say.
///
/// The path first, because the one thing a developer wants after a batch
/// of questions is the file to go and read.
pub(super) fn render_init(report: &actions::InitReport, verb: &str) -> String {
    let mut out = String::new();
    // Counted before anything is said about the file: "nothing left to
    // answer" above a list of slots marked unanswered contradicted itself.
    let unanswered = report
        .slots
        .iter()
        .filter(|slot| {
            slot.value
                .as_deref()
                .is_some_and(|v| v.starts_with(UNANSWERED))
        })
        .count();
    if report.answered_anything() {
        out.push_str(&format!("{verb} {}\n", report.config_file.display()));
        // Named only when this run put something there: the prelude is
        // about the machine, and a developer who never answered it should
        // not be pointed at a file pando did not touch.
        if let Some(user) = &report.user_file {
            out.push_str(&format!("{verb} {}\n", user.display()));
        }
    } else if unanswered > 0 {
        out.push_str(&format!(
            "{unanswered} {} still unanswered — `pando init` on a terminal asks {}, and \
             `--yes` takes pando's recommendation\n",
            if unanswered == 1 {
                "question is"
            } else {
                "questions are"
            },
            if unanswered == 1 { "it" } else { "them" },
        ));
    } else if report.config_file.exists() {
        out.push_str(&format!(
            "nothing left to answer — {} already says it all\n",
            report.config_file.display()
        ));
    } else {
        out.push_str("nothing to answer — pando has no questions about this project\n");
    }
    let width = report
        .slots
        .iter()
        .filter(|slot| slot.value.is_some())
        .map(|slot| slot.label.chars().count())
        .max()
        .unwrap_or(0);
    for slot in &report.slots {
        let Some(value) = &slot.value else { continue };
        out.push_str(&format!("  {:<width$}  {value}\n", slot.label));
    }
    out
}

/// What exit code 3 prints: the question, its options, and the ways out —
/// with the file named absolutely, what to paste into it, and the answers
/// file a program would use instead.
pub fn render_needs_answer(needs: &actions::NeedsAnswer) -> String {
    let mut out = render_question(needs);
    out.push_str(&how_to_answer(&needs.question));
    out
}

/// The paste-ready answer and the program's way in, under the question.
fn how_to_answer(question: &actions::Question) -> String {
    let mut out = String::new();
    let file = answer_file(question);
    let snippet = question.snippet.trim_end();
    if !snippet.is_empty() {
        out.push_str(match question.options.is_empty() {
            true => "  the key to set there, with a value of your own:\n",
            false => "  the first option, as it would be written there:\n",
        });
        for line in snippet.lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    if question.slot == crate::detect::Slot::Prelude {
        out.push_str(&format!(
            "  or `prelude = \"\"` under [runtime] in {file}: run every command with what \
             `bash -lc` resolves, and accept the mismatch\n"
        ));
    }
    // The value an answers file would carry for the first option, so a
    // program has a line to copy rather than a shape to guess.
    let value = match (question.multi, question.options.first()) {
        (true, _) => serde_json::Value::Array(
            question
                .checked
                .iter()
                .filter_map(|i| question.options.get(*i))
                .map(|(value, _)| serde_json::Value::String(value.clone()))
                .collect(),
        )
        .to_string(),
        (false, Some((value, _))) => serde_json::Value::String(value.clone()).to_string(),
        (false, None) => "\"<your own>\"".to_string(),
    };
    // Only for a question `init` asks. The login is asked by a namespaced
    // start alone, so an answers file has nowhere to put it; the table
    // above is the way in.
    if actions::ALL_SLOTS.contains(&question.slot) {
        out.push_str(&format!(
            "  or from a program: `pando init --answers <file.json>` with {{\"{}\": {value}}}\n",
            slot_name(question.slot)
        ));
    }
    out
}

fn render_question(needs: &actions::NeedsAnswer) -> String {
    let mut out = format!(
        "pando: {}
",
        needs.question.prompt
    );
    for line in &needs.question.details {
        out.push_str(&format!("  {line}\n"));
    }
    for (i, (value, why)) in needs.question.options.iter().enumerate() {
        // A set question shows what it would take, because that is what
        // `--yes` would accept and the thing an agent has to decide about.
        let prefix = match needs.question.multi {
            true if needs.question.checked.contains(&i) => "[x] ".to_string(),
            true => "[ ] ".to_string(),
            false => String::new(),
        };
        out.push_str(&format!(
            "  {prefix}{}) {value}  ({why})
",
            i + 1
        ));
    }
    // Nothing to write down: the answer empties a slot, and only a person
    // may choose whose.
    if needs.question.slot == crate::detect::Slot::FreeSlot {
        out.push_str(
            "pando: on a terminal pando asks which one to free; from a script, `pando rm` of a \
             worktree you no longer need frees its slot with it\n",
        );
        return out;
    }
    let file = answer_file(&needs.question);
    if needs.question.multi {
        out.push_str(&format!(
            "pando: answer it in {file} with a [[services]] table, or rerun with --yes to \
             take the ticked ones
"
        ));
        return out;
    }
    if needs.question.options.is_empty() {
        // `--yes` takes the first option, and there is no first option, so
        // offering it is an instruction to run the same failure again.
        out.push_str(&format!(
            "  (pando found no candidates for this)
pando: answer it in {file} — nothing pando can accept for you exists here
"
        ));
        return out;
    }
    if needs.question.preselect.is_none() {
        // There are options, and none of them is one a flag may take: this
        // slot's answer would have pando create a file out of contents it
        // did not write. Pointing at `--yes` here is an instruction to run
        // the same failure again.
        out.push_str(&format!(
            "pando: answer it in {file} — none of these is an option --yes may take for you
"
        ));
        return out;
    }
    out.push_str(&format!(
        "pando: answer it in {file}, or rerun with --yes to take the first option
"
    ));
    out
}

/// Which file an answer to this question is written to, so the way out
/// names the file to edit rather than the usual one: absolute, under
/// whatever home `PANDO_HOME` names, when the question knows it.
///
/// The prelude is about the machine, not the project, and its answer lives
/// in pando's machine-wide config — telling an agent to put it in
/// `pando.toml` would send it to a file pando will not read it from.
fn answer_file(question: &actions::Question) -> String {
    if let Some(file) = &question.answer_file {
        return file.display().to_string();
    }
    match question.slot.layer() {
        // The home this run uses, `PANDO_HOME` included, not the default.
        crate::config::Layer::User => crate::paths::default_home()
            .join("config.toml")
            .display()
            .to_string(),
        crate::config::Layer::Project => "pando.toml".to_string(),
    }
}
