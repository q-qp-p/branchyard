//! Metrics, as OpenTelemetry instruments, served at `GET /metrics` in the
//! Prometheus text exposition format (0.0.4) and pushed over OTLP when the
//! process is configured to (see `branchyard-telemetry`).
//!
//! Every family is declared once in [`FAMILIES`], so a name, its type, its
//! unit and its help text cannot drift apart; `docs/observability.md` lists
//! them. A counter or histogram is recorded as things happen in this
//! process. A gauge (and a counter whose absolute value another component
//! keeps, such as the sync replicator's) is an *observable* instrument: its
//! callback reads a [`Board`] of values that the scrape fills from the
//! operation store ([`Metrics::render_with`]), or that a registered
//! [`Source`] fills when the OTLP reader collects.
//!
//! Counters count what this process did: on several servers sharing a
//! database, sum them across servers (each operation is claimed, run and
//! finished by one of them). Gauges read from the store (queue depth and
//! age, live workers) describe the shared database, so every server
//! reports the same values: take one, or `max`, not the sum.
//!
//! Label values are bounded: a label is a state, a kind, a harness, a
//! configured name or a route template, never an id. The ones a client can
//! make up (a model's name, a tool's name) pass through [`Metrics::bounded`]
//! or an allowlist and collapse to `other` past a cap.

use branchyard_support::LockExt as _;
use branchyard_telemetry::prometheus::{
    collect_families, prometheus_name, Declared, Kind as PromKind, Sample,
};
use branchyard_telemetry::Meters;
use opentelemetry::metrics::{
    Counter, Histogram, ObservableCounter, ObservableGauge, UpDownCounter,
};
use opentelemetry::KeyValue;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A metric family's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Added to as things happen.
    Counter,
    /// Read when collected, from the [`Board`].
    Gauge,
    /// Observed as a distribution.
    Histogram,
    /// Added to and subtracted from as things happen (in flight).
    UpDown,
    /// A counter whose absolute value is read when collected, from the
    /// [`Board`].
    ObservedCounter,
}

impl Kind {
    fn prom(self) -> PromKind {
        match self {
            Kind::Counter | Kind::ObservedCounter => PromKind::Counter,
            Kind::Gauge | Kind::UpDown => PromKind::Gauge,
            Kind::Histogram => PromKind::Histogram,
        }
    }

    fn is_counter(self) -> bool {
        matches!(self, Kind::Counter | Kind::ObservedCounter)
    }
}

/// A declared family: name, type, unit, help, and a histogram's bucket bounds.
pub struct Family {
    /// The instrument's name (and, for Branchyard's own, its Prometheus name).
    pub name: &'static str,
    /// Its type.
    pub kind: Kind,
    /// Its UCUM unit (`s`, `By`, `{token}`), or empty.
    pub unit: &'static str,
    /// Its help text.
    pub help: &'static str,
    /// A histogram's bucket upper bounds.
    pub buckets: &'static [f64],
}

impl Family {
    /// The name this family has at `GET /metrics`.
    pub fn exposed(&self) -> String {
        prometheus_name(self.name, self.unit, self.kind.is_counter())
    }
}

const SECONDS: &[f64] = &[
    0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

/// Start latencies are seconds, often under one.
const START_SECONDS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 1800.0,
];

