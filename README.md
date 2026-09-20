# Arvo Desktop

The window on Arvo: an editor, a research workbench and the panels that show
what the engine found, in one place. The engine itself, the process that
runs studies, keeps the data library, holds credentials and keeps running
when this window closes, lives in
[arvo-engine](https://github.com/wjpin84/arvo-engine). This repository
talks to it over its gRPC API and knows nothing about how a study is run.

Arvo is an AI-native financial research platform, closer to "VS Code for
quantitative finance" than to another backtesting engine. The premise is
that most backtest results are noise and the job is to avoid believing them.
Hence the rule that orders everything else:

> Anything that makes a wrong answer look right outranks anything that adds
> capability.

## Where things are

| | |
|---|---|
| Architecture | [docs/architecture.md](docs/architecture.md) |
| The engine | [arvo-engine](https://github.com/wjpin84/arvo-engine) — the daemon, the MCP server, and every crate that decides anything |
| The engine's API | [arvo-engine-api](https://github.com/wjpin84/arvo-engine-api), checked out at `contract/` — the protos and the Rust and Python bindings this window builds against |
| Decisions | [arvo-adrs](https://github.com/wjpin84/arvo-adrs) — one per file, superseded rather than edited, across every Arvo repository |
| Roadmap | [GitHub Project](https://github.com/users/wjpin84/projects/5) — the UI board; the engine has its own |

Clone with `--recurse-submodules`, or run `git submodule update --init`
afterwards: the contract crates are path dependencies into `contract/`.

## What is in the workspace

| Crate | What it is |
|---|---|
| `app/arvo-runtime` | The Tauri host: the window, the tray, the commands the editor calls, and the client of the engine |
| `app/arvo-editor` | The Leptos workbench, compiled to WebAssembly |
| `app/arvo-window` | The shapes the host and the editor agree on and the engine never sees |
| `contract/rust/arvo-api`, `contract/rust/arvo-client` | The engine's API, from the contract repository |

Nothing here depends on an engine crate. That is the point of the split:
the window renders what the engine says, and the engine does not know the
window exists.

## Building and running

The window needs a built engine to start. Point it at one:

```
cargo test --workspace                                # the suite
$env:ARVO_ENGINE = "..\arvo-engine\target\debug\arvo-engine.exe"   # a local build
cargo tauri dev                                       # the app
```

Without `ARVO_ENGINE`, the window looks for `arvo-engine` beside its own
executable, which is where a bundle installs it and where a combined cargo
tree used to put it.

For an installer, ship a released engine rather than a local build:

```
python tools/fetch_engine.py          # downloads the version in app/arvo-runtime/engine-version
$env:ARVO_ENGINE = "<the path it prints>"
cargo tauri build
```

The build stages the engine as a sidecar beside the window (#32), so an
installed Arvo starts its own engine and a script run from the editor works
without a terminal. `engine-version` pins which engine release this window
was built against; bump it when the contract moves.

## Licence

Apache-2.0. NautilusTrader is LGPL-3.0-only and is linked into the shipped
engine — see [`NOTICE`](NOTICE) and
[ADR-0002](https://github.com/wjpin84/arvo-adrs/blob/main/0002-apache-2-with-lgpl-dependency.md).
