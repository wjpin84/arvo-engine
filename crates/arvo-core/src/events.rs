/// Deliberately light — a signal to go fetch detail from the source of
/// truth, not a payload duplicating it. A subscriber that needs the full
/// `Manifest`/failure reason calls `PluginRegistry::snapshot()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusKind {
    Reachable,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PluginStatusChanged { id: String, status: StatusKind },
}
