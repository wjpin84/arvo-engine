//! The one seam between this window and the Rust behind it.
//!
//! Every backend call goes through here, which is the point: the `invoke`
//! binding is easy to get subtly wrong, and it was — the extern originally
//! lacked `catch`, so a Tauri `Err` rejected the promise, wasm-bindgen
//! rethrew, and the calling future was simply abandoned. The spinner stayed
//! up forever and the reason the backend gave was discarded. Having exactly
//! one place that can make that mistake is worth a module.

use wasm_bindgen::prelude::*;

use crate::views::{EventView, PluginView, QuoteTick, EVENT_CHANNEL, QUOTE_CHANNEL};

#[wasm_bindgen]
extern "C" {
    // `catch` is load-bearing. A Tauri command returning `Err` rejects the
    // promise, and without it wasm-bindgen rethrows into the wasm boundary and
    // abandons the calling future — so a failed command left the UI spinning
    // forever with the reason thrown away. With it the rejection is a value we
    // can read and show.
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"])]
    pub(crate) async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    // The other direction. Reached directly rather than through glue in
    // index.html because there is nothing to glue — `withGlobalTauri` already
    // exposes it, and `core:default` already permits it. Returns a promise
    // for an unlisten function, which nothing here wants: the listener is
    // installed once for the life of the window.
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    async fn listen_raw(event: &str, handler: &JsValue) -> Result<JsValue, JsValue>;

    // app-shell ticket 04 — glue defined in index.html. on_panel_created
    // is a JS-callable closure invoked with (panel_name, element) at the
    // exact moment dockview creates each panel's element — see ticket 01's
    // Answer for why the element reference itself is what's passed, not
    // an id to re-query later. on_panel_removed fires from dockview's own
    // panel dispose() hook, so Rust can unmount the matching Leptos root
    // instead of leaking it when a panel is actually removed (not just
    // hidden).
    #[wasm_bindgen(js_namespace = window, js_name = initShell)]
    pub(crate) fn init_shell(host_id: &str, on_panel_created: &JsValue, on_panel_removed: &JsValue);

    // The layout, as dockview encodes it. Opaque on this side deliberately:
    // treating it as data is what stops a dockview upgrade from becoming a
    // Rust change.
    #[wasm_bindgen(js_namespace = window, js_name = captureLayout)]
    pub(crate) fn capture_layout() -> Option<String>;
    // Returns whether it worked. A layout naming a panel this build no longer
    // has throws, and the caller has to fall back to the default arrangement
    // rather than open to a blank window.
    #[wasm_bindgen(js_namespace = window, js_name = restoreLayout)]
    pub(crate) fn restore_layout(layout: &str) -> bool;
    // Debounced in JS, where the events are: a file write per animation frame
    // is the obvious way to make dragging a panel stutter.
    #[wasm_bindgen(js_namespace = window, js_name = onLayoutSettled)]
    pub(crate) fn on_layout_settled(callback: &JsValue);

    // Real width-collapse — add/remove the dockview panel, not just clear
    // its content. See ActivityBar's toggle_view.
    #[wasm_bindgen(js_namespace = window, js_name = setSidebarVisible)]
    pub(crate) fn set_sidebar_visible(visible: bool);

    // Opens (or focuses) a tab for one study, beside Welcome in the main
    // group. Creating the panel synchronously drives `on_panel_created`, so
    // the study must already be in the map before this is called.
    #[wasm_bindgen(js_namespace = window, js_name = openStudyPanel)]
    pub(crate) fn open_study_panel(id: &str, title: &str);

    // Draws both equity curves into an element. Defined in index.html against
    // the vendored charting library, so the chart's palette can be read from
    // the same CSS variables everything else uses.
    #[wasm_bindgen(js_namespace = window, js_name = renderEquityChart)]
    pub(crate) fn render_equity_chart(el: &web_sys::HtmlElement, strategy: JsValue, benchmark: JsValue);

    // The instrument's own bars with trades marked on them, and the
    // underwater plot beneath. Same vendored library as the equity chart,
    // same reading of the app's CSS variables for its palette.
    #[wasm_bindgen(js_namespace = window, js_name = renderPriceChart)]
    pub(crate) fn render_price_chart(el: &web_sys::HtmlElement, candles: JsValue, markers: JsValue);
    #[wasm_bindgen(js_namespace = window, js_name = renderCurves)]
    pub(crate) fn render_curves(el: &web_sys::HtmlElement, series: JsValue);
    #[wasm_bindgen(js_namespace = window, js_name = renderUnderwaterChart)]
    pub(crate) fn render_underwater_chart(el: &web_sys::HtmlElement, points: JsValue);

    // app-shell ticket 12 — Output moved into the View menu; same
    // add/remove-panel toggle as the sidebar's.
    #[wasm_bindgen(js_namespace = window, js_name = setOutputVisible)]
    pub(crate) fn set_output_visible_js(visible: bool);

    // Borderless window (tauri.conf.json's `decorations: false`) — these
    // call the global Tauri window API directly, no new Tauri command
    // needed since `withGlobalTauri` already exposes it.
    #[wasm_bindgen(js_namespace = window, js_name = minimizeWindow)]
    pub(crate) fn minimize_window();
    #[wasm_bindgen(js_namespace = window, js_name = toggleMaximizeWindow)]
    pub(crate) fn toggle_maximize_window();
    #[wasm_bindgen(js_namespace = window, js_name = closeWindow)]
    pub(crate) fn close_window();
}

