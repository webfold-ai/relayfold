//! A real, bounded nginx config parser — NOT a general nginx grammar. Covers exactly:
//! `upstream NAME { server HOST:PORT [weight=N]; ... }` blocks, and `server { listen PORT;
//! server_name NAME; location PATH { proxy_pass http[s]://TARGET; } }` blocks, where TARGET is
//! either a literal `host:port` or an `upstream` block's name.
//!
//! Explicitly NOT covered (rejected with a clear error naming the unsupported directive, never
//! silently dropped or guessed): regex locations (`location ~ ...`), `if`/`rewrite`/`return`,
//! SSL directives, `map`/`geo`, nested `location` blocks, any `server`/`upstream` directive not
//! listed above. A config using one of these fails to convert rather than silently producing an
//! incomplete or wrong `ProxyConfig`.

use crate::config::{ProxyConfig, Route, UpstreamCfg};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, PartialEq)]
pub struct NginxParseError(pub String);

impl fmt::Display for NginxParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NginxParseError {}

/// Strip `#`-comments and collapse to one directive/brace token per logical unit, so the block
/// walker below never has to think about whitespace or trailing comments.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw_line in input.lines() {
        let line = match raw_line.find('#') {
            Some(i) => &raw_line[..i],
            None => raw_line,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Split on `{`/`}`/`;` while keeping them as their own tokens.
        let mut current = String::new();
        for c in line.chars() {
            if c == '{' || c == '}' || c == ';' {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    tokens.push(trimmed.to_string());
                }
                tokens.push(c.to_string());
                current.clear();
            } else {
                current.push(c);
            }
        }
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            tokens.push(trimmed.to_string());
        }
    }
    tokens
}

struct UpstreamBlock {
    servers: Vec<UpstreamCfg>,
}

/// Parse `server HOST:PORT [weight=N];` — the only `upstream`-block directive this parser
/// understands. Anything else inside an `upstream {}` block is a hard error.
fn parse_upstream_body(directives: &[String]) -> Result<UpstreamBlock, NginxParseError> {
    let mut servers = Vec::new();
    for d in directives {
        let mut parts = d.split_whitespace();
        let Some(kw) = parts.next() else { continue };
        if kw != "server" {
            return Err(NginxParseError(format!(
                "unsupported upstream directive: {d:?} (only `server` is)"
            )));
        }
        let rest: Vec<&str> = parts.collect();
        let Some(addr) = rest.first() else {
            return Err(NginxParseError(
                "`server` directive missing an address".into(),
            ));
        };
        let Some((host, port_str)) = addr.rsplit_once(':') else {
            return Err(NginxParseError(format!(
                "server address {addr:?} must be host:port"
            )));
        };
        let port: u16 = port_str
            .parse()
            .map_err(|_| NginxParseError(format!("invalid port in {addr:?}")))?;
        let weight = rest
            .iter()
            .skip(1)
            .find_map(|p| p.strip_prefix("weight="))
            .map(|w| {
                w.parse::<u32>()
                    .map_err(|_| NginxParseError(format!("invalid weight in {d:?}")))
            })
            .transpose()?
            .unwrap_or(1);
        servers.push(UpstreamCfg {
            path: String::new(),
            scheme: "http".into(),
            host: host.to_string(),
            port,
            weight,
        });
    }
    Ok(UpstreamBlock { servers })
}

struct LocationBlock {
    path: String,
    proxy_pass: String,
}

