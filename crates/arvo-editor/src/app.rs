//! The window: activity bar, sidebar, menus, palette, and the dockview
//! wiring that mounts everything else.
//!
//! What is left here after the split is the *shell* — the parts that would
//! look much the same if this were a different application. The views it
//! mounts live beside it:
//!
//! | module | what it owns |
//! |---|---|
//! | [`crate::views`] | every wire DTO, so the contract with `arvo-runtime` reads in one place |
//! | [`crate::bridge`] | the single `invoke` seam and the names dockview knows a panel by |
//! | [`crate::format`] | number and verdict formatting, shared so one figure never renders two ways |
//! | [`crate::chart`] | cards, equity curves, the monthly grid |
//! | [`crate::dashboard`] | the Welcome tab: what you hold, what is wrong, what was last run |
//! | [`crate::research`] | the studies, panels and walk-forwards — the application |
//! | [`crate::portfolio`] | what you hold |
//! | [`crate::theme`] | which palette the window is wearing |

use crate::bridge::{
    call, call_typed, capture_layout, close_window, init_shell, minimize_window, on_event,
    on_layout_settled, on_quote, restore_layout, set_output_visible_js, set_sidebar_visible,
    open_study_panel, toggle_maximize_window, COMPARE_PANEL_ID, PANEL_PANEL_ID,
    PORTFOLIO_PANEL_ID, STUDY_PANEL_PREFIX, WALK_PANEL_PREFIX, WATCHLIST_PANEL_ID,
};
use crate::dashboard::{problems, Dashboard, ProblemList};
use crate::portfolio::{PortfolioSidebar, PortfolioTab};
use crate::research::{ComparisonReport, PanelTab, ResearchView, StudyTab, WalkTab};
use crate::theme::{apply_theme, prefers_dark, Theme};
use crate::watchlist::WatchlistTab;
use crate::views::*;

use leptos::mount::mount_to;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

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
    View,
    Help,
}

/// Fixed chrome — not a dockview panel. Matches VS Code's own shape: the
/// activity bar itself isn't draggable/floatable, only the regions beside
/// it are.
/// Clicking the already-active icon collapses the sidebar, same as VS
/// Code — this is a toggle, not a plain select.
impl ActivityView {
    /// The spelling a stored session uses.
    const fn slug(self) -> &'static str {
        match self {
            Self::Portfolio => "portfolio",
            Self::Research => "research",
            Self::Extensions => "extensions",
            Self::Alerts => "alerts",
            Self::Settings => "settings",
        }
    }

    /// `None` for a view this build does not have — a session from a newer
    /// build should open with the sidebar closed, not fail to open.
    fn from_slug(slug: &str) -> Option<Self> {
        [
            Self::Portfolio,
            Self::Research,
            Self::Extensions,
            Self::Alerts,
            Self::Settings,
        ]
        .into_iter()
        .find(|view| view.slug() == slug)
    }
}

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

/// How many things are wrong *now*: the badge, and the status bar's count.
///
/// Current state, deliberately, not a tally of events. Two reasons, and the
/// running app demonstrated both:
///
/// * Something already broken when the window opened produces no event,
///   because nothing transitioned. A launch with an expired token, or a
///   plugin that was unreachable on the first probe, is invisible to an
///   event count — the status bar read "No alerts" beside a red `1/2
///   plugins` segment, which is the app disagreeing with itself.
/// * A problem that has since been fixed should stop counting. An expired
///   session you then signed back into is not an outstanding alert, and a
///   badge that only ever climbs is one people learn to ignore.
///
/// The alerts list still shows the whole history. That is what a list is
/// for; a badge is for what is outstanding.
fn attention(
    plugins: ReadSignal<Vec<PluginView>>,
    feed_held: ReadSignal<bool>,
) -> Memo<usize> {
    // The same list the dashboard and the alerts sidebar render, counted.
    // Counting here with a filter of its own is how a badge starts disagreeing
    // with the panel it points at.
    Memo::new(move |_| problems(&plugins.get(), feed_held.get()).len())
}

