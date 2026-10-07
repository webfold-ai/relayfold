//! The live route table: a `relayfold::Registry` built from the config's pools, plus the
//! request-side glue that turns a request into a `Registry::pick` and a forward.
//!
//! **Why the Registry and not biota's `Proxy`.** `biota_http_server::Proxy` is the same
//! route→backend registry with an HTTP client bolted on, and its own `forward_raw` matches
//! routes by EXACT `uri.path()` with a `*` fallback — so it can express "one catch-all
//! upstream" and nothing else. A file-driven table needs prefix routes (`/api` covering
//! `/api/users/42`), which means the runtime must resolve the route itself and then ask the
//! registry which upstream. `Registry::pick_with` returns `(&Upstream, &Arc<HealthEstimator>)`
//! precisely for this: its doc says a real HTTP-forwarding caller fuses the measured latency
//! into that estimator, and that is exactly what [`RouteTable::forward`] does.
//!
//! **The registry's keys are POOLS, not route prefixes.** `Registry::upstream(route, upstream)`
//! is called once per backend with the pool's name, so `pick_with(pool, algo, key)` is the
//! load-balancing step and route resolution stays a separate, purely local prefix scan. That
//! is what lets two routes share one backend set without writing it twice.
//!
//! **Reload is a diff, not a rebuild.** [`RouteTable::apply`] reconciles each POOL's backend
//! set in place (`Backends::reconcile`), so a backend present in both the old and the new
//! config keeps its slot, its measured latency, its active-health verdict and its round-robin
//! position. The registry SHELL (the pool→backends map and the relationship store) is
//! rebuilt, because that is how a removed `allow` tuple stops granting access.

use crate::config::{self, AllowCfg, HashKeySource, ProxyConfig, RouteCfg};
use relayfold::{Backends, LoadBalance, ReconcileOutcome, Registry};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// What one reload actually changed. Every field names what changed, so the operator sees
/// "the deploy I just did" rather than "reload succeeded".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
    /// Per-pool backend reconciliation counts, for the pools that changed.
    pub backends: BTreeMap<String, ReconcileOutcome>,
}

impl ReloadDiff {
    /// True when the new config is equivalent to the live one — the signal to skip the swap,
    /// and to answer "did that reload actually do anything?" with a no.
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} added, {} changed, {} removed, {} unchanged",
            self.added.len(),
            self.changed.len(),
            self.removed.len(),
            self.unchanged.len()
        )
    }
}

/// The config-derived half of a route: what the `Registry` cannot know, because `Registry`
/// stores keys as opaque coordinates and knows nothing about prefixes or identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteMeta {
    pub prefix: String,
    pub host: Option<String>,
    pub pool: String,
    pub strip_prefix: bool,
    pub hash_key: HashKeySource,
    pub allow: Vec<AllowCfg>,
}

impl RouteMeta {
    fn of(rc: &RouteCfg) -> Result<Self, String> {
        let hash_key = HashKeySource::parse(rc.hash_key.as_deref())
            .map_err(|e| format!("route {}: {e}", rc.effective_prefix()))?;
        Ok(RouteMeta {
            prefix: rc.effective_prefix(),
            host: rc.effective_host(),
            pool: rc
                .pool_name()
                .ok_or_else(|| format!("route {}: no upstream pool", rc.effective_prefix()))?,
            strip_prefix: rc.strip_prefix(),
            hash_key,
            allow: rc.allow.clone(),
        })
    }

    /// Does this route match `req_path` on `host`?
    ///
    /// Prefix matching is on SEGMENT boundaries: `/api` covers `/api` and `/api/x` but not
    /// `/apix`, because `/apix` is a different resource and silently routing it through the
    /// `/api` pool is the kind of mistake that is invisible until it is a security incident.
    pub fn matches(&self, req_path: &str, host: Option<&str>) -> bool {
        if let Some(want) = &self.host {
            let got = host.unwrap_or_default();
            // A Host header carries a port routinely and is case-insensitive; compare the
            // bare host.
            let got = got.split(':').next().unwrap_or(got);
            if !got.eq_ignore_ascii_case(want) {
                return false;
            }
        }
        let base = self.prefix.trim_end_matches('/');
        base.is_empty()
            || req_path == base
            || (req_path.starts_with(base) && req_path.as_bytes().get(base.len()) == Some(&b'/'))
    }

