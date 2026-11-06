//! A pull reader for the OpenTelemetry metrics SDK that renders Prometheus
//! text exposition 0.0.4, so `GET /metrics` and an OTLP push read the same
//! instruments.
//!
//! This is a [`MetricReader`] over the SDK's own manual reader plus a
//! renderer of the [`ResourceMetrics`] it collects; no Prometheus client
//! crate is involved. (`opentelemetry-prometheus` would add `prometheus` and
//! its dependencies and is no longer where the OpenTelemetry project points
//! people; the custom-reader hook it would use is the one used here.)
//!
//! **Names.** The OpenTelemetry-to-Prometheus translation is the usual one:
//! `.` and any other character a name cannot hold become `_`; a unit adds a
//! suffix (`s` is `_seconds`, `By` is `_bytes`, a unit in braces such as
//! `{token}` adds nothing) unless the name already ends with it; a
//! monotonic sum ends in `_total`, once. Branchyard's own instruments
//! are named the way they are exposed (`branchyard_start_seconds`), so
//! translation leaves them as they are, and an OTLP backend that applies
//! the same rule agrees with `/metrics`.
//!
//! **Labels** are the attributes, sorted by name. The instrumentation
//! scope and resource are not written as labels.
//!
//! **Declared families.** The SDK reports an instrument only once it has a
//! data point; a [`Declared`] list makes the renderer write a family's
//! `HELP` and `TYPE` before it has any, in a fixed order, as `/metrics`
//! always has.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Weak};
use std::time::Duration;

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{InstrumentKind, ManualReader, Pipeline, Temporality};

/// The Prometheus type of a family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A monotonic sum.
    Counter,
    /// A gauge, or a sum that can fall.
    Gauge,
    /// A histogram.
    Histogram,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// A family that is written even before it has samples.
#[derive(Clone, Debug)]
pub struct Declared {
    /// The Prometheus name (see [`prometheus_name`]).
    pub name: String,
    /// Its type.
    pub kind: Kind,
    /// Its `HELP` text.
    pub help: String,
    /// The order its labels are written in (as exposed, so `.` already
    /// `_`); a label not listed follows, alphabetically. The SDK keeps a
    /// series' attributes sorted and loses the order they were recorded
    /// in, and a dashboard that matches a series by its text should not
    /// have its labels move.
    pub labels: Vec<String>,
}

/// The reader behind a `/metrics` endpoint: cumulative, collected on
/// demand, shared with the meter provider that owns the instruments.
#[derive(Clone, Debug)]
pub struct PromReader(Arc<ManualReader>);

impl PromReader {
    /// A reader that has not been given to a provider yet.
    pub fn new() -> PromReader {
        PromReader(Arc::new(
            ManualReader::builder()
                .with_temporality(Temporality::Cumulative)
                .build(),
        ))
    }

    /// Collect now, running every observable callback.
    pub fn collect(&self) -> Result<ResourceMetrics, String> {
        let mut rm = ResourceMetrics::default();
        self.0
            .collect(&mut rm)
            .map_err(|e| format!("could not collect metrics: {e}"))?;
        Ok(rm)
    }

    /// Collect now and render every [`Declared`] family, then any other
    /// instrument.
    pub fn render(&self, declared: &[Declared]) -> Result<String, String> {
        Ok(render(&self.collect()?, declared))
    }
}

impl Default for PromReader {
    fn default() -> PromReader {
        PromReader::new()
    }
}

impl MetricReader for PromReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.register_pipeline(pipeline);
    }

    fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.collect(rm)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

/// `name` with every character Prometheus does not allow in a metric or
/// label name replaced by `_`, and a leading digit guarded.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn unit_suffix(unit: &str) -> Option<String> {
    // A unit in braces is an annotation (`{token}`), not a dimension.
    let unit = unit.trim();
    if unit.is_empty() || unit == "1" || (unit.starts_with('{') && unit.ends_with('}')) {
        return None;
    }
    Some(match unit {
        "s" => "seconds".into(),
        "ms" => "milliseconds".into(),
        "us" => "microseconds".into(),
        "ns" => "nanoseconds".into(),
        "By" => "bytes".into(),
        "%" => "percent".into(),
        other => sanitize(&other.to_ascii_lowercase()),
    })
}

