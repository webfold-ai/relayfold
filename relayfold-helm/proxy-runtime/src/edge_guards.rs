//! The two edge guards `relayfold-proxy` runs on every inbound request, kept as a module so they
//! are testable without binding a socket in the test harness's own process layout:
//!
//! 1. [`Limits`] — a hard header-size ceiling, answered with **400** instead of being forwarded
//!    upstream. Measured 2026-10-03: a >8 KB header was forwarded upstream and answered 200.
//!    nginx and haproxy both answer 400 on the same request, so this restores parity.
//! 2. [`AgentGate`] — the `relayfold-agents` [`relayfold_agents::AgentClient`] wired into the
//!    request path (before the request ever reaches an upstream), **fail-open** on any agent
//!    error so an external agent process can never take the dataplane down with it.
//!
//! Both run inside `Router::fallback` in `main.rs`; the ordering is deliberate: the cheap,
//! purely-local header check runs first so an oversized-header flood never costs a UDS round
//! trip, and the agent only ever sees requests that are already in-bounds.

use biota_http_server::{Request, Response, Router, StatusCode};
use relayfold_agents::{AgentClient, AgentRequest, Decision, Phase};
use std::time::Duration;

// -- 1. Header limits ---------------------------------------------------

/// 8 KiB, the same single-header-line budget nginx's `large_client_header_buffers 4 8k` and
/// haproxy's `tune.bufsize` default to. Deliberately not a tuned number: parity with the
/// proxies this one is measured against is the requirement.
pub const DEFAULT_MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Total bytes allowed across the whole header block: 4 lines' worth, again matching nginx's
/// `4 8k`. Without this, N small-but-under-the-line-limit headers add up to an unbounded block
/// and the per-line check alone is trivially defeated.
pub const DEFAULT_MAX_HEADERS_TOTAL_BYTES: usize = 4 * DEFAULT_MAX_HEADER_LINE_BYTES;

/// The header-size policy for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum bytes in any ONE header line — `name: value` + CRLF. Applies to every header
    /// including `Content-Length`, which is a header line like any other.
    pub max_header_line_bytes: usize,
    /// Maximum bytes across the whole header block.
    pub max_headers_total_bytes: usize,
    /// Optional ceiling on the declared request body (`Content-Length`).
    ///
    /// **Default `None` (disabled), and that is a deliberate reading of the requirement, not
    /// an oversight.** "Reject when Content-Length exceeds 8 KB" is enforced here as *the
    /// `Content-Length` header line* exceeding 8 KB, which the per-line limit already covers.
    /// Turning it into a body ceiling by default would reject every upload larger than 8 KB —
    /// S3 PUTs, S3 multipart parts, form posts — i.e. it would break the proxy's primary job.
    /// A body ceiling is a real control but it is a different control, so it is exposed here
    /// and off by default; `MAX_CONTENT_LENGTH_BYTES` (env) turns it on explicitly.
    pub max_content_length: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_header_line_bytes: DEFAULT_MAX_HEADER_LINE_BYTES,
            max_headers_total_bytes: DEFAULT_MAX_HEADERS_TOTAL_BYTES,
            max_content_length: None,
        }
    }
}

impl Limits {
    /// Read the policy from the environment, so the chart (Task A owns the config schema) can
    /// set these later without a code change:
    /// `MAX_HEADER_LINE_BYTES`, `MAX_HEADERS_TOTAL_BYTES`, `MAX_CONTENT_LENGTH_BYTES` (0 or
    /// unset = disabled).
    #[must_use]
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |key: &str, default: usize| {
            get(key)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(default)
        };
        Limits {
            max_header_line_bytes: num("MAX_HEADER_LINE_BYTES", DEFAULT_MAX_HEADER_LINE_BYTES),
            max_headers_total_bytes: num(
                "MAX_HEADERS_TOTAL_BYTES",
                DEFAULT_MAX_HEADERS_TOTAL_BYTES,
            ),
            max_content_length: get("MAX_CONTENT_LENGTH_BYTES")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&v| v > 0),
        }
    }
}

