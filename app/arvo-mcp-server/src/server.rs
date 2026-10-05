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
//! Both facts are the engine's, not this program's (#151). Every tool is a
//! call on the engine's `Research` service, made with the research token
//! from `engine.json`, and that token reaches no other service. This server
//! could not fetch or trade if it tried; it holds nothing that can.
//!
//! # Why every run is the agent's, and deflated
//!
//! An agent running study after study until one comes out `Supported` is an
//! unbounded search nothing else counts (#25). So a run is saved as the
//! agent's, held to the bar for everything this agent has tried, and the
//! result says so. The agent's name is who it is across sessions: the
//! client's own name from `initialize`, or `--agent`.
//!
//! # The audit trail (#33)
//!
//! The engine appends every run to `agent-audit.jsonl` beside the evidence,
//! once, whichever front end asked: when, which agent, the arguments, whether
//! it worked, and the finding it produced. This server no longer writes a
//! line of its own, so a run reached through MCP and one reached through
//! the Python client are audited the same way.

use std::path::Path;
use std::time::{Duration, Instant};

use arvo_client::discovery::{self, Discovery};
use arvo_client::proto::common::Empty;
use arvo_client::proto::research::{BarsRequest, Finding, FindingId, FindingIds, PanelRequest, Param, PineScript, RankRequest, RuleText, ReviewRequest, RulesetForm, RunRequest};
use arvo_client::proto::services::research_client::ResearchClient;
use serde_json::{json, Value};
use tonic::transport::Channel;

/// The protocol revision this speaks, the one `arvo-mcp` speaks as a client.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How to do research here, given to the client at `initialize`. The loop
/// an agent runs, and the two facts that make it honest: most results are
/// noise, and every run counts against you.
const INSTRUCTIONS: &str = "\
Arvo's research memory and research runs. Nothing here fetches data or trades.

