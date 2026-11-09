//! `relayfold`'s own declarative route config — didn't exist before this crate. `relayfold`
//! itself (github.com/Ptyktos/relayfold) is built up purely in Rust today
//! (`Registry::new().upstream(route, Upstream::http(host, port)).load_balance(algo).allow(...)`)
//! — there was no file format to convert INTO until this crate defined one. Deliberately a plain
//! YAML shape (via `wirefold::codec::syntax::Surface::Yaml`, never `serde_yaml`), not a new
//! DSL like zentinel's own KDL target format.
//!
//! **Why `upstreams` is a flat, path-joined list instead of nested under each route** (a real
//! constraint, not a style choice): wirefold's own `render_yaml` is explicitly documented as
//! "mainly for round-trip testing, not YAML generation" — and empirically, a `Fourth` list nested
//! inside a `Bu` mapping that is itself an item of another `Fourth` list (route → its own
//! `upstreams` list, three levels of nesting) renders with broken indentation and fails to parse
//! back. Two-level nesting (a flat list of flat-scalar mappings) round-trips perfectly — verified
//! directly against `wirefold::codec::syntax::{render_surface, parse_surface}` before choosing
//! this shape. So `routes` carries only path + algorithm, and `upstreams` is its own top-level
//! list where each entry names which route it belongs to by `path` — a relational join, not a
//! nested tree. Loading this into `relayfold::Registry` groups `upstreams` by `path` and calls
//! `.upstream(path, ...)` once per matching entry.
//!
//! Loading this into an actual `Registry` (a `relayfold` dependency + a small `apply()` function)
//! is deliberately NOT done here — this crate's job is emitting a correct config, not depending
//! on `relayfold` itself. That loader is one straightforward function for whoever wires it in.

use wirefold::codec::syntax::{
    field_list, field_opt, field_str, term_str, AsTerm, Atom, Surface, SyntaxError, SyntaxTerm,
};

/// One route: a path prefix and a load-balancing algorithm name. Its upstream pool lives in
/// [`ProxyConfig::upstreams`], joined by `path` — see the module doc for why.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub path: String,
    /// One of `round_robin` / `random` / `weighted` / `consistent` / `healthiest` — the exact
    /// lowercase-snake-case spelling of `relayfold::LoadBalance`'s variants, so a loader's mapping
    /// back onto the enum is a plain string match, never a fuzzy guess.
    pub load_balance: String,
}

/// One upstream, tagged with the route `path` it belongs to — mirrors
/// `relayfold::Upstream::http`/`https`/`with_weight` exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct UpstreamCfg {
    pub path: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub weight: u32,
}

/// One `Relationships::relate` tuple — mirrors `relayfold::Registry::allow` exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct AllowRule {
    pub subject: String,
    pub relation: String,
    pub object: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ProxyConfig {
    pub routes: Vec<Route>,
    pub upstreams: Vec<UpstreamCfg>,
    pub allow: Vec<AllowRule>,
}

impl ProxyConfig {
    /// This route's upstreams, in the order they appear in [`Self::upstreams`].
    #[must_use]
    pub fn upstreams_for<'a>(&'a self, path: &str) -> Vec<&'a UpstreamCfg> {
        self.upstreams.iter().filter(|u| u.path == path).collect()
    }

    #[must_use]
    pub fn to_yaml(&self) -> String {
        wirefold::codec::syntax::render_surface(&self.to_term(), Surface::Yaml)
    }

    pub fn from_yaml(input: &str) -> Result<Self, SyntaxError> {
        let term = wirefold::codec::syntax::parse_surface(Surface::Yaml, input)?;
        Self::from_term(&term)
    }
}

impl AsTerm for UpstreamCfg {
    fn to_term(&self) -> SyntaxTerm {
        SyntaxTerm::Bu(vec![
            ("path".into(), SyntaxTerm::B(Atom::Str(self.path.clone()))),
            (
                "scheme".into(),
                SyntaxTerm::B(Atom::Str(self.scheme.clone())),
            ),
            ("host".into(), SyntaxTerm::B(Atom::Str(self.host.clone()))),
            (
                "port".into(),
                SyntaxTerm::B(Atom::Num(self.port.to_string())),
            ),
            (
                "weight".into(),
                SyntaxTerm::B(Atom::Num(self.weight.to_string())),
            ),
        ])
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("upstream must be a mapping"));
        };
        let path = field_str(fields, "path")?.to_string();
        let scheme = field_str(fields, "scheme")?.to_string();
        let host = field_str(fields, "host")?.to_string();
        let port: u16 = field_str(fields, "port")?
            .parse()
            .map_err(|_| SyntaxError::new("port must be a valid u16"))?;
        let weight: u32 = field_opt(fields, "weight", |t| {
            term_str(t)?
                .parse()
                .map_err(|_| SyntaxError::new("weight must be a valid u32"))
        })?
        .unwrap_or(1);
        Ok(UpstreamCfg {
            path,
            scheme,
            host,
            port,
            weight,
        })
    }
}

