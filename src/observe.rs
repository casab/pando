//! What a running process group is actually doing, and why it stopped.
//!
//! Two things the config cannot tell us. A dev server may pick a port pando
//! did not choose — a framework that ignores `PORT`, a second listener for
//! HMR — so the ports a worktree really serves on are scanned from its
//! process group's listening sockets. And when it dies, pando is no longer
//! its parent, so `waitpid` has nothing to say: the reason is read back
//! afterwards, from the last lines of the log and from the status the
//! shell recorded for itself on the way out.
//!
//! Every lookup here is best effort. A missing `lsof`, a denied scan, or a
//! group that has already gone yields an empty list rather than an error:
//! the assigned port is always a usable fallback, and a failure to observe
//! must never fail a command.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

/// Long enough for `lsof` on a busy machine, short enough that a hung scan
/// never holds up a tick.
const SCAN_TIMEOUT: Duration = Duration::from_secs(3);

/// Every pid in the process group, including the leader.
///
/// The leader is a `bash -lc`. It waits for the process it started when
/// pando asked it to record an exit status, and otherwise `exec`s away or
/// exits early — so the interesting pids are not reliably the one pando
/// recorded, in either case.
pub fn group_pids(pgid: i32) -> Vec<u32> {
    group_pids_checked(pgid).unwrap_or_default()
}

/// [`group_pids`], distinguishing "no processes" from "the scan could not
/// run". `None` is the second: no `ps`, denied, or timed out.
pub fn group_pids_checked(pgid: i32) -> Option<Vec<u32>> {
    if pgid <= 0 {
        return Some(Vec::new());
    }
    #[cfg(target_os = "linux")]
    if let Some(pids) = proc_group_pids(pgid) {
        return Some(pids);
    }
    let text = ps_listing()?;
    Some(parse_ps_pgid(&text, pgid))
}

/// `ps -axo pid=,pgid=`, when it can be believed.
///
/// `-axo pid=,pgid=` over `-g <pgid>`: one spelling that works on BSD and
/// GNU `ps` alike, and the filtering is ours rather than the tool's.
fn ps_listing() -> Option<String> {
    let mut cmd = Command::new("ps");
    cmd.args(["-axo", "pid=,pgid="]);
    let (succeeded, text) = run_capturing_status(cmd, SCAN_TIMEOUT)?;
    trusted_ps(succeeded, text)
}

/// A listing only when `ps` succeeded and listed somebody: it always lists
/// itself, so an empty one is a `ps` that could not look — denied by a
/// sandbox, say — and not a machine with no processes. Taken at its word,
/// it said every group was empty, which reads as "nothing bound" rather
/// than as a scan that could not run.
fn trusted_ps(succeeded: bool, text: String) -> Option<String> {
    (succeeded && parse_ps_rows(&text).next().is_some()).then_some(text)
}

/// The TCP ports `pids` are listening on, paired with the pid that owns each.
pub fn listening_ports(pids: &[u32]) -> Vec<(u32, u16)> {
    listening_ports_checked(pids).unwrap_or_default()
}

/// [`listening_ports`], distinguishing "nothing is listening" from "the
/// scan could not run".
// The Linux block's `return` is needed on macOS, where the other block
// follows it; on Linux it is the last expression and clippy says so.
#[allow(clippy::needless_return)]
pub fn listening_ports_checked(pids: &[u32]) -> Option<Vec<(u32, u16)>> {
    if pids.is_empty() {
        return Some(Vec::new());
    }
    #[cfg(target_os = "linux")]
    {
        let mut cmd = Command::new("ss");
        // -H drops the header, -p adds the owning process, -n keeps ports
        // numeric so nothing has to be resolved.
        cmd.args(["-ltnpH"]);
        let text = run_capturing(cmd, SCAN_TIMEOUT)?;
        let mut found = parse_ss(&text);
        found.retain(|(pid, _)| pids.contains(pid));
        return Some(found);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut cmd = Command::new("lsof");
        // -F pn is the machine-readable form: one field per line, `p` for
        // the pid and `n` for the address. -P and -n keep ports and hosts
        // numeric, so nothing waits on DNS.
        cmd.args([
            "-a",
            "-p",
            &list,
            "-iTCP",
            "-sTCP:LISTEN",
            "-P",
            "-n",
            "-F",
            "pn",
        ]);
        let text = run_capturing(cmd, SCAN_TIMEOUT)?;
        Some(parse_lsof(&text))
    }
}

