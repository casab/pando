//! The shell pando runs command strings in.
//!
//! A `Result` on every OS, because finding the shell can be a question: a
//! native Windows build will use Git's own bash, found from git itself,
//! never the `bash` on PATH, which there is WSL's launcher.

use std::io;
use std::process::Command;

/// `bash -lc <shell_cmd>`: how pando runs every command, so it resolves
/// the runtimes a login shell does rather than whatever pando was started
/// with.
///
/// Under `cargo test` the shell gets an empty HOME of its own. A login
/// shell reads the developer's `~/.bash_profile`, which with nvm or conda
/// in it costs most of a second per shell, and a test that passes only
/// because of what that profile loads is testing the laptop, not pando.
pub fn login(shell_cmd: &str) -> io::Result<Command> {
    let mut command = Command::new("bash");
    command.arg("-lc").arg(shell_cmd);
    #[cfg(test)]
    command.env("HOME", crate::testutil::shell_home());
    Ok(command)
}

/// A POSIX `sh`, for a script pando writes itself or one a tool ships: the
/// caller adds `-c` and the command, or the script's path. Never a login
/// shell: nothing the developer's profile sets is read.
pub fn posix() -> io::Result<Command> {
    Ok(Command::new("/bin/sh"))
}
