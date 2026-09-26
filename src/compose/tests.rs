use super::*;
use std::path::{Path, PathBuf};

const FIXTURE_ONE: &str = r#"
services:
  postgres:
    image: postgres:16
    ports: ["5432:5432"]
  redis:
    image: redis:7
    ports: ["6379:6379"]
  mailpit:
    image: axllent/mailpit
    ports: ["1025:1025"]
"#;

#[test]
fn a_flow_ports_list_parses_into_published_and_container() {
    let file = parse(FIXTURE_ONE).unwrap();
    assert_eq!(
        file.services.keys().collect::<Vec<_>>(),
        vec!["mailpit", "postgres", "redis"]
    );
    let pg = &file.services["postgres"];
    assert_eq!(pg.image.as_deref(), Some("postgres:16"));
    assert_eq!(
        pg.ports,
        vec![Port {
            container: 5432,
            published: Some(5432),
            host: None
        }]
    );
    assert_eq!(pg.container_port(), Some(5432));
    assert!(!pg.healthcheck);
}

#[test]
fn every_short_port_form_is_understood() {
    let text = r#"
services:
  a:
    ports:
      - "3000"
      - "8000:8000"
      - "127.0.0.1:8001:8002"
      - "9090-9091:8080-8081"
      - "6060:6060/udp"
      - "[::1]:70:71"
"#;
    let ports = &parse(text).unwrap().services["a"].ports;
    assert_eq!(
        ports,
        &vec![
            Port {
                container: 3000,
                published: None,
                host: None
            },
            Port {
                container: 8000,
                published: Some(8000),
                host: None
            },
            Port {
                container: 8002,
                published: Some(8001),
                host: Some("127.0.0.1".into())
            },
            Port {
                container: 8080,
                published: Some(9090),
                host: None
            },
            Port {
                container: 6060,
                published: Some(6060),
                host: None
            },
            Port {
                container: 71,
                published: Some(70),
                host: Some("[::1]".into())
            },
        ]
    );
}

#[test]
fn the_long_port_form_is_understood() {
    let text = r#"
services:
  db:
    image: postgres:16
    ports:
      - name: sql
        target: 5432
        published: "15432"
        protocol: tcp
      - target: 5433
"#;
    let ports = &parse(text).unwrap().services["db"].ports;
    assert_eq!(
        ports,
        &vec![
            Port {
                container: 5432,
                published: Some(15432),
                host: None
            },
            Port {
                container: 5433,
                published: None,
                host: None
            },
        ]
    );
}

#[test]
fn named_and_bind_volumes_are_told_apart() {
    let text = r#"
services:
  db:
    image: postgres:16
    volumes:
      - dbdata:/var/lib/postgresql/data
      - ./seed:/docker-entrypoint-initdb.d
      - /var/log
      - type: bind
        source: ./conf
        target: /etc/conf
      - type: volume
        source: extra
        target: /extra
volumes:
  dbdata:
  shared:
    name: literally-this
  borrowed:
    external: true
"#;
    let file = parse(text).unwrap();
    assert_eq!(
        file.services["db"].volumes,
        vec![
            Mount::Named("dbdata".into()),
            Mount::Bind("./seed".into()),
            Mount::Anonymous,
            Mount::Bind("./conf".into()),
            Mount::Named("extra".into()),
        ]
    );
    assert_eq!(file.volumes["dbdata"], TopVolume::default());
    assert_eq!(
        file.volumes["shared"].name.as_deref(),
        Some("literally-this")
    );
    assert!(file.volumes["borrowed"].external);
    assert!(!file.volumes["dbdata"].external);
}

#[test]
fn container_name_healthcheck_and_depends_on_are_read() {
    let text = r#"
services:
  web:
    image: nginx
    container_name: my-web
    depends_on:
      - db
      - cache
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost"]
      interval: 10s
  api:
    image: node
    depends_on:
      db:
        condition: service_healthy
"#;
    let file = parse(text).unwrap();
    let web = &file.services["web"];
    assert_eq!(web.container_name.as_deref(), Some("my-web"));
    assert!(web.healthcheck);
    assert_eq!(web.depends_on, vec!["db", "cache"]);
    assert!(!file.services["api"].healthcheck);
    assert_eq!(file.services["api"].depends_on, vec!["db"]);
}

