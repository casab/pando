# The shapes a program reads

pando publishes what it knows as JSON so that a program — a coding agent, a
Makefile, a CI job — can read it instead of parsing English. This file is
the contract. Everything in it is covered by the compatibility statement
below; anything pando prints that is *not* in here is a human-readable
convenience and may change without notice.

Six shapes are published:

| Command | Shape | Read it for |
|---|---|---|
| `pando signals` | one object | what the repository says about how to run itself, and every question pando would ask |
| `pando doctor --json` | one object | what this *machine* answers, and everything that is wrong |
| `pando status --json` | one object | what is running, on which ports, with which URL |
| `pando ls --json` | one object | the worktrees and their git state |
| `pando logs <name> --json` | one object **per line** | a log, with a level and a timestamp per line |
| `pando check --json` | one object | whether the setup works: a test run in a throwaway worktree, and why it failed |

There is one write path, and it is not JSON output: `pando init --answers`.
It is documented in [The answers file](#the-answers-file) below.

One more file is published rather than printed:
`~/.pando/projects/<id>/decisions.jsonl`, one object per line, which
records every question a program answered. See
[The decisions log](#the-decisions-log).

## Compatibility

Every shape carries `version`, an integer, currently **2**. It is bumped
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

**Changed in 2.** `signals.targets` went from a name-to-string map, where the
string was the first line of the target's recipe, to a name-to-object map
carrying `tool`, `prereqs` and `recipe`. The old shape was not merely
narrower, it was wrong: one line of a recipe is not the command a target
runs, and pando proposed such a line as a dev command on a real project.

One coarseness worth knowing: `signals`, `doctor`, `check`, `status` and
`logs` share a single version, so a break in one bumps all five. Nothing about `doctor`,
`status` or `logs` changed in 2. Splitting them so a program can pin what it
actually parses is recorded as a follow-up, not done here.

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

One exit 3 has no options: `pando init` on a project that would run
nothing — no process configured, and no proposal at `processes` or
`dev_cmd`. The question is `dev_cmd`, `--yes` has nothing to take for it,
and `init --agent` lists it open. Answer it with a command of your own, or
`processes` with an object of process tables (see the answers file below).
`new` and `start` never ask it.

One exit 3 has options and nothing preferred: `processes` below a root
with no manifest, when an app directory outside `packages/` has nothing in
the per-app option to start it. The option's `why` names it — `backend has
uv.lock but no dev script: nothing here starts it` — and its
`needs_a_human` is true. Answer `processes` with an object of process
tables covering every process. `new` and `start` take the option.

Two traps worth knowing:

- `pando doctor` exits **1** when it found something that will break a
  command. The report is on stdout and is complete — there is no extra
  sentence on stderr. `doctor --json` exits the same way and puts the same
  verdict in `ok`, so a program never has to count severities.
- `pando start` and `pando restart` wait for readiness only when stderr
  is a terminal. From a script or an agent they return as soon as
  everything is spawned, so exit 0 means "spawned", not "ready". Pass
  `--wait` to block until every process is ready; a process that fails
  while it waits exits **1**, with its reason and the closing lines of its
  log on stderr.

## `pando signals`

Read-only, identical on two runs, and it spawns nothing: no probe of this
machine's runtime, no `docker compose config`. It is a statement about the
repository, which is what makes it safe to read before deciding anything.

```jsonc
{
  "version": 2,
  "project": { "id": "...", "root": "/abs/path", "name": "..." },
  "signals": {
    "scripts": {},              // package.json scripts, and their equivalents
    "targets": {},              // Makefile / justfile targets: name to
                                //   { tool, prereqs, recipe } — see below
    "lockfiles": [],            // every lockfile at the root
    "workspace_markers": [],    // pnpm-workspace.yaml, turbo.json, …
    "version_files": [],        // .nvmrc, .python-version, mise.toml, …
    "runtime_requirements": [   // what those files and `engines` ask for
      { "language": "node", "spec": "22", "source": ".nvmrc", "pinned": true },
      { "language": "node", "spec": "20", "source": "backend/.nvmrc", "pinned": true,
        "dir": "backend" }      // an app directory's: only there is `dir` set
    ],
    "env_example": [],          // [key, value] pairs from the env example
    "markers": [],              // framework and toolchain markers
    "compose_files": [],        // the root's; with none there, those one directory below
    "ignored_present": [],      // gitignored files that exist in the checkout
    "provision_seeds": [],      // examples a worktree file could be copied from
    "workspace_env_links": [],  // [app/.env, .env]: the root .env for apps with none
    "app_dirs": [               // only when the root has no manifest, lockfile or marker:
      {                         //   the apps below it, directly or in apps/* and packages/*
        "dir": "backend",
        "lockfiles": ["uv.lock"],
        "scripts": {},
        "markers": ["pyproject.toml"]
      }
    ]
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

A target is published whole, because one line of a recipe is not the
command a target runs:

```jsonc
"targets": {
  "dev": {
    "tool": "make",           // or "just"
    "prereqs": ["build"],     // what runs first
    "recipe": [               // every command line, continuations joined,
      "@./build.sh",          //   `@`/`-`/`+` prefixes kept, comments dropped
      "@./serve"
    ]
  }
}
```

pando proposes `make dev` for that target and only proposes a recipe line
itself when the target *is* that line: no prerequisites, one command, no `$`
in a makefile or `{{` in a justfile, no `-` or `+` prefix, and nothing at the
top of the file that every recipe runs with — make's `export` or `include`,
a justfile's `set` or `export`.

A repository whose root is not an app — `backend/` beside `frontend/`,
and nothing at the root that says how to build either — is read one level
down. `app_dirs` lists what was found there, and the proposals come from
it: the install is each app's own, run in its directory, and the
`processes` answer has one process per app with a dev script, each with
its `cwd`, even when only one app has one. Its version files join
`version_files` under its path, `backend/.nvmrc`, and once that answer is
written a start checks the runtime there, in the directory its
processes run in. Each app's env example joins
`env_example`, and its local env files (`backend/.env`, never a cache or
a coverage file) join `ignored_present` and `provision_seeds` under its
path.

A compose file one directory down, `docker/compose.yml`, is listed in
`compose_files` and read under `compose` when the root has none of its
own, but the services question is only proposed from one at the root: a
file kept beside a deployment is as often the production stack as the
development one.

`extends`, `include` and `error` are why a services proposal can be missing
or under-ticked: they are the parts of a compose file this build did not
follow, published rather than papered over.

### The ten questions

`slots` has one entry per question pando can ask, in the order it asks
them. The names are frozen — they are the same strings `--answers` takes:

```
install  version_files  prelude  processes  dev_cmd  port_env  services  schema_hook  provision  base
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
        "processes": null,                 // the whole [processes] table a workspace answer is,
                                           // or the env and ready a dev_cmd option brings to [dev]
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
| `"proposal": null` | no rule had anything to say about this slot at all | **Nothing to choose, and still answerable.** There are no options and nobody is asked, so an `--answers` value is taken as a command of your own, or at `processes` as process tables — validated and written like any other. `services` and `prelude` are the two exceptions and report it as unused. Do not put the slot to a human: pando is not asking |
| `"decided": true` | a rule settled it; no question will be asked | Leave it alone. An `--answers` value for it is reported as unused |
| `"decided": false` | pando will ask | This is the only state an answer changes. Answer by value |

A proposal with `"decided": true`, no candidates and a `none_because` is a
rule deciding the answer is *none of them* — which is a real answer, and
the opposite of `"proposal": null`.

`answered` is the key that says a `null` proposal is closed. A slot a
program filled still has no proposal — nothing about the rules changed —
so a reader that watches `proposal` alone will answer it again on every
run. Watch `answered`.

Four more facts about `slots` that are not visible in the shape:

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
- **`base` is proposed only when origin/HEAD is far behind** the branch
  the main checkout is on: a hundred commits and thirty days. Its options
  are that branch, then origin/HEAD's; `"proposal": null` means origin/HEAD
  is where `new` forks from and `check` tests, as it is for most
  repositories. Either way it takes a branch name of your own, written as
  `[project] base`, and one this repository has no branch for, here or on
  origin, is refused with exit 2.
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
  "version": 2,
  "ok": true,               // the exit code as a value: false means exit 1
  "project":  { "id": "...", "root": "...", "home": "...", "worktrees_dir": "...",
                "home_mode": "700", "port_min": 17000, "port_max": 32767,
                "base_step": 8, "bases_in_range": 1971, "windows_held": 0 },
  "config":   { "layers": [ { "layer": "committed|user|project", "path": "...",
                              "present": true, "error": null,
                              "keys": [ { "key": "dev.cmd", "value": "\"pnpm dev\"",
                                          "note": "# detected: package.json scripts.dev",
                                          "ignored": false } ] } ],
                "error": null },
  "runtime":  { "prelude": "...", "prelude_from": "...",
                "languages": [...], "requirements": [...] },
  "tools":    [ { "name": "git", "path": "/usr/bin/git", "version": "...",
                  "detail": null, "needed_for": "...", "found": true } ],
  "worktrees":[ { "name": "...", "path": "...", "phase": "...", "created_by_pando": true,
                  "main": false,  // the main checkout's record, once pando has run it
                  "mode": "shared|namespaced|isolated", "isolated": false,
                  "locked": false, "prunable": false,
                  "prunable_reason": null, "known_to_git": true,
                  "processes": [...], "services": [...] } ],
  "services": { "compose": [...], "native": [...], "isolation": { ... } },
  "hooks":    [ { "name": "migrate", "after": "services", "cmd": "...",
                  "fingerprint": ["prisma/migrations/**"],
                  "matches": 3,     // null when the hook is keyed on nothing
                  "runs": [ { "worktree": "feat+one", "ran_at": "...",
                              "will_run_again": false } ] } ],
  "adoption": [],           // project folders that look like this repo from before it moved
  "findings": [ { "section": "project|config|runtime|tools|worktrees|services|hooks|adoption",
                  "severity": "problem|note", "message": "...", "fix": "..." } ]
}
```

`findings` is the part to read first. `severity` is `problem` — something
here will break a command, and `ok` is `false` — or `note`, which is
something to know that breaks nothing. Every finding carries its own `fix`.

`config.layers[].keys[].note` is the provenance of every key pando wrote,
verbatim from the file and so with the comment's own `#` on the front:
`# detected: <evidence>`, `# answered: <date>`, `# answered: a program,
<date>`, `# answered: --yes took the first of N options` (`# answered: --yes took the only option` when there was one), or, for the
services question, whose answer is a set, `# answered: --yes took the
<taken> of <offered> the rules resolved`. A `[[services]]` entry's note
is on the entry's own key, its place in the list: `services[0]`. A key
with a comment a developer wrote themselves carries that instead, and one
with no comment carries `null`. It is how a developer, or a program, tells
what decided each line.

A `# detected:` value that pando's rules **would not write now** is a
finding in the `config` section, at `note`. pando asks each question once
and never asks again, so a value an older rule wrote survives every
improvement to that rule — and so does a value that was right until the
repository changed under it. The finding names the key, the file, what it
holds, and what the rules offer for that key instead; the fix is to delete
the line, which is what makes the next command that needs it ask again.
pando never rewrites the value itself. A value marked `# answered:` is
never reported: that is a decision, by a person or by a program, and pando
does not second-guess decisions — so a deliberate answer closes this for
good.

`services.isolation` carries the native-versus-container decision and the
evidence behind it. It is the report a program should quote to a human
rather than re-deriving.

`hooks[].matches` is how many files the hook's `fingerprint` globs match
in the main checkout — at `0` it runs on every start, and a `hooks`
finding says so — and `null` for a hook keyed on nothing, which runs on
every start by design. `hooks[].runs` has one entry per worktree the
hook has run in: `ran_at`, and `will_run_again`, whether that worktree's
next start runs it again. A worktree it has never run in has no entry.

## `pando status --json`

```jsonc
{
  "version": 2,
  "project": { "id": "...", "root": "...", "name": "..." },
  "worktrees": [
    {
      "name": "feat+one",
      "main": false,                   // true for the main checkout, listed first
      "branch": "feat/one",
      "path": "/abs/path",
      "ports": { "web": 17008 },       // the roles this worktree holds
      "observed_ports": [17008],       // what is actually listening
      "url": "http://localhost:17008", // the readiness role's URL, or null
      "mode": "shared|namespaced|isolated", // which services it talks to
      "isolated": false,               // true exactly when mode is "isolated"
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
      "namespaces": [                  // its own, in the main checkout's servers
        { "service": "mariadb", "database": "shop__feat_one",
          "host": "localhost", "port": 3306, "in_use": true },
        { "service": "redis", "slot": 3,
          "host": "127.0.0.1", "port": 6379, "in_use": true }
      ],
      "hooks": { "install": { "fingerprint": "md5:…", "ran_at": "…" } }
    }
  ]
}
```

`main` is true for the main checkout, which pando runs too. It is the
first entry once pando has anything recorded for it — or when it is the
one named — and absent before, so a project whose main checkout was never
started lists its worktrees alone. Its `mode` is always `shared`.

`mode` says which services a worktree's processes talk to, or last
talked to once it is stopped: `shared` is the main checkout's servers and
its data, `namespaced` the main checkout's servers with a database and a
slot of the worktree's own in them, and `isolated` servers of its own. A
worktree never started reads `shared`. `isolated` came first and stays,
true exactly when `mode` is `isolated`, so a program written against it
keeps working.

`namespaces` lists what a worktree holds in the main checkout's own
servers: a `database` of its own, or a `slot` of its own, each on the
`host` and `port` it was made on. `in_use` is true while the worktree runs
namespaced, and false for one kept through a switch to another mode —
kept until `rm`, which drops it. The list is empty for a worktree that
never started namespaced. No login is ever in this shape.

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
  "version": 2,
  "project": { "id": "...", "root": "...", "name": "..." },
  "worktrees": [
    { "name": "feat+one", "main": false, "path": "...", "branch": "feat/one", "head": "abc1234",
      "detached": false, "dirty": false, "ahead": 0, "behind": 0,
      "created_by_pando": true, "mode": "shared|namespaced|isolated",
      "prunable": false, "locked": null,
      "pr": { "number": 12, "state": "open|merged|closed", "url": "..." } }
  ]
}
```

The first entry is always the main checkout, with `main` true and
`created_by_pando` false: pando runs it, and never removes it. Every other
entry is a worktree, `main` false.

`dirty`, `ahead` and `behind` are `null` when git could not be asked.
`locked` is `null` when the worktree is not locked, and the lock reason —
possibly an empty string — when it is. `pr` comes from a cache: `ls --json`
never spawns `gh`, so it works offline and may be stale or `null`.

## `pando logs <name> --json`

One JSON object per line, on stdout, forever if `-f` was passed:

```jsonc
{ "version": 2, "ts": "2026-09-20T10:00:00+00:00", "level": "debug|info|warn|error", "line": "ready in 412ms" }
```

`ts` is the line's *own* timestamp when pando could read one, normalised to
UTC, and `null` when the line has none — it is not the time pando read it.
`line` is the text with ANSI colour removed. `level` is pando's reading of
the line, not the writer's: a line containing no level word is `info`.

`--source` picks the log: a process name, a service name, or a hook name.
Without it, `dev` — or, for a worktree with no `dev` log and exactly one
process, that process. A worktree with no `dev` log and several processes
gets all of their logs merged into one stream, and then every line
carries one more key, `source`, naming the process it came from:

```jsonc
{ "version": 2, "ts": null, "level": "info", "line": "listening on 17008", "source": "api" }
```

A read of one log — any `--source` — has no `source` key. `status --json`
names every log a worktree has; pass `--source` rather than relying on
the default.

Every command that takes a worktree name also takes its branch: `feat/one`
and `feat+one` name the same worktree. The JSON always carries the
directory form, `feat+one`, in `name`.

## `pando check --json`

The result of one `pando check`: it makes a worktree of the commit a new
worktree would fork from, with no branch, installs it and starts every
process, waits until each one is ready, asks the process that owns the
worktree's URL for `/`, then stops and removes all of it. It writes under
pando's home and, for the throwaway worktree it removes again, inside
`.git`; never into the repository. Printed whatever the result — a pass,
a failure, a question still open, an interruption — as the one object on
stdout.

`pando check --base <branch>` tests the commit that branch is at, for that
run only, looked up as `new --base` looks one up; `base_ref` says which ref
it read. A base the repository does not have exits 1 with nothing tested.
`new` still forks from the project's own base, or origin/HEAD, so a pass
at another one is the setup's only once `base` is answered with it: until
then `notes` says so and the setup reads as untested.

`mode` says where it ran the project's data. `namespaced` when the
project has hooks after `services` or after `dev` to prove — a schema
step — and a `start --namespaced` of the check's worktree could run
without a question: a database, and a Redis slot, of the check's own are
made in the main checkout's servers, those hooks run there, and all of it
is dropped with the worktree, never the main checkout's own. `shared`
otherwise, as a plain start runs: those hooks are not run, since there
they would run against the developer's own data, and `notes` says which,
and — when there were some to prove — that the schema step was not
tested, and why.

```jsonc
{
  "version": 2,
  "project": { "id": "...", "root": "...", "name": "..." },
  "result": "passed|failed|not_set_up|interrupted",
  "mode": "shared|namespaced",     // where its data ran: see below
  "kind": "settings|machine|base", // whose the failure is; null unless "failed"
  "reason": "web exited with status 1 — ...", // null when "passed"
  "slot": "dev_cmd",               // the open question; null unless "not_set_up"
  "commit": "a1b2c3d4…",           // the full sha tested, or null
  "base_ref": "origin/main",       // the ref it was read from; null for HEAD
  "processes": [
    { "name": "web", "ready": true, // it came up: its port answered, or,
                                     // with no port, it stayed up
      "port": 17008,
      "http_status": 200,          // the page's status; null for every other process
      "secs": 4.2 }                // how long it took to be ready, or to fail
  ],
  "failed_process": "web",         // or "install"; null when nothing failed
  "failed_tail": ["..."],          // its last lines, with secrets hidden
  "notes": ["skipped the hooks that run after services (migrate): ..."],
  "ran_by": "tui|terminal|program",
  "pando_version": "0.5.0",
  "started_at": "2026-09-27T10:00:00Z",
  "finished_at": "2026-09-27T10:01:12Z",
  "settings_changed": false        // the settings changed while it ran
}
```

`kind` says who fixes a failure. `settings` is pando's settings for the
project — a dev command that exits, an install that fails, a schema step
that fails in a namespaced check, a page that answers `5xx` — and is
fixed through `pando init --answers -`, then checked again; a failed hook
is `failed_process`, by its name. `machine` is this machine: a shared
service nothing answers on, found before anything was made, or in a
namespaced check a server that refuses the login or a login with no grant
to make a database. `reason` then carries the command that fixes it, and
no setting changes it. `base` is the commit tested: a step failed on a
file — one its failure names, or a lockfile of a manager the install
runs — that the main checkout's branch has and that commit does not.
`reason` names both refs and `pando check --base <branch>`; no setting
fixes it, and loosening the install until it passes would pass on the
wrong commit. Which branch work starts from is the developer's to say,
and the `base` answer records it.

`ready` is about the process alone, judged as `start --wait` judges it.
The page is judged apart: a process can be `ready` with a page that
failed, and then `http_status` and `reason` say how. A check passes only
when every process is ready and the page answers.

A page answers when its status is below 500; a server that answers in
something other than plain HTTP — TLS, say — passes with a note, since
it answers. No answer within 90 seconds fails: a dev server compiling
its first page gets that long.

Exit codes: `0` passed, `1` failed or interrupted, `3` a question is still
open. On exit 3 the object has `"result": "not_set_up"`, `slot` naming the
question, `reason` and `commit` null and `processes` empty, and the
question itself is on stderr, as every exit 3 puts it; nothing was made.
When another check of the same project is running, it exits 1 with
nothing on stdout: that check's result is the one to read.

`failed_tail` has at most ten lines. Passwords in URLs, the values of keys
named like a secret, and bearer tokens read `(hidden)`: this list is
meant to be pasted into a conversation. `settings_changed` true means the
project's run settings were edited while the check ran, so the result
speaks for neither the old ones nor the new.

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
| `null` | "none of them", where `allow_none` is true. At `schema_hook` it is "no": the step is written with `on = "never"` |
| `[]` | "none of them" at the set question. A usage error anywhere else — `null` is how you say none |
| `{"api": {...}, "web": {...}}` | at `processes` only: process tables of your own, one per key, each exactly what `[processes.<name>]` takes — `cmd`, and optionally `cwd`, `ports`, `env` and `ready` |

The object is how a project of several processes is answered when no
option fits it. A string of your own at `processes` is **one** command,
written as `[dev]`; the `name: cmd in dir; …` text pando shows its own
per-app option in names that option and is never a way to describe one.

```jsonc
{ "processes": {
    "api":    { "cmd": "uv run uvicorn app.main:app --port {port}", "cwd": "backend",
                "ports": ["api"] },
    "worker": { "cmd": "uv run python -m app.worker", "cwd": "backend", "ports": [] },
    "web":    { "cmd": "npm run dev", "cwd": "frontend", "ports": { "PORT": "web" },
                "env": { "API_URL": "http://127.0.0.1:{port:api}" },
                "ready": { "timeout_s": 90 } } } }
```

`ports` is a list of the roles the process owns, whose port reaches it
as `{port}` or `{port:<role>}`, or a map of environment variable to role;
`[]` is a process with no port, a worker. `{port:<role>}` in `cmd` or
`env` may name any process's role or a service's name. `ready` takes
`role` and `timeout_s`. Each table is written with
`# answered: a program` on its header.

**Answers are by value, never by index.** An index breaks the day a rule
finds one more candidate; the text does not, and picking an option by its
text is what brings everything else the option carries — the ports a
command owns, the whole process table a workspace answer is, a service's
env key, the hook entry.

Refusals, all exit 2 and all naming the key: a name that is not a question;
a shape the question cannot take; a value that is not one of the options at
a question that has them; an empty string; a `prelude` that fails its own
probe on this machine, which is never written down; a `port_env` of your
own that is not environment variable names; a string at `processes` or
`dev_cmd` in the `name: cmd in dir; …` form that is none of the options;
process tables with a key a table does not take, no `cmd`, a variable
that is not a name, a `cwd` that is not a directory of the repository or
leaves it, a role two processes claim, or a `{…}` nothing will fill. Each
refusal says what the question does take.

A `port_env` of your own may name several variables, as one string
separated by commas, `"PORT, API_PORT"` — the way the option naming
several is written — and each owns the role its name says: `PORT` owns
`web`, `API_PORT` owns `api`. One variable owns `web` whatever it is
called.

An answer for a slot that was
already answered (without `--replace`), or that nothing asked about, is
**reported on stderr and not applied** — it is not an error, and the run
still exits 0. A slot with no proposal at all is not "nothing asked about":
it takes a custom answer, except at `services` and `prelude`. See the
three-state table above.

`--dry-run` runs the same pass against copies of the files it would write
and prints them on stdout, config first. Use it to show a diff before
writing. A question nothing answers is not a failure there: the slot is
listed as `(unanswered)` and the dry run still exits 0.

### Correcting an answer: `--replace`

`pando init --answers - --replace` applies the file to questions that are
already answered too. Preview it first with
`pando init --answers - --dry-run --replace`.
It is how a setup found wrong is corrected without editing `pando.toml`.
Every slot the file names takes the file's answer, in place of what config
says and of what a rule would decide, through the same checks a first
answer gets. It is written with the same note, `# answered: a program`
and the date, and an `answer` line in the decisions log, so the change is
never read as a person's override. Only pando's own config for the project
is written; a slot the file does not name is left as it is. `--replace`
needs `--answers`; without it, clap refuses the command with exit 2.

Refused under `--replace`, exit 2, before anything is written:

- a `prelude` that is already answered. The prelude is about the machine,
  and only a person changes it. If it has no answer yet, the file's answer
  is a first answer, as without the flag.
- `processes`, `services` or `schema_hook` when the committed `pando.toml`
  or the machine-wide config declares them. pando never writes those files,
  and its own config written over them would hide their tables or merge
  with them rather than replace them.