/// Invokes a command and decodes its reply, logging rather than swallowing a
/// decode failure.
///
/// `None` means the call or the decode failed — distinct from a successful
/// call returning something empty. Those two used to be indistinguishable
/// once rendered, with nothing logged anywhere.
pub(crate) async fn call_typed<T: serde::de::DeserializeOwned>(cmd: &str, args: JsValue) -> Result<T, String> {
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
pub(crate) async fn call(cmd: &str) -> Vec<PluginView> {
    match call_typed(cmd, JsValue::UNDEFINED).await {
        Ok(value) => value,
        Err(reason) => {
            web_sys::console::error_1(&reason.clone().into());
            Vec::new()
        }
    }
}

/// Runs `sink` for every event the backend pushes.
///
/// The counterpart to [`invoke`]: that one asks a question, this one hears
/// what happened without asking. Installed once from `App` — a second caller
/// would get its own listener, which is not wrong but is not what anyone
/// means.
///
/// A payload this build cannot read is logged and dropped rather than
/// panicking across the wasm boundary. The event stream is how the window
/// learns something went wrong; killing the window when one message is
/// malformed would be the worst possible failure mode for it.
pub(crate) fn on_event(sink: impl FnMut(EventView) + 'static) {
    on_channel(EVENT_CHANNEL, sink);
}

/// Runs `sink` for every live price the backend pushes.
///
/// Its own channel rather than more events: ticks arrive several times a
/// second, and every event is a candidate for an OS notification.
pub(crate) fn on_quote(sink: impl FnMut(QuoteTick) + 'static) {
    on_channel(QUOTE_CHANNEL, sink);
}

/// Runs `sink` for everything that arrives on one Tauri channel.
///
/// The counterpart to [`invoke`]: that one asks a question, this one hears
/// what happened without asking. One listener per channel for the life of the
/// window — a second caller would get its own, which is not wrong but is not
/// what anyone means.
///
/// A payload this build cannot read is logged and dropped rather than
/// panicking across the wasm boundary. The push channels are how the window
/// learns something went wrong; killing the window when one message is
/// malformed would be the worst possible failure mode for it.
fn on_channel<T>(channel: &'static str, mut sink: impl FnMut(T) + 'static)
where
    T: serde::de::DeserializeOwned,
{
    let handler = Closure::<dyn FnMut(JsValue)>::new(move |message: JsValue| {
        // Tauri wraps the payload: `{ event, id, payload }`.
        let payload = match js_sys::Reflect::get(&message, &JsValue::from_str("payload")) {
            Ok(payload) => payload,
            Err(_) => {
                web_sys::console::error_1(&"an event arrived with no payload".into());
                return;
            }
        };
        match serde_wasm_bindgen::from_value::<T>(payload) {
            Ok(value) => sink(value),
            Err(err) => {
                web_sys::console::error_1(&format!("unreadable {channel} message: {err}").into());
            }
        }
    });

    // Detached: the promise resolves to an unlisten function nobody calls,
    // and the closure has to outlive this call by the life of the window.
    let handler = handler.into_js_value();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(err) = listen_raw(channel, &handler).await {
            web_sys::console::error_1(
                &format!("could not subscribe to {channel}: {err:?}").into(),
            );
        }
    });
}

/// How a panel is addressed.
///
/// Beside [`open_study_panel`] rather than with the components, because these
/// are the *names* dockview knows a panel by and both the shell and the views
/// have to agree on them. Two modules each defining "study:" would compile,
/// and would silently stop reopening tabs the moment one changed.
/// The panel's own tab. One at a time — there is only one panel.
pub(crate) const PANEL_PANEL_ID: &str = "panel";

/// The portfolio overview's tab.
pub(crate) const PORTFOLIO_PANEL_ID: &str = "portfolio";

/// Dockview panel ids for study tabs are this plus the instrument.
pub(crate) const STUDY_PANEL_PREFIX: &str = "study:";

/// And for walk-forward tabs. A separate prefix so an instrument can have both
/// open at once — they answer different questions about the same rule, and
/// reading them side by side is the point.
pub(crate) const WALK_PANEL_PREFIX: &str = "walk:";

/// The watchlist's own tab. One at a time — it is one live view of one set
/// of instruments, and a second copy would be the same table polling twice.
pub(crate) const WATCHLIST_PANEL_ID: &str = "watchlist";

/// The comparison's own tab. One at a time: a second comparison replaces the
/// first, because two of them side by side is a comparison of comparisons and
/// nobody asked for that.
pub(crate) const COMPARE_PANEL_ID: &str = "compare";
