//! The decision points of the auth / liveness / config-verification pipeline, collected so each
//! can be swapped for a deliberately wrong one.
//!
//! `Stages::REAL` is what ships. A *mutant* is `Stages { signature_ok: |..| true, ..Stages::REAL }`:
//! the real pipeline (`auth::Verifier`, `liveness::fold`, `config::verify_config_with`, the
//! control plane) runs unchanged around one altered decision. The conformance checker
//! (`conformance::check`) is generic over `Stages`, so the same function judges the real
//! implementation and every mutant — see `tests/negative_controls.rs`.
//!
//! These are plain `fn` pointers (no captures, no trait objects), so `Stages` is `Copy` and a
//! mutant is a non-capturing closure.

use crate::auth::{self, Key, NonceVerdict, NonceWindow};
use crate::config;
use crate::liveness::{self, Liveness, LivenessEvent};

#[derive(Clone, Copy)]
pub struct Stages {
    /// `(now, request_ts, skew, not_before) -> accept?`
    pub timestamp_ok: fn(i64, i64, i64, i64) -> bool,
    /// Body digest folded into the signed message.
    pub body_hash: fn(&[u8]) -> [u8; 32],
    /// Request signature check: `(signer, message, signature) -> valid?`
    pub signature_ok: fn(&Key, &[u8], &[u8; 64]) -> bool,
    /// Replay window: `(window, signer, nonce, request_ts, now)`.
    pub nonce_fresh: fn(&mut NonceWindow, &Key, &str, i64, i64) -> NonceVerdict,
    /// May `caller` heartbeat an id bound to `bound`?
    pub may_heartbeat: fn(&Key, &Key) -> bool,
    /// May `caller` re-register an id already bound to `bound`?
    pub may_reregister: fn(&Key, &Key) -> bool,
    /// Instance liveness fold: `(state, event, ttl) -> state`.
    pub liveness_fold: fn(Liveness, LivenessEvent, i64) -> Liveness,
    /// Config signature check: `(control key, message, signature) -> valid?`
    pub config_signature_ok: fn(&Key, &[u8], &[u8; 64]) -> bool,
    /// Config version: `(pushed, last_applied) -> accept?`
    pub config_version_ok: fn(u64, u64) -> bool,
    /// Config target: `(signed_for, this_instance) -> accept?`
    pub config_target_ok: fn(&str, &str) -> bool,
}

impl Stages {
    pub const REAL: Stages = Stages {
        timestamp_ok: auth::timestamp_ok,
        body_hash: auth::sha256,
        signature_ok: auth::verify_ed25519,
        nonce_fresh: auth::nonce_fresh,
        may_heartbeat: auth::same_key,
        may_reregister: auth::same_key,
        liveness_fold: liveness::fold,
        config_signature_ok: auth::verify_ed25519,
        config_version_ok: config::version_is_newer,
        config_target_ok: config::target_matches,
    };
}

impl Default for Stages {
    fn default() -> Self {
        Stages::REAL
    }
}
