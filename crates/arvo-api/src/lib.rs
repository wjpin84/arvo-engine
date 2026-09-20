//! The contract's types: what the engine is made of, and what a front end renders.
//!
//! # Why this is a crate and not two mirrored files
//!
//! It used to be two. `arvo-runtime` defined these with `Serialize`, the
//! editor redefined all twenty-six of them by hand with `Deserialize`, and
//! nothing checked that the two agreed — a mismatch is invisible to the
//! compiler and appears at runtime as a deserialisation error, if it appears
//! at all.
//!
//! It cost two bugs in one afternoon. `CurvePoint.time` became `i64` on one
//! side and stayed `String` on the other, which broke every study the app
//! could run; and the editor never declared `WalkForwardView.recommendations`
//! at all, so serde quietly ignored the field and it could never have been
//! shown. The second was found only by writing a throwaway script to diff the
//! two files, which is not a thing anyone will remember to do again.
//!
//! One definition removes the class. That is the whole argument.
//!
//! # What belongs here
//!
//! Data, and nothing else. No behaviour, no dependency on the research
//! domain, no knowledge of Tauri or the DOM. A method here would be a place
//! for the two sides to start disagreeing about what a number means, and a
//! dependency here is one the WebAssembly build has to carry.
//!
//! Conversions from domain types live in `arvo-runtime`, as free functions
//! rather than `From` impls — with the types here and the domain there,
//! neither is local to that crate and the orphan rule forbids it. That is the
//! rule doing its job.

/// Generated from the contract in `contract/protos`: every message package,
/// each domain's models and the views a front end renders of them.
///
/// Generated rather than written here: a front end in any language reads the
/// same files, which is what makes the contract a contract rather than a Rust
/// crate two repositories happen to share (#127).
///
/// The module tree mirrors the package names, because that is how a type in
/// one package refers to a type in another. The services are not here;
/// `arvo-client` generates those and points them back at these types.
///
/// The lint allows are for generated code: a oneof over findings has one
/// variant far larger than the others, and that is what the shape is.
#[allow(clippy::large_enum_variant, clippy::doc_markdown, clippy::derive_partial_eq_without_eq)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/arvo.rs"));
}

/// One module per domain, for code that wants to say which it means.
pub use generated::arvo::{
    common::v1 as common, market::v1 as market, platform::v1 as platform,
    portfolio::v1 as portfolio, research::v1 as research, session::v1 as session,
};

/// Every shape at the root as well, because a window names a view on nearly
/// every line and `arvo_api::research::StudyView` would be noise. The names
/// do not collide: proto has no overloading and neither does this.
pub use generated::arvo::{
    common::v1::*, market::v1::*, platform::v1::*, portfolio::v1::*, research::v1::*,
    session::v1::*,
};

/// What the generated shapes cannot say for themselves.
///
/// proto3 makes every message field absent-able, so a nested shape arrives as
/// an `Option` even where it is always sent. These accessors say "the sender
/// always fills this", in one place, instead of at every reader.
mod ergonomics {
    use super::{
        platform::plugin_status_view, research::record_view,
        BookView, ImportView, MetricsView, PanelView, PluginStatusView, PluginStatusViewReachable,
        PluginStatusViewUnreachable, PluginView, PortfolioView, RecordView, ReportedView, StudyView,
        TradesView, WalkForwardView,
    };

