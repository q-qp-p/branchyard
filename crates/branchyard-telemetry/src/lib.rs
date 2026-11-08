//! Telemetry setup for every Branchyard binary: the server, a worker and
//! `by` run locally. One call, [`init`], installs a [`tracing`] subscriber
//! with the binary's own log layers plus a `tracing-opentelemetry` layer, an
//! OpenTelemetry tracer provider and meter provider configured from the
//! standard `OTEL_*` variables, and the W3C trace-context propagator.
//!
//! * **Off unless asked.** Nothing is exported, no thread is started and no
//!   span is bridged unless an OTLP endpoint (or `OTEL_TRACES_EXPORTER=otlp`
//!   and the metrics counterpart) is set. [`Settings`] says exactly when.
//! * **Spans** are `tracing` spans. The OpenTelemetry layer bridges the
//!   ones whose target is [`TRACE_TARGET`]; any other span stays a
//!   log-correlation span. A span whose times are known only afterwards (a
//!   turn read back from recorded events) is written with [`spans::record`].
//! * **Metrics** are OpenTelemetry instruments from a [`Meters`]. Each
//!   provider has a [`prometheus::PromReader`], so a server can answer
//!   `GET /metrics` from the same instruments it pushes over OTLP.
//! * **Propagation** is in [`propagation`]: inject into a header map or
//!   `TRACEPARENT`, extract from either, with the OpenTelemetry propagator.
//!
//! The engine crate and other libraries depend on none of this: they emit
//! `tracing` events and spans, and the binary that installs the SDK decides
//! where they go.

pub mod prometheus;
pub mod propagation;
pub mod settings;
pub mod spans;
#[cfg(feature = "testing")]
pub mod testing;

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::Layer as _;
use tracing_subscriber::Registry;

pub use settings::{Protocol, Role, SamplerChoice, Settings};

/// A log layer a binary hands to [`init`], already filtered the way it wants.
pub type BoxedLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync + 'static>;

/// How long [`Guard`] waits for the last spans and metrics to be sent.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What a binary tells [`init`].
pub struct Config {
    /// What this process is: the `by.role` resource attribute.
    pub role: Role,
    /// The `service.name` when `OTEL_SERVICE_NAME` (or
    /// `OTEL_RESOURCE_ATTRIBUTES`) does not set one.
    pub service_name: String,
    /// The `service.version`.
    pub service_version: String,
    /// The binary's own layers (its log output), each carrying its own
    /// filter.
    pub layers: Vec<BoxedLayer>,
}

/// The `target` of the spans that are exported: write
/// `info_span!(target: TRACE_TARGET, "name", ...)` for a span that belongs
/// in a trace. Every other span stays a log-correlation span only, so a
/// request, a poll or a library's internal span does not become a trace of
/// its own.
pub const TRACE_TARGET: &str = branchyard_support::trace::TARGET;

/// Whether the OpenTelemetry layer takes the span or event described by
/// `meta`: spans of [`TRACE_TARGET`], and the per-query store spans when the
/// operator asked for them; never events.
pub(crate) fn bridged(meta: &tracing::Metadata<'_>) -> bool {
    meta.is_span()
        && (meta.target() == TRACE_TARGET
            || (meta.target() == branchyard_support::trace::STORE_TARGET
                && branchyard_support::trace::store_spans()))
}

/// Whether finished spans go anywhere in this process: an exporter is
/// configured, or a test captures them. When false, building a span after
/// the fact (see [`spans::record`]) is wasted work.
pub fn spans_enabled() -> bool {
    branchyard_support::trace::enabled()
}

pub(crate) fn set_spans_enabled(on: bool) {
    if on {
        spans::install_hooks();
        branchyard_support::trace::set_store_spans(
            std::env::var("BRANCHYARD_TRACE_STORE").is_ok_and(|v| v == "1" || v == "true"),
        );
    }
    branchyard_support::trace::set_enabled(on);
}

/// The meter provider of a process and the reader that renders it as
/// Prometheus text. Cheap to clone.
#[derive(Clone)]
pub struct Meters {
    provider: SdkMeterProvider,
    reader: prometheus::PromReader,
}

