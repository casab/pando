//! Asking a question at the terminal: the prompt a developer answers
//! when pando's rules could not decide.

use super::notice;
use crate::actions;
use anyhow::Result;
use std::io::Write;

/// The CLI's way of asking.
///
/// With `--yes`, the rules' own recommendation is taken and said out loud.
/// On a terminal, a numbered prompt. Anywhere else — a script, an agent, a
/// pipe — the question becomes exit code 3 rather than a process that
/// blocks on a `read` nobody will ever answer.
pub(super) fn asker(yes: bool) -> impl Fn(&actions::Question) -> Result<actions::Answer> {
    move |question: &actions::Question| {
        if yes {
            // A set answer: `--yes` takes the options a rule already
            // resolved and leaves the rest, which is exactly what the
            // question pre-ticks.
            if question.multi {
                let taken: Vec<&str> = question
                    .checked
                    .iter()
                    .filter_map(|i| question.options.get(*i))
                    .map(|(value, _)| value.as_str())
                    .collect();
                notice(&format!("--yes: taking {}", joined(&taken)));
                // `Auto`, not `Many`: nobody chose these, a flag took the
                // ones the rules had resolved, and the comment written to
                // `pando.toml` has to say which of the two happened.
                return Ok(actions::Answer::Auto(0));
            }
            return match question.preselect {
                Some(index) => {
                    notice(&format!("--yes: taking {:?}", question.options[index].0));
                    // Not `Choice`: nobody chose it, so the line written to
                    // the config says a flag took it rather than claiming a
                    // confidence the rules never had.
                    Ok(actions::Answer::Auto(index))
                }
                None => Err(needs_answer(question)),
            };
        }
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Err(needs_answer(question));
        }
        prompt(question)
    }
}

fn needs_answer(question: &actions::Question) -> anyhow::Error {
    anyhow::Error::new(actions::NeedsAnswer {
        question: question.clone(),
    })
}

/// A numbered prompt on stderr, so a piped stdout is still only the
/// command's own output.
fn prompt(question: &actions::Question) -> Result<actions::Answer> {
    prompt_with(question, &mut std::io::stderr(), || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            // The terminal went away mid-question.
            return Ok(None);
        }
        Ok(Some(line))
    })
}

/// [`prompt`] with its terminal injected. `read` gives the next line, or
/// `None` when there is no more input.
pub(super) fn prompt_with(
    question: &actions::Question,
    out: &mut impl Write,
    read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    if question.multi {
        return prompt_many(question, out, read);
    }
    prompt_one(question, out, read)
}

/// The report a question carries, indented under its prompt. Everything
/// pando knows that made it worth asking, and on stderr like the question
/// itself.
fn write_details(question: &actions::Question, out: &mut impl Write) -> Result<()> {
    for line in &question.details {
        writeln!(out, "  {line}")?;
    }
    Ok(())
}

/// What "none" means, which is different for each slot that offers it.
fn none_label(slot: crate::detect::Slot) -> &'static str {
    match slot {
        crate::detect::Slot::Prelude => {
            "none — this machine needs no line in front of its commands"
        }
        crate::detect::Slot::Provision => "none — a new worktree needs no local file of yours",
        crate::detect::Slot::SchemaHook => {
            "no — do not run a schema step (written as on = \"never\")"
        }
        _ => "none — this process has no port",
    }
}

/// `a, b` — the readable form of a set, for a notice.
fn joined(values: &[&str]) -> String {
    if values.is_empty() {
        return "none of them".to_string();
    }
    values.join(", ")
}

