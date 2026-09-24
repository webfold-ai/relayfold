# Relayfold gateway benchmarks

This directory measures the deployed forwarding path. `relayfold` alone is an upstream-selection library; `biota-proxy/` supplies a deliberately small executable using Biota's real HTTP listener, client, and `Proxy::handle`. A test of that executable is labeled **Biota/Relayfold**, not a test of Tasmanian Cloud's `edge-lb` binary or a finished gateway product.

## Crosswalk first

[`gateway-crosswalk.json`](gateway-crosswalk.json) is the source of truth for the comparison. It names every candidate, every plane, every workload family, and the measurements required. Render it with:

```sh
python3 benchmarks/summarize_crosswalk.py benchmarks/gateway-crosswalk.json
```

The planes are deliberately separate: HTTP proxy/load-balancer, API policy/security, control-plane churn, MCP federation, agent-loop execution, and inference gateway behavior. A candidate receives `implemented`, `adapter`, `partial`, `planned`, `unsupported`, `blocked`, or `unmeasured`; an absent endpoint is not scored as zero.

## Persistent regression campaigns

Use `spbench` for durable run history and comparisons. The wrapper stores the Nighthawk campaign files under `benchmarks/results/`, records the Relayfold revision and target-manifest hash, and stores the wall-time campaign in spbench's configured `SANG_CODSPEED_DB`:

```sh
# standalone: install/build sang-codspeed, or point SPBENCH at it
SPBENCH=/path/to/sang-codspeed \
SANG_CODSPEED_DB=/var/lib/relayfold-bench \
benchmarks/run_spbench_campaign.sh benchmarks/http-targets.local-matrix.json \
  --rates 100,300,1000,3000 --duration 30 --warmup 5 --repeats 5 --connections 100

# compare a changed gateway/config against a prior durable spbench run
SPBENCH_BASE_RUN=<prior-run-id> benchmarks/run_spbench_campaign.sh \
  benchmarks/http-targets.local-matrix.json --rates 100,300,1000,3000 \
  --duration 30 --warmup 5 --repeats 5 --connections 100
```

This is the persistent performance gate. The short local campaign is only a bring-up check; release or version-change comparisons must use the wrapper, pinned target identities, five or more repetitions, resource traces, and the same manifest.

## Run a local connectivity and latency smoke

From the `relayfold` repository, in separate terminals:

```sh
BENCH_UPSTREAM_BIND=127.0.0.1:18081 \
  /path/to/bench_upstream
cargo build --release --manifest-path benchmarks/biota-proxy/Cargo.toml
BENCH_BIND=127.0.0.1:18080 BENCH_UPSTREAM_PORT=18081 \
  "${CARGO_TARGET_DIR:-benchmarks/biota-proxy/target}/release/relayfold-biota-bench-proxy"
python3 benchmarks/run_http.py benchmarks/http-targets.example.json \
  --rates 100 --duration 10 --warmup 2 --repeats 1 --connections 10
python3 benchmarks/summarize_http.py benchmarks/results/<campaign>/manifest.json
```

The performance fixture is the native async `bench_upstream` binary in `benchmarks/biota-proxy`; the Python server remains only as a wiring smoke fixture. For reported performance, place a controlled upstream, load generator, and gateway on separate machines on an isolated fast network. Record upstream p99 and capacity by running the direct target first. Set `BENCH_UPSTREAM_HOST` and `BENCH_UPSTREAM_PORT` for the proxy. `run_http.py` accepts any named URL list, so the **same request workload** runs against Nginx, HAProxy, Envoy, Pingap, Zentinel, APISIX, Kong, Tyk, and a production Relayfold gateway once those targets are provisioned.

The runner uses a pinned Nighthawk Docker image when `nighthawk_client` is absent. It explicitly enables `--open-loop`, fixes concurrency at one event loop so `--rps` is the aggregate offer, warms every run, retains raw JSON and stderr, and records the client version and host. `summarize_http.py` reports achieved RPS, completion fraction, HTTP errors, and latency percentiles. Keep the raw files: a high p99 with incomplete offered load is not a win. Pin each gateway binary/image and include its digest, configuration, CPU limits, and host details beside the campaign manifest.

Run contract regression separately from load testing. `regression-cases.json` covers health, empty/tiny, 1 KB, 1 MB, and missing-route behavior; `run_contract_regression.py` runs those cases against every target in a target manifest and fails if a status or body-size invariant changes:

```sh
python3 benchmarks/run_contract_regression.py benchmarks/http-targets.local-matrix.json
```

