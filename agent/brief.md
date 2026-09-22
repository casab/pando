# Setting pando up on a project, and running it

This is the procedure. It is written once and both host packagings — the
Claude Code plugin in `skills/`, the Codex skills in `codex/` — point at
this file rather than repeating any of it. If you are reading one of those
wrappers and it seems to contain reasoning, the wrapper is wrong.

You will need [`json.md`](./json.md) beside this file: it is the contract
for every shape named here, and it is versioned.

Your job in one sentence: **turn the evidence pando already publishes into
answers, ask the human only the things that are genuinely theirs, and write
nothing except through `pando init --answers`.**

---

## 0. Read before you ask

Two commands, in this order, before you form any opinion:

```bash
pando signals            # what the repository says about how to run itself
pando doctor --json      # what this machine answers, and what is wrong
```

`signals` is read-only, spawns nothing, and is identical on two runs.
`doctor` is the half that probes the machine: the login shell, the tools on
`PATH`, the engines a private service would need.

**Do not re-derive any of this.** No `cat package.json`, no `ls`, no
`docker compose config`, no `node --version`. Everything those would tell
you is already in the two objects above, and where it is *not*, that
absence is itself the answer — a rule looked and found nothing, and your
job is to notice that, not to go looking with different eyes.

Read the repository's own files only when you are about to ask a human
something and need one more sentence to make the question intelligible.
Never as a substitute for `signals`.

## 1. The question budget

> On a project the rules fully understand: **zero** questions.
> On a project they half understand: **one**.

Two things, and only two, are genuinely a human's:

1. **Which mechanism runs private services on this laptop** — containers or
   engines installed on the machine. It is a preference about their
   computer, not a fact about their repository. See §5.
2. **Which apps of a monorepo they want running.** It is a preference about
   their work this week.

Everything else — the install command, what pins the runtime, the dev
command, how the port reaches it, the schema step, which local files a
worktree needs — is *evidence*, and evidence is the rules' job. If you
find yourself wanting to ask about one of those, you have either not read
`signals` properly or you have found a genuine gap in the rules. In the
second case the right move is to answer it from the evidence, let pando
record it (see §7), and say so in your summary — that record is what makes
the rule better for everyone who has no agent.

If you must ask, ask **once**, with the options pando published and the
`why` beside each. Never ask a question whose answer is in `signals`.

## 2. Reading `signals`: three states, three behaviours

`slots` has nine entries, one per question pando can ask, in the order it
asks them. Each has a `proposal`, and its state decides what you do:

| State | Means | You |
|---|---|---|
| `"proposal": null` | no rule had anything to say | **do nothing.** There is no question here — with one exception, `prelude`, which is never proposed in `signals` at all. See below |
| `"decided": true` | a rule settled it | **do nothing.** Same |
| `"decided": false` | pando will ask | **this is the only one you answer** |

Also check `"answered": true` — config already says, from some layer, and
the slot is closed.

Getting this wrong is the commonest way to waste a run. A project with no
lockfile has `"install": {"proposal": null}`: pando will not propose a
non-frozen install and neither will you. A `decided: true` proposal with no
candidates and a `none_because` is a rule deciding the answer is *none of
them* — a real answer, and the opposite of `null`.

Three more facts that are not visible in the shape:

- **`prelude` is never proposed by `signals`** — it reads as
  `"proposal": null` on every project, answered or not. It is the one
  question about the laptop rather than the repository and it costs a
  shell probe, so it is raised at the moment an answer is needed, which
  means `init` can exit 3 on a slot `signals` showed you nothing for.
  `doctor`'s `runtime` section is the evidence: what the project pins,
  what `bash -lc` resolves here, and the exact line that would reconcile
  them, as the finding's `fix`. That line is the developer's to approve —
  it runs in front of every command pando spawns for them. Once they have,
  `{"prelude": "<the line>"}` is how it goes in, and `{"prelude": null}`
  is "this machine needs nothing". Never install a runtime to make the
  question go away.
- **Answering `processes` with the per-app form settles `dev_cmd` and
  `port_env` too** — every app gets its command and its port. Answers you
  sent for those two are then reported as unused, which is correct and not
  an error.
- **A `needs_a_human: true` candidate is not one `--yes` may take**, but an
  answers file naming its exact text *is* an explicit answer and is
  accepted. Seeding a worktree's `.env` from a committed example is the one
  that behaves this way: it creates a file out of contents pando did not
  write, so nobody's flag gets to decide it. If you name it, you are
  answering for the developer — be sure they want it.