    /// A count on the wire. Rust counts in `usize`; the contract is explicit
    /// about width, because a reader in another language has to be.
    #[must_use]
    pub fn count(n: usize) -> u32 {
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// A count read back, for code that indexes with it.
    #[must_use]
    pub fn size(n: u32) -> usize {
        usize::try_from(n).unwrap_or(usize::MAX)
    }

    macro_rules! always {
        ($owner:ty, $field:ident, $ty:ty) => {
            impl $owner {
                /// The sender always fills this.
                ///
                /// # Panics
                ///
                /// Only if it did not, which would mean the two sides disagree
                /// about the contract.
                #[must_use]
                pub fn $field(&self) -> &$ty {
                    self.$field.as_ref().expect(concat!(stringify!($owner), " always carries ", stringify!($field)))
                }
            }
        };
    }
    always!(StudyView, strategy, MetricsView);
    always!(StudyView, benchmark, MetricsView);
    always!(StudyView, trades_detail, TradesView);
    always!(WalkForwardView, strategy, MetricsView);
    always!(WalkForwardView, benchmark, MetricsView);
    always!(WalkForwardView, trades_detail, TradesView);
    always!(BookView, metrics, MetricsView);
    always!(PortfolioView, import, ImportView);

    impl PluginView {
        /// Whether the last probe reached it.
        #[must_use]
        pub fn reachable(&self) -> bool {
            matches!(self.status.as_ref().and_then(|s| s.of.as_ref()), Some(plugin_status_view::Of::Reachable(_)))
        }

        /// What it said it is, if the last probe reached it.
        #[must_use]
        pub fn reached(&self) -> Option<&PluginStatusViewReachable> {
            match self.status.as_ref().and_then(|status| status.of.as_ref()) {
                Some(plugin_status_view::Of::Reachable(status)) => Some(status),
                _ => None,
            }
        }

        /// Why the last probe did not reach it, if it did not.
        ///
        /// A plugin whose status is missing counts as unreached: the window
        /// says so rather than drawing it as healthy.
        #[must_use]
        pub fn unreachable(&self) -> Option<&str> {
            match self.status.as_ref().and_then(|status| status.of.as_ref()) {
                Some(plugin_status_view::Of::Unreachable(status)) => Some(&status.reason),
                Some(plugin_status_view::Of::Reachable(_)) => None,
                None => Some("the engine sent no status"),
            }
        }
    }

    impl PluginStatusView {
        /// It answered, and said what it is.
        #[must_use]
        pub fn reachable(name: String, version: String, capabilities: Vec<String>) -> Self {
            Self {
                of: Some(plugin_status_view::Of::Reachable(PluginStatusViewReachable { name, version, capabilities })),
            }
        }

        /// It did not answer, and why.
        #[must_use]
        pub fn unreachable(reason: String) -> Self {
            Self { of: Some(plugin_status_view::Of::Unreachable(PluginStatusViewUnreachable { reason })) }
        }
    }

    impl RecordView {
        /// A study, as a reopened record.
        #[must_use]
        pub fn study(view: StudyView) -> Self {
            Self { of: Some(record_view::Of::Study(view)) }
        }

        /// A walk-forward, as a reopened record.
        #[must_use]
        pub fn walk_forward(view: WalkForwardView) -> Self {
            Self { of: Some(record_view::Of::Walkforward(view)) }
        }

        /// A panel, as a reopened record.
        #[must_use]
        pub fn panel(view: PanelView) -> Self {
            Self { of: Some(record_view::Of::Panel(view)) }
        }

        /// Evidence Arvo did not compute, as a reopened record.
        #[must_use]
        pub fn reported(view: ReportedView) -> Self {
            Self { of: Some(record_view::Of::Reported(view)) }
        }
    }
}

pub use ergonomics::{count, size};

use serde::{Deserialize, Serialize};

/// A strategy the workbench can run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyView {
    pub name: String,
    pub label: String,
    pub premise: String,
    /// Spelled out rather than a boolean, because it is the reason a study
    /// may refuse an instrument the sidebar just listed.
    pub interval: String,
    /// Backtests one study will run. A spinner that says how much work is
    /// coming is the difference between waiting and suspecting a hang.
    pub backtests: usize,
}

/// One saved arrangement of the window.
///
/// # A list, not a map
///
/// Deliberately, and for the reason recorded in `arvo_runtime::session`: a
/// Rust map crossing into the webview through `serde_wasm_bindgen` becomes a
/// JavaScript `Map` rather than a plain object by default, which already cost
/// this app one wiped workspace. A list has no such trap, and it keeps the
/// order they were created in — which is the order someone expects to see
/// their own workspaces listed.
/// The open project folder, where a person's scripts live (#121).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectFolderView {
    /// The folder's full path, for display.
    pub root: String,
    /// Its last path segment.
    pub name: String,
}

