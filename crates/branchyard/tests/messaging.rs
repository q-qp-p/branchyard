//! Harness-to-harness messages against the fake ACP agent: authority
//! follows the delegation tree, `ask --wait` answers across the store's
//! waits, and pending messages are delivered at a branch's next turn.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::time::Duration;

use branchyard::stage::Reason;
use branchyard::{
    Activity, BranchStatus, Budget, DeliveredVia, Envelope, Error, MessageKind, Policy,
    ReportedState, Spawn, TaskOptions, PERSON,
};
use branchyard_testkit::wait;
use common::{fake_agent, text, Fixture};

fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn spawn(prompt: &str, name: &str) -> Spawn {
    Spawn {
        prompt: prompt.into(),
        name: Some(name.into()),
        ..Spawn::default()
    }
}

/// Wait until `branch`'s own first turn has recorded its `Activity::Prompt`
/// (past delivery at that turn's start), so a message sent to it afterwards
/// is never raced into that same turn's prompt instead of staying pending
/// for a later one, as `begin_submit` would otherwise be free to
/// do if the turn's handshake with its harness is still in flight.
fn wait_for_own_prompt(f: &Fixture, branch: &str) {
    wait::until(&format!("{branch} to record its own prompt"), || {
        f.yard
            .branch(branch)
            .and_then(|b| b.events())
            .is_ok_and(|events| {
                events
                    .iter()
                    .any(|e| matches!(e.activity, Activity::Prompt(_)))
            })
    });
}

#[test]
fn a_child_reports_and_asks_its_parent_across_both_event_logs() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let root_delegate = root.delegate(options.clone()).unwrap();
    let kid = root_delegate
        .spawn(spawn("HANG", "kid"))
        .expect("a child within the envelope");
    assert_eq!(kid.depth, 1);
    wait_for_own_prompt(&f, "kid");
    let kid_delegate = f
        .yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap();

    let report = kid_delegate.report("tests pass").unwrap();
    assert_eq!(report.from, "kid");
    assert_eq!(report.to, "root");
    assert_eq!(report.kind, branchyard::MessageKind::Report);
    assert!(report.id > 0);

    // Recorded on both event logs, so `by events`/`log` show it either way.
    for branch in ["kid", "root"] {
        let events = f.yard.branch(branch).unwrap().events().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.activity, Activity::Message(m) if m.id == report.id)),
            "{branch}'s log is missing the message"
        );
    }

    let asked = kid_delegate
        .ask("should I rename the module?", None)
        .unwrap();
    assert!(asked.answer.is_none(), "no --wait, no answer expected yet");
    assert!(asked.message.in_reply_to.is_none());
    let reason = |name: &str| {
        f.yard
            .branch(name)
            .unwrap()
            .info()
            .stage
            .map(|stage| stage.reason)
    };
    assert_eq!(reason("kid"), Some(Reason::Question));

    let answer = root_delegate
        .answer(asked.message.id, "yes, rename it")
        .unwrap();
    assert_eq!(reason("kid"), Some(Reason::Running));
    assert_eq!(answer.to, "kid");
    assert_eq!(answer.in_reply_to, Some(asked.message.id));

    let inbox = kid_delegate.inbox().unwrap();
    assert_eq!(inbox.branch, "kid");
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].id, answer.id);
    // kid's HANG turn is running and the fake agent takes steering, so the
    // answer was steered straight into it, not left for its next turn.
    assert!(inbox.messages[0].delivered, "steered into the running turn");
    // The engine records the delivery event just after it settles the
    // steer that marked the message delivered; wait for the entry.
    wait::until("the steered delivery to be recorded", || {
        let kid_log = f.yard.branch("kid").unwrap().events().unwrap();
        kid_log.iter().any(|e| matches!(&e.activity,
            Activity::MessagesDelivered { ids, via: DeliveredVia::Steer { .. } } if ids == &[answer.id]))
    });

    // A fresh question, answered from a background thread while `ask`
    // blocks for it: the wait sees the answer across processes' writes,
    // here the same process's, at once.
    let waiting_kid = f
        .yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let questions = root_delegate.inbox().unwrap().messages.len();
    let waiter = std::thread::spawn(move || {
        waiting_kid.ask("ready to merge?", Some(Duration::from_secs(10)))
    });
    wait::until("the new question to reach the root's inbox", || {
        root_delegate.inbox().unwrap().messages.len() > questions
    });
    let root_inbox = root_delegate.inbox().unwrap();
    let latest = root_inbox
        .messages
        .last()
        .expect("the new question arrived");
    root_delegate.answer(latest.id, "yes").unwrap();
    let asked2 = waiter.join().unwrap().unwrap();
    assert_eq!(
        asked2.answer.map(|a| a.text),
        Some("yes".to_owned()),
        "ask --wait should see the answer sent while it blocked"
    );

    // Delivery: the parent's next turn is given every pending message,
    // acknowledged (marked delivered) in the same step, so it is not
    // delivered twice.
    f.yard
        .branch("root")
        .unwrap()
        .send("WHOAMI", options.clone())
        .unwrap();
    wait::until("the root's turn to end", || {
        f.yard.branch("root").unwrap().info().status != BranchStatus::Running
    });
    let events = f.yard.branch("root").unwrap().events().unwrap();
    let prompt = events
        .iter()
        .rev()
        .find_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.clone()),
            _ => None,
        })
        .expect("a prompt was recorded");
    assert!(prompt.contains("<branchyard-inbox>"), "{prompt}");
    assert!(prompt.ends_with("WHOAMI"), "{prompt}");
    // root had no running turn when they were sent: delivered at the start.
    assert!(events.iter().any(|e| matches!(&e.activity,
        Activity::MessagesDelivered { via: DeliveredVia::TurnStart { .. }, ids } if ids.len() == 3)));

    let now_delivered = root_delegate.inbox().unwrap();
    assert!(
        now_delivered.messages.iter().all(|m| m.delivered),
        "{now_delivered:?}"
    );
}