    /// Specificity rank: lower sorts first and wins. A host-constrained route beats an
    /// unconstrained one at the same prefix; a longer prefix beats a shorter one.
    fn rank(&self) -> (u8, std::cmp::Reverse<usize>, u8) {
        (
            // Ascending, so `host-constrained` must map to 0 and `any-host` to 1.
            u8::from(self.host.is_none()),
            std::cmp::Reverse(self.prefix.len()),
            u8::from(self.prefix == "/"),
        )
    }
}

/// The live table. Requests take a cheap clone (every field behind an `Arc`) rather than
/// holding the table's lock across an upstream round-trip — otherwise one slow backend would
/// block the very reload that is meant to remove it.
pub struct RouteTable {
    registry: Arc<Registry>,
    /// Matching rules, in resolution order (see [`RouteMeta::rank`]).
    routes: Arc<Vec<RouteMeta>>,
    /// Every pool name, in config order. `Registry` stores its keys as opaque coordinates and
    /// cannot hand them back, so the names are tracked here — they are also what the health
    /// prober iterates and what a reload diff is written against.
    pool_names: Arc<Vec<String>>,
    /// Per-pool algorithm. A pool absent from this map uses [`RouteTable::default_algo`].
    algos: Arc<BTreeMap<String, LoadBalance>>,
    client: Arc<biota_http_server::Client>,
    /// Bumped on every applied config. The health prober compares against the value it
    /// captured when it snapshotted the backend list, so a probe that raced a reload
    /// discards its verdicts instead of writing them against shifted indices.
    generation: u64,
    /// Where caller identity for `allow` rules comes from.
    subject_header: String,
    /// The registry-wide algorithm, which is also the default for any pool that names none.
    default_algo: LoadBalance,
}

impl Clone for RouteTable {
    /// A snapshot sharing the same backend state. Cheap: every field is an `Arc`, and
    /// `Registry`'s clone copies the pool map but shares each `Backends`' health estimators,
    /// active-health flags and round-robin cursor — so a snapshot taken for a reload and one
    /// taken for an in-flight request observe the same backend state.
    fn clone(&self) -> Self {
        RouteTable {
            registry: Arc::new((*self.registry).clone()),
            routes: Arc::clone(&self.routes),
            pool_names: Arc::clone(&self.pool_names),
            algos: Arc::clone(&self.algos),
            client: Arc::clone(&self.client),
            generation: self.generation,
            subject_header: self.subject_header.clone(),
            default_algo: self.default_algo,
        }
    }
}

