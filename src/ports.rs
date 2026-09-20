//! Deterministic port allocation.
//!
//! Each worktree gets a base port derived from a hash of the project id plus
//! the worktree name, then one consecutive port per role. Bases step by
//! [`BASE_STEP`] so a worktree that grows from one role to four rarely walks
//! into a neighbour's base, and the project id is in the hash so two repos
//! with the same branch name do not collide by default.

pub const PORT_MIN: u16 = 17_000;
pub const PORT_MAX: u16 = 56_998;

/// Gap between consecutive bases: enough room for a web port plus an api,
/// database, and cache port without reaching the next worktree's base.
pub const BASE_STEP: u16 = 8;

/// How many bases fit in the range with a full [`BASE_STEP`] window each, so
/// every port a base can hand out stays inside `PORT_MIN..=PORT_MAX`.
const BASE_COUNT: u32 = (PORT_MAX as u32 - PORT_MIN as u32 + 1) / BASE_STEP as u32;

/// Bases tried before giving up. A caller that exhausts this has ~400 ports
/// bound in its neighbourhood and wants a real error, not a longer walk.
const MAX_PROBE_ATTEMPTS: u32 = 50;

/// Separator between the hash inputs, so `("ab", "c")` and `("a", "bc")`
/// cannot hash to the same base.
const HASH_SEPARATOR: u8 = 0x1f;

/// Whether nothing is listening on `port`.
///
/// Three addresses, because one bind answers only part of the question. A
/// listener on `127.0.0.1` leaves `0.0.0.0` bindable and the other way
/// round — verified on macOS — and a listener on `[::1]` leaves both IPv4
/// addresses bindable, which is where `listen(port, "localhost")` on Node
/// and `runserver [::1]:8000` land.
///
/// A server bound to one non-loopback interface is still missed; the
/// observed-port scan is what catches those.
///
/// This binds the port for an instant, so it is only ever used where taking
/// the port is the point: reserving one. Readiness is answered by
/// [`something_is_listening`] and by scanning the process group, because a
/// probe that takes the port can hand the server it is waiting for an
/// `EADDRINUSE`.
pub fn is_port_free(port: u16) -> bool {
    can_bind("0.0.0.0", port) && can_bind("127.0.0.1", port) && v6_loopback_free(port)
}

fn can_bind(host: &str, port: u16) -> bool {
    std::net::TcpListener::bind((host, port)).is_ok()
}

/// Whether `[::1]:port` is free, on a machine that has an IPv6 loopback.
///
/// Only `AddrInUse` counts as taken: a host without IPv6 answers every bind
/// with `AddrNotAvailable` or `AfNoSupport`, and reading that as "occupied"
/// would leave pando with no ports at all.
fn v6_loopback_free(port: u16) -> bool {
    match std::net::TcpListener::bind(("::1", port)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::AddrInUse,
    }
}

/// How long a readiness connection waits before calling the port closed.
/// Local, so anything slower than this is not a listener that is up.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether something accepts a connection on `port`, over either loopback.
///
/// The readiness question asked without taking anything: a refused
/// connection means not ready, and a successful one is released at once.
/// Used only where the process-group scan could not run — there is no
/// `lsof`, or it was denied — since a connection says nothing about *whose*
/// listener answered.
pub fn something_is_listening(port: u16) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
    for host in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        if TcpStream::connect_timeout(&SocketAddr::new(host, port), CONNECT_TIMEOUT).is_ok() {
            return true;
        }
    }
    false
}

/// The base port for a worktree, before any occupancy probing.
pub fn derive_base(project_id: &str, name: &str) -> u16 {
    let mut input = Vec::with_capacity(project_id.len() + name.len() + 1);
    input.extend_from_slice(project_id.as_bytes());
    input.push(HASH_SEPARATOR);
    input.extend_from_slice(name.as_bytes());
    let digest = md5::compute(&input);
    let num = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let base = (num % BASE_COUNT) * BASE_STEP as u32 + PORT_MIN as u32;
    base as u16
}

