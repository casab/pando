//! Fixture repositories for the integration tests.
//!
//! Every mutating test runs against one of these, never against a real
//! repository. They are built under the test's own temp directory and torn
//! down with it.

#![allow(dead_code)]

pub mod docker;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pando::config::{
    Config, HookConfig, HookPoint, PortsSpec, ProcessConfig, ReadySpec, ServiceConfig,
};
use pando::paths::PandoPaths;
use pando::project::ProjectRef;

/// A fixture identity, so committing works on any machine and never depends
/// on (or picks up) the operator's git config. Fixture repositories only.
const FIXTURE_IDENTITY: [&str; 10] = [
    "-c",
    "user.name=t",
    "-c",
    "user.email=t@t",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "init.defaultBranch=main",
];

pub fn git(cwd: &Path, args: &[&str]) {
    let out = git_raw(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

pub fn git_raw(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(FIXTURE_IDENTITY)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"))
}

/// `git status --porcelain` output for a checkout. Empty means clean.
pub fn status_porcelain(cwd: &Path) -> String {
    let out = git_raw(cwd, &["status", "--porcelain"]);
    assert!(
        out.status.success(),
        "git status failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Which fixture project to build. Shapes match the fixture catalogue; only
/// the files matter in this phase, since nothing is started yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The smallest repo that satisfies the common fixture contract: a
    /// commit, a gitignore, and an untracked ignored `.env`.
    Plain,
    /// A Next-shaped app with pnpm, compose services, and prisma migrations.
    NextPnpmCompose,
    /// A Django app with uv, a positional port, and split database settings.
    DjangoUvPostgres,
    /// A Go service: no lockfile install, no compose, port by convention.
    GoService,
    /// A library with no dev server at all. Level zero.
    RustLib,
    /// A workspace with a web and an api process, one needing the other's
    /// port.
    MonoWebApi,
    /// The Next fixture with deliberate ambiguity in scripts, ports, and
    /// service names.
    NextMessy,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "plain" => Kind::Plain,
            "next-pnpm-compose" => Kind::NextPnpmCompose,
            "django-uv-postgres" => Kind::DjangoUvPostgres,
            "go-service" => Kind::GoService,
            "rust-lib" => Kind::RustLib,
            "mono-web-api" => Kind::MonoWebApi,
            "next-messy" => Kind::NextMessy,
            _ => return None,
        })
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            Kind::Plain => "plain",
            Kind::NextPnpmCompose => "next-pnpm-compose",
            Kind::DjangoUvPostgres => "django-uv-postgres",
            Kind::GoService => "go-service",
            Kind::RustLib => "rust-lib",
            Kind::MonoWebApi => "mono-web-api",
            Kind::NextMessy => "next-messy",
        }
    }

    /// The config detection should arrive at for this fixture, for the
    /// slots this phase fills: install, runtime version files, the dev
    /// command, its port role, and provision.
    ///
    /// Written by hand from `fixtures.md` before detection existed, and
    /// compared structurally — never as TOML text, because key order and
    /// comments would make that fragile. Services, hooks and probes are
    /// later phases and are left empty here.
    pub fn expected_config(self) -> Config {
        let mut config = Config::default();
        match self {
            Kind::Plain => {
                config.project.provision = strings(&[".env", ".env.local"]);
            }
            Kind::NextPnpmCompose | Kind::NextMessy => {
                config.project.install = Some("pnpm install --frozen-lockfile".to_string());
                config.project.provision = strings(&[".env", ".env.local"]);
                config.runtime.version_files = strings(&[".nvmrc"]);
                config.processes.insert(
                    "dev".to_string(),
                    ProcessConfig {
                        cmd: "pnpm dev".to_string(),
                        ports: Some(port_env("PORT")),
                        ..Default::default()
                    },
                );
                if self == Kind::NextPnpmCompose {
                    // `mailpit` is offered unticked: it is a mail catcher,
                    // and nothing in the env example names it.
                    config.services.push(compose_service(
                        &["postgres", "redis"],
                        &[("DATABASE_URL", "postgres"), ("REDIS_URL", "redis")],
                    ));
                    config.hooks.push(migrate_hook(
                        &["prisma/migrations/**"],
                        "pnpm prisma migrate deploy",
                    ));
                } else {
                    // `db` and `mail` resolve by rule; `cache` and `queue`
                    // have nothing pointing at them, so they are the
                    // question, and declining them is the answer taken here.
                    config.services.push(compose_service(
                        &["db", "mail"],
                        &[("DB_PORT", "db"), ("SMTP_PORT", "mail")],
                    ));
                }
            }
            Kind::DjangoUvPostgres => {
                config.project.install = Some("uv sync --frozen".to_string());
                config.project.provision = strings(&[".env"]);
                config.runtime.version_files = strings(&[".python-version"]);
                config.processes.insert(
                    "dev".to_string(),
                    ProcessConfig {
                        // Positional: Django takes the port on the command
                        // line, so there is no environment variable to pick.
                        cmd: "uv run python manage.py runserver 127.0.0.1:{port:web}".to_string(),
                        ports: Some(PortsSpec::List(strings(&["web"]))),
                        ..Default::default()
                    },
                );
                // `DB_PORT` is a plain value, so it becomes the bare port;
                // `REDIS_URL` is a URL, so its port is rewritten in place.
                config.services.push(compose_service(
                    &["db", "redis"],
                    &[("DB_PORT", "db"), ("REDIS_URL", "redis")],
                ));
                config.hooks.push(migrate_hook(
                    &["*/migrations/*.py"],
                    "uv run python manage.py migrate",
                ));
            }
            Kind::GoService => {
                // No install step: `go run` resolves its own modules.
                config.processes.insert(
                    "dev".to_string(),
                    ProcessConfig {
                        cmd: "go run .".to_string(),
                        ports: Some(port_env("PORT")),
                        ..Default::default()
                    },
                );
            }
            // Level zero: a library has nothing to serve, so detection
            // proposes nothing and asks nothing.
            Kind::RustLib => {}
            Kind::MonoWebApi => {
                config.project.install = Some("pnpm install --frozen-lockfile".to_string());
                config.project.provision = strings(&[".env"]);
                // Two processes, each in its own directory, with the web
                // one told the api's port. The root `dev` script is a
                // `pnpm -r` wrapper, which works but gives one log and one
                // readiness rule for two servers.
                config.processes.insert(
                    "web".to_string(),
                    ProcessConfig {
                        // Vite takes its port on the command line, so the
                        // flag is appended to the app's own dev script.
                        cmd: "pnpm dev -- --port {port:web}".to_string(),
                        cwd: Some("apps/web".to_string()),
                        ports: Some(PortsSpec::List(strings(&["web"]))),
                        // The reason `{port:<role>}` exists: the web app
                        // has to be told the port the api was given in
                        // this worktree.
                        env: std::collections::BTreeMap::from([(
                            "VITE_API_URL".to_string(),
                            "http://localhost:{port:api}".to_string(),
                        )]),
                        ready: Some(ReadySpec {
                            role: Some("web".to_string()),
                            timeout_s: None,
                        }),
                    },
                );
                config.processes.insert(
                    "api".to_string(),
                    ProcessConfig {
                        cmd: "pnpm dev".to_string(),
                        cwd: Some("apps/api".to_string()),
                        ports: Some(PortsSpec::List(strings(&["api"]))),
                        env: std::collections::BTreeMap::from([(
                            "PORT".to_string(),
                            "{port:api}".to_string(),
                        )]),
                        ready: Some(ReadySpec {
                            role: Some("api".to_string()),
                            timeout_s: None,
                        }),
                    },
                );
                config.services.push(compose_service(
                    &["postgres"],
                    &[("DATABASE_URL", "postgres")],
                ));
            }
        }
        config
    }

    pub const ALL: [Kind; 7] = [
        Kind::Plain,
        Kind::NextPnpmCompose,
        Kind::DjangoUvPostgres,
        Kind::GoService,
        Kind::RustLib,
        Kind::MonoWebApi,
        Kind::NextMessy,
    ];
}

