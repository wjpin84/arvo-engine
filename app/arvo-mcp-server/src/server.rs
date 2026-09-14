//! The protocol and the tools, with no stdio in them.
//!
//! # What an agent may do
//!
//! Read research memory, and run research. Nothing else:
//!
//! - **No fetching.** Changing the data library makes existing findings stale
//!   and reaches a vendor with a credential; both are a person's decision.
//! - **No sharing, no broker, no orders.** No tool names a source, a key or an
//!   executor, so there is nothing to misuse — the boundary is the tool list,
//!   not a check an argument could talk its way past.
//!
//! # Why every run is the agent's, and deflated
//!
//! An agent running study after study until one comes out `Supported` is an
//! unbounded search nothing else counts (#25). So a run is saved through
//! `StoredRecord::by_agent`, held to the bar for everything this agent has
//! tried, and the result says so. The agent's name is who it is across
//! sessions: the client's own name from `initialize`, or `--agent`.
//!
//! # The audit trail (#33)
//!
//! Every tool call is appended to `agent-audit.jsonl` beside the evidence:
//! when, which agent, which tool, the arguments, whether it worked, and the
//! finding it produced. A number an agent reports is traceable to the call
//! that made it and the finding that holds it.

use std::io::Write;
use std::path::{Path, PathBuf};

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{
    DateRange, EvaluationCriteria, EvidenceStore, Record, StoredRecord, Verdict,
};
use arvo_runtime_lib::research::{study_for, walk_forward_for, StrategyPlan};
use serde_json::{json, Value};

/// The protocol revision this speaks, the one `arvo-mcp` speaks as a client.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Where tool calls are recorded, in the app data directory.
pub const AUDIT_FILE: &str = "agent-audit.jsonl";

pub struct Server {
    data: PathBuf,
    evidence: PathBuf,
    audit: PathBuf,
    /// Who is running, once known.
    agent: Option<String>,
    /// Set by `--agent`, and then not overridden by the client's name.
    pinned: bool,
}

impl Server {
    pub fn new(root: &Path, agent: Option<String>) -> Self {
        Self {
            data: root.join("data"),
            evidence: root.join("evidence"),
            audit: root.join(AUDIT_FILE),
            pinned: agent.is_some(),
            agent,
        }
    }

