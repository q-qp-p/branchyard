//! What the standard `OTEL_*` variables ask for, decided once and testable
//! without touching the process environment.
//!
//! The OTLP exporters read their own transport variables (the endpoint, its
//! per-signal forms, headers, timeouts, compression) themselves; this module
//! reads the ones that decide *whether* a signal is exported and how it is
//! sampled: `OTEL_SDK_DISABLED`, `OTEL_TRACES_EXPORTER`,
//! `OTEL_METRICS_EXPORTER`, `OTEL_EXPORTER_OTLP_PROTOCOL` (and the per-signal
//! forms), `OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG`.

/// How a process uses telemetry: it names the `by.role` resource attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `by serve` and `branchyard-server`.
    Server,
    /// `by worker`.
    Worker,
    /// Any other `by` command, run locally.
    Cli,
}

impl Role {
    /// The role's value in `by.role`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Worker => "worker",
            Role::Cli => "cli",
        }
    }
}

/// The OTLP transport a signal is exported over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// OTLP/HTTP with protobuf bodies: the specification's default.
    HttpProtobuf,
    /// OTLP/HTTP with JSON bodies.
    HttpJson,
    /// OTLP over gRPC.
    Grpc,
}

impl Protocol {
    /// The value of `OTEL_EXPORTER_OTLP_PROTOCOL` that names it.
    pub fn name(self) -> &'static str {
        match self {
            Protocol::HttpProtobuf => "http/protobuf",
            Protocol::HttpJson => "http/json",
            Protocol::Grpc => "grpc",
        }
    }
}

/// A head sampler, from `OTEL_TRACES_SAMPLER`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SamplerChoice {
    /// `always_on`.
    AlwaysOn,
    /// `always_off`.
    AlwaysOff,
    /// `traceidratio` with its ratio.
    Ratio(f64),
    /// `parentbased_always_on`: the default.
    ParentBasedAlwaysOn,
    /// `parentbased_always_off`.
    ParentBasedAlwaysOff,
    /// `parentbased_traceidratio` with its ratio.
    ParentBasedRatio(f64),
}

/// What the environment asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// `OTEL_SDK_DISABLED=true`: nothing is exported or sampled.
    pub disabled: bool,
    /// The transport spans are pushed over; `None` when they are not.
    pub traces: Option<Protocol>,
    /// The transport metrics are pushed over; `None` when they are not.
    pub metrics: Option<Protocol>,
    /// How root spans are sampled.
    pub sampler: SamplerChoice,
    /// A setting that could not be honoured, for the log.
    pub warnings: Vec<String>,
}

impl Settings {
    /// From the process environment.
    pub fn from_env() -> Settings {
        Settings::from_vars(|name| std::env::var(name).ok())
    }

