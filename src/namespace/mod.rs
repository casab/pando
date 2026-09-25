//! Namespaces: a worktree's own database, or its own numbered slot, inside
//! a server the main checkout already runs.
//!
//! The third mode. Shared hands a worktree the main checkout's servers and
//! their data; isolated starts servers of the worktree's own. Namespaced
//! keeps the servers and separates the data: `northwind_traders__feat_x`
//! beside `northwind_traders` in the same MariaDB, slot 3 beside slot 0 in
//! the same Redis. A branch's migrations and rows stay its own, at shared's
//! cost — no server to start, no data directory, no wait.
//!
//! The price is that pando now writes into a server the developer owns, so
//! this module is mostly about not getting that wrong:
//!
//! - **A name can only ever be a worktree's.** [`database_names`] derives
//!   it from the main database's name with [`MARKER`] after it, so it can
//!   never be the main one and can never fall outside the prefix a
//!   developer grants the app's login.
//! - **Only what pando made is ever dropped.** [`may_drop`] is the one
//!   gate every drop and every flush goes through.
//! - **A password is never printed.** A [`Login`] keeps it private and
//!   hands it to the engine's client in its environment, never on a
//!   command line.

mod guard;
mod login;
mod name;

pub use guard::{describe, may_drop, same_namespace};
pub use login::{
    Login, find as find_login, from_config as login_from_config,
    from_env_files as login_from_env_files,
};
pub use name::{MARKER, MAX_NAME, database_names, is_plain};

#[cfg(test)]
mod tests;
