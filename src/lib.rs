//! `relayfold` — the temporal Zanzibar-style relationship store, and the Pingora-shaped
//! upstream/load-balancing/health-check core, extracted for real out of `bedrock`.
//!
//! **Extraction history, disclosed precisely**: `bedrock/src/native_lib/proxy.rs` claims (in its
//! own module doc) to be `include!`'d into `native_lib.rs` — that's stale; no such `include!`
//! exists anymore, and that file is dead, unreferenced code. The REAL, live implementation lives
//! directly inline in `bedrock/src/native_lib.rs` (a 5,559-line file: a full hand-rolled HTTP
//! client + server + generic router, THEN this data-plane layer on top). This crate is that real,
//! live code — `Tuple`/`Relationships` (lines 3173–3446 there) and `Upstream`/`LoadBalance`/
//! `HealthEstimator`/`Backends` (3447–3712) — moved verbatim, not reimplemented.
//!
//! **What did NOT come along, and why**: bedrock's `Proxy` (the full request-forwarding driver,
//! `ProxyHttp` trait, real `handle`/`handle_with`) needs bedrock's own hand-rolled HTTP `Client`
//! (with real connection pooling) plus `Request`/`Response`/`HeaderMap`/etc. — and beyond that,
//! cross-references into `crate::httpsrv::{httpx,actix}` (cookies/multipart/HTTP2/SSE),
//! `crate::platform::service` (a tower-equivalent Layer/Service trait system), and
//! `crate::runtime::{cache,graceful,metrics,cluster}` (a separate 4,821-line file). That's a
//! genuinely large, separate, multi-module extraction — not rushed here. What ships in THIS
//! crate is the self-contained core: given a set of upstreams and an algorithm, which one to
//! pick, gated on real active+passive health.
//!
//! **`Registry`** is new — a thin wrapper (route → `Backends`, plus `Relationships` and a
//! `LoadBalance` algorithm) mirroring the route/upstream/pick/allow surface bedrock's `Proxy`
//! exposes MINUS its HTTP-forwarding half (`handle`/`handle_with`/`cache`/`spawn_health_checks`,
//! all of which need the real HTTP client this crate deliberately doesn't bring along). The
//! original tests were written against `Proxy`'s full surface; they're ported here against
//! `Registry` instead — behavior-identical for everything that doesn't touch HTTP.

use hashtrinity::{Coord, CoordMap, PHI};
use std::sync::Arc;

// ── Zanzibar/SpiceDB-style temporal relationship store ──────────────────────────────────────

/// One stored relationship, with the validity window Zanzibar leaves to its revision log.
///
/// The coordinate map answers "does this tuple hold" in O(1) and cannot answer anything else —
/// the hash is the tuple. Keeping the literal triple beside it is what makes `Expand` and
/// `LookupSubjects` possible, and those are the operations a graph *walk* is made of.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuple {
    pub subject: String,
    pub relation: String,
    pub object: String,
    /// When this became true. Days or seconds — the caller's unit, never interpreted here.
    pub valid_from: i64,
    /// When it stopped being true. `None` means it still holds.
    ///
    /// Invalidation rather than deletion: a fact that was believed and then superseded is part
    /// of the record, and a store that forgets it cannot answer "what did we think last Tuesday".
    pub invalid_at: Option<i64>,
    /// Why this edge exists — SpiceDB carries the same thing as caveat context.
    ///
    /// Provenance needs it: an extraction pipeline's reflection step produces a *reason* a fact
    /// was added or changed, and a reason with nowhere to live is a reason that gets dropped.
    pub context: String,
    /// How much to trust this edge — sibling to `context`: "how much" beside "why". Defaults to
    /// `1.0` (full trust) so every existing caller of `relate`/`relate_at`/`relate_with` is
    /// unaffected. A source-weighted read folds this across every episode supporting a fact
    /// instead of treating "supported" as a boolean.
    pub weight: f64,
}

impl Tuple {
    #[must_use]
    pub fn holds_at(&self, when: i64) -> bool {
        when >= self.valid_from && self.invalid_at.is_none_or(|end| when < end)
    }