/// Every port the group listens on, sorted and deduplicated. The one call
/// the rest of pando makes.
pub fn observed_ports(pgid: i32) -> Vec<u16> {
    observed_ports_checked(pgid).unwrap_or_default()
}

/// [`observed_ports`], distinguishing a group that is listening on nothing
/// from a scan that could not run at all.
///
/// The difference decides readiness: an empty answer from a working scan
/// means "not up yet", while `None` means pando has to fall back to asking
/// the port itself.
pub fn observed_ports_checked(pgid: i32) -> Option<Vec<u16>> {
    let pids = group_pids_checked(pgid)?;
    let ports: BTreeSet<u16> = listening_ports_checked(&pids)?
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    Some(ports.into_iter().collect())
}

/// [`observed_ports_checked`] for several groups at once: one process
/// listing and one socket scan however many groups there are, rather than
/// two spawns apiece. Each group's answer means what the single-group
/// call's would: `None` when a scan could not run, an empty list when the
/// group listens on nothing.
pub fn observed_ports_by_group(pgids: &[i32]) -> BTreeMap<i32, Option<Vec<u16>>> {
    let wanted: BTreeSet<i32> = pgids.iter().copied().filter(|g| *g > 0).collect();
    let mut out: BTreeMap<i32, Option<Vec<u16>>> = pgids
        .iter()
        .map(|g| (*g, (*g <= 0).then(Vec::new)))
        .collect();
    if wanted.is_empty() {
        return out;
    }
    let Some(members) = all_group_pids(&wanted) else {
        return out;
    };
    let pids: Vec<u32> = members.values().flatten().copied().collect();
    let Some(listening) = listening_ports_checked(&pids) else {
        return out;
    };
    for (pgid, group) in &members {
        let ports: BTreeSet<u16> = listening
            .iter()
            .filter(|(pid, _)| group.contains(pid))
            .map(|(_, port)| *port)
            .collect();
        out.insert(*pgid, Some(ports.into_iter().collect()));
    }
    out
}

/// The members of each wanted group, from one listing. A wanted group with
/// no members maps to an empty list.
fn all_group_pids(wanted: &BTreeSet<i32>) -> Option<BTreeMap<i32, Vec<u32>>> {
    let mut members: BTreeMap<i32, Vec<u32>> = wanted.iter().map(|g| (*g, Vec::new())).collect();
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/proc").is_dir() {
        for pgid in wanted {
            // A `/proc` that cannot be listed is a scan that could not
            // run, not a group with nobody in it.
            members.insert(*pgid, proc_group_pids(*pgid)?);
        }
        return Some(members);
    }
    let text = ps_listing()?;
    for (pid, group) in parse_ps_rows(&text) {
        if let Some(list) = members.get_mut(&group) {
            list.push(pid);
        }
    }
    Some(members)
}

/// Every `pid pgid` row of `ps -axo pid=,pgid=`; rows that do not parse
/// are skipped.
fn parse_ps_rows(text: &str) -> impl Iterator<Item = (u32, i32)> + '_ {
    text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let group = fields.next()?.parse::<i32>().ok()?;
        Some((pid, group))
    })
}

/// `ps -axo pid=,pgid=` filtered to one group. Rows that do not parse are
/// skipped rather than failing the scan.
fn parse_ps_pgid(text: &str, pgid: i32) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(group)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(group)) = (pid.parse::<u32>(), group.parse::<i32>()) else {
            continue;
        };
        if group == pgid {
            out.push(pid);
        }
    }
    out
}

/// The process group from one `/proc/<pid>/stat` line.
///
/// Compiled and tested on every platform even though only Linux calls it: a
/// parser that only builds on the machine nobody develops on is a parser
/// that rots.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
///
/// `comm` is in parentheses and may itself contain spaces and parentheses,
/// so the fields are counted from the *last* `)` — splitting the whole line
/// on whitespace is the classic bug here.
fn parse_proc_stat_pgid(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After comm: state, ppid, pgrp.
    rest.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn proc_group_pids(pgid: i32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        // A process that exits mid-scan is normal, not an error.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if parse_proc_stat_pgid(&stat) == Some(pgid) {
            out.push(pid);
        }
    }
    Some(out)
}

/// `lsof -F pn` output: `p<pid>` starts a process block, `n<host>:<port>`
/// names one of its sockets. Other field letters (`f` for the descriptor)
/// appear whether or not they were asked for, and are ignored.
///
/// Compiled on Linux too, where `/proc` answers instead, for the same
/// reason as [`parse_proc_stat_pgid`] the other way round.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_lsof(text: &str) -> Vec<(u32, u16)> {
    let mut out = Vec::new();
    let mut current: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix('n')
            && let Some(pid) = current
            && let Some(port) = port_of(rest)
        {
            out.push((pid, port));
        }
    }
    out
}

