//! Fixture repositories for the integration tests.
//!
//! Every mutating test runs against one of these, never against a real
//! repository. They are built under the test's own temp directory and torn
//! down with it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pando::config::{Config, PortsSpec, ProcessConfig};
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
                        ports: port_env("PORT"),
                        ..Default::default()
                    },
                );
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
                        ports: PortsSpec::List(strings(&["web"])),
                        ..Default::default()
                    },
                );
            }
            Kind::GoService => {
                // No install step: `go run` resolves its own modules.
                config.processes.insert(
                    "dev".to_string(),
                    ProcessConfig {
                        cmd: "go run .".to_string(),
                        ports: port_env("PORT"),
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
                // This phase proposes the root script as one process; the
                // two-process form is Phase 2b.
                config.processes.insert(
                    "dev".to_string(),
                    ProcessConfig {
                        cmd: "pnpm dev".to_string(),
                        ports: port_env("WEB_PORT"),
                        ..Default::default()
                    },
                );
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
    "lint": "next lint"
  }
}
"#,
            ),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
            (".nvmrc", "22\n"),
            (
                ".env.example",
                "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\nREDIS_URL=redis://localhost:6379\n",
            ),
            (
                "docker-compose.yml",
                r#"services:
  postgres:
    image: postgres:16
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
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
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
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
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

/// Writes `listener_config` into an injected pando home for `root`.
pub fn write_listener_config(home: &Path, root: &Path) -> PathBuf {
    let project = ProjectRef::from_root(root).unwrap();
    let dir = home.join("projects").join(&project.id);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pando.toml");
    std::fs::write(&path, listener_config()).unwrap();
    path
}