/// `n` consecutive free ports at or after `base`, walking the base grid.
///
/// `is_free` must combine the OS probe with the ports already recorded in
/// state for other worktrees of the project: a stopped worktree still owns
/// its ports, and the OS probe alone would hand them to someone else.
pub fn reserve(base: u16, n: usize, is_free: impl Fn(u16) -> bool) -> Option<Vec<u16>> {
    if n == 0 {
        return Some(Vec::new());
    }
    let mut candidate = align_to_grid(base);
    for _ in 0..MAX_PROBE_ATTEMPTS {
        if let Some(window) = window_at(candidate, n)
            && window.iter().all(|&p| is_free(p))
        {
            return Some(window);
        }
        candidate = next_base(candidate, n);
    }
    None
}

/// The `n` ports starting at `base`, or `None` when they would run past
/// `PORT_MAX`.
fn window_at(base: u16, n: usize) -> Option<Vec<u16>> {
    let last = base as u32 + n as u32 - 1;
    if last > PORT_MAX as u32 {
        return None;
    }
    Some((0..n as u16).map(|i| base + i).collect())
}

fn align_to_grid(port: u16) -> u16 {
    let clamped = port.clamp(PORT_MIN, PORT_MAX);
    let offset = (clamped - PORT_MIN) % BASE_STEP;
    clamped - offset
}

/// The next base to try, wrapping to `PORT_MIN` once a window of `n` no
/// longer fits below `PORT_MAX`.
fn next_base(base: u16, n: usize) -> u16 {
    let next = base as u32 + BASE_STEP as u32;
    if next + n as u32 - 1 > PORT_MAX as u32 {
        PORT_MIN
    } else {
        next as u16
    }
}

/// Ports for one worktree's roles, and whether they had to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub ports: std::collections::BTreeMap<String, u16>,
    /// True when ports this worktree owned were taken and a new window had
    /// to be found. The caller says so, because a URL the developer had
    /// bookmarked has just changed.
    pub reassigned: bool,
}

/// Assigns a port per role for `name`, recording them in state.
///
/// Ports are stable per worktree: once assigned they are reused on every
/// later start, so a bookmarked URL keeps working across a stop. They move
/// only when something else has taken them in the meantime.
///
/// The freeness test is the OS probe *and* "not recorded for another
/// worktree of this project" — a stopped worktree still owns its ports, and
/// the OS probe alone would hand them to the next caller.
///
/// The worktree's record must already exist: only the caller knows its path
/// and whether pando created it.
pub fn assign(
    paths: &crate::paths::PandoPaths,
    store: &mut crate::state::State,
    name: &str,
    roles: &[String],
) -> anyhow::Result<Assignment> {
    assign_with(paths, store, name, roles, is_port_free)
}

