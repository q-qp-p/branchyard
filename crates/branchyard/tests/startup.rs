//! A turn records how long it took to start, phase by phase
//! (`Activity::Startup`), and the report over those records, end to end
//! against the fake agent: a local harness, and
//! `branchyard_sandbox::fake::FakeProvider` standing in for a sandbox
//! provider, cold, resumed from a kept sandbox, and claimed from a pool.
//! Hermetic; nothing here is a measurement of any real provider.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::Arc;

use branchyard::startup::{self, Filter, Phase, Phases, Startup, StartupPath};
use branchyard::{
    Activity, Branch, Policy, Provider, SandboxKeep, SandboxOptions, TaskOptions, Yard,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_sandbox::Launch;
use common::Fixture;

fn startups(branch: &Branch) -> Vec<Startup> {
    branch
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Startup(startup) => Some(*startup),
            _ => None,
        })
        .collect()
}

/// A kept-sandbox task on a fake provider for the fixture's yard.
fn sandboxed(f: &Fixture) -> (Arc<FakeProvider>, TaskOptions) {
    let fake = Arc::new(FakeProvider::live(
        Box::new(LocalProvider::new()),
        f.dir.join("fake-provider"),
    ));
    f.yard.use_sandbox_provider(fake.clone());
    let options = TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep: SandboxKeep::Pause,
            ..SandboxOptions::default()
        })),
        policy: Policy::allow_all(),
        ..f.options()
    };
    (fake, options)
}

/// Every offset reached, in phase order, never goes backwards.
fn in_order(phases: &Phases) -> bool {
    let reached: Vec<u64> = Phase::ALL.iter().filter_map(|p| phases.get(*p)).collect();
    reached.windows(2).all(|pair| pair[0] <= pair[1])
}

