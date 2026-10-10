//! The wire-facing request/response shapes — `#[derive(hashtrinity::Term)]`, decoded/encoded by
//! `crate::json::Json<T>`. These may use `bool`/`Option` freely; `Term`'s field discipline is
//! real typed extraction.

use hashtrinity::Term;

#[derive(Clone, Debug, Term)]
pub struct RegisterRequest {
    pub id: String,
    pub addr: String,
    pub version: String,
}

#[derive(Clone, Debug, Term)]
pub struct HeartbeatRequest {
    pub id: String,
    pub healthy: bool,
    /// The config version the instance reports as applied (free-form, as before).
    pub config_version: String,
}

#[derive(Clone, Debug, Term)]
pub struct InstanceView {
    pub id: String,
    pub addr: String,
    pub version: String,
    /// Registered, heartbeat inside the TTL, and the last heartbeat reported healthy.
    pub healthy: bool,
    /// Unix seconds of the last sign of life this process saw; `0` = not heard since restart.
    pub last_heartbeat_unix: i64,
    pub config_version: String,
    /// The public key (z-base32) this id is bound to.
    pub public_key: String,
}

/// `target` is either one instance's `id`, or `"*"` for every registered instance.
#[derive(Clone, Debug, Term)]
pub struct ConfigPushRequest {
    pub target: String,
    pub config: String,
}

#[derive(Clone, Debug, Term)]
pub struct ConfigPushResult {
    pub id: String,
    pub ok: bool,
    pub detail: String,
    /// The control-plane `config_version` this push carried.
    pub config_version: i64,
}

/// A JSON array of `T::to_term()` — built manually via `SyntaxTerm::Fourth` rather than a
/// `Vec<T>` field on a wrapper `#[derive(Term)]` struct, since nesting a `Vec` of an already
/// `Term`-derived struct inside another derived struct isn't a shape this crate has verified the
/// derive supports; a hand-built array of terms is unambiguous either way.
pub fn json_array<T: hashtrinity::codec::syntax::AsTerm>(items: &[T]) -> servelet_core::Response {
    use hashtrinity::codec::syntax::{render_surface, Surface, SyntaxTerm};
    let term = SyntaxTerm::Fourth(
        items
            .iter()
            .map(hashtrinity::codec::syntax::AsTerm::to_term)
            .collect(),
    );
    let body = render_surface(&term, Surface::Json);
    servelet_core::Response::ok(body.into_bytes()).with_header("content-type", "application/json")
}
