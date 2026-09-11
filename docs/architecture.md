# Architecture

Where Arvo is going and what shape it takes to get there. Decisions with
consequences live in [`adr/`](adr/); this is the standing picture they add up to.

## Destination

Arvo is an **AI-native financial research platform** — closer to "VS Code for
quantitative finance" than to another backtesting engine. Its value sits *above*
the trading engine, in a research loop no trading engine provides:

```
Hypothesis → Experiment → Simulation → Evaluation → Evidence → Research Memory → AI Research Agent
```

NautilusTrader is the financial runtime underneath, linked in-process as Rust
crates. Arvo does not reimplement what Nautilus already solves — orders,
positions, accounts, portfolio, execution, backtesting, message bus, cache.

> **Arvo owns concepts unique to Arvo. Nautilus owns trading-engine concepts
> Nautilus already solves well.** — [ADR-0003](adr/0003-nautilus-containment.md)

## The differentiator is evaluation, not execution

Out-of-sample selection, deflation against multiple testing, `Inconclusive` as a
first-class verdict, reconciliation invariants, effective breadth, replayable
findings, ranked recommendations. Nautilus does none of this and is not trying
to. This is the part worth building carefully.

The premise that follows from it: **most backtest results are noise, and the job
is to avoid believing them.** A fully-formed, plausible, entirely fictitious
finding is the enemy — and the platform has produced one (an opening-range run
reporting +98% on two losing trades, with a Sharpe of 40).

Hence the sequencing rule that orders the roadmap:

> **Anything that makes a wrong answer look right outranks anything that adds
> capability.**

## Standing rules

- **The UI must not know Nautilus exists.** Leptos works in Arvo concepts —
  experiments, hypotheses, evidence, datasets — never engine internals.
- **The AI never receives broker credentials.** It acts through declared
  capabilities gated by `arvo-core`'s secrets. The LLM is not the learning
  system: it proposes hypotheses, and *evidence* updates them.
- **A provider trait is earned by removing a dependency or a panic**, never by
  anticipating an implementation nobody has asked for —
  [ADR-0005](adr/0005-providers-are-earned.md).
- **Systematic, not discretionary.** Nautilus is an algorithmic execution
  framework, and systematic is the only form the benchmark discipline can
  evaluate.

## Crate map

| Crate | Owns |
|---|---|
| `arvo-core` | Secrets, config, events — platform primitives |
| `arvo-data` | `Bar`, `Dividend`, `BarProvider`, the CSV library, quality and agreement checks |
| `arvo-research` | Experiments, evaluation, evidence, advice, the risk policy |
| `arvo-nautilus` | The only crate permitted to name a Nautilus type |
| `arvo-execution` | Venues, orders, fills, the paper executor, divergence |
| `arvo-runtime` | The Tauri host, data sources, commands |
| `arvo-views` | The shapes crossing to the UI, defined once |
| `arvo-editor` | The Leptos workbench |
| `arvo-mcp` / `arvo-oauth` | Protocol and authorization, both vendor-agnostic |
| `arvo-plugin-host` | Out-of-process plugins over gRPC |

Crates provide *architecture* (compile-time boundaries). Extensions provide
*replaceable runtime capabilities*. Not every crate is an extension, and not
every extension needs a crate.

## Data, and what it assumes

Bars are fetched to files and pinned by content hash, never read live during a
run — [ADR-0008](adr/0008-fetch-writes-files.md). Everything downstream reads
`arvo_data::CsvBars`.

Sources are asked for **split-adjusted** prices: raw prices make a split look
like a crash, which a breakout rule would trade. Split-adjusted is not
total-return adjusted, so dividends are absent from the series and nothing
credits them — a bias that always favours the strategy, now measured rather than
estimated ([ADR-0011](adr/0011-dividend-gap-beside-not-folded-in.md)).

Still open: one vendor per instrument unless cross-checked by hand, no
point-in-time index membership, and no delisted archive — so a panel over
today's names is a panel over survivors.

## Execution

Research and paper first. Live execution with real capital was always a separate
and explicit decision rather than a configuration flag
([ADR-0006](adr/0006-live-execution-is-a-separate-decision.md)); that decision
has since been taken, and the work is tracked in milestones M3–M5.

The load-bearing constraint is that **one risk policy runs in both the backtest
and a live session** ([ADR-0009](adr/0009-one-risk-policy.md)). A second risk
engine on the live side would not be a refinement — it would make every stored
verdict a statement about a system that does not exist.

## Repo layout

`arvo-platform/` is **not** a git repo; `arvo-desktop/` is.
`arvo-retirement-service/` is a sibling Python repo, parked — leftover reference
material to migrate into a platform extension later, not a component of this
architecture.
