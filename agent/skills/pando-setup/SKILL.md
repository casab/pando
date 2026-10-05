---
name: pando-setup
description: Configure pando on a repository for the first time, asking the developer nothing — read what it publishes about the project, decide what its rules cannot, write the config through `pando init --answers`, and end by reporting what was set and how to change it. Use when someone asks to set pando up, configure pando, add pando to a project, or when `pando start` exits 3 because a question is unanswered.
---

# Set pando up on this project

**Read `${CLAUDE_PLUGIN_ROOT}/brief.md` in full before you do anything
else.** It is the procedure. This file is glue, and it deliberately
contains none of the reasoning — if it disagrees with the brief, the brief
is right. `${CLAUDE_PLUGIN_ROOT}/json.md` is the contract for every shape
the brief names.

Or run `pando init --agent`: it prints the setup job for this project,
with the brief's first-run section, and the steps end with `pando check`.
Its last section is a block saying how to run this project's worktrees
with pando: once the check passes, the report says you can save it in
your own memory (`~/.claude/CLAUDE.md`), and you save it only when the developer
tells you to; never in the repository. Later sessions then run them through pando.

## Before you start

Confirm the working directory is the repository the developer asked you
about. `pando init` and `pando check` are mutating commands, and §8 of
the brief says they run nowhere else.

## The pass

Follow the brief's first-run section, which `pando init --agent` prints.
In outline, so you can tell whether you are lost:

1. `pando init --yes` saves pando's first choice for every open question,
   under `~/.pando`. Ask the developer nothing, at any point: every
   question is yours to decide.
2. When it exits 3, the open question is yours: answer it from
   `pando signals`, `pando doctor --json` and the project's own docs. Several processes are one object of process tables at
   `processes`, covering every app `signals` lists.
3. Answer on stdin, never from a file in the repository:
   `pando init --answers - --dry-run`, then the same without `--dry-run`.
   `--replace` corrects a slot that is already answered.
4. `pando check`, with a timeout of at least 10 minutes. A settings or
   base failure is yours to correct; a machine failure goes in the
   report with pando's command. Never rerun it unchanged.
5. Tell the developer it is set up and tested, list what was set and
   how to change it, and mention you can save the job's last block in
   your memory. End on no question; save it only when they say to.

## Reporting back

Say what pando chose and what you decided, each with why and how to
change it, and quote the check's result. Nothing is left open for the
developer to answer; what only they can do, such as start a server, is
the command pando printed. Do
not say the project starts unless `pando check` or `pando start` started
it.
