use leptos::mount::mount_to;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    // `catch` is load-bearing. A Tauri command returning `Err` rejects the
    // promise, and without it wasm-bindgen rethrows into the wasm boundary and
    // abandons the calling future — so a failed command left the UI spinning
    // forever with the reason thrown away. With it the rejection is a value we
    // can read and show.
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    // app-shell ticket 04 — glue defined in index.html. on_panel_created
    // is a JS-callable closure invoked with (panel_name, element) at the
    // exact moment dockview creates each panel's element — see ticket 01's
    // Answer for why the element reference itself is what's passed, not
    // an id to re-query later. on_panel_removed fires from dockview's own
    // panel dispose() hook, so Rust can unmount the matching Leptos root
    // instead of leaking it when a panel is actually removed (not just
    // hidden).
    #[wasm_bindgen(js_namespace = window, js_name = initShell)]
    fn init_shell(host_id: &str, on_panel_created: &JsValue, on_panel_removed: &JsValue);

    // Real width-collapse — add/remove the dockview panel, not just clear
    // its content. See ActivityBar's toggle_view.
    #[wasm_bindgen(js_namespace = window, js_name = setSidebarVisible)]
    fn set_sidebar_visible(visible: bool);

    // Opens (or focuses) a tab for one study, beside Welcome in the main
    // group. Creating the panel synchronously drives `on_panel_created`, so
    // the study must already be in the map before this is called.
    #[wasm_bindgen(js_namespace = window, js_name = openStudyPanel)]
    fn open_study_panel(id: &str, title: &str);

    // Draws both equity curves into an element. Defined in index.html against
    // the vendored charting library, so the chart's palette can be read from
    // the same CSS variables everything else uses.
    #[wasm_bindgen(js_namespace = window, js_name = renderEquityChart)]
    fn render_equity_chart(el: &web_sys::HtmlElement, strategy: JsValue, benchmark: JsValue);

    // app-shell ticket 12 — Output moved into the View menu; same
    // add/remove-panel toggle as the sidebar's.
    #[wasm_bindgen(js_namespace = window, js_name = setOutputVisible)]
    fn set_output_visible_js(visible: bool);

    // Borderless window (tauri.conf.json's `decorations: false`) — these
    // call the global Tauri window API directly, no new Tauri command
    // needed since `withGlobalTauri` already exposes it.
    #[wasm_bindgen(js_namespace = window, js_name = minimizeWindow)]
    fn minimize_window();
    #[wasm_bindgen(js_namespace = window, js_name = toggleMaximizeWindow)]
    fn toggle_maximize_window();
    #[wasm_bindgen(js_namespace = window, js_name = closeWindow)]
    fn close_window();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityView {
    Portfolio,
    Research,
    Extensions,
    Alerts,
    Settings,
}

/// Which top-menu-bar dropdown (if any) is open. Separate from
/// `ActivityView` — these are transient menus, not sidebar sections.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuId {
    File,
    View,
    Help,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "state")]
enum PluginStatusView {
    Reachable {
        name: String,
        version: String,
        capabilities: Vec<String>,
    },
    Unreachable {
        reason: String,
    },
}

#[derive(Clone, Deserialize)]
struct PluginView {
    id: String,
    address: Option<String>,
    status: PluginStatusView,
}

/// Invokes a command and decodes its reply, logging rather than swallowing a
/// decode failure.
///
/// `None` means the call or the decode failed — distinct from a successful
/// call returning something empty. Those two used to be indistinguishable
/// once rendered, with nothing logged anywhere.
async fn call_typed<T: serde::de::DeserializeOwned>(cmd: &str, args: JsValue) -> Result<T, String> {
    let result = invoke(cmd, args).await.map_err(|err| {
        // The command itself failed. Tauri serialises `CommandError` as a
        // plain string, so this is the reason the backend gave.
        err.as_string()
            .unwrap_or_else(|| format!("`{cmd}` failed with a non-text error"))
    })?;

    serde_wasm_bindgen::from_value(result)
        .map_err(|err| format!("could not read the `{cmd}` reply: {err}"))
}

/// Logs and discards the reason. Only for calls with no error surface of their
/// own — anything a user initiated should show them what went wrong instead.
async fn call(cmd: &str) -> Vec<PluginView> {
    match call_typed(cmd, JsValue::UNDEFINED).await {
        Ok(value) => value,
        Err(reason) => {
            web_sys::console::error_1(&reason.clone().into());
            Vec::new()
        }
    }
}

#[derive(Clone, Deserialize)]
struct MetricsView {
    total_return: f64,
    cagr: f64,
    max_drawdown: f64,
    volatility: f64,
    sharpe: Option<f64>,
    sortino: Option<f64>,
    calmar: Option<f64>,
    trades: u32,
}

#[derive(Clone, Deserialize)]
struct MonthlyReturnView {
    year: i32,
    month: u32,
    value: f64,
}

#[derive(Clone, Deserialize)]
struct InstrumentView {
    id: String,
    from: Option<String>,
    to: Option<String>,
    bars: usize,
    fingerprint: Option<String>,
}

#[derive(Clone, Deserialize)]
struct DataLibraryView {
    directory: String,
    instruments: Vec<InstrumentView>,
}

/// Mirrors `arvo_runtime::research::StudyView`. Nothing here names an engine,
/// a broker or an order — the workbench works in research concepts only.
#[derive(Clone, Deserialize)]
struct StudyView {
    instrument: String,
    verdict: String,
    reasons: Vec<String>,
    trials: usize,
    best_sharpe: f64,
    expected_best_under_null: Option<f64>,
    survived_deflation: bool,
    in_sample: String,
    out_of_sample: String,
    selected_params: Vec<(String, f64)>,
    strategy: MetricsView,
    benchmark: MetricsView,
    excess_return: f64,
    strategy_curve: Vec<CurvePoint>,
    benchmark_curve: Vec<CurvePoint>,
    monthly: Vec<MonthlyReturnView>,
    trades_detail: TradesView,
    dataset_version: String,
    strategy_name: String,
    starting_cash: f64,
    commission_bps: f64,
    slippage_bps: f64,
    engine: String,
}

/// The round trips behind a return, and what they cost.
#[derive(Clone, Deserialize)]
struct TradesView {
    closed: u32,
    still_open: u32,
    win_rate: Option<f64>,
    profit_factor: Option<f64>,
    expectancy: Option<f64>,
    average_win: Option<f64>,
    average_loss: Option<f64>,
    average_holding_days: Option<f64>,
    fees_paid: f64,
    fees_fraction: f64,
    signal_exits: u32,
    stop_exits: u32,
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct CurvePoint {
    time: String,
    value: f64,
}

#[derive(Clone, Deserialize)]
struct OutcomeView {
    instrument: String,
    strategy_return: f64,
    benchmark_return: f64,
    excess_return: f64,
    max_drawdown: f64,
    trades: u32,
}

/// Mirrors `arvo_runtime::research::PanelView`.
#[derive(Clone, Deserialize)]
struct PanelView {
    verdict: String,
    reasons: Vec<String>,
    instruments: usize,
    total_trades: u32,
    mean_excess_return: f64,
    beat_benchmark: usize,
    mean_max_drawdown: f64,
    worst_max_drawdown: f64,
    trials: usize,
    best_sharpe: f64,
    expected_best_under_null: Option<f64>,
    survived_deflation: bool,
    in_sample: String,
    out_of_sample: String,
    selected_params: Vec<(String, f64)>,
    per_instrument: Vec<OutcomeView>,
    failures: Vec<String>,
    dataset_version: String,
    strategy_name: String,
    starting_cash: f64,
    commission_bps: f64,
    slippage_bps: f64,
    engine: String,
}

#[derive(Clone, Deserialize)]
struct HoldingView {
    instrument: String,
    quantity: Option<f64>,
    price: Option<f64>,
    market_value: f64,
    cost_basis: Option<f64>,
    unrealized: Option<f64>,
    unrealized_pct: Option<f64>,
    weight: f64,
    priced_by: String,
}

#[derive(Clone, Deserialize)]
struct ImportView {
    columns: Vec<(String, String)>,
    ignored: Vec<String>,
    rows_imported: usize,
    rows_skipped: Vec<String>,
    cost_basis_derived: bool,
}

#[derive(Clone, Deserialize)]
struct ValuePoint {
    time: String,
    value: f64,
}

#[derive(Clone, Deserialize)]
struct ChangeView {
    from: String,
    to: String,
    absolute: f64,
    percent: Option<f64>,
}

#[derive(Clone, Deserialize)]
struct PortfolioView {
    name: String,
    as_of: String,
    total_value: f64,
    total_cost: Option<f64>,
    unrealized: Option<f64>,
    unrealized_pct: Option<f64>,
    without_cost_basis: usize,
    cash: f64,
    holdings: Vec<HoldingView>,
    unpriced: Vec<String>,
    value_history: Vec<ValuePoint>,
    change: Option<ChangeView>,
    import: ImportView,
}

#[derive(Clone, Deserialize)]
struct PortfolioLibraryView {
    directory: String,
    portfolios: Vec<PortfolioView>,
}

#[derive(Clone, Deserialize)]
struct HistoryEntryView {
    id: String,
    kind: String,
    subject: String,
    verdict: String,
    recorded_at: String,
    stale: Option<bool>,
}

/// Mirrors `arvo_runtime::research::RecordView`.
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecordView {
    // Both boxed: each carries curves and tables, so an unboxed enum would
    // size every record to whichever view is currently the larger.
    Study(Box<StudyView>),
    Panel(Box<PanelView>),
}

/// Grouped to thousands. A portfolio total is read as a quantity of money,
/// and `128450.75` is materially harder to read at a glance than
/// `128,450.75` — which matters more here than anywhere else in the app.
fn money(value: f64) -> String {
    let negative = value < 0.0;
    let whole = value.abs().trunc();
    let cents = ((value.abs() - whole) * 100.0).round() as u64;
    let digits: Vec<char> = format!("{whole:.0}").chars().collect();

    let mut grouped = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*digit);
    }

    format!("{}${grouped}.{cents:02}", if negative { "-" } else { "" })
}

fn percent(value: f64) -> String {
    format!("{:+.2}%", value * 100.0)
}

/// First twelve characters of a content hash. Enough to compare two by eye
/// and to spot that they differ; the full value lives in the record.
fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

fn ratio(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"))
}

/// app-shell ticket 12 follow-up: a third baked-in palette (Catppuccin
/// Mocha) alongside light/dark, picked from Settings rather than the old
/// binary sun/moon toggle — a third option doesn't fit a two-state icon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Theme {
    Light,
    Dark,
    CatppuccinMocha,
}

impl Theme {
    fn attr(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::CatppuccinMocha => "catppuccin-mocha",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::CatppuccinMocha => "Catppuccin Mocha",
        }
    }
}

fn prefers_dark() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
        .map(|m| m.matches())
        .unwrap_or(false)
}

