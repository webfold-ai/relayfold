//! The wire protocol between a `relayfold`-based proxy and an external agent process — one
//! [`AgentRequest`] in, one [`Decision`] back, framed as a 4-byte little-endian length prefix
//! followed by that many bytes of `wirefold::codec::syntax::Surface::Json`-rendered term.
//!
//! **Why not `serde_json`**: this workspace already has a real, zero-extra-dependency JSON
//! surface (`wirefold::codec::syntax`) — the same one bedrock's HTTP layer uses for its own
//! `HttpCall`/extractor terms. Reaching for `serde`/`serde_json` here would mean two JSON
//! implementations doing the same job in the same dependency graph; this crate hand-implements
//! `AsTerm` for its two wire types instead, the same way `HttpCall` does in wirefold itself.
//!
//! **Framing is one-shot, not multiplexed**: each hook call opens one connection, sends one
//! `AgentRequest`, reads one `Decision`, and the connection closes. This is honestly simpler than
//! zentinel's own persistent-connection agent protocol v2 — correct and crash-isolated, but pays
//! one UDS connection-setup cost per hook call rather than amortizing it across many. Real
//! production traffic would want a persistent, request-ID-tagged multiplexed stream instead; that
//! is a disclosed follow-up; this establishes the contract, not the final performance shape.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wirefold::base64::engine::general_purpose::STANDARD_NO_PAD;
use wirefold::base64::Engine;
use wirefold::codec::syntax::{
    field, field_num, field_str, term_str, AsTerm, Atom, Surface, SyntaxError, SyntaxTerm,
};

/// Which point in the request/response lifecycle this call represents — mirrors the phase names
/// a `ProxyHttp`-shaped caller already has (`request_filter`/`upstream_response_filter`/etc.),
/// named independently of any specific phase-hook trait so this crate has no compile-time
/// dependency on bedrock's or biota's `ProxyHttp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    RequestHeaders,
    RequestBody,
    ResponseHeaders,
    ResponseBody,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::RequestHeaders => "request_headers",
            Phase::RequestBody => "request_body",
            Phase::ResponseHeaders => "response_headers",
            Phase::ResponseBody => "response_body",
        }
    }

    fn parse(s: &str) -> Result<Self, SyntaxError> {
        match s {
            "request_headers" => Ok(Phase::RequestHeaders),
            "request_body" => Ok(Phase::RequestBody),
            "response_headers" => Ok(Phase::ResponseHeaders),
            "response_body" => Ok(Phase::ResponseBody),
            other => Err(SyntaxError::new(format!("unknown phase {other:?}"))),
        }
    }
}

/// One call from the proxy to an agent — one phase's worth of the request/response the proxy is
/// currently handling. `body` is raw bytes (carried on the wire as base64, since JSON strings
/// aren't a byte-safe container) and is empty for the two `*Headers` phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRequest {
    pub phase: Phase,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The agent's answer. `Allow`/`Deny` mirror the two outcomes every one of zentinel's own agents
/// reduces to; `SetHeaders` is the "continue, but mutate" case (inject a header, tag a request)
/// that a pure allow/deny pair can't express.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { status: u16, body: String },
    SetHeaders(Vec<(String, String)>),
}

