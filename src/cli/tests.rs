use super::answers::answer_from;
use super::answers::slot_named;
use super::answers::slot_names;
use super::logs::leading_timestamp;
use super::logs::silence_notes;
use super::ls::ORDER;
use super::prompt::asker;
use super::prompt::prompt_with;
use super::status::human_duration;
use super::*;
use crate::actions;
use crate::actions::worktree_url;
use crate::cache;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::project::ProjectRef;
use crate::state::{Phase, ProcessRecord, WorktreeRecord};
use crate::testutil::git;
use crate::worktree::PrState;
use anyhow::Result;
use chrono::Utc;
use clap::CommandFactory;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tempfile::{TempDir, tempdir};

struct Fx {
    _dir: TempDir,
    root: PathBuf,
    paths: PandoPaths,
    config: Config,
}

fn fixture() -> Fx {
    let dir = tempdir().unwrap();
    let root = dir.path().join("acme-shop");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "root"]);
    let project = ProjectRef::from_root(&root).unwrap();
    let paths = PandoPaths::new(dir.path().join("pando-home"), project);
    Fx {
        root: paths.root().to_path_buf(),
        paths,
        config: Config::default(),
        _dir: dir,
    }
}

fn capture(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
    let mut buf = Vec::new();
    f(&mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

/// The notice channel, for a test that is not about it.
fn quiet(_: &str) {}

/// stdout and the notices, separately — which is the whole point of
/// there being two channels.
fn capture_both(
    f: impl FnOnce(&mut Vec<u8>, &dyn Fn(&str)) -> Result<()>,
) -> (String, Vec<String>) {
    let notes = std::cell::RefCell::new(Vec::new());
    let mut buf = Vec::new();
    f(&mut buf, &|line: &str| {
        notes.borrow_mut().push(line.to_string())
    })
    .unwrap();
    (String::from_utf8(buf).unwrap(), notes.into_inner())
}

#[test]
fn the_cli_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn help_documents_the_needs_answer_exit_code() {
    let help = Cli::command().render_help().to_string();
    assert!(help.contains("Exit codes"), "{help}");
    assert!(
        help.contains("3  needs an answer"),
        "an agent has to be able to tell a question from a failure: {help}"
    );
}

// ---- ls columns ------------------------------------------------------

fn widths(pairs: &[(Col, usize)]) -> BTreeMap<Col, usize> {
    pairs.iter().copied().collect()
}

#[test]
fn a_wide_terminal_keeps_every_column() {
    let w = widths(&[
        (Col::Name, 8),
        (Col::Branch, 8),
        (Col::Head, 7),
        (Col::State, 7),
        (Col::Ports, 5),
        (Col::Status, 8),
        (Col::Path, 40),
    ]);
    assert_eq!(keep_columns(200, &w), ORDER.to_vec());
}

// A tmux split is the normal case, so the listing has to survive one:
// what a worktree is doing outlives what git thinks of it.
#[test]
fn a_narrow_terminal_sheds_columns_and_keeps_the_name() {
    let w = widths(&[
        (Col::Name, 10),
        (Col::Branch, 10),
        (Col::Head, 7),
        (Col::State, 7),
        (Col::Ports, 5),
        (Col::Status, 8),
        (Col::Path, 60),
    ]);
    let mid = keep_columns(60, &w);
    assert!(mid.contains(&Col::Name) && mid.contains(&Col::Status));
    assert!(
        !mid.contains(&Col::Path),
        "the path is the first thing to go after the sha"
    );
    let tight = keep_columns(20, &w);
    assert_eq!(tight, vec![Col::Name, Col::Status]);
    let sliver = keep_columns(4, &w);
    assert_eq!(sliver, vec![Col::Name], "the name is never dropped");
}

#[test]
fn ls_shows_the_ports_and_status_of_a_running_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.processes.insert(
        "dev".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: std::process::id() as i32,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "dev"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(text.contains("PORTS") && text.contains("STATUS"), "{text}");
    assert!(text.contains("17342"), "{text}");
    assert!(text.contains("running"), "{text}");

    let narrow = capture(|b| ls_text_at(&fx.paths, b, 24));
    assert!(narrow.contains("feat+one"), "{narrow}");
    assert!(
        !narrow.contains("PATH"),
        "a narrow listing sheds the path: {narrow}"
    );
}

#[test]
fn a_worktree_with_nothing_running_shows_dashes() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(text.contains("feat+one"), "{text}");
    assert!(text.contains(" -"), "{text}");
}

/// A process record for a live group with the sockets it was seen
/// holding.
fn listening(pgid: i32, observed: &[u16]) -> crate::state::ProcessRecord {
    crate::state::ProcessRecord {
        pid: std::process::id(),
        pgid,
        started_at: Utc::now(),
        log_path: PathBuf::from("/does/not/exist/dev.log"),
        ready_port: None,
        ready_timeout_s: None,
        observed_ports: observed.to_vec(),
        swept: false,
        phase: Phase::Running { since: Utc::now() },
    }
}

/// A worktree shaped like a one-process start: `dev` owning `web`.
fn one_process_record(observed: &[u16]) -> WorktreeRecord {
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_342);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    record
        .processes
        .insert("dev".to_string(), listening(101, observed));
    record.observed_ports = observed.to_vec();
    record
}

/// A worktree shaped like a two-process start: `web` and `api`, each
/// owning its own role and running in its own group.
fn two_process_record(web: &[u16], api: &[u16]) -> WorktreeRecord {
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_342);
    record.ports.insert("api".to_string(), 17_343);
    record
        .roles
        .insert("web".to_string(), vec!["web".to_string()]);
    record
        .roles
        .insert("api".to_string(), vec!["api".to_string()]);
    record
        .processes
        .insert("web".to_string(), listening(101, web));
    record
        .processes
        .insert("api".to_string(), listening(102, api));
    let mut union: Vec<u16> = web.iter().chain(api).copied().collect();
    union.sort_unstable();
    union.dedup();
    record.observed_ports = union;
    record
}

// The documented behaviour — "what it is really listening on when that
// is known" — was two identical match arms, so a framework that ignored
// `PORT` and bound something else still had the assigned port printed
// as its URL.
#[test]
fn the_url_prefers_a_port_the_process_is_really_listening_on() {
    assert_eq!(
        worktree_url(&one_process_record(&[17_342, 17_399])).as_deref(),
        Some("http://localhost:17342"),
        "the assigned port is among them, so it is the one"
    );
    assert_eq!(
        worktree_url(&one_process_record(&[3_000])).as_deref(),
        Some("http://localhost:3000"),
        "it ignored the port pando gave it; the URL follows the process"
    );
    assert_eq!(
        worktree_url(&one_process_record(&[])).as_deref(),
        Some("http://localhost:17342"),
        "nothing observed at all falls back to what was assigned"
    );
    // And nothing running at all: the port survives the stop, so the
    // URL the developer bookmarked is still the one they get.
    let mut record = one_process_record(&[3_000]);
    record.processes.clear();
    record.observed_ports.clear();
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:17342")
    );
}