impl std::fmt::Debug for Meters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Meters")
    }
}

static PROCESS_METERS: OnceLock<Meters> = OnceLock::new();

impl Meters {
    /// Meters of their own: a provider with only the Prometheus reader and
    /// no resource attributes. What a server uses when the process did not
    /// call [`init`], and what tests use for isolation.
    pub fn isolated() -> Meters {
        let reader = prometheus::PromReader::new();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        Meters { provider, reader }
    }

    /// The meters [`init`] installed, if it ran in this process.
    pub fn process() -> Option<Meters> {
        PROCESS_METERS.get().cloned()
    }

    /// The process's meters if [`init`] ran, else [`Meters::isolated`].
    pub fn process_or_isolated() -> Meters {
        Meters::process().unwrap_or_else(Meters::isolated)
    }

    /// A meter for instruments of the named instrumentation scope.
    pub fn meter(&self, scope: &'static str) -> Meter {
        self.provider.meter(scope)
    }

    /// The Prometheus text reader.
    pub fn reader(&self) -> &prometheus::PromReader {
        &self.reader
    }
}

/// Telemetry in use; flushes and shuts down when dropped.
pub struct Guard {
    tracer: Option<SdkTracerProvider>,
    meters: Option<SdkMeterProvider>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("traces", &self.tracer.is_some())
            .field("metrics", &self.meters.is_some())
            .finish()
    }
}

impl Guard {
    /// Whether spans are being exported.
    pub fn exporting_traces(&self) -> bool {
        self.tracer.is_some()
    }

    /// Whether metrics are being pushed.
    pub fn exporting_metrics(&self) -> bool {
        self.meters.is_some()
    }

