//! What branches still owe, as one projection (`docs/obligations.md`).
//!
//! A branch can end its turn and still owe something: a connector call
//! whose answer was lost and that nobody has asked the upstream about, a
//! staged call nobody has promoted or discarded, an undo whose window is
//! closing, a question waiting for a person, a lost turn nobody has
//! decided on, children it waits for, prerequisites, a ready child its
//! parent has not integrated. Each of those is already durable in its own
//! table: the effect ledger, the asks, the records, the leases. This
//! module reads them all and says, in one list, what is owed, since when,
//! by when, and which command deals with it. It stores nothing: an
//! obligation is discharged the moment its source changes.
//!
//! This is the obligation ledger of the treatise's three ledgers
//! (decision, effect, obligation): the one coding harnesses lack.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::effects::{short_id, EffectClass, EffectEntry, EffectState};
use crate::liveness::{Liveness, LivenessState};
use crate::state::{Record, Store};
use crate::{BranchStatus, Error};

/// What kind of thing is owed. The order is the order they are listed in:
/// what waits on a person first.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationKind {
    /// An approval ask waits for an answer; the turn is parked on it.
    Approval,
    /// A connector call whose answer was lost: it may have happened.
    /// Discharged when reconciliation confirms or fails it.
    UnknownEffect,
    /// A staged call (a draft upstream, or a call held in the outbox):
    /// it has not happened until it is promoted, and is owed a decision.
    StagedEffect,
    /// A turn its engine lost: it ended `interrupted` with its outcome
    /// unknown, and nobody has sent the branch another turn.
    LostTurn,
    /// A running turn whose engine is gone; recovery will end it.
    LostEngine,
    /// A child that cannot start: a prerequisite failed or was
    /// interrupted.
    Blocked,
    /// A delegated child that is ready and that its parent has not
    /// integrated, discarded or left to the engine to land.
    ReadyChild,
    /// A confirmed effect whose undo is possible only until a deadline.
    UndoWindow,
    /// A parent parked until its children settle; woken when they do.
    Children,
    /// A child waiting for its prerequisites; started when they settle.
    Prerequisites,
}

impl ObligationKind {
    /// The kind's name, as `--json` writes it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObligationKind::Approval => "approval",
            ObligationKind::UnknownEffect => "unknown_effect",
            ObligationKind::StagedEffect => "staged_effect",
            ObligationKind::LostTurn => "lost_turn",
            ObligationKind::LostEngine => "lost_engine",
            ObligationKind::Blocked => "blocked",
            ObligationKind::ReadyChild => "ready_child",
            ObligationKind::UndoWindow => "undo_window",
            ObligationKind::Children => "children",
            ObligationKind::Prerequisites => "prerequisites",
        }
    }
}

/// One thing a branch still owes.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Obligation {
    /// What kind of thing is owed.
    pub kind: ObligationKind,
    /// The branch that owes it. An effect's entry outlives its branch, so
    /// this may name a branch that was removed.
    pub branch: String,
    /// What is owed, in words.
    pub about: String,
    /// The effect entry's or the ask's id, when the obligation is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Since when it is owed, milliseconds since the Unix epoch, when
    /// that is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// By when it must be dealt with, milliseconds since the Unix epoch:
    /// an undo's deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<u64>,
    /// The commands that deal with it.
    pub next: Vec<String>,
}

/// Every obligation at `now_ms`, or those of `branch` and the branches
/// delegated below it; ordered by [`ObligationKind`], then branch, then
/// age.
pub(crate) fn list(
    store: &Store,
    branch: Option<&str>,
    now_ms: u64,
) -> Result<Vec<Obligation>, Error> {
    let records = store.list()?;
    let scope = match branch {
        None => None,
        Some(name) => Some(subtree(&records, name)?),
    };
    let within = |name: &str| scope.as_ref().is_none_or(|s| s.contains(name));
    let alive: HashMap<String, Liveness> =
        crate::liveness::by_branch(&store.backend().leases()?, now_ms);
    let mut owed = Vec::new();
    for ask in store.effects().asks(true)? {
        if within(&ask.branch) {
            let id = short_id(&ask.id).to_owned();
            owed.push(Obligation {
                kind: ObligationKind::Approval,
                branch: ask.branch.clone(),
                about: ask.about.describe(),
                id: Some(ask.id.clone()),
                since_ms: Some(ask.created_ms),
                due_ms: None,
                next: vec![
                    format!("by approvals allow {id}"),
                    format!("by approvals deny {id}"),
                ],
            });
        }
    }
    for entry in store.effects().effects(None)? {
        if within(&entry.branch) {
            let running = entry.state == EffectState::Begun
                && crate::effects::reconcile::turn_running(store, &entry, now_ms)?;
            owed.extend(of_effect(&entry, running, now_ms));
        }
    }
    for record in &records {
        if within(&record.info.name) {
            owed.extend(of_record(record, &records, alive.get(&record.info.name)));
        }
    }
    owed.sort_by(|a, b| {
        (a.kind, &a.branch, a.since_ms, &a.id).cmp(&(b.kind, &b.branch, b.since_ms, &b.id))
    });
    Ok(owed)
}