/// Whether the open project has a Python language server (#124).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanguageServerView {
    pub running: bool,
    /// The project folder as a `file://` URI, the server's workspace.
    pub root_uri: Option<String>,
    /// Why there is none, as an instruction.
    pub reason: Option<String>,
}

/// One file or folder in the project folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntryView {
    pub name: String,
    /// Relative to the project folder, `/`-separated. What every project
    /// command takes back.
    pub path: String,
    pub is_dir: bool,
}

/// One setting the workbench knows (ADR-0020): its key in VS Code's spelling
/// where the meaning matches, the kind of value it takes with the default,
/// and a line for the Settings tab. The runtime validates a write against
/// this and the tab renders from it, so there is one list.
#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    pub key: &'static str,
    pub default: SettingDefault,
    pub description: &'static str,
}

/// The kind of value a setting takes, carrying its default.
#[derive(Debug, Clone, Copy)]
pub enum SettingDefault {
    Bool(bool),
    Number(f64),
    Text(&'static str),
    /// The default, and every value allowed.
    Choice(&'static str, &'static [&'static str]),
    /// The default, and the values this build ships — but not a closed set:
    /// an extension may contribute more (ADR-0024), so a value outside the
    /// list is accepted. Validating against the list would have the window
    /// offer a theme the runtime then refuses to store, which is exactly
    /// what it did.
    OpenChoice(&'static str, &'static [&'static str]),
}

impl SettingSpec {
    #[must_use]
    pub fn by_key(key: &str) -> Option<&'static Self> {
        SETTINGS.iter().find(|spec| spec.key == key)
    }

    /// Whether `value` is the kind this setting takes.
    #[must_use]
    pub fn accepts(&self, value: &serde_json::Value) -> bool {
        match self.default {
            SettingDefault::Bool(_) => value.is_boolean(),
            SettingDefault::Number(_) => value.is_number(),
            SettingDefault::Text(_) => value.is_string(),
            SettingDefault::Choice(_, allowed) => value.as_str().is_some_and(|v| allowed.contains(&v)),
            SettingDefault::OpenChoice(..) => value.is_string(),
        }
    }

    #[must_use]
    pub fn default_value(&self) -> serde_json::Value {
        match self.default {
            SettingDefault::Bool(v) => serde_json::Value::Bool(v),
            SettingDefault::Number(v) => serde_json::json!(v),
            SettingDefault::Text(v) | SettingDefault::Choice(v, _) | SettingDefault::OpenChoice(v, _) => {
                serde_json::Value::String(v.to_owned())
            }
        }
    }
}

/// Every setting, in the order the Settings tab shows them. Only keys with
/// something behind them: a setting that changes nothing is a lie in a form.
pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec { key: "editor.fontSize", default: SettingDefault::Number(13.0), description: "The font size of the code editor, in pixels." },
    SettingSpec { key: "editor.fontFamily", default: SettingDefault::Text("Consolas, 'Courier New', monospace"), description: "The font family of the code editor." },
    SettingSpec { key: "editor.tabSize", default: SettingDefault::Number(4.0), description: "The number of spaces a tab is equal to." },
    SettingSpec { key: "editor.insertSpaces", default: SettingDefault::Bool(true), description: "Insert spaces when pressing Tab." },
    SettingSpec { key: "editor.wordWrap", default: SettingDefault::Choice("off", &["off", "on"]), description: "Wrap lines at the width of the editor." },
    SettingSpec { key: "editor.minimap.enabled", default: SettingDefault::Bool(false), description: "Show the minimap beside the scrollbar." },
    SettingSpec { key: "editor.lineNumbers", default: SettingDefault::Choice("on", &["on", "off", "relative"]), description: "How line numbers are shown." },
    SettingSpec { key: "editor.renderWhitespace", default: SettingDefault::Choice("selection", &["none", "boundary", "selection", "all"]), description: "Which whitespace characters are drawn." },
    SettingSpec { key: "editor.quickSuggestions", default: SettingDefault::Bool(true), description: "Show completions as you type, without pressing Ctrl+Space." },
    SettingSpec { key: "editor.hover.enabled", default: SettingDefault::Bool(true), description: "Show documentation when the pointer rests on a symbol." },
    SettingSpec { key: "editor.parameterHints.enabled", default: SettingDefault::Bool(true), description: "Show a function's parameters while typing its arguments." },
    SettingSpec { key: "editor.formatOnSave", default: SettingDefault::Bool(false), description: "Run the formatter (ruff) before every save." },
    SettingSpec { key: "files.autoSave", default: SettingDefault::Choice("off", &["off", "afterDelay", "onFocusChange"]), description: "Save a changed file on its own: after a delay, or when the editor loses focus." },
    SettingSpec { key: "files.autoSaveDelay", default: SettingDefault::Number(1000.0), description: "The delay before an automatic save, in milliseconds, when files.autoSave is afterDelay." },
    SettingSpec { key: "explorer.autoReveal", default: SettingDefault::Bool(true), description: "Select and unfold the active editor's file in the Files view." },
    SettingSpec { key: "window.zoomLevel", default: SettingDefault::Number(0.0), description: "Zoom the whole workbench; each step is ten percent. Ctrl+= and Ctrl+- change it, Ctrl+0 resets it." },
    SettingSpec { key: "workbench.colorTheme", default: SettingDefault::OpenChoice("system", &["system", "light", "dark"]), description: "The window's palette. System follows the operating system's light or dark preference." },
    SettingSpec { key: "python.defaultInterpreterPath", default: SettingDefault::Text(""), description: "The Python that runs scripts and hosts the language server. Empty means the project's own .venv, then whatever python is on the PATH. Best set per project." },
    SettingSpec { key: "arvo.languageServer.enabled", default: SettingDefault::Bool(true), description: "Start basedpyright for completion, hover and diagnostics when a Python file opens." },
];

/// Settings as the window reads them (ADR-0020): everything in force, and
/// what the project file alone says, so the tab can show which scope set a
/// value. The paths are for showing the person where the files are.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SettingsView {
    pub effective: serde_json::Map<String, serde_json::Value>,
    pub project: serde_json::Map<String, serde_json::Value>,
    pub user_path: String,
    pub project_path: Option<String>,
}

/// A file as the editor opens it: the text, and when the file last changed
/// on disk, which a save hands back so a file changed underneath the editor
/// is not silently overwritten.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileContentView {
    pub text: String,
    /// Milliseconds since the epoch; 0 when the file system cannot say.
    pub modified: u64,
}

/// What git says about one file, for the Explorer's decorations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitStatusView {
    /// Relative to the project folder.
    pub path: String,
    /// One letter: M modified, A added, D deleted, ? untracked, U conflict.
    pub status: String,
}

