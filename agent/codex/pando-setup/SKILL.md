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
with pando: save it in your own memory (`~/.codex/AGENTS.md`), never in the
repository, so later sessions run them through pando.

## Before you start

Confirm the working directory is the repository the developer asked you
about. `pando init` is a mutating command, and §8 of the brief says it
runs nowhere else.

## The pass

Follow the brief. In outline, so you can tell whether you are lost:

1. `pando signals` and `pando doctor --json`. Nothing else — do not
   re-derive what they publish.
2. Decide from the evidence. Ask the human at most one question, and only
   the kind §1 of the brief calls human.
3. Build the answers file: one key per slot whose proposal has
   `"decided": false`, answered **by value**. Nothing for a slot whose
   proposal is `null` or `decided`.
4. `pando init --answers <file> --dry-run`, show the developer what it
   would write, then run it without `--dry-run`.
5. `pando doctor`, and report what it said — including the notes, and
   including a problem you are not going to fix.

## Reporting back

Say what pando decided and what you decided, name the file it wrote, and
quote doctor's verdict. If a question was left open, say which one and
what it needs. Do not say the project starts unless you ran `pando start`.
