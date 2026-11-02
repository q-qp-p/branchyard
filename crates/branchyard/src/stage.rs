//! What a branch needs from a person now, and why: [`BranchInfo::stage`].
//! Derived on read, like [`BranchInfo::awaiting`], and never stored.
//! [`STAGES`] is the table in precedence order, and the first row that
//! matches a branch is its stage (`docs/concepts.md`, "What a branch needs
//! from you").

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{wake, BranchInfo, BranchStatus, Message, MessageKind, PERSON};

/// The column of the console a branch sits in.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// It waits on a person.
    NeedsYou,
    /// It is working, or will start again on its own.
    Running,
    /// Its candidate waits to be merged.
    Ready,
    /// Nothing more will happen to it.
    Done,
}

/// Why a branch is in its lane: one per row of [`STAGES`], declared in
/// precedence order.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, strum::EnumIter)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Its running turn is parked on an approval ask.
    Approval,
    /// It sent a question or escalation nobody has answered, and it is
    /// running or it asked the person.
    Question,
    /// It is running, and its harness has gone silent.
    Stalled,
    /// A settled delegated child whose parent still works.
    Parent,
    /// Its plan waits for approval.
    AwaitingPlanApproval,
    /// Its turn failed.
    Failed,
    /// Its turn was cancelled or its engine stopped.
    Interrupted,
    /// A limit stopped it.
    BudgetExceeded,
    /// A prerequisite did not succeed.
    Blocked,
    /// A turn is in progress.
    Running,
    /// Parked until its children settle.
    WaitingOnChildren,
    /// Waiting for its prerequisites.
    Waiting,
    /// Its candidate waits to be merged.
    Ready,
    /// Its last turn changed no file.
    NoChanges,
    /// Integrated.
    Merged,
    /// Set aside on purpose.
    Discarded,
}

/// Every [`Reason`] in precedence order, with its lane and when it applies.
/// `docs/concepts.md` renders this table (`tools/docs_facts.py`).
pub const STAGES: [(Reason, Lane, &str); 16] = [
    (
        Reason::Approval,
        Lane::NeedsYou,
        "Its running turn is parked on an approval ask. It moves only when you allow or deny it.",
    ),
    (
        Reason::Question,
        Lane::NeedsYou,
        "It sent a question or escalation that nobody has answered, and it is running or it \
         asked you.",
    ),
    (
        Reason::Stalled,
        Lane::NeedsYou,
        "It is running, and its harness has been silent for its stall window.",
    ),
    (
        Reason::Parent,
        Lane::Running,
        "It is a delegated child that settled without being merged or discarded, and its \
         parent still runs or waits. Its parent decides what comes next.",
    ),
    (
        Reason::AwaitingPlanApproval,
        Lane::NeedsYou,
        "Its plan waits for you to approve, edit or reject it.",
    ),
    (Reason::Failed, Lane::NeedsYou, "Its turn failed."),
    (
        Reason::Interrupted,
        Lane::NeedsYou,
        "Its turn was cancelled, or the engine running it stopped.",
    ),
    (
        Reason::BudgetExceeded,
        Lane::NeedsYou,
        "A limit you set stopped it.",
    ),
    (
        Reason::Blocked,
        Lane::NeedsYou,
        "A prerequisite did not succeed, so it will not start until its graph changes.",
    ),
    (Reason::Running, Lane::Running, "A turn is in progress."),
    (
        Reason::WaitingOnChildren,
        Lane::Running,
        "Its turn ended while its children still ran. It starts again on its own when they \
         settle.",
    ),
    (
        Reason::Waiting,
        Lane::Running,
        "It waits for its prerequisites to settle.",
    ),
    (
        Reason::Ready,
        Lane::Ready,
        "Its candidate waits to be merged.",
    ),
    (
        Reason::NoChanges,
        Lane::Done,
        "Its last turn changed no file.",
    ),
    (Reason::Merged, Lane::Done, "Its candidate was integrated."),
    (
        Reason::Discarded,
        Lane::Done,
        "It was set aside on purpose.",
    ),
];

impl Reason {
    /// The row a branch in `status` reaches when no row above the status
    /// rows matches it.
    pub fn of_status(status: &BranchStatus) -> Reason {
        match status {
            BranchStatus::Running => Reason::Running,
            BranchStatus::Waiting => Reason::Waiting,
            BranchStatus::Blocked { .. } => Reason::Blocked,
            BranchStatus::Ready => Reason::Ready,
            BranchStatus::AwaitingPlanApproval => Reason::AwaitingPlanApproval,
            BranchStatus::WaitingOnChildren => Reason::WaitingOnChildren,
            BranchStatus::NoChanges => Reason::NoChanges,
            BranchStatus::Interrupted => Reason::Interrupted,
            BranchStatus::BudgetExceeded { .. } => Reason::BudgetExceeded,
            BranchStatus::Failed { .. } => Reason::Failed,
            BranchStatus::Merged { .. } => Reason::Merged,
            BranchStatus::Discarded { .. } => Reason::Discarded,
        }
    }

