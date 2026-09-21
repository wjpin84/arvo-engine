//! Alpaca's bar stream, behind `Source::stream` (#185).
//!
//! One socket per session, on the feed the source fetches from, subscribed
//! to one symbol's one-minute bars. Alpaca sends a bar a moment after the
//! minute closes, which is what a 1-minute rule needs and what polling every
//! sixty seconds cannot give it.
//!
//! # What "stale" means here
//!
//! A dead socket, not a quiet one. Alpaca sends nothing between bars and
//! nothing at all while the market is closed, so silence is not evidence.
//! What is evidence is a ping that gets no pong: the socket is pinged every
//! [`PING`], and one that has said nothing back for [`QUIET`] is dropped,
//! reported as `Down`, and reconnected. A session sees `Down` and decides
//! what a dark feed means for it; it sees `Up` when the outage is over.
//!
//! ponytail: a live socket that stops delivering bars during market hours
//! is not caught here; add the venue clock (`/v2/clock`) to the check when
//! one has been seen.

use std::time::{Duration, Instant};

use arvo_data::source::{BarFeed, FeedEvent};
use arvo_data::Bar;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::auth::Keys;

const ENDPOINT: &str = "wss://stream.data.alpaca.markets/v2";
/// How often the socket is pinged.
const PING: Duration = Duration::from_secs(15);
/// How long a socket may say nothing — no bar, no pong — before it is
/// declared dead. Two pings' worth and change.
const QUIET: Duration = Duration::from_secs(40);
/// How long each step of the handshake may take.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How long to wait before reconnecting.
const BACKOFF: Duration = Duration::from_secs(3);

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Five one-minute bars into one five-minute bar, and so on: Alpaca streams
/// minutes and Arvo's intraday rules read five of them.
///
/// A bucket is `step` minutes from the top of the hour, the way the fetched
/// bars are aligned. The bucket's bar goes out the moment its last minute
/// arrives — one second after the close, not five minutes after — and, when
/// a minute never printed (IEX skips a quiet one), when the next bucket's
/// first bar shows the old one is over. A bucket still open when the feed
/// goes quiet waits; the poll underneath the session catches it up.
struct Aggregator {
    step: u32,
    partial: Option<Bar>,
}

impl Aggregator {
    fn bucket_of(&self, at: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
        use chrono::Timelike as _;
        let minute = at.minute() - at.minute() % self.step;
        at.with_minute(minute).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).unwrap_or(at)
    }

    /// Feeds one minute; returns a completed bucket when one is.
    fn push(&mut self, bar: Bar) -> Option<Bar> {
        use chrono::Timelike as _;
        let bucket = self.bucket_of(bar.at);
        let mut done = None;
        match &mut self.partial {
            Some(partial) if partial.at == bucket => {
                partial.high = partial.high.max(bar.high);
                partial.low = partial.low.min(bar.low);
                partial.close = bar.close;
                partial.volume += bar.volume;
            }
            Some(partial) => {
                done = Some(*partial);
                *partial = Bar { at: bucket, ..bar };
            }
            None => self.partial = Some(Bar { at: bucket, ..bar }),
        }
        // The last minute of the bucket closes it right away.
        if bar.at.minute() % self.step == self.step - 1 {
            if let Some(partial) = self.partial.take() {
                return Some(match done {
                    // Cannot happen — a bar that closed one bucket is the first
                    // of the next — but if it did, the older one goes first.
                    Some(older) => {
                        self.partial = Some(partial);
                        older
                    }
                    None => partial,
                });
            }
        }
        done
    }
}

/// The receiving end of one subscription.
pub struct AlpacaFeed {
    events: mpsc::Receiver<FeedEvent>,
    aggregate: Option<Aggregator>,
}

/// How long after a bucket's close to wait for its last minute before
/// closing it on the clock: the minute that never printed (no IEX trade in
/// it) would otherwise hold the bar until the next one showed, which the
/// first live run measured at seven seconds — the poll got there first.
const CLOSE_GRACE: Duration = Duration::from_millis(1500);

