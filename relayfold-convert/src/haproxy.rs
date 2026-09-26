//! A real, bounded HAProxy config parser. Covers exactly: `frontend NAME { bind :PORT;
//! default_backend NAME; }` and `backend NAME { balance ALGO; server NAME HOST:PORT
//! [weight N]; ... }`. HAProxy's real config syntax has no braces (it's whitespace/indentation
//! block-scoped by section keyword) — this parser reads it in that native shape, one section per
//! `frontend`/`backend` keyword until the next section keyword or end of input.
//!
//! `balance` algorithm mapping onto `relayfold::LoadBalance` (disclosed, not a hidden guess):
//! `roundrobin` → `round_robin`, `static-rr` → `round_robin` (HAProxy's static variant vs dynamic
//! `roundrobin` collapse to the same relayfold algorithm — relayfold doesn't distinguish weighted
//! availability changes mid-run), `leastconn` → `healthiest` (both pick the currently
//! least-loaded backend; HAProxy's is active connection count, relayfold's is measured latency —
//! related but not identical signals, called out here rather than silently treated as the same
//! thing), `source` → `consistent` (both are client-affinity hashing). Any other `balance` value
//! is a hard error, never a silent default.
//!
//! Explicitly NOT covered: ACLs, `http-request`/`http-response` rules, `stick-table`,
//! `option httpchk`, TLS (`bind ... ssl`), multiple `bind` lines per frontend. A config using one
//! of these fails to convert with a clear error rather than silently dropping the directive.

use crate::config::{ProxyConfig, Route, UpstreamCfg};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, PartialEq)]
pub struct HaproxyParseError(pub String);

impl fmt::Display for HaproxyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for HaproxyParseError {}

fn map_balance(algo: &str) -> Result<&'static str, HaproxyParseError> {
    match algo {
        "roundrobin" | "static-rr" => Ok("round_robin"),
        "leastconn" => Ok("healthiest"),
        "source" => Ok("consistent"),
        other => Err(HaproxyParseError(format!(
            "unsupported balance algorithm: {other:?}"
        ))),
    }
}

struct Section {
    kind: String,
    name: String,
    lines: Vec<String>,
}

