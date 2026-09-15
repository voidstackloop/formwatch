# Benchmarks

formwatch drives a real headless Chrome against every form, so its wall-clock
time is dominated by browser work (page load, axe-core, DOM evaluation), not
by formwatch's own bookkeeping. To make that measurable and reproducible
without touching anyone's website, the benchmarks run against the **bundled
demo site**: [`formwatch demo`](demo-sites/README.md) serves 35 local forms —
a clean baseline plus a page for every check and known edge case — and
`monitor demo-sites/forms.yml` checks all of them.

Because everything is served from `127.0.0.1`, there's no network latency in
these numbers — what's left is browser launch, the eleven built-in checks per
form, screenshot capture, and report writing.

## Latest results

Measured with `scripts/benchmark.sh` (median of the given number of runs,
after one warm-up run), sweeping the two knobs the "Reading the numbers"
section below used to only assert about:

| Forms | Checks/run | Runs | `--wait` | Concurrency | Screenshots | min (s) | median (s) | max (s) | forms/min | checks/s |
|------:|-----------:|-----:|---------:|------------:|:------------|--------:|-----------:|--------:|----------:|---------:|
| 35 | 385 | 5 | 1s | 4 (default) | on | 12.78 | 13.36 | 13.59 | 157.2 | 28.8 |
| 35 | 385 | 5 | 1s | 4 (default) | off | 12.03 | 12.61 | 12.93 | 166.5 | 30.5 |
| 35 | 385 | 5 | 1s | 1 | on | 47.21 | 47.44 | 47.66 | 44.3 | 8.1 |
| 35 | 385 | 3 | 1s | 8 | on | 7.87 | 8.05 | 8.12 | 260.9 | 47.8 |

The 35 demo forms cover a clean baseline, a failing page for most checks, a
three-step wizard and its slow/keyup/icon-only variants, shadow-DOM and
nested-shadow forms, a closed shadow root, RTL, lost input vs. explained
session loss, three bot-protection widgets, and the LLM-wording demos — a
realistic mix, not 35 trivial pages.

### Environment

- CPU: 13th Gen Intel Core i7-13620H (16 threads)
- RAM: 46.9 GiB
- OS: Ubuntu on WSL2
- Chromium (`~/.cache/formwatch/chrome`): 107.0.5296.0
- Rust: 1.98.1

## Reproduce

```
scripts/benchmark.sh                          # 5 runs, --wait 1, --max-concurrent 4
RUNS=10 WAIT=0 scripts/benchmark.sh           # more runs, no idle wait
MAX_CONCURRENT=1 scripts/benchmark.sh         # sweep concurrency
NO_SCREENSHOTS=1 scripts/benchmark.sh         # isolate screenshot cost
PORT=9001 WAIT=3 scripts/benchmark.sh
```

The script builds the release binary, starts `formwatch demo`, runs
`monitor` over `demo-sites/forms.yml` repeatedly, and prints the same table.
Everything is local; it never contacts a third-party site.

## Reading the numbers

- **`--wait` dominates.** Each form's Input-persistence check idles for
  `--wait` seconds (1s here; the default is 5s). That is deliberate idle
  time, not overhead — set `WAIT=0` to see the floor. Real `monitor` runs
  against remote forms add page-load time on top.
- **Concurrency matters far more than raw speed.** Going from
  `--max-concurrent 1` to the default `4` cut the median from 47.4s to
  13.4s on this 16-thread machine — a ~3.5x speedup from running more
  browsers at once, not from anything getting individually faster.
  `--max-concurrent 8` cut it further to 8.1s, though gains taper as
  contention for CPU/Chrome processes grows; the per-host pacer
  (`--per-host-delay-ms`) is the polite counterweight when checking a real,
  shared third-party host rather than a local demo server.
- **Screenshots cost something, but modestly**: 13.4s with screenshots vs.
  12.6s with `--no-screenshots` on this same demo mix — about 6%, worth
  knowing but not the dominant cost. Every non-Pass check captures a
  full-page PNG; `--no-screenshots` trades that evidence for the difference.
- **A failing page is slower than a passing one** (screenshots plus the
  extra validation work), which is why the demo mix matters.

## A note on comparing machines

Absolute seconds vary with CPU, disk, and the Chromium revision, so treat
the table as an order-of-magnitude reference and re-run the script on your
own hardware rather than quoting these numbers as a spec.