impl Decision {
    #[must_use]
    pub fn deny() -> DecisionBuilder {
        DecisionBuilder {
            status: 403,
            body: String::new(),
        }
    }

    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Fluent builder for [`Decision::Deny`] — `Decision::deny().with_status(429).with_body("slow down")`.
pub struct DecisionBuilder {
    status: u16,
    body: String,
}

impl DecisionBuilder {
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    #[must_use]
    pub fn build(self) -> Decision {
        Decision::Deny {
            status: self.status,
            body: self.body,
        }
    }
}

fn headers_to_term(headers: &[(String, String)]) -> SyntaxTerm {
    SyntaxTerm::Bu(
        headers
            .iter()
            .map(|(k, v)| (k.clone(), SyntaxTerm::B(Atom::Str(v.clone()))))
            .collect(),
    )
}

fn headers_from_term(term: &SyntaxTerm) -> Result<Vec<(String, String)>, SyntaxError> {
    match term {
        SyntaxTerm::Bu(fields) => fields
            .iter()
            .map(|(k, v)| Ok((k.clone(), term_str(v)?.to_string())))
            .collect(),
        SyntaxTerm::Nb => Ok(Vec::new()),
        _ => Err(SyntaxError::new("headers must be a B/U object")),
    }
}

impl AsTerm for AgentRequest {
    fn to_term(&self) -> SyntaxTerm {
        SyntaxTerm::Bu(vec![
            (
                "phase".into(),
                SyntaxTerm::B(Atom::Str(self.phase.as_str().to_string())),
            ),
            (
                "method".into(),
                SyntaxTerm::B(Atom::Str(self.method.clone())),
            ),
            ("path".into(), SyntaxTerm::B(Atom::Str(self.path.clone()))),
            ("headers".into(), headers_to_term(&self.headers)),
            (
                "body_b64".into(),
                SyntaxTerm::B(Atom::Str(STANDARD_NO_PAD.encode(&self.body))),
            ),
        ])
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("agent request must be a B/U object"));
        };
        let phase = Phase::parse(field_str(fields, "phase")?)?;
        let method = field_str(fields, "method")?.to_string();
        let path = field_str(fields, "path")?.to_string();
        let headers = field(fields, "headers")
            .map(headers_from_term)
            .transpose()?
            .unwrap_or_default();
        let body = STANDARD_NO_PAD
            .decode(field_str(fields, "body_b64")?)
            .map_err(|e| SyntaxError::new(format!("invalid body_b64: {e:?}")))?;
        Ok(AgentRequest {
            phase,
            method,
            path,
            headers,
            body,
        })
    }
}

impl AsTerm for Decision {
    fn to_term(&self) -> SyntaxTerm {
        match self {
            Decision::Allow => SyntaxTerm::Bu(vec![(
                "kind".into(),
                SyntaxTerm::B(Atom::Str("allow".into())),
            )]),
            Decision::Deny { status, body } => SyntaxTerm::Bu(vec![
                ("kind".into(), SyntaxTerm::B(Atom::Str("deny".into()))),
                (
                    "status".into(),
                    SyntaxTerm::B(Atom::Num(status.to_string())),
                ),
                ("body".into(), SyntaxTerm::B(Atom::Str(body.clone()))),
            ]),
            Decision::SetHeaders(headers) => SyntaxTerm::Bu(vec![
                (
                    "kind".into(),
                    SyntaxTerm::B(Atom::Str("set_headers".into())),
                ),
                ("headers".into(), headers_to_term(headers)),
            ]),
        }
    }

    fn from_term(term: &SyntaxTerm) -> Result<Self, SyntaxError> {
        let SyntaxTerm::Bu(fields) = term else {
            return Err(SyntaxError::new("decision must be a B/U object"));
        };
        match field_str(fields, "kind")? {
            "allow" => Ok(Decision::Allow),
            "deny" => {
                let status: u16 = field_num(fields, "status")?
                    .parse()
                    .map_err(|_| SyntaxError::new("status must be a valid u16"))?;
                let body = field_str(fields, "body")?.to_string();
                Ok(Decision::Deny { status, body })
            }
            "set_headers" => {
                let headers = field(fields, "headers")
                    .map(headers_from_term)
                    .transpose()?
                    .unwrap_or_default();
                Ok(Decision::SetHeaders(headers))
            }
            other => Err(SyntaxError::new(format!("unknown decision kind {other:?}"))),
        }
    }
}

