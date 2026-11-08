//! Observations, hermetically (`docs/memory.md`, section 2): each kind
//! derived from a synthesized log, the extractor's skip list honoured, an
//! external author recorded, the idempotency key over the incarnation, and
//! the scrubber over a generated corpus of secrets with the quarter rule.
//! No store and no model: the engine's path through a real turn is
//! `crates/branchyard/tests/memory.rs`.

use super::consolidate::{cluster, pass, title_of, Cluster};
use super::observe::*;
use super::scrub::Scrubber;
use super::store::{MemoryRecord, MemoryStatus, Scope};
use super::MemoryKind;
use crate::fleet::{FleetActivity, JudgeMark, TaskKind};
use crate::goal::{GoalActivity, GoalCheck};
use crate::plan::PlanActivity;
use crate::pull_request::{CheckRun, PullRequestActivity};
use crate::reported::ReportedState;
use crate::{
    prompt_digest, Activity, BranchStatus, DecisionSource, Feedback, FeedbackItem, FeedbackKind,
    FeedbackSource, JudgedBy, Message, MessageKind, RecordedEvent, ToolTarget,
};
use branchyard_harness::target::ScrubKind;
use branchyard_harness::Event;

fn at(activity: Activity) -> RecordedEvent {
    RecordedEvent {
        at_ms: 1_000,
        activity,
    }
}

fn prompt(text: &str) -> RecordedEvent {
    at(Activity::Prompt(text.into()))
}

/// A feed: `(seq, event)` with seq from 1.
fn feed(events: Vec<RecordedEvent>) -> Vec<(u64, RecordedEvent)> {
    events
        .into_iter()
        .enumerate()
        .map(|(n, e)| (n as u64 + 1, e))
        .collect()
}

fn tool(command: &str) -> RecordedEvent {
    at(Activity::Harness(Event::ToolStarted {
        turn: 1,
        call_id: "c".into(),
        name: "bash".into(),
        target: ToolTarget::command(command),
    }))
}

fn context<'a>(turn: u32, members: &'a [String], visibility: &'a Visibility) -> Context<'a> {
    Context {
        tenant: "local",
        repo: "app",
        branch: "fix",
        incarnation: 7,
        turn,
        visibility,
        harness: "fake",
        model: Some("small"),
        task_kind: Some(TaskKind::Bugfix),
        members,
        at_ms: 2_000,
    }
}

fn kinds(drafts: &[Draft]) -> Vec<(ObservationKind, &str)> {
    drafts.iter().map(|d| (d.kind, d.text.as_str())).collect()
}

