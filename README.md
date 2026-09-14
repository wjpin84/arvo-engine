# Arvo Desktop

An AI-native financial research platform — closer to "VS Code for quantitative
finance" than to another backtesting engine. NautilusTrader is the execution
engine underneath; Arvo owns the research loop above it.

```
Hypothesis → Experiment → Simulation → Evaluation → Evidence → Research Memory
```

The differentiator is **evaluation, not execution**: out-of-sample selection,
deflation against multiple testing, `Inconclusive` as a first-class verdict,
reconciliation invariants that refuse to let a self-contradicting result be read,
and replayable findings.

The premise is that most backtest results are noise and the job is to avoid
believing them. Hence the rule that orders everything else:

> Anything that makes a wrong answer look right outranks anything that adds
> capability.

## Where things are

| | |
|---|---|
| Architecture | [docs/architecture.md](docs/architecture.md) |
| Decisions | [docs/adr/](docs/adr/) — one per file, superseded rather than edited |
| Roadmap | [GitHub Project](https://github.com/users/wjpin84/projects/4) — milestones and issues |

`ROADMAP.md` and `gap_analysis.md` used to live here. Both are now the project
board; the decisions they carried are in `docs/adr/`.

## Building

```
cargo test --workspace        # the suite
cargo tauri dev               # the app
```

Headless examples, useful without the window:

```
cargo run -p arvo-runtime --example fetch -- <data-dir> [--source yahoo] SYMBOL...
cargo run -p arvo-runtime --example second_source -- SYMBOL...   # cross-check two vendors
cargo run -p arvo-runtime --example study -- <data-dir> SYMBOL
cargo run -p arvo-runtime --example recheck -- <data-dir> <evidence-dir>   # re-run stored findings
```

### As an MCP server

An agent can read research memory and run studies — never fetch or trade —
through a stdio MCP server over the window's own folders
([ADR-0016](docs/adr/0016-an-agent-reaches-arvo-through-a-tool-list-that-cannot-trade.md)):

```
cargo build --release -p arvo-mcp-server
claude mcp add arvo -- <path-to>/target/release/arvo-mcp-server
```

With no argument it uses `%APPDATA%/com.arvo.desktop`; pass a directory to use
another. Runs are saved as the agent's findings, deflated against everything
that agent has run, and every call is appended to `agent-audit.jsonl`.

## Licence

Apache-2.0. NautilusTrader is LGPL-3.0-only and is linked into the shipped
binary — see [`NOTICE`](NOTICE) and
[ADR-0002](docs/adr/0002-apache-2-with-lgpl-dependency.md).