    /// One line of JSON-RPC in, at most one line out. `None` for a
    /// notification, which gets no reply.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(request) => self.handle(&request)?,
            Err(err) => error(Value::Null, -32700, &format!("not JSON: {err}")),
        };
        Some(reply.to_string())
    }

    fn handle(&mut self, request: &Value) -> Option<Value> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        // A request without an id is a notification: act, never answer.
        let id = request.get("id").cloned()?;
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => {
                if !self.pinned {
                    self.agent = params
                        .pointer("/clientInfo/name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                result(
                    id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "arvo", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": "Arvo's research memory and research runs. Runs are saved as your findings and deflated against everything you have run; a verdict that did not survive that is not evidence. Nothing here fetches data or trades.",
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => result(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let outcome = self.call(name, &arguments);
                self.record(name, &arguments, &outcome);
                result(
                    id,
                    match outcome {
                        Ok(value) => json!({
                            "content": [{ "type": "text", "text": pretty(&value) }],
                            "structuredContent": value,
                            "isError": false,
                        }),
                        Err(reason) => json!({
                            "content": [{ "type": "text", "text": reason }],
                            "isError": true,
                        }),
                    },
                )
            }
            other => error(id, -32601, &format!("no method {other:?}")),
        })
    }

    fn agent(&self) -> String {
        self.agent.clone().unwrap_or_else(|| "unnamed-agent".to_owned())
    }

    fn call(&self, name: &str, arguments: &Value) -> Result<Value, String> {
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| format!("{name} needs a string argument {key:?}"))
        };
        match name {
            "list_strategies" => arvo_runtime_lib::research::list_strategies()
                .map_err(|err| err.to_string())
                .and_then(|plans| serde_json::to_value(plans).map_err(|err| err.to_string())),
            "list_instruments" => self.list_instruments(),
            "list_findings" => self.list_findings(),
            "open_finding" => {
                let id = text("id")?;
                let stored = self.store().open(&id).map_err(|err| err.to_string())?;
                Ok(summarize(&stored))
            }
            "run_study" => self.run(&text("instrument")?, &text("strategy")?, false),
            "run_walk_forward" => self.run(&text("instrument")?, &text("strategy")?, true),
            other => Err(format!("no tool {other:?}; tools/list says what there is")),
        }
    }

    fn store(&self) -> EvidenceStore {
        EvidenceStore::new(&self.evidence)
    }

    fn list_instruments(&self) -> Result<Value, String> {
        let bars = CsvBars::new(&self.data);
        let mut out = Vec::new();
        for interval in [
            arvo_data::BarInterval::DAILY,
            arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute),
        ] {
            let names: Vec<String> = if interval.is_intraday() {
                std::fs::read_dir(self.data.join(interval.to_string()))
                    .map(|entries| {
                        entries
                            .flatten()
                            .filter_map(|entry| {
                                entry.path().file_stem().map(|s| s.to_string_lossy().into_owned())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                bars.instruments().map_err(|err| err.to_string())?
            };
            for name in names {
                if let Ok(Some((from, to))) = bars.coverage(&name, interval) {
                    out.push(json!({
                        "instrument": name,
                        "interval": interval.to_string(),
                        "from": from.to_string(),
                        "to": to.to_string(),
                    }));
                }
            }
        }
        Ok(json!({ "instruments": out }))
    }

    fn list_findings(&self) -> Result<Value, String> {
        let (summaries, unreadable) = self.store().summaries().map_err(|err| err.to_string())?;
        Ok(json!({
            "findings": summaries.iter().map(|s| json!({
                "id": s.id,
                "kind": s.kind,
                "subject": s.subject,
                "verdict": s.verdict,
                "recorded_at": s.recorded_at.to_rfc3339(),
                "agent": s.agent,
            })).collect::<Vec<_>>(),
            "unreadable": unreadable.len(),
        }))
    }

    fn run(&self, instrument: &str, strategy: &str, rolling: bool) -> Result<Value, String> {
        let plan = StrategyPlan::find(strategy)
            .ok_or_else(|| format!("no strategy {strategy:?}; list_strategies says what there is"))?;
        if plan.ranks_a_set() {
            return Err(format!(
                "{strategy} ranks instruments against each other and cannot be run on one"
            ));
        }
        let interval = plan.interval();
        let bars = CsvBars::new(&self.data);
        let missing = || format!("{instrument} holds no {interval} bars, the resolution {strategy} runs at");
        let (from, to) = bars
            .coverage(instrument, interval)
            .map_err(|err| err.to_string())?
            .ok_or_else(missing)?;
        let fingerprint = bars
            .fingerprint(instrument, interval)
            .map_err(|err| err.to_string())?
            .ok_or_else(missing)?;
        let window = DateRange::new(from, to).map_err(|err| err.to_string())?;
        let simulation = NautilusSimulation::new(CsvBars::new(&self.data));
        let criteria = EvaluationCriteria::default();

        let record = if rolling {
            let procedure = walk_forward_for(instrument, plan, window, &fingerprint);
            Record::WalkForward(Box::new(
                arvo_research::run_walk_forward(&simulation, &procedure, &criteria)
                    .map_err(|err| err.to_string())?,
            ))
        } else {
            let family = study_for(instrument, plan, window, &fingerprint);
            Record::Study(Box::new(
                arvo_research::run_family(&simulation, &family, &criteria)
                    .map_err(|err| err.to_string())?,
            ))
        };

        let store = self.store();
        let history = store.load().map_err(|err| err.to_string())?.records;
        let stored = StoredRecord::by_agent(record, &self.agent(), &history, chrono::Utc::now());
        store.save(&stored).map_err(|err| err.to_string())?;
        Ok(summarize(&stored))
    }

    /// Appends one line to the audit trail. Best effort, loudly: a trail that
    /// cannot be written is said on stderr, and never fails the call — the
    /// finding, which is the durable record, is already saved.
    fn record(&self, tool: &str, arguments: &Value, outcome: &Result<Value, String>) {
        let line = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "agent": self.agent(),
            "tool": tool,
            "arguments": arguments,
            "ok": outcome.is_ok(),
            "finding": outcome.as_ref().ok().and_then(|value| value.get("id")).cloned(),
            "error": outcome.as_ref().err(),
        });
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(err) = written {
            eprintln!("could not append to {}: {err}", self.audit.display());
        }
    }
}

