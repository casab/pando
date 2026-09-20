//! Builds a fixture repository under the system temp directory and prints
//! its path, for trying pando by hand.
//!
//! The recipe is the one the tests use, pulled in by path rather than
//! copied so the two can never drift. Run it through
//! `scripts/fixture-repo.sh`.

#[path = "../tests/common/mod.rs"]
mod common;

use common::Kind;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let mut args = std::env::args().skip(1);
    let requested = args.next().unwrap_or_else(|| "plain".to_string());
    if requested == "--list" || requested == "-l" {
        for kind in Kind::ALL {
            println!("{}", kind.dir_name());
        }
        return;
    }
    let with_origin = args.any(|a| a == "--with-origin");

    let Some(kind) = Kind::parse(&requested) else {
        eprintln!("unknown fixture {requested:?}. Known kinds:");
        for kind in Kind::ALL {
            eprintln!("  {}", kind.dir_name());
        }
        std::process::exit(2);
    };

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // Under the system temp directory, never anywhere a real project lives.
    let parent = std::env::temp_dir().join(format!("pando-fixture-{stamp}"));
    std::fs::create_dir_all(&parent).expect("create the fixture directory");

    let fixture = if with_origin {
        common::build_with_origin(kind, &parent)
    } else {
        common::build(kind, &parent)
    };

    if let Some(remote) = &fixture.remote {
        eprintln!("origin: {}", remote.display());
    }
    eprintln!(
        "built the {} fixture. Try:\n  cd {}\n  PANDO_HOME={} pando ls",
        kind.dir_name(),
        fixture.root.display(),
        parent.join("pando-home").display()
    );
    println!("{}", fixture.root.display());
}
