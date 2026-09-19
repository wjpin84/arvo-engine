use serde::{Serialize, Serializer};

/// Error surface for the service and the commands over it.
///
/// Tauri requires an async command that borrows `State<'_, _>` to return a
/// `Result`, so command signatures cannot simply return `T` — but `()` as the
/// error type means a future failure reaches the UI carrying nothing at all.
///
/// Serialises as a plain string, because that is what the front end can
/// actually render.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("{0}")]
    Failed(String),
}

impl Serialize for CommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