/// What an agent needs to read a finding, and nothing it could mistake for
/// more: the verdict before the numbers, the advice beside them, and who ran it
/// against what bar.
fn summarize(stored: &StoredRecord) -> Value {
    let record = &stored.record;
    let mut out = json!({
        "id": stored.id,
        "kind": record.kind(),
        "subject": record.subject(),
        "verdict": record.verdict(),
        "recorded_at": stored.recorded_at.to_rfc3339(),
        "author": stored.author,
        "read_this_first": match record.verdict() {
            Verdict::Supported => "Supported is the one verdict worth acting on, and only as far as the advice below allows.",
            Verdict::NotSupported => "Not supported: the numbers below describe a rule that did not clear its bar. Do not report them as an edge.",
            Verdict::Inconclusive => "Inconclusive: too little evidence to say either way. Do not report the numbers below as a result.",
        },
    });
    match record {
        Record::Study(found) => {
            let evaluation = &found.out_of_sample_evidence.evaluation;
            out["reasons"] = json!(found.reasons);
            out["advice"] = advice(arvo_research::recommend(found));
            out["search"] = json!({
                "trials": found.selection.trials,
                "prior_trials": found.selection.prior_trials,
                "best_in_sample_sharpe": found.selection.best_sharpe,
                "expected_best_under_null": found.selection.expected_best_under_null,
                "survived_deflation": found.selection.survived_deflation,
                "chosen": found.selected.strategy.params,
            });
            out["out_of_sample"] = json!({
                "from": found.out_of_sample.from.to_string(),
                "to": found.out_of_sample.to.to_string(),
                "trades": evaluation.strategy.trades,
                "total_return": evaluation.strategy.total_return,
                "excess_return": evaluation.excess_return,
                "sharpe": evaluation.strategy.sharpe,
                "max_drawdown": evaluation.strategy.max_drawdown,
                "refused_orders": evaluation.refused_orders,
            });
        }
        Record::WalkForward(found) => {
            out["reasons"] = json!(found.reasons);
            out["advice"] = advice(arvo_research::recommend_walk_forward(found));
            out["combined"] = json!({
                "folds": found.folds.len(),
                "folds_surviving_deflation": found.folds_surviving_deflation,
                "trades": found.combined_trades.closed,
                "total_return": found.combined.total_return,
                "excess_return": found.excess_return,
                "sharpe": found.combined.sharpe,
                "max_drawdown": found.combined.max_drawdown,
            });
        }
        Record::Panel(found) => {
            out["reasons"] = json!(found.reasons);
            out["pooled"] = json!({
                "instruments": found.pooled.instruments,
                "trades": found.pooled.total_trades,
                "mean_excess_return": found.pooled.mean_excess_return,
            });
        }
    }
    out
}

fn advice(items: Vec<arvo_research::Recommendation>) -> Value {
    json!(items
        .iter()
        .map(|item| json!({
            "severity": item.severity.label(),
            "finding": item.finding,
            "action": item.action,
            "evidence": item.evidence,
        }))
        .collect::<Vec<_>>())
}

