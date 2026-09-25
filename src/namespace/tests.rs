use super::*;
use crate::state::{NamespaceKind, NamespaceRecord, State, WorktreeRecord};
use chrono::Utc;

// ---- names ------------------------------------------------------------------

fn names(main: &str, worktree: &str) -> [String; 2] {
    database_names(main, "acme-0000beef", worktree).unwrap()
}

#[test]
fn a_worktrees_database_is_the_main_one_the_marker_and_its_own_name() {
    let [readable, hashed] = names("northwind_traders", "feat+x");
    assert_eq!(readable, "northwind_traders__feat_x");
    assert!(hashed.starts_with("northwind_traders__feat_x_"), "{hashed}");
    assert_eq!(hashed.len(), "northwind_traders__feat_x_".len() + 8);
    assert!(
        hashed[hashed.len() - 8..]
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    );
}

#[test]
fn a_worktree_name_becomes_lowercase_letters_digits_and_single_underscores() {
    for (worktree, tail) in [
        ("feat+login", "feat_login"),
        ("Fix/Some Thing!!", "fix_some_thing"),
        ("--leading+and+trailing--", "leading_and_trailing"),
        ("a____b", "a_b"),
        ("pr-12+add-caché", "pr_12_add_cach"),
        ("v2.0", "v2_0"),
    ] {
        assert_eq!(
            names("shop", worktree)[0],
            format!("shop__{tail}"),
            "{worktree}"
        );
    }
}

// The same main database, project and worktree always name the same
// database: a restart that picked a new name would build an empty one.
#[test]
fn the_names_are_the_same_every_time_and_only_the_hashed_one_knows_the_project() {
    assert_eq!(names("shop", "feat+x"), names("shop", "feat+x"));
    let here = database_names("shop", "acme-0000beef", "feat+x").unwrap();
    let clone = database_names("shop", "acme-1111cafe", "feat+x").unwrap();
    assert_eq!(
        here[0], clone[0],
        "the readable name is the worktree's alone"
    );
    assert_ne!(here[1], clone[1], "the hashed one tells two clones apart");
}

// `feat+x` and `feat-x` read the same; the second name is what keeps them
// from sharing one database.
#[test]
fn two_worktrees_that_read_the_same_have_different_second_names() {
    let plus = names("shop", "feat+x");
    let dash = names("shop", "feat-x");
    assert_eq!(plus[0], dash[0]);
    assert_ne!(plus[1], dash[1]);
}

#[test]
fn a_name_past_the_limit_is_cut_and_hashed_so_long_names_still_differ() {
    let long = format!("feat+{}", "x".repeat(120));
    let [readable, hashed] = names("northwind_traders", &long);
    assert_eq!(readable.len(), MAX_NAME, "{readable}");
    assert_eq!(readable, hashed, "cut means hashed, both ways");
    let other = names("northwind_traders", &format!("{long}y"));
    assert_ne!(
        readable, other[0],
        "the hash is of the whole name, not the cut"
    );
    // Exactly at the limit is not past it.
    let fits = "y".repeat(MAX_NAME - "shop__".len());
    assert_eq!(names("shop", &fits)[0], format!("shop__{fits}"));
    let over = format!("{fits}y");
    assert_ne!(names("shop", &over)[0], format!("shop__{over}"));
}

#[test]
fn a_worktree_with_nothing_readable_in_its_name_is_named_by_its_hash() {
    for worktree in ["+++", "日本語", "_", ""] {
        let [readable, hashed] = names("shop", worktree);
        assert_eq!(readable, hashed, "{worktree:?}");
        assert_eq!(readable.len(), "shop__".len() + 8, "{readable}");
    }
}

#[test]
fn a_main_name_that_is_not_plain_or_leaves_no_room_is_refused() {
    for main in ["", "shop;drop", "sh`op", "a.b", "shöp", "shop db"] {
        let e = format!("{:#}", database_names(main, "p", "feat+x").unwrap_err());
        assert!(e.contains("not a plain name"), "{main:?}: {e}");
    }
    // `<main>__` plus `_`, eight hex digits and one character of the
    // worktree is the least a name can be.
    let longest = "m".repeat(MAX_NAME - MARKER.len() - 1 - 8 - 1);
    assert!(database_names(&longest, "p", "feat+x").is_ok());
    let e = format!(
        "{:#}",
        database_names(&format!("{longest}m"), "p", "feat+x").unwrap_err()
    );
    assert!(e.contains("too long"), "{e}");
}

