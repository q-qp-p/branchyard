# Observability

What a Branchyard server, a worker and a local `by` command tell you about themselves: OpenTelemetry traces of each operation from admission to its turns, OpenTelemetry metrics served at `GET /metrics` in Prometheus text and pushed over OTLP, a `traceparent` carried to harnesses, the connector gateway and webhook receivers, and `by stats` for a quick summary from the command line. The scheduling these describe (priority, fair share, aging) is in [the server reference](server.md#scheduling).

> **Status, 7 November 2026.** Tested hermetically: spans and metrics from the `tracing` and OpenTelemetry crates against in-memory exporters and against stand-in collectors on loopback (OTLP over HTTP with protobuf bodies, and over gRPC), the exposition format against an independent parser, the counters over real HTTP with the fake ACP agent, and a local `by run` exporting to a loopback collector. Phase 2 (this page's [live turn spans](#traces), the engine's own spans and `by trace`) is hermetic too: a turn with the fake ACP agent, with spans captured in memory, traced live with its tool, permission, steer, stall, delegation and message spans, once and not twice over real HTTP, and not at all with telemetry off; a sandbox provider's actions through `FakeProvider`; git operations on real repositories; `by trace` against the built `by` and a loopback collector. Not yet exported to a real collector or scraped by a real Prometheus, no dashboard is provided, and no model was called. Not done: a real collector, Jaeger, Tempo or Prometheus (the OTLP/JSON `by trace` writes is the encoding the specification gives and is read back in tests, not loaded into either); dashboards; OTLP logs; live spans for a harness's own work (a tool call's duration is still the time to the branch's next event, and a harness inside a sandbox is traced only by the `TRACEPARENT` it is given); spans for the Substrate provider's actions (its copy-in and copy-out path holds the concrete provider, not the traced one); a live span for a connector call ingested after its turn ended (the after-the-fact pass skips a turn it saw traced live, so such a call has no span); `by trace --remote`; and a sampled or aggregated form of the store spans (they are all or none).

## The stack

Branchyard uses the standard Rust stack, one crate for all of it:

| Piece | Crate | Version |
|---|---|---|
| Spans and logs in the code | [`tracing`](https://docs.rs/tracing) with [`tracing-subscriber`](https://docs.rs/tracing-subscriber) | the workspace's |
| The OpenTelemetry API and SDK | `opentelemetry`, `opentelemetry_sdk` | 0.33.1 |
| The bridge from `tracing` spans to OpenTelemetry spans | `tracing-opentelemetry` | 0.34.0 |
| OTLP export (HTTP with protobuf or JSON bodies, and gRPC) | `opentelemetry-otlp` | 0.33.1 |

[`branchyard-telemetry`](../crates/branchyard-telemetry/src/lib.rs) owns the setup for every binary: `init` installs a `tracing` subscriber with the binary's own log layer plus the OpenTelemetry layer, a tracer provider and a meter provider configured from the standard `OTEL_*` variables, and the W3C trace-context propagator, and returns a guard that flushes and shuts the exporters down when it is dropped. The server (`by serve`, `branchyard-server`), the worker (`by worker`) and every other `by` command call it, with the resource attribute `by.role` set to `server`, `worker` or `cli`. The engine crate depends on none of it: it emits `tracing` events and spans, through [`branchyard_support::trace`](../crates/branchyard-support/src/trace.rs) (`trace_span!`, which builds nothing while no exporter is installed, and three hooks the telemetry crate fills in: adopt a `traceparent`, read a span's, write a finished span), and the binary that installs the SDK decides where they go.

**Off unless asked.** With no OTLP endpoint in the environment nothing is exported, no exporter thread is started and no span is bridged; an incoming `traceparent` is passed on unchanged, so a trace started upstream is not lost. Branchyard's own `branchyard_*` metrics are always recorded in process, because `GET /metrics` serves them.

**Prometheus without a Prometheus crate.** The meter provider has a pull reader, [`PromReader`](../crates/branchyard-telemetry/src/prometheus.rs), over the SDK's manual reader (the `experimental_metrics_custom_reader` feature), and a renderer of what it collects as Prometheus text exposition 0.0.4. `opentelemetry-prometheus` would add the `prometheus` crate and its dependencies and is no longer where the OpenTelemetry project points people; the same instruments are pushed over OTLP when a metrics endpoint is set, so `/metrics` and a collector read one set of numbers. The translation of an instrument's name is the usual one: `.` becomes `_`, a unit adds a suffix (`s` is `_seconds`) unless the name has it, a monotonic sum ends in `_total` once. Branchyard's own families are named the way they are exposed, so nothing about them moved when the hand-written encoder went.

**Dependencies.** The change adds 9 third-party packages that are built (`opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `opentelemetry-proto`, `opentelemetry-http`, `tracing-opentelemetry`, `tonic-types`, `const-hex` and the crate itself) and three that only the lock names (`proptest` and two helpers, an optional dependency of `opentelemetry-proto`). No second TLS stack: `reqwest` and `tonic` were already in the tree, on `rustls` and `ring`. `cargo deny check` passes with no new exception.

## Configuration

Branchyard reads the standard OpenTelemetry variables; the OTLP exporters read their own transport variables.

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The collector's base URL. Setting it (or a signal's own endpoint) turns that signal's push on: spans go to `<it>/v1/traces`, metrics to `<it>/v1/metrics` over HTTP, or to the endpoint itself over gRPC. Unlike the specification's default, nothing is sent to `localhost:4318` unasked |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | A signal's full URL, used as is; wins over the above |
| `OTEL_EXPORTER_OTLP_PROTOCOL` (and `..._TRACES_PROTOCOL`, `..._METRICS_PROTOCOL`) | `http/protobuf` (the default), `http/json` or `grpc` |
| `OTEL_EXPORTER_OTLP_HEADERS` (and the per-signal forms) | `key=value,key2=value2`, percent-decoded, sent with each export (an API key, a tenant header) |
| `OTEL_EXPORTER_OTLP_TIMEOUT`, `..._COMPRESSION`, `..._INSECURE` | As the OTLP exporter reads them |
| `OTEL_SERVICE_NAME` | The resource's `service.name`; default `branchyard-server`, `branchyard-worker` or `by` |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes. Every resource also has `service.version`, `host.name` and `by.role` |
| `OTEL_SDK_DISABLED=true` | Nothing is exported or sampled |
| `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER` | `none` turns that signal's push off even with an endpoint set; `otlp` turns it on at the default endpoint. `prometheus` is the pull endpoint, always available on a server. Any other value is named in a warning and not exported to |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | `parentbased_always_on` (the default), `always_on`, `always_off`, `traceidratio`, `parentbased_always_off`, `parentbased_traceidratio`, with the ratio as the argument. A trace that arrives sampled stays sampled under a parent-based sampler |
| `BRANCHYARD_TRACE_STORE=1` | Also a span for each call to the store (`store <method>`), inside spans of the trace. Off by default: there are thousands per operation |
| `OTEL_BSP_*`, `OTEL_METRIC_EXPORT_INTERVAL` | The SDK's own batching and push interval (spans batch on a thread of their own; metrics push every 60 seconds) |

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318 OTEL_SERVICE_NAME=by-prod by serve --config server.json
```

An export that fails is logged by the SDK and dropped, never retried indefinitely, and never delays an operation. Telemetry settings that cannot be honoured (an unknown protocol, a sampler argument that is not a number) are logged once at start-up as `telemetry: ...` warnings.

These are the variables a harness's own telemetry (Claude Code's, Codex's) also reads. `by` run in an environment that sets them exports its own spans and metrics too, as the service `by`; to keep `by` itself quiet, set `OTEL_TRACES_EXPORTER=none` and `OTEL_METRICS_EXPORTER=none` in its environment (the harness inherits them, so this also silences a harness that reads them; `--telemetry` on a run sends the harness's own telemetry where the run says).

## Traces

A served operation's trace:

| Span | Parent | From, to | Attributes |
|---|---|---|---|
| `admission` (server kind) | the request's `traceparent` header, when it sends a valid one; else a new trace | the admission's transaction | `by.operation`, `by.repo`, `by.kind`, `by.tenant`, `by.priority`, `by.admitted` (false when a key replayed, a branch was busy or a quota refused) |
| `claim` | `admission` | admission to the claim: the time it waited queued | `by.operation`, `by.worker`, `by.fence`, `by.took_over`, `by.waited_ms`, `by.priority` |
| `operation <kind>` (`operation task`, `operation merge`, ...) | `admission` | the worker's run, start to outcome | `by.operation`, `by.repo`, `by.kind`, `by.tenant`, `by.branches`, `by.fence`; `error` and an error status when it failed |
| `turn` | `operation <kind>`, or the `delegation <tool>` that started the branch | the prompt to the turn's end | `gen_ai.operation.name` (`invoke_agent`), `gen_ai.agent.name` (the harness), `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `by.branch`, `by.harness`, `by.turn`, `by.outcome`, `by.live`; an error status and `error.type` unless it completed |
| `tool <name>` | `turn` | the harness's `tool_started` to the branch's next event (harnesses report when a tool starts, not when it ends) | `gen_ai.operation.name` (`execute_tool`), `gen_ai.tool.name`, `gen_ai.tool.call.id`, `by.tool`, `by.call_id`, `by.tool.target.kind`, `by.tool.target` |
| `permission wait` | `turn` | the harness asking to the answer | `by.tool`, `by.allowed`, `by.source` (what answered: a rule, the default, the ask callback, the approval policy; `withdrawn` when the harness took the request back) |
| `steer` | `turn` | the input written into the turn to the harness taking or refusing it | `by.steer`, `by.steered_by`, `by.accepted`; an error status when refused |
| `stall` | `turn` | no harness activity for the stall window, to activity (or the turn's end) | `by.since_ms` |
| `connector <operation>` (client kind) | `turn` | the gateway's recorded latency, ending when the call was ingested | `by.connector`, `by.operation`, `by.decision`; an error status unless allowed |
| `delegation <tool>` | the delegating branch's `turn` | a spawn, a plan, a graph change a branch asked for (`delegation send`, `cancel` and the rest are instants) | `by.branch`, `by.tool`, `by.child`, `by.refused`; an error status when refused |
| `connector token` | the branch's `turn` | a sandbox asking for its turn's gateway token | `by.branch`, `by.refusal` (the refusal's code) when refused; never the secret, the token or the manifest |
| `message observe` | the span the message carried in `meta.trace` (the sender's turn), else the span in scope | the messaging pipeline's observe stage, which tells the observer of a stored message | `by.message`, `by.message.kind`, `by.from`, `by.to`; never the text |
| `sandbox <action>` | the span in scope | one provider action: `ensure`, `exec` (starting the process, not its life), `checkpoint`, `restore`, `branch`, `pause`, `resume`, `stop`, `destroy` | `by.provider`, `by.action`, `by.sandbox`; an error status with the provider's error |
| `git <operation>` | the span in scope | a git operation on a branch's hot path: `worktree_add`, `commit`, `merge`, `rebase`, `push` | `by.git.operation` only, never the arguments; an error status when git exited non-zero |
| `store <method>` | the span in scope | one call to the store (only with `BRANCHYARD_TRACE_STORE=1`, below) | `by.store.backend` (`sqlite`, `postgres`), `by.store.method`; an error status |

`admission`, `operation <kind>`, and the spans from `turn` down are `tracing` spans entered and closed as the work happens, so the log lines written meanwhile (`--log-format json`) can be joined to the trace; `claim`, a connector call and the instants of a delegation are written with explicit times through the OpenTelemetry API (`claim` starts at the admission; the others are known only when recorded), still as they happen. Only spans of the target `branchyard_trace` are bridged, so a request, a poll or a library's own span does not become a trace of its own.

**Live turn spans.** The engine's recorder gives every activity it appends to a state machine ([`live_trace`](../crates/branchyard/src/live_trace.rs)) that opens the `turn` span at the prompt and closes it at the turn's end, with the tool, permission, steer and stall spans beneath it for as long as each lasts. A turn is a child of the operation's span (`TaskOptions::trace_parent`), or of the span in scope in a local `by` command. Attributes follow the OpenTelemetry GenAI conventions where one fits; a tool call carries the harness's bounded, scrubbed target ([`target.rs`](../crates/branchyard-harness/src/target.rs): a path relative to the worktree, a command with its secrets removed and cut at 120 characters, an MCP server and tool) and never its input, a steer never its text, a permission request never its input. A harness that titles a shell call with the command itself is named `tool command`, so the title is not copied.

**Delegation.** A branch's `spawn`, `apply_graph` and `delegate` are `delegation <tool>` spans under the branch's live turn; the child branches they start run their turns under that span, whichever thread they run on, so a fan-out reads parent turn, delegation, child turns in one trace. The children's turns carry `TRACEPARENT` of the delegation, as the parent's carry the operation's.

**One turn, traced once.** `claim` aside, a server that has live spans for a turn does not write the after-the-fact ones: the observe pass that reads a finished operation's events (below) writes a turn's `turn`, `tool` and `connector` spans only for a turn the engine did not trace live (one recorded by another process, one whose telemetry was off when it ran), keeps counting its metrics either way, and the live spans carry `by.live`. Only a turn the process traced is skipped: the engine remembers the branch and prompt time of each, and the pass asks. A connector call ingested while no turn of its branch is live is written by the pass when its turn was not traced live, and not at all otherwise (see Not done).

**Off means off.** With no exporter configured `trace_span!` builds no span at all: a counting subscriber sees none of the trace's spans through a turn with a tool call, a delegation and a message (`live_trace_off.rs`), where the same work with the switch on offers them. The timers of sandbox actions and git operations are a clock read each.

**Store spans** are off unless `BRANCHYARD_TRACE_STORE=1` and spans are exported: there are thousands of queries in an operation. Even then a query gets a span only when it runs inside a span of the trace, so a query on a thread with no span in scope starts no trace of its own; they are of the target `branchyard_trace_store`, which the telemetry layer bridges only while the switch is on.

The admission's context is stored with the operation (`StoredOperation::trace`), so the worker that claims it, on any server sharing the database, continues the same trace. A second worker's `claim` and `operation` spans are children of the admission in its trace, whichever process writes them.

**Sampling** is by `OTEL_TRACES_SAMPLER` (above); the default follows the parent's decision and samples every root.

### After the fact: the fallback

A turn the engine did not trace live is still traced, from the events its operation recorded: the observe pass ([`observe.rs`](../crates/branchyard-server/src/observe.rs)) reads the feed from the operation's admission to its end and writes `turn`, `tool <name>` and `connector <operation>` spans with the events' own timestamps, children of the operation's span. It is also what counts the turn metrics, for both paths.

### `by trace`

`by trace BRANCH [--subtree] [--json | --otlp FILE | --endpoint URL]` rebuilds a branch's trajectory from its event log ([`trace_export.rs`](../crates/branchyard/src/trace_export.rs)), so a run that is over can be loaded into Jaeger or Tempo, or a branch that ran without telemetry on. The tree is a `branch <name>` span over the log, a `turn` under it for each prompt, and under a turn its tools, permission waits, steers, stalls and connector calls; a `delegation <tool>` span where the branch asked for work, with the child branch's span under a spawn. With `--subtree` the branches delegated under BRANCH are included. The spans carry the attributes above and `by.source = event-log`.

| Option | |
|---|---|
| (none) | an indented tree with each span's duration and outcome |
| `--json` | the OTLP/JSON document (`resourceSpans`) on standard output |
| `--otlp FILE` | the same document written to FILE |
| `--endpoint URL` | the document posted to a collector's OTLP/HTTP traces endpoint (`/v1/traces` is added unless the URL ends with it); `OTEL_EXPORTER_OTLP_TRACES_HEADERS` or `OTEL_EXPORTER_OTLP_HEADERS` supplies headers such as an API key, sent only when the URL is the configured collector (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`, with or without `/v1/traces`): to any other URL the trace goes without them and the output says so, and they are refused (`denied`) over plain `http://` to a host that is not this machine, since they are credentials. A document that cannot be encoded is an error, never an empty file or an empty post |

**Ids are derived, not drawn.** The trace id is the trace a message the branch sent had stored (`meta.trace`, the `traceparent` of the sending turn) when it carried one, and then the root span is that message's span's child; otherwise it is a hash of the branch's name and the time of its first event. A span id is a hash of the trace id and the span's place in the log (branch, event index), so the same log always exports the same trace, exporting twice to a collector makes the same spans and not a second copy, and a child branch's span can name its parent's delegation without a lookup. `by trace` reads this repository's log: it refuses `--remote`. A branch with no events is an error with a next step. Tool inputs and steer text are not in the log's targets and are not exported.

## Local commands

A `by` command run locally exports the same way when an endpoint is set (it does not, otherwise). The command runs under a `by <command>` span (`by run`, `by send`, ...), a child of the `TRACEPARENT` it inherited when that is valid, so a CI job or a harness that runs `by` continues its own trace. The turns it runs are live spans under the command's span, as in a server (`by run` and the commands that run a turn in this process); when it ends, the turns, tool calls and connector calls the repository recorded while it ran that were not traced live become the same `turn`, `tool` and `connector` spans, under the command's span, and the same metrics (`branchyard_turns_*`, `branchyard_tool_*`, `gen_ai.client.*`, the start latency of a branch it made), as a served operation's: one recorder reads the events for both. A command that records no events makes the span only. Commands that run no turn (`by ls`) are spans of a few milliseconds.

## Metrics

Off by default. Turn them on with `--metrics` (on the server's own listener), `--metrics-addr ADDR` (also a plain-HTTP listener of its own that serves only `/metrics`), `--metrics-token-file FILE`, or in the configuration file:

```json
"metrics": { "listen": "127.0.0.1:9464", "token_file": "/etc/branchyard/metrics.token" }
```

| Where | Who may read it |
|---|---|
| `GET /metrics` on the server's listener | a principal with the `admin` scope, or the metrics token. Another principal gets `403 scope_required` (`detail.scope = "admin"`), no token `401` |
| `GET /metrics` on `--metrics-addr` | the metrics token when one is configured, else anyone who can reach it. A non-loopback metrics address needs a token and `--insecure-bind` (it is plain HTTP) |

The metrics token (at least 16 characters) reads `/metrics` and nothing else: it is not a credential for the API. A `by worker`, which has no listener, can serve metrics with `--metrics-addr`. With an OTLP metrics endpoint set the same instruments are also pushed every `OTEL_METRIC_EXPORT_INTERVAL` (60 s); the gauges read from the store are read for each push, at most once every 2 seconds.

The format is Prometheus text exposition 0.0.4 (`text/plain; version=0.0.4`). Every family is declared once, in [`metrics.rs`](../crates/branchyard-server/src/metrics.rs), with its type, unit and help, and written even before it has samples. Labels are written in the order the table gives. The catalogue below is generated from those declarations (`python3 tools/docs_facts.py --write`), <!-- fact:metrics.count -->59<!-- /fact --> families:

<!-- fact:metrics.table -->

| Metric | Type | Unit | Labels | What |
|---|---|---|---|---|
| `branchyard_build_info` | gauge |  | `version` | Always 1; the version label is this server's. |
| `branchyard_operations` | gauge |  | `state` | Unfinished operations in the shared queue, by state (queued: unclaimed; running: claimed). |
| `branchyard_operations_admitted_total` | counter |  | `kind`, `tenant` | Operations this process admitted, by kind and tenant. |
| `branchyard_operations_finished_total` | counter |  | `kind`, `state` | Operations this process recorded finished, by kind and state (succeeded, failed, interrupted). |
| `branchyard_operation_duration_seconds` | histogram | `s` | `kind`, `state` | Time an operation ran, from its run starting to its outcome being recorded, by kind and state. |
| `branchyard_queue_depth` | gauge |  | `tenant`, `priority`, `labels` | Unclaimed queued operations, by tenant, priority and required worker labels. |
| `branchyard_queue_oldest_age_seconds` | gauge | `s` | `tenant`, `priority`, `labels` | Age of the oldest unclaimed queued operation, by tenant, priority and required worker labels. |
| `branchyard_claims_total` | counter |  | `tenant`, `priority` | Queued operations this process's dispatcher claimed, by tenant and priority. |
| `branchyard_claim_wait_seconds` | histogram | `s` | `priority` | Time from admission to a claim, by priority. |
| `branchyard_lease_renewals_total` | counter |  | `result` | Claim lease renewals by this process, by result (renewed, lost, error). |
| `branchyard_lease_expiries_total` | counter |  | none | Claims this process took over from a worker whose lease had lapsed. |
| `branchyard_turns_started_total` | counter |  | `harness` | Turns started by operations this process ran, by harness. |
| `branchyard_turns_ended_total` | counter |  | `harness`, `outcome` | Turns ended by operations this process ran, by harness and outcome. |
| `branchyard_turn_duration_seconds` | histogram | `s` | `harness` | Turn duration, from the prompt to the turn's end, by harness. |
| `branchyard_turn_first_event_seconds` | histogram | `s` | `harness` | Time from a turn's prompt to the harness's first event, by harness. |
| `branchyard_turns_in_flight` | gauge |  | `harness` | Branches with a turn running, by harness, in the repositories this process serves. |
| `branchyard_turn_stalls_total` | counter |  | `harness` | Branches that an operation this process ran left stalled (no harness activity for the stall budget), by harness. |
| `gen_ai_client_operation_duration_seconds` | histogram | `s` | `gen_ai_operation_name`, `gen_ai_agent_name`, `by_turn_outcome`, `error_type` | GenAI client operation duration: a turn as an invoke_agent operation, by agent (harness) and outcome. |
| `gen_ai_client_token_usage` | histogram | `{token}` | `gen_ai_operation_name`, `gen_ai_token_type`, `gen_ai_agent_name`, `gen_ai_request_model` | Tokens harnesses reported spending in turns this process ran, by token type (input, output), agent (harness) and model. |
| `branchyard_usage_cost_usd_total` | counter |  | `harness`, `model` | Cost, in USD, harnesses reported with their usage in turns this process ran, by harness and model. |
| `branchyard_tool_calls_total` | counter |  | `harness` | Tool calls harnesses reported in operations this process ran, by harness. |
| `branchyard_tool_call_duration_seconds` | histogram | `s` | `harness`, `tool` | Tool call duration (from tool_started to the branch's next event), by harness and tool; tools outside the known list are `other`. |
| `branchyard_cost_usd_total` | counter |  | `tenant`, `harness` | Harness-reported cost, in USD, of operations this process ran, by tenant and harness. |
| `branchyard_connector_calls_total` | counter |  | `connector`, `decision` | Connector gateway calls in operations this process ran, by connector and decision. |
| `branchyard_connector_call_duration_seconds` | histogram | `s` | `connector`, `decision` | Connector gateway call latency in operations this process ran, by connector and decision. |
| `branchyard_skill_searches_total` | counter |  | `kind` | Searches of the tool tree (tools search) in turns this process ran, by the kind searched (all, skill, reference, connector, operation, helper). |
| `branchyard_skill_opens_total` | counter |  | `source`, `level` | Skills, references, connectors, operations and helpers opened (tools show) in turns this process ran, by source (built_in, repository, user, connector, helper) and level (1 to 3). |
| `branchyard_skill_search_unopened_total` | counter |  | none | Searches of the tool tree followed by no open before the turn ended, in turns this process ran. |
| `branchyard_model_calls_total` | counter |  | `model`, `decision` | Model gateway calls in operations this process ran, by model and decision. |
| `branchyard_model_tokens_total` | counter |  | `model`, `kind` | Tokens of model gateway calls in operations this process ran, by model and kind (input, output, cache_read, cache_write). |
| `branchyard_model_cost_usd_total` | counter |  | `model` | Metered cost, in USD, of model gateway calls in operations this process ran, by model. |
| `branchyard_webhook_deliveries_total` | counter |  | `result` | Webhook deliveries by this process, by result (delivered, retried, dead_lettered). |
| `branchyard_trigger_looks_total` | counter |  | none | Times this process's trigger dispatcher looked for schedules due and runs waiting to fire. |
| `branchyard_workers_live` | gauge |  | none | Workers that recorded themselves alive in the last 15 seconds. |
| `branchyard_worker_last_seen_seconds` | gauge | `s` | `worker`, `host` | Seconds since each live worker last recorded itself alive. |
| `branchyard_effects` | gauge |  | `repo`, `class`, `state` | Entries in each repository's effect ledger, by class (reversible, compensable, irreversible) and state. |
| `branchyard_approvals_pending` | gauge |  | `repo` | Approvals waiting for an answer in each repository. |
| `branchyard_pool_slots` | gauge |  | `repo`, `state` | Warm pool slots of each repository on this host, by state (ready, filling, claimed). |
| `branchyard_pool_claims_total` | counter |  | `repo`, `result` | New branches of operations this process ran whose workspace has a pool, by repository and result (hit, miss). |
| `branchyard_pool_slots_made_total` | counter |  | `repo`, `result` | Warm pool slots this process's keepers made, by repository and result (made, failed). |
| `branchyard_pool_fill_seconds` | histogram | `s` | `repo` | Time this process's keepers took to make one warm pool slot, by repository. |
| `branchyard_pool_slots_discarded_total` | counter |  | `repo` | Warm pool slots this process's keepers removed as stale or left by a stopped process, by repository. |
| `branchyard_start_seconds` | histogram | `s` | `pool`, `provider`, `path` | Start latency of a task's new branches, from admission to the harness's first prompt, by pool (hit, miss, none), provider and path. |
| `branchyard_start_phase_seconds` | histogram | `s` | `provider`, `path`, `phase` | Time one phase of a task's new branches' start took, by provider, path and phase; a phase that did not occur is not counted. |
| `branchyard_sync_bytes_total` | counter | `By` | `repo`, `direction` | Bytes this process's sync sent to and read from the remote, by repository and direction (up, down). |
| `branchyard_sync_objects_total` | counter |  | `repo`, `direction` | Objects this process's sync wrote to and read from the remote, by repository and direction (up, down). |
| `branchyard_sync_swaps_total` | counter |  | `repo` | Task manifests this process swapped into the remote, by repository. |
| `branchyard_sync_swap_conflicts_total` | counter |  | `repo` | Manifest swaps that lost to another writer and were merged and tried again, by repository. |
| `branchyard_sync_retries_total` | counter |  | `repo` | Remote requests tried again after a transient failure, by repository. |
| `branchyard_sync_divergences_total` | counter |  | `repo` | Divergences recorded as conflict branches, by repository. |
| `branchyard_sync_corrupt_total` | counter |  | `repo` | Objects read from the remote and refused because they did not match their name, by repository. |
| `branchyard_sync_errors_total` | counter |  | `repo` | Task syncs that failed (and were queued to try again), by repository. |
| `branchyard_sync_pending` | gauge |  | `repo` | Tasks queued in the sync outbox, by repository. |
| `branchyard_sync_lag_seconds` | gauge | `s` | `repo` | How long the oldest queued change has waited to reach the remote, by repository. |
| `http_server_request_duration_seconds` | histogram | `s` | `http_route`, `http_request_method`, `status_class` | Duration of HTTP requests this server served, by route template, method and status class. |
| `http_server_active_requests` | gauge | `{request}` | `http_request_method` | HTTP requests being served now, by method. |
| `branchyard_api_errors_total` | counter |  | `code` | API requests refused, by the error code in the response. |
| `branchyard_sandbox_action_duration_seconds` | histogram | `s` | `provider`, `action`, `result` | Time a sandbox provider's action took in turns this process ran, by provider, action (ensure, exec, checkpoint, restore, branch, pause, resume, stop, destroy) and result (ok, error). |
| `branchyard_git_operation_duration_seconds` | histogram | `s` | `operation`, `result` | Time a git operation on a hot path took in this process, by operation (worktree_add, commit, merge, rebase, push) and result (ok, error). |

<!-- /fact -->

**Names.** The families before `http_server_request_duration_seconds` keep the names and labels they have always had. The last few follow the OpenTelemetry semantic conventions where one fits: `http.server.request.duration` and `http.server.active_requests` (HTTP RED), and `gen_ai.client.operation.duration` and `gen_ai.client.token.usage` (a turn as an `invoke_agent` operation, and the tokens its harness reported). `by.turn.outcome`, `status_class` and the rest are Branchyard's own attributes.

**Label cardinality is bounded.** A label is a state, a kind, a harness, a configured name or a template, never an id: the HTTP route is the matched template (`/v1/repos/{repo}/tasks`; a request no route took is `unmatched`), the method is one of the standard seven or `_OTHER`, the status is its class (`2xx`). A name a client could make up passes through a guard: a tool is one of a closed list of known tools (`bash`, `read`, `edit`, `grep`, ...), `mcp` for any MCP server's tool, or `other`; a model's name is its own for the first 64 models seen and `other` after; an error code is one of the API's. The SDK also caps a series at 2000 attribute sets per instrument.

Histogram buckets are 0.1, 0.5, 1, 5, 15, 30, 60, 120, 300, 600, 1800 and 3600 seconds, except `branchyard_start_seconds` and `branchyard_start_phase_seconds` (0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 300, 1800), `http_server_request_duration_seconds` (OpenTelemetry's 0.005 to 10), tool and connector calls, sandbox actions and git operations (0.01 to 300), `branchyard_turn_first_event_seconds` (0.1 to 120) and `gen_ai_client_token_usage` (OpenTelemetry's powers of four, 1 to 67108864).

**Counters and gauges on several servers.** Counters count what the process that serves them did. Each operation is admitted by one server and claimed, run and finished by one worker, so on several servers and workers sharing a database, sum counters across them. Gauges read at scrape time from the store (`branchyard_operations`, queue depth and age, live workers, effects) describe the shared database, so every server reports the same values: take one, or `max`, not the sum. `branchyard_turns_in_flight` counts branches whose status is running in the repositories this process serves.

**What turns are counted.** Turns, tool calls, connector calls, tokens and cost are read, once an operation finishes, from the events its branches (and the branches they delegated to) recorded between its admission and its end. Turns no operation ran are not counted: a local `by run` on a served repository, and a graph dependent started by a recovery tick (a local command's own turns are counted by that command, above). Tokens a harness reports as a running total count what the total grew by; the first observation of a branch in an operation counts whole. A turn's duration is a GenAI client operation (`gen_ai_client_operation_duration_seconds`, by agent and outcome) as well as `branchyard_turn_duration_seconds`.

**Sandbox and git timings.** `branchyard_sandbox_action_duration_seconds` (provider, action, result) times each provider action the engine asks of a sandbox in a turn this process ran, and `branchyard_git_operation_duration_seconds` (operation, result) each git operation on a branch's hot path in this process (`worktree_add`, `commit`, `merge`, `rebase`, `push`). Their labels are closed sets: a provider is its kind (`microsandbox`, `recipe`, ...) and never a sandbox's name, an operation is one of five. A git run that exits non-zero counts as `error`, including `git merge-tree` reporting a conflict. The spans and the timers come from the same wrapper, so a number and the span it summarises agree.

**Request errors.** `branchyard_api_errors_total` counts every refused request by the `code` of its error response (`unknown_branch`, `scope_required`, `busy`, ...), so an alert can name a kind of refusal; the request's duration is in `http_server_request_duration_seconds` by status class.

## Propagation

W3C trace context, through the OpenTelemetry propagator ([`propagation`](../crates/branchyard-telemetry/src/propagation.rs)); a malformed header, all-zero ids or an unknown version is no parent, and a new trace starts.

- **Into the harness.** Each turn of a task, send, fork, reincarnate or spawn the server runs gets `TRACEPARENT` (the operation span's context) in its environment ([`TaskOptions::trace_parent`](../crates/branchyard/src/lib.rs)). What the harness runs inherits it (a turn's harness gets the operation's span, not the turn's: the environment is set before the prompt, which is where the turn span begins; a delegated child's gets its delegation's): a connector SDK that copies `TRACEPARENT` into the gateway call's `_meta.traceparent`, where Anvil reads it, makes the gateway's spans children of the operation; a tool that exports its own spans joins the trace the same way. A local `by run` passes on whatever `TRACEPARENT` its own environment has.
- **Into webhooks.** A delivery of a branch's activity carries a `traceparent` header with the context of the last operation this server ran on that branch, when there is one (not for a branch another server worked on).
- **Into messages.** A message a branch sends (`ask`, `report`, `escalate`, `answer`, `message`, `handoff`) carries the `TRACEPARENT` of the turn that sent it in the message's `meta.trace`, where the recipient's side can read it (`by inbox --json`, the event log), and the pipeline's `observe` stage is a `message observe` span under that context. A message sent with no trace has no `meta` at all, so the JSON of every existing message is unchanged ([messaging](messaging.md#decisions)).
- **From clients.** Send `traceparent` on any operation's `POST`; a malformed one starts a new trace.
- **From the environment.** A local `by` command continues a valid `TRACEPARENT`.

`tracestate` is not stored or forwarded.

## Survived failures

Some failures are acceptable (a temporary directory that cannot be removed, a process that is already gone, a lease released on a path that cannot return an error) but none is acceptable unseen. The [`branchyard-support`](../crates/branchyard-support/src/lib.rs) crate is the one way the workspace says "this may fail; carry on, but say so":

- A best-effort step that fails logs a `warn` through `tracing` with a context string and the error: `best-effort step failed: release a lease dropped without finish: ...`. Every `by` command shows it on its standard error as one line, `by: warning: release a lease dropped without finish: ...`, as it happens (help and usage errors too, where a failed print is itself such a step; not under the live `by watch` screen). `RUST_LOG` (or `BRANCHYARD_LOG`) sets the level: `RUST_LOG=trace by ...` adds the debug and trace lines of every crate. `by serve` and `branchyard-herdr` write the failures with the rest of their logs. A process can also install a sink (`set_failure_sink`) to receive each one as a structured `Failure`, or run a piece of work under `collect_failures` to get back the failures it survived on its own thread.
- The engine does the second for each turn: the steps a turn survived (a sandbox that would not tear down, a lease that would not release) are also appended to the branch's own log as `warning` events beginning `a step failed and the run carried on: `, once each and at most 20 per turn. `by log` lists them and `by show` gives them one line (`survived  1 step failed and the run carried on, the first: ...`), so a failure that scrolled past on standard error is still with the branch.
- A standard output that cannot be written does not end a command or lose a turn. A reader that went away (`by run ... | head -1`, a closed pipe) is its own choice and is not reported; a full disk or any other write error is reported once, `by: warning: write the console output (it is not written again): ...`. `by`'s own `eprintln!` ignores a standard error that cannot be written for the same reason, and the engine contains a panic in the observer that prints a branch's events, so a turn that completed is recorded as completed whatever happens to the terminal.
- A lock whose holder panicked (std calls it poisoned) is used anyway, as before, but the first use logs `lock `NAME` was poisoned by a panic in another thread; recovering its data (taken at FILE:LINE)`, once per poisoning.
- A branch's turn thread that panics appends a `warning` event ("the turn's thread panicked: ...") to the branch's own event log, besides the log line, so `by events` and `by watch` show it.
- A thread joined during shutdown that ended in a panic logs `join the NAME thread: it panicked: ...` instead of dropping the payload.
- A system clock set before 1970 reads as the epoch, and the first read logs `the system clock is set before 1970-01-01; reading it as the epoch`. Every part of Branchyard reads the clock through `branchyard_support::time::now_ms`, so this is one message, not one silent zero per crate.
- A failed read of the system random generator is an error where an id is made (`by run` and the other commands that start a task say `the system random generator failed`); it no longer yields an id made from zeros. A seed for jitter or routing that cannot be had from the system falls back to one mixed from the clock and process id, and logs `seeding from the clock and process id instead`.

Set `BRANCHYARD_LOG=branchyard_support=off` to silence them; there is no other switch, on purpose.

Not done: only the steps a turn survives reach the branch's log; a cleanup that fails in `by merge` or `by rm` is on standard error and nowhere else. Failures on threads the turn does not run on (a sandbox's output pump, the lease heartbeat) are logged but not collected. There is no summary footer at the end of a command; the lines appear as the failures happen.

## Startup phases

Each turn records one `startup` activity once its harness has said its first thing (or, if it ended first, when it ended): the `provider`, the `path` its sandbox was made ready by, and a millisecond offset from the operation's admission for each phase it reached: `queued`, `placement`, `image_ready`, `sandbox_ready`, `source_attached`, `harness_started`, `handshake_done`, `first_event`. A phase that did not occur is absent, not zero: a local harness has no sandbox phases, and `image_ready` appears only for a provider that measures its image. The clock is `branchyard_support::time`. `by log` prints a line (`startup: agent_sandbox pool; queued 4 ms, placement 9 ms, sandbox_ready 310 ms, ...`) and `by log --json` the `startup` object with its `phases`. The metrics above are made from the same activity, and `by startup report` reads the same activities back ([measuring launch time](providers.md#measuring-launch-time)); no second table is kept.

Hermetic evidence (fake clock, FakeProvider, the exposition parser); live: none. Not done: no number measured on any real provider.

## `by stats`

```text
$ by stats
branches  12 (failed 1, merged 4, ready 5, running 2)
turns     37 (claude-code 20, codex 17)
outcomes  budget_exceeded 2, completed 30, failed 2, interrupted 3
turn time median 42.0s, p90 3m10s
tools     120 calls
connectors 8 calls (allowed 7, denied 1)
cost      $4.12 (claude-code $3.00, codex $1.12)
```

Locally it reads the repository's store: branches by status, turns and cost by harness from their records, and outcomes, turn durations, tool calls and connector calls from the event store. When branches used a [warm pool](pools.md), it adds `pool` (hits, misses, and locally the ready slots of the pool's size and how long they took to make) and `start` lines (from a branch being asked for to its first prompt, of hits and of misses). With `--remote` it reads the server's branches (turns and cost, not the event store) and adds the queue: the caller's tenant's queued operations of the repository by priority, those running, and how long the oldest has waited. `--json` prints the same as an object (`branches`, `turns`, `cost_usd`, `outcomes`, `turn_seconds`, `tool_calls`, `connector_calls`, `queue`, `pool`).

## Tests

- `crates/branchyard-support/src/trace.rs` (1): with no exporter `trace_span!` is no span, and the hooks and the writer of finished spans are no-ops.
- `crates/branchyard/tests/live_trace.rs` (10, `cargo test -p branchyard --test live_trace`): against the fake ACP agent with spans captured in memory, a turn is a live span with the GenAI attributes and `by.live`, a child of the span it is given, with its shell call beneath it carrying the scrubbed target and nothing of the secret in the command; a permission wait with its answer; a stalled, interrupted turn with a `stall` span, an error status and `error.type`; a steer span with no text; a delegated child's turn under the `delegation spawn` span in the same trace; a sandbox provider's `ensure`, `exec` and `pause` through `FakeProvider` with the provider kind; the hot git operations and no arguments; store queries only when asked and only inside a trace (no orphan span); a message's observe stage under the sender's context with no text; a connector token request refused with its code and no secret.
- `crates/branchyard/tests/live_trace_off.rs` (1): a counting subscriber is offered no span of the trace through a turn with a tool call, a delegation and a message with telemetry off, and some with it on.
- `crates/branchyard/src/trace_export.rs` (5): a branch's log as a tree with the live attributes and the scrubbed target; the same log, the same ids; a stored trace adopted; a child under its parent's delegation; waits, steers, stalls and connector calls under their turn with no tool input or steer text; an empty log; an open turn.
- `crates/branchyard-workspace/src/git.rs` (1): which git invocations are hot operations.
- `crates/branchyard-cli/tests/trace.rs` (4): `by trace` over a run with the built `by`: deterministic ids, parent links, the scrubbed target, the tree, `--otlp` and `--json` the same document, `--endpoint` posting it with the configured headers, and refusals.
- `crates/branchyard-telemetry/src` (18, `cargo test -p branchyard-telemetry --lib`): the `OTEL_*` variables (nothing is pushed without an endpoint, per-signal endpoints, `none` and the kill switch, protocols, samplers and their arguments); `traceparent` round trips through the propagator, injection, and malformed headers, all-zero ids and bad versions as no parent; Prometheus name translation, exposition of counters, gauges and histograms with cumulative buckets and declared label order, observable callbacks, and a shut-down provider as an error; samplers deciding as the variables say, and isolated meters.
- `crates/branchyard-telemetry/tests/spans.rs` (3): nested `tracing` spans become parent and child with their attributes, kind and error status, only spans of the bridged target are exported, an incoming `traceparent` becomes the parent and a malformed one starts a new trace; spans written after the fact keep their times and their children join them.
- `crates/branchyard-telemetry/tests/otlp.rs` (1) and `tests/grpc.rs` (1): a real export to a stand-in collector on loopback, over OTLP/HTTP (the endpoint, headers and service name from the standard variables, the resource attributes, the span tree and a metric decoded from the protobuf) and over OTLP/gRPC from a process with no runtime of its own.
- `crates/branchyard-telemetry/tests/disabled.rs` (1): with nothing configured, nothing is exported, no thread is started (counted from `/proc`), no span is bridged, and an incoming `traceparent` passes through.
- `crates/branchyard-server/src/metrics.rs` (9): every family's `HELP` and `TYPE` in order, counters, histograms (cumulative buckets, `+Inf`, sum and count), queue gauges and label escaping, checked with a parser of the exposition format written independently of the renderer; the existing names unchanged; the new families under their semantic-convention names; bounded labels; gauges filled from a registered source for an OTLP push; the catalogue above naming every family.
- `crates/branchyard/src/startup.rs` (5) and `crates/branchyard/tests/startup.rs` (5): the timer on a fake clock, durations, the wire form, the path a launch means, the report's percentiles; a local and a sandboxed turn (FakeProvider, with and without a pool and an image measurement) recording ordered phases, the queue as the first phase, and the report over the store's feed.
- `crates/branchyard-server/src/observe.rs` (7, one of them the sandbox and git timers reaching the registry under the declared names): a task's new branches' start latency by pool use, provider and path, each startup phase's time, and hits and misses; turns, tools, connector calls, first-event latency, tokens and cost from recorded events into metrics and spans (a tool lasting until the next event, an engine-ended turn, another operation's branch ignored, no span without a parent); the closed set of tool labels; cost as each branch's growth, and stalls.
- `crates/branchyard-server/src/ops.rs` (1 of its tests): another worker claiming an operation from the store continues the admission's stored trace.
- `crates/branchyard-server/tests/observability.rs` (5, over real HTTP with the fake ACP agent): a live turn is one `turn` span and one `tool` span in the trace, not two; priority checked, capped and inherited; `/metrics` off by default, `401`/`403`, the metrics token reading nothing else, the counters after a task, and the separate listener; an operation traced from an incoming `traceparent` through admission, claim and run to its turn, with the harness seeing the operation's span as `TRACEPARENT`; the golden metrics (requests by route template and never by URL, errors by code, operation duration, the turn as a GenAI operation).
- `crates/branchyard/tests/peers.rs` (1 of its tests): an `ask` sent under a `TRACEPARENT` stores it in `meta.trace` on both logs, and without one the message's JSON has no `meta`, byte for byte as before.
- `crates/branchyard-server/tests/webhook.rs`: a delivery carries the operation's `traceparent`, and deliveries are counted.
- `crates/branchyard-server/tests/pools.rs` (1): the pool metrics over real HTTP after a miss and a hit.
- `crates/branchyard-server/tests/sync.rs` (1): the sync series over real HTTP after two pushes.
- `crates/branchyard-cli/tests/telemetry.rs` (2): a local `by run` with the fake agent exports its `by run` span, the turn under it and the turn metrics to a loopback collector; with `OTEL_SDK_DISABLED` or no endpoint nothing is sent.
- `crates/branchyard-cli/src/stats_cmd.rs` (2, one for pool hits, misses and start latency) and `crates/branchyard-cli/tests/remote.rs` (1): the summary's arithmetic and rendering; `--priority` reaching the server's queue, `by --remote stats` showing it, and `by stats` locally.
