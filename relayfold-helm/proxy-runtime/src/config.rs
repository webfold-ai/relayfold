//! The proxy-runtime's config file: schema, parse, validate.
//!
//! One YAML document (JSON is accepted too — same schema, and the extension decides which
//! parser is tried first so a `.json` file reports JSON errors rather than silently parsing as
//! YAML). Parsed at startup and again on every reload; see [`crate::reload`] for the triggers
//! and [`crate::routes`] for what a parsed config becomes.
//!
//! **The shape: routes name pools, pools hold backends.** A route is a *matching rule*
//! (prefix, optional host) and says which pool serves it; a pool is a *set of backends* with a
//! load-balancing algorithm. Splitting them is what lets two routes share one backend set
//! (`/api` and `/api/v2` both on `backend`), and lets one backend set serve several routes
//! without being written twice.
//!
//! ```yaml
//! listen: "0.0.0.0:8080"
//! header_limit: 8192
//! load_balance: round_robin        # the default for every pool that does not name one
//! routes:
//!   - match: { prefix: "/api" }
//!     upstream: backend
//! pools:
//!   backend:
//!     load_balance: weighted       # optional; overrides the global default
//!     backends:
//!       - { host: "10.0.60.20", port: 8080, weight: 3 }
//!       - { host: "10.0.60.21", port: 8080, weight: 1 }
//! health:
//!   path: "/healthz"
//!   interval_seconds: 10
//!   timeout_seconds: 2
//! ```
//!
//! **The inline shorthand.** A route may carry its own `upstreams` instead of naming a pool;
//! the parser desugars it into a pool named after the route. It is one line shorter for the
//! common one-route-one-pool case and it is what makes a diff legible — but it is sugar, not a
//! second schema, and the reload path treats the generated pool like any other.
//!
//! Unknown keys are a hard error (`deny_unknown_fields`), because a config key that is
//! silently ignored is a key that silently does not work.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Parse a config document. `text` is the file's contents; `path` chooses the JSON-before-YAML
/// order and appears in error messages.
pub fn parse(path: &Path, text: &str) -> Result<ProxyConfig, ConfigError> {
    if text.trim().is_empty() {
        // serde_yaml reads an empty document as `null` and, because every field of
        // `ProxyConfig` defaults, that deserializes to a valid all-defaults config — which
        // would silently start a proxy with no routes at all. An empty file is a mistake.
        return Err(ConfigError::Parse {
            path: path.display().to_string(),
            message:
                "the file is empty — an empty config is not the same as a config with no routes"
                    .to_string(),
        });
    }
    let json_first = matches!(path.extension().and_then(|e| e.to_str()), Some("json"))
        || text.trim_start().starts_with('{');

    let first = if json_first {
        serde_json::from_str::<ProxyConfig>(text).map_err(ConfigError::from)
    } else {
        serde_yaml::from_str::<ProxyConfig>(text).map_err(ConfigError::from)
    };
    match first {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            // The first parser may simply have been the wrong one for this file (a `.yaml`
            // holding JSON, or a `.json` holding YAML). Try the other before reporting, but
            // lead with the error from the parser the extension asked for.
            let second = if json_first {
                serde_yaml::from_str::<ProxyConfig>(text).map_err(ConfigError::from)
            } else {
                serde_json::from_str::<ProxyConfig>(text).map_err(ConfigError::from)
            };
            second.map_err(|e2| ConfigError::Parse {
                path: path.display().to_string(),
                message: format!("{e}\n(also tried the other parser: {e2})"),
            })
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        message: String,
    },
    Invalid(Vec<String>),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, source } => write!(f, "{path}: {source}"),
            ConfigError::Parse { path, message } => write!(f, "{path}: {message}"),
            ConfigError::Invalid(msgs) => {
                write!(f, "invalid config")?;
                for m in msgs {
                    write!(f, "\n  - {m}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    /// Did this fail because the file is simply not there? Distinct from "the file is there
    /// and is wrong": the first is a missing deployment artefact, the second is a deploy error
    /// that must stop the process rather than serve whatever the environment happens to say.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ConfigError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(e: serde_yaml::Error) -> Self {
        ConfigError::Parse {
            path: "<config>".into(),
            message: e.to_string(),
        }
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Parse {
            path: "<config>".into(),
            message: e.to_string(),
        }
    }
}

// ── Schema ──────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Schema version. Optional; anything but `1` is rejected rather than half-understood.
    #[serde(default)]
    pub version: Option<u32>,
    /// `host:port` to serve on. `LISTEN_ADDR` overrides this.
    #[serde(default)]
    pub listen: Option<String>,
    /// Total bytes allowed in the request header block. Read by the edge guards
    /// (`crate::edge_guards::Limits`), which is the only thing that enforces it.
    #[serde(default)]
    pub header_limit: Option<usize>,
    /// The load-balancing algorithm for every pool that does not name its own.
    #[serde(default)]
    pub load_balance: Option<LoadBalanceCfg>,
    /// Active health probing of every backend in every pool.
    #[serde(default)]
    pub health: Option<HealthCfg>,
    /// Edge TLS. Both paths must be set together or neither.
    #[serde(default)]
    pub tls: Option<TlsCfg>,
    /// Request header carrying the caller identity that `allow` rules are checked against.
    /// Falls back to the peer IP, then to `"anonymous"`. Default `x-forwarded-subject`.
    #[serde(default)]
    pub subject_header: Option<String>,
    /// Matching rules, most-specific-first at request time regardless of file order.
    #[serde(default)]
    pub routes: Vec<RouteCfg>,
    /// Backend sets, by name. A route's `upstream` must name one of these.
    #[serde(default)]
    pub pools: BTreeMap<String, PoolCfg>,
    /// Optional relayfold-control fleet source. Owns exactly one pool.
    #[serde(default)]
    pub control_plane: Option<ControlPlaneCfg>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceCfg {
    RoundRobin,
    Random,
    Weighted,
    Consistent,
    Healthiest,
}

impl LoadBalanceCfg {
    pub fn as_relayfold(self) -> relayfold::LoadBalance {
        match self {
            LoadBalanceCfg::RoundRobin => relayfold::LoadBalance::RoundRobin,
            LoadBalanceCfg::Random => relayfold::LoadBalance::Random,
            LoadBalanceCfg::Weighted => relayfold::LoadBalance::Weighted,
            LoadBalanceCfg::Consistent => relayfold::LoadBalance::Consistent,
            LoadBalanceCfg::Healthiest => relayfold::LoadBalance::Healthiest,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "round_robin" | "roundrobin" => Some(Self::RoundRobin),
            "random" => Some(Self::Random),
            "weighted" => Some(Self::Weighted),
            "consistent" => Some(Self::Consistent),
            "healthiest" => Some(Self::Healthiest),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthCfg {
    /// Path GETted on every backend. Default `/healthz`.
    #[serde(default)]
    pub path: Option<String>,
    /// Seconds between probe rounds. Default 10, floor 1. The alias accepts the older
    /// `interval_secs` spelling so an existing file keeps loading.
    #[serde(default, alias = "interval_secs")]
    pub interval_seconds: Option<u64>,
    /// Per-probe timeout. Default 2.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsCfg {
    pub cert_pem_path: String,
    pub key_pem_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneCfg {
    /// Base URL of relayfold-control, e.g. `http://relayfold-control:8080`.
    pub url: String,
    /// The pool whose backend set the control plane owns. Every other pool comes from this
    /// file alone, so a fleet refresh can never disturb an operator-authored pool.
    pub pool: String,
    /// Seconds between `/instances` polls. Default 15, floor 1.
    #[serde(default)]
    pub refresh_secs: Option<u64>,
}

/// One matching rule. Canonical form is `match: {prefix, host}` + `upstream: <pool>`; the
/// inline `path` + `upstreams` shorthand desugars to a pool named after the route.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteCfg {
    /// `{ prefix: "/api", host: "api.example.com" }`. `prefix` may be omitted (then `/`);
    /// `host` may be omitted or empty (then any host).
    #[serde(default, rename = "match")]
    pub matcher: Option<RouteMatchCfg>,
    /// Shorthand for `match.prefix`. `path` is an accepted alias — the spelling the inline
    /// shorthand and the Helm chart both use, so an existing config keeps loading.
    #[serde(default, alias = "path")]
    pub prefix: Option<String>,
    /// Shorthand for `match.host`.
    #[serde(default)]
    pub host: Option<String>,
    /// The pool that serves this route. Required unless `upstreams` is given.
    #[serde(default)]
    pub upstream: Option<String>,
    /// Forward with the matched prefix removed. Default false — the proxy is transparent.
    #[serde(default)]
    pub strip_prefix: Option<bool>,
    /// What `Consistent` hashes on: `path` (default), `client_ip`, or `header:<name>`.
    #[serde(default)]
    pub hash_key: Option<String>,
    /// Tuples that grant access to this route. Empty = open. Non-empty = closed.
    #[serde(default)]
    pub allow: Vec<AllowCfg>,
    /// The inline shorthand's backends.
    #[serde(default)]
    pub upstreams: Vec<UpstreamCfg>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteMatchCfg {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

/// A pool: a named backend set plus (optionally) its own algorithm. Deserializes from either
/// the bare list — `backend: [{host, port}]` — or the mapping form that can also carry
/// `load_balance`. Unknown keys fail: a `header_limit` written into a pool block is a mistake
/// that must not be silently dropped.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum PoolCfg {
    List(Vec<PoolEntry>),
    Detailed {
        #[serde(default)]
        load_balance: Option<LoadBalanceCfg>,
        backends: Vec<PoolEntry>,
    },
}

impl PoolCfg {
    pub fn backends(&self) -> &[PoolEntry] {
        match self {
            PoolCfg::List(v) => v,
            PoolCfg::Detailed { backends, .. } => backends,
        }
    }

    pub fn load_balance(&self) -> Option<LoadBalanceCfg> {
        match self {
            PoolCfg::List(_) => None,
            PoolCfg::Detailed { load_balance, .. } => *load_balance,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolEntry {
    pub host: String,
    /// Defaults to 80 for `http`, 443 for `https`.
    #[serde(default)]
    pub port: Option<u16>,
    /// `http` (default) or `https`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Relative weight for `weighted`. Default 1.
    #[serde(default)]
    pub weight: Option<u32>,
}

/// The inline shorthand's upstream form: a whole URL rather than host+port.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCfg {
    /// `http://host:port` or `https://host:port`.
    pub url: String,
    #[serde(default)]
    pub weight: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AllowCfg {
    pub subject: String,
    pub relation: String,
    /// The resource the relationship is about — conventionally this route's prefix. `*` grants
    /// for every object, which is `Relationships::check`'s own rule.
    pub object: String,
}

/// What `Consistent` hashes the request on to get affinity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashKeySource {
    Path,
    ClientIp,
    Header(String),
}

impl HashKeySource {
    pub fn parse(spec: Option<&str>) -> Result<Self, String> {
        match spec {
            None | Some("path") => Ok(Self::Path),
            Some("client_ip") => Ok(Self::ClientIp),
            Some(s) => match s.strip_prefix("header:") {
                Some(name) if !name.is_empty() => Ok(Self::Header(name.to_string())),
                _ => Err(format!(
                    "hash_key {spec:?} is not 'path', 'client_ip' or 'header:<name>'"
                )),
            },
        }
    }
}

// ── Desugaring and validation ───────────────────────────────────────────────────────────────

/// The pool name a route's inline `upstreams` shorthand desugars into. Keyed by the route's
/// own prefix so the name is meaningful in a log line, which is the entire benefit of naming
/// pools at all.
pub fn inline_pool_name(prefix: &str) -> String {
    if prefix.is_empty() || prefix == "/" {
        "*".to_string()
    } else {
        prefix.to_string()
    }
}

impl RouteCfg {
    /// The effective prefix, from `match.prefix` or the `prefix` shorthand. `"*"` normalises
    /// to `"/"`, which is what a catch-all is.
    pub fn effective_prefix(&self) -> String {
        let raw = self
            .matcher
            .as_ref()
            .and_then(|m| m.prefix.clone())
            .or_else(|| self.prefix.clone())
            .unwrap_or_else(|| "/".to_string());
        if raw == "*" {
            "/".to_string()
        } else {
            raw
        }
    }

    pub fn effective_host(&self) -> Option<String> {
        self.matcher
            .as_ref()
            .and_then(|m| m.host.clone())
            .or_else(|| self.host.clone())
            .filter(|h| !h.is_empty())
    }

    pub fn strip_prefix(&self) -> bool {
        self.strip_prefix.unwrap_or(false)
    }

    /// The pool this route resolves to, or `None` when the route is malformed — caught with a
    /// named message by `validate` before anything is served.
    pub fn pool_name(&self) -> Option<String> {
        self.upstream.clone().filter(|u| !u.is_empty()).or_else(|| {
            (!self.upstreams.is_empty()).then(|| inline_pool_name(&self.effective_prefix()))
        })
    }
}

impl ProxyConfig {
    /// Reject a document that parses but cannot mean anything. Every message names the
    /// offending route or pool, because the operator reading this is at 03:00 after a deploy
    /// and needs the line, not "invalid config".
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();
        if let Some(v) = self.version {
            if v != 1 {
                errs.push(format!(
                    "version: {v} is not supported (this build reads version 1)"
                ));
            }
        }
        if let Some(h) = self.header_limit {
            if h == 0 {
                errs.push(
                    "header_limit must be greater than zero (0 would refuse every request)".into(),
                );
            }
        }
        if let Some(tls) = &self.tls {
            if tls.cert_pem_path.is_empty() {
                errs.push("tls.cert_pem_path is empty".into());
            }
            if tls.key_pem_path.is_empty() {
                errs.push("tls.key_pem_path is empty".into());
            }
        }

        // A route's inline backends become a real pool, so desugar BEFORE validating pools —
        // otherwise the shorthand would always look like a route pointing at a missing pool.
        let mut pools = self.pools.clone();
        for (i, r) in self.routes.iter().enumerate() {
            let at = format!("routes[{i}] ({})", r.effective_prefix());
            if let Some(n) = r.pool_name() {
                if !r.upstreams.is_empty() && self.pools.contains_key(&n) {
                    errs.push(format!(
                        "{at}: declares inline `upstreams` AND names pool {n:?} — write one or the other"
                    ));
                } else if !r.upstreams.is_empty() {
                    pools.entry(n.clone()).or_insert_with(|| PoolCfg::Detailed {
                        load_balance: None,
                        backends: Vec::new(),
                    });
                    let entry = pools.get_mut(&n).expect("just inserted");
                    let PoolCfg::Detailed { backends, .. } = entry else {
                        unreachable!("inline insertion always makes the Detailed form")
                    };
                    backends.extend(r.upstreams.iter().filter_map(pool_entry_of_upstream));
                }
            } else {
                errs.push(format!(
                    "{at}: needs `upstream: <pool>` or inline `upstreams:`"
                ));
            }
            let prefix = r.effective_prefix();
            if !prefix.starts_with('/') {
                errs.push(format!(
                    "{at}: match.prefix must start with '/' (got {prefix:?})"
                ));
            }
            for (j, a) in r.allow.iter().enumerate() {
                if a.subject.is_empty() || a.relation.is_empty() || a.object.is_empty() {
                    errs.push(format!(
                        "{at}.allow[{j}]: subject, relation and object must all be non-empty"
                    ));
                }
            }
            if let Some(h) = &r.hash_key {
                if let Err(e) = HashKeySource::parse(Some(h)) {
                    errs.push(format!("{at}: {e}"));
                }
            }
        }

        let mut seen: BTreeSet<(String, Option<String>)> = BTreeSet::new();
        for (i, r) in self.routes.iter().enumerate() {
            let key = (r.effective_prefix(), r.effective_host());
            if !seen.insert(key.clone()) {
                errs.push(format!(
                    "routes[{i}]: duplicate match for prefix {:?} host {:?} — the later one can never win",
                    key.0, key.1
                ));
            }
        }

        for (name, pool) in &pools {
            let at = format!("pool {name:?}");
            if pool.backends().is_empty() {
                errs.push(format!("{at}: at least one backend is required"));
            }
            for (j, b) in pool.backends().iter().enumerate() {
                if let Err(e) = b.as_upstream() {
                    errs.push(format!("{at}.backends[{j}]: {e}"));
                }
                if b.weight == Some(0) {
                    errs.push(format!(
                        "{at}.backends[{j}]: weight 0 would make this backend unreachable"
                    ));
                }
            }
        }
        for (i, r) in self.routes.iter().enumerate() {
            if let Some(n) = r.pool_name() {
                if !pools.contains_key(&n) {
                    errs.push(format!(
                        "routes[{i}]: upstream {n:?} is not one of the configured pools ({:?})",
                        pools.keys().collect::<Vec<_>>()
                    ));
                }
            }
        }

        if let Some(cp) = &self.control_plane {
            if cp.url.is_empty() {
                errs.push("control_plane.url is empty".into());
            }
            if cp.pool.is_empty() {
                errs.push("control_plane.pool is empty".into());
            } else if !pools.contains_key(&cp.pool) && !pools.is_empty() {
                errs.push(format!(
                    "control_plane.pool ({:?}) is not a configured pool",
                    cp.pool
                ));
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errs))
        }
    }

    /// Every pool the config describes, inline shorthands desugared. This is what the runtime
    /// registers with the `Registry`; a route is only ever a pointer into it.
    pub fn resolved_pools(&self) -> BTreeMap<String, PoolCfg> {
        let mut pools = self.pools.clone();
        for r in &self.routes {
            if r.upstreams.is_empty() {
                continue;
            }
            if let Some(n) = r.pool_name() {
                pools.entry(n.clone()).or_insert(PoolCfg::List(Vec::new()));
                let entry = pools.get_mut(&n).expect("just inserted");
                let (PoolCfg::List(backends) | PoolCfg::Detailed { backends, .. }) = entry;
                backends.extend(r.upstreams.iter().filter_map(pool_entry_of_upstream));
            }
        }
        pools
    }
}

impl PoolEntry {
    /// The `relayfold::Upstream` this backend describes.
    pub fn as_upstream(&self) -> Result<relayfold::Upstream, String> {
        let scheme = self.scheme.as_deref().unwrap_or("http");
        let https = match scheme {
            "https" => true,
            "http" => false,
            other => return Err(format!("scheme {other:?} is not http or https")),
        };
        if self.host.is_empty() {
            return Err("host is empty".into());
        }
        let port = self.port.unwrap_or(if https { 443 } else { 80 });
        if port == 0 {
            return Err("port 0 is not routable".into());
        }
        let u = if https {
            relayfold::Upstream::https(&self.host, port)
        } else {
            relayfold::Upstream::http(&self.host, port)
        };
        Ok(u.with_weight(self.weight.unwrap_or(1)))
    }
}

/// The inline shorthand's entry: a whole URL plus a weight, which `pool_entry_of` alone
/// would drop.
pub fn pool_entry_of_upstream(u: &UpstreamCfg) -> Option<PoolEntry> {
    let mut e = pool_entry_of(&u.url).ok()?;
    e.weight = u.weight;
    Some(e)
}

/// `http://host:port` → a `PoolEntry`. Used by the inline shorthand and by the control-plane
/// refresher, which discovers backends as URLs. Validation of the host/port/scheme is
/// [`PoolEntry::as_upstream`]'s job, because this function is also the control plane's URL
/// parser and it must not reject an address it cannot itself judge routable.
pub fn pool_entry_of(url: &str) -> Result<PoolEntry, String> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s @ ("http" | "https"), rest)) => (s.to_string(), rest.to_string()),
        Some((other, _)) => return Err(format!("scheme {other:?} is not http or https")),
        None => return Err(format!("{url:?} has no scheme — write http:// or https://")),
    };
    let rest = rest.trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            Some(
                p.parse::<u16>()
                    .map_err(|_| format!("port {p:?} is not a number"))?,
            ),
        ),
        None => (rest.to_string(), None),
    };
    Ok(PoolEntry {
        host,
        port,
        scheme: Some(scheme),
        weight: None,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL: &str = r#"
listen: "0.0.0.0:8080"
header_limit: 8192
load_balance: round_robin
routes:
  - match: { prefix: "/api", host: "" }
    upstream: backend
  - match: { prefix: "/" }
    upstream: backend
pools:
  backend:
    load_balance: weighted
    backends:
      - { host: "10.0.60.20", port: 8080, weight: 3 }
      - { host: "10.0.60.21", scheme: https }
health:
  path: /healthz
  interval_seconds: 5
  timeout_seconds: 1
"#;

    fn yaml() -> ProxyConfig {
        let c = parse(Path::new("/etc/relayfold/proxy.yaml"), CANONICAL).expect("parses");
        c.validate().expect("valid");
        c
    }

    #[test]
    fn the_canonical_schema_parses() {
        let c = yaml();
        assert_eq!(c.listen.as_deref(), Some("0.0.0.0:8080"));
        assert_eq!(c.header_limit, Some(8192));
        assert_eq!(c.routes.len(), 2);
        assert_eq!(c.routes[0].pool_name().as_deref(), Some("backend"));
        assert_eq!(c.routes[0].effective_prefix(), "/api");
        assert_eq!(
            c.routes[0].effective_host(),
            None,
            "an empty host means any host"
        );
        assert_eq!(
            c.pools["backend"].load_balance(),
            Some(LoadBalanceCfg::Weighted)
        );
        assert_eq!(c.pools["backend"].backends().len(), 2);
        // No port on the second backend: the scheme's default is what a client would dial.
        assert_eq!(
            c.pools["backend"].backends()[1]
                .as_upstream()
                .unwrap()
                .port(),
            443
        );
        assert_eq!(c.health.as_ref().unwrap().interval_seconds, Some(5));
    }

    #[test]
    fn a_pool_may_be_a_bare_backend_list() {
        let c = parse(
            Path::new("p.yaml"),
            "routes: [{match: {prefix: /}, upstream: b}]\npools:\n  b:\n    - {host: h, port: 1}\n",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.pools["b"].backends().len(), 1);
        assert_eq!(
            c.pools["b"].load_balance(),
            None,
            "falls back to the global default"
        );
    }

    #[test]
    fn the_inline_shorthand_desugars_into_a_named_pool() {
        let c = parse(
            Path::new("p.yaml"),
            r#"
routes:
  - path: /api
    upstreams:
      - { url: "http://a:1", weight: 3 }
      - { url: "http://b:2" }
"#,
        )
        .unwrap();
        c.validate().unwrap();
        let pools = c.resolved_pools();
        let p = &pools["/api"];
        assert_eq!(p.backends().len(), 2);
        assert_eq!(p.backends()[0].host, "a");
        assert_eq!(p.backends()[0].weight, Some(3));
        assert_eq!(p.backends()[1].port, Some(2));
        assert_eq!(c.routes[0].pool_name().as_deref(), Some("/api"));
    }

    #[test]
    fn the_catch_all_shorthand_names_the_star_pool() {
        let r = RouteCfg {
            prefix: Some("*".into()),
            ..RouteCfg::default()
        };
        assert_eq!(r.effective_prefix(), "/");
    }

    #[test]
    fn the_same_schema_parses_as_json() {
        let json = r#"{
          "load_balance": "consistent",
          "routes": [{"match": {"prefix": "/api"}, "upstream": "b",
                      "hash_key": "header:x-session"}],
          "pools": {"b": {"backends": [{"host": "a", "port": 1}]}}
        }"#;
        let c = parse(Path::new("/etc/relayfold/proxy.json"), json).expect("parses");
        c.validate().unwrap();
        assert_eq!(c.load_balance, Some(LoadBalanceCfg::Consistent));
        assert_eq!(c.routes[0].hash_key.as_deref(), Some("header:x-session"));
    }

    #[test]
    fn a_yaml_file_whose_content_is_json_still_parses() {
        let json = r#"{"routes":[{"match":{"prefix":"/"},"upstream":"b"}],"pools":{"b":[{"host":"h","port":1}]}}"#;
        let c = parse(Path::new("/etc/relayfold/proxy.yaml"), json).expect("parses");
        assert_eq!(c.routes.len(), 1);
    }

    #[test]
    fn an_unknown_field_is_an_error_not_a_silent_drop() {
        let bad = "routes: [{match: {prefix: /x}, upstream: b, nope: 1}]\npools: {b: [{host: h, port: 1}]}";
        let e = parse(Path::new("p.yaml"), bad).unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
    }

    #[test]
    fn a_header_limit_written_into_a_pool_block_is_rejected() {
        let bad = "routes: [{match: {prefix: /}, upstream: b}]\npools:\n  b:\n    load_balance: random\n    header_limit: 8192\n    backends: [{host: h, port: 1}]";
        let e = parse(Path::new("p.yaml"), bad).unwrap_err();
        // The two-variant `PoolCfg` is `untagged`, so serde cannot say WHICH key was wrong
        // — it says the pool block matched neither shape. Loud, but not precise; the cost of
        // accepting both the bare-list and the mapping form without a hand-written
        // Deserialize. Recorded here so it is a known cost and not a surprise at 03:00.
        assert!(e.to_string().contains("PoolCfg"), "{e}");
        assert!(e.to_string().contains("pools"), "{e}");
    }

    #[test]
    fn validation_names_the_offending_route_and_pool() {
        let c = ProxyConfig {
            routes: vec![
                RouteCfg {
                    matcher: Some(RouteMatchCfg {
                        prefix: Some("api".into()),
                        host: None,
                    }),
                    upstream: Some("nope".into()),
                    ..RouteCfg::default()
                },
                RouteCfg {
                    matcher: Some(RouteMatchCfg {
                        prefix: Some("/x".into()),
                        host: None,
                    }),
                    upstream: Some("empty".into()),
                    ..RouteCfg::default()
                },
            ],
            pools: [("empty".to_string(), PoolCfg::List(vec![]))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let msg = c.validate().unwrap_err().to_string();
        assert!(
            msg.contains("routes[0] (api): match.prefix must start with '/'"),
            "{msg}"
        );
        assert!(
            msg.contains("upstream \"nope\" is not one of the configured pools"),
            "{msg}"
        );
        assert!(
            msg.contains("pool \"empty\": at least one backend is required"),
            "{msg}"
        );
    }

    #[test]
    fn duplicate_matches_are_rejected_rather_than_shadowed() {
        let r = |p: &str| RouteCfg {
            matcher: Some(RouteMatchCfg {
                prefix: Some(p.into()),
                host: None,
            }),
            upstream: Some("b".into()),
            ..RouteCfg::default()
        };
        let c = ProxyConfig {
            routes: vec![r("/api"), r("/api")],
            pools: [(
                "b".to_string(),
                PoolCfg::List(vec![PoolEntry {
                    host: "h".into(),
                    port: Some(1),
                    scheme: None,
                    weight: None,
                }]),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate match"));
    }

    #[test]
    fn a_route_may_not_name_a_pool_and_also_carry_inline_upstreams() {
        let c = ProxyConfig {
            routes: vec![RouteCfg {
                matcher: Some(RouteMatchCfg {
                    prefix: Some("/api".into()),
                    host: None,
                }),
                upstream: Some("b".into()),
                upstreams: vec![UpstreamCfg {
                    url: "http://x:1".into(),
                    weight: None,
                }],
                ..RouteCfg::default()
            }],
            pools: [("b".to_string(), PoolCfg::List(vec![]))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("write one or the other"));
    }

    #[test]
    fn an_empty_route_list_is_valid() {
        assert!(ProxyConfig::default().validate().is_ok());
    }

    #[test]
    fn an_empty_document_is_rejected_rather_than_read_as_all_defaults() {
        assert!(parse(Path::new("p.yaml"), "").is_err());
    }

    #[test]
    fn hash_key_specs_parse_and_bad_ones_do_not() {
        assert_eq!(HashKeySource::parse(None).unwrap(), HashKeySource::Path);
        assert_eq!(
            HashKeySource::parse(Some("client_ip")).unwrap(),
            HashKeySource::ClientIp
        );
        assert_eq!(
            HashKeySource::parse(Some("header:x-session")).unwrap(),
            HashKeySource::Header("x-session".into())
        );
        assert!(HashKeySource::parse(Some("header:")).is_err());
        assert!(HashKeySource::parse(Some("path_prefix")).is_err());
    }

    #[test]
    fn url_parsing_defaults_the_port_by_scheme() {
        assert_eq!(pool_entry_of("http://h").unwrap().port, None);
        assert_eq!(pool_entry_of("http://h:9/").unwrap().port, Some(9));
        assert_eq!(pool_entry_of("http://h:9/").unwrap().host, "h");
        assert!(pool_entry_of("h:9").is_err());
        assert!(pool_entry_of("http://h:x").is_err());
        // Port 0 is caught when the entry becomes an Upstream, which is the only place that
        // can also judge the scheme default.
        let zero = pool_entry_of("http://h:0").unwrap();
        assert!(zero.as_upstream().is_err(), "{:?}", zero.as_upstream());
    }
}
