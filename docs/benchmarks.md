# Benchmarks

What a backtest costs, measured rather than assumed. Every question about a
faster sweep, on more cores or on a GPU, is checked against these numbers,
and they are re-measured after a Nautilus bump or a change to the run path.

```
cargo run --release -p arvo-service --example bench_backtest -- <data-dir> [instrument] [--strategy NAME] [--runs N]
RAYON_NUM_THREADS=1 cargo run --release -p arvo-service --example bench_backtest -- <data-dir> [instrument]
```

Release, always: a debug Nautilus is an order of magnitude slower and says
nothing about the shipped engine. The example reads the library and records
no finding. "One backtest" is the sweep's first configuration over the whole
window; "sweep" is `run_family` for the strategy's grid, which runs every
configuration over the in-sample window, then the winner and its benchmark
out of sample. The sweep runs its configurations across the cores since
#212; the single-thread row is the same code with `RAYON_NUM_THREADS=1`.

## 2026-09-20 · commit 4abba79 · AMD Ryzen 5 5500 (6 cores, 12 threads), 16 GB, rustc 1.98.0

`sma_cross`, daily bars, grid of 9 configurations.

| Instrument | Bars | One backtest (median of 5) | Sweep, 1 thread | Sweep, 12 threads | Speed-up |
| --- | --- | --- | --- | --- | --- |
| `DRIFT.SIM` | 5,001 | 124 ms | 619 ms (68 ms per trial) | 182 ms (20 ms per trial) | 3.4× |
| `AAPL.YF` | 6,284 | 179 ms | 823 ms (91 ms per trial) | 249 ms (27 ms per trial) | 3.3× |

What to read from it:

- A daily backtest over twenty years costs on the order of 150 ms. A trial
  in the sweep costs less than one full backtest because it runs the
  in-sample window only.
- Nine trials do not fill twelve threads, and the out-of-sample run and the
  benchmark are still sequential after them, so the speed-up is 3.3× rather
  than the core count. A larger grid gets closer to the cores; the
  sequential tail stays the same size.
- Nothing here is a reason for a GPU. The compute-plane triggers in
  arvo-adrs (`research/2026-09-20-gpu-compute-plane.md`) are measured
  against this table; until a sweep takes minutes on every core, the answer
  they give is no.

The slowest single run in each set (around 400 ms) is the first: Nautilus's
one-time setup, which is why the median is the number.
