//! Memory rows over real HTTP, against the fake ACP agent
//! (`docs/memory.md`, sections 3 to 5): corrections sent through the
//! server become observations; `POST .../memory/consolidate` proposes rows
//! a person reads, approves (with `admin` or an approver's name) or
//! rejects; a person's row is hidden from another principal; the
//! dispatcher consolidates on the configured schedule.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard::memory::Visibility;
use branchyard::{MemorySettings, MemoryStatus, Yard};
use branchyard_client::api::{OperationState, PolicySpec, SendRequest};
use branchyard_client::memory_api::SCOPE_MINE;
use branchyard_client::{new_key, Client};
use branchyard_server::config::{Principal, Token};
use branchyard_server::triggers::Clock;
use branchyard_testkit::wait;
use common::{run, task, Fixture, Server};

const REVIEW: &str = "File: notes.txt\nLine: 1\nUser comment: \"Always end notes with a summary \
                      line, and date it\"";

const BOB_TOKEN: &str = "bob-token-0123456789abcd";

/// Two branches, each sent the same correction after its task.
fn corrected_twice(client: &Client) {
    let repo = client.repo("app");
    for name in ["one", "two"] {
        let op = run(client, &task("Add notes WRITE notes.txt=1", name));
        assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
        let sent = repo
            .send(
                name,
                &SendRequest {
                    prompt: REVIEW.into(),
                    policy: PolicySpec::allow_all(),
                    ..SendRequest::default()
                },
                &new_key(),
            )
            .unwrap();
        let op = common::await_operation(client, &sent.id);
        assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    }
}