impl RouteTable {
    /// First load: construct from config and register every pool.
    pub fn build(cfg: &ProxyConfig) -> Result<Self, String> {
        let default_algo = cfg
            .load_balance
            .map_or(LoadBalance::default(), config::LoadBalanceCfg::as_relayfold);
        let mut t = RouteTable {
            // `Registry::lb` is set to the default so the plain `Registry::pick` answers
            // correctly for every pool that does not override it; the per-pool override goes
            // through `Registry::pick_with`.
            registry: Arc::new(Registry::new().load_balance(default_algo)),
            routes: Arc::new(Vec::new()),
            pool_names: Arc::new(Vec::new()),
            algos: Arc::new(BTreeMap::new()),
            client: Arc::new(biota_http_server::Client::new()),
            generation: 1,
            subject_header: cfg
                .subject_header
                .clone()
                .unwrap_or_else(|| "x-forwarded-subject".to_string()),
            default_algo,
        };
        t.apply(cfg);
        Ok(t)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Every route prefix, in resolution order.
    pub fn route_paths(&self) -> Vec<&str> {
        self.routes.iter().map(|r| r.prefix.as_str()).collect()
    }

    pub fn has_route(&self, prefix: &str) -> bool {
        self.routes.iter().any(|r| r.prefix == prefix)
    }

    pub fn has_pool(&self, pool: &str) -> bool {
        self.registry.backends_for(pool).is_some()
    }

    /// The algorithm that governs `pool`.
    pub fn algo_for(&self, pool: &str) -> LoadBalance {
        self.algos.get(pool).copied().unwrap_or(self.default_algo)
    }

    /// One pool's backends, as `scheme://host:port` — what a diff is written against.
    pub fn backends_of(&self, pool: &str) -> Vec<String> {
        self.registry
            .backends_for(pool)
            .map(|b| b.upstreams().iter().map(|u| u.url_for("")).collect())
            .unwrap_or_default()
    }

    /// Every pool's backends, flattened for the health prober: `(pool, index, probe_url)`.
    /// The index is the one `Registry::set_health` wants — registration order, which
    /// `Backends::reconcile` preserves for any backend that survives a reload.
    pub fn all_backends(&self, probe_path: &str) -> Vec<(String, usize, String)> {
        let mut out = Vec::new();
        for pool in self.pool_names.iter() {
            if let Some(b) = self.registry.backends_for(pool) {
                for (i, u) in b.upstreams().iter().enumerate() {
                    out.push((pool.clone(), i, u.url_for(probe_path)));
                }
            }
        }
        out
    }

    /// Reconcile the live table against `cfg`. Returns what changed.
    ///
    /// The registry shell is rebuilt from scratch so a removed route's `allow` tuples stop
    /// granting access and a removed pool's backends stop being reachable; each pool's
    /// `Backends` is grafted across from the live registry — a clone, so the `Arc`s inside
    /// are shared and health/rotation survive.
    pub fn apply(&mut self, cfg: &ProxyConfig) -> ReloadDiff {
        let default_algo = cfg
            .load_balance
            .map_or(LoadBalance::default(), config::LoadBalanceCfg::as_relayfold);
        let mut diff = ReloadDiff::default();
        if default_algo != self.default_algo {
            diff.changed.push("<load_balance>".to_string());
        }

        let mut next_registry = Registry::new().load_balance(default_algo);
        let mut next_routes: Vec<RouteMeta> = Vec::new();
        let mut next_algos: BTreeMap<String, LoadBalance> = BTreeMap::new();
        let mut next_pool_names: Vec<String> = Vec::new();

        // -- pools: the registry's contents --
        let pools = cfg.resolved_pools();
        for (name, pool) in &pools {
            let desired: Vec<relayfold::Upstream> = pool
                .backends()
                .iter()
                .filter_map(|b| b.as_upstream().ok())
                .collect();
            let backends: Backends = match self.registry.backends_for(name) {
                Some(mut existing) => {
                    let out = existing.reconcile(&desired);
                    if out != ReconcileOutcome::default() {
                        diff.backends.insert(name.clone(), out);
                    }
                    existing
                }
                None => fresh_backends(name, &desired),
            };
            next_registry.set_backends(name, backends);
            next_pool_names.push(name.clone());
            if let Some(a) = pool.load_balance() {
                next_algos.insert(name.clone(), a.as_relayfold());
            }
        }

        // -- routes: the matching rules --
        for rc in &cfg.routes {
            let Ok(meta) = RouteMeta::of(rc) else {
                // `validate` runs before `apply` on every path into this function; a route
                // that cannot produce metadata here is a programming error, not config.
                // Skipping is strictly safer than serving it with guessed semantics.
                continue;
            };
            for a in &meta.allow {
                next_registry = next_registry.allow(&a.subject, &a.relation, &a.object);
            }
            let live = self
                .routes
                .iter()
                .find(|m| m.prefix == meta.prefix && m.host == meta.host);
            let known = live == Some(&meta);
            // An algorithm change with an identical backend set is still a change: the diff
            // has to say so, or an operator flips a pool from round_robin to weighted,
            // reloads, and is told "no change".
            let algo_moved = next_algos.get(&meta.pool).copied().unwrap_or(default_algo)
                != self.algo_for(&meta.pool);
            match live {
                // Same rule, same pool, same semantics: nothing to do. This is the case that
                // makes a no-op reload free.
                Some(_) if known && !diff.backends.contains_key(&meta.pool) && !algo_moved => {
                    diff.unchanged.push(meta.prefix.clone());
                }
                Some(_) => diff.changed.push(meta.prefix.clone()),
                None => diff.added.push(meta.prefix.clone()),
            }
            next_routes.push(meta);
        }
        for old in self.routes.iter() {
            if !next_routes
                .iter()
                .any(|m| m.prefix == old.prefix && m.host == old.host)
            {
                diff.removed.push(old.prefix.clone());
            }
        }

        if !diff.is_noop() {
            next_routes.sort_by_key(RouteMeta::rank);
            self.registry = Arc::new(next_registry);
            self.routes = Arc::new(next_routes);
            self.pool_names = Arc::new(next_pool_names);
            self.algos = Arc::new(next_algos);
            self.default_algo = default_algo;
            self.generation += 1;
            if let Some(h) = &cfg.subject_header {
                self.subject_header = h.clone();
            }
        }
        diff
    }

    /// Which route serves `req_path` for this `Host`: the most specific match.
    pub fn resolve(&self, req_path: &str, host: Option<&str>) -> Option<&RouteMeta> {
        self.routes.iter().find(|m| m.matches(req_path, host))
    }

    /// The path to forward, honouring `strip_prefix`. The query string is always preserved —
    /// a proxy that drops it breaks every query-bearing request.
    pub fn forward_path(&self, meta: &RouteMeta, req_path_and_query: &str) -> String {
        if !meta.strip_prefix {
            return req_path_and_query.to_string();
        }
        let (path, query) = match req_path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (req_path_and_query, None),
        };
        let base = meta.prefix.trim_end_matches('/');
        let stripped = if base.is_empty() {
            path
        } else {
            path.strip_prefix(base).unwrap_or(path)
        };
        let stripped = if stripped.is_empty() { "/" } else { stripped };
        match query {
            Some(q) => format!("{stripped}?{q}"),
            None => stripped.to_string(),
        }
    }

