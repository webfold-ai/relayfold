//! `Json<T>` request/response extractor.
//!
//! # Why this lives here rather than in servelet
//!
//! `biota-http-server` carried `actix::Json`, a typed body extractor over
//! `wirefold::AsTerm` — `#[derive(Term)]` DTOs, parsed with wirefold's own codec rather
//! than a second grammar. servelet does not have it, and it is genuinely not an identity
//! adapter: `Router::route` already takes `Fn(Request) -> Future<Response>`, so the
//! `to_handler*_async` shims collapsed into it and were deleted (82 call sites in
//! `ptyktos-webgui`). `Json<T>` is different — it does real work, decoding a body into a type.
//!
//! It is kept **local** rather than proposed upstream, deliberately. servelet is an active repo
//! with its own worker adding units; adding a crate to it from here would collide. This is ~60
//! lines with no dependency beyond the `wirefold` this workspace already uses, so the cost of
//! a local copy is lower than the cost of a cross-repo coupling.
//!
//! If a second consumer appears, the honest move is to lift this into servelet then, with the
//! duplication removed — not before.

use std::fmt;

use wirefold::codec::syntax::{AsTerm, Surface, SyntaxTerm as Term};
use servelet_core::{Request, Response, StatusCode};

/// A JSON body, typed. `Json<T>(T)` mirrors actix's `web::Json<T>` so handler code ports by
/// changing one import rather than being rewritten.
pub struct Json<T>(pub T);

/// Why a body could not be decoded into `T`.
#[derive(Debug)]
pub struct JsonError(String);

impl JsonError {
    fn new(msg: impl fmt::Display) -> Self {
        JsonError(msg.to_string())
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonError {}

/// actix's `Responder for JsonError` — a malformed body is a 400, not a 500.
impl JsonError {
    /// The response a failed extraction should become.
    pub fn into_response(self) -> Response {
        Response::new(StatusCode::BAD_REQUEST, self.0.into_bytes())
            .with_header("content-type", "text/plain; charset=utf-8")
    }
}

impl<T: AsTerm> Json<T> {
    /// Decode a request body as JSON into `T`.
    pub fn from_request(req: &Request) -> Result<Json<T>, JsonError> {
        Self::from_bytes(req.body())
    }

    /// Decode raw JSON bytes into `T` (the body-level half of [`Json::from_request`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Json<T>, JsonError> {
        let body = std::str::from_utf8(bytes).map_err(|e| JsonError::new(e.to_string()))?;
        let term = wirefold::codec::syntax::parse_surface(Surface::Json, body)
            // `SyntaxError` has no Display, so name its Debug form rather than losing the
            // parse position — a 400 that cannot say what was wrong is not debuggable.
            .map_err(|e| JsonError::new(format!("{e:?}")))?;
        // `SyntaxError` has no Display; use Debug so the field/position survives.
        T::from_term(&term)
            .map(Json)
            .map_err(|e| JsonError::new(format!("{e:?}")))
    }

    /// Serialize to JSON bytes.
    pub fn encode(&self) -> Vec<u8> {
        let term: Term = self.0.to_term();
        wirefold::codec::syntax::render_surface(&term, Surface::Json).into_bytes()
    }

    /// Serialize as a JSON response with the right Content-Type.
    pub fn into_response(self) -> Response {
        Response::new(StatusCode::OK, self.encode()).with_header("content-type", "application/json")
    }

    /// Unwrap to the inner `T`, matching `web::Json<T>::into_inner()`.
    pub fn into_inner(self) -> T {
        self.0
    }
}
