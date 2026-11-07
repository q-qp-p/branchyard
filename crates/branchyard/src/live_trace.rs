//! Live spans of a turn, opened and closed as the turn's events are
//! recorded.
//!
//! A turn's recorder (`record::Recorder`) feeds every activity it
//! appends to a `LiveTrace`, which keeps one `turn` span open from the
//! prompt to the turn's end, and under it the `tool`, `permission wait`,
//! `steer` and `stall` spans for as long as each lasts. The spans are
//! `tracing` spans of the trace target, built only when the process exports
//! spans ([`branchyard_support::trace::enabled`]); with telemetry off
//! nothing here allocates a span. Attributes follow the OpenTelemetry
//! GenAI conventions where one fits (`gen_ai.operation.name`,
//! `gen_ai.agent.name`, `gen_ai.request.model`, `gen_ai.usage.*`,
//! `error.type`); a tool call carries the harness's bounded, scrubbed
//! [`ToolTarget`](branchyard_harness::ToolTarget) and never its inputs.
//!
//! A turn that had live spans is remembered ([`was_live`]) so the server,
//! which writes spans after the fact for turns it has no live data for,
//! does not write a second set for it: one turn is traced once.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use branchyard_harness::Event;
use branchyard_support::trace;
use branchyard_support::trace_span as span;
use branchyard_support::LockExt as _;
use tracing::field::Empty;
use tracing::Span;

use crate::{Activity, BranchStatus};

/// Turns remembered as traced live, until their after-the-fact pass asks.
const REMEMBERED: usize = 8192;

/// A live turn's `traceparent`, by branch, for the spans other threads
/// open under it (a delegation, a connector call).
static PARENTS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// `(branch, prompt time)` of turns that had live spans.
static LIVE: Mutex<Option<HashSet<(String, u64)>>> = Mutex::new(None);

/// The `traceparent` of `branch`'s live turn span, while a turn runs and
/// spans are exported.
pub(crate) fn turn_traceparent(branch: &str) -> Option<String> {
    PARENTS
        .lock_recovering("live turn parents")
        .as_ref()
        .and_then(|m| m.get(branch).cloned())
}

/// Whether the turn of `branch` whose prompt was recorded at `prompt_ms`
/// was traced live in this process. Asking forgets the turn.
pub fn was_live(branch: &str, prompt_ms: u64) -> bool {
    LIVE.lock_recovering("live turns")
        .as_mut()
        .is_some_and(|set| set.remove(&(branch.to_owned(), prompt_ms)))
}

fn remember_live(branch: &str, prompt_ms: u64) {
    let mut guard = LIVE.lock_recovering("live turns");
    let set = guard.get_or_insert_with(HashSet::new);
    if set.len() >= REMEMBERED {
        set.clear();
    }
    set.insert((branch.to_owned(), prompt_ms));
}

/// The `kind` (or `state`) tag of a serde-tagged value.
pub(crate) fn tag(value: &impl serde::Serialize, key: &str) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.get(key).and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn is_error(outcome: &str) -> bool {
    !matches!(outcome, "completed" | "ready" | "no_changes")
}

/// Cut `text` to a bounded length for a span's status description.
fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// A tool's name for a span. Some harnesses title a shell call with the
/// command itself; a name with spaces next to a target is such a title, and
/// the span says `command` (the scrubbed target is its own attribute)
/// rather than copy the text.
pub(crate) fn tool_name(name: &str, has_target: bool) -> String {
    if has_target && name.contains(char::is_whitespace) {
        return "command".to_owned();
    }
    name.chars().take(80).collect()
}

struct Turn {
    span: Span,
    tool: Option<Span>,
    permission: Option<Span>,
    stall: Option<Span>,
    steers: Vec<(u64, Span)>,
    input: u64,
    output: u64,
    model: Option<String>,
}

/// The live spans of one branch's turn.
pub(crate) struct LiveTrace {
    branch: String,
    harness: String,
    parent: Option<String>,
    turn: Option<Turn>,
}

impl LiveTrace {
    /// A trace for `branch`'s turn run by `harness`, a child of `parent`
    /// (the operation's `traceparent`) when there is one, else of the span
    /// in scope.
    pub(crate) fn new(branch: &str, harness: &str, parent: Option<&str>) -> LiveTrace {
        LiveTrace {
            branch: branch.to_owned(),
            harness: harness.to_owned(),
            parent: parent.map(str::to_owned),
            turn: None,
        }
    }