    /// The identity an `allow` rule is checked against: the configured subject header if
    /// present, else the peer IP, else `"anonymous"`. This is a real trust boundary — the
    /// header is only as trustworthy as whatever put it there, so a deployment that does not
    /// scrub it should key subjects on `client_ip` and a per-network subject instead.
    pub fn subject_of(&self, req: &biota_http_server::Request) -> String {
        if let Some(v) = req
            .headers()
            .get(&self.subject_header)
            .and_then(|v| v.to_str().ok())
        {
            if !v.is_empty() {
                return v.to_string();
            }
        }
        req.peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "anonymous".to_string())
    }

    fn hash_key(&self, meta: &RouteMeta, req: &biota_http_server::Request) -> String {
        let path_and_query = req.uri().path_and_query().to_string();
        match &meta.hash_key {
            HashKeySource::Path => path_and_query,
            HashKeySource::ClientIp => req
                .peer_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or(path_and_query),
            HashKeySource::Header(name) => req
                .headers()
                .get(name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string())
                .unwrap_or(path_and_query),
        }
    }

    /// The route's `allow` tuples, checked against the caller's identity. A route with no
    /// rules is OPEN — the same default the pre-file runtime had, kept deliberately so adding
    /// a route does not silently 403 it.
    pub fn is_allowed(&self, meta: &RouteMeta, req: &biota_http_server::Request) -> bool {
        if meta.allow.is_empty() {
            return true;
        }
        let subject = self.subject_of(req);
        meta.allow.iter().any(|a| {
            self.registry
                .relationships()
                .check(&subject, &a.relation, &a.object)
        })
    }

    /// Resolve, authorize, pick a backend from the `Registry`, forward, and fuse the measured
    /// latency back into that backend's health estimator.
    pub async fn forward(&self, req: biota_http_server::Request) -> biota_http_server::Response {
        use biota_http_server::{Response, StatusCode};

        let path = req.uri().path().to_string();
        let host = req
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let Some(meta) = self.resolve(&path, host.as_deref()) else {
            return Response::new(StatusCode::NOT_FOUND, "no route matches this request");
        };
        if !self.is_allowed(meta, &req) {
            return Response::new(StatusCode::FORBIDDEN, "no allow rule matches this caller");
        }
        let key = self.hash_key(meta, &req);
        let Some((upstream, health)) =
            self.registry
                .pick_with(&meta.pool, self.algo_for(&meta.pool), &key)
        else {
            // `pick_with` only answers None for an empty backend set, and `validate` forbids
            // an empty one — so reaching here means the pool was removed between resolution
            // and pick. 503, not 502: the client's request was fine.
            return Response::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("pool {:?} has no backend", meta.pool),
            );
        };

        let url = upstream.url_for(&self.forward_path(meta, req.uri().path_and_query()));
        let mut builder = self
            .client
            .request(req.method().clone(), url.clone())
            .body_bytes(req.body().to_vec());
        for (name, value) in req.headers().iter() {
            // Hop-by-hop. `Expect: 100-continue` is answered here (the body is already
            // buffered); `Connection`/`Keep-Alive` are the DOWNSTREAM's framing preference and
            // forwarding them defeats this client's keepalive pool.
            if name == "expect" || name == "connection" || name == "keep-alive" {
                continue;
            }
            builder = builder.header(name, value.as_bytes().to_vec());
        }

        let t0 = Instant::now();
        let result = builder.send().await;
        health.observe(t0.elapsed().as_secs_f64());
        match result {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("[relayfold-proxy] backend {url} failed: {e}");
                Response::new(StatusCode::BAD_GATEWAY, e.to_string())
            }
        }
    }

    /// Mark one backend up/down. `set_health` indexes by registration order, which
    /// `Backends::reconcile` preserves across reloads for surviving backends.
    pub fn set_health(&self, pool: &str, idx: usize, healthy: bool) {
        self.registry.set_health(pool, idx, healthy);
    }
}

