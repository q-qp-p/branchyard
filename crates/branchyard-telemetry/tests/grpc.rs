//! Spans reach an OTLP/gRPC collector when `OTEL_EXPORTER_OTLP_PROTOCOL` is
//! `grpc`, from a process with no async runtime of its own (as `by` is).
//! The collector is a tonic stand-in on loopback.
//!
//! One test, because `init` installs process-wide providers and the
//! variables it reads are the process's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report

use std::sync::{Arc, Mutex};

use branchyard_telemetry::{init, Config, Role, TRACE_TARGET};
use branchyard_testkit::wait;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use tonic::{Request, Response, Status};

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<ExportTraceServiceRequest>>>);

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.0.lock().unwrap().push(request.into_inner());
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[test]
fn spans_reach_a_collector_over_otlp_grpc() {
    let collector = Collector::default();
    // gRPC needs HTTP/2, which `MockHttp` (HTTP/1.1) does not speak, so the
    // stand-in is a tonic server on the testkit's held loopback listener.
    let listener = branchyard_testkit::loopback_listener();
    let port = listener.local_addr().unwrap().port();
    let served = collector.clone();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            // Serves until the process ends; an error is the test's to
            // notice, as a collector that received nothing.
            branchyard_support::best_effort(
                "serve the stand-in collector",
                tonic::transport::Server::builder()
                    .add_service(TraceServiceServer::new(served))
                    .serve_with_incoming(incoming)
                    .await,
            );
        });
    });
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        format!("http://127.0.0.1:{port}"),
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc");
    std::env::set_var("OTEL_METRICS_EXPORTER", "none");
    std::env::set_var("OTEL_SERVICE_NAME", "by-grpc");
    let guard = init(Config {
        role: Role::Cli,
        service_name: "by".into(),
        service_version: "1".into(),
        layers: Vec::new(),
    });
    assert!(guard.exporting_traces(), "{guard:?}");
    assert!(!guard.exporting_metrics());
    {
        let span = tracing::info_span!(target: TRACE_TARGET, "admission", "by.operation" = "op_g");
        let _in = span.enter();
    }
    guard.flush();
    wait::until("the collector to receive the span over gRPC", || {
        !collector.0.lock().unwrap().is_empty()
    });
    let received = collector.0.lock().unwrap().clone();
    let spans: Vec<_> = received
        .iter()
        .flat_map(|r| &r.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect();
    assert!(spans.iter().any(|s| s.name == "admission"), "{received:?}");
    drop(guard);
    // The stand-in runs until the process ends; nothing joins it.
    drop(server);
}
