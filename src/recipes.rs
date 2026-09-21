//! Recipes: what pando knows about a thing it did not write.
//!
//! A recipe is **data**. For a service it says what initialises a data
//! directory, what starts the server on a port pando allocated, and how to
//! tell when it is ready; for a language it says which files pin it and
//! which version managers can satisfy it. Built-ins are compiled into the
//! binary as TOML text and go through exactly the same parser as a file in
//! `~/.pando/recipes/`, so a built-in has no privilege a developer's own
//! file does not: dropping in `postgres.toml` replaces the Postgres recipe
//! outright.
//!
//! **One loader, two kinds of recipe.** Every recipe shares an envelope —
//! `kind`, `name`, `aliases`, the binaries it needs on PATH, how to ask one
//! for its version, how to install it, a note worth printing — and carries
//! one body table, `[service]` or `[language]`. The language body is the
//! shape of [`crate::runtime::LANGUAGES`], which is a `const` table today
//! and was deliberately built to be loadable from disk later. Nothing here
//! migrates it; a round-trip test in this module is the proof that the
//! format could, and that "one loader" is not a slogan.
//!
//! Nothing in this module runs anything. It reads TOML and hands back
//! structs; `native.rs` is what spawns, and `doctor.rs` is what reports.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which body a recipe carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Service,
    Language,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Service => "service",
            Kind::Language => "language",
        }
    }
}

/// How to start, wait for, and address one server.
///
/// Every field is a shell command run through `bash -lc`, with the
/// placeholders [`crate::native`] documents. `cmd` is the only one that is
/// required: a server pando cannot start is not a service recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceRecipe {
    /// Starts the server in the foreground, on `{port}`. Detached by
    /// pando, so it must not daemonise itself: a recipe that forks leaves
    /// pando holding the pid of something that has already exited.
    pub cmd: String,
    /// Initialises `{datadir}`. Runs once, and never against a data
    /// directory that already holds data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<String>,
    /// Runs after every readiness, and has to be idempotent: it is where a
    /// recipe creates the role and database the app's own URL names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    /// Answers "is it up?" with its exit status. Omitted means a TCP
    /// connect, which only proves something is behind the port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout_s: Option<u64>,
    /// The environment key an app usually reads to find this service, used
    /// when the `[[services]]` block names no `env` map of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_env: Option<String>,
    /// What `{db_user}` means when the app's own URL names no user, and
    /// what `{db_name}` means when it names no database. Recipe data
    /// rather than adapter policy: `postgres`/`postgres` is true of
    /// Postgres and of nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_name: Option<String>,
}

/// One file that pins a language, in the shape [`crate::runtime::Source`]
/// already has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSource {
    pub file: String,
    /// The path to the value inside a TOML file, as
    /// `rust-toolchain.toml`'s `["toolchain", "channel"]`. Empty means the
    /// whole file is the version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toml_key: Vec<String>,
}

/// What a project says when it pins a language, and who can satisfy it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LanguageRecipe {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RecipeSource>,
    /// The `package.json` `engines` key that describes it, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines_key: Option<String>,
    /// Version managers that can satisfy it, by name, in the order a
    /// question should offer them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Service(ServiceRecipe),
    Language(LanguageRecipe),
}

/// One recipe, envelope and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub kind: Kind,
    pub name: String,
    /// Other names this recipe answers to. `.tool-versions` calls node
    /// `nodejs`; a developer may well call MariaDB `mysql`.
    pub aliases: Vec<String>,
    /// One line for a report.
    pub summary: Option<String>,
    /// What has to be on PATH, in the order to try them. The first one is
    /// what a version probe asks.
    pub binaries: Vec<String>,
    /// What to pass a binary to make it print its version.
    pub version_flag: Option<String>,
    /// How a developer installs it. **Printed, never run** — pando does
    /// not install an engine.
    pub install: Option<String>,
    /// Something true about this recipe that a developer should know
    /// without reading it: Postgres's trust authentication, for one.
    pub notes: Option<String>,
    pub body: Body,
}

impl Recipe {
    pub fn service(&self) -> Option<&ServiceRecipe> {
        match &self.body {
            Body::Service(s) => Some(s),
            Body::Language(_) => None,
        }
    }