// Docker reports no health at all for a check the file turns off, so a
// readiness wait for `healthy` on one of these ran out its whole timeout
// while the service was serving.
#[test]
fn a_healthcheck_the_file_turns_off_is_not_one_readiness_waits_for() {
    let text = r#"
services:
  disabled:
    image: redis:7
    healthcheck:
      disable: true
  none:
    image: redis:7
    healthcheck:
      test: ["NONE"]
  tuned:
    image: redis:7
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      disable: false
"#;
    let file = parse(text).unwrap();
    assert!(!file.services["disabled"].healthcheck);
    assert!(!file.services["none"].healthcheck);
    assert!(file.services["tuned"].healthcheck);

    // Compose keeps both opt-outs as written.
    let file = parse_config_json(
        r#"{"name": "p", "services": {
            "disabled": {"image": "redis:7", "healthcheck": {"disable": true}},
            "none": {"image": "redis:7", "healthcheck": {"test": ["NONE"]}},
            "shell": {"image": "redis:7", "healthcheck": {"test": ["CMD-SHELL", "NONE"]}}
        }}"#,
    )
    .unwrap();
    assert!(!file.services["disabled"].healthcheck);
    assert!(!file.services["none"].healthcheck);
    assert!(
        file.services["shell"].healthcheck,
        "`test: NONE` as a string is a shell command, not an opt-out"
    );
}

#[test]
fn a_service_with_no_ports_falls_back_to_the_image_table() {
    let text = "services:\n  cache:\n    image: redis:7-alpine\n  odd:\n    image: acme/thing\n";
    let file = parse(text).unwrap();
    assert_eq!(file.services["cache"].container_port(), Some(6379));
    assert_eq!(
        file.services["odd"].container_port(),
        None,
        "an unknown image with no declared port cannot be published"
    );
}

#[test]
fn an_image_is_matched_by_its_last_segment_without_its_tag() {
    assert_eq!(image_ports("postgres:16"), Some(&[5432u16][..]));
    assert_eq!(
        image_ports("public.ecr.aws/docker/library/postgres:16-alpine"),
        Some(&[5432u16][..])
    );
    assert_eq!(image_ports("axllent/mailpit"), Some(&[8025u16, 1025][..]));
    assert_eq!(image_ports("redis@sha256:abc"), Some(&[6379u16][..]));
    assert_eq!(image_ports("nginx"), None);
}

// The two older spellings, and the `version:` key compose now ignores.
#[test]
fn a_legacy_file_with_a_version_key_still_parses() {
    let text = "version: \"3.8\"\nservices:\n  db:\n    image: mysql:8\n";
    let file = parse(text).unwrap();
    assert_eq!(file.services["db"].container_port(), Some(3306));
}

#[test]
fn comments_and_blank_lines_are_ignored_but_not_inside_quotes() {
    let text =
        "# top\nservices:\n\n  db:\n    image: \"redis#7\"  # trailing\n    ports: [\"1:2\"]\n";
    let file = parse(text).unwrap();
    assert_eq!(file.services["db"].image.as_deref(), Some("redis#7"));
    assert_eq!(file.services["db"].ports.len(), 1);
}

#[test]
fn a_flow_mapping_is_understood() {
    let text = "services:\n  db:\n    image: postgres\n    environment: { POSTGRES_PASSWORD: x }\n    ports: [ \"5432:5432\" ]\n";
    let file = parse(text).unwrap();
    assert_eq!(file.services["db"].ports.len(), 1);
}

#[test]
fn a_file_with_no_services_is_not_an_error() {
    assert_eq!(parse("").unwrap(), ComposeFile::default());
    assert_eq!(parse("volumes:\n  a:\n").unwrap().services.len(), 0);
}

#[test]
fn find_prefers_composes_own_order() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(find(dir.path()), None);
    std::fs::write(dir.path().join("docker-compose.yml"), "services:\n").unwrap();
    assert_eq!(find(dir.path()).as_deref(), Some("docker-compose.yml"));
    std::fs::write(dir.path().join("compose.yaml"), "services:\n").unwrap();
    assert_eq!(find(dir.path()).as_deref(), Some("compose.yaml"));
}