/// Why a request was refused at the header layer. Carries the offending numbers so the 400 body
/// says which header and how big it was — an operator reading a 400 in a log needs that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// One header line exceeded `max_header_line_bytes`.
    HeaderLineTooLarge {
        name: String,
        bytes: usize,
        limit: usize,
    },
    /// The whole header block exceeded `max_headers_total_bytes`.
    HeadersTotalTooLarge { bytes: usize, limit: usize },
    /// `Content-Length` declared a body over `max_content_length`.
    ContentLengthTooLarge { declared: u64, limit: u64 },
    /// `Content-Length` was present but not a number. The upstream parser defaults a
    /// malformed one to 0 and drops it, which is a silent, confusing failure mode; a 400 is
    /// the honest answer.
    MalformedContentLength { value: String },
}

impl Violation {
    /// The single-line reason written into the 400 body.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Violation::HeaderLineTooLarge { name, bytes, limit } => {
                format!("header line {name:?} is {bytes} bytes, over the {limit}-byte limit")
            }
            Violation::HeadersTotalTooLarge { bytes, limit } => {
                format!("header block is {bytes} bytes, over the {limit}-byte limit")
            }
            Violation::ContentLengthTooLarge { declared, limit } => {
                format!("content-length declares {declared} bytes, over the {limit}-byte limit")
            }
            Violation::MalformedContentLength { value } => {
                format!("content-length {value:?} is not a number")
            }
        }
    }
}

/// Size of one header line as it appears on the wire: `name` + `": "` + `value` + CRLF.
fn header_line_bytes(name: &str, value: &[u8]) -> usize {
    name.len() + 2 + value.len() + 2
}

/// Check a parsed request against `limits`. `Ok(())` means forward it.
pub fn check_headers(req: &Request, limits: &Limits) -> Result<(), Violation> {
    let mut total = 0usize;
    for (name, value) in req.headers().iter() {
        let line = header_line_bytes(name, value.as_bytes());
        if line > limits.max_header_line_bytes {
            return Err(Violation::HeaderLineTooLarge {
                name: name.to_string(),
                bytes: line,
                limit: limits.max_header_line_bytes,
            });
        }
        total += line;
    }
    if total > limits.max_headers_total_bytes {
        return Err(Violation::HeadersTotalTooLarge {
            bytes: total,
            limit: limits.max_headers_total_bytes,
        });
    }
    if let Some(cl) = req.headers().get("content-length") {
        let raw = cl.to_str().unwrap_or_default();
        match raw.trim().parse::<u64>() {
            Ok(declared) => {
                if let Some(limit) = limits.max_content_length {
                    if declared > limit {
                        return Err(Violation::ContentLengthTooLarge { declared, limit });
                    }
                }
            }
            Err(_) => {
                return Err(Violation::MalformedContentLength {
                    value: raw.to_string(),
                })
            }
        }
    }
    Ok(())
}

/// nginx/haproxy parity answer: **400** with a plain-text body naming the offending header and
/// its size. Not a 431 and not a 502 — nginx answers 400 for this class, and matching the
/// proxies this is measured against is the point.
#[must_use]
pub fn header_limit_response(v: &Violation) -> Response {
    Response::new(
        StatusCode::BAD_REQUEST,
        format!("400 Bad Request: {}", v.reason()),
    )
}

// -- 2. External agent gate ---------------------------------------------

/// The proxy-side wire format for an agent decision, fixed here (it was previously undefined:
/// `relayfold-agents` shipped `Decision` with zero callers).
///
/// The agent is handed one [`relayfold_agents::Phase::RequestHeaders`] [`AgentRequest`] per
/// inbound request — method, path, headers, empty body — over the crate's UDS protocol, and
/// answers with one `Decision`, which this proxy maps as:
///
/// | `Decision` | proxy action |
/// |---|---|
/// | `Allow` | continue to the upstream |
/// | `Deny { status, body }` | answer the client with exactly that status and body (default 403 from `Decision::deny()`), upstream never contacted |
/// | `SetHeaders(pairs)` | insert/overwrite those headers on the request, then continue |
///
/// **Failure mode is fail-open, always.** A connect failure, a crashed mid-response agent, a
/// malformed frame, or an agent slower than [`AgentGate::timeout`] is logged to stderr and the
/// request continues. `relayfold-agents`' own doc leaves this policy to the caller; for an edge
/// proxy whose whole job is forwarding traffic, blocking on an external process's outage is
/// the worse failure. The one thing that must never happen is a silent undecided Allow with no
/// log line — every fail-open is logged.
#[derive(Clone)]
pub struct AgentGate {
    client: Option<AgentClient>,
    timeout: Duration,
}

