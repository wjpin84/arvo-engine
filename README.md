# Arvo Engine

The process behind Arvo: it runs studies, keeps the data library, holds
credentials, hosts plugins, schedules a person's scripts, and keeps doing all
of it when the window closes. Everything that decides anything is here.
What a person sees is [arvo-desktop](https://github.com/wjpin84/arvo-desktop),
which talks to this over the gRPC API in
[arvo-engine-api](https://github.com/wjpin84/arvo-engine-api) and knows
nothing about how a study is run.

Arvo is an AI-native financial research platform, closer to "VS Code for
quantitative finance" than to another backtesting engine. NautilusTrader is
the execution engine underneath; Arvo owns the research loop above it.

```
Hypothesis → Experiment → Simulation → Evaluation → Evidence → Research Memory
```

The differentiator is **evaluation, not execution**: out-of-sample selection,
deflation against multiple testing, `Inconclusive` as a first-class verdict,
reconciliation invariants that refuse to let a self-contradicting result be
read, and replayable findings. The premise is that most backtest results are
noise and the job is to avoid believing them. Hence the rule that orders
everything else:

> Anything that makes a wrong answer look right outranks anything that adds
> capability.

## Where things are

| | |
|---|---|
| Architecture | [docs/architecture.md](docs/architecture.md) |
| The API | [arvo-engine-api](https://github.com/wjpin84/arvo-engine-api), checked out at `contract/` — the protos this serves, and the Rust and Python bindings generated from them |
| The provider contract | [arvo-extension-api](https://github.com/wjpin84/arvo-extension-api), checked out at `extension/` — what a data source or signal plugin implements and this calls |
| The window | [arvo-desktop](https://github.com/wjpin84/arvo-desktop) |
| Decisions | [arvo-adrs](https://github.com/wjpin84/arvo-adrs) — one per file, superseded rather than edited, across every Arvo repository |
| Roadmap | [GitHub Project](https://github.com/users/wjpin84/projects/4) |

Clone with `--recurse-submodules`, or run `git submodule update --init`
afterwards: both contracts are submodules and the build reads them.

## What ships

Three binaries, released together as one archive per platform:

| Binary | What it is |
|---|---|
| `arvo-engine` | The daemon. Serves the API on loopback, writes `engine.json` and `control.json` to the app data directory, and is what the window starts and what a script reaches |
| `arvo-mcp-server` | Arvo as a stdio MCP server: an agent reads research memory and runs studies, and cannot fetch or trade ([ADR-0016](https://github.com/wjpin84/arvo-adrs/blob/main/0016-an-agent-reaches-arvo-through-a-tool-list-that-cannot-trade.md)) |
| `arvo` | The command line. Not written yet; it is a thin client over `arvo-client` and belongs here |

## Building

```
cargo test --workspace                 # the suite
cargo build -p arvo-engine             # the daemon, in target/debug
target/debug/arvo-engine               # leave running; the window and a script find it through engine.json
```

The Python client's tests live with the contract and run against a built
engine:

```
cd contract/python
ARVO_ENGINE=../../target/debug/arvo-engine uv run pytest
```

### As an MCP server

```
cargo build --release -p arvo-mcp-server
claude mcp add arvo -- <path-to>/target/release/arvo-mcp-server
```

With no argument it uses the open project folder (`%APPDATA%/com.arvo.desktop`
until one is chosen); pass a directory to use another. Runs are saved as the
agent's findings, deflated against everything that agent has run, and every
call is appended to `agent-audit.jsonl`.

## The workspace

| Crate | What it is |
|---|---|
| `app/arvo-engine` | The daemon: the gRPC surface and its two token tiers, sessions, the composition root |
| `app/arvo-mcp-server` | The MCP server over the research tier |
| `crates/arvo-service` | The service tier every front end shares: research, data, portfolios, accounts, plugins, scripts, jobs |
| `crates/arvo-core`, `arvo-data`, `arvo-research`, `arvo-risk`, `arvo-portfolio`, `arvo-execution` | The domain |
| `crates/arvo-plugin-host` | Out-of-process plugins over gRPC, against `extension/` |
| `crates/arvo-schedule` | The scheduler: intervals and cron, with the Unix weekday dialect translated |
| `crates/arvo-mcp`, `crates/arvo-oauth` | Protocol and authorization, vendor-agnostic |
| `engines/arvo-nautilus` | The only crate permitted to name a Nautilus type |
| `integrations/arvo-robinhood`, `arvo-yfinance`, `arvo-alpaca` | Vendors |
| `contract/rust/arvo-api`, `contract/rust/arvo-client` | The API, from the contract repository: this implements its server traits |

## Licence

Apache-2.0. NautilusTrader is LGPL-3.0-only and is linked into the shipped
binary — see [`NOTICE`](NOTICE) and
[ADR-0002](https://github.com/wjpin84/arvo-adrs/blob/main/0002-apache-2-with-lgpl-dependency.md).
