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
use arvo_views::{EventKindView, EventView, SeverityView, EVENT_CHANNEL};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

/// Tells the window, and interrupts the person if it is worth it.
///
/// Neither failure is worth propagating: a window that has not finished
/// loading and a notification permission that was refused are both states
/// where the *event* still happened, and turning a dropped plugin into a
/// failed command would be reporting the wrong problem.
pub fn emit(app: &AppHandle, event: EventView) {
    if event.severity == SeverityView::Warning {
        if let Err(err) = app
            .notification()
            .builder()
            .title(event.title.clone())
            .body(event.detail.clone())
            .show()
        {
            tracing::warn!(error = %err, "failed to show notification");
        }
    }

    if let Err(err) = app.emit(EVENT_CHANNEL, &event) {
        tracing::warn!(error = %err, "failed to push an event to the window");
    }
}

/// The registry's own event, in the shape the window reads.
pub fn plugin(event: &Event) -> EventView {
    let Event::PluginStatusChanged { id, status } = event;
    match status {
        StatusKind::Reachable => EventView {
            kind: EventKindView::Plugin {
                id: id.clone(),
                reachable: true,
            },
            title: "Plugin reachable".to_owned(),
            detail: format!("{id} is now reachable"),
            severity: SeverityView::Info,
        },
        StatusKind::Unreachable => EventView {
            kind: EventKindView::Plugin {
                id: id.clone(),
                reachable: false,
            },
            title: "Plugin unreachable".to_owned(),
            detail: format!("{id} went unreachable"),
            severity: SeverityView::Warning,
        },
    }
}

pub fn feed_connected(id: &str) -> EventView {
    EventView {
        kind: EventKindView::Feed {
            id: id.to_owned(),
            connected: true,
        },
        title: format!("Signed in to {id}"),
        detail: format!("{id} market data is available"),
        severity: SeverityView::Info,
    }
}

/// A connection that went away.
///
/// `deliberate` is what separates "you clicked sign out" from "your session
/// expired while you were reading a chart". Only the second is worth an OS
/// notification — the first is something the person is already looking at.
pub fn feed_disconnected(id: &str, reason: &str, deliberate: bool) -> EventView {
    EventView {
        kind: EventKindView::Feed {
            id: id.to_owned(),
            connected: false,
        },
        title: if deliberate {
            format!("Signed out of {id}")
        } else {
            format!("{id} session ended")
        },
        detail: reason.to_owned(),
        severity: if deliberate {
            SeverityView::Info
        } else {
            SeverityView::Warning
        },
    }
}

/// Live prices stopped arriving.
///
/// Worth interrupting someone for, and this is the one case where an absence
/// is the alarm: a dead socket and a market where nothing is trading look
/// identical on screen. Only raised once per outage, and only after the
/// socket had been working — see `stream`.
pub fn stream_stalled(reason: &str) -> EventView {
    EventView {
        kind: EventKindView::Stream { live: false },
        title: "Live prices interrupted".to_owned(),
        detail: reason.to_owned(),
        severity: SeverityView::Warning,
    }
}

/// Live prices came back.
pub fn stream_live() -> EventView {
    EventView {
        kind: EventKindView::Stream { live: true },
        title: "Live prices resumed".to_owned(),
        detail: "the price stream reconnected".to_owned(),
        severity: SeverityView::Info,
    }
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

        assert_eq!(up.severity, SeverityView::Info);
        assert_eq!(down.severity, SeverityView::Warning);
        assert!(down.detail.contains("stub"));
        assert_eq!(
            up.kind,
            EventKindView::Plugin {
                id: "stub".into(),
                reachable: true
            }
        );
    }

    /// The distinction the status bar acts on: an expired session needs a
    /// sign-in prompt, a deliberate sign-out does not need a popup.
    #[test]
    fn an_expired_session_interrupts_and_signing_out_does_not() {
        let expired = feed_disconnected("robinhood", "the token was rejected", false);
        let signed_out = feed_disconnected("robinhood", "you signed out", true);

        assert_eq!(expired.severity, SeverityView::Warning);
        assert_eq!(signed_out.severity, SeverityView::Info);
        // Both still say the feed is down — the status bar reads one field.
        for event in [&expired, &signed_out] {
            assert_eq!(
                event.kind,
                EventKindView::Feed {
                    id: "robinhood".into(),
                    connected: false
                }
            );
        }
    }
}
