use crate::events::{Event, StatusKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// The testable unit — OS delivery itself isn't easily assertable in an
/// automated test, so the mapping is kept pure and separate from it.
pub fn event_to_notification(event: &Event) -> Notification {
    let Event::PluginStatusChanged { id, status } = event;
    match status {
        StatusKind::Reachable => Notification {
            title: "Plugin reachable".into(),
            body: format!("{id} is now reachable"),
        },
        StatusKind::Unreachable => Notification {
            title: "Plugin unreachable".into(),
            body: format!("{id} went unreachable"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_reachable_transition() {
        let event = Event::PluginStatusChanged {
            id: "stub".into(),
            status: StatusKind::Reachable,
        };
        let notification = event_to_notification(&event);
        assert_eq!(notification.title, "Plugin reachable");
        assert!(notification.body.contains("stub"));
    }

    #[test]
    fn maps_unreachable_transition() {
        let event = Event::PluginStatusChanged {
            id: "stub".into(),
            status: StatusKind::Unreachable,
        };
        let notification = event_to_notification(&event);
        assert_eq!(notification.title, "Plugin unreachable");
        assert!(notification.body.contains("stub"));
    }
}
