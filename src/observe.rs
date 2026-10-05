//! Why a process group stopped.
//!
//! When a dev server dies, pando is no longer its parent, so `waitpid` has
//! nothing to say: the reason is read back afterwards, from the last lines
//! of the log and from the status the shell recorded for itself on the way
//! out. Which ports a running group really listens on is the platform's to
//! scan: [`crate::platform::process::ports_by_group`].

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
