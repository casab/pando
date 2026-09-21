//! Every location pando writes to, as a pure function of the home directory
//! and the project id.
//!
//! Invariant 1 lives here: nothing in this module ever points inside the
//! repository. Tests inject a temp `home`, so no test can touch the real
//! `~/.pando`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::project::ProjectRef;

/// Directory mode for pando's home. Later phases copy env files in, so the
/// tree is locked down from the first commit rather than tightened later.
const HOME_MODE: u32 = 0o700;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PandoPaths {
    pub home: PathBuf,
    pub project: ProjectRef,
}

impl PandoPaths {
    pub fn new(home: impl Into<PathBuf>, project: ProjectRef) -> Self {
        Self {
            home: home.into(),
            project,
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project.id
    }

    pub fn root(&self) -> &Path {
        &self.project.root
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.home.join("projects")
    }

    pub fn project_dir(&self) -> PathBuf {
        self.projects_dir().join(&self.project.id)
    }

    /// The only path pando ever writes config to. A `pando.toml` committed
    /// inside the repository is read-only, and is not this file.
    pub fn config_file(&self) -> PathBuf {
        self.project_dir().join("pando.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.project_dir().join("state.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.project_dir().join("state.lock")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.project_dir().join("cache")
    }

    pub fn enrich_cache_file(&self) -> PathBuf {
        self.cache_dir().join("enrich.json")
    }

    pub fn pr_cache_file(&self) -> PathBuf {
        self.cache_dir().join("prs.json")
    }

    /// Runtime probes that came back satisfied, so a start costs one extra
    /// spawn when the requirement or the prelude changes and none when
    /// neither has.
    pub fn runtime_cache_file(&self) -> PathBuf {
        self.cache_dir().join("runtime.json")
    }

    /// Where `new` puts worktrees unless config overrides it. Callers must go
    /// through the config-aware helper in `actions` rather than reading this
    /// directly, so a configured `worktrees_dir` is honoured everywhere.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.project_dir().join("worktrees")
    }

    pub fn worktree_path(&self, name: &str) -> PathBuf {
        self.worktrees_dir().join(name)
    }

    pub fn logs_dir(&self, name: &str) -> PathBuf {
        self.project_dir().join("logs").join(name)
    }

    /// `source` names one log of one worktree: `dev`, a process, a hook,
    /// `tunnel`, `proxy`. The TUI builds its log tabs from the files that
    /// exist, so nothing here enumerates the set.
    ///
    /// It is a path component, so every name that reaches this has to have
    /// been through [`validate_log_source`] first — at config load time for
    /// a process or a hook, at the argument for `logs --source`.
    pub fn log_file(&self, name: &str, source: &str) -> PathBuf {
        self.logs_dir(name).join(format!("{source}.log"))
    }

    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.project_dir().join("data").join(name)
    }

    pub fn compose_dir(&self) -> PathBuf {
        self.project_dir().join("compose")
    }

    pub fn compose_override_file(&self, name: &str) -> PathBuf {
        self.compose_dir().join(format!("{name}.override.yml"))
    }

    /// Shared by every pando-spawned tunnel, and passed with `--config` so a
    /// user's own ingress rules never apply to a quick tunnel.
    pub fn tunnel_config_file(&self) -> PathBuf {
        self.home.join("tunnel-config.yml")
    }

    /// The machine-wide config layer: one file for every project on this
    /// laptop, beneath the project layer and above a committed one.
    ///
    /// It sits directly under the home rather than under a project because
    /// what it holds is a property of the machine — which version manager
    /// this shell needs initialised — and not of any one repository.
    pub fn user_config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// Creates the home directory 0700 and the project directory under it.
    /// Idempotent; called before the first write of any run.
    pub fn ensure_home(&self) -> Result<()> {
        // The last gate: whatever computed this home, nothing is created
        // until it is proven to sit outside the repository.
        ensure_outside_repository("pando home", &self.home, self.root(), &[])?;
        ensure_dir_private(&self.home)?;
        std::fs::create_dir_all(self.project_dir())
            .with_context(|| format!("create {}", self.project_dir().display()))?;
        Ok(())
    }
}

