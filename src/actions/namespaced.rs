//! Namespaced starts: which services get a namespace, the login they are
//! made with, making them, and what the app is told.

use anyhow::{Result, bail};

use crate::config::{self, Config};
use crate::detect::Slot;
use crate::namespace::{self, Login};
use crate::paths::PandoPaths;
use crate::recipes::NamespaceRecipe;
use crate::state::NamespaceKind;

use super::questions::{Answer, Ask, Question, answered_by};

/// The login a namespaced start makes and drops `service`'s namespaces
/// with, asked for when nothing says.
///
/// The main checkout's own first: namespaced mode is its servers, and the
/// app in a worktree logs in the way the main checkout's does. Then the one
/// pando was given before. Then the question — the same one every front end
/// puts, a terminal prompt with nothing echoed, the TUI's modal with the
/// password as dots, exit 3 for a script — and the answer written to
/// pando's own file for the project, which is 0600 and never committed,
/// under `[namespaced.<service>]`.
///
/// `keys` are the env keys the app finds the service by, which is where
/// the main checkout keeps its login too; `needs_user` is the engine's.
pub fn namespace_login(
    paths: &PandoPaths,
    config: &Config,
    service: &str,
    keys: &[String],
    needs_user: bool,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Login> {
    let file = paths.config_file();
    if let Some(login) =
        namespace::find_login(paths.root(), config, service, keys, needs_user, &file)
    {
        return Ok(login);
    }
    let (answer, by) = answered_by(ask(&login_question(paths, service, keys))?);
    let Answer::Custom(typed) = answer else {
        bail!("the login for {service} is typed, as user:password — nothing was written");
    };
    let (user, password) = match typed.split_once(':') {
        Some((user, password)) => (user.trim(), Some(password)),
        None => (typed.trim(), None),
    };
    if user.is_empty() {
        bail!(
            "a login needs a user: type it as user:password, or the user alone when there is no \
             password — nothing was written"
        );
    }
    let password = password.filter(|p| !p.is_empty());
    let mut entries = vec![("user".to_string(), toml_edit::Value::from(user))];
    if let Some(password) = password {
        entries.push(("password".to_string(), toml_edit::Value::from(password)));
    }
    config::set_detected_table(
        paths,
        config::Layer::Project,
        &["namespaced", service],
        entries,
        by.note(config::Note::Answered),
    )?;
    let from = format!("[namespaced.{service}] in {}", file.display());
    progress(&format!(
        "{service}: the login for its namespaces is kept in {from}, and only pando reads it there"
    ));
    Ok(Login::new(
        Some(user.to_string()),
        password.map(str::to_string),
        from,
    ))
}

/// The question [`namespace_login`] puts: nothing to choose from, only a
/// login to type, and the table to write it in by hand instead.
pub fn login_question(paths: &PandoPaths, service: &str, keys: &[String]) -> Question {
    Question {
        slot: Slot::Login,
        prompt: format!(
            "Which login may create and drop this worktree's own databases in {service}?"
        ),
        options: Vec::new(),
        preselect: None,
        allow_custom: true,
        allow_none: false,
        multi: false,
        checked: Vec::new(),
        details: vec![
            format!(
                "the main checkout's env files give {service} no login — nothing beside {} \
                 names a user",
                match keys.is_empty() {
                    true => "its address".to_string(),
                    false => keys.join(", "),
                }
            ),
            "it is kept in pando's own config for this project, readable by you alone, and \
             handed to the database client in its environment — never on a command line"
                .to_string(),
        ],
        answer_file: Some(paths.config_file()),
        snippet: format!("[namespaced.{service}]\nuser = \"<user>\"\npassword = \"<password>\"\n"),
    }
}

/// A service a namespaced start gives the worktree a namespace in: where
/// the main checkout's server is, what its own database or slot is there,
/// and the keys that tell the app which one is the worktree's.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub service: String,
    /// The recipe whose `[namespace]` this is, by name — what state
    /// records, so `rm` can find the commands without config.
    pub recipe: String,
    pub namespace: NamespaceRecipe,
    /// The env keys the app finds the service by: where its login is too.
    pub keys: Vec<String>,
    pub host: String,
    pub port: u16,
    /// The main checkout's own database or slot on that server.
    pub main: String,
    /// Every one the main checkout's env files name on that server, `main`
    /// first: a second URL can name another slot of the same Redis — a
    /// queue's beside a cache's — and that one is main's just as much.
    /// None of them is ever given out or emptied.
    pub mains: Vec<String>,
    /// Every key the app reads its database or slot from.
    pub tells: Vec<Tell>,
}

/// One key the app reads which database, or slot, is its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Tell {
    /// A key of its own: `DATABASE_NAME=shop`, `REDIS_DB=0`.
    Key(String),
    /// The path of a URL: `mysql://…/shop`, `redis://…/0`.
    UrlPath(String),
}