/// `ss -ltnpH` rows: `LISTEN 0 511 0.0.0.0:17342 0.0.0.0:* users:(("node",pid=42,fd=20))`.
/// A row may name several processes, and a socket with no `users:` block
/// belongs to no pid this scan can claim.
///
/// Linux-only in practice, compiled everywhere so its tests run everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_ss(text: &str) -> Vec<(u32, u16)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // LISTEN, Recv-Q, Send-Q, Local Address:Port
        let Some(local) = fields.get(3) else { continue };
        let Some(port) = port_of(local) else { continue };
        for chunk in line.split("pid=").skip(1) {
            let digits: String = chunk.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                out.push((pid, port));
            }
        }
    }
    out
}

/// The port of a listening address. Handles `*:8080`, `127.0.0.1:8080`,
/// `[::1]:8080` and `*:*` — everything after the last colon, when it is a
/// number.
fn port_of(address: &str) -> Option<u16> {
    address.trim().rsplit_once(':')?.1.trim().parse().ok()
}

/// A recognised failure and the one line that tells the developer what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// What the log matched, short enough to sit in a status line.
    pub cause: &'static str,
    /// The fix, in one sentence.
    pub hint: String,
}

/// The four failures worth naming, matched against the last lines of a log.
///
/// Deliberately four. A large pattern library ages into wrong advice; these
/// four cover what actually stops a dev server on a developer's machine, and
/// anything else is shown as the raw log, which is honest.
pub fn classify_failure(last_lines: &[String]) -> Option<Hint> {
    // Newest first: the closing lines of a crash are the reason, and
    // everything above them is progress.
    for line in last_lines.iter().rev() {
        let lower = line.to_lowercase();
        if lower.contains("eaddrinuse") || lower.contains("address already in use") {
            return Some(Hint {
                cause: "port already in use",
                hint: match port_in_text(&lower) {
                    Some(port) => format!(
                        "something else is listening on port {port}; stop it, or `pando stop` \
                         the worktree that owns it"
                    ),
                    None => "something else is already listening on the port this process wanted"
                        .to_string(),
                },
            });
        }
        if lower.contains("econnrefused") || lower.contains("connection refused") {
            return Some(Hint {
                cause: "connection refused",
                hint: "something the app connects to is not running; start it, or point the \
                       app at one that is"
                    .to_string(),
            });
        }
        // Checked before "module not found": a native module built for
        // another runtime reports both, and the ABI message is the useful one.
        if lower.contains("node_module_version")
            || lower.contains("was compiled against a different")
            || lower.contains("invalid elf header")
        {
            return Some(Hint {
                cause: "native module built for another runtime",
                hint: "a compiled dependency was built against a different runtime version; \
                       reinstall it under the one this project uses"
                    .to_string(),
            });
        }
        // A shell's own words for a program that is not there: `sh: 1:
        // next: not found`, `bash: vite: command not found`, zsh's
        // `command not found: vite`. Almost always a tool the project's
        // dependencies provide, before they were installed.
        if lower.contains("command not found")
            || (lower.contains("sh:") && lower.ends_with(": not found"))
        {
            return Some(Hint {
                cause: "a command is not installed",
                hint: "a command it runs is not on PATH here — if it comes from the project's \
                       dependencies, the install step has not run; check `install` under \
                       [project] in pando.toml"
                    .to_string(),
            });
        }
        if lower.contains("module_not_found")
            || lower.contains("cannot find module")
            || lower.contains("modulenotfounderror")
            || lower.contains("no module named")
        {
            return Some(Hint {
                cause: "a dependency is missing",
                hint: "dependencies are missing or stale; the install step has not run here yet"
                    .to_string(),
            });
        }
    }
    None
}