/// A deterministic spread of awkward worktree names, so the invariants
/// below are asked of more than the examples someone thought of.
fn awkward_worktrees() -> Vec<String> {
    let pieces = [
        "feat", "+", "-", "_", "/", "Ü", "x", "LONG", "9", ".", " ", "__", "日",
    ];
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = vec![String::new(), "a".repeat(300)];
    for _ in 0..400 {
        let mut name = String::new();
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let len = (seed >> 58) as usize;
        for _ in 0..len {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            name.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
        }
        out.push(name);
    }
    out
}

// The whole safety argument for the names, over every awkward name above
// and a spread of main names: always inside the prefix, never the main
// database, never too long, always a plain identifier — and the second
// name never equal to the first unless both are hashed.
#[test]
fn every_name_is_inside_the_prefix_never_main_and_always_fits() {
    let mains = [
        "a".to_string(),
        "shop".into(),
        "northwind_traders".into(),
        "Shop_DB".into(),
        "shop__".into(),
        "m".repeat(MAX_NAME - MARKER.len() - 1 - 8 - 1),
    ];
    for main in &mains {
        let prefix = format!("{main}{MARKER}");
        for worktree in awkward_worktrees() {
            for name in names(main, &worktree) {
                assert!(name.starts_with(&prefix), "{main} {worktree:?}: {name}");
                assert!(name.len() > prefix.len(), "{main} {worktree:?}: {name}");
                assert!(!name.eq_ignore_ascii_case(main), "{main} {worktree:?}");
                assert!(name.len() <= MAX_NAME, "{main} {worktree:?}: {name}");
                assert!(is_plain(&name), "{main} {worktree:?}: {name}");
            }
        }
    }
}

// ---- the guard --------------------------------------------------------------

fn database(name: &str, main: &str) -> NamespaceRecord {
    NamespaceRecord {
        service: "mariadb".into(),
        recipe: "mariadb".into(),
        kind: NamespaceKind::Database,
        host: "localhost".into(),
        port: 3306,
        name: name.into(),
        main: main.into(),
        used_at: Utc::now(),
    }
}

fn slot(n: &str, main: &str) -> NamespaceRecord {
    NamespaceRecord {
        service: "redis".into(),
        recipe: "redis".into(),
        kind: NamespaceKind::Slot,
        host: "127.0.0.1".into(),
        port: 6379,
        name: n.into(),
        main: main.into(),
        used_at: Utc::now(),
    }
}

/// A state with each worktree holding the namespaces given.
fn state(worktrees: &[(&str, Vec<NamespaceRecord>)]) -> State {
    let mut state = State::new();
    for (name, namespaces) in worktrees {
        let mut record = WorktreeRecord::new(format!("/abs/{name}"), true);
        record.namespaces = namespaces.clone();
        state.worktrees.insert(name.to_string(), record);
    }
    state
}

fn refused(state: &State, worktree: &str, ns: &NamespaceRecord, main_now: Option<&str>) -> String {
    format!(
        "{:#}",
        may_drop(state, worktree, ns, main_now).expect_err("the guard let it through")
    )
}

#[test]
fn a_database_pando_made_for_this_worktree_may_be_dropped() {
    let ns = database("northwind_traders__feat_x", "northwind_traders");
    let st = state(&[("feat+x", vec![ns.clone()])]);
    may_drop(&st, "feat+x", &ns, Some("northwind_traders")).unwrap();
    may_drop(&st, "feat+x", &ns, None).unwrap();
}

#[test]
fn a_namespace_state_does_not_record_for_this_worktree_is_never_dropped() {
    let ns = database("northwind_traders__feat_x", "northwind_traders");
    let e = refused(&state(&[("feat+x", vec![])]), "feat+x", &ns, None);
    assert!(e.contains("not recorded"), "{e}");
    // Recorded for somebody else is not recorded for this one.
    let st = state(&[("feat+y", vec![ns.clone()]), ("feat+x", vec![])]);
    let e = refused(&st, "feat+x", &ns, None);
    assert!(e.contains("not recorded"), "{e}");
    // Nor is a worktree state has never heard of.
    let e = refused(&state(&[]), "feat+x", &ns, None);
    assert!(e.contains("not recorded"), "{e}");
    // A record that differs in any field is not the one state holds.
    let mut moved = ns.clone();
    moved.port = 3307;
    let e = refused(&state(&[("feat+x", vec![ns])]), "feat+x", &moved, None);
    assert!(e.contains("not recorded"), "{e}");
}

