//! Telemetry is off unless asked for: nothing is exported, no thread is
//! started, no span is bridged, and an incoming `traceparent` still passes
//! through.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report

use branchyard_telemetry::propagation::{adopt, pass_on, span_traceparent};
use branchyard_telemetry::{init_with, Config, Meters, Role, Settings, TRACE_TARGET};

/// Threads of this process, from the kernel.
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count)
}

#[test]
fn nothing_is_exported_started_or_bridged_by_default() {
    let before = threads();
    // The environment of a developer's shell: no endpoint, nothing set.
    let settings = Settings::from_vars(|_| None);
    let guard = init_with(
        Config {
            role: Role::Cli,
            service_name: "by".into(),
            service_version: "0".into(),
            layers: Vec::new(),
        },
        settings,
    );
    assert!(!guard.exporting_traces());
    assert!(!guard.exporting_metrics());
    assert!(!branchyard_telemetry::spans_enabled());

    // A span is made and entered, and has no OpenTelemetry context, so an
    // incoming traceparent is passed on as it came.
    let incoming = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let span = tracing::info_span!(target: TRACE_TARGET, "admission");
    assert!(
        !adopt(&span, incoming),
        "with no OpenTelemetry layer there is no span to give a parent"
    );
    let _in = span.enter();
    assert_eq!(span_traceparent(&span), None);
    assert_eq!(pass_on(&span, Some(incoming)).as_deref(), Some(incoming));
    assert_eq!(pass_on(&span, None), None);

    // The process's meters exist (a server answers /metrics from them) but
    // push nothing.
    let meters = Meters::process().unwrap();
    assert!(meters.reader().render(&[]).is_ok());

    // No exporter thread was started.
    assert_eq!(threads(), before, "telemetry started a thread while off");
    drop(_in);
    drop(guard);
    assert_eq!(threads(), before);
}
