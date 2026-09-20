//! The one place the backend speaks first.
//!
//! Every other command in this crate answers something the window asked. The
//! window had no way to hear anything else — no `emit` anywhere in the
//! workspace — so a plugin that dropped, or a broker session that expired
//! between two clicks, could only surface as the *next* thing to fail. The
//! status bar said "N/M plugins reachable" and the alerts panel was a
//! hardcoded empty state, because there was nothing to fill them from.
//!
//! # One seam, two renderers
//!
//! [`emit`] is that single point. It pushes to the window *and* decides
//! whether the event is worth an OS notification, so those two can never
//! disagree about what happened or how it is worded — the strings are built
//! once, here, and carried in the payload.
//!
//! This replaces `arvo_core::notifications`, whose entire job was mapping one
//! event to a title and a body. That mapping now has a second consumer, and
//! duplicating it was the alternative.

use arvo_core::events::{Event, StatusKind};
use arvo_api::{EventKindView, EventView, SeverityView};

/// The registry's own event, in the shape the window reads.
pub fn plugin(event: &Event) -> EventView {
    let Event::PluginStatusChanged { id, status } = event;
    match status {
        StatusKind::Reachable => EventView::new(
            EventKindView::plugin(id.clone(), true),
            "Plugin reachable".to_owned(),
            format!("{id} is now reachable"),
            SeverityView::Info,
        ),
        StatusKind::Unreachable => EventView::new(
            EventKindView::plugin(id.clone(), false),
            "Plugin unreachable".to_owned(),
            format!("{id} went unreachable"),
            SeverityView::Warning,
        ),
    }
}

pub fn feed_connected(id: &str) -> EventView {
    EventView::new(
        EventKindView::feed(id.to_owned(), true),
        format!("Signed in to {id}"),
        format!("{id} market data is available"),
        SeverityView::Info,
    )
}

/// A connection that went away.
///
/// `deliberate` is what separates "you clicked sign out" from "your session
/// expired while you were reading a chart". Only the second is worth an OS
/// notification — the first is something the person is already looking at.
pub fn feed_disconnected(id: &str, reason: &str, deliberate: bool) -> EventView {
    EventView::new(
        EventKindView::feed(id.to_owned(), false),
        if deliberate { format!("Signed out of {id}") } else { format!("{id} session ended") },
        reason.to_owned(),
        if deliberate { SeverityView::Info } else { SeverityView::Warning },
    )
}

/// Live prices stopped arriving.
///
/// Worth interrupting someone for, and this is the one case where an absence
/// is the alarm: a dead socket and a market where nothing is trading look
/// identical on screen. Only raised once per outage, and only after the
/// socket had been working — see `stream`.
pub fn stream_stalled(reason: &str) -> EventView {
    EventView::new(
        EventKindView::stream(false),
        "Live prices interrupted".to_owned(),
        reason.to_owned(),
        SeverityView::Warning,
    )
}

