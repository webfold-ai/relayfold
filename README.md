# relayfold (monorepo)

Ptyktos's relayfold family, consolidated into one Cargo workspace. Extracted
originally out of the fold monorepo; the split-out satellite repos have now
been folded back in here as workspace members. `relayfold-bench` (load-gen /
latency harness) stays a separate repo — it benchmarks this workspace from
the outside and isn't part of the build.

## Members

| Crate | Path | What it is |
|---|---|---|
| `relayfold` | [`relayfold/`](relayfold/) | Zanzibar-style temporal relationships + Pingora-shaped load-balancing/health core |
| `relayfold-convert` | [`relayfold-convert/`](relayfold-convert/) | nginx/HAProxy config → relayfold YAML route format converters |
| `relayfold-agents` | [`relayfold-agents/`](relayfold-agents/) | Crash-isolated external agent protocol + Rust SDK |
| `relayfold-control` | [`relayfold-control/`](relayfold-control/) | Fleet-management control plane (registry, heartbeat, config push) |
| `relayfold-chart-tool` | [`relayfold-helm/chart-tool/`](relayfold-helm/chart-tool/) | Validates/renders the Helm chart's values.yaml |
| `relayfold-proxy-runtime` | [`relayfold-helm/proxy-runtime/`](relayfold-helm/proxy-runtime/) | Standalone reverse-proxy binary for the Helm chart to deploy |
| — | [`relayfold-helm/chart/`](relayfold-helm/chart/) | The Helm chart itself (not a Rust crate) |

Each member keeps its own README with crate-specific detail.

## Build

```sh
cargo build --workspace
```

`relayfold-control` pulls `servelet`, whose manifest takes `hashtrinity` from the private GitLab
mirror (`gitlab.tas.twn.network`), so the machine running cargo needs git access to it — see
[`relayfold-control/README.md`](relayfold-control/README.md#build-and-test). Without it `cargo` fails
at dependency resolution, before compiling anything, and because cargo resolves the whole workspace
graph that applies to every `cargo` command in the workspace, not only to that member. With it, on 2026-10-11 the build box ran
`cargo test --workspace` green; the `modelfold` `src/sparse_fft.rs` (`E0583`) breakage this file
used to describe did not reproduce (`relayfold-proxy-runtime` compiled, with two unused-variable
warnings).

## Integration

Pin via Cargo:

```toml
[dependencies]
relayfold = { path = "../relayfold/relayfold" }
```

Ptyktos substrate: crates.io: `relayfold`

## License

SPDX: `MIT`. Ptyktos substrate (fold-extracted).