#[component]
fn ActivityBar(
    active_view: ReadSignal<Option<ActivityView>>,
    set_active_view: WriteSignal<Option<ActivityView>>,
    attention: Memo<usize>,
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
                // Only when there is something. A badge reading "0" is a
                // badge that has to be read before it can be ignored.
                {move || {
                    let count = attention.get();
                    (count > 0)
                        .then(|| view! { <span class="activity-badge">{count}</span> })
                }}
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

/// Extensions: what is installed, and whether it answers.
///
/// The "Theme Packs" heading and its permanent "none installed" line are
/// gone. There is no loader for them (app-shell ticket 03), so the section
/// advertised a capability the build does not have and could only ever read
/// empty. It comes back with the loader, not before.
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
        </div>
    }
}

/// What the backend has said unprompted, in the order it said it.
///
/// Was a hardcoded "No active alerts" — not because nothing ever happened,
/// but because nothing could reach the window to say so. Everything here
/// arrives on the push channel; see [`crate::bridge::on_event`].
///
/// Warnings first, then the rest. Not two lists: a session that expired and
/// the sign-in that fixed it are the same story and reading it out of order
/// is worse than reading it slightly ranked.
#[component]
fn AlertsView(
    plugins: ReadSignal<Vec<PluginView>>,
    feed_held: ReadSignal<bool>,
    set_feed_held: WriteSignal<bool>,
) -> impl IntoView {
    view! {
        <div class="sidebar-view">
            <h3>"Alerts"</h3>

            // What is wrong now, from state rather than from the event log: a
            // launch already signed out, or a plugin unreachable on the very
            // first probe, never transitioned and so never announced itself.
            //
            // Only that. The log of everything that has happened moved to the
            // Output panel, which had been an empty placeholder — these are
            // two different questions ("what should I do about it" and "what
            // has this thing been doing") and one list answered neither well.
            <ProblemList plugins=plugins feed_held=feed_held set_feed_held=set_feed_held />
        </div>
    }
}

