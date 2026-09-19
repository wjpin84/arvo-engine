//! The risk model a study runs under, as a file in the project (ADR-0020).
//!
//! `.arvo/risk.json` beside `.arvo/settings.json`: the `RiskModel` every new
//! experiment is pinned with, written out with the shipped values the first
//! time a project is opened so it can be found, read, and changed in the
//! editor like any other file. A finding keeps the model it was studied
//! with — this file shapes what is studied next, never what was concluded.
//!
//! # A bad file refuses, it does not default
//!
//! A limit that is mistyped and silently dropped is worse than no limit: the
//! person believes they have one (ADR-0009). So a file that does not parse,
//! fails `RiskModel::check`, or carries a key the model does not know stops
//! every study with the reason, until it is fixed. Only an *absent* file
//! means the shipped model.
//!
//! # One model in force
//!
//! Loaded into a process-wide slot at each study request, because the
//! functions that build an experiment (`study_for`, `walk_forward_for`) are
//! called from the window, the engine and the tests alike and none of them
//! carries a project root. The slot starts as the shipped model, so a caller
//! that never loads a file gets what every study got before the file existed.

use std::path::Path;
use std::sync::RwLock;

use arvo_research::RiskModel;

/// Where the file lives, relative to the project folder.
pub const PROJECT_FILE: &str = ".arvo/risk.json";

/// Risk settings for a workbench study, from the middle of the range the
/// systematic-trading literature actually uses: a 2x ATR stop and 1% of
/// capital at risk per trade.
///
/// Stated rather than left absent. Running with no stop is a different
/// strategy with a fatter left tail, and a study that quietly omitted one
/// would be answering an easier question than the one asked.
const STOP_ATR_MULTIPLE: f64 = 2.0;
const ATR_PERIOD: usize = 14;
const RISK_PER_TRADE: f64 = 0.01;

static CURRENT: RwLock<Option<RiskModel>> = RwLock::new(None);

/// The model Arvo ships: what a project gets until someone edits the file.
#[must_use]
pub fn shipped() -> RiskModel {
    RiskModel {
        stop_atr_multiple: Some(STOP_ATR_MULTIPLE),
        atr_period: ATR_PERIOD,
        risk_per_trade: Some(RISK_PER_TRADE),
        ..RiskModel::default()
    }
}

/// The model in force: the last one loaded, else the shipped one.
#[must_use]
pub fn current() -> RiskModel {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(shipped)
}

/// Reads the project's file into force. Absent means shipped; anything the
/// model cannot stand behind is an error and leaves what was in force alone.
///
/// # Errors
///
/// The file exists and cannot be read, is not a risk model, names a key the
/// model does not have, or fails the model's own check.
pub fn load(root: &Path) -> Result<RiskModel, String> {
    let model = read(&root.join(PROJECT_FILE))?;
    *CURRENT.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(model.clone());
    Ok(model)
}

/// The file's model, or the shipped one when there is no file.
///
/// # Errors
///
/// As [`load`].
pub fn read(path: &Path) -> Result<RiskModel, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(shipped()),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    parse(&text).map_err(|reason| format!("{}: {reason}", path.display()))
}

/// A model from its JSON, refusing a key it does not know.
pub fn parse(text: &str) -> Result<RiskModel, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let known = serde_json::to_value(RiskModel::default()).map_err(|err| err.to_string())?;
    if let (Some(given), Some(known)) = (value.as_object(), known.as_object()) {
        if let Some(unknown) = given.keys().find(|key| !known.contains_key(*key)) {
            return Err(format!(
                "\"{unknown}\" is not a risk setting; the settings are {}",
                known.keys().map(|key| format!("\"{key}\"")).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    let model: RiskModel = serde_json::from_value(value).map_err(|err| err.to_string())?;
    model.check()?;
    Ok(model)
}

/// Writes the shipped model when the project has no file yet, so there is
/// something to open. Never overwrites: the file is the person's.
///
/// # Errors
///
/// When the file cannot be written.
/// The project's risk file as the window and the engine report it: what is
/// in force for the next study, and why the file was refused if it was.
#[must_use]
pub fn view(root: &Path) -> arvo_views::RiskModelView {
    let path = root.join(PROJECT_FILE);
    let exists = path.exists();
    let (model, error) = match read(&path) {
        Ok(model) => (model, None),
        Err(reason) => (current(), Some(reason)),
    };
    arvo_views::RiskModelView {
        path: PROJECT_FILE.to_owned(),
        exists,
        model: serde_json::to_value(model).unwrap_or_default(),
        error,
    }
}

pub fn ensure(root: &Path) -> std::io::Result<()> {
    let path = root.join(PROJECT_FILE);
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&shipped()).map_err(std::io::Error::other)?;
    std::fs::write(path, text + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_shipped_model_and_a_bad_one_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(&dir.path().join(PROJECT_FILE)).expect("absent"), shipped());

        ensure(dir.path()).expect("written");
        let written = read(&dir.path().join(PROJECT_FILE)).expect("reads back");
        assert_eq!(written, shipped(), "the file round-trips the shipped model");
        assert!(written.stop_atr_multiple.is_some(), "the shipped model has a stop");

        let path = dir.path().join(PROJECT_FILE);
        std::fs::write(&path, r#"{"max_daily_los": 0.02}"#).expect("write");
        let refused = read(&path).expect_err("a mistyped key is not a limit");
        assert!(refused.contains("max_daily_los") && refused.contains("max_daily_loss"), "{refused}");

        std::fs::write(&path, r#"{"atr_period": 0}"#).expect("write");
        assert!(read(&path).expect_err("the model's own check").contains("atr_period"));

        std::fs::write(&path, "{").expect("write");
        assert!(read(&path).is_err());
    }
}