/// Log names pando keeps for its own use: the install hook's log, and the
/// `tunnel` and `proxy` logs `share` writes.
///
/// One list, in one place, because a process, a hook and `share` all write
/// into the same `logs/<worktree>/` directory and whichever of them starts
/// last truncates the other's log. Phase 3's `[[hooks]]` and Phase 4's
/// `share` extend this rather than keeping a second list somewhere else.
pub const RESERVED_LOG_SOURCES: [&str; 3] = ["install", "tunnel", "proxy"];

/// Refuses a name that would not be exactly one component of
/// `logs/<worktree>/<name>.log`.
///
/// A TOML key may be any quoted string, so `[processes."../../x"]` parses
/// happily; used as a path component it escapes the log directory, and
/// starting it creates and truncates whatever `.log` file it lands on.
/// Aimed back at the checkout that is Invariant 1 broken — pando writing
/// into the repository — and aimed anywhere else it is still a file pando
/// had no business touching.
///
/// `kind` names what is being checked, so the message reads as a sentence:
/// `process name`, `hook name`, `log source`.
pub fn validate_log_source(kind: &str, name: &str) -> Result<()> {
    const WHERE: &str = "a log lives at logs/<worktree>/<name>.log";
    if name.trim().is_empty() {
        anyhow::bail!("a {kind} must not be empty — {WHERE}");
    }
    if name == "." || name == ".." {
        anyhow::bail!("{kind} {name:?} is a directory, not a name — {WHERE}");
    }
    if name.contains('/') || name.contains('\\') {
        let mut message = format!("{kind} {name:?} must be a single name, not a path — {WHERE}");
        if let Some(suggestion) = suggest_log_source(name) {
            message.push_str(&format!("; try {suggestion:?}"));
        }
        anyhow::bail!("{message}");
    }
    Ok(())
}

/// The same check plus [`RESERVED_LOG_SOURCES`].
///
/// Process and hook names go through this one. `logs --source` does not:
/// `--source install` is how the install hook's log is read, and refusing
/// to read a log pando itself wrote would be absurd.
pub fn validate_owned_log_source(kind: &str, name: &str) -> Result<()> {
    validate_log_source(kind, name)?;
    if RESERVED_LOG_SOURCES.contains(&name) {
        anyhow::bail!(
            "{kind} {name:?} is reserved for pando's own logs ({}) — pando would truncate that \
             log on every start; pick another name",
            RESERVED_LOG_SOURCES.join(", ")
        );
    }
    Ok(())
}

/// The last component of a path-shaped name, when that is itself usable.
/// `apps/web` is a name a developer plausibly writes, and `web` is what
/// they meant.
fn suggest_log_source(name: &str) -> Option<&str> {
    let last = name
        .rsplit(['/', '\\'])
        .find(|part| !part.trim().is_empty())?;
    (last != "." && last != ".." && !RESERVED_LOG_SOURCES.contains(&last)).then_some(last)
}

/// Refuses a location pando would write to that lies inside the repository,
/// or inside any of the worktrees it was given. `label` names what the user
/// set, so the message points at the knob they can turn.
///
/// Invariant 1 has no exceptions for a path the user configured: a home or a
/// `worktrees_dir` in the working tree would fill it with state, caches, and
/// whole worktrees.
pub fn ensure_outside_repository(
    label: &str,
    path: &Path,
    root: &Path,
    worktrees: &[PathBuf],
) -> Result<()> {
    let resolved = resolve_for_compare(path);
    if resolved.starts_with(resolve_for_compare(root)) {
        anyhow::bail!(
            "{label} {} is inside the repository {} — pando never writes into your repository",
            path.display(),
            root.display()
        );
    }
    for worktree in worktrees {
        if resolved.starts_with(resolve_for_compare(worktree)) {
            anyhow::bail!(
                "{label} {} is inside the worktree {} — pando never writes into your repository",
                path.display(),
                worktree.display()
            );
        }
    }
    Ok(())
}