// Phase 2b review, finding 6. One rule with three implementations:
// `start` took the first role of the first process, `status` and `ls`
// took the alphabetically first *role*, and the TUI took that and never
// looked at what was really listening. Two processes whose role names
// sort the other way round from their own names were all it took for
// `pando start` and `pando status`, seconds apart, to hand out two
// different URLs.
#[test]
fn the_url_is_the_first_role_of_the_first_process_when_nothing_owns_web() {
    let mut record = WorktreeRecord::new("/trees/feat+url2", true);
    record.ports.insert("srv".to_string(), 19_056);
    record.ports.insert("admin".to_string(), 19_057);
    record
        .roles
        .insert("alpha".to_string(), vec!["srv".to_string()]);
    record
        .roles
        .insert("beta".to_string(), vec!["admin".to_string()]);
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:19056"),
        "alpha comes first, so alpha's first role is the worktree's URL"
    );

    // `web` still wins wherever anything owns it, whatever it sorts
    // against.
    record.ports.insert("web".to_string(), 19_058);
    record
        .roles
        .insert("zeta".to_string(), vec!["web".to_string()]);
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:19058")
    );
}

// Phase 2b review, finding 4. `f6093df` narrowed "prefer an observed
// port" to "prefer one no role claims", but the observed list was one
// flat set per worktree, so a socket the *api* opened — an HMR socket,
// `node --inspect`, a metrics port — was indistinguishable from one the
// web process opened, and became the worktree's URL while the web
// server was not serving at all.
#[test]
fn the_url_follows_a_listener_only_in_the_group_that_owns_the_role() {
    assert_eq!(
        worktree_url(&two_process_record(&[17_342], &[17_343])).as_deref(),
        Some("http://localhost:17342"),
        "both up, each on its own port"
    );

    assert_eq!(
        worktree_url(&two_process_record(&[17_342], &[9876, 17_343])).as_deref(),
        Some("http://localhost:17342"),
        "the api's second socket is the api's, whatever claims it"
    );

    // The review's reproduction: the web process stopped, the api kept
    // serving, and it holds a port no role claims.
    let mut record = two_process_record(&[], &[9876, 17_343]);
    record.processes.remove("web");
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:17342"),
        "the web role's own port, not whatever the api happens to hold"
    );

    // And a framework that ignored `PORT` is still followed, because
    // there it is the process that owns the role doing the ignoring.
    assert_eq!(
        worktree_url(&two_process_record(&[3000], &[17_343])).as_deref(),
        Some("http://localhost:3000"),
        "the web process itself bound 3000"
    );

    // A port another role already has is never a candidate either.
    assert_eq!(
        worktree_url(&two_process_record(&[17_343], &[17_343])).as_deref(),
        Some("http://localhost:17342")
    );
}

// ---- questions -------------------------------------------------------

fn dev_question(options: &[&str]) -> actions::Question {
    actions::Question {
        slot: crate::detect::Slot::DevCmd,
        prompt: "Which command starts the local development server?".to_string(),
        options: options
            .iter()
            .map(|v| (v.to_string(), "a signal".to_string()))
            .collect(),
        preselect: (!options.is_empty()).then_some(0),
        allow_custom: true,
        allow_none: false,
        multi: false,
        checked: Vec::new(),
        details: Vec::new(),
    }
}

/// The services question of fixture 6: two ticked by a rule, two the
/// rules could not place.
fn services_question() -> actions::Question {
    actions::Question {
        slot: crate::detect::Slot::Services,
        prompt: "Run private copies of these services for each worktree?".to_string(),
        options: ["cache", "db", "mail", "queue"]
            .iter()
            .map(|v| (v.to_string(), "docker-compose.yml".to_string()))
            .collect(),
        preselect: Some(0),
        allow_custom: false,
        allow_none: true,
        multi: true,
        checked: vec![1, 2],
        details: Vec::new(),
    }
}

/// The prompt driven by a script of typed lines, as a terminal would.
fn answer_with(question: &actions::Question, lines: &[&str]) -> (Result<actions::Answer>, String) {
    let mut typed = lines.iter().map(|l| format!("{l}\n"));
    let mut out = Vec::new();
    let answer = prompt_with(question, &mut out, || Ok(typed.next()));
    (answer, String::from_utf8(out).unwrap())
}

// ---- the multi-select question ---------------------------------------

