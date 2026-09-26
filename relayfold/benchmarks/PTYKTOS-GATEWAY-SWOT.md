# Ptyktos API-gateway SWOT

This is the product boundary for the benchmark work. `relayfold` is the routing and relationship primitive; `biota` is the HTTP/Pingora-style data plane; `conduit` federates upstream MCP services; `syndesi` is the tool/API host and policy-facing surface; `telos` makes authorization decisions and receipts; `agent-loop` executes approved actions. MCP is one adapter workload inside this gateway, not the gateway definition.

## What was actually verified locally

| Component | Evidence | Gateway meaning |
|---|---|---|
| Biota HTTP server | `cargo test -p biota-http-server --lib`: 298 passed, 4 ignored | Real HTTP serving, routing, proxying, pooling, health checks, TLS, h2c, streaming, cache, graceful drain |
| Relayfold adapter | Nighthawk fixed offer against direct and Biota/Relayfold paths | Data-plane wiring and routing overhead can be measured; production `edge-lb` is still a separate build target |
| Agent Loop | `cargo test --lib --tests`: 29 passed | Deterministic action execution, local/full policy gate ordering, permission pause/resume, MCP dispatch seam |
| Telos | Rust workspace contains Cedar + ZEN + financial/budget decision engine and receipt crates | Authorization, budget, policy versioning, and evidence layer; not an HTTP listener by itself |
| Conduit | Native MCP stdio/HTTP client, catalog federation, tool calls, code-mode path | Upstream federation and protocol adapter; not a complete reverse proxy/control plane |
| Syndesi | README documents `gateway web-serve`, `gateway mcp-serve`, artifact API, federation, guardrail and route/scan tools | Closest current host for a user-facing gateway surface; requires explicit HTTP policy/tenant/control-plane hardening |

## SWOT against Zentinel-style gateway products

| | Ptyktos/Tasmanian ecosystem | Consequence |
|---|---|---|
| Strengths | Native Rust ownership across data plane, federation, policy, receipts, and agent execution; Biota already has tested proxy primitives; capability composition is local/self-hosted | Can make one request identity and policy receipt span HTTP, MCP, agent action, and backend call instead of bolting agents onto a generic proxy |
| Strengths | Conduit and Syndesi already cover MCP discovery/federation and HTTP/MCP surfaces; Telos and Agent Loop provide decision and execution seams | Stronger agent-aware gateway story than a standalone reverse proxy once the seams are wired into one runtime |
| Weaknesses | No single named API-gateway product currently owns listener, route/config API, tenant isolation, hot reload, policy middleware, metrics, and deployment packaging | Incumbents are easy to deploy and benchmark as one binary; Ptyktos currently requires composition and an explicit gateway host |
| Weaknesses | Security-agent catalog is broader than demonstrated runtime coverage: WAF/CRS, ModSecurity, bot/IP reputation, GraphQL/gRPC/WebSocket inspectors, SPIFFE, ClamAV, SOAP, image optimization, Lua/JS/WASM, and MQTT need capability-by-capability evidence | Do not claim parity from crate names or adjacent ecosystem code; mark each feature implemented, adapter-only, planned, or unsupported |
| Opportunities | Build a Rust gateway shell around Biota with Relayfold routing, a Conduit/Syndesi adapter plane, Telos authorization, and Agent Loop execution | One benchmarkable product boundary with HTTP, MCP, LLM, and agent routes sharing identity, policy, audit, and rate-limit state |
| Opportunities | Port the Zentinel control-plane/agent contract: versioned config, signed policy bundles, agent ABI/SDK, health/metrics, hot reload, rollback, Helm/Kubernetes deployment | Converts the current collection of capable repos into a deployable gateway instead of a benchmark harness around libraries |
| Threats | NGINX/Envoy/Kong/Tyk/APISIX/Pingap/Zentinel have mature config, observability, plugin, and operations surfaces | Raw RPS will not compensate for missing lifecycle, policy, and failure semantics in a production comparison |
| Threats | MCP gateways hide work behind meta-tools or discovery hops; agentgateway benchmarks are inference-specific and vendor topology-specific | Keep HTTP, MCP, and inference scorecards separate; compare completed task, authorization, streaming, and failure recovery—not only p99 or tool-list bytes |

## Required gateway scorecard

The next implementation target is a single gateway executable or service boundary with these tests:

1. HTTP proxy floor: direct, Biota/Relayfold, NGINX, HAProxy, Envoy, Pingap, Zentinel, then Kong/Tyk/APISIX where provisionable.
2. Policy tax: no policy, API key, JWT/OIDC, rate limit, audit/metrics, Telos decision, and the production combination.
3. Security agents: WAF, denylist, bot/reputation, GraphQL/gRPC/WebSocket, content scan, SPIFFE, and transform cases; unsupported means unsupported.
4. Lifecycle: route/config update under traffic, propagation time, rollback, backend failure/recovery, memory after repeated churn.
5. Agent/MCP: Conduit/Syndesi federation, per-tool authorization through Telos, session/streaming, backend crash/restart, and completed-task cost.
6. Inference: reproduce the Agentgateway benchmark shape with identical model servers; record TTFT, output tokens/s, queueing, completion, and policy overhead.

The current local Nighthawk runs prove only the first thin slice. They are useful wiring evidence, not a verdict on the gateway product.
