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
| The engine's API | [arvo-engine-api](https://github.com/wjpin84/arvo-engine-api), checked out at `contract/` — clone with `--recurse-submodules` |
| Decisions | [arvo-adrs](https://github.com/wjpin84/arvo-adrs) — one per file, superseded rather than edited, across every Arvo repository |
| Roadmap | [GitHub Project](https://github.com/users/wjpin84/projects/4) — milestones and issues |

`ROADMAP.md` and `gap_analysis.md` used to live here. Both are now the project
board; the decisions they carried are in [arvo-adrs](https://github.com/wjpin84/arvo-adrs).

## Building

```
cargo test --workspace        # the suite
cargo tauri dev               # the app
cargo tauri build             # an installer, engine included
```

`cargo tauri build` builds `arvo-engine` first and ships it beside the window
as a sidecar (#32), so an installed Arvo starts its own engine and a script
run from the editor works without a terminal. In a `cargo build` tree the two
are already side by side and nothing is staged.

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
([ADR-0016](https://github.com/wjpin84/arvo-adrs/blob/main/0016-an-agent-reaches-arvo-through-a-tool-list-that-cannot-trade.md)):

```
cargo build --release -p arvo-mcp-server
claude mcp add arvo -- <path-to>/target/release/arvo-mcp-server
```

With no argument it uses the open project folder (`%APPDATA%/com.arvo.desktop` until one is chosen); pass a directory to use
another. Runs are saved as the agent's findings, deflated against everything
that agent has run, and every call is appended to `agent-audit.jsonl`.

## Licence

Apache-2.0. NautilusTrader is LGPL-3.0-only and is linked into the shipped
binary — see [`NOTICE`](NOTICE) and
[ADR-0002](https://github.com/wjpin84/arvo-adrs/blob/main/0002-apache-2-with-lgpl-dependency.md).
