mod app;
mod bridge;
mod chart;
mod dashboard;
mod portfolio;
mod research;
mod format;
mod theme;
mod trades;
mod views;
mod watchlist;

use app::*;
use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| {
        view! {
            <App/>
        }
    })
}