#[test]
fn messaging_authority_follows_the_delegation_tree() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let root_delegate = root.delegate(options.clone()).unwrap();
    root_delegate.spawn(spawn("HANG", "a")).expect("child a");
    root_delegate.spawn(spawn("HANG", "b")).expect("child b");
    wait_for_own_prompt(&f, "a");
    wait_for_own_prompt(&f, "b");

    let a_delegate = f
        .yard
        .branch("a")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let b_delegate = f.yard.branch("b").unwrap().delegate(options).unwrap();

    // A branch answers only its own descendants, never a sibling's.
    let question = a_delegate.ask("ok?", None).unwrap().message;
    match b_delegate.answer(question.id, "no") {
        Err(Error::Denied(why)) => assert!(
            why.contains("descendants"),
            "expected an authority refusal, got {why:?}"
        ),
        other => panic!("expected Denied, got {other:?}"),
    }

    // A question or a report may go only to one's own parent, not a sibling
    // or an unrelated branch.
    match a_delegate.answer(question.id, "yes") {
        // a is not an ancestor of a: refused as self-messaging or as not a
        // descendant, either way not honored.
        Err(Error::Denied(_)) => {}
        other => panic!("expected Denied, got {other:?}"),
    }
}

fn events(f: &Fixture, branch: &str) -> Vec<branchyard::RecordedEvent> {
    f.yard.branch(branch).unwrap().events().unwrap()
}

fn prompts(events: &[branchyard::RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn delivered(events: &[branchyard::RecordedEvent]) -> Vec<(Vec<u64>, DeliveredVia)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::MessagesDelivered { ids, via } => Some((ids.clone(), via.clone())),
            _ => None,
        })
        .collect()
}

/// `root` has finished one turn and has a finished child `kid`; returns
/// kid's delegate.
fn parent_and_child(f: &Fixture, options: &TaskOptions) -> branchyard::Delegate {
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    root.delegate(options.clone())
        .unwrap()
        .spawn(spawn("say hi", "kid"))
        .unwrap();
    wait::until("kid's turn to end", || {
        f.yard.branch("kid").unwrap().info().status != BranchStatus::Running
    });
    f.yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap()
}