/// The Prometheus name of an instrument `name` with `unit`, which is a
/// monotonic sum when `counter`.
pub fn prometheus_name(name: &str, unit: &str, counter: bool) -> String {
    let mut base = sanitize(name);
    if counter {
        if let Some(stripped) = base.strip_suffix("_total") {
            base = stripped.to_owned();
        }
    }
    if let Some(suffix) = unit_suffix(unit) {
        if !base.ends_with(&format!("_{suffix}")) {
            base.push('_');
            base.push_str(&suffix);
        }
    }
    if counter {
        base.push_str("_total");
    }
    base
}

/// A label value escaped for the exposition format: backslash, double
/// quote and line feed.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Help text escaped: backslash and line feed.
fn escape_help(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

/// A sample value: integers without a fraction, `+Inf`, `-Inf`, `NaN`.
pub fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Label names and values, sorted by name.
pub type Labels = Vec<(String, String)>;

/// One series' value.
#[derive(Clone, Debug, PartialEq)]
pub enum Sample {
    /// A counter's or gauge's value.
    Number(f64),
    /// A histogram's buckets, sum and count.
    Histogram {
        /// The bucket upper bounds, ascending.
        bounds: Vec<f64>,
        /// Cumulative counts, one per bound.
        cumulative: Vec<u64>,
        /// The sum of every observation.
        sum: f64,
        /// The number of observations.
        count: u64,
    },
}

/// A collected family: its type, help and series by labels.
#[derive(Clone, Debug)]
pub struct Collected {
    /// Its type.
    pub kind: Kind,
    /// Its description.
    pub help: String,
    /// Its series.
    pub series: BTreeMap<Labels, Sample>,
}

fn labels_of<'a>(attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Labels {
    let mut labels: Labels = attributes
        .map(|kv| (sanitize(kv.key.as_str()), kv.value.as_str().into_owned()))
        .collect();
    labels.sort();
    labels
}

/// The families in `rm` by Prometheus name.
pub fn collect_families(rm: &ResourceMetrics) -> BTreeMap<String, Collected> {
    let mut families: BTreeMap<String, Collected> = BTreeMap::new();
    for scope in rm.scope_metrics() {
        for metric in scope.metrics() {
            macro_rules! numbers {
                ($data:expr, $to:expr) => {
                    match $data {
                        MetricData::Gauge(g) => {
                            let name = prometheus_name(metric.name(), metric.unit(), false);
                            let entry = families.entry(name).or_insert_with(|| Collected {
                                kind: Kind::Gauge,
                                help: metric.description().to_owned(),
                                series: BTreeMap::new(),
                            });
                            for p in g.data_points() {
                                entry.series.insert(
                                    labels_of(p.attributes()),
                                    Sample::Number($to(p.value())),
                                );
                            }
                        }
                        MetricData::Sum(s) => {
                            let counter = s.is_monotonic();
                            let name = prometheus_name(metric.name(), metric.unit(), counter);
                            let entry = families.entry(name).or_insert_with(|| Collected {
                                kind: if counter { Kind::Counter } else { Kind::Gauge },
                                help: metric.description().to_owned(),
                                series: BTreeMap::new(),
                            });
                            for p in s.data_points() {
                                entry.series.insert(
                                    labels_of(p.attributes()),
                                    Sample::Number($to(p.value())),
                                );
                            }
                        }
                        MetricData::Histogram(h) => {
                            let name = prometheus_name(metric.name(), metric.unit(), false);
                            let entry = families.entry(name).or_insert_with(|| Collected {
                                kind: Kind::Histogram,
                                help: metric.description().to_owned(),
                                series: BTreeMap::new(),
                            });
                            for p in h.data_points() {
                                let bounds: Vec<f64> = p.bounds().collect();
                                let mut running = 0u64;
                                let cumulative = p
                                    .bucket_counts()
                                    .take(bounds.len())
                                    .map(|n| {
                                        running += n;
                                        running
                                    })
                                    .collect();
                                entry.series.insert(
                                    labels_of(p.attributes()),
                                    Sample::Histogram {
                                        bounds,
                                        cumulative,
                                        sum: $to(p.sum()),
                                        count: p.count(),
                                    },
                                );
                            }
                        }
                        // Not produced by an instrument built here; skipped.
                        MetricData::ExponentialHistogram(_) => {}
                    }
                };
            }
            match metric.data() {
                AggregatedMetrics::F64(data) => numbers!(data, |v: f64| v),
                AggregatedMetrics::U64(data) => numbers!(data, |v: u64| v as f64),
                AggregatedMetrics::I64(data) => numbers!(data, |v: i64| v as f64),
            }
        }
    }
    families
}

