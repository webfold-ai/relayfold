//! The wire-facing request/response shapes — `#[derive(hashtrinity::Term)]`, decoded/encoded by
//! `crate::json::Json<T>`. Unlike `model::InstanceRecord` these may use `bool`/`Option`
//! freely; `Term`'s field discipline is real typed extraction, not `Collapse`'s storage format.

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
    pub config_version: String,
}

#[derive(Clone, Debug, Term)]
pub struct InstanceView {
    pub id: String,
    pub addr: String,
    pub version: String,
    pub healthy: bool,
    pub last_heartbeat_unix: i64,
    pub config_version: String,
}

impl From<&crate::model::InstanceRecord> for InstanceView {
    fn from(r: &crate::model::InstanceRecord) -> Self {
        InstanceView {
            id: r.id.clone(),
            addr: r.addr.clone(),
            version: r.version.clone(),
            healthy: r.healthy != 0,
            last_heartbeat_unix: r.last_heartbeat_unix,
            config_version: r.config_version.clone(),
        }
    }
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
}

/// A JSON array of `T::to_term()` — built manually via `SyntaxTerm::Fourth` rather than a
/// `Vec<T>` field on a wrapper `#[derive(Term)]` struct, since nesting a `Vec` of an already
/// `Term`-derived struct inside another derived struct isn't a shape this crate has verified the
/// derive supports; a hand-built array of terms is unambiguous either way.
pub fn json_array<T: hashtrinity::codec::syntax::AsTerm>(
    items: &[T],
) -> servelet_core::Response {
    use hashtrinity::codec::syntax::{render_surface, Surface, SyntaxTerm};
    let term = SyntaxTerm::Fourth(
        items
            .iter()
            .map(hashtrinity::codec::syntax::AsTerm::to_term)
            .collect(),
    );
    let body = render_surface(&term, Surface::Json);
    servelet_core::Response::ok(body.into_bytes())
        .with_header("content-type", "application/json")
}
