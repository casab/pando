//! `pando update`: what it found, and how the update went.

use crate::actions::{self, Install, Survey, Updated};
use anyhow::{Result, bail};
use std::io::Write;

/// `pando update`, or with `check` only what it would do.
pub fn update<W: Write>(check: bool, out: &mut W) -> Result<()> {
    let survey = Survey::take()?;
    if check {
        out.write_all(check_text(&survey).as_bytes())?;
        if let Err(why) = &survey.latest {
            bail!("could not read the latest release: {why}");
        }
        return Ok(());
    }
    if let Ok(latest) = &survey.latest
        && survey.behind() == Some(true)
    {
        super::notice(&format!(
            "{latest} is out; updating pando {}, installed {}",
            survey.running,
            survey.install.describe()
        ));
    }
    let updated = actions::update(&survey, &super::notice)?;
    out.write_all(updated_text(&survey, &updated)?.as_bytes())?;
    Ok(())
}

/// What `--check` prints: this pando, the latest release, and what
/// would update one to the other.
pub(super) fn check_text(survey: &Survey) -> String {
    let mut text = format!(
        "pando {}, installed {}\n",
        survey.running,
        survey.install.describe()
    );
    let latest = survey.latest.as_ref().ok();
    match (latest, survey.behind()) {
        (Some(latest), Some(false)) if *latest == survey.running => {
            text.push_str("that is the latest release\n");
            return text;
        }
        (Some(latest), Some(false)) => {
            text.push_str(&format!("newer than the latest release, {latest}\n"));
            return text;
        }
        (Some(latest), _) => text.push_str(&format!("{latest} is out\n")),
        (None, _) => {}
    }
    match survey.install.updater(latest) {
        Ok(updater) => text.push_str(&format!(
            "update with: pando update   (runs {})\n",
            updater.command_line()
        )),
        Err(refusal) => text.push_str(&format!("{refusal}\n")),
    }
    text
}

/// What `pando update` prints once its command has run, or the error
/// that says it did nothing.
pub(super) fn updated_text(survey: &Survey, updated: &Updated) -> Result<String> {
    let running = &survey.running;
    Ok(match updated {
        Updated::Current if survey.latest.as_ref().is_ok_and(|l| l == running) => {
            format!("pando {running} is the latest release\n")
        }
        Updated::Current => format!(
            "pando {running} is newer than the latest release, {}\n",
            survey
                .latest
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        ),
        Updated::To(now) => format!("pando {running} → {now}\n"),
        // Whoever updated did not know of anything newer either.
        Updated::Unchanged if survey.latest.is_err() => {
            format!("pando {running}: nothing newer to install\n")
        }
        Updated::Unchanged => {
            let latest = survey
                .latest
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            match survey.install {
                Install::Homebrew { .. } => bail!(
                    "pando is still {running}: Homebrew's formula for {latest} is published a \
                     few minutes after the release — run `pando update` again shortly"
                ),
                _ => bail!("the update ran, and pando is still {running}, not {latest}"),
            }
        }
    })
}