// The one that matters most. Whatever state says, the main checkout's own
// database is never dropped — not by the name recorded beside it, and not
// by the name its env files give now, in any case.
#[test]
fn the_main_database_is_refused_whatever_state_says() {
    for (name, main, main_now) in [
        ("northwind_traders", "northwind_traders", None),
        ("NORTHWIND_TRADERS", "northwind_traders", None),
        (
            "northwind_traders",
            "northwind_traders",
            Some("northwind_traders"),
        ),
        ("shop__feat_x", "shop", Some("shop__feat_x")),
        ("shop__feat_x", "shop", Some("SHOP__FEAT_X")),
    ] {
        let ns = database(name, main);
        let st = state(&[("feat+x", vec![ns.clone()])]);
        let e = refused(&st, "feat+x", &ns, main_now);
        assert!(e.contains("main checkout's own database"), "{name}: {e}");
    }
}

#[test]
fn a_database_without_the_marker_after_the_main_name_is_refused() {
    for name in [
        "northwind_traders_feat_x",
        "other__feat_x",
        "northwind_traders__",
        "xnorthwind_traders__feat_x",
        "feat_x__northwind_traders",
    ] {
        let ns = database(name, "northwind_traders");
        let st = state(&[("feat+x", vec![ns.clone()])]);
        let e = refused(&st, "feat+x", &ns, None);
        assert!(e.contains("does not start with"), "{name}: {e}");
    }
    // In another case it is still the marker: MariaDB on macOS agrees.
    let ns = database("NORTHWIND_TRADERS__feat_x", "northwind_traders");
    let st = state(&[("feat+x", vec![ns.clone()])]);
    may_drop(&st, "feat+x", &ns, None).unwrap();
}

#[test]
fn a_database_name_a_statement_could_be_made_to_say_something_else_with_is_refused() {
    for name in [
        "shop__x`; DROP DATABASE shop; --",
        "shop__x'",
        "shop__x y",
        "shop__x.y",
        "shop__ünï",
    ] {
        let ns = database(name, "shop");
        let st = state(&[("feat+x", vec![ns.clone()])]);
        let e = refused(&st, "feat+x", &ns, None);
        assert!(e.contains("not a plain name"), "{name}: {e}");
    }
    let long = format!("shop__{}", "x".repeat(MAX_NAME));
    let ns = database(&long, "shop");
    let e = refused(&state(&[("feat+x", vec![ns.clone()])]), "feat+x", &ns, None);
    assert!(e.contains("at most 64"), "{e}");
}

#[test]
fn a_namespace_two_worktrees_claim_is_dropped_by_neither() {
    let ns = database("shop__feat_x", "shop");
    let mut elsewhere = ns.clone();
    // `localhost` and `127.0.0.1` are one server, and case is no
    // difference on MariaDB's side.
    elsewhere.host = "127.0.0.1".into();
    elsewhere.name = "SHOP__FEAT_X".into();
    let st = state(&[("feat+x", vec![ns.clone()]), ("feat-x", vec![elsewhere])]);
    let e = refused(&st, "feat+x", &ns, None);
    assert!(e.contains("recorded for feat-x as well"), "{e}");

    // The same name on another server is another database.
    let mut other_server = ns.clone();
    other_server.port = 3307;
    let st = state(&[("feat+x", vec![ns.clone()]), ("feat-x", vec![other_server])]);
    may_drop(&st, "feat+x", &ns, None).unwrap();
}

#[test]
fn a_slot_pando_allocated_may_be_emptied_and_zero_and_mains_never() {
    let ns = slot("3", "0");
    let st = state(&[("feat+x", vec![ns.clone()])]);
    may_drop(&st, "feat+x", &ns, Some("0")).unwrap();

    for (n, main, main_now, says) in [
        ("0", "0", None, "slot 0"),
        ("0", "5", None, "slot 0"),
        ("3", "3", None, "main checkout's own slot"),
        ("3", "0", Some("3"), "main checkout's own slot"),
        ("3", "0", Some(" 3 "), "main checkout's own slot"),
        ("x", "0", None, "not a slot number"),
        ("-1", "0", None, "not a slot number"),
        ("3; FLUSHALL", "0", None, "not a slot number"),
    ] {
        let ns = slot(n, main);
        let st = state(&[("feat+x", vec![ns.clone()])]);
        let e = refused(&st, "feat+x", &ns, main_now);
        assert!(e.contains(says), "{n} (main {main}, now {main_now:?}): {e}");
    }
}

