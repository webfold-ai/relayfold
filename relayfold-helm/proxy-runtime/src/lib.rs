//! `relayfold-proxy-runtime` as a library: the config reader and the load-balanced forwarding
//! path, so both are unit-testable without spawning a server. `main.rs` is the thin binary
//! wrapper around it.

pub mod balancer;
pub mod config;

/// Edge guards: the hard 8 KB header ceiling (answered 400, never forwarded upstream) and the
/// `relayfold-agents` gate on the request path (fail-open). Kept in its own module rather than in
/// `lib.rs` so the config/balancer work and this work do not collide on one file; see
/// `edge_guards.rs` for the full rationale and the decision wire format.
pub mod edge_guards;
/// The parsed-config → live route table: relayfold `Registry`/`Backends` per route, resolve,
/// authorize, and the per-request `pick`.
pub mod routes;

/// The SIGHUP / file-watch reload triggers.
pub mod reload;
