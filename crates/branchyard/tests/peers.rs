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
use branchyard_testkit::wait;
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
        a.message("b", "hi", Some("nope"), None, None, None),
        "no message kind named \"nope\"",
    );
    denied(
        a.message(
            "b",
            "hi",
            Some("step"),
            Some(json!({"n": "one"})),
            None,
            None,
        ),
        "payload.n: expected integer",
    );
    denied(
        a.message(
            "b",
            "hi",
            Some("step"),
            Some(json!({"n": 1, "x": 2})),
            None,
            None,
        ),
        "payload: the field \"x\" is not allowed",
    );
    denied(
        a.message("b", "hi", Some("step"), None, None, None),
        "needs a payload",
    );
    denied(
        a.message("root", "hi", None, None, None, None),
        "may only message its siblings",
    );
    denied(
        a.message("@person", "hi", None, None, None, None),
        "may only message its siblings",
    );
    denied(
        root.message("a", "hi", None, None, None, None),
        "has no siblings",
    );
    denied(
        a.message("a", "hi", None, None, None, None),
        "cannot message itself",
    );
    denied(a.message("b", "   ", None, None, None, None), "needs text");
    assert!(matches!(
        a.message("nobody", "hi", None, None, None, None),
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
            None,
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
            None,
        )
        .unwrap();
    assert_eq!(again, first);
    denied(
        a.message("b", "something else", None, None, Some("k1"), None),
        "a key names one message",
    );
    // b may use the same key for its own message.
    let from_b = b.message("a", "ack", None, None, Some("k1"), None).unwrap();
    assert_ne!(from_b.id, first.id);

    // A second message of the registered kind, after the first.
    let second = a
        .message(
            "b",
            "step two",
            Some("step"),
            Some(json!({"n": 2})),
            Some("k2"),
            None,
        )
        .unwrap();
    assert!(second.id > first.id);
    assert_eq!(second.kind_name(), "step");
    assert_eq!(second.meta.as_ref().unwrap().schema_version, Some(1));
    // The key with another kind is another message, though both are stored
    // as the peer class and the text and payload match: refused, not
    // answered with the step.
    denied(
        a.message(
            "b",
            "step two",
            None,
            Some(json!({"n": 2})),
            Some("k2"),
            None,
        ),
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
    let asked = a.ask("which lock?", None, None).unwrap();
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

/// The last prompt `branch` was given.
fn last_prompt(f: &Fixture, branch: &str) -> String {
    f.yard
        .branch(branch)
        .unwrap()
        .events()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .next_back()
        .unwrap()
}

/// A message past its deadline is not given to the turn that starts after
/// it: it counts delivered (so it is never given late), the prompt omits
/// it, and the recipient's log carries a warning naming its id, while a
/// message without a deadline sent beside it is delivered as usual.
#[test]
fn a_message_past_its_deadline_is_dropped_with_a_warning_on_the_recipients_log() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, a, b) = siblings(&f, &options);
    let deadline = branchyard_support::time::now_ms() + 600;
    let stale = a
        .message("b", "use the old lexer", None, None, None, Some(deadline))
        .unwrap();
    assert_eq!(stale.meta.as_ref().unwrap().deadline_ms, Some(deadline));
    let live = a
        .message("b", "use the new lexer", None, None, None, None)
        .unwrap();
    // A deadline in the past is refused at the send, as before.
    denied(
        a.message("b", "late", None, None, None, Some(1)),
        "has passed",
    );
    wait::until("the deadline to pass", || {
        branchyard_support::time::now_ms() > deadline
    });

    root.send("b", "WHOAMI").unwrap();
    root.wait("b", Duration::from_secs(120)).unwrap();
    let prompt = last_prompt(&f, "b");
    assert!(prompt.contains("use the new lexer"), "{prompt}");
    assert!(
        !prompt.contains("use the old lexer"),
        "an expired message is not given to the turn: {prompt}"
    );
    let events = f.yard.branch("b").unwrap().events().unwrap();
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Warning(text) if text.contains("expired") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with(&format!("message #{} from a expired", stale.id)),
        "{}",
        warnings[0]
    );
    let delivered: Vec<u64> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::MessagesDelivered { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(delivered, [live.id], "only the live message was delivered");
    let inbox = b.inbox().unwrap();
    assert!(
        inbox.messages.iter().all(|m| m.delivered),
        "the expired message no longer waits"
    );

    // Another turn does not warn again.
    root.send("b", "WHOAMI").unwrap();
    root.wait("b", Duration::from_secs(120)).unwrap();
    let again = f.yard.branch("b").unwrap().events().unwrap();
    assert_eq!(
        again
            .iter()
            .filter(|e| matches!(&e.activity, Activity::Warning(t) if t.contains("expired")))
            .count(),
        1
    );
}

