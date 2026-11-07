//! A branch's trajectory as a trace, rebuilt from its event log.
//!
//! The spans a server writes while a turn runs ([`crate::live_trace`]) are
//! gone once they are exported; the event log stays. [`spans`] reads the
//! logs of a branch (and, when asked, the branches under it) and builds the
//! same tree after the fact: a `branch <name>` span over each log, a `turn`
//! under it for each prompt, and under a turn its `tool`, `permission wait`,
//! `steer`, `stall` and `connector` spans; a `delegation <tool>` span where
//! a branch asked for work, with the child branch's span under it. The
//! attributes are the live ones (GenAI conventions, the bounded scrubbed
//! tool target, never a tool's input) plus `by.source = event-log`.
//!
//! Ids are derived, not drawn: the trace id is the trace a message the root
//! branch carried had stored (`meta.trace`), else a hash of the branch's
//! name and first event; a span id is a hash of the trace id and the span's
//! place in the log (branch, event index). The same log therefore always
//! exports the same trace, and exporting twice to a collector makes the
//! same spans, not a second copy.

use sha2::{Digest, Sha256};

use crate::live_trace::{is_error, tag, tool_name};
use crate::{Activity, BranchStatus, RecordedEvent};
use branchyard_harness::Event;
pub use branchyard_support::trace::Value;

/// One branch's log, with where it sits in the tree.
#[derive(Clone, Debug)]
pub struct BranchLog {
    /// The branch's name.
    pub name: String,
    /// Its parent branch, when the branch was delegated.
    pub parent: Option<String>,
    /// The harness that ran its turns.
    pub harness: String,
    /// Its recorded events, oldest first.
    pub events: Vec<RecordedEvent>,
}

/// A span of the rebuilt trace.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    /// The span's name.
    pub name: String,
    /// Its trace id, 32 hex digits.
    pub trace_id: String,
    /// Its id, 16 hex digits.
    pub span_id: String,
    /// Its parent's id; `None` for the root.
    pub parent_span_id: Option<String>,
    /// `internal` or `client`.
    pub kind: &'static str,
    /// When it began, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// When it ended, in milliseconds since the Unix epoch.
    pub end_ms: u64,
    /// Its attributes.
    pub attributes: Vec<(String, Value)>,
    /// What went wrong, when the span ended in error.
    pub error: Option<String>,
}

fn digest(trace: &str, place: &str, bytes: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"by-trace-v1/");
    hasher.update(trace.as_bytes());
    hasher.update(b"/");
    hasher.update(place.as_bytes());
    let mut out = hasher.finalize()[..bytes].to_vec();
    // An all-zero id is invalid in W3C trace context.
    if out.iter().all(|b| *b == 0) {
        if let Some(last) = out.last_mut() {
            *last = 1;
        }
    }
    hex::encode(out)
}

/// The trace id and span id of a stored `traceparent`.
fn stored(traceparent: &str) -> Option<(String, String)> {
    let mut parts = traceparent.split('-');
    let (_version, trace, span) = (parts.next()?, parts.next()?, parts.next()?);
    (trace.len() == 32 && span.len() == 16 && trace != "0".repeat(32) && span != "0".repeat(16))
        .then(|| (trace.to_owned(), span.to_owned()))
}

/// The spans of `logs`, the first of which is the root of the trace.
/// Empty when the root has no events.
pub fn spans(logs: &[BranchLog]) -> Vec<Span> {
    let Some(root) = logs.first().filter(|l| !l.events.is_empty()) else {
        return Vec::new();
    };
    let stored_root = root.events.iter().find_map(|e| match &e.activity {
        Activity::Message(m) => m
            .meta
            .as_ref()
            .and_then(|meta| meta.trace.as_deref())
            .and_then(stored),
        _ => None,
    });
    let trace_id = match &stored_root {
        Some((trace, _)) => trace.clone(),
        None => digest("", &format!("{}/{}", root.name, root.events[0].at_ms), 16),
    };
    let mut out = Vec::new();
    for log in logs.iter().filter(|l| !l.events.is_empty()) {
        let parent = match log.parent.as_deref() {
            None => stored_root.as_ref().map(|(_, span)| span.clone()),
            Some(parent) => {
                let spawned = logs
                    .iter()
                    .filter(|l| l.name == parent)
                    .flat_map(|l| &l.events)
                    .any(|e| delegated(&e.activity, &log.name));
                // Under the delegation that spawned it, else under its
                // parent's branch span.
                Some(if spawned {
                    digest(&trace_id, &format!("delegation/{parent}/{}", log.name), 8)
                } else {
                    digest(&trace_id, &format!("branch/{parent}"), 8)
                })
            }
        };
        Build::new(&trace_id, log, parent, &mut out).run();
    }
    out
}