pub struct Fixture {
    /// The main checkout.
    pub root: PathBuf,
    /// The bare repository acting as `origin`, when the fixture has one.
    pub remote: Option<PathBuf>,
}

/// The default fixture: a repo with one commit, a gitignore, and an
/// untracked ignored `.env`.
pub fn fixture_repo(parent: &Path) -> PathBuf {
    build(Kind::Plain, parent).root
}

/// Builds a fixture under `parent` and returns its paths. Every kind gets a
/// commit, a `.gitignore`, and any ignored files its config would provision.
pub fn build(kind: Kind, parent: &Path) -> Fixture {
    let root = parent.join(kind.dir_name());
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    for (rel, contents) in files_for(kind) {
        write_file(&root, rel, contents);
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "initial commit"]);
    for (rel, contents) in ignored_files_for(kind) {
        write_file(&root, rel, contents);
    }
    Fixture { root, remote: None }
}

/// The same fixture plus a bare `origin` it was cloned from, so
/// remote-tracking refs exist.
pub fn build_with_origin(kind: Kind, parent: &Path) -> Fixture {
    let bare = parent.join(format!("{}-origin.git", kind.dir_name()));
    git(
        parent,
        &[
            "init",
            "--bare",
            "--quiet",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );
    let seed = build(kind, &parent.join("seed"));
    git(
        &seed.root,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&seed.root, &["push", "--quiet", "origin", "main"]);

    let root = parent.join(kind.dir_name());
    git(
        parent,
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    for (rel, contents) in ignored_files_for(kind) {
        write_file(&root, rel, contents);
    }
    Fixture {
        root,
        remote: Some(bare),
    }
}

fn write_file(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
}

/// Paths pointing at an injected pando home, so no test can reach the real
/// `~/.pando`.
pub fn paths_for(home: &Path, root: &Path) -> PandoPaths {
    PandoPaths::new(home, ProjectRef::from_root(root).unwrap())
}

/// The lockfile pnpm writes for a manifest with no dependencies, byte for
/// byte (pnpm 12.5.1). A stub is not good enough: pnpm rewrites one it
/// considers malformed even under `--frozen-lockfile`, and then the
/// fixture's own worktree is dirty for reasons that have nothing to do
/// with what is being tested.
const PNPM_LOCK: &str = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  \
                         excludeLinksFromLockfile: false\n\nimporters:\n\n  .: {}\n";

/// The same for a workspace: pnpm lists every importer it resolved.
const PNPM_LOCK_WORKSPACE: &str = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  \
     excludeLinksFromLockfile: false\n\nimporters:\n\n  .: {}\n\n  apps/api: {}\n\n  \
     apps/web: {}\n";

/// Tracked files, written before the initial commit.
fn files_for(kind: Kind) -> Vec<(&'static str, &'static str)> {
    match kind {
        Kind::Plain => vec![
            (".gitignore", ".env\n.env.local\nnode_modules/\n"),
            ("README.md", "# plain fixture\n"),
        ],
        Kind::NextPnpmCompose => vec![
            (
                "package.json",
                r#"{
  "name": "next-pnpm-compose",
  "scripts": {
    "dev": "next dev",
    "build": "next build",
    "start": "next start",
    "lint": "next lint",
    "prisma": "echo prisma-stub"
  }
}
"#,
            ),
            ("pnpm-lock.yaml", PNPM_LOCK),
            (".nvmrc", "22\n"),
            (
                ".env.example",
                "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\nREDIS_URL=redis://localhost:6379\n",
            ),
            (
                "docker-compose.yml",
                // The password is what makes this file one docker can
                // really bring up: the official postgres image refuses to
                // initialise without one, so a fixture that leaves it out
                // cannot be started by hand or by the demo.
                r#"services:
  postgres:
    image: postgres:16
    environment:
      POSTGRES_PASSWORD: acme
    ports: ["5432:5432"]
  redis:
    image: redis:7
    ports: ["6379:6379"]
  mailpit:
    image: axllent/mailpit
    ports: ["1025:1025"]
"#,
            ),
            (
                "prisma/schema.prisma",
                "datasource db {\n  provider = \"postgresql\"\n  url = env(\"DATABASE_URL\")\n}\n",
            ),
            (
                "prisma/migrations/20260101000000_init/migration.sql",
                "CREATE TABLE item (id SERIAL PRIMARY KEY);\n",
            ),
            (".gitignore", ".env\n.env.local\nnode_modules/\n.next/\n"),
        ],
        Kind::DjangoUvPostgres => vec![
            (
                "pyproject.toml",
                "[project]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"django\"]\n",
            ),
            ("uv.lock", "version = 1\n"),
            (".python-version", "3.12\n"),
            (
                "manage.py",
                "#!/usr/bin/env python\nimport sys\n\nif __name__ == \"__main__\":\n    sys.exit(0)\n",
            ),
            ("app/__init__.py", ""),
            ("app/migrations/__init__.py", ""),
            ("app/migrations/0001_initial.py", "# initial migration\n"),
            (
                ".env.example",
                "DB_HOST=localhost\nDB_PORT=5432\nDB_NAME=app\nREDIS_URL=redis://localhost:6379/0\n",
            ),
            (
                "docker-compose.yml",
                r#"services:
  db:
    image: postgres:16
    ports: ["5432:5432"]
  redis:
    image: redis:7
    ports: ["6379:6379"]
"#,
            ),
            (".gitignore", ".env\n.venv/\n__pycache__/\n"),
        ],
        Kind::GoService => vec![
            ("go.mod", "module example.test/service\n\ngo 1.23\n"),
            ("go.sum", ""),
            (
                "main.go",
                r#"package main

import (
    "net/http"
    "os"
)

func main() {
    port := os.Getenv("PORT")
    http.ListenAndServe(":"+port, nil)
}
"#,
            ),
            ("Makefile", "run:\n\tgo run .\n"),
            (".gitignore", "bin/\n"),
        ],
        Kind::RustLib => vec![
            (
                "Cargo.toml",
                "[package]\nname = \"fixture-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
            ),
            ("Cargo.lock", "version = 3\n"),
            ("src/lib.rs", "pub fn answer() -> u32 {\n    42\n}\n"),
            (".gitignore", "target/\n"),
        ],
        Kind::MonoWebApi => vec![
            (
                "package.json",
                r#"{
  "name": "mono-web-api",
  "private": true,
  "workspaces": ["apps/*"],
  "scripts": { "dev": "pnpm -r --parallel dev" }
}
"#,
            ),
            ("pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n"),
            ("pnpm-lock.yaml", PNPM_LOCK_WORKSPACE),
            (
                "apps/web/package.json",
                "{\n  \"name\": \"web\",\n  \"scripts\": { \"dev\": \"vite\" }\n}\n",
            ),
            (
                "apps/web/vite.config.ts",
                "export default { server: { port: Number(process.env.WEB_PORT) } }\n",
            ),
            (
                "apps/api/package.json",
                "{\n  \"name\": \"api\",\n  \"scripts\": { \"dev\": \"node --watch src/index.js\" }\n}\n",
            ),
            (
                "apps/api/src/index.js",
                "const port = process.env.PORT;\nconsole.log('api on', port);\n",
            ),
            (
                ".env.example",
                "WEB_PORT=5173\nAPI_PORT=4000\nVITE_API_URL=http://localhost:4000\nDATABASE_URL=postgres://app:app@localhost:5432/app\n",
            ),
            (
                "docker-compose.yml",
                "services:\n  postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
            ),
            (".gitignore", ".env\nnode_modules/\ndist/\n"),
        ],
        Kind::NextMessy => vec![
            (
                "package.json",
                r#"{
  "name": "next-messy",
  "scripts": {
    "dev": "concurrently \"npm:dev:*\"",
    "dev:web": "next dev",
    "dev:worker": "node worker.js",
    "dev:all": "./scripts/dev.sh",
    "start": "next start",
    "serve": "serve out",
    "preview": "next start -p 4000"
  }
}
"#,
            ),
            ("pnpm-lock.yaml", PNPM_LOCK),
            (".nvmrc", "22\n"),
            ("worker.js", "setInterval(() => {}, 1000);\n"),
            ("scripts/dev.sh", "#!/bin/sh\nexec npm run dev:web\n"),
            (
                ".env.example",
                "PORT=3000\nAPI_PORT=3001\nVITE_PORT=5173\nDB_PORT=5432\nSMTP_PORT=1025\n",
            ),
            (
                "docker-compose.yml",
                r#"services:
  db:
    image: postgres:16
    ports: ["5432:5432"]
  cache:
    image: redis:7
    ports: ["6379:6379"]
  queue:
    image: rabbitmq:3
    ports: ["5672:5672"]
  mail:
    image: axllent/mailpit
    ports: ["1025:1025"]
"#,
            ),
            (".gitignore", ".env\n.env.local\nnode_modules/\n.next/\n"),
        ],
    }
}

