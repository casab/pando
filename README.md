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
- Turns an open pull request into a running worktree: pick it from the
  list, press enter — a fork's too
- Shows all of it on one terminal screen, with a real log viewer

Built for people, and for agents, who work on several branches at once.

## The promise

pando never writes into your repository. Not a config file, not a gitignore
line, not a lockfile change. Everything it learns and everything it runs lives
under `~/.pando`.

## First run

```bash
cd your-project
pando            # the TUI: n makes a worktree, s starts it
```

Nothing is asked where pando can tell. It reads the repository — the
lockfile, the dev script, the env example, the version file — and takes
its own first choice for anything it has one for, printing each as it goes
with the file it wrote it to:

```
pando: process list: using "npm run dev" (package.json scripts.dev, which starts the
       workspace's apps itself; API_PORT and WEB_PORT in the env example), pando's first
       choice, over 1 other option — change it in ~/.pando/projects/<id>/pando.toml
```

That file is the whole configuration; edit any line, or delete one and
run `pando init`, which puts every open question to you instead of taking
a default. `pando doctor` says what was detected and from where. A
question pando has no option for at all is still asked, and so is the one
real choice isolation brings — which services to run private copies of.

A project pando cannot read on its own — a dev server started some way no
rule knows — is what the agent plugin below is for: `/pando:pando-setup`
in Claude Code reads the project, writes the answers through
`pando init --answers`, and proves them by starting a scratch worktree.

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
pando stop [name]     stop one worktree; --all for every one
pando restart <name>  stop and start again, keeping the ports
pando ls              list worktrees: status, URL, ports, git; -l for paths
pando rm <name>       stop everything, remove the worktree, wipe its data
pando share <name>    expose it at a public URL
pando unshare <name>  take the public URL down
pando open <name>     open its URL in the browser; --public for the shared one
pando logs <name>     tail its logs; --json for machines
pando status          what runs where, per process; --json
pando path <name>     print the worktree's path
pando init            answer every setup question now instead of as you go
pando doctor          explain what was detected, why, and what is missing
pando signals         dump detection signals as JSON, for humans or agents
pando completions     print a completion script for bash, zsh, fish…
```

A worktree is named by its branch (`feat/login`) or by its directory
(`feat+login`). Inside a worktree, `start`, `stop`, `restart`, `logs`,
`open`, `share` and `unshare` need no name.

On a terminal, `start` and `restart` wait until every process answers,
and when one does not they print the last lines of its log and why.
From a script they return once everything is spawned; `--wait` and
`--no-wait` choose either way.

### In the TUI

```
⏎        open the logs when it runs, start it when it is stopped
s i S    start it: as last time, isolated, or on the shared services
x X      stop it, or stop everything
r P      restart it, or only the selected process
o O      open its URL, or the public one
c C y    copy its local URL, its public URL, its path
t        share it publicly, or stop sharing
! e      a shell in it, or open it in your editor
n d      new worktree, remove one
p        open pull requests: ⏎ makes a worktree for one
m ?      what pando said in full, and every key
```

A key that would interrupt a running worktree asks for a second press:
`r r` restarts it, `x x` stops it, `P P` restarts one process, and `i`
or `S` twice restarts it in the mode it already runs in. Esc takes the
first press back. Moving a running worktree between isolated and shared,
sharing it, and removing it ask in a dialog instead. On a stopped
worktree nothing asks.

A row in the list reads `● feat/login * :17342 ◈ ▣ ↑2 ◍42`. The glyph
says whether it runs (`●` running, `◌` starting, `✗` failed, `○`
stopped), `*` marks uncommitted changes, `:17342` is the port it serves
on, `◈` means it is shared publicly and `▣` that it runs isolated, then
how far it has drifted from the base branch and its pull request. The
full URL, the mode and the rest are in the detail pane beside it; `?`
lists every mark.

`p` lists the repository's open pull requests through the GitHub CLI
(`gh`, signed in); typing narrows them by number, title, branch or
author. Enter checks out the pull request's branch in a new worktree, or
goes to the worktree it already has. A pull request from a fork has no
branch on `origin`, so pando fetches it from `pull/<number>/head` into a
branch of its own, `pr-<number>/<branch>`. One whose branch cannot be
found on `origin` is refused rather than started as an empty branch of
the same name.

The log viewer has a tab per log, led by an `all` tab that merges every
process's when there are several. `1`–`9` switch tabs, `/` searches, `f`
filters by level, and `e`/`E` jump between errors.

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

Version 0.3.0, built and not published. Every command above is
implemented and covered by tests, in this order: worktrees and their
lifecycle; detached dev servers with their own ports, logs and readiness;
several processes per worktree; the log viewer; private per-worktree
services from the project's own compose file; public tunnel URLs; `init`,
`doctor` and `signals`; native service recipes for machines without
Docker; the JSON contract an agent reads; and a worktree from any open
pull request in the TUI.

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