## 3. The nine questions, and who answers each

| Question | Who | Notes |
|---|---|---|
| `install` | rules | a frozen install or nothing. Never propose a non-frozen one |
| `version_files` | rules | which file pins the runtime |
| `prelude` | machine → human | only when the machine does not resolve the pin. `doctor` gives the exact line; the human decides whether to run it |
| `processes` | **human** | one process, or one per app of a workspace |
| `dev_cmd` | rules | ask only when several scripts are plausible dev servers |
| `port_env` | rules | which variables carry the ports. The env example beats a framework convention |
| `services` | rules + **human** | which services get a private copy — see §5 for the mechanism |
| `schema_hook` | rules | the command that brings a fresh database to the schema |
| `provision` | rules, mostly | which local files a worktree needs. Seeding from an example needs a human |

## 4. Writing: `pando init --answers`, and nothing else

```bash
pando init --answers answers.json --dry-run   # look first
pando init --answers answers.json             # then write
```

`answers.json` is one object, keys are the slot names above, and:

- a **string** is the option whose `value` is exactly that text — **by
  value, never by index.** An index breaks the day a rule finds one more
  candidate, and picking by text is what brings everything else the option
  carries with it: the ports a command owns, the whole process table a
  workspace answer is, a service's env key, the hook entry.
- a **list of strings** is the set answer at the one question where
  `multi` is true, and the whole list at `version_files` and `provision`.
- **`null` is the only spelling of "none"**, and only where `allow_none`
  is true. An empty string is a usage error. `[]` means "none of them" at
  the set question and is a usage error anywhere else.
- a string that matches no option is a command of your own, wherever
  `allow_custom` is true.

**Never edit `pando.toml`.** Not with `sed`, not with an editor, not "just
this once". Every answer that goes through `init --answers` is validated,
is refused if it would make the config unloadable, and lands with
`# answered: a program, <date>` beside it, so the developer can see at a
glance which lines a machine chose. A key you wrote by hand has none of
that and is indistinguishable from one they wrote themselves.

There is **one thing you cannot write**, and it matters: the
native-versus-container preference lives in `[isolation] prefer` in the
developer's own `~/.pando/config.toml`. It is a machine-wide preference,
not one of the nine questions, and there is no `--answers` key for it. If
the project has both mechanisms available, quote pando's own line to the
developer and let them set it. See §5.

## 5. Private services: container, native, or neither

This is the one place where reading carefully beats reasoning quickly.

pando decides in this order, and publishes every step of it in
`doctor --json` under `services.isolation`:

1. **What the project declares.** A compose file with a service the
   *application depends on*; an address in the env example that names an
   engine pando has a recipe for.
2. **What this machine can run.** Whether docker answers, and whether each
   recipe's binaries exist in the shell pando actually spawns.
3. **The preference** — and only to break a tie.

### A compose file is not automatically a container option

**A compose file whose only service is built from the repository is not a
container option at all — it packages the application.** `build:` pointing
inside the project means that service *is* the app: running a private copy
of it per worktree would run a second copy of the thing pando is already
starting. pando filters those out, and if nothing is left it records the
negative so the question never comes back:

```jsonc
"services": { "proposal": { "decided": true, "candidates": [],
  "none_because": "docker-compose.yml declares only app, built from this repository" } }
```

Seeing that, you do nothing. There is no question, there is no mechanism to
choose, and asking the human "containers or native?" here would be asking
them to choose between two things that do not exist.

### A project can need services and describe none

The opposite shape: nothing in the repository says how to run anything, but
the env example addresses a database and a cache.

```jsonc
"env_example": [["DATABASE_URL", "postgres://user:pass@localhost:5432/appdb"],
                ["CACHE_URL", "redis://localhost:6379"]]
```

That is a declaration too — the application plainly needs a Postgres and a
Redis. With no compose file there is no container option, so pando proposes
its own recipes, and the engine comes from the **URL scheme or the default
port**, never from the key's name. `DATABASE_URL` says nothing about which
database; `postgres://` says everything.

Whether that proposal is `decided` depends on the machine: decided when the
engines are installed, a question when one is not — and the missing engine
is still offered, because the project plainly wants it. If it is a question
and you know the human wants those services, answer it by naming them.
**Never install an engine.** Not with brew, not with apt, not with a
container. Report what is missing and let the human decide.

### When both are real