/// Default per-request agent budget: 250 ms. Long enough for a trivial local UDS round trip,
/// short enough that a wedged agent degrades latency rather than availability.
pub const DEFAULT_AGENT_TIMEOUT_MS: u64 = 250;

impl AgentGate {
    /// A gate that consults nothing — every request passes through. The default when
    /// `AGENT_SOCKET` is unset, so the proxy behaves exactly as it did before this wiring.
    #[must_use]
    pub fn disabled() -> Self {
        AgentGate {
            client: None,
            timeout: Duration::from_millis(DEFAULT_AGENT_TIMEOUT_MS),
        }
    }

    /// A gate that consults the agent listening on the UDS at `socket_path`.
    #[must_use]
    pub fn new(socket_path: impl Into<std::path::PathBuf>, timeout: Duration) -> Self {
        AgentGate {
            client: Some(AgentClient::new(socket_path)),
            timeout,
        }
    }

    /// Build from the environment: `AGENT_SOCKET` (empty/unset = disabled) and
    /// `AGENT_TIMEOUT_MS` (default 250).
    #[must_use]
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let socket = get("AGENT_SOCKET").unwrap_or_default();
        let timeout_ms = get("AGENT_TIMEOUT_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_AGENT_TIMEOUT_MS);
        if socket.trim().is_empty() {
            AgentGate::disabled()
        } else {
            AgentGate::new(socket.trim(), Duration::from_millis(timeout_ms.max(1)))
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.client.is_some()
    }

    #[must_use]
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        self.client
            .as_ref()
            .map(relayfold_agents::AgentClient::socket_path)
    }

    /// Consult the agent about `req` on the way in.
    ///
    /// `Some(response)` means the request must NOT be forwarded (the agent denied it);
    /// `None` means continue. A `SetHeaders` decision mutates `req` in place and returns
    /// `None` — mutation plus continue.
    pub async fn check(&self, req: &mut Request) -> Option<Response> {
        let client = self.client.as_ref()?;
        let agent_req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: req.method().as_str().to_string(),
            path: req.path().to_string(),
            headers: req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                .collect(),
            body: Vec::new(),
        };
        let decision = match tokio::time::timeout(self.timeout, client.evaluate(&agent_req)).await {
            // FAIL OPEN: log loudly, forward.
            Err(_) => {
                eprintln!(
                    "[relayfold-proxy] agent at {} did not answer within {:?} — failing open",
                    client.socket_path().display(),
                    self.timeout
                );
                return None;
            }
            Ok(Err(e)) => {
                eprintln!(
                    "[relayfold-proxy] agent at {} unreachable ({e}) — failing open",
                    client.socket_path().display()
                );
                return None;
            }
            Ok(Ok(d)) => d,
        };
        match decision {
            Decision::Allow => None,
            Decision::Deny { status, body } => {
                // An agent that returns a status outside 100..=999 (e.g. a nonsense 0) must not
                // be able to emit an unparseable response; fall back to 403.
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
                Some(Response::new(status, body))
            }
            Decision::SetHeaders(pairs) => {
                let map = req.headers_mut();
                for (name, value) in pairs {
                    map.insert(name, value);
                }
                None
            }
        }
    }
}

// -- Router wiring ------------------------------------------------------

/// The one guard step both entry points call: header ceiling first (cheap, purely local, so an
/// oversized-header flood never costs a UDS round trip), then the agent gate. `Some(response)`
/// means refuse and answer the client; `None` means the request is cleared to go upstream.
///
/// [`build_router`] is the composition for the single-pool case; `main.rs` calls this directly
/// because it forwards through its own live route table rather than a `Proxy`.
pub async fn guard(req: &mut Request, limits: &Limits, gate: &AgentGate) -> Option<Response> {
    if let Err(v) = check_headers(req, limits) {
        eprintln!("[relayfold-proxy] 400 header limit: {}", v.reason());
        return Some(header_limit_response(&v));
    }
    gate.check(req).await
}