#[test]
fn a_compose_path_may_not_leave_the_worktree() {
    let worktree = Path::new("/tmp/wt");
    assert_eq!(
        file_in(worktree, "docker/compose.yml").unwrap(),
        PathBuf::from("/tmp/wt/docker/compose.yml")
    );
    for bad in ["/etc/compose.yml", "../compose.yml", "a/../../b.yml", "  "] {
        let err = file_in(worktree, bad).unwrap_err().to_string();
        assert!(
            err.contains("compose") || err.contains("file"),
            "{bad:?}: {err}"
        );
    }
}

// ---- the override ----------------------------------------------------

#[test]
fn a_project_name_is_readable_when_nothing_had_to_be_folded() {
    assert_eq!(
        project_name("acme-shop-3f9a2c1d", "main2"),
        "pando-acme-shop-3f9a2c1d-main2"
    );
}

#[test]
fn a_folded_project_name_carries_a_hash_so_two_branches_cannot_collide() {
    let plus = project_name("acme-3f9a2c1d", "feat+one");
    let dash = project_name("acme-3f9a2c1d", "feat-one");
    assert!(plus.starts_with("pando-acme-3f9a2c1d-feat-one-"), "{plus}");
    assert_ne!(
        plus, dash,
        "two worktrees must never share one set of containers and volumes"
    );
    // Every character compose allows, and nothing else.
    assert!(
        plus.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
        "{plus}"
    );
    assert_eq!(plus, project_name("acme-3f9a2c1d", "feat+one"), "stable");
}

#[test]
fn an_uppercase_branch_name_does_not_fold_onto_its_lowercase_twin() {
    assert_ne!(
        project_name("p-1", "Feat+One"),
        project_name("p-1", "feat+one")
    );
}

#[test]
fn the_override_replaces_ports_and_clears_the_container_name() {
    let rendered = render_override(
        "feat+one",
        &[
            Published {
                service: "redis".into(),
                container: 6379,
                host: 17_006,
            },
            Published {
                service: "postgres".into(),
                container: 5432,
                host: 17_004,
            },
        ],
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>(),
        vec![
            "services:",
            "  postgres:",
            "    ports: !override [\"127.0.0.1:17004:5432\"]",
            "    container_name: !reset",
            "  redis:",
            "    ports: !override [\"127.0.0.1:17006:6379\"]",
            "    container_name: !reset",
        ]
    );
    assert!(rendered.starts_with("# generated by pando for the worktree feat+one"));
}

#[test]
fn an_override_with_nothing_included_is_still_a_valid_file() {
    assert!(render_override("feat+one", &[]).ends_with("services: {}\n"));
}

#[test]
fn included_services_resolve_to_their_container_ports() {
    let file = parse(FIXTURE_ONE).unwrap();
    assert_eq!(
        resolve_included(&file, &["postgres".into(), "redis".into()], &[]).unwrap(),
        vec![("postgres".to_string(), 5432), ("redis".to_string(), 6379)]
    );
}

#[test]
fn a_service_the_compose_file_does_not_declare_is_refused_by_name() {
    let file = parse(FIXTURE_ONE).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["pgsql".into()], &[]).unwrap_err()
    );
    assert!(err.contains("pgsql"), "{err}");
    assert!(
        err.contains("postgres, redis"),
        "it lists what is there: {err}"
    );
}

#[test]
fn a_relative_bind_mount_is_refused_by_name() {
    let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - ./data:/var/lib/postgresql/data\n";
    let file = parse(text).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[]).unwrap_err()
    );
    assert!(err.contains("\"db\""), "{err}");
    assert!(err.contains("./data"), "{err}");
    assert!(
        err.contains("named volume"),
        "it says what to change: {err}"
    );
}

/// A compose file with one service and one bind mount of `source`.
fn with_bind(source: &str) -> ComposeFile {
    parse(&format!(
        "services:\n  db:\n    image: postgres:16\n    volumes:\n      - {source}:/data\n"
    ))
    .unwrap()
}

#[test]
fn an_absolute_bind_mount_inside_the_repository_is_refused_naming_the_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("acme-shop");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let source = root.join("data").join("pg");
    let file = with_bind(&source.display().to_string());
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], std::slice::from_ref(&root)).unwrap_err()
    );
    assert!(err.contains("\"db\""), "{err}");
    assert!(err.contains(&source.display().to_string()), "{err}");
    assert!(
        err.contains("named volume"),
        "it says what to change: {err}"
    );
}

