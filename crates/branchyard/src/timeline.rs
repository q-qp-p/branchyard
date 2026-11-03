//! The shape of a branch's work over time, without its content.
//!
//! A client that draws many branches at once (the console's board) needs
//! when each turn began and ended, when each tool call started and what it
//! was called, what each turn cost and when the branch merged. Reading that
//! from [`Branch::events`](crate::Branch::events) meant reading every
//! prompt, message, tool input and provisioning record, and a page of
//! recent events dropped the oldest turns of a long branch. [`Timeline::of`]
//! reduces the whole log to that shape once, on the side that holds it.
//!
//! It is the second of four levels a client reads a branch at, each asked
//! for when it is needed: [`BranchInfo`](crate::BranchInfo) (status,
//! counts, cost), this timeline, a page of events (the conversation), and
//! the diff or the inspection. See `docs/server.md`.

use serde::{Deserialize, Serialize};

use crate::{Activity, BranchStatus, Event, RecordedEvent};

/// A branch's turns and merge, as [`Timeline::of`] reads them.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    /// Every turn, oldest first.
    pub turns: Vec<TimelineTurn>,
    /// When the branch merged, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at_ms: Option<u64>,
}

/// One turn: from its prompt to the status that ended it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineTurn {
    /// When the prompt was submitted, or when the first tool call started
    /// for a turn whose prompt is not in the log.
    pub started_at_ms: u64,
    /// When a status other than running or waiting ended it; absent while
    /// it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// The status that ended it, such as `ready`, or `failed` with its
    /// reason; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BranchStatus>,
    /// What the turn cost on its own, when something measured it
    /// ([`Activity::TurnCost`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Its tool calls, in the order they started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<TimelineTool>,
}

/// A tool call's start.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTool {
    /// When it started.
    pub at_ms: u64,
    /// The tool's name, as the harness reported it.
    pub name: String,
    /// The harness's ID for the call. With [`TimelineTool::at_ms`] it
    /// names the call, so a client that keeps a timeline current from the
    /// event stream can tell a call it already has from a new one.
    pub call_id: String,
}

impl Timeline {
    /// Read the timeline from a branch's whole event log, oldest first.
    ///
    /// A prompt starts a turn; steered input into a running turn does not,
    /// and steered input after the turn ended starts one. A tool call with
    /// no turn open starts one at the call. A status other than running or
    /// waiting ends the open turn, and `merged` is also the merge time.
    pub fn of(events: &[RecordedEvent]) -> Timeline {
        let mut timeline = Timeline::default();
        let mut open = false;
        for event in events {
            let at = event.at_ms;
            match &event.activity {
                Activity::Prompt(_) => {
                    timeline.turns.push(TimelineTurn::starting(at));
                    open = true;
                }
                Activity::Steered { .. } if !open => {
                    timeline.turns.push(TimelineTurn::starting(at));
                    open = true;
                }
                Activity::Harness(Event::ToolStarted { name, call_id, .. }) => {
                    if !open {
                        timeline.turns.push(TimelineTurn::starting(at));
                        open = true;
                    }
                    if let Some(turn) = timeline.turns.last_mut() {
                        turn.tools.push(TimelineTool {
                            at_ms: at,
                            name: name.clone(),
                            call_id: call_id.clone(),
                        });
                    }
                }
                Activity::TurnCost { cost_usd } => {
                    if let Some(turn) = timeline.turns.last_mut() {
                        turn.cost_usd = Some(*cost_usd);
                    }
                }
                Activity::Status(status) => {
                    if matches!(status, BranchStatus::Merged { .. }) {
                        timeline.merged_at_ms = Some(at);
                    }
                    if open && !matches!(status, BranchStatus::Running | BranchStatus::Waiting) {
                        if let Some(turn) = timeline.turns.last_mut() {
                            turn.ended_at_ms = Some(at);
                            turn.outcome = Some(status.clone());
                        }
                        open = false;
                    }
                }
                _ => {}
            }
        }
        timeline
    }
}

