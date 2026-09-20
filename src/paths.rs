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

    /// `source` is a free string: `dev`, a service name, a hook name,
    /// `tunnel`, `proxy`. The TUI builds its log tabs from the files that
    /// exist, so nothing here enumerates the set.
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
            p.worktrees_dir(),
            p.worktree_path("feat+x"),
            p.logs_dir("feat+x"),
            p.log_file("feat+x", "dev"),
            p.data_dir("feat+x"),
            p.compose_dir(),
            p.compose_override_file("feat+x"),
            p.tunnel_config_file(),
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