fn parse_server_body(directives: &[BlockItem]) -> Result<Vec<LocationBlock>, NginxParseError> {
    let mut locations = Vec::new();
    for item in directives {
        match item {
            BlockItem::Directive(d) => {
                let kw = d.split_whitespace().next().unwrap_or("");
                if kw != "listen" && kw != "server_name" {
                    return Err(NginxParseError(format!(
                        "unsupported server directive: {d:?}"
                    )));
                }
                // `listen`/`server_name` are accepted but not needed to build a `ProxyConfig`
                // (routes are keyed by `location` path, not by listen port) — parsed, not used.
            }
            BlockItem::Block { header, body } => {
                let mut parts = header.split_whitespace();
                let kw = parts.next().unwrap_or("");
                if kw != "location" {
                    return Err(NginxParseError(format!(
                        "unsupported nested block in server: {header:?}"
                    )));
                }
                let path = parts
                    .next()
                    .ok_or_else(|| NginxParseError("location missing a path".into()))?;
                if path.starts_with('~') {
                    return Err(NginxParseError(format!(
                        "regex locations are not supported: {header:?}"
                    )));
                }
                let mut proxy_pass = None;
                for inner in body {
                    match inner {
                        BlockItem::Directive(d) => {
                            let mut p = d.split_whitespace();
                            if p.next() == Some("proxy_pass") {
                                proxy_pass = p.next().map(str::to_string);
                            } else {
                                return Err(NginxParseError(format!(
                                    "unsupported location directive: {d:?}"
                                )));
                            }
                        }
                        BlockItem::Block { header, .. } => {
                            return Err(NginxParseError(format!(
                                "nested blocks in location are not supported: {header:?}"
                            )));
                        }
                    }
                }
                let proxy_pass = proxy_pass.ok_or_else(|| {
                    NginxParseError(format!("location {path:?} has no proxy_pass"))
                })?;
                locations.push(LocationBlock {
                    path: path.to_string(),
                    proxy_pass,
                });
            }
        }
    }
    Ok(locations)
}

enum BlockItem {
    Directive(String),
    Block {
        header: String,
        body: Vec<BlockItem>,
    },
}

/// Group a flat token stream into a directive/block tree — one level of real recursion, which is
/// all this parser's supported grammar (`upstream`/`server`/`location`) ever needs.
fn parse_blocks(tokens: &[String], pos: &mut usize) -> Result<Vec<BlockItem>, NginxParseError> {
    let mut items = Vec::new();
    let mut pending = String::new();
    while *pos < tokens.len() {
        let tok = &tokens[*pos];
        match tok.as_str() {
            "{" => {
                let header = pending.trim().to_string();
                pending.clear();
                *pos += 1;
                let body = parse_blocks(tokens, pos)?;
                items.push(BlockItem::Block { header, body });
            }
            "}" => {
                *pos += 1;
                return Ok(items);
            }
            ";" => {
                if !pending.trim().is_empty() {
                    items.push(BlockItem::Directive(pending.trim().to_string()));
                }
                pending.clear();
                *pos += 1;
            }
            other => {
                if !pending.is_empty() {
                    pending.push(' ');
                }
                pending.push_str(other);
                *pos += 1;
            }
        }
    }
    Ok(items)
}

