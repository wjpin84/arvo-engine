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
//! | [`crate::research`] | the studies, panels and walk-forwards — the application |
//! | [`crate::portfolio`] | what you hold |
//! | [`crate::theme`] | which palette the window is wearing |

use crate::bridge::{
    call, call_typed, close_window, init_shell, minimize_window, set_output_visible_js,
    set_sidebar_visible, toggle_maximize_window, PANEL_PANEL_ID, PORTFOLIO_PANEL_ID,
    STUDY_PANEL_PREFIX, WALK_PANEL_PREFIX,
};
use crate::portfolio::{PortfolioSidebar, PortfolioTab};
use crate::research::{PanelTab, ResearchView, StudyTab, WalkTab};
use crate::theme::{apply_theme, prefers_dark, Theme};
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
    File,
    View,
    Help,
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
    walks: ReadSignal<std::collections::HashMap<String, WalkForwardView>>,
    set_walks: WriteSignal<std::collections::HashMap<String, WalkForwardView>>,
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
                            walks=walks
                            set_walks=set_walks
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
    let (walks, set_walks) = signal(std::collections::HashMap::<String, WalkForwardView>::new());
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
                                walks=walks
                                set_walks=set_walks
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
                    || id == PORTFOLIO_PANEL_ID
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