#[test]
fn a_set_question_starts_from_what_the_rules_resolved() {
    let question = services_question();
    let (answer, printed) = answer_with(&question, &[""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
    assert!(printed.contains("[ ] 1) cache"), "{printed}");
    assert!(printed.contains("[x] 2) db"), "{printed}");
    assert!(printed.contains("[x] 3) mail"), "{printed}");
    assert!(printed.contains("[ ] 4) queue"), "{printed}");
    assert!(printed.contains("accepts [db, mail]"), "{printed}");
}

#[test]
fn a_number_toggles_one_option_and_enter_takes_the_rest() {
    let question = services_question();
    // Tick `cache`, untick `mail`, accept.
    let (answer, _) = answer_with(&question, &["1", "3", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![0, 1]));
}

#[test]
fn unticking_everything_is_the_answer_none() {
    let question = services_question();
    let (answer, _) = answer_with(&question, &["2", "3", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::None);
    // And so is saying so outright.
    let (answer, _) = answer_with(&services_question(), &["n"]);
    assert_eq!(answer.unwrap(), actions::Answer::None);
}

#[test]
fn a_number_out_of_range_reprints_the_range_and_ticks_nothing() {
    let question = services_question();
    let (answer, printed) = answer_with(&question, &["9", "not-a-number", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
    assert_eq!(
        printed
            .matches("a number between 1 and 4 toggles one")
            .count(),
        2,
        "{printed}"
    );
}

// `Auto`, not `Many`: the resolver turns `Auto` into the ticked set
// *and* into a comment saying a flag took it. A `Many` would be
// written down as if a human had chosen, which is a config nobody can
// review.
#[test]
fn yes_takes_the_ticked_set_as_an_auto_answer_not_a_choice() {
    let question = services_question();
    assert_eq!(asker(true)(&question).unwrap(), actions::Answer::Auto(0));
}

#[test]
fn exit_three_shows_a_set_question_with_its_boxes() {
    let needs = actions::NeedsAnswer {
        question: services_question(),
    };
    let text = render_needs_answer(&needs);
    assert!(text.contains("[ ] 1) cache"), "{text}");
    assert!(text.contains("[x] 2) db"), "{text}");
    assert!(text.contains("[[services]]"), "{text}");
    assert!(
        text.contains("--yes to take the ticked ones"),
        "an agent has to be told what --yes would do: {text}"
    );
}

// A fat-fingered number used to fall through to "it must be a command",
// so `5` on a four-option question became `cmd = "5"`, dated as if a
// human had meant it, and `start` reported success over a shell error.
#[test]
fn a_number_at_the_prompt_is_always_a_choice() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let (answer, printed) = answer_with(&question, &["5", "0", "99", "2"]);
    assert_eq!(answer.unwrap(), actions::Answer::Choice(1));
    assert_eq!(
        printed.matches("pick a number between 1 and 2").count(),
        3,
        "every out-of-range number reprints the range: {printed}"
    );
    assert!(
        !printed.contains("command > "),
        "and none of them is a command: {printed}"
    );
}

// The way a command that is only digits is still reachable.
#[test]
fn a_number_typed_after_c_is_taken_as_the_command() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let (answer, _) = answer_with(&question, &["c", "5"]);
    assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
}

// Unchanged: anything that is not a number is the command itself, so
// nobody has to discover that `c` exists first.
#[test]
fn a_line_that_is_not_a_number_is_still_the_command() {
    let question = dev_question(&["pnpm dev"]);
    let (answer, _) = answer_with(&question, &["./my-own-server"]);
    assert_eq!(
        answer.unwrap(),
        actions::Answer::Custom("./my-own-server".to_string())
    );
}

// With nothing on offer, a number cannot be a choice at all.
#[test]
fn a_question_with_no_options_takes_a_number_as_the_command() {
    let question = dev_question(&[]);
    let (answer, _) = answer_with(&question, &["5"]);
    assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
}

// `--yes` takes the first option; it cannot take one that is not there.
#[test]
fn a_question_with_nothing_to_offer_does_not_point_at_yes() {
    let needs = actions::NeedsAnswer {
        question: dev_question(&[]),
    };
    let text = render_needs_answer(&needs);
    assert!(
        !text.contains("--yes"),
        "there is nothing for --yes to take: {text}"
    );
    assert!(text.contains("pando.toml"), "{text}");
}

#[test]
fn a_question_with_options_still_points_at_yes() {
    let needs = actions::NeedsAnswer {
        question: dev_question(&["pnpm dev", "pnpm dev:web"]),
    };
    assert!(render_needs_answer(&needs).contains("--yes"));
}

// ---- status ----------------------------------------------------------

#[test]
fn status_json_carries_the_documented_shape() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.observed_ports = vec![17_342, 17_399];
    // Alive, so the read path leaves it Running, with a process group
    // that no longer exists — so a scan that runs and finds nothing is
    // an answer, and the ports it is really listening on are none. The
    // last good answer survives only a scan that could not run at all.
    record.processes.insert(
        "dev".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_998,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "dev"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    record.processes.insert(
        "worker".to_string(),
        crate::state::ProcessRecord {
            pid: 4242,
            pgid: 4242,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "worker"),
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Failed {
                at: Utc::now(),
                reason: "process exited".to_string(),
            },
        },
    );
    record.hooks.insert(
        "install".to_string(),
        crate::state::HookRecord {
            fingerprint: Some("md5:abc".to_string()),
            ran_at: Utc::now(),
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Pinned as a literal on purpose: a bump must fail here, at the
    // commit that makes it, rather than passing quietly.
    assert_eq!(v["version"], 2);
    assert_eq!(v["project"]["name"], "acme-shop");
    let wt = &v["worktrees"][0];
    assert_eq!(wt["name"], "feat+one");
    assert_eq!(wt["branch"], "feat/one");
    assert_eq!(wt["ports"]["web"], 17_342);
    assert_eq!(wt["observed_ports"], serde_json::json!([]));
    assert_eq!(wt["url"], "http://localhost:17342");
    let dev = &wt["processes"]["dev"];
    assert_eq!(dev["pid"], std::process::id());
    assert_eq!(dev["phase"], "running");
    assert_eq!(dev["reason"], serde_json::Value::Null);
    assert!(dev["since"].is_string());
    assert!(dev["log"].as_str().unwrap().ends_with("dev.log"));
    let worker = &wt["processes"]["worker"];
    assert_eq!(worker["phase"], "failed");
    assert_eq!(worker["reason"], "process exited");
    assert_eq!(wt["hooks"]["install"]["fingerprint"], "md5:abc");
}

/// A worktree with a live share recorded, so the status shapes can be
/// asserted without a tunnel. Every pid is this process: alive, so the
/// refresh leaves the record alone — the application it publishes
/// included, because a share whose application is gone is closed.
fn with_share(fx: &Fx, name: &str, proxy: Option<u16>) {
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    record
        .processes
        .insert("dev".to_string(), listening(std::process::id() as i32, &[]));
    record.share_port = proxy;
    record.share = Some(crate::state::ShareRecord {
        tunnel_pid: std::process::id(),
        tunnel_pgid: 999_998,
        public_url: "https://fake-host.trycloudflare.com".to_string(),
        local_port: 17_342,
        started_at: Utc::now(),
        log_path: fx.paths.log_file(name, "tunnel"),
        proxy_pid: proxy.map(|_| std::process::id()),
        proxy_pgid: proxy.map(|_| 999_997),
        proxy_port: proxy,
    });
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
}

#[test]
fn status_json_carries_the_public_url_and_never_a_cookie() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let share = &v["worktrees"][0]["share"];
    assert_eq!(share["url"], "https://fake-host.trycloudflare.com");
    assert_eq!(share["local_port"], 17_342);
    assert_eq!(share["proxy_port"], 17_349);
    assert!(share["since"].is_string());
    assert!(
        !text.to_lowercase().contains("cookie"),
        "a credential must never reach a shape that gets piped into things:\n{text}"
    );
}

#[test]
fn status_json_says_null_for_a_worktree_that_is_not_shared() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"][0]["share"], serde_json::Value::Null);
}

#[test]
fn status_json_reports_a_share_with_no_proxy_in_front_of_it() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, None);
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["worktrees"][0]["share"]["proxy_port"],
        serde_json::Value::Null
    );
}