/// The theme picker is Settings' one real setting — replaces the old
/// activity-bar sun/moon toggle, which only had room for two states and
/// stopped fitting once Catppuccin Mocha became a third option.
#[component]
fn SettingsView(
    theme: ReadSignal<Theme>,
    set_theme: WriteSignal<Theme>,
    workspaces: ReadSignal<Vec<WorkspaceView>>,
    set_workspaces: WriteSignal<Vec<WorkspaceView>>,
    /// Writes the session. Saving a workspace changes no panel, so nothing
    /// else would ever write it to disk.
    persist: Callback<()>,
) -> impl IntoView {
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

    let (name, set_name) = signal(String::new());

    // Saving is the only place a layout is deliberately frozen; everywhere
    // else it is whatever the window happens to look like. A blank name is
    // refused rather than saved as "" — an unnameable entry in the list is
    // one nobody can tell from another.
    let save = move |_| {
        let chosen = name.get_untracked().trim().to_owned();
        if chosen.is_empty() {
            return;
        }
        let Some(layout) = capture_layout() else {
            // dockview could not serialise. Nothing is saved rather than an
            // entry that restores to a blank window.
            web_sys::console::error_1(&"the current layout could not be captured".into());
            return;
        };
        set_workspaces.update(|saved| {
            // Same name replaces rather than duplicates: "save over it" is
            // what typing an existing name means everywhere else.
            match saved.iter_mut().find(|existing| existing.name == chosen) {
                Some(existing) => existing.layout = layout,
                None => saved.push(WorkspaceView { name: chosen, layout }),
            }
        });
        set_name.set(String::new());
        persist.run(());
    };

    view! {
        <div class="sidebar-view">
            <h3>"Settings"</h3>
            <h4>"Theme"</h4>
            <div class="theme-picker">
                {option(Theme::Light)} {option(Theme::Dark)} {option(Theme::CatppuccinMocha)}
            </div>

            <h4>"Workspaces"</h4>
            <p class="research-hint">
                "A workspace is the arrangement of tabs and panels, saved by name.                  The window always reopens where you left off; these are the                  arrangements you choose to keep."
            </p>
            <div class="workspace-save">
                <input
                    type="text"
                    placeholder="Name this arrangement"
                    prop:value=move || name.get()
                    on:input:target=move |ev| set_name.set(ev.target().value())
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            save(());
                        }
                    }
                />
                <button
                    disabled=move || name.get().trim().is_empty()
                    on:click=move |_| save(())
                >
                    "Save"
                </button>
            </div>

            {move || {
                let saved = workspaces.get();
                if saved.is_empty() {
                    return view! { <p class="sidebar-empty">"No saved workspaces"</p> }
                        .into_any();
                }
                view! {
                    <ul class="workspace-list">
                        {saved
                            .into_iter()
                            .map(|workspace| {
                                let layout = workspace.layout.clone();
                                let switch = move |_| {
                                    // A layout naming a panel this build no
                                    // longer has throws, and dockview is left
                                    // half-applied. Say so rather than leave
                                    // someone looking at a broken window with
                                    // no idea which click did it.
                                    if !restore_layout(&layout) {
                                        web_sys::console::error_1(
                                            &"that workspace could not be restored".into(),
                                        );
                                    }
                                };
                                let dropped = workspace.name.clone();
                                let remove = move |_| {
                                    set_workspaces
                                        .update(|saved| {
                                            saved.retain(|existing| existing.name != dropped);
                                        });
                                    persist.run(());
                                };
                                view! {
                                    <li class="workspace-row">
                                        <button class="workspace-open" on:click=switch>
                                            {workspace.name.clone()}
                                        </button>
                                        <button
                                            class="workspace-remove"
                                            title="Forget this workspace"
                                            on:click=remove
                                        >
                                            "×"
                                        </button>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
        </div>
    }
}

#[component]
fn SidebarPanel(
    active_view: ReadSignal<Option<ActivityView>>,
    feed_held: ReadSignal<bool>,
    set_feed_held: WriteSignal<bool>,
    plugins: ReadSignal<Vec<PluginView>>,
    set_plugins: WriteSignal<Vec<PluginView>>,
    theme: ReadSignal<Theme>,
    set_theme: WriteSignal<Theme>,
    workspaces: ReadSignal<Vec<WorkspaceView>>,
    set_workspaces: WriteSignal<Vec<WorkspaceView>>,
    persist: Callback<()>,
    studies: ReadSignal<std::collections::HashMap<String, StudyView>>,
    set_studies: WriteSignal<std::collections::HashMap<String, StudyView>>,
    walks: ReadSignal<std::collections::HashMap<String, WalkForwardView>>,
    set_walks: WriteSignal<std::collections::HashMap<String, WalkForwardView>>,
    chosen: ReadSignal<String>,
    set_chosen: WriteSignal<String>,
    panel: ReadSignal<Option<PanelView>>,
    set_panel: WriteSignal<Option<PanelView>>,
    set_comparison: WriteSignal<Option<ComparisonView>>,
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
                            connected=feed_held
                            set_connected=set_feed_held
                            studies=studies
                            set_studies=set_studies
                            walks=walks
                            set_walks=set_walks
                            chosen=chosen
                            set_chosen=set_chosen
                            panel=panel
                            set_panel=set_panel
                            set_comparison=set_comparison
                        />
                    }
                        .into_any()
                }
                Some(ActivityView::Extensions) => {
                    view! { <ExtensionsView plugins=plugins set_plugins=set_plugins /> }
                        .into_any()
                }
                Some(ActivityView::Alerts) => {
                    view! {
                        <AlertsView
                            plugins=plugins
                            feed_held=feed_held
                            set_feed_held=set_feed_held
                        />
                    }
                        .into_any()
                }
                Some(ActivityView::Settings) => {
                    view! {
                        <SettingsView
                            theme=theme
                            set_theme=set_theme
                            workspaces=workspaces
                            set_workspaces=set_workspaces
                            persist=persist
                        />
                    }
                        .into_any()
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
    /// Owned, not `&'static str`: the palette lists your own portfolios and
    /// workspaces by name, and those names are not known at compile time.
    title: String,
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
    portfolios: ReadSignal<Option<PortfolioLibraryView>>,
    set_open_portfolio: WriteSignal<Option<PortfolioView>>,
    workspaces: ReadSignal<Vec<WorkspaceView>>,
) -> Vec<Command> {
    let show = move |view: ActivityView| {
        Arc::new(move || set_active_view.set(Some(view))) as Arc<dyn Fn() + Send + Sync>
    };

    // Read, not captured: this whole function re-runs inside the palette's
    // reactive scope, so a portfolio imported or a workspace saved while the
    // app is open is in the list the next time it opens.
    let named: Vec<Command> = portfolios
        .get()
        .map(|library| library.portfolios)
        .unwrap_or_default()
        .into_iter()
        .map(|portfolio| {
            let title = portfolio.name.clone();
            Command {
                category: "Portfolio",
                title,
                // The same two calls the sidebar and the dashboard make, so
                // an already-open tab is focused rather than duplicated.
                run: Arc::new(move || {
                    set_open_portfolio.set(Some(portfolio.clone()));
                    open_study_panel(PORTFOLIO_PANEL_ID, "Portfolio");
                }),
            }
        })
        .chain(workspaces.get().into_iter().map(|workspace| Command {
            category: "Workspace",
            title: workspace.name.clone(),
            run: Arc::new(move || {
                if !restore_layout(&workspace.layout) {
                    web_sys::console::error_1(
                        &"that workspace could not be restored".into(),
                    );
                }
            }),
        }))
        .collect();

    let mut all = vec![
        Command {
            category: "View",
            title: "Toggle Sidebar".to_owned(),
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
            title: "Watchlist".to_owned(),
            run: Arc::new(|| open_study_panel(WATCHLIST_PANEL_ID, "Watchlist")),
        },
        Command {
            category: "View",
            title: "Toggle Output Panel".to_owned(),
            run: Arc::new(move || set_output_visible.set(!output_visible.get_untracked())),
        },
        Command {
            category: "View",
            title: "Research".to_owned(),
            run: show(ActivityView::Research),
        },
        Command {
            category: "View",
            title: "Extensions".to_owned(),
            run: show(ActivityView::Extensions),
        },
        Command {
            category: "View",
            title: "Alerts".to_owned(),
            run: show(ActivityView::Alerts),
        },
        Command {
            category: "Preferences",
            title: "Settings".to_owned(),
            run: show(ActivityView::Settings),
        },
        Command {
            category: "Theme",
            title: "Light".to_owned(),
            run: Arc::new(move || set_theme.set(Theme::Light)),
        },
        Command {
            category: "Theme",
            title: "Dark".to_owned(),
            run: Arc::new(move || set_theme.set(Theme::Dark)),
        },
        Command {
            category: "Theme",
            title: "Catppuccin Mocha".to_owned(),
            run: Arc::new(move || set_theme.set(Theme::CatppuccinMocha)),
        },
        Command {
            category: "Window",
            title: "Minimize".to_owned(),
            run: Arc::new(minimize_window),
        },
        Command {
            category: "Window",
            title: "Toggle Maximize".to_owned(),
            run: Arc::new(toggle_maximize_window),
        },
        Command {
            category: "Window",
            title: "Close".to_owned(),
            run: Arc::new(close_window),
        },
    ];

    // After the fixed ones. Scoring reorders anything the query actually
    // matches, so this only decides ties — and on an empty query, where every
    // command scores the same, "Toggle Sidebar" is a better first row than
    // whichever portfolio happens to sort first.
    all.extend(named);
    all
}

/// VS Code's command palette: a filter box over everything the shell can do.
///
/// Opens on Ctrl+Shift+P or F1, filters as you type, moves with the arrow
/// keys, runs on Enter, dismisses on Escape or a click outside.
#[component]
fn CommandPalette(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
    /// Called for every filter pass rather than taken once. The list is no
    /// longer fixed — it carries your portfolios and saved workspaces, and one
    /// captured at startup would be missing everything made since.
    commands: Arc<dyn Fn() -> Vec<Command> + Send + Sync>,
) -> impl IntoView {
    let (query, set_query) = signal(String::new());
    let (selected, set_selected) = signal(0_usize);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let matches = {
        let commands = commands.clone();
        move || {
            let query = query.get();
            let available = commands();
            let mut scored: Vec<(u32, Command)> = available
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

/// The row that says what state the app is in.
///
/// It used to render one string — a plugin count — with a comment admitting
/// it was there for visual consistency. The reason it could not say more was
/// structural: nothing pushed to the window, so there was no live state to
/// report. Now there is.
///
/// Every segment is a button. A status bar that only tells you a thing is
/// wrong, without being the place you go about it, makes you hunt for the
/// screen that can — which for the broker session meant finding a sign-in
/// button below a strategy picker in a sidebar that might be closed.
#[component]
fn StatusBar(
    plugins: ReadSignal<Vec<PluginView>>,
    attention: Memo<usize>,
    feed_held: ReadSignal<bool>,
    set_active_view: WriteSignal<Option<ActivityView>>,
) -> impl IntoView {
    // ponytail: the feed segment opens Alerts, where the sign-in button is,
    // rather than starting the sign-in itself. The connect flow has a
    // five-minute wait and its own error surface, and giving it a second
    // entry point means duplicating both. Call `connect_feed` from here when
    // that flow reports progress somewhere shared.
    let show = move |view: ActivityView| move |_| set_active_view.set(Some(view));

    view! {
        <div class="status-bar">
            <button
                class="status-segment"
                class:status-warn=move || !feed_held.get()
                title="Robinhood market data"
                on:click=show(ActivityView::Alerts)
            >
                {move || if feed_held.get() { "◆ Robinhood" } else { "◇ Signed out" }}
            </button>

            <button
                class="status-segment"
                class:status-warn=move || {
                    plugins
                        .get()
                        .iter()
                        .any(|p| matches!(p.status, PluginStatusView::Unreachable { .. }))
                }
                title="Plugin reachability"
                on:click=show(ActivityView::Extensions)
            >
                {move || {
                    let all = plugins.get();
                    let reachable = all
                        .iter()
                        .filter(|p| matches!(p.status, PluginStatusView::Reachable { .. }))
                        .count();
                    format!("{reachable}/{} plugins", all.len())
                }}
            </button>

            <div class="status-spacer" />

            <button
                class="status-segment"
                class:status-warn=move || { attention.get() > 0 }
                title="Alerts"
                on:click=show(ActivityView::Alerts)
            >
                {move || {
                    let count = attention.get();
                    if count == 0 {
                        "No alerts".to_owned()
                    } else {
                        format!("{count} alert{}", if count == 1 { "" } else { "s" })
                    }
                }}
            </button>
        </div>
    }
}

/// Everything the backend has said, newest first.
///
/// Was the string "Output" in an otherwise empty panel — a toggle in the View
/// menu that revealed a placeholder. It is the natural home for the event log
/// that the alerts sidebar was carrying: alerts answer "what should I do
/// about it", this answers "what has this been doing", and the second is a
/// transcript rather than a to-do list.
///
/// Chronological, not sorted by severity. A log reordered by importance is
/// one you cannot read a sequence of events out of, which is the only reason
/// to keep a log.
#[component]
fn BottomPanel(events: ReadSignal<Vec<EventView>>) -> impl IntoView {
    view! {
        <div class="output-panel">
            {move || {
                let log = events.get();
                if log.is_empty() {
                    return view! { <p class="sidebar-empty">"Nothing has happened yet"</p> }
                        .into_any();
                }
                view! {
                    <ul class="output-log">
                        {log
                            .into_iter()
                            .map(|event| {
                                let warning = event.severity == SeverityView::Warning;
                                view! {
                                    <li class="output-line" class:output-warning=warning>
                                        <span class="output-title">{event.title}</span>
                                        <span class="output-detail">{event.detail}</span>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
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
    let (walks, set_walks) = signal(std::collections::HashMap::<String, WalkForwardView>::new());
    // Up here rather than inside the research sidebar so the saved workspace
    // can hold it: which strategy is selected is part of where you left off.
    let (chosen, set_chosen) = signal(String::new());
    // One panel at a time: there is only one panel, and re-running it should
    // replace what the tab shows rather than accumulate tabs.
    let (panel, set_panel) = signal(None::<PanelView>);
    let (comparison, set_comparison) = signal(None::<ComparisonView>);
    let (portfolios, set_portfolios) = signal(None::<PortfolioLibraryView>);
    // Kept apart from the library itself. `None` there means "not loaded",
    // which the dashboard renders as "no holdings imported yet" — a claim
    // about what the person has done, and one they will have done. A failed
    // read must not be able to make it.
    let (portfolio_error, set_portfolio_error) = signal(None::<String>);
    let (open_portfolio, set_open_portfolio) = signal(None::<PortfolioView>);
    // Everything the backend has said unprompted, newest first. Held here
    // rather than in the alerts sidebar because the sidebar is unmounted
    // whenever it is closed, and an alert that only exists while you are
    // looking at it is not an alert.
    let (events, set_events) = signal(Vec::<EventView>::new());
    // Whether a broker session is held. Hoisted out of ResearchView for the
    // same reason `chosen` was — two owners of this became two answers the
    // moment the status bar wanted to show it, and the event stream can
    // change it from underneath both.
    let (feed_held, set_feed_held) = signal(false);
    let attention = attention(plugins, feed_held);
    // Saved arrangements. Held in the shell because the autosave below writes
    // the whole session on every layout change — a copy owned by the Settings
    // view would be absent from that write, and every panel drag would erase
    // every saved workspace.
    let (workspaces, set_workspaces) = signal(Vec::<WorkspaceView>::new());
    // Every symbol's latest price, keyed by ticker. Here rather than in the
    // watchlist panel because the panel is unmounted whenever its tab is
    // closed, and a listener installed per mount would leave one dead closure
    // per open writing into a signal nobody can see. The panel reads this;
    // the socket that fills it does not care whether anyone is looking.
    let quotes = RwSignal::new(std::collections::HashMap::<String, QuoteTick>::new());
    provide_context(quotes);

    // Live prices. Separate from the event channel below: a tick is not an
    // alert, is worth nothing once the next one lands, and must never reach
    // the capped alerts log.
    Effect::new(move |_| {
        on_quote(move |tick| {
            quotes.update(|latest| {
                latest.insert(tick.symbol.clone(), tick);
            });
        });
    });

    // The push channel. One listener for the window's lifetime.
    Effect::new(move |_| {
        on_event(move |event| {
            match event.kind {
                EventKindView::Feed { connected, .. } => set_feed_held.set(connected),
                // The event is a signal to go and re-read, not the new state
                // itself — `arvo_core::events` says so, and this is the half
                // that was missing. Without it the log announced a plugin had
                // come back while the badge, the status bar and the alerts
                // list all still read the snapshot from startup and called it
                // unreachable.
                EventKindView::Plugin { .. } => {
                    spawn_local(async move {
                        set_plugins.set(call("list_plugins").await);
                    });
                }
                // Nothing to re-read: the prices themselves arrive on their
                // own channel, and this only says whether they are still
                // arriving. It lands in the alerts log like everything else.
                EventKindView::Stream { .. } => {}
            }
            set_events.update(|log| {
                log.insert(0, event);
                // ponytail: a flat cap, not a ring buffer or a persisted log.
                // 200 events is more than anyone scrolls; swap in something
                // durable when alerts need to survive a restart.
                log.truncate(200);
            });
        });
    });

    Effect::new(move |_| {
        spawn_local(async move {
            // The cold-start answer. Events cover every change after this,
            // but a launch with an already-expired token has no event to
            // announce it — nothing transitioned, it was already dead.
            if let Ok(held) = call_typed::<bool>("feed_connected", JsValue::UNDEFINED).await {
                set_feed_held.set(held);
            }
        });
    });

    Effect::new(move |_| {
        spawn_local(async move {
            set_plugins.set(call("list_plugins").await);
        });
    });

    Effect::new(move |_| {
        spawn_local(async move {
            match call_typed::<PortfolioLibraryView>("list_portfolios", JsValue::UNDEFINED).await {
                Ok(library) => {
                    set_portfolios.set(Some(library));
                    set_portfolio_error.set(None);
                }
                Err(reason) => {
                    web_sys::console::error_1(&reason.clone().into());
                    set_portfolio_error.set(Some(reason));
                }
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

    // Writing the session, as one thing anything can ask for.
    //
    // It used to live inside the layout-settled callback and nowhere else,
    // which quietly made "the layout changed" the only reason the session was
    // ever written. Saving a workspace moves no panel, so it fired nothing:
    // the new workspace sat in memory looking saved and was gone on the next
    // launch. Anything that changes persisted state calls this.
    let persist = Callback::new(move |()| {
        let session = SessionView {
            layout: capture_layout(),
            active_view: active_view
                .get_untracked()
                .map(|view| view.slug().to_owned()),
            output_visible: output_visible.get_untracked(),
            theme: Some(theme.get_untracked().slug().to_owned()),
            strategy: Some(chosen.get_untracked()).filter(|name| !name.is_empty()),
            // Every field, every time: this writes the whole file, so
            // anything omitted here is deleted from disk.
            workspaces: workspaces.get_untracked(),
        };
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "session": session,
            }))
            .unwrap_or(JsValue::UNDEFINED);
            // Logged, not surfaced. A workspace that failed to save is worth
            // knowing about and is not worth interrupting anyone over —
            // nothing they were doing has been lost.
            if let Err(reason) = call_typed::<()>("save_session", args).await {
                web_sys::console::warn_1(&reason.into());
            }
        });
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
                                feed_held=feed_held
                                set_feed_held=set_feed_held
                                plugins=plugins
                                set_plugins=set_plugins
                                theme=theme
                                set_theme=set_theme
                                workspaces=workspaces
                                set_workspaces=set_workspaces
                                persist=persist
                                studies=studies
                                set_studies=set_studies
                                walks=walks
                                set_walks=set_walks
                                chosen=chosen
                                set_chosen=set_chosen
                                panel=panel
                                set_panel=set_panel
                                set_comparison=set_comparison
                                portfolios=portfolios
                                set_open_portfolio=set_open_portfolio
                            />
                        }
                    });
                    *sidebar_mount_created.borrow_mut() = Some(handle);
                }
                "main" => {
                    // The Welcome tab is the dashboard now. It was a wordmark
                    // and a tagline, which is the right default for an editor
                    // that knows nothing until you open a folder and the
                    // wrong one here, where the app already knows what you
                    // hold and what it cannot reach.
                    mount_to(
                            el,
                            move || {
                                view! {
                                    <Dashboard
                                        portfolios=portfolios
                                        portfolio_error=portfolio_error
                                        set_open_portfolio=set_open_portfolio
                                        plugins=plugins
                                        feed_held=feed_held
                                        set_feed_held=set_feed_held
                                    />
                                }
                            },
                        )
                        .forget();
                }
                "bottom" => {
                    let handle = mount_to(el, move || view! { <BottomPanel events=events /> });
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
                WATCHLIST_PANEL_ID => {
                    let handle = mount_to(el, move || {
                        view! { <WatchlistTab connected=feed_held /> }.into_any()
                    });
                    study_mounts_created
                        .borrow_mut()
                        .insert(WATCHLIST_PANEL_ID.to_owned(), handle);
                }
                COMPARE_PANEL_ID => {
                    let handle = mount_to(el, move || {
                        view! {
                            <div class="study-panel">
                                {move || {
                                    comparison
                                        .get()
                                        .map(|comparison| {
                                            view! {
                                                <ComparisonReport comparison=comparison />
                                            }
                                        })
                                }}
                            </div>
                        }
                            .into_any()
                    });
                    study_mounts_created
                        .borrow_mut()
                        .insert(COMPARE_PANEL_ID.to_owned(), handle);
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
                id if id.starts_with(WALK_PANEL_PREFIX) => {
                    let instrument = id[WALK_PANEL_PREFIX.len()..].to_owned();
                    let handle = mount_to(el, move || {
                        view! { <WalkTab instrument=instrument walks=walks /> }.into_any()
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
                    || id == COMPARE_PANEL_ID
                    || id == PORTFOLIO_PANEL_ID
                    || id == WATCHLIST_PANEL_ID
                    || id.starts_with(STUDY_PANEL_PREFIX)
                    || id.starts_with(WALK_PANEL_PREFIX) =>
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

        // The workspace, put back. After `init_shell`, because restoring
        // drives the same panel-created callback a fresh layout does and
        // there has to be something listening when it fires.
        spawn_local(async move {
            let session = call_typed::<SessionView>("load_session", JsValue::UNDEFINED)
                .await
                .unwrap_or_default();

            if let Some(theme) = session.theme.as_deref().and_then(Theme::from_slug) {
                set_theme.set(theme);
            }
            if let Some(strategy) = session.strategy {
                set_chosen.set(strategy);
            }
            set_workspaces.set(session.workspaces);
            // Set before the layout goes back: the sidebar's own panel is part
            // of that layout, and an active view that disagreed with it would
            // show a sidebar with nothing in it.
            set_active_view.set(
                session
                    .active_view
                    .as_deref()
                    .and_then(ActivityView::from_slug),
            );
            set_output_visible.set(session.output_visible);

            if let Some(layout) = session.layout {
                // A layout that will not restore is discarded rather than
                // fought with, and the JS side puts the default arrangement
                // back rather than leaving whatever half-applied state the
                // failure produced. Opening to a window with no main panel is
                // the failure a workspace file must never cause — and did.
                restore_layout(&layout);
            }

            // Only now: restoring is itself a layout change, and saving
            // during it would race the thing that produced it.
            let record = Closure::<dyn FnMut()>::new(move || persist.run(()));
            on_layout_settled(record.as_ref());
            record.forget();
        });
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
                commands=Arc::new(move || {
                    commands(
                        set_active_view,
                        output_visible,
                        set_output_visible,
                        set_theme,
                        portfolios,
                        set_open_portfolio,
                        workspaces,
                    )
                })
            />
            <div class="shell">
                <ActivityBar
                active_view=active_view
                set_active_view=set_active_view
                attention=attention
            />
                <div
                    id="shell-host"
                    class="shell-host"
                    class:sidebar-open=move || active_view.get().is_some()
                ></div>
            </div>
            <StatusBar
                plugins=plugins
                attention=attention
                feed_held=feed_held
                set_active_view=set_active_view
            />
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
