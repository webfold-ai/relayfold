//! The load-balancing contract for this runtime, and where it is tested.
//!
//! There is deliberately **one** pick on the request path, and it is
//! `relayfold::Registry::pick_with` → `relayfold::Backends::pick`, called from
//! [`crate::routes::RouteTable::forward`] once per request, after route resolution and the
//! `allow` check, before the forward. biota's `LoadBalance` and biota's `Proxy` are not on the
//! path at all (`RouteTable` forwards on its own shared `Client`). So this module holds no
//! second pick path — it holds the tests that pin the behaviour the runtime promises, because
//! those promises are exactly what a config change or a dependency bump can silently break:
//!
//! - `RoundRobin` distributes across a pool's backends, per pool.
//! - `Consistent` is sticky for one known hash key, and still spreads a population of keys.
//! - Two pools may run DIFFERENT algorithms at the same time (`pick_with` is what makes that
//!   real, since `Registry`'s own `lb` is registry-wide).
//! - `set_health` — what `HEALTH_CHECK_INTERVAL_SECS` drives — takes a peer out of rotation
//!   without a restart, and a reload must not resurrect it.
//!
//! Every table here is built by parsing a real YAML document through [`crate::config`], so the
//! tests exercise the schema the chart renders rather than a private builder.

#![cfg(test)]

use crate::config::{self, ProxyConfig};
use crate::routes::RouteTable;
use relayfold::LoadBalance;
use std::path::Path;

fn cfg(yaml: &str) -> ProxyConfig {
    let c = config::parse(Path::new("proxy-runtime.yaml"), yaml).expect("test config parses");
    c.validate().expect("test config validates");
    c
}

/// Two backends in pool `backend` (algorithm `lb`), served by routes `/api` and the wildcard.
fn two_upstreams(lb: &str) -> RouteTable {
    RouteTable::build(&cfg(&format!(
        r#"
routes:
  - match: {{ prefix: "/api" }}
    upstream: backend
  - match: {{ prefix: "/" }}
    upstream: backend
pools:
  backend:
    load_balance: {lb}
    backends:
      - {{ host: "10.0.0.1", port: 8080, weight: 1 }}
      - {{ host: "10.0.0.2", port: 8080, weight: 1 }}
"#
    )))
    .expect("route table builds")
}

/// `lb.backends_of(pool)` — the pool's backends as `scheme://host:port`, in `set_health` index
/// order. Used to assert on distribution without hard-coding the pick order everywhere.
fn members(t: &RouteTable, pool: &str) -> Vec<String> {
    t.backends_of(pool)
}

/// N picks through the REAL data path — resolve the route, then `pick_with` with that pool's
/// algorithm — as `host:port` strings, in pick order.
fn picks(t: &RouteTable, route: &str, hash_key: &str, n: usize) -> Vec<String> {
    let meta = t.resolve(route, None).expect("route resolves");
    let pool = meta.pool.clone();
    (0..n)
        .map(|_| {
            t.registry()
                .pick_with(&pool, t.algo_for(&pool), hash_key)
                .expect("pool has backends")
                .0
                .url_for("")
        })
        .collect()
}

#[test]
fn round_robin_distributes_two_upstreams() {
    let t = two_upstreams("round_robin");
    let got = picks(&t, "/api", "/api/k", 4);
    let (a, b) = (&members(&t, "backend")[0], &members(&t, "backend")[1]);
    assert_eq!(
        got,
        vec![a.clone(), b.clone(), a.clone(), b.clone()],
        "round robin must alternate the two backends: {got:?}"
    );
    assert_eq!(
        got.iter().filter(|u| *u == a).count(),
        2,
        "both backends must get half the picks"
    );
}

