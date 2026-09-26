//! The registry's stored shape — a `sangdb::Model`, not an in-memory-only `HashMap`, so a
//! restarted control plane doesn't forget every proxy instance that had registered.
//!
//! Collapse's field discipline (scalars/`String` only, no `Option`, no bare `Vec<String>`) is why
//! `healthy` is `i64` (0/1) rather than `bool` here — the wire-facing DTOs in `dto.rs` use real
//! `bool` via `#[derive(hashtrinity::Term)]`, which has no such restriction; this type never
//! crosses the wire directly.

use hashtrinity::Collapse;
use sangdb::Model;

#[derive(Clone, Debug, Collapse)]
pub struct InstanceRecord {
    pub id: String,
    pub addr: String,
    pub version: String,
    /// 1 = last heartbeat reported healthy, 0 = unhealthy. See the module doc for why not `bool`.
    pub healthy: i64,
    pub last_heartbeat_unix: i64,
    pub config_version: String,
}

impl Model for InstanceRecord {
    type Key = String;
    const TABLE: &'static str = "instances";

    fn key(&self) -> Self::Key {
        self.id.clone()
    }

    fn decode(bytes: &[u8]) -> Vec<Self> {
        Self::collapse_decode(bytes)
    }
}