fn apply_theme(theme: Theme) {
    if let Some(html) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = html.set_attribute("data-theme", theme.attr());
    }
}

/// Fixed chrome — not a dockview panel. Matches VS Code's own shape: the
/// activity bar itself isn't draggable/floatable, only the regions beside
/// it are.
/// Clicking the already-active icon collapses the sidebar, same as VS
/// Code — this is a toggle, not a plain select.
fn toggle_view(
    view: ActivityView,
    active_view: ReadSignal<Option<ActivityView>>,
    set_active_view: WriteSignal<Option<ActivityView>>,
) {
    set_active_view.set(if active_view.get_untracked() == Some(view) {
        None
    } else {
        Some(view)
    });
}

/// Extensions (puzzle piece) and Alerts (bell) — the same silhouettes VS
/// Code's own activity bar uses for these, hand-drawn as plain `currentColor`
/// paths rather than vendoring an icon font/library for two glyphs.
/// A stacked-bars mark: the portfolio view is about composition, not a price
/// line, and the icon should say allocation rather than chart.
#[component]
fn PortfolioIcon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M3 20h18v2H3v-2Zm2-8h3v7H5v-7Zm5-6h3v13h-3V6Zm5 3h3v10h-3V9Z" />
        </svg>
    }
}

/// A conical flask — the research view runs experiments, and the icon should
/// say experiment rather than chart.
#[component]
fn ResearchIcon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M9 2h6v2h-1v5.2l5.6 9.3A2 2 0 0 1 17.9 22H6.1a2 2 0 0 1-1.7-3.5L10 9.2V4H9V2Zm3 9.6-2.6 4.4h5.2L12 11.6Z" />
        </svg>
    }
}

#[component]
fn ExtensionsIcon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M14 2a2 2 0 0 1 2 2v2h2a2 2 0 0 1 2 2v2.2a1.8 1.8 0 1 0 0 3.6V16a2 2 0 0 1-2 2h-2.2a1.8 1.8 0 1 1-3.6 0H8a2 2 0 0 1-2-2v-2h-2a1.8 1.8 0 1 1 0-3.6V8a2 2 0 0 1 2-2h2V4a2 2 0 0 1 2-2Z" />
        </svg>
    }
}

#[component]
fn AlertsIcon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M12 2a1 1 0 0 1 1 1v1.06A7 7 0 0 1 19 11v4l1.6 2.4a1 1 0 0 1-.83 1.6H4.23a1 1 0 0 1-.83-1.6L5 15v-4a7 7 0 0 1 6-6.94V3a1 1 0 0 1 1-1Zm0 20a2.5 2.5 0 0 0 2.45-2h-4.9A2.5 2.5 0 0 0 12 22Z" />
        </svg>
    }
}

#[component]
fn SettingsIcon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8Zm9.4 4c0 .66-.06 1.3-.17 1.94l2.13 1.66-2 3.46-2.49-1a8.6 8.6 0 0 1-1.68.98L16.8 22H9.2l-.4-2.96a8.6 8.6 0 0 1-1.68-.98l-2.49 1-2-3.46 2.13-1.66A8.9 8.9 0 0 1 4.6 12c0-.66.06-1.3.17-1.94L2.64 8.4l2-3.46 2.49 1c.5-.4 1.07-.73 1.68-.98L9.2 2h7.6l.4 2.96c.6.25 1.17.58 1.68.98l2.49-1 2 3.46-2.13 1.66c.11.64.17 1.28.17 1.94Z" />
        </svg>
    }
}

#[component]
fn ActivityBar(
    active_view: ReadSignal<Option<ActivityView>>,
    set_active_view: WriteSignal<Option<ActivityView>>,
) -> impl IntoView {
    view! {
        <nav class="activity-bar" class:integrated=move || active_view.get().is_some()>
            <button
                class="activity-bar-item"
                class:active=move || active_view.get() == Some(ActivityView::Portfolio)
                title="Portfolio"
                on:click=move |_| toggle_view(ActivityView::Portfolio, active_view, set_active_view)
            >
                <PortfolioIcon />
            </button>
            <button
                class="activity-bar-item"
                class:active=move || active_view.get() == Some(ActivityView::Research)
                title="Research"
                on:click=move |_| toggle_view(ActivityView::Research, active_view, set_active_view)
            >
                <ResearchIcon />
            </button>
            <button
                class="activity-bar-item"
                class:active=move || active_view.get() == Some(ActivityView::Extensions)
                title="Extensions"
                on:click=move |_| toggle_view(ActivityView::Extensions, active_view, set_active_view)
            >
                <ExtensionsIcon />
            </button>
            <button
                class="activity-bar-item"
                class:active=move || active_view.get() == Some(ActivityView::Alerts)
                title="Alerts"
                on:click=move |_| toggle_view(ActivityView::Alerts, active_view, set_active_view)
            >
                <AlertsIcon />
            </button>
            <div class="activity-bar-spacer" />
            <button
                class="activity-bar-item"
                class:active=move || active_view.get() == Some(ActivityView::Settings)
                title="Settings"
                on:click=move |_| toggle_view(ActivityView::Settings, active_view, set_active_view)
            >
                <SettingsIcon />
            </button>
        </nav>
    }
}

/// Extensions: one unified list of everything installable — plugins and
/// theme packs together, matching VS Code's own Extensions view rather
/// than a separate icon per install-type. Theme packs have no loader yet
/// (app-shell ticket 03, sequenced after this one) — shown empty, not
/// faked.
#[component]
fn ExtensionsView(
    plugins: ReadSignal<Vec<PluginView>>,
    set_plugins: WriteSignal<Vec<PluginView>>,
) -> impl IntoView {
    let refresh = move |_| {
        spawn_local(async move {
            set_plugins.set(call("refresh_plugins").await);
        });
    };
    let (filter, set_filter) = signal(String::new());

    view! {
        <div class="sidebar-view">
            <h3>"Extensions"</h3>

            <input
                type="search"
                class="extension-filter"
                placeholder="Filter extensions..."
                prop:value=filter
                on:input=move |ev| set_filter.set(event_target_value(&ev))
            />

            <h4>"Plugins"</h4>
            <button on:click=refresh>"Refresh"</button>
            <ul class="extension-list">
                {move || {
                    let needle = filter.get().to_lowercase();
                    plugins
                        .get()
                        .into_iter()
                        .filter(|plugin| needle.is_empty() || plugin.id.to_lowercase().contains(&needle))
                        .map(|plugin| {
                            let status = match plugin.status {
                                PluginStatusView::Reachable { name, version, capabilities } => {
                                    format!(
                                        "Reachable — {name} v{version} [{}]",
                                        capabilities.join(", "),
                                    )
                                }
                                PluginStatusView::Unreachable { reason } => {
                                    format!("Unreachable — {reason}")
                                }
                            };
                            // ponytail: path-based (WASM) entries have no address yet —
                            // see plugin-execution-tiers ticket 04.
                            let location = plugin.address.as_deref().unwrap_or("wasm");
                            view! {
                                <li>
                                    <span class="extension-kind">"Plugin"</span>
                                    {format!(" {} ({})", plugin.id, location)} " — " {status}
                                </li>
                            }
                        })
                        .collect_view()
                }}
            </ul>

            <h4>"Theme Packs"</h4>
            <p class="sidebar-empty">"No theme packs installed"</p>
        </div>
    }
}