#[async_trait::async_trait]
impl BarFeed for AlpacaFeed {
    async fn next(&mut self) -> Option<FeedEvent> {
        loop {
            // A bucket that is over on the clock goes out even if its last
            // minute never printed; the deadline is that instant plus grace.
            let deadline = self.aggregate.as_ref().and_then(|aggregate| aggregate.partial.as_ref()).map(|partial| {
                let closes = (partial.at + chrono::Duration::minutes(i64::from(self.aggregate.as_ref().map_or(1, |a| a.step)))).and_utc();
                let wait = (closes - chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO) + CLOSE_GRACE;
                tokio::time::Instant::now() + wait
            });
            let event = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, self.events.recv()).await {
                    Ok(event) => event?,
                    Err(_) => {
                        if let Some(bar) = self.aggregate.as_mut().and_then(|aggregate| aggregate.partial.take()) {
                            return Some(FeedEvent::Bar(bar));
                        }
                        continue;
                    }
                },
                None => self.events.recv().await?,
            };
            match (&mut self.aggregate, event) {
                (Some(aggregate), FeedEvent::Bar(minute)) => {
                    if let Some(bar) = aggregate.push(minute) {
                        return Some(FeedEvent::Bar(bar));
                    }
                }
                (_, event) => return Some(event),
            }
        }
    }
}

/// Opens the stream for `symbol` on `feed` (`iex` or `sip`), delivering
/// bars of `minutes` each. The socket lives on a task of the current
/// runtime and dies with the receiver.
pub(crate) fn open(feed: &'static str, symbol: String, keys: Keys, minutes: u32) -> AlpacaFeed {
    let (events, receiver) = mpsc::channel(64);
    tokio::spawn(run(feed, symbol, keys, events));
    AlpacaFeed { events: receiver, aggregate: (minutes > 1).then_some(Aggregator { step: minutes, partial: None }) }
}

async fn run(feed: &'static str, symbol: String, keys: Keys, events: mpsc::Sender<FeedEvent>) {
    let url = format!("{ENDPOINT}/{feed}");
    loop {
        match serve(&url, &symbol, &keys, &events, PING, QUIET).await {
            // The receiver is gone: the session ended.
            Ok(()) => return,
            Err(reason) => {
                if events.send(FeedEvent::Down(reason)).await.is_err() {
                    return;
                }
                tokio::time::sleep(BACKOFF).await;
            }
        }
    }
}

/// One connection, from handshake to the frame that kills it. `ping` and
/// `quiet` are [`PING`] and [`QUIET`] outside the tests.
async fn serve(
    url: &str,
    symbol: &str,
    keys: &Keys,
    events: &mpsc::Sender<FeedEvent>,
    ping: Duration,
    quiet: Duration,
) -> Result<(), String> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|err| format!("connecting: {err}"))?;
    expect(&mut socket, "connected").await?;
    send(&mut socket, json!({ "action": "auth", "key": keys.key_id, "secret": keys.secret })).await?;
    expect(&mut socket, "authenticated").await?;
    send(&mut socket, json!({ "action": "subscribe", "bars": [symbol] })).await?;
    if events.send(FeedEvent::Up).await.is_err() {
        return Ok(());
    }

    let mut heard = Instant::now();
    let mut ping = tokio::time::interval(ping);
    ping.tick().await;
    loop {
        tokio::select! {
            frame = socket.next() => {
                let message = match frame {
                    None => return Err("the socket closed".to_owned()),
                    Some(Err(err)) => return Err(format!("the socket failed: {err}")),
                    Some(Ok(message)) => message,
                };
                heard = Instant::now();
                for event in decode(&message)? {
                    if events.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
            _ = ping.tick() => {
                if heard.elapsed() > quiet {
                    return Err(format!("nothing heard for {}s", heard.elapsed().as_secs()));
                }
                socket.send(Message::Ping(Default::default())).await.map_err(|err| format!("pinging: {err}"))?;
            }
        }
    }
}

async fn send(socket: &mut Socket, body: Value) -> Result<(), String> {
    socket.send(Message::text(body.to_string())).await.map_err(|err| format!("sending: {err}"))
}

/// Reads until Alpaca says `{"T":"success","msg":<msg>}`, failing on an
/// error message or on silence.
async fn expect(socket: &mut Socket, msg: &str) -> Result<(), String> {
    let deadline = tokio::time::sleep(HANDSHAKE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            frame = socket.next() => {
                let message = match frame {
                    None => return Err(format!("the socket closed before {msg:?}")),
                    Some(Err(err)) => return Err(format!("the socket failed before {msg:?}: {err}")),
                    Some(Ok(message)) => message,
                };
                if items(&message)?.iter().any(|item| item.get("T").and_then(Value::as_str) == Some("success") && item.get("msg").and_then(Value::as_str) == Some(msg)) {
                    return Ok(());
                }
            }
            () = &mut deadline => return Err(format!("no {msg:?} within {}s", HANDSHAKE.as_secs())),
        }
    }
}

