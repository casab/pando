use super::probe::DONE_MARK;
use super::probe::PATH_MARK;
use super::probe::VERSION_MARK;
use super::*;
use std::path::Path;
use tempfile::TempDir;

fn repo(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    dir
}

fn found(files: &[(&str, &str)]) -> Vec<(String, String, String, bool)> {
    let dir = repo(files);
    requirements(dir.path())
        .into_iter()
        .map(|r| (r.language, r.spec, r.source, r.pinned))
        .collect()
}

#[test]
fn a_repository_that_says_nothing_requires_nothing() {
    assert!(found(&[("README.md", "hi\n")]).is_empty());
}

#[test]
fn the_node_version_files_pin_node() {
    assert_eq!(
        found(&[(".nvmrc", "22\n")]),
        vec![("node".into(), "22".into(), ".nvmrc".into(), true)]
    );
    // A `v` prefix is decoration, and the spec is recorded as written.
    assert_eq!(
        found(&[(".node-version", "v20.11.0\n")]),
        vec![(
            "node".into(),
            "v20.11.0".into(),
            ".node-version".into(),
            true
        )]
    );
}

#[test]
fn an_alias_that_names_no_version_is_recorded_and_is_not_a_pin() {
    let out = found(&[(".nvmrc", "lts/hydrogen\n")]);
    assert_eq!(out[0].1, "lts/hydrogen");
    assert!(!out[0].3, "an alias is not a pin: {out:?}");
}

#[test]
fn engines_is_read_and_is_a_range() {
    let out = found(&[(
        "package.json",
        r#"{"name":"x","engines":{"node":">=18 <21","pnpm":"9"}}"#,
    )]);
    assert_eq!(
        out,
        vec![
            (
                "node".into(),
                ">=18 <21".into(),
                "package.json engines.node".into(),
                false
            ),
            // Not a language pando probes, and still a fact about the
            // project that `signals --json` publishes.
            (
                "pnpm".into(),
                "9".into(),
                "package.json engines.pnpm".into(),
                true
            ),
        ]
    );
}

// Both are recorded — a report that names only one of them cannot
// explain the mismatch — and the pin is the one to compare against.
#[test]
fn a_pin_and_a_range_are_both_recorded_with_the_pin_first() {
    let out = found(&[
        (".nvmrc", "22\n"),
        ("package.json", r#"{"engines":{"node":">=18"}}"#),
    ]);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].2, ".nvmrc");
    assert!(out[0].3, "the pin sorts first");
    assert_eq!(out[1].2, "package.json engines.node");

    let dir = repo(&[
        (".nvmrc", "22\n"),
        ("package.json", r#"{"engines":{"node":">=18"}}"#),
    ]);
    let all = requirements(dir.path());
    assert_eq!(
        for_language(&all, "node").map(|r| r.spec.as_str()),
        Some("22"),
        "the pin is what a comparison uses"
    );
    assert!(for_language(&all, "elixir").is_none());
}

// Even when the range is written first, because the file order is not
// the project's opinion about which one is more specific.
#[test]
fn a_range_stated_before_a_pin_still_loses_to_it() {
    let out = found(&[
        ("mise.toml", "[tools]\nnode = \">=18\"\n"),
        (".tool-versions", "nodejs 22.11.0\n"),
    ]);
    assert_eq!(out[0].1, "22.11.0", "{out:?}");
    assert_eq!(out[1].1, ">=18", "{out:?}");
}

#[test]
fn tool_versions_names_several_languages_and_normalises_their_names() {
    let out = found(&[(
        ".tool-versions",
        "# a comment\nnodejs 22.11.0\npython 3.12.1 3.11.6\ngolang 1.22.0\n\njava temurin-21.0.1\n",
    )]);
    assert_eq!(
        out,
        vec![
            (
                "node".into(),
                "22.11.0".into(),
                ".tool-versions".into(),
                true
            ),
            // The first version only: the rest are fallbacks.
            (
                "python".into(),
                "3.12.1".into(),
                ".tool-versions".into(),
                true
            ),
            ("go".into(), "1.22.0".into(), ".tool-versions".into(), true),
            // A vendor prefix is decoration too, so this is still a pin.
            (
                "java".into(),
                "temurin-21.0.1".into(),
                ".tool-versions".into(),
                true
            ),
        ]
    );
}

