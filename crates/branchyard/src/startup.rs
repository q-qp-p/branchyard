//! How long a turn took to start, phase by phase, and a report over the
//! turns that were recorded.
//!
//! A turn records one [`Startup`] ([`crate::Activity::Startup`]) when its
//! harness has said its first thing, or when the turn ended without that.
//! Each [`Phase`] is a millisecond offset from the operation's admission,
//! read from `branchyard_support::time`, and is absent when the phase did
//! not occur: a local harness has no sandbox, a provider that reports no
//! image state has no `image_ready`. The offsets are cumulative; the time
//! one phase took is its offset less the previous phase that occurred
//! ([`Phases::durations`]), which is what the metrics and [`report`] count.
//! See `docs/providers.md#measuring-launch-time`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Activity, FeedEvent, SandboxOrigin};

/// The stages of starting a turn, in the order they happen.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    strum::Display,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Phase {
    /// The turn left the queue and began to run.
    Queued,
    /// The turn's sandbox placement was decided and the provider asked.
    Placement,
    /// The sandbox's image was ready to run, when the provider measured it.
    ImageReady,
    /// The sandbox was running, created, resumed, branched or claimed.
    SandboxReady,
    /// The worktree was in the sandbox (copied in, or mounted).
    SourceAttached,
    /// The harness process was started.
    HarnessStarted,
    /// The harness was open and ready for its prompt.
    HandshakeDone,
    /// The harness's first event after the prompt.
    FirstEvent,
}

impl Phase {
    /// Every phase, in order.
    pub const ALL: [Phase; 8] = [
        Phase::Queued,
        Phase::Placement,
        Phase::ImageReady,
        Phase::SandboxReady,
        Phase::SourceAttached,
        Phase::HarnessStarted,
        Phase::HandshakeDone,
        Phase::FirstEvent,
    ];
}

/// Which way a turn's sandbox was made ready. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    strum::Display,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StartupPath {
    /// Created for the turn, its image pulled on the way (or the provider
    /// cannot say it was not).
    Cold,
    /// Created for the turn from an image the host already had.
    CachedImage,
    /// Branched from a provider snapshot or a prepared environment.
    Snapshot,
    /// The branch's own kept sandbox, resumed.
    Resumed,
    /// Claimed from a pool of sandboxes booted ahead of time.
    Pool,
    /// No sandbox: the harness ran on this host.
    Local,
}

/// Millisecond offsets from admission at which each [`Phase`] was reached;
/// a phase that did not occur is absent.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phases {
    /// See [`Phase::Queued`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<u64>,
    /// See [`Phase::Placement`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<u64>,
    /// See [`Phase::ImageReady`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ready: Option<u64>,
    /// See [`Phase::SandboxReady`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_ready: Option<u64>,
    /// See [`Phase::SourceAttached`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_attached: Option<u64>,
    /// See [`Phase::HarnessStarted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_started: Option<u64>,
    /// See [`Phase::HandshakeDone`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_done: Option<u64>,
    /// See [`Phase::FirstEvent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event: Option<u64>,
}

impl Phases {
    /// The offset of `phase`, when it occurred.
    pub fn get(&self, phase: Phase) -> Option<u64> {
        match phase {
            Phase::Queued => self.queued,
            Phase::Placement => self.placement,
            Phase::ImageReady => self.image_ready,
            Phase::SandboxReady => self.sandbox_ready,
            Phase::SourceAttached => self.source_attached,
            Phase::HarnessStarted => self.harness_started,
            Phase::HandshakeDone => self.handshake_done,
            Phase::FirstEvent => self.first_event,
        }
    }

    fn slot(&mut self, phase: Phase) -> &mut Option<u64> {
        match phase {
            Phase::Queued => &mut self.queued,
            Phase::Placement => &mut self.placement,
            Phase::ImageReady => &mut self.image_ready,
            Phase::SandboxReady => &mut self.sandbox_ready,
            Phase::SourceAttached => &mut self.source_attached,
            Phase::HarnessStarted => &mut self.harness_started,
            Phase::HandshakeDone => &mut self.handshake_done,
            Phase::FirstEvent => &mut self.first_event,
        }
    }

    /// The last offset reached: how long the turn had taken to start, as
    /// far as it got.
    pub fn total(&self) -> Option<u64> {
        Phase::ALL.iter().filter_map(|p| self.get(*p)).max()
    }