/// `name` and every branch delegated below it.
fn subtree(records: &[Record], name: &str) -> Result<BTreeSet<String>, Error> {
    let by_name: HashMap<&str, &Record> =
        records.iter().map(|r| (r.info.name.as_str(), r)).collect();
    if !by_name.contains_key(name) {
        return Err(Error::UnknownBranch(name.to_owned()));
    }
    let mut seen = BTreeSet::new();
    let mut queue = vec![name.to_owned()];
    while let Some(next) = queue.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        if let Some(record) = by_name.get(next.as_str()) {
            queue.extend(record.info.children.iter().cloned());
        }
    }
    Ok(seen)
}

/// What an effect entry owes, if anything. Only a `begun` entry belonging
/// to a live, held turn is in flight rather than an obligation.
fn of_effect(entry: &EffectEntry, running: bool, now_ms: u64) -> Option<Obligation> {
    let id = short_id(&entry.id).to_owned();
    let (kind, about, due_ms, next) = match entry.state {
        EffectState::Unknown => (
            ObligationKind::UnknownEffect,
            format!("{} may have happened: its answer was lost", entry.title()),
            None,
            vec![
                "by effects reconcile".to_owned(),
                format!("by effects show {id}"),
            ],
        ),
        EffectState::Begun if !running => (
            ObligationKind::UnknownEffect,
            format!(
                "{} may have happened: its turn ended before its answer",
                entry.title()
            ),
            None,
            vec![
                "by effects reconcile".to_owned(),
                format!("by effects show {id}"),
            ],
        ),
        EffectState::Staged => (
            ObligationKind::StagedEffect,
            format!("{} is staged and has not happened", entry.title()),
            None,
            vec![
                format!("by effects promote {id}"),
                format!("by approvals deny --branch {}", entry.branch),
            ],
        ),
        EffectState::Confirmed if entry.class == EffectClass::Reversible => {
            let deadline = entry.undo.as_ref()?.deadline_ms?;
            if deadline < now_ms {
                return None;
            }
            (
                ObligationKind::UndoWindow,
                format!("{} can be undone until its deadline", entry.title()),
                Some(deadline),
                vec![format!("by undo {} --plan", entry.branch)],
            )
        }
        _ => return None,
    };
    Some(Obligation {
        kind,
        branch: entry.branch.clone(),
        about,
        id: Some(entry.id.clone()),
        since_ms: Some(entry.created_ms),
        due_ms,
        next,
    })
}

