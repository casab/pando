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
  Redis, whatever your compose file declares
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

## Status

Design phase. There is no code yet.
