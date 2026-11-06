//! Messages between siblings through the messaging layer
//! (`docs/messaging.md`), against the fake ACP agent: a registered kind's
//! schema is checked at the boundary, authority stops at the siblings, a
//! key makes a repeat return the first message, messages keep their order,
//! the recipient reads them in its inbox and in its next turn's prompt,
//! both logs record them, and the sender's trace rides along.

#![allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
mod common;

use std::time::Duration;

use branchyard::messaging::{Class, KindSpec};
use branchyard::{
    Activity, Delegate, DeliveredVia, Envelope, Error, MessageKind, Policy, Spawn, TaskOptions,
};
use common::{fake_agent, Fixture};
use serde_json::json;

/// The sender's turn's `traceparent`, as a server sets it.
const TRACE: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

fn delegating(f: &Fixture) -> TaskOptions {
    TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        trace_parent: Some(TRACE.into()),
        ..f.options()
    }
}

fn spawn(name: &str) -> Spawn {
    Spawn {
        prompt: format!("WRITE {name}.txt=1"),
        name: Some(name.into()),
        ..Spawn::default()
    }
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(reason)) if reason.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

fn messages_on(f: &Fixture, branch: &str) -> Vec<branchyard::Message> {
    f.yard
        .branch(branch)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Message(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// A root with two settled leaf children, `a` and `b`, and a delegate
/// for each.
fn siblings(f: &Fixture, options: &TaskOptions) -> (Delegate, Delegate, Delegate) {
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let root_delegate = root.delegate(options.clone()).unwrap();
    for name in ["a", "b"] {
        let child = root_delegate.spawn(spawn(name)).unwrap();
        assert_eq!(child.depth, 1);
        root_delegate
            .wait(name, Duration::from_secs(120))
            .unwrap_or_else(|e| panic!("{name} never settled: {e}"));
    }
    let delegate = |name: &str| {
        f.yard
            .branch(name)
            .unwrap()
            .delegate(options.clone())
            .unwrap()
    };
    (root_delegate, delegate("a"), delegate("b"))
}

#[test]
fn a_sibling_message_is_checked_stored_once_and_read_at_the_next_turn() {
    let f = Fixture::new();
    let options = delegating(&f);
    // A kind registered by schema: a payload with an integer `n`.
    f.yard
        .register_kind(
            KindSpec::new("step", Class::Peer)
                .unwrap()
                .with_schema(json!({
                    "type": "object",
                    "required": ["n"],
                    "properties": {"n": {"type": "integer"}},
                    "additionalProperties": false
                }))
                .unwrap(),
        )
        .unwrap();
    assert!(f.yard.message_kinds().iter().any(|k| k.name == "step"));
    let (root, a, b) = siblings(&f, &options);

    // Refusals, each by name, before anything is stored.
    denied(
        a.message("b", "hi", Some("nope"), None, None),
        "no message kind named \"nope\"",
    );
    denied(
        a.message("b", "hi", Some("step"), Some(json!({"n": "one"})), None),
        "payload.n: expected integer",
    );
    denied(
        a.message("b", "hi", Some("step"), Some(json!({"n": 1, "x": 2})), None),
        "payload: the field \"x\" is not allowed",
    );
    denied(
        a.message("b", "hi", Some("step"), None, None),
        "needs a payload",
    );
    denied(
        a.message("root", "hi", None, None, None),
        "may only message its siblings",
    );
    denied(
        a.message("@person", "hi", None, None, None),
        "may only message its siblings",
    );
    denied(root.message("a", "hi", None, None, None), "has no siblings");
    denied(
        a.message("a", "hi", None, None, None),
        "cannot message itself",
    );
    denied(a.message("b", "   ", None, None, None), "needs text");
    assert!(matches!(
        a.message("nobody", "hi", None, None, None),
        Err(Error::UnknownBranch(_))
    ));
    assert!(
        b.inbox().unwrap().messages.is_empty(),
        "a refusal stores nothing"
    );
    assert!(messages_on(&f, "a").is_empty() && messages_on(&f, "b").is_empty());

    // A note with a payload, under a key.
    let first = a
        .message(
            "b",
            "lexer done",
            None,
            Some(json!({"tokens": 3})),
            Some("k1"),
        )
        .unwrap();
    assert_eq!((first.from.as_str(), first.to.as_str()), ("a", "b"));
    assert_eq!(first.kind, MessageKind::Peer);
    assert_eq!(first.kind_name(), "note");
    assert!(first.next_turn, "a peer message waits for the next turn");
    assert!(!first.delivered);
    let meta = first
        .meta
        .clone()
        .expect("meta carries the kind, payload and trace");
    assert_eq!(meta.kind.as_deref(), Some("note"));
    assert_eq!(meta.payload, Some(json!({"tokens": 3})));
    assert_eq!(
        meta.trace.as_deref(),
        Some(TRACE),
        "the sender's turn's trace"
    );
    assert_eq!(serde_json::to_value(&first).unwrap()["kind"], json!("peer"));

    // The same key again is the same message; the key names one message.
    let again = a
        .message(
            "b",
            "lexer done",
            None,
            Some(json!({"tokens": 3})),
            Some("k1"),
        )
        .unwrap();
    assert_eq!(again, first);
    denied(
        a.message("b", "something else", None, None, Some("k1")),
        "a key names one message",
    );
    // b may use the same key for its own message.
    let from_b = b.message("a", "ack", None, None, Some("k1")).unwrap();
    assert_ne!(from_b.id, first.id);

    // A second message of the registered kind, after the first.
    let second = a
        .message(
            "b",
            "step two",
            Some("step"),
            Some(json!({"n": 2})),
            Some("k2"),
        )
        .unwrap();
    assert!(second.id > first.id);
    assert_eq!(second.kind_name(), "step");
    assert_eq!(second.meta.as_ref().unwrap().schema_version, Some(1));
    // The key with another kind is another message, though both are stored
    // as the peer class and the text and payload match: refused, not
    // answered with the step.
    denied(
        a.message("b", "step two", None, Some(json!({"n": 2})), Some("k2")),
        "a key names one message",
    );

    // Visible in the recipient's inbox, oldest first, unread, and on both
    // logs exactly once each (the repeat recorded nothing).
    let inbox = b.inbox().unwrap();
    let ids: Vec<u64> = inbox.messages.iter().map(|m| m.id).collect();
    assert_eq!(ids, [first.id, second.id]);
    assert!(inbox.messages.iter().all(|m| inbox.is_unread(m)));
    assert_eq!(b.unread().unwrap().messages.len(), 2);
    for branch in ["a", "b"] {
        let logged: Vec<u64> = messages_on(&f, branch)
            .iter()
            .filter(|m| m.to == "b")
            .map(|m| m.id)
            .collect();
        assert_eq!(logged, [first.id, second.id], "{branch}'s log");
    }

    // Nothing was steered: b's next turn is given both, in order, with the
    // payload beside the text, and they count delivered then.
    root.send("b", "WHOAMI").unwrap();
    root.wait("b", Duration::from_secs(120)).unwrap();
    let events = f.yard.branch("b").unwrap().events().unwrap();
    let prompt = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert!(prompt.starts_with("<branchyard-inbox>"), "{prompt}");
    let first_line = format!(
        "[#{}] note from a: lexer done\npayload: {{\"tokens\":3}}\n",
        first.id
    );
    let second_line = format!(
        "[#{}] step from a: step two\npayload: {{\"n\":2}}\n",
        second.id
    );
    let (at_first, at_second) = (
        prompt
            .find(&first_line)
            .unwrap_or_else(|| panic!("{prompt}")),
        prompt
            .find(&second_line)
            .unwrap_or_else(|| panic!("{prompt}")),
    );
    assert!(at_first < at_second, "oldest first");
    assert!(prompt.ends_with("WHOAMI"), "{prompt}");
    let delivered: Vec<(Vec<u64>, DeliveredVia)> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::MessagesDelivered { ids, via } => Some((ids.clone(), via.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0].0, [first.id, second.id]);
    assert!(matches!(delivered[0].1, DeliveredVia::TurnStart { .. }));
    assert!(b.inbox().unwrap().messages.iter().all(|m| m.delivered));
    assert!(b.unread().unwrap().messages.is_empty());
}

/// The legacy paths ride the same pipeline with their output unchanged: a
/// question's message has no `meta` while the sender's turn has no trace,
/// and `peer` never reaches `authorize`'s legacy rules by another name.
#[test]
fn a_question_through_the_pipeline_reads_as_before_and_a_registration_is_checked() {
    let f = Fixture::new();
    let options = TaskOptions {
        trace_parent: None,
        ..delegating(&f)
    };
    let (root, a, _b) = siblings(&f, &options);
    let asked = a.ask("which lock?", None).unwrap();
    assert_eq!(asked.message.kind, MessageKind::Question);
    assert_eq!(
        asked.message.meta, None,
        "nothing to carry: the JSON is as it was"
    );
    assert!(!asked.message.next_turn);
    let json = serde_json::to_value(&asked.message).unwrap();
    assert!(
        json.get("meta").is_none() && json.get("next_turn").is_none(),
        "{json}"
    );
    let answer = root.answer(asked.message.id, "the mutex").unwrap();
    assert_eq!(answer.in_reply_to, Some(asked.message.id));
    // A built-in name cannot be taken, and a kind outside the schema
    // subset is refused where it stands.
    denied(
        f.yard
            .register_kind(KindSpec::new("question", Class::Peer).unwrap()),
        "built-in",
    );
    denied(
        KindSpec::new("ref", Class::Peer)
            .unwrap()
            .with_schema(json!({"$ref": "#/x"}))
            .map(|_| ()),
        "schema uses `$ref`",
    );
}