/// What a branch's record owes, from its status and its lease.
fn of_record(record: &Record, records: &[Record], alive: Option<&Liveness>) -> Vec<Obligation> {
    let name = &record.info.name;
    let owe = |kind, about: String, next: Vec<String>| Obligation {
        kind,
        branch: name.clone(),
        about,
        id: None,
        since_ms: None,
        due_ms: None,
        next,
    };
    let mut owed = Vec::new();
    match &record.info.status {
        BranchStatus::Running => {
            let liveness = alive.cloned().unwrap_or_else(Liveness::unowned);
            if liveness.state == LivenessState::Lost {
                let mut lost = owe(
                    ObligationKind::LostEngine,
                    format!(
                        "its running turn has no engine: {}",
                        liveness.reason.as_deref().unwrap_or("its lease lapsed")
                    ),
                    vec![format!("by inspect {name}")],
                );
                lost.since_ms = (liveness.heard_ms != 0).then_some(liveness.heard_ms);
                owed.push(lost);
            }
        }
        BranchStatus::Interrupted if record.lost.is_some() => owed.push(owe(
            ObligationKind::LostTurn,
            "its engine stopped mid-turn; the turn's outcome is unknown and it was not \
             submitted again"
                .to_owned(),
            vec![
                format!("by send {name} \"<prompt>\""),
                format!("by send {name} --retry"),
            ],
        )),
        BranchStatus::Blocked { reason } => owed.push(owe(
            ObligationKind::Blocked,
            format!("cannot start: {reason}"),
            graph_next(record),
        )),
        BranchStatus::WaitingOnChildren => {
            let waiting = record
                .info
                .children
                .iter()
                .filter(|child| {
                    records
                        .iter()
                        .any(|r| &r.info.name == *child && crate::wake::unsettled(&r.info.status))
                })
                .count();
            owed.push(owe(
                ObligationKind::Children,
                format!("parked until its children settle ({waiting} still running or waiting)"),
                vec![format!("by children {name}")],
            ));
        }
        BranchStatus::Waiting => owed.push(owe(
            ObligationKind::Prerequisites,
            "waits for its prerequisites; it starts when they settle".to_owned(),
            graph_next(record),
        )),
        BranchStatus::Ready if record.info.depth > 0 && record.landing.is_none() => {
            if let Some(parent) = &record.info.parent {
                owed.push(owe(
                    ObligationKind::ReadyChild,
                    format!("ready, and not yet integrated into {parent}"),
                    vec![format!("by integrate {name}"), format!("by discard {name}")],
                ));
            }
        }
        _ => {}
    }
    owed
}

