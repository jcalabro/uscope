# Performance work

Performance changes must use the project's measurements. Do not add one-off
timing prints.

## Timings

Give a new phase one `span!` and count repeated work with `count!` from
`src/timings`. Do not create a span for every row or entry.

Run uscope with `--timings FILE`, then summarize or compare reports:

```sh
just timings FILE
just timings OLD NEW
```

Reports include spans, stops, and requests, so a change can be tied to the
work that caused it.

## Allocation and load benchmarks

`just bench-smoke` compares load allocation counts with
`bench/baseline.json`. Record a new baseline only when the allocation change is
intentional. Use `just bench --compare BASE` to compare a larger run.

## Recorder overhead

`just profile-overhead` measures reference workloads under each recording
preset. It takes minutes and stays outside the normal gate. Compare results
with `bench/overhead.json`; update the baseline only for a deliberate change.

## Profilers

The default shell contains command-line profiling tools. For the larger tools
used interactively, enter:

```sh
nix develop .#profile
```

Use the existing `just profile-*` recipes so measurements use the same build
and environment as their baselines.
