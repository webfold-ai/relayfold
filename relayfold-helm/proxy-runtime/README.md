# relayfold-proxy-runtime

The deployable reverse-proxy binary: `biota-http-server` terminates HTTP and TLS, and
`relayfold::Registry` decides which backend every request goes to.

Routes come from a **config file that reloads without a restart**. A customer can add a
route, move a route onto a different backend pool, add or remove a backend, or revoke an
`allow` tuple — and the running proxy serves the new table after a `SIGHUP` or a file
change, with no dropped connection and no restart.

```
PROXY_CONFIG=/etc/relayfold/proxy.yaml LISTEN_ADDR=0.0.0.0:8080 relayfold-proxy
```

| | |
|---|---|
| config file | `PROXY_CONFIG`, default `./proxy-runtime.yaml`. YAML, or JSON with the same schema. |
| listen address | the file's `listen`, overridden by `LISTEN_ADDR`. |
| reload | `kill -HUP <pid>`, or edit the file — a 250 ms stat poll catches both. |
| probes | `GET /healthz`, `GET /readyz`. Never forwarded upstream, never agent-gated. |

Those two environment variables are the whole environment surface. Every other knob is in
the file: a knob that exists in two places is a knob that will be set in one of them and
then be the wrong one.

---

## The schema

The shape is **routes name pools, pools hold backends**.

- A **route** is a matching rule — a path prefix, optionally narrowed to a `Host` — and says
  which pool serves it.
- A **pool** is a named set of backends plus a load-balancing algorithm.

Splitting them is what lets two routes share one backend set (`/api` and `/api/v2` both on
`backend`), and lets one backend set serve several routes without being written twice.

```yaml
version: 1
listen: "0.0.0.0:8080"

header_limit: 8192          # total bytes of request headers; enforced by the edge guards
load_balance: round_robin   # the default for every pool that does not name its own

routes:
  - match: { prefix: "/api" }
    upstream: backend
  - match: { prefix: "/api/v2", host: "api.example.com" }
    upstream: backend
  - match: { prefix: "/" }  # the catch-all: prefix "/" is the same as "*"
    upstream: fallback

pools:
  backend:
    load_balance: weighted  # optional; overrides the global default
    backends:
      - { host: "10.0.60.20", port: 8080, weight: 3 }
      - { host: "10.0.60.21", port: 8080, weight: 1 }
  fallback:
    backends:
      - { host: "10.0.60.21", port: 8080 }

health:
  path: "/healthz"
  interval_seconds: 10
  timeout_seconds: 2

tls:                        # optional; both paths or neither
  cert_pem_path: /etc/tls/fullchain.pem
  key_pem_path:  /etc/tls/privkey.pem

subject_header: x-forwarded-subject   # where `allow` reads the caller's identity from
```

### `routes[]`

| key | required | default | meaning |
|---|---|---|---|
| `match.prefix` | no | `/` | Path prefix. Must start with `/`. `*` is accepted and means `/`. |
| `match.host` | no | any | Restrict to one `Host`. Compared case-insensitively and without the port. |
| `upstream` | yes* | — | The pool that serves this route. |
| `strip_prefix` | no | `false` | Forward with the matched prefix removed. The proxy is transparent by default. |
| `hash_key` | no | `path` | What `consistent` hashes on: `path`, `client_ip`, or `header:<name>`. |
| `allow` | no | `[]` | Tuples that grant access. Empty is **open**. |

`*` — a route must either name a pool or carry inline `upstreams`.

**Prefix matching is on segment boundaries.** `/api` covers `/api` and `/api/users/42` but
**not** `/apix`, because `/apix` is a different resource and silently routing it into the
`/api` pool is the kind of mistake that stays invisible until it is a security incident.

**Resolution order** is by specificity, not file order: a host-constrained rule beats an
unconstrained one at the same prefix, a longer prefix beats a shorter one, and `/` is last.
Two routes with the same `(prefix, host)` are a config error, not a silently shadowed rule.

### `pools.<name>`

A pool is either a bare backend list or a mapping that can also carry `load_balance`:

```yaml
pools:
  backend:                              # bare list: no per-pool algorithm
    - { host: "10.0.60.20", port: 8080 }
  weighted-pool:
    load_balance: weighted
    backends:
      - { host: "10.0.60.21", port: 8080, weight: 4 }
```

`host` is required; `port` defaults to 80 (`http`) or 443 (`https`); `scheme` is `http`
(default) or `https`; `weight` defaults to 1 and is read only by `weighted`.

`load_balance` is one of `round_robin` (default) · `random` · `weighted` · `consistent` ·
`healthiest`. It may be set per pool or globally at the top level; the pool's wins.

### `health`