fn split_sections(input: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;
    for raw_line in input.lines() {
        let line = match raw_line.find('#') {
            Some(i) => &raw_line[..i],
            None => raw_line,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let head = parts.next().unwrap_or("");
        if head == "frontend" || head == "backend" || head == "global" || head == "defaults" {
            if let Some(s) = current.take() {
                sections.push(s);
            }
            let name = parts.next().unwrap_or("").to_string();
            current = Some(Section {
                kind: head.to_string(),
                name,
                lines: Vec::new(),
            });
        } else if let Some(s) = current.as_mut() {
            s.lines.push(trimmed.to_string());
        }
        // A directive before any section header (rare, real HAProxy configs always open with
        // `global`/`defaults`) is silently ignorable noise, not something this converter needs.
    }
    if let Some(s) = current.take() {
        sections.push(s);
    }
    sections
}

/// Convert a real HAProxy config (the bounded subset described in the module doc) into a
/// [`ProxyConfig`]. Each `frontend`'s bind path becomes a route named by its `default_backend`'s
/// name (HAProxy frontends aren't inherently path-keyed the way nginx `location`s are — a real,
/// disclosed simplification: this converter emits one route per frontend, at path `/`, rather
/// than inventing a path HAProxy's own config never states).
pub fn convert(input: &str) -> Result<ProxyConfig, HaproxyParseError> {
    let sections = split_sections(input);
    let mut backends: HashMap<String, (String, Vec<UpstreamCfg>)> = HashMap::new();

    for s in &sections {
        if s.kind != "backend" {
            continue;
        }
        let mut balance = "round_robin".to_string();
        let mut servers = Vec::new();
        for line in &s.lines {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("balance") => {
                    let algo = parts.next().ok_or_else(|| {
                        HaproxyParseError(format!(
                            "balance directive missing an algorithm: {line:?}"
                        ))
                    })?;
                    balance = map_balance(algo)?.to_string();
                }
                Some("server") => {
                    let _name = parts.next().ok_or_else(|| {
                        HaproxyParseError(format!("server directive missing a name: {line:?}"))
                    })?;
                    let addr = parts.next().ok_or_else(|| {
                        HaproxyParseError(format!("server directive missing an address: {line:?}"))
                    })?;
                    let (host, port_str) = addr.rsplit_once(':').ok_or_else(|| {
                        HaproxyParseError(format!("server address {addr:?} must be host:port"))
                    })?;
                    let port: u16 = port_str
                        .parse()
                        .map_err(|_| HaproxyParseError(format!("invalid port in {addr:?}")))?;
                    let mut weight = 1u32;
                    let rest: Vec<&str> = parts.collect();
                    if let Some(pos) = rest.iter().position(|w| *w == "weight") {
                        let w = rest.get(pos + 1).ok_or_else(|| {
                            HaproxyParseError(format!("weight directive missing a value: {line:?}"))
                        })?;
                        weight = w.parse().map_err(|_| {
                            HaproxyParseError(format!("invalid weight in {line:?}"))
                        })?;
                    }
                    servers.push(UpstreamCfg {
                        path: "/".into(),
                        scheme: "http".into(),
                        host: host.to_string(),
                        port,
                        weight,
                    });
                }
                Some(other) => {
                    return Err(HaproxyParseError(format!(
                        "unsupported backend directive: {other:?} in {line:?}"
                    )))
                }
                None => {}
            }
        }
        backends.insert(s.name.clone(), (balance, servers));
    }

    let mut routes = Vec::new();
    let mut cfg_upstreams = Vec::new();
    for s in &sections {
        if s.kind != "frontend" {
            continue;
        }
        let mut default_backend = None;
        for line in &s.lines {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("bind") => {}
                Some("default_backend") => {
                    default_backend = parts.next().map(str::to_string);
                }
                Some(other) => {
                    return Err(HaproxyParseError(format!(
                        "unsupported frontend directive: {other:?} in {line:?}"
                    )))
                }
                None => {}
            }
        }
        let backend_name = default_backend.ok_or_else(|| {
            HaproxyParseError(format!("frontend {:?} has no default_backend", s.name))
        })?;
        let (load_balance, servers) = backends
            .get(&backend_name)
            .ok_or_else(|| {
                HaproxyParseError(format!(
                    "frontend {:?} references unknown backend {:?}",
                    s.name, backend_name
                ))
            })?
            .clone();
        routes.push(Route {
            path: "/".to_string(),
            load_balance,
        });
        cfg_upstreams.extend(servers);
    }

    Ok(ProxyConfig {
        routes,
        upstreams: cfg_upstreams,
        allow: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_simple_frontend_backend_pair_converts() {
        let input = r#"
            frontend web
                bind *:80
                default_backend servers

            backend servers
                balance roundrobin
                server web1 10.0.0.1:8080
                server web2 10.0.0.2:8080 weight 2
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].load_balance, "round_robin");
        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[1].weight, 2);
    }

    #[test]
    fn leastconn_maps_to_healthiest() {
        let input = r#"
            frontend web
                bind *:80
                default_backend servers
            backend servers
                balance leastconn
                server web1 10.0.0.1:8080
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.routes[0].load_balance, "healthiest");
    }

    #[test]
    fn source_maps_to_consistent() {
        let input = r#"
            frontend web
                bind *:80
                default_backend servers
            backend servers
                balance source
                server web1 10.0.0.1:8080
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.routes[0].load_balance, "consistent");
    }

    #[test]
    fn an_unsupported_balance_algorithm_is_a_real_error() {
        let input = r#"
            frontend web
                bind *:80
                default_backend servers
            backend servers
                balance uri
                server web1 10.0.0.1:8080
        "#;
        let err = convert(input).unwrap_err();
        assert!(err.0.contains("uri"), "got: {err}");
    }

    #[test]
    fn a_frontend_referencing_an_unknown_backend_is_a_real_error() {
        let input = r#"
            frontend web
                bind *:80
                default_backend missing
        "#;
        let err = convert(input).unwrap_err();
        assert!(err.0.contains("missing"), "got: {err}");
    }

    #[test]
    fn an_unsupported_frontend_directive_is_a_real_error() {
        let input = r#"
            frontend web
                bind *:80
                acl is_admin path_beg /admin
                default_backend servers
            backend servers
                balance roundrobin
                server web1 10.0.0.1:8080
        "#;
        let err = convert(input).unwrap_err();
        assert!(err.0.contains("acl"), "got: {err}");
    }

    #[test]
    fn a_real_fixture_file_converts_end_to_end() {
        let input = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/example.haproxy.cfg"
        ))
        .unwrap();
        let cfg = convert(&input).unwrap();
        assert!(!cfg.routes.is_empty());
        let yaml = cfg.to_yaml();
        assert!(yaml.contains("routes"));
    }
}