/// Every service of the project, as a namespaced start sees it.
#[derive(Debug, Clone, Default)]
pub(super) struct Plan {
    /// The ones that get a namespace.
    pub targets: Vec<Target>,
    /// Each one that stays on the main checkout's own data, and why —
    /// said at every namespaced start, because an app half on its own data
    /// and half on main's is only safe when somebody knows.
    pub shared: Vec<(String, String)>,
    /// The shared ones a step after the services could reach the main
    /// checkout's data through: every one but an engine whose namespace is
    /// a slot — a Redis the app names no slot for, which decision 3 leaves
    /// shared — and a helper the app keeps nothing in, like a mail catcher.
    pub shared_data: Vec<String>,
}

impl Plan {
    /// `redis: shared — the app reads no slot setting`, one a service
    /// that stays on main's data.
    pub fn shared_lines(&self) -> Vec<String> {
        self.shared
            .iter()
            .map(|(service, why)| format!("{service}: shared — {why}"))
            .collect()
    }
}

/// Which of the project's services a namespaced start gives the worktree a
/// namespace in, read from config, the recipes, and the main checkout's
/// env files.
///
/// A service gets one when its recipe says what a namespace is on its
/// engine, and the main checkout's env files say where its server is and
/// what the main checkout's own is called there. Anything else stays
/// shared, with the reason.
pub(super) fn plan(paths: &PandoPaths, config: &Config) -> Plan {
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    let mut out = Plan::default();
    for declared in services_with_recipes(paths, config, &recipes) {
        let data = !declared.helper
            && !declared
                .recipe
                .as_ref()
                .and_then(|recipe| recipe.namespace.as_ref())
                .is_some_and(|namespace| namespace.kind == NamespaceKind::Slot);
        match target(
            paths.root(),
            &declared.service,
            declared.recipe.as_ref(),
            declared.keys,
        ) {
            Ok(target) => out.targets.push(target),
            Err(why) => {
                if data {
                    out.shared_data.push(declared.service.clone());
                }
                out.shared.push((declared.service, why));
            }
        }
    }
    out
}

/// One service config declares, as a namespaced start first sees it.
struct Declared {
    service: String,
    /// The recipe that knows its engine, when one does.
    recipe: Option<crate::recipes::Recipe>,
    /// The env keys the app finds it by.
    keys: Vec<String>,
    /// A compose image the catalog knows as a helper the app keeps no
    /// data in: a mail catcher.
    helper: bool,
}

/// Every service config declares, the recipe that knows its engine when
/// one does, and the env keys the app finds it by.
fn services_with_recipes(
    paths: &PandoPaths,
    config: &Config,
    recipes: &crate::recipes::Recipes,
) -> Vec<Declared> {
    let mut out = Vec::new();
    for service in &config.services {
        match service {
            config::ServiceConfig::Native { .. } => {
                let Some(entry) = crate::native::Entry::of(service) else {
                    continue;
                };
                let recipe = crate::native::resolve(recipes, &entry)
                    .ok()
                    .map(|resolved| resolved.recipe);
                let keys = entry
                    .env_map(recipe.as_ref())
                    .0
                    .into_iter()
                    .filter(|(_, name)| name == entry.name)
                    .map(|(key, _)| key)
                    .collect();
                out.push(Declared {
                    service: entry.name.to_string(),
                    recipe,
                    keys,
                    helper: false,
                });
            }
            config::ServiceConfig::Compose {
                file, include, env, ..
            } => {
                // The image the main checkout's compose file runs is what
                // says which engine it is: a service called `db` running
                // `mariadb:11` is a MariaDB.
                let parsed = crate::compose::file_in(paths.root(), file)
                    .and_then(|file| crate::compose::read(&file))
                    .ok();
                for name in include {
                    let reference = parsed
                        .as_ref()
                        .and_then(|p| p.services.get(name))
                        .and_then(|s| s.image.as_deref());
                    let image = reference.map(crate::catalog::images::image_name);
                    let recipe = image
                        .into_iter()
                        .chain(std::iter::once(name.as_str()))
                        .find_map(|candidate| recipes.get(candidate).ok())
                        .map(|loaded| loaded.recipe.clone());
                    let keys = env
                        .iter()
                        .filter(|(_, service)| *service == name)
                        .map(|(key, _)| key.clone())
                        .collect();
                    let helper = reference
                        .and_then(crate::catalog::images::known)
                        .is_some_and(|known| known.role == crate::catalog::images::Role::Utility);
                    out.push(Declared {
                        service: name.clone(),
                        recipe,
                        keys,
                        helper,
                    });
                }
            }
        }
    }
    out
}

