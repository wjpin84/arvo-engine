mod app;
mod bridge;
mod chart;
mod portfolio;
mod research;
mod format;
mod theme;
mod trades;
mod views;

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