#[test]
fn an_absolute_bind_mount_inside_a_worktree_is_refused_the_same_way() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("acme-shop");
    let worktree = dir.path().join("worktrees").join("feat+one");
    std::fs::create_dir_all(worktree.join("pgdata")).unwrap();
    let source = worktree.join("pgdata");
    let file = with_bind(&source.display().to_string());
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[root, worktree]).unwrap_err()
    );
    assert!(err.contains(&source.display().to_string()), "{err}");
    assert!(err.contains("named volume"), "{err}");
}

/// The path does not have to exist yet, and it may reach the same
/// directory through a symlinked ancestor — `/var` and `/private/var`
/// are one directory on macOS. Both are what `resolve_for_compare`
/// exists for, and a bind mount gets the same treatment as a
/// configured home.
#[test]
fn a_bind_mount_is_compared_after_the_same_canonicalisation_paths_uses() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("acme-shop");
    std::fs::create_dir_all(&root).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    // Through the canonical form, into a directory nothing created.
    let source = canonical.join("var").join("pg");
    let file = with_bind(&source.display().to_string());
    assert!(
        resolve_included(&file, &["db".into()], &[root]).is_err(),
        "a path under the repository is inside it however it is spelled"
    );
}

#[test]
fn a_home_relative_bind_mount_is_expanded_before_it_is_judged() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let file = with_bind("~/acme-data");
    // With the home directory itself standing in for the repository,
    // `~/acme-data` is inside it — which only a `~` that was expanded
    // can be.
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[home]).unwrap_err()
    );
    assert!(
        err.contains("~/acme-data"),
        "it names what the file says: {err}"
    );
    assert!(err.contains("named volume"), "{err}");
}

#[test]
fn an_absolute_bind_mount_outside_the_repository_is_refused_as_one_every_worktree_shares() {
    let file = with_bind("/var/lib/acme/pgdata");
    let err = format!(
        "{:#}",
        resolve_included(
            &file,
            &["db".into()],
            &[PathBuf::from("/nowhere/acme-shop")]
        )
        .unwrap_err()
    );
    assert!(err.contains("/var/lib/acme/pgdata"), "{err}");
    assert!(
        err.contains("every worktree"),
        "it says why it matters: {err}"
    );
}

#[test]
fn a_bind_mount_pando_cannot_place_is_refused_rather_than_guessed_at() {
    let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - $PWD/data:/data\n";
    let file = parse(text).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[]).unwrap_err()
    );
    assert!(err.contains("cannot tell"), "{err}");
}

#[test]
fn a_volume_the_project_name_does_not_isolate_is_refused_by_name() {
    for (tail, needle) in [
        ("volumes:\n  dbdata:\n    external: true\n", "external"),
        ("volumes:\n  dbdata:\n    name: shared-db\n", "shared-db"),
    ] {
        let text = format!(
            "services:\n  db:\n    image: postgres:16\n    volumes:\n      - dbdata:/var/lib/postgresql/data\n{tail}"
        );
        let file = parse(&text).unwrap();
        let err = format!(
            "{:#}",
            resolve_included(&file, &["db".into()], &[]).unwrap_err()
        );
        assert!(err.contains("dbdata"), "{err}");
        assert!(err.contains(needle), "{err}");
        assert!(err.contains("share"), "it says why it matters: {err}");
    }
}

#[test]
fn a_plain_named_volume_is_isolated_by_the_project_name_and_allowed() {
    let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - dbdata:/var/lib/postgresql/data\nvolumes:\n  dbdata:\n";
    let file = parse(text).unwrap();
    assert_eq!(
        resolve_included(&file, &["db".into()], &[]).unwrap(),
        vec![("db".to_string(), 5432)]
    );
}

#[test]
fn a_dependency_left_out_of_include_is_refused_naming_both() {
    let text = "services:\n  api:\n    image: node\n    ports: [\"3000:3000\"]\n    depends_on:\n      - db\n  db:\n    image: postgres:16\n";
    let file = parse(text).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["api".into()], &[]).unwrap_err()
    );
    assert!(err.contains("\"api\""), "{err}");
    assert!(err.contains("\"db\""), "{err}");
    // And it is fine once both are in.
    assert!(resolve_included(&file, &["api".into(), "db".into()], &[]).is_ok());
}

