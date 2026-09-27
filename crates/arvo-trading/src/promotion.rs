//! Whether a session may start at all.
//!
//! The gate in front of real money: an executor must be one this build knows,
//! and a live one must be preceded by a paper session that ran long enough and
//! did not diverge. A refusal here happens before any thread starts.

use std::path::Path;

use arvo_research::{EvidenceStore, Verdict};
use serde::Serialize;

use crate::record::record_path;

/// Where a session's record goes, under the data root: one JSON line per
/// event, which is what a later view of "what did the system do" reads.
pub const SUBDIR: &str = "sessions";

/// How a session names its venue: `alpaca-paper`, `alpaca-live`, or
/// `robinhood-<last four of the account>`, since a Robinhood login holds
/// more than one account and a session trades exactly one.
pub const EXECUTORS: &[&str] = &["alpaca-paper", "alpaca-live", "robinhood-<last4>"];

/// How long a finding must have run on paper before real money (#194).
///
/// Calendar days between the paper session's start and its last event.
/// Five is one trading week: long enough for a daily rule to have seen a
/// few bars and for the feed, the fills and the reconciliation to have
/// been exercised, and short enough that it is done rather than skipped.
/// The session verdict (#221) is what says whether those days looked like
/// the finding; this only says they happened.
pub const PAPER_MINIMUM_DAYS: i64 = 5;

/// The one executor that is not real money.
pub(crate) fn is_paper(executor: &str) -> bool {
    executor == "alpaca-paper"
}

/// The promotion gate's answer for one finding (#194): whether it may go to
/// real money, every reason it may not, and what the gate looked at.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Promotion {
    pub allowed: bool,
    pub reasons: Vec<String>,
    /// The finding's own verdict, when it can be opened.
    pub verdict: Option<String>,
    /// Days on paper, by the paper record; `None` without one.
    pub paper_days: Option<i64>,
    /// The paper session's last verdict against the finding, when it has one.
    pub paper_verdict: Option<String>,
}

/// Why a finding may not go to real money: every reason, so the person
/// fixes them all at once rather than one per attempt. Empty when it may.
///
/// The promotion gate (#194): a live executor accepts only a finding whose
/// verdict is Supported and which has run on paper for
/// [`PAPER_MINIMUM_DAYS`] without diverging from itself. The check is here,
/// on the start, for people and agents alike; nothing else creates a
/// session.
pub(crate) fn promotion(data: &Path, finding: &str) -> Promotion {
    let mut reasons = Vec::new();
    let answer = |reasons: Vec<String>,
                  verdict: Option<String>,
                  paper_days: Option<i64>,
                  paper_verdict: Option<String>| Promotion {
        allowed: reasons.is_empty(),
        reasons,
        verdict,
        paper_days,
        paper_verdict,
    };
    let store = EvidenceStore::new(data.join("evidence"));
    let found = match store.open(finding) {
        Ok(stored) => {
            let verdict = stored.record.verdict();
            if verdict != Verdict::Supported {
                reasons.push(format!(
                    "the finding's verdict is {verdict:?}, not Supported"
                ));
            }
            Some(format!("{verdict:?}"))
        }
        Err(err) => {
            reasons.push(format!("the finding cannot be opened: {err}"));
            None
        }
    };

    let paper = record_path(data, &format!("{finding}@alpaca-paper"));
    let Ok(text) = std::fs::read_to_string(&paper) else {
        reasons.push(format!("no paper session on this finding; run it on alpaca-paper for {PAPER_MINIMUM_DAYS} days first"));
        return answer(reasons, found, None, None);
    };
    let mut started: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut verdict: Option<(String, Option<String>)> = None;
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let at = event["at"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc));
        match event["event"].as_str() {
            Some("started") if started.is_none() => started = at,
            Some("verdict") => {
                verdict = event["detail"]["verdict"].as_str().map(|name| {
                    (
                        name.to_owned(),
                        event["detail"]["reason"].as_str().map(str::to_owned),
                    )
                });
            }
            _ => {}
        }
        last = at.or(last);
    }
    let paper_days = match (started, last) {
        (Some(started), Some(last)) => {
            let days = (last - started).num_days();
            if days < PAPER_MINIMUM_DAYS {
                reasons.push(format!(
                    "the paper session ran {days} day(s); {PAPER_MINIMUM_DAYS} are needed"
                ));
            }
            Some(days)
        }
        _ => {
            reasons.push("the paper session's record has no start".to_owned());
            None
        }
    };
    if let Some(("diverging", why)) = verdict.as_ref().map(|(name, why)| (name.as_str(), why)) {
        reasons.push(format!(
            "the paper session was diverging from the finding when last judged{}",
            why.as_ref()
                .map_or(String::new(), |why| format!(" ({why})"))
        ));
    }
    answer(reasons, found, paper_days, verdict.map(|(name, _)| name))
}

pub(crate) fn executor_is_known(executor: &str) -> bool {
    matches!(executor, "alpaca-paper" | "alpaca-live")
        || executor
            .strip_prefix("robinhood-")
            .is_some_and(|last| last.len() == 4 && last.chars().all(|c| c.is_ascii_digit()))
}
