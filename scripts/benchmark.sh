#!/usr/bin/env bash
# Benchmark formwatch against the bundled demo site.
#
#   scripts/benchmark.sh            # 5 timed runs, default port/wait
#   RUNS=10 WAIT=0 PORT=9001 scripts/benchmark.sh
#
# Starts `formwatch demo` on localhost, then repeatedly runs `monitor` over
# every demo form and reports wall-clock timings. Everything is local, so
# the numbers reflect formwatch's own overhead (browser launch, checks,
# report writing) plus the `--wait` you set — not network latency. It never
# touches a third-party site, so it's safe to run anywhere.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8099}"
RUNS="${RUNS:-5}"
WAIT="${WAIT:-1}"
ADDR="127.0.0.1:${PORT}"
BIN="./target/release/formwatch"
HIST="$(mktemp -d)/history"

echo "building release binary…" >&2
cargo build --release -q

"$BIN" demo --addr "$ADDR" >/dev/null 2>&1 &
SRV=$!
trap 'kill "$SRV" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    curl -sf -o /dev/null "http://${ADDR}/good-form.html" && break
    sleep 0.2
done

run_once() {
    "$BIN" --history-dir "$HIST" monitor demo-sites/forms.yml \
        --wait "$WAIT" --max-concurrent 4 --json >/tmp/formwatch-bench.json 2>/dev/null || true
}

run_once  # warm-up (Chrome launch, page cache)

times=()
for _ in $(seq 1 "$RUNS"); do
    start=$(date +%s.%N)
    run_once
    end=$(date +%s.%N)
    times+=("$(awk -v a="$start" -v b="$end" 'BEGIN { printf "%.2f", b - a }')")
done

forms=$(grep -o '"url"' /tmp/formwatch-bench.json | wc -l | tr -d ' ')
checks=$(grep -o '"status"' /tmp/formwatch-bench.json | wc -l | tr -d ' ')

printf '%s\n' "${times[@]}" | sort -n >/tmp/formwatch-bench-times
min=$(head -1 /tmp/formwatch-bench-times)
max=$(tail -1 /tmp/formwatch-bench-times)
median=$(awk -v n="$RUNS" 'NR == int((n + 1) / 2) { print; exit }' /tmp/formwatch-bench-times)
forms_per_min=$(awk -v f="$forms" -v m="$median" 'BEGIN { printf "%.1f", (f / m) * 60 }')
checks_per_sec=$(awk -v c="$checks" -v m="$median" 'BEGIN { printf "%.1f", c / m }')

echo
echo "| Forms | Checks/run | Runs | Wait | min (s) | median (s) | max (s) | forms/min | checks/s |"
echo "|------:|-----------:|-----:|-----:|--------:|-----------:|--------:|----------:|---------:|"
echo "| $forms | $checks | $RUNS | ${WAIT}s | $min | $median | $max | $forms_per_min | $checks_per_sec |"