#[test]
fn an_unknown_image_with_no_declared_port_is_refused_by_name() {
    let text = "services:\n  odd:\n    image: acme/thing:1\n";
    let file = parse(text).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["odd".into()], &[]).unwrap_err()
    );
    assert!(err.contains("odd"), "{err}");
    assert!(err.contains("acme/thing:1"), "it names the image: {err}");
    assert!(err.contains("ports:"), "it says what to add: {err}");
}

const EXTENDING: &str = "include:\n  - extra.yml\n\
         services:\n  \
         db:\n    extends:\n      file: base.yml\n      service: template\n  \
         cache:\n    image: redis:7\n    ports: [\"6379:6379\"]\n";

#[test]
fn extends_and_a_top_level_include_are_recorded_rather_than_ignored() {
    let file = parse(EXTENDING).unwrap();
    assert_eq!(file.unresolved.extends, vec!["db".to_string()]);
    assert!(file.unresolved.include);
    assert!(file.unresolved.any());
    // The service pando *can* read is unaffected.
    assert_eq!(file.services["cache"].container_port(), Some(6379));
}

// "add a `ports:` entry to it in the compose file" is what a developer
// whose file already has one was told, in a place they cannot usefully
// add it. The refusal has to say which key pando did not follow.
#[test]
fn a_refusal_about_an_unfollowed_file_says_which_key_it_did_not_follow() {
    let file = parse(EXTENDING).unwrap();
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[]).unwrap_err()
    );
    assert!(err.contains("extends"), "{err}");
    assert!(err.contains("docker compose config"), "{err}");

    // And a service only the `include:` declares is not flatly "not
    // there", which is untrue of the file compose reads.
    let err = format!(
        "{:#}",
        resolve_included(&file, &["fromInclude".into()], &[]).unwrap_err()
    );
    assert!(err.contains("fromInclude"), "{err}");
    assert!(err.contains("include"), "{err}");
}

/// A merge key carrying a bind mount into the worktree, which is the shape
/// a file that shares one block between its services arrives in.
const MERGING: &str = "x-pg: &pg\n  volumes:\n    - ./pgdata:/var/lib/postgresql/data\n\
         services:\n  postgres:\n    <<: *pg\n    image: postgres:16\n";

// Compose expands `<<: *pg` when it runs `up`, so the bind mount is there
// whether or not this reader saw it. Approving the service on what was
// read would put a database cluster inside the worktree.
#[test]
fn a_bind_mount_brought_in_by_a_merge_key_is_never_approved_on_half_a_file() {
    let file = parse(MERGING).unwrap();
    assert!(file.unresolved.aliases);
    assert!(file.unresolved.any(), "so compose is asked to resolve it");
    assert!(
        file.unresolved.describe().unwrap().contains("`<<:`"),
        "{:?}",
        file.unresolved.describe()
    );
    let err = format!(
        "{:#}",
        resolve_included(&file, &["postgres".into()], &[]).unwrap_err()
    );
    assert!(err.contains("\"postgres\""), "{err}");
    assert!(err.contains("docker compose config"), "{err}");
}

// Compose follows `extends:` when it runs `up`, so a bind mount the
// extended service declares is there whether or not this reader saw it.
// Approved on what was read, `up -d db` wrote a database cluster into the
// worktree whenever `docker compose config` could not answer.
#[test]
fn a_service_that_extends_another_is_never_approved_on_half_a_file() {
    let file = parse(
        "services:\n  db:\n    extends:\n      file: base.yml\n      service: pg\n    \
         image: postgres:16\n    ports: [\"5432:5432\"]\n  \
         cache:\n    image: redis:7\n",
    )
    .unwrap();
    assert_eq!(
        file.services["db"].container_port(),
        Some(5432),
        "everything this reader saw of db would pass"
    );
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[]).unwrap_err()
    );
    assert!(err.contains("\"db\""), "{err}");
    assert!(err.contains("`extends:`"), "{err}");
    assert!(err.contains("docker compose config"), "{err}");
    assert_eq!(
        resolve_included(&file, &["cache".into()], &[]).unwrap(),
        vec![("cache".to_string(), 6379)],
        "a service written out whole is read whole"
    );
}

#[test]
fn an_alias_is_recorded_rather_than_read_as_an_anonymous_volume() {
    let file = parse(
        "x-data: &data\n  - /srv/pg:/var/lib/postgresql/data\n\
         services:\n  db:\n    image: postgres:16\n    volumes: *data\n",
    )
    .unwrap();
    assert!(file.unresolved.aliases);
    assert_eq!(
        file.services["db"].volumes,
        Vec::new(),
        "`*data` is not a mount with no source"
    );
    assert!(resolve_included(&file, &["db".into()], &[]).is_err());
}