    /// The service body, to be overridden by what a `[[services]]` entry
    /// says inline.
    pub fn service_mut(&mut self) -> Option<&mut ServiceRecipe> {
        match &mut self.body {
            Body::Service(s) => Some(s),
            Body::Language(_) => None,
        }
    }

    pub fn language(&self) -> Option<&LanguageRecipe> {
        match &self.body {
            Body::Language(l) => Some(l),
            Body::Service(_) => None,
        }
    }

    /// The binary a version probe asks, which is the first one listed.
    pub fn version_cmd(&self) -> Option<String> {
        let binary = self.binaries.first()?;
        let flag = self.version_flag.as_deref()?;
        Some(format!("{binary} {flag}"))
    }
}

/// Where a recipe came from, which is the first thing a developer asks
/// when one behaves unexpectedly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Compiled into this build.
    BuiltIn,
    /// A file the developer put in the recipes directory.
    User(PathBuf),
}

impl Origin {
    /// `built-in`, or the path of the file that replaced it.
    pub fn describe(&self) -> String {
        match self {
            Origin::BuiltIn => "built-in".to_string(),
            Origin::User(path) => path.display().to_string(),
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Origin::User(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub recipe: Recipe,
    pub origin: Origin,
    /// Whether a built-in of the same name was replaced by this file.
    pub replaces_built_in: bool,
}

/// A user file that did not parse. Kept rather than dropped: the name it
/// claimed is *unusable*, not quietly served by the built-in it was
/// meant to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub path: PathBuf,
    pub error: String,
}

/// Every recipe this build and this machine have, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipes {
    entries: BTreeMap<String, Loaded>,
    broken: BTreeMap<String, Broken>,
}

impl Recipes {
    /// The recipes compiled into this build, with nothing read from disk.
    pub fn built_in() -> Recipes {
        let mut entries = BTreeMap::new();
        for (name, text) in BUILT_IN {
            // A built-in that does not parse is a bug in this build, not a
            // developer's problem, and `every_built_in_recipe_parses`
            // fails the commit that introduces one. Skipped rather than
            // panicked over, so a bad build still runs `pando ls`.
            match parse(text) {
                Ok(recipe) if recipe.name == name => {
                    entries.insert(
                        name.to_string(),
                        Loaded {
                            recipe,
                            origin: Origin::BuiltIn,
                            replaces_built_in: false,
                        },
                    );
                }
                _ => continue,
            }
        }
        Recipes {
            entries,
            broken: BTreeMap::new(),
        }
    }

    /// The built-ins with the developer's own recipes merged over them.
    ///
    /// A file's *stem* is the name it claims, so "drop in a file of the
    /// same name and it replaces the built-in" is literally true. A file
    /// whose `name` disagrees with its stem is refused naming both, rather
    /// than silently registering under one of them.
    pub fn load(dir: &Path) -> Recipes {
        let mut recipes = Recipes::built_in();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return recipes;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .filter(|path| {
                !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            })
            .collect();
        // Sorted, so two files cannot swap which of them was read last
        // between runs on filesystems that do not order a directory.
        files.sort();
        for path in files {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            match read(&path, &stem) {
                Ok(recipe) => {
                    let replaces_built_in = BUILT_IN.iter().any(|(name, _)| *name == stem);
                    recipes.broken.remove(&stem);
                    recipes.entries.insert(
                        stem,
                        Loaded {
                            recipe,
                            origin: Origin::User(path),
                            replaces_built_in,
                        },
                    );
                }
                Err(e) => {
                    // The built-in is *not* used in its place. A developer
                    // who edits `postgres.toml` and mistypes one line must
                    // not have pando quietly run the built-in against the
                    // data directory their own recipe made.
                    recipes.entries.remove(&stem);
                    recipes.broken.insert(
                        stem,
                        Broken {
                            path,
                            error: format!("{e:#}"),
                        },
                    );
                }
            }
        }
        recipes
    }

