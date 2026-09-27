# pando

**One repo. Every branch alive.**

> Pando is a forest that is one tree. Your repo is too. `pando` checks out every
> branch you are working on side by side, gives each one a running dev server,
> its own database, and a shareable URL, and never writes a byte into your repo.

Created by [Mert Karadayi](https://github.com/mertkaradayi).

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
- Or, experimentally, a database and a Redis slot of its own inside the
  servers your main checkout already runs: no server to start, nothing to
  wait for, and main's data untouched
- Shares any running worktree through a public tunnel URL
- Turns an open pull request into a running worktree: pick it from the
  list, press enter — a fork's too
- Shows all of it on one terminal screen, with a real log viewer

Built for people, and for agents, who work on several branches at once.

## The promise

pando never writes into your repository. Not a config file, not a gitignore
line, not a lockfile change. Everything it learns and everything it runs lives
under `~/.pando`.

One mode writes somewhere that is yours all the same: a namespaced
worktree gets a database of its own in your own database server. That is
opt-in, named after your main database so it can never be it, limited by a
grant you give once to that prefix and nothing else, and dropped only by
`rm`, only when pando's own records say pando made it. See
[Shared, namespaced, isolated](#shared-namespaced-isolated).

## Your first run

Install pando (see [Install](#install)), then:

```bash
cd your-project
pando
```

The first time, pando opens its setup screen instead of the list, on a
grove of its own: Pando, the aspen that is one tree with 47,000 stems,
drawn in dithered blocks, its leaves quaking. It comes alive when the
setup passes. Every project is a little different, so the surest start is to let your own
coding agent look at it. Press `a` to copy this one line, and paste it
into Claude Code or Codex, opened in the project:

```
Set up pando here: run `pando init --agent` and follow what it says.
```

`pando init --agent` prints the job for this project and this version of
pando: what pando already sees, the questions only the project can
answer, and the steps. The agent answers through `pando init --answers -`
on stdin, never a file in your repository, and proves the answers with
`pando check`: a throwaway worktree of the commit a new branch would fork
from, installed, started, its page asked for, and removed again, with no
branch and nothing left behind. When it passes, the agent says so, and
the setup screen turns green by itself: you're ready. From then on,
`pando` opens the list.

No agent? Press enter on the setup screen and pando tries its own guess,
tested by the same `pando check`. Esc skips the setup altogether and goes
straight to the list; nothing is ever gated on it.

`pando new` and `pando start` work on a project that was never set up
too. Nothing is asked where pando can tell: it reads the repository —
the lockfile, the dev script, the env example, the version file — and
takes its own first choice for anything it has one for, printing each as
it goes with the file it wrote it to:

```
pando: process list: using "npm run dev" (package.json scripts.dev, which starts the
       workspace's apps itself; API_PORT and WEB_PORT in the env example), pando's first
       choice, over 1 other option — change it in ~/.pando/projects/<id>/pando.toml
```

That file is the whole configuration; edit any line, or delete one and
run `pando init`, which puts every open question to you instead of taking
a default. `pando doctor` says what was detected and from where, and
`pando check` tests the setup again at any time. A question pando has no
option for at all is still asked, and so is the one real choice
isolation brings — which services to run private copies of.

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

The binary carries the brief and the shapes too: `pando init --agent`
prints the setup job for the project you are in, and
`pando init --agent --reference brief` or `--reference json` prints either
document whole.

## Commands

```
pando                 open the TUI for the repo you are in
pando new <branch>    create a worktree and branch from the default base
pando start <name>    start its dev server; --isolated for private services,
                      --namespaced (experimental) for its own database in yours
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
pando check           test the setup in a throwaway worktree, then remove it
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
⏎        pick its mode — shared, namespaced (experimental), isolated —
         and start it, or switch it when it runs
s i S    start it: as last time, isolated, or on the shared services
l        open the log viewer
x X      stop it, or stop everything
r P      restart it, or only the selected process
o O      open its URL, or the public one
c C y    copy its local URL, its public URL, its path
t        share it publicly, or stop sharing
! e      a shell in it, or open it in your editor
n d      new worktree, remove one
p        open pull requests: ⏎ makes a worktree for one
a v      copy the setup prompt, or test the setup (pando check)
m ?      what pando said in full, and every key
```

Enter opens the mode chooser on every worktree. On a stopped one the
mode it last ran in is under the cursor and marked `last used`, so
enter, enter is still one quick start; one never started has shared
there. On a running one the mode it runs in is marked `running`, and
choosing another switches it — every process restarts on the other
services, and the chooser was the asking. The logs are `l`.

A key that would interrupt a running worktree asks for a second press:
`r r` restarts it, `x x` stops it, `P P` restarts one process, and `i`
or `S` twice restarts it in the mode it already runs in. Esc takes the
first press back. Moving a running worktree between modes with `i` or
`S`, sharing it, and removing it ask in a dialog instead. On a stopped
worktree nothing asks.

The list is a table with a header row: `branch`, then `changes`
(`uncommitted` when there are uncommitted changes), `port`, `public` (`◈`
while it is shared), `mode` (`isolated` when it runs private copies of
the services, `namespaced` when it runs on a database and a slot of its
own in the main checkout's servers, each in its own colour), `git` (commits ahead of and behind the base branch), `PR`
and `status` (`failed`, or what is being done to it). A column shows
only when some row has something in it. The glyph before the branch
says whether it runs: `●` running, `◌` starting, `✗` failed, `○`
stopped. The full URL and the rest are in the detail pane beside it.

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

### Shared, namespaced, isolated

A worktree's code, processes, dependencies, ports and logs are its own
whatever it runs on. Its mode decides what happens to its data:

- **shared** — the main checkout's database and cache, data and all. The
  default, and the way back from the other two: `start --shared`, or `S`.
- **namespaced** (experimental) — the main checkout's servers, with a
  database and a Redis slot of the worktree's own in them: `shop__feat_x`
  beside `shop`, slot 3 beside slot 0. The database is built by the
  branch's own schema step, never copied from main's, and both are kept
  through a switch to another mode until `rm` drops them.
  `start --namespaced`, or the chooser on enter.
- **isolated** — servers of its own on ports of its own: containers from
  your compose file, or native engines from a recipe. `start --isolated`,
  or `i`.

A namespaced start writes into a server you own, so it is careful:

- It logs in as your app does, with the user and password beside the
  address in the main checkout's `.env`. Where there are none it asks
  once, keeps the answer in pando's own config for the project (mode
  0600), and hands it to the database client in its environment, never
  on a command line.
- That login has to be allowed to make databases named after the main
  one, and only those. The first start that is not allowed stops with
  nothing made and prints the grant to run once, as an administrator:

  ```sql
  GRANT ALL ON `shop\_\_%`.* TO 'app'@'localhost';
  ```

- A Redis slot is given out only where the app reads a slot setting
  (`REDIS_DB`, or the path of its URL), and only while the slot is empty:
  keys pando did not put there are somebody else's. When all fifteen are
  held, the start asks which stopped worktree gives its slot up.
- `rm` drops only what pando's records say it made, on the server it made
  it on, and never the main checkout's database or slot whatever a record
  says. `doctor` lists a database named for a worktree that no record
  holds, with the command that drops it, and never drops it itself.

MariaDB and MySQL databases and Redis slots are what it makes today;
every other service stays shared, and a namespaced start says so for each.
What a namespace is on an engine is a recipe's `[namespace]` table, so
another engine is a recipe rather than a release.

### Themes

`T` in the TUI lists the colour themes, each with a swatch of its
colours; moving through them repaints the screen, enter keeps one and
esc puts the old one back. The built-ins are pando's own, Catppuccin,
Flexoki, GitHub (default, dimmed, high contrast, colorblind), Gruvbox,
Kanagawa, Monokai Pro, One Dark, Rosé Pine, Tokyo Night, VS Code and
Zenwritten, each with a dark and a light half that follows the system.

A choice is saved in `~/.pando/config.toml`:

```toml
[ui]
theme = "catppuccin"
# appearance = "dark"            # or "light"; "auto" follows the system
# theme_from = "~/.config/theme-switcher/current"
```

`theme_from` names a file whose first line is a theme's name, such as
the state file a terminal theme switcher keeps. pando follows it while
it runs, so switching the terminal's theme switches pando's with it.
`PANDO_THEME` and `PANDO_APPEARANCE` override both for one run.

A theme is a small TOML file: a background, a foreground and seven
accents for each half, and every other colour is derived from them. One
in `~/.pando/themes/<name>.toml` is listed beside the built-ins, and
replaces the built-in of the same name.

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

Version 0.4.0, built and not published. Every command above is
implemented and covered by tests, in this order: worktrees and their
lifecycle; detached dev servers with their own ports, logs and readiness;
several processes per worktree; the log viewer; private per-worktree
services from the project's own compose file; public tunnel URLs; `init`,
`doctor` and `signals`; native service recipes for machines without
Docker; the JSON contract an agent reads; a worktree from any open
pull request in the TUI; and, experimentally, namespaced worktrees — a
database and a Redis slot of their own in the main checkout's servers,
tested against throwaway MariaDB and Redis servers the tests start
themselves. 0.4.0 adds no command: it is all of that after a review of
the whole project and the fixes it found, with a test suite that reads
no developer's shell profile and needs none of their tools.

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