#[test]
fn a_message_is_steered_into_the_parents_running_turn_and_never_repeated() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);

    // root's next turn waits for steering, as a busy parent would.
    let root = f.yard.branch("root").unwrap();
    let turn = {
        let options = options.clone();
        std::thread::spawn(move || root.send("AWAIT_STEER", options))
    };
    wait::until("root to wait for steering", || {
        text(&events(&f, "root")).contains("waiting for steering")
    });

    let report = kid.report("tests pass").unwrap();
    // The sender waited until the running turn took it.
    let inbox = f
        .yard
        .branch("root")
        .unwrap()
        .delegate(options.clone())
        .unwrap()
        .inbox()
        .unwrap();
    assert!(inbox.messages.iter().all(|m| m.delivered), "{inbox:?}");

    let root = turn.join().unwrap().unwrap();
    assert_eq!(root.info().status, BranchStatus::NoChanges);
    let log = events(&f, "root");
    let said = text(&log);
    assert!(said.contains("steered: <branchyard-inbox>"), "{said}");
    assert!(
        said.contains(&format!("[#{}] report from kid: tests pass", report.id)),
        "{said}"
    );
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Steered { by, .. } if by == "kid")));
    let paths = delivered(&log);
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0].0, [report.id]);
    assert!(
        matches!(paths[0].1, DeliveredVia::Steer { .. }),
        "{paths:?}"
    );

    // The next turn is not given it again.
    let root = root.send("WHOAMI", options.clone()).unwrap();
    let log = root.events().unwrap();
    let last = prompts(&log).pop().unwrap();
    assert!(!last.contains("<branchyard-inbox>"), "{last}");
    assert_eq!(delivered(&log).len(), 1, "no second delivery");
}

#[test]
fn a_message_waits_for_the_next_turn_when_the_parent_cannot_steer() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);

    // The same agent, without the steering extension.
    let no_steer = TaskOptions {
        command: Some(vec![
            "env".into(),
            "FAKE_ACP_NO_STEER=1".into(),
            fake_agent().display().to_string(),
        ]),
        ..options.clone()
    };
    let root = f.yard.branch("root").unwrap();
    let turn = std::thread::spawn(move || root.send("HANG", no_steer));
    wait::until("root's turn to run", || {
        prompts(&events(&f, "root")).len() == 2
    });

    let question = kid.ask("rename the module?", None).unwrap().message;
    let root_inbox = || {
        f.yard
            .branch("root")
            .unwrap()
            .delegate(options.clone())
            .unwrap()
            .inbox()
            .unwrap()
    };
    assert!(!root_inbox().messages[0].delivered, "left pending");
    // Its steered input was refused, and the turn goes on.
    wait::until("the steer to be refused", || {
        events(&f, "root").iter().any(|e| {
            matches!(&e.activity,
            Activity::Warning(w) if w.contains("was not delivered"))
        })
    });
    assert_eq!(
        f.yard.branch("root").unwrap().info().status,
        BranchStatus::Running
    );
    assert!(!f.yard.cancel("root").unwrap().is_empty());
    turn.join().unwrap().unwrap();
    assert!(!root_inbox().messages[0].delivered, "still pending");

    // Delivered once, at the next turn's start.
    let root = f
        .yard
        .branch("root")
        .unwrap()
        .send("WHOAMI", options.clone())
        .unwrap();
    let log = root.events().unwrap();
    let last = prompts(&log).pop().unwrap();
    assert!(
        last.contains(&format!("[#{}] question from kid", question.id)),
        "{last}"
    );
    assert_eq!(
        delivered(&log),
        [(
            vec![question.id],
            DeliveredVia::TurnStart {
                boundary: "turn_start".to_owned()
            }
        )]
    );
    assert!(root_inbox().messages[0].delivered);
    let root = root.send("WHOAMI", options).unwrap();
    let last = prompts(&root.events().unwrap()).pop().unwrap();
    assert!(!last.contains("<branchyard-inbox>"), "{last}");
}

