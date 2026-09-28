---
name: pando-setup
description: Configure pando on a repository for the first time — read what it publishes about the project, answer only what its rules cannot, and write the config through `pando init --answers`. Use when someone asks to set pando up, configure pando, add pando to a project, or when `pando start` exits 3 because a question is unanswered.
---

# Set pando up on this project

**Read `brief.md`, beside this file, in full before you do anything else.**
It is the procedure. This file is glue and deliberately contains none of
the reasoning — if it disagrees with the brief, the brief is right.
`json.md`, also beside this file, is the contract for every shape the
brief names. (In a pando checkout the two are `agent/brief.md` and
`agent/json.md`; `agent/codex/install.sh` is what put copies here.)

Or run `pando init --agent`: it prints the setup job for this project,
with the brief's first-run section, and the steps end with `pando check`.
Its last section is a block saying how to run this project's worktrees
with pando: once the check passes, offer to save it in your own memory
(`~/.codex/AGENTS.md`) and save it only if the developer says yes; never
in the repository. Later sessions then run them through pando.

## Before you start

Confirm the working directory is the repository the developer asked you
about. `pando init` and `pando check` are mutating commands, and §8 of
the brief says they run nowhere else.

## The pass

Follow the brief's first-run section, which `pando init --agent` prints.
In outline, so you can tell whether you are lost:

1. `pando init --yes` saves pando's first choice for every open question,
   under `~/.pando`. Ask the developer nothing that pando or the
   project's docs answer.
2. When it exits 3, the open question has no option, and it is yours:
   answer it from `pando signals`, `pando doctor --json` and the project's
   own docs. Several processes are one object of process tables at
   `processes`, covering every app `signals` lists.
3. Answer on stdin, never from a file in the repository:
   `pando init --answers - --dry-run`, then the same without `--dry-run`.
   `--replace` corrects a slot that is already answered.
4. `pando check`, with a timeout of at least 10 minutes. A settings
   failure is yours to correct; a machine or base failure is the
   developer's to hear about. Never rerun it unchanged.
5. Tell the developer it is set up and tested, and offer to save the
   job's last block in your memory, saving it only on their yes.

## Reporting back

Say what pando chose and what you answered, and quote the check's
result. If a question was left open, say which one and what it needs. Do
not say the project starts unless `pando check` or `pando start` started
it.