/// The JSON items in one frame: Alpaca sends arrays, one message each.
/// Control frames carry none. An `error` item is the whole frame's failure.
fn items(message: &Message) -> Result<Vec<Value>, String> {
    let text = match message {
        Message::Text(text) => text.as_str().to_owned(),
        Message::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => return Ok(Vec::new()),
    };
    let value: Value = serde_json::from_str(&text).map_err(|err| format!("not JSON: {err}"))?;
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    if let Some(error) = items.iter().find(|item| item.get("T").and_then(Value::as_str) == Some("error")) {
        return Err(format!(
            "alpaca refused: {} (code {})",
            error.get("msg").and_then(Value::as_str).unwrap_or("no reason"),
            error.get("code").and_then(Value::as_i64).unwrap_or_default()
        ));
    }
    Ok(items)
}

/// The bars in one frame, regular session only, like the fetched ones.
/// Everything else Alpaca sends — subscription confirmations, trade
/// updates nobody asked for — is not an event.
fn decode(message: &Message) -> Result<Vec<FeedEvent>, String> {
    Ok(items(message)?
        .iter()
        .filter(|item| item.get("T").and_then(Value::as_str) == Some("b"))
        .map(bar_of)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|bar| arvo_data::session::in_regular_session(bar.at))
        .map(FeedEvent::Bar)
        .collect())
}

