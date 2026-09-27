# pando for coding agents

A developer's coding agent should be able to set pando up on a project
neither of them has seen before, correctly, on the first try — and then
operate it every day. The intelligence for that is not in any host
integration. It is in the evidence pando already publishes, plus a written
procedure for turning that evidence into answers.

So there are two files and some packaging:

| File | What it is |
|---|---|
| [`brief.md`](./brief.md) | **The procedure.** The only place the reasoning lives |
| [`json.md`](./json.md) | The contract: every JSON shape, versioned, with the exit codes |
| `skills/` | The Claude Code plugin's two skills |
| `codex/` | The same two skills, packaged for Codex |

Both hosts' wrappers are glue. Neither contains reasoning, and a test
fails if either grows past fifty lines — two copies of a procedure drift
within a month, and a procedure written for a language model still reads
perfectly when it is wrong.

You do not need either wrapper. An agent that reads `brief.md` and
`json.md` has everything they have, and the pando binary carries both:
`pando init --agent` prints the setup job for the project it runs in, and
`--reference brief` or `--reference json` prints either file whole.
`--reference memory` prints the block the job ends with: how to run this
project's worktrees with pando, which the agent saves in its own memory.
pando keeps the same block as `CLAUDE.md` and `AGENTS.md` in the project's
directory under `~/.pando`, above every worktree it makes.

## Claude Code

The plugin root is this directory, so the brief ships with the skills.

```bash
claude plugin marketplace add mertkaradayi/pando
claude plugin install pando@pando
```

## Codex

```bash
agent/codex/install.sh      # copies the skills, the brief and the contract
                            # into ~/.codex/skills
```

Run it again after upgrading pando: the brief it installs is a copy, and
a stale copy is the one failure mode this layout has.

## If you are the agent

Run `pando init --agent`, or read [`brief.md`](./brief.md). Start with
`pando signals` and `pando doctor --json`, write only through
`pando init --answers`, and never write a byte into the developer's
repository.
