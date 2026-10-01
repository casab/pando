#!/usr/bin/env bash
#
# Build a fixture repository under $TMPDIR and print its path.
#
#   scripts/fixture-repo.sh                 # the default "plain" fixture
#   scripts/fixture-repo.sh next-pnpm-compose
#   scripts/fixture-repo.sh plain --with-origin
#   scripts/fixture-repo.sh next-pnpm-compose --listener
#   scripts/fixture-repo.sh mono-web-api --listener
#   scripts/fixture-repo.sh plain --with-origin --listener --drift
#   scripts/fixture-repo.sh --list
#
# --listener also writes a pando-home config whose dev process is a python
# listener, so `pando start` has something real to start without installing
# the fixture's framework. For the mono-web-api workspace it writes two of
# them, one per app, and the web one prints the VITE_API_URL it was given
# so the cross-process template is visible in its log.
#
# --drift (with --with-origin) moves origin's main three commits on and
# makes three worktrees beside the fixture — one that rebases cleanly, one
# that conflicts, one with uncommitted work — for trying the TUI's git
# menu (u) on each.
#
# Mutating pando commands are only ever run against one of these, never
# against a real repository. The recipe lives in tests/common/mod.rs; this
# script is a wrapper around the example binary that uses it, so the two
# cannot drift.

set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo run --quiet --example fixture-repo -- "$@"