    /// Each phase that occurred with the time it took: its offset less the
    /// latest earlier offset (admission is zero). An offset that would go
    /// backwards, as a provider's own figure can against this host's
    /// clock, counts as zero.
    pub fn durations(&self) -> Vec<(Phase, u64)> {
        let mut before = 0;
        let mut out = Vec::new();
        for phase in Phase::ALL {
            if let Some(at) = self.get(phase) {
                out.push((phase, at.saturating_sub(before)));
                before = before.max(at);
            }
        }
        out
    }
}

/// One turn's startup, as [`crate::Activity::Startup`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Startup {
    /// Where the harness ran: `local`, or the sandbox provider
    /// (`microsandbox`, `container`, `agent_sandbox`, `substrate`, a recipe).
    pub provider: String,
    /// Which way the sandbox was made ready.
    pub path: StartupPath,
    /// The phases that occurred.
    #[serde(default)]
    pub phases: Phases,
}

impl Startup {
    /// `local cold: queued 0 ms, placement 12 ms, ...`: one line for
    /// `by log`, each phase's offset from admission.
    pub fn line(&self) -> String {
        let phases: Vec<String> = Phase::ALL
            .iter()
            .filter_map(|p| self.phases.get(*p).map(|ms| format!("{p} {ms} ms")))
            .collect();
        format!(
            "startup: {} {}; {}",
            self.provider,
            self.path,
            match phases.is_empty() {
                true => "no phase reached".to_owned(),
                false => phases.join(", "),
            }
        )
    }
}

/// The clock a [`Timer`] reads: milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Collects a turn's phase offsets on one clock.
pub struct Timer {
    clock: Clock,
    admitted_ms: u64,
    phases: Phases,
}

impl Timer {
    /// A timer for an operation admitted at `admitted_ms` (on `clock`).
    pub fn new(clock: Clock, admitted_ms: u64) -> Timer {
        Timer {
            clock,
            admitted_ms,
            phases: Phases::default(),
        }
    }

    /// A timer on the system clock (`branchyard_support::time::now_ms`).
    /// Without an admission time the turn's own start is the admission.
    pub fn system(admitted_ms: Option<u64>) -> Timer {
        let clock: Clock = Arc::new(branchyard_support::time::now_ms);
        let admitted = admitted_ms.unwrap_or_else(|| clock());
        Timer::new(clock, admitted)
    }

    /// The offset from admission now.
    pub fn offset(&self) -> u64 {
        (self.clock)().saturating_sub(self.admitted_ms)
    }

    /// Record that `phase` is reached now. The first time only.
    pub fn mark(&mut self, phase: Phase) {
        let at = self.offset();
        self.mark_at(phase, at);
    }

    /// Record that `phase` was reached at `offset` ms from admission. The
    /// first time only.
    pub fn mark_at(&mut self, phase: Phase, offset: u64) {
        let slot = self.phases.slot(phase);
        if slot.is_none() {
            *slot = Some(offset);
        }
    }

    /// Whether `phase` has been recorded.
    pub fn has(&self, phase: Phase) -> bool {
        self.phases.get(phase).is_some()
    }

    /// The phases so far.
    pub fn phases(&self) -> &Phases {
        &self.phases
    }

    /// The startup to record, for a turn that ran `provider` by `path`.
    pub fn finish(&self, provider: &str, path: StartupPath) -> Startup {
        Startup {
            provider: provider.to_owned(),
            path,
            phases: self.phases.clone(),
        }
    }
}

/// Which [`StartupPath`] a sandbox's origin and its provider's report mean.
pub fn path_of(origin: &SandboxOrigin, launch: Option<branchyard_sandbox::Launch>) -> StartupPath {
    match origin {
        SandboxOrigin::Resumed => StartupPath::Resumed,
        SandboxOrigin::Branched { .. }
        | SandboxOrigin::Prepared { .. }
        | SandboxOrigin::Environment { .. } => StartupPath::Snapshot,
        SandboxOrigin::Fresh { .. } => match launch {
            Some(launch) if launch.pooled => StartupPath::Pool,
            Some(launch) if launch.image_cached == Some(true) => StartupPath::CachedImage,
            _ => StartupPath::Cold,
        },
    }
}

/// What [`report`] selects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only this provider.
    pub provider: Option<String>,
    /// Only startups recorded at or after this time (milliseconds since
    /// the Unix epoch).
    pub since_ms: Option<u64>,
}

/// The distribution of one phase over the startups of one provider and path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The provider.
    pub provider: String,
    /// The path.
    pub path: StartupPath,
    /// A [`Phase`]'s name, or `total` for the whole startup (the last
    /// offset reached).
    pub phase: String,
    /// Startups in which the phase occurred.
    pub count: usize,
    /// Median time the phase took, in milliseconds.
    pub p50_ms: u64,
    /// 95th percentile, in milliseconds (the nearest rank).
    pub p95_ms: u64,
}