#[test]
fn mise_tools_are_read_in_every_shape_they_are_written_in() {
    let out = found(&[(
        "mise.toml",
        "[tools]\nnode = \"22\"\npython = [\"3.12\", \"3.11\"]\nruby = { version = \"3.2\" }\n",
    )]);
    assert_eq!(
        out,
        vec![
            ("node".into(), "22".into(), "mise.toml".into(), true),
            ("python".into(), "3.12".into(), "mise.toml".into(), true),
            ("ruby".into(), "3.2".into(), "mise.toml".into(), true),
        ]
    );
}

// The dot-prefixed spelling is the common one in the wild, and a project
// pinning its runtimes that way used to be invisible to pando.
#[test]
fn a_dot_mise_toml_is_read_like_mise_toml() {
    assert_eq!(
        found(&[(".mise.toml", "[tools]\nnode = \"22\"\n")]),
        vec![("node".into(), "22".into(), ".mise.toml".into(), true)]
    );
    // Both present: mise lets `mise.toml` win, so its entry sorts first.
    let both = found(&[
        ("mise.toml", "[tools]\nnode = \"22\"\n"),
        (".mise.toml", "[tools]\nnode = \"20\"\n"),
    ]);
    assert_eq!(both[0].2, "mise.toml", "{both:?}");
    assert_eq!(both[1].2, ".mise.toml", "{both:?}");
}

#[test]
fn a_rust_toolchain_file_is_read_through_its_channel_key() {
    assert_eq!(
        found(&[("rust-toolchain.toml", "[toolchain]\nchannel = \"1.75.0\"\n")]),
        vec![(
            "rust".into(),
            "1.75.0".into(),
            "rust-toolchain.toml".into(),
            true
        )]
    );
    let out = found(&[("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n")]);
    assert!(!out[0].3, "a channel name is not a pin: {out:?}");
}

#[test]
fn a_ruby_version_file_carries_its_vendor_prefix_and_is_still_a_pin() {
    let out = found(&[(".ruby-version", "ruby-3.2.2\n")]);
    assert_eq!(out[0].1, "ruby-3.2.2");
    assert!(out[0].3);
    assert_eq!(normalize_spec("ruby-3.2.2"), "3.2.2");
}

#[test]
fn comments_and_blank_lines_are_not_versions() {
    let out = found(&[(
        ".python-version",
        "\n# pyenv writes these\n3.12.1\n3.11.6\n",
    )]);
    assert_eq!(out[0].1, "3.12.1", "{out:?}");
    assert_eq!(out.len(), 1, "only the first line is the version: {out:?}");
}

#[test]
fn a_file_pando_cannot_parse_is_not_a_requirement() {
    assert!(found(&[("package.json", "{ not json")]).is_empty());
    assert!(found(&[("mise.toml", "[tools\nnode =")]).is_empty());
    assert!(found(&[("rust-toolchain.toml", "channel = 3")]).is_empty());
    assert!(found(&[(".nvmrc", "\n\n")]).is_empty());
}

// ---- what the machine resolves ---------------------------------------

fn node() -> &'static Language {
    language("node").expect("node is in the table")
}

/// A shell that answers with whatever a test decided this machine is.
fn machine(path: &str, version: &str) -> impl Fn(&str) -> Option<String> {
    let reply = format!("{PATH_MARK}{path}\n{VERSION_MARK}{version}\n{DONE_MARK}\n");
    move |_cmd: &str| Some(reply.clone())
}

fn check_node(spec: &str, shell: Shell<'_>) -> Check {
    check(&Requirement::new("node", spec, ".nvmrc".into()), "", shell)
}

#[test]
fn the_probe_asks_for_the_path_as_well_as_the_version() {
    let command = probe_command(node(), "");
    assert!(command.contains("command -v"), "{command}");
    assert!(command.contains("for __pando_bin in node;"), "{command}");
    assert!(command.contains("-v 2>&1"), "{command}");
    // Both markers, because a prelude that prints something of its own
    // must not be mistaken for the answer.
    assert!(command.contains(PATH_MARK) && command.contains(VERSION_MARK));
    // Every binary the language may go by, in order.
    let python = probe_command(language("python").unwrap(), "");
    assert!(python.contains("python3 python"), "{python}");
}

#[test]
fn a_prelude_is_composed_the_way_a_real_spawn_composes_it() {
    let command = probe_command(node(), "nvm use");
    assert!(
        command.starts_with("nvm use && {"),
        "the probe measures what a spawn would do: {command}"
    );
}

