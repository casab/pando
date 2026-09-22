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
service recipes, and the agent layer.

What is left is not a phase. `plans/open-follow-ups.md` carries the known
edges, each with who found it and where it belongs, and the release
checklist in `docs/08-roadmap.md` is untouched: no licence, no CI, no
published crate. Linux is a declared target that has never been compiled.

The working rules do not change. One conventional commit per work item, with
`cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean before every commit. Do not start a phase that has
no plan file. The repo is local-only: never push, never add a remote, never
open a PR. No attribution lines in commit messages.

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
- The maintainer's own repositories are off limits to pando sessions, which
  has a consequence worth stating plainly: **the fixture corpus is the only
  validation pando gets.** A corpus of tidy shapes therefore proves very
  little — `plans/fixture-hard-shapes.md` exists because of this, and the
  first real project pando met broke it in three ways no fixture had.

## Conventions

- Config file: `pando.toml`. pando home: `~/.pando/`.
- CLI verbs, used identically in every document:
  `new start stop restart ls rm share unshare logs status path init doctor
  signals`.
- The two invariants in `docs/02-principles.md` override anything else in the
  docs. If a design idea conflicts with them, the idea loses.
- Anything marked "default, undecided" in `docs/09-open-decisions.md` is not
  settled. Do not write docs or code that silently settles it.
