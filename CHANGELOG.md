# Changelog

Every version of pando, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and pando uses
[semantic versioning](https://semver.org). Before 1.0, a minor version
may change behaviour.

## Unreleased

### Added

- **Apps below a root with no manifest** ([#4], [#5]). A repository
  whose root has no manifest, lockfile or marker is read one level
  down, and also under `apps/*` and `packages/*`. pando proposes each
  app's frozen install in its own directory, e.g.
  `(cd backend && uv sync --frozen) && (cd frontend && npm ci)`, and a
  `[processes.<app>]` with a `cwd` for every app with a dev script.
  It also proposes the apps' gitignored env files for `provision`,
  their version files, and their env examples. `pando signals` lists
  the directories as `app_dirs`, and a compose file one directory
  down, such as `docker/compose.yml`.
- **Expo** is a framework rule ([#5]). An app whose `start` script
  runs `expo start` is proposed with `RCT_METRO_PORT` (Expo ignores
  `PORT`), `CI=1` so it never waits on a key, and 90 seconds to get
  ready. A workspace app with only a `start` or `serve` script that
  runs a framework's server is an app too. An app's own
  `.env.example`, such as `EXPO_PUBLIC_API_URL=http://127.0.0.1:3000`,
  gives it `{port:<role>}` env for the app it points at.
- `--answers` takes `processes` as an object of process tables (`cmd`,
  `cwd`, `ports`, `env`, `ready`), checked as a detected option is, so
  an agent can describe a multi-process app ([#4]).
- A `base` question, asked only where origin/HEAD is at least 100
  commits and 30 days behind the main checkout's branch, which it
  offers first. `pando check --base <branch>` tests another base for
  one run, and `doctor` notes a far-behind origin/HEAD ([#4]).
- A check that fails because the tested commit lacks a file the main
  checkout's branch has, such as a lockfile, fails with `kind: "base"`
  and names both refs, instead of asking for other settings ([#4]).
  A `check --base` of another branch is a probe: it never replaces the
  last result at the project's own base. With the `base` question still
  open, `pando check` asks it instead of testing origin/HEAD.
- `pando status` gives the command that opens an Expo app on the iOS
  simulator (`xcrun simctl openurl booted exp://127.0.0.1:<port>`), and
  `status --json` carries it as `app`, with the development build's
  link beside it ([#5]).
- When `bash -lc` resolves the wrong runtime version, the prelude
  question also offers a line that puts a matching binary first on PATH,
  from where your own shell finds it or a well-known place such as
  Homebrew's. Such a line is offered and never taken for you, because
  it changes every project on the machine.
- `doctor` notes a queue worker (ARQ, Celery, RQ, Sidekiq, BullMQ)
  whose Redis every worktree shares: a job queued in one worktree can
  run on another's code ([#4]).
- The agent brief documents the `[processes]` table, `{port:<role>}`
  in `env`, and that a phone needs the machine's LAN address ([#4],
  [#5]). For a project with an Expo process, the setup job and the
  memory block also tell the developer about the LAN address.

### Changed

- A project that would run nothing is an open question: `init --agent`
  lists the dev command, and `init --yes` exits 3 instead of writing a
  config that starts nothing ([#4]).
- An app directory that nothing starts, such as a Python API with no
  dev script beside a frontend with one, keeps `processes` open:
  `init --yes` exits 3 and names the directory, so a first run cannot
  pass while leaving the app out ([#4]).
- An app's default port includes the port its own env files state, so
  a sibling's URL to it (`EXPO_PUBLIC_API_BASE_URL=http://127.0.0.1:8787`)
  is given the worktree's port ([#5]).
- A shared service's port is also read from the env files beside the
  apps (`backend/.env`), and the job and the check name the file it
  came from ([#4]).
- An answer for a question that already has one is refused with exit
  2 and a pointer to `--replace`. Before, it was dropped and the run
  exited 0.
- The Claude Code and Codex setup skills, and the website, follow the
  first run as it now is.
- A typed `port_env` answer such as `PORT, API_PORT` is split into its
  variables, and a name that is not an environment variable is
  refused ([#4]).
- Split `*_HOST`/`*_PORT` pairs in an env example, such as
  `POSTGRES_SERVER` and `POSTGRES_PORT`, are read as a service's
  address, like a URL ([#4]).
- A version file in an app directory (`backend/.nvmrc`) counts:
  `doctor` and `start` check it where that app's processes run, and
  `init` raises the runtime prelude for it rather than leaving it to the
  first `check` ([#5]).
- `doctor` no longer says no version manager is installed while listing
  nvm: a manager that lacks the pinned version is named with its install
  command.
- A program's answer that config would refuse exits 2, as the other
  refusals do.
- The setup screen says "your agent is probably on it" only after a
  settings failure. A machine or base failure is the developer's to
  fix, and the screen says so ([#4]).

### Fixed

- `provision` no longer offers tool caches and artifacts such as
  `.coverage`, `.pytest_cache` or `.venv` ([#4]).

[#4]: https://github.com/mertkaradayi/pando/issues/4
[#5]: https://github.com/mertkaradayi/pando/issues/5

## 0.5.1 — 2026-09-28

### Added

- Binaries for macOS (Apple silicon and Intel) and Linux (x86_64 and
  arm64, static) with every release, so installing pando needs no Rust:
  `brew install mertkaradayi/tap/pando`, or one install script that puts
  it in `~/.local/bin`. Every download has a checksum and a GitHub
  attestation, and every release is installed and run on all four
  before it counts as done.

### Changed

- pando states the oldest Rust it builds with, 1.88, in `Cargo.toml`
  (`rust-version`), so an older toolchain gets a clear message instead
  of a build error, and CI builds with exactly that version.
- The README and the website say how to get Rust (rustup) when there is
  none, and install with `cargo install --locked`, the dependency
  versions CI tested. The website no longer calls what shipped in 0.5.0
  unreleased.

## 0.5.0 — 2026-09-28

### Added

- **The guided first run.** The first `pando` in a project with nothing
  to run opens a setup screen, on a dithered grove of its own, whose
  one-line prompt hands the job to the developer's own coding agent.
  `esc` always skips it, and a project configured before it is never
  sent there.
- `pando init --agent` prints the setup job for the project you are in;
  `pando init --answers - --replace` corrects an answer.
- `pando check` proves the setup in a throwaway detached worktree,
  installs and starts it, asks for its page, and removes it again. In
  namespaced mode it proves the schema step in namespaces of its own.
- The setup screen turns green by itself when a check passes, and the
  first-time tip and a passed check draw the grove on the CLI too.
- The agent offers to remember, in its own memory and never in the
  repository, how to run the project's worktrees with pando, and saves
  it only if the developer says yes.
- The main checkout runs like a worktree, and is listed first.
- The TUI's list shows each worktree's pull request and its state first,
  marks its mode and git state, and lines its columns up.
- The licence (AGPL-3.0-only), contribution guide, code of conduct,
  security policy, and CI on macOS and Linux.

### Fixed

- A failed hook says why, and how to fix it.
- A worktree's namespaced database is made in the main one's shape.
- `start` says a process is ready as soon as it is, and keeps a
  worktree's window; `stop --all` names a running check.

## 0.4.0 — 2026-09-26

### Added

- **Namespaced mode**, experimental: `start --namespaced`, or the TUI's
  mode chooser on enter. A worktree keeps the main checkout's servers
  and gets a MariaDB/MySQL database and a Redis slot of its own in them.
  Every drop and flush goes through one guard, and `rm` drops only what
  pando's records say it made. `doctor` lists databases no record holds.
- A recipe's `[namespace]` table says how an engine makes, finds and
  drops a namespace.
- A worktree's mode is shared, namespaced or isolated, and enter picks it.
- Colour themes as data, a live picker on `T`, and following a terminal
  theme switcher's state file.
- The worktree list is a table with a header row, glyphs for state and
  one colour per meaning.
- Keys that would interrupt a running worktree ask for a second press.
- `new`, `start` and the TUI take pando's first choice instead of asking,
  and print it with the file it went to.
- A workspace whose root dev script starts its own apps runs as that one
  script; a project that gitignores its lockfile gets the plain install.

### Fixed

- About 230 fixes from an audit of the whole project, by module and by
  concern: data safety in isolated and namespaced mode, the lifecycle,
  detection, share, the CLI, the TUI and doctor.
- The test suite is hermetic: it reads no developer's shell profile and
  needs none of their tools.

## 0.3.0 — 2026-09-25

### Added

- `p` in the TUI lists the repository's open pull requests, and enter
  makes a worktree for one. A fork's is fetched from `pull/<n>/head` into
  `pr-<n>/<branch>`. Restarting only the selected process moved to `P`.
- The TUI shows which GitHub account `gh` uses for the project.
- Workspace apps are told each other's port variables, and apps with no
  env file get the root `.env`.
- The project's own schema script is offered as the schema hook.
- `new` names the submodules a new worktree leaves empty.
- `completions` prints a completion script for bash, zsh, fish and more,
  after a UX and correctness pass over the TUI, the CLI and the lifecycle.

### Fixed

- A pull request worktree checks out the pull request, or refuses.
- A readiness timeout names the ports the process opened instead.

## 0.2.0 — 2026-09-23

The first tagged version: everything pando does, built in seven phases
from 2026-09-20, then restructured for maintainability with no change in
behaviour.

### Added

- Worktrees and their lifecycle: `new`, `ls`, `rm`, `path`.
- Detached dev servers with their own ports, logs and readiness:
  `start`, `stop`, `restart`, `status`, `logs`, `open`; several processes
  per worktree.
- The TUI and its log viewer.
- Private per-worktree services from the project's own compose file.
- Public tunnel URLs: `share`, `unshare`.
- `init`, `doctor` and `signals`.
- Native service recipes (Postgres, MariaDB, Redis, MongoDB) for
  machines without Docker.
- The JSON contract an agent reads, the setup brief, and a Claude Code
  plugin and Codex skills over it.
