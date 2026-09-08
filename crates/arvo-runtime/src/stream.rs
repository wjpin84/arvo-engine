//! A live price stream, held open for the life of the app.
//!
//! # Why this is a task and not a plugin
//!
//! The roadmap's plugin model is a separate process reached over gRPC, and
//! arvo-desktop deliberately does not own a plugin's lifecycle — the registry
//! goes `Registered -> Reachable -> Unreachable` and has no Starting or
//! Stopping, because it does not run the process. A price feed shipped that
//! way would be a program the user has to remember to start before any number
//! moves, and "Plugin process spawning / sidecar packaging" is explicitly
//! deferred.
//!
//! What is actually needed here is one socket, held open, decoded, and pushed
//! at the window. Relaying that through a second process would be decoding
//! protobuf in order to re-encode protobuf. If it ever does need to be a
//! plugin — a source needing credentials the desktop should not hold, say —
//! the seam is [`Stream`], and moving it out is mechanical.
//!
//! # Why Yahoo, and what that costs
//!
//! It is the one source that streams without an API key, which makes it the
//! right thing to start with rather than the right thing to end with. The
//! protocol is not documented by Yahoo; it is what `yfinance` speaks, and it
//! can change without notice. Nothing downstream is allowed to depend on it:
//! ticks are display-only.
//!
//! # The rule this must not break
//!
//! **A tick never becomes a bar.** The data library is fetched files with a
//! content hash, and that hash is what makes a stored verdict checkable. A
//! price that arrived over a socket has no place in it, and nothing here can
//! write there — see [`crate::feed`] for the full argument.
//!
//! ponytail: no tick history, no candle aggregation, no reconnect jitter.
//! Add them when something other than a watchlist row reads this.

use std::time::Duration;

use arvo_views::{QuoteTick, QUOTE_CHANNEL};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use crate::events;

const ENDPOINT: &str = "wss://streamer.finance.yahoo.com/?version=2";

/// How often the subscription is re-sent.
///
/// Not a ping. The server drops a subscription it has not heard restated, so
/// this is the subscription itself repeated — the same 15 seconds `yfinance`
/// uses, and the reason an open socket can still go quiet.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// How long to wait before reconnecting.
const BACKOFF: Duration = Duration::from_secs(3);

/// Yahoo's `market_hours` value for the regular session.
const REGULAR_MARKET: i32 = 1;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The fields of Yahoo's `PricingData` this reads.
///
/// Declared with prost's derive rather than compiled from a `.proto`: the
/// tags are the contract, the names are ours, and proto3 ignores fields it
/// was not told about — so four fields here decode a message carrying
/// thirty-three, with no build script, no codegen step and no vendored
/// `protoc`. The tags match `yfinance/pricing.proto`; a wrong one silently
/// mis-decodes, so they are the first thing to check if prices look wrong.
#[derive(Clone, PartialEq, prost::Message)]
struct PricingData {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(float, tag = "2")]
    price: f32,
    #[prost(int32, tag = "7")]
    market_hours: i32,
    /// A percentage, not a fraction — divided on the way out.
    #[prost(float, tag = "8")]
    change_percent: f32,
}

#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error("the price socket failed: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("the price socket closed")]
    Closed,
}

/// Whether the window currently believes prices are flowing.
///
/// Two flags rather than one, because "the socket is up" and "prices are
/// arriving" are different facts and only the second is what the window is
/// being told about. A connection that opens and immediately closes is up
/// repeatedly and delivers nothing; the heartbeat above exists because an open
/// socket can go quiet, which is the same observation from the other side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Health {
    /// The window has been told prices stopped and not yet told otherwise.
    told_stalled: bool,
    /// A price has arrived at some point. Until one has, a failing socket is
    /// a log line rather than something to interrupt anyone for: nothing on
    /// screen has gone stale because nothing was ever live.
    ever_delivered: bool,
}

impl Health {
    /// A price arrived. Returns whether the window should be told prices are
    /// back.
    ///
    /// Announced here rather than on connect, which is the whole point. A
    /// socket that opens and dies without delivering would otherwise raise
    /// "resumed" on every attempt and "interrupted" on none of them, because
    /// the stall is gated on having delivered something and this session never
    /// did. The window would keep its last message — "resumed" — and sit
    /// on frozen prices indefinitely.
    const fn on_tick(&mut self) -> bool {
        self.ever_delivered = true;
        let announce = self.told_stalled;
        self.told_stalled = false;
        announce
    }

    /// The socket failed. Returns whether the window should be told.
    const fn on_drop(&mut self) -> bool {
        if self.ever_delivered && !self.told_stalled {
            self.told_stalled = true;
            true
        } else {
            false
        }
    }
}

/// The handle the rest of the app holds.
///
/// One method, because there is one thing to say: this is the set of symbols
/// worth streaming now. Not `subscribe`/`unsubscribe` — two calls that have
/// to agree about a set held somewhere else is how a watchlist ends up
/// streaming a symbol nobody is looking at.
pub struct Stream {
    symbols: watch::Sender<Vec<String>>,
}