// An anchor is only a label. Reading `volumes: &v` as the text `&v` lost
// the list under it, and the bind mount in it with the list.
#[test]
fn an_anchor_is_only_a_label_and_what_it_marks_is_read() {
    let file = parse(
        "services:\n  db:\n    image: &img postgres:16\n    volumes: &v\n      \
         - ./data:/var/lib/postgresql/data\n    ports: [&p \"5432:5432\"]\n",
    )
    .unwrap();
    assert!(!file.unresolved.any(), "nothing here refers elsewhere");
    let db = &file.services["db"];
    assert_eq!(db.image.as_deref(), Some("postgres:16"));
    assert_eq!(db.volumes, vec![Mount::Bind("./data".to_string())]);
    assert_eq!(db.container_port(), Some(5432));
    let err = format!(
        "{:#}",
        resolve_included(&file, &["db".into()], &[]).unwrap_err()
    );
    assert!(
        err.contains("./data"),
        "refused for the mount it has: {err}"
    );
}

// Only what pando reads counts: an alias inside an `x-` block nothing here
// uses changes no port and no mount, and a quoted `*` is a string.
#[test]
fn an_alias_outside_services_and_a_quoted_star_leave_the_file_whole() {
    let file = parse(
        "x-one: &one {a: b}\nx-two: *one\n\
         services:\n  cache:\n    image: redis:7\n    command: [\"redis-server\", \"*\"]\n",
    )
    .unwrap();
    assert!(!file.unresolved.any(), "{:?}", file.unresolved);
    assert_eq!(
        resolve_included(&file, &["cache".into()], &[]).unwrap(),
        vec![("cache".to_string(), 6379)]
    );
}

#[test]
fn every_key_not_followed_is_named_in_one_sentence() {
    let unresolved = Unresolved {
        extends: vec!["db".to_string()],
        include: true,
        aliases: true,
    };
    assert_eq!(
        unresolved.describe().unwrap(),
        "`extends:` (on db), a top-level `include:` and YAML aliases or merge keys (`*`, `<<:`)"
    );
}

/// Captured verbatim from `docker compose config --format json` on
/// Compose 5.0.1, against a file using `extends` and a top-level
/// `include:`. Every shape here is one the hand-rolled parser does not
/// produce: `published` a string, `depends_on` a map, and a `name` on
/// every volume.
const COMPOSE_CONFIG_JSON: &str = r#"{
      "name": "pando-probe-cfg",
      "services": {
        "db": {
          "depends_on": { "fromInclude": { "condition": "service_started", "required": true } },
          "environment": { "POSTGRES_PASSWORD": "x" },
          "healthcheck": { "test": ["CMD", "true"] },
          "image": "postgres:16",
          "ports": [ { "mode": "ingress", "target": 5432, "published": "5432", "protocol": "tcp" } ],
          "volumes": [ { "type": "volume", "source": "pgdata", "target": "/var/lib/postgresql/data", "volume": {} } ]
        },
        "fromInclude": {
          "image": "redis:7",
          "ports": [ { "mode": "ingress", "target": 6379, "published": "6379", "protocol": "tcp" } ]
        }
      },
      "volumes": { "pgdata": { "name": "pando-probe-cfg_pgdata" } }
    }"#;

#[test]
fn what_compose_itself_says_reads_into_the_same_shape() {
    let file = parse_config_json(COMPOSE_CONFIG_JSON).unwrap();
    assert_eq!(
        file.services.keys().collect::<Vec<_>>(),
        vec!["db", "fromInclude"],
        "the service the `include:` brought in is there too"
    );
    let db = &file.services["db"];
    assert_eq!(db.image.as_deref(), Some("postgres:16"));
    assert_eq!(db.container_port(), Some(5432));
    assert_eq!(db.ports[0].published, Some(5432), "published is a string");
    assert!(db.healthcheck, "so readiness goes through health");
    assert_eq!(db.depends_on, vec!["fromInclude".to_string()]);
    assert_eq!(db.volumes, vec![Mount::Named("pgdata".to_string())]);
    assert_eq!(file.services["fromInclude"].container_port(), Some(6379));
    assert!(
        !file.unresolved.any(),
        "compose resolved it, so nothing is left unfollowed"
    );

    // `<project>_<key>` is the prefix compose applies itself, which is
    // what isolates the data — not a name the project pinned.
    assert_eq!(file.volumes["pgdata"].name, None);
    assert!(!file.volumes["pgdata"].external);
    assert_eq!(
        resolve_included(&file, &["db".into(), "fromInclude".into()], &[]).unwrap(),
        vec![("db".to_string(), 5432), ("fromInclude".to_string(), 6379)]
    );
}