impl AsTerm for Route {
    fn to_term(&self) -> SyntaxTerm {
        SyntaxTerm::Bu(vec![
            ("path".into(), SyntaxTerm::B(Atom::Str(self.path.clone()))),
            (
                "load_balance".into(),
                SyntaxTerm::B(Atom::Str(self.load_balance.clone())),
            ),
        ])
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("route must be a mapping"));
        };
        let path = field_str(fields, "path")?.to_string();
        let load_balance = field_opt(fields, "load_balance", |t| Ok(term_str(t)?.to_string()))?
            .unwrap_or_else(|| "round_robin".to_string());
        Ok(Route { path, load_balance })
    }
}

impl AsTerm for AllowRule {
    fn to_term(&self) -> SyntaxTerm {
        SyntaxTerm::Bu(vec![
            (
                "subject".into(),
                SyntaxTerm::B(Atom::Str(self.subject.clone())),
            ),
            (
                "relation".into(),
                SyntaxTerm::B(Atom::Str(self.relation.clone())),
            ),
            (
                "object".into(),
                SyntaxTerm::B(Atom::Str(self.object.clone())),
            ),
        ])
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("allow rule must be a mapping"));
        };
        Ok(AllowRule {
            subject: field_str(fields, "subject")?.to_string(),
            relation: field_str(fields, "relation")?.to_string(),
            object: field_str(fields, "object")?.to_string(),
        })
    }
}

impl AsTerm for ProxyConfig {
    fn to_term(&self) -> SyntaxTerm {
        SyntaxTerm::Bu(vec![
            (
                "routes".into(),
                SyntaxTerm::Fourth(self.routes.iter().map(AsTerm::to_term).collect()),
            ),
            (
                "upstreams".into(),
                SyntaxTerm::Fourth(self.upstreams.iter().map(AsTerm::to_term).collect()),
            ),
            (
                "allow".into(),
                SyntaxTerm::Fourth(self.allow.iter().map(AsTerm::to_term).collect()),
            ),
        ])
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("config must be a mapping"));
        };
        let routes = field_list(fields, "routes")?
            .iter()
            .map(Route::from_term)
            .collect::<Result<Vec<_>, _>>()?;
        let upstreams = field_list(fields, "upstreams")?
            .iter()
            .map(UpstreamCfg::from_term)
            .collect::<Result<Vec<_>, _>>()?;
        let allow = field_list(fields, "allow")?
            .iter()
            .map(AllowRule::from_term)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ProxyConfig {
            routes,
            upstreams,
            allow,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_round_trips_through_yaml() {
        let cfg = ProxyConfig {
            routes: vec![Route {
                path: "/api".into(),
                load_balance: "weighted".into(),
            }],
            upstreams: vec![
                UpstreamCfg {
                    path: "/api".into(),
                    scheme: "http".into(),
                    host: "10.0.0.1".into(),
                    port: 8080,
                    weight: 3,
                },
                UpstreamCfg {
                    path: "/api".into(),
                    scheme: "https".into(),
                    host: "10.0.0.2".into(),
                    port: 8443,
                    weight: 1,
                },
            ],
            allow: vec![AllowRule {
                subject: "user:alice".into(),
                relation: "read".into(),
                object: "route:/api".into(),
            }],
        };
        let yaml = cfg.to_yaml();
        let back = ProxyConfig::from_yaml(&yaml).expect("round trip must parse");
        assert_eq!(cfg, back);
    }

    #[test]
    fn a_missing_weight_defaults_to_one() {
        let yaml = "routes:\n  - path: /x\n    load_balance: round_robin\nupstreams:\n  - path: /x\n    scheme: http\n    host: a\n    port: 80\n";
        let cfg = ProxyConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.upstreams[0].weight, 1);
    }

    #[test]
    fn a_missing_load_balance_defaults_to_round_robin() {
        let yaml = "routes:\n  - path: /x\nupstreams:\n  - path: /x\n    scheme: http\n    host: a\n    port: 80\n";
        let cfg = ProxyConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.routes[0].load_balance, "round_robin");
    }

    #[test]
    fn upstreams_for_filters_by_route_path() {
        let cfg = ProxyConfig {
            routes: vec![
                Route {
                    path: "/a".into(),
                    load_balance: "round_robin".into(),
                },
                Route {
                    path: "/b".into(),
                    load_balance: "round_robin".into(),
                },
            ],
            upstreams: vec![
                UpstreamCfg {
                    path: "/a".into(),
                    scheme: "http".into(),
                    host: "h1".into(),
                    port: 1,
                    weight: 1,
                },
                UpstreamCfg {
                    path: "/b".into(),
                    scheme: "http".into(),
                    host: "h2".into(),
                    port: 2,
                    weight: 1,
                },
                UpstreamCfg {
                    path: "/a".into(),
                    scheme: "http".into(),
                    host: "h3".into(),
                    port: 3,
                    weight: 1,
                },
            ],
            allow: vec![],
        };
        let a: Vec<&str> = cfg
            .upstreams_for("/a")
            .iter()
            .map(|u| u.host.as_str())
            .collect();
        assert_eq!(a, vec!["h1", "h3"]);
    }
}
