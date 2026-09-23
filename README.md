# pando

**One repo. Every branch alive.**

> Pando is a forest that is one tree. Your repo is too. `pando` checks out every
> branch you are working on side by side, gives each one a running dev server,
> its own database, and a shareable URL, and never writes a byte into your repo.

## The story

Pando is a single quaking aspen in Fishlake National Forest, Utah. It looks like
a forest. It is one organism.

- About 47,000 stems, all genetically identical, sharing one root system
- About 43 hectares, roughly 106 acres
- About 6,000 metric tons, the heaviest known living thing
- A root system between 9,000 and 16,000 years old
- Each stem lives 100 to 130 years and is replaced from the roots
- Named in 1993; the Latin means "I spread"

Your repository works the same way.

| Pando | Your repo |
|---|---|
| The root system | The repo. One object store, one history, never touched. |
| Each stem | A worktree. A complete tree above ground, with its own running app and its own database. |
| Stems come and go | Branches come and go. The root persists. |
| "I spread" | Spread your branches out and let every one of them live. |

## What it does

- Lists, creates, and removes git worktrees for the repo you are in
- Starts each worktree's dev server on its own ports, detached, with logs
- Optionally gives each worktree private copies of its services: Postgres,
  Redis, MariaDB, whatever your compose file declares — or, on a machine
  that would rather not run Docker, the same databases natively from a
  recipe you can override
- Shares any running worktree through a public tunnel URL
- Shows all of it on one terminal screen, with a real log viewer

Built for people, and for agents, who work on several branches at once.

## The promise

pando never writes into your repository. Not a config file, not a gitignore
line, not a lockfile change. Everything it learns and everything it runs lives
under `~/.pando`.

## For agents

pando publishes what it knows as JSON so a program can read it instead of
parsing English, and answers come back through one validated write path.

- [`agent/json.md`](agent/json.md) — every machine-readable shape,
  versioned, with the exit codes. `3` means pando has a question and the
  question is on stderr.
- [`agent/brief.md`](agent/brief.md) — the procedure for turning that
  evidence into answers: read before asking, write only through
  `pando init --answers`, never a byte in the repository.
- [`agent/`](agent/README.md) — a Claude Code plugin and Codex skills, both
  thin over that one brief.

## Commands

```
pando                 open the TUI for the repo you are in
pando new <branch>    create a worktree and branch from the default base
pando start <name>    start its dev server; --isolated for private services
pando stop [name]     stop one worktree, or all of them
pando restart <name>  stop and start again, keeping the ports
pando ls              list worktrees with ports and status
pando rm <name>       stop everything, remove the worktree, wipe its data
pando share <name>    expose it at a public URL
pando unshare <name>  take the public URL down
pando logs <name>     tail its logs; --json for machines
pando status          machine-readable state; --json
pando path <name>     print the worktree's path
pando init            answer every setup question now instead of as you go
pando doctor          explain what was detected, why, and what is missing
pando signals         dump detection signals as JSON, for humans or agents
```

## Install

There is no published binary yet. Build it from source with a Rust
toolchain (edition 2024):

```bash
cargo install --path .        # puts `pando` in ~/.cargo/bin
# or
cargo build --release         # the binary is target/release/pando
```

`pando --version` says which version you have.

## Working on pando

`src/lib.rs` is the map: the dependency direction between modules, how a
module that grew past one concern is laid out as a directory, and a table
of where to add a package manager, a framework, a service image, a
language, a native service recipe, a CLI verb, a question, a `doctor`
section or a TUI key.

What pando knows about the ecosystem lives in `src/catalog/` as data, one
row per fact, and every module that needs a fact reads that row. The
built-in service recipes are TOML files in `src/recipes/builtin/`, in
the same format as a recipe you drop into `~/.pando/recipes/`.

Before every commit: `cargo test`, `cargo clippy --all-targets -- -D
warnings`, and `cargo fmt --check`.

## Status

Version 0.2.0, built and not published. Every command above is
implemented and covered by tests, in this order: worktrees and their
lifecycle; detached dev servers with their own ports, logs and readiness;
several processes per worktree; the log viewer; private per-worktree
services from the project's own compose file; public tunnel URLs; `init`,
`doctor` and `signals`; native service recipes for machines without
Docker; and the JSON contract an agent reads.

macOS is what it is developed and tested on. The Unix-only parts have
Linux branches written and no CI, so Linux is intended rather than
demonstrated: nobody has yet compiled it there, let alone run it.

What that does not mean: there is no crate, no release binary and no
package to install, and almost every worktree pando has created has been
inside a generated fixture repository. It has been pointed at exactly one
real project, which found three bugs in an afternoon — a Makefile target
read down to its first line, a failure that left an empty log and no
explanation, and a backgrounded server reported as dead. All three are
fixed, and the count is the point: a tool this heavily tested against
situations it invented still breaks on first contact with one it did
not.