#[test]
fn a_turn_waiting_for_an_answer_is_not_stalled() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);
    // kid's next turn hangs, with a short stall window.
    let stalling = TaskOptions {
        budget: Budget::default().stall_after(Duration::from_millis(400)),
        ..options.clone()
    };
    let branch = f.yard.branch("kid").unwrap();
    let turn = std::thread::spawn(move || branch.send("HANG", stalling));
    wait::until("kid's turn to run", || {
        prompts(&events(&f, "kid")).len() == 2
    });
    // The harness blocks in `ask --wait`: the same call a harness's `by ask
    // --wait` reaches through the broker, recorded in the store.
    let asking = std::thread::spawn(move || kid.ask("which way?", Some(Duration::from_secs(2))));
    wait::settle(
        "the stall window passes while the turn waits for an answer",
        Duration::from_millis(1500),
    );
    let stalled = |f: &Fixture| {
        events(f, "kid")
            .iter()
            .any(|e| matches!(e.activity, Activity::Stalled { .. }))
    };
    assert!(!stalled(&f), "not stalled while it waits for an answer");
    assert!(!f.yard.branch("kid").unwrap().info().stalled);
    let asked = asking.join().unwrap().unwrap();
    assert!(asked.answer.is_none());
    // Once the wait is over, the turn's idle time counts again.
    wait::until("kid to stall", || stalled(&f));
    assert!(!f.yard.cancel("kid").unwrap().is_empty());
    let kid = turn.join().unwrap().unwrap();
    assert_eq!(kid.info().status, BranchStatus::Interrupted);
}

const PULL: &str = "https://example.com/pull/7";

/// A branch's own account of itself is recorded on its log, shown to its
/// parent in `inspect` beside the protocol's status, and changes nothing
/// about that status.
#[test]
fn a_childs_own_status_is_seen_by_its_parent_beside_its_protocol_status() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let parent = root.delegate(options.clone()).unwrap();
    parent.spawn(spawn("HANG", "kid")).unwrap();
    wait_for_own_prompt(&f, "kid");
    let kid = f
        .yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap();

    assert_eq!(parent.inspect("kid").unwrap().reported, None);
    let first = kid
        .report_status(
            ReportedState::Working,
            "  reading the failing test ",
            Some(PULL),
            None,
        )
        .unwrap();
    assert_eq!(first.report.state, ReportedState::Working);
    assert_eq!(first.report.summary, "reading the failing test");
    assert_eq!(first.report.link.as_deref(), Some(PULL));
    assert!(first.report.at_ms > 0);
    assert!(first.escalation.is_none(), "working asks for nobody");

    let seen = parent.inspect("kid").unwrap();
    assert_eq!(
        seen.status,
        BranchStatus::Running,
        "the claim is not a status"
    );
    assert_eq!(seen.reported, Some(first.report.clone()));
    assert_eq!(
        kid.inspect("kid").unwrap().reported,
        Some(first.report.clone())
    );
    let wire = serde_json::to_value(&seen).unwrap();
    assert_eq!(
        wire["reported"],
        serde_json::json!({
            "state": "working",
            "summary": "reading the failing test",
            "link": PULL,
            "at_ms": first.report.at_ms,
        })
    );

    // The latest report is the one shown; the log keeps both, in order.
    let second = kid
        .report_status(ReportedState::Complete, "done", None, None)
        .unwrap();
    assert_eq!(
        parent.inspect("kid").unwrap().reported,
        Some(second.report.clone())
    );
    let recorded: Vec<(ReportedState, String, Option<String>, u64)> = events(&f, "kid")
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::StatusReport {
                state,
                summary,
                link,
                ..
            } => Some((state, summary, link, e.at_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        [
            (
                ReportedState::Working,
                "reading the failing test".to_owned(),
                Some(PULL.to_owned()),
                first.report.at_ms
            ),
            (
                ReportedState::Complete,
                "done".to_owned(),
                None,
                second.report.at_ms
            ),
        ]
    );
    // Neither state is a call for attention: the parent's inbox is empty.
    assert!(parent.inbox().unwrap().messages.is_empty());
}

