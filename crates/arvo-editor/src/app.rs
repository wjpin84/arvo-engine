use leptos::mount::mount_to;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;

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

async fn call(cmd: &str) -> Vec<PluginView> {
    let result = invoke(cmd, JsValue::UNDEFINED).await;
    serde_wasm_bindgen::from_value(result).unwrap_or_default()
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
) -> impl IntoView {
    view! {
        // Needs a real height: .sidebar-view inside it is `height: 100%`,
        // which resolves against nothing if this wrapper is auto-height —
        // so an overflowing sidebar was being clipped by the panel's own
        // `overflow: hidden` instead of scrolling (ticket 14).
        <div class="sidebar-panel-root">
            {move || match active_view.get() {
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
                </MenuDropdown>
                <MenuDropdown id=MenuId::Help label="Help" open_menu=open_menu set_open_menu=set_open_menu>
                    <div class="menu-item-static">"Arvo Desktop"</div>
                </MenuDropdown>
            </nav>
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
    let (theme, set_theme) = signal(if prefers_dark() { Theme::Dark } else { Theme::Light });
    // Output moved out of the default layout into the View menu.
    let (output_visible, set_output_visible) = signal(false);

    Effect::new(move |_| {
        spawn_local(async move {
            set_plugins.set(call("list_plugins").await);
        });
    });

    Effect::new(move |_| apply_theme(theme.get()));

    // Sidebar and Output's mount handles are tracked (not `.forget()`-ten,
    // unlike the permanent main panel) so removing either actually unmounts
    // the Leptos root instead of leaking it — see the extern block's notes.
    // Output needs the same tracking as the sidebar now that ticket 12
    // makes it toggle on and off too, instead of being a permanent panel.
    let sidebar_mount = Rc::new(RefCell::new(None));
    let bottom_mount = Rc::new(RefCell::new(None));

    // Runs once on mount; dispatches each dockview panel to its Leptos
    // component as dockview creates it. See ticket 01's Answer for why
    // the element reference (not an id) is what makes this reliable.
    Effect::new(move |_| {
        let sidebar_mount_created = sidebar_mount.clone();
        let sidebar_mount_removed = sidebar_mount.clone();
        let bottom_mount_created = bottom_mount.clone();
        let bottom_mount_removed = bottom_mount.clone();

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
                _ => {}
            }
        });

        init_shell("shell-host", on_panel_created.as_ref(), on_panel_removed.as_ref());
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
            <TopMenuBar output_visible=output_visible set_output_visible=set_output_visible />
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