    /// From `var`, which returns a variable's value if it is set.
    ///
    /// A signal is pushed when its exporter is not `none` and an endpoint
    /// is configured (`OTEL_EXPORTER_OTLP_ENDPOINT`, or the signal's own),
    /// or when its exporter variable names `otlp`. Unlike the
    /// specification's default, nothing is pushed to `localhost:4318`
    /// unasked: a local `by` command stays silent unless an endpoint is set.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Settings {
        let get = |name: &str| {
            var(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let mut warnings = Vec::new();
        let disabled = get("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let signal =
            |exporter: &str, endpoint: &str, protocol: &str, warnings: &mut Vec<String>| {
                if disabled {
                    return None;
                }
                let named = get(exporter);
                let mut otlp = false;
                let mut other = Vec::new();
                for item in named.iter().flat_map(|v| v.split(',')) {
                    match item.trim().to_ascii_lowercase().as_str() {
                        "otlp" => otlp = true,
                        "none" | "" => {}
                        // The Prometheus reader is always on for a server; it is
                        // a pull endpoint, not a push.
                        "prometheus" => {}
                        unknown => other.push(unknown.to_owned()),
                    }
                }
                if !other.is_empty() {
                    warnings.push(format!(
                        "{exporter} names {}, which is not built in; only otlp is",
                        other.join(", ")
                    ));
                }
                let none = named
                    .as_deref()
                    .is_some_and(|v| v.split(',').all(|i| i.trim().eq_ignore_ascii_case("none")));
                let configured =
                    get("OTEL_EXPORTER_OTLP_ENDPOINT").is_some() || get(endpoint).is_some();
                if none || !(otlp || (named.is_none() && configured)) {
                    return None;
                }
                let chosen = get(protocol).or_else(|| get("OTEL_EXPORTER_OTLP_PROTOCOL"));
                Some(
                    match chosen.as_deref().map(str::to_ascii_lowercase).as_deref() {
                        None | Some("http/protobuf") => Protocol::HttpProtobuf,
                        Some("http/json") => Protocol::HttpJson,
                        Some("grpc") => Protocol::Grpc,
                        Some(other) => {
                            warnings.push(format!(
                                "unknown OTLP protocol {other:?}; using http/protobuf"
                            ));
                            Protocol::HttpProtobuf
                        }
                    },
                )
            };
        let traces = signal(
            "OTEL_TRACES_EXPORTER",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            &mut warnings,
        );
        let metrics = signal(
            "OTEL_METRICS_EXPORTER",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            &mut warnings,
        );
        let ratio = |warnings: &mut Vec<String>| match get("OTEL_TRACES_SAMPLER_ARG") {
            None => 1.0,
            Some(arg) => match arg.parse::<f64>() {
                Ok(r) if r.is_finite() => r.clamp(0.0, 1.0),
                _ => {
                    warnings.push(format!(
                        "OTEL_TRACES_SAMPLER_ARG {arg:?} is not a number; sampling every trace"
                    ));
                    1.0
                }
            },
        };
        let sampler = match get("OTEL_TRACES_SAMPLER")
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("parentbased_always_on") => SamplerChoice::ParentBasedAlwaysOn,
            Some("always_on") => SamplerChoice::AlwaysOn,
            Some("always_off") => SamplerChoice::AlwaysOff,
            Some("traceidratio") => SamplerChoice::Ratio(ratio(&mut warnings)),
            Some("parentbased_always_off") => SamplerChoice::ParentBasedAlwaysOff,
            Some("parentbased_traceidratio") => {
                SamplerChoice::ParentBasedRatio(ratio(&mut warnings))
            }
            Some(other) => {
                warnings.push(format!(
                    "OTEL_TRACES_SAMPLER {other:?} is not supported; using parentbased_always_on"
                ));
                SamplerChoice::ParentBasedAlwaysOn
            }
        };
        Settings {
            disabled,
            traces,
            metrics,
            sampler,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_vars(|name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    #[test]
    fn nothing_is_pushed_without_an_endpoint() {
        let s = settings(&[]);
        assert_eq!((s.traces, s.metrics, s.disabled), (None, None, false));
        assert_eq!(s.sampler, SamplerChoice::ParentBasedAlwaysOn);
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn an_endpoint_turns_on_both_signals_over_http_protobuf() {
        let s = settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")]);
        assert_eq!(s.traces, Some(Protocol::HttpProtobuf));
        assert_eq!(s.metrics, Some(Protocol::HttpProtobuf));
    }

    #[test]
    fn a_signals_own_endpoint_turns_on_only_that_signal() {
        let s = settings(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://c:4318/v1/traces",
        )]);
        assert_eq!((s.traces.is_some(), s.metrics.is_some()), (true, false));
    }

    #[test]
    fn none_and_the_kill_switch_win_over_an_endpoint() {
        let on = [("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")];
        let s = settings(&[on[0], ("OTEL_TRACES_EXPORTER", "none")]);
        assert_eq!((s.traces, s.metrics.is_some()), (None, true));
        let s = settings(&[on[0], ("OTEL_METRICS_EXPORTER", "none")]);
        assert_eq!((s.traces.is_some(), s.metrics), (true, None));
        let s = settings(&[on[0], ("OTEL_SDK_DISABLED", "TRUE")]);
        assert_eq!((s.traces, s.metrics, s.disabled), (None, None, true));
    }

    #[test]
    fn the_protocol_comes_from_the_signal_then_the_general_variable() {
        let s = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "http/json"),
        ]);
        assert_eq!(s.traces, Some(Protocol::Grpc));
        assert_eq!(s.metrics, Some(Protocol::HttpJson));
        let s = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "carrier-pigeon"),
        ]);
        assert_eq!(s.traces, Some(Protocol::HttpProtobuf));
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
    }

    #[test]
    fn exporters_that_are_not_built_in_are_named_and_not_exported_to() {
        let s = settings(&[("OTEL_TRACES_EXPORTER", "jaeger")]);
        assert_eq!(s.traces, None);
        assert!(s.warnings[0].contains("jaeger"), "{:?}", s.warnings);
        let s = settings(&[("OTEL_METRICS_EXPORTER", "prometheus")]);
        assert_eq!(s.metrics, None);
        assert!(s.warnings.is_empty());
        let s = settings(&[("OTEL_TRACES_EXPORTER", "otlp")]);
        assert_eq!(s.traces, Some(Protocol::HttpProtobuf));
    }

    #[test]
    fn samplers_parse_with_their_ratio() {
        let ratio = |name: &str, arg: Option<&str>| {
            let mut pairs = vec![("OTEL_TRACES_SAMPLER", name)];
            pairs.extend(arg.map(|a| ("OTEL_TRACES_SAMPLER_ARG", a)));
            settings(&pairs)
        };
        assert_eq!(
            ratio("parentbased_traceidratio", Some("0.25")).sampler,
            SamplerChoice::ParentBasedRatio(0.25)
        );
        assert_eq!(
            ratio("traceidratio", Some("7")).sampler,
            SamplerChoice::Ratio(1.0)
        );
        assert_eq!(ratio("always_off", None).sampler, SamplerChoice::AlwaysOff);
        let bad = ratio("traceidratio", Some("lots"));
        assert_eq!(bad.sampler, SamplerChoice::Ratio(1.0));
        assert!(bad.warnings[0].contains("lots"));
        let unknown = ratio("jaeger_remote", None);
        assert_eq!(unknown.sampler, SamplerChoice::ParentBasedAlwaysOn);
        assert!(!unknown.warnings.is_empty());
    }
}