/// Where the project's repository stands (#152). `is_repo: false` for a
/// folder that is not one, which a project need not be.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitBranchView {
    pub is_repo: bool,
    pub branch: String,
    /// `origin/main`, or `None` for a branch that tracks nothing yet.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Files that differ from the last commit, untracked ones included.
    pub changed: usize,
}

/// One line that matched in Find in Files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchHitView {
    /// Relative to the project folder.
    pub path: String,
    pub line: u32,
    pub column: u32,
    /// The matching line, trimmed and cut short.
    pub text: String,
}

/// One line of `keybindings.json` (ADR-0019), in VS Code's shape so a person
/// who has one can copy it. A `command` beginning with `-` removes the
/// default binding instead of adding one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeybindingView {
    pub key: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceView {
    pub name: String,
    /// dockview's own serialisation, as text. Opaque here exactly as it is in
    /// the session: treating it as data is what stops a dockview upgrade from
    /// becoming a Rust change.
    pub layout: String,
}

/// One price, as it arrived.
///
/// Separate from [`QuoteView`] and deliberately thinner: a tick carries only
/// what moved. Which rows exist, and which of them you hold, is settled once
/// by the `watchlist` command — a stream that also decided the row set would
/// make a socket blip look like a portfolio change.
///
/// `regular` is not decoration. Outside 09:30–16:00 the stream keeps sending,
/// on thin volume and wide spreads, and a pre-market print rendered
/// identically to a regular-session one is a worse answer than no price at
/// all. The panel marks it; it does not hide it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuoteTick {
    /// The bare ticker, matching [`QuoteView::symbol`].
    pub symbol: String,
    pub price: f64,
    /// Move since the previous close, as a fraction.
    pub change: Option<f64>,
    /// Whether this print happened in the regular session.
    pub regular: bool,
}

