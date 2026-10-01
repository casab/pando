//! The git menu's moves on the drifted fixture `scripts/fixture-repo.sh
//! --drift` builds for trying them by hand: the recipe and the moves held
//! to each other, so the fixture always shows every shape it promises.

use crate::common;
use common::{Kind, build_with_origin, drift};
use pando::actions::git::{self, GitAction, Ran};
use pando::worktree::InProgress;

fn quiet(_: &str) {}

#[test]
fn the_drifted_fixture_shows_every_shape_the_git_menu_handles() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = build_with_origin(Kind::Plain, dir.path());
    let worktrees = drift(&fixture, dir.path());
    let [clean, conflict, dirty] = &worktrees[..] else {
        panic!("three worktrees: {worktrees:?}");
    };
    let base = Some("origin/main");

    // Unfetched: nothing to see until the fetch, then three behind.
    let main = git::read(&fixture.root, true, base);
    assert_eq!(main.upstream_drift, Some((0, 0)));
    let fetched = git::run(&fixture.root, true, base, GitAction::Fetch, &quiet).unwrap();
    assert_eq!(
        fetched,
        Ran::Unchanged("fetched origin · 1 branch moved".into())
    );
    let main = git::read(&fixture.root, true, base);
    assert_eq!(main.upstream_drift, Some((0, 3)));

    let ran = git::run(clean, false, base, GitAction::Rebase, &quiet).unwrap();
    assert!(ran.moved(), "{ran:?}");
    assert_eq!(git::read(clean, false, base).base_drift, Some((2, 0)));

    let ran = git::run(conflict, false, base, GitAction::Rebase, &quiet).unwrap();
    assert!(
        matches!(&ran, Ran::Conflict { op: InProgress::Rebase, files, .. } if files == &["notes.txt"]),
        "{ran:?}"
    );
    assert_eq!(pando::worktree::in_progress(conflict), None);

    let refused = git::run(dirty, false, base, GitAction::Rebase, &quiet).unwrap_err();
    assert!(format!("{refused}").contains("uncommitted"), "{refused}");

    let ran = git::run(&fixture.root, true, base, GitAction::Pull, &quiet).unwrap();
    assert_eq!(
        ran,
        Ran::Moved("main fast-forwarded 3 commits to origin/main".into())
    );
}
