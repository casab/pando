#!/usr/bin/env bash
#
# Build a fixture repository under $TMPDIR and print its path.
#
#   scripts/fixture-repo.sh                 # the default "plain" fixture
#   scripts/fixture-repo.sh next-pnpm-compose
#   scripts/fixture-repo.sh plain --with-origin
#   scripts/fixture-repo.sh --list
#
# Mutating pando commands are only ever run against one of these, never
# against a real repository. The recipe lives in tests/common/mod.rs; this
# script is a wrapper around the example binary that uses it, so the two
# cannot drift.

set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo run --quiet --example fixture-repo -- "$@"