/// One service as a namespaced start would reach it, or why it cannot.
fn target(
    root: &std::path::Path,
    service: &str,
    recipe: Option<&crate::recipes::Recipe>,
    keys: Vec<String>,
) -> std::result::Result<Target, String> {
    let recipe = recipe.ok_or("pando has no recipe that knows its engine")?;
    let namespace = recipe
        .namespace
        .clone()
        .ok_or_else(|| format!("pando knows no namespace for {}", recipe.name))?;
    if keys.is_empty() {
        return Err("nothing in the project's env tells the app where it is".to_string());
    }
    let values: Vec<(String, String)> = keys
        .iter()
        .filter_map(|key| Some((key.clone(), crate::services::value_in_env(root, key)?)))
        .map(|(key, value)| (key, value.trim().to_string()))
        .collect();
    let urls: Vec<&(String, String)> = values.iter().filter(|(_, v)| v.contains("://")).collect();
    let port = values
        .iter()
        .find_map(|(_, value)| crate::services::port_of_value(value))
        .ok_or("the main checkout's env files give no port for it")?;
    let host = urls
        .iter()
        .find_map(|(_, url)| crate::services::url_host(url))
        .or_else(|| {
            crate::services::sibling_value(root, keys.iter().map(String::as_str), &["_HOST"])
                .map(|(_, host)| host)
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let (mains, tells) = match namespace.kind {
        NamespaceKind::Database => database_main(root, &keys, &urls)
            .ok_or("nothing in the main checkout's env files names its database")?,
        NamespaceKind::Slot => {
            slot_main(root, &keys, &urls).ok_or("the app reads no slot setting")?
        }
    };
    Ok(Target {
        service: service.to_string(),
        recipe: recipe.name.clone(),
        namespace,
        keys,
        host,
        port,
        main: mains[0].clone(),
        mains,
        tells,
    })
}

/// Every slot the main checkout's env files name, the one it is known by
/// first, and every key that says which one the app uses: the path of
/// each URL — none is slot 0 — and a key of its own beside the address,
/// `REDIS_DB` next to `REDIS_PORT`. An app that reads neither has nowhere
/// to be told another slot, and stays shared.
fn slot_main(
    root: &std::path::Path,
    keys: &[String],
    urls: &[&(String, String)],
) -> Option<(Vec<String>, Vec<Tell>)> {
    let mut mains: Vec<String> = Vec::new();
    let mut tells: Vec<Tell> = Vec::new();
    for (key, url) in urls {
        let (_, path) = crate::services::url_identity(url);
        let slot = path.unwrap_or_else(|| "0".to_string());
        if slot.chars().all(|c| c.is_ascii_digit()) {
            add_main(&mut mains, slot);
            tells.push(Tell::UrlPath(key.clone()));
        }
    }
    if let Some((key, value)) =
        crate::services::sibling_value(root, keys.iter().map(String::as_str), &["_DB"])
        && value.chars().all(|c| c.is_ascii_digit())
    {
        add_main(&mut mains, value);
        tells.push(Tell::Key(key));
    }
    (!mains.is_empty()).then_some((mains, tells))
}

/// Adds one name the main checkout's env files give its own, once: `07`
/// and `7` are one slot, `Shop` and `shop` one database to MariaDB on
/// macOS.
fn add_main(mains: &mut Vec<String>, main: String) {
    let same = |a: &str, b: &str| match (a.parse::<u32>(), b.parse::<u32>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.eq_ignore_ascii_case(b),
    };
    if !mains.iter().any(|known| same(known, &main)) {
        mains.push(main);
    }
}

/// Every database the main checkout's env files name, the one it is
/// known by first, and every key that says which one the app uses: the
/// path of each URL, and a key of its own beside the address —
/// `DATABASE_NAME` next to `DATABASE_PORT`.
fn database_main(
    root: &std::path::Path,
    keys: &[String],
    urls: &[&(String, String)],
) -> Option<(Vec<String>, Vec<Tell>)> {
    let mut mains: Vec<String> = Vec::new();
    let mut tells: Vec<Tell> = Vec::new();
    for (key, url) in urls {
        if let (_, Some(database)) = crate::services::url_identity(url) {
            add_main(&mut mains, database);
            tells.push(Tell::UrlPath(key.clone()));
        }
    }
    for key in keys {
        let Some(prefix) = ["_PORT", "_HOST", "_URL"]
            .iter()
            .find_map(|suffix| key.strip_suffix(suffix))
        else {
            continue;
        };
        for suffix in ["_NAME", "_DATABASE", "_DB"] {
            let sibling = format!("{prefix}{suffix}");
            let Some(value) = crate::services::value_in_env(root, &sibling) else {
                continue;
            };
            let value = value.trim();
            // All digits is a numbered database — a slot — not a name.
            if value.is_empty() || value.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            add_main(&mut mains, value.to_string());
            let tell = Tell::Key(sibling);
            if !tells.contains(&tell) {
                tells.push(tell);
            }
            break;
        }
    }
    (!mains.is_empty()).then_some((mains, tells))
}

/// What a namespaced start made ready before anything was stopped.
#[derive(Debug, Clone, Default)]
pub(super) struct Ready {
    pub plan: Plan,
    /// One per target, as recorded. Carried to the start rather than read
    /// back from state, so the start writes them into the record it spawns
    /// from — whatever happened to that record in between.
    pub namespaces: Vec<crate::state::NamespaceRecord>,
    /// Whether a namespace was made just now: an empty database the
    /// schema step has to fill, whatever its fingerprint says.
    pub fresh: bool,
}

impl Ready {
    /// Why the steps after the services — a schema, a migration, a seed —
    /// would not run on data of the worktree's own, when they would not: a
    /// service that stays on the main checkout's data they could reach, or
    /// no database of the worktree's own at all, only a slot. `None` when
    /// they run on the worktree's own, as on an isolated start.
    pub fn not_own_data(&self) -> Option<String> {
        if !self.plan.shared_data.is_empty() {
            return Some(format!(
                "{} {} on the main checkout's data",
                self.plan.shared_data.join(", "),
                match self.plan.shared_data.len() {
                    1 => "stays",
                    _ => "stay",
                }
            ));
        }
        (!self
            .namespaces
            .iter()
            .any(|namespace| namespace.kind == NamespaceKind::Database))
        .then(|| "no database here is this worktree's own, only a slot".to_string())
    }
}

/// Everything a namespaced start does before any process is stopped or
/// spawned: a login for each server, each server answering, and each
/// namespace made — or found again — and recorded.
///
/// First, so that a start which cannot happen costs nothing: a server that
/// does not answer, a login nothing gives, or a login the server will not
/// let make a database stops the start here with nothing made, and the
/// worktree's processes as they were. Nothing it does is undone by a later
/// failure either: a namespace is the worktree's until `rm`.
pub(super) fn prepare(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    progress: &dyn Fn(&str),
) -> Result<Ready> {
    let plan = plan(paths, config);
    let mut servers: Vec<(&Target, namespace::Server<'_>)> = Vec::new();
    for target in &plan.targets {
        let server = server_for(paths, config, target)?;
        server.ping()?;
        servers.push((target, server));
    }
    let mut fresh = false;
    let mut namespaces = Vec::new();
    for (target, server) in &servers {
        let (namespace, made) = match target.namespace.kind {
            NamespaceKind::Database => ensure_database(paths, name, target, server, progress)?,
            NamespaceKind::Slot => ensure_slot(paths, name, target, server)?,
        };
        // A slot given out is empty and needs no schema; only a database
        // made just now does.
        fresh |= made && namespace.kind == NamespaceKind::Database;
        progress(&format!(
            "{}: {}{}",
            target.service,
            own(&namespace),
            if made { ", made just now" } else { "" }
        ));
        namespaces.push(namespace);
    }
    for line in plan.shared_lines() {
        progress(&line);
    }
    Ok(Ready {
        plan,
        namespaces,
        fresh,
    })
}

/// `own database northwind_traders__feat_x`, `slot 3` — what a worktree
/// got in one service, as a line says it.
pub(super) fn own(namespace: &crate::state::NamespaceRecord) -> String {
    match namespace.kind {
        NamespaceKind::Database => format!("own database {}", namespace.name),
        NamespaceKind::Slot => format!("slot {}", namespace.name),
    }
}

/// The server one target lives on, with the login to reach it.
pub(super) fn server_for<'a>(
    paths: &PandoPaths,
    config: &Config,
    target: &'a Target,
) -> Result<namespace::Server<'a>> {
    let file = paths.config_file();
    let login = match namespace::find_login(
        paths.root(),
        config,
        &target.service,
        &target.keys,
        target.namespace.user,
        &file,
    ) {
        Some(login) => login,
        None if !target.namespace.user => Login::none(),
        None => bail!(
            "nothing gives pando a login for {}'s namespaces — the main checkout's env files \
             have none beside {}. `pando start` on a terminal asks for one; or write \
             [namespaced.{}] with a user and a password in {}",
            target.service,
            target.keys.join(", "),
            target.service,
            file.display()
        ),
    };
    Ok(namespace::Server {
        service: &target.service,
        recipe: &target.namespace,
        host: target.host.clone(),
        port: target.port,
        login,
        bin_dir: paths.home.join("bin"),
    })
}

/// The login each target needs and nothing gives, asked for now: what
/// `resolve_for_start` does for a namespaced start before anything runs.
///
/// An answer is written to pando's own file, and read back from there into
/// `config`, so the start this is for finds it in the config it is handed.
pub(super) fn ask_for_logins(
    paths: &PandoPaths,
    config: &mut Config,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    for target in plan(paths, config).targets {
        if !target.namespace.user || config.namespaced.contains_key(&target.service) {
            continue;
        }
        let login = namespace_login(
            paths,
            config,
            &target.service,
            &target.keys,
            true,
            ask,
            progress,
        )?;
        if login.from.starts_with("[namespaced.")
            && let Some(written) = config::load(paths)?
                .config
                .namespaced
                .get(&target.service)
                .cloned()
        {
            config.namespaced.insert(target.service.clone(), written);
        }
    }
    Ok(())
}

/// This worktree's database on one server: the one state records, found
/// again — or made again, if somebody dropped it by hand — else a new one
/// under the first of its two names the server does not already have.
///
/// Returns it, recorded, and whether it was made just now.
fn ensure_database(
    paths: &PandoPaths,
    name: &str,
    target: &Target,
    server: &namespace::Server<'_>,
    progress: &dyn Fn(&str),
) -> Result<(crate::state::NamespaceRecord, bool)> {
    let wanted = |candidate: &str| crate::state::NamespaceRecord {
        service: target.service.clone(),
        recipe: target.recipe.clone(),
        kind: NamespaceKind::Database,
        host: target.host.clone(),
        port: target.port,
        name: candidate.to_string(),
        main: target.main.clone(),
        keys: target.keys.clone(),
        used_at: chrono::Utc::now(),
    };
    let store = crate::state::load(&paths.state_file())?;
    let recorded = store.worktrees.get(name).and_then(|record| {
        record.namespaces.iter().find(|ns| {
            ns.service == target.service
                && ns.kind == NamespaceKind::Database
                && ns.main == target.main
                && namespace::same_namespace(ns, &wanted(&ns.name))
        })
    });
    if let Some(recorded) = recorded {
        let recorded = recorded.clone();
        if server.exists(&recorded.name)? {
            return Ok((record(paths, name, wanted(&recorded.name))?, false));
        }
        // Made by pando for this worktree, and gone: somebody dropped it.
        // The same name, made again and empty.
        progress(&format!(
            "{}: {} is gone from {}, so it is made again, empty",
            target.service,
            recorded.name,
            server.address()
        ));
        server.create(&recorded.name, &target.main)?;
        return Ok((record(paths, name, wanted(&recorded.name))?, true));
    }
    let names = namespace::database_names(&target.main, paths.project_id(), name)?;
    for candidate in &names {
        // Another worktree's, as state knows it, is not this one's to use.
        let held = store.worktrees.iter().any(|(other, record)| {
            other != name
                && record
                    .namespaces
                    .iter()
                    .any(|ns| namespace::same_namespace(ns, &wanted(candidate)))
        });
        if held {
            continue;
        }
        match server.create(candidate, &target.main)? {
            namespace::Created::Made => {
                return Ok((record(paths, name, wanted(candidate))?, true));
            }
            namespace::Created::AlreadyThere => progress(&format!(
                "{}: {candidate} is already on {} and pando did not make it, so it is left \
                 alone",
                target.service,
                server.address()
            )),
        }
    }
    bail!(
        "{} on {} already has {}, and pando made none of them — drop them there if they are \
         leftovers, and start again",
        target.service,
        server.address(),
        names.join(" and ")
    )
}

/// This worktree's slot on one server: the one state records, else the
/// first one no worktree holds and the server says is empty.
///
/// Empty, not merely unrecorded: a slot with keys in it that no worktree
/// of this project holds is somebody else's — another project's, or the
/// developer's own — and emptying it later would destroy their data. When
/// every slot is held the start stops and says by whom; a start that can
/// ask has already been asked which one to free.
fn ensure_slot(
    paths: &PandoPaths,
    name: &str,
    target: &Target,
    server: &namespace::Server<'_>,
) -> Result<(crate::state::NamespaceRecord, bool)> {
    let slot = |n: &str| crate::state::NamespaceRecord {
        service: target.service.clone(),
        recipe: target.recipe.clone(),
        kind: NamespaceKind::Slot,
        host: target.host.clone(),
        port: target.port,
        name: n.to_string(),
        main: target.main.clone(),
        keys: target.keys.clone(),
        used_at: chrono::Utc::now(),
    };
    let store = crate::state::load(&paths.state_file())?;
    if let Some(recorded) = store.worktrees.get(name).and_then(|record| {
        record.namespaces.iter().find(|ns| {
            ns.service == target.service && namespace::same_namespace(ns, &slot(&ns.name))
        })
    }) {
        return Ok((record(paths, name, slot(&recorded.name))?, false));
    }
    let holders = slot_holders(&store, target);
    let mut full: Vec<u32> = Vec::new();
    for n in allocatable(target) {
        if holders.iter().any(|holder| holder.slot == n) {
            continue;
        }
        if server.size(n)? > 0 {
            full.push(n);
            continue;
        }
        return Ok((record(paths, name, slot(&n.to_string()))?, true));
    }
    if full.is_empty() {
        bail!(
            "every slot of {} on {} that pando gives out is held — {}. `pando start` on a \
             terminal asks which stopped one to free; `pando rm` of one you no longer need frees \
             its slot with it",
            target.service,
            server.address(),
            holders
                .iter()
                .map(Holder::describe)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    bail!(
        "no slot of {} on {} is free: {} hold keys no worktree of this project put there, and \
         pando never takes a slot somebody else is using",
        target.service,
        server.address(),
        full.iter()
            .map(|n| format!("slot {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The slots pando may give a worktree on a target's server: every one it
/// has but 0 — where an app that names no slot keeps its keys — and every
/// one the main checkout's env files name.
fn allocatable(target: &Target) -> Vec<u32> {
    let slots = target.namespace.slots.unwrap_or(0);
    let mains: Vec<u32> = target
        .mains
        .iter()
        .filter_map(|main| main.parse::<u32>().ok())
        .collect();
    (1..slots).filter(|n| !mains.contains(n)).collect()
}

/// A worktree holding a slot on a target's server.
#[derive(Debug, Clone)]
struct Holder {
    worktree: String,
    slot: u32,
    namespace: crate::state::NamespaceRecord,
    running: bool,
}

impl Holder {
    /// `feat+x (slot 3, running)`.
    fn describe(&self) -> String {
        format!(
            "{} (slot {}, {})",
            self.worktree,
            self.slot,
            match self.running {
                true => "running".to_string(),
                false => format!("last ran {}", since(self.namespace.used_at)),
            }
        )
    }
}

/// Every worktree holding a slot on this target's server, as state says.
fn slot_holders(store: &crate::state::State, target: &Target) -> Vec<Holder> {
    let mut out = Vec::new();
    for (worktree, record) in &store.worktrees {
        let running = record.processes.values().any(|p| {
            !matches!(p.phase, crate::state::Phase::Failed { .. })
                && crate::process::is_alive(p.pid)
        });
        for ns in &record.namespaces {
            let here = crate::state::NamespaceRecord {
                host: target.host.clone(),
                port: target.port,
                kind: NamespaceKind::Slot,
                ..ns.clone()
            };
            if !namespace::same_namespace(ns, &here) {
                continue;
            }
            if let Ok(slot) = ns.name.parse::<u32>() {
                out.push(Holder {
                    worktree: worktree.clone(),
                    slot,
                    namespace: ns.clone(),
                    running,
                });
            }
        }
    }
    out.sort_by_key(|holder| holder.slot);
    out
}

/// `3 days ago`, `just now` — when a stopped worktree last ran, for a
/// choice about whose data to empty.
fn since(at: chrono::DateTime<chrono::Utc>) -> String {
    let ago = chrono::Utc::now() - at;
    match ago.num_minutes() {
        m if m < 1 => "just now".to_string(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 60 * 24 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (60 * 24)),
    }
}

/// Decision 9: when every slot a target's server has is held and this
/// worktree has none, asks which stopped worktree gives its slot up — and
/// empties and frees the one chosen, through the guard, before anything
/// else of this start happens.
///
/// A running worktree is never offered: its app is using the slot. When
/// every holder is running, the start stops naming them. A script gets
/// exit 3 with the list, as for any question.
pub(super) fn free_slots_if_full(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    for target in plan(paths, config).targets {
        if target.namespace.kind != NamespaceKind::Slot {
            continue;
        }
        let store = crate::state::load(&paths.state_file())?;
        let holders = slot_holders(&store, &target);
        if holders.iter().any(|holder| holder.worktree == name) {
            continue;
        }
        let taken = allocatable(&target)
            .iter()
            .all(|n| holders.iter().any(|holder| holder.slot == *n));
        if !taken {
            continue;
        }
        let stopped: Vec<&Holder> = holders.iter().filter(|holder| !holder.running).collect();
        let running: Vec<String> = holders
            .iter()
            .filter(|holder| holder.running)
            .map(Holder::describe)
            .collect();
        if stopped.is_empty() {
            bail!(
                "every slot of {} on {}:{} is held by a running worktree — {}. Stop one, and its \
                 slot can be freed",
                target.service,
                target.host,
                target.port,
                running.join(", ")
            );
        }
        let question = Question {
            slot: Slot::FreeSlot,
            prompt: format!(
                "Every slot of {} on {}:{} is held. Which stopped worktree gives up its slot?",
                target.service, target.host, target.port
            ),
            options: stopped
                .iter()
                .map(|holder| {
                    (
                        holder.worktree.clone(),
                        format!(
                            "slot {}, last ran {}",
                            holder.slot,
                            since(holder.namespace.used_at)
                        ),
                    )
                })
                .collect(),
            preselect: None,
            allow_custom: false,
            allow_none: true,
            multi: false,
            checked: Vec::new(),
            details: std::iter::once(
                "the one chosen has its slot emptied — every key in it deleted — and gets a new, \
                 empty one the next time it starts"
                    .to_string(),
            )
            .chain(
                (!running.is_empty())
                    .then(|| format!("running, so not offered: {}", running.join(", "))),
            )
            .collect(),
            answer_file: None,
            snippet: String::new(),
        };
        let (answer, _) = answered_by(ask(&question)?);
        let chosen = match answer {
            Answer::Choice(index) => stopped
                .get(index)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("option {index} is not on offer"))?,
            Answer::None => bail!("no slot was freed, so nothing was started"),
            _ => bail!("the slot to free is chosen from the list, not typed"),
        };
        free_slot(paths, config, &target, chosen, progress)?;
    }
    Ok(())
}

/// Empties and releases one worktree's slot: checked against state again
/// under the lock — still recorded, still stopped — then through
/// [`namespace::may_drop`], then emptied, then forgotten.
fn free_slot(
    paths: &PandoPaths,
    config: &Config,
    target: &Target,
    holder: &Holder,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let server = server_for(paths, config, target)?;
    let _lock = crate::state::lock(&paths.lock_file())?;
    let mut store = crate::state::load(&paths.state_file())?;
    let still = slot_holders(&store, target)
        .into_iter()
        .find(|h| h.worktree == holder.worktree && h.slot == holder.slot);
    match still {
        Some(h) if !h.running => {}
        Some(_) => bail!(
            "{} started again while this start was asking, so its slot was not freed",
            holder.worktree
        ),
        None => bail!("{} no longer holds slot {}", holder.worktree, holder.slot),
    }
    namespace::may_drop(
        &store,
        &holder.worktree,
        &holder.namespace,
        &target.mains.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    server.drop(&holder.namespace.name, &target.main)?;
    if let Some(record) = store.worktrees.get_mut(&holder.worktree) {
        record.namespaces.retain(|ns| ns != &holder.namespace);
    }
    crate::state::save(&paths.state_file(), &store)?;
    progress(&format!(
        "{}: slot {} emptied and freed — {} gets a new, empty one when it next starts",
        target.service, holder.slot, holder.worktree
    ));
    Ok(())
}

/// Writes a namespace into this worktree's record — the moment after the
/// server made it, which is what makes the record pando's word that pando
/// made it — or marks a recorded one as used now.
fn record(
    paths: &PandoPaths,
    name: &str,
    namespace: crate::state::NamespaceRecord,
) -> Result<crate::state::NamespaceRecord> {
    let worktree = super::worktree::find_worktree(paths, name)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or(worktree.path);
    let _lock = crate::state::lock(&paths.lock_file())?;
    let mut store = crate::state::load(&paths.state_file())?;
    let record = store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| crate::state::WorktreeRecord::new(canonical, false));
    keep(record, &namespace);
    crate::state::save(&paths.state_file(), &store)?;
    Ok(namespace)
}

/// Puts a namespace into a worktree's record, or marks the one already
/// there as used now. The caller holds the lock.
pub(super) fn keep(
    record: &mut crate::state::WorktreeRecord,
    namespace: &crate::state::NamespaceRecord,
) {
    match record
        .namespaces
        .iter_mut()
        .find(|ns| ns.service == namespace.service && namespace::same_namespace(ns, namespace))
    {
        Some(existing) => existing.used_at = namespace.used_at,
        None => record.namespaces.push(namespace.clone()),
    }
}

/// What a namespaced worktree's processes and hooks are told about the
/// services: the main checkout's own addresses, as a shared start tells
/// them, with every key that names a database or a slot naming the
/// worktree's own instead.
///
/// A target with no namespace among `namespaces` is an error, never a
/// shared service: its keys would go on naming the main checkout's
/// database, and a branch's migrations would run against it.
pub(super) fn namespaced_env(
    paths: &PandoPaths,
    config: &Config,
    plan: &Plan,
    namespaces: &[crate::state::NamespaceRecord],
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut env = super::services::shared_service_env(paths, config);
    for target in &plan.targets {
        let Some(namespace) = namespaces.iter().find(|ns| {
            ns.service == target.service
                && ns.main == target.main
                && ns.port == target.port
                && ns.kind == target.namespace.kind
        }) else {
            bail!(
                "{} has no namespace of this worktree's own recorded on {}:{}, so pando will not \
                 point it at the main checkout's data — start it namespaced again to make one",
                target.service,
                target.host,
                target.port
            );
        };
        for tell in &target.tells {
            match tell {
                Tell::Key(key) => {
                    env.insert(key.clone(), namespace.name.clone());
                }
                Tell::UrlPath(key) => {
                    let url = env
                        .get(key)
                        .cloned()
                        .or_else(|| crate::services::value_in_env(paths.root(), key));
                    let Some(rewritten) = url.and_then(|url| {
                        crate::services::with_url_path(url.trim(), &namespace.name)
                    }) else {
                        bail!(
                            "{key} is not a URL pando can point at {}, so it would go on naming \
                             the main checkout's",
                            namespace.name
                        );
                    };
                    env.insert(key.clone(), rewritten);
                }
            }
        }
    }
    Ok(env)
}

/// What goes with a worktree when it is removed, one phrase a namespace:
/// `drops database northwind_traders__feat_x`, `empties redis slot 3`.
/// What the remove dialog says before anything happens.
pub fn namespaces_rm_drops(record: &crate::state::WorktreeRecord) -> Vec<String> {
    record
        .namespaces
        .iter()
        .map(|ns| match ns.kind {
            NamespaceKind::Database => format!("drops database {}", ns.name),
            NamespaceKind::Slot => format!("empties {} slot {}", ns.service, ns.name),
        })
        .collect()
}

/// Drops every namespace a removed worktree held — each database dropped,
/// each slot emptied — through [`namespace::may_drop`], on the server it
/// was made on.
///
/// Called by `rm` once the worktree itself is gone, so a removal git
/// refused never costs a worktree its data. One pando may not or cannot
/// drop is said, with the command that drops it by hand; its record goes
/// with the worktree's, and `doctor` lists it after that.
pub(super) fn drop_namespaces(
    paths: &PandoPaths,
    store: &crate::state::State,
    name: &str,
    progress: &dyn Fn(&str),
) {
    let Some(record) = store.worktrees.get(name) else {
        return;
    };
    if record.namespaces.is_empty() {
        return;
    }
    // `rm` works with a config that does not load; the logins it might
    // hold are only a fallback to the main checkout's own.
    let config = config::load(paths)
        .map(|loaded| loaded.config)
        .unwrap_or_else(|_| config::load_without_home(paths).config);
    let recipes = crate::recipes::Recipes::load(&paths.recipes_dir());
    let targets = plan(paths, &config).targets;
    for ns in &record.namespaces {
        let target = targets
            .iter()
            .find(|t| t.service == ns.service && t.port == ns.port && t.namespace.kind == ns.kind);
        // Every name the main checkout's env files give its own today,
        // refused whatever the record says.
        let main_now: Vec<&str> = target
            .iter()
            .flat_map(|t| t.mains.iter().map(String::as_str))
            .collect();
        let what = namespace::describe(ns);
        if let Err(e) = namespace::may_drop(store, name, ns, &main_now) {
            progress(&format!("{}: {what} is left as it is — {e:#}", ns.service));
            continue;
        }
        let Some(recipe) = recipes
            .get(&ns.recipe)
            .ok()
            .and_then(|loaded| loaded.recipe.namespace.clone())
        else {
            progress(&format!(
                "{}: {what} is left as it is — the recipe {:?} no longer says how to drop it",
                ns.service, ns.recipe
            ));
            continue;
        };
        let keys = match (&ns.keys[..], target) {
            ([], Some(target)) => target.keys.clone(),
            (keys, _) => keys.to_vec(),
        };
        let login = namespace::find_login(
            paths.root(),
            &config,
            &ns.service,
            &keys,
            recipe.user,
            &paths.config_file(),
        )
        .unwrap_or_else(Login::none);
        let server = namespace::Server {
            service: &ns.service,
            recipe: &recipe,
            host: ns.host.clone(),
            port: ns.port,
            login,
            bin_dir: paths.home.join("bin"),
        };
        match server.drop(&ns.name, &ns.main) {
            Ok(()) => progress(&format!(
                "{}: {}",
                ns.service,
                match ns.kind {
                    NamespaceKind::Database => format!("dropped database {}", ns.name),
                    NamespaceKind::Slot => format!("emptied slot {}", ns.name),
                }
            )),
            Err(e) => progress(&format!(
                "{}: {what} could not be dropped — {e:#}{}",
                ns.service,
                server
                    .by_hand(&ns.name)
                    .map(|command| format!(" — `{command}` drops it by hand"))
                    .unwrap_or_default()
            )),
        }
    }
}

/// A database named for a worktree of this project that no worktree's
/// record holds: one `rm` could not drop, or one whose record was lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub service: String,
    /// `host:port`.
    pub address: String,
    pub name: String,
    /// The command that drops it, for a person: pando never drops what it
    /// has no record of.
    pub by_hand: Option<String>,
}

/// Every leftover database of this project on the servers it namespaces
/// in, as far as each server says — asked only of a project that runs
/// namespaced worktrees, because asking is a login to a server.
///
/// Read-only: a listing, never a drop. A server that does not answer, or
/// a login nothing gives, is skipped rather than reported: this is a look
/// for leftovers, and not being able to look is not one.
pub fn namespace_leftovers(
    paths: &PandoPaths,
    config: &Config,
    store: &crate::state::State,
) -> Vec<Leftover> {
    let namespaced = store.worktrees.values().any(|record| {
        !record.namespaces.is_empty() || record.mode == Some(crate::state::ServiceMode::Namespaced)
    });
    if !namespaced {
        return Vec::new();
    }
    let mut out = Vec::new();
    for target in plan(paths, config).targets {
        if target.namespace.kind != NamespaceKind::Database || target.namespace.list.is_none() {
            continue;
        }
        let Ok(server) = server_for(paths, config, &target) else {
            continue;
        };
        let Ok(names) = server.list(&target.main) else {
            continue;
        };
        for name in names {
            let held = store.worktrees.values().any(|record| {
                record.namespaces.iter().any(|ns| {
                    ns.kind == NamespaceKind::Database
                        && ns.port == target.port
                        && ns.name.eq_ignore_ascii_case(&name)
                })
            });
            if held {
                continue;
            }
            out.push(Leftover {
                service: target.service.clone(),
                address: server.address(),
                by_hand: server.by_hand(&name),
                name,
            });
        }
    }
    out
}

/// What a worktree holds in each service, as `status` says it: the
/// service, a word, and the rest of the line.
///
/// `own` for a namespace it runs on; `kept` for one it holds while running
/// in another mode, which waits for the way back until `rm`; and, for a
/// namespaced worktree, `shared` for a service that stays on the main
/// checkout's data, with why. `config` is `None` when it does not load,
/// and then the shared ones go unsaid.
pub fn namespace_lines(
    paths: &PandoPaths,
    config: Option<&Config>,
    record: &crate::state::WorktreeRecord,
) -> Vec<(String, &'static str, String)> {
    let running_on_them = record.mode() == crate::state::ServiceMode::Namespaced;
    let mut out: Vec<(String, &'static str, String)> = record
        .namespaces
        .iter()
        .map(|ns| {
            let what = match ns.kind {
                NamespaceKind::Database => format!("database {}", ns.name),
                NamespaceKind::Slot => format!("slot {}", ns.name),
            };
            let (word, until) = match running_on_them {
                true => ("own", ""),
                false => ("kept", ", until rm"),
            };
            (
                ns.service.clone(),
                word,
                format!("{what} on {}:{}{until}", ns.host, ns.port),
            )
        })
        .collect();
    if running_on_them && let Some(config) = config {
        for (service, why) in plan(paths, config).shared {
            out.push((service, "shared", why));
        }
    }
    out
}