impl Stream {
    /// Streams exactly these symbols — bare tickers — and nothing else.
    ///
    /// Idempotent: restating the current set does not disturb the socket.
    pub fn watch(&self, symbols: Vec<String>) {
        self.symbols.send_if_modified(|current| {
            if *current == symbols {
                false
            } else {
                *current = symbols;
                true
            }
        });
    }
}

/// Starts the stream. It stays down until something asks for a symbol.
pub fn start(app: AppHandle) -> Stream {
    let (symbols, receiver) = watch::channel(Vec::new());
    tauri::async_runtime::spawn(run(app, receiver));
    Stream { symbols }
}

/// Connect, serve, reconnect, forever.
async fn run(app: AppHandle, mut symbols: watch::Receiver<Vec<String>>) {
    let mut health = Health::default();

    loop {
        // No symbols yet: wait to be told, rather than holding a socket open
        // subscribed to nothing. The borrow is released before the await —
        // holding a watch guard across one is not allowed in a spawned task.
        let idle = symbols.borrow_and_update().is_empty();
        if idle {
            if symbols.changed().await.is_err() {
                return;
            }
            continue;
        }

        match session(&app, &mut symbols, &mut health).await {
            // The sender went away: the app is shutting down.
            Ok(()) => return,
            Err(err) => {
                tracing::warn!(error = %err, "price stream dropped; reconnecting");
                if health.on_drop() {
                    events::emit(&app, events::stream_stalled(&err.to_string()));
                }
                tokio::time::sleep(BACKOFF).await;
            }
        }
    }
}

/// One connection, from open to failure.
async fn session(
    app: &AppHandle,
    symbols: &mut watch::Receiver<Vec<String>>,
    health: &mut Health,
) -> Result<(), StreamError> {
    let (mut socket, _) = tokio_tungstenite::connect_async(ENDPOINT).await?;

    let wanted = symbols.borrow_and_update().clone();
    subscribe(&mut socket, &wanted).await?;

    let mut beat = tokio::time::interval(HEARTBEAT);
    // A tokio interval fires its first tick immediately, and the subscription
    // has just been sent.
    beat.tick().await;

    loop {
        tokio::select! {
            changed = symbols.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let wanted = symbols.borrow_and_update().clone();
                subscribe(&mut socket, &wanted).await?;
            }
            _ = beat.tick() => {
                let wanted = symbols.borrow().clone();
                subscribe(&mut socket, &wanted).await?;
            }
            message = socket.next() => {
                let message = message.ok_or(StreamError::Closed)??;
                if let Some(tick) = tick_of(&message) {
                    if health.on_tick() {
                        events::emit(app, events::stream_live());
                    }
                    // Not fatal: a window that has not finished loading is no
                    // reason to tear down a working socket.
                    if let Err(err) = app.emit(QUOTE_CHANNEL, &tick) {
                        tracing::warn!(error = %err, "could not push a tick to the window");
                    }
                }
            }
        }
    }
}

/// Sends the whole subscription, every time.
///
/// The full set rather than a delta: the server forgets, the heartbeat has to
/// restate it anyway, and one code path that always sends the truth cannot
/// drift from a set held somewhere else.
async fn subscribe(socket: &mut Socket, symbols: &[String]) -> Result<(), StreamError> {
    let request = serde_json::json!({ "subscribe": symbols }).to_string();
    socket.send(Message::text(request)).await?;
    Ok(())
}

/// A frame, if it carries a price.
///
/// Every failure here returns `None` rather than an error. One malformed
/// frame is not a reason to drop a socket that is otherwise delivering prices
/// for nine other symbols, and this protocol is undocumented enough that
/// unreadable frames should be assumed.
fn tick_of(message: &Message) -> Option<QuoteTick> {
    let Message::Text(text) = message else {
        return None;
    };
    let envelope: serde_json::Value = serde_json::from_str(text.as_str()).ok()?;
    let encoded = envelope.get("message")?.as_str()?;
    let bytes = decode_base64(encoded)?;
    let data = PricingData::decode(&bytes[..]).ok()?;

    // A frame about something with no price is a status message, not a quote.
    if data.id.is_empty() || data.price <= 0.0 {
        return None;
    }

    Some(QuoteTick {
        symbol: data.id,
        price: f64::from(data.price),
        change: Some(f64::from(data.change_percent) / 100.0),
        regular: data.market_hours == REGULAR_MARKET,
    })
}

fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(encoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the frame the server sends, so a wrong tag number — the failure
    /// this file is most exposed to — fails here rather than by showing
    /// someone a plausible wrong price.
    fn frame(data: &PricingData) -> Message {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(data.encode_to_vec());
        Message::text(serde_json::json!({ "message": encoded }).to_string())
    }

    #[test]
    fn a_socket_that_reconnects_without_delivering_does_not_claim_prices_are_back() {
        // The bug this shape exists to prevent. Announcing on connect rather
        // than on a price means a socket that opens and dies repeatedly says
        // "resumed" every time and "interrupted" never again, because the
        // stall is gated on having delivered something and those sessions
        // never do. The window keeps its last message and sits on frozen
        // prices.
        let mut health = Health::default();

        // A working session, then a failure the window is told about.
        assert!(!health.on_tick(), "nothing to resume from yet");
        assert!(health.on_drop(), "a stream that was working and stopped");

        // Now the flapping: connect, no ticks, die. Repeatedly.
        for _ in 0..5 {
            assert!(
                !health.on_drop(),
                "already told, and still nothing has arrived"
            );
        }
        assert!(health.told_stalled, "the window must still believe it stalled");

        // Only an actual price clears it.
        assert!(health.on_tick(), "a real price is what resumes");
        assert!(!health.told_stalled);
    }

    #[test]
    fn a_stream_that_never_worked_is_not_worth_interrupting_anyone_for() {
        // Nothing on screen has gone stale, because nothing was ever live.
        let mut health = Health::default();
        for _ in 0..3 {
            assert!(!health.on_drop());
        }
    }

    #[test]
    fn one_interruption_is_announced_once() {
        let mut health = Health::default();
        health.on_tick();
        assert!(health.on_drop());
        assert!(!health.on_drop(), "a retry loop must not notify every attempt");
    }

    #[test]
    fn a_price_arriving_while_nothing_is_wrong_announces_nothing() {
        let mut health = Health::default();
        assert!(!health.on_tick());
        assert!(!health.on_tick());
    }

    #[test]
    fn decodes_a_price_and_turns_the_percentage_into_a_fraction() {
        let tick = tick_of(&frame(&PricingData {
            id: "AAPL".into(),
            price: 231.5,
            market_hours: REGULAR_MARKET,
            change_percent: 1.25,
        }))
        .expect("a priced frame is a tick");

        assert_eq!(tick.symbol, "AAPL");
        assert!((tick.price - 231.5).abs() < 1e-4);
        // 1.25% arrives as 1.25 and has to leave as 0.0125, or every move on
        // screen is a hundred times too big.
        assert!((tick.change.unwrap() - 0.0125).abs() < 1e-6);
        assert!(tick.regular);
    }

    #[test]
    fn a_print_outside_the_session_is_kept_and_marked() {
        let tick = tick_of(&frame(&PricingData {
            id: "AAPL".into(),
            price: 230.0,
            market_hours: 2,
            change_percent: 0.0,
        }))
        .expect("an extended-hours print is still a price");
        assert!(!tick.regular);
    }

    #[test]
    fn frames_without_a_price_are_dropped_rather_than_shown_as_zero() {
        for unpriced in [
            PricingData {
                id: "AAPL".into(),
                price: 0.0,
                market_hours: REGULAR_MARKET,
                change_percent: 0.0,
            },
            PricingData {
                id: String::new(),
                price: 100.0,
                market_hours: REGULAR_MARKET,
                change_percent: 0.0,
            },
        ] {
            assert!(tick_of(&frame(&unpriced)).is_none());
        }
    }

    /// Proves the wire format against the real server: the endpoint, the
    /// subscribe message, the JSON envelope and — the part that cannot be
    /// checked any other way — that the tag numbers above still match what
    /// Yahoo sends.
    ///
    /// `#[ignore]` because it needs the internet and a market with something
    /// trading in it. Run it by hand when prices look wrong:
    /// `cargo test -p arvo-runtime -- --ignored --nocapture live_stream`.
    #[tokio::test]
    #[ignore = "hits the live Yahoo stream"]
    async fn live_stream_still_speaks_the_protocol_this_decodes() {
        let (mut socket, _) = tokio_tungstenite::connect_async(ENDPOINT)
            .await
            .expect("connect");
        subscribe(&mut socket, &["AAPL".to_owned(), "MSFT".to_owned(), "BTC-USD".to_owned()])
            .await
            .expect("subscribe");

        let tick = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(message) = socket.next().await {
                if let Some(tick) = tick_of(&message.expect("frame")) {
                    return tick;
                }
            }
            panic!("the socket closed before pricing anything");
        })
        .await
        .expect("a tick within 30s");

        println!("{tick:?}");
        assert!(tick.price > 0.0);
        assert!(!tick.symbol.is_empty());
    }

    #[test]
    fn junk_is_dropped_rather_than_killing_the_socket() {
        assert!(tick_of(&Message::text("not json")).is_none());
        assert!(tick_of(&Message::text(r#"{"message":"!!!not base64"}"#)).is_none());
        assert!(tick_of(&Message::text(r#"{"heartbeat":true}"#)).is_none());
    }
}
