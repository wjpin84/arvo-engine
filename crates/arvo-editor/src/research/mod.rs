//! The research views: what a study, a panel and a walk-forward look like.
//!
//! Split from the shell because they are the application and the shell is the
//! window it sits in. This is where every honesty decision the platform makes
//! becomes something a person actually reads — the verdict before the chart,
//! the reasons before the number, the recommendations above the equity curve
//! that would otherwise be believed before they were read.

mod panel;
mod study;
mod walk;

pub(crate) use panel::PanelTab;
pub(crate) use study::StudyReport;
pub(crate) use walk::WalkForwardReport;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::bridge::{
    call_typed, open_study_panel, PANEL_PANEL_ID, STUDY_PANEL_PREFIX, WALK_PANEL_PREFIX,
};
use crate::format::verdict_dot;
use crate::views::*;

/// The research view: pick an instrument, run a parameter study, read what
/// survived.
///
/// The display is deliberately loaded with caveats — the split, the number of
/// configurations tried, the bar a no-skill search would clear, the costs
/// assumed. A verdict shown alone is a number that looks like a fact, and the
/// whole reason this platform exists is that backtests are easy to believe.
#[component]
pub(crate) fn ResearchView(
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
    set_studies: WriteSignal<std::collections::HashMap<String, StudyView>>,
    walks: ReadSignal<std::collections::HashMap<String, WalkForwardView>>,
    set_walks: WriteSignal<std::collections::HashMap<String, WalkForwardView>>,
    /// Hoisted out of this component so the saved workspace can hold it —
    /// which strategy is selected is part of where you left off.
    chosen: ReadSignal<String>,
    set_chosen: WriteSignal<String>,
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
    // Findings the store could not read. Shown, not logged: four were once
    // lost to a field rename and the only trace was a warning nobody had
    // reason to look at.
    let (unreadable, set_unreadable) = signal(Vec::<UnreadableView>::new());
    // What the engine can actually run, fetched rather than hardcoded: a menu
    // that drifts from the engine offers rules it will then refuse.
    let (strategies, set_strategies) = signal(Vec::<StrategyView>::new());

    // Whether a broker token is held, never the token itself. A getter for the
    // credential would put it on the wire to the web view for no reason the UI
    // actually has.
    let (connected, set_connected) = signal(false);
    let (symbol, set_symbol) = signal(String::new());
    let (fetched, set_fetched) = signal(None::<FetchView>);

    // Refetched after every run, so a finding appears in the history the
    // moment it is recorded rather than only after a restart.
    let refresh_history = move || {
        spawn_local(async move {
            if let Ok(entries) =
                call_typed::<HistoryView>("list_history", JsValue::UNDEFINED).await
            {
                set_unreadable.set(entries.unreadable);
                set_history.set(entries.entries);
            }
        });
    };

    spawn_local(async move {
        match call_typed::<DataLibraryView>("list_instruments", JsValue::UNDEFINED).await {
            Ok(value) => set_library.set(Some(value)),
            Err(reason) => set_error.set(Some(reason)),
        }
    });
    spawn_local(async move {
        if let Ok(found) =
            call_typed::<Vec<StrategyView>>("list_strategies", JsValue::UNDEFINED).await
        {
            // Only when nothing was restored: a strategy remembered from the
            // last session must not be overwritten by whichever one happens
            // to come first in the engine's list.
            if chosen.get_untracked().is_empty() {
                if let Some(first) = found.first() {
                    set_chosen.set(first.name.clone());
                }
            }
            set_strategies.set(found);
        }
    });
    refresh_history();

    let refresh_library = move || {
        spawn_local(async move {
            if let Ok(value) =
                call_typed::<DataLibraryView>("list_instruments", JsValue::UNDEFINED).await
            {
                set_library.set(Some(value));
            }
        });
    };
    spawn_local(async move {
        if let Ok(held) = call_typed::<bool>("feed_connected", JsValue::UNDEFINED).await {
            set_connected.set(held);
        }
    });

    let connect = move |_| {
        // A slow command by design: it returns when the sign-in finishes in
        // the browser, or after five minutes. Saying so beats a spinner that
        // is indistinguishable from a hang for that long.
        set_running.set(Some("Waiting for the sign-in in your browser…".to_owned()));
        set_error.set(None);
        spawn_local(async move {
            match call_typed::<bool>("connect_feed", JsValue::UNDEFINED).await {
                Ok(held) => set_connected.set(held),
                Err(reason) => set_error.set(Some(reason)),
            }
            set_running.set(None);
        });
    };

    let disconnect = move |_| {
        spawn_local(async move {
            match call_typed::<bool>("disconnect_feed", JsValue::UNDEFINED).await {
                Ok(held) => set_connected.set(held),
                Err(reason) => set_error.set(Some(reason)),
            }
        });
    };

    let fetch = move |_| {
        let instrument = symbol.get_untracked().trim().to_uppercase();
        if instrument.is_empty() {
            set_error.set(Some("Name an instrument to fetch".to_owned()));
            return;
        }
        // The resolution the selected rule is defined at, so a session-anchored
        // strategy pulls the bars it can actually run on rather than daily ones
        // it will refuse.
        let interval = strategies.with_untracked(|found| {
            found
                .iter()
                .find(|plan| plan.name == chosen.get_untracked())
                .map_or_else(|| "1day".to_owned(), |plan| plan.interval.clone())
        });
        set_running.set(Some(format!("Fetching {instrument} at {interval}")));
        set_error.set(None);
        set_fetched.set(None);
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "instrument": instrument,
                "interval": interval,
            }))
            .unwrap_or(JsValue::UNDEFINED);
            match call_typed::<FetchView>("fetch_bars", args).await {
                Ok(report) => {
                    set_fetched.set(Some(report));
                    refresh_library();
                }
                Err(reason) => set_error.set(Some(reason)),
            }
            set_running.set(None);
        });
    };

    let run = move |instrument: String| {
        let strategy = chosen.get_untracked();
        // The engine's own count for this rule, not a constant: the grids
        // differ per strategy, and a spinner promising 11 backtests during a
        // 5-backtest run is worse than one that says nothing.
        let (label, backtests) = strategies.with_untracked(|found| {
            found
                .iter()
                .find(|plan| plan.name == strategy)
                .map_or_else(
                    || (strategy.clone(), 11),
                    |plan| (plan.label.clone(), plan.backtests),
                )
        });
        set_running.set(Some(format!(
            "Running {label} on {instrument}: {backtests} backtests"
        )));
        set_error.set(None);
        // No clearing of previous results: open tabs stay open, which is the
        // point of having them.
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "instrument": instrument,
                "strategy": strategy,
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

    let run_walk = move |instrument: String| {
        let strategy = chosen.get_untracked();
        let (label, backtests) = strategies.with_untracked(|found| {
            found
                .iter()
                .find(|plan| plan.name == strategy)
                .map_or_else(
                    || (strategy.clone(), 11),
                    |plan| (plan.label.clone(), plan.backtests),
                )
        });
        // A fold is a whole study. Saying so up front matters more here than
        // anywhere else in this view: a walk-forward is the slowest thing the
        // workbench runs, by roughly the number of folds.
        set_running.set(Some(format!(
            "Rolling {label} across {instrument}: {backtests} backtests per fold"
        )));
        set_error.set(None);
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "instrument": instrument,
                "strategy": strategy,
            }))
            .unwrap_or(JsValue::UNDEFINED);
            match call_typed::<WalkForwardView>("run_walk_forward", args).await {
                Ok(result) => {
                    let instrument = result.instrument.clone();
                    set_walks.update(|walks| {
                        walks.insert(instrument.clone(), result);
                    });
                    open_study_panel(
                        &format!("{WALK_PANEL_PREFIX}{instrument}"),
                        &format!("{instrument} rolling"),
                    );
                }
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

            // Which rule an instrument is tested against, chosen before it is
            // clicked. The premise line matters as much as the name: these are
            // four different claims about how prices behave, and a menu of
            // bare names invites picking one because it sounds impressive.
            <label class="research-strategy">
                <span>"Strategy"</span>
                <select
                    prop:value=move || chosen.get()
                    disabled=move || running.get().is_some()
                    on:change:target=move |ev| set_chosen.set(ev.target().value())
                >
                    {move || {
                        strategies
                            .get()
                            .into_iter()
                            .map(|plan| {
                                view! {
                                    <option value=plan.name.clone()>
                                        {plan.label.clone()}
                                    </option>
                                }
                            })
                            .collect_view()
                    }}
                </select>
            </label>
            {move || {
                strategies
                    .get()
                    .into_iter()
                    .find(|plan| plan.name == chosen.get())
                    .map(|plan| {
                        view! {
                            <p class="research-hint">
                                {plan.premise.clone()}
                                " Runs on "
                                <strong>{plan.interval.clone()}</strong>
                                " bars."
                            </p>
                        }
                    })
            }}

            // Fetching is a separate act from running, and the separation is
            // the point: an experiment pins its dataset as a content hash of
            // the bars it ran on, so a study that went to the network mid-run
            // would give a different answer whenever the vendor revised a bar.
            <div class="research-fetch">
                {move || {
                    if connected.get() {
                        view! {
                            <div>
                                <div class="research-fetch-row">
                                    <input
                                        type="text"
                                        placeholder="MSFT.NASDAQ"
                                        prop:value=move || symbol.get()
                                        on:input:target=move |ev| set_symbol.set(ev.target().value())
                                    />
                                    <button disabled=move || running.get().is_some() on:click=fetch>
                                        "Fetch"
                                    </button>
                                </div>
                                <button class="research-linkish" on:click=disconnect>
                                    "Disconnect Robinhood"
                                </button>
                            </div>
                        }
                            .into_any()
                    } else {
                        view! {
                            <div>
                                <p class="research-hint">
                                    "Sign in to Robinhood to pull bars. This opens your own \
                                     browser — never a window inside Arvo, so you can see whose \
                                     page you are typing a password into. Arvo keeps only the \
                                     resulting token, in the OS keychain, and uses it to read \
                                     price history."
                                </p>
                                <button
                                    class="research-panel-run"
                                    disabled=move || running.get().is_some()
                                    on:click=connect
                                >
                                    "Sign in to Robinhood"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                }}
                {move || {
                    fetched
                        .get()
                        .map(|report| {
                            // Interpolated bars are gap-fill the server
                            // synthesised. Saying how many were dropped is the
                            // difference between a series you can trust and one
                            // that is quietly part invention.
                            let invented = if report.interpolated > 0 {
                                format!(", {} invented bars dropped", report.interpolated)
                            } else {
                                String::new()
                            };
                            view! {
                                <p class="research-hint">
                                    {format!(
                                        "{}: {} {} bars{}",
                                        report.instrument,
                                        report.bars,
                                        report.interval,
                                        invented,
                                    )}
                                    {report
                                        .from
                                        .as_ref()
                                        .zip(report.to.as_ref())
                                        .map(|(from, to)| format!(" ({from} → {to})"))}
                                </p>
                            }
                        })
                }}
            </div>

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
                                    let id_rolling = instrument.id.clone();
                                    let held_walk = {
                                        let id = instrument.id.clone();
                                        move || walks.with(|walks| walks.contains_key(&id))
                                    };
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
                                            // The rolling run, beside the single split rather
                                            // than replacing it. They answer different
                                            // questions — did this configuration hold, versus
                                            // does choosing this way work — and a reader wants
                                            // both about the same rule.
                                            <button
                                                class="research-rerun"
                                                title=move || {
                                                    if held_walk() {
                                                        "Open the rolling result already held"
                                                    } else {
                                                        "Re-select on a rolling schedule"
                                                    }
                                                }
                                                disabled=move || running.get().is_some() || !runnable
                                                on:click={
                                                    let id = id_rolling.clone();
                                                    move |_| {
                                                        // Same reopen-rather-than-recompute rule
                                                        // as a study, and it matters far more
                                                        // here: a walk-forward is a study per
                                                        // fold.
                                                        if walks
                                                            .with_untracked(|walks| {
                                                                walks.contains_key(&id)
                                                            })
                                                        {
                                                            open_study_panel(
                                                                &format!("{WALK_PANEL_PREFIX}{id}"),
                                                                &format!("{id} rolling"),
                                                            );
                                                        } else {
                                                            run_walk(id.clone());
                                                        }
                                                    }
                                                }
                                            >
                                                "\u{21c9}"
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
                            // Said out loud. A store that quietly forgets is
                            // worse than one that admits it has: these are
                            // findings that exist on disk and cannot be read,
                            // and knowing that is the difference between
                            // "I never ran that" and "I ran it and lost it".
                            {move || {
                                let lost = unreadable.get();
                                (!lost.is_empty())
                                    .then(|| {
                                        view! {
                                            <p class="research-flag">
                                                {format!(
                                                    "{} finding{} could not be read",
                                                    lost.len(),
                                                    if lost.len() == 1 { "" } else { "s" },
                                                )}
                                            </p>
                                            <ul class="research-reasons">
                                                {lost
                                                    .into_iter()
                                                    .map(|item| {
                                                        view! { <li title=item.reason>{item.id}</li> }
                                                    })
                                                    .collect_view()}
                                            </ul>
                                        }
                                    })
                            }}
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
                                                    Ok(RecordView::WalkForward(walk)) => {
                                                        let instrument = walk.instrument.clone();
                                                        set_walks
                                                            .update(|walks| {
                                                                walks.insert(instrument.clone(), *walk);
                                                            });
                                                        open_study_panel(
                                                            &format!("{WALK_PANEL_PREFIX}{instrument}"),
                                                            &format!("{instrument} rolling"),
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




/// One study tab's content.
///
/// Reads its study back out of the shared map by key rather than capturing a
/// value, so re-running an instrument refreshes the tab that is already open
/// instead of leaving a stale report behind it.
#[component]
pub(crate) fn StudyTab(
    instrument: String,
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || match studies.get().get(&instrument).cloned() {
                Some(study) => view! { <StudyReport study=study /> }.into_any(),
                // A tab restored from a saved workspace, whose result lives in
                // memory and did not survive the restart. Said, rather than
                // left as an empty panel: a blank rectangle is the failure
                // shape this project has lost time to more than once, and
                // silently re-running a study on launch would spend a minute
                // of work nobody asked for.
                None => {
                    view! {
                        <NotLoaded subject=instrument.clone() />
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// A tab whose result is not in memory.
///
/// The durable copy is in research memory — every run is recorded — so the
/// honest instruction is to open it from History rather than to re-run it.
#[component]
fn NotLoaded(subject: String) -> impl IntoView {
    view! {
        <div class="study-panel-empty">
            <p class="sidebar-empty">{format!("{subject} is not loaded")}</p>
            <p class="research-hint">
                "This tab was restored from your last session. The finding itself is in \
                 research memory — open it from History in the Research sidebar, or run it \
                 again."
            </p>
        </div>
    }
}

/// One walk-forward tab's content.
#[component]
pub(crate) fn WalkTab(
    instrument: String,
    walks: ReadSignal<std::collections::HashMap<String, WalkForwardView>>,
) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || match walks.get().get(&instrument).cloned() {
                Some(walk) => view! { <WalkForwardReport walk=walk /> }.into_any(),
                None => {
                    view! { <NotLoaded subject=format!("{instrument} rolling") /> }.into_any()
                }
            }}
        </div>
    }
}