/// Whether `activity` is the spawn of `child`.
fn delegated(activity: &Activity, child: &str) -> bool {
    matches!(activity, Activity::Delegation { tool, branch, refused: false, .. }
        if tool == "spawn" && branch == child)
}

/// A tool call that has started and not yet been followed by an event.
struct OpenTool {
    index: usize,
    start_ms: u64,
    name: String,
    call_id: String,
    /// The target's kind and bounded, scrubbed value.
    target: Option<(String, String)>,
}

struct OpenTurn {
    index: usize,
    start_ms: u64,
    number: Option<u64>,
    model: Option<String>,
    input: u64,
    output: u64,
    tool: Option<OpenTool>,
    permission: Option<(usize, u64, String)>,
    steers: Vec<(usize, u64, u64, String)>,
    stall: Option<(usize, u64)>,
    children: Vec<Span>,
}

struct Build<'a> {
    trace: &'a str,
    log: &'a BranchLog,
    branch_span: String,
    parent: Option<String>,
    out: &'a mut Vec<Span>,
    turn: Option<OpenTurn>,
}

impl<'a> Build<'a> {
    fn new(
        trace: &'a str,
        log: &'a BranchLog,
        parent: Option<String>,
        out: &'a mut Vec<Span>,
    ) -> Build<'a> {
        Build {
            trace,
            log,
            branch_span: digest(trace, &format!("branch/{}", log.name), 8),
            parent,
            out,
            turn: None,
        }
    }

    fn id(&self, place: &str) -> String {
        digest(self.trace, &format!("{}/{place}", self.log.name), 8)
    }

    fn span(
        &self,
        name: String,
        place: &str,
        parent: &str,
        start_ms: u64,
        end_ms: u64,
        attributes: Vec<(String, Value)>,
    ) -> Span {
        let mut attributes = attributes;
        attributes.push(("by.branch".into(), self.log.name.as_str().into()));
        attributes.push(("by.source".into(), "event-log".into()));
        Span {
            name,
            trace_id: self.trace.to_owned(),
            span_id: self.id(place),
            parent_span_id: Some(parent.to_owned()),
            kind: "internal",
            start_ms,
            end_ms: end_ms.max(start_ms),
            attributes,
            error: None,
        }
    }

    fn parent_span(&self) -> String {
        match &self.turn {
            Some(turn) => self.id(&format!("turn/{}", turn.index)),
            None => self.branch_span.clone(),
        }
    }

    fn attach(&mut self, span: Span) {
        match &mut self.turn {
            Some(turn) => turn.children.push(span),
            None => self.out.push(span),
        }
    }

    fn end_tool(&mut self, at: u64) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let Some(OpenTool {
            index,
            start_ms: start,
            name,
            call_id,
            target,
        }) = turn.tool.take()
        else {
            return;
        };
        let turn_index = turn.index;
        let parent = self.id(&format!("turn/{turn_index}"));
        let mut attributes: Vec<(String, Value)> = vec![
            ("by.tool".into(), name.as_str().into()),
            ("by.call_id".into(), call_id.as_str().into()),
            ("gen_ai.operation.name".into(), "execute_tool".into()),
            ("gen_ai.tool.name".into(), name.as_str().into()),
            ("gen_ai.tool.call.id".into(), call_id.as_str().into()),
        ];
        if let Some((kind, value)) = target {
            attributes.push(("by.tool.target.kind".into(), kind.into()));
            attributes.push(("by.tool.target".into(), value.into()));
        }
        let span = self.span(
            format!("tool {name}"),
            &format!("tool/{index}"),
            &parent,
            start,
            at,
            attributes,
        );
        if let Some(turn) = self.turn.as_mut() {
            turn.children.push(span);
        }
    }

    fn end_turn(&mut self, at: u64, outcome: &str) {
        self.end_tool(at);
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        let turn_id = self.id(&format!("turn/{}", turn.index));
        if let Some((index, start, tool)) = turn.permission.take() {
            let parent = turn_id.clone();
            let span = self.span(
                "permission wait".into(),
                &format!("permission/{index}"),
                &parent,
                start,
                at,
                vec![
                    ("by.tool".into(), tool.into()),
                    ("by.source".into(), "unanswered".into()),
                ],
            );
            turn.children.push(span);
        }
        for (index, start, id, by) in std::mem::take(&mut turn.steers) {
            let span = self.span(
                "steer".into(),
                &format!("steer/{index}"),
                &turn_id,
                start,
                at,
                vec![
                    (
                        "by.steer".into(),
                        i64::try_from(id).unwrap_or(i64::MAX).into(),
                    ),
                    ("by.steered_by".into(), by.into()),
                ],
            );
            turn.children.push(span);
        }
        if let Some((index, since)) = turn.stall.take() {
            let span = self.span(
                "stall".into(),
                &format!("stall/{index}"),
                &turn_id,
                since,
                at,
                vec![(
                    "by.since_ms".into(),
                    i64::try_from(since).unwrap_or(i64::MAX).into(),
                )],
            );
            turn.children.push(span);
        }
        let mut attributes: Vec<(String, Value)> = vec![
            ("by.harness".into(), self.log.harness.as_str().into()),
            ("gen_ai.operation.name".into(), "invoke_agent".into()),
            ("gen_ai.agent.name".into(), self.log.harness.as_str().into()),
            ("by.outcome".into(), outcome.into()),
        ];
        if let Some(n) = turn.number {
            attributes.push((
                "by.turn".into(),
                i64::try_from(n).unwrap_or(i64::MAX).into(),
            ));
        }
        if let Some(model) = &turn.model {
            attributes.push(("gen_ai.request.model".into(), model.as_str().into()));
        }
        if turn.input > 0 {
            attributes.push((
                "gen_ai.usage.input_tokens".into(),
                i64::try_from(turn.input).unwrap_or(i64::MAX).into(),
            ));
        }
        if turn.output > 0 {
            attributes.push((
                "gen_ai.usage.output_tokens".into(),
                i64::try_from(turn.output).unwrap_or(i64::MAX).into(),
            ));
        }
        let parent = self.branch_span.clone();
        let mut span = self.span(
            "turn".into(),
            &format!("turn/{}", turn.index),
            &parent,
            turn.start_ms,
            at,
            attributes,
        );
        if is_error(outcome) {
            span.attributes
                .push(("error.type".into(), outcome.to_owned().into()));
            span.error = Some(outcome.to_owned());
        }
        self.out.push(span);
        self.out.append(&mut turn.children);
    }

    fn run(mut self) {
        let log = self.log;
        let mut last = 0;
        for (index, event) in log.events.iter().enumerate() {
            let at = event.at_ms;
            last = last.max(at);
            if !matches!(event.activity, Activity::Prompt(_)) {
                self.end_tool(at);
            }
            self.event(index, at, &event.activity);
        }
        self.end_turn(last, "unfinished");
        let first = log.events.first().map_or(0, |e| e.at_ms);
        let mut branch = Span {
            name: format!("branch {}", log.name),
            trace_id: self.trace.to_owned(),
            span_id: self.branch_span.clone(),
            parent_span_id: self.parent.clone(),
            kind: "internal",
            start_ms: first,
            end_ms: last.max(first),
            attributes: vec![
                ("by.branch".into(), log.name.as_str().into()),
                ("by.harness".into(), log.harness.as_str().into()),
                ("by.source".into(), "event-log".into()),
            ],
            error: None,
        };
        if let Some(parent) = log.parent.as_deref() {
            branch
                .attributes
                .push(("by.parent".into(), parent.to_owned().into()));
        }
        self.out.insert(0, branch);
    }

    fn event(&mut self, index: usize, at: u64, activity: &Activity) {
        match activity {
            Activity::Prompt(_) => {
                self.end_turn(at, "unknown");
                self.turn = Some(OpenTurn {
                    index,
                    start_ms: at,
                    number: None,
                    model: None,
                    input: 0,
                    output: 0,
                    tool: None,
                    permission: None,
                    steers: Vec::new(),
                    stall: None,
                    children: Vec::new(),
                });
            }
            Activity::Harness(event) => self.harness(index, at, event),
            Activity::Decision {
                allowed, source, ..
            } => {
                let parent = self.parent_span();
                if let Some(turn) = self.turn.as_mut() {
                    if let Some((opened, start, tool)) = turn.permission.take() {
                        let attributes = vec![
                            ("by.tool".into(), tool.into()),
                            ("by.allowed".into(), (*allowed).into()),
                            ("by.source".into(), tag(source, "kind").into()),
                        ];
                        let span = self.span(
                            "permission wait".into(),
                            &format!("permission/{opened}"),
                            &parent,
                            start,
                            at,
                            attributes,
                        );
                        if let Some(turn) = self.turn.as_mut() {
                            turn.children.push(span);
                        }
                    }
                }
            }
            Activity::Steered { id, by, .. } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.steers.push((index, at, *id, by.clone()));
                }
            }
            Activity::Stalled { since_ms } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.stall = Some((index, (*since_ms).min(at)));
                }
            }
            Activity::Resumed => {
                let parent = self.parent_span();
                if let Some(turn) = self.turn.as_mut() {
                    if let Some((opened, since)) = turn.stall.take() {
                        let span = self.span(
                            "stall".into(),
                            &format!("stall/{opened}"),
                            &parent,
                            since,
                            at,
                            vec![(
                                "by.since_ms".into(),
                                i64::try_from(since).unwrap_or(i64::MAX).into(),
                            )],
                        );
                        if let Some(turn) = self.turn.as_mut() {
                            turn.children.push(span);
                        }
                    }
                }
            }
            Activity::ConnectorCall(call) => {
                let parent = self.parent_span();
                let mut span = self.span(
                    format!("connector {}", call.operation),
                    &format!("connector/{index}"),
                    &parent,
                    at.saturating_sub(call.latency_ms.unwrap_or(0)),
                    at,
                    vec![
                        ("by.connector".into(), call.connector.as_str().into()),
                        ("by.operation".into(), call.operation.as_str().into()),
                        ("by.decision".into(), call.decision.as_str().into()),
                    ],
                );
                span.kind = "client";
                span.error = (call.decision != "allowed")
                    .then(|| call.reason.clone().unwrap_or_else(|| call.decision.clone()));
                self.attach(span);
            }
            Activity::Delegation {
                tool,
                branch,
                outcome: _,
                refused,
            } => {
                let parent = self.parent_span();
                let place = if tool == "spawn" && !*refused {
                    format!("delegation/{}/{branch}", self.log.name)
                } else {
                    format!("delegation/{index}")
                };
                // A spawn's id is the one its child's branch span is under.
                let mut span = self.span(
                    format!("delegation {tool}"),
                    "unused",
                    &parent,
                    at,
                    at,
                    vec![
                        ("by.tool".into(), tool.as_str().into()),
                        ("by.child".into(), branch.as_str().into()),
                        ("by.refused".into(), (*refused).into()),
                    ],
                );
                span.span_id = if tool == "spawn" && !*refused {
                    digest(self.trace, &place, 8)
                } else {
                    self.id(&place)
                };
                span.error = refused.then(|| "refused".to_owned());
                self.attach(span);
            }
            Activity::Status(status)
                if !matches!(status, BranchStatus::Running | BranchStatus::Waiting) =>
            {
                self.end_turn(at, &tag(status, "state"));
            }
            _ => {}
        }
    }

    fn harness(&mut self, index: usize, at: u64, event: &Event) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        match event {
            Event::TurnAccepted { turn: n, .. } => turn.number = Some(*n),
            Event::ToolStarted {
                name,
                call_id,
                target,
                ..
            } => {
                let shown = tool_name(name, target.is_some());
                let target = target.as_ref().map(|t| {
                    (
                        format!("{:?}", t.kind).to_ascii_lowercase(),
                        t.value.clone(),
                    )
                });
                turn.tool = Some(OpenTool {
                    index,
                    start_ms: at,
                    name: shown,
                    call_id: call_id.clone(),
                    target,
                });
            }
            Event::PermissionRequested { request, .. } => {
                turn.permission = Some((index, at, request.tool.clone()));
            }
            Event::PermissionWithdrawn { .. } => {
                turn.permission = None;
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
            Event::SteerAccepted { steer, .. } | Event::SteerRejected { steer, .. } => {
                let accepted = matches!(event, Event::SteerAccepted { .. });
                if let Some(at_index) = turn.steers.iter().position(|(_, _, id, _)| id == steer) {
                    let (opened, start, id, by) = turn.steers.remove(at_index);
                    let turn_index = turn.index;
                    let parent = self.id(&format!("turn/{turn_index}"));
                    let mut span = self.span(
                        "steer".into(),
                        &format!("steer/{opened}"),
                        &parent,
                        start,
                        at,
                        vec![
                            (
                                "by.steer".into(),
                                i64::try_from(id).unwrap_or(i64::MAX).into(),
                            ),
                            ("by.steered_by".into(), by.into()),
                            ("by.accepted".into(), accepted.into()),
                        ],
                    );
                    if let Event::SteerRejected { reason, .. } = event {
                        span.error = Some(reason.chars().take(200).collect());
                    }
                    if let Some(turn) = self.turn.as_mut() {
                        turn.children.push(span);
                    }
                }
            }
            Event::TurnEnded { turn: n, outcome } => {
                turn.number.get_or_insert(*n);
                let outcome = tag(outcome, "kind");
                self.end_turn(at, &outcome);
            }
            Event::OutcomeUnknown { .. } => self.end_turn(at, "unknown"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_harness::{TurnOutcome, Usage};

    fn at(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn log(name: &str, parent: Option<&str>, events: Vec<RecordedEvent>) -> BranchLog {
        BranchLog {
            name: name.into(),
            parent: parent.map(str::to_owned),
            harness: "claude-code".into(),
            events,
        }
    }

    fn turn_events() -> Vec<RecordedEvent> {
        vec![
            at(1_000, Activity::Prompt("go".into())),
            at(
                1_010,
                Activity::Harness(Event::TurnAccepted {
                    turn: 1,
                    native: None,
                }),
            ),
            at(
                1_100,
                Activity::Harness(Event::ToolStarted {
                    turn: 1,
                    call_id: "c1".into(),
                    name: "Bash".into(),
                    target: branchyard_harness::ToolTarget::command("make test --token=abc123xyz"),
                }),
            ),
            at(
                1_400,
                Activity::Harness(Event::UsageObserved {
                    turn: Some(1),
                    usage: Usage {
                        input_tokens: Some(120),
                        output_tokens: Some(30),
                        model: Some("m-1".into()),
                        ..Usage::default()
                    },
                }),
            ),
            at(
                2_000,
                Activity::Harness(Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Completed,
                }),
            ),
        ]
    }

    fn named<'a>(spans: &'a [Span], name: &str) -> &'a Span {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no {name} in {spans:#?}"))
    }

    fn text<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        span.attributes.iter().find_map(|(k, v)| match v {
            Value::Str(s) if k == key => Some(s.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_branch_is_a_tree_of_spans_with_the_live_attributes() {
        let spans = spans(&[log("a", None, turn_events())]);
        let branch = named(&spans, "branch a");
        let turn = named(&spans, "turn");
        let tool = named(&spans, "tool Bash");
        assert_eq!(turn.parent_span_id.as_ref(), Some(&branch.span_id));
        assert_eq!(tool.parent_span_id.as_ref(), Some(&turn.span_id));
        assert_eq!((turn.start_ms, turn.end_ms), (1_000, 2_000));
        // The tool lasts to the branch's next event.
        assert_eq!((tool.start_ms, tool.end_ms), (1_100, 1_400));
        assert_eq!(text(turn, "gen_ai.operation.name"), Some("invoke_agent"));
        assert_eq!(text(turn, "gen_ai.agent.name"), Some("claude-code"));
        assert_eq!(text(turn, "gen_ai.request.model"), Some("m-1"));
        assert_eq!(text(turn, "by.outcome"), Some("completed"));
        assert!(turn
            .attributes
            .contains(&("gen_ai.usage.input_tokens".into(), Value::Int(120))));
        assert_eq!(text(tool, "by.tool.target.kind"), Some("command"));
        let target = text(tool, "by.tool.target").unwrap();
        assert!(target.contains("[redacted]") && !target.contains("abc123xyz"));
        assert_eq!(turn.error, None);
        assert!(spans
            .iter()
            .all(|s| text(s, "by.source") == Some("event-log")));
        assert!(spans.iter().all(|s| s.trace_id == branch.trace_id));
    }

    #[test]
    fn ids_come_from_the_log_alone_and_a_stored_trace_is_adopted() {
        let first = spans(&[log("a", None, turn_events())]);
        let second = spans(&[log("a", None, turn_events())]);
        assert_eq!(first, second, "the same log exports the same trace");
        let other = spans(&[log("b", None, turn_events())]);
        assert_ne!(first[0].trace_id, other[0].trace_id);
        assert_eq!(first[0].trace_id.len(), 32);
        assert!(first.iter().all(|s| s.span_id.len() == 16));
        // A message the branch sent under a traceparent names the trace.
        let mut with_message = turn_events();
        with_message.push(at(
            2_100,
            Activity::Message(crate::Message {
                id: 1,
                from: "a".into(),
                to: "@person".into(),
                kind: crate::MessageKind::Report,
                text: "done".into(),
                in_reply_to: None,
                at_ms: 2_100,
                delivered: false,
                handoff: None,
                next_turn: false,
                meta: Some(crate::messaging::MessageMeta {
                    trace: Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into()),
                    ..Default::default()
                }),
            }),
        ));
        let adopted = spans(&[log("a", None, with_message)]);
        let branch = named(&adopted, "branch a");
        assert_eq!(branch.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(branch.parent_span_id.as_deref(), Some("b7ad6b7169203331"));
    }

    #[test]
    fn a_child_branch_hangs_under_the_delegation_that_spawned_it() {
        let parent = log(
            "lead",
            None,
            vec![
                at(1_000, Activity::Prompt("go".into())),
                at(
                    1_500,
                    Activity::Delegation {
                        tool: "spawn".into(),
                        branch: "kid".into(),
                        outcome: "created".into(),
                        refused: false,
                    },
                ),
                at(
                    3_000,
                    Activity::Harness(Event::TurnEnded {
                        turn: 1,
                        outcome: TurnOutcome::Completed,
                    }),
                ),
            ],
        );
        let kid = log("kid", Some("lead"), turn_events());
        let spans = spans(&[parent, kid]);
        let delegation = named(&spans, "delegation spawn");
        let kid_branch = named(&spans, "branch kid");
        assert_eq!(
            kid_branch.parent_span_id.as_ref(),
            Some(&delegation.span_id)
        );
        assert_eq!(text(delegation, "by.child"), Some("kid"));
        let lead_turn = spans
            .iter()
            .find(|s| s.name == "turn" && text(s, "by.branch") == Some("lead"))
            .unwrap();
        assert_eq!(delegation.parent_span_id.as_ref(), Some(&lead_turn.span_id));
    }

    #[test]
    fn waits_steers_stalls_and_connector_calls_are_children_of_their_turn() {
        let mut events = vec![at(1_000, Activity::Prompt("go".into()))];
        events.push(at(
            1_100,
            Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request: branchyard_harness::PermissionRequest {
                    key: branchyard_harness::PermissionKey("k1".into()),
                    tool: "Edit".into(),
                    input: serde_json::json!({"path": "secret.txt"}),
                },
            }),
        ));
        events.push(at(
            1_300,
            Activity::Decision {
                tool: "Edit".into(),
                allowed: true,
                message: None,
                source: crate::DecisionSource::Default,
            },
        ));
        events.push(at(
            1_400,
            Activity::Steered {
                id: 7,
                by: "person".into(),
                text: "do it differently".into(),
            },
        ));
        events.push(at(
            1_450,
            Activity::Harness(Event::SteerAccepted { turn: 1, steer: 7 }),
        ));
        events.push(at(1_600, Activity::Stalled { since_ms: 1_500 }));
        events.push(at(1_900, Activity::Resumed));
        events.push(at(
            2_000,
            Activity::Harness(Event::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Failed {
                    message: "boom".into(),
                },
            }),
        ));
        let spans = spans(&[log("a", None, events)]);
        let turn = named(&spans, "turn");
        for child in ["permission wait", "steer", "stall"] {
            assert_eq!(
                named(&spans, child).parent_span_id.as_ref(),
                Some(&turn.span_id),
                "{child}"
            );
        }
        let wait = named(&spans, "permission wait");
        assert_eq!((wait.start_ms, wait.end_ms), (1_100, 1_300));
        assert_eq!(text(wait, "by.tool"), Some("Edit"));
        assert!(
            !spans
                .iter()
                .flat_map(|s| &s.attributes)
                .any(|(_, v)| matches!(
                    v,
                    Value::Str(s) if s.contains("secret.txt") || s.contains("differently")
                )),
            "neither a tool's input nor a steer's text is exported"
        );
        assert_eq!(named(&spans, "stall").start_ms, 1_500);
        assert_eq!(turn.error.as_deref(), Some("failed"));
    }

    #[test]
    fn an_empty_log_has_no_trace_and_an_open_turn_ends_with_the_last_event() {
        assert!(spans(&[log("a", None, Vec::new())]).is_empty());
        let spans = spans(&[log(
            "a",
            None,
            vec![
                at(1_000, Activity::Prompt("go".into())),
                at(1_700, Activity::Warning("hmm".into())),
            ],
        )]);
        let turn = named(&spans, "turn");
        assert_eq!(turn.end_ms, 1_700);
        assert_eq!(text(turn, "by.outcome"), Some("unfinished"));
    }
}
