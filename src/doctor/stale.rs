//! Detections that have gone stale: a detected value the rules would no
//! longer write.

use std::collections::BTreeMap;
use std::path::Path;

use crate::detect;
use crate::paths::PandoPaths;

use super::config::value_repr;
use super::report::{ConfigReport, Finding, KeyReport, LayerReport, Section};

/// One value a rule would write right now, and the signal behind it.
struct Offer {
    /// The comparison form: quote style, whitespace and the order an
    /// inline table happens to be written in taken out.
    canonical: String,
    /// The value as a file would hold it.
    text: String,
    /// What made it a candidate — `package.json scripts.dev`.
    why: String,
}

/// Keys pando wrote itself that pando would not write now.
///
/// pando's rule is "ask just in time, once": a slot with an answer in it
/// is never asked again. That is the right rule and it has a cost nothing
/// else here pays — improve a detection rule, and every value the older,
/// worse version of it wrote stays frozen exactly as wrong as the day it
/// was written. pando records that it detected a value itself, re-derives
/// detection on every read path, and until now never compared the two: a
/// developer who upgraded saw the identical failure and had every reason
/// to think the binary was stale.
///
/// **Reported, never fixed.** Silently rewriting a value would change what
/// a developer's commands do with nobody asked, which breaks a larger
/// promise than the one it mends. So this names the key, the file, what
/// was detected, what would be detected now, and the one edit that reopens
/// the question.
///
/// Three things it is careful about:
///
/// - **Only `# detected:`.** An `# answered:` value — a person's, a
///   program's, `--yes`'s — is a decision, and pando does not second-guess
///   decisions. A comment a developer wrote by hand is not one of pando's
///   notes at all and is left alone for the same reason.
/// - **Not among the candidates, rather than not the first.** Several
///   candidates can each be right, and a rule that demotes one has not
///   rejected it. Absent from the list is the signal that the rules have
///   moved — or that the repository has, which fires the same check and is
///   just as worth knowing.
/// - **Keyed by the key, not by the slot.** A dev-command candidate that
///   carries roles writes `dev.ports` beside `dev.cmd`, under the dev
///   command's own note, so "what would be written here" has to be
///   gathered across every proposal rather than from whichever slot owns
///   the key.
pub(super) fn stale_detection_findings(
    paths: &PandoPaths,
    config: &ConfigReport,
    findings: &mut Vec<Finding>,
) {
    let detected: Vec<(&LayerReport, &KeyReport, String)> = effective_keys(config)
        .into_iter()
        .filter_map(|(layer, key)| Some((layer, key, detected_why(key.note.as_deref())?)))
        .collect();
    // Nothing pando wrote: no reason to re-derive detection at all, which
    // reads the repository and shells out to `git check-ignore`.
    if detected.is_empty() {
        return;
    }
    let offers = detectable_now(paths.root());
    for (layer, key, why) in detected {
        let (Some(raw), Some(text)) = (&key.raw, &key.value) else {
            continue;
        };
        // pando has no opinion about this key any more, and a divergence
        // needs two opinions. A rule that was withdrawn and put nothing in
        // its place has nothing to say about the value it left behind.
        let Some(offers) = offers.get(&key.key) else {
            continue;
        };
        if offers.iter().any(|offer| offer.canonical == canonical(raw)) {
            continue;
        }
        let list = offers
            .iter()
            .map(|offer| format!("{} ({})", offer.text, offer.why))
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(
            Finding::note(
                Section::Config,
                format!(
                    "{} = {text} in {} is a value pando detected itself ({why}), and pando \
                     would not detect it now — for this key its rules offer {list}",
                    key.key, layer.path
                ),
            )
            .with_fix(format!(
                "delete that line from {} and the next command that needs it asks the question \
                 again; keep it if it is what you want — pando will not change a value on its own",
                layer.path
            )),
        );
    }
}

/// Every key the merged config actually reads, with the layer it reads it
/// from.
///
/// The layers are built lowest precedence first, which is the order
/// `config::load_layers` merges them in, so the last layer to set a key is
/// the one that wins. `merge_tables` merges a table key by key, which is
/// what makes "the last layer that set *this key*" the right question
/// rather than "the last layer that set its table".
///
/// Table headers and array-of-table entries carry a note and no value of
/// their own, and a stripped key is one pando removes from that layer and
/// never reads. Neither is a value to have an opinion about.
fn effective_keys(config: &ConfigReport) -> Vec<(&LayerReport, &KeyReport)> {
    let mut out: Vec<(&LayerReport, &KeyReport)> = Vec::new();
    for layer in &config.layers {
        for key in &layer.keys {
            if key.value.is_none() || key.ignored {
                continue;
            }
            match out.iter_mut().find(|(_, held)| held.key == key.key) {
                Some(entry) => *entry = (layer, key),
                None => out.push((layer, key)),
            }
        }
    }
    out
}

/// Every value detection would write right now, keyed by the config key it
/// would write it to.
///
/// Through [`detect::edits`], which is the one translation from a
/// candidate to the keys it becomes — the same function the answer path
/// writes through. A second reading of what a candidate means would be a
/// second opinion, and the two would drift.
fn detectable_now(root: &Path) -> BTreeMap<String, Vec<Offer>> {
    let signals = detect::signals(root);
    let mut out: BTreeMap<String, Vec<Offer>> = BTreeMap::new();
    for proposal in detect::propose(root, &signals) {
        for candidate in &proposal.candidates {
            for edit in detect::edits(proposal.slot, candidate) {
                let key = match edit.table.is_empty() {
                    true => edit.key.clone(),
                    false => format!("{}.{}", edit.table.join("."), edit.key),
                };
                let offer = Offer {
                    canonical: canonical(&edit.value),
                    text: value_repr(&edit.value),
                    why: candidate.why.clone(),
                };
                let held = out.entry(key).or_default();
                if !held.iter().any(|other| other.canonical == offer.canonical) {
                    held.push(offer);
                }
            }
        }
    }
    out
}

/// The evidence out of a `# detected:` note, and `None` for anything else.
///
/// `# answered:` in each of its four spellings, and a comment a developer
/// wrote themselves, all come back `None`: this only ever looks at values
/// pando's own rules put there.
fn detected_why(note: Option<&str>) -> Option<String> {
    let body = note?.strip_prefix('#')?.trim_start();
    Some(body.strip_prefix("detected:")?.trim().to_string())
}

/// A value with its formatting taken out, so that two values meaning the
/// same thing compare equal.
///
/// A literal string and a basic string are the same value; so are two
/// inline tables written in a different key order, and an array with a
/// space after its comma. Array *order* is kept, because `version_files`
/// is a precedence list and reordering it means something.
fn canonical(value: &toml_edit::Value) -> String {
    match value {
        toml_edit::Value::String(s) => format!("{:?}", s.value()),
        toml_edit::Value::Array(array) => {
            let items: Vec<String> = array.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        toml_edit::Value::InlineTable(table) => {
            let mut items: Vec<String> = table
                .iter()
                .map(|(key, value)| format!("{key:?}={}", canonical(value)))
                .collect();
            items.sort();
            format!("{{{}}}", items.join(","))
        }
        other => value_repr(other),
    }
}