#[test]
fn chatter_from_the_prelude_is_not_mistaken_for_the_answer() {
    let shell = |_: &str| {
        Some(format!(
            "Now using node v22.11.0 (npm v10.9.0)\n\
                 {PATH_MARK}/home/dev/.nvm/versions/node/v22.11.0/bin/node\n\
                 {VERSION_MARK}v22.11.0\n{DONE_MARK}\n"
        ))
    };
    let check = check_node("22", &shell);
    assert_eq!(check.verdict, Verdict::Satisfied);
    assert_eq!(check.resolved.version.as_deref(), Some("22.11.0"));
    assert_eq!(
        check.resolved.path.as_deref(),
        Some("/home/dev/.nvm/versions/node/v22.11.0/bin/node")
    );
}

#[test]
fn a_pin_the_machine_does_not_meet_is_a_mismatch_with_the_path_it_resolved() {
    let shell = machine("/opt/homebrew/bin/node", "v24.21.0");
    let check = check_node("22", &shell);
    assert_eq!(check.verdict, Verdict::Mismatch);
    // The path, not only the version: pando's shell is not the
    // developer's shell, and this is the line that says so.
    assert_eq!(
        check.resolved.path.as_deref(),
        Some("/opt/homebrew/bin/node")
    );
}

#[test]
fn a_binary_that_is_not_there_at_all_is_a_mismatch() {
    let shell = |_: &str| Some(format!("{DONE_MARK}\n"));
    let check = check_node("22", &shell);
    assert_eq!(check.verdict, Verdict::Mismatch);
    assert_eq!(check.resolved.path, None);
    assert!(
        check.resolved.ran,
        "the probe ran; there was nothing to find"
    );
}

#[test]
fn a_prelude_that_fails_never_reaches_the_probe_and_says_so() {
    let shell = |_: &str| Some("bash: nvm: command not found\n".to_string());
    let check = check(
        &Requirement::new("node", "22", ".nvmrc".into()),
        "nvm use 22",
        &shell,
    );
    assert_eq!(check.verdict, Verdict::Mismatch);
    assert!(!check.resolved.ran);
    assert_eq!(
        check.resolved.failure.as_deref(),
        Some("bash: nvm: command not found")
    );
}

// Everything pando cannot judge proceeds: a check that guesses is
// worse than no check.
#[test]
fn a_shell_that_cannot_run_or_a_spec_that_cannot_be_read_blocks_nothing() {
    let dead = |_: &str| None;
    assert_eq!(check_node("22", &dead).verdict, Verdict::Unknown);

    let shell = machine("/usr/bin/node", "v24.21.0");
    assert_eq!(check_node("lts/hydrogen", &shell).verdict, Verdict::Unknown);

    // There, but it answered with something that is not a version.
    let odd = |_: &str| {
        Some(format!(
            "{PATH_MARK}/usr/bin/node\n{VERSION_MARK}\n{DONE_MARK}\n"
        ))
    };
    assert_eq!(check_node("22", &odd).verdict, Verdict::Unknown);

    // And a language with no probe in the table is recorded, never
    // judged.
    let pnpm = Requirement::new("pnpm", "9", "package.json engines.pnpm".into());
    assert_eq!(check(&pnpm, "", &shell).verdict, Verdict::Unknown);
}

#[test]
fn every_languages_version_output_reads_the_same_way() {
    for (output, want) in [
        ("v22.14.0", "22.14.0"),
        ("Python 3.11.5", "3.11.5"),
        ("ruby 3.2.2p53 (2023-03-30 revision e51014f9c0)", "3.2.2"),
        ("rustc 1.75.0 (82e1608df 2023-12-21)", "1.75.0"),
        ("go version go1.22.0 darwin/arm64", "1.22.0"),
        ("openjdk version \"21.0.1\" 2023-10-17", "21.0.1"),
    ] {
        assert_eq!(first_version(output).as_deref(), Some(want), "{output}");
    }
    assert_eq!(first_version("command not found"), None);
}