/// [`assign`] with the host probe injected.
///
/// Tests drive this one: [`is_port_free`] answers by binding the port for
/// an instant, so two tests probing at the same moment can each see the
/// other's probe and conclude the port is taken. That is a real property of
/// the probe, not a bug — but it makes "the same ports come back" flaky to
/// assert, so the assertions use a probe that does not move.
pub fn assign_with(
    paths: &crate::paths::PandoPaths,
    store: &mut crate::state::State,
    name: &str,
    roles: &[String],
    is_free: impl Fn(u16) -> bool,
) -> anyhow::Result<Assignment> {
    use anyhow::Context as _;
    use std::collections::{BTreeMap, HashSet};

    if roles.is_empty() {
        if let Some(record) = store.worktrees.get_mut(name) {
            record.ports.clear();
        }
        return Ok(Assignment {
            ports: BTreeMap::new(),
            reassigned: false,
        });
    }

    let taken_by_others: HashSet<u16> = store
        .worktrees
        .iter()
        .filter(|(other, _)| other.as_str() != name)
        .flat_map(|(_, record)| record.ports.values().copied())
        .collect();

    let record = store
        .worktrees
        .get_mut(name)
        .with_context(|| format!("no state record for {name}"))?;

    // Reuse only when the recorded set is exactly the roles being asked for:
    // a worktree that grew a second role gets one consecutive window rather
    // than a port here and a port there.
    let recorded: Vec<u16> = roles
        .iter()
        .filter_map(|role| record.ports.get(role).copied())
        .collect();
    if recorded.len() == roles.len() && record.ports.len() == roles.len() {
        let usable = recorded
            .iter()
            .all(|p| is_free(*p) && !taken_by_others.contains(p));
        if usable {
            return Ok(Assignment {
                ports: record.ports.clone(),
                reassigned: false,
            });
        }
    }

    let had_ports = !record.ports.is_empty();
    let base = derive_base(paths.project_id(), name);
    let window = reserve(base, roles.len(), |port| {
        is_free(port) && !taken_by_others.contains(&port)
    })
    .with_context(|| {
        format!(
            "no free run of {} ports for {name} in {PORT_MIN}..={PORT_MAX}",
            roles.len()
        )
    })?;

    let ports: BTreeMap<String, u16> = roles.iter().cloned().zip(window).collect();
    record.ports = ports.clone();
    Ok(Assignment {
        ports,
        reassigned: had_ports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const PROJECT: &str = "acme-shop-3f9a2c1d";

    #[test]
    fn derive_base_is_deterministic() {
        assert_eq!(
            derive_base(PROJECT, "feat+checkout"),
            derive_base(PROJECT, "feat+checkout")
        );
        assert_ne!(
            derive_base(PROJECT, "feat+checkout"),
            derive_base(PROJECT, "feat+cart")
        );
    }

    #[test]
    fn bases_sit_on_the_grid_and_inside_the_range() {
        for name in [
            "a",
            "b",
            "feat+foo",
            "v4.1",
            "",
            "a-very-long-worktree-name",
        ] {
            let base = derive_base(PROJECT, name);
            assert_eq!(
                (base - PORT_MIN) % BASE_STEP,
                0,
                "{name}: base {base} is off the {BASE_STEP}-port grid"
            );
            assert!(
                (PORT_MIN..=PORT_MAX).contains(&base),
                "{name}: base {base} out of range"
            );
            let last = base + BASE_STEP - 1;
            assert!(
                last <= PORT_MAX,
                "{name}: the base's full window ends at {last}, past PORT_MAX"
            );
        }
    }

    // Without the project id in the hash, two repositories with a branch of
    // the same name would fight over one port.
    #[test]
    fn the_same_name_in_two_projects_gets_different_bases() {
        assert_ne!(
            derive_base("acme-shop-3f9a2c1d", "feat+checkout"),
            derive_base("acme-shop-11111111", "feat+checkout")
        );
    }

    #[test]
    fn the_hash_separator_keeps_split_points_distinct() {
        assert_ne!(derive_base("ab", "c"), derive_base("a", "bc"));
    }

    // A listener on `[::1]` leaves both IPv4 addresses bindable, so a probe
    // that only tries those hands out a port that is already taken.
    #[test]
    fn a_port_held_by_an_ipv6_only_listener_is_not_free() {
        let Ok(listener) = std::net::TcpListener::bind(("::1", 0)) else {
            eprintln!("skipping: no IPv6 loopback on this machine");
            return;
        };
        let port = listener.local_addr().unwrap().port();
        assert!(
            !is_port_free(port),
            "{port} is held by an IPv6 listener and must not be handed out"
        );
        drop(listener);
        assert!(is_port_free(port), "and it is free again once that goes");
    }

    // A machine with no IPv6 at all must not have every port read as taken.
    #[test]
    fn a_free_port_is_free_whatever_this_machine_thinks_of_ipv6() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(is_port_free(port));
    }

    #[test]
    fn reserve_returns_the_base_window_when_free() {
        assert_eq!(reserve(17_000, 2, |_| true), Some(vec![17_000, 17_001]));
        assert_eq!(reserve(17_000, 1, |_| true), Some(vec![17_000]));
        assert_eq!(reserve(17_000, 0, |_| true), Some(vec![]));
    }

    #[test]
    fn reserve_steps_a_whole_base_past_an_occupied_port() {
        let busy: HashSet<u16> = [17_001].into_iter().collect();
        assert_eq!(
            reserve(17_000, 2, |p| !busy.contains(&p)),
            Some(vec![17_008, 17_009]),
            "a collision moves to the next base, not the next port"
        );
    }

    #[test]
    fn reserve_walks_past_several_occupied_bases() {
        let busy: HashSet<u16> = [17_000, 17_008, 17_016].into_iter().collect();
        assert_eq!(
            reserve(17_000, 4, |p| !busy.contains(&p)),
            Some(vec![17_024, 17_025, 17_026, 17_027])
        );
    }

    #[test]
    fn reserve_aligns_an_off_grid_start_down_to_the_grid() {
        assert_eq!(reserve(17_005, 2, |_| true), Some(vec![17_000, 17_001]));
    }

    #[test]
    fn reserve_wraps_at_the_upper_bound() {
        let last_base = PORT_MIN + (BASE_COUNT as u16 - 1) * BASE_STEP;
        let busy: HashSet<u16> = (last_base..=PORT_MAX).collect();
        assert_eq!(
            reserve(last_base, 2, |p| !busy.contains(&p)),
            Some(vec![PORT_MIN, PORT_MIN + 1])
        );
    }

    #[test]
    fn reserve_gives_up_after_the_attempt_cap() {
        assert_eq!(reserve(17_000, 2, |_| false), None);
    }

    #[test]
    fn reserved_ports_never_leave_the_range() {
        // Sweep every base a name can hash to, including the last one, and
        // ask for more ports than a base's own window holds.
        for k in [0u32, 1, BASE_COUNT / 2, BASE_COUNT - 2, BASE_COUNT - 1] {
            let base = PORT_MIN + (k as u16) * BASE_STEP;
            for n in [1usize, 2, 4, 8, 12] {
                let ports = reserve(base, n, |_| true)
                    .unwrap_or_else(|| panic!("base {base} n {n} found nothing"));
                assert_eq!(ports.len(), n);
                for p in ports {
                    assert!(
                        (PORT_MIN..=PORT_MAX).contains(&p),
                        "base {base} n {n} produced {p}, outside the range"
                    );
                }
            }
        }
    }

    #[test]
    fn reserved_ports_are_consecutive() {
        let ports = reserve(derive_base(PROJECT, "feat+x"), 4, |_| true).unwrap();
        for pair in ports.windows(2) {
            assert_eq!(pair[1], pair[0] + 1);
        }
    }

    // ---- assign ----------------------------------------------------------

    fn assign_fixture() -> (tempfile::TempDir, crate::paths::PandoPaths) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        let project = crate::project::ProjectRef::from_root(&root).unwrap();
        let paths = crate::paths::PandoPaths::new(dir.path().join("pando-home"), project);
        (dir, paths)
    }

    fn with_record(store: &mut crate::state::State, name: &str) {
        store.worktrees.insert(
            name.to_string(),
            crate::state::WorktreeRecord::new(format!("/tmp/{name}"), true),
        );
    }

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A host with nothing bound on it, so an assertion about which ports
    /// come back is about pando's own rules and not about the machine.
    fn all_free(_: u16) -> bool {
        true
    }

    #[test]
    fn assign_records_a_port_per_role_and_reuses_it() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        with_record(&mut store, "feat+one");

        let first = assign_with(
            &paths,
            &mut store,
            "feat+one",
            &roles(&["web", "api"]),
            all_free,
        )
        .unwrap();
        assert_eq!(first.ports.len(), 2);
        assert!(!first.reassigned);
        assert_eq!(first.ports["api"], first.ports["web"] + 1, "consecutive");
        assert_eq!(
            store.worktrees["feat+one"].ports, first.ports,
            "the assignment is recorded, so a stopped worktree keeps its ports"
        );

        let second = assign_with(
            &paths,
            &mut store,
            "feat+one",
            &roles(&["web", "api"]),
            all_free,
        )
        .unwrap();
        assert_eq!(second.ports, first.ports, "ports are stable per worktree");
        assert!(!second.reassigned);
    }

    // The OS probe alone would hand a stopped worktree's ports to the next
    // caller, and the URL the developer had open would start serving someone
    // else's branch.
    #[test]
    fn a_second_worktree_never_takes_the_ports_another_one_owns() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        with_record(&mut store, "feat+one");
        with_record(&mut store, "feat+two");

        let one = assign_with(&paths, &mut store, "feat+one", &roles(&["web"]), all_free).unwrap();
        store.worktrees.get_mut("feat+two").unwrap().ports.clear();
        let two = assign_with(&paths, &mut store, "feat+two", &roles(&["web"]), all_free).unwrap();
        assert_ne!(one.ports["web"], two.ports["web"]);

        // And explicitly: a worktree asking for a port already recorded
        // elsewhere is moved off it.
        let stolen = one.ports["web"];
        store
            .worktrees
            .get_mut("feat+two")
            .unwrap()
            .ports
            .insert("web".to_string(), stolen);
        let again =
            assign_with(&paths, &mut store, "feat+two", &roles(&["web"]), all_free).unwrap();
        assert_ne!(
            again.ports["web"], stolen,
            "a port recorded for another worktree is not free"
        );
        assert!(again.reassigned);
    }

    #[test]
    fn a_recorded_port_that_something_else_bound_moves_and_says_so() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        with_record(&mut store, "feat+one");

        // Bind first and record that port as this worktree's, rather than
        // assigning and then racing to bind what was just probed free — the
        // range overlaps the OS ephemeral range, so that race is real.
        let squatter = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let held = squatter.local_addr().unwrap().port();
        store
            .worktrees
            .get_mut("feat+one")
            .unwrap()
            .ports
            .insert("web".to_string(), held);

        let moved = assign(&paths, &mut store, "feat+one", &roles(&["web"])).unwrap();
        assert_ne!(moved.ports["web"], held, "a bound port is not reusable");
        assert!(
            (PORT_MIN..=PORT_MAX).contains(&moved.ports["web"]),
            "and the real host probe is what `assign` uses"
        );
        assert!(
            moved.reassigned,
            "a moved port is worth telling the user about"
        );
        drop(squatter);
    }

    // A worktree that grows a role gets one consecutive window rather than
    // its old port plus whatever happened to be next to it.
    #[test]
    fn adding_a_role_reassigns_the_whole_window() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        with_record(&mut store, "feat+one");
        assign_with(&paths, &mut store, "feat+one", &roles(&["web"]), all_free).unwrap();
        let grown = assign_with(
            &paths,
            &mut store,
            "feat+one",
            &roles(&["web", "api"]),
            all_free,
        )
        .unwrap();
        assert_eq!(grown.ports.len(), 2);
        assert_eq!(grown.ports["api"], grown.ports["web"] + 1);
        assert_eq!(store.worktrees["feat+one"].ports.len(), 2);
    }

    #[test]
    fn a_process_with_no_roles_gets_no_ports() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        with_record(&mut store, "feat+one");
        let assigned = assign(&paths, &mut store, "feat+one", &[]).unwrap();
        assert!(assigned.ports.is_empty());
        assert!(store.worktrees["feat+one"].ports.is_empty());
    }

    #[test]
    fn assigning_for_a_worktree_with_no_record_is_an_error() {
        let (_dir, paths) = assign_fixture();
        let mut store = crate::state::State::new();
        let err = assign(&paths, &mut store, "ghost", &roles(&["web"])).unwrap_err();
        assert!(format!("{err:#}").contains("ghost"));
    }

    #[test]
    fn is_port_free_reports_a_bound_port_as_taken() {
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_port_free(port), "a bound port must not read as free");
        drop(listener);
    }

    // A dev server that binds loopback only leaves `0.0.0.0:<port>`
    // bindable, so probing one address would report it free and readiness
    // would never arrive.
    #[test]
    fn a_loopback_only_listener_is_not_free_either() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
            "the premise: the wildcard address is still bindable"
        );
        assert!(!is_port_free(port), "but the port is in use");
        drop(listener);
    }
}