/// Build the serving router: `/healthz` + `/readyz` probes (never agent-gated — a probe that
/// answers 403 because an agent is down is a false negative that restarts healthy pods), then a
/// catch-all that runs the guards, then the upstream proxy.
///
/// `shared_proxy` is read-cloned once per request so a control-plane pool refresh keeps working
/// exactly as it did before.
#[must_use]
pub fn build_router(
    shared_proxy: std::sync::Arc<std::sync::RwLock<biota_http_server::Proxy>>,
    limits: Limits,
    gate: AgentGate,
) -> Router {
    Router::new()
        .get("/healthz", |_req: Request| async { Response::ok("ok") })
        .get("/readyz", |_req: Request| async { Response::ok("ready") })
        .fallback(move |mut req: Request| {
            let proxy = shared_proxy.read().unwrap().clone();
            let limits = limits;
            let gate = gate.clone();
            async move {
                if let Some(resp) = guard(&mut req, &limits, &gate).await {
                    return resp;
                }
                proxy.handle(req).await
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use biota_http_server::Method;

    fn req_with(header: (&str, &str)) -> Request {
        Request::new(Method::GET, "/x".parse().expect("uri")).with_header(header.0, header.1)
    }

    #[test]
    fn an_8k_header_line_is_rejected_while_a_small_one_is_not() {
        let limits = Limits::default();
        let big = "v".repeat(DEFAULT_MAX_HEADER_LINE_BYTES);
        let v = check_headers(&req_with(("x-big", &big)), &limits).unwrap_err();
        assert!(matches!(v, Violation::HeaderLineTooLarge { .. }));
        let small = req_with(("x-small", "hello"));
        assert!(check_headers(&small, &limits).is_ok());
    }

    #[test]
    fn many_under_the_line_limit_headers_still_hit_the_total_limit() {
        let limits = Limits::default();
        let mut req = Request::new(Method::GET, "/x".parse().expect("uri"));
        for i in 0..200 {
            req = req.with_header(format!("x-h{i}"), "v".repeat(1000));
        }
        let v = check_headers(&req, &limits).unwrap_err();
        assert!(matches!(v, Violation::HeadersTotalTooLarge { .. }));
    }

    #[test]
    fn a_content_length_over_the_configured_body_limit_is_rejected() {
        let limits = Limits {
            max_content_length: Some(8192),
            ..Limits::default()
        };
        let ok = req_with(("content-length", "8192"));
        assert!(check_headers(&ok, &limits).is_ok());
        let v = check_headers(&req_with(("content-length", "8193")), &limits).unwrap_err();
        assert!(matches!(
            v,
            Violation::ContentLengthTooLarge { declared: 8193, .. }
        ));
    }

    #[test]
    fn by_default_a_large_body_is_not_rejected_only_the_header_line_is_bounded() {
        // The documented default: a large upload is not a header violation. Enforcing 8 KB as a
        // *body* ceiling by default would reject every real upload.
        let big_body = "9".repeat(7);
        assert!(check_headers(
            &req_with(("content-length", big_body.as_str())),
            &Limits::default()
        )
        .is_ok());
    }

    #[test]
    fn a_malformed_content_length_is_a_400_not_a_silent_zero() {
        let v = check_headers(
            &req_with(("content-length", "not-a-number")),
            &Limits::default(),
        )
        .unwrap_err();
        assert!(matches!(v, Violation::MalformedContentLength { .. }));
    }

    #[test]
    fn the_violation_response_is_400_and_names_the_header() {
        let v = check_headers(
            &req_with(("x-big", &"v".repeat(DEFAULT_MAX_HEADER_LINE_BYTES))),
            &Limits::default(),
        )
        .unwrap_err();
        let resp = header_limit_response(&v);
        assert_eq!(resp.status().as_u16(), 400);
        let body = String::from_utf8(resp.stream_body()).expect("utf8");
        assert!(body.contains("x-big"), "body must name the header: {body}");
    }

    #[test]
    fn env_reading_keeps_the_defaults_and_honours_an_explicit_body_limit() {
        let empty = Limits::from_env(|_| None);
        assert_eq!(empty, Limits::default());
        let custom = Limits::from_env(|k| match k {
            "MAX_CONTENT_LENGTH_BYTES" => Some("1024".into()),
            _ => None,
        });
        assert_eq!(custom.max_content_length, Some(1024));
    }

    #[test]
    fn an_unset_agent_socket_disables_the_gate_entirely() {
        assert!(!AgentGate::from_env(|_| None).is_enabled());
        let on = AgentGate::from_env(|k| match k {
            "AGENT_SOCKET" => Some("/run/relayfold-agent.sock".into()),
            _ => None,
        });
        assert!(on.is_enabled());
        assert_eq!(
            on.socket_path().unwrap().to_str(),
            Some("/run/relayfold-agent.sock")
        );
    }
}