/// Canonicalises the deepest existing ancestor and re-appends the rest, so a
/// not-yet-created path still compares correctly against a canonical root —
/// on macOS `/var/...` and `/private/var/...` are the same directory.
pub fn resolve_for_compare(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&cursor) {
            let mut out = canonical;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        suffix.push(name);
        if !cursor.pop() {
            return path.to_path_buf();
        }
    }
}

fn ensure_dir_private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut perms = std::fs::metadata(dir)
        .with_context(|| format!("stat {}", dir.display()))?
        .permissions();
    if perms.mode() & 0o777 != HOME_MODE {
        perms.set_mode(HOME_MODE);
        std::fs::set_permissions(dir, perms)
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

/// `$PANDO_HOME` when set and non-empty, else `~/.pando`. The only place in
/// the crate that reads the environment for a path — tests inject a home
/// instead, because an env var set inside parallel tests is racy.
pub fn default_home() -> PathBuf {
    if let Some(v) = std::env::var_os("PANDO_HOME")
        && !v.is_empty()
    {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(".pando")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> PandoPaths {
        PandoPaths::new(
            "/tmp/pando-home",
            ProjectRef {
                id: "acme-shop-3f9a2c1d".into(),
                root: PathBuf::from("/tmp/code/acme-shop"),
                display_name: "acme-shop".into(),
            },
        )
    }

    #[test]
    fn computes_expected_subpaths() {
        let p = paths();
        let base = PathBuf::from("/tmp/pando-home/projects/acme-shop-3f9a2c1d");
        assert_eq!(p.project_dir(), base);
        assert_eq!(p.config_file(), base.join("pando.toml"));
        assert_eq!(p.state_file(), base.join("state.json"));
        assert_eq!(p.lock_file(), base.join("state.lock"));
        assert_eq!(p.cache_dir(), base.join("cache"));
        assert_eq!(p.enrich_cache_file(), base.join("cache/enrich.json"));
        assert_eq!(p.pr_cache_file(), base.join("cache/prs.json"));
        assert_eq!(p.runtime_cache_file(), base.join("cache/runtime.json"));
        assert_eq!(p.worktrees_dir(), base.join("worktrees"));
        assert_eq!(p.worktree_path("feat+x"), base.join("worktrees/feat+x"));
        assert_eq!(p.logs_dir("feat+x"), base.join("logs/feat+x"));
        assert_eq!(
            p.log_file("feat+x", "dev"),
            base.join("logs/feat+x/dev.log")
        );
        assert_eq!(p.data_dir("feat+x"), base.join("data/feat+x"));
        assert_eq!(p.compose_dir(), base.join("compose"));
        assert_eq!(
            p.compose_override_file("feat+x"),
            base.join("compose/feat+x.override.yml")
        );
        assert_eq!(
            p.tunnel_config_file(),
            PathBuf::from("/tmp/pando-home/tunnel-config.yml")
        );
        assert_eq!(
            p.user_config_file(),
            PathBuf::from("/tmp/pando-home/config.toml"),
            "the machine-wide layer is one file for every project"
        );
    }

    #[test]
    fn every_location_is_under_home_and_never_under_the_repository() {
        let p = paths();
        let locations = [
            p.project_dir(),
            p.config_file(),
            p.state_file(),
            p.lock_file(),
            p.cache_dir(),
            p.enrich_cache_file(),
            p.pr_cache_file(),
            p.runtime_cache_file(),
            p.worktrees_dir(),
            p.worktree_path("feat+x"),
            p.logs_dir("feat+x"),
            p.log_file("feat+x", "dev"),
            p.data_dir("feat+x"),
            p.compose_dir(),
            p.compose_override_file("feat+x"),
            p.tunnel_config_file(),
            p.user_config_file(),
        ];
        for loc in locations {
            assert!(loc.starts_with(&p.home), "{} escapes home", loc.display());
            assert!(
                !loc.starts_with(p.root()),
                "{} is inside the repository — invariant 1 forbids it",
                loc.display()
            );
        }
    }

    // A process name is a path component of its log file, so anything that
    // is not one component is a write outside the logs directory waiting to
    // happen. Both separators, because a name with a backslash in it is one
    // component here and not on every filesystem pando's logs might be read
    // from.
    #[test]
    fn a_log_source_that_is_not_one_path_component_is_refused() {
        for bad in [
            "../../../../../escaped-log",
            "apps/web",
            "a\\b",
            "/absolute",
            ".",
            "..",
            "",
            "   ",
        ] {
            let err = validate_log_source("process name", bad)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("logs/<worktree>"),
                "{bad:?} was refused without saying why: {err}"
            );
        }
    }

    #[test]
    fn a_plain_name_is_a_log_source_whatever_alphabet_it_is_in() {
        for good in ["dev", "web", "api", "wörker", "install", ".hidden", "a b"] {
            validate_log_source("process name", good)
                .unwrap_or_else(|e| panic!("{good:?} should be a usable log source: {e:#}"));
        }
    }

    #[test]
    fn the_names_pando_writes_its_own_logs_under_are_refused_to_processes() {
        for reserved in RESERVED_LOG_SOURCES {
            let err = validate_owned_log_source("process name", reserved)
                .unwrap_err()
                .to_string();
            assert!(err.contains("reserved"), "{err}");
            assert!(err.contains(reserved), "{err}");
        }
        // Only the exact names; `installer` is a perfectly good process.
        validate_owned_log_source("process name", "installer").unwrap();
        // And `logs --source install` still reads the hook's own log.
        validate_log_source("log source", "install").unwrap();
    }

    // `[processes."apps/web"]` is a name a developer plausibly writes, so
    // the refusal says what they meant rather than only that they are wrong.
    #[test]
    fn a_path_shaped_name_suggests_its_last_component() {
        let err = validate_log_source("process name", "apps/web")
            .unwrap_err()
            .to_string();
        assert!(err.contains("try \"web\""), "{err}");
        // Nothing usable to suggest, so nothing is suggested.
        let err = validate_log_source("process name", "../..")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("try "), "{err}");
    }

    #[test]
    fn locations_depend_only_on_home_and_project_id() {
        let a = paths();
        let mut project = a.project.clone();
        project.root = PathBuf::from("/somewhere/else");
        let b = PandoPaths::new(a.home.clone(), project);
        assert_eq!(a.project_dir(), b.project_dir());
        assert_eq!(a.state_file(), b.state_file());
        assert_eq!(a.worktree_path("n"), b.worktree_path("n"));
    }

    #[test]
    fn ensure_home_creates_a_private_home_and_project_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("pando-home");
        let p = PandoPaths::new(&home, paths().project);
        p.ensure_home().unwrap();
        p.ensure_home().unwrap(); // idempotent

        assert!(p.project_dir().is_dir());
        let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, HOME_MODE,
            "pando home must be 0700 — later phases copy env files into it"
        );
    }

    // `ensure_home` is the last gate before pando's home is created, so it
    // refuses a home inside the repository even when nothing checked earlier.
    #[test]
    fn ensure_home_refuses_a_home_inside_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        let project = ProjectRef {
            id: "acme-shop-3f9a2c1d".into(),
            root: root.clone(),
            display_name: "acme-shop".into(),
        };
        let home = root.join(".pando");
        let p = PandoPaths::new(&home, project);

        let err = p.ensure_home().unwrap_err();
        assert!(
            format!("{err:#}").contains("inside the repository"),
            "unexpected error: {err:#}"
        );
        assert!(!home.exists(), "a refused home must not be created");
    }

    // The only test that touches the environment. `default_home` is the one
    // function in the crate that reads it, and nothing else calls that.
    #[test]
    fn default_home_honours_pando_home_then_falls_back() {
        let previous = std::env::var_os("PANDO_HOME");
        unsafe { std::env::set_var("PANDO_HOME", "/tmp/injected-pando") };
        assert_eq!(default_home(), PathBuf::from("/tmp/injected-pando"));

        unsafe { std::env::set_var("PANDO_HOME", "") };
        assert!(
            default_home().ends_with(".pando"),
            "an empty PANDO_HOME must fall back to ~/.pando"
        );

        unsafe { std::env::remove_var("PANDO_HOME") };
        assert!(default_home().ends_with(".pando"));

        if let Some(v) = previous {
            unsafe { std::env::set_var("PANDO_HOME", v) };
        }
    }
}
