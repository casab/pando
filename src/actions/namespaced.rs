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
    /// A line for each one that stays on the main checkout's own data, and
    /// why — said at every namespaced start, because an app half on its
    /// own data and half on main's is only safe when somebody knows.
    pub shared: Vec<String>,
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
    for (service, recipe, keys) in services_with_recipes(paths, config, &recipes) {
        match target(paths.root(), &service, recipe.as_ref(), keys) {
            Ok(target) => out.targets.push(target),
            Err(why) => out.shared.push(format!("{service}: shared — {why}")),
        }
    }
    out
}

/// Every service config declares, the recipe that knows its engine when
/// one does, and the env keys the app finds it by.
fn services_with_recipes(
    paths: &PandoPaths,
    config: &Config,
    recipes: &crate::recipes::Recipes,
) -> Vec<(String, Option<crate::recipes::Recipe>, Vec<String>)> {
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
                out.push((entry.name.to_string(), recipe, keys));
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
                    let image = parsed
                        .as_ref()
                        .and_then(|p| p.services.get(name))
                        .and_then(|s| s.image.as_deref())
                        .map(crate::catalog::images::image_name);
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
                    out.push((name.clone(), recipe, keys));
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
    let (main, tells) = match namespace.kind {
        NamespaceKind::Database => database_main(root, &keys, &urls)
            .ok_or("nothing in the main checkout's env files names its database")?,
        NamespaceKind::Slot => {
            return Err("pando does not give a worktree a slot of its own in it yet".to_string());
        }
    };
    Ok(Target {
        service: service.to_string(),
        recipe: recipe.name.clone(),
        namespace,
        keys,
        host,
        port,
        main,
        tells,
    })
}

/// The main checkout's own database, and every key that says which one
/// the app uses: the path of each URL, and a key of its own beside the
/// address — `DATABASE_NAME` next to `DATABASE_PORT`.
fn database_main(
    root: &std::path::Path,
    keys: &[String],
    urls: &[&(String, String)],
) -> Option<(String, Vec<Tell>)> {
    let mut main: Option<String> = None;
    let mut tells: Vec<Tell> = Vec::new();
    for (key, url) in urls {
        if let (_, Some(database)) = crate::services::url_identity(url) {
            main.get_or_insert(database);
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
            main.get_or_insert_with(|| value.to_string());
            let tell = Tell::Key(sibling);
            if !tells.contains(&tell) {
                tells.push(tell);
            }
            break;
        }
    }
    Some((main?, tells))
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
            NamespaceKind::Slot => bail!(
                "pando does not give a worktree a slot of its own in {} yet",
                target.service
            ),
        };
        fresh |= made;
        progress(&format!(
            "{}: {}{}",
            target.service,
            own(&namespace),
            if made { ", made just now" } else { "" }
        ));
        namespaces.push(namespace);
    }
    for line in &plan.shared {
        progress(line);
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