/// What the *way* a process ended says, when its log says nothing.
///
/// Two facts and no diagnosis past them: the status it exited with, and
/// whether it wrote anything at all. Exiting 0 with an empty log is the
/// one combination worth a suggestion — a dev server that is running is
/// still running, and one that meant to fail says why — and it is exactly
/// what a guard line lifted out of a Makefile recipe does when the tool it
/// checks for is installed.
///
/// `None` when the log has something in it: then the log is the answer,
/// and [`classify_failure`] is what reads it.
///
/// `left_something_running` is the one thing that takes the suggestion
/// away, and it is why this takes three arguments rather than two. A
/// command that backgrounds the server and returns — `./app &`, the shape
/// a recipe line that ends in `&` has — exits 0 having printed nothing
/// while the server it started is up. Saying "wrong command" there would
/// be a confident lie; saying what is actually true is more use than
/// either.
///
/// The note follows the reason its caller already wrote — "process
/// exited with status 0" — so it never restates the status: the two
/// joined used to say "exited with status 0" twice in one line.
pub fn exit_note(
    code: Option<i32>,
    printed_anything: bool,
    left_something_running: bool,
) -> Option<String> {
    if printed_anything {
        // A clean exit is the one ending a log cannot explain: nothing in
        // it failed. A dev server stays up, so a command that exits 0 on
        // its own is not one — a build, a one-shot script, a guard line.
        return (code == Some(0) && !left_something_running).then(|| {
            "a dev server stays up, so this command is probably not the one that starts it"
                .to_string()
        });
    }
    Some(
        if left_something_running {
            "it printed nothing at all, and something it started is still running"
        } else if code == Some(0) {
            "it printed nothing at all; a dev server stays up, so a command that ends \
             straight away is usually not the one that starts it"
        } else {
            "it printed nothing at all, so there is no log to read"
        }
        .to_string(),
    )
}

/// The first port-looking number in a line, for the address-in-use hint.
/// Bounded to the ephemeral-and-above range so a timestamp or a pid is not
/// reported as a port.
///
/// A number written as a port — after a `:` (`:::3000`, `127.0.0.1:3000`)
/// or after the word `port` — is preferred to the first one on the line:
/// most logs open with a date, and the year passes every other test.
fn port_in_text(text: &str) -> Option<u16> {
    let addressed = ["port ", "port: ", "port=", ":"].iter().find_map(|mark| {
        text.match_indices(mark).find_map(|(at, _)| {
            let digits: String = text[at + mark.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            // At least 1024, as below: `10:05:07` is a time.
            digits.parse::<u16>().ok().filter(|port| *port >= 1024)
        })
    });
    addressed.or_else(|| first_port_like(text))
}

/// The first run of four or more digits on the line that fits a port.
fn first_port_like(text: &str) -> Option<u16> {
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !c.is_ascii_digit() {
            continue;
        }
        // Skip the rest of this run of digits, so "17342" is read once.
        let digits: String = text[i..].chars().take_while(char::is_ascii_digit).collect();
        for _ in 1..digits.len() {
            chars.next();
        }
        // `2026-09-23`, `2026/09/23`: a date's year, not a port.
        let dated = text[i + digits.len()..].starts_with(['-', '/']);
        if !dated
            && digits.len() >= 4
            && let Ok(port) = digits.parse::<u16>()
            && port >= 1024
        {
            return Some(port);
        }
    }
    None
}

/// Runs a command and returns its stdout, or `None` on any failure —
/// including a command that is not installed, or one that hangs.
///
/// A scan must never outlive its usefulness: the child is killed at the
/// deadline rather than waited on, so a stuck `lsof` (a hung network mount
/// is the classic cause) costs one tick and not the session.
fn run_capturing(cmd: Command, timeout: Duration) -> Option<String> {
    run_capturing_status(cmd, timeout).map(|(_, text)| text)
}

/// The programs a scan killed at its deadline that have not exited yet.
///
/// A killed child exits only once it is back out of the kernel, and `ps`
/// on a machine deep in swap, or `lsof` on a hung network mount, can stay
/// in there for minutes. A second copy started meanwhile gets stuck the
/// same way. So while one lingers, the next is not started, and the scan
/// is one that could not run, answered at once.
static LINGERING: Mutex<BTreeSet<OsString>> = Mutex::new(BTreeSet::new());

fn lingering() -> MutexGuard<'static, BTreeSet<OsString>> {
    LINGERING.lock().unwrap_or_else(|e| e.into_inner())
}