#[test]
fn the_comparisons_a_version_file_and_an_engines_range_actually_use() {
    for (spec, version, want) in [
        // A pin matches on the components it names, and no others.
        ("22", "22.14.0", Verdict::Satisfied),
        ("22", "24.21.0", Verdict::Mismatch),
        ("22.14.0", "22.14.1", Verdict::Mismatch),
        ("v20.11.0", "20.11.0", Verdict::Satisfied),
        ("3.12", "3.12.1", Verdict::Satisfied),
        ("18.x", "18.2.0", Verdict::Satisfied),
        ("18.x", "20.2.0", Verdict::Mismatch),
        // Ranges, as `engines` writes them.
        (">=18", "24.21.0", Verdict::Satisfied),
        (">=18", "16.20.0", Verdict::Mismatch),
        (">=18 <21", "24.21.0", Verdict::Mismatch),
        (">=18 <21", "20.11.0", Verdict::Satisfied),
        (">=18 || >=20", "24.0.0", Verdict::Satisfied),
        // An operator may stand apart from its version.
        (">= 18", "22.0.0", Verdict::Satisfied),
        (">= 18 < 21", "20.1.0", Verdict::Satisfied),
        (">= 18 < 21", "22.0.0", Verdict::Mismatch),
        ("^ 18", "18.2.0", Verdict::Satisfied),
        // A hyphen range, whose partial upper end takes in its minors.
        ("18 - 22", "20.1.0", Verdict::Satisfied),
        ("18 - 22", "22.5.0", Verdict::Satisfied),
        ("18 - 22", "23.0.0", Verdict::Mismatch),
        ("18 - 22", "16.0.0", Verdict::Mismatch),
        ("18.2.0 - 22.1.3", "22.1.4", Verdict::Mismatch),
        // And so does any partial bound that includes its end.
        ("<=22", "22.5.0", Verdict::Satisfied),
        ("<=22", "23.0.0", Verdict::Mismatch),
        ("<=22.3", "22.3.9", Verdict::Satisfied),
        (">22", "22.5.0", Verdict::Mismatch),
        (">22", "23.0.0", Verdict::Satisfied),
        (">22.1.0", "22.1.1", Verdict::Satisfied),
        ("^18.0.0", "18.20.1", Verdict::Satisfied),
        ("^18.0.0", "19.0.0", Verdict::Mismatch),
        ("^0.2.3", "0.2.9", Verdict::Satisfied),
        ("^0.2.3", "0.3.0", Verdict::Mismatch),
        ("~3.2", "3.2.9", Verdict::Satisfied),
        ("~3.2", "3.3.0", Verdict::Mismatch),
        ("*", "24.0.0", Verdict::Satisfied),
        // And everything this build does not model.
        ("lts/*", "22.0.0", Verdict::Unknown),
        ("stable", "1.75.0", Verdict::Unknown),
        ("", "22.0.0", Verdict::Unknown),
        (">=18 lts/*", "24.0.0", Verdict::Unknown),
        (">=", "22.0.0", Verdict::Unknown),
        (">=18 - 22", "20.0.0", Verdict::Unknown),
        ("18 - 22 <25", "20.0.0", Verdict::Unknown),
        // A definite failure is still a failure, whatever sits beside it.
        ("<18 lts/*", "24.0.0", Verdict::Mismatch),
    ] {
        assert_eq!(
            satisfies(spec, version),
            want,
            "{spec:?} against {version:?}"
        );
    }
}

// ---- version managers -------------------------------------------------

fn home_with(paths: &[&str]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for path in paths {
        let full = dir.path().join(path);
        if path.ends_with(".sh") || path.ends_with("fnm") || path.ends_with("rvm") {
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "#!/bin/sh\n").unwrap();
        } else {
            std::fs::create_dir_all(full).unwrap();
        }
    }
    dir
}

#[test]
fn a_source_and_eval_manager_gets_an_init_line_and_a_use_line() {
    let home = home_with(&[".nvm/nvm.sh"]);
    let requirement = Requirement::new("node", "22", ".nvmrc".into());
    // Scoped to the injected home: Homebrew installs nvm and fnm
    // outside `$HOME`, so an unfiltered count is a fact about the host
    // and not about the code.
    let fixes = from_home(node(), &home, from_version_file(node(), &requirement));
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    let line = &fix_from(&fixes, "nvm").line;
    assert!(line.contains("nvm.sh"), "{line}");
    // No version in it: the line lands in a file every project on the
    // machine shares, and `nvm use` reads the repository's own file.
    assert!(line.ends_with("nvm use >/dev/null"), "{line}");
    assert!(!line.contains("nvm use 22"), "{line}");
}

// A requirement that came from `engines` is not in a file nvm can
// read, so there is nothing for a bare `nvm use` to find.
#[test]
fn a_requirement_no_manager_can_read_gets_the_init_line_alone() {
    let home = home_with(&[".nvm/nvm.sh"]);
    let requirement = Requirement::new("node", ">=18", "package.json engines.node".into());
    let fixes = from_home(node(), &home, from_version_file(node(), &requirement));
    let nvm = fix_from(&fixes, "nvm");
    assert!(!nvm.line.contains("nvm use"), "{nvm:?}");
}