/// [`report`]'s result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Startups counted.
    pub startups: usize,
    /// One row per provider, path and phase, in that order.
    pub rows: Vec<Row>,
}

/// The nearest-rank percentile `q` (0 to 1) of `sorted`, which is not empty.
fn rank(sorted: &[u64], q: f64) -> u64 {
    let n = sorted.len();
    let index = ((q * n as f64).ceil() as usize).clamp(1, n) - 1;
    sorted[index]
}

/// Percentiles of the recorded startups in `events` that `filter` selects,
/// per provider, path and phase. A phase's figure is the time that phase
/// took, so providers compare stage by stage; `total` is the whole startup.
pub fn report(events: &[FeedEvent], filter: &Filter) -> Report {
    let mut samples: BTreeMap<(String, StartupPath, String), Vec<u64>> = BTreeMap::new();
    let mut startups = 0;
    for event in events {
        let Activity::Startup(startup) = &event.event.activity else {
            continue;
        };
        if filter
            .since_ms
            .is_some_and(|since| event.event.at_ms < since)
            || filter
                .provider
                .as_deref()
                .is_some_and(|provider| provider != startup.provider)
        {
            continue;
        }
        startups += 1;
        let mut add = |phase: String, ms: u64| {
            samples
                .entry((startup.provider.clone(), startup.path, phase))
                .or_default()
                .push(ms)
        };
        for (phase, ms) in startup.phases.durations() {
            add(phase.to_string(), ms);
        }
        if let Some(total) = startup.phases.total() {
            add("total".to_owned(), total);
        }
    }
    let rows = samples
        .into_iter()
        .map(|((provider, path, phase), mut values)| {
            values.sort_unstable();
            Row {
                provider,
                path,
                phase,
                count: values.len(),
                p50_ms: rank(&values, 0.5),
                p95_ms: rank(&values, 0.95),
            }
        })
        .collect();
    Report { startups, rows }
}