    /// Send what is buffered now.
    pub fn flush(&self) {
        if let Some(tracer) = &self.tracer {
            branchyard_support::best_effort("flush spans", tracer.force_flush());
        }
        if let Some(meters) = &self.meters {
            branchyard_support::best_effort("flush metrics", meters.force_flush());
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(tracer) = self.tracer.take() {
            branchyard_support::best_effort(
                "shut down the trace exporter",
                tracer.shutdown_with_timeout(SHUTDOWN_TIMEOUT),
            );
        }
        if let Some(meters) = self.meters.take() {
            branchyard_support::best_effort(
                "shut down the metrics exporter",
                meters.shutdown_with_timeout(SHUTDOWN_TIMEOUT),
            );
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn hostname() -> Option<String> {
    let name = std::env::var("HOSTNAME").ok().or_else(|| {
        // Linux keeps it here; elsewhere there is no such file, which is
        // not worth a warning for an attribute that is only nice to have.
        match std::fs::read_to_string("/proc/sys/kernel/hostname") {
            Ok(name) => Some(name),
            Err(e) => {
                tracing::debug!(error = %e, "no host name for the telemetry resource");
                None
            }
        }
    })?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn resource(config: &Config) -> Resource {
    let mut builder = Resource::builder();
    let named = std::env::var("OTEL_SERVICE_NAME").is_ok_and(|v| !v.trim().is_empty())
        || std::env::var("OTEL_RESOURCE_ATTRIBUTES").is_ok_and(|v| v.contains("service.name="));
    if !named {
        builder = builder.with_service_name(config.service_name.clone());
    }
    builder = builder.with_attributes([
        KeyValue::new("service.version", config.service_version.clone()),
        KeyValue::new("by.role", config.role.as_str()),
    ]);
    if let Some(host) = hostname() {
        builder = builder.with_attribute(KeyValue::new("host.name", host));
    }
    builder.build()
}

fn sampler(choice: SamplerChoice) -> Sampler {
    let parent = |inner: Sampler| Sampler::ParentBased(Box::new(inner));
    match choice {
        SamplerChoice::AlwaysOn => Sampler::AlwaysOn,
        SamplerChoice::AlwaysOff => Sampler::AlwaysOff,
        SamplerChoice::Ratio(r) => Sampler::TraceIdRatioBased(r),
        SamplerChoice::ParentBasedAlwaysOn => parent(Sampler::AlwaysOn),
        SamplerChoice::ParentBasedAlwaysOff => parent(Sampler::AlwaysOff),
        SamplerChoice::ParentBasedRatio(r) => parent(Sampler::TraceIdRatioBased(r)),
    }
}

/// Build the exporters of `protocol` where the transport needs it: HTTP
/// exporters are built on a thread of their own (a blocking HTTP client
/// must not be created inside an async runtime), gRPC ones inside `runtime`.
fn on_build_thread<T: Send>(
    runtime: Option<&tokio::runtime::Runtime>,
    build: impl FnOnce() -> T + Send,
) -> Result<T, String> {
    match runtime {
        Some(runtime) => Ok(runtime.block_on(async { build() })),
        None => std::thread::scope(|scope| {
            scope
                .spawn(build)
                .join()
                .map_err(|_| "the exporter's builder panicked".to_owned())
        }),
    }
}

fn span_exporter(
    protocol: Protocol,
    runtime: Option<&tokio::runtime::Runtime>,
) -> Result<opentelemetry_otlp::SpanExporter, String> {
    let built = on_build_thread(runtime, move || match protocol {
        Protocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build(),
        Protocol::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .build(),
        Protocol::HttpJson => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
            .build(),
    })?;
    built.map_err(|e| format!("the OTLP trace exporter: {e}"))
}

fn metric_exporter(
    protocol: Protocol,
    runtime: Option<&tokio::runtime::Runtime>,
) -> Result<opentelemetry_otlp::MetricExporter, String> {
    let built = on_build_thread(runtime, move || match protocol {
        Protocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .build(),
        Protocol::HttpProtobuf => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .build(),
        Protocol::HttpJson => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
            .build(),
    })?;
    built.map_err(|e| format!("the OTLP metrics exporter: {e}"))
}

/// Install telemetry for this process. Safe to call more than once and in
/// a test binary that links several tests: only the first call installs a
/// global subscriber, and a later one returns a guard that exports nothing
/// it did not itself set up.
pub fn init(config: Config) -> Guard {
    init_with(config, Settings::from_env())
}

/// [`init`] with `settings` instead of the environment's, for tests.
pub fn init_with(config: Config, settings: Settings) -> Guard {
    // The rustls provider the HTTPS exporter needs, installed once.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        branchyard_support::best_effort(
            "install the default TLS crypto provider",
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "a default crypto provider is already installed"),
        );
    }
    let resource = resource(&config);
    let wants_grpc =
        settings.traces == Some(Protocol::Grpc) || settings.metrics == Some(Protocol::Grpc);
    let runtime = if wants_grpc {
        branchyard_support::best_effort(
            "start the runtime of the gRPC exporters",
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("branchyard-otlp")
                .enable_all()
                .build(),
        )
    } else {
        None
    };
    let mut notes: Vec<String> = settings.warnings.clone();

    let tracer =
        settings
            .traces
            .and_then(|protocol| match span_exporter(protocol, runtime.as_ref()) {
                Ok(exporter) => {
                    let provider = SdkTracerProvider::builder()
                        .with_resource(resource.clone())
                        .with_sampler(sampler(settings.sampler))
                        .with_batch_exporter(exporter)
                        .build();
                    Some((provider, protocol))
                }
                Err(e) => {
                    notes.push(format!("traces are off: {e}"));
                    None
                }
            });

    // The metrics provider always has the Prometheus reader (a server
    // answers `/metrics` from it) and a periodic OTLP reader when pushed.
    let reader = prometheus::PromReader::new();
    let mut meter_builder = SdkMeterProvider::builder()
        .with_resource(resource.clone())
        .with_reader(reader.clone());
    let mut pushing_metrics = None;
    if let Some(protocol) = settings.metrics {
        match metric_exporter(protocol, runtime.as_ref()) {
            Ok(exporter) => {
                meter_builder = meter_builder.with_periodic_exporter(exporter);
                pushing_metrics = Some(protocol);
            }
            Err(e) => notes.push(format!("metrics are not pushed: {e}")),
        }
    }
    let meter_provider = meter_builder.build();
    let meters = Meters {
        provider: meter_provider.clone(),
        reader,
    };

    let otel_layer = tracer.as_ref().map(|(provider, _)| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("branchyard"))
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_tracked_inactivity(false)
            .with_filter(tracing_subscriber::filter::filter_fn(bridged))
    });
    // An empty `Vec` of layers would answer "never" to every callsite;
    // `None` stays out of the way.
    let logs = (!config.layers.is_empty()).then_some(config.layers);
    let subscriber = Registry::default().with(logs).with(otel_layer);
    let installed = tracing::subscriber::set_global_default(subscriber).is_ok();

    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let exporting_spans = installed && tracer.is_some();
    if exporting_spans {
        if let Some((provider, _)) = &tracer {
            opentelemetry::global::set_tracer_provider(provider.clone());
        }
        set_spans_enabled(true);
    }
    if PROCESS_METERS.set(meters.clone()).is_ok() {
        opentelemetry::global::set_meter_provider(meter_provider.clone());
    }
    for note in &notes {
        tracing::warn!("telemetry: {note}");
    }
    if let (true, Some((_, protocol))) = (exporting_spans, &tracer) {
        tracing::info!(protocol = protocol.name(), "exporting traces over OTLP");
    }
    if let Some(protocol) = pushing_metrics {
        tracing::info!(protocol = protocol.name(), "exporting metrics over OTLP");
    }
    Guard {
        tracer: if exporting_spans {
            tracer.map(|(provider, _)| provider)
        } else {
            None
        },
        meters: pushing_metrics.map(|_| meter_provider),
        runtime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{
        Span as _, SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
        Tracer as _,
    };
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    /// How many of two spans (a root, and a child of a remote parent with
    /// `parent_sampled`) a provider sampling with `choice` exports.
    fn exported(choice: SamplerChoice, parent: Option<bool>) -> usize {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(sampler(choice))
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let context = match parent {
            None => opentelemetry::Context::new(),
            Some(sampled) => {
                opentelemetry::Context::new().with_remote_span_context(SpanContext::new(
                    TraceId::from_bytes([1; 16]),
                    SpanId::from_bytes([2; 8]),
                    if sampled {
                        TraceFlags::SAMPLED
                    } else {
                        TraceFlags::default()
                    },
                    true,
                    TraceState::default(),
                ))
            }
        };
        let mut span = tracer.build_with_context(tracer.span_builder("s"), &context);
        span.end();
        exporter.get_finished_spans().map_or(0, |spans| spans.len())
    }

    #[test]
    fn samplers_decide_as_the_environment_says() {
        use SamplerChoice::*;
        // The default follows its parent and samples roots.
        assert_eq!(exported(ParentBasedAlwaysOn, None), 1);
        assert_eq!(exported(ParentBasedAlwaysOn, Some(true)), 1);
        assert_eq!(exported(ParentBasedAlwaysOn, Some(false)), 0);
        // A ratio applies to roots only; a sampled parent keeps its trace.
        assert_eq!(exported(ParentBasedRatio(0.0), None), 0);
        assert_eq!(exported(ParentBasedRatio(1.0), None), 1);
        assert_eq!(exported(ParentBasedRatio(0.0), Some(true)), 1);
        assert_eq!(exported(ParentBasedAlwaysOff, None), 0);
        assert_eq!(exported(ParentBasedAlwaysOff, Some(true)), 1);
        // Unconditional samplers ignore the parent.
        assert_eq!(exported(AlwaysOff, Some(true)), 0);
        assert_eq!(exported(AlwaysOn, Some(false)), 1);
    }

    #[test]
    fn isolated_meters_are_apart_from_each_other() {
        let (a, b) = (Meters::isolated(), Meters::isolated());
        a.meter("t")
            .u64_counter("only_in_a_total")
            .build()
            .add(1, &[]);
        assert!(a
            .reader()
            .render(&[])
            .is_ok_and(|t| t.contains("only_in_a_total 1")));
        assert!(b
            .reader()
            .render(&[])
            .is_ok_and(|t| !t.contains("only_in_a_total")));
    }
}
