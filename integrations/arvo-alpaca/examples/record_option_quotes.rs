//! Records SPY option quotes every fifteen minutes through the regular session,
//! without the window open (#83). Read-only market data; no orders.
//!
//! ```text
//! cargo run -p arvo-alpaca --example record_option_quotes -- [dir] [--once | --until-close]
//! ```
//!
//! `dir` defaults to the window's own folder, so both write one record.
//! `--until-close` records through today's session and exits after its close,
//! which is what a daily scheduled task wants; without it the recorder runs
//! until stopped.
//!
//! ponytail: no exchange calendar, so on a market holiday it records the last
//! session's quotes again — each row's `quote_at` shows they are stale.

use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let once = args.iter().any(|arg| arg == "--once");
    let until_close = args.iter().any(|arg| arg == "--until-close");
    let dir = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(
                std::env::var("APPDATA").expect("APPDATA, or pass a directory"),
            )
            .join("com.arvo.desktop")
            .join("option-quotes")
        });

    loop {
        let now = chrono::Utc::now();
        if once || arvo_data::session::in_regular_session(now.naive_utc()) {
            match arvo_alpaca::options::record_chain("SPY", &dir, now).await {
                Ok(recorded) => println!(
                    "{now} {} contracts -> {}",
                    recorded.contracts,
                    recorded.path.display()
                ),
                Err(err) => eprintln!("{now} {err}"),
            }
        }
        if once {
            return;
        }
        if until_close && now.naive_utc() >= arvo_data::session::regular_close(now.date_naive()) {
            println!("{now} the session has closed");
            return;
        }
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
    }
}