fn tools() -> Value {
    let instrument_and_strategy = json!({
        "type": "object",
        "properties": {
            "instrument": { "type": "string", "description": "An instrument id from list_instruments, e.g. AAPL.RH" },
            "strategy": { "type": "string", "description": "A strategy name from list_strategies, e.g. sma_cross" },
        },
        "required": ["instrument", "strategy"],
    });
    let none = json!({ "type": "object", "properties": {} });
    json!([
        {
            "name": "list_strategies",
            "description": "The rules Arvo can test, each with the resolution it runs at and what it claims.",
            "inputSchema": none,
        },
        {
            "name": "list_instruments",
            "description": "Instruments with data in the library, by resolution and date range. Data is fetched from the Arvo window, not here.",
            "inputSchema": none,
        },
        {
            "name": "list_findings",
            "description": "Every finding in research memory: id, kind, subject, verdict, when, and which agent ran it (null for a person).",
            "inputSchema": none,
        },
        {
            "name": "open_finding",
            "description": "One finding: verdict, reasons, advice, the search it came from, and its out-of-sample numbers. Read the verdict and advice before any number.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "A finding id from list_findings" } },
                "required": ["id"],
            },
        },
        {
            "name": "run_study",
            "description": "Search a strategy's parameter grid on one instrument, choose in-sample, judge out-of-sample. Saved as your finding and deflated against every run you have made, so running until something passes does not make it pass. Takes seconds to a minute.",
            "inputSchema": instrument_and_strategy,
        },
        {
            "name": "run_walk_forward",
            "description": "Re-select a strategy on a rolling schedule across an instrument's whole history, and judge the stitched out-of-sample record. Slower than run_study by roughly the number of folds. Saved as your finding and deflated the same way.",
            "inputSchema": instrument_and_strategy,
        },
    ])
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> (tempfile::TempDir, Server) {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = Server::new(dir.path(), None);
        (dir, server)
    }

    fn call(server: &mut Server, request: Value) -> Value {
        serde_json::from_str(&server.handle_line(&request.to_string()).expect("a reply"))
            .expect("json")
    }

    #[test]
    fn initialize_names_the_agent_from_its_client() {
        let (_dir, mut server) = server();
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "clientInfo": { "name": "claude-code", "version": "1" } } }),
        );
        assert_eq!(reply["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(server.agent(), "claude-code");
    }

    #[test]
    fn a_pinned_agent_is_not_renamed_by_the_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut server = Server::new(dir.path(), Some("research-bot".to_owned()));
        call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "clientInfo": { "name": "claude-code" } } }),
        );
        assert_eq!(server.agent(), "research-bot");
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let (_dir, mut server) = server();
        let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert_eq!(server.handle_line(&line), None);
    }

    #[test]
    fn nothing_offered_can_fetch_share_or_trade() {
        // The boundary is the tool list. A tool that named a source, a key or
        // an order would be a way past it no argument check could close.
        let (_dir, mut server) = server();
        let reply = call(&mut server, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            ["list_strategies", "list_instruments", "list_findings", "open_finding", "run_study", "run_walk_forward"]
        );
        for forbidden in ["fetch", "order", "trade", "share", "import", "key", "sign"] {
            assert!(!names.iter().any(|name| name.contains(forbidden)), "{forbidden}");
        }
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_and_is_audited() {
        let (dir, mut server) = server();
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": "place_order", "arguments": { "symbol": "AAPL" } } }),
        );
        assert_eq!(reply["result"]["isError"], json!(true));
        let audit = std::fs::read_to_string(dir.path().join(AUDIT_FILE)).expect("audited");
        let line: Value = serde_json::from_str(audit.lines().last().expect("a line")).expect("json");
        assert_eq!(line["tool"], "place_order");
        assert_eq!(line["ok"], json!(false));
    }

    #[test]
    fn an_unknown_method_and_bad_json_are_protocol_errors() {
        let (_dir, mut server) = server();
        let reply = call(&mut server, json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }));
        assert_eq!(reply["error"]["code"], json!(-32601));
        let bad: Value =
            serde_json::from_str(&server.handle_line("{not json").expect("a reply")).expect("json");
        assert_eq!(bad["error"]["code"], json!(-32700));
    }

    #[test]
    fn a_missing_argument_says_which() {
        let (_dir, mut server) = server();
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": { "name": "run_study", "arguments": { "strategy": "sma_cross" } } }),
        );
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("instrument"));
    }
}