The loop: read list_findings and open_finding before proposing anything, so you \
do not re-run what is already known. State a hypothesis in one sentence. Write it \
as a ruleset with write_ruleset (one of Arvo's rules, the parameters you fix, the \
axes you search - keep the search small, a dozen configurations is plenty). Run it \
with run_study on one instrument, then open_finding and read the verdict, the \
reasons, data_findings and the advice before any number. data_findings says what \
is wrong with the bars the finding rests on - a gap, a suspected unadjusted split, \
a stalled feed - and a fault there undermines every number after it, so say so \
when you report the finding. NotSupported and Inconclusive are the ordinary \
outcomes and are evidence: say what the run ruled out. Change one thing at a time \
and say why.

Every run is saved as your finding and deflated against everything you have run, \
so running until something passes does not make it pass; a Supported verdict that \
survives that is worth reporting, and one that does not is not. Report what you \
found, what you ruled out, and what you would try next.";

/// How long to wait for an engine this server started to write `engine.json`.
const START_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Server {
    /// The calls are async; the protocol loop is a line at a time. One
    /// runtime, one call in flight.
    runtime: tokio::runtime::Runtime,
    research: ResearchClient<Channel>,
    /// The research token, and only that one.
    token: String,
    /// Who is running, once known.
    agent: Option<String>,
    /// Set by `--agent`, and then not overridden by the client's name.
    pinned: bool,
}

impl Server {
    /// Connects to the engine `root`'s `engine.json` names, starting one when
    /// none is answering. `explicit` says whether `root` was given on the
    /// command line, in which case an engine this starts is told to use it.
    ///
    /// # Errors
    ///
    /// No engine is running and none could be started, or the connection
    /// failed.
    pub fn connect(root: &Path, explicit: bool, agent: Option<String>) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("starting a runtime: {err}"))?;
        let found = match discovery::running(root) {
            Some(found) => found,
            None => start_engine(root, explicit)?,
        };
        let channel = runtime
            .block_on(async {
                Channel::from_shared(found.endpoint())
                    .map_err(|err| format!("the engine's address is not a url: {err}"))?
                    .connect()
                    .await
                    .map_err(|err| format!("connecting to the engine at {}: {err}", found.address))
            })?;
        let research = ResearchClient::new(channel).max_decoding_message_size(arvo_client::wire::MAX_MESSAGE_BYTES);
        Ok(Self { runtime, research, token: found.token, pinned: agent.is_some(), agent })
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
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => result(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                result(
                    id,
                    match self.call(name, &arguments) {
                        // `structuredContent` must be an object (MCP schema);
                        // a list goes under `items`, the text keeps the list.
                        Ok(value) => json!({
                            "content": [{ "type": "text", "text": pretty(&value) }],
                            "structuredContent": if value.is_array() { json!({ "items": value }) } else { value },
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

    /// `message`, carrying the research token and the agent's name.
    ///
    /// The name is what the engine's audit trail reads for a call that does
    /// not record a finding: writing a rule, translating a script (#15). A
    /// run says it again in the request, because there it decides whose
    /// finding it is. A name that cannot be sent as a header is left off
    /// rather than failing every call the agent makes.
    fn request<T: Clone>(&self, message: T) -> Result<tonic::Request<T>, String> {
        arvo_client::request_as(&self.token, &self.agent(), message.clone())
            .or_else(|_| arvo_client::request(&self.token, message))
            .map_err(|err| err.to_string())
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| format!("{name} needs a string argument {key:?}"))
        };
        match name {
            "list_strategies" => {
                let request = self.request(Empty {})?;
                let listed = self.runtime.block_on(self.research.list_strategies(request)).map_err(refused)?;
                encode(&listed.into_inner().strategies)
            }
            "list_instruments" => {
                let request = self.request(Empty {})?;
                let listed = self.runtime.block_on(self.research.list_instruments(request)).map_err(refused)?;
                Ok(json!({ "instruments": encode(&listed.into_inner().instruments)? }))
            }
            "list_rulesets" => {
                let request = self.request(Empty {})?;
                let listed = self.runtime.block_on(self.research.list_rulesets(request)).map_err(refused)?;
                encode(&listed.into_inner().rulesets)
            }
            "list_rules" => {
                let request = self.request(Empty {})?;
                let listed = self.runtime.block_on(self.research.list_rule_files(request)).map_err(refused)?;
                encode(&listed.into_inner().rules)
            }
            "translate_pine" => {
                let asked = PineScript {
                    text: text("script")?,
                    interval: arguments.get("interval").and_then(Value::as_str).map(ToOwned::to_owned),
                };
                let request = self.request(asked)?;
                let translated = self.runtime.block_on(self.research.translate_pine(request)).map_err(refused)?;
                encode(&translated.into_inner())
            }
            "write_rule" => {
                // The definition's own JSON: the engine is the one parser.
                let json = match arguments.get("rule") {
                    Some(Value::String(text)) => text.clone(),
                    Some(value) => serde_json::to_string(value).map_err(|err| err.to_string())?,
                    None => return Err("write_rule needs rule: the definition, as an object or its JSON".to_owned()),
                };
                let request = self.request(RuleText { json })?;
                let written = self.runtime.block_on(self.research.write_rule(request)).map_err(refused)?;
                encode(&written.into_inner())
            }
            "write_ruleset" => {
                let numbers = |key: &str| -> Result<std::collections::BTreeMap<String, f64>, String> {
                    match arguments.get(key) {
                        None | Some(Value::Null) => Ok(Default::default()),
                        Some(value) => serde_json::from_value(value.clone())
                            .map_err(|err| format!("{key} is an object of parameter name to number: {err}")),
                    }
                };
                let axes: std::collections::BTreeMap<String, Vec<f64>> = match arguments.get("axes") {
                    None | Some(Value::Null) => Default::default(),
                    Some(value) => serde_json::from_value(value.clone())
                        .map_err(|err| format!("axes is an object of parameter name to a list of numbers: {err}"))?,
                };
                let optional = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                // A parameter with one value is fixed; with several, searched.
                // The same form the window's editor sends.
                let mut params: Vec<Param> =
                    numbers("fixed")?.into_iter().map(|(name, value)| Param { name, values: vec![value] }).collect();
                params.extend(axes.into_iter().map(|(name, values)| Param { name, values }));
                let form = RulesetForm {
                    name: text("name")?,
                    rule: text("rule")?,
                    label: optional("label"),
                    premise: optional("premise"),
                    params,
                };
                let request = self.request(form)?;
                let written = self.runtime.block_on(self.research.write_ruleset(request)).map_err(refused)?;
                encode(&written.into_inner())
            }
            "list_findings" => {
                let request = self.request(Empty {})?;
                let listed = self.runtime.block_on(self.research.list_findings(request)).map_err(refused)?;
                encode(&listed.into_inner())
            }
            "open_finding" => {
                let request = self.request(FindingId { id: text("id")? })?;
                let found = self.runtime.block_on(self.research.open_finding(request)).map_err(refused)?;
                Ok(finding_json(found.into_inner()))
            }
            "query_market_data" | "inspect_regime" => {
                let optional = |key: &str| arguments.get(key).and_then(Value::as_str).map(ToOwned::to_owned);
                let asked = BarsRequest {
                    instrument: text("instrument")?,
                    interval: optional("interval"),
                    from: optional("from"),
                    to: optional("to"),
                    last: arguments.get("last").and_then(Value::as_u64).and_then(|last| u32::try_from(last).ok()),
                };
                let request = self.request(asked)?;
                if name == "query_market_data" {
                    let bars = self.runtime.block_on(self.research.read_bars(request)).map_err(refused)?;
                    encode(&bars.into_inner())
                } else {
                    let regimes = self.runtime.block_on(self.research.view_regime(request)).map_err(refused)?;
                    encode(&regimes.into_inner())
                }
            }
            "read_review" => {
                let day = arguments.get("day").and_then(Value::as_str).map(ToOwned::to_owned);
                let request = self.request(ReviewRequest { day })?;
                let reviewed = self.runtime.block_on(self.research.view_review(request)).map_err(refused)?;
                encode(&reviewed.into_inner())
            }
            "rank_findings" => {
                let text = |key: &str| arguments.get(key).and_then(Value::as_str).map(ToOwned::to_owned);
                let request = self.request(RankRequest { rule: text("rule"), instrument: text("instrument") })?;
                let ranking = self.runtime.block_on(self.research.rank_findings(request)).map_err(refused)?;
                encode(&ranking.into_inner())
            }
            "compare_experiments" => {
                let ids: Vec<String> = match arguments.get("ids") {
                    Some(Value::Array(ids)) => ids.iter().filter_map(Value::as_str).map(ToOwned::to_owned).collect(),
                    _ => Vec::new(),
                };
                if ids.len() < 2 {
                    return Err("compare_experiments needs ids: a list of at least two finding ids".to_owned());
                }
                let request = self.request(FindingIds { ids })?;
                let mut compared = self.runtime.block_on(self.research.view_comparison(request)).map_err(refused)?.into_inner();
                // The curves are for drawing; the rows and the deflation of
                // the comparison itself are what an agent reasons from.
                compared.curves.clear();
                encode(&compared)
            }
            "run_panel" => {
                // The agent's, like a study: a panel is the widest search
                // there is, and it counts against whoever ran it (#15).
                let asked = PanelRequest {
                    universe: text("universe")?,
                    strategy: arguments.get("strategy").and_then(Value::as_str).map(ToOwned::to_owned),
                    author: self.agent(),
                    ..Default::default()
                };
                let request = self.request(asked)?;
                let view = self.runtime.block_on(self.research.run_panel(request)).map_err(refused)?;
                encode(&view.into_inner())
            }
            "run_study" | "run_walk_forward" => {
                let asked = RunRequest {
                    instrument: text("instrument")?,
                    strategy: text("strategy")?,
                    author: self.agent(),
                    ..Default::default()
                };
                let request = self.request(asked)?;
                let found = if name == "run_study" {
                    self.runtime.block_on(self.research.run_study(request))
                } else {
                    self.runtime.block_on(self.research.run_walk_forward(request))
                }
                .map_err(refused)?;
                Ok(finding_json(found.into_inner()))
            }
            other => Err(format!("no tool {other:?}; tools/list says what there is")),
        }
    }
}

/// Starts an engine for `root` and waits for it to answer.
///
/// The binary is `ARVO_ENGINE`, else `arvo-engine` beside this executable,
/// which is where a release archive puts it. It is told the root only when
/// the person named one; otherwise it finds the app data directory and the
/// open project the way the window's engine does.
fn start_engine(root: &Path, explicit: bool) -> Result<Discovery, String> {
    let binary = match std::env::var_os("ARVO_ENGINE") {
        Some(named) => std::path::PathBuf::from(named),
        None => {
            let exe = std::env::current_exe().map_err(|err| format!("could not locate this program: {err}"))?;
            exe.with_file_name(format!("arvo-engine{}", std::env::consts::EXE_SUFFIX))
        }
    };
    if !binary.is_file() {
        return Err(format!(
            "no engine is running for {} and {} is missing; start arvo-engine, or set ARVO_ENGINE to one",
            root.display(),
            binary.display()
        ));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("engine.log"))
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    let mut command = std::process::Command::new(&binary);
    if explicit {
        command.arg(root);
    }
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window, and its own process group: it outlives this
        // server, as one engine per user means (ADR-0018).
        command.creation_flags(0x0800_0000 | 0x0000_0200);
        keep_the_protocol_pipes_to_ourselves();
    }
    command.spawn().map_err(|err| format!("could not start {}: {err}", binary.display()))?;
    eprintln!("arvo-mcp-server: started {}", binary.display());

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(found) = discovery::running(root) {
            return Ok(found);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("{} did not write {} within {:?}", binary.display(), discovery::FILE, START_TIMEOUT))
}

/// Marks this process's own stdin, stdout and stderr as not inheritable.
///
/// Windows hands a child every inheritable handle the parent holds, not
/// only the three it is given as its own. Without this the engine inherited
/// the pipes the MCP client speaks to this server over, and since the engine
/// outlives the server on purpose, the client never saw those pipes close:
/// it waited on a server that had already exited.
#[cfg(windows)]
fn keep_the_protocol_pipes_to_ourselves() {
    use std::os::windows::io::AsRawHandle as _;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    for handle in [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()] {
        // SAFETY: a documented kernel32 call on a handle this process owns;
        // it changes a flag on the handle and nothing else.
        unsafe {
            SetHandleInformation(handle.cast(), HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// A refusal, in the engine's words.
fn refused(status: tonic::Status) -> String {
    status.message().to_owned()
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| err.to_string())
}

/// A finding as the tools return it: the summary's fields at the top, the
/// verdict's reading, the advice, and the numbers the engine sent as JSON
/// merged in under their own names, so `search`, `out_of_sample` and
/// `combined` read as they always did.
fn finding_json(found: Finding) -> Value {
    let summary = found.summary.unwrap_or_default();
    let mut out = json!({
        "id": summary.id,
        "kind": summary.kind,
        "subject": summary.subject,
        "verdict": summary.verdict,
        "recorded_at": summary.recorded_at,
        "author": summary.author,
        "strategy": summary.strategy,
        "code_commit": summary.code_commit,
        "ruleset_hash": summary.ruleset_hash,
        "read_this_first": found.read_this_first,
        "reasons": found.reasons,
        // What is wrong with the bars under it (#16), ahead of the advice and
        // the numbers: a verdict is only as good as its series.
        "data_findings": found.data_findings,
        "advice": found.advice,
        "attachments": found.attachments,
    });
    if let Ok(Value::Object(detail)) = serde_json::from_str::<Value>(&found.detail_json) {
        if let Value::Object(top) = &mut out {
            for (key, value) in detail {
                top.entry(key).or_insert(value);
            }
        }
    }
    out
}

fn tools() -> Value {
    let instrument_and_strategy = json!({
        "type": "object",
        "properties": {
            "instrument": { "type": "string", "description": "An instrument id from list_instruments, e.g. AAPL.RH" },
            "strategy": { "type": "string", "description": "A strategy name from list_strategies, or a ruleset name from list_rulesets" },
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
            "name": "list_rulesets",
            "description": "The rulesets in the project: a rule with its fixed parameters and the axes a study searches, as files. Each says whether it can run and why not.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "list_rules",
            "description": "The project's rules written as data: each file's name, interval, indicators, parameter defaults, entry and exit read back as a sentence, and its content hash. A file that cannot run is listed with the reason instead. These are the rules a ruleset may name, beside Arvo's own from list_strategies.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "translate_pine",
            "description": "Translates a Pine v5 strategy into a rule this engine can study. Translates the subset the rule language says — ta.sma, ta.atr, ta.highest, ta.lowest, ta.crossover, ta.crossunder, comparisons, and/or/not, input.* as parameters, strategy.entry and strategy.close — and refuses every other construct by name, listing all of them at once so the script can be rewritten. Drawing (plot and its neighbours) is read, set aside and listed, never silently dropped. It writes nothing: read the rule it returns, then pass it to write_rule to keep it. Reading many scripts and keeping one is a search of many, and your findings are deflated against it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "The Pine v5 source" },
                    "interval": { "type": "string", "description": "The resolution the rule runs at, e.g. 1day or 5minute; Pine takes it from the chart. 1day when absent." }
                },
                "required": ["script"]
            },
        },
        {
            "name": "write_rule",
            "description": "Writes a rule as data the engine evaluates exactly as it evaluates a compiled rule: named indicators (SMA, ATR, MAX, MIN; period a number or a parameter name) and entry and exit conditions in JSON Logic over them, with cross_above and cross_below as the two stateful operators, plus defaults for every number a ruleset's grid may vary. Refuses anything the engine would not run and says which construct. Then write_ruleset naming this rule, or run_study on the rule itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rule": {
                        "type": "object",
                        "description": "The definition: name, label, premise, interval {step, unit}, params, indicators, entry, exit. E.g. {\"name\":\"twin_cross\",\"interval\":{\"step\":1,\"unit\":\"day\"},\"params\":{\"fast\":10,\"slow\":30},\"indicators\":{\"fast\":{\"kind\":\"SMA\",\"period\":\"fast\"},\"slow\":{\"kind\":\"SMA\",\"period\":\"slow\"}},\"entry\":{\"cross_above\":[{\"var\":\"fast\"},{\"var\":\"slow\"}]},\"exit\":{\"cross_below\":[{\"var\":\"fast\"},{\"var\":\"slow\"}]}}"
                    }
                },
                "required": ["rule"]
            },
        },
        {
            "name": "write_ruleset",
            "description": "Writes a ruleset the project can study and a person can open in the editor: one of Arvo's rules from list_strategies, the parameters fixed, the axes searched. Refuses anything the engine would not run and says why. Replaces a ruleset of the same name. Then run_study with strategy set to the ruleset's name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Letters, digits, _ and -; not one of Arvo's own rule names" },
                    "rule": { "type": "string", "description": "One of Arvo's rules, e.g. sma_cross" },
                    "fixed": { "type": "object", "description": "Parameters every trial shares, name to number", "additionalProperties": { "type": "number" } },
                    "axes": { "type": "object", "description": "Parameters the study searches, name to the values tried. Keep it small.", "additionalProperties": { "type": "array", "items": { "type": "number" } } },
                    "label": { "type": "string", "description": "What to call it in a menu" },
                    "premise": { "type": "string", "description": "One line: the hypothesis this ruleset tests" }
                },
                "required": ["name", "rule"]
            },
        },
        {
            "name": "list_instruments",
            "description": "Instruments with data in the library, by resolution and date range. Data is fetched from the Arvo window, not here.",
            "inputSchema": none,
        },
        {
            "name": "list_findings",
            "description": "Every finding in research memory: id, kind, subject, verdict, when, who ran it (empty for a person), and which build and ruleset produced it.",
            "inputSchema": none,
        },
        {
            "name": "open_finding",
            "description": "One finding: verdict, reasons, data_findings (what is wrong with the bars it rests on), advice, the search it came from, and its out-of-sample numbers. Read the verdict, data_findings and advice before any number.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "A finding id from list_findings" } },
                "required": ["id"],
            },
        },
        {
            "name": "query_market_data",
            "description": "The library's bars for an instrument over a window: open, high, low, close, volume per bar. Read-only; nothing is fetched. What a study saw, for reading why it did what it did.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instrument": { "type": "string", "description": "An instrument id from list_instruments, e.g. AAPL.RH" },
                    "interval": { "type": "string", "description": "1day (default), or 5minute and other minute steps the library holds" },
                    "from": { "type": "string", "description": "YYYY-MM-DD, inclusive; the library's start when absent" },
                    "to": { "type": "string", "description": "YYYY-MM-DD, inclusive; today when absent" },
                    "last": { "type": "integer", "description": "At most this many bars from the end of the window. 60 when absent, 2000 at most" }
                },
                "required": ["instrument"]
            },
        },
        {
            "name": "inspect_regime",
            "description": "The regime each bar closed in over a window: trending up, trending down or ranging, labelled after the fact over the closes. Says what the market was doing, not what a rule could have known. Compare with the regime on each trade in open_finding.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instrument": { "type": "string", "description": "An instrument id from list_instruments" },
                    "interval": { "type": "string", "description": "1day (default), or a minute step the library holds" },
                    "from": { "type": "string", "description": "YYYY-MM-DD, inclusive" },
                    "to": { "type": "string", "description": "YYYY-MM-DD, inclusive" },
                    "last": { "type": "integer", "description": "At most this many bars from the end of the window. 60 when absent" }
                },
                "required": ["instrument"]
            },
        },
        {
            "name": "read_review",
            "description": "The review after the close for a day: what every session did (bars, signals, what the gate refused and why, fills against their decision prices, round trips with the rule and regime they opened in, losses grouped by condition, regime and exit, freezes, halts, verdict and warning changes) and what a person did (positions adopted, halts by hand, resumes, stops). Written after the close, or on first ask.",
            "inputSchema": {
                "type": "object",
                "properties": { "day": { "type": "string", "description": "YYYY-MM-DD (UTC); today when absent" } }
            },
        },
        {
            "name": "rank_findings",
            "description": "The leaderboard: every comparable finding in the one order Arvo ranks by. Supported under the conservative cost tier first, by mean profit per trade under that tier; then Supported under the stated costs where the conservative tier was never measured; then everything else whatever its return. Ties by drawdown, then trades. Each row: rule, instrument, interval, verdict, conservative verdict, expectancy and which costs it is under, return, drawdown, trades, regimes traded in, search size, whether its data has changed. Filter by rule or instrument.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rule": { "type": "string", "description": "A rule's name, as findings record it" },
                    "instrument": { "type": "string", "description": "SYMBOL.VENUE" }
                }
            },
        },
        {
            "name": "compare_experiments",
            "description": "Two or more findings read against each other: one row each with verdict, return, excess return, Sharpe, drawdown, trades, win rate, and whether its data has changed since; then the comparison's own deflation, because keeping the best of six is a search of size six. Explains why two findings differ without reading their files.",
            "inputSchema": {
                "type": "object",
                "properties": { "ids": { "type": "array", "items": { "type": "string" }, "description": "Finding ids from list_findings; at least two" } },
                "required": ["ids"]
            },
        },
        {
            "name": "run_study",
            "description": "Search a strategy's parameter grid on one instrument, choose in-sample, judge out-of-sample. Saved as your finding and deflated against every run you have made, so running until something passes does not make it pass. Takes seconds to a minute.",
            "inputSchema": instrument_and_strategy,
        },
        {
            "name": "run_panel",
            "description": "One rule, one parameter set, every member of a universe at once: does the rule hold across names? A universe is a file under the project's universes/ folder, chosen for a stated reason other than returns; the finding records the universe, its reason, its size as the search that keeping its best member would be, and that membership is today's. Members without a series yet are named and left out. Minutes for a hundred names.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "universe": { "type": "string", "description": "A universe's name: universes/<name>.json in the project" },
                    "strategy": { "type": "string", "description": "A rule from list_strategies; the default rule when absent" }
                },
                "required": ["universe"]
            },
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

    /// An engine in process, over a temporary directory, on its own thread
    /// and runtime, that `Server::connect` finds through `engine.json` the
    /// way it finds a real one. The token is the research token only.
    struct TestEngine {
        dir: tempfile::TempDir,
        _stop: tokio::sync::oneshot::Sender<()>,
        /// The engine's picker tables — the project's rules (#225) and what
        /// extensions contribute — are process-wide, and every engine here
        /// has a project root of its own. Two of them at once would replace
        /// each other's table between a write and the read that checks it, so
        /// an engine in this module is held one at a time.
        _picker: std::sync::MutexGuard<'static, ()>,
    }

    static PICKER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn engine() -> TestEngine {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                let research = arvo_engine::research::Research::new(&root);
                let events = tokio::sync::broadcast::channel(256).0;
                let sessions = std::sync::Arc::new(arvo_trading::Sessions::new(&root, events.clone(), std::sync::Arc::new(arvo_engine::venues::Brokers)));
                let tokens = arvo_engine::grpc::Tokens { research: "research-token".to_owned(), control: "control-token".to_owned() };
                let jobs = arvo_schedule::Jobs::new(std::sync::Arc::new(|future| {
                    let handle = tokio::spawn(future);
                    Box::new(move || handle.abort())
                }));
                let plugins = arvo_service::plugins::Plugins::start(&root, &jobs, |_| {}).await;
                let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
                let ticks = tokio::sync::broadcast::channel(16).0;
                let (stream, streaming) = arvo_service::stream::start(ticks.clone(), |_| {});
                tokio::spawn(streaming);
                discovery::write(&root, &Discovery { address, token: tokens.research.clone(), pid: std::process::id() })
                    .expect("engine.json");
                let engine = arvo_engine::grpc::Engine { research, sessions, events, jobs, plugins, stream, ticks, stop: stop_tx };
                ready_tx.send(()).expect("the test is waiting");
                arvo_engine::grpc::serve(listener, engine, &tokens, async {
                    let _ = stopped.await;
                })
                .await
                .expect("serves");
            });
        });
        ready_rx.recv().expect("the engine came up");
        TestEngine { dir, _stop: stop, _picker: PICKER.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
    }

    fn server(engine: &TestEngine, agent: Option<&str>) -> Server {
        Server::connect(engine.dir.path(), true, agent.map(ToOwned::to_owned)).expect("connects")
    }

    fn call(server: &mut Server, request: Value) -> Value {
        serde_json::from_str(&server.handle_line(&request.to_string()).expect("a reply")).expect("json")
    }

    #[test]
    fn initialize_names_the_agent_from_its_client() {
        let engine = engine();
        let mut server = server(&engine, None);
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
        let engine = engine();
        let mut server = server(&engine, Some("research-bot"));
        call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "clientInfo": { "name": "claude-code" } } }),
        );
        assert_eq!(server.agent(), "research-bot");
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let engine = engine();
        let mut server = server(&engine, None);
        let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert_eq!(server.handle_line(&line), None);
    }

    #[test]
    fn nothing_offered_can_fetch_share_or_trade() {
        // The boundary is the tool list, and behind it the token: this server
        // holds the research token, which reaches no service that could.
        let engine = engine();
        let mut server = server(&engine, None);
        let reply = call(&mut server, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            [
                "list_strategies",
                "list_rulesets",
                "list_rules",
                "translate_pine",
                "write_rule",
                "write_ruleset",
                "list_instruments",
                "list_findings",
                "open_finding",
                "query_market_data",
                "inspect_regime",
                "read_review",
                "rank_findings",
                "compare_experiments",
                "run_study",
                "run_panel",
                "run_walk_forward"
            ]
        );
        for forbidden in ["fetch", "order", "trade", "share", "import", "key", "sign"] {
            assert!(!names.iter().any(|name| name.contains(forbidden)), "{forbidden}");
        }
    }

    /// Every tool is a call on the engine. This one goes there and back:
    /// the file lands where the engine keeps them, and the engine's refusal
    /// comes back as the tool's.
    #[test]
    fn an_agent_can_write_a_ruleset_it_can_then_study_and_a_bad_one_is_refused() {
        let engine = engine();
        let mut server = server(&engine, None);
        let written = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": { "name": "write_ruleset", "arguments": {
                        "name": "agent_cross", "rule": "sma_cross",
                        "fixed": { "trade_size": 10 }, "axes": { "fast": [5, 10], "slow": [50] },
                        "premise": "a fast cross on a slow base" } } }),
        );
        assert_eq!(written["result"]["isError"], json!(false), "{written}");
        assert_eq!(written["result"]["structuredContent"]["searches"], json!(2));
        assert!(engine.dir.path().join("rulesets/agent_cross.json").exists());

        let listed = call(&mut server, json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                                               "params": { "name": "list_rulesets", "arguments": {} } }));
        assert_eq!(listed["result"]["structuredContent"]["items"][0]["name"], json!("agent_cross"));

        let strategies = call(&mut server, json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
                                                   "params": { "name": "list_strategies", "arguments": {} } }));
        let names: Vec<&str> = strategies["result"]["structuredContent"]["items"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|plan| plan["name"].as_str().expect("name"))
            .collect();
        assert!(names.contains(&"agent_cross"), "the ruleset is offered: {names:?}");

        let refused = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                    "params": { "name": "write_ruleset", "arguments": {
                        "name": "nothing", "rule": "sma_cross", "axes": { "fast": [] } } } }),
        );
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(refused["result"]["content"][0]["text"].as_str().expect("text").contains("fast"));
        assert!(!engine.dir.path().join("rulesets/nothing.json").exists());
    }

    #[test]
    fn an_agent_can_write_a_rule_as_data_then_a_ruleset_over_it() {
        let engine = engine();
        let mut server = server(&engine, None);
        let definition = json!({
            "name": "agent_twin",
            "label": "The twin, written by an agent",
            "premise": "the control, written down",
            "interval": { "step": 1, "unit": "day" },
            "params": { "fast": 10, "slow": 30 },
            "indicators": {
                "fast": { "kind": "SMA", "period": "fast" },
                "slow": { "kind": "SMA", "period": "slow" }
            },
            "entry": { "cross_above": [ { "var": "fast" }, { "var": "slow" } ] },
            "exit": { "cross_below": [ { "var": "fast" }, { "var": "slow" } ] }
        });
        let written = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 20, "method": "tools/call",
                    "params": { "name": "write_rule", "arguments": { "rule": definition } } }),
        );
        assert_eq!(written["result"]["isError"], json!(false), "{written}");
        let file = &written["result"]["structuredContent"];
        assert_eq!(file["path"], json!("rules/agent_twin.json"));
        assert_eq!(file["entry"], json!("fast crossed above slow"));
        assert!(engine.dir.path().join("rules/agent_twin.json").exists());

        let listed = call(&mut server, json!({ "jsonrpc": "2.0", "id": 21, "method": "tools/call",
                                               "params": { "name": "list_rules", "arguments": {} } }));
        assert_eq!(listed["result"]["structuredContent"]["items"][0]["name"], json!("agent_twin"));

        // The rule is offered, so a ruleset may search it.
        let over = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 22, "method": "tools/call",
                    "params": { "name": "write_ruleset", "arguments": {
                        "name": "agent_twin_grid", "rule": "agent_twin",
                        "axes": { "fast": [5, 10], "slow": [20, 30] } } } }),
        );
        assert_eq!(over["result"]["isError"], json!(false), "{over}");
        assert_eq!(over["result"]["structuredContent"]["searches"], json!(4));

        // A rule the engine could not evaluate is refused, and nothing is written.
        let mut broken = definition.clone();
        broken["name"] = json!("agent_broken");
        broken["entry"] = json!({ "cross_above": [ { "var": "rsi" }, { "var": "slow" } ] });
        let refused = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 23, "method": "tools/call",
                    "params": { "name": "write_rule", "arguments": { "rule": broken } } }),
        );
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(refused["result"]["content"][0]["text"].as_str().expect("text").contains("rsi"), "{refused}");
        assert!(!engine.dir.path().join("rules/agent_broken.json").exists());

        // Writing a rule is the start of a search, so each write is in the
        // trail under the agent's name, the refused one included (#15).
        let trail = audit(&engine);
        let said: Vec<(&str, &str, bool)> = trail
            .iter()
            .map(|line| {
                (line["agent"].as_str().expect("agent"), line["tool"].as_str().expect("tool"), line["ok"] == json!(true))
            })
            .collect();
        assert_eq!(
            said,
            vec![
                ("unnamed-agent", "write_rule", true),
                ("unnamed-agent", "write_ruleset", true),
                ("unnamed-agent", "write_rule", false),
            ],
            "{trail:?}"
        );
        assert_eq!(trail[0]["arguments"]["name"], json!("agent_twin"));
        assert!(trail[2]["error"].as_str().expect("why").contains("rsi"));
    }

    /// Every line of the engine's audit trail, in order.
    fn audit(engine: &TestEngine) -> Vec<Value> {
        std::fs::read_to_string(engine.dir.path().join("agent-audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a line of json"))
            .collect()
    }

    /// A panel an agent runs is the agent's (#15): saved under its name, held
    /// to everything it has run, and in the trail. It used to be saved as a
    /// person's, so an agent could run panels until one passed and nothing
    /// counted them.
    #[test]
    fn a_panel_an_agent_runs_is_charged_to_the_agent() {
        let engine = engine();
        // Three series that wander differently, long enough for the default
        // rule's slowest average and a split.
        let start = chrono::NaiveDate::from_ymd_opt(2023, 1, 2).expect("date").and_hms_opt(0, 0, 0).expect("time");
        let library = arvo_data::CsvBars::new(engine.dir.path().join("data"));
        for (name, phase) in [("AAA.YF", 0.0_f64), ("BBB.YF", 1.7), ("CCC.YF", 3.1)] {
            let bars: Vec<arvo_data::Bar> = (0..700)
                .map(|i| {
                    let t = f64::from(i);
                    let close = 100.0 + t * 0.05 + 12.0 * (t / 37.0 + phase).sin() + 3.0 * (t / 5.0 + phase).cos();
                    arvo_data::Bar {
                        at: start + chrono::Duration::days(i64::from(i)),
                        open: close - 0.2,
                        high: close + 0.6,
                        low: close - 0.6,
                        close,
                        volume: 10_000.0,
                    }
                })
                .collect();
            library.write(name, arvo_data::BarInterval::DAILY, &bars).expect("written");
        }
        std::fs::create_dir_all(engine.dir.path().join("universes")).expect("universes/");
        std::fs::write(
            engine.dir.path().join("universes/three.json"),
            r#"{ "name": "three", "reason": "three made-up series, for a test", "interval": {"step":1,"unit":"day"},
                 "instruments": ["AAA.YF", "BBB.YF", "CCC.YF"] }"#,
        )
        .expect("write");

        let mut server = server(&engine, Some("panel-agent"));
        let panels = |engine: &TestEngine| -> Vec<Value> {
            let mut found: Vec<Value> = std::fs::read_dir(engine.dir.path().join("evidence"))
                .expect("evidence/")
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .filter(|path| path.file_name().is_some_and(|name| name != "index.json"))
                .map(|path| serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json"))
                .collect();
            found.sort_by_key(|stored: &Value| stored["recorded_at"].as_str().map(ToOwned::to_owned));
            found
        };
        let run = |server: &mut Server, id: u32| {
            call(
                server,
                json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": { "name": "run_panel", "arguments": { "universe": "three" } } }),
            )
        };

        let first = run(&mut server, 30);
        assert_ne!(first["result"]["isError"], json!(true), "{first}");
        let stored = panels(&engine);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["record"]["kind"], json!("panel"));
        assert_eq!(stored[0]["author"]["by"], json!("agent"), "not a person's: {}", stored[0]["author"]);
        assert_eq!(stored[0]["author"]["id"], json!("panel-agent"));
        assert_eq!(stored[0]["author"]["search"]["prior_findings"], json!(0));
        let once = stored[0]["author"]["search"]["trials"].as_u64().expect("trials");
        assert!(once > 0);

        // Run again, and the second is held to both: the count that was
        // never kept is kept now.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = run(&mut server, 31);
        assert_ne!(second["result"]["isError"], json!(true), "{second}");
        let stored = panels(&engine);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1]["author"]["search"]["prior_findings"], json!(1));
        assert_eq!(stored[1]["author"]["search"]["trials"].as_u64(), Some(once * 2), "its own search and the one before");

        // And both are in the trail, each naming the finding it made.
        let trail = audit(&engine);
        assert_eq!(trail.len(), 2, "{trail:?}");
        for (line, kept) in trail.iter().zip(&stored) {
            assert_eq!(line["agent"], json!("panel-agent"));
            assert_eq!(line["tool"], json!("run_panel"));
            assert_eq!(line["arguments"]["universe"], json!("three"));
            assert_eq!(line["finding"], kept["id"]);
        }

        // A universe that does not exist is a refusal the trail also keeps.
        let missing = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 32, "method": "tools/call",
                    "params": { "name": "run_panel", "arguments": { "universe": "nowhere" } } }),
        );
        assert_eq!(missing["result"]["isError"], json!(true));
        let last = audit(&engine).pop().expect("a line");
        assert_eq!((last["tool"].as_str(), last["ok"] == json!(true)), (Some("run_panel"), false));
    }

    /// Seven hundred daily bars that wander, with the days in `missing` left
    /// out: a hole in the series, late enough to fall in what a study judges.
    fn wandering(phase: f64, missing: std::ops::Range<u32>) -> Vec<arvo_data::Bar> {
        let start = chrono::NaiveDate::from_ymd_opt(2023, 1, 2).expect("date").and_hms_opt(0, 0, 0).expect("time");
        (0..700)
            .filter(|day| !missing.contains(day))
            .map(|i| {
                let t = f64::from(i);
                let close = 100.0 + t * 0.05 + 12.0 * (t / 37.0 + phase).sin() + 3.0 * (t / 5.0 + phase).cos();
                arvo_data::Bar {
                    at: start + chrono::Duration::days(i64::from(i)),
                    open: close - 0.2,
                    high: close + 0.6,
                    low: close - 0.6,
                    close,
                    volume: 10_000.0,
                }
            })
            .collect()
    }

    /// An agent reads the verdict and the numbers, and until now nothing about
    /// the series under them (#16). The window showed a hole beside the chart;
    /// the reader most likely to take a number at face value never saw it.
    #[test]
    fn a_finding_tells_an_agent_what_is_wrong_with_its_bars() {
        let engine = engine();
        let library = arvo_data::CsvBars::new(engine.dir.path().join("data"));
        library.write("WHOLE.YF", arvo_data::BarInterval::DAILY, &wandering(0.0, 0..0)).expect("written");
        // Three weeks with nothing in them, in the last part of the history.
        library.write("HOLED.YF", arvo_data::BarInterval::DAILY, &wandering(1.7, 640..661)).expect("written");
        let mut server = server(&engine, Some("careful-agent"));
        let study = |server: &mut Server, id: u32, instrument: &str| {
            let reply = call(
                server,
                json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": { "name": "run_study", "arguments": { "instrument": instrument, "strategy": "sma_cross" } } }),
            );
            assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
            reply["result"]["structuredContent"].clone()
        };

        // A clean series says so by saying nothing, and says it as a list.
        let whole = study(&mut server, 40, "WHOLE.YF");
        assert_eq!(whole["data_findings"], json!([]), "{whole}");

        // The holed one names the hole, on the run's own reply.
        let holed = study(&mut server, 41, "HOLED.YF");
        let found = holed["data_findings"].as_array().expect("a list");
        assert_eq!(found.len(), 1, "{holed}");
        assert_eq!((found[0]["severity"].as_str(), found[0]["kind"].as_str()), (Some("suspect"), Some("gap")));
        assert!(found[0]["detail"].as_str().expect("detail").contains("nothing in between"), "{}", found[0]);

        // And again whenever the finding is opened, read from the library as
        // it is then.
        let opened = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 42, "method": "tools/call",
                    "params": { "name": "open_finding", "arguments": { "id": holed["id"] } } }),
        );
        assert_eq!(opened["result"]["structuredContent"]["data_findings"], holed["data_findings"]);
        assert!(
            opened["result"]["structuredContent"].get("search").is_some(),
            "and the numbers are still there, after it"
        );

        // A panel has many series. Each member worth a look gets one line
        // naming it, not every row of every member.
        std::fs::create_dir_all(engine.dir.path().join("universes")).expect("universes/");
        std::fs::write(
            engine.dir.path().join("universes/pair.json"),
            r#"{ "name": "pair", "reason": "two made-up series, for a test", "interval": {"step":1,"unit":"day"},
                 "instruments": ["WHOLE.YF", "HOLED.YF"] }"#,
        )
        .expect("write");
        let panel = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 43, "method": "tools/call",
                    "params": { "name": "run_panel", "arguments": { "universe": "pair" } } }),
        );
        assert_ne!(panel["result"]["isError"], json!(true), "{panel}");
        let id = panel["result"]["structuredContent"]["id"].clone();
        let opened = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 44, "method": "tools/call",
                    "params": { "name": "open_finding", "arguments": { "id": id } } }),
        );
        let found = opened["result"]["structuredContent"]["data_findings"].as_array().expect("a list").clone();
        assert_eq!(found.len(), 1, "{opened}");
        assert_eq!(found[0]["kind"], json!("summary"));
        assert_eq!(found[0]["detail"], json!("HOLED.YF: 1 worth a look (1 gap)"));
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error() {
        let engine = engine();
        let mut server = server(&engine, None);
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": "place_order", "arguments": { "symbol": "AAPL" } } }),
        );
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("place_order"));
    }

    #[test]
    fn an_unknown_method_and_bad_json_are_protocol_errors() {
        let engine = engine();
        let mut server = server(&engine, None);
        let reply = call(&mut server, json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }));
        assert_eq!(reply["error"]["code"], json!(-32601));
        let bad: Value = serde_json::from_str(&server.handle_line("{not json").expect("a reply")).expect("json");
        assert_eq!(bad["error"]["code"], json!(-32700));
    }

    #[test]
    fn a_missing_argument_says_which() {
        let engine = engine();
        let mut server = server(&engine, None);
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": { "name": "run_study", "arguments": { "strategy": "sma_cross" } } }),
        );
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("instrument"));
    }

    #[test]
    fn the_read_tools_look_at_the_library_and_at_findings_side_by_side() {
        let engine = engine();
        // Sixty daily bars that climb, so the tail is labelled trending up.
        let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).expect("date").and_hms_opt(0, 0, 0).expect("time");
        let bars: Vec<arvo_data::Bar> = (0..60)
            .map(|i| {
                let close = 100.0 + f64::from(i) * 0.8;
                arvo_data::Bar {
                    at: start + chrono::Duration::days(i64::from(i)),
                    open: close - 0.3,
                    high: close + 0.5,
                    low: close - 0.5,
                    close,
                    volume: 1_000.0,
                }
            })
            .collect();
        arvo_data::CsvBars::new(engine.dir.path().join("data"))
            .write("UP.SIM", arvo_data::BarInterval::DAILY, &bars)
            .expect("written");
        let mut server = server(&engine, None);

        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                    "params": { "name": "query_market_data", "arguments": { "instrument": "UP.SIM", "last": 5 } } }),
        );
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        let read = &reply["result"]["structuredContent"];
        assert_eq!(read["interval"], json!("1day"));
        assert_eq!(read["bars"].as_array().map(Vec::len), Some(5), "the last five: {read}");
        assert_eq!(read["bars"][4]["close"], json!(100.0 + 59.0 * 0.8));

        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
                    "params": { "name": "inspect_regime", "arguments": { "instrument": "UP.SIM", "last": 10 } } }),
        );
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        let regimes = &reply["result"]["structuredContent"];
        assert_eq!(regimes["current"], json!("trending up"), "{regimes}");
        assert_eq!(regimes["points"].as_array().map(Vec::len), Some(10));
        assert_eq!(regimes["shares"]["trending up"], json!(10), "the lookback was read before the window: {regimes}");

        // Nothing there says so by name; a comparison of one is not one.
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                    "params": { "name": "query_market_data", "arguments": { "instrument": "NOPE.SIM" } } }),
        );
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("NOPE.SIM"));
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/call",
                    "params": { "name": "compare_experiments", "arguments": { "ids": ["one"] } } }),
        );
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("at least two"));

        // A day with no session record reviews an empty day, and is written.
        let reply = call(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/call",
                    "params": { "name": "read_review", "arguments": { "day": "2026-09-21" } } }),
        );
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        let reviewed = &reply["result"]["structuredContent"];
        assert_eq!(reviewed["day"], json!("2026-09-21"));
        assert_eq!(reviewed["written_now"], json!(true));
        assert!(reviewed["markdown"].as_str().expect("text").starts_with("# Review · 2026-09-21"));
        assert!(engine.dir.path().join("reviews").join("2026-09-21.md").exists());
    }

    /// No engine answers and none can be started: the error names what to do.
    #[test]
    fn without_an_engine_the_error_says_how_to_get_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ARVO_ENGINE", dir.path().join("nowhere").to_string_lossy().to_string());
        let Err(err) = Server::connect(dir.path(), true, None) else { panic!("connected to nothing") };
        std::env::remove_var("ARVO_ENGINE");
        assert!(err.contains("ARVO_ENGINE"), "{err}");
    }
}