/// [`run_capturing`], with whether the command exited successfully. Its
/// output is decoded leniently: a stray invalid byte in one line used to
/// cost the whole listing, which `read_to_string` then leaves empty.
fn run_capturing_status(mut cmd: Command, timeout: Duration) -> Option<(bool, String)> {
    let program = cmd.get_program().to_os_string();
    if lingering().contains(&program) {
        return None;
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // Read on a thread: a child that fills the pipe while we wait on it, and
    // a wait that never returns, are the two ways this deadlocks otherwise.
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    let Ok(text) = rx.recv_timeout(timeout) else {
        // Killed, and reaped on a thread of its own rather than here. The
        // kill lands only when the child gets back out of the kernel, and
        // the pipe closes only when every process holding it has exited;
        // waiting for either made the deadline a suggestion, and on a machine
        // deep in swap `start --wait` sat on one `ps` for minutes.
        let _ = child.kill();
        lingering().insert(program.clone());
        thread::spawn(move || {
            let _ = child.wait();
            let _ = reader.join();
            lingering().remove(&program);
        });
        return None;
    };
    let status = child.wait();
    let _ = reader.join();
    Some((status.is_ok_and(|s| s.success()), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        Detached, python_listener, python3_available, spawn_guarded, wait_until,
    };
    use std::time::Duration;
    use tempfile::tempdir;

    // ---- parsers ---------------------------------------------------------

    #[test]
    fn ps_output_is_filtered_to_one_group() {
        let text = "  4242  4242\n  4243  4242\n  4244  1\nnonsense\n  x  y\n";
        assert_eq!(parse_ps_pgid(text, 4242), vec![4242, 4243]);
        assert_eq!(parse_ps_pgid(text, 1), vec![4244]);
        assert!(parse_ps_pgid(text, 999).is_empty());
        assert!(parse_ps_pgid("", 4242).is_empty());
    }

    // The comm field is attacker-chosen in the sense that any program can
    // name itself "(weird) thing)"; counting fields from the left is the
    // classic way to read the wrong number.
    #[test]
    fn a_proc_stat_with_parentheses_in_the_command_still_parses() {
        assert_eq!(
            parse_proc_stat_pgid("42 (node) S 1 4242 4242 0 -1 4194304"),
            Some(4242)
        );
        assert_eq!(
            parse_proc_stat_pgid("42 (my (odd) name) S 1 777 777 0 -1"),
            Some(777)
        );
        assert_eq!(parse_proc_stat_pgid("no parens here"), None);
        assert_eq!(parse_proc_stat_pgid("42 (node) S 1"), None);
    }

    #[test]
    fn lsof_fields_pair_each_address_with_the_process_above_it() {
        let text = "p4242\nf3\nn127.0.0.1:17342\nn*:17343\np4243\nf7\nn[::1]:17344\n";
        assert_eq!(
            parse_lsof(text),
            vec![(4242, 17_342), (4242, 17_343), (4243, 17_344)]
        );
    }

    #[test]
    fn lsof_output_with_no_process_line_yields_nothing() {
        assert!(parse_lsof("n127.0.0.1:17342\n").is_empty());
        assert!(parse_lsof("").is_empty());
        // `*:*` is a socket with no numeric port.
        assert!(parse_lsof("p1\nn*:*\n").is_empty());
    }

    #[test]
    fn ss_rows_yield_every_process_listed_for_the_socket() {
        let text = "LISTEN 0 511  0.0.0.0:17342 0.0.0.0:* users:((\"node\",pid=42,fd=20))\n\
                    LISTEN 0 4096 [::1]:17343   [::]:*    users:((\"a\",pid=7,fd=3),(\"b\",pid=8,fd=4))\n\
                    LISTEN 0 128  0.0.0.0:22    0.0.0.0:*\n";
        assert_eq!(
            parse_ss(text),
            vec![(42, 17_342), (7, 17_343), (8, 17_343)],
            "a socket with no users: block belongs to no pid we can claim"
        );
    }

    #[test]
    fn an_address_yields_its_port_in_every_spelling() {
        assert_eq!(port_of("127.0.0.1:17342"), Some(17_342));
        assert_eq!(port_of("*:80"), Some(80));
        assert_eq!(port_of("[::1]:17342"), Some(17_342));
        assert_eq!(port_of("0.0.0.0:*"), None);
        assert_eq!(port_of("garbage"), None);
    }

    // ---- the classifier --------------------------------------------------

    #[test]
    fn the_four_patterns_are_recognised() {
        let cases: [(&str, &str); 8] = [
            (
                "Error: listen EADDRINUSE: address already in use :::17342",
                "port already in use",
            ),
            (
                "OSError: [Errno 48] Address already in use",
                "port already in use",
            ),
            ("connect ECONNREFUSED 127.0.0.1:5432", "connection refused"),
            (
                "psycopg.OperationalError: connection refused",
                "connection refused",
            ),
            (
                "Error: Cannot find module 'next'",
                "a dependency is missing",
            ),
            (
                "ModuleNotFoundError: No module named 'django'",
                "a dependency is missing",
            ),
            (
                "Error: The module was compiled against a different Node.js version using \
                 NODE_MODULE_VERSION 127",
                "native module built for another runtime",
            ),
            (
                "Error: /app/x.node: invalid ELF header",
                "native module built for another runtime",
            ),
        ];
        for (line, cause) in cases {
            let hint = classify_failure(&[line.to_string()])
                .unwrap_or_else(|| panic!("no hint for {line:?}"));
            assert_eq!(hint.cause, cause, "{line:?}");
            assert!(!hint.hint.is_empty());
        }
    }

    #[test]
    fn an_unrecognised_log_gets_no_hint() {
        assert_eq!(classify_failure(&[]), None);
        assert_eq!(
            classify_failure(&["ready in 412ms".to_string(), "compiled".to_string()]),
            None
        );
    }

    #[test]
    fn the_last_matching_line_wins() {
        let lines = vec![
            "connect ECONNREFUSED 127.0.0.1:5432".to_string(),
            "Error: listen EADDRINUSE :::17342".to_string(),
        ];
        assert_eq!(
            classify_failure(&lines).unwrap().cause,
            "port already in use",
            "the closing line of a crash is the reason"
        );
    }

    // A native-module failure reports both "cannot find module" and the ABI
    // mismatch; the ABI line is the one that tells you what to do.
    #[test]
    fn an_abi_mismatch_beats_the_missing_module_it_also_reports() {
        let line = "Error: Cannot find module — NODE_MODULE_VERSION 127 was compiled against a \
                    different Node.js version";
        assert_eq!(
            classify_failure(&[line.to_string()]).unwrap().cause,
            "native module built for another runtime"
        );
    }

    #[test]
    fn the_address_in_use_hint_names_the_port_when_the_log_does() {
        let hint = classify_failure(&["listen EADDRINUSE: address already in use :::17342".into()])
            .unwrap();
        assert!(hint.hint.contains("17342"), "{}", hint.hint);
        let vague = classify_failure(&["Address already in use".into()]).unwrap();
        assert!(!vague.hint.contains("port 0"), "{}", vague.hint);
    }

    #[test]
    fn a_port_is_only_read_out_of_a_plausible_number() {
        assert_eq!(port_in_text("listen eaddrinuse :::17342"), Some(17_342));
        assert_eq!(port_in_text("errno 48 address already in use"), None);
        assert_eq!(port_in_text("no numbers at all"), None);
    }

    // Most logs open with a date, and the year passed every test a port
    // had to: the hint said "something else is listening on port 2026".
    #[test]
    fn a_timestamp_in_front_is_not_read_as_the_port() {
        for line in [
            "2026-09-23 10:05:07 error: listen eaddrinuse: address already in use :::3000",
            "[2026-09-23t10:05:07.123z] eaddrinuse 127.0.0.1:3000",
            "2026/09/23 10:05:07 port 3000 is in use: address already in use",
            "10:05:07 error while attempting to bind on address ('127.0.0.1', 3000): \
             address already in use",
        ] {
            assert_eq!(port_in_text(line), Some(3000), "{line}");
        }
    }

    // ---- the exit note ---------------------------------------------------

    #[test]
    fn a_silent_success_is_the_only_exit_worth_a_suggestion() {
        let zero = exit_note(Some(0), false, false).expect("a silent exit 0 is worth saying");
        assert!(zero.contains("printed nothing"), "{zero}");
        assert!(
            zero.contains("ends straight away"),
            "and says why that is a signal: {zero}"
        );

        let one = exit_note(Some(1), false, false).expect("a silent failure is still worth saying");
        assert!(one.contains("printed nothing"), "{one}");
        assert!(
            !one.contains("straight away"),
            "a command that failed loudly is not evidence about the command: {one}"
        );

        assert_eq!(
            exit_note(None, false, false),
            exit_note(Some(1), false, false),
            "with no status recorded, the silence is still all there is"
        );
    }

    /// A command that backgrounds the server and returns exits 0 having
    /// said nothing, and the server is fine. The suggestion would be a
    /// confident lie, so it is replaced by what is true.
    #[test]
    fn a_silent_success_that_left_something_running_is_not_accused() {
        let note = exit_note(Some(0), false, true).expect("still worth saying");
        assert!(
            !note.contains("not the one that starts it"),
            "nothing may be diagnosed over a group that is still up: {note}"
        );
        assert!(note.contains("still running"), "{note}");
    }

    #[test]
    fn a_process_that_printed_something_gets_no_note() {
        assert_eq!(exit_note(Some(1), true, false), None);
        assert_eq!(
            exit_note(None, true, true),
            None,
            "the log is the answer whenever there is one"
        );
    }

    // Except for a clean exit: nothing in the log failed, and a dev server
    // does not exit on its own.
    #[test]
    fn a_clean_exit_is_said_even_when_it_printed_something() {
        let note = exit_note(Some(0), true, false).expect("an exit 0 is worth saying");
        assert!(note.contains("dev server stays up"), "{note}");
        assert_eq!(
            exit_note(Some(0), true, true),
            None,
            "not over a group that is still up"
        );
    }

    // "process exited with status 0 — it exited with status 0 on its own —
    // …" in start, `ls`, status and doctor: the note follows a reason that
    // already names the status, and must not name it again.
    #[test]
    fn an_exit_note_never_restates_the_status_its_reason_carries() {
        for (code, printed, alive) in [
            (Some(0), true, false),
            (Some(0), false, false),
            (Some(0), false, true),
            (Some(1), false, false),
            (None, false, false),
        ] {
            let Some(note) = exit_note(code, printed, alive) else {
                continue;
            };
            let joined = format!("{} with status {} — {note}", crate::state::EXITED, 0);
            assert_eq!(joined.matches("status").count(), 1, "{joined}");
            assert!(!note.contains("exit"), "{note}");
        }
    }

    #[test]
    fn a_missing_command_points_at_the_install_step() {
        for line in [
            "sh: 1: next: not found",
            "bash: vite: command not found",
            "zsh: command not found: vite",
        ] {
            let hint = classify_failure(&[line.to_string()])
                .unwrap_or_else(|| panic!("no hint for {line:?}"));
            assert_eq!(hint.cause, "a command is not installed", "{line}");
            assert!(hint.hint.contains("install"), "{}", hint.hint);
        }
    }

    // ---- against real processes -----------------------------------------

    #[test]
    fn a_group_with_no_listener_observes_no_ports() {
        let dir = tempdir().unwrap();
        let child = spawn_guarded("sleep 30", dir.path(), &dir.path().join("log"));
        assert!(
            group_pids(child.pgid).contains(&child.pid),
            "the leader must be in its own group"
        );
        assert!(observed_ports(child.pgid).is_empty());
    }

    #[test]
    fn a_dead_group_observes_nothing_rather_than_failing() {
        assert!(group_pids(999_999).is_empty());
        assert!(observed_ports(999_999).is_empty());
        assert!(observed_ports(0).is_empty(), "pgid 0 is never a real group");
        assert!(listening_ports(&[]).is_empty());
    }

    #[test]
    fn a_child_that_binds_a_port_is_observed_on_it() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let dir = tempdir().unwrap();
        // A port the OS just handed out is free, and freeing it right before
        // the child binds it is as close to raceless as this gets.
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let child: Detached =
            spawn_guarded(&python_listener(port), dir.path(), &dir.path().join("log"));
        let observed = wait_until(Duration::from_secs(15), || {
            observed_ports(child.pgid).contains(&port)
        });
        assert!(
            observed,
            "the listener never showed up on {port}; log: {:?}",
            std::fs::read_to_string(dir.path().join("log"))
        );
        // The pid that owns it is in the group, not necessarily the leader:
        // `bash -lc` execs python, but a shell may fork instead.
        let pids = group_pids(child.pgid);
        let owners = listening_ports(&pids);
        assert!(
            owners
                .iter()
                .any(|&(pid, p)| p == port && pids.contains(&pid)),
            "the listening pid must belong to the group: {owners:?} vs {pids:?}"
        );
    }

    #[test]
    fn one_batched_scan_answers_each_group_as_the_single_scan_does() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let dir = tempdir().unwrap();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let child: Detached =
            spawn_guarded(&python_listener(port), dir.path(), &dir.path().join("log"));
        assert!(wait_until(Duration::from_secs(15), || {
            observed_ports(child.pgid).contains(&port)
        }));
        // A pgid nothing holds: an empty answer, not a failed one.
        let gone = i32::MAX - 7;
        let scans = observed_ports_by_group(&[child.pgid, gone, 0, child.pgid]);
        assert_eq!(scans.len(), 3);
        assert_eq!(scans[&child.pgid], observed_ports_checked(child.pgid));
        assert_eq!(scans[&gone], Some(Vec::new()));
        assert_eq!(scans[&0], Some(Vec::new()));
    }

    // A `ps` that was denied prints nothing to stdout and exits non-zero.
    // Its empty listing used to be read as every group being empty —
    // "nothing bound" — when what it means is that nobody could look.
    #[test]
    fn a_ps_that_could_not_look_is_a_failed_scan_not_an_empty_one() {
        assert_eq!(trusted_ps(false, String::new()), None);
        assert_eq!(trusted_ps(true, String::new()), None, "ps lists itself");
        assert_eq!(trusted_ps(true, "garbage\n".into()), None);
        assert_eq!(trusted_ps(false, "  1  1\n".into()), None);
        assert_eq!(
            trusted_ps(true, "  1  1\n 42 42\n".into()).as_deref(),
            Some("  1  1\n 42 42\n")
        );
        let mut failing = Command::new("sh");
        failing.args(["-c", "echo '1 1'; exit 1"]);
        assert_eq!(
            run_capturing_status(failing, Duration::from_secs(5)),
            Some((false, "1 1\n".to_string()))
        );
    }

    #[test]
    fn a_command_that_is_not_installed_yields_nothing() {
        let cmd = Command::new("pando-no-such-command-exists");
        assert_eq!(run_capturing(cmd, Duration::from_secs(1)), None);
    }

    #[test]
    fn a_hung_command_is_killed_at_the_deadline() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = std::time::Instant::now();
        assert_eq!(run_capturing(cmd, Duration::from_millis(200)), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a scan must not outlive its deadline"
        );
    }

    // A killed child that could not be reaped at once held the scan until
    // it could: the wait for it, and for its output to close, came after
    // the kill. On a machine deep in swap `ps` took minutes to die, and so
    // did `start --wait`. Here a child of the scan's own child holds the
    // output open until the test lets it go, which is the same wait.
    #[test]
    fn a_scan_whose_child_lingers_after_the_kill_still_ends_at_its_deadline() {
        let dir = tempdir().unwrap();
        let release = dir.path().join("release");
        let open = dir.path().join("open");
        std::fs::write(&open, "").unwrap();
        let held = dir.path().join("held");
        // A program no other test runs: what lingers is kept per program.
        // Given a path that exists, it answers at once; otherwise a child
        // of its own holds its output open until that path appears.
        let script = dir.path().join("lingering-scan");
        std::fs::write(
            &script,
            "#!/bin/sh\n[ -e \"$1\" ] && { echo done; exit 0; }\n\
             (echo held > \"$2\"; while [ ! -e \"$1\" ]; do sleep 0.05; done) &\n\
             exec sleep 30\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let scan = |gate: &std::path::Path, deadline: Duration| {
            let mut cmd = Command::new(&script);
            cmd.arg(gate).arg(&held);
            run_capturing(cmd, deadline)
        };
        let holding = || held.exists();
        let lingers = || lingering().contains(script.as_os_str());

        thread::scope(|s| {
            // On a thread, so that the bug fails this test instead of
            // hanging it: the scan answers at its deadline or not at all.
            let (tx, rx) = mpsc::channel();
            let gate = release.as_path();
            s.spawn(move || {
                loop {
                    let answer = scan(gate, Duration::from_millis(200));
                    // The kill can land before the shell has forked the
                    // child that holds the output. A run like that holds
                    // nothing and proves nothing, so the scan runs again.
                    crate::testutil::wait_until(Duration::from_secs(30), || {
                        holding() || !lingers()
                    });
                    let held = holding();
                    let _ = tx.send((answer, held));
                    if held {
                        return;
                    }
                }
            });
            let answered = loop {
                match rx.recv_timeout(Duration::from_secs(60)) {
                    Ok((answer, true)) => break Ok(answer),
                    Ok((_, false)) => continue,
                    Err(e) => break Err(e),
                }
            };
            // While the first is still there, a second is not started: had
            // it been, it would have answered "done".
            let again = scan(&open, Duration::from_secs(30));
            std::fs::write(&release, "").unwrap();
            assert_eq!(answered, Ok(None), "a scan must not outlive its deadline");
            assert_eq!(again, None, "nothing runs beside a scan still lingering");
        });

        // Once the first has gone, the program runs again.
        assert!(
            crate::testutil::wait_until(Duration::from_secs(30), || !lingers()),
            "the lingering child was reaped once it let go"
        );
        assert_eq!(
            scan(&open, Duration::from_secs(30)).as_deref(),
            Some("done\n")
        );
    }
}