/// A recipient with PEER_PENDING_MAX undelivered peer messages refuses
/// another, naming the count and the limit; a repeat under a key still
/// returns the first message, and the refusal stores nothing.
#[test]
fn a_recipient_with_too_many_pending_peer_messages_refuses_another() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, a, b) = siblings(&f, &options);
    let max = branchyard::messaging::PEER_PENDING_MAX;
    let first = a
        .message("b", "note 0", None, None, Some("k0"), None)
        .unwrap();
    for n in 1..max {
        a.message("b", &format!("note {n}"), None, None, None, None)
            .unwrap();
    }
    assert_eq!(b.unread().unwrap().messages.len(), max);
    let refused = a
        .message("b", "one too many", None, None, None, None)
        .unwrap_err();
    match &refused {
        Error::Denied(reason) => assert!(
            reason.contains(&format!("already has {max} undelivered peer messages"))
                && reason.contains(&format!("the limit is {max}")),
            "{reason}"
        ),
        other => panic!("expected denied, got {other:?}"),
    }
    assert_eq!(b.unread().unwrap().messages.len(), max, "nothing stored");
    // A repeat of a message already sent is not a new one.
    let repeat = a
        .message("b", "note 0", None, None, Some("k0"), None)
        .unwrap();
    assert_eq!(repeat, first);
    // The count is the recipient's: b can still message a.
    b.message("a", "hello", None, None, None, None).unwrap();

    // Once b's next turn starts it reads its messages (at most 20 per
    // turn), which makes room.
    root.send("b", "WHOAMI").unwrap();
    root.wait("b", Duration::from_secs(120)).unwrap();
    a.message("b", "now there is room", None, None, None, None)
        .unwrap();
}

/// `[[messaging.kinds]]` registers kinds when a yard is configured, with
/// the refusals `register_kind` makes: a payload is checked against the
/// configured schema, configuring twice is not a duplicate, and a bad kind
/// is a config error naming it that registers none of the file's kinds.
#[test]
fn configured_kinds_are_registered_and_a_bad_one_names_itself() {
    let f = Fixture::new();
    let good = branchyard::config::parse(
        "[[messaging.kinds]]\nname = \"step\"\nschema_version = 4\n\
         description = \"a step finished\"\n\
         [messaging.kinds.schema]\ntype = \"object\"\nrequired = [\"n\"]\n\
         [messaging.kinds.schema.properties.n]\ntype = \"integer\"\n",
    )
    .unwrap();
    f.yard.configure(&good).unwrap();
    f.yard.configure(&good).unwrap();
    let step = f
        .yard
        .message_kinds()
        .into_iter()
        .find(|k| k.name == "step")
        .unwrap();
    assert_eq!((step.class, step.version), (Class::Peer, 4));
    assert_eq!(step.description.as_deref(), Some("a step finished"));

    let options = delegating(&f);
    let (_root, a, _b) = siblings(&f, &options);
    denied(
        a.message(
            "b",
            "x",
            Some("step"),
            Some(json!({"n": "one"})),
            None,
            None,
        ),
        "payload.n: expected integer",
    );
    let sent = a
        .message("b", "x", Some("step"), Some(json!({"n": 1})), None, None)
        .unwrap();
    assert_eq!(sent.meta.unwrap().schema_version, Some(4));

    for (text, needle) in [
        ("[[messaging.kinds]]\nname = \"note\"\n", "built-in"),
        (
            "[[messaging.kinds]]\nname = \"fresh\"\n[[messaging.kinds]]\nname = \"report\"\n",
            "\"report\"",
        ),
        (
            "[[messaging.kinds]]\nname = \"loose\"\n[messaging.kinds.schema]\n\"$ref\" = \"#/x\"\n",
            "$ref",
        ),
        (
            "[[messaging.kinds]]\nname = \"step\"\nschema_version = 5\n",
            "registered already",
        ),
    ] {
        let bad = branchyard::config::parse(text).unwrap();
        match f.yard.configure(&bad) {
            Err(Error::Config(why)) => assert!(why.contains(needle), "{text}: {why}"),
            other => panic!("{text}: expected a config error, got {other:?}"),
        }
    }
    let names: Vec<String> = f.yard.message_kinds().into_iter().map(|k| k.name).collect();
    assert!(
        !names.contains(&"fresh".to_owned()) && !names.contains(&"loose".to_owned()),
        "a refused file registers nothing: {names:?}"
    );
}

/// `ask` takes an idempotency key as `message` does: sender-scoped, a
/// repeat returns the first question (and its answer once there is one)
/// and records nothing again, and the same key with other text is refused.
#[test]
fn a_keyed_ask_returns_the_first_question_and_refuses_other_text() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, a, b) = siblings(&f, &options);
    let first = a.ask("which lock?", None, Some("q1")).unwrap();
    assert_eq!(first.message.kind, MessageKind::Question);
    let again = a.ask("which lock?", None, Some("q1")).unwrap();
    assert_eq!(again.message, first.message);
    denied(
        a.ask("another question?", None, Some("q1")),
        "a key names one message",
    );
    // No key: a second question. Another sender may use the same key.
    let plain = a.ask("which lock?", None, None).unwrap();
    assert_ne!(plain.message.id, first.message.id);
    let from_b = b.ask("which lock?", None, Some("q1")).unwrap();
    assert_ne!(from_b.message.id, first.message.id);
    let asked: Vec<u64> = messages_on(&f, "a")
        .iter()
        .filter(|m| m.kind == MessageKind::Question)
        .map(|m| m.id)
        .collect();
    assert_eq!(asked, [first.message.id, plain.message.id], "recorded once");

    // Once answered, the same keyed ask finds the answer.
    let answer = root.answer(first.message.id, "the mutex").unwrap();
    let repeat = a
        .ask("which lock?", Some(Duration::from_secs(30)), Some("q1"))
        .unwrap();
    assert_eq!(repeat.message.id, first.message.id);
    assert_eq!(repeat.answer.map(|m| m.id), Some(answer.id));
}