/// The channel name the live prices arrive on.
///
/// Its own channel rather than an [`EventView`]: every event is a candidate
/// for an OS notification and lands in a capped alerts log, and a price tick
/// is neither. Ticks arrive several times a second and are worth nothing once
/// the next one lands.
pub const QUOTE_CHANNEL: &str = "arvo://quote";

/// A script run's output, one line at a time (#123).
///
/// Its own channel for the reason quotes have one: a script can print
/// thousands of lines, and the event channel is a capped alerts log where
/// anything can become an OS notification.
pub const SCRIPT_CHANNEL: &str = "arvo://script";

/// One installed extension (ADR-0024): what it is, what it contributes, and
/// where it came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub themes: Vec<ThemeView>,
    /// The strategies it contributes as documents (#161, #162). `default`
    /// because an extension installed before this shipped contributes none.
    #[serde(default)]
    pub strategies: Vec<StrategyContributionView>,
    /// The processes it contributes, each with its recipe (ADR-0025).
    #[serde(default)]
    pub providers: Vec<ProviderView>,
    /// The last build of its providers, if there has been one.
    #[serde(default)]
    pub built: Option<BuildView>,
    /// Whether its contributions are in force. A disabled extension stays on
    /// disk and in this list, and contributes nothing.
    pub enabled: bool,
    /// The repository it was fetched from; `None` for a folder dropped in.
    pub source: Option<String>,
    /// The commit it is pinned to, when it was fetched.
    pub commit: Option<String>,
    pub path: String,
    /// Why this folder is not a usable extension, or what about it this
    /// build cannot run. Shown rather than hidden.
    pub problem: Option<String>,
}

/// One provider an extension contributes: a process, and the recipe that
/// produces it (ADR-0025). Nothing here has run until a person confirms the
/// build.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    /// The prebuilt binary offered for this platform, if the manifest offers
    /// one (#178). With one, installing downloads and checksums it instead
    /// of running the recipe — which is what a machine with no toolchain
    /// needs. `default`: a manifest read before this offered none.
    #[serde(default)]
    pub asset: Option<String>,
    /// The gRPC services it serves, as the manifest names them.
    pub services: Vec<String>,
    /// How it is built, verbatim: what the confirmation shows.
    pub build: String,
    /// What is started, verbatim: a file the build produces, or a command.
    pub run: String,
    /// Where the built artifact was copied, when `run` named a file and the
    /// build produced it. `None` before a build, after a failed one, or when
    /// `run` is a command rather than a file.
    pub artifact: Option<String>,
}

/// How an extension's last build went.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildView {
    /// The commit it was built at, or `local` for a folder dropped in.
    pub commit: String,
    pub at: String,
    /// `built`, or why not, in words.
    pub outcome: String,
    pub ok: bool,
}

/// One theme an extension contributes: custom properties over a base palette.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThemeView {
    /// Namespaced by its extension: `midnight.deep`.
    pub id: String,
    pub label: String,
    /// `dark` or `light`: the built-in palette it starts from.
    pub base: String,
    /// CSS custom properties, by name.
    pub colors: std::collections::BTreeMap<String, String>,
}

/// One scheduled script: a project file the runtime runs on a timer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptJobView {
    /// Relative to the project folder, and what identifies the schedule:
    /// one schedule per script.
    pub script: String,
    /// The cadence when `cron` is `None`. Kept either way, so switching to a
    /// cron expression and back does not lose what the interval was.
    pub every_secs: u64,
    /// A cron expression, as the person wrote it. When this is set it decides
    /// when the script runs and `every_secs` is not consulted.
    #[serde(default)]
    pub cron: Option<String>,
    pub enabled: bool,
}

