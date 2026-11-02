//! Whether a branch's record agrees with its event log
//! (`docs/durability.md#the-record-and-the-log`).
//!
//! The log says what happened; the record says what is true now. Every
//! write that changes a branch's status also appends an
//! [`Activity::Status`] event in the same transaction (a turn's start, its
//! conclusion through `Recorder::finish`, a merge, a rewind, a park, a
//! recovery), so the last status the log records is the status the record
//! holds. [`Yard::drift`](crate::Yard::drift) checks exactly that: a branch
//! whose record moved without its log, or whose log moved without its
//! record, is drift, and names a write path that broke the rule.
//!
//! The status is the field checked because everything keys on it:
//! recovery, waits, obligations, the console. The candidate is not: a
//! rewind decides it by comparing trees, which its event does not carry.
//! A branch whose log has no status event yet (one imported from an
//! earlier version) has nothing to compare and is not drift.

use serde::{Deserialize, Serialize};

use crate::state::Record;
use crate::{Activity, BranchStatus, RecordedEvent};

/// One branch whose record and log disagree.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Drift {
    /// The branch.
    pub branch: String,
    /// The field that disagrees: `status`.
    pub field: String,
    /// What the record holds.
    pub record: String,
    /// What folding the log gives.
    pub log: String,
}

/// The status the log ends on: its last [`Activity::Status`] event.
pub(crate) fn logged_status(events: &[RecordedEvent]) -> Option<&BranchStatus> {
    events.iter().rev().find_map(|event| match &event.activity {
        Activity::Status(status) => Some(status),
        _ => None,
    })
}

/// Where `record` and its `events` disagree, if they do.
pub(crate) fn check(record: &Record, events: &[RecordedEvent]) -> Option<Drift> {
    let logged = logged_status(events)?;
    (logged != &record.info.status).then(|| Drift {
        branch: record.info.name.clone(),
        field: "status".into(),
        record: crate::wake::status_text(&record.info.status),
        log: crate::wake::status_text(logged),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: serde_json::Value) -> Record {
        serde_json::from_value(serde_json::json!({
            "info": {
                "name": "b", "git_branch": "by/b", "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": status, "turns": 1, "cost_usd": null, "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null
        }))
        .unwrap()
    }

    fn at(activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms: 0, activity }
    }

    #[test]
    fn the_last_status_event_is_the_records_status_or_it_is_drift() {
        let ready = record(serde_json::json!({"state": "ready"}));
        let events = vec![
            at(Activity::Status(BranchStatus::Running)),
            at(Activity::Prompt("p".into())),
            at(Activity::Status(BranchStatus::Ready)),
            at(Activity::Warning("after".into())),
        ];
        assert_eq!(check(&ready, &events), None);

        let failed = record(serde_json::json!({"state": "failed", "reason": "x"}));
        let drift = check(&failed, &events).unwrap();
        assert_eq!(drift.branch, "b");
        assert_eq!(drift.field, "status");
        assert_eq!(drift.record, "failed: x");
        assert_eq!(drift.log, "ready");
    }

    #[test]
    fn a_log_without_a_status_event_has_nothing_to_compare() {
        let ready = record(serde_json::json!({"state": "ready"}));
        assert_eq!(check(&ready, &[]), None);
        assert_eq!(check(&ready, &[at(Activity::Prompt("p".into()))]), None);
    }
}