/// OpenTelemetry's recommended bounds for HTTP server durations.
const HTTP_SECONDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// OpenTelemetry's recommended bounds for `gen_ai.client.token.usage`.
const TOKENS: &[f64] = &[
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

/// A tool call or a gateway call: milliseconds to minutes.
const CALL_SECONDS: &[f64] = &[0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0];

/// From the prompt to the harness's first event.
const FIRST_EVENT_SECONDS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

const fn family(name: &'static str, kind: Kind, unit: &'static str, help: &'static str) -> Family {
    Family {
        name,
        kind,
        unit,
        help,
        buckets: &[],
    }
}

const fn histogram(
    name: &'static str,
    unit: &'static str,
    help: &'static str,
    buckets: &'static [f64],
) -> Family {
    Family {
        name,
        kind: Kind::Histogram,
        unit,
        help,
        buckets,
    }
}

pub const OPERATIONS: &str = "branchyard_operations";
pub const ADMITTED: &str = "branchyard_operations_admitted_total";
pub const FINISHED: &str = "branchyard_operations_finished_total";
pub const QUEUE_DEPTH: &str = "branchyard_queue_depth";
pub const QUEUE_OLDEST: &str = "branchyard_queue_oldest_age_seconds";
pub const CLAIMS: &str = "branchyard_claims_total";
pub const CLAIM_WAIT: &str = "branchyard_claim_wait_seconds";
pub const RENEWALS: &str = "branchyard_lease_renewals_total";
pub const EXPIRIES: &str = "branchyard_lease_expiries_total";
pub const TURNS_STARTED: &str = "branchyard_turns_started_total";
pub const TURNS_ENDED: &str = "branchyard_turns_ended_total";
pub const TURN_DURATION: &str = "branchyard_turn_duration_seconds";
pub const TOOL_CALLS: &str = "branchyard_tool_calls_total";
pub const COST: &str = "branchyard_cost_usd_total";
pub const CONNECTOR_CALLS: &str = "branchyard_connector_calls_total";
pub const MODEL_CALLS: &str = "branchyard_model_calls_total";
pub const MODEL_TOKENS: &str = "branchyard_model_tokens_total";
pub const MODEL_COST: &str = "branchyard_model_cost_usd_total";
pub const WEBHOOKS: &str = "branchyard_webhook_deliveries_total";
/// Looks the trigger dispatcher finished; see `triggers::dispatch::spawn`.
pub const TRIGGER_LOOKS: &str = "branchyard_trigger_looks_total";
pub const WORKERS: &str = "branchyard_workers_live";
pub const WORKER_SEEN: &str = "branchyard_worker_last_seen_seconds";
pub const BUILD: &str = "branchyard_build_info";
pub const POOL_SLOTS: &str = "branchyard_pool_slots";
pub const POOL_CLAIMS: &str = "branchyard_pool_claims_total";
pub const POOL_MADE: &str = "branchyard_pool_slots_made_total";
pub const POOL_FILL: &str = "branchyard_pool_fill_seconds";
pub const POOL_DISCARDED: &str = "branchyard_pool_slots_discarded_total";
pub const START: &str = "branchyard_start_seconds";
/// Histogram of seconds each startup phase of a turn took, labelled
/// `provider`, `path` and `phase`.
pub const START_PHASE: &str = "branchyard_start_phase_seconds";
pub const SYNC_BYTES: &str = "branchyard_sync_bytes_total";
pub const SYNC_OBJECTS: &str = "branchyard_sync_objects_total";
pub const SYNC_SWAPS: &str = "branchyard_sync_swaps_total";
pub const SYNC_SWAP_CONFLICTS: &str = "branchyard_sync_swap_conflicts_total";
pub const SYNC_RETRIES: &str = "branchyard_sync_retries_total";
pub const SYNC_DIVERGENCES: &str = "branchyard_sync_divergences_total";
pub const SYNC_CORRUPT: &str = "branchyard_sync_corrupt_total";
pub const SYNC_ERRORS: &str = "branchyard_sync_errors_total";
pub const SYNC_PENDING: &str = "branchyard_sync_pending";
pub const SYNC_LAG: &str = "branchyard_sync_lag_seconds";
pub const EFFECTS: &str = "branchyard_effects";
pub const APPROVALS_PENDING: &str = "branchyard_approvals_pending";
/// Requests served, by route template, method and status class.
pub const HTTP_DURATION: &str = "http.server.request.duration";
/// Requests being served now.
pub const HTTP_ACTIVE: &str = "http.server.active_requests";
/// Refused API requests, by the error code in the response.
pub const API_ERRORS: &str = "branchyard_api_errors_total";
/// From an operation's run starting to its outcome being recorded.
pub const OPERATION_DURATION: &str = "branchyard_operation_duration_seconds";
/// Branches with a turn running, read at scrape time.
pub const TURNS_IN_FLIGHT: &str = "branchyard_turns_in_flight";
/// From a turn's prompt to the harness's first event.
pub const TURN_FIRST_EVENT: &str = "branchyard_turn_first_event_seconds";
/// Branches an operation finished with stalled.
pub const TURN_STALLS: &str = "branchyard_turn_stalls_total";
/// A turn as a GenAI client operation (`invoke_agent`).
pub const GENAI_OPERATION_DURATION: &str = "gen_ai.client.operation.duration";
/// Tokens harnesses reported spending.
pub const GENAI_TOKEN_USAGE: &str = "gen_ai.client.token.usage";
/// Cost harnesses reported, by harness and model.
pub const USAGE_COST: &str = "branchyard_usage_cost_usd_total";
/// A tool call, from `tool_started` to the branch's next event.
pub const TOOL_DURATION: &str = "branchyard_tool_call_duration_seconds";
/// A connector gateway call's recorded latency.
pub const CONNECTOR_DURATION: &str = "branchyard_connector_call_duration_seconds";

/// Time a sandbox provider's action took.
pub const SANDBOX_ACTION: &str = "branchyard_sandbox_action_duration_seconds";
// The sandbox crate times its actions under the same name; a test below
// checks the two are one string.
/// Time a git operation on a hot path took.
pub const GIT_OPERATION: &str = "branchyard_git_operation_duration_seconds";
// The workspace crate times git under the same name; a test below checks
// the two are one string.

/// Every family this server exposes, in the order it is written.
pub const FAMILIES: &[Family] = &[
    family(
        BUILD,
        Kind::Gauge,
        "",
        "Always 1; the version label is this server's.",
    ),
    family(
        OPERATIONS,
        Kind::Gauge,
        "",
        "Unfinished operations in the shared queue, by state (queued: unclaimed; running: claimed).",
    ),
    family(
        ADMITTED,
        Kind::Counter,
        "",
        "Operations this process admitted, by kind and tenant.",
    ),
    family(
        FINISHED,
        Kind::Counter,
        "",
        "Operations this process recorded finished, by kind and state (succeeded, failed, interrupted).",
    ),
    histogram(
        OPERATION_DURATION,
        "s",
        "Time an operation ran, from its run starting to its outcome being recorded, by kind and state.",
        SECONDS,
    ),
    family(
        QUEUE_DEPTH,
        Kind::Gauge,
        "",
        "Unclaimed queued operations, by tenant, priority and required worker labels.",
    ),
    family(
        QUEUE_OLDEST,
        Kind::Gauge,
        "s",
        "Age of the oldest unclaimed queued operation, by tenant, priority and required worker labels.",
    ),
    family(
        CLAIMS,
        Kind::Counter,
        "",
        "Queued operations this process's dispatcher claimed, by tenant and priority.",
    ),
    histogram(
        CLAIM_WAIT,
        "s",
        "Time from admission to a claim, by priority.",
        SECONDS,
    ),
    family(
        RENEWALS,
        Kind::Counter,
        "",
        "Claim lease renewals by this process, by result (renewed, lost, error).",
    ),
    family(
        EXPIRIES,
        Kind::Counter,
        "",
        "Claims this process took over from a worker whose lease had lapsed.",
    ),
    family(
        TURNS_STARTED,
        Kind::Counter,
        "",
        "Turns started by operations this process ran, by harness.",
    ),
    family(
        TURNS_ENDED,
        Kind::Counter,
        "",
        "Turns ended by operations this process ran, by harness and outcome.",
    ),
    histogram(
        TURN_DURATION,
        "s",
        "Turn duration, from the prompt to the turn's end, by harness.",
        SECONDS,
    ),
    histogram(
        TURN_FIRST_EVENT,
        "s",
        "Time from a turn's prompt to the harness's first event, by harness.",
        FIRST_EVENT_SECONDS,
    ),
    family(
        TURNS_IN_FLIGHT,
        Kind::Gauge,
        "",
        "Branches with a turn running, by harness, in the repositories this process serves.",
    ),
    family(
        TURN_STALLS,
        Kind::Counter,
        "",
        "Branches that an operation this process ran left stalled (no harness activity for the stall budget), by harness.",
    ),
    histogram(
        GENAI_OPERATION_DURATION,
        "s",
        "GenAI client operation duration: a turn as an invoke_agent operation, by agent (harness) and outcome.",
        SECONDS,
    ),
    histogram(
        GENAI_TOKEN_USAGE,
        "{token}",
        "Tokens harnesses reported spending in turns this process ran, by token type (input, output), agent (harness) and model.",
        TOKENS,
    ),
    family(
        USAGE_COST,
        Kind::Counter,
        "",
        "Cost, in USD, harnesses reported with their usage in turns this process ran, by harness and model.",
    ),
    family(
        TOOL_CALLS,
        Kind::Counter,
        "",
        "Tool calls harnesses reported in operations this process ran, by harness.",
    ),
    histogram(
        TOOL_DURATION,
        "s",
        "Tool call duration (from tool_started to the branch's next event), by harness and tool; tools outside the known list are `other`.",
        CALL_SECONDS,
    ),
    family(
        COST,
        Kind::Counter,
        "",
        "Harness-reported cost, in USD, of operations this process ran, by tenant and harness.",
    ),
    family(
        CONNECTOR_CALLS,
        Kind::Counter,
        "",
        "Connector gateway calls in operations this process ran, by connector and decision.",
    ),
    histogram(
        CONNECTOR_DURATION,
        "s",
        "Connector gateway call latency in operations this process ran, by connector and decision.",
        CALL_SECONDS,
    ),
    family(
        MODEL_CALLS,
        Kind::Counter,
        "",
        "Model gateway calls in operations this process ran, by model and decision.",
    ),
    family(
        MODEL_TOKENS,
        Kind::Counter,
        "",
        "Tokens of model gateway calls in operations this process ran, by model and kind (input, output, cache_read, cache_write).",
    ),
    family(
        MODEL_COST,
        Kind::Counter,
        "",
        "Metered cost, in USD, of model gateway calls in operations this process ran, by model.",
    ),
    family(
        WEBHOOKS,
        Kind::Counter,
        "",
        "Webhook deliveries by this process, by result (delivered, retried, dead_lettered).",
    ),
    family(
        TRIGGER_LOOKS,
        Kind::Counter,
        "",
        "Times this process's trigger dispatcher looked for schedules due and runs waiting to fire.",
    ),
    family(
        WORKERS,
        Kind::Gauge,
        "",
        "Workers that recorded themselves alive in the last 15 seconds.",
    ),
    family(
        WORKER_SEEN,
        Kind::Gauge,
        "s",
        "Seconds since each live worker last recorded itself alive.",
    ),
    family(
        EFFECTS,
        Kind::Gauge,
        "",
        "Entries in each repository's effect ledger, by class (reversible, compensable, irreversible) and state.",
    ),
    family(
        APPROVALS_PENDING,
        Kind::Gauge,
        "",
        "Approvals waiting for an answer in each repository.",
    ),
    family(
        POOL_SLOTS,
        Kind::Gauge,
        "",
        "Warm pool slots of each repository on this host, by state (ready, filling, claimed).",
    ),
    family(
        POOL_CLAIMS,
        Kind::Counter,
        "",
        "New branches of operations this process ran whose workspace has a pool, by repository and result (hit, miss).",
    ),
    family(
        POOL_MADE,
        Kind::Counter,
        "",
        "Warm pool slots this process's keepers made, by repository and result (made, failed).",
    ),
    histogram(
        POOL_FILL,
        "s",
        "Time this process's keepers took to make one warm pool slot, by repository.",
        SECONDS,
    ),
    family(
        POOL_DISCARDED,
        Kind::Counter,
        "",
        "Warm pool slots this process's keepers removed as stale or left by a stopped process, by repository.",
    ),
    histogram(
        START,
        "s",
        "Start latency of a task's new branches, from admission to the harness's first prompt, by pool (hit, miss, none), provider and path.",
        START_SECONDS,
    ),
    histogram(
        START_PHASE,
        "s",
        "Time one phase of a task's new branches' start took, by provider, path and phase; a phase that did not occur is not counted.",
        START_SECONDS,
    ),
    family(
        SYNC_BYTES,
        Kind::ObservedCounter,
        "By",
        "Bytes this process's sync sent to and read from the remote, by repository and direction (up, down).",
    ),
    family(
        SYNC_OBJECTS,
        Kind::ObservedCounter,
        "",
        "Objects this process's sync wrote to and read from the remote, by repository and direction (up, down).",
    ),
    family(
        SYNC_SWAPS,
        Kind::ObservedCounter,
        "",
        "Task manifests this process swapped into the remote, by repository.",
    ),
    family(
        SYNC_SWAP_CONFLICTS,
        Kind::ObservedCounter,
        "",
        "Manifest swaps that lost to another writer and were merged and tried again, by repository.",
    ),
    family(
        SYNC_RETRIES,
        Kind::ObservedCounter,
        "",
        "Remote requests tried again after a transient failure, by repository.",
    ),
    family(
        SYNC_DIVERGENCES,
        Kind::ObservedCounter,
        "",
        "Divergences recorded as conflict branches, by repository.",
    ),
    family(
        SYNC_CORRUPT,
        Kind::ObservedCounter,
        "",
        "Objects read from the remote and refused because they did not match their name, by repository.",
    ),
    family(
        SYNC_ERRORS,
        Kind::ObservedCounter,
        "",
        "Task syncs that failed (and were queued to try again), by repository.",
    ),
    family(
        SYNC_PENDING,
        Kind::Gauge,
        "",
        "Tasks queued in the sync outbox, by repository.",
    ),
    family(
        SYNC_LAG,
        Kind::Gauge,
        "s",
        "How long the oldest queued change has waited to reach the remote, by repository.",
    ),
    histogram(
        HTTP_DURATION,
        "s",
        "Duration of HTTP requests this server served, by route template, method and status class.",
        HTTP_SECONDS,
    ),
    family(
        HTTP_ACTIVE,
        Kind::UpDown,
        "{request}",
        "HTTP requests being served now, by method.",
    ),
    family(
        API_ERRORS,
        Kind::Counter,
        "",
        "API requests refused, by the error code in the response.",
    ),
    histogram(
        SANDBOX_ACTION,
        "s",
        "Time a sandbox provider's action took in turns this process ran, by provider, action (ensure, exec, checkpoint, restore, branch, pause, resume, stop, destroy) and result (ok, error).",
        CALL_SECONDS,
    ),
    histogram(
        GIT_OPERATION,
        "s",
        "Time a git operation on a hot path took in this process, by operation (worktree_add, commit, merge, rebase, push) and result (ok, error).",
        CALL_SECONDS,
    ),
];

fn declared(name: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| f.name == name)
}