Both a compose file with real dependencies *and* engines the machine has:
that is the one genuine tie, and pando breaks it towards the project's own
compose file, saying so:

```
nobody has said which to prefer, so the project's own compose file wins —
set `[isolation] prefer = "native"` in ~/.pando/config.toml to run the
recipes instead
```

Quote that line to the developer, once, and stop. You cannot write it,
`--answers` has no key for it, and it is a preference about their laptop.
If they have no docker and the engines are there, pando has already chosen
the recipes on the evidence and there is nothing to ask at all.

## 6. Verify, don't claim

A setup ends with proof, not with a summary.

1. `pando init --answers answers.json --dry-run` — stdout is the file as it
   would be, with the provenance comments. Show it to the developer.
2. `pando init --answers answers.json` — the real write.
3. `pando doctor` — and **report what it said**, including the notes.
   Exit 0 means nothing found will break a command; exit 1 means something
   will, and every problem is printed with its own fix.

If doctor exits 1, read the finding before touching anything. Some are not
yours to fix:

- **a runtime the machine does not resolve.** The fix is a `prelude` line,
  and `doctor` prints the exact one. It is about their laptop: offer it,
  do not run an installer.
- **a non-frozen install in a config somebody wrote by hand.** pando will
  not run one. Tell them; do not "fix" it by loosening anything.

Do not claim a project starts unless you started it. If you did not run
`pando start`, say that you did not.

## 7. What pando records about you

Every answer you supply to a question the rules could not decide is
appended to `~/.pando/projects/<id>/decisions.jsonl` with the evidence you
had, and a later line records it if the developer changes it. You do not
write that file; pando does.

This is deliberate and it is in your interest to make it accurate. It is
the corpus that turns a question the rules could not answer into a rule —
which is what every developer without an agent gets. So: answer from the
published evidence, or refuse. **An answer you guessed pollutes a corpus
somebody will train a rule on.** A question asked is cheap; a wrong config
written confidently is not.

## 8. Guardrails

Absolute. None of these has an exception worth taking.

- **Never write into the developer's repository.** Not a config file, not a
  cache, not a marker, not a `.env`. pando's own promise is "not a byte",
  and a developer will not distinguish your plugin from the tool. If a
  project cannot run without an untracked file, say which file and why, and
  let them create it in their own project.
- **Never propose or run a non-frozen install.** `npm ci`, not
  `npm install`. `pnpm install --frozen-lockfile`, not `pnpm install`. No
  lockfile means no install step — silence is the answer.
- **Never run a mutating pando command against a repository the developer
  did not point you at.** `new`, `start`, `stop`, `rm`, `share`, `init`
  are mutating. `ls`, `status`, `path`, `logs`, `doctor`, `signals` are
  not. Check the working directory is the repository they asked about.
- **Never install a toolchain or a database engine.** Not node, not a
  version manager, not Postgres, not docker. Report what is missing, with
  what pando said about it.
- **Never edit `pando.toml`, and never edit a framework config file.** If
  an app hardcodes its port, that is a one-line change in *their* project
  and their decision to make.
- **Never take a `needs_a_human` option without a human.** If nobody is
  there to ask, exit and say which question is open.

---

# Worked examples

Real output, from real runs. Your project will differ; the shape of the
reasoning will not.

## A. A single-app repository the rules fully understand

`signals` (abridged) — every slot either decided or silent:

```
install     decided=True   ['npm ci']            why: package-lock.json
dev_cmd     decided=True   ['npm run dev']       why: package.json scripts.dev
port_env    decided=True   ['PORT']              why: the Node convention
provision   decided=True   ['.env']              why: gitignored and present in the main checkout
services    proposal null
schema_hook proposal null
```

**Questions to ask: none.** There is no answers file to write at all.

```bash
pando init          # no --yes needed: nothing is undecided
pando doctor
```

`init` prints one line per slot saying what it took and why. If you pass
`--yes` here you have added nothing and told the config a flag decided
something the rules did. Do not.

## B. A workspace monorepo with several apps

Several apps under `apps/`, workspaces declared in `package.json`, and —
in this shape — no lockfile at all:

```
install     proposal null                        ← no lockfile, so no frozen install exists
processes   decided=False
              1) 'api: npm run dev in apps/api; web: npm run dev in apps/web'
                    why: a dev script in each of 2 workspace apps
              2) 'npm run dev'
                    why: package.json scripts.dev
port_env    decided=False  ['WEB_PORT, API_PORT', 'WEB_PORT', 'API_PORT']
```

