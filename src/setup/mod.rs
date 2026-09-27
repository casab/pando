//! A project's setup: whether pando is set up and tested for it, and the
//! one line that hands the job to the developer's own coding agent.
//!
//! Read from files pando owns — the config layers, the last check's
//! record, and what the setup screen remembers — never from the
//! repository.

/// The prompt a developer pastes into their coding agent: one line,
/// identical on the setup screen, in the header hint, in the CLI tip and
/// in the README. The rules live in the job `pando init --agent` prints,
/// so the prompt never changes with them.
pub const SETUP_PROMPT: &str =
    "Set up pando for this project: run `pando init --agent` and follow what it says.";
