//! The probe cache: a resolved runtime remembered until what it depends
//! on changes.

use super::Requirement;
use super::languages::language;
use super::probe::probe_command;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---- the probe cache ------------------------------------------------------

pub(super) const CACHE_VERSION: u32 = 1;

/// Probes that came back satisfied, keyed by a fingerprint of what was
/// asked and how.
///
/// Only matches are kept, deliberately. A cached mismatch would keep
/// reporting a failure the developer has just fixed, and a mismatch stops
/// the start anyway, so there is no spawn to save by remembering it. What
/// this buys is the common case: a start costs one extra spawn when the
/// requirement or the prelude changes, and none when neither has.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProbeCache {
    pub version: u32,
    /// Fingerprint to the version that satisfied it, which is what makes
    /// the file readable when something has to be explained.
    pub satisfied: BTreeMap<String, String>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            satisfied: BTreeMap::new(),
        }
    }

    pub fn holds(&self, fingerprint: &str) -> bool {
        self.satisfied.contains_key(fingerprint)
    }

    pub fn remember(&mut self, fingerprint: String, version: String) {
        self.satisfied.insert(fingerprint, version);
    }
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// What a cached probe is keyed on: everything that could change its
/// answer except the machine itself — the requirement, where it came from,
/// the prelude in front of it, and the command that would be run.
pub fn fingerprint(requirement: &Requirement, prelude: &str) -> String {
    let probe = language(&requirement.language)
        .map(|language| probe_command(language, prelude))
        .unwrap_or_default();
    let mut context = md5::Context::new();
    for part in [
        requirement.language.as_str(),
        requirement.spec.as_str(),
        requirement.source.as_str(),
        prelude,
        &probe,
    ] {
        context.consume(part.as_bytes());
        context.consume([0]);
    }
    format!("md5:{:x}", context.finalize())
}

pub fn load_cache(path: &Path) -> ProbeCache {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProbeCache::new();
    };
    match serde_json::from_str::<ProbeCache>(&text) {
        Ok(cache) if cache.version == CACHE_VERSION => cache,
        _ => ProbeCache::new(),
    }
}

/// Writes the cache atomically, through a temp file that is this writer's
/// own, named for the process and a counter.
///
/// Two starts that learn something at once both save. With one shared
/// temp name, the first rename moved the file the second was about to
/// rename, and the second failed on a cache nothing was wrong with.
pub fn save_cache(path: &Path, cache: &ProbeCache) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cache).context("serialize the runtime probe cache")?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(json.as_bytes()))
        .with_context(|| format!("write {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, path)
                .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}
