#!/usr/bin/env bash
#
# Install pando's Codex skills into ~/.codex/skills.
#
# Codex has no plugin-root variable for a skill to resolve a path
# against, so each skill directory it installs is self-contained: the
# wrapper, plus the brief and the contract it points at, copied in beside
# it. There is still exactly one of each in this repository — these are
# copies made at install time, not a second source. Run this again after
# upgrading pando to refresh them.
#
#   agent/codex/install.sh              # into ~/.codex/skills
#   CODEX_HOME=/tmp/x agent/codex/install.sh    # somewhere else
#
# Claude Code needs none of this: its plugin root is `agent/`, so the
# brief is already beside the skills. See agent/README.md.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
agent="$(dirname "$here")"
dest="${CODEX_HOME:-$HOME/.codex}/skills"

for skill in pando-setup pando-operate; do
    mkdir -p "$dest/$skill"
    cp "$here/$skill/SKILL.md" "$dest/$skill/SKILL.md"
    # The one brain, copied in so the wrapper's "beside this file" is
    # true where it is read rather than only where it is written.
    cp "$agent/brief.md" "$dest/$skill/brief.md"
    cp "$agent/json.md" "$dest/$skill/json.md"
    echo "installed $dest/$skill"
done