/// Stored findings went stale while nobody was looking.
///
/// A warning, so it raises a notification: the whole point is that the person
/// was not in History when it happened. Names the subjects, a few of them —
/// a count alone says something changed and not what to reopen.
pub fn findings_stale(changed: &[String], gone: &[String]) -> EventView {
    const NAMED: usize = 3;
    let named = |subjects: &[String]| {
        // First-seen order, each once: several findings on one instrument
        // are one thing to reopen.
        let mut seen = std::collections::BTreeSet::new();
        let unique: Vec<&str> = subjects
            .iter()
            .map(String::as_str)
            .filter(|subject| seen.insert(*subject))
            .collect();
        let shown = unique.iter().take(NAMED).copied().collect::<Vec<_>>().join(", ");
        match unique.len().saturating_sub(NAMED) {
            0 => shown,
            more => format!("{shown} and {more} more"),
        }
    };
    let count = changed.len() + gone.len();
    let mut detail = Vec::new();
    if !changed.is_empty() {
        detail.push(format!(
            "{} produced from data that has since changed ({})",
            plural(changed.len(), "finding was", "findings were"),
            named(changed),
        ));
    }
    if !gone.is_empty() {
        detail.push(format!(
            "{} whose data is no longer present ({})",
            plural(gone.len(), "finding", "findings"),
            named(gone),
        ));
    }
    EventView::new(
        EventKindView::findings(arvo_api::count(count)),
        format!("{} went stale", plural(count, "finding", "findings")),
        format!(
            "{}. Their verdicts describe data you no longer have; re-run from History to see whether they still hold.",
            detail.join("; ")
        ),
        SeverityView::Warning,
    )
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A trading session changed state.
///
/// Halted and failed are worth interrupting someone for: money that was
/// being managed is not, any more, and nobody clicked anything. The rest
/// is information.
#[must_use]
pub fn session(id: &str, state: &str, detail: Option<&str>) -> EventView {
    let alarming = matches!(state, "halted" | "failed");
    EventView::new(
        EventKindView::session(id.to_owned(), state.to_owned()),
        format!("Session {state}"),
        match detail {
            Some(why) => format!("{id}: {why}"),
            None => id.to_owned(),
        },
        if alarming { SeverityView::Warning } else { SeverityView::Info },
    )
}

/// A source failure worth announcing, if it is one.
///
/// A session can expire between any two calls, and the call that happens to
/// discover it is not the one a person is looking at. Both the engine and the
/// window map the failure through here, so a dead broker session reaches the
/// alerts list whichever of them found it.
#[must_use]
pub fn disconnected(err: &crate::source::SourceError) -> Option<EventView> {
    match err {
        crate::source::SourceError::NoSession { vendor } => {
            Some(feed_disconnected(vendor, &err.to_string(), false))
        }
        _ => None,
    }
}

/// Live prices came back.
pub fn stream_live() -> EventView {
    EventView::new(
        EventKindView::stream(true),
        "Live prices resumed".to_owned(),
        "the price stream reconnected".to_owned(),
        SeverityView::Info,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_plugin_is_worth_interrupting_someone_for_and_a_recovered_one_is_not() {
        let up = plugin(&Event::PluginStatusChanged {
            id: "stub".into(),
            status: StatusKind::Reachable,
        });
        let down = plugin(&Event::PluginStatusChanged {
            id: "stub".into(),
            status: StatusKind::Unreachable,
        });

        assert_eq!(up.severity(), SeverityView::Info);
        assert_eq!(down.severity(), SeverityView::Warning);
        assert!(down.detail.contains("stub"));
        assert_eq!(up.kind, Some(EventKindView::plugin("stub".into(), true)));
    }

    #[test]
    fn a_stale_finding_interrupts_and_names_what_to_reopen() {
        let event = findings_stale(
            &["AAPL.YF".to_owned(), "MSFT.YF".to_owned()],
            &["OLD.SIM".to_owned()],
        );
        assert_eq!(event.severity(), SeverityView::Warning);
        assert_eq!(event.kind, Some(EventKindView::findings(3)));
        assert_eq!(event.title, "3 findings went stale");
        assert!(event.detail.contains("2 findings were produced"), "{}", event.detail);
        assert!(event.detail.contains("MSFT.YF"), "{}", event.detail);
        assert!(event.detail.contains("1 finding whose data"), "{}", event.detail);
    }

    #[test]
    fn a_long_list_is_cut_to_a_few_names() {
        let mut many: Vec<String> = (0..7).map(|i| format!("S{i}.YF")).collect();
        many.push("S0.YF".to_owned());
        let event = findings_stale(&many, &[]);
        assert!(event.detail.contains("and 4 more"), "{}", event.detail);
    }

    /// The distinction the status bar acts on: an expired session needs a
    /// sign-in prompt, a deliberate sign-out does not need a popup.
    #[test]
    fn an_expired_session_interrupts_and_signing_out_does_not() {
        let expired = feed_disconnected("robinhood", "the token was rejected", false);
        let signed_out = feed_disconnected("robinhood", "you signed out", true);

        assert_eq!(expired.severity(), SeverityView::Warning);
        assert_eq!(signed_out.severity(), SeverityView::Info);
        // Both still say the feed is down — the status bar reads one field.
        for event in [&expired, &signed_out] {
            assert_eq!(event.kind, Some(EventKindView::feed("robinhood".into(), false)));
        }
    }
}