/// Write one length-prefixed, JSON-rendered term. The single framing primitive both
/// [`crate::AgentClient`] (writes an `AgentRequest`) and [`crate::AgentRunner`] (writes a
/// `Decision`) build on.
pub async fn write_frame<W: AsyncWrite + Unpin, T: AsTerm>(
    w: &mut W,
    value: &T,
) -> std::io::Result<()> {
    let rendered = wirefold::codec::syntax::render_surface(&value.to_term(), Surface::Json);
    let bytes = rendered.into_bytes();
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Read one length-prefixed, JSON-rendered term back into a concrete `AsTerm` type. Rejects
/// frames over 16 MiB outright — a real bound, not an arbitrary one: it is larger than any
/// legitimate single-request header/body slice this protocol carries, and an unbounded read here
/// would let a misbehaving peer allocate without limit before a single byte of content is even
/// validated.
pub async fn read_frame<R: AsyncRead + Unpin, T: AsTerm>(r: &mut R) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    const MAX_FRAME: usize = 16 * 1024 * 1024;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME}-byte limit"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let text = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let term = wirefold::codec::syntax::parse_surface(Surface::Json, &text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.msg))?;
    T::from_term(&term).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_request_round_trips_through_its_term() {
        let req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: "POST".into(),
            path: "/admin/reset".into(),
            headers: vec![("x-real-ip".into(), "203.0.113.7".into())],
            body: vec![1, 2, 3, 0, 255],
        };
        let term = req.to_term();
        let back = AgentRequest::from_term(&term).expect("round trip must succeed");
        assert_eq!(req, back);
    }

    #[test]
    fn empty_body_round_trips_as_empty_not_as_a_decode_error() {
        let req = AgentRequest {
            phase: Phase::RequestBody,
            method: "GET".into(),
            path: "/".into(),
            headers: vec![],
            body: vec![],
        };
        let back = AgentRequest::from_term(&req.to_term()).unwrap();
        assert_eq!(back.body, Vec::<u8>::new());
    }

    #[test]
    fn decision_allow_round_trips() {
        let back = Decision::from_term(&Decision::Allow.to_term()).unwrap();
        assert_eq!(back, Decision::Allow);
    }

    #[test]
    fn decision_deny_round_trips_with_status_and_body() {
        let d = Decision::deny()
            .with_status(429)
            .with_body("slow down")
            .build();
        let back = Decision::from_term(&d.to_term()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn decision_set_headers_round_trips() {
        let d = Decision::SetHeaders(vec![("x-agent-tag".into(), "reviewed".into())]);
        let back = Decision::from_term(&d.to_term()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn an_unknown_decision_kind_is_a_real_error_not_a_silent_allow() {
        let term = SyntaxTerm::Bu(vec![(
            "kind".into(),
            SyntaxTerm::B(Atom::Str("nonsense".into())),
        )]);
        assert!(
            Decision::from_term(&term).is_err(),
            "an unrecognized kind must never decode as a decision at all, let alone Allow"
        );
    }

    #[tokio::test]
    async fn write_frame_then_read_frame_round_trips_over_a_real_pipe() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let req = AgentRequest {
            phase: Phase::ResponseHeaders,
            method: "GET".into(),
            path: "/status".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: b"hello".to_vec(),
        };
        write_frame(&mut client, &req).await.unwrap();
        let back: AgentRequest = read_frame(&mut server).await.unwrap();
        assert_eq!(req, back);
    }

    #[tokio::test]
    async fn a_frame_claiming_an_oversized_length_is_rejected_before_allocating_it() {
        let (mut client, mut server) = tokio::io::duplex(64);
        // Claim a body far larger than the real payload that follows — a real peer sending a
        // corrupt or hostile length prefix, not a valid frame truncated in transit.
        client
            .write_all(&(64u32 * 1024 * 1024).to_le_bytes())
            .await
            .unwrap();
        drop(client);
        let result: std::io::Result<Decision> = read_frame(&mut server).await;
        assert!(
            result.is_err(),
            "an oversized claimed length must be rejected, not trigger a 64 MiB allocation"
        );
    }
}
