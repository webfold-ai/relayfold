# relayfold

Two cores you usually end up writing yourself, in one crate:

1. **Temporal relationships** — Zanzibar-style: "may this subject do this to that object?", with
   a validity window, a reason, and a weight.
2. **Upstream selection** — consistent-hash and weighted picking across a route's upstreams,
   gated on real active *and* passive health.

## Why this instead of the obvious alternative

A reverse proxy forwards traffic. The two things that are *not* forwarding are the things you
end up writing yourself, usually badly and usually twice:

- **Authorization as a graph.** "Does this subject hold this relation to this object?" is a
  question, and a config file cannot answer it. The Zanzibar shape is the standard answer:
  tuples with a validity window, so a relation can be *invalidated* rather than deleted, and a
  fact that was extracted by a pipeline has somewhere to put **why**.
- **Choosing an upstream.** Round-robin is easy; *sticky* selection, weights, and health that
  reflects what users are actually experiencing is not. Here the selection algorithm and the
  health estimate are separate objects, so a route can be weighted and health-aware at the same
  time.

## The relationship core

A `Tuple` is one stored relationship:

| field | what it is |
|---|---|
| `subject` / `relation` / `object` | the triple |
| `valid_from` | when it became true (the caller's unit — days or seconds — never interpreted here) |
| `invalid_at` | when it stopped being true; `None` means it still holds |
| `context` | why the edge exists (the same slot a caveat takes elsewhere) |
| `weight` | how much to trust it, beside the *why*; defaults to `1.0` |

Invalidation rather than deletion: a fact that was believed and then superseded is part of the
record, and a store that forgets it cannot answer "what did we think last Tuesday".

`Relationships` keeps the tuples in a coordinate map — the hash *is* the tuple, so "does this
hold" is O(1) — beside an enumerable copy, which is what makes `Expand` and `LookupSubjects`
possible. Those are the operations a graph *walk* is made of.

```rust
let mut rel = relayfold::Relationships::new();
rel.relate("alice", "member", "team-eng");          // holds now
rel.relate_at("bob", "member", "team-eng", 1_700_000_000);
rel.relate_with("carol", "member", "team-eng", "imported by pipeline X");
rel.relate_weighted("dave", "member", "team-eng", 0.25);
```

## The selection core

| type | what it is |
|---|---|
| `Upstream` | one candidate: an address and the coordinates it hashes on |
| `LoadBalance` | the algorithm: `RoundRobin`, `Random`, `Weighted`, `Consistent` |
| `HealthEstimator` | a latency estimator per upstream — merging an observation into it is commutative and order-free, so concurrent updates never coordinate on ordering |
| `Backends` | one route's upstream list plus the state its algorithm needs (the round-robin counter and the health estimates are `Arc`-shared, so clones share one rotation) |
| `Registry` | route registration + picking + the relationship store |
| `pick_index` | the selection itself: one hash, one solve, one pick |
| `coord_of` | the coordinate of any keyed value, so your own key type can be hashed the same way |

`Consistent` uses the request path as its hash key — session affinity by path. Real deployments
may want client-IP affinity instead; that is not configurable yet, and it is a stated scope
limit rather than a hidden one.

Health fails **open**: if every candidate is marked down, the algorithm's own pick still goes
through, because a stale health checker must not turn a real outage into a guaranteed refusal on
every request.

## Honest scope limits

- **No HTTP forwarding.** This crate is the data plane's *core*: which upstream, gated on what
  health, and which relationships hold. The request-forwarding driver (connection pooling,
  streaming bodies, the layer that ties them together) is not here.
- **The latency estimator is the simplest faithful instance.** It carries `(η, τ)` and folds an
  observation in as componentwise addition — order-free, so it is safe under concurrency — but
  each observation is unit-precision, so `μ` recovers a plain running mean rather than the full
  weighted-precision Kalman gain.
- **Affinity is by path**, not by client IP.

## Build

```sh
cargo build --release
```

## Integration

```toml
[dependencies]
relayfold = { path = "../relayfold" }
```

or from git:

```toml
[dependencies]
relayfold = { git = "https://github.com/webfold-ai/relayfold", rev = "<pin>" }
```

## License

SPDX: `Apache-2.0` (declared in `Cargo.toml`).
