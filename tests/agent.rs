//! The agent layer's packaging: the Claude Code plugin and the Codex
//! skills.
//!
//! Both hosts get wrappers, and the whole design rests on those wrappers
//! staying *thin*. The reasoning lives once, in `agent/brief.md`; two
//! copies of it drift within a month and nobody notices, because a
//! procedure for a language model still reads perfectly when it is wrong.
//! So the tests here are mostly about what the wrappers must **not**
//! become.
//!
//! What a document *says* is held to what the binary takes by
//! `cli::tests::assert_every_documented_command_is_real`, which reads
//! these same files.

use std::path::{Path, PathBuf};

/// How much glue a host wrapper is allowed. The phase plan says "under
/// fifty lines of glue each"; this counts the body, since the frontmatter
/// is the host's own metadata rather than anything a reader follows.
const MAX_WRAPPER_BODY_LINES: usize = 50;

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read(relative: &str) -> String {
    let path = repo(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every host wrapper, by path. One list, so a host added later cannot
/// escape any test here.
const WRAPPERS: [&str; 4] = [
    "agent/skills/pando-setup/SKILL.md",
    "agent/skills/pando-operate/SKILL.md",
    "agent/codex/pando-setup/SKILL.md",
    "agent/codex/pando-operate/SKILL.md",
];

/// The two skills, under the names both hosts use for them.
const SKILLS: [&str; 2] = ["pando-setup", "pando-operate"];

/// The frontmatter block and the body, split at the closing `---`.
fn frontmatter_and_body(text: &str) -> (String, String) {
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "a skill file starts with its frontmatter"
    );
    let mut front = String::new();
    for line in lines.by_ref() {
        if line == "---" {
            return (front, lines.collect::<Vec<_>>().join("\n"));
        }
        front.push_str(line);
        front.push('\n');
    }
    panic!("frontmatter was never closed");
}

#[test]
fn every_wrapper_declares_a_name_and_a_description() {
    for file in WRAPPERS {
        let (front, _) = frontmatter_and_body(&read(file));
        // The name a host lists it under, and the sentence a host matches
        // against what the developer asked for. A skill with no
        // description is a skill nothing ever invokes.
        assert!(front.contains("name: "), "{file} declares no name");
        let description = front
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .unwrap_or_else(|| panic!("{file} declares no description"));
        assert!(
            description.len() > 60,
            "{file}'s description is too short to match anything: {description:?}"
        );
        assert!(
            description.to_lowercase().contains("pando"),
            "{file}'s description never says what tool it is about"
        );
    }
}

/// The load-bearing test of this phase.
///
/// If a wrapper grows past glue, it has started to contain reasoning —
/// and the moment there are two copies of the reasoning, the one nobody
/// is reading goes stale silently. The brief is the only place it lives.
#[test]
fn no_wrapper_is_thick_enough_to_hold_reasoning() {
    for file in WRAPPERS {
        let (_, body) = frontmatter_and_body(&read(file));
        let lines = body.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            lines <= MAX_WRAPPER_BODY_LINES,
            "{file} is {lines} lines of glue — over {MAX_WRAPPER_BODY_LINES}, it has started \
             to contain reasoning, and the brief is where that belongs"
        );
    }
}

#[test]
fn every_wrapper_sends_its_reader_to_the_brief() {
    for file in WRAPPERS {
        let text = read(file);
        assert!(
            text.contains("brief.md"),
            "{file} never points at the brief, so it is either useless or a second copy of it"
        );
    }
}

