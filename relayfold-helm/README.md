# relayfold-helm

The Kubernetes-deployment gap from the zentinel SWOT: `zentinel-helm` is a Go-template Helm
chart deploying their proxy; we had no deployment chart at all. "But in Rust"
is honored where it actually applies — the chart artifact itself is standard Helm YAML (that's
the only format `helm install` consumes), and the logic beyond plain templating is real Rust,
not a Go template.

## What's here

- **`chart/relayfold-proxy/`** — a real Helm chart (Deployment, Service, HorizontalPodAutoscaler,
  Ingress). `helm lint` clean; `helm template` output validated against the real Kubernetes API
  schema with `kubeconform` (4/4 resources valid with autoscaling+ingress enabled, 2/2 on
  defaults).
- **`proxy-runtime/`** — `relayfold-proxy`, a minimal standalone reverse-proxy binary wiring
  the real `Router`/`Proxy` into a runnable service. **This didn't exist before**: the HTTP
  server ships as a library only, with no binary that calls `serve()`. `/healthz`/`/readyz` for k8s probes, catch-all `Proxy::handle`
  forwarding to one configured upstream. Verified end-to-end against a real upstream over a real
  socket (see tests).
- **`chart-tool/`** — `relayfold-chart-tool`, a Rust CLI validating `values.yaml` against the
  real env-var schema `proxy-runtime` reads (not an invented schema), and rendering typed
  `dev`/`staging`/`prod` presets. Uses `wirefold::codec::syntax` (`Surface::Yaml`), not
  `serde_yaml` — this workspace's standing rule.

## Verified, not asserted

```
relayfold-chart-tool render prod | tee /tmp/prod-values.yaml
relayfold-chart-tool validate /tmp/prod-values.yaml   # -> valid
helm template prod-release chart/relayfold-proxy -f /tmp/prod-values.yaml | kubeconform -summary
# -> Valid: 2, Invalid: 0, Errors: 0
```

The full chain — Rust preset render → Rust schema validation → real Helm templating → real
Kubernetes API schema validation — was run for real this session, not asserted from reading the
code. `helm` and `kubeconform` binaries were downloaded fresh (not present in the base
environment) to make this verification real rather than skipped.

## Honestly out of scope

- Multi-upstream / multi-route configuration — `proxy-runtime` registers exactly one upstream
  under the wildcard route; a real multi-route setup needs its config surface extended past env
  vars.
- cert-manager integration (the Ingress template accepts a pre-existing TLS secret name; it
  doesn't request one).
- PodDisruptionBudget, NetworkPolicy, ServiceMonitor (Prometheus Operator) — none included.
- A real container image was not built: `proxy-runtime/Dockerfile` is real (multi-stage,
  `rust:1.82-slim` → `debian:bookworm-slim`), but this session's build environment was at 94%
  disk and load average 10.5 with ~326Mi free RAM when this was written — a full-workspace
  `cargo build --release` inside a fresh container (compiling the whole dependency tree from
  scratch) risked
  tipping a heavily-loaded shared machine into OOM, so it wasn't attempted. No registry
  credentials exist in this environment either. `image.repository`/`image.tag` in `values.yaml`
  are placeholders (`ghcr.io/webfold-ai/relayfold-proxy`) pointing at an image that doesn't exist
  yet — build it with `docker build -f proxy-runtime/Dockerfile -t <tag> .` from the repo root
  once the target environment has headroom.