// A shim manager resolves per directory by itself. When it fails it is
// a PATH problem in a login bash shell, and a `use` line would be the
// wrong fix.
/// Only the fixes that came from the injected home. A manager can also
/// be installed system-wide — Homebrew puts nvm in its own prefix —
/// and whether this machine has one is not something a test decides.
fn from_home(language: &Language, home: &TempDir, reads_version_file: bool) -> Vec<Fix> {
    let home_path = home.path().display().to_string();
    fixes(language, home.path(), reads_version_file)
        .into_iter()
        .filter(|fix| fix.line.contains(&home_path))
        .collect()
}

/// One installed manager, by name.
fn manager_named(language: &Language, home: &Path, name: &str) -> &'static Manager {
    installed(language, home)
        .into_iter()
        .find(|manager| manager.name == name)
        .unwrap_or_else(|| panic!("{name} is installed in this home"))
}

/// The fix from one named manager. By name, never by index: the table's
/// order is a product decision, and a test that quietly checked a
/// different manager after a reorder would be worse than one that fails.
fn fix_from<'a>(fixes: &'a [Fix], manager: &str) -> &'a Fix {
    fixes
        .iter()
        .find(|fix| fix.manager == manager)
        .unwrap_or_else(|| panic!("{manager} is installed in this home: {fixes:?}"))
}

#[test]
fn a_shim_manager_gets_a_path_line_and_never_a_use_line() {
    let home = home_with(&[".volta/bin"]);
    let fixes = from_home(node(), &home, true);
    let volta = fix_from(&fixes, "volta");
    assert!(volta.line.starts_with("export PATH="), "{volta:?}");
    assert!(!volta.line.contains("use"), "{volta:?}");
    assert!(volta.line.contains(".volta/bin"), "{volta:?}");
}

#[test]
fn only_the_managers_this_machine_has_are_offered() {
    let empty = TempDir::new().unwrap();
    assert!(
        from_home(node(), &empty, true).is_empty(),
        "a home with no manager in it offers no line"
    );

    // And the order is the table's: a PATH line is the sturdier fix in
    // a non-interactive shell, so it comes first.
    let home = home_with(&[".nvm/nvm.sh", ".volta/bin"]);
    let offered: Vec<&str> = from_home(node(), &home, true)
        .iter()
        .map(|fix| fix.manager)
        .collect();
    assert_eq!(offered, vec!["volta", "nvm"]);
}

// Printed, never run: installing a toolchain is the developer's call.
#[test]
fn an_install_command_uses_the_name_the_manager_itself_uses() {
    let home = home_with(&[".asdf/shims"]);
    let asdf = manager_named(node(), home.path(), "asdf");
    assert_eq!(
        asdf.install_command(node(), "22").as_deref(),
        Some("asdf install nodejs 22"),
        "asdf's plugin is nodejs, not node"
    );
    let volta_home = home_with(&[".volta/bin"]);
    let volta = manager_named(node(), volta_home.path(), "volta");
    assert_eq!(
        volta.install_command(node(), "v22").as_deref(),
        Some("volta install node@22")
    );
}

// ---- the probe cache --------------------------------------------------

#[test]
fn the_cache_keys_on_the_requirement_and_the_prelude() {
    let a = Requirement::new("node", "22", ".nvmrc".into());
    let b = Requirement::new("node", "24", ".nvmrc".into());
    assert_eq!(fingerprint(&a, ""), fingerprint(&a, ""));
    assert_ne!(fingerprint(&a, ""), fingerprint(&b, ""));
    assert_ne!(
        fingerprint(&a, ""),
        fingerprint(&a, "nvm use"),
        "a new prelude is a new question"
    );
}

#[test]
fn the_cache_round_trips_and_a_future_version_is_ignored() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache").join("runtime.json");
    let mut cache = ProbeCache::new();
    cache.remember("md5:abc".into(), "22.11.0".into());
    save_cache(&path, &cache).unwrap();
    assert_eq!(load_cache(&path), cache);
    assert!(load_cache(&path).holds("md5:abc"));

    std::fs::write(&path, r#"{"version":99,"satisfied":{}}"#).unwrap();
    assert!(!load_cache(&path).holds("md5:abc"));
    assert!(!load_cache(&dir.path().join("nothing.json")).holds("md5:abc"));
}