    #[must_use]
    pub fn holds_now(&self) -> bool {
        self.invalid_at.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct Relationships {
    tuples: CoordMap<u64, bool>,
    /// The enumerable face. A `Vec` and not an index-per-field: relationship sets are small per
    /// object, the scan is linear in a few thousand at worst, and one representation that cannot
    /// disagree with itself beats three that can.
    triples: Vec<Tuple>,
}

impl Relationships {
    #[must_use]
    pub fn new() -> Self {
        Relationships {
            tuples: CoordMap::coordinate(),
            triples: Vec::new(),
        }
    }

    pub fn relate(
        &mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
    ) {
        self.relate_at(subject, relation, object, 0);
    }

    /// Relate, recording when it became true.
    ///
    /// Re-relating a tuple that was invalidated revives it: the same statement becoming true
    /// again is one tuple with a new window, not a second row that would double-count in every
    /// enumeration.
    pub fn relate_at(
        &mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
        valid_from: i64,
    ) {
        let (s, r, o) = (subject.as_ref(), relation.as_ref(), object.as_ref());
        self.tuples.mark(relationship_coord(s, r, o));
        match self
            .triples
            .iter_mut()
            .find(|t| t.subject == s && t.relation == r && t.object == o)
        {
            Some(existing) => {
                existing.invalid_at = None;
                existing.valid_from = valid_from;
            }
            None => self.triples.push(Tuple {
                subject: s.to_string(),
                relation: r.to_string(),
                object: o.to_string(),
                valid_from,
                invalid_at: None,
                context: String::new(),
                weight: 1.0,
            }),
        }
    }

    /// Relate, carrying the reason the edge exists.
    pub fn relate_with(
        &mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
        valid_from: i64,
        context: impl Into<String>,
    ) {
        let (s, r, o) = (
            subject.as_ref().to_string(),
            relation.as_ref().to_string(),
            object.as_ref().to_string(),
        );
        self.relate_at(&s, &r, &o, valid_from);
        if let Some(t) = self
            .triples
            .iter_mut()
            .find(|t| t.subject == s && t.relation == r && t.object == o)
        {
            t.context = context.into();
        }
    }

    /// Relate, carrying both the reason and how much to trust it. `weight` is the caller's own
    /// scale (a raw source-reliability score, a calibrated probability, whatever the domain
    /// uses) — this store folds it, never interprets it.
    pub fn relate_weighted(
        &mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
        valid_from: i64,
        context: impl Into<String>,
        weight: f64,
    ) {
        let (s, r, o) = (
            subject.as_ref().to_string(),
            relation.as_ref().to_string(),
            object.as_ref().to_string(),
        );
        self.relate_at(&s, &r, &o, valid_from);
        if let Some(t) = self
            .triples
            .iter_mut()
            .find(|t| t.subject == s && t.relation == r && t.object == o)
        {
            t.context = context.into();
            t.weight = weight;
        }
    }

    /// The stored tuple, if present — for reading its window and its `context`.
    #[must_use]
    pub fn tuple(&self, subject: &str, relation: &str, object: &str) -> Option<&Tuple> {
        self.triples
            .iter()
            .find(|t| t.subject == subject && t.relation == relation && t.object == object)
    }

    /// Mark a tuple no longer true as of `at`. Returns whether it was holding.
    ///
    /// The coordinate is unmarked too, so [`check`](Self::check) stays the "true right now"
    /// question and cannot answer yes for something retracted — the trap a temporal field on the
    /// side would have set.
    pub fn invalidate(
        &mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
        at: i64,
    ) -> bool {
        let (s, r, o) = (subject.as_ref(), relation.as_ref(), object.as_ref());
        let Some(t) = self
            .triples
            .iter_mut()
            .find(|t| t.subject == s && t.relation == r && t.object == o && t.invalid_at.is_none())
        else {
            return false;
        };
        t.invalid_at = Some(at);
        self.tuples.remove(&relationship_coord(s, r, o));
        true
    }

    /// Every tuple, including retracted ones — the audit face.
    #[must_use]
    pub fn tuples(&self) -> &[Tuple] {
        &self.triples
    }

    /// Zanzibar `LookupSubjects`: who stands in `relation` to `object`, right now.
    ///
    /// This is the graph walk's single step. Without it a relationship store is a membership
    /// oracle — it can confirm an edge you already name, and never hand you one you don't.
    #[must_use]
    pub fn expand(&self, relation: &str, object: &str) -> Vec<&str> {
        self.triples
            .iter()
            .filter(|t| t.relation == relation && t.object == object && t.holds_now())
            .map(|t| t.subject.as_str())
            .collect()
    }

    /// The same walk as of a point in time — what the graph said *then*.
    #[must_use]
    pub fn expand_at(&self, relation: &str, object: &str, when: i64) -> Vec<&str> {
        self.triples
            .iter()
            .filter(|t| t.relation == relation && t.object == object && t.holds_at(when))
            .map(|t| t.subject.as_str())
            .collect()
    }

    /// Zanzibar `LookupResources`: what `subject` stands in `relation` to.
    #[must_use]
    pub fn objects_of(&self, subject: &str, relation: &str) -> Vec<&str> {
        self.triples
            .iter()
            .filter(|t| t.subject == subject && t.relation == relation && t.holds_now())
            .map(|t| t.object.as_str())
            .collect()
    }

    /// Every relation name a subject participates in — capability discovery, the reflective read
    /// that lets an agent find out what it can traverse rather than being told.
    #[must_use]
    pub fn relations_of(&self, subject: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for t in self
            .triples
            .iter()
            .filter(|t| t.subject == subject && t.holds_now())
        {
            if !out.contains(&t.relation.as_str()) {
                out.push(&t.relation);
            }
        }
        out
    }

    /// Drop every tuple mentioning `entity` on either side. Returns how many were removed.
    ///
    /// Erasure, not invalidation — the one operation that must leave no trace, because "right to
    /// be forgotten" is not satisfied by a row marked no-longer-true.
    pub fn forget(&mut self, entity: &str) -> usize {
        let before = self.triples.len();
        for t in self
            .triples
            .iter()
            .filter(|t| t.subject == entity || t.object == entity)
        {
            self.tuples
                .remove(&relationship_coord(&t.subject, &t.relation, &t.object));
        }
        self.triples
            .retain(|t| t.subject != entity && t.object != entity);
        before - self.triples.len()
    }

    #[must_use]
    pub fn check(
        &self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
    ) -> bool {
        self.tuples.has(&relationship_coord(
            subject.as_ref(),
            relation.as_ref(),
            object.as_ref(),
        )) || self.tuples.has(&relationship_coord(
            subject.as_ref(),
            relation.as_ref(),
            "*",
        ))
    }
}

impl Default for Relationships {
    fn default() -> Self {
        Relationships::new()
    }
}

fn relationship_coord(subject: &str, relation: &str, object: &str) -> u64 {
    subject.coord().wrapping_mul(PHI)
        ^ relation.coord().wrapping_mul(PHI.rotate_left(17))
        ^ object.coord()
}

// ── Pingora face: upstream pools, load balancing, health ────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    scheme: String,
    host: String,
    port: u16,
    /// Relative selection weight for `LoadBalance::Weighted` — mirrors Pingora's own weighted
    /// selection (`pingora-load-balancing::selection::Weighted`). Ignored by the other three
    /// algorithms. Defaults to 1 (equal weighting); set via `with_weight`.
    weight: u32,
}

impl Upstream {
    #[must_use]
    pub fn http(host: impl Into<String>, port: u16) -> Self {
        Upstream {
            scheme: "http".into(),
            host: host.into(),
            port,
            weight: 1,
        }
    }

