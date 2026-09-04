"""Generates synthetic daily bars for exercising the Research view.

    python tools/make_sample_data.py [output-dir]

With no argument it writes to the app data directory the desktop app reads
(`%APPDATA%/com.arvo.desktop/data` on Windows).

THIS IS NOT MARKET DATA. Every price here comes out of a random number
generator. The instruments are named `*.SIM` so that a file sitting in a data
directory can never be mistaken for a real ticker, and so that nothing
produced from it can be mistaken for a result about a real market.

What it is good for: exercising the Research view end to end with data whose
properties are known, so what the tool concludes can be checked against what
was actually put in.

  TREND.SIM  Regime-switching drift, so momentum genuinely exists here.
  DRIFT.SIM  Steady upward drift, no regimes. Buy-and-hold is hard to beat.
  NOISE.SIM  Driftless random walk. There is nothing in it to find.

At the time of writing, all three come back **Not supported**, and that is the
honest outcome rather than a broken one. Each fails on two counts: the best
in-sample Sharpe does not clear what a nine-trial search would be expected to
produce with no skill, and the out-of-sample run makes 11 to 24 round trips
against a 30-trade minimum.

The trade-count failure is the interesting one, and it is a fact about daily
trend following rather than about this data. A moving-average crossover on one
instrument simply does not trade often enough to support a claim, however much
history you give it. The real answer is breadth — the same strategy across many
instruments — which the machinery does not do yet.

Resist the urge to tune the generator until something comes back Supported.
Adjusting the data, the grid or the criteria until the verdict changes is the
exact failure the multiple-testing check exists to catch, and doing it by hand
does not make it rigorous. If you want to see the other verdict branches, the
place to do it is `EvaluationCriteria`, deliberately and in the open.

Deterministic: a fixed seed and a hand-rolled generator, so re-running
produces byte-identical files. That matters because an experiment records a
dataset version, and a dataset that changes underneath a result silently
invalidates it.
"""

from __future__ import annotations

import datetime as dt
import math
import os
import pathlib
import sys

# About twenty years. Sized from the far end: the study's slowest average is
# 200 bars, the split holds back 30%, and a verdict needs 30 round trips. A
# 200-day average crossing 30 times needs *thousands* of out-of-sample bars,
# not hundreds — six years of data produced one trade and an unconditionally
# inconclusive result.
TRADING_DAYS = 5000
# Ends near the present day rather than in the future: a file of "daily bars"
# dated 2037 invites a double-take every time somebody opens it.
START = dt.date(2006, 1, 2)
START_PRICE = 100.0


class Rng:
    """A small linear congruential generator.

    Hand-rolled rather than `random`, so the output does not depend on the
    Python version's Mersenne Twister implementation details. The file is
    meant to be byte-identical everywhere.
    """

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFF

    def next_float(self) -> float:
        # Numerical Recipes' LCG constants.
        self.state = (1664525 * self.state + 1013904223) & 0xFFFFFFFF
        return self.state / 0x100000000

    def normal(self) -> float:
        """Box-Muller, one value per call (the second is discarded)."""
        u1 = max(self.next_float(), 1e-12)
        u2 = self.next_float()
        return math.sqrt(-2.0 * math.log(u1)) * math.cos(2.0 * math.pi * u2)


def trading_days(count: int) -> list[dt.date]:
    """Weekdays only. No exchange holiday calendar — a backtest on this data
    is testing the code path, not a calendar."""
    days: list[dt.date] = []
    day = START
    while len(days) < count:
        if day.weekday() < 5:
            days.append(day)
        day += dt.timedelta(days=1)
    return days


def series(rng: Rng, days: list[dt.date], drift: float, vol: float, regimes: bool):
    """Geometric random walk, optionally switching drift sign between regimes.

    Regimes average about sixty trading days, which is long enough for a
    50-to-200-day moving average pair to actually catch one.
    """
    rows = []
    price = START_PRICE
    direction = 1.0
    for day in days:
        if regimes and rng.next_float() < 1.0 / 60.0:
            direction = -direction

        open_price = price
        ret = drift * direction + vol * rng.normal()
        close_price = max(open_price * math.exp(ret), 1.0)

        # Intraday range around the open/close pair, so high and low always
        # bracket both — the ordering arvo-data checks on parse and Nautilus
        # checks again at the engine.
        span = abs(rng.normal()) * vol * 0.6
        high = max(open_price, close_price) * (1.0 + span)
        low = min(open_price, close_price) * (1.0 - span)

        o, h, l, c = (round(v, 2) for v in (open_price, high, low, close_price))
        # Re-clamp after rounding: rounding is monotonic but can make a high
        # equal to a close it should exceed, and a bar that fails the OHLC
        # predicates is rejected at the data boundary.
        h = max(h, o, c)
        l = min(l, o, c)

        volume = int(500_000 * (1.0 + 0.4 * abs(rng.normal())))
        rows.append((day, o, h, l, c, volume))
        price = close_price
    return rows


def write(path: pathlib.Path, rows) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write("date,open,high,low,close,volume\n")
        for day, o, h, l, c, volume in rows:
            handle.write(f"{day.isoformat()},{o:.2f},{h:.2f},{l:.2f},{c:.2f},{volume}\n")


def default_output_dir() -> pathlib.Path:
    if sys.platform == "win32":
        base = pathlib.Path(os.environ.get("APPDATA", pathlib.Path.home()))
        return base / "com.arvo.desktop" / "data"
    if sys.platform == "darwin":
        return pathlib.Path.home() / "Library" / "Application Support" / "com.arvo.desktop" / "data"
    base = pathlib.Path(os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local" / "share"))
    return base / "com.arvo.desktop" / "data"


# (instrument, seed, drift, volatility, regime-switching)
INSTRUMENTS = [
    ("TREND.SIM", 20240101, 0.0016, 0.011, True),
    ("DRIFT.SIM", 20240202, 0.0005, 0.010, False),
    ("NOISE.SIM", 20240303, 0.0000, 0.012, False),
]


def main() -> int:
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else default_output_dir()
    out.mkdir(parents=True, exist_ok=True)
    days = trading_days(TRADING_DAYS)

    for name, seed, drift, vol, regimes in INSTRUMENTS:
        rows = series(Rng(seed), days, drift, vol, regimes)
        path = out / f"{name}.csv"
        write(path, rows)
        closes = [row[4] for row in rows]
        print(
            f"{path}  {len(rows)} bars  {days[0]}..{days[-1]}  "
            f"{closes[0]:.2f} -> {closes[-1]:.2f}"
        )

    print("\nSynthetic data only. Restart Arvo, open Research, and pick one.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