#[test]
fn a_slot_another_worktree_holds_on_the_same_server_is_never_emptied() {
    let ns = slot("3", "0");
    let st = state(&[
        ("feat+x", vec![ns.clone()]),
        ("feat+y", vec![slot("3", "0")]),
    ]);
    let e = refused(&st, "feat+x", &ns, None);
    assert!(e.contains("feat+y"), "{e}");

    let mut other_server = slot("3", "0");
    other_server.port = 6380;
    let st = state(&[("feat+x", vec![ns.clone()]), ("feat+y", vec![other_server])]);
    may_drop(&st, "feat+x", &ns, None).unwrap();
}

#[test]
fn a_database_and_a_slot_of_the_same_name_are_not_the_same_namespace() {
    let mut as_slot = slot("3", "0");
    as_slot.port = 3306;
    as_slot.host = "localhost".into();
    let as_database = database("3", "0");
    assert!(!same_namespace(&as_slot, &as_database));
}

// The names and the guard, held to each other: every name pando would give
// a worktree is one the guard lets it drop, and the main database — in
// any case, recorded under any worktree — is one it never does.
#[test]
fn every_name_pando_gives_passes_the_guard_and_the_main_database_never_does() {
    for main in ["shop", "northwind_traders", "Shop_DB"] {
        for worktree in awkward_worktrees() {
            for name in names(main, &worktree) {
                let ns = database(&name, main);
                let st = state(&[("w", vec![ns.clone()])]);
                may_drop(&st, "w", &ns, Some(main))
                    .unwrap_or_else(|e| panic!("{main} {worktree:?}: {name} refused: {e:#}"));
            }
        }
        for spelled in [main.to_string(), main.to_uppercase(), main.to_lowercase()] {
            let ns = database(&spelled, main);
            let st = state(&[("w", vec![ns.clone()])]);
            assert!(may_drop(&st, "w", &ns, Some(main)).is_err(), "{spelled}");
            assert!(may_drop(&st, "w", &ns, None).is_err(), "{spelled}");
        }
    }
}

// ---- the login ----------------------------------------------------------------

fn main_checkout(env: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), env).unwrap();
    dir
}