/// The labels of a family in the order `GET /metrics` writes them: the
/// order they have always had, which the instruments' own storage (sorted
/// attributes) would otherwise change.
pub fn label_order(name: &str) -> &'static [&'static str] {
    match name {
        BUILD => &["version"],
        OPERATIONS => &["state"],
        ADMITTED => &["kind", "tenant"],
        FINISHED | OPERATION_DURATION => &["kind", "state"],
        QUEUE_DEPTH | QUEUE_OLDEST => &["tenant", "priority", "labels"],
        CLAIMS => &["tenant", "priority"],
        CLAIM_WAIT => &["priority"],
        RENEWALS | WEBHOOKS => &["result"],
        TURNS_STARTED | TURN_DURATION | TURN_FIRST_EVENT | TURNS_IN_FLIGHT | TURN_STALLS
        | TOOL_CALLS => &["harness"],
        TURNS_ENDED => &["harness", "outcome"],
        GENAI_OPERATION_DURATION => &[
            "gen_ai.operation.name",
            "gen_ai.agent.name",
            "by.turn.outcome",
            "error.type",
        ],
        GENAI_TOKEN_USAGE => &[
            "gen_ai.operation.name",
            "gen_ai.token.type",
            "gen_ai.agent.name",
            "gen_ai.request.model",
        ],
        USAGE_COST => &["harness", "model"],
        TOOL_DURATION => &["harness", "tool"],
        COST => &["tenant", "harness"],
        CONNECTOR_CALLS | CONNECTOR_DURATION => &["connector", "decision"],
        MODEL_CALLS => &["model", "decision"],
        MODEL_TOKENS => &["model", "kind"],
        MODEL_COST => &["model"],
        WORKER_SEEN => &["worker", "host"],
        EFFECTS => &["repo", "class", "state"],
        APPROVALS_PENDING | POOL_FILL | POOL_DISCARDED | SYNC_SWAPS | SYNC_SWAP_CONFLICTS
        | SYNC_RETRIES | SYNC_DIVERGENCES | SYNC_CORRUPT | SYNC_ERRORS | SYNC_PENDING
        | SYNC_LAG => &["repo"],
        POOL_SLOTS => &["repo", "state"],
        POOL_CLAIMS | POOL_MADE => &["repo", "result"],
        START => &["pool", "provider", "path"],
        START_PHASE => &["provider", "path", "phase"],
        SYNC_BYTES | SYNC_OBJECTS => &["repo", "direction"],
        HTTP_DURATION => &["http.route", "http.request.method", "status_class"],
        HTTP_ACTIVE => &["http.request.method"],
        API_ERRORS => &["code"],
        SANDBOX_ACTION => &["provider", "action", "result"],
        GIT_OPERATION => &["operation", "result"],
        _ => &[],
    }
}