#[test]
fn consolidation_proposes_rows_that_approvers_decide_over_http() {
    let f = Fixture::new();
    let mut config = f.config();
    // Bob holds `run` and `read` but not `admin`, and is no approver.
    config.tokens.push(Token {
        name: "bob".into(),
        secret: BOB_TOKEN.into(),
    });
    let mut bob = Principal::default_for("bob");
    bob.scopes.remove("admin");
    config.principals.insert("bob".into(), bob);
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    corrected_twice(&client);

    let consolidated = repo.consolidate_memory(None).unwrap();
    assert!(!consolidated.lost_race);
    assert_eq!(consolidated.run.counts.observations, 2);
    assert_eq!(consolidated.proposed.len(), 1, "{consolidated:?}");
    let id = consolidated.proposed[0].id;
    assert_eq!(consolidated.run.by, "tester");
    assert_eq!(consolidated.run.trigger, "asked");
    let proposed = repo.memories(None, Some(MemoryStatus::Proposed)).unwrap();
    assert_eq!(proposed.iter().map(|m| m.id).collect::<Vec<_>>(), [id]);
    assert_eq!(proposed[0].scope.tenant(), "default");
    assert_eq!(repo.memory(id).unwrap(), proposed[0]);
    let evidence = repo.memory_evidence(id).unwrap();
    assert_eq!(evidence.observations.len(), 2);
    assert_eq!(evidence.observations[0].branch, "one");
    assert!(evidence.observations.iter().all(|o| o.author.is_none()));
    let runs = repo.memory_runs(None).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("proposed"));

    // Without admin or a name in the approvers, the team's memory is not
    // theirs to decide: 403 denied.
    let bob = Client::new(&server.url(), BOB_TOKEN).unwrap();
    let refused = bob.repo("app").approve_memory(id).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");
    let refused = bob.repo("app").reject_memory(id, Some("no")).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");
    // Reading is `read`'s.
    assert_eq!(bob.repo("app").memory(id).unwrap().id, id);

    // The admin approves: the bundle is written into the served checkout.
    let approved = repo.approve_memory(id).unwrap();
    assert_eq!(approved.status, MemoryStatus::Approved);
    assert_eq!(approved.decided_by.as_deref(), Some("tester"));
    let published = approved.published.unwrap();
    assert_eq!(published.skill, "memory-notes-txt");
    assert!(f
        .root
        .join(".agents/skills/memory-notes-txt")
        .join(&published.path)
        .is_file());
    let edited = repo
        .edit_memory(
            id,
            &branchyard_client::memory_api::MemoryEditRequest {
                title: Some("End every note with a dated summary line".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(edited.title, "End every note with a dated summary line");
    let forgotten = repo.forget_memory(id).unwrap();
    assert_eq!(forgotten.id, id);
    assert!(!f.root.join(".agents/skills/memory-notes-txt").exists());
    let gone = repo.memory(id).unwrap_err();
    assert_eq!(gone.code(), Some("unknown_memory"), "{gone}");
    // A bad scope or id is refused as such.
    let bad = bob.repo("app").memories(Some("ours"), None).unwrap_err();
    assert_eq!(bad.code(), Some("invalid_request"), "{bad}");
    server.stop();
}

#[test]
fn a_persons_row_is_theirs_alone_and_an_admin_sees_only_that_it_exists() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens.push(Token {
        name: "bob".into(),
        secret: BOB_TOKEN.into(),
    });
    config
        .principals
        .insert("bob".into(), Principal::default_for("bob"));
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    corrected_twice(&client);
    // The server has no private sessions yet: a second handle on the same
    // store derives the turns' observations as tester's private ones, as a
    // private session would.
    let yard = Yard::open(&f.root).unwrap();
    yard.use_memory(MemorySettings {
        visibility: Visibility::Private {
            subject: "tester".into(),
        },
        ..MemorySettings::default()
    });
    for name in ["one", "two"] {
        assert_eq!(yard.observe_turn(name, 2, &[]).unwrap().len(), 1);
    }
    let mine = repo.consolidate_memory(Some(SCOPE_MINE)).unwrap();
    assert_eq!(mine.proposed.len(), 1, "{mine:?}");
    let id = mine.proposed[0].id;
    assert_eq!(mine.proposed[0].scope.subject(), Some("tester"));
    assert_eq!(repo.memories(Some(SCOPE_MINE), None).unwrap().len(), 1);
    assert!(repo.memories(None, None).unwrap().is_empty());
    // Bob is an admin: he sees that it exists, whose it is and its size.
    let bob = Client::new(&server.url(), BOB_TOKEN).unwrap();
    let seen = bob.repo("app").memory(id).unwrap();
    assert_eq!(seen.id, id);
    assert_eq!(seen.scope.subject(), Some("tester"));
    assert!(seen.title.starts_with("[private: "), "{}", seen.title);
    assert!(seen.body.starts_with("[private: "), "{}", seen.body);
    assert!(!seen.body.contains("summary line"));
    let evidence = bob.repo("app").memory_evidence(id).unwrap();
    assert!(evidence.observations.is_empty(), "no evidence text either");
    assert!(bob
        .repo("app")
        .memories(Some(SCOPE_MINE), None)
        .unwrap()
        .is_empty());
    // Not his to decide, admin or not.
    let refused = bob.repo("app").reject_memory(id, None).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");
    // Nor to approve on a server, even for its subject: the person's
    // folder is not here until the registry publishes it.
    let refused = repo.approve_memory(id).unwrap_err();
    assert_eq!(refused.code(), Some("unsupported"), "{refused}");
    // The subject promotes it: a new proposed row for the team.
    let promoted = repo.promote_memory(id).unwrap();
    assert_eq!(promoted.scope.subject(), None);
    assert_eq!(promoted.status, MemoryStatus::Proposed);
    assert_eq!(repo.memory(id).unwrap().status, MemoryStatus::Proposed);
    server.stop();
}

#[test]
fn the_dispatcher_consolidates_on_the_configured_schedule() {
    let f = Fixture::new();
    let mut config = f.config();
    let time = Arc::new(AtomicU64::new(1_762_500_000_000));
    config.triggers.clock = Clock::manual(time.clone());
    config.triggers.tick = Duration::from_millis(50);
    config.memory.consolidate = Some("* * * * *".into());
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    corrected_twice(&client);
    assert!(repo.memory_runs(None).unwrap().is_empty());
    // The next whole minute comes: the dispatcher runs the scope that has
    // unread observations, by itself.
    time.fetch_add(2 * 60 * 1_000, Ordering::SeqCst);
    let runs = wait::until("the scheduled consolidation to run", || {
        let runs = repo.memory_runs(None).unwrap();
        match runs
            .iter()
            .any(|r| r.trigger == "schedule" && r.finished_ms.is_some())
        {
            true => Ok(runs),
            false => Err(format!("{runs:?}")),
        }
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].by, "the dispatcher");
    assert_eq!(runs[0].outcome.as_deref(), Some("proposed"));
    assert_eq!(runs[0].counts.proposed, 1);
    assert_eq!(
        repo.memories(None, Some(MemoryStatus::Proposed))
            .unwrap()
            .len(),
        1
    );
    // A minute with nothing new runs no scope; the minute after a third
    // correction runs one, so the schedule has fired twice in all and the
    // store holds two runs, not three.
    time.fetch_add(60 * 1_000, Ordering::SeqCst);
    let op = run(&client, &task("Add notes WRITE notes.txt=1", "three"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let sent = repo
        .send(
            "three",
            &SendRequest {
                prompt: REVIEW.into(),
                policy: PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(
        common::await_operation(&client, &sent.id).state,
        OperationState::Succeeded
    );
    time.fetch_add(60 * 1_000, Ordering::SeqCst);
    let runs = wait::until("the second scheduled consolidation to run", || {
        let runs = repo.memory_runs(None).unwrap();
        match runs.iter().filter(|r| r.finished_ms.is_some()).count() >= 2 {
            true => Ok(runs),
            false => Err(format!("{runs:?}")),
        }
    });
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].counts.observations, 1);
    assert_eq!(runs[0].counts.merged, 1, "the third correction is evidence");
    server.stop();
}

const CAROL_TOKEN: &str = "carol-token-0123456789abc";

#[test]
fn another_tenants_row_is_unknown_a_non_admin_cannot_read_a_persons_row_and_listings_page() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens.push(Token {
        name: "carol".into(),
        secret: CAROL_TOKEN.into(),
    });
    let mut carol = Principal::default_for("carol");
    carol.scopes.remove("admin");
    config.principals.insert("carol".into(), carol);
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    corrected_twice(&client);
    let yard = Yard::open(&f.root).unwrap();
    yard.use_memory(MemorySettings {
        visibility: Visibility::Private {
            subject: "tester".into(),
        },
        ..MemorySettings::default()
    });
    for name in ["one", "two"] {
        assert_eq!(yard.observe_turn(name, 2, &[]).unwrap().len(), 1);
    }
    let mine = repo.consolidate_memory(Some(SCOPE_MINE)).unwrap();
    let person = mine.proposed[0].id;

    // A caller without admin who is not the subject is refused, row and
    // evidence alike.
    let carol = Client::new(&server.url(), CAROL_TOKEN).unwrap();
    let refused = carol.repo("app").memory(person).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");
    let refused = carol.repo("app").memory_evidence(person).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");
    let refused = carol.repo("app").promote_memory(person).unwrap_err();
    assert_eq!(refused.code(), Some("denied"), "{refused}");

    // A row of another tenant does not exist for this one, whatever is
    // asked of it.
    let other = yard
        .promote_memory(person, "tester", yard.memory_scope("other"))
        .unwrap();
    let unknown = |error: branchyard_client::Error| {
        assert_eq!(error.code(), Some("unknown_memory"), "{error}");
    };
    unknown(repo.memory(other.id).unwrap_err());
    unknown(repo.memory_evidence(other.id).unwrap_err());
    unknown(repo.approve_memory(other.id).unwrap_err());
    unknown(repo.reject_memory(other.id, None).unwrap_err());
    unknown(repo.forget_memory(other.id).unwrap_err());
    unknown(repo.promote_memory(other.id).unwrap_err());
    unknown(
        repo.edit_memory(other.id, &Default::default())
            .unwrap_err(),
    );
    assert!(repo.memories(None, None).unwrap().is_empty());

    // Listings are pages with a cursor.
    for _ in 0..3 {
        repo.promote_memory(person).unwrap();
    }
    let first = repo.memories_page(None, None, Some(2), None).unwrap();
    assert_eq!(first.memories.len(), 2);
    let cursor = first.next.expect("another page follows");
    assert_eq!(cursor, first.memories[1].id);
    let second = repo
        .memories_page(None, None, Some(2), Some(cursor))
        .unwrap();
    assert_eq!(second.memories.len(), 1);
    assert_eq!(second.next, None);
    assert!(second.memories[0].id > cursor);
    let bad = repo.memories_page(None, None, Some(0), None).unwrap_err();
    assert_eq!(bad.code(), Some("invalid_request"), "{bad}");
    server.stop();
}