impl TimelineTurn {
    fn starting(at_ms: u64) -> TimelineTurn {
        TimelineTurn {
            started_at_ms: at_ms,
            ended_at_ms: None,
            outcome: None,
            cost_usd: None,
            tools: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn tool(turn: u64, name: &str) -> Activity {
        Activity::Harness(Event::ToolStarted {
            turn,
            call_id: format!("{name}-{turn}"),
            name: name.into(),
        })
    }

    #[test]
    fn each_prompt_is_a_turn_with_its_calls_cost_and_end() {
        let timeline = Timeline::of(&[
            at(10, Activity::Status(BranchStatus::Running)),
            at(11, Activity::Prompt("fix it".into())),
            at(12, tool(1, "Read")),
            at(13, tool(1, "Edit")),
            at(14, Activity::TurnCost { cost_usd: 0.25 }),
            at(15, Activity::Status(BranchStatus::Ready)),
            at(20, Activity::Prompt("and the docs".into())),
            at(21, Activity::Status(BranchStatus::Running)),
        ]);
        assert_eq!(timeline.turns.len(), 2);
        let first = &timeline.turns[0];
        assert_eq!((first.started_at_ms, first.ended_at_ms), (11, Some(15)));
        assert_eq!(first.outcome, Some(BranchStatus::Ready));
        assert_eq!(first.cost_usd, Some(0.25));
        let names: Vec<_> = first
            .tools
            .iter()
            .map(|t| (t.at_ms, t.name.as_str()))
            .collect();
        assert_eq!(names, [(12, "Read"), (13, "Edit")]);
        assert_eq!(first.tools[0].call_id, "Read-1");
        let second = &timeline.turns[1];
        assert_eq!(
            (second.started_at_ms, second.ended_at_ms),
            (20, None),
            "still running"
        );
        assert_eq!(timeline.merged_at_ms, None);
    }

    #[test]
    fn steering_a_running_turn_is_not_a_new_turn_but_steering_after_it_is() {
        let steer = |id| Activity::Steered {
            id,
            by: "person".into(),
            text: "also this".into(),
        };
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(2, steer(1)),
            at(3, Activity::Status(BranchStatus::Ready)),
            at(4, steer(2)),
            at(5, Activity::Status(BranchStatus::Ready)),
        ]);
        let spans: Vec<_> = timeline
            .turns
            .iter()
            .map(|t| (t.started_at_ms, t.ended_at_ms))
            .collect();
        assert_eq!(spans, [(1, Some(3)), (4, Some(5))]);
    }

    #[test]
    fn a_tool_call_without_its_prompt_opens_a_turn_and_merging_is_recorded() {
        let timeline = Timeline::of(&[
            at(5, tool(1, "Bash")),
            at(6, Activity::Status(BranchStatus::Ready)),
            at(
                9,
                Activity::Status(BranchStatus::Merged {
                    target: "main".into(),
                    commit: "abc".into(),
                }),
            ),
        ]);
        assert_eq!(timeline.turns.len(), 1);
        assert_eq!(timeline.turns[0].started_at_ms, 5);
        assert_eq!(timeline.turns[0].tools.len(), 1);
        assert_eq!(timeline.merged_at_ms, Some(9));
        assert_eq!(
            timeline.turns[0].ended_at_ms,
            Some(6),
            "the merge does not move a closed turn"
        );
    }

    #[test]
    fn a_failed_turn_ends_with_its_outcome_and_no_cost() {
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(
                2,
                Activity::Status(BranchStatus::Failed {
                    reason: "boom".into(),
                }),
            ),
        ]);
        assert_eq!(
            timeline.turns[0].outcome,
            Some(BranchStatus::Failed {
                reason: "boom".into()
            })
        );
        assert_eq!(timeline.turns[0].cost_usd, None);
    }

    #[test]
    fn the_wire_leaves_out_what_is_absent() {
        let timeline = Timeline::of(&[at(1, Activity::Prompt("go".into()))]);
        let wire = serde_json::to_value(&timeline).unwrap();
        assert_eq!(wire, serde_json::json!({"turns": [{"started_at_ms": 1}]}));
        assert_eq!(serde_json::from_value::<Timeline>(wire).unwrap(), timeline);
    }

    #[test]
    fn an_empty_log_is_an_empty_timeline() {
        assert_eq!(Timeline::of(&[]), Timeline::default());
    }
}