fn bar_of(item: &Value) -> Result<Bar, String> {
    let field = |name: &str| item.get(name).and_then(Value::as_f64);
    let (Some(open), Some(high), Some(low), Some(close)) = (field("o"), field("h"), field("l"), field("c")) else {
        return Err("a streamed bar is missing one of o/h/l/c".to_owned());
    };
    let at = item.get("t").and_then(Value::as_str).ok_or("a streamed bar has no timestamp")?;
    let at = chrono::DateTime::parse_from_rfc3339(at)
        .map_err(|err| format!("timestamp {at:?}: {err}"))?
        .naive_utc();
    Ok(Bar { at, open, high, low, close, volume: field("v").unwrap_or_default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Message {
        Message::text(text.to_owned())
    }

    #[test]
    fn a_bar_frame_becomes_a_bar_at_its_open_and_a_confirmation_becomes_nothing() {
        // 14:30 UTC is 10:30 New York on a Tuesday: inside the regular session.
        let decoded = decode(&frame(
            r#"[{"T":"subscription","bars":["AAPL"]},{"T":"b","S":"AAPL","o":100.0,"h":101.0,"l":99.5,"c":100.5,"v":1200,"t":"2026-09-15T14:30:00Z","n":40,"vw":100.2}]"#,
        ))
        .expect("decodes");
        let [FeedEvent::Bar(bar)] = decoded.as_slice() else {
            panic!("one bar, no event for the confirmation: {decoded:?}");
        };
        assert_eq!(bar.at, chrono::NaiveDate::from_ymd_opt(2026, 9, 15).unwrap().and_hms_opt(14, 30, 0).unwrap());
        assert!((bar.close - 100.5).abs() < 1e-9);
        assert!((bar.volume - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn a_bar_outside_the_regular_session_is_dropped_like_a_fetched_one() {
        // 08:00 New York: pre-market. The REST path drops these; so does this.
        let decoded = decode(&frame(r#"[{"T":"b","S":"AAPL","o":1,"h":1,"l":1,"c":1,"v":1,"t":"2026-09-15T12:00:00Z"}]"#)).expect("decodes");
        assert!(decoded.is_empty(), "{decoded:?}");
    }

    #[test]
    fn an_error_from_alpaca_fails_the_frame_with_its_reason() {
        let refused = decode(&frame(r#"[{"T":"error","code":402,"msg":"auth failed"}]"#)).expect_err("refused");
        assert!(refused.contains("auth failed") && refused.contains("402"), "{refused}");
    }

    #[test]
    fn half_a_bar_is_refused_rather_than_filled_in() {
        let refused = decode(&frame(r#"[{"T":"b","S":"AAPL","o":1,"h":1,"t":"2026-09-15T14:30:00Z"}]"#)).expect_err("refused");
        assert!(refused.contains("o/h/l/c"), "{refused}");
    }

    #[test]
    fn five_minutes_become_one_bar_the_moment_the_fifth_closes() {
        let minute = |m: u32, o: f64, h: f64, l: f64, c: f64, v: f64| Bar {
            at: chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap().and_hms_opt(13, m, 0).unwrap(),
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        };
        let mut five = Aggregator { step: 5, partial: None };
        assert!(five.push(minute(30, 10.0, 11.0, 9.0, 10.5, 100.0)).is_none());
        assert!(five.push(minute(31, 10.5, 12.0, 10.0, 11.0, 50.0)).is_none());
        assert!(five.push(minute(32, 11.0, 11.5, 8.0, 9.0, 25.0)).is_none());
        assert!(five.push(minute(33, 9.0, 9.5, 8.5, 9.2, 25.0)).is_none());
        let bar = five.push(minute(34, 9.2, 9.9, 9.1, 9.8, 100.0)).expect("the fifth minute closes the bucket");
        assert_eq!(bar.at, minute(30, 0.0, 0.0, 0.0, 0.0, 0.0).at);
        assert_eq!((bar.open, bar.high, bar.low, bar.close, bar.volume), (10.0, 12.0, 8.0, 9.8, 300.0));
        // A quiet minute never printed: the bucket closes when the next one starts.
        assert!(five.push(minute(35, 9.8, 9.9, 9.7, 9.8, 10.0)).is_none());
        assert!(five.push(minute(36, 9.8, 9.9, 9.7, 9.8, 10.0)).is_none());
        let late = five.push(minute(40, 9.8, 9.9, 9.7, 9.8, 10.0)).expect("the old bucket is over");
        assert_eq!(late.at, minute(35, 0.0, 0.0, 0.0, 0.0, 0.0).at);
        assert!((late.volume - 20.0).abs() < 1e-9);
    }

    #[test]
    fn control_frames_carry_no_events() {
        assert!(decode(&Message::Pong(Default::default())).expect("fine").is_empty());
    }

    /// A stand-in for Alpaca on a loopback port: answers the handshake, then
    /// does what `then` says with the socket.
    async fn fake_alpaca<F>(then: F) -> String
    where
        F: FnOnce(Socket) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let url = format!("ws://{}", listener.local_addr().expect("bound"));
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("a client");
            let mut socket: Socket = tokio_tungstenite::accept_async(tokio_tungstenite::MaybeTlsStream::Plain(tcp)).await.expect("a websocket");
            socket.send(Message::text(r#"[{"T":"success","msg":"connected"}]"#)).await.expect("sent");
            let auth = socket.next().await.expect("auth").expect("a frame");
            assert!(auth.to_text().expect("text").contains(r#""action":"auth""#), "{auth:?}");
            socket.send(Message::text(r#"[{"T":"success","msg":"authenticated"}]"#)).await.expect("sent");
            let subscribe = socket.next().await.expect("subscribe").expect("a frame");
            assert!(subscribe.to_text().expect("text").contains(r#""bars":["AAPL"]"#), "{subscribe:?}");
            then(socket).await;
        });
        url
    }

    fn keys() -> Keys {
        Keys { key_id: "k".to_owned(), secret: "s".to_owned() }
    }

    #[tokio::test]
    async fn a_socket_that_stops_answering_pings_is_declared_dead() {
        // Never reads, so never pongs: what a half-open connection looks like.
        let url = fake_alpaca(|socket| {
            Box::pin(async move {
                let _held_open = socket;
                std::future::pending::<()>().await;
            })
        })
        .await;
        let (events, mut receiver) = mpsc::channel(8);
        let started = Instant::now();
        let ended = serve(&url, "AAPL", &keys(), &events, Duration::from_millis(50), Duration::from_millis(200)).await;

        assert_eq!(receiver.recv().await, Some(FeedEvent::Up));
        let reason = ended.expect_err("declared dead");
        assert!(reason.starts_with("nothing heard"), "{reason}");
        assert!(started.elapsed() < Duration::from_secs(3), "and promptly: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_bar_arrives_as_an_event_and_a_close_is_reported_with_its_reason() {
        let url = fake_alpaca(|mut socket| {
            Box::pin(async move {
                socket
                    .send(Message::text(r#"[{"T":"b","S":"AAPL","o":1,"h":2,"l":0.5,"c":1.5,"v":10,"t":"2026-09-15T14:30:00Z"}]"#))
                    .await
                    .expect("sent");
                socket.close(None).await.expect("closed");
            })
        })
        .await;
        let (events, mut receiver) = mpsc::channel(8);
        let ended = serve(&url, "AAPL", &keys(), &events, Duration::from_secs(1), Duration::from_secs(5)).await;

        assert_eq!(receiver.recv().await, Some(FeedEvent::Up));
        let Some(FeedEvent::Bar(bar)) = receiver.recv().await else { panic!("a bar") };
        assert!((bar.close - 1.5).abs() < 1e-9);
        // Closed, or reset once the fake is gone: either way the socket, named.
        assert!(ended.expect_err("ended").contains("the socket"));
    }
}
