//! With telemetry off no span of the trace is built: a turn with a tool
//! call, a permission wait, a delegation, a message and the store's queries
//! run, and a subscriber that counts every span it is offered sees none of
//! the trace's. Turn the switch on and the same work is offered spans, so
//! the zero is the switch's doing and not the subscriber's blindness.

#![allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use branchyard::{Envelope, Policy, Spawn, TaskOptions};
use common::{fake_agent, Fixture};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

static OFFERED: AtomicU64 = AtomicU64::new(0);
static IDS: AtomicU64 = AtomicU64::new(1);

/// Counts the spans of the trace's targets that are built.
struct Counting;

impl Subscriber for Counting {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        if attributes
            .metadata()
            .target()
            .starts_with(branchyard_support::trace::TARGET)
        {
            OFFERED.fetch_add(1, Ordering::SeqCst);
        }
        Id::from_u64(IDS.fetch_add(1, Ordering::SeqCst))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn work(f: &Fixture, name: &str) {
    let options = TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    };
    let lead = f
        .yard
        .task("SH echo hi")
        .options(options.clone())
        .name(name)
        .run()
        .unwrap();
    let delegate = lead.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "say kid".into(),
            name: Some(format!("{name}-kid")),
            ..Spawn::default()
        })
        .unwrap();
    lead.wait_subtree().unwrap();
}

#[test]
fn nothing_is_built_while_telemetry_is_off() {
    tracing::subscriber::set_global_default(Counting).unwrap();
    assert!(!branchyard_support::trace::enabled());
    let f = Fixture::new();
    work(&f, "off");
    assert_eq!(
        OFFERED.load(Ordering::SeqCst),
        0,
        "spans of the trace were built with telemetry off"
    );

    // The control: the same work with the switch on is offered spans.
    branchyard_support::trace::set_enabled(true);
    work(&f, "on");
    branchyard_support::trace::set_enabled(false);
    assert!(
        OFFERED.load(Ordering::SeqCst) > 0,
        "the counting subscriber saw no span even with the switch on"
    );
}