#[test]
fn every_kind_is_derived_from_its_events_and_the_skip_list_holds() {
    let review = "File: src/a.rs\nLine: 3\nUser comment: \"Name the constant after its unit\"";
    let members = vec!["ann".to_owned(), "ann@example.com".to_owned()];
    let feedback = Feedback {
        source: FeedbackSource::PullRequest,
        reference: Some("https://x/4".into()),
        items: vec![
            FeedbackItem {
                kind: FeedbackKind::Comment,
                path: Some("src/lib.rs".into()),
                line: Some(3),
                author: Some("@drive-by".into()),
                body: "Never unwrap in library code".into(),
                ..FeedbackItem::default()
            },
            FeedbackItem {
                kind: FeedbackKind::Comment,
                path: None,
                author: Some("ann".into()),
                body: "Keep the changelog in step".into(),
                ..FeedbackItem::default()
            },
            FeedbackItem {
                kind: FeedbackKind::Check,
                body: "CI check \"test\" failed on abc".into(),
                ..FeedbackItem::default()
            },
        ],
        prompt: Some(prompt_digest(review)),
    };
    let events = feed(vec![
        prompt("Fix the parser"),
        at(Activity::Status(BranchStatus::Ready)),
        // Turn 2: a correction, a steer, skipped prompts, the lot.
        at(Activity::Feedback(Box::new(feedback))),
        prompt(review),
        at(Activity::Steered {
            id: 1,
            by: "by send".into(),
            text: "Prefer small commits over one big one".into(),
        }),
        at(Activity::Steered {
            id: 2,
            by: "by send".into(),
            text: "ok".into(),
        }),
        at(Activity::Steered {
            id: 3,
            by: "by send".into(),
            text: "[branchyard plan approved] carry it out".into(),
        }),
        at(Activity::Decision {
            tool: "Bash".into(),
            allowed: false,
            message: Some("no".into()),
            source: DecisionSource::Rule {
                pattern: "Bash(rm *)".into(),
            },
        }),
        at(Activity::Decision {
            tool: "Read".into(),
            allowed: true,
            message: None,
            source: DecisionSource::Default,
        }),
        at(Activity::PullRequest(Box::new(
            PullRequestActivity::Checked(CheckRun {
                commit: "abc".into(),
                argv: vec!["cargo".into(), "test".into()],
                passed: false,
                timed_out: false,
                output_tail: (1..=45)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }),
        ))),
        at(Activity::Goal(Box::new(GoalActivity::Verdict {
            round: 0,
            turn: 2,
            met: false,
            evidence: vec!["tests ran".into()],
            missing: vec!["the changelog entry".into(), "a test".into()],
            deterministic: GoalCheck {
                check: "failed".into(),
                changes: true,
                passed: false,
            },
            by: JudgedBy::Deterministic,
        }))),
        at(Activity::Fleet(Box::new(FleetActivity::Judged(
            JudgeMark {
                score: 40.0,
                rank: 2,
                of: 2,
                picked: false,
                by: "deterministic".into(),
                reason: Some("no test for the fix".into()),
                check: "failed".into(),
            },
        )))),
        at(Activity::Plan(Box::new(PlanActivity::Rejected {
            by: "ann".into(),
            reason: Some("too broad".into()),
            replan: true,
            round: 1,
        }))),
        at(Activity::Message(Message {
            id: 1,
            from: "fix".into(),
            to: "person".into(),
            kind: MessageKind::Escalation,
            text: "I need the staging credentials".into(),
            in_reply_to: None,
            at_ms: 0,
            delivered: false,
            handoff: None,
            next_turn: false,
            meta: None,
        })),
        at(Activity::StatusReport {
            state: ReportedState::Blocked,
            summary: "waiting on a credential".into(),
            link: None,
            ttl_seconds: None,
        }),
        tool("cargo test -p parser"),
        tool("cargo test -p parser"),
        tool("ls"),
        at(Activity::TurnCost { cost_usd: 1.25 }),
        at(Activity::Status(BranchStatus::Failed {
            reason: "the check failed".into(),
        })),
        // Turn 3: nothing of this turn belongs to turn 2.
        prompt("continue"),
    ]);
    let shared = Visibility::Shared;
    let first = derive(&events, &context(1, &members, &shared));
    assert!(first.is_empty(), "the first prompt is the task: {first:?}");
    let drafts = derive(&events, &context(2, &members, &shared));
    assert_eq!(
        kinds(&drafts),
        [
            (
                ObservationKind::Correction,
                "[pull request] Never unwrap in library code"
            ),
            (
                ObservationKind::Correction,
                "[pull request] Keep the changelog in step"
            ),
            (
                ObservationKind::CheckFailed,
                "CI check \"test\" failed on abc"
            ),
            (
                ObservationKind::Correction,
                "[steer] Prefer small commits over one big one"
            ),
            (
                ObservationKind::ToolDenied,
                "Bash denied by rule Bash(rm *)"
            ),
            (
                ObservationKind::CheckFailed,
                &format!(
                    "check `cargo test` failed\n{}",
                    (1..=45)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            ),
            (ObservationKind::GoalMissing, "the changelog entry\na test"),
            (
                ObservationKind::JudgeVerdict,
                "not picked: no test for the fix"
            ),
            (ObservationKind::PlanRejected, "too broad"),
            (
                ObservationKind::Escalation,
                "I need the staging credentials"
            ),
            (
                ObservationKind::Escalation,
                "blocked: waiting on a credential"
            ),
            (
                ObservationKind::Outcome,
                "failed: the check failed after 2 turns, this turn $1.25"
            ),
            (
                ObservationKind::RepeatedCommand,
                "cargo test -p parser (2 times)"
            ),
        ]
    );
    // The author rule: a forge login that is no member is external, a
    // member's comment is a member's, a steer has no author.
    let external = &drafts[0];
    assert_eq!(
        external.author,
        Some(Author {
            name: "@drive-by".into(),
            association: Association::External,
        })
    );
    assert_eq!(external.paths, ["src/lib.rs"]);
    assert_eq!(external.seqs, [3, 4], "the feedback record and its prompt");
    assert_eq!(
        drafts[1].author.as_ref().map(|a| a.association),
        Some(Association::Member)
    );
    assert_eq!(drafts[3].author, None);
    assert_eq!(drafts[12].seqs, [16, 17]);
    // Turn 3's one prompt is a continuation: nothing.
    assert!(derive(&events, &context(3, &members, &shared)).is_empty());
    assert!(derive(&events, &context(4, &members, &shared)).is_empty());
}

#[test]
fn observations_carry_the_context_and_key_by_incarnation_and_tenant() {
    let members = Vec::new();
    let private = Visibility::Private {
        subject: "ana".into(),
    };
    let events = feed(vec![
        prompt("Fix"),
        prompt("Run cargo fmt before you finish"),
    ]);
    let ctx = context(2, &members, &private);
    let [draft] = derive(&events, &ctx).try_into().unwrap();
    let observation =
        draft
            .clone()
            .into_observation(&ctx, draft.text.clone(), vec![ScrubKind::Email]);
    assert_eq!(observation.tenant, "local");
    assert_eq!(observation.repo, "app");
    assert_eq!(
        observation.events,
        [EventRef {
            incarnation: 7,
            seq: 2
        }]
    );
    assert_eq!(observation.visibility.subject(), Some("ana"));
    assert_eq!(observation.task_kind, Some(TaskKind::Bugfix));
    assert_eq!(observation.model.as_deref(), Some("small"));
    assert_eq!(observation.scrubbed, [ScrubKind::Email]);
    assert!(!observation.external());
    let key = observation.key();
    assert_eq!(key.len(), 64);
    // The same turn settled twice: the same key. A new incarnation, another
    // tenant or another person: a new one.
    let mut again = observation.clone();
    again.at_ms += 5;
    again.scrubbed.clear();
    assert_eq!(again.key(), key);
    let mut recreated = observation.clone();
    recreated.incarnation = 8;
    assert_ne!(recreated.key(), key);
    let mut other_tenant = observation.clone();
    other_tenant.tenant = "acme".into();
    assert_ne!(other_tenant.key(), key);
    let mut shared = observation.clone();
    shared.visibility = Visibility::Shared;
    assert_ne!(shared.key(), key);
    // Serialized flat, the visibility beside the other fields.
    let json = serde_json::to_value(&observation).unwrap();
    assert_eq!(json["visibility"], "private");
    assert_eq!(json["subject"], "ana");
    assert_eq!(json["kind"], "correction");
    let back: NewObservation = serde_json::from_value(json).unwrap();
    assert_eq!(back, observation);
    // Long text is cut at the limit.
    let long = "x".repeat(TEXT_MAX + 50);
    let cut = draft.into_observation(&ctx, long, Vec::new());
    assert!(cut.text.chars().count() <= TEXT_MAX);
    assert!(cut.text.ends_with("..."));
}

#[test]
fn turn_events_split_the_feed_at_prompts() {
    let events = feed(vec![
        prompt("one"),
        at(Activity::Status(BranchStatus::Ready)),
        prompt("two"),
        prompt("three"),
    ]);
    assert_eq!(turn_events(&events, 1).unwrap().len(), 2);
    assert_eq!(turn_events(&events, 2).unwrap().len(), 1);
    assert_eq!(turn_events(&events, 3).unwrap().len(), 1);
    assert!(turn_events(&events, 4).is_none());
    assert!(turn_events(&events, 0).is_none());
}

/// A generated corpus: for each shape a value and a sentence that holds
/// it, with its base64 and URL-encoded forms for the exact values. None is
/// a real secret.
fn corpus() -> Vec<(String, String, ScrubKind)> {
    let mut rows = Vec::new();
    let filler = "Before you report the numbers, run the benches with the large fixture and \
                  compare both runs against main, naming the fixture each number came from; ";
    let mut push = |value: String, kind: ScrubKind| {
        rows.push((
            value.clone(),
            format!("{filler}the value was {value} there."),
            kind,
        ));
    };
    for n in 0..5u32 {
        push(
            format!("AKIA{:0>16}", format!("EXAMPLE{n}")),
            ScrubKind::Token,
        );
        push(
            format!("ghp_{}", "abcdefghij".repeat(3) + &n.to_string()),
            ScrubKind::Token,
        );
        push(
            format!("sk-{}", "k0".repeat(12) + &n.to_string()),
            ScrubKind::Token,
        );
        push(format!("xoxb-{n}234567890-abcdefghijkl"), ScrubKind::Token);
        push(
            format!("eyJhbGciOiJIUzI1NiJ9.eyJuIjoie{n}fQ.c2lnbmF0dXJl{n}"),
            ScrubKind::Jwt,
        );
        push(format!("password=hunter{n}pw"), ScrubKind::Assignment);
        push(format!("https://h/x?token=t{n}&page=2"), ScrubKind::Url);
        push(format!("someone{n}@elsewhere.example"), ScrubKind::Email);
        push(
            format!("Bearer {}", "ab12".repeat(5) + &n.to_string()),
            ScrubKind::Bearer,
        );
    }
    rows
}

#[test]
fn every_shape_of_the_corpus_is_scrubbed_and_counted() {
    let scrubber = Scrubber::new(&[], &[]);
    for (value, text, kind) in corpus() {
        let (out, kinds) = scrubber
            .scrub(&text)
            .unwrap_or_else(|| panic!("{text:?} was dropped"));
        let secret = value.split_whitespace().last().unwrap_or(&value);
        assert!(!out.contains(secret), "{text:?} -> {out:?} keeps the value");
        assert_eq!(kinds, [kind], "{text:?} -> {out:?}");
    }
}

#[test]
fn injected_values_go_in_every_encoding_and_a_quarter_scrubbed_text_is_dropped() {
    use base64::Engine as _;
    let values: Vec<String> = (0..4)
        .map(|n| format!("inj3cted-value-{n}-{}", "q".repeat(6)))
        .collect();
    let scrubber = Scrubber::new(&values, &[]);
    let filler = "The benches want the large fixture; the small one says nothing about real \
                  inputs, so compare both runs against main before you report any number and \
                  name the fixture each came from, every time, in the pull request. ";
    for value in &values {
        let b64 = base64::engine::general_purpose::STANDARD.encode(value);
        let url = percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
        let text = format!("{filler}{filler}plain {value}, b64 {b64}, url {url}.");
        let (out, kinds) = scrubber.scrub(&text).unwrap();
        for form in [value.as_str(), b64.as_str(), url.as_str()] {
            assert!(!out.contains(form), "{form} kept in {out}");
        }
        assert_eq!(
            kinds,
            [ScrubKind::Value, ScrubKind::Value, ScrubKind::Value]
        );
        // The same value said four times in a short text is most of it.
        assert!(scrubber
            .scrub(&format!("{value} {value} {value} {value} ok"))
            .is_none());
    }
}

fn observation(
    id: u64,
    kind: ObservationKind,
    text: &str,
    paths: &[&str],
    external: bool,
) -> Observation {
    Observation {
        id,
        body: NewObservation {
            tenant: "local".into(),
            repo: "app".into(),
            branch: format!("b{id}"),
            incarnation: 1,
            turn: 2,
            events: vec![EventRef {
                incarnation: 1,
                seq: id,
            }],
            visibility: Visibility::Shared,
            kind,
            text: text.into(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            task_kind: Some(TaskKind::Tests),
            harness: "fake".into(),
            model: None,
            author: external.then(|| Author {
                name: "drive-by".into(),
                association: Association::External,
            }),
            at_ms: 1,
            scrubbed: Vec::new(),
        },
    }
}

#[test]
fn clusters_group_normalized_text_and_merge_similar_groups() {
    let observations = vec![
        observation(
            1,
            ObservationKind::Correction,
            "[send] Run the benches with the 1 MB fixture",
            &["crates/parser/**"],
            false,
        ),
        observation(
            2,
            ObservationKind::Correction,
            "[steer] run the  Benches with the 2 MB fixture",
            &[],
            false,
        ),
        observation(
            3,
            ObservationKind::Correction,
            "[send] Run the benches with the large fixture first",
            &["crates/parser/**"],
            false,
        ),
        observation(
            4,
            ObservationKind::Correction,
            "[send] Keep the changelog in step with the code",
            &[],
            false,
        ),
        observation(
            5,
            ObservationKind::CheckFailed,
            "Run the benches with the 1 MB fixture",
            &[],
            false,
        ),
    ];
    let clusters: Vec<Cluster> = cluster(&observations);
    let ids: Vec<Vec<u64>> = clusters.iter().map(|c| c.ids.clone()).collect();
    assert_eq!(ids, [vec![1, 2, 3], vec![4], vec![5]], "{ids:?}");
    assert_eq!(clusters[0].kind, ObservationKind::Correction);
    assert_eq!(clusters[0].paths, ["crates/parser/**"]);
    assert_eq!(clusters[0].task_kinds, [TaskKind::Tests]);
    assert_eq!(
        clusters[2].kind,
        ObservationKind::CheckFailed,
        "kinds never mix"
    );
}

#[test]
fn the_pass_proposes_clusters_of_two_matches_evidence_and_skips_external_authors() {
    let scope = Scope::Repo {
        tenant: "local".into(),
        repo: "app".into(),
    };
    let existing = MemoryRecord {
        id: 50,
        scope: scope.clone(),
        kind: MemoryKind::Rule,
        title: "Keep the changelog in step with the code".into(),
        body: "Every change ships its changelog line.".into(),
        paths: Vec::new(),
        task_kinds: Vec::new(),
        status: MemoryStatus::Rejected,
        provenance: vec![9],
        seen: 1,
        first_seen_ms: 1,
        last_seen_ms: 1,
        decided_by: Some("ann".into()),
        decided_ms: Some(2),
        note: Some("wrong".into()),
        published: None,
    };
    let observations = vec![
        observation(
            1,
            ObservationKind::Correction,
            "[send] Run the benches with the 1 MB fixture",
            &["crates/parser/**"],
            false,
        ),
        observation(
            2,
            ObservationKind::Correction,
            "[steer] Run the benches with the 2 MB fixture",
            &["crates/parser/**"],
            false,
        ),
        observation(
            3,
            ObservationKind::Correction,
            "[send] Keep the changelog in step with the code",
            &[],
            false,
        ),
        observation(
            4,
            ObservationKind::Correction,
            "[pull request] Rename this helper",
            &[],
            true,
        ),
        observation(
            5,
            ObservationKind::Correction,
            "[pull request] Rename this helper",
            &[],
            true,
        ),
        observation(
            6,
            ObservationKind::Correction,
            "[send] Format before you finish",
            &[],
            false,
        ),
        observation(
            7,
            ObservationKind::Correction,
            "[send] Format before you finish",
            &[],
            false,
        ),
        observation(
            8,
            ObservationKind::ToolDenied,
            "Bash denied by rule x",
            &[],
            false,
        ),
        observation(
            9,
            ObservationKind::ToolDenied,
            "Bash denied by rule x",
            &[],
            false,
        ),
        observation(
            10,
            ObservationKind::CheckFailed,
            "check `cargo test` failed\nassertion failed",
            &[],
            false,
        ),
    ];
    let result = pass(
        &scope,
        &observations,
        std::slice::from_ref(&existing),
        false,
        20,
        3,
        500,
    );
    // The rejected memory gained evidence and nothing else.
    assert_eq!(result.evidence, [(50, vec![3])]);
    // One memory candidate (paths), one knowledge candidate (short,
    // repository-wide), and neither the external pair, the denials nor the
    // lone check.
    assert_eq!(result.proposals.len(), 1, "{:?}", result.proposals);
    let proposal = &result.proposals[0];
    assert_eq!(proposal.title, "Run the benches with the 1 MB fixture");
    assert_eq!(proposal.kind, MemoryKind::Rule);
    assert_eq!(proposal.paths, ["crates/parser/**"]);
    assert_eq!(proposal.provenance, [1, 2]);
    assert_eq!(proposal.note.as_deref(), Some("deterministic"));
    assert!(proposal
        .body
        .contains("- b1 turn 2: [send] Run the benches with the 1 MB fixture"));
    assert!(proposal
        .body
        .contains("- b2 turn 2: [steer] Run the benches with the 2 MB fixture"));
    assert_eq!(result.knowledge.len(), 1);
    assert_eq!(result.knowledge[0].text, "Format before you finish");
    assert!(matches!(
        &result.knowledge[0].source,
        crate::KnowledgeSource::Consolidation { run: 3, observations } if observations == &vec![6, 7]
    ));
    // With external authors allowed, their pair is a candidate too.
    let open = pass(&scope, &observations, &[existing], true, 20, 3, 500);
    assert_eq!(open.knowledge.len(), 2);
    assert!(open
        .knowledge
        .iter()
        .any(|k| k.text == "Rename this helper"));
    // The cap.
    let capped = pass(&scope, &observations, &[], true, 1, 3, 500);
    assert_eq!(capped.proposals.len() + capped.knowledge.len(), 1);
    assert_eq!(
        title_of(&observations[0].text),
        "Run the benches with the 1 MB fixture"
    );
}

#[test]
fn a_short_correction_is_knowledge_only_for_the_local_team_scope() {
    let observations = vec![
        observation(
            1,
            ObservationKind::Correction,
            "[send] Format before you finish",
            &[],
            false,
        ),
        observation(
            2,
            ObservationKind::Correction,
            "[send] Format before you finish",
            &[],
            false,
        ),
    ];
    let run = |scope: &Scope| pass(scope, &observations, &[], false, 20, 1, 5);
    let local = run(&Scope::Repo {
        tenant: "local".into(),
        repo: "app".into(),
    });
    assert_eq!((local.knowledge.len(), local.proposals.len()), (1, 0));
    // A served tenant's repository may be another tenant's too: the
    // correction stays a row of the tenant's own scope.
    let served = run(&Scope::Repo {
        tenant: "acme".into(),
        repo: "app".into(),
    });
    assert!(served.knowledge.is_empty(), "{:?}", served.knowledge);
    assert_eq!(served.proposals.len(), 1);
    assert_eq!(served.proposals[0].title, "Format before you finish");
    // A person's private corrections never become team knowledge.
    for repo in [None, Some("app".to_owned())] {
        let person = run(&Scope::Person {
            tenant: "local".into(),
            subject: "ana".into(),
            repo,
        });
        assert!(person.knowledge.is_empty(), "{:?}", person.knowledge);
        assert_eq!(person.proposals.len(), 1);
    }
}

#[test]
fn a_cut_body_counts_observations_not_lines() {
    // Failed checks near the text limit, each of forty lines: the body is
    // cut after the first and counts the others, not an underflowed number.
    let line = "x".repeat(97);
    let text = format!("check `cargo test` failed\n{}", vec![line; 40].join("\n"));
    let observations = vec![
        observation(1, ObservationKind::CheckFailed, &text, &[], false),
        observation(2, ObservationKind::CheckFailed, &text, &[], false),
        observation(3, ObservationKind::CheckFailed, &text, &[], false),
    ];
    let scope = Scope::Repo {
        tenant: "local".into(),
        repo: "app".into(),
    };
    let result = pass(&scope, &observations, &[], false, 20, 1, 5);
    assert_eq!(result.proposals.len(), 1);
    let body = &result.proposals[0].body;
    assert!(body.ends_with("- ...and 2 more"), "{}", &body[body.len() - 60..]);
}

#[test]
fn a_failed_checks_output_is_scrubbed_before_it_is_cut_to_its_last_lines() {
    // A multi-line secret whose first lines fall before the cut: the lines
    // that remain in the tail are still removed, and the tail is the last
    // forty.
    let value = "PRIVATE KEY BEGIN\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nKgwggSkAgEAAoIBAQC7VJTUt9Us8cKj\nZm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFi\nPRIVATE KEY END";
    let ordinary = |n: u32| format!("line {n} of ordinary compiler output, long enough to matter");
    let mut output: Vec<String> = (1..=3).map(ordinary).collect();
    output.extend(value.lines().map(str::to_owned));
    output.extend((4..=60).map(ordinary));
    // The value's later lines come again inside the last forty.
    let inside = output.len() - 38;
    output.splice(inside..inside, value.lines().skip(2).map(str::to_owned));
    let events = feed(vec![
        prompt("task"),
        at(Activity::PullRequest(Box::new(
            PullRequestActivity::Checked(CheckRun {
                commit: "abc".into(),
                argv: vec!["cargo".into(), "test".into()],
                passed: false,
                timed_out: false,
                output_tail: output.join("\n"),
            }),
        ))),
    ]);
    let members = Vec::new();
    let visibility = Visibility::Shared;
    let ctx = context(1, &members, &visibility);
    let drafts = derive(&events, &ctx);
    assert_eq!(drafts.len(), 1);
    let scrubber = Scrubber::new(&[value.to_owned()], &[]);
    let draft = drafts.into_iter().next().unwrap();
    let (text, _) = scrubber.scrub(&draft.text).unwrap();
    let stored = draft.into_observation(&ctx, text, Vec::new()).text;
    assert!(stored.starts_with("check `cargo test` failed\n"), "{stored}");
    assert_eq!(stored.lines().count(), 41, "{stored}");
    for part in value.lines().filter(|l| l.len() > 8) {
        assert!(!stored.contains(part), "{part} kept in {stored}");
    }
    assert!(stored.ends_with("line 60 of ordinary compiler output, long enough to matter"));
}