fn fresh_backends(pool: &str, desired: &[relayfold::Upstream]) -> Backends {
    // `Registry::upstream` is builder-shaped, so the cheapest honest way to get a `Backends`
    // for a brand-new pool is to let a throwaway registry build it. The key is the same
    // string the real registry uses, so nothing is lost.
    let mut tmp = Registry::new();
    for u in desired {
        tmp = tmp.upstream(pool, u.clone());
    }
    tmp.backends_for(pool).unwrap_or_else(|| {
        Backends::single(
            desired
                .first()
                .cloned()
                .unwrap_or_else(|| relayfold::Upstream::http("127.0.0.1", 1)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse, PoolCfg, PoolEntry};
    use biota_http_server::{Method, Request, Uri};
    use std::path::Path;

    fn cfg(yaml: &str) -> ProxyConfig {
        let c = parse(Path::new("t.yaml"), yaml).expect("parses");
        c.validate().expect("valid");
        c
    }

    fn backend(host: &str, port: u16) -> PoolEntry {
        PoolEntry {
            host: host.into(),
            port: Some(port),
            scheme: None,
            weight: None,
        }
    }

    fn canonical() -> ProxyConfig {
        cfg(r#"
routes:
  - match: { prefix: /api }
    upstream: backend
  - match: { prefix: "/" }
    upstream: backend
pools:
  backend:
    backends:
      - { host: "10.0.0.1", port: 8080 }
      - { host: "10.0.0.2", port: 8080 }
"#)
    }

    #[test]
    fn routes_resolve_most_specific_first_and_share_one_pool() {
        let t = RouteTable::build(&canonical()).unwrap();
        assert_eq!(t.resolve("/api/users/42", None).unwrap().prefix, "/api");
        assert_eq!(t.resolve("/other", None).unwrap().prefix, "/");
        assert_eq!(t.resolve("/api/users/42", None).unwrap().pool, "backend");
        assert_eq!(
            t.backends_of("backend").len(),
            2,
            "one pool, shared by both routes"
        );
    }

    #[test]
    fn prefix_matching_respects_segment_boundaries() {
        let t = RouteTable::build(&canonical()).unwrap();
        assert!(t.resolve("/api", None).is_some());
        assert_eq!(
            t.resolve("/apix", None).unwrap().prefix,
            "/",
            "a longer segment is a different resource"
        );
    }

    #[test]
    fn a_host_constrained_route_wins_over_an_unconstrained_one_and_ignores_the_port() {
        let t = RouteTable::build(&cfg(r#"
routes:
  - match: { prefix: /api }
    upstream: general
  - match: { prefix: /api, host: "api.example.com" }
    upstream: dedicated
pools:
  general: [{host: g, port: 1}]
  dedicated: [{host: d, port: 1}]
"#))
        .unwrap();
        assert_eq!(
            t.resolve("/api", Some("api.example.com:8443"))
                .unwrap()
                .pool,
            "dedicated"
        );
        assert_eq!(
            t.resolve("/api", Some("API.EXAMPLE.COM")).unwrap().pool,
            "dedicated"
        );
        assert_eq!(
            t.resolve("/api", Some("other.example.com")).unwrap().pool,
            "general"
        );
    }

    #[test]
    fn a_per_pool_algorithm_overrides_the_global_default() {
        let t = RouteTable::build(&cfg(r#"
load_balance: round_robin
routes:
  - match: { prefix: /a }
    upstream: rr
  - match: { prefix: /b }
    upstream: weighted
pools:
  rr: [{host: a1, port: 1}, {host: a2, port: 2}]
  weighted:
    load_balance: weighted
    backends: [{host: b1, port: 1, weight: 1}]
"#))
        .unwrap();
        assert_eq!(t.algo_for("rr"), LoadBalance::RoundRobin);
        assert_eq!(t.algo_for("weighted"), LoadBalance::Weighted);
        assert_eq!(
            t.algo_for("unknown"),
            LoadBalance::RoundRobin,
            "unknown falls back"
        );
    }

    #[test]
    fn a_new_route_is_added_and_a_removed_one_is_gone() {
        let mut t = RouteTable::build(&canonical()).unwrap();
        let d = t.apply(&cfg(r#"
routes:
  - match: { prefix: /new }
    upstream: other
  - match: { prefix: "/" }
    upstream: backend
pools:
  backend: [{host: "10.0.0.1", port: 8080}, {host: "10.0.0.2", port: 8080}]
  other: [{host: c, port: 3}]
"#));
        assert_eq!(d.added, vec!["/new"]);
        assert_eq!(d.removed, vec!["/api"]);
        assert!(t.has_route("/new"));
        assert!(
            !t.has_route("/api"),
            "a removed route can never serve again"
        );
        assert_eq!(t.resolve("/api/x", None).unwrap().prefix, "/");
    }

    #[test]
    fn an_identical_reload_is_a_no_op() {
        let c = canonical();
        let mut t = RouteTable::build(&c).unwrap();
        let gen = t.generation();
        let d = t.apply(&c);
        assert!(d.is_noop(), "{d:?}");
        assert_eq!(d.unchanged.len(), 2);
        assert_eq!(t.generation(), gen, "a no-op must not bump the generation");
    }

    #[test]
    fn adding_a_backend_patches_the_pool_and_leaves_other_pools_untouched() {
        let mut t = RouteTable::build(&canonical()).unwrap();
        let gen = t.generation();
        let d = t.apply(&cfg(
            r#"
routes:
  - match: { prefix: /api }
    upstream: backend
  - match: { prefix: "/" }
    upstream: backend
  - match: { prefix: /other }
    upstream: side
pools:
  backend: [{host: "10.0.0.1", port: 8080}, {host: "10.0.0.2", port: 8080}, {host: "10.0.0.3", port: 8080}]
  side: [{host: s, port: 1}]
"#,
        ));
        assert_eq!(d.added, vec!["/other"]);
        assert_eq!(
            d.backends["backend"],
            ReconcileOutcome {
                added: 1,
                removed: 0,
                reweighted: 0
            }
        );
        assert_eq!(t.backends_of("backend").len(), 3);
        assert!(t.generation() > gen);
    }

    #[test]
    fn a_surviving_backend_keeps_its_health_verdict_across_a_reload() {
        // This is the whole reason reload is a diff: a backend that was probed DOWN and stays
        // in the config must not be handed a fresh optimistic AtomicBool, or a reload would
        // resurrect every backend the prober just took out.
        let mut t = RouteTable::build(&canonical()).unwrap();
        t.set_health("backend", 0, false);
        t.apply(&cfg(
            r#"
routes:
  - match: { prefix: /api }
    upstream: backend
  - match: { prefix: "/" }
    upstream: backend
pools:
  backend: [{host: "10.0.0.1", port: 8080}, {host: "10.0.0.2", port: 8080}, {host: "10.0.0.3", port: 8080}]
"#,
        ));
        let mut picked = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let (u, _) = t
                .registry()
                .pick_with("backend", LoadBalance::RoundRobin, "/api/k")
                .unwrap();
            picked.insert(u.url_for(""));
        }
        assert!(
            !picked.contains(&"http://10.0.0.1:8080".to_string()),
            "down backend came back: {picked:?}"
        );
        assert!(
            picked.contains(&"http://10.0.0.3:8080".to_string()),
            "new backend never picked"
        );
    }

    #[test]
    fn removing_a_removed_pool_is_visible_in_the_diff() {
        let mut t = RouteTable::build(&cfg(r#"
routes: [{match: {prefix: /}, upstream: a}, {match: {prefix: /x}, upstream: b}]
pools:
  a: [{host: a, port: 1}]
  b: [{host: b, port: 1}]
"#))
        .unwrap();
        let d = t.apply(&cfg(
            "routes: [{match: {prefix: /}, upstream: a}]\npools:\n  a: [{host: a, port: 1}]\n",
        ));
        assert_eq!(d.removed, vec!["/x"]);
        assert!(!t.has_pool("b"), "a removed pool can never be picked");
        assert!(
            t.resolve("/x", None).is_some(),
            "and its route falls through to /"
        );
        assert_eq!(t.resolve("/x", None).unwrap().pool, "a");
    }

    #[test]
    fn removing_an_allow_rule_revokes_access() {
        let with_allow = cfg(r#"
routes:
  - match: { prefix: /api }
    upstream: b
    allow: [{subject: tenant-a, relation: may_use, object: /api}]
pools:
  b: [{host: h, port: 1}]
"#);
        let mut t = RouteTable::build(&with_allow).unwrap();
        let allowed = Request::new(Method::GET, "/api".parse::<Uri>().unwrap())
            .with_header("x-forwarded-subject", "tenant-a");
        assert!(t.is_allowed(t.resolve("/api", None).unwrap(), &allowed));
        let denied = Request::new(Method::GET, "/api".parse::<Uri>().unwrap())
            .with_header("x-forwarded-subject", "tenant-b");
        assert!(!t.is_allowed(t.resolve("/api", None).unwrap(), &denied));

        t.apply(&cfg(
            "routes: [{match: {prefix: /api}, upstream: b}]\npools:\n  b: [{host: h, port: 1}]\n",
        ));
        assert!(
            t.is_allowed(t.resolve("/api", None).unwrap(), &denied),
            "the revoked tuple must stop granting"
        );
        assert!(!t
            .registry()
            .relationships()
            .check("tenant-a", "may_use", "/api"));
    }

    #[test]
    fn a_route_with_no_allow_rules_is_open() {
        let t = RouteTable::build(&canonical()).unwrap();
        let req = Request::new(Method::GET, "/anything".parse::<Uri>().unwrap());
        assert!(t.is_allowed(t.resolve("/anything", None).unwrap(), &req));
    }

    #[test]
    fn subject_falls_back_to_peer_ip_then_anonymous() {
        let t = RouteTable::build(&canonical()).unwrap();
        let bare = Request::new(Method::GET, "/x".parse::<Uri>().unwrap());
        assert_eq!(t.subject_of(&bare), "anonymous");
        let with_peer = bare
            .clone()
            .with_peer_addr("10.1.2.3:5555".parse().unwrap());
        assert_eq!(t.subject_of(&with_peer), "10.1.2.3");
        let with_hdr = bare.with_header("x-forwarded-subject", "tenant-z");
        assert_eq!(t.subject_of(&with_hdr), "tenant-z");
    }

    #[test]
    fn strip_prefix_rewrites_the_path_and_keeps_the_query() {
        let t = RouteTable::build(&cfg(r#"
routes:
  - match: { prefix: /api }
    upstream: b
    strip_prefix: true
  - match: { prefix: /keep }
    upstream: b
pools:
  b: [{host: h, port: 1}]
"#))
        .unwrap();
        let api = t.resolve("/api", None).unwrap();
        assert_eq!(t.forward_path(api, "/api/users?page=2"), "/users?page=2");
        assert_eq!(t.forward_path(api, "/api"), "/");
        let keep = t.resolve("/keep", None).unwrap();
        assert_eq!(
            t.forward_path(keep, "/keep/x"),
            "/keep/x",
            "transparent by default"
        );
    }

    #[test]
    fn a_changed_strip_prefix_counts_as_a_change_even_with_identical_backends() {
        let before =
            "routes: [{match: {prefix: /api}, upstream: b}]\npools:\n  b: [{host: h, port: 1}]\n";
        let after =
            "routes: [{match: {prefix: /api}, upstream: b, strip_prefix: true}]\npools:\n  b: [{host: h, port: 1}]\n";
        let mut t = RouteTable::build(&cfg(before)).unwrap();
        let d = t.apply(&cfg(after));
        assert_eq!(d.changed, vec!["/api"]);
        assert!(d.backends.is_empty(), "no backend churn, only semantics");
    }

    #[test]
    fn a_changed_pool_algorithm_counts_as_a_change_with_no_backend_churn() {
        let mut t = RouteTable::build(&cfg(
            "routes: [{match: {prefix: /}, upstream: b}]\npools:\n  b: [{host: h, port: 1}]\n",
        ))
        .unwrap();
        let d = t.apply(&cfg(
            "routes: [{match: {prefix: /}, upstream: b}]\npools:\n  b:\n    load_balance: random\n    backends: [{host: h, port: 1}]\n",
        ));
        assert_eq!(d.changed, vec!["/"]);
        assert_eq!(t.algo_for("b"), LoadBalance::Random);
    }

    #[test]
    fn the_probe_snapshot_is_flattened_per_backend_with_its_index() {
        let t = RouteTable::build(&canonical()).unwrap();
        let got = t.all_backends("/healthz");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, 0);
        assert_eq!(got[0].2, "http://10.0.0.1:8080/healthz");
        assert_eq!(got[1].1, 1);
    }

    #[test]
    fn the_inline_shorthand_still_builds_a_working_table() {
        let mut t = RouteTable::build(&cfg(r#"
routes:
  - path: /api
    upstreams:
      - { url: "http://10.0.0.1:8080" }
      - { url: "http://10.0.0.2:8080" }
  - path: "*"
    upstreams: [{ url: "http://10.0.0.3:8080" }]
"#))
        .unwrap();
        assert_eq!(t.backends_of("/api").len(), 2);
        assert_eq!(t.resolve("/anything", None).unwrap().pool, "*");
        let d = t.apply(&cfg(r#"
routes:
  - path: /api
    upstreams:
      - { url: "http://10.0.0.1:8080" }
  - path: "*"
    upstreams: [{ url: "http://10.0.0.3:8080" }]
"#));
        assert_eq!(
            d.backends["/api"],
            ReconcileOutcome {
                added: 0,
                removed: 1,
                reweighted: 0
            }
        );
    }

    #[test]
    fn a_pool_entry_round_trips_through_the_registry() {
        let t = RouteTable::build(&cfg(
            "routes: [{match: {prefix: /}, upstream: b}]\npools:\n  b: [{host: a, port: 8443, scheme: https}]\n",
        ))
        .unwrap();
        assert_eq!(t.backends_of("b"), ["https://a:8443"]);
    }

    #[test]
    fn an_empty_config_yields_an_empty_table_that_answers_nothing() {
        let t = RouteTable::build(&ProxyConfig::default()).unwrap();
        assert!(t.resolve("/", None).is_none());
        assert!(t.backends_of("anything").is_empty());
        let _ = backend("h", 1);
        let _ = PoolCfg::List(vec![]);
    }
}