/// `blocked` and `failure` become an escalation in the parent's inbox, as
/// `escalate` would; the other states and an exact repeat do not.
#[test]
fn a_blocked_or_failed_report_is_escalated_to_the_parent_once() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);
    let parent = f
        .yard
        .branch("root")
        .unwrap()
        .delegate(options.clone())
        .unwrap();

    let blocked = kid
        .report_status(
            ReportedState::Blocked,
            "needs the API key",
            Some(PULL),
            None,
        )
        .unwrap();
    let message = blocked.escalation.expect("a blocked report is escalated");
    assert_eq!(
        (message.from.as_str(), message.to.as_str(), message.kind),
        ("kid", "root", MessageKind::Escalation)
    );
    assert_eq!(
        message.text,
        format!("status blocked: needs the API key ({PULL})")
    );
    let inbox = parent.inbox().unwrap();
    assert_eq!(
        inbox.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        [message.id]
    );
    // Both logs show it, like any message.
    for branch in ["kid", "root"] {
        assert!(
            events(&f, branch)
                .iter()
                .any(|e| matches!(&e.activity, Activity::Message(m) if m.id == message.id)),
            "{branch}'s log is missing the escalation"
        );
    }

    // The same report again is recorded, and the parent is not told twice.
    let again = kid
        .report_status(
            ReportedState::Blocked,
            "needs the API key",
            Some(PULL),
            None,
        )
        .unwrap();
    assert!(again.escalation.is_none());
    assert_eq!(parent.inbox().unwrap().messages.len(), 1);

    // A claim that asks for nobody, then a different one that does.
    for state in [
        ReportedState::Working,
        ReportedState::Idle,
        ReportedState::Complete,
    ] {
        let said = kid.report_status(state, "x", None, None).unwrap();
        assert!(said.escalation.is_none(), "{state} escalated");
    }
    let failed = kid
        .report_status(ReportedState::Failure, "gave up\non the parser", None, None)
        .unwrap();
    assert_eq!(
        failed.escalation.expect("failure is escalated").text,
        "status failure: gave up on the parser"
    );
    assert_eq!(parent.inbox().unwrap().messages.len(), 2);
}

/// A bad summary or link is refused with the usual denial and recorded
/// nowhere; a root's blocked report, like its `escalate`, reaches the
/// person.
#[test]
fn a_bad_status_report_is_refused_and_a_root_escalates_to_the_person() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);
    let refused = |summary: &str, link: Option<&str>| match kid.report_status(
        ReportedState::Working,
        summary,
        link,
        None,
    ) {
        Err(Error::Denied(why)) => why,
        other => panic!("expected a denial, got {other:?}"),
    };
    assert!(refused(&"x".repeat(281), None).contains("281 characters"));
    assert!(refused("   ", None).contains("needs a summary"));
    assert!(refused("ok", Some("javascript:alert(1)")).contains("http or https URL"));
    assert!(!events(&f, "kid")
        .iter()
        .any(|e| matches!(e.activity, Activity::StatusReport { .. })));
    // 280 characters is the limit, counted in characters, not bytes.
    kid.report_status(ReportedState::Working, &"é".repeat(280), None, None)
        .unwrap();

    // A time-to-live is whole seconds from 1 to 7 days.
    for bad in [
        Duration::ZERO,
        Duration::from_millis(1500),
        Duration::from_secs(7 * 24 * 3600 + 1),
    ] {
        match kid.report_status(ReportedState::Working, "x", None, Some(bad)) {
            Err(Error::Denied(why)) => assert!(why.contains("ttl_seconds"), "{why}"),
            other => panic!("{bad:?} was not refused: {other:?}"),
        }
    }
    let timed = kid
        .report_status(
            ReportedState::Blocked,
            "waiting on CI",
            None,
            Some(Duration::from_secs(600)),
        )
        .unwrap()
        .report;
    assert_eq!(timed.ttl_seconds, Some(600));
    assert_eq!(timed.expires_at_ms, Some(timed.at_ms + 600_000));
    assert!(!timed.stale);
    let shown = kid.inspect("kid").unwrap().reported.unwrap();
    assert_eq!(shown, timed, "read well within its ttl, it is current");

    let root = f.yard.branch("root").unwrap().delegate(options).unwrap();
    let said = root
        .report_status(ReportedState::Blocked, "needs a decision", None, None)
        .unwrap();
    let escalation = said.escalation.expect("the person hears of it");
    assert_eq!(
        (
            escalation.from.as_str(),
            escalation.to.as_str(),
            escalation.kind
        ),
        ("root", PERSON, MessageKind::Escalation)
    );
    assert_eq!(
        root.inspect("root").unwrap().reported.unwrap().state,
        ReportedState::Blocked
    );
    let escalated = root.escalate("postgres or sqlite?").unwrap();
    assert_eq!(escalated.to, PERSON);
    let inbox = f.yard.inbox().unwrap();
    assert_eq!(inbox.branch, PERSON);
    assert_eq!(
        inbox.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        [escalation.id, escalated.id]
    );
    assert_eq!(reason(&f, "root"), Some(Reason::Question));
}