/// Untracked but gitignored files, written after the initial commit, so
/// provisioning has something to link and `git status` stays clean.
fn ignored_files_for(kind: Kind) -> Vec<(&'static str, &'static str)> {
    match kind {
        Kind::Plain => vec![(".env", "SECRET=1\n"), (".env.local", "LOCAL=1\n")],
        Kind::NextPnpmCompose | Kind::NextMessy => vec![
            (
                ".env",
                "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\nREDIS_URL=redis://localhost:6379\n",
            ),
            (".env.local", "NEXT_PUBLIC_FLAG=1\n"),
        ],
        Kind::DjangoUvPostgres => vec![(
            ".env",
            "DB_HOST=localhost\nDB_PORT=5432\nDB_NAME=app\nREDIS_URL=redis://localhost:6379/0\n",
        )],
        Kind::MonoWebApi => vec![(
            ".env",
            "WEB_PORT=5173\nAPI_PORT=4000\nVITE_API_URL=http://localhost:4000\n",
        )],
        Kind::GoService | Kind::RustLib => vec![],
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// The `[[services]]` entry detection writes for a fixture's compose file.
fn compose_service(include: &[&str], env: &[(&str, &str)]) -> ServiceConfig {
    ServiceConfig::Compose {
        file: "docker-compose.yml".to_string(),
        include: strings(include),
        env: env
            .iter()
            .map(|(key, service)| (key.to_string(), service.to_string()))
            .collect(),
        ready_timeout_s: None,
    }
}

/// The `[[hooks]]` entry detection writes for a project's schema step.
fn migrate_hook(fingerprint: &[&str], cmd: &str) -> HookConfig {
    HookConfig {
        name: "migrate".to_string(),
        after: HookPoint::Services,
        fingerprint: strings(fingerprint),
        cmd: cmd.to_string(),
        cwd: None,
        fallback: None,
    }
}

/// `ports = { <VAR> = "web" }`: the sugar for one role reached through an
/// environment variable.
fn port_env(var: &str) -> PortsSpec {
    PortsSpec::Map(std::collections::BTreeMap::from([(
        var.to_string(),
        "web".to_string(),
    )]))
}

/// Whether `python3` is on PATH. Tests that need a process which really
/// binds a port skip with a message rather than failing on a machine
/// without it.
pub fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A dev "server" for tests and demos: it binds its port, says so, and
/// stays up. Real enough to drive readiness, observed ports, and the log
/// tail, and available on every machine that has python3.
///
/// Deliberately free of `{` and `}` except the placeholder itself: braces
/// are pando's template syntax, and a command full of them would need
/// escaping everywhere it appears.
pub fn listener_on_port_env() -> String {
    "python3 -u -c \"import os,socket,time;p=int(os.environ['PORT']);s=socket.socket();\
     s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('127.0.0.1',p));\
     s.listen(5);print('listening on',p);time.sleep(3600)\""
        .to_string()
}

/// The same listener with its port coming from a template rather than the
/// environment — the positional shape Django and Rails use.
pub fn listener_on_port_template() -> String {
    "python3 -u -c \"import socket,time;s=socket.socket();\
     s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('127.0.0.1',{port:web}));\
     s.listen(5);print('listening on {port:web}');time.sleep(3600)\""
        .to_string()
}

/// A listener that prints an environment variable before it binds, so what
/// one process was told about another is visible in its log.
///
/// Brace-free except the placeholder: `{` is pando's template syntax.
pub fn listener_printing(var: &str) -> String {
    format!(
        "python3 -u -c \"import os,socket,time;\
         print('{var}=' + os.environ['{var}']);s=socket.socket();\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
         s.bind(('127.0.0.1',{{port:web}}));s.listen(5);\
         print('listening on {{port:web}}');time.sleep(3600)\""
    )
}

/// The pando-home config the `--listener` fixture writes for the workspace
/// fixture: two processes, each in its own directory, with the web one told
/// the api's port through a template — and printing it, so the
/// cross-process reference is visible in its own log.
pub fn workspace_listener_config() -> String {
    let mut out = String::new();
    out.push_str("# written by scripts/fixture-repo.sh --listener\n");
    out.push_str("[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n");
    out.push_str("[processes.web]\ncwd = \"apps/web\"\ncmd = '''");
    out.push_str(&listener_printing("VITE_API_URL"));
    out.push_str("'''\nports = [\"web\"]\n");
    out.push_str("env = { VITE_API_URL = \"http://localhost:{port:api}\" }\n");
    out.push_str("ready = { role = \"web\" }\n\n");
    out.push_str("[processes.api]\ncwd = \"apps/api\"\ncmd = '''");
    out.push_str(&listener_on_port_env());
    out.push_str("'''\nports = [\"api\"]\nenv = { PORT = \"{port:api}\" }\n");
    out.push_str("ready = { role = \"api\" }\n");
    out
}

/// The listener config for a fixture: the workspace gets two processes,
/// everything else the single one.
pub fn listener_config_for(kind: Kind) -> String {
    match kind {
        Kind::MonoWebApi => workspace_listener_config(),
        _ => listener_config(),
    }
}

/// The pando-home config the `--listener` fixture writes: a dev process
/// that needs no framework installed and no real server.
///
/// The command is a TOML literal string, so the quotes inside it survive
/// exactly as written.
pub fn listener_config() -> String {
    let mut out = String::new();
    out.push_str("# written by scripts/fixture-repo.sh --listener\n");
    out.push_str("[project]\nprovision = [\".env\"]\n");
    // A no-op stand-in for a real install: the demo needs a hook that runs,
    // logs, and records a fingerprint, not a package manager.
    out.push_str("install = \"true\"\n\n[dev]\ncmd = '''");
    out.push_str(&listener_on_port_env());
    out.push_str("'''\nports = { PORT = \"web\" }\n");
    out
}

/// Writes the fixture's listener config into an injected pando home.
pub fn write_listener_config(kind: Kind, home: &Path, root: &Path) -> PathBuf {
    let project = ProjectRef::from_root(root).unwrap();
    let dir = home.join("projects").join(&project.id);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pando.toml");
    std::fs::write(&path, listener_config_for(kind)).unwrap();
    path
}