    fn start(&mut self, at_ms: u64) {
        if !trace::enabled() {
            return;
        }
        let span = span!(
            "turn",
            "by.branch" = self.branch.as_str(),
            "by.harness" = self.harness.as_str(),
            "by.live" = true,
            "by.turn" = Empty,
            "by.outcome" = Empty,
            "gen_ai.operation.name" = "invoke_agent",
            "gen_ai.agent.name" = self.harness.as_str(),
            "gen_ai.request.model" = Empty,
            "gen_ai.usage.input_tokens" = Empty,
            "gen_ai.usage.output_tokens" = Empty,
            "error.type" = Empty,
            "otel.status_description" = Empty,
        );
        if span.is_none() {
            return;
        }
        if let Some(parent) = &self.parent {
            trace::adopt(&span, parent);
        }
        remember_live(&self.branch, at_ms);
        if let Some(own) = trace::traceparent(&span) {
            PARENTS
                .lock_recovering("live turn parents")
                .get_or_insert_with(HashMap::new)
                .insert(self.branch.clone(), own);
        }
        self.turn = Some(Turn {
            span,
            tool: None,
            permission: None,
            stall: None,
            steers: Vec::new(),
            input: 0,
            output: 0,
            model: None,
        });
    }

    fn end(&mut self, outcome: &str) {
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        // Children end before the turn does.
        turn.tool = None;
        turn.permission = None;
        turn.stall = None;
        turn.steers.clear();
        turn.span.record("by.outcome", outcome);
        if turn.input > 0 {
            turn.span.record(
                "gen_ai.usage.input_tokens",
                i64::try_from(turn.input).unwrap_or(i64::MAX),
            );
        }
        if turn.output > 0 {
            turn.span.record(
                "gen_ai.usage.output_tokens",
                i64::try_from(turn.output).unwrap_or(i64::MAX),
            );
        }
        if let Some(model) = &turn.model {
            turn.span.record("gen_ai.request.model", model.as_str());
        }
        if is_error(outcome) {
            turn.span.record("error.type", outcome);
            turn.span.record("otel.status_description", outcome);
        }
        if let Some(parents) = PARENTS.lock_recovering("live turn parents").as_mut() {
            parents.remove(&self.branch);
        }
        drop(turn);
    }

