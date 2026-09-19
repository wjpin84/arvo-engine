# Architecture

Where Arvo is going and what shape it takes to get there. Decisions with
consequences live in [arvo-adrs](https://github.com/wjpin84/arvo-adrs); this is the standing picture they add up to.

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
> Nautilus already solves well.** — [ADR-0003](https://github.com/wjpin84/arvo-adrs/blob/main/0003-nautilus-containment.md)

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

Decisions are recorded in [arvo-adrs](https://github.com/wjpin84/arvo-adrs),
one numbered file each, across every Arvo repository. This document links to
them by number.


- **The UI must not know Nautilus exists.** Leptos works in Arvo concepts —
  experiments, hypotheses, evidence, datasets — never engine internals.
- **The AI never receives broker credentials.** It acts through declared
  capabilities gated by `arvo-core`'s secrets. The LLM is not the learning
  system: it proposes hypotheses, and *evidence* updates them.
- **A provider trait is earned by removing a dependency or a panic**, never by
  anticipating an implementation nobody has asked for —
  [ADR-0005](https://github.com/wjpin84/arvo-adrs/blob/main/0005-providers-are-earned.md).
- **Systematic, not discretionary.** Nautilus is an algorithmic execution
  framework, and systematic is the only form the benchmark discipline can
  evaluate.

## Protocol boundary

Extensions integrate against the published protocol, not against this
repository. The protobuf definitions and `PROTOCOL.md` in
[`arvo-extension-api`](https://github.com/wjpin84/arvo-extension-api) are
the compatibility boundary; the engine is an implementation of them. Internal
types, services, storage and layout stay private unless promoted into that
API on purpose.

There are two directions across the boundary, and they are different
contracts:

- **Arvo calls a provider.** A provider serves `arvo.source.v1.Source` or
  `arvo.signal.v1.Signals`; Arvo spawns it, hands it a token per spawn and a
  `Grant` per call, and asks nothing of a service it did not claim
  ([ADR-0022](https://github.com/wjpin84/arvo-adrs/blob/main/0022-a-plugin-serves-an-earned-trait-and-the-sources-go-first.md),
  [ADR-0023](https://github.com/wjpin84/arvo-adrs/blob/main/0023-arvo-owns-the-lifecycle-of-what-it-spawns.md)). The
  provider authenticates with its vendor; that is what the grant is for.
- **Something calls Arvo.** An agent over MCP, a script or the window over
  the engine's gRPC. Arvo is the authenticated service, behind the research
  token and the control token, and the tool list is the boundary
  ([ADR-0016](https://github.com/wjpin84/arvo-adrs/blob/main/0016-an-agent-reaches-arvo-through-a-tool-list-that-cannot-trade.md),
  [ADR-0018](https://github.com/wjpin84/arvo-adrs/blob/main/0018-arvo-keeps-running-when-the-window-closes.md)).

Neither direction gets a generic identity, credential-reference or capability
protocol until a second-party caller needs one. What an outside developer
needs today is to build a provider without this repository, which is #181.

## Layout

Four groups, one rule each.

```
crates/        the platform
engines/       runs a backtest
integrations/  talks to a vendor
app/           the desktop application
```

| | Owns |
|---|---|
| `crates/arvo-core` | Secrets, config, events — platform primitives |
| `crates/arvo-data` | `Bar`, `Dividend`, `BarProvider`, the CSV library, quality and agreement checks, and the `Source` trait with its ingest pipeline |
| `crates/arvo-research` | Experiments, evaluation, evidence, advice; re-exports `arvo-risk` as `arvo_research::risk` |
| `crates/arvo-risk` | The risk gate, costs, collateral and trades — what the live path shares with the backtest, without the research |
| `crates/arvo-portfolio` | Holdings imported from a statement |
| `crates/arvo-execution` | The `Executor` trait, the paper executor, divergence |
| `crates/arvo-mcp`, `crates/arvo-oauth` | Protocol and authorization, both vendor-agnostic |
| `crates/arvo-plugin-host` | Out-of-process plugins over gRPC |
| `engines/arvo-nautilus` | The only crate permitted to name a Nautilus type |
| `integrations/arvo-robinhood` | Bars, search and quotes over MCP, with OAuth |
| `integrations/arvo-yfinance` | A second opinion, and dividends |
| `integrations/arvo-alpaca` | Bars and corporate actions, on two feeds |
| `app/arvo-runtime` | The Tauri host, the source registry, commands |
| `app/arvo-views` | The shapes crossing to the UI, defined once |
| `app/arvo-editor` | The Leptos workbench |
| `app/arvo-mcp-server` | Arvo as a stdio MCP server: an agent reads and runs research, never trades |
| `app/arvo-engine` | The engine's local gRPC API and the research tier every non-window front end shares ([ADR-0018](https://github.com/wjpin84/arvo-adrs/blob/main/0018-arvo-keeps-running-when-the-window-closes.md)) |

Crates provide *architecture* (compile-time boundaries). Extensions provide
*replaceable runtime capabilities*. Not every crate is an extension, and not
every extension needs a crate.

### Where the traits live

Beside their implementer or their primary consumer, never in a shared types
crate — see [ADR-0005](https://github.com/wjpin84/arvo-adrs/blob/main/0005-providers-are-earned.md), and note that
`SimulationProvider` sits in `arvo-research` *specifically* so that
`arvo-nautilus` depends on it and not the reverse.

| Trait | Lives in |
|---|---|
| `BarProvider` | `arvo-data`, next to `CsvBars` |
| `Source` | `arvo-data`, next to the library it fills |
| `Executor` | `arvo-execution`, next to `PaperExecutor` |
| `SimulationProvider` | `arvo-research`, to invert the dependency |
| `Correlations` | `arvo-risk`, next to the gate that consumes it |

An integration depends only on `arvo-data` and its own protocol crates —
never on `app/arvo-runtime`. The registry that knows every vendor is the one
thing that must, and it lives in the composition root.

## Data, and what it assumes

Bars are fetched to files and pinned by content hash, never read live during a
run — [ADR-0008](https://github.com/wjpin84/arvo-adrs/blob/main/0008-fetch-writes-files.md). Everything downstream reads
`arvo_data::CsvBars`.

Sources are asked for **split-adjusted** prices: raw prices make a split look
like a crash, which a breakout rule would trade. Split-adjusted is not
total-return adjusted, so dividends are absent from the series and nothing
credits them — a bias that always favours the strategy, now measured rather than
estimated ([ADR-0011](https://github.com/wjpin84/arvo-adrs/blob/main/0011-dividend-gap-beside-not-folded-in.md)).

Total-return prices are served too, by separate sources under separate venues
(`YFTR`, `AIEXTR`, `ASIPTR`): distributions reinvested at the ex-date, which is
what an account with DRIP does ([ADR-0013](https://github.com/wjpin84/arvo-adrs/blob/main/0013-dividends-arrive-as-reinvestment.md)).
The basis is read from the venue, so a study on one records it and the dividend
gap describes the margin rather than correcting it.

Still open: one vendor per instrument unless cross-checked by hand, no
point-in-time index membership, and no delisted archive — so a panel over
today's names is a panel over survivors.

## Execution

Research and paper first. Live execution with real capital was always a separate
and explicit decision rather than a configuration flag
([ADR-0006](https://github.com/wjpin84/arvo-adrs/blob/main/0006-live-execution-is-a-separate-decision.md)); that decision
has since been taken, and the work is tracked in milestones M3–M5.

The load-bearing constraint is that **one risk policy runs in both the backtest
and a live session** ([ADR-0009](https://github.com/wjpin84/arvo-adrs/blob/main/0009-one-risk-policy.md)). A second risk
engine on the live side would not be a refinement — it would make every stored
verdict a statement about a system that does not exist.

## Repo layout

`arvo-platform/` is **not** a git repo; `arvo-desktop/` is.
`arvo-retirement-service/` is a sibling Python repo, parked — leftover reference
material to migrate into a platform extension later, not a component of this
architecture.
