# relayfold-proxy-runtime — what is wired

Scoped note for agents working in this crate. Read this before editing `src/main.rs` or
`src/config.rs`.

## The routing layer is `relayfold::Registry`, not `biota_http_server::Proxy`

`biota-http-server`'s `Proxy` is a fork of the same route→backend registry with an HTTP
client bolted on, and **its own `forward_raw` matches routes by EXACT `uri.path()`** with a
`*` fallback. That is why the pre-2026-10 runtime could only express one catch-all upstream
and needed a new pod for every change. A file-driven table needs prefix routes, so this crate
resolves the route itself and then asks the registry which backend:

```
request path ──▶ RouteTable::resolve (longest prefix, host-constrained)   [routes.rs]
             ──▶ RouteTable::is_allowed (Registry::relationships().check) [routes.rs]
             ──▶ Registry::pick_with(pool, algo, hash_key)                [relayfold]
             ──▶ biota Client forward, latency fused into HealthEstimator  [routes.rs]
```

`biota_http_server::Router`/`serve` still terminates HTTP and TLS and serves the probes, and
`biota_http_server::Client` still does the forwarding. Do not introduce a second pick path:
there is exactly one, and `balancer.rs` holds its tests.

**The registry's keys are POOL NAMES, not route prefixes.** `Registry::upstream(pool, u)` is
called once per backend. Route resolution is a purely local prefix scan over
`RouteTable::routes`; `Registry` never sees a prefix. That split is what lets two routes
share one backend set.

## Reload is a diff, and it must stay one

`RouteTable::apply` grafts each pool's live `Backends` across from the old registry —
`Registry::backends_for` returns a clone, and a `Backends` clone shares its `Arc`s — then
reconciles. **A backend that survives a reload keeps its health verdict and its round-robin
position.** Rebuilding the table wholesale would resurrect every backend the health prober
just took out, on every config change.

The registry *shell* is rebuilt deliberately: that is how a removed route stops matching and
a removed `allow` tuple stops granting.

Two tests pin this and must stay green:
`routes::tests::a_surviving_backend_keeps_its_health_verdict_across_a_reload` and
`balancer::set_health_takes_a_peer_out_of_rotation_and_a_reload_keeps_it_out`.

## Additions to `relayfold` itself

Purely additive, all patch-oriented, none touching `Backends::pick` or `set_health`:

| | |
|---|---|
| `Registry::backends_for` / `set_backends` | take and put a route's `Backends`, the graft pair |
| `Registry::remove_route` | drop a route on reload |
| `Registry::set_load_balance` | the in-place setter (`load_balance` is the consuming builder) |
| `Registry::forget` | drop tuples mentioning an entity |
| `Registry::pick_with` | `pick` with a per-call algorithm, which is what makes a **per-pool** algorithm real — one registry cannot carry five algorithms |
| `Backends::reconcile` → `ReconcileOutcome` | the in-place backend diff that preserves per-backend state |

## Environment surface is two variables

`PROXY_CONFIG` (path, default `./proxy-runtime.yaml`) and `LISTEN_ADDR` (overrides the
file's `listen`). Nothing else. The old `UPSTREAMS` / `UPSTREAM_HOST` / `UPSTREAM_PORT` /
`LOAD_BALANCE` / `HEALTH_CHECK_*` / `TLS_*_PEM_PATH` / `RELAYFOLD_CONTROL_URL` env surface
is **gone**: a knob that exists in two places is a knob that will be set in one of them and
then be the wrong one. `TLS_*` and the control plane moved into the file's `tls:` and
`control_plane:` blocks. The Helm chart's env block must be updated to match.

## Bad config never takes traffic down

A reload that fails to parse or validate logs and **discards**; the last-good table keeps
serving. At startup there is no last-good table, so a broken or missing config exits
non-zero. Do not "helpfully" fall back to defaults or to environment variables — that turns a
failed deploy into a silent misroute.

## The watch is a 250 ms stat poll, not inotify

On `(mtime, length, inode)`, so it sees an editor's write-then-rename **and** a Kubernetes
`ConfigMap` symlink swap, which inotify on the watched path does not report. A transition to
*absent* deliberately does not fire: a file the editor briefly unlinked is not a config, and
reloading it would only produce an error nobody caused.

## Gotchas that cost time here

- **`req.uri().path_and_query()` returns `&str`**, not `Option` — no `.map().unwrap_or()`.
- **An `RwLockReadGuard` held across an `.await` makes the future non-`Send`** and
  `tokio::spawn` rejects it. Scope every guard in a block.
- **`serde_yaml` reads an empty document as `null`**, which deserializes into a valid
  all-defaults `ProxyConfig` (every field defaults). `config::parse` rejects empty text
  explicitly for this reason — do not remove that check.
- **`Registry` cannot enumerate its keys.** They are opaque coordinates. The pool names are
  tracked in `RouteTable::pool_names`; that is why the health prober can iterate at all.
- **`PoolCfg` is an `untagged` enum**, so an unknown key in a pool block is rejected but the
  message says "did not match any variant" rather than naming the key. Known cost of
  accepting both the bare-list and mapping forms.