    /// Follow one recorded activity, stamped `at_ms`.
    pub(crate) fn observe(&mut self, at_ms: u64, activity: &Activity) {
        if !trace::enabled() {
            return;
        }
        if matches!(activity, Activity::Prompt(_)) {
            self.end("unknown");
            self.start(at_ms);
            return;
        }
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        // The harness says when a tool starts, not when it ends: a tool
        // call lasts until the branch's next event.
        turn.tool = None;
        match activity {
            Activity::Harness(event) => match event {
                Event::TurnAccepted { turn: n, .. } => {
                    turn.span
                        .record("by.turn", i64::try_from(*n).unwrap_or(i64::MAX));
                }
                Event::ToolStarted {
                    name,
                    call_id,
                    target,
                    ..
                } => {
                    let (kind, value) = match target {
                        Some(t) => (
                            format!("{:?}", t.kind).to_ascii_lowercase(),
                            t.value.as_str(),
                        ),
                        None => (String::new(), ""),
                    };
                    let name = &tool_name(name, target.is_some());
                    let tool = turn.span.in_scope(|| {
                        span!(
                            "tool",
                            "otel.name" = format!("tool {name}"),
                            "by.branch" = self.branch.as_str(),
                            "by.tool" = name.as_str(),
                            "by.call_id" = call_id.as_str(),
                            "by.tool.target.kind" = kind.as_str(),
                            "by.tool.target" = value,
                            "gen_ai.operation.name" = "execute_tool",
                            "gen_ai.tool.name" = name.as_str(),
                            "gen_ai.tool.call.id" = call_id.as_str(),
                        )
                    });
                    turn.tool = Some(tool);
                }
                Event::PermissionRequested { request, .. } => {
                    let wait = turn.span.in_scope(|| {
                        span!(
                            "permission wait",
                            "by.branch" = self.branch.as_str(),
                            "by.tool" = request.tool.as_str(),
                            "by.allowed" = Empty,
                            "by.source" = Empty,
                        )
                    });
                    turn.permission = Some(wait);
                }
                Event::PermissionWithdrawn { .. } => {
                    if let Some(wait) = turn.permission.take() {
                        wait.record("by.source", "withdrawn");
                    }
                }
                Event::UsageObserved { usage, .. } => {
                    let (input, output) = (
                        usage.input_tokens.unwrap_or(0),
                        usage.output_tokens.unwrap_or(0),
                    );
                    if usage.cumulative {
                        turn.input = turn.input.max(input);
                        turn.output = turn.output.max(output);
                    } else {
                        turn.input = turn.input.saturating_add(input);
                        turn.output = turn.output.saturating_add(output);
                    }
                    if turn.model.is_none() {
                        turn.model.clone_from(&usage.model);
                    }
                }
                Event::SteerAccepted { steer, .. } => {
                    if let Some(at) = turn.steers.iter().position(|(id, _)| id == steer) {
                        let (_, span) = turn.steers.remove(at);
                        span.record("by.accepted", true);
                    }
                }
                Event::SteerRejected { steer, reason, .. } => {
                    if let Some(at) = turn.steers.iter().position(|(id, _)| id == steer) {
                        let (_, span) = turn.steers.remove(at);
                        span.record("by.accepted", false);
                        span.record("otel.status_description", short(reason).as_str());
                    }
                }
                Event::TurnEnded { turn: n, outcome } => {
                    turn.span
                        .record("by.turn", i64::try_from(*n).unwrap_or(i64::MAX));
                    let outcome = tag(outcome, "kind");
                    self.end(&outcome);
                }
                Event::OutcomeUnknown { .. } => self.end("unknown"),
                _ => {}
            },
            Activity::Decision {
                allowed, source, ..
            } => {
                if let Some(wait) = turn.permission.take() {
                    wait.record("by.allowed", *allowed);
                    wait.record("by.source", tag(source, "kind").as_str());
                }
            }
            Activity::Steered { id, by, .. } => {
                let steer = turn.span.in_scope(|| {
                    span!(
                        "steer",
                        "by.branch" = self.branch.as_str(),
                        "by.steer" = i64::try_from(*id).unwrap_or(i64::MAX),
                        "by.steered_by" = by.as_str(),
                        "by.accepted" = Empty,
                        "otel.status_description" = Empty,
                    )
                });
                turn.steers.push((*id, steer));
            }
            Activity::Stalled { since_ms } => {
                let stall = turn.span.in_scope(|| {
                    span!(
                        "stall",
                        "by.branch" = self.branch.as_str(),
                        "by.since_ms" = i64::try_from(*since_ms).unwrap_or(i64::MAX),
                    )
                });
                turn.stall = Some(stall);
            }
            Activity::Resumed => turn.stall = None,
            // The engine ended the turn itself (a budget, a cancel, a
            // failure before the harness answered).
            Activity::Status(status)
                if !matches!(status, BranchStatus::Running | BranchStatus::Waiting) =>
            {
                let outcome = tag(status, "state");
                self.end(&outcome);
            }
            _ => {}
        }
    }
}

impl Drop for LiveTrace {
    fn drop(&mut self) {
        self.end("unfinished");
    }
}

/// Write the span of a connector gateway call as it is ingested, under the
/// live turn of `branch` when there is one (a call ingested when no turn
/// is live is traced from the log after the operation, as before). The
/// span covers the gateway's recorded latency, ending at `at_ms`.
pub(crate) fn connector_call(branch: &str, at_ms: u64, call: &crate::connectors::ConnectorCall) {
    if !trace::enabled() {
        return;
    }
    let Some(parent) = turn_traceparent(branch) else {
        return;
    };
    trace::record(&trace::Finished {
        name: format!("connector {}", call.operation),
        parent: Some(parent),
        kind: "client",
        start_ms: at_ms.saturating_sub(call.latency_ms.unwrap_or(0)),
        end_ms: at_ms,
        attributes: vec![
            ("by.branch", branch.into()),
            ("by.connector", call.connector.as_str().into()),
            ("by.operation", call.operation.as_str().into()),
            ("by.decision", call.decision.as_str().into()),
            ("by.live", true.into()),
        ],
        error: (call.decision != "allowed")
            .then(|| call.reason.clone().unwrap_or_else(|| call.decision.clone())),
    });
}