/// A question whose answer is a *set*: every option with a checkbox,
/// toggled by number, confirmed with enter.
///
/// Toggling rather than typing a list, because the pre-ticked set is
/// already the answer for most projects: enter accepts it, and the one
/// service the rules could not place is one keystroke away.
fn prompt_many(
    question: &actions::Question,
    out: &mut impl Write,
    mut read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    let mut checked: Vec<bool> = (0..question.options.len())
        .map(|i| question.checked.contains(&i))
        .collect();
    let width = question
        .options
        .iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);
    writeln!(out, "pando: {}", question.prompt)?;
    write_details(question, out)?;
    loop {
        for (i, (value, why)) in question.options.iter().enumerate() {
            let box_ = if checked[i] { "[x]" } else { "[ ]" };
            writeln!(out, "  {box_} {}) {value:<width$}  {why}", i + 1)?;
        }
        write!(
            out,
            "  a number toggles it, ⏎ accepts [{}] > ",
            joined(
                &question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| checked[*i])
                    .map(|(_, (value, _))| value.as_str())
                    .collect::<Vec<_>>()
            )
        )?;
        out.flush()?;
        let Some(line) = read()? else {
            return Err(needs_answer(question));
        };
        let line = line.trim();
        if line.is_empty() {
            let chosen: Vec<usize> = checked
                .iter()
                .enumerate()
                .filter(|(_, on)| **on)
                .map(|(i, _)| i)
                .collect();
            return Ok(if chosen.is_empty() {
                actions::Answer::None
            } else {
                actions::Answer::Many(chosen)
            });
        }
        if question.allow_none && (line == "n" || line == "N") {
            return Ok(actions::Answer::None);
        }
        match line
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1 && *n <= checked.len())
        {
            Some(n) => checked[n - 1] = !checked[n - 1],
            None => writeln!(
                out,
                "pando: a number between 1 and {} toggles one, n takes none, ⏎ accepts",
                checked.len()
            )?,
        }
    }
}

fn prompt_one(
    question: &actions::Question,
    out: &mut impl Write,
    mut read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    let width = question
        .options
        .iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);
    writeln!(out, "pando: {}", question.prompt)?;
    write_details(question, out)?;
    for (i, (value, why)) in question.options.iter().enumerate() {
        let marker = if question.preselect == Some(i) {
            "*"
        } else {
            " "
        };
        writeln!(out, "  {marker}{}) {value:<width$}  {why}", i + 1)?;
    }
    if question.allow_custom {
        writeln!(
            out,
            "   c) something else — type the {}",
            question.slot.custom_noun()
        )?;
    }
    if question.allow_none {
        writeln!(out, "   n) {}", none_label(question.slot))?;
    }
    let default = question.preselect.map(|i| i + 1);
    loop {
        match default {
            Some(n) => write!(out, "  [{n}] > ")?,
            None => write!(out, "  > ")?,
        }
        out.flush()?;
        let Some(line) = read()? else {
            return Err(needs_answer(question));
        };
        let line = line.trim();
        if line.is_empty()
            && let Some(index) = question.preselect
        {
            writeln!(out, "  → {}", question.options[index].0)?;
            return Ok(actions::Answer::Choice(index));
        }
        if question.allow_none && (line == "n" || line == "N") {
            return Ok(actions::Answer::None);
        }
        if question.allow_custom && (line == "c" || line == "C") {
            write!(out, "  {} > ", question.slot.custom_noun())?;
            out.flush()?;
            let custom = read()?.unwrap_or_default().trim().to_string();
            if !custom.is_empty() {
                return Ok(actions::Answer::Custom(custom));
            }
            writeln!(
                out,
                "pando: nothing was typed — an empty {} is not an answer",
                question.slot.custom_noun()
            )?;
            continue;
        }
        // A number is a choice, always — even one that is out of range.
        // Letting it fall through to "then it must be a command" is how a
        // fat-fingered `5` on a four-option question became `cmd = "5"`,
        // written down as an answer and never questioned again. A command
        // that really is only digits is still reachable through `c`.
        match line.parse::<i64>() {
            Ok(_) if !question.options.is_empty() => {
                let chosen = line
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1 && *n <= question.options.len());
                match chosen {
                    Some(n) => {
                        writeln!(out, "  → {}", question.options[n - 1].0)?;
                        return Ok(actions::Answer::Choice(n - 1));
                    }
                    None => writeln!(
                        out,
                        "pando: pick a number between 1 and {}",
                        question.options.len()
                    )?,
                }
            }
            // Anything else is taken as the command itself, so nobody has
            // to discover that `c` exists first.
            _ if question.allow_custom && !line.is_empty() => {
                return Ok(actions::Answer::Custom(line.to_string()));
            }
            _ => writeln!(
                out,
                "pando: pick a number between 1 and {}",
                question.options.len()
            )?,
        }
    }
}