fn keys(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

#[test]
fn a_login_is_read_from_the_keys_beside_the_services_address() {
    let root = main_checkout(
        "DATABASE_HOST=localhost\nDATABASE_PORT=3306\nDATABASE_NAME=shop\n\
         DATABASE_USER=shop_user\nDATABASE_PASSWORD=s3cr3t:w0rd\n",
    );
    let login = login_from_env_files(root.path(), &keys(&["DATABASE_PORT"])).unwrap();
    assert_eq!(login.user.as_deref(), Some("shop_user"));
    assert_eq!(
        login.env(Some("MYSQL_PWD")),
        vec![("MYSQL_PWD".to_string(), "s3cr3t:w0rd".to_string())]
    );
    assert!(
        login.from.contains("DATABASE_USER and DATABASE_PASSWORD"),
        "{}",
        login.from
    );
    // The other spellings an app uses.
    let root = main_checkout("DB_PORT=3306\nDB_USERNAME=u\nDB_PASS=p\n");
    let login = login_from_env_files(root.path(), &keys(&["DB_PORT"])).unwrap();
    assert_eq!(login.user.as_deref(), Some("u"));
    assert_eq!(
        login.env(Some("X")),
        vec![("X".to_string(), "p".to_string())]
    );
}

#[test]
fn a_login_in_a_url_is_read_with_its_escapes_undone() {
    let root =
        main_checkout("DATABASE_URL=mysql://shop%40corp:p%40ss%3Aw0rd@localhost:3306/shop\n");
    let login = login_from_env_files(root.path(), &keys(&["DATABASE_URL"])).unwrap();
    assert_eq!(login.user.as_deref(), Some("shop@corp"));
    assert_eq!(
        login.env(Some("MYSQL_PWD")),
        vec![("MYSQL_PWD".to_string(), "p@ss:w0rd".to_string())]
    );
    assert!(login.from.contains("DATABASE_URL"), "{}", login.from);
    // A password alone, the way a development Redis is protected.
    let root = main_checkout("REDIS_URL=redis://:only-a-password@localhost:6379/0\n");
    let login = login_from_env_files(root.path(), &keys(&["REDIS_URL"])).unwrap();
    assert_eq!(login.user, None);
    assert!(login.has_password());
}

#[test]
fn a_url_with_no_login_in_it_and_no_keys_beside_it_is_no_login() {
    let root = main_checkout("DATABASE_URL=mysql://localhost:3306/shop\nREDIS_PORT=6379\n");
    assert_eq!(
        login_from_env_files(root.path(), &keys(&["DATABASE_URL"])),
        None
    );
    assert_eq!(
        login_from_env_files(root.path(), &keys(&["REDIS_PORT"])),
        None
    );
    assert_eq!(login_from_env_files(root.path(), &keys(&[])), None);
    // Empty values say nothing either.
    let root = main_checkout("DATABASE_PORT=3306\nDATABASE_USER=\nDATABASE_PASSWORD=\n");
    assert_eq!(
        login_from_env_files(root.path(), &keys(&["DATABASE_PORT"])),
        None
    );
}

#[test]
fn the_login_written_for_pando_is_used_when_the_env_files_have_none_that_will_do() {
    let file = std::path::Path::new("/home/.pando/projects/p/pando.toml");
    let mut config = crate::config::Config::default();
    config.namespaced.insert(
        "mariadb".into(),
        crate::config::LoginConfig {
            user: Some("root".into()),
            password: Some("hunter2".into()),
        },
    );
    // Nothing in the env files: the one written down.
    let root = main_checkout("DATABASE_PORT=3306\n");
    let login = find_login(
        root.path(),
        &config,
        "mariadb",
        &keys(&["DATABASE_PORT"]),
        true,
        file,
    )
    .unwrap();
    assert_eq!(login.user.as_deref(), Some("root"));
    assert!(
        login.from.contains("[namespaced.mariadb]"),
        "{}",
        login.from
    );
    // A password with no user will not do for an engine that logs in as
    // somebody, so the one written down wins over it…
    let root = main_checkout("DATABASE_PORT=3306\nDATABASE_PASSWORD=x\n");
    let login = find_login(
        root.path(),
        &config,
        "mariadb",
        &keys(&["DATABASE_PORT"]),
        true,
        file,
    )
    .unwrap();
    assert_eq!(login.user.as_deref(), Some("root"));
    // …and does for one that does not.
    let login = find_login(
        root.path(),
        &config,
        "redis",
        &keys(&["DATABASE_PORT"]),
        false,
        file,
    )
    .unwrap();
    assert_eq!(login.user, None);
    // The main checkout's own login, when it has one, beats pando's.
    let root = main_checkout("DATABASE_PORT=3306\nDATABASE_USER=app\n");
    let login = find_login(
        root.path(),
        &config,
        "mariadb",
        &keys(&["DATABASE_PORT"]),
        true,
        file,
    )
    .unwrap();
    assert_eq!(login.user.as_deref(), Some("app"));
    // Nothing anywhere is nothing.
    let root = main_checkout("DATABASE_PORT=3306\n");
    let none = crate::config::Config::default();
    assert!(
        find_login(
            root.path(),
            &none,
            "mariadb",
            &keys(&["DATABASE_PORT"]),
            true,
            file
        )
        .is_none()
    );
}

// A config and a login both end up in error messages and `{:?}`s; neither
// may carry the password there.
#[test]
fn a_password_is_never_in_what_a_login_or_its_config_prints() {
    let login = Login::new(Some("app".into()), Some("hunter2".into()), "somewhere");
    let shown = format!("{login:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("hidden") && shown.contains("app"), "{shown}");
    let config = crate::config::LoginConfig {
        user: Some("app".into()),
        password: Some("hunter2".into()),
    };
    let shown = format!("{config:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    // And the only way out is the environment of the command it is for.
    assert_eq!(login.env(None), Vec::new());
    assert_eq!(Login::none().env(Some("MYSQL_PWD")), Vec::new());
}