#[test]
fn status_text_prints_the_public_url_under_its_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_text_at(&fx.paths, None, b, 120));
    assert!(text.contains("share"), "{text}");
    assert!(
        text.contains("https://fake-host.trycloudflare.com"),
        "{text}"
    );
    assert!(
        text.contains("through a proxy on 17349"),
        "the proxy is worth saying: a visitor arrives authenticated: {text}"
    );
}

// The same degradation every other row has: truncated, never wrapped.
#[test]
fn the_share_row_truncates_on_a_narrow_terminal() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_text_at(&fx.paths, None, b, 40));
    for line in text.lines() {
        assert!(
            line.chars().count() <= 40,
            "a row wider than the terminal: {line:?}"
        );
    }
}

#[test]
fn status_json_reports_a_worktree_that_was_never_started() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let wt = &v["worktrees"][0];
    assert!(wt["processes"].as_object().unwrap().is_empty());
    assert_eq!(wt["url"], serde_json::Value::Null);
    assert!(wt["observed_ports"].as_array().unwrap().is_empty());
}

#[test]
fn status_can_be_asked_about_one_worktree() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    actions::new(&fx.paths, &fx.config, "feat/two", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, Some("feat+two"), b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"].as_array().unwrap().len(), 1);
    assert_eq!(v["worktrees"][0]["name"], "feat+two");

    let text = capture(|b| status_text(&fx.paths, Some("nope"), b));
    assert!(text.contains("no worktree named"), "{text}");
}

#[test]
fn status_text_names_what_each_worktree_is_doing() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_text(&fx.paths, None, b));
    assert!(text.contains("feat+one"), "{text}");
    assert!(text.contains("stopped"), "{text}");
}

/// A worktree running `web` and `api`, recorded as a refresh would
/// leave it. `pid` is this test process, which really is alive, so the
/// read path does not turn the phase into a failure underneath.
fn with_two_processes(fx: &Fx, name: &str, api_phase: Phase) {
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.ports.insert("api".to_string(), 17_343);
    record.observed_ports = vec![17_342, 17_343];
    record.processes.insert(
        "web".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_998,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(name, "web"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    record.processes.insert(
        "api".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_997,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(name, "api"),
            ready_port: Some(17_343),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: api_phase,
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
}

// `the_url_follows_a_listener_only_when_no_other_role_owns_that_port`
// lived here. It asserted that a port no role claims becomes the
// worktree's URL, which is the bug finding 4 reproduces: that port
// belongs to whichever group opened it.
// `the_url_follows_a_listener_only_in_the_group_that_owns_the_role`
// above is the rule it should have pinned.

#[test]
fn status_text_lists_every_process_under_its_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

    let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "one worktree line and two process lines:\n{text}"
    );
    assert!(lines[0].starts_with("feat+one"), "{text}");
    assert!(lines[0].contains("running"), "{text}");
    assert!(
        lines[0].contains("api 17343") && lines[0].contains("web 17342"),
        "the worktree line carries every role: {text}"
    );
    assert!(
        lines[0].contains("http://localhost:17342"),
        "and one URL, the web role's: {text}"
    );
    // Indented, in config order, each with its own pid.
    assert!(lines[1].starts_with("  api"), "{text}");
    assert!(lines[2].starts_with("  web"), "{text}");
    for line in &lines[1..] {
        assert!(
            line.contains(&format!("pid {}", std::process::id())),
            "{text}"
        );
        assert!(line.contains("running"), "{text}");
    }
}

// Phase 2b review, finding 9. `ls` sheds columns and the TUI detail
// pane truncates; `status` had no width parameter at all, and the
// per-process rows 2b added grow the block it prints. In the tmux split
// the TUI is designed for, it wrapped.
#[test]
fn status_text_sheds_the_url_and_then_the_ports_as_the_terminal_narrows() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

    let wide = capture(|b| status_text_at(&fx.paths, None, b, 200));
    assert!(wide.contains("http://localhost:17342"), "{wide}");
    assert!(
        wide.contains("api 17343") && wide.contains("web 17342"),
        "{wide}"
    );

    for width in [60, 44, 32, 24, 12] {
        let text = capture(|b| status_text_at(&fx.paths, None, b, width));
        for line in text.lines() {
            assert!(
                line.chars().count() <= width,
                "{line:?} is wider than {width} columns:\n{text}"
            );
        }
        assert!(
            text.contains("feat+one"),
            "the name is the identifier and never goes: {text}"
        );
    }

    // The URL is the longest cell and `--json` still carries it, so it
    // is the first thing to go; the ports cell is truncated after that.
    let narrow = capture(|b| status_text_at(&fx.paths, None, b, 44));
    assert!(!narrow.contains("http://"), "{narrow}");
    assert!(narrow.contains("running"), "{narrow}");
}

#[test]
fn status_text_says_which_process_failed() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(
        &fx,
        &name,
        Phase::Failed {
            at: Utc::now(),
            reason: "process exited".to_string(),
        },
    );

    let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].contains("failed") && lines[0].contains("api: process exited"),
        "a worktree with a dead api is failed, and says which: {text}"
    );
    assert!(
        lines[1].contains("api") && lines[1].contains("failed"),
        "{text}"
    );
    assert!(
        lines[2].contains("web") && lines[2].contains("running"),
        "the process that is still up says so: {text}"
    );
}

#[test]
fn the_listing_shows_the_aggregate_not_the_first_process() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    // `api` sorts first and is running; `web` is the failed one, so a
    // row that showed the first process would read "running".
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store
        .worktrees
        .get_mut(&name)
        .unwrap()
        .processes
        .get_mut("web")
        .unwrap()
        .phase = Phase::Failed {
        at: Utc::now(),
        reason: "process exited".to_string(),
    };
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(
        text.contains("failed"),
        "the row is the worst of its processes: {text}"
    );
}