    /// The recipe `name` asks for, or an error naming the ones there are.
    pub fn get(&self, name: &str) -> Result<&Loaded> {
        if let Some(loaded) = self.entries.get(name) {
            return Ok(loaded);
        }
        if let Some(broken) = self.broken.get(name) {
            bail!(
                "the recipe {name:?} is in {} and does not load: {} — fix that file, or delete \
                 it to go back to the built-in",
                broken.path.display(),
                broken.error
            );
        }
        // An alias is a second chance before the refusal, not a first
        // lookup: a file named `mysql.toml` beats a built-in that merely
        // answers to `mysql`.
        if let Some(loaded) = self
            .entries
            .values()
            .find(|l| l.recipe.aliases.iter().any(|a| a == name))
        {
            return Ok(loaded);
        }
        bail!(
            "there is no recipe named {name:?} — this build knows {}",
            self.listed()
        )
    }

    /// Every name, built-in and user, alphabetically.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The names of every recipe that starts a server.
    pub fn service_names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, l)| l.recipe.kind == Kind::Service)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Loaded)> {
        self.entries.iter().map(|(name, l)| (name.as_str(), l))
    }

    pub fn broken(&self) -> impl Iterator<Item = (&str, &Broken)> {
        self.broken.iter().map(|(name, b)| (name.as_str(), b))
    }

    fn listed(&self) -> String {
        if self.entries.is_empty() {
            return "none".to_string();
        }
        self.names().join(", ")
    }
}

/// Reads one recipe file, checking that it claims the name its file does.
fn read(path: &Path, stem: &str) -> Result<Recipe> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let recipe = parse(&text)?;
    if recipe.name != stem {
        bail!(
            "it is called {:?} inside and {stem:?} by its file name — a recipe is found by its \
             file name, so rename one of them",
            recipe.name
        );
    }
    Ok(recipe)
}

