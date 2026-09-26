//! Frameworks: how pando recognises one, starts it, and tells it a port.
//!
//! A framework is one row in [`RULES`]. The files that identify a framework
//! are also the marker files `signals` looks for, derived from the rows, so
//! a new framework is recognised the moment its row exists.

/// How a framework is told which port to listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortMechanism {
    /// An environment variable, named here.
    Env(&'static str),
    /// A placeholder already inside the command pando proposes.
    InCommand,
    /// Nothing pando knows about; the developer says.
    Ask,
}

/// One framework: how to recognise it, how to start it, and how it takes a
/// port. Data, not code paths — a wrong entry is one table row to fix.
#[derive(Debug, Clone, Copy)]
pub struct FrameworkRule {
    pub name: &'static str,
    /// Any one of these files identifies it.
    pub markers: &'static [&'static str],
    /// Any one of these substrings in a script body identifies it.
    pub script_markers: &'static [&'static str],
    pub port: PortMechanism,
    pub default_port: u16,
    /// The command to propose when the project has no script to run, or
    /// when its scripts only build assets. `{runner}` is replaced with the
    /// project's package or venv runner.
    pub command: Option<&'static str>,
    /// The flag that tells this framework its port, for an app whose own
    /// script pando runs rather than the command above: `pnpm dev --port
    /// 1234`. `{port}` is replaced with the role template. `None`
    /// for a framework that takes its port some other way — Django's
    /// positional `host:port` cannot be appended to somebody's script.
    pub port_flag: Option<&'static str>,
    /// What a project with one of the markers must also be before this
    /// rule claims it.
    pub guard: Guard,
    /// Whether the framework's own server is the app and a `package.json`
    /// beside it only builds the assets: Laravel's `vite`, a Rails app on
    /// vite_ruby. Its command then leads the scripts, which run the asset
    /// server, not the app.
    pub scripts_build_assets: bool,
}

/// What a marker match must also pass, for a marker file that more than
/// one kind of project has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// The marker file is enough.
    Marker,
    /// A crate that builds something runnable: a Cargo.toml with no binary
    /// is a library, and there is nothing to serve.
    BinaryCrate,
    /// One of these files contains this text: every Mix project has a
    /// `mix.exs`, and only one that depends on `:phoenix` is Phoenix.
    Mentions(&'static [&'static str], &'static str),
}

/// The rules pando ships with. Order matters: the first match wins, so the
/// specific frameworks come before the conventions they are built on, and
/// an app server before the asset pipeline it builds with.
pub const RULES: [FrameworkRule; 12] = [
    FrameworkRule {
        name: "Next.js",
        markers: &[
            "next.config.js",
            "next.config.mjs",
            "next.config.cjs",
            "next.config.ts",
        ],
        script_markers: &["next dev"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: Some("npx next dev"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    FrameworkRule {
        name: "Nuxt",
        markers: &["nuxt.config.ts", "nuxt.config.js", "nuxt.config.mjs"],
        script_markers: &["nuxt dev"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: Some("npx nuxt dev"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    // Astro sits on Vite but has its own CLI, its own default port and its
    // own command, so it comes before the Vite row that would claim it.
    FrameworkRule {
        name: "Astro",
        markers: &["astro.config.mjs", "astro.config.ts", "astro.config.js"],
        script_markers: &["astro dev"],
        port: PortMechanism::InCommand,
        default_port: 4321,
        command: Some("npx astro dev --port {port:web}"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    FrameworkRule {
        name: "Angular",
        markers: &["angular.json"],
        script_markers: &["ng serve"],
        port: PortMechanism::InCommand,
        default_port: 4200,
        command: Some("npx ng serve --port {port:web}"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    // The app servers come before the Vite row: a Laravel app has a
    // vite.config.js and a `dev: vite` script, and so do Django and Rails
    // apps that build their assets with Vite.
    FrameworkRule {
        name: "Django",
        markers: &["manage.py"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("{runner}python manage.py runserver 127.0.0.1:{port:web}"),
        port_flag: None,
        guard: Guard::Marker,
        scripts_build_assets: true,
    },
    FrameworkRule {
        name: "Rails",
        // Rails' own files. `config.ru` is every Rack app's and `bin/dev`
        // is a helper script in any language.
        markers: &["bin/rails", "config/application.rb"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 3000,
        command: Some("bin/rails server -p {port:web}"),
        port_flag: Some("-p {port}"),
        guard: Guard::Marker,
        scripts_build_assets: true,
    },
    FrameworkRule {
        name: "Phoenix",
        markers: &["mix.exs"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 4000,
        command: Some("mix phx.server"),
        port_flag: None,
        // `{:phoenix, …}` in the deps, or `:phoenix,` in the lockfile an
        // umbrella keeps at its root beside a `mix.exs` that has none.
        guard: Guard::Mentions(&["mix.exs", "mix.lock"], ":phoenix,"),
        scripts_build_assets: true,
    },
    FrameworkRule {
        name: "Laravel",
        markers: &["artisan"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("php artisan serve --port {port:web}"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: true,
    },
    FrameworkRule {
        name: "Vite",
        markers: &[
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.mts",
            "vite.config.cjs",
        ],
        // React Router and Remix's Vite mode are Vite underneath and take
        // the same `--port` flag.
        script_markers: &[
            "vite",
            "svelte-kit dev",
            "react-router dev",
            "remix vite:dev",
        ],
        // Vite reads PORT only through its config, so the flag is the
        // reliable route — and it is one pando can put in the command.
        port: PortMechanism::InCommand,
        default_port: 5173,
        command: Some("npx vite --port {port:web}"),
        port_flag: Some("--port {port}"),
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    FrameworkRule {
        name: "Go",
        markers: &["go.mod"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 8080,
        command: Some("go run ."),
        port_flag: None,
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
    FrameworkRule {
        name: "Rust",
        markers: &["Cargo.toml"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 8080,
        // Only for a crate that builds a binary; a library has nothing to
        // run, which `binary_crate` decides.
        command: Some("cargo run"),
        port_flag: None,
        guard: Guard::BinaryCrate,
        scripts_build_assets: false,
    },
    FrameworkRule {
        name: "Node",
        markers: &[],
        script_markers: &["node ", "nodemon", "tsx ", "ts-node", "fastify", "express"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: None,
        port_flag: None,
        guard: Guard::Marker,
        scripts_build_assets: false,
    },
];

/// Files that say something about the project's toolchain without
/// identifying a framework. Listed with the framework markers in
/// `signals`.
const TOOLCHAIN_MARKERS: [&str; 1] = ["pyproject.toml"];

/// Every marker file `signals` looks for: each rule's markers, in rule
/// order, then the toolchain markers.
pub fn marker_files() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let rules = RULES.iter().flat_map(|rule| rule.markers.iter().copied());
    for marker in rules.chain(TOOLCHAIN_MARKERS) {
        if !out.contains(&marker) {
            out.push(marker);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_marker_is_looked_for() {
        let markers = marker_files();
        for rule in RULES {
            for marker in rule.markers {
                assert!(markers.contains(marker), "{} marker {marker}", rule.name);
            }
        }
    }

    #[test]
    fn rule_names_are_unique() {
        for (i, rule) in RULES.iter().enumerate() {
            assert!(
                RULES[i + 1..].iter().all(|other| other.name != rule.name),
                "{} is listed twice",
                rule.name
            );
        }
    }
}
