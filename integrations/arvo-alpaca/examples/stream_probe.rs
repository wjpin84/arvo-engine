//! Opens Alpaca's bar stream for one symbol and prints what arrives (#185).
//!
//!     cargo run -p arvo-alpaca --example stream_probe -- AAPL 90
//!
//! Needs an Alpaca key pair: the keychain's, or `APCA_API_KEY_ID` and
//! `APCA_API_SECRET_KEY`. Outside market hours this prints `up` and then
//! nothing, which is the stream working: Alpaca sends no bars for a closed
//! market. During the session a bar arrives a moment after each minute.

use arvo_data::source::{FeedEvent, Source as _};
use arvo_data::{BarInterval, IntervalUnit};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let symbol = args.next().unwrap_or_else(|| "AAPL".to_owned());
    let seconds: u64 = args.next().and_then(|text| text.parse().ok()).unwrap_or(60);

    let Some(mut feed) = arvo_alpaca::Alpaca::iex().stream(&symbol, BarInterval::new(1, IntervalUnit::Minute)) else {
        eprintln!("no stream: no Alpaca keys, or not a one-minute interval");
        std::process::exit(1);
    };
    let started = std::time::Instant::now();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = feed.next() => match event {
                Some(FeedEvent::Up) => println!("{:>6.1}s  up", started.elapsed().as_secs_f64()),
                Some(FeedEvent::Down(why)) => println!("{:>6.1}s  down: {why}", started.elapsed().as_secs_f64()),
                Some(FeedEvent::Bar(bar)) => println!(
                    "{:>6.1}s  bar {} o {} h {} l {} c {} v {}  (received {:.1}s after its close)",
                    started.elapsed().as_secs_f64(), bar.at, bar.open, bar.high, bar.low, bar.close, bar.volume,
                    (chrono::Utc::now().naive_utc() - (bar.at + chrono::Duration::minutes(1))).num_milliseconds() as f64 / 1000.0
                ),
                None => { println!("feed ended"); return; }
            },
            () = &mut deadline => { println!("done after {seconds}s"); return; }
        }
    }
}