/// Two pools, two DIFFERENT algorithms, live at once: `pick_with` is the only reason that is
/// expressible, because `Registry`'s own `lb` is one value for the whole registry.
#[test]
fn two_pools_run_two_different_algorithms() {
    let t = RouteTable::build(&cfg(r#"
routes:
  - match: { prefix: "/rr" }
    upstream: rr_pool
  - match: { prefix: "/consistent" }
    upstream: sticky_pool
pools:
  rr_pool:
    load_balance: round_robin
    backends:
      - { host: "10.0.0.1", port: 8080 }
      - { host: "10.0.0.2", port: 8080 }
  sticky_pool:
    load_balance: consistent
    backends:
      - { host: "10.0.1.1", port: 8080 }
      - { host: "10.0.1.2", port: 8080 }
"#))
    .unwrap();
    assert_eq!(t.algo_for("rr_pool"), LoadBalance::RoundRobin);
    assert_eq!(t.algo_for("sticky_pool"), LoadBalance::Consistent);

    let rr = picks(&t, "/rr/x", "/rr/x", 4);
    assert_eq!(
        rr.iter().collect::<std::collections::BTreeSet<_>>().len(),
        2,
        "the round_robin pool must rotate: {rr:?}"
    );
    let sticky = picks(&t, "/consistent/y", "user-9", 4);
    assert_eq!(
        sticky
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "the consistent pool must send one key to one backend: {sticky:?}"
    );
    assert!(
        sticky[0].starts_with("http://10.0.1."),
        "the sticky pool must not hand out the other pool's backends: {sticky:?}"
    );
}

/// `Weighted` with unequal weights over the same two-backend shape: both are reachable and the
/// heavier one is not starved.
#[test]
fn weighted_reaches_both_peers() {
    let t = RouteTable::build(&cfg(r#"
routes:
  - match: { prefix: "/" }
    upstream: w
pools:
  w:
    load_balance: weighted
    backends:
      - { host: "10.0.0.1", port: 8080, weight: 3 }
      - { host: "10.0.0.2", port: 8080, weight: 1 }
"#))
    .unwrap();
    let got = picks(&t, "/x", "/x", 400);
    let heavy = members(&t, "w")[0].clone();
    let a = got.iter().filter(|u| *u == &heavy).count();
    let b = got.len() - a;
    assert!(
        a > 0 && b > 0,
        "weighted must reach both backends: a={a} b={b}"
    );
    assert!(
        a > b,
        "the 3:1 weight must favour the heavier backend: a={a} b={b}"
    );
}

/// The deliverable's second half: `Consistent` with a KNOWN key. Sticky across many picks, and
/// deterministic — a second table built from the same config makes the same decision, because
/// the answer comes from the hash ring over the backend coordinates, not from per-instance
/// state.
#[test]
fn consistent_is_sticky_for_a_known_key_and_spreads_across_keys() {
    let t = two_upstreams("consistent");

    const KNOWN_KEY: &str = "session-abc";
    let first = picks(&t, "/api", KNOWN_KEY, 1).remove(0);
    assert!(
        members(&t, "backend").contains(&first),
        "the known key must land on a configured backend, got {first}"
    );
    for _ in 0..50 {
        assert_eq!(
            picks(&t, "/api", KNOWN_KEY, 1).remove(0),
            first,
            "consistent hashing must send one key to one backend every time"
        );
    }

    // A freshly built table (no shared round-robin cursor, no shared health flags) agrees.
    let rebuilt = two_upstreams("consistent");
    assert_eq!(
        picks(&rebuilt, "/api", KNOWN_KEY, 1).remove(0),
        first,
        "the consistent ring must be a pure function of (key, backends)"
    );

    // And it still spreads: distinct keys do not all collapse onto one backend.
    let spread: std::collections::BTreeSet<String> = (0..64)
        .map(|i| picks(&t, "/api", &format!("key-{i}"), 1).remove(0))
        .collect();
    assert_eq!(
        spread.len(),
        2,
        "consistent hashing must spread a key population over both backends: {spread:?}"
    );
}

/// `set_health` is what the health-check interval drives. A backend marked down stops receiving
/// picks immediately, and a config reload that keeps that backend must not resurrect it.
#[test]
fn set_health_takes_a_peer_out_of_rotation_and_a_reload_keeps_it_out() {
    let mut t = two_upstreams("round_robin");
    let down = members(&t, "backend")[0].clone();
    let up = members(&t, "backend")[1].clone();

    t.set_health("backend", 0, false);
    let got = picks(&t, "/api", "/api/k", 8);
    assert!(
        got.iter().all(|u| u == &up) && !got.contains(&down),
        "a backend marked down must take no traffic: {got:?}"
    );

    // Reload with the SAME backends: the surviving backend keeps its slot AND its verdict.
    t.apply(&cfg(r#"
routes:
  - match: { prefix: "/api" }
    upstream: backend
  - match: { prefix: "/" }
    upstream: backend
pools:
  backend:
    load_balance: round_robin
    backends:
      - { host: "10.0.0.1", port: 8080, weight: 1 }
      - { host: "10.0.0.2", port: 8080, weight: 1 }
"#));
    let after = picks(&t, "/api", "/api/k", 8);
    assert!(
        after.iter().all(|u| u == &up),
        "a reload must not resurrect a backend the prober took out: {after:?}"
    );
}

/// A pool that fails OPEN: when every backend is marked down, `pick_with` still answers rather
/// than returning `None` and turning a health-checker bug into a hard outage.
#[test]
fn an_all_down_pool_still_picks() {
    let t = two_upstreams("round_robin");
    t.set_health("backend", 0, false);
    t.set_health("backend", 1, false);
    assert!(
        picks(&t, "/api", "/api/k", 1).len() == 1,
        "pick must fail open when every backend is marked down"
    );
}
