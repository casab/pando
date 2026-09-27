use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct SpawnOptions<'a> {
    pub shell_cmd: &'a str,
    pub cwd: &'a Path,
    pub log_file: &'a Path,
    pub env: &'a [(String, String)],
    /// Where the shell writes the exit status it ends with, for the
    /// callers that want one. See [`spawn_detached`].
    pub status_file: Option<&'a Path>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnResult {
    pub pid: u32,
    pub pgid: i32,
}

/// `bash -lc <shell_cmd>`: how pando runs every command, so it resolves
/// the runtimes a login shell does rather than whatever pando was started
/// with.
///
/// Under `cargo test` the shell gets an empty HOME of its own. A login
/// shell reads the developer's `~/.bash_profile`, which with nvm or conda
/// in it costs most of a second per shell, and a test that passes only
/// because of what that profile loads is testing the laptop, not pando.
pub(crate) fn login_shell(shell_cmd: &str) -> Command {
    let mut command = Command::new("bash");
    command.arg("-lc").arg(shell_cmd);
    #[cfg(test)]
    command.env("HOME", crate::testutil::shell_home());
    command
}

/// Starts the child `command` spawns in a session of its own: the leader of
/// a new process group, with no controlling terminal.
///
/// The one place pando asks for that, because it is `setsid` in the child
/// between fork and exec, so the child is forked rather than spawned, and
/// a fork has to be made safe first: [`settle_before_fork`].
pub(crate) fn new_session(command: &mut Command) -> &mut Command {
    settle_before_fork();
    // SAFETY: `setsid` is async-signal-safe, and it is all the closure does.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        });
    }
    command
}

/// Finishes libnotify's one-time set-up in this process before any fork,
/// so no fork can land in the middle of it.
///
/// On macOS a forked child runs libSystem's fork handlers before it execs,
/// and libnotify's reaches its globals through a once-gate. When another
/// thread is setting them up at the moment of the fork — the first FSEvents
/// watcher in a process does, the TUI's among them — the child finds the
/// gate held by a thread it does not have and is killed before `exec`
/// ("os_once_t is corrupt", in its crash report): a dev server, a tunnel or
/// a hook that never ran, with nothing in its log. The set-up happens once
/// per process, so finishing it here, on the thread about to fork, closes
/// the window for every fork after. Any libnotify call does it; this one
/// asks nothing of the daemon.
#[cfg(target_os = "macos")]
fn settle_before_fork() {
    LIBNOTIFY_SETTLED.call_once(|| {
        unsafe extern "C" {
            fn notify_is_valid_token(token: libc::c_int) -> bool;
        }
        // SAFETY: it takes any integer, and reads nothing but its argument
        // and libnotify's own globals.
        unsafe { notify_is_valid_token(0) };
    });
}

/// Done once [`settle_before_fork`] has run in this process.
#[cfg(target_os = "macos")]
static LIBNOTIFY_SETTLED: std::sync::Once = std::sync::Once::new();

/// Nothing to settle: the handlers that make a fork unsafe mid-set-up are
/// macOS's.
#[cfg(not(target_os = "macos"))]
fn settle_before_fork() {}

