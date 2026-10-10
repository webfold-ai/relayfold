//! `relayfold-control` — a fleet-management control plane for relayfold-based proxy instances:
//! dialkey-signed registry, heartbeat expiry, signed config push, durable append log.
//!
//! Built from scratch for OUR proxy stack (relayfold/bedrock/biota) on `servelet` (HTTP) and
//! `sangdb-log` (durability), as a Rust alternative to zentinel's Elixir/Phoenix control plane.
//!
//! # What it guarantees
//!
//! * **Identity** ([`auth`]): register, heartbeat and config push each carry a signature by a
//!   dialkey-derived Ed25519 key over `(method, path, body hash, timestamp, nonce)`. `register`
//!   binds `id -> public key`, first register wins; only that key may heartbeat the id.
//! * **Replay protection** ([`auth::NonceWindow`]): bounded nonce window plus a timestamp skew
//!   limit, both against an injected [`clock::Clock`]; deterministic.
//! * **Signed config** ([`config`]): pushes are signed by a control-plane key and carry a
//!   persisted, strictly increasing `config_version`; [`config::verify_config`] is the pure
//!   instance-side check.
//! * **Liveness** ([`liveness`]): a pure fold with a configurable TTL; an instance with no
//!   heartbeat inside the TTL is not healthy in `list()`.
//! * **Durability** ([`store`]): an error-returning append path on `sangdb-log`; identities, bound
//!   keys and `config_version` survive restart; a torn tail is truncated on open.
//! * **Checkable** ([`stages`], [`conformance`]): every decision point can be swapped for a wrong
//!   one, and one checker judges the real implementation and its mutants.
//!
//! # Not built (disclosed)
//!
//! * **No enrollment policy.** Any keypair may register a new, unclaimed id; first-register-wins
//!   protects an id once claimed, not the namespace. Gate enrollment at the network edge or add an
//!   allowlist before exposing this beyond a trusted network.
//! * **`GET /instances` is unauthenticated** (read-only, but it lists addresses and public keys).
//! * **Plain HTTP.** Requests are authenticated and configs are signed, but nothing is encrypted;
//!   a signed config is integrity-protected, not confidential. No TLS/mTLS.
//! * **The nonce window is memory-only.** A restart is covered by rejecting timestamps older than
//!   the process start; an authenticated key can still fill the (bounded) window and make
//!   others wait (429) until entries age out of the skew window.
//! * **No log compaction.** The log holds registrations and config pushes only (heartbeats are not
//!   logged), so it grows with fleet churn, not with time; `sangdb-log` offers `compact` but it is
//!   not wired in.
//! * No multi-tenant dashboards, canary/staged rollout, RBAC'd admin UI, or auto-discovery. What
//!   an instance does with a verified config is the instance's job.

pub mod app;
pub mod auth;
pub mod clock;
pub mod config;
pub mod conformance;

pub mod dto;
pub mod json;
pub mod liveness;
pub mod plane;
pub mod stages;
pub mod store;

pub use app::{router, serve_on, AppState};
pub use clock::{Clock, ManualClock, SystemClock};
pub use plane::{ControlConfig, ControlError, ControlPlane};
pub use stages::Stages;