/// Label names and values, in a fixed order.
pub type Labels = Vec<(String, String)>;

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn attributes(pairs: &[(&str, &str)]) -> Vec<KeyValue> {
    pairs
        .iter()
        .map(|(k, v)| KeyValue::new((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// The values of the families that are read when collected (gauges and
/// observed counters), by family and labels.
#[derive(Clone, Debug, Default)]
pub struct Board {
    series: BTreeMap<&'static str, BTreeMap<Labels, f64>>,
}

impl Board {
    /// The value of `name` with exactly `pairs`, if set.
    pub fn get(&self, name: &str, pairs: &[(&str, &str)]) -> Option<f64> {
        let wanted = labels(pairs);
        self.series
            .iter()
            .find(|(n, _)| **n == name)
            .and_then(|(_, series)| series.get(&wanted))
            .copied()
    }
}

/// Fills a [`Board`] from wherever its values live (the operation store, the
/// sync replicator). An error leaves the previous values in place.
pub type Source = Arc<dyn Fn(&mut Board) -> Result<(), String> + Send + Sync>;

/// How long a filled board is current for observable callbacks that find a
/// [`Source`]: one collection runs every callback, and only the first needs
/// to read the store.
const FRESH_FOR: Duration = Duration::from_secs(2);

/// Models and other names a client can make up: how many distinct values a
/// label takes before the rest are `other`.
const BOUNDED_LABEL_VALUES: usize = 64;

#[derive(Default)]
struct BoardState {
    board: Mutex<Board>,
    source: Mutex<Option<Source>>,
    filled: Mutex<Option<Instant>>,
}

impl BoardState {
    fn fill(&self, fill: &dyn Fn(&mut Board) -> Result<(), String>) -> Result<(), String> {
        let mut next = Board::default();
        fill(&mut next)?;
        *self.board.lock_recovering("metrics board") = next;
        *self.filled.lock_recovering("metrics board time") = Some(Instant::now());
        Ok(())
    }

    /// Re-read through the registered source when the board is stale.
    fn ensure_current(&self) {
        let Some(source) = self.source.lock_recovering("metrics source").clone() else {
            return;
        };
        let current = self
            .filled
            .lock_recovering("metrics board time")
            .is_some_and(|at| at.elapsed() < FRESH_FOR);
        if !current {
            branchyard_support::best_effort(
                "read the gauges for a collection",
                self.fill(&*source),
            );
        }
    }

    fn observe(&self, name: &str, mut observe: impl FnMut(f64, &[KeyValue])) {
        self.ensure_current();
        let board = self.board.lock_recovering("metrics board");
        if let Some(series) = board.series.get(name) {
            for (labels, value) in series {
                let attrs: Vec<KeyValue> = labels
                    .iter()
                    .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
                    .collect();
                observe(*value, &attrs);
            }
        }
    }
}

/// The process's instruments.
pub struct Metrics {
    meters: Meters,
    counters: HashMap<&'static str, Counter<f64>>,
    histograms: HashMap<&'static str, Histogram<f64>>,
    up_downs: HashMap<&'static str, UpDownCounter<f64>>,
    state: Arc<BoardState>,
    bounded: Mutex<HashMap<&'static str, HashSet<String>>>,
    // The observable instruments stay registered while they live.
    _gauges: Vec<ObservableGauge<f64>>,
    _observed: Vec<ObservableCounter<f64>>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Metrics")
    }
}

impl Default for Metrics {
    /// Metrics of their own, apart from the process's: what tests use.
    fn default() -> Metrics {
        Metrics::new(Meters::isolated())
    }
}

impl Metrics {
    /// Instruments on `meters`.
    pub fn new(meters: Meters) -> Metrics {
        let meter = meters.meter("branchyard");
        let state = Arc::new(BoardState::default());
        let mut counters = HashMap::new();
        let mut histograms = HashMap::new();
        let mut up_downs = HashMap::new();
        let mut gauges = Vec::new();
        let mut observed = Vec::new();
        for family in FAMILIES {
            match family.kind {
                Kind::Counter => {
                    counters.insert(
                        family.name,
                        meter
                            .f64_counter(family.name)
                            .with_unit(family.unit)
                            .with_description(family.help)
                            .build(),
                    );
                }
                Kind::Histogram => {
                    histograms.insert(
                        family.name,
                        meter
                            .f64_histogram(family.name)
                            .with_unit(family.unit)
                            .with_description(family.help)
                            .with_boundaries(family.buckets.to_vec())
                            .build(),
                    );
                }
                Kind::UpDown => {
                    up_downs.insert(
                        family.name,
                        meter
                            .f64_up_down_counter(family.name)
                            .with_unit(family.unit)
                            .with_description(family.help)
                            .build(),
                    );
                }
                Kind::Gauge => {
                    let (state, name) = (state.clone(), family.name);
                    gauges.push(
                        meter
                            .f64_observable_gauge(family.name)
                            .with_unit(family.unit)
                            .with_description(family.help)
                            .with_callback(move |observer| {
                                state.observe(name, |value, attrs| observer.observe(value, attrs));
                            })
                            .build(),
                    );
                }
                Kind::ObservedCounter => {
                    let (state, name) = (state.clone(), family.name);
                    observed.push(
                        meter
                            .f64_observable_counter(family.name)
                            .with_unit(family.unit)
                            .with_description(family.help)
                            .with_callback(move |observer| {
                                state.observe(name, |value, attrs| observer.observe(value, attrs));
                            })
                            .build(),
                    );
                }
            }
        }
        Metrics {
            meters,
            counters,
            histograms,
            up_downs,
            state,
            bounded: Mutex::new(HashMap::new()),
            _gauges: gauges,
            _observed: observed,
        }
    }

    /// The meters these instruments are on.
    pub fn meters(&self) -> &Meters {
        &self.meters
    }

    /// Add `by` to a counter. A negative or non-finite amount is ignored.
    pub fn add(&self, name: &str, pairs: &[(&str, &str)], by: f64) {
        let counter = self.counters.get(name);
        debug_assert!(counter.is_some(), "{name} is not a declared counter");
        if let (Some(counter), true) = (counter, by.is_finite() && by >= 0.0) {
            counter.add(by, &attributes(pairs));
        }
    }

    /// Add one to a counter.
    pub fn inc(&self, name: &str, pairs: &[(&str, &str)]) {
        self.add(name, pairs, 1.0);
    }

    /// Record an observation in a histogram. A non-finite value is ignored.
    pub fn observe(&self, name: &str, pairs: &[(&str, &str)], value: f64) {
        let histogram = self.histograms.get(name);
        debug_assert!(histogram.is_some(), "{name} is not a declared histogram");
        if let (Some(histogram), true) = (histogram, value.is_finite()) {
            histogram.record(value, &attributes(pairs));
        }
    }

    /// Add `by` (which may be negative) to an up-down counter.
    pub fn up_down(&self, name: &str, pairs: &[(&str, &str)], by: f64) {
        let counter = self.up_downs.get(name);
        debug_assert!(
            counter.is_some(),
            "{name} is not a declared up-down counter"
        );
        if let Some(counter) = counter {
            counter.add(by, &attributes(pairs));
        }
    }

    /// `value` as a label of `class`, or `other` once the class has had
    /// 64 distinct values: a name a client can make up
    /// cannot grow the series without limit.
    pub fn bounded(&self, class: &'static str, value: &str) -> String {
        let mut seen = self.bounded.lock_recovering("bounded labels");
        let values = seen.entry(class).or_default();
        if values.contains(value) {
            value.to_owned()
        } else if values.len() < BOUNDED_LABEL_VALUES {
            values.insert(value.to_owned());
            value.to_owned()
        } else {
            "other".to_owned()
        }
    }

    /// Read gauges from `source` whenever a collection finds them stale
    /// (an OTLP push has no scrape to fill them first).
    pub fn set_source(&self, source: Source) {
        *self.state.source.lock_recovering("metrics source") = Some(source);
    }

    /// The families as `GET /metrics` writes them, in [`FAMILIES`] order.
    pub fn declared() -> Vec<Declared> {
        FAMILIES
            .iter()
            .map(|f| Declared {
                name: f.exposed(),
                kind: f.kind.prom(),
                help: f.help.to_owned(),
                labels: label_order(f.name)
                    .iter()
                    .map(|l| branchyard_telemetry::prometheus::sanitize(l))
                    .collect(),
            })
            .collect()
    }

    /// Fill the gauges with `fill`, then render everything in the
    /// Prometheus text format. A `fill` error is returned and nothing is
    /// rendered.
    pub fn render_with<E: std::fmt::Display>(
        &self,
        fill: impl FnOnce(&mut Board) -> Result<(), E>,
    ) -> Result<String, String> {
        let mut board = Board::default();
        fill(&mut board).map_err(|e| e.to_string())?;
        self.state.fill(&|next| {
            *next = board.clone();
            Ok(())
        })?;
        self.meters.reader().render(&Metrics::declared())
    }

    /// Render with the gauges as they were last filled.
    pub fn render(&self) -> Result<String, String> {
        self.meters.reader().render(&Metrics::declared())
    }

    /// The series of `name` with exactly `pairs` as labels, as collected
    /// now: a counter's or gauge's value, or a histogram's buckets, sum
    /// and count. For tests and health checks.
    pub fn sample(&self, name: &str, pairs: &[(&str, &str)]) -> Option<Sample> {
        let family = declared(name)?;
        let collected = self.meters.reader().collect().ok()?;
        let mut families = collect_families(&collected);
        let series = families.remove(&family.exposed())?.series;
        let mut wanted: Labels = pairs
            .iter()
            .map(|(k, v)| {
                (
                    branchyard_telemetry::prometheus::sanitize(k),
                    (*v).to_owned(),
                )
            })
            .collect();
        wanted.sort();
        series.get(&wanted).cloned()
    }

    /// A counter's or gauge's value; 0 when it has no such series.
    pub fn value(&self, name: &str, pairs: &[(&str, &str)]) -> f64 {
        match self.sample(name, pairs) {
            Some(Sample::Number(n)) => n,
            _ => 0.0,
        }
    }

    /// A histogram's observation count and sum; zeros when it has no such
    /// series.
    pub fn distribution(&self, name: &str, pairs: &[(&str, &str)]) -> (u64, f64) {
        match self.sample(name, pairs) {
            Some(Sample::Histogram { count, sum, .. }) => (count, sum),
            _ => (0, 0.0),
        }
    }
}

/// Each repository's sync counters (this process's, read from the
/// replicator at scrape time), queued tasks and lag.
pub fn sync_series(
    board: &mut Board,
    repos: &[(String, branchyard_sync::stats::Snapshot, usize, f64)],
) {
    for (repo, s, pending, lag) in repos {
        let r = repo.as_str();
        let mut counter = |name: &'static str, pairs: &[(&str, &str)], value: u64| {
            set(board, name, pairs, value as f64);
        };
        counter(SYNC_BYTES, &[("repo", r), ("direction", "up")], s.bytes_up);
        counter(
            SYNC_BYTES,
            &[("repo", r), ("direction", "down")],
            s.bytes_down,
        );
        counter(
            SYNC_OBJECTS,
            &[("repo", r), ("direction", "up")],
            s.objects_up,
        );
        counter(
            SYNC_OBJECTS,
            &[("repo", r), ("direction", "down")],
            s.objects_down,
        );
        counter(SYNC_SWAPS, &[("repo", r)], s.swaps);
        counter(SYNC_SWAP_CONFLICTS, &[("repo", r)], s.swap_conflicts);
        counter(SYNC_RETRIES, &[("repo", r)], s.retries);
        counter(SYNC_DIVERGENCES, &[("repo", r)], s.divergences);
        counter(SYNC_CORRUPT, &[("repo", r)], s.corrupt);
        counter(SYNC_ERRORS, &[("repo", r)], s.errors);
        set(board, SYNC_PENDING, &[("repo", r)], *pending as f64);
        set(board, SYNC_LAG, &[("repo", r)], *lag);
    }
}

/// A value read at scrape time, added to a [`Board`].
pub fn set(board: &mut Board, name: &'static str, pairs: &[(&str, &str)], value: f64) {
    debug_assert!(
        declared(name).is_some_and(|f| matches!(f.kind, Kind::Gauge | Kind::ObservedCounter)),
        "{name} is not a declared gauge"
    );
    board
        .series
        .entry(name)
        .or_default()
        .insert(labels(pairs), value);
}

/// The content type of the text [`Metrics::render_with`] returns.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The gauges read from the store at scrape time: the shared queue by
/// state, its depth and oldest age by tenant, priority and labels, and live
/// workers. `now_ms` is the scraping process's clock.
pub fn queue_gauges(
    board: &mut Board,
    queue: &[crate::store::Queued],
    workers: &[crate::store::LiveWorker],
    now_ms: i64,
) {
    let running = queue.iter().filter(|q| q.claimed).count();
    set(
        board,
        OPERATIONS,
        &[("state", "queued")],
        (queue.len() - running) as f64,
    );
    set(board, OPERATIONS, &[("state", "running")], running as f64);
    let mut groups: BTreeMap<(String, i32, String), (usize, i64)> = BTreeMap::new();
    for q in queue.iter().filter(|q| !q.claimed) {
        let key = (q.tenant.clone(), q.priority, q.requires.join(","));
        let entry = groups.entry(key).or_insert((0, i64::MAX));
        entry.0 += 1;
        entry.1 = entry.1.min(q.enqueued_ms);
    }
    for ((tenant, priority, required), (depth, oldest)) in groups {
        let priority = priority.to_string();
        let pairs = [
            ("tenant", tenant.as_str()),
            ("priority", priority.as_str()),
            ("labels", required.as_str()),
        ];
        set(board, QUEUE_DEPTH, &pairs, depth as f64);
        let age = (now_ms - oldest).max(0) as f64 / 1000.0;
        set(board, QUEUE_OLDEST, &pairs, age);
    }
    set(board, WORKERS, &[], workers.len() as f64);
    for worker in workers {
        set(
            board,
            WORKER_SEEN,
            &[("worker", &worker.id), ("host", &worker.host)],
            worker.seen_ms_ago as f64 / 1000.0,
        );
    }
    set(board, BUILD, &[("version", env!("CARGO_PKG_VERSION"))], 1.0);
}

/// The warm pool gauge read at scrape time: `repo`'s slots on this host
/// by state, every state written (zero included).
pub fn pool_gauges(board: &mut Board, repo: &str, slots: &[branchyard::PoolSlot]) {
    for (state, name) in [
        (branchyard::PoolSlotState::Ready, "ready"),
        (branchyard::PoolSlotState::Filling, "filling"),
        (branchyard::PoolSlotState::Claimed, "claimed"),
    ] {
        let n = slots.iter().filter(|s| s.state == state).count();
        set(
            board,
            POOL_SLOTS,
            &[("repo", repo), ("state", name)],
            n as f64,
        );
    }
}

/// The turns running now, read at scrape time: `branches` of one repository
/// whose status is running, by harness. A harness with a turn running in no
/// branch is not written (a gauge of zero would need the list of harnesses).
pub fn turn_gauges(board: &mut Board, branches: &[branchyard::BranchInfo]) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for branch in branches {
        if matches!(branch.status, branchyard::BranchStatus::Running) {
            *counts.entry(branch.harness.as_str()).or_default() += 1;
        }
    }
    for (harness, n) in counts {
        let key = vec![("harness".to_owned(), harness.to_owned())];
        let slot = board
            .series
            .entry(TURNS_IN_FLIGHT)
            .or_default()
            .entry(key)
            .or_insert(0.0);
        *slot += n as f64;
    }
}

/// The effect ledger's gauges read at scrape time: `repo`'s entries by
/// class and state (only pairs that occur), and its waiting approvals
/// (zero included).
pub fn effect_gauges(
    board: &mut Board,
    repo: &str,
    entries: &[branchyard::effects::EffectEntry],
    pending: usize,
) {
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for entry in entries {
        *counts
            .entry((entry.class.as_str(), entry.state.as_str()))
            .or_default() += 1;
    }
    for ((class, state), n) in counts {
        set(
            board,
            EFFECTS,
            &[("repo", repo), ("class", class), ("state", state)],
            n as f64,
        );
    }
    set(board, APPROVALS_PENDING, &[("repo", repo)], pending as f64);
}

/// Count what a keeper's fill of `repo`'s pool did.
pub fn record_fill(metrics: &Metrics, repo: &str, filled: &branchyard::PoolFill) {
    for slot in &filled.made {
        metrics.inc(POOL_MADE, &[("repo", repo), ("result", "made")]);
        if let Some(ms) = slot.fill_ms {
            metrics.observe(POOL_FILL, &[("repo", repo)], ms as f64 / 1000.0);
        }
    }
    if filled.error.is_some() {
        metrics.inc(POOL_MADE, &[("repo", repo), ("result", "failed")]);
    }
    let removed = filled.discarded.len() + filled.reclaimed.len();
    if removed > 0 {
        metrics.add(POOL_DISCARDED, &[("repo", repo)], removed as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LiveWorker, Queued};

    /// A minimal parser of the exposition format, independent of the
    /// encoder: every line is a comment (`# HELP name text` or `# TYPE
    /// name type`) or a sample `name{labels} value`, label values quoted
    /// with `\\`, `\"` and `\n` escapes. Returns the samples.
    /// A sample: its name, labels and value.
    type Sample = (String, Vec<(String, String)>, String);

    fn parse(text: &str) -> Vec<Sample> {
        let mut samples = Vec::new();
        let mut typed = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# ") {
                let mut words = rest.splitn(3, ' ');
                let (kind, name, tail) = (words.next(), words.next(), words.next());
                match kind {
                    Some("HELP") => assert!(tail.is_some(), "{line}"),
                    Some("TYPE") => {
                        let tail = tail.unwrap();
                        assert!(
                            ["counter", "gauge", "histogram", "summary", "untyped"].contains(&tail),
                            "{line}"
                        );
                        assert!(
                            typed.insert(name.unwrap().to_owned(), tail).is_none(),
                            "a family typed twice: {line}"
                        );
                    }
                    _ => panic!("unexpected comment {line}"),
                }
                continue;
            }
            let name_end = line.find(['{', ' ']).unwrap();
            let name = &line[..name_end];
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                "{line}"
            );
            let mut rest = &line[name_end..];
            let mut labels = Vec::new();
            if let Some(body) = rest.strip_prefix('{') {
                let mut chars = body.char_indices().peekable();
                loop {
                    let start = chars.peek().unwrap().0;
                    let mut end = start;
                    for (i, c) in chars.by_ref() {
                        if c == '=' {
                            end = i;
                            break;
                        }
                    }
                    let key = body[start..end].to_owned();
                    assert_eq!(chars.next().map(|c| c.1), Some('"'), "{line}");
                    let mut value = String::new();
                    loop {
                        match chars.next().unwrap().1 {
                            '\\' => match chars.next().unwrap().1 {
                                'n' => value.push('\n'),
                                c @ ('\\' | '"') => value.push(c),
                                c => panic!("bad escape \\{c} in {line}"),
                            },
                            '"' => break,
                            c => value.push(c),
                        }
                    }
                    labels.push((key, value));
                    match chars.next().unwrap() {
                        (_, ',') => continue,
                        (i, '}') => {
                            rest = &body[i + 1..];
                            break;
                        }
                        (_, c) => panic!("unexpected {c} in {line}"),
                    }
                }
            }
            let value = rest.strip_prefix(' ').unwrap();
            assert!(
                value.parse::<f64>().is_ok() || ["+Inf", "-Inf", "NaN"].contains(&value),
                "{line}"
            );
            let family = name
                .strip_suffix("_bucket")
                .or_else(|| name.strip_suffix("_sum"))
                .or_else(|| name.strip_suffix("_count"))
                .filter(|f| typed.get(*f) == Some(&"histogram"))
                .unwrap_or(name);
            assert!(
                typed.contains_key(family),
                "a sample before its TYPE: {line}"
            );
            samples.push((name.to_owned(), labels, value.to_owned()));
        }
        samples
    }

    fn sample<'a>(samples: &'a [Sample], name: &str, labels: &[(&str, &str)]) -> Option<&'a str> {
        samples
            .iter()
            .find(|(n, l, _)| {
                n == name
                    && l.len() == labels.len()
                    && labels
                        .iter()
                        .all(|(k, v)| l.iter().any(|(lk, lv)| lk == k && lv == v))
            })
            .map(|(_, _, v)| v.as_str())
    }

    fn queue_fixture() -> (Vec<Queued>, Vec<LiveWorker>) {
        let queued = |id: &str, tenant: &str, priority, requires: &[&str], at, claimed| Queued {
            id: id.into(),
            repo: "r".into(),
            tenant: tenant.into(),
            priority,
            requires: requires.iter().map(|s| (*s).to_owned()).collect(),
            enqueued_ms: at,
            claimed,
        };
        (
            vec![
                queued("a", "acme", 0, &["gpu", "linux"], 1_000, false),
                queued("b", "acme", 0, &["gpu", "linux"], 4_000, false),
                queued("c", "other", -3, &[], 9_000, true),
            ],
            vec![LiveWorker {
                id: "w_1".into(),
                host: "h".into(),
                labels: vec![],
                repos: vec!["r".into()],
                seen_ms_ago: 1_500,
                inventory: None,
            }],
        )
    }

    #[test]
    fn counters_histograms_and_gauges_render_in_the_exposition_format() {
        let metrics = Metrics::default();
        metrics.inc(CLAIMS, &[("tenant", "acme"), ("priority", "5")]);
        metrics.inc(CLAIMS, &[("tenant", "acme"), ("priority", "5")]);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], 0.25);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], -1.0);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], f64::NAN);
        // A label value that needs every escape.
        metrics.inc(
            CONNECTOR_CALLS,
            &[("connector", "a\"b\\c\nd"), ("decision", "allowed")],
        );
        for seconds in [0.05, 3.0, 4000.0] {
            metrics.observe(TURN_DURATION, &[("harness", "claude-code")], seconds);
        }
        let (queue, workers) = queue_fixture();
        let text = metrics
            .render_with(|board| {
                queue_gauges(board, &queue, &workers, 11_000);
                Ok::<_, String>(())
            })
            .unwrap();
        let samples = parse(&text);
        // Every declared family has its HELP and TYPE, once, in order.
        let order: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        let declared: Vec<String> = FAMILIES.iter().map(Family::exposed).collect();
        assert_eq!(order, declared);
        for family in FAMILIES {
            let kind = match family.kind.prom() {
                PromKind::Counter => "counter",
                PromKind::Gauge => "gauge",
                PromKind::Histogram => "histogram",
            };
            assert!(
                text.contains(&format!("# TYPE {} {kind}\n", family.exposed())),
                "{}",
                family.name
            );
            if family.kind.is_counter() {
                assert!(family.exposed().ends_with("_total"), "{}", family.name);
            }
        }
        assert_eq!(
            sample(&samples, CLAIMS, &[("tenant", "acme"), ("priority", "5")]),
            Some("2")
        );
        assert_eq!(
            sample(&samples, COST, &[("tenant", "acme"), ("harness", "codex")]),
            Some("0.25"),
            "negative and NaN additions are ignored"
        );
        assert_eq!(
            sample(
                &samples,
                CONNECTOR_CALLS,
                &[("connector", "a\"b\\c\nd"), ("decision", "allowed")]
            ),
            Some("1")
        );
        assert!(text.contains(r#"connector="a\"b\\c\nd""#), "{text}");
        // Histogram buckets are cumulative, with +Inf, sum and count.
        let bucket = |le: &str| {
            sample(
                &samples,
                &format!("{TURN_DURATION}_bucket"),
                &[("harness", "claude-code"), ("le", le)],
            )
        };
        assert_eq!(bucket("0.1"), Some("1"));
        assert_eq!(bucket("1"), Some("1"));
        assert_eq!(bucket("5"), Some("2"));
        assert_eq!(bucket("3600"), Some("2"));
        assert_eq!(bucket("+Inf"), Some("3"));
        assert_eq!(
            sample(
                &samples,
                &format!("{TURN_DURATION}_count"),
                &[("harness", "claude-code")]
            ),
            Some("3")
        );
        assert_eq!(
            sample(
                &samples,
                &format!("{TURN_DURATION}_sum"),
                &[("harness", "claude-code")]
            ),
            Some("4003.05")
        );
        // Gauges from the queue: unclaimed depth and oldest age by group.
        assert_eq!(
            sample(&samples, OPERATIONS, &[("state", "queued")]),
            Some("2")
        );
        assert_eq!(
            sample(&samples, OPERATIONS, &[("state", "running")]),
            Some("1")
        );
        let group = [
            ("tenant", "acme"),
            ("priority", "0"),
            ("labels", "gpu,linux"),
        ];
        assert_eq!(sample(&samples, QUEUE_DEPTH, &group), Some("2"));
        assert_eq!(sample(&samples, QUEUE_OLDEST, &group), Some("10"));
        assert_eq!(
            sample(
                &samples,
                QUEUE_DEPTH,
                &[("tenant", "other"), ("priority", "-3"), ("labels", "")]
            ),
            None,
            "a claimed operation is not queue depth"
        );
        assert_eq!(sample(&samples, WORKERS, &[]), Some("1"));
        assert_eq!(
            sample(&samples, WORKER_SEEN, &[("worker", "w_1"), ("host", "h")]),
            Some("1.5")
        );
    }

    /// The names a Prometheus dashboard or alert already uses: the
    /// OpenTelemetry rebuild must not move one.
    #[test]
    fn the_existing_metric_names_are_unchanged() {
        const LEGACY: &[&str] = &[
            "branchyard_build_info",
            "branchyard_operations",
            "branchyard_operations_admitted_total",
            "branchyard_operations_finished_total",
            "branchyard_queue_depth",
            "branchyard_queue_oldest_age_seconds",
            "branchyard_claims_total",
            "branchyard_claim_wait_seconds",
            "branchyard_lease_renewals_total",
            "branchyard_lease_expiries_total",
            "branchyard_turns_started_total",
            "branchyard_turns_ended_total",
            "branchyard_turn_duration_seconds",
            "branchyard_tool_calls_total",
            "branchyard_cost_usd_total",
            "branchyard_connector_calls_total",
            "branchyard_model_calls_total",
            "branchyard_model_tokens_total",
            "branchyard_model_cost_usd_total",
            "branchyard_webhook_deliveries_total",
            "branchyard_trigger_looks_total",
            "branchyard_workers_live",
            "branchyard_worker_last_seen_seconds",
            "branchyard_effects",
            "branchyard_approvals_pending",
            "branchyard_pool_slots",
            "branchyard_pool_claims_total",
            "branchyard_pool_slots_made_total",
            "branchyard_pool_fill_seconds",
            "branchyard_pool_slots_discarded_total",
            "branchyard_start_seconds",
            "branchyard_start_phase_seconds",
            "branchyard_sync_bytes_total",
            "branchyard_sync_objects_total",
            "branchyard_sync_swaps_total",
            "branchyard_sync_swap_conflicts_total",
            "branchyard_sync_retries_total",
            "branchyard_sync_divergences_total",
            "branchyard_sync_corrupt_total",
            "branchyard_sync_errors_total",
            "branchyard_sync_pending",
            "branchyard_sync_lag_seconds",
        ];
        let exposed: Vec<String> = FAMILIES.iter().map(Family::exposed).collect();
        for name in LEGACY {
            assert!(
                exposed.iter().any(|e| e == name),
                "{name} is no longer exposed"
            );
        }
        // Branchyard's own families are named the way they are exposed.
        for family in FAMILIES
            .iter()
            .filter(|f| f.name.starts_with("branchyard_"))
        {
            assert_eq!(family.exposed(), family.name);
        }
        let mut unique = exposed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), exposed.len(), "two families share a name");
    }

    /// The catalogue in `docs/observability.md` is generated from the
    /// declarations (`tools/docs_facts.py`); this checks it names what the
    /// server really exposes, with the Rust translation of the names.
    #[test]
    fn every_family_is_in_the_documented_catalogue() {
        let docs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/observability.md"
        ))
        .unwrap();
        for family in FAMILIES {
            assert!(
                docs.contains(&format!("| `{}` |", family.exposed())),
                "{} is not in docs/observability.md; run `python3 tools/docs_facts.py --write`",
                family.exposed()
            );
            for label in label_order(family.name) {
                assert!(
                    docs.contains(&branchyard_telemetry::prometheus::sanitize(label)),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn warm_pool_gauges_and_fills_render() {
        let slot = |state: &str, fill_ms: Option<u64>| -> branchyard::PoolSlot {
            serde_json::from_value(serde_json::json!({
                "id": "s", "state": state, "base": "c", "path": "/p",
                "created_ms": 1, "changed_ms": 1, "fill_ms": fill_ms
            }))
            .unwrap()
        };
        let metrics = Metrics::default();
        record_fill(
            &metrics,
            "app",
            &branchyard::PoolFill {
                made: vec![slot("ready", Some(2_500)), slot("ready", Some(500))],
                discarded: vec![("old".into(), "setup changed".into())],
                reclaimed: vec![("gone".into(), "its process stopped".into())],
                error: Some("setup failed".into()),
                ..branchyard::PoolFill::default()
            },
        );
        metrics.observe(
            START,
            &[("pool", "hit"), ("provider", "local"), ("path", "local")],
            0.2,
        );
        let text = metrics
            .render_with(|board| {
                pool_gauges(
                    board,
                    "app",
                    &[
                        slot("ready", None),
                        slot("ready", None),
                        slot("filling", None),
                    ],
                );
                Ok::<_, String>(())
            })
            .unwrap();
        let samples = parse(&text);
        let gauge = |state| sample(&samples, POOL_SLOTS, &[("repo", "app"), ("state", state)]);
        assert_eq!(gauge("ready"), Some("2"));
        assert_eq!(gauge("filling"), Some("1"));
        assert_eq!(gauge("claimed"), Some("0"));
        assert_eq!(
            sample(&samples, POOL_MADE, &[("repo", "app"), ("result", "made")]),
            Some("2")
        );
        assert_eq!(
            sample(
                &samples,
                POOL_MADE,
                &[("repo", "app"), ("result", "failed")]
            ),
            Some("1")
        );
        assert_eq!(
            sample(&samples, POOL_DISCARDED, &[("repo", "app")]),
            Some("2")
        );
        assert_eq!(
            sample(&samples, &format!("{POOL_FILL}_sum"), &[("repo", "app")]),
            Some("3")
        );
        let start = |le| {
            sample(
                &samples,
                &format!("{START}_bucket"),
                &[
                    ("pool", "hit"),
                    ("provider", "local"),
                    ("path", "local"),
                    ("le", le),
                ],
            )
        };
        assert_eq!(start("0.25"), Some("1"));
        assert_eq!(start("0.1"), Some("0"));
    }

    #[test]
    fn the_new_golden_families_render_with_semantic_convention_names() {
        let metrics = Metrics::default();
        metrics.observe(
            HTTP_DURATION,
            &[
                ("http.route", "/v1/repos/{repo}/tasks"),
                ("http.request.method", "POST"),
                ("status_class", "2xx"),
            ],
            0.03,
        );
        metrics.up_down(HTTP_ACTIVE, &[("http.request.method", "GET")], 1.0);
        metrics.up_down(HTTP_ACTIVE, &[("http.request.method", "GET")], 1.0);
        metrics.up_down(HTTP_ACTIVE, &[("http.request.method", "GET")], -1.0);
        metrics.observe(
            GENAI_TOKEN_USAGE,
            &[
                ("gen_ai.token.type", "input"),
                ("gen_ai.agent.name", "codex"),
                ("gen_ai.request.model", "gpt"),
            ],
            1200.0,
        );
        metrics.observe(
            GENAI_OPERATION_DURATION,
            &[
                ("gen_ai.operation.name", "invoke_agent"),
                ("gen_ai.agent.name", "codex"),
                ("by.turn.outcome", "completed"),
            ],
            12.0,
        );
        let text = metrics.render().unwrap();
        let samples = parse(&text);
        assert_eq!(
            sample(
                &samples,
                "http_server_request_duration_seconds_count",
                &[
                    ("http_route", "/v1/repos/{repo}/tasks"),
                    ("http_request_method", "POST"),
                    ("status_class", "2xx")
                ]
            ),
            Some("1")
        );
        assert_eq!(
            sample(
                &samples,
                "http_server_active_requests",
                &[("http_request_method", "GET")]
            ),
            Some("1")
        );
        assert_eq!(
            sample(
                &samples,
                "gen_ai_client_token_usage_sum",
                &[
                    ("gen_ai_token_type", "input"),
                    ("gen_ai_agent_name", "codex"),
                    ("gen_ai_request_model", "gpt")
                ]
            ),
            Some("1200")
        );
        assert_eq!(
            sample(
                &samples,
                "gen_ai_client_operation_duration_seconds_count",
                &[
                    ("gen_ai_operation_name", "invoke_agent"),
                    ("gen_ai_agent_name", "codex"),
                    ("by_turn_outcome", "completed")
                ]
            ),
            Some("1")
        );
    }

    /// A name a client can make up takes a bounded number of label values.
    #[test]
    fn bounded_labels_collapse_to_other() {
        let metrics = Metrics::default();
        let labels: Vec<String> = (0..BOUNDED_LABEL_VALUES + 10)
            .map(|i| metrics.bounded("model", &format!("m{i}")))
            .collect();
        assert_eq!(labels[0], "m0");
        assert_eq!(
            labels[BOUNDED_LABEL_VALUES - 1],
            format!("m{}", BOUNDED_LABEL_VALUES - 1)
        );
        assert!(labels[BOUNDED_LABEL_VALUES..].iter().all(|l| l == "other"));
        // A value already seen keeps its name.
        assert_eq!(metrics.bounded("model", "m3"), "m3");
        // Classes count apart.
        assert_eq!(metrics.bounded("tool", "new"), "new");
    }

    /// An OTLP push has no scrape to fill the gauges first: a collection
    /// that finds them stale reads the registered source, once for all of
    /// its callbacks.
    #[test]
    fn a_registered_source_fills_stale_gauges_for_a_collection() {
        let metrics = Metrics::default();
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = reads.clone();
        metrics.set_source(Arc::new(move |board| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            set(board, WORKERS, &[], 4.0);
            set(board, SYNC_PENDING, &[("repo", "r")], 2.0);
            Ok(())
        }));
        assert_eq!(metrics.value(WORKERS, &[]), 4.0);
        assert_eq!(metrics.value(SYNC_PENDING, &[("repo", "r")]), 2.0);
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one read for two collections and two gauges: the board is current for a moment"
        );
    }

    #[test]
    fn a_failing_fill_renders_nothing() {
        let metrics = Metrics::default();
        let refused = metrics.render_with(|_| Err::<(), _>("the store is down"));
        assert_eq!(refused, Err("the store is down".to_owned()));
    }

    #[test]
    fn sample_helpers_read_back_what_was_recorded() {
        let metrics = Metrics::default();
        metrics.add(COST, &[("tenant", "t"), ("harness", "h")], 1.5);
        metrics.observe(CLAIM_WAIT, &[("priority", "0")], 2.0);
        metrics.observe(CLAIM_WAIT, &[("priority", "0")], 4.0);
        assert_eq!(
            metrics.value(COST, &[("harness", "h"), ("tenant", "t")]),
            1.5
        );
        assert_eq!(
            metrics.value(COST, &[("tenant", "t")]),
            0.0,
            "labels match exactly"
        );
        assert_eq!(
            metrics.distribution(CLAIM_WAIT, &[("priority", "0")]),
            (2, 6.0)
        );
        assert_eq!(
            metrics.distribution(CLAIM_WAIT, &[("priority", "9")]),
            (0, 0.0)
        );
    }
}