/// One of Arvo's rules, as the Rulesets view describes it: what a ruleset
/// can be made from, with the numbers it takes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleView {
    pub name: String,
    pub label: String,
    /// What the rule claims about how prices behave.
    pub premise: String,
    pub interval: String,
    /// Parameters every trial shares, with the shipped values.
    pub fixed: Vec<(String, f64)>,
    /// What a shipped study searches, with the values it tries.
    pub axes: Vec<(String, Vec<f64>)>,
    /// Needs a set of instruments rather than one.
    pub ranks_a_set: bool,
    /// Trades the instrument's option chain.
    pub trades_options: bool,
}

/// A ruleset's parts, for the form that makes and changes one without the
/// file's JSON in between. One entry per parameter: a single value fixes
/// it, several values search them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulesetFormView {
    pub name: String,
    pub rule: String,
    pub label: String,
    pub premise: String,
    /// Parameter name to the values it takes: one fixes, several search.
    pub params: Vec<(String, Vec<f64>)>,
}

/// One ruleset file in the project, as the Rulesets view shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulesetView {
    /// Relative to the project: `rulesets/<name>.json`.
    pub path: String,
    /// The name a finding records. Empty when the file could not be read.
    pub name: String,
    pub label: String,
    /// The engine rule it runs.
    pub rule: String,
    pub interval: String,
    /// How many parameter sets a study of it searches.
    pub searches: usize,
    /// Why it is not in the picker, when it is not.
    pub problem: Option<String>,
}

/// The risk model in force for the next study, as the Risk view shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskModelView {
    /// The file, relative to the project: `.arvo/risk.json`.
    pub path: String,
    /// Whether the file exists. When it does not, `model` is what Arvo ships.
    pub exists: bool,
    /// The model as its JSON object, one key per setting, `null` for a limit
    /// not set. Kept as JSON so a setting added later shows up unasked.
    pub model: serde_json::Value,
    /// Why the file cannot be used, when it cannot. Studies refuse to run
    /// until this is empty; `model` is then the last one that loaded.
    pub error: Option<String>,
}

/// One live session the engine is hosting, as the Sessions view shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TradingSessionView {
    pub id: String,
    pub finding: String,
    /// `alpaca-paper`, `alpaca-live` or `robinhood-<last four>`.
    pub executor: String,
    /// Simulated money. Drawn apart from real money, as the dashboard does.
    pub paper: bool,
    pub instrument: String,
    pub strategy: String,
    pub started_at: String,
    /// `starting`, `running`, `halted`, `stopped` or `failed`.
    pub state: String,
    pub signals: u32,
    pub submitted: u32,
    pub refused: u32,
    pub fills: u32,
    pub halted: Option<String>,
    pub last_error: Option<String>,
    pub last_bar: Option<String>,
}

/// One line of a session's record: what happened, when, and the detail the
/// engine wrote with it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TradingEventView {
    pub at: String,
    /// `started`, `bar`, `signal`, `submitted`, `refused`, `exit`, `filled`,
    /// `halted`, `stopped`, and the failures.
    pub event: String,
    /// The detail as one line, already rendered.
    pub detail: String,
}

/// One background job, as the Jobs view shows it.
/// A row as it comes back from the view.
///
/// Deserialized rather than re-derived from the stored finding: the table
/// exports what is on screen, including whatever sort the reader applied. An
/// export that silently differed from the table above it would be worse than
/// none.
#[derive(Deserialize, Serialize)]
pub struct TradeRowExport {
    /// Which instrument the round trip was in.
    ///
    /// `default` because an ordinary study's rows do not carry one: the
    /// subject line already says it, and a column repeating the same name on
    /// every line is noise. On a book it is the difference between a usable
    /// export and a list of trades from three instruments with no way to tell
    /// them apart.
    #[serde(default)]
    pub instrument: String,
    pub opened: String,
    pub closed: String,
    pub direction: String,
    pub quantity: f64,
    pub entry: f64,
    pub exit: Option<f64>,
    pub pnl: f64,
    pub commission: f64,
    pub held_days: Option<f64>,
    pub exit_reason: String,
}

/// Where a terminal's bytes arrive in the window (ADR-0021).
pub const TERMINAL_CHANNEL: &str = "arvo://terminal";