/// A path a wrapper names has to resolve from where that wrapper is
/// installed, which is not where it sits in this repository.
///
/// The Claude Code plugin root is `agent/` — which is why the brief lives
/// there and not one directory up. A plugin is installed by a sparse
/// checkout of the paths it declares, so `${CLAUDE_PLUGIN_ROOT}/../` is
/// not a place anything can be relied on to exist.
#[test]
fn the_brief_is_reachable_from_every_wrapper_as_it_names_it() {
    for skill in SKILLS {
        let text = read(&format!("agent/skills/{skill}/SKILL.md"));
        assert!(
            text.contains("${CLAUDE_PLUGIN_ROOT}/brief.md"),
            "agent/skills/{skill} must reach the brief through the plugin root, which is agent/"
        );
    }
    // Which is only true because the plugin root is the directory the
    // brief is in.
    assert!(repo("agent/.claude-plugin/plugin.json").is_file());
    assert!(repo("agent/brief.md").is_file());
    assert!(repo("agent/json.md").is_file());

    // Codex has no such variable, so its installer puts the brief beside
    // the wrapper that names it. Run for real, into a temporary home —
    // the claim "beside this file" is worth nothing unless something
    // checks that it lands there.
    let home = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("bash")
        .arg(repo("agent/codex/install.sh"))
        .env("CODEX_HOME", home.path())
        .output()
        .expect("run the installer");
    assert!(
        out.status.success(),
        "the installer failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for skill in SKILLS {
        let installed = home.path().join("skills").join(skill);
        for file in ["SKILL.md", "brief.md", "json.md"] {
            assert!(
                installed.join(file).is_file(),
                "{} is not there after an install",
                installed.join(file).display()
            );
        }
        // The same brief, not a second one.
        assert_eq!(
            std::fs::read_to_string(installed.join("brief.md")).unwrap(),
            read("agent/brief.md"),
            "the installed brief is not the one in the repository"
        );
        assert_eq!(
            std::fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            read(&format!("agent/codex/{skill}/SKILL.md"))
        );
    }
    // And it wrote nowhere else: a script that installs into somebody's
    // real home when asked for a temporary one is a script nothing can
    // test.
    let mut top: Vec<String> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    top.sort();
    assert_eq!(top, vec!["skills"]);
}

/// Both hosts, the same two skills, under the same two names.
///
/// A developer who moves between them should not have to learn a second
/// vocabulary, and a bug report naming a skill should be findable in
/// either packaging.
#[test]
fn both_hosts_ship_the_same_two_skills_under_the_same_names() {
    for host in ["agent/skills", "agent/codex"] {
        for skill in SKILLS {
            let file = format!("{host}/{skill}/SKILL.md");
            let (front, _) = frontmatter_and_body(&read(&file));
            assert!(
                front.contains(&format!("name: {skill}")),
                "{file} calls itself something else"
            );
        }
    }
    // And they describe themselves identically, because they are the same
    // skill: a host that matched one and not the other would send a
    // developer down two different paths for the same request.
    for skill in SKILLS {
        let described = |host: &str| {
            frontmatter_and_body(&read(&format!("{host}/{skill}/SKILL.md")))
                .0
                .lines()
                .find_map(|l| l.strip_prefix("description: ").map(str::to_string))
                .expect("a description")
        };
        assert_eq!(
            described("agent/skills"),
            described("agent/codex"),
            "{skill} means two different things to the two hosts"
        );
    }
}

#[test]
fn the_plugin_manifest_is_valid_and_the_marketplace_points_at_it() {
    let plugin: serde_json::Value =
        serde_json::from_str(&read("agent/.claude-plugin/plugin.json")).expect("valid JSON");
    assert_eq!(plugin["name"], "pando");
    assert!(plugin["description"].as_str().is_some_and(|d| d.len() > 40));
    assert!(plugin["version"].as_str().is_some());

    let market: serde_json::Value =
        serde_json::from_str(&read(".claude-plugin/marketplace.json")).expect("valid JSON");
    let entries = market["plugins"].as_array().expect("a list of plugins");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], plugin["name"]);
    let source = entries[0]["source"].as_str().expect("a source");
    assert_eq!(
        source, "./agent",
        "the marketplace must point at the directory that holds the brief"
    );
    assert!(
        repo(source.trim_start_matches("./"))
            .join(".claude-plugin/plugin.json")
            .is_file(),
        "the marketplace's source has no plugin manifest in it"
    );
}