/// The research view: pick an instrument, run a parameter study, read what
/// survived.
///
/// The display is deliberately loaded with caveats — the split, the number of
/// configurations tried, the bar a no-skill search would clear, the costs
/// assumed. A verdict shown alone is a number that looks like a fact, and the
/// whole reason this platform exists is that backtests are easy to believe.
#[component]
fn ResearchView(
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
    set_studies: WriteSignal<std::collections::HashMap<String, StudyView>>,
    panel: ReadSignal<Option<PanelView>>,
    set_panel: WriteSignal<Option<PanelView>>,
) -> impl IntoView {
    let (library, set_library) = signal(None::<DataLibraryView>);
    // What is running, not merely that something is. A panel is a few dozen
    // backtests and takes tens of seconds in a debug build; a bare spinner for
    // that long is indistinguishable from a hang, which this codebase has
    // already been bitten by once.
    let (running, set_running) = signal(None::<String>);
    let (error, set_error) = signal(None::<String>);
    let (history, set_history) = signal(Vec::<HistoryEntryView>::new());

    // Refetched after every run, so a finding appears in the history the
    // moment it is recorded rather than only after a restart.
    let refresh_history = move || {
        spawn_local(async move {
            if let Ok(entries) =
                call_typed::<Vec<HistoryEntryView>>("list_history", JsValue::UNDEFINED).await
            {
                set_history.set(entries);
            }
        });
    };

    spawn_local(async move {
        match call_typed::<DataLibraryView>("list_instruments", JsValue::UNDEFINED).await {
            Ok(value) => set_library.set(Some(value)),
            Err(reason) => set_error.set(Some(reason)),
        }
    });
    refresh_history();

    let run = move |instrument: String| {
        set_running.set(Some(format!("Running study on {instrument}: 11 backtests")));
        set_error.set(None);
        // No clearing of previous results: open tabs stay open, which is the
        // point of having them.
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "instrument": instrument,
            }))
            .unwrap_or(JsValue::UNDEFINED);
            match call_typed::<StudyView>("run_study", args).await {
                Ok(result) => {
                    let instrument = result.instrument.clone();
                    // Into the map first: opening the panel mounts its
                    // content synchronously, and that mount reads this key.
                    set_studies.update(|studies| {
                        studies.insert(instrument.clone(), result);
                    });
                    open_study_panel(&format!("{STUDY_PANEL_PREFIX}{instrument}"), &instrument);
                }
                // Show the backend's own words. The previous version could not
                // even reach this branch: the rejected promise killed the
                // future above, leaving the spinner up and nothing logged.
                Err(reason) => set_error.set(Some(reason)),
            }
            set_running.set(None);
            refresh_history();
        });
    };

    let run_panel = move |_| {
        let count = library.with_untracked(|library| {
            library.as_ref().map_or(0, |library| {
                library.instruments.iter().filter(|i| i.bars > 0).count()
            })
        });
        // 9 configurations in-sample on every instrument, then the winner and
        // its benchmark out-of-sample on each: 11 runs per instrument.
        set_running.set(Some(format!(
            "Running panel: {count} instruments, {} backtests",
            count * 11
        )));
        set_error.set(None);
        spawn_local(async move {
            match call_typed::<PanelView>("run_panel", JsValue::UNDEFINED).await {
                Ok(result) => {
                    set_panel.set(Some(result));
                    open_study_panel(PANEL_PANEL_ID, "Panel");
                }
                Err(reason) => set_error.set(Some(reason)),
            }
            set_running.set(None);
            refresh_history();
        });
    };

    view! {
        <div class="sidebar-view">
            <h3>"Research"</h3>

            // The panel is the run that can actually conclude something: one
            // instrument yields a dozen round trips against a thirty-trade
            // bar, and no amount of history fixes that.
            <button
                class="research-panel-run"
                title="Choose one configuration across every instrument, then judge it on data it                        has not seen"
                disabled=move || running.get().is_some()
                on:click=run_panel
            >
                {move || {
                    if panel.get().is_some() {
                        "Re-run panel across all instruments"
                    } else {
                        "Run panel across all instruments"
                    }
                }}
            </button>

            {move || match library.get() {
                None => view! { <p class="sidebar-empty">"Looking for data…"</p> }.into_any(),
                Some(library) if library.instruments.is_empty() => {
                    view! {
                        <div>
                            <p class="sidebar-empty">"No instruments yet"</p>
                            <p class="research-hint">
                                "Drop daily-bar CSV files here, one per instrument, named "
                                <code>"SYMBOL.VENUE.csv"</code>
                                " with the header "
                                <code>"date,open,high,low,close,volume"</code>
                            </p>
                            <p class="research-path">{library.directory.clone()}</p>
                        </div>
                    }
                    .into_any()
                }
                Some(library) => {
                    view! {
                        <ul class="research-instruments">
                            {library
                                .instruments
                                .into_iter()
                                .map(|instrument| {
                                    let id = instrument.id.clone();
                                    let coverage = match (&instrument.from, &instrument.to) {
                                        (Some(from), Some(to)) => {
                                            format!("{} bars · {from} → {to}", instrument.bars)
                                        }
                                        // A file that parsed to nothing is shown rather
                                        // than hidden: silence would look like it was
                                        // never added.
                                        _ => "no usable bars".to_owned(),
                                    };
                                    let runnable = instrument.bars > 0;
                                    let id_again = instrument.id.clone();
                                    let live_hash = instrument.fingerprint.clone();
                                    // Two closures rather than one shared: each
                                    // owns a String, so the predicate is not
                                    // `Copy` and cannot be moved into two views.
                                    // `with`, not `get` — this re-runs per row on
                                    // every change and only needs a yes/no, not a
                                    // clone of the whole map.
                                    let held_title = {
                                        let id = instrument.id.clone();
                                        move || studies.with(|studies| studies.contains_key(&id))
                                    };
                                    let held_rerun = {
                                        let id = instrument.id.clone();
                                        move || studies.with(|studies| studies.contains_key(&id))
                                    };
                                    // A held result is stale when the data it
                                    // was produced from no longer hashes to
                                    // what is on disk. This is the whole point
                                    // of recording a dataset version: without
                                    // it a cached result is trusted forever.
                                    let stale = {
                                        let id = instrument.id.clone();
                                        let live = live_hash.clone();
                                        move || {
                                            studies.with(|studies| {
                                                match (studies.get(&id), live.as_deref()) {
                                                    (Some(study), Some(live)) => {
                                                        study.dataset_version != live
                                                    }
                                                    // No held result, or no
                                                    // readable data: nothing to
                                                    // call stale.
                                                    _ => false,
                                                }
                                            })
                                        }
                                    };
                                    view! {
                                        <li class="research-row">
                                            <button
                                                class="research-instrument"
                                                title=move || {
                                                    if held_title() {
                                                        "Open the result already held"
                                                    } else {
                                                        "Run a study"
                                                    }
                                                }
                                                disabled=move || running.get().is_some() || !runnable
                                                on:click={
                                                    let id = id.clone();
                                                    move |_| {
                                                        // Reopen what we already have rather than
                                                        // spending eleven backtests to recompute a
                                                        // result that has not changed. Closing a
                                                        // tab is a display decision, not a reason
                                                        // to throw the finding away.
                                                        if studies
                                                            .with_untracked(|s| s.contains_key(&id))
                                                        {
                                                            open_study_panel(
                                                                &format!(
                                                                    "{STUDY_PANEL_PREFIX}{id}",
                                                                ),
                                                                &id,
                                                            );
                                                        } else {
                                                            run(id.clone());
                                                        }
                                                    }
                                                }
                                            >
                                                <span class="research-instrument-id">
                                                    {instrument.id.clone()}
                                                </span>
                                                <span class="research-instrument-meta">
                                                    {coverage}
                                                </span>
                                                {move || {
                                                    stale()
                                                        .then(|| {
                                                            view! {
                                                                <span class="research-stale">
                                                                    "held result is out of date \
                                                                     \u{2014} the data has changed"
                                                                </span>
                                                            }
                                                        })
                                                }}
                                            </button>
                                            // Only once there is something to redo. Without it a
                                            // held result could never be refreshed, which would be
                                            // worse than always recomputing — the data on disk can
                                            // change underneath it, and nothing detects that yet.
                                            {move || {
                                                held_rerun()
                                                    .then(|| {
                                                        let id = id_again.clone();
                                                        view! {
                                                            <button
                                                                class="research-rerun"
                                                                title="Run again"
                                                                disabled=move || running.get().is_some()
                                                                on:click=move |_| run(id.clone())
                                                            >
                                                                "\u{21bb}"
                                                            </button>
                                                        }
                                                    })
                                            }}
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                    .into_any()
                }
            }}

            {move || {
                running.get().map(|what| view! { <p class="research-running">{what}</p> })
            }}

            {move || error.get().map(|message| view! { <p class="research-error">{message}</p> })}

            // Findings outlive the window they were produced in. Reopening one
            // costs nothing; producing it cost dozens of backtests.
            {move || {
                let entries = history.get();
                (!entries.is_empty())
                    .then(|| {
                        view! {
                            <h4 class="research-section">"History"</h4>
                            <ul class="research-history">
                                {entries
                                    .into_iter()
                                    .map(|entry| {
                                        let id = entry.id.clone();
                                        let open = move |_| {
                                            let id = id.clone();
                                            spawn_local(async move {
                                                let args = serde_wasm_bindgen::to_value(
                                                        &serde_json::json!({ "id" : id }),
                                                    )
                                                    .unwrap_or(JsValue::UNDEFINED);
                                                match call_typed::<
                                                    RecordView,
                                                >("open_record", args)
                                                    .await
                                                {
                                                    Ok(RecordView::Study(study)) => {
                                                        let instrument = study.instrument.clone();
                                                        set_studies
                                                            .update(|studies| {
                                                                studies.insert(instrument.clone(), *study);
                                                            });
                                                        open_study_panel(
                                                            &format!("{STUDY_PANEL_PREFIX}{instrument}"),
                                                            &instrument,
                                                        );
                                                    }
                                                    Ok(RecordView::Panel(panel)) => {
                                                        set_panel.set(Some(*panel));
                                                        open_study_panel(PANEL_PANEL_ID, "Panel");
                                                    }
                                                    Err(reason) => set_error.set(Some(reason)),
                                                }
                                            });
                                        };
                                        // `Some(false)` is current, `Some(true)` is stale, and
                                        // `None` means the data it referenced is gone entirely —
                                        // three different things, shown as three different things.
                                        let mark = match entry.stale {
                                            Some(true) => "data changed since",
                                            None => "data no longer present",
                                            Some(false) => "",
                                        };
                                        view! {
                                            <li>
                                                <button class="research-history-entry" on:click=open>
                                                    <span class="research-history-line">
                                                        <span class=verdict_dot(&entry.verdict) />
                                                        <span class="research-history-subject">
                                                            {entry.subject.clone()}
                                                        </span>
                                                        <span class="research-history-kind">
                                                            {entry.kind.clone()}
                                                        </span>
                                                    </span>
                                                    <span class="research-instrument-meta">
                                                        {format!(
                                                            "{} · {}",
                                                            entry.recorded_at.clone(),
                                                            entry.verdict.clone(),
                                                        )}
                                                    </span>
                                                    {(!mark.is_empty())
                                                        .then(|| {
                                                            view! {
                                                                <span class="research-stale">{mark}</span>
                                                            }
                                                        })}
                                                </button>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                    })
            }}
        </div>
    }
}

/// A coloured dot carrying the verdict, so a history list scans at a glance
/// without reading every line.
fn verdict_dot(verdict: &str) -> &'static str {
    match verdict {
        "Supported" => "research-dot supported",
        "Not supported" => "research-dot refuted",
        _ => "research-dot inconclusive",
    }
}

fn verdict_class(verdict: &str) -> &'static str {
    match verdict {
        "Supported" => "research-verdict supported",
        "Not supported" => "research-verdict refuted",
        _ => "research-verdict inconclusive",
    }
}

/// The panel's own tab. One at a time — there is only one panel.
const PANEL_PANEL_ID: &str = "panel";

/// The portfolio overview's tab.
const PORTFOLIO_PANEL_ID: &str = "portfolio";

/// Dockview panel ids for study tabs are this plus the instrument.
const STUDY_PANEL_PREFIX: &str = "study:";

/// One study tab's content.
///
/// Reads its study back out of the shared map by key rather than capturing a
/// value, so re-running an instrument refreshes the tab that is already open
/// instead of leaving a stale report behind it.
#[component]
fn StudyTab(
    instrument: String,
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || {
                studies
                    .get()
                    .get(&instrument)
                    .cloned()
                    .map(|study| view! { <StudyReport study=study /> })
            }}
        </div>
    }
}

/// The panel tab's content, read back out of the signal so a re-run refreshes
/// the tab that is already open.
#[component]
fn PanelTab(panel: ReadSignal<Option<PanelView>>) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || panel.get().map(|panel| view! { <PanelReport panel=panel /> })}
        </div>
    }
}

/// A panel study: one configuration, many instruments, and what the spread
/// across them says that any single one could not.
#[component]
fn PanelReport(panel: PanelView) -> impl IntoView {
    let verdict_class = verdict_class(&panel.verdict);
    let params = panel
        .selected_params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let deflation = panel.expected_best_under_null.map_or_else(
        || "not applicable: every configuration scored alike".to_owned(),
        |bar| {
            format!(
                "best pooled in-sample Sharpe {:.2} against {bar:.2} expected from {} no-skill \
                 trials",
                panel.best_sharpe, panel.trials
            )
        },
    );
    let deflation_class = if panel.survived_deflation {
        ""
    } else {
        "research-flag"
    };
    let consistency = format!(
        "{} of {} instruments beat their own benchmark",
        panel.beat_benchmark, panel.instruments
    );
    let consistent = panel.instruments > 0 && panel.beat_benchmark * 2 > panel.instruments;

    view! {
        <div class="research-report">
            <div class=verdict_class>{panel.verdict.clone()}</div>
            <p class="research-subject">
                {format!("Panel of {} instruments", panel.instruments)}
            </p>

            <ul class="research-reasons">
                {panel.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
            </ul>

            <div class="metric-cards">
                <MetricCard
                    label="Mean excess return"
                    value=percent(panel.mean_excess_return)
                    tone=panel.mean_excess_return
                    note="vs buy and hold".to_owned()
                />
                <MetricCard
                    label="Consistency"
                    value=format!("{}/{}", panel.beat_benchmark, panel.instruments)
                    note="instruments beat their benchmark".to_owned()
                />
                <MetricCard
                    label="Trades (pooled)"
                    value=panel.total_trades.to_string()
                    note=format!("{} configurations tried", panel.trials)
                />
                <MetricCard
                    label="Mean drawdown"
                    value=percent(panel.mean_max_drawdown)
                    note=format!("worst {}", percent(panel.worst_max_drawdown))
                />
            </div>

            <h4>"Pooled out of sample"</h4>
            <table class="research-metrics">
                <tbody>
                    <tr>
                        <td>"Mean excess return"</td>
                        <td>{percent(panel.mean_excess_return)}</td>
                    </tr>
                    <tr>
                        <td>"Consistency"</td>
                        <td class=if consistent { "" } else { "research-flag" }>{consistency}</td>
                    </tr>
                    <tr>
                        <td>"Trades (pooled)"</td>
                        <td>{panel.total_trades}</td>
                    </tr>
                    <tr>
                        <td>"Mean drawdown"</td>
                        <td>{percent(panel.mean_max_drawdown)}</td>
                    </tr>
                    <tr>
                        <td>"Worst drawdown"</td>
                        <td>{percent(panel.worst_max_drawdown)}</td>
                    </tr>
                </tbody>
            </table>

            <h4>"Per instrument"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th>"Instrument"</th>
                        <th>"Strategy"</th>
                        <th>"Buy and hold"</th>
                        <th>"Excess"</th>
                        <th>"Drawdown"</th>
                        <th>"Trades"</th>
                    </tr>
                </thead>
                <tbody>
                    {panel
                        .per_instrument
                        .iter()
                        .map(|outcome| {
                            let beat = outcome.excess_return > 0.0;
                            view! {
                                <tr>
                                    <td>{outcome.instrument.clone()}</td>
                                    <td>{percent(outcome.strategy_return)}</td>
                                    <td>{percent(outcome.benchmark_return)}</td>
                                    <td class=if beat { "" } else { "research-flag" }>
                                        {percent(outcome.excess_return)}
                                    </td>
                                    <td>{percent(outcome.max_drawdown)}</td>
                                    <td>{outcome.trades}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>

            <h4>"How this was arrived at"</h4>
            <dl class="research-provenance">
                <dt>"Chosen on"</dt>
                <dd>{panel.in_sample.clone()}</dd>
                <dt>"Judged on"</dt>
                <dd>{panel.out_of_sample.clone()}</dd>
                <dt>"Configurations tried"</dt>
                <dd>{panel.trials}</dd>
                <dt>"Multiple-testing check"</dt>
                <dd class=deflation_class>{deflation}</dd>
                <dt>"One configuration for all"</dt>
                <dd>{params}</dd>
                <dt>"Dataset"</dt>
                <dd class="research-hash">{short_hash(&panel.dataset_version)}</dd>
                <dt>"Strategy"</dt>
                <dd>{panel.strategy_name.clone()}</dd>
                <dt>"Starting cash"</dt>
                <dd>{format!("{:.0} per instrument", panel.starting_cash)}</dd>
                <dt>"Commission"</dt>
                <dd>{format!("{} bps", panel.commission_bps)}</dd>
                <dt>"Slippage"</dt>
                <dd>{format!("{} bps a side", panel.slippage_bps)}</dd>
                <dt>"Engine"</dt>
                <dd>{panel.engine.clone()}</dd>
            </dl>

            {(!panel.failures.is_empty())
                .then(|| {
                    view! {
                        <div>
                            <h4>"Runs that did not complete"</h4>
                            <ul class="research-reasons">
                                {panel
                                    .failures
                                    .iter()
                                    .map(|f| view! { <li>{f.clone()}</li> })
                                    .collect_view()}
                            </ul>
                        </div>
                    }
                })}
        </div>
    }
}

/// A headline number with its label, and a sign-coloured variant for the ones
/// where up and down mean good and bad.
#[component]
fn MetricCard(
    label: &'static str,
    value: String,
    #[prop(optional)] tone: Option<f64>,
    #[prop(optional)] note: Option<String>,
) -> impl IntoView {
    // Only where a sign genuinely carries meaning. Colouring a drawdown or a
    // trade count red would be decoration pretending to be information.
    let tone_class = match tone {
        Some(v) if v > 0.0 => "metric-card-value up",
        Some(v) if v < 0.0 => "metric-card-value down",
        _ => "metric-card-value",
    };
    view! {
        <div class="metric-card">
            <div class="metric-card-label">{label}</div>
            <div class=tone_class>{value}</div>
            {note.map(|note| view! { <div class="metric-card-note">{note}</div> })}
        </div>
    }
}

/// The equity curve, strategy against benchmark.
///
/// Mounts into a real element and hands it to the charting library, rather
/// than trying to describe a chart in `view!`. The effect re-runs when the
/// data changes, so an open tab redraws when its study is re-run.
#[component]
fn EquityChart(strategy: Vec<CurvePoint>, benchmark: Vec<CurvePoint>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        let to_js = |points: &Vec<CurvePoint>| {
            serde_wasm_bindgen::to_value(points).unwrap_or(JsValue::NULL)
        };
        render_equity_chart(&el, to_js(&strategy), to_js(&benchmark));
    });

    view! {
        <div class="equity-chart">
            <div class="equity-chart-legend">
                <span class="equity-chart-key strategy">"Strategy"</span>
                <span class="equity-chart-key benchmark">"Buy and hold"</span>
            </div>
            <div class="equity-chart-canvas" node_ref=holder></div>
        </div>
    }
}

/// Month-by-month returns as a grid of years against months.
///
/// What the round trips looked like, and what they cost.
///
/// A return says a rule made money. This says whether it did so the way it
/// would have to keep doing so: a high win rate with negative expectancy is
/// the most common shape of a strategy that looks good and loses, and no
/// summary statistic drawn from the equity curve can show it.
///
/// Fees are shown as money *and* as a fraction of capital, because that is
/// the comparison that matters — 2% of fees against a 3% return is the
/// finding, and neither number says it alone. They are fees and not the total
/// cost of trading: slippage is charged inside the fill prices, so it is
/// already subtracted from the return and never appears as a line item.
#[component]
fn TradeDetail(trades: TradesView) -> impl IntoView {
    // "—" rather than a zero throughout: a statistic that has no value
    // because nothing closed is not the same as one that measured zero, and
    // the whole point of these being `Option` upstream is to keep them apart.
    let pct = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{:.0}%", v * 100.0));
    let ratio = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"));
    let money = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{v:+.0}"));
    let days = trades
        .average_holding_days
        .map_or_else(|| "—".to_owned(), |d| format!("{d:.1} days"));
    let open_note = (trades.still_open > 0)
        .then(|| format!(" ({} still open)", trades.still_open));
    let expectancy_class = match trades.expectancy {
        Some(value) if value > 0.0 => "research-good",
        Some(_) => "research-bad",
        None => "",
    };

    view! {
        <dl class="research-provenance">
            <dt>"Closed round trips"</dt>
            <dd>{format!("{}{}", trades.closed, open_note.unwrap_or_default())}</dd>
            <dt>"Win rate"</dt>
            <dd>{pct(trades.win_rate)}</dd>
            <dt>"Expectancy per trade"</dt>
            <dd class=expectancy_class>{money(trades.expectancy)}</dd>
            <dt>"Profit factor"</dt>
            <dd>{ratio(trades.profit_factor)}</dd>
            <dt>"Average win / loss"</dt>
            <dd>{format!("{} / {}", money(trades.average_win), money(trades.average_loss.map(|l| -l)))}</dd>
            <dt>"Average hold"</dt>
            <dd>{days}</dd>
            <dt>"Exits"</dt>
            <dd>{format!("{} on signal, {} on stop", trades.signal_exits, trades.stop_exits)}</dd>
            <dt>"Fees and commission"</dt>
            <dd title="Slippage is charged in the fill prices and is already in the return">
                {format!("{:.0} ({:.2}% of capital)", trades.fees_paid, trades.fees_fraction * 100.0)}
            </dd>
        </dl>
    }
}

/// A total return says what was earned; this says whether it arrived steadily
/// or in one quarter that will not repeat. Cells are shaded by magnitude
/// relative to the largest move in the table, so a quiet strategy is not
/// rendered as a wall of colour and a violent one is not washed out.
#[component]
fn MonthlyReturns(months: Vec<MonthlyReturnView>) -> impl IntoView {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let mut years: Vec<i32> = months.iter().map(|m| m.year).collect();
    years.sort_unstable();
    years.dedup();

    // Scale to the biggest absolute move present, with a floor so a table of
    // near-zero months does not get amplified into apparent drama.
    let peak = months
        .iter()
        .map(|m| m.value.abs())
        .fold(0.0_f64, f64::max)
        .max(0.01);

    view! {
        <div class="monthly-scroll">
            <table class="monthly">
                <thead>
                    <tr>
                        <th></th>
                        {NAMES.iter().map(|name| view! { <th>{*name}</th> }).collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {years
                        .into_iter()
                        .map(|year| {
                            let cells = (1..=12u32)
                                .map(|month| {
                                    let found = months
                                        .iter()
                                        .find(|m| m.year == year && m.month == month);
                                    match found {
                                        None => view! { <td class="monthly-empty"></td> }.into_any(),
                                        Some(entry) => {
                                            // Opacity carries magnitude, hue carries sign. Two
                                            // channels for two facts, rather than a colour ramp
                                            // that has to be looked up in a legend.
                                            let weight = (entry.value.abs() / peak).clamp(0.12, 1.0);
                                            let class = if entry.value >= 0.0 {
                                                "monthly-cell up"
                                            } else {
                                                "monthly-cell down"
                                            };
                                            view! {
                                                <td
                                                    class=class
                                                    style=format!("--weight:{weight:.3}")
                                                    title=format!(
                                                        "{} {year}: {}",
                                                        NAMES[(month - 1) as usize],
                                                        percent(entry.value),
                                                    )
                                                >
                                                    {format!("{:.1}", entry.value * 100.0)}
                                                </td>
                                            }
                                                .into_any()
                                        }
                                    }
                                })
                                .collect_view();
                            view! {
                                <tr>
                                    <th class="monthly-year">{year}</th>
                                    {cells}
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

/// Portfolio value over time.
///
/// Reuses the equity-chart glue with an empty second series rather than adding
/// a second charting path: one series is the degenerate case of two, and a
/// parallel implementation would be a second place for the theme handling and
/// resize behaviour to drift.
#[component]
fn ValueChart(points: Vec<CurvePoint>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        let series = serde_wasm_bindgen::to_value(&points).unwrap_or(JsValue::NULL);
        let empty =
            serde_wasm_bindgen::to_value(&Vec::<CurvePoint>::new()).unwrap_or(JsValue::NULL);
        render_equity_chart(&el, series, empty);
    });

    view! {
        <div class="equity-chart">
            <div class="equity-chart-legend">
                <span class="equity-chart-key strategy">"Portfolio value"</span>
            </div>
            <div class="equity-chart-canvas" node_ref=holder></div>
        </div>
    }
}

/// One study, rendered with its caveats attached rather than beside it.
#[component]
fn StudyReport(study: StudyView) -> impl IntoView {
    let verdict_class = verdict_class(&study.verdict);

    let params = study
        .selected_params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");

    let deflation = study.expected_best_under_null.map_or_else(
        || "not applicable: every configuration scored alike".to_owned(),
        |bar| {
            format!(
                "best in-sample Sharpe {:.2} against {bar:.2} expected from {} no-skill trials",
                study.best_sharpe, study.trials
            )
        },
    );
    let deflation_class = if study.survived_deflation {
        ""
    } else {
        "research-flag"
    };

    view! {
        <div class="research-report">
            <div class=verdict_class>{study.verdict.clone()}</div>
            <p class="research-subject">{study.instrument.clone()}</p>

            <ul class="research-reasons">
                {study.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
            </ul>

            <div class="metric-cards">
                <MetricCard
                    label="Excess return"
                    value=percent(study.excess_return)
                    tone=study.excess_return
                    note="vs buy and hold".to_owned()
                />
                <MetricCard
                    label="Strategy"
                    value=percent(study.strategy.total_return)
                    tone=study.strategy.total_return
                />
                <MetricCard
                    label="Buy and hold"
                    value=percent(study.benchmark.total_return)
                    tone=study.benchmark.total_return
                />
                <MetricCard label="Sharpe" value=ratio(study.strategy.sharpe) />
                <MetricCard
                    label="Max drawdown"
                    value=percent(study.strategy.max_drawdown)
                />
                <MetricCard
                    label="Trades"
                    value=study.strategy.trades.to_string()
                    note=format!("{} configurations tried", study.trials)
                />
            </div>

            {(!study.strategy_curve.is_empty())
                .then({
                    let strategy = study.strategy_curve.clone();
                    let benchmark = study.benchmark_curve.clone();
                    move || view! { <EquityChart strategy=strategy benchmark=benchmark /> }
                })}

            <h4>"Out of sample"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th></th>
                        <th>"Strategy"</th>
                        <th>"Buy and hold"</th>
                    </tr>
                </thead>
                <tbody>
                    <tr>
                        <td>"Return"</td>
                        <td>{percent(study.strategy.total_return)}</td>
                        <td>{percent(study.benchmark.total_return)}</td>
                    </tr>
                    <tr>
                        <td>"CAGR"</td>
                        <td>{percent(study.strategy.cagr)}</td>
                        <td>{percent(study.benchmark.cagr)}</td>
                    </tr>
                    <tr>
                        <td>"Max drawdown"</td>
                        <td>{percent(study.strategy.max_drawdown)}</td>
                        <td>{percent(study.benchmark.max_drawdown)}</td>
                    </tr>
                    <tr>
                        <td>"Volatility"</td>
                        <td>{percent(study.strategy.volatility)}</td>
                        <td>{percent(study.benchmark.volatility)}</td>
                    </tr>
                    <tr>
                        <td>"Sharpe"</td>
                        <td>{ratio(study.strategy.sharpe)}</td>
                        <td>{ratio(study.benchmark.sharpe)}</td>
                    </tr>
                    <tr>
                        <td>"Sortino"</td>
                        <td>{ratio(study.strategy.sortino)}</td>
                        <td>{ratio(study.benchmark.sortino)}</td>
                    </tr>
                    <tr>
                        <td>"Calmar"</td>
                        <td>{ratio(study.strategy.calmar)}</td>
                        <td>{ratio(study.benchmark.calmar)}</td>
                    </tr>
                    <tr>
                        <td>"Trades"</td>
                        <td>{study.strategy.trades}</td>
                        <td>{study.benchmark.trades}</td>
                    </tr>
                    <tr class="research-excess">
                        <td>"Excess return"</td>
                        <td colspan="2">{percent(study.excess_return)}</td>
                    </tr>
                </tbody>
            </table>

            {(!study.monthly.is_empty())
                .then({
                    let months = study.monthly.clone();
                    move || {
                        view! {
                            <h4>"Monthly returns"</h4>
                            <MonthlyReturns months=months />
                        }
                    }
                })}

            <h4>"The trades behind it"</h4>
            <TradeDetail trades=study.trades_detail.clone() />

            <h4>"How this was arrived at"</h4>
            <dl class="research-provenance">
                <dt>"Chosen on"</dt>
                <dd>{study.in_sample.clone()}</dd>
                <dt>"Judged on"</dt>
                <dd>{study.out_of_sample.clone()}</dd>
                <dt>"Configurations tried"</dt>
                <dd>{study.trials}</dd>
                <dt>"Multiple-testing check"</dt>
                <dd class=deflation_class>{deflation}</dd>
                <dt>"Winning parameters"</dt>
                <dd>{params}</dd>
                <dt>"Dataset"</dt>
                <dd class="research-hash">{short_hash(&study.dataset_version)}</dd>
                <dt>"Strategy"</dt>
                <dd>{study.strategy_name.clone()}</dd>
                <dt>"Starting cash"</dt>
                <dd>{format!("{:.0}", study.starting_cash)}</dd>
                <dt>"Commission"</dt>
                <dd>{format!("{} bps", study.commission_bps)}</dd>
                <dt>"Slippage"</dt>
                <dd>{format!("{} bps a side", study.slippage_bps)}</dd>
                <dt>"Engine"</dt>
                <dd>{study.engine.clone()}</dd>
            </dl>
        </div>
    }
}

/// What you hold: the sidebar picker, and the overview it opens.
///
/// Read-only, and there is no credential anywhere in this path. Holdings come
/// from a file you export yourself — see `arvo_portfolio::csv` for why that is
/// the deliberate choice rather than a placeholder.
#[component]
fn PortfolioSidebar(
    portfolios: ReadSignal<Option<PortfolioLibraryView>>,
    set_open_portfolio: WriteSignal<Option<PortfolioView>>,
) -> impl IntoView {
    view! {
        <div class="sidebar-view">
            <h3>"Portfolio"</h3>
            {move || match portfolios.get() {
                None => view! { <p class="sidebar-empty">"Looking for holdings…"</p> }.into_any(),
                Some(library) if library.portfolios.is_empty() => {
                    view! {
                        <div>
                            <p class="sidebar-empty">"No holdings yet"</p>
                            <p class="research-hint">
                                "Drop a CSV here, one per portfolio, with the header "
                                <code>"instrument,quantity,cost_basis,price"</code>
                                ". Cost basis is the total paid, not per share; price may be blank \
                                 to use the last close from your data. "
                                <code>"CASH"</code>
                                " is worth face value."
                            </p>
                            <p class="research-path">{library.directory.clone()}</p>
                        </div>
                    }
                        .into_any()
                }
                Some(library) => {
                    view! {
                        <ul class="research-instruments">
                            {library
                                .portfolios
                                .into_iter()
                                .map(|portfolio| {
                                    let name = portfolio.name.clone();
                                    let summary = format!(
                                        "{} · {} holdings",
                                        money(portfolio.total_value),
                                        portfolio.holdings.len(),
                                    );
                                    // A 401(k) usually reports no cost basis,
                                    // so there may be no gain to show at all.
                                    let gain = portfolio.unrealized;
                                    let gain_class = match gain {
                                        Some(gain) if gain < 0.0 => "portfolio-delta down",
                                        Some(_) => "portfolio-delta up",
                                        None => "portfolio-delta",
                                    };
                                    let gain_text = gain.map_or_else(
                                        || "cost basis not reported".to_owned(),
                                        money,
                                    );
                                    view! {
                                        <li>
                                            <button
                                                class="research-instrument"
                                                on:click=move |_| {
                                                    set_open_portfolio.set(Some(portfolio.clone()));
                                                    open_study_panel(
                                                        PORTFOLIO_PANEL_ID,
                                                        "Portfolio",
                                                    );
                                                }
                                            >
                                                <span class="research-instrument-id">{name}</span>
                                                <span class="research-instrument-meta">
                                                    {summary}
                                                </span>
                                                <span class=gain_class>{gain_text}</span>
                                            </button>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// The portfolio tab, read back out of the signal so it refreshes in place.
#[component]
fn PortfolioTab(portfolio: ReadSignal<Option<PortfolioView>>) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || {
                portfolio.get().map(|portfolio| view! { <PortfolioReport portfolio=portfolio /> })
            }}
        </div>
    }
}

/// Holdings, what they are worth, and how the money is distributed.
#[component]
fn PortfolioReport(portfolio: PortfolioView) -> impl IntoView {
    let invested = portfolio.total_value - portfolio.cash;
    // Hoisted out of the view: the macro wants a value or a closure in an
    // attribute, never a bare `if`.
    let unrealized_value = portfolio.unrealized.map_or_else(|| "—".to_owned(), money);
    let unrealized_note = portfolio.unrealized_pct.map_or_else(
        || {
            format!(
                "no cost basis on {} holding(s)",
                portfolio.without_cost_basis
            )
        },
        percent,
    );
    let cost_value = portfolio.total_cost.map_or_else(|| "—".to_owned(), money);
    let cost_note = if portfolio.without_cost_basis > 0 {
        "not reported by the source".to_owned()
    } else {
        String::new()
    };
    let cash_note = if portfolio.total_value == 0.0 {
        String::new()
    } else {
        format!(
            "{} of total",
            percent(portfolio.cash / portfolio.total_value)
        )
    };

    // A single observation has nothing to have changed from, and an em dash
    // says that where a "+0.00" would claim a flat day nobody measured.
    let (change_value, change_note, change_tone) = match &portfolio.change {
        None => (
            "—".to_owned(),
            "needs a second day to compare".to_owned(),
            0.0,
        ),
        // Both dates, not just "since X". Snapshots are taken when the
        // portfolio is looked at, so two consecutive ones can be weeks apart —
        // calling that "today's change" would be wrong.
        Some(change) => (
            money(change.absolute),
            format!(
                "{} · {} → {}",
                change.percent.map_or_else(|| "no base".to_owned(), percent),
                change.from.clone(),
                change.to.clone(),
            ),
            change.absolute,
        ),
    };
    let chart_points: Vec<CurvePoint> = portfolio
        .value_history
        .iter()
        .map(|point| CurvePoint {
            time: point.time.clone(),
            value: point.value,
        })
        .collect();
    // One point draws a chart with nothing to see; the card already says so.
    let show_chart = chart_points.len() > 1;
    view! {
        <div class="research-report">
            <p class="research-subject">{portfolio.name.clone()}</p>
            <p class="research-instrument-meta">
                {format!("Valued {}", portfolio.as_of.clone())}
            </p>

            <div class="metric-cards">
                <MetricCard label="Total value" value=money(portfolio.total_value) />
                <MetricCard
                    label="Unrealised"
                    value=unrealized_value
                    tone=portfolio.unrealized.unwrap_or(0.0)
                    note=unrealized_note
                />
                <MetricCard label="Cost basis" value=cost_value note=cost_note />
                <MetricCard
                    label="Cash"
                    value=money(portfolio.cash)
                    note=cash_note
                />
                <MetricCard
                    label="Change"
                    value=change_value
                    tone=change_tone
                    note=change_note
                />
                <MetricCard label="Invested" value=money(invested) />
                <MetricCard
                    label="Positions"
                    value=portfolio.holdings.len().to_string()
                />
            </div>

            {show_chart
                .then({
                    let chart_points = chart_points.clone();
                    move || {
                        view! {
                            <h4>"Value over time"</h4>
                            <ValueChart points=chart_points />
                        }
                    }
                })}

            <h4>"Allocation"</h4>
            <div class="allocation">
                {portfolio
                    .holdings
                    .iter()
                    .map(|holding| {
                        let width = format!("{:.2}%", holding.weight * 100.0);
                        view! {
                            <div class="allocation-row">
                                <span class="allocation-label">{holding.instrument.clone()}</span>
                                <span class="allocation-track">
                                    <span class="allocation-bar" style=format!("width:{width}") />
                                </span>
                                <span class="allocation-weight">{percent(holding.weight)}</span>
                            </div>
                        }
                    })
                    .collect_view()}
            </div>

            <h4>"Holdings"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th>"Instrument"</th>
                        <th>"Qty"</th>
                        <th>"Price"</th>
                        <th>"Value"</th>
                        <th>"Cost"</th>
                        <th>"Unrealised"</th>
                        <th>"Priced by"</th>
                    </tr>
                </thead>
                <tbody>
                    {portfolio
                        .holdings
                        .iter()
                        .map(|holding| {
                            let gain_class = match holding.unrealized {
                                Some(gain) if gain < 0.0 => "gain-down",
                                Some(_) => "gain-up",
                                None => "",
                            };
                            let gain_text = match holding.unrealized {
                                None => "—".to_owned(),
                                Some(gain) => format!(
                                    "{} {}",
                                    money(gain),
                                    holding
                                        .unrealized_pct
                                        .map_or_else(String::new, |pct| format!("({})", percent(pct))),
                                ),
                            };
                            let cost_text = holding
                                .cost_basis
                                .map_or_else(|| "—".to_owned(), money);
                            // A collective trust reports a balance and no unit
                            // count; an em dash says "not reported" where a 0
                            // would claim the position is empty.
                            let qty_text = holding
                                .quantity
                                .map_or_else(|| "—".to_owned(), |q| format!("{q:.4}"));
                            let price_text = holding
                                .price
                                .map_or_else(|| "—".to_owned(), money);
                            view! {
                                <tr>
                                    <td>{holding.instrument.clone()}</td>
                                    <td>{qty_text}</td>
                                    <td>{price_text}</td>
                                    <td>{money(holding.market_value)}</td>
                                    <td>{cost_text}</td>
                                    <td class=gain_class>{gain_text}</td>
                                    <td class="priced-by">{holding.priced_by.clone()}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>

            <h4>"How this file was read"</h4>
            <p class="research-hint">
                "Column names vary by broker, so they are matched by name and the mapping is \
                 shown here. A wrong guess produces a portfolio that looks entirely plausible; \
                 this is what makes it visible."
            </p>
            <dl class="research-provenance">
                {portfolio
                    .import
                    .columns
                    .iter()
                    .map(|(role, column)| {
                        view! {
                            <dt>{role.clone()}</dt>
                            <dd class="research-hash">{column.clone()}</dd>
                        }
                    })
                    .collect_view()}
                <dt>"Rows imported"</dt>
                <dd>{portfolio.import.rows_imported}</dd>
            </dl>
            {portfolio
                .import
                .cost_basis_derived
                .then(|| {
                    view! {
                        <p class="research-stale">
                            "Cost basis was multiplied up from a per-share column. Check one \
                             holding against your statement before trusting the totals."
                        </p>
                    }
                })}
            {(!portfolio.import.ignored.is_empty())
                .then({
                    let ignored = portfolio.import.ignored.join(", ");
                    move || {
                        view! {
                            <p class="research-hint">{format!("Columns not used: {ignored}")}</p>
                        }
                    }
                })}
            {(!portfolio.import.rows_skipped.is_empty())
                .then({
                    let skipped = portfolio.import.rows_skipped.clone();
                    move || {
                        view! {
                            <div>
                                <p class="research-hint">
                                    "Lines that were not holdings — usually a disclaimer footer:"
                                </p>
                                <ul class="research-reasons">
                                    {skipped
                                        .iter()
                                        .map(|line| view! { <li>{line.clone()}</li> })
                                        .collect_view()}
                                </ul>
                            </div>
                        }
                    }
                })}

            {(!portfolio.unpriced.is_empty())
                .then({
                    let unpriced = portfolio.unpriced.clone();
                    move || {
                        view! {
                            <div>
                                <h4>"Not included"</h4>
                                <p class="research-hint">
                                    "No price was available for these, so they are excluded from \
                                     every total above rather than counted as zero. Add a price \
                                     column, or add their daily bars to the data library."
                                </p>
                                <ul class="research-reasons">
                                    {unpriced
                                        .iter()
                                        .map(|id| view! { <li>{id.clone()}</li> })
                                        .collect_view()}
                                </ul>
                            </div>
                        }
                    }
                })}
        </div>
    }
}

#[component]
fn AlertsView() -> impl IntoView {
    view! {
        <div class="sidebar-view">
            <h3>"Alerts"</h3>
            <p class="sidebar-empty">"No active alerts"</p>
        </div>
    }
}

/// The theme picker is Settings' one real setting — replaces the old
/// activity-bar sun/moon toggle, which only had room for two states and
/// stopped fitting once Catppuccin Mocha became a third option.
#[component]
fn SettingsView(theme: ReadSignal<Theme>, set_theme: WriteSignal<Theme>) -> impl IntoView {
    let option = |value: Theme| {
        view! {
            <button
                class="theme-option"
                class:active=move || theme.get() == value
                on:click=move |_| set_theme.set(value)
            >
                {value.label()}
            </button>
        }
    };
    view! {
        <div class="sidebar-view">
            <h3>"Settings"</h3>
            <h4>"Theme"</h4>
            <div class="theme-picker">
                {option(Theme::Light)} {option(Theme::Dark)} {option(Theme::CatppuccinMocha)}
            </div>
        </div>
    }
}

#[component]
fn SidebarPanel(
    active_view: ReadSignal<Option<ActivityView>>,
    plugins: ReadSignal<Vec<PluginView>>,
    set_plugins: WriteSignal<Vec<PluginView>>,
    theme: ReadSignal<Theme>,
    set_theme: WriteSignal<Theme>,
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
    set_studies: WriteSignal<std::collections::HashMap<String, StudyView>>,
    panel: ReadSignal<Option<PanelView>>,
    set_panel: WriteSignal<Option<PanelView>>,
    portfolios: ReadSignal<Option<PortfolioLibraryView>>,
    set_open_portfolio: WriteSignal<Option<PortfolioView>>,
) -> impl IntoView {
    view! {
        // Needs a real height: .sidebar-view inside it is `height: 100%`,
        // which resolves against nothing if this wrapper is auto-height —
        // so an overflowing sidebar was being clipped by the panel's own
        // `overflow: hidden` instead of scrolling (ticket 14).
        <div class="sidebar-panel-root">
            {move || match active_view.get() {
                Some(ActivityView::Portfolio) => {
                    view! {
                        <PortfolioSidebar
                            portfolios=portfolios
                            set_open_portfolio=set_open_portfolio
                        />
                    }
                        .into_any()
                }
                Some(ActivityView::Research) => {
                    view! {
                        <ResearchView
                            studies=studies
                            set_studies=set_studies
                            panel=panel
                            set_panel=set_panel
                        />
                    }
                        .into_any()
                }
                Some(ActivityView::Extensions) => {
                    view! { <ExtensionsView plugins=plugins set_plugins=set_plugins /> }
                        .into_any()
                }
                Some(ActivityView::Alerts) => view! { <AlertsView /> }.into_any(),
                Some(ActivityView::Settings) => {
                    view! { <SettingsView theme=theme set_theme=set_theme /> }.into_any()
                }
                // ponytail: the dockview panel itself stays present (see
                // ActivityBar's toggle) — this collapses its *content*,
                // not its width. True width-collapse (matching VS Code
                // exactly) would mean dockview add/remove-ing the panel,
                // which risks leaking the Leptos mount on repeated
                // toggles; add if a real need for it shows up.
                None => view! { <div /> }.into_any(),
            }}
        </div>
    }
}

/// One File/View/Help dropdown. Visibility is a CSS class toggle, not a
/// mount/unmount (`<Show>`'s children run per-flip, but `children` here is
/// `FnOnce` — building the panel once and hiding it is simpler than
/// threading a `ChildrenFn` through for two dropdown items). Clicking the
/// open menu's own label closes it; clicking a different label switches
/// directly. No click-outside-to-dismiss yet — a real gap, not chased here
/// since every item that needs it already closes itself on click (see
/// View's Output toggle).
#[component]
fn MenuDropdown(
    id: MenuId,
    label: &'static str,
    open_menu: ReadSignal<Option<MenuId>>,
    set_open_menu: WriteSignal<Option<MenuId>>,
    children: Children,
) -> impl IntoView {
    let content = children();
    view! {
        <div class="menu-dropdown">
            <button
                class="menu-label"
                class:active=move || open_menu.get() == Some(id)
                on:click=move |_| {
                    set_open_menu.update(|m| *m = if *m == Some(id) { None } else { Some(id) })
                }
            >
                {label}
            </button>
            <div class="menu-panel" class:hidden=move || open_menu.get() != Some(id)>
                {content}
            </div>
        </div>
    }
}

/// One thing the palette can do.
///
/// The action is a closure over the app's own signals rather than a message
/// enum: every command here is something a menu item or a button already does,
/// so a parallel dispatch layer would be a second way to describe the same
/// action and a second place for the two to disagree.
#[derive(Clone)]
struct Command {
    /// Shown as "Category: Title", and matched against in that combined form
    /// so typing "theme dark" finds "Theme: Dark".
    category: &'static str,
    title: &'static str,
    run: Arc<dyn Fn() + Send + Sync>,
}

impl Command {
    fn label(&self) -> String {
        format!("{}: {}", self.category, self.title)
    }
}

/// Subsequence match, the way a command palette is expected to behave: every
/// character of `query` must appear in order, not necessarily adjacently, so
/// "tgo" finds "Toggle Output".
///
/// Returns a score where **lower is better**, or `None` for no match. The
/// score charges for distance — a match that starts late, or is scattered
/// across the string, ranks below a tight one near the front.
fn fuzzy_score(haystack: &str, query: &str) -> Option<u32> {
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0_u32;
    let mut cursor = 0_usize;
    let mut previous: Option<usize> = None;

    for needle in query.to_lowercase().chars() {
        // Spaces are how people separate words they half-remember; requiring
        // them to match literally would break "theme mocha".
        if needle.is_whitespace() {
            continue;
        }
        let found = hay[cursor..].iter().position(|c| *c == needle)? + cursor;
        score += u32::try_from(match previous {
            // A gap between matched characters costs; an adjacent run is free.
            Some(prev) => found - prev - 1,
            // The first match costs its distance from the start.
            None => found,
        })
        .unwrap_or(u32::MAX);
        previous = Some(found);
        cursor = found + 1;
    }

    Some(score)
}

/// Everything the palette can run.
///
/// Only actions that already exist elsewhere in the shell. A palette listing
/// commands that do nothing is worse than a short palette.
fn commands(
    set_active_view: WriteSignal<Option<ActivityView>>,
    output_visible: ReadSignal<bool>,
    set_output_visible: WriteSignal<bool>,
    set_theme: WriteSignal<Theme>,
) -> Vec<Command> {
    let show = move |view: ActivityView| {
        Arc::new(move || set_active_view.set(Some(view))) as Arc<dyn Fn() + Send + Sync>
    };

    vec![
        Command {
            category: "View",
            title: "Toggle Sidebar",
            run: Arc::new(move || {
                set_active_view.update(|current| {
                    *current = match *current {
                        Some(_) => None,
                        None => Some(ActivityView::Research),
                    };
                });
            }),
        },
        Command {
            category: "View",
            title: "Toggle Output Panel",
            run: Arc::new(move || set_output_visible.set(!output_visible.get_untracked())),
        },
        Command {
            category: "View",
            title: "Research",
            run: show(ActivityView::Research),
        },
        Command {
            category: "View",
            title: "Extensions",
            run: show(ActivityView::Extensions),
        },
        Command {
            category: "View",
            title: "Alerts",
            run: show(ActivityView::Alerts),
        },
        Command {
            category: "Preferences",
            title: "Settings",
            run: show(ActivityView::Settings),
        },
        Command {
            category: "Theme",
            title: "Light",
            run: Arc::new(move || set_theme.set(Theme::Light)),
        },
        Command {
            category: "Theme",
            title: "Dark",
            run: Arc::new(move || set_theme.set(Theme::Dark)),
        },
        Command {
            category: "Theme",
            title: "Catppuccin Mocha",
            run: Arc::new(move || set_theme.set(Theme::CatppuccinMocha)),
        },
        Command {
            category: "Window",
            title: "Minimize",
            run: Arc::new(minimize_window),
        },
        Command {
            category: "Window",
            title: "Toggle Maximize",
            run: Arc::new(toggle_maximize_window),
        },
        Command {
            category: "Window",
            title: "Close",
            run: Arc::new(close_window),
        },
    ]
}

/// VS Code's command palette: a filter box over everything the shell can do.
///
/// Opens on Ctrl+Shift+P or F1, filters as you type, moves with the arrow
/// keys, runs on Enter, dismisses on Escape or a click outside.
#[component]
fn CommandPalette(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
    commands: Vec<Command>,
) -> impl IntoView {
    let (query, set_query) = signal(String::new());
    let (selected, set_selected) = signal(0_usize);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let matches = {
        let commands = commands.clone();
        move || {
            let query = query.get();
            let mut scored: Vec<(u32, Command)> = commands
                .iter()
                .filter_map(|command| {
                    fuzzy_score(&command.label(), &query).map(|score| (score, command.clone()))
                })
                .collect();
            // `sort_by_key` is stable, so equally-scored commands keep the
            // declaration order above — which is a deliberate ordering, not an
            // arbitrary one.
            scored.sort_by_key(|(score, _)| *score);
            scored
                .into_iter()
                .map(|(_, command)| command)
                .collect::<Vec<_>>()
        }
    };

    // A fresh query means the old highlight points at a different command.
    Effect::new(move |_| {
        query.track();
        set_selected.set(0);
    });

    // Focus on open, and clear whatever was typed last time: a palette that
    // reopens holding a stale filter hides the commands you just asked for.
    Effect::new(move |_| {
        if open.get() {
            set_query.set(String::new());
            set_selected.set(0);
            if let Some(input) = input_ref.get() {
                let _ = input.focus();
            }
        }
    });

    let run_selected = {
        let matches = matches.clone();
        move || {
            if let Some(command) = matches().get(selected.get_untracked()) {
                set_open.set(false);
                (command.run)();
            }
        }
    };

    let on_key = {
        let matches = matches.clone();
        let run_selected = run_selected.clone();
        move |ev: leptos::ev::KeyboardEvent| match ev.key().as_str() {
            "Escape" => set_open.set(false),
            "Enter" => run_selected(),
            "ArrowDown" => {
                ev.prevent_default();
                let count = matches().len();
                if count > 0 {
                    set_selected.update(|i| *i = (*i + 1) % count);
                }
            }
            "ArrowUp" => {
                ev.prevent_default();
                let count = matches().len();
                if count > 0 {
                    set_selected.update(|i| *i = (*i + count - 1) % count);
                }
            }
            _ => {}
        }
    };

    view! {
        <div class="palette-layer" class:hidden=move || !open.get()>
            // Same backdrop trick the menus use: dismissal by a real element
            // underneath, rather than a document listener that has to work out
            // whether the click landed inside the palette.
            <div class="palette-backdrop" on:click=move |_| set_open.set(false) />
            <div class="palette">
                <input
                    node_ref=input_ref
                    class="palette-input"
                    type="text"
                    placeholder="Type a command…"
                    autocomplete="off"
                    spellcheck="false"
                    prop:value=move || query.get()
                    on:input=move |ev| set_query.set(event_target_value(&ev))
                    on:keydown=on_key
                />
                <ul class="palette-results">
                    {move || {
                        let found = matches();
                        if found.is_empty() {
                            return view! {
                                <li class="palette-empty">"No matching commands"</li>
                            }
                            .into_any();
                        }
                        found
                            .into_iter()
                            .enumerate()
                            .map(|(index, command)| {
                                let run = command.run.clone();
                                view! {
                                    <li
                                        class="palette-result"
                                        class:selected=move || selected.get() == index
                                        // Pointer, not click: the input would
                                        // lose focus on mousedown and the
                                        // backdrop would win the click.
                                        on:pointerdown=move |ev| {
                                            ev.prevent_default();
                                            set_open.set(false);
                                            run();
                                        }
                                        on:pointerenter=move |_| set_selected.set(index)
                                    >
                                        <span class="palette-category">{command.category}</span>
                                        <span class="palette-title">{command.title}</span>
                                    </li>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </ul>
            </div>
        </div>
    }
}

/// Fixed chrome, same category as `ActivityBar` — not a dockview panel.
/// Doubles as the OS title bar (`tauri.conf.json`'s `decorations: false`):
/// the app icon and the empty stretch between the menu and window controls
/// carry `data-tauri-drag-region` (Tauri's own attribute, no JS needed) so
/// the window drags from there; buttons deliberately don't carry it, or
/// their clicks would be swallowed by the drag gesture instead of firing.
/// File and Help have no commands yet, shown honestly rather than faked;
/// View's one real item — Output — is real because ticket 12 moved the
/// Output panel out of the default layout and into this menu.
#[component]
fn TopMenuBar(
    output_visible: ReadSignal<bool>,
    set_output_visible: WriteSignal<bool>,
    set_palette_open: WriteSignal<bool>,
) -> impl IntoView {
    let (open_menu, set_open_menu) = signal(None::<MenuId>);

    view! {
        <div class="top-menu-bar">
            // app-shell ticket 16: click-outside-to-dismiss, the gap ticket
            // 12 left open. A backdrop behind the open panel (rather than a
            // document-level listener that has to work out whether the click
            // landed inside a menu) — the menu labels sit above it, so they
            // still toggle normally.
            <div
                class="menu-backdrop"
                class:hidden=move || open_menu.get().is_none()
                on:click=move |_| set_open_menu.set(None)
            />
            <img class="app-icon" src="/tauri.svg" alt="" data-tauri-drag-region="true" />
            <nav class="menu-bar">
                <MenuDropdown id=MenuId::File label="File" open_menu=open_menu set_open_menu=set_open_menu>
                    <div class="menu-item-static">"No commands yet"</div>
                </MenuDropdown>
                <MenuDropdown id=MenuId::View label="View" open_menu=open_menu set_open_menu=set_open_menu>
                    <button
                        class="menu-item"
                        on:click=move |_| {
                            set_output_visible.update(|v| *v = !*v);
                            set_open_menu.set(None);
                        }
                    >
                        {move || if output_visible.get() { "\u{2713} Output" } else { "Output" }}
                    </button>
                    <button
                        class="menu-item"
                        on:click=move |_| {
                            set_open_menu.set(None);
                            set_palette_open.set(true);
                        }
                    >
                        "Command Palette…"
                    </button>
                </MenuDropdown>
                <MenuDropdown id=MenuId::Help label="Help" open_menu=open_menu set_open_menu=set_open_menu>
                    <div class="menu-item-static">"Arvo Desktop"</div>
                </MenuDropdown>
            </nav>
            // The palette trigger sits centred, VS Code style. Flanking
            // spacers (not a margin) keep it centred in the *window* rather
            // than in the leftover room beside the menus, and both carry the
            // drag region so the title bar still drags either side of it.
            <div class="title-bar-spacer" data-tauri-drag-region="true" />
            <button class="palette-trigger" on:click=move |_| set_palette_open.set(true)>
                <svg class="palette-trigger-icon" viewBox="0 0 16 16" aria-hidden="true">
                    <path
                        d="M7 2a5 5 0 1 0 3.1 8.9l3 3 1.4-1.4-3-3A5 5 0 0 0 7 2Zm0 2a3 3 0 1 1 0 6 3 3 0 0 1 0-6Z"
                        fill="currentColor"
                    />
                </svg>
                <span class="palette-trigger-label">"Search commands"</span>
                <span class="palette-trigger-hint">"Ctrl+Shift+P"</span>
            </button>
            <div class="title-bar-spacer" data-tauri-drag-region="true" />
            <div class="window-controls">
                <button class="window-control" title="Minimize" on:click=move |_| minimize_window()>
                    <svg viewBox="0 0 10 10" aria-hidden="true">
                        <rect x="0" y="4.5" width="10" height="1" fill="currentColor" />
                    </svg>
                </button>
                <button class="window-control" title="Maximize" on:click=move |_| toggle_maximize_window()>
                    <svg viewBox="0 0 10 10" aria-hidden="true">
                        <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" />
                    </svg>
                </button>
                <button class="window-control window-control-close" title="Close" on:click=move |_| close_window()>
                    <svg viewBox="0 0 10 10" aria-hidden="true">
                        <path d="M0.5 0.5L9.5 9.5M9.5 0.5L0.5 9.5" stroke="currentColor" />
                    </svg>
                </button>
            </div>
        </div>
    }
}

/// Real content, not an empty placeholder — a plugin reachability summary
/// from data already in `plugins`, no new state. Not interactive; this is
/// a visual-consistency ask, not a notifications center.
#[component]
fn StatusBar(plugins: ReadSignal<Vec<PluginView>>) -> impl IntoView {
    view! {
        <div class="status-bar">
            {move || {
                let all = plugins.get();
                let reachable = all
                    .iter()
                    .filter(|p| matches!(p.status, PluginStatusView::Reachable { .. }))
                    .count();
                format!("{reachable}/{} plugins reachable", all.len())
            }}
        </div>
    }
}

/// Default "Welcome" tab content. Wordmark, subtitle and tagline are the
/// real strings off `images/arvo_logos.png`'s own primary lockup; the mark
/// is a simplified line-art take on the same logo (peak + underlying
/// sweep), not a pixel copy of its 3D-rendered artwork.
#[component]
fn MainPanel() -> impl IntoView {
    view! {
        <div class="welcome">
            <svg class="welcome-mark" viewBox="0 0 100 100" aria-hidden="true">
                <path d="M28 82 L50 16 L72 82" />
                <path class="welcome-mark-sweep" d="M30 54 Q50 76 70 66" />
            </svg>
            <h1 class="welcome-title">"ARVO"</h1>
            <p class="welcome-subtitle">"Financial Intelligence Platform"</p>
            <p class="welcome-tagline">
                "Analyze" <span>"•"</span> "Simulate" <span>"•"</span> "Invest" <span>"•"</span> "Grow"
            </p>
        </div>
    }
}

#[component]
fn BottomPanel() -> impl IntoView {
    view! {
        <div class="sidebar-view">
            <p class="sidebar-empty">"Output"</p>
        </div>
    }
}

#[component]
pub fn App() -> impl IntoView {
    let (plugins, set_plugins) = signal(Vec::<PluginView>::new());
    // app-shell ticket 12: nothing open on cold start — Extensions used to
    // auto-open here, which is a bigger default than a fresh launch needs.
    let (active_view, set_active_view) = signal(None::<ActivityView>);
    // Catppuccin Mocha is never the OS-preference default — only light/dark
    // follow that; Catppuccin is opt-in from Settings.
    let (theme, set_theme) = signal(if prefers_dark() {
        Theme::Dark
    } else {
        Theme::Light
    });
    // Output moved out of the default layout into the View menu.
    let (output_visible, set_output_visible) = signal(false);
    let (palette_open, set_palette_open) = signal(false);
    // Hoisted out of ResearchView: the sidebar starts studies, and each one
    // gets its own tab beside Welcome. Keyed by instrument so a tab that is
    // already open refreshes rather than duplicating, and so a panel can find
    // its own study when dockview mounts it.
    let (studies, set_studies) = signal(std::collections::HashMap::<String, StudyView>::new());
    // One panel at a time: there is only one panel, and re-running it should
    // replace what the tab shows rather than accumulate tabs.
    let (panel, set_panel) = signal(None::<PanelView>);
    let (portfolios, set_portfolios) = signal(None::<PortfolioLibraryView>);
    let (open_portfolio, set_open_portfolio) = signal(None::<PortfolioView>);

    Effect::new(move |_| {
        spawn_local(async move {
            set_plugins.set(call("list_plugins").await);
        });
    });

    Effect::new(move |_| {
        spawn_local(async move {
            match call_typed::<PortfolioLibraryView>("list_portfolios", JsValue::UNDEFINED).await {
                Ok(library) => set_portfolios.set(Some(library)),
                Err(reason) => web_sys::console::error_1(&reason.into()),
            }
        });
    });

    Effect::new(move |_| apply_theme(theme.get()));

    // Window-level, not on an element: the shortcut has to work whatever has
    // focus, including inside a dockview panel that Leptos does not own.
    Effect::new(move |_| {
        let handle = window_event_listener(leptos::ev::keydown, move |ev| {
            let palette = (ev.ctrl_key() || ev.meta_key()) && ev.shift_key() && ev.key() == "P";
            if palette || ev.key() == "F1" {
                ev.prevent_default();
                set_palette_open.set(true);
            }
        });
        // Returned so Leptos drops the listener with the effect rather than
        // leaving it bound to a torn-down closure.
        on_cleanup(move || handle.remove());
    });

    // Sidebar and Output's mount handles are tracked (not `.forget()`-ten,
    // unlike the permanent main panel) so removing either actually unmounts
    // the Leptos root instead of leaking it — see the extern block's notes.
    // Output needs the same tracking as the sidebar now that ticket 12
    // makes it toggle on and off too, instead of being a permanent panel.
    let sidebar_mount = Rc::new(RefCell::new(None));
    let bottom_mount = Rc::new(RefCell::new(None));
    // Study tabs are closable, so their mounts need the same tracking the
    // sidebar and output already have — keyed, since there can be several.
    let study_mounts = Rc::new(RefCell::new(std::collections::HashMap::<String, _>::new()));

    // Runs once on mount; dispatches each dockview panel to its Leptos
    // component as dockview creates it. See ticket 01's Answer for why
    // the element reference (not an id) is what makes this reliable.
    Effect::new(move |_| {
        let sidebar_mount_created = sidebar_mount.clone();
        let sidebar_mount_removed = sidebar_mount.clone();
        let bottom_mount_created = bottom_mount.clone();
        let bottom_mount_removed = bottom_mount.clone();
        let study_mounts_created = study_mounts.clone();
        let study_mounts_removed = study_mounts.clone();

        let on_panel_created = Closure::<dyn FnMut(String, web_sys::HtmlElement)>::new(
            move |name: String, el: web_sys::HtmlElement| match name.as_str() {
                "sidebar" => {
                    let handle = mount_to(el, move || {
                        view! {
                            <SidebarPanel
                                active_view=active_view
                                plugins=plugins
                                set_plugins=set_plugins
                                theme=theme
                                set_theme=set_theme
                                studies=studies
                                set_studies=set_studies
                                panel=panel
                                set_panel=set_panel
                                portfolios=portfolios
                                set_open_portfolio=set_open_portfolio
                            />
                        }
                    });
                    *sidebar_mount_created.borrow_mut() = Some(handle);
                }
                "main" => {
                    mount_to(el, || view! { <MainPanel /> }).forget();
                }
                "bottom" => {
                    let handle = mount_to(el, || view! { <BottomPanel /> });
                    *bottom_mount_created.borrow_mut() = Some(handle);
                }
                // `into_any` on both: a study tab and the panel tab are
                // different opaque view types, and one map has to hold both.
                PORTFOLIO_PANEL_ID => {
                    let handle = mount_to(el, move || {
                        view! { <PortfolioTab portfolio=open_portfolio /> }.into_any()
                    });
                    study_mounts_created
                        .borrow_mut()
                        .insert(PORTFOLIO_PANEL_ID.to_owned(), handle);
                }
                PANEL_PANEL_ID => {
                    let handle =
                        mount_to(el, move || view! { <PanelTab panel=panel /> }.into_any());
                    study_mounts_created
                        .borrow_mut()
                        .insert(PANEL_PANEL_ID.to_owned(), handle);
                }
                id if id.starts_with(STUDY_PANEL_PREFIX) => {
                    let instrument = id[STUDY_PANEL_PREFIX.len()..].to_owned();
                    let handle = mount_to(el, move || {
                        view! { <StudyTab instrument=instrument studies=studies /> }.into_any()
                    });
                    study_mounts_created
                        .borrow_mut()
                        .insert(id.to_owned(), handle);
                }
                _ => {}
            },
        );
        let on_panel_removed = Closure::<dyn FnMut(String)>::new(move |name: String| {
            // Dropping the handle runs Leptos's real unmount cleanup — the
            // point of tracking it instead of `.forget()`ing.
            match name.as_str() {
                "sidebar" => {
                    sidebar_mount_removed.borrow_mut().take();
                }
                "bottom" => {
                    bottom_mount_removed.borrow_mut().take();
                }
                // Closing a study tab unmounts it but keeps the result in the
                // map, so reopening the same instrument is instant and does
                // not re-run eleven backtests.
                id if id == PANEL_PANEL_ID
                    || id == PORTFOLIO_PANEL_ID
                    || id.starts_with(STUDY_PANEL_PREFIX) =>
                {
                    study_mounts_removed.borrow_mut().remove(id);
                }
                _ => {}
            }
        });

        init_shell(
            "shell-host",
            on_panel_created.as_ref(),
            on_panel_removed.as_ref(),
        );
        on_panel_created.forget();
        on_panel_removed.forget();
    });

    // Keeps dockview's actual panels in sync with each toggle — real
    // width/height-collapse, not just cleared content.
    Effect::new(move |_| {
        set_sidebar_visible(active_view.get().is_some());
    });
    Effect::new(move |_| {
        set_output_visible_js(output_visible.get());
    });

    view! {
        <div class="shell-root">
            <TopMenuBar
                output_visible=output_visible
                set_output_visible=set_output_visible
                set_palette_open=set_palette_open
            />
            <CommandPalette
                open=palette_open
                set_open=set_palette_open
                commands=commands(
                    set_active_view,
                    output_visible,
                    set_output_visible,
                    set_theme,
                )
            />
            <div class="shell">
                <ActivityBar active_view=active_view set_active_view=set_active_view />
                <div
                    id="shell-host"
                    class="shell-host"
                    class:sidebar-open=move || active_view.get().is_some()
                ></div>
            </div>
            <StatusBar plugins=plugins />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn an_empty_query_matches_everything_equally() {
        assert_eq!(fuzzy_score("View: Toggle Sidebar", ""), Some(0));
    }

    #[test]
    fn characters_need_not_be_adjacent() {
        assert!(
            fuzzy_score("View: Toggle Output Panel", "tgo").is_some(),
            "a palette that only does substrings is not worth having"
        );
    }

    #[test]
    fn order_still_matters() {
        assert_eq!(
            fuzzy_score("View: Toggle Output Panel", "otggle"),
            None,
            "a subsequence match is not an anagram match"
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_query_spaces() {
        assert!(fuzzy_score("Theme: Catppuccin Mocha", "THEME MOCHA").is_some());
        assert!(fuzzy_score("Theme: Catppuccin Mocha", "theme mocha").is_some());
    }

    #[test]
    fn a_tighter_earlier_match_scores_better() {
        let exact = fuzzy_score("Theme: Dark", "theme").expect("matches");
        let scattered = fuzzy_score("View: Toggle Output Panel", "toe").expect("matches");
        assert!(
            exact < scattered,
            "a contiguous prefix must outrank a scattered one: {exact} vs {scattered}"
        );
    }

    #[test]
    fn a_missing_character_is_no_match() {
        assert_eq!(fuzzy_score("Theme: Dark", "zzz"), None);
    }
}
