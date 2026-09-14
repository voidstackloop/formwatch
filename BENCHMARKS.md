# Benchmarks

formwatch drives a real headless Chrome against every form, so its wall-clock
time is dominated by browser work (page load, axe-core, DOM evaluation), not
by formwatch's own bookkeeping. To make that measurable and reproducible
without touching anyone's website, the benchmarks run against the **bundled
demo site**: [`formwatch demo`](demo-sites/README.md) serves 35 local forms —
a clean baseline plus a page for every check and known edge case — and
`monitor demo-sites/forms.yml` checks all of them.

Because everything is served from `127.0.0.1`, there's no network latency in
these numbers — what's left is browser launch, the eight built-in checks per
form, screenshot capture, and report writing.

## Latest results

Measured with `RUNS=5 WAIT=1 scripts/benchmark.sh` (median of 5 runs, after
one warm-up run):

| Forms | Checks/run | Runs | `--wait` | min (s) | median (s) | max (s) | forms/min | checks/s |
|------:|-----------:|-----:|---------:|--------:|-----------:|--------:|----------:|---------:|
| 35 | 280 | 5 | 1s | 12.93 | 12.96 | 13.19 | 162.0 | 21.6 |

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
- `--max-concurrent 4` (the default)

## Reproduce

```
scripts/benchmark.sh                 # 5 runs, --wait 1
RUNS=10 WAIT=0 scripts/benchmark.sh  # more runs, no idle wait
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
- **Concurrency matters more than raw speed.** `--max-concurrent` (default
  4) multiplies throughput on multi-core machines at the cost of running
  more browsers at once; the per-host pacer (`--per-host-delay-ms`) is the
  polite counterweight.
- **Screenshots cost something.** Every non-Pass check captures a full-page
  PNG; `--no-screenshots` trades evidence for speed.
- **A failing page is slower than a passing one** (screenshots plus the
  extra validation work), which is why the demo mix matters.

## A note on comparing machines

Absolute seconds vary with CPU, disk, and the Chromium revision, so treat
the table as an order-of-magnitude reference and re-run the script on your
own hardware rather than quoting these numbers as a spec.
