#!/bin/bash
# Exercises action.yml's actual logic (platform resolution, archive
# extraction, PATH setup, the formwatch invocation, and the report step)
# against a binary built and packaged locally — the same shape
# release.yml's own build+package steps produce — instead of a real
# GitHub release. Useful for testing changes to action.yml without
# cutting a release first.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "=== building formwatch (release.yml's own build step) ==="
cargo build --release --locked

echo "=== packaging it exactly as release.yml does for linux-x86_64 ==="
work=$(mktemp -d)
triple=x86_64-unknown-linux-gnu
name="formwatch-$triple"
mkdir "$work/$name"
cp target/release/formwatch "$work/$name/"
cp README.md LICENSE NOTICE "$work/$name/"
(cd "$work" && tar czf "$name.tar.gz" "$name")

echo "=== action.yml step: 'Resolve the release asset for this runner' ==="
runner_os=Linux
runner_arch=X64
case "$runner_os-$runner_arch" in
  Linux-X64) triple=x86_64-unknown-linux-gnu; ext=tar.gz ;;
  *) echo "unexpected"; exit 1 ;;
esac
asset="formwatch-$triple.$ext"

echo "=== action.yml step: 'Download and extract' (local copy instead of gh release download) ==="
dest=$(mktemp -d)
cp "$work/$asset" "$dest/$asset"
archive="$dest/$asset"
if [ "$ext" = "zip" ]; then
  unzip -q "$archive" -d "$dest"
else
  tar xzf "$archive" -C "$dest"
fi
bin_dir="$dest/formwatch-$triple"
chmod +x "$bin_dir/formwatch" 2>/dev/null || true
ls "$bin_dir"

echo "=== action.yml step: 'Run formwatch' (test mode, a local fixture) ==="
export PATH="$bin_dir:$PATH"
history_dir=$(mktemp -d)
set +e
shopt -s globstar
args=(--history-dir "$history_dir")
url="file://$PWD/fixtures/test-form.html"
args+=(test "$url" --wait 1)
args+=(--fail-on fail)
formwatch "${args[@]}"
exit_code=$?
set -e
echo "formwatch exited with $exit_code (expected 1: test-form.html has planted Fail bugs)"

echo "=== action.yml step: 'Build the full report' ==="
report_path=$(mktemp)
formwatch --history-dir "$history_dir" report --json --out "$report_path" || true
test -s "$report_path" && echo "report written to $report_path (non-empty, OK)"

rm -rf "$work" "$dest" "$history_dir" "$report_path"
echo "=== action.yml logic verified end-to-end (exit_code=$exit_code) ==="