#[test]
fn logs_read_the_source_they_are_asked_for() {
    let fx = fixture();
    write_log(&fx, "feat+one", "web", "web line\n");
    write_log(&fx, "feat+one", "api", "api line\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "api", 5, false, false, b, &quiet));
    assert_eq!(text, "api line\n");

    let mut out = Vec::new();
    let err = logs(
        &fx.paths, "feat+one", "worker", 5, false, false, &mut out, &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("worker"), "{msg}");
    assert!(
        msg.contains("api") && msg.contains("web"),
        "an unknown source lists the ones there are: {msg}"
    );
}

#[test]
fn uptime_reads_in_the_unit_that_fits() {
    use chrono::TimeDelta;
    assert_eq!(human_duration(TimeDelta::seconds(9)), "9s");
    assert_eq!(human_duration(TimeDelta::seconds(70)), "1m10s");
    assert_eq!(human_duration(TimeDelta::seconds(3_700)), "1h1m");
    assert_eq!(human_duration(TimeDelta::seconds(90_000)), "1d1h");
    assert_eq!(human_duration(TimeDelta::seconds(-5)), "0s");
}

// ---- logs ------------------------------------------------------------

fn write_log(fx: &Fx, name: &str, source: &str, text: &str) {
    let path = fx.paths.log_file(name, source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// The read side of finding 1: `--source` is the other half of the same
// path component, so a traversal there reads a file outside the
// worktree's log directory — one `available_sources` never lists, so
// nothing even suggests it is reachable.
#[test]
fn a_log_source_that_escapes_the_log_directory_is_refused() {
    let fx = fixture();
    write_log(&fx, "feat+one", "web", "web line\n");
    // A real file the traversal would reach, so the refusal is about
    // the name rather than about the file not being there.
    std::fs::create_dir_all(fx.paths.project_dir()).unwrap();
    std::fs::write(fx.paths.project_dir().join("outside.log"), "secret\n").unwrap();

    let mut out = Vec::new();
    let err = logs(
        &fx.paths,
        "feat+one",
        "../../outside",
        5,
        false,
        false,
        &mut out,
        &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("\"../../outside\""), "{msg}");
    assert!(msg.contains("logs/<worktree>"), "{msg}");
    assert!(
        out.is_empty(),
        "nothing outside the log directory may be printed: {}",
        String::from_utf8_lossy(&out)
    );

    // And the hook logs pando writes itself are still readable by name.
    write_log(&fx, "feat+one", "install", "install line\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "install", 5, false, false, b, &quiet));
    assert_eq!(text, "install line\n");
}

#[test]
fn logs_prints_the_last_lines() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "one\ntwo\nthree\nfour\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 2, false, false, b, &quiet));
    assert_eq!(text, "three\nfour\n");
}

/// The third dead end of the first contact run, and the worst of them:
/// `doctor` said the process failed and pointed at `pando logs`; that
/// printed nothing and exited 0. Following pando's own advice led to
/// silence, with no way to tell an empty log from a wrong worktree
/// name, a wrong `--source`, or a broken command.
#[test]
fn an_empty_log_says_that_it_is_empty() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert_eq!(text, "", "stdout is still only the log");
    assert!(
        notes.iter().any(|n| n.contains("is empty")),
        "an empty log is a fact pando knows: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("dev") && n.contains("feat+one")),
        "and it names what was read: {notes:?}"
    );
}

/// A process killed mid-line leaves its last words without a newline.
/// The failure classifier reads them — `snapshot` flushes the pending
/// line — and `pando logs` used to withhold them and print nothing, so
/// pando knew more about the crash than the developer could see.
#[test]
fn a_last_line_with_no_newline_is_still_printed() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "done\nSegmentation fault");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert_eq!(text, "done\nSegmentation fault\n");
    assert!(
        notes.is_empty(),
        "there was something to print, so nothing to explain: {notes:?}"
    );
}

/// Which leaves one state a one-shot read cannot reach and a follower
/// can: `-f` on a file holding an unterminated first line, where the
/// rest of it really is still coming.
#[test]
fn a_log_with_no_complete_line_is_described_by_its_size() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "half a line with no newline");
    let notes = silence_notes(
        &fx.paths,
        "feat+one",
        "dev",
        &fx.paths.log_file("feat+one", "dev"),
    );
    assert!(
        notes.iter().any(|n| n.contains("27 bytes")),
        "the size is the whole difference from an empty file: {notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("is empty")),
        "and it is not empty: {notes:?}"
    );
}

/// The sentence the developer came for: why the log is empty. The
/// record already knows, because `explain_failure` wrote it there.
#[test]
fn an_empty_log_carries_the_reason_the_record_knows() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let mut store = crate::state::State::new();
    let mut record = WorktreeRecord::new(fx.paths.worktree_path("feat+one"), true);
    record.processes.insert(
        "dev".to_string(),
        ProcessRecord {
            pid: 1,
            pgid: 1,
            started_at: Utc::now(),
            log_path: fx.paths.log_file("feat+one", "dev"),
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Failed {
                at: Utc::now(),
                reason: "process exited with status 0 — it printed nothing at all".to_string(),
            },
        },
    );
    store.worktrees.insert("feat+one".to_string(), record);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let (_, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert!(
        notes.iter().any(|n| n.contains("status 0")),
        "the reason is already written down; this is where it is wanted: {notes:?}"
    );
}

/// `--json` is a stream of objects, one per line. A note about the log
/// is not one of them, so it goes to the other channel and stdout
/// stays parseable — empty is a valid answer there.
#[test]
fn an_empty_log_in_json_keeps_stdout_clean() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, true, b, n));
    assert_eq!(text, "", "nothing that is not a log line may be on stdout");
    assert!(
        !notes.is_empty(),
        "and the developer is still told: {notes:?}"
    );
}

#[test]
fn logs_json_emits_one_object_per_line() {
    let fx = fixture();
    write_log(
        &fx,
        "feat+one",
        "dev",
        "2026-09-20T10:00:00Z ready in 412ms\nError: it broke\n",
    );
    let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 10, false, true, b, &quiet));
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ts"], "2026-09-20T10:00:00+00:00");
    assert_eq!(lines[0]["level"], "info");
    assert!(lines[0]["line"].as_str().unwrap().contains("ready in"));
    assert_eq!(lines[1]["ts"], serde_json::Value::Null);
    assert_eq!(lines[1]["level"], "error");
}

#[test]
fn a_line_with_no_timestamp_pando_can_read_gets_null() {
    assert_eq!(
        leading_timestamp("2026-09-20T10:00:00Z ready"),
        Some("2026-09-20T10:00:00+00:00".to_string())
    );
    assert_eq!(
        leading_timestamp("[2026-09-20T10:00:00+02:00] ready"),
        Some("2026-09-20T08:00:00+00:00".to_string())
    );
    assert_eq!(leading_timestamp("ready in 412ms"), None);
    assert_eq!(leading_timestamp(""), None);
    assert_eq!(leading_timestamp("20/09/2026 10:00:00 ready"), None);
}

#[test]
fn logs_names_the_sources_a_worktree_has() {
    let fx = fixture();
    let err = logs(
        &fx.paths,
        "feat+one",
        "dev",
        10,
        false,
        false,
        &mut Vec::new(),
        &quiet,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("no logs for feat+one"),
        "{err:#}"
    );

    write_log(&fx, "feat+one", "install", "installing\n");
    let err = logs(
        &fx.paths,
        "feat+one",
        "dev",
        10,
        false,
        false,
        &mut Vec::new(),
        &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("install"),
        "it says what is there instead: {msg}"
    );
}