fn reason(f: &Fixture, name: &str) -> Option<Reason> {
    f.yard
        .branch(name)
        .unwrap()
        .info()
        .stage
        .map(|stage| stage.reason)
}

/// A root started with delegation was delegated by nobody, so its question
/// goes to the person. It stays in Needs you while it runs, and after its
/// turn ends, until someone answers.
#[test]
fn a_root_asks_the_person_and_still_asks_after_its_turn_ends() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let turn = {
        let task = f.yard.task("HANG").options(options.clone()).name("root");
        std::thread::spawn(move || task.run())
    };
    wait_for_own_prompt(&f, "root");
    let root = f.yard.branch("root").unwrap().delegate(options).unwrap();

    let asked = root.ask("which database?", None).unwrap().message;
    assert_eq!((asked.from.as_str(), asked.to.as_str()), ("root", PERSON));
    assert!(events(&f, "root")
        .iter()
        .any(|e| matches!(&e.activity, Activity::Message(m) if m.id == asked.id)));
    assert_eq!(
        f.yard.inbox().unwrap().messages,
        std::slice::from_ref(&asked)
    );
    assert_eq!(reason(&f, "root"), Some(Reason::Question));

    assert!(!f.yard.cancel("root").unwrap().is_empty());
    let ended = turn.join().unwrap().unwrap();
    assert_eq!(ended.info().status, BranchStatus::Interrupted);
    assert_eq!(reason(&f, "root"), Some(Reason::Question));
}

/// A fork's `parent` is the branch it was forked from, which did not give
/// it its task and cannot answer it, so its question goes to the person. A
/// delegated child still asks its parent.
#[test]
fn a_fork_asks_the_person_and_a_delegated_child_its_parent() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let source = f
        .yard
        .task("WRITE a.txt=1")
        .options(options.clone())
        .name("source")
        .run()
        .unwrap();
    let fork = source
        .fork(
            "say hi",
            true,
            TaskOptions {
                name: Some("fork".into()),
                ..options.clone()
            },
        )
        .unwrap();
    assert_eq!(
        (fork.info().parent.as_deref(), fork.info().depth),
        (Some("source"), 0)
    );
    let asked = fork
        .delegate(options.clone())
        .unwrap()
        .ask("which way?", None);
    assert_eq!(asked.unwrap().message.to, PERSON);

    source
        .delegate(options.clone())
        .unwrap()
        .spawn(spawn("say hi", "kid"))
        .unwrap();
    wait::until("kid's turn to end", || {
        f.yard.branch("kid").unwrap().info().status != BranchStatus::Running
    });
    let kid = f.yard.branch("kid").unwrap().delegate(options).unwrap();
    assert_eq!(kid.ask("which way?", None).unwrap().message.to, "source");
}

/// The person's answer reaches a root blocked in `ask` with a wait, and is
/// steered into its running turn.
#[test]
fn the_persons_answer_reaches_a_root_waiting_in_ask() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let turn = {
        let task = f.yard.task("HANG").options(options.clone()).name("root");
        std::thread::spawn(move || task.run())
    };
    wait_for_own_prompt(&f, "root");
    let root = f.yard.branch("root").unwrap().delegate(options).unwrap();
    let asking =
        std::thread::spawn(move || root.ask("which database?", Some(Duration::from_secs(30))));
    wait::until("the question to reach the person", || {
        !f.yard.inbox().unwrap().messages.is_empty()
    });
    let question = f.yard.inbox().unwrap().messages.remove(0);

    let answer = f.yard.answer(question.id, "postgres").unwrap();
    assert_eq!(
        (answer.from.as_str(), answer.to.as_str(), answer.kind),
        (PERSON, "root", MessageKind::Answer)
    );
    assert_eq!(answer.in_reply_to, Some(question.id));
    let asked = asking.join().unwrap().unwrap();
    let waited = asked.answer.expect("the wait sees the person's answer");
    assert_eq!(
        (waited.id, waited.from.as_str(), waited.text.as_str()),
        (answer.id, PERSON, "postgres")
    );
    assert_eq!(reason(&f, "root"), Some(Reason::Running));
    wait::until("the answer to be steered into root's turn", || {
        delivered(&events(&f, "root"))
            .iter()
            .any(|(ids, via)| ids == &[answer.id] && matches!(via, DeliveredVia::Steer { .. }))
    });

    assert!(!f.yard.cancel("root").unwrap().is_empty());
    turn.join().unwrap().unwrap();
}

