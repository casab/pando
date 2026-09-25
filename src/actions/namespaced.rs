//! Namespaced starts: the login they make namespaces with.

use anyhow::{Result, bail};

use crate::config::{self, Config};
use crate::detect::Slot;
use crate::namespace::{self, Login};
use crate::paths::PandoPaths;

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
