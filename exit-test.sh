#!/bin/bash
set -e
cd ~/projects/formwatch
git add .
git commit -m "$(cat <<'EOF'
Initial commit: formwatch CLI

Tests public-service forms for broken submission flows, accessibility,
mobile usability, lost input, and unclear validation/document requirements.
Built via ADR-driven design (docs/adr/0001), a chromiumoxide-based check
engine with axe-core for accessibility, custom-check plugin support via
--checks-dir, run history with diffing, HTML/JSON reporting, and a
scaffolded (not deployed) community registry + dashboard layer.

Ten real bugs found and fixed via code review during development, each
verified with a test proven to fail before the fix and pass after:
date-field min/max handling, a false-positive session-timeout regex, a
checkbox/radio invalidation no-op, cross-check page-state confusion after
--submit, a single failing check losing an entire form's report, an
undetected chrome-error:// page-load failure, a non-injective URL-to-
directory collision, a same-second history-file clobber, a missing diff
in the plain-text report, and a release workflow that would never
actually publish results — plus dependency hygiene (dropped an unused,
unmaintained async-std runtime pulled in by default features) and a
release-tag/Cargo.toml version-mismatch guard.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
git log --oneline -1
git status --short
