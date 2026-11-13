//! The reverse-proxy forwarding driver behind `relayfold-proxy`.
//!
//! Selection (round robin, random, weighted, consistent hash, lowest estimated latency) and the
//! per-upstream health flag come from `relayfold::Registry`; the forwarding itself is a servelet
//! client call. The pool is registered under the wildcard route `"*"`, so every request goes
//! through the load balancer; a request with no healthy upstream gets `503`, a failed or timed-out
//! upstream call gets `502`.
//!
//! What this does **not** do: it buffers whole request and response bodies (servelet's client and
//! `Response` are buffered), follows no redirects (they are passed through to the caller), and
//! speaks HTTP/1.1 to upstreams only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use relayfold::{LoadBalance, Registry, Upstream};
use servelet_client::Client;
use servelet_core::{HeaderMap, Request, Response, StatusCode};

/// Headers that describe one connection, not the message (RFC 9110 §7.6.1), plus the framing
/// headers servelet's wire writer sets itself.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "host",
];

/// Copy `src` minus the fields that describe one connection rather than the message: the fixed
/// hop-by-hop list, plus every field a `Connection` header names (RFC 9110 §7.6.1), matched
/// case-insensitively across all `Connection` fields in the message.
fn end_to_end_headers(src: &HeaderMap) -> HeaderMap {
    let nominated: Vec<String> = src
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .filter_map(|(_, value)| value.to_str().ok())
        .flat_map(|v| {
            v.split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in src.iter() {
        let lower = name.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || nominated.contains(&lower) {
            continue;
        }
        out.append(name, value.clone());
    }
    out
}

/// Parse `http://host:port` / `https://host:port` (port optional) into an [`Upstream`].
#[must_use]
pub fn parse_upstream(addr: &str) -> Upstream {
    if let Some(hostport) = addr.strip_prefix("https://") {
        let (h, p) = hostport.rsplit_once(':').unwrap_or((hostport, "443"));
        Upstream::https(h, p.parse().unwrap_or(443))
    } else {
        let a = addr.trim_start_matches("http://");
        let (h, p) = a.rsplit_once(':').unwrap_or((a, "80"));
        Upstream::http(h, p.parse().unwrap_or(80))
    }
}

/// A load-balancing reverse proxy. Cloning is cheap and clones share health and rotation state.
#[derive(Clone)]
pub struct Proxy {
    registry: Registry,
    /// The upstreams in registration order: the index `Registry::set_health` takes.
    upstreams: Arc<Vec<Upstream>>,
    client: Client,
    timeout: Duration,
}

impl Proxy {
    /// A proxy over `addrs` (`http://host:port` or `https://host:port`).
    #[must_use]
    pub fn new(lb: LoadBalance, addrs: &[String]) -> Self {
        let mut registry = Registry::new().load_balance(lb);
        let upstreams: Vec<Upstream> = addrs.iter().map(|a| parse_upstream(a)).collect();
        for u in &upstreams {
            registry = registry.upstream("*", u.clone());
        }
        Proxy {
            registry,
            upstreams: Arc::new(upstreams),
            client: Client::builder().redirect(false).build(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Per-request upstream timeout (default 30 s).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Forward `req` to the upstream the load balancer picks.
    pub async fn handle(&self, req: Request) -> Response {
        let path_and_query = req.uri().path_and_query().to_string();
        let Some((upstream, estimator)) = self.registry.pick("*", req.path()) else {
            return Response::new(StatusCode::SERVICE_UNAVAILABLE, "no healthy upstream");
        };
        let url = upstream.url_for(&path_and_query);
        let estimator = Arc::clone(estimator);

        let mut headers = end_to_end_headers(req.headers());
        if let Some(peer) = req.peer_addr() {
            let ip = peer.ip().to_string();
            let forwarded = match req
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
            {
                Some(prior) => format!("{prior}, {ip}"),
                None => ip,
            };
            headers.insert("x-forwarded-for", forwarded);
        }

        let method = req.method().clone();
        let body = req.into_body();
        let started = Instant::now();
        let call = self
            .client
            .request(method, &url)
            .headers(&headers)
            .body_bytes(body)
            .send();
        match tokio::time::timeout(self.timeout, call).await {
            Ok(Ok(upstream_resp)) => {
                estimator.observe(started.elapsed().as_secs_f64());
                let mut out = Response::new(upstream_resp.status, upstream_resp.body.clone());
                out.headers = end_to_end_headers(&upstream_resp.headers);
                out
            }
            Ok(Err(e)) => Response::new(StatusCode::BAD_GATEWAY, format!("upstream error: {e}")),
            Err(_) => Response::new(
                StatusCode::from_u16(504).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream timed out",
            ),
        }
    }

    /// Probe every upstream's `path` every `interval` and mark it up (a response below 500) or
    /// down (an error, a timeout, or a 5xx). A down upstream is skipped by selection until the
    /// next probe finds it up. Abort the returned handle to stop probing.
    #[must_use]
    pub fn spawn_health_checks(
        &self,
        path: String,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let registry = self.registry.clone();
        let upstreams = Arc::clone(&self.upstreams);
        let client = self.client.clone();
        tokio::spawn(async move {
            loop {
                for (i, u) in upstreams.iter().enumerate() {
                    let probe = client.get(u.url_for(&path)).send();
                    let up = matches!(
                        tokio::time::timeout(Duration::from_secs(3), probe).await,
                        Ok(Ok(r)) if r.status.as_u16() < 500
                    );
                    registry.set_health("*", i, up);
                }
                tokio::time::sleep(interval).await;
            }
        })
    }
}