// A service compose builds is the thing being developed, not something
// it depends on. Every spelling of `build:` has to be readable, because
// the one that is missed is the one offered as a dependency.
#[test]
fn every_spelling_of_a_build_context_is_read() {
    let file = parse(
        r#"
services:
  web:
    build: .
  api:
    build:
      context: ./services/api
      dockerfile: Dockerfile.dev
  jobs:
    build:
      dockerfile: Dockerfile
  shared:
    build: ../shared
  tagged:
    image: acme/web:dev
    build: ./web
  postgres:
    image: postgres:16
"#,
    )
    .unwrap();
    assert_eq!(file.services["web"].build.as_deref(), Some("."));
    assert_eq!(
        file.services["api"].build.as_deref(),
        Some("./services/api")
    );
    assert_eq!(
        file.services["jobs"].build.as_deref(),
        Some("."),
        "a build with only a dockerfile still has compose's default context"
    );
    assert_eq!(file.services["shared"].build.as_deref(), Some("../shared"));
    assert_eq!(
        file.services["tagged"].build.as_deref(),
        Some("./web"),
        "an image beside a build is the tag to write, not a dependency to pull"
    );
    assert_eq!(file.services["postgres"].build, None);
}

// Compose resolves the context against the file's own directory and
// hands back an absolute path, so the reader has to take it as given.
#[test]
fn compose_reports_a_build_context_already_resolved() {
    let json = r#"{
          "name": "pando-probe-build",
          "services": {
            "web": { "build": { "context": "/Users/me/code/acme", "dockerfile": "Dockerfile" } },
            "postgres": { "image": "postgres:16" }
          }
        }"#;
    let file = parse_config_json(json).unwrap();
    assert_eq!(
        file.services["web"].build.as_deref(),
        Some("/Users/me/code/acme")
    );
    assert_eq!(file.services["postgres"].build, None);
}

#[test]
fn a_pinned_or_external_volume_survives_composes_normalisation() {
    let json = r#"{
          "name": "pando-probe-vol",
          "services": { "a": {
            "image": "postgres:16",
            "container_name": "fixed-name",
            "ports": [ { "target": 5432, "published": "5432" } ],
            "volumes": [ { "type": "volume", "source": "pinned", "target": "/p", "volume": {} } ]
          } },
          "volumes": {
            "plain": { "name": "pando-probe-vol_plain" },
            "pinned": { "name": "literally-this" },
            "shared": { "name": "shared", "external": true }
          }
        }"#;
    let file = parse_config_json(json).unwrap();
    assert_eq!(file.volumes["plain"].name, None);
    assert_eq!(
        file.volumes["pinned"].name.as_deref(),
        Some("literally-this")
    );
    assert!(file.volumes["shared"].external);
    assert_eq!(
        file.services["a"].container_name.as_deref(),
        Some("fixed-name")
    );
    let err = format!(
        "{:#}",
        resolve_included(&file, &["a".into()], &[]).unwrap_err()
    );
    assert!(err.contains("literally-this"), "{err}");
}

#[test]
fn every_fixture_compose_file_parses() {
    // The shapes the fixture catalogue uses, so a parser change that
    // breaks one is caught here and not three phases later.
    let messy = "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n  \
                     cache:\n    image: redis:7\n    ports: [\"6379:6379\"]\n  \
                     queue:\n    image: rabbitmq:3\n    ports: [\"5672:5672\"]\n  \
                     mail:\n    image: axllent/mailpit\n    ports: [\"1025:1025\"]\n";
    let file = parse(messy).unwrap();
    assert_eq!(file.services.len(), 4);
    assert_eq!(file.services["queue"].container_port(), Some(5672));
    assert_eq!(file.services["mail"].container_port(), Some(1025));
}