    /// The lane a branch in this row sits in.
    pub fn lane(self) -> Lane {
        match self {
            Reason::Approval
            | Reason::Question
            | Reason::Stalled
            | Reason::AwaitingPlanApproval
            | Reason::Failed
            | Reason::Interrupted
            | Reason::BudgetExceeded
            | Reason::Blocked => Lane::NeedsYou,
            Reason::Parent | Reason::Running | Reason::WaitingOnChildren | Reason::Waiting => {
                Lane::Running
            }
            Reason::Ready => Lane::Ready,
            Reason::NoChanges | Reason::Merged | Reason::Discarded => Lane::Done,
        }
    }
}

/// A branch's lane and why: [`BranchInfo::stage`]. Serialized as
/// `{"lane": "needs_you", "reason": "approval"}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// The lane, always [`Reason::lane`] of `reason`.
    pub lane: Lane,
    /// The first row of [`STAGES`] that matched.
    pub reason: Reason,
}

impl From<Reason> for Stage {
    fn from(reason: Reason) -> Stage {
        Stage {
            lane: reason.lane(),
            reason,
        }
    }
}

/// What [`classify`] reads about one branch.
pub(crate) struct Facts<'a> {
    pub(crate) info: &'a BranchInfo,
    /// Its oldest unanswered message that [`counts_as_asking`].
    pub(crate) asking: Option<&'a Message>,
    /// Its parent's status, when it has a parent.
    pub(crate) parent: Option<&'a BranchStatus>,
}

/// The first row of [`STAGES`] that matches.
pub(crate) fn classify(facts: &Facts<'_>) -> Stage {
    let info = facts.info;
    let reason = if info.awaiting.is_some() {
        Reason::Approval
    } else if facts.asking.is_some() {
        Reason::Question
    } else if info.stalled && info.status == BranchStatus::Running {
        Reason::Stalled
    } else if info.depth > 0
        && wake::awaits_parent(&info.status)
        && facts.parent.is_some_and(wake::unsettled)
    {
        Reason::Parent
    } else {
        Reason::of_status(&info.status)
    };
    Stage::from(reason)
}

/// Whether an unanswered `message` from a branch in `asker` status asks
/// for an answer now: a question or escalation, while its asker runs, or,
/// when it went to the person, until its asker is merged or discarded,
/// since nobody else will answer it.
pub(crate) fn counts_as_asking(message: &Message, asker: &BranchStatus) -> bool {
    let set_aside = matches!(
        asker,
        BranchStatus::Merged { .. } | BranchStatus::Discarded { .. }
    );
    matches!(
        message.kind,
        MessageKind::Question | MessageKind::Escalation
    ) && ((message.to == PERSON && !set_aside) || *asker == BranchStatus::Running)
}

/// Per asker, the first of the unanswered `questions` (oldest first) that
/// [`counts_as_asking`], given each asker's status (`None` for a branch
/// not known): the message its [`Reason::Question`] row counts.
pub(crate) fn asking<'a>(
    questions: &[Message],
    status_of: impl Fn(&str) -> Option<&'a BranchStatus>,
) -> HashMap<String, Message> {
    let mut asking = HashMap::new();
    for message in questions {
        if !asking.contains_key(&message.from)
            && status_of(&message.from).is_some_and(|s| counts_as_asking(message, s))
        {
            asking.insert(message.from.clone(), message.clone());
        }
    }
    asking
}

/// `info` with its stage, given the message it is asking and its parent's
/// status.
pub(crate) fn with_stage(
    mut info: BranchInfo,
    asking: Option<&Message>,
    parent: Option<&BranchStatus>,
) -> BranchInfo {
    info.stage = Some(classify(&Facts {
        info: &info,
        asking,
        parent,
    }));
    info
}