fn label_set(labels: &[(String, String)], extra: Option<(&str, &str)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{}\"", escape_label(v)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

/// `rm` in the text exposition format: every [`Declared`] family in order
/// with its `HELP` and `TYPE`, written even with no series, then any other
/// collected family by name. Series are sorted by labels.
pub fn render(rm: &ResourceMetrics, declared: &[Declared]) -> String {
    let mut collected = collect_families(rm);
    let mut out = String::new();
    let mut write_family =
        |name: &str, kind: Kind, help: &str, order: &[String], family: Option<Collected>| {
            let mut line = |text: std::fmt::Arguments<'_>| {
                branchyard_support::best_effort("render a metrics line", writeln!(out, "{text}"));
            };
            line(format_args!("# HELP {name} {}", escape_help(help)));
            line(format_args!("# TYPE {name} {}", kind.name()));
            let Some(family) = family else { return };
            for (labels, sample) in &family.series {
                // Declared labels first, in their order; a stable sort leaves
                // the rest alphabetical.
                let mut ordered = labels.clone();
                ordered.sort_by_key(|(key, _)| {
                    order.iter().position(|o| o == key).unwrap_or(usize::MAX)
                });
                let labels = &ordered;
                match sample {
                    Sample::Number(n) => {
                        line(format_args!(
                            "{name}{} {}",
                            label_set(labels, None),
                            number(*n)
                        ));
                    }
                    Sample::Histogram {
                        bounds,
                        cumulative,
                        sum,
                        count,
                    } => {
                        for (bound, n) in bounds.iter().zip(cumulative) {
                            line(format_args!(
                                "{name}_bucket{} {n}",
                                label_set(labels, Some(("le", &number(*bound))))
                            ));
                        }
                        line(format_args!(
                            "{name}_bucket{} {count}",
                            label_set(labels, Some(("le", "+Inf")))
                        ));
                        line(format_args!(
                            "{name}_sum{} {}",
                            label_set(labels, None),
                            number(*sum)
                        ));
                        line(format_args!(
                            "{name}_count{} {count}",
                            label_set(labels, None)
                        ));
                    }
                }
            }
        };
    for family in declared {
        let found = collected.remove(&family.name);
        write_family(
            &family.name,
            family.kind,
            &family.help,
            &family.labels,
            found,
        );
    }
    for (name, family) in std::mem::take(&mut collected) {
        let (kind, help) = (family.kind, family.help.clone());
        write_family(&name, kind, &help, &[], Some(family));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::SdkMeterProvider;

    fn provider() -> (SdkMeterProvider, PromReader) {
        let reader = PromReader::new();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        (provider, reader)
    }

    #[test]
    fn names_translate_as_prometheus_and_otlp_backends_do() {
        assert_eq!(
            prometheus_name("http.server.request.duration", "s", false),
            "http_server_request_duration_seconds"
        );
        assert_eq!(
            prometheus_name("gen_ai.client.token.usage", "{token}", false),
            "gen_ai_client_token_usage"
        );
        assert_eq!(
            prometheus_name("branchyard_start_seconds", "s", false),
            "branchyard_start_seconds"
        );
        assert_eq!(
            prometheus_name("branchyard_claims_total", "", true),
            "branchyard_claims_total"
        );
        assert_eq!(prometheus_name("jobs.done", "1", true), "jobs_done_total");
        assert_eq!(
            prometheus_name("sync.sent", "By", true),
            "sync_sent_bytes_total"
        );
        assert_eq!(
            prometheus_name("branchyard_cost_usd_total", "USD", true),
            "branchyard_cost_usd_total"
        );
        assert_eq!(sanitize("9 lives-x"), "_9_lives_x");
    }

    #[test]
    fn counters_gauges_and_histograms_render_with_cumulative_buckets() {
        let (provider, reader) = provider();
        let meter = provider.meter("test");
        let counter = meter.f64_counter("jobs_total").build();
        counter.add(
            2.0,
            &[KeyValue::new("kind", "a\"b"), KeyValue::new("a.b", "x")],
        );
        counter.add(
            1.0,
            &[KeyValue::new("kind", "a\"b"), KeyValue::new("a.b", "x")],
        );
        let histogram = meter
            .f64_histogram("wait_seconds")
            .with_unit("s")
            .with_boundaries(vec![1.0, 5.0])
            .build();
        for v in [0.5, 2.0, 2.0, 9.0] {
            histogram.record(v, &[]);
        }
        let depth = meter.f64_up_down_counter("depth").build();
        depth.add(3.0, &[]);
        let declared = [
            Declared {
                name: "never_seen".into(),
                kind: Kind::Gauge,
                help: "A family with no data.\nTwo lines.".into(),
                labels: vec![],
            },
            Declared {
                name: "jobs_total".into(),
                kind: Kind::Counter,
                help: "Jobs.".into(),
                // Written in this order, not alphabetically.
                labels: vec!["kind".into(), "a_b".into()],
            },
        ];
        let text = reader.render(&declared).expect("collected");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "# HELP never_seen A family with no data.\\nTwo lines."
        );
        assert_eq!(lines[1], "# TYPE never_seen gauge");
        assert_eq!(lines[2], "# HELP jobs_total Jobs.");
        assert!(
            text.contains("jobs_total{kind=\"a\\\"b\",a_b=\"x\"} 3\n"),
            "{text}"
        );
        assert!(text.contains("# TYPE wait_seconds histogram"), "{text}");
        assert!(text.contains("wait_seconds_bucket{le=\"1\"} 1\n"), "{text}");
        assert!(text.contains("wait_seconds_bucket{le=\"5\"} 3\n"), "{text}");
        assert!(
            text.contains("wait_seconds_bucket{le=\"+Inf\"} 4\n"),
            "{text}"
        );
        assert!(text.contains("wait_seconds_sum 13.5\n"), "{text}");
        assert!(text.contains("wait_seconds_count 4\n"), "{text}");
        assert!(text.contains("# TYPE depth gauge\ndepth 3\n"), "{text}");
    }

    #[test]
    fn observable_gauges_run_their_callback_on_each_collection() {
        let (provider, reader) = provider();
        let meter = provider.meter("test");
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen = calls.clone();
        let _gauge = meter
            .f64_observable_gauge("level")
            .with_callback(move |observer| {
                let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                observer.observe(n as f64, &[KeyValue::new("pool", "p")]);
            })
            .build();
        assert!(reader
            .render(&[])
            .expect("first")
            .contains("level{pool=\"p\"} 1\n"));
        assert!(reader
            .render(&[])
            .expect("second")
            .contains("level{pool=\"p\"} 2\n"));
    }

    #[test]
    fn a_shut_down_provider_reports_an_error_not_stale_text() {
        let (provider, reader) = provider();
        provider.shutdown().expect("shutdown");
        assert!(reader.render(&[]).is_err());
    }

    #[test]
    fn number_formats_like_the_exposition_format() {
        assert_eq!(number(3.0), "3");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(f64::INFINITY), "+Inf");
        assert_eq!(number(f64::NAN), "NaN");
    }
}
