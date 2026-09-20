use anyhow::Result;
use clap::Parser;
use std::process::ExitCode;

use pando::cli::{Cli, dispatch};
use pando::paths::{PandoPaths, default_home};
use pando::{config, project};

/// 0 ok, 1 error, 2 usage (clap's own), 3 reserved for needs-answer.
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    // Parsed before anything else so `--help` and `--version` work outside a
    // repository, and a usage error exits 2 through clap.
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` flattens the context chain onto one line: a CLI failure
            // is one sentence, not a stack.
            eprintln!("pando: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project = project::discover(&cwd)?;
    let paths = PandoPaths::new(default_home(), project);
    let loaded = config::load(&paths)?;
    for warning in &loaded.warnings {
        eprintln!("pando: {warning}");
    }
    match cli.command {
        Some(command) => dispatch(command, &paths, &loaded.config),
        None => anyhow::bail!("run `pando --help` to see the available commands"),
    }
}