/// `infos`, a whole list with each one's `awaiting` already set, each with
/// its stage, given the unanswered `questions` oldest first.
pub(crate) fn staged(infos: Vec<BranchInfo>, questions: &[Message]) -> Vec<BranchInfo> {
    let statuses: HashMap<String, BranchStatus> = infos
        .iter()
        .map(|info| (info.name.clone(), info.status.clone()))
        .collect();
    let asking = asking(questions, |name| statuses.get(name));
    infos
        .into_iter()
        .map(|info| {
            let parent = info.parent.as_ref().and_then(|p| statuses.get(p));
            let asks = asking.get(&info.name);
            with_stage(info, asks, parent)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;
    use crate::{Awaiting, AwaitingKind};

    fn info(status: BranchStatus) -> BranchInfo {
        serde_json::from_value(serde_json::json!({
            "name": "b", "git_branch": "by/b", "worktree": "/w", "prompt": "p",
            "harness": "fake", "profile": "p", "base": "main",
            "status": status, "turns": 1, "created_at": 0
        }))
        .unwrap()
    }

    fn parked(mut info: BranchInfo) -> BranchInfo {
        info.awaiting = Some(Awaiting {
            kind: AwaitingKind::Approval,
            id: "ask-1".into(),
            about: "tool write marker".into(),
            since_ms: 0,
        });
        info
    }

    fn stalled() -> BranchInfo {
        BranchInfo {
            stalled: true,
            ..info(BranchStatus::Running)
        }
    }

    fn child(status: BranchStatus) -> BranchInfo {
        BranchInfo {
            parent: Some("p".into()),
            depth: 1,
            ..info(status)
        }
    }

    fn message(kind: MessageKind) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": 1, "from": "b", "to": "p", "kind": kind, "text": "which?", "at_ms": 0
        }))
        .unwrap()
    }

    fn classified(
        info: &BranchInfo,
        asking: Option<&Message>,
        parent: Option<&BranchStatus>,
    ) -> Reason {
        classify(&Facts {
            info,
            asking,
            parent,
        })
        .reason
    }

    /// A branch that only `reason`'s row matches, with what it asks and its
    /// parent's status.
    fn fixture(reason: Reason) -> (BranchInfo, Option<Message>, Option<BranchStatus>) {
        let failed = || BranchStatus::Failed { reason: "x".into() };
        match reason {
            Reason::Approval => (parked(info(BranchStatus::Running)), None, None),
            Reason::Question => (
                info(BranchStatus::Running),
                Some(message(MessageKind::Question)),
                None,
            ),
            Reason::Stalled => (stalled(), None, None),
            Reason::Parent => (
                child(BranchStatus::Ready),
                None,
                Some(BranchStatus::Running),
            ),
            Reason::AwaitingPlanApproval => (info(BranchStatus::AwaitingPlanApproval), None, None),
            Reason::Failed => (info(failed()), None, None),
            Reason::Interrupted => (info(BranchStatus::Interrupted), None, None),
            Reason::BudgetExceeded => (
                info(BranchStatus::BudgetExceeded {
                    limit: "max_turns".into(),
                }),
                None,
                None,
            ),
            Reason::Blocked => (
                info(BranchStatus::Blocked { reason: "x".into() }),
                None,
                None,
            ),
            Reason::Running => (info(BranchStatus::Running), None, None),
            Reason::WaitingOnChildren => (info(BranchStatus::WaitingOnChildren), None, None),
            Reason::Waiting => (info(BranchStatus::Waiting), None, None),
            Reason::Ready => (info(BranchStatus::Ready), None, None),
            Reason::NoChanges => (info(BranchStatus::NoChanges), None, None),
            Reason::Merged => (
                child(BranchStatus::Merged {
                    target: "main".into(),
                    commit: "c".into(),
                }),
                None,
                Some(BranchStatus::Running),
            ),
            Reason::Discarded => (
                child(BranchStatus::Discarded { reason: "x".into() }),
                None,
                Some(BranchStatus::Running),
            ),
        }
    }

    #[test]
    fn every_row_has_a_branch_classified_to_it() {
        for reason in Reason::iter() {
            let (info, asking, parent) = fixture(reason);
            assert_eq!(classified(&info, asking.as_ref(), parent.as_ref()), reason);
        }
    }

    #[test]
    fn the_table_is_the_reasons_in_order_with_their_lanes() {
        let rows: Vec<Reason> = STAGES.iter().map(|(reason, _, _)| *reason).collect();
        assert_eq!(rows, Reason::iter().collect::<Vec<_>>());
        for (reason, lane, condition) in STAGES {
            assert_eq!(lane, reason.lane(), "{reason:?}");
            assert!(
                !condition.contains([':', '\u{2014}']),
                "{reason:?}: {condition}"
            );
        }
    }

    #[test]
    fn a_stage_is_a_lane_and_a_reason_on_the_wire() {
        assert_eq!(
            serde_json::to_value(Stage::from(Reason::Approval)).unwrap(),
            serde_json::json!({"lane": "needs_you", "reason": "approval"})
        );
        assert_eq!(
            serde_json::to_value(Stage::from(Reason::WaitingOnChildren)).unwrap(),
            serde_json::json!({"lane": "running", "reason": "waiting_on_children"})
        );
    }

    #[test]
    fn approval_comes_before_a_question() {
        let question = message(MessageKind::Question);
        let info = parked(info(BranchStatus::Running));
        assert_eq!(classified(&info, Some(&question), None), Reason::Approval);
    }

    #[test]
    fn a_question_comes_before_a_stall() {
        let question = message(MessageKind::Escalation);
        assert_eq!(
            classified(&stalled(), Some(&question), None),
            Reason::Question
        );
    }

    #[test]
    fn a_question_that_counts_comes_before_ready() {
        let question = message(MessageKind::Question);
        assert_eq!(
            classified(&info(BranchStatus::Ready), Some(&question), None),
            Reason::Question
        );
    }

    #[test]
    fn approval_comes_before_parent() {
        let info = parked(child(BranchStatus::Ready));
        let parent = BranchStatus::Running;
        assert_eq!(classified(&info, None, Some(&parent)), Reason::Approval);
    }

    #[test]
    fn only_a_running_askers_question_or_escalation_counts() {
        let running = BranchStatus::Running;
        assert!(counts_as_asking(&message(MessageKind::Question), &running));
        assert!(counts_as_asking(
            &message(MessageKind::Escalation),
            &running
        ));
        assert!(!counts_as_asking(&message(MessageKind::Report), &running));
        assert!(!counts_as_asking(&message(MessageKind::Answer), &running));
        for settled in [
            BranchStatus::Ready,
            BranchStatus::WaitingOnChildren,
            BranchStatus::Interrupted,
        ] {
            assert!(!counts_as_asking(&message(MessageKind::Question), &settled));
        }
    }

    #[test]
    fn a_settled_branch_asks_only_when_it_asked_the_person() {
        let to_person = Message {
            to: PERSON.into(),
            ..message(MessageKind::Question)
        };
        let root = staged(
            vec![info(BranchStatus::Interrupted)],
            std::slice::from_ref(&to_person),
        );
        assert_eq!(root[0].stage.map(|s| s.reason), Some(Reason::Question));
        let discarded = BranchStatus::Discarded {
            reason: "not needed".into(),
        };
        let root = staged(vec![info(discarded)], &[to_person]);
        assert_eq!(root[0].stage.map(|s| s.reason), Some(Reason::Discarded));
        let kid = staged(
            vec![child(BranchStatus::Ready)],
            &[message(MessageKind::Question)],
        );
        assert_eq!(kid[0].stage.map(|s| s.reason), Some(Reason::Ready));
    }

    #[test]
    fn each_asker_asks_its_oldest_message_that_counts() {
        let sent = |id, from: &str, kind| Message {
            id,
            from: from.into(),
            ..message(kind)
        };
        let questions = [
            sent(1, "ended", MessageKind::Question),
            sent(2, "b", MessageKind::Report),
            sent(3, "b", MessageKind::Escalation),
            sent(4, "b", MessageKind::Question),
            sent(5, "gone", MessageKind::Question),
        ];
        let (running, ready) = (BranchStatus::Running, BranchStatus::Ready);
        let asking = asking(&questions, |name| match name {
            "b" => Some(&running),
            "ended" => Some(&ready),
            _ => None,
        });
        let ids: Vec<(&str, u64)> = asking.iter().map(|(b, m)| (b.as_str(), m.id)).collect();
        assert_eq!(ids, [("b", 3)]);
    }

    #[test]
    fn a_child_whose_parent_settled_falls_to_its_own_status() {
        let child = child(BranchStatus::Ready);
        for parent in [BranchStatus::Ready, BranchStatus::Interrupted] {
            assert_eq!(classified(&child, None, Some(&parent)), Reason::Ready);
        }
        assert_eq!(classified(&child, None, None), Reason::Ready);
        for parent in [BranchStatus::Waiting, BranchStatus::WaitingOnChildren] {
            assert_eq!(classified(&child, None, Some(&parent)), Reason::Parent);
        }
    }

    #[test]
    fn a_branch_at_depth_zero_is_never_parent() {
        let fork = BranchInfo {
            parent: Some("p".into()),
            ..info(BranchStatus::Ready)
        };
        let parent = BranchStatus::Running;
        assert_eq!(classified(&fork, None, Some(&parent)), Reason::Ready);
    }
}
