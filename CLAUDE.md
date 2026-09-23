# CLAUDE.md

## What this is

pando is an open-source CLI and TUI that manages git worktrees and, for each
one, a running dev environment: dev server, optional private services, logs,
and a shareable URL. It is the generalization of `dwt`, a private tool that was
built for a single project.

## Current phase: all seven are built; the work now is edges

Implementation ran 2026-09-20 to 2026-09-22 and every phase in
`plans/00-master.md` is done: worktrees, start and stop, multiple processes,
the log viewer, compose isolation, share, `init`/`doctor`/`signals`, native
service recipes, and the agent layer. On 2026-09-23 the code was
restructured for maintainability with no behaviour change
(`plans/refactor-maintainability.md`), and 0.2.0 was built and tagged.

What is left is not a phase. `plans/open-follow-ups.md` carries the known
edges, each with who found it and where it belongs, and the release
checklist in `docs/08-roadmap.md` is untouched: no licence, no CI, no
published crate. Linux is a declared target that has never been compiled.

The working rules do not change. One conventional commit per work item, with
`cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean before every commit. Do not start a phase that has
no plan file. The repo is local-only: never push, never add a remote, never
open a PR. No attribution lines in commit messages.

## Code layout

`src/lib.rs` is the map: the dependency direction, and a table of where to
add each kind of thing. Keep to its shape:

- **One fact, one row.** What pando knows about the ecosystem is data in
  `src/catalog/` (package managers, frameworks, service images), in
  `runtime/languages.rs` (languages, version managers), or in
  `recipes/builtin/*.toml` (native services). Never add a second list of
  the same fact. Where two lists must differ in order because one is
  published or stored, keep both and add a test holding them to one set.
- **A module with more than one concern is a directory.** Its `mod.rs`
  holds the module doc and `pub use` re-exports, so callers never name a
  file; each file below it is one concern with a `//!` line; tests go in
  `tests.rs`. Items shared between sibling files are `pub(super)`, no
  wider.
- **Contracts have tests.** A string `agent/json.md` documents, a slot
  name, a CLI verb in the list below: each is held to the code by a test.
  Adding one without its test is not done.

## Releases

A release is local: bump `version` in `Cargo.toml` and in
`agent/.claude-plugin/plugin.json` together, commit it as
`chore(release): X.Y.Z`, tag `vX.Y.Z` with `git tag -a`, and
`cargo build --release`. Publishing anything is the launch checklist's job,
not a release's.

## Docs and plans are never committed

`docs/`, `plans/`, and any `*.plan.md` file are gitignored on purpose. They are
private working material and must never be committed or pushed, even when the
repo goes public. Put new design or plan files inside `docs/` or `plans/`. Do
not link to them from files that are committed, such as `README.md`.

## The origin project is frozen

`dwt` is a separate, private project and stays untouched while pando is built.
Its local path is in `CLAUDE.local.md`, which is not committed. Never edit,
commit to, build, or run anything in that directory from a pando session.
Reading it to port a module is fine; `docs/07-architecture.md` lists which
modules carry over.

## Public-repo hygiene

These docs will be published. Nothing specific to dwt's origin project belongs
here: no internal hostnames, routes, cookie or env variable names, schema
details, organisation or account names, PR links. Describe the origin only in
generic terms, for example "a monorepo with three processes, a native
database, a prod schema dump, and cookie auth."

## Testing policy

- Mutating commands (`new`, `start`, `stop`, `rm`, `share`, `init`) run only
  against generated fixture repositories in temporary directories. Tests
  create them; a fixtures script creates them for manual runs.
- Real repositories on this machine may be used only with read-only commands:
  `ls`, `doctor`, `signals`, `status`, `path`. Never `new` or `start` on them
  during development.
- dwt's origin project is never used, not even read-only. Its path is in
  `CLAUDE.local.md` only so it can be recognised and avoided.
- Never run two `cargo test`s at once. Two tests are load-sensitive
  (`plans/open-follow-ups.md`); under a parallel build they fail for
  reasons that have nothing to do with the change. Rerun a readiness or
  timeout failure alone before concluding anything.
- The maintainer's own repositories are off limits to pando sessions, which
  has a consequence worth stating plainly: **the fixture corpus is the only
  validation pando gets.** A corpus of tidy shapes therefore proves very
  little — `plans/fixture-hard-shapes.md` exists because of this, and the
  first real project pando met broke it in three ways no fixture had.

## Conventions

- Config file: `pando.toml`. pando home: `~/.pando/`.
- CLI verbs, used identically in every document:
  `new start stop restart ls rm share unshare open logs status path init
  doctor signals completions`.
- The two invariants in `docs/02-principles.md` override anything else in the
  docs. If a design idea conflicts with them, the idea loses.
- Anything marked "default, undecided" in `docs/09-open-decisions.md` is not
  settled. Do not write docs or code that silently settles it.