For the local seven-path passthrough run, use `bash benchmarks/run_local_matrix.sh --rates 100 --duration 5 --warmup 1 --repeats 2 --connections 20`. It builds and starts the native async upstream, Biota proxy, and five named proxy containers, then cleans up only those processes. The Nginx, HAProxy, Envoy, and Zentinel configs under `configs/` are port/upstream adaptations of [zentinel-bench](https://github.com/zentinelproxy/zentinel-bench) commit `66e621b9edc5ae34a444330911abc24b4a8978e1` (MIT); Pingap uses its documented `--upstream` mode. Container image digests are pinned in the script.

Every completed run is now rejected unless Nighthawk reports nonzero HTTP 2xx completions and a 1024-byte response body. Each run also has a hard command timeout, so an overloaded generator cannot be recorded as a successful benchmark. A high-rate campaign is `bash benchmarks/run_local_matrix.sh --rates 1000000 --duration 3 --warmup 1 --repeats 3 --connections 1000 --command-timeout 30`; report achieved 2xx RPS and wire counters alongside the offered rate.

## HTTP workload matrix

| Case | Workload and invariant | Methodology |
|---|---|---|
| Proxy floor | Direct upstream, then one route, no policy; 0 B and 1 KB; H1 keepalive | [NGINX sizing](https://www.f5.com/company/blog/nginx/nginx-plus-sizing-guide-how-we-tested), [Zentinel bench](https://github.com/zentinelproxy/zentinel-bench) |
| Connection and bytes | New HTTPS connection per request; 1 MB transfer; TLS to upstream separately; worker/core sweep | [NGINX sizing](https://www.f5.com/company/blog/nginx/nginx-plus-sizing-guide-how-we-tested) |
| Fixed-load curve | Offered RPS ladder below and past the knee; H1/H2, connection/stream sweeps; p50/p95/p99/p99.9, errors, CPU, RSS | [Nighthawk](https://github.com/envoyproxy/nighthawk), [Envoy performance guidance](https://www.envoyproxy.io/docs/envoy/latest/faq/performance/how_to_benchmark_envoy) |
| Policy tax | Vanilla, metrics/logs, key or JWT auth, quota, rate limit, all enabled; same effective decision and identities | [Tyk suite](https://github.com/TykTechnologies/tyk-ansible-performance-testing), [Kong suite](https://github.com/Kong/kong-gateway-performance-benchmark) |
| Routing scale | 1, 36, 100, 217, 609, 1,000 mixed static/dynamic routes, distributed requests | [Routing benchmark](https://github.com/vm-001/gateways-routing-benchmark), [Kong suite](https://github.com/Kong/kong-gateway-performance-benchmark) |
| Churn and failover | Route updates under traffic, backend fail/recovery, config propagation latency, transient errors and post-churn RSS | [Gateway API bench](https://github.com/howardjohn/gateway-api-bench), [Zentinel bench](https://github.com/zentinelproxy/zentinel-bench) |
| Inference | Same model/backend pool, direct service versus gateway; TTFT, output tokens/s, completion, saturation, backend queue balance | [Agentgateway campaigns](https://github.com/agentgateway/benchmarks) |

Run equivalent behavior, not plugin names. Mark a capability `unsupported` if it cannot enforce the same rule. Existing k6 and TRex can cover scripted scenarios and packet-level stress; Nighthawk supplies the independent fixed-rate L7 latency curve. Published vendor scores define workloads, not cross-machine rankings.

## MCP scorecard

Keep MCP distinct from HTTP proxy throughput. Candidate gateways: [MikkoParkkola/mcp-gateway](https://github.com/MikkoParkkola/mcp-gateway) (compact tool catalog and on-demand discovery), [MCPJungle](https://github.com/mcpjungle/MCPJungle) (shared registry and tool groups), [Supergateway](https://github.com/supercorp-ai/supergateway) (stdio/SSE/Streamable HTTP transport bridge), and the local Ptyktos path: Conduit federation behind Syndesi's gateway surface, with Telos authorization and Agent Loop execution. Those products expose different tool surfaces: compare outcomes and cost per completed task where the interfaces differ, and use identical wire tests only where they implement the same MCP transport.

The included deterministic stdio backend exposes 100 tools by default. To test a Streamable HTTP bridge, start it with that backend and run:

```sh
MCP_FIXTURE_TOOLS=100 npx -y supergateway@4.0.0 \
  --stdio "python3 $PWD/benchmarks/mcp_fixture.py" \
  --outputTransport streamableHttp --stateful --port 18082
python3 benchmarks/probe_mcp.py http://127.0.0.1:18082/mcp --calls 20
```

For each gateway, record initialize/version negotiation, `tools/list` count and bytes, tool-call correctness and latency, per-tool authorization, session continuity, streaming, backend failure behavior, and CPU/RSS. Repeat with 1/10/100/1,000 tools and 1/10/100 backend servers. For compact meta-tool gateways, report the exposed definitions **and** discovery/invocation hops; raw `tools/list` bytes alone would reward a gateway for hiding work elsewhere. `probe_mcp.py` is a sequential correctness/latency smoke, not a concurrency benchmark.

## Current boundary

Tasmanian Cloud's `edge-lb` build currently fails in `bedrock` before a runnable binary is produced. Biota's `biota-http-server` package builds separately, so `biota-proxy/` is the executable candidate used here. No localhost smoke result should be compared with a vendor-published multi-machine performance figure.

The current Ptyktos path is Conduit federation behind Syndesi, with Telos authorization and Agent Loop execution. An end-to-end run of that composed API-gateway boundary is still a required measurement; the passing Conduit tests are transport/federation evidence, not a complete gateway benchmark.

The crosswalk includes [agentgateway/benchmarks](https://github.com/agentgateway/benchmarks), [MCP Gateway](https://github.com/MikkoParkkola/mcp-gateway), [MCPJungle](https://github.com/mcpjungle/MCPJungle), and [Supergateway](https://github.com/supercorp-ai/supergateway). Their results remain separate from HTTP gateway rankings because they measure inference or MCP transport/catalog behavior rather than the same reverse-proxy workload.

## Transport and resource measurement

Biota depends on the local `transport` crate. Its native suite currently passes the real UDP authenticated/encrypted transport tests, including handshake, peer identity, encrypted data, ACK clearing, retransmission, and congestion-window behavior (`cargo test --all-targets` in `/home/ctown/projects/transport`: 162 passed). The crate's `transport::quic` is a custom QUIC-like authenticated UDP transport; it must not be reported as HTTP/3 until an H3 request layer exists. The crosswalk therefore keeps `transport-native` and `transport-quic` as separate planes.

Sample a gateway process during any Nighthawk or k6 run:

```sh
python3 benchmarks/sample_process.py <pid> --interval 0.25 --output benchmarks/results/<run>-resources.json
```

For containers, collect the equivalent `docker stats --no-stream` record with the image digest and CPU limit. Report CPU as percent of one core and RSS in bytes, alongside achieved RPS and tail latency.