/// The command that shows a child's place in its parent's graph.
fn graph_next(record: &Record) -> Vec<String> {
    match &record.info.parent {
        Some(parent) => vec![format!("by graph show {parent}")],
        None => vec![format!("by inspect {}", record.info.name)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{Undo, UndoKind};

    fn entry(state: EffectState, class: EffectClass) -> EffectEntry {
        serde_json::from_value(serde_json::json!({
            "id": "01HZZZZZZZZZZZZZZZZZABCDEFGH",
            "task": "t", "branch": "b", "turn": 1, "subject": "me",
            "connector": "slack", "operation": "chat_post",
            "class": class, "state": state,
            "request_digest": "blake3:x",
            "created_ms": 5, "updated_ms": 5
        }))
        .unwrap()
    }

    #[test]
    fn an_effect_owes_what_its_state_leaves_open() {
        let none = false;
        let unknown = of_effect(
            &entry(EffectState::Unknown, EffectClass::Irreversible),
            none,
            10,
        )
        .unwrap();
        assert_eq!(unknown.kind, ObligationKind::UnknownEffect);
        assert_eq!(unknown.next[0], "by effects reconcile");
        assert_eq!(unknown.next[1], "by effects show ABCDEFGH");
        assert_eq!(unknown.since_ms, Some(5));

        // In flight only while its own turn has a live held lease.
        let begun = entry(EffectState::Begun, EffectClass::Irreversible);
        let running = true;
        assert_eq!(of_effect(&begun, running, 10), None);
        assert_eq!(
            of_effect(&begun, none, 10).unwrap().kind,
            ObligationKind::UnknownEffect
        );

        let staged = of_effect(
            &entry(EffectState::Staged, EffectClass::Irreversible),
            none,
            10,
        )
        .unwrap();
        assert_eq!(staged.kind, ObligationKind::StagedEffect);
        assert_eq!(staged.next[0], "by effects promote ABCDEFGH");

        for settled in [
            EffectState::Failed,
            EffectState::Undone,
            EffectState::Expired,
        ] {
            assert_eq!(
                of_effect(&entry(settled, EffectClass::Reversible), none, 10),
                None
            );
        }
    }

    fn record(name: &str, status: serde_json::Value, parent: Option<&str>) -> Record {
        serde_json::from_value(serde_json::json!({
            "info": {
                "name": name, "git_branch": format!("by/{name}"), "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": parent, "depth": u32::from(parent.is_some()),
                "base": "b", "candidate": null,
                "status": status, "turns": 1, "cost_usd": null, "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null
        }))
        .unwrap()
    }

    fn kinds(record: &Record, records: &[Record], alive: Option<&Liveness>) -> Vec<ObligationKind> {
        of_record(record, records, alive)
            .iter()
            .map(|o| o.kind)
            .collect()
    }

    #[test]
    fn a_record_owes_what_its_status_leaves_open() {
        use serde_json::json;
        let mut lead = record("lead", json!({"state": "waiting_on_children"}), None);
        lead.info.children = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let a = record("a", json!({"state": "ready"}), Some("lead"));
        let b = record("b", json!({"state": "waiting"}), Some("lead"));
        let c = record(
            "c",
            json!({"state": "blocked", "reason": "a prerequisite failed"}),
            Some("lead"),
        );
        let d = record("d", json!({"state": "running"}), Some("lead"));
        let all = [lead.clone(), a.clone(), b.clone(), c.clone(), d.clone()];

        let parked = of_record(&lead, &all, None);
        assert_eq!(parked[0].kind, ObligationKind::Children);
        assert!(
            parked[0].about.contains("(2 still running or waiting)"),
            "{parked:?}"
        );
        assert_eq!(parked[0].next, ["by children lead"]);
        assert_eq!(kinds(&a, &all, None), [ObligationKind::ReadyChild]);
        assert_eq!(
            of_record(&a, &all, None)[0].next,
            ["by integrate a", "by discard a"]
        );
        assert_eq!(kinds(&b, &all, None), [ObligationKind::Prerequisites]);
        assert_eq!(of_record(&c, &all, None)[0].next, ["by graph show lead"]);
        assert_eq!(kinds(&c, &all, None), [ObligationKind::Blocked]);

        // A running turn owes nothing while its engine is heard from.
        let live = Liveness {
            state: LivenessState::Live,
            heard_ms: 1,
            known_until_ms: 30_001,
            reason: None,
        };
        assert!(kinds(&d, &all, Some(&live)).is_empty());
        let orphaned = of_record(&d, &all, None);
        assert_eq!(orphaned[0].kind, ObligationKind::LostEngine);
        assert_eq!(orphaned[0].since_ms, None);
        assert!(orphaned[0].about.contains("no held lease"));
        let lost = Liveness {
            state: LivenessState::Lost,
            reason: Some("its engine's lease expired 3s ago".into()),
            ..live
        };
        let owed = of_record(&d, &all, Some(&lost));
        assert_eq!(owed[0].kind, ObligationKind::LostEngine);
        assert_eq!(owed[0].since_ms, Some(1));

        // A ready branch you started, and one the engine lands, owe nothing.
        let mut top = record("top", json!({"state": "ready"}), None);
        assert!(kinds(&top, &all, None).is_empty());
        top.info.status = BranchStatus::Interrupted;
        assert!(kinds(&top, &all, None).is_empty(), "interrupted, not lost");
        top.lost = Some("the engine stopped".into());
        assert_eq!(kinds(&top, &all, None), [ObligationKind::LostTurn]);
    }

    #[test]
    fn a_subtree_is_a_branch_and_everything_delegated_below_it() {
        use serde_json::json;
        let mut lead = record("lead", json!({"state": "ready"}), None);
        lead.info.children = vec!["a".into()];
        let mut a = record("a", json!({"state": "ready"}), Some("lead"));
        a.info.children = vec!["a1".into()];
        let a1 = record("a1", json!({"state": "ready"}), Some("a"));
        let other = record("other", json!({"state": "ready"}), None);
        let all = [lead, a, a1, other];
        let names: Vec<String> = subtree(&all, "lead").unwrap().into_iter().collect();
        assert_eq!(names, ["a", "a1", "lead"]);
        assert!(matches!(
            subtree(&all, "nope"),
            Err(Error::UnknownBranch(_))
        ));
    }

    #[test]
    fn an_undo_window_is_owed_until_its_deadline_and_only_with_one() {
        let none = false;
        let mut confirmed = entry(EffectState::Confirmed, EffectClass::Reversible);
        assert_eq!(of_effect(&confirmed, none, 10), None, "no undo");
        confirmed.undo = Some(Undo {
            kind: UndoKind::Inverse,
            operation: "slack.chat.delete".into(),
            tool: "slack__chat_delete".into(),
            arguments: serde_json::json!({}),
            deadline_ms: Some(100),
        });
        let window = of_effect(&confirmed, none, 10).unwrap();
        assert_eq!(window.kind, ObligationKind::UndoWindow);
        assert_eq!(window.due_ms, Some(100));
        assert_eq!(window.next, ["by undo b --plan"]);
        assert_eq!(of_effect(&confirmed, none, 101), None, "it closed");
    }
}