/// One read from a shell's pseudo-terminal: bytes, not text, because a
/// read can end inside a character. `exited` once, at the end.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalOutputView {
    pub id: u32,
    pub data: Vec<u8>,
    pub exited: bool,
}

/// Where the language server's stderr lines arrive in the window, one text
/// per event, for the Output panel's Language Server channel (ADR-0021).
pub const LSP_LOG_CHANNEL: &str = "arvo://lsp-log";

/// One line from a running script, or the fact that it ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptOutputView {
    /// Which run, as `run_project_script` returned it.
    pub run: u32,
    /// `"out"`, `"err"`, `"info"` (what Arvo started) or `"exit"`.
    pub stream: String,
    /// The line without its newline; for `"exit"`, how it ended.
    pub text: String,
}

/// The channel name the push events arrive on.
///
/// Here rather than in either crate that uses it for the same reason every
/// shape above is: a name only one side changes is a channel that goes quiet
/// with nothing failing to compile.
pub const EVENT_CHANNEL: &str = "arvo://event";

/// Something the backend reports without being asked.
///
/// Everything else in this file answers a question the window put to a
/// command. This is the other direction — what happened while nobody was
/// looking: a plugin dropped, a broker session ended.
///
/// # Why the text is in the payload
///
/// `title` and `detail` are filled in by the backend rather than derived from
/// `kind` by whoever renders it. There are two renderers — the OS
/// notification and the in-app alerts list — and text derived twice is text
/// that drifts. `kind` is left for what a renderer needs *structurally*: the
/// status bar needs to know a feed is down, not how to phrase it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventView {
    pub kind: EventKindView,
    pub title: String,
    pub detail: String,
    pub severity: SeverityView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "of")]
pub enum EventKindView {
    Plugin { id: String, reachable: bool },
    /// A broker connection came up or went away. `connected: false` covers
    /// both signing out and a session that expired underneath you — which of
    /// the two it was is in `detail`, because the difference matters to a
    /// person reading it and not at all to the status bar.
    Feed { id: String, connected: bool },
    /// The live price stream stopped or came back.
    ///
    /// Its own variant rather than another `Feed`: the status bar reads
    /// `Feed` to decide whether a broker session is held, and a price socket
    /// dropping says nothing about that. Folding the two together would have
    /// a Yahoo reconnect claim you had been signed out of your broker.
    ///
    /// Worth an event at all because the failure is otherwise invisible: a
    /// dead socket looks exactly like a market where nothing is trading.
    Stream { live: bool },
    /// Stored findings went stale since they were last checked — their data
    /// changed or disappeared. `count` is how many were newly so.
    Findings { count: usize },
    /// A trading session changed state: starting, running, stopped, halted
    /// or failed.
    Session { id: String, state: String },
}

/// Whether this is worth interrupting someone for.
///
/// The one thing that decides it: `Warning` raises an OS notification,
/// `Info` only lands in the alerts list. Both are always recorded, so the
/// distinction costs nothing but noise.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SeverityView {
    Info,
    Warning,
}

#[cfg(test)]
mod tests {
    use super::{SettingDefault, SettingSpec};

    #[test]
    fn an_open_choice_takes_a_value_this_build_has_never_heard_of() {
        // The colour theme is the case: an extension contributes themes
        // (ADR-0024), so the listed values are what ships, not all there is.
        // A closed choice would have the picker offer a theme the runtime
        // then refuses to store, which is exactly what it did.
        let theme = SettingSpec::by_key("workbench.colorTheme").expect("the theme is a setting");
        assert!(matches!(theme.default, SettingDefault::OpenChoice(..)));
        assert!(theme.accepts(&serde_json::json!("catppuccin.mocha")));
        assert!(theme.accepts(&serde_json::json!("dark")));
        assert!(!theme.accepts(&serde_json::json!(3)), "still a string");

        let wrap = SettingSpec::by_key("editor.wordWrap").expect("a closed choice");
        assert!(wrap.accepts(&serde_json::json!("on")));
        assert!(!wrap.accepts(&serde_json::json!("sideways")), "a closed choice stays closed");
    }
}