#[test]
fn ls_text_says_so_when_there_are_no_worktrees() {
    let fx = fixture();
    let text = capture(|b| ls_text(&fx.paths, b));
    assert!(text.contains("no worktrees"), "{text}");
}

#[test]
fn ls_text_lists_name_branch_head_state_and_path() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text(&fx.paths, b));

    assert!(text.contains("NAME"), "{text}");
    assert!(text.contains("feat+one"), "{text}");
    assert!(text.contains("feat/one"), "{text}");
    assert!(text.contains("pando"), "state column: {text}");
    assert!(
        text.contains(
            &fx.paths
                .worktrees_dir()
                .join("feat+one")
                .display()
                .to_string()
        ),
        "{text}"
    );
}

#[test]
fn ls_text_marks_adopted_dirty_and_gone_worktrees() {
    let fx = fixture();
    let adopted = fx.root.parent().unwrap().join("adopted");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );
    let dirty = actions::new(&fx.paths, &fx.config, "feat/dirty", None, &|_| {}).unwrap();
    std::fs::write(
        fx.paths.worktrees_dir().join(&dirty).join("scratch.txt"),
        "wip",
    )
    .unwrap();
    let gone = actions::new(&fx.paths, &fx.config, "feat/gone", None, &|_| {}).unwrap();
    std::fs::remove_dir_all(fx.paths.worktrees_dir().join(&gone)).unwrap();

    let text = capture(|b| ls_text(&fx.paths, b));
    for word in ["adopted", "dirty", "gone"] {
        assert!(text.contains(word), "missing {word:?} in:\n{text}");
    }
}

