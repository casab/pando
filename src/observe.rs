//! What a running process group is actually doing, and why it stopped.
//!
//! Two things the config cannot tell us. A dev server may pick a port pando
//! did not choose — a framework that ignores `PORT`, a second listener for
//! HMR — so the ports a worktree really serves on are scanned from its
//! process group's listening sockets. And when it dies, pando is no longer
//! its parent, so there is no exit status to read: the reason comes from the
//! last lines of the log.
//!
//! Every lookup here is best effort. A missing `lsof`, a denied scan, or a
//! group that has already gone yields an empty list rather than an error:
//! the assigned port is always a usable fallback, and a failure to observe
//! must never fail a command.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Long enough for `lsof` on a busy machine, short enough that a hung scan
/// never holds up a tick.
const SCAN_TIMEOUT: Duration = Duration::from_secs(3);

/// Every pid in the process group, including the leader.
///
/// The leader is a `bash -lc` that usually `exec`s away or exits early, so
/// the interesting pids are almost never the one pando recorded.
pub fn group_pids(pgid: i32) -> Vec<u32> {
    if pgid <= 0 {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    if let Some(pids) = proc_group_pids(pgid) {
        return pids;
    }
    let mut cmd = Command::new("ps");
    // `-axo pid=,pgid=` over `-g <pgid>`: one spelling that works on BSD and
    // GNU `ps` alike, and the filtering is ours rather than the tool's.
    cmd.args(["-axo", "pid=,pgid="]);
    match run_capturing(cmd, SCAN_TIMEOUT) {
        Some(text) => parse_ps_pgid(&text, pgid),
        None => Vec::new(),
    }
}

/// The TCP ports `pids` are listening on, paired with the pid that owns each.
pub fn listening_ports(pids: &[u32]) -> Vec<(u32, u16)> {
    if pids.is_empty() {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    {
        let mut cmd = Command::new("ss");
        // -H drops the header, -p adds the owning process, -n keeps ports
        // numeric so nothing has to be resolved.
        cmd.args(["-ltnpH"]);
        let Some(text) = run_capturing(cmd, SCAN_TIMEOUT) else {
            return Vec::new();
        };
        let mut found = parse_ss(&text);
        found.retain(|(pid, _)| pids.contains(pid));
        return found;
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
        match run_capturing(cmd, SCAN_TIMEOUT) {
            Some(text) => parse_lsof(&text),
            None => Vec::new(),
        }
    }
}

/// Every port the group listens on, sorted and deduplicated. The one call
/// the rest of pando makes.
pub fn observed_ports(pgid: i32) -> Vec<u16> {
    let pids = group_pids(pgid);
    let ports: BTreeSet<u16> = listening_ports(&pids).into_iter().map(|(_, p)| p).collect();
    ports.into_iter().collect()
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
                        "something else is listening on port {port} — stop it, or `pando stop` \
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
                hint: "something the app connects to is not running — start it, or point the \
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
                hint: "a compiled dependency was built against a different runtime version — \
                       reinstall it under the one this project uses"
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
                hint: "dependencies are missing or stale — the install step has not run here yet"
                    .to_string(),
            });
        }
    }
    None
}

/// The first port-looking number in a line, for the address-in-use hint.
/// Bounded to the ephemeral-and-above range so a timestamp or a pid is not
/// reported as a port.
fn port_in_text(text: &str) -> Option<u16> {
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
        if digits.len() >= 4
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
fn run_capturing(mut cmd: Command, timeout: Duration) -> Option<String> {
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
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let text = rx.recv_timeout(timeout).ok();
    if text.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = reader.join();
    text
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
}