/// An answer to a root whose turn ended starts no turn: it waits, and is
/// given at the start of the root's next turn.
#[test]
fn the_persons_answer_to_a_root_whose_turn_ended_waits_for_its_next_turn() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let question = delegate.ask("which database?", None).unwrap().message;
    assert_eq!(reason(&f, "root"), Some(Reason::Question));

    let answer = f.yard.answer(question.id, "postgres").unwrap();
    assert_ne!(reason(&f, "root"), Some(Reason::Question));
    let root = f.yard.branch("root").unwrap();
    assert_eq!(root.info().turns, 1, "the answer started no turn");
    let pending: Vec<(u64, bool)> = delegate
        .inbox()
        .unwrap()
        .messages
        .iter()
        .map(|m| (m.id, m.delivered))
        .collect();
    assert_eq!(pending, [(answer.id, false)]);

    let root = root.send("WHOAMI", options).unwrap();
    let log = root.events().unwrap();
    let last = prompts(&log).pop().unwrap();
    assert!(
        last.contains(&format!("[#{}] answer from {PERSON}: postgres", answer.id)),
        "{last}"
    );
    assert_eq!(
        delivered(&log),
        [(
            vec![answer.id],
            DeliveredVia::TurnStart {
                boundary: "turn_start".to_owned()
            }
        )]
    );
}

/// The person answers a question or an escalation once, with text, and
/// nothing else; one sent to a branch is answered as that branch.
#[test]
fn the_person_answers_each_question_once_and_nothing_else() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);
    let report = kid.report("tests pass").unwrap();
    let to_root = kid.ask("rename it?", None).unwrap().message;
    let root = f.yard.branch("root").unwrap().delegate(options).unwrap();
    let to_person = root.ask("which database?", None).unwrap().message;
    let denied = |id: u64, text: &str| match f.yard.answer(id, text) {
        Err(Error::Denied(why)) => why,
        other => panic!("expected a denial, got {other:?}"),
    };

    let why = denied(report.id, "thanks");
    assert!(why.contains("only a question or an escalation"), "{why}");
    assert!(denied(to_person.id, "  ").contains("needs text"));
    assert!(matches!(
        f.yard.answer(9999, "yes"),
        Err(Error::UnknownMessage(9999))
    ));

    let answer = f.yard.answer(to_person.id, "postgres").unwrap();
    assert_eq!((answer.from.as_str(), answer.to.as_str()), (PERSON, "root"));
    let why = denied(to_person.id, "sqlite");
    assert!(why.contains("answered already"), "{why}");

    let as_root = f.yard.answer(to_root.id, "yes").unwrap();
    assert_eq!(
        (as_root.from.as_str(), as_root.to.as_str()),
        ("root", "kid")
    );
    assert_eq!(as_root.in_reply_to, Some(to_root.id));
    let inbox: Vec<u64> = f
        .yard
        .inbox()
        .unwrap()
        .messages
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(inbox, [to_person.id]);
}

/// No turn reads the person's inbox, so a question in it counts read once
/// it is answered: it stays listed, with its id in `answered`, and only the
/// question still waiting is unread.
#[test]
fn the_persons_inbox_marks_an_answered_question_read() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let answered = delegate.ask("which database?", None).unwrap().message;
    let waiting = delegate.ask("which port?", None).unwrap().message;
    f.yard.answer(answered.id, "postgres").unwrap();

    let inbox = f.yard.inbox().unwrap();
    let listed: Vec<u64> = inbox.messages.iter().map(|m| m.id).collect();
    assert_eq!(listed, [answered.id, waiting.id]);
    assert_eq!(inbox.answered, [answered.id]);
    assert!(!inbox.is_unread(&inbox.messages[0]));
    assert!(inbox.is_unread(&inbox.messages[1]));
    let unread = inbox.unread_only();
    let listed: Vec<u64> = unread.messages.iter().map(|m| m.id).collect();
    assert_eq!((listed, unread.answered), (vec![waiting.id], vec![]));
}