/// Convert a real nginx config (the bounded subset described in the module doc) into a
/// [`ProxyConfig`]. `weighted` is used as the route's load-balance algorithm whenever an upstream
/// has more than one server (nginx's own default without an explicit balancing directive is a
/// weighted round-robin) — plain `round_robin` for a single-server upstream or a bare
/// `host:port` proxy_pass target, since weight is meaningless with one candidate.
pub fn convert(input: &str) -> Result<ProxyConfig, NginxParseError> {
    let tokens = tokenize(input);
    let mut pos = 0;
    let top = parse_blocks(&tokens, &mut pos)?;

    let mut upstreams: HashMap<String, UpstreamBlock> = HashMap::new();
    let mut locations: Vec<LocationBlock> = Vec::new();

    for item in &top {
        match item {
            BlockItem::Directive(d) => {
                return Err(NginxParseError(format!(
                    "unsupported top-level directive: {d:?}"
                )));
            }
            BlockItem::Block { header, body } => {
                let mut parts = header.split_whitespace();
                match parts.next() {
                    Some("upstream") => {
                        let name = parts.next().ok_or_else(|| {
                            NginxParseError("upstream block missing a name".into())
                        })?;
                        let flat: Vec<String> = body
                            .iter()
                            .map(|b| match b {
                                BlockItem::Directive(d) => Ok(d.clone()),
                                BlockItem::Block { header, .. } => Err(NginxParseError(format!(
                                    "nested block in upstream: {header:?}"
                                ))),
                            })
                            .collect::<Result<_, _>>()?;
                        upstreams.insert(name.to_string(), parse_upstream_body(&flat)?);
                    }
                    Some("server") => {
                        locations.extend(parse_server_body(body)?);
                    }
                    _ => {
                        return Err(NginxParseError(format!(
                            "unsupported top-level block: {header:?}"
                        )))
                    }
                }
            }
        }
    }

    let mut routes = Vec::new();
    let mut cfg_upstreams = Vec::new();
    for loc in locations {
        let target = loc
            .proxy_pass
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let scheme = if loc.proxy_pass.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        let servers: Vec<UpstreamCfg> = if let Some(block) = upstreams.get(target) {
            block
                .servers
                .iter()
                .cloned()
                .map(|mut u| {
                    u.scheme = scheme.to_string();
                    u.path = loc.path.clone();
                    u
                })
                .collect()
        } else {
            let Some((host, port_str)) = target.rsplit_once(':') else {
                return Err(NginxParseError(format!(
                    "proxy_pass target {target:?} is neither a known upstream nor host:port"
                )));
            };
            let port: u16 = port_str
                .parse()
                .map_err(|_| NginxParseError(format!("invalid port in proxy_pass {target:?}")))?;
            vec![UpstreamCfg {
                path: loc.path.clone(),
                scheme: scheme.to_string(),
                host: host.to_string(),
                port,
                weight: 1,
            }]
        };
        let load_balance = if servers.len() > 1 {
            "weighted"
        } else {
            "round_robin"
        };
        routes.push(Route {
            path: loc.path,
            load_balance: load_balance.to_string(),
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
    fn a_single_upstream_location_converts() {
        let input = r#"
            server {
                listen 80;
                server_name example.com;
                location /api {
                    proxy_pass http://127.0.0.1:9000;
                }
            }
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].path, "/api");
        assert_eq!(cfg.routes[0].load_balance, "round_robin");
        assert_eq!(
            cfg.upstreams,
            vec![UpstreamCfg {
                path: "/api".into(),
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port: 9000,
                weight: 1
            }]
        );
    }

    #[test]
    fn a_weighted_upstream_block_converts() {
        let input = r#"
            upstream backend {
                server 10.0.0.1:8080 weight=3;
                server 10.0.0.2:8080 weight=1;
            }
            server {
                listen 443;
                location / {
                    proxy_pass http://backend;
                }
            }
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].load_balance, "weighted");
        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[0].weight, 3);
        assert_eq!(cfg.upstreams[1].weight, 1);
    }

    #[test]
    fn multiple_locations_become_multiple_routes() {
        let input = r#"
            server {
                location /a { proxy_pass http://10.0.0.1:80; }
                location /b { proxy_pass http://10.0.0.2:80; }
            }
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(
            cfg.routes
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/a", "/b"]
        );
    }

    #[test]
    fn a_regex_location_is_a_real_error_not_a_silent_skip() {
        let input = r#"
            server {
                location ~ \.php$ { proxy_pass http://10.0.0.1:80; }
            }
        "#;
        let err = convert(input).unwrap_err();
        assert!(err.0.contains("regex"), "got: {err}");
    }

    #[test]
    fn an_unsupported_directive_is_a_real_error_not_a_silent_skip() {
        let input = r#"
            server {
                location / {
                    proxy_pass http://10.0.0.1:80;
                    rewrite ^/old$ /new permanent;
                }
            }
        "#;
        let err = convert(input).unwrap_err();
        assert!(err.0.contains("rewrite"), "got: {err}");
    }

    #[test]
    fn a_literal_host_port_proxy_pass_without_an_upstream_block_works() {
        let input = r#"
            server {
                location / { proxy_pass http://192.168.1.5:3000; }
            }
        "#;
        let cfg = convert(input).unwrap();
        assert_eq!(cfg.upstreams[0].host, "192.168.1.5");
        assert_eq!(cfg.upstreams[0].port, 3000);
    }

    #[test]
    fn a_real_fixture_file_converts_end_to_end() {
        let input = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/example.nginx.conf"
        ))
        .unwrap();
        let cfg = convert(&input).unwrap();
        assert!(!cfg.routes.is_empty());
        // Also prove the result actually renders through the shared YAML surface.
        let yaml = cfg.to_yaml();
        assert!(yaml.contains("routes"));
    }
}
