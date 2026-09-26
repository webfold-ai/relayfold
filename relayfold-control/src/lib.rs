//! `relayfold-control` — a real fleet-management control plane for relayfold-based proxy
//! instances: registry, heartbeat/health, and config push.
//!
//! **What this replaces, honestly**: zentinel's own `zentinel-control-plane` (Elixir/Phoenix,
//! self-declared Beta) does fleet management for zentinel proxies. This is a from-scratch Rust
//! equivalent for OUR proxy stack (relayfold/bedrock/biota) — no Elixir/Phoenix dependency, built
//! on `biota-http-server`'s own `Router`/`Client` (this workspace hand-rolls its HTTP stack
//! deliberately) and `sangdb` for real, disk-persisted registry state.
//!
//! **Scope, disclosed precisely** — this is an MVP, not parity with zentinel's fuller feature
//! set:
//! - Registry + heartbeat + health snapshot: real (see [`app::AppState`]).
//! - Config push: real HTTP delivery to each registered instance's `/relayfold-control/config`
//!   endpoint, per-instance success/failure reporting. What a receiving instance actually DOES
//!   with a pushed config (validate it, hot-reload it, apply it to `relayfold::Registry`) is the
//!   receiving proxy's job — this crate is the control plane, not the reload logic.
//! - **NOT built**: multi-tenant dashboards, canary/staged rollout, an authenticated/RBAC'd admin
//!   UI, TLS between control plane and instances (plain HTTP — real production use needs mTLS or
//!   at least a shared bearer token, neither implemented here), and instance auto-discovery (an
//!   instance must call `/register` itself; there's no scanning/service-discovery mechanism).

pub mod app;
pub mod dto;
pub mod model;

pub use app::{router, AppState};