impl Report {
    /// The report as a table for a terminal.
    pub fn render(&self) -> String {
        if self.rows.is_empty() {
            return "no recorded startups\n".to_owned();
        }
        let mut out = format!("{} startups\n", self.startups);
        out += &format!(
            "{:<14} {:<13} {:<16} {:>5} {:>9} {:>9}\n",
            "PROVIDER", "PATH", "PHASE", "N", "P50", "P95"
        );
        for row in &self.rows {
            out += &format!(
                "{:<14} {:<13} {:<16} {:>5} {:>9} {:>9}\n",
                row.provider,
                row.path.to_string(),
                row.phase,
                row.count,
                format!("{} ms", row.p50_ms),
                format!("{} ms", row.p95_ms)
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::RecordedEvent;

    fn fake_clock(now: &Arc<AtomicU64>) -> Clock {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    #[test]
    fn a_timer_reads_offsets_from_admission_on_its_clock() {
        let now = Arc::new(AtomicU64::new(10_000));
        let mut timer = Timer::new(fake_clock(&now), 9_000);
        timer.mark(Phase::Queued);
        now.store(10_040, Ordering::SeqCst);
        timer.mark(Phase::Placement);
        now.store(10_900, Ordering::SeqCst);
        timer.mark(Phase::SandboxReady);
        // Only the first time counts.
        now.store(99_000, Ordering::SeqCst);
        timer.mark(Phase::SandboxReady);
        timer.mark_at(Phase::ImageReady, 700);
        let phases = timer.phases();
        assert_eq!(phases.queued, Some(1_000));
        assert_eq!(phases.placement, Some(1_040));
        assert_eq!(phases.image_ready, Some(700));
        assert_eq!(phases.sandbox_ready, Some(1_900));
        assert_eq!(phases.first_event, None);
        assert!(!timer.has(Phase::FirstEvent));
        assert_eq!(phases.total(), Some(1_900));
    }

    #[test]
    fn durations_are_the_gaps_between_phases_that_occurred() {
        let phases = Phases {
            queued: Some(5),
            placement: Some(20),
            // No image_ready: the provider did not measure it.
            sandbox_ready: Some(920),
            source_attached: Some(1_000),
            harness_started: Some(1_100),
            handshake_done: Some(1_300),
            first_event: Some(1_250),
            ..Phases::default()
        };
        let durations: Vec<(String, u64)> = phases
            .durations()
            .into_iter()
            .map(|(p, ms)| (p.to_string(), ms))
            .collect();
        assert_eq!(
            durations,
            [
                ("queued".to_owned(), 5),
                ("placement".to_owned(), 15),
                ("sandbox_ready".to_owned(), 900),
                ("source_attached".to_owned(), 80),
                ("harness_started".to_owned(), 100),
                ("handshake_done".to_owned(), 200),
                // Earlier than the phase before it: counts as no time.
                ("first_event".to_owned(), 0),
            ]
        );
        assert_eq!(phases.total(), Some(1_300));
    }

    #[test]
    fn the_wire_form_omits_phases_that_did_not_occur() {
        let startup = Startup {
            provider: "local".into(),
            path: StartupPath::Local,
            phases: Phases {
                queued: Some(0),
                placement: Some(3),
                ..Phases::default()
            },
        };
        let value = serde_json::to_value(&startup).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "provider": "local", "path": "local",
                "phases": {"queued": 0, "placement": 3}
            })
        );
        assert_eq!(serde_json::from_value::<Startup>(value).unwrap(), startup);
        assert_eq!(
            startup.line(),
            "startup: local local; queued 0 ms, placement 3 ms"
        );
    }

    #[test]
    fn a_sandbox_origin_and_the_providers_report_choose_the_path() {
        let fresh = SandboxOrigin::Fresh { reason: None };
        let launch = |pooled, cached| {
            Some(branchyard_sandbox::Launch {
                pooled,
                image_cached: cached,
                image_ready_ms: None,
            })
        };
        assert_eq!(path_of(&fresh, None), StartupPath::Cold);
        assert_eq!(path_of(&fresh, launch(false, None)), StartupPath::Cold);
        assert_eq!(
            path_of(&fresh, launch(false, Some(true))),
            StartupPath::CachedImage
        );
        assert_eq!(path_of(&fresh, launch(true, Some(true))), StartupPath::Pool);
        assert_eq!(
            path_of(&SandboxOrigin::Resumed, launch(true, None)),
            StartupPath::Resumed
        );
    }

    fn recorded(
        position: u64,
        at_ms: u64,
        provider: &str,
        path: StartupPath,
        total: u64,
    ) -> FeedEvent {
        FeedEvent {
            position,
            branch: "b".into(),
            event: RecordedEvent {
                at_ms,
                activity: Activity::Startup(Box::new(Startup {
                    provider: provider.into(),
                    path,
                    phases: Phases {
                        queued: Some(0),
                        sandbox_ready: Some(total / 2),
                        first_event: Some(total),
                        ..Phases::default()
                    },
                })),
            },
        }
    }

    #[test]
    fn the_report_gives_nearest_rank_percentiles_per_provider_path_and_phase() {
        let mut events: Vec<FeedEvent> = (1..=20)
            .map(|n| recorded(n, 1_000 + n, "agent_sandbox", StartupPath::Pool, n * 100))
            .collect();
        events.push(recorded(
            21,
            1_000,
            "microsandbox",
            StartupPath::Cold,
            4_000,
        ));
        events.push(recorded(22, 50, "microsandbox", StartupPath::Cold, 9_000));
        // Not a startup.
        events.push(FeedEvent {
            position: 23,
            branch: "b".into(),
            event: RecordedEvent {
                at_ms: 2_000,
                activity: Activity::Prompt("p".into()),
            },
        });

        let all = report(&events, &Filter::default());
        assert_eq!(all.startups, 22);
        let pool_total = all
            .rows
            .iter()
            .find(|r| r.provider == "agent_sandbox" && r.phase == "total")
            .unwrap();
        assert_eq!(
            (pool_total.count, pool_total.p50_ms, pool_total.p95_ms),
            (20, 1_000, 1_900)
        );
        // sandbox_ready took half the total: total / 2 after queued (0).
        let ready = all
            .rows
            .iter()
            .find(|r| r.provider == "agent_sandbox" && r.phase == "sandbox_ready")
            .unwrap();
        assert_eq!((ready.p50_ms, ready.p95_ms), (500, 950));
        // Rows are ordered by provider, path, then phase.
        let order: Vec<&str> = all.rows.iter().map(|r| r.provider.as_str()).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted);

        let recent = report(
            &events,
            &Filter {
                provider: Some("microsandbox".into()),
                since_ms: Some(1_000),
            },
        );
        assert_eq!(recent.startups, 1);
        assert!(recent.rows.iter().all(|r| r.provider == "microsandbox"));
        assert_eq!(
            recent
                .rows
                .iter()
                .find(|r| r.phase == "total")
                .unwrap()
                .p50_ms,
            4_000
        );
        assert!(recent.render().contains("microsandbox"));
        assert_eq!(
            report(&[], &Filter::default()).render(),
            "no recorded startups\n"
        );
    }
}