    #[must_use]
    pub fn https(host: impl Into<String>, port: u16) -> Self {
        Upstream {
            scheme: "https".into(),
            host: host.into(),
            port,
            weight: 1,
        }
    }

    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.max(1);
        self
    }

    #[must_use]
    pub fn url_for(&self, path: &str) -> String {
        format!("{}://{}:{}{}", self.scheme, self.host, self.port, path)
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    fn identity(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

/// Load-balancing algorithm across a route's upstreams — mirrors Pingora's own
/// `pingora-load-balancing::selection` algorithm set (`RoundRobin`/`Random`/`Weighted`/
/// `Consistent`). `Consistent` uses the request path as its hash key (session/cache affinity by
/// path) — a reasonable default, though real deployments may want client-IP-based affinity
/// instead; not yet configurable, an honest scope limit rather than a hidden one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LoadBalance {
    #[default]
    RoundRobin,
    Random,
    Weighted,
    Consistent,
    /// Pick the upstream with the lowest estimated response latency — the estimate is a real,
    /// continuously-fused measurement (`HealthEstimator`), not a static config value like
    /// `Weighted`'s weight. An upstream with no observations yet is treated as best (untested
    /// backends get a fair first try, the standard load-balancer convention).
    Healthiest,
}

/// A Kalman/information-form latency estimator for one upstream: carry `(η, τ)` (information,
/// precision), the estimate is `μ = η/τ`, and folding in a new observation is the commutative-
/// monoid merge — componentwise addition, order-free, so concurrent requests updating this from
/// different tasks never need to coordinate on ordering. Each observation here is unit-precision
/// (`τ_obs = 1`), so `μ` recovers a plain running mean — the simplest faithful instance, not the
/// full weighted-precision Kalman gain (an honest scope limit, inherited from the original).
#[derive(Debug)]
pub struct HealthEstimator {
    state: std::sync::Mutex<(f64, f64)>,
}

impl HealthEstimator {
    #[must_use]
    pub fn new() -> Self {
        // `(0, 0)` — the uninformative-prior unit.
        HealthEstimator {
            state: std::sync::Mutex::new((0.0, 0.0)),
        }
    }

    /// Fuse in one latency observation (seconds).
    pub fn observe(&self, latency_secs: f64) {
        let mut s = self.state.lock().unwrap();
        *s = (s.0 + latency_secs, s.1 + 1.0);
    }

    /// The current estimate `μ = η/τ`. `None` before any observation (`τ = 0`, the untested
    /// prior — not a false zero-latency claim).
    #[must_use]
    pub fn estimate(&self) -> Option<f64> {
        let s = *self.state.lock().unwrap();
        if s.1 == 0.0 {
            None
        } else {
            Some(s.0 / s.1)
        }
    }
}

impl Default for HealthEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// splitmix64 — a tiny, dependency-free, well-known mixing step. Not cryptographic; fine for
/// load-balancing spread, which is all it's used for here. `pub`: bedrock's own session-ID
/// generator (unrelated to load balancing) reuses this exact function rather than carrying a
/// second copy of the same four lines.
pub fn splitmix64(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let z = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// VNODES for the Ketama-style ring `consistent_ring_index` builds — each node gets this many
/// virtual points, spreading placement evenly even across a handful of real nodes.
pub const CONSISTENT_HASH_VNODES: u32 = 40;

/// Ketama-style consistent-hash ring lookup — proven stable + spreading by
/// `load_balance_consistent_is_stable_and_spreads` below. Build a ring of `vnodes` virtual
/// points per node (`splitmix64(node_coord ^ v)`), then route `key_coord` to the first ring
/// point at-or-after its own `splitmix64` hash, wrapping to the ring's first point past the end.
#[must_use]
pub fn consistent_ring_index(key_coord: u64, node_coords: &[u64], vnodes: u32) -> usize {
    if node_coords.is_empty() {
        return 0;
    }
    let key_hash = splitmix64(key_coord);
    let mut ring: Vec<(u64, usize)> = Vec::with_capacity(node_coords.len() * vnodes as usize);
    for (i, &coord) in node_coords.iter().enumerate() {
        for v in 0..vnodes {
            ring.push((splitmix64(coord ^ (v as u64)), i));
        }
    }
    ring.sort_unstable_by_key(|(h, _)| *h);
    ring.iter()
        .find(|(h, _)| *h >= key_hash)
        .or_else(|| ring.first())
        .map(|(_, i)| *i)
        .unwrap_or(0)
}

/// One route's backend set: the upstream list plus whatever state its selection algorithm
/// needs. `rr_counter` is `Arc`-shared (not a plain `AtomicUsize`) so cloning shares the SAME
/// round-robin rotation across all clones. `health` is parallel to `upstreams` (same index),
/// each `Arc`-shared for the same reason.
#[derive(Clone, Debug)]
pub struct Backends {
    upstreams: Vec<Upstream>,
    health: Vec<Arc<HealthEstimator>>,
    /// Active health-check state — `true` (the optimistic default) until a background probe
    /// marks it down. Independent of `health`'s PASSIVE measured-latency estimate: this is
    /// Pingora's own separate active-probe concept.
    active_health: Vec<Arc<std::sync::atomic::AtomicBool>>,
    rr_counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Backends {
    #[must_use]
    pub fn single(upstream: Upstream) -> Self {
        Backends {
            upstreams: vec![upstream],
            health: vec![Arc::new(HealthEstimator::new())],
            active_health: vec![Arc::new(std::sync::atomic::AtomicBool::new(true))],
            rr_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn push(&mut self, upstream: Upstream) {
        self.upstreams.push(upstream);
        self.health.push(Arc::new(HealthEstimator::new()));
        self.active_health
            .push(Arc::new(std::sync::atomic::AtomicBool::new(true)));
    }

    /// The registered upstreams, in registration order — for callers (like a background active
    /// health-check loop) that need to probe every one directly rather than go through `pick`.
    #[must_use]
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// Select one upstream by `algo`, returning it along with its health estimator so the
    /// caller can fuse in a real measured latency after forwarding. `hash_key` only matters for
    /// `Consistent`.
    #[must_use]
    pub fn pick(
        &self,
        algo: LoadBalance,
        hash_key: &str,
    ) -> Option<(&Upstream, &Arc<HealthEstimator>)> {
        if self.upstreams.is_empty() {
            return None;
        }
        if self.upstreams.len() == 1 {
            return Some((&self.upstreams[0], &self.health[0]));
        }
        let idx = match algo {
            LoadBalance::RoundRobin => {
                self.rr_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % self.upstreams.len()
            }
            LoadBalance::Random => {
                let seed = self
                    .rr_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    as u64;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                (splitmix64(seed ^ now) as usize) % self.upstreams.len()
            }
            LoadBalance::Weighted => {
                let total: u64 = self.upstreams.iter().map(|u| u.weight as u64).sum();
                let seed = self
                    .rr_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    as u64;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let mut r = splitmix64(seed ^ now) % total.max(1);
                let mut chosen = 0;
                for (i, u) in self.upstreams.iter().enumerate() {
                    if r < u.weight as u64 {
                        chosen = i;
                        break;
                    }
                    r -= u.weight as u64;
                }
                chosen
            }
            LoadBalance::Consistent => {
                let node_coords: Vec<u64> = self
                    .upstreams
                    .iter()
                    .map(|u| u.identity().coord())
                    .collect();
                consistent_ring_index(hash_key.coord(), &node_coords, CONSISTENT_HASH_VNODES)
            }
            LoadBalance::Healthiest => self
                .health
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let av = a.estimate().unwrap_or(f64::MIN);
                    let bv = b.estimate().unwrap_or(f64::MIN);
                    av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
        };

        let idx = if self
            .active_health
            .get(idx)
            .map(|h| h.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(true)
        {
            idx
        } else {
            self.active_health
                .iter()
                .position(|h| h.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(idx)
        };
        Some((self.upstreams.get(idx)?, &self.health[idx]))
    }

    /// Mark upstream `idx` up or down — what a background active-health-check probe calls.
    pub fn set_health(&self, idx: usize, healthy: bool) {
        if let Some(h) = self.active_health.get(idx) {
            h.store(healthy, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Route registration + load-balanced picking + the relationship store, WITHOUT bedrock's HTTP
/// forwarding half (`Proxy::handle`/`handle_with`/`cache`/`spawn_health_checks`) — see the module
/// doc for exactly why that half didn't come along. A real HTTP-forwarding `Proxy` built later
/// should hold one of these (or reimplement this same shape) rather than re-deriving
/// route→backend registration and picking a second time.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    upstreams: CoordMap<u64, Backends>,
    relationships: Relationships,
    lb: LoadBalance,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Registry {
            upstreams: CoordMap::coordinate(),
            relationships: Relationships::new(),
            lb: LoadBalance::default(),
        }
    }

    /// Set the load-balancing algorithm across ALL routes registered on this `Registry` (one
    /// algorithm per instance, not per-route — a real, honest scope limit inherited from the
    /// original `Proxy`).
    #[must_use]
    pub fn load_balance(mut self, algo: LoadBalance) -> Self {
        self.lb = algo;
        self
    }

    #[must_use]
    pub fn upstream(mut self, route: impl Into<String>, upstream: Upstream) -> Self {
        let route = route.into();
        let key = route.coord();
        match self.upstreams.get(&key) {
            Some(existing) => {
                let mut backends = existing.clone();
                backends.push(upstream);
                self.upstreams.put(key, backends);
            }
            None => {
                self.upstreams.put(key, Backends::single(upstream));
            }
        }
        self
    }

    #[must_use]
    pub fn allow(
        mut self,
        subject: impl AsRef<str>,
        relation: impl AsRef<str>,
        object: impl AsRef<str>,
    ) -> Self {
        self.relationships.relate(subject, relation, object);
        self
    }

    #[must_use]
    pub fn relationships(&self) -> &Relationships {
        &self.relationships
    }

    #[must_use]
    pub fn has_upstream(&self, route: &str) -> bool {
        self.pick(route, route)
            .or_else(|| self.pick("*", route))
            .is_some()
    }

    /// `hash_key` is only consulted by `LoadBalance::Consistent`. Returns the picked upstream
    /// together with its health estimator so a real HTTP-forwarding caller can fuse in the
    /// measured latency after forwarding.
    #[must_use]
    pub fn pick(&self, route: &str, hash_key: &str) -> Option<(&Upstream, &Arc<HealthEstimator>)> {
        self.upstreams
            .get(&route.coord())
            .and_then(|backends| backends.pick(self.lb, hash_key))
    }

    /// Mark an upstream on `route` (by index, same order as registered) up or down — what a real
    /// background active-health-check probe would call.
    pub fn set_health(&self, route: &str, idx: usize, healthy: bool) {
        if let Some(backends) = self.upstreams.get(&route.coord()) {
            backends.set_health(idx, healthy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationships_back_authzed_shape() {
        let mut rel = Relationships::new();
        rel.relate("user:lucas", "admin", "tenant:twn");
        assert!(rel.check("user:lucas", "admin", "tenant:twn"));
        assert!(!rel.check("user:lucas", "admin", "tenant:other"));

        rel.relate("user:lucas", "read", "*");
        assert!(rel.check("user:lucas", "read", "tenant:any"));
    }

    #[test]
    fn masquerade_authzed_relationship_surface() {
        let mut rel = Relationships::new();
        rel.relate("tenant:twn", "parent", "tenant:sub");
        rel.relate("user:lucas", "member", "tenant:twn");
        rel.relate("user:lucas", "admin", "tenant:twn");
        rel.relate("user:lucas", "read", "*");

        assert!(rel.check("user:lucas", "member", "tenant:twn"));
        assert!(rel.check("user:lucas", "admin", "tenant:twn"));
        assert!(!rel.check("user:lucas", "member", "tenant:other"));
        assert!(rel.check("tenant:twn", "parent", "tenant:sub"));

        // wildcard: "read *" means can read ANY object
        assert!(rel.check("user:lucas", "read", "any:resource"));
        assert!(rel.check("user:lucas", "read", "different:thing"));
        assert!(!rel.check("user:lucas", "admin", "any:resource"));
    }

    #[test]
    fn masquerade_pingora_registry_surface() {
        let registry = Registry::new()
            .upstream("/api", Upstream::http("backend1", 3000))
            .upstream("/api", Upstream::http("backend2", 3000))
            .upstream("/admin", Upstream::https("secure", 443))
            .allow("user:alice", "read", "route:/api")
            .allow("user:bob", "admin", "route:/admin");

        assert!(registry.has_upstream("/api"));
        assert!(registry.has_upstream("/admin"));
        assert!(!registry.has_upstream("/unknown"));

        assert!(registry
            .relationships()
            .check("user:alice", "read", "route:/api"));
        assert!(registry
            .relationships()
            .check("user:bob", "admin", "route:/admin"));
        assert!(!registry
            .relationships()
            .check("user:alice", "admin", "route:/admin"));
    }

    /// Proof RoundRobin actually cycles, not just "load balancing" in name only (the bug the
    /// original bedrock upgrade fixed: `pick()` used to always return the first upstream).
    #[test]
    fn load_balance_round_robin_actually_cycles() {
        let registry = Registry::new()
            .load_balance(LoadBalance::RoundRobin)
            .upstream("/api", Upstream::http("a", 1))
            .upstream("/api", Upstream::http("b", 2))
            .upstream("/api", Upstream::http("c", 3));

        let picks: Vec<u16> = (0..9)
            .map(|_| registry.pick("/api", "/api").unwrap().0.port())
            .collect();
        assert_eq!(
            picks,
            vec![1, 2, 3, 1, 2, 3, 1, 2, 3],
            "round robin should cycle exactly in order"
        );
    }

    /// Random must actually cover every upstream given enough picks (not silently degenerate to
    /// always picking one).
    #[test]
    fn load_balance_random_covers_all_upstreams() {
        let registry = Registry::new()
            .load_balance(LoadBalance::Random)
            .upstream("/r", Upstream::http("a", 1))
            .upstream("/r", Upstream::http("b", 2))
            .upstream("/r", Upstream::http("c", 3));

        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            seen.insert(registry.pick("/r", "/r").unwrap().0.port());
        }
        assert_eq!(
            seen,
            [1u16, 2, 3].into_iter().collect(),
            "500 random picks across 3 upstreams should hit all three"
        );
    }

    /// A heavily-weighted upstream must be picked dramatically more often than a low-weight one
    /// — proof the weight field is actually consulted, not just carried and ignored.
    #[test]
    fn load_balance_weighted_favors_higher_weight() {
        let registry = Registry::new()
            .load_balance(LoadBalance::Weighted)
            .upstream("/w", Upstream::http("heavy", 1).with_weight(99))
            .upstream("/w", Upstream::http("light", 2).with_weight(1));

        let mut heavy_count = 0;
        for _ in 0..1000 {
            if registry.pick("/w", "/w").unwrap().0.port() == 1 {
                heavy_count += 1;
            }
        }
        assert!(heavy_count > 900, "weight-99-vs-1 should land on the heavy upstream >90% of the time, got {heavy_count}/1000");
    }

    /// Consistent hashing must be STABLE (the same key always lands on the same upstream) and
    /// must actually SPREAD across upstreams for different keys, not collapse to one.
    #[test]
    fn load_balance_consistent_is_stable_and_spreads() {
        let registry = Registry::new()
            .load_balance(LoadBalance::Consistent)
            .upstream("/c", Upstream::http("a", 1))
            .upstream("/c", Upstream::http("b", 2))
            .upstream("/c", Upstream::http("c", 3));

        let first = registry.pick("/c", "user-42").unwrap().0.port();
        for _ in 0..20 {
            assert_eq!(
                registry.pick("/c", "user-42").unwrap().0.port(),
                first,
                "consistent hash must be stable for the same key"
            );
        }

        let mut seen = std::collections::HashSet::new();
        for i in 0..50 {
            seen.insert(registry.pick("/c", &format!("user-{i}")).unwrap().0.port());
        }
        assert!(
            seen.len() > 1,
            "50 distinct keys across 3 upstreams should not all collapse onto one, got {seen:?}"
        );
    }

    /// **Adapted from the original** (disclosed): the original test drove this end-to-end
    /// through real HTTP requests against real local servers with different response delays,
    /// which needed bedrock's HTTP client (deliberately not extracted here — see the module
    /// doc). This version fuses the same real latency numbers directly via `HealthEstimator`,
    /// via `pick()`'s own returned handle — proving `Healthiest`'s selection logic responds
    /// correctly to real observations, not proving the HTTP measurement path that would produce
    /// them in a real deployment.
    #[test]
    fn load_balance_healthiest_prefers_the_lower_latency_upstream() {
        let registry = Registry::new()
            .load_balance(LoadBalance::Healthiest)
            .upstream("/svc", Upstream::http("fast", 1))
            .upstream("/svc", Upstream::http("slow", 2));

        // Fuse real observations: "fast" consistently 5ms, "slow" consistently 500ms.
        for _ in 0..6 {
            let (upstream, health) = registry.pick("/svc", "/svc").unwrap();
            let latency = if upstream.port() == 1 { 0.005 } else { 0.5 };
            health.observe(latency);
        }

        let mut fast_count = 0;
        for _ in 0..20 {
            if registry.pick("/svc", "/svc").unwrap().0.port() == 1 {
                fast_count += 1;
            }
        }
        assert!(fast_count > 15, "after real fused observations, Healthiest should overwhelmingly prefer the faster upstream, got {fast_count}/20");
    }

    #[test]
    fn a_route_with_one_upstream_never_calls_the_algorithm() {
        // Real edge case the original code special-cases (`if upstreams.len() == 1`) — worth a
        // named test so a future change to the algorithm dispatch can't silently break it.
        let registry = Registry::new()
            .load_balance(LoadBalance::Consistent)
            .upstream("/one", Upstream::http("only", 7));
        assert_eq!(registry.pick("/one", "anything").unwrap().0.port(), 7);
    }

    #[test]
    fn set_health_routes_around_a_down_backend() {
        let registry = Registry::new()
            .load_balance(LoadBalance::RoundRobin)
            .upstream("/h", Upstream::http("a", 1))
            .upstream("/h", Upstream::http("b", 2));
        registry.set_health("/h", 0, false);
        for _ in 0..5 {
            assert_eq!(
                registry.pick("/h", "/h").unwrap().0.port(),
                2,
                "a marked-down backend must be skipped"
            );
        }
    }
}