#[test]
fn ls_json_emits_the_documented_shape() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_json(&fx.paths, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Pinned as a literal on purpose: a bump must fail here, at the
    // commit that makes it, rather than passing quietly.
    assert_eq!(v["version"], 2);
    assert_eq!(v["project"]["id"], fx.paths.project.id.as_str());
    assert_eq!(v["project"]["name"], "acme-shop");
    assert_eq!(v["project"]["root"], fx.root.display().to_string().as_str());

    let w = &v["worktrees"][0];
    assert_eq!(w["name"], "feat+one");
    assert_eq!(w["branch"], "feat/one");
    assert_eq!(w["detached"], false);
    assert_eq!(w["dirty"], false);
    assert_eq!(w["ahead"], 0);
    assert_eq!(w["behind"], 0);
    assert_eq!(w["created_by_pando"], true);
    assert_eq!(w["prunable"], false);
    assert_eq!(w["locked"], serde_json::Value::Null);
    assert_eq!(w["pr"], serde_json::Value::Null);
    assert!(w["head"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(w["path"].as_str().is_some_and(|s| s.starts_with('/')));
}

#[test]
fn ls_json_is_an_empty_list_rather_than_an_error_with_no_worktrees() {
    let fx = fixture();
    let text = capture(|b| ls_json(&fx.paths, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"].as_array().unwrap().len(), 0);
}

#[test]
fn ls_json_reports_a_locked_worktree_with_its_reason() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    git(
        &fx.root,
        &[
            "worktree",
            "lock",
            "--reason",
            "benchmarking",
            fx.paths.worktrees_dir().join(&name).to_str().unwrap(),
        ],
    );
    let text = capture(|b| ls_json(&fx.paths, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"][0]["locked"], "benchmarking");
}

// The CLI never spawns `gh`; chips come from whatever the TUI last saw.
#[test]
fn ls_json_fills_the_pr_field_from_the_cache() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut prs = cache::PrCacheFile::new();
    prs.prs.insert(
        "feat/one".into(),
        crate::worktree::PrInfo {
            number: 42,
            title: "feat: one".into(),
            branch: "feat/one".into(),
            author: "dev".into(),
            draft: false,
            state: PrState::Open,
            url: "https://example.test/pull/42".into(),
        },
    );
    cache::save_prs(&fx.paths.pr_cache_file(), &prs).unwrap();

    let text = capture(|b| ls_json(&fx.paths, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"][0]["pr"]["number"], 42);
    assert_eq!(v["worktrees"][0]["pr"]["state"], "open");
    assert_eq!(
        v["worktrees"][0]["pr"]["url"],
        "https://example.test/pull/42"
    );
}

#[test]
fn ellipsize_keeps_short_strings_and_truncates_long_ones() {
    assert_eq!(ellipsize("short", 10), "short");
    assert_eq!(ellipsize("abcdefghij", 5), "abcd…");
}

// `head` is documented as "abc1234". Enrichment supplies the seven
// characters, but porcelain's sha is all forty, so a worktree whose
// enrichment failed used to publish a different shape in the same field.
#[test]
fn the_json_head_is_always_the_short_sha() {
    let mut w = crate::tui::app::tests::wt("feat+one");
    w.head = Some("0123456789012345678901234567890123456789".into());
    w.head_sha = None;
    assert_eq!(short_head(&w).as_deref(), Some("0123456"));

    w.head_sha = Some("abc1234".into());
    assert_eq!(short_head(&w).as_deref(), Some("abc1234"));

    w.head = None;
    w.head_sha = None;
    assert_eq!(short_head(&w), None);
}

// ---- an answers file -------------------------------------------------

// The names in the file are the names `signals` publishes, because
// they are the same function of the same type.
#[test]
fn every_question_has_one_name_that_round_trips() {
    for slot in actions::ALL_SLOTS {
        let name = slot_name(slot);
        assert!(!name.is_empty(), "{slot:?} has no name");
        assert_eq!(slot_named(&name), Some(slot), "{name} does not round trip");
    }
    assert_eq!(slot_names().len(), actions::ALL_SLOTS.len());
}

/// The nine names, written out.
///
/// `signals` publishes them and `--answers` takes them, and both get
/// them from `Slot`'s own serde names — so a rename stays invisible to
/// every test that only compares the two against each other, while
/// breaking every program ever written against them. This is the
/// assertion a rename has to walk past, and the list is also published
/// in `agent/json.md`, which the test below holds to the same order.
#[test]
fn the_nine_question_names_are_frozen() {
    assert_eq!(
        slot_names(),
        [
            "install",
            "version_files",
            "prelude",
            "processes",
            "dev_cmd",
            "port_env",
            "services",
            "schema_hook",
            "provision",
        ]
    );
}

/// Every `pando …` an agent-facing document tells a reader to run,
/// from its fenced blocks and its inline code spans.
fn commands_named_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            let line = line.trim().split('#').next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("pando ") {
                out.push(rest.trim().to_string());
            }
            continue;
        }
        // Inline: `pando doctor --json` in the middle of a sentence.
        for span in line.split('`').skip(1).step_by(2) {
            if let Some(rest) = span.strip_prefix("pando ") {
                out.push(rest.trim().to_string());
            }
        }
    }
    out
}

/// Holds a document's commands to what the binary really takes.
///
/// Instructions for a language model are the one kind of code that
/// fails silently and plausibly: a flag renamed in `cli.rs` leaves a
/// document that still reads perfectly and no longer works. clap is
/// asked rather than a list kept beside it, so there is nothing to
/// keep in step.
fn assert_every_documented_command_is_real(file: &str) {
    use clap::CommandFactory;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cli = Cli::command();
    let mut checked = 0;
    for command in commands_named_in(&text) {
        let mut tokens = command.split_whitespace();
        let Some(verb) = tokens.next() else { continue };
        // A bare `pando` with a flag of its own, like --version.
        if verb.starts_with('-') {
            continue;
        }
        let sub = cli
            .get_subcommands()
            .find(|c| c.get_name() == verb)
            .unwrap_or_else(|| {
                panic!("{file} says `pando {command}`, and pando has no {verb:?} command")
            });
        for token in tokens {
            let Some(flag) = token.strip_prefix("--") else {
                continue;
            };
            // `--answers answers.json` — the value is the next token
            // and is not a flag; nothing here needs to know that.
            assert!(
                sub.get_arguments().any(|a| a.get_long() == Some(flag)),
                "{file} says `pando {command}`, and `pando {verb}` has no --{flag}"
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "{file} names no commands at all — did the format change?"
    );
}

#[test]
fn the_contract_only_names_commands_pando_has() {
    assert_every_documented_command_is_real("agent/json.md");
}

// The host wrappers are glue, but glue that names commands: the same
// rename that would rot the brief rots them.
#[test]
fn every_host_wrapper_only_names_commands_pando_has() {
    // The repository's own README is prose for people, whose command
    // table is not in this shape. Everything here is a document an
    // agent is pointed at and follows literally.
    for file in [
        "agent/README.md",
        "agent/skills/pando-setup/SKILL.md",
        "agent/skills/pando-operate/SKILL.md",
        "agent/codex/pando-setup/SKILL.md",
        "agent/codex/pando-operate/SKILL.md",
    ] {
        assert_every_documented_command_is_real(file);
    }
}

// The brief is a procedure written for a language model, which is the
// one kind of reader that will follow a command that does not exist
// and report that it worked.
#[test]
fn the_brief_only_names_commands_pando_has() {
    assert_every_documented_command_is_real("agent/brief.md");
}

/// `CLAUDE.md` names the CLI verbs and calls them canonical — "used
/// identically in every document" — which is exactly the claim that
/// rots. It was missing `restart` for as long as `restart` existed,
/// in the file that tells every other document what the list is.
///
/// The README has its own check; this is the second place the verbs
/// are written down by hand, and the last one that was unguarded.
#[test]
fn claude_md_lists_every_verb_pando_has() {
    use clap::CommandFactory;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("CLAUDE.md");
    let text = std::fs::read_to_string(&path).expect("CLAUDE.md");
    // Hard-wrapped, so the list can straddle lines.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for sub in Cli::command().get_subcommands() {
        let name = sub.get_name();
        if name == "help" || sub.is_hide_set() {
            continue;
        }
        assert!(
            flat.contains(&format!(" {name} ")) || flat.contains(&format!(" {name}`")),
            "CLAUDE.md calls its verb list canonical and does not name {name:?}"
        );
    }
}

/// The README's own command list, against clap.
///
/// It is prose for people, so it is not in the shape
/// [`assert_every_documented_command_is_real`] parses — the
/// description runs on after the verb, and the first line is a bare
/// `pando` that opens the TUI. But the Status section under the list
/// says every command in it is implemented, and that is a claim worth
/// failing over: the line above it said "there is no code yet" for
/// eight phases of code, because nothing read it.
#[test]
fn the_readme_lists_only_commands_pando_has() {
    use clap::CommandFactory;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let text = std::fs::read_to_string(&path).expect("the README");
    let cli = Cli::command();
    let mut checked = 0;
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        // Only the fenced list. "pando never writes into your
        // repository" is a sentence, and the promise it belongs to is
        // not a command table.
        if !fenced {
            continue;
        }
        let Some(rest) = line.strip_prefix("pando ") else {
            continue;
        };
        // The one line with no verb: `pando` alone, padded out to the
        // description column.
        if rest.starts_with(' ') {
            continue;
        }
        let verb = rest.split_whitespace().next().unwrap_or_default();
        assert!(
            cli.get_subcommands().any(|c| c.get_name() == verb),
            "README.md lists `pando {verb}`, and pando has no {verb:?} command"
        );
        checked += 1;
    }
    assert!(
        checked >= 13,
        "only {checked} commands were found in README.md — did the list move?"
    );

    // And the other direction, which is the half that was missing:
    // the Status section says every command in the list is
    // implemented, and a reader takes a checked list to be a whole
    // one. `help` is clap's own, and a hidden subcommand is hidden
    // precisely because it is not for people.
    for sub in cli.get_subcommands() {
        let name = sub.get_name();
        if name == "help" || sub.is_hide_set() {
            continue;
        }
        assert!(
            text.contains(&format!("\npando {name} "))
                || text.contains(&format!("\npando {name}\n")),
            "pando has a {name:?} command and README.md does not list it — a list that is \
                 checked reads as a complete one"
        );
    }
}

/// The brief is the only place the reasoning lives, so the things it
/// has to teach are worth failing over if somebody trims it.
///
/// Phrases, not sentences: this is a guard against a section being
/// deleted, not a style checker.
#[test]
fn the_brief_teaches_the_things_only_it_teaches() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/brief.md");
    let text = std::fs::read_to_string(&path).expect("the brief");
    // Collapsed, because the document is hard-wrapped and a phrase it
    // makes is as likely as not to straddle two lines.
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for (phrase, why) in [
        ("init --answers", "the one write path"),
        ("never edit", "and that nothing else is"),
        ("by value", "how an option is named"),
        ("non-frozen", "the install guardrail"),
        (
            "built from the repository",
            "a compose file that only packages the app",
        ),
        ("[isolation] prefer", "the preference an agent cannot write"),
        (
            "machine-wide",
            "that the preference is not a fact about this repository",
        ),
        (
            "gap in the corpus",
            "the one answer the decisions log cannot hold",
        ),
        ("decisions.jsonl", "what pando records about the answerer"),
        ("exit 3", "the code that means a question is open"),
        ("--json", "never parse human-readable output"),
    ] {
        assert!(
            text.to_lowercase().contains(&phrase.to_lowercase()),
            "the brief no longer teaches {why}: it never says {phrase:?}"
        );
    }
    // And every question it tells a reader to answer.
    for name in slot_names() {
        assert!(text.contains(&name), "the brief never mentions {name}");
    }
}

/// The contract file says the same names, in the same order.
///
/// A document is the one part of a contract nothing compiles, so it is
/// the part that rots. Reading it from the test is what makes a rename
/// fail in the commit that does it rather than in somebody's agent a
/// month later.
#[test]
fn the_published_contract_names_every_question_in_order() {
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md");
    let text =
        std::fs::read_to_string(&doc).unwrap_or_else(|e| panic!("read {}: {e}", doc.display()));
    assert!(
        text.contains(&slot_names().join("  ")),
        "agent/json.md does not list the nine questions in the order pando asks them"
    );
    for name in slot_names() {
        assert!(text.contains(&name), "agent/json.md never mentions {name}");
    }
}

fn parse_err(json: &str) -> String {
    format!("{:#}", Answers::parse(json).unwrap_err())
}

#[test]
fn a_name_pando_does_not_ask_about_is_a_usage_error_naming_it() {
    let err = parse_err(r#"{"dev_command": "pnpm dev"}"#);
    assert!(err.contains("dev_command"), "{err}");
    assert!(err.contains("dev_cmd"), "and what it does ask about: {err}");
    assert!(
        Answers::parse(r#"{"dev_command": "x"}"#)
            .unwrap_err()
            .downcast_ref::<UsageError>()
            .is_some(),
        "a name that is not a question is a usage error, not a failure"
    );
}

// Caught when the file is read, not when a question happens to reach
// the slot: a shape this slot cannot take is knowable from the slot.
#[test]
fn a_shape_the_slot_cannot_take_is_refused_before_anything_is_written() {
    let err = parse_err(r#"{"install": 42}"#);
    assert!(err.contains("install"), "{err}");
    assert!(err.contains("string"), "{err}");

    let err = parse_err(r#"{"install": null}"#);
    assert!(err.contains("no \"none\" answer"), "{err}");

    let err = parse_err(r#"{"install": ["a", "b"]}"#);
    assert!(err.contains("not a list"), "{err}");

    let err = parse_err(r#"{"services": "db"}"#);
    assert!(err.contains("list of the options"), "{err}");

    let err = parse_err(r#"{"install": "   "}"#);
    assert!(err.contains("empty string"), "{err}");

    // And the shapes that are fine everywhere they are offered.
    assert!(Answers::parse(r#"{"port_env": null}"#).is_ok());
    assert!(Answers::parse(r#"{"services": []}"#).is_ok());
    assert!(Answers::parse(r#"{"provision": [".env"]}"#).is_ok());
    assert!(Answers::parse(r#"{"version_files": [".nvmrc"]}"#).is_ok());
}

#[test]
fn a_file_that_is_not_a_json_object_is_a_usage_error() {
    assert!(parse_err("[1, 2]").contains("JSON object"));
    assert!(parse_err("{").contains("not JSON"));
}

// By value, never by index: the option carries the roles a command
// owns and the process tables a workspace answer is, and a list of
// indexes is a contract that breaks the day a rule finds one more
// candidate.
#[test]
fn an_option_is_answered_by_its_own_text() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let answer = answer_from(&question, &serde_json::json!("pnpm dev:web")).unwrap();
    assert_eq!(
        answer,
        actions::Answer::Program(Box::new(actions::Answer::Choice(1)))
    );
}

// Every question has a custom answer, and a program gets the same one.
#[test]
fn a_value_no_option_has_is_a_command_of_your_own() {
    let question = dev_question(&["pnpm dev"]);
    let answer = answer_from(&question, &serde_json::json!("./serve.sh")).unwrap();
    assert_eq!(
        answer,
        actions::Answer::Program(Box::new(actions::Answer::Custom("./serve.sh".to_string())))
    );
}

// Except at the set question, where there is nothing to type: a
// service the compose file does not declare is not one pando can run.
#[test]
fn a_set_answer_that_names_nothing_on_offer_says_what_is() {
    let question = services_question();
    let err = answer_from(&question, &serde_json::json!(["postgres"])).unwrap_err();
    let printed = format!("{err:#}");
    assert!(printed.contains("postgres"), "{printed}");
    assert!(printed.contains("cache, db, mail, queue"), "{printed}");
    assert!(err.downcast_ref::<UsageError>().is_some());
}

#[test]
fn a_set_answer_is_the_options_it_names_and_an_empty_one_is_none_of_them() {
    let question = services_question();
    assert_eq!(
        answer_from(&question, &serde_json::json!(["db", "cache"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Many(vec![1, 0])))
    );
    assert_eq!(
        answer_from(&question, &serde_json::json!([])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::None))
    );
    assert_eq!(
        answer_from(&question, &serde_json::Value::Null).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::None))
    );
}

// A list slot takes a JSON array, joined into the one value the slot
// writes — so a program never has to know the separator.
#[test]
fn a_list_slot_takes_an_array_and_joins_it_the_way_the_slot_splits_it() {
    let question = actions::Question {
        slot: crate::detect::Slot::Provision,
        prompt: crate::detect::Slot::Provision.prompt().to_string(),
        options: vec![(".env,.env.local".to_string(), "here".to_string())],
        preselect: Some(0),
        allow_custom: true,
        allow_none: true,
        multi: false,
        checked: Vec::new(),
        details: Vec::new(),
    };
    // The option's own text, reached without spelling the separator.
    assert_eq!(
        answer_from(&question, &serde_json::json!([".env", ".env.local"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Choice(0)))
    );
    // And a list nothing offered is still an answer.
    assert_eq!(
        answer_from(&question, &serde_json::json!([".env", ".envrc"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Custom(".env,.envrc".to_string())))
    );
}

// An answer nothing asked about is reported rather than dropped: a
// program that answered a question pando did not ask has to hear it.
#[test]
fn the_answers_a_run_never_used_are_the_ones_nothing_asked_about() {
    let answers = Answers::parse(r#"{"install": "npm ci", "dev_cmd": "pnpm dev"}"#).unwrap();
    let question = dev_question(&["pnpm dev"]);
    assert!(answers.for_question(&question).is_some());
    assert_eq!(answers.unasked(), vec![crate::detect::Slot::Install]);
}