`processes` is the human question — which apps they want running — and it
is the *only* one, because taking the per-app form settles the dev command
and the ports for every app with it.

Ask once, with both options and their `why`. Then:

```json
{ "processes": "api: npm run dev in apps/api; web: npm run dev in apps/web" }
```

Note what you did **not** do: you did not answer `install` (there is no
question — and inventing `npm install` would be a non-frozen install), and
you did not answer `port_env` (the `processes` answer settles it; pando
will report your value as unused if you send it, which is noise in front of
the thing that matters).

## C. A compose file that only packages the app

```jsonc
"compose": [ { "file": "docker-compose.yml", "services": ["app"], "extends": [], "include": false } ],
"services": { "proposal": { "decided": true, "candidates": [],
  "none_because": "docker-compose.yml declares only app, built from this repository" } }
```

There is a compose file, and there is still **no container question**. The
one service is built from the repository: it *is* the application. pando
records the negative — `include = []` — so the question never comes back on
an isolated start.

**Questions to ask: none.** Not about services, not about mechanisms. If
you ask the developer "should I run your services in containers?" here, you
have asked them about something that does not exist, and you have spent the
entire question budget doing it.

Afterwards, `doctor` says so in its own words, which is what you report:

```
services
  isolation     nothing here to run a private copy of
                docker-compose.yml declares nothing this project depends on
                nothing in its env example names an engine pando has a recipe for
```

## D. Services with no manifest — where the machine decides

```
services  decided=<depends on this machine>  mechanism=native
    * postgres | why: .env.example DATABASE_URL=postgres://…; postgres is on this machine
    * redis    | why: .env.example CACHE_URL=redis://localhost:6379
```

No compose file, so there is no container option and the preference never
comes into it. The app's own addresses name the engines. On a machine that
has them, this is `decided` and you do nothing; on one that does not, it is
a question whose options are still those two engines, and the `why` says
what is missing.

If you answer it — `{"services": ["postgres", "redis"]}` — say plainly in
your report that the engines are not installed and that `start --isolated`
will fail until they are. Then stop. Installing Postgres is not your
decision to make on somebody's laptop.

---

# Operating, day to day

Setup happens once. This is the part you do every day, and it is a much
smaller contract.

**Never parse human-readable output.** Every command below has a `--json`
form or an exit code that answers the question. The text is for people and
it changes.

```bash
pando status --json              # what is running, on which ports, with which URL
pando status <name> --json       # one worktree
pando ls --json                  # the worktrees and their git state
pando logs <name> --json         # a log, one JSON object per line
pando logs <name> --source <s> --json
pando new <branch>               # create a worktree
pando start <name>               # start it
pando start <name> --isolated    # …with private copies of its services
pando stop <name>
pando share <name>               # publish it at a public URL
pando unshare <name>
```

### The exit-code discipline

| Code | You |
|---|---|
| `0` | carry on |
| `1` | read stderr. It is one sentence. **Do not retry** — a failure that repeats is a failure that repeats |
| `2` | you asked wrongly: a bad flag, or an answers file naming a question pando does not ask. Fix the request |
| `3` | **a question is unanswered, and it is on stderr** with its options. Answer it through `init --answers`, or put it to the human. Never retry unchanged, and never add `--yes` to make it go away |

`--yes` is not a way past exit 3. It takes the rules' own preferred option,
which is a decision you are making on the developer's behalf with no
evidence you did not already have. Use it only when the developer asked for
it.

One trap: `init --answers` exits **1**, not 2, when a `prelude` you
supplied fails its probe on this machine. stderr says so. It is a fact
about the laptop, not a shape error.

### Reading a failure

```bash
pando status <name> --json
```

- a process with `"phase": "failed"` carries `"reason"` — the sentence to
  show the human — and `"log"`, the file to read with
  `pando logs <name> --source <process>`.
- `"up": false` on a service means nothing is answering on its port.
- `"logging": false` with `"up": true` means the log pump died: the log tab
  has stopped filling. `pando start` or `pando restart` puts it back.
- `"observed_ports"` is what is really listening, against `"ports"`, which
  is what pando assigned.

### What not to do while operating

- Do not `start` a worktree the developer did not name.
- Do not `rm` anything. Removing a worktree is theirs.
- Do not `share` without being asked: it publishes their machine on the
  public internet.
- Do not restart something to "see if it works" while they are using it.
- Do not read log files from disk. `pando logs` handles truncation,
  partial lines and levels; opening the file gets you none of that.