#[test]
fn a_local_turn_records_the_phases_it_reached_and_none_it_did_not() {
    let f = Fixture::new();
    let branch = f.task("SH true").name("local").run().unwrap();
    let recorded = startups(&branch);
    assert_eq!(recorded.len(), 1, "{:?}", branch.events());
    let startup = &recorded[0];
    assert_eq!(startup.provider, "local");
    assert_eq!(startup.path, StartupPath::Local);
    let phases = &startup.phases;
    for reached in [
        Phase::Queued,
        Phase::Placement,
        Phase::HarnessStarted,
        Phase::HandshakeDone,
        Phase::FirstEvent,
    ] {
        assert!(phases.get(reached).is_some(), "{reached}: {phases:?}");
    }
    // No sandbox, so none of its phases occurred.
    for absent in [
        Phase::ImageReady,
        Phase::SandboxReady,
        Phase::SourceAttached,
    ] {
        assert_eq!(phases.get(absent), None, "{absent}: {phases:?}");
    }
    assert!(in_order(phases), "{phases:?}");
    // It is recorded after the prompt, with the harness's first event.
    let events = branch.events().unwrap();
    let prompt = events
        .iter()
        .position(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap();
    let at = events
        .iter()
        .position(|e| matches!(e.activity, Activity::Startup(_)))
        .unwrap();
    assert!(prompt < at, "{events:?}");
}

#[test]
fn the_queue_before_a_turn_is_its_first_phase() {
    let f = Fixture::new();
    let waited = 5_000;
    let options = TaskOptions {
        admitted_ms: Some(branchyard_support::time::now_ms() - waited),
        ..f.options()
    };
    let branch = f
        .yard
        .task("SH true")
        .options(options)
        .name("queued")
        .run()
        .unwrap();
    let startup = &startups(&branch)[0];
    let queued = startup.phases.queued.unwrap();
    assert!(
        (waited..waited + 60_000).contains(&queued),
        "admitted {waited} ms before the turn began: {queued}"
    );
    // Every later phase is counted from the same admission.
    assert!(startup.phases.first_event.unwrap() >= queued);
}

#[test]
fn a_sandboxed_turn_names_its_provider_and_its_path_and_reaches_the_sandbox_phases() {
    let f = Fixture::new();
    let (_fake, options) = sandboxed(&f);
    let branch = f
        .yard
        .task("SH true")
        .options(options.clone())
        .name("boxed")
        .run()
        .unwrap();
    let first = &startups(&branch)[0];
    assert_eq!(first.provider, "microsandbox");
    assert_eq!(first.path, StartupPath::Cold);
    for reached in [
        Phase::Queued,
        Phase::Placement,
        Phase::SandboxReady,
        Phase::SourceAttached,
        Phase::HarnessStarted,
        Phase::HandshakeDone,
        Phase::FirstEvent,
    ] {
        assert!(first.phases.get(reached).is_some(), "{reached}: {first:?}");
    }
    // The provider measured no image, so none is claimed.
    assert_eq!(first.phases.image_ready, None);
    assert!(in_order(&first.phases), "{first:?}");

    // The kept sandbox, resumed on the next turn.
    let branch = branch.send("SH true", options).unwrap();
    let recorded = startups(&branch);
    assert_eq!(recorded.len(), 2, "{:?}", branch.events());
    assert_eq!(recorded[1].path, StartupPath::Resumed);
}

#[test]
fn a_provider_that_claims_from_a_pool_and_times_its_image_says_so() {
    let f = Fixture::new();
    let (fake, options) = sandboxed(&f);
    fake.set_launch(Some(Launch {
        pooled: true,
        image_cached: Some(true),
        image_ready_ms: Some(0),
    }));
    let branch = f
        .yard
        .task("SH true")
        .options(options.clone())
        .name("pooled")
        .run()
        .unwrap();
    let startup = &startups(&branch)[0];
    assert_eq!(startup.path, StartupPath::Pool, "{startup:?}");
    assert!(startup.phases.image_ready.is_some(), "{startup:?}");
    assert!(in_order(&startup.phases), "{startup:?}");

    // Cached but not pooled: a fresh sandbox from an image the host had.
    fake.set_launch(Some(Launch {
        pooled: false,
        image_cached: Some(true),
        image_ready_ms: None,
    }));
    let cached = f
        .yard
        .task("SH true")
        .options(options)
        .name("cached")
        .run()
        .unwrap();
    assert_eq!(startups(&cached)[0].path, StartupPath::CachedImage);
}

fn feed(yard: &Yard) -> Vec<branchyard::FeedEvent> {
    let mut all = Vec::new();
    let mut cursor = 0;
    loop {
        let page = yard.events_since(cursor, 1000).unwrap();
        if page.events.is_empty() {
            return all;
        }
        cursor = page.next_cursor;
        all.extend(page.events);
    }
}

#[test]
fn the_report_reads_the_recorded_turns_of_the_store() {
    let f = Fixture::new();
    f.task("SH true").name("one").run().unwrap();
    f.task("SH true").name("two").run().unwrap();
    let (_fake, options) = sandboxed(&f);
    f.yard
        .task("SH true")
        .options(options)
        .name("three")
        .run()
        .unwrap();

    let events = feed(&f.yard);
    let all = startup::report(&events, &Filter::default());
    assert_eq!(all.startups, 3);
    let row = |provider: &str, phase: &str| {
        all.rows
            .iter()
            .find(|r| r.provider == provider && r.phase == phase)
            .unwrap_or_else(|| panic!("{provider} {phase}: {all:?}"))
    };
    assert_eq!(row("local", "total").count, 2);
    assert_eq!(row("microsandbox", "total").count, 1);
    assert_eq!(row("microsandbox", "sandbox_ready").count, 1);
    assert!(all
        .rows
        .iter()
        .all(|r| !(r.provider == "local" && r.phase == "sandbox_ready")));

    let only = startup::report(
        &events,
        &Filter {
            provider: Some("microsandbox".into()),
            since_ms: None,
        },
    );
    assert_eq!(only.startups, 1);
    let future = startup::report(
        &events,
        &Filter {
            provider: None,
            since_ms: Some(branchyard_support::time::now_ms() + 60_000),
        },
    );
    assert_eq!(future.startups, 0);
    assert_eq!(future.render(), "no recorded startups\n");
}
