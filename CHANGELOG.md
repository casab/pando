# Changelog

Every version of pando, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and pando uses
[semantic versioning](https://semver.org). Before 1.0, a minor version
may change behaviour.

## Unreleased

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
- The agent remembers, in its own memory and never in the repository,
  how to run the project's worktrees with pando.
- The main checkout runs like a worktree, and is listed first.
- The licence (AGPL-3.0-only), contribution guide, code of conduct,
  security policy, and CI on macOS and Linux.

### Fixed

- A failed hook says why, and how to fix it.
- A worktree's namespaced database is made in the main one's shape.

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
