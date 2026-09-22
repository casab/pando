# The shapes a program reads

pando publishes what it knows as JSON so that a program — a coding agent, a
Makefile, a CI job — can read it instead of parsing English. This file is
the contract. Everything in it is covered by the compatibility statement
below; anything pando prints that is *not* in here is a human-readable
convenience and may change without notice.

Five shapes are published:

| Command | Shape | Read it for |
|---|---|---|
| `pando signals` | one object | what the repository says about how to run itself, and every question pando would ask |
| `pando doctor --json` | one object | what this *machine* answers, and everything that is wrong |
| `pando status --json` | one object | what is running, on which ports, with which URL |
| `pando ls --json` | one object | the worktrees and their git state |
| `pando logs <name> --json` | one object **per line** | a log, with a level and a timestamp per line |

There is one write path, and it is not JSON output: `pando init --answers`.
It is documented in [The answers file](#the-answers-file) below.

One more file is published rather than printed:
`~/.pando/projects/<id>/decisions.jsonl`, one object per line, which
records every question a program answered. See
[The decisions log](#the-decisions-log).

## Compatibility

Every shape carries `version`, an integer, currently **1**. It is bumped
independently of pando's own version, so a program pins what it parses
rather than which release it runs against.

Within a version:

- **Keys are added, never removed and never renamed.** A new key may appear
  in any release. Parse leniently: read the keys you need, ignore the rest,
  and never assert on the *set* of keys.
- **A key's type never changes**, and neither does the meaning of its value.
- **`null` means "pando has nothing to say here."** It never means `false`,
  never means zero, and never means an empty list. `"dirty": null` is "git
  could not be asked", which is a different fact from `"dirty": false`.
- **A key may be absent where it would be `null` or empty.** Treat absent,
  `null` and empty as the same thing — `signals` omits
  `slots[].proposal.candidates[].service.recipe` for a compose service,
  and the decisions log omits an empty list rather than printing `[]`.
- Object key **order is not part of the contract**. Array order is: every
  list pando prints is deterministic, and two runs of a read-only command
  print the same bytes.

A change that breaks any of the above bumps `version`. Pin it, check it,
and refuse a shape you were not written for rather than guessing.

## stdout, stderr, and exit codes

**stdout is the answer; stderr is the narration.** Everything pando says
about what it is doing — every guess it made visible, every warning, every
question — goes to stderr. stdout carries only what the command was asked
for. `pando signals > signals.json` is a file and nothing else.

Exit codes are the first thing to read, and they are the same everywhere:

| Code | Means | What a program does |
|---|---|---|
| `0` | it worked | carry on |
| `1` | it failed | read stderr; the sentence says what and usually what to do. Do not retry |
| `2` | you asked wrongly | a bad flag, or an answers file naming a question pando does not ask. Fix the request, then retry |
| `3` | **pando has a question** | the question, its options, and the ways out are on stderr. Answer it; do not retry unchanged |

Exit 3 is not a failure. It is the whole reason the answers file exists: a
program that cannot answer a question gets the question rather than a
guess, a hang, or a wrong config.

Two traps worth knowing:

- `pando doctor` exits **1** when it found something that will break a
  command. The report is on stdout and is complete — there is no extra
  sentence on stderr. `doctor --json` exits the same way and puts the same
  verdict in `ok`, so a program never has to count severities.
- `pando init --answers` can exit **1** rather than 2 for one kind of bad
  answer: a `prelude` that fails its probe on this machine. It is a machine
  fact rather than a shape error, and stderr says so.

## `pando signals`

Read-only, identical on two runs, and it spawns nothing: no probe of this
machine's runtime, no `docker compose config`. It is a statement about the
repository, which is what makes it safe to read before deciding anything.

```jsonc
{
  "version": 1,
  "project": { "id": "...", "root": "/abs/path", "name": "..." },
  "signals": {
    "scripts": {},              // package.json scripts, and their equivalents
    "targets": {},              // Makefile / justfile targets
    "lockfiles": [],            // every lockfile at the root
    "workspace_markers": [],    // pnpm-workspace.yaml, turbo.json, …
    "version_files": [],        // .nvmrc, .python-version, mise.toml, …
    "runtime_requirements": [   // what those files and `engines` ask for
      { "language": "node", "spec": "22", "source": ".nvmrc", "pinned": true }
    ],
    "env_example": [],          // [key, value] pairs from the env example
    "markers": [],              // framework and toolchain markers
    "compose_files": [],
    "ignored_present": [],      // gitignored files that exist in the checkout
    "provision_seeds": []       // examples a worktree file could be copied from
  },
  "compose": [                  // one entry per compose file, as pando's own reader sees it
    {
      "file": "docker-compose.yml",
      "services": ["postgres", "redis"],
      "extends": [],            // services whose real definition is elsewhere
      "include": false,         // a top-level `include:` this reader does not follow
      "error": null
    }
  ],
  "slots": [ /* one per question — see below */ ]
}
```

`extends`, `include` and `error` are why a services proposal can be missing
or under-ticked: they are the parts of a compose file this build did not
follow, published rather than papered over.

### The nine questions

`slots` has one entry per question pando can ask, in the order it asks
them. The names are frozen — they are the same strings `--answers` takes:

```
install  version_files  prelude  processes  dev_cmd  port_env  services  schema_hook  provision
```

```jsonc
{
  "slot": "dev_cmd",
  "prompt": "Which command starts the local development server?",
  "answered": false,      // config already says — from any layer
  "proposal": {
    "decided": false,     // the rules are sure enough to take this without asking
    "preferred": 0,       // the option a question preselects, and the one --yes takes
    "checked": [],        // for the set question: the options that start ticked
    "multi": false,       // the answer is a set of the options, not one of them
    "allow_custom": true, // a command of your own is an answer here
    "allow_none": false,  // null is an answer here
    "file": null,         // the compose file a services proposal is about
    "none_because": null, // why a rule settled the slot with the empty answer
    "candidates": [
      {
        "value": "pnpm dev",               // what an answers file names to pick this
        "why": "package.json scripts.dev", // the evidence, written into the config
        "preselected": false,
        "needs_a_human": false,            // --yes may not take this; a file naming it may
        "ports": null,
        "processes": null,                 // the whole [processes] table a workspace answer is
        "service": null,                   // { "file": "...", "recipe": "...", "env_key": "..." }
        "hook": null,                      // the whole [[hooks]] entry this option is
        "provision_from": {}               // which paths would be copied from an example
      }
    ]
  }
}
```

**`proposal` has three states, and they mean three different things.**
Getting this wrong is the most common way a program wastes a question or
loops:

| State | Means | What a program does |
|---|---|---|
| `"proposal": null` | no rule had anything to say about this slot at all | **Nothing to choose, and still answerable.** There are no options and nobody is asked, so an `--answers` value is taken as a command of your own — validated and written like any other. `services` and `prelude` are the two exceptions and report it as unused. Do not put the slot to a human: pando is not asking |
| `"decided": true` | a rule settled it; no question will be asked | Leave it alone. An `--answers` value for it is reported as unused |
| `"decided": false` | pando will ask | This is the only state an answer changes. Answer by value |

A proposal with `"decided": true`, no candidates and a `none_because` is a
rule deciding the answer is *none of them* — which is a real answer, and
the opposite of `"proposal": null`.

`answered` is the key that says a `null` proposal is closed. A slot a
program filled still has no proposal — nothing about the rules changed —
so a reader that watches `proposal` alone will answer it again on every
run. Watch `answered`.

Three more facts about `slots` that are not visible in the shape:

- **`prelude` is never proposed here.** It is the one question about this
  laptop rather than this repository, it costs a `bash -lc` probe, and
  `signals` spawns nothing. It is asked only when this machine does not
  resolve what the project pins; `doctor`'s `runtime` section is where that
  evidence lives.
- **Two slots take no answer when nothing was proposed.** The set question,
  `services`, is answered by naming options, and there are none. `prelude`
  is verified against this machine before it is written, and the check only
  exists behind the proposal that raises the question. A value for either
  is reported as unused.
- **Answering one slot can settle another.** Taking the per-app form at
  `processes` fills the dev command and its ports for every app, so
  `dev_cmd` and `port_env` are never asked and an answer sent for them is
  reported as unused. That is correct, not an error.

## `pando doctor --json`

The whole report, plus the exit code as a value. It probes this machine —
`bash -lc`, the tools on `PATH`, the engines a recipe needs — so it is the
half `signals` deliberately leaves out. It writes nothing anywhere.

```jsonc
{
  "version": 1,
  "ok": true,               // the exit code as a value: false means exit 1
  "project":  { "id": "...", "root": "...", "home": "...", "worktrees_dir": "...",
                "home_mode": "700", "port_min": 17000, "port_max": 32767,
                "base_step": 8, "bases_in_range": 1971, "windows_held": 0 },
  "config":   { "layers": [ { "layer": "committed|user|project", "path": "...",
                              "present": true, "error": null,
                              "keys": [ { "key": "dev.cmd", "value": "\"pnpm dev\"",
                                          "note": "detected: package.json scripts.dev",
                                          "ignored": false } ] } ],
                "error": null },
  "runtime":  { "prelude": "...", "prelude_from": "...",
                "languages": [...], "requirements": [...] },
  "tools":    [ { "name": "git", "path": "/usr/bin/git", "version": "...",
                  "detail": null, "needed_for": "...", "found": true } ],
  "worktrees":[ { "name": "...", "path": "...", "phase": "...", "created_by_pando": true,
                  "isolated": false, "locked": false, "prunable": false,
                  "prunable_reason": null, "known_to_git": true,
                  "processes": [...], "services": [...] } ],
  "services": { "compose": [...], "native": [...], "isolation": { ... } },
  "hooks":    [ { "name": "migrate", "after": "services", "cmd": "...",
                  "fingerprint": [...], "matches": [...], "runs": true } ],
  "adoption": [],           // project folders that look like this repo from before it moved
  "findings": [ { "section": "runtime", "severity": "problem|note",
                  "message": "...", "fix": "..." } ]
}
```

`findings` is the part to read first. `severity` is `problem` — something
here will break a command, and `ok` is `false` — or `note`, which is
something to know that breaks nothing. Every finding carries its own `fix`.

`config.layers[].keys[].note` is the provenance of every key pando wrote:
`detected: <evidence>`, `answered: <date>`, `answered: a program, <date>`,
or `answered: --yes took the first of N options`. It is how a developer, or
a program, tells what decided each line.

`services.isolation` carries the native-versus-container decision and the
evidence behind it. It is the report a program should quote to a human
rather than re-deriving.

## `pando status --json`

```jsonc
{
  "version": 1,
  "project": { "id": "...", "root": "...", "name": "..." },
  "worktrees": [
    {
      "name": "feat+one",
      "branch": "feat/one",
      "path": "/abs/path",
      "ports": { "web": 17008 },       // the roles this worktree holds
      "observed_ports": [17008],       // what is actually listening
      "url": "http://localhost:17008", // the readiness role's URL, or null
      "isolated": false,               // runs private copies of the services
      "share": null,                   // or { url, local_port, proxy_port, since }
      "processes": {
        "dev": { "pid": 1234, "phase": "starting|running|failed",
                 "since": "2026-09-21T23:22:05Z", "reason": null,
                 "log": "/abs/path/dev.log" }
      },
      "services": {
        "postgres": { "kind": "compose|native", "port": 17010, "up": true,
                      "logging": true, "project": "pando-…" }
      },
      "hooks": { "install": { "fingerprint": "md5:…", "ran_at": "…" } }
    }
  ]
}
```

`phase` is per process; a worktree is only as up as its worst one.
`reason` is non-null exactly when `phase` is `failed`, and it is the
sentence to show a human. `log` is an absolute path — read it with
`pando logs`, not by opening the file, so truncation and partial lines are
handled for you.

`share.url` is the public URL. **No cookie is ever in this shape**, even
when the share is behind an auth command.

## `pando ls --json`

```jsonc
{
  "version": 1,
  "project": { "id": "...", "root": "...", "name": "..." },
  "worktrees": [
    { "name": "feat+one", "path": "...", "branch": "feat/one", "head": "abc1234",
      "detached": false, "dirty": false, "ahead": 0, "behind": 0,
      "created_by_pando": true, "prunable": false, "locked": null,
      "pr": { "number": 12, "state": "open|merged|closed", "url": "..." } }
  ]
}
```

`dirty`, `ahead` and `behind` are `null` when git could not be asked.
`locked` is `null` when the worktree is not locked, and the lock reason —
possibly an empty string — when it is. `pr` comes from a cache: `ls --json`
never spawns `gh`, so it works offline and may be stale or `null`.

## `pando logs <name> --json`

One JSON object per line, on stdout, forever if `-f` was passed:

```jsonc
{ "version": 1, "ts": "2026-09-20T10:00:00+00:00", "level": "debug|info|warn|error", "line": "ready in 412ms" }
```

`ts` is the line's *own* timestamp when pando could read one, normalised to
UTC, and `null` when the line has none — it is not the time pando read it.
`line` is the text with ANSI colour removed. `level` is pando's reading of
the line, not the writer's: a line containing no level word is `info`.

`--source` picks the log: `dev` by default, or a process name, a service
name, or a hook name. `status --json` names every one a worktree has.

## The decisions log

`~/.pando/projects/<id>/decisions.jsonl`. Appended to, never rewritten;
one JSON object per line, oldest first.

Every answer a **program** supplies to a question the rules could not
decide is written here with the evidence it was decided from, and a later
line records it if a person changes it afterwards. Nothing else goes in:
not a rule's own answer, not a person's, and not `--yes` taking what the
rules already preferred.

It exists because a skill that answers pando's questions is a crutch
unless somebody reads what it answered. This is the labelled corpus that
improves the rules — and the rules are what every developer gets,
including the ones with no agent.

```jsonc
// a program answered a question
{ "version": 1, "at": "2026-09-21T23:22:05Z", "slot": "dev_cmd", "kind": "answer",
  "answer": "pnpm dev:web",   // exactly the shape an answers file sends
  "shape": "choice|custom|set|none",
  "wrote": "pnpm dev:web",    // what config says about the slot afterwards
  "evidence": {
    "prompt": "Which command starts the local development server?",
    // the three below are omitted when they are empty, as everywhere else
    "details": [],            // what the question printed above its options
    "mechanism": null,        // "compose" or "native", at the services question
    "weighed": [],            // and the facts that chose that mechanism
    "preferred": 0,           // the option the rules would have taken
    "options": [ { "value": "pnpm dev", "why": "package.json scripts.dev",
                   "preselected": true, "needs_a_human": false } ]
  } }

// …and a person later changed it
{ "version": 1, "at": "2026-09-22T09:04:11Z", "slot": "dev_cmd", "kind": "override",
  "was": "pnpm dev:web", "now": "pnpm dev:all" }
```

Two things follow from `answer` being the shape an answers file sends:
the log **replays** — every `answer` line for a project is an answers
file, with `jq` and nothing else — and it is directly comparable with
what pando would propose today, which is what makes it a corpus rather
than an audit trail.

An override is noticed by comparing what config says about the slot
against what the log last recorded, on the next command that resolves
anything. The comparison is on the slot's *answer*: a comment or a
reordering is not an override, and neither is editing a command inside
the shape that was chosen at `processes` — switching between a process
per app and the root script is. pando would rather miss an override than
invent one. `version` here is the line's own, bumped independently of the
`version` on the printed shapes.

## The answers file

`pando init --answers <path>` — or `-` for stdin — is the one way a program
writes config. It is not a TOML editor: every value goes through the same
question, the same validation and the same file a person's answer does, and
lands with `# answered: a program, <date>` beside it.

One JSON object. Keys are the question names above. Values:

| JSON | Means |
|---|---|
| `"some text"` | the option whose `value` is exactly that text, **or**, if nothing matches — or there were no options at all — a command of your own (where `allow_custom` is true) |
| `["a", "b"]` | the set answer, at the one question where `multi` is true; every element must name an option |
| `["a", "b"]` | the whole list, at `version_files` and `provision`, whose single answer is a list of files |
| `null` | "none of them", where `allow_none` is true |
| `[]` | "none of them" at the set question. A usage error anywhere else — `null` is how you say none |

**Answers are by value, never by index.** An index breaks the day a rule
finds one more candidate; the text does not, and picking an option by its
text is what brings everything else the option carries — the ports a
command owns, the whole process table a workspace answer is, a service's
env key, the hook entry.

Refusals, all exit 2 and all naming the key: a name that is not a question;
a shape the question cannot take; a value that is not one of the options at
a question that has them; an empty string. An answer for a slot that was
already answered, or that nothing asked about, is **reported on stderr and
not applied** — it is not an error, and the run still exits 0. A slot with
no proposal at all is not "nothing asked about": it takes a custom answer,
except at `services` and `prelude`. See the three-state table above.

`--dry-run` runs the same pass against copies of the files it would write
and prints them on stdout, config first. Use it to show a diff before
writing.
