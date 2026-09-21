use anyhow::{Context, Result};
use clap::Parser;
use std::process::ExitCode;

use pando::cli::{Cli, dispatch};
use pando::paths::{PandoPaths, default_home};
use pando::{actions, config, project, tui};

/// 0 ok, 1 error, 2 usage (clap's own), 3 needs-answer.
const EXIT_ERROR: u8 = 1;
/// pando has a question it cannot answer on its own. Its own code, so an
/// agent can tell "ask the human" from "it broke" without parsing text.
pub const EXIT_NEEDS_ANSWER: u8 = 3;

fn main() -> ExitCode {
    // Parsed before anything else so `--help` and `--version` work outside a
    // repository, and a usage error exits 2 through clap.
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        // A question is not a failure. It gets its own exit code and its own
        // shape, so an agent can answer it instead of guessing what broke.
        Err(e) if e.downcast_ref::<actions::NeedsAnswer>().is_some() => {
            let needs = e
                .downcast_ref::<actions::NeedsAnswer>()
                .expect("just checked");
            eprint!("{}", pando::cli::render_needs_answer(needs));
            ExitCode::from(EXIT_NEEDS_ANSWER)
        }
        Err(e) => {
            // `{:#}` flattens the context chain onto one line: a CLI failure
            // is one sentence, not a stack.
            eprintln!("pando: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // Before anything looks for a repository. The share proxy runs detached
    // from a temp directory, with no project, no config and no home to
    // guard — one environment variable and two ports are all it has.
    if let Some(pando::cli::Command::ShareProxy { listen, upstream }) = cli.command {
        return pando::cli::run_share_proxy(listen, upstream);
    }
    let cwd = std::env::current_dir().context(
        "cannot read the current directory — it may have been deleted; cd somewhere that exists",
    )?;
    let project = project::discover(&cwd)?;
    // A relative `PANDO_HOME` is resolved here rather than compared as-is:
    // `.pando` looks like it is outside the repository until the moment it
    // is created inside it.
    let paths = PandoPaths::new(cwd.join(default_home()), project);
    // `new`, `start`, `restart` and the TUI act on `pando.toml`; nothing
    // else needs it. A home layer pando cannot use stops those and only
    // those — you need `stop` most when that file is broken, and `ls` to
    // see what is there at all.
    let needs_config = cli
        .command
        .as_ref()
        .map(pando::cli::Command::needs_config)
        .unwrap_or(true);
    let loaded = match config::load(&paths) {
        Ok(loaded) => loaded,
        Err(e) if needs_config => return Err(e),
        Err(e) => {
            eprintln!(
                "pando: {e:#} — carrying on without it; `new`, `start` and `restart` need it fixed"
            );
            config::load_without_home(&paths)
        }
    };
    for warning in &loaded.warnings {
        eprintln!("pando: {warning}");
    }
    // Once, before dispatch: every command shares the same home, so a home
    // in the working tree is worth refusing even on a read-only command.
    actions::guard_write_locations(&paths, &loaded.config)?;
    match cli.command {
        Some(command) => dispatch(command, &paths, &loaded.config),
        None => tui::run(paths, loaded.config),
    }
}