/// The one parser, for a built-in string and a developer's file alike.
pub fn parse(text: &str) -> Result<Recipe> {
    let raw: RawRecipe = toml::from_str(text).context("parse the recipe")?;
    if raw.name.trim().is_empty() {
        bail!("a recipe needs a `name`");
    }
    if raw.name.contains(['/', '\\']) || raw.name == "." || raw.name == ".." {
        bail!(
            "the recipe name {:?} is not a name — it is what a file is called and what a \
             `[[services]] preset` says",
            raw.name
        );
    }
    let body = match raw.kind {
        Kind::Service => {
            if raw.language.is_some() {
                bail!("a `kind = \"service\"` recipe may not carry a `[language]` table");
            }
            let service = raw
                .service
                .context("a `kind = \"service\"` recipe needs a `[service]` table")?;
            if service.cmd.trim().is_empty() {
                bail!("`[service] cmd` is what starts the server, and it must not be empty");
            }
            Body::Service(service)
        }
        Kind::Language => {
            if raw.service.is_some() {
                bail!("a `kind = \"language\"` recipe may not carry a `[service]` table");
            }
            Body::Language(
                raw.language
                    .context("a `kind = \"language\"` recipe needs a `[language]` table")?,
            )
        }
    };
    Ok(Recipe {
        kind: raw.kind,
        name: raw.name,
        aliases: raw.aliases,
        summary: raw.summary,
        binaries: raw.binaries,
        version_flag: raw.version_flag,
        install: raw.install,
        notes: raw.notes,
        body,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    kind: Kind,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    binaries: Vec<String>,
    #[serde(default)]
    version_flag: Option<String>,
    #[serde(default)]
    install: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    service: Option<ServiceRecipe>,
    #[serde(default)]
    language: Option<LanguageRecipe>,
}

/// The recipes this build ships, as `(file name, TOML text)`.
///
/// One engine for now, and deliberately: a path proven end to end with a
/// real server is worth more than four recipes nothing has run.
pub const BUILT_IN: [(&str, &str); 1] = [("postgres", POSTGRES)];

/// PostgreSQL.
///
/// Two things here are not obvious and are both deliberate:
///
/// - **`-k {socket_dir}`.** Postgres puts its Unix socket inside the data
///   directory unless told otherwise, and `sun_path` is 104 bytes on
///   macOS. A project name plus a branch name overflows that, so the
///   socket goes to a short, hashed path in the temporary directory and
///   every client here connects over TCP.
/// - **`--auth trust` on `127.0.0.1`.** A development database on a random
///   high port bound to loopback gets trust authentication, because the
///   alternative is a password the developer never asked for and cannot
///   find. `doctor` says so in one line. Anyone who wants a password drops
///   their own `postgres.toml` in the recipes directory.
const POSTGRES: &str = r#"
kind = "service"
name = "postgres"
aliases = ["postgresql", "pg"]
summary = "PostgreSQL: a cluster of this worktree's own, on a port pando allocated"
binaries = ["postgres", "initdb", "pg_isready", "psql", "createdb"]
version_flag = "--version"
install = "brew install postgresql@16   (or your distribution's postgresql-server package)"
notes = "trust authentication on 127.0.0.1 — any password in the URL is accepted, and nothing off this machine can reach it"

[service]
port_env = "DATABASE_URL"
ready_timeout_s = 60

# What initdb makes, and so what `{db_user}` and `{db_name}` mean when the
# app's own URL names neither.
db_user = "postgres"
db_name = "postgres"

init = "initdb --pgdata {datadir} --username postgres --auth trust --encoding UTF8 --no-locale"

# `exec`, so the pid pando records is the server's and not a shell that is
# waiting on it. `listen_addresses` is spelled out because the default,
# `localhost`, also binds `::1`.
cmd = "exec postgres -D {datadir} -p {port} -k {socket_dir} -c listen_addresses=127.0.0.1"

# `-U postgres -d postgres`, not because pg_isready authenticates — it
# does not, and returns 0 either way — but because the connection it opens
# is refused by name, and a bare probe leaves
# `FATAL: role "<your login>" does not exist` in the server's own log every
# time it is asked. A developer reading that log has enough to worry about.
ready = "pg_isready -h 127.0.0.1 -p {port} -U postgres -d postgres -q"

# Idempotent, and run after every readiness: the role and the database the
# app's own URL names have to exist, and which ones those are can change
# without the data directory changing at all. `{db_user}` and `{db_name}`
# come from the URL pando rewrote, and are `postgres` when there is none —
# in which case both lines are no-ops, since initdb made them.
create = '''
psql -h 127.0.0.1 -p {port} -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "SELECT 1 FROM pg_roles WHERE rolname = '{db_user}'" | grep -q 1 || psql -h 127.0.0.1 -p {port} -U postgres -d postgres -v ON_ERROR_STOP=1 -c "CREATE ROLE \"{db_user}\" LOGIN SUPERUSER"
psql -h 127.0.0.1 -p {port} -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "SELECT 1 FROM pg_database WHERE datname = '{db_name}'" | grep -q 1 || createdb -h 127.0.0.1 -p {port} -U postgres -O "{db_user}" "{db_name}"
'''
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    const MINIMAL: &str = "kind = \"service\"\nname = \"tiny\"\n\n[service]\ncmd = \"sleep 1\"\n";

    #[test]
    fn every_built_in_recipe_parses_and_claims_its_own_file_name() {
        for (name, text) in BUILT_IN {
            let recipe = parse(text).unwrap_or_else(|e| panic!("built-in {name}: {e:#}"));
            assert_eq!(
                recipe.name, name,
                "built-in {name} calls itself something else"
            );
        }
        let recipes = Recipes::built_in();
        assert_eq!(recipes.names(), vec!["postgres"]);
        assert_eq!(recipes.service_names(), vec!["postgres"]);
    }

    #[test]
    fn the_postgres_recipe_says_how_to_init_start_wait_and_address_it() {
        let loaded = Recipes::built_in().get("postgres").unwrap().clone();
        assert_eq!(loaded.origin, Origin::BuiltIn);
        let service = loaded.recipe.service().expect("a service recipe");
        assert!(service.init.as_deref().unwrap().contains("initdb"));
        assert!(service.cmd.contains("{port}"), "{}", service.cmd);
        // The whole point of the socket directory: never inside the data
        // directory, which is long enough to overflow `sun_path`.
        assert!(service.cmd.contains("-k {socket_dir}"), "{}", service.cmd);
        assert!(!service.cmd.contains("-k {datadir}"), "{}", service.cmd);
        assert!(service.ready.as_deref().unwrap().contains("pg_isready"));
        assert_eq!(service.port_env.as_deref(), Some("DATABASE_URL"));
        assert_eq!(
            loaded.recipe.version_cmd().as_deref(),
            Some("postgres --version")
        );
        // Printed, never run — but it has to exist, because a missing
        // engine is a sentence naming what to install.
        assert!(
            loaded
                .recipe
                .install
                .as_deref()
                .unwrap()
                .contains("postgresql")
        );
        assert!(loaded.recipe.notes.as_deref().unwrap().contains("trust"));
    }

    #[test]
    fn a_user_file_replaces_the_built_in_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let recipes = Recipes::load(dir.path());
        assert!(!recipes.get("postgres").unwrap().origin.is_user());

        write(
            dir.path(),
            "postgres.toml",
            "kind = \"service\"\nname = \"postgres\"\n\n[service]\ncmd = \"mine -p {port}\"\n",
        );
        let recipes = Recipes::load(dir.path());
        let loaded = recipes.get("postgres").unwrap();
        assert_eq!(
            loaded.origin,
            Origin::User(dir.path().join("postgres.toml"))
        );
        assert!(loaded.replaces_built_in);
        assert_eq!(loaded.recipe.service().unwrap().cmd, "mine -p {port}");
        // Replaced outright, not merged: the built-in's initdb line is
        // gone, because a recipe is one file and not a patch on another.
        assert_eq!(loaded.recipe.service().unwrap().init, None);
    }

    #[test]
    fn a_user_recipe_of_its_own_name_is_added_beside_the_built_ins() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tiny.toml", MINIMAL);
        let recipes = Recipes::load(dir.path());
        assert_eq!(recipes.names(), vec!["postgres", "tiny"]);
        assert!(!recipes.get("tiny").unwrap().replaces_built_in);
    }

    #[test]
    fn a_broken_user_recipe_shadows_the_built_in_rather_than_falling_back_to_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "postgres.toml", "kind = \"service\"\nname =");
        let recipes = Recipes::load(dir.path());
        let e = format!("{:#}", recipes.get("postgres").unwrap_err());
        assert!(e.contains("does not load"), "{e}");
        assert!(e.contains("postgres.toml"), "{e}");
        assert!(e.contains("delete it to go back to the built-in"), "{e}");
        assert_eq!(recipes.broken().count(), 1);
        // And it is not silently answered by the built-in.
        assert!(!recipes.names().contains(&"postgres"));
    }

    #[test]
    fn a_recipe_whose_name_disagrees_with_its_file_name_is_refused_naming_both() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mine.toml", MINIMAL);
        let recipes = Recipes::load(dir.path());
        let e = format!("{:#}", recipes.get("mine").unwrap_err());
        assert!(e.contains("\"tiny\""), "{e}");
        assert!(e.contains("\"mine\""), "{e}");
    }

    #[test]
    fn an_unknown_recipe_names_the_ones_there_are() {
        let recipes = Recipes::built_in();
        let e = format!("{:#}", recipes.get("mysql").unwrap_err());
        assert!(e.contains("no recipe named \"mysql\""), "{e}");
        assert!(e.contains("postgres"), "{e}");
    }

    #[test]
    fn an_alias_finds_a_recipe_but_never_beats_a_file_of_that_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Recipes::built_in().get("pg").unwrap().recipe.name,
            "postgres"
        );
        write(
            dir.path(),
            "pg.toml",
            "kind = \"service\"\nname = \"pg\"\n\n[service]\ncmd = \"x\"\n",
        );
        let recipes = Recipes::load(dir.path());
        assert_eq!(
            recipes.get("pg").unwrap().recipe.service().unwrap().cmd,
            "x"
        );
    }

    #[test]
    fn a_service_recipe_with_no_command_is_refused() {
        let e = format!(
            "{:#}",
            parse("kind = \"service\"\nname = \"x\"\n\n[service]\ncmd = \"\"\n").unwrap_err()
        );
        assert!(e.contains("cmd"), "{e}");
        let e = format!(
            "{:#}",
            parse("kind = \"service\"\nname = \"x\"\n").unwrap_err()
        );
        assert!(e.contains("[service]"), "{e}");
    }

    #[test]
    fn a_recipe_may_not_carry_the_other_kinds_body() {
        let e = format!(
            "{:#}",
            parse(
                "kind = \"service\"\nname = \"x\"\n\n[service]\ncmd = \"c\"\n\n[language]\n\
                 managers = [\"mise\"]\n"
            )
            .unwrap_err()
        );
        assert!(e.contains("[language]"), "{e}");
    }

    #[test]
    fn an_unknown_key_is_a_typo_and_is_refused() {
        let e = format!(
            "{:#}",
            parse("kind = \"service\"\nname = \"x\"\n\n[service]\ncmd = \"c\"\nreadyy = \"r\"\n")
                .unwrap_err()
        );
        assert!(e.contains("readyy"), "{e}");
    }

    #[test]
    fn a_recipe_name_that_is_a_path_is_refused() {
        let e = format!(
            "{:#}",
            parse("kind = \"service\"\nname = \"../x\"\n\n[service]\ncmd = \"c\"\n").unwrap_err()
        );
        assert!(e.contains("not a name"), "{e}");
    }

    // ---- the runtime table, in this format ------------------------------
    //
    // The claim Phase 6 has to make good on is "one loader, two kinds of
    // recipe": the per-language `const` table in `runtime.rs` could be read
    // off disk later without a second format being invented for it. Nothing
    // migrates here; these two tests are the proof that it could.

    /// What `runtime::LANGUAGES`' entry for `name` looks like in this
    /// format. Written by hand, deliberately: a generated fixture would
    /// prove only that the generator agrees with itself.
    fn language_toml(name: &str) -> &'static str {
        match name {
            "node" => {
                r#"
kind = "language"
name = "node"
aliases = ["nodejs"]
binaries = ["node"]
version_flag = "-v"

[language]
engines_key = "node"
managers = ["volta", "mise", "asdf", "nvm", "fnm"]
files = [{ file = ".nvmrc" }, { file = ".node-version" }]
"#
            }
            "rust" => {
                r#"
kind = "language"
name = "rust"
binaries = ["rustc"]
version_flag = "-V"

[language]
managers = ["rustup", "mise", "asdf"]
files = [{ file = "rust-toolchain.toml", toml_key = ["toolchain", "channel"] }]
"#
            }
            other => panic!("no fixture for {other}"),
        }
    }

    /// The same entry, read out of the `const` table.
    fn from_table(name: &str) -> Recipe {
        let entry = crate::runtime::LANGUAGES
            .iter()
            .find(|l| l.name == name)
            .expect("a language in the table");
        Recipe {
            kind: Kind::Language,
            name: entry.name.to_string(),
            aliases: entry.aliases.iter().map(|a| a.to_string()).collect(),
            summary: None,
            binaries: entry.binaries.iter().map(|b| b.to_string()).collect(),
            version_flag: Some(entry.version_flag.to_string()),
            install: None,
            notes: None,
            body: Body::Language(LanguageRecipe {
                files: entry
                    .files
                    .iter()
                    .map(|source| RecipeSource {
                        file: source.file.to_string(),
                        toml_key: match source.kind {
                            crate::runtime::SourceKind::Plain => Vec::new(),
                            crate::runtime::SourceKind::TomlKey(table, key) => {
                                vec![table.to_string(), key.to_string()]
                            }
                        },
                    })
                    .collect(),
                engines_key: entry.engines_key.map(str::to_string),
                managers: entry.managers.iter().map(|m| m.name.to_string()).collect(),
            }),
        }
    }

    #[test]
    fn a_language_from_the_runtime_table_round_trips_through_this_format() {
        // node: aliases, an engines key, five managers, two plain files.
        assert_eq!(parse(language_toml("node")).unwrap(), from_table("node"));
        // rust: the other source shape, a key inside a TOML file, which is
        // the one thing a flat `files = [...]` list could not carry.
        assert_eq!(parse(language_toml("rust")).unwrap(), from_table("rust"));
    }

    #[test]
    fn a_language_recipe_loads_beside_a_service_recipe_from_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "node.toml", language_toml("node"));
        write(dir.path(), "tiny.toml", MINIMAL);
        let recipes = Recipes::load(dir.path());
        assert_eq!(recipes.names(), vec!["node", "postgres", "tiny"]);
        // One loader, two kinds: the service list is not polluted by the
        // language, and the language keeps its own body.
        assert_eq!(recipes.service_names(), vec!["postgres", "tiny"]);
        let node = recipes.get("node").unwrap();
        assert_eq!(node.recipe.kind, Kind::Language);
        assert_eq!(
            node.recipe.language().unwrap().managers,
            vec!["volta", "mise", "asdf", "nvm", "fnm"]
        );
        assert!(node.recipe.service().is_none());
    }
}