/// Starts a command in its own session, writing its output to a log file.
///
/// `status_file`, when given, is how a caller learns *how* the process
/// ended. `waitpid` has nothing to say by the time anything asks — `start`
/// returns and the process is reparented, and a pando that stays has
/// reaped it already (below) — so the shell records the status itself: an
/// `EXIT` trap that writes `$?` beside the log. The file is only ever as
/// good as the shell managing to run its trap, so every reader treats a
/// missing one as "not known" rather than as a failure.
///
/// The trap is also what stops bash from `exec`ing the command in its
/// place, so the recorded pid is the shell rather than the command. That
/// costs one process and changes nothing that reads it: the shell waits
/// for the command, so it is alive for exactly as long, and both are in
/// the group `stop` signals.
///
/// The process is reaped the moment it ends, by a thread that waits on its
/// pid for as long as this pando runs. That matters in the TUI, which is
/// the parent of everything it starts: an unreaped leader is a zombie that
/// keeps its group in being, and another pando cannot tell from outside
/// that the group is empty — macOS answers `EPERM` for it — so a `stop`
/// from the command line would wait out its whole grace for nothing. The
/// thread waits on this pid only; a wait on any child would take the
/// statuses other waits in the process are there for. A pando that exits
/// first leaves the process to be reparented, as before.
pub fn spawn_detached(opts: SpawnOptions<'_>) -> Result<SpawnResult> {
    if let Some(parent) = opts.log_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(opts.log_file)
        .with_context(|| format!("open log file {}", opts.log_file.display()))?;
    let err = out
        .try_clone()
        .context("clone log file handle for stderr")?;

    // `$?` inside an EXIT trap is the status the shell is exiting with,
    // including the one an explicit `exit` was given.
    //
    // The path goes in a variable rather than into the trap body: the body
    // is itself a quoted word, and a worktree is named after a branch,
    // which may contain the very quote that would end it.
    let recorded = opts.status_file.map(|status| {
        format!(
            "__pando_status={}\n\
             trap 'printf %s \"$?\" > \"$__pando_status\"' EXIT\n\
             {}",
            shell_quote(&status.to_string_lossy()),
            opts.shell_cmd
        )
    });
    let mut cmd = login_shell(recorded.as_deref().unwrap_or(opts.shell_cmd));
    cmd.current_dir(opts.cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);

    for (k, v) in opts.env {
        cmd.env(k, v);
    }

    // Full detachment: new session, new process group, no controlling tty.
    new_session(&mut cmd);

    let mut child = cmd.spawn().context("spawn detached bash")?;
    let pid = child.id();
    // After setsid() the child's pgid equals its pid (it becomes session leader).
    let pgid = pid as i32;
    // Waited on rather than dropped, so that it is reaped when it ends. The
    // status the wait returns goes nowhere: the status file is what says
    // how the process ended.
    let _ = std::thread::Builder::new()
        .name(format!("reap-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        });

    Ok(SpawnResult { pid, pgid })
}

/// The exit status a [`spawn_detached`] process recorded, once it has
/// ended. `None` is "nothing recorded one": no status file was asked for,
/// the process is still running, or its shell died before its trap could
/// run.
///
/// [`stop`] is the third case. Measured on bash 3.2 and 5.3, a group
/// `SIGTERM` or `SIGKILL` leaves no status behind — see
/// `a_stopped_process_never_records_a_clean_exit`, which asserts the part
/// that would actually mislead rather than the shell's exact behaviour: a
/// process pando ended is never reported as having finished cleanly.
pub fn recorded_exit_status(status_file: &Path) -> Option<i32> {
    std::fs::read_to_string(status_file)
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn is_alive(pid: u32) -> bool {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    // Reap zombie first if we're the parent (TUI session). Returns
    // ECHILD when pid isn't our child — fall through to signal probe.
    match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => return false,
        Ok(WaitStatus::StillAlive) => return true,
        _ => {}
    }
    match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => false,
    }
}

/// Whether any process is still in the group — the leader, a child it
/// forked, or a grandchild.
///
/// `kill(-pgid, 0)` and not `kill(leader, 0)`: a dev server's group leader is
/// a `bash -lc` that very often exits while the server it backgrounded keeps
/// running and keeps the port. Asking after the leader alone is how a stop
/// reports success over a process that is still serving.
pub fn group_alive(pgid: i32) -> bool {
    // 0 is "our own group" and 1 is "every process we may signal". Neither
    // is ever a worktree's process group, and both would be catastrophic to
    // pass to kill(2).
    if pgid <= 1 {
        return false;
    }
    match nix::sys::signal::kill(Pid::from_raw(-pgid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        // The group exists but holds something we may not signal.
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// What a captured command printed, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The last non-empty line of stderr — what a failing script actually
    /// complained about, without the build noise above it.
    pub fn last_stderr_line(&self) -> Option<&str> {
        self.stderr
            .lines()
            .map(str::trim)
            .rev()
            .find(|l| !l.is_empty())
    }
}

/// Runs a shell command to completion with its output captured and a hard
/// deadline.
///
/// Two differences from `Command::output()`, both of which matter for a
/// command a project supplied:
///
/// - **It leads its own process group.** A command that times out is killed
///   with everything it started, not just the shell that started them.
/// - **It cannot hang forever.** A script that waits on something that
///   never comes would otherwise block whatever asked for its answer, and
///   in the TUI that is a slot nothing can clear.
///
/// The pipes are drained on their own threads, so a command that prints
/// more than a pipe buffer holds does not deadlock against the wait — and
/// the deadline covers the drain as well as the wait, because anything the
/// shell backgrounded inherited its stdout and can hold the pipe open long
/// after the shell itself has exited. When that happens the group is
/// killed, which closes the pipes, and whatever was read by then is what
/// the caller gets.
pub fn run_captured(
    shell_cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<Captured> {
    let mut command = login_shell(shell_cmd);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    new_session(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("run {shell_cmd:?}"))?;
    // After `setsid` the child leads a group whose id is its pid.
    let pgid = child.id() as i32;

    // Shared rather than returned by the threads: when the deadline passes
    // with a pipe still open, what was read so far is still the answer, and
    // a `join` would be the very wait this is here to bound.
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_reader = {
        let into = Arc::clone(&out_buf);
        std::thread::spawn(move || drain_into(out_pipe, &into))
    };
    let err_reader = {
        let into = Arc::clone(&err_buf);
        std::thread::spawn(move || drain_into(err_pipe, &into))
    };

    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait().context("wait for the command")? {
            Some(status) => break status.code(),
            None if budget_over(deadline, None) => {
                // The group, not the child: the shell may have started
                // something that is the actual reason this is still here.
                let _ = stop(pgid, Duration::from_secs(1));
                let _ = child.wait();
                // The group is gone, so the pipes are closing; give the
                // readers that long and no longer.
                let settled = Instant::now() + DRAIN_SETTLE;
                wait_until(&out_reader, &err_reader, || Instant::now() >= settled);
                let stderr = text_of(&err_buf);
                // Not the command itself: a project's `auth_cmd` may hold a
                // literal credential, and a timeout must not be how it
                // reaches a terminal and a TUI status line. The caller adds
                // the context that names which command this was.
                bail!(
                    "the command was still running after {}s{}",
                    timeout.as_secs(),
                    match stderr.lines().map(str::trim).rev().find(|l| !l.is_empty()) {
                        Some(line) => format!(" — its last output was: {line}"),
                        None => String::new(),
                    }
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // The shell has exited, which is not the same as its pipes being
    // closed: anything it backgrounded inherited them. It gets the rest of
    // the budget, then the group goes the way a timed-out one does.
    let exited = Instant::now();
    if !wait_until(&out_reader, &err_reader, || {
        budget_over(deadline, Some(exited))
    }) {
        let _ = stop(pgid, Duration::from_secs(1));
        let settled = Instant::now() + DRAIN_SETTLE;
        wait_until(&out_reader, &err_reader, || Instant::now() >= settled);
    }
    Ok(Captured {
        code,
        stdout: text_of(&out_buf),
        stderr: text_of(&err_buf),
    })
}

/// How long the readers are given once the group they were reading from
/// has been killed. Closing a pipe is not instantaneous, and the bytes
/// already in it are the diagnosis.
const DRAIN_SETTLE: Duration = Duration::from_millis(500);

/// What a test on this thread said ends a command's budget. It is asked
/// with the moment the shell exited, or `None` while the shell is running;
/// once it has exited, only while something it started holds a pipe.
type BudgetOver = Box<dyn Fn(Option<Instant>) -> bool>;

thread_local! {
    static BUDGET_OVER_HERE: std::cell::RefCell<Option<BudgetOver>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether a command's budget is spent: its deadline has passed, or
/// `with_budget_over_when` is running on this thread and says it is.
/// `exited` is when the shell exited, if it has.
fn budget_over(deadline: Instant, exited: Option<Instant>) -> bool {
    Instant::now() >= deadline
        || BUDGET_OVER_HERE.with(|over| over.borrow().as_ref().is_some_and(|over| over(exited)))
}

/// Runs `f` with the budget of every [`run_captured`] it makes on this
/// thread also over as soon as `over` says so. For tests: a budget short
/// enough to wait out in one has to cover a login shell's start as well,
/// which takes milliseconds on an idle machine and has taken seconds on a
/// loaded one, and a command killed before it got to what the test is
/// about proves nothing about it. An event the test names ends the budget
/// instead, and the clock stays as the backstop. Per thread, so the tests
/// beside it keep the clock alone.
#[cfg(test)]
pub(crate) fn with_budget_over_when<R>(
    over: impl Fn(Option<Instant>) -> bool + 'static,
    f: impl FnOnce() -> R,
) -> R {
    let before = BUDGET_OVER_HERE.with(|o| o.replace(Some(Box::new(over))));
    // Put back on unwind too, so a failing test leaves nothing behind on
    // a thread the harness may reuse.
    struct Restore(Option<BudgetOver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BUDGET_OVER_HERE.with(|o| *o.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(before);
    f()
}

/// For [`with_budget_over_when`]: a budget that runs out `grace` after the
/// shell exits, however long the shell took to get there.
#[cfg(test)]
pub(crate) fn after_the_exit(grace: Duration) -> impl Fn(Option<Instant>) -> bool {
    move |exited| exited.is_some_and(|at| at.elapsed() >= grace)
}

/// Whether both readers finished before `over` said to stop waiting.
fn wait_until(
    out: &std::thread::JoinHandle<()>,
    err: &std::thread::JoinHandle<()>,
    over: impl Fn() -> bool,
) -> bool {
    loop {
        if out.is_finished() && err.is_finished() {
            return true;
        }
        if over() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reads a pipe to its end, publishing as it goes so a caller that gives up
/// waiting still has everything that arrived.
fn drain_into(pipe: Option<impl std::io::Read>, into: &Mutex<Vec<u8>>) {
    let Some(mut pipe) = pipe else {
        return;
    };
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => held(into).extend_from_slice(&chunk[..n]),
        }
    }
}

fn text_of(buf: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&held(buf)).into_owned()
}

/// The buffer, whether or not a thread panicked while holding it. A partial
/// read is still worth more than a panic in the caller.
fn held(buf: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    buf.lock().unwrap_or_else(|e| e.into_inner())
}

/// Single-quotes one word of a `bash -lc` command line, with any single
/// quote inside closed, escaped, and reopened.
///
/// Here rather than beside a caller because every command pando builds ends
/// up in [`SpawnOptions::shell_cmd`], and a home directory or a worktree
/// path with a space in it is ordinary: a word that is not quoted is a
/// command that runs something else.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// SIGTERM to the whole group, then SIGKILL to whatever is left after
/// `grace`. Returns once the group is empty, or once it has been SIGKILLed.
///
/// Signalling is unconditional, including for a process pando has already
/// written off as failed: a leader that exited is not a group that exited,
/// and skipping the signal is exactly how a backgrounded child outlives the
/// tool that started it.
pub fn stop(pgid: i32, grace: Duration) -> Result<()> {
    // Nor the group pando itself runs in. Nothing pando spawns is in it —
    // every spawn leads a session of its own — so a record naming it is a
    // number that has been handed out again, and the shell that ran this
    // pando leads its job's group: signalling it is pando killing itself,
    // and its terminal's job with it, halfway through a mutation.
    if pgid <= 1 || pgid == nix::unistd::getpgrp().as_raw() {
        return Ok(());
    }
    let group = Pid::from_raw(pgid);
    let any_in_group = Pid::from_raw(-pgid);

    match killpg(group, Signal::SIGTERM) {
        Ok(()) => {}
        // Nothing left in the group; there is nothing to wait for.
        Err(Errno::ESRCH) => {
            reap_group(any_in_group);
            return Ok(());
        }
        // macOS returns EPERM for a group whose only member is a zombie
        // leader. Falling through reaps it.
        Err(Errno::EPERM) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGTERM")),
    }

    let deadline = Instant::now() + grace;
    loop {
        // Reap first: a zombie child of ours still counts as a group member
        // to kill(2), so an unreaped leader would look alive forever.
        reap_group(any_in_group);
        if !group_alive(pgid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    match killpg(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGKILL")),
    }
    // SIGKILL is not instantaneous; the caller is about to reuse this
    // worktree's ports, so it is worth the few milliseconds to see it land.
    let hard_deadline = Instant::now() + KILL_SETTLE;
    loop {
        reap_group(any_in_group);
        if !group_alive(pgid) || Instant::now() >= hard_deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long to wait for SIGKILL to be delivered before giving up and
/// returning anyway.
const KILL_SETTLE: Duration = Duration::from_millis(500);

fn reap_group(any_in_group: Pid) {
    loop {
        match waitpid(Some(any_in_group), Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// How long a test waits for a login shell to get somewhere, before it
    /// gives up and fails. Only a failing test waits it out, so it is sized
    /// for a loaded machine rather than an idle one: a shell that starts in
    /// milliseconds alone has taken seconds with builds and other suites
    /// running beside it, and a test that fails for that says nothing about
    /// pando.
    const BACKSTOP: Duration = Duration::from_secs(60);

    /// The pid a script wrote to `file` with `echo`, once all of it is
    /// there: the newline is the last byte `echo` writes.
    fn pid_in(file: &Path) -> Option<u32> {
        std::fs::read_to_string(file)
            .ok()?
            .strip_suffix('\n')?
            .parse()
            .ok()
    }

    #[test]
    fn is_alive_returns_false_for_nonexistent_pid() {
        assert!(!is_alive(999_999));
    }

    #[test]
    fn a_captured_command_reports_what_it_printed_and_how_it_ended() {
        let dir = tempdir().unwrap();
        let captured = run_captured("echo out; echo err >&2", dir.path(), &[], BACKSTOP).unwrap();
        assert!(captured.success());
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "out");
        assert_eq!(captured.stderr.trim(), "err");
    }

    #[test]
    fn a_captured_command_that_fails_keeps_its_code_and_last_complaint() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "echo noise >&2; echo 'the real reason' >&2; exit 7",
            dir.path(),
            &[],
            BACKSTOP,
        )
        .unwrap();
        assert!(!captured.success());
        assert_eq!(captured.code, Some(7));
        assert_eq!(captured.last_stderr_line(), Some("the real reason"));
    }

    #[test]
    fn a_captured_command_runs_where_it_was_told_with_the_environment_it_was_given() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "pwd; printf 'SECRET=%s\\n' \"$SECRET\"",
            dir.path(),
            &[("SECRET".to_string(), "abc".to_string())],
            BACKSTOP,
        )
        .unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert!(
            captured.stdout.contains(&canonical.display().to_string()),
            "{}",
            captured.stdout
        );
        assert!(
            captured.stdout.contains("SECRET=abc"),
            "{}",
            captured.stdout
        );
    }

    // More than a pipe buffer holds. A wait that does not drain the pipes
    // deadlocks here rather than returning.
    #[test]
    fn a_captured_command_that_prints_a_lot_does_not_deadlock() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "for i in $(seq 1 5000); do echo 'a line of output that is not especially short'; done",
            dir.path(),
            &[],
            BACKSTOP,
        )
        .unwrap();
        assert!(captured.success());
        assert_eq!(captured.stdout.lines().count(), 5000);
    }

    // The whole reason this is not `Command::output()`: a script that hangs
    // must not hang the share that asked it a question — and killing the
    // shell alone would leave whatever it started behind.
    //
    // The budget runs out once the script has started what it hangs on,
    // however long its login shell took to get that far. A command killed
    // before it ran anything at all would prove nothing, and a clock short
    // enough to wait out here is one a loaded machine can spend entirely on
    // the shell's start.
    #[test]
    fn a_captured_command_that_hangs_is_killed_with_everything_it_started() {
        let dir = tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let child_started = {
            let pidfile = pidfile.clone();
            move |_| pid_in(&pidfile).is_some()
        };
        let err = with_budget_over_when(child_started, || {
            run_captured(
                &format!("sleep 300 & echo $! > {}; wait", pidfile.display()),
                dir.path(),
                &[],
                BACKSTOP,
            )
        })
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("still running after"),
            "{err:#}"
        );

        let child = pid_in(&pidfile).expect("the script wrote its child's pid");
        let deadline = Instant::now() + BACKSTOP;
        while is_alive(child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !is_alive(child),
            "the grandchild outlived the command that started it"
        );
    }

    // Finding 4. The deadline wrapped the wait and not the drain, so a
    // script that backgrounded anything — an ordinary thing for a session
    // minting script to do — held its stdout open after the shell exited
    // and blocked the caller for as long as that child lived: 45 s here,
    // an hour for `sleep 3600 &`, whatever the timeout said.
    //
    // The budget runs out half a second after the shell exits, however long
    // the shell took to get there. It used to be a two-second clock from
    // the spawn, which also had to cover a login shell's start: on a loaded
    // machine the start took longer, the shell was killed before it had
    // backgrounded anything, and the test failed at its `unwrap` without
    // having asked about the pipe at all. The half second is the reader's:
    // what the shell printed is in the pipe before it exits, and it is read
    // before the group is killed rather than raced against the kill.
    #[test]
    fn a_captured_command_is_bounded_when_a_background_child_holds_its_pipe() {
        let dir = tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let holds_for = Duration::from_secs(45);
        let started = Instant::now();
        let captured = with_budget_over_when(after_the_exit(Duration::from_millis(500)), || {
            run_captured(
                &format!(
                    "printf 'session=abc'; sleep {} & echo $! > {}",
                    holds_for.as_secs(),
                    pidfile.display()
                ),
                dir.path(),
                &[],
                BACKSTOP,
            )
        })
        .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < holds_for,
            "a backgrounded child held the pipe and the caller waited for it: {elapsed:?}"
        );
        assert!(captured.success(), "{captured:?}");
        assert_eq!(
            captured.stdout.trim(),
            "session=abc",
            "what was read before the budget ran out is still what the caller asked for"
        );

        let child = pid_in(&pidfile).expect("the script wrote its child's pid");
        // Half the child's life: long enough for a loaded machine to land
        // the kill, and short enough that the child being gone cannot be it
        // ending by itself.
        let deadline = started + holds_for / 2;
        while is_alive(child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !is_alive(child),
            "the group that outlasted its budget was not killed with it"
        );
    }

    // A `[share].auth_cmd` may hold a literal credential — printing one is
    // the shape of the simplest possible auth command — and a timeout must
    // not be how it reaches a terminal, a log, or a TUI status line.
    #[test]
    fn a_timed_out_command_is_not_quoted_back_in_the_message() {
        let dir = tempdir().unwrap();
        let err = run_captured(
            "printf 'session=a-literal-credential'; sleep 60",
            dir.path(),
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            !message.contains("a-literal-credential"),
            "the command text carried a credential into an error message: {message}"
        );
        assert!(message.contains("still running after"), "{message}");
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("/tmp/x y/pando"), "'/tmp/x y/pando'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn stop_is_idempotent_when_group_already_gone() {
        stop(999_999, Duration::from_millis(100)).unwrap();
        assert!(!group_alive(999_999));
    }

    // `kill(-0, …)` signals our own process group and `kill(-1, …)` signals
    // every process we are allowed to signal. Neither is ever a worktree.
    #[test]
    fn stop_refuses_the_process_group_ids_that_would_signal_ourselves() {
        assert!(!group_alive(0));
        assert!(!group_alive(1));
        assert!(!group_alive(-5));
        stop(0, Duration::from_millis(10)).unwrap();
        stop(1, Duration::from_millis(10)).unwrap();
        stop(-1, Duration::from_millis(10)).unwrap();
        // Our own group, which no spawn is ever in: a record naming it
        // is a reused number, and the signal would land on this process
        // and the job that ran it.
        stop(nix::unistd::getpgrp().as_raw(), Duration::from_millis(10)).unwrap();
        // Still here, and still runnable: we did not signal ourselves.
        assert!(is_alive(std::process::id()));
    }

    // The failure the origin tool had: a leader that exits leaves its
    // backgrounded child holding the port, and a stop that watches only the
    // leader reports success over a process that is still serving.
    #[test]
    fn stop_kills_a_child_whose_leader_already_exited() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            // bash backgrounds the sleep and exits immediately. The sleep
            // outlives every wait below, so the group is still there however
            // long the shell took to start.
            shell_cmd: "sleep 300 & exit 0",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();

        let deadline = Instant::now() + BACKSTOP;
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_alive(r.pid), "the leader should have exited on its own");
        assert!(
            group_alive(r.pgid),
            "the backgrounded child keeps the group alive"
        );

        // `stop` returns once the group is empty, so the grace is only
        // waited out if the child ignores SIGTERM, which a sleep does not.
        stop(r.pgid, BACKSTOP).unwrap();
        assert!(
            !group_alive(r.pgid),
            "stop must empty the group, not just outlive its leader"
        );
    }

    /// The whole point of the sidecar: by the time anything asks how a
    /// dev server ended, pando is not its parent and `waitpid` has nothing
    /// to say. The shell says instead.
    #[test]
    fn a_process_records_the_status_it_exited_with() {
        // A directory with a space and a quote in its name, because a
        // worktree is named after a branch and a branch can be called
        // anything.
        let dir = tempdir().unwrap();
        let odd = dir.path().join("it's here");
        std::fs::create_dir_all(&odd).unwrap();
        for (cmd, expected) in [("exit 0", 0), ("exit 3", 3), ("true", 0)] {
            let log = odd.join("log.txt");
            let status = crate::paths::exit_status_file(&log);
            let _ = std::fs::remove_file(&status);
            let r = spawn_detached(SpawnOptions {
                shell_cmd: cmd,
                cwd: dir.path(),
                log_file: &log,
                env: &[],
                status_file: Some(&status),
            })
            .unwrap();
            let deadline = Instant::now() + BACKSTOP;
            while recorded_exit_status(&status).is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                recorded_exit_status(&status),
                Some(expected),
                "{cmd:?} should have recorded {expected}"
            );
            // The trap writes the status and *then* the shell finishes
            // exiting, so the status file appearing does not mean the
            // process is already reaped. Asserting straight off that race
            // is a flake under load, which is how this one was found.
            let gone = Instant::now() + BACKSTOP;
            while is_alive(r.pid) && Instant::now() < gone {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!is_alive(r.pid), "{cmd:?} should be over");
        }
    }

    /// A status is recorded through an explicit `exit` as well as off the
    /// end of the script, because an `EXIT` trap runs for both — and the
    /// guard shape that started all of this is an explicit `exit`.
    #[test]
    fn an_explicit_exit_is_recorded_too() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let status = crate::paths::exit_status_file(&log);
        spawn_detached(SpawnOptions {
            shell_cmd: "command -v sh >/dev/null || { echo missing; exit 1; }",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: Some(&status),
        })
        .unwrap();
        let deadline = Instant::now() + BACKSTOP;
        while recorded_exit_status(&status).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(recorded_exit_status(&status), Some(0));
        assert!(
            !std::fs::read_to_string(&log).unwrap().contains("missing"),
            "the guard took its success branch and said nothing"
        );
    }

    /// A process pando ended did not end on its own, and must never be
    /// read as having exited 0 by itself. What the shell records for a
    /// fatal signal is its business — nothing here, on either bash on this
    /// machine — but a clean status is the one answer that would be a lie.
    #[test]
    fn a_stopped_process_never_records_a_clean_exit() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let status = crate::paths::exit_status_file(&log);
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "sleep 30",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: Some(&status),
        })
        .unwrap();
        assert!(is_alive(r.pid));
        stop(r.pgid, Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_ne!(
            recorded_exit_status(&status),
            Some(0),
            "a process pando signalled did not finish on its own"
        );
    }

    /// The TUI is the parent of everything it starts, and a leader it never
    /// reaps stays a zombie that keeps its group in being: a `stop` from
    /// another pando then waits out its whole grace on a group with nothing
    /// in it. Nothing here asks after the process, since `is_alive` would
    /// reap it: `ps` looks, from outside, as that `stop` would.
    #[test]
    fn a_detached_process_that_ends_leaves_no_zombie_behind() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "exit 0",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();
        let listed = || {
            let out = Command::new("ps")
                .args(["-o", "stat=", "-p", &r.pid.to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let deadline = Instant::now() + BACKSTOP;
        while !listed().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            listed(),
            "",
            "the ended leader is still in the process table"
        );
    }

    #[test]
    fn nothing_is_recorded_for_a_caller_that_did_not_ask() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "exit 7",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();
        let deadline = Instant::now() + BACKSTOP;
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            recorded_exit_status(&crate::paths::exit_status_file(&log)),
            None,
            "no sidecar is written beside a log nobody asked to pair one with"
        );
    }

    #[test]
    fn spawn_puts_child_in_its_own_process_group() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "sleep 30",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();

        let our_pgid = nix::unistd::getpgrp().as_raw();
        assert_ne!(
            r.pgid, our_pgid,
            "child pgid ({}) must differ from parent pgid ({})",
            r.pgid, our_pgid
        );

        assert!(is_alive(r.pid));
        stop(r.pgid, Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!is_alive(r.pid), "child should be dead after stop");
    }

    // The TUI's first FSEvents watcher sets libnotify up on a thread of its
    // own, and a fork made while it did was killed before `exec`: a tunnel
    // whose log stayed empty, one full run in fifteen. Settled before the
    // command can be spawned, the set-up is over before any fork starts.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_command_given_its_own_session_forks_only_once_libnotify_is_settled() {
        let mut command = Command::new("true");
        new_session(&mut command);
        assert!(LIBNOTIFY_SETTLED.is_completed());
    }

    // `pre_exec` is what makes std fork rather than spawn, and a fork that
    // skips `new_session` skips the settling that makes it safe. So the one
    // `pre_exec` in pando is the one in `new_session`.
    #[test]
    fn every_session_pando_starts_is_started_by_new_session() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Built rather than written out, so this file's own needle is not
        // one of the uses it finds.
        let needle = [".pre", "_exec("].concat();
        let mut uses = Vec::new();
        let mut dirs = vec![root.join("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let rel = path.strip_prefix(root).unwrap().display().to_string();
                    for (n, line) in text.lines().enumerate() {
                        if line.contains(&needle) {
                            uses.push(format!("{rel}:{}", n + 1));
                        }
                    }
                }
            }
        }
        assert_eq!(uses.len(), 1, "{uses:?}");
        assert!(uses[0].starts_with("src/process.rs:"), "{uses:?}");
    }

    #[test]
    fn spawn_redirects_stdout_to_log_file() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "echo hello-from-child && echo err-line >&2",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();

        let deadline = Instant::now() + BACKSTOP;
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));

        let content = std::fs::read_to_string(&log).unwrap();
        assert!(
            content.contains("hello-from-child"),
            "log missing stdout: {content:?}"
        );
        assert!(
            content.contains("err-line"),
            "log missing stderr: {content:?}"
        );
    }

    #[test]
    fn stop_terminates_quickly_with_sigterm_when_child_cooperates() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "sleep 30",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
            status_file: None,
        })
        .unwrap();

        // Returning before the grace is up is what says SIGTERM was enough:
        // past it `stop` sends SIGKILL. How long SIGTERM takes to land is
        // the machine's load, not pando, so the grace is long and the
        // assertion is against it rather than against a guess.
        let grace = BACKSTOP;
        let start = Instant::now();
        stop(r.pgid, grace).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < grace,
            "sleep should die on SIGTERM within the grace window; took {elapsed:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(!is_alive(r.pid));
    }

    #[test]
    fn spawn_writes_env_vars_into_child_environment() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "printf 'WEB=%s API=%s' \"$WEB_PORT\" \"$API_PORT\"",
            cwd: dir.path(),
            log_file: &log,
            env: &[
                ("WEB_PORT".into(), "17224".into()),
                ("API_PORT".into(), "17225".into()),
            ],
            status_file: None,
        })
        .unwrap();

        let deadline = Instant::now() + BACKSTOP;
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));

        let content = std::fs::read_to_string(&log).unwrap();
        assert!(
            content.contains("WEB=17224") && content.contains("API=17225"),
            "env not passed through: {content:?}"
        );
    }
}
