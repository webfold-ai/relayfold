# relayfold

The data plane of a reverse proxy, in Rust: **temporal relationships**, **load balancing and
health**, a **fleet-management control plane**, a **crash-isolated agent protocol**, and the
**config converters** and **Helm chart** to deploy it.

## Why this instead of the obvious alternative

nginx, HAProxy and Envoy are excellent at forwarding traffic and are what most deployments run.
What they are not is a **library**: you configure them with files, and you cannot ask them a
question from your own process. The moment a proxy is part of a system — not a web server in
front of a web server — you start needing things a config file cannot give you:

| what you would otherwise build | what you use here |
|---|---|
| "may this subject do this to that object, and did it hold last Tuesday?" | `relayfold`: Zanzibar-style relationships with a validity window and a reason |
| "which upstream, and is it healthy right now?" | `relayfold`: consistent-hash selection, weighted policies, active + passive health estimation |
| "register this instance, push it a config, know who is alive" | `relayfold-control`: a signed, audited, compacted control plane |
| "run the security check in a process that can crash" | `relayfold-agents`: an agent protocol over a socket, so a bad agent cannot take the data plane down |
| "convert our nginx config" | `relayfold-convert`: nginx and HAProxy parsers that emit the route format |
| "deploy it" | `relayfold-helm`: a real Helm chart plus a standalone proxy binary |

Two properties the alternatives do not give you:

1. **The relationships are temporal and carry a reason.** A relation is invalidated, not
   deleted, and every edge has a context and a weight. A store that forgets a superseded fact
   cannot answer "what did we think last Tuesday", and a fact that was extracted by a pipeline
   needs somewhere to put *why* it was extracted.
2. **The control plane is auditable and refuses.** Every call is signed, the enrollment
   allowlist is closed by default, refusals are durable, and the log compacts to a snapshot
   that is verified against the tail on open. A config push is signed per target and the
   instance verifies it.

## Members

| Crate | Path | What it is |
|---|---|---|
| `relayfold` | [`relayfold/`](relayfold/) | Zanzibar-style temporal relationships + the load-balancing/health core |
| `relayfold-convert` | [`relayfold-convert/`](relayfold-convert/) | nginx/HAProxy config → the relayfold YAML route format |
| `relayfold-agents` | [`relayfold-agents/`](relayfold-agents/) | Crash-isolated external agent protocol + Rust SDK |
| `relayfold-control` | [`relayfold-control/`](relayfold-control/) | Fleet-management control plane (registry, heartbeat, config push) |
| `relayfold-bench` | [`relayfold-bench/`](relayfold-bench/) | Load-generation and latency harness for relayfold-based proxies |
| `relayfold-chart-tool` | [`relayfold-helm/chart-tool/`](relayfold-helm/chart-tool/) | Validates/renders the Helm chart's values.yaml |
| `relayfold-proxy-runtime` | [`relayfold-helm/proxy-runtime/`](relayfold-helm/proxy-runtime/) | Standalone reverse-proxy binary for the Helm chart to deploy |
| — | [`relayfold-helm/chart/`](relayfold-helm/chart/) | The Helm chart itself (not a Rust crate) |

Each member keeps its own README with crate-specific detail.

## Build

```sh
cargo build --workspace
```

The workspace's git dependencies (`servelet`, `transport`, `iotadb`, `wirefold`, `apeiron`) are pinned by
rev in each member's `Cargo.toml`, and cargo resolves the whole workspace graph, so every `cargo`
command needs to be able to fetch them.

Quickstart: `cargo test --workspace`, then `cargo run -p relayfold-bench --release -- / 10 8 name=host:port`
to load-test a running server.

## Integration

```toml
[dependencies]
relayfold = { path = "../relayfold/relayfold" }
```

or from git:

```toml
[dependencies]
relayfold = { git = "https://github.com/webfold-ai/relayfold", rev = "<pin>" }
```

## License

SPDX: `Apache-2.0` — declared in each member's `Cargo.toml`.
