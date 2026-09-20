# CLAUDE.md

## What this is

pando is an open-source CLI and TUI that manages git worktrees and, for each
one, a running dev environment: dev server, optional private services, logs,
and a shareable URL. It is the generalization of `dwt`, a private tool that was
built for a single project.

## Current phase: design only

There is no code in this folder yet, on purpose. `docs/` is the spec. Do not
scaffold a Cargo project, write source files, or add tooling unless the user
explicitly says the implementation phase has started. The repo is local-only
until the user says to publish it; never push or add a remote unasked.

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

## Conventions

- Config file: `pando.toml`. pando home: `~/.pando/`.
- CLI verbs, used identically in every document:
  `new start stop ls rm share unshare logs status path init doctor signals`.
- The two invariants in `docs/02-principles.md` override anything else in the
  docs. If a design idea conflicts with them, the idea loses.
- Anything marked "default, undecided" in `docs/09-open-decisions.md` is not
  settled. Do not write docs or code that silently settles it.