`path` (default `/healthz`), `interval_seconds` (default 10), `timeout_seconds` (default 2).
Every backend in every pool is GETted; the verdict goes back through `Registry::set_health`.
A health round that raced a reload is **discarded**, because the indices it snapshotted no
longer line up with the live backend set.

### The inline shorthand

A route may carry its own backends instead of naming a pool:

```yaml
routes:
  - path: /api
    upstreams:
      - { url: "http://10.0.60.20:8080", weight: 3 }
      - { url: "https://10.0.60.21" }
```

The parser desugars this into a pool named after the route. It is one line shorter for the
common one-route-one-pool case, and the reload path treats the generated pool like any
other — including the diff. A route may do this **or** name a pool, never both.

### `allow`

```yaml
routes:
  - match: { prefix: "/api" }
    upstream: backend
    allow:
      - { subject: tenant-a, relation: may_use, object: /api }
      - { subject: "*",        relation: may_use, object: "*" }   # everyone, everything
```

The caller identity is the `subject_header` header if present, else the peer IP, else
`"anonymous"`. A route with **no** `allow` rules is open; a route **with** rules is closed
unless the caller satisfies at least one. Removing a rule on reload revokes it — the
relationship store is rebuilt, so a revoked tuple cannot keep granting.

> The subject header is only as trustworthy as whatever put it there. A deployment that does
> not scrub it at the edge should key subjects on `client_ip` and per-network subjects rather
> than on a client-supplied header.

### Validation

Unknown keys are a hard error everywhere (`deny_unknown_fields`) — a config key that is
silently ignored is a key that silently does not work. A write-then-rename is detected; a
`ConfigMap` symlink swap is detected; an empty file is an error rather than "a config with no
routes", which is a very different thing.

---

## Reload

Two triggers, one code path:

- **`kill -HUP <pid>`** — the operator's explicit "reload now".
- **the file changed** — a 250 ms stat poll on `(mtime, length, inode)`.

The watch is a stat poll rather than inotify on purpose. The requirement is a 250 ms
debounce, which a poll at that interval satisfies exactly; and a poll that stats the resolved
path sees the two ways this file actually changes in production — an editor's
write-then-rename, and a Kubernetes `ConfigMap` update, which is a *symlink swap* that
inotify on the watched path does not report at all. The cost is four `stat`s a second on one
file.

### A reload is a diff, not a rebuild

This is the part that matters, and it is why the runtime holds a `Registry` rather than
reconstructing a router:

- Each pool's backend set is **reconciled in place** (`Backends::reconcile`). A backend
  present in both the old and the new config keeps its slot, its measured latency, its
  active-health verdict and its round-robin position. **A reload cannot resurrect a backend
  the health prober just took out.**
- The registry *shell* — the pool→backends map and the relationship store — is rebuilt, which
  is how a removed route stops matching and a removed `allow` tuple stops granting.
- A reload whose diff is empty changes nothing at all: no swap, no generation bump, no log
  line beyond `no change`.

The log says what changed, by name:

```
[relayfold-proxy] reloaded on SIGHUP: 1 added, 1 changed, 0 removed, 1 unchanged
[relayfold-proxy]   pool backend: +1 -0 (~0 reweighted)
```

### A bad config does not take traffic down

A reload that fails to parse or validate logs the error and **discards the file**; the
last-good table keeps serving. A typo must not 502 every request in the fleet. At *startup*
there is no last-good table, so a broken or missing config exits non-zero instead — a failed
deploy, rather than a silent misroute.

---

## Control plane

`relayfold-control` can own one pool's backend set:

```yaml
control_plane:
  url: "http://relayfold-control:8080"
  pool: backend         # exactly one pool; every other pool is the file's alone
  refresh_secs: 15
```

The runtime polls `GET {url}/instances`, filters to the healthy addresses, and — only when
that set has actually changed — rewrites that pool's `backends` and pushes it through the
same diff a file reload uses. An unreachable control plane, or one reporting zero healthy
instances, leaves the current pool alone; a zero-healthy answer is never allowed to empty a
pool that is serving traffic.

---

## What is where

| file | owns |
|---|---|
| `src/config.rs` | the schema, the parser, validation |
| `src/routes.rs` | the live table: `Registry` per pool, resolution, authorize, pick, forward, diff-and-patch |
| `src/reload.rs` | the SIGHUP listener and the file watch |
| `src/balancer.rs` | the load-balancing contract tests, over the real YAML schema |
| `src/edge_guards.rs` | header ceiling and the agent gate |
| `src/main.rs` | wiring: config → table → reloaders → `Router` |

The load-balancing policy and the health-checking policy are `relayfold::LoadBalance` and
`Backends`; this crate supplies them with a table and a clock and does not reimplement them.
